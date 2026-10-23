//! Superscript, subscript and symbol-font characters: where each reader takes them from, and the
//! Markdown it renders.

use crate::build::{W, docx_with, relationships};
use crate::support::{converter, fixture, reported, reporting};
use almiladoc::{DiagnosticCode, Format, Input, Region};

pub(crate) fn markdown(bytes: &[u8], format: Format) -> String {
    converter(1).convert(Input::new(bytes, format)).unwrap().into_markdown()
}

pub(crate) fn docx(styles: &str, body: &str) -> Vec<u8> {
    let rels = relationships(&[("rId1", "styles", "styles.xml")]);
    let styles = format!("<w:styles {W}>{styles}</w:styles>");
    docx_with(
        body,
        &[
            ("word/_rels/document.xml.rels", rels.as_bytes()),
            ("word/styles.xml", styles.as_bytes()),
        ],
    )
}

pub(crate) fn para(style: &str, runs: &str) -> String {
    let ppr = if style.is_empty() {
        String::new()
    } else {
        format!(r#"<w:pPr><w:pStyle w:val="{style}"/></w:pPr>"#)
    };
    format!("<w:p>{ppr}{runs}</w:p>")
}

pub(crate) fn run(rpr: &str, text: &str) -> String {
    format!(r#"<w:r><w:rPr>{rpr}</w:rPr><w:t xml:space="preserve">{text}</w:t></w:r>"#)
}

fn vert_align(value: &str) -> String {
    format!(r#"<w:vertAlign w:val="{value}"/>"#)
}

fn character_style(id: &str) -> String {
    format!(r#"<w:rStyle w:val="{id}"/>"#)
}

#[test]
fn word_takes_the_nearest_shift_from_defaults_paragraph_style_character_style_and_run() {
    let styles = format!(
        r#"<w:docDefaults><w:rPrDefault><w:rPr>{sup}</w:rPr></w:rPrDefault></w:docDefaults>
        <w:style w:type="paragraph" w:styleId="Level"><w:name w:val="Level"/><w:rPr>{base}</w:rPr></w:style>
        <w:style w:type="character" w:styleId="Down"><w:name w:val="Down"/><w:rPr>{sub}</w:rPr></w:style>
        <w:style w:type="character" w:styleId="Below"><w:name w:val="Below"/><w:basedOn w:val="Down"/></w:style>
        <w:style w:type="character" w:styleId="Heavy"><w:name w:val="Heavy"/><w:rPr><w:b/></w:rPr></w:style>"#,
        sup = vert_align("superscript"),
        base = vert_align("baseline"),
        sub = vert_align("subscript"),
    );
    let body = [
        para("", &run("", "a")),
        para("Level", &run("", "b")),
        para("Level", &run(&character_style("Below"), "c")),
        para("Level", &run(&(character_style("Down") + &vert_align("superscript")), "d")),
        para("", &run(&character_style("Heavy"), "e")),
        para("Level", &(run("", "f") + &run(&vert_align("subscript"), "2") + &run("", "g"))),
        para("", &run(&vert_align("baseline"), "h")),
    ]
    .concat();
    assert_eq!(
        markdown(&docx(&styles, &body), Format::Docx),
        "<sup>a</sup>\n\nb\n\n<sub>c</sub>\n\n<sup>d</sup>\n\n<sup>**e**</sup>\n\nf<sub>2</sub>g\n\nh\n"
    );
}

#[test]
fn a_paragraph_naming_no_style_or_a_missing_one_takes_the_default_paragraph_styles_shift() {
    let styles = format!(
        r#"<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:rPr>{sup}</w:rPr></w:style>
        <w:style w:type="paragraph" w:styleId="Body"><w:name w:val="Body"/></w:style>
        <w:style w:type="paragraph" w:styleId="Low"><w:name w:val="Low"/><w:rPr>{sub}</w:rPr></w:style>
        <w:style w:type="paragraph" w:styleId="LowChild"><w:name w:val="Low Child"/><w:basedOn w:val="Low"/></w:style>"#,
        sup = vert_align("superscript"),
        sub = vert_align("subscript"),
    );
    let body = [
        para("", &run("", "a")),
        para("Missing", &run("", "b")),
        para("Body", &run("", "c")),
        para("LowChild", &run("", "d")),
    ]
    .concat();
    assert_eq!(
        markdown(&docx(&styles, &body), Format::Docx),
        "<sup>a</sup>\n\n<sup>b</sup>\n\nc\n\n<sub>d</sub>\n"
    );
}

#[test]
fn a_shift_set_directly_through_a_style_or_cancelled_renders_alike_in_word_odf_and_epub() {
    let expected = "Direct: 10<sup>-3</sup> M and H<sub>2</sub>O.\n\n\
                    Through a style: x<sup>2</sup> and CO<sub>2</sub>.\n\n\
                    Baseline over a superscript style: level.\n";
    for (path, format) in [
        ("docx/handmade-position.docx", Format::Docx),
        ("odt/handmade-position.odt", Format::Odt),
        ("epub/handmade-position.epub", Format::Epub),
    ] {
        assert_eq!(markdown(&fixture(path), format), expected, "{path}");
    }
}

#[test]
fn a_deck_saved_as_pptx_and_as_ppt_renders_and_reports_alike() {
    let expected = "## Scripts and symbols\n\n\
                    Area: 5 m<sup>2</sup>\n\n\
                    H<sub>2</sub>O\n\n\
                    Size: 10 \u{3bc}m\n\n\
                    Gap: 3 \u{3bc}s\n\n\
                    Checked \u{2713} end\n";
    let converter = reporting();
    for (path, format) in
        [("pptx/handmade-scripts.pptx", Format::Pptx), ("ppt/handmade-scripts.ppt", Format::Ppt)]
    {
        let conversion = converter.convert(Input::new(&fixture(path), format)).unwrap();
        assert_eq!(conversion.markdown(), expected, "{path}");
        assert_eq!(
            reported(conversion.diagnostics()),
            [(DiagnosticCode::UnmappedSymbol, Region::Body, Some(1), 1)],
            "{path}"
        );
    }
}
