//! The piece table: where each character position lies in the file, and the property
//! modifier its piece carries, read in place from the checked Clx.

use super::Plc;
use super::sprm::{
    SPRM_C_ISS, SPRM_CF_BOLD, SPRM_CF_ITALIC, SPRM_CF_RMARK_DEL, SPRM_CF_RMARK_INS, SPRM_CF_STRIKE,
    SPRM_CF_VANISH, SPRM_P_ILVL, SPRM_P_OUT_LVL, SPRM_PF_IN_TABLE, SPRM_PF_TTP,
};
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::shared::binary::{get_u16, get_u32};
use std::ops::Range;

/// The `Prc`s a 15-bit `Prm1.igrpprl` can name.
pub(super) const MAX_PRCS: usize = 1 << 15;

/// What a piece table must cover and where its text may lie.
#[derive(Clone, Copy)]
pub(super) struct Coverage {
    /// The last CP the stories declare, which `PlcPcd`'s last CP must equal.
    pub(super) terminal_cp: usize,
    /// `FibRgLw97.cbMac`: the WordDocument bytes that have meaning.
    pub(super) cb_mac: usize,
}

/// A checked `PlcPcd` and the `Prc`s before it, borrowed from the table stream.
pub(super) struct PieceTable<'t> {
    clx: &'t [u8],
    /// The offset in `clx` of each `Prc`, up to [`MAX_PRCS`].
    prcs: Vec<u32>,
    plc: Plc<'t>,
}

/// One piece: its CPs, the file offset of its first character and its width.
#[derive(Clone, Copy)]
pub(super) struct Pcd {
    pub(super) cps: (usize, usize),
    pub(super) fc: usize,
    pub(super) compressed: bool,
    prm: u16,
}

impl Pcd {
    pub(super) fn cp_range(self) -> Range<usize> {
        self.cps.0..self.cps.1
    }

    /// The bytes one character position of the piece takes.
    pub(super) fn width(self) -> usize {
        if self.compressed { 1 } else { 2 }
    }

    /// The bytes of the piece's text in the stream, `None` when their end overflows.
    pub(super) fn bytes(self) -> Option<Range<usize>> {
        let len = (self.cps.1 - self.cps.0).checked_mul(self.width())?;
        Some(self.fc..self.fc.checked_add(len)?)
    }
}

/// A piece's property modifier: the grpprl of the `Prc` a `Prm1` names, or the one sprm a
/// `Prm0` carries.
pub(super) enum Prm<'t> {
    None,
    Prc(&'t [u8]),
    Single([u8; 3]),
}

impl Prm<'_> {
    pub(super) fn grpprl(&self) -> Option<&[u8]> {
        match self {
            Self::None => None,
            Self::Prc(grpprl) => Some(grpprl),
            Self::Single(sprm) => Some(sprm),
        }
    }
}

impl<'t> PieceTable<'t> {
    /// The Clx at `fc`/`lcb` of `table`, its `PlcPcd` checked against `coverage`: exactly one
    /// first CP of 0, ascending CPs ending at the terminal CP, and text inside `cbMac`. Each
    /// piece is charged; any other shape is `Malformed`.
    pub(super) fn parse(
        parse_ctx: &ParseCtx<'_>,
        table: &'t [u8],
        fc: usize,
        lcb: usize,
        coverage: Coverage,
    ) -> Result<Self, ConvertError> {
        let clx = table
            .get(fc..fc.saturating_add(lcb))
            .ok_or_else(|| ConvertError::malformed("Clx out of bounds"))?;
        let mut prcs = Vec::new();
        let mut pos = 0;
        loop {
            let rest = clx.get(pos..).ok_or_else(|| ConvertError::malformed("malformed Clx"))?;
            match rest.first() {
                Some(1) => {
                    let cb = get_u16(rest, 1).ok_or_else(|| ConvertError::malformed("bad Prc"))?
                        as usize;
                    if rest.len() < 3 + cb {
                        return Err(ConvertError::malformed("malformed Clx"));
                    }
                    parse_ctx.charge(1)?;
                    if prcs.len() < MAX_PRCS {
                        prcs.push(
                            u32::try_from(pos).map_err(|_| ConvertError::malformed("bad Prc"))?,
                        );
                    }
                    pos += 3 + cb;
                }
                Some(2) => {
                    let lcb_plc = get_u32(rest, 1)
                        .ok_or_else(|| ConvertError::malformed("bad Pcdt"))?
                        as usize;
                    let plc = rest
                        .get(5..lcb_plc.saturating_add(5))
                        .ok_or_else(|| ConvertError::malformed("PlcPcd out of bounds"))?;
                    return Self::checked(parse_ctx, clx, prcs, plc, coverage);
                }
                _ => return Err(ConvertError::malformed("malformed Clx")),
            }
        }
    }

    fn checked(
        parse_ctx: &ParseCtx<'_>,
        clx: &'t [u8],
        prcs: Vec<u32>,
        plc: &'t [u8],
        coverage: Coverage,
    ) -> Result<Self, ConvertError> {
        let Some(plc) = Plc::new(plc, 8) else {
            return Err(ConvertError::malformed("piece table of no whole piece"));
        };
        if plc.cp(0) != 0 || plc.cp(plc.n) != coverage.terminal_cp {
            return Err(ConvertError::malformed("piece table misses the stories"));
        }
        let pieces = Self { clx, prcs, plc };
        for i in 0..pieces.len() {
            parse_ctx.charge(1)?;
            let pcd = pieces.pcd(i);
            if pcd.cp_range().is_empty() {
                return Err(ConvertError::malformed("piece table CPs out of order"));
            }
            if pcd.bytes().is_none_or(|bytes| bytes.end > coverage.cb_mac) {
                return Err(ConvertError::malformed("piece past cbMac"));
            }
            if pcd.prm & 1 == 0 && pcd.prm != 0 && prm0_grpprl(pcd.prm).is_none() {
                parse_ctx.emit(DiagnosticCode::FormattingUnreadable, Region::Body);
            }
        }
        Ok(pieces)
    }

    pub(super) fn len(&self) -> usize {
        self.plc.n
    }

    /// Piece `i`, which is below [`Self::len`].
    pub(super) fn pcd(&self, i: usize) -> Pcd {
        let data = self.plc.data(i);
        let fc_raw = get_u32(data, 2).unwrap_or_default();
        let compressed = fc_raw & 0x4000_0000 != 0;
        let fc = (fc_raw & 0x3FFF_FFFF) as usize;
        Pcd {
            cps: (self.plc.cp(i), self.plc.cp(i + 1)),
            fc: if compressed { fc / 2 } else { fc },
            compressed,
            prm: get_u16(data, 6).unwrap_or_default(),
        }
    }

    /// The piece that holds `cp`, which is below the terminal CP.
    pub(super) fn find(&self, cp: usize) -> usize {
        let (mut lo, mut hi) = (0, self.plc.n);
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if self.plc.cp(mid) <= cp {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    pub(super) fn prm(&self, i: usize) -> Prm<'t> {
        let prm = self.pcd(i).prm;
        if prm & 1 != 0 {
            let grpprl = self.prcs.get(usize::from(prm >> 1)).and_then(|&at| {
                let rest = self.clx.get(at as usize..)?;
                rest.get(3..)?.get(..usize::from(get_u16(rest, 1)?))
            });
            grpprl.map_or(Prm::None, Prm::Prc)
        } else {
            prm0_grpprl(prm).map_or(Prm::None, Prm::Single)
        }
    }
}

/// The one-sprm grpprl of a `Prm0` ([MS-DOC] 2.9.215), for the sprms the converter models.
fn prm0_grpprl(prm: u16) -> Option<[u8; 3]> {
    let isprm = (prm >> 1) & 0x7F;
    let [_, val] = prm.to_le_bytes();
    let sprm: u16 = match isprm {
        0x0C => SPRM_P_ILVL,
        0x18 => SPRM_PF_IN_TABLE,
        0x19 => SPRM_PF_TTP,
        0x41 => SPRM_CF_RMARK_DEL,
        0x42 => SPRM_CF_RMARK_INS,
        0x55 => SPRM_CF_BOLD,
        0x56 => SPRM_CF_ITALIC,
        0x57 => SPRM_CF_STRIKE,
        0x5C => SPRM_CF_VANISH,
        0x68 => SPRM_C_ISS,
        0x78 => SPRM_P_OUT_LVL,
        _ => return None,
    };
    let [lo, hi] = sprm.to_le_bytes();
    Some([lo, hi, val])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::doc::tests::clx;
    use crate::ledger::CancelToken;
    use crate::limits::{Limit, Limits};
    use crate::shared::test_support::cancelled;

    #[test]
    fn prm0_decodes_modeled_sprms() {
        // isprm 0x55 = sprmCFBold (0x0835), val 0x01, fComplex clear.
        let prm = (0x55u16 << 1) | (0x01 << 8);
        assert_eq!(prm0_grpprl(prm), Some([0x35, 0x08, 0x01]));
        // isprm 0x18 = sprmPFInTable (0x2416).
        let prm = (0x18u16 << 1) | (0x01 << 8);
        assert_eq!(prm0_grpprl(prm), Some([0x16, 0x24, 0x01]));
        for (isprm, sprm) in [(0x41u16, [0x00, 0x08]), (0x42, [0x01, 0x08]), (0x5C, [0x3C, 0x08])] {
            let prm = (isprm << 1) | (0x81 << 8);
            assert_eq!(prm0_grpprl(prm), Some([sprm[0], sprm[1], 0x81]), "isprm {isprm:#x}");
        }
        // isprm 0x05 (sprmPJc) is outside the converted model.
        assert_eq!(prm0_grpprl(0x05 << 1), None);
    }

    /// `clx` whose `Pcdt` names one byte more of `PlcPcd` than it holds.
    fn overstated(mut clx: Vec<u8>) -> Vec<u8> {
        let lcb = u32::from_le_bytes(clx[1..5].try_into().unwrap()) + 1;
        clx[1..5].copy_from_slice(&lcb.to_le_bytes());
        clx
    }

    /// `clx` with one byte more in its `PlcPcd`.
    fn stray(clx: Vec<u8>) -> Vec<u8> {
        let mut clx = overstated(clx);
        clx.push(0);
        clx
    }

    const COVERAGE: Coverage = Coverage { terminal_cp: 4, cb_mac: 0x1000 };

    fn parsed(table: &[u8], coverage: Coverage) -> Result<PieceTable<'_>, ConvertError> {
        PieceTable::parse(&ParseCtx::default(), table, 0, table.len(), coverage)
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of piece-table shapes and their verdicts")]
    fn a_piece_table_is_checked_against_the_stories_and_cb_mac() {
        let compressed = |fc: u32| (fc * 2) | 0x4000_0000;
        let malformed = |message: &'static str| Err(ConvertError::malformed(message));
        type Case = (&'static str, Vec<u8>, Coverage, Result<usize, ConvertError>);
        let cases: [Case; 11] = [
            ("two pieces", clx(&[], &[0, 2, 4], &[(0x800, 0), (0x900, 0)]), COVERAGE, Ok(2)),
            (
                "a PlcPcd one byte short",
                overstated(clx(&[], &[0, 4], &[(0x800, 0)])),
                COVERAGE,
                malformed("PlcPcd out of bounds"),
            ),
            (
                "a stray byte",
                stray(clx(&[], &[0, 4], &[(0x800, 0)])),
                COVERAGE,
                malformed("piece table of no whole piece"),
            ),
            (
                "a first CP past 0",
                clx(&[], &[1, 4], &[(0x800, 0)]),
                COVERAGE,
                malformed("piece table misses the stories"),
            ),
            (
                "a last CP short of the stories",
                clx(&[], &[0, 3], &[(0x800, 0)]),
                COVERAGE,
                malformed("piece table misses the stories"),
            ),
            (
                "a last CP past the stories",
                clx(&[], &[0, 5], &[(0x800, 0)]),
                COVERAGE,
                malformed("piece table misses the stories"),
            ),
            (
                "a repeated CP",
                clx(&[], &[0, 2, 2, 4], &[(0x800, 0), (0x900, 0), (0xA00, 0)]),
                COVERAGE,
                malformed("piece table CPs out of order"),
            ),
            (
                "a CP going back",
                clx(&[], &[0, 3, 2, 4], &[(0x800, 0), (0x900, 0), (0xA00, 0)]),
                COVERAGE,
                malformed("piece table CPs out of order"),
            ),
            ("text ending at cbMac", clx(&[], &[0, 4], &[(compressed(0xFFC), 0)]), COVERAGE, Ok(1)),
            (
                "text past cbMac",
                clx(&[], &[0, 4], &[(0xFFA, 0)]),
                COVERAGE,
                malformed("piece past cbMac"),
            ),
            (
                "no piece",
                clx(&[], &[0], &[]),
                Coverage { terminal_cp: 0, ..COVERAGE },
                malformed("piece table of no whole piece"),
            ),
        ];
        for (case, table, coverage, expected) in cases {
            assert_eq!(parsed(&table, coverage).map(|pieces| pieces.len()), expected, "{case}");
        }
        let table = clx(&[], &[0, 4], &[(0x800, 0)]);
        assert_eq!(
            PieceTable::parse(&ParseCtx::default(), &table, 0, table.len() + 1, COVERAGE)
                .map(|pieces| pieces.len()),
            malformed("Clx out of bounds"),
        );
    }

    #[test]
    fn each_piece_is_found_by_any_cp_it_holds() {
        let table = clx(&[], &[0, 1, 5, 6, 9], &[(0, 0), (0, 0), (0, 0), (0, 0)]);
        let pieces = parsed(&table, Coverage { terminal_cp: 9, ..COVERAGE }).unwrap();
        let found: Vec<usize> = (0..9).map(|cp| pieces.find(cp)).collect();
        assert_eq!(found, [0, 1, 1, 1, 1, 2, 3, 3, 3]);
    }

    #[test]
    fn a_complex_modifier_names_a_retained_prc_and_a_compressed_one_carries_its_sprm() {
        let table = clx(
            &[&[0x35, 0x08, 1], &[]],
            &[0, 1, 2, 3, 4],
            &[(0, 1), (0, 3), (0, 5), (0, (0x55 << 1) | (1 << 8))],
        );
        let pieces = parsed(&table, COVERAGE).unwrap();
        let grpprls: Vec<Option<Vec<u8>>> =
            (0..4).map(|i| pieces.prm(i).grpprl().map(<[u8]>::to_vec)).collect();
        assert_eq!(
            grpprls,
            [Some(vec![0x35, 0x08, 1]), Some(vec![]), None, Some(vec![0x35, 0x08, 1])]
        );
    }

    #[test]
    fn prcs_past_what_an_index_names_are_checked_and_charged_but_not_kept() {
        let prcs = vec![&[0x35, 0x08, 1][..]; MAX_PRCS + 1];
        let table = clx(&prcs, &[0, 4], &[(0, (0x7FFF << 1) | 1)]);
        let parse_ctx = ParseCtx::default();
        let pieces = PieceTable::parse(&parse_ctx, &table, 0, table.len(), COVERAGE).unwrap();
        assert_eq!(pieces.prcs.len(), MAX_PRCS);
        assert_eq!(pieces.prm(0).grpprl(), Some(&[0x35, 0x08, 1][..]));
        assert_eq!(parse_ctx.fuel(), MAX_PRCS as u64 + 2);
        let cut = [&table[..MAX_PRCS * 6 + 3], &[1, 9, 0]].concat();
        assert_eq!(parsed(&cut, COVERAGE).err(), Some(ConvertError::malformed("malformed Clx")));
    }

    #[test]
    fn a_prc_is_charged_only_once_its_whole_payload_is_present() {
        let cases: [(&str, &[u8], u64); 2] = [
            ("one payload byte short", &[1, 2, 0, 0xAA], 0),
            ("the payload ends the Clx", &[1, 2, 0, 0xAA, 0xBB], 1),
        ];
        for (case, table, fuel) in cases {
            let parse_ctx = ParseCtx::default();
            let parsed = PieceTable::parse(&parse_ctx, table, 0, table.len(), COVERAGE).err();
            assert_eq!(parsed, Some(ConvertError::malformed("malformed Clx")), "{case}");
            assert_eq!(parse_ctx.fuel(), fuel, "{case}");
        }
    }

    #[test]
    fn each_prc_and_piece_checked_is_charged_and_polls_cancellation() {
        let table = clx(&[&[]], &[0, 2, 4], &[(0, 0), (0, 0)]);
        let fuel = |max_work| {
            let limits = Limits { max_work, ..Limits::DEFAULT };
            let parse_ctx = ParseCtx::new(&limits);
            let parsed = PieceTable::parse(&parse_ctx, &table, 0, table.len(), COVERAGE).map(drop);
            (parsed, parse_ctx.fuel())
        };
        assert_eq!(fuel(3), (Ok(()), 3));
        assert_eq!(fuel(2), (Err(ConvertError::limit(Limit::Work, 2)), 3));
        let token = CancelToken::new();
        let parse_ctx = cancelled(&token);
        let cancelled = PieceTable::parse(&parse_ctx, &table, 0, table.len(), COVERAGE).err();
        assert_eq!(cancelled, Some(ConvertError::Cancelled));
    }

    #[test]
    fn an_unknown_compressed_modifier_is_reported_once_per_piece() {
        let table = clx(&[], &[0, 2, 4], &[(0, 0x05 << 1), (0, 0x05 << 1)]);
        let parse_ctx = ParseCtx::default();
        let pieces = PieceTable::parse(&parse_ctx, &table, 0, table.len(), COVERAGE).unwrap();
        assert!(pieces.prm(0).grpprl().is_none());
        let reported: Vec<_> = parse_ctx.diagnostics().iter().map(|d| (d.code, d.count)).collect();
        assert_eq!(reported, [(DiagnosticCode::FormattingUnreadable, 2)]);
    }
}
