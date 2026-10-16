//! OpenDocument Text (.odt), Spreadsheet (.ods), and Presentation (.odp).

mod styles;
mod table;
mod text;

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Block, Document, Inline, inlines_are_empty};
use crate::package::archive::Role;
use crate::package::xml::{Element, ns};
use crate::package::{Package, PackageKind};
use crate::shared::assets::AssetSink;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use text::{Ctx, holds_content, parse_container};

const DR3D: &str = "urn:oasis:names:tc:opendocument:xmlns:dr3d:1.0";
const OFFICEOOO: &str = "http://openoffice.org/2009/office";

pub fn parse(parse_ctx: &ParseCtx, bytes: &[u8]) -> Result<Document, ConvertError> {
    let pkg = RefCell::new(Package::open(parse_ctx, bytes, PackageKind::Odf)?);

    if is_encrypted(&pkg)? {
        return Err(ConvertError::Encrypted);
    }

    let content_tree =
        pkg.borrow_mut().required_xml_part("content.xml", Role::Content(Region::Body))?;
    let body = content_tree
        .find(ns::OFFICE, "document-content")
        .and_then(|d| d.find(ns::OFFICE, "body"))
        .ok_or_else(|| ConvertError::malformed_part("content.xml", "no office:body"))?;

    let styles_role = if body.find(ns::OFFICE, "spreadsheet").is_some() {
        Role::Formatting
    } else {
        Role::Content(Region::Body)
    };
    let styles_tree = pkg.borrow_mut().optional_xml_part("styles.xml", styles_role)?;
    let mut styles = styles::OdfStyles::default();
    if let Some(tree) = &styles_tree {
        styles.collect(tree);
    }
    styles.collect(&content_tree);

    let assets = RefCell::new(AssetSink::new(parse_ctx));
    let ctx = Ctx::new(parse_ctx, &styles, &pkg, &assets);

    let blocks = if let Some(text) = body.find(ns::OFFICE, "text") {
        for master in text_master_pages(text, &styles) {
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

/// Encrypted ODF packages carry `manifest:encryption-data` elements on file
/// entries. The manifest is parsed properly - substring matching would
/// classify a document as encrypted over a mere comment. An absent or
/// unreadable manifest proves nothing (content parsing decides), but fatal
/// resource-limit errors propagate.
fn is_encrypted(pkg: &RefCell<Package>) -> Result<bool, ConvertError> {
    let Some(tree) = pkg.borrow_mut().optional_xml_part("META-INF/manifest.xml", Role::Probe)?
    else {
        return Ok(false);
    };
    Ok(tree.first_descendant(ns::MANIFEST, "encryption-data").is_some())
}

/// The master pages a text document's pages use: the first, which starts
/// a document naming none, those a paragraph or table style applied in the
/// text starts, and each one's next.
fn text_master_pages<'a>(text: &Element, styles: &styles::OdfStyles<'a>) -> Vec<&'a Element> {
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
    let mut pending: Vec<&str> = applied
        .into_iter()
        .filter_map(|(family, name)| styles.style_master_page(family, name))
        .collect();
    pending.extend(styles.first_master_page().and_then(|m| m.attr(ns::STYLE, "name")));
    let mut used = BTreeSet::new();
    while let Some(name) = pending.pop() {
        let Some(master) = styles.master_page(name) else { continue };
        if used.insert(name) {
            pending.extend(master.attr(ns::STYLE, "next-style-name"));
        }
    }
    used.into_iter().filter_map(|name| styles.master_page(name)).collect()
}

/// Report the header and footer a master page shows on every page it
/// starts.
fn report_running(master: &Element, parse_ctx: &ParseCtx) {
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

fn parse_presentation(pres: &Element, ctx: &Ctx) -> Result<Vec<Block>, ConvertError> {
    let styles = ctx.styles;
    let declared = declarations(pres);
    let mut blocks = Vec::new();
    let mut masters: BTreeMap<Option<&str>, (&Element, Shown)> = BTreeMap::new();
    for (position, page) in pres.find_all(ns::DRAW, "page").enumerate() {
        let _unit = ctx.parse_ctx.enter_unit(position);
        let props = page_properties(page, ctx);
        let master = match page.attr(ns::DRAW, "master-page-name").unwrap_or("") {
            "" => styles.first_master_page(),
            name => styles.master_page(name),
        };
        if let Some(master) = master {
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
        // Speaker notes are included (fixed policy), set off as a quote.
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

/// A slide's own drawing-page properties.
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
    ctx: &Ctx,
) {
    let kinds = [
        (
            "use-header-name",
            "header-decl",
            "display-header",
            DiagnosticCode::HeaderOmitted,
            Region::Header,
        ),
        (
            "use-footer-name",
            "footer-decl",
            "display-footer",
            DiagnosticCode::FooterOmitted,
            Region::Footer,
        ),
        (
            "use-date-time-name",
            "date-time-decl",
            "display-date-time",
            DiagnosticCode::FooterOmitted,
            Region::Footer,
        ),
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
fn decorates(master: &Element, shown: Shown, ctx: &Ctx) -> bool {
    let shape = shown.objects
        && master.child_elems().any(|shape| {
            shape.ns.as_deref() == Some(ns::DRAW) && shape.attr(ns::PRESENTATION, "class").is_none()
        });
    let background = shown.background
        && master
            .attr(ns::DRAW, "style-name")
            .and_then(|name| ctx.styles.style("drawing-page", name))
            .and_then(|style| style.find(ns::STYLE, "drawing-page-properties"))
            .is_some_and(|props| props.attr(ns::DRAW, "fill") == Some("bitmap"));
    shape || background
}

/// Walk a page's shapes in document order, recursing into `draw:g` groups.
fn walk_shapes(
    parent: &Element,
    ctx: &Ctx,
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
        if child.ns.as_deref().is_none_or(|n| n != ns::DRAW) {
            continue;
        }
        match child.local.as_str() {
            "frame" => {
                if let Some((code, region)) = running_frame(child) {
                    ctx.parse_ctx.emit(code, region);
                    continue;
                }
                let class = child.attr(ns::PRESENTATION, "class").unwrap_or("");
                let mut inner = Vec::new();
                for content in child.child_elems() {
                    if content.is(ns::DRAW, "text-box") {
                        inner.extend(parse_container(content, ctx)?);
                    } else if content.is(ns::TABLE, "table") {
                        inner.extend(table::parse_table(content, ctx)?);
                    } else if text::is_graphic(content) {
                        match text::frame_graphic(child, ctx)? {
                            Some(Inline::Math(tex)) => inner.push(Block::Math(tex)),
                            Some(graphic) => inner.push(Block::Paragraph(vec![graphic])),
                            None => {}
                        }
                        break;
                    }
                }
                if class == "title" {
                    push_title_heading(inner, title, ctx);
                } else {
                    body.append(&mut inner);
                }
            }
            "g" => walk_shapes(child, ctx, title, body, notes)?,
            "a" => text::report_unplaced(child, ctx)?,
            "custom-shape" | "rect" | "ellipse" | "circle" | "polygon" | "polyline"
            | "regular-polygon" | "path" | "line" | "connector" | "caption" | "measure" => {
                for content in child.child_elems() {
                    if content.is(ns::TEXT, "p") || content.is(ns::TEXT, "list") {
                        body.extend(parse_container(child, ctx)?);
                        break;
                    }
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
fn walk_notes(page: &Element, ctx: &Ctx, notes: &mut Vec<Block>) -> Result<(), ConvertError> {
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
fn push_title_heading(inner: Vec<Block>, blocks: &mut Vec<Block>, ctx: &Ctx) {
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
mod tests {
    use super::*;
    use crate::limits::{Limit, Limits};
    use std::io::{Cursor, Write};

    const CONTENT: &[u8] = br#"<office:document-content
        xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
        xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
        <office:body><office:text><text:p>hello</text:p></office:text></office:body>
        </office:document-content>"#;

    fn odt(manifest: &[u8]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("META-INF/manifest.xml", opts).unwrap();
        w.write_all(manifest).unwrap();
        w.start_file("content.xml", opts).unwrap();
        w.write_all(CONTENT).unwrap();
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn corrupt_manifest_does_not_classify_as_encrypted() {
        let doc = parse(&ParseCtx::default(), &odt(b"<manifest:manifest <<< not xml")).unwrap();
        assert!(!doc.blocks.is_empty());
    }

    fn odt_with_content(content: &str) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file("content.xml", zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(content.as_bytes()).unwrap();
        w.finish().unwrap().into_inner()
    }

    fn table_doc(rows: &str) -> String {
        format!(
            r#"<office:document-content
            xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
            xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
            xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0">
            <office:body><office:text><table:table>{rows}</table:table></office:text></office:body>
            </office:document-content>"#
        )
    }

    #[test]
    fn repeated_rows_cannot_amplify_text_beyond_the_byte_budget() {
        // H3: the slot budget alone would admit 1000 copies of a 100 KB
        // cell (~100 MB); the duplicated-bytes budget rejects it up front.
        let fat = "x".repeat(100_000);
        let rows = format!(
            r#"<table:table-row table:number-rows-repeated="1000">
            <table:table-cell><text:p>{fat}</text:p></table:table-cell>
            </table:table-row>"#
        );
        let err = parse(&ParseCtx::default(), &odt_with_content(&table_doc(&rows))).unwrap_err();
        assert!(
            matches!(err, ConvertError::ResourceLimit { limit: Limit::ExpansionTextBytes, .. }),
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
        let doc = parse(&ParseCtx::default(), &odt_with_content(&table_doc(rows))).unwrap();
        assert_eq!(doc.notes.len(), 1, "repeated rows must not duplicate notes");
    }

    #[test]
    fn styled_runs_stop_at_tables_and_work_inside_cells() {
        let content = r#"<office:document-content
            xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
            xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
            xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
            xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0">
            <office:automatic-styles>
              <style:style style:name="Source_20_Code" style:family="paragraph"/>
            </office:automatic-styles>
            <office:body><office:text>
              <text:p text:style-name="Source_20_Code">before table</text:p>
              <table:table><table:table-row><table:table-cell>
                <text:p text:style-name="Source_20_Code">inside cell</text:p>
              </table:table-cell></table:table-row></table:table>
              <text:p>after table</text:p>
            </office:text></office:body>
            </office:document-content>"#;
        let doc = parse(&ParseCtx::default(), &odt_with_content(content)).unwrap();
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
    fn resource_limit_in_manifest_is_fatal() {
        let mut manifest = Vec::new();
        for _ in 0..(crate::limits::Limits::default().max_nesting_depth + 2) {
            manifest.extend_from_slice(b"<d>");
        }
        let err = parse(&ParseCtx::default(), &odt(&manifest)).unwrap_err();
        assert!(
            matches!(err, ConvertError::ResourceLimit { limit: Limit::NestingDepth, .. }),
            "encryption probing must not swallow fatal errors, got: {err}"
        );
    }

    #[test]
    fn form_checkboxes_anchored_in_a_cell_follow_its_content() {
        let content = r#"<office:document-content
            xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
            xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
            xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
            xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
            xmlns:form="urn:oasis:names:tc:opendocument:xmlns:form:1.0"
            xmlns:xml="http://www.w3.org/XML/1998/namespace">
            <office:body><office:spreadsheet><table:table table:name="S">
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
            </table:table></office:spreadsheet></office:body>
            </office:document-content>"#;
        let doc = parse(&ParseCtx::default(), &odt_with_content(content)).unwrap();
        let md = crate::render::markdown::document_to_markdown(&doc);
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
        fn build(&self) -> Vec<u8> {
            let content = format!(
                "<office:document-content {NS}>{head}<office:body><office:{kind}>{body}</office:{kind}></office:body></office:document-content>",
                kind = self.kind,
                head = self.head,
                body = self.body
            );
            let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("content.xml", opts).unwrap();
            w.write_all(content.as_bytes()).unwrap();
            if !self.styles.is_empty() {
                let styles = format!(
                    "<office:document-styles {NS}>{}</office:document-styles>",
                    self.styles
                );
                w.start_file("styles.xml", opts).unwrap();
                w.write_all(styles.as_bytes()).unwrap();
            }
            for (name, bytes) in self.parts {
                w.start_file(*name, opts).unwrap();
                w.write_all(bytes.as_bytes()).unwrap();
            }
            w.finish().unwrap().into_inner()
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

    fn convert(odf: &Odf) -> (String, Vec<Reported>) {
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, &odf.build()).unwrap();
        let md = crate::render::markdown::document_to_markdown(&doc);
        let unit = |d: &crate::diagnostics::Diagnostic| d.unit.map(std::num::NonZeroU32::get);
        let reported =
            parse_ctx.diagnostics().iter().map(|d| (d.code, d.region, unit(&d), d.count)).collect();
        (md, reported)
    }

    fn diagnose(odf: &Odf) -> Vec<Reported> {
        convert(odf).1
    }

    const BODY: Region = Region::Body;
    use DiagnosticCode::{
        BlockUnread, CommentOmitted, EmbeddedObject, FooterOmitted, FormControlUnread,
        GraphicUnread, HeaderOmitted, ImageUnavailable, ImageUnplaced, RecoveredXml,
        RevisionOmitted, TableSpanRepaired, TextBoxUnread,
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
        let cases: Vec<(&str, Odf, Vec<Reported>)> = vec![
            (
                "object href unresolvable",
                text(&inline(r#"<draw:object xlink:href="a%2Fb"/>"#)),
                vec![(EmbeddedObject, BODY, None, 1)],
            ),
            (
                "object part absent",
                text(&inline(&replaced(r#"<draw:object xlink:href="./Object 9"/>"#))),
                vec![(EmbeddedObject, BODY, None, 1)],
            ),
            (
                "object outside the package",
                text(&inline(r#"<draw:object xlink:href="https://example.org/o"/>"#)),
                vec![(EmbeddedObject, BODY, None, 1)],
            ),
            ("chart object", text(&inline(&replaced(chart))), vec![(GraphicUnread, BODY, None, 1)]),
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
                vec![(GraphicUnread, BODY, Some(2), 1)],
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
                assert_eq!(diagnose(&odf), [(code, BODY, unit, 1)], "{anchor} {media}");
            }
        }
    }

    #[test]
    fn an_object_part_reports_in_the_region_that_names_it() {
        let recovering = format!("{FORMULA}<unclosed>");
        let parts = [("Object 1/content.xml", recovering.as_str())];
        let body = format!(
            r#"<text:p>x<text:note text:id="n"><text:note-body><text:p>{}</text:p></text:note-body></text:note></text:p>"#,
            frame(r#"<draw:object xlink:href="./Object 1"/>"#)
        );
        let doc =
            parse(&ParseCtx::default(), &Odf { parts: &parts, ..text(&body) }.build()).unwrap();
        let [Block::Paragraph(note)] = &doc.notes[0].blocks[..] else {
            panic!("unexpected note: {:?}", doc.notes)
        };
        assert!(matches!(&note[..], [Inline::Math(tex)] if tex == "x"), "{note:?}");
        let (_, reported) = convert(&Odf { parts: &parts, ..text(&body) });
        assert_eq!(reported, [(RecoveredXml, Region::Note, None, 1)]);
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
            assert!(
                matches!(err, ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 10 }),
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
        assert_eq!(md, "xbars\n");
        assert_eq!(reported, [(GraphicUnread, BODY, None, 1)]);
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
        assert!(matches!(content[..], [Inline::Image { .. }]), "{content:?}");
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

        let shapes: String = ["circle", "polyline", "regular-polygon", "measure"]
            .iter()
            .map(|shape| format!("<draw:{shape}><text:p>{shape}</text:p></draw:{shape}>"))
            .collect();
        let (md, reported) = convert(&slides(&format!("<draw:page>{shapes}</draw:page>")));
        assert_eq!(md, "circle\n\npolyline\n\nregular-polygon\n\nmeasure\n");
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
}
