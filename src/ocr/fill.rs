use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::slice;

use crate::diagnostics::{DiagnosticCode, Diagnostics, Region};
use crate::model::{Block, Cell, CellSlot, Document, Gap, GapId, ListItem};
use crate::ocr::GapError;

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum GapState {
    Unfilled,
    Failed,
    Filled(Vec<Block>),
    FilledPreservingSpans(Vec<Block>),
}

impl fmt::Debug for GapState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unfilled => f.write_str("Unfilled"),
            Self::Failed => f.write_str("Failed"),
            Self::Filled(blocks) => write!(f, "Filled({} blocks)", blocks.len()),
            Self::FilledPreservingSpans(blocks) => {
                write!(f, "FilledPreservingSpans({} blocks)", blocks.len())
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Slot {
    region: Region,
    depth: u64,
    state: GapState,
    unpainted: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct GapFills {
    slots: HashMap<GapId, Slot>,
}

impl GapFills {
    pub(crate) fn of(document: &Document) -> Self {
        let mut slots = HashMap::new();
        for Located { gap, region, depth } in gaps(document) {
            let slot = Slot { region, depth, state: GapState::Unfilled, unpainted: 0 };
            slots.entry(gap.id).or_insert(slot);
        }
        Self { slots }
    }

    pub(crate) fn state(&self, id: GapId) -> Option<&GapState> {
        self.slots.get(&id).map(|slot| &slot.state)
    }

    pub(crate) fn filled(&self, gap: &Gap) -> Option<&[Block]> {
        match self.state(gap.id) {
            Some(GapState::Filled(blocks) | GapState::FilledPreservingSpans(blocks)) => {
                Some(blocks)
            }
            _ => None,
        }
    }

    /// The nesting depth of the container an unfilled gap sits in.
    pub(crate) fn unsettled(&self, id: GapId) -> Result<u64, GapError> {
        match self.slots.get(&id) {
            None => Err(GapError::UnknownGap),
            Some(Slot { state: GapState::Unfilled, depth, .. }) => Ok(*depth),
            Some(_) => Err(GapError::Settled),
        }
    }

    /// Record that the image of gap `id`'s latest request left out `unpainted` items.
    pub(crate) fn note_unpainted(&mut self, id: GapId, unpainted: u64) {
        if let Some(slot) = self.slots.get_mut(&id) {
            slot.unpainted = unpainted;
        }
    }

    /// Settle a gap as `state`, its `ocr_gap` in `diagnostics` becoming `ocr_failed` when it
    /// failed and leaving when it filled, a fill reporting `raster_incomplete` for each item
    /// its latest request's image left out.
    pub(crate) fn settle(&mut self, id: GapId, state: GapState, diagnostics: &mut Diagnostics) {
        let Some(slot) = self.slots.get_mut(&id) else { return };
        let unit = id.unit().map(|unit| unit.ordinal);
        diagnostics.remove(DiagnosticCode::OcrGap, slot.region, unit, 1);
        match state {
            GapState::Failed => diagnostics.add(DiagnosticCode::OcrFailed, slot.region, unit, 1),
            GapState::Filled(_) | GapState::FilledPreservingSpans(_) => {
                diagnostics.add(
                    DiagnosticCode::RasterIncomplete,
                    slot.region,
                    unit,
                    slot.unpainted,
                );
            }
            GapState::Unfilled => {}
        }
        slot.state = state;
    }

    /// `document` with each filled gap's blocks in its place.
    #[cfg_attr(
        not(any(feature = "serde", test)),
        expect(dead_code, reason = "only the JSON writes a spliced document")
    )]
    #[expect(clippy::disallowed_methods, reason = "whether any gap is filled is order-free")]
    pub(crate) fn spliced<'d>(&self, document: &'d Document) -> Cow<'d, Document> {
        if !self.slots.values().any(|slot| {
            matches!(slot.state, GapState::Filled(_) | GapState::FilledPreservingSpans(_))
        }) {
            return Cow::Borrowed(document);
        }
        let mut spliced = document.clone();
        let mut stack: Vec<&mut Vec<Block>> = vec![&mut spliced.blocks];
        stack.extend(spliced.notes.iter_mut().map(|note| &mut note.blocks));
        while let Some(blocks) = stack.pop() {
            let mut kept = Vec::with_capacity(blocks.len());
            for block in std::mem::take(blocks) {
                match block {
                    Block::Gap(gap) if let Some(fill) = self.filled(&gap) => {
                        kept.extend_from_slice(fill);
                    }
                    block => kept.push(block),
                }
            }
            *blocks = kept;
            for block in blocks {
                match block {
                    Block::BlockQuote(inner) => stack.push(inner),
                    Block::List(list) => stack.extend(list.items.iter_mut().map(|i| &mut i.blocks)),
                    Block::Table(table) => stack.extend(
                        table.grid.iter_mut().flatten().filter_map(|slot| match slot {
                            CellSlot::Origin(cell) => Some(&mut cell.blocks),
                            CellSlot::Covered { .. } => None,
                        }),
                    ),
                    _ => {}
                }
            }
        }
        Cow::Owned(spliced)
    }
}

pub(crate) struct Located<'d> {
    pub(crate) gap: &'d Gap,
    pub(crate) region: Region,
    depth: u64,
}

/// Every gap of `document` in document order, the body's before the notes'.
pub(crate) fn gaps(document: &Document) -> impl Iterator<Item = Located<'_>> {
    let body = std::iter::once((Region::Body, document.blocks.as_slice()));
    let notes = document.notes.iter().map(|note| (Region::Note, note.blocks.as_slice()));
    body.chain(notes).flat_map(|(region, blocks)| {
        Walk { stack: vec![(Frame::Blocks(blocks.iter()), 0)] }.map(move |(gap, depth)| Located {
            gap,
            region,
            depth,
        })
    })
}

/// Where a walk stands in one container: its blocks, a list's items or a table's cells, each
/// entered only when reached, so the walk holds a frame per level of nesting.
enum Frame<'d> {
    Blocks(slice::Iter<'d, Block>),
    Items(slice::Iter<'d, ListItem>),
    Cells(Box<dyn Iterator<Item = &'d Cell> + 'd>),
}

struct Walk<'d> {
    stack: Vec<(Frame<'d>, u64)>,
}

impl<'d> Iterator for Walk<'d> {
    type Item = (&'d Gap, u64);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (frame, depth) = self.stack.last_mut()?;
            let depth = *depth;
            let entered = match frame {
                Frame::Items(items) => items.next().map(|item| item.blocks.iter()),
                Frame::Cells(cells) => cells.next().map(|cell| cell.blocks.iter()),
                Frame::Blocks(blocks) => {
                    let Some(block) = blocks.next() else {
                        self.stack.pop();
                        continue;
                    };
                    let inner = depth.saturating_add(1);
                    let frame = match block {
                        Block::Gap(gap) => return Some((gap, depth)),
                        Block::BlockQuote(blocks) => Frame::Blocks(blocks.iter()),
                        Block::List(list) => Frame::Items(list.items.iter()),
                        Block::Table(table) => Frame::Cells(Box::new(table.origins())),
                        _ => continue,
                    };
                    self.stack.push((frame, inner));
                    continue;
                }
            };
            match entered {
                Some(blocks) => self.stack.push((Frame::Blocks(blocks), depth)),
                None => _ = self.stack.pop(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::model::{
        Inline, List, MarkerKind, Note, NoteKind, OcrOrigin, Table, Unit, UnitKind,
    };

    fn gap(page: u32) -> Block {
        let page = NonZeroU32::new(page).unwrap();
        Block::Gap(Gap {
            id: GapId::new(Some(Unit::new(UnitKind::Page, page)), NonZeroU32::MIN),
            origin: OcrOrigin::Page(page),
            reasons: Vec::new(),
        })
    }

    fn list(items: Vec<Vec<Block>>) -> Block {
        let items =
            items.into_iter().map(|blocks| ListItem { blocks, marker_label: None }).collect();
        Block::List(List { marker: MarkerKind::Bullet, start: 1, items })
    }

    #[test]
    fn gaps_walk_every_container_in_order_the_body_before_the_notes() {
        let table =
            Table::from_rows(vec![vec![Cell::new(vec![gap(4)]), Cell::new(vec![gap(5)])]], 0);
        let document = Document {
            metadata: None,
            blocks: vec![
                gap(1),
                Block::BlockQuote(vec![Block::Paragraph(vec![Inline::plain("x")]), gap(2)]),
                list(vec![vec![gap(3)], vec![Block::Table(table)]]),
                gap(6),
            ],
            notes: vec![Note { id: "n".into(), kind: NoteKind::Footnote, blocks: vec![gap(7)] }],
            assets: Vec::new(),
        };
        let found: Vec<_> = gaps(&document)
            .map(|located| {
                (located.gap.id.unit().unwrap().ordinal.get(), located.region, located.depth)
            })
            .collect();
        let body = Region::Body;
        assert_eq!(
            found,
            [
                (1, body, 0),
                (2, body, 1),
                (3, body, 1),
                (4, body, 2),
                (5, body, 2),
                (6, body, 0),
                (7, Region::Note, 0)
            ]
        );
    }

    #[test]
    fn spliced_puts_each_filled_gap_s_blocks_in_its_place_in_every_container() {
        let para = |text: &str| Block::Paragraph(vec![Inline::plain(text)]);
        let table = |blocks| Block::Table(Table::from_rows(vec![vec![Cell::new(blocks)]], 0));
        let note = |blocks| Note { id: "n".into(), kind: NoteKind::Footnote, blocks };
        let document = Document {
            metadata: None,
            blocks: vec![
                gap(1),
                Block::BlockQuote(vec![gap(2)]),
                list(vec![vec![gap(3)]]),
                table(vec![gap(4)]),
                gap(5),
                gap(6),
            ],
            notes: vec![note(vec![gap(7)])],
            assets: Vec::new(),
        };
        let mut fills = GapFills::of(&document);
        assert!(matches!(fills.spliced(&document), Cow::Borrowed(_)));
        let mut diagnostics = Diagnostics::default();
        for page in [1, 2, 3, 4, 7] {
            let id = GapId::new(
                Some(Unit::new(UnitKind::Page, NonZeroU32::new(page).unwrap())),
                NonZeroU32::MIN,
            );
            let fill = GapState::Filled(vec![para(&page.to_string()), Block::Rule]);
            fills.settle(id, fill, &mut diagnostics);
        }
        let failed = GapId::new(
            Some(Unit::new(UnitKind::Page, NonZeroU32::new(6).unwrap())),
            NonZeroU32::MIN,
        );
        fills.settle(failed, GapState::Failed, &mut diagnostics);
        let filled = |page: &str| vec![para(page), Block::Rule];
        let expected = Document {
            metadata: None,
            blocks: [
                filled("1"),
                vec![Block::BlockQuote(filled("2"))],
                vec![list(vec![filled("3")])],
                vec![table(filled("4"))],
                vec![gap(5), gap(6)],
            ]
            .concat(),
            notes: vec![note(filled("7"))],
            assets: Vec::new(),
        };
        assert_eq!(*fills.spliced(&document), expected);
    }

    #[test]
    fn a_gap_settles_once_and_moves_its_diagnostic() {
        let document = Document { blocks: vec![gap(1), gap(2)], ..Document::default() };
        let [one, two] = [1, 2].map(|page| {
            GapId::new(
                Some(Unit::new(UnitKind::Page, NonZeroU32::new(page).unwrap())),
                NonZeroU32::MIN,
            )
        });
        let mut fills = GapFills::of(&document);
        let mut diagnostics = Diagnostics::default();
        for id in [one, two] {
            diagnostics.add(DiagnosticCode::OcrGap, Region::Body, id.unit().map(|u| u.ordinal), 1);
        }
        fills.settle(one, GapState::Failed, &mut diagnostics);
        fills.settle(two, GapState::Filled(Vec::new()), &mut diagnostics);
        let rows: Vec<_> =
            diagnostics.iter().map(|d| (d.code, d.unit.map(NonZeroU32::get))).collect();
        assert_eq!(rows, [(DiagnosticCode::OcrFailed, Some(1))]);
        assert_eq!(fills.unsettled(one), Err(GapError::Settled));
        assert_eq!(fills.unsettled(two), Err(GapError::Settled));
        let other = GapId::new(
            Some(Unit::new(UnitKind::Page, NonZeroU32::MIN)),
            NonZeroU32::new(2).unwrap(),
        );
        assert_eq!(fills.unsettled(other), Err(GapError::UnknownGap));
        assert_eq!(format!("{:?}", fills.state(two)), "Some(Filled(0 blocks))");
    }
}
