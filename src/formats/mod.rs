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

/// What a parse keeps of its input, beside its document, to paint its pages: a PDF's parser.
#[derive(Default)]
pub(crate) struct Source {
    #[cfg(feature = "raster")]
    pub(crate) pdf: Option<pdf::PdfSource>,
}

impl Source {
    /// Whether it keeps pages to paint.
    #[cfg_attr(
        not(feature = "raster"),
        expect(clippy::unused_self, reason = "without painting no source keeps pages")
    )]
    pub(crate) fn paints(&self) -> bool {
        #[cfg(feature = "raster")]
        return self.pdf.is_some();
        #[cfg(not(feature = "raster"))]
        false
    }
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source").field("paints", &self.paints()).finish()
    }
}

#[cfg(test)]
pub(crate) fn parse(
    parse_ctx: &ParseCtx<'_>,
    bytes: &[u8],
    format: Format,
) -> Result<Document, ConvertError> {
    parse_keeping(parse_ctx, bytes, format).map(|(document, _)| document)
}

/// The document of `bytes` and the source its pages are painted from.
pub(crate) fn parse_keeping(
    parse_ctx: &ParseCtx<'_>,
    bytes: &[u8],
    format: Format,
) -> Result<(Document, Source), ConvertError> {
    let document = match format {
        Format::Excel => sheet::parse(parse_ctx, bytes),
        Format::Csv => csv::parse(parse_ctx, bytes),
        Format::Docx => docx::parse(parse_ctx, bytes),
        Format::Odt | Format::Ods | Format::Odp => odf::parse(parse_ctx, bytes),
        Format::Pptx => pptx::parse(parse_ctx, bytes),
        Format::Epub => epub::parse(parse_ctx, bytes),
        Format::Rtf => rtf::parse(parse_ctx, bytes),
        Format::Doc => doc::parse(parse_ctx, bytes),
        Format::Ppt => ppt::parse(parse_ctx, bytes),
        #[cfg(feature = "raster")]
        Format::Pdf => {
            let read = pdf::read(parse_ctx, bytes)?;
            return Ok((read.document, Source { pdf: Some(read.source) }));
        }
        #[cfg(all(feature = "pdf", not(feature = "raster")))]
        Format::Pdf => pdf::read(parse_ctx, bytes).map(|read| read.document),
        #[cfg(not(feature = "pdf"))]
        Format::Pdf => Err(ConvertError::from(UnsupportedKind::FeatureDisabled { feature: "pdf" })),
    }?;
    Ok((document, Source::default()))
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
        Format::Pdf => pdf::read(parse_ctx, bytes).map(|read| (read.document, read.verdicts)),
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
