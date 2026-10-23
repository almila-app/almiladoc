use crate::build::{docx_with, pptx, relationships};
use crate::support::{
    POLICIES, configured, count, fixture, inlines, reported, reporting, trips, zip_package,
};
use almiladoc::{
    Conversion, ConvertError, Converter, DiagnosticCode, Diagnostics, Format, Input, LossClass,
    OnLoss, Region,
};
use std::assert_matches;
use std::io::{Cursor, Read, Write};

#[derive(Clone, Copy)]
enum Edit {
    Corrupt,
    Unclose,
    Remove,
    Put(&'static str),
    Set(usize, u8),
}

/// The package with each named part corrupted, left unclosed, removed,
/// written anew or patched at one byte.
fn repack(bytes: &[u8], edits: &[(&str, Edit)]) -> Vec<u8> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut parts: Vec<(String, Vec<u8>)> = (0..archive.len())
        .map(|i| {
            let mut file = archive.by_index(i).unwrap();
            let mut content = Vec::new();
            file.read_to_end(&mut content).unwrap();
            (file.name().unwrap().into_owned(), content)
        })
        .collect();
    for &(name, edit) in edits {
        let at = parts.iter().position(|(part, _)| part == name);
        match (edit, at) {
            (Remove, Some(at)) => drop(parts.remove(at)),
            (Put(content), at) => {
                at.map(|at| parts.remove(at));
                parts.push((name.to_string(), content.as_bytes().to_vec()));
            }
            (Unclose, Some(at)) => parts[at].1.extend_from_slice(b"<unclosed>"),
            (Set(offset, byte), Some(at)) => parts[at].1[offset] = byte,
            (Corrupt, Some(_)) => {}
            (_, None) => panic!("no part {name}"),
        }
    }
    for (name, content) in &parts {
        writer.start_file(name.as_str(), stored).unwrap();
        writer.write_all(content).unwrap();
    }
    let mut data = writer.finish().unwrap().into_inner();
    for &(name, edit) in edits {
        if matches!(edit, Corrupt) {
            let start = zip::ZipArchive::new(Cursor::new(data.as_slice()))
                .unwrap()
                .by_name(name)
                .unwrap()
                .data_start()
                .unwrap();
            data[usize::try_from(start).unwrap()] ^= 1;
        }
    }
    data
}

fn converter(on_loss: OnLoss) -> Converter {
    configured(|config| config.on_loss = on_loss)
}

/// What `conversion` reports beyond `baseline`, the same input before an
/// edit: the policy drops a fixture holds stay out of each case. Only
/// increases count; a code the edit lowers or removes is not seen.
fn added(conversion: &Conversion, baseline: &Conversion) -> Vec<Expected> {
    let before = reported(baseline.diagnostics());
    reported(conversion.diagnostics())
        .into_iter()
        .filter_map(|(code, region, unit, count)| {
            let held = before
                .iter()
                .find(|&&(c, r, u, _)| (c, r, u) == (code, region, unit))
                .map_or(0, |&(.., n)| n);
            (count > held).then_some((code, region, unit, count - held))
        })
        .collect()
}

fn graphic_pptx() -> Vec<u8> {
    let frame = |reference: &str| {
        format!(
            "<p:graphicFrame><a:graphic><a:graphicData>{reference}</a:graphicData></a:graphic></p:graphicFrame>"
        )
    };
    let chart = frame(
        r#"<c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rId8"/>"#,
    );
    let diagram = frame(
        r#"<dgm:relIds xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" r:dm="rId9"/>"#,
    );
    let rels = relationships(&[
        ("rId8", "chart", "../charts/chart1.xml"),
        ("rId9", "diagramData", "../diagrams/data1.xml"),
    ]);
    pptx(
        &format!("{chart}{diagram}"),
        &[
            ("ppt/slides/_rels/slide1.xml.rels", rels.as_bytes()),
            ("ppt/charts/chart1.xml", CHART.as_bytes()),
            ("ppt/diagrams/data1.xml", DIAGRAM.as_bytes()),
        ],
    )
}

/// A DOCX whose body draws one chart and one diagram, both empty, so that
/// nothing is lost before an edit.
fn graphic_docx() -> Vec<u8> {
    let drawing = |reference: &str| {
        format!(
            "<w:p><w:r><w:drawing><a:graphic><a:graphicData>{reference}</a:graphicData></a:graphic></w:drawing></w:r></w:p>"
        )
    };
    let chart = drawing(
        r#"<c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rId8"/>"#,
    );
    let diagram = drawing(
        r#"<dgm:relIds xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" r:dm="rId9"/>"#,
    );
    let rels = relationships(&[
        ("rId8", "chart", "charts/chart1.xml"),
        ("rId9", "diagramData", "diagrams/data1.xml"),
    ]);
    docx_with(
        &format!("{chart}{diagram}"),
        &[
            ("word/_rels/document.xml.rels", rels.as_bytes()),
            ("word/charts/chart1.xml", CHART.as_bytes()),
            ("word/diagrams/data1.xml", DIAGRAM.as_bytes()),
        ],
    )
}

const SHEET_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/vmlDrawing" Target="../drawings/vmlDrawing1.vml"/></Relationships>"#;

const NO_PART_TARGET: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="/"/></Relationships>"#;

const FOOTNOTE_RELS: &str =
    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>"#;

/// The visibility of the fixture xlsb's hidden second sheet, in its
/// workbook's `BrtBundleSh` record.
const HIDDEN_SHEET_STATE: usize = 0x35;

const HIDING_STYLES: &str = r#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="Hidden"><w:rPr><w:vanish/></w:rPr></w:style>"#;

const HIDING_ODS_STYLES: &str = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:styles><style:default-style style:family="table"><style:table-properties table:display="false"/></style:default-style></office:styles>"#;

const HIDING_SHEET: &str = ".h { display: none }";

const BOLD_SHEET: &str = "p { font-weight: bold }";

const NOTE_CHART: &str = r#"<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:footnote w:id="2"><w:p><w:r><w:drawing><a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:graphicData><c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rId1"/></a:graphicData></a:graphic></w:drawing></w:r></w:p></w:footnote></w:footnotes>"#;

const NOTE_CHART_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="charts/chart1.xml"/></Relationships>"#;

const NOTE_DIAGRAM: &str = r#"<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:footnote w:id="2"><w:p><w:r><w:drawing><a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:graphicData><dgm:relIds xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" r:dm="rId1"/></a:graphicData></a:graphic></w:drawing></w:r></w:p></w:footnote></w:footnotes>"#;

const NOTE_DIAGRAM_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/diagramData" Target="diagrams/data1.xml"/></Relationships>"#;

const DIAGRAM: &str =
    r#"<dgm:dataModel xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram"/>"#;

const CHART: &str =
    r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"/>"#;

#[derive(Clone, Copy)]
enum Source {
    Fixture(&'static str, Format),
    GraphicPptx,
    GraphicDocx,
}

impl Source {
    fn bytes(self) -> Vec<u8> {
        match self {
            Fixture(path, _) => fixture(path),
            GraphicPptx => graphic_pptx(),
            GraphicDocx => graphic_docx(),
        }
    }

    fn format(self) -> Format {
        match self {
            Fixture(_, format) => format,
            GraphicPptx => Format::Pptx,
            GraphicDocx => Format::Docx,
        }
    }
}

type Expected = (DiagnosticCode, Region, Option<u32>, u64);
type Row = (Source, &'static [(&'static str, Edit)], &'static [Expected]);

const PART: DiagnosticCode = DiagnosticCode::PartUnreadable;
const STYLE: DiagnosticCode = DiagnosticCode::StylePartUnreadable;
const CELL: DiagnosticCode = DiagnosticCode::CellValueUnreadable;
const UNIT: DiagnosticCode = DiagnosticCode::UnitUnreadable;
const CONVENTIONAL: DiagnosticCode = DiagnosticCode::ConventionalPartUsed;
const GRAPHIC: DiagnosticCode = DiagnosticCode::GraphicUnread;
const UNAVAILABLE: DiagnosticCode = DiagnosticCode::ImageUnavailable;
const ENCODING: DiagnosticCode = DiagnosticCode::InvalidStyleEncoding;

const DOCX: Source = Fixture("docx/text.docx", Format::Docx);
const PPTX: Source = Fixture("pptx/pres.pptx", Format::Pptx);
const XLSX: Source = Fixture("xlsx/sheet.xlsx", Format::Excel);
const XLSB: Source = Fixture("xlsb/handmade-sheet.xlsb", Format::Excel);
const EPUB: Source = Fixture("epub/book.epub", Format::Epub);
const ODT: Source = Fixture("odt/text.odt", Format::Odt);
const ODS: Source = Fixture("ods/sheet.ods", Format::Ods);

use Edit::{Corrupt, Put, Remove, Set, Unclose};
use Region::{Body, Container, Note};
use Source::{Fixture, GraphicDocx, GraphicPptx};

const EACH_EPUB_CHAPTER: &[Expected] =
    &[(PART, Body, Some(1), 1), (PART, Body, Some(2), 1), (PART, Body, Some(3), 1)];

/// One case per site that names a part's role: each corrupts, removes or
/// leaves unclosed the part it reads, and expects that role's report in its
/// unit.
const CASES: &[Row] = &[
    (DOCX, &[("_rels/.rels", Corrupt)], &[(CONVENTIONAL, Container, None, 1)]),
    (DOCX, &[("_rels/.rels", Put(NO_PART_TARGET))], &[(CONVENTIONAL, Container, None, 1)]),
    (
        DOCX,
        &[("word/_rels/document.xml.rels", Corrupt)],
        &[(PART, Body, None, 1), (UNAVAILABLE, Body, None, 1)],
    ),
    (DOCX, &[("word/styles.xml", Corrupt)], &[(PART, Body, None, 1)]),
    (
        Fixture("docx/handmade-tables.docx", Format::Docx),
        &[("word/styles.xml", Unclose)],
        &[(STYLE, Container, None, 1)],
    ),
    (
        Fixture("docx/handmade-tables.docx", Format::Docx),
        &[("word/styles.xml", Corrupt)],
        &[(PART, Body, None, 1)],
    ),
    (
        Fixture("docx/handmade-tables.docx", Format::Docx),
        &[("word/styles.xml", Put(HIDING_STYLES))],
        &[(PART, Body, None, 1)],
    ),
    (DOCX, &[("word/numbering.xml", Corrupt)], &[(PART, Body, None, 1)]),
    (DOCX, &[("word/numbering.xml", Remove)], &[(PART, Body, None, 1)]),
    (DOCX, &[("word/footnotes.xml", Corrupt)], &[(PART, Note, None, 1)]),
    (DOCX, &[("word/endnotes.xml", Remove)], &[(PART, Note, None, 1)]),
    (
        DOCX,
        &[
            ("word/_rels/footnotes.xml.rels", Put(FOOTNOTE_RELS)),
            ("word/_rels/footnotes.xml.rels", Corrupt),
        ],
        &[(PART, Note, None, 1)],
    ),
    (GraphicDocx, &[("word/charts/chart1.xml", Corrupt)], &[(GRAPHIC, Body, None, 1)]),
    (GraphicDocx, &[("word/diagrams/data1.xml", Corrupt)], &[(GRAPHIC, Body, None, 1)]),
    (
        DOCX,
        &[
            ("word/footnotes.xml", Put(NOTE_CHART)),
            ("word/_rels/footnotes.xml.rels", Put(NOTE_CHART_RELS)),
            ("word/charts/chart1.xml", Put(CHART)),
            ("word/charts/chart1.xml", Unclose),
        ],
        &[(GRAPHIC, Note, None, 1)],
    ),
    (
        DOCX,
        &[
            ("word/footnotes.xml", Put(NOTE_DIAGRAM)),
            ("word/_rels/footnotes.xml.rels", Put(NOTE_DIAGRAM_RELS)),
            ("word/diagrams/data1.xml", Put(DIAGRAM)),
            ("word/diagrams/data1.xml", Unclose),
        ],
        &[(GRAPHIC, Note, None, 1)],
    ),
    (PPTX, &[("_rels/.rels", Corrupt)], &[(CONVENTIONAL, Container, None, 1)]),
    (PPTX, &[("_rels/.rels", Put(NO_PART_TARGET))], &[(CONVENTIONAL, Container, None, 1)]),
    (PPTX, &[("ppt/slides/_rels/slide1.xml.rels", Corrupt)], &[(PART, Body, Some(1), 1)]),
    (PPTX, &[("ppt/slides/slide1.xml", Corrupt)], &[(UNIT, Body, Some(1), 1)]),
    (PPTX, &[("ppt/notesSlides/notesSlide1.xml", Corrupt)], &[(PART, Note, Some(1), 1)]),
    (PPTX, &[("ppt/notesSlides/notesSlide2.xml", Remove)], &[(PART, Note, Some(2), 1)]),
    (PPTX, &[("ppt/notesSlides/_rels/notesSlide1.xml.rels", Corrupt)], &[(PART, Note, Some(1), 1)]),
    (PPTX, &[("ppt/slideLayouts/slideLayout1.xml", Corrupt)], &[(PART, Body, None, 1)]),
    (PPTX, &[("ppt/slideLayouts/_rels/slideLayout1.xml.rels", Corrupt)], &[(PART, Body, None, 1)]),
    (PPTX, &[("ppt/slideMasters/slideMaster1.xml", Corrupt)], &[(PART, Body, None, 1)]),
    (GraphicPptx, &[("ppt/charts/chart1.xml", Unclose)], &[(GRAPHIC, Body, Some(1), 1)]),
    (GraphicPptx, &[("ppt/diagrams/data1.xml", Unclose)], &[(GRAPHIC, Body, Some(1), 1)]),
    (ODT, &[("styles.xml", Corrupt)], &[(PART, Body, None, 1)]),
    (ODS, &[("styles.xml", Corrupt)], &[(PART, Body, None, 1)]),
    (ODS, &[("styles.xml", Unclose)], &[(STYLE, Container, None, 1)]),
    (ODS, &[("styles.xml", Put(HIDING_ODS_STYLES))], &[(PART, Body, None, 1)]),
    (ODT, &[("META-INF/manifest.xml", Unclose)], &[]),
    (
        Fixture("odt/handmade-math.odt", Format::Odt),
        &[("Object 1/content.xml", Corrupt)],
        &[
            (DiagnosticCode::ImagePlaceholder, Body, None, 1),
            (DiagnosticCode::EmbeddedObject, Body, None, 1),
        ],
    ),
    (
        ODT,
        &[("Pictures/100000010000000100000001C4EE90A9.png", Corrupt)],
        &[(UNAVAILABLE, Body, None, 1)],
    ),
    (EPUB, &[("EPUB/text/ch001.xhtml", Corrupt)], &[(UNIT, Body, Some(2), 1)]),
    (EPUB, &[("EPUB/media/file0.png", Corrupt)], &[(UNAVAILABLE, Body, Some(2), 1)]),
    (EPUB, &[("EPUB/styles/stylesheet1.css", Corrupt)], EACH_EPUB_CHAPTER),
    (
        EPUB,
        &[
            ("EPUB/styles/stylesheet1.css", Put(HIDING_SHEET)),
            ("EPUB/styles/stylesheet1.css", Set(6, 0xFF)),
        ],
        EACH_EPUB_CHAPTER,
    ),
    (
        EPUB,
        &[
            ("EPUB/styles/stylesheet1.css", Put(BOLD_SHEET)),
            ("EPUB/styles/stylesheet1.css", Set(18, 0xFF)),
        ],
        &[(ENCODING, Container, None, 1)],
    ),
    (EPUB, &[("EPUB/styles/stylesheet1.css", Remove)], &[(STYLE, Container, None, 1)]),
    (XLSX, &[("_rels/.rels", Corrupt)], &[(CONVENTIONAL, Container, None, 1)]),
    (XLSX, &[("_rels/.rels", Put(NO_PART_TARGET))], &[(CONVENTIONAL, Container, None, 1)]),
    (XLSX, &[("xl/_rels/workbook.xml.rels", Corrupt)], &[(PART, Body, None, 1)]),
    (
        XLSX,
        &[("xl/sharedStrings.xml", Corrupt)],
        &[(CELL, Body, Some(1), 17), (CELL, Body, Some(2), 2)],
    ),
    (XLSX, &[("xl/worksheets/sheet1.xml", Corrupt)], &[(UNIT, Body, Some(1), 1)]),
    (XLSX, &[("xl/styles.xml", Corrupt)], &[(PART, Body, None, 1)]),
    (XLSX, &[("xl/styles.xml", Remove)], &[(PART, Body, None, 1)]),
    (
        XLSX,
        &[
            ("xl/worksheets/_rels/sheet1.xml.rels", Put(SHEET_RELS)),
            ("xl/worksheets/_rels/sheet1.xml.rels", Corrupt),
        ],
        &[(PART, Body, Some(1), 1)],
    ),
    (
        XLSX,
        &[("xl/worksheets/_rels/sheet1.xml.rels", Put(SHEET_RELS))],
        &[(PART, Body, Some(1), 1)],
    ),
    (XLSB, &[("_rels/.rels", Corrupt)], &[(CONVENTIONAL, Container, None, 1)]),
    (XLSB, &[("_rels/.rels", Put(NO_PART_TARGET))], &[(CONVENTIONAL, Container, None, 1)]),
    (XLSB, &[("xl/_rels/workbook.bin.rels", Corrupt)], &[(PART, Body, None, 1)]),
    (
        XLSB,
        &[("xl/worksheets/_rels/sheet1.bin.rels", Put(SHEET_RELS))],
        &[(PART, Body, Some(1), 1)],
    ),
    (
        XLSB,
        &[("xl/workbook.bin", Set(HIDDEN_SHEET_STATE, 0)), ("xl/worksheets/sheet1.bin", Corrupt)],
        &[(UNIT, Body, Some(1), 1)],
    ),
    (XLSB, &[("xl/styles.bin", Corrupt)], &[(PART, Body, None, 1)]),
    (XLSB, &[("xl/sharedStrings.bin", Corrupt)], &[(CELL, Body, Some(1), 2)]),
];

#[test]
fn every_part_reports_by_the_role_its_reader_names() {
    let (fail, report) = (converter(OnLoss::Fail), converter(OnLoss::Report));
    for (index, &(source, edits, expected)) in CASES.iter().enumerate() {
        let (original, format) = (source.bytes(), source.format());
        let clean = report.convert(Input::new(&original, format)).unwrap();
        match source {
            Fixture(..) => assert!(clean.is_complete(), "case {index} before its edit"),
            GraphicPptx | GraphicDocx => {
                assert_eq!(reported(clean.diagnostics()), [], "case {index} before its edit");
            }
        }
        let bytes = repack(&original, edits);
        let reporting = report.convert(Input::new(&bytes, format));
        let conversion = reporting.unwrap_or_else(|e| panic!("case {index}: {e}"));
        assert_eq!(added(&conversion, &clean), expected, "case {index}");
        let lost = expected.iter().any(|(code, ..)| code.class() == LossClass::Lost);
        assert_eq!(conversion.is_complete(), !lost, "case {index}");
        match fail.convert(Input::new(&bytes, format)) {
            Err(ConvertError::Incomplete { diagnostics, .. }) => {
                assert!(lost, "case {index}");
                assert_eq!(&diagnostics, conversion.diagnostics(), "case {index}");
            }
            Ok(failing) => {
                assert!(!lost, "case {index}");
                assert_eq!(failing, conversion, "case {index}");
            }
            Err(e) => panic!("case {index}: {e}"),
        }
    }
}

#[test]
fn parse_reports_and_render_decides() {
    let bytes = repack(&fixture("docx/text.docx"), &[("word/footnotes.xml", Corrupt)]);
    for on_loss in [OnLoss::Fail, OnLoss::Report] {
        let parsed = converter(on_loss).parse(Input::new(&bytes, Format::Docx)).unwrap();
        assert_eq!(parsed.document().assets.len(), 1);
        let expected = [(PART, Note, None, 1), (DiagnosticCode::ImagePlaceholder, Body, None, 1)];
        let diagnostics: Vec<_> =
            parsed.diagnostics().iter().map(|d| (d.code, d.region, d.unit, d.count)).collect();
        assert_eq!(diagnostics, expected);
        match (on_loss, parsed.render()) {
            (OnLoss::Fail, Err(ConvertError::Incomplete { diagnostics, .. })) => {
                assert_eq!(&diagnostics, parsed.diagnostics());
            }
            (OnLoss::Report, Ok(conversion)) => {
                assert_eq!(conversion.diagnostics(), parsed.diagnostics());
                assert!(!conversion.is_complete());
            }
            (_, rendered) => panic!("{on_loss:?}: {rendered:?}"),
        }
    }
}

#[test]
fn an_incomplete_conversion_names_what_was_lost() {
    let bytes = repack(&fixture("docx/text.docx"), &[("word/footnotes.xml", Corrupt)]);
    let error = converter(OnLoss::Fail).convert(Input::new(&bytes, Format::Docx)).unwrap_err();
    assert_matches!(error, ConvertError::Incomplete { .. });
    assert_eq!(error.to_string(), "incomplete conversion, content lost: part_unreadable");
}

#[test]
fn the_identity_fingerprints_the_loss_policy() {
    let (fail, report) = (converter(OnLoss::Fail), converter(OnLoss::Report));
    assert_eq!(fail.identity().version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        fail.identity().config_fingerprint,
        "64793a7a830129ecbef0c5e222bb22b493b88b9dde158117a8a41b78855b62e5"
    );
    assert_ne!(fail.identity(), report.identity());
    assert_eq!(fail.identity(), configured(|_| {}).identity());
}

#[test]
fn image_policy_is_consistent_across_parse_render_and_convert() {
    use almiladoc::ImagePolicy::Placeholder;
    let source = br"{\rtf1 before{\pict\pngblip 0102}between{\pict\wbitmap 03}after{\footnote note{\pict\pngblip 0405}}}";
    for images in POLICIES {
        for on_loss in [OnLoss::Fail, OnLoss::Report] {
            let converter = configured(|config| {
                config.images = images;
                config.on_loss = on_loss;
            });
            let parsed = converter.parse(Input::new(source, Format::Rtf)).unwrap();
            let converted = converter.convert(Input::new(source, Format::Rtf));
            assert_eq!(converted, parsed.render());
            match converted {
                Ok(converted) => {
                    assert_eq!(converted.diagnostics(), parsed.diagnostics());
                    assert_eq!(converted.is_complete(), images != Placeholder);
                    if images == Placeholder {
                        assert!(
                            converted.markdown().contains("![Image](asset:0)between\\[Image\\]")
                        );
                    } else {
                        assert!(
                            converted
                                .diagnostics()
                                .iter()
                                .all(|d| d.code == DiagnosticCode::ImageOmitted)
                        );
                        assert_eq!(count(converted.diagnostics(), |d| d.region == Body), 2);
                        assert_eq!(count(converted.diagnostics(), |d| d.region == Note), 1);
                    }
                }
                Err(ConvertError::Incomplete { .. }) => {
                    assert_eq!((images, on_loss), (Placeholder, OnLoss::Fail));
                }
                result => panic!("{images:?} {on_loss:?}: {result:?}"),
            }
            assert_eq!(parsed.document().assets.len(), 2);
        }
    }
}

#[test]
fn image_policy_preserves_fatal_resource_and_cancellation_failures() {
    use almiladoc::{CancelToken, Limit};
    for images in POLICIES {
        let converter = configured(|config| {
            config.images = images;
            config.limits.max_asset_total_bytes = 1;
        });
        let source = br"{\rtf1 {\pict\pngblip 0102}}";
        let refusal = converter.convert(Input::new(source, Format::Rtf));
        assert!(trips(&refusal, Limit::AssetTotalBytes, 1), "{images:?}: {refusal:?}");
        let cancel = CancelToken::new();
        cancel.cancel();
        assert_matches!(
            converter.convert(Input::new(source, Format::Rtf).with_cancel(cancel)),
            Err(ConvertError::Cancelled)
        );
    }
}

#[test]
fn only_the_selected_note_definition_reports_an_unread_image() {
    use almiladoc::ImagePolicy::Placeholder;
    let ns = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main""#;
    let good = "<w:p><w:r><w:t>First wins.</w:t></w:r></w:p>";
    let unread = "<w:p><w:r><w:drawing><a:blip/></w:drawing></w:r></w:p>";
    for (first, second, selected_unread) in
        [(good, unread, false), (unread, good, true), ("<w:p/>", unread, false), (unread, "", true)]
    {
        let document = format!(
            r#"<w:document {ns}><w:body><w:p><w:r><w:t>Body</w:t><w:footnoteReference w:id="1"/></w:r></w:p></w:body></w:document>"#
        );
        let mut notes = format!(r#"<w:footnotes {ns}><w:footnote w:id="1">{first}</w:footnote>"#);
        if !second.is_empty() {
            notes.push_str(&format!(r#"<w:footnote w:id="1">{second}</w:footnote>"#));
        }
        notes.push_str("</w:footnotes>");
        let bytes = zip_package(&[("word/document.xml", document), ("word/footnotes.xml", notes)]);
        for images in POLICIES {
            for on_loss in [OnLoss::Fail, OnLoss::Report] {
                let converter = configured(|config| {
                    config.images = images;
                    config.on_loss = on_loss;
                });
                let parsed = converter.parse(Input::new(&bytes, Format::Docx)).unwrap();
                let failures = count(parsed.diagnostics(), |d| d.code == UNAVAILABLE);
                assert_eq!(failures, u64::from(images == Placeholder && selected_unread));
                let converted = converter.convert(Input::new(&bytes, Format::Docx));
                assert_eq!(converted, parsed.render());
                match converted {
                    Ok(converted) => {
                        if images == Placeholder && first == good {
                            assert!(converted.markdown().contains("First wins."));
                            assert!(!converted.markdown().contains("Image"));
                        }
                    }
                    Err(ConvertError::Incomplete { .. }) => {
                        assert_eq!(
                            (images, on_loss, selected_unread),
                            (Placeholder, OnLoss::Fail, true)
                        );
                    }
                    result => panic!("{images:?} {on_loss:?}: {result:?}"),
                }
            }
        }
    }
}

#[test]
fn unread_image_units_survive_diagnostic_folding_in_the_model() {
    use almiladoc::model::{ImageSource, Inline, UnitKind};
    let count = Diagnostics::MAX_UNITS + 1;
    let items: String = (1..=count)
        .map(|n| {
            format!(r#"<item id="i{n}" href="c{n}.xhtml" media-type="application/xhtml+xml"/>"#)
        })
        .collect();
    let refs: String = (1..=count).map(|n| format!(r#"<itemref idref="i{n}"/>"#)).collect();
    let opf = format!(
        r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata/><manifest>{items}</manifest><spine>{refs}</spine></package>"#
    );
    let mut parts = vec![("META-INF/container.xml".to_string(), r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="c.opf"/></rootfiles></container>"#.to_string()), ("c.opf".to_string(), opf)];
    parts.extend((1..=count).map(|n| (format!("c{n}.xhtml"), format!(r#"<html xmlns="http://www.w3.org/1999/xhtml"><head/><body><p>before{n}<img src="missing.png"/>after</p></body></html>"#))));
    let bytes = zip_package(&parts);
    let parsed = reporting().parse(Input::new(&bytes, Format::Epub)).unwrap();
    let units: Vec<_> = inlines(&parsed.document().blocks)
        .into_iter()
        .filter_map(|inline| match inline {
            Inline::Image { source: ImageSource::Unread, origin, .. } => origin.unit,
            _ => None,
        })
        .collect();
    assert_eq!(units.len(), count);
    for (position, unit) in units.iter().enumerate() {
        assert_eq!(unit.kind, UnitKind::SpineItem);
        assert_eq!(unit.ordinal.get() as usize, position + 1);
    }
    let losses: Vec<_> = parsed.diagnostics().iter().filter(|d| d.code == UNAVAILABLE).collect();
    assert_eq!(losses.iter().map(|d| d.count).sum::<u64>(), count as u64);
    assert_eq!(losses.iter().filter(|d| d.unit.is_some()).count(), Diagnostics::MAX_UNITS);
    assert!(losses.iter().any(|d| d.unit.is_none() && d.count == 1));
    let rendered = parsed.render().unwrap();
    assert_eq!(rendered.markdown().matches("\\[Image\\]").count(), count);
    assert_eq!(rendered.diagnostics(), parsed.diagnostics());
}

const UNCLOSED_SLIDE: &str = r#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Unclosed slide words</a:t></a:r></a:p></p:txBody></p:sp>"#;

const SHEET_DAMAGED_AFTER_ROW_1: &str = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Row one words</t></is></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>Row two words</t></is></c></sheetData></worksheet>"#;

#[test]
fn a_damaged_unit_part_is_unreadable_and_none_of_its_text_renders() {
    let report = converter(OnLoss::Report);
    for (path, format, edit, text) in [
        ("pptx/pres.pptx", Format::Pptx, ("ppt/slides/slide1.xml", UNCLOSED_SLIDE), "Unclosed"),
        (
            "xlsx/sheet.xlsx",
            Format::Excel,
            ("xl/worksheets/sheet1.xml", SHEET_DAMAGED_AFTER_ROW_1),
            "Row one",
        ),
    ] {
        let original = fixture(path);
        let clean = report.convert(Input::new(&original, format)).unwrap();
        let bytes = repack(&original, &[(edit.0, Put(edit.1))]);
        let conversion = report.convert(Input::new(&bytes, format)).unwrap();
        assert!(!conversion.markdown().contains(text), "{path}");
        assert_eq!(added(&conversion, &clean), [(UNIT, Body, Some(1), 1)], "{path}");
    }
}

#[test]
fn a_main_part_that_does_not_decode_is_malformed() {
    let document = concat!(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
        "<w:body><w:p><w:r><w:t>a\u{1F600}b</w:t></w:r></w:p></w:body></w:document>",
    );
    let mut units: Vec<u16> = document.encode_utf16().collect();
    let pair = units.iter().position(|&unit| (0xD800..0xDC00).contains(&unit)).unwrap();
    units.remove(pair + 1);
    let mut utf16 = vec![0xFF, 0xFE];
    utf16.extend(units.iter().flat_map(|unit| unit.to_le_bytes()));
    let bytes = zip_package(&[("word/document.xml", utf16)]);
    for on_loss in [OnLoss::Fail, OnLoss::Report] {
        let err = converter(on_loss).convert(Input::new(&bytes, Format::Docx)).unwrap_err();
        assert_matches!(
            &err,
            ConvertError::Malformed { part: Some(part), reason: "bytes that do not decode", .. }
                if part.as_str() == "word/document.xml"
        );
    }
}
