//! Context-sensitive minimal escaping: document text is escaped only where a
//! character could actually parse as Markdown syntax in its context.

use crate::error::ConvertError;
use crate::render::markdown::sink::Text;

/// Where an inline run is being rendered; controls which characters can be
/// syntax there.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum InlineContext {
    Block,
    Heading,
    TableCell,
}

/// Fine-grained escaping context beyond [`InlineContext`]: where the run
/// sits relative to its surroundings.
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
}

/// Set of pairable delimiter characters (`*` `_` `~` `` ` `` `]` `$`).
#[derive(Clone, Copy, Default)]
pub(crate) struct Delims([bool; 6]);

impl Delims {
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

    pub(crate) fn union(&mut self, other: Delims) {
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
    let prev = text.get(..start).and_then(|before| before.chars().next_back());
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
    runs(text).any(|(start, _, c)| {
        c == '$'
            && !text
                .get(..start)
                .and_then(|b| b.chars().next_back())
                .is_some_and(char::is_whitespace)
    })
}

/// Escape Markdown syntax in document text into `out`.
pub(crate) fn escape_text(
    text: &str,
    ctx: InlineContext,
    opts: EscapeOpts,
    out: &mut Text<'_>,
) -> Result<(), ConvertError> {
    let EscapeOpts {
        at_line_start,
        styled,
        trailing_active,
        trailing_nonspace,
        trailing_delims,
        in_label,
    } = opts;
    // Last position of each delimiter that can pair; one with no later
    // partner is inert.
    let mut last: [Option<usize>; 6] = [None; 6]; // * _ ~ ` ] $
    for (start, end, c) in runs(text) {
        if let Some(slot) = partner_slot(text, start, end, c).and_then(|s| last.get_mut(s)) {
            *slot = Some(end - c.len_utf8());
        }
    }
    let mut line_has_content = !(at_line_start && ctx == InlineContext::Block);
    let mut flushed = 0;
    let mut prev: Option<char> = None;
    let mut i = 0;
    while let Some(rest) = text.get(i..)
        && let Some(c) = rest.chars().next()
    {
        let after = i + c.len_utf8();
        if c == '\n' {
            if ctx == InlineContext::Block {
                line_has_content = false;
            }
            prev = Some(c);
            i = after;
            continue;
        }
        let start_of_line = !line_has_content;
        if !c.is_whitespace() {
            line_has_content = true;
        }
        let next = rest.chars().nth(1);
        // At the run's end the next character is unknown; trailing_active assumes the worst.
        let next_nonspace =
            next.map_or(trailing_active || trailing_nonspace, |n| !n.is_whitespace());
        let paired = |slot: usize| {
            trailing_active
                || trailing_delims.contains(slot)
                || last.get(slot).copied().flatten().is_some_and(|j| j > i)
        };
        let escape = match c {
            '\\' => true,
            // Opens a math span only directly before non-space, and only
            // when a `$` that can close follows.
            '$' => next_nonspace && paired(5),
            ']' if in_label => true,
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
            '|' if ctx == InlineContext::TableCell => true,
            '&' if entity_ahead(rest) => {
                out.push_str(text.get(flushed..i).unwrap_or_default())?;
                out.push_str("&amp;")?;
                flushed = after;
                prev = Some(c);
                i = after;
                continue;
            }
            '#' if start_of_line => {
                rest.trim_start_matches('#').chars().next().is_none_or(char::is_whitespace)
            }
            '-' if start_of_line => !next_nonspace || line_is_only(rest, '-'),
            '+' if start_of_line => !next_nonspace,
            '>' if start_of_line => true,
            '=' if start_of_line => line_is_only(rest, '='),
            '0'..='9' if start_of_line => {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                let mut tail = rest.get(digits..).unwrap_or_default().chars();
                if let Some(mark @ ('.' | ')')) = tail.next()
                    && tail.next().is_none_or(char::is_whitespace)
                {
                    let at = i + digits;
                    out.push_str(text.get(flushed..at).unwrap_or_default())?;
                    out.push('\\')?;
                    flushed = at;
                    prev = Some(mark);
                    i = at + mark.len_utf8();
                    continue;
                }
                false
            }
            _ => false,
        };
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

/// True when the rest of the current line is just `c`, spaces, and tabs
/// (a setext underline or thematic break).
fn line_is_only(rest: &str, c: char) -> bool {
    rest.chars().take_while(|&ch| ch != '\n').all(|ch| ch == c || ch == ' ' || ch == '\t')
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

/// Write a link destination into `out`, angle-bracketing when needed.
pub(crate) fn format_url(url: &str, out: &mut Text<'_>) -> Result<(), ConvertError> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
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
            out.push('%')?;
            for nibble in [byte >> 4, byte & 0x0F] {
                out.push(char::from(HEX.get(usize::from(nibble)).copied().unwrap_or(b'0')))?;
            }
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
    let cleaned: String = url.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    escape_text(
        &cleaned,
        ctx,
        EscapeOpts { trailing_active: true, in_label: true, ..Default::default() },
        out,
    )
}

/// Shortest backtick fence longer than any backtick run in `text`.
pub(crate) fn backtick_fence(text: &str, min: usize) -> String {
    let longest_run = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat((longest_run + 1).max(min))
}
