use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::model::{Block, Gap, HeaderFooter, Inline, Node, Note, NoteKind, Unit};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct ReferenceUnits {
    pub(crate) first: Option<Unit>,
    pub(crate) all: BTreeSet<Option<Unit>>,
}

#[expect(
    clippy::too_many_lines,
    reason = "iterative dispatch over containers and isolated body, side and ordinary note references"
)]
pub(crate) fn reference_units<'d>(
    blocks: &'d [Block],
    definitions: &'d [Note],
    filled: impl Fn(&'d Gap) -> Option<&'d [Block]>,
    charge: impl Fn(u64) -> Result<(), ConvertError>,
) -> Result<BTreeMap<&'d str, ReferenceUnits>, ConvertError> {
    let mut notes = BTreeMap::new();
    for note in definitions.iter().filter(|note| note.kind != NoteKind::Comment) {
        charge(note.id.len() as u64 + 1)?;
        notes.entry(note.id.as_str()).or_insert(note);
    }
    let mut current = None;
    let mut pending = Vec::new();
    for block in blocks {
        if let Block::UnitStart(unit) = block {
            current = Some(*unit);
        }
        pending.push((Node::Blocks(std::slice::from_ref(block)), current, true));
    }
    pending.reverse();
    let mut unused = definitions.iter().filter(|note| note.kind != NoteKind::Comment);
    let mut seen = BTreeSet::new();
    let mut referenced = BTreeSet::new();
    let mut units = BTreeMap::new();
    loop {
        if pending.is_empty() {
            let Some(note) = unused.next() else { break };
            charge(note.id.len() as u64)?;
            if referenced.contains(note.id.as_str()) || !seen.insert((note.id.as_str(), None)) {
                continue;
            }
            pending.push((Node::Blocks(&note.blocks), None, true));
        }
        let Some((node, unit, body_notes)) = pending.pop() else { break };
        charge(1)?;
        match node {
            Node::Blocks(blocks) => {
                charge(blocks.len() as u64)?;
                for block in blocks.iter().rev() {
                    let body_notes = body_notes && !matches!(block, Block::HeaderFooter(_));
                    pending.extend(
                        crate::model::children(block, &filled)
                            .into_iter()
                            .rev()
                            .map(|child| (child, unit, body_notes)),
                    );
                }
            }
            Node::Inlines(inlines) => pending.extend(
                inlines.iter().rev().map(|inline| (Node::Inline(inline), unit, body_notes)),
            ),
            Node::Inline(Inline::CommentRef(id)) => {
                charge(id.len() as u64)?;
                units
                    .entry(id.as_str())
                    .or_insert_with(|| ReferenceUnits { first: unit, all: BTreeSet::new() })
                    .all
                    .insert(unit);
            }
            Node::Inline(Inline::NoteRef(id)) if body_notes => {
                charge(id.len() as u64)?;
                referenced.insert(id.as_str());
                if seen.insert((id.as_str(), unit))
                    && let Some(note) = notes.get(id.as_str())
                {
                    pending.push((Node::Blocks(&note.blocks), unit, true));
                }
            }
            Node::Inline(Inline::Link { content, .. }) => {
                pending.push((Node::Inlines(content), unit, body_notes));
            }
            Node::Inline(_) => {}
            Node::Cells(mut cells) => {
                if let Some(cell) = cells.next() {
                    pending.push((Node::Cells(cells), unit, body_notes));
                    pending.push((Node::Blocks(&cell.blocks), unit, body_notes));
                }
            }
        }
    }
    Ok(units)
}

pub(crate) fn remap_references(
    ctx: &ParseCtx<'_>,
    blocks: &mut [Block],
    roots: &BTreeMap<String, String>,
) -> Result<(), ConvertError> {
    let mut pending = vec![Content::Blocks(blocks)];
    while let Some(content) = pending.pop() {
        match content {
            Content::Blocks(blocks) => {
                ctx.charge(blocks.len() as u64)?;
                for block in blocks {
                    match block {
                        Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
                            pending.push(Content::Inlines(inlines));
                        }
                        Block::BlockQuote(inner)
                        | Block::HeaderFooter(HeaderFooter { content: Some(inner), .. }) => {
                            pending.push(Content::Blocks(inner));
                        }
                        Block::List(list) => pending.extend(
                            list.items.iter_mut().map(|item| Content::Blocks(&mut item.blocks)),
                        ),
                        Block::Table(table) => pending.extend(
                            table
                                .grid
                                .iter_mut()
                                .flatten()
                                .filter_map(|slot| match slot {
                                    crate::model::CellSlot::Origin(cell) => Some(cell),
                                    crate::model::CellSlot::Covered { .. } => None,
                                })
                                .map(|cell| Content::Blocks(&mut cell.blocks)),
                        ),
                        _ => {}
                    }
                }
            }
            Content::Inlines(inlines) => {
                ctx.charge(inlines.len() as u64)?;
                for inline in inlines {
                    match inline {
                        Inline::CommentRef(id) => {
                            ctx.charge(id.len() as u64)?;
                            if let Some(root) = roots.get(id) {
                                ctx.charge(root.len() as u64)?;
                                id.clone_from(root);
                            }
                        }
                        Inline::Link { content, .. } => pending.push(Content::Inlines(content)),
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

enum Content<'a> {
    Blocks(&'a mut [Block]),
    Inlines(&'a mut [Inline]),
}

pub(crate) struct SourceComment {
    pub(crate) id: String,
    pub(crate) parent: Option<String>,
    pub(crate) blocks: Vec<Block>,
}

pub(crate) struct CommentThreads {
    parents: Vec<Option<usize>>,
    roots: Vec<usize>,
    selected: BTreeSet<usize>,
    aliases: BTreeMap<String, String>,
    depths: Vec<u64>,
    losses: Vec<ThreadLoss>,
    duplicates: BTreeSet<usize>,
}

impl CommentThreads {
    pub(crate) fn new(
        ctx: &ParseCtx<'_>,
        comments: &[SourceComment],
        selected: Option<&BTreeSet<String>>,
    ) -> Result<Self, ConvertError> {
        let (mut parents, mut losses) = index_comments(ctx, comments)?;
        let mut roots = vec![None; comments.len()];
        let mut depths = vec![0; comments.len()];
        for index in 0..comments.len() {
            resolve_thread(ctx, &mut parents, &mut roots, &mut depths, &mut losses, index)?;
        }
        let roots = roots.into_iter().collect::<Option<Vec<_>>>().ok_or(ConvertError::Internal)?;
        let selected = selected_roots(ctx, comments, &roots, selected)?;
        let mut aliases = BTreeMap::new();
        for (index, comment) in comments.iter().enumerate() {
            let root = comments
                .get(*roots.get(index).ok_or(ConvertError::Internal)?)
                .ok_or(ConvertError::Internal)?;
            ctx.charge(comment.id.len().saturating_add(root.id.len()) as u64)?;
            aliases.insert(comment.id.clone(), root.id.clone());
        }
        let duplicates = losses
            .iter()
            .filter_map(|(index, code)| {
                (*code == crate::DiagnosticCode::DuplicateNote).then_some(*index)
            })
            .collect();
        Ok(Self { parents, roots, selected, aliases, depths, losses, duplicates })
    }

    pub(crate) fn aliases(&self) -> &BTreeMap<String, String> {
        &self.aliases
    }

    pub(crate) fn select(
        &mut self,
        ctx: &ParseCtx<'_>,
        comments: &[SourceComment],
        selected: &BTreeSet<String>,
    ) -> Result<(), ConvertError> {
        if comments.len() != self.roots.len() {
            return Err(ConvertError::Internal);
        }
        self.selected = selected_roots(ctx, comments, &self.roots, Some(selected))?;
        Ok(())
    }

    pub(crate) fn admit(&self, ctx: &ParseCtx<'_>) -> Result<(), ConvertError> {
        let max = ctx.limits().max_nesting_depth;
        for (index, depth) in self.depths.iter().enumerate() {
            ctx.charge(1)?;
            if !self.duplicates.contains(&index)
                && *depth > max
                && self.selected.contains(self.roots.get(index).ok_or(ConvertError::Internal)?)
            {
                return Err(ConvertError::limit(crate::Limit::NestingDepth, max));
            }
        }
        Ok(())
    }

    pub(crate) fn includes(&self, index: usize) -> bool {
        !self.duplicates.contains(&index)
            && self.roots.get(index).is_some_and(|root| self.selected.contains(root))
    }

    fn admit_bodies(
        &self,
        ctx: &ParseCtx<'_>,
        comments: &mut [SourceComment],
    ) -> Result<(), ConvertError> {
        let max = ctx.limits().max_nesting_depth;
        for (index, comment) in comments.iter_mut().enumerate() {
            if !self.includes(index) {
                continue;
            }
            crate::model::charge_blocks_copy(&comment.blocks, |work| ctx.charge(work))?;
            let depth = *self.depths.get(index).ok_or(ConvertError::Internal)?;
            let document = crate::model::Document {
                blocks: std::mem::take(&mut comment.blocks),
                ..crate::model::Document::default()
            };
            let admitted =
                crate::model::check_nesting(&document, |_| None, max.saturating_sub(depth));
            comment.blocks = document.blocks;
            if admitted.is_err() {
                return Err(ConvertError::limit(crate::Limit::NestingDepth, max));
            }
        }
        Ok(())
    }

    pub(crate) fn finish(
        self,
        ctx: &ParseCtx<'_>,
        comments: Vec<SourceComment>,
    ) -> Result<(Vec<Note>, BTreeMap<String, String>), ConvertError> {
        self.finish_with(ctx, comments, |_, code| {
            ctx.emit(code, crate::Region::Comment);
            Ok(())
        })
    }

    pub(crate) fn finish_with(
        self,
        ctx: &ParseCtx<'_>,
        mut comments: Vec<SourceComment>,
        mut report: impl FnMut(usize, crate::DiagnosticCode) -> Result<(), ConvertError>,
    ) -> Result<(Vec<Note>, BTreeMap<String, String>), ConvertError> {
        self.admit(ctx)?;
        self.admit_bodies(ctx, &mut comments)?;
        for (index, code) in &self.losses {
            if self.selected.contains(self.roots.get(*index).ok_or(ConvertError::Internal)?) {
                report(*index, *code)?;
            }
        }
        if comments.len() != self.parents.len() {
            return Err(ConvertError::Internal);
        }
        let mut children = vec![Vec::new(); comments.len()];
        for (index, parent) in self.parents.iter().enumerate() {
            ctx.charge(1)?;
            if self.duplicates.contains(&index) {
                continue;
            }
            if let Some(parent) = parent {
                children.get_mut(*parent).ok_or(ConvertError::Internal)?.push(index);
            }
        }
        let (mut ids, mut blocks): (Vec<_>, Vec<_>) =
            comments.into_iter().map(|comment| (Some(comment.id), Some(comment.blocks))).unzip();
        let mut notes = Vec::new();
        for root in self.selected {
            let mut pending = vec![(root, false)];
            while let Some((index, entered)) = pending.pop() {
                ctx.charge(1)?;
                if !entered {
                    pending.push((index, true));
                    pending.extend(
                        children
                            .get(index)
                            .ok_or(ConvertError::Internal)?
                            .iter()
                            .rev()
                            .map(|child| (*child, false)),
                    );
                    continue;
                }
                let content =
                    blocks.get_mut(index).and_then(Option::take).ok_or(ConvertError::Internal)?;
                if let Some(parent) = self.parents.get(index).copied().flatten() {
                    blocks
                        .get_mut(parent)
                        .and_then(Option::as_mut)
                        .ok_or(ConvertError::Internal)?
                        .push(Block::BlockQuote(content));
                } else {
                    let id =
                        ids.get_mut(index).and_then(Option::take).ok_or(ConvertError::Internal)?;
                    notes.push(Note { id, kind: NoteKind::Comment, blocks: content });
                }
            }
        }
        Ok((notes, self.aliases))
    }
}

type ThreadLoss = (usize, crate::DiagnosticCode);

fn index_comments(
    ctx: &ParseCtx<'_>,
    comments: &[SourceComment],
) -> Result<(Vec<Option<usize>>, Vec<ThreadLoss>), ConvertError> {
    let mut ids = BTreeMap::new();
    for (index, comment) in comments.iter().enumerate() {
        ctx.charge(comment.id.len() as u64 + 1)?;
        ids.entry(comment.id.as_str()).or_insert(index);
    }
    let mut parents = Vec::new();
    let mut losses = Vec::new();
    for (index, comment) in comments.iter().enumerate() {
        ctx.charge(comment.id.len() as u64 + 1)?;
        let first = *ids.get(comment.id.as_str()).ok_or(ConvertError::Internal)?;
        let parent = if first != index {
            losses.push((index, crate::DiagnosticCode::DuplicateNote));
            Some(first)
        } else if let Some(parent) = &comment.parent {
            ctx.charge(parent.len() as u64)?;
            let found = ids.get(parent.as_str()).copied();
            if found.is_none() {
                losses.push((index, crate::DiagnosticCode::DanglingReference));
            }
            found
        } else {
            None
        };
        parents.push(parent);
    }
    Ok((parents, losses))
}

fn selected_roots(
    ctx: &ParseCtx<'_>,
    comments: &[SourceComment],
    roots: &[usize],
    selected: Option<&BTreeSet<String>>,
) -> Result<BTreeSet<usize>, ConvertError> {
    let mut found = BTreeSet::new();
    for (index, comment) in comments.iter().enumerate() {
        ctx.charge(comment.id.len() as u64 + 1)?;
        if selected.is_none_or(|selected| selected.contains(&comment.id)) {
            found.insert(*roots.get(index).ok_or(ConvertError::Internal)?);
        }
    }
    Ok(found)
}

fn resolve_thread(
    ctx: &ParseCtx<'_>,
    parents: &mut [Option<usize>],
    roots: &mut [Option<usize>],
    depths: &mut [u64],
    losses: &mut Vec<ThreadLoss>,
    index: usize,
) -> Result<(), ConvertError> {
    let mut current = index;
    let mut path = Vec::new();
    let mut seen = BTreeSet::new();
    loop {
        ctx.charge(1)?;
        if roots.get(current).copied().flatten().is_some() {
            break;
        }
        if !seen.insert(current) {
            *parents.get_mut(current).ok_or(ConvertError::Internal)? = None;
            losses.push((current, crate::DiagnosticCode::DanglingReference));
        }
        path.push(current);
        if let Some(parent) = parents.get(current).copied().flatten() {
            current = parent;
        } else {
            *roots.get_mut(current).ok_or(ConvertError::Internal)? = Some(current);
            break;
        }
    }
    for step in path.into_iter().rev() {
        if roots.get(step).copied().flatten().is_some() {
            continue;
        }
        let parent = parents.get(step).copied().flatten().ok_or(ConvertError::Internal)?;
        let depth = depths.get(parent).ok_or(ConvertError::Internal)?.saturating_add(1);
        *depths.get_mut(step).ok_or(ConvertError::Internal)? = depth;
        *roots.get_mut(step).ok_or(ConvertError::Internal)? =
            *roots.get(parent).ok_or(ConvertError::Internal)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn threads(
        ctx: &ParseCtx<'_>,
        comments: Vec<SourceComment>,
    ) -> Result<(Vec<Note>, BTreeMap<String, String>), ConvertError> {
        CommentThreads::new(ctx, &comments, None)?.finish(ctx, comments)
    }

    fn source(id: &str, parent: Option<&str>) -> SourceComment {
        SourceComment {
            id: id.into(),
            parent: parent.map(str::to_owned),
            blocks: vec![Block::Paragraph(vec![Inline::plain(id)])],
        }
    }
    #[test]
    fn duplicate_ids_keep_only_the_first_definition_without_reading_losers() {
        let mut first = source("same", None);
        first.blocks = vec![Block::Paragraph(vec![Inline::plain("first")])];
        let mut duplicate = source("same", None);
        duplicate.blocks = vec![Block::Paragraph(vec![Inline::plain("second")])];
        let records = vec![first, duplicate];
        let ctx = ParseCtx::default();
        let plan = CommentThreads::new(&ctx, &records, None).unwrap();
        assert_eq!((plan.includes(0), plan.includes(1)), (true, false));
        let (notes, _) = plan.finish(&ctx, records).unwrap();
        assert_eq!(notes[0].blocks, [Block::Paragraph(vec![Inline::plain("first")])]);
        assert_eq!(
            ctx.diagnostics().iter().map(|d| (d.code, d.count)).collect::<Vec<_>>(),
            [(crate::DiagnosticCode::DuplicateNote, 1)]
        );
    }
    #[test]
    fn reply_depth_and_body_nesting_share_one_bound_before_composition() {
        let mut reply = source("reply", Some("root"));
        reply.blocks = vec![Block::BlockQuote(vec![Block::BlockQuote(reply.blocks)])];
        let limits = crate::Limits { max_nesting_depth: 2, ..crate::Limits::default() };
        assert_eq!(
            threads(&ParseCtx::new(&limits), vec![source("root", None), reply]).err(),
            Some(ConvertError::limit(crate::Limit::NestingDepth, 2))
        );
    }
    #[test]
    fn replies_keep_each_immediate_parent_even_when_source_order_differs() {
        let (notes, roots) = threads(
            &ParseCtx::default(),
            vec![
                source("child", Some("root")),
                source("root", None),
                source("leaf", Some("child")),
            ],
        )
        .unwrap();
        assert_eq!(roots["leaf"], "root");
        assert_eq!(
            notes[0].blocks,
            vec![
                Block::Paragraph(vec![Inline::plain("root")]),
                Block::BlockQuote(vec![
                    Block::Paragraph(vec![Inline::plain("child")]),
                    Block::BlockQuote(vec![Block::Paragraph(vec![Inline::plain("leaf")])])
                ])
            ]
        );
    }
    #[test]
    fn malformed_reply_edges_preserve_content_and_report_only_selected_groups() {
        let comments = vec![
            source("root", None),
            source("a", Some("b")),
            source("b", Some("a")),
            source("orphan", Some("absent")),
        ];
        let selected = ["root".to_owned()].into_iter().collect();
        let ctx = ParseCtx::default();
        let plan = CommentThreads::new(&ctx, &comments, Some(&selected)).unwrap();
        assert_eq!(plan.finish(&ctx, comments).unwrap().0.len(), 1);
        assert_eq!(ctx.diagnostics().iter().count(), 0);
        let ctx = ParseCtx::default();
        let (notes, _) = threads(
            &ctx,
            vec![source("a", Some("b")), source("b", Some("a")), source("orphan", Some("absent"))],
        )
        .unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(
            ctx.diagnostics().iter().map(|d| (d.code, d.count)).collect::<Vec<_>>(),
            [(crate::DiagnosticCode::DanglingReference, 2)]
        );
    }
    #[test]
    fn reply_nesting_is_refused_before_building_recursive_blocks() {
        let limits = crate::Limits { max_nesting_depth: 1, ..crate::Limits::default() };
        let result = threads(
            &ParseCtx::new(&limits),
            vec![source("a", None), source("b", Some("a")), source("c", Some("b"))],
        );
        assert_eq!(result.err(), Some(ConvertError::limit(crate::Limit::NestingDepth, 1)));
    }
}
