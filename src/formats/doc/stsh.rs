//! Full STSH (style sheet) parsing per MS-DOC: STD records with their
//! `istdBase` inheritance chains and UPX formatting payloads, resolved into
//! effective per-style character formatting and paragraph properties.

use super::Fib;
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::formats::doc::sprm::{
    CharOps, Chp, Fonts, PapDelta, apply_pap_sprms, compile_style_chpx,
};
use crate::shared::binary::{decode_utf16, get_u16, utf16le_units};
use crate::shared::blockstyle::{self, BlockStyle};
use std::collections::{BTreeMap, HashMap, HashSet};

const ISTD_NIL: u16 = 0x0FFF;

/// [MS-DOC] Stshif `ftcAsci`, `ftcFE` and `ftcOther`, then `ftcBi` after Stshif, as
/// offsets into the Stshi.
const STSHI_FTC_ASCI: usize = 12;
const STSHI_FTC_FE: usize = 14;
const STSHI_FTC_OTHER: usize = 16;
const STSHI_FTC_BI: usize = 18;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum Stk {
    Paragraph,
    Character,
    Table,
    #[default]
    Other,
}

impl Stk {
    /// The type the low nibble of the STD word at record offset 2 (`sgc`) names.
    fn of(word: u16) -> Self {
        match word & 0x000F {
            1 => Self::Paragraph,
            2 => Self::Character,
            3 => Self::Table,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct ResolvedStyle {
    /// The chain's character formatting as changes to the properties it sits on.
    chars: CharOps,
    /// Effective character formatting of the chain over plain text.
    pub chp: Chp,
    /// Effective paragraph properties of the chain.
    pub pap: PapDelta,
    /// Heading level for built-in `heading N` styles.
    pub heading: Option<u8>,
    /// The block container the style's name designates.
    pub block: Option<BlockStyle>,
    kind: Stk,
}

#[derive(Debug, Default)]
pub(super) struct Stylesheet {
    resolved: HashMap<u16, ResolvedStyle>,
    /// Returned for unknown istds so `get` never allocates.
    default: ResolvedStyle,
    #[cfg(test)]
    compilations: usize,
}

impl Stylesheet {
    pub(super) fn get(&self, istd: u16) -> &ResolvedStyle {
        self.resolved.get(&istd).unwrap_or(&self.default)
    }

    /// What the character style `istd` changes about the paragraph's formatting, or `None` when
    /// `istd` names no character style.
    pub(super) fn character(&self, istd: u16) -> Option<CharOps> {
        let style = self.resolved.get(&istd)?;
        (style.kind == Stk::Character).then_some(style.chars)
    }
}

#[cfg(test)]
impl Stylesheet {
    pub(super) fn with_block(istd: u16, block: BlockStyle) -> Self {
        let style = ResolvedStyle { block: Some(block), ..ResolvedStyle::default() };
        Self { resolved: HashMap::from([(istd, style)]), ..Self::default() }
    }
}

struct Std<'a> {
    sti: u16,
    istd_base: u16,
    /// The block container this style's name designates.
    block: Option<BlockStyle>,
    /// The PAPX grpprl with its leading istd, empty unless a paragraph style.
    upx_papx: &'a [u8],
    upx_chpx: &'a [u8],
    kind: Stk,
}

/// What a style sheet leaves unread, by what the unread part may hold.
#[derive(Debug, PartialEq)]
enum Unread {
    Nothing,
    /// Records of types that hold no character formatting, none of them a base of a read style
    /// nor inheriting from one.
    Inert,
    /// Records that may hold character formatting, or an extent no bound can be put on.
    Formatting,
}

/// The style sheet. One that does not read in full is `part_unreadable` when the unread part may
/// hold formatting that hides text, or the document has list tables, whose labels styles can
/// carry, and `style_part_unreadable` otherwise.
pub(super) fn parse(
    parse_ctx: &ParseCtx<'_>,
    fib: Fib<'_>,
    table: &[u8],
    has_lists: bool,
) -> Result<Stylesheet, ConvertError> {
    let stsh = fib.table_range(table, Fib::FC_STSHF);
    let (stds, unread) = match stsh {
        Some([]) => (BTreeMap::new(), Unread::Nothing),
        Some(stsh) => parse_stds(parse_ctx, stsh)?,
        None => (BTreeMap::new(), Unread::Formatting),
    };
    match (unread, has_lists) {
        (Unread::Nothing, _) => {}
        (Unread::Inert, false) => {
            parse_ctx.emit(DiagnosticCode::StylePartUnreadable, Region::Container);
        }
        _ => parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Body),
    }
    resolve_all(parse_ctx, &stds, stsh.map(default_fonts).unwrap_or_default())
}

/// The document's default fonts, each 0 where the Stshi ends before it.
fn default_fonts(stsh: &[u8]) -> Fonts {
    let cb_stshi = get_u16(stsh, 0).map_or(0, usize::from);
    let field = |at: usize| {
        (at + 2 <= cb_stshi).then(|| get_u16(stsh, 2 + at)).flatten().unwrap_or_default()
    };
    Fonts {
        ascii: field(STSHI_FTC_ASCI),
        east_asia: field(STSHI_FTC_FE),
        other: field(STSHI_FTC_OTHER),
        bidi: field(STSHI_FTC_BI),
    }
}

/// Style records, charging each slot and complete record before parsing, and what stays unread.
fn parse_stds<'a>(
    parse_ctx: &ParseCtx<'_>,
    stsh: &'a [u8],
) -> Result<(BTreeMap<u16, Std<'a>>, Unread), ConvertError> {
    let mut stds = BTreeMap::new();
    let (Some(cb_stshi), Some(cstd), Some(cb_std_base)) =
        (get_u16(stsh, 0), get_u16(stsh, 2), get_u16(stsh, 4))
    else {
        return Ok((stds, Unread::Formatting));
    };
    let mut pos = 2 + cb_stshi as usize;
    let mut inert = BTreeMap::new();
    let mut formatting = false;
    for istd in 0..cstd {
        let Some(cb_std) = get_u16(stsh, pos) else {
            return Ok((stds, Unread::Formatting));
        };
        parse_ctx.charge(1)?;
        pos += 2;
        if cb_std == 0 {
            continue;
        }
        let Some(record) = stsh.get(pos..).and_then(|rest| rest.get(..cb_std as usize)) else {
            return Ok((stds, Unread::Formatting));
        };
        parse_ctx.charge(u64::from(cb_std))?;
        pos = (pos + cb_std as usize).next_multiple_of(2);
        match parse_std(parse_ctx, record, cb_std_base as usize) {
            Some(std) => drop(stds.insert(istd, std)),
            None => match get_u16(record, 2) {
                Some(word) if Stk::of(word) == Stk::Other => {
                    inert.insert(istd, istd_base(word));
                }
                _ => formatting = true,
            },
        }
    }
    let unread = if formatting
        || stds.values().any(|std| inert.contains_key(&std.istd_base))
        || inherits_read_style(&inert, &stds)
    {
        Unread::Formatting
    } else if inert.is_empty() {
        Unread::Nothing
    } else {
        Unread::Inert
    };
    Ok((stds, unread))
}

/// The base the high 12 bits of the STD word at record offset 2 (`istdBase`) name.
fn istd_base(word: u16) -> u16 {
    (word >> 4) & 0x0FFF
}

/// Whether an unread record's `istdBase` chain, through other unread records, reaches a read style
/// whose formatting it would inherit; each record is walked once.
fn inherits_read_style(inert: &BTreeMap<u16, u16>, stds: &BTreeMap<u16, Std<'_>>) -> bool {
    let mut walked = HashSet::with_capacity(inert.len());
    inert.keys().any(|&start| {
        let mut cursor = start;
        while let Some(&base) = inert.get(&cursor) {
            if !walked.insert(cursor) {
                return false;
            }
            cursor = base;
        }
        stds.contains_key(&cursor)
    })
}

fn parse_std<'a>(
    parse_ctx: &ParseCtx<'_>,
    record: &'a [u8],
    cb_std_base: usize,
) -> Option<Std<'a>> {
    let first = get_u16(record, 0)?;
    let sti = first & 0x0FFF;
    let second = get_u16(record, 2)?;
    let istd_base = istd_base(second);
    let cupx = get_u16(record, 4).map_or(0, |v| v & 0x000F);

    // The name (Xstz) follows the fixed STDF area; UPX payloads follow the
    // name, each 2-byte aligned relative to the record start.
    let name_off = cb_std_base.max(10);
    let name_len = get_u16(record, name_off)? as usize;
    // Unicode name: length prefix + UTF-16 chars + terminator.
    let name_bytes = name_len.checked_mul(2)?;
    let name_units = record.get(name_off + 2..).and_then(|rest| rest.get(..name_bytes));
    let (name, replaced) = decode_utf16(utf16le_units(name_units.unwrap_or_default()));
    if replaced {
        parse_ctx.emit(DiagnosticCode::InvalidStyleEncoding, Region::Container);
    }
    let mut upx_pos = name_off.checked_add(4)?.checked_add(name_bytes)?;

    let none: &[u8] = &[];
    let mut upx = [none; 3];
    for index in 0..cupx {
        upx_pos = upx_pos.next_multiple_of(2);
        let cb = usize::from(get_u16(record, upx_pos)?);
        let payload = record.get(upx_pos + 2..).and_then(|rest| rest.get(..cb))?;
        if let Some(held) = upx.get_mut(usize::from(index)) {
            *held = payload;
        }
        upx_pos += 2 + cb;
    }

    let kind = Stk::of(second);
    // The UPX a style type holds, in order ([MS-DOC] StkParaGRLPUPX, StkCharGRLPUPX,
    // StkTableGRLPUPX, StkListGRLPUPX): paragraph PAPX, CHPX; character CHPX; table TAPX, PAPX,
    // CHPX; numbering PAPX.
    let [first, second, third] = upx;
    let (upx_papx, upx_chpx) = match kind {
        Stk::Paragraph => (first, second),
        Stk::Character => (none, first),
        Stk::Table => (none, third),
        Stk::Other => (none, none),
    };
    let block = blockstyle::from_style_name(&name);
    Some(Std { sti, istd_base, block, upx_papx, upx_chpx, kind })
}

/// Every style resolved over its `istdBase` chain from plain text in the fonts `defaults`, which an
/// istd no style holds also has. Each cycle is cut once,
/// at its lowest istd, which then resolves with no base, so a cyclic sheet
/// resolves one way whatever order its styles are visited in.
fn resolve_all(
    parse_ctx: &ParseCtx<'_>,
    stds: &BTreeMap<u16, Std<'_>>,
    defaults: Fonts,
) -> Result<Stylesheet, ConvertError> {
    let bases = acyclic_bases(parse_ctx, stds);
    let mut sheet =
        Stylesheet { resolved: HashMap::with_capacity(stds.len()), ..Stylesheet::default() };
    sheet.default.chp.fonts = defaults;
    for &istd in stds.keys() {
        resolve(parse_ctx, istd, stds, &bases, &mut sheet)?;
    }
    Ok(sheet)
}

/// Each style's base among the parsed styles, with every cycle cut at its
/// lowest istd and reported once as `style_cycle`; a style based on itself
/// has no base.
fn acyclic_bases(parse_ctx: &ParseCtx<'_>, stds: &BTreeMap<u16, Std<'_>>) -> HashMap<u16, u16> {
    let mut bases: HashMap<u16, u16> = stds
        .iter()
        .filter(|&(&istd, std)| {
            ![ISTD_NIL, istd].contains(&std.istd_base) && stds.contains_key(&std.istd_base)
        })
        .map(|(&istd, std)| (istd, std.istd_base))
        .collect();
    let mut walked: HashMap<u16, (usize, usize)> = HashMap::with_capacity(stds.len());
    for (walk, &start) in stds.keys().enumerate() {
        let mut path = Vec::new();
        let mut cursor = Some(start);
        while let Some(cur) = cursor {
            match walked.get(&cur) {
                Some(&(seen, at)) if seen == walk => {
                    if let Some(&lowest) = path.get(at..).and_then(|cycle| cycle.iter().min()) {
                        bases.remove(&lowest);
                    }
                    parse_ctx.emit(DiagnosticCode::StyleCycle, Region::Container);
                    break;
                }
                Some(_) => break,
                None => {}
            }
            walked.insert(cur, (walk, path.len()));
            path.push(cur);
            cursor = bases.get(&cur).copied();
        }
    }
    bases
}

/// Resolve `istd` over its acyclic chain, memoised in `sheet`; a style compiles only when first
/// resolved, charged one unit and its formatting's bytes before.
fn resolve(
    parse_ctx: &ParseCtx<'_>,
    istd: u16,
    stds: &BTreeMap<u16, Std<'_>>,
    bases: &HashMap<u16, u16>,
    sheet: &mut Stylesheet,
) -> Result<(), ConvertError> {
    let mut chain = Vec::new();
    let mut base = sheet.default.clone();
    let mut cursor = Some(istd);
    while let Some(cur) = cursor {
        if let Some(hit) = sheet.resolved.get(&cur) {
            base = hit.clone();
            break;
        }
        let Some(std) = stds.get(&cur) else {
            break;
        };
        chain.push((cur, std));
        cursor = bases.get(&cur).copied();
    }
    for &(cur, std) in chain.iter().rev() {
        parse_ctx.charge(1 + (std.upx_papx.len() + std.upx_chpx.len()) as u64)?;
        if let Some(papx) = std.upx_papx.get(2..) {
            let mut delta = PapDelta::default();
            apply_pap_sprms(parse_ctx, papx, &[], &mut delta)?;
            base.pap = base.pap.merge(delta);
        }
        let (own, forbidden) = compile_style_chpx(std.upx_chpx);
        #[cfg(test)]
        {
            sheet.compilations += 1;
        }
        if forbidden {
            parse_ctx.emit(DiagnosticCode::FormattingUnreadable, Region::Container);
        }
        base.chars = base.chars.then(own);
        base.chp = base.chars.apply(sheet.default.chp);
        base.kind = std.kind;
        if let sti @ 1..=9 = std.sti {
            base.heading = u8::try_from(sti).ok();
        }
        base.block = std.block.or(base.block);
        sheet.resolved.insert(cur, base.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::CancelToken;
    use crate::limits::{Limit, Limits};
    use crate::shared::test_support::{cancelled, diagnostic_rows};
    use std::assert_matches;

    fn style(sti: u16, istd_base: u16) -> Std<'static> {
        Std { sti, istd_base, block: None, upx_papx: &[], upx_chpx: &[], kind: Stk::Paragraph }
    }

    const BOLD: [u8; 3] = [0x35, 0x08, 0x01];

    const ITALIC: [u8; 3] = [0x36, 0x08, 0x01];

    const PAPX: [u8; 6] = [0, 0, 0x0B, 0x46, 2, 0];

    fn character(istd_base: u16, chpx: &[u8]) -> Std<'_> {
        Std { kind: Stk::Character, upx_chpx: chpx, ..style(0x0FFE, istd_base) }
    }

    fn bold(sti: u16, istd_base: u16) -> Std<'static> {
        Std { upx_chpx: &BOLD, ..style(sti, istd_base) }
    }

    fn cycles(parse_ctx: &ParseCtx<'_>) -> u64 {
        let diagnostics = parse_ctx.diagnostics();
        let cycles = diagnostics
            .iter()
            .filter(|d| (d.code, d.region) == (DiagnosticCode::StyleCycle, Region::Container));
        cycles.map(|d| d.count).sum()
    }

    #[test]
    fn every_style_resolves_over_the_default_fonts() {
        let defaults = Fonts { ascii: 1, east_asia: 2, other: 3, bidi: 4 };
        let stds = BTreeMap::from([
            (0u16, style(0, ISTD_NIL)),
            (1, bold(0x0FFE, 0)),
            (2, character(ISTD_NIL, &ITALIC)),
        ]);
        let sheet = resolve_all(&ParseCtx::default(), &stds, defaults).unwrap();
        for istd in 0..=2 {
            assert_eq!(sheet.get(istd).chp.fonts, defaults, "{istd}");
        }
    }

    #[test]
    fn deep_istd_base_chains_resolve_fully() {
        let mut stds = BTreeMap::new();
        stds.insert(0u16, style(3, ISTD_NIL));
        for i in 1u16..=40 {
            stds.insert(i, style(0x0FFE, i - 1));
        }
        assert_eq!(
            resolve_all(&ParseCtx::default(), &stds, Fonts::default()).unwrap().get(40).heading,
            Some(3)
        );
    }

    #[test]
    fn a_cycle_is_cut_at_its_lowest_istd_and_reported_once() {
        let stds = BTreeMap::from([
            (0u16, style(2, 3)),
            (3, style(0x0FFE, 5)),
            (5, bold(0x0FFE, 1)),
            (1, style(4, 3)),
            (7, style(0x0FFE, 7)),
            (8, style(0x0FFE, ISTD_NIL)),
        ]);
        let parse_ctx = ParseCtx::default();
        let sheet = resolve_all(&parse_ctx, &stds, Fonts::default()).unwrap();
        let shown = |istd| (sheet.get(istd).heading, sheet.get(istd).chp.style.bold);
        assert_eq!(shown(1), (Some(4), false));
        assert_eq!(shown(5), (Some(4), true));
        assert_eq!(shown(3), (Some(4), true));
        assert_eq!(shown(0), (Some(2), true));
        assert_eq!(shown(7), (None, false));
        assert_eq!(cycles(&parse_ctx), 1);
        assert_eq!(sheet.compilations, stds.len());
    }

    #[test]
    fn every_member_of_many_cycles_resolves_from_the_cut() {
        let pairs = 64u16;
        let stds: BTreeMap<u16, Std<'_>> = (0..pairs)
            .flat_map(|k| [(2 * k, style(1, 2 * k + 1)), (2 * k + 1, bold(0x0FFE, 2 * k))])
            .collect();
        let parse_ctx = ParseCtx::default();
        let sheet = resolve_all(&parse_ctx, &stds, Fonts::default()).unwrap();
        for k in 0..pairs {
            let [cut, based] = [2 * k, 2 * k + 1].map(|istd| sheet.get(istd));
            assert_eq!((cut.heading, cut.chp.style.bold), (Some(1), false), "istd {}", 2 * k);
            assert_eq!(
                (based.heading, based.chp.style.bold),
                (Some(1), true),
                "istd {}",
                2 * k + 1
            );
        }
        assert_eq!(cycles(&parse_ctx), u64::from(pairs));
    }

    const fn copy<T: Copy>() {}

    const _: () = copy::<CharOps>();

    #[test]
    fn a_deep_branching_sheet_compiles_each_style_once_and_composes_by_depth() {
        const CHAIN: u16 = 1024;
        const BRANCHES: u16 = 512;
        let invert = [0x35, 0x08, 0x81];
        for based_above in [false, true] {
            let base = |istd: u16| {
                if based_above {
                    Some(istd + 1).filter(|&above| above < CHAIN)
                } else {
                    istd.checked_sub(1)
                }
            };
            let depth = |istd: u16| if based_above { CHAIN - istd } else { istd + 1 };
            let mut stds = BTreeMap::new();
            for istd in 0..CHAIN {
                stds.insert(istd, character(base(istd).unwrap_or(ISTD_NIL), &invert));
            }
            for k in 0..BRANCHES {
                stds.insert(CHAIN + k, character(2 * k, &ITALIC));
            }
            let sheet = resolve_all(&ParseCtx::default(), &stds, Fonts::default()).unwrap();
            assert_eq!(sheet.compilations, stds.len(), "based above: {based_above}");
            let shown = |istd| sheet.character(istd).unwrap().apply(Chp::default()).style;
            for istd in 0..CHAIN {
                assert_eq!(shown(istd).bold, depth(istd) % 2 == 1, "istd {istd}");
            }
            for k in 0..BRANCHES {
                let branch = shown(CHAIN + k);
                assert_eq!((branch.bold, branch.italic), (depth(2 * k) % 2 == 1, true), "{k}");
            }
        }
    }

    fn sheet_of(records: &[Vec<u8>]) -> Vec<u8> {
        let mut stsh =
            [4, u16::try_from(records.len()).unwrap(), 10].map(u16::to_le_bytes).concat();
        for record in records {
            stsh.extend(u16::try_from(record.len()).unwrap().to_le_bytes());
            stsh.extend(record);
            stsh.resize(stsh.len().next_multiple_of(2), 0);
        }
        stsh
    }

    #[test]
    fn each_slot_is_charged_a_unit_and_its_record_before_the_record_is_read() {
        let records = [record(2, &[&BOLD]), Vec::new(), named(0xD800)];
        let stsh = sheet_of(&records);
        let parse_ctx = ParseCtx::default();
        let (stds, unread) = parse_stds(&parse_ctx, &stsh).unwrap();
        assert_eq!((stds.len(), unread), (2, Unread::Nothing));
        let bytes: usize = records.iter().map(Vec::len).sum();
        assert_eq!(parse_ctx.fuel(), 3 + bytes as u64);
        let limits = Limits { max_work: 2 + records[0].len() as u64, ..Limits::DEFAULT };
        let parse_ctx = ParseCtx::new(&limits);
        let refused = parse_stds(&parse_ctx, &stsh).map(drop);
        assert_matches!(refused, Err(ConvertError::ResourceLimit { limit: Limit::Work, .. }));
        assert!(parse_ctx.diagnostics().is_empty());
        let token = CancelToken::new();
        let cancelled = parse_stds(&cancelled(&token), &stsh).map(drop);
        assert_matches!(cancelled, Err(ConvertError::Cancelled));
    }

    #[test]
    fn a_truncated_style_record_bills_only_its_slot() {
        let mut stsh = sheet_of(&[Vec::new()]);
        stsh[6..8].copy_from_slice(&0x7FFFu16.to_le_bytes());
        stsh.push(0);
        let limits = Limits { max_work: 1, ..Limits::DEFAULT };
        let parse_ctx = ParseCtx::new(&limits);
        let (stds, unread) = parse_stds(&parse_ctx, &stsh).unwrap();
        assert!(stds.is_empty());
        assert_eq!(unread, Unread::Formatting);
        assert_eq!(parse_ctx.fuel(), 1);
    }

    #[test]
    fn an_odd_style_record_is_followed_by_padding_outside_its_length() {
        let records = [record(2, &[&BOLD]), record(2, &[&ITALIC])];
        assert!(records.iter().all(|record| record.len() % 2 == 1));
        let stsh = sheet_of(&records);
        let parse_ctx = ParseCtx::default();
        let (stds, unread) = parse_stds(&parse_ctx, &stsh).unwrap();
        assert_eq!(unread, Unread::Nothing);
        assert_eq!(stds.len(), 2);
        assert_eq!(stds[&0].upx_chpx, BOLD);
        assert_eq!(stds[&1].upx_chpx, ITALIC);
        assert_eq!(
            parse_ctx.fuel(),
            2 + records.iter().map(|record| record.len() as u64).sum::<u64>()
        );
    }

    #[test]
    fn each_style_is_charged_a_unit_and_its_formatting_before_it_compiles() {
        let stds = BTreeMap::from([
            (0u16, Std { upx_papx: &PAPX, ..bold(1, ISTD_NIL) }),
            (1, character(0, &ITALIC)),
            (2, character(1, &[])),
        ]);
        let parse_ctx = ParseCtx::default();
        assert_eq!(resolve_all(&parse_ctx, &stds, Fonts::default()).unwrap().compilations, 3);
        assert_eq!(parse_ctx.fuel(), (1 + 6 + 3) + (1 + 3) + 1);
        let limits = Limits { max_work: 14, ..Limits::DEFAULT };
        let parse_ctx = ParseCtx::new(&limits);
        let bases = acyclic_bases(&parse_ctx, &stds);
        let mut sheet = Stylesheet::default();
        let refused = resolve(&parse_ctx, 2, &stds, &bases, &mut sheet);
        assert_matches!(refused, Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 14 }));
        assert_eq!(sheet.compilations, 2);
        let token = CancelToken::new();
        assert_matches!(
            resolve_all(&cancelled(&token), &stds, Fonts::default()),
            Err(ConvertError::Cancelled)
        );
    }

    #[test]
    fn a_style_sheet_refused_or_cancelled_fails_rather_than_reading_as_unreadable() {
        let record = record(2, &[&ITALIC]);
        let stsh = sheet_of(std::slice::from_ref(&record));
        let mut fib = [0u8; super::super::FIB_MIN];
        let lcb = u32::try_from(stsh.len()).unwrap().to_le_bytes();
        fib[Fib::FC_STSHF + 4..Fib::FC_STSHF + 8].copy_from_slice(&lcb);
        let parse = |parse_ctx: &ParseCtx<'_>, has_lists| {
            let parsed = parse(parse_ctx, Fib(&fib), &stsh, has_lists).map(|_| ());
            (parsed, parse_ctx.diagnostics().is_empty())
        };
        for has_lists in [false, true] {
            for max_work in [0, 1 + record.len() as u64] {
                let limits = Limits { max_work, ..Limits::DEFAULT };
                let refused = parse(&ParseCtx::new(&limits), has_lists);
                let limit = Err(ConvertError::limit(Limit::Work, max_work));
                assert_eq!(refused, (limit, true), "{max_work} {has_lists}");
            }
            let token = CancelToken::new();
            let refused = parse(&cancelled(&token), has_lists);
            assert_eq!(refused, (Err(ConvertError::Cancelled), true), "{has_lists}");
        }
    }

    #[test]
    fn a_character_lookup_finds_character_styles_only() {
        let stds = BTreeMap::from([(0u16, bold(1, ISTD_NIL)), (1, character(ISTD_NIL, &BOLD))]);
        let sheet = resolve_all(&ParseCtx::default(), &stds, Fonts::default()).unwrap();
        assert_eq!(sheet.character(0), None);
        assert!(sheet.character(1).is_some());
        assert_eq!(sheet.character(2), None);
        assert!(sheet.get(0).chp.style.bold);
    }

    fn record(sgc: u16, upx: &[&[u8]]) -> Vec<u8> {
        let mut record = vec![0u8; 10];
        record[2] = u8::try_from(sgc).unwrap();
        record[4] = u8::try_from(upx.len()).unwrap();
        record.extend_from_slice(&[0; 4]);
        for payload in upx {
            record.resize(record.len().next_multiple_of(2), 0);
            record.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_le_bytes());
            record.extend_from_slice(payload);
        }
        record
    }

    #[test]
    fn each_style_type_reads_the_upx_it_holds_as_its_chpx() {
        let papx: &[u8] = &PAPX;
        let deleting: &[u8] = &[0x00, 0x08, 1];
        type Case<'a> = (&'a str, u16, Vec<&'a [u8]>, &'a [u8]);
        let cases: [Case<'_>; 7] = [
            ("paragraph", 1, vec![papx, &ITALIC], &ITALIC),
            ("character", 2, vec![&ITALIC], &ITALIC),
            ("table", 3, vec![deleting, papx, &ITALIC], &ITALIC),
            ("numbering", 4, vec![deleting], &[]),
            ("a short table", 3, vec![deleting, papx], &[]),
            ("no type", 0, vec![deleting, deleting], &[]),
            ("an unknown type", 5, vec![deleting, deleting], &[]),
        ];
        for (case, sgc, upx, chpx) in cases {
            let parse_ctx = ParseCtx::default();
            let bytes = record(sgc, &upx);
            let std = parse_std(&parse_ctx, &bytes, 10).unwrap();
            assert_eq!(std.upx_chpx, chpx, "{case}");
            assert_eq!(std.upx_papx, if sgc == 1 { papx } else { &[] }, "{case}");
            let sheet =
                resolve_all(&parse_ctx, &BTreeMap::from([(0u16, std)]), Fonts::default()).unwrap();
            assert_eq!(sheet.get(0).chp.style.italic, !chpx.is_empty(), "{case}");
            assert!(parse_ctx.diagnostics().is_empty(), "{case}");
        }
    }

    #[test]
    fn a_declared_upx_past_the_third_must_still_lie_inside_the_record() {
        let whole = record(2, &[&ITALIC[..]; 4]);
        let parse_ctx = ParseCtx::default();
        assert!(parse_std(&parse_ctx, &whole, 10).is_some());
        assert!(parse_std(&parse_ctx, &whole[..whole.len() - 1], 10).is_none());
    }

    #[test]
    fn a_forbidden_property_is_reported_once_for_the_style_that_holds_it() {
        let deleting = [0x00, 0x08, 1];
        let held = [&BOLD[..], &deleting, &deleting].concat();
        let stds = BTreeMap::from([
            (0u16, character(ISTD_NIL, &held)),
            (1, character(0, &ITALIC)),
            (2, character(ISTD_NIL, &deleting)),
        ]);
        let parse_ctx = ParseCtx::default();
        let sheet = resolve_all(&parse_ctx, &stds, Fonts::default()).unwrap();
        let reported: Vec<(DiagnosticCode, Region, u64)> =
            diagnostic_rows(&parse_ctx.diagnostics());
        assert_eq!(reported, [(DiagnosticCode::FormattingUnreadable, Region::Container, 2)]);
        let child = sheet.character(1).unwrap().apply(Chp::default()).style;
        assert_eq!((child.bold, child.italic), (true, true));
    }

    /// A paragraph style record named by the one UTF-16 `unit`.
    fn named(unit: u16) -> Vec<u8> {
        let mut record = vec![0u8; 10];
        record[2] = 0x01;
        record.extend_from_slice(&1u16.to_le_bytes());
        record.extend_from_slice(&unit.to_le_bytes());
        record.extend_from_slice(&[0, 0]);
        record
    }

    #[test]
    fn a_style_name_with_an_unpaired_surrogate_is_reported() {
        let parse_ctx = ParseCtx::default();
        assert!(parse_std(&parse_ctx, &named(0x41), 10).is_some());
        assert!(parse_ctx.diagnostics().is_empty());
        assert!(parse_std(&parse_ctx, &named(0xD800), 10).is_some());
        let reported: Vec<(DiagnosticCode, Region, u64)> =
            diagnostic_rows(&parse_ctx.diagnostics());
        assert_eq!(reported, [(DiagnosticCode::InvalidStyleEncoding, Region::Container, 1)]);
    }

    #[test]
    fn a_style_sheet_cut_short_does_not_read_in_full() {
        let unread = |stsh: &[u8]| parse_stds(&ParseCtx::default(), stsh).unwrap().1;
        assert_eq!(unread(&[4, 0, 1, 0, 10, 0, 0, 0]), Unread::Nothing);
        for (case, stsh) in [
            ("a header too short", &[4, 0, 1, 0][..]),
            ("a missing record length", &[4, 0, 2, 0, 10, 0, 0, 0]),
            ("a record past the end", &[4, 0, 1, 0, 10, 0, 20, 0, 1, 2, 3]),
        ] {
            assert_eq!(unread(stsh), Unread::Formatting, "{case}");
        }
    }

    fn refused(sgc: u16, istd_base: u16) -> Vec<u8> {
        [&[0, 0][..], &(sgc | (istd_base << 4)).to_le_bytes()].concat()
    }

    fn based(mut record: Vec<u8>, istd_base: u16) -> Vec<u8> {
        let sgc = u16::from(record[2] & 0x0F);
        record[2..4].copy_from_slice(&(sgc | (istd_base << 4)).to_le_bytes());
        record
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of style sheets that stop short")]
    fn an_unread_style_sheet_is_lost_unless_nothing_in_it_can_hide_text() {
        use DiagnosticCode::{PartUnreadable, StylePartUnreadable};
        type Row = (DiagnosticCode, Region, u64);
        type Case = (&'static str, Vec<u8>, u32, [&'static [Row]; 2]);
        const READ: [&[Row]; 2] = [&[], &[]];
        const FORMATTING: [&[Row]; 2] =
            [&[(StylePartUnreadable, Region::Container, 1)], &[(PartUnreadable, Region::Body, 1)]];
        const TEXT: [&[Row]; 2] = [&[(PartUnreadable, Region::Body, 1)]; 2];
        let bold = || record(2, &[&BOLD]);
        let cut_after_bare = {
            let mut stsh = sheet_of(&[refused(4, ISTD_NIL)]);
            stsh[2] = 2;
            stsh
        };
        let sheets: [Case; 26] = [
            ("read in full", sheet_of(&[bold()]), 0, READ),
            ("an empty range", Vec::new(), 0, READ),
            ("a range past the table", sheet_of(&[bold()]), 1, TEXT),
            ("a header too short", vec![4, 0, 1, 0], 0, TEXT),
            ("a missing record length", vec![4, 0, 2, 0, 10, 0, 0, 0], 0, TEXT),
            ("a record past the end", vec![4, 0, 1, 0, 10, 0, 20, 0, 1, 2, 3], 0, TEXT),
            ("a paragraph style cut short", sheet_of(&[refused(1, ISTD_NIL)]), 0, TEXT),
            ("a character style cut short", sheet_of(&[refused(2, ISTD_NIL)]), 0, TEXT),
            ("a table style cut short", sheet_of(&[refused(3, ISTD_NIL)]), 0, TEXT),
            ("a record too short to show its type", sheet_of(&[vec![0, 0, 1]]), 0, TEXT),
            ("an untyped style cut short", sheet_of(&[refused(0, ISTD_NIL)]), 0, FORMATTING),
            ("a numbering style cut short", sheet_of(&[refused(4, ISTD_NIL)]), 0, FORMATTING),
            ("an unknown type cut short", sheet_of(&[refused(5, ISTD_NIL)]), 0, FORMATTING),
            (
                "a cut numbering style that a later style is based on",
                sheet_of(&[refused(4, ISTD_NIL), bold()]),
                0,
                TEXT,
            ),
            (
                "a cut numbering style that an earlier style is based on",
                sheet_of(&[based(bold(), 1), refused(4, ISTD_NIL)]),
                0,
                TEXT,
            ),
            (
                "a cut numbering style that no style is based on",
                sheet_of(&[refused(4, ISTD_NIL), based(bold(), ISTD_NIL)]),
                0,
                FORMATTING,
            ),
            (
                "a cut paragraph style after a cut numbering style",
                sheet_of(&[refused(4, ISTD_NIL), refused(1, ISTD_NIL)]),
                0,
                TEXT,
            ),
            (
                "a cut paragraph style before a cut numbering style",
                sheet_of(&[refused(1, ISTD_NIL), refused(4, ISTD_NIL)]),
                0,
                TEXT,
            ),
            ("a missing record after a cut numbering style", cut_after_bare, 0, TEXT),
            (
                "a cut numbering style based on a later style",
                sheet_of(&[refused(4, 1), bold()]),
                0,
                TEXT,
            ),
            (
                "a cut numbering style based on an earlier style",
                sheet_of(&[bold(), refused(4, 0)]),
                0,
                TEXT,
            ),
            (
                "a cut numbering style based through another on a style",
                sheet_of(&[refused(4, 1), refused(4, 2), bold()]),
                0,
                TEXT,
            ),
            ("a cut numbering style based on itself", sheet_of(&[refused(4, 0)]), 0, FORMATTING),
            ("a cut numbering style based on no record", sheet_of(&[refused(4, 5)]), 0, FORMATTING),
            (
                "a cut numbering style based on an empty slot",
                sheet_of(&[refused(4, 1), Vec::new()]),
                0,
                FORMATTING,
            ),
            (
                "cut numbering styles based on each other",
                sheet_of(&[refused(4, 1), refused(4, 0)]),
                0,
                FORMATTING,
            ),
        ];
        for (case, stsh, fc, expected) in sheets {
            let mut fib = [0u8; super::super::FIB_MIN];
            fib[Fib::FC_STSHF..][..4].copy_from_slice(&fc.to_le_bytes());
            let lcb = u32::try_from(stsh.len()).unwrap().to_le_bytes();
            fib[Fib::FC_STSHF + 4..][..4].copy_from_slice(&lcb);
            for (has_lists, expected) in [false, true].into_iter().zip(expected) {
                let parse_ctx = ParseCtx::default();
                parse(&parse_ctx, Fib(&fib), &stsh, has_lists).unwrap();
                let reported: Vec<Row> = diagnostic_rows(&parse_ctx.diagnostics());
                assert_eq!(reported, expected, "{case}, lists: {has_lists}");
            }
        }
    }
}
