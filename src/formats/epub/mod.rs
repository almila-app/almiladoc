//! EPUB: XHTML chapters in spine order, concatenated into one document with
//! chapter-scoped anchors so intra-book navigation survives.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{AnchorId, Block, Document, ImageSource, Inline, LinkTarget, Unit, UnitKind};
use crate::package::archive::{Role, report};
use crate::package::xml::Element;
use crate::package::{Package, PackageKind, path};
use crate::shared::assets::{AssetSink, media_type_for};
use crate::shared::css::{self, Applies};
use crate::shared::html::{HtmlCtx, Stylesheet};
use crate::shared::uri::is_absolute_uri;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, hash_map::Entry};

pub(super) fn parse(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Document, ConvertError> {
    let pkg = Package::open(parse_ctx, bytes, PackageKind::Epub)?;

    let opf_path = rootfile_path(&pkg)?;
    let opf = pkg.required_xml_part(&opf_path, Role::Content(Region::Body))?;

    let mut doc = Document::default();
    let title = opf.descendants_any("title").next().map(Element::text);
    if let Some(title) = title.as_deref().map(str::trim).filter(|title| !title.is_empty()) {
        doc.blocks.push(Block::heading(1, vec![Inline::plain(title)]));
    }

    let manifest: HashMap<&str, Option<&str>> = opf
        .descendants_any("item")
        .filter_map(|item| Some((item.attr_any("id")?, item.attr_any("href"))))
        .collect();

    // A part repeated in the spine is read once: each repeat would cost another parse and copy.
    let mut spine_paths = Vec::new();
    let mut spine_units = HashMap::new();
    for (position, itemref) in opf.descendants_any("itemref").enumerate() {
        let scope = parse_ctx.begin_unit(UnitKind::SpineItem, position)?;
        let Some(target) = itemref
            .attr_any("idref")
            .and_then(|idref| manifest.get(idref).copied().flatten())
            .and_then(|href| path::resolve(&opf_path, href).ok())
        else {
            parse_ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body);
            continue;
        };
        if let Entry::Vacant(entry) = spine_units.entry(target.path.clone()) {
            entry.insert(scope.unit().model());
            spine_paths.push((scope.unit(), target.path));
        }
    }

    let assets = RefCell::new(AssetSink::new(parse_ctx));
    let mut sheets = Sheets::default();
    let mut converted = 0usize;
    for (admitted, chapter_path) in &spine_paths {
        let unit = parse_ctx.enter_admitted(*admitted);
        let tree = pkg.related_xml_part(chapter_path, Role::Reported(Region::Body))?;
        let Some((tree, body)) = tree.as_ref().and_then(|tree| Some((tree, chapter_body(tree)?)))
        else {
            parse_ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body);
            continue;
        };
        let cascade = {
            let _shared = parse_ctx.outside_units();
            chapter_stylesheet(parse_ctx, tree, chapter_path, &pkg, &mut sheets)?
        };
        parse_ctx.emit_count(
            DiagnosticCode::PartUnreadable,
            Region::Body,
            cascade.lost.len() as u64,
        );
        let ctx =
            ChapterCtx { pkg: &pkg, assets: &assets, chapter_path, spine_units: &spine_units };
        doc.blocks.push(Block::UnitStart(unit.unit().model()));
        doc.blocks.extend(crate::shared::html::to_blocks(parse_ctx, body, &cascade.css, &ctx)?);
        converted += 1;
    }
    if converted == 0 && opf.descendants_any("itemref").next().is_some() {
        return Err(ConvertError::malformed("no chapter in the book could be read"));
    }

    doc.assets = std::mem::take(&mut assets.borrow_mut().assets);
    Ok(doc)
}

fn rootfile_path(pkg: &Package<'_>) -> Result<String, ConvertError> {
    let container = pkg.required_xml_part("META-INF/container.xml", Role::Content(Region::Body))?;
    container
        .descendants_any("rootfile")
        .next()
        .and_then(|r| r.attr_any("full-path"))
        .map(str::to_string)
        .ok_or_else(|| ConvertError::malformed_part("META-INF/container.xml", "no rootfile entry"))
}

fn chapter_body(tree: &Element) -> Option<&Element> {
    let html = tree.child_elems().find(|e| e.local == "html")?;
    html.child_elems().find(|e| e.local == "body")
}

/// Linked stylesheets across chapters: each part is read, and each
/// unresolvable href reported, once.
#[derive(Default)]
struct Sheets {
    parts: HashMap<String, Sheet>,
    unresolvable: HashSet<String>,
}

/// A linked stylesheet's text, absent when its part is missing or will not read, and its id
/// when what it lost may hide text.
struct Sheet {
    text: Option<String>,
    lost: Option<usize>,
}

/// A chapter's CSS cascade: its linked stylesheets and inline `<style>`
/// blocks, in document order, and the lost sheets it links.
struct Cascade {
    css: Stylesheet,
    lost: HashSet<usize>,
}

fn chapter_stylesheet(
    parse_ctx: &ParseCtx<'_>,
    tree: &Element,
    chapter_path: &str,
    pkg: &Package<'_>,
    sheets: &mut Sheets,
) -> Result<Cascade, ConvertError> {
    let mut cascade = Cascade { css: Stylesheet::default(), lost: HashSet::new() };
    let mut stack: Vec<&Element> = tree.child_elems().collect();
    stack.reverse();
    while let Some(elem) = stack.pop() {
        match elem.local.as_str() {
            "link" => {
                if let Some(sheet) = linked_sheet(parse_ctx, elem, chapter_path, pkg, sheets)? {
                    if css::media_applies(media_of(elem)) != Applies::Never {
                        cascade.lost.extend(sheet.lost);
                    }
                    if let Some(text) = &sheet.text {
                        cascade.css.add(parse_ctx, text, media_of(elem))?;
                    }
                }
            }
            "style" => cascade.css.add(parse_ctx, &elem.text(), media_of(elem))?,
            _ => {
                let start = stack.len();
                stack.extend(elem.child_elems());
                stack[start..].reverse();
            }
        }
    }
    Ok(cascade)
}

fn media_of(elem: &Element) -> &str {
    elem.attr_any("media").unwrap_or_default()
}

fn linked_sheet<'s>(
    parse_ctx: &ParseCtx<'_>,
    link: &Element,
    chapter_path: &str,
    pkg: &Package<'_>,
    sheets: &'s mut Sheets,
) -> Result<Option<&'s Sheet>, ConvertError> {
    let is_sheet = link
        .attr_any("rel")
        .is_some_and(|rel| rel.split_whitespace().any(|r| r.eq_ignore_ascii_case("stylesheet")));
    let Some(href) = link.attr_any("href").filter(|_| is_sheet) else {
        return Ok(None);
    };
    let Ok(target) = path::resolve(chapter_path, href) else {
        if sheets.unresolvable.insert(href.to_string()) {
            report(parse_ctx, Role::Formatting.unreadable());
        }
        return Ok(None);
    };
    let id = sheets.parts.len();
    Ok(Some(match sheets.parts.entry(target.path) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let sheet = read_sheet(parse_ctx, pkg, entry.key(), id)?;
            entry.insert(sheet)
        }
    }))
}

/// Missing, a stylesheet costs formatting alone; unreadable, or undecoded where a replacement
/// character may have hidden a `display`, it may hide text. A UTF-16 byte order mark picks
/// UTF-16, else UTF-8.
fn read_sheet(
    parse_ctx: &ParseCtx<'_>,
    pkg: &Package<'_>,
    part: &str,
    id: usize,
) -> Result<Sheet, ConvertError> {
    let bytes = match pkg.part(part) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            report(parse_ctx, Role::Formatting.unreadable());
            return Ok(Sheet { text: None, lost: None });
        }
        Err(e) if e.is_fatal() => return Err(e),
        Err(_) => return Ok(Sheet { text: None, lost: Some(id) }),
    };
    let (text, _, undecoded) = encoding_rs::UTF_8.decode(&bytes);
    let lost = undecoded && Stylesheet::damage_may_hide(parse_ctx, &text)?;
    if undecoded && !lost {
        parse_ctx.emit(DiagnosticCode::InvalidStyleEncoding, Region::Container);
    }
    Ok(Sheet { text: Some(text.into_owned()), lost: lost.then_some(id) })
}

struct ChapterCtx<'a, 'b> {
    pkg: &'b Package<'a>,
    assets: &'b RefCell<AssetSink>,
    chapter_path: &'b str,
    spine_units: &'b HashMap<String, Unit>,
}

impl HtmlCtx for ChapterCtx<'_, '_> {
    fn link_target(&self, href: &str) -> Option<LinkTarget> {
        if is_absolute_uri(href) {
            return Some(LinkTarget::External(href.to_string()));
        }
        let spine_hit = path::resolve(self.chapter_path, href).ok().and_then(|target| {
            let unit = self.spine_units.get(&target.path).copied()?;
            Some((target, unit))
        });
        Some(match spine_hit {
            Some((target, unit)) => match target.fragment.filter(|f| !f.is_empty()) {
                Some(fragment) => {
                    LinkTarget::Anchor { id: scoped(&target.path, &fragment), unit: Some(unit) }
                }
                None => LinkTarget::Unit(unit),
            },
            None => LinkTarget::Relative(href.to_string()),
        })
    }

    fn image_source(&self, src: &str) -> Result<ImageSource, ConvertError> {
        if src.is_empty() {
            return Ok(ImageSource::Unread);
        }
        if is_absolute_uri(src) {
            return Ok(ImageSource::External(src.to_string()));
        }
        let Ok(target) = path::resolve(self.chapter_path, src) else {
            return Ok(ImageSource::Unread);
        };
        let Some(bytes) = self.pkg.related_part(&target.path, Role::Reported(Region::Body))? else {
            return Ok(ImageSource::Unread);
        };
        let media = media_type_for(&target.path);
        let id = self.assets.borrow_mut().add(media, target.path, &bytes)?;
        Ok(ImageSource::Asset(id))
    }

    fn anchor_id(&self, raw: &str) -> AnchorId {
        scoped(self.chapter_path, raw)
    }
}

/// Chapter-scoped anchor id: the chapter path itself targets the chapter
/// start; `path#fragment` targets an element inside it.
fn scoped(path: &str, fragment: &str) -> AnchorId {
    if fragment.is_empty() { path.to_string() } else { format!("{path}#{fragment}") }
}

#[cfg(test)]
#[expect(clippy::format_collect, reason = "fixture XML is built with format!")]
mod tests {
    use super::*;
    use crate::shared::test_support::{
        corrupt_entry, diagnostic_rows, stored, unit_ordinals, zip_package, zip_package_with,
    };

    type Reported = (DiagnosticCode, Region, Option<u32>, u64);

    const CONTAINER: &str = r#"<?xml version="1.0"?>
            <container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
            <rootfiles><rootfile full-path="c.opf"/></rootfiles></container>"#;

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
        let opf = format!(
            r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata/>
            <manifest>{items}</manifest><spine>{refs}</spine></package>"#
        );
        let fixed: [(&str, &[u8]); 2] =
            [("META-INF/container.xml", CONTAINER.as_bytes()), ("c.opf", opf.as_bytes())];
        zip_package_with(&[&fixed, parts].concat(), stored())
    }

    fn chapter(head: &str, body: &str) -> Vec<u8> {
        format!(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head>{head}</head><body><p>text</p>{body}</body></html>"#
        )
        .into_bytes()
    }

    fn diagnose(epub: &[u8]) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, epub).unwrap();
        let diagnostics =
            crate::converter::diagnostics_of(parse_ctx, &doc, crate::ImagePolicy::Placeholder);
        diagnostic_rows(&diagnostics.unwrap())
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
                vec![(UnitUnreadable, BODY, Some(1), 1)],
            ),
            (
                "chapter part absent",
                book(&["gone.xhtml", "ch.xhtml"], &[("ch.xhtml", &plain)]),
                vec![(UnitUnreadable, BODY, Some(1), 1)],
            ),
            (
                "chapter part absent after an unresolvable and a repeated itemref",
                book(
                    &["a%2Fb.xhtml", "ch.xhtml", "ch.xhtml", "gone.xhtml"],
                    &[("ch.xhtml", &plain)],
                ),
                vec![(UnitUnreadable, BODY, Some(1), 1), (UnitUnreadable, BODY, Some(4), 1)],
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
                vec![
                    (DiagnosticCode::ImagePlaceholder, BODY, Some(1), 1),
                    (DiagnosticCode::ImageOmitted, BODY, Some(1), 1),
                ],
            ),
        ];
        for (name, epub, expected) in cases {
            assert_eq!(diagnose(&epub), expected, "{name}");
        }
    }

    fn linking_book(href: &str, sheet: &[u8]) -> Vec<u8> {
        let linking = chapter(&format!(r#"<link rel="stylesheet" href="{href}"/>"#), "");
        book(
            &["one.xhtml", "two.xhtml"],
            &[("one.xhtml", &linking), ("two.xhtml", &linking), ("s.css", sheet)],
        )
    }

    #[test]
    fn a_linked_stylesheet_reports_by_what_its_loss_may_hide() {
        use DiagnosticCode::{InvalidStyleEncoding, PartUnreadable, StylePartUnreadable};
        const BODY: Region = Region::Body;
        const CONTAINER: Region = Region::Container;
        let missing: &[Reported] = &[(StylePartUnreadable, CONTAINER, None, 1)];
        let undecoded: &[Reported] = &[(InvalidStyleEncoding, CONTAINER, None, 1)];
        let lost: &[Reported] =
            &[(PartUnreadable, BODY, Some(1), 1), (PartUnreadable, BODY, Some(2), 1)];
        let cases: [(&str, &str, &[u8], &[Reported]); 10] = [
            ("href unresolvable", "a%2Fb.css", b"", missing),
            ("part absent", "gone.css", b"", missing),
            ("not UTF-8 after the last rule", "s.css", b"p { font-weight: bold }\xff", undecoded),
            ("UTF-8", "s.css", "p::before { content: \"\u{e9}\" }".as_bytes(), &[]),
            ("replacement in a declaration name", "s.css", b".h { disp\xfflay: none }", lost),
            (
                "replacement in a name beside one that reads",
                "s.css",
                b".h { font-weight: bold; dis\xffplay: none }",
                lost,
            ),
            (
                "replacement in the selector of a rule that declares display",
                "s.css",
                b".h\xff { display: none }",
                lost,
            ),
            (
                "replacement in the value of a rule that declares display",
                "s.css",
                b".h { display: n\xffne }",
                lost,
            ),
            (
                "replacement in a rule that declares no display",
                "s.css",
                b".h { font-family: \"\xff\" }",
                undecoded,
            ),
            (
                "replacement in a rule before one that declares display",
                "s.css",
                b".a { font-family: \"\xff\" } .b { display: none }",
                undecoded,
            ),
        ];
        for (name, href, sheet, expected) in cases {
            assert_eq!(diagnose(&linking_book(href, sheet)), expected, "{name}");
        }
    }

    #[test]
    fn a_chapter_reports_each_lost_stylesheet_in_its_own_unit_once() {
        use DiagnosticCode::{PartUnreadable, StylePartUnreadable};
        let twice = chapter(
            concat!(
                r#"<link rel="stylesheet" href="s.css"/><link rel="stylesheet" href="t.css"/>"#,
                r#"<link rel="stylesheet" href="s.css"/>"#
            ),
            "",
        );
        let once = chapter(r#"<link rel="stylesheet" href="s.css"/>"#, "");
        let elsewhere = chapter(r#"<link rel="stylesheet" href="gone.css"/>"#, "");
        let mut epub = book(
            &["one.xhtml", "two.xhtml", "three.xhtml", "four.xhtml"],
            &[
                ("one.xhtml", &twice),
                ("two.xhtml", &chapter("", "")),
                ("three.xhtml", &once),
                ("four.xhtml", &elsewhere),
                ("s.css", b".h { display: none }"),
                ("t.css", b".h { display: none }"),
            ],
        );
        corrupt_entry(&mut epub, "s.css");
        corrupt_entry(&mut epub, "t.css");
        assert_eq!(
            diagnose(&epub),
            [
                (PartUnreadable, Region::Body, Some(1), 2),
                (PartUnreadable, Region::Body, Some(3), 1),
                (StylePartUnreadable, Region::Container, None, 1),
            ]
        );
    }

    fn paragraphs(epub: &[u8]) -> Vec<String> {
        let doc = parse(&ParseCtx::default(), epub).unwrap();
        doc.blocks
            .iter()
            .filter_map(|block| match block {
                Block::Paragraph(inlines) => Some(crate::model::inlines_to_plain_text(inlines)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_media_attribute_gates_the_sheet_it_sits_on() {
        const HIDE: &[u8] = b".h { display: none }";
        let secret = |head: &str| {
            let linking = chapter(head, r#"<p class="h">secret</p>"#);
            let epub = book(&["one.xhtml"], &[("one.xhtml", &linking), ("s.css", HIDE)]);
            paragraphs(&epub).contains(&"secret".to_string())
        };
        let link = |media: &str| format!(r#"<link rel="stylesheet" href="s.css"{media}/>"#);
        let style = |media: &str| format!("<style{media}>.h {{ display: none }}</style>");
        let cases = [
            ("a link", link(""), false),
            ("a link for screen", link(r#" media="screen""#), false),
            ("a link for all", link(r#" media="ALL""#), false),
            ("a link for print", link(r#" media="print""#), true),
            ("a link for speech and print", link(r#" media="speech, print""#), true),
            ("a link with a feature", link(r#" media="(min-width: 1px)""#), true),
            ("a style", style(""), false),
            ("a style for screen", style(r#" media="screen""#), false),
            ("a style for print", style(r#" media="print""#), true),
            ("a style with a feature", style(r#" media="screen and (color)""#), true),
        ];
        for (name, head, expected) in cases {
            assert_eq!(secret(&head), expected, "{name}");
        }
    }

    #[test]
    fn each_chapter_reads_a_shared_sheet_under_the_media_of_its_own_link() {
        let print = chapter(
            r#"<link rel="stylesheet" href="s.css" media="print"/>"#,
            r#"<p class="h">secret</p>"#,
        );
        let all = chapter(r#"<link rel="stylesheet" href="s.css"/>"#, r#"<p class="h">secret</p>"#);
        let epub = book(
            &["print.xhtml", "all.xhtml"],
            &[("print.xhtml", &print), ("all.xhtml", &all), ("s.css", b".h { display: none }")],
        );
        assert_eq!(paragraphs(&epub), ["text", "secret", "text"]);
    }

    #[test]
    fn a_stylesheet_that_may_hide_text_fails_the_conversion_instead_of_showing_it() {
        let linking =
            chapter(r#"<link rel="stylesheet" href="s.css"/>"#, r#"<p class="h">secret</p>"#);
        let mut epub =
            book(&["one.xhtml"], &[("one.xhtml", &linking), ("s.css", b".h { display: none }")]);
        corrupt_entry(&mut epub, "s.css");
        let converter = |on_loss| {
            crate::Converter::new(crate::Config { on_loss, ..crate::Config::default() }).unwrap()
        };
        let input = || crate::Input::new(&epub, crate::Format::Epub);
        let Err(ConvertError::Incomplete { diagnostics }) =
            converter(crate::OnLoss::Fail).convert(input())
        else {
            panic!("the conversion shows what the sheet may hide");
        };
        assert_eq!(
            diagnostic_rows::<Reported>(&diagnostics),
            [(DiagnosticCode::PartUnreadable, Region::Body, Some(1), 1)]
        );
        let kept = converter(crate::OnLoss::Report).convert(input()).unwrap();
        assert_eq!(kept.markdown(), "text\n\nsecret\n");
    }

    #[test]
    fn a_lost_stylesheet_counts_only_under_a_media_list_a_screen_may_hold() {
        let damaged: &[u8] = b".h { disp\xffay: none }";
        let cases = [
            ("unreadable for print", HIDE_H, true, r#" media="print""#, true),
            ("unreadable for speech and print", HIDE_H, true, r#" media="speech, print""#, true),
            ("unreadable with a feature", HIDE_H, true, r#" media="(color)""#, false),
            ("unreadable for screen", HIDE_H, true, r#" media="screen""#, false),
            ("undecoded for print", damaged, false, r#" media="print""#, true),
            ("undecoded with no media", damaged, false, "", false),
        ];
        for (name, sheet, corrupted, media, converts) in cases {
            let head = format!(r#"<link rel="stylesheet" href="s.css"{media}/>"#);
            let linking = chapter(&head, r#"<p class="h">secret</p>"#);
            let mut epub = book(&["one.xhtml"], &[("one.xhtml", &linking), ("s.css", sheet)]);
            if corrupted {
                corrupt_entry(&mut epub, "s.css");
            }
            let converted = crate::Converter::new(crate::Config::default())
                .unwrap()
                .convert(crate::Input::new(&epub, crate::Format::Epub));
            match converted {
                Ok(kept) => {
                    assert!(converts, "{name}");
                    assert_eq!(kept.markdown(), "text\n\nsecret\n", "{name}");
                }
                Err(ConvertError::Incomplete { diagnostics }) => {
                    assert!(!converts, "{name}");
                    assert_eq!(
                        diagnostic_rows::<Reported>(&diagnostics),
                        [(DiagnosticCode::PartUnreadable, Region::Body, Some(1), 1)],
                        "{name}"
                    );
                }
                Err(e) => panic!("{name}: {e:?}"),
            }
        }
    }

    const HIDE_H: &[u8] = b".h { display: none }";

    #[test]
    fn a_stylesheet_behind_a_byte_order_mark_hides_text_in_its_encoding() {
        use DiagnosticCode::{InvalidStyleEncoding, PartUnreadable};
        let utf16 = |css: &str, to_bytes: fn(u16) -> [u8; 2]| -> Vec<u8> {
            let lone = |unit| if unit == u16::from(b'~') { 0xd800 } else { unit };
            std::iter::once(0xfeff).chain(css.encode_utf16().map(lone)).flat_map(to_bytes).collect()
        };
        let le: fn(u16) -> [u8; 2] = u16::to_le_bytes;
        let be: fn(u16) -> [u8; 2] = u16::to_be_bytes;
        let hides = "text\n";
        let shows = "text\n\nsecret\n";
        let lost: &[Reported] = &[(PartUnreadable, Region::Body, Some(1), 1)];
        let cases: [(&str, Vec<u8>, &str, &[Reported]); 6] = [
            ("UTF-16LE", utf16(".h { display: none }", le), hides, &[]),
            ("UTF-16BE", utf16(".h { display: none }", be), hides, &[]),
            ("UTF-8", b"\xef\xbb\xbf.h { display: none }".to_vec(), hides, &[]),
            (
                "UTF-16LE with a lone surrogate in a name",
                utf16(".h { dis~play: none }", le),
                shows,
                lost,
            ),
            (
                "UTF-16BE with a lone surrogate in a name",
                utf16(".h { dis~play: none }", be),
                shows,
                lost,
            ),
            (
                "UTF-16LE with a lone surrogate in a rule that declares no display",
                utf16(".a { font-family: ~ } .h { display: none }", le),
                hides,
                &[(InvalidStyleEncoding, Region::Container, None, 1)],
            ),
        ];
        let converter = crate::Converter::new(crate::Config {
            on_loss: crate::OnLoss::Report,
            ..crate::Config::default()
        })
        .unwrap();
        for (name, sheet, markdown, expected) in cases {
            let linking =
                chapter(r#"<link rel="stylesheet" href="s.css"/>"#, r#"<p class="h">secret</p>"#);
            let epub = book(&["one.xhtml"], &[("one.xhtml", &linking), ("s.css", &sheet)]);
            let kept = converter.convert(crate::Input::new(&epub, crate::Format::Epub)).unwrap();
            assert_eq!(kept.markdown(), markdown, "{name}");
            let reported: Vec<Reported> = diagnostic_rows(kept.diagnostics());
            let reported: Vec<Reported> = reported
                .into_iter()
                .filter(|(code, ..)| *code != DiagnosticCode::HiddenOmitted)
                .collect();
            assert_eq!(reported, expected, "{name}");
        }
    }

    #[test]
    fn the_scan_of_a_stylesheet_that_does_not_decode_charges_its_length() {
        let fuel = |sheet: &[u8]| {
            let parse_ctx = ParseCtx::default();
            parse(&parse_ctx, &linking_book("s.css", sheet)).unwrap();
            parse_ctx.fuel()
        };
        let body = "x".repeat(1000);
        let undecoded = [body.as_bytes(), b"\xff"].concat();
        let decoded = format!("{body}\u{fffd}");
        assert_eq!(fuel(&undecoded) - fuel(decoded.as_bytes()), 1003);
    }

    #[test]
    fn a_stylesheet_entry_past_a_limit_fails_the_conversion_instead_of_reading_as_lost() {
        use crate::limits::{Limit, Limits};
        use std::assert_matches;
        let sheet = format!("/*{}*/", "x".repeat(4000));
        let limits = Limits { max_entry_bytes: 3000, ..Limits::default() };
        assert_matches!(
            parse(&ParseCtx::new(&limits), &linking_book("s.css", sheet.as_bytes())),
            Err(ConvertError::ResourceLimit { limit: Limit::EntryBytes, max: 3000 })
        );
    }

    #[test]
    fn a_part_repeated_across_the_spine_is_read_once() {
        let epub = book(&["ch.xhtml"; 64], &[("ch.xhtml", &chapter("", ""))]);
        let doc = parse(&ParseCtx::default(), &epub).unwrap();
        let text = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph(inlines) => Some(crate::model::inlines_to_plain_text(inlines)),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "text", "sixty-four itemrefs naming one part");
    }

    #[test]
    fn an_itemref_read_or_skipped_holds_its_number_against_max_pages() {
        use crate::limits::{Limit, Limits};
        use std::assert_matches;
        let plain = chapter("", "");
        let parts: [(&str, &[u8]); 2] = [("one.xhtml", &plain), ("two.xhtml", &plain)];
        for spine in [
            ["a%2Fb.xhtml", "b%2Fc.xhtml", "one.xhtml"],
            ["one.xhtml", "one.xhtml", "two.xhtml"],
            ["one.xhtml", "a%2Fb.xhtml", "b%2Fc.xhtml"],
            ["one.xhtml", "two.xhtml", "two.xhtml"],
        ] {
            let epub = book(&spine, &parts);
            let at =
                |max_pages| parse(&ParseCtx::new(&Limits { max_pages, ..Limits::DEFAULT }), &epub);
            assert!(at(3).is_ok(), "{spine:?}");
            assert_matches!(
                at(2),
                Err(ConvertError::ResourceLimit { limit: Limit::Pages, max: 2 }),
                "{spine:?}"
            );
        }
    }

    #[test]
    fn empty_fragments_target_units_and_nonempty_fragments_target_source_anchors() {
        let first = chapter(
            "",
            r##"<p><a href="">empty</a><a href="#">self</a><a href="two.xhtml#">next</a><a href="two.xhtml#mark">fragment</a></p>"##,
        );
        let second = chapter("", r#"<p id="mark">second</p>"#);
        let bytes = book(
            &["bad%2Fpart.xhtml", "one.xhtml", "two.xhtml"],
            &[("one.xhtml", &first), ("two.xhtml", &second)],
        );
        let document = parse(&ParseCtx::default(), &bytes).unwrap();
        assert_eq!(unit_ordinals(&document), [2, 3]);
        let markdown = crate::render::markdown::document_to_markdown(&document);
        assert!(markdown.contains("[empty](#spine-item-2)"), "{markdown}");
        assert!(markdown.contains("[self](#spine-item-2)"), "{markdown}");
        assert!(markdown.contains("[next](#spine-item-3)"), "{markdown}");
        assert!(markdown.contains("[fragment](#two-xhtml-mark)"), "{markdown}");
    }

    #[test]
    fn unread_chapter_fragments_do_not_hide_independent_missing_anchors() {
        let first = chapter(
            "",
            r##"<p><a href="gone.xhtml#lost">unread</a><a href="#missing">independent</a></p>"##,
        );
        let bytes = book(&["one.xhtml", "gone.xhtml"], &[("one.xhtml", &first)]);
        let config = crate::Config { on_loss: crate::OnLoss::Report, ..crate::Config::default() };
        let converter = crate::Converter::new(config).unwrap();
        let converted = converter.convert(crate::Input::new(&bytes, crate::Format::Epub)).unwrap();
        assert_eq!(converted.markdown(), "text\n\nunreadindependent\n");
        for code in [DiagnosticCode::UnitUnreadable, DiagnosticCode::DanglingReference] {
            assert_eq!(
                converted
                    .diagnostics()
                    .iter()
                    .filter(|d| d.code == code)
                    .map(|d| d.count)
                    .sum::<u64>(),
                1,
                "{code:?}"
            );
        }
        assert_eq!(
            converter
                .parse(crate::Input::new(&bytes, crate::Format::Epub))
                .unwrap()
                .render()
                .unwrap(),
            converted
        );
    }

    #[test]
    fn a_link_outside_the_spine_stays_relative_and_an_empty_id_anchors_the_chapter() {
        use std::assert_matches;
        let html = chapter("", r#"<h2 id="">head</h2><p><a href="files/a.zip">get</a></p>"#);
        let bytes = book(&["one.xhtml"], &[("one.xhtml", &html)]);
        let document = parse(&ParseCtx::default(), &bytes).unwrap();
        let [_, _, Block::Heading { anchor, .. }, Block::Paragraph(links)] =
            document.blocks.as_slice()
        else {
            panic!("{:?}", document.blocks);
        };
        assert_eq!(anchor.as_deref(), Some("one.xhtml"));
        assert_matches!(
            links.as_slice(),
            [Inline::Link { target: LinkTarget::Relative(href), .. }] if href == "files/a.zip"
        );
    }

    #[test]
    fn the_opf_title_heads_the_book_only_when_it_has_text() {
        let chapter = chapter("", "");
        for (label, title, expected) in
            [("blank", " \n ", &[][..]), ("padded", " Moby Dick ", &["Moby Dick"])]
        {
            let opf = format!(
                r#"<package xmlns="http://www.idpf.org/2007/opf">
                <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
                <dc:title>{title}</dc:title></metadata>
                <manifest><item id="i" href="ch.xhtml"/></manifest>
                <spine><itemref idref="i"/></spine></package>"#
            );
            let parts: [(&str, &[u8]); 3] = [
                ("META-INF/container.xml", CONTAINER.as_bytes()),
                ("c.opf", opf.as_bytes()),
                ("ch.xhtml", &chapter),
            ];
            let document = parse(&ParseCtx::default(), &zip_package(&parts)).unwrap();
            let headings: Vec<String> = document
                .blocks
                .iter()
                .filter(|block| matches!(block, Block::Heading { .. }))
                .map(Block::plain_text)
                .collect();
            assert_eq!(headings, expected, "{label}");
        }
    }

    #[test]
    fn a_stylesheet_that_is_not_utf8_still_styles_by_its_readable_text() {
        let linking = chapter(r#"<link rel="stylesheet" href="s.css"/>"#, "");
        let sheet: &[u8] = b"p { font-weight: bold }\xff";
        let bytes = book(&["one.xhtml"], &[("one.xhtml", &linking), ("s.css", sheet)]);
        let document = parse(&ParseCtx::default(), &bytes).unwrap();
        assert_eq!(crate::render::markdown::document_to_markdown(&document), "**text**\n");
    }

    #[test]
    fn empty_chapters_keep_units_without_inventing_output() {
        use std::assert_matches;
        let empty = br#"<html xmlns="http://www.w3.org/1999/xhtml"><body/></html>"#;
        let bytes =
            book(&["one.xhtml", "two.xhtml"], &[("one.xhtml", empty), ("two.xhtml", empty)]);
        let document = parse(&ParseCtx::default(), &bytes).unwrap();
        assert_matches!(document.blocks.as_slice(), [Block::UnitStart(first), Block::UnitStart(second)] if first.ordinal.get() == 1 && second.ordinal.get() == 2);
        assert_eq!(crate::render::markdown::document_to_markdown(&document), "");
    }

    #[test]
    fn an_image_asset_takes_its_media_type_from_its_part_name() {
        let html = chapter("", r#"<img src="a.png"/><img src="b.jpg"/>"#);
        let bytes =
            book(&["c.xhtml"], &[("c.xhtml", &html), ("a.png", b"first"), ("b.jpg", b"second")]);
        let document = parse(&ParseCtx::default(), &bytes).unwrap();
        let assets: Vec<_> = document
            .assets
            .iter()
            .map(|asset| (asset.origin_part.as_str(), asset.media_type.as_str()))
            .collect();
        assert_eq!(assets, [("a.png", "image/png"), ("b.jpg", "image/jpeg")]);
    }

    #[test]
    fn an_unreadable_picture_without_alt_keeps_its_chapter_position() {
        for src in ["", "missing.png"] {
            let html = format!(
                r#"<html xmlns="http://www.w3.org/1999/xhtml"><head/><body><p>before<img src="{src}"/>after</p></body></html>"#
            );
            let ctx = ParseCtx::default();
            let document =
                parse(&ctx, &book(&["c.xhtml"], &[("c.xhtml", html.as_bytes())])).unwrap();
            assert_eq!(
                crate::render::markdown::document_to_markdown(&document),
                "before\\[Image\\]after\n"
            );
            assert_eq!(
                crate::converter::diagnostics_of(ctx, &document, crate::ImagePolicy::Placeholder)
                    .unwrap()
                    .iter()
                    .map(|d| (d.code, d.count))
                    .collect::<Vec<_>>(),
                [(DiagnosticCode::ImageUnavailable, 1)]
            );
            let html = html.replace("<img ", "<img style=\"display:none\" ");
            let ctx = ParseCtx::default();
            let document =
                parse(&ctx, &book(&["c.xhtml"], &[("c.xhtml", html.as_bytes())])).unwrap();
            assert_eq!(crate::render::markdown::document_to_markdown(&document), "beforeafter\n");
            assert!(ctx.diagnostics().iter().all(|d| d.code == DiagnosticCode::HiddenOmitted));
        }
    }
}
