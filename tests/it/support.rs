//! Helpers shared by the integration tests.

use std::io::{Cursor, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use crate::build::epub_bare;
use almiladoc::model::{Block, CellSlot, Inline};
use almiladoc::{
    Config, ConvertError, Converter, Diagnostic, DiagnosticCode, Diagnostics, Format, ImagePolicy,
    Input, Limit, Limits, OnLoss, Region,
};

pub(crate) const POLICIES: [ImagePolicy; 3] =
    [ImagePolicy::Placeholder, ImagePolicy::AltText, ImagePolicy::Omit];

pub(crate) fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

pub(crate) fn fixture(path: &str) -> Vec<u8> {
    std::fs::read(fixture_root().join(path)).unwrap()
}

/// Recursively collect every file under `dir`, sorted for determinism.
pub(crate) fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
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
pub(crate) fn corpus() -> Vec<PathBuf> {
    corpus_of(&fixture_root())
}

/// The fixture's format: detected from its content, else named by its
/// extension.
pub(crate) fn format_of(path: &Path, bytes: &[u8]) -> Option<Format> {
    Format::sniff(bytes)
        .or_else(|| path.extension().and_then(|ext| ext.to_str()).and_then(Format::from_extension))
}

pub(crate) fn configured(set: impl FnOnce(&mut Config)) -> Converter {
    let mut config = Config::default();
    set(&mut config);
    Converter::new(config).unwrap()
}

/// A converter on `threads` threads with default limits.
pub(crate) fn converter(threads: usize) -> Converter {
    configured(|config| config.threads = threads)
}

pub(crate) fn reporting() -> Converter {
    configured(|config| config.on_loss = OnLoss::Report)
}

pub(crate) fn limited(set: impl FnOnce(&mut Limits)) -> Converter {
    configured(|config| {
        set(&mut config.limits);
        config.on_loss = OnLoss::Report;
    })
}

/// Panics for a limit no `Limits` field holds.
pub(crate) fn limit_mut(limits: &mut Limits, limit: Limit) -> &mut u64 {
    match limit {
        Limit::EntryBytes => &mut limits.max_entry_bytes,
        Limit::TotalBytes => &mut limits.max_total_bytes,
        Limit::EntryCount => &mut limits.max_entry_count,
        Limit::NestingDepth => &mut limits.max_nesting_depth,
        Limit::XmlNodes => &mut limits.max_xml_nodes,
        Limit::GridSlots => &mut limits.max_grid_slots,
        Limit::Expansion => &mut limits.max_expansion,
        Limit::ExpansionTextBytes => &mut limits.max_expansion_text_bytes,
        Limit::AssetTotalBytes => &mut limits.max_asset_total_bytes,
        Limit::RecordDepth => &mut limits.max_record_depth,
        Limit::Records => &mut limits.max_records,
        Limit::InputBytes => &mut limits.max_input_bytes,
        Limit::CompressionRatio => &mut limits.max_compression_ratio,
        Limit::OutputBytes => &mut limits.max_output_bytes,
        Limit::Work => &mut limits.max_work,
        Limit::Pages => &mut limits.max_pages,
        other => panic!("{other} has no Limits field"),
    }
}

pub(crate) fn trips<T>(result: &Result<T, ConvertError>, limit: Limit, max: u64) -> bool {
    matches!(
        result,
        Err(ConvertError::ResourceLimit { limit: l, max: m, .. }) if *l == limit && *m == max
    )
}

pub(crate) fn reported(
    diagnostics: &Diagnostics,
) -> Vec<(DiagnosticCode, Region, Option<u32>, u64)> {
    diagnostics.iter().map(|d| (d.code, d.region, d.unit.map(NonZeroU32::get), d.count)).collect()
}

pub(crate) fn count(diagnostics: &Diagnostics, keep: impl Fn(&Diagnostic) -> bool) -> u64 {
    diagnostics.iter().filter(keep).map(|d| d.count).sum()
}

/// Every inline of `blocks` in document order, a link standing for its content.
pub(crate) fn inlines(blocks: &[Block]) -> Vec<&Inline> {
    fn leaves(inline: &Inline) -> Vec<&Inline> {
        match inline {
            Inline::Link { content, .. } => content.iter().flat_map(leaves).collect(),
            _ => vec![inline],
        }
    }
    blocks
        .iter()
        .flat_map(|block| match block {
            Block::Paragraph(content) | Block::Heading { content, .. } => {
                content.iter().flat_map(leaves).collect()
            }
            Block::BlockQuote(inner) => inlines(inner),
            Block::List(list) => list.items.iter().flat_map(|item| inlines(&item.blocks)).collect(),
            Block::Table(table) => table
                .grid
                .iter()
                .flatten()
                .flat_map(|slot| match slot {
                    CellSlot::Origin(cell) => inlines(&cell.blocks),
                    _ => Vec::new(),
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

pub(crate) fn zip_package<N: AsRef<str>, B: AsRef<[u8]>>(parts: &[(N, B)]) -> Vec<u8> {
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
    let _: std::io::Result<()> = std::fs::remove_dir_all(&root);
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

#[test]
fn inlines_reach_an_image_in_every_container() {
    let image = r#"<img src="missing.png"/>"#;
    for body in [
        format!("<p>{image}</p>"),
        format!("<h1>{image}</h1>"),
        format!(r##"<p><a href="#x">{image}</a></p>"##),
        format!("<blockquote><p>{image}</p></blockquote>"),
        format!("<ul><li>{image}</li></ul>"),
        format!("<table><tr><td>{image}</td></tr></table>"),
    ] {
        let parsed = reporting().parse(Input::new(&epub_bare(&body), Format::Epub)).unwrap();
        let images = inlines(&parsed.document().blocks)
            .into_iter()
            .filter(|inline| matches!(inline, Inline::Image { .. }));
        assert_eq!(images.count(), 1, "{body}");
    }
}
