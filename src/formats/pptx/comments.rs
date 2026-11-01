use super::{COMMENT_RELS, SlideCtx, parse_text_body};
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Block, Inline, Note, Unit};
use crate::package::archive::Role;
use crate::package::path;
use crate::package::relationships::{Relationship, TargetMode, read_rels, rels_part_for};
use crate::package::xml::{Element, ns};
use crate::shared::comments::{CommentThreads, SourceComment};
use crate::shared::xml::DamageScope;

const MODERN: &str = "http://schemas.microsoft.com/office/powerpoint/2018/8/main";
const THREADING: &str = "http://schemas.microsoft.com/office/powerpoint/2012/main";

pub(super) fn read(
    ctx: &SlideCtx<'_, '_>,
    unit: Unit,
    blocks: &mut Vec<Block>,
    notes: &mut Vec<Note>,
) -> Result<(), ConvertError> {
    if !ctx.parse_ctx.comments_requested() {
        return Ok(());
    }
    let mut references = Vec::new();
    let rels = ctx.rels.iter().filter(|(_, rel)| COMMENT_RELS.contains(&rel.rel_type.as_str()));
    for (index, (_, rel)) in rels.enumerate() {
        let prefix = format!("slide-{}-comment-{index}", unit.ordinal);
        read_part(ctx, rel, &prefix, &mut references, notes)?;
    }
    if !references.is_empty() {
        blocks.push(Block::Paragraph(references));
    }
    Ok(())
}

fn read_part(
    ctx: &SlideCtx<'_, '_>,
    rel: &Relationship,
    prefix: &str,
    references: &mut Vec<Inline>,
    notes: &mut Vec<Note>,
) -> Result<(), ConvertError> {
    ctx.parse_ctx.charge(rel.target.len() as u64 + 1)?;
    let target = path::resolve(ctx.base_part, &rel.target);
    let Some(target) = target.ok().filter(|_| rel.mode == TargetMode::Internal) else {
        ctx.parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Comment);
        return Ok(());
    };
    let role = Role::DeferredContent(Region::Comment);
    let Some(tree) = ctx.pkg.related_xml_part(&target.path, role)? else {
        return Ok(());
    };
    let Some(root) = tree.find(ns::P, "cmLst").or_else(|| tree.find(MODERN, "cmLst")) else {
        ctx.parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Comment);
        return Ok(());
    };
    let (mut sources, elements) = index(ctx, root, prefix)?;
    let plan = CommentThreads::new(ctx.parse_ctx, &sources, None)?;
    plan.admit(ctx.parse_ctx)?;
    let rels = read_rels(ctx.pkg, &rels_part_for(&target.path), role)?;
    let damage = DamageScope::default();
    let comment_ctx = SlideCtx {
        rels: &rels,
        base_part: &target.path,
        region: Region::Comment,
        layout: None,
        master: None,
        sides: None,
        damage: Some(&damage),
        ..*ctx
    };
    for (index, (source, element)) in sources.iter_mut().zip(elements).enumerate() {
        if !plan.includes(index) {
            continue;
        }
        damage.reset();
        source.blocks = body(element, &comment_ctx)?;
    }
    let (comments, _) = plan.finish(ctx.parse_ctx, sources)?;
    for note in &comments {
        ctx.parse_ctx.charge(note.id.len() as u64)?;
        references.push(Inline::CommentRef(note.id.clone()));
    }
    notes.extend(comments);
    Ok(())
}

fn index<'e>(
    ctx: &SlideCtx<'_, '_>,
    root: &'e Element,
    prefix: &str,
) -> Result<(Vec<SourceComment>, Vec<&'e Element>), ConvertError> {
    let namespace = root.ns.as_deref().unwrap_or(ns::P);
    let mut pending: Vec<_> =
        root.find_all(namespace, "cm").map(|comment| (comment, None)).collect();
    pending.reverse();
    let (mut sources, mut elements) = (Vec::new(), Vec::new());
    while let Some((comment, parent)) = pending.pop() {
        ctx.parse_ctx.charge(1)?;
        let id = source_id(ctx, comment, prefix, sources.len(), namespace)?;
        let parent = if namespace == ns::P {
            comment.first_descendant(THREADING, "parentCm").map(|parent| {
                legacy_key(parent).map_or_else(
                    || format!("{prefix}-unread-parent"),
                    |key| format!("{prefix}-{key}"),
                )
            })
        } else {
            parent
        };
        if let Some(replies) = comment.find(MODERN, "replyLst") {
            let replies: Vec<_> = replies.find_all(MODERN, "reply").collect();
            for reply in replies.into_iter().rev() {
                ctx.parse_ctx.charge(id.len() as u64)?;
                pending.push((reply, Some(id.clone())));
            }
        }
        sources.push(SourceComment { id, parent, blocks: Vec::new() });
        elements.push(comment);
    }
    Ok((sources, elements))
}

fn source_id(
    ctx: &SlideCtx<'_, '_>,
    comment: &Element,
    prefix: &str,
    index: usize,
    namespace: &str,
) -> Result<String, ConvertError> {
    if namespace == ns::P {
        return Ok(legacy_key(comment)
            .map_or_else(|| format!("{prefix}-comment-{index}"), |key| format!("{prefix}-{key}")));
    }
    if let Some(id) = comment.attr_any("id").filter(|id| !id.is_empty()) {
        ctx.parse_ctx.charge(prefix.len().saturating_add(id.len()) as u64)?;
        Ok(format!("{prefix}-{id}"))
    } else {
        Ok(format!("{prefix}-comment-{index}"))
    }
}

fn legacy_key(comment: &Element) -> Option<String> {
    let author: u32 = comment.attr(ns::P, "authorId")?.parse().ok()?;
    let index: u32 = comment.attr(ns::P, "idx")?.parse().ok()?;
    Some(format!("legacy-{author}-{index}"))
}

fn body(comment: &Element, ctx: &SlideCtx<'_, '_>) -> Result<Vec<Block>, ConvertError> {
    let mut blocks = Vec::new();
    if let Some(text) = comment.find(ns::P, "text") {
        let value = text.checked_text(ctx.parse_ctx)?;
        if let Some(damage) = ctx.damage {
            damage.report(
                ctx.parse_ctx,
                Region::Comment,
                text.unresolved_text && !value.trim().is_empty(),
            );
        }
        blocks.push(Block::Paragraph(vec![Inline::plain(value)]));
    } else if let Some(text) = comment.find(MODERN, "txBody") {
        parse_text_body(text, ctx, None, &mut blocks);
    }
    Ok(blocks)
}
