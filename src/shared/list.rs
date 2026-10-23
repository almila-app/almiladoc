//! Nested-list assembly with numbering identity.
//!
//! Frontends resolve each list paragraph to a [`ListEntry`] carrying its
//! indent level, its list identity ([`ListKey`]), and its computed effective
//! number. Assembly splits runs whenever the identity or marker kind changes
//! at a level, or an ordered sequence is non-contiguous (a restart), so the
//! renderer's `start + index` numbering reproduces the source exactly.

pub(crate) use crate::model::MarkerKind;
use crate::model::{Block, List, ListItem};

/// Identity of a resolved list at one level: which list instance the entry
/// belongs to and what marker its level uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ListKey {
    /// Stable per-instance id (DOCX `numId`, RTF `\lsN`, DOC list identity
    /// `lsid`, a counter for HTML/ODF lists).
    pub instance: u64,
    pub marker: MarkerKind,
}

#[derive(Debug)]
pub(crate) struct ListMark {
    pub key: ListKey,
    pub level: usize,
    /// Effective item number at this entry (ignored for bullets).
    pub number: u64,
    /// Literal marker text when the source number text is not reproducible
    /// from the marker kind and number alone (composite number text).
    pub label: Option<String>,
}

/// One flat, fully resolved list paragraph (plus any blocks attached to the
/// same item, like text-box content anchored in it).
#[derive(Debug)]
pub(crate) struct ListEntry {
    pub mark: ListMark,
    pub blocks: Vec<Block>,
}

/// The deepest list level a run nests to, the last of the nine levels
/// (0 to 8) WordprocessingML, DrawingML, RTF and Word 97 define (PowerPoint
/// 97 defines five); a deeper source level is read as this one.
const MAX_LIST_LEVEL: usize = 8;

/// Pop the accumulated run of list paragraphs into list blocks; one block
/// per identity segment.
pub(crate) fn flush_list(blocks: &mut Vec<Block>, run: &mut Vec<ListEntry>) {
    for entry in run.iter_mut() {
        entry.mark.level = entry.mark.level.min(MAX_LIST_LEVEL);
    }
    blocks.extend(build_lists(std::mem::take(run)));
}

struct Open {
    list: List,
    key: Option<ListKey>,
    last: u64,
}

fn close(open: Option<Open>, out: &mut Vec<Block>) {
    if let Some(Open { list, .. }) = open
        && !list.items.is_empty()
    {
        out.push(Block::List(list));
    }
}

/// Fold a flat run into nested lists, splitting at identity/marker changes
/// and ordered-sequence discontinuities.
fn build_lists(entries: Vec<ListEntry>) -> Vec<Block> {
    let Some(min_lvl) = entries.iter().map(|e| e.mark.level).min() else {
        return Vec::new();
    };
    let mut out: Vec<Block> = Vec::new();
    let mut current: Option<Open> = None;

    let mut iter = entries.into_iter().peekable();
    while let Some(entry) = iter.next() {
        if entry.mark.level <= min_lvl {
            let ListMark { key, number, label, .. } = entry.mark;
            let split = current.as_ref().is_none_or(|open| {
                open.key != Some(key)
                    || (key.marker.ordered() && open.last.checked_add(1) != Some(number))
            });
            if split {
                close(current.take(), &mut out);
            }
            let open = current.get_or_insert_with(|| Open {
                list: List {
                    marker: key.marker,
                    start: if key.marker.ordered() { number } else { 1 },
                    items: Vec::new(),
                },
                key: Some(key),
                last: number,
            });
            open.list.items.push(ListItem { blocks: entry.blocks, marker_label: label });
            open.last = number;
        } else {
            let mut sub = vec![entry];
            sub.extend(std::iter::from_fn(|| iter.next_if(|e| e.mark.level > min_lvl)));
            let sublists = build_lists(sub);
            if sublists.is_empty() {
                continue;
            }
            let open = current.get_or_insert_with(|| Open {
                list: List { marker: MarkerKind::Bullet, start: 1, items: Vec::new() },
                key: None,
                last: 0,
            });
            match open.list.items.last_mut() {
                Some(item) => item.blocks.extend(sublists),
                None => open.list.items.push(ListItem { blocks: sublists, marker_label: None }),
            }
        }
    }
    close(current, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Inline;
    use std::assert_matches;

    fn entry(
        level: usize,
        instance: u64,
        marker: MarkerKind,
        number: u64,
        text: &str,
    ) -> ListEntry {
        ListEntry {
            mark: ListMark { key: ListKey { instance, marker }, level, number, label: None },
            blocks: vec![Block::Paragraph(vec![Inline::plain(text)])],
        }
    }

    fn lists(entries: Vec<ListEntry>) -> Vec<Block> {
        let mut blocks = Vec::new();
        flush_list(&mut blocks, &mut { entries });
        blocks
    }

    fn depth(blocks: &[Block]) -> usize {
        blocks
            .iter()
            .map(|block| match block {
                Block::List(list) => {
                    1 + list.items.iter().map(|item| depth(&item.blocks)).max().unwrap_or(0)
                }
                _ => 0,
            })
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn contiguous_numbers_stay_one_list() {
        let out = lists(vec![
            entry(0, 1, MarkerKind::Decimal, 1, "a"),
            entry(0, 1, MarkerKind::Decimal, 2, "b"),
        ]);
        assert_eq!(out.len(), 1);
        let Block::List(l) = &out[0] else { panic!() };
        assert!(l.ordered());
        assert_eq!((l.start, l.items.len()), (1, 2));
    }

    #[test]
    fn restart_splits_with_new_start() {
        let out = lists(vec![
            entry(0, 1, MarkerKind::Decimal, 1, "a"),
            entry(0, 1, MarkerKind::Decimal, 10, "restarted"),
        ]);
        assert_eq!(out.len(), 2);
        let Block::List(l) = &out[1] else { panic!() };
        assert_eq!(l.start, 10);
    }

    #[test]
    fn distinct_instances_split() {
        let out = lists(vec![
            entry(0, 1, MarkerKind::Decimal, 1, "a"),
            entry(0, 2, MarkerKind::Decimal, 1, "b"),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn marker_change_splits() {
        let out = lists(vec![
            entry(0, 1, MarkerKind::Decimal, 1, "a"),
            entry(0, 1, MarkerKind::Bullet, 0, "b"),
        ]);
        assert_eq!(out.len(), 2);
        let Block::List(l) = &out[1] else { panic!() };
        assert!(!l.ordered());
    }

    #[test]
    fn nesting_preserved() {
        let out = lists(vec![
            entry(0, 1, MarkerKind::Decimal, 1, "outer"),
            entry(1, 1, MarkerKind::LowerRoman, 1, "inner"),
            entry(0, 1, MarkerKind::Decimal, 2, "outer2"),
        ]);
        assert_eq!(out.len(), 1);
        let Block::List(l) = &out[0] else { panic!() };
        assert_eq!(l.items.len(), 2);
        assert_matches!(l.items[0].blocks.last(), Some(Block::List(sub)) if sub.ordered());
    }

    #[test]
    fn consecutive_deeper_entries_form_one_nested_list() {
        let out = lists(vec![
            entry(0, 1, MarkerKind::Decimal, 1, "outer"),
            entry(1, 1, MarkerKind::LowerRoman, 1, "a"),
            entry(1, 1, MarkerKind::LowerRoman, 2, "b"),
        ]);
        let [Block::List(outer)] = out.as_slice() else { panic!("{out:?}") };
        let [Block::Paragraph(_), Block::List(nested)] = outer.items[0].blocks.as_slice() else {
            panic!("{:?}", outer.items[0].blocks)
        };
        assert_eq!(nested.items.len(), 2);
    }

    #[test]
    fn no_entry_continues_the_anonymous_host_of_a_leading_sublist() {
        let out = lists(vec![
            entry(1, u64::MAX, MarkerKind::Bullet, 0, "nested"),
            entry(0, u64::MAX, MarkerKind::Bullet, 0, "top"),
        ]);
        let [Block::List(host), Block::List(top)] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((host.items.len(), top.items.len()), (1, 1));
    }

    #[test]
    fn levels_past_the_last_nest_no_deeper_than_it() {
        let rising = (0..100_000).map(|level| entry(level, 1, MarkerKind::Bullet, 0, "x"));
        assert_eq!(depth(&lists(rising.collect())), MAX_LIST_LEVEL + 1);
        let out = lists(vec![
            entry(MAX_LIST_LEVEL, 1, MarkerKind::Bullet, 0, "last"),
            entry(usize::MAX, 1, MarkerKind::Bullet, 0, "past"),
        ]);
        let [Block::List(l)] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(l.items.len(), 2);
    }
}
