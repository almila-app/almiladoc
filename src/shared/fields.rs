use crate::model::{Inline, LinkTarget, inlines_are_empty};
use std::ops::Range;

#[derive(Default)]
pub(crate) struct FieldFrame {
    pub instr: String,
    pub unresolved: Vec<Range<usize>>,
    pub in_result: bool,
    pub inlines: Vec<Inline>,
    pub token: Option<&'static str>,
    pub locked: bool,
}

impl FieldFrame {
    pub(crate) fn side_token(&self) -> Option<&'static str> {
        side_field_token(&self.instr, self.locked)
    }
}

pub(crate) fn field_result(instr: &str, content: Vec<Inline>) -> Vec<Inline> {
    match hyperlink_target(instr) {
        Some(target) if !inlines_are_empty(&content) => {
            vec![Inline::Link { content, target }]
        }
        _ => content,
    }
}

pub(crate) fn side_field_token(instr: &str, locked: bool) -> Option<&'static str> {
    if locked {
        return None;
    }
    let tokens = tokenize(instr);
    let Some(Token::Word(keyword, _)) = tokens.first() else { return None };
    let path = tokens.contains(&Token::Switch('p'));
    Some(match keyword.to_ascii_uppercase().as_str() {
        "PAGE" => "{page}",
        "NUMPAGES" | "SECTIONPAGES" => "{pages}",
        "DATE" => "{date}",
        "TIME" => "{time}",
        "FILENAME" if path => "{path}",
        "FILENAME" => "{file}",
        _ => return None,
    })
}

pub(crate) fn is_choice_form_field(instr: &str) -> bool {
    let keyword = instr.split_whitespace().next().unwrap_or_default();
    keyword.eq_ignore_ascii_case("FORMCHECKBOX") || keyword.eq_ignore_ascii_case("FORMDROPDOWN")
}

#[derive(Debug, PartialEq)]
enum Token {
    Word(String, Range<usize>),
    Switch(char),
}

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
            while chars.next_if(|&(_, c)| !c.is_whitespace()).is_some() {}
            let end = chars.peek().map_or(instr.len(), |&(i, _)| i);
            out.push(Token::Word(instr.get(at..end).unwrap_or_default().to_owned(), at..end));
        }
    }
    out
}

fn trimmed_span(instr: &str, span: Range<usize>) -> Range<usize> {
    let raw = instr.get(span.clone()).unwrap_or_default();
    let start = span.start + (raw.len() - raw.trim_start().len());
    start..start + raw.trim().len()
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct TargetSpans {
    pub url: Option<Range<usize>>,
    pub anchor: Option<Range<usize>>,
}

impl TargetSpans {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Range<usize>> {
        self.url.iter().chain(self.anchor.iter())
    }
}

pub(crate) fn hyperlink_target(instr: &str) -> Option<LinkTarget> {
    hyperlink_target_spans(instr).map(|(target, _)| target)
}

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
                let arg = tokens
                    .next_if(|t| matches!(s, 'l' | 'o' | 't') && matches!(t, Token::Word(..)));
                if s == 'l'
                    && let Some(Token::Word(a, span)) = arg
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
        (None, Some(frag)) => LinkTarget::anchor(frag),
        (None, None) => return None,
    };
    Some((target, spans))
}

pub(crate) fn classify_rel_target(external: bool, target: &str) -> LinkTarget {
    if external { classify(target.to_string()) } else { LinkTarget::Relative(target.to_string()) }
}

fn classify(url: String) -> LinkTarget {
    if let Some(anchor) = url.strip_prefix('#') {
        return LinkTarget::anchor(anchor.to_string());
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
    fn only_a_page_field_s_first_keyword_names_its_side_token() {
        for (instr, expected) in [
            (" PAGE \\* MERGEFORMAT ", Some("{page}")),
            ("page", Some("{page}")),
            ("NUMPAGES", Some("{pages}")),
            ("SectionPages", Some("{pages}")),
            ("DATE \\@ \"d MMMM yyyy\"", Some("{date}")),
            ("TIME", Some("{time}")),
            ("FILENAME", Some("{file}")),
            ("FILENAME \\p", Some("{path}")),
            ("FILENAME \\P \\* Upper", Some("{path}")),
            ("PAGEREF _Toc1", None),
            ("SAVEDATE", None),
            ("REF PAGE", None),
            ("", None),
        ] {
            assert_eq!(side_field_token(instr, false), expected, "{instr:?}");
            assert_eq!(side_field_token(instr, true), None, "{instr:?}");
        }
    }

    #[test]
    fn only_the_first_keyword_names_a_choice_form_field() {
        for (instr, expected) in [
            (" FORMCHECKBOX ", true),
            ("formcheckbox", true),
            ("FormDropDown \\* MERGEFORMAT", true),
            ("\tFORMDROPDOWN", true),
            ("FORMTEXT", false),
            ("MERGEFIELD FORMCHECKBOX", false),
            ("FORMCHECKBOXES", false),
            ("", false),
            ("   ", false),
        ] {
            assert_eq!(is_choice_form_field(instr), expected, "{instr:?}");
        }
    }

    #[test]
    fn a_hyperlink_instruction_reads_its_url_and_anchor_through_quotes_and_switches() {
        for (instr, expected) in [
            (
                r#" HYPERLINK "https://e.com/a b" "#,
                Some(LinkTarget::External("https://e.com/a b".into())),
            ),
            (r#"hyperlink "https://e.com""#, Some(LinkTarget::External("https://e.com".into()))),
            (r#"HYPERLINK \l "sec2""#, Some(LinkTarget::anchor("sec2".into()))),
            (
                r#"HYPERLINK "https://e.com/p" \l "frag""#,
                Some(LinkTarget::External("https://e.com/p#frag".into())),
            ),
            (
                r#"HYPERLINK \o "tooltip text" "https://e.com""#,
                Some(LinkTarget::External("https://e.com".into())),
            ),
            (
                r#"HYPERLINK \t "_blank" "https://e.com""#,
                Some(LinkTarget::External("https://e.com".into())),
            ),
            (
                r#"HYPERLINK "https://e.com/\"q\"""#,
                Some(LinkTarget::External(r#"https://e.com/"q""#.into())),
            ),
            (
                r#"HYPERLINK "docs/readme.docx""#,
                Some(LinkTarget::Relative("docs/readme.docx".into())),
            ),
            (
                r#"HYPERLINK "https://e.com/p" \o \l "frag""#,
                Some(LinkTarget::External("https://e.com/p#frag".into())),
            ),
            ("PAGEREF _Toc123 \\h", None),
        ] {
            assert_eq!(hyperlink_target(instr), expected, "{instr}");
        }
    }

    #[test]
    fn spans_cover_the_url_and_anchor_text_inside_quotes() {
        let instr = r#"HYPERLINK " http://e.com/ " \o "tip" \l "frag""#;
        let (_, spans) = hyperlink_target_spans(instr).unwrap();
        assert_eq!(instr.get(spans.url.unwrap()), Some("http://e.com/"));
        assert_eq!(instr.get(spans.anchor.unwrap()), Some("frag"));
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
        assert_eq!(instr.get(spans.url.unwrap()), Some(r#"a\"b"#));
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
}
