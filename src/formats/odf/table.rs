//! ODF tables: canonical grid construction with covered-cell consumption,
//! repeats honored in every container, a hard expansion budget for
//! repeat expansion, and typed value-attribute fallback for cells without
//! display text.
//!
//! Empty filler runs *inside* the used range materialize fully so every
//! later cell keeps its source coordinates; only trailing filler (empty rows
//! at the end, empty cells at a row's end) is elided. All materialization is
//! charged against the expansion budget, and every position the grid holds
//! against its slots.

use crate::ctx::ParseCtx;
use crate::diagnostics::DiagnosticCode;
use crate::error::ConvertError;
use crate::formats::odf::text::{Ctx, parse_children, report_unplaced};
use crate::limits::{Limit, Limits};
use crate::model::{
    Block, Cell, GridBuilder, ImageSource, Inline, LinkTarget, List, ListItem, TableKind,
};
use crate::package::xml::{Element, ns};
use crate::shared::header::resolve_header_rows;
use crate::shared::text::clean_text;
use std::collections::HashMap;

pub fn parse_table(elem: &Element, ctx: &Ctx) -> Result<Vec<Block>, ConvertError> {
    let limits = ctx.parse_ctx.limits();
    let mut state = TableState {
        limits,
        builder: GridBuilder::new(ctx.parse_ctx),
        expansion: 0,
        expansion_bytes: 0,
        pending_rows: 0,
        header_rows: 0,
        rows_emitted: 0,
        checkboxes: read_checkboxes(elem),
        held: Held::default(),
    };
    walk_rows(elem, ctx, &mut state, true)?;
    let mut table = state.builder.finish(TableKind::Data)?;
    if table.grid.is_empty() {
        return Ok(Vec::new());
    }
    let positions =
        u64::try_from(table.grid.iter().map(Vec::len).sum::<usize>()).unwrap_or(u64::MAX);
    ctx.hold(state.held.add(Held { positions, bytes: 0 }));
    table.header_rows = resolve_header_rows(&table, state.header_rows);
    Ok(vec![Block::Table(table)])
}

struct TableState<'l, 'c> {
    limits: &'l Limits,
    builder: GridBuilder<&'l ParseCtx<'c>>,
    /// Grid slots produced by expansion, checked against its budget.
    expansion: u64,
    /// Text bytes duplicated by expansion, checked against their own budget.
    expansion_bytes: u64,
    pending_rows: u64,
    header_rows: usize,
    rows_emitted: usize,
    /// The sheet's form checkboxes by control id, as the inlines a
    /// `draw:control` in a cell expands to.
    checkboxes: HashMap<String, Vec<Inline>>,
    /// What the cells placed so far hold, every copy counted.
    held: Held,
}

/// The grid positions and text bytes parsed tables hold, their nested
/// tables' included.
#[derive(Clone, Copy, Default)]
pub(super) struct Held {
    positions: u64,
    bytes: u64,
}

impl Held {
    pub(super) fn add(self, other: Held) -> Held {
        Held {
            positions: self.positions.saturating_add(other.positions),
            bytes: self.bytes.saturating_add(other.bytes),
        }
    }

    fn times(self, copies: u64) -> Held {
        Held {
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
    ctx: &Ctx,
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

impl TableState<'_, '_> {
    fn charge(&mut self, cells: u64) -> Result<(), ConvertError> {
        self.expansion = self.expansion.saturating_add(cells);
        let max = self.limits.max_expansion;
        if self.expansion > max {
            return Err(ConvertError::limit(Limit::Expansion, max));
        }
        Ok(())
    }

    /// Charge text bytes that repeat expansion duplicates: the slot budget
    /// bounds positions, this bounds the amplified content itself.
    fn charge_bytes(&mut self, bytes: u64) -> Result<(), ConvertError> {
        self.expansion_bytes = self.expansion_bytes.saturating_add(bytes);
        let max = self.limits.max_expansion_text_bytes;
        if self.expansion_bytes > max {
            return Err(ConvertError::limit(Limit::ExpansionTextBytes, max));
        }
        Ok(())
    }
}

/// One parsed cell of a row template, reused across row repeats.
enum RowCell {
    Covered { repeat: u64 },
    Cell { repeat: u64, col_span: u32, row_span: u32, blocks: Vec<Block>, held: Held },
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
            Block::Table(_) | Block::Rule => 0,
        })
        .sum()
}

fn inline_bytes(inlines: &[Inline]) -> u64 {
    inlines
        .iter()
        .map(|inline| match inline {
            Inline::Text { text, style: _ } => text_bytes(text),
            Inline::Link { content, target } => inline_bytes(content) + target_bytes(target),
            Inline::Image { alt, source } => text_bytes(alt) + source_bytes(source),
            Inline::Math(tex) => text_bytes(tex),
            Inline::Anchor(id) | Inline::NoteRef(id) => text_bytes(id),
            Inline::Checkbox(_) => 3,
            Inline::LineBreak => 1,
        })
        .sum()
}

fn target_bytes(target: &LinkTarget) -> u64 {
    match target {
        LinkTarget::External(text) | LinkTarget::Relative(text) | LinkTarget::Anchor(text) => {
            text_bytes(text)
        }
    }
}

fn source_bytes(source: &ImageSource) -> u64 {
    match source {
        ImageSource::External(url) => text_bytes(url),
        ImageSource::Asset(_) | ImageSource::Unavailable => 0,
    }
}

fn optional_bytes(text: Option<&str>) -> u64 {
    text.map_or(0, text_bytes)
}

fn text_bytes(text: &str) -> u64 {
    u64::try_from(text.len()).unwrap_or(u64::MAX)
}

fn walk_rows(
    container: &Element,
    ctx: &Ctx,
    state: &mut TableState,
    top: bool,
) -> Result<(), ConvertError> {
    for child in container.child_elems() {
        if child.ns.as_deref().is_none_or(|n| n != ns::TABLE) {
            continue;
        }
        match child.local.as_str() {
            "table-header-rows" => {
                let before = state.rows_emitted;
                walk_rows(child, ctx, state, false)?;
                if top && state.header_rows == 0 {
                    state.header_rows = state.rows_emitted - before;
                }
            }
            "table-rows" | "table-row-group" => walk_rows(child, ctx, state, false)?,
            "shapes" => {
                for shape in child.child_elems() {
                    report_unplaced(shape, ctx)?;
                }
            }
            "table-row" => {
                let repeat: u64 = child
                    .attr(ns::TABLE, "number-rows-repeated")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1)
                    .max(1);
                emit_row(child, ctx, state, repeat)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn emit_row(
    row: &Element,
    ctx: &Ctx,
    state: &mut TableState,
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
    let mut cells = parse_row_cells(row, ctx, &state.checkboxes)?;
    state.charge(repeat.saturating_sub(1))?;
    for cell in &cells {
        if let RowCell::Cell { repeat: cell_repeat, held, .. } = cell {
            let placed = repeat.saturating_mul(*cell_repeat);
            let copies = placed.saturating_sub(1);
            state.charge_bytes(held.bytes.saturating_mul(copies))?;
            ctx.parse_ctx.admit_grid_positions(held.positions.saturating_mul(copies))?;
            state.held = state.held.add(held.times(placed));
        }
    }
    for row in 1..=repeat {
        state.builder.next_row();
        state.rows_emitted += 1;
        emit_parsed_cells(&mut cells, row == repeat, ctx, state)?;
    }
    Ok(())
}

/// Parse one row's cells into the reusable template.
fn parse_row_cells(
    row: &Element,
    ctx: &Ctx,
    checkboxes: &HashMap<String, Vec<Inline>>,
) -> Result<Vec<RowCell>, ConvertError> {
    let mut out = Vec::new();
    for cell in row.child_elems() {
        let repeat: u64 = cell
            .attr(ns::TABLE, "number-columns-repeated")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1)
            .max(1);
        if cell.is(ns::TABLE, "covered-table-cell") {
            out.push(RowCell::Covered { repeat });
            continue;
        }
        if !cell.is(ns::TABLE, "table-cell") {
            continue;
        }
        let col_span: u32 = cell
            .attr(ns::TABLE, "number-columns-spanned")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1)
            .max(1);
        let row_span: u32 = cell
            .attr(ns::TABLE, "number-rows-spanned")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1)
            .max(1);
        let (blocks, nested) = ctx.holding(|| cell_blocks(cell, ctx));
        let mut blocks = blocks?;
        append_cell_controls(cell, ctx, checkboxes, &mut blocks);
        let held = nested.add(Held { positions: 0, bytes: block_bytes(&blocks) });
        out.push(RowCell::Cell { repeat, col_span, row_span, blocks, held });
    }
    Ok(out)
}

/// Place a row's cells, moving each cell's content into its last copy on
/// the `last_row`.
fn emit_parsed_cells(
    cells: &mut [RowCell],
    last_row: bool,
    ctx: &Ctx,
    state: &mut TableState,
) -> Result<(), ConvertError> {
    let mut pending_cells: u64 = 0;
    let mut stray: u64 = 0;
    for cell in cells {
        match cell {
            RowCell::Covered { repeat } => {
                flush_gap(state, &mut pending_cells)?;
                // One explicitly written covered position each; a stray one
                // (no span accounts for it) becomes an empty cell inside
                // covered().
                state.charge(*repeat)?;
                for _ in 0..*repeat {
                    if !state.builder.covered()? {
                        stray = stray.saturating_add(1);
                    }
                }
            }
            RowCell::Cell { repeat, col_span, row_span, blocks, .. } => {
                if blocks.is_empty() && *col_span == 1 && *row_span == 1 {
                    pending_cells = pending_cells.saturating_add(*repeat);
                    continue;
                }
                flush_gap(state, &mut pending_cells)?;
                state.charge(repeat.saturating_mul(*col_span as u64))?;
                for copy in 1..=*repeat {
                    let content = if last_row && copy == *repeat {
                        std::mem::take(blocks)
                    } else {
                        blocks.clone()
                    };
                    state.builder.place(Cell::spanning(content, *col_span, *row_span))?;
                }
            }
        }
    }
    if stray > 0 {
        ctx.emit_count(DiagnosticCode::TableSpanRepaired, stray);
    }
    Ok(())
}

/// Materialize a buffered empty-cell run in full so the next cell lands on
/// its source column. Trailing runs are never flushed and stay elided.
fn flush_gap(state: &mut TableState, pending: &mut u64) -> Result<(), ConvertError> {
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
/// when the producer wrote no display text.
fn cell_blocks(cell: &Element, ctx: &Ctx) -> Result<Vec<Block>, ConvertError> {
    let blocks = parse_children(cell.child_elems().filter(|e| !e.is(ns::DRAW, "control")), ctx)?;
    let has_content = blocks.iter().any(|b| match b {
        Block::Paragraph(inlines) => !crate::model::inlines_are_empty(inlines),
        _ => true,
    });
    if has_content {
        return Ok(blocks);
    }
    match value_text(cell) {
        Some(text) => Ok(vec![Block::Paragraph(vec![Inline::plain(text)])]),
        None => Ok(Vec::new()),
    }
}

fn value_text(cell: &Element) -> Option<String> {
    let value_type = cell.attr(ns::OFFICE, "value-type")?;
    match value_type {
        "percentage" => {
            let v: f64 = cell.attr(ns::OFFICE, "value")?.parse().ok()?;
            Some(format!("{}%", trim_float(v * 100.0)))
        }
        "currency" => {
            let v = cell.attr(ns::OFFICE, "value")?;
            let cur = cell.attr(ns::OFFICE, "currency").unwrap_or("");
            Some(if cur.is_empty() {
                trim_float(v.parse().ok()?)
            } else {
                format!("{} {cur}", trim_float(v.parse().ok()?))
            })
        }
        "float" => Some(trim_float(cell.attr(ns::OFFICE, "value")?.parse().ok()?)),
        "date" => cell.attr(ns::OFFICE, "date-value").map(str::to_string),
        "time" => cell.attr(ns::OFFICE, "time-value").map(duration_text),
        "boolean" => cell
            .attr(ns::OFFICE, "boolean-value")
            .map(|b| if b == "true" { "TRUE" } else { "FALSE" }.to_string()),
        "string" => cell.attr(ns::OFFICE, "string-value").map(str::to_string),
        _ => None,
    }
}

/// Shortest round-trip float formatting - no fixed-precision rounding.
fn trim_float(v: f64) -> String {
    format!("{v}")
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
            'W' | 'D' if !in_time => {
                let unit = if c == 'W' { 604_800.0 } else { 86_400.0 };
                total += number.parse::<f64>().unwrap_or(0.0) * unit;
                number.clear();
            }
            'H' | 'M' | 'S' if in_time => {
                let unit = match c {
                    'H' => 3600.0,
                    'M' => 60.0,
                    _ => 1.0,
                };
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
    let total_ms = total_ms as u64;
    let sign = if negative { "-" } else { "" };
    let (hours, rest) = (total_ms / 3_600_000, total_ms % 3_600_000);
    let (minutes, ms) = (rest / 60_000, rest % 60_000);
    if ms % 1000 != 0 {
        format!("{sign}{hours}:{minutes:02}:{:06.3}", ms as f64 / 1000.0)
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
pub fn parse_spreadsheet(sheet: &Element, ctx: &Ctx) -> Result<Vec<Block>, ConvertError> {
    let tables: Vec<&Element> = sheet.child_elems().filter(|e| e.is(ns::TABLE, "table")).collect();
    let multi_sheet = tables.len() > 1;
    let mut blocks = Vec::new();
    for (position, table) in tables.into_iter().enumerate() {
        let _unit = ctx.parse_ctx.enter_unit(position);
        let named = match table.attr(ns::TABLE, "style-name") {
            Some(style) => ctx.styles.style_master_page(ctx.parse_ctx, "table", style)?,
            None => None,
        };
        let master = named
            .map_or_else(|| ctx.styles.first_master_page(), |name| ctx.styles.master_page(name));
        if let Some(master) = master {
            super::report_running(master, ctx.parse_ctx);
        }
        let name = table.attr(ns::TABLE, "name").unwrap_or("");
        let content = parse_table(table, ctx)?;
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
    use crate::model::MarkerKind;

    #[test]
    fn a_copy_of_an_image_holds_the_url_beside_its_alt_text() {
        let image = |source| Inline::Image { alt: "ab".into(), source };
        assert_eq!(inline_bytes(&[image(ImageSource::External("http://e/u".into()))]), 2 + 10);
        assert_eq!(inline_bytes(&[image(ImageSource::Unavailable)]), 2);
    }

    #[test]
    fn a_copy_of_a_heading_holds_its_anchor_beside_its_text() {
        let heading = |anchor: Option<&str>| Block::Heading {
            level: 1,
            anchor: anchor.map(str::to_string),
            content: vec![Inline::plain("text")],
        };
        assert_eq!(block_bytes(&[heading(Some("anchor"))]), 6 + 4);
        assert_eq!(block_bytes(&[heading(None)]), 4);
    }

    #[test]
    fn a_copy_of_a_code_block_holds_its_language_beside_its_text() {
        let code = |lang: Option<&str>| Block::CodeBlock {
            lang: lang.map(str::to_string),
            text: "fn".into(),
        };
        assert_eq!(block_bytes(&[code(Some("rust"))]), 4 + 2);
        assert_eq!(block_bytes(&[code(None)]), 2);
    }

    #[test]
    fn a_copy_of_a_list_holds_each_items_marker_label_and_content() {
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
    }

    #[test]
    fn a_copy_of_a_link_holds_its_target_of_any_kind_beside_its_text() {
        let link = |target| Inline::Link { content: vec![Inline::plain("ab")], target };
        for (target, len) in [
            (LinkTarget::External("http://e".into()), 8),
            (LinkTarget::Relative("../e/f".into()), 6),
            (LinkTarget::Anchor("anchor".into()), 6),
        ] {
            assert_eq!(inline_bytes(&[link(target)]), 2 + len);
        }
    }

    #[test]
    fn a_copy_of_the_remaining_content_holds_its_text() {
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
}
