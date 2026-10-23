//! OpenDocument Text (.odt), Spreadsheet (.ods), and Presentation (.odp).

mod axes;
mod datastyle;
mod styles;
mod table;
mod text;

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Block, Document, Inline, inlines_are_empty};
use crate::package::archive::{Role, report};
use crate::package::xml::{Element, ns};
use crate::package::{Package, PackageKind};
use crate::shared::assets::AssetSink;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use text::{Ctx, holds_content, parse_container};

const STYLES_PART: &str = "styles.xml";
const DR3D: &str = "urn:oasis:names:tc:opendocument:xmlns:dr3d:1.0";
const OFFICEOOO: &str = "http://openoffice.org/2009/office";

pub(super) fn parse(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Document, ConvertError> {
    let pkg = Package::open(parse_ctx, bytes, PackageKind::Odf)?;

    if is_encrypted(&pkg)? {
        return Err(ConvertError::Encrypted);
    }

    let content_tree = pkg.required_xml_part("content.xml", Role::Content(Region::Body))?;
    let body = content_tree
        .find(ns::OFFICE, "document-content")
        .and_then(|d| d.find(ns::OFFICE, "body"))
        .ok_or_else(|| ConvertError::malformed_part("content.xml", "no office:body"))?;

    let (styles_tree, styles_unread) = if body.find(ns::OFFICE, "spreadsheet").is_some() {
        read_spreadsheet_styles(parse_ctx, &pkg)?
    } else {
        (pkg.optional_xml_part(STYLES_PART, Role::Content(Region::Body))?, false)
    };
    let mut styles = styles::OdfStyles::default();
    if styles_unread {
        styles.mark_unread();
    }
    for tree in styles_tree.iter().chain([&content_tree]) {
        styles.collect(tree);
    }

    let assets = RefCell::new(AssetSink::new(parse_ctx));
    let ctx = Ctx::new(parse_ctx, &styles, &pkg, &assets);

    let blocks = if let Some(text) = body.find(ns::OFFICE, "text") {
        for master in text_master_pages(parse_ctx, text, &styles)? {
            report_running(master, parse_ctx);
        }
        parse_container(text, &ctx)?
    } else if let Some(sheet) = body.find(ns::OFFICE, "spreadsheet") {
        table::parse_spreadsheet(sheet, &ctx)?
    } else if let Some(pres) = body.find(ns::OFFICE, "presentation") {
        parse_presentation(pres, &ctx)?
    } else {
        return Err(ConvertError::malformed_part(
            "content.xml",
            "no recognized office body (text, spreadsheet, or presentation)",
        ));
    };

    let notes = ctx.notes.into_inner();
    let assets = std::mem::take(&mut assets.borrow_mut().assets);
    Ok(Document { blocks, notes, assets })
}

/// A spreadsheet's styles part that does not read costs formatting alone, unless its entry
/// cannot be inflated, or it does not parse and sets a `display` that may hide a sheet or text.
/// Returns its tree, and whether formatting alone was lost with it.
fn read_spreadsheet_styles(
    parse_ctx: &ParseCtx<'_>,
    pkg: &Package<'_>,
) -> Result<(Option<Element>, bool), ConvertError> {
    let lost = match pkg.part(STYLES_PART) {
        Ok(Some(bytes)) => match pkg.optional_xml_part(STYLES_PART, Role::Probe)? {
            Some(tree) => return Ok((Some(tree), false)),
            None if styles::sets_hiding(parse_ctx, &bytes)? => Role::Content(Region::Body),
            None => Role::Formatting,
        },
        Ok(None) => return Ok((None, false)),
        Err(e) if e.is_fatal() => return Err(e),
        Err(_) => Role::Content(Region::Body),
    };
    report(parse_ctx, lost.unreadable());
    Ok((None, lost == Role::Formatting))
}

/// Encrypted ODF packages carry `manifest:encryption-data` elements on file
/// entries. The manifest is parsed properly - substring matching would
/// classify a document as encrypted over a mere comment. An absent or
/// unreadable manifest proves nothing (content parsing decides), but fatal
/// resource-limit errors propagate.
fn is_encrypted(pkg: &Package<'_>) -> Result<bool, ConvertError> {
    let Some(tree) = pkg.optional_xml_part("META-INF/manifest.xml", Role::Probe)? else {
        return Ok(false);
    };
    Ok(tree.first_descendant(ns::MANIFEST, "encryption-data").is_some())
}

/// The master pages a text document's pages use: the first, which starts
/// a document naming none, those a paragraph or table style applied in the
/// text starts, and each one's next.
fn text_master_pages<'a>(
    parse_ctx: &ParseCtx<'_>,
    text: &Element,
    styles: &styles::OdfStyles<'a>,
) -> Result<Vec<&'a Element>, ConvertError> {
    let applied: BTreeSet<(&str, &str)> = text
        .descendant_elems()
        .filter_map(|e| {
            if e.is(ns::TEXT, "p") || e.is(ns::TEXT, "h") {
                Some(("paragraph", e.attr(ns::TEXT, "style-name")?))
            } else if e.is(ns::TABLE, "table") {
                Some(("table", e.attr(ns::TABLE, "style-name")?))
            } else {
                None
            }
        })
        .collect();
    let mut pending = Vec::new();
    for (family, name) in applied {
        pending.extend(styles.style_master_page(parse_ctx, family, name)?);
    }
    pending.extend(styles.first_master_page().and_then(|m| m.attr(ns::STYLE, "name")));
    let mut used = BTreeSet::new();
    while let Some(name) = pending.pop() {
        let Some(master) = styles.master_page(name) else { continue };
        if used.insert(name) {
            pending.extend(master.attr(ns::STYLE, "next-style-name"));
        }
    }
    Ok(used.into_iter().filter_map(|name| styles.master_page(name)).collect())
}

/// Report the header and footer a master page shows on every page it
/// starts.
fn report_running(master: &Element, parse_ctx: &ParseCtx<'_>) {
    let running = [
        (["header", "header-left", "header-first"], DiagnosticCode::HeaderOmitted, Region::Header),
        (["footer", "footer-left", "footer-first"], DiagnosticCode::FooterOmitted, Region::Footer),
    ];
    for (kinds, code, region) in running {
        let shown = kinds
            .iter()
            .filter_map(|kind| master.find(ns::STYLE, kind))
            .any(|part| part.attr(ns::STYLE, "display") != Some("false") && holds_content(part));
        if shown {
            parse_ctx.emit(code, region);
        }
    }
}

/// The running header or footer a presentation frame class stands for.
fn running_frame(frame: &Element) -> Option<(DiagnosticCode, Region)> {
    match frame.attr(ns::PRESENTATION, "class")? {
        "header" => Some((DiagnosticCode::HeaderOmitted, Region::Header)),
        "footer" | "page-number" | "date-time" => {
            Some((DiagnosticCode::FooterOmitted, Region::Footer))
        }
        _ => None,
    }
}

fn parse_presentation(pres: &Element, ctx: &Ctx<'_, '_>) -> Result<Vec<Block>, ConvertError> {
    let styles = ctx.styles;
    let declared = declarations(pres);
    let mut blocks = Vec::new();
    let mut masters: BTreeMap<Option<&str>, (&Element, Shown)> = BTreeMap::new();
    for (position, page) in pres.find_all(ns::DRAW, "page").enumerate() {
        let unit = ctx.parse_ctx.begin_unit(crate::model::UnitKind::Slide, position)?;
        blocks.push(Block::UnitStart(unit.unit().model()));
        let props = page_properties(page, ctx);
        if let Some(master) = styles.master_or_first(page.attr(ns::DRAW, "master-page-name")) {
            let (_, shown) =
                masters.entry(master.attr(ns::STYLE, "name")).or_insert((master, Shown::default()));
            shown.objects |= shows(props, "background-objects-visible");
            shown.background |= shows(props, "background-visible");
        }
        report_declared(page, props, &declared, ctx);
        let mut title = Vec::new();
        let mut body = Vec::new();
        let mut notes = Vec::new();
        walk_shapes(page, ctx, &mut title, &mut body, &mut notes)?;
        blocks.append(&mut title);
        blocks.append(&mut body);
        if !notes.is_empty() {
            blocks.push(Block::BlockQuote(notes));
        }
    }
    for (master, shown) in masters.into_values() {
        if decorates(master, shown, ctx) {
            ctx.parse_ctx.emit(DiagnosticCode::HeaderOmitted, Region::Header);
        }
    }
    Ok(blocks)
}

/// What some slide using a master page shows of it.
#[derive(Default, Clone, Copy)]
struct Shown {
    objects: bool,
    background: bool,
}

fn page_properties<'a>(page: &Element, ctx: &Ctx<'_, 'a>) -> Option<&'a Element> {
    page.attr(ns::DRAW, "style-name")
        .and_then(|name| ctx.styles.style("drawing-page", name))
        .and_then(|style| style.find(ns::STYLE, "drawing-page-properties"))
}

/// Whether a slide's page properties leave `attr` at its default, shown.
fn shows(props: Option<&Element>, attr: &str) -> bool {
    props.and_then(|p| p.attr(ns::PRESENTATION, attr)) != Some("false")
}

/// The header, footer and date-time declarations that show anything, by
/// kind and name.
fn declarations(pres: &Element) -> HashSet<(&str, &str)> {
    pres.child_elems()
        .filter(|d| {
            d.ns.as_deref() == Some(ns::PRESENTATION)
                && matches!(d.local.as_str(), "header-decl" | "footer-decl" | "date-time-decl")
                && (holds_content(d) || d.attr(ns::PRESENTATION, "source") == Some("current-date"))
        })
        .filter_map(|d| Some((d.local.as_str(), d.attr(ns::PRESENTATION, "name")?)))
        .collect()
}

/// Report the header, footer and date a slide shows from its presentation's
/// declarations, unless its page style hides them.
fn report_declared(
    page: &Element,
    props: Option<&Element>,
    declared: &HashSet<(&str, &str)>,
    ctx: &Ctx<'_, '_>,
) {
    use DiagnosticCode::{FooterOmitted, HeaderOmitted};
    use Region::{Footer, Header};
    let kinds = [
        ("use-header-name", "header-decl", "display-header", HeaderOmitted, Header),
        ("use-footer-name", "footer-decl", "display-footer", FooterOmitted, Footer),
        ("use-date-time-name", "date-time-decl", "display-date-time", FooterOmitted, Footer),
    ];
    for (used, decl, display, code, region) in kinds {
        let Some(name) = page.attr(ns::PRESENTATION, used) else { continue };
        if shows(props, display) && declared.contains(&(decl, name)) {
            ctx.parse_ctx.emit(code, region);
        }
    }
}

/// Whether a master page shows anything of its own on the slides using it:
/// a shape that is no placeholder, or a background picture, each where
/// some slide leaves it visible.
fn decorates(master: &Element, shown: Shown, ctx: &Ctx<'_, '_>) -> bool {
    let shape = shown.objects
        && master.child_elems().any(|shape| {
            shape.ns.as_deref() == Some(ns::DRAW) && shape.attr(ns::PRESENTATION, "class").is_none()
        });
    let background = shown.background
        && page_properties(master, ctx)
            .is_some_and(|props| props.attr(ns::DRAW, "fill") == Some("bitmap"));
    shape || background
}

/// Walk a page's shapes in document order, recursing into `draw:g` groups.
fn walk_shapes(
    parent: &Element,
    ctx: &Ctx<'_, '_>,
    title: &mut Vec<Block>,
    body: &mut Vec<Block>,
    notes: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    for child in parent.child_elems() {
        if child.is(ns::PRESENTATION, "notes") {
            ctx.in_region(Region::Note, || walk_notes(child, ctx, notes))?;
            continue;
        }
        if child.is(ns::OFFICE, "annotation") || child.is(OFFICEOOO, "annotation") {
            ctx.parse_ctx.emit(DiagnosticCode::CommentOmitted, Region::Comment);
            continue;
        }
        if child.is(DR3D, "scene") {
            text::report_unplaced(child, ctx)?;
            continue;
        }
        if child.ns.as_deref() != Some(ns::DRAW) {
            continue;
        }
        match child.local.as_str() {
            "frame" => {
                if let Some((code, region)) = running_frame(child) {
                    ctx.parse_ctx.emit(code, region);
                    continue;
                }
                let inner = match text::frame_content(child, ctx)? {
                    Some(text::FrameContent::Blocks(blocks)) => blocks,
                    Some(text::FrameContent::Inline(Inline::Math(tex))) => vec![Block::Math(tex)],
                    Some(text::FrameContent::Inline(graphic)) => {
                        vec![Block::Paragraph(vec![graphic])]
                    }
                    None => Vec::new(),
                };
                if child.attr(ns::PRESENTATION, "class") == Some("title") {
                    push_title_heading(inner, title, ctx);
                } else {
                    body.extend(inner);
                }
            }
            "g" => walk_shapes(child, ctx, title, body, notes)?,
            "a" => text::report_unplaced(child, ctx)?,
            local if text::is_shape(local) => {
                if child.child_elems().any(|c| c.is(ns::TEXT, "p") || c.is(ns::TEXT, "list")) {
                    body.extend(parse_container(child, ctx)?);
                }
            }
            "control" => ctx.emit(DiagnosticCode::FormControlUnread),
            _ => {}
        }
    }
    Ok(())
}

/// A notes page's text boxes, through its groups; its running header and
/// footer frames and every other shape are reported, not rendered.
fn walk_notes(
    page: &Element,
    ctx: &Ctx<'_, '_>,
    notes: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    for shape in page.child_elems() {
        if shape.is(ns::DRAW, "g") {
            walk_notes(shape, ctx, notes)?;
        } else if !shape.is(ns::DRAW, "frame") {
            text::report_unplaced(shape, ctx)?;
        } else if let Some((code, region)) = running_frame(shape) {
            ctx.parse_ctx.emit(code, region);
        } else if let Some(text_box) = shape.find(ns::DRAW, "text-box") {
            notes.extend(parse_container(text_box, ctx)?);
        } else {
            text::report_unplaced(shape, ctx)?;
        }
    }
    Ok(())
}

/// Collapse a title frame's paragraphs into one slide heading; any other
/// block there is reported.
fn push_title_heading(inner: Vec<Block>, blocks: &mut Vec<Block>, ctx: &Ctx<'_, '_>) {
    let mut inlines: Vec<Inline> = Vec::new();
    for block in inner {
        let Block::Paragraph(para) = block else {
            ctx.emit(DiagnosticCode::BlockUnread);
            continue;
        };
        if inlines_are_empty(&para) {
            continue;
        }
        if !inlines.is_empty() {
            inlines.push(Inline::LineBreak);
        }
        inlines.extend(para);
    }
    if !inlines_are_empty(&inlines) {
        let anchor = Some(crate::model::inlines_to_plain_text(&inlines));
        blocks.push(Block::Heading { level: 2, anchor, content: inlines });
    }
}

#[cfg(test)]
#[expect(clippy::format_collect, reason = "fixture XML is built with format!")]
mod tests {
    use super::*;
    use crate::limits::{Limit, Limits};
    use crate::render::markdown::document_to_markdown;
    use crate::shared::test_support::{
        corrupt_entry, diagnostic_rows, stored, unit_ordinals, zip_package, zip_package_with,
    };
    use std::assert_matches;

    const CODE_STYLE: &str = r#"<office:automatic-styles><style:style style:name="Source_20_Code" style:family="paragraph"/></office:automatic-styles>"#;

    fn with_manifest(manifest: &str) -> Result<Document, ConvertError> {
        let parts = [("META-INF/manifest.xml", manifest)];
        let odf = Odf { parts: &parts, ..text("<text:p>hello</text:p>") };
        parse(&ParseCtx::default(), &odf.build())
    }

    #[test]
    fn corrupt_manifest_does_not_classify_as_encrypted() {
        let doc = with_manifest("<manifest:manifest <<< not xml").unwrap();
        assert_ne!(doc.blocks, []);
    }

    #[test]
    fn repeated_rows_cannot_amplify_text_beyond_the_byte_budget() {
        // The slot budget alone would admit 1000 copies of a 100 KB
        // cell (~100 MB); the duplicated-bytes budget rejects it up front.
        let fat = "x".repeat(100_000);
        let rows = format!(
            r#"<table:table-row table:number-rows-repeated="1000">
            <table:table-cell><text:p>{fat}</text:p></table:table-cell>
            </table:table-row>"#
        );
        let odf = text(&format!("<table:table>{rows}</table:table>"));
        let err = parse(&ParseCtx::default(), &odf.build()).unwrap_err();
        assert_matches!(
            err,
            ConvertError::ResourceLimit { limit: Limit::ExpansionTextBytes, .. },
            "expected the text-byte budget, got: {err}"
        );
    }

    #[test]
    fn repeated_rows_parse_content_once() {
        // A note inside a repeated row must register once, not per copy.
        let rows = r#"<table:table-row table:number-rows-repeated="3">
            <table:table-cell><text:p>cell<text:note text:id="n1">
            <text:note-body><text:p>note body</text:p></text:note-body>
            </text:note></text:p></table:table-cell>
            </table:table-row>"#;
        let odf = text(&format!("<table:table>{rows}</table:table>"));
        let doc = parse(&ParseCtx::default(), &odf.build()).unwrap();
        assert_eq!(doc.notes.len(), 1, "repeated rows must not duplicate notes");
    }

    #[test]
    fn styled_runs_stop_at_tables_and_work_inside_cells() {
        let body = r#"
            <text:p text:style-name="Source_20_Code">before table</text:p>
            <table:table><table:table-row><table:table-cell>
              <text:p text:style-name="Source_20_Code">inside cell</text:p>
            </table:table-cell></table:table-row></table:table>
            <text:p>after table</text:p>
            "#;
        let odf = Odf { head: CODE_STYLE, ..text(body) };
        let doc = parse(&ParseCtx::default(), &odf.build()).unwrap();
        let [Block::CodeBlock { text: before, .. }, Block::Table(table), Block::Paragraph(after)] =
            &doc.blocks[..]
        else {
            panic!("unexpected block order: {:?}", doc.blocks);
        };
        assert_eq!(before, "before table");
        assert_eq!(crate::model::inlines_to_plain_text(after), "after table");
        let crate::model::CellSlot::Origin(cell) = &table.grid[0][0] else {
            panic!("expected an origin cell")
        };
        let [Block::CodeBlock { text: inside, .. }] = &cell.blocks[..] else {
            panic!("unexpected cell blocks: {:?}", cell.blocks)
        };
        assert_eq!(inside, "inside cell");
    }

    #[test]
    fn styled_runs_stop_at_lists() {
        let body = r#"
            <text:p text:style-name="Source_20_Code">before list</text:p>
            <text:list><text:list-item><text:p>item</text:p></text:list-item></text:list>
            "#;
        let odf = Odf { head: CODE_STYLE, ..text(body) };
        let doc = parse(&ParseCtx::default(), &odf.build()).unwrap();
        let [Block::CodeBlock { text, .. }, Block::List(_)] = &doc.blocks[..] else {
            panic!("unexpected block order: {:?}", doc.blocks);
        };
        assert_eq!(text, "before list");
    }

    #[test]
    fn a_styled_paragraph_holding_a_text_box_closes_its_run_before_the_box() {
        let body = r#"<text:p text:style-name="Source_20_Code">code<draw:frame><draw:text-box><text:p>box</text:p></draw:text-box></draw:frame></text:p>"#;
        let odf = Odf { head: CODE_STYLE, ..text(body) };
        let doc = parse(&ParseCtx::default(), &odf.build()).unwrap();
        let [Block::CodeBlock { text, .. }, Block::Paragraph(boxed)] = &doc.blocks[..] else {
            panic!("unexpected block order: {:?}", doc.blocks);
        };
        assert_eq!(text, "code");
        assert_eq!(crate::model::inlines_to_plain_text(boxed), "box");
    }

    #[test]
    fn resource_limit_in_manifest_is_fatal() {
        let depth = usize::try_from(Limits::default().max_nesting_depth).unwrap() + 2;
        let manifest = "<d>".repeat(depth);
        let err = with_manifest(&manifest).unwrap_err();
        assert_matches!(
            err,
            ConvertError::ResourceLimit { limit: Limit::NestingDepth, .. },
            "encryption probing must not swallow fatal errors, got: {err}"
        );
    }

    #[test]
    fn form_checkboxes_anchored_in_a_cell_follow_its_content() {
        let body = r#"<table:table table:name="S">
            <office:forms><form:form>
              <form:checkbox xml:id="c1" form:label="Roof" form:current-state="checked"/>
              <form:checkbox xml:id="c2" form:label="Wall"/>
              <form:checkbox xml:id="c3" form:current-state="unknown"/>
            </form:form></office:forms>
            <table:table-row>
              <table:table-cell><text:p>14</text:p></table:table-cell>
              <table:table-cell><text:p>L/R</text:p><draw:control draw:control="c1"/></table:table-cell>
              <table:table-cell><draw:control draw:control="c2"/><draw:control draw:control="c3"/></table:table-cell>
            </table:table-row>
            </table:table>"#;
        let md = convert(&sheet(body)).0;
        assert_eq!(md, "|  |  |  |\n| --- | --- | --- |\n| 14 | L/R [x] Roof | [ ] Wall |\n");
    }

    const NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
        xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
        xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
        xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
        xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
        xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0"
        xmlns:form="urn:oasis:names:tc:opendocument:xmlns:form:1.0"
        xmlns:xlink="http://www.w3.org/1999/xlink"
        xmlns:dr3d="urn:oasis:names:tc:opendocument:xmlns:dr3d:1.0"
        xmlns:officeooo="http://openoffice.org/2009/office"
        xmlns:math="http://www.w3.org/1998/Math/MathML""#;

    const CHART: &str = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"><office:body><office:chart/></office:body></office:document-content>"#;
    const SHEET: &str = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"><office:body><office:spreadsheet/></office:body></office:document-content>"#;
    const FORMULA: &str = r#"<math:math xmlns:math="http://www.w3.org/1998/Math/MathML"><math:mi>x</math:mi></math:math>"#;

    struct Odf<'s> {
        kind: &'s str,
        head: &'s str,
        body: String,
        styles: &'s str,
        parts: &'s [(&'s str, &'s str)],
    }

    impl Odf<'_> {
        fn content(&self) -> String {
            format!(
                "<office:document-content {NS}>{head}<office:body><office:{kind}>{body}</office:{kind}></office:body></office:document-content>",
                kind = self.kind,
                head = self.head,
                body = self.body
            )
        }

        fn build(&self) -> Vec<u8> {
            let content = self.content();
            let styles = (!self.styles.is_empty()).then(|| {
                format!("<office:document-styles {NS}>{}</office:document-styles>", self.styles)
            });
            let mut parts = vec![("content.xml", content.as_str())];
            if let Some(styles) = &styles {
                parts.push(("styles.xml", styles));
            }
            parts.extend_from_slice(self.parts);
            zip_package(&parts)
        }
    }

    fn text(body: &str) -> Odf<'static> {
        Odf { kind: "text", head: "", body: body.to_string(), styles: "", parts: &[] }
    }

    fn sheet(body: &str) -> Odf<'static> {
        Odf { kind: "spreadsheet", head: "", body: body.to_string(), styles: "", parts: &[] }
    }

    fn slides(body: &str) -> Odf<'static> {
        Odf { kind: "presentation", head: "", body: body.to_string(), styles: "", parts: &[] }
    }

    type Reported = (DiagnosticCode, Region, Option<u32>, u64);

    fn parsed(odf: &Odf<'_>) -> (Document, Vec<Reported>) {
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, &odf.build()).unwrap();
        let diagnostics =
            crate::converter::diagnostics_of(parse_ctx, &doc, crate::ImagePolicy::Placeholder);
        (doc, diagnostic_rows(&diagnostics.unwrap()))
    }

    fn convert(odf: &Odf<'_>) -> (String, Vec<Reported>) {
        let (doc, reported) = parsed(odf);
        (document_to_markdown(&doc), reported)
    }

    fn diagnose(odf: &Odf<'_>) -> Vec<Reported> {
        parsed(odf).1
    }

    const BODY: Region = Region::Body;
    use DiagnosticCode::{
        BlockUnread, CommentOmitted, EmbeddedObject, FooterOmitted, FormControlUnread,
        GraphicUnread, HeaderOmitted, HiddenOmitted, ImageUnavailable, ImageUnplaced,
        PartUnreadable, RevisionOmitted, StylePartUnreadable, TableSpanRepaired, TextBoxUnread,
        UnmappedSymbol,
    };

    fn frame(content: &str) -> String {
        format!("<draw:frame>{content}</draw:frame>")
    }

    fn sheet_body(cell: &str, shapes: &str) -> String {
        format!(
            r#"<table:table table:name="S">{shapes}<table:table-row><table:table-cell><text:p>v</text:p>{cell}</table:table-cell></table:table-row></table:table>"#
        )
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the table of lost sites, then the media by anchor product"
    )]
    fn each_lost_site_reports_its_code_once_where_it_sits() {
        let image = r#"<draw:image xlink:href="Pictures/a.png"/>"#;
        let chart = r#"<draw:object xlink:href="./Object 1"/>"#;
        let parts = [
            ("Pictures/a.png", "png"),
            ("Object 1/content.xml", CHART),
            ("Object 2/content.xml", SHEET),
        ];
        let inline = |content: &str| format!("<text:p>x{}</text:p>", frame(content));
        let replaced =
            |object: &str| format!(r#"{object}<draw:image xlink:href="Pictures/gone.png"/>"#);
        let shapes =
            |shapes: &str| sheet_body("", &format!("<table:shapes>{shapes}</table:shapes>"));
        let cases: Vec<(&str, Odf<'_>, Vec<Reported>)> = vec![
            (
                "object href unresolvable",
                text(&inline(r#"<draw:object xlink:href="a%2Fb"/>"#)),
                vec![(EmbeddedObject, BODY, None, 1)],
            ),
            (
                "object part absent",
                text(&inline(&replaced(r#"<draw:object xlink:href="./Object 9"/>"#))),
                vec![
                    (ImageUnavailable, Region::Body, None, 1),
                    (EmbeddedObject, Region::Body, None, 1),
                ],
            ),
            (
                "object outside the package",
                text(&inline(r#"<draw:object xlink:href="https://example.org/o"/>"#)),
                vec![(EmbeddedObject, BODY, None, 1)],
            ),
            (
                "chart object",
                text(&inline(&replaced(chart))),
                vec![
                    (ImageUnavailable, Region::Body, None, 1),
                    (GraphicUnread, Region::Body, None, 1),
                ],
            ),
            (
                "other object",
                text(&inline(r#"<draw:object xlink:href="./Object 2"/>"#)),
                vec![(EmbeddedObject, BODY, None, 1)],
            ),
            (
                "ole object",
                text(&inline(r#"<draw:object-ole xlink:href="./Object 3"/>"#)),
                vec![(EmbeddedObject, BODY, None, 1)],
            ),
            (
                "image href unresolvable",
                text(&inline(r#"<draw:image xlink:href="a%2Fb.png"/>"#)),
                vec![(ImageUnavailable, BODY, None, 1)],
            ),
            (
                "image href empty",
                text(&inline("<draw:image/>")),
                vec![(ImageUnavailable, BODY, None, 1)],
            ),
            (
                "image part absent",
                text(&inline(r#"<draw:image xlink:href="Pictures/gone.png"/>"#)),
                vec![(ImageUnavailable, BODY, None, 1)],
            ),
            (
                "image in a note",
                text(&format!(
                    r#"<text:p>x<text:note text:id="n"><text:note-body><text:p>{}</text:p></text:note-body></text:note></text:p>"#,
                    frame(r#"<draw:image xlink:href="Pictures/gone.png"/>"#)
                )),
                vec![(ImageUnavailable, Region::Note, None, 1)],
            ),
            (
                "numbered paragraph",
                text("<text:numbered-paragraph><text:p>gone</text:p></text:numbered-paragraph>"),
                vec![(BlockUnread, BODY, None, 1)],
            ),
            ("page-anchored picture", text(&frame(image)), vec![(ImageUnplaced, BODY, None, 1)]),
            (
                "page-anchored text box",
                text(&frame("<draw:text-box><text:p>t</text:p></draw:text-box>")),
                vec![(TextBoxUnread, BODY, None, 1)],
            ),
            (
                "image in speaker notes",
                slides(&format!(
                    "<draw:page><presentation:notes>{}</presentation:notes></draw:page>",
                    frame(&format!(
                        "<draw:text-box><text:p>n{}</text:p></draw:text-box>",
                        frame(r#"<draw:image xlink:href="Pictures/gone.png"/>"#)
                    ))
                )),
                vec![(ImageUnavailable, Region::Note, Some(1), 1)],
            ),
            (
                "empty text box",
                text(&frame("<draw:text-box><text:p> </text:p></draw:text-box>")),
                vec![],
            ),
            (
                "text box holding only a comment",
                text(&frame(
                    "<draw:text-box><text:p><office:annotation><text:p>c</text:p></office:annotation></text:p></draw:text-box>",
                )),
                vec![],
            ),
            (
                "text box holding only a deletion",
                text(&frame(
                    r#"<draw:text-box><text:tracked-changes><text:changed-region text:id="c"><text:deletion><text:p>d</text:p></text:deletion></text:changed-region></text:tracked-changes><text:p/></draw:text-box>"#,
                )),
                vec![],
            ),
            (
                "page-anchored media after its preview",
                text(&frame(&format!("{image}<draw:plugin/>"))),
                vec![(GraphicUnread, BODY, None, 1)],
            ),
            (
                "notes picture outside a text box",
                slides(&format!(
                    "<draw:page><presentation:notes><draw:page-thumbnail/>{}</presentation:notes></draw:page>",
                    frame(image)
                )),
                vec![(ImageUnplaced, Region::Note, Some(1), 1)],
            ),
            (
                "text shape in speaker notes",
                slides(
                    "<draw:page><presentation:notes><draw:custom-shape><text:p>t</text:p></draw:custom-shape></presentation:notes></draw:page>",
                ),
                vec![(GraphicUnread, Region::Note, Some(1), 1)],
            ),
            (
                "notes object in a group",
                slides(&format!(
                    "<draw:page><presentation:notes><draw:g>{}</draw:g></presentation:notes></draw:page>",
                    frame(r#"<draw:object-ole xlink:href="./Object 3"/>"#)
                )),
                vec![(EmbeddedObject, Region::Note, Some(1), 1)],
            ),
            ("page-anchored chart", text(&frame(chart)), vec![(GraphicUnread, BODY, None, 1)]),
            (
                "page-anchored formula",
                text(&frame(&format!("<draw:object>{FORMULA}</draw:object>"))),
                vec![(EmbeddedObject, BODY, None, 1)],
            ),
            (
                "page-anchored shape text",
                text("<draw:custom-shape><text:p>s</text:p></draw:custom-shape>"),
                vec![(GraphicUnread, BODY, None, 1)],
            ),
            (
                "page-anchored group",
                text(&format!("<draw:g>{}<draw:rect/></draw:g>", frame(image))),
                vec![(ImageUnplaced, BODY, None, 1)],
            ),
            (
                "page-anchored control",
                text("<draw:control/>"),
                vec![(FormControlUnread, BODY, None, 1)],
            ),
            (
                "inline control",
                text(r#"<text:p>x<draw:control draw:control="c"/></text:p>"#),
                vec![(FormControlUnread, BODY, None, 1)],
            ),
            (
                "control the table does not declare",
                text(&format!(
                    "<table:table><table:table-row><table:table-cell>{}</table:table-cell></table:table-row></table:table>",
                    r#"<draw:control draw:control="c"/>"#
                )),
                vec![(FormControlUnread, BODY, None, 1)],
            ),
            (
                "mixed checkbox",
                sheet(&sheet_body(
                    r#"<draw:control draw:control="c"/>"#,
                    r#"<office:forms><form:form><form:checkbox xml:id="c" form:current-state="unknown"/></form:form></office:forms>"#,
                )),
                vec![(FormControlUnread, BODY, Some(1), 1)],
            ),
            (
                "other control",
                sheet(&sheet_body(
                    r#"<draw:control draw:control="c"/>"#,
                    r#"<office:forms><form:form><form:radio xml:id="c" form:label="r"/></form:form></office:forms>"#,
                )),
                vec![(FormControlUnread, BODY, Some(1), 1)],
            ),
            (
                "cell-anchored picture",
                sheet(&sheet_body(&frame(image), "")),
                vec![(ImageUnplaced, BODY, Some(1), 1)],
            ),
            (
                "cell-anchored chart",
                sheet(&sheet_body(&frame(chart), "")),
                vec![(GraphicUnread, BODY, Some(1), 1)],
            ),
            (
                "sheet shapes",
                sheet(&shapes(&format!(
                    "{}{}{}<draw:custom-shape><text:p>s</text:p></draw:custom-shape><draw:control/>",
                    frame(image),
                    frame(chart),
                    frame("<draw:text-box><text:p>t</text:p></draw:text-box>")
                ))),
                vec![
                    (ImageUnplaced, BODY, Some(1), 1),
                    (GraphicUnread, BODY, Some(1), 2),
                    (TextBoxUnread, BODY, Some(1), 1),
                    (FormControlUnread, BODY, Some(1), 1),
                ],
            ),
            (
                "slide chart",
                slides(&format!("<draw:page/><draw:page>{}</draw:page>", frame(&replaced(chart)))),
                vec![
                    (ImageUnavailable, Region::Body, Some(2), 1),
                    (GraphicUnread, Region::Body, Some(2), 1),
                ],
            ),
            (
                "slide link",
                slides(&format!(
                    r#"<draw:page><draw:a xlink:href="https://example.org">{}</draw:a></draw:page>"#,
                    frame("<draw:text-box><text:p>t</text:p></draw:text-box>")
                )),
                vec![(TextBoxUnread, BODY, Some(1), 1)],
            ),
            (
                "slide scene",
                slides(
                    "<draw:page><dr3d:scene><text:p>s</text:p></dr3d:scene><dr3d:scene/></draw:page>",
                ),
                vec![(GraphicUnread, BODY, Some(1), 1)],
            ),
            (
                "slide control",
                slides("<draw:page><draw:control/></draw:page>"),
                vec![(FormControlUnread, BODY, Some(1), 1)],
            ),
            (
                "slide title list",
                slides(
                    r#"<draw:page><draw:frame presentation:class="title"><draw:text-box><text:p>T</text:p><text:list><text:list-item><text:p>i</text:p></text:list-item></text:list></draw:text-box></draw:frame></draw:page>"#,
                ),
                vec![(BlockUnread, BODY, Some(1), 1)],
            ),
        ];
        for (name, mut odf, expected) in cases {
            odf.parts = &parts;
            assert_eq!(diagnose(&odf), expected, "{name}");
        }

        for (media, code) in [
            ("plugin", GraphicUnread),
            ("floating-frame", EmbeddedObject),
            ("applet", EmbeddedObject),
        ] {
            let content = format!("<draw:{media}/>{image}");
            let placed = [
                ("inline", text(&inline(&content)), None),
                ("page-anchored", text(&frame(&content)), None),
                ("slide", slides(&format!("<draw:page>{}</draw:page>", frame(&content))), Some(1)),
            ];
            for (anchor, mut odf, unit) in placed {
                odf.parts = &parts;
                let mut expected = vec![(code, BODY, unit, 1)];
                if anchor != "page-anchored" {
                    expected.insert(0, (DiagnosticCode::ImagePlaceholder, BODY, unit, 1));
                }
                assert_eq!(diagnose(&odf), expected, "{anchor} {media}");
            }
        }
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of formulas by where they render")]
    fn a_formula_reports_the_image_glyphs_it_drops_where_it_renders() {
        let math = |body: &str| {
            format!(
                r#"<math:math xmlns:math="http://www.w3.org/1998/Math/MathML">{body}</math:math>"#
            )
        };
        let (glyph, lone) = (math("<math:mi>x</math:mi><math:mglyph/>"), math("<math:mglyph/>"));
        let linked = format!(
            "<office:document-content {NS}><office:body>{glyph}</office:body></office:document-content>"
        );
        let parts = [("Pictures/a.png", "png"), ("Object 1/content.xml", linked.as_str())];
        let object = |inner: &str| format!("<draw:object>{inner}</draw:object>");
        let inline = |content: &str| format!("<text:p>t{}</text:p>", frame(content));
        let lost = |region, unit, count| vec![(UnmappedSymbol, region, unit, count)];
        let cases: Vec<(&str, Odf<'_>, &str, Vec<Reported>)> = vec![
            ("inline", text(&inline(&object(&glyph))), "t$x$\n", lost(BODY, None, 1)),
            (
                "linked",
                text(&inline(r#"<draw:object xlink:href="./Object 1"/>"#)),
                "t$x$\n",
                lost(BODY, None, 1),
            ),
            ("only glyphs", text(&inline(&object(&lone))), "t\n", lost(BODY, None, 1)),
            (
                "only glyphs before the picture",
                text(&inline(&format!(
                    r#"{}<draw:image xlink:href="Pictures/a.png"/><svg:title xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0">f</svg:title>"#,
                    object(&lone)
                ))),
                "t![f](asset:0)\n",
                vec![
                    (DiagnosticCode::ImagePlaceholder, BODY, None, 1),
                    (UnmappedSymbol, BODY, None, 1),
                ],
            ),
            ("empty", text(&inline(&object(&math("")))), "t\n", vec![]),
            (
                "in a note",
                text(&format!(
                    r#"<text:p>t<text:note text:id="n"><text:note-body>{}</text:note-body></text:note></text:p>"#,
                    inline(&object(&glyph))
                )),
                "t[^1]\n\n[^1]: t$x$\n",
                lost(Region::Note, None, 1),
            ),
            (
                "on a slide",
                slides(&format!("<draw:page>{}</draw:page>", frame(&object(&glyph)))),
                "$$\nx\n$$\n",
                lost(BODY, Some(1), 1),
            ),
            (
                "page-anchored",
                text(&frame(&object(&glyph))),
                "",
                vec![(EmbeddedObject, BODY, None, 1)],
            ),
            (
                "page-anchored, only glyphs",
                text(&frame(&object(&lone))),
                "",
                vec![(EmbeddedObject, BODY, None, 1)],
            ),
        ];
        for (name, mut odf, md, expected) in cases {
            odf.parts = &parts;
            assert_eq!(convert(&odf), (md.to_string(), expected), "{name}");
        }
    }

    #[test]
    fn an_object_part_reports_in_the_region_that_names_it() {
        let unclosed = format!("{FORMULA}<unclosed>");
        let parts = [("Object 1/content.xml", unclosed.as_str())];
        let body = format!(
            r#"<text:p>x<text:note text:id="n"><text:note-body><text:p>{}</text:p></text:note-body></text:note></text:p>"#,
            frame(r#"<draw:object xlink:href="./Object 1"/>"#)
        );
        let (doc, reported) = parsed(&Odf { parts: &parts, ..text(&body) });
        let [Block::Paragraph(note)] = &doc.notes[0].blocks[..] else {
            panic!("unexpected note: {:?}", doc.notes)
        };
        assert_eq!(note, &[]);
        assert_eq!(reported, [(EmbeddedObject, Region::Note, None, 1)]);
    }

    #[test]
    fn unplaced_objects_are_read_under_the_configured_limits() {
        let deep = format!("{}{}", "<d>".repeat(12), "</d>".repeat(12));
        let parts = [("Object 1/content.xml", deep.as_str())];
        let chart = frame(r#"<draw:object xlink:href="./Object 1"/>"#);
        let cases = [
            (
                "sheet shapes",
                sheet(&sheet_body("", &format!("<table:shapes>{chart}</table:shapes>"))),
            ),
            (
                "speaker notes",
                slides(&format!(
                    "<draw:page><presentation:notes>{chart}</presentation:notes></draw:page>"
                )),
            ),
            ("slide link", slides(&format!("<draw:page><draw:a>{chart}</draw:a></draw:page>"))),
        ];
        let limits = Limits { max_nesting_depth: 10, ..Limits::DEFAULT };
        for (name, mut odf) in cases {
            odf.parts = &parts;
            let err = parse(&ParseCtx::new(&limits), &odf.build()).unwrap_err();
            assert_matches!(
                err,
                ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 10 },
                "{name}: {err}"
            );
        }
    }

    #[test]
    fn a_lost_object_keeps_its_replacement_picture() {
        let parts = [("Pictures/a.png", "png"), ("Object 1/content.xml", CHART)];
        let body = format!(
            "<text:p>x{}</text:p>",
            frame(
                r#"<draw:object xlink:href="./Object 1"/><draw:image xlink:href="Pictures/a.png"/><svg:title xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0">bars</svg:title>"#
            )
        );
        let (md, reported) = convert(&Odf { parts: &parts, ..text(&body) });
        assert_eq!(md, "x![bars](asset:0)\n");
        assert_eq!(
            reported,
            [(DiagnosticCode::ImagePlaceholder, BODY, None, 1), (GraphicUnread, BODY, None, 1)]
        );
    }

    #[test]
    fn a_linked_inline_picture_keeps_its_target() {
        let parts = [("Pictures/a.png", "png")];
        let body = format!(
            r#"<text:p>see <draw:a xlink:href="https://example.org/t">{}</draw:a></text:p>"#,
            frame(r#"<draw:image xlink:href="Pictures/a.png"/>"#)
        );
        let doc =
            parse(&ParseCtx::default(), &Odf { parts: &parts, ..text(&body) }.build()).unwrap();
        let [Block::Paragraph(inlines)] = &doc.blocks[..] else {
            panic!("unexpected blocks: {:?}", doc.blocks)
        };
        let [_, Inline::Link { content, target }] = &inlines[..] else {
            panic!("unexpected inlines: {inlines:?}")
        };
        assert_eq!(*target, crate::model::LinkTarget::External("https://example.org/t".into()));
        assert_matches!(content[..], [Inline::Image { .. }]);
    }

    #[test]
    fn a_note_class_of_endnote_makes_an_endnote_and_any_other_a_footnote() {
        use crate::model::NoteKind;
        let kinds = |class: &str| {
            let note = format!(
                r#"<text:p><text:note text:note-class="{class}"><text:note-body><text:p>n</text:p></text:note-body></text:note></text:p>"#
            );
            let doc = parse(&ParseCtx::default(), &text(&note).build()).unwrap();
            doc.notes.iter().map(|note| note.kind).collect::<Vec<_>>()
        };
        assert_eq!(kinds("endnote"), [NoteKind::Endnote]);
        assert_eq!(kinds("footnote"), [NoteKind::Footnote]);
    }

    #[test]
    fn a_space_run_is_capped_at_twenty() {
        for (count, kept) in [("3", 3), ("20", 20), ("25", 20)] {
            let body = format!(r#"<text:p>a<text:s text:c="{count}"/>b</text:p>"#);
            let doc = parse(&ParseCtx::default(), &text(&body).build()).unwrap();
            let [Block::Paragraph(inlines)] = &doc.blocks[..] else {
                panic!("unexpected blocks: {:?}", doc.blocks)
            };
            let expected = format!("a{}b", " ".repeat(kept));
            assert_eq!(crate::model::inlines_to_plain_text(inlines), expected, "{count}");
        }
    }

    #[test]
    fn content_xml_redefines_a_style_styles_xml_defines() {
        let odf = Odf {
            head: r#"<office:automatic-styles><style:style style:name="S" style:family="text"><style:text-properties text:display="none"/></style:style></office:automatic-styles>"#,
            styles: r#"<office:styles><style:style style:name="S" style:family="text"><style:text-properties text:display="true"/></style:style></office:styles>"#,
            ..text(r#"<text:p>a<text:span text:style-name="S">b</text:span>c</text:p>"#)
        };
        assert_eq!(convert(&odf), ("ac\n".to_string(), vec![(HiddenOmitted, BODY, None, 1)]));
    }

    #[test]
    fn indexes_and_shapes_the_walkers_read_report_nothing() {
        let indexes: String = ["table-index", "object-index", "user-index"]
            .iter()
            .map(|index| format!("<text:{index}><text:index-body><text:p>{index}</text:p></text:index-body></text:{index}>"))
            .collect();
        let (md, reported) = convert(&text(&indexes));
        assert_eq!(md, "table-index\n\nobject-index\n\nuser-index\n");
        assert_eq!(reported, []);

        let kinds = [
            "table-of-content",
            "alphabetical-index",
            "bibliography",
            "illustration-index",
            "table-index",
            "object-index",
            "user-index",
        ];
        let indexes: String = kinds
            .iter()
            .map(|index| format!("<text:{index}><text:index-title><text:p>title</text:p></text:index-title><text:index-body><text:p>{index}</text:p></text:index-body></text:{index}>"))
            .collect();
        let (md, reported) = convert(&text(&indexes));
        let entries: Vec<String> = kinds.iter().map(|index| format!("title\n\n{index}")).collect();
        assert_eq!(md, format!("{}\n", entries.join("\n\n")));
        assert_eq!(reported, []);

        let shapes: String = ["circle", "polyline", "regular-polygon", "measure"]
            .iter()
            .map(|shape| format!("<draw:{shape}><text:p>{shape}</text:p></draw:{shape}>"))
            .collect();
        let (md, reported) = convert(&slides(&format!("<draw:page>{shapes}</draw:page>")));
        assert_eq!(md, "circle\n\npolyline\n\nregular-polygon\n\nmeasure\n");
        assert_eq!(reported, []);
    }

    #[test]
    fn a_slide_shape_holding_only_a_list_renders_the_list() {
        let list = "<text:list><text:list-item><text:p>item</text:p></text:list-item></text:list>";
        let (md, reported) =
            convert(&slides(&format!("<draw:page><draw:circle>{list}</draw:circle></draw:page>")));
        assert_eq!(md, "- item\n");
        assert_eq!(reported, []);
    }

    #[test]
    fn comments_and_deletions_are_omitted_and_counted() {
        let body = r#"<text:tracked-changes>
            <text:changed-region text:id="c1"><text:deletion><text:p>gone</text:p></text:deletion></text:changed-region>
            <text:changed-region text:id="c2"><text:insertion/></text:changed-region>
            <text:changed-region text:id="c3"><text:deletion><text:p>gone too</text:p></text:deletion></text:changed-region>
            </text:tracked-changes>
            <text:p>kept<text:tracked-changes><text:changed-region text:id="c4"><text:deletion><text:p>inline</text:p></text:deletion></text:changed-region></text:tracked-changes><office:annotation><text:p>remark</text:p></office:annotation><office:annotation-end/></text:p>"#;
        let (md, reported) = convert(&text(body));
        assert_eq!(md, "kept\n");
        assert_eq!(
            reported,
            [(CommentOmitted, Region::Comment, None, 1), (RevisionOmitted, BODY, None, 3)]
        );

        let cell = "<office:annotation><text:p>note</text:p></office:annotation>";
        assert_eq!(
            diagnose(&sheet(&sheet_body(cell, ""))),
            [(CommentOmitted, Region::Comment, Some(1), 1)]
        );
        let page = "<draw:page><office:annotation/><officeooo:annotation/></draw:page>";
        assert_eq!(diagnose(&slides(page)), [(CommentOmitted, Region::Comment, Some(1), 2)]);
    }

    const HIDING: &str = r#"<office:automatic-styles>
        <style:style style:name="Hide" style:family="text"><style:text-properties text:display="none"/></style:style>
        <style:style style:name="Show" style:family="text"><style:text-properties text:display="true"/></style:style>
        <style:style style:name="If" style:family="text"><style:text-properties text:display="condition" text:condition="ooow:x"/></style:style>
        <style:style style:name="Odd" style:family="text"><style:text-properties text:display="bogus"/></style:style>
        <style:style style:name="Heir" style:family="text" style:parent-style-name="Hide"/>
        <style:style style:name="Heir2" style:family="text" style:parent-style-name="Heir"/>
        <style:style style:name="Rebel" style:family="text" style:parent-style-name="Heir"><style:text-properties text:display="true"/></style:style>
        <style:style style:name="Gone" style:family="paragraph"><style:text-properties text:display="none"/></style:style>
        <style:style style:name="P1" style:family="paragraph" style:parent-style-name="Gone"/>
        <style:style style:name="Kept" style:family="paragraph"><style:text-properties text:display="true"/></style:style>
        <style:style style:name="OddP" style:family="paragraph"><style:text-properties text:display="bogus"/></style:style>
        <text:list-style style:name="Num"><text:list-level-style-number text:level="1" style:num-format="1"/></text:list-style>
        <text:list-style style:name="Deep"><text:list-level-style-number text:level="1" style:num-format="1"/><text:list-level-style-number text:level="2" style:num-format="1" text:display-levels="2"/></text:list-style>
        <text:list-style style:name="Split"><text:list-level-style-number text:level="1" style:num-format="1" text:start-value="4"/><text:list-level-style-number text:level="2" style:num-format="1" text:start-value="3" text:display-levels="2"/><text:list-level-style-number text:level="3" style:num-format="1" text:start-value="6" text:display-levels="3"/></text:list-style>
        <text:outline-style style:name="Outline"><text:outline-level-style text:level="1" style:num-format="1"/></text:outline-style>
        </office:automatic-styles>"#;

    fn hidden(count: u64) -> Vec<Reported> {
        vec![(HiddenOmitted, BODY, None, count)]
    }

    fn hiding(body: &str) -> Odf<'static> {
        Odf { head: HIDING, ..text(body) }
    }

    fn check(cases: Vec<(&str, Odf<'_>, &str, Vec<Reported>)>) {
        let parts = [("Pictures/a.png", "png")];
        for (name, mut odf, md, expected) in cases {
            odf.parts = &parts;
            assert_eq!(convert(&odf), (md.to_string(), expected), "{name}");
        }
    }

    const NUM: &str = r#"text:style-name="Num""#;

    fn item(attrs: &str, content: &str) -> String {
        format!("<text:list-item {attrs}>{content}</text:list-item>")
    }

    fn list(attrs: &str, items: &[String]) -> String {
        format!("<text:list {attrs}>{}</text:list>", items.concat())
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the table of hiding styles and the ranges they drop"
    )]
    fn hidden_text_drops_through_the_style_cascade_once_per_range() {
        let default_hides = r#"<office:styles><style:default-style style:family="paragraph"><style:text-properties text:display="none"/></style:default-style></office:styles>"#;
        check(vec![
            (
                "span",
                hiding(
                    r#"<text:p>a<text:span text:style-name="Hide">b<text:s/></text:span>c</text:p>"#,
                ),
                "ac\n",
                hidden(1),
            ),
            (
                "paragraph style under a shown span",
                hiding(
                    r#"<text:p text:style-name="Gone">a<text:span text:style-name="Show">b</text:span>c</text:p>"#,
                ),
                "b\n",
                hidden(2),
            ),
            (
                "double flip",
                hiding(
                    r#"<text:p text:style-name="Gone">a<text:span text:style-name="Show">b<text:span text:style-name="Hide">c</text:span>d</text:span>e</text:p>"#,
                ),
                "bd\n",
                hidden(3),
            ),
            (
                "parent style",
                hiding(r#"<text:p text:style-name="P1">a</text:p><text:p>b</text:p>"#),
                "b\n",
                hidden(1),
            ),
            (
                "default style",
                Odf {
                    styles: default_hides,
                    ..hiding(r#"<text:p>a</text:p><text:p text:style-name="Kept">b</text:p>"#)
                },
                "b\n",
                hidden(1),
            ),
            (
                "unknown value inherits",
                hiding(
                    r#"<text:p text:style-name="Gone">a<text:span text:style-name="Odd">b</text:span></text:p><text:p text:style-name="OddP">c</text:p>"#,
                ),
                "c\n",
                hidden(1),
            ),
            (
                "condition shows",
                hiding(
                    r#"<text:p text:style-name="Gone">a<text:span text:style-name="If">b</text:span></text:p>"#,
                ),
                "b\n",
                hidden(1),
            ),
            (
                "whitespace only",
                hiding(
                    r#"<text:p>a<text:span text:style-name="Hide"> <text:s text:c="3"/><text:tab/><text:line-break/></text:span>b</text:p>"#,
                ),
                "ab\n",
                vec![],
            ),
            (
                "empty",
                hiding(
                    r#"<text:p>a<text:span text:style-name="Hide"/><text:span text:style-name="Hide"><draw:frame/></text:span>b</text:p>"#,
                ),
                "ab\n",
                vec![],
            ),
            (
                "across a bookmark and an empty field",
                hiding(
                    r#"<text:p>a<text:span text:style-name="Hide">b</text:span><text:bookmark text:name="m"/><text:page-number/><text:span text:style-name="Hide">c</text:span>d</text:p>"#,
                ),
                "ad\n",
                hidden(1),
            ),
            (
                "split by a shown field result",
                hiding(
                    r#"<text:p><text:span text:style-name="Hide">a</text:span><text:page-number>3</text:page-number><text:span text:style-name="Hide">b</text:span></text:p>"#,
                ),
                "3\n",
                hidden(2),
            ),
            (
                "split by a shown space",
                hiding(
                    r#"<text:p>x<text:span text:style-name="Hide">a</text:span><text:s/><text:span text:style-name="Hide">b</text:span>y</text:p>"#,
                ),
                "x y\n",
                hidden(2),
            ),
            (
                "split by a shown tab, line break, note and picture",
                hiding(&format!(
                    "<text:p>x{h}a</text:span><text:tab/>{h}b</text:span><text:line-break/>{h}c</text:span><text:note><text:note-body><text:p>n</text:p></text:note-body></text:note>{h}d</text:span>{}{h}e</text:span>y</text:p>",
                    frame(
                        r#"<draw:image xlink:href="Pictures/a.png"/><svg:title xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0">f</svg:title>"#
                    ),
                    h = r#"<text:span text:style-name="Hide">"#
                )),
                "x \\\n[^1]![f](asset:0)y\n\n[^1]: n\n",
                vec![
                    (DiagnosticCode::ImagePlaceholder, BODY, None, 1),
                    (HiddenOmitted, BODY, None, 5),
                ],
            ),
            (
                "two levels of spans",
                hiding(
                    r#"<text:p>a<text:span text:style-name="Hide"><text:span><text:span>b</text:span></text:span></text:span></text:p>"#,
                ),
                "a\n",
                hidden(1),
            ),
            (
                "in a link in a field result",
                hiding(
                    r#"<text:p>x<text:meta-field><text:a xlink:href="https://www.example.org/"><text:span text:style-name="Hide">h</text:span></text:a></text:meta-field>y</text:p>"#,
                ),
                "xy\n",
                hidden(1),
            ),
            (
                "part of a link",
                hiding(
                    r#"<text:p><text:a xlink:href="https://www.example.org/">see <text:span text:style-name="Hide">gone </text:span>here</text:a></text:p>"#,
                ),
                "[see here](https://www.example.org/)\n",
                hidden(1),
            ),
            (
                "in a note",
                hiding(
                    r#"<text:p>a<text:note><text:note-body><text:p>n<text:span text:style-name="Hide">h</text:span></text:p></text:note-body></text:note></text:p>"#,
                ),
                "a[^1]\n\n[^1]: n\n",
                vec![(HiddenOmitted, Region::Note, None, 1)],
            ),
        ]);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of losses inside hidden content")]
    fn nothing_hidden_content_holds_reports() {
        let gone = frame(r#"<draw:image xlink:href="Pictures/gone.png"/>"#);
        check(vec![
            (
                "note",
                hiding(&format!(
                    r#"<text:p>a<text:span text:style-name="Hide"><text:note><text:note-body><text:p>n{gone}</text:p></text:note-body></text:note></text:span><text:note><text:note-body><text:p>m</text:p></text:note-body></text:note></text:p>"#
                )),
                "a[^1]\n\n[^1]: m\n",
                hidden(1),
            ),
            (
                "picture",
                hiding(&format!(
                    r#"<text:p>a<text:span text:style-name="Hide">{gone}</text:span></text:p>"#
                )),
                "a\n",
                hidden(1),
            ),
            (
                "text box",
                hiding(&format!(
                    r#"<text:p>a<text:span text:style-name="Hide">{}</text:span></text:p>"#,
                    frame("<draw:text-box><text:p>t</text:p></draw:text-box>")
                )),
                "a\n",
                hidden(1),
            ),
            (
                "shown text box beside hidden text",
                hiding(&format!(
                    r#"<text:p><text:span text:style-name="Hide">a</text:span>{}</text:p>"#,
                    frame("<draw:text-box><text:p>t</text:p></draw:text-box>")
                )),
                "t\n",
                hidden(1),
            ),
            (
                "comment and control",
                hiding(
                    r#"<text:p>a<text:span text:style-name="Hide"><office:annotation><text:p>c</text:p></office:annotation><draw:control draw:control="c"/></text:span></text:p>"#,
                ),
                "a\n",
                vec![],
            ),
            (
                "paragraph",
                hiding(&format!(
                    r#"<text:p>{gone}<text:note><text:note-body><text:p>n{gone}</text:p></text:note-body></text:note><text:hidden-paragraph text:is-hidden="true"/></text:p>"#
                )),
                "",
                hidden(1),
            ),
            (
                "section",
                hiding(&format!(
                    r#"<text:section text:display="none"><text:p>s{gone}<office:annotation><text:p>c</text:p></office:annotation></text:p></text:section>"#
                )),
                "",
                hidden(1),
            ),
        ]);
        let body = r#"<text:p>a<text:span text:style-name="Hide"><text:note><text:note-body><text:p>n</text:p></text:note-body></text:note></text:span><text:note><text:note-body><text:p>m</text:p></text:note-body></text:note></text:p>"#;
        let doc = parse(&ParseCtx::default(), &hiding(body).build()).unwrap();
        let ids: Vec<&str> = doc.notes.iter().map(|note| note.id.as_str()).collect();
        assert_eq!(ids, ["odf:a:0"]);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of hidden fields and sections")]
    fn hidden_fields_and_sections_drop_what_they_hide() {
        let gone = frame(r#"<draw:image xlink:href="Pictures/gone.png"/>"#);
        check(vec![
            (
                "hidden paragraph",
                hiding(
                    r#"<text:p>a<text:hidden-paragraph text:condition="ooow:x" text:is-hidden="true"/></text:p><text:p>b</text:p>"#,
                ),
                "b\n",
                hidden(1),
            ),
            (
                "hidden paragraph field in a span",
                hiding(
                    r#"<text:p>a<text:span><text:hidden-paragraph text:is-hidden="true"/></text:span></text:p>"#,
                ),
                "",
                hidden(1),
            ),
            (
                "shown paragraph fields",
                hiding(
                    r#"<text:p>a<text:hidden-paragraph text:condition="ooow:x"/></text:p><text:p>b<text:hidden-paragraph text:is-hidden="false"/></text:p><text:p>c<text:hidden-paragraph text:is-hidden="TRUE"/></text:p>"#,
                ),
                "a\n\nb\n\nc\n",
                vec![],
            ),
            (
                "a paragraph field shown by another",
                hiding(
                    r#"<text:p>a<text:hidden-paragraph text:is-hidden="true"/><text:span><text:hidden-paragraph text:is-hidden="false"/></text:span></text:p>"#,
                ),
                "a\n",
                vec![],
            ),
            (
                "empty hidden paragraph",
                hiding(
                    r#"<text:p><text:s/><text:hidden-paragraph text:is-hidden="true"/></text:p><text:p>b</text:p>"#,
                ),
                "b\n",
                vec![],
            ),
            (
                "picture-only hidden paragraph",
                hiding(&format!(
                    r#"<text:p>{gone}<text:hidden-paragraph text:is-hidden="true"/></text:p>"#
                )),
                "",
                hidden(1),
            ),
            (
                "note-only hidden paragraph",
                hiding(
                    r#"<text:p><text:note><text:note-citation/><text:note-body/></text:note><text:hidden-paragraph text:is-hidden="true"/></text:p>"#,
                ),
                "",
                hidden(1),
            ),
            (
                "hidden paragraph in a note",
                hiding(
                    r#"<text:p>a<text:note><text:note-body><text:p>n<text:hidden-paragraph text:is-hidden="true"/></text:p><text:p>m</text:p></text:note-body></text:note></text:p>"#,
                ),
                "a[^1]\n\n[^1]: m\n",
                vec![(HiddenOmitted, Region::Note, None, 1)],
            ),
            (
                "hidden paragraph in a text box",
                hiding(&format!(
                    "<text:p>a{}</text:p>",
                    frame(
                        r#"<draw:text-box><text:p>t<text:hidden-paragraph text:is-hidden="true"/></text:p></draw:text-box>"#
                    )
                )),
                "a\n",
                hidden(1),
            ),
            (
                "hidden text beside a hidden span",
                hiding(
                    r#"<text:p>a<text:hidden-text text:condition="ooow:x" text:string-value="b" text:is-hidden="true">b</text:hidden-text><text:span text:style-name="Hide">c</text:span>d</text:p>"#,
                ),
                "ad\n",
                hidden(1),
            ),
            (
                "shown hidden text",
                hiding(
                    r#"<text:p>a<text:hidden-text text:condition="ooow:x">b</text:hidden-text>c<text:hidden-text text:is-hidden="false">d</text:hidden-text><text:hidden-text text:is-hidden="1">e</text:hidden-text></text:p>"#,
                ),
                "abcde\n",
                vec![],
            ),
            (
                "whitespace hidden text",
                hiding(
                    r#"<text:p>a<text:hidden-text text:is-hidden="true"> </text:hidden-text>b</text:p>"#,
                ),
                "ab\n",
                vec![],
            ),
            (
                "hidden text over a shown span",
                hiding(
                    r#"<text:p>a<text:hidden-text text:is-hidden="true"><text:span text:style-name="Show">b</text:span></text:hidden-text></text:p>"#,
                ),
                "a\n",
                hidden(1),
            ),
            (
                "hidden section",
                hiding(
                    r#"<text:section text:name="S" text:display="none"><text:p>s</text:p></text:section><text:p>b</text:p>"#,
                ),
                "b\n",
                hidden(1),
            ),
            (
                "empty hidden section",
                hiding(
                    r#"<text:section text:display="none"><text:p><text:s/></text:p></text:section><text:p>b</text:p>"#,
                ),
                "b\n",
                vec![],
            ),
            (
                "shown sections",
                hiding(
                    r#"<text:section text:display="condition" text:condition="ooow:x"><text:p>c</text:p></text:section><text:section text:display="true"><text:p>t</text:p></text:section>"#,
                ),
                "c\n\nt\n",
                vec![],
            ),
        ]);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of dropped list paragraphs")]
    fn a_dropped_paragraph_takes_no_number_and_joins_nothing() {
        let (one, two) = (item("", "<text:p>one</text:p>"), item("", "<text:p>two</text:p>"));
        let field = item("", r#"<text:p>x<text:hidden-paragraph text:is-hidden="true"/></text:p>"#);
        check(vec![
            (
                "between paragraphs",
                hiding(
                    r#"<text:p>a</text:p><text:p text:style-name="Gone">b</text:p><text:p>c</text:p>"#,
                ),
                "a\n\nc\n",
                hidden(1),
            ),
            (
                "before a table and at a cell's end",
                hiding(
                    r#"<text:p>a</text:p><text:p>b<text:hidden-paragraph text:is-hidden="true"/></text:p><table:table><table:table-row><table:table-cell><text:p>c</text:p><text:p text:style-name="Gone">d</text:p></table:table-cell></table:table-row></table:table>"#,
                ),
                "a\n\n|  |\n| --- |\n| c |\n",
                hidden(2),
            ),
            (
                "list item",
                hiding(&list(NUM, &[
                    one.clone(),
                    item("", r#"<text:p text:style-name="Gone">x</text:p>"#),
                    two.clone(),
                ])),
                "1. one\n2. two\n",
                hidden(1),
            ),
            (
                "list item keeping a shown paragraph",
                hiding(&list(NUM, &[
                    item("", r#"<text:p>one</text:p><text:p text:style-name="Gone">x</text:p>"#),
                    two.clone(),
                ])),
                "1. one\n2. two\n",
                hidden(1),
            ),
            (
                "list item two levels down",
                hiding(&list(NUM, &[
                    one.clone(),
                    item("", &list("", &[field])),
                    two.clone(),
                ])),
                "1. one\n2. two\n",
                hidden(1),
            ),
            (
                "list header",
                hiding(&list(NUM, &[
                    one.clone(),
                    r#"<text:list-header><text:p text:style-name="Gone">h</text:p></text:list-header>"#
                        .to_string(),
                    two.clone(),
                ])),
                "1. one\n2. two\n",
                hidden(1),
            ),
            (
                "empty list items",
                hiding(&list(NUM, &[one, item("", "<text:p/>"), item("", ""), two])),
                "1. one\n2. \n3. \n4. two\n",
                vec![],
            ),
            (
                "headings",
                hiding(
                    r#"<text:h text:outline-level="1">A</text:h><text:h text:outline-level="1" text:style-name="Gone">B</text:h><text:h text:outline-level="1">C<text:hidden-paragraph text:is-hidden="true"/></text:h><text:h text:outline-level="1">D</text:h>"#,
                ),
                "# 1 A\n\n# 2 D\n",
                hidden(2),
            ),
        ]);
    }

    #[test]
    fn sheets_and_slides_drop_hidden_text_in_their_unit() {
        let cell =
            |attrs: &str, p: &str| format!("<table:table-cell {attrs}>{p}</table:table-cell>");
        let row = [
            cell(
                r#"office:value-type="float" office:value="3""#,
                r#"<text:p><text:span text:style-name="Hide">3</text:span></text:p>"#,
            ),
            cell(r#"office:value-type="float" office:value="4""#, ""),
            cell(
                r#"office:value-type="string" office:string-value="s""#,
                r#"<text:p>s<text:hidden-paragraph text:is-hidden="true"/></text:p>"#,
            ),
            cell("", "<text:p>v</text:p>"),
        ]
        .concat();
        let body = format!(
            r#"<table:table table:name="S"><table:table-row>{row}</table:table-row></table:table>"#
        );
        assert_eq!(
            convert(&Odf { head: HIDING, ..sheet(&body) }),
            (
                "|  |  |  |  |\n| --- | --- | --- | --- |\n|  | 4 |  | v |\n".to_string(),
                vec![(HiddenOmitted, BODY, Some(1), 2)]
            )
        );
        let page = r#"<draw:page/><draw:page><draw:frame><draw:text-box><text:p>a<text:span text:style-name="Hide">b</text:span></text:p></draw:text-box></draw:frame></draw:page>"#;
        assert_eq!(
            convert(&Odf { head: HIDING, ..slides(page) }),
            ("a\n".to_string(), vec![(HiddenOmitted, BODY, Some(2), 1)])
        );
    }

    #[test]
    fn invisible_characters_hold_nothing_hidden() {
        let z = "\u{feff}\u{200b}\u{ad}\u{90}";
        check(vec![
            (
                "hidden range",
                hiding(&format!(
                    r#"<text:p>a<text:span text:style-name="Hide">{z}</text:span>b</text:p>"#
                )),
                "ab\n",
                vec![],
            ),
            (
                "paragraph hidden by its style",
                hiding(&format!(
                    r#"<text:p text:style-name="Gone">{z}</text:p><text:p>b</text:p>"#
                )),
                "b\n",
                vec![],
            ),
            (
                "paragraph hidden by a field",
                hiding(&format!(
                    r#"<text:p>{z}<text:hidden-paragraph text:is-hidden="true"/></text:p><text:p>b</text:p>"#
                )),
                "b\n",
                vec![],
            ),
            (
                "hidden text",
                hiding(&format!(
                    r#"<text:p>a<text:hidden-text text:is-hidden="true">{z}</text:hidden-text>b</text:p>"#
                )),
                "ab\n",
                vec![],
            ),
            (
                "hidden section",
                hiding(&format!(
                    r#"<text:section text:display="none"><text:p>{z}</text:p></text:section><text:p>b</text:p>"#
                )),
                "b\n",
                vec![],
            ),
        ]);
    }

    #[test]
    fn a_shown_frame_that_yields_nothing_leaves_the_hidden_range_open() {
        let split = |frame: &str| {
            hiding(&format!(
                r#"<text:p>x<text:span text:style-name="Hide">a</text:span>{frame}<text:span text:style-name="Hide">b</text:span>y</text:p>"#
            ))
        };
        check(vec![
            (
                "text box with text",
                split(&frame("<draw:text-box><text:p>t</text:p></draw:text-box>")),
                "xy\n\nt\n",
                hidden(2),
            ),
            ("empty frame", split(&frame("")), "xy\n", hidden(1)),
            ("empty text box", split(&frame("<draw:text-box/>")), "xy\n", hidden(1)),
            (
                "text box whose text drops",
                split(&frame(
                    r#"<draw:text-box><text:p text:style-name="Gone">t</text:p></draw:text-box>"#,
                )),
                "xy\n",
                hidden(2),
            ),
        ]);
    }

    #[test]
    fn a_dropped_list_item_applies_no_start_value() {
        let one = item("", "<text:p>one</text:p>");
        let two = item("", "<text:p>two</text:p>");
        let gone = item(r#"text:start-value="5""#, r#"<text:p text:style-name="Gone">x</text:p>"#);
        check(vec![
            (
                "shown item",
                hiding(&list(
                    NUM,
                    &[one.clone(), item(r#"text:start-value="5""#, "<text:p>two</text:p>")],
                )),
                "1. one\n\n5) two\n",
                vec![],
            ),
            (
                "between items",
                hiding(&list(NUM, &[one.clone(), gone.clone(), two.clone()])),
                "1. one\n2. two\n",
                hidden(1),
            ),
            (
                "first item",
                hiding(&list(NUM, &[gone, one.clone(), two.clone()])),
                "1. one\n2. two\n",
                hidden(1),
            ),
            (
                "bullet item",
                hiding(&format!(
                    "<text:list>{one}{}</text:list>",
                    item(r#"text:start-value="5""#, "<text:p>two</text:p>")
                )),
                "- one\n- two\n",
                vec![],
            ),
            (
                "list header",
                hiding(&format!(
                    r#"<text:list text:style-name="Deep"><text:list-header text:start-value="5"><text:list>{two}</text:list></text:list-header></text:list>"#
                )),
                "- 1.1 two\n",
                vec![],
            ),
        ]);
    }

    #[test]
    fn a_list_nested_past_the_style_levels_labels_from_the_levels_its_style_displays() {
        let levels: String = (1..=10)
            .map(|n| format!(r#"<text:list-level-style-number text:level="{n}" style:num-format="1" text:display-levels="{n}"/>"#))
            .collect();
        let head = format!(
            r#"<office:automatic-styles><text:list-style style:name="Wide">{levels}</text:list-style></office:automatic-styles>"#
        );
        let nest = (0..12).fold(String::new(), |inner, _| {
            let content = format!("<text:p>x</text:p>{inner}");
            list(r#"text:style-name="Wide""#, &[item("", &content)])
        });
        let doc = parse(&ParseCtx::default(), &Odf { head: &head, ..text(&nest) }.build()).unwrap();
        let mut labels = Vec::new();
        let mut blocks = &doc.blocks[..];
        while let Some(Block::List(list)) = blocks.iter().find(|b| matches!(b, Block::List(_))) {
            labels.push(list.items[0].marker_label.clone());
            blocks = &list.items[0].blocks;
        }
        let expected: Vec<_> = (1..=12).map(|n| Some(["1"; 10][..n.min(10)].join("."))).collect();
        assert_eq!(labels, expected);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of list-only items by what precedes them")]
    fn an_item_holding_only_a_list_continues_the_item_before_it() {
        let p = |text: &str| item("", &format!("<text:p>{text}</text:p>"));
        let split = |attrs: &str, items: &[String]| item(attrs, &list("", items));
        let (sub, gap) = (r#"text:style-name="Split""#, "<text:p>gap</text:p>");
        let sibling = || list(NUM, &[p("one")]);
        let only = |text: &str| list(NUM, &[split("", &[p(text)])]);
        let with = |attrs: &str| format!("{sub} {attrs}");
        let gone = || item("", r#"<text:p text:style-name="Gone">h</text:p>"#);
        check(vec![
            (
                "the only item, after a paragraph",
                hiding(&format!("{gap}{}", only("x"))),
                "gap\n\n- x\n",
                vec![],
            ),
            (
                "the only item, after a sibling list",
                hiding(&format!("{}{}", sibling(), only("x"))),
                "1. one\n\n   - x\n",
                vec![],
            ),
            (
                "the only item, in a chain of three sibling lists",
                hiding(&[sibling(), only("x"), only("y")].concat()),
                "1. one\n\n   - x\n\n   * y\n",
                vec![],
            ),
            (
                "the only item, in a chain of four sibling lists",
                hiding(&[sibling(), only("x"), only("y"), only("z")].concat()),
                "1. one\n\n   - x\n\n   * y\n\n   - z\n",
                vec![],
            ),
            (
                "the only item, in a chain through one whose items all drop",
                hiding(&[sibling(), list(NUM, &[split("", &[gone()])]), only("y")].concat()),
                "1. one\n\n   - y\n",
                hidden(1),
            ),
            (
                "the only item, in a chain broken by a list header",
                hiding(
                    &[
                        sibling(),
                        only("x"),
                        list(
                            NUM,
                            &["<text:list-header><text:p>h</text:p></text:list-header>".into()],
                        ),
                        only("y"),
                        only("z"),
                    ]
                    .concat(),
                ),
                "1. one\n\n   - x\n\nh\n\n- y\n\n* z\n",
                vec![],
            ),
            (
                "the only item, after a sibling list holding only an unwrapped one",
                hiding(&format!("{gap}{}{}", only("x"), only("y"))),
                "gap\n\n- x\n\n* y\n",
                vec![],
            ),
            (
                "the only item, after a sibling list ending in one unwrapped after a dropped item",
                hiding(&format!(
                    "{}{}",
                    list(NUM, &[p("one"), gone(), split("", &[p("x")])]),
                    only("y")
                )),
                "1. one\n\n- x\n\n* y\n",
                hidden(1),
            ),
            (
                "the only item, after a sibling list ending in an empty list header",
                hiding(&format!(
                    "{}{}",
                    list(NUM, &[p("one"), "<text:list-header/>".into()]),
                    only("x")
                )),
                "1. one\n\n- x\n",
                vec![],
            ),
            (
                "two in a row",
                hiding(&list(
                    NUM,
                    &[p("one"), split("", &[p("a")]), split("", &[p("b")]), p("two")],
                )),
                "1. one\n\n   - a\n\n   * b\n\n2. two\n",
                vec![],
            ),
            (
                "an item with a paragraph after its list",
                hiding(&list(
                    NUM,
                    &[
                        p("one"),
                        item("", &format!("{}<text:p>m</text:p>", list("", &[p("a")]))),
                        p("two"),
                    ],
                )),
                "1. one\n\n2. - a\n\n   m\n\n3. two\n",
                vec![],
            ),
            (
                "an item with a heading after its list",
                hiding(&list(
                    NUM,
                    &[
                        p("one"),
                        item("", &format!("{}<text:h>m</text:h>", list("", &[p("a")]))),
                        p("two"),
                    ],
                )),
                "1. one\n\n2. - a\n\n   # 1 m\n\n3. two\n",
                vec![],
            ),
            (
                "one with a page break before its list",
                hiding(&list(
                    NUM,
                    &[
                        p("one"),
                        item("", &format!("<text:soft-page-break/>{}", list("", &[p("a")]))),
                        p("two"),
                    ],
                )),
                "1. one\n\n   - a\n\n2. two\n",
                vec![],
            ),
            (
                "an item with a number before its list",
                hiding(&list(
                    NUM,
                    &[
                        p("one"),
                        item("", &format!("<text:number>2.</text:number>{}", list("", &[p("a")]))),
                        p("two"),
                    ],
                )),
                "1. one\n2. - a\n3. two\n",
                vec![],
            ),
            (
                "one with a page break after its list",
                hiding(&list(
                    NUM,
                    &[
                        p("one"),
                        item("", &format!("{}<text:soft-page-break/>", list("", &[p("a")]))),
                        p("two"),
                    ],
                )),
                "1. one\n\n   - a\n\n2. two\n",
                vec![],
            ),
            (
                "an empty one",
                hiding(&list(NUM, &[p("one"), item("", "<text:list/>"), p("two")])),
                "1. one\n2. two\n",
                vec![],
            ),
            (
                "one holding a hidden paragraph",
                hiding(&list(
                    NUM,
                    &[
                        p("one"),
                        split(
                            "",
                            &[item("", r#"<text:p text:style-name="Gone">x</text:p>"#), p("y")],
                        ),
                        p("two"),
                    ],
                )),
                "1. one\n\n   - y\n\n2. two\n",
                hidden(1),
            ),
            (
                "after a start-value restart, with a start value of its own",
                hiding(&list(
                    sub,
                    &[
                        p("one"),
                        item(r#"text:start-value="7""#, "<text:p>seven</text:p>"),
                        split(r#"text:start-value="5""#, &[p("x")]),
                        p("eight"),
                    ],
                )),
                "4. one\n\n7) seven\n\n   - 7.3 x\n\n8) eight\n",
                vec![],
            ),
            (
                "first after a sibling list, with a start value of its own",
                hiding(&format!(
                    "{}{}",
                    list(sub, &[p("a"), p("b")]),
                    list(sub, &[split(r#"text:start-value="9""#, &[p("x")]), p("c")])
                )),
                "4. a\n\n5. b\n\n   - 5.3 x\n\n4) c\n",
                vec![],
            ),
            (
                "across continue-numbering and continue-list",
                hiding(&format!(
                    "{}{gap}{}{}",
                    list(
                        &format!(r#"{NUM} xml:id="a""#),
                        &[p("one"), split("", &[p("a")]), p("two")]
                    ),
                    list(&format!(r#"{NUM} text:continue-numbering="true""#), &[p("three")]),
                    list(&format!(r#"{NUM} text:continue-list="a""#), &[p("three again")])
                )),
                "1. one\n\n   - a\n\n2. two\n\ngap\n\n3. three\n\n3) three again\n",
                vec![],
            ),
            (
                "alone in a list, which stores no number to continue",
                hiding(
                    &[
                        list(&with(r#"xml:id="a""#), &[p("one"), p("two")]),
                        list(sub, &[split("", &[p("x")])]),
                        list(&with(r#"text:continue-numbering="true""#), &[p("six")]),
                        list(
                            &with(r#"xml:id="b" text:continue-list="a""#),
                            &[split("", &[p("y")])],
                        ),
                        list(&with(r#"text:continue-list="b""#), &[p("z")]),
                    ]
                    .join(gap),
                ),
                "4. one\n5. two\n\ngap\n\n- 4.3 x\n\ngap\n\n6. six\n\ngap\n\n- 4.3 y\n\ngap\n\n4. z\n",
                vec![],
            ),
            (
                "first and unattached in a list resuming its numbers",
                hiding(&format!(
                    "{}{gap}{}",
                    list(sub, &[p("one"), p("two")]),
                    list(
                        &with(r#"text:continue-numbering="true""#),
                        &[split(r#"text:start-value="8""#, &[p("x")]), p("six")]
                    )
                )),
                "4. one\n5. two\n\ngap\n\n- 4.3 x\n\n6. six\n",
                vec![],
            ),
            (
                "after a list header that follows a sibling list",
                hiding(&format!(
                    "{}{}",
                    sibling(),
                    list(
                        NUM,
                        &[
                            "<text:list-header><text:p>h</text:p></text:list-header>".into(),
                            split("", &[p("x")])
                        ]
                    )
                )),
                "1. one\n\nh\n\n- x\n",
                vec![],
            ),
            (
                "a list header holding only a list, after a sibling list",
                hiding(&format!(
                    "{}{}",
                    sibling(),
                    list(
                        NUM,
                        &[format!("<text:list-header>{}</text:list-header>", list("", &[p("x")]))]
                    )
                )),
                "1. one\n\n- x\n",
                vec![],
            ),
            (
                "inside a list header",
                hiding(&list(
                    sub,
                    &[format!(
                        "<text:list-header>{}</text:list-header>",
                        list("", &[p("a"), split("", &[p("b")])])
                    )],
                )),
                "- 4.3 a\n\n      - 4.3.6 b\n",
                vec![],
            ),
            (
                "first in a nested list after a paragraph",
                hiding(&list(
                    sub,
                    &[item(
                        "",
                        &format!(
                            "<text:p>one</text:p>{}",
                            list("", &[split(r#"text:start-value="8""#, &[p("deep")]), p("b")])
                        ),
                    )],
                )),
                "4. one\n\n   - 4.3.6 deep\n\n   * 4.3 b\n",
                vec![],
            ),
            (
                "first in a nested list after a nested list",
                hiding(&list(
                    sub,
                    &[item(
                        "",
                        &format!(
                            "<text:p>one</text:p>{}{}",
                            list("", &[item(r#"text:start-value="9""#, "<text:p>a</text:p>")]),
                            list("", &[split("", &[p("deep")]), p("b")])
                        ),
                    )],
                )),
                "4. one\n\n   - 4.9 a\n\n         - 4.9.6 deep\n\n   * 4.3 b\n",
                vec![],
            ),
            (
                "first in a nested list after a nested list and a hidden paragraph",
                hiding(&list(
                    sub,
                    &[item(
                        "",
                        &format!(
                            r#"<text:p>one</text:p>{}<text:p text:style-name="Gone">h</text:p>{}"#,
                            list("", &[item(r#"text:start-value="9""#, "<text:p>a</text:p>")]),
                            list("", &[split("", &[p("deep")]), p("b")])
                        ),
                    )],
                )),
                "4. one\n\n   - 4.9 a\n\n   * 4.3.6 deep\n\n   - 4.3 b\n",
                hidden(1),
            ),
            (
                "first in a nested list after a nested list whose items all drop",
                hiding(&list(
                    sub,
                    &[item(
                        "",
                        &format!(
                            "<text:p>one</text:p>{}{}{}",
                            list("", &[item(r#"text:start-value="9""#, "<text:p>a</text:p>")]),
                            list("", &[gone()]),
                            list("", &[split("", &[p("deep")]), p("b")])
                        ),
                    )],
                )),
                "4. one\n\n   - 4.9 a\n\n   * 4.3.6 deep\n\n   - 4.3 b\n",
                hidden(1),
            ),
            (
                "after a dropped item",
                hiding(&list(
                    NUM,
                    &[p("one"), gone(), split("", &[p("x")]), split("", &[p("y")]), p("two")],
                )),
                "1. one\n\n- x\n\n* y\n\n2. two\n",
                hidden(1),
            ),
            (
                "first after a dropped item, after a sibling list",
                hiding(&format!("{}{}", sibling(), list(NUM, &[gone(), split("", &[p("x")])]))),
                "1. one\n\n- x\n",
                hidden(1),
            ),
            (
                "after a dropped item and a shown one",
                hiding(&list(NUM, &[p("one"), gone(), p("two"), split("", &[p("x")])])),
                "1. one\n\n2. two\n\n   - x\n",
                hidden(1),
            ),
            (
                "holding only a dropped item, after a dropped item",
                hiding(&list(NUM, &[p("one"), gone(), split("", &[gone()]), p("two")])),
                "1. one\n2. two\n",
                hidden(2),
            ),
            (
                "holding only a dropped item, before another",
                hiding(&list(
                    NUM,
                    &[p("one"), split("", &[gone()]), split("", &[p("x")]), p("two")],
                )),
                "1. one\n\n   - x\n\n2. two\n",
                hidden(1),
            ),
            (
                "first after a sibling list and a hidden paragraph",
                hiding(&format!(
                    r#"{}<text:p text:style-name="Gone">h</text:p>{}"#,
                    sibling(),
                    only("x")
                )),
                "1. one\n\n- x\n",
                hidden(1),
            ),
            (
                "first after a sibling list and a hidden section",
                hiding(&format!(
                    r#"{}<text:section text:display="none"><text:p>s</text:p></text:section>{}"#,
                    sibling(),
                    only("x")
                )),
                "1. one\n\n- x\n",
                hidden(1),
            ),
            (
                "first after a section ending in a list",
                hiding(&format!("<text:section>{}</text:section>{}", sibling(), only("x"))),
                "1. one\n\n- x\n",
                vec![],
            ),
            (
                "first after a sibling list whose items all drop",
                hiding(&[sibling(), list(NUM, &[gone()]), only("x")].concat()),
                "1. one\n\n- x\n",
                hidden(1),
            ),
            (
                "first after a sibling list and a page break",
                hiding(&format!("{}<text:soft-page-break/>{}", sibling(), only("x"))),
                "1. one\n\n   - x\n",
                vec![],
            ),
            (
                "first after a sibling list, a hidden paragraph and a page break",
                hiding(&format!(
                    r#"{}<text:p text:style-name="Gone">h</text:p><text:soft-page-break/>{}"#,
                    sibling(),
                    only("x")
                )),
                "1. one\n\n- x\n",
                hidden(1),
            ),
            (
                "first after a sibling list whose last item drops, which it still continues",
                hiding(&format!("{}{}", list(NUM, &[p("one"), gone()]), only("x"))),
                "1. one\n\n   - x\n",
                hidden(1),
            ),
            (
                "first after an empty list header that follows a sibling list",
                hiding(&format!(
                    "{}{}",
                    sibling(),
                    list(NUM, &["<text:list-header/>".into(), split("", &[p("x")])])
                )),
                "1. one\n\n- x\n",
                vec![],
            ),
            (
                "after an empty list header",
                hiding(&list(
                    NUM,
                    &[p("one"), "<text:list-header/>".into(), split("", &[p("x")]), p("two")],
                )),
                "1. one\n\n- x\n\n2. two\n",
                vec![],
            ),
            (
                "nested in another",
                hiding(&list(sub, &[p("one"), split("", &[split("", &[p("deep")])]), p("two")])),
                "4. one\n\n   - 4.3.6 deep\n\n5. two\n",
                vec![],
            ),
        ]);
    }

    #[test]
    fn a_paragraph_field_two_levels_down_hides_its_paragraph() {
        let field = |hidden: &str| format!(r#"<text:hidden-paragraph text:is-hidden="{hidden}"/>"#);
        let deep = |content: &str| {
            format!(
                r#"<text:span><text:a xlink:href="https://www.example.org/">l{content}</text:a></text:span>"#
            )
        };
        check(vec![
            (
                "hidden",
                hiding(&format!("<text:p>a{}</text:p><text:p>b</text:p>", deep(&field("true")))),
                "b\n",
                hidden(1),
            ),
            (
                "shown by another",
                hiding(&format!("<text:p>a{}{}</text:p>", field("true"), deep(&field("false")))),
                "a[l](https://www.example.org/)\n",
                vec![],
            ),
        ]);
    }

    #[test]
    fn a_bookmark_in_hidden_content_drops() {
        let link = r##"<text:p><text:a xlink:href="#m">to m</text:a></text:p>"##;
        let (one, two) = (
            "<text:list-item><text:p>one</text:p></text:list-item>",
            "<text:list-item><text:p>two</text:p></text:list-item>",
        );
        check(vec![
            (
                "in a hidden range",
                hiding(&format!(
                    r#"{link}<text:p>a<text:span text:style-name="Hide">b<text:bookmark text:name="m"/></text:span>c</text:p>"#
                )),
                "to m\n\nac\n",
                vec![
                    (HiddenOmitted, BODY, None, 1),
                    (DiagnosticCode::DanglingReference, BODY, None, 1),
                ],
            ),
            (
                "alone in a hidden range",
                hiding(&format!(
                    r#"{link}<text:p>a<text:span text:style-name="Hide"><text:bookmark-start text:name="m"/></text:span>c</text:p>"#
                )),
                "to m\n\nac\n",
                vec![(DiagnosticCode::DanglingReference, BODY, None, 1)],
            ),
            (
                "alone in a hidden list item",
                hiding(&format!(
                    r#"<text:list text:style-name="Num">{one}<text:list-item><text:p text:style-name="Gone"><text:bookmark text:name="m"/></text:p></text:list-item>{two}</text:list>"#
                )),
                "1. one\n2. two\n",
                vec![],
            ),
        ]);
    }

    #[test]
    fn a_span_style_hides_through_its_parent_text_style() {
        check(vec![(
            "parent and grandparent",
            hiding(
                r#"<text:p>a<text:span text:style-name="Heir">b</text:span><text:span text:style-name="Heir2">c</text:span><text:span text:style-name="Rebel">d</text:span>e</text:p>"#,
            ),
            "ade\n",
            hidden(1),
        )]);
    }

    #[test]
    fn a_formatting_change_is_neither_applied_nor_counted() {
        let body = r#"<text:tracked-changes><text:changed-region text:id="c1"><text:format-change><office:change-info/></text:format-change></text:changed-region></text:tracked-changes><text:p><text:change-start text:change-id="c1"/>kept<text:change-end text:change-id="c1"/></text:p>"#;
        assert_eq!(convert(&text(body)), ("kept\n".to_string(), vec![]));
    }

    #[test]
    fn running_headers_and_footers_are_omitted_where_shown() {
        let masters = r#"<office:master-styles>
            <style:master-page style:name="Standard"><style:header><text:p>Head</text:p></style:header><style:footer style:display="false"><text:p>Foot</text:p></style:footer></style:master-page>
            <style:master-page style:name="Unused"><style:header><text:p>H</text:p></style:header><style:footer><text:p>F</text:p></style:footer></style:master-page>
            <style:master-page style:name="Named" style:next-style-name="Next"><style:footer-left><text:p>F</text:p></style:footer-left></style:master-page>
            <style:master-page style:name="Next" style:next-style-name="Named"><style:header-first><text:p>H</text:p></style:header-first></style:master-page>
            </office:master-styles>"#;
        let named = r#"<office:automatic-styles><style:style style:name="Chapter" style:family="paragraph" style:master-page-name="Named"/><style:style style:name="P1" style:family="paragraph" style:parent-style-name="Chapter"/></office:automatic-styles>"#;
        let odt = Odf { styles: masters, head: named, ..text("<text:p>x</text:p>") };
        assert_eq!(diagnose(&odt), [(HeaderOmitted, Region::Header, None, 1)]);
        let odt = Odf { body: r#"<text:p text:style-name="P1">x</text:p>"#.into(), ..odt };
        assert_eq!(
            diagnose(&odt),
            [(HeaderOmitted, Region::Header, None, 2), (FooterOmitted, Region::Footer, None, 1)]
        );
        let blank = r#"<office:master-styles><style:master-page style:name="Blank"><style:header><text:p> </text:p></style:header></style:master-page></office:master-styles>"#;
        assert_eq!(diagnose(&Odf { styles: blank, ..text("<text:p>x</text:p>") }), []);
        let footed = r#"<office:master-styles><style:master-page style:name="Standard"/><style:master-page style:name="Footed"><style:footer><text:p>F</text:p></style:footer></style:master-page></office:master-styles>"#;
        let starts = [
            ("paragraph", r#"<text:h text:style-name="S">x</text:h>"#),
            (
                "table",
                r#"<table:table table:style-name="S"><table:table-row><table:table-cell><text:p>v</text:p></table:table-cell></table:table-row></table:table>"#,
            ),
        ];
        for (family, body) in starts {
            let head = format!(
                r#"<office:automatic-styles><style:style style:name="S" style:family="{family}" style:master-page-name="Footed"/></office:automatic-styles>"#
            );
            let odt = Odf { styles: footed, head: &head, ..text(body) };
            assert_eq!(diagnose(&odt), [(FooterOmitted, Region::Footer, None, 1)], "{family}");
        }
    }

    #[test]
    fn running_headers_and_footers_are_omitted_where_a_sheet_shows_them() {
        let sheet_masters = r#"<office:master-styles>
            <style:master-page style:name="First"><style:header><text:p>H</text:p></style:header></style:master-page>
            <style:master-page style:name="Second"><style:footer><text:p>F</text:p></style:footer></style:master-page>
            </office:master-styles>"#;
        let table_styles = r#"<office:automatic-styles><style:style style:name="ta2" style:family="table" style:master-page-name="Second"/><style:style style:name="ta0" style:family="table" style:master-page-name=""/></office:automatic-styles>"#;
        let sheets = [r#"table:style-name="ta2""#, "", r#"table:style-name="ta0""#]
            .map(|style| sheet_body("", "").replace(r#"table:name="S""#, style))
            .concat();
        let ods = Odf { head: table_styles, styles: sheet_masters, ..sheet(&sheets) };
        assert_eq!(
            diagnose(&ods),
            [
                (HeaderOmitted, Region::Header, Some(2), 1),
                (HeaderOmitted, Region::Header, Some(3), 1),
                (FooterOmitted, Region::Footer, Some(1), 1)
            ]
        );
    }

    #[test]
    fn running_headers_and_footers_are_omitted_where_a_slide_shows_them() {
        let slide = r#"<draw:page draw:master-page-name="Logo">
            <draw:frame presentation:class="footer"><draw:text-box><text:p>f</text:p></draw:text-box></draw:frame>
            <draw:frame presentation:class="page-number"/>
            <draw:frame presentation:class="date-time"><draw:text-box><text:p>d</text:p></draw:text-box></draw:frame>
            <presentation:notes><draw:frame presentation:class="header"><draw:text-box><text:p>notes head</text:p></draw:text-box></draw:frame>
            <draw:frame><draw:text-box><text:p>spoken</text:p></draw:text-box></draw:frame></presentation:notes></draw:page>"#;
        let decks = r#"<office:automatic-styles><style:style style:name="bg" style:family="drawing-page"><style:drawing-page-properties draw:fill="bitmap"/></style:style><style:style style:name="hide" style:family="drawing-page"><style:drawing-page-properties presentation:display-footer="false"/></style:style><style:style style:name="no-objects" style:family="drawing-page"><style:drawing-page-properties presentation:background-objects-visible="false"/></style:style><style:style style:name="no-background" style:family="drawing-page"><style:drawing-page-properties presentation:background-visible="false"/></style:style></office:automatic-styles>
            <office:master-styles>
            <style:master-page style:name="Logo"><draw:frame presentation:class="title"><draw:text-box/></draw:frame><draw:frame><draw:image xlink:href="Pictures/logo.png"/></draw:frame></style:master-page>
            <style:master-page style:name="Backdrop" draw:style-name="bg"/>
            <style:master-page style:name="Plain"><draw:frame presentation:class="footer"><draw:text-box><text:p>f</text:p></draw:text-box></draw:frame></style:master-page>
            <style:master-page style:name="Rule"><draw:rect/></style:master-page>
            <style:master-page style:name="Mark"><draw:rect/></style:master-page>
            <style:master-page style:name="Wash" draw:style-name="bg"/>
            </office:master-styles>"#;
        let declared = r#"<draw:page draw:master-page-name="Backdrop" presentation:use-footer-name="f1" presentation:use-date-time-name="d1"/>
            <draw:page draw:master-page-name="Plain" draw:style-name="hide" presentation:use-footer-name="f1"/>
            <draw:page draw:master-page-name="Rule" presentation:use-header-name="h0" presentation:use-footer-name="f0"/>
            <draw:page draw:master-page-name="Mark" draw:style-name="no-objects"/>
            <draw:page draw:master-page-name="Wash" draw:style-name="no-background"/>
            <presentation:header-decl presentation:name="h0"/>
            <presentation:footer-decl presentation:name="f0"><text:p> </text:p></presentation:footer-decl>
            <presentation:footer-decl presentation:name="f1">Confidential</presentation:footer-decl>
            <presentation:date-time-decl presentation:name="d1" presentation:source="current-date"/>"#;
        let deck = format!("{slide}{declared}");
        let (md, reported) = convert(&Odf { styles: decks, ..slides(&deck) });
        assert_eq!(md, "> spoken\n");
        assert_eq!(
            reported,
            [
                (HeaderOmitted, Region::Header, None, 3),
                (HeaderOmitted, Region::Header, Some(1), 1),
                (FooterOmitted, Region::Footer, Some(1), 3),
                (FooterOmitted, Region::Footer, Some(2), 2)
            ]
        );
    }

    #[test]
    fn a_slide_naming_no_master_page_shows_the_documents_first() {
        let masters = r#"<office:master-styles><style:master-page style:name="First"><draw:rect/></style:master-page><style:master-page style:name="Second"/></office:master-styles>"#;
        for (label, slide, reported) in [
            ("absent", "<draw:page/>", vec![(HeaderOmitted, Region::Header, None, 1)]),
            (
                "empty",
                r#"<draw:page draw:master-page-name=""/>"#,
                vec![(HeaderOmitted, Region::Header, None, 1)],
            ),
            ("named", r#"<draw:page draw:master-page-name="Second"/>"#, vec![]),
            ("unknown", r#"<draw:page draw:master-page-name="Gone"/>"#, vec![]),
        ] {
            assert_eq!(diagnose(&Odf { styles: masters, ..slides(slide) }), reported, "{label}");
        }
    }

    #[test]
    fn stray_covered_cells_are_repaired_once_per_row_copy() {
        let body = r#"<table:table table:name="S"><table:table-row table:number-rows-repeated="3"><table:table-cell><text:p>a</text:p></table:table-cell><table:covered-table-cell table:number-columns-repeated="2"/></table:table-row></table:table>"#;
        assert_eq!(diagnose(&sheet(body)), [(TableSpanRepaired, BODY, Some(1), 6)]);
    }

    #[test]
    fn each_sheet_and_slide_reports_in_its_own_unit() {
        let lost = frame(r#"<draw:image xlink:href="Pictures/gone.png"/>"#);
        let sheets = format!(
            r#"<table:table table:name="A"/>{}"#,
            sheet_body(&lost, "").replace("\"S\"", "\"B\"")
        );
        assert_eq!(diagnose(&sheet(&sheets)), [(ImageUnplaced, BODY, Some(2), 1)]);
        let pages = format!("<draw:page/><draw:page/><draw:page>{lost}</draw:page>");
        assert_eq!(diagnose(&slides(&pages)), [(ImageUnavailable, BODY, Some(3), 1)]);
    }

    const HIDING_SHEETS: &str = r#"<office:automatic-styles>
        <style:style style:name="Hid" style:family="table"><style:table-properties table:display="false"/></style:style>
        <style:style style:name="Shown" style:family="table"><style:table-properties table:display="true"/></style:style>
        <style:style style:name="Child" style:family="table" style:parent-style-name="Hid"/>
        <style:style style:name="Reopened" style:family="table" style:parent-style-name="Hid"><style:table-properties table:display="true"/></style:style>
        <style:style style:name="Loop" style:family="table" style:parent-style-name="Loop"/>
        </office:automatic-styles>"#;

    fn cell(text: &str) -> String {
        format!("<table:table-cell><text:p>{text}</text:p></table:table-cell>")
    }

    fn row(attrs: &str, texts: &[&str]) -> String {
        let cells: String = texts.iter().map(|text| cell(text)).collect();
        format!("<table:table-row {attrs}>{cells}</table:table-row>")
    }

    fn table(name: &str, attrs: &str, body: &str) -> String {
        format!(r#"<table:table table:name="{name}" {attrs}>{body}</table:table>"#)
    }

    fn lone_sheet(body: &str) -> Odf<'static> {
        sheet(&table("S", "", body))
    }

    fn omitted(count: u64) -> Vec<Reported> {
        vec![(HiddenOmitted, BODY, Some(1), count)]
    }

    /// The first table's grid as text, `text:columns x rows` for a span and
    /// `~` for a covered position.
    fn table_text(doc: &Document) -> Vec<Vec<String>> {
        use crate::model::{CellSlot, inlines_to_plain_text};
        let table = doc.blocks.iter().find_map(|block| match block {
            Block::Table(table) => Some(table),
            _ => None,
        });
        let text = |cell: &crate::model::Cell| -> String {
            let paragraphs = cell.blocks.iter().filter_map(|block| match block {
                Block::Paragraph(inlines) => Some(inlines_to_plain_text(inlines)),
                _ => None,
            });
            paragraphs.collect()
        };
        table.map_or_else(Vec::new, |table| {
            let slot = |slot: &CellSlot| match slot {
                CellSlot::Covered { .. } => "~".to_string(),
                CellSlot::Origin(cell) => match (cell.col_span, cell.row_span) {
                    (1, 1) => text(cell),
                    (cols, rows) => format!("{}:{cols}x{rows}", text(cell)),
                },
            };
            table.grid.iter().map(|row| row.iter().map(slot).collect()).collect()
        })
    }

    fn shape(odf: &Odf<'_>) -> (Vec<Vec<String>>, Vec<Reported>) {
        let (doc, reported) = parsed(odf);
        (table_text(&doc), reported)
    }

    fn sheet_shape(hidden_cols: &[bool], rows: &str) -> (Vec<Vec<String>>, Vec<Reported>) {
        shape(&lone_sheet(&format!("{}{rows}", columns(hidden_cols))))
    }

    fn grid(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter().map(|row| row.iter().map(ToString::to_string).collect()).collect()
    }

    fn shown_sheets(body: &str) -> Odf<'static> {
        Odf { head: HIDING_SHEETS, ..sheet(body) }
    }

    #[test]
    fn a_sheet_whose_table_style_sets_display_false_is_skipped_and_reported_once() {
        let body = [
            table("A", "", &row("", &["a"])),
            table("B", r#"table:style-name="Hid""#, &row("", &["gone"])),
            table("C", r#"table:style-name="Shown""#, &row("", &["c"])),
        ]
        .concat();
        let (doc, reported) = parsed(&shown_sheets(&body));
        let md = document_to_markdown(&doc);
        assert_eq!(md, "## A\n\n|  |\n| --- |\n| a |\n\n## C\n\n|  |\n| --- |\n| c |\n");
        assert_eq!(reported, [(HiddenOmitted, BODY, Some(2), 1)]);
        assert_eq!(unit_ordinals(&doc), [1, 3]);
    }

    #[test]
    fn a_sheets_heading_needs_a_second_sheet_that_shows() {
        let body = [
            table("A", "", &row("", &["a"])),
            table("B", r#"table:style-name="Hid""#, &row("", &["gone"])),
        ]
        .concat();
        let (md, reported) = convert(&shown_sheets(&body));
        assert_eq!(md, "|  |\n| --- |\n| a |\n");
        assert_eq!(reported, [(HiddenOmitted, BODY, Some(2), 1)]);
    }

    #[test]
    fn a_child_style_inherits_its_parents_display_until_it_sets_its_own() {
        let body = [
            table("A", r#"table:style-name="Child""#, &row("", &["gone"])),
            table("B", r#"table:style-name="Reopened""#, &row("", &["b"])),
            table("C", r#"table:style-name="Missing""#, &row("", &["c"])),
        ]
        .concat();
        let (md, reported) = convert(&shown_sheets(&body));
        assert_eq!(md, "## B\n\n|  |\n| --- |\n| b |\n\n## C\n\n|  |\n| --- |\n| c |\n");
        assert_eq!(reported, [(HiddenOmitted, BODY, Some(1), 1)]);
    }

    #[test]
    fn the_default_table_style_decides_every_sheet_that_sets_no_display() {
        let styles = r#"<office:styles><style:default-style style:family="table"><style:table-properties table:display="false"/></style:default-style></office:styles>"#;
        let body = [
            table("A", "", &row("", &["gone"])),
            table("B", r#"table:style-name="Shown""#, &row("", &["b"])),
            table("C", r#"table:style-name="Child""#, &row("", &["gone"])),
        ]
        .concat();
        let (md, reported) = convert(&Odf { styles, ..shown_sheets(&body) });
        assert_eq!(md, "|  |\n| --- |\n| b |\n");
        assert_eq!(
            reported,
            [(HiddenOmitted, BODY, Some(1), 1), (HiddenOmitted, BODY, Some(3), 1)]
        );
    }

    #[test]
    fn a_table_style_that_inherits_from_itself_fails_only_where_a_sheet_uses_it() {
        let used = table("A", r#"table:style-name="Loop""#, &row("", &["a"]));
        let err = parse(&ParseCtx::default(), &shown_sheets(&used).build()).unwrap_err();
        assert_matches!(err, ConvertError::Malformed { .. });
        let unused = table("A", "", &row("", &["a"]));
        parse(&ParseCtx::default(), &shown_sheets(&unused).build()).unwrap();
    }

    #[test]
    fn a_hidden_sheet_starts_no_master_page_and_reads_none_of_its_content() {
        let head = |display: &str| {
            format!(
                r#"<office:automatic-styles>
                <style:style style:name="Hid" style:family="table" style:master-page-name="Headed"><style:table-properties table:display="{display}"/></style:style>
                <style:style style:name="Open" style:family="table" style:master-page-name="Plain"/>
                </office:automatic-styles>"#
            )
        };
        let masters = r#"<office:master-styles><style:master-page style:name="Plain"/><style:master-page style:name="Headed"><style:header><text:p>running</text:p></style:header></style:master-page></office:master-styles>"#;
        let note = r#"<text:note text:id="n1" text:note-class="footnote"><text:note-citation>1</text:note-citation><text:note-body><text:p>body</text:p></text:note-body></text:note>"#;
        let body = [
            table("A", r#"table:style-name="Open""#, &row("", &["a"])),
            table("B", r#"table:style-name="Hid""#, &row("", &[&format!("b{note}")])),
        ]
        .concat();
        fn odf<'s>(head: &'s str, styles: &'s str, body: &str) -> Odf<'s> {
            Odf { head, styles, ..sheet(body) }
        }
        let (doc, reported) = parsed(&odf(&head("false"), masters, &body));
        assert_eq!(document_to_markdown(&doc), "|  |\n| --- |\n| a |\n");
        assert_eq!(reported, [(HiddenOmitted, BODY, Some(2), 1)]);
        assert_eq!(doc.notes.len(), 0);
        let (md, reported) = convert(&odf(&head("true"), masters, &body));
        assert_eq!(
            md,
            "## A\n\n|  |\n| --- |\n| a |\n\n## B\n\n|  |\n| --- |\n| b[^1] |\n\n[^1]: body\n"
        );
        assert_eq!(reported, [(HeaderOmitted, Region::Header, Some(2), 1)]);
    }

    #[test]
    fn collapsed_and_filtered_rows_drop_and_any_other_token_keeps_the_row() {
        let rows = [
            row(COLLAPSED, &["x1"]),
            row(r#"table:visibility="filter""#, &["x2"]),
            row("table:visibility=\"&#9;collapse&#10;\"", &["x3"]),
            row(r#"table:visibility="visible""#, &["k1"]),
            row(r#"table:visibility="Collapse""#, &["k2"]),
            row(r#"table:visibility="hidden""#, &["k3"]),
            row(r#"table:visibility="""#, &["k4"]),
            row("", &["k5"]),
        ]
        .concat();
        assert_eq!(
            convert(&lone_sheet(&rows)),
            ("| k1 |\n| --- |\n| k2 |\n| k3 |\n| k4 |\n| k5 |\n".to_string(), omitted(3))
        );
    }

    #[test]
    fn a_hidden_row_group_hides_every_row_inside_and_no_nested_group_reopens_one() {
        let group = |display: &str, inner: &str| {
            format!(
                r#"<table:table-row-group table:display="{display}">{inner}</table:table-row-group>"#
            )
        };
        let hidden = [
            row("", &["x1"]),
            group("true", &row("", &["x2"])),
            group("TRUE", &row("", &["x3"])),
            format!("<table:table-rows>{}</table:table-rows>", row("", &["x4"])),
            format!("<table:table-header-rows>{}</table:table-header-rows>", row("", &["x5"])),
        ]
        .concat();
        let rows = [
            group("false", &hidden),
            group("true", &row("", &["k1"])),
            group("unknown", &row("", &["k2"])),
            group(" false\n", &row("", &["x6"])),
            row("", &["k3"]),
        ]
        .concat();
        assert_eq!(
            convert(&lone_sheet(&rows)),
            ("| k1 |\n| --- |\n| k2 |\n| k3 |\n".to_string(), omitted(6))
        );
    }

    #[test]
    fn a_header_row_group_counts_only_the_header_rows_that_show() {
        let header =
            [row(r#"table:visibility="collapse""#, &["x"]), row("", &["H1"]), row("", &["H2"])]
                .concat();
        let rows = format!(
            "{}<table:table-header-rows>{header}</table:table-header-rows>{}{}",
            row("", &["top"]),
            row("", &["d1"]),
            row("", &["d2"])
        );
        let doc = parse(&ParseCtx::default(), &lone_sheet(&rows).build()).unwrap();
        let [_, Block::Table(table)] = doc.blocks.as_slice() else { panic!("one table expected") };
        assert_eq!((table.grid.len(), table.header_rows), (5, 2));
    }

    #[test]
    fn visible_empty_rows_keep_their_places_around_the_rows_that_hide() {
        let empty = "<table:table-row><table:table-cell/></table:table-row>";
        let rows = [
            empty.to_string(),
            row(r#"table:visibility="collapse""#, &["x1"]),
            row("", &["a"]),
            empty.to_string(),
            row(r#"table:visibility="collapse""#, &["x2"]),
            row("", &["b"]),
        ]
        .concat();
        assert_eq!(shape(&lone_sheet(&rows)), (grid(&[&[], &["a"], &[], &["b"]]), omitted(2)));
    }

    #[test]
    fn a_run_of_hidden_rows_or_columns_is_never_enumerated() {
        let huge = "4000000000000000000";
        let rows = [
            row(
                &format!(r#"table:number-rows-repeated="{huge}" table:visibility="collapse""#),
                &["x"],
            ),
            row("", &["k"]),
        ]
        .concat();
        let expected = (grid(&[&["k"]]), omitted(4_000_000_000_000_000_000));
        assert_eq!(shape(&lone_sheet(&rows)), expected);
        let columns = format!(
            r#"<table:table-column table:number-columns-repeated="{huge}" table:visibility="collapse"/><table:table-column/>"#
        );
        let cells = format!(
            r#"<table:table-row><table:table-cell table:number-columns-repeated="{huge}"><text:p>x</text:p></table:table-cell>{}</table:table-row>"#,
            cell("k")
        );
        assert_eq!(shape(&lone_sheet(&format!("{columns}{cells}"))), expected);
    }

    #[test]
    fn collapsed_filtered_and_grouped_columns_drop_with_their_cells() {
        let columns = r#"<table:table-column/>
            <table:table-column table:visibility="collapse"/>
            <table:table-column-group table:display="false">
                <table:table-column/>
                <table:table-column-group table:display="true"><table:table-column table:visibility="visible"/></table:table-column-group>
            </table:table-column-group>
            <table:table-header-columns><table:table-column table:visibility="filter"/></table:table-header-columns>
            <table:table-columns><table:table-column table:number-columns-repeated="2"/></table:table-columns>"#;
        let cells = row("", &["a", "x1", "x2", "x3", "x4", "e", "f", "g"]);
        assert_eq!(
            shape(&lone_sheet(&format!("{columns}{cells}"))),
            (grid(&[&["a", "e", "f", "g"]]), omitted(4))
        );
    }

    #[test]
    fn a_cell_repeated_across_hidden_columns_keeps_only_the_copies_that_show() {
        let columns = r#"<table:table-column/><table:table-column table:number-columns-repeated="2" table:visibility="collapse"/><table:table-column/>"#;
        let repeated = r#"<table:table-row><table:table-cell table:number-columns-repeated="4"><text:p>x</text:p></table:table-cell></table:table-row>"#;
        assert_eq!(
            shape(&lone_sheet(&format!("{columns}{repeated}"))),
            (grid(&[&["x", "x"]]), omitted(2))
        );
        let covered = r#"<table:table-row><table:table-cell table:number-columns-spanned="4"><text:p>m</text:p></table:table-cell><table:covered-table-cell table:number-columns-repeated="3"/><table:table-cell><text:p>t</text:p></table:table-cell></table:table-row>"#;
        assert_eq!(
            shape(&lone_sheet(&format!("{columns}{covered}"))),
            (grid(&[&["m:2x1", "~", "t"]]), Vec::new())
        );
    }

    fn columns(hidden: &[bool]) -> String {
        hidden
            .iter()
            .map(|&hidden| {
                let attrs = if hidden { r#" table:visibility="collapse""# } else { "" };
                format!("<table:table-column{attrs}/>")
            })
            .collect()
    }

    fn origin(text: &str, cols: u32, rows: u32) -> String {
        let text = if text.is_empty() { String::new() } else { format!("<text:p>{text}</text:p>") };
        format!(
            r#"<table:table-cell table:number-columns-spanned="{cols}" table:number-rows-spanned="{rows}">{text}</table:table-cell>"#
        )
    }

    fn covered(count: u32) -> String {
        format!(r#"<table:covered-table-cell table:number-columns-repeated="{count}"/>"#)
    }

    fn raw_row(attrs: &str, cells: &[&str]) -> String {
        format!("<table:table-row {attrs}>{}</table:table-row>", cells.concat())
    }

    const COLLAPSED: &str = r#"table:visibility="collapse""#;

    #[test]
    fn a_span_counts_only_the_columns_and_rows_it_covers_that_show() {
        let wide = raw_row("", &[&origin("m", 3, 1), &covered(2)]);
        assert_eq!(
            sheet_shape(&[false, true, false], &format!("{wide}{}", row("", &["a", "b", "c"]))),
            (grid(&[&["m:2x1", "~"], &["a", "c"]]), omitted(1))
        );
        let tall = [
            raw_row("", &[&origin("m", 1, 3), &cell("a")]),
            raw_row(COLLAPSED, &[&covered(1), &cell("b")]),
            raw_row("", &[&covered(1), &cell("c")]),
        ]
        .concat();
        assert_eq!(
            sheet_shape(&[false, false], &tall),
            (grid(&[&["m:1x2", "a"], &["~", "c"]]), omitted(1))
        );
    }

    #[test]
    fn a_merge_origin_in_a_hidden_column_moves_to_the_first_column_the_merge_shows() {
        let rows = [
            raw_row("", &[&origin("m", 2, 1), &covered(1), &cell("x")]),
            row("", &["a", "b", "c"]),
        ]
        .concat();
        assert_eq!(
            sheet_shape(&[true, false, false], &rows),
            (grid(&[&["m", "x"], &["b", "c"]]), omitted(1))
        );
        let rows = [
            raw_row("", &[&origin("", 3, 1), &covered(2), &cell("t")]),
            row("", &["a", "b", "c", "d"]),
        ]
        .concat();
        assert_eq!(
            sheet_shape(&[true, false, false, false], &rows),
            (grid(&[&[":2x1", "~", "t"], &["b", "c", "d"]]), omitted(1))
        );
    }

    #[test]
    fn a_merge_origin_in_a_hidden_row_moves_to_the_first_row_the_merge_shows() {
        let rows = [
            raw_row(COLLAPSED, &[&origin("m", 1, 2), &cell("x")]),
            raw_row("", &[&covered(1), &cell("y")]),
            row("", &["p", "q"]),
        ]
        .concat();
        assert_eq!(shape(&lone_sheet(&rows)), (grid(&[&["m", "y"], &["p", "q"]]), omitted(1)));
    }

    #[test]
    fn a_merge_origin_hidden_on_both_axes_moves_to_the_first_row_and_column_that_show() {
        let rows = [
            raw_row(COLLAPSED, &[&origin("m", 2, 2), &covered(1), &cell("z")]),
            raw_row("", &[&covered(2), &cell("w")]),
            row("", &["a", "b", "c"]),
        ]
        .concat();
        assert_eq!(
            sheet_shape(&[true, false, false], &rows),
            (grid(&[&["m", "w"], &["b", "c"]]), omitted(2))
        );
    }

    #[test]
    fn a_merge_with_no_row_or_column_that_shows_goes_with_its_content() {
        let rows = [
            raw_row(COLLAPSED, &[&origin("m", 1, 2)]),
            raw_row(COLLAPSED, &[&covered(1)]),
            row("", &["k"]),
        ]
        .concat();
        assert_eq!(shape(&lone_sheet(&rows)), (grid(&[&["k"]]), omitted(1)));
        let rows = raw_row("", &[&origin("m", 2, 1), &covered(1), &cell("k")]);
        assert_eq!(sheet_shape(&[true, true, false], &rows), (grid(&[&["k"]]), omitted(1)));
    }

    #[test]
    fn an_unplaced_moved_merge_origin_counts_only_when_it_holds_content() {
        let down = |text| {
            let rows = [raw_row(COLLAPSED, &[&origin(text, 1, 2)]), row("", &["k"])].concat();
            sheet_shape(&[false], &rows)
        };
        assert_eq!(down(""), (grid(&[&["k"]]), Vec::new()));
        assert_eq!(down("m"), (grid(&[&["k"]]), omitted(1)));
        let across =
            |text| sheet_shape(&[true, false], &raw_row("", &[&origin(text, 2, 1), &cell("n")]));
        assert_eq!(across(""), (grid(&[&["n"]]), Vec::new()));
        assert_eq!(across("m"), (grid(&[&["n"]]), omitted(1)));
    }

    #[test]
    fn a_moved_merge_origin_lands_after_the_empty_cells_before_it() {
        let cells = ["<table:table-cell/>", &origin("m", 2, 1), &covered(1), &cell("x")];
        assert_eq!(
            sheet_shape(&[false, true, false], &raw_row("", &cells)),
            (grid(&[&["", "m", "x"]]), Vec::new())
        );
    }

    #[test]
    fn an_empty_moved_merge_origin_still_holds_its_position() {
        let rows = raw_row("", &[&origin("", 2, 1), &covered(1), &cell("x")]);
        assert_eq!(sheet_shape(&[true, false, false], &rows), (grid(&[&["", "x"]]), Vec::new()));
    }

    #[test]
    fn empty_cells_repeated_across_hidden_columns_leave_a_gap_per_copy_that_shows() {
        let gap = r#"<table:table-cell table:number-columns-repeated="3"/>"#;
        let rows = raw_row("", &[gap, &cell("x")]);
        assert_eq!(
            sheet_shape(&[false, true, false], &rows),
            (grid(&[&["", "", "x"]]), Vec::new())
        );
    }

    #[test]
    fn two_merge_origins_moving_to_one_position_leave_the_first_out_and_counted() {
        let rows = raw_row("", &[&origin("m", 3, 1), &origin("n", 2, 1), &covered(1)]);
        assert_eq!(sheet_shape(&[true, true, false], &rows), (grid(&[&["n"]]), omitted(1)));
    }

    #[test]
    fn a_merge_origin_set_aside_and_never_placed_reads_none_of_its_content() {
        let payload = r#"<text:p>m<text:note text:id="n1" text:note-class="footnote"><text:note-citation>1</text:note-citation><text:note-body><text:p>note</text:p></text:note-body></text:note></text:p><draw:frame><draw:object xlink:href="./Missing"/></draw:frame>"#;
        let loaded =
            |spans: &str| format!("<table:table-cell {spans}>{payload}</table:table-cell>");
        let orphaned =
            [raw_row(COLLAPSED, &[&loaded(r#"table:number-rows-spanned="2""#)]), row("", &["k"])]
                .concat();
        let replaced = raw_row(
            "",
            &[&loaded(r#"table:number-columns-spanned="3""#), &origin("n", 2, 1), &covered(1)],
        );
        let hidden_columns = format!("{}{replaced}", columns(&[true, true, false]));
        for (name, odf, kept) in [
            ("orphaned", lone_sheet(&orphaned), "k"),
            ("replaced", lone_sheet(&hidden_columns), "n"),
        ] {
            let (doc, reported) = parsed(&odf);
            assert_eq!(doc.notes.len(), 0, "{name}");
            assert_eq!((table_text(&doc), reported), (grid(&[&[kept]]), omitted(1)), "{name}");
        }
    }

    #[test]
    fn a_merge_origin_repeated_in_a_hidden_row_is_left_out_with_each_copy() {
        let rows = [
            raw_row(
                &format!(r#"{COLLAPSED} table:number-rows-repeated="2""#),
                &[&origin("m", 1, 3)],
            ),
            row("", &["k"]),
        ]
        .concat();
        assert_eq!(shape(&lone_sheet(&rows)), (grid(&[&["k"]]), omitted(2)));
    }

    #[test]
    fn a_merge_origin_repeated_across_hidden_columns_is_left_out_with_each_copy() {
        let repeated = r#"<table:table-cell table:number-columns-repeated="2" table:number-columns-spanned="3"><text:p>m</text:p></table:table-cell>"#;
        let rows = raw_row("", &[repeated, &cell("x")]);
        assert_eq!(sheet_shape(&[true, true, false], &rows), (grid(&[&["x"]]), omitted(2)));
    }

    #[test]
    fn a_moved_merge_origin_spans_the_rows_it_covers_that_show() {
        let rows = [
            raw_row("", &[&origin("m", 2, 2), &covered(1), &cell("x")]),
            raw_row("", &[&covered(2), &cell("y")]),
        ]
        .concat();
        assert_eq!(
            sheet_shape(&[true, false, false], &rows),
            (grid(&[&["m:1x2", "x"], &["~", "y"]]), Vec::new())
        );
    }

    #[test]
    fn hidden_rows_and_columns_count_where_they_hold_content() {
        let columns = r#"<table:table-column/><table:table-column table:number-columns-repeated="16383" table:visibility="collapse"/>"#;
        let rows = [
            row("", &["a", "b", "c"]),
            row(COLLAPSED, &["h"]),
            raw_row(COLLAPSED, &["<table:table-cell/>"]),
        ]
        .concat();
        assert_eq!(shape(&lone_sheet(&format!("{columns}{rows}"))), (grid(&[&["a"]]), omitted(3)));
    }

    #[test]
    fn a_cell_in_a_hidden_row_and_a_hidden_column_counts_on_both_axes() {
        let rows = [row(COLLAPSED, &["x", "y"]), raw_row("", &["<table:table-cell/>", &cell("q")])]
            .concat();
        assert_eq!(sheet_shape(&[true, false], &rows), (grid(&[&["q"]]), omitted(2)));
        let rows = [row("", &["a", "k1"]), row("", &["b", "k2"])].concat();
        assert_eq!(sheet_shape(&[true, false], &rows), (grid(&[&["k1"], &["k2"]]), omitted(1)));
    }

    #[test]
    fn each_repeat_of_a_hidden_row_that_holds_content_counts_and_empty_ones_count_nothing() {
        let rows = [
            row(&format!(r#"{COLLAPSED} table:number-rows-repeated="3""#), &["x"]),
            row(&format!(r#"{COLLAPSED} table:number-rows-repeated="2""#), &["", " "]),
            raw_row(
                COLLAPSED,
                &[r#"<table:table-cell office:value-type="string" office:string-value="s"/>"#],
            ),
            row("", &["k"]),
        ]
        .concat();
        assert_eq!(shape(&lone_sheet(&rows)), (grid(&[&["k"]]), omitted(4)));
        let empties = [
            raw_row(COLLAPSED, &["<table:table-cell><text:p/></table:table-cell>"]),
            raw_row(COLLAPSED, &[r#"<table:table-cell office:value-type="string"/>"#]),
            raw_row(
                COLLAPSED,
                &[r#"<table:table-cell office:value-type="float" office:value="x"/>"#],
            ),
            row("", &["k"]),
        ]
        .concat();
        assert_eq!(shape(&lone_sheet(&empties)), (grid(&[&["k"]]), Vec::new()));
    }

    #[test]
    fn what_a_hidden_cell_holds_is_never_read() {
        let payload = r#"<text:p>v<text:note text:id="n1" text:note-class="footnote"><text:note-citation>1</text:note-citation><text:note-body><text:p>note</text:p></text:note-body></text:note></text:p><draw:frame><draw:object xlink:href="./Missing"/></draw:frame><draw:frame><draw:image xlink:href="Pictures/gone.png"/></draw:frame><office:annotation><text:p>c</text:p></office:annotation>"#;
        let loaded = format!("<table:table-cell>{payload}</table:table-cell>");
        let kept = row("", &["k"]);
        let (_, visible) = sheet_shape(&[], &format!("{kept}{}", raw_row("", &[&loaded])));
        assert!(visible.iter().any(|d| d.0 == EmbeddedObject), "{visible:?}");
        let hidden_row = [kept.clone(), raw_row(COLLAPSED, &[&loaded])].concat();
        let hidden_column = [kept, raw_row("", &["<table:table-cell/>", &loaded])].concat();
        for (name, odf) in [
            ("row", lone_sheet(&hidden_row)),
            ("column", lone_sheet(&format!("{}{hidden_column}", columns(&[false, true])))),
        ] {
            let (doc, reported) = parsed(&odf);
            assert_eq!((doc.notes.len(), doc.assets.len()), (0, 0), "{name}");
            assert_eq!(reported, omitted(1), "{name}");
        }
    }

    #[test]
    fn a_text_documents_table_and_a_table_in_a_cell_keep_their_collapsed_lines() {
        let collapsed = [
            r#"<table:table-column table:visibility="collapse"/><table:table-column/>"#,
            r#"<table:table-row table:visibility="collapse"><table:table-cell><text:p>a</text:p></table:table-cell><table:table-cell><text:p>b</text:p></table:table-cell></table:table-row>"#,
            "<table:table-row><table:table-cell><text:p>c</text:p></table:table-cell><table:table-cell><text:p>d</text:p></table:table-cell></table:table-row>",
        ]
        .concat();
        let plain = collapsed.replace(r#" table:visibility="collapse""#, "");
        let tabled = |rows: &str| table("T", "", rows);
        let expected = convert(&text(&tabled(&plain)));
        assert_eq!(convert(&text(&tabled(&collapsed))), expected);
        assert!(expected.0.contains("| a | b |"), "{}", expected.0);
        let nested = |rows: &str| {
            let inner = tabled(rows);
            lone_sheet(&format!(
                "<table:table-row><table:table-cell>{inner}</table:table-cell></table:table-row>"
            ))
        };
        let expected = convert(&nested(&plain));
        assert_eq!(convert(&nested(&collapsed)), expected);
        assert!(expected.0.contains('a'), "{}", expected.0);
    }

    #[test]
    fn an_unreadable_picture_without_alt_keeps_its_inline_position() {
        let image = frame(r#"<draw:image xlink:href="Pictures/missing.png"/>"#);
        let body = format!("<text:p>before{image}after</text:p>");
        let (markdown, diagnostics) = convert(&text(&body));
        assert_eq!(markdown, "before\\[Image\\]after\n");
        assert_eq!(diagnostics, [(ImageUnavailable, BODY, None, 1)]);
        let (markdown, diagnostics) =
            convert(&text(&format!("<text:section text:display=\"none\">{body}</text:section>")));
        assert_eq!(markdown, "");
        assert!(diagnostics.iter().all(|d| d.0 == HiddenOmitted));
    }
    #[test]
    fn speaker_note_images_keep_source_origins_under_every_policy() {
        use crate::model::{ImageSource, UnitKind};
        use crate::{Config, Converter, Format, ImagePolicy, Input, OnLoss};
        let title = r#"<svg:title xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0">f</svg:title>"#;
        for (source, expected_source, placeholder_code) in [
            (
                r#"<draw:image xlink:href="Pictures/a.png"/>"#,
                ImageSource::Asset(crate::model::AssetId(0)),
                DiagnosticCode::ImagePlaceholder,
            ),
            (r#"<draw:image xlink:href="missing.png"/>"#, ImageSource::Unread, ImageUnavailable),
            ("", ImageSource::Unavailable, DiagnosticCode::ImagePlaceholder),
            (
                r#"<draw:image xlink:href="javascript:bad"/>"#,
                ImageSource::External("javascript:bad".into()),
                DiagnosticCode::UnsafeLink,
            ),
        ] {
            let body = format!(
                "<draw:page><presentation:notes>{}</presentation:notes></draw:page>",
                frame(&format!(
                    "<draw:text-box><text:p>before{}after</text:p></draw:text-box>",
                    frame(&format!("{source}{title}"))
                ))
            );
            let parts = [("Pictures/a.png", "png")];
            let bytes = Odf { parts: &parts, ..slides(&body) }.build();
            for images in [ImagePolicy::Placeholder, ImagePolicy::AltText, ImagePolicy::Omit] {
                let config = Config { images, on_loss: OnLoss::Report, ..Config::default() };
                let converter = Converter::new(config).unwrap();
                let parsed = converter.parse(Input::new(&bytes, Format::Odp)).unwrap();
                let [Block::UnitStart(_), Block::BlockQuote(notes)] = &parsed.document().blocks[..]
                else {
                    panic!("unexpected speaker notes")
                };
                let [Block::Paragraph(content)] = &notes[..] else {
                    panic!("unexpected speaker text")
                };
                let Inline::Image { source, origin, .. } = &content[1] else {
                    panic!("picture position lost")
                };
                assert_eq!(*source, expected_source);
                assert_eq!(origin.region, Region::Note);
                let unit = origin.unit.unwrap();
                assert_eq!((unit.kind, unit.ordinal.get()), (UnitKind::Slide, 1));
                let diagnostics: Vec<Reported> = diagnostic_rows(parsed.diagnostics());
                let code = if images == ImagePolicy::Placeholder {
                    placeholder_code
                } else {
                    DiagnosticCode::ImageOmitted
                };
                assert_eq!(diagnostics, [(code, Region::Note, Some(1), 1)]);
                assert_eq!(
                    parsed.render().unwrap(),
                    converter.convert(Input::new(&bytes, Format::Odp)).unwrap()
                );
            }
        }
    }

    type Outcome = (bool, Vec<Reported>);

    fn outcome(bytes: &[u8], format: crate::Format) -> Outcome {
        let converter = crate::Converter::new(crate::Config::default()).unwrap();
        match converter.convert(crate::Input::new(bytes, format)) {
            Ok(done) => (true, diagnostic_rows(done.diagnostics())),
            Err(ConvertError::Incomplete { diagnostics }) => (false, diagnostic_rows(&diagnostics)),
            Err(other) => panic!("neither converted nor incomplete: {other}"),
        }
    }

    fn lost_styles() -> Outcome {
        (false, vec![(PartUnreadable, BODY, None, 1)])
    }

    fn formatting_styles() -> Outcome {
        (true, vec![(StylePartUnreadable, Region::Container, None, 1)])
    }

    fn unclosed_styles(inner: &str) -> String {
        format!("<office:document-styles {NS}>{inner}")
    }

    fn hiding_text_style() -> String {
        unclosed_styles(
            r#"<office:styles><style:style style:name="Hid" style:family="text"><style:text-properties text:display="none"/></style:style></office:styles>"#,
        )
    }

    fn sheet_with_styles(styles: &[u8]) -> Vec<u8> {
        let body = row("", &[r#"a<text:span text:style-name="Hid">secret</text:span>"#]);
        let content = sheet(&table("Secret", r#"table:style-name="Hid""#, &body)).content();
        zip_package(&[("content.xml", content.as_bytes()), ("styles.xml", styles)])
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the table of styles parts that do not parse and what each declares"
    )]
    fn a_spreadsheet_styles_part_that_does_not_parse_is_a_loss_where_it_may_hide() {
        let style = |kind: &str, family: &str, properties: &str| {
            format!(
                r#"<office:styles><style:{kind} style:name="Hid" style:family="{family}">{properties}</style:{kind}></office:styles>"#
            )
        };
        let named_style = |family: &str, properties: &str| style("style", family, properties);
        let default_style =
            |family: &str, properties: &str| style("default-style", family, properties);
        let table_props = |attributes: &str| format!("<style:table-properties {attributes}/>");
        let text_props = |attributes: &str| format!("<style:text-properties {attributes}/>");
        let hides_text = text_props(r#"text:display="none""#);
        let cases = [
            (
                "a default table style that hides",
                default_style("table", &table_props(r#"table:display="false""#)),
                lost_styles(),
            ),
            (
                "a named table style that hides",
                named_style("table", &table_props(r#"table:display="false""#)),
                lost_styles(),
            ),
            ("a named text style that hides", named_style("text", &hides_text), lost_styles()),
            (
                "a default paragraph style that hides",
                default_style("paragraph", &hides_text),
                lost_styles(),
            ),
            (
                "a table display that shows, before one that hides",
                named_style(
                    "table",
                    &(table_props(r#"table:display="true""#)
                        + &table_props(r#"table:display="false""#)),
                ),
                lost_styles(),
            ),
            (
                "a table display of false in white space",
                named_style("table", &table_props("table:display=' false\n'")),
                lost_styles(),
            ),
            (
                "a properties element and a display in another prefix",
                named_style("table", r#"<s:table-properties s:display="false"/>"#),
                lost_styles(),
            ),
            (
                "a table display held as an entity",
                named_style("table", &table_props(r#"table:display="&#102;alse""#)),
                lost_styles(),
            ),
            (
                "a text display held as an entity",
                named_style("text", &text_props(r#"text:display="n&#111;ne""#)),
                lost_styles(),
            ),
            (
                "an unquoted display",
                named_style("text", &text_props(r#"fo:color="red" text:display=none"#)),
                lost_styles(),
            ),
            (
                "an attribute with no value before a display that shows",
                named_style("text", &text_props(r#"bogus text:display="true""#)),
                lost_styles(),
            ),
            (
                "an attribute with no value after one read",
                named_style("text", &text_props(r#"fo:color="red" bogus"#)),
                lost_styles(),
            ),
            (
                "a greater-than sign in a value before a display that hides",
                named_style("text", &text_props(r#"fo:font-family="a>b" text:display="none""#)),
                lost_styles(),
            ),
            (
                "a hiding display after a comment",
                named_style("text", &format!("<!-- c -->{hides_text}")),
                lost_styles(),
            ),
            (
                "a hiding display after a processing instruction that opens a comment",
                named_style("text", &format!("<?pi <!--?>{hides_text}")),
                lost_styles(),
            ),
            (
                "a table display of true",
                named_style("table", &table_props(r#"table:display="true""#)),
                formatting_styles(),
            ),
            (
                "a text display of true",
                named_style("text", &text_props(r#"text:display="true""#)),
                formatting_styles(),
            ),
            (
                "a text display of condition",
                named_style("text", &text_props(r#"text:display="condition""#)),
                formatting_styles(),
            ),
            (
                "a text display of None",
                named_style("text", &text_props(r#"text:display="None""#)),
                formatting_styles(),
            ),
            (
                "a text display of false",
                named_style("text", &text_props(r#"text:display="false""#)),
                formatting_styles(),
            ),
            (
                "a table display of none",
                named_style("table", &table_props(r#"table:display="none""#)),
                formatting_styles(),
            ),
            (
                "an attribute that only ends in display",
                named_style("table", &table_props(r#"table:displays="false""#)),
                formatting_styles(),
            ),
            (
                "a properties element that sets no display",
                named_style("text", &text_props(r#"fo:font-weight="bold""#)),
                formatting_styles(),
            ),
            (
                "a hiding display on another element",
                named_style("paragraph", r#"<style:paragraph-properties text:display="none"/>"#),
                formatting_styles(),
            ),
            (
                "a greater-than sign in a value of a display that shows",
                named_style("text", &text_props(r#"fo:font-family=">" text:display="true""#)),
                formatting_styles(),
            ),
            (
                "a hiding display in a comment",
                named_style("text", &format!("<!-- {hides_text} -->")),
                formatting_styles(),
            ),
            (
                "a hiding display in character data",
                named_style("text", &format!("<![CDATA[{hides_text}]]>")),
                formatting_styles(),
            ),
            (
                "a hiding display in a processing instruction",
                named_style("text", &format!("<?pi {hides_text} ?>")),
                formatting_styles(),
            ),
            (
                "a hiding display in an unclosed processing instruction",
                named_style("text", &format!("<?pi {hides_text}")),
                formatting_styles(),
            ),
        ];
        for (case, styles, expected) in cases {
            let bytes = sheet_with_styles(unclosed_styles(&styles).as_bytes());
            assert_eq!(outcome(&bytes, crate::Format::Ods), expected, "{case}");
        }
        for literal in ["<!--", "<?"] {
            let doctype = format!(r#"<!DOCTYPE office:document-styles SYSTEM "{literal}">"#);
            let bytes = sheet_with_styles((doctype + &hiding_text_style()).as_bytes());
            assert_eq!(outcome(&bytes, crate::Format::Ods), lost_styles(), "{literal}");
        }
    }

    #[test]
    fn a_text_or_presentation_styles_part_that_does_not_parse_is_a_loss_whatever_it_sets() {
        let styles = unclosed_styles("<office:styles/>");
        let parts = |odf: &Odf<'_>| {
            zip_package(&[("content.xml", odf.content().as_str()), ("styles.xml", &styles)])
        };
        assert_eq!(outcome(&parts(&text("<text:p>a</text:p>")), crate::Format::Odt), lost_styles());
        assert_eq!(outcome(&parts(&slides("<draw:page/>")), crate::Format::Odp), lost_styles());
        assert_eq!(outcome(&parts(&sheet("")), crate::Format::Ods), formatting_styles());
    }

    #[test]
    fn a_styles_part_that_does_not_parse_is_scanned_as_decoded_text() {
        let hiding = hiding_text_style();
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(hiding.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        let (head, tail) = hiding.split_once("none").unwrap();
        let undecodable = [head.as_bytes(), &[0xFF], tail.as_bytes()].concat();
        for (case, styles) in [("UTF-16", utf16), ("a value that does not decode", undecodable)] {
            assert_eq!(
                outcome(&sheet_with_styles(&styles), crate::Format::Ods),
                lost_styles(),
                "{case}"
            );
        }
    }

    #[test]
    fn a_styles_entry_that_cannot_be_inflated_is_a_loss_of_what_it_may_hide() {
        let content = sheet("").content();
        let mut bytes = zip_package_with(
            &[("content.xml", content.as_str()), ("styles.xml", "<office:styles/>")],
            stored(),
        );
        corrupt_entry(&mut bytes, "styles.xml");
        assert_eq!(outcome(&bytes, crate::Format::Ods), lost_styles());
    }

    #[test]
    fn a_styles_entry_past_a_limit_fails_the_conversion_instead_of_reading_as_absent() {
        let styles = unclosed_styles(&format!("<!--{}-->", "x".repeat(4000)));
        let content = sheet("").content();
        let bytes = zip_package_with(
            &[("content.xml", content.as_str()), ("styles.xml", styles.as_str())],
            stored(),
        );
        let limits = Limits { max_entry_bytes: 3000, ..Limits::default() };
        let err = parse(&ParseCtx::new(&limits), &bytes).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::EntryBytes, max: 3000 });
    }
}
