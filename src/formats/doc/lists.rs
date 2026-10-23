//! Word binary list tables: `PlfLst` (list definitions with per-level
//! formats) and `PlfLfo` (list format overrides referenced by `sprmPIlfo`),
//! resolved per [MS-DOC]: each list keeps its identity (`lsid`) so numbering
//! state is shared by every LFO referencing the same list, levels keep their
//! restart limits (`fNoRestart`/`ilvlRestartLim`), LFOLVL start-at overrides
//! apply when their LFO is first used, and the number text (`xst` with
//! `rgbxchNums` placeholders) is preserved for composite markers.

use super::Fib;
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::shared::binary::{get_u16, get_u32, utf16le_units};
use crate::shared::list::MarkerKind;
use crate::shared::numbering::{
    LevelCounters, NumberPattern, NumberText, composite_label, marker_for_nfc,
};
use std::collections::HashMap;

pub(super) const LEVELS: usize = crate::shared::numbering::OFFICE_LEVELS;

#[derive(Debug, Clone)]
pub(super) struct LevelDef {
    pub marker: Option<MarkerKind>,
    pub start: u64,
    /// Restart rule, as in WordprocessingML `lvlRestart`: `None` = restart
    /// after any more significant level (`fNoRestart` clear), `Some(0)` =
    /// never restart, `Some(n)` = restart when a level with ilvl < n is used
    /// (`fNoRestart` set with `ilvlRestartLim`).
    pub restart: Option<u32>,
    /// The level's number text (`xst` with `rgbxchNums` placeholders and
    /// LVLF `fLegal`); empty for bullets and malformed levels.
    pub pattern: NumberPattern,
    /// The table-stream offset of the LVL when its number text holds an
    /// unpaired surrogate: reported where a label shows it, once per region.
    pub damaged_at: Option<usize>,
}

impl Default for LevelDef {
    fn default() -> Self {
        Self {
            marker: Some(MarkerKind::Bullet),
            start: 1,
            restart: None,
            pattern: NumberPattern::default(),
            damaged_at: None,
        }
    }
}

/// The numbering sequence a list continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ListId {
    /// A list the `PlfLst` defines, by its `lsid`.
    Lsid(u32),
    /// The stand-in for an ilfo whose list no table defines.
    UnknownLfo(u16),
}

impl ListId {
    /// The list's assembly identity, every stand-in above every `lsid`.
    pub(super) fn instance(self) -> u64 {
        match self {
            Self::Lsid(lsid) => u64::from(lsid),
            Self::UnknownLfo(ilfo) => (1 << 32) | u64::from(ilfo),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ListDef {
    /// List identity: numbering state is keyed by this, not the ilfo, so
    /// every override referencing the same list continues its sequence.
    pub id: ListId,
    pub levels: [LevelDef; LEVELS],
    /// LFOLVL start-at overrides; each restarts the shared sequence at its
    /// value when this LFO is first used at that level.
    pub start_override: [Option<u64>; LEVELS],
}

impl ListDef {
    pub(super) fn level(&self, ilvl: usize) -> &LevelDef {
        &self.levels[LevelCounters::<LEVELS>::clamp(ilvl)]
    }

    /// Fallback definition for an ilfo with no parsed list behind it, under
    /// an identity of its own.
    pub(super) fn unknown(ilfo: u16) -> Self {
        Self {
            id: ListId::UnknownLfo(ilfo),
            levels: std::array::from_fn(|_| LevelDef::default()),
            start_override: [None; LEVELS],
        }
    }
}

/// The resolved definition of each 1-based ilfo.
pub(super) type Lists = HashMap<u16, ListDef>;

/// List tables that do not read in full are `part_unreadable`: the labels
/// of the lists they lose do not render. A level's undecodable number text
/// is reported where a paragraph shows it, not here.
pub(super) fn parse(parse_ctx: &ParseCtx<'_>, fib: Fib<'_>, table: &[u8]) -> Lists {
    let lst_fc = fib.size(Fib::FC_PLF_LST);
    let lst_lcb = fib.size(Fib::LCB_PLF_LST);
    let lfo_fc = fib.size(Fib::FC_PLF_LFO);
    let lfo_lcb = fib.size(Fib::LCB_PLF_LFO);

    let mut lists = Lists::default();
    if lst_lcb == 0 {
        return lists;
    }
    // lcbPlfLst covers only the LSTF array; each list's LVL structures
    // follow it in the table stream, uncounted.
    let (by_lsid, lst_complete) = parse_plf_lst(table, lst_fc, lst_lcb);
    let lfo_complete = lfo_lcb == 0
        || table
            .get(lfo_fc..lfo_fc.saturating_add(lfo_lcb))
            .is_some_and(|plf_lfo| parse_plf_lfo(plf_lfo, lfo_fc, &by_lsid, &mut lists));
    if !(lst_complete && lfo_complete) {
        parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Body);
    }
    lists
}

const LSTF_SIZE: usize = 28;

/// Each list's levels by `lsid`, and whether every list read.
fn parse_plf_lst(table: &[u8], fc: usize, lcb: usize) -> (HashMap<u32, [LevelDef; LEVELS]>, bool) {
    let mut out = HashMap::new();
    let Some(plf) = table.get(fc..).filter(|plf| plf.len() >= lcb) else {
        return (out, false);
    };
    let Some(count) = get_u16(plf, 0) else {
        return (out, false);
    };
    let (lstfs, _) = plf.get(2..).unwrap_or_default().as_chunks::<LSTF_SIZE>();
    let Some(lstfs) = lstfs.get(..usize::from(count)) else {
        return (out, false);
    };
    // The LVL array begins where lcbPlfLst ends, still relative to `plf`.
    let mut pos = lcb;
    for lstf in lstfs {
        let Some((lsid, levels)) = parse_list(plf, lstf, &mut pos, fc) else {
            return (out, false);
        };
        out.insert(lsid, levels);
    }
    (out, true)
}

/// The `lsid` and levels of the list `lstf` declares, whose LVLs follow at `pos`.
fn parse_list(
    plf: &[u8],
    lstf: &[u8; LSTF_SIZE],
    pos: &mut usize,
    origin: usize,
) -> Option<(u32, [LevelDef; LEVELS])> {
    let simple = lstf[26] & 0x01 != 0;
    let mut levels: [LevelDef; LEVELS] = std::array::from_fn(|_| LevelDef::default());
    for slot in levels.iter_mut().take(if simple { 1 } else { LEVELS }) {
        let (def, next) = parse_lvl(plf, *pos, origin)?;
        *slot = def;
        *pos = next;
    }
    if simple {
        let def = levels[0].clone();
        levels = std::array::from_fn(|_| def.clone());
    }
    Some((get_u32(lstf, 0)?, levels))
}

/// One LVL at `pos` of `plf`, which starts at table-stream offset `origin`:
/// an LVLF header, then PAPX and CHPX grpprls, then the number text, whose
/// unpaired surrogates mark the level damaged.
fn parse_lvl(plf: &[u8], pos: usize, origin: usize) -> Option<(LevelDef, usize)> {
    const LVLF_SIZE: usize = 28;
    let record = plf.get(pos..)?;
    let header = record.first_chunk::<LVLF_SIZE>()?;
    let start = u64::from(get_u32(header, 0)?);
    let nfc = header[4];
    let flags = header[5];
    let legal = flags & 0x04 != 0;
    let no_restart = flags & 0x08 != 0;
    // rgbxchNums: one-based offsets of number placeholders within xst,
    // zero-terminated unless full.
    let placeholders = header[6..15].split(|&b| b == 0).next().unwrap_or_default();
    let restart_lim = header[26];
    let cb_papx = header[25] as usize;
    let cb_chpx = header[24] as usize;
    let mut next = LVLF_SIZE.checked_add(cb_papx)?.checked_add(cb_chpx)?;
    let xst_len = 2 * usize::from(get_u16(record, next)?);
    next = next.checked_add(2)?;
    let marker = marker_for_nfc(i32::from(nfc));
    let mut text: Vec<NumberText> = Vec::new();
    let mut replaced = false;
    if marker.is_some_and(MarkerKind::ordered) {
        let xst = record.get(next..)?.get(..xst_len)?;
        let mut i = 0usize;
        for unit in char::decode_utf16(utf16le_units(xst)) {
            let c = unit.unwrap_or_else(|_| {
                replaced = true;
                char::REPLACEMENT_CHARACTER
            });
            i += c.len_utf16();
            if let Some(level) = u8::try_from(u32::from(c)).ok().filter(|&level| level <= 0x08)
                && u8::try_from(i).is_ok_and(|at| placeholders.contains(&at))
            {
                text.push(NumberText::Level(level));
            } else if !c.is_control() {
                match text.last_mut() {
                    Some(NumberText::Literal(s)) => s.push(c),
                    _ => text.push(NumberText::Literal(c.to_string())),
                }
            }
        }
    }
    next = next.checked_add(xst_len)?;
    let restart = no_restart.then_some(u32::from(restart_lim));
    let damaged_at = replaced.then(|| origin.saturating_add(pos));
    Some((
        LevelDef { marker, start, restart, pattern: NumberPattern { text, legal }, damaged_at },
        pos.checked_add(next)?,
    ))
}

const LFO_SIZE: usize = 16;

/// Resolve each LFO of `plf`, which starts at table-stream offset `origin`,
/// into `lists`, and whether every LFOLVL read. Each LFO's LFOData opens
/// with a 4-byte `cp`, overrides or none; one missing at the end loses
/// nothing.
fn parse_plf_lfo(
    plf: &[u8],
    origin: usize,
    by_lsid: &HashMap<u32, [LevelDef; LEVELS]>,
    lists: &mut Lists,
) -> bool {
    let Some(count) = get_u32(plf, 0).map(|v| v as usize) else {
        return false;
    };
    let (records, _) = plf.get(4..).unwrap_or_default().as_chunks::<LFO_SIZE>();
    let Some(records) = records.get(..count) else {
        return false;
    };
    let mut pos = 4 + records.len() * LFO_SIZE;
    let mut complete = true;
    for (i, record) in records.iter().enumerate() {
        let (Ok(ilfo), Some(lsid)) = (u16::try_from(i + 1), get_u32(record, 0)) else {
            break;
        };
        let mut def = match by_lsid.get(&lsid) {
            Some(levels) => ListDef {
                id: ListId::Lsid(lsid),
                levels: levels.clone(),
                start_override: [None; LEVELS],
            },
            None => ListDef::unknown(ilfo),
        };
        pos = pos.saturating_add(4);
        let read = (0..record[12]).try_for_each(|_| read_lfolvl(plf, &mut pos, origin, &mut def));
        complete &= read.is_some();
        lists.insert(ilfo, def);
    }
    complete
}

/// Apply the LFOLVL at `pos` of `plf` (and the LVL it embeds) to `def`, and
/// move `pos` past them; a failed embedded LVL leaves `pos` after the LFOLVL.
fn read_lfolvl(plf: &[u8], pos: &mut usize, origin: usize, def: &mut ListDef) -> Option<()> {
    let record = plf.get(*pos..)?.first_chunk::<8>()?;
    let start_at = get_u32(record, 0)?;
    let ilvl = usize::from(record[4] & 0x0F);
    let f_start_at = record[4] & 0x10 != 0;
    let f_formatting = record[4] & 0x20 != 0;
    *pos += 8;
    if f_formatting {
        // The override embeds a full LVL; its own start wins.
        let (lvl_def, next) = parse_lvl(plf, *pos, origin)?;
        if ilvl < LEVELS {
            if f_start_at {
                def.start_override[ilvl] = Some(lvl_def.start);
            }
            def.levels[ilvl] = lvl_def;
        }
        *pos = next;
    } else if f_start_at && ilvl < LEVELS {
        def.levels[ilvl].start = u64::from(start_at);
        def.start_override[ilvl] = Some(u64::from(start_at));
    }
    Some(())
}

#[derive(Default)]
pub(super) struct Counters {
    /// Numbering state keyed by list identity: every LFO referencing the
    /// same list continues its sequence.
    state: HashMap<ListId, LevelCounters<LEVELS>>,
    /// (ilfo, level) pairs whose first-use start-at override has fired.
    overridden: std::collections::HashSet<(u16, usize)>,
}

impl Counters {
    /// Advance the sequence for a paragraph numbered by (`ilfo`, `level`)
    /// and return its effective number plus, when the level's number text is
    /// not reproducible from the marker kind alone, the composite label.
    pub(super) fn next(
        &mut self,
        ilfo: u16,
        list: &ListDef,
        level: usize,
    ) -> (u64, Option<String>) {
        let level = LevelCounters::<LEVELS>::clamp(level);
        let def = list.level(level);
        let first_use = self.overridden.insert((ilfo, level));
        let counters = self.state.entry(list.id).or_default();
        let restarts = |deeper: usize| match list.level(deeper).restart {
            None => true,
            Some(lim) => u32::try_from(level).is_ok_and(|level| level < lim),
        };
        let value = match list.start_override[level] {
            Some(start) if first_use => {
                counters.seed(level, start, restarts);
                start
            }
            _ => counters.next(level, def.start, restarts),
        };
        let label = def.marker.and_then(|marker| {
            composite_label(
                &def.pattern,
                marker,
                value,
                |l| list.level(l).marker.unwrap_or(MarkerKind::Decimal),
                |l| counters.value_or(l, list.level(l).start),
            )
        });
        (value, label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::doc::tests::words;

    #[test]
    fn an_lfo_count_past_the_records_keeps_no_list() {
        let mut plf = 0xFFFF_FFFFu32.to_le_bytes().to_vec();
        plf.extend_from_slice(&[0u8; 2 * LFO_SIZE]);
        let mut lists = Lists::default();
        assert!(!parse_plf_lfo(&plf, 0, &HashMap::new(), &mut lists));
        assert!(lists.is_empty());
    }

    /// An LVL: an LVLF header of zeros `set_header` edits, then the xst
    /// `units` behind their count.
    fn lvl(units: &[u16], set_header: impl FnOnce(&mut [u8])) -> Vec<u8> {
        let mut lvl = vec![0u8; 28];
        set_header(&mut lvl);
        lvl.extend_from_slice(&u16::try_from(units.len()).unwrap().to_le_bytes());
        lvl.extend(units.iter().flat_map(|u| u.to_le_bytes()));
        lvl
    }

    #[test]
    fn lvl_parses_the_complete_metadata() {
        let lvl = lvl(&[0x0000, '-' as u16, 0x0001, ')' as u16], |header| {
            header[0..4].copy_from_slice(&5u32.to_le_bytes()); // iStartAt
            header[4] = 4; // nfc: lowerLetter
            header[5] = 0x08; // fNoRestart
            header[6] = 1; // rgbxchNums: placeholder offsets
            header[7] = 3;
            header[26] = 1; // ilvlRestartLim
        });
        let (def, next) = parse_lvl(&lvl, 0, 0).unwrap();
        assert_eq!(next, lvl.len());
        assert_eq!(def.damaged_at, None);
        assert_eq!(def.start, 5);
        assert_eq!(def.marker, Some(MarkerKind::LowerAlpha));
        assert_eq!(def.restart, Some(1));
        assert!(!def.pattern.legal);
        assert_eq!(
            def.pattern.text,
            vec![
                NumberText::Level(0),
                NumberText::Literal("-".into()),
                NumberText::Level(1),
                NumberText::Literal(")".into()),
            ]
        );
    }

    #[test]
    fn number_text_keeps_surrogate_pairs_and_marks_an_unpaired_unit() {
        let lvl_at = |placeholder: u8, units: &[u16]| lvl(units, |header| header[6] = placeholder);
        let lvl = |units: &[u16]| lvl_at(0, units);
        let (def, _) = parse_lvl(&lvl(&[0xD835, 0xDC00, 0x2E]), 0, 0).unwrap();
        let literal = |text: &str| vec![NumberText::Literal(text.into())];
        assert_eq!(def.pattern.text, literal("\u{1D400}."));
        assert_eq!(def.damaged_at, None);
        let (def, _) = parse_lvl(&lvl_at(3, &[0xD835, 0xDC00, 0x0000]), 0, 0).unwrap();
        assert_eq!(
            def.pattern.text,
            [NumberText::Literal("\u{1D400}".into()), NumberText::Level(0)]
        );
        assert_eq!(def.damaged_at, None);
        let padded = [vec![0; 3], lvl(&[0xDC00, 0x2E])].concat();
        let (def, _) = parse_lvl(&padded, 3, 100).unwrap();
        assert_eq!(def.pattern.text, literal("\u{FFFD}."));
        assert_eq!(def.damaged_at, Some(103));
    }

    #[test]
    fn a_placeholder_offset_is_matched_in_full_not_modulo_a_byte() {
        let units: Vec<u16> = [vec![0x41; 256], vec![0x01]].concat();
        let (def, _) = parse_lvl(&lvl(&units, |header| header[6] = 1), 0, 0).unwrap();
        assert_eq!(def.pattern.text, [NumberText::Literal("A".repeat(256))]);

        let tab = lvl(&[0x41, 0x09], |header| header[6] = 2);
        let (def, _) = parse_lvl(&tab, 0, 0).unwrap();
        assert_eq!(def.pattern.text, [NumberText::Literal("A".into())]);
    }

    #[test]
    fn placeholder_offsets_end_at_the_first_zero() {
        let spaced = lvl(&[0x41, 0x01, 0x42, 0x01], |header| {
            header[6..9].copy_from_slice(&[2, 0, 4]);
        });
        let (def, _) = parse_lvl(&spaced, 0, 0).unwrap();
        let literal = |text: &str| NumberText::Literal(text.into());
        assert_eq!(def.pattern.text, [literal("A"), NumberText::Level(1), literal("B")]);
    }

    #[test]
    fn a_cut_number_text_fails_an_ordered_level_only() {
        let cut = |nfc: u8| {
            let mut lvl = lvl(&[0x41, 0x42], |header| header[4] = nfc);
            lvl.truncate(lvl.len() - 2);
            lvl
        };
        assert_eq!(parse_lvl(&cut(0), 0, 0).map(|(_, next)| next), None);
        let (def, next) = parse_lvl(&cut(23), 0, 0).unwrap();
        assert_eq!((def.marker, next), (Some(MarkerKind::Bullet), 34));
    }

    #[test]
    fn list_overrides_past_the_last_id_a_paragraph_can_name_are_skipped_without_loss() {
        let table = |count: u32| {
            let headers = (0..count).flat_map(|_| lfo(0, 0)).collect::<Vec<u8>>();
            [words(&[count]), headers].concat()
        };
        let mut lists = Lists::default();
        assert!(parse_plf_lfo(&table(65_535), 0, &HashMap::new(), &mut lists));
        assert!(lists.contains_key(&65_535));
        let mut lists = Lists::default();
        assert!(parse_plf_lfo(&table(65_536), 0, &HashMap::new(), &mut lists));
        assert!(lists.contains_key(&65_535));
        assert!(!lists.contains_key(&0));
    }

    /// A decimal LVL with no grpprls and an empty number text.
    fn empty_lvl() -> Vec<u8> {
        lvl(&[], |_| {})
    }

    /// A decimal LVL starting at `start` whose number text is an unpaired
    /// surrogate.
    fn damaged_lvl(start: u32) -> Vec<u8> {
        lvl(&[0xDC00], |header| header[0..4].copy_from_slice(&start.to_le_bytes()))
    }

    #[test]
    fn a_damaged_list_level_is_marked_at_its_table_offset() {
        let mut lstf = vec![0u8; LSTF_SIZE];
        lstf[26] = 0x01;
        let plf = [&[1u8, 0][..], &lstf].concat();
        let table = [vec![0; 5], plf.clone(), damaged_lvl(1)].concat();
        let (by_lsid, complete) = parse_plf_lst(&table, 5, plf.len());
        assert!(complete);
        let marks: Vec<_> = by_lsid[&0].iter().map(|level| level.damaged_at).collect();
        assert_eq!(marks, [Some(5 + plf.len()); LEVELS]);
    }

    #[test]
    fn a_list_table_cut_short_does_not_read_in_full() {
        let complete = |table: &[u8], lcb: usize| parse_plf_lst(table, 0, lcb).1;
        let mut lstf = vec![0u8; LSTF_SIZE];
        lstf[26] = 0x01;
        let plf = [&[1u8, 0][..], &lstf].concat();
        assert!(complete(&[plf.clone(), empty_lvl()].concat(), plf.len()));
        for (case, table, lcb) in [
            ("a count cut short", vec![1u8], 1),
            ("an LSTF array cut short", plf[..12].to_vec(), 12),
            ("an LVL cut short", [plf.clone(), empty_lvl()[..10].to_vec()].concat(), plf.len()),
        ] {
            assert!(!complete(&table, lcb), "{case}");
        }
    }

    #[test]
    fn an_lstf_array_cut_short_keeps_no_list_whose_levels_read() {
        let mut lstf = vec![0u8; LSTF_SIZE];
        lstf[26] = 0x01;
        let plf = [&[3u8, 0][..], &lstf].concat();
        let table = [plf.clone(), empty_lvl()].concat();
        let (by_lsid, complete) = parse_plf_lst(&table, 0, plf.len());
        assert!(by_lsid.is_empty());
        assert!(!complete);
    }

    #[test]
    fn an_lfolvl_applies_its_overrides_and_moves_the_cursor_past_what_it_read() {
        let start_at = |bits: u8| [words(&[7]), vec![bits, 0, 0, 0]].concat();
        let with = |bits: u8, lvl: Vec<u8>| [start_at(bits), lvl].concat();
        for (case, plf, read, cursor, ilvl, start, overridden) in [
            ("an LFOLVL cut short", start_at(0x10)[..7].to_vec(), false, 0, 0, 1, None),
            ("a start-at alone", start_at(0x10), true, 8, 0, 7, Some(7)),
            ("a level past the last", start_at(0x1F), true, 8, 0, 1, None),
            ("the first level past the last", start_at(0x19), true, 8, 0, 1, None),
            ("the last level", start_at(0x18), true, 8, 0, 1, None),
            ("a start-at at the last level", start_at(0x18), true, 8, 8, 7, Some(7)),
            ("an embedded LVL", with(0x20, empty_lvl()), true, 38, 0, 0, None),
            ("an embedded LVL past the last", with(0x2F, empty_lvl()), true, 38, 0, 1, None),
            ("an embedded LVL one past the last", with(0x29, empty_lvl()), true, 38, 0, 1, None),
            ("an embedded LVL and start", with(0x30, damaged_lvl(9)), true, 40, 0, 9, Some(9)),
            (
                "an embedded LVL at the last level",
                with(0x38, damaged_lvl(9)),
                true,
                40,
                8,
                9,
                Some(9),
            ),
            ("a cut embedded LVL", with(0x20, empty_lvl()[..10].to_vec()), false, 8, 0, 1, None),
        ] {
            let mut def = ListDef::unknown(1);
            let mut at = 0;
            assert_eq!(read_lfolvl(&plf, &mut at, 0, &mut def).is_some(), read, "{case}");
            assert_eq!(
                (at, def.levels[ilvl].start, def.start_override[ilvl]),
                (cursor, start, overridden),
                "{case}"
            );
        }
    }

    #[test]
    fn a_list_override_table_cut_short_does_not_read_in_full() {
        let complete = |plf: &[u8]| parse_plf_lfo(plf, 0, &HashMap::new(), &mut Lists::default());
        let formatting = [words(&[1]), lfo(0, 1), words(&[0]), lfolvl(0x20)].concat();
        assert!(complete(&[words(&[1]), lfo(0, 0)].concat()));
        assert!(complete(&[formatting.clone(), empty_lvl()].concat()));
        for (case, plf) in [
            ("a count cut short", vec![1u8, 0]),
            ("an LFO array cut short", [words(&[2]), lfo(0, 0)].concat()),
            ("an early override cut short", [words(&[2]), lfo(0, 1), lfo(0, 0)].concat()),
            ("an override with no cp", [words(&[1]), lfo(0, 1), lfolvl(0)].concat()),
            ("an override's LVL cut short", [formatting, empty_lvl()[..10].to_vec()].concat()),
        ] {
            assert!(!complete(&plf), "{case}");
        }
    }

    fn lfo(lsid: u32, clfolvl: u8) -> Vec<u8> {
        let mut lfo = vec![0u8; LFO_SIZE];
        lfo[..4].copy_from_slice(&lsid.to_le_bytes());
        lfo[12] = clfolvl;
        lfo
    }

    /// An LFOLVL for level 0 with the `fStartAt`/`fFormatting` bits `flags`.
    fn lfolvl(flags: u8) -> Vec<u8> {
        [words(&[0]), vec![flags, 0, 0, 0]].concat()
    }

    #[test]
    fn every_lfo_data_opens_with_its_cp() {
        let mut levels: [LevelDef; LEVELS] = std::array::from_fn(|_| LevelDef::default());
        levels[0].marker = Some(MarkerKind::Decimal);
        let by_lsid = HashMap::from([(9, levels)]);
        let head = [words(&[2]), lfo(9, 0), lfo(9, 1)].concat();
        let data = [words(&[0x0A0B_0C0D, 0x1A1B_1C1D]), lfolvl(0x30)].concat();
        let overriding = [head, data].concat();
        let plf = [overriding.clone(), damaged_lvl(7)].concat();
        let mut lists = Lists::default();
        assert!(parse_plf_lfo(&plf, 1000, &by_lsid, &mut lists));
        let first = &lists[&1];
        assert_eq!((first.levels[0].start, first.start_override[0]), (1, None));
        let second = &lists[&2];
        assert_eq!(second.id, ListId::Lsid(9));
        assert_eq!(second.levels[0].marker, Some(MarkerKind::Decimal));
        assert_eq!((second.levels[0].start, second.start_override[0]), (7, Some(7)));
        assert_eq!(second.levels[0].damaged_at, Some(1000 + overriding.len()));
        assert_eq!(second.levels[1].damaged_at, None);
    }

    #[test]
    fn default_restart_when_f_no_restart_is_clear() {
        let lvl = lvl(&[], |header| {
            header[0..4].copy_from_slice(&1u32.to_le_bytes());
            header[26] = 3; // ilvlRestartLim is ignored without fNoRestart
        });
        let (def, _) = parse_lvl(&lvl, 0, 0).unwrap();
        assert_eq!(def.restart, None);
    }

    fn list(lsid: u32) -> ListDef {
        let mut levels: [LevelDef; LEVELS] = std::array::from_fn(|_| LevelDef::default());
        levels[0].marker = Some(MarkerKind::Decimal);
        levels[1].marker = Some(MarkerKind::LowerAlpha);
        ListDef { id: ListId::Lsid(lsid), levels, start_override: [None; LEVELS] }
    }

    #[test]
    fn a_level_past_the_last_is_the_last_level() {
        let mut l = list(7);
        l.levels[LEVELS - 1].start = 9;
        assert_eq!(l.level(1).marker, Some(MarkerKind::LowerAlpha));
        let starts = [LEVELS - 1, LEVELS, usize::MAX].map(|ilvl| l.level(ilvl).start);
        assert_eq!(starts, [9, 9, 9]);
    }

    #[test]
    fn shared_lsid_continues_across_ilfos() {
        let mut c = Counters::default();
        let l = list(7);
        assert_eq!(c.next(1, &l, 0).0, 1);
        assert_eq!(c.next(2, &l, 0).0, 2);
    }

    #[test]
    fn first_use_override_restarts_the_shared_sequence() {
        let mut c = Counters::default();
        let l = list(7);
        let mut over = list(7);
        over.start_override[0] = Some(10);
        assert_eq!(c.next(1, &l, 0).0, 1);
        assert_eq!(c.next(1, &l, 0).0, 2);
        assert_eq!(c.next(2, &over, 0).0, 10); // fires on first use only
        assert_eq!(c.next(2, &over, 0).0, 11);
        assert_eq!(c.next(1, &l, 0).0, 12); // same lsid: sequence is shared
    }

    #[test]
    fn no_restart_level_survives_shallower_items() {
        let mut c = Counters::default();
        let mut l = list(7);
        l.levels[1].restart = Some(0); // fNoRestart, never restart
        assert_eq!(c.next(1, &l, 0).0, 1);
        assert_eq!(c.next(1, &l, 1).0, 1);
        assert_eq!(c.next(1, &l, 0).0, 2);
        assert_eq!(c.next(1, &l, 1).0, 2); // continues past the level-0 item
        let restarting = list(8);
        assert_eq!(c.next(3, &restarting, 0).0, 1);
        assert_eq!(c.next(3, &restarting, 1).0, 1);
        assert_eq!(c.next(3, &restarting, 0).0, 2);
        assert_eq!(c.next(3, &restarting, 1).0, 1); // default rule restarts
    }

    #[test]
    fn restart_limit_distinguishes_trigger_levels() {
        let mut c = Counters::default();
        let mut l = list(7);
        l.levels[2].marker = Some(MarkerKind::Decimal);
        l.levels[2].restart = Some(2); // restart only after levels 0 or 1
        assert_eq!(c.next(1, &l, 2).0, 1);
        c.next(1, &l, 1); // level 1 < 2: triggers a restart
        assert_eq!(c.next(1, &l, 2).0, 1);
        assert_eq!(c.next(1, &l, 2).0, 2);
    }

    #[test]
    fn composite_label_renders_parent_numbers() {
        let mut c = Counters::default();
        let mut l = list(7);
        l.levels[1].pattern.text = vec![
            NumberText::Level(0),
            NumberText::Literal("-".into()),
            NumberText::Level(1),
            NumberText::Literal(")".into()),
        ];
        c.next(1, &l, 0);
        let (n, label) = c.next(1, &l, 1);
        assert_eq!(n, 1);
        assert_eq!(label.as_deref(), Some("1-a)"));
    }

    #[test]
    fn default_number_text_produces_no_label() {
        let mut c = Counters::default();
        let mut l = list(7);
        l.levels[0].pattern.text = vec![NumberText::Level(0), NumberText::Literal(".".into())];
        assert_eq!(c.next(1, &l, 0), (1, None));
    }

    #[test]
    fn an_unknown_lfo_numbers_apart_from_the_lsid_its_stand_in_once_took() {
        let mut c = Counters::default();
        let known = list(u32::MAX ^ 2);
        let unknown = ListDef::unknown(2);
        for expected in [1, 2] {
            assert_eq!(c.next(1, &known, 0).0, expected);
            assert_eq!(c.next(2, &unknown, 0).0, expected);
        }
    }
}
