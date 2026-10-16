//! Container facts come from the ZIP central directory before any entry is
//! opened: duplicate names, encryption flags and the entry count; an
//! encrypted OOXML file, which is a compound file, is still `Encrypted`.
//! XML never converts short: markup a repair would misread fails, a repaired
//! required part fails, a slide listed or a chart drawn many times parses
//! once, and every drawing reports the part's loss and damage in its unit.
//! An EPUB spine item that names no chapter is reported, and a spine that
//! names none fails. Content its label contradicts fails, naming the format
//! detected.

use std::io::{Cursor, Read, Write};
use std::path::Path;

use almiladoc::{
    Config, Conversion, ConvertError, Converter, DiagnosticCode, Format, Input, Limit, OnLoss,
    Region, UnsupportedKind,
};
use zip::write::SimpleFileOptions;

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

fn archive(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in parts {
        writer.start_file(*name, SimpleFileOptions::default()).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn root_rels(target: &str) -> String {
    format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{target}"/></Relationships>"#
    )
}

fn document(text: &str) -> String {
    format!(
        r#"<w:document xmlns:w="{W}"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"#
    )
}

fn docx(rels_target: &str, extra: &[(&str, &[u8])]) -> Vec<u8> {
    let rels = root_rels(rels_target);
    let body = document("main text");
    let mut parts: Vec<(&str, &[u8])> =
        vec![("_rels/.rels", rels.as_bytes()), ("word/document.xml", body.as_bytes())];
    parts.extend_from_slice(extra);
    archive(&parts)
}

fn odt(content_name: &str, extra: &[(&str, &[u8])]) -> Vec<u8> {
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text><text:p>odt text</text:p></office:text></office:body></office:document-content>"#;
    let mut parts: Vec<(&str, &[u8])> = vec![
        ("mimetype", b"application/vnd.oasis.opendocument.text"),
        (content_name, content.as_bytes()),
    ];
    parts.extend_from_slice(extra);
    archive(&parts)
}

fn rename_every(data: &mut [u8], from: &[u8], to: &[u8]) {
    assert_eq!(from.len(), to.len());
    let at: Vec<usize> =
        data.windows(from.len()).enumerate().filter(|(_, w)| *w == from).map(|(i, _)| i).collect();
    assert_eq!(at.len(), 2, "a local and a central header");
    for i in at {
        data[i..i + to.len()].copy_from_slice(to);
    }
}

fn set_flags(data: &mut [u8], name: &str, flags: u16) {
    let mut patched = 0;
    for (signature, name_at, flags_at) in [(b"PK\x03\x04", 30, 6), (b"PK\x01\x02", 46, 8)] {
        for at in 0..data.len().saturating_sub(name_at) {
            if data[at..].starts_with(signature)
                && data[at + name_at..].starts_with(name.as_bytes())
            {
                data[at + flags_at..at + flags_at + 2].copy_from_slice(&flags.to_le_bytes());
                patched += 1;
            }
        }
    }
    assert_eq!(patched, 2, "a local and a central header of {name}");
}

fn convert(bytes: &[u8], format: Format) -> Result<String, ConvertError> {
    Converter::new(Config::default())
        .unwrap()
        .convert(Input::new(bytes, format))
        .map(|c| c.markdown().to_string())
}

fn malformed(result: Result<String, ConvertError>, reason: &str) {
    let err = result.unwrap_err();
    assert!(matches!(&err, ConvertError::Malformed { reason: r, .. } if *r == reason), "{err:?}");
}

#[test]
fn exact_duplicate_names_are_malformed() {
    let mut data =
        docx("word/document.xml", &[("word/documenX.xml", document("second").as_bytes())]);
    rename_every(&mut data, b"word/documenX.xml", b"word/document.xml");
    malformed(convert(&data, Format::Docx), "duplicate entry names");
    let mut data = odt("content.xml", &[("contenX.xml", b"<x/>")]);
    rename_every(&mut data, b"contenX.xml", b"content.xml");
    malformed(convert(&data, Format::Odt), "duplicate entry names");
}

#[test]
fn opc_names_equal_after_ascii_case_folding_are_malformed() {
    let data = docx("word/document.xml", &[("word/Document.xml", document("second").as_bytes())]);
    malformed(convert(&data, Format::Docx), "entry names equal after case folding");
    let data = odt("content.xml", &[("Content.xml", b"<x/>")]);
    assert!(convert(&data, Format::Odt).unwrap().contains("odt text"));
}

#[test]
fn opc_part_names_compare_after_ascii_case_folding() {
    let data = docx("word/Document.xml", &[]);
    assert!(convert(&data, Format::Docx).unwrap().contains("main text"));
    let data = odt("Content.xml", &[]);
    let err = convert(&data, Format::Odt).unwrap_err();
    assert!(matches!(err, ConvertError::MissingPart { .. }), "{err:?}");
}

#[test]
fn an_encryption_flag_on_any_entry_is_encrypted() {
    let rels =
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>"#;
    let comments = format!(r#"<w:comments xmlns:w="{W}"/>"#);
    let data = docx(
        "word/document.xml",
        &[
            ("word/_rels/document.xml.rels", rels.as_bytes()),
            ("word/comments.xml", comments.as_bytes()),
        ],
    );
    assert!(convert(&data, Format::Docx).is_ok());
    for name in ["word/_rels/document.xml.rels", "word/comments.xml"] {
        for flag in [1u16, 1 << 6] {
            let mut patched = data.clone();
            set_flags(&mut patched, name, flag);
            let err = convert(&patched, Format::Docx).unwrap_err();
            assert!(matches!(err, ConvertError::Encrypted), "{name} {flag}: {err:?}");
        }
    }
}

#[test]
fn a_limit_tripped_after_an_ole_prefix_is_not_taken_for_an_ole_file() {
    let mut config = Config::default();
    config.limits.max_entry_count = 2;
    let converter = Converter::new(config).unwrap();
    let data = [OLE_MAGIC.as_slice(), &docx("word/document.xml", &[("a.xml", b"<a/>")])].concat();
    for format in [Format::Docx, Format::Pptx] {
        let err = converter.convert(Input::new(&data, format)).unwrap_err();
        assert!(
            matches!(err, ConvertError::ResourceLimit { limit: Limit::EntryCount, max: 2, .. }),
            "{format:?}: {err:?}"
        );
    }
}

#[test]
fn an_encrypted_ooxml_compound_file_is_encrypted() {
    let mut file = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    for stream in ["EncryptionInfo", "EncryptedPackage"] {
        file.create_stream(stream).unwrap().write_all(b"\x04\x00\x04\x00").unwrap();
    }
    file.flush().unwrap();
    let data = file.into_inner().into_inner();
    for format in [Format::Docx, Format::Pptx] {
        let err = convert(&data, format).unwrap_err();
        assert!(matches!(err, ConvertError::Encrypted), "{format:?}: {err:?}");
    }
}

const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const CHART: &str = "http://schemas.openxmlformats.org/drawingml/2006/chart";

fn relationships(rels: &[(&str, &str, &str)]) -> String {
    let rels: String = rels
        .iter()
        .map(|(id, kind, target)| {
            format!(r#"<Relationship Id="{id}" Type="{REL}/{kind}" Target="{target}"/>"#)
        })
        .collect();
    format!(r#"<Relationships xmlns="{PKG_RELS}">{rels}</Relationships>"#)
}

/// A presentation whose list names `listed` slides, each by its own
/// relationship to the one slide part.
fn pptx(listed: usize, slide_text: &str) -> Vec<u8> {
    let text =
        format!("<p:sp><p:txBody><a:p><a:r><a:t>{slide_text}</a:t></a:r></a:p></p:txBody></p:sp>");
    presentation(&vec!["slides/slide1.xml"; listed], &text, &[])
}

/// A presentation listing one slide per entry of `slides`, a target beside
/// the presentation part; every slide part holds `shapes`, and `parts` sit
/// beside them.
fn presentation(slides: &[&str], shapes: &str, parts: &[(&str, &[u8])]) -> Vec<u8> {
    let ids: String = (0..slides.len())
        .map(|i| format!(r#"<p:sldId id="{}" r:id="rId{i}"/>"#, 256 + i))
        .collect();
    let presentation = format!(
        r#"<p:presentation xmlns:p="{P}" xmlns:r="{REL}"><p:sldIdLst>{ids}</p:sldIdLst></p:presentation>"#
    );
    let ids: Vec<String> = (0..slides.len()).map(|i| format!("rId{i}")).collect();
    let targets: Vec<(&str, &str, &str)> =
        ids.iter().zip(slides).map(|(id, target)| (id.as_str(), "slide", *target)).collect();
    let slide = format!(
        r#"<p:sld xmlns:p="{P}" xmlns:a="{A}" xmlns:r="{REL}"><p:cSld><p:spTree>{shapes}</p:spTree></p:cSld><p:extLst>{}</p:extLst></p:sld>"#,
        "<p:ext/>".repeat(100)
    );
    let mut names: Vec<String> = slides.iter().map(|target| format!("ppt/{target}")).collect();
    names.sort();
    names.dedup();
    let mut all: Vec<(&str, &[u8])> = vec![("ppt/presentation.xml", presentation.as_bytes())];
    all.extend(names.iter().map(|name| (name.as_str(), slide.as_bytes())));
    let root = root_rels("ppt/presentation.xml");
    let rels = relationships(&targets);
    all.extend([
        ("_rels/.rels", root.as_bytes()),
        ("ppt/_rels/presentation.xml.rels", rels.as_bytes()),
    ]);
    all.extend_from_slice(parts);
    archive(&all)
}

fn book(manifest: &str, spine: &str, chapters: &[(&str, &str)]) -> Vec<u8> {
    let container = r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#;
    let opf = format!(
        r#"<package xmlns="http://www.idpf.org/2007/opf"><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
    );
    let names: Vec<String> = chapters.iter().map(|(name, _)| format!("OEBPS/{name}")).collect();
    let mut parts: Vec<(&str, &[u8])> = vec![
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", container.as_bytes()),
        ("OEBPS/content.opf", opf.as_bytes()),
    ];
    parts.extend(
        names.iter().zip(chapters).map(|(name, (_, body))| (name.as_str(), body.as_bytes())),
    );
    archive(&parts)
}

const CHAPTER_ITEM: &str = r#"<item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/>"#;

fn epub(chapter: &str) -> Vec<u8> {
    book(CHAPTER_ITEM, r#"<itemref idref="c1"/>"#, &[("c1.xhtml", chapter)])
}

fn xhtml(doctype: &str, body: &str) -> String {
    format!(r#"{doctype}<html xmlns="http://www.w3.org/1999/xhtml"><body>{body}</body></html>"#)
}

fn reporting(bytes: &[u8], format: Format) -> (String, Vec<(DiagnosticCode, Region)>) {
    let mut config = Config::default();
    config.on_loss = OnLoss::Report;
    let conversion = Converter::new(config).unwrap().convert(Input::new(bytes, format)).unwrap();
    let reported = conversion.diagnostics().iter().map(|d| (d.code, d.region)).collect();
    (conversion.markdown().to_string(), reported)
}

fn malformed_part(result: Result<String, ConvertError>, part: &str, reason: &str) {
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ConvertError::Malformed { part: Some(p), reason: r, .. }
            if p.as_str() == part && *r == reason),
        "{err:?}"
    );
}

#[test]
fn a_bare_less_than_never_converts_short() {
    let data = archive(&[("word/document.xml", document("Pump A < pump B").as_bytes())]);
    malformed_part(convert(&data, Format::Docx), "word/document.xml", "bare `<` in markup");
    let data = pptx(1, "Pump A < pump B");
    let err = convert(&data, Format::Pptx).unwrap_err();
    assert!(
        matches!(
            err,
            ConvertError::Malformed { reason: "no slide in the presentation could be read", .. }
        ),
        "{err:?}"
    );
    let data = epub(&xhtml("", "<p>Pump A < pump B</p>"));
    let err = convert(&data, Format::Epub).unwrap_err();
    assert!(
        matches!(
            err,
            ConvertError::Malformed { reason: "no chapter in the book could be read", .. }
        ),
        "{err:?}"
    );
}

fn with_part_appended(bytes: &[u8], name: &str, tail: &[u8]) -> Vec<u8> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).unwrap();
        let entry = file.name().unwrap().into_owned();
        let mut content = Vec::new();
        file.read_to_end(&mut content).unwrap();
        if entry == name {
            content.extend_from_slice(tail);
        }
        writer.start_file(entry.as_str(), SimpleFileOptions::default()).unwrap();
        writer.write_all(&content).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn a_required_part_that_needs_repair_is_malformed() {
    for (fixture, format, part) in [
        ("docx/text.docx", Format::Docx, "word/document.xml"),
        ("pptx/pres.pptx", Format::Pptx, "ppt/presentation.xml"),
        ("odt/text.odt", Format::Odt, "content.xml"),
        ("epub/book.epub", Format::Epub, "META-INF/container.xml"),
        ("epub/book.epub", Format::Epub, "EPUB/content.opf"),
        ("xlsx/sheet.xlsx", Format::Excel, "xl/workbook.xml"),
    ] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture);
        let original = std::fs::read(path).unwrap();
        assert!(convert(&original, format).is_ok(), "{fixture}");
        let data = with_part_appended(&original, part, b"<unclosed>");
        malformed_part(convert(&data, format), part, "ill-formed xml");
    }
}

#[test]
fn an_end_tag_that_does_not_repeat_its_start_tag_fails_a_required_part() {
    let body = format!(
        r#"<w:document xmlns:w="{W}" xmlns:x="{W}"><w:body><w:p><w:r><w:t>a</x:t></w:r></w:p></w:body></w:document>"#
    );
    let data = archive(&[("word/document.xml", body.as_bytes())]);
    malformed_part(convert(&data, Format::Docx), "word/document.xml", "ill-formed xml");
}

#[test]
fn named_references_resolve_only_where_the_format_declares_them() {
    let data = archive(&[("word/document.xml", document("a&nbsp;b &amp; Tom & Jerry").as_bytes())]);
    assert!(matches!(convert(&data, Format::Docx), Err(ConvertError::Incomplete { .. })));
    let (markdown, reported) = reporting(&data, Format::Docx);
    assert!(markdown.contains("nbsp;b & Tom & Jerry"), "{markdown}");
    assert_eq!(reported, [(DiagnosticCode::UnresolvedEntity, Region::Body)]);
    let data = epub(&xhtml("<!DOCTYPE html>", "<p>a&nbsp;b &eacute; Tom & Jerry</p>"));
    assert_eq!(convert(&data, Format::Epub).unwrap(), "a b \u{e9} Tom & Jerry\n");
}

#[test]
fn an_internal_subset_fails_ooxml_and_is_ignored_in_epub() {
    let subset = r#"<!DOCTYPE d [<!ENTITY e "expanded">]>"#;
    let main = format!("{subset}{}", document("&e;"));
    let data = archive(&[("word/document.xml", main.as_bytes())]);
    malformed_part(
        convert(&data, Format::Docx),
        "word/document.xml",
        "document type with an internal subset",
    );
    let data = epub(&xhtml(subset, "<p>&eacute;&e;</p>"));
    let (markdown, reported) = reporting(&data, Format::Epub);
    assert!(markdown.starts_with("\u{e9}&amp;e;"), "{markdown}");
    assert_eq!(reported, [(DiagnosticCode::UnresolvedEntity, Region::Body)]);
}

fn reporting_converter() -> Converter {
    let mut config = Config::default();
    config.on_loss = OnLoss::Report;
    Converter::new(config).unwrap()
}

#[test]
fn every_spine_item_naming_no_chapter_is_reported_outside_any_unit() {
    let chapter = xhtml("", "<p>Chapter one</p>");
    let chapters = [("c1.xhtml", chapter.as_str())];
    let dangling = [
        ("no idref", "", "<itemref/>"),
        ("idref absent from the manifest", "", r#"<itemref idref="gone"/>"#),
        (
            "manifest item without href",
            r#"<item id="bare" media-type="application/xhtml+xml"/>"#,
            r#"<itemref idref="bare"/>"#,
        ),
    ];
    let every_item: String = dangling.iter().map(|(_, item, _)| *item).collect();
    let every_ref: String = dangling.iter().map(|(_, _, itemref)| *itemref).collect();
    let mut cases: Vec<(&str, String, String, u64)> = dangling
        .iter()
        .map(|(name, item, itemref)| (*name, (*item).to_string(), (*itemref).to_string(), 1))
        .collect();
    cases.push(("all three", every_item.clone(), every_ref.clone(), 3));
    for (name, item, itemref, count) in cases {
        let data = book(
            &format!("{CHAPTER_ITEM}{item}"),
            &format!(r#"{itemref}<itemref idref="c1"/>"#),
            &chapters,
        );
        assert!(
            matches!(convert(&data, Format::Epub), Err(ConvertError::Incomplete { .. })),
            "{name}"
        );
        let converted = reporting_converter().convert(Input::new(&data, Format::Epub)).unwrap();
        assert_eq!(converted.markdown(), "Chapter one\n", "{name}");
        assert_eq!(
            reported_as(&converted, DiagnosticCode::UnitUnreadable),
            [(Region::Body, None, count)],
            "{name}"
        );
    }
    let data = book(&format!("{CHAPTER_ITEM}{every_item}"), &every_ref, &chapters);
    let err = reporting_converter().convert(Input::new(&data, Format::Epub)).unwrap_err();
    assert!(
        matches!(
            err,
            ConvertError::Malformed { reason: "no chapter in the book could be read", .. }
        ),
        "{err:?}"
    );
    let data = book(CHAPTER_ITEM, "", &chapters);
    assert_eq!(convert(&data, Format::Epub).unwrap(), "");
}

#[test]
fn a_title_page_with_a_bare_ampersand_renders_before_the_first_chapter() {
    let title = xhtml("", "<h1>Pride & Prejudice</h1>");
    let chapter = xhtml("", "<p>Chapter one</p>");
    let data = book(
        &format!(
            r#"<item id="title" href="title.xhtml" media-type="application/xhtml+xml"/>{CHAPTER_ITEM}"#
        ),
        r#"<itemref idref="title"/><itemref idref="c1"/>"#,
        &[("c1.xhtml", &chapter), ("title.xhtml", &title)],
    );
    assert_eq!(convert(&data, Format::Epub).unwrap(), "# Pride & Prejudice\n\nChapter one\n");
}

fn fuel_limited(max_work: u64) -> Converter {
    let mut config = Config::default();
    config.limits.max_work = max_work;
    Converter::new(config).unwrap()
}

#[test]
fn a_slide_listed_many_times_converts_once() {
    let data = pptx(10_000, "only slide");
    let converted = fuel_limited(150_000).convert(Input::new(&data, Format::Pptx)).unwrap();
    assert_eq!(converted.markdown().matches("only slide").count(), 1);
    assert!(converted.is_complete());
}

/// A chart, padded with `padding` extension nodes, whose second value has
/// no category, so every reading of it loses that value.
fn lossy_chart(padding: usize) -> String {
    let series = r#"<c:ser><c:cat><c:strCache><c:pt idx="0"><c:v>Q1</c:v></c:pt></c:strCache></c:cat><c:val><c:numCache><c:pt idx="0"><c:v>1</c:v></c:pt><c:pt idx="1"><c:v>2</c:v></c:pt></c:numCache></c:val></c:ser>"#;
    format!(
        r#"<c:chartSpace xmlns:c="{CHART}" xmlns:a="{A}"><c:chart><c:title><c:tx><c:rich><a:p><a:r><a:t>Sales</a:t></a:r></a:p></c:rich></c:tx></c:title><c:plotArea><c:barChart>{series}</c:barChart></c:plotArea></c:chart><c:extLst>{}</c:extLst></c:chartSpace>"#,
        "<c:ext/>".repeat(padding)
    )
}

/// The `drawn`th chart reference of a drawing, alternating between two
/// relationships whose targets differ only in ASCII case.
fn chart_reference(drawn: usize) -> String {
    format!(
        r#"<a:graphic xmlns:a="{A}"><a:graphicData><c:chart xmlns:c="{CHART}" r:id="rId{}"/></a:graphicData></a:graphic>"#,
        1 + drawn % 2
    )
}

/// A docx and a pptx that draw `part` as a chart `drawn` times.
fn charts_drawn(part: &str, drawn: usize) -> [(Vec<u8>, Format); 2] {
    let paragraphs: String = (0..drawn)
        .map(|i| format!("<w:p><w:r><w:drawing>{}</w:drawing></w:r></w:p>", chart_reference(i)))
        .collect();
    let main = format!(
        r#"<w:document xmlns:w="{W}" xmlns:r="{REL}"><w:body>{paragraphs}</w:body></w:document>"#
    );
    let rels = relationships(&[
        ("rId1", "chart", "charts/chart1.xml"),
        ("rId2", "chart", "Charts/Chart1.xml"),
    ]);
    let docx = archive(&[
        ("word/document.xml", main.as_bytes()),
        ("word/_rels/document.xml.rels", rels.as_bytes()),
        ("word/charts/chart1.xml", part.as_bytes()),
    ]);
    let frames: String = (0..drawn)
        .map(|i| format!("<p:graphicFrame>{}</p:graphicFrame>", chart_reference(i)))
        .collect();
    let rels = relationships(&[
        ("rId1", "chart", "../charts/chart1.xml"),
        ("rId2", "chart", "../Charts/Chart1.xml"),
    ]);
    let pptx = presentation(
        &["slides/slide1.xml"],
        &frames,
        &[
            ("ppt/slides/_rels/slide1.xml.rels", rels.as_bytes()),
            ("ppt/charts/chart1.xml", part.as_bytes()),
        ],
    );
    [(docx, Format::Docx), (pptx, Format::Pptx)]
}

fn chart_converter(max_work: u64) -> Converter {
    let mut config = Config::default();
    config.on_loss = OnLoss::Report;
    config.limits.max_work = max_work;
    Converter::new(config).unwrap()
}

fn reported_as(conversion: &Conversion, code: DiagnosticCode) -> Vec<(Region, Option<u32>, u64)> {
    conversion
        .diagnostics()
        .iter()
        .filter(|d| d.code == code)
        .map(|d| (d.region, d.unit.map(std::num::NonZeroU32::get), d.count))
        .collect()
}

#[test]
fn a_chart_drawn_many_times_parses_once_and_reports_each_drawing() {
    let converter = chart_converter(1_000_000);
    for ((data, format), unit) in charts_drawn(&lossy_chart(1), 10_000).iter().zip([None, Some(1)])
    {
        let converted = converter.convert(Input::new(data, *format)).unwrap();
        assert_eq!(converted.markdown().matches("**Sales**").count(), 10_000, "{format:?}");
        assert_eq!(
            reported_as(&converted, DiagnosticCode::GraphicUnread),
            [(Region::Body, unit, 10_000)],
            "{format:?}"
        );
    }
}

#[test]
fn each_drawing_of_a_read_chart_charges_its_nodes() {
    let converter = chart_converter(1_000_000);
    for (data, format) in charts_drawn(&lossy_chart(1_000), 2_000) {
        let err = converter.convert(Input::new(&data, format)).unwrap_err();
        assert!(
            matches!(err, ConvertError::ResourceLimit { limit: Limit::Work, .. }),
            "{format:?}: {err:?}"
        );
    }
}

#[test]
fn each_drawing_of_a_part_rooted_elsewhere_charges_its_nodes() {
    let part = "<x/>".repeat(1_000);
    for (data, format) in charts_drawn(&part, 400) {
        let converted = chart_converter(1_000_000).convert(Input::new(&data, format)).unwrap();
        let unread: u64 =
            reported_as(&converted, DiagnosticCode::GraphicUnread).iter().map(|d| d.2).sum();
        assert_eq!(unread, 400, "{format:?}");
    }
    let out_of_fuel: Vec<_> = charts_drawn(&part, 2_000)
        .iter()
        .map(|(data, format)| {
            let converted = chart_converter(1_000_000).convert(Input::new(data, *format));
            let err = converted.err();
            (*format, matches!(err, Some(ConvertError::ResourceLimit { limit: Limit::Work, .. })))
        })
        .collect();
    assert_eq!(out_of_fuel, [(Format::Docx, true), (Format::Pptx, true)]);
}

#[test]
fn a_lossy_chart_on_two_slides_is_reported_in_each_slide() {
    let rels = relationships(&[("rId1", "chart", "../charts/chart1.xml")]);
    let chart = lossy_chart(1);
    let data = presentation(
        &["slides/slide1.xml", "slides/slide2.xml"],
        &format!("<p:graphicFrame>{}</p:graphicFrame>", chart_reference(0)),
        &[
            ("ppt/slides/_rels/slide1.xml.rels", rels.as_bytes()),
            ("ppt/slides/_rels/slide2.xml.rels", rels.as_bytes()),
            ("ppt/charts/chart1.xml", chart.as_bytes()),
        ],
    );
    let converted = chart_converter(1_000_000).convert(Input::new(&data, Format::Pptx)).unwrap();
    assert_eq!(converted.markdown().matches("**Sales**").count(), 2);
    assert_eq!(
        reported_as(&converted, DiagnosticCode::GraphicUnread),
        [(Region::Body, Some(1), 1), (Region::Body, Some(2), 1)]
    );
}

#[test]
fn a_damaged_chart_on_two_slides_reports_its_damage_in_each_slide() {
    let rels = relationships(&[("rId1", "chart", "../charts/chart1.xml")]);
    let chart = lossy_chart(1)
        .replace("<a:t>Sales</a:t>", "<a:t>Sales &bogus;</a:t>")
        .replace("<c:ext/>", "<c:ext><c:x>");
    let data = presentation(
        &["slides/slide1.xml", "slides/slide2.xml"],
        &format!("<p:graphicFrame>{}</p:graphicFrame>", chart_reference(0)),
        &[
            ("ppt/slides/_rels/slide1.xml.rels", rels.as_bytes()),
            ("ppt/slides/_rels/slide2.xml.rels", rels.as_bytes()),
            ("ppt/charts/chart1.xml", chart.as_bytes()),
        ],
    );
    let converted = chart_converter(1_000_000).convert(Input::new(&data, Format::Pptx)).unwrap();
    assert_eq!(converted.markdown().matches("**Sales &amp;bogus;**").count(), 2);
    let mut damage: Vec<_> = converted
        .diagnostics()
        .iter()
        .filter(|d| {
            matches!(d.code, DiagnosticCode::RecoveredXml | DiagnosticCode::UnresolvedEntity)
        })
        .map(|d| (d.code.as_str(), d.region, d.unit.map(std::num::NonZeroU32::get), d.count))
        .collect();
    damage.sort();
    assert_eq!(
        damage,
        [
            ("recovered_xml", Region::Body, Some(1), 1),
            ("recovered_xml", Region::Body, Some(2), 1),
            ("unresolved_entity", Region::Body, Some(1), 1),
            ("unresolved_entity", Region::Body, Some(2), 1),
        ]
    );
}

fn detected(result: Result<impl std::fmt::Debug, ConvertError>) -> Format {
    match result {
        Err(ConvertError::Unsupported {
            kind: UnsupportedKind::FormatMismatch { detected, .. },
            ..
        }) => detected,
        other => panic!("expected a format mismatch, got {other:?}"),
    }
}

#[test]
fn content_its_label_contradicts_fails_naming_the_format_detected() {
    let converter = Converter::new(Config::default()).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures");
    let cases = [
        ("rtf/text.rtf", Format::Doc, Format::Rtf),
        ("ods/sheet.ods", Format::Odt, Format::Ods),
        ("docx/text.docx", Format::Pdf, Format::Docx),
    ];
    for (path, label, format) in cases {
        let bytes = std::fs::read(fixtures.join(path)).unwrap();
        assert_eq!(detected(converter.convert(Input::new(&bytes, label))), format, "{path}");
        assert_eq!(detected(converter.parse(Input::new(&bytes, label))), format, "{path}");
        assert!(converter.convert(Input::new(&bytes, format)).is_ok(), "{path}");
    }
}
