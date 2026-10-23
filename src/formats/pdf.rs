//! PDF via [pdf-inspector]. A document with scanned or image-only pages
//! fails `NeedsOcr` naming them, whether every page or one of a hundred,
//! because output missing them would read as complete. Text from broken
//! font encodings is reported as `garbled_font_encoding`.
//!
//! [pdf-inspector]: https://docs.rs/pdf-inspector

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use pdf_inspector::PdfError;
use std::io::ErrorKind;

pub(crate) fn to_markdown(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<String, ConvertError> {
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
        parse_ctx.emit(DiagnosticCode::GarbledFontEncoding, Region::Body);
    }
    let Some(mut markdown) = result.markdown.filter(|markdown| !markdown.trim().is_empty()) else {
        return Err(textless(result.page_count));
    };
    if !markdown.ends_with('\n') {
        markdown.push('\n');
    }
    Ok(markdown)
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
pub(crate) mod tests {
    use super::*;
    use std::assert_matches;
    use std::fmt::Write as _;
    use std::io::Write as _;

    const HELVETICA: &str =
        "/Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >>";

    fn stream(data: &[u8]) -> Vec<u8> {
        [format!("<< /Length {} >>\nstream\n", data.len()).as_bytes(), data, b"\nendstream"]
            .concat()
    }

    fn pdf(page_count: usize, resources: &str, content: &[u8]) -> Vec<u8> {
        let kids: Vec<String> = (0..page_count).map(|i| format!("{} 0 R", 4 + i)).collect();
        let mut objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            format!("<< /Type /Pages /Kids [{}] /Count {page_count} >>", kids.join(" "))
                .into_bytes(),
            stream(content),
        ];
        let page = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 3 0 R {resources} >>"
        );
        objects.extend((0..page_count).map(|_| page.clone().into_bytes()));
        build(&objects)
    }

    fn build(objects: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            writeln!(out, "{} 0 obj", i + 1).unwrap();
            out.extend_from_slice(object);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        let size = objects.len() + 1;
        writeln!(out, "xref\n0 {size}\n0000000000 65535 f ").unwrap();
        for offset in offsets {
            writeln!(out, "{offset:010} 00000 n ").unwrap();
        }
        writeln!(out, "trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF").unwrap();
        out
    }

    /// One page of `sentence` in a composite font whose `ToUnicode` maps every
    /// character of it, followed by `unmapped` codes it does not map.
    pub(crate) fn composite_font_pdf(sentence: &str, unmapped: &[u16]) -> Vec<u8> {
        let mut chars: Vec<char> = sentence.chars().collect();
        chars.sort_unstable();
        chars.dedup();
        let code = |c: char| chars.iter().position(|d| *d == c).map_or(0, |i| i + 1);
        let mut shown = String::new();
        shown.extend(sentence.chars().map(|c| format!("{:04X}", code(c))));
        shown.extend(unmapped.iter().map(|code| format!("{code:04X}")));
        let mut cmap = String::from(
            "/CIDInit /ProcSet findresource begin 12 dict begin begincmap /CMapName /T def \
             1 begincodespacerange <0000> <FFFF> endcodespacerange\n",
        );
        writeln!(cmap, "{} beginbfchar", chars.len()).unwrap();
        for &c in &chars {
            writeln!(cmap, "<{:04X}> <{:04X}>", code(c), u32::from(c)).unwrap();
        }
        cmap.push_str("endbfchar endcmap CMapName currentdict /CMap defineresource pop end end");
        build(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [4 0 R] /Count 1 >>".to_vec(),
            stream(format!("BT /F1 12 Tf 20 100 Td <{shown}> Tj ET").as_bytes()),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 200] /Contents 3 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /Arial /Encoding /Identity-H \
              /DescendantFonts [6 0 R] /ToUnicode 7 0 R >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /Arial /CIDSystemInfo \
              << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /DW 500 >>"
                .to_vec(),
            stream(cmap.as_bytes()),
        ])
    }

    #[test]
    fn a_page_with_text_converts() {
        let bytes = pdf(1, HELVETICA, b"BT /F1 12 Tf 20 100 Td (Hello) Tj ET");
        let parse_ctx = ParseCtx::default();
        assert!(to_markdown(&parse_ctx, &bytes).unwrap().contains("Hello"));
        assert!(parse_ctx.into_diagnostics().is_empty());
    }

    #[test]
    fn a_code_no_cmap_maps_is_garbled_font_encoding_lost_in_the_body() {
        let sentence = "The board approved the plan for the coming season after a long review.";
        let parse_ctx = ParseCtx::default();
        let markdown = to_markdown(&parse_ctx, &composite_font_pdf(sentence, &[])).unwrap();
        assert_eq!(markdown, format!("{sentence}\n"));
        assert!(parse_ctx.into_diagnostics().is_empty());

        let parse_ctx = ParseCtx::default();
        let markdown = to_markdown(&parse_ctx, &composite_font_pdf(sentence, &[0x0200])).unwrap();
        assert!(markdown.starts_with(sentence), "{markdown}");
        let diagnostics: Vec<_> = parse_ctx
            .into_diagnostics()
            .iter()
            .map(|d| (d.code, d.class, d.region, d.unit, d.count))
            .collect();
        assert_eq!(
            diagnostics,
            [(
                DiagnosticCode::GarbledFontEncoding,
                crate::diagnostics::LossClass::Lost,
                Region::Body,
                None,
                1
            )]
        );
    }

    #[test]
    fn a_textless_pdf_needs_ocr_on_every_page() {
        let err = to_markdown(&ParseCtx::default(), &pdf(2, "", b"0 0 m 100 100 l S")).unwrap_err();
        assert_matches!(&err, ConvertError::NeedsOcr { pages, page_count: 2 } if pages == &[1, 2]);
    }

    #[test]
    fn a_pdf_without_pages_is_malformed() {
        let err = to_markdown(&ParseCtx::default(), &pdf(0, "", b"")).unwrap_err();
        assert_matches!(err, ConvertError::Malformed { part: None, .. });
    }

    #[test]
    fn a_textless_result_names_every_page_or_reports_none() {
        assert_matches!(
            textless(3),
            ConvertError::NeedsOcr { pages, page_count: 3 } if pages == [1, 2, 3]
        );
        assert_matches!(
            textless(0),
            ConvertError::Malformed { part: None, reason: "PDF has no pages" }
        );
    }

    #[test]
    fn engine_errors_carry_no_engine_text() {
        let oom = map_error(PdfError::Io(ErrorKind::OutOfMemory.into()));
        assert_matches!(oom, ConvertError::ResourceLimit { limit: Limit::Allocation, max: 0 });
        for e in [
            PdfError::Io(ErrorKind::InvalidData.into()),
            PdfError::NotAPdf("\u{1b}[2J secret".into()),
            PdfError::Parse("\u{1b}[2J secret".into()),
            PdfError::InvalidStructure,
        ] {
            let mapped = map_error(e);
            assert_matches!(mapped, ConvertError::Malformed { part: None, .. });
            assert!(!mapped.to_string().contains("secret"));
        }
        assert_matches!(map_error(PdfError::Encrypted), ConvertError::Encrypted);
    }
}
