//! Inline normalization and rendering.

use crate::error::ConvertError;
use crate::model::{Inline, LinkTarget, Style};
use crate::render::markdown::Ctx;
use crate::render::markdown::escape::{
    Delims, EscapeOpts, InlineContext, MathSite, backtick_fence, escape_math, escape_text,
    escape_url_as_text, format_url, masked_dollar_closes,
};
use crate::render::markdown::image::{Presentation, label, project};
use crate::render::markdown::marks::{Cluster, Side, Stack, Writer, push_reference};
use crate::render::markdown::shape::nonspace;
use crate::render::markdown::sink::{Meter, Text};
use crate::shared::uri::is_safe_link;
use std::borrow::Cow;

/// One piece of the effective view of a text scope.
pub(crate) enum Norm<'a> {
    /// Unstyled text whole; styled text one line's stretch at a time, edge whitespace apart.
    Text {
        text: Cow<'a, str>,
        style: Style,
    },
    Link {
        label: Label<'a>,
        target: &'a LinkTarget,
    },
    Image {
        alt: Cow<'a, str>,
        presentation: Presentation<'a>,
    },
    Anchor(&'a str),
    NoteRef(&'a str),
    LineBreak,
    Math(&'a str),
    Checkbox(bool),
}

pub(crate) enum Label<'a> {
    Content(Vec<Norm<'a>>),
    Url(&'a str),
}

/// The effective view of `inlines` in `ctx`, every run also carrying `base`'s marks: what
/// renders nothing drops out, an image label written as text joins the text beside it, a link
/// that keeps no destination gives its content to the scope around it, and a nested link splits
/// the link around it, so no link holds another. Each inline read is charged one unit, and so
/// is each one a link's look at its content reads.
pub(crate) fn normalize<'a>(
    inlines: &'a [Inline],
    ctx: InlineContext,
    base: Style,
    rc: &Ctx<'_>,
    meter: &Meter<'_>,
) -> Result<Vec<Norm<'a>>, ConvertError> {
    let mut flat = Flatten {
        ctx,
        base,
        rc,
        meter,
        out: Vec::new(),
        segment: Vec::new(),
        owner: None,
        pending: None,
        group: 0,
    };
    flat.walk(inlines, None)?;
    flat.flush();
    Ok(flat.out)
}

type Owner<'a> = Option<&'a LinkTarget>;

struct Flatten<'a, 'r, 'd> {
    ctx: InlineContext,
    base: Style,
    rc: &'r Ctx<'d>,
    meter: &'r Meter<'r>,
    out: Vec<Norm<'a>>,
    /// The pieces of the link `owner` read since it last opened or was split.
    segment: Vec<Norm<'a>>,
    owner: Owner<'a>,
    /// Adjacent runs of one style and owner, joined before they split into stretches.
    pending: Option<(Cow<'a, str>, Style, Owner<'a>)>,
    /// Where the pieces of the outermost link being read start in `out`: only links split out
    /// of one source link join.
    group: usize,
}

fn same(a: Owner<'_>, b: Owner<'_>) -> bool {
    a.map(std::ptr::from_ref) == b.map(std::ptr::from_ref)
}

impl<'a> Flatten<'a, '_, '_> {
    fn walk(&mut self, inlines: &'a [Inline], owner: Owner<'a>) -> Result<(), ConvertError> {
        for inline in inlines {
            self.meter.charge(1)?;
            match inline {
                Inline::Text { text, style } => self.text(Cow::Borrowed(text), *style, owner),
                Inline::Link { content, target } => {
                    if target.is_empty() || self.rc.degrades(target) {
                        self.walk(content, owner)?;
                    } else if nonspace(content, self.ctx, self.rc, self.meter)? {
                        if owner.is_none() {
                            self.flush();
                            self.group = self.out.len();
                        }
                        self.walk(content, Some(target))?;
                    } else if let Some(url) = url_label(target) {
                        self.flush();
                        self.out.push(Norm::Link { label: Label::Url(url), target });
                    } else {
                        self.walk(content, owner)?;
                    }
                }
                Inline::Image { alt, source, .. } => {
                    match project(self.rc.images, source, owner.is_some()).presentation {
                        Presentation::Omit => {}
                        Presentation::Label => self.text(label(alt), Style::PLAIN, owner),
                        presentation => {
                            self.push(owner, Norm::Image { alt: label(alt), presentation });
                        }
                    }
                }
                Inline::Anchor(id) if self.rc.anchors.html_id(id).is_some() => {
                    self.push(owner, Norm::Anchor(id));
                }
                Inline::NoteRef(id) if self.rc.nums.contains_key(id) => {
                    self.push(owner, Norm::NoteRef(id));
                }
                Inline::LineBreak if self.ctx == InlineContext::Heading => {
                    self.text(Cow::Borrowed(" "), Style::PLAIN, owner);
                }
                Inline::LineBreak => self.push(owner, Norm::LineBreak),
                Inline::Math(tex) if !tex.trim().is_empty() => {
                    self.push(owner, Norm::Math(tex.trim()));
                }
                Inline::Checkbox(checked) => self.push(owner, Norm::Checkbox(*checked)),
                Inline::Anchor(_) | Inline::NoteRef(_) | Inline::Math(_) => {}
            }
        }
        Ok(())
    }

    fn text(&mut self, text: Cow<'a, str>, style: Style, owner: Owner<'a>) {
        if text.is_empty() {
            return;
        }
        let base = self.base;
        let style = Style {
            bold: style.bold || base.bold,
            italic: style.italic || base.italic,
            strike: style.strike || base.strike,
            ..style
        };
        if let Some((prev, prev_style, prev_owner)) = &mut self.pending
            && *prev_style == style
            && same(*prev_owner, owner)
        {
            prev.to_mut().push_str(&text);
            return;
        }
        self.settle();
        self.pending = Some((text, style, owner));
    }

    fn settle(&mut self) {
        let Some((text, style, owner)) = self.pending.take() else { return };
        if style == Style::PLAIN || text.trim().is_empty() {
            self.place(owner, Norm::Text { text, style: Style::PLAIN });
            return;
        }
        for (start, end, core) in stretches(&text, style.code) {
            let piece = match &text {
                Cow::Borrowed(text) => Cow::Borrowed(text.get(start..end).unwrap_or_default()),
                Cow::Owned(text) => Cow::Owned(text.get(start..end).unwrap_or_default().to_owned()),
            };
            let style = if core { style } else { Style::PLAIN };
            self.place(owner, Norm::Text { text: piece, style });
        }
    }

    fn push(&mut self, owner: Owner<'a>, item: Norm<'a>) {
        self.settle();
        self.place(owner, item);
    }

    fn place(&mut self, owner: Owner<'a>, item: Norm<'a>) {
        if !same(owner, self.owner) {
            self.end_segment();
            self.owner = owner;
        }
        join(&mut self.segment, item);
    }

    fn flush(&mut self) {
        self.settle();
        self.end_segment();
    }

    /// Close the segment read for `owner`: a link where it shows, joined to an adjacent link to
    /// an equal target; otherwise its whitespace, as plain text in its place.
    fn end_segment(&mut self) {
        let items = std::mem::take(&mut self.segment);
        if let Some(target) = self.owner
            && shows(&items, self.ctx)
        {
            let joined = self.out.len() > self.group;
            if let Some(Norm::Link { label: Label::Content(prev), target: last }) =
                self.out.last_mut()
                && joined
                && **last == *target
            {
                for item in items {
                    join(prev, item);
                }
            } else {
                self.out.push(Norm::Link { label: Label::Content(items), target });
            }
            return;
        }
        for item in items {
            join(&mut self.out, item);
        }
    }
}

fn join<'a>(items: &mut Vec<Norm<'a>>, item: Norm<'a>) {
    if let Norm::Text { text, style } = &item
        && let Some(Norm::Text { text: prev, style: prev_style }) = items.last_mut()
        && prev_style == style
    {
        prev.to_mut().push_str(text);
        return;
    }
    items.push(item);
}

/// The byte ranges of `text` as whitespace and the stretches between, each stretch marked: one
/// per line, or one for a code run, whose span writes its line breaks as spaces.
fn stretches(text: &str, code: bool) -> impl Iterator<Item = (usize, usize, bool)> + '_ {
    let mut at = 0;
    std::iter::from_fn(move || {
        let rest = text.get(at..).filter(|rest| !rest.is_empty())?;
        let start = at;
        let blank = rest.find(|c: char| !c.is_whitespace()).unwrap_or(rest.len());
        if blank > 0 {
            at += blank;
            return Some((start, at, false));
        }
        let line = if code { rest.len() } else { rest.find(['\n', '\r']).unwrap_or(rest.len()) };
        at += rest.get(..line).unwrap_or_default().trim_end().len();
        Some((start, at, true))
    })
}

fn shows(items: &[Norm<'_>], ctx: InlineContext) -> bool {
    items.iter().any(|item| match item {
        Norm::Text { text, .. } => !text.trim().is_empty(),
        Norm::LineBreak => ctx == InlineContext::Block,
        _ => true,
    })
}

/// Render `inlines` into `out`, as if `out` started where they start.
pub(crate) fn render_inlines(
    inlines: &[Inline],
    ctx: InlineContext,
    rc: &Ctx<'_>,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    render_based(inlines, ctx, Style::PLAIN, rc, out)
}

/// Render `inlines` into `out` as [`render_inlines`] does, every run also carrying `base`'s
/// marks.
pub(crate) fn render_based(
    inlines: &[Inline],
    ctx: InlineContext,
    base: Style,
    rc: &Ctx<'_>,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let items = normalize(inlines, ctx, base, rc, out.meter())?;
    render_scope(&items, Scope { ctx, in_label: false, base: Stack::of(base) }, rc, out)
}

/// What a scope holds from one piece on: the marks its first visible piece keeps open, whether
/// that piece is this one, the side this piece shows a delimiter written before it, and the
/// delimiters written from here to the scope's end.
#[derive(Clone, Copy)]
struct Ahead {
    marks: Stack,
    visible: bool,
    first: Side,
    delims: Delims,
}

const END: Ahead =
    Ahead { marks: Stack::EMPTY, visible: true, first: Side::Space, delims: Delims::NONE };

/// Where a scope's pieces render, and the marks its pieces of markup carry.
#[derive(Clone, Copy)]
struct Scope {
    ctx: InlineContext,
    in_label: bool,
    base: Stack,
}

fn lookahead(items: &[Norm<'_>], scope: Scope, out: &Text<'_>) -> Result<Vec<Ahead>, ConvertError> {
    let mut ahead = vec![END; items.len() + 1];
    for (idx, item) in items.iter().enumerate().rev() {
        let next = ahead.get(idx + 1).copied().unwrap_or(END);
        out.charge(1)?;
        if let Norm::Link { label: Label::Content(label), .. } = item {
            out.charge(u64::try_from(label.len()).unwrap_or(u64::MAX))?;
        }
        let mut delims = next.delims;
        delims.union(delims_of(item));
        let (marks, visible, first) = match item {
            Norm::Text { text, style } if *style == Style::PLAIN => {
                let first = Side::of(text.chars().next());
                if !text.trim().is_empty() {
                    (Stack::EMPTY, true, first)
                } else if text.contains(['\n', '\r']) {
                    (Stack::EMPTY, false, first)
                } else {
                    (next.marks, false, first)
                }
            }
            Norm::Text { style, .. } if style.code => (Stack::of(*style), true, Side::Fixed),
            Norm::Text { text, style } => (Stack::of(*style), true, Side::of(text.chars().next())),
            Norm::LineBreak if scope.ctx == InlineContext::TableCell => {
                (Stack::EMPTY, true, Side::Space)
            }
            Norm::LineBreak | Norm::Link { label: Label::Content(_), .. } => {
                (Stack::EMPTY, true, Side::Fixed)
            }
            _ => (scope.base, true, Side::Fixed),
        };
        if let Some(slot) = ahead.get_mut(idx) {
            *slot = Ahead { marks, visible, first, delims };
        }
    }
    Ok(ahead)
}

/// Whether a piece's first and last characters are written as numeric references.
#[derive(Clone, Copy, Default)]
struct Ends {
    first: bool,
    last: bool,
}

/// Which of each piece's first and last characters are written as numeric references, so that
/// every cluster of delimiters flanks. Decided from the scope's end back, as a reference given
/// to the first character of a piece changes what the cluster before that piece sees; a piece
/// of one character shows that one reference to both its clusters, so referencing it re-decides
/// the cluster after it.
fn references(items: &[Norm<'_>], ahead: &[Ahead]) -> Vec<Ends> {
    let mut open = Stack::EMPTY;
    let mut clusters = Vec::with_capacity(items.len());
    for (here, next) in ahead.iter().zip(ahead.iter().skip(1)) {
        if here.visible {
            open = here.marks;
        }
        clusters.push(Cluster::between(open, next.marks, next.visible));
        open = open.truncated(open.common(next.marks));
    }
    let single = |idx: usize| {
        matches!(items.get(idx), Some(Norm::Text { text, style })
            if !style.code && text.chars().nth(1).is_none())
    };
    let mut refs = vec![Ends::default(); items.len()];
    for seam in (0..items.len()).rev() {
        let mut seam = seam;
        while let (Some(cluster), Some(item)) = (clusters.get(seam), items.get(seam)) {
            let encoded = |idx: usize| refs.get(idx).copied().unwrap_or_default();
            let left = if encoded(seam).last { Side::Fixed } else { last_side(item) };
            let right = match ahead.get(seam + 1) {
                _ if encoded(seam + 1).first => Side::Fixed,
                Some(next) => next.first,
                None => Side::Space,
            };
            let (l, r) = cluster.references(left, right);
            if l && let Some(slot) = refs.get_mut(seam) {
                *slot = Ends { first: slot.first || single(seam), last: true };
            }
            if !r {
                break;
            }
            let lone = single(seam + 1);
            if let Some(slot) = refs.get_mut(seam + 1) {
                *slot = Ends { first: true, last: slot.last || lone };
            }
            if !lone {
                break;
            }
            seam += 1;
        }
    }
    refs
}

/// The side a delimiter written after `item` sees.
fn last_side(item: &Norm<'_>) -> Side {
    match item {
        Norm::Text { style, .. } if style.code => Side::Fixed,
        Norm::Text { text, .. } => Side::of(text.chars().next_back()),
        Norm::LineBreak => Side::Space,
        _ => Side::Fixed,
    }
}

/// Render one scope's pieces: a paragraph, heading or cell part, or one link label.
fn render_scope(
    items: &[Norm<'_>],
    scope: Scope,
    rc: &Ctx<'_>,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let Scope { ctx, in_label, .. } = scope;
    let start = out.len();
    let ahead = lookahead(items, scope, out)?;
    let references = references(items, &ahead);
    let mut marks = Writer::default();
    for (idx, item) in items.iter().enumerate() {
        out.charge(1)?;
        let here = ahead.get(idx).copied().unwrap_or(END);
        let next = ahead.get(idx + 1).copied().unwrap_or(END);
        if here.visible {
            marks.open_to(here.marks, out)?;
        }
        let after_delimiter = std::mem::take(&mut marks.delimited);
        let ends = references.get(idx).copied().unwrap_or_default();
        match item {
            Norm::Text { text, style } if style.code => push_code_span(text, ctx, out)?,
            Norm::Text { text, style } => {
                let opts = EscapeOpts {
                    at_line_start: *style == Style::PLAIN
                        && (out.len() == start || out.as_str().ends_with(['\r', '\n'])),
                    styled: *style != Style::PLAIN,
                    trailing_delims: next.delims,
                    in_label,
                    joining: text
                        .chars()
                        .next()
                        .filter(|c| after_delimiter && matches!(c, '*' | '_' | '~')),
                    ..trailing(items.get(idx + 1))
                };
                write_text(text, ctx, opts, ends, out)?;
            }
            Norm::NoteRef(id) => {
                if let Some(num) = rc.nums.get(*id) {
                    out.push_note_ref(&format!("[^{num}]"))?;
                }
            }
            Norm::Link { label, target } => render_link(label, target, scope, rc, out)?,
            Norm::Image { alt, presentation } => render_image(alt, *presentation, scope, out)?,
            Norm::Anchor(id) => {
                if let Some(html_id) = rc.anchors.html_id(id) {
                    out.push_str(&format!("<a id=\"{html_id}\"></a>"))?;
                }
            }
            Norm::LineBreak => match ctx {
                InlineContext::Block => out.push_str("\\\n")?,
                InlineContext::Heading => out.push(' ')?,
                InlineContext::TableCell => out.push('\n')?,
            },
            Norm::Math(tex) => push_math_span(tex, ctx, out)?,
            Norm::Checkbox(checked) => {
                out.push_str(if *checked { "[x]" } else { "[ ]" })?;
                if matches!(items.get(idx + 1), Some(item) if !starts_with_space(item)) {
                    out.push(' ')?;
                }
            }
        }
        marks.close_to(next.marks, out)?;
    }
    Ok(())
}

fn trailing(follower: Option<&Norm<'_>>) -> EscapeOpts {
    EscapeOpts {
        trailing_active: match follower {
            Some(Norm::Link { .. } | Norm::Image { .. } | Norm::NoteRef(_) | Norm::Math(_)) => true,
            Some(Norm::Text { style, .. }) => *style != Style::PLAIN,
            _ => false,
        },
        trailing_nonspace: matches!(
            follower,
            Some(Norm::Anchor(_) | Norm::Checkbox(_) | Norm::LineBreak)
        ),
        ..EscapeOpts::default()
    }
}

/// Escape `text` into `out`, writing its first or last character as a numeric reference where
/// the delimiters beside it need one to flank.
fn write_text(
    text: &str,
    ctx: InlineContext,
    mut opts: EscapeOpts,
    Ends { first, last }: Ends,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let mut body = text;
    if first && let Some(c) = body.chars().next() {
        push_reference(c, out)?;
        body = body.get(c.len_utf8()..).unwrap_or_default();
        opts.at_line_start = false;
        opts.joining = None;
    }
    let tail = if last { body.chars().next_back() } else { None };
    if let Some(c) = tail {
        body = body.get(..body.len() - c.len_utf8()).unwrap_or_default();
        opts.trailing_nonspace = true;
    }
    escape_text(body, ctx, opts, out)?;
    tail.map_or(Ok(()), |c| push_reference(c, out))
}

pub(super) fn url_label(target: &LinkTarget) -> Option<&str> {
    match target {
        LinkTarget::External(url) | LinkTarget::Relative(url)
            if !url.is_empty() && is_safe_link(url) =>
        {
            Some(url)
        }
        _ => None,
    }
}

fn render_link(
    label: &Label<'_>,
    target: &LinkTarget,
    scope: Scope,
    rc: &Ctx<'_>,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let ctx = scope.ctx;
    let url = match target {
        LinkTarget::External(url) | LinkTarget::Relative(url) => Cow::Borrowed(url.as_str()),
        LinkTarget::Unit(unit) => {
            Cow::Owned(format!("#{}", rc.anchors.unit_fragment(*unit).unwrap_or_default()))
        }
        LinkTarget::Anchor { id, .. } => {
            Cow::Owned(format!("#{}", rc.anchors.fragment(id).unwrap_or_default()))
        }
    };
    out.push('[')?;
    match label {
        Label::Content(items) => render_scope(items, Scope { in_label: true, ..scope }, rc, out)?,
        Label::Url(label) => escape_url_as_text(label, ctx, out)?,
    }
    out.push_str("](")?;
    format_url(&url, out)?;
    out.push(')')
}

fn render_image(
    alt: &str,
    presentation: Presentation<'_>,
    Scope { ctx, in_label, .. }: Scope,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let bracketed = matches!(presentation, Presentation::Asset(_) | Presentation::Link(_));
    match presentation {
        Presentation::Asset(_) => out.push_str("![")?,
        Presentation::Link(_) => out.push('[')?,
        Presentation::Marker => out.push_str("\\[")?,
        Presentation::Label | Presentation::Omit => {}
    }
    escape_text(
        alt,
        ctx,
        EscapeOpts {
            in_label: in_label || bracketed,
            at_line_start: ctx == InlineContext::Block,
            trailing_active: true,
            ..Default::default()
        },
        out,
    )?;
    match presentation {
        Presentation::Asset(id) => out.push_str(&format!("](asset:{})", id.0)),
        Presentation::Link(url) => {
            out.push_str("](")?;
            format_url(url, out)?;
            out.push(')')
        }
        Presentation::Marker => out.push_str("\\]"),
        Presentation::Label | Presentation::Omit => Ok(()),
    }
}

/// What one piece contributes to an earlier piece's pairing partners: closer-capable literals
/// in plain text, and the markup the other pieces write. Emphasis cannot cross a link boundary,
/// but a code span can: a backtick in a label or destination pairs with one outside.
fn delims_of(item: &Norm<'_>) -> Delims {
    let mut delims = Delims::NONE;
    match item {
        Norm::Text { text, style } if *style == Style::PLAIN => delims.insert_closers(text),
        Norm::Text { text, style } => {
            for c in Stack::of(*style).delimiters() {
                delims.insert(c);
            }
            // Emphasis content is escaped, which neutralizes everything but backticks: code
            // spans ignore backslash escapes, so an emitted `\`` still closes a span an earlier
            // raw backtick opens. `]` is the one character escaping leaves raw.
            if style.code || text.contains('`') {
                delims.insert('`');
            }
            if !style.code && text.contains(']') {
                delims.insert(']');
            }
            if !style.code && masked_dollar_closes(text) {
                delims.insert('$');
            }
        }
        Norm::Link { label, target } => {
            if emits_backtick(label)
                || matches!(target, LinkTarget::External(url) | LinkTarget::Relative(url)
                    if url.contains('`'))
            {
                delims.insert('`');
            }
        }
        Norm::Image { alt, presentation } => {
            delims.insert_closers(alt);
            if let Presentation::Link(url) = presentation
                && url.contains('`')
            {
                delims.insert('`');
            }
        }
        Norm::NoteRef(_)
        | Norm::Anchor(_)
        | Norm::LineBreak
        | Norm::Math(_)
        | Norm::Checkbox(_) => {}
    }
    delims
}

/// True when a link label writes a backtick an earlier raw one can pair with: any backtick in
/// its text counts even where the label escapes it, since code spans ignore backslash escapes.
fn emits_backtick(label: &Label<'_>) -> bool {
    match label {
        Label::Url(_) => false,
        Label::Content(items) => items.iter().any(|item| match item {
            Norm::Text { text, style } => style.code || text.contains('`'),
            Norm::Image { alt, .. } => alt.contains('`'),
            _ => false,
        }),
    }
}

fn starts_with_space(item: &Norm<'_>) -> bool {
    match item {
        Norm::Text { text, .. } => text.starts_with(char::is_whitespace),
        Norm::LineBreak => true,
        _ => false,
    }
}

/// GFM inline math: `$` hugging both ends of the source.
pub(crate) fn push_math_span(
    tex: &str,
    ctx: InlineContext,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let site = if ctx == InlineContext::TableCell { MathSite::TableCell } else { MathSite::Span };
    out.push('$')?;
    escape_math(tex.trim(), site, |piece| out.push_str(piece))?;
    out.push('$')
}

/// A code span; line breaks inside it become spaces. A row is split into
/// cells before any code span is parsed, so in a cell a pipe is syntax even
/// though everything else between the fences is literal: a backslash run
/// already in front of one would pair off with its escape and leave it bare,
/// so the run is doubled to keep the escape intact. That doubling survives
/// into the rendered code span: GFM has no encoding for a code span that
/// contains a backslash immediately before a pipe, and an intact row is worth
/// more than the exact backslash count.
pub(crate) fn push_code_span(
    text: &str,
    ctx: InlineContext,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let fence = backtick_fence(text, 1);
    let pad = if text.starts_with('`') || text.ends_with('`') { " " } else { "" };
    out.push_str(&fence)?;
    out.push_str(pad)?;
    let mut flushed = 0;
    let mut backslashes = 0;
    for (i, c) in text.char_indices() {
        match c {
            '\n' => {
                out.push_str(text.get(flushed..i).unwrap_or_default())?;
                out.push(' ')?;
                flushed = i + 1;
                backslashes = 0;
            }
            '|' if ctx == InlineContext::TableCell => {
                out.push_str(text.get(flushed..i).unwrap_or_default())?;
                for _ in 0..=backslashes {
                    out.push('\\')?;
                }
                flushed = i;
                backslashes = 0;
            }
            '\\' => backslashes += 1,
            _ => backslashes = 0,
        }
    }
    out.push_str(text.get(flushed..).unwrap_or_default())?;
    out.push_str(pad)?;
    out.push_str(&fence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Block, Document, ImageOrigin, ImageSource};
    use crate::render::markdown::document_to_markdown;

    #[test]
    fn a_follower_is_active_or_non_space_markup_by_its_kind_and_style() {
        let target = LinkTarget::External("https://x.test".into());
        let text = |style| Norm::Text { text: Cow::Borrowed("x"), style };
        let image = Norm::Image { alt: Cow::Borrowed("i"), presentation: Presentation::Marker };
        let link = Norm::Link { label: Label::Url("u"), target: &target };
        let bold = Style { bold: true, ..Style::PLAIN };
        for (label, follower, active, nonspace) in [
            ("nothing", None, false, false),
            ("plain text", Some(text(Style::PLAIN)), false, false),
            ("styled text", Some(text(bold)), true, false),
            ("link", Some(link), true, false),
            ("image", Some(image), true, false),
            ("note reference", Some(Norm::NoteRef("n")), true, false),
            ("math", Some(Norm::Math("x")), true, false),
            ("anchor", Some(Norm::Anchor("a")), false, true),
            ("checkbox", Some(Norm::Checkbox(true)), false, true),
            ("line break", Some(Norm::LineBreak), false, true),
        ] {
            let opts = trailing(follower.as_ref());
            let flags = (opts.trailing_active, opts.trailing_nonspace);
            assert_eq!(flags, (active, nonspace), "{label}");
        }
    }

    #[test]
    fn a_backtick_in_a_later_label_alt_or_destination_escapes_the_raw_one_before_it() {
        let link = |content, target| Inline::Link { content, target };
        let image = || Inline::Image {
            origin: ImageOrigin::new(crate::Region::Body, None),
            alt: "b`".into(),
            source: ImageSource::Unavailable,
        };
        let site = || LinkTarget::External("https://x.test".into());
        for (label, later, expected) in [
            (
                "relative destination",
                link(vec![Inline::plain("b")], LinkTarget::Relative("c`d".into())),
                "\\`a\\\n[b](c`d)\n",
            ),
            (
                "external destination",
                link(vec![Inline::plain("b")], LinkTarget::External("https://c`d".into())),
                "\\`a\\\n[b](https://c`d)\n",
            ),
            (
                "label text",
                link(vec![Inline::plain("b`")], site()),
                "\\`a\\\n[b`](https://x.test)\n",
            ),
            (
                "label image alt",
                link(vec![image()], site()),
                "\\`a\\\n[\\[b\\`\\]](https://x.test)\n",
            ),
            ("image alt", image(), "\\`a\\\n\\[b\\`\\]\n"),
            ("no backtick", link(vec![Inline::plain("b")], site()), "`a\\\n[b](https://x.test)\n"),
        ] {
            let inlines = vec![Inline::plain("`a"), Inline::LineBreak, later];
            let doc = Document { blocks: vec![Block::Paragraph(inlines)], ..Document::default() };
            assert_eq!(document_to_markdown(&doc), expected, "{label}");
        }
    }
}
