mod common;

use almiladoc::{
    CancelToken, Config, ConfigError, ConvertError, Converter, Format, Input, Limit, Limits,
    UnsupportedKind,
};
use common::{converter, fixture_root, format_of, walk};

const fn assert_send_sync<T: Send + Sync>() {}
const _: () = assert_send_sync::<Converter>();

#[test]
fn every_fixture_converts_identically_on_one_and_three_threads() {
    let root = fixture_root();
    let mut files = Vec::new();
    walk(&root, &mut files);
    let (one, three) = (converter(1), converter(3));
    let mut pdfs = 0;
    for path in files {
        if path.strip_prefix(&root).unwrap().starts_with("abuse") {
            continue;
        }
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
    config.limits.max_pages = 0;
    assert!(matches!(
        Converter::new(config),
        Err(ConfigError::ZeroLimit { limit: Limit::Pages, .. })
    ));

    let mut config = Config::default();
    config.limits.max_nesting_depth = Limits::MAX_NESTING_CEILING + 1;
    assert!(matches!(
        Converter::new(config),
        Err(ConfigError::AboveCeiling { limit: Limit::NestingDepth, ceiling: 1024, .. })
    ));

    for threads in [0, Config::MAX_THREADS + 1] {
        let mut config = Config::default();
        config.threads = threads;
        assert!(matches!(Converter::new(config), Err(ConfigError::Threads { max: 64, .. })));
    }
}

fn pdf() -> Vec<u8> {
    std::fs::read(fixture_root().join("pdf").join("text.pdf")).unwrap()
}

#[cfg(feature = "pdf")]
#[test]
fn a_pdf_converts_but_has_no_document_model() {
    let bytes = pdf();
    let converter = converter(1);
    assert!(!converter.convert(Input::new(&bytes, Format::Pdf)).unwrap().markdown().is_empty());
    let error = converter.parse(Input::new(&bytes, Format::Pdf)).unwrap_err();
    assert!(
        matches!(error, ConvertError::Unsupported { kind: UnsupportedKind::PdfDocumentModel, .. }),
        "{error:?}"
    );
}

#[cfg(not(feature = "pdf"))]
#[test]
fn a_build_without_pdf_refuses_every_pdf() {
    let bytes = pdf();
    let converter = converter(1);
    let errors = [
        converter.convert(Input::new(&bytes, Format::Pdf)).unwrap_err(),
        converter.parse(Input::new(&bytes, Format::Pdf)).unwrap_err(),
    ];
    for error in errors {
        assert!(
            matches!(
                error,
                ConvertError::Unsupported {
                    kind: UnsupportedKind::FeatureDisabled { feature: "pdf", .. },
                    ..
                }
            ),
            "{error:?}"
        );
    }
    assert_eq!(Format::sniff(&bytes), Some(Format::Pdf));
}

fn limit_mut(limits: &mut Limits, limit: Limit) -> &mut u64 {
    match limit {
        Limit::EntryBytes => &mut limits.max_entry_bytes,
        Limit::TotalBytes => &mut limits.max_total_bytes,
        Limit::EntryCount => &mut limits.max_entry_count,
        Limit::NestingDepth => &mut limits.max_nesting_depth,
        Limit::XmlNodes => &mut limits.max_xml_nodes,
        Limit::GridSlots => &mut limits.max_grid_slots,
        Limit::Expansion => &mut limits.max_expansion,
        Limit::ExpansionTextBytes => &mut limits.max_expansion_text_bytes,
        Limit::AssetTotalBytes => &mut limits.max_asset_total_bytes,
        Limit::RecordDepth => &mut limits.max_record_depth,
        Limit::Records => &mut limits.max_records,
        Limit::InputBytes => &mut limits.max_input_bytes,
        Limit::Work => &mut limits.max_work,
        other => panic!("{other} is not enforced"),
    }
}

fn convert_with(bytes: &[u8], format: Format, limit: Limit, max: u64) -> Result<(), ConvertError> {
    let mut config = Config::default();
    *limit_mut(&mut config.limits, limit) = max;
    Converter::new(config).unwrap().convert(Input::new(bytes, format)).map(drop)
}

fn zip(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in parts {
        writer.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(&mut writer, bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn ods_with_repeated_text() -> Vec<u8> {
    let content = r#"<office:document-content
        xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
        xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
        xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0">
        <office:body><office:spreadsheet><table:table table:name="S">
        <table:table-row table:number-rows-repeated="3">
        <table:table-cell><text:p>abcd</text:p></table:table-cell>
        </table:table-row>
        </table:table></office:spreadsheet></office:body></office:document-content>"#;
    zip(&[("content.xml", content.as_bytes())])
}

fn pptx(sp_tree: &str, extra: &[(&str, &[u8])]) -> Vec<u8> {
    let p = r#"xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main""#;
    let a = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main""#;
    let r = r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
    let presentation = format!(
        r#"<p:presentation {p} {r}><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#
    );
    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#;
    let slide =
        format!(r#"<p:sld {p} {a} {r}><p:cSld><p:spTree>{sp_tree}</p:spTree></p:cSld></p:sld>"#);
    let mut parts: Vec<(&str, &[u8])> = vec![
        ("ppt/presentation.xml", presentation.as_bytes()),
        ("ppt/_rels/presentation.xml.rels", rels.as_bytes()),
        ("ppt/slides/slide1.xml", slide.as_bytes()),
    ];
    parts.extend_from_slice(extra);
    zip(&parts)
}

fn pptx_with_spanning_cell() -> Vec<u8> {
    let cell = |attrs: &str, text: &str| {
        format!(
            r#"<a:tc {attrs}><a:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></a:txBody></a:tc>"#
        )
    };
    let row =
        format!("<a:tr>{}{}</a:tr>", cell(r#"gridSpan="3""#, "wide"), cell(r#"hMerge="1""#, ""));
    pptx(
        &format!(
            r#"<p:graphicFrame><a:graphic><a:graphicData><a:tbl>{row}</a:tbl></a:graphicData></a:graphic></p:graphicFrame>"#
        ),
        &[],
    )
}

fn pptx_with_picture() -> Vec<u8> {
    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/></Relationships>"#;
    pptx(
        r#"<p:pic><p:nvPicPr><p:cNvPr id="2" name="Picture"/></p:nvPicPr><p:blipFill><a:blip r:embed="rId2"/></p:blipFill></p:pic>"#,
        &[
            ("ppt/slides/_rels/slide1.xml.rels", rels.as_bytes()),
            ("ppt/media/image1.png", b"\x89PNG\r\n\x1a\n"),
        ],
    )
}

fn epub_with_spanning_cell() -> Vec<u8> {
    let container = r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
    let opf = r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0"><manifest><item id="c" href="c.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c"/></spine></package>"#;
    let chapter = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><table><tr><td colspan="3">wide</td></tr></table></body></html>"#;
    zip(&[
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", container.as_bytes()),
        ("content.opf", opf.as_bytes()),
        ("c.xhtml", chapter.as_bytes()),
    ])
}

fn docx_nested(depth: usize) -> Vec<u8> {
    let w = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
    let body = "<w:body><w:p><w:r><w:t>deep</w:t></w:r></w:p></w:body>";
    let nest = format!("{}{}", "<x>".repeat(depth), "</x>".repeat(depth));
    let document = format!("<w:document {w}>{body}{nest}</w:document>");
    zip(&[("word/document.xml", document.as_bytes())])
}

fn odt_with_spanning_cell() -> Vec<u8> {
    let content = r#"<office:document-content
        xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
        xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
        xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0">
        <office:body><office:text><table:table table:name="T"><table:table-row>
        <table:table-cell table:number-rows-spanned="3"><text:p>tall</text:p></table:table-cell>
        </table:table-row></table:table></office:text></office:body></office:document-content>"#;
    zip(&[("content.xml", content.as_bytes())])
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
        format!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{}" Target="{target_prefix}{}"/></Relationships>"#,
            self.rel_type, self.part
        )
    }

    fn deep_part(&self, depth: usize) -> String {
        let nest = format!("{}{}", "<x>".repeat(depth), "</x>".repeat(depth));
        format!(r#"<root xmlns="{}">{nest}</root>"#, self.root_ns)
    }
}

fn docx_with_deep(graphic: &Graphic, depth: usize) -> Vec<u8> {
    let w = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
    let r = r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
    let document = format!(
        r#"<w:document {w} {r}><w:body><w:p><w:r><w:drawing><wp:inline xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"><a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:graphicData>{}</a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p></w:body></w:document>"#,
        graphic.reference
    );
    let part = format!("word/{}", graphic.part);
    zip(&[
        ("word/document.xml", document.as_bytes()),
        ("word/_rels/document.xml.rels", graphic.rels("").as_bytes()),
        (&part, graphic.deep_part(depth).as_bytes()),
    ])
}

fn pptx_with_deep(graphic: &Graphic, depth: usize) -> Vec<u8> {
    let part = format!("ppt/{}", graphic.part);
    pptx(
        &format!(
            r#"<p:graphicFrame><a:graphic><a:graphicData>{}</a:graphicData></a:graphic></p:graphicFrame>"#,
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
    let fixture = |path: &str| std::fs::read(fixture_root().join(path)).unwrap();
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
    let defaults = converter(1);
    for (bytes, format, limit, max) in cases {
        let case = format!("{format:?} {limit} {max}");
        assert!(defaults.convert(Input::new(&bytes, format)).is_ok(), "{case}");
        let error = convert_with(&bytes, format, limit, max).expect_err(&case);
        assert!(
            matches!(error, ConvertError::ResourceLimit { limit: l, max: m, .. } if l == limit && m == max),
            "{case}: {error:?}"
        );
    }
}

#[test]
fn a_raised_nesting_depth_admits_deeper_input() {
    let bytes = docx_nested(300);
    let error = converter(1).convert(Input::new(&bytes, Format::Docx)).unwrap_err();
    assert!(
        matches!(error, ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 256, .. }),
        "{error:?}"
    );
    convert_with(&bytes, Format::Docx, Limit::NestingDepth, Limits::MAX_NESTING_CEILING).unwrap();
}

fn with_limit(limit: Limit, max: u64) -> Converter {
    let mut config = Config::default();
    *limit_mut(&mut config.limits, limit) = max;
    Converter::new(config).unwrap()
}

#[test]
fn max_input_bytes_is_checked_before_parse_and_convert_of_every_format() {
    let docx = std::fs::read(fixture_root().join("docx/text.docx")).unwrap();
    let converter = with_limit(Limit::InputBytes, 64);
    for (bytes, format) in [(docx, Format::Docx), (pdf(), Format::Pdf)] {
        let parsed = converter.parse(Input::new(&bytes, format)).map(drop);
        let converted = converter.convert(Input::new(&bytes, format)).map(drop);
        for error in [parsed.unwrap_err(), converted.unwrap_err()] {
            assert!(
                matches!(
                    error,
                    ConvertError::ResourceLimit { limit: Limit::InputBytes, max: 64, .. }
                ),
                "{format:?}: {error:?}"
            );
        }
    }
    let exact = with_limit(Limit::InputBytes, 7);
    assert!(exact.convert(Input::new(b"a,b\n1,2", Format::Csv)).is_ok());
    assert!(exact.convert(Input::new(b"a,b\n1,2\n", Format::Csv)).is_err());
}

#[test]
fn a_pre_cancelled_token_cancels_every_format() {
    let token = CancelToken::new();
    token.cancel();
    let converter = converter(1);
    let mut files = Vec::new();
    walk(&fixture_root(), &mut files);
    for path in files.iter().filter(|path| !path.to_string_lossy().contains("abuse")) {
        let bytes = std::fs::read(path).unwrap();
        let format = format_of(path, &bytes).unwrap();
        let input = Input::new(&bytes, format).with_cancel(token.clone());
        let parsed = converter.parse(input.clone()).map(drop);
        let converted = converter.convert(input).map(drop);
        for result in [parsed, converted] {
            assert!(matches!(result, Err(ConvertError::Cancelled)), "{}", path.display());
        }
    }
}

#[test]
fn an_uncancelled_token_changes_nothing() {
    let bytes = std::fs::read(fixture_root().join("docx/text.docx")).unwrap();
    let converter = converter(1);
    let plain = converter.convert(Input::new(&bytes, Format::Docx)).unwrap();
    let held = Input::new(&bytes, Format::Docx).with_cancel(CancelToken::new());
    assert_eq!(converter.convert(held).unwrap(), plain);
}
