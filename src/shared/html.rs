//! (X)HTML element tree -> model blocks. Used by the EPUB frontend.
//!
//! Applies a deliberately small CSS subset - the semantic properties only:
//! `font-weight`, `font-style`, `text-decoration: line-through`,
//! `vertical-align: super|sub|baseline` and `display: none` - from inline
//! `style` attributes and element/class rules.
//! Tables build the canonical grid (`rowspan`/`colspan`); ordered lists honor
//! `start`, `reversed`, `type`, and per-item `value`.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{
    AnchorId, Block, Cell, GridBuilder, ImageSource, Inline, LinkTarget, List, ListItem,
    MarkerKind, VerticalPosition, inlines_are_empty,
};
use crate::package::xml::{Element, Node};
use crate::shared::css::{self, Applies};
use crate::shared::delta::{StyleDelta, rebase_emphasis};
use crate::shared::header::resolve_header_rows;
use crate::shared::math::{MathTex, mathml_is_display, mathml_to_tex};
use crate::shared::text::{clean_text, collapse_ws};
use std::collections::HashMap;

/// Frontend hooks: how hrefs, image sources, and anchor ids resolve in the
/// containing document (EPUB scopes them per chapter).
pub(crate) trait HtmlCtx {
    fn link_target(&self, href: &str) -> Option<LinkTarget>;
    fn image_source(&self, src: &str) -> Result<ImageSource, ConvertError>;
    fn anchor_id(&self, raw: &str) -> AnchorId;
}

pub(crate) fn to_blocks(
    parse_ctx: &ParseCtx<'_>,
    body: &Element,
    css: &Stylesheet,
    ctx: &dyn HtmlCtx,
) -> Result<Vec<Block>, ConvertError> {
    walk_blocks(parse_ctx, css, ctx, body, StyleDelta::default(), true)
}

fn walk_blocks(
    parse_ctx: &ParseCtx<'_>,
    css: &Stylesheet,
    ctx: &dyn HtmlCtx,
    elem: &Element,
    delta: StyleDelta,
    start_boundary: bool,
) -> Result<Vec<Block>, ConvertError> {
    let mut builder =
        Builder { blocks: Vec::new(), inlines: Vec::new(), css, ctx, parse_ctx, start_boundary };
    builder.walk_children(elem, delta)?;
    Ok(builder.finish())
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct StyleProps {
    pub delta: StyleDelta,
    /// `display` visibility as a tri-state: a higher-priority
    /// `display: block` (or any non-none value) can restore content a
    /// lower-priority `display: none` hid.
    pub hidden: Option<bool>,
    /// The element's own `vertical-align`, which CSS does not inherit: `baseline` cancels
    /// only a shift its tag applies, never one an ancestor does.
    pub vertical_align: Option<VerticalPosition>,
}

impl StyleProps {
    fn merge(self, over: Self) -> Self {
        Self {
            delta: self.delta.merge(over.delta),
            hidden: over.hidden.or(self.hidden),
            vertical_align: over.vertical_align.or(self.vertical_align),
        }
    }

    fn is_default(self) -> bool {
        self == Self::default()
    }

    fn display_shown(self) -> Self {
        Self { hidden: self.hidden.filter(|hidden| !hidden), ..Self::default() }
    }
}

/// Cascade priority tiers: rules < inline style < `!important` rules <
/// `!important` inline style, with selector specificity ordering within a
/// tier and source order breaking ties.
const INLINE_PRIORITY: u32 = 100_000;
const IMPORTANT_PRIORITY: u32 = 1_000_000;

#[derive(Debug)]
struct Rule {
    tag: Option<String>,
    /// Cascade priority: the tier base plus selector specificity within the
    /// supported subset (a class outweighs any number of tags: `.c` = 10,
    /// `t.c` = 11, `t` = 1). Source order breaks ties (rules are stored in
    /// document order).
    priority: u32,
    props: StyleProps,
}

/// Each rule sits in exactly one index: `by_class` when it names a class
/// (`t.c` too), else `by_tag`.
#[derive(Debug, Default)]
pub(crate) struct Stylesheet {
    rules: Vec<Rule>,
    by_class: HashMap<String, Vec<usize>>,
    by_tag: HashMap<String, Vec<usize>>,
}

impl Stylesheet {
    /// Add the rules of one stylesheet's text that a screen applies under its `media` list. Only
    /// simple `tag`, `.class`, and `tag.class` selectors participate; `!important` declarations
    /// enter the higher cascade tier; a rule whose group conditions a screen leaves unsettled
    /// contributes only a `display` that shows; charges the length of `css`.
    pub(crate) fn add(
        &mut self,
        parse_ctx: &ParseCtx<'_>,
        css: &str,
        media: &str,
    ) -> Result<(), ConvertError> {
        parse_ctx.charge(css.len() as u64)?;
        let css = strip_css_comments(css);
        for rule in css::scan(&css, css::media_applies(media)) {
            let decls = parse_declarations(rule.declarations.iter().copied());
            let decls =
                if rule.applies == Applies::Unsettled { decls.display_shown() } else { decls };
            if decls.normal.is_default() && decls.important.is_default() {
                continue;
            }
            for selector in rule.selectors.split(',') {
                let s = selector.trim();
                if s.is_empty() || s.contains(' ') || s.contains(':') || s.contains('[') {
                    continue;
                }
                let (tag, class) = match s.split_once('.') {
                    Some(("", c)) => (None, Some(c.to_string())),
                    Some((t, c)) => (Some(t.to_ascii_lowercase()), Some(c.to_string())),
                    None => (Some(s.to_ascii_lowercase()), None),
                };
                let specificity = u32::from(class.is_some()) * 10 + u32::from(tag.is_some());
                for (props, base) in [(decls.normal, 0), (decls.important, IMPORTANT_PRIORITY)] {
                    if !props.is_default() {
                        let index = match (&class, &tag) {
                            (Some(class), _) => self.by_class.entry(class.clone()).or_default(),
                            (None, Some(tag)) => self.by_tag.entry(tag.clone()).or_default(),
                            (None, None) => continue,
                        };
                        index.push(self.rules.len());
                        self.rules.push(Rule {
                            tag: tag.clone(),
                            priority: base + specificity,
                            props,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether, outside comments, a replacement character sits in a declaration name or in a rule
    /// or a group condition above it that declares `display`, among the rules
    /// [`Stylesheet::add`] reads from `css` on a screen; charges the length of `css`.
    pub(crate) fn damage_may_hide(
        parse_ctx: &ParseCtx<'_>,
        css: &str,
    ) -> Result<bool, ConvertError> {
        parse_ctx.charge(css.len() as u64)?;
        let damaged = |text: &str| text.contains(char::REPLACEMENT_CHARACTER);
        let css = strip_css_comments(css);
        Ok(css::scan(&css, Applies::Always).any(|rule| {
            rule.damaged
                && rule
                    .declarations
                    .iter()
                    .filter_map(|segment| segment.split_once(':'))
                    .any(|(name, _)| damaged(name) || name.trim().eq_ignore_ascii_case("display"))
        }))
    }

    /// Matching rules for one element as (priority, props) pairs, in source
    /// order, each candidate rule its tag or classes name charged.
    fn matching_rules(
        &self,
        parse_ctx: &ParseCtx<'_>,
        tag: &str,
        mut classes: Vec<&str>,
    ) -> Result<Vec<(u32, StyleProps)>, ConvertError> {
        classes.sort_unstable();
        classes.dedup();
        let by_class = classes.into_iter().filter_map(|class| self.by_class.get(class));
        let mut candidates: Vec<usize> = Vec::new();
        for rules in self.by_tag.get(tag).into_iter().chain(by_class) {
            parse_ctx.charge(rules.len() as u64)?;
            candidates.extend_from_slice(rules);
        }
        candidates.sort_unstable();
        Ok(candidates
            .into_iter()
            .filter_map(|index| self.rules.get(index))
            .filter(|rule| rule.tag.as_deref().is_none_or(|t| t == tag))
            .map(|rule| (rule.priority, rule.props))
            .collect())
    }
}

fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some((before, comment)) = rest.split_once("/*") {
        out.push_str(before);
        match comment.split_once("*/") {
            Some((_, after)) => rest = after,
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// A declaration block's properties, split by cascade tier.
#[derive(Debug, Clone, Copy, Default)]
struct DeclProps {
    normal: StyleProps,
    important: StyleProps,
}

impl DeclProps {
    fn display_shown(self) -> Self {
        Self { normal: self.normal.display_shown(), important: self.important.display_shown() }
    }
}

fn parse_declarations<'a>(segments: impl IntoIterator<Item = &'a str>) -> DeclProps {
    let mut out = DeclProps::default();
    for (name, value) in segments.into_iter().filter_map(|segment| segment.split_once(':')) {
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_ascii_lowercase();
        let (value, important) = match value.split_once('!') {
            Some((kept, flag)) => (kept.trim_end(), flag.trim() == "important"),
            None => (value.as_str(), false),
        };
        let props = if important { &mut out.important } else { &mut out.normal };
        match name.as_str() {
            "font-weight" => {
                props.delta.bold = Some(
                    matches!(value, "bold" | "bolder")
                        || value.parse::<u32>().is_ok_and(|n| n >= 600),
                );
            }
            "font-style" => props.delta.italic = Some(matches!(value, "italic" | "oblique")),
            "text-decoration" | "text-decoration-line" => {
                if value.contains("line-through") {
                    props.delta.strike = Some(true);
                } else if value == "none" {
                    props.delta.strike = Some(false);
                }
            }
            "display" => props.hidden = Some(value == "none"),
            "vertical-align" => match value {
                "super" => props.vertical_align = Some(VerticalPosition::Superscript),
                "sub" => props.vertical_align = Some(VerticalPosition::Subscript),
                "baseline" => props.vertical_align = Some(VerticalPosition::Baseline),
                _ => {}
            },
            _ => {}
        }
    }
    out
}

struct Builder<'e> {
    blocks: Vec<Block>,
    inlines: Vec<Inline>,
    css: &'e Stylesheet,
    ctx: &'e dyn HtmlCtx,
    parse_ctx: &'e ParseCtx<'e>,
    /// Whether text appended while `inlines` is empty sits at a whitespace
    /// boundary (block start: leading whitespace collapses away; inline
    /// sub-builders inherit the surrounding run's state instead).
    start_boundary: bool,
}

/// Content worth keeping: visible text, or an anchor node some link may
/// target (the renderer drops unreferenced anchors itself).
fn keeps_paragraph(inlines: &[Inline]) -> bool {
    !inlines_are_empty(inlines) || inlines.iter().any(|i| matches!(i, Inline::Anchor(_)))
}

/// Whether text appended after `inlines` sits at a whitespace boundary
/// (its leading spaces collapse away): at a block start, after emitted
/// whitespace or a line break, looking through zero-width anchors.
fn at_space_boundary(inlines: &[Inline], start: bool) -> bool {
    for inline in inlines.iter().rev() {
        match inline {
            Inline::Text { text, .. } if !text.is_empty() => {
                return text.ends_with(char::is_whitespace);
            }
            Inline::Link { content, .. } if !inlines_are_empty(content) => {
                return at_space_boundary(content, false);
            }
            Inline::LineBreak => return true,
            Inline::Anchor(_) | Inline::Text { .. } | Inline::Link { .. } => {}
            Inline::Image { .. } | Inline::NoteRef(_) | Inline::Math(_) | Inline::Checkbox(_) => {
                return false;
            }
        }
    }
    start
}

impl Builder<'_> {
    fn push_paragraph(&mut self, inlines: Vec<Inline>) {
        if keeps_paragraph(&inlines) {
            self.blocks.push(Block::Paragraph(inlines));
        }
    }

    fn flush_paragraph(&mut self) {
        let inlines = std::mem::take(&mut self.inlines);
        self.push_paragraph(inlines);
        self.start_boundary = true;
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush_paragraph();
        self.blocks
    }

    fn sub_blocks(
        &mut self,
        elem: &Element,
        delta: StyleDelta,
    ) -> Result<Vec<Block>, ConvertError> {
        self.sub_blocks_at(elem, delta, true)
    }

    /// Sub-walk starting at the given whitespace-boundary state (`false`
    /// when the sub-content continues an inline run, so its leading space
    /// stays significant relative to the surrounding text).
    fn sub_blocks_at(
        &mut self,
        elem: &Element,
        delta: StyleDelta,
        start_boundary: bool,
    ) -> Result<Vec<Block>, ConvertError> {
        walk_blocks(self.parse_ctx, self.css, self.ctx, elem, delta, start_boundary)
    }

    /// Element-level props: matching stylesheet rules and the inline `style`
    /// attribute merged in one cascade order — normal rules, inline style,
    /// `!important` rules, `!important` inline style.
    fn element_props(&self, elem: &Element) -> Result<StyleProps, ConvertError> {
        let classes: Vec<&str> =
            elem.attr_any("class").map(|c| c.split_whitespace().collect()).unwrap_or_default();
        let mut entries = self.css.matching_rules(self.parse_ctx, &elem.local, classes)?;
        if let Some(style) = elem.attr_any("style") {
            let decls = parse_declarations(style.split(';'));
            entries.push((INLINE_PRIORITY, decls.normal));
            entries.push((IMPORTANT_PRIORITY + INLINE_PRIORITY, decls.important));
        }
        entries.sort_by_key(|&(priority, _)| priority); // stable: keeps source order
        Ok(entries.into_iter().fold(StyleProps::default(), |props, (_, entry)| props.merge(entry)))
    }

    fn hides(&self, elem: &Element) -> Result<bool, ConvertError> {
        Ok(self.element_props(elem)?.hidden == Some(true))
    }

    fn push_anchor(&mut self, elem: &Element) {
        let name = elem.attr_any("name").filter(|_| elem.local == "a");
        for raw in [elem.attr_any("id"), name].into_iter().flatten().filter(|v| !v.is_empty()) {
            self.inlines.push(Inline::Anchor(self.ctx.anchor_id(raw)));
        }
    }

    fn walk_children(&mut self, elem: &Element, delta: StyleDelta) -> Result<(), ConvertError> {
        for node in &elem.children {
            match node {
                Node::Text(t) => self.push_text(t, delta),
                Node::Elem(e) => self.walk_elem(e, delta)?,
            }
        }
        Ok(())
    }

    fn push_text(&mut self, text: &str, delta: StyleDelta) {
        let mut text = collapse_ws(&clean_text(text));
        // Collapse across node boundaries: leading whitespace vanishes at a
        // block start and after already-emitted whitespace, no matter how
        // the source split its text nodes and inline elements.
        if text.starts_with(' ') && at_space_boundary(&self.inlines, self.start_boundary) {
            text.remove(0);
        }
        if text.is_empty() {
            return;
        }
        self.inlines.push(Inline::Text { text, style: delta.resolve() });
    }

    fn walk_elem(&mut self, elem: &Element, delta: StyleDelta) -> Result<(), ConvertError> {
        let props = self.element_props(elem)?;
        if props.hidden == Some(true) {
            if holds_content(elem) {
                self.parse_ctx.emit(DiagnosticCode::HiddenOmitted, Region::Body);
            }
            return Ok(());
        }
        let delta = element_delta(elem, delta, props);
        match elem.local.as_str() {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.push_heading(elem, delta)?,
            "p" => {
                self.flush_paragraph();
                self.push_anchor(elem);
                let mut content = std::mem::take(&mut self.inlines);
                content.extend(self.inline_children(elem, delta)?);
                self.push_paragraph(content);
            }
            "ul" | "ol" => {
                self.flush_paragraph();
                let lists = self.parse_list(elem, delta)?;
                self.blocks.extend(lists);
            }
            "table" => self.push_table(elem, delta)?,
            "blockquote" => {
                self.flush_paragraph();
                let inner = self.sub_blocks(elem, delta)?;
                if !inner.is_empty() {
                    self.blocks.push(Block::BlockQuote(inner));
                }
            }
            "pre" => {
                self.flush_paragraph();
                let text = elem.text();
                if !text.trim().is_empty() {
                    self.blocks.push(Block::CodeBlock { lang: None, text });
                }
            }
            "hr" => {
                self.flush_paragraph();
                self.blocks.push(Block::Rule);
            }
            "math" => self.push_math(elem),
            name if is_container_tag(name) => {
                self.push_anchor(elem);
                if has_block_children(elem) {
                    self.flush_paragraph();
                    self.walk_children(elem, delta)?;
                    self.flush_paragraph();
                } else {
                    self.walk_children(elem, delta)?;
                }
            }
            name if renders_nothing(name) => {}
            _ => self.walk_inline(elem, delta)?,
        }
        Ok(())
    }

    fn push_heading(&mut self, elem: &Element, delta: StyleDelta) -> Result<(), ConvertError> {
        self.flush_paragraph();
        let level = elem.local.strip_prefix('h').and_then(|n| n.parse::<u8>().ok()).unwrap_or(1);
        let mut content = self.inline_children(elem, delta)?;
        // The heading element's own styling is how it looks, not
        // markup applied to the words; a `<b>` inside it still is.
        rebase_emphasis(&mut content, delta.resolve());
        let anchor = elem.attr_any("id").map(|id| self.ctx.anchor_id(id));
        if inlines_are_empty(&content) {
            // An empty heading still carries link targets: its own
            // id and any anchors inside.
            let kept = anchor
                .map(Inline::Anchor)
                .into_iter()
                .chain(content.into_iter().filter(|i| matches!(i, Inline::Anchor(_))))
                .collect();
            self.push_paragraph(kept);
        } else {
            self.blocks.push(Block::Heading { level, anchor, content });
        }
        Ok(())
    }

    fn push_table(&mut self, elem: &Element, delta: StyleDelta) -> Result<(), ConvertError> {
        self.flush_paragraph();
        if let Some(caption) = elem.child_elems().find(|e| e.local == "caption") {
            let content = self.inline_children(caption, delta)?;
            self.push_paragraph(content);
        }
        if let Some(table) = self.parse_table(elem, delta)? {
            self.blocks.push(table);
        }
        Ok(())
    }

    fn push_math(&mut self, elem: &Element) {
        let MathTex { tex, glyphs } = mathml_to_tex(elem);
        self.parse_ctx.emit_count(DiagnosticCode::UnmappedSymbol, Region::Body, glyphs);
        if tex.is_empty() {
            return;
        }
        if mathml_is_display(elem) {
            self.flush_paragraph();
            self.blocks.push(Block::Math(tex));
        } else {
            self.inlines.push(Inline::Math(tex));
        }
    }

    fn walk_inline(&mut self, elem: &Element, delta: StyleDelta) -> Result<(), ConvertError> {
        self.push_anchor(elem);
        match elem.local.as_str() {
            "br" => self.inlines.push(Inline::LineBreak),
            "img" | "image" => {
                let alt = clean_text(elem.attr_any("alt").unwrap_or(""));
                let src = elem.attr_any("src").or_else(|| elem.attr_any("href")).unwrap_or("");
                let source = self.ctx.image_source(src)?;
                self.inlines.push(Inline::Image {
                    origin: self.parse_ctx.image_origin(Region::Body),
                    alt,
                    source,
                });
            }
            "a" => {
                let target = elem.attr_any("href").and_then(|href| self.ctx.link_target(href));
                let content = self.inline_children_at(
                    elem,
                    delta,
                    at_space_boundary(&self.inlines, self.start_boundary),
                )?;
                // An empty label still keeps a resolved target: the renderer
                // shows the URL as the link text.
                match target {
                    Some(target) => self.inlines.push(Inline::Link { content, target }),
                    None => self.inlines.extend(content),
                }
            }
            "video" | "audio" => {
                self.parse_ctx.emit(DiagnosticCode::GraphicUnread, Region::Body);
                self.walk_children(elem, delta)?;
            }
            "object" | "embed" | "iframe" => {
                self.parse_ctx.emit(DiagnosticCode::EmbeddedObject, Region::Body);
                self.walk_children(elem, delta)?;
            }
            _ => self.walk_children(elem, delta)?,
        }
        Ok(())
    }

    fn inline_children(
        &mut self,
        elem: &Element,
        delta: StyleDelta,
    ) -> Result<Vec<Inline>, ConvertError> {
        self.inline_children_at(elem, delta, true)
    }

    fn inline_children_at(
        &mut self,
        elem: &Element,
        delta: StyleDelta,
        start_boundary: bool,
    ) -> Result<Vec<Inline>, ConvertError> {
        let mut blocks = self.sub_blocks_at(elem, delta, start_boundary)?;
        if let [Block::Paragraph(inlines)] = blocks.as_mut_slice() {
            return Ok(std::mem::take(inlines));
        }
        let mut out = Vec::new();
        for (i, block) in blocks.into_iter().enumerate() {
            if i > 0 {
                out.push(Inline::LineBreak);
            }
            match block {
                Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
                    out.extend(inlines);
                }
                other => out.push(Inline::plain(collapse_ws(&other.plain_text()))),
            }
        }
        Ok(out)
    }

    /// Ordered/unordered list with `start`, `reversed`, `type`, and per-item
    /// `value`. Non-contiguous numbering splits into consecutive list blocks
    /// so the rendered numbers match the source.
    fn parse_list(
        &mut self,
        elem: &Element,
        delta: StyleDelta,
    ) -> Result<Vec<Block>, ConvertError> {
        self.report_unread(elem, &["li"], false)?;
        let lis: Vec<&Element> = elem.child_elems().filter(|e| e.local == "li").collect();
        if lis.is_empty() {
            return Ok(Vec::new());
        }
        let mut items = Vec::with_capacity(lis.len());
        for li in &lis {
            items.push(ListItem { blocks: self.sub_blocks(li, delta)?, marker_label: None });
        }
        if elem.local != "ol" {
            return Ok(vec![Block::List(List { marker: MarkerKind::Bullet, start: 1, items })]);
        }
        let marker = match elem.attr_any("type") {
            Some("a") => MarkerKind::LowerAlpha,
            Some("A") => MarkerKind::UpperAlpha,
            Some("i") => MarkerKind::LowerRoman,
            Some("I") => MarkerKind::UpperRoman,
            _ => MarkerKind::Decimal,
        };
        let numbers = item_numbers(elem, &lis);
        // Zero/negative numbers are valid ordered-list values but cannot be
        // a `start` for the renderer's start+index numbering; such lists
        // carry every number as an explicit literal marker instead.
        if numbers.iter().any(|&n| n < 1) {
            for (item, n) in items.iter_mut().zip(&numbers) {
                item.marker_label = Some(format!("{n}."));
            }
            return Ok(vec![Block::List(List { marker, start: 1, items })]);
        }
        let mut items = items.into_iter();
        Ok(numbers
            .chunk_by(|a, b| a.checked_add(1) == Some(*b))
            .map(|run| {
                let start = run[0].cast_unsigned();
                Block::List(List { marker, start, items: items.by_ref().take(run.len()).collect() })
            })
            .collect())
    }

    /// Reports each child of a list, table, row group or row that holds
    /// content outside the children it reads (`read`): reading systems
    /// render it unless it, or `elem` when `hidden`, is under `display: none`.
    fn report_unread(
        &self,
        elem: &Element,
        read: &[&str],
        hidden: bool,
    ) -> Result<(), ConvertError> {
        for node in &elem.children {
            let shown = match node {
                Node::Text(text) if !text.trim().is_empty() => !hidden,
                Node::Elem(child)
                    if !read.contains(&child.local.as_str()) && holds_content(child) =>
                {
                    !hidden && has_content(child, |e| self.hides(e))?
                }
                _ => continue,
            };
            let code =
                if shown { DiagnosticCode::BlockUnread } else { DiagnosticCode::HiddenOmitted };
            self.parse_ctx.emit(code, Region::Body);
        }
        Ok(())
    }

    /// Rows as (row, in `thead`, row-group index): each `thead`/`tbody`/`tfoot` is
    /// one group, and consecutive direct `tr` children form an implicit one.
    fn table_rows<'a>(
        &self,
        elem: &'a Element,
    ) -> Result<Vec<(&'a Element, bool, usize)>, ConvertError> {
        let mut rows = Vec::new();
        let mut group = 0usize;
        let mut in_implicit_group = false;
        self.report_unread(
            elem,
            &["thead", "tbody", "tfoot", "tr", "caption", "colgroup", "col"],
            false,
        )?;
        for child in elem.child_elems() {
            match child.local.as_str() {
                "thead" | "tbody" | "tfoot" => {
                    if in_implicit_group {
                        in_implicit_group = false;
                        group += 1;
                    }
                    let hidden = self.hides(child)?;
                    self.report_unread(child, &["tr"], hidden)?;
                    for tr in child.child_elems().filter(|e| e.local == "tr") {
                        let row_hidden = hidden || self.hides(tr)?;
                        self.report_unread(tr, &["td", "th"], row_hidden)?;
                        rows.push((tr, child.local == "thead", group));
                    }
                    group += 1;
                }
                "tr" => {
                    in_implicit_group = true;
                    let hidden = self.hides(child)?;
                    self.report_unread(child, &["td", "th"], hidden)?;
                    rows.push((child, false, group));
                }
                _ => {}
            }
        }
        Ok(rows)
    }

    fn parse_table(
        &mut self,
        elem: &Element,
        delta: StyleDelta,
    ) -> Result<Option<Block>, ConvertError> {
        let row_elems = self.table_rows(elem)?;
        if row_elems.is_empty() {
            return Ok(None);
        }
        let group_last_row: HashMap<usize, usize> =
            row_elems.iter().enumerate().map(|(i, &(_, _, group))| (group, i)).collect();
        let mut builder = GridBuilder::new(self.parse_ctx);
        let mut header_rows = 0usize;
        for (i, &(tr, in_head, group)) in row_elems.iter().enumerate() {
            builder.next_row();
            let mut all_th = true;
            let mut any_cell = false;
            for cell in tr.child_elems() {
                if !matches!(cell.local.as_str(), "td" | "th") {
                    continue;
                }
                any_cell = true;
                if cell.local != "th" {
                    all_th = false;
                }
                // HTML clamps colspan to 1000 and rowspan to 65534.
                let col_span: u32 = cell
                    .attr_any("colspan")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1)
                    .clamp(1, 1000);
                let row_span: u32 = match cell.attr_any("rowspan").and_then(|v| v.parse().ok()) {
                    Some(0) => u32::try_from(group_last_row[&group] - i + 1).unwrap_or(u32::MAX),
                    Some(n) => n.clamp(1, 65534),
                    None => 1,
                };
                let blocks = self.sub_blocks(cell, delta)?;
                builder.place(Cell::spanning(blocks, col_span, row_span))?;
            }
            if i == header_rows && (in_head || (all_th && any_cell)) {
                header_rows += 1;
            }
        }
        let mut table = builder.finish()?;
        if table.grid.is_empty() {
            return Ok(None);
        }
        table.header_rows = resolve_header_rows(&table, header_rows);
        Ok(Some(Block::Table(table)))
    }
}

fn item_numbers(list: &Element, items: &[&Element]) -> Vec<i64> {
    let reversed = list.attr_any("reversed").is_some();
    let mut next: i64 = list
        .attr_any("start")
        .and_then(|v| v.parse().ok())
        .unwrap_or(if reversed { i64::try_from(items.len()).unwrap_or(i64::MAX) } else { 1 });
    let mut numbers = Vec::with_capacity(items.len());
    for li in items {
        if let Some(v) = li.attr_any("value").and_then(|v| v.parse::<i64>().ok()) {
            next = v;
        }
        numbers.push(next);
        // Source-controlled values sit anywhere in the i64 range; the
        // step must not overflow.
        next = if reversed { next.saturating_sub(1) } else { next.saturating_add(1) };
    }
    numbers
}

/// Elements whose content no reading system renders in the page.
fn renders_nothing(name: &str) -> bool {
    matches!(name, "script" | "style" | "head" | "template" | "noscript")
}

fn holds_content(elem: &Element) -> bool {
    // The walk fails only through `hides`, which never does here.
    matches!(has_content(elem, |_| Ok(false)), Ok(true))
}

fn has_content(
    elem: &Element,
    mut hides: impl FnMut(&Element) -> Result<bool, ConvertError>,
) -> Result<bool, ConvertError> {
    let mut stack = vec![elem];
    while let Some(elem) = stack.pop() {
        if renders_nothing(&elem.local) || hides(elem)? {
            continue;
        }
        if matches!(elem.local.as_str(), "img" | "image") {
            return Ok(true);
        }
        for node in &elem.children {
            match node {
                Node::Text(text) if !text.trim().is_empty() => return Ok(true),
                Node::Text(_) => {}
                Node::Elem(child) => stack.push(child),
            }
        }
    }
    Ok(false)
}

/// Cascade order: the inherited delta, then the tag's presentational default (`<b>`, `<i>`,
/// …), then CSS, so `font-weight: normal` can undo a `<b>`. A shift is the element's own, its
/// CSS else its tag's, and only a raise or a drop overrides the one it inherits; a block
/// element's `vertical-align` shifts nothing, as CSS applies it to inline-level boxes only.
fn element_delta(elem: &Element, inherited: StyleDelta, props: StyleProps) -> StyleDelta {
    let mut delta = inherited;
    let mut tag_shift = None;
    match elem.local.as_str() {
        "b" | "strong" => delta.bold = Some(true),
        "i" | "em" | "cite" | "dfn" | "var" => delta.italic = Some(true),
        "s" | "del" | "strike" => delta.strike = Some(true),
        "code" | "kbd" | "samp" | "tt" => delta.code = Some(true),
        "sup" => tag_shift = Some(VerticalPosition::Superscript),
        "sub" => tag_shift = Some(VerticalPosition::Subscript),
        _ => {}
    }
    let mut delta = delta.merge(props.delta);
    let declared = props.vertical_align.filter(|_| !is_block_tag(&elem.local));
    match declared.or(tag_shift) {
        None | Some(VerticalPosition::Baseline) => {}
        shift => delta.position = shift,
    }
    delta
}

/// Elements that only group other content, walked through transparently.
// rustfmt would set each of these long names on its own line.
#[rustfmt::skip]
const CONTAINER_TAGS: [&str; 18] = [
    "div", "section", "article", "aside", "main", "nav", "header", "footer", "figure",
    "figcaption", "center", "details", "summary", "li", "dl", "dt", "dd", "body",
];

const BLOCK_ONLY_TAGS: [&str; 13] =
    ["p", "ul", "ol", "table", "blockquote", "pre", "hr", "h1", "h2", "h3", "h4", "h5", "h6"];

fn is_container_tag(name: &str) -> bool {
    CONTAINER_TAGS.contains(&name)
}

fn is_block_tag(name: &str) -> bool {
    is_container_tag(name) || BLOCK_ONLY_TAGS.contains(&name)
}

fn has_block_children(elem: &Element) -> bool {
    elem.child_elems().any(|e| is_block_tag(&e.local))
}

#[cfg(test)]
#[expect(clippy::format_collect, reason = "fixture HTML is built with format!")]
mod tests {
    use super::*;
    use crate::model::{CellSlot, Style};
    use crate::package::archive::Role;
    use crate::package::xml::parse_xml;
    use crate::shared::test_support::{cancelled, diagnostic_rows};
    use std::assert_matches;

    struct NullCtx;

    impl HtmlCtx for NullCtx {
        fn link_target(&self, href: &str) -> Option<LinkTarget> {
            (!href.is_empty()).then(|| LinkTarget::External(href.to_string()))
        }

        fn image_source(&self, _src: &str) -> Result<ImageSource, ConvertError> {
            Ok(ImageSource::Unavailable)
        }

        fn anchor_id(&self, raw: &str) -> AnchorId {
            raw.to_string()
        }
    }

    fn convert(
        html: &str,
        css: &str,
        parse_ctx: &ParseCtx<'_>,
    ) -> Result<Vec<Block>, ConvertError> {
        convert_sheets(html, &[("", css)], parse_ctx)
    }

    fn convert_sheets(
        html: &str,
        sheets: &[(&str, &str)],
        parse_ctx: &ParseCtx<'_>,
    ) -> Result<Vec<Block>, ConvertError> {
        let tree = parse_xml(&ParseCtx::default(), html.as_bytes(), Role::Probe).unwrap();
        let body = tree.child_elems().next().unwrap();
        let mut sheet = Stylesheet::default();
        for &(media, css) in sheets {
            sheet.add(&ParseCtx::default(), css, media).unwrap();
        }
        to_blocks(parse_ctx, body, &sheet, &NullCtx)
    }

    fn blocks_with_css(html: &str, css: &str) -> Vec<Block> {
        convert(html, css, &ParseCtx::default()).unwrap()
    }

    fn blocks(html: &str) -> Vec<Block> {
        blocks_with_css(html, "")
    }

    #[test]
    fn an_element_charges_the_rules_its_tag_and_classes_name() {
        let mut sheet = Stylesheet::default();
        let rules: String = (0..1000).map(|i| format!(".c{i} {{ font-weight: bold }} ")).collect();
        let css = format!("{rules} p {{ font-style: italic }} span.c7 {{ display: none }}");
        sheet.add(&ParseCtx::default(), &css, "").unwrap();
        let parse_ctx = ParseCtx::default();
        let found =
            sheet.matching_rules(&parse_ctx, "p", vec!["c7", "c9", "c7", "nowhere"]).unwrap();
        assert_eq!(parse_ctx.fuel(), 4);
        let props: Vec<_> =
            found.iter().map(|(priority, props)| (*priority, props.delta)).collect();
        let bold = StyleDelta { bold: Some(true), ..StyleDelta::default() };
        let italic = StyleDelta { italic: Some(true), ..StyleDelta::default() };
        assert_eq!(props, [(10, bold), (10, bold), (1, italic)]);
        let limits = crate::limits::Limits { max_work: 3, ..crate::limits::Limits::default() };
        let refused = sheet.matching_rules(&ParseCtx::new(&limits), "p", vec!["c7", "c9"]);
        assert_matches!(
            refused,
            Err(ConvertError::ResourceLimit { limit: crate::limits::Limit::Work, max: 3 })
        );
        let token = crate::ledger::CancelToken::new();
        let cancelled = cancelled(&token);
        let refused = sheet.matching_rules(&cancelled, "p", vec!["c7", "c9"]);
        assert_matches!(refused, Err(ConvertError::Cancelled));
    }

    #[test]
    fn table_spans_charge_the_contexts_expansion_budget() {
        let html = r#"<body><table><tr><td colspan="3">wide</td></tr></table></body>"#;
        let mut limits =
            crate::limits::Limits { max_expansion: 1, ..crate::limits::Limits::default() };
        let err = convert(html, "", &ParseCtx::new(&limits));
        assert_matches!(
            err,
            Err(ConvertError::ResourceLimit { limit: crate::limits::Limit::Expansion, max: 1 })
        );
        limits.max_expansion = 2;
        convert(html, "", &ParseCtx::new(&limits)).unwrap();
    }

    type Reported = (DiagnosticCode, Region, u64);

    fn reported(parse_ctx: &ParseCtx<'_>) -> Vec<Reported> {
        diagnostic_rows(&parse_ctx.diagnostics())
    }

    fn diagnose(html: &str, css: &str) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        convert(html, css, &parse_ctx).unwrap();
        reported(&parse_ctx)
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of unread and hidden elements")]
    fn content_left_unread_or_hidden_reports_once_per_element() {
        use DiagnosticCode::{
            BlockUnread, EmbeddedObject, GraphicUnread, HiddenOmitted, UnmappedSymbol,
        };
        const BODY: Region = Region::Body;
        let cases: &[(&str, &str, &str, &[Reported])] = &[
            (
                "hidden paragraph",
                r#"<p style="display: none">x</p>"#,
                "",
                &[(HiddenOmitted, BODY, 1)],
            ),
            (
                "hidden picture",
                r#"<div style="display: none"><img src="a.png"/></div>"#,
                "",
                &[(HiddenOmitted, BODY, 1)],
            ),
            (
                "hidden by a rule",
                r#"<p class="h">x</p><p class="h">y</p>"#,
                ".h { display: none }",
                &[(HiddenOmitted, BODY, 2)],
            ),
            ("hidden and empty", r#"<div style="display: none"> <br/></div>"#, "", &[]),
            ("hidden template", "<template><p>x</p></template>", "template { display: none }", &[]),
            (
                "hidden script only",
                r#"<div style="display: none"><script>x</script></div>"#,
                "",
                &[],
            ),
            (
                "list in a list",
                "<ul><li>a</li><ul><li>b</li></ul></ul>",
                "",
                &[(BlockUnread, BODY, 1)],
            ),
            ("paragraph in a list", "<ol><li>a</li><p>b</p></ol>", "", &[(BlockUnread, BODY, 1)]),
            ("text in a list", "<ul><li>a</li>b</ul>", "", &[(BlockUnread, BODY, 1)]),
            ("list without items", "<ul><div>a</div></ul>", "", &[(BlockUnread, BODY, 1)]),
            (
                "hidden child of a list",
                r#"<ul><p class="h">x</p><li>a</li></ul>"#,
                ".h { display: none }",
                &[(HiddenOmitted, BODY, 1)],
            ),
            (
                "hidden content in a division in a list",
                r#"<ul><div><p style="display: none">x</p></div><li>a</li></ul>"#,
                "",
                &[(HiddenOmitted, BODY, 1)],
            ),
            (
                "hidden text beside shown text in a list",
                r#"<ul><li>a</li><div>b<span style="display: none">x</span></div></ul>"#,
                "",
                &[(BlockUnread, BODY, 1)],
            ),
            (
                "script, space and a break in a list",
                "<ul> <script>x</script><br/><li>a</li></ul>",
                "",
                &[],
            ),
            (
                "paragraph in a table",
                "<table><p>x</p><tr><td>a</td></tr></table>",
                "",
                &[(BlockUnread, BODY, 1)],
            ),
            (
                "text in a row group",
                "<table><tbody>x<tr><td>a</td></tr></tbody></table>",
                "",
                &[(BlockUnread, BODY, 1)],
            ),
            (
                "division in a row",
                "<table><tr><div>x</div><td>a</td></tr></table>",
                "",
                &[(BlockUnread, BODY, 1)],
            ),
            (
                "division in a hidden row group",
                r#"<table><tbody style="display: none"><div>x</div><tr><td>a</td></tr></tbody></table>"#,
                "",
                &[(HiddenOmitted, BODY, 1)],
            ),
            (
                "text in a row of a hidden row group",
                r#"<table><tbody class="h"><tr>x<td>a</td></tr></tbody></table>"#,
                ".h { display: none }",
                &[(HiddenOmitted, BODY, 1)],
            ),
            (
                "division in a hidden row",
                r#"<table><tr style="display: none"><div>x</div><td>a</td></tr></table>"#,
                "",
                &[(HiddenOmitted, BODY, 1)],
            ),
            (
                "division in a hidden row of a row group",
                r#"<table><tbody><tr style="display: none"><div>x</div><td>a</td></tr></tbody></table>"#,
                "",
                &[(HiddenOmitted, BODY, 1)],
            ),
            ("video", r#"<video src="a.mp4">fallback</video>"#, "", &[(GraphicUnread, BODY, 1)]),
            ("audio", r#"<p><audio src="a.mp3"/></p>"#, "", &[(GraphicUnread, BODY, 1)]),
            (
                "object",
                r#"<object data="a.svg">fallback</object>"#,
                "",
                &[(EmbeddedObject, BODY, 1)],
            ),
            ("embed", r#"<embed src="a.swf"/>"#, "", &[(EmbeddedObject, BODY, 1)]),
            (
                "glyphs in math",
                "<p><math><mi>x</mi><mglyph/></math><math><mglyph/><mglyph/></math></p>",
                "",
                &[(UnmappedSymbol, BODY, 3)],
            ),
            (
                "only a glyph in display math",
                r#"<math display="block"><mglyph alt="a"/></math>"#,
                "",
                &[(UnmappedSymbol, BODY, 1)],
            ),
            ("empty math", "<p><math><mrow/></math></p>", "", &[]),
            (
                "hidden math",
                r#"<p style="display: none"><math><mi>x</mi><mglyph/></math></p>"#,
                "",
                &[(HiddenOmitted, BODY, 1)],
            ),
            ("iframe", r#"<iframe src="a.xhtml"/>"#, "", &[(EmbeddedObject, BODY, 1)]),
            (
                "caption and columns",
                "<table><caption>c</caption><colgroup><col/></colgroup><thead><tr><th>h</th></tr></thead><tr><td>a</td></tr></table>",
                "",
                &[],
            ),
        ];
        for (name, body, css, expected) in cases {
            let reported = diagnose(&format!("<body>{body}</body>"), css);
            assert_eq!(reported, *expected, "{name}");
        }
    }

    fn para_text(block: &Block) -> String {
        let Block::Paragraph(inlines) = block else { panic!("expected paragraph: {block:?}") };
        crate::model::inlines_to_plain_text(inlines)
    }

    fn first_text_style(inlines: &[Inline]) -> Style {
        for inline in inlines {
            match inline {
                Inline::Text { style, .. } => return *style,
                Inline::Link { content, .. } => return first_text_style(content),
                _ => {}
            }
        }
        panic!("no text run in {inlines:?}");
    }

    fn first_style(html: &str, css: &str) -> Style {
        let out = blocks_with_css(html, css);
        let Block::Paragraph(inlines) = &out[0] else { panic!("{out:?}") };
        first_text_style(inlines)
    }

    fn shifts(html: &str, css: &str) -> Vec<(String, VerticalPosition)> {
        let out = blocks_with_css(&format!("<body><div>{html}</div></body>"), css);
        let Block::Paragraph(inlines) = &out[0] else { panic!("{out:?}") };
        let mut runs = Vec::new();
        let mut stack: Vec<&Inline> = inlines.iter().rev().collect();
        while let Some(inline) = stack.pop() {
            match inline {
                Inline::Text { text, style } => runs.push((text.clone(), style.position)),
                Inline::Link { content, .. } => stack.extend(content.iter().rev()),
                _ => {}
            }
        }
        runs
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of shifting tags and styles")]
    fn a_tag_or_a_vertical_align_shifts_and_baseline_cancels_only_its_own_element() {
        use VerticalPosition::{Baseline, Subscript, Superscript};
        let runs = |pairs: &[(&str, VerticalPosition)]| -> Vec<(String, VerticalPosition)> {
            pairs.iter().map(|&(text, position)| (text.to_string(), position)).collect()
        };
        type Case<'a> = (&'a str, &'a str, &'a str, &'a [(&'a str, VerticalPosition)]);
        let cases: &[Case<'_>] = &[
            (
                "tags",
                "H<sub>2</sub>O<sup>+</sup>",
                "",
                &[("H", Baseline), ("2", Subscript), ("O", Baseline), ("+", Superscript)],
            ),
            (
                "span",
                r#"x<span style="vertical-align: super">2</span>"#,
                "",
                &[("x", Baseline), ("2", Superscript)],
            ),
            (
                "class",
                r#"x<span class="lo">2</span>"#,
                ".lo { vertical-align: sub }",
                &[("x", Baseline), ("2", Subscript)],
            ),
            (
                "tag cancelled",
                "a<sup>b</sup>",
                "sup { vertical-align: baseline }",
                &[("a", Baseline), ("b", Baseline)],
            ),
            (
                "css over tag",
                "a<sup>b</sup>",
                "sup { vertical-align: sub }",
                &[("a", Baseline), ("b", Subscript)],
            ),
            (
                "important",
                r#"a<sup class="up">b</sup>"#,
                "sup { vertical-align: baseline !important } .up { vertical-align: sub }",
                &[("a", Baseline), ("b", Baseline)],
            ),
            (
                "reset sheet",
                r#"a<sup><a href="u">b</a><span>c</span></sup>"#,
                "span, a { vertical-align: baseline }",
                &[("a", Baseline), ("b", Superscript), ("c", Superscript)],
            ),
            (
                "other values",
                r#"a<sup style="vertical-align: top">b</sup><span style="vertical-align: 33%">c</span>"#,
                "",
                &[("a", Baseline), ("b", Superscript), ("c", Baseline)],
            ),
            (
                "innermost",
                "x<sup>a<sub>b</sub></sup>",
                "",
                &[("x", Baseline), ("a", Superscript), ("b", Subscript)],
            ),
            (
                "division",
                r#"<div class="x"><p>div child <sup>2</sup></p></div>"#,
                ".x { vertical-align: sub }",
                &[("div child ", Baseline), ("2", Superscript)],
            ),
            (
                "paragraph",
                r#"<p style="vertical-align: super">para text</p>"#,
                "",
                &[("para text", Baseline)],
            ),
        ];
        for &(name, html, css, expected) in cases {
            assert_eq!(shifts(html, css), runs(expected), "{name}");
        }
    }

    #[test]
    fn a_nested_shift_renders_flattened_to_the_innermost() {
        let out = blocks("<body><p>x<sup>a<sub>b</sub>c</sup></p></body>");
        let markdown = crate::render::markdown::document_to_markdown(&crate::model::Document {
            blocks: out,
            ..crate::model::Document::default()
        });
        assert_eq!(markdown, "x<sup>a</sup><sub>b</sub><sup>c</sup>\n");
    }

    #[test]
    fn a_rule_declaring_only_a_vertical_align_is_kept() {
        let mut sheet = Stylesheet::default();
        let css = ".up { vertical-align: super } .flat { vertical-align: baseline !important }";
        sheet.add(&ParseCtx::default(), css, "").unwrap();
        let parse_ctx = ParseCtx::default();
        assert_eq!(sheet.matching_rules(&parse_ctx, "span", vec!["up", "flat"]).unwrap().len(), 2);
    }

    #[test]
    fn whitespace_collapses_across_inline_boundaries() {
        let out = blocks("<body><p>foo<span> </span>bar</p></body>");
        assert_eq!(para_text(&out[0]), "foo bar");
        let out = blocks("<body><p>foo <span> bar</span></p></body>");
        assert_eq!(para_text(&out[0]), "foo bar");
        let out = blocks("<body><p>  foo\n  bar  </p></body>");
        assert_eq!(para_text(&out[0]), "foo bar ");
        let out = blocks("<body>  foo</body>");
        assert_eq!(para_text(&out[0]), "foo");
        let Block::Paragraph(inlines) = &blocks("<body><p> <i>x</i></p></body>")[0] else {
            panic!()
        };
        assert_eq!(inlines.len(), 1);
    }

    #[test]
    fn link_label_whitespace_joins_the_surrounding_run() {
        let out = blocks(r#"<body><p>foo <a href="u"> bar</a></p></body>"#);
        let Block::Paragraph(inlines) = &out[0] else { panic!() };
        let Some(Inline::Link { content, .. }) =
            inlines.iter().find(|i| matches!(i, Inline::Link { .. }))
        else {
            panic!("no link in {inlines:?}");
        };
        assert_eq!(crate::model::inlines_to_plain_text(content), "bar");
        let out = blocks(r#"<body><p>foo<a href="u"> bar</a></p></body>"#);
        assert_eq!(para_text(&out[0]), "foo bar");
        let out = blocks(r#"<body><div>foo<a href="u"><p>x</p> y</a></div></body>"#);
        assert_eq!(para_text(&out[0]), "foox\ny");
    }

    #[test]
    fn inline_style_overrides_the_tag_default() {
        let style = first_style(r#"<body><p><b style="font-weight: normal">x</b></p></body>"#, "");
        assert!(!style.bold);
    }

    #[test]
    fn important_declarations_win_the_cascade() {
        let css = "span.x { font-weight: bold !important } span.x { font-weight: normal }";
        assert!(first_style(r#"<body><p><span class="x">x</span></p></body>"#, css).bold);
    }

    #[test]
    fn a_bang_that_is_not_important_stays_in_the_normal_tier() {
        let css = "span.x { font-weight: bold !ie } span.x { font-weight: normal }";
        assert!(!first_style(r#"<body><p><span class="x">x</span></p></body>"#, css).bold);
    }

    #[test]
    fn css_comments_hide_what_they_enclose_to_their_end_or_the_sheets() {
        for css in [
            "span.x { font-weight: bold } /* span.x { font-weight: normal } */",
            "span.x { font-weight: bold } /* span.x { font-weight: normal }",
            "span.x { font-weight: bold /* normal */ } /* a */ /* b */",
            "span.x { font-weight: bold } /*/ span.x { font-weight: normal } */",
        ] {
            assert!(
                first_style(r#"<body><p><span class="x">x</span></p></body>"#, css).bold,
                "{css}"
            );
        }
    }

    const MARKED: &str = r#"<body><p>before</p><p class="x">T</p><p>after</p></body>"#;
    const STYLED: &str = r#"<body><p><span class="x">x</span></p></body>"#;

    fn hides_in(sheets: &[(&str, &str)]) -> bool {
        let parse_ctx = ParseCtx::default();
        let blocks = convert_sheets(MARKED, sheets, &parse_ctx).unwrap();
        let shown = blocks.iter().map(para_text).any(|text| text == "T");
        let expected =
            if shown { Vec::new() } else { vec![(DiagnosticCode::HiddenOmitted, Region::Body, 1)] };
        assert_eq!(reported(&parse_ctx), expected, "{sheets:?}");
        !shown
    }

    fn hides(css: &str) -> bool {
        hides_in(&[("", css)])
    }

    fn assert_hides(cases: &[(&str, &str, bool)]) {
        for &(name, css, expected) in cases {
            assert_eq!(hides(css), expected, "{name}");
        }
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of media groups")]
    fn a_rule_in_a_media_group_hides_text_only_where_a_screen_applies_it() {
        assert_hides(&[
            ("no group", ".x { display: none }", true),
            ("a screen group", "@media screen { .x { display: none } }", true),
            (
                "a screen group after another rule in it",
                "@media screen { h1 { font-weight: bold } .x { display: none } }",
                true,
            ),
            ("all", "@media all { .x { display: none } }", true),
            ("only screen", "@media only screen { .x { display: none } }", true),
            ("no query", "@media { .x { display: none } }", true),
            ("capitals", "@MEDIA SCREEN { .x { display: none } }", true),
            ("no space before the block", "@media screen{.x{display:none}}", true),
            ("a list with a screen", "@media print, screen { .x { display: none } }", true),
            ("not print", "@media not print { .x { display: none } }", true),
            ("a print group", "@media print { .x { display: none } }", false),
            (
                "a print group after another rule in it",
                "@media print { h1 { font-weight: bold } .x { display: none } }",
                false,
            ),
            ("a speech group", "@media speech { .x { display: none } }", false),
            ("not screen", "@media not screen { .x { display: none } }", false),
            ("print and a feature", "@media print and (color) { .x { display: none } }", false),
            ("print and speech", "@media print, speech { .x { display: none } }", false),
            ("a malformed query", "@media screen and { .x { display: none } }", false),
            (
                "a print group that does not undo an earlier hide",
                ".x { display: none } @media print { h1 { font-weight: bold } .x { display: block } }",
                true,
            ),
            (
                "a screen group that restores",
                ".x { display: none } @media screen { .x { display: block } }",
                false,
            ),
            (
                "a nested print group",
                "@media screen { @media print { .x { display: none } } }",
                false,
            ),
            (
                "a nested screen group",
                "@media screen { @media screen { .x { display: none } } }",
                true,
            ),
            (
                "a screen group in a print group",
                "@media print { @media screen { .x { display: none } } }",
                false,
            ),
            (
                "a rule after a print group",
                "@media print { h1 { font-weight: bold } } .x { display: none }",
                true,
            ),
            (
                "a rule after a nested group",
                "@media screen { @media print { h1 { color: red } } .x { display: none } }",
                true,
            ),
        ]);
    }

    #[test]
    fn an_unsettled_group_contributes_only_a_display_that_shows() {
        let groups = [
            "@media (min-width: 1px)",
            "@media screen and (color)",
            "@media not (color)",
            "@supports (display: grid)",
            "@layer base",
            "@layer",
            "@container (min-width: 1px)",
            "@scope (.a)",
        ];
        for group in groups {
            let hiding = format!("{group} {{ .x {{ display: none }} }}");
            let showing = format!(".x {{ display: none }} {group} {{ .x {{ display: block }} }}");
            assert!(!hides(&hiding), "{group}");
            assert!(!hides(&showing), "{group}");
        }
        assert_hides(&[
            (
                "unsettled in a screen group",
                "@media screen { @supports (a: b) { .x { display: none } } }",
                false,
            ),
            (
                "a screen group in an unsettled group",
                "@supports (a: b) { @media screen { .x { display: none } } }",
                false,
            ),
            (
                "a print group in an unsettled group",
                "@supports (a: b) { @media print { .x { display: block } } } .x { display: none }",
                true,
            ),
            (
                "a display that shows",
                ".x { display: none } @supports (a: b) { .x { display: block } }",
                false,
            ),
            ("an important none", "@supports (a: b) { .x { display: none !important } }", false),
            (
                "an important display that shows",
                ".x { display: none !important } @supports (a: b) { .x { display: block !important } }",
                false,
            ),
            (
                "a rule after the group",
                "@supports (a: b) { h1 { color: red } } .x { display: none }",
                true,
            ),
        ]);
        let bold = |css| first_style(STYLED, css).bold;
        assert!(bold("@media screen { h1 { color: red } span.x { font-weight: bold } }"));
        assert!(!bold("@media print { span.x { font-weight: bold } }"));
        assert!(!bold("@supports (a: b) { span.x { font-weight: bold } }"));
        assert!(!bold("@media (color) { span.x { font-weight: bold !important } }"));
    }

    #[test]
    fn a_group_that_styles_nothing_contributes_nothing() {
        assert_hides(&[
            ("starting-style", "@starting-style { .x { display: none } }", false),
            ("page", "@page { .x { display: none } }", false),
            ("keyframes", "@keyframes k { .x { display: none } }", false),
            ("font-face", "@font-face { .x { display: none } }", false),
            ("an unknown at-rule", "@unknown { .x { display: none } }", false),
            (
                "a name that only starts like a group",
                ".x { display: none } @layer-x { .x { display: block } }",
                true,
            ),
            (
                "a font-face before a rule",
                "@font-face { font-family: f; src: url(f.woff) } .x { display: none }",
                true,
            ),
            (
                "a keyframes before a rule",
                "@keyframes k { from { color: red } to { color: blue } } .x { display: none }",
                true,
            ),
        ]);
    }

    #[test]
    fn a_statement_at_rule_ends_at_its_semicolon() {
        assert_hides(&[
            ("charset", "@charset \"utf-8\";\n.x { display: none }", true),
            (
                "namespace",
                "@namespace epub \"http://www.idpf.org/2007/ops\";\n.x { display: none }",
                true,
            ),
            ("import of a url", "@import url(a.css);\n.x { display: none }", true),
            ("import with a media list", "@import \"a.css\" screen;\n.x { display: none }", true),
            ("a layer list", "@layer a, b;\n.x { display: none }", true),
            ("several", "@charset \"utf-8\"; @import url(a.css); .x { display: none }", true),
            (
                "a semicolon and a brace in a string",
                "@import \"a;b{c.css\"; .x { display: none }",
                true,
            ),
            (
                "before a print group",
                "@charset \"utf-8\"; @media print { .x { display: none } }",
                false,
            ),
            ("inside a group", "@media screen { @import \"a.css\"; .x { display: none } }", true),
            (
                "inside a print group",
                "@media print { @import \"a.css\"; .x { display: none } } .x { display: block }",
                false,
            ),
            ("a stray semicolon after a rule", ".y { color: red }; .x { display: none }", false),
            ("a stray semicolon with no space", ".y{color:red};.x{display:none}", false),
            ("a semicolon that starts the sheet", ";.x { display: none }", false),
            ("text before a semicolon", "junk; .x { display: none }", false),
            (
                "a stray semicolon in a group",
                "@media screen { .y { color: red } ; .x { display: none } }",
                true,
            ),
            ("a semicolon in an import's url", "@import url(a;b.css); .x { display: none }", true),
        ]);
    }

    #[test]
    fn a_backslash_escapes_the_byte_after_it_unless_a_replacement_character_precedes_it() {
        assert_hides(&[
            ("an escaped close", "@media print { .p\\} .x { display: none } }", false),
            ("an escaped quote", ".q\\\" { color: red } .x { display: none }", true),
            (
                "an escaped line break in a string",
                ".y { content: \"a\\\r\nb}\" } .x { display: none }",
                true,
            ),
            (
                "an escaped carriage return in a string",
                ".y { content: \"a\\\rb}\" } .x { display: none }",
                true,
            ),
            (
                "an escaped lone carriage return before a block",
                "@media print { .a\\\r{ } .x { display: none } }",
                false,
            ),
            (
                "an escaped lone carriage return before a close",
                ".a { x: 1 \\\r} .x { display: none }",
                true,
            ),
            (
                "an escaped lone carriage return before a semicolon",
                "@charset \"c\" \\\r; .x { display: none }",
                true,
            ),
            (
                "a trail byte in a string",
                ".a { content: \"\u{fffd}\\\" } .x { display: none }",
                true,
            ),
            (
                "a trail byte outside a string",
                "@media print { .p\u{fffd}\\{ color: red } .x { display: none } }",
                false,
            ),
            (
                "an escape after another character ending in the same byte",
                ".y { content: \"\u{bd}\\\" } .x { display: none }",
                false,
            ),
        ]);
    }

    #[test]
    fn an_unquoted_url_keeps_its_punctuation_out_of_the_structure() {
        assert_hides(&[
            ("an open brace", ".x { background: url(foo{bar); display: none }", true),
            ("capitals", ".x { background: URL(foo{bar); display: none }", true),
            ("white space first", ".x { background: url( a{b ); display: none }", true),
            (
                "a close brace in a print group",
                "@media print { .y { background: url(a}b) } .x { display: none } }",
                false,
            ),
            (
                "an escaped parenthesis",
                "@media print { .y { a: url(a\\)}b) } .x { display: none } }",
                false,
            ),
            ("a quoted url", ".y { a: url(\"a)}\") } .x { display: none }", true),
            (
                "the other quote after white space",
                ".y { a: url( 'a)}') } .x { display: none }",
                true,
            ),
            ("a function ending in url", ".x { a: xurl(a{b); display: none }", false),
            ("a hyphen before url", ".x { a: -url(a{b); display: none }", false),
            ("an underscore before url", ".x { a: _url(a{b); display: none }", false),
        ]);
    }

    #[test]
    fn a_quoted_string_keeps_its_braces_and_semicolons_out_of_the_structure() {
        assert_hides(&[
            ("a close brace", ".y { content: \"}\" } .x { display: none }", true),
            ("an open brace", ".y { content: \"{\" } .x { display: none }", true),
            ("a semicolon", ".y { content: \";\" } .x { display: none }", true),
            ("single quotes", ".y { content: '}' } .x { display: none }", true),
            ("the other quote inside", ".y { content: '\"' } .x { display: none }", true),
            ("an escaped quote", ".y { content: \"\\\"}\" } .x { display: none }", true),
            ("an escaped backslash", ".y { content: \"\\\\\" } .x { display: none }", true),
            ("a string cut by a line feed", ".y { font-family: it's\n} .x { display: none }", true),
            (
                "a string cut by a carriage return",
                ".y { font-family: it's\r} .x { display: none }",
                true,
            ),
            (
                "a string cut by a form feed",
                ".y { font-family: it's\x0c} .x { display: none }",
                true,
            ),
            ("a quoted selector", ".y[a=\"{\"] { color: red } .x { display: none }", true),
        ]);
    }

    #[test]
    fn a_rule_nested_in_a_rule_is_skipped_and_the_rule_keeps_its_own_declarations() {
        let styled = |css| first_style(STYLED, css);
        assert_hides(&[
            (
                "a nested rule after the declaration",
                ".x { display: none; &:hover { color: red } }",
                true,
            ),
            ("a nested rule that hides", ".x { & a { display: none } }", false),
            ("a declaration after a nested rule", ".x { & a { color: red } display: none }", true),
            ("a nested group", ".x { @media screen { display: none } }", false),
            ("semicolons in a nested rule", ".x { & a { display: none; color: red; } }", false),
            (
                "a nested group after the declaration",
                ".x { display: none; @media print { color: red } }",
                true,
            ),
            ("two levels", ".x { a { b { display: none } } }", false),
            (
                "a rule after the nesting",
                ".x { a { b { color: red } } } .x { display: none }",
                true,
            ),
        ]);
        let css = ".x { & b { color: red } font-weight: bold; i { font-style: italic } }";
        assert!(styled(css).bold);
        assert!(!styled(css).italic);
    }

    #[test]
    fn a_block_left_open_closes_at_the_end_of_the_sheet() {
        assert_hides(&[
            ("a rule", ".x { display: none", true),
            ("a group and its rule", "@media screen { .x { display: none }", true),
            ("a group and a rule left open", "@media screen { .x { display: none", true),
            ("a print group", "@media print { .x { display: none", false),
            ("a stray close", "} .x { display: none }", false),
            ("a stray close after a rule", ".y { color: red } } .x { display: none }", false),
            (
                "a rule after a stray close and a rule",
                "} .y { color: red } .x { display: none }",
                true,
            ),
        ]);
    }

    #[test]
    fn nesting_deeper_than_the_stack_still_reads() {
        let hidden = format!("{}.x {{ display: none }}", "@media all {".repeat(50_000));
        assert!(hides(&hidden));
        let shown =
            format!("{}.x {{ display: none }}", "@media all {".repeat(50_000) + "@media print {");
        assert!(!hides(&shown));
    }

    #[test]
    fn the_media_list_of_a_sheet_bounds_every_rule_in_it() {
        let hide = ".x { display: none }";
        let cases = [
            ("no list", "", hide, true),
            ("screen", "screen", hide, true),
            ("all", "all", hide, true),
            ("not print", "not print", hide, true),
            ("print and screen", "print, screen", hide, true),
            ("print", "print", hide, false),
            ("speech", "speech", hide, false),
            ("a feature", "(min-width: 1px)", hide, false),
            ("screen and a feature", "screen and (color)", hide, false),
            (
                "screen over a screen group",
                "screen",
                "@media screen { .x { display: none } }",
                true,
            ),
            ("screen over a print group", "screen", "@media print { .x { display: none } }", false),
            ("print over a screen group", "print", "@media screen { .x { display: none } }", false),
            (
                "a feature over a screen group",
                "(color)",
                "@media screen { .x { display: none } }",
                false,
            ),
        ];
        for (name, media, css, expected) in cases {
            assert_eq!(hides_in(&[(media, css)]), expected, "{name}");
        }
        let restored = [("", hide), ("(color)", ".x { display: block }")];
        assert!(!hides_in(&restored), "an unsettled sheet that shows");
        let kept = [("", hide), ("print", ".x { display: block }")];
        assert!(hides_in(&kept), "a print sheet that shows");
    }

    #[test]
    fn reading_a_sheet_charges_its_length() {
        use crate::limits::{Limit, Limits};
        let css = format!("@media screen {{ .h {{ display: none }} }}{}", " ".repeat(100));
        let len = css.len() as u64;
        let parse_ctx = ParseCtx::default();
        Stylesheet::default().add(&parse_ctx, &css, "screen").unwrap();
        assert_eq!(parse_ctx.fuel(), len);
        let limits = Limits { max_work: len - 1, ..Limits::default() };
        assert_matches!(
            Stylesheet::default().add(&ParseCtx::new(&limits), &css, ""),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max }) if max == len - 1
        );
        let token = crate::ledger::CancelToken::new();
        assert_matches!(
            Stylesheet::default().add(&cancelled(&token), &css, ""),
            Err(ConvertError::Cancelled)
        );
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of damaged sheets")]
    fn a_replacement_character_may_hide_through_a_declaration_name_or_a_rule_that_declares_display()
    {
        let cases = [
            ("a declaration name", ".h { disp\u{fffd}lay: none }", true),
            (
                "a name beside one that reads",
                ".h { font-weight: bold; \u{fffd}isplay: none }",
                true,
            ),
            ("a name that is no display", ".h { fo\u{fffd}nt-weight: bold }", true),
            ("the selector of a rule that declares display", ".h\u{fffd} { display: none }", true),
            ("the value of a rule that declares display", ".h { display: n\u{fffd}ne }", true),
            (
                "a value beside a display that shows",
                ".h { font-family: \"\u{fffd}\"; display: block }",
                true,
            ),
            ("a display in capitals and white space", ".h\u{fffd} { DISPLAY : none }", true),
            (
                "a rule whose comment holds a brace",
                ".a { x: 1 /* } */ ; display: none; y: \u{fffd} }",
                true,
            ),
            ("a rule left open", ".h { display: none; y: \u{fffd}", true),
            (
                "a value of a rule that declares no display",
                ".h { font-family: \"\u{fffd}\" }",
                false,
            ),
            (
                "a name that only starts with display",
                ".h { display-mode: none; y: \u{fffd} }",
                false,
            ),
            (
                "a rule before one that declares display",
                ".a { font-family: \u{fffd} } .b { display: none }",
                false,
            ),
            (
                "a rule after one that declares display",
                ".b { display: none } .a { font-family: \u{fffd} }",
                false,
            ),
            (
                "a group condition over a rule that declares display",
                "@media sc\u{fffd}een { .h { display: none } }",
                true,
            ),
            (
                "a group condition over a rule with a damaged name",
                "@media sc\u{fffd}een { .h { fo\u{fffd}nt-weight: bold } }",
                true,
            ),
            (
                "a group condition over a rule that declares no display",
                "@media sc\u{fffd}een { .h { font-weight: bold } }",
                false,
            ),
            (
                "a group condition in a group",
                "@media screen { @supports (\u{fffd}) { .h { display: none } } }",
                true,
            ),
            (
                "a group condition above a clean group",
                "@media sc\u{fffd}een { @supports (a: b) { .h { display: none } } }",
                true,
            ),
            ("a rule in a screen group", "@media screen { .h\u{fffd} { display: none } }", true),
            (
                "a rule in a group a screen leaves unsettled",
                "@supports (a: b) { .h { disp\u{fffd}lay: none } }",
                true,
            ),
            ("a rule in a print group", "@media print { .h\u{fffd} { display: none } }", false),
            (
                "a rule in a print group in a screen group",
                "@media screen { @media print { .h { disp\u{fffd}lay: none } } }",
                false,
            ),
            ("a charset", "@charset \"\u{fffd}\"; .h { display: none }", false),
            ("an import", "@import url(\"\u{fffd}.css\") screen; .h { display: none }", false),
            ("a namespace", "@namespace \u{fffd} url(x); .h { display: none }", false),
            (
                "a rule nested in a rule that declares display",
                ".h { display: none; & a { x: \u{fffd} } }",
                false,
            ),
            (
                "a declaration after a rule nested in a rule",
                ".h { & a { x: 1 } display: none; y: \u{fffd} }",
                true,
            ),
            ("a font-face", "@font-face { font-family: \u{fffd} } .h { display: none }", false),
            ("a comment", "/* \u{fffd} */ .h { display: none }", false),
            ("a comment left open", ".h { display: none } /* \u{fffd}", false),
            ("a fragment with no colon", ".h { font-weight: bold; \u{fffd} }", false),
            ("a fragment with no colon beside a display", ".h { display: none; \u{fffd} }", true),
            (
                "a trail byte that closes a string before a damaged rule",
                ".a { content: \"\u{fffd}\\\" } .h\u{fffd} { display: none }",
                true,
            ),
            (
                "a trail byte that closes a string before a clean rule",
                ".a { content: \"\u{fffd}\\\" } .x { display: none }",
                false,
            ),
            ("no replacement", ".h { display: none }", false),
        ];
        for (case, css, expected) in cases {
            assert_eq!(
                Stylesheet::damage_may_hide(&ParseCtx::default(), css).unwrap(),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn the_damage_scan_charges_the_length_of_the_sheet() {
        use crate::limits::{Limit, Limits};
        let css = format!(".h {{ display: none }}{}", " ".repeat(100));
        let len = css.len() as u64;
        let parse_ctx = ParseCtx::default();
        Stylesheet::damage_may_hide(&parse_ctx, &css).unwrap();
        assert_eq!(parse_ctx.fuel(), len);
        let limits = Limits { max_work: len - 1, ..Limits::default() };
        assert_matches!(
            Stylesheet::damage_may_hide(&ParseCtx::new(&limits), &css),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max }) if max == len - 1
        );
        let token = crate::ledger::CancelToken::new();
        assert_matches!(
            Stylesheet::damage_may_hide(&cancelled(&token), &css),
            Err(ConvertError::Cancelled)
        );
    }

    #[test]
    fn heading_styling_stays_out_of_its_content() {
        let css = "h2 { font-style: italic; font-weight: bold }";
        let out = blocks_with_css("<body><h2>title <b>b</b></h2></body>", css);
        let Block::Heading { content, .. } = &out[0] else { panic!("{out:?}") };
        assert_eq!(first_text_style(content), Style::PLAIN);

        let out = blocks_with_css("<body><h2>title <b>b</b></h2></body>", "");
        let Block::Heading { content, .. } = &out[0] else { panic!("{out:?}") };
        assert_eq!(first_text_style(content), Style::PLAIN);
        let Some(Inline::Text { style, .. }) =
            content.iter().rfind(|i| matches!(i, Inline::Text { .. }))
        else {
            panic!()
        };
        assert!(style.bold);
    }

    #[test]
    fn extreme_ordered_list_values_do_not_overflow() {
        let html = format!(
            "<body><ol start=\"{}\"><li>a</li><li>b</li><li value=\"{}\">c</li><li>d</li></ol></body>",
            i64::MAX,
            i64::MIN,
        );
        let out = blocks(&html);
        assert_ne!(out, []);
    }

    #[test]
    fn unread_content_in_lists_and_tables_charges_the_rules_of_each_element_it_walks() {
        use DiagnosticCode::{BlockUnread, HiddenOmitted};
        const CSS: &str =
            ".h { display: none } .s { font-weight: bold } div, tbody { font-style: italic }";
        const LIST: &str = r#"<ul>
            <div><span class="s">a</span><span class="h">b</span><script class="s">x</script></div>
            <p class="h">c</p>
            <li><span class="s">d</span></li>
        </ul>"#;
        const TABLE: &str = r#"<table>
            <div><span class="s">a</span><span class="h">b</span></div>
            <tbody>
                <p class="h">x</p>
                <tr class="h"><div>y</div><td><span class="s">c</span></td></tr>
                <tr><td>d</td></tr>
            </tbody>
            <tr class="s"><div>z</div><td>e</td></tr>
        </table>"#;
        const NO_RULES: &str = "q { color: red }";
        const HIDDEN_HEAD: &str =
            "<table><thead><tr><td>a</td></tr><tr><td>b</td></tr></thead></table>";
        const HEAD_AND_ROW_RULES: &str = "thead { display: none } tr { font-style: italic }";
        let body = Region::Body;
        let cases: &[(&str, &str, &str, u64, &[Reported])] = &[
            ("list", LIST, CSS, 5, &[(BlockUnread, body, 1), (HiddenOmitted, body, 1)]),
            ("table", TABLE, CSS, 12, &[(BlockUnread, body, 2), (HiddenOmitted, body, 2)]),
            ("table without rules", TABLE, NO_RULES, 3, &[(BlockUnread, body, 4)]),
            ("rows of a hidden head", HIDDEN_HEAD, HEAD_AND_ROW_RULES, 3, &[]),
        ];
        for &(name, html, css, fuel, expected) in cases {
            let parse_ctx = ParseCtx::default();
            convert(&format!("<body>{html}</body>"), css, &parse_ctx).unwrap();
            assert_eq!(parse_ctx.fuel(), fuel, "{name}");
            assert_eq!(reported(&parse_ctx), expected, "{name}");
        }
    }

    fn list_block(marker: MarkerKind, start: u64, items: &[(&str, Option<&str>)]) -> Block {
        let items = items
            .iter()
            .map(|&(text, label)| ListItem {
                blocks: vec![Block::Paragraph(vec![Inline::plain(text)])],
                marker_label: label.map(str::to_string),
            })
            .collect();
        Block::List(List { marker, start, items })
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of list numbering jumps")]
    fn list_numbering_splits_where_it_jumps_and_labels_what_start_cannot_carry() {
        use MarkerKind::{Bullet, Decimal, LowerAlpha};
        let max = i64::MAX.cast_unsigned();
        let max_html = format!(r#"<ol start="{}"><li>a</li><li>b</li></ol>"#, i64::MAX);
        let cases: Vec<(&str, String, Vec<Block>)> = vec![
            ("empty", "<ol/>".into(), vec![]),
            ("no items", "<ul><div>a</div></ul>".into(), vec![]),
            (
                "bullets",
                "<ul><li>a</li><li>b</li></ul>".into(),
                vec![list_block(Bullet, 1, &[("a", None), ("b", None)])],
            ),
            (
                "type and start",
                r#"<ol type="a" start="3"><li>a</li><li>b</li></ol>"#.into(),
                vec![list_block(LowerAlpha, 3, &[("a", None), ("b", None)])],
            ),
            (
                "value jump",
                r#"<ol><li>a</li><li>b</li><li value="7">c</li><li>d</li></ol>"#.into(),
                vec![
                    list_block(Decimal, 1, &[("a", None), ("b", None)]),
                    list_block(Decimal, 7, &[("c", None), ("d", None)]),
                ],
            ),
            (
                "reversed",
                r#"<ol reversed=""><li>a</li><li>b</li><li>c</li></ol>"#.into(),
                vec![
                    list_block(Decimal, 3, &[("a", None)]),
                    list_block(Decimal, 2, &[("b", None)]),
                    list_block(Decimal, 1, &[("c", None)]),
                ],
            ),
            (
                "reversed then ascending values",
                r#"<ol reversed=""><li>a</li><li value="2">b</li><li value="3">c</li></ol>"#.into(),
                vec![
                    list_block(Decimal, 3, &[("a", None)]),
                    list_block(Decimal, 2, &[("b", None), ("c", None)]),
                ],
            ),
            (
                "reversed from a start",
                r#"<ol reversed="" start="5"><li>a</li><li value="9">b</li><li>c</li></ol>"#.into(),
                vec![
                    list_block(Decimal, 5, &[("a", None)]),
                    list_block(Decimal, 9, &[("b", None)]),
                    list_block(Decimal, 8, &[("c", None)]),
                ],
            ),
            (
                "negative start",
                r#"<ol start="-1"><li>a</li><li>b</li><li>c</li></ol>"#.into(),
                vec![list_block(
                    Decimal,
                    1,
                    &[("a", Some("-1.")), ("b", Some("0.")), ("c", Some("1."))],
                )],
            ),
            (
                "zero value",
                r#"<ol><li>a</li><li value="0">b</li><li>c</li></ol>"#.into(),
                vec![list_block(
                    Decimal,
                    1,
                    &[("a", Some("1.")), ("b", Some("0.")), ("c", Some("1."))],
                )],
            ),
            (
                "saturated start",
                max_html,
                vec![
                    list_block(Decimal, max, &[("a", None)]),
                    list_block(Decimal, max, &[("b", None)]),
                ],
            ),
        ];
        for (name, html, expected) in cases {
            assert_eq!(blocks(&format!("<body>{html}</body>")), expected, "{name}");
        }
    }

    #[test]
    fn text_sits_at_a_space_boundary_after_whitespace_a_break_or_the_start_of_a_block() {
        let link = |content: Vec<Inline>| Inline::Link {
            content,
            target: LinkTarget::External("u".into()),
        };
        let cases: Vec<(&str, Vec<Inline>, bool, bool)> = vec![
            ("block start", vec![], true, true),
            ("inline run start", vec![], false, false),
            ("after a space", vec![Inline::plain("a ")], false, true),
            ("after a word", vec![Inline::plain("a")], true, false),
            (
                "through an anchor",
                vec![Inline::plain("a "), Inline::Anchor("n".into())],
                false,
                true,
            ),
            ("through an empty text", vec![Inline::plain("a "), Inline::plain("")], false, true),
            ("after a break", vec![Inline::plain("a"), Inline::LineBreak], false, true),
            ("after a formula", vec![Inline::plain("a "), Inline::Math("x".into())], true, false),
            ("into a label", vec![link(vec![Inline::plain("a ")])], false, true),
            ("through an empty label", vec![Inline::plain("a "), link(vec![])], false, true),
            ("into a label of empty links", vec![link(vec![link(vec![])])], true, false),
        ];
        for (name, inlines, start, expected) in cases {
            assert_eq!(at_space_boundary(&inlines, start), expected, "{name}");
        }
    }

    #[test]
    fn font_weight_and_style_values_decide_bold_and_italic() {
        let cases = [
            ("font-weight: bold", true, false),
            ("font-weight: bolder", true, false),
            ("font-weight: 600", true, false),
            ("font-weight: 599", false, false),
            ("font-weight: normal", false, false),
            ("font-style: italic", false, true),
            ("font-style: oblique", false, true),
            ("font-style: normal", false, false),
        ];
        for (declaration, bold, italic) in cases {
            let css = format!(".x {{ {declaration} }}");
            let style = first_style(r#"<body><p><span class="x">x</span></p></body>"#, &css);
            assert_eq!((style.bold, style.italic), (bold, italic), "{declaration}");
        }
    }

    #[test]
    fn a_selector_tag_is_lowercased_and_its_class_is_not() {
        let html = r#"<body><p><span class="x">x</span></p></body>"#;
        assert!(first_style(html, "SPAN.x { font-weight: bold }").bold);
        assert!(!first_style(html, "SPAN.X { font-weight: bold }").bold);
    }

    #[test]
    fn an_elements_id_then_an_anchors_name_become_link_targets() {
        let cases: &[(&str, &str, &[&str])] = &[
            ("id and name", r#"<a id="i" name="n">x</a>"#, &["i", "n"]),
            ("name outside an anchor", r#"<span id="i" name="n">x</span>"#, &["i"]),
            ("empty id and name", r#"<a id="" name="">x</a>"#, &[]),
        ];
        for &(name, html, expected) in cases {
            let out = blocks(&format!("<body><p>{html}</p></body>"));
            let Block::Paragraph(inlines) = &out[0] else { panic!("{out:?}") };
            let ids: Vec<&str> = inlines
                .iter()
                .filter_map(|i| if let Inline::Anchor(id) = i { Some(id.as_str()) } else { None })
                .collect();
            assert_eq!(ids, expected, "{name}");
        }
    }

    #[test]
    fn an_empty_heading_keeps_its_link_targets_as_a_paragraph() {
        let anchors = |ids: &[&str]| {
            vec![Block::Paragraph(ids.iter().map(|&id| Inline::Anchor(id.into())).collect())]
        };
        let cases: Vec<(&str, &str, Vec<Block>)> = vec![
            ("own id", r#"<h2 id="top"> </h2>"#, anchors(&["top"])),
            ("inner anchor", r#"<h2><a id="in"/> </h2>"#, anchors(&["in"])),
            ("both", r#"<h2 id="top"><a id="in"/> </h2>"#, anchors(&["top", "in"])),
            ("neither", "<h2> </h2>", vec![]),
        ];
        for (name, html, expected) in cases {
            assert_eq!(blocks(&format!("<body>{html}</body>")), expected, "{name}");
        }
    }

    #[test]
    fn display_math_is_a_block_and_inline_math_stays_in_its_paragraph() {
        let cases: Vec<(&str, &str, Vec<Block>)> = vec![
            (
                "display",
                r#"<math display="block"><mi>x</mi></math>"#,
                vec![Block::Math("x".into())],
            ),
            (
                "inline",
                "<p>a <math><mi>x</mi></math></p>",
                vec![Block::Paragraph(vec![Inline::plain("a "), Inline::Math("x".into())])],
            ),
            ("empty display", r#"<math display="block"><mrow/></math>"#, vec![]),
            ("empty inline", "<p><math><mrow/></math></p>", vec![]),
        ];
        for (name, html, expected) in cases {
            assert_eq!(blocks(&format!("<body>{html}</body>")), expected, "{name}");
        }
    }

    #[test]
    fn a_table_caption_precedes_the_table_as_a_paragraph() {
        let out = blocks("<body><table><caption>cap</caption><tr><td>a</td></tr></table></body>");
        assert_eq!(out.len(), 2);
        assert_eq!(para_text(&out[0]), "cap");
        assert_matches!(out[1], Block::Table(_));
    }

    #[test]
    fn header_rows_come_from_thead_or_a_leading_row_of_header_cells() {
        let cases = [
            ("thead of data cells", "<thead><tr><td>1</td></tr></thead><tr><td>2</td></tr>", 1),
            (
                "thead of two rows",
                "<thead><tr><td>1</td></tr><tr><td>2</td></tr></thead><tr><td>3</td></tr>",
                2,
            ),
            ("leading header cells", "<tr><th>1</th></tr><tr><td>2</td></tr>", 1),
            ("no header markup", "<tr><td>1</td></tr><tr><td>2</td></tr>", 0),
        ];
        for (name, rows, expected) in cases {
            let out = blocks(&format!("<body><table>{rows}</table></body>"));
            let Block::Table(table) = &out[0] else { panic!("{name}: {out:?}") };
            assert_eq!(table.header_rows, expected, "{name}");
        }
    }

    #[test]
    fn rowspan_zero_stops_at_the_implicit_row_group_before_a_row_group() {
        let html = r#"<body><table>
            <tr><td rowspan="0">a</td></tr>
            <tbody><tr><td>b</td></tr></tbody>
        </table></body>"#;
        let out = blocks(html);
        let Block::Table(t) = &out[0] else { panic!("{out:?}") };
        assert_matches!(&t.grid[0][0], CellSlot::Origin(c) if c.row_span == 1);
        assert_matches!(&t.grid[1][0], CellSlot::Origin(_));
    }

    #[test]
    fn container_and_block_tags_break_the_paragraph_around_their_block_children() {
        let paragraphs = |html: &str| -> Vec<String> {
            blocks(&format!("<body>{html}</body>"))
                .iter()
                .filter(|b| matches!(b, Block::Paragraph(_)))
                .map(para_text)
                .collect()
        };
        let containers = "div section article aside main nav header footer"
            .split(' ')
            .chain("figure figcaption center details summary li dl dt dd body".split(' '));
        for tag in containers {
            let html = format!("a<{tag}><span>b</span><p>c</p></{tag}>d");
            assert_eq!(paragraphs(&html), ["a", "b", "c", "d"], "{tag}");
        }
        for tag in "p ul ol table blockquote pre hr h1 h2 h3 h4 h5 h6".split(' ') {
            let html = format!("a<div><span>b</span><{tag}/></div>");
            assert_eq!(paragraphs(&html), ["a", "b"], "{tag}");
        }
    }

    #[test]
    fn rowspan_zero_spans_to_the_end_of_the_row_group() {
        let html = r#"<body><table>
            <tbody>
                <tr><td rowspan="0">tall</td><td>a</td></tr>
                <tr><td>b</td></tr>
            </tbody>
            <tbody><tr><td>c</td><td>d</td></tr></tbody>
        </table></body>"#;
        let out = blocks(html);
        let Block::Table(t) = &out[0] else { panic!("{out:?}") };
        assert_matches!(&t.grid[0][0], CellSlot::Origin(c) if c.row_span == 2);
        assert_matches!(t.grid[1][0], CellSlot::Covered { origin_row: 0, origin_col: 0 });
        assert_matches!(&t.grid[2][0], CellSlot::Origin(_), "next group must not be covered");
    }
}
