//! Table rendering over the canonical grid. Covered positions render as
//! blank cells (GFM has no span syntax); the grid invariant means the
//! renderer never synthesizes or duplicates columns.

use crate::error::ConvertError;
use crate::model::{Block, Cell, CellSlot, MarkerKind, Table};
use crate::render::markdown::escape::InlineContext;
use crate::render::markdown::inline::{push_code_span, push_math_span, render_inlines};
use crate::render::markdown::sink::{Sink, Text};
use crate::render::markdown::{Ctx, escape_marker_label};

struct RenderedCell<'m> {
    text: Text<'m>,
    covered_span: bool,
}

/// Write `table`, rendering every cell first, held until written: trailing
/// blank rows and columns are only known once all are rendered.
pub(crate) fn write_table(
    table: &Table,
    rc: &Ctx,
    sink: &mut Sink<'_>,
) -> Result<(), ConvertError> {
    // Interior empty rows render as blank rows — they carry the source's
    // row coordinates; trailing blank rows are popped below.
    let meter = sink.meter();
    let mut rendered: Vec<Vec<RenderedCell<'_>>> = Vec::with_capacity(table.grid.len());
    for row in &table.grid {
        let mut cells: Vec<RenderedCell<'_>> = Vec::with_capacity(row.len());
        for slot in row {
            let mut text = Text::new(meter);
            if let CellSlot::Origin(cell) = slot {
                render_cell(cell, rc, &mut text)?;
            }
            cells.push(RenderedCell {
                text,
                covered_span: matches!(slot, CellSlot::Covered { .. }),
            });
        }
        rendered.push(cells);
    }
    while rendered.len() > 1
        && rendered
            .last()
            .is_some_and(|r| r.iter().all(|c| c.text.as_str().is_empty() && !c.covered_span))
    {
        rendered.pop();
    }
    let width = rendered
        .iter()
        .map(|r| {
            r.iter()
                .rposition(|c| !c.text.as_str().is_empty() || c.covered_span)
                .map_or(0, |i| i + 1)
        })
        .max()
        .unwrap_or(0);
    if width == 0 {
        return Ok(());
    }
    for row in &mut rendered {
        row.truncate(width);
    }

    // GFM tables always carry a delimiter row, so a table with no header row
    // of its own renders an empty one above the data.
    let mut rows = rendered.into_iter();
    let header = if table.header_rows >= 1 { rows.next() } else { None };
    if let Some(header) = header {
        write_row(header, width, sink)?;
    } else {
        sink.write("|")?;
        write_blank_cells(width, sink)?;
    }
    sink.write("\n|")?;
    for _ in 0..width {
        sink.write(" --- |")?;
    }
    for row in rows {
        sink.write("\n")?;
        write_row(row, width, sink)?;
    }
    Ok(())
}

fn write_row(
    cells: Vec<RenderedCell<'_>>,
    width: usize,
    sink: &mut Sink<'_>,
) -> Result<(), ConvertError> {
    let blanks = width.saturating_sub(cells.len());
    sink.write("|")?;
    for cell in cells {
        sink.write(" ")?;
        sink.write_text(cell.text)?;
        sink.write(" |")?;
    }
    write_blank_cells(blanks, sink)
}

fn write_blank_cells(count: usize, sink: &mut Sink<'_>) -> Result<(), ConvertError> {
    const BLANK: &str = "  |";
    const CHUNK: &str = "  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |";
    const PER_CHUNK: usize = CHUNK.len() / BLANK.len();
    for _ in 0..count / PER_CHUNK {
        sink.write(CHUNK)?;
    }
    sink.write(CHUNK.get(..count % PER_CHUNK * BLANK.len()).unwrap_or_default())
}

/// Flatten arbitrary block content into a single table-cell line in `out`:
/// its parts joined by `<br>`, each line trimmed, blank lines dropped and
/// the rest joined by `<br>` as `out` takes them. A cell's edge whitespace
/// is padding the table's own padding would swallow anyway, and
/// spreadsheets carry it by the thousand.
pub(crate) fn render_cell(cell: &Cell, rc: &Ctx, out: &mut Text<'_>) -> Result<(), ConvertError> {
    out.open_cell();
    let mut parts = Join::new("<br>");
    for block in &cell.blocks {
        cell_block_text(block, rc, out, &mut parts)?;
    }
    out.close_cell();
    Ok(())
}

/// Parts rendered in place into one text, `sep` between those kept.
struct Join {
    sep: &'static str,
    any: bool,
}

impl Join {
    fn new(sep: &'static str) -> Self {
        Join { sep, any: false }
    }

    /// Render one part after the separator it needs; `render` reports
    /// whether it keeps what it rendered since the mark it is given, and a
    /// part not kept is taken back with its separator.
    fn part(
        &mut self,
        out: &mut Text<'_>,
        render: impl FnOnce(&mut Text<'_>, usize) -> Result<bool, ConvertError>,
    ) -> Result<(), ConvertError> {
        let (any, sep) = (self.any, self.sep);
        let kept = out.attempt(|out| {
            if any {
                out.push_str(sep)?;
            }
            let start = out.len();
            render(out, start)
        })?;
        self.any |= kept;
        Ok(())
    }
}

fn cell_block_text(
    block: &Block,
    rc: &Ctx,
    out: &mut Text<'_>,
    parts: &mut Join,
) -> Result<(), ConvertError> {
    match block {
        Block::Heading { content, .. } => parts.part(out, |out, _| {
            out.push_str("**")?;
            let text = out.len();
            out.open_cell();
            render_inlines(content, InlineContext::TableCell, rc, out)?;
            out.close_cell();
            if out.len() == text {
                return Ok(false);
            }
            out.push_str("**")?;
            Ok(true)
        }),
        Block::Paragraph(inlines) => parts.part(out, |out, start| {
            render_inlines(inlines, InlineContext::TableCell, rc, out)?;
            Ok(!out.since(start).trim().is_empty())
        }),
        Block::List(list) => {
            for (i, item) in list.items.iter().enumerate() {
                parts.part(out, |out, _| {
                    match (&item.marker_label, list.marker) {
                        (Some(label), _) => {
                            escape_marker_label(label, InlineContext::TableCell, out)?;
                            out.push(' ')?;
                        }
                        (None, MarkerKind::Bullet) => out.push_str("• ")?,
                        (None, kind) => out.push_str(&format!(
                            "{} ",
                            kind.label(list.start.saturating_add(i as u64))
                        ))?,
                    }
                    let mut inner = Join::new(" ");
                    for b in &item.blocks {
                        cell_block_text(b, rc, out, &mut inner)?;
                    }
                    Ok(inner.any)
                })?;
            }
            Ok(())
        }
        Block::Table(table) => {
            // Keep empty cells in the join so values stay in their source
            // column positions.
            for row in &table.grid {
                parts.part(out, |out, _| {
                    let mut any = false;
                    for (i, slot) in row.iter().enumerate() {
                        if i > 0 {
                            out.push_str(" / ")?;
                        }
                        if let CellSlot::Origin(cell) = slot {
                            let mark = out.len();
                            render_cell(cell, rc, out)?;
                            any |= out.len() > mark;
                        }
                    }
                    Ok(any)
                })?;
            }
            Ok(())
        }
        Block::BlockQuote(blocks) => {
            for b in blocks {
                cell_block_text(b, rc, out, parts)?;
            }
            Ok(())
        }
        Block::CodeBlock { text, .. } => {
            let t = text.trim();
            if t.is_empty() {
                return Ok(());
            }
            parts
                .part(out, |out, _| push_code_span(t, InlineContext::TableCell, out).map(|()| true))
        }
        Block::Math(tex) => {
            if tex.trim().is_empty() {
                return Ok(());
            }
            parts.part(out, |out, _| {
                push_math_span(tex, InlineContext::TableCell, out).map(|()| true)
            })
        }
        Block::Rule => Ok(()),
    }
}
