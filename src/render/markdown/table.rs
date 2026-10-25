//! Table rendering over the canonical grid. Covered positions render as
//! blank cells (GFM has no span syntax); the grid invariant means the
//! renderer never synthesizes or duplicates columns.

use crate::error::ConvertError;
use crate::model::{Block, Cell, CellSlot, List, MarkerKind, Style, Table};
use crate::render::markdown::Ctx;
use crate::render::markdown::escape::{InlineContext, escape_marker_label};
use crate::render::markdown::inline::{
    push_code_span, push_math_span, render_based, render_inlines,
};
use crate::render::markdown::sink::{Meter, Sink, Text};

/// One row's cells that show text, by column, and how many columns of the row
/// show anything: a covered position counts, a blank cell does not.
struct RenderedRow<'m> {
    shown: usize,
    texts: Vec<(usize, Text<'m>)>,
}

fn render_row<'m>(
    row: &[CellSlot],
    rc: &Ctx<'_>,
    meter: &'m Meter<'m>,
) -> Result<RenderedRow<'m>, ConvertError> {
    let mut rendered = RenderedRow { shown: 0, texts: Vec::new() };
    for (col, slot) in row.iter().enumerate() {
        match slot {
            CellSlot::Origin(cell) => {
                let mut text = Text::new(meter);
                render_cell(cell, rc, &mut text)?;
                if !text.as_str().is_empty() {
                    rendered.shown = col + 1;
                    rendered.texts.push((col, text));
                }
            }
            CellSlot::Covered { .. } => rendered.shown = col + 1,
        }
    }
    Ok(rendered)
}

/// Write `table`, rendering every cell first and holding the texts until
/// written: trailing blank rows and columns are only known once all are
/// rendered, and a blank position is held as nothing.
pub(crate) fn write_table(
    table: &Table,
    rc: &Ctx<'_>,
    sink: &mut Sink<'_>,
) -> Result<(), ConvertError> {
    let meter = sink.meter();
    let mut rendered =
        table.grid.iter().map(|row| render_row(row, rc, meter)).collect::<Result<Vec<_>, _>>()?;
    while rendered.last().is_some_and(|row| row.shown == 0) {
        rendered.pop();
    }
    let width = rendered.iter().map(|row| row.shown).max().unwrap_or(0);
    if width == 0 {
        return Ok(());
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

fn write_row(row: RenderedRow<'_>, width: usize, sink: &mut Sink<'_>) -> Result<(), ConvertError> {
    sink.write("|")?;
    let mut written = 0;
    for (col, text) in row.texts {
        write_blank_cells(col - written, sink)?;
        sink.write(" ")?;
        sink.write_text(text)?;
        sink.write(" |")?;
        written = col + 1;
    }
    write_blank_cells(width.saturating_sub(written), sink)
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
pub(crate) fn render_cell(
    cell: &Cell,
    rc: &Ctx<'_>,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    out.open_cell();
    let mut parts = Join::new("<br>");
    cell.blocks.iter().try_for_each(|block| cell_block_text(block, rc, out, &mut parts))?;
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
        Self { sep, any: false }
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
    rc: &Ctx<'_>,
    out: &mut Text<'_>,
    parts: &mut Join,
) -> Result<(), ConvertError> {
    match block {
        Block::Heading { content, .. } => parts.part(out, |out, start| {
            let bold = Style { bold: true, ..Style::PLAIN };
            out.open_cell();
            render_based(content, InlineContext::TableCell, bold, rc, out)?;
            out.close_cell();
            Ok(out.len() > start)
        }),
        Block::Paragraph(inlines) => parts.part(out, |out, start| {
            render_inlines(inlines, InlineContext::TableCell, rc, out)?;
            Ok(!out.since(start).trim().is_empty())
        }),
        Block::List(list) => list_text(list, rc, out, parts),
        Block::Table(table) => table.grid.iter().try_for_each(|row| {
            // Keep empty cells in the join so values stay in their source
            // column positions.
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
            })
        }),
        Block::BlockQuote(blocks) => {
            blocks.iter().try_for_each(|block| cell_block_text(block, rc, out, parts))
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
        Block::Gap(gap) => rc
            .fills
            .filled(gap)
            .unwrap_or_default()
            .iter()
            .try_for_each(|block| cell_block_text(block, rc, out, parts)),
        Block::Rule | Block::UnitStart(_) => Ok(()),
    }
}

fn list_text(
    list: &List,
    rc: &Ctx<'_>,
    out: &mut Text<'_>,
    parts: &mut Join,
) -> Result<(), ConvertError> {
    list.items.iter().enumerate().try_for_each(|(i, item)| {
        parts.part(out, |out, _| {
            match (&item.marker_label, list.marker) {
                (Some(label), _) => {
                    escape_marker_label(label, InlineContext::TableCell, out)?;
                    out.push(' ')?;
                }
                (None, MarkerKind::Bullet) => out.push_str("• ")?,
                (None, kind) => {
                    out.push_str(&format!("{} ", kind.label(list.start.saturating_add(i as u64))))?;
                }
            }
            let mut inner = Join::new(" ");
            item.blocks.iter().try_for_each(|block| cell_block_text(block, rc, out, &mut inner))?;
            Ok(inner.any)
        })
    })
}
