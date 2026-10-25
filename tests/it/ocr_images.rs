//! The caller's OCR filling the gaps a parse leaves after each embedded PNG or JPEG.

use std::num::NonZeroU32;

use crate::build::{W, docx_with, relationships};
use crate::support::configured;
use almiladoc::model::{GapId, OcrOrigin};
use almiladoc::{
    Converter, DiagnosticCode, Format, GapFill, ImageOcr, ImagePolicy, Input, Parsed, Region,
};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
const JPEG: &[u8] = b"\xff\xd8\xff\xe0";
const GIF: &[u8] = b"GIF89a";

fn pictured(text: &str, rel: &str) -> String {
    format!(
        r#"<w:r><w:t>{text}</w:t></w:r><w:r><w:drawing><a:blip r:embed="{rel}"/></w:drawing></w:r>"#
    )
}

/// A paragraph, a list item, a table cell, a gif and a footnote, each with a picture.
fn pictures() -> Vec<u8> {
    let item = |text: &str| {
        format!(r#"<w:p><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr>{text}</w:p>"#)
    };
    let body = [
        format!("<w:p>{}</w:p>", pictured("para", "png")),
        item(&pictured("item", "jpeg")),
        item("<w:r><w:t>next</w:t></w:r>"),
        format!("<w:tbl><w:tr><w:tc><w:p>{}</w:p></w:tc></w:tr></w:tbl>", pictured("x", "png")),
        NOTE_REFERENCE.to_owned(),
        format!("<w:p>{}</w:p>", pictured("gif", "gif")),
    ]
    .concat();
    package(&body, &format!("<w:p>{}</w:p>", pictured("foot", "png")))
}

const NOTE_REFERENCE: &str =
    r#"<w:p><w:r><w:t>note</w:t></w:r><w:r><w:footnoteReference w:id="2"/></w:r></w:p>"#;

/// A document of `body` whose footnote 2 holds `note`, with its pictures and list numbering.
fn package(body: &str, note: &str) -> Vec<u8> {
    let footnotes =
        format!(r#"<w:footnotes {W}><w:footnote w:id="2">{note}</w:footnote></w:footnotes>"#);
    let numbering = format!(
        r#"<w:numbering {W}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
    );
    let rels = relationships(&[
        ("png", "image", "media/a.png"),
        ("jpeg", "image", "media/b.jpeg"),
        ("gif", "image", "media/c.gif"),
        ("notes", "footnotes", "footnotes.xml"),
        ("numbers", "numbering", "numbering.xml"),
    ]);
    docx_with(
        body,
        &[
            ("word/_rels/document.xml.rels", rels.as_bytes()),
            (
                "word/_rels/footnotes.xml.rels",
                relationships(&[("png", "image", "media/a.png")]).as_bytes(),
            ),
            ("word/footnotes.xml", footnotes.as_bytes()),
            ("word/numbering.xml", numbering.as_bytes()),
            ("word/media/a.png", PNG),
            ("word/media/b.jpeg", JPEG),
            ("word/media/c.gif", GIF),
        ],
    )
}

fn table(cells: &[&str]) -> String {
    let cells: String = cells.iter().map(|cell| format!("<w:tc>{cell}</w:tc>")).collect();
    format!("<w:tbl><w:tr>{cells}</w:tr></w:tbl>")
}

fn parsed(set: impl FnOnce(&mut almiladoc::Config)) -> Parsed {
    let converter = configured(|config| {
        config.image_ocr = ImageOcr::All;
        set(config);
    });
    converter.parse(Input::new(&pictures(), Format::Docx)).unwrap()
}

fn ids(parsed: &Parsed) -> Vec<GapId> {
    parsed.gaps().map(|gap| gap.id).collect()
}

fn fill_all(parsed: &mut Parsed, fills: [&str; 4], order: [usize; 4]) {
    let ids = ids(parsed);
    for i in order {
        parsed.fill_gap(ids[i], GapFill::Markdown(fills[i].to_owned())).unwrap();
    }
}

/// The diagnostics image OCR adds.
fn rows(parsed: &Parsed) -> Vec<(DiagnosticCode, Region, Option<u32>, u64)> {
    parsed
        .diagnostics()
        .iter()
        .filter(|d| d.code != DiagnosticCode::ImagePlaceholder)
        .map(|d| (d.code, d.region, d.unit.map(NonZeroU32::get), d.count))
        .collect()
}

const FILLED: &str = "para![Image](asset:0)\n\nA\n\n\
                      1. item![Image](asset:1)\n\n   B\n\n\
                      2. next\n\n\
                      |  |\n| --- |\n| x![Image](asset:0)<br>C |\n\n\
                      note[^1]\n\n\
                      gif![Image](asset:2)\n\n\
                      [^1]: foot![Image](asset:0)\n\n    D\n";

#[test]
fn each_png_or_jpeg_leaves_a_unit_less_gap_after_its_block_in_document_order() {
    let parsed = parsed(|_| {});
    let gaps: Vec<_> =
        parsed.gaps().map(|gap| (gap.id.unit(), gap.id.ordinal().get(), gap.origin)).collect();
    let assets = &parsed.document().assets;
    let image = |part: &str| {
        let asset = assets.iter().find(|asset| asset.origin_part.ends_with(part)).unwrap();
        OcrOrigin::Image(asset.id)
    };
    assert_eq!(
        gaps,
        [
            (None, 1, image("a.png")),
            (None, 2, image("b.jpeg")),
            (None, 3, image("a.png")),
            (None, 4, image("a.png")),
        ]
    );
    assert!(parsed.gaps().all(|gap| gap.reasons.is_empty()));
}

#[test]
fn fills_render_after_their_blocks_in_any_order() {
    for order in [[0, 1, 2, 3], [3, 2, 1, 0]] {
        let mut parsed = parsed(|_| {});
        fill_all(&mut parsed, ["A", "B", "C", "D"], order);
        let conversion = parsed.render().unwrap();
        assert_eq!(conversion.markdown(), FILLED);
        assert!(conversion.is_complete());
        assert_eq!(rows(&parsed), [(DiagnosticCode::ImageNotOcred, Region::Body, None, 1)]);
    }
}

#[test]
fn a_cell_joins_its_image_s_fill_by_line_breaks() {
    let mut parsed = parsed(|config| config.images = ImagePolicy::Omit);
    fill_all(&mut parsed, ["", "", "a b\n\nc", ""], [0, 1, 2, 3]);
    let markdown = parsed.render().unwrap().into_markdown();
    assert!(markdown.contains("| x<br>a b<br>c |\n"), "{markdown}");
}

fn without_image_ocr() -> String {
    configured(|_| {}).convert(Input::new(&pictures(), Format::Docx)).unwrap().into_markdown()
}

#[test]
fn unfilled_and_failed_image_gaps_render_as_without_image_ocr() {
    let unfilled = parsed(|_| {});
    let conversion = unfilled.render().unwrap();
    assert_eq!(conversion.markdown(), without_image_ocr());
    assert!(!conversion.is_complete());
    let ocr_gap = DiagnosticCode::OcrGap;
    assert_eq!(
        rows(&unfilled),
        [
            (ocr_gap, Region::Body, None, 3),
            (ocr_gap, Region::Note, None, 1),
            (DiagnosticCode::ImageNotOcred, Region::Body, None, 1),
        ]
    );

    let mut failed = parsed(|_| {});
    for id in ids(&failed) {
        failed.fail_gap(id).unwrap();
    }
    let conversion = failed.render().unwrap();
    assert_eq!(conversion.markdown(), without_image_ocr());
    assert!(!conversion.is_complete());
    let ocr_failed = DiagnosticCode::OcrFailed;
    assert_eq!(
        rows(&failed),
        [
            (ocr_failed, Region::Body, None, 3),
            (ocr_failed, Region::Note, None, 1),
            (DiagnosticCode::ImageNotOcred, Region::Body, None, 1),
        ]
    );
}

#[test]
fn a_one_call_conversion_leaves_its_image_gaps_unfilled() {
    let converter = configured(|config| config.image_ocr = ImageOcr::All);
    let conversion = converter.convert(Input::new(&pictures(), Format::Docx)).unwrap();
    assert_eq!(conversion.markdown(), without_image_ocr());
    assert!(!conversion.is_complete());
}

#[test]
fn without_image_ocr_no_image_leaves_a_gap() {
    let converter: Converter = configured(|_| {});
    let parsed = converter.parse(Input::new(&pictures(), Format::Docx)).unwrap();
    assert_eq!(parsed.gaps().count(), 0);
    assert!(rows(&parsed).iter().all(|row| row.0 != DiagnosticCode::ImageNotOcred));
}

/// What `python/tests` expects of the same fixture and fill.
const RICH_FILLED: &str = "Rich objects follow.\n\n**Quarterly Widgets**\n\n\
                           | Quarter | Widgets |\n| --- | --- |\n| Q1 | 10 |\n| Q2 | 14 |\n\n\
                           - Plan\n- Build\n- Ship\n\n\
                           ![tiny dot image](asset:0)\n\nRead _aloud_\n\n\
                           After the objects.\n";

#[test]
fn the_rich_fixture_s_picture_fills_after_its_paragraph() {
    let bytes = crate::support::fixture("docx/handmade-rich.docx");
    let converter = configured(|config| {
        config.image_ocr = ImageOcr::All;
        config.on_loss = almiladoc::OnLoss::Report;
    });
    let mut parsed = converter.parse(Input::new(&bytes, Format::Docx)).unwrap();
    let [id] = ids(&parsed).try_into().unwrap();
    assert_eq!((id.unit(), id.ordinal().get()), (None, 1));
    parsed.fill_gap(id, GapFill::Markdown("Read *aloud*".to_owned())).unwrap();
    assert_eq!(parsed.render().unwrap().markdown(), RICH_FILLED);
}

fn omitted(bytes: &[u8], image_ocr: ImageOcr, fill: Option<&str>) -> String {
    let converter = configured(|config| {
        (config.image_ocr, config.images) = (image_ocr, ImagePolicy::Omit);
    });
    let mut parsed = converter.parse(Input::new(bytes, Format::Docx)).unwrap();
    for id in ids(&parsed) {
        if let Some(fill) = fill {
            parsed.fill_gap(id, GapFill::Markdown(fill.to_owned())).unwrap();
        }
    }
    parsed.render().unwrap().into_markdown()
}

#[test]
fn an_image_gap_not_filled_shows_nothing_to_the_shapes_around_it() {
    let paragraph = |text: &str| format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>");
    let picture = format!("<w:p>{}</w:p>", pictured("", "png"));
    let inner = table(&[&paragraph("inner"), &paragraph("two")]);
    let framed = package(&table(&[&format!("{picture}{inner}")]), &paragraph("n"));
    let image_note = package(NOTE_REFERENCE, &picture);
    for (bytes, markdown) in
        [(&framed, "|  |  |\n| --- | --- |\n| inner | two |\n\n[^1]: n\n"), (&image_note, "note\n")]
    {
        assert_eq!(omitted(bytes, ImageOcr::None, None), markdown);
        assert_eq!(omitted(bytes, ImageOcr::All, None), markdown);
    }
}

#[test]
fn a_cell_shows_its_image_s_fill_where_the_image_alone_shows_nothing() {
    let picture = format!("<w:p>{}</w:p>", pictured("", "png"));
    let image_table_note = package(NOTE_REFERENCE, &table(&[&picture]));
    assert_eq!(omitted(&image_table_note, ImageOcr::None, None), "note\n");
    assert_eq!(
        omitted(&image_table_note, ImageOcr::All, Some("read")),
        "note[^1]\n\n[^1]: |  |\n    | --- |\n    | read |\n"
    );
}
