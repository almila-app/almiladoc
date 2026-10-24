//! One frontend per input format; each parses bytes into the document model.

mod csv;
pub(crate) mod detect;
mod doc;
mod docx;
mod epub;
mod odf;
#[cfg(feature = "pdf")]
pub(crate) mod pdf;
mod ppt;
mod pptx;
mod rtf;
mod sheet;

use crate::Format;
use crate::ctx::ParseCtx;
use crate::error::{ConvertError, UnsupportedKind};
use crate::inspect::PageVerdict;
use crate::model::Document;

pub(crate) fn parse(
    parse_ctx: &ParseCtx<'_>,
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
        Format::Doc => doc::parse(parse_ctx, bytes),
        Format::Ppt => ppt::parse(parse_ctx, bytes),
        #[cfg(feature = "pdf")]
        Format::Pdf => pdf::parse(parse_ctx, bytes),
        #[cfg(not(feature = "pdf"))]
        Format::Pdf => Err(ConvertError::from(UnsupportedKind::FeatureDisabled { feature: "pdf" })),
    }
}

/// A PDF's document and each page's verdict, in page order; any other format is not inspectable.
#[cfg_attr(
    not(feature = "pdf"),
    expect(unused_variables, reason = "without the PDF reader nothing reads the input")
)]
pub(crate) fn inspect(
    parse_ctx: &ParseCtx<'_>,
    bytes: &[u8],
    format: Format,
) -> Result<(Document, Vec<PageVerdict>), ConvertError> {
    match format {
        #[cfg(feature = "pdf")]
        Format::Pdf => pdf::read(parse_ctx, bytes),
        #[cfg(not(feature = "pdf"))]
        Format::Pdf => Err(ConvertError::from(UnsupportedKind::FeatureDisabled { feature: "pdf" })),
        _ => Err(ConvertError::from(UnsupportedKind::NotInspectable)),
    }
}

#[cfg(test)]
#[expect(clippy::disallowed_methods, reason = "the tests read fixtures")]
mod tests {
    use super::*;
    use crate::limits::{Limit, Limits};
    use std::assert_matches;

    #[test]
    fn the_label_checks_reads_count_against_the_parses_total_bytes() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/docx/text.docx");
        let bytes = std::fs::read(path).unwrap();
        let alone = ParseCtx::default();
        docx::parse(&alone, &bytes).unwrap();
        let checked = ParseCtx::default();
        detect::check_label(&checked, &bytes, Format::Docx).unwrap();
        parse(&checked, &bytes, Format::Docx).unwrap();
        assert!(checked.archive_bytes() > alone.archive_bytes());

        let limits = Limits { max_total_bytes: alone.archive_bytes(), ..Limits::DEFAULT };
        docx::parse(&ParseCtx::new(&limits), &bytes).unwrap();
        assert_matches!(
            {
                let ctx = ParseCtx::new(&limits);
                detect::check_label(&ctx, &bytes, Format::Docx)
                    .and_then(|()| parse(&ctx, &bytes, Format::Docx))
            },
            Err(ConvertError::ResourceLimit { limit: Limit::TotalBytes, .. })
        );
    }
}
