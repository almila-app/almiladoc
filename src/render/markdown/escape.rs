use std::borrow::Cow;

use crate::error::ConvertError;
use crate::render::markdown::sink::Text;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum InlineContext {
    Block,
    Heading,
    TableCell,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct EscapeOpts {
    pub at_line_start: bool,
    /// The run is wrapped in emphasis delimiters, so delimiter characters
    /// inside it always need escaping.
    pub styled: bool,
    pub trailing_active: bool,
    /// The rest of the line starts with a non-whitespace character that is
    /// not active markup (a hard break's backslash, an anchor tag), so a
    /// run-final delimiter can still be left-flanking.
    pub trailing_nonspace: bool,
    pub trailing_delims: Delims,
    /// Inside a link label / image alt, where an unmatched `]` (or `[`)
    /// would terminate the label early.
    pub in_label: bool,
    /// The run's first character, where it would join the delimiter run written right before
    /// the run: it is escaped with the rest of its own run.
    pub joining: Option<char>,
    /// A styled run follows, whose delimiters and code fence an email address continues through.
    pub trailing_marked: bool,
    pub sharing: Sharing,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Sharing {
    #[default]
    Alone,
    /// Shares an inline context with separately rendered text (cell parts, a table's cells), so
    /// each backtick and `[` is escaped.
    Parts,
    /// Shares an inline context with math, which pairs by its dollars, so each `$`, backtick and
    /// bracket, and each `<` a non-space follows, is escaped.
    Math,
}

impl Sharing {
    pub(crate) fn math_if(holds_math: bool) -> Self {
        if holds_math { Self::Math } else { Self::Alone }
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Delims([bool; 6]);

impl Delims {
    pub(crate) const NONE: Self = Self([false; 6]);

    fn slot(c: char) -> Option<usize> {
        match c {
            '*' => Some(0),
            '_' => Some(1),
            '~' => Some(2),
            '`' => Some(3),
            ']' => Some(4),
            '$' => Some(5),
            _ => None,
        }
    }

    pub(crate) fn insert(&mut self, c: char) {
        if let Some(slot) = Self::slot(c) {
            self.0[slot] = true;
        }
    }

    pub(crate) fn insert_closers(&mut self, text: &str) {
        for (start, end, c) in runs(text) {
            if let Some(set) = partner_slot(text, start, end, c).and_then(|s| self.0.get_mut(s)) {
                *set = true;
            }
        }
    }

    pub(crate) fn union(&mut self, other: Self) {
        for (slot, set) in self.0.iter_mut().zip(other.0) {
            *slot |= set;
        }
    }

    fn contains(self, slot: usize) -> bool {
        self.0[slot]
    }
}

fn runs(text: &str) -> impl Iterator<Item = (usize, usize, char)> + '_ {
    let mut chars = text.char_indices().peekable();
    std::iter::from_fn(move || {
        let (start, c) = chars.next()?;
        let mut end = start + c.len_utf8();
        while let Some(&(at, next)) = chars.peek()
            && next == c
        {
            end = at + next.len_utf8();
            chars.next();
        }
        Some((start, end, c))
    })
}

/// `*`, `_` and `~` pair by flanking, and `$` closes math only after a non-space and before a non-digit, so those count only where they can close.
fn partner_slot(text: &str, start: usize, end: usize, c: char) -> Option<usize> {
    let slot = Delims::slot(c)?;
    let prev = char_before(text, start);
    let next = text.get(end..).and_then(|after| after.chars().next());
    let closes = match c {
        '`' | ']' => true,
        '$' => can_close_math(prev, next),
        _ => can_close(c, prev, next),
    };
    closes.then_some(slot)
}

fn can_close_math(prev: Option<char>, next: Option<char>) -> bool {
    !prev.is_some_and(char::is_whitespace) && !next.is_some_and(|n| n.is_ascii_digit())
}

/// Unknown neighbours at the edges assume the worst; the punctuation test stays ASCII so an unclassified character never suppresses a genuine closer.
fn can_close(c: char, prev: Option<char>, next: Option<char>) -> bool {
    if prev.is_some_and(char::is_whitespace) {
        return false;
    }
    if prev.is_some_and(|p| p.is_ascii_punctuation()) && next.is_some_and(char::is_alphanumeric) {
        return false;
    }
    c != '_'
        || !(prev.is_some_and(char::is_alphanumeric) && next.is_some_and(char::is_alphanumeric))
}

pub(crate) fn masked_dollar_closes(text: &str) -> bool {
    runs(text)
        .any(|(start, _, c)| c == '$' && !char_before(text, start).is_some_and(char::is_whitespace))
}

fn char_before(text: &str, at: usize) -> Option<char> {
    text.get(..at).and_then(|before| before.chars().next_back())
}

fn ordered_marker(text: &str) -> Option<(usize, char)> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let mut tail = text.get(digits..)?.chars();
    let mark = tail.next().filter(|c| matches!(c, '.' | ')'))?;
    tail.next().is_none_or(char::is_whitespace).then_some((digits, mark))
}

struct Scan {
    ctx: InlineContext,
    opts: EscapeOpts,
    last: [Option<usize>; 6],
}

impl Scan {
    fn escapes(
        &self,
        c: char,
        at: usize,
        rest: &str,
        prev: Option<char>,
        start_of_line: bool,
        out: &Text<'_>,
    ) -> bool {
        let EscapeOpts {
            styled,
            trailing_active,
            trailing_nonspace,
            trailing_delims,
            in_label,
            trailing_marked,
            sharing,
            ..
        } = self.opts;
        let in_label = in_label || sharing == Sharing::Math;
        let next = rest.chars().nth(1);
        let next_nonspace =
            next.map_or(trailing_active || trailing_nonspace, |n| !n.is_whitespace());
        let paired = |slot: usize| {
            trailing_active
                || trailing_delims.contains(slot)
                || self.last.get(slot).copied().flatten().is_some_and(|j| j > at)
        };
        match c {
            '\\' => true,
            '(' | ':' if at == 0 => note_syntax(c, self.ctx, out.after_note()),
            '$' => sharing == Sharing::Math || (next_nonspace && paired(5)),
            ']' if in_label => true,
            '~' if start_of_line && rest.starts_with("~~~") => true,
            '_' if start_of_line && line_is_only(rest, '_', 3) => true,
            '`' => styled || sharing != Sharing::Alone || paired(3),
            '*' => styled || start_of_line || (next_nonspace && paired(0)),
            '_' => {
                let prev_alnum = prev.is_some_and(char::is_alphanumeric);
                let next_alnum = next.is_some_and(char::is_alphanumeric);
                styled || (next_nonspace && !(prev_alnum && next_alnum) && paired(1))
            }
            '~' => styled || (next_nonspace && paired(2)),
            '[' => in_label || sharing != Sharing::Alone || paired(4),
            '<' => {
                opens_markup(next)
                    || opens_email(rest.get(1..).unwrap_or_default(), styled || trailing_marked)
                    || (sharing == Sharing::Math && !next.is_some_and(char::is_whitespace))
            }
            '!' => next.is_none() && trailing_active,
            '|' if self.ctx == InlineContext::TableCell => true,
            '#' if start_of_line => {
                rest.trim_start_matches('#').chars().next().is_none_or(char::is_whitespace)
            }
            '-' if start_of_line => !next_nonspace || line_is_only(rest, '-', 1),
            '+' if start_of_line => !next_nonspace,
            '>' if start_of_line => true,
            '=' if start_of_line => line_is_only(rest, '=', 1),
            _ => false,
        }
    }
}

pub(crate) fn escape_text(
    text: &str,
    ctx: InlineContext,
    opts: EscapeOpts,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let scan = Scan { ctx, opts, last: last_partners(text) };
    let mut line_has_content = !(opts.at_line_start && ctx == InlineContext::Block);
    let mut flushed = 0;
    let mut prev: Option<char> = None;
    let mut escaped_delimiter = opts.joining;
    let mut i = 0;
    while let Some(rest) = text.get(i..)
        && let Some(c) = rest.chars().next()
    {
        let after = i + c.len_utf8();
        if matches!(c, '\r' | '\n') {
            if ctx == InlineContext::Block {
                line_has_content = false;
            }
            prev = Some(c);
            escaped_delimiter = None;
            i = after;
            continue;
        }
        let start_of_line = !line_has_content;
        if !c.is_whitespace() {
            line_has_content = true;
        }
        if c == '&' && entity_ahead(rest) {
            out.push_str(text.get(flushed..i).unwrap_or_default())?;
            out.push_str("&amp;")?;
            flushed = after;
            prev = Some(c);
            i = after;
            continue;
        }
        if start_of_line
            && c.is_ascii_digit()
            && let Some((digits, mark)) = ordered_marker(rest)
        {
            let at = i + digits;
            out.push_str(text.get(flushed..at).unwrap_or_default())?;
            out.push('\\')?;
            flushed = at;
            prev = Some(mark);
            i = at + mark.len_utf8();
            continue;
        }
        let escape =
            scan.escapes(c, i, rest, prev, start_of_line, out) || escaped_delimiter == Some(c);
        escaped_delimiter =
            if escape && matches!(c, '*' | '_' | '~' | '`') { Some(c) } else { None };
        if escape {
            out.push_str(text.get(flushed..i).unwrap_or_default())?;
            out.push('\\')?;
            flushed = i;
        }
        prev = Some(c);
        i = after;
    }
    out.push_str(text.get(flushed..).unwrap_or_default())
}

/// Whether `c`, written right after a note reference, would turn it into
/// syntax: `(` into a link destination, and `:` into a note definition
/// where the reference opens a line of a block (`opens_line`).
fn note_syntax(c: char, ctx: InlineContext, after_note: Option<bool>) -> bool {
    after_note.is_some_and(|opens_line| {
        c == '(' || (c == ':' && opens_line && ctx == InlineContext::Block)
    })
}

fn last_partners(text: &str) -> [Option<usize>; 6] {
    let mut last = [None; 6];
    for (start, end, c) in runs(text) {
        if let Some(slot) = partner_slot(text, start, end, c).and_then(|s| last.get_mut(s)) {
            *slot = Some(end - c.len_utf8());
        }
    }
    last
}

fn line_is_only(rest: &str, c: char, minimum: usize) -> bool {
    let line = rest.split(['\r', '\n']).next().unwrap_or_default();
    line.chars().all(|ch| ch == c || matches!(ch, ' ' | '\t'))
        && line.chars().filter(|&ch| ch == c).count() >= minimum
}

fn entity_ahead(rest: &str) -> bool {
    let after = rest.get(1..).unwrap_or_default();
    if after.starts_with('#') {
        return true;
    }
    let name = after.bytes().take_while(u8::is_ascii_alphanumeric).count();
    name > 0 && after.as_bytes().get(name) == Some(&b';')
}

/// Write a link destination into `out` so a Markdown parser reads back exactly `url`:
/// angle-bracketed when needed, a backslash and an `&` that opens a character reference escaped.
pub(crate) fn format_url(url: &str, out: &mut Text<'_>) -> Result<(), ConvertError> {
    // Controls are encoded below, so only the whitespace that stays raw
    // needs the brackets.
    let bracketed =
        url.chars().any(|c| (c.is_whitespace() && !c.is_control()) || c == '(' || c == ')');
    if bracketed {
        out.push('<')?;
    }
    let mut flushed = 0;
    for (i, c) in url.char_indices() {
        let encoded = match c {
            '<' => "%3C",
            '>' => "%3E",
            '|' => "%7C",
            '\\' => "\\\\",
            '&' if entity_ahead(url.get(i..).unwrap_or_default()) => "\\&",
            _ if !c.is_control() => continue,
            _ => "",
        };
        out.push_str(url.get(flushed..i).unwrap_or_default())?;
        flushed = i + c.len_utf8();
        if !encoded.is_empty() {
            out.push_str(encoded)?;
            continue;
        }
        // Encode controls so they cannot split the Markdown output.
        for byte in c.encode_utf8(&mut [0; 4]).bytes() {
            out.push_str(&format!("%{byte:02X}"))?;
        }
    }
    out.push_str(url.get(flushed..).unwrap_or_default())?;
    if bracketed {
        out.push('>')?;
    }
    Ok(())
}

pub(crate) fn escape_url_as_text(
    url: &str,
    ctx: InlineContext,
    sharing: Sharing,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let opts = EscapeOpts { trailing_active: true, in_label: true, sharing, ..Default::default() };
    escape_text(&controls_to_spaces(url), ctx, opts, out)
}

pub(super) fn escape_marker_label(
    label: &str,
    ctx: InlineContext,
    sharing: Sharing,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let opts = EscapeOpts {
        at_line_start: ctx == InlineContext::Block,
        trailing_active: true,
        sharing,
        ..Default::default()
    };
    escape_text(&controls_to_spaces(label), ctx, opts, out)
}

pub(super) fn controls_to_spaces(text: &str) -> Cow<'_, str> {
    if text.contains(char::is_control) {
        Cow::Owned(text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect())
    } else {
        Cow::Borrowed(text)
    }
}

pub(crate) fn backtick_fence(text: &str, min: usize) -> String {
    let longest_run = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat((longest_run + 1).max(min))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MathSite {
    Block,
    Span,
    TableCell,
}

fn opens_markup(next: Option<char>) -> bool {
    next.is_some_and(|n| n.is_ascii_alphabetic() || matches!(n, '/' | '!' | '?'))
}

/// Whether the local-part characters after a `<` end at an `@`, or, when `continues`, at the
/// run's end.
fn opens_email(after: &str, continues: bool) -> bool {
    let local = after.find(|c: char| !is_email_local(c)).unwrap_or(after.len());
    match after.get(local..).and_then(|rest| rest.chars().next()) {
        Some(c) => c == '@' && local > 0,
        None => continues,
    }
}

fn is_email_local(c: char) -> bool {
    c.is_ascii_alphanumeric() || ".!#$%&'*+/=?^_`{|}~-".contains(c)
}

/// Escapes each `$`, backtick and unmatched brace no odd backslash run escapes, as pulldown-cmark
/// closes math only at a dollar in the brace context it opened in; off a block a line break
/// becomes a space, a cell escapes a bare `|` (GFM splits rows first) and a space follows each `<`
/// a non-space follows and each `]` before `(`, `[` or `:`.
pub(super) fn escape_math(
    tex: &str,
    site: MathSite,
    mut write: impl FnMut(&str) -> Result<(), ConvertError>,
) -> Result<(), ConvertError> {
    let mut flushed = 0;
    let mut backslashes = 0;
    let mut unmatched = unmatched_braces(tex).into_iter().peekable();
    let mut chars = tex.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let next = chars.peek().map(|&(_, next)| next);
        let escaped = matches!(c, '$' | '`')
            || (c == '|' && site == MathSite::TableCell)
            || unmatched.next_if_eq(&i).is_some();
        let cut = if c == '\n' && site != MathSite::Block {
            Some((i, " ", i + 1))
        } else if (c == '<' && next.is_some_and(|next| !next.is_whitespace()))
            || (c == ']' && matches!(next, Some('(' | '[' | ':')))
        {
            Some((i + 1, " ", i + 1))
        } else if escaped && backslashes % 2 == 0 {
            Some((i, "\\", i))
        } else {
            None
        };
        if let Some((end, insert, resume)) = cut {
            write(tex.get(flushed..end).unwrap_or_default())?;
            write(insert)?;
            flushed = resume;
        }
        backslashes = if c == '\\' { backslashes + 1 } else { 0 };
    }
    write(tex.get(flushed..).unwrap_or_default())
}

fn unmatched_braces(tex: &str) -> Vec<usize> {
    let (mut open, mut unmatched) = (Vec::new(), Vec::new());
    let mut backslashes = 0;
    for (i, c) in tex.char_indices() {
        match c {
            '{' if backslashes % 2 == 0 => open.push(i),
            '}' if backslashes % 2 == 0 && open.pop().is_none() => unmatched.push(i),
            _ => {}
        }
        backslashes = if c == '\\' { backslashes + 1 } else { 0 };
    }
    unmatched.extend(open);
    unmatched.sort_unstable();
    unmatched
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;
    use crate::render::markdown::sink::Meter;

    fn written(write: impl FnOnce(&mut Text<'_>) -> Result<(), ConvertError>) -> String {
        let ledger = Ledger::new(u64::MAX, None);
        let meter = Meter::new(&ledger, u64::MAX);
        let mut text = Text::new(&meter);
        write(&mut text).unwrap();
        text.as_str().to_owned()
    }

    #[test]
    fn controls_become_spaces_and_clean_text_stays_borrowed() {
        for (text, spaced) in [("a\tb\nc\u{85}d\u{7f}", "a b c d "), ("é ü  x", "é ü  x")] {
            let cleaned = controls_to_spaces(text);
            assert_eq!(cleaned, spaced, "{text:?}");
            assert_eq!(matches!(cleaned, Cow::Borrowed(_)), spaced == text, "{text:?}");
        }
    }

    #[test]
    fn only_digits_open_an_ordered_marker_at_a_line_start() {
        let opts = EscapeOpts { at_line_start: true, ..Default::default() };
        for (text, escaped) in [
            ("1. x", "1\\. x"),
            ("12) x", "12\\) x"),
            (". x", ". x"),
            (") x", ") x"),
            ("1.x", "1.x"),
        ] {
            let out = written(|out| escape_text(text, InlineContext::Block, opts, out));
            assert_eq!(out, escaped, "{text:?}");
        }
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of math sources and their written pieces")]
    fn math_source_is_written_in_the_pieces_its_site_escapes() {
        for (label, site, tex, pieces) in [
            ("block escapes a bare dollar", MathSite::Block, "a$b", &["a", "\\", "$b"][..]),
            ("block keeps an escaped dollar", MathSite::Block, "a\\$b", &["a\\$b"]),
            ("block escapes after an even run", MathSite::Block, "a\\\\$b", &["a\\\\", "\\", "$b"]),
            ("block leaves a pipe and a line break", MathSite::Block, "a|b\nc", &["a|b\nc"]),
            ("span writes a line break as a space", MathSite::Span, "a\nb", &["a", " ", "b"]),
            ("span leaves a pipe", MathSite::Span, "a|b", &["a|b"]),
            (
                "span escapes each brace left unmatched by its unescaped braces",
                MathSite::Span,
                "a}{b}{\\{\\\\{",
                &["a", "\\", "}{b}", "\\", "{\\{\\\\", "\\", "{"],
            ),
            (
                "block leaves balanced braces",
                MathSite::Block,
                "\\frac{a}{b_{1}}",
                &["\\frac{a}{b_{1}}"],
            ),
            (
                "span escapes each backtick no odd run escapes",
                MathSite::Span,
                "a`b``c\\`d",
                &["a", "\\", "`b", "\\", "`", "\\", "`c\\`d"],
            ),
            (
                "span restarts the run at a line break",
                MathSite::Span,
                "\\\n$",
                &["\\", " ", "", "\\", "$"],
            ),
            (
                "block spaces each opener of markup",
                MathSite::Block,
                "<script>\n</b><!x<?y",
                &["<", " ", "script>\n<", " ", "/b><", " ", "!x<", " ", "?y"],
            ),
            (
                "span spaces each opener a non-space follows",
                MathSite::Span,
                "a < b<c<1<",
                &["a < b<", " ", "c<", " ", "1<"],
            ),
            (
                "span spaces an email opener",
                MathSite::Span,
                "<1@example.com><+a@example.com>",
                &["<", " ", "1@example.com><", " ", "+a@example.com>"],
            ),
            ("cell spaces a tag opener", MathSite::TableCell, "<img>", &["<", " ", "img>"]),
            (
                "span spaces a bracket before link syntax",
                MathSite::Span,
                "![a](b)[c][d]: e]f",
                &["![a]", " ", "(b)[c]", " ", "[d]", " ", ": e]f"],
            ),
            ("cell escapes a pipe", MathSite::TableCell, "a|b", &["a", "\\", "|b"]),
            ("cell keeps an escaped pipe", MathSite::TableCell, "a\\|b", &["a\\|b"]),
            (
                "cell escapes dollar and pipe around a line break",
                MathSite::TableCell,
                "$\n|",
                &["", "\\", "$", " ", "", "\\", "|"],
            ),
        ] {
            let mut written = Vec::new();
            escape_math(tex, site, |piece| {
                written.push(piece.to_owned());
                Ok(())
            })
            .unwrap();
            assert_eq!(written, pieces, "{label}");
        }
    }

    #[test]
    fn a_url_writes_each_control_byte_as_an_uppercase_percent_escape() {
        let url = written(|out| format_url("a\u{7}b\u{85}c\u{1f}\n", out));
        assert_eq!(url, "a%07b%C2%85c%1F%0A");
    }
}
