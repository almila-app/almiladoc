use super::assemble::Assembler;
use super::sides::Layout;
use super::{Fib, Plc, read_plc};
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Block, Note};
use crate::shared::binary::{get_u16, get_u32};
use crate::shared::comments::{CommentThreads, SourceComment, remap_references};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

const ATRD_SIZE: usize = 30;
const POST10_SIZE: usize = 18;
const POST10_DEPTH: usize = 6;
const POST10_PARENT: usize = 10;
const FC_LCB_START: usize = 0x9A;
const FC_LCB_COUNT: usize = 0x98;
const ATRD_EXTRA_PAIR: usize = 112;
const WORD_2002: u16 = 0x0101;

#[derive(Default)]
pub(super) struct References {
    by_cp: HashMap<usize, Vec<usize>>,
    selected: RefCell<BTreeSet<String>>,
    count: usize,
}

impl References {
    pub(super) fn read(
        ctx: &ParseCtx<'_>,
        fib: Fib<'_>,
        table: &[u8],
    ) -> Result<Self, ConvertError> {
        let Some(refs) = read_plc(fib, table, Fib::FC_PLCFAND_REF, ATRD_SIZE) else {
            return Ok(Self::default());
        };
        let mut by_cp = HashMap::<_, Vec<_>>::new();
        for index in 0..refs.n {
            ctx.charge(1)?;
            by_cp.entry(refs.cp(index)).or_default().push(index);
        }
        Ok(Self { by_cp, count: refs.n, ..Self::default() })
    }

    pub(super) fn at(&self, cp: usize) -> Option<&[usize]> {
        self.by_cp.get(&cp).map(Vec::as_slice)
    }

    pub(super) fn select(&self, ctx: &ParseCtx<'_>, index: usize) -> Result<String, ConvertError> {
        let id = comment_id(index);
        ctx.charge(id.len() as u64 * 2)?;
        self.selected.borrow_mut().insert(id.clone());
        Ok(id)
    }
}

fn comment_id(index: usize) -> String {
    format!("doc-comment-{index}")
}

pub(super) fn append(
    ctx: &ParseCtx<'_>,
    layout: &Layout<'_>,
    assembler: &Assembler<'_, '_>,
    blocks: &mut [Block],
    notes: &mut Vec<Note>,
) -> Result<(), ConvertError> {
    let selected = assembler.comments.selected.borrow();
    if selected.is_empty() {
        return Ok(());
    }
    let count = assembler.comments.count;
    let extra = Extra::read(layout);
    let mut comments = records(ctx, &extra, count)?;
    let threads = CommentThreads::new(ctx, &comments, Some(&selected))?;
    drop(selected);
    threads.admit(ctx)?;
    let text = read_plc(layout.fib, layout.table, Fib::FC_PLCFAND_TXT, 0);
    let story = layout.stories.comments();
    for (index, comment) in comments.iter_mut().enumerate() {
        if !threads.includes(index) {
            continue;
        }
        if extra.malformed(index) {
            ctx.emit(DiagnosticCode::PartUnreadable, Region::Comment);
        }
        let range = body_range(ctx, text, index, count, &story);
        if let Some(range) = range {
            comment.blocks =
                assembler.side_blocks(&story, &range, Region::Comment)?.unwrap_or_default();
        }
    }
    let (comments, aliases) = threads.finish(ctx, comments)?;
    remap_references(ctx, blocks, &aliases)?;
    for note in &mut *notes {
        remap_references(ctx, &mut note.blocks, &aliases)?;
    }
    notes.extend(comments);
    Ok(())
}

fn records(
    ctx: &ParseCtx<'_>,
    extra: &Extra<'_>,
    count: usize,
) -> Result<Vec<SourceComment>, ConvertError> {
    let mut comments = Vec::new();
    for index in 0..count {
        ctx.charge(1)?;
        let id = comment_id(index);
        let parent = extra.parent(index).map(comment_id);
        ctx.charge(id.len().saturating_add(parent.as_ref().map_or(0, String::len)) as u64)?;
        comments.push(SourceComment { id, parent, blocks: Vec::new() });
    }
    Ok(comments)
}

fn body_range(
    ctx: &ParseCtx<'_>,
    text: Option<Plc<'_>>,
    index: usize,
    count: usize,
    story: &std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    let Some(text) = text.filter(|text| index + 1 < text.n) else {
        ctx.emit(DiagnosticCode::PartUnreadable, Region::Comment);
        return None;
    };
    let end = story.end.saturating_sub(story.start).saturating_sub(1);
    let (start_cp, end_cp) = (text.cp(index), text.cp(index + 1));
    if start_cp >= end_cp || end_cp > end || (index + 1 == count && end_cp != end) {
        ctx.emit(DiagnosticCode::PartUnreadable, Region::Comment);
    }
    let (start_cp, end_cp) = (start_cp.min(end), end_cp.min(end));
    (start_cp < end_cp).then_some(story.start + start_cp..story.start + end_cp)
}

#[derive(Default)]
struct Extra<'a> {
    bytes: &'a [u8],
    unread: bool,
}

impl<'a> Extra<'a> {
    fn read(layout: &Layout<'a>) -> Self {
        let count = usize::from(layout.fib.u16(FC_LCB_COUNT));
        if count <= ATRD_EXTRA_PAIR {
            return Self::default();
        }
        let csw_new = FC_LCB_START + count * 8;
        let version = match get_u16(layout.word_doc, csw_new) {
            Some(0) => get_u16(layout.word_doc, 2),
            Some(_) => get_u16(layout.word_doc, csw_new + 2),
            None => None,
        };
        let Some(version) = version else {
            return Self { unread: true, ..Self::default() };
        };
        if version < WORD_2002 {
            return Self::default();
        }
        let pair = FC_LCB_START + ATRD_EXTRA_PAIR * 8;
        let bytes = get_u32(layout.word_doc, pair)
            .zip(get_u32(layout.word_doc, pair + 4))
            .and_then(|(start, len)| {
                let start = usize::try_from(start).ok()?;
                let len = usize::try_from(len).ok()?;
                if len == 0 {
                    Some(&[][..])
                } else {
                    layout.table.get(start..start.checked_add(len)?)
                }
            });
        Self { bytes: bytes.unwrap_or_default(), unread: bytes.is_none() }
    }

    fn record(&self, index: usize) -> Option<&[u8]> {
        self.bytes.get(index.checked_mul(POST10_SIZE)?..)?.get(..POST10_SIZE)
    }

    fn parent(&self, index: usize) -> Option<usize> {
        let parent = get_u32(self.record(index)?, POST10_PARENT)?;
        let delta = parent.cast_signed();
        if delta == 0 {
            None
        } else {
            usize::try_from(i64::try_from(index).ok()?.checked_add(i64::from(delta))?).ok()
        }
    }

    fn malformed(&self, index: usize) -> bool {
        if self.unread {
            return true;
        }
        if self.bytes.is_empty() {
            return false;
        }
        let depth = self.record(index).and_then(|row| get_u32(row, POST10_DEPTH));
        let delta = self.record(index).and_then(|row| get_u32(row, POST10_PARENT));
        let expected = if delta == Some(0) {
            Some(0)
        } else {
            self.parent(index)
                .and_then(|parent| get_u32(self.record(parent)?, POST10_DEPTH)?.checked_add(1))
        };
        depth.is_none() || depth != expected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{Limit, Limits};
    use std::assert_matches;

    #[test]
    fn invalid_relative_parent_is_not_recovered_as_a_valid_root() {
        for parent in [u32::MAX, 1] {
            let mut row = [0; POST10_SIZE];
            row[POST10_PARENT..POST10_PARENT + 4].copy_from_slice(&parent.to_le_bytes());
            let extra = Extra { bytes: &row, unread: false };
            assert!(extra.malformed(0));
        }
        let row = [0; POST10_SIZE];
        assert!(!Extra { bytes: &row, unread: false }.malformed(0));
    }

    #[test]
    fn each_selected_reference_charges_both_owned_ids() {
        for max_work in [25, 26] {
            let limits = Limits { max_work, ..Limits::DEFAULT };
            let ctx = ParseCtx::new(&limits);
            let references = References::default();
            let selected = references.select(&ctx, 0);
            if max_work == 26 {
                assert_eq!(selected.unwrap(), "doc-comment-0");
                assert_eq!(
                    references.selected.into_inner(),
                    BTreeSet::from(["doc-comment-0".into()])
                );
            } else {
                assert_matches!(
                    selected,
                    Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 25 })
                );
                assert_eq!(references.selected.into_inner(), BTreeSet::new());
            }
            assert_eq!(ctx.fuel(), 26);
        }
    }

    #[test]
    fn comment_record_indexing_charges_every_record_and_owned_id() {
        for max_work in [27, 28] {
            let limits = Limits { max_work, ..Limits::DEFAULT };
            let ctx = ParseCtx::new(&limits);
            let comments = records(&ctx, &Extra::default(), 2);
            if max_work == 28 {
                assert_eq!(
                    comments.unwrap().iter().map(|comment| comment.id.as_str()).collect::<Vec<_>>(),
                    ["doc-comment-0", "doc-comment-1"]
                );
            } else {
                assert_matches!(
                    comments.map(drop),
                    Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 27 })
                );
            }
            assert_eq!(ctx.fuel(), 28);
        }
    }
}
