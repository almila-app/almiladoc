//! Content-based format detection.
//!
//! Identifies the format from what each specification designates as the
//! container's identity, never from heuristics over document content:
//!
//! - PDF: the `%PDF-` header (ISO 32000; implementations accept leading
//!   junk, bounded here at 1024 bytes).
//! - RTF: the `{\rtf` group that must open every RTF file.
//! - OLE compound files: the [MS-CFB] signature, then the stream name each
//!   binary Office format mandates ([MS-DOC] `WordDocument`, [MS-PPT]
//!   `PowerPoint Document`, [MS-XLS] `Workbook`/`Book`).
//! - ZIP packages: the local-file-header signature, then the package's own
//!   identity: the `mimetype` part (ODF/EPUB OCF), or for OPC the content
//!   type of the part the package-level officeDocument relationship
//!   designates as the main document (with the namespace of the main part's
//!   mandated root element as the authority when content types are stale
//!   or generic). A relationship that resolves to a part decides alone;
//!   otherwise the conventional main-part locations are typed.
//!
//! A package is read only as far as its identity: at most
//! [`MIMETYPE_BYTES`] of `mimetype`, [`METADATA_BYTES`] of each metadata
//! part and the first [`ROOT_TAG_BYTES`] of the main part, within the entry
//! and total byte budgets, which count these reads with the parse's own.
//! `mimetype` or a metadata part past its cap or a budget reads as absent,
//! whatever size it declares; the main part is read only as far as they
//! allow.
//!
//! Plain-text formats (CSV) carry no signature and are never detected.
//! Detection never fails a sniff: any unreadable or ambiguous container
//! yields `None`. The label check runs it on the parse's own context, so
//! a limit or cancellation there fails the parse.

use crate::Format;
use crate::ctx::ParseCtx;
use crate::error::{ConvertError, UnsupportedKind};
use crate::limits::Limits;
use crate::package::relationships::{ROOT_RELS, Relationships, main_part};
use crate::package::xml::{Element, ns, root_namespace};
use crate::package::{Package, PackageKind};
use std::io::Cursor;

const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const PDF_MAGIC: &[u8] = b"%PDF-";
const CT_NS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const MANIFEST_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";
const SPREADSHEET_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

/// The OOXML families in match order: main-part directory, content-type
/// segments, root-element namespace and format. A content type names its
/// family by a segment that covers every variant (document, template,
/// slideshow); the `ms-*` segments are the macro-enabled ones, and `ms-excel`
/// also the binary `.xlsb` main part. The namespace is normalized from Strict
/// to Transitional.
const FAMILIES: [(&str, [&str; 2], &str, Format); 3] = [
    ("word/", ["wordprocessingml", "ms-word"], ns::W, Format::Docx),
    ("ppt/", ["presentationml", "ms-powerpoint"], ns::P, Format::Pptx),
    ("xl/", ["spreadsheetml", "ms-excel"], SPREADSHEET_NS, Format::Excel),
];

const CONVENTIONAL_PARTS: [&str; 4] =
    ["word/document.xml", "ppt/presentation.xml", "xl/workbook.xml", "xl/workbook.bin"];

const MIMETYPE_BYTES: u64 = 256;
const METADATA_BYTES: u64 = 1024 * 1024;
const ROOT_TAG_BYTES: u64 = 16 * 1024;

pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Format> {
    detect(&ParseCtx::new(&Limits::DEFAULT), bytes).ok().flatten()
}

/// Fails [`UnsupportedKind::FormatMismatch`] when the content is recognized
/// as a format other than `label`. A PDF header past the first byte does
/// not contradict a CSV label: delimited text may quote one.
pub(crate) fn check_label(
    parse_ctx: &ParseCtx<'_>,
    bytes: &[u8],
    label: Format,
) -> Result<(), ConvertError> {
    match detect(parse_ctx, bytes)? {
        Some(Format::Pdf) if label == Format::Csv && !bytes.starts_with(PDF_MAGIC) => Ok(()),
        Some(detected) if detected != label => {
            Err(ConvertError::from(UnsupportedKind::FormatMismatch { detected }))
        }
        _ => Ok(()),
    }
}

/// The format the content is, `None` when it names none; only a fatal error
/// fails it.
pub(crate) fn detect(
    parse_ctx: &ParseCtx<'_>,
    bytes: &[u8],
) -> Result<Option<Format>, ConvertError> {
    #[cfg(test)]
    parse_ctx.note_detection();
    if bytes.starts_with(b"{\\rtf") {
        return Ok(Some(Format::Rtf));
    }
    if bytes.starts_with(&OLE_MAGIC) {
        return Ok(detect_ole(bytes));
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return detect_zip(parse_ctx, bytes);
    }
    let head = bytes.get(..1024).unwrap_or(bytes);
    Ok(head.windows(PDF_MAGIC.len()).any(|w| w == PDF_MAGIC).then_some(Format::Pdf))
}

/// Classify an OLE compound file by its mandated content stream. Encrypted
/// OOXML packages (`EncryptedPackage`) stay `None`: the inner format is
/// unknowable, and the frontend reports `Encrypted` precisely.
fn detect_ole(bytes: &[u8]) -> Option<Format> {
    const STREAMS: [(&str, Format); 4] = [
        ("WordDocument", Format::Doc),
        ("PowerPoint Document", Format::Ppt),
        ("Workbook", Format::Excel),
        ("Book", Format::Excel),
    ];
    let ole = cfb::CompoundFile::open(Cursor::new(bytes)).ok()?;
    // Stream-name comparison is case-insensitive, matching CFB's own
    // uppercase name comparisons; producers vary (`WORKBOOK`, `BOOK`).
    ole.read_root_storage().find_map(|entry| {
        STREAMS.iter().find_map(|&(stream, format)| {
            entry.name().eq_ignore_ascii_case(stream).then_some(format)
        })
    })
}

fn detect_zip(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Option<Format>, ConvertError> {
    let Some(pkg) = opened(Package::open(parse_ctx, bytes, PackageKind::Odf))? else {
        return Ok(None);
    };

    // ODF and EPUB designate the `mimetype` part as the package identity;
    // their part names compare case-sensitively, so OPC's folding waits.
    if let Some(mime) = pkg.probe_part("mimetype", MIMETYPE_BYTES)? {
        return Ok(std::str::from_utf8(&mime).ok().and_then(|mime| mimetype_format(mime.trim())));
    }

    if let Some(opc) = opened(pkg.reopen(PackageKind::Opc))?
        && let Some(format) = opc_format_of(&opc)?
    {
        return Ok(Some(format));
    }

    // ODF without its mandatory `mimetype`: the manifest's root file entry
    // carries the same media type.
    if let Some(manifest) = pkg.probe_xml_part("META-INF/manifest.xml", METADATA_BYTES)?
        && let Some(root) = manifest
            .descendants(MANIFEST_NS, "file-entry")
            .find(|e| e.attr(MANIFEST_NS, "full-path") == Some("/"))
        && let Some(mime) = root.attr(MANIFEST_NS, "media-type")
    {
        return Ok(mimetype_format(mime.trim()));
    }

    // EPUB without its mandatory `mimetype`: the OCF container descriptor.
    Ok(pkg.has_part("META-INF/container.xml").then_some(Format::Epub))
}

fn opened(pkg: Result<Package<'_>, ConvertError>) -> Result<Option<Package<'_>>, ConvertError> {
    match pkg {
        Ok(pkg) => Ok(Some(pkg)),
        Err(e) if e.is_fatal() => Err(e),
        Err(_) => Ok(None),
    }
}

fn opc_format_of(pkg: &Package<'_>) -> Result<Option<Format>, ConvertError> {
    let rels = pkg.probe_xml_part(ROOT_RELS, METADATA_BYTES)?;
    let rels = rels.as_ref().map(Relationships::from_root).unwrap_or_default();
    let Some(main) = main_part(pkg, &rels, &CONVENTIONAL_PARTS) else {
        return Ok(None);
    };
    main_part_format(pkg, &main.path)
}

fn main_part_format(pkg: &Package<'_>, part: &str) -> Result<Option<Format>, ConvertError> {
    if let Some(types) = pkg.probe_xml_part("[Content_Types].xml", METADATA_BYTES)?
        && let Some(format) = content_type_of(&types, part).and_then(opc_format)
    {
        return Ok(Some(format));
    }
    // Content types can be stale or generic; the main part's mandated root
    // element (`w:document`, `p:presentation`, `workbook`) is the next
    // authority.
    if let Some(head) = pkg.probe_head(part, ROOT_TAG_BYTES)?
        && let Some(format) = root_namespace(&head).as_deref().and_then(namespace_format)
    {
        return Ok(Some(format));
    }
    // Binary main parts (`xl/workbook.bin`) and unreadable ones: the family
    // directory names the format.
    Ok(opc_format_by_path(part))
}

fn mimetype_format(mime: &str) -> Option<Format> {
    // `-template` variants share the base format's parser.
    let mime = mime.strip_suffix("-template").unwrap_or(mime);
    Some(match mime {
        "application/epub+zip" => Format::Epub,
        "application/vnd.oasis.opendocument.text" => Format::Odt,
        "application/vnd.oasis.opendocument.spreadsheet" => Format::Ods,
        "application/vnd.oasis.opendocument.presentation" => Format::Odp,
        _ => return None,
    })
}

/// Resolve a part's content type per OPC: an `Override` for the exact part
/// name wins, else the `Default` for its extension.
fn content_type_of<'a>(types: &'a Element, part: &str) -> Option<&'a str> {
    let find = |tag, key, value: &str| {
        types
            .descendants(CT_NS, tag)
            .find(|e| e.attr_any(key).is_some_and(|v| v.eq_ignore_ascii_case(value)))
            .and_then(|e| e.attr_any("ContentType"))
    };
    find("Override", "PartName", &format!("/{part}"))
        .or_else(|| find("Default", "Extension", part.rsplit_once('.')?.1))
}

fn opc_format(content_type: &str) -> Option<Format> {
    let content_type = content_type.to_ascii_lowercase();
    FAMILIES.iter().find_map(|&(_, segments, _, format)| {
        segments.iter().any(|segment| content_type.contains(segment)).then_some(format)
    })
}

fn namespace_format(namespace: &str) -> Option<Format> {
    FAMILIES.iter().find_map(|&(_, _, root, format)| (root == namespace).then_some(format))
}

fn opc_format_by_path(part: &str) -> Option<Format> {
    FAMILIES
        .iter()
        .find_map(|&(directory, _, _, format)| part.starts_with(directory).then_some(format))
}

#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    clippy::disallowed_methods,
    reason = "the tests read fixtures and write known small sizes into fixed-width fields"
)]
mod tests {
    use super::*;
    use crate::ledger::CancelToken;
    use crate::shared::test_support::{cancelled, stored, zip_package, zip_package_with};
    use std::assert_matches;
    use std::io::Write;

    const RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const OFFICE_DOCUMENT: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
    const PRESENTATION: &str =
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml";

    fn ole_of(streams: &[&str]) -> Vec<u8> {
        let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        for name in streams {
            ole.create_stream(name).unwrap().write_all(b"x").unwrap();
        }
        ole.into_inner().into_inner()
    }

    #[test]
    fn signatures() {
        assert_eq!(from_bytes(b"%PDF-1.7\n"), Some(Format::Pdf));
        // Leading junk before the header is accepted, bounded.
        let mut junk = vec![b' '; 500];
        junk.extend_from_slice(b"%PDF-1.4");
        assert_eq!(from_bytes(&junk), Some(Format::Pdf));
        assert_eq!(from_bytes(b"{\\rtf1\\ansi hi}"), Some(Format::Rtf));
        assert_eq!(from_bytes(b"a,b,c\n1,2,3\n"), None);
        assert_eq!(from_bytes(b""), None);
    }

    #[test]
    fn container_signature_wins_over_an_early_embedded_pdf() {
        let pkg =
            zip_package(&[("embedded.pdf", "%PDF-1.7\n"), ("word/document.xml", "<document/>")]);
        assert_eq!(from_bytes(&pkg), Some(Format::Docx));
    }

    #[test]
    fn ole_streams_designate_the_binary_format() {
        assert_eq!(from_bytes(&ole_of(&["WordDocument"])), Some(Format::Doc));
        assert_eq!(from_bytes(&ole_of(&["PowerPoint Document"])), Some(Format::Ppt));
        assert_eq!(from_bytes(&ole_of(&["Workbook"])), Some(Format::Excel));
        assert_eq!(from_bytes(&ole_of(&["BOOK"])), Some(Format::Excel));
        // Encrypted OOXML: inner format unknowable, extension decides.
        assert_eq!(from_bytes(&ole_of(&["EncryptedPackage", "EncryptionInfo"])), None);
    }

    #[test]
    fn mimetype_identifies_odf_and_epub() {
        for (mime, format) in [
            ("application/vnd.oasis.opendocument.text", Format::Odt),
            ("application/vnd.oasis.opendocument.text-template", Format::Odt),
            ("application/vnd.oasis.opendocument.spreadsheet", Format::Ods),
            ("application/vnd.oasis.opendocument.presentation", Format::Odp),
            ("application/epub+zip", Format::Epub),
        ] {
            assert_eq!(from_bytes(&zip_package(&[("mimetype", mime.as_bytes())])), Some(format));
        }
        assert_eq!(from_bytes(&zip_package(&[("mimetype", "application/zip")])), None);
    }

    #[test]
    fn opc_main_part_content_type_wins_over_its_path() {
        // Main part at a nonconventional path; only the content type says
        // this is a presentation.
        let rels = rels_naming("/custom/main.xml");
        let types = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
            <Override PartName="/custom/main.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
        </Types>"#;
        let pkg = zip_package(&[
            ("_rels/.rels", rels.as_str()),
            ("[Content_Types].xml", types),
            ("custom/main.xml", "<p/>"),
        ]);
        assert_eq!(from_bytes(&pkg), Some(Format::Pptx));
    }

    #[test]
    fn opc_stale_content_types_defer_to_the_root_element() {
        // The Override names a part that does not exist; the real main part
        // only gets the generic Default. Its w:document root decides.
        let rels = rels_naming("content/main.xml");
        let types = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
            <Default Extension="xml" ContentType="application/xml"/>
            <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
        </Types>"#;
        let main = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body/></w:document>"#;
        let pkg = zip_package(&[
            ("_rels/.rels", rels.as_str()),
            ("[Content_Types].xml", types),
            ("content/main.xml", main),
        ]);
        assert_eq!(from_bytes(&pkg), Some(Format::Docx));
    }

    #[test]
    fn opc_falls_back_to_conventional_paths() {
        // No content types: the main-part path decides.
        let rels = rels_naming("word/document.xml");
        let pkg = zip_package(&[("_rels/.rels", rels.as_str()), ("word/document.xml", "<d/>")]);
        assert_eq!(from_bytes(&pkg), Some(Format::Docx));

        // No rels at all: conventional part existence decides.
        let pkg = zip_package(&[("xl/workbook.bin", "\0")]);
        assert_eq!(from_bytes(&pkg), Some(Format::Excel));
    }

    #[test]
    fn each_conventional_part_names_its_format_when_the_package_names_none() {
        let no_office_document = format!(
            r#"<Relationships xmlns="{RELS}"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#
        );
        for (part, format) in [
            ("word/document.xml", Format::Docx),
            ("ppt/presentation.xml", Format::Pptx),
            ("xl/workbook.xml", Format::Excel),
            ("xl/workbook.bin", Format::Excel),
        ] {
            assert_eq!(from_bytes(&zip_package(&[(part, "<x/>")])), Some(format), "{part}");
            let with_rels =
                zip_package(&[(part, "<x/>"), ("_rels/.rels", no_office_document.as_str())]);
            assert_eq!(from_bytes(&with_rels), Some(format), "{part}");
        }
        let all = zip_package(&[
            ("xl/workbook.bin", "<x/>"),
            ("xl/workbook.xml", "<x/>"),
            ("ppt/presentation.xml", "<x/>"),
            ("word/document.xml", "<x/>"),
        ]);
        assert_eq!(from_bytes(&all), Some(Format::Docx));
        let spreadsheets = zip_package(&[("xl/workbook.bin", "<x/>"), ("xl/workbook.xml", "<x/>")]);
        assert_eq!(from_bytes(&spreadsheets), Some(Format::Excel));
    }

    #[test]
    fn a_conventional_main_part_is_typed_like_a_named_one() {
        let presentation = format!(r#"<p:presentation xmlns:p="{}"/>"#, ns::P);
        let pkg = zip_package(&[("word/document.xml", presentation.as_str())]);
        assert_eq!(from_bytes(&pkg), Some(Format::Pptx));
        let types = format!(
            r#"<Types xmlns="{CT_NS}"><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#
        );
        let pkg = zip_package(&[("xl/workbook.xml", "<x/>"), ("[Content_Types].xml", &types)]);
        assert_eq!(from_bytes(&pkg), Some(Format::Docx));
    }

    #[test]
    fn a_target_that_names_no_part_is_typed_by_the_conventional_part() {
        for target in ["", "/", "#frag", "a%2Fb.xml"] {
            let pkg = zip_package(&[
                ("_rels/.rels", rels_naming(target)),
                ("ppt/presentation.xml", "<x/>".to_string()),
            ]);
            assert_eq!(from_bytes(&pkg), Some(Format::Pptx), "{target:?}");
        }
    }

    #[test]
    fn a_target_that_resolves_is_never_replaced_by_a_leftover_part() {
        let types = types_naming("application/octet-stream");
        for leftover in CONVENTIONAL_PARTS {
            for (target, present) in [("custom/main.xml", true), ("missing/main.xml", false)] {
                let mut parts = vec![
                    ("_rels/.rels", rels_naming(target)),
                    ("[Content_Types].xml", types.clone()),
                    (leftover, "<x/>".to_string()),
                ];
                if present {
                    parts.push((target, "<main/>".to_string()));
                }
                let pkg = zip_package(&parts);
                assert_eq!(from_bytes(&pkg), None, "{leftover} beside {target}");
                for label in [Format::Docx, Format::Pptx, Format::Excel] {
                    check_label(&ParseCtx::default(), &pkg, label).unwrap();
                }
            }
        }
    }

    #[test]
    fn a_workbook_the_content_types_cannot_type_converts_beside_a_leftover_document() {
        let fixture = |name: &str| {
            std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
        };
        let original = fixture("xlsb/handmade-sheet.xlsb");
        let mut source = zip::ZipArchive::new(Cursor::new(original.as_slice())).unwrap();
        let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
        for index in 0..source.len() {
            let mut file = source.by_index(index).unwrap();
            let name = file.name().unwrap().into_owned();
            let mut body = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut body).unwrap();
            let name = name.strip_prefix("xl/").map_or(name.clone(), |rest| format!("book/{rest}"));
            parts.push((name, body));
        }
        parts.retain(|(name, _)| name != "_rels/.rels");
        parts.push(("_rels/.rels".into(), rels_naming("book/workbook.bin").into_bytes()));
        parts.push((
            "[Content_Types].xml".into(),
            types_naming("application/octet-stream").into_bytes(),
        ));
        parts.push(("word/document.xml".into(), b"<w:document/>".to_vec()));
        let moved = zip_package(&parts);

        let parse_ctx = ParseCtx::default();
        assert_eq!(detect(&parse_ctx, &moved).unwrap(), None);
        check_label(&parse_ctx, &moved, Format::Excel).unwrap();
        let converted = crate::formats::parse(&parse_ctx, &moved, Format::Excel).unwrap();
        let expected =
            crate::formats::parse(&ParseCtx::default(), &original, Format::Excel).unwrap();
        let markdown = crate::render::markdown::document_to_markdown;
        assert_eq!(markdown(&converted), markdown(&expected));
    }

    #[test]
    fn broken_odf_and_epub_fall_back_to_manifest_and_container() {
        let manifest = manifest_naming("application/vnd.oasis.opendocument.spreadsheet");
        let pkg =
            zip_package(&[("META-INF/manifest.xml", manifest.as_str()), ("content.xml", "<c/>")]);
        assert_eq!(from_bytes(&pkg), Some(Format::Ods));

        let pkg = zip_package(&[("META-INF/container.xml", "<container/>")]);
        assert_eq!(from_bytes(&pkg), Some(Format::Epub));

        // A plain zip is not a document.
        assert_eq!(from_bytes(&zip_package(&[("readme.txt", "hi")])), None);
    }

    fn types_naming(content_type: &str) -> String {
        format!(
            r#"<Types xmlns="{CT_NS}"><Default Extension="xml" ContentType="application/xml"/><Override PartName="/custom/main.xml" ContentType="{content_type}"/></Types>"#
        )
    }

    fn rels_naming(target: &str) -> String {
        format!(
            r#"<Relationships xmlns="{RELS}"><Relationship Id="rId1" Type="{OFFICE_DOCUMENT}" Target="{target}"/></Relationships>"#
        )
    }

    fn manifest_naming(media_type: &str) -> String {
        format!(
            r#"<manifest:manifest xmlns:manifest="{MANIFEST_NS}"><manifest:file-entry manifest:full-path="/" manifest:media-type="{media_type}"/></manifest:manifest>"#
        )
    }

    fn main_part_package(main: &[u8], types: Option<&str>) -> Vec<u8> {
        let rels = rels_naming("custom/main.xml");
        let mut parts: Vec<(&str, &[u8])> =
            vec![("custom/main.xml", main), ("_rels/.rels", rels.as_bytes())];
        if let Some(types) = types {
            parts.push(("[Content_Types].xml", types.as_bytes()));
        }
        zip_package(&parts)
    }

    #[test]
    fn macro_enabled_main_content_types_name_their_family() {
        for (content_type, format) in [
            ("application/vnd.ms-word.document.macroEnabled.main+xml", Format::Docx),
            ("application/vnd.ms-word.template.macroEnabledTemplate.main+xml", Format::Docx),
            ("application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml", Format::Pptx),
            ("application/vnd.ms-powerpoint.slideshow.macroEnabled.main+xml", Format::Pptx),
            ("application/vnd.ms-excel.sheet.macroEnabled.main+xml", Format::Excel),
            ("APPLICATION/VND.MS-EXCEL.SHEET.MACROENABLED.MAIN+XML", Format::Excel),
        ] {
            let pkg = main_part_package(b"<main/>", Some(&types_naming(content_type)));
            assert_eq!(from_bytes(&pkg), Some(format), "{content_type}");
        }
        let generic = types_naming("application/xml");
        assert_eq!(from_bytes(&main_part_package(b"<main/>", Some(&generic))), None);
    }

    #[test]
    fn an_override_without_a_content_type_falls_through_to_the_extension_default() {
        let typed_by = |overrides: &str, extension: &str| {
            let types = format!(
                r#"<Types xmlns="{CT_NS}">{overrides}<Default Extension="{extension}" ContentType="{PRESENTATION}"/></Types>"#
            );
            from_bytes(&main_part_package(b"<main/>", Some(&types)))
        };
        let nameless = r#"<Override PartName="/CUSTOM/main.xml"/>"#;
        let other = r#"<Override PartName="/other.xml" ContentType="a/b"/>"#;
        assert_eq!(typed_by("", "XML"), Some(Format::Pptx));
        assert_eq!(typed_by(nameless, "xml"), Some(Format::Pptx));
        assert_eq!(typed_by(other, "xml"), Some(Format::Pptx));
        assert_eq!(typed_by("", "bin"), None);
    }

    #[test]
    fn the_root_element_is_read_from_the_head_of_the_main_part() {
        let strict = br#"<?xml version="1.0"?><!-- p --><w:document xmlns:w="http://purl.oclc.org/ooxml/wordprocessingml/main"><w:body>"#;
        assert_eq!(from_bytes(&main_part_package(strict, None)), Some(Format::Docx));
        let workbook =
            br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"/>"#;
        assert_eq!(from_bytes(&main_part_package(workbook, None)), Some(Format::Excel));
        let unbound = b"<p:presentation><p:sldIdLst/>";
        assert_eq!(from_bytes(&main_part_package(unbound, None)), None);
        let tag = br#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"/>"#;
        let ending_at = |end: usize| {
            let mut main = vec![b' '; end - tag.len()];
            main.extend_from_slice(tag);
            main.extend_from_slice(b"<p:sldIdLst/>");
            main_part_package(&main, None)
        };
        let cap = usize::try_from(ROOT_TAG_BYTES).unwrap();
        assert_eq!(from_bytes(&ending_at(cap)), Some(Format::Pptx));
        assert_eq!(from_bytes(&ending_at(cap + 1)), None);
    }

    #[test]
    fn a_main_part_declaring_512_mib_is_sniffed_from_its_head() {
        let mut main =
            format!(r#"<w:document xmlns:w="{}"><w:body><w:p><w:r><w:t>"#, ns::W).into_bytes();
        let mut state = 0x2545_f491_u32;
        main.extend((0..1 << 20).map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            b'a' + (state % 26) as u8
        }));
        let mut pkg = main_part_package(&main, Some(&types_naming("application/xml")));
        let declared = 512 * 1024 * 1024_u32;
        assert!(pkg.starts_with(b"PK\x03\x04"));
        pkg[22..26].copy_from_slice(&declared.to_le_bytes());
        let central = pkg.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        pkg[central + 24..central + 28].copy_from_slice(&declared.to_le_bytes());

        let parse_ctx = ParseCtx::default();
        assert_eq!(detect(&parse_ctx, &pkg).unwrap(), Some(Format::Docx));
        let fuel = parse_ctx.fuel();
        assert!(fuel <= ROOT_TAG_BYTES / 1024 + 16, "fuel {fuel}");
    }

    #[test]
    fn a_metadata_part_past_its_cap_reads_as_absent() {
        let types = types_naming(PRESENTATION);
        assert_eq!(from_bytes(&main_part_package(b"<main/>", Some(&types))), Some(Format::Pptx));
        let padded = format!("{types}{}", " ".repeat(METADATA_BYTES as usize));
        assert_eq!(from_bytes(&main_part_package(b"<main/>", Some(&padded))), None);

        let docx = |mimetype: &[u8]| {
            zip_package(&[("mimetype", mimetype), ("word/document.xml", b"<d/>".as_slice())])
        };
        assert_eq!(from_bytes(&docx(b"application/zip")), None);
        let oversized = [b' '; MIMETYPE_BYTES as usize + 1];
        assert_eq!(from_bytes(&docx(&oversized)), Some(Format::Docx));
    }

    #[test]
    fn a_label_the_content_contradicts_is_a_mismatch() {
        let parse_ctx = ParseCtx::default();
        let mismatch = |label, bytes: &[u8]| match check_label(&parse_ctx, bytes, label) {
            Err(ConvertError::Unsupported {
                kind: UnsupportedKind::FormatMismatch { detected },
            }) => Some(detected),
            Ok(()) => None,
            Err(e) => panic!("{e}"),
        };
        let rtf = b"{\\rtf1\\ansi hi}";
        assert_eq!(mismatch(Format::Doc, rtf), Some(Format::Rtf));
        assert_eq!(mismatch(Format::Rtf, rtf), None);
        let ods = zip_package(&[("mimetype", "application/vnd.oasis.opendocument.spreadsheet")]);
        assert_eq!(mismatch(Format::Odt, &ods), Some(Format::Ods));
        assert_eq!(mismatch(Format::Ods, &ods), None);
        assert_eq!(mismatch(Format::Excel, &ole_of(&["WordDocument"])), Some(Format::Doc));
        assert_eq!(mismatch(Format::Docx, b"not a document"), None);

        let quoted = b"name,note\nreport,\"%PDF-1.7 header\"\n";
        assert_eq!(mismatch(Format::Csv, quoted), None);
        assert_eq!(mismatch(Format::Docx, quoted), Some(Format::Pdf));
        assert_eq!(mismatch(Format::Csv, b"%PDF-1.7\n"), Some(Format::Pdf));
    }

    #[test]
    fn the_label_check_runs_on_the_parses_own_context() {
        let docx =
            main_part_package(format!(r#"<w:document xmlns:w="{}"/>"#, ns::W).as_bytes(), None);
        let token = CancelToken::new();
        let cancelled = cancelled(&token);
        assert_matches!(check_label(&cancelled, &docx, Format::Docx), Err(ConvertError::Cancelled));

        for (limits, limit) in [
            (Limits { max_entry_count: 1, ..Limits::DEFAULT }, crate::Limit::EntryCount),
            (Limits { max_xml_nodes: 1, ..Limits::DEFAULT }, crate::Limit::XmlNodes),
        ] {
            let lowered = ParseCtx::new(&limits);
            assert_matches!(
                check_label(&lowered, &docx, Format::Docx),
                Err(ConvertError::ResourceLimit { limit: crossed, max: 1 }) if crossed == limit
            );
        }
        assert_eq!(from_bytes(&docx), Some(Format::Docx));

        let fails = |limits: &Limits, bytes: &[u8], label, limit| {
            let lowered = ParseCtx::new(limits);
            match check_label(&lowered, bytes, label) {
                Err(ConvertError::ResourceLimit { limit: crossed, .. }) => {
                    assert_eq!(crossed, limit);
                }
                other => panic!("{other:?}"),
            }
        };
        let no_work = Limits { max_work: 0, ..Limits::DEFAULT };
        let mimetype = zip_package(&[("mimetype", "application/vnd.oasis.opendocument.text")]);
        fails(&no_work, &mimetype, Format::Odt, crate::Limit::Work);
        let manifest = zip_package(&[("META-INF/manifest.xml", "<manifest/>")]);
        fails(&no_work, &manifest, Format::Odt, crate::Limit::Work);

        let types = types_naming("application/xml");
        let two_nodes = Limits { max_xml_nodes: 2, ..Limits::DEFAULT };
        fails(
            &two_nodes,
            &main_part_package(b"<main/>", Some(&types)),
            Format::Docx,
            crate::Limit::XmlNodes,
        );

        let unnamed = main_part_package(b"<main/>", None);
        let parse_ctx = ParseCtx::default();
        check_label(&parse_ctx, &unnamed, Format::Docx).unwrap();
        let before_the_head = Limits { max_work: parse_ctx.fuel() - 1, ..Limits::DEFAULT };
        fails(&before_the_head, &unnamed, Format::Docx, crate::Limit::Work);
    }

    #[test]
    fn an_unreadable_or_ill_formed_identity_part_reads_as_absent() {
        let rels = rels_naming("word/document.xml");
        let ill_formed: &[u8] = b"<Types><!DOCTYPE x [<!ENTITY a \"b\">]>";
        let corrupt = |mut pkg: Vec<u8>, part: &[u8]| {
            let at = pkg.windows(part.len()).position(|w| w == part).unwrap();
            pkg[at] ^= 1;
            pkg
        };
        let types = types_naming("application/xml");
        let main: &[u8] = b"<main-part/>";
        let epub: &[u8] = b"application/epub+zip";
        for pkg in [
            zip_package_with(
                &[
                    ("_rels/.rels", rels.as_bytes()),
                    ("[Content_Types].xml", ill_formed),
                    ("word/document.xml", main),
                ],
                stored(),
            ),
            corrupt(
                zip_package_with(
                    &[
                        ("_rels/.rels", rels.as_bytes()),
                        ("[Content_Types].xml", types.as_bytes()),
                        ("word/document.xml", main),
                    ],
                    stored(),
                ),
                types.as_bytes(),
            ),
            corrupt(
                zip_package_with(
                    &[("_rels/.rels", rels.as_bytes()), ("word/document.xml", main)],
                    stored(),
                ),
                main,
            ),
            corrupt(
                zip_package_with(&[("mimetype", epub), ("word/document.xml", main)], stored()),
                epub,
            ),
        ] {
            check_label(&ParseCtx::default(), &pkg, Format::Docx).unwrap();
            assert_eq!(from_bytes(&pkg), Some(Format::Docx));
        }
    }

    #[test]
    fn odf_identity_is_read_before_opc_folds_part_names() {
        let ods = zip_package(&[
            ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
            ("Pictures/A.png", "a"),
            ("Pictures/a.png", "b"),
        ]);
        assert_eq!(from_bytes(&ods), Some(Format::Ods));
        assert_matches!(
            check_label(&ParseCtx::default(), &ods, Format::Odt),
            Err(ConvertError::Unsupported {
                kind: UnsupportedKind::FormatMismatch { detected: Format::Ods }
            })
        );
        let manifest = manifest_naming("application/vnd.oasis.opendocument.text");
        let odt =
            zip_package(&[("META-INF/manifest.xml", manifest.as_str()), ("A", "a"), ("a", "b")]);
        assert_eq!(from_bytes(&odt), Some(Format::Odt));
    }

    #[test]
    fn odf_and_epub_fallbacks_match_part_names_exactly_after_opc_finds_nothing() {
        let types = types_naming("application/xml");
        let manifest = manifest_naming("application/vnd.oasis.opendocument.text");
        let odt = zip_package(&[
            ("[Content_Types].xml", types.as_str()),
            ("meta-inf/manifest.xml", manifest.as_str()),
        ]);
        assert_eq!(from_bytes(&odt), None);
        let epub = zip_package(&[
            ("[Content_Types].xml", types.as_str()),
            ("META-INF/CONTAINER.XML", "<container/>"),
        ]);
        assert_eq!(from_bytes(&epub), None);
    }

    #[test]
    fn identity_reads_stay_within_the_entry_and_total_budgets() {
        let types = types_naming(PRESENTATION);
        let pkg = main_part_package(b"<main/>", Some(&types));
        let detected = |limits: &Limits| detect(&ParseCtx::new(limits), &pkg).unwrap();
        assert_eq!(detected(&Limits::DEFAULT), Some(Format::Pptx));
        let rels_part = rels_naming("custom/main.xml");
        let rels = u64::try_from(rels_part.len()).unwrap();
        let types = u64::try_from(types.len()).unwrap();
        assert!(types > rels);
        let fits = Limits {
            max_entry_bytes: types,
            max_total_bytes: types + rels + 100,
            ..Limits::DEFAULT
        };
        assert_eq!(detected(&fits), Some(Format::Pptx));
        assert_eq!(detected(&Limits { max_entry_bytes: types - 1, ..fits }), None);
        assert_eq!(detected(&Limits { max_total_bytes: types + 1, ..fits }), None);

        let manifest = manifest_naming("application/vnd.oasis.opendocument.text");
        let pkg = zip_package(&[
            ("_rels/.rels", rels_part.as_str()),
            ("META-INF/manifest.xml", manifest.as_str()),
        ]);
        let both = rels + u64::try_from(manifest.len()).unwrap();
        let total = |max_total_bytes| Limits { max_total_bytes, ..Limits::DEFAULT };
        assert_eq!(detect(&ParseCtx::new(&total(both)), &pkg).unwrap(), Some(Format::Odt));
        assert_eq!(detect(&ParseCtx::new(&total(both - 1)), &pkg).unwrap(), None);
    }
}
