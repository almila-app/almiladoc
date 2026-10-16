//! Word field-instruction parsing (DOC, DOCX, RTF), via a small
//! case-insensitive lexer with known switch arities.

use crate::model::{Inline, LinkTarget, inlines_are_empty};
use std::ops::Range;

/// Field accumulator: instruction text before the separator, result after.
#[derive(Default)]
pub struct FieldFrame {
    pub instr: String,
    pub in_result: bool,
    pub inlines: Vec<Inline>,
}

/// Finish a field: wrap the result in a link when the instruction is a
/// hyperlink, otherwise pass the result content through.
pub fn field_result(instr: &str, content: Vec<Inline>) -> Vec<Inline> {
    match hyperlink_target(instr) {
        Some(target) if !inlines_are_empty(&content) => {
            vec![Inline::Link { content, target }]
        }
        _ => content,
    }
}

#[derive(Debug, PartialEq)]
enum Token {
    Word(String, Range<usize>),
    Switch(char),
}

/// Tokenize a field instruction: quoted strings (with `\"` and `\\`
/// escapes), backslash switches, bare words. A word carries the byte span
/// of its raw text in `instr`, inside any quotes and trimmed of whitespace.
fn tokenize(instr: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = instr.char_indices().peekable();
    while let Some(&(at, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            let start = at + 1;
            let mut end = instr.len();
            let mut word = String::new();
            while let Some((i, c)) = chars.next() {
                match c {
                    '"' => {
                        end = i;
                        break;
                    }
                    '\\' => match chars.next() {
                        Some((_, esc @ ('"' | '\\'))) => word.push(esc),
                        Some((_, other)) => {
                            word.push('\\');
                            word.push(other);
                        }
                        None => break,
                    },
                    c => word.push(c),
                }
            }
            out.push(Token::Word(word, trimmed_span(instr, start..end)));
        } else if c == '\\' {
            chars.next();
            if let Some(&(_, switch)) = chars.peek() {
                chars.next();
                out.push(Token::Switch(switch.to_ascii_lowercase()));
            }
        } else {
            let mut end = instr.len();
            while let Some(&(i, c)) = chars.peek() {
                if c.is_whitespace() {
                    end = i;
                    break;
                }
                chars.next();
            }
            out.push(Token::Word(instr[at..end].to_string(), at..end));
        }
    }
    out
}

/// `span` of `instr` without its leading and trailing whitespace.
fn trimmed_span(instr: &str, span: Range<usize>) -> Range<usize> {
    let raw = &instr[span.clone()];
    let start = span.start + (raw.len() - raw.trim_start().len());
    start..start + raw.trim().len()
}

/// Switches of the HYPERLINK field that take an argument.
fn hyperlink_switch_takes_arg(switch: char) -> bool {
    matches!(switch, 'l' | 'o' | 't')
}

/// Byte spans in a HYPERLINK instruction of the text its target is made of.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct TargetSpans {
    /// The URL token.
    pub url: Option<Range<usize>>,
    /// The `\l` argument.
    pub anchor: Option<Range<usize>>,
}

impl TargetSpans {
    /// The spans that are present.
    pub fn iter(&self) -> impl Iterator<Item = &Range<usize>> {
        self.url.iter().chain(self.anchor.iter())
    }
}

/// Interpret a HYPERLINK field instruction as a link target.
pub fn hyperlink_target(instr: &str) -> Option<LinkTarget> {
    hyperlink_target_spans(instr).map(|(target, _)| target)
}

/// Interpret a HYPERLINK field instruction as a link target, with the byte
/// spans in `instr` of the text the target is made of.
pub(crate) fn hyperlink_target_spans(instr: &str) -> Option<(LinkTarget, TargetSpans)> {
    let mut tokens = tokenize(instr).into_iter().peekable();
    match tokens.next() {
        Some(Token::Word(w, _)) if w.eq_ignore_ascii_case("HYPERLINK") => {}
        _ => return None,
    }
    let mut url: Option<String> = None;
    let mut anchor: Option<String> = None;
    let mut spans = TargetSpans::default();
    while let Some(token) = tokens.next() {
        match token {
            Token::Word(w, span) => {
                if url.is_none() && !w.trim().is_empty() {
                    url = Some(w.trim().to_string());
                    spans.url = Some(span);
                }
            }
            Token::Switch(s) => {
                // A switch's argument is the next token only when it is a
                // word; a following switch means the argument was omitted.
                let arg = if hyperlink_switch_takes_arg(s)
                    && matches!(tokens.peek(), Some(Token::Word(..)))
                {
                    match tokens.next() {
                        Some(Token::Word(w, span)) => Some((w, span)),
                        _ => None,
                    }
                } else {
                    None
                };
                if s == 'l'
                    && let Some((a, span)) = arg
                    && !a.trim().is_empty()
                {
                    anchor = Some(a.trim().to_string());
                    spans.anchor = Some(span);
                }
            }
        }
    }
    let target = match (url, anchor) {
        (Some(url), Some(frag)) => classify(format!("{url}#{frag}")),
        (Some(url), None) => classify(url),
        (None, Some(frag)) => LinkTarget::Anchor(frag),
        (None, None) => return None,
    };
    Some((target, spans))
}

/// Classify an OPC relationship target as a link target. External-mode
/// targets are parsed like hyperlink field targets (scheme detection);
/// internal-mode targets stay package-relative.
pub fn classify_rel_target(external: bool, target: &str) -> LinkTarget {
    if external { classify(target.to_string()) } else { LinkTarget::Relative(target.to_string()) }
}

fn classify(url: String) -> LinkTarget {
    if let Some(anchor) = url.strip_prefix('#') {
        return LinkTarget::Anchor(anchor.to_string());
    }
    if crate::shared::uri::is_absolute_uri(&url) {
        LinkTarget::External(url)
    } else {
        LinkTarget::Relative(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_url() {
        assert_eq!(
            hyperlink_target(r#" HYPERLINK "https://e.com/a b" "#),
            Some(LinkTarget::External("https://e.com/a b".into()))
        );
    }

    #[test]
    fn case_insensitive_keyword() {
        assert_eq!(
            hyperlink_target(r#"hyperlink "https://e.com""#),
            Some(LinkTarget::External("https://e.com".into()))
        );
    }

    #[test]
    fn anchor_only() {
        assert_eq!(
            hyperlink_target(r#"HYPERLINK \l "sec2""#),
            Some(LinkTarget::Anchor("sec2".into()))
        );
    }

    #[test]
    fn url_plus_anchor() {
        assert_eq!(
            hyperlink_target(r#"HYPERLINK "https://e.com/p" \l "frag""#),
            Some(LinkTarget::External("https://e.com/p#frag".into()))
        );
    }

    #[test]
    fn switch_with_argument_not_mistaken_for_url() {
        assert_eq!(
            hyperlink_target(r#"HYPERLINK \o "tooltip text" "https://e.com""#),
            Some(LinkTarget::External("https://e.com".into()))
        );
    }

    #[test]
    fn escaped_quotes_in_target() {
        assert_eq!(
            hyperlink_target(r#"HYPERLINK "https://e.com/\"q\"""#),
            Some(LinkTarget::External(r#"https://e.com/"q""#.into()))
        );
    }

    #[test]
    fn relative_target() {
        assert_eq!(
            hyperlink_target(r#"HYPERLINK "docs/readme.docx""#),
            Some(LinkTarget::Relative("docs/readme.docx".into()))
        );
    }

    #[test]
    fn argless_switch_does_not_swallow_following_switch() {
        assert_eq!(
            hyperlink_target(r#"HYPERLINK "https://e.com/p" \o \l "frag""#),
            Some(LinkTarget::External("https://e.com/p#frag".into()))
        );
    }

    #[test]
    fn spans_cover_the_url_and_anchor_text_inside_quotes() {
        let instr = r#"HYPERLINK " http://e.com/ " \o "tip" \l "frag""#;
        let (_, spans) = hyperlink_target_spans(instr).unwrap();
        assert_eq!(&instr[spans.url.unwrap()], "http://e.com/");
        assert_eq!(&instr[spans.anchor.unwrap()], "frag");
    }

    #[test]
    fn spans_of_bare_words() {
        let instr = "HYPERLINK  http://e.com/p \\l a";
        let (_, spans) = hyperlink_target_spans(instr).unwrap();
        assert_eq!(spans.url, Some(11..25));
        assert_eq!(spans.anchor, Some(29..30));
    }

    #[test]
    fn span_of_an_escaped_url_keeps_its_raw_text() {
        let instr = r#"HYPERLINK "a\"b""#;
        let (target, spans) = hyperlink_target_spans(instr).unwrap();
        assert_eq!(target, LinkTarget::Relative(r#"a"b"#.into()));
        assert_eq!(&instr[spans.url.unwrap()], r#"a\"b"#);
    }

    #[test]
    fn span_of_an_unclosed_quote_runs_to_the_end() {
        let instr = "HYPERLINK \"http://e.com/ ";
        let (_, spans) = hyperlink_target_spans(instr).unwrap();
        assert_eq!(spans.url, Some(11..24));
        assert_eq!(spans.anchor, None);
    }

    #[test]
    fn spans_of_an_anchor_only_link() {
        let instr = r#"HYPERLINK \l "sec2""#;
        let (_, spans) = hyperlink_target_spans(instr).unwrap();
        assert_eq!(spans, TargetSpans { url: None, anchor: Some(14..18) });
    }

    #[test]
    fn non_hyperlink_fields_ignored() {
        assert_eq!(hyperlink_target("PAGEREF _Toc123 \\h"), None);
    }
}
