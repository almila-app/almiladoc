//! Whether a hyperlink's visible label already shows its external target,
//! so dropping the target loses nothing a reader needs.

/// True when `label` shows `target`: equal after normalising both (scheme
/// and host case-folded, an empty path read as `/`, percent-decoded), equal
/// to the target without its `http`, `https` or `mailto` scheme, or, for a
/// `doi.org` target, equal to its DOI with or without a `doi:` prefix.
pub(crate) fn label_shows_target(label: &str, target: &str) -> bool {
    let label = label.trim();
    if label.is_empty() {
        return false;
    }
    let target = Url::parse(target.trim());
    let without_scheme = match target.scheme.as_deref() {
        Some(scheme @ ("http" | "https")) => Url::parse(&format!("{scheme}://{label}")) == target,
        Some("mailto") => Url::parse(&format!("mailto:{label}")) == target,
        _ => false,
    };
    Url::parse(label) == target || without_scheme || shows_doi(label, &target)
}

/// True when `label` is itself an absolute URL, `scheme://…` or `mailto:…`,
/// so a link whose target is unknown still shows where it points.
pub(crate) fn label_is_absolute_url(label: &str) -> bool {
    let label = label.trim();
    let mailto = label.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("mailto:"));
    let hierarchical =
        label.split_once("://").is_some_and(|(scheme, rest)| is_scheme(scheme) && !rest.is_empty());
    (mailto && label.len() > 7) || hierarchical
}

fn shows_doi(label: &str, target: &Url) -> bool {
    let Some(path) = target.rest.strip_prefix("//") else {
        return false;
    };
    let Some((host, doi)) = path.split_once('/') else {
        return false;
    };
    if !matches!(host, "doi.org" | "dx.doi.org") || doi.is_empty() {
        return false;
    }
    let label = match (label.get(..4), label.get(4..)) {
        (Some(prefix), Some(rest)) if prefix.eq_ignore_ascii_case("doi:") => rest.trim_start(),
        _ => label,
    };
    label.eq_ignore_ascii_case(doi)
}

/// A URL reduced to what a label comparison needs: its lowercase scheme and
/// the rest, with a hierarchical host or a mail domain lowercased and
/// an empty path read as `/`, all percent-decoded.
#[derive(PartialEq, Eq)]
struct Url {
    scheme: Option<String>,
    rest: String,
}

impl Url {
    fn parse(s: &str) -> Url {
        let s = percent_decoded(s);
        let (scheme, rest) = match s.split_once(':') {
            Some((scheme, rest)) if is_scheme(scheme) => {
                (Some(scheme.to_ascii_lowercase()), rest.to_string())
            }
            _ => (None, s),
        };
        let rest = match rest.strip_prefix("//") {
            Some(hier) => {
                let end = hier.find(['/', '?', '#']).unwrap_or(hier.len());
                let (authority, path) = hier.split_at(end);
                let root = if path.starts_with('/') { "" } else { "/" };
                let (user, host) = match authority.rsplit_once('@') {
                    Some((user, host)) => (format!("{user}@"), host),
                    None => (String::new(), authority),
                };
                format!("//{user}{}{root}{path}", host.to_lowercase())
            }
            None if scheme.as_deref() == Some("mailto") => {
                let (address, query) = rest.split_at(rest.find(['?', '#']).unwrap_or(rest.len()));
                match address.rsplit_once('@') {
                    Some((local, domain)) => format!("{local}@{}{query}", domain.to_lowercase()),
                    None => rest,
                }
            }
            None => rest,
        };
        Url { scheme, rest }
    }
}

fn is_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn percent_decoded(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut rest = s.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        let decoded = tail
            .get(..2)
            .filter(|hex| b == b'%' && hex.iter().all(u8::is_ascii_hexdigit))
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (decoded, tail.get(2..)) {
            (Some(decoded), Some(after)) => {
                out.push(decoded);
                rest = after;
            }
            _ => {
                out.push(b);
                rest = tail;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::{label_is_absolute_url, label_shows_target};

    #[test]
    fn a_label_shows_its_target_after_normalisation() {
        for (label, target) in [
            ("http://www.example.org/", "http://www.example.org/"),
            ("www.example.org", "http://www.example.org/"),
            ("HTTPS://Example.ORG", "https://example.org/"),
            ("example.org/a b", "https://example.org/a%20b"),
            ("example.org/çay", "https://example.org/%C3%A7ay"),
            ("someone@example.org", "mailto:someone@example.org"),
            ("10.1234/abc", "https://doi.org/10.1234/abc"),
            ("doi:10.1234/ABC", "https://dx.doi.org/10.1234/abc"),
            ("  10.1234/abc ", "http://doi.org/10.1234/abc"),
            ("www.example.org:8080/x", "http://www.example.org:8080/x"),
            ("localhost:8888", "http://localhost:8888/"),
            ("someone@Example.org", "mailto:someone@example.org"),
            ("example.org/#x", "http://example.org#x"),
            ("ftp://User@Example.org/a", "ftp://User@example.org/a"),
            ("someone@Example.org?subject=Hi", "mailto:someone@example.org?subject=Hi"),
        ] {
            assert!(label_shows_target(label, target), "{label} -> {target}");
        }
    }

    #[test]
    fn a_label_that_hides_its_target_does_not_show_it() {
        for (label, target) in [
            ("see here", "http://www.example.org/"),
            ("", "http://www.example.org/"),
            ("www.example.org/other", "http://www.example.org/"),
            ("example.org", "ftp://example.org/"),
            ("10.1234/abc", "https://example.org/10.1234/abc"),
            ("doi:", "https://doi.org/"),
            ("someone", "mailto:someone@example.org"),
            ("Someone@example.org", "mailto:someone@example.org"),
            ("example.org/%+1", "http://example.org/%01"),
            ("https://example.org/", "http://example.org/"),
            ("ftp://user@example.org/a", "ftp://User@example.org/a"),
            ("someone@example.org?subject=hi", "mailto:someone@example.org?subject=Hi"),
        ] {
            assert!(!label_shows_target(label, target), "{label} -> {target}");
        }
    }

    #[test]
    fn only_a_scheme_with_an_authority_or_mailto_is_an_absolute_label() {
        for label in ["https://example.org/x", " HTTP://a ", "mailto:someone@example.org"] {
            assert!(label_is_absolute_url(label), "{label}");
        }
        for label in ["localhost:8888", "www.example.org:8080/x", "see here", "https://", "mailto:"]
        {
            assert!(!label_is_absolute_url(label), "{label}");
        }
    }
}
