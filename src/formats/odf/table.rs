//! ODF tables: canonical grid construction with covered-cell consumption,
//! repeats honored in every container, a hard expansion budget for
//! repeat expansion, and typed value-attribute fallback for cells without
//! display text. A spreadsheet drops its hidden sheets, and a sheet its
//! hidden rows, columns and groups; a merge they cut keeps the part that
//! shows.
//!
//! A cell holding a `text:p` or `text:h` shows that text, empty or not; one holding neither shows
//! its typed value, and reports the number style that formats it, which the fallback does not
//! apply.
//!
//! Empty filler runs *inside* the used range materialize fully so every
//! later cell keeps its source coordinates; only trailing filler (empty rows
//! at the end, empty cells at a row's end) is elided. All materialization is
//! charged against the expansion budget, and every position the grid holds
//! against its slots.

use crate::ctx::ParseCtx;
use crate::diagnostics::DiagnosticCode;
use crate::error::ConvertError;
use crate::formats::odf::axes::{Axes, Step, Visibility, declared_end, repeats, row_steps};
use crate::formats::odf::datastyle::{FormatCost, Shown};
use crate::formats::odf::styles::named;
use crate::formats::odf::text::{Ctx, holds_content, parse_children, report_unplaced};
use crate::formats::sheet::format_float;
use crate::limits::{Limit, Limits};
use crate::model::{Block, Cell, GridBuilder, ImageSource, Inline, LinkTarget, List, ListItem};
use crate::package::xml::{Element, ns};
use crate::shared::header::resolve_header_rows;
use crate::shared::hidden_runs::HiddenRuns;
use crate::shared::lossy;
use crate::shared::text::clean_text;
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

pub(super) fn parse_table(elem: &Element, ctx: &Ctx<'_, '_>) -> Result<Vec<Block>, ConvertError> {
    build_table(elem, ctx, Visibility::Ignored)
}

fn build_table(
    elem: &Element,
    ctx: &Ctx<'_, '_>,
    visibility: Visibility,
) -> Result<Vec<Block>, ConvertError> {
    let steps = row_steps(elem, visibility);
    let axes = Axes::of(elem, &steps, visibility, ctx.parse_ctx)?;
    let mut state = TableState::new(ctx.parse_ctx, read_checkboxes(elem), axes);
    walk_steps(&steps, ctx, &mut state)?;
    state.report_omitted(ctx);
    let mut table = state.builder.finish()?;
    if table.grid.is_empty() {
        return Ok(Vec::new());
    }
    let positions =
        u64::try_from(table.grid.iter().map(Vec::len).sum::<usize>()).unwrap_or(u64::MAX);
    ctx.hold(state.held.add(Held { positions, bytes: 0 }));
    table.header_rows = resolve_header_rows(&table, state.header_rows);
    Ok(vec![Block::Table(table)])
}

struct TableState<'l, 'c, 'e> {
    limits: &'l Limits,
    builder: GridBuilder<&'l ParseCtx<'c>>,
    /// Grid slots produced by expansion, checked against its budget.
    expansion: u64,
    pending_rows: u64,
    header_rows: usize,
    header_from: usize,
    rows_emitted: usize,
    /// The sheet's form checkboxes by control id, as the inlines a
    /// `draw:control` in a cell expands to.
    checkboxes: HashMap<String, Vec<Inline>>,
    /// What the cells placed so far hold, every copy counted.
    held: Held,
    axes: Axes<'e>,
    /// Merge origins their hidden row or column displaced, by the first
    /// shown position the merge covers.
    moved: BTreeMap<(u64, u64), Moved<'e>>,
    omitted: Omitted,
}

/// A merge origin waiting at the shown position that takes its place,
/// decoded only once it is placed there.
struct Moved<'e> {
    origin: (u64, u64),
    cell: &'e Element,
    col_span: u32,
    row_span: u32,
    style: Option<&'e str>,
}

/// The source rows and columns that hold content no sheet shows.
#[derive(Default)]
struct Omitted {
    rows: Vec<Range<u64>>,
    cols: Vec<Range<u64>>,
}

/// The grid positions and text bytes parsed tables hold, their nested
/// tables' included.
#[derive(Clone, Copy, Default)]
pub(super) struct Held {
    positions: u64,
    bytes: u64,
}

impl Held {
    pub(super) fn add(self, other: Self) -> Self {
        Self {
            positions: self.positions.saturating_add(other.positions),
            bytes: self.bytes.saturating_add(other.bytes),
        }
    }

    fn times(self, copies: u64) -> Self {
        Self {
            positions: self.positions.saturating_mul(copies),
            bytes: self.bytes.saturating_mul(copies),
        }
    }
}

/// Checkbox controls declared in the table's `office:forms`. The mixed
/// ("unknown") state has no token and is left out.
fn read_checkboxes(table: &Element) -> HashMap<String, Vec<Inline>> {
    let mut out = HashMap::new();
    for cb in table.find_all(ns::OFFICE, "forms").flat_map(|f| f.descendants(ns::FORM, "checkbox"))
    {
        let Some(id) = cb.attr_qualified(ns::XML, "id").or_else(|| cb.attr(ns::FORM, "id")) else {
            continue;
        };
        let state = cb.attr(ns::FORM, "current-state").or_else(|| cb.attr(ns::FORM, "state"));
        let checked = match state {
            Some("checked") => true,
            Some("unchecked") | None => false,
            Some(_) => continue,
        };
        let mut inlines = vec![Inline::Checkbox(checked)];
        let label = cb.attr(ns::FORM, "label").map(clean_text).unwrap_or_default();
        if !label.is_empty() {
            inlines.push(Inline::plain(format!(" {label}")));
        }
        out.insert(id.to_string(), inlines);
    }
    out
}

/// Append the checkboxes anchored in a cell after its content, on the same
/// line, and report every other control anchored there, including one the
/// table does not declare.
fn append_cell_controls(
    cell: &Element,
    ctx: &Ctx<'_, '_>,
    checkboxes: &HashMap<String, Vec<Inline>>,
    blocks: &mut Vec<Block>,
) {
    for control in cell.find_all(ns::DRAW, "control") {
        let Some(found) = control.attr(ns::DRAW, "control").and_then(|id| checkboxes.get(id))
        else {
            ctx.emit(DiagnosticCode::FormControlUnread);
            continue;
        };
        match blocks.last_mut() {
            Some(Block::Paragraph(inlines)) => {
                inlines.push(Inline::plain(" "));
                inlines.extend(found.iter().cloned());
            }
            _ => blocks.push(Block::Paragraph(found.clone())),
        }
    }
}

impl<'l, 'c, 'e> TableState<'l, 'c, 'e> {
    fn new(
        parse_ctx: &'l ParseCtx<'c>,
        checkboxes: HashMap<String, Vec<Inline>>,
        axes: Axes<'e>,
    ) -> Self {
        Self {
            limits: parse_ctx.limits(),
            builder: GridBuilder::new(parse_ctx),
            expansion: 0,
            pending_rows: 0,
            header_rows: 0,
            header_from: 0,
            rows_emitted: 0,
            checkboxes,
            held: Held::default(),
            axes,
            moved: BTreeMap::new(),
            omitted: Omitted::default(),
        }
    }

    fn charge(&mut self, cells: u64) -> Result<(), ConvertError> {
        self.expansion = self.expansion.saturating_add(cells);
        let max = self.limits.max_expansion;
        if self.expansion > max {
            return Err(ConvertError::limit(Limit::Expansion, max));
        }
        Ok(())
    }

    /// The first shown position a cell's span covers, where its merge shows
    /// when the cell itself does not.
    fn anchor(&self, row: u64, span: &CellSpan) -> Option<(u64, u64)> {
        let rows = row..row.saturating_add(u64::from(span.rows));
        let cols = span.cols.start..span.cols.start.saturating_add(u64::from(span.cols_spanned));
        Some((self.axes.rows.first_shown(rows)?, self.axes.cols.first_shown(cols)?))
    }

    /// Count the hidden row and the hidden columns of a left-out cell that
    /// holds content.
    fn omit(&mut self, cell: &Element, rows: Option<Range<u64>>, cols: &Range<u64>) {
        if holds_value(cell) {
            self.omitted.rows.extend(rows);
            self.omitted.cols.push(cols.clone());
        }
    }

    fn omit_moved(&mut self, moved: &Moved<'_>) {
        let (row, col) = moved.origin;
        if holds_value(moved.cell) {
            if self.axes.rows.contains(row) {
                self.omitted.rows.push(row..row.saturating_add(1));
            }
            self.omitted.cols.push(col..col.saturating_add(1));
        }
    }

    /// Report the rows and columns hidden with content, once each, and the
    /// merge origins no covered position took.
    fn report_omitted(&mut self, ctx: &Ctx<'_, '_>) {
        for moved in std::mem::take(&mut self.moved).into_values() {
            self.omit_moved(&moved);
        }
        let rows = HiddenRuns::of(std::mem::take(&mut self.omitted.rows)).count();
        let cols = HiddenRuns::of(std::mem::take(&mut self.omitted.cols))
            .hidden_in(0..u64::MAX)
            .map(|run| run.end - run.start - self.axes.cols.shown_in(run))
            .fold(0u64, u64::saturating_add);
        ctx.emit_count(DiagnosticCode::HiddenOmitted, rows.saturating_add(cols));
    }
}

struct Line<'e> {
    start: u64,
    repeat: u64,
    hidden: bool,
    /// The `table:default-cell-style-name` of the row, which its cells that name no style take.
    style: Option<&'e str>,
}

impl<'e> Line<'e> {
    fn of(row: &'e Element, start: u64, repeat: u64, hidden: bool) -> Self {
        let style = named(row.attr(ns::TABLE, "default-cell-style-name"));
        Self { start, repeat, hidden, style }
    }
}

/// Where a cell sits among the defaults its row and columns name for a cell that names no style.
struct Site<'s> {
    row: Option<&'s str>,
    /// The runs of columns that name a default and overlap the cell.
    columns: &'s [(Range<u64>, &'s str)],
    /// The hidden columns and the cell's own, when a hidden copy is left out whole.
    shown: Option<(&'s HiddenRuns, Range<u64>)>,
}

impl<'s> Site<'s> {
    /// The defaults of every column in `cols`.
    fn new(row: Option<&'s str>, axes: &'s Axes<'s>, cols: &Range<u64>) -> Self {
        Self { row, columns: axes.column_styles(cols), shown: None }
    }

    /// The defaults of the columns in `cols` that show: a hidden copy is left out whole.
    fn shown(row: Option<&'s str>, axes: &'s Axes<'s>, cols: &Range<u64>) -> Self {
        Self { row, columns: axes.column_styles(cols), shown: Some((&axes.cols, cols.clone())) }
    }

    fn shows(&self, run: &Range<u64>) -> bool {
        self.shown.as_ref().is_none_or(|(hidden, cols)| {
            hidden.shown_in(run.start.max(cols.start)..run.end.min(cols.end)) > 0
        })
    }
}

/// A cell of the source: the columns its copies sit in and the extent each
/// spans.
struct CellSpan {
    cols: Range<u64>,
    cols_spanned: u32,
    rows: u32,
}

/// One parsed cell of a row template, reused across row repeats.
enum RowCell {
    Covered {
        cols: Range<u64>,
    },
    Cell {
        cols: Range<u64>,
        shown: u64,
        col_span: u32,
        row_span: u32,
        blocks: Vec<Block>,
        held: Held,
    },
}

/// The text bytes a clone of parsed cell content carries, outside its tables.
fn block_bytes(blocks: &[Block]) -> u64 {
    blocks
        .iter()
        .map(|block| match block {
            Block::Paragraph(inlines) => inline_bytes(inlines),
            Block::Heading { level: _, anchor, content } => {
                optional_bytes(anchor.as_deref()) + inline_bytes(content)
            }
            Block::List(List { marker: _, start: _, items }) => items
                .iter()
                .map(|ListItem { blocks, marker_label }| {
                    optional_bytes(marker_label.as_deref()) + block_bytes(blocks)
                })
                .sum(),
            Block::BlockQuote(blocks) => block_bytes(blocks),
            Block::CodeBlock { lang, text } => optional_bytes(lang.as_deref()) + text_bytes(text),
            Block::Math(tex) => text_bytes(tex),
            Block::Table(_)
            | Block::Rule
            | Block::UnitStart(_)
            | Block::Gap(_)
            | Block::HeaderFooter(_) => 0,
        })
        .sum()
}

fn inline_bytes(inlines: &[Inline]) -> u64 {
    inlines
        .iter()
        .map(|inline| match inline {
            Inline::Text { text, style: _ } => text_bytes(text),
            Inline::Link { content, target } => inline_bytes(content) + target_bytes(target),
            Inline::Image { alt, source, .. } => text_bytes(alt) + source_bytes(source),
            Inline::Math(tex) => text_bytes(tex),
            Inline::Anchor(id) | Inline::NoteRef(id) => text_bytes(id),
            Inline::Checkbox(_) => 3,
            Inline::LineBreak => 1,
        })
        .sum()
}

fn target_bytes(target: &LinkTarget) -> u64 {
    match target {
        LinkTarget::Unit(_) => 0,
        LinkTarget::External(text)
        | LinkTarget::Relative(text)
        | LinkTarget::Anchor { id: text, .. } => text_bytes(text),
    }
}

fn source_bytes(source: &ImageSource) -> u64 {
    match source {
        ImageSource::External(url) => text_bytes(url),
        ImageSource::Asset(_) | ImageSource::Unavailable | ImageSource::Unread => 0,
    }
}

fn optional_bytes(text: Option<&str>) -> u64 {
    text.map_or(0, text_bytes)
}

fn text_bytes(text: &str) -> u64 {
    u64::try_from(text.len()).unwrap_or(u64::MAX)
}

fn walk_steps<'e>(
    steps: &[Step<'e>],
    ctx: &Ctx<'_, '_>,
    state: &mut TableState<'_, '_, 'e>,
) -> Result<(), ConvertError> {
    let mut start = 0u64;
    for step in steps {
        match step {
            Step::Shapes(shape) => report_unplaced(shape, ctx)?,
            Step::HeaderOpen => state.header_from = state.rows_emitted,
            Step::HeaderClose => {
                if state.header_rows == 0 {
                    state.header_rows = state.rows_emitted - state.header_from;
                }
            }
            Step::Row { row, repeat, hidden } => {
                if *hidden {
                    read_row(row, &Line::of(row, start, *repeat, true), ctx, state)?;
                } else {
                    emit_row(row, start, ctx, state, *repeat)?;
                }
                start = start.saturating_add(*repeat);
            }
        }
    }
    Ok(())
}

fn emit_row<'e>(
    row: &'e Element,
    start: u64,
    ctx: &Ctx<'_, '_>,
    state: &mut TableState<'_, '_, 'e>,
    repeat: u64,
) -> Result<(), ConvertError> {
    if row_is_empty(row) {
        state.pending_rows = state.pending_rows.saturating_add(repeat);
        return Ok(());
    }
    // Materialize any buffered empty gap first, in full: rows after the gap
    // keep their source coordinates. The rows are charged so a pathological
    // gap hits the budget instead of memory.
    if state.pending_rows > 0 {
        state.charge(state.pending_rows)?;
        for _ in 0..state.pending_rows {
            state.builder.next_row();
            state.rows_emitted += 1;
        }
        state.pending_rows = 0;
    }
    // Parse the row template exactly once: repeated rows clone the parsed
    // cells instead of reparsing, so per-parse side effects (notes, assets)
    // happen once, and the text bytes and nested grid positions each copy
    // duplicates are charged up front.
    let mut cells = read_row(row, &Line::of(row, start, repeat, false), ctx, state)?;
    state.charge(repeat.saturating_sub(1))?;
    for cell in &cells {
        if let RowCell::Cell { shown, held, .. } = cell {
            let placed = repeat.saturating_mul(*shown);
            let copies = placed.saturating_sub(1);
            ctx.parse_ctx.admit_expansion_text_bytes(held.bytes.saturating_mul(copies))?;
            ctx.parse_ctx.admit_grid_positions(held.positions.saturating_mul(copies))?;
            state.held = state.held.add(held.times(placed));
        }
    }
    for copy in 1..=repeat {
        state.builder.next_row();
        state.rows_emitted += 1;
        emit_parsed_cells(&mut cells, copy == repeat, start, ctx, state)?;
    }
    Ok(())
}

/// Read one row's cells into the reusable template. A cell none of whose
/// copies shows is set aside to move its merge to the position that shows
/// it, else left out unread.
fn read_row<'e>(
    row: &'e Element,
    line: &Line<'e>,
    ctx: &Ctx<'_, '_>,
    state: &mut TableState<'_, '_, 'e>,
) -> Result<Vec<RowCell>, ConvertError> {
    let mut out = Vec::new();
    let mut at = 0u64;
    for cell in row.child_elems() {
        let covered = cell.is(ns::TABLE, "covered-table-cell");
        if !covered && !cell.is(ns::TABLE, "table-cell") {
            continue;
        }
        let end = declared_end(ctx.parse_ctx, at, repeats(cell, "number-columns-repeated"))?;
        let cols = at..end;
        at = end;
        let shown = if line.hidden { 0 } else { state.axes.cols.shown_in(cols.clone()) };
        if covered {
            if shown > 0 {
                out.push(RowCell::Covered { cols });
            }
            continue;
        }
        let span = CellSpan {
            cols_spanned: spanned(cell, "number-columns-spanned"),
            rows: spanned(cell, "number-rows-spanned"),
            cols,
        };
        if shown == 0 {
            hide_cell(cell, line, &span, state);
            continue;
        }
        if shown < span.cols.end - span.cols.start {
            state.omit(cell, None, &span.cols);
        }
        let site = Site::shown(line.style, &state.axes, &span.cols);
        let (blocks, held) = decode_cell(cell, ctx, &state.checkboxes, &site)?;
        out.push(RowCell::Cell {
            cols: span.cols,
            shown,
            col_span: span.cols_spanned,
            row_span: span.rows,
            blocks,
            held,
        });
    }
    Ok(out)
}

fn spanned(cell: &Element, attr: &str) -> u32 {
    cell.attr(ns::TABLE, attr).and_then(|v| v.parse().ok()).unwrap_or(1).max(1)
}

/// A cell no copy of which shows. One written once that opens a merge with a
/// shown part moves there; the rest are left out, because the copies of a
/// repeated merge origin overlap and no producer writes them.
fn hide_cell<'e>(
    cell: &'e Element,
    line: &Line<'e>,
    span: &CellSpan,
    state: &mut TableState<'_, '_, 'e>,
) {
    let once = line.repeat == 1 && span.cols.end - span.cols.start == 1;
    let anchor = state.anchor(line.start, span).filter(|_| once);
    let Some(anchor) = anchor else {
        let rows = line.hidden.then(|| line.start..line.start.saturating_add(line.repeat));
        state.omit(cell, rows, &span.cols);
        return;
    };
    let shown_in = |axis: &HiddenRuns, from: u64, spanned: u32| {
        let shown = axis.shown_in(from..from.saturating_add(u64::from(spanned)));
        u32::try_from(shown).unwrap_or(u32::MAX)
    };
    let moved = Moved {
        origin: (line.start, span.cols.start),
        cell,
        col_span: shown_in(&state.axes.cols, span.cols.start, span.cols_spanned),
        row_span: shown_in(&state.axes.rows, line.start, span.rows),
        style: line.style,
    };
    if let Some(replaced) = state.moved.insert(anchor, moved) {
        state.omit_moved(&replaced);
    }
}

/// A cell's blocks with the checkboxes anchored in it, and what its nested
/// tables and text hold.
fn decode_cell(
    cell: &Element,
    ctx: &Ctx<'_, '_>,
    checkboxes: &HashMap<String, Vec<Inline>>,
    site: &Site<'_>,
) -> Result<(Vec<Block>, Held), ConvertError> {
    let (blocks, nested) = ctx.holding(|| cell_blocks(cell, ctx, site));
    let mut blocks = blocks?;
    append_cell_controls(cell, ctx, checkboxes, &mut blocks);
    let held = nested.add(Held { positions: 0, bytes: block_bytes(&blocks) });
    Ok((blocks, held))
}

/// Whether a cell shows anything: text, a picture, a note or a typed value.
fn holds_value(cell: &Element) -> bool {
    fallback_text(cell).is_some_and(|text| !text.trim().is_empty()) || holds_content(cell)
}

/// Place a row's cells, moving each cell's content into its last copy on
/// the `last_row`; `row` is where the template sits in the source. Only the
/// positions that show take a slot, and a span counts only the positions it
/// covers that show.
fn emit_parsed_cells(
    cells: &mut [RowCell],
    last_row: bool,
    row: u64,
    ctx: &Ctx<'_, '_>,
    state: &mut TableState<'_, '_, '_>,
) -> Result<(), ConvertError> {
    let mut pending_cells: u64 = 0;
    let mut stray: u64 = 0;
    for cell in cells {
        match cell {
            RowCell::Covered { cols } => {
                // One explicitly written covered position each; a stray one
                // (no span accounts for it) becomes an empty cell inside
                // covered().
                state.charge(state.axes.cols.shown_in(cols.clone()))?;
                let mut next = cols.start;
                while let Some(col) = state.axes.cols.first_shown(next..cols.end) {
                    next = col + 1;
                    if let Some(moved) = state.moved.remove(&(row, col)) {
                        let blocks = decode_moved(&moved, ctx, state)?;
                        place_cell(
                            blocks,
                            moved.col_span,
                            moved.row_span,
                            &mut pending_cells,
                            state,
                        )?;
                        continue;
                    }
                    flush_gap(state, &mut pending_cells)?;
                    if !state.builder.covered()? {
                        stray = stray.saturating_add(1);
                    }
                }
            }
            RowCell::Cell { cols, shown, col_span, row_span, blocks, .. } => {
                if blocks.is_empty() && *col_span == 1 && *row_span == 1 {
                    pending_cells = pending_cells.saturating_add(*shown);
                    continue;
                }
                flush_gap(state, &mut pending_cells)?;
                state.charge(*shown)?;
                let rows = state.axes.rows.shown_in(row..row.saturating_add(u64::from(*row_span)));
                let row_span = u32::try_from(rows).unwrap_or(u32::MAX);
                let mut left = *shown;
                let mut next = cols.start;
                while let Some(col) = state.axes.cols.first_shown(next..cols.end) {
                    next = col + 1;
                    left = left.saturating_sub(1);
                    let width =
                        state.axes.cols.shown_in(col..col.saturating_add(u64::from(*col_span)));
                    state.charge(width.saturating_sub(1))?;
                    let content =
                        if last_row && left == 0 { std::mem::take(blocks) } else { blocks.clone() };
                    let col_span = u32::try_from(width).unwrap_or(u32::MAX);
                    state.builder.place(Cell::spanning(content, col_span, row_span))?;
                }
            }
        }
    }
    if stray > 0 {
        ctx.emit_count(DiagnosticCode::TableSpanRepaired, stray);
    }
    Ok(())
}

/// The blocks of a merge origin read where it moved to, under the defaults of the column and
/// row it was written in.
fn decode_moved(
    moved: &Moved<'_>,
    ctx: &Ctx<'_, '_>,
    state: &TableState<'_, '_, '_>,
) -> Result<Vec<Block>, ConvertError> {
    let col = moved.origin.1;
    let site = Site::new(moved.style, &state.axes, &(col..col.saturating_add(1)));
    Ok(decode_cell(moved.cell, ctx, &state.checkboxes, &site)?.0)
}

/// Place a merge origin that moved to the position it covers, or buffer it
/// as a gap when it is empty and spans nothing.
fn place_cell(
    blocks: Vec<Block>,
    col_span: u32,
    row_span: u32,
    pending: &mut u64,
    state: &mut TableState<'_, '_, '_>,
) -> Result<(), ConvertError> {
    if blocks.is_empty() && col_span == 1 && row_span == 1 {
        *pending = pending.saturating_add(1);
        return Ok(());
    }
    flush_gap(state, pending)?;
    state.charge(u64::from(col_span))?;
    state.builder.place(Cell::spanning(blocks, col_span, row_span))?;
    Ok(())
}

/// Materialize a buffered empty-cell run in full so the next cell lands on
/// its source column. Trailing runs are never flushed and stay elided.
fn flush_gap(state: &mut TableState<'_, '_, '_>, pending: &mut u64) -> Result<(), ConvertError> {
    if *pending == 0 {
        return Ok(());
    }
    state.charge(*pending)?;
    for _ in 0..*pending {
        state.builder.place(Cell::default())?;
    }
    *pending = 0;
    Ok(())
}

/// A cell's blocks: its text content, or a typed value-attribute fallback
/// when the producer wrote no display text; display text hidden whole
/// leaves the cell empty, and a fallback reports the number style it ignores.
fn cell_blocks(
    cell: &Element,
    ctx: &Ctx<'_, '_>,
    site: &Site<'_>,
) -> Result<Vec<Block>, ConvertError> {
    let dropped = ctx.dropped();
    let blocks = parse_children(cell.child_elems().filter(|e| !e.is(ns::DRAW, "control")), ctx)?;
    let has_content = blocks.iter().any(|b| match b {
        Block::Paragraph(inlines) => !crate::model::inlines_are_empty(inlines),
        _ => true,
    });
    if has_content || ctx.dropped() != dropped {
        return Ok(blocks);
    }
    let Some(text) = fallback_text(cell) else {
        return Ok(Vec::new());
    };
    if let Some(code) = unapplied_format(cell, ctx, site)?.code() {
        ctx.emit(code);
    }
    Ok(vec![Block::Paragraph(vec![Inline::plain(text)])])
}

/// What the typed value of a cell shows when the producer wrote no display text: a `text:p` or
/// `text:h`, empty or not, is the display text.
fn fallback_text(cell: &Element) -> Option<String> {
    let displays =
        cell.child_elems().any(|child| child.is(ns::TEXT, "p") || child.is(ns::TEXT, "h"));
    if displays { None } else { value_text(cell) }
}

/// The worst cost of the number styles a fallback cell may have: its own style, else its row's
/// default, else the defaults of the columns it covers that show, each run walked charged.
fn unapplied_format(
    cell: &Element,
    ctx: &Ctx<'_, '_>,
    site: &Site<'_>,
) -> Result<FormatCost, ConvertError> {
    let shown = Shown::of(cell.attr(ns::OFFICE, "value-type"));
    if let Some(style) = named(cell.attr(ns::TABLE, "style-name")).or(site.row) {
        return format_cost(ctx, style, shown);
    }
    let mut worst = FormatCost::None;
    for (run, style) in site.columns {
        ctx.parse_ctx.charge(1)?;
        if site.shows(run) {
            worst = worst.max(format_cost(ctx, style, shown)?);
        }
    }
    Ok(worst)
}

/// The cost of one style a cell tries, its lookup charged by the name's length.
fn format_cost(ctx: &Ctx<'_, '_>, style: &str, shown: Shown) -> Result<FormatCost, ConvertError> {
    ctx.parse_ctx.charge(u64::try_from(style.len()).unwrap_or(u64::MAX).max(1))?;
    ctx.styles.format_cost(ctx.parse_ctx, style, shown)
}

fn value_text(cell: &Element) -> Option<String> {
    let number = || cell.attr(ns::OFFICE, "value")?.parse::<f64>().ok();
    match cell.attr(ns::OFFICE, "value-type")? {
        "percentage" => Some(format!("{}%", format_float(number()? * 100.0))),
        "currency" => {
            let v = number()?;
            Some(match cell.attr(ns::OFFICE, "currency") {
                Some(cur) if !cur.is_empty() => format!("{v} {cur}"),
                _ => v.to_string(),
            })
        }
        "float" => Some(number()?.to_string()),
        "date" => cell.attr(ns::OFFICE, "date-value").map(str::to_string),
        "time" => cell.attr(ns::OFFICE, "time-value").map(duration_text),
        "boolean" => cell
            .attr(ns::OFFICE, "boolean-value")
            .map(|b| if b == "true" { "TRUE" } else { "FALSE" }.to_string()),
        "string" => cell.attr(ns::OFFICE, "string-value").map(str::to_string),
        _ => None,
    }
}

fn unit_seconds(in_time: bool, c: char) -> Option<f64> {
    match (in_time, c) {
        (false, 'W') => Some(604_800.0),
        (false, 'D') => Some(86_400.0),
        (true, 'H') => Some(3600.0),
        (true, 'M') => Some(60.0),
        (true, 'S') => Some(1.0),
        _ => None,
    }
}

/// Full ISO 8601 duration -> clock text: `P1DT2H` -> `26:00:00`,
/// `PT26H30M15S` -> `26:30:15`, weeks are 7 days, and fractional values are
/// valid on any component (`PT1.5H` -> `1:30:00`). Year/month components
/// have no fixed length, and unrepresentably large values overflow the
/// clock format, so both keep their raw ISO text.
fn duration_text(iso: &str) -> String {
    let mut total = 0.0f64; // seconds
    let mut number = String::new();
    let mut in_time = false;
    let mut negative = false;
    for c in iso.chars() {
        match c {
            '-' if total == 0.0 && number.is_empty() => negative = true,
            'P' => {}
            'T' => {
                in_time = true;
                number.clear();
            }
            'Y' | 'M' if !in_time => {
                if number.parse::<f64>().is_ok_and(|n| n != 0.0) {
                    return iso.to_string();
                }
                number.clear();
            }
            _ if let Some(unit) = unit_seconds(in_time, c) => {
                total += number.parse::<f64>().unwrap_or(0.0) * unit;
                number.clear();
            }
            c if c.is_ascii_digit() || c == '.' => number.push(c),
            _ => number.clear(),
        }
    }
    // Round to milliseconds; a value the clock format cannot represent
    // exactly keeps its source text instead of silently degrading.
    let total_ms = (total * 1000.0).round();
    if !(0.0..1e18).contains(&total_ms) {
        return iso.to_string();
    }
    let total_ms = lossy::round_u64(total_ms);
    let sign = if negative { "-" } else { "" };
    let (hours, rest) = (total_ms / 3_600_000, total_ms % 3_600_000);
    let (minutes, ms) = (rest / 60_000, rest % 60_000);
    if ms % 1000 != 0 {
        format!("{sign}{hours}:{minutes:02}:{:02}.{:03}", ms / 1000, ms % 1000)
    } else {
        format!("{sign}{hours}:{minutes:02}:{:02}", ms / 1000)
    }
}

fn row_is_empty(row: &Element) -> bool {
    row.child_elems().all(|cell| {
        if cell.is(ns::TABLE, "covered-table-cell") {
            return false; // structural: belongs to a span
        }
        if !cell.is(ns::TABLE, "table-cell") {
            return true;
        }
        cell.attr(ns::TABLE, "number-columns-spanned").is_none()
            && cell.attr(ns::TABLE, "number-rows-spanned").is_none()
            && cell.attr(ns::OFFICE, "value-type").is_none()
            && cell.children.is_empty()
    })
}

/// One sheet of a spreadsheet body.
pub(super) fn parse_spreadsheet(
    sheet: &Element,
    ctx: &Ctx<'_, '_>,
) -> Result<Vec<Block>, ConvertError> {
    let tables = sheet
        .child_elems()
        .filter(|e| e.is(ns::TABLE, "table"))
        .map(|table| {
            let style = table.attr(ns::TABLE, "style-name");
            Ok((table, ctx.styles.table_shown(ctx.parse_ctx, style)?))
        })
        .collect::<Result<Vec<(&Element, bool)>, ConvertError>>()?;
    let multi_sheet = tables.iter().filter(|(_, shown)| *shown).count() > 1;
    let mut blocks = Vec::new();
    for (position, (table, shown)) in tables.into_iter().enumerate() {
        let unit = ctx.parse_ctx.begin_unit(crate::model::UnitKind::Sheet, position)?;
        if !shown {
            ctx.emit(DiagnosticCode::HiddenOmitted);
            continue;
        }
        let named = table
            .attr(ns::TABLE, "style-name")
            .map(|style| ctx.styles.style_master_page(ctx.parse_ctx, "table", style))
            .transpose()?
            .flatten();
        if let Some(master) = ctx.styles.master_or_first(named) {
            super::report_running(master, ctx.parse_ctx);
        }
        let name = table.attr(ns::TABLE, "name").unwrap_or("");
        let content = build_table(table, ctx, Visibility::Applied)?;
        blocks.push(Block::UnitStart(unit.unit().model()));
        if content.is_empty() {
            continue;
        }
        if multi_sheet {
            blocks.push(Block::heading(2, vec![Inline::plain(name)]));
        }
        blocks.extend(content);
    }
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Region;
    use crate::model::MarkerKind;
    use crate::render::markdown::document_to_markdown;
    use crate::shared::test_support::{diagnostic_rows, zip_package};
    use std::assert_matches;

    #[test]
    fn a_left_out_cell_records_its_columns_once_whatever_hidden_runs_they_cross() {
        let xml = format!(
            r#"<table:table-cell xmlns:table="{}" xmlns:text="{}"><text:p>x</text:p></table:table-cell>"#,
            ns::TABLE,
            ns::TEXT
        );
        let parse_ctx = ParseCtx::default();
        let doc = crate::package::xml::parse_xml(
            &parse_ctx,
            xml.as_bytes(),
            crate::package::archive::Role::Probe,
        )
        .unwrap();
        let cell = doc.find(ns::TABLE, "table-cell").unwrap();
        let runs = 500u64;
        let axes = Axes {
            rows: HiddenRuns::default(),
            cols: HiddenRuns::of((0..runs).map(|run| 2 * run..2 * run + 1).collect()),
            ..Axes::default()
        };
        let mut state = TableState::new(&parse_ctx, HashMap::new(), axes);
        for row in 0..runs {
            state.omit(cell, Some(row..row + 1), &(0..2 * runs));
        }
        assert_eq!((state.omitted.rows.len(), state.omitted.cols.len()), (500, 500));
    }

    fn parsed_cell(attrs: &str) -> Element {
        let xml = format!(
            r#"<table:table-cell xmlns:table="{}" xmlns:office="{}" {attrs}/>"#,
            ns::TABLE,
            ns::OFFICE
        );
        crate::package::xml::parse_xml(
            &ParseCtx::default(),
            xml.as_bytes(),
            crate::package::archive::Role::Probe,
        )
        .unwrap()
    }

    #[test]
    fn a_span_attribute_reads_one_unless_it_names_a_larger_count() {
        let doc = parsed_cell(r#"table:a="3" table:b="0" table:c="x" table:d="-2""#);
        let cell = doc.find(ns::TABLE, "table-cell").unwrap();
        for (attr, span) in [("a", 3), ("b", 1), ("c", 1), ("d", 1), ("absent", 1)] {
            assert_eq!(spanned(cell, attr), span, "{attr}");
        }
    }

    #[test]
    fn a_typed_value_reads_as_the_text_its_type_names() {
        for (label, kind, attrs, text) in [
            ("percentage", "percentage", r#"office:value="0.25""#, Some("25%")),
            ("percentage the product inflates", "percentage", r#"office:value="0.07""#, Some("7%")),
            ("percentage without a value", "percentage", "", None),
            ("float", "float", r#"office:value="2.50""#, Some("2.5")),
            ("float that is no number", "float", r#"office:value="x""#, None),
            (
                "currency",
                "currency",
                r#"office:value="1.5" office:currency="USD""#,
                Some("1.5 USD"),
            ),
            ("currency without a symbol", "currency", r#"office:value="1.5""#, Some("1.5")),
            (
                "currency with an empty symbol",
                "currency",
                r#"office:value="1.5" office:currency="""#,
                Some("1.5"),
            ),
            ("currency that is no number", "currency", r#"office:value="x""#, None),
            ("date", "date", r#"office:date-value="2024-02-29""#, Some("2024-02-29")),
            ("time", "time", r#"office:time-value="PT1H30M""#, Some("1:30:00")),
            ("boolean true", "boolean", r#"office:boolean-value="true""#, Some("TRUE")),
            ("boolean false", "boolean", r#"office:boolean-value="false""#, Some("FALSE")),
            ("string", "string", r#"office:string-value="s""#, Some("s")),
            ("unknown type", "void", r#"office:value="1""#, None),
        ] {
            let doc = parsed_cell(&format!(r#"office:value-type="{kind}" {attrs}"#));
            let cell = doc.find(ns::TABLE, "table-cell").unwrap();
            assert_eq!(value_text(cell).as_deref(), text, "{label}");
        }
    }

    #[test]
    fn a_duration_reads_as_clock_text_with_milliseconds_only_when_present() {
        for (iso, clock) in [
            ("PT26H30M15S", "26:30:15"),
            ("P1DT2H", "26:00:00"),
            ("P1W", "168:00:00"),
            ("PT1.5H", "1:30:00"),
            ("PT1.5S", "0:00:01.500"),
            ("PT1M2.25S", "0:01:02.250"),
            ("-PT5S", "-0:00:05"),
            ("P1Y", "P1Y"),
        ] {
            assert_eq!(duration_text(iso), clock, "{iso}");
        }
    }

    #[test]
    fn a_copy_holds_every_owned_string() {
        let image = |source| Inline::Image {
            origin: crate::model::ImageOrigin::new(Region::Body, None),
            alt: "ab".into(),
            source,
        };
        assert_eq!(inline_bytes(&[image(ImageSource::External("http://e/u".into()))]), 2 + 10);
        assert_eq!(inline_bytes(&[image(ImageSource::Unavailable)]), 2);

        let heading = |anchor: Option<&str>| Block::Heading {
            level: 1,
            anchor: anchor.map(str::to_string),
            content: vec![Inline::plain("text")],
        };
        assert_eq!(block_bytes(&[heading(Some("anchor"))]), 6 + 4);
        assert_eq!(block_bytes(&[heading(None)]), 4);

        let code = |lang: Option<&str>| Block::CodeBlock {
            lang: lang.map(str::to_string),
            text: "fn".into(),
        };
        assert_eq!(block_bytes(&[code(Some("rust"))]), 4 + 2);
        assert_eq!(block_bytes(&[code(None)]), 2);

        let item = |label: Option<&str>| ListItem {
            blocks: vec![Block::Paragraph(vec![Inline::plain("abc")])],
            marker_label: label.map(str::to_string),
        };
        let list = Block::List(List {
            marker: MarkerKind::Decimal,
            start: 1,
            items: vec![item(Some("1-a)")), item(None)],
        });
        assert_eq!(block_bytes(&[list]), 4 + 3 + 3);

        let link = |target| Inline::Link { content: vec![Inline::plain("ab")], target };
        for (target, len) in [
            (LinkTarget::External("http://e".into()), 8),
            (LinkTarget::Relative("../e/f".into()), 6),
            (LinkTarget::anchor("anchor".into()), 6),
        ] {
            assert_eq!(inline_bytes(&[link(target)]), 2 + len);
        }

        let quoted = Block::BlockQuote(vec![Block::Paragraph(vec![
            Inline::Math("x^2".into()),
            Inline::Anchor("id".into()),
            Inline::NoteRef("n1".into()),
            Inline::Checkbox(true),
            Inline::LineBreak,
        ])]);
        assert_eq!(block_bytes(&[quoted]), 3 + 2 + 2 + 3 + 1);
        assert_eq!(block_bytes(&[Block::Math("ab".into()), Block::Rule]), 2);
    }

    type Reported = (DiagnosticCode, Region, Option<u32>, u64);

    const FLOAT: &str = r#"office:value-type="float" office:value="5""#;
    const COLUMNS: &str = "<table:table-column/><table:table-column/><table:table-column/>";
    const UNREAD: Reported = (DiagnosticCode::NumberFormatUnread, Region::Body, Some(1), 1);
    const FALLBACK: Reported = (DiagnosticCode::NumberFormatFallback, Region::Body, Some(1), 1);
    const HIDDEN: Reported = (DiagnosticCode::HiddenOmitted, Region::Body, Some(1), 1);
    const GONE: &str = r#"table:visibility="collapse""#;

    const STYLES_END: &str = "</office:document-styles>";

    fn namespaces() -> String {
        format!(
            r#"xmlns:office="{}" xmlns:table="{}" xmlns:text="{}" xmlns:style="{}" xmlns:number="{}""#,
            ns::OFFICE,
            ns::TABLE,
            ns::TEXT,
            ns::STYLE,
            ns::NUMBER
        )
    }

    fn styles_part(named: &str, tail: &str) -> String {
        format!(
            "<office:document-styles {}><office:styles>{named}</office:styles>{tail}",
            namespaces()
        )
    }

    fn package(named: &str, automatic: &str, table: &str) -> Vec<u8> {
        package_of(Some(&styles_part(named, STYLES_END)), automatic, table)
    }

    fn package_of(styles: Option<&str>, automatic: &str, table: &str) -> Vec<u8> {
        let content = format!(
            r#"<office:document-content {}><office:automatic-styles>{automatic}</office:automatic-styles><office:body><office:spreadsheet><table:table table:name="S">{table}</table:table></office:spreadsheet></office:body></office:document-content>"#,
            namespaces()
        );
        let mut parts = vec![("content.xml", content.as_str())];
        parts.extend(styles.map(|styles| ("styles.xml", styles)));
        zip_package(&parts)
    }

    fn convert(package: &[u8]) -> (String, Vec<Reported>) {
        let parse_ctx = ParseCtx::default();
        let doc = crate::formats::odf::parse(&parse_ctx, package).unwrap();
        let policy = crate::ImagePolicy::Placeholder;
        let diagnostics = crate::converter::diagnostics_of(parse_ctx, &doc, policy).unwrap();
        (document_to_markdown(&doc), diagnostic_rows(&diagnostics))
    }

    fn fuel(package: &[u8]) -> u64 {
        let parse_ctx = ParseCtx::default();
        crate::formats::odf::parse(&parse_ctx, package).unwrap();
        parse_ctx.fuel()
    }

    fn cell(attrs: &str, inner: &str) -> String {
        format!("<table:table-cell {attrs}>{inner}</table:table-cell>")
    }

    fn sheet_row(attrs: &str, middle: &str) -> String {
        let end = |text: &str| cell("", &format!("<text:p>{text}</text:p>"));
        format!("<table:table-row {attrs}>{}{middle}{}</table:table-row>", end("a"), end("z"))
    }

    fn rendered(middle: &[&str]) -> String {
        let row = [&["a"], middle, &["z"]].concat();
        let width = row.len();
        format!("|{}\n|{}\n| {} |\n", "  |".repeat(width), " --- |".repeat(width), row.join(" | "))
    }

    #[test]
    fn a_cell_holding_a_paragraph_or_heading_shows_its_display_text_empty_or_not() {
        for (label, inner, shown) in [
            ("no paragraph", "", "5"),
            ("empty paragraph", "<text:p/>", ""),
            ("empty heading", "<text:h/>", ""),
            ("paragraph", "<text:p>5.00</text:p>", "5.00"),
        ] {
            let table = format!("{COLUMNS}{}", sheet_row("", &cell(FLOAT, inner)));
            assert_eq!(convert(&package("", "", &table)), (rendered(&[shown]), vec![]), "{label}");
        }
    }

    #[test]
    fn a_hidden_cell_holds_a_value_only_where_it_shows_no_display_text() {
        let hidden_column =
            format!("<table:table-column/><table:table-column {GONE}/><table:table-column/>");
        let visible = sheet_row("", &cell("", "<text:p>v</text:p>"));
        for (label, inner, reported) in [
            ("no paragraph", "", vec![HIDDEN]),
            ("empty paragraph", "<text:p/>", vec![]),
            ("empty heading", "<text:h/>", vec![]),
            ("paragraph", "<text:p>5</text:p>", vec![HIDDEN]),
        ] {
            let middle = cell(FLOAT, inner);
            let column = format!("{hidden_column}{}", sheet_row("", &middle));
            let row =
                format!("{COLUMNS}<table:table-row {GONE}>{middle}</table:table-row>{visible}");
            assert_eq!(
                convert(&package("", "", &column)),
                (rendered(&[]), reported.clone()),
                "{label} in a hidden column"
            );
            assert_eq!(
                convert(&package("", "", &row)),
                (rendered(&["v"]), reported),
                "{label} in a hidden row"
            );
        }
    }

    const NUMBER: &str = r#"<number:number number:decimal-places="2" number:min-integer-digits="1" number:grouping="true"/>"#;
    const TEXT: &str = r#"office:value-type="string" office:string-value="s""#;
    const STYLES: &str = r#"<number:currency-style style:name="U"><number:currency-symbol>$</number:currency-symbol><number:number/></number:currency-style>
        <number:number-style style:name="F"><number:number/></number:number-style>
        <style:style style:name="cu" style:family="table-cell" style:data-style-name="U"/>
        <style:style style:name="cf" style:family="table-cell" style:data-style-name="F"/>
        <style:style style:name="plain" style:family="table-cell"/>"#;

    fn data_style(kind: &str, inner: &str) -> String {
        format!(r#"<number:{kind} style:name="N">{inner}</number:{kind}>"#)
    }

    fn named_style(name: &str, family: &str, attrs: &str) -> String {
        format!(r#"<style:style style:name="{name}" style:family="{family}" {attrs}/>"#)
    }

    fn formatted(data: &str, attrs: &str) -> (String, Vec<Reported>) {
        let automatic =
            format!("{data}{}", named_style("ce", "table-cell", r#"style:data-style-name="N""#));
        let middle = cell(&format!(r#"table:style-name="ce" {attrs}"#), "");
        convert(&package("", &automatic, &format!("{COLUMNS}{}", sheet_row("", &middle))))
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of number styles and what each costs")]
    fn a_value_shown_for_want_of_display_text_reports_the_number_style_it_ignores() {
        let percentage = r#"office:value-type="percentage" office:value="0.07""#;
        let currency =
            r#"office:value-type="currency" office:value="1234.5" office:currency="USD""#;
        let date = r#"office:value-type="date" office:date-value="2024-02-29""#;
        let time = r#"office:value-type="time" office:time-value="PT1H30M""#;
        let boolean = r#"office:value-type="boolean" office:boolean-value="true""#;
        let sign = "<number:text>%</number:text>";
        let number = |inner: &str| data_style("number-style", inner);
        let percent = |inner: &str| data_style("percentage-style", inner);
        let text = |inner: &str| data_style("text-style", inner);
        let content = "<number:text-content/>";
        for (label, data, attrs, shown, reported) in [
            ("a bare number", number(NUMBER), FLOAT, "5", Some(FALLBACK)),
            ("a number with no attribute", number("<number:number/>"), FLOAT, "5", Some(FALLBACK)),
            ("an empty text", number("<number:text></number:text>"), FLOAT, "5", Some(UNREAD)),
            ("no element", number(""), FLOAT, "5", Some(UNREAD)),
            (
                "a number and text",
                number(&format!("{NUMBER}<number:text> kg</number:text>")),
                FLOAT,
                "5",
                Some(UNREAD),
            ),
            (
                "text before the number",
                number(&format!("<number:text>kg </number:text>{NUMBER}")),
                FLOAT,
                "5",
                Some(UNREAD),
            ),
            ("two numbers", number(&format!("{NUMBER}{NUMBER}")), FLOAT, "5", Some(UNREAD)),
            (
                "a display factor",
                number(r#"<number:number number:display-factor="1000"/>"#),
                FLOAT,
                "5",
                Some(UNREAD),
            ),
            (
                "an attribute of another vocabulary",
                number(r#"<number:number style:decimal-places="2"/>"#),
                FLOAT,
                "5",
                Some(UNREAD),
            ),
            (
                "a decimal replacement",
                number(r#"<number:number number:decimal-replacement="-"/>"#),
                FLOAT,
                "5",
                Some(UNREAD),
            ),
            (
                "embedded text",
                number(
                    r#"<number:number><number:embedded-text number:position="1">-</number:embedded-text></number:number>"#,
                ),
                FLOAT,
                "5",
                Some(UNREAD),
            ),
            (
                "a map",
                number(&format!(
                    r#"{NUMBER}<style:map style:condition="value()&gt;0" style:apply-style-name="cf"/>"#
                )),
                FLOAT,
                "5",
                Some(UNREAD),
            ),
            ("a percentage", percent(&format!("{NUMBER}{sign}")), percentage, "7%", Some(FALLBACK)),
            ("a percentage with no sign", percent(NUMBER), percentage, "7%", Some(UNREAD)),
            (
                "a percentage style on a float",
                percent(&format!("{NUMBER}{sign}")),
                FLOAT,
                "5",
                Some(UNREAD),
            ),
            ("a number style on a percentage", number(NUMBER), percentage, "7%", Some(UNREAD)),
            ("a number style on a currency", number(NUMBER), currency, "1234.5 USD", Some(UNREAD)),
            ("a number style on a date", number(NUMBER), date, "2024-02-29", Some(UNREAD)),
            (
                "a percentage with a padded sign",
                percent(&format!("{NUMBER}<number:text> %</number:text>")),
                percentage,
                "7%",
                Some(UNREAD),
            ),
            (
                "a sign before the number",
                percent(&format!("{sign}{NUMBER}")),
                percentage,
                "7%",
                Some(UNREAD),
            ),
            (
                "two signs",
                percent(&format!("{NUMBER}{sign}{sign}")),
                percentage,
                "7%",
                Some(UNREAD),
            ),
            (
                "a sign that is not text",
                percent(&format!("{NUMBER}<number:day>%</number:day>")),
                percentage,
                "7%",
                Some(UNREAD),
            ),
            (
                "a currency",
                data_style(
                    "currency-style",
                    &format!("<number:currency-symbol>$</number:currency-symbol>{NUMBER}"),
                ),
                currency,
                "1234.5 USD",
                Some(UNREAD),
            ),
            (
                "a date",
                data_style("date-style", "<number:year/>"),
                date,
                "2024-02-29",
                Some(UNREAD),
            ),
            ("a time", data_style("time-style", "<number:hours/>"), time, "1:30:00", Some(UNREAD)),
            (
                "a boolean",
                data_style("boolean-style", "<number:boolean/>"),
                boolean,
                "TRUE",
                Some(UNREAD),
            ),
            ("a text style on a number", text(content), FLOAT, "5", Some(UNREAD)),
            ("a text content", text(content), TEXT, "s", None),
            (
                "text before the content",
                text(&format!("<number:text>Hi </number:text>{content}")),
                TEXT,
                "s",
                Some(UNREAD),
            ),
            (
                "text after the content",
                text(&format!("{content}<number:text>!</number:text>")),
                TEXT,
                "s",
                Some(UNREAD),
            ),
            ("no content", text(""), TEXT, "s", Some(UNREAD)),
            ("text and no content", text("<number:text>!</number:text>"), TEXT, "s", Some(UNREAD)),
            ("a number style on text", number("<number:text/>"), TEXT, "s", None),
            ("a currency style on text", data_style("currency-style", NUMBER), TEXT, "s", None),
        ] {
            assert_eq!(
                formatted(&data, attrs),
                (rendered(&[shown]), reported.into_iter().collect()),
                "{label}"
            );
        }
    }

    #[test]
    fn no_data_style_but_the_number_and_percentage_ones_is_reproduced() {
        let sign = "<number:text>%</number:text>";
        for kind in ["currency-style", "date-style", "time-style", "boolean-style", "text-style"] {
            for inner in [NUMBER.to_string(), format!("{NUMBER}{sign}")] {
                assert_eq!(
                    formatted(&data_style(kind, &inner), FLOAT),
                    (rendered(&["5"]), vec![UNREAD]),
                    "{kind} {inner}"
                );
            }
        }
    }

    fn styled_value(named: &str, automatic: &str, style: &str) -> (String, Vec<Reported>) {
        let middle = cell(&format!(r#"table:style-name="{style}" {FLOAT}"#), "");
        convert(&package(named, automatic, &format!("{COLUMNS}{}", sheet_row("", &middle))))
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of style chains and where each part sits")]
    fn a_cells_number_style_is_the_nearest_its_style_chain_names_in_either_part() {
        let kid = |attrs: &str| named_style("kid", "table-cell", attrs);
        let data_u = STYLES.lines().next().unwrap();
        let cell_u = named_style("cu", "table-cell", r#"style:data-style-name="U""#);
        for (label, named, automatic, style, reported) in [
            ("its own", String::new(), STYLES.to_string(), "cu", Some(UNREAD)),
            ("another", String::new(), STYLES.to_string(), "cf", Some(FALLBACK)),
            ("none", String::new(), STYLES.to_string(), "plain", None),
            ("a style that is not there", String::new(), STYLES.to_string(), "gone", None),
            (
                "a parent's",
                String::new(),
                format!("{STYLES}{}", kid(r#"style:parent-style-name="cu""#)),
                "kid",
                Some(UNREAD),
            ),
            (
                "a grandparent's",
                String::new(),
                format!(
                    "{STYLES}{}{}",
                    kid(r#"style:parent-style-name="mid""#),
                    named_style("mid", "table-cell", r#"style:parent-style-name="cu""#)
                ),
                "kid",
                Some(UNREAD),
            ),
            (
                "the child's over the parent's",
                String::new(),
                format!(
                    "{STYLES}{}",
                    kid(r#"style:parent-style-name="cu" style:data-style-name="F""#)
                ),
                "kid",
                Some(FALLBACK),
            ),
            (
                "the child's over the parent's, the other way",
                String::new(),
                format!(
                    "{STYLES}{}",
                    kid(r#"style:parent-style-name="cf" style:data-style-name="U""#)
                ),
                "kid",
                Some(UNREAD),
            ),
            (
                "an empty name over the parent's",
                String::new(),
                format!(
                    "{STYLES}{}",
                    kid(r#"style:parent-style-name="cu" style:data-style-name="""#)
                ),
                "kid",
                None,
            ),
            (
                "a name no data style bears",
                String::new(),
                format!("{STYLES}{}", kid(r#"style:data-style-name="Gone""#)),
                "kid",
                None,
            ),
            (
                "a style of another family",
                String::new(),
                format!(
                    "{STYLES}{}",
                    named_style("kid", "paragraph", r#"style:data-style-name="U""#)
                ),
                "kid",
                None,
            ),
            (
                "a data style in the styles part",
                data_u.to_string(),
                cell_u.clone(),
                "cu",
                Some(UNREAD),
            ),
            (
                "a style in the styles part",
                format!("{data_u}{cell_u}"),
                String::new(),
                "cu",
                Some(UNREAD),
            ),
            ("a data style in the content part", cell_u, data_u.to_string(), "cu", Some(UNREAD)),
            (
                "a number element that is no style",
                String::new(),
                format!(
                    r#"{STYLES}<number:number style:name="N"/>{}"#,
                    kid(r#"style:data-style-name="N""#)
                ),
                "kid",
                None,
            ),
            (
                "a style of another vocabulary",
                String::new(),
                format!(
                    r#"{STYLES}<style:number-style style:name="N"/>{}"#,
                    kid(r#"style:data-style-name="N""#)
                ),
                "kid",
                None,
            ),
        ] {
            assert_eq!(
                styled_value(&named, &automatic, style),
                (rendered(&["5"]), reported.into_iter().collect()),
                "{label}"
            );
        }
    }

    enum Styles {
        Read,
        Unread,
        Hiding,
        Absent,
    }

    const STYLE_PART_UNREADABLE: Reported =
        (DiagnosticCode::StylePartUnreadable, Region::Container, None, 1);
    const PART_UNREADABLE: Reported = (DiagnosticCode::PartUnreadable, Region::Body, None, 1);

    fn beside_unread_styles(reported: Option<Reported>) -> Vec<Reported> {
        let mut rows: Vec<Reported> =
            [Some(STYLE_PART_UNREADABLE), reported].into_iter().flatten().collect();
        rows.sort_by_key(|row| row.0);
        rows
    }

    fn text_display(value: &str) -> String {
        format!(
            r#"<style:style style:name="h" style:family="text"><style:text-properties text:display="{value}"/></style:style>"#
        )
    }

    fn package_in(part: &Styles, named: &str, automatic: &str, table: &str) -> Vec<u8> {
        let part = match part {
            Styles::Read => Some(styles_part(named, STYLES_END)),
            Styles::Unread => Some(styles_part(&format!("{named}{}", text_display("true")), "")),
            Styles::Hiding => Some(styles_part(&format!("{named}{}", text_display("none")), "")),
            Styles::Absent => None,
        };
        package_of(part.as_deref(), automatic, table)
    }

    fn in_styles(
        part: &Styles,
        (named, automatic): (&str, &str),
        table: &str,
    ) -> (String, Vec<Reported>) {
        convert(&package_in(part, named, automatic, table))
    }

    fn one_styled(style: &str) -> String {
        let middle = cell(&format!(r#"table:style-name="{style}" {FLOAT}"#), "");
        format!("{COLUMNS}{}", sheet_row("", &middle))
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the table of where a style chain ends and what the styles part may have held"
    )]
    fn a_style_the_unread_styles_part_may_have_held_costs_what_an_unread_number_style_does() {
        let kid = |attrs: &str| named_style("kid", "table-cell", attrs);
        let number = data_style("number-style", NUMBER);
        let in_content = format!("{number}{}", kid(r#"style:data-style-name="N""#));
        for (label, named, automatic, style, read, unread) in [
            (
                "its own style, held there",
                STYLES.to_string(),
                named_style("other", "table-cell", ""),
                "plain",
                None,
                Some(UNREAD),
            ),
            (
                "its own style, held nowhere",
                STYLES.to_string(),
                String::new(),
                "gone",
                None,
                Some(UNREAD),
            ),
            ("a family no style bears", String::new(), String::new(), "plain", None, Some(UNREAD)),
            (
                "a style that has nothing to inherit",
                String::new(),
                named_style("plain", "table-cell", ""),
                "plain",
                None,
                None,
            ),
            (
                "a parent held there",
                STYLES.to_string(),
                kid(r#"style:parent-style-name="plain""#),
                "kid",
                None,
                Some(UNREAD),
            ),
            (
                "a grandparent held there",
                STYLES.to_string(),
                format!(
                    "{}{}",
                    kid(r#"style:parent-style-name="mid""#),
                    named_style("mid", "table-cell", r#"style:parent-style-name="plain""#)
                ),
                "kid",
                None,
                Some(UNREAD),
            ),
            (
                "a parent held nowhere",
                STYLES.to_string(),
                kid(r#"style:parent-style-name="ghost""#),
                "kid",
                None,
                Some(UNREAD),
            ),
            (
                "a parent of another family",
                STYLES.to_string(),
                format!(
                    "{}{}",
                    kid(r#"style:parent-style-name="para""#),
                    named_style("para", "paragraph", "")
                ),
                "kid",
                None,
                Some(UNREAD),
            ),
            (
                "a data style held there",
                STYLES.to_string(),
                kid(r#"style:data-style-name="F""#),
                "kid",
                Some(FALLBACK),
                Some(UNREAD),
            ),
            (
                "a data style held nowhere",
                STYLES.to_string(),
                kid(r#"style:data-style-name="Gone""#),
                "kid",
                None,
                Some(UNREAD),
            ),
            (
                "a data style held in the content",
                String::new(),
                in_content,
                "kid",
                Some(FALLBACK),
                Some(FALLBACK),
            ),
            (
                "its own data style over a parent held there",
                STYLES.to_string(),
                format!(
                    "{number}{}",
                    kid(r#"style:parent-style-name="plain" style:data-style-name="N""#)
                ),
                "kid",
                Some(FALLBACK),
                Some(FALLBACK),
            ),
            (
                "its own data style held nowhere over a parent held in the content",
                String::new(),
                format!(
                    "{STYLES}{}",
                    kid(r#"style:parent-style-name="cf" style:data-style-name="Gone""#)
                ),
                "kid",
                None,
                Some(UNREAD),
            ),
            (
                "an empty data style name over a parent held there",
                STYLES.to_string(),
                kid(r#"style:parent-style-name="plain" style:data-style-name="""#),
                "kid",
                None,
                None,
            ),
            (
                "an empty parent name",
                STYLES.to_string(),
                kid(r#"style:parent-style-name="""#),
                "kid",
                None,
                None,
            ),
        ] {
            let table = one_styled(style);
            let parts = (named.as_str(), automatic.as_str());
            assert_eq!(
                in_styles(&Styles::Read, parts, &table),
                (rendered(&["5"]), read.into_iter().collect()),
                "{label}, the part read"
            );
            assert_eq!(
                in_styles(&Styles::Unread, parts, &table),
                (rendered(&["5"]), beside_unread_styles(unread)),
                "{label}, the part did not"
            );
        }
    }

    #[test]
    fn a_default_style_the_unread_styles_part_may_have_held_costs_what_an_unread_number_style_does()
    {
        let col = |attrs: &str| format!("<table:table-column {attrs}/>");
        let default = |name: &str| format!(r#"table:default-cell-style-name="{name}""#);
        let styled = named_style("own", "table-cell", "");
        let columns = |name: &str| {
            format!("<table:table-column/>{}<table:table-column/>", col(&default(name)))
        };
        for (label, columns, row, attrs, read, unread) in [
            ("a row default held nowhere", columns(""), default("gone"), FLOAT, None, Some(UNREAD)),
            (
                "a column default held nowhere",
                columns("gone"),
                String::new(),
                FLOAT,
                None,
                Some(UNREAD),
            ),
            ("an empty row default", columns(""), default(""), FLOAT, None, None),
            ("an empty column default", columns(""), String::new(), FLOAT, None, None),
            (
                "the cell's own style over a row default held nowhere",
                columns(""),
                default("gone"),
                r#"table:style-name="own" office:value-type="float" office:value="5""#,
                None,
                None,
            ),
            (
                "a row default held in the content over a column default held nowhere",
                columns("gone"),
                default("own"),
                FLOAT,
                None,
                None,
            ),
        ] {
            let table = format!("{columns}{}", sheet_row(&row, &cell(attrs, "")));
            let parts = (STYLES, styled.as_str());
            assert_eq!(
                in_styles(&Styles::Read, parts, &table),
                (rendered(&["5"]), read.into_iter().collect()),
                "{label}, the part read"
            );
            assert_eq!(
                in_styles(&Styles::Unread, parts, &table),
                (rendered(&["5"]), beside_unread_styles(unread)),
                "{label}, the part did not"
            );
        }
    }

    #[test]
    fn every_typed_value_shown_without_a_style_the_unread_styles_part_may_have_held_reports_it() {
        let automatic = [
            named_style("kid", "table-cell", r#"style:parent-style-name="ghost""#),
            named_style("lost", "table-cell", r#"style:data-style-name="Gone""#),
            named_style("held", "table-cell", r#"style:data-style-name="N""#),
            data_style("number-style", NUMBER),
        ]
        .concat();
        for (value, attrs, collected) in [
            ("7%", r#"office:value-type="percentage" office:value="0.07""#, Some(UNREAD)),
            ("s", TEXT, None),
            (
                "2026-10-23",
                r#"office:value-type="date" office:date-value="2026-10-23""#,
                Some(UNREAD),
            ),
        ] {
            for (style, read, unread) in [
                ("gone", None, Some(UNREAD)),
                ("kid", None, Some(UNREAD)),
                ("lost", None, Some(UNREAD)),
                ("held", collected, collected),
            ] {
                let middle = cell(&format!(r#"table:style-name="{style}" {attrs}"#), "");
                let table = format!("{COLUMNS}{}", sheet_row("", &middle));
                let parts = (STYLES, automatic.as_str());
                assert_eq!(
                    in_styles(&Styles::Read, parts, &table),
                    (rendered(&[value]), read.into_iter().collect()),
                    "{value} in {style}, the part read"
                );
                assert_eq!(
                    in_styles(&Styles::Unread, parts, &table),
                    (rendered(&[value]), beside_unread_styles(unread)),
                    "{value} in {style}, the part did not"
                );
            }
        }
    }

    #[test]
    fn a_styles_part_that_is_absent_or_lost_leaves_the_names_it_would_hold_silent() {
        let automatic = format!(
            "{}{}",
            named_style("kid", "table-cell", r#"style:parent-style-name="plain""#),
            named_style("named", "table-cell", r#"style:data-style-name="F""#)
        );
        for (part, reported) in [(Styles::Absent, vec![]), (Styles::Hiding, vec![PART_UNREADABLE])]
        {
            for style in ["cu", "kid", "named", "gone"] {
                assert_eq!(
                    in_styles(&part, (STYLES, &automatic), &one_styled(style)),
                    (rendered(&["5"]), reported.clone()),
                    "{style}"
                );
            }
        }
    }

    #[test]
    fn the_walk_of_a_style_costs_the_same_whether_or_not_the_styles_part_read() {
        let automatic = [
            named_style("kid", "table-cell", r#"style:parent-style-name="mid""#),
            named_style("mid", "table-cell", r#"style:parent-style-name="ghost""#),
            named_style("lost", "table-cell", r#"style:data-style-name="Gone""#),
            named_style("held", "table-cell", r#"style:data-style-name="N""#),
            data_style("number-style", NUMBER),
        ]
        .concat();
        let middle = ["kid", "lost", "held", "gone", ""]
            .map(|style| cell(&format!(r#"table:style-name="{style}" {FLOAT}"#), ""))
            .concat();
        let columns = r#"<table:table-column table:default-cell-style-name="absent"/>"#;
        let table = format!("{columns}{COLUMNS}{}", sheet_row("", &middle));
        let fuel_of = |part: &Styles| fuel(&package_in(part, STYLES, &automatic, &table));
        assert_eq!(fuel_of(&Styles::Unread), fuel_of(&Styles::Hiding));
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the table of default styles and which cell takes each"
    )]
    fn a_cell_with_no_style_takes_its_rows_default_else_its_columns() {
        let col = |attrs: &str| format!("<table:table-column {attrs}/>");
        let default = |name: &str| format!(r#"table:default-cell-style-name="{name}""#);
        let run = |name: &str| format!(r#"table:number-columns-repeated="2" {}"#, default(name));
        let cells = |attrs: &str| cell(&format!("{attrs} {FLOAT}"), "");
        let twice = r#"table:number-columns-repeated="2""#;
        for (label, columns, row, middle, copies, reported) in [
            (
                "a column",
                [col(""), col(&default("cu")), col("")].concat(),
                String::new(),
                cells(""),
                1,
                Some(UNREAD),
            ),
            (
                "the column before",
                [col(&default("cu")), col(""), col("")].concat(),
                String::new(),
                cells(""),
                1,
                None,
            ),
            (
                "the column after",
                [col(""), col(""), col(&default("cu"))].concat(),
                String::new(),
                cells(""),
                1,
                None,
            ),
            (
                "a run of columns",
                [col(""), col(&run("cu"))].concat(),
                String::new(),
                cells(""),
                1,
                Some(UNREAD),
            ),
            (
                "a run that starts where the cell ends",
                [col(""), col(""), col(&run("cu"))].concat(),
                String::new(),
                cells(""),
                1,
                None,
            ),
            (
                "a column in a group",
                format!(
                    "{}<table:table-column-group>{}</table:table-column-group>{}",
                    col(""),
                    col(&default("cu")),
                    col("")
                ),
                String::new(),
                cells(""),
                1,
                Some(UNREAD),
            ),
            ("its row", COLUMNS.to_string(), default("cu"), cells(""), 1, Some(UNREAD)),
            (
                "a row's empty name",
                [col(""), col(&default("cu")), col("")].concat(),
                default(""),
                cells(""),
                1,
                Some(UNREAD),
            ),
            (
                "the row over the column",
                [col(""), col(&default("cu")), col("")].concat(),
                default("cf"),
                cells(""),
                1,
                Some(FALLBACK),
            ),
            (
                "its own over the row",
                COLUMNS.to_string(),
                default("cu"),
                cells(r#"table:style-name="plain""#),
                1,
                None,
            ),
            (
                "its own over the column",
                [col(""), col(&default("cu")), col("")].concat(),
                String::new(),
                cells(r#"table:style-name="plain""#),
                1,
                None,
            ),
            (
                "its own empty name",
                COLUMNS.to_string(),
                default("cu"),
                cells(r#"table:style-name="""#),
                1,
                Some(UNREAD),
            ),
            (
                "columns that differ, the worse last",
                [col(""), col(&default("cf")), col(&default("cu"))].concat(),
                String::new(),
                cells(twice),
                2,
                Some(UNREAD),
            ),
            (
                "columns that differ, the worse first",
                [col(""), col(&default("cu")), col(&default("cf"))].concat(),
                String::new(),
                cells(twice),
                2,
                Some(UNREAD),
            ),
            (
                "a hidden column the cell's copies cross",
                [col(""), col(&format!("{GONE} {}", default("cu"))), col(""), col("")].concat(),
                String::new(),
                cells(r#"table:number-columns-repeated="3""#),
                2,
                Some(HIDDEN),
            ),
            (
                "columns that agree",
                [col(""), col(&default("cf")), col(&default("cf"))].concat(),
                String::new(),
                cells(twice),
                2,
                Some(FALLBACK),
            ),
        ] {
            let table = format!("{columns}{}", sheet_row(&row, &middle));
            assert_eq!(
                convert(&package("", STYLES, &table)),
                (rendered(&vec!["5"; copies]), reported.into_iter().collect()),
                "{label}"
            );
        }
    }

    #[test]
    fn a_merge_origin_a_hidden_column_displaced_keeps_the_defaults_it_had_there() {
        let col = |attrs: &str| format!("<table:table-column {attrs}/>");
        let default = |name: &str| format!(r#"table:default-cell-style-name="{name}""#);
        let origin = cell(&format!(r#"table:number-columns-spanned="2" {FLOAT}"#), "");
        let middle = format!("{origin}<table:covered-table-cell/>");
        for (label, columns, row, reported) in [
            (
                "its column's",
                [col(""), col(&format!("{GONE} {}", default("cu"))), col(""), col("")].concat(),
                String::new(),
                Some(UNREAD),
            ),
            (
                "not the column it moved to",
                [col(""), col(GONE), col(&default("cu")), col("")].concat(),
                String::new(),
                None,
            ),
            (
                "its row's",
                [col(""), col(GONE), col(""), col("")].concat(),
                default("cu"),
                Some(UNREAD),
            ),
        ] {
            let table = format!("{columns}{}", sheet_row(&row, &middle));
            assert_eq!(
                convert(&package("", STYLES, &table)),
                (rendered(&["5"]), reported.into_iter().collect()),
                "{label}"
            );
        }
    }

    #[test]
    fn a_repeated_cell_and_a_repeated_row_report_once_per_cell_written() {
        let repeated = cell(
            &format!(r#"table:style-name="cu" table:number-columns-repeated="3" {FLOAT}"#),
            "",
        );
        let table =
            format!("{COLUMNS}{}", sheet_row(r#"table:number-rows-repeated="2""#, &repeated));
        assert_eq!(convert(&package("", STYLES, &table)).1, [UNREAD]);
    }

    #[test]
    fn a_name_resolves_once_and_each_name_a_cell_tries_is_charged() {
        let col =
            |name: &str| format!(r#"<table:table-column table:default-cell-style-name="{name}"/>"#);
        let automatic = [
            STYLES.to_string(),
            named_style("c1", "table-cell", r#"style:parent-style-name="cf""#),
            named_style("c2", "table-cell", r#"style:parent-style-name="cf""#),
            named_style("d1", "table-cell", ""),
            named_style("d2", "table-cell", ""),
            named_style("d3", "table-cell", ""),
        ]
        .concat();
        let cells = |style: &str| cell(&format!("{style} {FLOAT}"), "");
        let own = |styles: [&str; 3]| {
            let middle =
                styles.map(|style| cells(&format!(r#"table:style-name="{style}""#))).concat();
            fuel(&package("", &automatic, &format!("{COLUMNS}{}", sheet_row("", &middle))))
        };
        let unstyled = own(["", "", ""]);
        assert_eq!(own(["c1", "c2", "c1"]) - unstyled, 3 * 2 + (2 + 1) + 1);

        let middle = cells(r#"table:number-columns-repeated="3""#);
        let across = |names: [&str; 3]| {
            let columns = format!("<table:table-column/>{}", names.map(col).concat());
            fuel(&package("", &automatic, &format!("{columns}{}", sheet_row("", &middle))))
        };
        assert_eq!(across(["d1", "d2", "d3"]) - across(["", "", ""]), 3 + 3 * 2 + 3);

        let middle = cells(r#"table:number-columns-repeated="4""#);
        let hidden = |names: [&str; 3]| {
            let gone = names.map(|name| {
                let default = format!(r#"table:default-cell-style-name="{name}""#);
                format!(
                    "<table:table-column {GONE} {}/>",
                    if name.is_empty() { "" } else { &default }
                )
            });
            let columns = format!("<table:table-column/><table:table-column/>{}", gone.concat());
            fuel(&package("", &automatic, &format!("{columns}{}", sheet_row("", &middle))))
        };
        assert_eq!(hidden(["d1", "d2", "d3"]) - hidden(["", "", ""]), 3);
    }

    #[test]
    fn a_cell_with_display_text_walks_none_of_its_columns_defaults() {
        let runs = 50;
        let middle = cell(
            &format!(r#"table:number-columns-repeated="{runs}" {FLOAT}"#),
            "<text:p>5</text:p>",
        );
        let across = |column: &str| {
            let columns = format!("<table:table-column/>{}", column.repeat(runs));
            fuel(&package("", STYLES, &format!("{columns}{}", sheet_row("", &middle))))
        };
        assert_eq!(
            across(r#"<table:table-column table:default-cell-style-name="cf"/>"#),
            across(r#"<table:table-column table:unnamed-cell-style-name="cf"/>"#)
        );
    }

    #[test]
    fn each_fallback_cell_under_a_row_default_pays_for_the_name_it_looks_up() {
        let growth = |length: usize| {
            let name = "r".repeat(length);
            let automatic = named_style(&name, "table-cell", "");
            let default = format!(r#"table:default-cell-style-name="{name}""#);
            let row = |cells: usize| {
                let middle = cell(FLOAT, "").repeat(cells);
                let columns = "<table:table-column/>".repeat(cells + 2);
                let table = format!("{columns}{}", sheet_row(&default, &middle));
                fuel(&package("", &automatic, &table))
            };
            row(41) - row(1)
        };
        assert_eq!(growth(1000) - growth(1), 40 * 999);
    }

    #[test]
    fn only_the_copies_that_show_take_their_columns_defaults() {
        let col = |attrs: &str| format!("<table:table-column {attrs}/>");
        let default = |name: &str| format!(r#"table:default-cell-style-name="{name}""#);
        let middle = cell(&format!(r#"table:number-columns-repeated="3" {FLOAT}"#), "");
        for (label, columns, reported) in [
            (
                "a hidden copy's worse default",
                [col(""), col(&format!("{GONE} {}", default("cu"))), col(&default("cf")), col("")]
                    .concat(),
                vec![HIDDEN, FALLBACK],
            ),
            (
                "a shown copy's worse default",
                [col(""), col(&format!("{GONE} {}", default("cf"))), col(&default("cu")), col("")]
                    .concat(),
                vec![UNREAD, HIDDEN],
            ),
            (
                "a hidden run that ends inside the cell",
                [col(""), col(&format!("{GONE} {}", run_of(2, "cu"))), col("")].concat(),
                vec![(DiagnosticCode::HiddenOmitted, Region::Body, Some(1), 2)],
            ),
        ] {
            let table = format!("{columns}{}", sheet_row("", &middle));
            assert_eq!(convert(&package("", STYLES, &table)).1, reported, "{label}");
        }
    }

    fn run_of(count: u32, name: &str) -> String {
        format!(r#"table:number-columns-repeated="{count}" table:default-cell-style-name="{name}""#)
    }

    #[test]
    fn a_table_in_a_cell_gives_its_cells_its_own_columns_defaults() {
        let nested = |column: &str| {
            let cell = cell(FLOAT, "");
            format!("<table:table>{column}<table:table-row>{cell}</table:table-row></table:table>")
        };
        let unstyled = "<table:table-column/>";
        let styled = r#"<table:table-column table:default-cell-style-name="cu"/>"#;
        let outer_styled = r#"<table:table-column/><table:table-column table:default-cell-style-name="cu"/><table:table-column/>"#;
        for (label, columns, column, reported) in [
            ("its column's", COLUMNS, styled, vec![UNREAD]),
            ("not the column around it", outer_styled, unstyled, vec![]),
            (
                "not a column after the cell",
                COLUMNS,
                r#"<table:table-column/><table:table-column table:default-cell-style-name="cu"/>"#,
                vec![],
            ),
        ] {
            let middle = cell("", &nested(column));
            let table = format!("{columns}{}", sheet_row("", &middle));
            assert_eq!(convert(&package("", STYLES, &table)).1, reported, "{label}");
        }
    }

    #[test]
    fn a_cell_style_that_inherits_from_itself_fails_where_a_value_needs_it() {
        let automatic = [
            STYLES.to_string(),
            named_style("a", "table-cell", r#"style:parent-style-name="b""#),
            named_style("b", "table-cell", r#"style:parent-style-name="a""#),
        ]
        .concat();
        let parse = |inner: &str| {
            let middle = cell(&format!(r#"table:style-name="a" {FLOAT}"#), inner);
            let table = format!("{COLUMNS}{}", sheet_row("", &middle));
            crate::formats::odf::parse(&ParseCtx::default(), &package("", &automatic, &table))
        };
        assert_matches!(
            parse("").unwrap_err(),
            ConvertError::Malformed { reason: "style inheritance cycle", .. }
        );
        parse("<text:p>5</text:p>").unwrap();
    }
}
