//! The style rules of a CSS sheet that an on-screen reading system applies, read out of its group
//! rules in one pass over the text, without recursion.

use std::iter::Enumerate;
use std::slice;

const REPLACEMENT: char = char::REPLACEMENT_CHARACTER;
const REPLACEMENT_BYTES: &[u8] = "\u{fffd}".as_bytes();

/// Whether a condition holds on screen. Ordered so that `max` conjoins conditions and `min`
/// disjoins them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Applies {
    Always,
    Unsettled,
    Never,
}

impl Applies {
    fn inverted(self) -> Self {
        match self {
            Self::Always => Self::Never,
            Self::Unsettled => Self::Unsettled,
            Self::Never => Self::Always,
        }
    }
}

/// A style rule: its selectors, the text of each declaration it holds itself that names a value
/// (not those of rules nested in it), the condition its groups put on it, and whether a
/// replacement character sits in its selectors, any of its segments or the conditions of its
/// groups.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SheetRule<'a> {
    pub(crate) selectors: &'a str,
    pub(crate) declarations: Vec<&'a str>,
    pub(crate) applies: Applies,
    pub(crate) damaged: bool,
}

impl<'a> SheetRule<'a> {
    fn hold(&mut self, text: &'a str) {
        self.damaged |= text.contains(REPLACEMENT);
        if text.contains(':') {
            self.declarations.push(text);
        }
    }
}

/// Whether a media query list holds on screen; an empty list holds.
pub(crate) fn media_applies(list: &str) -> Applies {
    if list.trim().is_empty() {
        return Applies::Always;
    }
    list.split(',').map(query_applies).fold(Applies::Never, Applies::min)
}

fn query_applies(query: &str) -> Applies {
    let query = query.trim().to_ascii_lowercase();
    let (negated, rest) = strip_keyword(&query, "not");
    let (_, rest) = strip_keyword(rest.trim_start(), "only");
    let rest = rest.trim_start();
    let (kind, tail) =
        rest.split_at(rest.find(|c: char| c.is_whitespace() || c == '(').unwrap_or(rest.len()));
    let Some(featured) = features(kind, tail.trim_start()) else {
        return Applies::Never;
    };
    let applies = match kind {
        "all" | "screen" | "" if featured => Applies::Unsettled,
        "all" | "screen" => Applies::Always,
        _ => Applies::Never,
    };
    if negated { applies.inverted() } else { applies }
}

/// Whether the text after a query's type joins features to it; absent when something other than
/// `and` and a feature follows the type, which holds nowhere.
fn features(kind: &str, tail: &str) -> Option<bool> {
    if kind.is_empty() {
        return Some(tail.starts_with('('));
    }
    match strip_keyword(tail, "and") {
        _ if tail.is_empty() => Some(false),
        (true, rest) if rest.trim_start().starts_with('(') => Some(true),
        _ => None,
    }
}

fn strip_keyword<'a>(text: &'a str, keyword: &str) -> (bool, &'a str) {
    match text.strip_prefix(keyword).filter(|rest| rest.starts_with(char::is_whitespace)) {
        Some(rest) => (true, rest),
        None => (false, text),
    }
}

/// The rules of `css` that apply on a screen under `base`, in source order. A statement at-rule
/// ends at its `;`, and any other `;` outside a block joins the prelude after it; a block left
/// open closes at the end of the text; braces and semicolons escaped, in a quoted string or in an
/// unquoted `url(` move nothing.
pub(crate) fn scan(css: &str, base: Applies) -> impl Iterator<Item = SheetRule<'_>> {
    let root = if base == Applies::Never {
        Frame::Skip { nested: 0 }
    } else {
        Frame::Group { applies: base, damaged: false }
    };
    Rules {
        scanner: Scanner { css, open: vec![root], rule: None, start: 0 },
        bytes: css.as_bytes().iter().enumerate(),
        lex: Lex::PLAIN,
    }
}

struct Rules<'a> {
    scanner: Scanner<'a>,
    bytes: Enumerate<slice::Iter<'a, u8>>,
    lex: Lex,
}

impl<'a> Iterator for Rules<'a> {
    type Item = SheetRule<'a>;

    fn next(&mut self) -> Option<SheetRule<'a>> {
        let css = self.scanner.css.as_bytes();
        for (at, &byte) in self.bytes.by_ref() {
            let closed = match byte {
                _ if !self.lex.structural() => None,
                b'{' => self.scanner.open_block(at),
                b';' => self.scanner.end_statement(at),
                b'}' => self.scanner.close_block(at),
                _ => None,
            };
            self.lex = self.lex.next(css, at);
            if closed.is_some() {
                return closed;
            }
        }
        self.scanner.finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Lex {
    within: Within,
    escape: Escape,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Within {
    Plain,
    Quoted(u8),
    Url,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    None,
    Backslash,
    CarriageReturn,
}

impl Lex {
    const PLAIN: Self = Self { within: Within::Plain, escape: Escape::None };

    /// Whether the next byte can be a brace or semicolon of the sheet's structure. A backslash
    /// before a line break outside a string escapes nothing, so a carriage return it meets leaves
    /// the byte after it structural.
    fn structural(self) -> bool {
        self.within == Within::Plain && self.escape != Escape::Backslash
    }

    /// The state after `css[at]`. A backslash right after a replacement character is the trail
    /// byte of a legacy double-byte character, not an escape.
    fn next(self, css: &[u8], at: usize) -> Self {
        let byte = css[at];
        let within = self.within;
        let escape = match self.escape {
            Escape::Backslash if byte == b'\r' => Some(Escape::CarriageReturn),
            Escape::Backslash => Some(Escape::None),
            Escape::CarriageReturn if byte == b'\n' => Some(Escape::None),
            _ if byte == b'\\' && !css[..at].ends_with(REPLACEMENT_BYTES) => {
                Some(Escape::Backslash)
            }
            _ => None,
        };
        if let Some(escape) = escape {
            return Self { within, escape };
        }
        let within = match within {
            Within::Plain if matches!(byte, b'"' | b'\'') => Within::Quoted(byte),
            Within::Plain if byte == b'(' && opens_url(css, at) => Within::Url,
            Within::Quoted(quote) if byte == quote || matches!(byte, b'\n' | b'\r' | b'\x0c') => {
                Within::Plain
            }
            Within::Url if byte == b')' => Within::Plain,
            within => within,
        };
        Self { within, escape: Escape::None }
    }
}

/// Whether the `(` at `at` opens an unquoted `url(` token.
fn opens_url(css: &[u8], at: usize) -> bool {
    let name = at.saturating_sub(3);
    let named = css[name..at].eq_ignore_ascii_case(b"url")
        && !css[..name]
            .last()
            .is_some_and(|&b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    named && !css[at + 1..].trim_ascii_start().first().is_some_and(|b| matches!(b, b'"' | b'\''))
}

#[derive(Clone, Copy)]
enum Frame {
    Group { applies: Applies, damaged: bool },
    Style,
    Skip { nested: usize },
}

struct Scanner<'a> {
    css: &'a str,
    open: Vec<Frame>,
    rule: Option<SheetRule<'a>>,
    start: usize,
}

impl<'a> Scanner<'a> {
    fn text_to(&mut self, at: usize) -> &'a str {
        let text = self.css.get(self.start..at).unwrap_or_default();
        self.start = at + 1;
        text
    }

    fn open_block(&mut self, at: usize) -> Option<SheetRule<'a>> {
        let prelude = self.text_to(at).trim();
        let frame = match self.open.last_mut() {
            Some(Frame::Skip { nested }) => {
                *nested += 1;
                return None;
            }
            Some(&mut Frame::Group { applies, damaged }) => self.enter(prelude, applies, damaged),
            Some(Frame::Style) | None => Frame::Skip { nested: 0 },
        };
        self.open.push(frame);
        None
    }

    fn enter(&mut self, prelude: &'a str, applies: Applies, damaged: bool) -> Frame {
        let damaged = damaged || prelude.contains(REPLACEMENT);
        if !prelude.starts_with('@') {
            let declarations = Vec::new();
            self.rule = Some(SheetRule { selectors: prelude, declarations, applies, damaged });
            return Frame::Style;
        }
        match group_condition(prelude).map(|own| applies.max(own)) {
            Some(applies) if applies != Applies::Never => Frame::Group { applies, damaged },
            _ => Frame::Skip { nested: 0 },
        }
    }

    fn end_statement(&mut self, at: usize) -> Option<SheetRule<'a>> {
        let pending = self.css.get(self.start..at).unwrap_or_default();
        if self.open.len() < 2 && !pending.trim_start().starts_with('@') {
            return None;
        }
        let text = self.text_to(at);
        if let (Some(Frame::Style), Some(rule)) = (self.open.last(), self.rule.as_mut()) {
            rule.hold(text);
        }
        None
    }

    fn close_block(&mut self, at: usize) -> Option<SheetRule<'a>> {
        if let Some(Frame::Skip { nested }) = self.open.last_mut()
            && *nested > 0
        {
            *nested -= 1;
            return None;
        }
        if self.open.len() < 2 {
            return None;
        }
        let frame = self.open.pop()?;
        let last = self.text_to(at);
        if !matches!(frame, Frame::Style) {
            return None;
        }
        let mut rule = self.rule.take()?;
        rule.hold(last);
        Some(rule)
    }

    fn finish(&mut self) -> Option<SheetRule<'a>> {
        while self.open.len() > 1 {
            if let Some(rule) = self.close_block(self.css.len()) {
                return Some(rule);
            }
        }
        None
    }
}

/// The condition an at-rule block puts on the rules in it, absent when it holds none a screen
/// reads; a replacement character in its prelude leaves it unsettled.
fn group_condition(prelude: &str) -> Option<Applies> {
    let at_rule = prelude.strip_prefix('@')?;
    let name_end =
        at_rule.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).unwrap_or(at_rule.len());
    let (name, condition) = at_rule.split_at_checked(name_end)?;
    if prelude.contains(REPLACEMENT) {
        return Some(Applies::Unsettled);
    }
    match name.to_ascii_lowercase().as_str() {
        "media" => Some(media_applies(condition)),
        "supports" | "layer" | "container" | "scope" => Some(Applies::Unsettled),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::Applies::{Always, Never, Unsettled};
    use super::*;

    #[test]
    fn a_media_query_list_holds_on_screen_by_its_types_and_features() {
        let cases = [
            ("an empty list", "", Always),
            ("white space", "  ", Always),
            ("all", "all", Always),
            ("screen", "screen", Always),
            ("capitals", "SCREEN", Always),
            ("only", "only screen", Always),
            ("print", "print", Never),
            ("speech", "speech", Never),
            ("a type no screen is", "tv", Never),
            ("a word that starts with only", "onlyscreen", Never),
            ("a word that starts with not", "nothing", Never),
            ("not and no type", "not", Never),
            ("not print", "not print", Always),
            ("not screen", "not  screen", Never),
            ("not only screen", "not only screen", Never),
            ("a feature", "(min-width: 1px)", Unsettled),
            ("screen and a feature", "screen and (color)", Unsettled),
            ("all and a feature", "all and (color)", Unsettled),
            ("print and a feature", "print and (color)", Never),
            ("not a feature", "not (color)", Unsettled),
            ("not print and a feature", "not print and (color)", Always),
            ("not screen and a feature", "not screen and (color)", Unsettled),
            ("features joined", "(color) and (min-width: 1px)", Unsettled),
            ("print then screen", "print, screen", Always),
            ("screen then print", "screen, print", Always),
            ("print then a feature", "print, (color)", Unsettled),
            ("a feature then screen", "(color), screen", Always),
            ("not print after a comma", "(color), not print", Always),
            ("print and speech", "print, speech", Never),
            ("an empty query after print", "print,", Never),
            ("an empty query after screen", "screen,", Always),
            ("a dangling and", "screen and", Never),
            ("not before a malformed query", "not screen and", Never),
            ("not before a malformed print query", "not print junk", Never),
            ("a word after the type", "screen junk", Never),
            ("and with no feature", "screen and color", Never),
            ("a feature with no and", "screen (color)", Never),
            ("and joined to a feature", "screen and(color)", Never),
            ("a malformed query then screen", "screen and, screen", Always),
        ];
        for (name, list, expected) in cases {
            assert_eq!(media_applies(list), expected, "{name}: {list:?}");
        }
    }

    fn rules(css: &str, base: Applies) -> Vec<SheetRule<'_>> {
        scan(css, base).collect()
    }

    fn rule<'a>(
        selectors: &'a str,
        declarations: &[&'a str],
        applies: Applies,
        damaged: bool,
    ) -> SheetRule<'a> {
        SheetRule { selectors, declarations: declarations.to_vec(), applies, damaged }
    }

    #[test]
    fn a_rule_comes_out_of_its_groups_with_the_condition_they_put_on_it() {
        let css = "@charset \"x\"; .a, b.c { x: 1; ; y: 2 ; z } \
            @media screen { .d { z: 3 } @supports (a: b) { @media print { .e { z: 4 } } .f { z: 5 } } .g { z: 6 } } \
            @media print { .h { z: 7 } } @font-face { font-family: f } .i { z: 8 }";
        assert_eq!(
            rules(css, Always),
            [
                rule(".a, b.c", &[" x: 1", " y: 2 "], Always, false),
                rule(".d", &[" z: 3 "], Always, false),
                rule(".f", &[" z: 5 "], Unsettled, false),
                rule(".g", &[" z: 6 "], Always, false),
                rule(".i", &[" z: 8 "], Always, false),
            ]
        );
    }

    #[test]
    fn a_semicolon_outside_a_block_ends_only_an_at_rule() {
        let css = ".a { x: 1 }; .b { x: 2 } junk; .c { x: 3 } @charset \"c\"; .d { x: 4 }";
        assert_eq!(
            rules(css, Always),
            [
                rule(".a", &[" x: 1 "], Always, false),
                rule("; .b", &[" x: 2 "], Always, false),
                rule("junk; .c", &[" x: 3 "], Always, false),
                rule(".d", &[" x: 4 "], Always, false),
            ]
        );
    }

    #[test]
    fn the_condition_of_the_sheet_bounds_every_rule_in_it() {
        let css = ".a { x: 1 } @media screen { .b { x: 2 } } @media print { .c { x: 3 } }";
        assert_eq!(rules(css, Never), []);
        assert_eq!(
            rules(css, Unsettled),
            [rule(".a", &[" x: 1 "], Unsettled, false), rule(".b", &[" x: 2 "], Unsettled, false),]
        );
    }

    #[test]
    fn a_rule_holds_its_own_declarations_and_not_those_of_the_rules_nested_in_it() {
        assert_eq!(
            rules(
                ".a { x: 1; & b { y: 2; & c { y: 3 } } z: 4; @media print { w: 5 } v: 6 }",
                Always
            ),
            [rule(".a", &[" x: 1", " z: 4", " v: 6 "], Always, false)]
        );
    }

    #[test]
    fn a_replacement_character_marks_the_rules_whose_text_or_groups_hold_it() {
        let css = ".a\u{fffd} { x: 1 } .b { x: \u{fffd} } @media sc\u{fffd}een { .c { x: 1 } } \
            @media sc\u{fffd}een { @supports (a: b) { .g { x: 1 } } } @media print { .d\u{fffd} { x: 1 } } .e { x: 1 } @import \"\u{fffd}\"; \
            .f { & g\u{fffd} { x: 1 } } .h { x: 1; \u{fffd} }";
        assert_eq!(
            rules(css, Always),
            [
                rule(".a\u{fffd}", &[" x: 1 "], Always, true),
                rule(".b", &[" x: \u{fffd} "], Always, true),
                rule(".c", &[" x: 1 "], Unsettled, true),
                rule(".g", &[" x: 1 "], Unsettled, true),
                rule(".e", &[" x: 1 "], Always, false),
                rule(".f", &[], Always, false),
                rule(".h", &[" x: 1"], Always, true),
            ]
        );
    }
}
