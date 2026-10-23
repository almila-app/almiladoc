//! RTF table assembly: cell properties captured at `\cellx` time, per-depth
//! row accumulation (nested tables live at `\itap` depth); the canonical
//! grid construction is the shared edge-based assembly (`shared::grid`).

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::model::{Block, Inline, inlines_are_empty};
use crate::shared::blockstyle::{BlockStyle, StyledRun};
use crate::shared::grid::{CellProp, GridRow, build_edge_table};

#[derive(Default)]
struct TableBuild {
    rows: Vec<GridRow>,
    /// Props declared by the current `\trowd ... \cellx` run.
    row_props: Vec<CellProp>,
    prop_cursor: usize,
    pending_prop: CellProp,
    /// The open row's closed cells.
    cells: Vec<(Vec<Block>, CellProp)>,
    row_header: bool,
}

#[derive(Default)]
struct PendingCell {
    blocks: Vec<Block>,
    run: StyledRun,
}

/// Table builders indexed by nesting depth - 1, plus the per-depth pending
/// cell content.
pub(super) struct TableState<'c> {
    parse_ctx: &'c ParseCtx<'c>,
    tables: Vec<TableBuild>,
    cells: Vec<PendingCell>,
}

impl<'c> TableState<'c> {
    pub(super) fn new(parse_ctx: &'c ParseCtx<'c>) -> Self {
        Self { parse_ctx, tables: Vec::new(), cells: Vec::new() }
    }

    pub(super) fn depth(&self) -> usize {
        self.tables.len()
    }

    fn table_at(&mut self, depth: usize) -> &mut TableBuild {
        self.tables.resize_with(self.tables.len().max(depth), TableBuild::default);
        &mut self.tables[depth - 1]
    }

    fn cell(&mut self, depth: usize) -> &mut PendingCell {
        self.cells.resize_with(self.cells.len().max(depth), PendingCell::default);
        &mut self.cells[depth - 1]
    }

    fn flush_cell_run(&mut self, depth: usize) {
        let PendingCell { blocks, run } = self.cell(depth);
        run.flush(blocks);
    }

    pub(super) fn begin_row(&mut self, depth: usize) {
        let t = self.table_at(depth);
        t.row_props.clear();
        t.prop_cursor = 0;
        t.pending_prop = CellProp::default();
        t.row_header = false;
    }

    pub(super) fn mark_header_row(&mut self, depth: usize) {
        self.table_at(depth).row_header = true;
    }

    /// The property slot the next `\cellx` will seal.
    pub(super) fn pending_prop(&mut self, depth: usize) -> &mut CellProp {
        &mut self.table_at(depth).pending_prop
    }

    /// `\cellxN`: seal the pending properties with the right boundary.
    pub(super) fn declare_cell(&mut self, depth: usize, right: i64) {
        let t = self.table_at(depth);
        let mut prop = std::mem::take(&mut t.pending_prop);
        prop.right = right;
        t.row_props.push(prop);
    }

    pub(super) fn push_cell_paragraph(
        &mut self,
        depth: usize,
        style: Option<BlockStyle>,
        inlines: Vec<Inline>,
    ) {
        let PendingCell { blocks, run } = self.cell(depth);
        if let Some(style) = style {
            run.push(style, inlines, blocks);
        } else {
            run.flush(blocks);
            if !inlines_are_empty(&inlines) {
                blocks.push(Block::Paragraph(inlines));
            }
        }
    }

    pub(super) fn has_pending_cell(&mut self, depth: usize) -> bool {
        self.flush_cell_run(depth);
        !self.cell(depth).blocks.is_empty()
    }

    /// Whether a row at `depth` is partially built (pending cell content or
    /// already-closed cells awaiting their `\row`).
    pub(super) fn has_partial_row(&mut self, depth: usize) -> bool {
        self.has_pending_cell(depth)
            || self.tables.get(depth - 1).is_some_and(|t| !t.cells.is_empty())
    }

    /// `\cell` / `\nestcell`: close the cell, folding in any completed
    /// deeper table.
    pub(super) fn end_cell(
        &mut self,
        depth: usize,
        style: Option<BlockStyle>,
        inlines: Vec<Inline>,
    ) -> Result<(), ConvertError> {
        self.push_cell_paragraph(depth, style, inlines);
        self.flush_cell_run(depth);
        self.flush_into_cell(depth + 1, depth)?;
        let blocks = std::mem::take(&mut self.cell(depth).blocks);
        let t = self.table_at(depth);
        let prop = t.row_props.get(t.prop_cursor).copied().unwrap_or_default();
        t.prop_cursor += 1;
        t.cells.push((blocks, prop));
        Ok(())
    }

    /// `\row` / `\nestrow`: close the row (its last cell must already be
    /// closed by the caller).
    pub(super) fn end_row(&mut self, depth: usize) {
        let t = self.table_at(depth);
        t.prop_cursor = 0;
        let cells = std::mem::take(&mut t.cells);
        if !cells.is_empty() {
            t.rows.push(GridRow { cells, header: t.row_header });
        }
    }

    pub(super) fn take_table(&mut self, depth: usize) -> Result<Option<Block>, ConvertError> {
        if self.tables.len() < depth || self.tables[depth - 1].rows.is_empty() {
            return Ok(None);
        }
        let t = std::mem::take(&mut self.tables[depth - 1]);
        build_edge_table(self.parse_ctx, t.rows)
    }

    fn flush_into_cell(&mut self, depth: usize, into: usize) -> Result<(), ConvertError> {
        if let Some(block) = self.take_table(depth)? {
            self.flush_cell_run(into);
            self.cell(into).blocks.push(block);
        }
        Ok(())
    }

    pub(super) fn collapse_nested(&mut self) -> Result<(), ConvertError> {
        for depth in (2..=self.tables.len()).rev() {
            self.flush_into_cell(depth, depth - 1)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styled_paragraphs_are_grouped_within_one_cell() {
        let parse_ctx = ParseCtx::default();
        let mut state = TableState::new(&parse_ctx);
        state.begin_row(1);
        state.declare_cell(1, 1000);
        state.push_cell_paragraph(1, Some(BlockStyle::Code), vec![Inline::plain("first")]);
        state.end_cell(1, Some(BlockStyle::Code), vec![Inline::plain("second")]).unwrap();
        state.end_row(1);

        let Block::Table(table) = state.take_table(1).unwrap().unwrap() else {
            panic!("expected a table")
        };
        let crate::model::CellSlot::Origin(cell) = &table.grid[0][0] else {
            panic!("expected an origin cell")
        };
        let [Block::CodeBlock { text, .. }] = &cell.blocks[..] else {
            panic!("unexpected cell blocks: {:?}", cell.blocks)
        };
        assert_eq!(text, "first\nsecond");
    }

    fn one_cell_row(state: &mut TableState<'_>, text: &str) {
        state.end_cell(1, None, vec![Inline::plain(text)]).unwrap();
        state.end_row(1);
    }

    #[test]
    fn a_header_mark_holds_for_every_row_until_the_next_row_definition() {
        let parse_ctx = ParseCtx::default();
        let mut state = TableState::new(&parse_ctx);
        state.begin_row(1);
        state.mark_header_row(1);
        state.declare_cell(1, 1000);
        one_cell_row(&mut state, "a");
        one_cell_row(&mut state, "b");
        state.begin_row(1);
        state.declare_cell(1, 1000);
        one_cell_row(&mut state, "c");

        let Block::Table(table) = state.take_table(1).unwrap().unwrap() else {
            panic!("expected a table")
        };
        assert_eq!((table.grid.len(), table.header_rows), (3, 2));
    }

    #[test]
    fn pending_cell_content_is_kept_per_depth_whichever_depth_is_touched_first() {
        let parse_ctx = ParseCtx::default();
        let mut state = TableState::new(&parse_ctx);
        state.push_cell_paragraph(2, None, vec![Inline::plain("inner")]);
        let pending = [1, 2, 3, 2].map(|depth| state.has_pending_cell(depth));
        assert_eq!(pending, [false, true, false, true]);
    }

    #[test]
    fn a_styled_paragraph_still_grouping_is_pending_cell_content() {
        let parse_ctx = ParseCtx::default();
        let mut state = TableState::new(&parse_ctx);
        state.push_cell_paragraph(1, Some(BlockStyle::Code), vec![Inline::plain("code")]);
        assert!(state.has_pending_cell(1));
    }

    #[test]
    fn a_nested_table_survives_work_at_the_outer_depth() {
        let parse_ctx = ParseCtx::default();
        let mut state = TableState::new(&parse_ctx);
        state.begin_row(2);
        state.declare_cell(2, 500);
        state.end_cell(2, None, vec![Inline::plain("inner")]).unwrap();
        state.end_row(2);
        state.begin_row(1);
        assert_eq!(state.depth(), 2);
        assert!(matches!(state.take_table(2).unwrap(), Some(Block::Table(_))));
    }

    #[test]
    fn a_row_without_closed_cells_is_dropped_with_its_header_mark() {
        let parse_ctx = ParseCtx::default();
        let mut state = TableState::new(&parse_ctx);
        state.begin_row(1);
        state.mark_header_row(1);
        state.declare_cell(1, 1000);
        state.end_row(1);
        state.begin_row(1);
        state.declare_cell(1, 1000);
        one_cell_row(&mut state, "1");
        one_cell_row(&mut state, "2");

        let Block::Table(table) = state.take_table(1).unwrap().unwrap() else {
            panic!("expected a table")
        };
        assert_eq!((table.grid.len(), table.header_rows), (2, 0));
    }

    #[test]
    fn a_row_without_its_own_definition_reuses_the_boundaries_of_the_row_before() {
        let parse_ctx = ParseCtx::default();
        let mut state = TableState::new(&parse_ctx);
        state.begin_row(1);
        state.declare_cell(1, 1000);
        state.declare_cell(1, 2000);
        for row in [["a", "b"], ["c", "d"]] {
            for text in row {
                state.end_cell(1, None, vec![Inline::plain(text)]).unwrap();
            }
            state.end_row(1);
        }

        let Block::Table(table) = state.take_table(1).unwrap().unwrap() else {
            panic!("expected a table")
        };
        assert_eq!(table.grid.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2]);
    }

    #[test]
    fn a_closed_cell_makes_the_row_partial_until_its_row_ends() {
        let parse_ctx = ParseCtx::default();
        let mut state = TableState::new(&parse_ctx);
        state.begin_row(1);
        state.declare_cell(1, 1000);
        state.end_cell(1, None, vec![Inline::plain("a")]).unwrap();
        assert!(state.has_partial_row(1));
        state.end_row(1);
        assert!(!state.has_partial_row(1));
    }
}
