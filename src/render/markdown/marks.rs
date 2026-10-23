//! The mark stack inline text streams through, outermost first: a shift tag, then `~~`, `**`
//! and `_`. Where marks change between two characters with no whitespace between, the delimiter
//! runs must still flank under `CommonMark`'s rules, and the source character on a side whose raw
//! value would stop them is written as a numeric character reference instead.

use crate::error::ConvertError;
use crate::model::{Style, VerticalPosition};
use crate::render::markdown::sink::Text;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mark {
    Sup,
    Sub,
    Strike,
    Bold,
    Italic,
}

impl Mark {
    fn open(self) -> &'static str {
        match self {
            Self::Sup => "<sup>",
            Self::Sub => "<sub>",
            Self::Strike => "~~",
            Self::Bold => "**",
            Self::Italic => "_",
        }
    }

    fn close(self) -> &'static str {
        match self {
            Self::Sup => "</sup>",
            Self::Sub => "</sub>",
            Self::Strike | Self::Bold | Self::Italic => self.open(),
        }
    }

    fn delimiter(self) -> Option<char> {
        match self {
            Self::Sup | Self::Sub => None,
            Self::Strike => Some('~'),
            Self::Bold => Some('*'),
            Self::Italic => Some('_'),
        }
    }
}

/// The marks one style keeps open, outermost first.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Stack([Option<Mark>; 4]);

impl Stack {
    pub(crate) const EMPTY: Self = Self([None; 4]);

    pub(crate) fn of(style: Style) -> Self {
        let shift = match style.position {
            VerticalPosition::Baseline => None,
            VerticalPosition::Superscript => Some(Mark::Sup),
            VerticalPosition::Subscript => Some(Mark::Sub),
        };
        let marks = [
            shift,
            style.strike.then_some(Mark::Strike),
            style.bold.then_some(Mark::Bold),
            style.italic.then_some(Mark::Italic),
        ];
        Self::collect(marks.into_iter().flatten())
    }

    fn collect(marks: impl Iterator<Item = Mark>) -> Self {
        let mut stack = Self::EMPTY;
        for (slot, mark) in stack.0.iter_mut().zip(marks) {
            *slot = Some(mark);
        }
        stack
    }

    fn iter(self) -> impl DoubleEndedIterator<Item = Mark> {
        self.0.into_iter().flatten()
    }

    fn len(self) -> usize {
        self.iter().count()
    }

    /// How many marks, from the outermost, `self` and `other` share.
    pub(crate) fn common(self, other: Self) -> usize {
        self.iter().zip(other.iter()).take_while(|(a, b)| a == b).count()
    }

    pub(crate) fn truncated(mut self, keep: usize) -> Self {
        for slot in self.0.iter_mut().skip(keep) {
            *slot = None;
        }
        self
    }

    pub(crate) fn delimiters(self) -> impl Iterator<Item = char> {
        self.iter().filter_map(Mark::delimiter)
    }
}

/// What a delimiter run at a seam sees on one side.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Side {
    /// Whitespace, a line end, or the edge of the text.
    Space,
    /// Markup ending or starting in ASCII punctuation: a tag, a fence, a bracket.
    Fixed,
    /// A source character, which can be written as a numeric reference.
    Char(char),
}

impl Side {
    pub(crate) fn of(c: Option<char>) -> Self {
        match c {
            Some(c) if !is_markdown_space(c) => Self::Char(c),
            _ => Self::Space,
        }
    }
}

/// `CommonMark`'s Unicode whitespace: category Zs, tab, line feed, form feed and carriage return.
fn is_markdown_space(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\u{b}' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Punct,
    Other,
}

/// The classes a side may have: the library carries no Unicode punctuation table, so a non-ASCII
/// character that is neither alphanumeric nor whitespace may be either. The circled and squared
/// Latin letters are alphabetic yet of category So, which `CommonMark` 0.31.2 counts as
/// punctuation and GitHub's older tables do not.
fn classes(side: Side) -> &'static [Class] {
    match side {
        Side::Space => &[Class::Space],
        Side::Fixed => &[Class::Punct],
        Side::Char(c) if c.is_ascii_punctuation() => &[Class::Punct],
        Side::Char('\u{24B6}'..='\u{24E9}' | '\u{1F130}'..='\u{1F189}') => {
            &[Class::Punct, Class::Other]
        }
        Side::Char(c) if c.is_ascii() || c.is_alphanumeric() => &[Class::Other],
        Side::Char(_) => &[Class::Punct, Class::Other],
    }
}

fn left_flanking(prev: Class, next: Class) -> bool {
    next != Class::Space && (next != Class::Punct || prev != Class::Other)
}

fn right_flanking(prev: Class, next: Class) -> bool {
    prev != Class::Space && (prev != Class::Punct || next != Class::Other)
}

/// `CommonMark` 0.31.2 section 6.2, rules 1 to 8; GitHub's `~~` follows the `*` rules.
fn can_open(c: char, prev: Class, next: Class) -> bool {
    left_flanking(prev, next) && (c != '_' || !right_flanking(prev, next) || prev == Class::Punct)
}

fn can_close(c: char, prev: Class, next: Class) -> bool {
    right_flanking(prev, next) && (c != '_' || !left_flanking(prev, next) || next == Class::Punct)
}

/// The marks closed and then opened at one seam.
#[derive(Clone, Copy)]
pub(crate) struct Cluster {
    pub(crate) closing: Stack,
    pub(crate) opening: Stack,
}

impl Cluster {
    /// The marks that close between `open` and `next`, and those that open after them when
    /// `next` follows directly.
    pub(crate) fn between(open: Stack, next: Stack, adjacent: bool) -> Self {
        let keep = open.common(next);
        Self {
            closing: Stack::collect(open.iter().skip(keep)),
            opening: if adjacent { Stack::collect(next.iter().skip(keep)) } else { Stack::EMPTY },
        }
    }

    fn is_empty(self) -> bool {
        self.closing.len() == 0 && self.opening.len() == 0
    }

    fn runs(self) -> impl Iterator<Item = (Option<char>, bool)> {
        let closing = self.closing.iter().rev().map(|mark| (mark.delimiter(), true));
        closing.chain(self.opening.iter().map(|mark| (mark.delimiter(), false)))
    }

    fn flanks(self, left: Class, right: Class) -> bool {
        let last = (self.closing.len() + self.opening.len()).saturating_sub(1);
        self.runs().enumerate().all(|(k, (delimiter, closing))| {
            let Some(c) = delimiter else { return true };
            let prev = if k == 0 { left } else { Class::Punct };
            let next = if k == last { right } else { Class::Punct };
            if closing { can_close(c, prev, next) } else { can_open(c, prev, next) }
        })
    }

    fn flanks_between(self, left: Side, right: Side) -> bool {
        classes(left).iter().all(|&l| classes(right).iter().all(|&r| self.flanks(l, r)))
    }

    /// Which of the two sides to write as numeric references so every run of the cluster
    /// flanks: none where the raw characters already let them, else the fewest, left first.
    pub(crate) fn references(self, left: Side, right: Side) -> (bool, bool) {
        if self.is_empty() {
            return (false, false);
        }
        let encodable = |side| matches!(side, Side::Char(_));
        let referenced = |encode: bool, side| if encode { Side::Fixed } else { side };
        [(false, false), (true, false), (false, true), (true, true)]
            .into_iter()
            .filter(|&(l, r)| (!l || encodable(left)) && (!r || encodable(right)))
            .find(|&(l, r)| self.flanks_between(referenced(l, left), referenced(r, right)))
            .unwrap_or((encodable(left), encodable(right)))
    }
}

/// The open marks of one text scope.
#[derive(Default)]
pub(crate) struct Writer {
    open: Stack,
    /// Whether the last thing written is a delimiter run, which a delimiter character written
    /// next would join.
    pub(crate) delimited: bool,
}

impl Writer {
    /// Close what `next` does not share.
    pub(crate) fn close_to(&mut self, next: Stack, out: &mut Text<'_>) -> Result<(), ConvertError> {
        let keep = self.open.common(next);
        for mark in Stack::collect(self.open.iter().skip(keep)).iter().rev() {
            out.push_str(mark.close())?;
            self.delimited = mark.delimiter().is_some();
        }
        self.open = self.open.truncated(keep);
        Ok(())
    }

    /// Close what `marks` does not share, then open the rest of `marks`.
    pub(crate) fn open_to(&mut self, marks: Stack, out: &mut Text<'_>) -> Result<(), ConvertError> {
        self.close_to(marks, out)?;
        for mark in marks.iter().skip(self.open.len()) {
            out.push_str(mark.open())?;
        }
        self.open = marks;
        Ok(())
    }
}

/// Write `c` as a decimal numeric character reference; `&#0;` reads back as U+FFFD, the
/// replacement `CommonMark` 0.31.2 section 2.3 also makes of a raw U+0000.
pub(crate) fn push_reference(c: char, out: &mut Text<'_>) -> Result<(), ConvertError> {
    out.push_str(&format!("&#{};", u32::from(c)))
}
