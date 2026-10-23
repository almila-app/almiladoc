use crate::build::{docx_with, epub, odf, ods, pptx, relationships};
use crate::support::{
    configured, converter, corpus, fixture, format_of, limit_mut, reporting, zip_package,
};
use almiladoc::{
    CancelToken, Config, ConfigError, ConvertError, Converter, Format, Input, Limit, Limits,
    OnLoss, UnsupportedKind,
};
use std::assert_matches;

#[test]
fn every_fixture_converts_identically_on_one_and_three_threads() {
    let on_threads = |threads| {
        configured(|config| {
            config.threads = threads;
            config.on_loss = OnLoss::Report;
        })
    };
    let (one, three) = (on_threads(1), on_threads(3));
    let mut pdfs = 0;
    for path in corpus() {
        let bytes = std::fs::read(&path).unwrap();
        let format = format_of(&path, &bytes).unwrap();
        pdfs += usize::from(format == Format::Pdf);
        let single = one.convert(Input::new(&bytes, format)).map_err(|e| e.to_string());
        let several = three.convert(Input::new(&bytes, format)).map_err(|e| e.to_string());
        assert_eq!(single, several, "{}", path.display());
    }
    assert!(pdfs > 0, "the corpus holds PDF fixtures");
}

#[test]
fn a_config_out_of_range_is_refused() {
    let mut config = Config::default();
    config.limits.max_work = 0;
    assert_matches!(Converter::new(config), Err(ConfigError::ZeroLimit { limit: Limit::Work, .. }));

    let mut config = Config::default();
    config.limits.max_nesting_depth = Limits::MAX_NESTING_CEILING + 1;
    assert_matches!(
        Converter::new(config),
        Err(ConfigError::AboveCeiling { limit: Limit::NestingDepth, ceiling: 1024, .. })
    );

    for threads in [0, Config::MAX_THREADS + 1] {
        let mut config = Config::default();
        config.threads = threads;
        assert_matches!(Converter::new(config), Err(ConfigError::Threads { max: 64, .. }));
    }
}

#[cfg(feature = "pdf")]
#[test]
fn a_pdf_converts_but_has_no_document_model() {
    let bytes = fixture("pdf/text.pdf");
    let converter = converter(1);
    assert_ne!(converter.convert(Input::new(&bytes, Format::Pdf)).unwrap().markdown(), "");
    let error = converter.parse(Input::new(&bytes, Format::Pdf)).unwrap_err();
    assert_matches!(
        error,
        ConvertError::Unsupported { kind: UnsupportedKind::PdfDocumentModel, .. }
    );
}

#[cfg(not(feature = "pdf"))]
#[test]
fn a_build_without_pdf_refuses_every_pdf() {
    let bytes = fixture("pdf/text.pdf");
    let converter = converter(1);
    let errors = [
        converter.convert(Input::new(&bytes, Format::Pdf)).unwrap_err(),
        converter.parse(Input::new(&bytes, Format::Pdf)).unwrap_err(),
        converter.convert(Input::auto(&bytes, None)).unwrap_err(),
        converter.parse(Input::auto(&bytes, None)).unwrap_err(),
    ];
    for error in errors {
        assert_matches!(
            error,
            ConvertError::Unsupported {
                kind: UnsupportedKind::FeatureDisabled { feature: "pdf", .. },
                ..
            }
        );
    }
    assert_eq!(Format::sniff(&bytes), Some(Format::Pdf));
}

fn convert_with(bytes: &[u8], format: Format, limit: Limit, max: u64) -> Result<(), ConvertError> {
    with_limit(limit, max).convert(Input::new(bytes, format)).map(drop)
}

fn ods_with_repeated_text() -> Vec<u8> {
    ods(
        r#"<table:table-row table:number-rows-repeated="3"><table:table-cell><text:p>abcd</text:p></table:table-cell></table:table-row>"#,
    )
}

fn pptx_with_spanning_cell() -> Vec<u8> {
    let cell = |attrs: &str, text: &str| {
        format!("<a:tc {attrs}><a:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></a:txBody></a:tc>")
    };
    let row =
        format!("<a:tr>{}{}</a:tr>", cell(r#"gridSpan="3""#, "wide"), cell(r#"hMerge="1""#, ""));
    pptx(
        &format!(
            "<p:graphicFrame><a:graphic><a:graphicData><a:tbl>{row}</a:tbl></a:graphicData></a:graphic></p:graphicFrame>"
        ),
        &[],
    )
}

fn pptx_with_picture() -> Vec<u8> {
    let rels = relationships(&[("rId2", "image", "../media/image1.png")]);
    pptx(
        r#"<p:pic><p:nvPicPr><p:cNvPr id="2" name="Picture"/></p:nvPicPr><p:blipFill><a:blip r:embed="rId2"/></p:blipFill></p:pic>"#,
        &[
            ("ppt/slides/_rels/slide1.xml.rels", rels.as_bytes()),
            ("ppt/media/image1.png", b"\x89PNG\r\n\x1a\n"),
        ],
    )
}

fn epub_with_spanning_cell() -> Vec<u8> {
    epub(r#"<table><tr><td colspan="3">wide</td></tr></table>"#)
}

fn docx_nested(depth: usize) -> Vec<u8> {
    let w = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
    let body = "<w:body><w:p><w:r><w:t>deep</w:t></w:r></w:p></w:body>";
    let nest = format!("{}{}", "<x>".repeat(depth), "</x>".repeat(depth));
    let document = format!("<w:document {w}>{body}{nest}</w:document>");
    zip_package(&[("word/document.xml", document.as_bytes())])
}

fn odt_with_spanning_cell() -> Vec<u8> {
    odf(
        r#"<office:text><table:table table:name="T"><table:table-row><table:table-cell table:number-rows-spanned="3"><text:p>tall</text:p></table:table-cell></table:table-row></table:table></office:text>"#,
    )
}

struct Graphic {
    reference: &'static str,
    rel_type: &'static str,
    part: &'static str,
    root_ns: &'static str,
}

const CHART: Graphic = Graphic {
    reference: r#"<c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rId9"/>"#,
    rel_type: "chart",
    part: "charts/chart1.xml",
    root_ns: "http://schemas.openxmlformats.org/drawingml/2006/chart",
};

const DIAGRAM: Graphic = Graphic {
    reference: r#"<dgm:relIds xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" r:dm="rId9"/>"#,
    rel_type: "diagramData",
    part: "diagrams/data1.xml",
    root_ns: "http://schemas.openxmlformats.org/drawingml/2006/diagram",
};

impl Graphic {
    fn rels(&self, target_prefix: &str) -> String {
        relationships(&[("rId9", self.rel_type, &format!("{target_prefix}{}", self.part))])
    }

    fn deep_part(&self, depth: usize) -> String {
        let nest = format!("{}{}", "<x>".repeat(depth), "</x>".repeat(depth));
        format!(r#"<root xmlns="{}">{nest}</root>"#, self.root_ns)
    }
}

fn docx_with_deep(graphic: &Graphic, depth: usize) -> Vec<u8> {
    let body = format!(
        "<w:p><w:r><w:drawing><wp:inline><a:graphic><a:graphicData>{}</a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>",
        graphic.reference
    );
    let part = format!("word/{}", graphic.part);
    docx_with(
        &body,
        &[
            ("word/_rels/document.xml.rels", graphic.rels("").as_bytes()),
            (&part, graphic.deep_part(depth).as_bytes()),
        ],
    )
}

fn pptx_with_deep(graphic: &Graphic, depth: usize) -> Vec<u8> {
    let part = format!("ppt/{}", graphic.part);
    pptx(
        &format!(
            "<p:graphicFrame><a:graphic><a:graphicData>{}</a:graphicData></a:graphic></p:graphicFrame>",
            graphic.reference
        ),
        &[
            ("ppt/slides/_rels/slide1.xml.rels", graphic.rels("../").as_bytes()),
            (&part, graphic.deep_part(depth).as_bytes()),
        ],
    )
}

#[test]
fn every_enforced_limit_is_read_from_the_config() {
    let cases = [
        (fixture("docx/text.docx"), Format::Docx, Limit::EntryBytes, 64),
        (fixture("doc/text.doc"), Format::Doc, Limit::EntryBytes, 64),
        (fixture("xlsx/sheet.xlsx"), Format::Excel, Limit::EntryBytes, 64),
        (fixture("xls/sheet.xls"), Format::Excel, Limit::EntryBytes, 64),
        (fixture("ppt/pres.ppt"), Format::Ppt, Limit::EntryBytes, 64),
        (fixture("odt/text.odt"), Format::Odt, Limit::EntryBytes, 64),
        (fixture("epub/book.epub"), Format::Epub, Limit::EntryBytes, 64),
        (pptx_with_picture(), Format::Pptx, Limit::EntryBytes, 64),
        (fixture("docx/text.docx"), Format::Docx, Limit::TotalBytes, 64),
        (fixture("docx/text.docx"), Format::Docx, Limit::EntryCount, 1),
        (fixture("docx/text.docx"), Format::Docx, Limit::NestingDepth, 2),
        (docx_with_deep(&CHART, 20), Format::Docx, Limit::NestingDepth, 16),
        (docx_with_deep(&DIAGRAM, 20), Format::Docx, Limit::NestingDepth, 16),
        (pptx_with_deep(&CHART, 20), Format::Pptx, Limit::NestingDepth, 16),
        (pptx_with_deep(&DIAGRAM, 20), Format::Pptx, Limit::NestingDepth, 16),
        (fixture("docx/text.docx"), Format::Docx, Limit::XmlNodes, 3),
        (fixture("xlsx/sheet.xlsx"), Format::Excel, Limit::GridSlots, 1),
        (fixture("xlsx/handmade-merged.xlsx"), Format::Excel, Limit::Expansion, 1),
        (fixture("docx/handmade-tables.docx"), Format::Docx, Limit::Expansion, 1),
        (fixture("rtf/handmade-merge.rtf"), Format::Rtf, Limit::Expansion, 1),
        (fixture("ods/handmade-gaps.ods"), Format::Ods, Limit::Expansion, 1),
        (fixture("odt/text.odt"), Format::Odt, Limit::Expansion, 1),
        (pptx_with_spanning_cell(), Format::Pptx, Limit::Expansion, 1),
        (odt_with_spanning_cell(), Format::Odt, Limit::Expansion, 1),
        (epub_with_spanning_cell(), Format::Epub, Limit::Expansion, 1),
        (ods_with_repeated_text(), Format::Ods, Limit::ExpansionTextBytes, 4),
        (fixture("docx/handmade-ole.docx"), Format::Docx, Limit::AssetTotalBytes, 1),
        (fixture("odt/text.odt"), Format::Odt, Limit::AssetTotalBytes, 1),
        (pptx_with_picture(), Format::Pptx, Limit::AssetTotalBytes, 1),
        (fixture("epub/book.epub"), Format::Epub, Limit::AssetTotalBytes, 1),
        (fixture("rtf/text.rtf"), Format::Rtf, Limit::AssetTotalBytes, 1),
        (fixture("ppt/pres.ppt"), Format::Ppt, Limit::RecordDepth, 1),
        (fixture("ppt/pres.ppt"), Format::Ppt, Limit::Records, 1),
        (fixture("xls/sheet.xls"), Format::Excel, Limit::Records, 1),
        (fixture("xlsb/handmade-sheet.xlsb"), Format::Excel, Limit::Records, 1),
        (fixture("docx/text.docx"), Format::Docx, Limit::InputBytes, 64),
        (fixture("csv/sheet.csv"), Format::Csv, Limit::InputBytes, 64),
        (fixture("docx/text.docx"), Format::Docx, Limit::Work, 1),
        (fixture("rtf/text.rtf"), Format::Rtf, Limit::Work, 1),
        (fixture("csv/sheet.csv"), Format::Csv, Limit::Work, 1),
        (fixture("doc/text.doc"), Format::Doc, Limit::Work, 1),
        (fixture("ppt/pres.ppt"), Format::Ppt, Limit::Work, 1),
        (fixture("xls/sheet.xls"), Format::Excel, Limit::Work, 1),
    ];
    let defaults = reporting();
    for (bytes, format, limit, max) in cases {
        let case = format!("{format:?} {limit} {max}");
        assert!(defaults.convert(Input::new(&bytes, format)).is_ok(), "{case}");
        let error = convert_with(&bytes, format, limit, max).expect_err(&case);
        assert_matches!(
            error,
            ConvertError::ResourceLimit { limit: l, max: m, .. } if l == limit && m == max,
            "{case}"
        );
    }
}

#[test]
fn a_raised_nesting_depth_admits_deeper_input() {
    let bytes = docx_nested(300);
    let error = converter(1).convert(Input::new(&bytes, Format::Docx)).unwrap_err();
    assert_matches!(
        error,
        ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 256, .. }
    );
    convert_with(&bytes, Format::Docx, Limit::NestingDepth, Limits::MAX_NESTING_CEILING).unwrap();
}

fn with_limit(limit: Limit, max: u64) -> Converter {
    configured(|config| *limit_mut(&mut config.limits, limit) = max)
}

#[test]
fn max_input_bytes_is_checked_before_parse_and_convert_of_every_format() {
    let docx = fixture("docx/text.docx");
    let converter = with_limit(Limit::InputBytes, 64);
    for (bytes, format) in [(docx, Format::Docx), (fixture("pdf/text.pdf"), Format::Pdf)] {
        let parsed = converter.parse(Input::new(&bytes, format)).map(drop);
        let converted = converter.convert(Input::new(&bytes, format)).map(drop);
        for error in [parsed.unwrap_err(), converted.unwrap_err()] {
            assert_matches!(
                error,
                ConvertError::ResourceLimit { limit: Limit::InputBytes, max: 64, .. },
                "{format:?}"
            );
        }
    }
    let exact = with_limit(Limit::InputBytes, 7);
    exact.convert(Input::new(b"a,b\n1,2", Format::Csv)).unwrap();
    assert_matches!(
        exact.convert(Input::new(b"a,b\n1,2\n", Format::Csv)),
        Err(ConvertError::ResourceLimit { limit: Limit::InputBytes, max: 7, .. })
    );
}

#[test]
fn a_pre_cancelled_token_cancels_every_format() {
    let token = CancelToken::new();
    token.cancel();
    let converter = converter(1);
    for path in corpus() {
        let bytes = std::fs::read(&path).unwrap();
        let format = format_of(&path, &bytes).unwrap();
        let input = Input::new(&bytes, format).with_cancel(token.clone());
        let parsed = converter.parse(input.clone()).map(drop);
        let converted = converter.convert(input).map(drop);
        for result in [parsed, converted] {
            assert_matches!(result, Err(ConvertError::Cancelled), "{}", path.display());
        }
    }
}

#[test]
fn an_uncancelled_token_changes_nothing() {
    let bytes = fixture("docx/text.docx");
    let converter = converter(1);
    let plain = converter.convert(Input::new(&bytes, Format::Docx)).unwrap();
    let held = Input::new(&bytes, Format::Docx).with_cancel(CancelToken::new());
    assert_eq!(converter.convert(held).unwrap(), plain);
}
