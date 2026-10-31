use crate::limits::{Limit, LimitExceeded};
use crate::model::{Block, Inline, inlines_are_empty};
use std::collections::VecDeque;
use std::ops::Range;

/// Canonical table grid.
///
/// **Invariant:** every logical grid position appears exactly once -
/// content and spans exist only on the origin slot, and each
/// position covered by a span holds a [`CellSlot::Covered`] marker pointing
/// back at its origin. Frontends construct grids through one internal builder
/// that enforces this, or span-less through `Table::from_rows`, so a `Table`
/// handed to a consumer always holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct Table {
    /// Rows of slots. Rows may differ in length when the source is ragged.
    pub grid: Vec<Vec<CellSlot>>,
    /// Number of leading rows that are header rows (0 = no header).
    pub header_rows: usize,
}

/// One position in a [`Table::grid`]: either a cell or the shadow of one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize),
    serde(tag = "kind", content = "value", rename_all = "snake_case")
)]
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

impl CellSlot {
    pub(crate) fn origin(&self) -> Option<&Cell> {
        match self {
            Self::Origin(cell) => Some(cell),
            Self::Covered { .. } => None,
        }
    }
}

/// A table cell and the extent it spans.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
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
        Self::new(Vec::new())
    }
}

impl Cell {
    /// A cell spanning one position.
    #[must_use]
    pub fn new(blocks: Vec<Block>) -> Self {
        Self { blocks, col_span: 1, row_span: 1 }
    }

    /// A one-paragraph cell spanning one position.
    #[must_use]
    pub fn from_inlines(inlines: Vec<Inline>) -> Self {
        Self::new(vec![Block::Paragraph(inlines)])
    }

    /// A cell covering `col_span` by `row_span` positions; either span given
    /// as 0 is raised to 1.
    #[must_use]
    pub fn spanning(blocks: Vec<Block>, col_span: u32, row_span: u32) -> Self {
        Self { blocks, col_span: col_span.max(1), row_span: row_span.max(1) }
    }

    /// True when the cell holds nothing that would render: only paragraphs
    /// count toward emptiness, so a cell with a table or list in it is not
    /// empty even if that content is blank.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks
            .iter()
            .all(|b| matches!(b, Block::Paragraph(inlines) if inlines_are_empty(inlines)))
    }
}

impl Table {
    /// Build a plain span-less table from rows of cells (spreadsheets, CSV).
    #[must_use]
    pub fn from_rows(rows: Vec<Vec<Cell>>, header_rows: usize) -> Self {
        let mut grid: Vec<Vec<CellSlot>> = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|cell| CellSlot::Origin(Cell { col_span: 1, row_span: 1, ..cell }))
                    .collect()
            })
            .collect();
        trim_empty_tail(&mut grid);
        Self { grid, header_rows }
    }

    /// True when the table is a single origin cell (any covered padding aside).
    #[must_use]
    pub fn is_single_cell(&self) -> bool {
        self.only_cell().is_some()
    }

    pub(crate) fn origins(&self) -> impl DoubleEndedIterator<Item = &Cell> {
        self.grid.iter().flatten().filter_map(CellSlot::origin)
    }

    pub(crate) fn only_cell(&self) -> Option<&Cell> {
        let mut origins = self.origins();
        let only = origins.next()?;
        origins.next().is_none().then_some(only)
    }
}

fn trim_empty_tail(grid: &mut Vec<Vec<CellSlot>>) {
    while grid.last().is_some_and(|r| r.iter().all(|s| s.origin().is_none_or(Cell::is_empty))) {
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

/// The area an origin owns, the origin among its positions.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Span {
    pub(crate) origin: (usize, usize),
    pub(crate) cols: usize,
    pub(crate) rows: usize,
}

impl Span {
    fn cover_in(&self, row: usize) -> Cover {
        let (origin_row, origin_col) = self.origin;
        let first = origin_col + usize::from(row == origin_row);
        Cover { cols: first..origin_col + self.cols, origin: self.origin }
    }
}

/// The columns of one row that the span of `origin` covers.
#[derive(Debug)]
pub(crate) struct Cover {
    pub(crate) cols: Range<usize>,
    pub(crate) origin: (usize, usize),
}

/// The covers of rows asked in ascending order, holding only the spans that
/// reach the current row, never a position.
#[derive(Debug, Default)]
pub(crate) struct Sweep {
    started: usize,
    reaching: Vec<Span>,
}

impl Sweep {
    /// The covers of `row` by the spans that reach it, ascending by column;
    /// `spans` ascend by origin row.
    pub(crate) fn row(&mut self, spans: &[Span], row: usize) -> Vec<Cover> {
        let starting = spans[self.started..].partition_point(|span| span.origin.0 <= row);
        self.reaching.extend_from_slice(&spans[self.started..self.started + starting]);
        self.started += starting;
        self.reaching.retain(|span| row < span.origin.0 + span.rows);
        let mut covers: Vec<Cover> = self.reaching.iter().map(|span| span.cover_in(row)).collect();
        covers.sort_by_key(|cover| cover.cols.start);
        covers
    }
}

/// Sole constructor for spanning [`Table`] grids; [`Table::from_rows`] builds
/// span-less ones. Enforces the exactly-once invariant:
/// spans cover their positions, later placements skip over them,
/// and overlapping spans are clamped rather than double-counted.
#[derive(Debug)]
pub(crate) struct GridBuilder<L> {
    grid: Vec<Vec<CellSlot>>,
    /// Every span that covers more than its origin, in placement order.
    spans: Vec<Span>,
    sweep: Sweep,
    /// What the cursor's row has covered at or past the cursor, nearest first.
    ahead: VecDeque<Cover>,
    /// Covered positions claimed by span expansion, charged against
    /// `max_expansion` *before* any per-position work so a tiny document
    /// carrying a huge span cannot force unbounded work.
    expansion: u64,
    ledger: L,
}

impl<L: GridLedger> GridBuilder<L> {
    pub(crate) fn new(ledger: L) -> Self {
        Self {
            grid: Vec::new(),
            spans: Vec::new(),
            sweep: Sweep::default(),
            ahead: VecDeque::new(),
            expansion: 0,
            ledger,
        }
    }

    pub(crate) fn next_row(&mut self) {
        self.grid.push(Vec::new());
        self.ahead = self.sweep.row(&self.spans, self.grid.len() - 1).into();
    }

    fn row_index(&mut self) -> usize {
        if self.grid.is_empty() {
            self.grid.push(Vec::new());
        }
        self.grid.len() - 1
    }

    /// Put a position already admitted, as every covered one is.
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

    /// The origin covering the cursor's position `col`, which the cursor
    /// then passes.
    fn take_covered(&mut self, col: usize) -> Option<(usize, usize)> {
        let cover = self.ahead.front_mut().filter(|cover| cover.cols.start == col)?;
        let origin = cover.origin;
        cover.cols.next();
        if cover.cols.is_empty() {
            self.ahead.pop_front();
        }
        Some(origin)
    }

    /// Materialize covered positions at the cursor, so the next placement
    /// lands on the first genuinely free column.
    fn skip_covered(&mut self, row: usize) {
        while let Some((origin_row, origin_col)) = self.take_covered(self.grid[row].len()) {
            self.put(row, CellSlot::Covered { origin_row, origin_col });
        }
    }

    /// Place a cell at the next free position of the current row. The cell's
    /// spans cover positions the cursor has yet to reach: an explicitly
    /// written covered cell (ODF) consumes one via [`GridBuilder::covered`],
    /// while producers that omit them (HTML, OOXML) have the positions
    /// materialized automatically when the next cell is placed.
    ///
    /// The complete span area is charged against the expansion budget before
    /// any expansion happens, and refused past it. Each position the span
    /// then owns is admitted before it is covered, in rows the table
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
        self.skip_covered(row);
        let origin = (row, self.grid.get(row).map_or(0, Vec::len));
        self.ledger.admit_positions(1)?;
        // Earlier spans clamp the columns only: one that covers a position
        // below this origin covers its column in this row too.
        let col_span = self
            .ahead
            .front()
            .and_then(|cover| u32::try_from(cover.cols.start - origin.1).ok())
            .map_or(cols, |gap| gap.min(cols));
        for _ in 1..col_span {
            self.ledger.admit_positions(1)?;
        }
        for _ in 1..rows {
            self.ledger.admit_positions(u64::from(col_span))?;
        }
        self.register(Span { origin, cols: col_span as usize, rows: rows as usize });
        self.put(row, CellSlot::Origin(Cell { col_span, row_span: rows, ..cell }));
        Ok(())
    }

    fn register(&mut self, span: Span) {
        if span.cols > 1 {
            self.ahead.push_front(span.cover_in(span.origin.0));
        }
        if span.cols > 1 || span.rows > 1 {
            self.spans.push(span);
        }
    }

    /// Consume one explicitly-written covered position (ODF
    /// `covered-table-cell`). Returns `false` when no span accounts for the
    /// position - the stray marker then becomes an empty cell.
    pub(crate) fn covered(&mut self) -> Result<bool, L::Error> {
        let row = self.row_index();
        let col = self.grid[row].len();
        if let Some((origin_row, origin_col)) = self.take_covered(col) {
            self.put(row, CellSlot::Covered { origin_row, origin_col });
            Ok(true)
        } else {
            self.push(row, CellSlot::Origin(Cell::default()))?;
            Ok(false)
        }
    }

    pub(crate) fn finish(self) -> Result<Table, L::Error> {
        self.finish_rows(false)
    }

    pub(crate) fn finish_preserving_rows(self) -> Result<Table, L::Error> {
        self.finish_rows(true)
    }

    fn finish_rows(mut self, preserve_rows: bool) -> Result<Table, L::Error> {
        let mut sweep = Sweep::default();
        for row in 0..self.grid.len() {
            for Cover { cols, origin: (origin_row, origin_col) } in sweep.row(&self.spans, row) {
                for col in cols.start.max(self.grid[row].len())..cols.end {
                    while self.grid[row].len() < col {
                        self.push(row, CellSlot::Origin(Cell::default()))?;
                    }
                    self.put(row, CellSlot::Covered { origin_row, origin_col });
                }
            }
        }
        if !preserve_rows {
            trim_empty_tail(&mut self.grid);
        }
        let rows = self.grid.len();
        for (r, row) in self.grid.iter_mut().enumerate() {
            let width = row.len();
            for (c, slot) in row.iter_mut().enumerate() {
                if let CellSlot::Origin(cell) = slot {
                    cell.row_span = cell.row_span.min(u32::try_from(rows - r).unwrap_or(u32::MAX));
                    cell.col_span = cell.col_span.min(u32::try_from(width - c).unwrap_or(u32::MAX));
                }
            }
        }
        Ok(Table { grid: self.grid, header_rows: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn a_single_cell_is_the_one_origin_whatever_positions_it_covers() {
        let origin = || CellSlot::Origin(Cell::new(Vec::new()));
        let covered = || CellSlot::Covered { origin_row: 0, origin_col: 0 };
        let table = |grid| Table { grid, header_rows: 0 };
        for (grid, single) in [
            (vec![vec![origin()]], true),
            (vec![vec![origin(), covered()]], true),
            (vec![vec![origin()], vec![covered()]], true),
            (vec![vec![], vec![origin()]], true),
            (vec![vec![covered(), origin()]], true),
            (vec![vec![origin(), origin()]], false),
            (vec![vec![origin()], vec![origin()]], false),
            (vec![vec![covered()]], false),
            (vec![], false),
        ] {
            assert_eq!(table(grid.clone()).is_single_cell(), single, "{grid:?}");
        }
    }

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

    impl GridLedger for &mut Vec<u64> {
        type Error = LimitExceeded;

        fn max_expansion(&self) -> u64 {
            BUDGET
        }

        fn admit_positions(&mut self, count: u64) -> Result<(), LimitExceeded> {
            self.push(count);
            Ok(())
        }
    }

    type Recorded<'a> = GridBuilder<&'a mut Vec<u64>>;

    fn admissions(script: impl FnOnce(&mut Recorded<'_>)) -> Vec<u64> {
        let mut calls = Vec::new();
        let mut b = GridBuilder::new(&mut calls);
        b.next_row();
        script(&mut b);
        b.finish().unwrap();
        calls
    }

    type Case = (&'static str, fn(&mut Recorded<'_>), &'static [u64]);

    fn assert_admissions(cases: &[Case]) {
        for &(name, script, expected) in cases {
            assert_eq!(admissions(script), expected, "{name}");
        }
    }

    #[test]
    fn a_span_admits_its_columns_one_by_one_and_each_row_below_by_its_width() {
        assert_admissions(&[
            (
                "a wide span",
                |b| {
                    b.place(spanning_text("w", 3, 1)).unwrap();
                    b.place(text_cell("e")).unwrap();
                },
                &[1, 1, 1, 1],
            ),
            (
                "a block",
                |b| {
                    b.place(spanning_text("b", 2, 2)).unwrap();
                    b.next_row();
                    b.place(text_cell("e")).unwrap();
                },
                &[1, 1, 2, 1],
            ),
            (
                "rows the table never supplies",
                |b| b.place(spanning_text("t", 2, 3)).unwrap(),
                &[1, 1, 2, 2],
            ),
            (
                "an earlier span's column ends a wide origin's",
                |b| {
                    b.place(text_cell("a")).unwrap();
                    b.place(text_cell("b")).unwrap();
                    b.place(spanning_text("t", 1, 2)).unwrap();
                    b.next_row();
                    b.place(spanning_text("w", 4, 2)).unwrap();
                },
                &[1, 1, 1, 1, 1, 1, 2],
            ),
        ]);
    }

    #[test]
    fn finishing_admits_the_gap_before_each_position_the_rows_never_reached() {
        assert_admissions(&[
            (
                "gaps in one short row",
                |b| {
                    for _ in 0..2 {
                        b.place(text_cell("h")).unwrap();
                    }
                    b.place(spanning_text("t", 1, 2)).unwrap();
                    b.place(text_cell("h")).unwrap();
                    b.place(spanning_text("t", 1, 2)).unwrap();
                    b.next_row();
                    b.place(text_cell("o")).unwrap();
                },
                &[1, 1, 1, 1, 1, 1, 1, 1, 1, 1],
            ),
            (
                "rows past a short one",
                |b| {
                    for _ in 0..3 {
                        b.place(text_cell("h")).unwrap();
                    }
                    b.place(spanning_text("t", 2, 3)).unwrap();
                    b.next_row();
                    b.place(text_cell("o")).unwrap();
                    b.next_row();
                },
                &[1, 1, 1, 1, 1, 2, 2, 1, 1, 1, 1, 1, 1],
            ),
        ]);
    }

    #[test]
    fn an_explicit_covered_position_admits_itself_only_when_no_span_covers_it() {
        assert_admissions(&[
            ("a stray one", |b| assert!(!b.covered().unwrap()), &[1]),
            (
                "one a span covers",
                |b| {
                    b.place(spanning_text("w", 2, 1)).unwrap();
                    assert!(b.covered().unwrap());
                },
                &[1, 1],
            ),
        ]);
    }

    fn builder() -> GridBuilder<Slots> {
        let mut b = GridBuilder::new(ledger(BUDGET, BUDGET));
        b.next_row();
        b
    }

    fn text_cell(t: &str) -> Cell {
        Cell::from_inlines(vec![Inline::plain(t)])
    }

    fn widths(table: &Table) -> Vec<usize> {
        table.grid.iter().map(Vec::len).collect()
    }

    #[test]
    fn row_span_skips_next_row_position() {
        let mut b = builder();
        b.place(Cell::spanning(vec![], 1, 2)).unwrap();
        b.place(text_cell("b1")).unwrap();
        b.next_row();
        b.place(text_cell("b2")).unwrap();
        let t = b.finish().unwrap();
        assert_eq!(widths(&t), vec![2, 2]);
        assert_matches!(t.grid[1][0], CellSlot::Covered { origin_row: 0, origin_col: 0 });
        assert_matches!(&t.grid[1][1], CellSlot::Origin(c) if !c.is_empty());
    }

    #[test]
    fn explicit_covered_consumes_exactly_one() {
        // ODF shape: spanning cell followed by its explicit covered marker.
        let mut b = builder();
        b.place(Cell::spanning(vec![], 2, 1)).unwrap();
        assert!(b.covered().unwrap());
        b.place(text_cell("end")).unwrap();
        let t = b.finish().unwrap();
        assert_eq!(widths(&t), vec![3]);
    }

    #[test]
    fn omitted_covered_positions_materialize() {
        // HTML/OOXML shape: no explicit covered cells at all.
        let mut b = builder();
        b.place(Cell::spanning(vec![], 2, 1)).unwrap();
        b.place(text_cell("end")).unwrap();
        let t = b.finish().unwrap();
        assert_eq!(widths(&t), vec![3]);
        assert_matches!(t.grid[0][1], CellSlot::Covered { origin_row: 0, origin_col: 0 });
        assert_matches!(&t.grid[0][2], CellSlot::Origin(c) if !c.is_empty());
    }

    #[test]
    fn covered_row_consumption() {
        let mut b = builder();
        b.place(Cell::spanning(vec![], 1, 2)).unwrap();
        b.place(text_cell("b1")).unwrap();
        b.next_row();
        assert!(b.covered().unwrap());
        b.place(text_cell("b2")).unwrap();
        let t = b.finish().unwrap();
        assert_eq!(widths(&t), vec![2, 2]);
    }

    #[test]
    fn pending_positions_behind_a_gap_materialize_in_column_order() {
        let mut b = builder();
        for col in 0..24 {
            b.place(spanning_text("x", 1, if col % 2 == 1 { 2 } else { 1 })).unwrap();
        }
        b.next_row();
        b.place(text_cell("y")).unwrap();
        let t = b.finish().unwrap();
        assert_eq!(widths(&t), vec![24, 24]);
        for (col, slot) in t.grid[1].iter().enumerate() {
            if col % 2 == 1 {
                assert_matches!(slot, CellSlot::Covered { origin_row: 0, origin_col } if *origin_col == col);
            } else {
                assert_matches!(slot, CellSlot::Origin(_));
            }
        }
    }

    #[test]
    fn stray_covered_becomes_empty_cell() {
        let mut b = builder();
        assert!(!b.covered().unwrap());
        b.place(text_cell("x")).unwrap();
        let t = b.finish().unwrap();
        assert_eq!(widths(&t), vec![2]);
    }

    #[test]
    fn a_span_covers_only_the_rows_it_spans() {
        let mut b = builder();
        b.place(text_cell("a")).unwrap();
        b.place(spanning_text("t", 1, 2)).unwrap();
        b.next_row();
        b.place(text_cell("b")).unwrap();
        b.next_row();
        b.place(text_cell("c")).unwrap();
        b.place(text_cell("d")).unwrap();
        let t = b.finish().unwrap();
        assert_eq!(widths(&t), vec![2, 2, 2]);
        assert_matches!(t.grid[1][1], CellSlot::Covered { origin_row: 0, origin_col: 1 });
        assert_matches!(&t.grid[2][1], CellSlot::Origin(c) if !c.is_empty());
    }

    #[test]
    fn spans_met_out_of_column_order_cover_their_own_columns() {
        let mut b = builder();
        for _ in 0..3 {
            b.place(text_cell("h")).unwrap();
        }
        b.place(spanning_text("far", 1, 3)).unwrap();
        b.next_row();
        b.place(spanning_text("near", 1, 2)).unwrap();
        b.next_row();
        for _ in 0..3 {
            b.place(text_cell("t")).unwrap();
        }
        let t = b.finish().unwrap();
        assert_eq!(widths(&t), vec![4, 4, 5]);
        assert_matches!(t.grid[2][0], CellSlot::Covered { origin_row: 1, origin_col: 0 });
        assert_matches!(t.grid[2][3], CellSlot::Covered { origin_row: 0, origin_col: 3 });
        assert_spans_backed(&t);
    }

    #[test]
    fn a_span_placed_before_an_earlier_rows_cover_is_reached_first() {
        let mut b = builder();
        for _ in 0..3 {
            b.place(text_cell("h")).unwrap();
        }
        b.place(spanning_text("tall", 1, 2)).unwrap();
        b.next_row();
        b.place(spanning_text("wide", 2, 1)).unwrap();
        b.place(text_cell("e")).unwrap();
        let t = b.finish().unwrap();
        assert_eq!(widths(&t), vec![4, 4]);
        assert_matches!(t.grid[1][1], CellSlot::Covered { origin_row: 1, origin_col: 0 });
        assert_matches!(&t.grid[1][2], CellSlot::Origin(c) if !c.is_empty());
        assert_matches!(t.grid[1][3], CellSlot::Covered { origin_row: 0, origin_col: 3 });
    }

    #[test]
    fn explicit_covered_cells_consume_only_the_positions_spans_cover() {
        let mut b = builder();
        b.place(text_cell("a")).unwrap();
        b.place(spanning_text("tall", 1, 2)).unwrap();
        b.place(spanning_text("wide", 2, 1)).unwrap();
        let row0 = [b.covered().unwrap(), b.covered().unwrap()];
        b.next_row();
        let row1 = [b.covered().unwrap(), b.covered().unwrap(), b.covered().unwrap()];
        assert_eq!((row0, row1), ([true, false], [false, true, false]));
    }

    fn spanning_text(t: &str, cols: u32, rows: u32) -> Cell {
        Cell::spanning(vec![Block::Paragraph(vec![Inline::plain(t)])], cols, rows)
    }

    /// Every position a stored span claims must hold a covered marker
    /// pointing back at that origin.
    fn assert_spans_backed(t: &Table) {
        for (r, row) in t.grid.iter().enumerate() {
            for (c, slot) in row.iter().enumerate() {
                if let Some(cell) = slot.origin() {
                    assert_span_backed(t, (r, c), cell);
                }
            }
        }
    }

    fn assert_span_backed(t: &Table, (r, c): (usize, usize), cell: &Cell) {
        for dr in 0..cell.row_span as usize {
            for dc in 0..cell.col_span as usize {
                if (dr, dc) == (0, 0) {
                    continue;
                }
                let covered = t.grid.get(r + dr).and_then(|row| row.get(c + dc));
                assert_matches!(
                    covered,
                    Some(CellSlot::Covered { origin_row, origin_col })
                        if *origin_row == r && *origin_col == c,
                    "span of origin ({r},{c}) not backed at ({},{})",
                    r + dr,
                    c + dc
                );
            }
        }
    }

    #[test]
    fn overlapping_spans_clamp_the_late_origin() {
        let mut b = builder();
        b.place(spanning_text("tall", 1, 3)).unwrap();
        b.place(text_cell("a")).unwrap();
        b.next_row();
        assert!(b.covered().unwrap());
        b.place(spanning_text("wide", 2, 2)).unwrap();
        b.next_row();
        assert!(b.covered().unwrap());
        b.place(text_cell("tail")).unwrap();
        let t = b.finish().unwrap();
        assert_spans_backed(&t);
    }

    #[test]
    fn conflicting_span_rectangles_stay_consistent() {
        let mut b = builder();
        b.place(spanning_text("block", 2, 2)).unwrap(); // owns (1,0),(1,1)
        b.next_row();
        b.place(spanning_text("late", 2, 1)).unwrap();
        let t = b.finish().unwrap();
        assert_matches!(t.grid[1][0], CellSlot::Covered { .. });
        assert_matches!(t.grid[1][1], CellSlot::Covered { .. });
        assert_matches!(&t.grid[1][2], CellSlot::Origin(c) if c.col_span == 2);
        assert_spans_backed(&t);
    }

    #[test]
    fn trimmed_rows_clamp_surviving_spans() {
        let mut b = builder();
        b.place(spanning_text("x", 1, 3)).unwrap();
        b.next_row();
        b.next_row();
        let t = b.finish().unwrap();
        assert_eq!(t.grid.len(), 1);
        assert_matches!(&t.grid[0][0], CellSlot::Origin(c) if c.row_span == 1);
        assert_spans_backed(&t);
    }

    #[test]
    fn trimmed_rows_clamp_spans_below_the_first_row() {
        let mut b = builder();
        b.place(text_cell("a")).unwrap();
        b.next_row();
        b.place(spanning_text("x", 1, 3)).unwrap();
        b.next_row();
        b.next_row();
        let t = b.finish().unwrap();
        assert_eq!(widths(&t), vec![1, 1]);
        assert_matches!(&t.grid[1][0], CellSlot::Origin(c) if c.row_span == 1);
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
        let mut b = builder();
        let err = b.place(Cell::spanning(vec![], u32::MAX, u32::MAX)).unwrap_err();
        assert_eq!(err, LimitExceeded { limit: Limit::Expansion, max: BUDGET });
    }

    #[test]
    fn accumulated_spans_hit_the_expansion_budget() {
        let mut b = builder();
        b.expansion = BUDGET - 10;
        b.place(Cell::spanning(vec![], 3, 3)).unwrap(); // 8 more: still within
        let err = b.place(Cell::spanning(vec![], 2, 2)).unwrap_err(); // 3 more: over
        assert_eq!(err, LimitExceeded { limit: Limit::Expansion, max: BUDGET });
    }

    #[test]
    fn covered_tail_behind_short_row_gap_materializes() {
        let mut b = builder();
        for _ in 0..5 {
            b.place(text_cell("h")).unwrap();
        }
        b.place(spanning_text("tall", 1, 2)).unwrap();
        b.next_row();
        b.place(text_cell("only")).unwrap();
        let t = b.finish().unwrap();
        assert_eq!(t.grid[1].len(), 6);
        assert_matches!(t.grid[1][5], CellSlot::Covered { origin_row: 0, origin_col: 5 });
        assert_matches!(&t.grid[0][5], CellSlot::Origin(c) if c.row_span == 2);
        assert_spans_backed(&t);
    }

    #[test]
    fn trailing_empty_rows_trimmed() {
        let mut b = builder();
        b.place(text_cell("x")).unwrap();
        b.next_row();
        b.place(Cell::default()).unwrap();
        let t = b.finish().unwrap();
        assert_eq!(widths(&t), vec![1]);
    }

    #[test]
    fn every_origin_spans_at_least_one_position() {
        assert_eq!((Cell::default().col_span, Cell::default().row_span), (1, 1));
        let mut b = builder();
        assert!(!b.covered().unwrap());
        b.place(text_cell("a")).unwrap();
        b.place(spanning_text("tall", 1, 2)).unwrap();
        b.next_row();
        b.place(text_cell("b")).unwrap();
        let t = b.finish().unwrap();
        assert_eq!(widths(&t), vec![3, 3]);
        assert_eq!(t.origins().count(), 5);
        assert!(t.origins().all(|c| c.col_span >= 1 && c.row_span >= 1));
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
            b.finish()
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
            b.finish()
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
            b.finish()
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
        );
        assert_eq!(widths(&t), vec![0, 2]);
        assert_eq!(t.header_rows, 1);
        assert!(t.origins().all(|c| (c.col_span, c.row_span) == (1, 1)));
    }
}
