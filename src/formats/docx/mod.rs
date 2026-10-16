//! OOXML WordprocessingML (.docx / .docm).
//!
//! Resolution pipeline: package parts -> style/numbering models ->
//! spec-order property resolution -> document model.

mod content;
mod numbering;
mod styles;

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Document, Note, NoteKind};
use crate::package::archive::{Role, report};
use crate::package::relationships::{Relationships, read_rels, rel_type, rels_part_for};
use crate::package::xml::{Element, ns};
use crate::package::{Package, PackageKind};
use crate::shared::assets::AssetSink;
use content::Ctx;
use numbering::Counters;
use std::cell::RefCell;
use std::collections::BTreeSet;

pub fn parse(parse_ctx: &ParseCtx, bytes: &[u8]) -> Result<Document, ConvertError> {
    let pkg = match Package::open(parse_ctx, bytes, PackageKind::Opc) {
        Ok(p) => p,
        Err(e @ ConvertError::Malformed { .. }) => {
            return Err(crate::package::archive::probe_ole(bytes).unwrap_or(e));
        }
        Err(e) => return Err(e),
    };
    let pkg = RefCell::new(pkg);

    let root_rels = read_rels(&mut pkg.borrow_mut(), "_rels/.rels", Role::Reported(Region::Body))?;
    let main_part = match root_rels
        .first_of_type(rel_type::OFFICE_DOCUMENT)
        .map(|rel| crate::package::path::resolve("", &rel.target))
    {
        Some(Ok(target)) => target.path,
        _ => {
            if pkg.borrow().has_part("_rels/.rels") {
                parse_ctx.emit(DiagnosticCode::ConventionalPartUsed, Region::Container);
            }
            "word/document.xml".to_string()
        }
    };
    let doc_rels =
        read_rels(&mut pkg.borrow_mut(), &rels_part_for(&main_part), Role::Content(Region::Body))?;

    let typed =
        |rel_type, fallback| typed_part_path(parse_ctx, &doc_rels, &main_part, rel_type, fallback);
    let numbering_part = typed(rel_type::NUMBERING, "numbering.xml");
    let styles_part = typed(rel_type::STYLES, "styles.xml");
    let styles_role = if pkg.borrow().has_part(&numbering_part.path) {
        Role::Content(Region::Body)
    } else {
        Role::Formatting
    };
    let (styles_tree, _) = styles_part.read(parse_ctx, &pkg, styles_role, "styles")?;
    let styles =
        styles::Styles::parse_opt(styles_tree.as_ref().and_then(|t| t.find(ns::W, "styles")));

    let (numbering_tree, numbering_reported) =
        numbering_part.read(parse_ctx, &pkg, Role::Content(Region::Body), "numbering")?;
    let numbering = match numbering_tree.as_ref().and_then(|t| t.find(ns::W, "numbering")) {
        Some(root) => numbering::parse(root, &|style_id| styles.direct_num_id(style_id))?,
        None => Default::default(),
    };

    let doc_tree = pkg.borrow_mut().required_xml_part(&main_part, Role::Content(Region::Body))?;
    let body = doc_tree
        .find(ns::W, "document")
        .and_then(|d| d.find(ns::W, "body"))
        .ok_or_else(|| ConvertError::malformed_part(&main_part, "no document body"))?;
    report_side_parts(parse_ctx, body, &doc_rels, &main_part);
    let mut commented = body.first_descendant(ns::W, "commentReference").is_some();

    let counters = RefCell::new(Counters::default());
    let assets = RefCell::new(AssetSink::new(parse_ctx));

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
    };
    let blocks = content::parse_blocks(body, &ctx)?;

    let mut notes = Vec::new();
    for (part, root_name, elem_name, prefix, kind) in [
        (footnotes_part, "footnotes", "footnote", "fn", NoteKind::Footnote),
        (endnotes_part, "endnotes", "endnote", "en", NoteKind::Endnote),
    ] {
        let (Some(tree), _) = part.read(parse_ctx, &pkg, Role::Content(Region::Note), root_name)?
        else {
            continue;
        };
        let Some(root) = tree.find(ns::W, root_name) else {
            continue;
        };
        commented |= root.first_descendant(ns::W, "commentReference").is_some();
        let note_rels = read_rels(
            &mut pkg.borrow_mut(),
            &rels_part_for(&part.path),
            Role::Content(Region::Note),
        )?;
        let note_ctx = ctx.for_part(note_rels, part.path, Region::Note);
        for note in root.find_all(ns::W, elem_name) {
            if matches!(
                note.attr(ns::W, "type"),
                Some("separator") | Some("continuationSeparator") | Some("continuationNotice")
            ) {
                continue;
            }
            let Some(id) = note.attr(ns::W, "id") else { continue };
            notes.push(Note {
                id: format!("{prefix}{id}"),
                kind,
                blocks: content::parse_blocks(note, &note_ctx)?,
            });
        }
    }
    if commented {
        parse_ctx.emit(DiagnosticCode::CommentOmitted, Region::Comment);
    }

    let assets = std::mem::take(&mut assets.borrow_mut().assets);
    Ok(Document { blocks, notes, assets })
}

/// Headers and footers the sections name, each part once (by its id where
/// its relationship is unknown): none is read.
fn report_side_parts(parse_ctx: &ParseCtx, body: &Element, rels: &Relationships, base: &str) {
    for (reference, code, region) in [
        ("headerReference", DiagnosticCode::HeaderOmitted, Region::Header),
        ("footerReference", DiagnosticCode::FooterOmitted, Region::Footer),
    ] {
        let parts: BTreeSet<String> = body
            .descendants(ns::W, "sectPr")
            .flat_map(|section| section.find_all(ns::W, reference))
            .filter_map(|r| r.attr_qualified(ns::R, "id"))
            .map(|id| {
                rels.internal_target(id)
                    .and_then(|target| crate::package::path::resolve(base, target).ok())
                    .map_or_else(|| id.to_string(), |target| target.path)
            })
            .collect();
        if !parts.is_empty() {
            parse_ctx.emit_count(code, region, parts.len() as u64);
        }
    }
}

/// A typed part of the main part, and whether a relationship of its type
/// names it.
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
        parse_ctx: &ParseCtx,
        pkg: &RefCell<Package>,
        role: Role,
        root: &str,
    ) -> Result<(Option<Element>, bool), ConvertError> {
        let tree = if self.related {
            pkg.borrow_mut().related_xml_part(&self.path, role)?
        } else {
            pkg.borrow_mut().optional_xml_part(&self.path, role)?
        };
        match tree {
            Some(tree) if tree.find(ns::W, root).is_some() => Ok((Some(tree), false)),
            Some(_) => {
                report(parse_ctx, role.unreadable());
                Ok((None, true))
            }
            None => Ok((None, self.related || pkg.borrow().has_part(&self.path))),
        }
    }
}

/// Path of a typed related part, resolved against the main part; falls back
/// to the conventional sibling name when the relationship is absent or its
/// target does not resolve, the latter reported.
fn typed_part_path(
    parse_ctx: &ParseCtx,
    rels: &Relationships,
    base: &str,
    rel_type: &str,
    fallback: &str,
) -> TypedPart {
    let related = rels.first_of_type(rel_type).map(|rel| rel.target.as_str());
    let reference = related.unwrap_or(fallback);
    let path = match crate::package::path::resolve(base, reference) {
        Ok(target) => target.path,
        Err(_) => {
            parse_ctx.emit(DiagnosticCode::ConventionalPartUsed, Region::Container);
            crate::package::path::resolve(base, fallback)
                .map(|t| t.path)
                .unwrap_or_else(|_| fallback.to_string())
        }
    };
    TypedPart { path, related: related.is_some() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::DiagnosticCode;
    use crate::model::{Block, ImageSource, Inline};
    use std::io::{Cursor, Write};

    fn docx_parts(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        for (name, body) in parts {
            w.start_file(*name, opts).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn docx(document: &str, rels: &str) -> Vec<u8> {
        docx_parts(&[("word/document.xml", document), ("word/_rels/document.xml.rels", rels)])
    }

    const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn find_image(blocks: &[Block]) -> Option<&Inline> {
        blocks.iter().find_map(|b| match b {
            Block::Paragraph(inlines) => inlines.iter().find(|i| matches!(i, Inline::Image { .. })),
            _ => None,
        })
    }

    #[test]
    fn linked_image_relationship_becomes_an_external_source() {
        // M9: `r:link` with an external-mode relationship must carry the URL
        // instead of failing the internal-part loader.
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
        let doc = parse(&ParseCtx::default(), &docx(document, rels)).unwrap();
        let image = find_image(&doc.blocks).expect("image inline");
        let Inline::Image { source, .. } = image else { unreachable!() };
        assert_eq!(*source, ImageSource::External("https://e.com/pic.png".into()));
        assert!(doc.assets.is_empty(), "external images are not retained as assets");
    }

    fn numbering_with_start(start: &str) -> String {
        format!(
            r#"<w:numbering {W}>
            <w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0">
                <w:numFmt w:val="decimal"/><w:start w:val="{start}"/>
            </w:lvl></w:abstractNum>
            <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
            </w:numbering>"#
        )
    }

    fn numbered_paragraphs() -> String {
        let para = r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr>
            <w:r><w:t>item</w:t></w:r></w:p>"#;
        format!(r#"<w:document {W}><w:body>{para}{para}</w:body></w:document>"#)
    }

    #[test]
    fn huge_numbering_start_values_cannot_overflow() {
        // H2: w:start is ST_DecimalNumber (xsd:int); out-of-range values are
        // clamped so document-order increments can never overflow.
        for start in ["18446744073709551615", "-5", "2147483647"] {
            let bytes = docx_parts(&[
                ("word/document.xml", &numbered_paragraphs()),
                ("word/numbering.xml", &numbering_with_start(start)),
            ]);
            let doc = parse(&ParseCtx::default(), &bytes).expect(start);
            assert!(!doc.blocks.is_empty());
        }
    }

    #[test]
    fn partial_num_pr_inherits_property_by_property() {
        // M1: a direct numPr carrying only ilvl merges with the style's
        // numId instead of suppressing numbering.
        let document = format!(
            r#"<w:document {W}><w:body>
            <w:p><w:pPr><w:pStyle w:val="Listy"/>
                <w:numPr><w:ilvl w:val="1"/></w:numPr></w:pPr>
                <w:r><w:t>second level</w:t></w:r></w:p>
            </w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W}>
            <w:style w:type="paragraph" w:styleId="Listy">
                <w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr>
            </w:style></w:styles>"#
        );
        let numbering = format!(
            r#"<w:numbering {W}>
            <w:abstractNum w:abstractNumId="0">
                <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:start w:val="1"/></w:lvl>
                <w:lvl w:ilvl="1"><w:numFmt w:val="lowerLetter"/><w:start w:val="1"/></w:lvl>
            </w:abstractNum>
            <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
            </w:numbering>"#
        );
        let bytes = docx_parts(&[
            ("word/document.xml", &document),
            ("word/styles.xml", &styles),
            ("word/numbering.xml", &numbering),
        ]);
        let doc = parse(&ParseCtx::default(), &bytes).unwrap();
        let Some(Block::List(list)) = doc.blocks.first() else {
            panic!("expected a list, got {:?}", doc.blocks.first());
        };
        assert_eq!(
            list.marker,
            crate::model::MarkerKind::LowerAlpha,
            "level 1 of the style's numbering"
        );
    }

    #[test]
    fn unmarked_run_edge_whitespace_is_kept() {
        // Converters that never write xml:space carry inter-word spacing on
        // run edges; dropping it glues the words together.
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:t>This</w:t></w:r>
            <w:r><w:t> by-law</w:t></w:r>
            <w:r><w:t> grants</w:t></w:r>
            </w:p></w:body></w:document>"#
        );
        let doc =
            parse(&ParseCtx::default(), &docx_parts(&[("word/document.xml", &document)])).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else { panic!() };
        assert_eq!(crate::model::inlines_to_plain_text(inlines), "This by-law grants");
    }

    #[test]
    fn page_break_between_runs_keeps_the_word_boundary() {
        // A w:br is unrepresentable in Markdown whatever its type, but the
        // runs it separates must not merge into a word the document never
        // had. A break ending a paragraph still leaves no stray marker.
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
            let document = format!(r#"<w:document {W}><w:body>{body}</w:body></w:document>"#);
            let bytes = docx_parts(&[("word/document.xml", &document)]);
            let markdown = crate::render::markdown::document_to_markdown(
                &parse(&ParseCtx::default(), &bytes).unwrap(),
            );
            assert_eq!(markdown, expected, "body: {body}");
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
        r#"xmlns:v="urn:schemas-microsoft-com:vml" xmlns:o="urn:schemas-microsoft-com:office:office""#,
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

    const PICTURE: &str = r#"<a:blip r:embed="rId2"/>"#;
    const CHART: &str = r#"<c:chart r:id="rId3"/>"#;
    const OLE: &str = r#"<o:OLEObject ProgID="Equation.3" ShapeID="s1" r:id="rId4"/>"#;
    const VML_PICTURE: &str = r#"<v:shape id="s1"><v:imagedata r:id="rId2"/></v:shape>"#;
    const CELL: &str = "<w:tc><w:p/></w:tc>";
    const SHAPE_TEXT: &str = "<a:p><a:r><a:t>label</a:t></a:r></a:p>";
    const NUMBERED: &str = r#"<w:p><w:pPr><w:numPr><w:numId w:val="7"/></w:numPr></w:pPr></w:p>"#;

    type Reported = (DiagnosticCode, Region, u64);
    type Parts = Vec<(&'static str, String)>;

    fn reported(parts: &[(&str, &str)]) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, &docx_parts(parts)).unwrap();
        let mut reported: Vec<Reported> =
            parse_ctx.diagnostics().iter().map(|d| (d.code, d.region, d.count)).collect();
        reported.sort();
        reported
    }

    #[test]
    fn each_loss_is_reported_in_its_region() {
        use DiagnosticCode::*;
        use Region::{Body, Comment, Container, Footer, Header, Note};
        let parts = relationships(&[
            ("rId2", "image", "media/one.png"),
            ("rId3", "chart", "charts/chart1.xml"),
            ("rId4", "oleObject", "embeddings/one.bin"),
            ("rId5", "diagramData", "diagrams/data1.xml"),
        ]);
        let graphics = |body: &str| {
            vec![
                ("word/document.xml", document(body)),
                ("word/_rels/document.xml.rels", parts.clone()),
                ("word/media/one.png", "png".into()),
                ("word/embeddings/one.bin", "ole".into()),
            ]
        };
        let chart = |xml: &str| {
            let mut parts = graphics(&drawing(CHART));
            parts.push(("word/charts/chart1.xml", xml.into()));
            parts
        };
        let alone = |body: &str| vec![("word/document.xml", document(body))];
        let declined_in_note = |choice: &str| {
            vec![
                ("word/document.xml", document("<w:p/>")),
                (
                    "word/footnotes.xml",
                    format!(
                        r#"<w:footnotes {NS}><w:footnote w:id="1">{}</w:footnote></w:footnotes>"#,
                        run(&alternate(choice, ""))
                    ),
                ),
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
                ("word/_rels/document.xml.rels", "</Relationships>".to_string()),
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
                "deleted and moved-out text",
                alone(
                    "<w:p><w:del><w:r><w:delText>gone</w:delText></w:r></w:del>
                    <w:moveFrom><w:r><w:t>moved</w:t></w:r></w:moveFrom></w:p>",
                ),
                vec![(RevisionOmitted, Body, 2)],
            ),
            (
                "a symbol that is not a checkbox",
                alone(&run(
                    r#"<w:sym w:font="Symbol" w:char="F061"/><w:sym w:font="Wingdings" w:char="F0FE"/>"#,
                )),
                vec![(UnmappedSymbol, Body, 1)],
            ),
            ("ruby", alone(&run("<w:ruby><w:rubyBase/></w:ruby>")), vec![(RubyUnread, Body, 1)]),
            (
                "ruby in a note",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    (
                        "word/footnotes.xml",
                        format!(
                            r#"<w:footnotes {NS}><w:footnote w:id="1">{}</w:footnote></w:footnotes>"#,
                            run("<w:ruby/>")
                        ),
                    ),
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
                    <w:fldChar w:fldCharType="begin"><w:ffData><w:ddList/></w:ffData></w:fldChar>
                    <w:fldChar w:fldCharType="begin"><w:ffData><w:textInput/></w:ffData></w:fldChar>"#,
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
                "a main part target that does not resolve",
                vec![
                    ("word/document.xml", document("<w:p/>")),
                    ("_rels/.rels", relationships(&[("rId1", "officeDocument", "a%2Fb.xml")])),
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
                    (
                        "word/footnotes.xml",
                        format!(
                            r#"<w:footnotes {NS}><w:footnote w:id="1">{}</w:footnote></w:footnotes>"#,
                            run(r#"<w:commentReference w:id="1"/>"#)
                        ),
                    ),
                ],
                vec![(CommentOmitted, Comment, 1)],
            ),
            (
                "comments, once",
                alone(&run(r#"<w:commentReference w:id="1"/><w:commentReference w:id="2"/>"#)),
                vec![(CommentOmitted, Comment, 1)],
            ),
            (
                "WordArt",
                alone(&run(r#"<w:pict><v:shape><v:textpath string="Big"/></v:shape></w:pict>"#)),
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "a VML picture named by o:relid",
                graphics(&run(
                    r#"<w:pict><v:shape><v:imagedata o:relid="rId2"/></v:shape></w:pict>"#,
                )),
                vec![(ImageUnavailable, Body, 1)],
            ),
            (
                "an embedded object, its preview aside",
                graphics(&run(&format!("<w:object>{VML_PICTURE}{OLE}</w:object>"))),
                vec![(EmbeddedObject, Body, 1)],
            ),
            (
                "an embedded object beside a picture that is not its preview",
                graphics(&run(&format!(
                    r#"<w:pict><v:group>{VML_PICTURE}{OLE}<v:shape id="s2"><v:imagedata r:id="rId2"/></v:shape></v:group></w:pict>"#
                ))),
                vec![(EmbeddedObject, Body, 1), (ImageUnplaced, Body, 1)],
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
                vec![(GraphicUnread, Body, 1)],
            ),
            (
                "a diagram with no part beside the picture read",
                graphics(&drawing(&format!("<dgm:relIds/>{PICTURE}"))),
                vec![(GraphicUnread, Body, 1)],
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
                chart(&format!(r#"<c:chartSpace {NS}><c:ser><c:xVal/></c:ser></c:chartSpace>"#)),
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
                vec![(GraphicUnread, Body, 1)],
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
                vec![
                    ("word/document.xml", document(&drawing(r#"<a:blip r:embed="rId9"/>"#))),
                    ("word/_rels/document.xml.rels", "</Relationships>".into()),
                ],
                vec![(PartUnreadable, Body, 1)],
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
                vec![(ImageUnplaced, Body, 1), (EmbeddedObject, Body, 1), (GraphicUnread, Body, 1)],
            ),
            (
                "every picture after the first",
                graphics(&drawing(&format!("{PICTURE}{PICTURE}"))),
                vec![(ImageUnplaced, Body, 1)],
            ),
            (
                "a link of an unknown id, unless its label is a URL or it has an anchor",
                alone(
                    r#"<w:p><w:hyperlink r:id="rId9"><w:r><w:t>see here</w:t></w:r></w:hyperlink>
                    <w:hyperlink r:id="rId9" w:anchor="top"><w:r><w:t>see top</w:t></w:r></w:hyperlink>
                    <w:hyperlink r:id="rId9"><w:r><w:t>https://example.org</w:t></w:r></w:hyperlink></w:p>"#,
                ),
                vec![(LinkTargetUnread, Body, 1)],
            ),
            (
                "a declined chart, whatever the fallback shows",
                alone(&run(&alternate(
                    r#"<w:drawing><cx:chart r:id="rId3"/></w:drawing>"#,
                    r#"<mc:Fallback><w:t>This chart isn't available</w:t></mc:Fallback>"#,
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
            ("declined text in a note", declined_in_note("<w:t>words</w:t>"), vec![(BlockUnread, Note, 1)]),
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
                "declined shape text",
                declined(&format!("<w:drawing>{SHAPE_TEXT}</w:drawing>")),
                vec![(BlockUnread, Body, 1)],
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
                vec![],
            ),
            (
                "deleted rows and cells",
                alone(
                    r#"<w:tbl><w:tr><w:trPr><w:del/></w:trPr><w:tc><w:p/></w:tc></w:tr>
                    <w:tr><w:tc><w:tcPr><w:cellDel/></w:tcPr><w:p/></w:tc><w:tc><w:p/></w:tc></w:tr></w:tbl>"#,
                ),
                vec![(RevisionOmitted, Body, 2)],
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
        for (case, parts, mut expected) in cases {
            expected.sort();
            let parts: Vec<(&str, &str)> =
                parts.iter().map(|(name, xml)| (*name, xml.as_str())).collect();
            assert_eq!(reported(&parts), expected, "{case}");
        }
    }

    #[test]
    fn an_imported_part_alone_fails_the_conversion() {
        let bytes = docx_parts(&[("word/document.xml", &document(r#"<w:altChunk r:id="rId1"/>"#))]);
        let converter = crate::Converter::new(crate::Config::default()).unwrap();
        let result = converter.convert(crate::Input::new(&bytes, crate::Format::Docx));
        assert!(matches!(result, Err(ConvertError::Incomplete { .. })), "{result:?}");
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
            let bytes = docx_parts(&[
                ("word/document.xml", &document(&body)),
                ("word/numbering.xml", &numbering),
            ]);
            let markdown = crate::render::markdown::document_to_markdown(
                &parse(&ParseCtx::default(), &bytes).unwrap(),
            );
            assert_eq!(markdown, expected);
        }
    }

    #[test]
    fn numbered_heading_keeps_its_number() {
        // H1: a heading style with numbering shows its label and advances
        // the sequence.
        let document = format!(
            r#"<w:document {W}><w:body>
            <w:p><w:pPr><w:pStyle w:val="H1"/></w:pPr><w:r><w:t>Intro</w:t></w:r></w:p>
            <w:p><w:pPr><w:pStyle w:val="H1"/></w:pPr><w:r><w:t>Details</w:t></w:r></w:p>
            </w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W}>
            <w:style w:type="paragraph" w:styleId="H1"><w:name w:val="heading 1"/>
                <w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr>
            </w:style></w:styles>"#
        );
        let numbering = format!(
            r#"<w:numbering {W}>
            <w:abstractNum w:abstractNumId="0">
                <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:start w:val="1"/>
                    <w:lvlText w:val="%1."/><w:pStyle w:val="H1"/></w:lvl>
            </w:abstractNum>
            <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
            </w:numbering>"#
        );
        let bytes = docx_parts(&[
            ("word/document.xml", &document),
            ("word/styles.xml", &styles),
            ("word/numbering.xml", &numbering),
        ]);
        let doc = parse(&ParseCtx::default(), &bytes).unwrap();
        let headings: Vec<String> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Heading { content, .. } => {
                    Some(crate::model::inlines_to_plain_text(content))
                }
                _ => None,
            })
            .collect();
        assert_eq!(headings, vec!["1. Intro", "2. Details"]);
    }
}
