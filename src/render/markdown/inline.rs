//! Inline run normalization and rendering.

use crate::error::ConvertError;
use crate::model::{ImageSource, Inline, LinkTarget, Style, checkbox_text};
use crate::render::markdown::Ctx;
use crate::render::markdown::escape::{
    Delims, EscapeOpts, InlineContext, backtick_fence, escape_text, escape_url_as_text, format_url,
    masked_dollar_closes,
};
use crate::render::markdown::sink::Text;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;

pub(crate) enum Norm<'a> {
    Text { text: Cow<'a, str>, style: Style },
    Link { content: &'a [Inline], target: &'a LinkTarget },
    Image { alt: &'a str, source: &'a ImageSource },
    Anchor(&'a str),
    NoteRef(&'a str),
    LineBreak,
    Math(&'a str),
    Checkbox(bool),
}

/// Single-pass normalization: drops empty runs, strips styling from
/// whitespace-only runs, merges adjacent same-style runs, and re-joins styled
/// runs split only by whitespace (`**a** **b**` -> `**a b**`). Runs borrow
/// from the source inlines; text is owned only where runs actually merge.
/// Untargeted anchors drop out here: they render as nothing, so leaving them
/// in would part runs that belong together.
pub(crate) fn normalize<'a>(inlines: &'a [Inline], rc: &Ctx) -> Vec<Norm<'a>> {
    let mut out: Vec<Norm<'a>> = Vec::new();
    for inline in inlines {
        match inline {
            Inline::Text { text, style } => {
                if text.is_empty() {
                    continue;
                }
                let style = if text.trim().is_empty() { Style::PLAIN } else { *style };
                if let Some(Norm::Text { text: prev, style: prev_style }) = out.last_mut()
                    && *prev_style == style
                {
                    prev.to_mut().push_str(text);
                    continue;
                }
                // Bridge: [styled S][ws plain][incoming styled S] merges into one run.
                if style != Style::PLAIN
                    && !style.code
                    && out.len() >= 2
                    && matches!(&out[out.len() - 1],
                        Norm::Text { text: ws, style: s } if *s == Style::PLAIN && ws.trim().is_empty())
                    && matches!(&out[out.len() - 2],
                        Norm::Text { style: s, .. } if *s == style)
                {
                    let Some(Norm::Text { text: ws, .. }) = out.pop() else { unreachable!() };
                    let Some(Norm::Text { text: prev, .. }) = out.last_mut() else {
                        unreachable!()
                    };
                    let prev = prev.to_mut();
                    prev.push_str(&ws);
                    prev.push_str(text);
                    continue;
                }
                out.push(Norm::Text { text: Cow::Borrowed(text.as_str()), style });
            }
            Inline::Link { content, target } => {
                if target.is_empty() {
                    // No usable destination: keep the content as plain inlines.
                    let runs = normalize(content, rc);
                    if !runs.iter().all(run_is_blank) {
                        out.extend(runs);
                    }
                    continue;
                }
                out.push(Norm::Link { content, target });
            }
            Inline::Image { alt, source } => out.push(Norm::Image { alt, source }),
            Inline::Anchor(id) if rc.anchors.html_id(id).is_none() => continue,
            Inline::Anchor(id) => out.push(Norm::Anchor(id)),
            Inline::NoteRef(id) => out.push(Norm::NoteRef(id)),
            Inline::LineBreak => out.push(Norm::LineBreak),
            Inline::Math(tex) if tex.trim().is_empty() => continue,
            Inline::Math(tex) => out.push(Norm::Math(tex.trim())),
            Inline::Checkbox(checked) => out.push(Norm::Checkbox(*checked)),
        }
    }
    out
}

/// Whether `run` renders nothing visible, as
/// [`inlines_are_empty`](crate::model::inlines_are_empty) judges
/// the inlines it came from.
fn run_is_blank(run: &Norm) -> bool {
    match run {
        Norm::Text { text, .. } => text.trim().is_empty(),
        Norm::Anchor(_) | Norm::LineBreak => true,
        Norm::Link { .. }
        | Norm::Image { .. }
        | Norm::NoteRef(_)
        | Norm::Math(_)
        | Norm::Checkbox(_) => false,
    }
}

/// What each link's content emits for the lookahead, computed once per link
/// and keyed by the content's address, so nested links cost linear time.
#[derive(Default)]
pub(crate) struct LinkMemo {
    plain: RefCell<HashMap<*const Inline, Delims>>,
    backtick: RefCell<HashMap<*const Inline, bool>>,
    #[cfg(test)]
    pub(crate) computed: std::cell::Cell<usize>,
}

impl LinkMemo {
    fn get_or<T: Copy>(
        &self,
        memo: &RefCell<HashMap<*const Inline, T>>,
        content: &[Inline],
        compute: impl FnOnce() -> T,
    ) -> T {
        if let Some(value) = memo.borrow().get(&content.as_ptr()) {
            return *value;
        }
        #[cfg(test)]
        self.computed.set(self.computed.get() + 1);
        let value = compute();
        memo.borrow_mut().insert(content.as_ptr(), value);
        value
    }
}

/// Render `inlines` into `out`, as if `out` started where they start.
pub(crate) fn render_inlines(
    inlines: &[Inline],
    ctx: InlineContext,
    rc: &Ctx,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    render_inlines_mode(inlines, ctx, false, rc, out)
}

fn render_inlines_mode(
    inlines: &[Inline],
    ctx: InlineContext,
    in_label: bool,
    rc: &Ctx,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let start = out.len();
    let runs = normalize(inlines, rc);
    let suffix = delims_ahead(&runs, rc);
    for (idx, run) in runs.iter().enumerate() {
        match run {
            Norm::Text { text, style } => {
                let next = runs.get(idx + 1);
                let next_active = matches!(
                    next,
                    Some(Norm::Link { .. } | Norm::Image { .. } | Norm::NoteRef(_) | Norm::Math(_))
                ) || matches!(
                    next,
                    Some(Norm::Text { style, .. }) if *style != Style::PLAIN
                );
                // A hard break renders as `\`, an anchor as `<a ...>`: not
                // markup, but a nonspace character a run-final delimiter can
                // be left-flanking against.
                let next_nonspace = matches!(next, Some(Norm::Anchor(_) | Norm::Checkbox(_)))
                    || (matches!(next, Some(Norm::LineBreak)) && ctx != InlineContext::Heading);
                let opts = EscapeOpts {
                    at_line_start: out.len() == start || out.as_str().ends_with('\n'),
                    trailing_active: next_active,
                    trailing_nonspace: next_nonspace,
                    trailing_delims: suffix[idx + 1],
                    in_label,
                    ..Default::default()
                };
                render_text_run(text, *style, ctx, opts, out)?;
            }
            Norm::NoteRef(id) => {
                if let Some(num) = rc.nums.get(*id) {
                    out.push_str(&format!("[^{num}]"))?;
                }
            }
            Norm::Link { content, target } => render_link(content, target, ctx, rc, out)?,
            Norm::Image { alt, source } => render_image(alt, source, ctx, in_label, out)?,
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
                out.push_str(checkbox_text(*checked))?;
                // The token stands apart from a caption that follows it.
                if matches!(runs.get(idx + 1), Some(run) if !starts_with_space(run)) {
                    out.push(' ')?;
                }
            }
        }
    }
    Ok(())
}

/// Render a link in place: its label goes straight into `out`, and is taken
/// back when it shows nothing.
fn render_link(
    content: &[Inline],
    target: &LinkTarget,
    ctx: InlineContext,
    rc: &Ctx,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let url = match target {
        LinkTarget::External(url) | LinkTarget::Relative(url) => Cow::Borrowed(url.as_str()),
        LinkTarget::Anchor(id) => match rc.anchors.fragment(id) {
            Some(fragment) => Cow::Owned(format!("#{fragment}")),
            // Target exists nowhere in the document: degrade to plain text.
            None => return render_inlines_mode(content, ctx, false, rc, out),
        },
    };
    let mark = out.len();
    out.push('[')?;
    let label = out.len();
    render_inlines_mode(content, ctx, true, rc, out)?;
    // Emptiness is tested on the trimmed label, but the rendered label keeps
    // its source-significant edge spaces.
    if out.since(label).trim().is_empty() {
        out.truncate(mark);
        if matches!(target, LinkTarget::Anchor(_)) {
            return Ok(());
        }
        out.push('[')?;
        escape_url_as_text(&url, ctx, out)?;
    }
    out.push_str("](")?;
    format_url(&url, out)?;
    out.push(')')
}

fn render_image(
    alt: &str,
    source: &ImageSource,
    ctx: InlineContext,
    in_label: bool,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    match source {
        ImageSource::External(url) => {
            out.push_str("![")?;
            escape_text(alt.trim(), ctx, EscapeOpts { in_label: true, ..Default::default() }, out)?;
            out.push_str("](")?;
            format_url(url, out)?;
            out.push(')')
        }
        // Embedded assets render as their alt text: Markdown cannot embed
        // bytes, and the bytes stay available in `Document::assets`. A
        // source-less image has only its alt text to offer.
        ImageSource::Asset(_) | ImageSource::Unavailable => {
            if alt.trim().is_empty() {
                return Ok(());
            }
            escape_text(alt.trim(), ctx, EscapeOpts { in_label, ..Default::default() }, out)
        }
    }
}

/// Pairable delimiters the remaining runs will emit into the current rendered
/// line: closer-capable literals in plain text, plus the markup that styled runs,
/// code spans, links and images produce. A delimiter in an earlier run can
/// pair with any of them across the run seam, hard breaks included.
/// The delimiters each suffix of `runs` emits, indexed by where the suffix
/// starts, so one reverse pass answers every run's lookahead.
fn delims_ahead(runs: &[Norm], rc: &Ctx) -> Vec<Delims> {
    let mut suffix = vec![Delims::default(); runs.len() + 1];
    for idx in (0..runs.len()).rev() {
        let mut delims = suffix[idx + 1];
        delims.union(delims_of(&runs[idx], rc));
        suffix[idx] = delims;
    }
    suffix
}

/// What one run contributes to a later run's pairing partners.
fn delims_of(run: &Norm, rc: &Ctx) -> Delims {
    let mut delims = Delims::default();
    match run {
        Norm::Text { style, .. } if style.code => delims.insert('`'),
        Norm::Text { text, style } if *style == Style::PLAIN => delims.insert_closers(text),
        Norm::Text { text, style } => {
            // Emphasis content is escaped, which neutralizes everything
            // but backticks: code spans ignore backslash escapes, so an
            // emitted `\`` still closes a span an earlier raw backtick
            // opens. `]` is the one character escaping leaves raw.
            if style.bold || style.italic {
                delims.insert('*');
            }
            if style.strike {
                delims.insert('~');
            }
            if text.contains('`') {
                delims.insert('`');
            }
            if text.contains(']') {
                delims.insert(']');
            }
            if masked_dollar_closes(text) {
                delims.insert('$');
            }
        }
        Norm::Link { content, target } => match target {
            // An unresolved target degrades to its rendered content
            // (see render_link), which emits like any sibling runs.
            LinkTarget::Anchor(id) if rc.anchors.fragment(id).is_none() => {
                delims.union(rc.links.get_or(&rc.links.plain, content, || {
                    let mut plain = Delims::default();
                    for run in &normalize(content, rc) {
                        plain.union(delims_of(run, rc));
                    }
                    plain
                }));
            }
            // Emphasis cannot cross a link boundary, but a code span
            // can: a backtick in the label or destination pairs with
            // one outside.
            _ => {
                if emits_backtick(content, rc) || target_has_backtick(target) {
                    delims.insert('`');
                }
            }
        },
        Norm::Image { alt, source } => match source {
            ImageSource::External(_) if alt.contains('`') => delims.insert('`'),
            ImageSource::External(_) => {}
            // Sourceless images degrade to their alt as plain text.
            ImageSource::Asset(_) | ImageSource::Unavailable => delims.insert_closers(alt),
        },
        Norm::NoteRef(_)
        | Norm::Anchor(_)
        | Norm::LineBreak
        | Norm::Math(_)
        | Norm::Checkbox(_) => {}
    }
    delims
}

fn starts_with_space(run: &Norm) -> bool {
    match run {
        Norm::Text { text, .. } => text.starts_with(char::is_whitespace),
        Norm::LineBreak => true,
        _ => false,
    }
}

fn target_has_backtick(target: &LinkTarget) -> bool {
    match target {
        LinkTarget::External(url) | LinkTarget::Relative(url) => url.contains('`'),
        LinkTarget::Anchor(_) => false,
    }
}

/// True when rendering `inlines` inside a link label emits a backtick an
/// earlier raw one can pair with: any backtick in text counts even where the
/// label escapes it (code spans ignore backslash escapes), and a code run
/// emits fences unless it is whitespace-only and loses its styling.
fn emits_backtick(inlines: &[Inline], rc: &Ctx) -> bool {
    rc.links.get_or(&rc.links.backtick, inlines, || {
        inlines.iter().any(|inline| match inline {
            Inline::Text { text, style } => {
                text.contains('`') || (style.code && !text.trim().is_empty())
            }
            Inline::Link { content, target } => {
                emits_backtick(content, rc) || target_has_backtick(target)
            }
            Inline::Image { alt, .. } => alt.contains('`'),
            _ => false,
        })
    })
}

/// Emit a styled run, moving edge whitespace outside the delimiters.
/// `opts` carries the line start and the trailing context of a plain run;
/// `styled` is filled in here.
fn render_text_run(
    text: &str,
    style: Style,
    ctx: InlineContext,
    opts: EscapeOpts,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    if style == Style::PLAIN {
        return escape_text(text, ctx, opts, out);
    }
    let core_start = text.len() - text.trim_start().len();
    let core_end = text.trim_end().len();
    let (lead, core, trail) = (&text[..core_start], &text[core_start..core_end], &text[core_end..]);
    out.push_str(lead)?;
    if !core.is_empty() {
        if style.code {
            push_code_span(core, ctx, out)?;
        } else {
            let mut open = String::new();
            if style.strike {
                open.push_str("~~");
            }
            if style.bold {
                open.push_str("**");
            }
            if style.italic {
                open.push('*');
            }
            let close: String = open.chars().rev().collect();
            out.push_str(&open)?;
            escape_text(
                core,
                ctx,
                EscapeOpts { styled: true, in_label: opts.in_label, ..Default::default() },
                out,
            )?;
            out.push_str(&close)?;
        }
    }
    out.push_str(trail)
}

/// GFM inline math: `$` hugging both ends of the source. A line break
/// inside it would end the paragraph's math span, and a bare `$` (never
/// valid inside math) would close it early. A row is split into cells
/// before the math span is parsed, so in a cell a bare pipe is syntax too;
/// GFM strips the escaping backslash before the math is read, so an already
/// escaped pipe stays as it is.
pub(crate) fn push_math_span(
    tex: &str,
    ctx: InlineContext,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let tex = tex.trim();
    out.push('$')?;
    let mut flushed = 0;
    let mut backslashes = 0;
    for (i, c) in tex.char_indices() {
        let escaped = c == '$' || (c == '|' && ctx == InlineContext::TableCell);
        if c == '\n' || (escaped && backslashes % 2 == 0) {
            out.push_str(tex.get(flushed..i).unwrap_or_default())?;
            if c == '\n' {
                out.push(' ')?;
                flushed = i + 1;
            } else {
                out.push('\\')?;
                flushed = i;
            }
        }
        backslashes = if c == '\\' { backslashes + 1 } else { 0 };
    }
    out.push_str(tex.get(flushed..).unwrap_or_default())?;
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
