use super::content::{self, Ctx, References};
use super::numbering::Counters;
use super::typed_part_path;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Block, Note};
use crate::package::archive::Role;
use crate::package::relationships::{read_rels, rels_part_for};
use crate::package::xml::{Element, ns};
use crate::shared::comments::{CommentThreads, SourceComment};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

const COMMENTS: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";
const EXTENDED: &str = "http://schemas.microsoft.com/office/2011/relationships/commentsExtended";
const W14: &str = "http://schemas.microsoft.com/office/word/2010/wordml";
const W15: &str = "http://schemas.microsoft.com/office/word/2012/wordml";

#[derive(Default)]
pub(super) struct Scopes {
    counters: RefCell<Counters>,
    references: References,
}

pub(super) fn append<'b>(
    ctx: &Ctx<'_, 'b>,
    scopes: &'b Scopes,
    side_ids: Vec<String>,
    blocks: &mut [Block],
    notes: &mut Vec<Note>,
) -> Result<(), ConvertError> {
    if !ctx.parse_ctx.comments_requested() {
        ctx.parse_ctx.emit_count(
            DiagnosticCode::CommentOmitted,
            Region::Comment,
            ctx.references.comments(),
        );
        return Ok(());
    }
    let ids = ctx.references.comment_ids().into_iter().chain(side_ids);
    let (comments, roots) = read(ctx, scopes, ids)?;
    crate::shared::comments::remap_references(ctx.parse_ctx, blocks, &roots)?;
    for note in &mut *notes {
        crate::shared::comments::remap_references(ctx.parse_ctx, &mut note.blocks, &roots)?;
    }
    notes.extend(comments);
    Ok(())
}

fn read<'b>(
    ctx: &Ctx<'_, 'b>,
    scopes: &'b Scopes,
    ids: impl Iterator<Item = String>,
) -> Result<(Vec<Note>, BTreeMap<String, String>), ConvertError> {
    let ids: BTreeSet<_> = ids.collect();
    if ids.is_empty() {
        return Ok((Vec::new(), BTreeMap::new()));
    }
    let part = typed_part_path(ctx.parse_ctx, &ctx.rels, &ctx.base_part, COMMENTS, "comments.xml");
    let role = Role::DeferredContent(Region::Comment);
    let (tree, reported) = part.read(ctx.parse_ctx, ctx.pkg, role, "comments")?;
    let Some(tree) = tree else {
        if !reported {
            ctx.parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Comment);
        }
        return Ok((Vec::new(), BTreeMap::new()));
    };
    let Some(root) = tree.find(ns::W, "comments") else {
        return Ok((Vec::new(), BTreeMap::new()));
    };
    let elements: Vec<_> = root.find_all(ns::W, "comment").collect();
    if !references_definition(ctx, &ids, &elements)? {
        return Ok((Vec::new(), BTreeMap::new()));
    }
    let parents = parents(ctx, root)?;
    let mut comments = Vec::new();
    for element in &elements {
        let id = element.attr(ns::W, "id").unwrap_or_default();
        ctx.parse_ctx.charge(id.len() as u64)?;
        let parent = parents.get(id);
        ctx.parse_ctx.charge(parent.map_or(0, String::len) as u64)?;
        comments.push(SourceComment {
            id: id.to_owned(),
            parent: parent.cloned(),
            blocks: Vec::new(),
        });
    }
    let threads = CommentThreads::new(ctx.parse_ctx, &comments, Some(&ids))?;
    threads.admit(ctx.parse_ctx)?;
    if (0..comments.len()).any(|index| threads.includes(index)) {
        let rels = read_rels(ctx.pkg, &rels_part_for(&part.path), role)?;
        let comment_ctx =
            ctx.for_side((rels, part.path, Region::Comment), &scopes.counters, &scopes.references);
        for (index, (comment, element)) in comments.iter_mut().zip(elements).enumerate() {
            if !threads.includes(index) {
                continue;
            }
            scopes.counters.replace(Counters::default());
            comment_ctx.moves.replace(content::Moves::default());
            comment_ctx.damage.reset();
            comment.blocks = content::parse_blocks(element, &comment_ctx)?;
            comment_ctx.settle_moves()?;
        }
    }
    threads.finish(ctx.parse_ctx, comments)
}

fn references_definition(
    ctx: &Ctx<'_, '_>,
    ids: &BTreeSet<String>,
    elements: &[&Element],
) -> Result<bool, ConvertError> {
    for element in elements {
        let id = element.attr(ns::W, "id").unwrap_or_default();
        ctx.parse_ctx.charge(id.len() as u64 + 1)?;
        if ids.contains(id) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parents(ctx: &Ctx<'_, '_>, root: &Element) -> Result<BTreeMap<String, String>, ConvertError> {
    let part =
        typed_part_path(ctx.parse_ctx, &ctx.rels, &ctx.base_part, EXTENDED, "commentsExtended.xml");
    let role = Role::DeferredContent(Region::Comment);
    let tree = if part.related {
        ctx.pkg.related_xml_part(&part.path, role)?
    } else {
        ctx.pkg.optional_xml_part(&part.path, role)?
    };
    let Some(tree) = tree else {
        return Ok(BTreeMap::new());
    };
    let Some(tree) = tree.find(W15, "commentsEx") else {
        ctx.parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Comment);
        return Ok(BTreeMap::new());
    };
    let mut paragraphs = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for comment in root.find_all(ns::W, "comment") {
        let id = comment.attr(ns::W, "id").unwrap_or_default();
        ctx.parse_ctx.charge(id.len() as u64)?;
        if !seen.insert(id) {
            continue;
        }
        if let Some(para) =
            comment.descendants(ns::W, "p").last().and_then(|p| p.attr(W14, "paraId"))
        {
            ctx.parse_ctx.charge(para.len().saturating_add(id.len()) as u64)?;
            paragraphs.entry(para).or_insert(id);
        }
    }
    let mut parents = BTreeMap::new();
    for comment in tree.descendants(W15, "commentEx") {
        if let (Some(id), Some(parent)) =
            (comment.attr(W15, "paraId"), comment.attr(W15, "paraIdParent"))
        {
            ctx.parse_ctx.charge(id.len().saturating_add(parent.len()) as u64)?;
            if let Some(id) = paragraphs.get(id) {
                let resolved = paragraphs.get(parent).copied();
                let length = resolved
                    .map_or_else(|| "unresolved:".len().saturating_add(parent.len()), str::len);
                ctx.parse_ctx.charge(id.len().saturating_add(length) as u64)?;
                let parent = resolved.map_or_else(|| format!("unresolved:{parent}"), str::to_owned);
                parents.entry((*id).to_owned()).or_insert(parent);
            }
        }
    }
    Ok(parents)
}
