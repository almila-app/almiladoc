//! Block and inline walking for ODF text content.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::formats::odf::styles::{LIST_LEVELS, OdfStyles, parse_start};
use crate::formats::odf::table::parse_table;
use crate::model::{
    Block, ImageSource, Inline, LinkTarget, List, ListItem, Note, NoteKind, Style,
    inlines_are_empty,
};
use crate::package::Package;
use crate::package::archive::Role;
use crate::package::xml::{Element, Node, ns};
use crate::shared::assets::{AssetSink, media_type_for};
use crate::shared::blockstyle::StyledRun;
use crate::shared::delta::{StyleDelta, rebase_emphasis};
use crate::shared::math::{MathTex, mathml_to_tex};
use crate::shared::text::{clean_text, collapse_ws};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

pub struct Ctx<'a, 'b> {
    pub parse_ctx: &'b ParseCtx<'b>,
    pub styles: &'b OdfStyles<'b>,
    pub pkg: &'b Package<'a>,
    pub assets: &'b RefCell<AssetSink>,
    pub notes: RefCell<Vec<Note>>,
    /// Continuation counters per (list style, depth): the next number a
    /// `text:continue-numbering` list resumes at.
    list_counters: RefCell<HashMap<(String, usize), u64>>,
    /// Next number per list `xml:id`, resolved by `text:continue-list`.
    list_ids: RefCell<HashMap<String, u64>>,
    /// Heading numbering state for `text:outline-style` (values, started).
    heading_counters: RefCell<([u64; LIST_LEVELS], [bool; LIST_LEVELS])>,
    region: Cell<Region>,
}

impl<'a, 'b> Ctx<'a, 'b> {
    pub fn new(
        parse_ctx: &'b ParseCtx<'b>,
        styles: &'b OdfStyles<'b>,
        pkg: &'b Package<'a>,
        assets: &'b RefCell<AssetSink>,
    ) -> Self {
        Ctx {
            parse_ctx,
            styles,
            pkg,
            assets,
            notes: RefCell::new(Vec::new()),
            list_counters: Default::default(),
            list_ids: Default::default(),
            heading_counters: Default::default(),
            region: Cell::new(Region::Body),
        }
    }

    /// Report one occurrence of `code` in the region being walked.
    pub fn emit(&self, code: DiagnosticCode) {
        self.emit_count(code, 1);
    }

    /// Report `count` occurrences of `code` in the region being walked.
    pub fn emit_count(&self, code: DiagnosticCode, count: u64) {
        self.parse_ctx.emit_count(code, self.region.get(), count);
    }

    /// Walk `f` with its losses reported in `region`.
    pub fn in_region<T>(&self, region: Region, f: impl FnOnce() -> T) -> T {
        let outer = self.region.replace(region);
        let out = f();
        self.region.set(outer);
        out
    }
}

pub fn parse_container(parent: &Element, ctx: &Ctx) -> Result<Vec<Block>, ConvertError> {
    parse_children(parent.child_elems(), ctx)
}

pub fn parse_children<'e>(
    children: impl Iterator<Item = &'e Element>,
    ctx: &Ctx,
) -> Result<Vec<Block>, ConvertError> {
    let mut blocks = Vec::new();
    let mut run = StyledRun::default();
    for child in children {
        parse_block_elem(child, ctx, &mut blocks, &mut run)?;
    }
    run.flush(&mut blocks);
    Ok(blocks)
}

fn parse_block_elem(
    elem: &Element,
    ctx: &Ctx,
    blocks: &mut Vec<Block>,
    run: &mut StyledRun,
) -> Result<(), ConvertError> {
    let in_text = elem.ns.as_deref().is_some_and(|n| n == ns::TEXT);
    if in_text {
        // Only a run of same-styled paragraphs continues a container.
        if !elem.is(ns::TEXT, "p") {
            run.flush(blocks);
        }
        match elem.local.as_str() {
            "h" => {
                let level = elem
                    .attr(ns::TEXT, "outline-level")
                    .and_then(|v| v.parse::<u8>().ok())
                    .unwrap_or(1);
                let (inlines, boxes) = parse_inline_content(elem, ctx)?;
                if !inlines_are_empty(&inlines) {
                    let mut content = inlines;
                    rebase_emphasis(&mut content, paragraph_base(elem, ctx)?.resolve());
                    // ODF outline links target headings by their text; carry
                    // it as the heading's anchor id (without the number).
                    let anchor = Some(crate::model::inlines_to_plain_text(&content));
                    if let Some(label) = heading_label(elem, level, ctx) {
                        content.insert(0, Inline::Text { text: label, style: Style::PLAIN });
                    }
                    blocks.push(Block::Heading { level, anchor, content });
                }
                blocks.extend(boxes);
                return Ok(());
            }
            "p" => {
                let (inlines, boxes) = parse_inline_content(elem, ctx)?;
                let style =
                    elem.attr(ns::TEXT, "style-name").and_then(|n| ctx.styles.block_style(n));
                match style {
                    Some(style) => run.push(style, inlines, blocks),
                    None => {
                        run.flush(blocks);
                        blocks.push(Block::Paragraph(inlines));
                    }
                }
                if !boxes.is_empty() {
                    // A frame is not part of the container's text.
                    run.flush(blocks);
                    blocks.extend(boxes);
                }
                return Ok(());
            }
            "list" => {
                blocks.extend(parse_list(elem, ctx, 0, None, &[])?);
                return Ok(());
            }
            "section" | "index-body" | "index-title" => {
                blocks.extend(parse_container(elem, ctx)?);
                return Ok(());
            }
            "table-of-content" | "alphabetical-index" | "bibliography" | "illustration-index"
            | "table-index" | "object-index" | "user-index" => {
                // The stored index body is real document text (the generated
                // entries are written into `text:index-body`).
                blocks.extend(parse_container(elem, ctx)?);
                return Ok(());
            }
            "numbered-paragraph" => ctx.emit(DiagnosticCode::BlockUnread),
            "tracked-changes" => report_deletions(elem, ctx),
            _ => {}
        }
        return Ok(());
    }
    if elem.is(ns::TABLE, "table") {
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
fn report_deletions(changes: &Element, ctx: &Ctx) {
    let deletions =
        changes.descendants(ns::TEXT, "deletion").fold(0u64, |n, _| n.saturating_add(1));
    if deletions > 0 {
        ctx.emit_count(DiagnosticCode::RevisionOmitted, deletions);
    }
}

/// Report a drawing a container holds outside its text, which no output
/// places: a frame's picture, text box, object or media, a form control,
/// or a shape's text.
pub(super) fn report_unplaced(elem: &Element, ctx: &Ctx) -> Result<(), ConvertError> {
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
            "custom-shape" | "rect" | "ellipse" | "circle" | "polygon" | "polyline"
            | "regular-polygon" | "path" | "line" | "connector" | "caption" | "measure" => {
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

/// What a frame no output places loses: its first text box, object or
/// media, else its picture, which is only their replacement when they are
/// present.
fn frame_loss(frame: &Element, ctx: &Ctx) -> Result<Option<DiagnosticCode>, ConvertError> {
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
        "image" => Some(DiagnosticCode::ImageUnplaced),
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

/// Whether an element shows anything: text, a picture, an object or media.
/// Comments and tracked deletions, which no output shows, do not count.
pub(super) fn holds_content(elem: &Element) -> bool {
    let mut pending = vec![elem];
    while let Some(parent) = pending.pop() {
        for node in &parent.children {
            match node {
                Node::Text(text) if !text.trim().is_empty() => return true,
                Node::Text(_) => {}
                Node::Elem(e) if is_graphic(e) => return true,
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

/// One `text:list` -> blocks: list headers render without markers (as
/// plain blocks alongside the list), and every `text:start-value` restart
/// after the first item splits the run into a new list with that start.
/// `ancestors` carries the enclosing levels' current numbers so composite
/// labels (`text:display-levels` > 1) render the full chain.
fn parse_list(
    elem: &Element,
    ctx: &Ctx,
    depth: usize,
    inherited_style: Option<&str>,
    ancestors: &[u64],
) -> Result<Vec<Block>, ConvertError> {
    let style_name = elem.attr(ns::TEXT, "style-name").or(inherited_style);
    let level = ctx.styles.list_level(style_name.unwrap_or(""), depth);
    let ordered = level.marker.ordered();

    let counter_key = (style_name.unwrap_or("").to_string(), depth);
    let mut start = level.start;
    if ordered {
        // Continuation identity: an explicit target list (`continue-list`
        // by xml:id) wins over the style-scoped `continue-numbering`.
        if let Some(target) = elem.attr(ns::TEXT, "continue-list") {
            if let Some(&resume) = ctx.list_ids.borrow().get(target) {
                start = resume;
            }
        } else if elem.attr(ns::TEXT, "continue-numbering") == Some("true")
            && let Some(&resume) = ctx.list_counters.borrow().get(&counter_key)
        {
            start = resume;
        }
    }

    let mut out: Vec<Block> = Vec::new();
    let mut current = List { marker: level.marker, start, items: Vec::new() };
    let mut next = start;
    let mut first_item = true;
    let flush = |current: &mut List, out: &mut Vec<Block>, start: u64| {
        let done =
            std::mem::replace(current, List { marker: current.marker, start, items: Vec::new() });
        if !done.items.is_empty() {
            out.push(Block::List(done));
        }
    };
    for item in elem.child_elems() {
        let header = item.is(ns::TEXT, "list-header");
        if !(header || item.is(ns::TEXT, "list-item")) {
            continue;
        }
        // Establish this item's number before walking children: nested
        // lists render composite labels against the ancestor chain.
        if !header
            && ordered
            && let Some(sv) = item.attr(ns::TEXT, "start-value").and_then(parse_start)
        {
            if first_item {
                current.start = sv;
            } else {
                flush(&mut current, &mut out, sv);
            }
        }
        let number = current.start.saturating_add(current.items.len() as u64);
        let mut chain = ancestors.to_vec();
        chain.push(number);
        let mut item_blocks = Vec::new();
        let mut item_run = StyledRun::default();
        for child in item.child_elems() {
            if child.is(ns::TEXT, "list") {
                item_run.flush(&mut item_blocks);
                item_blocks.extend(parse_list(child, ctx, depth + 1, style_name, &chain)?);
            } else {
                parse_block_elem(child, ctx, &mut item_blocks, &mut item_run)?;
            }
        }
        item_run.flush(&mut item_blocks);
        if header {
            // A list header has no marker: its blocks sit next to the list
            // and do not consume a number.
            flush(&mut current, &mut out, next);
            out.extend(item_blocks);
            continue;
        }
        first_item = false;
        let label = item_label(ctx, style_name, depth, &chain);
        current.items.push(ListItem { blocks: item_blocks, marker_label: label });
        next = current.start.saturating_add(current.items.len() as u64);
    }
    flush(&mut current, &mut out, next);
    if ordered && !out.is_empty() {
        ctx.list_counters.borrow_mut().insert(counter_key, next);
        if let Some(id) = elem.attr_qualified(ns::XML, "id") {
            ctx.list_ids.borrow_mut().insert(id.to_string(), next);
        }
    }
    Ok(out)
}

/// A list item's composite marker label (`num-prefix`/`num-suffix`/
/// `display-levels`); `None` when the default `n.` label is faithful.
fn item_label(ctx: &Ctx, style_name: Option<&str>, depth: usize, chain: &[u64]) -> Option<String> {
    let levels = ctx.styles.list_levels(style_name?)?;
    let depth = depth.min(LIST_LEVELS - 1);
    let lvl = &levels[depth];
    if !lvl.marker.ordered() {
        return None;
    }
    crate::shared::numbering::composite_label(
        &lvl.pattern(depth),
        lvl.marker,
        *chain.last().unwrap_or(&1),
        |l| levels[l.min(LIST_LEVELS - 1)].marker,
        |l| chain.get(l).copied().unwrap_or_else(|| levels[l.min(LIST_LEVELS - 1)].start),
    )
}

/// ODF heading numbering: the `text:outline-style` level formats with the
/// heading's `restart-numbering`/`start-value`/`is-list-header` controls.
/// Advances the outline sequence; `None` for unnumbered headings.
fn heading_label(elem: &Element, level: u8, ctx: &Ctx) -> Option<String> {
    let levels = ctx.styles.outline_levels()?;
    let idx = (level.max(1) as usize - 1).min(LIST_LEVELS - 1);
    let lvl = &levels[idx];
    if !lvl.marker.ordered() {
        return None;
    }
    // An unnumbered heading displays no number and consumes none.
    if elem.attr(ns::TEXT, "is-list-header") == Some("true") {
        return None;
    }
    let (values, started) = &mut *ctx.heading_counters.borrow_mut();
    let restart = elem.attr(ns::TEXT, "restart-numbering") == Some("true");
    let explicit = elem.attr(ns::TEXT, "start-value").and_then(parse_start);
    let value = match explicit {
        Some(v) => v,
        None if started[idx] && !restart => values[idx].saturating_add(1),
        None => lvl.start,
    };
    values[idx] = value;
    started[idx] = true;
    for deeper in started.iter_mut().skip(idx + 1) {
        *deeper = false;
    }
    let label = crate::shared::numbering::composite_label(
        &lvl.pattern(idx),
        lvl.marker,
        value,
        |l| levels[l.min(LIST_LEVELS - 1)].marker,
        |l| {
            let l = l.min(LIST_LEVELS - 1);
            if started[l] { values[l] } else { levels[l].start }
        },
    );
    // Headings have no native numbering in the output, so the default label
    // is rendered too.
    Some(format!("{} ", label.unwrap_or_else(|| lvl.marker.label(value))))
}

/// Inline content of a paragraph plus block attachments (text boxes) that
/// were anchored in it.
fn parse_inline_content(
    elem: &Element,
    ctx: &Ctx,
) -> Result<(Vec<Inline>, Vec<Block>), ConvertError> {
    let base = paragraph_base(elem, ctx)?;
    let mut out = Vec::new();
    let mut boxes = Vec::new();
    walk_inlines(elem, ctx, base, &mut out, &mut boxes)?;
    Ok((out, boxes))
}

/// The style a paragraph's runs cascade from. An unstyled paragraph still sits
/// on the family's default style.
fn paragraph_base(elem: &Element, ctx: &Ctx) -> Result<StyleDelta, ConvertError> {
    ctx.styles.delta("paragraph", elem.attr(ns::TEXT, "style-name").unwrap_or(""))
}

fn walk_inlines(
    elem: &Element,
    ctx: &Ctx,
    delta: StyleDelta,
    out: &mut Vec<Inline>,
    boxes: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    let style = delta.resolve();
    for node in &elem.children {
        match node {
            Node::Text(t) => {
                let text = collapse_ws(&clean_text(t));
                if !text.is_empty() {
                    out.push(Inline::Text { text, style });
                }
            }
            Node::Elem(child) => {
                let in_text = child.ns.as_deref().is_some_and(|n| n == ns::TEXT);
                if in_text || child.is(ns::DRAW, "a") {
                    match child.local.as_str() {
                        "span" => {
                            let merged = match child.attr(ns::TEXT, "style-name") {
                                Some(name) => delta.merge(ctx.styles.delta("text", name)?),
                                None => delta,
                            };
                            walk_inlines(child, ctx, merged, out, boxes)?;
                            continue;
                        }
                        "a" => {
                            let href = child.attr(ns::XLINK, "href").unwrap_or("");
                            let mut content = Vec::new();
                            walk_inlines(child, ctx, delta, &mut content, boxes)?;
                            match classify_href(href) {
                                Some(target) if !inlines_are_empty(&content) => {
                                    out.push(Inline::Link { content, target })
                                }
                                _ => out.append(&mut content),
                            }
                            continue;
                        }
                        "s" => {
                            let n = child
                                .attr(ns::TEXT, "c")
                                .and_then(|v| v.parse::<usize>().ok())
                                .unwrap_or(1);
                            out.push(Inline::Text {
                                text: " ".repeat(n.min(20)),
                                style: Style::PLAIN,
                            });
                            continue;
                        }
                        "tab" => {
                            out.push(Inline::Text { text: " ".into(), style: Style::PLAIN });
                            continue;
                        }
                        "line-break" => {
                            out.push(Inline::LineBreak);
                            continue;
                        }
                        "bookmark" | "bookmark-start" => {
                            if let Some(name) = child.attr(ns::TEXT, "name") {
                                out.push(Inline::Anchor(name.to_string()));
                            }
                            continue;
                        }
                        "note" => {
                            let idx = ctx.notes.borrow().len();
                            let id = child
                                .attr(ns::TEXT, "id")
                                .map(String::from)
                                .unwrap_or_else(|| format!("odt{idx}"));
                            let kind = match child.attr(ns::TEXT, "note-class") {
                                Some("endnote") => NoteKind::Endnote,
                                _ => NoteKind::Footnote,
                            };
                            let note_blocks = match child.find(ns::TEXT, "note-body") {
                                Some(b) => {
                                    ctx.in_region(Region::Note, || parse_container(b, ctx))?
                                }
                                None => Vec::new(),
                            };
                            ctx.notes.borrow_mut().push(Note {
                                id: id.clone(),
                                kind,
                                blocks: note_blocks,
                            });
                            out.push(Inline::NoteRef(id));
                            continue;
                        }
                        "tracked-changes" => {
                            report_deletions(child, ctx);
                            continue;
                        }
                        "soft-page-break" => continue,
                        _ => {}
                    }
                }
                if child.is(ns::OFFICE, "annotation") {
                    ctx.parse_ctx.emit(DiagnosticCode::CommentOmitted, Region::Comment);
                    continue;
                }
                if child.is(ns::DRAW, "frame") {
                    walk_frame(child, ctx, out, boxes)?;
                    continue;
                }
                if child.is(ns::DRAW, "control") {
                    ctx.emit(DiagnosticCode::FormControlUnread);
                    continue;
                }
                walk_inlines(child, ctx, delta, out, boxes)?;
            }
        }
    }
    Ok(())
}

/// A draw:frame inline in text: an image (resolved into the asset store) or
/// a text box (attached as blocks after the paragraph).
pub(super) fn walk_frame(
    frame: &Element,
    ctx: &Ctx,
    out: &mut Vec<Inline>,
    boxes: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    if let Some(text_box) = frame.find(ns::DRAW, "text-box") {
        boxes.extend(parse_container(text_box, ctx)?);
        return Ok(());
    }
    out.extend(frame_graphic(frame, ctx)?);
    Ok(())
}

/// A frame's formula, or else its picture: the object's own replacement
/// image when the object itself does not convert.
pub(super) fn frame_graphic(frame: &Element, ctx: &Ctx) -> Result<Option<Inline>, ConvertError> {
    let object_loss = if let Some(object) = frame.find(ns::DRAW, "object") {
        match read_object(ctx, object)? {
            Object::Formula(MathTex { tex, glyphs }) => {
                ctx.emit_count(DiagnosticCode::UnmappedSymbol, glyphs);
                if !tex.is_empty() {
                    return Ok(Some(Inline::Math(tex)));
                }
                None
            }
            Object::Empty => None,
            Object::Unread(code) => Some(code),
        }
    } else {
        frame.child_elems().filter(|c| c.ns.as_deref() == Some(ns::DRAW)).find_map(media_loss)
    };
    if let Some(code) = object_loss {
        ctx.emit(code);
    }
    let alt = frame
        .first_descendant(ns::SVG_COMPAT, "title")
        .map(|t| t.text())
        .or_else(|| frame.first_descendant(ns::SVG_COMPAT, "desc").map(|d| d.text()))
        .unwrap_or_default();
    let alt = clean_text(alt.trim());
    if let Some(image) = frame.first_descendant(ns::DRAW, "image") {
        let href = image.attr(ns::XLINK, "href").unwrap_or("");
        let source = load_image(ctx, href)?;
        if source.is_none() && object_loss.is_none() {
            ctx.emit(DiagnosticCode::ImageUnavailable);
        }
        if source.is_some() || !alt.is_empty() {
            return Ok(Some(Inline::Image {
                alt,
                source: source.unwrap_or(ImageSource::Unavailable),
            }));
        }
        return Ok(None);
    }
    Ok((!alt.is_empty()).then_some(Inline::Image { alt, source: ImageSource::Unavailable }))
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
pub(super) fn read_object(ctx: &Ctx, object: &Element) -> Result<Object, ConvertError> {
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

/// A picture's bytes into the asset store; `None` when the href names no
/// part the package holds. Fatal errors propagate.
fn load_image(ctx: &Ctx, href: &str) -> Result<Option<ImageSource>, ConvertError> {
    if href.is_empty() {
        return Ok(None);
    }
    if crate::shared::uri::is_absolute_uri(href) {
        return Ok(Some(ImageSource::External(href.to_string())));
    }
    let Ok(target) = crate::package::path::resolve("content.xml", href) else {
        return Ok(None);
    };
    let bytes = ctx.pkg.related_part(&target.path, Role::Reported(ctx.region.get()))?;
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let media = media_type_for(&target.path);
    let id = ctx.assets.borrow_mut().add(media, target.path, &bytes)?;
    Ok(Some(ImageSource::Asset(id)))
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
        return Some(LinkTarget::Anchor(crate::package::path::decode_fragment(target)));
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
            Some(LinkTarget::Anchor("café menu".into()))
        );
    }
}
