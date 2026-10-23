//! Block and inline walking for ODF text content.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::formats::odf::styles::{LIST_LEVELS, ListLevel, OdfStyles, parse_start};
use crate::formats::odf::table::{Held, parse_table};
use crate::model::{
    Block, ImageSource, Inline, LinkTarget, List, ListItem, MarkerKind, Note, NoteKind,
    inlines_are_empty,
};
use crate::package::Package;
use crate::package::archive::Role;
use crate::package::xml::{Element, Node, ns};
use crate::shared::assets::{AssetSink, media_type_for};
use crate::shared::blockstyle::StyledRun;
use crate::shared::delta::{StyleDelta, rebase_emphasis};
use crate::shared::math::{MathTex, mathml_to_tex};
use crate::shared::numbering::{LevelCounters, composite_label, every_deeper};
use crate::shared::text::{clean_text, collapse_ws};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

pub(super) struct Ctx<'a, 'b> {
    pub parse_ctx: &'b ParseCtx<'b>,
    pub styles: &'b OdfStyles<'b>,
    pub pkg: &'b Package<'a>,
    pub assets: &'b RefCell<AssetSink>,
    pub notes: RefCell<Vec<Note>>,
    note_ids: RefCell<std::collections::HashSet<String>>,
    /// Continuation counters per (list style, depth): the next number a
    /// `text:continue-numbering` list resumes at.
    list_counters: RefCell<HashMap<(String, usize), u64>>,
    /// Next number per list `xml:id`, resolved by `text:continue-list`.
    list_ids: RefCell<HashMap<String, u64>>,
    /// Heading numbering state for `text:outline-style` (values, started).
    heading_counters: RefCell<LevelCounters<LIST_LEVELS>>,
    region: Cell<Region>,
    held: Cell<Held>,
    dropped: Cell<u64>,
}

impl<'a, 'b> Ctx<'a, 'b> {
    pub(super) fn new(
        parse_ctx: &'b ParseCtx<'b>,
        styles: &'b OdfStyles<'b>,
        pkg: &'b Package<'a>,
        assets: &'b RefCell<AssetSink>,
    ) -> Self {
        Self {
            parse_ctx,
            styles,
            pkg,
            assets,
            notes: RefCell::new(Vec::new()),
            note_ids: RefCell::default(),
            list_counters: RefCell::default(),
            list_ids: RefCell::default(),
            heading_counters: RefCell::default(),
            region: Cell::new(Region::Body),
            held: Cell::default(),
            dropped: Cell::default(),
        }
    }

    pub(super) fn emit(&self, code: DiagnosticCode) {
        self.emit_count(code, 1);
    }

    pub(super) fn emit_count(&self, code: DiagnosticCode, count: u64) {
        self.parse_ctx.emit_count(code, self.region.get(), count);
    }

    /// `f`'s result and what the tables it parses hold, apart from the
    /// tables around it.
    pub(super) fn holding<T>(&self, f: impl FnOnce() -> T) -> (T, Held) {
        let outer = self.held.take();
        let out = f();
        (out, self.held.replace(outer))
    }

    /// Count what a parsed table holds toward the innermost
    /// [`Ctx::holding`].
    pub(super) fn hold(&self, table: Held) {
        self.held.set(self.held.get().add(table));
    }

    /// Walk `f` with its losses reported in `region`.
    pub(super) fn in_region<T>(&self, region: Region, f: impl FnOnce() -> T) -> T {
        let outer = self.region.replace(region);
        let out = f();
        self.region.set(outer);
        out
    }

    pub(super) fn dropped(&self) -> u64 {
        self.dropped.get()
    }

    fn mark_dropped(&self) {
        self.dropped.set(self.dropped.get() + 1);
    }

    fn drop_block(&self, block: &Element) {
        if holds_content(block) {
            self.emit(DiagnosticCode::HiddenOmitted);
        }
        self.mark_dropped();
    }
}

pub(super) fn parse_container(
    parent: &Element,
    ctx: &Ctx<'_, '_>,
) -> Result<Vec<Block>, ConvertError> {
    parse_children(parent.child_elems(), ctx)
}

pub(super) fn parse_children<'e>(
    children: impl Iterator<Item = &'e Element>,
    ctx: &Ctx<'_, '_>,
) -> Result<Vec<Block>, ConvertError> {
    parse_blocks(children, ctx, 0, None, &[])
}

fn parse_blocks<'e>(
    children: impl Iterator<Item = &'e Element>,
    ctx: &Ctx<'_, '_>,
    depth: usize,
    style_name: Option<&str>,
    ancestors: &[u64],
) -> Result<Vec<Block>, ConvertError> {
    let mut blocks = Vec::new();
    let mut run = StyledRun::default();
    let mut after_list = false;
    for child in children {
        after_list = if child.is(ns::TEXT, "list") {
            run.flush(&mut blocks);
            parse_list(child, ctx, depth, style_name, ancestors, after_list, &mut blocks)?
        } else {
            parse_block_elem(child, ctx, &mut blocks, &mut run)?;
            after_list && child.is(ns::TEXT, "soft-page-break")
        };
    }
    run.flush(&mut blocks);
    Ok(blocks)
}

fn parse_block_elem(
    elem: &Element,
    ctx: &Ctx<'_, '_>,
    blocks: &mut Vec<Block>,
    run: &mut StyledRun,
) -> Result<(), ConvertError> {
    if elem.ns.as_deref() == Some(ns::TEXT) {
        if !elem.is(ns::TEXT, "p") {
            run.flush(blocks);
        }
        match elem.local.as_str() {
            "h" => {
                let level = elem
                    .attr(ns::TEXT, "outline-level")
                    .and_then(|v| v.parse::<u8>().ok())
                    .unwrap_or(1);
                if let Some((mut inlines, boxes)) = parse_inline_content(elem, ctx)? {
                    if !inlines_are_empty(&inlines) {
                        rebase_emphasis(&mut inlines, paragraph_base(elem, ctx)?.resolve());
                        // ODF outline links target headings by their text; carry
                        // it as the heading's anchor id (without the number).
                        let anchor = Some(crate::model::inlines_to_plain_text(&inlines));
                        if let Some(label) = heading_label(elem, level, ctx) {
                            inlines.insert(0, Inline::plain(label));
                        }
                        blocks.push(Block::Heading { level, anchor, content: inlines });
                    }
                    blocks.extend(boxes);
                }
            }
            "p" => {
                if let Some((inlines, boxes)) = parse_inline_content(elem, ctx)? {
                    let style = elem
                        .attr(ns::TEXT, "style-name")
                        .map(|name| ctx.styles.block_style(ctx.parse_ctx, name))
                        .transpose()?
                        .flatten();
                    if let Some(style) = style {
                        run.push(style, inlines, blocks);
                    } else {
                        run.flush(blocks);
                        blocks.push(Block::Paragraph(inlines));
                    }
                    if !boxes.is_empty() {
                        run.flush(blocks);
                        blocks.extend(boxes);
                    }
                }
            }
            // ODF 1.3 Part 3 19.802.6: `condition` defers to `text:condition`,
            // which is never evaluated, so only `none` hides.
            "section" if elem.attr(ns::TEXT, "display") == Some("none") => ctx.drop_block(elem),
            "section" | "index-body" | "index-title" | "table-of-content"
            | "alphabetical-index" | "bibliography" | "illustration-index" | "table-index"
            | "object-index" | "user-index" => blocks.extend(parse_container(elem, ctx)?),
            "numbered-paragraph" => ctx.emit(DiagnosticCode::BlockUnread),
            "tracked-changes" => report_deletions(elem, ctx),
            _ => {}
        }
    } else if elem.is(ns::TABLE, "table") {
        run.flush(blocks);
        blocks.extend(parse_table(elem, ctx)?);
    } else if elem.is(ns::OFFICE, "annotation") {
        ctx.parse_ctx.emit(DiagnosticCode::CommentOmitted, Region::Comment);
    } else {
        report_unplaced(elem, ctx)?;
    }
    Ok(())
}

/// Report the deleted text a `text:tracked-changes` holds, which accepted
/// revisions leave out.
fn report_deletions(changes: &Element, ctx: &Ctx<'_, '_>) {
    let deletions =
        changes.descendants(ns::TEXT, "deletion").fold(0u64, |n, _| n.saturating_add(1));
    if deletions > 0 {
        ctx.emit_count(DiagnosticCode::RevisionOmitted, deletions);
    }
}

/// Report a drawing a container holds outside its text, which no output
/// places: a frame's picture, text box, object or media, a form control,
/// or a shape's text.
pub(super) fn report_unplaced(elem: &Element, ctx: &Ctx<'_, '_>) -> Result<(), ConvertError> {
    let code = if elem.is(super::DR3D, "scene") {
        holds_content(elem).then_some(DiagnosticCode::GraphicUnread)
    } else if elem.ns.as_deref() != Some(ns::DRAW) {
        None
    } else {
        match elem.local.as_str() {
            "frame" => frame_loss(elem, ctx)?,
            "g" | "a" => {
                for child in elem.child_elems() {
                    report_unplaced(child, ctx)?;
                }
                None
            }
            "control" => Some(DiagnosticCode::FormControlUnread),
            local if is_shape(local) => {
                holds_content(elem).then_some(DiagnosticCode::GraphicUnread)
            }
            _ => None,
        }
    };
    if let Some(code) = code {
        ctx.emit(code);
    }
    Ok(())
}

pub(super) fn is_shape(local: &str) -> bool {
    matches!(
        local,
        "custom-shape"
            | "rect"
            | "ellipse"
            | "circle"
            | "polygon"
            | "polyline"
            | "regular-polygon"
            | "path"
            | "line"
            | "connector"
            | "caption"
            | "measure"
    )
}

/// What a frame no output places loses: its first text box, object or
/// media, else its picture, which is only their replacement when they are
/// present.
fn frame_loss(frame: &Element, ctx: &Ctx<'_, '_>) -> Result<Option<DiagnosticCode>, ConvertError> {
    let Some(content) = frame
        .child_elems()
        .find(|c| c.is(ns::DRAW, "text-box") || (is_graphic(c) && c.local != "image"))
        .or_else(|| frame.find(ns::DRAW, "image"))
    else {
        return Ok(None);
    };
    Ok(match content.local.as_str() {
        "text-box" => holds_content(content).then_some(DiagnosticCode::TextBoxUnread),
        "object" => match read_object(ctx, content)? {
            Object::Formula(_) => Some(DiagnosticCode::EmbeddedObject),
            Object::Empty => None,
            Object::Unread(code) => Some(code),
        },
        "image" => Some(ctx.parse_ctx.unplaced_image_code()),
        _ => media_loss(content),
    })
}

/// Whether a frame child is a picture, an object or media, which a frame
/// shows as a graphic.
pub(super) fn is_graphic(content: &Element) -> bool {
    content.ns.as_deref() == Some(ns::DRAW)
        && matches!(
            content.local.as_str(),
            "image" | "object" | "object-ole" | "plugin" | "floating-frame" | "applet"
        )
}

/// The loss of a frame's OLE object, plugin (audio or video), floating
/// frame or applet, of which only a preview picture can render.
fn media_loss(content: &Element) -> Option<DiagnosticCode> {
    match content.local.as_str() {
        "plugin" => Some(DiagnosticCode::GraphicUnread),
        "object-ole" | "floating-frame" | "applet" => Some(DiagnosticCode::EmbeddedObject),
        _ => None,
    }
}

/// Whether an element shows anything: text, a picture, an object, media or
/// a note reference. Comments and tracked deletions, which no output shows,
/// do not count.
pub(super) fn holds_content(elem: &Element) -> bool {
    let mut pending = vec![elem];
    while let Some(parent) = pending.pop() {
        for node in &parent.children {
            match node {
                Node::Text(text) if renders(text) => return true,
                Node::Text(_) => {}
                Node::Elem(e) if is_graphic(e) || e.is(ns::TEXT, "note") => return true,
                Node::Elem(e)
                    if e.is(ns::OFFICE, "annotation")
                        || e.is(super::OFFICEOOO, "annotation")
                        || e.is(ns::TEXT, "tracked-changes") => {}
                Node::Elem(e) => pending.push(e),
            }
        }
    }
    false
}

/// Whether text shows anything but white space once cleaned for output.
fn renders(text: &str) -> bool {
    !clean_text(text).trim().is_empty()
}

/// One `text:list` -> blocks appended to `blocks`, the container it sits
/// in: list headers render without markers (as plain blocks alongside the
/// list), and every `text:start-value` restart after the first item splits
/// the run into a new list with that start. A continuation's blocks nest
/// under the item before it, or, leading the list, under the last item of
/// a list ending `blocks` when `after_list`; after a header or a dropped
/// item, or with neither, they sit alongside like a header's.
/// `ancestors` carries the enclosing levels' current numbers so composite
/// labels (`text:display-levels` > 1) render the full chain. Returns
/// whether a continuation leading the next sibling list may nest under the
/// last item of the list then ending `blocks`: after an item or a
/// continuation nested under one, not after a header, a continuation set
/// alongside, or nothing placed.
fn parse_list(
    elem: &Element,
    ctx: &Ctx<'_, '_>,
    depth: usize,
    inherited_style: Option<&str>,
    ancestors: &[u64],
    after_list: bool,
    blocks: &mut Vec<Block>,
) -> Result<bool, ConvertError> {
    let style_name = elem.attr(ns::TEXT, "style-name").or(inherited_style);
    let (marker, level_start) = ctx.styles.list_level(style_name.unwrap_or(""), depth);
    let ordered = marker.ordered();
    let counter_key = (style_name.unwrap_or("").to_string(), depth);
    let start = if ordered {
        resumed_start(elem, ctx, &counter_key).unwrap_or(level_start)
    } else {
        level_start
    };
    let mut run = ListRun::new(marker, start);
    for item in elem.child_elems() {
        let header = item.is(ns::TEXT, "list-header");
        if !(header || item.is(ns::TEXT, "list-item")) {
            continue;
        }
        let continuation = !header && continues(item);
        let (number, prior, restart) = if continuation {
            let prior = if run.barrier {
                None
            } else {
                continued_item(&mut run.current, blocks, after_list)
            };
            (prior.as_ref().map_or(level_start, |(number, _)| *number), prior, None)
        } else {
            let restart = item
                .attr(ns::TEXT, "start-value")
                .and_then(parse_start)
                .filter(|_| !header && ordered);
            (restart.unwrap_or_else(|| run.next_number()), None, restart)
        };
        let chain = [ancestors, &[number]].concat();
        let dropped = ctx.dropped();
        let item_blocks = parse_blocks(item.child_elems(), ctx, depth + 1, style_name, &chain)?;
        if continuation {
            if let Some((_, prior)) = prior {
                prior.blocks.extend(item_blocks);
                run.continuable = true;
            } else if !item_blocks.is_empty() {
                run.unwrapped += item_blocks.len();
                run.alongside(item_blocks);
            }
        } else if item_blocks.is_empty() && ctx.dropped() != dropped {
            run.barrier = true;
        } else if header {
            run.barrier = true;
            run.alongside(item_blocks);
        } else {
            let marker_label = item_label(ctx, style_name, depth, &chain);
            run.item(restart, ListItem { blocks: item_blocks, marker_label });
        }
    }
    run.flush(run.next_number());
    if ordered && run.out.len() > run.unwrapped {
        ctx.list_counters.borrow_mut().insert(counter_key, run.next_number());
        if let Some(id) = elem.attr_qualified(ns::XML, "id") {
            ctx.list_ids.borrow_mut().insert(id.to_string(), run.next_number());
        }
    }
    blocks.extend(run.out);
    Ok(run.continuable)
}

/// Where an ordered list resumes: an explicit `continue-list` target (by
/// `xml:id`) wins over the style-scoped `continue-numbering`.
fn resumed_start(elem: &Element, ctx: &Ctx<'_, '_>, key: &(String, usize)) -> Option<u64> {
    match elem.attr(ns::TEXT, "continue-list") {
        Some(target) => ctx.list_ids.borrow().get(target).copied(),
        None if elem.attr(ns::TEXT, "continue-numbering") == Some("true") => {
            ctx.list_counters.borrow().get(key).copied()
        }
        None => None,
    }
}

struct ListRun {
    current: List,
    out: Vec<Block>,
    unwrapped: usize,
    barrier: bool,
    continuable: bool,
}

impl ListRun {
    fn new(marker: MarkerKind, start: u64) -> Self {
        Self {
            current: List { marker, start, items: Vec::new() },
            out: Vec::new(),
            unwrapped: 0,
            barrier: false,
            continuable: false,
        }
    }

    fn next_number(&self) -> u64 {
        self.current.start.saturating_add(self.current.items.len() as u64)
    }

    fn flush(&mut self, start: u64) {
        let marker = self.current.marker;
        let done = std::mem::replace(&mut self.current, List { marker, start, items: Vec::new() });
        if !done.items.is_empty() {
            self.out.push(Block::List(done));
        }
    }

    fn alongside(&mut self, blocks: Vec<Block>) {
        self.flush(self.next_number());
        self.out.extend(blocks);
        self.continuable = false;
    }

    fn item(&mut self, restart: Option<u64>, item: ListItem) {
        if let Some(start) = restart {
            self.flush(start);
        }
        self.barrier = false;
        self.continuable = true;
        self.current.items.push(item);
    }
}

/// ODF 1.3 Part 3 5.3.5 shows no label on "a list item \[that\] starts another
/// list and does not contain any text".
fn continues(item: &Element) -> bool {
    item.child_elems()
        .find(|child| !child.is(ns::TEXT, "soft-page-break"))
        .is_some_and(|child| child.is(ns::TEXT, "list"))
        && !item.child_elems().any(|child| child.is(ns::TEXT, "p") || child.is(ns::TEXT, "h"))
}

fn last_item(list: &mut List) -> Option<(u64, &mut ListItem)> {
    let number = list.start.saturating_add(list.items.len().saturating_sub(1) as u64);
    list.items.last_mut().map(|item| (number, item))
}

fn continued_item<'l>(
    current: &'l mut List,
    blocks: &'l mut [Block],
    after_list: bool,
) -> Option<(u64, &'l mut ListItem)> {
    if !current.items.is_empty() {
        return last_item(current);
    }
    match blocks.last_mut() {
        Some(Block::List(list)) if after_list => last_item(list),
        _ => None,
    }
}

/// A list item's composite marker label (`num-prefix`/`num-suffix`/
/// `display-levels`); `None` when the default `n.` label is faithful. `chain` holds
/// the number of each enclosing level, outermost first, then the item's: `depth + 1` of them.
fn item_label(
    ctx: &Ctx<'_, '_>,
    style_name: Option<&str>,
    depth: usize,
    chain: &[u64],
) -> Option<String> {
    let levels = ctx.styles.list_levels(style_name?)?;
    let depth = LevelCounters::<LIST_LEVELS>::clamp(depth);
    if !level(levels, depth).marker.ordered() {
        return None;
    }
    let value = *chain.last().unwrap_or(&1);
    label(levels, depth, value, |l| chain[l])
}

/// ODF heading numbering: the `text:outline-style` level formats with the
/// heading's `restart-numbering`/`start-value`/`is-list-header` controls.
/// Advances the outline sequence; `None` for unnumbered headings.
fn heading_label(elem: &Element, outline_level: u8, ctx: &Ctx<'_, '_>) -> Option<String> {
    let levels = ctx.styles.outline_levels()?;
    let idx = LevelCounters::<LIST_LEVELS>::clamp(usize::from(outline_level.max(1)) - 1);
    let lvl = level(levels, idx);
    if !lvl.marker.ordered() {
        return None;
    }
    // An unnumbered heading displays no number and consumes none.
    if elem.attr(ns::TEXT, "is-list-header") == Some("true") {
        return None;
    }
    let counters = &mut *ctx.heading_counters.borrow_mut();
    if elem.attr(ns::TEXT, "restart-numbering") == Some("true") {
        counters.restart(idx);
    }
    let value = match elem.attr(ns::TEXT, "start-value").and_then(parse_start) {
        Some(value) => {
            counters.seed(idx, value, every_deeper);
            value
        }
        None => counters.next(idx, lvl.start, every_deeper),
    };
    let numbered = label(levels, idx, value, |l| counters.value_or(l, level(levels, l).start));
    // Headings have no native numbering in the output, so the default label
    // is rendered too.
    Some(format!("{} ", numbered.unwrap_or_else(|| lvl.marker.label(value))))
}

type Levels = [ListLevel; LIST_LEVELS];

fn level(levels: &Levels, l: usize) -> &ListLevel {
    &levels[LevelCounters::<LIST_LEVELS>::clamp(l)]
}

fn label(levels: &Levels, idx: usize, value: u64, start: impl Fn(usize) -> u64) -> Option<String> {
    let lvl = level(levels, idx);
    composite_label(&lvl.pattern(idx), lvl.marker, value, |l| level(levels, l).marker, start)
}

/// Inline content of a paragraph plus block attachments (text boxes) that
/// were anchored in it.
type ParagraphContent = (Vec<Inline>, Vec<Block>);

/// A paragraph's content; `None` when the paragraph is hidden whole or all
/// its content drops as hidden.
fn parse_inline_content(
    elem: &Element,
    ctx: &Ctx<'_, '_>,
) -> Result<Option<ParagraphContent>, ConvertError> {
    if hidden_by_fields(elem, ctx.parse_ctx)? {
        ctx.drop_block(elem);
        return Ok(None);
    }
    let base = paragraph_base(elem, ctx)?;
    let mut out = Vec::new();
    let mut boxes = Vec::new();
    let mut hidden = Hidden::default();
    walk_inlines(elem, ctx, base, &mut out, &mut boxes, &mut hidden)?;
    hidden.end(ctx);
    if hidden.any && boxes.is_empty() && inlines_are_empty(&out) {
        ctx.mark_dropped();
        return Ok(None);
    }
    Ok(Some((out, boxes)))
}

/// Whether the `text:hidden-paragraph` fields of a paragraph hide it. ODF 1.3
/// Part 3 7.7.11: one whose condition holds hides it unless another's fails;
/// each condition is read from its cached `text:is-hidden` (19.821), never
/// evaluated, so an absent value shows.
fn hidden_by_fields(paragraph: &Element, parse_ctx: &ParseCtx<'_>) -> Result<bool, ConvertError> {
    let mut fields = Fields::default();
    paragraph_fields(paragraph, parse_ctx, &mut fields)?;
    Ok(fields.hide && !fields.show)
}

#[derive(Default)]
struct Fields {
    hide: bool,
    show: bool,
}

/// Record what the `text:hidden-paragraph` fields under `elem` say, through
/// the elements ODF 1.3 Part 3 7.7.11 allows them in, charging one unit per
/// element looked at.
fn paragraph_fields(
    elem: &Element,
    parse_ctx: &ParseCtx<'_>,
    fields: &mut Fields,
) -> Result<(), ConvertError> {
    for child in elem.child_elems() {
        parse_ctx.charge(1)?;
        if child.is(ns::TEXT, "hidden-paragraph") {
            if child.attr(ns::TEXT, "is-hidden") == Some("true") {
                fields.hide = true;
            } else {
                fields.show = true;
            }
        } else if child.ns.as_deref() == Some(ns::TEXT)
            && matches!(
                child.local.as_str(),
                "span" | "a" | "meta" | "meta-field" | "ruby" | "ruby-base"
            )
        {
            paragraph_fields(child, parse_ctx, fields)?;
        }
    }
    Ok(())
}

/// What a paragraph's walk drops as hidden: whether it dropped anything, and
/// whether the range dropped since the last shown content holds anything
/// that renders.
#[derive(Default)]
struct Hidden {
    any: bool,
    holds: bool,
}

impl Hidden {
    fn hide(&mut self, renders: bool) {
        self.any = true;
        self.holds |= renders;
    }

    fn end(&mut self, ctx: &Ctx<'_, '_>) {
        if std::mem::take(&mut self.holds) {
            ctx.emit(DiagnosticCode::HiddenOmitted);
        }
    }

    fn push(&mut self, ctx: &Ctx<'_, '_>, out: &mut Vec<Inline>, inline: Inline) {
        self.end(ctx);
        out.push(inline);
    }
}

/// The style a paragraph's runs cascade from. An unstyled paragraph still sits
/// on the family's default style.
fn paragraph_base(elem: &Element, ctx: &Ctx<'_, '_>) -> Result<StyleDelta, ConvertError> {
    ctx.styles.delta(ctx.parse_ctx, "paragraph", elem.attr(ns::TEXT, "style-name").unwrap_or(""))
}

fn walk_inlines(
    elem: &Element,
    ctx: &Ctx<'_, '_>,
    delta: StyleDelta,
    out: &mut Vec<Inline>,
    boxes: &mut Vec<Block>,
    hidden: &mut Hidden,
) -> Result<(), ConvertError> {
    let style = delta.resolve();
    let hides = delta.hidden == Some(true);
    for node in &elem.children {
        match node {
            Node::Text(t) if hides => hidden.hide(renders(t)),
            Node::Text(t) => {
                let text = collapse_ws(&clean_text(t));
                if !text.is_empty() {
                    hidden.push(ctx, out, Inline::Text { text, style });
                }
            }
            Node::Elem(child)
                if child.ns.as_deref() == Some(ns::TEXT) || child.is(ns::DRAW, "a") =>
            {
                walk_text_elem(child, ctx, delta, out, boxes, hidden)?;
            }
            Node::Elem(child) if child.is(ns::OFFICE, "annotation") => {
                if !hides {
                    ctx.parse_ctx.emit(DiagnosticCode::CommentOmitted, Region::Comment);
                }
            }
            Node::Elem(child) if child.is(ns::DRAW, "frame") && hides => {
                hidden.hide(holds_content(child));
            }
            Node::Elem(child) if child.is(ns::DRAW, "frame") => match frame_content(child, ctx)? {
                Some(FrameContent::Inline(inline)) => hidden.push(ctx, out, inline),
                Some(FrameContent::Blocks(placed)) if !placed.is_empty() => {
                    hidden.end(ctx);
                    boxes.extend(placed);
                }
                _ => {}
            },
            Node::Elem(child) if child.is(ns::DRAW, "control") => {
                if !hides {
                    ctx.emit(DiagnosticCode::FormControlUnread);
                }
            }
            Node::Elem(child) => walk_inlines(child, ctx, delta, out, boxes, hidden)?,
        }
    }
    Ok(())
}

fn walk_text_elem(
    child: &Element,
    ctx: &Ctx<'_, '_>,
    delta: StyleDelta,
    out: &mut Vec<Inline>,
    boxes: &mut Vec<Block>,
    hidden: &mut Hidden,
) -> Result<(), ConvertError> {
    let hides = delta.hidden == Some(true);
    match child.local.as_str() {
        "span" => {
            let merged = child
                .attr(ns::TEXT, "style-name")
                .map(|name| ctx.styles.delta(ctx.parse_ctx, "text", name))
                .transpose()?
                .map_or(delta, |named| delta.merge(named));
            walk_inlines(child, ctx, merged, out, boxes, hidden)?;
        }
        "a" => {
            let href = child.attr(ns::XLINK, "href").unwrap_or("");
            let mut content = Vec::new();
            walk_inlines(child, ctx, delta, &mut content, boxes, hidden)?;
            match classify_href(href) {
                Some(target) if !inlines_are_empty(&content) => {
                    out.push(Inline::Link { content, target });
                }
                _ => out.append(&mut content),
            }
        }
        "s" | "tab" | "line-break" | "bookmark" | "bookmark-start" if hides => hidden.hide(false),
        "s" => {
            let n = child.attr(ns::TEXT, "c").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
            hidden.push(ctx, out, Inline::plain(" ".repeat(n.min(20))));
        }
        "tab" => hidden.push(ctx, out, Inline::plain(" ")),
        "line-break" => hidden.push(ctx, out, Inline::LineBreak),
        "bookmark" | "bookmark-start" => {
            if let Some(name) = child.attr(ns::TEXT, "name") {
                out.push(Inline::Anchor(name.to_string()));
            }
        }
        "note" if hides => hidden.hide(true),
        "note" => push_note(child, ctx, hidden, out)?,
        "hidden-text" if child.attr(ns::TEXT, "is-hidden") == Some("true") => {
            hidden.hide(holds_content(child));
        }
        "tracked-changes" => report_deletions(child, ctx),
        "soft-page-break" => {}
        _ => walk_inlines(child, ctx, delta, out, boxes, hidden)?,
    }
    Ok(())
}

fn push_note(
    note: &Element,
    ctx: &Ctx<'_, '_>,
    hidden: &mut Hidden,
    out: &mut Vec<Inline>,
) -> Result<(), ConvertError> {
    hidden.end(ctx);
    let idx = ctx.notes.borrow().len();
    let id = note
        .attr(ns::TEXT, "id")
        .map_or_else(|| format!("odf:a:{idx}"), |id| format!("odf:n:{id}"));
    if !ctx.note_ids.borrow_mut().insert(id.clone()) {
        ctx.parse_ctx.emit(DiagnosticCode::DuplicateNote, Region::Note);
        out.push(Inline::NoteRef(id));
        return Ok(());
    }
    let kind = match note.attr(ns::TEXT, "note-class") {
        Some("endnote") => NoteKind::Endnote,
        _ => NoteKind::Footnote,
    };
    ctx.notes.borrow_mut().push(Note { id: id.clone(), kind, blocks: Vec::new() });
    let blocks = note
        .find(ns::TEXT, "note-body")
        .map(|body| ctx.holding(|| ctx.in_region(Region::Note, || parse_container(body, ctx))).0)
        .transpose()?
        .unwrap_or_default();
    ctx.notes.borrow_mut()[idx].blocks = blocks;
    out.push(Inline::NoteRef(id));
    Ok(())
}

pub(super) enum FrameContent {
    Blocks(Vec<Block>),
    Inline(Inline),
}

pub(super) fn frame_content(
    frame: &Element,
    ctx: &Ctx<'_, '_>,
) -> Result<Option<FrameContent>, ConvertError> {
    let alt = frame
        .find(ns::SVG_COMPAT, "title")
        .map(Element::text)
        .or_else(|| frame.find(ns::SVG_COMPAT, "desc").map(Element::text))
        .unwrap_or_default();
    let alt = clean_text(alt.trim());
    let image = |alt, source| {
        let origin = ctx.parse_ctx.image_origin(ctx.region.get());
        FrameContent::Inline(Inline::Image { origin, alt, source })
    };
    for content in frame.child_elems() {
        if content.is(ns::DRAW, "text-box") {
            return Ok(Some(FrameContent::Blocks(parse_container(content, ctx)?)));
        }
        if content.is(ns::TABLE, "table") {
            return Ok(Some(FrameContent::Blocks(parse_table(content, ctx)?)));
        }
        if content.is(ns::DRAW, "object") {
            match read_object(ctx, content)? {
                Object::Formula(MathTex { tex, glyphs }) => {
                    ctx.emit_count(DiagnosticCode::UnmappedSymbol, glyphs);
                    if !tex.is_empty() {
                        return Ok(Some(FrameContent::Inline(Inline::Math(tex))));
                    }
                }
                Object::Unread(code) => ctx.emit(code),
                Object::Empty => {}
            }
        } else if content.is(ns::DRAW, "image") {
            let source = load_image(ctx, content.attr(ns::XLINK, "href").unwrap_or(""))?;
            return Ok(Some(image(alt, source)));
        } else if content.ns.as_deref() == Some(ns::DRAW)
            && let Some(code) = media_loss(content)
        {
            ctx.emit(code);
        }
    }
    Ok((!alt.is_empty()).then(|| image(alt, ImageSource::Unavailable)))
}

/// What a `draw:object` converts to.
pub(super) enum Object {
    /// A formula with LaTeX to show or image glyphs it drops.
    Formula(MathTex),
    /// A formula with nothing to show.
    Empty,
    /// The loss to report: the object's content is not converted.
    Unread(DiagnosticCode),
}

/// Read a `draw:object`: MathML inline in the object, or in the
/// `content.xml` of the object directory it links to, converts; a chart or
/// any other object does not.
pub(super) fn read_object(ctx: &Ctx<'_, '_>, object: &Element) -> Result<Object, ConvertError> {
    if let Some(math) = object.find(ns::MATHML, "math") {
        return Ok(formula(math));
    }
    let unread = Object::Unread(DiagnosticCode::EmbeddedObject);
    let href = object.attr(ns::XLINK, "href").unwrap_or("");
    if href.is_empty() || crate::shared::uri::is_absolute_uri(href) {
        return Ok(unread);
    }
    let Ok(target) = crate::package::path::resolve("content.xml", &format!("{href}/content.xml"))
    else {
        return Ok(unread);
    };
    let Some(tree) = ctx.pkg.related_xml_part(&target.path, Role::Reported(ctx.region.get()))?
    else {
        return Ok(unread);
    };
    if let Some(math) = tree.first_descendant(ns::MATHML, "math") {
        return Ok(formula(math));
    }
    let chart = tree
        .find(ns::OFFICE, "document-content")
        .and_then(|d| d.find(ns::OFFICE, "body"))
        .is_some_and(|b| b.find(ns::OFFICE, "chart").is_some());
    Ok(if chart { Object::Unread(DiagnosticCode::GraphicUnread) } else { unread })
}

fn formula(math: &Element) -> Object {
    let formula = mathml_to_tex(math);
    if formula.tex.is_empty() && formula.glyphs == 0 {
        Object::Empty
    } else {
        Object::Formula(formula)
    }
}

/// A picture's bytes into the asset store; `Unread` when the href names no
/// part the package holds. Fatal errors propagate.
fn load_image(ctx: &Ctx<'_, '_>, href: &str) -> Result<ImageSource, ConvertError> {
    if href.is_empty() {
        return Ok(ImageSource::Unread);
    }
    if crate::shared::uri::is_absolute_uri(href) {
        return Ok(ImageSource::External(href.to_string()));
    }
    let Ok(target) = crate::package::path::resolve("content.xml", href) else {
        return Ok(ImageSource::Unread);
    };
    let Some(bytes) = ctx.pkg.related_part(&target.path, Role::Reported(ctx.region.get()))? else {
        return Ok(ImageSource::Unread);
    };
    let media = media_type_for(&target.path);
    let id = ctx.assets.borrow_mut().add(media, target.path, &bytes)?;
    Ok(ImageSource::Asset(id))
}

/// ODF hrefs: external URLs, package-relative paths, or `#target` internal
/// references (with `|outline`-style suffixes on generated links).
fn classify_href(href: &str) -> Option<LinkTarget> {
    if href.is_empty() {
        return None;
    }
    if let Some(fragment) = href.strip_prefix('#') {
        let target = fragment.split('|').next().unwrap_or(fragment);
        if target.is_empty() {
            return None;
        }
        return Some(LinkTarget::anchor(crate::package::path::decode_fragment(target)));
    }
    if crate::shared::uri::is_absolute_uri(href) {
        Some(LinkTarget::External(href.to_string()))
    } else {
        Some(LinkTarget::Relative(href.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_href_fragments_are_percent_decoded() {
        assert_eq!(
            classify_href("#caf%C3%A9%20menu"),
            Some(LinkTarget::anchor("café menu".into()))
        );
    }

    #[test]
    fn paragraph_fields_are_sought_only_where_one_may_sit_and_each_look_is_charged() {
        let field = r#"<text:p><text:hidden-paragraph text:is-hidden="true"/></text:p>"#;
        let xml = format!(
            r#"<text:p xmlns:text="{}" xmlns:draw="{}"><text:span><text:a><text:s/></text:a></text:span><text:meta><text:s/></text:meta><text:meta-field><text:s/></text:meta-field><text:ruby><text:ruby-base><text:s/></text:ruby-base><text:ruby-text>r</text:ruby-text></text:ruby><text:note><text:note-body>{field}</text:note-body></text:note><draw:frame><draw:text-box>{field}</draw:text-box></draw:frame></text:p>"#,
            ns::TEXT,
            ns::DRAW
        );
        let tree =
            crate::package::xml::parse_xml(&ParseCtx::default(), xml.as_bytes(), Role::Probe)
                .unwrap();
        let paragraph = tree.find(ns::TEXT, "p").unwrap();
        let parse_ctx = ParseCtx::default();
        assert!(!hidden_by_fields(paragraph, &parse_ctx).unwrap());
        assert_eq!(parse_ctx.fuel(), 13);
    }
}
