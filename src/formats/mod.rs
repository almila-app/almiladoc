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
    detect::check_label(parse_ctx, bytes, format)?;
    match format {
        Format::Excel => sheet::parse(parse_ctx, bytes),
        Format::Csv => csv::parse(parse_ctx, bytes),
        Format::Docx => docx::parse(parse_ctx, bytes),
        Format::Odt | Format::Ods | Format::Odp => odf::parse(parse_ctx, bytes),
        Format::Pptx => pptx::parse(parse_ctx, bytes),
        Format::Epub => epub::parse(parse_ctx, bytes),
        Format::Rtf => rtf::parse(parse_ctx, bytes),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{Limit, Limits};

    #[test]
    fn the_label_checks_reads_count_against_the_parses_total_bytes() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/docx/text.docx");
        let bytes = std::fs::read(path).unwrap();
        let alone = ParseCtx::default();
        docx::parse(&alone, &bytes).unwrap();
        let checked = ParseCtx::default();
        parse(&checked, &bytes, Format::Docx).unwrap();
        assert!(checked.archive_bytes() > alone.archive_bytes());

        let limits = Limits { max_total_bytes: alone.archive_bytes(), ..Limits::DEFAULT };
        docx::parse(&ParseCtx::new(&limits), &bytes).unwrap();
        assert!(matches!(
            parse(&ParseCtx::new(&limits), &bytes, Format::Docx),
            Err(ConvertError::ResourceLimit { limit: Limit::TotalBytes, .. })
        ));
    }
}
