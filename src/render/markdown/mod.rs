//! GitHub-Flavored Markdown serializer for the document model.

mod anchors;
mod escape;
mod image;
mod inline;
mod marks;
mod shape;
mod sink;
mod table;

#[cfg(test)]
#[expect(clippy::format_collect, reason = "fixture text is built with format!")]
mod tests;

use crate::diagnostics::{DiagnosticCode, Diagnostics, Region};
use crate::error::ConvertError;
use crate::model::Node;
use crate::model::{
    Block, Cell, Document, Gap, Inline, LinkTarget, List, MarkerKind, Note, OcrOrigin, Table, Unit,
    UnitKind, inlines_are_empty,
};
use crate::ocr::{GapFills, GapState};
use crate::shared::uri::is_safe_link;
use crate::{ImagePolicy, UnitSpan};
use anchors::{AnchorMap, resolve_anchors};
use escape::{InlineContext, MathSite, backtick_fence, escape_marker_label, escape_math};
use inline::render_inlines;
use shape::Shapes;
pub(crate) use sink::Meter;
use sink::{Sink, Text};
use std::borrow::Cow;
use std::cell::Cell as StateCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Footnote id -> rendered number, shared by all render functions.
type NoteNumbers = HashMap<String, usize>;

pub(crate) struct Ctx<'d> {
    preserve_spans: StateCell<bool>,
    fills: &'d GapFills,
    nums: NoteNumbers,
    anchors: AnchorMap,
    shapes: Shapes,
    images: ImagePolicy,
    selected_notes: Vec<&'d Note>,
    defined_notes: HashSet<&'d str>,
    omitted_notes: HashSet<&'d str>,
    duplicate_notes: u64,
    side_index: StateCell<usize>,
}

impl<'d> Ctx<'d> {
    fn of(
        doc: &'d Document,
        fills: &'d GapFills,
        images: ImagePolicy,
        meter: &Meter<'_>,
    ) -> Result<Self, ConvertError> {
        let mut defined_notes = HashSet::new();
        let selected_notes: Vec<&Note> =
            doc.notes.iter().filter(|note| defined_notes.insert(note.id.as_str())).collect();
        let duplicate_notes =
            u64::try_from(doc.notes.len() - selected_notes.len()).unwrap_or(u64::MAX);
        let mut ctx = Self::empty(fills, images, 0);
        ctx.selected_notes = selected_notes;
        ctx.defined_notes = defined_notes;
        ctx.duplicate_notes = duplicate_notes;
        if images == ImagePolicy::Omit {
            ctx.omit_image_notes(meter)?;
        }
        ctx.nums = number_notes(&doc.blocks, &ctx.selected_notes, &ctx.omitted_notes, fills);
        let notes: Vec<&Note> = numbered_notes(&ctx).into_iter().map(|(note, _)| note).collect();
        ctx.anchors = resolve_anchors(doc, fills, &notes, images, &ctx.nums);
        Ok(ctx)
    }

    fn empty(fills: &'d GapFills, images: ImagePolicy, side_index: usize) -> Self {
        Self {
            preserve_spans: StateCell::new(false),
            fills,
            nums: HashMap::new(),
            anchors: AnchorMap::default(),
            shapes: Shapes::default(),
            images,
            selected_notes: Vec::new(),
            defined_notes: HashSet::new(),
            omitted_notes: HashSet::new(),
            duplicate_notes: 0,
            side_index: StateCell::new(side_index),
        }
    }

    fn omit_image_notes(&mut self, meter: &Meter<'_>) -> Result<(), ConvertError> {
        let count = self.selected_notes.len();
        let valid: HashMap<&str, usize> = self
            .selected_notes
            .iter()
            .enumerate()
            .filter(|(_, note)| !note.blocks.iter().all(block_is_blank))
            .map(|(i, note)| (note.id.as_str(), i))
            .collect();
        let mut base = vec![false; count];
        let mut images = vec![false; count];
        let mut refs = vec![0_usize; count];
        let mut reverse = vec![Vec::new(); count];
        for (i, note) in self.selected_notes.iter().enumerate() {
            base[i] = shape::any(&note.blocks, |block| shape::shows(block, self, meter))?;
            visit_inlines(&note.blocks, self, &mut |inline, _| match inline {
                Inline::Image { .. } => images[i] = true,
                Inline::NoteRef(id) => {
                    if let Some(&target) = valid.get(id.as_str()) {
                        refs[i] += 1;
                        reverse[target].push(i);
                    }
                }
                _ => {}
            });
        }
        let mut pending: Vec<usize> =
            (0..count).filter(|&i| images[i] && !base[i] && refs[i] == 0).collect();
        while let Some(i) = pending.pop() {
            if !self.omitted_notes.insert(self.selected_notes[i].id.as_str()) {
                continue;
            }
            for &dependent in &reverse[i] {
                refs[dependent] -= 1;
                if !base[dependent] && refs[dependent] == 0 {
                    pending.push(dependent);
                }
            }
        }
        self.shapes = Shapes::default();
        Ok(())
    }

    /// Whether a link to `target` renders its content alone: an anchor that exists nowhere, or a
    /// destination the scheme gate refuses.
    fn degrades(&self, target: &LinkTarget) -> bool {
        match target {
            LinkTarget::Anchor { id, .. } => self.anchors.fragment(id).is_none(),
            LinkTarget::Unit(unit) => self.anchors.unit_fragment(*unit).is_none(),
            LinkTarget::External(url) | LinkTarget::Relative(url) => !is_safe_link(url),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Rendered {
    pub(crate) markdown: String,
    pub(crate) units: Vec<UnitSpan>,
}

/// Render `doc` with its gaps as `fills` holds them as GitHub-Flavored Markdown, every byte
/// charged to `meter`.
pub(crate) fn render(
    doc: &Document,
    fills: &GapFills,
    meter: &Meter<'_>,
    images: ImagePolicy,
) -> Result<Rendered, ConvertError> {
    let rc = Ctx::of(doc, fills, images, meter)?;
    let mut sink = Sink::new(meter);
    let units = write_body(&doc.blocks, &rc, &mut sink)?;
    for (note, num) in numbered_notes(&rc) {
        if sink.len() > 0 {
            sink.gap(2);
        }
        let mut label = Text::new(meter);
        label.push_str(&format!("[^{num}]: "))?;
        sink.push_marker(label, 4);
        write_blocks(&note.blocks, &rc, &mut sink, None)?;
        sink.pop();
    }
    Ok(Rendered { markdown: sink.finish()?, units })
}

#[cfg(test)]
pub(crate) fn document_to_markdown(doc: &Document) -> String {
    let ledger = crate::ledger::Ledger::new(u64::MAX, None);
    render(doc, &GapFills::default(), &Meter::new(&ledger, u64::MAX), ImagePolicy::Placeholder)
        .expect("an unlimited render succeeds")
        .markdown
}

/// Which causes the parse already counted, silencing the render losses they
/// cover.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Unread<'a> {
    /// A notes part did not read, so references to its notes dangle.
    pub(crate) notes: bool,
    targets: Option<&'a BTreeSet<Unit>>,
}

impl<'a> Unread<'a> {
    /// Whether `diagnostics` already counts an unreadable notes part.
    pub(crate) fn of(diagnostics: &Diagnostics) -> Self {
        Self {
            notes: diagnostics
                .iter()
                .any(|d| d.code == DiagnosticCode::PartUnreadable && d.region == Region::Note),
            targets: None,
        }
    }
    pub(crate) fn with_targets(mut self, targets: &'a BTreeSet<Unit>) -> Self {
        self.targets = Some(targets);
        self
    }
    fn contains(self, unit: Unit) -> bool {
        self.targets.is_some_and(|targets| targets.contains(&unit))
    }
}

/// One loss rendering reports: its code, region and count.
pub(crate) type RenderLoss = (DiagnosticCode, Region, Option<Unit>, u64);

/// What rendering `doc` loses, by code, region and count: note bodies under
/// an id already defined, and links to anchors and note references whose
/// target exists nowhere, unless `unread` names their cause. Read from the
/// model alone: nothing is rendered, and what is read is charged to `meter`.
pub(crate) fn render_losses(
    doc: &Document,
    unread: Unread<'_>,
    images: ImagePolicy,
    meter: &Meter<'_>,
) -> Result<Vec<RenderLoss>, ConvertError> {
    let fills = GapFills::default();
    let rc = Ctx::of(doc, &fills, images, meter)?;
    let mut notes = LossCounts::default();
    for note in &rc.selected_notes {
        let omitted = rc.omitted_notes.contains(note.id.as_str());
        if omitted || shape::any(&note.blocks, |block| shape::shows(block, &rc, meter))? {
            visit_inlines(&note.blocks, &rc, &mut |inline, in_label| {
                if !omitted || matches!(inline, Inline::Image { .. } | Inline::NoteRef(_)) {
                    notes.tally(inline, &rc, unread, in_label);
                }
            });
        }
    }
    let mut body = LossCounts::default();
    visit_inlines(&doc.blocks, &rc, &mut |inline, in_label| {
        body.tally(inline, &rc, unread, in_label);
    });
    let dangling = |found: &LossCounts| {
        found.links.saturating_add(if unread.notes { 0 } else { found.note_refs })
    };
    let mut losses: Vec<RenderLoss> = [
        (DiagnosticCode::DuplicateNote, Region::Note, None, rc.duplicate_notes),
        (DiagnosticCode::DanglingReference, Region::Body, None, dangling(&body)),
        (DiagnosticCode::DanglingReference, Region::Note, None, dangling(&notes)),
        (DiagnosticCode::UnsafeLink, Region::Body, None, body.unsafe_links),
        (DiagnosticCode::UnsafeLink, Region::Note, None, notes.unsafe_links),
    ]
    .into_iter()
    .filter(|&(_, _, _, count)| count > 0)
    .collect();
    losses.extend(
        crate::ocr::gaps(doc)
            .map(|located| (DiagnosticCode::OcrGap, located.region, located.gap.id.unit(), 1)),
    );
    losses.extend(
        body.images
            .into_iter()
            .chain(notes.images)
            .map(|((code, origin), count)| (code, origin.region, origin.unit, count)),
    );
    Ok(losses)
}

#[derive(Debug, Clone, Default)]
struct LossCounts {
    links: u64,
    note_refs: u64,
    unsafe_links: u64,
    images: BTreeMap<(DiagnosticCode, crate::model::ImageOrigin), u64>,
}

impl LossCounts {
    fn tally(&mut self, inline: &Inline, rc: &Ctx<'_>, unread: Unread<'_>, in_label: bool) {
        match inline {
            Inline::Image { source, origin, .. } => {
                let code = image::project(rc.images, source, in_label).loss;
                let count = self.images.entry((code, *origin)).or_default();
                *count = count.saturating_add(1);
            }
            Inline::Link { target: LinkTarget::Unit(unit), .. }
                if rc.anchors.unit_fragment(*unit).is_none() && !unread.contains(*unit) =>
            {
                self.links = self.links.saturating_add(1);
            }
            Inline::Link { target: LinkTarget::Anchor { id, unit }, .. }
                if !id.is_empty()
                    && rc.anchors.fragment(id).is_none()
                    && !unit.is_some_and(|unit| unread.contains(unit)) =>
            {
                self.links = self.links.saturating_add(1);
            }
            Inline::Link {
                target: LinkTarget::External(url) | LinkTarget::Relative(url), ..
            } if !is_safe_link(url) => {
                self.unsafe_links = self.unsafe_links.saturating_add(1);
            }
            Inline::NoteRef(id) if !rc.defined_notes.contains(id.as_str()) => {
                self.note_refs = self.note_refs.saturating_add(1);
            }
            _ => {}
        }
    }
}

fn visit_inlines(blocks: &[Block], rc: &Ctx<'_>, visit: &mut impl FnMut(&Inline, bool)) {
    fn walk(
        inlines: &[Inline],
        in_label: bool,
        rc: &Ctx<'_>,
        visit: &mut impl FnMut(&Inline, bool),
    ) {
        for inline in inlines {
            visit(inline, in_label);
            if let Inline::Link { content, target } = inline {
                walk(content, in_label || (!target.is_empty() && !rc.degrades(target)), rc, visit);
            }
        }
    }
    for child in blocks.iter().flat_map(|block| children(block, rc.fills)) {
        match child {
            Node::Inlines(inlines) => walk(inlines, false, rc, visit),
            Node::Blocks(blocks) => visit_inlines(blocks, rc, visit),
            Node::Cells(cells) => {
                for cell in cells {
                    visit_inlines(&cell.blocks, rc, visit);
                }
            }
            Node::Inline(_) => {}
        }
    }
}

fn numbered_notes<'d>(rc: &Ctx<'d>) -> Vec<(&'d Note, usize)> {
    let mut ordered: Vec<_> = rc
        .selected_notes
        .iter()
        .filter_map(|&note| rc.nums.get(&note.id).map(|&num| (note, num)))
        .collect();
    ordered.sort_by_key(|(_, num)| *num);
    ordered
}

fn number_notes<'d>(
    blocks: &'d [Block],
    notes: &[&'d Note],
    omitted: &HashSet<&str>,
    fills: &'d GapFills,
) -> NoteNumbers {
    let valid: HashMap<&str, &Note> = notes
        .iter()
        .copied()
        .filter(|note| {
            !omitted.contains(note.id.as_str()) && !note.blocks.iter().all(block_is_blank)
        })
        .map(|note| (note.id.as_str(), note))
        .collect();
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    collect_note_refs(blocks, &valid, &mut order, &mut seen, fills);
    for note in notes {
        if valid.contains_key(note.id.as_str()) && seen.insert(note.id.as_str()) {
            order.push(note.id.as_str());
        }
    }
    order.into_iter().enumerate().map(|(i, id)| (id.to_owned(), i + 1)).collect()
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
fn collect_note_refs<'d>(
    blocks: &'d [Block],
    valid: &HashMap<&str, &'d Note>,
    order: &mut Vec<&'d str>,
    seen: &mut HashSet<&'d str>,
    fills: &'d GapFills,
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
                    && seen.insert(id.as_str())
                {
                    order.push(id.as_str());
                    stack.push(Node::Blocks(&note.blocks));
                }
            }
            Node::Inline(Inline::Link { content, .. }) => stack.push(Node::Inlines(content)),
            Node::Inline(_) => {}
            Node::Blocks(blocks) => {
                for block in blocks.iter().rev() {
                    stack.extend(children(block, fills).into_iter().rev());
                }
            }
            Node::Cells(mut cells) => {
                if let Some(cell) = cells.next() {
                    stack.push(Node::Cells(cells));
                    stack.push(Node::Blocks(&cell.blocks));
                }
            }
        }
    }
}

#[cfg(test)]
fn check_nesting(doc: &Document, fills: &GapFills, max: u64) -> Result<(), ConvertError> {
    crate::model::check_nesting(doc, |gap| fills.filled(gap), max).map_err(Into::into)
}

fn children<'d>(block: &'d Block, fills: &'d GapFills) -> Vec<Node<'d>> {
    if matches!(block, Block::HeaderFooter(_)) {
        Vec::new()
    } else {
        crate::model::children(block, &|gap| fills.filled(gap))
    }
}

fn write_body(
    blocks: &[Block],
    rc: &Ctx<'_>,
    sink: &mut Sink<'_>,
) -> Result<Vec<UnitSpan>, ConvertError> {
    let first =
        blocks.iter().position(|b| matches!(b, Block::UnitStart(_))).unwrap_or(blocks.len());
    let mut last = write_blocks(&blocks[..first], rc, sink, None)?;
    let mut spans = Vec::new();
    let mut rest = &blocks[first..];
    let mut shown_slide = false;
    while let Some((Block::UnitStart(unit), after)) = rest.split_first() {
        let count =
            after.iter().position(|b| matches!(b, Block::UnitStart(_))).unwrap_or(after.len());
        let body = &after[..count];
        let shown = shape::any(body, |block| shape::shows(block, rc, sink.meter()))?;
        if unit.kind == UnitKind::Slide && shown {
            if shown_slide {
                sink.gap(2);
                sink.write("---")?;
                last = None;
            }
            shown_slide = true;
        }
        if let Some(id) = rc.anchors.unit_fragment(*unit) {
            if sink.len() > 0 {
                sink.gap(2);
            }
            sink.write(&format!("<a id=\"{id}\"></a>"))?;
            last = None;
        }
        if shown && sink.len() > 0 {
            sink.gap(2);
            sink.flush_gap()?;
        }
        let start = sink.len();
        last = write_blocks(body, rc, sink, last)?;
        spans.push(UnitSpan { unit: *unit, bytes: start..sink.len() });
        rest = &after[count..];
    }
    Ok(spans)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListMark {
    Dash,
    Star,
    Period,
    Paren,
}

impl ListMark {
    fn as_char(self) -> char {
        match self {
            Self::Dash => '-',
            Self::Star => '*',
            Self::Period => '.',
            Self::Paren => ')',
        }
    }

    /// The mark of the same kind that `CommonMark` reads as starting a new
    /// list rather than continuing this one.
    fn other(self) -> Self {
        match self {
            Self::Dash => Self::Star,
            Self::Star => Self::Dash,
            Self::Period => Self::Paren,
            Self::Paren => Self::Period,
        }
    }
}

/// Write `blocks` separated by empty lines, skipping those that write
/// nothing. `last` marks the sibling list written right before them, and
/// the mark of the list that ends them, else `last` when none wrote, is
/// returned.
fn write_blocks(
    blocks: &[Block],
    rc: &Ctx<'_>,
    sink: &mut Sink<'_>,
    mut last: Option<ListMark>,
) -> Result<Option<ListMark>, ConvertError> {
    let start = sink.len();
    for block in blocks {
        if sink.len() > start {
            sink.gap(2);
        }
        let before = sink.len();
        let mark = write_block(block, rc, sink, last)?;
        if sink.len() > before {
            last = mark;
        }
    }
    if sink.len() > start {
        sink.drop_gap();
    }
    Ok(last)
}

/// Take off every frame around `table`: a single cell that holds one table
/// and, beside it, only paragraphs that show nothing gives way to that table.
fn unwrap<'t>(
    mut table: &'t Table,
    rc: &Ctx<'_>,
    meter: &Meter<'_>,
) -> Result<&'t Table, ConvertError> {
    while let Some(cell) = table.only_cell() {
        let Some(inner) = framed(cell, rc, meter)? else { break };
        table = inner;
    }
    Ok(table)
}

fn framed<'t>(
    cell: &'t Cell,
    rc: &Ctx<'_>,
    meter: &Meter<'_>,
) -> Result<Option<&'t Table>, ConvertError> {
    let mut inner = None;
    for block in &cell.blocks {
        match block {
            Block::Table(table) if inner.is_none() => inner = Some(table),
            Block::Paragraph(_) | Block::Gap(_) if !shape::shows(block, rc, meter)? => {}
            _ => return Ok(None),
        }
    }
    Ok(inner)
}

/// Write `block` after the sibling list marked `last`, returning the mark
/// of the list that ends what it wrote at this level.
fn write_block(
    block: &Block,
    rc: &Ctx<'_>,
    sink: &mut Sink<'_>,
    last: Option<ListMark>,
) -> Result<Option<ListMark>, ConvertError> {
    match block {
        Block::List(list) => return write_list(list, rc, sink, last).map(Some),
        Block::Table(t) => {
            if rc.preserve_spans.get() {
                for row in &t.grid {
                    sink.meter().charge(row.len() as u64 + 1)?;
                }
                if t.origins().any(|cell| cell.row_span > 1 || cell.col_span > 1) {
                    return table::write_preserved(t, sink).map(|()| None);
                }
            }
            return table::write_table(unwrap(t, rc, sink.meter())?, rc, sink).map(|()| None);
        }
        Block::Heading { level, content, .. } => {
            let mut text = Text::new(sink.meter());
            render_inlines(content, InlineContext::Heading, rc, &mut text)?;
            let text = text.into_string();
            let text = text.trim();
            if !text.is_empty() {
                let level = (*level).clamp(1, 6) as usize;
                sink.write(&format!("{} ", "#".repeat(level)))?;
                sink.write(text)?;
            }
        }
        Block::Paragraph(inlines) => {
            let mut text = Text::new(sink.meter());
            render_inlines(inlines, InlineContext::Block, rc, &mut text)?;
            write_paragraph(&text.into_string(), sink)?;
        }
        Block::BlockQuote(blocks) => {
            sink.push_quote();
            write_blocks(blocks, rc, sink, None)?;
            sink.pop();
        }
        Block::CodeBlock { lang, text } => {
            let fence = backtick_fence(text, 3);
            sink.write(&fence)?;
            sink.write(lang.as_deref().unwrap_or(""))?;
            sink.write("\n")?;
            sink.write(text.trim_end_matches('\n'))?;
            sink.write("\n")?;
            sink.write(&fence)?;
        }
        Block::Rule => sink.write("---")?,
        Block::Math(tex) => {
            let tex = tex.trim();
            if !tex.is_empty() {
                sink.write("$$\n")?;
                escape_math(tex, MathSite::Block, |piece| sink.write(piece))?;
                sink.write("\n$$")?;
            }
        }
        Block::Gap(gap) => return write_gap(gap, rc, sink, last),
        Block::HeaderFooter(value) => write_header_footer(value, rc, sink)?,
        Block::UnitStart(_) => {}
    }
    Ok(None)
}

fn write_header_footer(
    value: &crate::model::HeaderFooter,
    rc: &Ctx<'_>,
    sink: &mut Sink<'_>,
) -> Result<(), ConvertError> {
    let number = rc.side_index.get().saturating_add(1);
    rc.side_index.set(number);
    sink.push_quote();
    let region = value.region.name();
    let variant = value.variant.name();
    let label = if region == "header" { "Header" } else { "Footer" };
    let inherited = if value.content.is_none() { ", uses default" } else { "" };
    sink.write(&format!(
        "<a id=\"almiladoc:{region}:{variant}:{number}\"></a>{label} ({variant}{inherited}):"
    ))?;
    if let Some(content) = &value.content {
        let side = Ctx::empty(rc.fills, rc.images, number);
        sink.gap(2);
        write_blocks(content, &side, sink, None)?;
        rc.side_index.set(side.side_index.get());
    }
    sink.pop();
    Ok(())
}

/// Write a filled gap's fill in its place after the sibling list marked `last`, else a page's
/// marker.
fn write_gap(
    gap: &Gap,
    rc: &Ctx<'_>,
    sink: &mut Sink<'_>,
    last: Option<ListMark>,
) -> Result<Option<ListMark>, ConvertError> {
    let state = rc.fills.state(gap.id);
    if let Some(GapState::Filled(blocks) | GapState::FilledPreservingSpans(blocks)) = state {
        let before =
            rc.preserve_spans.replace(matches!(state, Some(GapState::FilledPreservingSpans(_))));
        let result = write_blocks(blocks, rc, sink, last);
        rc.preserve_spans.set(before);
        return result;
    }
    if let OcrOrigin::Page(page) = gap.origin {
        let marker = if state == Some(&GapState::Failed) { "ocr-failed" } else { "needs-ocr" };
        sink.write(&format!("<!-- {marker} page {page} -->"))?;
    }
    Ok(None)
}

/// `CommonMark` reads an ordered list marker of at most nine digits.
const MAX_ORDINAL: u64 = 999_999_999;

fn write_list(
    list: &List,
    rc: &Ctx<'_>,
    sink: &mut Sink<'_>,
    last: Option<ListMark>,
) -> Result<ListMark, ConvertError> {
    let loose = shape::loose(list, rc, sink.meter())?;
    let ordered = ordered_mark(list);
    let wanted = ordered.unwrap_or(ListMark::Dash);
    let mark = if last == Some(wanted) { wanted.other() } else { wanted };
    for (i, item) in list.items.iter().enumerate() {
        if i > 0 {
            sink.gap(if loose { 2 } else { 1 });
        }
        let n = list.start.saturating_add(i as u64);
        let mut marker = Text::new(sink.meter());
        if ordered.is_some() {
            marker.push_str(&n.to_string())?;
            marker.push(mark.as_char())?;
            marker.push(' ')?;
        } else {
            marker.push(mark.as_char())?;
            marker.push(' ')?;
            let label = match (&item.marker_label, list.marker) {
                (Some(label), _) => Some(Cow::Borrowed(label.as_str())),
                (None, MarkerKind::Bullet) => None,
                (None, kind) => Some(Cow::Owned(kind.label(n))),
            };
            if let Some(label) = label {
                escape_marker_label(&label, InlineContext::Block, &mut marker)?;
                marker.push(' ')?;
            }
        }
        let indent = marker.as_str().chars().count();
        sink.push_marker(marker, indent);
        write_blocks(&item.blocks, rc, sink, None)?;
        sink.pop_shown()?;
    }
    Ok(mark)
}

fn ordered_mark(list: &List) -> Option<ListMark> {
    if list.marker != MarkerKind::Decimal {
        return None;
    }
    let extra = u64::try_from(list.items.len()).ok()?.checked_sub(1)?;
    let end = list.start.checked_add(extra).filter(|&end| end <= MAX_ORDINAL)?;
    let mut mark = ListMark::Paren;
    for (item, n) in list.items.iter().zip(list.start..=end) {
        let Some(label) = item.marker_label.as_deref() else {
            mark = ListMark::Period;
            continue;
        };
        match label.strip_prefix(n.to_string().as_str())? {
            "" => mark = ListMark::Period,
            ")" => {}
            _ => return None,
        }
    }
    Some(mark)
}

/// Write a paragraph's rendered `text` with its lines trimmed, keeping
/// hard-break backslashes intact.
fn write_paragraph(text: &str, sink: &mut Sink<'_>) -> Result<(), ConvertError> {
    let lines: Vec<&str> = text
        .lines()
        .map(|l| {
            let t = l.trim_start();
            let t = if ends_with_hard_break(t) { t } else { t.trim_end() };
            if t == "\\" { "" } else { t }
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
