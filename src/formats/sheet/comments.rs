use super::grid::{CellRef, SheetContent};
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Block, Inline};
use crate::package::Package;
use crate::package::archive::Role;
use crate::package::relationships::{Relationships, rel_target_path};
use crate::package::xml::{Element, Node, ns};
use crate::shared::text::clean_text;
use std::collections::{BTreeMap, BTreeSet};

const LEGACY_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";
const THREADED_REL: &str =
    "http://schemas.microsoft.com/office/2017/10/relationships/threadedComment";
const THREADED: &str = "http://schemas.microsoft.com/office/spreadsheetml/2018/threadedcomments";

#[derive(Debug)]
pub(super) struct Comment {
    pub(super) at: Option<CellRef>,
    pub(super) blocks: Vec<Block>,
    pub(super) losses: Vec<DiagnosticCode>,
}

impl Comment {
    pub(super) fn text(at: Option<CellRef>, text: String) -> Self {
        Self { at, blocks: vec![Block::Paragraph(vec![Inline::plain(text)])], losses: Vec::new() }
    }
}

#[derive(Debug)]
pub(super) struct Legacy {
    pub(super) comment: Comment,
    pub(super) placeholder: Option<String>,
}

struct Thread {
    id: String,
    parent: Option<String>,
    at: Option<CellRef>,
    text: Option<Element>,
}

pub(super) fn read(
    ctx: &ParseCtx<'_>,
    pkg: &Package<'_>,
    base: &str,
    rels: &Relationships,
    sheet: &mut SheetContent,
) -> Result<(), ConvertError> {
    let mut legacy = Vec::new();
    let mut threaded = Vec::new();
    let mut targets = BTreeSet::new();
    for (id, rel) in
        rels.iter().filter(|(_, rel)| matches!(rel.rel_type.as_str(), LEGACY_REL | THREADED_REL))
    {
        ctx.charge(id.len() as u64 + 1)?;
        if !ctx.comments_requested() {
            ctx.emit(DiagnosticCode::CommentOmitted, Region::Comment);
            continue;
        }
        let Some(target) = rel_target_path(rels, base, id) else {
            ctx.emit(DiagnosticCode::PartUnreadable, Region::Comment);
            continue;
        };
        ctx.charge(target.len() as u64)?;
        if !targets.insert(target.clone()) {
            continue;
        }
        let Some(bytes) = pkg.related_part(&target, Role::Content(Region::Comment))? else {
            continue;
        };
        if rel.rel_type == LEGACY_REL && !is_xml(&bytes) {
            match super::xlsb::read_comments(ctx, &bytes) {
                Ok(comments) => legacy.extend(comments),
                Err(e) if e.is_fatal() => return Err(e),
                Err(_) => ctx.emit(DiagnosticCode::PartUnreadable, Region::Comment),
            }
            continue;
        }
        let root = pkg.related_xml_part_streamed(
            &target,
            Role::Content(Region::Comment),
            &mut super::SelectedTextTree,
        )?;
        if let Some(root) = root {
            if rel.rel_type == THREADED_REL {
                read_threaded(ctx, root, &mut threaded)?;
            } else {
                read_legacy(ctx, &root, &mut legacy)?;
            }
        }
    }
    let mut roots = group_threads(ctx, threaded, &legacy, sheet)?;
    for Legacy { comment, placeholder } in legacy {
        if let Some(root) = placeholder.and_then(|id| roots.get_mut(&id)) {
            if comment.at.is_some() {
                root.at = comment.at;
            }
        } else {
            sheet.comment(comment)?;
        }
    }
    for comment in roots.into_values() {
        sheet.comment(comment)?;
    }
    Ok(())
}

fn is_xml(bytes: &[u8]) -> bool {
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    matches!(body.iter().find(|b| !b.is_ascii_whitespace()), Some(b'<' | 0xFF | 0xFE))
}

fn read_legacy(
    ctx: &ParseCtx<'_>,
    tree: &Element,
    out: &mut Vec<Legacy>,
) -> Result<(), ConvertError> {
    let Some(root) = tree.find(ns::SML, "comments") else {
        ctx.emit(DiagnosticCode::PartUnreadable, Region::Comment);
        return Ok(());
    };
    let authors: Vec<String> = root
        .find(ns::SML, "authors")
        .into_iter()
        .flat_map(|authors| authors.find_all(ns::SML, "author"))
        .map(|author| author.checked_text(ctx))
        .collect::<Result<_, _>>()?;
    for entry in root
        .find(ns::SML, "commentList")
        .into_iter()
        .flat_map(|list| list.find_all(ns::SML, "comment"))
    {
        ctx.charge(1)?;
        let at = entry.attr_unqualified("ref").and_then(super::xlsx::parse_ref);
        let text = entry.find(ns::SML, "text");
        let mut comment = comment_text(ctx, at, text)?;
        if text.is_none() {
            comment.losses.push(DiagnosticCode::PartUnreadable);
        }
        let author = entry
            .attr_unqualified("authorId")
            .and_then(|id| id.parse::<usize>().ok())
            .and_then(|id| authors.get(id));
        if let Some(author) = author {
            ctx.charge(author.len() as u64)?;
        }
        let placeholder =
            author.and_then(|author| author.strip_prefix("tc=")).map(str::to_ascii_lowercase);
        out.push(Legacy { comment, placeholder });
    }
    Ok(())
}

fn read_threaded(
    ctx: &ParseCtx<'_>,
    tree: Element,
    out: &mut Vec<Thread>,
) -> Result<(), ConvertError> {
    let Some(root) = tree
        .children
        .into_iter()
        .filter_map(|node| match node {
            Node::Elem(element) => Some(element),
            Node::Text(_) => None,
        })
        .find(|element| element.is(THREADED, "ThreadedComments"))
    else {
        ctx.emit(DiagnosticCode::PartUnreadable, Region::Comment);
        return Ok(());
    };
    for node in root.children {
        let Node::Elem(entry) = node else { continue };
        if !entry.is(THREADED, "threadedComment") {
            continue;
        }
        ctx.charge(1)?;
        let Some(id) = entry.attr_unqualified("id").filter(|id| !id.is_empty()) else {
            ctx.emit(DiagnosticCode::PartUnreadable, Region::Comment);
            continue;
        };
        ctx.charge(id.len() as u64)?;
        let id = id.to_ascii_lowercase();
        let at = entry.attr_unqualified("ref").and_then(super::xlsx::parse_ref);
        let parent = entry.attr_unqualified("parentId");
        ctx.charge(parent.map_or(0, |parent| parent.len() as u64))?;
        let parent = parent.map(str::to_ascii_lowercase);
        let text = entry.children.into_iter().find_map(|node| match node {
            Node::Elem(element) if element.is(THREADED, "text") => Some(element),
            _ => None,
        });
        out.push(Thread { id, parent, at, text });
    }
    Ok(())
}

fn comment_text(
    ctx: &ParseCtx<'_>,
    at: Option<CellRef>,
    text: Option<&Element>,
) -> Result<Comment, ConvertError> {
    let mut value = String::new();
    let mut damaged = false;
    if let Some(text) = text {
        for run in super::xlsx::rich_runs(text) {
            value.push_str(&run.checked_text(ctx)?);
            damaged |= super::text_damaged(run);
        }
    }
    let mut comment = Comment::text(at, clean_text(&value));
    if damaged {
        comment.losses.push(DiagnosticCode::UnresolvedEntity);
    }
    Ok(comment)
}

fn group_threads(
    ctx: &ParseCtx<'_>,
    entries: Vec<Thread>,
    legacy: &[Legacy],
    sheet: &SheetContent,
) -> Result<BTreeMap<String, Comment>, ConvertError> {
    let mut anchors = BTreeMap::new();
    let mut bodies = Vec::new();
    let mut source = Vec::new();
    for Thread { id, parent, at, text } in entries {
        ctx.charge(id.len() as u64 + 1)?;
        anchors.entry(id.clone()).or_insert(at);
        bodies.push(text);
        source.push(crate::shared::comments::SourceComment { id, parent, blocks: Vec::new() });
    }
    let mut plan = crate::shared::comments::CommentThreads::new(ctx, &source, None)?;
    let mut roots = BTreeMap::new();
    for id in plan.aliases().values() {
        ctx.charge(id.len() as u64 + 1)?;
        roots.insert(id.clone(), anchors.get(id).copied().flatten());
    }
    for entry in legacy {
        if let Some(id) = &entry.placeholder {
            ctx.charge(id.len() as u64 + 1)?;
            if let Some(at) = roots.get_mut(id)
                && entry.comment.at.is_some()
            {
                *at = entry.comment.at;
            }
        }
    }
    plan.select(ctx, &source, &sheet.visible_comments(ctx, &roots)?)?;
    plan.admit(ctx)?;
    let mut losses = BTreeMap::<String, Vec<DiagnosticCode>>::new();
    for (index, (comment, text)) in source.iter_mut().zip(bodies).enumerate() {
        if !plan.includes(index) {
            continue;
        }
        let value =
            text.as_ref().map(|text| text.checked_text(ctx)).transpose()?.unwrap_or_default();
        comment.blocks = Comment::text(None, clean_text(&value)).blocks;
        if text.as_ref().is_some_and(super::text_damaged) {
            ctx.charge(comment.id.len() as u64)?;
            let root = plan.aliases().get(&comment.id).ok_or(ConvertError::Internal)?;
            ctx.charge(root.len() as u64)?;
            losses.entry(root.clone()).or_default().push(DiagnosticCode::UnresolvedEntity);
        }
    }
    let (notes, _) = plan.finish(ctx, source)?;
    let mut grouped: BTreeMap<_, _> = roots
        .into_iter()
        .map(|(id, at)| {
            let losses = losses.remove(&id).unwrap_or_default();
            (id, Comment { at, blocks: Vec::new(), losses })
        })
        .collect();
    for note in notes {
        ctx.charge(note.id.len() as u64)?;
        grouped.get_mut(&note.id).ok_or(ConvertError::Internal)?.blocks = note.blocks;
    }
    Ok(grouped)
}
