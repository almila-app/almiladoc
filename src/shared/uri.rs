//! URI classification shared across frontends (EPUB hrefs, ODF hrefs and
//! image references, Word field targets).

use std::borrow::Cow;

/// True when `s` is an RFC 3986 scheme (`scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`).
pub(crate) fn is_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn has_scheme(s: &str) -> bool {
    s.split_once(':').is_some_and(|(scheme, _)| is_scheme(scheme))
}

/// True for Windows drive-letter paths (`C:\...`, `C:/...`). They parse as
/// a one-letter scheme under RFC 3986, but in documents they are local file
/// paths (legacy Word HYPERLINK fields), never URIs.
fn is_drive_path(s: &str) -> bool {
    matches!(s.as_bytes(), [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic())
}

/// True when `s` should be treated as an absolute URI with a scheme rather
/// than a relative/package reference.
pub(crate) fn is_absolute_uri(s: &str) -> bool {
    has_scheme(s) && !is_drive_path(s)
}

/// Whether a link destination may reach the Markdown: judged on the destination a browser would
/// follow after WHATWG URL preprocessing (leading and trailing C0 controls and spaces stripped,
/// tab, CR and LF removed), its scheme ASCII-lowercased. A scheme must be `http`, `https` or
/// `mailto`, which also refuses a drive path (`C:`); a destination without one passes unless it
/// is a UNC path.
pub(crate) fn is_safe_link(dest: &str) -> bool {
    const INNER_STRIPPED: [char; 3] = ['\t', '\r', '\n'];
    let trimmed = dest.trim_matches(|c: char| c <= ' ');
    let url = if trimmed.contains(INNER_STRIPPED) {
        Cow::Owned(trimmed.replace(INNER_STRIPPED, ""))
    } else {
        Cow::Borrowed(trimmed)
    };
    if url.starts_with("\\\\") {
        return false;
    }
    match url.split_once(':') {
        Some((scheme, _)) if is_scheme(scheme) => {
            ["http", "https", "mailto"].iter().any(|allowed| scheme.eq_ignore_ascii_case(allowed))
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_and_mail_schemes_and_plain_paths_are_safe() {
        for refused in [
            "javascript:alert(1)",
            " javascript:x",
            "\u{1}javascript:x",
            "JavaScript:x",
            "java\tscript:x",
            "java\nscript:x",
            "jav\rascript:x",
            "data:text/html,x",
            "vbscript:x",
            "file:///etc/passwd",
            "FILE:x",
            r"C:\x",
            "c:/x",
            r"\\srv\x",
            " \\\\srv",
            "ms-word:ofe|u|x",
        ] {
            assert!(!is_safe_link(refused), "{refused:?}");
        }
        for kept in [
            "https://e.com",
            "HTTP://e.com",
            " https://e.com ",
            "mailto:a@b.c",
            "notes.txt",
            "../a.odt",
            "#top",
            "//e.com/x",
            "a/b:c",
            "",
        ] {
            assert!(is_safe_link(kept), "{kept:?}");
        }
    }

    #[test]
    fn schemes_follow_rfc_3986() {
        assert!(has_scheme("https://e.com"));
        assert!(has_scheme("mailto:x@y"));
        assert!(has_scheme("a:relative-scheme-uri"), "one-character schemes are valid");
        assert!(has_scheme("view-source:x"));
        assert!(!has_scheme("1http:x"), "schemes must start with a letter");
        assert!(!has_scheme("no scheme here"));
        assert!(!has_scheme(":empty"));
        assert!(!has_scheme("path/with:colon"), "slash before the colon is not a scheme");
    }

    #[test]
    fn drive_paths_are_not_uris() {
        assert!(is_drive_path(r"C:\docs\a.doc"));
        assert!(is_drive_path("c:/docs/a.doc"));
        assert!(!is_drive_path("cs:whatever"));
        assert!(!is_drive_path("c:x"));
        assert!(!is_drive_path("1:/x"));
        assert!(!is_absolute_uri(r"C:\docs\a.doc"));
        assert!(is_absolute_uri("https://e.com"));
        assert!(is_absolute_uri("c:x"));
    }
}
