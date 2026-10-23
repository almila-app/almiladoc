//! Fixture corpus snapshot harness.
//!
//! One snapshot per fixture of the single conversion result under
//! `OnLoss::Report`: the Markdown output, or the terminal error. A fixture
//! with diagnostics has a second snapshot, `<name>.diagnostics`, of each
//! diagnostic and the verdict under `OnLoss::Fail`. Logs are never
//! snapshotted. A reader that panics fails the sweep. Fixtures under
//! `abuse/` are resource-abuse shapes, each asserted against its limit in
//! `tests/it/limits.rs`, not by the corpus sweep. Malformed fixtures encode their
//! expected outcome as `name--<outcome>.ext` where outcome is one of
//! `recovers`, `skips`, `ignores`, `errors`.

use crate::support::{self, converter, fixture, fixture_root, format_of, inlines, reporting, walk};
use almiladoc::model::{Block, Document, ImageSource, Inline, inlines_are_empty};
use almiladoc::{Conversion, ConvertError, Diagnostics, Format, Input};
use pulldown_cmark::{Event, Options, Parser, Tag};
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event as XmlEvent};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use zip::ZipArchive;

fn convert_file(path: &Path) -> Result<Conversion, ConvertError> {
    let bytes = std::fs::read(path).unwrap();
    let format = format_of(path, &bytes).unwrap();
    reporting().convert(Input::new(&bytes, format))
}

fn document(rel: &str, format: Format) -> Document {
    converter(1).parse(Input::new(&fixture(rel), format)).unwrap().into_document()
}

/// The Markdown and diagnostics of a conversion, or the error it ended in;
/// the pool reports a panic in a reader as `Internal`, which fails the sweep.
fn outcome(name: &str, result: Result<Conversion, ConvertError>) -> (String, Diagnostics) {
    match result {
        Ok(conversion) => (conversion.markdown().to_string(), conversion.diagnostics().clone()),
        Err(ConvertError::Internal) => panic!("{name}: a reader panicked"),
        Err(e) => (format!("ERROR: {e:#}"), Diagnostics::default()),
    }
}

fn assert_snapshot(name: &str, value: &str) {
    let mut settings = insta::Settings::clone_current();
    settings.set_snapshot_path("../snapshots");
    settings.set_prepend_module_to_snapshot(false);
    settings.bind(|| insta::assert_snapshot!(format!("snapshots__{name}"), value));
}

/// Each diagnostic on a line, then what `OnLoss::Fail` makes of them.
fn diagnostics_snapshot(diagnostics: &Diagnostics) -> String {
    let mut out = String::new();
    for d in diagnostics.iter() {
        write!(out, "{} {} {}", d.code.as_str(), d.class.as_str(), d.region.as_str()).unwrap();
        if let Some(unit) = d.unit {
            write!(out, " unit={unit}").unwrap();
        }
        writeln!(out, " count={}", d.count).unwrap();
    }
    let verdict = if diagnostics.has_loss() { "incomplete" } else { "complete" };
    writeln!(out, "under fail: {verdict}").unwrap();
    out
}

fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("snapshots")
}

fn expected_outcome(path: &Path) -> Option<&'static str> {
    let stem = path.file_stem()?.to_str()?;
    let (_, outcome) = stem.rsplit_once("--")?;
    ["recovers", "skips", "ignores", "errors"].into_iter().find(|&label| label == outcome)
}

fn corpus_cases() -> Vec<(PathBuf, String)> {
    let root = fixture_root();
    support::corpus()
        .into_iter()
        .map(|path| {
            let name =
                path.strip_prefix(&root).unwrap().to_string_lossy().replace(['\\', '/'], "__");
            (path, name)
        })
        .collect()
}

fn is_pdf(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

fn sweep(pdf: bool) {
    let cases: Vec<_> =
        corpus_cases().into_iter().filter(|(path, _)| is_pdf(path) == pdf).collect();
    assert!(!cases.is_empty(), "no fixtures in this sweep");
    for (path, name) in cases {
        let (output, diagnostics) = outcome(&name, convert_file(&path));
        if let Some(expected) = expected_outcome(&path) {
            let is_err = output.starts_with("ERROR: ");
            match expected {
                "errors" => {
                    assert!(is_err, "{name}: annotated `errors` but converted successfully");
                }
                _ => assert!(!is_err, "{name}: annotated `{expected}` but failed: {output}"),
            }
        }
        assert_snapshot(&name, &output);
        let diagnostics_name = format!("{name}.diagnostics");
        if diagnostics.is_empty() {
            let stale = snapshot_dir().join(format!("snapshots__{diagnostics_name}.snap"));
            assert!(!stale.exists(), "{name} reports nothing, yet {} exists", stale.display());
        } else {
            assert_snapshot(&diagnostics_name, &diagnostics_snapshot(&diagnostics));
        }
    }
}

#[test]
#[should_panic(expected = "a reader panicked")]
fn an_internal_error_fails_the_sweep() {
    outcome("fixture.docx", Err(ConvertError::Internal));
}

/// Non-PDF output is byte-identical on every tested target.
#[test]
fn corpus() {
    sweep(false);
}

/// PDF output is pinned on the reference target alone.
#[test]
#[cfg(feature = "pdf")]
#[cfg_attr(
    not(all(target_arch = "x86_64", target_os = "linux", target_env = "gnu")),
    ignore = "PDF snapshots are byte-compared on x86_64-unknown-linux-gnu only"
)]
fn corpus_pdf() {
    sweep(true);
}

#[test]
fn snapshots_match_the_corpus() {
    let corpus: BTreeSet<String> = corpus_cases().into_iter().map(|(_, name)| name).collect();
    let actual: BTreeSet<String> = std::fs::read_dir(snapshot_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with("snapshots__sample__"))
        .collect();
    let markdown: BTreeSet<String> =
        corpus.iter().map(|name| format!("snapshots__{name}.snap")).collect();
    assert!(markdown.is_subset(&actual), "{:?}", markdown.difference(&actual));
    for file in actual.difference(&markdown) {
        let name = file
            .strip_prefix("snapshots__")
            .and_then(|name| name.strip_suffix(".diagnostics.snap"));
        assert!(name.is_some_and(|name| corpus.contains(name)), "{file} belongs to no fixture");
    }
}

#[test]
fn fixtures_detect_from_content() {
    let expected = [
        ("doc", Some(Format::Doc)),
        ("docx", Some(Format::Docx)),
        ("epub", Some(Format::Epub)),
        ("odp", Some(Format::Odp)),
        ("ods", Some(Format::Ods)),
        ("odt", Some(Format::Odt)),
        ("pdf", Some(Format::Pdf)),
        ("ppt", Some(Format::Ppt)),
        ("pptx", Some(Format::Pptx)),
        ("rtf", Some(Format::Rtf)),
        ("xls", Some(Format::Excel)),
        ("xlsb", Some(Format::Excel)),
        ("xlsx", Some(Format::Excel)),
        ("csv", None),
    ];
    let root = fixture_root();
    for (dir, format) in expected {
        let mut files = Vec::new();
        walk(&root.join(dir), &mut files);
        assert!(!files.is_empty(), "no fixtures under {dir}/");
        for path in files {
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(Format::sniff(&bytes), format, "detection mismatch for {}", path.display());
        }
    }
}

type Package = ZipArchive<Cursor<Vec<u8>>>;

const RETYPED: [&str; 4] = [
    "docx/handmade-altpath.docx",
    "pptx/handmade-altpath.pptx",
    "pptx/handmade-order.pptx",
    "pptx/handmade-strict.pptx",
];

fn open_package(path: &Path) -> Package {
    ZipArchive::new(Cursor::new(std::fs::read(path).unwrap())).unwrap()
}

fn part_text(package: &mut Package, name: &str) -> String {
    let mut text = String::new();
    if let Ok(mut part) = package.by_name(name) {
        part.read_to_string(&mut text).unwrap();
    }
    text
}

fn attributes(element: &BytesStart<'_>) -> BTreeMap<String, String> {
    let named = element.attributes().map(Result::unwrap);
    named
        .map(|attribute| {
            let value = attribute.normalized_value(XmlVersion::Implicit1_0).unwrap();
            (attribute.key.local_name().as_ref().to_owned(), value.into_owned())
        })
        .collect()
}

fn elements(xml: &str, local_name: &str) -> Vec<BTreeMap<String, String>> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut found = Vec::new();
    loop {
        match reader.read_event().unwrap() {
            XmlEvent::Eof => return found,
            XmlEvent::Start(e) | XmlEvent::Empty(e) if e.local_name().as_ref() == local_name => {
                found.push(attributes(&e));
            }
            _ => {}
        }
    }
}

fn resolve(base_dir: &str, target: &str) -> String {
    let (mut segments, relative) = match target.strip_prefix('/') {
        Some(absolute) => (Vec::new(), absolute),
        None => (base_dir.split('/').filter(|s| !s.is_empty()).collect(), target),
    };
    for segment in relative.split('/') {
        match segment {
            "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }
    segments.join("/")
}

fn related(package: &mut Package, source: &str, kind: &str) -> Vec<String> {
    let (dir, file) = source.rsplit_once('/').unwrap_or(("", source));
    let rels = format!("{dir}/_rels/{file}.rels");
    let xml = part_text(package, rels.trim_start_matches('/'));
    elements(&xml, "Relationship")
        .into_iter()
        .filter(|rel| rel["Type"].ends_with(kind))
        .map(|rel| resolve(dir, &rel["Target"]))
        .collect()
}

fn overridden(package: &mut Package) -> BTreeMap<String, String> {
    let types = part_text(package, "[Content_Types].xml");
    let parts = elements(&types, "Override");
    let typed = parts
        .into_iter()
        .map(|mut e| (e.remove("PartName").unwrap(), e.remove("ContentType").unwrap()));
    typed.map(|(part, ty)| (part.trim_start_matches('/').to_owned(), ty)).collect()
}

fn office_type(kind: &str) -> String {
    format!("application/vnd.openxmlformats-officedocument.{kind}+xml")
}

fn main_part_and_slides(package: &mut Package, main_kind: &str) -> Vec<(String, String)> {
    let mains = related(package, "", "/officeDocument");
    let slides: Vec<_> = mains.iter().flat_map(|main| related(package, main, "/slide")).collect();
    let typed_mains = mains.into_iter().map(|part| (part, office_type(main_kind)));
    typed_mains
        .chain(slides.into_iter().map(|part| (part, office_type("presentationml.slide"))))
        .collect()
}

#[test]
fn office_fixtures_type_their_main_part_and_slides() {
    let root = fixture_root();
    let main_kinds = [
        ("docx", "wordprocessingml.document.main"),
        ("pptx", "presentationml.presentation.main"),
        ("xlsx", "spreadsheetml.sheet.main"),
    ];
    for (dir, main_kind) in main_kinds {
        let mut files = Vec::new();
        walk(&root.join(dir), &mut files);
        for path in files {
            let mut package = open_package(&path);
            let typed = overridden(&mut package);
            for (part, expected) in main_part_and_slides(&mut package, main_kind) {
                let found = typed.get(&part).map(String::as_str);
                assert_eq!(
                    found,
                    Some(expected.as_str()),
                    "{}: Override for {part}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn retyped_fixtures_type_only_parts_they_hold() {
    for rel in RETYPED {
        let mut package = open_package(&fixture_root().join(rel));
        for part in overridden(&mut package).into_keys() {
            assert!(package.by_name(&part).is_ok(), "{rel}: Override for missing part {part}");
        }
    }
}

#[test]
#[cfg(feature = "pdf")]
fn scanned_pages_are_reported_not_dropped() {
    match convert_file(&fixture_root().join("pdf/handmade-mixed.pdf"))
        .map(Conversion::into_markdown)
    {
        Err(ConvertError::NeedsOcr { pages, page_count, .. }) => {
            assert_eq!((pages, page_count), (vec![2], 2));
        }
        other => panic!("expected NeedsOcr, got {other:?}"),
    }
}

#[test]
fn embedded_ole_payload_is_retained() {
    let doc = document("pptx/handmade-order.pptx", Format::Pptx);
    let ole = doc
        .assets
        .iter()
        .find(|a| a.media_type == "application/vnd.ms-ole-object")
        .expect("OLE payload retained as an asset");
    assert_eq!(ole.bytes, b"OLE-PAYLOAD-STAND-IN".repeat(4));
}

#[test]
fn docx_ole_preview_takes_the_place_and_the_payload_stays_unreferenced() {
    let doc = document("docx/handmade-ole.docx", Format::Docx);
    let placed: Vec<(&str, &str)> = inlines(&doc.blocks)
        .into_iter()
        .filter_map(|inline| match inline {
            Inline::Image { alt, source: ImageSource::Asset(id), .. } => {
                Some((alt.as_str(), doc.assets[id.0].media_type.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(placed, [("", "image/png")]);
    let ole = doc
        .assets
        .iter()
        .find(|a| a.media_type == "application/vnd.ms-ole-object")
        .expect("OLE payload retained as an asset");
    assert_eq!(ole.bytes, b"DOCX-OLE-PAYLOAD".repeat(4));
}

#[test]
fn repeated_part_references_convert_and_dedupe() {
    let doc = document("docx/handmade-manyrefs.docx", Format::Docx);
    assert_eq!(doc.assets.len(), 1, "one shared asset for seventy references");
}

/// The binary DOC inline picture (sprmCPicLocation -> PICF + OfficeArt in
/// the Data stream) is retained as an asset.
#[test]
fn doc_inline_picture_is_retained() {
    let doc = document("doc/text.doc", Format::Doc);
    assert!(
        doc.assets.iter().any(|a| a.media_type.starts_with("image/")),
        "expected an extracted picture asset, got {:?}",
        doc.assets.iter().map(|a| (&a.media_type, a.bytes.len())).collect::<Vec<_>>()
    );
}

#[test]
fn rtf_inline_picture_is_retained() {
    let doc = document("rtf/text.rtf", Format::Rtf);
    let png = doc.assets.iter().find(|a| a.media_type == "image/png");
    let png = png.expect("png pict retained as an asset");
    assert!(png.bytes.starts_with(&[0x89, b'P', b'N', b'G']), "payload decodes from hex");
}

/// Local-only sweep over the ~100 real-world files in `samples/` (gitignored).
/// Snapshots a compact digest (length + hash + head) per file so drift is
/// detected without committing megabytes of output. Run with:
/// `cargo test --test it samples_sweep -- --ignored`
#[test]
#[ignore = "requires the local gitignored samples/ directory"]
fn samples_sweep() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("samples");
    assert!(root.is_dir(), "samples/ is absent");
    let mut files = Vec::new();
    walk(&root, &mut files);
    for path in files {
        if is_pdf(&path) && !cfg!(feature = "pdf") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let (output, _) = outcome(&name, convert_file(&path));
        assert_snapshot(&format!("sample__{name}"), &digest(&output));
    }
}

fn digest(output: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash: String =
        Sha256::digest(output.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    let mut s = String::new();
    writeln!(s, "chars: {}", output.chars().count()).unwrap();
    writeln!(s, "sha256: {hash}").unwrap();
    writeln!(s, "--- head ---").unwrap();
    s.extend(output.chars().take(2000));
    s
}

fn block_assets(blocks: &[Block]) -> usize {
    inlines(blocks)
        .into_iter()
        .filter(|inline| matches!(inline, Inline::Image { source: ImageSource::Asset(_), .. }))
        .count()
}

fn is_empty_paragraph(block: &Block) -> bool {
    matches!(block, Block::Paragraph(content) if inlines_are_empty(content))
}

fn placed_assets(doc: &Document) -> usize {
    let mut ids = BTreeSet::new();
    let notes: usize = doc
        .notes
        .iter()
        .filter(|note| ids.insert(note.id.as_str()) && !note.blocks.iter().all(is_empty_paragraph))
        .map(|note| block_assets(&note.blocks))
        .sum();
    block_assets(&doc.blocks) + notes
}

fn rendered_assets(name: &str, markdown: &str, assets: usize) -> usize {
    let mut count = 0;
    for event in Parser::new_ext(markdown, Options::ENABLE_TABLES) {
        if let Event::Start(Tag::Image { dest_url, .. }) = event {
            let id: usize = dest_url
                .strip_prefix("asset:")
                .unwrap_or_else(|| panic!("{name}: fetchable image {dest_url}"))
                .parse()
                .unwrap();
            assert!(id < assets, "{name}: missing asset {id}");
            count += 1;
        }
    }
    count
}

#[test]
fn no_rendered_image_has_a_fetchable_destination() {
    for (path, name) in corpus_cases().into_iter().filter(|(path, _)| !is_pdf(path)) {
        let bytes = std::fs::read(&path).unwrap();
        let format = format_of(&path, &bytes).unwrap();
        let parsed = match reporting().parse(Input::new(&bytes, format)) {
            Ok(parsed) => parsed,
            Err(_) if expected_outcome(&path) == Some("errors") => continue,
            Err(error) => panic!("{name}: {error}"),
        };
        let doc = parsed.document();
        let markdown = parsed.render().unwrap().into_markdown();
        let count = rendered_assets(&name, &markdown, doc.assets.len());
        assert_eq!(count, placed_assets(doc), "{name}: lost or invented image marker");
    }
}
