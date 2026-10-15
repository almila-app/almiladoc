//! One frontend per input format; each parses bytes into the document model.

mod csv;
pub mod detect;
mod doc;
mod docx;
mod epub;
mod odf;
#[cfg(feature = "pdf")]
pub mod pdf;
mod ppt;
mod pptx;
mod rtf;
mod sheet;

use crate::Format;
use crate::ctx::ParseCtx;
use crate::error::{ConvertError, UnsupportedKind};
use crate::model::Document;

pub(crate) fn parse(
    parse_ctx: &ParseCtx,
    bytes: &[u8],
    format: Format,
) -> Result<Document, ConvertError> {
    match format {
        Format::Excel => sheet::parse(parse_ctx, bytes),
        Format::Csv => csv::parse(parse_ctx, bytes),
        Format::Docx => docx::parse(parse_ctx, bytes),
        Format::Odt | Format::Ods | Format::Odp => odf::parse(parse_ctx, bytes),
        Format::Pptx => pptx::parse(parse_ctx, bytes),
        Format::Epub => epub::parse(parse_ctx, bytes),
        Format::Rtf => rtf::parse(parse_ctx, bytes),
        // RTF files wearing a .doc extension are common in the wild.
        Format::Doc if bytes.starts_with(b"{\\rtf") => rtf::parse(parse_ctx, bytes),
        Format::Doc => doc::parse(parse_ctx, bytes),
        Format::Ppt => ppt::parse(parse_ctx, bytes),
        #[cfg(feature = "pdf")]
        Format::Pdf => Err(ConvertError::unsupported(UnsupportedKind::PdfDocumentModel)),
        #[cfg(not(feature = "pdf"))]
        Format::Pdf => {
            Err(ConvertError::unsupported(UnsupportedKind::FeatureDisabled { feature: "pdf" }))
        }
    }
}
