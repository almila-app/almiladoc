//! Full STSH (style sheet) parsing per MS-DOC: STD records with their
//! `istdBase` inheritance chains and UPX formatting payloads, resolved into
//! effective per-style character formatting and paragraph properties.

use super::Fib;
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::formats::doc::sprm::{PapDelta, apply_pap_sprms, apply_style_chpx};
use crate::model::Style;
use crate::shared::binary::{decode_utf16, get_u16, utf16le_units};
use crate::shared::blockstyle::{self, BlockStyle};
use std::collections::{BTreeMap, HashMap};

const ISTD_NIL: u16 = 0x0FFF;

#[derive(Debug, Clone, Default)]
pub struct ResolvedStyle {
    /// Effective character formatting of the chain.
    pub chp: Style,
    /// Effective paragraph properties of the chain.
    pub pap: PapDelta,
    /// Heading level for built-in `heading N` styles.
    pub heading: Option<u8>,
    /// The block container the style's name designates.
    pub block: Option<BlockStyle>,
}

#[derive(Debug, Default)]
pub struct Stylesheet {
    resolved: HashMap<u16, ResolvedStyle>,
    /// Returned for unknown istds so `get` never allocates.
    default: ResolvedStyle,
}

impl Stylesheet {
    pub fn get(&self, istd: u16) -> &ResolvedStyle {
        self.resolved.get(&istd).unwrap_or(&self.default)
    }
}

struct Std {
    sti: u16,
    istd_base: u16,
    /// The block container this style's name designates.
    block: Option<BlockStyle>,
    /// Paragraph style: (papx grpprl incl. leading istd, chpx grpprl);
    /// character style: (empty, chpx grpprl).
    upx_papx: Vec<u8>,
    upx_chpx: Vec<u8>,
    is_paragraph: bool,
}

/// The style sheet; one that does not read in full is `part_unreadable`
/// when the document has list tables, whose labels styles can carry, and
/// `style_part_unreadable` otherwise.
pub fn parse(parse_ctx: &ParseCtx, fib: Fib<'_>, table: &[u8], has_lists: bool) -> Stylesheet {
    let (stds, complete) = match fib.table_range(table, 0xA2) {
        Some([]) => (BTreeMap::new(), true),
        Some(stsh) => parse_stds(parse_ctx, stsh),
        None => (BTreeMap::new(), false),
    };
    if !complete {
        if has_lists {
            parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Body);
        } else {
            parse_ctx.emit(DiagnosticCode::StylePartUnreadable, Region::Container);
        }
    }
    resolve_all(parse_ctx, &stds)
}

/// The style records of an `STSH`, and whether every one read.
fn parse_stds(parse_ctx: &ParseCtx, stsh: &[u8]) -> (BTreeMap<u16, Std>, bool) {
    let mut stds = BTreeMap::new();
    let (Some(cb_stshi), Some(cstd), Some(cb_std_base)) =
        (get_u16(stsh, 0), get_u16(stsh, 2), get_u16(stsh, 4))
    else {
        return (stds, false);
    };
    let mut pos = 2 + cb_stshi as usize;
    let mut complete = true;
    for istd in 0..cstd {
        let Some(cb_std) = get_u16(stsh, pos) else {
            return (stds, false);
        };
        pos += 2;
        if cb_std == 0 {
            continue;
        }
        let Some(record) = stsh.get(pos..).and_then(|rest| rest.get(..cb_std as usize)) else {
            return (stds, false);
        };
        pos += cb_std as usize;
        match parse_std(parse_ctx, record, cb_std_base as usize) {
            Some(std) => drop(stds.insert(istd, std)),
            None => complete = false,
        }
    }
    (stds, complete)
}

fn parse_std(parse_ctx: &ParseCtx, record: &[u8], cb_std_base: usize) -> Option<Std> {
    let first = get_u16(record, 0)?;
    let sti = first & 0x0FFF;
    let second = get_u16(record, 2)?;
    let sgc = second & 0x000F; // 1 = paragraph, 2 = character
    let istd_base = (second >> 4) & 0x0FFF;
    let cupx = get_u16(record, 4).map(|v| v & 0x000F).unwrap_or(0);

    // The name (Xstz) follows the fixed STDF area; UPX payloads follow the
    // name, each 2-byte aligned relative to the record start.
    let name_off = cb_std_base.max(10);
    let name_len = get_u16(record, name_off)? as usize;
    // Unicode name: length prefix + UTF-16 chars + terminator.
    let name_bytes = name_len.checked_mul(2)?;
    let name_units: Vec<u16> = utf16le_units(
        record
            .get(name_off..)
            .and_then(|rest| rest.get(2..))
            .and_then(|rest| rest.get(..name_bytes))
            .unwrap_or_default(),
    )
    .collect();
    let (name, replaced) = decode_utf16(name_units);
    if replaced {
        parse_ctx.emit(DiagnosticCode::InvalidStyleEncoding, Region::Container);
    }
    let mut upx_pos = name_off.checked_add(4)?.checked_add(name_bytes)?;

    let mut upx: Vec<&[u8]> = Vec::new();
    for _ in 0..cupx {
        if upx_pos % 2 == 1 {
            upx_pos += 1;
        }
        let cb = get_u16(record, upx_pos)? as usize;
        let payload = record.get(upx_pos..)?.get(2..)?.get(..cb)?;
        upx.push(payload);
        upx_pos += 2 + cb;
    }

    let is_paragraph = sgc == 1;
    let (upx_papx, upx_chpx) = if is_paragraph {
        (
            upx.first().copied().unwrap_or_default().to_vec(),
            upx.get(1).copied().unwrap_or_default().to_vec(),
        )
    } else {
        (Vec::new(), upx.first().copied().unwrap_or_default().to_vec())
    };
    Some(Std {
        sti,
        istd_base,
        block: blockstyle::from_style_name(&name),
        upx_papx,
        upx_chpx,
        is_paragraph,
    })
}

/// Every style resolved over its `istdBase` chain. Each cycle is cut once,
/// at its lowest istd, which then resolves with no base, so a cyclic sheet
/// resolves one way whatever order its styles are visited in.
fn resolve_all(parse_ctx: &ParseCtx, stds: &BTreeMap<u16, Std>) -> Stylesheet {
    let bases = acyclic_bases(parse_ctx, stds);
    let mut resolved = HashMap::with_capacity(stds.len());
    for &istd in stds.keys() {
        resolve(istd, stds, &bases, &mut resolved);
    }
    Stylesheet { resolved, default: ResolvedStyle::default() }
}

/// Each style's base among the parsed styles, with every cycle cut at its
/// lowest istd and reported once as `style_cycle`; a style based on itself
/// has no base.
fn acyclic_bases(parse_ctx: &ParseCtx, stds: &BTreeMap<u16, Std>) -> HashMap<u16, u16> {
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

/// Resolve `istd` over its acyclic chain, memoised in `resolved`.
fn resolve(
    istd: u16,
    stds: &BTreeMap<u16, Std>,
    bases: &HashMap<u16, u16>,
    resolved: &mut HashMap<u16, ResolvedStyle>,
) {
    let mut chain = Vec::new();
    let mut base = ResolvedStyle::default();
    let mut cursor = Some(istd);
    while let Some(cur) = cursor {
        if let Some(hit) = resolved.get(&cur) {
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
        if let Some(papx) = std.upx_papx.get(2..).filter(|_| std.is_paragraph) {
            let mut delta = PapDelta::default();
            apply_pap_sprms(papx, &[], &mut delta);
            base.pap = base.pap.merge(delta);
        }
        base.chp = apply_style_chpx(&std.upx_chpx, base.chp);
        if let sti @ 1..=9 = std.sti {
            base.heading = u8::try_from(sti).ok();
        }
        base.block = std.block.or(base.block);
        resolved.insert(cur, base.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(sti: u16, istd_base: u16) -> Std {
        Std {
            sti,
            istd_base,
            block: None,
            upx_papx: Vec::new(),
            upx_chpx: Vec::new(),
            is_paragraph: true,
        }
    }

    const BOLD: [u8; 3] = [0x35, 0x08, 0x01];

    fn bold(mut std: Std) -> Std {
        std.upx_chpx = BOLD.to_vec();
        std
    }

    fn cycles(parse_ctx: &ParseCtx) -> u64 {
        let diagnostics = parse_ctx.diagnostics();
        let cycles = diagnostics
            .iter()
            .filter(|d| (d.code, d.region) == (DiagnosticCode::StyleCycle, Region::Container));
        cycles.map(|d| d.count).sum()
    }

    #[test]
    fn deep_istd_base_chains_resolve_fully() {
        let mut stds = BTreeMap::new();
        stds.insert(0u16, style(3, ISTD_NIL));
        for i in 1u16..=40 {
            stds.insert(i, style(0x0FFE, i - 1));
        }
        assert_eq!(resolve_all(&ParseCtx::default(), &stds).get(40).heading, Some(3));
    }

    #[test]
    fn a_cycle_is_cut_at_its_lowest_istd_and_reported_once() {
        let stds = BTreeMap::from([
            (0u16, style(2, 3)),
            (3, style(0x0FFE, 5)),
            (5, bold(style(0x0FFE, 1))),
            (1, style(4, 3)),
            (7, style(0x0FFE, 7)),
            (8, style(0x0FFE, ISTD_NIL)),
        ]);
        let parse_ctx = ParseCtx::default();
        let sheet = resolve_all(&parse_ctx, &stds);
        let shown = |istd| (sheet.get(istd).heading, sheet.get(istd).chp.bold);
        assert_eq!(shown(1), (Some(4), false));
        assert_eq!(shown(5), (Some(4), true));
        assert_eq!(shown(3), (Some(4), true));
        assert_eq!(shown(0), (Some(2), true));
        assert_eq!(shown(7), (None, false));
        assert_eq!(cycles(&parse_ctx), 1);
    }

    #[test]
    fn every_member_of_many_cycles_resolves_from_the_cut() {
        let pairs = 64u16;
        let stds: BTreeMap<u16, Std> = (0..pairs)
            .flat_map(|k| [(2 * k, style(1, 2 * k + 1)), (2 * k + 1, bold(style(0x0FFE, 2 * k)))])
            .collect();
        let parse_ctx = ParseCtx::default();
        let sheet = resolve_all(&parse_ctx, &stds);
        for k in 0..pairs {
            let [cut, based] = [2 * k, 2 * k + 1].map(|istd| sheet.get(istd));
            assert_eq!((cut.heading, cut.chp.bold), (Some(1), false), "istd {}", 2 * k);
            assert_eq!((based.heading, based.chp.bold), (Some(1), true), "istd {}", 2 * k + 1);
        }
        assert_eq!(cycles(&parse_ctx), u64::from(pairs));
    }

    #[test]
    fn a_style_name_with_an_unpaired_surrogate_is_reported() {
        let record = |unit: u16| {
            let mut record = vec![0u8; 10];
            record[2] = 0x01;
            record.extend_from_slice(&1u16.to_le_bytes());
            record.extend_from_slice(&unit.to_le_bytes());
            record.extend_from_slice(&[0, 0]);
            record
        };
        let parse_ctx = ParseCtx::default();
        assert!(parse_std(&parse_ctx, &record(0x41), 10).is_some());
        assert!(parse_ctx.diagnostics().is_empty());
        assert!(parse_std(&parse_ctx, &record(0xD800), 10).is_some());
        let reported: Vec<_> =
            parse_ctx.diagnostics().iter().map(|d| (d.code, d.region, d.count)).collect();
        assert_eq!(reported, [(DiagnosticCode::InvalidStyleEncoding, Region::Container, 1)]);
    }

    #[test]
    fn a_style_sheet_cut_short_does_not_read_in_full() {
        let complete = |stsh: &[u8]| parse_stds(&ParseCtx::default(), stsh).1;
        assert!(complete(&[4, 0, 1, 0, 10, 0, 0, 0]));
        for (case, stsh) in [
            ("a header too short", &[4, 0, 1, 0][..]),
            ("a missing record length", &[4, 0, 2, 0, 10, 0, 0, 0]),
            ("a record past the end", &[4, 0, 1, 0, 10, 0, 20, 0, 1, 2, 3]),
        ] {
            assert!(!complete(stsh), "{case}");
        }
    }
}
