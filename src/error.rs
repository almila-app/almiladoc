//! Typed conversion errors.
//!
//! An error means a complete conversion was impossible: the input was
//! unreadable or structurally unusable, encrypted, crossed a safety limit,
//! or lost content under [`OnLoss::Fail`](crate::OnLoss::Fail). Recoverable
//! producer quirks are recovered or skipped, and reported as [`Diagnostics`].
//! No message carries input text beyond a [`PartName`].

use std::fmt;

use crate::Format;
use crate::diagnostics::Diagnostics;
use crate::limits::{Limit, LimitExceeded};

/// Why a conversion could not produce a useful result.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConvertError {
    /// The input cannot be converted at all.
    #[error("unsupported input: {kind}")]
    #[non_exhaustive]
    Unsupported {
        /// Why it cannot.
        kind: UnsupportedKind,
    },
    /// The document is structurally unusable: no meaningful content could
    /// be extracted.
    #[error("malformed document{}: {reason}", in_part(part.as_ref()))]
    #[non_exhaustive]
    Malformed {
        /// Package part or stream the failure was found in, when known.
        part: Option<PartName>,
        /// What was wrong with it.
        reason: &'static str,
    },
    /// The document is encrypted or password-protected.
    #[error("document is encrypted")]
    Encrypted,
    /// A part required for any meaningful output is missing.
    #[error("missing required part: {part}")]
    #[non_exhaustive]
    MissingPart {
        /// The part or stream that was absent.
        part: PartName,
    },
    /// A safety limit was crossed, or the allocator refused a reservation.
    /// Fatal everywhere, optional parts included.
    #[error("{}", resource_limit(*limit, *max))]
    #[non_exhaustive]
    ResourceLimit {
        /// The limit that was crossed.
        limit: Limit,
        /// The limit's value; for [`Limit::Allocation`], the size of the
        /// reservation or the bound of the read the allocator refused, or 0
        /// where the PDF engine does not say.
        max: u64,
    },
    /// Some pages of a PDF are scanned or image-only and need OCR, which
    /// almiladoc does not do. Nothing is returned for the document, so
    /// output missing those pages never passes as complete.
    #[error("{}", needs_ocr(pages, *page_count))]
    #[non_exhaustive]
    NeedsOcr {
        /// 1-indexed pages that need OCR.
        pages: Vec<u32>,
        /// Pages in the document.
        page_count: u32,
    },
    /// Content was lost, and the configuration's
    /// [`OnLoss`](crate::OnLoss) is `Fail`.
    #[error("{}", incomplete(diagnostics))]
    #[non_exhaustive]
    Incomplete {
        /// Every diagnostic of the conversion, the losses among them.
        diagnostics: Diagnostics,
    },
    /// The caller cancelled the conversion.
    #[error("conversion cancelled")]
    Cancelled,
    /// The converter failed in a way no input should cause, a panic
    /// included, or the host could not start the call's threads; the caller
    /// recycles the worker.
    #[error("internal error")]
    Internal,
}

/// Why an input cannot be converted at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnsupportedKind {
    /// No format was named and none was recognized from the content.
    UnknownFormat,
    /// The format needs a crate feature this build leaves out.
    #[non_exhaustive]
    FeatureDisabled {
        /// The missing feature.
        feature: &'static str,
    },
    /// The content is a different format from the one named.
    #[non_exhaustive]
    FormatMismatch {
        /// The format the content was recognized as.
        detected: Format,
    },
    /// A PDF has no document-model form; it converts to Markdown only.
    PdfDocumentModel,
}

impl fmt::Display for UnsupportedKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnsupportedKind::UnknownFormat => {
                f.write_str("unrecognized content: name the format explicitly")
            }
            UnsupportedKind::FeatureDisabled { feature } => {
                write!(f, "this build leaves out the {feature} feature")
            }
            UnsupportedKind::FormatMismatch { detected } => {
                write!(f, "the content is {detected:?}, not the format named")
            }
            UnsupportedKind::PdfDocumentModel => {
                f.write_str("a PDF has no document model; convert it to Markdown")
            }
        }
    }
}

/// A package part or stream name, escaped and capped so that it can be
/// shown safely.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PartName(String);

impl PartName {
    /// Longest escaped name kept, in bytes.
    pub const MAX_BYTES: usize = 256;

    pub(crate) fn new(raw: &str) -> Self {
        let mut name = String::new();
        for c in raw.chars() {
            let escaped = if c.is_control() || is_bidi_control(c) {
                c.escape_unicode().to_string()
            } else {
                c.to_string()
            };
            if name.len() + escaped.len() > Self::MAX_BYTES {
                break;
            }
            name.push_str(&escaped);
        }
        PartName(name)
    }

    /// The escaped name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PartName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

impl From<LimitExceeded> for ConvertError {
    fn from(e: LimitExceeded) -> Self {
        ConvertError::ResourceLimit { limit: e.limit, max: e.max }
    }
}

impl ConvertError {
    /// Stable, machine-readable name for the variant: what a caller branches
    /// on, where the `Display` message carries the detail.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            ConvertError::Unsupported { .. } => "unsupported",
            ConvertError::NeedsOcr { .. } => "needsOcr",
            ConvertError::Malformed { .. } => "malformed",
            ConvertError::Encrypted => "encrypted",
            ConvertError::ResourceLimit { .. } => "resourceLimit",
            ConvertError::MissingPart { .. } => "missingPart",
            ConvertError::Incomplete { .. } => "incomplete",
            ConvertError::Cancelled => "cancelled",
            ConvertError::Internal => "internal",
        }
    }

    pub(crate) fn unsupported(kind: UnsupportedKind) -> Self {
        ConvertError::Unsupported { kind }
    }

    pub(crate) fn malformed(reason: &'static str) -> Self {
        ConvertError::Malformed { part: None, reason }
    }

    pub(crate) fn malformed_part(part: &str, reason: &'static str) -> Self {
        ConvertError::Malformed { part: Some(PartName::new(part)), reason }
    }

    pub(crate) fn missing_part(part: &str) -> Self {
        ConvertError::MissingPart { part: PartName::new(part) }
    }

    pub(crate) fn limit(limit: Limit, max: u64) -> Self {
        ConvertError::ResourceLimit { limit, max }
    }

    /// True for `ResourceLimit`, `Cancelled` and `Internal`, which no
    /// recovery swallows, optional parts included.
    pub(crate) fn is_fatal(&self) -> bool {
        matches!(
            self,
            ConvertError::ResourceLimit { .. } | ConvertError::Cancelled | ConvertError::Internal
        )
    }
}

fn in_part(part: Option<&PartName>) -> String {
    part.map_or_else(String::new, |part| format!(" ({part})"))
}

fn resource_limit(limit: Limit, max: u64) -> String {
    match (limit, max) {
        (Limit::Allocation, 0) => "resource limit exceeded: allocation refused".to_string(),
        (Limit::Allocation, _) => {
            format!("resource limit exceeded: allocation refused reading up to {max} bytes")
        }
        _ => format!("resource limit exceeded: {limit} is {max}"),
    }
}

fn incomplete(diagnostics: &Diagnostics) -> String {
    let lost: Vec<&str> = diagnostics.lost_codes().map(|code| code.as_str()).collect();
    format!("incomplete conversion, content lost: {}", lost.join(", "))
}

fn needs_ocr(pages: &[u32], page_count: u32) -> String {
    match pages {
        [page] => format!("page {page} of {page_count} needs OCR"),
        _ if pages.len() >= page_count as usize => format!("all {page_count} pages need OCR"),
        _ => format!("pages {} of {page_count} need OCR", page_ranges(pages)),
    }
}

/// `2, 5-7, 12` from ascending page numbers.
fn page_ranges(pages: &[u32]) -> String {
    let mut ranges = Vec::new();
    let mut pages = pages.iter().copied().peekable();
    while let Some(start) = pages.next() {
        let mut end = start;
        while pages.next_if_eq(&(end + 1)).is_some() {
            end += 1;
        }
        ranges.push(if end > start { format!("{start}-{end}") } else { start.to_string() });
    }
    ranges.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_variant() -> Vec<ConvertError> {
        vec![
            ConvertError::unsupported(UnsupportedKind::UnknownFormat),
            ConvertError::NeedsOcr { pages: vec![1], page_count: 1 },
            ConvertError::malformed("bad"),
            ConvertError::Encrypted,
            ConvertError::limit(Limit::EntryBytes, 1),
            ConvertError::missing_part("word/document.xml"),
            ConvertError::Incomplete { diagnostics: lost() },
            ConvertError::Cancelled,
            ConvertError::Internal,
        ]
    }

    fn lost() -> Diagnostics {
        use crate::diagnostics::{DiagnosticCode, Region};
        let mut diagnostics = Diagnostics::default();
        diagnostics.add(DiagnosticCode::RecoveredXml, Region::Note, None, 2);
        diagnostics.add(DiagnosticCode::StyleCycle, Region::Container, None, 1);
        diagnostics.add(DiagnosticCode::PartUnreadable, Region::Body, None, 1);
        diagnostics.add(DiagnosticCode::PartUnreadable, Region::Note, None, 1);
        diagnostics
    }

    /// The message alone has to name the pages.
    #[test]
    fn needs_ocr_names_the_pages() {
        let scattered = ConvertError::NeedsOcr { pages: vec![2, 5, 6, 7, 12], page_count: 20 };
        assert_eq!(scattered.to_string(), "pages 2, 5-7, 12 of 20 need OCR");
        let all = ConvertError::NeedsOcr { pages: vec![1, 2], page_count: 2 };
        assert_eq!(all.to_string(), "all 2 pages need OCR");
        let one = ConvertError::NeedsOcr { pages: vec![3], page_count: 4 };
        assert_eq!(one.to_string(), "page 3 of 4 needs OCR");
    }

    /// Callers branch on these strings, so changing one breaks them.
    #[test]
    fn codes_name_every_variant() {
        let codes: Vec<&str> = every_variant().iter().map(ConvertError::code).collect();
        assert_eq!(
            codes,
            [
                "unsupported",
                "needsOcr",
                "malformed",
                "encrypted",
                "resourceLimit",
                "missingPart",
                "incomplete",
                "cancelled",
                "internal"
            ]
        );
    }

    #[test]
    fn recovery_never_swallows_limits_cancellation_or_internal_faults() {
        let fatal: Vec<bool> = every_variant().iter().map(ConvertError::is_fatal).collect();
        assert_eq!(fatal, [false, false, false, false, true, false, false, true, true]);
        assert!(ConvertError::limit(Limit::Allocation, 64).is_fatal());
    }

    #[test]
    fn messages_carry_no_input_text_beyond_the_part_name() {
        let cases = [
            (ConvertError::malformed("no document body"), "malformed document: no document body"),
            (
                ConvertError::malformed_part("word/document.xml", "no document body"),
                "malformed document (word/document.xml): no document body",
            ),
            (
                ConvertError::missing_part("ppt/slides/slide1.xml"),
                "missing required part: ppt/slides/slide1.xml",
            ),
            (
                ConvertError::limit(Limit::EntryBytes, 134_217_728),
                "resource limit exceeded: max_entry_bytes is 134217728",
            ),
            (
                ConvertError::limit(Limit::Allocation, 4096),
                "resource limit exceeded: allocation refused reading up to 4096 bytes",
            ),
            (
                ConvertError::limit(Limit::Allocation, 0),
                "resource limit exceeded: allocation refused",
            ),
            (
                ConvertError::unsupported(UnsupportedKind::UnknownFormat),
                "unsupported input: unrecognized content: name the format explicitly",
            ),
            (
                ConvertError::unsupported(UnsupportedKind::FeatureDisabled { feature: "pdf" }),
                "unsupported input: this build leaves out the pdf feature",
            ),
            (
                ConvertError::unsupported(UnsupportedKind::FormatMismatch {
                    detected: Format::Rtf,
                }),
                "unsupported input: the content is Rtf, not the format named",
            ),
            (
                ConvertError::unsupported(UnsupportedKind::PdfDocumentModel),
                "unsupported input: a PDF has no document model; convert it to Markdown",
            ),
            (ConvertError::Encrypted, "document is encrypted"),
            (
                ConvertError::Incomplete { diagnostics: lost() },
                "incomplete conversion, content lost: part_unreadable, recovered_xml",
            ),
            (ConvertError::Cancelled, "conversion cancelled"),
            (ConvertError::Internal, "internal error"),
        ];
        for (error, message) in cases {
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn limit_exceeded_keeps_its_limit_and_max() {
        let error = ConvertError::from(LimitExceeded { limit: Limit::Expansion, max: 7 });
        assert!(matches!(error, ConvertError::ResourceLimit { limit: Limit::Expansion, max: 7 }));
    }

    #[test]
    fn part_names_escape_controls_and_bidi_overrides() {
        let name = PartName::new("a\u{1b}[31m\u{202e}b\u{2066}c\u{7f}d\u{200b}é");
        assert_eq!(name.as_str(), "a\\u{1b}[31m\\u{202e}b\\u{2066}c\\u{7f}d\u{200b}é");
        for c in ['\u{202a}', '\u{202d}', '\u{2069}', '\u{0}', '\u{9f}'] {
            assert_eq!(PartName::new(&c.to_string()).as_str(), c.escape_unicode().to_string());
        }
        for c in ['\u{2029}', '\u{206a}', '\u{2065}', '\u{202f}'] {
            assert_eq!(PartName::new(&c.to_string()).as_str(), c.to_string());
        }
    }

    #[test]
    fn part_names_cap_at_256_bytes_without_splitting_a_char_or_an_escape() {
        let exact = "a".repeat(PartName::MAX_BYTES);
        assert_eq!(PartName::new(&exact).as_str(), exact);
        assert_eq!(PartName::new(&format!("{exact}b")).as_str(), exact);

        let wide = format!("{}é", "a".repeat(PartName::MAX_BYTES - 1));
        assert_eq!(PartName::new(&wide).as_str(), "a".repeat(PartName::MAX_BYTES - 1));

        let escape = format!("{}\u{1b}b", "a".repeat(PartName::MAX_BYTES - 5));
        assert_eq!(PartName::new(&escape).as_str(), "a".repeat(PartName::MAX_BYTES - 5));
        let fits = format!("{}\u{1b}", "a".repeat(PartName::MAX_BYTES - 6));
        let kept = PartName::new(&fits);
        assert_eq!(kept.as_str().len(), PartName::MAX_BYTES);
        assert!(kept.as_str().ends_with("\\u{1b}"));
    }
}
