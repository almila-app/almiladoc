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

use crate::support::{self, converter, fixture_root, format_of, walk};
use almiladoc::{
    Config, Conversion, ConvertError, Converter, Diagnostics, Format, Input, OnLoss,
    model::Document,
};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn convert_file(path: &Path) -> Result<Conversion, ConvertError> {
    let bytes = std::fs::read(path).unwrap();
    let format = format_of(path, &bytes).unwrap();
    let mut config = Config::default();
    config.on_loss = OnLoss::Report;
    Converter::new(config).unwrap().convert(Input::new(&bytes, format))
}

fn document(path: &Path, format: Format) -> Document {
    let bytes = std::fs::read(path).unwrap();
    converter(1).parse(Input::new(&bytes, format)).unwrap().into_document()
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
    match outcome {
        "recovers" => Some("recovers"),
        "skips" => Some("skips"),
        "ignores" => Some("ignores"),
        "errors" => Some("errors"),
        _ => None,
    }
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
                    assert!(is_err, "{name}: annotated `errors` but converted successfully")
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

/// Every snapshot belongs to a corpus fixture and none is pending review; a
/// fixture's diagnostics snapshot sits beside its Markdown one.
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

/// Every well-formed corpus fixture must be identified from its bytes
/// alone. Only the signature-less text formats (csv) legitimately need the
/// extension fallback.
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

/// A PDF with scanned pages is reported, not silently shortened: the error
/// names the pages that need OCR.
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

/// Embedded object payloads land in `Document::assets` with their identity
/// and media type (the Markdown output shows only the alt text).
#[test]
fn embedded_ole_payload_is_retained() {
    let doc = document(&fixture_root().join("pptx").join("handmade-order.pptx"), Format::Pptx);
    let ole = doc
        .assets
        .iter()
        .find(|a| a.media_type == "application/vnd.ms-ole-object")
        .expect("OLE payload retained as an asset");
    assert_eq!(ole.bytes, b"OLE-PAYLOAD-STAND-IN".repeat(4));
}

/// Standard Word OLE markup places a VML preview image next to the
/// `o:OLEObject`; the object payload (not the preview) must be retained.
#[test]
fn docx_ole_payload_wins_over_its_preview_image() {
    let doc = document(&fixture_root().join("docx").join("handmade-ole.docx"), Format::Docx);
    let ole = doc
        .assets
        .iter()
        .find(|a| a.media_type == "application/vnd.ms-ole-object")
        .expect("OLE payload retained as an asset");
    assert_eq!(ole.bytes, b"DOCX-OLE-PAYLOAD".repeat(4));
}

/// Repeated references to one part must neither re-decompress against the
/// archive budget nor duplicate the retained asset.
#[test]
fn repeated_part_references_convert_and_dedupe() {
    let doc = document(&fixture_root().join("docx").join("handmade-manyrefs.docx"), Format::Docx);
    assert_eq!(doc.assets.len(), 1, "one shared asset for seventy references");
}

/// The binary DOC inline picture (sprmCPicLocation -> PICF + OfficeArt in
/// the Data stream) is retained as an asset.
#[test]
fn doc_inline_picture_is_retained() {
    let doc = document(&fixture_root().join("doc").join("text.doc"), Format::Doc);
    assert!(
        doc.assets.iter().any(|a| a.media_type.starts_with("image/")),
        "expected an extracted picture asset, got {:?}",
        doc.assets.iter().map(|a| (&a.media_type, a.bytes.len())).collect::<Vec<_>>()
    );
}

/// The RTF `\pict` payload is retained as an asset (the Markdown output
/// shows only the alt text, which is empty for pictures without one).
#[test]
fn rtf_inline_picture_is_retained() {
    let doc = document(&fixture_root().join("rtf").join("text.rtf"), Format::Rtf);
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
    let head: String = output.chars().take(2000).collect();
    s.push_str(&head);
    s
}
