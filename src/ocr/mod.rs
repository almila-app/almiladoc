//! What the caller's OCR gives a parsed document's gaps, and why a fill is not applied.

mod driver;
mod engine;
mod fill;
#[cfg(feature = "ocr-http")]
mod http;
mod images;
mod raster;
mod request;

use std::fmt;

use crate::error::ConvertError;
use crate::limits::Limit;

pub(crate) use driver::{Target, run};
pub use engine::{OcrEngine, OcrFailure, OcrOutcome, OcrSummary};
pub(crate) use fill::{GapFills, GapState, gaps};
#[cfg(feature = "ocr-http")]
pub use http::{OcrHttp, OcrHttpConfig};
pub(crate) use images::leave_image_gaps;
pub use raster::RasterSpec;
pub(crate) use request::{Inputs, requests};
pub use request::{OcrImage, OcrRequest};

/// What OCR read for one gap.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GapFill {
    /// Markdown, plain text included: raw HTML in it stays text, a link its label and an image
    /// its alt, and its text is composed to NFC.
    Markdown(String),
}

impl fmt::Debug for GapFill {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Markdown(markdown) => write!(f, "Markdown({} bytes)", markdown.len()),
        }
    }
}

/// Why a fill or a fail was not applied; the parsed document is unchanged.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GapError {
    /// No gap of the document has this id.
    #[error("no gap of the document has this id")]
    UnknownGap,
    /// The gap was already filled or failed.
    #[error("the gap is already filled or failed")]
    Settled,
    /// The fill crossed a limit that bounds one fill, which a smaller fill may pass.
    #[error("fill refused: past {limit}")]
    #[non_exhaustive]
    Refused {
        /// The limit it crossed.
        limit: Limit,
    },
    /// Applying the fill failed as a conversion does: past
    /// [`Limits::max_work`](crate::Limits::max_work), cancelled, or internal.
    #[error(transparent)]
    Convert(ConvertError),
}

impl From<ConvertError> for GapError {
    fn from(error: ConvertError) -> Self {
        match error {
            ConvertError::ResourceLimit { limit, .. } if limit != Limit::Work => {
                Self::Refused { limit }
            }
            error => Self::Convert(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_fill_limit_is_a_refusal() {
        let cases = [
            (
                ConvertError::limit(Limit::OcrTextBytes, 1),
                GapError::Refused { limit: Limit::OcrTextBytes },
            ),
            (
                ConvertError::limit(Limit::NestingDepth, 1),
                GapError::Refused { limit: Limit::NestingDepth },
            ),
            (
                ConvertError::limit(Limit::GridSlots, 1),
                GapError::Refused { limit: Limit::GridSlots },
            ),
            (
                ConvertError::limit(Limit::Work, 1),
                GapError::Convert(ConvertError::limit(Limit::Work, 1)),
            ),
            (ConvertError::Cancelled, GapError::Convert(ConvertError::Cancelled)),
            (ConvertError::Internal, GapError::Convert(ConvertError::Internal)),
        ];
        for (error, expected) in cases {
            assert_eq!(GapError::from(error), expected);
        }
    }

    #[test]
    fn messages_and_debug_hold_no_fill_text() {
        let fill = GapFill::Markdown("secret".into());
        assert_eq!(format!("{fill:?}"), "Markdown(6 bytes)");
        let messages = [
            (GapError::UnknownGap, "no gap of the document has this id"),
            (GapError::Settled, "the gap is already filled or failed"),
            (
                GapError::Refused { limit: Limit::OcrTextBytes },
                "fill refused: past max_ocr_text_bytes",
            ),
            (GapError::Convert(ConvertError::Cancelled), "conversion cancelled"),
        ];
        for (error, message) in messages {
            assert_eq!(error.to_string(), message);
        }
    }
}
