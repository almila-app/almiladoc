use super::{
    AssetId, Block, Cell, CellSlot, Document, Gap, ImageSource, Inline, ListItem, OcrOrigin, Table,
};
use crate::limits::{Limit, LimitExceeded};
use std::slice::IterMut;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetPhase {
    Main,
    Side,
}

enum AssetNode<'d> {
    Blocks(IterMut<'d, Block>, bool),
    Inlines(IterMut<'d, Inline>),
    Items(IterMut<'d, ListItem>, bool),
    Rows(IterMut<'d, Vec<CellSlot>>, bool),
    Slots(IterMut<'d, CellSlot>, bool),
}

pub(crate) fn visit_asset_ids<E: From<LimitExceeded>>(
    document: &mut Document,
    phase: AssetPhase,
    mut charge: impl FnMut(u64) -> Result<(), E>,
    mut visit: impl FnMut(&mut AssetId) -> Result<(), E>,
) -> Result<(), E> {
    asset_blocks(&mut document.blocks, phase, &mut charge, &mut visit)?;
    for note in &mut document.notes {
        charge(1)?;
        asset_blocks(&mut note.blocks, phase, &mut charge, &mut visit)?;
    }
    Ok(())
}

fn asset_blocks<E: From<LimitExceeded>>(
    blocks: &mut [Block],
    phase: AssetPhase,
    charge: &mut impl FnMut(u64) -> Result<(), E>,
    visit: &mut impl FnMut(&mut AssetId) -> Result<(), E>,
) -> Result<(), E> {
    let mut pending = Vec::new();
    push_asset_node(&mut pending, AssetNode::Blocks(blocks.iter_mut(), false), charge)?;
    while let Some(node) = pending.pop() {
        charge(1)?;
        asset_node(node, phase, &mut pending, charge, visit)?;
    }
    Ok(())
}

fn asset_node<'d, E: From<LimitExceeded>>(
    node: AssetNode<'d>,
    phase: AssetPhase,
    pending: &mut Vec<AssetNode<'d>>,
    charge: &mut impl FnMut(u64) -> Result<(), E>,
    visit: &mut impl FnMut(&mut AssetId) -> Result<(), E>,
) -> Result<(), E> {
    match node {
        AssetNode::Blocks(mut blocks, side) => {
            if let Some(block) = blocks.next() {
                push_asset_node(pending, AssetNode::Blocks(blocks, side), charge)?;
                asset_block(block, phase, side, pending, charge, visit)?;
            }
        }
        AssetNode::Inlines(mut inlines) => {
            if let Some(inline) = inlines.next() {
                push_asset_node(pending, AssetNode::Inlines(inlines), charge)?;
                match inline {
                    Inline::Image { source: ImageSource::Asset(id), .. } => visit(id)?,
                    Inline::Link { content, .. } => {
                        push_asset_node(pending, AssetNode::Inlines(content.iter_mut()), charge)?;
                    }
                    _ => {}
                }
            }
        }
        AssetNode::Items(mut items, side) => {
            if let Some(item) = items.next() {
                push_asset_node(pending, AssetNode::Items(items, side), charge)?;
                push_asset_node(pending, AssetNode::Blocks(item.blocks.iter_mut(), side), charge)?;
            }
        }
        AssetNode::Rows(mut rows, side) => {
            if let Some(row) = rows.next() {
                push_asset_node(pending, AssetNode::Rows(rows, side), charge)?;
                push_asset_node(pending, AssetNode::Slots(row.iter_mut(), side), charge)?;
            }
        }
        AssetNode::Slots(mut slots, side) => {
            if let Some(slot) = slots.next() {
                push_asset_node(pending, AssetNode::Slots(slots, side), charge)?;
                if let CellSlot::Origin(cell) = slot {
                    push_asset_node(
                        pending,
                        AssetNode::Blocks(cell.blocks.iter_mut(), side),
                        charge,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn asset_block<'d, E: From<LimitExceeded>>(
    block: &'d mut Block,
    phase: AssetPhase,
    side: bool,
    pending: &mut Vec<AssetNode<'d>>,
    charge: &mut impl FnMut(u64) -> Result<(), E>,
    visit: &mut impl FnMut(&mut AssetId) -> Result<(), E>,
) -> Result<(), E> {
    let active = (phase == AssetPhase::Side) == side;
    match block {
        Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } if active => {
            push_asset_node(pending, AssetNode::Inlines(inlines.iter_mut()), charge)?;
        }
        Block::List(list) => {
            push_asset_node(pending, AssetNode::Items(list.items.iter_mut(), side), charge)?;
        }
        Block::Table(table) => {
            push_asset_node(pending, AssetNode::Rows(table.grid.iter_mut(), side), charge)?;
        }
        Block::BlockQuote(blocks) => {
            push_asset_node(pending, AssetNode::Blocks(blocks.iter_mut(), side), charge)?;
        }
        Block::HeaderFooter(value) if phase == AssetPhase::Side => {
            if let Some(content) = &mut value.content {
                push_asset_node(pending, AssetNode::Blocks(content.iter_mut(), true), charge)?;
            }
        }
        Block::Gap(Gap { origin: OcrOrigin::Image(id), .. }) if active => visit(id)?,
        Block::Paragraph(_)
        | Block::Heading { .. }
        | Block::HeaderFooter(_)
        | Block::Gap(_)
        | Block::CodeBlock { .. }
        | Block::Rule
        | Block::Math(_)
        | Block::UnitStart(_) => {}
    }
    Ok(())
}

fn push_asset_node<'d, E: From<LimitExceeded>>(
    pending: &mut Vec<AssetNode<'d>>,
    node: AssetNode<'d>,
    charge: &mut impl FnMut(u64) -> Result<(), E>,
) -> Result<(), E> {
    charge(1)?;
    pending.try_reserve(1).map_err(|_| LimitExceeded { limit: Limit::Allocation, max: 0 })?;
    pending.push(node);
    Ok(())
}

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
