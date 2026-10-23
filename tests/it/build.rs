//! Packages and records the integration tests assemble by hand.

use std::io::{Cursor, Write};

use crate::support::zip_package;

pub(crate) const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

pub(crate) const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:v="urn:schemas-microsoft-com:vml" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:x="urn:unsupported""#;
pub(crate) const ODF: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0""#;
const PML: &str = r#"xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

pub(crate) fn relationships(rels: &[(&str, &str, &str)]) -> String {
    let rels: String = rels
        .iter()
        .map(|(id, kind, target)| {
            format!(r#"<Relationship Id="{id}" Type="{REL}/{kind}" Target="{target}"/>"#)
        })
        .collect();
    format!(r#"<Relationships xmlns="{PKG_RELS}">{rels}</Relationships>"#)
}

pub(crate) fn ole(streams: &[(&str, &[u8])]) -> Vec<u8> {
    let mut file = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    for (name, body) in streams {
        file.create_stream(name).unwrap().write_all(body).unwrap();
    }
    file.flush().unwrap();
    file.into_inner().into_inner()
}

pub(crate) fn record(version: u16, kind: u16, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len()).unwrap().to_le_bytes();
    [&version.to_le_bytes()[..], &kind.to_le_bytes(), &len, body].concat()
}

pub(crate) fn atom(kind: u16, words: &[u32]) -> Vec<u8> {
    record(0, kind, &words.iter().flat_map(|word| word.to_le_bytes()).collect::<Vec<_>>())
}

pub(crate) fn container(instance: u16, kind: u16, children: &[Vec<u8>]) -> Vec<u8> {
    record((instance << 4) | 15, kind, &children.concat())
}

pub(crate) fn epub(body: &str) -> Vec<u8> {
    let container = r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
    let opf = r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0"><manifest><item id="c" href="c.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c"/></spine></package>"#;
    let chapter =
        format!(r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>{body}</body></html>"#);
    zip_package(&[
        ("mimetype", "application/epub+zip"),
        ("META-INF/container.xml", container),
        ("content.opf", opf),
        ("c.xhtml", &chapter),
    ])
}

pub(crate) fn epub_bare(body: &str) -> Vec<u8> {
    zip_package(&[
        ("META-INF/container.xml", r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="book.opf"/></rootfiles></container>"#.into()),
        ("book.opf", r#"<package xmlns="http://www.idpf.org/2007/opf"><manifest><item id="c" href="c.xhtml"/></manifest><spine><itemref idref="c"/></spine></package>"#.into()),
        ("c.xhtml", format!(r#"<html xmlns="http://www.w3.org/1999/xhtml"><head/><body>{body}</body></html>"#)),
    ])
}

pub(crate) fn docx_document(body: &str) -> String {
    format!("<w:document {W}><w:body>{body}</w:body></w:document>")
}

pub(crate) fn docx(body: &str) -> Vec<u8> {
    docx_with(body, &[])
}

pub(crate) fn docx_with(body: &str, parts: &[(&str, &[u8])]) -> Vec<u8> {
    let document = docx_document(body);
    let mut all: Vec<(&str, &[u8])> = vec![("word/document.xml", document.as_bytes())];
    all.extend_from_slice(parts);
    zip_package(&all)
}

pub(crate) fn odf_styled(styles: &str, body: &str) -> Vec<u8> {
    let content = format!(
        "<office:document-content {ODF}>{styles}<office:body>{body}</office:body></office:document-content>"
    );
    zip_package(&[("content.xml", content)])
}

pub(crate) fn odf(body: &str) -> Vec<u8> {
    odf_styled("", body)
}

pub(crate) fn ods(body: &str) -> Vec<u8> {
    odf(&format!(
        r#"<office:spreadsheet><table:table table:name="S">{body}</table:table></office:spreadsheet>"#
    ))
}

pub(crate) fn slide_part(root: &str, tree: &str) -> String {
    format!("<p:{root} {PML}><p:cSld><p:spTree>{tree}</p:spTree></p:cSld></p:{root}>")
}

pub(crate) fn pptx(tree: &str, parts: &[(&str, &[u8])]) -> Vec<u8> {
    let presentation = format!(
        r#"<p:presentation {PML}><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#
    );
    let rels = relationships(&[("rId1", "slide", "slides/slide1.xml")]);
    let slide = slide_part("sld", tree);
    let mut all: Vec<(&str, &[u8])> = vec![
        ("ppt/presentation.xml", presentation.as_bytes()),
        ("ppt/_rels/presentation.xml.rels", rels.as_bytes()),
        ("ppt/slides/slide1.xml", slide.as_bytes()),
    ];
    all.extend_from_slice(parts);
    zip_package(&all)
}
