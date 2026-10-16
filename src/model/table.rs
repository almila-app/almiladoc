use crate::limits::{Limit, LimitExceeded};
use crate::model::{Block, Inline, inlines_are_empty};
use std::collections::HashMap;

/// Canonical table grid.
///
/// **Invariant:** every logical grid position appears exactly once -
/// content and spans exist only on the origin slot, and each
/// position covered by a span holds a [`CellSlot::Covered`] marker pointing
/// back at its origin. Frontends construct grids through one internal builder
/// that enforces this, or span-less through `Table::from_rows`, so a `Table`
/// handed to a consumer always holds.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Table {
    /// Rows of slots. Rows may differ in length when the source is ragged.
    pub grid: Vec<Vec<CellSlot>>,
    /// Number of leading rows that are header rows (0 = no header).
    pub header_rows: usize,
    /// Whether the source used this table for data or for layout.
    pub kind: TableKind,
}

/// What a table is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TableKind {
    /// A real data table.
    #[default]
    Data,
    /// Layout scaffolding (text boxes, positioning tables); renderers may
    /// unwrap trivial layout tables.
    Layout,
}

/// One position in a [`Table::grid`]: either a cell or the shadow of one.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum CellSlot {
    /// The cell itself, holding the content and the span extents.
    Origin(Cell),
    /// A position swallowed by a span, pointing back at the origin that
    /// covers it.
    Covered {
        /// Row of the covering origin.
        origin_row: usize,
        /// Column of the covering origin.
        origin_col: usize,
    },
}

/// A table cell and the extent it spans.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Cell {
    /// The cell's content.
    pub blocks: Vec<Block>,
    /// Columns covered, at least 1.
    pub col_span: u32,
    /// Rows covered, at least 1.
    pub row_span: u32,
}

impl Default for Cell {
    fn default() -> Self {
        Cell::new(Vec::new())
    }
}

impl Cell {
    /// A cell spanning one position.
    pub fn new(blocks: Vec<Block>) -> Self {
        Cell { blocks, col_span: 1, row_span: 1 }
    }

    /// A one-paragraph cell spanning one position.
    pub fn from_inlines(inlines: Vec<Inline>) -> Self {
        Cell::new(vec![Block::Paragraph(inlines)])
    }

    /// A cell covering `col_span` by `row_span` positions; either span given
    /// as 0 is raised to 1.
    pub fn spanning(blocks: Vec<Block>, col_span: u32, row_span: u32) -> Self {
        Cell { blocks, col_span: col_span.max(1), row_span: row_span.max(1) }
    }

    /// True when the cell holds nothing that would render: only paragraphs
    /// count toward emptiness, so a cell with a table or list in it is not
    /// empty even if that content is blank.
    pub fn is_empty(&self) -> bool {
        self.blocks.iter().all(|b| match b {
            Block::Paragraph(inlines) => inlines_are_empty(inlines),
            _ => false,
        })
    }
}

impl Table {
    /// Build a plain span-less table from rows of cells (spreadsheets, CSV).
    pub fn from_rows(rows: Vec<Vec<Cell>>, header_rows: usize, kind: TableKind) -> Table {
        let mut grid: Vec<Vec<CellSlot>> = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|cell| CellSlot::Origin(Cell { col_span: 1, row_span: 1, ..cell }))
                    .collect()
            })
            .collect();
        trim_empty_tail(&mut grid);
        Table { grid, header_rows, kind }
    }

    /// True when the table is a single origin cell (any covered padding aside).
    pub fn is_single_cell(&self) -> bool {
        self.grid.len() == 1
            && self.grid[0].len() == 1
            && matches!(self.grid[0][0], CellSlot::Origin(_))
    }
}

fn trim_empty_tail(grid: &mut Vec<Vec<CellSlot>>) {
    while grid.last().is_some_and(|r| {
        r.iter().all(|s| match s {
            CellSlot::Origin(c) => c.is_empty(),
            CellSlot::Covered { .. } => true,
        })
    }) {
        grid.pop();
    }
}

/// What a grid's positions are counted and charged against as they
/// materialize.
pub(crate) trait GridLedger {
    type Error: From<LimitExceeded>;

    /// Covered positions the spans of one table may claim.
    fn max_expansion(&self) -> u64;

    fn admit_positions(&mut self, count: u64) -> Result<(), Self::Error>;
}

/// Sole constructor for spanning [`Table`] grids; [`Table::from_rows`] builds
/// span-less ones. Enforces the exactly-once invariant:
/// spans register their covered positions, later placements skip over them,
/// and overlapping spans are clamped rather than double-counted.
#[derive(Debug)]
pub(crate) struct GridBuilder<L> {
    grid: Vec<Vec<CellSlot>>,
    /// Positions in future rows covered by an earlier row-spanning origin.
    pending: HashMap<(usize, usize), (usize, usize)>,
    /// Covered positions claimed by span expansion, charged against
    /// `max_expansion` *before* any per-position work so a tiny document
    /// carrying a huge span cannot force unbounded insertions.
    expansion: u64,
    ledger: L,
}

impl<L: GridLedger> GridBuilder<L> {
    pub(crate) fn new(ledger: L) -> Self {
        GridBuilder { grid: Vec::new(), pending: HashMap::new(), expansion: 0, ledger }
    }

    pub(crate) fn next_row(&mut self) {
        self.grid.push(Vec::new());
    }

    fn row_index(&mut self) -> usize {
        if self.grid.is_empty() {
            self.grid.push(Vec::new());
        }
        self.grid.len() - 1
    }

    /// Put a position already admitted, as every pending one is.
    fn put(&mut self, row: usize, slot: CellSlot) {
        if let Some(cells) = self.grid.get_mut(row) {
            cells.push(slot);
        }
    }

    fn push(&mut self, row: usize, slot: CellSlot) -> Result<(), L::Error> {
        self.ledger.admit_positions(1)?;
        self.put(row, slot);
        Ok(())
    }

    /// Materialize pending covered positions at the cursor, so the next
    /// placement lands on the first genuinely free column.
    fn skip_pending(&mut self, row: usize) {
        while let Some((origin_row, origin_col)) = self.pending.remove(&(row, self.grid[row].len()))
        {
            self.put(row, CellSlot::Covered { origin_row, origin_col });
        }
    }

    /// Place a cell at the next free position of the current row. The cell's
    /// spans register covered positions as *pending*: an explicitly written
    /// covered cell (ODF) consumes one via [`GridBuilder::covered`], while
    /// producers that omit them (HTML, OOXML) have the positions
    /// materialized automatically when the next cell is placed.
    ///
    /// The complete span area is charged against the expansion budget before
    /// any expansion happens, and refused past it. Each position the span
    /// then owns is admitted before it turns pending, in rows the table
    /// never supplies too, and is not admitted again when it materializes.
    pub(crate) fn place(&mut self, cell: Cell) -> Result<(), L::Error> {
        let (cols, rows) = (cell.col_span.max(1), cell.row_span.max(1));
        let area = u64::from(cols) * u64::from(rows);
        self.expansion = self.expansion.saturating_add(area - 1);
        let max = self.ledger.max_expansion();
        if self.expansion > max {
            return Err(LimitExceeded { limit: Limit::Expansion, max }.into());
        }
        let row = self.row_index();
        self.skip_pending(row);
        let origin = (row, self.grid.get(row).map_or(0, Vec::len));
        self.ledger.admit_positions(1)?;
        // Clamp the spans to positions this origin can actually own - an
        // overlap with an earlier span would break the exactly-once
        // invariant (the stored span must match the covered markers).
        let columns = |span: u32| (origin.1..).take(span as usize);
        let mut col_span = 1;
        while col_span < cols && !self.pending.contains_key(&(row, origin.1 + col_span as usize)) {
            self.ledger.admit_positions(1)?;
            self.pending.insert((row, origin.1 + col_span as usize), origin);
            col_span += 1;
        }
        let mut row_span = 1;
        while row_span < rows {
            let below = row + row_span as usize;
            if columns(col_span).any(|col| self.pending.contains_key(&(below, col))) {
                break;
            }
            self.ledger.admit_positions(u64::from(col_span))?;
            self.pending.extend(columns(col_span).map(|col| ((below, col), origin)));
            row_span += 1;
        }
        self.put(row, CellSlot::Origin(Cell { col_span, row_span, ..cell }));
        Ok(())
    }

    /// Consume one explicitly-written covered position (ODF
    /// `covered-table-cell`). Returns `false` when no span accounts for the
    /// position - the stray marker then becomes an empty cell.
    pub(crate) fn covered(&mut self) -> Result<bool, L::Error> {
        let row = self.row_index();
        let col = self.grid[row].len();
        match self.pending.remove(&(row, col)) {
            Some((origin_row, origin_col)) => {
                self.put(row, CellSlot::Covered { origin_row, origin_col });
                Ok(true)
            }
            None => {
                self.push(row, CellSlot::Origin(Cell::default()))?;
                Ok(false)
            }
        }
    }

    pub(crate) fn finish(mut self, kind: TableKind) -> Result<Table, L::Error> {
        // Materialize every pending covered position in surviving rows,
        // including tails behind a gap (a short row under a span at a later
        // column): intervening slots fill with empty cells so each covered
        // marker lands on its true column.
        let mut by_row: HashMap<usize, Vec<usize>> = HashMap::new();
        for &(row, col) in self.pending.keys() {
            if row < self.grid.len() {
                by_row.entry(row).or_default().push(col);
            }
        }
        for (row, mut cols) in by_row {
            cols.sort_unstable();
            for col in cols {
                while self.grid[row].len() < col {
                    self.push(row, CellSlot::Origin(Cell::default()))?;
                }
                let Some((origin_row, origin_col)) = self.pending.remove(&(row, col)) else {
                    continue;
                };
                // Placement always consumes pending slots at the cursor, so a
                // still-pending position can only sit at or past the row end.
                debug_assert_eq!(self.grid[row].len(), col);
                self.put(row, CellSlot::Covered { origin_row, origin_col });
            }
        }
        trim_empty_tail(&mut self.grid);
        // Clamp stored spans to the surviving grid so every claimed
        // position is backed by a covered marker (exactly-once invariant).
        let rows = self.grid.len();
        for r in 0..rows {
            let width = self.grid[r].len();
            for c in 0..width {
                if let CellSlot::Origin(cell) = &mut self.grid[r][c] {
                    cell.row_span = cell.row_span.min((rows - r) as u32);
                    cell.col_span = cell.col_span.min((width - c) as u32);
                }
            }
        }
        Ok(Table { grid: self.grid, header_rows: 0, kind })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: u64 = 4_000_000;

    #[derive(Debug)]
    struct Slots {
        max_expansion: u64,
        left: u64,
        max: u64,
    }

    impl GridLedger for Slots {
        type Error = LimitExceeded;

        fn max_expansion(&self) -> u64 {
            self.max_expansion
        }

        fn admit_positions(&mut self, count: u64) -> Result<(), LimitExceeded> {
            let refused = LimitExceeded { limit: Limit::GridSlots, max: self.max };
            self.left = self.left.checked_sub(count).ok_or(refused)?;
            Ok(())
        }
    }

    fn ledger(max_expansion: u64, max: u64) -> Slots {
        Slots { max_expansion, left: max, max }
    }

    fn text_cell(t: &str) -> Cell {
        Cell::from_inlines(vec![Inline::plain(t)])
    }

    fn widths(table: &Table) -> Vec<usize> {
        table.grid.iter().map(|r| r.len()).collect()
    }

    #[test]
    fn col_span_covers_positions() {
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        b.place(Cell::spanning(vec![], 2, 1)).unwrap();
        b.place(text_cell("end")).unwrap();
        let t = b.finish(TableKind::Data).unwrap();
        assert_eq!(widths(&t), vec![3]);
        assert!(matches!(t.grid[0][1], CellSlot::Covered { origin_row: 0, origin_col: 0 }));
    }

    #[test]
    fn row_span_skips_next_row_position() {
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        b.place(Cell::spanning(vec![], 1, 2)).unwrap();
        b.place(text_cell("b1")).unwrap();
        b.next_row();
        b.place(text_cell("b2")).unwrap();
        let t = b.finish(TableKind::Data).unwrap();
        assert_eq!(widths(&t), vec![2, 2]);
        assert!(matches!(t.grid[1][0], CellSlot::Covered { origin_row: 0, origin_col: 0 }));
        assert!(matches!(&t.grid[1][1], CellSlot::Origin(c) if !c.is_empty()));
    }

    #[test]
    fn explicit_covered_consumes_exactly_one() {
        // ODF shape: spanning cell followed by its explicit covered marker.
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        b.place(Cell::spanning(vec![], 2, 1)).unwrap();
        assert!(b.covered().unwrap());
        b.place(text_cell("end")).unwrap();
        let t = b.finish(TableKind::Data).unwrap();
        assert_eq!(widths(&t), vec![3]);
    }

    #[test]
    fn omitted_covered_positions_materialize() {
        // HTML/OOXML shape: no explicit covered cells at all.
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        b.place(Cell::spanning(vec![], 2, 1)).unwrap();
        b.place(text_cell("end")).unwrap();
        let t = b.finish(TableKind::Data).unwrap();
        assert_eq!(widths(&t), vec![3]);
        assert!(matches!(t.grid[0][1], CellSlot::Covered { .. }));
        assert!(matches!(&t.grid[0][2], CellSlot::Origin(c) if !c.is_empty()));
    }

    #[test]
    fn covered_row_consumption() {
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        b.place(Cell::spanning(vec![], 1, 2)).unwrap();
        b.place(text_cell("b1")).unwrap();
        b.next_row();
        assert!(b.covered().unwrap());
        b.place(text_cell("b2")).unwrap();
        let t = b.finish(TableKind::Data).unwrap();
        assert_eq!(widths(&t), vec![2, 2]);
    }

    #[test]
    fn stray_covered_becomes_empty_cell() {
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        assert!(!b.covered().unwrap());
        b.place(text_cell("x")).unwrap();
        let t = b.finish(TableKind::Data).unwrap();
        assert_eq!(widths(&t), vec![2]);
    }

    fn spanning_text(t: &str, cols: u32, rows: u32) -> Cell {
        Cell::spanning(vec![Block::Paragraph(vec![Inline::plain(t)])], cols, rows)
    }

    /// Every position a stored span claims must hold a covered marker
    /// pointing back at that origin.
    fn assert_spans_backed(t: &Table) {
        for (r, row) in t.grid.iter().enumerate() {
            for (c, slot) in row.iter().enumerate() {
                if let CellSlot::Origin(cell) = slot {
                    for dr in 0..cell.row_span as usize {
                        for dc in 0..cell.col_span as usize {
                            if (dr, dc) == (0, 0) {
                                continue;
                            }
                            let covered = t.grid.get(r + dr).and_then(|row| row.get(c + dc));
                            assert!(
                                matches!(covered,
                                    Some(CellSlot::Covered { origin_row, origin_col })
                                        if *origin_row == r && *origin_col == c),
                                "span of origin ({r},{c}) not backed at ({},{})",
                                r + dr,
                                c + dc
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn overlapping_spans_clamp_the_late_origin() {
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        b.place(spanning_text("tall", 1, 3)).unwrap();
        b.place(text_cell("a")).unwrap();
        b.next_row();
        assert!(b.covered().unwrap());
        b.place(spanning_text("wide", 2, 2)).unwrap();
        b.next_row();
        assert!(b.covered().unwrap());
        b.place(text_cell("tail")).unwrap();
        let t = b.finish(TableKind::Data).unwrap();
        assert_spans_backed(&t);
    }

    #[test]
    fn conflicting_span_rectangles_stay_consistent() {
        // An origin placed where covered positions belong lands after them
        // and keeps a span consistent with what it owns.
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        b.place(spanning_text("block", 2, 2)).unwrap(); // owns (1,0),(1,1)
        b.next_row();
        b.place(spanning_text("late", 2, 1)).unwrap();
        let t = b.finish(TableKind::Data).unwrap();
        assert!(matches!(t.grid[1][0], CellSlot::Covered { .. }));
        assert!(matches!(t.grid[1][1], CellSlot::Covered { .. }));
        assert!(matches!(&t.grid[1][2], CellSlot::Origin(c) if c.col_span == 2));
        assert_spans_backed(&t);
    }

    #[test]
    fn trimmed_rows_clamp_surviving_spans() {
        // A row-span whose covered rows are trimmed as trailing filler must
        // not keep claiming them.
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        b.place(Cell::spanning(vec![Block::Paragraph(vec![Inline::plain("x")])], 1, 3)).unwrap();
        b.next_row();
        b.next_row();
        let t = b.finish(TableKind::Data).unwrap();
        assert_eq!(t.grid.len(), 1);
        assert!(matches!(&t.grid[0][0], CellSlot::Origin(c) if c.row_span == 1));
        assert_spans_backed(&t);
    }

    #[test]
    fn the_expansion_budget_is_the_builders() {
        let mut b = GridBuilder::new(ledger(3, BUDGET));
        b.next_row();
        b.place(Cell::spanning(vec![], 2, 2)).unwrap();
        let err = b.place(Cell::spanning(vec![], 2, 1)).unwrap_err();
        assert_eq!(err, LimitExceeded { limit: Limit::Expansion, max: 3 });
    }

    #[test]
    fn huge_span_hits_the_expansion_budget_before_expanding() {
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        let err = b.place(Cell::spanning(vec![], u32::MAX, u32::MAX)).unwrap_err();
        assert_eq!(err, LimitExceeded { limit: Limit::Expansion, max: BUDGET });
    }

    #[test]
    fn accumulated_spans_hit_the_expansion_budget() {
        // Preload the counter instead of allocating millions of real pending
        // entries (the global allocation counter feeds the R22 memory test).
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.expansion = BUDGET - 10;
        b.next_row();
        b.place(Cell::spanning(vec![], 3, 3)).unwrap(); // 8 more: still within
        let err = b.place(Cell::spanning(vec![], 2, 2)).unwrap_err(); // 3 more: over
        assert_eq!(err, LimitExceeded { limit: Limit::Expansion, max: BUDGET });
    }

    #[test]
    fn covered_tail_behind_short_row_gap_materializes() {
        // M5: a row-span at a late column must stay backed even when the
        // next row stops short of it — the gap fills with empty cells.
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        for _ in 0..5 {
            b.place(text_cell("h")).unwrap();
        }
        b.place(spanning_text("tall", 1, 2)).unwrap();
        b.next_row();
        b.place(text_cell("only")).unwrap();
        let t = b.finish(TableKind::Data).unwrap();
        assert_eq!(t.grid[1].len(), 6);
        assert!(matches!(t.grid[1][5], CellSlot::Covered { origin_row: 0, origin_col: 5 }));
        assert!(matches!(&t.grid[0][5], CellSlot::Origin(c) if c.row_span == 2));
        assert_spans_backed(&t);
    }

    #[test]
    fn trailing_empty_rows_trimmed() {
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        b.place(text_cell("x")).unwrap();
        b.next_row();
        b.place(Cell::default()).unwrap();
        let t = b.finish(TableKind::Data).unwrap();
        assert_eq!(widths(&t), vec![1]);
    }

    fn origins(t: &Table) -> impl Iterator<Item = &Cell> {
        t.grid.iter().flatten().filter_map(|slot| match slot {
            CellSlot::Origin(cell) => Some(cell),
            CellSlot::Covered { .. } => None,
        })
    }

    #[test]
    fn every_origin_spans_at_least_one_position() {
        assert_eq!((Cell::default().col_span, Cell::default().row_span), (1, 1));
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        assert!(!b.covered().unwrap());
        b.place(text_cell("a")).unwrap();
        b.place(spanning_text("tall", 1, 2)).unwrap();
        b.next_row();
        b.place(text_cell("b")).unwrap();
        let t = b.finish(TableKind::Data).unwrap();
        assert_eq!(widths(&t), vec![3, 3]);
        assert_eq!(origins(&t).count(), 5);
        assert!(origins(&t).all(|c| c.col_span >= 1 && c.row_span >= 1));
    }

    #[test]
    fn every_position_counts_against_the_slots() {
        let place = |max_slots| {
            let mut b = GridBuilder::new(ledger(BUDGET, max_slots));
            b.next_row();
            b.covered()?;
            b.place(spanning_text("wide", 2, 1))?;
            b.place(spanning_text("tall", 1, 2))?;
            b.next_row();
            b.place(text_cell("b"))?;
            b.finish(TableKind::Data)
        };
        assert_eq!(widths(&place(8).unwrap()), vec![4, 4]);
        assert_eq!(place(7).unwrap_err(), LimitExceeded { limit: Limit::GridSlots, max: 7 });
    }

    #[test]
    fn a_span_counts_its_positions_once_whether_rows_supply_them_or_not() {
        let past_the_rows = |max_slots| {
            let mut b = GridBuilder::new(ledger(BUDGET, max_slots));
            b.next_row();
            b.place(spanning_text("tall", 2, 1000))?;
            b.finish(TableKind::Data)
        };
        assert_eq!(widths(&past_the_rows(2000).unwrap()), vec![2]);
        let refused = past_the_rows(1999).unwrap_err();
        assert_eq!(refused, LimitExceeded { limit: Limit::GridSlots, max: 1999 });
        let written = |max_slots| {
            let mut b = GridBuilder::new(ledger(BUDGET, max_slots));
            b.next_row();
            b.place(spanning_text("block", 2, 2))?;
            b.covered()?;
            b.next_row();
            b.covered()?;
            b.covered()?;
            b.place(text_cell("c"))?;
            b.finish(TableKind::Data)
        };
        assert_eq!(widths(&written(5).unwrap()), vec![2, 3]);
        assert_eq!(written(4).unwrap_err(), LimitExceeded { limit: Limit::GridSlots, max: 4 });
    }

    #[test]
    fn a_stray_covered_position_is_a_slot() {
        let mut b = GridBuilder::new(ledger(BUDGET, 1));
        b.next_row();
        assert!(!b.covered().unwrap());
        let err = b.covered().unwrap_err();
        assert_eq!(err, LimitExceeded { limit: Limit::GridSlots, max: 1 });
    }

    #[test]
    fn rows_without_cells_survive_from_rows_only_before_content() {
        let t = Table::from_rows(
            vec![vec![], vec![text_cell("x"), Cell::spanning(vec![], 3, 2)], vec![Cell::default()]],
            1,
            TableKind::Data,
        );
        assert_eq!(widths(&t), vec![0, 2]);
        assert_eq!(t.header_rows, 1);
        assert!(origins(&t).all(|c| (c.col_span, c.row_span) == (1, 1)));
    }
}
