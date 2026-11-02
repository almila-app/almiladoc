// ZWNJ/ZWJ preserve Arabic/Indic shaping and emoji sequences.
pub(crate) fn clean_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{a0}' | '\n' => out.push(' '),
            '\t' => out.push('\t'),
            '\r' => {
                chars.next_if_eq(&'\n');
                out.push(' ');
            }
            c if c.is_control() || is_layout_invisible(c) => {}
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn is_layout_invisible(c: char) -> bool {
    matches!(c, '\u{ad}' | '\u{200b}' | '\u{feff}')
}

pub(crate) fn collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if !c.is_whitespace() {
            out.push(c);
        } else if !out.ends_with(' ') {
            out.push(' ');
        }
    }
    out
}

/// Trims TeX but keeps a trailing space a backslash escapes, which TeX reads as a control space.
pub(crate) fn trim_tex(tex: &str) -> &str {
    let tex = tex.trim_start();
    let kept = tex.trim_end();
    let escapes = kept.bytes().rev().take_while(|&byte| byte == b'\\').count();
    match tex.get(kept.len()..).and_then(|rest| rest.chars().next()) {
        Some(space) if escapes % 2 == 1 => tex.get(..kept.len() + space.len_utf8()).unwrap_or(kept),
        _ => kept,
    }
}

#[cfg(test)]
mod tests {
    use super::{clean_text, collapse_ws, trim_tex};

    #[test]
    fn tex_keeps_a_trailing_space_only_a_backslash_escapes() {
        for (tex, trimmed) in [
            (" a \\  ", "a \\ "),
            ("a\\\t\n", "a\\\t"),
            ("a\\\\ ", "a\\\\"),
            ("\\ ", "\\ "),
            (" a ", "a"),
        ] {
            assert_eq!(trim_tex(tex), trimmed, "{tex:?}");
        }
    }

    #[test]
    fn join_controls_preserved() {
        assert_eq!(clean_text("می\u{200c}خواهم"), "می\u{200c}خواهم");
        assert_eq!(clean_text("👨\u{200d}👩\u{200d}👧"), "👨\u{200d}👩\u{200d}👧");
    }

    #[test]
    fn layout_invisibles_stripped() {
        assert_eq!(clean_text("a\u{ad}b\u{200b}c\u{feff}d\u{a0}e"), "abcd e");
    }

    #[test]
    fn line_breaks_become_spaces_and_a_crlf_pair_is_one_break() {
        for (label, text, expected) in [
            ("lf", "a\nb", "a b"),
            ("cr", "a\rb", "a b"),
            ("crlf", "a\r\nb", "a b"),
            ("lf then cr", "a\n\rb", "a  b"),
            ("two crlf pairs", "a\r\n\r\nb", "a  b"),
            ("cr at the end", "a\r", "a "),
            ("cr then a control", "a\r\u{1}b", "a b"),
            ("tab kept, controls dropped", "a\t\u{7}b", "a\tb"),
        ] {
            assert_eq!(clean_text(text), expected, "{label}");
        }
    }

    #[test]
    fn whitespace_runs_of_any_kind_collapse_to_one_space() {
        for (label, text, expected) in [
            ("empty", "", ""),
            ("no whitespace", "abc", "abc"),
            ("leading", "  a", " a"),
            ("trailing", "a \t", "a "),
            ("only whitespace", " \n\t ", " "),
            ("mixed ASCII", "a \t\r\n b", "a b"),
            ("ideographic space and line separator", "a\u{3000}\u{2028}b", "a b"),
            ("no-break space and em space", "\u{a0}a\u{2003}\u{a0}b", " a b"),
            ("a space after a run", "a\u{3000} b", "a b"),
            (
                "zero-width characters are not whitespace",
                "a\u{200b}\u{feff}b",
                "a\u{200b}\u{feff}b",
            ),
            ("two runs", "a  b   c", "a b c"),
        ] {
            assert_eq!(collapse_ws(text), expected, "{label}");
        }
    }
}
