use almiladoc::{
    Config, Conversion, ConvertError, Converter, Diagnostic, DiagnosticCode, Format, Input,
    LossClass, OnLoss, Region,
};
use std::io::{Cursor, Read, Write};
use std::path::Path;

#[derive(Clone, Copy)]
enum Edit {
    Corrupt,
    Recover,
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
            (file.name().to_string(), content)
        })
        .collect();
    for &(name, edit) in edits {
        let at = parts.iter().position(|(part, _)| part == name);
        match (edit, at) {
            (Edit::Remove, Some(at)) => drop(parts.remove(at)),
            (Edit::Put(content), at) => {
                at.map(|at| parts.remove(at));
                parts.push((name.to_string(), content.as_bytes().to_vec()));
            }
            (Edit::Recover, Some(at)) => parts[at].1.extend_from_slice(b"<unclosed>"),
            (Edit::Set(offset, byte), Some(at)) => parts[at].1[offset] = byte,
            (Edit::Corrupt, Some(_)) => {}
            (_, None) => panic!("no part {name}"),
        }
    }
    for (name, content) in &parts {
        writer.start_file(name.as_str(), stored).unwrap();
        writer.write_all(content).unwrap();
    }
    let mut data = writer.finish().unwrap().into_inner();
    for &(name, edit) in edits {
        if matches!(edit, Edit::Corrupt) {
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

fn fixture(path: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(path)).unwrap()
}

fn converter(on_loss: OnLoss) -> Converter {
    let mut config = Config::default();
    config.on_loss = on_loss;
    Converter::new(config).unwrap()
}

fn reported(conversion: &Conversion) -> Vec<(DiagnosticCode, Region, Option<u32>, u64)> {
    let unit = |d: &Diagnostic| d.unit.map(std::num::NonZeroU32::get);
    conversion.diagnostics().iter().map(|d| (d.code, d.region, unit(&d), d.count)).collect()
}

/// What `conversion` reports beyond `baseline`, the same input before an
/// edit: the policy drops a fixture holds stay out of each case. Only
/// increases count; a code the edit lowers or removes is not seen.
fn added(
    conversion: &Conversion,
    baseline: &Conversion,
) -> Vec<(DiagnosticCode, Region, Option<u32>, u64)> {
    let before = reported(baseline);
    reported(conversion)
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

const RELS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

fn graphic_pptx() -> Vec<u8> {
    let p = r#"xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main""#;
    let a = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main""#;
    let r = format!(r#"xmlns:r="{RELS}""#);
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
    let presentation = format!(
        r#"<p:presentation {p} {r}><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#
    );
    let rel = |id: &str, kind: &str, target: &str| {
        format!(r#"<Relationship Id="{id}" Type="{RELS}/{kind}" Target="{target}"/>"#)
    };
    let rels = |body: String| {
        format!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{body}</Relationships>"#
        )
    };
    let slide = format!(
        r#"<p:sld {p} {a} {r}><p:cSld><p:spTree>{chart}{diagram}</p:spTree></p:cSld></p:sld>"#
    );
    let parts = [
        ("ppt/presentation.xml", presentation),
        ("ppt/_rels/presentation.xml.rels", rels(rel("rId1", "slide", "slides/slide1.xml"))),
        ("ppt/slides/slide1.xml", slide),
        (
            "ppt/slides/_rels/slide1.xml.rels",
            rels(rel("rId8", "chart", "../charts/chart1.xml") + &rel("rId9", "diagramData", "../diagrams/data1.xml")),
        ),
        (
            "ppt/charts/chart1.xml",
            r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"/>"#.to_string(),
        ),
        (
            "ppt/diagrams/data1.xml",
            r#"<dgm:dataModel xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram"/>"#.to_string(),
        ),
    ];
    zipped(&parts)
}

fn zipped(parts: &[(&str, String)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, content) in parts {
        writer.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        writer.write_all(content.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// A DOCX whose body draws one chart and one diagram, both empty, so that
/// nothing is lost before an edit.
fn graphic_docx() -> Vec<u8> {
    let drawing = |reference: &str| {
        format!(
            r#"<w:p><w:r><w:drawing><a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:graphicData>{reference}</a:graphicData></a:graphic></w:drawing></w:r></w:p>"#
        )
    };
    let chart = drawing(
        r#"<c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rId8"/>"#,
    );
    let diagram = drawing(
        r#"<dgm:relIds xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" r:dm="rId9"/>"#,
    );
    let document = format!(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="{RELS}"><w:body>{chart}{diagram}</w:body></w:document>"#
    );
    let rels = format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId8" Type="{RELS}/chart" Target="charts/chart1.xml"/><Relationship Id="rId9" Type="{RELS}/diagramData" Target="diagrams/data1.xml"/></Relationships>"#
    );
    zipped(&[
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", rels),
        ("word/charts/chart1.xml", CHART.to_string()),
        ("word/diagrams/data1.xml", DIAGRAM.to_string()),
    ])
}

const SHEET_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/vmlDrawing" Target="../drawings/vmlDrawing1.vml"/></Relationships>"#;

const FOOTNOTE_RELS: &str =
    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>"#;

/// The visibility of the fixture xlsb's hidden second sheet, in its
/// workbook's `BrtBundleSh` record.
const HIDDEN_SHEET_STATE: usize = 0x35;

const NOTE_CHART: &str = r#"<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:footnote w:id="2"><w:p><w:r><w:drawing><a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:graphicData><c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rId1"/></a:graphicData></a:graphic></w:drawing></w:r></w:p></w:footnote></w:footnotes>"#;

const NOTE_CHART_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="charts/chart1.xml"/></Relationships>"#;

const NOTE_DIAGRAM: &str = r#"<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:footnote w:id="2"><w:p><w:r><w:drawing><a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:graphicData><dgm:relIds xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" r:dm="rId1"/></a:graphicData></a:graphic></w:drawing></w:r></w:p></w:footnote></w:footnotes>"#;

const NOTE_DIAGRAM_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/diagramData" Target="diagrams/data1.xml"/></Relationships>"#;

const DIAGRAM: &str =
    r#"<dgm:dataModel xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram"/>"#;

const CHART: &str =
    r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"/>"#;

struct Case {
    source: Source,
    format: Format,
    edits: &'static [(&'static str, Edit)],
    expected: &'static [(DiagnosticCode, Region, Option<u32>, u64)],
}

#[derive(Clone, Copy)]
enum Source {
    Fixture(&'static str),
    GraphicPptx,
    GraphicDocx,
}

const PART: DiagnosticCode = DiagnosticCode::PartUnreadable;
const RECOVERED: DiagnosticCode = DiagnosticCode::RecoveredXml;
const STYLE: DiagnosticCode = DiagnosticCode::StylePartUnreadable;
const CELL: DiagnosticCode = DiagnosticCode::CellValueUnreadable;
const UNIT: DiagnosticCode = DiagnosticCode::UnitUnreadable;
const CONVENTIONAL: DiagnosticCode = DiagnosticCode::ConventionalPartUsed;
const GRAPHIC: DiagnosticCode = DiagnosticCode::GraphicUnread;

const fn case(
    source: Source,
    format: Format,
    edits: &'static [(&'static str, Edit)],
    expected: &'static [(DiagnosticCode, Region, Option<u32>, u64)],
) -> Case {
    Case { source, format, edits, expected }
}

use Edit::{Corrupt, Put, Recover, Remove, Set};
use Region::{Body, Container, Note};
use Source::{Fixture, GraphicDocx, GraphicPptx};

/// One case per site that names a part's role: each corrupts, removes or
/// leaves unclosed the part it reads, and expects that role's report in its
/// unit. A role that parses bytes another read fetched shows only in the
/// recovery case, which `Content` and `Reported` report alike.
const CASES: &[Case] = &[
    case(
        Fixture("docx/text.docx"),
        Format::Docx,
        &[("_rels/.rels", Corrupt)],
        &[(CONVENTIONAL, Container, None, 1)],
    ),
    case(
        Fixture("docx/text.docx"),
        Format::Docx,
        &[("_rels/.rels", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("docx/text.docx"),
        Format::Docx,
        &[("word/_rels/document.xml.rels", Corrupt)],
        &[(PART, Body, None, 1)],
    ),
    case(
        Fixture("docx/text.docx"),
        Format::Docx,
        &[("word/styles.xml", Corrupt)],
        &[(PART, Body, None, 1)],
    ),
    case(
        Fixture("docx/handmade-tables.docx"),
        Format::Docx,
        &[("word/styles.xml", Corrupt)],
        &[(STYLE, Container, None, 1)],
    ),
    case(
        Fixture("docx/text.docx"),
        Format::Docx,
        &[("word/numbering.xml", Corrupt)],
        &[(PART, Body, None, 1)],
    ),
    case(
        Fixture("docx/text.docx"),
        Format::Docx,
        &[("word/numbering.xml", Remove)],
        &[(PART, Body, None, 1)],
    ),
    case(
        Fixture("docx/text.docx"),
        Format::Docx,
        &[("word/document.xml", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("docx/text.docx"),
        Format::Docx,
        &[("word/footnotes.xml", Corrupt)],
        &[(PART, Note, None, 1)],
    ),
    case(
        Fixture("docx/text.docx"),
        Format::Docx,
        &[("word/endnotes.xml", Remove)],
        &[(PART, Note, None, 1)],
    ),
    case(
        Fixture("docx/text.docx"),
        Format::Docx,
        &[
            ("word/_rels/footnotes.xml.rels", Put(FOOTNOTE_RELS)),
            ("word/_rels/footnotes.xml.rels", Corrupt),
        ],
        &[(PART, Note, None, 1)],
    ),
    case(
        GraphicDocx,
        Format::Docx,
        &[("word/charts/chart1.xml", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        GraphicDocx,
        Format::Docx,
        &[("word/diagrams/data1.xml", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        GraphicDocx,
        Format::Docx,
        &[("word/charts/chart1.xml", Corrupt)],
        &[(GRAPHIC, Body, None, 1)],
    ),
    case(
        GraphicDocx,
        Format::Docx,
        &[("word/diagrams/data1.xml", Corrupt)],
        &[(GRAPHIC, Body, None, 1)],
    ),
    case(
        Fixture("docx/text.docx"),
        Format::Docx,
        &[
            ("word/footnotes.xml", Put(NOTE_CHART)),
            ("word/_rels/footnotes.xml.rels", Put(NOTE_CHART_RELS)),
            ("word/charts/chart1.xml", Put(CHART)),
            ("word/charts/chart1.xml", Recover),
        ],
        &[(RECOVERED, Note, None, 1), (DiagnosticCode::DanglingReference, Body, None, 1)],
    ),
    case(
        Fixture("docx/text.docx"),
        Format::Docx,
        &[
            ("word/footnotes.xml", Put(NOTE_DIAGRAM)),
            ("word/_rels/footnotes.xml.rels", Put(NOTE_DIAGRAM_RELS)),
            ("word/diagrams/data1.xml", Put(DIAGRAM)),
            ("word/diagrams/data1.xml", Recover),
        ],
        &[(RECOVERED, Note, None, 1), (DiagnosticCode::DanglingReference, Body, None, 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("_rels/.rels", Corrupt)],
        &[(CONVENTIONAL, Container, None, 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("ppt/slides/slide1.xml", Recover), ("ppt/slides/slide2.xml", Recover)],
        &[(RECOVERED, Body, Some(1), 1), (RECOVERED, Body, Some(2), 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("_rels/.rels", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("ppt/presentation.xml", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("ppt/_rels/presentation.xml.rels", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("ppt/slides/_rels/slide1.xml.rels", Corrupt)],
        &[(PART, Body, Some(1), 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("ppt/slides/slide1.xml", Recover)],
        &[(RECOVERED, Body, Some(1), 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("ppt/slides/slide1.xml", Corrupt)],
        &[(UNIT, Body, Some(1), 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("ppt/notesSlides/notesSlide1.xml", Corrupt)],
        &[(PART, Note, Some(1), 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("ppt/notesSlides/notesSlide2.xml", Remove)],
        &[(PART, Note, Some(2), 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("ppt/notesSlides/_rels/notesSlide1.xml.rels", Corrupt)],
        &[(PART, Note, Some(1), 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("ppt/slideLayouts/slideLayout1.xml", Corrupt)],
        &[(PART, Body, None, 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("ppt/slideLayouts/_rels/slideLayout1.xml.rels", Corrupt)],
        &[(PART, Body, None, 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("ppt/slideMasters/slideMaster1.xml", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("pptx/pres.pptx"),
        Format::Pptx,
        &[("ppt/slideMasters/slideMaster1.xml", Corrupt)],
        &[(PART, Body, None, 1)],
    ),
    case(
        GraphicPptx,
        Format::Pptx,
        &[("ppt/charts/chart1.xml", Recover)],
        &[(RECOVERED, Body, Some(1), 1)],
    ),
    case(
        GraphicPptx,
        Format::Pptx,
        &[("ppt/diagrams/data1.xml", Recover)],
        &[(RECOVERED, Body, Some(1), 1)],
    ),
    case(
        Fixture("odt/text.odt"),
        Format::Odt,
        &[("content.xml", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("odt/text.odt"),
        Format::Odt,
        &[("styles.xml", Corrupt)],
        &[(PART, Body, None, 1)],
    ),
    case(
        Fixture("ods/sheet.ods"),
        Format::Ods,
        &[("styles.xml", Corrupt)],
        &[(STYLE, Container, None, 1)],
    ),
    case(Fixture("odt/text.odt"), Format::Odt, &[("META-INF/manifest.xml", Recover)], &[]),
    case(
        Fixture("odt/handmade-math.odt"),
        Format::Odt,
        &[("Object 1/content.xml", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("odt/handmade-math.odt"),
        Format::Odt,
        &[("Object 1/content.xml", Corrupt)],
        &[(DiagnosticCode::EmbeddedObject, Body, None, 1)],
    ),
    case(
        Fixture("odt/text.odt"),
        Format::Odt,
        &[("Pictures/100000010000000100000001C4EE90A9.png", Corrupt)],
        &[(DiagnosticCode::ImageUnavailable, Body, None, 1)],
    ),
    case(
        Fixture("epub/book.epub"),
        Format::Epub,
        &[("META-INF/container.xml", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("epub/book.epub"),
        Format::Epub,
        &[("EPUB/content.opf", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("epub/book.epub"),
        Format::Epub,
        &[("EPUB/text/ch001.xhtml", Recover)],
        &[(RECOVERED, Body, Some(2), 1)],
    ),
    case(
        Fixture("epub/book.epub"),
        Format::Epub,
        &[("EPUB/text/ch001.xhtml", Corrupt)],
        &[(UNIT, Body, Some(2), 1)],
    ),
    case(
        Fixture("epub/book.epub"),
        Format::Epub,
        &[("EPUB/media/file0.png", Corrupt)],
        &[(DiagnosticCode::ImageUnavailable, Body, Some(2), 1)],
    ),
    case(
        Fixture("epub/book.epub"),
        Format::Epub,
        &[("EPUB/styles/stylesheet1.css", Corrupt)],
        &[(STYLE, Container, None, 1)],
    ),
    case(
        Fixture("epub/book.epub"),
        Format::Epub,
        &[("EPUB/styles/stylesheet1.css", Remove)],
        &[(STYLE, Container, None, 1)],
    ),
    case(
        Fixture("xlsx/sheet.xlsx"),
        Format::Excel,
        &[("_rels/.rels", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(Fixture("xlsx/sheet.xlsx"), Format::Excel, &[("_rels/.rels", Corrupt)], &[]),
    case(
        Fixture("xlsx/sheet.xlsx"),
        Format::Excel,
        &[("xl/_rels/workbook.xml.rels", Corrupt)],
        &[(PART, Body, None, 1)],
    ),
    case(
        Fixture("xlsx/sheet.xlsx"),
        Format::Excel,
        &[("xl/sharedStrings.xml", Corrupt)],
        &[(CELL, Body, Some(1), 17), (CELL, Body, Some(2), 2)],
    ),
    case(
        Fixture("xlsx/sheet.xlsx"),
        Format::Excel,
        &[("xl/worksheets/sheet1.xml", Corrupt)],
        &[(UNIT, Body, Some(1), 1)],
    ),
    case(
        Fixture("xlsx/sheet.xlsx"),
        Format::Excel,
        &[("xl/worksheets/sheet2.xml", Recover)],
        &[(RECOVERED, Body, Some(2), 1)],
    ),
    case(
        Fixture("xlsx/sheet.xlsx"),
        Format::Excel,
        &[("xl/workbook.xml", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("xlsx/sheet.xlsx"),
        Format::Excel,
        &[("xl/_rels/workbook.xml.rels", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("xlsx/sheet.xlsx"),
        Format::Excel,
        &[("xl/sharedStrings.xml", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("xlsx/sheet.xlsx"),
        Format::Excel,
        &[("xl/styles.xml", Corrupt)],
        &[(PART, Body, None, 1)],
    ),
    case(
        Fixture("xlsx/sheet.xlsx"),
        Format::Excel,
        &[("xl/styles.xml", Remove)],
        &[(PART, Body, None, 1)],
    ),
    case(
        Fixture("xlsx/sheet.xlsx"),
        Format::Excel,
        &[("xl/worksheets/sheet1.xml", Recover)],
        &[(RECOVERED, Body, Some(1), 1)],
    ),
    case(
        Fixture("xlsx/sheet.xlsx"),
        Format::Excel,
        &[
            ("xl/worksheets/_rels/sheet1.xml.rels", Put(SHEET_RELS)),
            ("xl/worksheets/_rels/sheet1.xml.rels", Corrupt),
        ],
        &[(PART, Body, Some(1), 1)],
    ),
    case(
        Fixture("xlsx/sheet.xlsx"),
        Format::Excel,
        &[("xl/worksheets/_rels/sheet1.xml.rels", Put(SHEET_RELS))],
        &[(PART, Body, Some(1), 1)],
    ),
    case(
        Fixture("xlsb/handmade-sheet.xlsb"),
        Format::Excel,
        &[("xl/_rels/workbook.bin.rels", Recover)],
        &[(RECOVERED, Body, None, 1)],
    ),
    case(
        Fixture("xlsb/handmade-sheet.xlsb"),
        Format::Excel,
        &[("xl/_rels/workbook.bin.rels", Corrupt)],
        &[(PART, Body, None, 1)],
    ),
    case(
        Fixture("xlsb/handmade-sheet.xlsb"),
        Format::Excel,
        &[("xl/worksheets/_rels/sheet1.bin.rels", Put(SHEET_RELS))],
        &[(PART, Body, Some(1), 1)],
    ),
    case(
        Fixture("xlsb/handmade-sheet.xlsb"),
        Format::Excel,
        &[("xl/workbook.bin", Set(HIDDEN_SHEET_STATE, 0)), ("xl/worksheets/sheet1.bin", Corrupt)],
        &[(UNIT, Body, Some(1), 1)],
    ),
    case(
        Fixture("xlsb/handmade-sheet.xlsb"),
        Format::Excel,
        &[("xl/styles.bin", Corrupt)],
        &[(PART, Body, None, 1)],
    ),
    case(
        Fixture("xlsb/handmade-sheet.xlsb"),
        Format::Excel,
        &[("xl/sharedStrings.bin", Corrupt)],
        &[(CELL, Body, Some(1), 2)],
    ),
];

#[test]
fn every_part_reports_by_the_role_its_reader_names() {
    let (fail, report) = (converter(OnLoss::Fail), converter(OnLoss::Report));
    for (index, case) in CASES.iter().enumerate() {
        let original = match case.source {
            Source::Fixture(path) => fixture(path),
            Source::GraphicPptx => graphic_pptx(),
            Source::GraphicDocx => graphic_docx(),
        };
        let clean = report.convert(Input::new(&original, case.format)).unwrap();
        match case.source {
            Source::Fixture(_) => assert!(clean.is_complete(), "case {index} before its edit"),
            Source::GraphicPptx | Source::GraphicDocx => {
                assert_eq!(reported(&clean), [], "case {index} before its edit")
            }
        }
        let bytes = repack(&original, case.edits);
        let reporting = report.convert(Input::new(&bytes, case.format));
        let conversion = reporting.unwrap_or_else(|e| panic!("case {index}: {e}"));
        assert_eq!(added(&conversion, &clean), case.expected, "case {index}");
        let lost = case.expected.iter().any(|(code, ..)| code.class() == LossClass::Lost);
        assert_eq!(conversion.is_complete(), !lost, "case {index}");
        match fail.convert(Input::new(&bytes, case.format)) {
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
        let expected = [(PART, Note, None, 1)];
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
fn incomplete_has_its_own_code_and_names_what_was_lost() {
    let bytes = repack(&fixture("docx/text.docx"), &[("word/footnotes.xml", Corrupt)]);
    let error = converter(OnLoss::Fail).convert(Input::new(&bytes, Format::Docx)).unwrap_err();
    assert_eq!(error.code(), "incomplete");
    assert_eq!(error.to_string(), "incomplete conversion, content lost: part_unreadable");
}

#[test]
fn the_identity_fingerprints_the_loss_policy() {
    let (fail, report) = (converter(OnLoss::Fail), converter(OnLoss::Report));
    assert_eq!(fail.identity().version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        fail.identity().config_fingerprint,
        "ac094cceb5a939fbceb023e4c1a703508a33fc1720b12af971a4d5972cd50702"
    );
    assert_ne!(fail.identity(), report.identity());
    assert_eq!(fail.identity(), Converter::new(Config::default()).unwrap().identity());
}
