//! Word binary list tables: `PlfLst` (list definitions with per-level
//! formats) and `PlfLfo` (list format overrides referenced by `sprmPIlfo`),
//! resolved per [MS-DOC]: each list keeps its identity (`lsid`) so numbering
//! state is shared by every LFO referencing the same list, levels keep their
//! restart limits (`fNoRestart`/`ilvlRestartLim`), LFOLVL start-at overrides
//! apply when their LFO is first used, and the number text (`xst` with
//! `rgbxchNums` placeholders) is preserved for composite markers.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::shared::binary::{get_u16, get_u32};
use crate::shared::list::MarkerKind;
use crate::shared::numbering::{NumberPattern, NumberText};
use std::collections::HashMap;

pub const LEVELS: usize = 9;

#[derive(Debug, Clone)]
pub struct LevelDef {
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
        LevelDef {
            marker: Some(MarkerKind::Bullet),
            start: 1,
            restart: None,
            pattern: NumberPattern::default(),
            damaged_at: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ListDef {
    /// List identity: numbering state is keyed by this, not the ilfo, so
    /// every override referencing the same list continues its sequence.
    pub lsid: u32,
    pub levels: [LevelDef; LEVELS],
    /// LFOLVL start-at overrides; each restarts the shared sequence at its
    /// value when this LFO is first used at that level.
    pub start_override: [Option<u64>; LEVELS],
}

impl ListDef {
    /// Fallback definition for an ilfo with no parsed list behind it; the
    /// synthetic identity keeps it distinct from every real `lsid`.
    pub fn unknown(ilfo: u16) -> ListDef {
        ListDef {
            lsid: u32::MAX ^ u32::from(ilfo),
            levels: std::array::from_fn(|_| LevelDef::default()),
            start_override: [None; LEVELS],
        }
    }
}

#[derive(Debug, Default)]
pub struct Lists {
    /// 1-based ilfo -> resolved definition.
    by_ilfo: HashMap<u16, ListDef>,
}

impl Lists {
    pub fn get(&self, ilfo: u16) -> Option<&ListDef> {
        self.by_ilfo.get(&ilfo)
    }
}

/// FIB offsets (Word 97+): fcPlfLst/lcbPlfLst at 0x2E2, fcPlfLfo at 0x2EA.
/// List tables that do not read in full are `part_unreadable`: the labels
/// of the lists they lose do not render. A level's undecodable number text
/// is reported where a paragraph shows it, not here.
pub fn parse(parse_ctx: &ParseCtx, word_doc: &[u8], table: &[u8]) -> Lists {
    let lst_fc = get_u32(word_doc, 0x2E2).unwrap_or(0) as usize;
    let lst_lcb = get_u32(word_doc, 0x2E6).unwrap_or(0) as usize;
    let lfo_fc = get_u32(word_doc, 0x2EA).unwrap_or(0) as usize;
    let lfo_lcb = get_u32(word_doc, 0x2EE).unwrap_or(0) as usize;

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
    let Some(count) = get_u16(plf, 0).map(|v| v as usize) else {
        return (out, false);
    };
    let mut lstfs: Vec<(u32, bool)> = Vec::with_capacity(count);
    let mut pos = 2usize;
    for _ in 0..count {
        let Some(record) = plf.get(pos..).and_then(|rest| rest.get(..LSTF_SIZE)) else {
            return (out, false);
        };
        let Some(lsid) = get_u32(record, 0) else {
            return (out, false);
        };
        let simple = record[26] & 0x01 != 0;
        lstfs.push((lsid, simple));
        pos = match pos.checked_add(LSTF_SIZE) {
            Some(next) => next,
            None => return (out, false),
        };
    }
    // The LVL array begins where lcbPlfLst ends, still relative to `plf`.
    let mut pos = lcb;
    for (lsid, simple) in lstfs {
        let n_levels = if simple { 1 } else { LEVELS };
        let mut levels: [LevelDef; LEVELS] = std::array::from_fn(|_| LevelDef::default());
        for slot in levels.iter_mut().take(n_levels) {
            match parse_lvl(plf, pos, fc) {
                Some((def, next)) => {
                    *slot = def;
                    pos = next;
                }
                None => return (out, false),
            }
        }
        if simple {
            // A simple list applies its single level everywhere.
            let def = levels[0].clone();
            levels = std::array::from_fn(|_| def.clone());
        }
        out.insert(lsid, levels);
    }
    (out, true)
}

/// One LVL at `pos` of `plf`, which starts at table-stream offset `origin`:
/// an LVLF header, then PAPX and CHPX grpprls, then the number text, whose
/// unpaired surrogates mark the level damaged.
fn parse_lvl(plf: &[u8], pos: usize, origin: usize) -> Option<(LevelDef, usize)> {
    const LVLF_SIZE: usize = 28;
    let record = plf.get(pos..)?;
    let header = record.get(..LVLF_SIZE)?;
    let start = get_u32(header, 0)? as u64;
    let nfc = header[4];
    let flags = header[5];
    let legal = flags & 0x04 != 0;
    let no_restart = flags & 0x08 != 0;
    // rgbxchNums: one-based offsets of number placeholders within xst,
    // zero-terminated unless full.
    let placeholder_offsets: Vec<u8> =
        header[6..15].iter().copied().take_while(|&b| b != 0).collect();
    let restart_lim = header[26];
    let cb_papx = header[25] as usize;
    let cb_chpx = header[24] as usize;
    let mut next = LVLF_SIZE.checked_add(cb_papx)?.checked_add(cb_chpx)?;
    let xch_count = get_u16(record, next)? as usize;
    next = next.checked_add(2)?;
    let marker = marker_for_nfc(nfc);
    let mut text: Vec<NumberText> = Vec::new();
    let mut replaced = false;
    if marker.is_some_and(MarkerKind::ordered) {
        let units = (0..xch_count)
            .map(|i| get_u16(record, i.checked_mul(2)?.checked_add(next)?))
            .collect::<Option<Vec<u16>>>()?;
        let mut i = 0usize;
        for unit in char::decode_utf16(units) {
            let c = unit.unwrap_or_else(|_| {
                replaced = true;
                char::REPLACEMENT_CHARACTER
            });
            i += c.len_utf16();
            if u32::from(c) <= 0x08 && placeholder_offsets.contains(&(i as u8)) {
                text.push(NumberText::Level(u32::from(c) as u8));
            } else if !c.is_control() {
                match text.last_mut() {
                    Some(NumberText::Literal(s)) => s.push(c),
                    _ => text.push(NumberText::Literal(c.to_string())),
                }
            }
        }
    }
    next = next.checked_add(xch_count.checked_mul(2)?)?;
    let restart = no_restart.then_some(restart_lim as u32);
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
    let mut lfos: Vec<(u32, usize)> = Vec::with_capacity(count);
    let mut pos = 4;
    for _ in 0..count {
        let Some(record) = plf.get(pos..).and_then(|rest| rest.get(..LFO_SIZE)) else {
            return false;
        };
        let Some(lsid) = get_u32(record, 0) else {
            return false;
        };
        let clfolvl = record[12] as usize;
        lfos.push((lsid, clfolvl));
        let Some(next) = pos.checked_add(LFO_SIZE) else {
            return false;
        };
        pos = next;
    }
    let mut complete = true;
    for (i, (lsid, clfolvl)) in lfos.into_iter().enumerate() {
        let ilfo = (i + 1) as u16;
        let mut def = match by_lsid.get(&lsid) {
            Some(levels) => {
                ListDef { lsid, levels: levels.clone(), start_override: [None; LEVELS] }
            }
            None => ListDef::unknown(ilfo),
        };
        pos = pos.saturating_add(4);
        for _ in 0..clfolvl {
            let Some(override_record) = plf.get(pos..).and_then(|rest| rest.get(..8)) else {
                complete = false;
                break;
            };
            let Some(start_at) = get_u32(override_record, 0) else {
                complete = false;
                break;
            };
            let bits = override_record[4];
            let ilvl = (bits & 0x0F) as usize;
            let f_start_at = bits & 0x10 != 0;
            let f_formatting = bits & 0x20 != 0;
            let Some(next) = pos.checked_add(8) else {
                complete = false;
                break;
            };
            pos = next;
            if f_formatting {
                // The override embeds a full LVL; its own start wins.
                match parse_lvl(plf, pos, origin) {
                    Some((lvl_def, next)) => {
                        if ilvl < LEVELS {
                            if f_start_at {
                                def.start_override[ilvl] = Some(lvl_def.start);
                            }
                            def.levels[ilvl] = lvl_def;
                        }
                        pos = next;
                    }
                    None => {
                        complete = false;
                        break;
                    }
                }
            } else if f_start_at && ilvl < LEVELS {
                def.levels[ilvl].start = start_at as u64;
                def.start_override[ilvl] = Some(start_at as u64);
            }
        }
        lists.by_ilfo.insert(ilfo, def);
    }
    complete
}

/// MS-OSHARED numbering format codes.
fn marker_for_nfc(nfc: u8) -> Option<MarkerKind> {
    match nfc {
        0 => Some(MarkerKind::Decimal),
        1 => Some(MarkerKind::UpperRoman),
        2 => Some(MarkerKind::LowerRoman),
        3 => Some(MarkerKind::UpperAlpha),
        4 => Some(MarkerKind::LowerAlpha),
        23 => Some(MarkerKind::Bullet),
        0xFF => None,
        _ => Some(MarkerKind::Decimal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built LVL: LVLF header + xst "\x00-\x01)" with placeholders
    /// at one-based offsets 1 and 3.
    #[test]
    fn lvl_parses_the_complete_metadata() {
        let mut lvl = vec![0u8; 28];
        lvl[0..4].copy_from_slice(&5u32.to_le_bytes()); // iStartAt
        lvl[4] = 4; // nfc: lowerLetter
        lvl[5] = 0x08; // fNoRestart
        lvl[6] = 1; // rgbxchNums: placeholder offsets
        lvl[7] = 3;
        lvl[26] = 1; // ilvlRestartLim
        lvl.extend_from_slice(&4u16.to_le_bytes()); // xst length
        for ch in [0x0000u16, '-' as u16, 0x0001, ')' as u16] {
            lvl.extend_from_slice(&ch.to_le_bytes());
        }
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
        let lvl_at = |placeholder: u8, units: &[u16]| {
            let mut lvl = vec![0u8; 28];
            lvl[6] = placeholder;
            lvl.extend_from_slice(&u16::try_from(units.len()).unwrap().to_le_bytes());
            lvl.extend(units.iter().flat_map(|u| u.to_le_bytes()));
            lvl
        };
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

    /// A decimal LVL with no grpprls and an empty number text.
    fn empty_lvl() -> Vec<u8> {
        vec![0u8; 30]
    }

    /// A decimal LVL starting at `start` whose number text is an unpaired
    /// surrogate.
    fn damaged_lvl(start: u32) -> Vec<u8> {
        let mut lvl = vec![0u8; 28];
        lvl[0..4].copy_from_slice(&start.to_le_bytes());
        [lvl, vec![1, 0, 0x00, 0xDC]].concat()
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
    fn a_list_override_table_cut_short_does_not_read_in_full() {
        let complete = |plf: &[u8]| parse_plf_lfo(plf, 0, &HashMap::new(), &mut Lists::default());
        let formatting = [words(&[1]), lfo(0, 1), words(&[0]), lfolvl(0x20)].concat();
        assert!(complete(&[words(&[1]), lfo(0, 0)].concat()));
        assert!(complete(&[formatting.clone(), empty_lvl()].concat()));
        for (case, plf) in [
            ("a count cut short", vec![1u8, 0]),
            ("an LFO array cut short", [words(&[2]), lfo(0, 0)].concat()),
            ("an override with no cp", [words(&[1]), lfo(0, 1), lfolvl(0)].concat()),
            ("an override's LVL cut short", [formatting, empty_lvl()[..10].to_vec()].concat()),
        ] {
            assert!(!complete(&plf), "{case}");
        }
    }

    fn words(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
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
        let first = lists.get(1).unwrap();
        assert_eq!((first.levels[0].start, first.start_override[0]), (1, None));
        let second = lists.get(2).unwrap();
        assert_eq!(second.lsid, 9);
        assert_eq!(second.levels[0].marker, Some(MarkerKind::Decimal));
        assert_eq!((second.levels[0].start, second.start_override[0]), (7, Some(7)));
        assert_eq!(second.levels[0].damaged_at, Some(1000 + overriding.len()));
        assert_eq!(second.levels[1].damaged_at, None);
    }

    #[test]
    fn default_restart_when_f_no_restart_is_clear() {
        let mut lvl = vec![0u8; 28];
        lvl[0..4].copy_from_slice(&1u32.to_le_bytes());
        lvl[4] = 0; // decimal
        lvl[26] = 3; // ilvlRestartLim is ignored without fNoRestart
        lvl.extend_from_slice(&0u16.to_le_bytes());
        let (def, _) = parse_lvl(&lvl, 0, 0).unwrap();
        assert_eq!(def.restart, None);
    }
}
