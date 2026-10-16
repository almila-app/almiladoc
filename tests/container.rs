//! Container facts come from the ZIP central directory before any entry is
//! opened: duplicate names, encryption flags and the entry count; an
//! encrypted OOXML file, which is a compound file, is still `Encrypted`.

use std::io::{Cursor, Write};

use almiladoc::{Config, ConvertError, Converter, Format, Input, Limit};
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
