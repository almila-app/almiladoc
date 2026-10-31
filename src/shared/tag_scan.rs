//! A scan of the tags in XML text that does not parse, outside comments, character data,
//! processing instructions and document type declarations.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::package::xml::to_utf8;

/// Whether `judges` accepts any tag of `bytes`, called with its name and attributes. The scan
/// charges the length of `bytes`.
pub(crate) fn any_tag(
    parse_ctx: &ParseCtx<'_>,
    bytes: &[u8],
    judges: impl Fn(&[u8], Attributes<'_>) -> bool,
) -> Result<bool, ConvertError> {
    scan(parse_ctx, bytes, judges, false)
}

pub(crate) fn any_tag_or_end(
    parse_ctx: &ParseCtx<'_>,
    bytes: &[u8],
    judges: impl Fn(&[u8], Attributes<'_>) -> bool,
) -> Result<bool, ConvertError> {
    scan(parse_ctx, bytes, judges, true)
}

fn scan(
    parse_ctx: &ParseCtx<'_>,
    bytes: &[u8],
    judges: impl Fn(&[u8], Attributes<'_>) -> bool,
    include_end: bool,
) -> Result<bool, ConvertError> {
    parse_ctx.charge(bytes.len() as u64)?;
    let text = to_utf8(bytes).0;
    let mut rest: &[u8] = &text;
    while let Some(open) = rest.iter().position(|&b| b == b'<') {
        rest = &rest[open + 1..];
        if let Some(after) = past_doctype(rest).or_else(|| past_unmarked(rest)) {
            rest = after;
            continue;
        }
        let (tag, after) = rest.split_at(tag_len(rest));
        let tag = if include_end { tag.strip_prefix(b"/").unwrap_or(tag) } else { tag };
        let name_end =
            tag.iter().position(|&b| b.is_ascii_whitespace() || b == b'/').unwrap_or(tag.len());
        let (name, attributes) = tag.split_at(name_end);
        if judges(name, Attributes { rest: attributes }) {
            return Ok(true);
        }
        rest = after;
    }
    Ok(false)
}

pub(crate) fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|&b| b == b':').next().unwrap_or(name)
}

/// The attributes of a tag as name and raw value, up to the first that does not read.
pub(crate) struct Attributes<'a> {
    rest: &'a [u8],
}

impl Attributes<'_> {
    /// Whether text other than the end of the tag remains after the last attribute read.
    pub(crate) fn stopped(&self) -> bool {
        !matches!(self.rest.trim_ascii(), b"" | b"/")
    }
}

impl<'a> Iterator for Attributes<'a> {
    type Item = (&'a [u8], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let eq = self.rest.iter().position(|&b| b == b'=')?;
        let (name, after_name) = self.rest.split_at(eq);
        let name = name.trim_ascii();
        if name.is_empty() || name.iter().any(u8::is_ascii_whitespace) {
            return None;
        }
        let (&quote @ (b'"' | b'\''), body) = after_name[1..].trim_ascii_start().split_first()?
        else {
            return None;
        };
        let close = body.iter().position(|&b| b == quote)?;
        let (value, after) = body.split_at(close);
        self.rest = &after[1..];
        Some((name, value))
    }
}

fn past_unmarked(rest: &[u8]) -> Option<&[u8]> {
    let (opener, closer): (&[u8], &[u8]) = if rest.starts_with(b"!--") {
        (b"!--", b"-->")
    } else if rest.starts_with(b"![CDATA[") {
        (b"![CDATA[", b"]]>")
    } else if rest.starts_with(b"?") {
        (b"?", b"?>")
    } else {
        return None;
    };
    let body = &rest[opener.len()..];
    let close = body.windows(closer.len()).position(|window| window == closer);
    Some(close.map_or(&[][..], |at| &body[at + closer.len()..]))
}

/// The text after a document type declaration, read as quick-xml reads one: its keyword in any
/// case, quoted literals to their closing quote, and an internal subset to its `]`, with the
/// comments and processing instructions in it skipped.
fn past_doctype(rest: &[u8]) -> Option<&[u8]> {
    const KEYWORD: &[u8] = b"!DOCTYPE";
    let mut rest = rest
        .get(KEYWORD.len()..)
        .filter(|_| rest[..KEYWORD.len()].eq_ignore_ascii_case(KEYWORD))?;
    let mut subset = false;
    while let Some(at) =
        rest.iter().position(|&b| matches!(b, b'"' | b'\'' | b'[' | b']' | b'<' | b'>'))
    {
        let (mark, after) = (rest[at], &rest[at + 1..]);
        rest = match mark {
            b'"' | b'\'' => {
                after.iter().position(|&b| b == mark).map_or(&[][..], |close| &after[close + 1..])
            }
            b'>' if !subset => return Some(after),
            b'<' if subset => past_unmarked(after).unwrap_or(after),
            b'[' => {
                subset = true;
                after
            }
            b']' => {
                subset = false;
                after
            }
            _ => after,
        };
    }
    Some(&[])
}

/// The tag's length: a `>` inside a quoted value does not end it, and a `<` always does, since
/// a value holds none.
fn tag_len(rest: &[u8]) -> usize {
    let mut quote = None;
    for (at, &b) in rest.iter().enumerate() {
        match (quote, b) {
            (_, b'<') | (None, b'>') => return at,
            (None, b'"' | b'\'') => quote = Some(b),
            (Some(open), _) if b == open => quote = None,
            _ => {}
        }
    }
    rest.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{Limit, Limits};
    use std::assert_matches;

    type Tag = (String, Vec<(String, String)>, bool);

    fn tags(text: &str) -> Vec<Tag> {
        let seen = std::cell::RefCell::new(Vec::new());
        let text = text.as_bytes();
        let none = any_tag(&ParseCtx::default(), text, |name, mut attributes| {
            let read: Vec<_> = attributes
                .by_ref()
                .map(|(n, v)| {
                    (
                        String::from_utf8_lossy(n).into_owned(),
                        String::from_utf8_lossy(v).into_owned(),
                    )
                })
                .collect();
            let name = String::from_utf8_lossy(name).into_owned();
            seen.borrow_mut().push((name, read, attributes.stopped()));
            false
        });
        assert!(!none.unwrap());
        seen.into_inner()
    }

    fn row(name: &str, attributes: &[(&str, &str)], stopped: bool) -> Tag {
        let attributes = attributes.iter().map(|&(n, v)| (n.into(), v.into())).collect();
        (name.into(), attributes, stopped)
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of tags and what the scan reads of each")]
    fn tags_are_read_with_their_attributes() {
        let cases = [
            (
                "quotes of either kind and spaces around the equals sign",
                r#"<a:b x = "1" y='2' z="3"/>"#,
                vec![row("a:b", &[("x", "1"), ("y", "2"), ("z", "3")], false)],
            ),
            (
                "a closing tag, which names nothing, and an empty element",
                "<a></a><b/>",
                vec![row("a", &[], false), row("", &[], true), row("b", &[], false)],
            ),
            (
                "a greater-than sign in a quoted value",
                r#"<a x=">" y="2"><b/>"#,
                vec![row("a", &[("x", ">"), ("y", "2")], false), row("b", &[], false)],
            ),
            (
                "a less-than sign ends the tag whatever quote is open",
                r#"<a x="1><b y="2"/>"#,
                vec![row("a", &[], true), row("b", &[("y", "2")], false)],
            ),
            ("a value that is never closed", "<a x='1", vec![row("a", &[], true)]),
            ("an attribute with no value", r#"<a x y="2"/>"#, vec![row("a", &[], true)]),
            ("an unquoted value", "<a x=1/>", vec![row("a", &[], true)]),
            (
                "an attribute read before one that does not",
                r#"<a x="1" y/>"#,
                vec![row("a", &[("x", "1")], true)],
            ),
            (
                "comments and character data hold no tag",
                "<!-- <a/> --><![CDATA[<b/>]]><c/><!-- <d/>",
                vec![row("c", &[], false)],
            ),
            ("text between tags", "t<a/>u>v<b/>", vec![row("a", &[], false), row("b", &[], false)]),
            (
                "processing instructions hold no tag, whatever markup they hold",
                "<?xml version='1.0'?><?pi <!--?><a/><?pi <b x='1'/> > ?><c/><?pi <d/>",
                vec![row("a", &[], false), row("c", &[], false)],
            ),
            ("an attribute with no name", r#"<a ="1" y="2"/>"#, vec![row("a", &[], true)]),
            (
                "a document type declaration whose literal opens a comment",
                r#"<!DOCTYPE r SYSTEM "<!--"><a/>"#,
                vec![row("a", &[], false)],
            ),
            (
                "a lower-case document type declaration whose literal in the other quote opens a \
                 processing instruction",
                "<!doctype r PUBLIC '<?' '<b/>'><a/>",
                vec![row("a", &[], false)],
            ),
            (
                "an internal subset holding declarations, a comment and a processing instruction",
                r#"<!DOCTYPE r [<!ELEMENT r ANY><!ATTLIST r x CDATA "]>"><!-- ' ]> --><?pi ]>?>]><a/>"#,
                vec![row("a", &[], false)],
            ),
            ("a literal that is never closed", r#"<!DOCTYPE r SYSTEM "><a/>"#, vec![]),
            (
                "a less-than sign outside an internal subset",
                "<!DOCTYPE r <!--><a/>",
                vec![row("a", &[], false)],
            ),
            (
                "a document type declaration that is never closed",
                r#"<!DOCTYPE r [<a/><!ATTLIST r x CDATA "]"#,
                vec![],
            ),
            (
                "white space before the end of a tag",
                r#"<a x="1" /><b >"#,
                vec![row("a", &[("x", "1")], false), row("b", &[], false)],
            ),
        ];
        for (case, text, expected) in cases {
            assert_eq!(tags(text), expected, "{case}");
        }
    }

    #[test]
    fn local_names_drop_every_prefix() {
        for (name, local) in
            [("w:vanish", "vanish"), ("vanish", "vanish"), ("a:b:c", "c"), ("", "")]
        {
            assert_eq!(local_name(name.as_bytes()), local.as_bytes(), "{name}");
        }
    }

    #[test]
    fn a_text_in_another_encoding_is_scanned_as_decoded() {
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend("<a x=\"1\"/>".encode_utf16().flat_map(u16::to_le_bytes));
        let found = any_tag(&ParseCtx::default(), &utf16, |name, mut attributes| {
            name == b"a" && attributes.next() == Some((b"x", b"1"))
        });
        assert!(found.unwrap());
    }

    #[test]
    fn the_scan_charges_the_length_of_the_text() {
        let limits = Limits { max_work: 100, ..Limits::default() };
        let scan = |filler: usize| {
            any_tag(&ParseCtx::new(&limits), "<a/>".repeat(filler).as_bytes(), |_, _| false)
        };
        assert!(!scan(10).unwrap());
        assert_matches!(
            scan(100),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 100 })
        );
    }
}
