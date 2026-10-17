//! Helpers shared by the integration tests.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use almiladoc::{Config, Converter, Format};

pub fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

pub fn fixture(path: &str) -> Vec<u8> {
    std::fs::read(fixture_root().join(path)).unwrap()
}

/// Recursively collect every file under `dir`, sorted for determinism.
pub fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn corpus_of(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk(root, &mut files);
    files.retain(|path| path.strip_prefix(root).is_ok_and(|rel| !rel.starts_with("abuse")));
    files
}

/// Every committed fixture bar the resource-abuse shapes under `abuse/`.
pub fn corpus() -> Vec<PathBuf> {
    corpus_of(&fixture_root())
}

/// The fixture's format: detected from its content, else named by its
/// extension.
pub fn format_of(path: &Path, bytes: &[u8]) -> Option<Format> {
    Format::sniff(bytes)
        .or_else(|| path.extension().and_then(|ext| ext.to_str()).and_then(Format::from_extension))
}

/// A converter on `threads` threads with default limits.
pub fn converter(threads: usize) -> Converter {
    let mut config = Config::default();
    config.threads = threads;
    Converter::new(config).unwrap()
}

pub fn zip_package<N: AsRef<str>, B: AsRef<[u8]>>(parts: &[(N, B)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, body) in parts {
        writer.start_file(name.as_ref(), zip::write::SimpleFileOptions::default()).unwrap();
        writer.write_all(body.as_ref()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn the_corpus_leaves_out_abuse_by_position_not_by_spelling() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("abuse-root");
    let _ = std::fs::remove_dir_all(&root);
    for rel in ["abuse/over.bin", "docx/abuse.docx", "docx/kept.docx"] {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }
    let kept: Vec<_> = corpus_of(&root)
        .iter()
        .map(|path| path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"))
        .collect();
    assert_eq!(kept, ["docx/abuse.docx", "docx/kept.docx"]);
    std::fs::remove_dir_all(&root).unwrap();
}
