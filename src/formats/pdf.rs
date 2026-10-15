//! PDF via [pdf-inspector]: classification plus direct Markdown extraction.
//!
//! Unlike the other frontends, pdf-inspector emits Markdown itself, so PDFs
//! bypass the document model and the shared GFM writer. OCR is out of scope
//! here: a document with scanned or image-only pages errors naming them,
//! whether that is every page or one of a hundred, because output missing
//! those pages would read as complete.
//!
//! [pdf-inspector]: https://github.com/firecrawl/pdf-inspector

use crate::error::ConvertError;
use crate::limits::Limit;
use pdf_inspector::PdfError;
use std::io::ErrorKind;

pub fn to_markdown(bytes: &[u8]) -> Result<String, ConvertError> {
    let result = pdf_inspector::process_pdf_mem(bytes).map_err(map_error)?;
    if !result.pages_needing_ocr.is_empty() {
        // Detection samples content streams and over-reports short or
        // image-heavy text pages; extraction knows which of them yielded none.
        let flagged: Vec<u32> = result.pages_needing_ocr.iter().map(|page| page - 1).collect();
        let pages = pdf_inspector::extract_pages_markdown_mem(bytes, Some(&flagged))
            .map_err(map_error)?
            .pages_needing_ocr;
        if !pages.is_empty() {
            return Err(ConvertError::NeedsOcr { pages, page_count: result.page_count });
        }
    }
    if result.has_encoding_issues {
        log::warn!("broken font encodings detected; extracted text may be garbled");
    }
    match result.markdown {
        Some(mut markdown) if !markdown.trim().is_empty() => {
            if !markdown.ends_with('\n') {
                markdown.push('\n');
            }
            Ok(markdown)
        }
        _ => Err(textless(result.page_count)),
    }
}

fn textless(page_count: u32) -> ConvertError {
    match page_count {
        0 => ConvertError::malformed("PDF has no pages"),
        _ => ConvertError::NeedsOcr { pages: (1..=page_count).collect(), page_count },
    }
}

fn map_error(e: PdfError) -> ConvertError {
    match e {
        PdfError::Encrypted => ConvertError::Encrypted,
        PdfError::Io(e) if e.kind() == ErrorKind::OutOfMemory => {
            ConvertError::limit(Limit::Allocation, 0)
        }
        PdfError::Io(_) => ConvertError::malformed("unreadable PDF"),
        PdfError::NotAPdf(_) => ConvertError::malformed("not a PDF"),
        PdfError::InvalidStructure => ConvertError::malformed("invalid PDF structure"),
        PdfError::Parse(_) => ConvertError::malformed("unparseable PDF"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELVETICA: &str =
        "/Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >>";

    fn pdf(page_count: usize, resources: &str, content: &[u8]) -> Vec<u8> {
        let kids: Vec<String> = (0..page_count).map(|i| format!("{} 0 R", 4 + i)).collect();
        let mut objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            format!("<< /Type /Pages /Kids [{}] /Count {page_count} >>", kids.join(" "))
                .into_bytes(),
            [
                format!("<< /Length {} >>\nstream\n", content.len()).as_bytes(),
                content,
                b"\nendstream",
            ]
            .concat(),
        ];
        let page = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 3 0 R {resources} >>"
        );
        objects.extend((0..page_count).map(|_| page.clone().into_bytes()));
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(object);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        let size = objects.len() + 1;
        out.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                .as_bytes(),
        );
        out
    }

    #[test]
    fn a_page_with_text_converts() {
        let bytes = pdf(1, HELVETICA, b"BT /F1 12 Tf 20 100 Td (Hello) Tj ET");
        assert!(to_markdown(&bytes).unwrap().contains("Hello"));
    }

    #[test]
    fn a_textless_pdf_needs_ocr_on_every_page() {
        let err = to_markdown(&pdf(2, "", b"0 0 m 100 100 l S")).unwrap_err();
        assert!(
            matches!(&err, ConvertError::NeedsOcr { pages, page_count: 2 } if pages == &[1, 2]),
            "{err:?}"
        );
    }

    #[test]
    fn a_textless_result_names_every_page() {
        let err = textless(3);
        assert!(
            matches!(&err, ConvertError::NeedsOcr { pages, page_count: 3 } if pages == &[1, 2, 3]),
            "{err:?}"
        );
    }

    #[test]
    fn a_pdf_without_pages_is_malformed() {
        let err = to_markdown(&pdf(0, "", b"")).unwrap_err();
        assert!(matches!(err, ConvertError::Malformed { part: None, .. }), "{err:?}");
        let err = textless(0);
        assert!(
            matches!(err, ConvertError::Malformed { part: None, reason: "PDF has no pages" }),
            "{err:?}"
        );
    }

    #[test]
    fn engine_errors_carry_no_engine_text() {
        let oom = map_error(PdfError::Io(ErrorKind::OutOfMemory.into()));
        assert!(matches!(oom, ConvertError::ResourceLimit { limit: Limit::Allocation, max: 0 }));
        for e in [
            PdfError::Io(ErrorKind::InvalidData.into()),
            PdfError::NotAPdf("\u{1b}[2J secret".into()),
            PdfError::Parse("\u{1b}[2J secret".into()),
            PdfError::InvalidStructure,
        ] {
            let mapped = map_error(e);
            assert!(matches!(mapped, ConvertError::Malformed { part: None, .. }), "{mapped:?}");
            assert!(!mapped.to_string().contains("secret"));
        }
        assert!(matches!(map_error(PdfError::Encrypted), ConvertError::Encrypted));
    }
}
