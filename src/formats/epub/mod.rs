//! EPUB: XHTML chapters in spine order, concatenated into one document with
//! chapter-scoped anchors so intra-book navigation survives.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{AnchorId, Block, Document, ImageSource, Inline, LinkTarget};
use crate::package::archive::{Role, report};
use crate::package::xml::Element;
use crate::package::{Package, PackageKind, path};
use crate::shared::assets::{AssetSink, media_type_for};
use crate::shared::html::{HtmlCtx, Stylesheet};
use crate::shared::uri::is_absolute_uri;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

pub fn parse(parse_ctx: &ParseCtx, bytes: &[u8]) -> Result<Document, ConvertError> {
    let pkg = RefCell::new(Package::open(parse_ctx, bytes, PackageKind::Epub)?);

    let container = pkg
        .borrow_mut()
        .required_xml_part("META-INF/container.xml", Role::Content(Region::Body))?;
    let opf_path = container
        .descendants_any("rootfile")
        .next()
        .and_then(|r| r.attr_any("full-path"))
        .ok_or_else(|| ConvertError::malformed_part("META-INF/container.xml", "no rootfile entry"))?
        .to_string();

    let opf = pkg.borrow_mut().required_xml_part(&opf_path, Role::Content(Region::Body))?;

    let mut doc = Document::default();
    if let Some(title) = opf.descendants_any("title").next().map(|t| t.text()) {
        let title = title.trim().to_string();
        if !title.is_empty() {
            doc.blocks.push(Block::heading(1, vec![Inline::plain(title)]));
        }
    }

    let mut manifest: HashMap<String, (String, String)> = HashMap::new();
    for item in opf.descendants_any("item") {
        if let (Some(id), Some(href)) = (item.attr_any("id"), item.attr_any("href")) {
            let media = item.attr_any("media-type").unwrap_or("").to_string();
            manifest.insert(id.to_string(), (href.to_string(), media));
        }
    }

    // Every spine part in spine order: non-linear items are auxiliary but
    // still publication content, and unusable parts degrade at parse time.
    // Intra-book links target these; links to any other resource stay
    // Relative.
    // A part holds one position in a reading order, and each repeat would
    // cost another parse of it and another copy of its anchor.
    let mut spine_entries = 0usize;
    let mut spine_paths: Vec<String> = Vec::new();
    let mut spine_parts: HashSet<String> = HashSet::new();
    for href in opf
        .descendants_any("itemref")
        .filter_map(|ir| ir.attr_any("idref"))
        .filter_map(|idref| manifest.get(idref))
        .map(|(href, _)| href.as_str())
    {
        spine_entries += 1;
        let target = match path::resolve(&opf_path, href) {
            Ok(t) => t,
            Err(_) => {
                parse_ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body);
                continue;
            }
        };
        if spine_parts.insert(target.path.clone()) {
            spine_paths.push(target.path);
        }
    }

    let assets = RefCell::new(AssetSink::new(parse_ctx));
    let mut sheets = Sheets::default();
    let mut converted = 0usize;
    for (position, chapter_path) in spine_paths.iter().enumerate() {
        let _unit = parse_ctx.enter_unit(position);
        let Some(tree) =
            pkg.borrow_mut().related_xml_part(chapter_path, Role::Reported(Region::Body))?
        else {
            parse_ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body);
            continue;
        };
        let Some(body) = tree
            .child_elems()
            .find(|e| e.local == "html")
            .and_then(|h| h.child_elems().find(|e| e.local == "body"))
        else {
            parse_ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body);
            continue;
        };
        let css = {
            let _shared = parse_ctx.outside_units();
            chapter_stylesheet(parse_ctx, &tree, chapter_path, &pkg, &mut sheets)?
        };
        let ctx = ChapterCtx {
            parse_ctx,
            pkg: &pkg,
            assets: &assets,
            chapter_path: chapter_path.clone(),
            spine_parts: &spine_parts,
        };
        // Chapter-start anchor: renders only when a link targets this chapter.
        doc.blocks.push(Block::Paragraph(vec![Inline::Anchor(chapter_path.clone())]));
        doc.blocks.extend(crate::shared::html::to_blocks(parse_ctx, body, &css, &ctx)?);
        converted += 1;
    }
    if spine_entries > 0 && converted == 0 {
        return Err(ConvertError::malformed("no chapter in the book could be read"));
    }

    doc.assets = std::mem::take(&mut assets.borrow_mut().assets);
    Ok(doc)
}

/// Linked stylesheets across chapters: each part is read, and each
/// unresolvable href reported, once.
#[derive(Default)]
struct Sheets {
    parts: HashMap<String, Option<String>>,
    unresolvable: HashSet<String>,
}

/// A chapter's CSS cascade: its linked stylesheets and inline `<style>`
/// blocks, in document order.
fn chapter_stylesheet(
    parse_ctx: &ParseCtx,
    tree: &Element,
    chapter_path: &str,
    pkg: &RefCell<Package>,
    sheets: &mut Sheets,
) -> Result<Stylesheet, ConvertError> {
    let mut css = Stylesheet::default();
    let mut stack: Vec<&Element> = tree.child_elems().collect();
    stack.reverse();
    while let Some(elem) = stack.pop() {
        match elem.local.as_str() {
            "link" => {
                let rel = elem.attr_any("rel").unwrap_or("");
                let is_sheet = rel.split_whitespace().any(|r| r.eq_ignore_ascii_case("stylesheet"));
                if is_sheet && let Some(href) = elem.attr_any("href") {
                    let Ok(target) = path::resolve(chapter_path, href) else {
                        if sheets.unresolvable.insert(href.to_string()) {
                            report(parse_ctx, Role::Formatting.unreadable());
                        }
                        continue;
                    };
                    if !sheets.parts.contains_key(&target.path) {
                        let text = pkg
                            .borrow_mut()
                            .related_part(&target.path, Role::Formatting)?
                            .map(|bytes| match std::str::from_utf8(&bytes) {
                                Ok(text) => text.to_string(),
                                Err(_) => {
                                    report(parse_ctx, Role::Formatting.undecodable());
                                    String::from_utf8_lossy(&bytes).into_owned()
                                }
                            });
                        sheets.parts.insert(target.path.clone(), text);
                    }
                    if let Some(Some(text)) = sheets.parts.get(&target.path) {
                        css.add(text);
                    }
                }
            }
            "style" => css.add(&elem.text()),
            _ => {
                let start = stack.len();
                stack.extend(elem.child_elems());
                stack[start..].reverse();
            }
        }
    }
    Ok(css)
}

struct ChapterCtx<'a, 'b> {
    parse_ctx: &'b ParseCtx<'a>,
    pkg: &'b RefCell<Package<'a>>,
    assets: &'b RefCell<AssetSink>,
    chapter_path: String,
    spine_parts: &'b HashSet<String>,
}

impl HtmlCtx for ChapterCtx<'_, '_> {
    fn link_target(&self, href: &str) -> Option<LinkTarget> {
        if href.is_empty() {
            return None;
        }
        if let Some(fragment) = href.strip_prefix('#') {
            let fragment = path::decode_fragment(fragment);
            return Some(LinkTarget::Anchor(scoped(&self.chapter_path, Some(&fragment))));
        }
        if is_absolute_uri(href) {
            return Some(LinkTarget::External(href.to_string()));
        }
        // Anchors only for converted spine documents; links to any other
        // package resource (images, downloads, non-linear content) keep
        // their relative form.
        match path::resolve(&self.chapter_path, href) {
            Ok(target) if self.spine_parts.contains(&target.path) => {
                Some(LinkTarget::Anchor(scoped(&target.path, target.fragment.as_deref())))
            }
            _ => Some(LinkTarget::Relative(href.to_string())),
        }
    }

    fn image_source(&self, src: &str) -> Result<Option<ImageSource>, ConvertError> {
        if src.is_empty() {
            return Ok(None);
        }
        if is_absolute_uri(src) {
            return Ok(Some(ImageSource::External(src.to_string())));
        }
        let bytes = match path::resolve(&self.chapter_path, src) {
            Ok(target) => self
                .pkg
                .borrow_mut()
                .related_part(&target.path, Role::Reported(Region::Body))?
                .map(|bytes| (target.path, bytes)),
            Err(_) => None,
        };
        let Some((path, bytes)) = bytes else {
            self.parse_ctx.emit(DiagnosticCode::ImageUnavailable, Region::Body);
            return Ok(None);
        };
        let media = media_type_for(&path);
        let id = self.assets.borrow_mut().add(media, path, &bytes)?;
        Ok(Some(ImageSource::Asset(id)))
    }

    fn anchor_id(&self, raw: &str) -> AnchorId {
        scoped(&self.chapter_path, Some(raw))
    }
}

/// Chapter-scoped anchor id: the chapter path itself targets the chapter
/// start; `path#fragment` targets an element inside it.
fn scoped(chapter_path: &str, fragment: Option<&str>) -> AnchorId {
    match fragment {
        Some(f) if !f.is_empty() => format!("{chapter_path}#{f}"),
        _ => chapter_path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    type Reported = (DiagnosticCode, Region, Option<u32>, u64);

    fn book(spine: &[&str], parts: &[(&str, &[u8])]) -> Vec<u8> {
        let items: String = spine
            .iter()
            .enumerate()
            .map(|(i, href)| {
                format!(r#"<item id="i{i}" href="{href}" media-type="application/xhtml+xml"/>"#)
            })
            .collect();
        let refs: String =
            (0..spine.len()).map(|i| format!(r#"<itemref idref="i{i}"/>"#)).collect();
        let container = r#"<?xml version="1.0"?>
            <container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
            <rootfiles><rootfile full-path="c.opf"/></rootfiles></container>"#;
        let opf = format!(
            r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata/>
            <manifest>{items}</manifest><spine>{refs}</spine></package>"#
        );
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let fixed: [(&str, &[u8]); 2] =
            [("META-INF/container.xml", container.as_bytes()), ("c.opf", opf.as_bytes())];
        for (name, body) in fixed.iter().chain(parts) {
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn chapter(head: &str, body: &str) -> Vec<u8> {
        format!(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head>{head}</head><body><p>text</p>{body}</body></html>"#
        )
        .into_bytes()
    }

    fn diagnose(epub: &[u8]) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, epub).unwrap();
        let unit = |d: &crate::diagnostics::Diagnostic| d.unit.map(std::num::NonZeroU32::get);
        parse_ctx.diagnostics().iter().map(|d| (d.code, d.region, unit(&d), d.count)).collect()
    }

    #[test]
    fn each_lost_site_reports_its_code_once_where_it_sits() {
        use DiagnosticCode::{ImageUnavailable, UnitUnreadable};
        const BODY: Region = Region::Body;
        let plain = chapter("", "");
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><text>page</text></svg>"#;
        let unresolvable = chapter("", r#"<img src="a%2Fb.png" alt="a"/>"#);
        let absent = chapter("", r#"<img src="gone.png"/>"#);
        let present = chapter("", r#"<img src="pic.png"/><img src="https://example.org/p.png"/>"#);
        let cases: Vec<(&str, Vec<u8>, Vec<Reported>)> = vec![
            (
                "chapter href unresolvable",
                book(&["a%2Fb.xhtml", "ch.xhtml"], &[("ch.xhtml", &plain)]),
                vec![(UnitUnreadable, BODY, None, 1)],
            ),
            (
                "chapter part absent",
                book(&["gone.xhtml", "ch.xhtml"], &[("ch.xhtml", &plain)]),
                vec![(UnitUnreadable, BODY, Some(1), 1)],
            ),
            (
                "spine item with no body",
                book(&["ch.xhtml", "page.svg"], &[("ch.xhtml", &plain), ("page.svg", svg)]),
                vec![(UnitUnreadable, BODY, Some(2), 1)],
            ),
            (
                "image src unresolvable",
                book(&["ch.xhtml"], &[("ch.xhtml", &unresolvable)]),
                vec![(ImageUnavailable, BODY, Some(1), 1)],
            ),
            (
                "image part absent",
                book(&["ch.xhtml"], &[("ch.xhtml", &absent)]),
                vec![(ImageUnavailable, BODY, Some(1), 1)],
            ),
            (
                "image present or outside the package",
                book(&["ch.xhtml"], &[("ch.xhtml", &present), ("pic.png", b"png")]),
                vec![],
            ),
        ];
        for (name, epub, expected) in cases {
            assert_eq!(diagnose(&epub), expected, "{name}");
        }
    }

    #[test]
    fn a_linked_stylesheet_reports_once_however_many_chapters_link_it() {
        use DiagnosticCode::{InvalidStyleEncoding, StylePartUnreadable};
        const CONTAINER: Region = Region::Container;
        let cases: [(&str, &str, &[u8], &[Reported]); 4] = [
            ("href unresolvable", "a%2Fb.css", b"", &[(StylePartUnreadable, CONTAINER, None, 1)]),
            ("part absent", "gone.css", b"", &[(StylePartUnreadable, CONTAINER, None, 1)]),
            (
                "not UTF-8",
                "s.css",
                b"p { font-weight: bold }\xff",
                &[(InvalidStyleEncoding, CONTAINER, None, 1)],
            ),
            ("UTF-8", "s.css", "p::before { content: \"\u{e9}\" }".as_bytes(), &[]),
        ];
        for (name, href, sheet, expected) in cases {
            let linking = chapter(&format!(r#"<link rel="stylesheet" href="{href}"/>"#), "");
            let epub = book(
                &["one.xhtml", "two.xhtml"],
                &[("one.xhtml", &linking), ("two.xhtml", &linking), ("s.css", sheet)],
            );
            assert_eq!(diagnose(&epub), expected, "{name}");
        }
    }

    #[test]
    fn a_part_repeated_across_the_spine_is_read_once() {
        let items: String = (0..64)
            .map(|i| {
                format!(r#"<item id="i{i}" href="ch.xhtml" media-type="application/xhtml+xml"/>"#)
            })
            .collect();
        let refs: String = (0..64).map(|i| format!(r#"<itemref idref="i{i}"/>"#)).collect();
        let parts = [
            (
                "META-INF/container.xml",
                r#"<?xml version="1.0"?>
                <container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
                <rootfiles><rootfile full-path="c.opf"
                media-type="application/oebps-package+xml"/></rootfiles></container>"#
                    .to_string(),
            ),
            (
                "c.opf",
                format!(
                    r#"<?xml version="1.0"?>
                    <package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata/>
                    <manifest>{items}</manifest><spine>{refs}</spine></package>"#
                ),
            ),
            (
                "ch.xhtml",
                r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml">
                <body><p>chapter text</p></body></html>"#
                    .to_string(),
            ),
        ];
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in &parts {
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }

        let doc = parse(&ParseCtx::default(), &w.finish().unwrap().into_inner()).unwrap();
        let text = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph(inlines) => Some(crate::model::inlines_to_plain_text(inlines)),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "chapter text", "sixty-four itemrefs naming one part");
    }
}
