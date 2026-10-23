//! Context-sensitive minimal escaping: document text is escaped only where a
//! character could actually parse as Markdown syntax in its context.

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
    /// The run begins at the start of an output line, where block syntax
    /// (headings, list markers, setext underlines) could form.
    pub at_line_start: bool,
    /// The run is wrapped in emphasis delimiters, so delimiter characters
    /// inside it always need escaping.
    pub styled: bool,
    /// The character following the run is unknown or active markup; pairable
    /// delimiters must assume the worst.
    pub trailing_active: bool,
    /// The rest of the line starts with a non-whitespace character that is
    /// not active markup (a hard break's backslash, an anchor tag), so a
    /// run-final delimiter can still be left-flanking.
    pub trailing_nonspace: bool,
    /// Delimiters that later runs on the same rendered line will emit; a
    /// delimiter in this run can pair with one of them across the run seam.
    pub trailing_delims: Delims,
    /// Inside a link label / image alt, where an unmatched `]` (or `[`)
    /// would terminate the label early.
    pub in_label: bool,
    /// The run's first character, where it would join the delimiter run written right before
    /// the run: it is escaped with the rest of its own run.
    pub joining: Option<char>,
}

/// Set of pairable delimiter characters (`*` `_` `~` `` ` `` `]` `$`).
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

    /// Record the partners `text` contributes when emitted as document
    /// text: every backtick and `]`, plus the emphasis delimiters that
    /// can close.
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

/// The runs of one repeated character in `text`: the byte offsets each
/// starts and ends at, and its character.
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

/// Slot for the run of `c` at `start..end` of `text` when it can act as a
/// pairing partner. Backticks and `]` always can: code spans pair by
/// backtick-string length (even a backslash-escaped backtick still closes
/// one) and brackets pair as link structure. `*`, `_` and `~` pair by
/// flanking, and `$` closes math only after a non-space and before a
/// non-digit, so those count only where they can close.
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

/// Whether a `$` run between `prev` and `next` could close a math span: not
/// preceded by whitespace, not followed by a digit. Unknown neighbours at the
/// edges assume the worst.
fn can_close_math(prev: Option<char>, next: Option<char>) -> bool {
    !prev.is_some_and(char::is_whitespace) && !next.is_some_and(|n| n.is_ascii_digit())
}

/// Whether a run of the emphasis or strikethrough character `c` between
/// `prev` and `next` could close a pair: approximate right-flanking (not
/// preceded by whitespace, nor preceded by punctuation with a word character
/// after), plus the intraword exclusion for `_`. Unknown neighbours at the
/// edges assume the worst; the punctuation test stays ASCII so an
/// unclassified character never suppresses a genuine closer.
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

/// Whether a `$` in `text` could close a math span once every other
/// non-space character is masked: one not preceded by whitespace.
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
    /// Whether `c`, the first character of `rest` and at byte `at` of the text, is written
    /// escaped.
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
            ..
        } = self.opts;
        let next = rest.chars().nth(1);
        // At the run's end the next character is unknown; trailing_active assumes the worst.
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
            // Opens a math span only directly before non-space, and only
            // when a `$` that can close follows.
            '$' => next_nonspace && paired(5),
            ']' if in_label => true,
            '~' if start_of_line && rest.starts_with("~~~") => true,
            '_' if start_of_line && line_is_only(rest, '_', 3) => true,
            '`' => styled || paired(3),
            '*' => styled || start_of_line || (next_nonspace && paired(0)),
            '_' => {
                let prev_alnum = prev.is_some_and(char::is_alphanumeric);
                let next_alnum = next.is_some_and(char::is_alphanumeric);
                styled || (next_nonspace && !(prev_alnum && next_alnum) && paired(1))
            }
            '~' => styled || (next_nonspace && paired(2)),
            '[' => in_label || paired(4),
            '<' => next.is_some_and(|n| n.is_ascii_alphabetic() || matches!(n, '/' | '!' | '?')),
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

/// Whether the `&` that starts `rest` opens a character reference.
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
            // Raw pipes split GFM table cells.
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
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let opts = EscapeOpts { trailing_active: true, in_label: true, ..Default::default() };
    escape_text(&controls_to_spaces(url), ctx, opts, out)
}

pub(super) fn escape_marker_label(
    label: &str,
    ctx: InlineContext,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let opts = EscapeOpts {
        // List-item content re-opens block syntax after the `- ` marker.
        at_line_start: ctx == InlineContext::Block,
        trailing_active: true,
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

/// Shortest backtick fence longer than any backtick run in `text`.
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

/// Escapes each `$` no odd backslash run escapes, as it would close math; off a block a line break
/// ends math, so it becomes a space, and a cell escapes a bare `|`, since GFM splits rows first.
pub(super) fn escape_math(
    tex: &str,
    site: MathSite,
    mut write: impl FnMut(&str) -> Result<(), ConvertError>,
) -> Result<(), ConvertError> {
    let mut flushed = 0;
    let mut backslashes = 0;
    for (i, c) in tex.char_indices() {
        let newline = c == '\n' && site != MathSite::Block;
        let escaped = c == '$' || (c == '|' && site == MathSite::TableCell);
        if newline || (escaped && backslashes % 2 == 0) {
            write(tex.get(flushed..i).unwrap_or_default())?;
            if newline {
                write(" ")?;
                flushed = i + 1;
            } else {
                write("\\")?;
                flushed = i;
            }
        }
        backslashes = if c == '\\' { backslashes + 1 } else { 0 };
    }
    write(tex.get(flushed..).unwrap_or_default())
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
    fn math_source_is_written_in_the_pieces_its_site_escapes() {
        for (label, site, tex, pieces) in [
            ("block escapes a bare dollar", MathSite::Block, "a$b", &["a", "\\", "$b"][..]),
            ("block keeps an escaped dollar", MathSite::Block, "a\\$b", &["a\\$b"]),
            ("block escapes after an even run", MathSite::Block, "a\\\\$b", &["a\\\\", "\\", "$b"]),
            ("block leaves a pipe and a line break", MathSite::Block, "a|b\nc", &["a|b\nc"]),
            ("span writes a line break as a space", MathSite::Span, "a\nb", &["a", " ", "b"]),
            ("span leaves a pipe", MathSite::Span, "a|b", &["a|b"]),
            (
                "span restarts the run at a line break",
                MathSite::Span,
                "\\\n$",
                &["\\", " ", "", "\\", "$"],
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
