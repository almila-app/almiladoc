//! GitHub-Flavored Markdown serializer for the document model.

mod anchors;
mod escape;
mod inline;
mod shape;
mod sink;
mod table;

#[cfg(test)]
mod tests;

use crate::diagnostics::{DiagnosticCode, Diagnostics, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{
    Block, Cell, CellSlot, Document, Inline, LinkTarget, List, MarkerKind, Note, Table, TableKind,
    inlines_are_empty,
};
use anchors::{AnchorMap, resolve_anchors};
use escape::{EscapeOpts, InlineContext, backtick_fence, escape_text};
use inline::{LinkMemo, render_inlines};
use shape::Shapes;
pub(crate) use sink::Meter;
use sink::{Sink, Text};
use std::collections::{HashMap, HashSet};

/// Escape a source-derived composite marker label for literal use into
/// `out`: control characters collapse to spaces and Markdown syntax is
/// neutralized so a crafted label cannot alter document structure.
pub(crate) fn escape_marker_label(
    label: &str,
    ctx: InlineContext,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let cleaned: String = label.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let opts = EscapeOpts {
        // List-item content re-opens block syntax after the `- ` marker.
        at_line_start: ctx == InlineContext::Block,
        trailing_active: true,
        ..Default::default()
    };
    escape_text(&cleaned, ctx, opts, out)
}

/// Footnote id -> rendered number, shared by all render functions.
type NoteNumbers = HashMap<String, usize>;

/// Render context threaded through every render function.
pub(crate) struct Ctx {
    nums: NoteNumbers,
    anchors: AnchorMap,
    links: LinkMemo,
    shapes: Shapes,
}

impl Ctx {
    fn of(doc: &Document) -> Self {
        Ctx {
            nums: number_notes(doc),
            anchors: resolve_anchors(doc),
            links: LinkMemo::default(),
            shapes: Shapes::default(),
        }
    }
}

/// Render `doc` as GitHub-Flavored Markdown, every byte charged to `meter`.
pub(crate) fn render(doc: &Document, meter: &Meter<'_>) -> Result<String, ConvertError> {
    let rc = Ctx::of(doc);
    let mut sink = Sink::new(meter);
    write_blocks(&doc.blocks, &rc, &mut sink)?;
    let mut defined: HashSet<usize> = HashSet::new();
    for (note, num) in numbered_notes(doc, &rc) {
        if defined.contains(&num) {
            continue;
        }
        if sink.len() > 0 {
            sink.gap(2);
        }
        let mut label = Text::new(meter);
        label.push_str(&format!("[^{num}]: "))?;
        sink.push_marker(label, 4);
        let before = sink.len();
        write_blocks(&note.blocks, &rc, &mut sink)?;
        sink.pop();
        if sink.len() > before {
            defined.insert(num);
        }
    }
    sink.finish()
}

#[cfg(test)]
pub(crate) fn document_to_markdown(doc: &Document) -> String {
    let ledger = crate::ledger::Ledger::new(u64::MAX, None);
    render(doc, &Meter::new(&ledger, u64::MAX)).expect("an unlimited render succeeds")
}

/// Which causes the parse already counted, silencing the render losses they
/// cover.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Unread {
    /// A notes part did not read, so references to its notes dangle.
    pub(crate) notes: bool,
    /// A whole unit did not read, so links to its anchors dangle.
    pub(crate) units: bool,
}

impl Unread {
    /// The causes `diagnostics` counts: any `part_unreadable` in Note, any
    /// `unit_unreadable`.
    pub(crate) fn of(diagnostics: &Diagnostics) -> Self {
        Unread {
            notes: diagnostics
                .iter()
                .any(|d| d.code == DiagnosticCode::PartUnreadable && d.region == Region::Note),
            units: diagnostics.iter().any(|d| d.code == DiagnosticCode::UnitUnreadable),
        }
    }
}

/// One loss rendering reports: its code, region and count.
pub(crate) type RenderLoss = (DiagnosticCode, Region, u64);

/// What rendering `doc` loses, by code, region and count: note bodies under
/// an id already defined, and links to anchors and note references whose
/// target exists nowhere, unless `unread` names their cause. Read from the
/// model alone: nothing is rendered.
pub(crate) fn render_losses(doc: &Document, unread: Unread) -> Vec<RenderLoss> {
    let rc = Ctx::of(doc);
    let mut numbers: HashSet<usize> = HashSet::new();
    let mut duplicates: u64 = 0;
    let mut notes = Dangling::default();
    for (note, num) in numbered_notes(doc, &rc) {
        if !note.blocks.iter().any(|b| shape::shows(b, &rc)) {
            continue;
        }
        if numbers.insert(num) {
            visit_inlines(&note.blocks, &mut |inline| notes.tally(inline, &rc));
        } else {
            duplicates = duplicates.saturating_add(1);
        }
    }
    let mut body = Dangling::default();
    visit_inlines(&doc.blocks, &mut |inline| body.tally(inline, &rc));
    let dangling = |found: Dangling| {
        let links = if unread.units { 0 } else { found.links };
        links.saturating_add(if unread.notes { 0 } else { found.note_refs })
    };
    [
        (DiagnosticCode::DuplicateNote, Region::Note, duplicates),
        (DiagnosticCode::DanglingReference, Region::Body, dangling(body)),
        (DiagnosticCode::DanglingReference, Region::Note, dangling(notes)),
    ]
    .into_iter()
    .filter(|&(_, _, count)| count > 0)
    .collect()
}

#[derive(Default)]
struct Dangling {
    links: u64,
    note_refs: u64,
}

impl Dangling {
    fn tally(&mut self, inline: &Inline, rc: &Ctx) {
        match inline {
            Inline::Link { target: LinkTarget::Anchor(id), .. }
                if !id.is_empty() && rc.anchors.fragment(id).is_none() =>
            {
                self.links = self.links.saturating_add(1);
            }
            Inline::NoteRef(id) if !rc.nums.contains_key(id) => {
                self.note_refs = self.note_refs.saturating_add(1);
            }
            _ => {}
        }
    }
}

fn visit_inlines(blocks: &[Block], visit: &mut impl FnMut(&Inline)) {
    fn walk(inlines: &[Inline], visit: &mut impl FnMut(&Inline)) {
        for inline in inlines {
            visit(inline);
            if let Inline::Link { content, .. } = inline {
                walk(content, visit);
            }
        }
    }
    for block in blocks {
        match block {
            Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
                walk(inlines, visit)
            }
            Block::List(list) => {
                for item in &list.items {
                    visit_inlines(&item.blocks, visit);
                }
            }
            Block::Table(t) => {
                for row in &t.grid {
                    for slot in row {
                        if let crate::model::CellSlot::Origin(cell) = slot {
                            visit_inlines(&cell.blocks, visit);
                        }
                    }
                }
            }
            Block::BlockQuote(blocks) => visit_inlines(blocks, visit),
            Block::CodeBlock { .. } | Block::Rule | Block::Math(_) => {}
        }
    }
}

/// The notes that have a number, in number order and, under one number, in
/// document order: the first whose body shows anything is its definition.
fn numbered_notes<'d>(doc: &'d Document, rc: &Ctx) -> Vec<(&'d Note, usize)> {
    let mut ordered: Vec<(&Note, usize)> =
        doc.notes.iter().filter_map(|n| rc.nums.get(&n.id).map(|&num| (n, num))).collect();
    ordered.sort_by_key(|(_, num)| *num);
    ordered
}

/// Number notes in first-reference order; unreferenced notes follow at the
/// end. The first note wins a duplicated id.
fn number_notes(doc: &Document) -> NoteNumbers {
    let mut valid: HashMap<&str, &Note> = HashMap::new();
    for note in &doc.notes {
        if !note.blocks.iter().all(block_is_blank) {
            valid.entry(note.id.as_str()).or_insert(note);
        }
    }
    let mut order: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    collect_note_refs(&doc.blocks, &valid, &mut order, &mut seen);
    for note in &doc.notes {
        if valid.contains_key(note.id.as_str()) && seen.insert(note.id.clone()) {
            order.push(note.id.clone());
        }
    }
    order.into_iter().enumerate().map(|(i, id)| (id, i + 1)).collect()
}

fn block_is_blank(block: &Block) -> bool {
    match block {
        Block::Paragraph(inlines) => inlines_are_empty(inlines),
        _ => false,
    }
}

/// Push every note `blocks` reference in first-reference order, each note's
/// own references right after it, with an explicit stack: a chain of notes
/// referencing the next is as long as the document's note count.
fn collect_note_refs(
    blocks: &[Block],
    valid: &HashMap<&str, &Note>,
    order: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    let mut stack: Vec<Node<'_>> = vec![Node::Blocks(blocks)];
    while let Some(node) = stack.pop() {
        match node {
            Node::Inlines(inlines) => {
                for inline in inlines.iter().rev() {
                    stack.push(Node::Inline(inline));
                }
            }
            Node::Inline(Inline::NoteRef(id)) => {
                if let Some(note) = valid.get(id.as_str())
                    && seen.insert(id.clone())
                {
                    order.push(id.clone());
                    stack.push(Node::Blocks(&note.blocks));
                }
            }
            Node::Inline(Inline::Link { content, .. }) => stack.push(Node::Inlines(content)),
            Node::Inline(_) => {}
            Node::Blocks(blocks) => {
                for block in blocks.iter().rev() {
                    stack.extend(children(block).into_iter().rev());
                }
            }
        }
    }
}

/// A slice of the model one iterative walk has yet to visit.
#[derive(Clone, Copy)]
enum Node<'d> {
    Blocks(&'d [Block]),
    Inlines(&'d [Inline]),
    Inline(&'d Inline),
}

/// What `block` holds, in document order.
fn children(block: &Block) -> Vec<Node<'_>> {
    match block {
        Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
            vec![Node::Inlines(inlines)]
        }
        Block::List(list) => list.items.iter().map(|item| Node::Blocks(&item.blocks)).collect(),
        Block::Table(t) => t
            .grid
            .iter()
            .flatten()
            .filter_map(|slot| match slot {
                crate::model::CellSlot::Origin(cell) => Some(Node::Blocks(&cell.blocks)),
                crate::model::CellSlot::Covered { .. } => None,
            })
            .collect(),
        Block::BlockQuote(blocks) => vec![Node::Blocks(blocks)],
        Block::CodeBlock { .. } | Block::Rule | Block::Math(_) => Vec::new(),
    }
}

/// Fail [`Limit::NestingDepth`] when lists, table cells, quotes and links
/// nest deeper than `max` anywhere in `doc`, before a recursive walk renders
/// it; the walk itself is iterative.
pub(crate) fn check_nesting(doc: &Document, max: u64) -> Result<(), ConvertError> {
    let mut stack: Vec<(Node<'_>, u64)> = vec![(Node::Blocks(&doc.blocks), 0)];
    stack.extend(doc.notes.iter().map(|note| (Node::Blocks(&note.blocks), 0)));
    while let Some((node, depth)) = stack.pop() {
        if depth > max {
            return Err(ConvertError::limit(Limit::NestingDepth, max));
        }
        match node {
            Node::Blocks(blocks) => {
                for block in blocks {
                    let inner = match block {
                        Block::Paragraph(_) | Block::Heading { .. } => depth,
                        _ => depth.saturating_add(1),
                    };
                    stack.extend(children(block).into_iter().map(|child| (child, inner)));
                }
            }
            Node::Inlines(inlines) => {
                for inline in inlines {
                    if let Inline::Link { content, .. } = inline {
                        stack.push((Node::Inlines(content), depth.saturating_add(1)));
                    }
                }
            }
            Node::Inline(_) => {}
        }
    }
    Ok(())
}

/// Write `blocks` separated by empty lines, skipping those that write
/// nothing.
fn write_blocks(blocks: &[Block], rc: &Ctx, sink: &mut Sink<'_>) -> Result<(), ConvertError> {
    let start = sink.len();
    for block in blocks {
        if sink.len() > start {
            sink.gap(2);
        }
        write_block(block, rc, sink)?;
    }
    if sink.len() > start {
        sink.drop_gap();
    }
    Ok(())
}

/// The one cell of a trivial layout table, which is scaffolding: its content
/// renders directly.
fn layout_cell(table: &Table) -> Option<&Cell> {
    if table.kind != TableKind::Layout || !table.is_single_cell() {
        return None;
    }
    match table.grid.first().and_then(|row| row.first()) {
        Some(CellSlot::Origin(cell)) => Some(cell),
        _ => None,
    }
}

fn write_block(block: &Block, rc: &Ctx, sink: &mut Sink<'_>) -> Result<(), ConvertError> {
    match block {
        Block::Heading { level, content, .. } => {
            let mut text = Text::new(sink.meter());
            render_inlines(content, InlineContext::Heading, rc, &mut text)?;
            let text = text.into_string();
            let text = text.trim();
            if text.is_empty() {
                return Ok(());
            }
            let level = (*level).clamp(1, 6) as usize;
            sink.write(&format!("{} ", "#".repeat(level)))?;
            sink.write(text)
        }
        Block::Paragraph(inlines) => {
            let mut text = Text::new(sink.meter());
            render_inlines(inlines, InlineContext::Block, rc, &mut text)?;
            write_paragraph(&text.into_string(), sink)
        }
        Block::List(list) => write_list(list, rc, sink),
        Block::Table(t) => match layout_cell(t) {
            Some(cell) => write_blocks(&cell.blocks, rc, sink),
            None => table::write_table(t, rc, sink),
        },
        Block::BlockQuote(blocks) => {
            sink.push_quote();
            write_blocks(blocks, rc, sink)?;
            sink.pop();
            Ok(())
        }
        Block::CodeBlock { lang, text } => {
            let fence = backtick_fence(text, 3);
            sink.write(&fence)?;
            sink.write(lang.as_deref().unwrap_or(""))?;
            sink.write("\n")?;
            sink.write(text.trim_end_matches('\n'))?;
            sink.write("\n")?;
            sink.write(&fence)
        }
        Block::Rule => sink.write("---"),
        Block::Math(tex) => {
            let tex = tex.trim();
            if tex.is_empty() {
                return Ok(());
            }
            // A bare `$` is never valid inside math; escaped, it cannot
            // close the block early.
            sink.write("$$\n")?;
            let mut flushed = 0;
            let mut backslashes = 0;
            for (i, c) in tex.char_indices() {
                if c == '$' && backslashes % 2 == 0 {
                    sink.write(tex.get(flushed..i).unwrap_or_default())?;
                    sink.write("\\")?;
                    flushed = i;
                }
                backslashes = if c == '\\' { backslashes + 1 } else { 0 };
            }
            sink.write(tex.get(flushed..).unwrap_or_default())?;
            sink.write("\n$$")
        }
    }
}

fn write_list(list: &List, rc: &Ctx, sink: &mut Sink<'_>) -> Result<(), ConvertError> {
    let loose = shape::loose(list, rc, sink.meter())?;
    for (i, item) in list.items.iter().enumerate() {
        if i > 0 {
            sink.gap(if loose { 2 } else { 1 });
        }
        // GFM has decimal ordered lists only, so Roman/alphabetic levels
        // render as bullets carrying the source marker as literal text
        // (`- iv. …`) — the source marker semantics stay visible. Items with
        // an explicit label (composite number text) render it the same way.
        let mut marker = Text::new(sink.meter());
        match (&item.marker_label, list.marker) {
            (Some(label), _) => {
                marker.push_str("- ")?;
                escape_marker_label(label, InlineContext::Block, &mut marker)?;
                marker.push(' ')?;
            }
            (None, MarkerKind::Bullet) => marker.push_str("- ")?,
            (None, MarkerKind::Decimal) => {
                marker.push_str(&format!("{}. ", list.start.saturating_add(i as u64)))?;
            }
            (None, kind) => {
                marker
                    .push_str(&format!("- {} ", kind.label(list.start.saturating_add(i as u64))))?;
            }
        }
        let indent = marker.as_str().chars().count();
        sink.push_marker(marker, indent);
        write_blocks(&item.blocks, rc, sink)?;
        sink.pop_shown()?;
    }
    Ok(())
}

/// Write a paragraph's rendered `text` with its lines trimmed, keeping
/// hard-break backslashes intact.
fn write_paragraph(text: &str, sink: &mut Sink<'_>) -> Result<(), ConvertError> {
    let lines: Vec<&str> = text
        .lines()
        .map(|l| {
            let t = l.trim_start();
            let t = if ends_with_hard_break(t) { t } else { t.trim_end() };
            if t.trim_end_matches('\\').trim().is_empty() { "" } else { t }
        })
        .collect();
    let start = lines.iter().position(|l| !l.is_empty());
    let end = lines.iter().rposition(|l| !l.is_empty());
    let (Some(start), Some(end)) = (start, end) else {
        return Ok(());
    };
    for line in lines.iter().take(end).skip(start) {
        sink.write(line)?;
        sink.write("\n")?;
    }
    let last = lines.get(end).copied().unwrap_or_default();
    match last.strip_suffix('\\') {
        Some(kept) if ends_with_hard_break(last) => sink.write(kept.trim_end()),
        _ => sink.write(last),
    }
}

fn ends_with_hard_break(line: &str) -> bool {
    line.chars().rev().take_while(|&c| c == '\\').count() % 2 == 1
}

#[cfg(test)]
fn render_block(block: &Block, rc: &Ctx) -> Option<String> {
    let ledger = crate::ledger::Ledger::new(u64::MAX, None);
    let meter = Meter::new(&ledger, u64::MAX);
    let mut sink = Sink::new(&meter);
    write_block(block, rc, &mut sink).ok()?;
    sink.finish().ok().filter(|markdown| !markdown.is_empty())
}
