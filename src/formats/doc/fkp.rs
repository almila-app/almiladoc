//! Formatting runs: CHPX and PAPX out of FKP pages.

use super::sprm::{PapDelta, apply_pap_sprms};
use super::{Fib, read_plc};
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::shared::binary::{ByteReader, get_u32};

#[derive(Clone, Copy)]
pub(super) enum FkpKind {
    Chpx,
    Papx,
}

#[derive(Default)]
pub(super) struct RunProps {
    /// Raw CHPX grpprl (application depends on the style base).
    pub(super) chpx: Vec<u8>,
    pub(super) istd: u16,
    pub(super) pap: PapDelta,
}

pub(super) struct Run {
    pub(super) fc_start: u32,
    pub(super) fc_end: u32,
    pub(super) props: RunProps,
}

pub(super) struct Runs {
    runs: Vec<Run>,
}

impl Runs {
    pub(super) fn new(mut runs: Vec<Run>) -> Self {
        runs.sort_by_key(|r| r.fc_start);
        Self { runs }
    }

    pub(super) fn lookup(&self, fc: u32) -> Option<&RunProps> {
        self.span(fc).0
    }

    /// The properties at `fc`, and the first FC past it where they can
    /// change: the end of its run or the start of the next.
    pub(super) fn span(&self, fc: u32) -> (Option<&RunProps>, u32) {
        let idx = self.runs.partition_point(|r| r.fc_start <= fc);
        let next = self.runs.get(idx).map_or(u32::MAX, |r| r.fc_start);
        match idx.checked_sub(1).and_then(|at| self.runs.get(at)) {
            Some(run) if fc < run.fc_end => (Some(&run.props), next.min(run.fc_end)),
            _ => (None, next),
        }
    }
}

/// The runs of the FKP pages the bin table at `fib_off` names, each page read
/// once: a page's runs carry absolute FCs, so a page named again repeats them.
pub(super) fn parse_fkps(
    parse_ctx: &ParseCtx<'_>,
    fib: Fib<'_>,
    word_doc: &[u8],
    table: &[u8],
    fib_off: usize,
    kind: FkpKind,
    data: &[u8],
) -> Result<Vec<Run>, ConvertError> {
    let mut runs = Vec::new();
    let Some(bin_table) = read_plc(fib, table, fib_off, 4) else {
        parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Body);
        return Ok(runs);
    };
    let mut read = vec![false; word_doc.len() / 512];
    for i in 0..bin_table.n {
        let pn = (get_u32(bin_table.data(i), 0).unwrap_or(0) & 0x3F_FFFF) as usize;
        let page = pn
            .checked_mul(512)
            .and_then(|off| word_doc.get(off..))
            .and_then(<[u8]>::first_chunk::<512>);
        match (page, read.get_mut(pn)) {
            (Some(_), Some(true)) => {}
            (Some(page), Some(seen)) => {
                *seen = true;
                parse_fkp_page(parse_ctx, page, kind, data, &mut runs)?;
            }
            _ => parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Body),
        }
    }
    Ok(runs)
}

fn parse_fkp_page(
    parse_ctx: &ParseCtx<'_>,
    page: &[u8; 512],
    kind: FkpKind,
    data: &[u8],
    runs: &mut Vec<Run>,
) -> Result<(), ConvertError> {
    let count = usize::from(page[511]);
    let entry_size = match kind {
        FkpKind::Chpx => 1,
        FkpKind::Papx => 13,
    };
    for k in 0..count {
        parse_ctx.charge(1)?;
        let b_offset_pos = (count + 1) * 4 + k * entry_size;
        let (Some(fc_start), Some(fc_end), Some(&b_offset)) =
            (get_u32(page, k * 4), get_u32(page, (k + 1) * 4), page.get(b_offset_pos))
        else {
            continue;
        };
        let mut props = RunProps::default();
        if b_offset != 0 {
            let off = usize::from(b_offset) * 2;
            match kind {
                FkpKind::Chpx => {
                    if let Some(&cb) = page.get(off)
                        && let Some(grpprl) = page.get(off + 1..off + 1 + usize::from(cb))
                    {
                        props.chpx = grpprl.to_vec();
                    }
                }
                FkpKind::Papx => {
                    if let Some(&cb) = page.get(off) {
                        let (start, len) = if cb == 0 {
                            let cb2 = usize::from(page.get(off + 1).copied().unwrap_or(0));
                            (off + 2, cb2 * 2)
                        } else {
                            (off + 1, usize::from(cb) * 2 - 1)
                        };
                        if let Some(mut grpprl) = page.get(start..start + len).map(ByteReader::new)
                            && let Some(istd) = grpprl.u16()
                        {
                            props.istd = istd;
                            apply_pap_sprms(parse_ctx, grpprl.rest(), data, &mut props.pap)?;
                        }
                    }
                }
            }
        }
        runs.push(Run { fc_start, fc_end, props });
    }
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    reason = "the tests build records from known small sizes"
)]
mod tests {
    use super::*;

    /// The runs and fuel of a CHPX bin table naming `pns`, over two FKP pages
    /// of three runs each.
    fn fkp_runs(pns: &[u32]) -> (usize, u64) {
        let mut word_doc = vec![0u8; 1536];
        word_doc[..2].copy_from_slice(&0xA5ECu16.to_le_bytes());
        for (page, start) in [(1, 0u32), (2, 30)] {
            for k in 0..4u32 {
                let at = page * 512 + k as usize * 4;
                word_doc[at..at + 4].copy_from_slice(&(start + k * 10).to_le_bytes());
            }
            word_doc[page * 512 + 511] = 3;
        }
        let cps = (0..=pns.len() as u32).map(|i| i * 30);
        let table: Vec<u8> = cps.chain(pns.iter().copied()).flat_map(u32::to_le_bytes).collect();
        word_doc[0xFE..0x102].copy_from_slice(&(table.len() as u32).to_le_bytes());
        let parse_ctx = ParseCtx::default();
        let fib = Fib::read(&word_doc).unwrap();
        let runs =
            parse_fkps(&parse_ctx, fib, &word_doc, &table, 0xFA, FkpKind::Chpx, &[]).unwrap();
        assert_eq!(parse_ctx.diagnostics().iter().count(), 0);
        (runs.len(), parse_ctx.fuel())
    }

    fn huge_properties() -> Vec<u8> {
        let mut data = 0x3FA2u16.to_le_bytes().to_vec();
        data.extend([0x16, 0x24, 1].repeat(0x3FA2 / 3));
        data
    }

    #[test]
    fn refused_huge_properties_leave_the_delta_unchanged_and_skip_later_modifiers() {
        use crate::ledger::CancelToken;
        use crate::limits::{Limit, Limits};
        use std::assert_matches;
        let data = huge_properties();
        let grpprl = [0x46, 0x66, 0, 0, 0, 0, 0x17, 0x24, 1, 0x46, 0x66, 0, 0, 0, 0];
        let limits = Limits { max_work: 0x3FA2 - 1, ..Limits::DEFAULT };
        let token = CancelToken::new();
        token.cancel();
        for cancelled in [false, true] {
            let parse_ctx = ParseCtx::new(&limits).with_cancel(cancelled.then_some(&token));
            let mut delta = PapDelta::default();
            let result = apply_pap_sprms(&parse_ctx, &grpprl, &data, &mut delta);
            if cancelled {
                assert_matches!(result, Err(ConvertError::Cancelled));
            } else {
                assert_matches!(
                    result,
                    Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 0x3FA1 })
                );
            }
            assert_eq!((delta.in_table, delta.ttp), (None, None));
            assert_eq!(parse_ctx.fuel(), 0x3FA2);
        }
    }

    #[test]
    fn a_truncated_huge_record_bills_no_unread_bytes() {
        let limits = crate::limits::Limits { max_work: 0, ..Default::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let mut delta = PapDelta::default();
        apply_pap_sprms(&parse_ctx, &[0x46, 0x66, 0, 0, 0, 0], &[0xFF, 0x7F], &mut delta).unwrap();
        assert_eq!(parse_ctx.fuel(), 0);
        assert_eq!(delta.in_table, None);
    }

    fn page_run(kind: FkpKind, b_offset: u8, payload: &[u8]) -> Run {
        let mut page = [0u8; 512];
        page[..8].copy_from_slice(&[10, 0, 0, 0, 20, 0, 0, 0]);
        page[8] = b_offset;
        let at = usize::from(b_offset) * 2;
        page[at..at + payload.len()].copy_from_slice(payload);
        page[511] = 1;
        let mut runs = Vec::new();
        parse_fkp_page(&ParseCtx::default(), &page, kind, &[], &mut runs).unwrap();
        assert_eq!(runs.len(), 1);
        runs.pop().unwrap()
    }

    #[test]
    fn a_papx_ends_where_its_length_says_and_never_past_the_page() {
        type Case<'a> = (&'a str, u8, &'a [u8], (u16, Option<u16>, Option<bool>));
        let cases: [Case<'_>; 6] = [
            ("a split operand", 0x80, &[3, 7, 0, 0x0B, 0x46, 5, 0], (7, None, None)),
            ("a whole operand", 0x80, &[4, 7, 0, 0x0B, 0x46, 5, 0], (7, Some(5), None)),
            ("a long split opcode", 0x80, &[0, 2, 7, 0, 0x16, 0x24, 1], (7, None, None)),
            ("a long whole opcode", 0x80, &[0, 3, 7, 0, 0x16, 0x24, 1, 0], (7, None, Some(true))),
            ("the last byte of the page", 0xFE, &[2, 7, 0, 1], (7, None, None)),
            ("a byte past the page", 0xFE, &[3, 7, 0, 1], (0, None, None)),
        ];
        for (case, b_offset, payload, expected) in cases {
            let run = page_run(FkpKind::Papx, b_offset, payload);
            assert_eq!((run.fc_start, run.fc_end), (10, 20), "{case}");
            let pap = &run.props.pap;
            assert_eq!((run.props.istd, pap.ilfo, pap.in_table), expected, "{case}");
        }
    }

    #[test]
    fn every_fkp_run_is_charged_once() {
        assert_eq!(fkp_runs(&[1, 2]), (6, 6));
    }

    #[test]
    fn an_fkp_page_the_bin_table_names_again_is_read_once() {
        assert_eq!(fkp_runs(&[1, 1, 1, 2, 1]), (6, 6));
    }
}
