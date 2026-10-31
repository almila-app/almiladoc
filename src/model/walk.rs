use super::{Block, Cell, CellSlot, Document, Gap, Inline, Table};
use crate::limits::{Limit, LimitExceeded};

/// A slice of the model one iterative walk has yet to visit.
pub(crate) enum Node<'d> {
    Blocks(&'d [Block]),
    Inlines(&'d [Inline]),
    Inline(&'d Inline),
    Cells(Cells<'d>),
}

/// The origin cells a walk has yet to visit in a table, held as the rows left
/// and the next column of the first, so that a walk's stack never grows with
/// the table's positions.
pub(crate) struct Cells<'d> {
    rows: &'d [Vec<CellSlot>],
    col: usize,
}

impl<'d> Cells<'d> {
    /// The cells of `table`, none when it holds no origin.
    fn of(table: &'d Table) -> Option<Self> {
        table.origins().next().map(|_| Self { rows: &table.grid, col: 0 })
    }
}

impl<'d> Iterator for Cells<'d> {
    type Item = &'d Cell;

    fn next(&mut self) -> Option<&'d Cell> {
        loop {
            let (row, rest) = self.rows.split_first()?;
            let Some(slot) = row.get(self.col) else {
                (self.rows, self.col) = (rest, 0);
                continue;
            };
            self.col += 1;
            if let Some(cell) = slot.origin() {
                return Some(cell);
            }
        }
    }
}

/// What `block` holds, in document order, a filled gap its fill.
pub(crate) fn children<'d>(
    block: &'d Block,
    filled: &impl Fn(&'d Gap) -> Option<&'d [Block]>,
) -> Vec<Node<'d>> {
    match block {
        Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
            vec![Node::Inlines(inlines)]
        }
        Block::List(list) => list.items.iter().map(|item| Node::Blocks(&item.blocks)).collect(),
        Block::Table(t) => Cells::of(t).map(Node::Cells).into_iter().collect(),
        Block::BlockQuote(blocks) => vec![Node::Blocks(blocks)],
        Block::HeaderFooter(value) => {
            value.content.as_deref().map(Node::Blocks).into_iter().collect()
        }
        Block::Gap(gap) => filled(gap).map(Node::Blocks).into_iter().collect(),
        Block::CodeBlock { .. } | Block::Rule | Block::Math(_) | Block::UnitStart(_) => Vec::new(),
    }
}

/// Fail [`Limit::NestingDepth`] when lists, table cells, quotes and links
/// nest deeper than `max` anywhere in `doc`, a fill in its gap's container,
/// before a recursive walk renders it; the walk itself is iterative.
pub(crate) fn check_nesting<'d>(
    doc: &'d Document,
    filled: impl Fn(&'d Gap) -> Option<&'d [Block]>,
    max: u64,
) -> Result<(), LimitExceeded> {
    let mut stack: Vec<(Node<'_>, u64)> = vec![(Node::Blocks(&doc.blocks), 0)];
    stack.extend(doc.notes.iter().map(|note| (Node::Blocks(&note.blocks), 0)));
    while let Some((node, depth)) = stack.pop() {
        if depth > max {
            return Err(LimitExceeded { limit: Limit::NestingDepth, max });
        }
        match node {
            Node::Blocks(blocks) => {
                for block in blocks {
                    let inner = match block {
                        Block::Paragraph(_) | Block::Heading { .. } | Block::Gap(_) => depth,
                        _ => depth.saturating_add(1),
                    };
                    stack.extend(children(block, &filled).into_iter().map(|child| (child, inner)));
                }
            }
            Node::Inlines(inlines) => {
                for inline in inlines {
                    if let Inline::Link { content, .. } = inline {
                        stack.push((Node::Inlines(content), depth.saturating_add(1)));
                    }
                }
            }
            Node::Cells(mut cells) => {
                if let Some(cell) = cells.next() {
                    stack.push((Node::Cells(cells), depth));
                    stack.push((Node::Blocks(&cell.blocks), depth));
                }
            }
            Node::Inline(_) => {}
        }
    }
    Ok(())
}
