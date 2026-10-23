//! Package target resolution per OPC/EPUB URI semantics, applied before any
//! archive lookup.
//!
//! The reference is treated as a URI: fragment and query split off first,
//! then each path segment is percent-decoded *after* segmentation. A segment
//! whose decoded form would introduce structure (`/`, `\`, `.`, `..`) is
//! rejected — encoded traversal never becomes path structure.

use crate::error::ConvertError;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Target {
    /// Normalized archive path (no leading slash).
    pub(crate) path: String,
    pub(crate) fragment: Option<String>,
}

/// Resolve a relative or package-absolute reference against the part it
/// appears in.
pub(crate) fn resolve(base_part: &str, reference: &str) -> Result<Target, ConvertError> {
    let (reference, fragment) = reference
        .split_once('#')
        .map_or((reference, None), |(r, f)| (r, Some(decode_component(f))));
    let reference = reference.split_once('?').map_or(reference, |(r, _)| r);
    if reference.is_empty() {
        return Ok(Target { path: base_part.to_string(), fragment });
    }

    let mut segments: Vec<String> = Vec::new();
    if !reference.starts_with('/')
        && let Some((dir, _)) = base_part.rsplit_once('/')
    {
        segments.extend(dir.split('/').filter(|s| !s.is_empty()).map(str::to_string));
    }
    for raw in reference.split('/') {
        match raw {
            "" | "." => {}
            ".." => {
                // Producers climb above the package root; clamp there.
                segments.pop();
            }
            raw => {
                let decoded = decode_component(raw);
                if decoded.contains(['/', '\\']) {
                    return Err(ConvertError::malformed(
                        "percent-encoded separator in a package reference",
                    ));
                }
                if matches!(decoded.as_str(), "." | "..") {
                    return Err(ConvertError::malformed(
                        "percent-encoded traversal in a package reference",
                    ));
                }
                segments.push(decoded);
            }
        }
    }
    Ok(Target { path: segments.join("/"), fragment })
}

/// Percent-decode one URI component. Infallible by design: a `%` not
/// followed by two hex digits passes through literally (producers emit such
/// names), and non-UTF-8 decoded bytes degrade lossily - the result is only
/// ever matched against archive entry names, where a near-miss simply fails
/// the lookup.
fn decode_component(component: &str) -> String {
    let bytes = component.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = component.get(i..).and_then(|rest| rest.get(1..3))
            // from_str_radix alone also accepts `+5`; only true hex digit
            // pairs decode.
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn decode_fragment(fragment: &str) -> String {
    decode_component(fragment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_resolve_against_the_base_part_and_split_off_query_and_fragment() {
        let cases = [
            (
                "nested sibling",
                "word/document.xml",
                "media/image1.png",
                "word/media/image1.png",
                None,
            ),
            ("sibling", "word/document.xml", "styles.xml", "word/styles.xml", None),
            ("root sibling", "content.opf", "ch1.xhtml", "ch1.xhtml", None),
            (
                "package-absolute",
                "word/document.xml",
                "/docProps/core.xml",
                "docProps/core.xml",
                None,
            ),
            ("parent", "OEBPS/text/ch1.xhtml", "../images/i.png", "OEBPS/images/i.png", None),
            ("current directory", "a/b.xml", "./c.xml", "a/c.xml", None),
            ("clamped at the root", "a/b.xml", "../../../x.xml", "x.xml", None),
            ("fragment", "OEBPS/ch1.xhtml", "ch2.xhtml#sec-2", "OEBPS/ch2.xhtml", Some("sec-2")),
            (
                "query and fragment",
                "OEBPS/ch1.xhtml",
                "ch2.xhtml?x=1#f",
                "OEBPS/ch2.xhtml",
                Some("f"),
            ),
            (
                "decoded fragment",
                "OEBPS/ch1.xhtml",
                "ch2.xhtml#caf%C3%A9%20menu",
                "OEBPS/ch2.xhtml",
                Some("café menu"),
            ),
            ("decoded segment", "OEBPS/x.opf", "my%20file.xhtml", "OEBPS/my file.xhtml", None),
            ("fragment only", "OEBPS/ch1.xhtml", "#local", "OEBPS/ch1.xhtml", Some("local")),
        ];
        for (label, base, reference, path, fragment) in cases {
            let target = Target { path: path.to_string(), fragment: fragment.map(str::to_string) };
            assert_eq!(resolve(base, reference), Ok(target), "{label}");
        }
        assert_eq!(decode_fragment("caf%C3%A9"), "café");
    }

    #[test]
    fn an_encoded_separator_or_traversal_segment_is_rejected() {
        let separator = "percent-encoded separator in a package reference";
        let traversal = "percent-encoded traversal in a package reference";
        let cases = [
            ("x%2Fy.xml", separator),
            ("x%5Cy.xml", separator),
            ("%2E%2E/secret.xml", traversal),
            ("%2e%2e/secret.xml", traversal),
        ];
        for (reference, reason) in cases {
            assert_eq!(resolve("a/b.xml", reference), Err(ConvertError::malformed(reason)));
        }
    }
}
