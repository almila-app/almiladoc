use super::text::{Ctx, parse_container};
use crate::ctx::ParseCtx;
use crate::diagnostics::Region;
use crate::error::ConvertError;
use crate::model::{Block, Inline};
use crate::package::xml::{ContentSelection, Element, Node, ns};
use crate::shared::comments::{CommentThreads, ReferenceUnits, SourceComment};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const LOEXT: &str = "urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0";

#[derive(Default)]
pub(super) struct Comments<'e> {
    sources: HashMap<usize, (&'e Element, usize)>,
    definitions: Vec<Definition<'e>>,
}

struct Definition<'e> {
    element: &'e Element,
    comment: SourceComment,
}

impl<'e> Comments<'e> {
    pub(super) fn new(
        ctx: &ParseCtx<'_>,
        trees: impl Iterator<Item = &'e Element>,
    ) -> Result<Self, ConvertError> {
        let mut comments = Self::default();
        if !ctx.comments_requested() {
            return Ok(comments);
        }
        for tree in trees {
            let mut pending = vec![(tree, false)];
            while let Some((element, nested)) = pending.pop() {
                ctx.charge(1)?;
                let annotation = is_annotation(element);
                if annotation {
                    comments.index(ctx, element, nested)?;
                }
                for node in element.children.iter().rev() {
                    if let Node::Elem(child) = node {
                        pending.push((child, nested || annotation));
                    }
                }
            }
        }
        Ok(comments)
    }

    fn index(
        &mut self,
        ctx: &ParseCtx<'_>,
        element: &'e Element,
        nested: bool,
    ) -> Result<(), ConvertError> {
        let key = source_key(element);
        if self.sources.contains_key(&key) {
            return Ok(());
        }
        let ordinal = self.sources.len();
        self.sources.insert(key, (element, ordinal));
        if nested {
            return Ok(());
        }
        let id = source_id(ctx, element, ordinal)?;
        let parent = element.attr_qualified(LOEXT, "parent-name");
        ctx.charge(parent.map_or(0, |name| name.len().saturating_add("odf:c:n:".len())) as u64)?;
        self.definitions.push(Definition {
            element,
            comment: SourceComment {
                id,
                parent: parent.map(|name| format!("odf:c:n:{name}")),
                blocks: Vec::new(),
            },
        });
        Ok(())
    }

    pub(super) fn reference(
        &self,
        ctx: &ParseCtx<'_>,
        element: &Element,
    ) -> Result<Inline, ConvertError> {
        let (source, ordinal) =
            self.sources.get(&source_key(element)).copied().ok_or(ConvertError::Internal)?;
        source_id(ctx, source, ordinal).map(Inline::CommentRef)
    }
}

fn source_key(element: &Element) -> usize {
    match element.selection.as_deref() {
        Some(ContentSelection::Source(key)) => *key,
        _ => std::ptr::from_ref(element) as usize,
    }
}

pub(super) fn is_annotation(element: &Element) -> bool {
    element.is(ns::OFFICE, "annotation") || element.is(super::OFFICEOOO, "annotation")
}

fn source_id(ctx: &ParseCtx<'_>, source: &Element, ordinal: usize) -> Result<String, ConvertError> {
    if let Some(name) = source.attr_qualified(ns::OFFICE, "name") {
        ctx.charge(name.len().saturating_add("odf:c:n:".len()) as u64)?;
        return Ok(format!("odf:c:n:{name}"));
    }
    let id = format!("odf:c:a:{ordinal}");
    ctx.charge(id.len() as u64)?;
    Ok(id)
}

pub(super) fn append(ctx: &Ctx<'_, '_>, blocks: &mut [Block]) -> Result<(), ConvertError> {
    let definitions = std::mem::take(&mut ctx.comments.borrow_mut().definitions);
    if definitions.is_empty() {
        return Ok(());
    }
    let referenced = references(ctx, blocks)?;
    if referenced.is_empty() {
        return Ok(());
    }
    let selected = referenced
        .keys()
        .map(|id| {
            ctx.parse_ctx.charge(id.len() as u64)?;
            Ok(id.clone())
        })
        .collect::<Result<BTreeSet<_>, ConvertError>>()?;
    let (elements, mut comments): (Vec<_>, Vec<_>) =
        definitions.into_iter().map(|definition| (definition.element, definition.comment)).unzip();
    let threads = CommentThreads::new(ctx.parse_ctx, &comments, Some(&selected))?;
    threads.admit(ctx.parse_ctx)?;
    remap(ctx, blocks, threads.aliases())?;
    let units = references(ctx, blocks)?;
    read_bodies(ctx, &mut comments, &elements, &threads, &units)?;
    let roots = comments
        .iter()
        .map(|comment| {
            let root = threads.aliases().get(&comment.id).ok_or(ConvertError::Internal)?;
            ctx.parse_ctx.charge(root.len() as u64)?;
            Ok(root.clone())
        })
        .collect::<Result<Vec<_>, ConvertError>>()?;
    let (notes, _) = threads.finish_with(ctx.parse_ctx, comments, |index, code| {
        let root = roots.get(index).ok_or(ConvertError::Internal)?;
        let units = units.get(root).ok_or(ConvertError::Internal)?;
        for unit in &units.all {
            let _reports = ctx.parse_ctx.scope(*unit);
            ctx.parse_ctx.emit(code, Region::Comment);
        }
        Ok(())
    })?;
    ctx.notes.borrow_mut().extend(notes);
    Ok(())
}

fn read_bodies<'e>(
    ctx: &Ctx<'_, 'e>,
    comments: &mut [SourceComment],
    elements: &[&'e Element],
    threads: &CommentThreads,
    units: &BTreeMap<String, ReferenceUnits>,
) -> Result<(), ConvertError> {
    for (index, (comment, element)) in comments.iter_mut().zip(elements).enumerate() {
        if !threads.includes(index) {
            continue;
        }
        let root = threads.aliases().get(&comment.id).ok_or(ConvertError::Internal)?;
        let units = units.get(root).ok_or(ConvertError::Internal)?;
        let _reports = ctx.parse_ctx.scope(units.first);
        let selected = ctx.selected_story(element)?;
        let source = selected.as_ref().unwrap_or(element);
        let (parsed, held) = ctx
            .parse_ctx
            .holding_reports(|| ctx.in_side(Region::Comment, || parse_container(source, ctx)))?;
        comment.blocks = parsed;
        for unit in &units.all {
            let _reports = ctx.parse_ctx.scope(*unit);
            ctx.parse_ctx.report_held(&held)?;
        }
    }
    Ok(())
}

fn references(
    ctx: &Ctx<'_, '_>,
    blocks: &[Block],
) -> Result<BTreeMap<String, ReferenceUnits>, ConvertError> {
    let notes = ctx.notes.borrow();
    crate::shared::comments::reference_units(
        blocks,
        &notes,
        |_| None,
        |work| ctx.parse_ctx.charge(work),
    )?
    .into_iter()
    .map(|(id, units)| {
        ctx.parse_ctx.charge(id.len() as u64)?;
        Ok((id.to_owned(), units))
    })
    .collect()
}

fn remap(
    ctx: &Ctx<'_, '_>,
    blocks: &mut [Block],
    roots: &BTreeMap<String, String>,
) -> Result<(), ConvertError> {
    crate::shared::comments::remap_references(ctx.parse_ctx, blocks, roots)?;
    for note in &mut *ctx.notes.borrow_mut() {
        crate::shared::comments::remap_references(ctx.parse_ctx, &mut note.blocks, roots)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::CommentPolicy;
    use crate::ctx::ParseCtx;
    use crate::diagnostics::{DiagnosticCode, Region};
    use crate::error::ConvertError;
    use crate::formats::odf::parse;
    use crate::model::{Block, Document, Inline, NoteKind};
    use crate::shared::test_support::{diagnostic_rows, zip_package};

    const NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
        xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
        xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
        xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
        xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
        xmlns:dc="http://purl.org/dc/elements/1.1/"
        xmlns:loext="urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0"
        xmlns:officeooo="http://openoffice.org/2009/office"
        xmlns:xlink="http://www.w3.org/1999/xlink""#;

    fn package(kind: &str, head: &str, body: &str) -> Vec<u8> {
        let content = format!(
            "<office:document-content {NS}>{head}<office:body><office:{kind}>{body}</office:{kind}></office:body></office:document-content>"
        );
        zip_package(&[("content.xml", content.as_bytes()), ("Pictures/p.png", &[1, 2, 3])])
    }

    type Reported = (DiagnosticCode, Region, Option<u32>, u64);

    fn emitted(kind: &str, head: &str, body: &str) -> (Document, Vec<Reported>) {
        let ctx = ParseCtx::default().with_comments(CommentPolicy::Emit);
        let doc = parse(&ctx, &package(kind, head, body)).unwrap();
        (doc, diagnostic_rows(&ctx.into_diagnostics()))
    }

    fn annotation(name: &str, parent: Option<&str>, body: &str) -> String {
        let parent = parent.map_or(String::new(), |id| format!(r#" loext:parent-name="{id}""#));
        format!(r#"<office:annotation office:name="{name}"{parent}>{body}</office:annotation>"#)
    }

    #[test]
    fn rich_annotations_keep_blocks_assets_and_exclude_metadata() {
        let body = annotation(
            "n",
            None,
            r#"<dc:creator>private author</dc:creator><dc:date>2025-02-03</dc:date><text:p>rich<text:span text:style-name="Bold"> bold</text:span><draw:frame><draw:image xlink:href="Pictures/p.png"/></draw:frame></text:p><text:list><text:list-item><text:p>item</text:p></text:list-item></text:list><table:table><table:table-row><table:table-cell><text:p>cell</text:p></table:table-cell></table:table-row></table:table>"#,
        );
        let head = r#"<office:automatic-styles><style:style style:name="Bold" style:family="text"><style:text-properties fo:font-weight="bold" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"/></style:style></office:automatic-styles>"#;
        let (doc, losses) = emitted("text", head, &format!("<text:p>body{body}</text:p>"));
        assert_eq!(losses, []);
        assert_eq!(doc.notes.len(), 1);
        assert_eq!(doc.notes[0].kind, NoteKind::Comment);
        assert_eq!(doc.notes[0].id, "odf:c:n:n");
        assert!(matches!(
            doc.notes[0].blocks.as_slice(),
            [Block::Paragraph(_), Block::List(_), Block::Table(_)]
        ));
        let Block::Paragraph(inlines) = &doc.notes[0].blocks[0] else { panic!() };
        assert!(
            matches!(&inlines[1], Inline::Text { text, style } if text == " bold" && style.bold)
        );
        assert_eq!(doc.assets.len(), 1);
        assert!(!format!("{:?}", doc.notes).contains("private author"));
        assert!(
            matches!(&doc.blocks[0], Block::Paragraph(inlines) if matches!(inlines.last(), Some(Inline::CommentRef(id)) if id == "odf:c:n:n"))
        );
    }

    #[test]
    fn global_reply_links_keep_immediate_parent_quotes_across_slides() {
        let root = annotation("root", None, "<text:p>root</text:p>");
        let reply = annotation("reply", Some("root"), "<text:p>reply</text:p>");
        let nested = annotation("nested", Some("reply"), "<text:p>nested</text:p>");
        let (doc, losses) = emitted(
            "presentation",
            "",
            &format!("<draw:page>{root}</draw:page><draw:page>{reply}{nested}</draw:page>"),
        );
        assert_eq!(losses, []);
        assert_eq!(doc.notes.len(), 1);
        assert_eq!(doc.notes[0].id, "odf:c:n:root");
        let [Block::Paragraph(_), Block::BlockQuote(reply)] = doc.notes[0].blocks.as_slice() else {
            panic!()
        };
        assert!(matches!(reply.as_slice(), [Block::Paragraph(_), Block::BlockQuote(_)]));
        assert!(
            matches!(&doc.blocks[3], Block::Paragraph(inlines) if matches!(inlines.first(), Some(Inline::CommentRef(id)) if id == "odf:c:n:root"))
        );
    }

    #[test]
    fn duplicate_annotation_bodies_stay_unread_and_note_namespaces_stay_separate() {
        let first = annotation("same", None, "<text:p>first</text:p>");
        let loser = annotation(
            "same",
            Some("missing"),
            "<text:p><draw:frame><draw:image/></draw:frame></text:p>",
        );
        let body = format!(
            r#"<text:p>{first}{loser}<text:note text:id="same"><text:note-body><text:p>footnote</text:p></text:note-body></text:note></text:p>"#
        );
        let (doc, losses) = emitted("text", "", &body);
        assert_eq!(losses, [(DiagnosticCode::DuplicateNote, Region::Comment, None, 1)]);
        assert_eq!(doc.notes.len(), 2);
        assert_eq!(
            doc.notes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            ["odf:n:same", "odf:c:n:same"]
        );
        assert_eq!(doc.assets, []);
    }

    #[test]
    fn source_reuse_in_repeated_cells_registers_one_definition() {
        let comment = annotation("cell", None, "<text:p>one body</text:p>");
        let body = format!(
            r#"<table:table><table:table-row table:number-rows-repeated="3"><table:table-cell><text:p>{comment}</text:p></table:table-cell></table:table-row></table:table>"#
        );
        let (doc, losses) = emitted("spreadsheet", "", &body);
        assert_eq!(losses, []);
        assert_eq!(doc.notes.len(), 1);
        let Block::Table(table) = &doc.blocks[1] else { panic!() };
        assert_eq!(table.grid.len(), 3);
        assert_eq!(doc.notes[0].blocks, [Block::Paragraph(vec![Inline::plain("one body")])]);
    }

    #[test]
    fn graph_losses_follow_the_units_that_consume_the_selected_thread() {
        let root = annotation("root", None, "<text:p>first</text:p>");
        let duplicate = annotation("root", None, "<text:p>loser</text:p>");
        let dangling = annotation("dangling", Some("absent"), "<text:p>orphan</text:p>");
        let (doc, losses) = emitted(
            "presentation",
            "",
            &format!("<draw:page>{root}</draw:page><draw:page>{duplicate}{dangling}</draw:page>"),
        );
        assert_eq!(doc.notes.len(), 2);
        assert_eq!(
            losses,
            [
                (DiagnosticCode::DanglingReference, Region::Comment, Some(2), 1),
                (DiagnosticCode::DuplicateNote, Region::Comment, Some(1), 1),
                (DiagnosticCode::DuplicateNote, Region::Comment, Some(2), 1)
            ]
        );
    }

    #[test]
    fn nested_note_and_comment_references_never_import_side_bodies() {
        let nested = annotation(
            "nested",
            Some("absent"),
            r#"<text:p><draw:frame><draw:image xlink:href="Pictures/p.png"/></draw:frame>unread nested body</text:p>"#,
        );
        let note = r#"<text:note text:id="side"><text:note-body><text:p><draw:frame><draw:image xlink:href="Pictures/p.png"/></draw:frame>unread note body</text:p></text:note-body></text:note>"#;
        let root = annotation("root", None, &format!("<text:p>outer{nested}{note}</text:p>"));
        let bytes = package("text", "", &format!("<text:p>{root}</text:p>"));
        let ctx = ParseCtx::default().with_comments(CommentPolicy::Emit);
        let doc = parse(&ctx, &bytes).unwrap();
        assert_eq!(doc.notes.len(), 1);
        assert_eq!(doc.assets, []);
        assert!(!format!("{:?}", doc.notes).contains("unread"));
        let diagnostics =
            crate::converter::diagnostics_of(ctx, &doc, crate::ImagePolicy::Placeholder).unwrap();
        assert_eq!(
            diagnostic_rows::<(DiagnosticCode, Region, Option<u32>, u64)>(&diagnostics),
            [(DiagnosticCode::DanglingReference, Region::Comment, None, 2)]
        );
    }

    #[test]
    fn hidden_and_rejected_annotations_never_enter_the_graph() {
        let comment = annotation("hidden", Some("absent"), "<text:p>not selected</text:p>");
        let body = format!(
            r#"<text:section text:display="none"><text:p>hidden{comment}</text:p></text:section><text:p><text:change-start text:change-id="inserted"/>{comment}<text:change-end text:change-id="inserted"/></text:p><text:tracked-changes><text:changed-region text:id="inserted"><text:insertion/></text:changed-region></text:tracked-changes>"#
        );
        let bytes = package("text", "", &body);
        let ctx = ParseCtx::default()
            .with_comments(CommentPolicy::Emit)
            .with_revisions(crate::RevisionPolicy::Reject);
        let doc = parse(&ctx, &bytes).unwrap();
        assert_eq!(doc.notes, []);
        assert_eq!(
            diagnostic_rows::<(DiagnosticCode, Region, Option<u32>, u64)>(&ctx.into_diagnostics()),
            [
                (DiagnosticCode::RevisionOmitted, Region::Body, None, 1),
                (DiagnosticCode::HiddenOmitted, Region::Body, None, 1),
            ]
        );
    }

    #[test]
    fn reply_nesting_and_comment_assets_obey_limits() {
        let mut chain = annotation("c0", None, "<text:p>root</text:p>");
        for index in 1..12 {
            chain.push_str(&annotation(
                &format!("c{index}"),
                Some(&format!("c{}", index - 1)),
                "<text:p>reply</text:p>",
            ));
        }
        let limits = crate::Limits { max_nesting_depth: 8, ..crate::Limits::DEFAULT };
        let error = parse(
            &ParseCtx::new(&limits).with_comments(CommentPolicy::Emit),
            &package("text", "", &chain),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::ResourceLimit { limit: crate::Limit::NestingDepth, max: 8 }
        ));
        let image = annotation(
            "image",
            None,
            r#"<text:p><draw:frame><draw:image xlink:href="Pictures/p.png"/></draw:frame></text:p>"#,
        );
        let limits = crate::Limits { max_asset_total_bytes: 0, ..crate::Limits::DEFAULT };
        let error = parse(
            &ParseCtx::new(&limits).with_comments(CommentPolicy::Emit),
            &package("text", "", &image),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::ResourceLimit { limit: crate::Limit::AssetTotalBytes, max: 0 }
        ));
    }

    #[test]
    fn omitted_annotations_only_count_references_without_importing_bodies() {
        let first = annotation(
            "duplicate",
            Some("absent"),
            r#"<text:p><draw:frame><draw:image xlink:href="Pictures/p.png"/></draw:frame></text:p>"#,
        );
        let duplicate = annotation("duplicate", Some("duplicate"), "<text:p>loser</text:p>");
        let limits = crate::Limits { max_asset_total_bytes: 0, ..crate::Limits::DEFAULT };
        let ctx = ParseCtx::new(&limits);
        let doc =
            parse(&ctx, &package("text", "", &format!("<text:p>main{first}{duplicate}</text:p>")))
                .unwrap();
        assert_eq!(doc.notes, []);
        assert_eq!(doc.assets, []);
        assert_eq!(
            diagnostic_rows::<(DiagnosticCode, Region, Option<u32>, u64)>(&ctx.into_diagnostics()),
            [(DiagnosticCode::CommentOmitted, Region::Comment, None, 2)]
        );
    }

    #[test]
    fn anonymous_and_extended_annotations_keep_distinct_source_identities() {
        let page = "<draw:page><office:annotation><text:p>standard</text:p></office:annotation><officeooo:annotation><text:p>extended</text:p></officeooo:annotation></draw:page>";
        let (doc, losses) = emitted("presentation", "", &format!("{page}{page}"));
        assert_eq!(losses, []);
        assert_eq!(
            doc.notes.iter().map(|note| note.id.as_str()).collect::<Vec<_>>(),
            ["odf:c:a:0", "odf:c:a:1", "odf:c:a:2", "odf:c:a:3"]
        );
        assert_eq!(doc.notes.len(), 4);
    }

    #[test]
    fn a_cached_header_reuses_comments_and_keeps_note_references_isolated() {
        let comment = annotation("header", None, "<text:p>header comment</text:p>");
        let side_note = r#"<text:note text:id="shared"><text:note-body><text:p><draw:frame><draw:image xlink:href="Pictures/p.png"/></draw:frame>side body</text:p></text:note-body></text:note>"#;
        let styles = format!(
            r#"<office:document-styles {NS}><office:master-styles><style:master-page style:name="M"><style:header><text:p>head{comment}{side_note}</text:p></style:header></style:master-page></office:master-styles></office:document-styles>"#
        );
        let main_note = r#"<text:note text:id="shared"><text:note-body><text:p>main note</text:p></text:note-body></text:note>"#;
        let cell = |value: &str| {
            format!(
                "<table:table><table:table-row><table:table-cell><text:p>{value}</text:p></table:table-cell></table:table-row></table:table>"
            )
        };
        let content = format!(
            "<office:document-content {NS}><office:body><office:spreadsheet>{}{}</office:spreadsheet></office:body></office:document-content>",
            cell(main_note),
            cell("second")
        );
        let bytes = zip_package(&[
            ("content.xml", content.as_bytes()),
            ("styles.xml", styles.as_bytes()),
            ("Pictures/p.png", &[1, 2, 3]),
        ]);
        let limits = crate::Limits { max_asset_total_bytes: 0, ..crate::Limits::DEFAULT };
        let ctx = ParseCtx::new(&limits)
            .with_comments(CommentPolicy::Emit)
            .with_headers_footers(crate::HeaderFooterPolicy::Emit);
        let doc = parse(&ctx, &bytes).unwrap();
        assert_eq!(
            doc.notes.iter().map(|note| note.id.as_str()).collect::<Vec<_>>(),
            ["odf:n:shared", "odf:c:n:header"]
        );
        assert_eq!(doc.assets, []);
        assert_eq!(diagnostic_rows::<Reported>(&ctx.into_diagnostics()), []);
        assert!(!format!("{:?}", doc.notes).contains("side body"));
    }

    #[test]
    fn a_visible_reply_selects_its_hidden_parent_thread_only() {
        let root = annotation("root", None, "<text:p>hidden parent body</text:p>");
        let sibling = annotation("sibling", Some("root"), "<text:p>hidden sibling body</text:p>");
        let unrelated = annotation(
            "unrelated",
            Some("absent"),
            r#"<text:p><draw:frame><draw:image xlink:href="Pictures/p.png"/></draw:frame></text:p>"#,
        );
        let reply = annotation("visible", Some("root"), "<text:p>visible reply</text:p>");
        let body = format!(
            r#"<text:section text:display="none"><text:p>hidden text{root}{sibling}{unrelated}</text:p></text:section><text:p>shown{reply}</text:p>"#
        );
        let limits = crate::Limits { max_asset_total_bytes: 0, ..crate::Limits::DEFAULT };
        let ctx = ParseCtx::new(&limits).with_comments(CommentPolicy::Emit);
        let doc = parse(&ctx, &package("text", "", &body)).unwrap();
        assert_eq!(doc.notes.len(), 1);
        assert_eq!(doc.notes[0].id, "odf:c:n:root");
        assert!(matches!(
            doc.notes[0].blocks.as_slice(),
            [Block::Paragraph(_), Block::BlockQuote(_), Block::BlockQuote(_)]
        ));
        assert_eq!(doc.assets, []);
        assert_eq!(
            diagnostic_rows::<Reported>(&ctx.into_diagnostics()),
            [(DiagnosticCode::HiddenOmitted, Region::Body, None, 1)]
        );
        assert!(
            matches!(&doc.blocks[0], Block::Paragraph(inlines) if matches!(inlines.last(), Some(Inline::CommentRef(id)) if id == "odf:c:n:root"))
        );
    }

    #[test]
    fn a_visible_reply_keeps_a_rejected_parent_and_its_inherited_body_revisions() {
        let root = annotation(
            "root",
            None,
            r#"<text:p>old<text:change-start text:change-id="body-change"/>new<text:change-end text:change-id="body-change"/></text:p>"#,
        );
        let reply = annotation("reply", Some("root"), "<text:p>reply</text:p>");
        let definitions = r#"<text:tracked-changes><text:changed-region text:id="anchor-change"><text:insertion/></text:changed-region><text:changed-region text:id="body-change"><text:insertion/></text:changed-region></text:tracked-changes>"#;
        let body = format!(
            r#"{definitions}<text:p><text:change-start text:change-id="anchor-change"/>{root}<text:change-end text:change-id="anchor-change"/>shown{reply}</text:p>"#
        );
        let ctx = ParseCtx::default()
            .with_comments(CommentPolicy::Emit)
            .with_revisions(crate::RevisionPolicy::Reject);
        let doc = parse(&ctx, &package("text", "", &body)).unwrap();
        assert_eq!(doc.notes.len(), 1);
        assert_eq!(doc.notes[0].id, "odf:c:n:root");
        assert_eq!(doc.notes[0].blocks[0], Block::Paragraph(vec![Inline::plain("old")]));
        assert!(matches!(&doc.notes[0].blocks[1], Block::BlockQuote(_)));
        assert_eq!(
            diagnostic_rows::<Reported>(&ctx.into_diagnostics()),
            [
                (DiagnosticCode::RevisionOmitted, Region::Body, None, 1),
                (DiagnosticCode::RevisionOmitted, Region::Comment, None, 1)
            ]
        );
    }

    #[test]
    fn retained_deletion_copies_reuse_the_original_annotation_definition() {
        let comment = annotation("retained", None, "<text:p>one definition</text:p>");
        let body = format!(
            r#"<text:tracked-changes><text:changed-region text:id="d"><text:deletion><text:p>{comment}</text:p></text:deletion></text:changed-region></text:tracked-changes><text:p>a<text:change text:change-id="d"/>b<text:change text:change-id="d"/></text:p>"#
        );
        let ctx = ParseCtx::default()
            .with_comments(CommentPolicy::Emit)
            .with_revisions(crate::RevisionPolicy::Reject);
        let doc = parse(&ctx, &package("text", "", &body)).unwrap();
        assert_eq!(doc.notes.len(), 1);
        assert_eq!(doc.notes[0].id, "odf:c:n:retained");
        assert_eq!(doc.notes[0].blocks, [Block::Paragraph(vec![Inline::plain("one definition")])]);
        assert_eq!(diagnostic_rows::<Reported>(&ctx.into_diagnostics()), []);
    }

    #[test]
    fn selected_comment_damage_is_isolated_from_body_and_unread_definitions() {
        let root = annotation(
            "root",
            None,
            "<dc:creator>&author;</dc:creator><dc:date>&date;</dc:date><text:p>comment&visible;</text:p>",
        );
        let duplicate = annotation("root", Some("absent"), "<text:p>loser&loser;</text:p>");
        let unselected =
            annotation("unselected", Some("absent"), "<text:p>hidden&hidden;</text:p>");
        let body = format!(
            r#"<text:p>main&body;{root}{duplicate}</text:p><text:section text:display="none"><text:p>{unselected}</text:p></text:section>"#
        );
        let (doc, losses) = emitted("text", "", &body);
        assert_eq!(doc.notes.len(), 1);
        assert_eq!(
            losses
                .iter()
                .filter(|row| row.0 == DiagnosticCode::UnresolvedEntity)
                .copied()
                .collect::<Vec<_>>(),
            [
                (DiagnosticCode::UnresolvedEntity, Region::Body, None, 1),
                (DiagnosticCode::UnresolvedEntity, Region::Comment, None, 1)
            ]
        );
        assert_eq!(
            losses
                .iter()
                .filter(|row| row.0 == DiagnosticCode::DuplicateNote)
                .copied()
                .collect::<Vec<_>>(),
            [(DiagnosticCode::DuplicateNote, Region::Comment, None, 1)]
        );
        assert_eq!(losses.len(), 3);
        assert!(!format!("{:?}", doc.notes).contains("loser"));
    }
}
