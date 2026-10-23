use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::model::{Block, Cell, CellSlot, Document, Table, inlines_to_plain_text};
use crate::shared::test_support::diagnostic_rows;

pub(super) type Reported = (DiagnosticCode, Region, Option<u32>, u64);

pub(super) fn first_table(doc: &Document) -> &Table {
    match doc.blocks.iter().find_map(|b| match b {
        Block::Table(t) => Some(t),
        _ => None,
    }) {
        Some(t) => t,
        None => panic!("expected a table, got {:?}", doc.blocks),
    }
}

pub(super) fn texts(table: &Table) -> Vec<Vec<String>> {
    table
        .grid
        .iter()
        .map(|row| {
            row.iter()
                .map(|slot| match slot {
                    CellSlot::Origin(cell) => cell
                        .blocks
                        .iter()
                        .filter_map(|b| match b {
                            Block::Paragraph(i) => Some(inlines_to_plain_text(i)),
                            _ => None,
                        })
                        .collect(),
                    CellSlot::Covered { .. } => "<covered>".to_string(),
                })
                .collect()
        })
        .collect()
}

pub(super) fn origin(table: &Table) -> &Cell {
    let CellSlot::Origin(cell) = &table.grid[0][0] else {
        panic!("expected the merge origin at (0,0)");
    };
    cell
}

pub(super) fn reported(parse_ctx: ParseCtx<'_>) -> Vec<Reported> {
    diagnostic_rows(&parse_ctx.into_diagnostics())
}

pub(super) fn in_unit(code: DiagnosticCode, unit: u32, count: u64) -> Reported {
    let region = match code {
        DiagnosticCode::HeaderOmitted => Region::Header,
        DiagnosticCode::FooterOmitted => Region::Footer,
        DiagnosticCode::CommentOmitted => Region::Comment,
        _ => Region::Body,
    };
    (code, region, Some(unit), count)
}
