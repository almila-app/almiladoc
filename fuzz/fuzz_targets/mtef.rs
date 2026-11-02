#![no_main]

use almiladoc::Format;
use almiladoc_fuzz::{convert, fuzz_target, stored};
use std::io::{Cursor, Write};

// An equation object around the mutated MTEF lets mutations reach the equation decoder.
fuzz_target!(|data: &[u8]| {
    let Ok(declared) = u32::try_from(data.len()) else {
        return;
    };
    let header = [&28_u16.to_le_bytes()[..], &[0; 6], &declared.to_le_bytes(), &[0; 16]].concat();
    let Ok(mut ole) = cfb::CompoundFile::create(Cursor::new(Vec::new())) else {
        return;
    };
    let native = [header.as_slice(), data].concat();
    if ole
        .create_stream("Equation Native")
        .and_then(|mut stream| stream.write_all(&native))
        .is_err()
        || ole.flush().is_err()
    {
        return;
    }
    let object = ole.into_inner().into_inner();
    let parts: [(&str, &[u8]); 5] = [
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", DOCUMENT.as_bytes()),
        ("word/_rels/document.xml.rels", DOCUMENT_RELS.as_bytes()),
        ("word/embeddings/oleObject1.bin", &object),
    ];
    let Ok(bytes) = stored(&parts) else {
        return;
    };
    convert(&bytes, Format::Docx);
});

const CONTENT_TYPES: &str = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="bin" ContentType="application/vnd.openxmlformats-officedocument.oleObject"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
const RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
const DOCUMENT_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/oleObject" Target="embeddings/oleObject1.bin"/></Relationships>"#;
const DOCUMENT: &str = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:o="urn:schemas-microsoft-com:office:office"><w:body><w:p><w:r><w:object><o:OLEObject Type="Embed" ProgID="Equation.DSMT4" r:id="rId1"/></w:object></w:r></w:p><w:p><w:r><w:t>beside </w:t></w:r><w:r><w:object><o:OLEObject Type="Embed" ProgID="Equation.3" r:id="rId1"/></w:object></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:object><o:OLEObject Type="Embed" ProgID="Equation.DSMT4" r:id="rId1"/></w:object></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;
