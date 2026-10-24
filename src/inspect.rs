//! What [`Converter::inspect`](crate::Converter::inspect) reports of a PDF: its pages and how
//! each reads, without rendering it.

use std::num::NonZeroU32;

use crate::diagnostics::Diagnostics;
use crate::model::GapReason;

/// A PDF's container facts, its pages' verdicts and the diagnostics its parse reports.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Inspection {
    pub(crate) pages: Vec<PageInspection>,
    pub(crate) diagnostics: Diagnostics,
}

impl Inspection {
    /// The pages the document holds.
    #[must_use]
    pub fn page_count(&self) -> u32 {
        u32::try_from(self.pages.len()).unwrap_or(u32::MAX)
    }

    /// Every page in order, each with its verdict.
    #[must_use]
    pub fn pages(&self) -> &[PageInspection] {
        &self.pages
    }

    /// What a parse of the document reports.
    #[must_use]
    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }
}

/// One page and how its text reads.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PageInspection {
    /// Its one-based number.
    pub page: NonZeroU32,
    /// How its text reads.
    pub verdict: PageVerdict,
}

/// How a page's text reads.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PageVerdict {
    /// Its text stands for what it shows.
    Text,
    /// It shows no text, nor an image or a drawing that could hold some.
    Blank,
    /// What it shows needs OCR, for these reasons.
    NeedsOcr {
        /// Why its own text does not serve, in a fixed order.
        reasons: Vec<GapReason>,
    },
}

impl PageVerdict {
    /// The verdict's stable `snake_case` name: `text`, `no_text` or `needs_ocr`.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Blank => "no_text",
            Self::NeedsOcr { .. } => "needs_ocr",
        }
    }
}
