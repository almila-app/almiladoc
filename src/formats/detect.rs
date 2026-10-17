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
//!   or generic).
//!
//! A package is read only as far as its identity: at most
//! [`MIMETYPE_BYTES`] of `mimetype`, [`METADATA_BYTES`] of each of
//! `_rels/.rels`, `[Content_Types].xml` and `META-INF/manifest.xml`, and the
//! first [`ROOT_TAG_BYTES`] of the main part, each also within the entry
//! and total byte budgets, which count these reads with the parse's own. A
//! metadata part past its cap or budget reads as absent, whatever size it
//! declares; the main part is scanned only as far as its cap and the
//! budgets allow.
//!
//! Plain-text formats (CSV) carry no signature and are never detected.
//! Detection never fails a sniff: any unreadable or ambiguous container
//! yields `None`. The label check runs it on the parse's own context, so
//! a limit or cancellation there fails the parse.

use crate::Format;
use crate::ctx::ParseCtx;
use crate::error::{ConvertError, UnsupportedKind};
use crate::limits::Limits;
use crate::package::path::resolve;
use crate::package::relationships::{Relationships, rel_type};
use crate::package::xml::{Element, ns, root_namespace};
use crate::package::{Package, PackageKind};
use std::io::Cursor;

const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const PDF_MAGIC: &[u8] = b"%PDF-";
const CT_NS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const MANIFEST_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";
const SPREADSHEET_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

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
    parse_ctx: &ParseCtx,
    bytes: &[u8],
    label: Format,
) -> Result<(), ConvertError> {
    match detect(parse_ctx, bytes)? {
        Some(Format::Pdf) if label == Format::Csv && !bytes.starts_with(PDF_MAGIC) => Ok(()),
        Some(detected) if detected != label => {
            Err(ConvertError::unsupported(UnsupportedKind::FormatMismatch { detected }))
        }
        _ => Ok(()),
    }
}

/// The format the content is, `None` when it names none; only a fatal error
/// fails it.
fn detect(parse_ctx: &ParseCtx, bytes: &[u8]) -> Result<Option<Format>, ConvertError> {
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
    let pdf = head.windows(PDF_MAGIC.len()).any(|w| w == PDF_MAGIC);
    Ok(pdf.then_some(Format::Pdf))
}

/// Classify an OLE compound file by its mandated content stream. Encrypted
/// OOXML packages (`EncryptedPackage`) stay `None`: the inner format is
/// unknowable, and the frontend reports `Encrypted` precisely.
fn detect_ole(bytes: &[u8]) -> Option<Format> {
    let ole = cfb::CompoundFile::open(Cursor::new(bytes)).ok()?;
    // Stream-name comparison is case-insensitive, matching CFB's own
    // uppercase name comparisons; producers vary (`WORKBOOK`, `BOOK`).
    let mut found = None;
    for entry in ole.read_root_storage() {
        let name = entry.name();
        if name.eq_ignore_ascii_case("WordDocument") {
            found = Some(Format::Doc);
        } else if name.eq_ignore_ascii_case("PowerPoint Document") {
            found = Some(Format::Ppt);
        } else if name.eq_ignore_ascii_case("Workbook") || name.eq_ignore_ascii_case("Book") {
            found = Some(Format::Excel);
        }
        if found.is_some() {
            break;
        }
    }
    found
}

fn detect_zip(parse_ctx: &ParseCtx, bytes: &[u8]) -> Result<Option<Format>, ConvertError> {
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

fn opened<'a>(pkg: Result<Package<'a>, ConvertError>) -> Result<Option<Package<'a>>, ConvertError> {
    match pkg {
        Ok(pkg) => Ok(Some(pkg)),
        Err(e) if e.is_fatal() => Err(e),
        Err(_) => Ok(None),
    }
}

fn opc_format_of(pkg: &Package) -> Result<Option<Format>, ConvertError> {
    // The package-level officeDocument relationship designates the main
    // part; its content type names the document kind.
    let rels = pkg.probe_xml_part("_rels/.rels", METADATA_BYTES)?;
    let rels = rels.as_ref().map(Relationships::from_root).unwrap_or_default();
    if let Some(rel) = rels.first_of_type(rel_type::OFFICE_DOCUMENT)
        && let Ok(target) = resolve("", &rel.target)
        && let Some(format) = main_part_format(pkg, &target.path)?
    {
        return Ok(Some(format));
    }

    // Packages with no usable rels: conventional main-part locations.
    Ok([
        ("word/document.xml", Format::Docx),
        ("ppt/presentation.xml", Format::Pptx),
        ("xl/workbook.xml", Format::Excel),
        ("xl/workbook.bin", Format::Excel),
    ]
    .into_iter()
    .find_map(|(part, format)| pkg.has_part(part).then_some(format)))
}

fn main_part_format(pkg: &Package, part: &str) -> Result<Option<Format>, ConvertError> {
    if let Some(types) = pkg.probe_xml_part("[Content_Types].xml", METADATA_BYTES)?
        && let Some(format) = content_type_of(&types, part).as_deref().and_then(opc_format)
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
    // Binary main parts (`xl/workbook.bin`) and unreadable ones: the
    // conventional locations the frontends also fall back to.
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
fn content_type_of(types: &Element, part: &str) -> Option<String> {
    let part_name = format!("/{part}");
    if let Some(ct) = types
        .descendants(CT_NS, "Override")
        .find(|e| e.attr_any("PartName").is_some_and(|p| p.eq_ignore_ascii_case(&part_name)))
        .and_then(|e| e.attr_any("ContentType"))
    {
        return Some(ct.to_string());
    }
    let ext = part.rsplit_once('.').map(|(_, e)| e)?;
    types
        .descendants(CT_NS, "Default")
        .find(|e| e.attr_any("Extension").is_some_and(|x| x.eq_ignore_ascii_case(ext)))
        .and_then(|e| e.attr_any("ContentType"))
        .map(str::to_string)
}

/// Map a main-part content type onto its parser. The family segment covers
/// every variant (document, template, slideshow); the `ms-word`,
/// `ms-powerpoint` and `ms-excel` families are the macro-enabled ones, and
/// `ms-excel` also the binary `.xlsb` main part.
fn opc_format(content_type: &str) -> Option<Format> {
    let ct = content_type.to_ascii_lowercase();
    Some(if ct.contains("wordprocessingml") || ct.contains("ms-word") {
        Format::Docx
    } else if ct.contains("presentationml") || ct.contains("ms-powerpoint") {
        Format::Pptx
    } else if ct.contains("spreadsheetml") || ct.contains("ms-excel") {
        Format::Excel
    } else {
        return None;
    })
}

/// Each OOXML main part has a mandated root element; its namespace,
/// normalized from Strict to Transitional, names the format.
fn namespace_format(namespace: &str) -> Option<Format> {
    match namespace {
        ns::W => Some(Format::Docx),
        ns::P => Some(Format::Pptx),
        SPREADSHEET_NS => Some(Format::Excel),
        _ => None,
    }
}

fn opc_format_by_path(part: &str) -> Option<Format> {
    Some(if part.starts_with("word/") {
        Format::Docx
    } else if part.starts_with("ppt/") {
        Format::Pptx
    } else if part.starts_with("xl/") {
        Format::Excel
    } else {
        return None;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::CancelToken;
    use crate::shared::test_support::zip_package;
    use std::assert_matches;
    use std::io::Write;

    const RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const OFFICE_DOCUMENT: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";

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
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="/custom/main.xml"/>
        </Relationships>"#;
        let types = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
            <Override PartName="/custom/main.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
        </Types>"#;
        let pkg = zip_package(&[
            ("_rels/.rels", rels),
            ("[Content_Types].xml", types),
            ("custom/main.xml", "<p/>"),
        ]);
        assert_eq!(from_bytes(&pkg), Some(Format::Pptx));
    }

    #[test]
    fn opc_stale_content_types_defer_to_the_root_element() {
        // The Override names a part that does not exist; the real main part
        // only gets the generic Default. Its w:document root decides.
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="content/main.xml"/>
        </Relationships>"#;
        let types = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
            <Default Extension="xml" ContentType="application/xml"/>
            <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
        </Types>"#;
        let main = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body/></w:document>"#;
        let pkg = zip_package(&[
            ("_rels/.rels", rels),
            ("[Content_Types].xml", types),
            ("content/main.xml", main),
        ]);
        assert_eq!(from_bytes(&pkg), Some(Format::Docx));
    }

    #[test]
    fn opc_falls_back_to_conventional_paths() {
        // No content types: the main-part path decides.
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
        </Relationships>"#;
        let pkg = zip_package(&[("_rels/.rels", rels), ("word/document.xml", "<d/>")]);
        assert_eq!(from_bytes(&pkg), Some(Format::Docx));

        // No rels at all: conventional part existence decides.
        let pkg = zip_package(&[("xl/workbook.bin", "\0")]);
        assert_eq!(from_bytes(&pkg), Some(Format::Excel));
    }

    #[test]
    fn broken_odf_and_epub_fall_back_to_manifest_and_container() {
        let manifest = r#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0">
            <manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/>
        </manifest:manifest>"#;
        let pkg = zip_package(&[("META-INF/manifest.xml", manifest), ("content.xml", "<c/>")]);
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
        ] {
            let pkg = main_part_package(b"<main/>", Some(&types_naming(content_type)));
            assert_eq!(from_bytes(&pkg), Some(format), "{content_type}");
        }
        let generic = types_naming("application/xml");
        assert_eq!(from_bytes(&main_part_package(b"<main/>", Some(&generic))), None);
    }

    #[test]
    fn the_root_element_is_read_from_the_head_of_the_main_part() {
        let strict = br#"<?xml version="1.0"?><!-- p --><w:document xmlns:w="http://purl.oclc.org/ooxml/wordprocessingml/main"><w:body>"#;
        assert_eq!(from_bytes(&main_part_package(strict, None)), Some(Format::Docx));
        let workbook =
            br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"/>"#;
        assert_eq!(from_bytes(&main_part_package(workbook, None)), Some(Format::Excel));
        let unbound = br#"<p:presentation><p:sldIdLst/>"#;
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
        let ct =
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml";
        let types = types_naming(ct);
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
        token.cancel();
        let cancelled = ParseCtx::default().with_cancel(Some(&token));
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
                    assert_eq!(crossed, limit)
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
        let ill_formed = b"<Types><!DOCTYPE x [<!ENTITY a \"b\">]>";
        let corrupt = |mut pkg: Vec<u8>, part: &[u8]| {
            let at = pkg.windows(part.len()).position(|w| w == part).unwrap();
            pkg[at] ^= 1;
            pkg
        };
        let stored = |parts: &[(&str, &[u8])]| {
            let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, bytes) in parts {
                w.start_file(*name, options).unwrap();
                w.write_all(bytes).unwrap();
            }
            w.finish().unwrap().into_inner()
        };
        let types = types_naming("application/xml");
        let main = b"<main-part/>";
        for pkg in [
            stored(&[
                ("_rels/.rels", rels.as_bytes()),
                ("[Content_Types].xml", ill_formed),
                ("word/document.xml", main),
            ]),
            corrupt(
                stored(&[
                    ("_rels/.rels", rels.as_bytes()),
                    ("[Content_Types].xml", types.as_bytes()),
                    ("word/document.xml", main),
                ]),
                types.as_bytes(),
            ),
            corrupt(stored(&[("_rels/.rels", rels.as_bytes()), ("word/document.xml", main)]), main),
            corrupt(
                stored(&[("mimetype", b"application/epub+zip"), ("word/document.xml", main)]),
                b"application/epub+zip",
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
        let manifest = r#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/></manifest:manifest>"#;
        let odt = zip_package(&[("META-INF/manifest.xml", manifest), ("A", "a"), ("a", "b")]);
        assert_eq!(from_bytes(&odt), Some(Format::Odt));
    }

    #[test]
    fn odf_and_epub_fallbacks_match_part_names_exactly_after_opc_finds_nothing() {
        let types = types_naming("application/xml");
        let manifest = r#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/></manifest:manifest>"#;
        let odt = zip_package(&[
            ("[Content_Types].xml", types.as_str()),
            ("meta-inf/manifest.xml", manifest),
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
        let ct =
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml";
        let types = types_naming(ct);
        let pkg = main_part_package(b"<main/>", Some(&types));
        let detected = |limits: &Limits| detect(&ParseCtx::new(limits), &pkg).unwrap();
        assert_eq!(detected(&Limits::DEFAULT), Some(Format::Pptx));
        let rels = u64::try_from(rels_naming("custom/main.xml").len()).unwrap();
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

        let rels_part = rels_naming("custom/main.xml");
        let manifest = r#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/></manifest:manifest>"#;
        let pkg = zip_package(&[
            ("_rels/.rels", rels_part.as_str()),
            ("META-INF/manifest.xml", manifest),
        ]);
        let both = rels + u64::try_from(manifest.len()).unwrap();
        let total = |max_total_bytes| Limits { max_total_bytes, ..Limits::DEFAULT };
        assert_eq!(detect(&ParseCtx::new(&total(both)), &pkg).unwrap(), Some(Format::Odt));
        assert_eq!(detect(&ParseCtx::new(&total(both - 1)), &pkg).unwrap(), None);
    }
}
