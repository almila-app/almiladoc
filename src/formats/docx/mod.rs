//! OOXML WordprocessingML (.docx / .docm).

mod content;
mod numbering;
mod styles;

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Document, Note, NoteKind};
use crate::package::archive::{Role, open_ooxml, report};
use crate::package::relationships::{
    Relationships, read_main_part, read_rels, rel_target_path, rel_type, rels_part_for,
};
use crate::package::xml::{Element, ns};
use crate::package::{Package, path};
use crate::shared::assets::AssetSink;
use crate::shared::tag_scan::{self, Attributes};
use content::{Ctx, References};
use numbering::Counters;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use std::rc::Rc;

const MAIN_PART: &str = "word/document.xml";

pub(super) fn parse(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Document, ConvertError> {
    let pkg = open_ooxml(parse_ctx, bytes)?;

    let main_part = read_main_part(parse_ctx, &pkg, &[MAIN_PART])?
        .ok_or_else(|| ConvertError::missing_part(MAIN_PART))?;
    let doc_rels = read_rels(&pkg, &rels_part_for(&main_part), Role::Content(Region::Body))?;

    let typed =
        |rel_type, fallback| typed_part_path(parse_ctx, &doc_rels, &main_part, rel_type, fallback);
    let numbering_part = typed(rel_type::NUMBERING, "numbering.xml");
    let styles_part = typed(rel_type::STYLES, "styles.xml");
    let styles_tree =
        styles_part.read_styles(parse_ctx, &pkg, pkg.has_part(&numbering_part.path))?;
    let styles = styles::Styles::parse(styles_tree.as_ref().and_then(|t| t.find(ns::W, "styles")));

    let (numbering, numbering_reported) =
        read_numbering(parse_ctx, &pkg, &numbering_part, &styles)?;

    let doc_tree = pkg.required_xml_part(&main_part, Role::Content(Region::Body))?;
    let body = doc_tree
        .find(ns::W, "document")
        .and_then(|d| d.find(ns::W, "body"))
        .ok_or_else(|| ConvertError::malformed_part(&main_part, "no document body"))?;
    report_side_parts(parse_ctx, body, &doc_rels, &main_part);

    let counters = RefCell::new(Counters::default());
    let assets = RefCell::new(AssetSink::new(parse_ctx));
    let references = References::default();

    let footnotes_part = typed(rel_type::FOOTNOTES, "footnotes.xml");
    let endnotes_part = typed(rel_type::ENDNOTES, "endnotes.xml");

    let ctx = Ctx {
        parse_ctx,
        pkg: &pkg,
        rels: doc_rels,
        base_part: main_part,
        region: Region::Body,
        styles: &styles,
        numbering: &numbering,
        numbering_reported,
        counters: &counters,
        assets: &assets,
        references: &references,
        moves: RefCell::default(),
    };
    let blocks = content::parse_blocks(body, &ctx)?;
    ctx.settle_moves()?;

    let notes = parse_notes(&ctx, footnotes_part, endnotes_part)?;
    parse_ctx.emit_count(DiagnosticCode::CommentOmitted, Region::Comment, references.comments());

    let assets = assets.into_inner().assets;
    Ok(Document { blocks, notes, assets, ..Document::default() })
}

fn read_numbering(
    parse_ctx: &ParseCtx<'_>,
    pkg: &Package<'_>,
    part: &TypedPart,
    styles: &styles::Styles<'_>,
) -> Result<(numbering::Numbering, bool), ConvertError> {
    let (tree, reported) = part.read(parse_ctx, pkg, Role::Content(Region::Body), "numbering")?;
    let numbering = match tree.as_ref().and_then(|t| t.find(ns::W, "numbering")) {
        Some(root) => numbering::parse(parse_ctx, root, &|style_id| {
            styles.direct_num_id(parse_ctx, style_id)
        })?,
        None => numbering::Numbering::default(),
    };
    Ok((numbering, reported))
}

fn parse_notes(
    ctx: &Ctx<'_, '_>,
    footnotes: TypedPart,
    endnotes: TypedPart,
) -> Result<Vec<Note>, ConvertError> {
    let (parse_ctx, pkg, references) = (ctx.parse_ctx, ctx.pkg, ctx.references);
    let role = Role::Content(Region::Note);
    let mut notes = Vec::new();
    for (part, root_name, elem_name, prefix, kind) in [
        (footnotes, "footnotes", "footnote", "fn", NoteKind::Footnote),
        (endnotes, "endnotes", "endnote", "en", NoteKind::Endnote),
    ] {
        let (dropped, rendered) = references.note_references(parse_ctx, prefix)?;
        // The part may hold no note that is read, so it is read silently first: its loss
        // reports where a reference to its notes rendered or a note in it is read, or, when it
        // cannot be read, unless every reference to its notes dropped.
        let probed = part.shared(parse_ctx, pkg, Role::Probe, root_name)?;
        let read = rendered
            || match probed.as_deref().and_then(|tree| tree.find(ns::W, root_name)) {
                Some(root) => {
                    !read_notes(parse_ctx, references, root, elem_name, prefix)?.0.is_empty()
                }
                None => !dropped,
            };
        if !read {
            continue;
        }
        let tree = part.shared(parse_ctx, pkg, role, root_name)?;
        let Some(root) = tree.as_deref().and_then(|tree| tree.find(ns::W, root_name)) else {
            continue;
        };
        let (found, duplicates) = read_notes(parse_ctx, references, root, elem_name, prefix)?;
        parse_ctx.emit_count(DiagnosticCode::DuplicateNote, Region::Note, duplicates);
        if found.is_empty() {
            continue;
        }
        let rels = read_rels(pkg, &rels_part_for(&part.path), role)?;
        let note_ctx = ctx.for_part(rels, part.path, Region::Note);
        for (id, note) in found {
            notes.push(Note { id, kind, blocks: content::parse_blocks(note, &note_ctx)? });
        }
        note_ctx.settle_moves()?;
    }
    Ok(notes)
}

/// Headers and footers the sections name, each part once (by its id where
/// its relationship is unknown): none is read.
fn report_side_parts(parse_ctx: &ParseCtx<'_>, body: &Element, rels: &Relationships, base: &str) {
    for (reference, code, region) in [
        ("headerReference", DiagnosticCode::HeaderOmitted, Region::Header),
        ("footerReference", DiagnosticCode::FooterOmitted, Region::Footer),
    ] {
        let parts: BTreeSet<String> = body
            .descendants(ns::W, "sectPr")
            .flat_map(|section| section.find_all(ns::W, reference))
            .filter_map(|r| r.attr_qualified(ns::R, "id"))
            .map(|id| rel_target_path(rels, base, id).unwrap_or_else(|| id.to_string()))
            .collect();
        parse_ctx.emit_count(code, region, parts.len() as u64);
    }
}

struct TypedPart {
    path: String,
    related: bool,
}

impl TypedPart {
    /// The part's tree when its root element is `w:{root}`, and whether its
    /// loss was reported: unreadable, absent though related, or rooted
    /// elsewhere.
    fn read(
        &self,
        parse_ctx: &ParseCtx<'_>,
        pkg: &Package<'_>,
        role: Role,
        root: &str,
    ) -> Result<(Option<Element>, bool), ConvertError> {
        let tree = if self.related {
            pkg.related_xml_part(&self.path, role)?
        } else {
            pkg.optional_xml_part(&self.path, role)?
        };
        match tree {
            Some(tree) if tree.find(ns::W, root).is_some() => Ok((Some(tree), false)),
            Some(_) => {
                report(parse_ctx, role.unreadable());
                Ok((None, true))
            }
            None => Ok((None, self.related || pkg.has_part(&self.path))),
        }
    }

    /// A styles part that does not read costs formatting alone, unless numbering reads styles, its
    /// entry cannot be inflated, or it does not parse and sets `vanish`: then it costs text.
    fn read_styles(
        &self,
        parse_ctx: &ParseCtx<'_>,
        pkg: &Package<'_>,
        numbered: bool,
    ) -> Result<Option<Element>, ConvertError> {
        if numbered {
            return Ok(self.read(parse_ctx, pkg, Role::Content(Region::Body), "styles")?.0);
        }
        let lost = match pkg.part(&self.path) {
            Ok(Some(bytes)) => match pkg.optional_xml_part(&self.path, Role::Probe)? {
                Some(tree) if tree.find(ns::W, "styles").is_some() => return Ok(Some(tree)),
                Some(_) => Role::Formatting,
                None => unparsed_styles_role(parse_ctx, &bytes)?,
            },
            Ok(None) if self.related => Role::Formatting,
            Ok(None) => return Ok(None),
            Err(e) if e.is_fatal() => return Err(e),
            Err(_) => Role::Content(Region::Body),
        };
        report(parse_ctx, lost.unreadable());
        Ok(None)
    }

    /// [`TypedPart::read`] through the package's shared trees, so a second read with another
    /// role reports what the first did not without parsing the part again.
    fn shared(
        &self,
        parse_ctx: &ParseCtx<'_>,
        pkg: &Package<'_>,
        role: Role,
        root: &str,
    ) -> Result<Option<Rc<Element>>, ConvertError> {
        if !self.related && !pkg.has_part(&self.path) {
            return Ok(None);
        }
        match pkg.related_xml_tree(&self.path, role)? {
            Some(tree) if tree.find(ns::W, root).is_some() => Ok(Some(tree)),
            Some(_) => {
                report(parse_ctx, role.unreadable());
                Ok(None)
            }
            None => Ok(None),
        }
    }
}

const HIDING_PROPERTY: &[u8] = b"vanish";

fn unparsed_styles_role(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Role, ConvertError> {
    let hides = tag_scan::any_tag(parse_ctx, bytes, enables_hiding)?;
    Ok(if hides { Role::Content(Region::Body) } else { Role::Formatting })
}

fn enables_hiding(name: &[u8], attributes: Attributes<'_>) -> bool {
    tag_scan::local_name(name) == HIDING_PROPERTY && !val_turns_off(attributes)
}

fn val_turns_off(attributes: Attributes<'_>) -> bool {
    let (mut qualified, mut unqualified) = (None, None);
    for (name, value) in attributes {
        match name {
            b"w:val" => qualified = qualified.or(Some(value)),
            b"val" => unqualified = unqualified.or(Some(value)),
            _ => {}
        }
    }
    qualified.or(unqualified).is_some_and(styles::turns_off)
}

type NoteElements<'e> = Vec<(String, &'e Element)>;

/// The notes of a notes part that are read, each with its id (no separator, and none every
/// reference to which dropped), and how many repeated ids it skipped.
fn read_notes<'e>(
    parse_ctx: &ParseCtx<'_>,
    references: &References,
    root: &'e Element,
    elem_name: &'e str,
    prefix: &str,
) -> Result<(NoteElements<'e>, u64), ConvertError> {
    let mut notes = Vec::new();
    let mut seen = HashSet::new();
    let mut duplicates = 0;
    for note in root.find_all(ns::W, elem_name) {
        if matches!(
            note.attr(ns::W, "type"),
            Some("separator" | "continuationSeparator" | "continuationNotice")
        ) {
            continue;
        }
        let Some(raw_id) = note.attr(ns::W, "id") else { continue };
        let id = format!("{prefix}{raw_id}");
        if references.reads_note(parse_ctx, &id)? {
            if seen.insert(raw_id) {
                notes.push((id, note));
            } else {
                duplicates += 1;
            }
        }
    }
    Ok((notes, duplicates))
}

/// Path of a typed related part, resolved against the main part; falls back
/// to the conventional sibling name when the relationship is absent or its
/// target does not resolve, the latter reported.
fn typed_part_path(
    parse_ctx: &ParseCtx<'_>,
    rels: &Relationships,
    base: &str,
    rel_type: &str,
    fallback: &str,
) -> TypedPart {
    let related = rels.first_of_type(rel_type).map(|rel| rel.target.as_str());
    let reference = related.unwrap_or(fallback);
    let path = if let Ok(target) = path::resolve(base, reference) {
        target.path
    } else {
        parse_ctx.emit(DiagnosticCode::ConventionalPartUsed, Region::Container);
        path::resolve(base, fallback).map_or_else(|_| fallback.to_string(), |t| t.path)
    };
    TypedPart { path, related: related.is_some() }
}

#[cfg(test)]
#[expect(clippy::format_collect, reason = "fixture XML is built with format!")]
mod tests {
    use super::*;
    use crate::ImagePolicy;
    use crate::converter::diagnostics_of;
    use crate::model::{Block, CellSlot, ImageSource, Inline, MarkerKind, inlines_to_plain_text};
    use crate::render::markdown::document_to_markdown;
    use crate::shared::test_support::{
        corrupt_entry, diagnostic_rows, stored, zip_package, zip_package_with,
    };
    use std::assert_matches;
    use std::fmt::Debug;

    #[test]
    fn linked_image_relationship_becomes_an_external_source() {
        let document = r#"<w:document
            xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"
            xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
            <w:body><w:p><w:r><w:drawing>
                <a:blip r:link="rId9"/>
            </w:drawing></w:r></w:p></w:body></w:document>"#;
        let rels = r#"<Relationships
            xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId9"
                Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image"
                Target="https://e.com/pic.png" TargetMode="External"/>
            </Relationships>"#;
        let bytes =
            zip_package(&[("word/document.xml", document), ("word/_rels/document.xml.rels", rels)]);
        let doc = parse(&ParseCtx::default(), &bytes).unwrap();
        let [Block::Paragraph(inlines)] = &doc.blocks[..] else { panic!("{:?}", doc.blocks) };
        let [Inline::Image { source, .. }] = &inlines[..] else { panic!("{inlines:?}") };
        assert_eq!(*source, ImageSource::External("https://e.com/pic.png".into()));
        assert!(doc.assets.is_empty(), "external images are not retained as assets");
    }

    #[test]
    fn huge_numbering_start_values_cannot_overflow() {
        // w:start is ST_DecimalNumber (xsd:int); out-of-range values are
        // clamped so document-order increments can never overflow.
        let para = r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr>
            <w:r><w:t>item</w:t></w:r></w:p>"#;
        let document = document(&format!("{para}{para}"));
        for start in ["18446744073709551615", "-5", "2147483647"] {
            let numbering = format!(
                r#"<w:numbering {NS}>
            <w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0">
                <w:numFmt w:val="decimal"/><w:start w:val="{start}"/>
            </w:lvl></w:abstractNum>
            <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
            </w:numbering>"#
            );
            let bytes = zip_package(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", &numbering),
            ]);
            let doc = parse(&ParseCtx::default(), &bytes).expect(start);
            assert_ne!(doc.blocks, []);
        }
    }

    #[test]
    fn partial_num_pr_inherits_property_by_property() {
        let document = format!(
            r#"<w:document {NS}><w:body>
            <w:p><w:pPr><w:pStyle w:val="Listy"/>
                <w:numPr><w:ilvl w:val="1"/></w:numPr></w:pPr>
                <w:r><w:t>second level</w:t></w:r></w:p>
            </w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {NS}>
            <w:style w:type="paragraph" w:styleId="Listy">
                <w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr>
            </w:style></w:styles>"#
        );
        let numbering = format!(
            r#"<w:numbering {NS}>
            <w:abstractNum w:abstractNumId="0">
                <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:start w:val="1"/></w:lvl>
                <w:lvl w:ilvl="1"><w:numFmt w:val="lowerLetter"/><w:start w:val="1"/></w:lvl>
            </w:abstractNum>
            <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
            </w:numbering>"#
        );
        let bytes = zip_package(&[
            ("word/document.xml", &document),
            ("word/styles.xml", &styles),
            ("word/numbering.xml", &numbering),
        ]);
        let doc = parse(&ParseCtx::default(), &bytes).unwrap();
        let Some(Block::List(list)) = doc.blocks.first() else {
            panic!("expected a list, got {:?}", doc.blocks.first());
        };
        assert_eq!(list.marker, MarkerKind::LowerAlpha, "level 1 of the style's numbering");
    }

    #[test]
    fn unmarked_run_edge_whitespace_is_kept() {
        // Converters that never write xml:space carry inter-word spacing on
        // run edges; dropping it glues the words together.
        let document = document(
            "<w:p>
            <w:r><w:t>This</w:t></w:r>
            <w:r><w:t> by-law</w:t></w:r>
            <w:r><w:t> grants</w:t></w:r>
            </w:p>",
        );
        let doc =
            parse(&ParseCtx::default(), &zip_package(&[("word/document.xml", &document)])).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else { panic!() };
        assert_eq!(inlines_to_plain_text(inlines), "This by-law grants");
    }

    #[test]
    fn page_break_between_runs_keeps_the_word_boundary() {
        let cases = [
            (
                r#"<w:p><w:r><w:t>Alfa</w:t><w:br w:type="page"/><w:t>Beta</w:t></w:r></w:p>"#,
                "Alfa\\\nBeta\n",
            ),
            (
                r#"<w:p><w:r><w:t>Alfa</w:t><w:br w:type="page"/></w:r></w:p>
                <w:p><w:r><w:t>Beta</w:t></w:r></w:p>"#,
                "Alfa\n\nBeta\n",
            ),
        ];
        for (body, expected) in cases {
            let bytes = zip_package(&[("word/document.xml", &document(body))]);
            let markdown = document_to_markdown(&parse(&ParseCtx::default(), &bytes).unwrap());
            assert_eq!(markdown, expected, "body: {body}");
        }
    }

    #[test]
    fn a_package_without_its_main_part_is_missing_it() {
        let styles = relationships(&[("rId1", "styles", "styles.xml")]);
        let absent = relationships(&[("rId1", "officeDocument", "missing.xml")]);
        let cases = [
            (vec![("docProps/core.xml", "<x/>".to_string())], "word/document.xml"),
            (
                vec![("_rels/.rels", styles), ("ppt/presentation.xml", "<x/>".into())],
                "word/document.xml",
            ),
            (vec![("_rels/.rels", absent), ("word/document.xml", "<x/>".into())], "missing.xml"),
        ];
        for (parts, missing) in cases {
            let error = parse(&ParseCtx::default(), &zip_package(&parts)).unwrap_err();
            assert_matches!(error, ConvertError::MissingPart { part } if part.to_string() == missing);
        }
    }

    const NS: &str = concat!(
        r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" "#,
        r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" "#,
        r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" "#,
        r#"xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" "#,
        r#"xmlns:cx="http://schemas.microsoft.com/office/drawing/2014/chartex" "#,
        r#"xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" "#,
        r#"xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" "#,
        r#"xmlns:v="urn:schemas-microsoft-com:vml" "#,
        r#"xmlns:o="urn:schemas-microsoft-com:office:office" "#,
        r#"xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math""#,
    );

    fn document(body: &str) -> String {
        format!("<w:document {NS}><w:body>{body}</w:body></w:document>")
    }

    fn relationships(targets: &[(&str, &str, &str)]) -> String {
        let rels: String = targets
            .iter()
            .map(|(id, kind, target)| {
                format!(
                    r#"<Relationship Id="{id}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}" Target="{target}"/>"#
                )
            })
            .collect();
        format!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
        )
    }

    fn run(content: &str) -> String {
        format!("<w:p><w:r>{content}</w:r></w:p>")
    }

    fn drawing(content: &str) -> String {
        run(&format!("<w:drawing>{content}</w:drawing>"))
    }

    fn text_box(text: &str) -> String {
        format!("<w:txbxContent><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:txbxContent>")
    }

    fn alternate(choice: &str, fallback: &str) -> String {
        format!(
            r#"<mc:AlternateContent><mc:Choice Requires="cx">{choice}</mc:Choice>{fallback}</mc:AlternateContent>"#
        )
    }

    fn declined(choice: &str) -> String {
        alternate(choice, "<mc:Fallback/>")
    }

    fn graphics(body: &str) -> Parts {
        let rels = relationships(&[
            ("rId2", "image", "media/one.png"),
            ("rId3", "chart", "charts/chart1.xml"),
            ("rId4", "oleObject", "embeddings/one.bin"),
            ("rId5", "diagramData", "diagrams/data1.xml"),
        ]);
        vec![
            ("word/document.xml", document(body)),
            ("word/_rels/document.xml.rels", rels),
            ("word/media/one.png", "png".into()),
            ("word/embeddings/one.bin", "ole".into()),
        ]
    }

    const PICTURE: &str = r#"<a:blip r:embed="rId2"/>"#;
    const CHART: &str = r#"<c:chart r:id="rId3"/>"#;
    const OLE: &str = r#"<o:OLEObject ProgID="Equation.3" ShapeID="s1" r:id="rId4"/>"#;
    const VML_PICTURE: &str = r#"<v:shape id="s1"><v:imagedata r:id="rId2"/></v:shape>"#;
    const VML_FILL: &str = r#"<v:rect id="s1"><v:fill r:id="rId2" type="frame"/></v:rect>"#;
    const CELL: &str = "<w:tc><w:p/></w:tc>";
    const TEXT_PARAGRAPH: &str = "<w:p><w:r><w:t>x</w:t></w:r></w:p>";
    const TEXT_CELL: &str = "<w:tc><w:p><w:r><w:t>x</w:t></w:r></w:p></w:tc>";
    const ROW_DEL: &str = "<w:trPr><w:del/></w:trPr>";
    const DELETED_TEXT_ROW: &str = concat!(
        "<w:tr><w:trPr><w:del/></w:trPr>",
        "<w:tc><w:p><w:r><w:t>x</w:t></w:r></w:p></w:tc></w:tr>",
    );
    const DELETED_TEXT_CELL: &str = concat!(
        "<w:tc><w:tcPr><w:cellDel/></w:tcPr>",
        "<w:p><w:r><w:t>x</w:t></w:r></w:p></w:tc>",
    );
    const EMPTY_DELETED_ROW: &str = concat!(
        "<w:tr><w:trPr><w:del/></w:trPr><w:tc><w:p/></w:tc>",
        r#"<w:tc><w:p><w:r><w:t xml:space="preserve"> </w:t></w:r></w:p></w:tc></w:tr>"#,
    );
    const EMPTY_DELETED_CELL: &str = concat!(
        "<w:tc><w:tcPr><w:cellDel/></w:tcPr>",
        r#"<w:p><w:r><w:t xml:space="preserve"> </w:t></w:r></w:p></w:tc>"#,
    );
    const SHAPE_TEXT: &str = "<a:p><a:r><a:t>label</a:t></a:r></a:p>";
    const NUMBERED: &str = r#"<w:p><w:pPr><w:numPr><w:numId w:val="7"/></w:numPr></w:pPr></w:p>"#;
    const DELETED_MATH: &str =
        r#"<w:del w:id="1"><m:oMath><m:r><m:t>y</m:t></m:r></m:oMath></w:del>"#;
    const MATH: &str = "<m:oMath><m:r><m:t>y</m:t></m:r></m:oMath>";
    const HIDDEN_MATH: &str =
        "<m:oMath><m:r><w:rPr><w:vanish/></w:rPr><m:t>y</m:t></m:r></m:oMath>";
    const SHOWN_MATH: &str =
        r#"<m:oMath><m:r><w:rPr><w:vanish w:val="0"/></w:rPr><m:t>y</m:t></m:r></m:oMath>"#;
    const PARTLY_HIDDEN_MATH: &str = concat!(
        "<m:oMath><m:r><w:rPr><w:vanish/></w:rPr><m:t>x</m:t></m:r>",
        "<m:r><m:t>y</m:t></m:r></m:oMath>",
    );
    const RUNLESS_MATH: &str = "<m:oMath><m:d><m:e/></m:d></m:oMath>";
    const SYMBOL: &str = r#"<w:sym w:font="Symbol" w:char="F061"/>"#;
    const BOOKMARK: &str = r#"<w:bookmarkStart w:id="0" w:name="here"/>"#;
    const CYCLIC_MATH: &str =
        r#"<m:oMath><m:r><w:rPr><w:rStyle w:val="A"/></w:rPr><m:t>y</m:t></m:r></m:oMath>"#;
    const CYCLIC_DELETED_RUN: &str = concat!(
        r#"<w:del w:id="1"><w:r><w:rPr><w:rStyle w:val="A"/>"#,
        r#"<w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr>"#,
        "<w:delText>\u{a0}</w:delText></w:r></w:del>",
    );
    const PARTLY_HIDDEN_FRACTION: &str = concat!(
        r#"<m:oMath><m:f><m:fPr><m:type m:val="lin"/></m:fPr>"#,
        "<m:num><m:r><m:t>a</m:t></m:r></m:num><m:den><m:r><m:t>b</m:t></m:r></m:den></m:f>",
        "<m:r><w:rPr><w:vanish/></w:rPr><m:t>h</m:t></m:r></m:oMath>",
    );
    const HIDDEN_FRACTION: &str = concat!(
        "<m:oMath><m:f><m:num><m:r><w:rPr><w:vanish/></w:rPr><m:t>a</m:t></m:r></m:num>",
        "<m:den><m:r><w:rPr><w:vanish/></w:rPr><m:t>b</m:t></m:r></m:den></m:f></m:oMath>",
    );
    const EMPTY_SHOWN_MATH: &str = concat!(
        "<m:oMath><m:r><w:rPr><w:vanish/></w:rPr><m:t>x</m:t></m:r>",
        "<m:r><m:t/></m:r></m:oMath>",
    );
    const CYCLIC_PARTLY_HIDDEN_MATH: &str = concat!(
        "<m:oMath><m:r><w:rPr><w:vanish/></w:rPr><m:t>x</m:t></m:r>",
        r#"<m:r><w:rPr><w:rStyle w:val="A"/></w:rPr><m:t>y</m:t></m:r></m:oMath>"#,
    );
    const CYCLIC_STYLES: &str = concat!(
        r#"<w:style w:type="character" w:styleId="A"><w:basedOn w:val="B"/></w:style>"#,
        r#"<w:style w:type="character" w:styleId="B"><w:basedOn w:val="A"/></w:style>"#,
    );
    const HIDDEN_CHECKBOX: &str = concat!(
        r#"<w:r><w:rPr><w:rFonts w:ascii="Wingdings" w:hAnsi="Wingdings"/><w:vanish/></w:rPr>"#,
        "<w:t>\u{f0fe}</w:t></w:r>",
    );
    const DESTINATION_START: &str = r#"<w:moveToRangeStart w:id="3" w:name="m"/>"#;
    const SYMBOL_FONT: &str = r#"<w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/>"#;
    const HIDE_RPR: &str = "<w:vanish/>";

    fn symbol_run(rpr: &str, tag: &str, text: &str) -> String {
        let rpr = format!("<w:rPr>{SYMBOL_FONT}{rpr}</w:rPr>");
        format!(r#"<w:r>{rpr}<{tag} xml:space="preserve">{text}</{tag}></w:r>"#)
    }

    const HIDING_DEFAULT_STYLE: &str = concat!(
        r#"<w:style w:type="paragraph" w:default="1" w:styleId="Normal">"#,
        "<w:rPr><w:vanish/></w:rPr></w:style>",
        r#"<w:style w:type="paragraph" w:styleId="Plain"/>"#,
    );
    const CORRUPT_RELS: &str = "</Relationships>";
    const SECOND_REFERENCE: &str = r#"<w:r><w:footnoteReference w:id="2"/></w:r>"#;
    const TWO_NOTES: &str = concat!(
        r#"<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
        r#"<w:footnote w:id="1"><w:p><w:r><w:t>one</w:t></w:r></w:p></w:footnote>"#,
        r#"<w:footnote w:id="2"><w:p><w:r><w:t>two</w:t></w:r></w:p></w:footnote>"#,
        "</w:footnotes>",
    );
    const NO_NOTE_UNCLOSED: &str = concat!(
        r#"<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
        r#"<w:footnote w:type="separator" w:id="0"><w:p/></w:footnote>"#,
    );

    /// An inline equation of math runs, each `(hidden, text)`.
    fn math(runs: &[(bool, &str)]) -> String {
        let runs: String = runs
            .iter()
            .map(|(hidden, text)| {
                let rpr = if *hidden { "<w:rPr><w:vanish/></w:rPr>" } else { "" };
                format!(r#"<m:r>{rpr}<m:t xml:space="preserve">{text}</m:t></m:r>"#)
            })
            .collect();
        format!("<m:oMath>{runs}</m:oMath>")
    }

    const BLOCK_MATH: &str = concat!(
        "<m:oMathPara><m:oMath><m:r><w:rPr><w:vanish/></w:rPr><m:t>x</m:t></m:r>",
        "<m:r><m:t>y</m:t></m:r></m:oMath></m:oMathPara>",
    );
    const EMPTY_ALTERNATE: &str = "<mc:AlternateContent><mc:Fallback/></mc:AlternateContent>";
    const DECLINED_TEXT: &str = concat!(
        r#"<mc:AlternateContent><mc:Choice Requires="cx"><w:t>c</w:t></mc:Choice>"#,
        "<mc:Fallback/></mc:AlternateContent>",
    );
    const DECLINED_REFERENCE: &str = concat!(
        r#"<mc:AlternateContent><mc:Choice Requires="cx"><w:footnoteReference w:id="1"/>"#,
        "</mc:Choice><mc:Fallback/></mc:AlternateContent>",
    );
    const EMPTY_ALTERNATE_ROW: &str = concat!(
        "<w:tr><w:trPr><w:del/></w:trPr><w:tc>",
        "<mc:AlternateContent><mc:Fallback/></mc:AlternateContent><w:p/></w:tc></w:tr>",
    );
    const RANGE_START: &str = r#"<w:moveFromRangeStart w:id="1" w:name="m"/>"#;
    const RANGE_END: &str = r#"<w:moveFromRangeEnd w:id="1"/>"#;
    const SYMBOL_STYLES: &str = concat!(
        r#"<w:style w:type="paragraph" w:styleId="Sym"><w:rPr>"#,
        r#"<w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr></w:style>"#,
    );
    const SYMBOL_DEFAULT_STYLE: &str = concat!(
        r#"<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:rPr>"#,
        r#"<w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr></w:style>"#,
    );
    const SYMBOL_DEFAULTS: &str = concat!(
        "<w:docDefaults><w:rPrDefault><w:rPr>",
        r#"<w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr></w:rPrDefault></w:docDefaults>"#,
    );
    const NBSP_PARAGRAPH: &str = "<w:p><w:r><w:t>\u{a0}</w:t></w:r></w:p>";
    const SYMBOL_NBSP_PARAGRAPH: &str = concat!(
        r#"<w:p><w:pPr><w:pStyle w:val="Sym"/></w:pPr>"#,
        "<w:r><w:t>\u{a0}</w:t></w:r></w:p>",
    );
    const CYCLIC_PARAGRAPH_STYLES: &str = concat!(
        r#"<w:style w:type="paragraph" w:styleId="A"><w:basedOn w:val="B"/></w:style>"#,
        r#"<w:style w:type="paragraph" w:styleId="B"><w:basedOn w:val="A"/></w:style>"#,
    );
    const CYCLIC_PARAGRAPH: &str =
        r#"<w:p><w:pPr><w:pStyle w:val="A"/></w:pPr><w:r><w:t>x</w:t></w:r></w:p>"#;

    fn equation(content: &str) -> String {
        format!("<m:oMath>{content}</m:oMath>")
    }

    fn math_run(text: &str) -> String {
        format!(r#"<m:r><m:t xml:space="preserve">{text}</m:t></m:r>"#)
    }

    fn in_alternate(content: &str) -> String {
        format!("<mc:AlternateContent><mc:Fallback>{content}</mc:Fallback></mc:AlternateContent>")
    }

    fn table(rows: &str) -> String {
        format!("<w:tbl>{rows}</w:tbl>")
    }

    type Reported = (DiagnosticCode, Region, u64);
    type Parts = Vec<(&'static str, String)>;

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of losses by region")]
    fn each_loss_is_reported_in_its_region() {
        use DiagnosticCode::*;
        use Region::{Body, Comment, Container, Footer, Header, Note};
        let chart = |xml: &str| {
            let mut parts = graphics(&drawing(CHART));
            parts.push(("word/charts/chart1.xml", xml.into()));
            parts
        };
        let charted = |drawn: &str| {
            let mut parts = graphics(&drawing(drawn));
            parts.push(("word/charts/chart1.xml", format!("<c:chartSpace {NS}/>")));
            parts
        };
        let alone = |body: &str| vec![("word/document.xml", document(body))];
        let declined_in_note = |choice: &str| {
            vec![
                ("word/document.xml", document("<w:p/>")),
                ("word/footnotes.xml", footnotes(&run(&alternate(choice, "")))),
            ]
        };
        let styled = |styles: &str, numbering: Option<&str>| {
            let mut parts = alone("<w:p/>");
            parts.push(("word/styles.xml", styles.into()));
            parts.extend(numbering.map(|xml| ("word/numbering.xml", xml.to_string())));
            parts
        };
        let headed = |rels: &[(&str, &str, &str)]| {
            let mut parts = alone(
                r#"<w:p><w:pPr><w:sectPr><w:headerReference r:id="rId1"/></w:sectPr></w:pPr></w:p>
                <w:sectPr><w:headerReference r:id="rId1"/><w:headerReference r:id="rId2"/>
                <w:footerReference r:id="rId3"/></w:sectPr>"#,
            );
            parts.push(("word/_rels/document.xml.rels", relationships(rels)));
            parts
        };
        let unreadable_rels = |body: &str| {
            vec![
                ("word/document.xml", document(body)),
                ("word/_rels/document.xml.rels", CORRUPT_RELS.to_string()),
            ]
        };
        let declined = |choice: &str| alone(&run(&alternate(choice, "<mc:Fallback/>")));
        let cases: Vec<(&str, Parts, Vec<Reported>)> = vec![
            (
                "an imported part",
                alone(r#"<w:altChunk r:id="rId1"/>"#),
                vec![(ImportedPartUnread, Body, 1)],
            ),
            (
                "a subdocument",
                alone(r#"<w:p><w:subDoc r:id="rId1"/></w:p>"#),
                vec![(ImportedPartUnread, Body, 1)],
            ),
            (
                "deleted and moved-out text, in one range",
                alone(
                    "<w:p><w:del><w:r><w:delText>gone</w:delText></w:r></w:del>
                    <w:moveFrom><w:r><w:t>moved</w:t></w:r></w:moveFrom></w:p>",
                ),
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "a symbol that is not a checkbox",
                alone(&run(
                    r#"<w:sym w:font="Symbol" w:char="F060"/><w:sym w:font="Wingdings" w:char="F0FE"/>"#,
                )),
                vec![(UnmappedSymbol, Body, 1)],
            ),
            (
                "a dingbat no character encodes",
                alone(&run(
                    r#"<w:sym w:font="Wingdings" w:char="F0FF"/><w:sym w:font="Wingdings" w:char="F0FC"/>"#,
                )),
                vec![(UnmappedSymbol, Body, 1)],
            ),
            ("ruby", alone(&run("<w:ruby><w:rubyBase/></w:ruby>")), vec![(RubyUnread, Body, 1)]),
            (
                "ruby in a note",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    ("word/footnotes.xml", footnotes(&run("<w:ruby/>"))),
                ],
                vec![(RubyUnread, Note, 1)],
            ),
            ("ink", alone(&run(r#"<w:contentPart r:id="rId1"/>"#)), vec![(GraphicUnread, Body, 1)]),
            (
                "page and date characters",
                alone(&run("<w:pgNum/><w:yearLong/>")),
                vec![(FieldValueUnread, Body, 2)],
            ),
            (
                "form field states",
                alone(&run(
                    r#"<w:fldChar w:fldCharType="begin"><w:ffData><w:checkBox/></w:ffData></w:fldChar>
                    <w:fldChar w:fldCharType="end"/>
                    <w:fldChar w:fldCharType="begin"><w:ffData><w:ddList/></w:ffData></w:fldChar>
                    <w:fldChar w:fldCharType="end"/>
                    <w:fldChar w:fldCharType="begin"><w:ffData><w:textInput/></w:ffData></w:fldChar>
                    <w:fldChar w:fldCharType="end"/>"#,
                )),
                vec![(FormControlUnread, Body, 2)],
            ),
            (
                "an undefined numbering instance",
                alone(NUMBERED),
                vec![(NumberingUnresolved, Body, 1)],
            ),
            (
                "an instance of an unknown abstract, once",
                vec![
                    ("word/document.xml", document(NUMBERED)),
                    (
                        "word/numbering.xml",
                        format!(
                            r#"<w:numbering {NS}><w:num w:numId="7"><w:abstractNumId w:val="3"/></w:num></w:numbering>"#
                        ),
                    ),
                ],
                vec![(NumberingUnresolved, Body, 1)],
            ),
            (
                "a numbering part rooted elsewhere, at the part alone",
                vec![
                    ("word/document.xml", document(NUMBERED)),
                    ("word/numbering.xml", format!("<w:styles {NS}/>")),
                ],
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "an unrelated numbering part that does not parse, at the part alone",
                vec![
                    ("word/document.xml", document(NUMBERED)),
                    ("word/numbering.xml", "</w:numbering>".to_string()),
                ],
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "a related numbering part that is absent, at the part alone",
                vec![
                    ("word/document.xml", document(NUMBERED)),
                    (
                        "word/_rels/document.xml.rels",
                        relationships(&[("rId1", "numbering", "numbering.xml")]),
                    ),
                ],
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "an undefined numbering instance in a note",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    ("word/footnotes.xml", footnotes(NUMBERED)),
                ],
                vec![(NumberingUnresolved, Note, 1)],
            ),
            (
                "an undefined numbering instance in a note, beside a numbering part reported",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    ("word/footnotes.xml", footnotes(NUMBERED)),
                    ("word/numbering.xml", format!("<w:styles {NS}/>")),
                ],
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "a styles part rooted elsewhere, beside numbering",
                styled(&format!("<w:numbering {NS}/>"), Some(&format!("<w:numbering {NS}/>"))),
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "a styles part rooted elsewhere, alone",
                styled(&format!("<w:numbering {NS}/>"), None),
                vec![(StylePartUnreadable, Container, 1)],
            ),
            (
                "a styles part that does not parse and sets vanish, alone",
                styled(&format!("<w:styles {NS}><w:style>{HIDE_RPR}</w:style>"), None),
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "a styles part that does not parse and sets vanish, beside numbering",
                styled(
                    &format!("<w:styles {NS}><w:style>{HIDE_RPR}</w:style>"),
                    Some(&format!("<w:numbering {NS}/>")),
                ),
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "a styles part that does not parse and sets no vanish, alone",
                styled(&format!("<w:styles {NS}><w:style></w:styles>"), None),
                vec![(StylePartUnreadable, Container, 1)],
            ),
            (
                "a styles part that does not parse and sets specVanish alone",
                styled(&format!("<w:styles {NS}><w:style><w:specVanish/></w:style>"), None),
                vec![(StylePartUnreadable, Container, 1)],
            ),
            (
                "a styles part that does not parse and turns vanish off, alone",
                styled(&format!(r#"<w:styles {NS}><w:style><w:vanish w:val="0"/></w:style>"#), None),
                vec![(StylePartUnreadable, Container, 1)],
            ),
            (
                "a styles part that does not parse and turns vanish on, alone",
                styled(&format!(r#"<w:styles {NS}><w:style><w:vanish w:val="1"/></w:style>"#), None),
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "a styles part that does not parse and turns vanish off unprefixed, alone",
                styled(&format!(r#"<w:styles {NS}><w:style><w:vanish val="0"/></w:style>"#), None),
                vec![(StylePartUnreadable, Container, 1)],
            ),
            (
                "a styles part that does not parse and turns vanish on qualified after off unprefixed",
                styled(&format!(r#"<w:styles {NS}><w:vanish val="0" w:val="1"/>"#), None),
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "a styles part that does not parse and turns vanish off in a foreign namespace",
                styled(&format!(r#"<w:styles {NS}><w:vanish r:val="0"/>"#), None),
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "a styles part that does not parse and turns vanish off after another attribute",
                styled(&format!(r#"<w:styles {NS}><w:vanish w:x="a" w:val='0'/>"#), None),
                vec![(StylePartUnreadable, Container, 1)],
            ),
            (
                "a styles part that does not parse and turns vanish off after a greater-than sign in a value",
                styled(&format!(r#"<w:styles {NS}><w:vanish w:x=">" w:val="0"/>"#), None),
                vec![(StylePartUnreadable, Container, 1)],
            ),
            (
                "a styles part that does not parse and sets vanish after an instruction opening a comment",
                styled(&format!("<w:styles {NS}><?pi <!--?>{HIDE_RPR}"), None),
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "a styles part that does not parse and names vanish in an unclosed comment",
                styled(&format!("<w:styles {NS}><!-- {HIDE_RPR}"), None),
                vec![(StylePartUnreadable, Container, 1)],
            ),
            (
                "a styles part that does not parse and sets vanish with another attribute off",
                styled(&format!(r#"<w:styles {NS}><w:vanish w:x="0"/>"#), None),
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "a styles part that does not parse and names vanish in a comment, alone",
                styled(&format!("<w:styles {NS}><!-- {HIDE_RPR} --><w:style></w:styles>"), None),
                vec![(StylePartUnreadable, Container, 1)],
            ),
            (
                "a styles part that does not parse and names vanish in character data, alone",
                styled(&format!("<w:styles {NS}><w:t><![CDATA[{HIDE_RPR}]]></w:t><w:style>"), None),
                vec![(StylePartUnreadable, Container, 1)],
            ),
            (
                "a styles part that does not parse and names vanish in an attribute, alone",
                styled(&format!(r#"<w:styles {NS}><w:style w:styleId="vanish"></w:styles>"#), None),
                vec![(StylePartUnreadable, Container, 1)],
            ),
            (
                "a styles part that does not parse and sets vanish after a comment, alone",
                styled(&format!("<w:styles {NS}><!-- x --><w:style>{HIDE_RPR}</w:style>"), None),
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "a styles part rooted elsewhere that sets vanish, alone",
                styled(&format!("<w:numbering {NS}><w:lvl>{HIDE_RPR}</w:lvl></w:numbering>"), None),
                vec![(StylePartUnreadable, Container, 1)],
            ),
            (
                "a related styles part that is absent, alone",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    (
                        "word/_rels/document.xml.rels",
                        relationships(&[("rId1", "styles", "styles.xml")]),
                    ),
                ],
                vec![(StylePartUnreadable, Container, 1)],
            ),
            (
                "a notes part rooted elsewhere",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    ("word/footnotes.xml", format!("<w:endnotes {NS}/>")),
                ],
                vec![(PartUnreadable, Note, 1)],
            ),
            (
                "a typed target that does not resolve",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    (
                        "word/_rels/document.xml.rels",
                        relationships(&[("rId1", "styles", "a%2Fb.xml")]),
                    ),
                ],
                vec![(StylePartUnreadable, Container, 1), (ConventionalPartUsed, Container, 1)],
            ),
            (
                "a typed target that does not resolve, beside the conventional part",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    ("word/styles.xml", format!("<w:styles {NS}/>")),
                    (
                        "word/_rels/document.xml.rels",
                        relationships(&[("rId1", "styles", "a%2Fb.xml")]),
                    ),
                ],
                vec![(ConventionalPartUsed, Container, 1)],
            ),
            (
                "a main part target that does not resolve",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    ("_rels/.rels", relationships(&[("rId1", "officeDocument", "a%2Fb.xml")])),
                ],
                vec![(ConventionalPartUsed, Container, 1)],
            ),
            (
                "a main part target that names no part",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    ("_rels/.rels", relationships(&[("rId1", "officeDocument", "")])),
                ],
                vec![(ConventionalPartUsed, Container, 1)],
            ),
            (
                "headers and footers of unknown ids, each id once",
                headed(&[]),
                vec![(HeaderOmitted, Header, 2), (FooterOmitted, Footer, 1)],
            ),
            (
                "headers of two ids naming one part, once",
                headed(&[
                    ("rId1", "header", "header1.xml"),
                    ("rId2", "header", "./header1.xml"),
                    ("rId3", "footer", "footer1.xml"),
                ]),
                vec![(HeaderOmitted, Header, 1), (FooterOmitted, Footer, 1)],
            ),
            (
                "a comment referenced from a note",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    ("word/footnotes.xml", footnotes(&run(r#"<w:commentReference w:id="1"/>"#))),
                ],
                vec![(CommentOmitted, Comment, 1)],
            ),
            (
                "each comment",
                alone(&run(r#"<w:commentReference w:id="1"/><w:commentReference w:id="2"/>"#)),
                vec![(CommentOmitted, Comment, 2)],
            ),
            (
                "comments in the body and a note, in one total",
                vec![
                    (
                        "word/document.xml",
                        document(&run(
                            r#"<w:commentReference w:id="1"/><w:footnoteReference w:id="1"/>"#,
                        )),
                    ),
                    ("word/footnotes.xml", footnotes(&run(r#"<w:commentReference w:id="2"/>"#))),
                ],
                vec![(CommentOmitted, Comment, 2)],
            ),
            (
                "a comment in a field instruction",
                alone(&run(concat!(
                    r#"<w:fldChar w:fldCharType="begin"/><w:commentReference w:id="1"/>"#,
                    r#"<w:fldChar w:fldCharType="separate"/>"#,
                ))),
                vec![],
            ),
            (
                "WordArt",
                alone(&run(r#"<w:pict><v:shape><v:textpath string="Big"/></v:shape></w:pict>"#)),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "WordArt of blank text",
                alone(&run(r#"<w:pict><v:shape><v:textpath string=" "/></v:shape></w:pict>"#)),
                vec![],
            ),
            (
                "a VML picture named by o:relid",
                graphics(&run(
                    r#"<w:pict><v:shape><v:imagedata o:relid="rId2"/></v:shape></w:pict>"#,
                )),
                vec![(ImageUnavailable, Body, 1)],
            ),
            (
                "a VML picture fill",
                graphics(&run(&format!("<w:pict>{VML_FILL}</w:pict>"))),
                vec![(ImagePlaceholder, Body, 1)],
            ),
            (
                "a VML tile fill",
                graphics(&run(r#"<w:pict><v:oval><v:fill r:id="rId2" type="tile"/></v:oval></w:pict>"#)),
                vec![(ImagePlaceholder, Body, 1)],
            ),
            (
                "a VML picture fill switched on",
                graphics(&run(
                    r#"<w:pict><v:rect><v:fill r:id="rId2" type="frame" on="true"/></v:rect></w:pict>"#,
                )),
                vec![(ImagePlaceholder, Body, 1)],
            ),
            (
                "a VML picture fill switched on under a shape switched off",
                graphics(&run(
                    r#"<w:pict><v:rect filled="f"><v:fill r:id="rId2" type="frame" on="t"/></v:rect></w:pict>"#,
                )),
                vec![(ImagePlaceholder, Body, 1)],
            ),
            (
                "a VML picture fill under a shape switched on",
                graphics(&run(
                    r#"<w:pict><v:rect filled="true"><v:fill r:id="rId2" type="frame"/></v:rect></w:pict>"#,
                )),
                vec![(ImagePlaceholder, Body, 1)],
            ),
            (
                "a VML picture fill of a shape whose type is switched off",
                graphics(&run(
                    r##"<w:pict><v:shapetype id="t" filled="f"/><v:rect type="#t"><v:fill r:id="rId2" type="frame"/></v:rect></w:pict>"##,
                )),
                vec![(ImagePlaceholder, Body, 1)],
            ),
            (
                "a VML picture fill under a group switched off",
                graphics(&run(
                    r#"<w:pict><v:group filled="f"><v:fill r:id="rId2" type="frame"/></v:group></w:pict>"#,
                )),
                vec![(ImagePlaceholder, Body, 1)],
            ),
            (
                "a VML picture fill named by o:relid",
                graphics(&run(r#"<w:pict><v:rect><v:fill o:relid="rId2" type="frame"/></v:rect></w:pict>"#)),
                vec![(ImageUnavailable, Body, 1)],
            ),
            (
                "a VML picture fill beside a VML picture",
                graphics(&run(&format!("<w:pict>{VML_FILL}{VML_PICTURE}</w:pict>"))),
                vec![(ImagePlaceholder, Body, 1), (ImageUnplaced, Body, 1)],
            ),
            (
                "a VML picture fill beside a text box",
                graphics(&run(&format!(
                    r#"<w:pict><v:rect id="s1"><v:fill r:id="rId2" type="frame"/>{}</v:rect></w:pict>"#,
                    text_box("caption")
                ))),
                vec![(ImageUnplaced, Body, 1)],
            ),
            (
                "an embedded object, its preview aside",
                graphics(&run(&format!("<w:object>{VML_PICTURE}{OLE}</w:object>"))),
                vec![(ImagePlaceholder, Body, 1), (EmbeddedObject, Body, 1)],
            ),
            (
                "an embedded object beside a picture that is not its preview",
                graphics(&run(&format!(
                    r#"<w:pict><v:group>{VML_PICTURE}{OLE}<v:shape id="s2"><v:imagedata r:id="rId2"/></v:shape></v:group></w:pict>"#
                ))),
                vec![(ImagePlaceholder, Body, 1), (ImageUnplaced, Body, 1), (EmbeddedObject, Body, 1)],
            ),
            (
                "a chart whose part is absent",
                graphics(&drawing(CHART)),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "a chart with no part",
                graphics(&drawing("<c:chart/>")),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "a chart with no part beside the picture read",
                graphics(&drawing(&format!("<c:chart/>{PICTURE}"))),
                vec![(ImagePlaceholder, Body, 1), (GraphicUnread, Body, 1)],
            ),
            (
                "a diagram with no part beside the picture read",
                graphics(&drawing(&format!("<dgm:relIds/>{PICTURE}"))),
                vec![(ImagePlaceholder, Body, 1), (GraphicUnread, Body, 1)],
            ),
            (
                "a chart beside a diagram, which is left unread",
                charted(&format!(r#"{CHART}<dgm:relIds r:dm="rId5"/>"#)),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "a diagram before a chart, which is read",
                charted(&format!(r#"<dgm:relIds r:dm="rId5"/>{CHART}"#)),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "two charts, the second left unread",
                charted(&format!(r#"{CHART}<c:chart r:id="rId9"/>"#)),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "a diagram beside an object, which is left unread",
                {
                    let mut parts = graphics(&drawing(&format!(r#"<dgm:relIds r:dm="rId5"/>{OLE}"#)));
                    parts.push(("word/diagrams/data1.xml", format!("<dgm:dataModel {NS}/>")));
                    parts
                },
                vec![(EmbeddedObject, Body, 1)],
            ),
            (
                "a chart part rooted elsewhere",
                chart(&format!("<dgm:dataModel {NS}/>")),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "a diagram part rooted elsewhere",
                {
                    let mut parts = graphics(&drawing(r#"<dgm:relIds r:dm="rId5"/>"#));
                    parts.push(("word/diagrams/data1.xml", format!("<c:chartSpace {NS}/>")));
                    parts
                },
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "a diagram whose part is absent",
                graphics(&drawing(r#"<dgm:relIds r:dm="rId5"/>"#)),
                vec![(GraphicUnread, Body, 1)],
            ),
            ("a corrupt chart", chart("</c:chartSpace>"), vec![(GraphicUnread, Body, 1)]),
            (
                "a corrupt diagram",
                {
                    let mut parts = graphics(&drawing(r#"<dgm:relIds r:dm="rId5"/>"#));
                    parts.push(("word/diagrams/data1.xml", "</dgm:dataModel>".into()));
                    parts
                },
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "a chart's lost data",
                chart(&format!("<c:chartSpace {NS}><c:ser><c:xVal/></c:ser></c:chartSpace>")),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "shape text in a drawing",
                graphics(&drawing(SHAPE_TEXT)),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "shape text beside the picture read",
                graphics(&drawing(&format!("{PICTURE}{SHAPE_TEXT}"))),
                vec![(ImagePlaceholder, Body, 1), (GraphicUnread, Body, 1)],
            ),
            (
                "shape text beside a text box",
                graphics(&drawing(&format!("{}{SHAPE_TEXT}", text_box("caption")))),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "a picture with no reference",
                graphics(&drawing("<a:blip/>")),
                vec![(ImageUnavailable, Body, 1)],
            ),
            (
                "a picture whose part is absent",
                graphics(&drawing(r#"<a:blip r:embed="rId3"/>"#)),
                vec![(ImageUnavailable, Body, 1)],
            ),
            (
                "a picture of an unknown id",
                graphics(&drawing(r#"<a:blip r:embed="rId9"/>"#)),
                vec![(ImageUnavailable, Body, 1)],
            ),
            (
                "a picture behind an unreadable relationships part",
                unreadable_rels(&drawing(r#"<a:blip r:embed="rId9"/>"#)),
                vec![(PartUnreadable, Body, 1), (ImageUnavailable, Body, 1)],
            ),
            (
                "a link behind an unreadable relationships part",
                unreadable_rels(
                    r#"<w:p><w:hyperlink r:id="rId9"><w:r><w:t>see here</w:t></w:r></w:hyperlink></w:p>"#,
                ),
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "a chart behind an unreadable relationships part",
                unreadable_rels(&drawing(CHART)),
                vec![(PartUnreadable, Body, 1)],
            ),
            (
                "graphics beside a text box",
                graphics(&drawing(&format!("{}{PICTURE}{CHART}{OLE}", text_box("caption")))),
                vec![(EmbeddedObject, Body, 1), (GraphicUnread, Body, 1)],
            ),
            (
                "a picture of another shape beside a text box",
                graphics(&drawing(&format!(
                    r#"{}<v:shape id="s2"><v:imagedata r:id="rId2"/></v:shape>{CHART}{OLE}"#,
                    text_box("caption")
                ))),
                vec![(ImageUnplaced, Body, 1), (EmbeddedObject, Body, 1), (GraphicUnread, Body, 1)],
            ),
            (
                "every picture after the first",
                graphics(&drawing(&format!("{PICTURE}{PICTURE}"))),
                vec![(ImagePlaceholder, Body, 1), (ImageUnplaced, Body, 1)],
            ),
            (
                "a link of an unknown id, unless its label is a URL or it has an anchor",
                alone(
                    r#"<w:p><w:hyperlink r:id="rId9"><w:r><w:t>see here</w:t></w:r></w:hyperlink>
                    <w:hyperlink r:id="rId9" w:anchor="top"><w:r><w:t>see top</w:t></w:r></w:hyperlink>
                    <w:hyperlink r:id="rId9"><w:r><w:t>https://example.org</w:t></w:r></w:hyperlink></w:p>"#,
                ),
                vec![(LinkTargetUnread, Body, 1), (DanglingReference, Body, 1)],
            ),
            (
                "a declined chart, whatever the fallback shows",
                alone(&run(&alternate(
                    r#"<w:drawing><cx:chart r:id="rId3"/></w:drawing>"#,
                    "<mc:Fallback><w:t>This chart isn't available</w:t></mc:Fallback>",
                ))),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "declined ink",
                alone(&run(&alternate(r#"<w:contentPart r:id="rId1"/>"#, ""))),
                vec![(GraphicUnread, Body, 1)],
            ),
            ("declined ink in a note", declined_in_note(r#"<w:contentPart r:id="rId1"/>"#), vec![(GraphicUnread, Note, 1)]),
            (
                "a declined picture in a note",
                declined_in_note(&format!("<w:drawing>{PICTURE}</w:drawing>")),
                vec![(ImageUnplaced, Note, 1)],
            ),
            (
                "a picture in a note, resolved against the note part",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    ("word/footnotes.xml", footnotes(&drawing(PICTURE))),
                    (
                        "word/_rels/footnotes.xml.rels",
                        relationships(&[("rId2", "image", "media/one.png")]),
                    ),
                    ("word/media/one.png", "png".into()),
                ],
                vec![(ImagePlaceholder, Note, 1)],
            ),
            ("declined text in a note", declined_in_note("<w:t>words</w:t>"), vec![(BlockUnread, Note, 1)]),
            (
                "a note id repeated twice after its first",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    (
                        "word/footnotes.xml",
                        format!(
                            "<w:footnotes {NS}>{}</w:footnotes>",
                            r#"<w:footnote w:id="1"><w:p/></w:footnote>"#.repeat(3)
                        ),
                    ),
                ],
                vec![(DuplicateNote, Note, 2)],
            ),
            (
                "a declined DrawingML chart",
                declined(&format!("<w:drawing>{CHART}</w:drawing>")),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "a declined diagram",
                declined(r#"<w:drawing><dgm:relIds r:dm="rId5"/></w:drawing>"#),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "a declined VML picture",
                declined(&format!("<w:pict>{VML_PICTURE}</w:pict>")),
                vec![(ImageUnplaced, Body, 1)],
            ),
            (
                "a declined VML picture fill",
                declined(&format!("<w:pict>{VML_FILL}</w:pict>")),
                vec![(ImageUnplaced, Body, 1)],
            ),
            (
                "a declined VML picture fill its shape switched off",
                declined(
                    r#"<w:pict><v:rect filled="f"><v:fill r:id="rId2" type="frame"/></v:rect></w:pict>"#,
                ),
                vec![],
            ),
            (
                "a declined picture over a VML picture fill the fallback shows",
                graphics(&run(&alternate(
                    &format!("<w:drawing>{PICTURE}</w:drawing>"),
                    &format!("<mc:Fallback><w:pict>{VML_FILL}</w:pict></mc:Fallback>"),
                ))),
                vec![(ImagePlaceholder, Body, 1)],
            ),
            (
                "a declined picture over a VML picture fill its shape switched off in the fallback",
                graphics(&run(&alternate(
                    &format!("<w:drawing>{PICTURE}</w:drawing>"),
                    r#"<mc:Fallback><w:pict><v:rect filled="f"><v:fill r:id="rId2" type="frame"/></v:rect></w:pict></mc:Fallback>"#,
                ))),
                vec![(ImageUnplaced, Body, 1)],
            ),
            (
                "a declined picture over a pattern fill the fallback shows",
                graphics(&run(&alternate(
                    &format!("<w:drawing>{PICTURE}</w:drawing>"),
                    r#"<mc:Fallback><w:pict><v:rect><v:fill r:id="rId2" type="pattern"/></v:rect></w:pict></mc:Fallback>"#,
                ))),
                vec![(ImageUnplaced, Body, 1)],
            ),
            (
                "a declined picture over a fill of another vocabulary the fallback shows",
                graphics(&run(&alternate(
                    &format!("<w:drawing>{PICTURE}</w:drawing>"),
                    r#"<mc:Fallback><w:pict><v:rect><o:fill r:id="rId2" type="frame"/></v:rect></w:pict></mc:Fallback>"#,
                ))),
                vec![(ImageUnplaced, Body, 1)],
            ),
            (
                "declined shape text",
                declined(&format!("<w:drawing>{SHAPE_TEXT}</w:drawing>")),
                vec![(BlockUnread, Body, 1)],
            ),
            (
                "declined blank text",
                declined(r#"<w:t xml:space="preserve"> </w:t>"#),
                vec![],
            ),
            (
                "a declined picture and text the fallback lacks",
                alone(&run(&alternate(
                    &format!("<w:drawing>{PICTURE}{}</w:drawing>", text_box("caption")),
                    "<mc:Fallback/>",
                ))),
                vec![(BlockUnread, Body, 1), (ImageUnplaced, Body, 1)],
            ),
            (
                "a declined picture and text the fallback shows",
                graphics(&run(&alternate(
                    &format!("<w:drawing>{PICTURE}{}</w:drawing>", text_box("caption")),
                    &format!(
                        "<mc:Fallback><w:drawing>{PICTURE}</w:drawing><w:pict>{}</w:pict></mc:Fallback>",
                        text_box("caption")
                    ),
                ))),
                vec![(ImagePlaceholder, Body, 1)],
            ),
            (
                "deleted rows and cells that hold text",
                alone(&format!(
                    "<w:tbl>{DELETED_TEXT_ROW}<w:tr>{DELETED_TEXT_CELL}{CELL}</w:tr></w:tbl>"
                )),
                vec![(RevisionOmitted, Body, 2)],
            ),
            (
                "an empty deleted row",
                alone(&format!("<w:tbl>{EMPTY_DELETED_ROW}<w:tr>{TEXT_CELL}</w:tr></w:tbl>")),
                vec![],
            ),
            (
                "an empty deleted cell",
                alone(&format!("<w:tbl><w:tr>{EMPTY_DELETED_CELL}{TEXT_CELL}</w:tr></w:tbl>")),
                vec![],
            ),
            (
                "a deleted row that holds alternate content",
                alone(&format!(
                    "<w:tbl><w:tr>{ROW_DEL}<w:tc>{}</w:tc></w:tr><w:tr>{TEXT_CELL}</w:tr></w:tbl>",
                    alternate("", &format!("<mc:Fallback>{TEXT_PARAGRAPH}</mc:Fallback>")),
                )),
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "a deleted row that holds alternate content, whose taken branch is empty",
                alone(&format!(
                    "<w:tbl><w:tr>{ROW_DEL}<w:tc>{}</w:tc></w:tr><w:tr>{TEXT_CELL}</w:tr></w:tbl>",
                    alternate(TEXT_PARAGRAPH, ""),
                )),
                vec![],
            ),
            (
                "rows and cells behind wrappers",
                alone(&format!(
                    "<w:tbl><w:tr>{CELL}{}</w:tr><w:sdt><w:sdtContent><w:tr>{CELL}</w:tr></w:sdtContent></w:sdt>
                    <w:customXml><w:tr>{CELL}</w:tr></w:customXml>{}</w:tbl>",
                    alternate(CELL, ""),
                    alternate(&format!("<w:tr>{CELL}</w:tr>"), ""),
                )),
                vec![(BlockUnread, Body, 4)],
            ),
        ];
        assert_reports(cases);
    }

    #[test]
    fn an_imported_part_alone_fails_the_conversion() {
        let bytes =
            zip_package(&[("word/document.xml", &document(r#"<w:altChunk r:id="rId1"/>"#))]);
        let converter = crate::Converter::new(crate::Config::default()).unwrap();
        let result = converter.convert(crate::Input::new(&bytes, crate::Format::Docx));
        assert_matches!(result, Err(ConvertError::Incomplete { .. }));
    }

    fn styles_part_losses(parts: &[(&str, Vec<u8>)]) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, &zip_package(parts)).unwrap();
        diagnostic_rows(&diagnostics_of(parse_ctx, &doc, ImagePolicy::Placeholder).unwrap())
    }

    #[test]
    fn a_styles_part_that_may_hide_text_and_does_not_parse_fails_the_conversion() {
        let hiding = format!("<w:styles {NS}>{HIDING_STYLES}");
        let plain = format!("<w:styles {NS}><w:style></w:styles>");
        let body = p(r#"<w:pPr><w:pStyle w:val="HidePara"/></w:pPr>"#, &shown("secret"));
        let converter = crate::Converter::new(crate::Config::default()).unwrap();
        for (styles, fails) in [(hiding, true), (plain, false)] {
            let bytes = zip_package(&[
                ("word/document.xml", &document(&body)),
                ("word/styles.xml", &styles),
            ]);
            let result = converter.convert(crate::Input::new(&bytes, crate::Format::Docx));
            assert_eq!(matches!(result, Err(ConvertError::Incomplete { .. })), fails, "{styles}");
        }
    }

    #[test]
    fn a_utf16_styles_part_that_does_not_parse_is_scanned_as_decoded_text() {
        use DiagnosticCode::{PartUnreadable, StylePartUnreadable};
        use Region::{Body, Container};
        let utf16 = |text: &str| {
            let mut bytes = vec![0xFF, 0xFE];
            bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
            bytes
        };
        let document = document("<w:p/>").into_bytes();
        for (styles, expected) in [
            (format!("<w:styles {NS}><w:style>{HIDE_RPR}</w:style>"), (PartUnreadable, Body, 1)),
            (format!("<w:styles {NS}><w:style></w:styles>"), (StylePartUnreadable, Container, 1)),
        ] {
            let parts =
                [("word/document.xml", document.clone()), ("word/styles.xml", utf16(&styles))];
            assert_eq!(styles_part_losses(&parts), [expected], "{styles}");
        }
    }

    #[test]
    fn a_corrupt_styles_entry_is_a_loss_of_the_text_it_may_hide() {
        let mut bytes = zip_package_with(
            &[
                ("word/document.xml", document("<w:p/>")),
                ("word/styles.xml", format!("<w:styles {NS}/>")),
            ],
            stored(),
        );
        corrupt_entry(&mut bytes, "word/styles.xml");
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, &bytes).unwrap();
        let diagnostics = diagnostics_of(parse_ctx, &doc, ImagePolicy::Placeholder).unwrap();
        let reported: Vec<Reported> = diagnostic_rows(&diagnostics);
        assert_eq!(reported, [(DiagnosticCode::PartUnreadable, Region::Body, 1)]);
    }

    #[test]
    fn the_scan_of_a_styles_part_that_does_not_parse_charges_its_length() {
        use crate::limits::{Limit, Limits};
        let limits = Limits { max_work: 2000, ..Limits::default() };
        let scan = |filler: usize| {
            let styles = format!("<w:styles {NS}><w:style>{}", "x".repeat(filler));
            let bytes = zip_package(&[
                ("word/document.xml", document("<w:p/>")),
                ("word/styles.xml", styles),
            ]);
            parse(&ParseCtx::new(&limits), &bytes)
        };
        scan(100).unwrap();
        assert_matches!(
            scan(4000),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 2000 })
        );
    }

    #[test]
    fn a_deleted_paragraph_mark_joins_the_next_paragraph() {
        let deleted = r#"<w:rPr><w:del w:id="1"/></w:rPr>"#;
        let item = |mark: &str, text: &str| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:numId w:val="7"/></w:numPr>{mark}</w:pPr><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>"#
            )
        };
        let numbering = format!(
            r#"<w:numbering {NS}><w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="1"/></w:num></w:numbering>"#
        );
        let cases = [
            (
                format!(
                    "<w:p><w:pPr>{deleted}</w:pPr><w:r><w:t>one </w:t></w:r></w:p><w:p><w:r><w:t>two</w:t></w:r></w:p>
                    <w:p><w:pPr>{deleted}</w:pPr><w:r><w:t>last</w:t></w:r></w:p>"
                ),
                "one two\n\nlast\n",
            ),
            (
                format!(
                    "{}{}{}{}",
                    item("", "one"),
                    item(deleted, "two "),
                    item("", "three"),
                    item("", "four")
                ),
                "1. one\n2. two three\n3. four\n",
            ),
        ];
        for (body, expected) in cases {
            let bytes = zip_package(&[
                ("word/document.xml", &document(&body)),
                ("word/numbering.xml", &numbering),
            ]);
            let markdown = document_to_markdown(&parse(&ParseCtx::default(), &bytes).unwrap());
            assert_eq!(markdown, expected);
        }
    }

    const HIDE: &str = "<w:rPr><w:vanish/></w:rPr>";
    const SHOW: &str = r#"<w:rPr><w:vanish w:val="0"/></w:rPr>"#;
    const LINK_RELS: &str = concat!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
        r#"<Relationship Id="rId1" "#,
        r#"Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" "#,
        r#"Target="https://www.example.org/" TargetMode="External"/></Relationships>"#,
    );
    const HIDING_STYLES: &str = concat!(
        r#"<w:style w:type="paragraph" w:styleId="HidePara"><w:rPr><w:vanish/></w:rPr>"#,
        "</w:style>",
        r#"<w:style w:type="paragraph" w:styleId="HideTwice"><w:basedOn w:val="HidePara"/>"#,
        r#"<w:rPr><w:vanish/></w:rPr></w:style>"#,
        r#"<w:style w:type="paragraph" w:styleId="FalseAdds"><w:basedOn w:val="HidePara"/>"#,
        r#"<w:rPr><w:vanish w:val="0"/></w:rPr></w:style>"#,
        r#"<w:style w:type="character" w:styleId="HideChar"><w:rPr><w:vanish/></w:rPr>"#,
        "</w:style>",
    );
    const HIDING_DEFAULTS: &str =
        "<w:docDefaults><w:rPrDefault><w:rPr><w:vanish/></w:rPr></w:rPrDefault></w:docDefaults>";

    fn r(rpr: &str, text: &str) -> String {
        format!(r#"<w:r>{rpr}<w:t xml:space="preserve">{text}</w:t></w:r>"#)
    }

    fn p(ppr: &str, runs: &str) -> String {
        format!("<w:p>{ppr}{runs}</w:p>")
    }

    fn shown(text: &str) -> String {
        r("", text)
    }

    fn hidden(text: &str) -> String {
        r(HIDE, text)
    }

    fn hidden_run(content: &str) -> String {
        format!("<w:r>{HIDE}{content}</w:r>")
    }

    fn deleted(text: &str) -> String {
        let run = format!(r#"<w:r><w:delText xml:space="preserve">{text}</w:delText></w:r>"#);
        format!(r#"<w:del w:id="90">{run}</w:del>"#)
    }

    fn deleted_run(content: &str) -> String {
        format!(r#"<w:del w:id="1"><w:r>{content}</w:r></w:del>"#)
    }

    fn mark(rpr: &str) -> String {
        format!("<w:pPr><w:rPr>{rpr}</w:rPr></w:pPr>")
    }

    fn in_style(id: &str, runs: &str) -> String {
        p(&format!(r#"<w:pPr><w:pStyle w:val="{id}"/></w:pPr>"#), runs)
    }

    fn field(rpr: &str, kind: &str) -> String {
        format!(r#"<w:r>{rpr}<w:fldChar w:fldCharType="{kind}"/></w:r>"#)
    }

    fn instr(rpr: &str, text: &str) -> String {
        format!(r#"<w:r>{rpr}<w:instrText xml:space="preserve">{text}</w:instrText></w:r>"#)
    }

    fn link(content: &str) -> String {
        format!(r#"<w:hyperlink r:id="rId1">{content}</w:hyperlink>"#)
    }

    fn move_source(name: &str, text: &str) -> String {
        let start = format!(r#"<w:moveFromRangeStart w:id="1" w:name="{name}"/>"#);
        let run = shown(text);
        format!(r#"{start}<w:moveFrom w:id="2">{run}</w:moveFrom><w:moveFromRangeEnd w:id="1"/>"#)
    }

    fn move_out(text: &str) -> String {
        format!(r#"<w:moveFrom w:id="2">{}</w:moveFrom>"#, shown(text))
    }

    const UNCLOSED_RANGE: &str = concat!(
        r#"<w:moveFromRangeStart w:id="1" w:name="m"/>"#,
        r#"<w:moveFrom w:id="2"><w:r><w:t>one</w:t></w:r></w:moveFrom>"#,
    );
    const DUPLICATED_RANGE_IDS: &str = concat!(
        r#"<w:moveFromRangeStart w:id="1" w:name="a"/>"#,
        r#"<w:moveFromRangeStart w:id="1" w:name="b"/>"#,
        r#"<w:moveFrom w:id="2"><w:r><w:t>one</w:t></w:r></w:moveFrom>"#,
        r#"<w:moveFromRangeEnd w:id="1"/>"#,
        r#"<w:moveFrom w:id="2"><w:r><w:t>two</w:t></w:r></w:moveFrom>"#,
        r#"<w:moveFromRangeEnd w:id="1"/>"#,
    );

    fn move_destination(name: &str, text: &str) -> String {
        let start = format!(r#"<w:moveToRangeStart w:id="3" w:name="{name}"/>"#);
        let run = shown(text);
        format!(r#"{start}<w:moveTo w:id="4">{run}</w:moveTo><w:moveToRangeEnd w:id="3"/>"#)
    }

    fn footnotes(content: &str) -> String {
        format!(r#"<w:footnotes {NS}><w:footnote w:id="1">{content}</w:footnote></w:footnotes>"#)
    }

    fn styled(body: &str, styles: &str) -> Parts {
        vec![
            ("word/document.xml", document(body)),
            ("word/styles.xml", format!("<w:styles {NS}>{styles}</w:styles>")),
        ]
    }

    fn one(runs: &str) -> Parts {
        styled(&p("", runs), "")
    }

    fn two(first: &str, second: &str) -> Parts {
        styled(&format!("{}{}", p("", first), p("", second)), "")
    }

    fn hide(count: u64) -> Vec<Reported> {
        vec![(DiagnosticCode::HiddenOmitted, Region::Body, count)]
    }

    fn revise(count: u64) -> Vec<Reported> {
        vec![(DiagnosticCode::RevisionOmitted, Region::Body, count)]
    }

    fn deletion(content: &str) -> String {
        format!(r#"<w:del w:id="1">{content}</w:del>"#)
    }

    fn char_style(id: &str, text: &str) -> String {
        r(&format!(r#"<w:rPr><w:rStyle w:val="{id}"/></w:rPr>"#), text)
    }

    fn fielded(inside: &str, result: &str) -> String {
        let (begin, separate, end) = (field("", "begin"), field("", "separate"), field("", "end"));
        format!("{begin}{inside}{separate}{}{end}", shown(result))
    }

    fn in_instruction(content: &str) -> String {
        fielded(&format!("{}{content}", instr("", "REF here")), "result")
    }

    fn converted(parts: &Parts) -> (String, Vec<Reported>) {
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, &zip_package(parts)).unwrap();
        let diagnostics = diagnostics_of(parse_ctx, &doc, ImagePolicy::Placeholder).unwrap();
        let mut reported: Vec<Reported> = diagnostic_rows(&diagnostics);
        reported.sort();
        (document_to_markdown(&doc), reported)
    }

    fn assert_cases<E: PartialEq + Debug>(cases: Vec<(&str, Parts, E)>, got: impl Fn(&Parts) -> E) {
        let wrong: Vec<String> = cases
            .into_iter()
            .filter_map(|(case, parts, expected)| {
                let got = got(&parts);
                (got != expected).then(|| format!("{case}: {got:?}, expected {expected:?}"))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    fn assert_converts(cases: Vec<(&str, Parts, &str, Vec<Reported>)>) {
        let cases = cases.into_iter().map(|(case, parts, markdown, mut reported)| {
            reported.sort();
            (case, parts, (markdown.to_string(), reported))
        });
        assert_cases(cases.collect(), converted);
    }

    fn assert_reports(cases: Vec<(&str, Parts, Vec<Reported>)>) {
        let cases = cases.into_iter().map(|(case, parts, mut reported)| {
            reported.sort();
            (case, parts, reported)
        });
        assert_cases(cases.collect(), |parts| converted(parts).1);
    }

    fn assert_markdown(cases: Vec<(&str, Parts, &str)>) {
        let cases =
            cases.into_iter().map(|(case, parts, markdown)| (case, parts, markdown.to_string()));
        assert_cases(cases.collect(), |parts| converted(parts).0);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of hiding properties and what they drop")]
    fn hidden_text_resolves_like_bold_and_drops() {
        use DiagnosticCode::HiddenOmitted;
        use Region::Body;
        let both = format!("{HIDING_DEFAULTS}{HIDING_STYLES}");
        let then_a = |first: String| format!("{first}{}", p("", &shown("a")));
        let cases = vec![
            (
                "direct",
                styled(&p("", &format!("{}{}{}", shown("a"), hidden("b"), shown("c"))), ""),
                "ac\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            ("direct off", styled(&p("", &r(SHOW, "a")), ""), "a\n", vec![]),
            (
                "a malformed operand, on as for bold",
                styled(
                    &p(
                        "",
                        &format!(
                            "{}{}",
                            shown("a"),
                            r(r#"<w:rPr><w:vanish w:val="maybe"/></w:rPr>"#, "b")
                        ),
                    ),
                    "",
                ),
                "a\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "web hidden, unread",
                styled(&p("", &r("<w:rPr><w:webHidden/></w:rPr>", "a")), ""),
                "a\n",
                vec![],
            ),
            (
                "a format change's old properties",
                styled(
                    &p(
                        "",
                        &r(
                            concat!(
                                r#"<w:rPr><w:rPrChange w:id="1"><w:rPr><w:vanish/></w:rPr>"#,
                                "</w:rPrChange></w:rPr>",
                            ),
                            "a",
                        ),
                    ),
                    "",
                ),
                "a\n",
                vec![],
            ),
            (
                "a character style",
                styled(
                    &p("", &format!("{}{}", shown("a"), char_style("HideChar", "b"))),
                    HIDING_STYLES,
                ),
                "a\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "a paragraph style, whose mark joins",
                styled(&then_a(in_style("HidePara", &shown("b"))), HIDING_STYLES),
                "a\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "paragraph and character styles flip twice",
                styled(&then_a(in_style("HidePara", &char_style("HideChar", "b"))), HIDING_STYLES),
                "ba\n",
                vec![],
            ),
            (
                "a chain flips twice",
                styled(&then_a(in_style("HideTwice", &shown("b"))), HIDING_STYLES),
                "b\n\na\n",
                vec![],
            ),
            (
                "a style's false adds nothing",
                styled(&then_a(in_style("FalseAdds", &shown("b"))), HIDING_STYLES),
                "a\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "direct off over a hiding style",
                styled(&then_a(in_style("HidePara", &r(SHOW, "b"))), HIDING_STYLES),
                "ba\n",
                vec![],
            ),
            (
                "document defaults",
                styled(&format!("{}{}", p("", &shown("b")), p("", &r(SHOW, "a"))), HIDING_DEFAULTS),
                "a\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "a style flips the defaults",
                styled(&in_style("HidePara", &shown("b")), &both),
                "b\n",
                vec![],
            ),
            (
                "math in a hidden paragraph",
                styled(&then_a(in_style("HidePara", MATH)), HIDING_STYLES),
                "a\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "math whose runs are hidden",
                styled(&p("", &format!("{}{HIDDEN_MATH}", shown("x"))), ""),
                "x\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "a fraction whose runs are hidden",
                styled(&p("", &format!("{}{HIDDEN_FRACTION}", shown("x"))), ""),
                "x\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "math shown in a hidden paragraph",
                styled(&then_a(in_style("HidePara", SHOWN_MATH)), HIDING_STYLES),
                "$y$a\n",
                vec![],
            ),
            (
                "math with one shown run",
                styled(&p("", PARTLY_HIDDEN_MATH), ""),
                "$y$\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "a hidden run inside math",
                styled(&p("", &math(&[(false, "x"), (true, "h"), (false, "y")])), ""),
                "$xy$\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "two hidden stretches inside math",
                styled(
                    &p(
                        "",
                        &math(&[
                            (false, "x"),
                            (true, "h"),
                            (false, "y"),
                            (true, "i"),
                            (false, "z"),
                        ]),
                    ),
                    "",
                ),
                "$xyz$\n",
                vec![(HiddenOmitted, Body, 2)],
            ),
            (
                "hidden runs that meet hidden text on either side",
                styled(
                    &p(
                        "",
                        &format!(
                            "{}{}{}{}",
                            hidden("a"),
                            math(&[(true, "h"), (false, "y"), (true, "i")]),
                            hidden("b"),
                            shown("c")
                        ),
                    ),
                    "",
                ),
                "$y$c\n",
                vec![(HiddenOmitted, Body, 2)],
            ),
            (
                "a hidden space inside math",
                styled(&p("", &math(&[(false, "x"), (true, " "), (false, "y")])), ""),
                "$xy$\n",
                vec![],
            ),
            (
                "a linear fraction beside a hidden run",
                styled(&p("", PARTLY_HIDDEN_FRACTION), ""),
                "${a}/{b}$\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "display math with a hidden run",
                styled(&p("", &display(&math(&[(true, "x"), (false, "y")]))), ""),
                "$$\ny\n$$\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "hidden text from the default paragraph style",
                styled(
                    &format!("{}{}", p("", &shown("b")), p("", &r(SHOW, "a"))),
                    HIDING_DEFAULT_STYLE,
                ),
                "a\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "a hidden mark from the default paragraph style",
                styled(
                    &format!("{}{}", p("", &r(SHOW, "one")), p("", &r(SHOW, "two"))),
                    HIDING_DEFAULT_STYLE,
                ),
                "onetwo\n",
                vec![],
            ),
            (
                "the default paragraph style beside a style that does not exist",
                styled(
                    &format!("{}{}", in_style("Missing", &shown("b")), p("", &r(SHOW, "a"))),
                    HIDING_DEFAULT_STYLE,
                ),
                "a\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "a named style over a hiding default paragraph style",
                styled(&in_style("Plain", &shown("b")), HIDING_DEFAULT_STYLE),
                "b\n",
                vec![],
            ),
            (
                "runless math in a hidden paragraph",
                styled(&then_a(in_style("HidePara", RUNLESS_MATH)), HIDING_STYLES),
                "a\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            ("runless math", styled(&p("", RUNLESS_MATH), ""), "$\\left(\\right)$\n", vec![]),
            (
                "display math under the body with a hidden run",
                styled(&then_a(BLOCK_MATH.into()), ""),
                "$$\ny\n$$\n\na\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "an equation under the body, all hidden",
                styled(&then_a(HIDDEN_MATH.into()), ""),
                "a\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "an equation under the body, under a hiding default paragraph style",
                styled(&format!("{MATH}{}", p("", &r(SHOW, "a"))), HIDING_DEFAULT_STYLE),
                "a\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "an equation in a cell, all hidden",
                styled(
                    &table(&format!("<w:tr><w:tc>{HIDDEN_MATH}{TEXT_PARAGRAPH}</w:tc></w:tr>")),
                    "",
                ),
                "|  |\n| --- |\n| x |\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
        ];
        assert_converts(cases);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of revised math runs")]
    fn math_drops_its_deleted_and_moved_runs_like_text() {
        let moved_away =
            |content: String| format!(r#"<w:moveFrom w:id="2">{content}</w:moveFrom>"#);
        let hidden_math_run = |text: &str| {
            format!(
                r#"<m:r><w:rPr><w:vanish/></w:rPr><m:t xml:space="preserve">{text}</m:t></m:r>"#
            )
        };
        let cases = vec![
            (
                "a deleted run",
                one(&equation(&format!("{}{}", math_run("x"), deletion(&math_run("d"))))),
                "$x$\n",
                revise(1),
            ),
            (
                "deleted runs either side of a shown one",
                one(&equation(&format!(
                    "{}{}{}",
                    deletion(&math_run("d")),
                    math_run("x"),
                    deletion(&math_run("e"))
                ))),
                "$x$\n",
                revise(2),
            ),
            (
                "a deleted run meeting a deletion before the equation",
                one(&format!(
                    "{}{}",
                    deleted("a"),
                    equation(&format!("{}{}", deletion(&math_run("d")), math_run("x")))
                )),
                "$x$\n",
                revise(1),
            ),
            (
                "a hidden run inside a deletion counts as deleted",
                one(&equation(&format!("{}{}", math_run("x"), deletion(&hidden_math_run("d"))))),
                "$x$\n",
                revise(1),
            ),
            (
                "an equation whose runs are all deleted",
                one(&format!("{}{}", shown("a"), equation(&deletion(&math_run("d"))))),
                "a\n",
                revise(1),
            ),
            (
                "a move away with no destination",
                one(&equation(&format!(
                    "{}{RANGE_START}{}{RANGE_END}",
                    math_run("x"),
                    moved_away(math_run("d"))
                ))),
                "$x$\n",
                revise(1),
            ),
            (
                "a move away with its destination",
                two(
                    &equation(&format!(
                        "{}{RANGE_START}{}{RANGE_END}",
                        math_run("x"),
                        moved_away(math_run("d"))
                    )),
                    &move_destination("m", "d"),
                ),
                "$x$\n\nd\n",
                vec![],
            ),
            (
                "a range opened inside math",
                two(
                    &format!(
                        "{}{}{RANGE_END}",
                        equation(&format!("{}{RANGE_START}", math_run("x"))),
                        move_out("d")
                    ),
                    &move_destination("m", "d"),
                ),
                "$x$\n\nd\n",
                vec![],
            ),
            (
                "a destination inside math",
                two(
                    &move_source("m", "d"),
                    &equation(&format!(
                        r#"{DESTINATION_START}<w:moveTo w:id="4">{}</w:moveTo>"#,
                        math_run("d")
                    )),
                ),
                "$d$\n",
                vec![],
            ),
            (
                "a destination inside deleted math",
                two(
                    &move_source("m", "d"),
                    &format!(
                        "{}{}",
                        shown("a"),
                        deletion(&equation(&format!(
                            r#"{DESTINATION_START}<w:moveTo w:id="4">{}</w:moveTo>"#,
                            math_run("d")
                        )))
                    ),
                ),
                "a\n",
                revise(2),
            ),
            (
                "math inside a deletion whose runs are empty",
                one(&format!("{}{}", shown("a"), deletion(&equation(&math_run(" "))))),
                "a\n",
                vec![],
            ),
            (
                "runless math inside a deletion",
                one(&format!("{}{}", shown("a"), deletion(RUNLESS_MATH))),
                "a\n",
                revise(1),
            ),
            (
                "an empty equation inside a deletion",
                one(&format!("{}{}", shown("a"), deletion(&equation("")))),
                "a\n",
                vec![],
            ),
            (
                "an equation of hidden empty runs",
                one(&format!("{}{}", shown("a"), math(&[(true, " "), (true, "")]))),
                "a\n",
                vec![],
            ),
            (
                "empty alternate content in a deleted run",
                one(&format!("{}{}", shown("a"), deleted_run(EMPTY_ALTERNATE))),
                "a\n",
                vec![],
            ),
            (
                "empty alternate content in a deletion",
                one(&format!("{}{}", shown("a"), deletion(EMPTY_ALTERNATE))),
                "a\n",
                vec![],
            ),
            (
                "empty alternate content in a hidden run",
                one(&format!("{}{}", shown("a"), hidden_run(EMPTY_ALTERNATE))),
                "a\n",
                vec![],
            ),
            (
                "alternate content holding deleted text",
                one(&format!(
                    "{}{}",
                    shown("a"),
                    deleted_run(&in_alternate("<w:delText>d</w:delText>"))
                )),
                "a\n",
                revise(1),
            ),
            (
                "the declined choice of deleted alternate content",
                one(&format!("{}{}", shown("a"), deleted_run(DECLINED_TEXT))),
                "a\n",
                vec![],
            ),
            (
                "alternate content holding hidden text",
                one(&format!("{}{}", shown("a"), hidden_run(&in_alternate("<w:t>h</w:t>")))),
                "a\n",
                hide(1),
            ),
            (
                "empty alternate content in a deleted row",
                styled(&table(&format!("{EMPTY_ALTERNATE_ROW}<w:tr>{TEXT_CELL}</w:tr>")), ""),
                "|  |\n| --- |\n| x |\n",
                vec![],
            ),
        ];
        assert_converts(cases);
    }

    const DECLINED_MATH_RUN: &str = concat!(
        r#"<mc:AlternateContent><mc:Choice Requires="cx"><m:r><m:t>c</m:t></m:r></mc:Choice>"#,
        "<mc:Fallback><m:r><m:t>f</m:t></m:r></mc:Fallback></mc:AlternateContent>",
    );
    const DECLINED_HIDDEN_MATH_RUN: &str = concat!(
        r#"<mc:AlternateContent><mc:Choice Requires="cx">"#,
        "<m:r><w:rPr><w:vanish/></w:rPr><m:t>h</m:t></m:r></mc:Choice>",
        "<mc:Fallback/></mc:AlternateContent>",
    );
    const TAKEN_HIDDEN_MATH_RUN: &str = concat!(
        r#"<mc:AlternateContent><mc:Choice Requires="cx"><m:r><m:t>c</m:t></m:r></mc:Choice>"#,
        "<mc:Fallback><m:r><w:rPr><w:vanish/></w:rPr><m:t>h</m:t></m:r></mc:Fallback>",
        "</mc:AlternateContent>",
    );
    const DECLINED_DESTINATION: &str = concat!(
        r#"<mc:AlternateContent><mc:Choice Requires="cx">"#,
        r#"<w:moveToRangeStart w:id="3" w:name="m"/></mc:Choice>"#,
        "<mc:Fallback/></mc:AlternateContent>",
    );
    const EMPTY_FRACTION: &str = "<m:f><m:num><m:r><m:t/></m:r></m:num><m:den/></m:f>";

    fn display(content: &str) -> String {
        format!("<m:oMathPara>{content}</m:oMathPara>")
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of math moves by where they render")]
    fn math_counts_and_tracks_moves_only_where_it_renders() {
        use DiagnosticCode::HiddenOmitted;
        use Region::Body;
        let between = |math: &str| format!("{}{math}{}{}", hidden("a"), hidden("b"), shown("x"));
        let cases = vec![
            (
                "an equation of only empty shown runs does not split",
                one(&between(&math(&[(false, "")]))),
                "x\n",
                hide(1),
            ),
            ("an empty equation does not split", one(&between(&equation(""))), "x\n", hide(1)),
            (
                "an empty display equation does not split",
                one(&between(&display("<m:oMath/>"))),
                "x\n",
                hide(1),
            ),
            (
                "a fraction of empty runs splits",
                one(&between(&equation(EMPTY_FRACTION))),
                "$\\frac{}{}$x\n",
                hide(2),
            ),
            (
                "only the taken branch of alternate content renders",
                one(&equation(DECLINED_MATH_RUN)),
                "$f$\n",
                vec![],
            ),
            (
                "a hidden run in a declined choice counts nothing",
                one(&equation(&format!("{}{DECLINED_HIDDEN_MATH_RUN}", math_run("x")))),
                "$x$\n",
                vec![],
            ),
            (
                "a hidden run in the taken branch drops, leaving the declined text unread",
                one(&equation(&format!("{}{TAKEN_HIDDEN_MATH_RUN}", math_run("x")))),
                "$x$\n",
                vec![(DiagnosticCode::BlockUnread, Body, 1), (HiddenOmitted, Body, 1)],
            ),
            (
                "a destination in a declined choice",
                two(
                    &move_source("m", "moved"),
                    &equation(&format!("{}{DECLINED_DESTINATION}", math_run("x"))),
                ),
                "$x$\n",
                revise(1),
            ),
            (
                "a hidden run of display math in a field instruction",
                one(&in_instruction(BLOCK_MATH)),
                "$$\ny\n$$\n\nresult\n",
                hide(1),
            ),
            (
                "a hidden run of inline math in a field instruction",
                one(&in_instruction(&math(&[(true, "h"), (false, "y")]))),
                "result\n",
                vec![],
            ),
            (
                "runless display math deleted in a field instruction",
                one(&in_instruction(&deletion(&display(RUNLESS_MATH)))),
                "result\n",
                revise(1),
            ),
            (
                "a destination in display math in a field instruction",
                two(
                    &move_source("m", "moved"),
                    &in_instruction(&display(&equation(&format!(
                        r#"{DESTINATION_START}<w:moveTo w:id="4">{}</w:moveTo>"#,
                        math_run("moved")
                    )))),
                ),
                "$$\nmoved\n$$\n\nresult\n",
                vec![],
            ),
        ];
        assert_converts(cases);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of dropped ranges and their counts")]
    fn dropped_content_counts_once_per_range_that_renders() {
        use DiagnosticCode::{HiddenOmitted, RevisionOmitted};
        use Region::Body;
        let ref_field = |inside: &str| {
            format!("{}{}{inside}{}", field("", "begin"), instr("", "REF here"), field("", "end"))
        };
        let between_shown =
            |inside: &str| one(&format!("{}{}{}", shown("a"), ref_field(inside), shown("x")));
        let between_hidden = |inside: &str| {
            one(&format!("{}{}{}{}", hidden("a"), ref_field(inside), hidden("b"), shown("x")))
        };
        let moved = |text: &str| format!(r#"<w:moveFrom w:id="8">{}</w:moveFrom>"#, shown(text));
        let deleted_row = |paragraph: &str| {
            table(&format!("<w:tr>{ROW_DEL}<w:tc>{paragraph}</w:tc></w:tr><w:tr>{CELL}</w:tr>"))
        };
        let deleted_cell = |paragraph: &str| {
            let cell = format!("<w:tc><w:tcPr><w:cellDel/></w:tcPr>{paragraph}</w:tc>");
            table(&format!("<w:tr>{cell}{CELL}</w:tr>"))
        };
        let cases = vec![
            (
                "adjacent runs",
                one(&format!("{}{}{}", hidden("a"), hidden("b"), shown("c"))),
                "c\n",
                hide(1),
            ),
            (
                "split by shown text",
                one(&format!("{}{}{}", hidden("a"), shown("x"), hidden("b"))),
                "x\n",
                hide(2),
            ),
            (
                "split by shown whitespace",
                one(&format!(
                    "{}{}{}{}{}",
                    shown("x"),
                    hidden("a"),
                    shown(" "),
                    hidden("b"),
                    shown("y")
                )),
                "x y\n",
                hide(2),
            ),
            (
                "whitespace only",
                one(&format!("{}{}", shown("a"), hidden(" \u{a0}\u{200b} "))),
                "a\n",
                vec![],
            ),
            (
                "a space a symbol font draws",
                one(&format!("{}{}", shown("a"), symbol_run(HIDE_RPR, "w:t", "\u{a0}"))),
                "a\n",
                hide(1),
            ),
            (
                "a soft hyphen a symbol font draws",
                one(&format!("{}{}", shown("a"), symbol_run(HIDE_RPR, "w:t", "\u{ad}"))),
                "a\n",
                hide(1),
            ),
            (
                "a deleted space a symbol font draws",
                one(&format!("{}{}", shown("a"), deletion(&symbol_run("", "w:delText", "\u{a0}")))),
                "a\n",
                revise(1),
            ),
            (
                "text after a move out inside a deletion",
                one(&format!(
                    r#"{}<w:del w:id="1">{}{}</w:del>"#,
                    shown("a"),
                    moved("m"),
                    shown("b")
                )),
                "a\n",
                revise(1),
            ),
            (
                "a symbol font's space",
                one(&format!("{}{}", shown("a"), symbol_run(HIDE_RPR, "w:t", "\u{f020}"))),
                "a\n",
                vec![],
            ),
            (
                "a checkbox a symbol font draws",
                one(&format!("{}{HIDDEN_CHECKBOX}", shown("a"))),
                "a\n",
                hide(1),
            ),
            (
                "a deleted run whose character style cannot resolve, by its direct fonts",
                styled(&p("", &format!("{}{CYCLIC_DELETED_RUN}", shown("a"))), CYCLIC_STYLES),
                "a\n",
                revise(1),
            ),
            (
                "partly hidden math in a field instruction",
                one(&fielded(&math(&[(true, "h"), (false, "y")]), "r")),
                "r\n",
                vec![],
            ),
            (
                "a tab and a break",
                one(&format!("{}{}", shown("a"), hidden_run("<w:tab/><w:br/>"))),
                "a\n",
                vec![],
            ),
            ("an empty run", one(&format!("{}<w:r>{HIDE}</w:r>", shown("a"))), "a\n", vec![]),
            (
                "one range per paragraph",
                two(
                    &format!("{}{}", hidden("a"), shown("x")),
                    &format!("{}{}", hidden("b"), shown("y")),
                ),
                "x\n\ny\n",
                hide(2),
            ),
            (
                "a joining mark still closes its range",
                styled(
                    &format!(
                        "{}{}",
                        p(&mark("<w:vanish/>"), &hidden("a")),
                        p("", &format!("{}{}", hidden("b"), shown("y")))
                    ),
                    "",
                ),
                "y\n",
                hide(2),
            ),
            (
                "a bookmark inside",
                one(&format!(
                    r#"{}<w:bookmarkStart w:id="1" w:name="here"/><w:bookmarkEnd w:id="1"/>{}{}"#,
                    hidden("a"),
                    hidden("b"),
                    shown("x")
                )),
                "x\n",
                hide(1),
            ),
            (
                "field delimiters inside",
                one(&format!(
                    "{}{}{}{}{}{}{}{}",
                    hidden("a"),
                    field("", "begin"),
                    instr("", "REF here"),
                    field("", "separate"),
                    hidden("b"),
                    field("", "end"),
                    hidden("c"),
                    shown("x")
                )),
                "x\n",
                hide(1),
            ),
            (
                "text in a field instruction renders nothing either way",
                one(&format!(
                    "{}{}{}{}{}{}",
                    field("", "begin"),
                    instr(HIDE, r#"HYPERLINK "https://www.example.org/" "#),
                    hidden_run("<w:t>code</w:t>"),
                    field("", "separate"),
                    shown("site"),
                    field("", "end")
                )),
                "[site](https://www.example.org/)\n",
                vec![],
            ),
            (
                "a hidden begin keeps its field",
                one(&format!(
                    "{}{}{}{}{}{}",
                    field(HIDE, "begin"),
                    instr("", r#"HYPERLINK "https://www.example.org/" "#),
                    field("", "separate"),
                    shown("site"),
                    field("", "end"),
                    shown("x")
                )),
                "[site](https://www.example.org/)x\n",
                vec![],
            ),
            (
                "a hidden field drops whole",
                one(&format!(
                    "{}{}{}{}{}{}",
                    field(HIDE, "begin"),
                    instr(HIDE, "REF here"),
                    field(HIDE, "separate"),
                    hidden("result"),
                    field(HIDE, "end"),
                    shown("after")
                )),
                "after\n",
                hide(1),
            ),
            (
                "a deleted field drops whole",
                one(&format!(
                    r#"<w:del w:id="1">{}{}{}<w:r><w:delText>result</w:delText></w:r>{}</w:del>{}"#,
                    field("", "begin"),
                    "<w:r><w:delInstrText>REF here</w:delInstrText></w:r>",
                    field("", "separate"),
                    field("", "end"),
                    shown("after")
                )),
                "after\n",
                revise(1),
            ),
            (
                "kinds apart",
                one(&format!("{}{}{}", hidden("a"), deleted("b"), shown("x"))),
                "x\n",
                vec![(HiddenOmitted, Body, 1), (RevisionOmitted, Body, 1)],
            ),
            (
                "deleted wins over hidden",
                one(&format!(
                    r#"<w:del w:id="1"><w:r>{HIDE}<w:delText>a</w:delText></w:r></w:del>{}"#,
                    shown("x")
                )),
                "x\n",
                revise(1),
            ),
            (
                "a deletion beside a move out of no range",
                one(&format!("{}{}{}", deleted("a"), moved("b"), shown("x"))),
                "x\n",
                revise(1),
            ),
            (
                "deletions split by shown text",
                one(&format!("{}{}{}", deleted("a"), shown("x"), deleted("b"))),
                "x\n",
                revise(2),
            ),
            (
                "deleted whitespace",
                one(&format!("{}{}", deleted(" \t "), shown("x"))),
                "x\n",
                vec![],
            ),
            (
                "a picture",
                one(&format!("{}{}", shown("x"), hidden_run("<w:drawing/>"))),
                "x\n",
                hide(1),
            ),
            (
                "a note reference",
                one(&format!("{}{}", shown("x"), hidden_run(r#"<w:footnoteReference w:id="1"/>"#))),
                "x\n",
                hide(1),
            ),
            (
                "a checkbox",
                one(&format!(
                    "{}{}",
                    shown("x"),
                    hidden_run(r#"<w:sym w:font="Wingdings" w:char="F0FE"/>"#)
                )),
                "x\n",
                hide(1),
            ),
            (
                "a no-break hyphen",
                one(&format!("{}{}", shown("x"), hidden_run("<w:noBreakHyphen/>"))),
                "x\n",
                hide(1),
            ),
            ("deleted math", one(&format!("{DELETED_MATH}{}", shown("x"))), "x\n", revise(1)),
            (
                "a deleted subdocument",
                one(&format!("{}{}", deletion(r#"<w:subDoc r:id="rId1"/>"#), shown("x"))),
                "x\n",
                revise(1),
            ),
            (
                "a deleted branch",
                one(&format!(
                    "{}{}",
                    deletion(&alternate(
                        "",
                        &format!("<mc:Fallback>{}</mc:Fallback>", shown("words"))
                    )),
                    shown("x")
                )),
                "x\n",
                revise(1),
            ),
            (
                "a deleted branch, whose taken branch is empty",
                one(&format!(
                    "{}{}",
                    deletion(&alternate(&shown("words"), "<mc:Fallback/>")),
                    shown("x")
                )),
                "x\n",
                vec![],
            ),
            (
                "split by a text box",
                one(&format!(
                    "{}<w:r><w:drawing>{}</w:drawing></w:r>{}",
                    hidden("a"),
                    text_box("box"),
                    hidden("b")
                )),
                "box\n",
                hide(2),
            ),
            (
                "deleted math in a field instruction",
                one(&format!(
                    "{}{}{DELETED_MATH}{}{}{}",
                    field("", "begin"),
                    instr("", r#"HYPERLINK "https://www.example.org/" "#),
                    field("", "separate"),
                    shown("site"),
                    field("", "end")
                )),
                "[site](https://www.example.org/)\n",
                vec![],
            ),
            (
                "text in a field instruction does not split",
                between_hidden(&shown("code")),
                "x\n",
                hide(1),
            ),
            (
                "a text box in a field instruction still renders and splits",
                between_hidden(&format!("<w:r><w:drawing>{}</w:drawing></w:r>", text_box("box"))),
                "box\n\nx\n",
                hide(2),
            ),
            (
                "a hidden text box in a field instruction",
                between_shown(&hidden_run(&format!("<w:drawing>{}</w:drawing>", text_box("box")))),
                "ax\n",
                hide(1),
            ),
            (
                "a deleted text box in a field instruction",
                between_shown(&deleted_run(&format!("<w:drawing>{}</w:drawing>", text_box("box")))),
                "ax\n",
                revise(1),
            ),
            (
                "a hidden chart in a field instruction",
                between_shown(&hidden_run(&format!("<w:drawing>{CHART}</w:drawing>"))),
                "ax\n",
                hide(1),
            ),
            (
                "a hidden diagram in a field instruction",
                between_shown(&hidden_run(r#"<w:drawing><dgm:relIds r:dm="rId5"/></w:drawing>"#)),
                "ax\n",
                hide(1),
            ),
            (
                "a hidden chart and diagram with no part in a field instruction",
                between_shown(&hidden_run("<w:drawing><c:chart/><dgm:relIds/></w:drawing>")),
                "ax\n",
                vec![],
            ),
            (
                "a hidden ruby holding a text box in a field instruction",
                between_shown(&hidden_run(&format!(
                    "<w:ruby><w:rubyBase>{}</w:rubyBase></w:ruby>",
                    run(&format!("<w:drawing>{}</w:drawing>", text_box("box")))
                ))),
                "ax\n",
                vec![],
            ),
            (
                "a hidden picture in a field instruction",
                between_shown(&hidden_run(&format!("<w:drawing>{PICTURE}</w:drawing>"))),
                "ax\n",
                vec![],
            ),
            (
                "a symbol alone",
                one(&format!("{}{}", shown("x"), deleted_run(SYMBOL))),
                "x\n",
                revise(1),
            ),
            (
                "a VML picture alone",
                one(&format!("{}{}", shown("x"), deleted_run("<w:pict/>"))),
                "x\n",
                revise(1),
            ),
            (
                "a date alone",
                one(&format!("{}{}", shown("x"), deleted_run("<w:dayLong/>"))),
                "x\n",
                revise(1),
            ),
            (
                "a deleted row's space its paragraph style's symbol font draws",
                styled(&deleted_row(SYMBOL_NBSP_PARAGRAPH), SYMBOL_STYLES),
                "",
                revise(1),
            ),
            (
                "a deleted cell's space the default paragraph style's symbol font draws",
                styled(&deleted_cell(NBSP_PARAGRAPH), SYMBOL_DEFAULT_STYLE),
                "",
                revise(1),
            ),
            (
                "a deleted cell's space the document defaults' symbol font draws",
                styled(&deleted_cell(NBSP_PARAGRAPH), SYMBOL_DEFAULTS),
                "",
                revise(1),
            ),
            (
                "a deleted row's space in a paragraph style that draws none",
                styled(&deleted_row(NBSP_PARAGRAPH), SYMBOL_STYLES),
                "",
                vec![],
            ),
            (
                "a deleted row whose paragraph style cannot resolve, by the outer fonts",
                styled(&deleted_row(CYCLIC_PARAGRAPH), CYCLIC_PARAGRAPH_STYLES),
                "",
                revise(1),
            ),
            ("display math in a deleted row", styled(&deleted_row(BLOCK_MATH), ""), "", revise(1)),
            (
                "a deleted cell at a row's end",
                styled(&table(&format!("<w:tr>{CELL}{DELETED_TEXT_CELL}</w:tr>")), ""),
                "",
                revise(1),
            ),
            (
                "inline math in a field instruction does not split",
                between_hidden(&math(&[(false, "m")])),
                "x\n",
                hide(1),
            ),
            (
                "display math in a field instruction still renders and splits",
                between_hidden(BLOCK_MATH),
                "$$\ny\n$$\n\nx\n",
                hide(2),
            ),
            (
                "runless math deleted in a field instruction",
                one(&format!(
                    "{}{}{}{}",
                    shown("x"),
                    field("", "begin"),
                    deletion(RUNLESS_MATH),
                    field("", "end")
                )),
                "x\n",
                vec![],
            ),
            (
                "a subdocument deleted in a field instruction",
                one(&format!(
                    "{}{}{}{}",
                    shown("x"),
                    field("", "begin"),
                    deletion(r#"<w:subDoc r:id="rId1"/>"#),
                    field("", "end")
                )),
                "x\n",
                vec![],
            ),
        ];
        assert_converts(cases);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of losses inside a dropped range")]
    fn nothing_inside_a_dropped_range_reports() {
        let hidden_in = |content: &str| styled(&p("", &hidden_run(content)), "");
        let cases: Vec<(&str, Parts, Vec<Reported>)> = vec![
            ("an unmapped symbol", hidden_in(r#"<w:sym w:font="Symbol" w:char="F060"/>"#), hide(1)),
            (
                "unmapped symbol-font text",
                styled(
                    &p(
                        "",
                        &r(
                            concat!(
                                "<w:rPr><w:vanish/>",
                                r#"<w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr>"#,
                            ),
                            "\u{F060}",
                        ),
                    ),
                    "",
                ),
                hide(1),
            ),
            (
                "ruby",
                hidden_in("<w:ruby><w:rubyBase><w:r><w:t>base</w:t></w:r></w:rubyBase></w:ruby>"),
                hide(1),
            ),
            ("ink", hidden_in(r#"<w:contentPart r:id="rId1"/>"#), hide(1)),
            ("a page number", hidden_in("<w:pgNum/>"), hide(1)),
            (
                "a form field",
                hidden_in(concat!(
                    r#"<w:fldChar w:fldCharType="begin"><w:ffData><w:checkBox/></w:ffData>"#,
                    "</w:fldChar>",
                )),
                hide(1),
            ),
            (
                "a picture that is not there",
                hidden_in(&format!("<w:drawing>{PICTURE}</w:drawing>")),
                hide(1),
            ),
            ("an embedded object", hidden_in(&format!("<w:object>{OLE}</w:object>")), hide(1)),
            ("a chart", hidden_in(&format!("<w:drawing>{CHART}</w:drawing>")), hide(1)),
            (
                "a text box's own losses",
                hidden_in(&format!(
                    "<w:drawing><w:txbxContent>{}</w:txbxContent></w:drawing>",
                    run("<w:ruby/>")
                )),
                hide(1),
            ),
            (
                "a declined branch, beside an empty one taken",
                hidden_in(&alternate("<w:t>words</w:t>", "<mc:Fallback/>")),
                vec![],
            ),
            (
                "a link of an unknown id",
                styled(
                    &p(
                        "",
                        &format!(
                            r#"<w:hyperlink r:id="rId9">{}</w:hyperlink>"#,
                            hidden("see here")
                        ),
                    ),
                    "",
                ),
                hide(1),
            ),
            ("a comment", hidden_in(r#"<w:commentReference w:id="1"/>"#), vec![]),
            (
                "a deleted comment",
                styled(
                    &p("", r#"<w:del w:id="1"><w:r><w:commentReference w:id="1"/></w:r></w:del>"#),
                    "",
                ),
                vec![],
            ),
            (
                "a deleted symbol",
                styled(&p("", &deleted_run(r#"<w:sym w:font="Symbol" w:char="F060"/>"#)), ""),
                revise(1),
            ),
            (
                "a note's own losses",
                vec![
                    (
                        "word/document.xml",
                        document(&p("", &hidden_run(r#"<w:footnoteReference w:id="1"/>"#))),
                    ),
                    ("word/footnotes.xml", footnotes(&run("<w:ruby/>"))),
                ],
                hide(1),
            ),
        ];
        assert_reports(cases);
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the table of declined choices and what they would show"
    )]
    fn a_declined_choice_reports_only_text_that_would_show() {
        use DiagnosticCode::{BlockUnread, GraphicUnread, HiddenOmitted};
        use Region::Body;
        let unread = || vec![(BlockUnread, Body, 1)];
        let declined = |choice: &str| alternate(choice, "");
        let drawn = |graphic: &str| format!("<w:drawing>{graphic}</w:drawing>");
        let chart = format!("<w:r>{}</w:r>", drawn(CHART));
        let boxed = |paragraph: &str| format!("<w:txbxContent>{paragraph}</w:txbxContent>");
        let hiding_box = boxed(&in_style("HidePara", &shown("secret")));
        let cases = vec![
            ("hidden text", styled(&p("", &declined(&hidden("secret"))), ""), "", vec![]),
            ("shown text", styled(&p("", &declined(&shown("secret"))), ""), "", unread()),
            (
                "text in a hiding paragraph",
                styled(&in_style("HidePara", &declined(&shown("secret"))), HIDING_STYLES),
                "",
                vec![],
            ),
            (
                "text shown over a hiding paragraph",
                styled(&in_style("HidePara", &declined(&r(SHOW, "secret"))), HIDING_STYLES),
                "",
                unread(),
            ),
            (
                "text in a hiding character style",
                styled(&p("", &declined(&char_style("HideChar", "secret"))), HIDING_STYLES),
                "",
                vec![],
            ),
            (
                "a hiding character style over a hiding paragraph",
                styled(
                    &in_style("HidePara", &declined(&char_style("HideChar", "secret"))),
                    HIDING_STYLES,
                ),
                "",
                unread(),
            ),
            (
                "a cyclic character style",
                styled(&p("", &declined(&char_style("A", "secret"))), CYCLIC_STYLES),
                "",
                unread(),
            ),
            (
                "a hiding paragraph at block level",
                styled(&declined(&in_style("HidePara", &shown("secret"))), HIDING_STYLES),
                "",
                vec![],
            ),
            (
                "a paragraph in a hiding default style at block level",
                styled(&declined(&p("", &shown("secret"))), HIDING_DEFAULT_STYLE),
                "",
                vec![],
            ),
            (
                "a shown paragraph at block level",
                styled(&declined(&p("", &shown("secret"))), ""),
                "",
                unread(),
            ),
            (
                "a paragraph in a cyclic style at block level",
                styled(&declined(CYCLIC_PARAGRAPH), CYCLIC_PARAGRAPH_STYLES),
                "",
                unread(),
            ),
            (
                "a hiding text box in a run",
                styled(
                    &run(&declined(&format!("<w:drawing>{hiding_box}</w:drawing>"))),
                    HIDING_STYLES,
                ),
                "",
                vec![],
            ),
            (
                "a hiding text box in a drawing",
                styled(&drawing(&declined(&hiding_box)), HIDING_STYLES),
                "",
                vec![],
            ),
            (
                "a shown text box in a drawing",
                styled(&drawing(&declined(&boxed(&p("", &shown("secret"))))), ""),
                "",
                unread(),
            ),
            (
                "a text box in a cyclic style in a drawing",
                styled(&drawing(&declined(&boxed(CYCLIC_PARAGRAPH))), CYCLIC_PARAGRAPH_STYLES),
                "",
                unread(),
            ),
            (
                "a text box in a cyclic style in a run",
                styled(
                    &run(&declined(&format!("<w:drawing>{}</w:drawing>", boxed(CYCLIC_PARAGRAPH)))),
                    CYCLIC_PARAGRAPH_STYLES,
                ),
                "",
                unread(),
            ),
            (
                "hidden text the fallback holds",
                styled(
                    &p(
                        "",
                        &alternate(
                            &shown("secret"),
                            &format!("<mc:Fallback>{}</mc:Fallback>", hidden("secret")),
                        ),
                    ),
                    "",
                ),
                "",
                vec![(BlockUnread, Body, 1), (HiddenOmitted, Body, 1)],
            ),
            ("moved-away text", styled(&p("", &declined(&move_out("secret"))), ""), "", vec![]),
            (
                "deleted text",
                styled(&p("", &declined(&deleted_run("<w:t>secret</w:t>"))), ""),
                "",
                vec![],
            ),
            (
                "a deleted paragraph at block level",
                styled(&declined(&deletion(TEXT_PARAGRAPH)), ""),
                "",
                vec![],
            ),
            (
                "a deleted chart",
                styled(&p("", &declined(&deleted_run(&drawn(CHART)))), ""),
                "",
                vec![],
            ),
            (
                "a deleted picture",
                styled(&p("", &declined(&deleted_run(&drawn(PICTURE)))), ""),
                "",
                vec![],
            ),
            (
                "a chart moved away",
                styled(&p("", &declined(&format!("<w:moveFrom>{chart}</w:moveFrom>"))), ""),
                "",
                vec![],
            ),
            (
                "inserted text",
                styled(&p("", &declined(&format!("<w:ins>{}</w:ins>", shown("secret")))), ""),
                "",
                unread(),
            ),
            ("a chart", styled(&p("", &declined(&chart)), ""), "", vec![(GraphicUnread, Body, 1)]),
        ];
        assert_converts(cases);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of declined choices in equations")]
    fn math_reports_what_a_declined_choice_would_show() {
        use DiagnosticCode::{BlockUnread, HiddenOmitted, RevisionOmitted};
        use Region::Body;
        let after_x = |choice: &str| equation(&format!("{}{}", math_run("x"), declined(choice)));
        let math_text = |rpr: &str| format!("<m:r>{rpr}<m:t>secret</m:t></m:r>");
        let hiding_run = math_text(r#"<w:rPr><w:rStyle w:val="HideChar"/></w:rPr>"#);
        let unread = || vec![(BlockUnread, Body, 1)];
        let cases = vec![
            ("shown math text", one(&after_x(&math_text(""))), "$x$\n", unread()),
            (
                "shown math text in display math",
                one(&display(&after_x(&math_text("")))),
                "$$\nx\n$$\n",
                unread(),
            ),
            (
                "an equation of only a declined choice",
                one(&equation(&declined(&math_text("")))),
                "",
                unread(),
            ),
            (
                "math text the fallback shows too",
                one(&equation(&alternate(&math_text(""), FALLBACK_MATH_RUN))),
                "$f$\n",
                vec![],
            ),
            ("hidden math text", one(&after_x(&math_text(HIDE))), "$x$\n", vec![]),
            (
                "math text in a hiding character style",
                styled(&p("", &after_x(&hiding_run)), HIDING_STYLES),
                "$x$\n",
                vec![],
            ),
            (
                "math text shown over a hiding paragraph, in an equation that drops",
                styled(&in_style("HidePara", &after_x(&math_text(SHOW))), HIDING_STYLES),
                "",
                vec![(HiddenOmitted, Body, 1)],
            ),
            ("deleted math text", one(&after_x(&deletion(&math_text("")))), "$x$\n", vec![]),
            (
                "math text moved away",
                one(&after_x(&format!(r#"<w:moveFrom w:id="1">{}</w:moveFrom>"#, math_text("")))),
                "$x$\n",
                vec![],
            ),
            (
                "a deleted equation",
                one(&deletion(&after_x(&math_text("")))),
                "",
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "an equation whose runs are hidden",
                one(&equation(&format!(
                    "<m:r>{HIDE}<m:t>x</m:t></m:r>{}",
                    declined(&math_text(""))
                ))),
                "",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "inline math in a field instruction",
                one(&in_instruction(&after_x(&math_text("")))),
                "result\n",
                vec![],
            ),
            (
                "math text beside word text counts once",
                one(&after_x(&format!("{}{}", math_text(""), shown("word")))),
                "$x$\n",
                unread(),
            ),
            (
                "math text in a hiding paragraph, in an equation that renders",
                styled(
                    &in_style(
                        "HidePara",
                        &equation(&format!(
                            "<m:r>{SHOW}<m:t>x</m:t></m:r>{}",
                            declined(&math_text(""))
                        )),
                    ),
                    HIDING_STYLES,
                ),
                "$x$\n",
                vec![],
            ),
            (
                "a declined choice deleted",
                one(&equation(&format!(
                    "{}{}",
                    math_run("x"),
                    deletion(&declined(&math_text("")))
                ))),
                "$x$\n",
                vec![],
            ),
            (
                "a declined choice moved away",
                one(&equation(&format!(
                    r#"{}<w:moveFrom w:id="1">{}</w:moveFrom>"#,
                    math_run("x"),
                    declined(&math_text(""))
                ))),
                "$x$\n",
                vec![],
            ),
            (
                "a declined choice inside a taken fallback",
                one(&equation(&alternate(
                    &math_text(""),
                    &format!("<mc:Fallback>{}</mc:Fallback>", declined(&math_text(""))),
                ))),
                "",
                unread(),
            ),
        ];
        assert_converts(cases);
    }

    const FALLBACK_MATH_RUN: &str = "<mc:Fallback><m:r><m:t>f</m:t></m:r></mc:Fallback>";

    #[test]
    fn a_dropped_range_counts_where_it_sits() {
        use DiagnosticCode::HiddenOmitted;
        use Region::{Body, Note};
        let hidden_note = vec![
            ("word/document.xml", document(&run(r#"<w:footnoteReference w:id="1"/>"#))),
            ("word/footnotes.xml", footnotes(&p("", &hidden("secret")))),
        ];
        let cell = styled(
            &format!("<w:tbl><w:tr><w:tc>{}</w:tc></w:tr></w:tbl>", p("", &hidden("secret"))),
            "",
        );
        let text_box = styled(
            &run(&format!(
                "<w:drawing><w:txbxContent>{}</w:txbxContent></w:drawing>",
                p("", &hidden("secret"))
            )),
            "",
        );
        assert_reports(vec![
            ("a note", hidden_note, vec![(HiddenOmitted, Note, 1)]),
            ("a cell", cell, vec![(HiddenOmitted, Body, 1)]),
            ("a text box", text_box, vec![(HiddenOmitted, Body, 1)]),
        ]);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of moves by source and destination")]
    fn a_move_counts_only_where_no_walked_destination_names_its_range() {
        use DiagnosticCode::{BlockUnread, RevisionOmitted};
        use Region::Body;
        let referenced = |source: String, note: String| {
            vec![
                (
                    "word/document.xml",
                    document(&p(
                        "",
                        &format!(r#"{source}<w:r><w:footnoteReference w:id="1"/></w:r>"#),
                    )),
                ),
                ("word/footnotes.xml", footnotes(&p("", &note))),
            ]
        };
        let in_order = |rows: &str| {
            styled(&format!("{}{}", table(rows), p("", &move_destination("m", "x"))), "")
        };
        let cell_of = |runs: &str| format!("<w:tc>{}</w:tc>", p("", runs));
        let deleted_cell_of =
            |runs: &str| format!("<w:tc><w:tcPr><w:cellDel/></w:tcPr>{}</w:tc>", p("", runs));
        let deleted_cell_marking =
            |marker: &str| format!("<w:tc><w:tcPr><w:cellDel/></w:tcPr>{marker}<w:p/></w:tc>");
        let merge_start = |runs: &str| {
            format!(r#"<w:tc><w:tcPr><w:hMerge w:val="restart"/></w:tcPr>{}</w:tc>"#, p("", runs))
        };
        let merged =
            |runs: &str| format!("<w:tc><w:tcPr><w:hMerge/></w:tcPr>{}</w:tc>", p("", runs));
        let cases = vec![
            (
                "a pair",
                two(&move_source("m", "moved"), &move_destination("m", "moved")),
                "moved\n",
                vec![],
            ),
            (
                "a destination in a field instruction",
                two(&move_source("m", "moved"), &fielded(DESTINATION_START, "result")),
                "result\n",
                revise(1),
            ),
            (
                "a destination in a link in a field instruction",
                two(
                    &move_source("m", "moved"),
                    &fielded(&format!("<w:hyperlink>{DESTINATION_START}</w:hyperlink>"), "result"),
                ),
                "result\n",
                revise(1),
            ),
            (
                "a destination in math in a field instruction",
                two(&move_source("m", "moved"), &fielded(&equation(DESTINATION_START), "result")),
                "result\n",
                revise(1),
            ),
            (
                "the destination first",
                two(&move_destination("m", "moved"), &move_source("m", "moved")),
                "moved\n",
                vec![],
            ),
            (
                "a destination of another name",
                two(&move_source("m", "moved"), &move_destination("n", "moved")),
                "moved\n",
                revise(1),
            ),
            (
                "a source in no range",
                two(
                    r#"<w:moveFrom w:id="2"><w:r><w:t>moved</w:t></w:r></w:moveFrom>"#,
                    &move_destination("m", "moved"),
                ),
                "moved\n",
                revise(1),
            ),
            (
                "a source after its range closed",
                two(
                    concat!(
                        r#"<w:moveFromRangeStart w:id="1" w:name="m"/>"#,
                        r#"<w:moveFromRangeEnd w:id="1"/>"#,
                        r#"<w:moveFrom w:id="2"><w:r><w:t>moved</w:t></w:r></w:moveFrom>"#,
                    ),
                    &move_destination("m", "moved"),
                ),
                "moved\n",
                revise(1),
            ),
            (
                "a range with no name",
                two(
                    concat!(
                        r#"<w:moveFromRangeStart w:id="1"/>"#,
                        r#"<w:moveFrom w:id="2"><w:r><w:t>moved</w:t></w:r></w:moveFrom>"#,
                        r#"<w:moveFromRangeEnd w:id="1"/>"#,
                    ),
                    &move_destination("m", "moved"),
                ),
                "moved\n",
                revise(1),
            ),
            ("whitespace moved alone", two(&move_source("m", "  "), &shown("x")), "x\n", vec![]),
            (
                "a deletion inside a named range",
                two(
                    concat!(
                        r#"<w:moveFromRangeStart w:id="1" w:name="m"/>"#,
                        r#"<w:del w:id="5"><w:r><w:delText>gone</w:delText></w:r></w:del>"#,
                        r#"<w:moveFromRangeEnd w:id="1"/>"#,
                    ),
                    &move_destination("m", "moved"),
                ),
                "moved\n",
                revise(1),
            ),
            (
                "a move out inside a deletion inside a named range",
                two(
                    concat!(
                        r#"<w:moveFromRangeStart w:id="1" w:name="m"/>"#,
                        r#"<w:del w:id="5"><w:moveFrom w:id="2">"#,
                        r#"<w:r><w:t>gone</w:t></w:r></w:moveFrom></w:del>"#,
                        r#"<w:moveFromRangeEnd w:id="1"/>"#,
                    ),
                    &move_destination("m", "moved"),
                ),
                "moved\n",
                revise(1),
            ),
            (
                "a destination range at block level",
                styled(
                    &format!(
                        r#"{}<w:moveToRangeStart w:id="3" w:name="m"/>{}{}"#,
                        p("", &move_source("m", "moved")),
                        p("", &format!(r#"<w:moveTo w:id="4">{}</w:moveTo>"#, shown("moved"))),
                        r#"<w:moveToRangeEnd w:id="3"/>"#,
                    ),
                    "",
                ),
                "moved\n",
                vec![],
            ),
            (
                "ranges and a mark at block level",
                styled(
                    &format!(
                        concat!(
                            r#"<w:moveFromRangeStart w:id="1" w:name="m"/>{}"#,
                            r#"<w:moveFromRangeEnd w:id="1"/>{}{}"#,
                        ),
                        p(
                            &mark(r#"<w:moveFrom w:id="5"/>"#),
                            &format!(r#"<w:moveFrom w:id="2">{}</w:moveFrom>"#, shown("moved ")),
                        ),
                        p("", &shown("next")),
                        p("", &move_destination("m", "moved "))
                    ),
                    "",
                ),
                "next\n\nmoved\n",
                vec![],
            ),
            (
                "a destination in a declined branch",
                styled(
                    &format!(
                        "{}{}",
                        p("", &move_source("m", "moved")),
                        alternate(&p("", &move_destination("m", "moved")), "<mc:Fallback/>"),
                    ),
                    "",
                ),
                "",
                vec![(RevisionOmitted, Body, 1), (BlockUnread, Body, 1)],
            ),
            (
                "a destination in the branch taken",
                styled(
                    &format!(
                        "{}{}",
                        p("", &move_source("m", "moved")),
                        alternate(
                            &p("", &move_destination("m", "moved")),
                            &format!(
                                "<mc:Fallback>{}</mc:Fallback>",
                                p("", &move_destination("m", "moved")),
                            ),
                        )
                    ),
                    "",
                ),
                "moved\n",
                vec![],
            ),
            (
                "a destination in a deleted row",
                styled(
                    &format!(
                        "{}<w:tbl><w:tr>{ROW_DEL}<w:tc>{}</w:tc></w:tr></w:tbl>",
                        p("", &move_source("m", "moved")),
                        p("", &move_destination("m", "moved"))
                    ),
                    "",
                ),
                "",
                revise(2),
            ),
            (
                "a destination inside a deletion",
                two(
                    &move_source("m", "moved"),
                    &format!(r#"<w:del w:id="6">{}</w:del>"#, move_destination("m", "moved")),
                ),
                "",
                revise(2),
            ),
            (
                "a duplicated range id, which ends the latest range",
                two(DUPLICATED_RANGE_IDS, &move_destination("b", "x")),
                "x\n",
                revise(1),
            ),
            (
                "an unclosed range, to the part's end",
                styled(
                    &format!(
                        "{}{}{}",
                        p("", UNCLOSED_RANGE),
                        p("", &format!("{}{}", move_out("two"), shown("y"))),
                        p("", &move_destination("m", "x"))
                    ),
                    "",
                ),
                "y\n\nx\n",
                vec![],
            ),
            (
                "an unclosed range, not into a note",
                referenced(
                    format!("{UNCLOSED_RANGE}{}", move_destination("m", "x")),
                    format!("{}{}", move_out("two"), move_destination("m", "y")),
                ),
                "x[^1]\n\n[^1]: y\n",
                vec![(RevisionOmitted, Region::Note, 1)],
            ),
            (
                "a destination in another part",
                referenced(move_source("m", "moved"), move_destination("m", "moved")),
                "[^1]\n\n[^1]: moved\n",
                revise(1),
            ),
            (
                "a source in a note",
                referenced(String::new(), format!("{}{}", move_source("m", "moved"), shown("x"))),
                "[^1]\n\n[^1]: x\n",
                vec![(RevisionOmitted, Region::Note, 1)],
            ),
            (
                "a range a deleted row opens after the source",
                in_order(&format!(
                    "<w:tr>{}</w:tr><w:tr>{ROW_DEL}{}</w:tr>",
                    cell_of(&format!("{}{}", shown("a"), move_out("x"))),
                    cell_of(RANGE_START),
                )),
                "|  |\n| --- |\n| a |\n\nx\n",
                revise(1),
            ),
            (
                "a range a deleted cell opens after the source",
                in_order(&format!(
                    "<w:tr>{}{}</w:tr>",
                    cell_of(&format!("{}{}", shown("a"), move_out("x"))),
                    deleted_cell_of(RANGE_START),
                )),
                "|  |\n| --- |\n| a |\n\nx\n",
                revise(1),
            ),
            (
                "a range a deleted row closes before the source",
                in_order(&format!(
                    "<w:tr>{}</w:tr><w:tr>{ROW_DEL}{}</w:tr><w:tr>{}</w:tr>",
                    cell_of(&format!("{}{RANGE_START}", shown("a"))),
                    cell_of(RANGE_END),
                    cell_of(&format!("{}{}", shown("b"), move_out("x"))),
                )),
                "| a |\n| --- |\n| b |\n\nx\n",
                revise(1),
            ),
            (
                "a range a deleted row opens before the source",
                in_order(&format!(
                    "<w:tr>{ROW_DEL}{}</w:tr><w:tr>{}</w:tr>",
                    cell_of(RANGE_START),
                    cell_of(&format!("{}{}", shown("a"), move_out("x"))),
                )),
                "|  |\n| --- |\n| a |\n\nx\n",
                vec![],
            ),
            (
                "a range a deleted cell opens before the source",
                in_order(&format!(
                    "<w:tr>{}{}</w:tr>",
                    deleted_cell_of(RANGE_START),
                    cell_of(&format!("{}{}", shown("a"), move_out("x"))),
                )),
                "|  |\n| --- |\n| a |\n\nx\n",
                vec![],
            ),
            (
                "a range a deleted row opens outside its paragraphs",
                in_order(&format!(
                    "<w:tr>{ROW_DEL}{RANGE_START}<w:tc><w:p/></w:tc></w:tr><w:tr>{}</w:tr>",
                    cell_of(&format!("{}{}", shown("a"), move_out("x"))),
                )),
                "|  |\n| --- |\n| a |\n\nx\n",
                vec![],
            ),
            (
                "a range a deleted cell closes outside its paragraphs",
                in_order(&format!(
                    "<w:tr>{}{}{}</w:tr>",
                    cell_of(&format!("{}{RANGE_START}", shown("a"))),
                    deleted_cell_marking(RANGE_END),
                    cell_of(&format!("{}{}", shown("b"), move_out("x"))),
                )),
                "|  |  |\n| --- | --- |\n| a | b |\n\nx\n",
                revise(1),
            ),
            (
                "a destination outside the paragraphs of a deleted cell",
                styled(
                    &table(&format!(
                        "<w:tr>{}{}</w:tr>",
                        cell_of(&format!(
                            "{}{RANGE_START}{}{RANGE_END}",
                            shown("a"),
                            move_out("x")
                        )),
                        deleted_cell_marking(DESTINATION_START),
                    )),
                    "",
                ),
                "|  |\n| --- |\n| a |\n",
                revise(1),
            ),
            (
                "a range a deleted cell opens before a merged continuation",
                in_order(&format!(
                    "<w:tr>{}{}{}</w:tr>",
                    merge_start(&shown("a")),
                    deleted_cell_of(RANGE_START),
                    merged(&format!("{}{}", shown("b"), move_out("x"))),
                )),
                "|  |  |\n| --- | --- |\n| a<br>b |  |\n\nx\n",
                vec![],
            ),
            (
                "a range a deleted cell closes before a merged continuation",
                in_order(&format!(
                    "<w:tr>{}{}{}</w:tr>",
                    merge_start(&format!("{}{RANGE_START}", shown("a"))),
                    deleted_cell_of(RANGE_END),
                    merged(&format!("{}{}", shown("b"), move_out("x"))),
                )),
                "|  |  |\n| --- | --- |\n| a<br>b |  |\n\nx\n",
                revise(1),
            ),
            (
                "a range a deleted cell closes after a merged continuation",
                in_order(&format!(
                    "<w:tr>{}{}{}</w:tr>",
                    merge_start(&format!("{}{RANGE_START}", shown("a"))),
                    merged(&format!("{}{}", shown("b"), move_out("x"))),
                    deleted_cell_of(RANGE_END),
                )),
                "|  |  |\n| --- | --- |\n| a<br>b |  |\n\nx\n",
                vec![],
            ),
            (
                "a range a deleted cell opens after a merged continuation",
                in_order(&format!(
                    "<w:tr>{}{}{}{}</w:tr>",
                    merge_start(&shown("a")),
                    merged(&shown("b")),
                    deleted_cell_of(RANGE_START),
                    cell_of(&format!("{}{}", shown("c"), move_out("x"))),
                )),
                "|  |  |  |\n| --- | --- | --- |\n| a<br>b |  | c |\n\nx\n",
                vec![],
            ),
            (
                "a deleted cell after a merged continuation",
                in_order(&format!(
                    "<w:tr>{}{}{}</w:tr>",
                    merge_start(&shown("a")),
                    merged(&shown("b")),
                    deleted_cell_of(&shown("gone")),
                )),
                "|  |  |\n| --- | --- |\n| a<br>b |  |\n\nx\n",
                revise(1),
            ),
            (
                "a range opened between rows before the source",
                in_order(&format!(
                    "{RANGE_START}<w:tr>{}</w:tr>",
                    cell_of(&format!("{}{}", shown("a"), move_out("x"))),
                )),
                "|  |\n| --- |\n| a |\n\nx\n",
                vec![],
            ),
            (
                "a range opened between rows after the source",
                in_order(&format!(
                    "<w:tr>{}</w:tr>{RANGE_START}",
                    cell_of(&format!("{}{}", shown("a"), move_out("x"))),
                )),
                "|  |\n| --- |\n| a |\n\nx\n",
                revise(1),
            ),
            (
                "a range closed between rows before the source",
                in_order(&format!(
                    "<w:tr>{}</w:tr>{RANGE_END}<w:tr>{}</w:tr>",
                    cell_of(&format!("{}{RANGE_START}", shown("a"))),
                    cell_of(&format!("{}{}", shown("b"), move_out("x"))),
                )),
                "| a |\n| --- |\n| b |\n\nx\n",
                revise(1),
            ),
            (
                "a destination between rows",
                styled(
                    &table(&format!(
                        "<w:tr>{}</w:tr>{DESTINATION_START}",
                        cell_of(&format!(
                            "{}{RANGE_START}{}{RANGE_END}",
                            shown("a"),
                            move_out("x")
                        )),
                    )),
                    "",
                ),
                "|  |\n| --- |\n| a |\n",
                vec![],
            ),
            (
                "a range opened between cells before the source",
                in_order(&format!(
                    "<w:tr>{RANGE_START}{}</w:tr>",
                    cell_of(&format!("{}{}", shown("a"), move_out("x"))),
                )),
                "|  |\n| --- |\n| a |\n\nx\n",
                vec![],
            ),
            (
                "a range opened between cells after the source",
                in_order(&format!(
                    "<w:tr>{}{RANGE_START}{}</w:tr>",
                    cell_of(&format!("{}{}", shown("a"), move_out("x"))),
                    cell_of(&shown("b")),
                )),
                "|  |  |\n| --- | --- |\n| a | b |\n\nx\n",
                revise(1),
            ),
            (
                "a range closed between cells before the source",
                in_order(&format!(
                    "<w:tr>{}{RANGE_END}{}</w:tr>",
                    cell_of(&format!("{}{RANGE_START}", shown("a"))),
                    cell_of(&format!("{}{}", shown("b"), move_out("x"))),
                )),
                "|  |  |\n| --- | --- |\n| a | b |\n\nx\n",
                revise(1),
            ),
            (
                "a range opened between cells before a merged continuation",
                in_order(&format!(
                    "<w:tr>{}{RANGE_START}{}</w:tr>",
                    merge_start(&shown("a")),
                    merged(&format!("{}{}", shown("b"), move_out("x"))),
                )),
                "|  |  |\n| --- | --- |\n| a<br>b |  |\n\nx\n",
                vec![],
            ),
            (
                "a destination between cells",
                styled(
                    &table(&format!(
                        "<w:tr>{}{DESTINATION_START}</w:tr>",
                        cell_of(&format!(
                            "{}{RANGE_START}{}{RANGE_END}",
                            shown("a"),
                            move_out("x")
                        )),
                    )),
                    "",
                ),
                "|  |\n| --- |\n| a |\n",
                vec![],
            ),
        ];
        assert_converts(cases);
        let in_note = referenced(
            String::new(),
            format!("{}{}", move_source("m", "moved"), move_destination("m", "moved")),
        );
        assert_eq!(converted(&in_note).1, vec![], "a pair in a note");
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of block-level revisions")]
    fn a_block_level_revision_drops_and_counts_by_range() {
        let block_deletion = |text: &str| {
            deleted_run(&format!(r#"<w:delText xml:space="preserve">{text}</w:delText>"#))
        };
        let cases = vec![
            (
                "text",
                styled(&format!("{}{}", block_deletion("gone"), p("", &shown("x"))), ""),
                "x\n",
                revise(1),
            ),
            (
                "whitespace",
                styled(&format!("{}{}", block_deletion("  "), p("", &shown("x"))), ""),
                "x\n",
                vec![],
            ),
            (
                "a space the document defaults' symbol font draws",
                styled(&format!("{}<w:p/>", block_deletion("\u{a0}")), SYMBOL_DEFAULTS),
                "",
                revise(1),
            ),
            (
                "two elements",
                styled(
                    &format!(
                        "{}{}{}",
                        block_deletion("a"),
                        block_deletion("b"),
                        p("", &shown("x"))
                    ),
                    "",
                ),
                "x\n",
                revise(2),
            ),
            (
                "a move out of no range",
                styled(
                    &format!(
                        r#"<w:moveFrom w:id="1"><w:r><w:t>moved</w:t></w:r></w:moveFrom>{}"#,
                        p("", &shown("x"))
                    ),
                    "",
                ),
                "x\n",
                revise(1),
            ),
            (
                "a paired move",
                styled(
                    &format!(
                        "{}{}",
                        move_source("m", "moved"),
                        p("", &move_destination("m", "moved"))
                    ),
                    "",
                ),
                "moved\n",
                vec![],
            ),
            (
                "a paired move with both halves at body level",
                styled(
                    &format!(
                        r#"{}{DESTINATION_START}<w:moveTo w:id="4">{}</w:moveTo>{}"#,
                        move_source("m", "moved"),
                        p("", &shown("moved")),
                        r#"<w:moveToRangeEnd w:id="3"/>"#
                    ),
                    "",
                ),
                "moved\n",
                vec![],
            ),
            (
                "an insertion at body level",
                styled(
                    &format!(
                        r#"<w:ins w:id="5">{}</w:ins>{}"#,
                        p("", &shown("new")),
                        p("", &shown("x"))
                    ),
                    "",
                ),
                "new\n\nx\n",
                vec![],
            ),
            (
                "an insertion in a cell",
                styled(
                    &table(&format!(
                        r#"<w:tr><w:tc><w:ins w:id="5">{}</w:ins></w:tc></w:tr>"#,
                        p("", &shown("new"))
                    )),
                    "",
                ),
                "|  |\n| --- |\n| new |\n",
                vec![],
            ),
            (
                "a held mark waits across one",
                styled(
                    &format!(
                        "{}{}{}",
                        p(&mark(r#"<w:del w:id="2"/>"#), &shown("a")),
                        block_deletion("gone"),
                        p("", &shown("b"))
                    ),
                    "",
                ),
                "ab\n",
                revise(1),
            ),
        ];
        assert_converts(cases);
    }

    fn dropped_mark_numbering() -> String {
        format!(
            "<w:numbering {NS}>{}</w:numbering>",
            concat!(
                r#"<w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0">"#,
                r#"<w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum>"#,
                r#"<w:num w:numId="7"><w:abstractNumId w:val="1"/></w:num>"#,
                r#"<w:abstractNum w:abstractNumId="2"><w:lvl w:ilvl="0">"#,
                r#"<w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/><w:pStyle w:val="H1"/>"#,
                "</w:lvl></w:abstractNum>",
                r#"<w:num w:numId="8"><w:abstractNumId w:val="2"/></w:num>"#,
            ),
        )
    }

    fn dropped_mark_styles() -> String {
        format!(
            "{HIDING_STYLES}{}",
            concat!(
                r#"<w:style w:type="paragraph" w:styleId="H1"><w:name w:val="heading 1"/>"#,
                r#"<w:pPr><w:numPr><w:numId w:val="8"/></w:numPr></w:pPr></w:style>"#,
            ),
        )
    }

    fn numbered(rpr: &str, runs: &str) -> String {
        p(
            &format!(
                r#"<w:pPr><w:numPr><w:numId w:val="7"/></w:numPr><w:rPr>{rpr}</w:rPr></w:pPr>"#
            ),
            runs,
        )
    }

    fn numbered_item(rpr: &str, text: &str) -> String {
        numbered(rpr, &shown(text))
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of dropped marks by container")]
    fn a_dropped_mark_joins_in_its_container_and_keeps_its_kind_until_then() {
        let deleted_mark = r#"<w:del w:id="1"/>"#;
        let numbering = dropped_mark_numbering();
        let styles = dropped_mark_styles();
        let item = numbered_item;
        let heading = |rpr: &str, text: &str| {
            p(
                &format!(r#"<w:pPr><w:pStyle w:val="H1"/><w:rPr>{rpr}</w:rPr></w:pPr>"#),
                &shown(text),
            )
        };
        let table =
            "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl>";
        let math = "<m:oMathPara><m:oMath><m:r><m:t>y</m:t></m:r></m:oMath></m:oMathPara>";
        let two = |mark_rpr: &str| {
            format!("{}{}", p(&mark(mark_rpr), &shown("one ")), p("", &shown("two")))
        };
        let cases: Vec<(&str, String, &str)> = vec![
            ("a hidden mark", two("<w:vanish/>"), "one two\n"),
            (
                "a mark hidden by its character style",
                two(r#"<w:rStyle w:val="HideChar"/>"#),
                "one two\n",
            ),
            ("a special hidden mark", two("<w:specVanish/>"), "one two\n"),
            ("a moved-out mark", two(r#"<w:moveFrom w:id="1"/>"#), "one two\n"),
            (
                "a special hidden mark turned off",
                two(r#"<w:specVanish w:val="0"/>"#),
                "one\n\ntwo\n",
            ),
            (
                "a hidden mark shown directly",
                two(r#"<w:rStyle w:val="HideChar"/><w:vanish w:val="0"/>"#),
                "one\n\ntwo\n",
            ),
            (
                "a mark's format change",
                two(concat!(
                    r#"<w:rPrChange w:id="1"><w:rPr><w:del w:id="2"/><w:vanish/></w:rPr>"#,
                    "</w:rPrChange>",
                )),
                "one\n\ntwo\n",
            ),
            (
                "a section's last paragraph",
                format!(
                    "{}{}",
                    p(
                        &format!("<w:pPr><w:rPr>{deleted_mark}</w:rPr><w:sectPr/></w:pPr>"),
                        &shown("one")
                    ),
                    p("", &shown("two"))
                ),
                "one\n\ntwo\n",
            ),
            (
                "numbered, at the container's end",
                format!("{}{}", item("", "one"), item(deleted_mark, "two")),
                "1. one\n2. two\n",
            ),
            (
                "numbered, before a table",
                format!("{}{table}", item(deleted_mark, "one")),
                "1. one\n\n|  |\n| --- |\n| cell |\n",
            ),
            (
                "numbered, before display math",
                format!("{}{math}{}", item(deleted_mark, "one"), p("", &shown("two"))),
                "1. one\n\n$$\ny\n$$\n\ntwo\n",
            ),
            (
                "the next paragraph's kind",
                format!("{}{}", heading(deleted_mark, "Intro "), p("", &shown("body"))),
                "Intro body\n",
            ),
            ("a numbered heading at the end", heading(deleted_mark, "Intro"), "# 1. Intro\n"),
            (
                "numbered and all dropped, before a table",
                format!(
                    "{}{}{table}{}",
                    item("", "one"),
                    numbered(deleted_mark, &deleted("gone")),
                    item("", "two")
                ),
                "1. one\n\n|  |\n| --- |\n| cell |\n\n2. two\n",
            ),
            (
                "numbered and all dropped, at the container's end",
                format!("{}{}", item("", "one"), numbered(deleted_mark, &deleted("gone"))),
                "1. one\n",
            ),
        ];
        let numbered_cases = cases.into_iter().map(|(case, body, expected)| {
            let mut parts = styled(&body, &styles);
            parts.push(("word/numbering.xml", numbering.clone()));
            (case, parts, expected)
        });
        assert_markdown(numbered_cases.collect());
    }

    #[test]
    fn a_dropped_mark_in_a_table_cell_joins_its_list_items_there() {
        let deleted_mark = r#"<w:del w:id="1"/>"#;
        let cell = format!(
            "<w:tbl><w:tr><w:tc>{}{}{}</w:tc></w:tr></w:tbl>{}",
            numbered_item(deleted_mark, "a "),
            numbered_item("", "b"),
            numbered_item(deleted_mark, "c"),
            p("", &shown("d"))
        );
        let mut parts = styled(&cell, &dropped_mark_styles());
        parts.push(("word/numbering.xml", dropped_mark_numbering()));
        let doc = parse(&ParseCtx::default(), &zip_package(&parts)).unwrap();
        let [Block::Table(table), Block::Paragraph(after)] = &doc.blocks[..] else {
            panic!("{:?}", doc.blocks)
        };
        assert_eq!(inlines_to_plain_text(after), "d");
        let CellSlot::Origin(cell) = &table.grid[0][0] else { panic!("{:?}", table.grid[0][0]) };
        let [Block::List(list)] = &cell.blocks[..] else { panic!("{:?}", cell.blocks) };
        let items: Vec<String> = list
            .items
            .iter()
            .map(|item| match &item.blocks[..] {
                [Block::Paragraph(inlines)] => inlines_to_plain_text(inlines),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(items, ["a b", "c"]);
        assert_eq!(list.start, 1);
    }

    #[test]
    fn a_held_plain_paragraph_whose_content_all_drops_renders_nothing() {
        let blocks = |body: &str| {
            parse(&ParseCtx::default(), &zip_package(&styled(body, ""))).unwrap().blocks
        };
        let all_hidden = p(&mark("<w:vanish/>"), &hidden("gone"));
        assert_matches!(
            blocks(&format!("{all_hidden}<w:tbl><w:tr>{TEXT_CELL}</w:tr></w:tbl>"))[..],
            [Block::Table(_)]
        );
        assert_matches!(blocks(&all_hidden)[..], []);
        let anchored = p(&mark("<w:vanish/>"), &format!("{BOOKMARK}{}", hidden("gone")));
        assert_matches!(blocks(&anchored)[..], [Block::Paragraph(_)]);
        let boxed = p(
            &mark("<w:vanish/>"),
            &format!("{}<w:r><w:drawing>{}</w:drawing></w:r>", hidden("gone"), text_box("box")),
        );
        assert_matches!(blocks(&boxed)[..], [Block::Paragraph(_)]);
    }

    #[test]
    fn a_join_never_leaves_a_note_or_a_text_box() {
        let deleted_mark = mark(r#"<w:del w:id="1"/>"#);
        let notes: Parts = vec![
            (
                "word/document.xml",
                document(&run(r#"<w:footnoteReference w:id="1"/><w:footnoteReference w:id="2"/>"#)),
            ),
            (
                "word/footnotes.xml",
                format!(
                    r#"<w:footnotes {NS}><w:footnote w:id="1">{}</w:footnote>{}</w:footnotes>"#,
                    p(&deleted_mark, &shown("one")),
                    format_args!(r#"<w:footnote w:id="2">{}</w:footnote>"#, p("", &shown("two"))),
                ),
            ),
        ];
        assert_eq!(converted(&notes).0, "[^1][^2]\n\n[^1]: one\n\n[^2]: two\n", "a note's end");
        let boxed = p(
            "",
            &format!(
                "<w:r><w:drawing><w:txbxContent>{}</w:txbxContent></w:drawing></w:r>{}",
                p(&deleted_mark, &shown("in box")),
                shown("after")
            ),
        );
        assert_eq!(converted(&styled(&boxed, "")).0, "in box\n\nafter\n", "a text box's end");
    }

    #[test]
    fn a_cyclic_character_style_on_a_mark_or_math_leaves_them_to_their_direct_properties() {
        let cyclic = format!("{HIDING_STYLES}{CYCLIC_STYLES}");
        let two = |rpr: &str| format!("{}{}", p(&mark(rpr), &shown("one ")), p("", &shown("two")));
        let in_hidden_paragraph = format!(
            "{}{}",
            p(
                r#"<w:pPr><w:pStyle w:val="HidePara"/><w:rPr><w:rStyle w:val="A"/></w:rPr></w:pPr>"#,
                &r(SHOW, "one ")
            ),
            p("", &shown("two"))
        );
        let cases = [
            ("a mark", two(r#"<w:rStyle w:val="A"/>"#), "one\n\ntwo\n"),
            ("a hidden mark", two(r#"<w:rStyle w:val="A"/><w:vanish/>"#), "one two\n"),
            ("a mark in a hidden paragraph", in_hidden_paragraph, "one two\n"),
            ("math", p("", &format!("{}{CYCLIC_MATH}", shown("x"))), "x$y$\n"),
            ("partly hidden math", p("", CYCLIC_PARTLY_HIDDEN_MATH), "$y$\n"),
            ("a deleted run", p("", &format!("{}{CYCLIC_DELETED_RUN}", shown("x"))), "x\n"),
        ];
        let cases = cases
            .into_iter()
            .map(|(case, body, expected)| (case, styled(&body, &cyclic), expected));
        assert_markdown(cases.collect());
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of note references and how they drop")]
    fn a_note_is_read_unless_every_reference_to_it_drops() {
        use DiagnosticCode::{
            DanglingReference, HiddenOmitted, PartUnreadable, RevisionOmitted, RubyUnread,
        };
        use Region::{Body, Note};
        let reference_alone = r#"<w:footnoteReference w:id="1"/>"#;
        let reference = r#"<w:r><w:footnoteReference w:id="1"/></w:r>"#;
        let hidden_reference = hidden_run(reference_alone);
        let note = footnotes(&run("<w:t>note</w:t><w:ruby/>"));
        let unclosed = note.trim_end_matches("</w:footnotes>").to_string();
        let both = format!("{hidden_reference}{}{reference}", shown("x"));
        let noted = |runs: String, notes: &str, rels: Option<&str>| {
            let mut parts = vec![
                ("word/document.xml", document(&p("", &runs))),
                ("word/footnotes.xml", notes.to_string()),
            ];
            parts.extend(rels.map(|rels| ("word/_rels/footnotes.xml.rels", rels.to_string())));
            parts
        };
        let beside_note = |body: String| {
            vec![("word/document.xml", document(&body)), ("word/footnotes.xml", note.clone())]
        };
        let with_note = |runs: String| beside_note(p("", &runs));
        let boxed = |content: &str| {
            format!("<w:drawing><w:txbxContent>{content}</w:txbxContent></w:drawing>")
        };
        let ruby = |guide: &str, base: &str| {
            format!("<w:ruby><w:rt>{guide}</w:rt><w:rubyBase>{base}</w:rubyBase></w:ruby>")
        };
        let cases = vec![
            (
                "hidden",
                with_note(format!("{}{hidden_reference}", shown("x"))),
                "x\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "deleted",
                with_note(format!(r#"{}<w:del w:id="2">{reference}</w:del>"#, shown("x"))),
                "x\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "one of two rendering",
                with_note(format!("{hidden_reference}{}{reference}", shown("x"))),
                "x[^1]\n\n[^1]: note\n",
                vec![(HiddenOmitted, Body, 1), (RubyUnread, Note, 1)],
            ),
            (
                "the rendering one first",
                with_note(format!("{reference}{}{hidden_reference}", shown("x"))),
                "[^1]x\n\n[^1]: note\n",
                vec![(HiddenOmitted, Body, 1), (RubyUnread, Note, 1)],
            ),
            (
                "none, as before",
                with_note(shown("x")),
                "x\n\n[^1]: note\n",
                vec![(RubyUnread, Note, 1)],
            ),
            (
                "none, beside a dropped reference to another note",
                with_note(shown("x") + &hidden_run(r#"<w:footnoteReference w:id="2"/>"#)),
                "x\n\n[^1]: note\n",
                vec![(HiddenOmitted, Body, 1), (RubyUnread, Note, 1)],
            ),
            (
                "in a deleted row",
                beside_note(format!(
                    "<w:tbl><w:tr>{ROW_DEL}<w:tc>{}</w:tc></w:tr><w:tr>{TEXT_CELL}</w:tr></w:tbl>",
                    p("", reference),
                )),
                "|  |\n| --- |\n| x |\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "in a hidden text box",
                with_note(format!("{}{}", shown("x"), hidden_run(&boxed(&p("", reference))))),
                "x\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "in a deleted text box",
                with_note(format!(
                    r#"{}<w:del w:id="2"><w:r>{}</w:r></w:del>"#,
                    shown("x"),
                    boxed(&p("", reference))
                )),
                "x\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "only in the declined branch of a hidden text box",
                with_note(format!(
                    "{}{}",
                    shown("x"),
                    hidden_run(&boxed(&run(DECLINED_REFERENCE)))
                )),
                "x\n\n[^1]: note\n",
                vec![(HiddenOmitted, Body, 1), (RubyUnread, Note, 1)],
            ),
            (
                "in a hidden ruby's base",
                with_note(format!("{}{}", shown("x"), hidden_run(&ruby(&shown("a"), reference)))),
                "x\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "in a deleted ruby's guide",
                with_note(format!(
                    r#"{}<w:del w:id="2"><w:r>{}</w:r></w:del>"#,
                    shown("x"),
                    ruby(reference, &shown("a"))
                )),
                "x\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "only in the declined branch of a hidden ruby",
                with_note(format!(
                    "{}{}",
                    shown("x"),
                    hidden_run(&ruby(&shown("a"), &run(DECLINED_REFERENCE)))
                )),
                "x\n\n[^1]: note\n",
                vec![(HiddenOmitted, Body, 1), (RubyUnread, Note, 1)],
            ),
            (
                "only in a hidden ruby in a field instruction",
                with_note(fielded(&hidden_run(&ruby(&shown("a"), reference)), "x")),
                "x\n",
                vec![],
            ),
            (
                "only in a hidden text box in a field instruction",
                with_note(fielded(&hidden_run(&boxed(&p("", reference))), "x")),
                "x\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "only in a deleted text box in a field instruction",
                with_note(fielded(
                    &format!(r#"<w:del w:id="2"><w:r>{}</w:r></w:del>"#, boxed(&p("", reference))),
                    "x",
                )),
                "x\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
            ("only in a field instruction", with_note(fielded(reference, "x")), "x\n", vec![]),
            (
                "only in a ruby's base in a field instruction",
                with_note(fielded(&bare(&ruby(&shown("a"), reference)), "x")),
                "x\n",
                vec![],
            ),
            (
                "only in a ruby's guide in a field instruction",
                with_note(fielded(&bare(&ruby(reference, &shown("a"))), "x")),
                "x\n",
                vec![],
            ),
            (
                "only in the declined branch of a ruby in a field instruction",
                with_note(fielded(&bare(&ruby(&shown("a"), &run(DECLINED_REFERENCE))), "x")),
                "x\n\n[^1]: note\n",
                vec![(RubyUnread, Note, 1)],
            ),
            (
                "in a ruby in a field result",
                with_note(format!(
                    "{}{}{}{}{}",
                    field("", "begin"),
                    field("", "separate"),
                    shown("x"),
                    bare(&ruby(&shown("a"), reference)),
                    field("", "end")
                )),
                "x\n\n[^1]: note\n",
                vec![(RubyUnread, Body, 1), (RubyUnread, Note, 1)],
            ),
            (
                "only hidden in a field instruction",
                with_note(fielded(&hidden_reference, "x")),
                "x\n",
                vec![],
            ),
            (
                "in a field instruction, beside one rendering",
                with_note(format!("{}{reference}", fielded(reference, "x"))),
                "x[^1]\n\n[^1]: note\n",
                vec![(RubyUnread, Note, 1)],
            ),
            (
                "skipped, beside unreadable relationships",
                noted(format!("{}{hidden_reference}", shown("x")), &note, Some(CORRUPT_RELS)),
                "x\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "read, beside unreadable relationships",
                noted(both.clone(), &note, Some(CORRUPT_RELS)),
                "x[^1]\n\n[^1]: note\n",
                vec![(HiddenOmitted, Body, 1), (PartUnreadable, Note, 1), (RubyUnread, Note, 1)],
            ),
            (
                "two read beside unreadable relationships",
                noted(format!("{both}{SECOND_REFERENCE}"), TWO_NOTES, Some(CORRUPT_RELS)),
                "x[^1][^2]\n\n[^1]: one\n\n[^2]: two\n",
                vec![(HiddenOmitted, Body, 1), (PartUnreadable, Note, 1)],
            ),
            (
                "none read, beside unreadable relationships",
                noted(
                    format!("{}{hidden_reference}{SECOND_REFERENCE}", shown("x")),
                    &note,
                    Some(CORRUPT_RELS),
                ),
                "x\n",
                vec![(HiddenOmitted, Body, 1), (DanglingReference, Body, 1)],
            ),
            (
                "skipped, in an unclosed part",
                noted(format!("{}{hidden_reference}", shown("x")), &unclosed, None),
                "x\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "read, in an unclosed part",
                noted(both.clone(), &unclosed, None),
                "x\n",
                vec![(HiddenOmitted, Body, 1), (PartUnreadable, Note, 1)],
            ),
            (
                "skipped, in a part that does not parse",
                noted(format!("{}{hidden_reference}", shown("x")), "</w:footnotes>", None),
                "x\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "read, in a part that does not parse",
                noted(both.clone(), "</w:footnotes>", None),
                "x\n",
                vec![(HiddenOmitted, Body, 1), (PartUnreadable, Note, 1)],
            ),
            (
                "read, in a part that does not parse, beside a dropped reference to another",
                noted(
                    format!("{both}{}", hidden_run(r#"<w:footnoteReference w:id="2"/>"#)),
                    "</w:footnotes>",
                    None,
                ),
                "x\n",
                vec![(HiddenOmitted, Body, 2), (PartUnreadable, Note, 1)],
            ),
            (
                "read, in a part rooted elsewhere",
                noted(both.clone(), &format!("<w:endnotes {NS}/>"), None),
                "x\n",
                vec![(HiddenOmitted, Body, 1), (PartUnreadable, Note, 1)],
            ),
            (
                "read, with no notes part",
                vec![("word/document.xml", document(&p("", &both)))],
                "x\n",
                vec![(HiddenOmitted, Body, 1), (DanglingReference, Body, 1)],
            ),
            (
                "an unclosed part with no note, nothing dropped",
                noted(shown("x"), NO_NOTE_UNCLOSED, None),
                "x\n",
                vec![(PartUnreadable, Note, 1)],
            ),
            (
                "an unclosed part with no note, beside a dropped reference",
                noted(format!("{}{hidden_reference}", shown("x")), NO_NOTE_UNCLOSED, None),
                "x\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "an unclosed part with no note, beside a rendered reference",
                noted(format!("{}{reference}", shown("x")), NO_NOTE_UNCLOSED, None),
                "x\n",
                vec![(PartUnreadable, Note, 1)],
            ),
            (
                "an unclosed part with no note, beside a rendered and a hidden one",
                noted(both.clone(), NO_NOTE_UNCLOSED, None),
                "x\n",
                vec![(HiddenOmitted, Body, 1), (PartUnreadable, Note, 1)],
            ),
            (
                "an unreadable endnotes part beside a read footnote",
                vec![
                    (
                        "word/document.xml",
                        document(&p(
                            "",
                            &format!(
                                "{reference}{}{}",
                                shown("x"),
                                hidden_run(r#"<w:endnoteReference w:id="1"/>"#)
                            ),
                        )),
                    ),
                    ("word/footnotes.xml", note.clone()),
                    ("word/endnotes.xml", "</w:endnotes>".to_string()),
                ],
                "[^1]x\n\n[^1]: note\n",
                vec![(HiddenOmitted, Body, 1), (RubyUnread, Note, 1)],
            ),
            (
                "an endnote's only reference in a hidden text box",
                vec![
                    (
                        "word/document.xml",
                        document(&p(
                            "",
                            &format!(
                                "{}{}",
                                shown("x"),
                                hidden_run(&boxed(&run(r#"<w:endnoteReference w:id="1"/>"#)))
                            ),
                        )),
                    ),
                    (
                        "word/endnotes.xml",
                        format!(
                            r#"<w:endnotes {NS}><w:endnote w:id="1">{}</w:endnote></w:endnotes>"#,
                            run("<w:t>note</w:t><w:ruby/>")
                        ),
                    ),
                ],
                "x\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "deleted, inside alternate content",
                with_note(format!("{}{}", shown("x"), deleted_run(&in_alternate(reference_alone)))),
                "x\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "hidden, inside alternate content",
                with_note(format!("{}{}", shown("x"), hidden_run(&in_alternate(reference_alone)))),
                "x\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "deleted, inside alternate content around the run",
                with_note(format!(
                    r#"{}<w:del w:id="2">{}</w:del>"#,
                    shown("x"),
                    in_alternate(reference)
                )),
                "x\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "in a declined choice of deleted alternate content, as with no reference",
                with_note(format!("{}{}", shown("x"), deleted_run(DECLINED_REFERENCE))),
                "x\n\n[^1]: note\n",
                vec![(RubyUnread, Note, 1)],
            ),
            (
                "in alternate content in a deleted row",
                beside_note(table(&format!(
                    "<w:tr>{ROW_DEL}<w:tc>{}<w:p/></w:tc></w:tr><w:tr>{TEXT_CELL}</w:tr>",
                    in_alternate(&p("", reference)),
                ))),
                "|  |\n| --- |\n| x |\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
        ];
        assert_converts(cases);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of links whose content drops")]
    fn a_link_whose_content_all_drops_renders_nothing() {
        use DiagnosticCode::RevisionOmitted;
        use Region::Body;
        let linked = |runs: String| {
            vec![
                ("word/document.xml", document(&p("", &runs))),
                ("word/_rels/document.xml.rels", LINK_RELS.to_string()),
            ]
        };
        let simple =
            |content: String| format!(r#"<w:fldSimple w:instr="REF here">{content}</w:fldSimple>"#);
        let cases = vec![
            ("hidden", linked(format!("{}{}", shown("x"), link(&hidden("label")))), "x\n", hide(1)),
            (
                "deleted",
                linked(format!("{}{}", shown("x"), link(&deleted("label")))),
                "x\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "deleted math",
                linked(format!("{}{}", shown("x"), link(DELETED_MATH))),
                "x\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "deleted runless math",
                linked(format!("{}{}", shown("x"), link(&deletion(RUNLESS_MATH)))),
                "x\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "a deleted subdocument",
                linked(format!("{}{}", shown("x"), link(&deletion(r#"<w:subDoc r:id="rId1"/>"#)))),
                "x\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "math whose only shown run is empty",
                linked(format!("{}{}", shown("x"), link(EMPTY_SHOWN_MATH))),
                "x\n",
                hide(1),
            ),
            (
                "a field in a link, all dropped",
                linked(format!("{}{}", shown("x"), link(&simple(hidden("w"))))),
                "x\n",
                hide(1),
            ),
            (
                "an empty run hidden",
                linked(format!("{}{}", shown("x"), link(&format!("<w:r>{HIDE}</w:r>")))),
                "x\n",
                vec![],
            ),
            (
                "an empty link shows its target between two ranges",
                linked(format!("{}{}{}", hidden("a"), link(""), hidden("b"))),
                "[https://www.example.org/](https://www.example.org/)\n",
                hide(2),
            ),
            (
                "an empty link in a deletion",
                linked(format!(r#"{}<w:del w:id="5">{}</w:del>"#, shown("x"), link(""))),
                "x\n",
                vec![],
            ),
            (
                "an empty link moved out",
                linked(format!(r#"{}<w:moveFrom w:id="5">{}</w:moveFrom>"#, shown("x"), link(""))),
                "x\n",
                vec![],
            ),
            (
                "an empty link of an unknown id in a deletion",
                linked(format!(
                    r#"{}<w:del w:id="5"><w:hyperlink r:id="rId9"/></w:del>"#,
                    shown("x")
                )),
                "x\n",
                vec![],
            ),
            (
                "a hyperlink field",
                linked(format!(
                    r#"{}<w:fldSimple w:instr=" HYPERLINK {} ">{}</w:fldSimple>"#,
                    shown("x"),
                    "&quot;https://www.example.org/&quot;",
                    hidden("label")
                )),
                "x\n",
                hide(1),
            ),
            (
                "a range across a link in a field result",
                linked(format!(
                    "{}{}{}{}",
                    shown("x"),
                    simple(link(&format!("{}{}", shown("v"), hidden("w")))),
                    hidden("z"),
                    shown("y")
                )),
                "x[v](https://www.example.org/)y\n",
                hide(1),
            ),
            (
                "a link in a field result, all dropped",
                linked(format!(
                    "{}{}{}{}",
                    hidden("a"),
                    simple(link(&hidden("w"))),
                    hidden("z"),
                    shown("y")
                )),
                "y\n",
                hide(1),
            ),
            (
                "a range across a link in a complex field's result",
                linked(format!(
                    "{}{}{}{}{}{}{}{}",
                    shown("x"),
                    field("", "begin"),
                    instr("", "REF here"),
                    field("", "separate"),
                    link(&format!("{}{}", shown("v"), hidden("w"))),
                    field("", "end"),
                    hidden("z"),
                    shown("y")
                )),
                "x[v](https://www.example.org/)y\n",
                hide(1),
            ),
        ];
        assert_converts(cases);
    }

    #[test]
    fn links_and_hyperlink_fields_place_what_they_hold() {
        use DiagnosticCode::{HiddenOmitted, LinkTargetUnread};
        use Region::Body;
        let unknown =
            |content: &str| format!(r#"<w:hyperlink r:id="rId9">{content}</w:hyperlink>"#);
        let boxed = format!("<w:r><w:drawing>{}</w:drawing></w:r>", text_box("box"));
        let anchor_after_hidden = format!(
            r#"{}<w:hyperlink w:anchor="here">{}</w:hyperlink>"#,
            unknown(&format!("{}{BOOKMARK}", hidden("gone"))),
            shown("go")
        );
        let field = format!(
            r#"<w:fldSimple w:instr=" HYPERLINK &quot;https://www.example.org/&quot; ">{}</w:fldSimple>"#,
            shown("label")
        );
        let unread = vec![(LinkTargetUnread, Body, 1)];
        let cases = vec![
            ("a target that is unknown", one(&unknown(&shown("see"))), "see\n", unread.clone()),
            ("a text box in it", one(&unknown(&boxed)), "box\n", unread),
            (
                "an anchor beside hidden text",
                one(&anchor_after_hidden),
                "<a id=\"here\"></a>[go](#here)\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            ("a hyperlink field", one(&field), "[label](https://www.example.org/)\n", vec![]),
        ];
        assert_converts(cases);
    }

    fn fuel(parts: &Parts) -> u64 {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, &zip_package(parts)).unwrap();
        parse_ctx.fuel()
    }

    #[test]
    fn move_names_and_note_ids_are_charged_by_length() {
        let moved = |name: &str| {
            styled(
                &format!(
                    "{}{}",
                    p("", &move_source(name, "moved")),
                    p("", &move_destination(name, "moved"))
                ),
                "",
            )
        };
        let noted = |id: &str| {
            vec![
                (
                    "word/document.xml",
                    document(&run(&format!(r#"<w:footnoteReference w:id="{id}"/>"#))),
                ),
                (
                    "word/footnotes.xml",
                    format!(
                        r#"<w:footnotes {NS}><w:footnote w:id="{id}">{}</w:footnote>{}"#,
                        run("<w:t>note</w:t>"),
                        "</w:footnotes>",
                    ),
                ),
            ]
        };
        let long = "m".repeat(5000);
        let short_moves = fuel(&moved("m"));
        assert!(
            fuel(&moved(&long)) >= short_moves + 3 * 4999,
            "a source's range, its destination and its lookup"
        );
        let digits = "1".repeat(5000);
        let short_notes = fuel(&noted("1"));
        assert!(
            fuel(&noted(&digits)) >= short_notes + 3 * 4999,
            "a rendered reference, the part's scan of references and its note's lookup"
        );
    }

    #[test]
    fn an_unmatched_move_end_is_charged_per_open_range() {
        let unmatched_ends = |count: usize| {
            let starts: String = (0..200)
                .map(|id| format!(r#"<w:moveFromRangeStart w:id="{id}" w:name="m"/>"#))
                .collect();
            let ends = r#"<w:moveFromRangeEnd w:id="x"/>"#.repeat(count);
            styled(&p("", &format!("{starts}{ends}")), "")
        };
        assert!(
            fuel(&unmatched_ends(200)) >= fuel(&unmatched_ends(0)) + 200 * 200 * 2,
            "each open range an unmatched end passes"
        );
    }

    #[test]
    fn a_dropped_drawing_is_charged_per_element_it_scans() {
        let hidden_holder = |name: &str| {
            let holder = format!("<w:{name}>{}</w:{name}>", "<w:p/>".repeat(1000));
            styled(&p("", &hidden_run(&holder)), "")
        };
        assert!(
            fuel(&hidden_holder("drawing")) >= fuel(&hidden_holder("unknown")) + 1000,
            "each element a dropped drawing's scan for note references passes"
        );
    }

    #[test]
    fn a_declined_choice_is_charged_per_element_judged_and_per_byte_copied() {
        let choice = |requires: &str, held: usize| {
            let unknowns = |count: usize| "<w:unknown/>".repeat(count);
            let dropped = hidden_run(&format!("<w:unknown>{}</w:unknown>", unknowns(1000 - held)));
            let choice = format!("<mc:Choice{requires}>{}</mc:Choice>", unknowns(held));
            let alternate = format!("<mc:AlternateContent>{choice}</mc:AlternateContent>");
            styled(&p("", &format!("{dropped}{alternate}")), "")
        };
        let declined = r#" Requires="cx""#;
        let (judged, unjudged) = (fuel(&choice(declined, 1000)), fuel(&choice(declined, 0)));
        assert!(
            judged >= unjudged + 1000,
            "each element of a declined choice judged on what would show: {judged} {unjudged}"
        );
        let (taken, untaken) = (fuel(&choice("", 1000)), fuel(&choice("", 0)));
        assert!(taken < untaken + 1000, "a taken choice is not judged: {taken} {untaken}");
        let (text, name) = ("t".repeat(50_000), "n".repeat(10_000));
        let copied_text = |in_choice: bool| {
            let run = format!(r#"<w:r><w:t w:{name}="{text}">{text}</w:t><w:{name}/></w:r>"#);
            let (inside, outside) =
                if in_choice { (run, String::new()) } else { (String::new(), run) };
            let dropped = hidden_run(&format!("<w:unknown>{outside}</w:unknown>"));
            let alternate = alternate(&inside, "");
            styled(&p("", &format!("{dropped}{alternate}")), "")
        };
        let (copied, uncopied) = (fuel(&copied_text(true)), fuel(&copied_text(false)));
        assert!(
            copied >= uncopied + 2 * 50_000 + 2 * 10_000,
            "the text, attributes and names a declined choice's copy holds: {copied} {uncopied}"
        );
    }

    #[test]
    fn an_equations_filtered_copy_is_charged_by_its_text() {
        let text = "t".repeat(50_000);
        let math = |first: &str| {
            let runs = format!("<m:r>{first}<m:t/></m:r><m:r><m:t>{text}</m:t></m:r>");
            styled(&p("", &MATH.replace("<m:r><m:t>y</m:t></m:r>", &runs)), "")
        };
        let (filtered, unfiltered) = (fuel(&math(HIDE)), fuel(&math("")));
        assert!(
            filtered >= unfiltered + 50_000,
            "the text an equation's filtered copy holds: {filtered} {unfiltered}"
        );
    }

    #[test]
    fn numbered_heading_keeps_its_number() {
        let document = format!(
            r#"<w:document {NS}><w:body>
            <w:p><w:pPr><w:pStyle w:val="H1"/></w:pPr><w:r><w:t>Intro</w:t></w:r></w:p>
            <w:p><w:pPr><w:pStyle w:val="H1"/></w:pPr><w:r><w:t>Details</w:t></w:r></w:p>
            <w:p><w:pPr><w:pStyle w:val="H1"/><w:numPr><w:ilvl w:val="1"/></w:numPr></w:pPr>
                <w:r><w:t>Part</w:t></w:r></w:p>
            </w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {NS}>
            <w:style w:type="paragraph" w:styleId="H1"><w:name w:val="heading 1"/>
                <w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr>
            </w:style></w:styles>"#
        );
        let numbering = format!(
            r#"<w:numbering {NS}>
            <w:abstractNum w:abstractNumId="0">
                <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:start w:val="1"/>
                    <w:lvlText w:val="%1."/><w:pStyle w:val="H1"/></w:lvl>
                <w:lvl w:ilvl="1"><w:numFmt w:val="decimal"/><w:start w:val="1"/>
                    <w:lvlText w:val="%1.%2"/></w:lvl>
            </w:abstractNum>
            <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
            </w:numbering>"#
        );
        let bytes = zip_package(&[
            ("word/document.xml", &document),
            ("word/styles.xml", &styles),
            ("word/numbering.xml", &numbering),
        ]);
        let doc = parse(&ParseCtx::default(), &bytes).unwrap();
        let headings: Vec<String> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Heading { content, .. } => Some(inlines_to_plain_text(content)),
                _ => None,
            })
            .collect();
        assert_eq!(headings, vec!["1. Intro", "2. Details", "2.1 Part"]);
    }

    #[test]
    fn a_drawing_with_nothing_to_read_stands_in_as_its_description() {
        let wp =
            r#"xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing""#;
        let stand_in = |drawn: &str| {
            let bytes = zip_package(&[
                ("word/document.xml", document(&drawing(drawn)).as_str()),
                ("word/_rels/document.xml.rels", ""),
            ]);
            let doc = parse(&ParseCtx::default(), &bytes).unwrap();
            doc.blocks.iter().find_map(|block| match block {
                Block::Paragraph(inlines) => inlines.iter().find_map(|inline| match inline {
                    Inline::Image { alt, source, .. } => Some((alt.clone(), source.clone())),
                    _ => None,
                }),
                _ => None,
            })
        };
        let described = |descr: &str| format!(r#"<wp:docPr {wp} descr="{descr}"/>"#);
        let cases = [
            ("a description", described("a chart"), Some(("a chart", ImageSource::Unavailable))),
            ("a blank description", described(" "), None),
            ("an object of no preview", r#"<o:OLEObject r:id="rId9"/>"#.to_string(), None),
            (
                "an object beside a blank description",
                format!(r#"{}<o:OLEObject ProgID="Equation.3"/>"#, described(" ")),
                None,
            ),
            (
                "an object beside a description",
                format!(r#"{}<o:OLEObject ProgID="Equation.3"/>"#, described("a sheet")),
                Some(("a sheet", ImageSource::Unavailable)),
            ),
        ];
        for (case, drawn, expected) in cases {
            let expected = expected.map(|(alt, source)| (alt.to_string(), source));
            assert_eq!(stand_in(&drawn), expected, "{case}");
        }
    }

    #[test]
    fn an_unreadable_picture_without_alt_keeps_its_place() {
        let xml = document(
            "<w:p><w:r><w:t>before</w:t><w:drawing><a:blip/></w:drawing><w:t>after</w:t></w:r></w:p>",
        );
        let bytes = zip_package(&[
            ("word/document.xml", &xml),
            ("word/_rels/document.xml.rels", &relationships(&[])),
        ]);
        let ctx = ParseCtx::default();
        let document = parse(&ctx, &bytes).unwrap();
        assert_eq!(document_to_markdown(&document), "before\\[Image\\]after\n");
        assert_eq!(
            diagnostics_of(ctx, &document, ImagePolicy::Placeholder)
                .unwrap()
                .iter()
                .filter(|d| d.code == DiagnosticCode::ImageUnavailable)
                .map(|d| d.count)
                .sum::<u64>(),
            1
        );
        for hidden in [
            xml.replace("<w:r>", &format!("<w:r>{HIDE}")),
            xml.replace("<w:r>", "<w:del><w:r>").replace("</w:r>", "</w:r></w:del>"),
        ] {
            let ctx = ParseCtx::default();
            let bytes = zip_package(&[
                ("word/document.xml", hidden.as_str()),
                ("word/_rels/document.xml.rels", ""),
            ]);
            let document = parse(&ctx, &bytes).unwrap();
            assert_eq!(document_to_markdown(&document), "");
            assert!(ctx.diagnostics().iter().all(|d| matches!(
                d.code,
                DiagnosticCode::HiddenOmitted | DiagnosticCode::RevisionOmitted
            )));
        }
    }

    fn bare(content: &str) -> String {
        format!("<w:r>{content}</w:r>")
    }

    fn check_box() -> String {
        let begin = bare(concat!(
            r#"<w:fldChar w:fldCharType="begin">"#,
            "<w:ffData><w:checkBox/></w:ffData></w:fldChar>",
        ));
        format!("{begin}{}{}{}", instr("", "FORMCHECKBOX"), field("", "separate"), field("", "end"))
    }

    fn retained_assets(parts: &Parts) -> usize {
        parse(&ParseCtx::default(), &zip_package(parts)).unwrap().assets.len()
    }

    #[test]
    fn a_vml_fill_that_paints_no_picture_is_neither_read_nor_reported() {
        let frame = r#"<v:fill r:id="rId2" type="frame"/>"#;
        let tile = r#"<v:fill r:id="rId2" type="tile"/>"#;
        let fills = [
            ("a pattern fill", "", r#"<v:fill r:id="rId2" type="pattern"/>"#),
            ("a solid fill", "", r#"<v:fill r:id="rId2" type="solid"/>"#),
            ("a fill of no type", "", r#"<v:fill r:id="rId2"/>"#),
            ("a frame fill naming no image", "", r#"<v:fill type="frame"/>"#),
            ("a tile fill naming no image", "", r#"<v:fill type="tile"/>"#),
            ("a fill switched off", "", r#"<v:fill r:id="rId2" type="frame" on="f"/>"#),
            ("a fill switched off in full", "", r#"<v:fill r:id="rId2" type="tile" on="false"/>"#),
            ("a fill of another vocabulary", "", r#"<o:fill r:id="rId2" type="frame"/>"#),
            ("a fill its shape switched off", r#"filled="f""#, frame),
            ("a fill its shape switched off in full", r#"filled="false""#, tile),
            (
                "a fill switched off under a shape switched on",
                r#"filled="t""#,
                r#"<v:fill r:id="rId2" type="frame" on="f"/>"#,
            ),
        ];
        let parts = |attributes: &str, fill: &str| {
            graphics(&run(&format!("<w:pict><v:rect {attributes}>{fill}</v:rect></w:pict>")))
        };
        assert_converts(
            fills
                .iter()
                .map(|(case, attributes, fill)| (*case, parts(attributes, fill), "", vec![]))
                .collect(),
        );
        let retained: Vec<&str> = fills
            .iter()
            .filter(|(_, attributes, fill)| retained_assets(&parts(attributes, fill)) > 0)
            .map(|(case, ..)| *case)
            .collect();
        assert_eq!(retained, Vec::<&str>::new());
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the table of what an instruction holds, in it and out"
    )]
    fn a_field_instruction_retains_and_reports_nothing_it_never_renders() {
        use DiagnosticCode::*;
        let anchor =
            r#"xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing""#;
        let second_picture = r#"<v:shape id="s2"><v:imagedata r:id="rId2"/></v:shape>"#;
        let reported = |code, count| vec![(code, Region::Body, count)];
        let cases: Vec<(&str, String, Vec<Reported>)> = vec![
            (
                "an embedded object",
                bare(&format!("<w:object>{VML_PICTURE}{OLE}</w:object>")),
                vec![(EmbeddedObject, Region::Body, 1), (ImagePlaceholder, Region::Body, 1)],
            ),
            (
                "a picture",
                bare(&format!("<w:drawing>{PICTURE}</w:drawing>")),
                reported(ImagePlaceholder, 1),
            ),
            (
                "a floating picture",
                bare(&format!("<w:drawing><wp:anchor {anchor}>{PICTURE}</wp:anchor></w:drawing>")),
                reported(ImagePlaceholder, 1),
            ),
            (
                "two VML pictures",
                bare(&format!("<w:pict>{VML_PICTURE}{second_picture}</w:pict>")),
                vec![(ImagePlaceholder, Region::Body, 1), (ImageUnplaced, Region::Body, 1)],
            ),
            (
                "a VML picture fill",
                bare(&format!("<w:pict>{VML_FILL}</w:pict>")),
                reported(ImagePlaceholder, 1),
            ),
            (
                "WordArt",
                bare(r#"<w:pict><v:shape><v:textpath string="Big"/></v:shape></w:pict>"#),
                reported(GraphicUnread, 1),
            ),
            (
                "shape text",
                bare(&format!("<w:drawing>{SHAPE_TEXT}</w:drawing>")),
                reported(GraphicUnread, 1),
            ),
            ("a nested check box", check_box(), reported(FormControlUnread, 1)),
            (
                "ruby",
                bare("<w:ruby><w:rubyBase><w:r><w:t>base</w:t></w:r></w:rubyBase></w:ruby>"),
                reported(RubyUnread, 1),
            ),
            ("ink", bare(r#"<w:contentPart r:id="rId1"/>"#), reported(GraphicUnread, 1)),
            ("a date", bare("<w:dayLong/>"), reported(FieldValueUnread, 1)),
            ("a page number", bare("<w:pgNum/>"), reported(FieldValueUnread, 1)),
            (
                "a hyperlink of an unknown id",
                r#"<w:hyperlink r:id="rId9"><w:r><w:t>label</w:t></w:r></w:hyperlink>"#.into(),
                reported(LinkTargetUnread, 1),
            ),
            ("a subdocument", r#"<w:subDoc r:id="rId9"/>"#.into(), reported(ImportedPartUnread, 1)),
            (
                "a declined picture in a run",
                bare(&declined(&format!("<w:drawing>{PICTURE}</w:drawing>"))),
                reported(ImageUnplaced, 1),
            ),
            ("declined text in a run", bare(&declined("<w:t>c</w:t>")), reported(BlockUnread, 1)),
            (
                "declined ink in a run",
                bare(&declined(r#"<w:contentPart r:id="rId1"/>"#)),
                reported(GraphicUnread, 1),
            ),
            (
                "declined text beside runs",
                declined(&bare("<w:t>c</w:t>")),
                reported(BlockUnread, 1),
            ),
            (
                "a declined picture in a drawing",
                bare(&format!("<w:drawing>{}</w:drawing>", declined(PICTURE))),
                reported(ImageUnplaced, 1),
            ),
            (
                "declined shape text in a drawing",
                bare(&format!("<w:drawing>{}</w:drawing>", declined(SHAPE_TEXT))),
                reported(BlockUnread, 1),
            ),
        ];
        let inside = |runs: &str| graphics(&p("", &in_instruction(runs)));
        assert_converts(
            cases.iter().map(|(case, runs, _)| (*case, inside(runs), "result\n", vec![])).collect(),
        );
        assert_reports(
            cases
                .iter()
                .map(|(case, runs, reported)| (*case, graphics(&p("", runs)), reported.clone()))
                .collect(),
        );
        let retained: Vec<&str> = cases
            .iter()
            .filter(|(_, runs, _)| retained_assets(&inside(runs)) > 0)
            .map(|(case, ..)| *case)
            .collect();
        assert_eq!(retained, Vec::<&str>::new());
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of blocks an instruction renders")]
    fn a_field_instruction_still_renders_and_reports_what_would_have_rendered() {
        use DiagnosticCode::{BlockUnread, GraphicUnread, ImageUnplaced, RubyUnread};
        let ruby = |content: &str| {
            bare(&format!("<w:ruby><w:rubyBase>{}</w:rubyBase></w:ruby>", bare(content)))
        };
        let diagram = format!(
            "<dgm:dataModel {NS}><dgm:ptLst><dgm:pt><dgm:t>node</dgm:t></dgm:pt></dgm:ptLst></dgm:dataModel>"
        );
        let in_graphics = |runs: &str, extra: Option<(&'static str, String)>| {
            let mut parts = graphics(&p("", &in_instruction(runs)));
            parts.extend(extra);
            parts
        };
        let cases = vec![
            (
                "a ruby holding a text box",
                in_graphics(&ruby(&format!("<w:drawing>{}</w:drawing>", text_box("box"))), None),
                "result\n",
                vec![(RubyUnread, Region::Body, 1)],
            ),
            (
                "a ruby holding a chart",
                in_graphics(&ruby(&format!("<w:drawing>{CHART}</w:drawing>")), None),
                "result\n",
                vec![(RubyUnread, Region::Body, 1)],
            ),
            (
                "a ruby holding a diagram",
                in_graphics(&ruby(r#"<w:drawing><dgm:relIds r:dm="rId5"/></w:drawing>"#), None),
                "result\n",
                vec![(RubyUnread, Region::Body, 1)],
            ),
            (
                "a ruby holding a chart and a diagram of no part",
                in_graphics(&ruby("<w:drawing><c:chart/><dgm:relIds/></w:drawing>"), None),
                "result\n",
                vec![],
            ),
            (
                "a ruby holding a picture",
                in_graphics(&ruby(&format!("<w:drawing>{PICTURE}</w:drawing>")), None),
                "result\n",
                vec![],
            ),
            (
                "a declined chart in a run",
                in_graphics(&bare(&declined(&format!("<w:drawing>{CHART}</w:drawing>"))), None),
                "result\n",
                vec![(GraphicUnread, Region::Body, 1)],
            ),
            (
                "a declined diagram in a drawing",
                in_graphics(
                    &bare(&format!(
                        "<w:drawing>{}</w:drawing>",
                        declined(r#"<dgm:relIds r:dm="rId5"/>"#)
                    )),
                    None,
                ),
                "result\n",
                vec![(GraphicUnread, Region::Body, 1)],
            ),
            (
                "a declined text box in a drawing",
                in_graphics(
                    &bare(&format!("<w:drawing>{}</w:drawing>", declined(&text_box("box")))),
                    None,
                ),
                "result\n",
                vec![(BlockUnread, Region::Body, 1)],
            ),
            (
                "a declined text box whose fallback holds one",
                in_graphics(
                    &bare(&format!(
                        "<w:drawing>{}</w:drawing>",
                        alternate(
                            &text_box("box"),
                            &format!("<mc:Fallback>{}</mc:Fallback>", text_box("fallback"))
                        )
                    )),
                    None,
                ),
                "fallback\n\nresult\n",
                vec![],
            ),
            (
                "a declined picture in a text box",
                in_graphics(
                    &bare(&format!(
                        "<w:drawing>{}</w:drawing>",
                        declined(&format!("<w:txbxContent>{}</w:txbxContent>", drawing(PICTURE)))
                    )),
                    None,
                ),
                "result\n",
                vec![(ImageUnplaced, Region::Body, 1)],
            ),
            (
                "a chart of no part",
                in_graphics(&bare("<w:drawing><c:chart/></w:drawing>"), None),
                "result\n",
                vec![(GraphicUnread, Region::Body, 1)],
            ),
            (
                "a diagram of no part",
                in_graphics(&bare("<w:drawing><dgm:relIds/></w:drawing>"), None),
                "result\n",
                vec![(GraphicUnread, Region::Body, 1)],
            ),
            (
                "a diagram",
                in_graphics(
                    &bare(r#"<w:drawing><dgm:relIds r:dm="rId5"/></w:drawing>"#),
                    Some(("word/diagrams/data1.xml", diagram)),
                ),
                "- node\n\nresult\n",
                vec![],
            ),
        ];
        assert_converts(cases);
    }
}
