//! Helpers shared by the integration-test binaries.

use std::path::{Path, PathBuf};

pub fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
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

/// The fixture's format: detected from its content, else named by its
/// extension.
pub fn format_of(path: &Path, bytes: &[u8]) -> Option<almiladoc::Format> {
    almiladoc::Format::from_bytes(bytes).or_else(|| {
        path.extension().and_then(|ext| ext.to_str()).and_then(almiladoc::Format::from_extension)
    })
}
