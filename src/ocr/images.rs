use std::collections::{BTreeMap, HashSet};
use std::num::NonZeroU32;
use std::slice;

use crate::ctx::ParseCtx;
use crate::diagnostics::DiagnosticCode;
use crate::error::ConvertError;
use crate::model::{
    Asset, AssetId, Block, Cell, CellSlot, Document, Gap, GapId, ImageSource, Inline, ListItem,
    OcrOrigin, Unit,
};

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const JPEG_SIGNATURE: &[u8] = b"\xff\xd8\xff";

/// Leave a gap after each block of `document` that shows an embedded PNG or JPEG, numbered in
/// document order within its image's unit, and count `image_not_ocred` for every other image;
/// a note whose id an earlier note took never renders, so it gains none.
pub(crate) fn leave_image_gaps(
    parse_ctx: &ParseCtx<'_>,
    document: &mut Document,
) -> Result<(), ConvertError> {
    let Document { blocks, notes, assets } = document;
    let mut ids = HashSet::new();
    let selected: Vec<bool> = notes.iter().map(|note| ids.insert(note.id.as_str())).collect();
    let notes =
        notes.iter_mut().zip(selected).filter_map(|(note, kept)| kept.then_some(&mut note.blocks));
    let mut next = BTreeMap::new();
    for blocks in std::iter::once(blocks).chain(notes) {
        let mut stack = vec![Frame::Blocks(with_gaps(parse_ctx, blocks, assets)?)];
        while let Some(frame) = stack.last_mut() {
            let entered = match frame {
                Frame::Items(items) => items.next().map(|item| &mut item.blocks),
                Frame::Cells(cells) => cells.next().map(|cell| &mut cell.blocks),
                Frame::Blocks(blocks) => {
                    let Some(block) = blocks.next() else {
                        stack.pop();
                        continue;
                    };
                    match block {
                        Block::Gap(gap) => number(gap, &mut next),
                        Block::BlockQuote(inner) => {
                            stack.push(Frame::Blocks(with_gaps(parse_ctx, inner, assets)?));
                        }
                        Block::List(list) => stack.push(Frame::Items(list.items.iter_mut())),
                        Block::Table(table) => stack.push(Frame::Cells(Box::new(
                            table.grid.iter_mut().flatten().filter_map(|slot| match slot {
                                CellSlot::Origin(cell) => Some(cell),
                                CellSlot::Covered { .. } => None,
                            }),
                        ))),
                        _ => {}
                    }
                    continue;
                }
            };
            match entered {
                Some(blocks) => stack.push(Frame::Blocks(with_gaps(parse_ctx, blocks, assets)?)),
                None => _ = stack.pop(),
            }
        }
    }
    Ok(())
}

/// Where the walk stands in one container, entered only when reached, so it holds a frame
/// per level of nesting.
enum Frame<'d> {
    Blocks(slice::IterMut<'d, Block>),
    Items(slice::IterMut<'d, ListItem>),
    Cells(Box<dyn Iterator<Item = &'d mut Cell> + 'd>),
}

/// `blocks` with a gap after each that shows an embedded PNG or JPEG, to walk on.
fn with_gaps<'d>(
    parse_ctx: &ParseCtx<'_>,
    blocks: &'d mut Vec<Block>,
    assets: &[Asset],
) -> Result<slice::IterMut<'d, Block>, ConvertError> {
    parse_ctx.charge(u64::try_from(blocks.len()).unwrap_or(u64::MAX))?;
    let mut kept = Vec::with_capacity(blocks.len());
    for block in std::mem::take(blocks) {
        let gaps = match &block {
            Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
                image_gaps(parse_ctx, inlines, assets)?
            }
            _ => Vec::new(),
        };
        kept.push(block);
        kept.extend(gaps);
    }
    *blocks = kept;
    Ok(blocks.iter_mut())
}

/// Number `gap` by its place among its unit's gaps reached so far.
fn number(gap: &mut Gap, next: &mut BTreeMap<Option<Unit>, NonZeroU32>) {
    let unit = gap.id.unit();
    let ordinal = next.entry(unit).or_insert(NonZeroU32::MIN);
    gap.id = GapId::new(unit, *ordinal);
    *ordinal = ordinal.saturating_add(1);
}

/// A gap for each embedded PNG or JPEG `inlines` show, links included, in order.
fn image_gaps(
    parse_ctx: &ParseCtx<'_>,
    inlines: &[Inline],
    assets: &[Asset],
) -> Result<Vec<Block>, ConvertError> {
    let mut gaps = Vec::new();
    let mut walked = 0_u64;
    let mut stack = vec![inlines.iter()];
    while let Some(inlines) = stack.last_mut() {
        let Some(inline) = inlines.next() else {
            stack.pop();
            continue;
        };
        walked = walked.saturating_add(1);
        match inline {
            Inline::Link { content, .. } => stack.push(content.iter()),
            Inline::Image { source: ImageSource::Asset(id), origin, .. }
                if ocrable(assets, *id) =>
            {
                gaps.push(Block::Gap(Gap {
                    id: GapId::new(origin.unit, NonZeroU32::MIN),
                    origin: OcrOrigin::Image(*id),
                    reasons: Vec::new(),
                }));
            }
            Inline::Image { origin, .. } => {
                parse_ctx.emit_at(DiagnosticCode::ImageNotOcred, *origin);
            }
            _ => {}
        }
    }
    parse_ctx.charge(walked)?;
    Ok(gaps)
}

fn ocrable(assets: &[Asset], id: AssetId) -> bool {
    assets.get(id.0).is_some_and(|asset| media_type(&asset.bytes).is_some())
}

/// `image/png` or `image/jpeg` when `bytes` open with that format's signature.
pub(crate) fn media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(PNG_SIGNATURE) {
        Some("image/png")
    } else if bytes.starts_with(JPEG_SIGNATURE) {
        Some("image/jpeg")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Region;
    use crate::limits::Limits;
    use crate::model::{ImageOrigin, List, MarkerKind, Note, NoteKind, Table, UnitKind};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n...";
    const JPEG: &[u8] = b"\xff\xd8\xff\xdb";

    fn slide(n: u32) -> Unit {
        Unit::new(UnitKind::Slide, NonZeroU32::new(n).unwrap())
    }

    fn image(asset: usize, region: Region, unit: Option<Unit>) -> Inline {
        Inline::Image {
            alt: String::new(),
            source: ImageSource::Asset(AssetId(asset)),
            origin: ImageOrigin::new(region, unit),
        }
    }

    fn para(inlines: Vec<Inline>) -> Block {
        Block::Paragraph(inlines)
    }

    fn gap(asset: usize, unit: Option<Unit>, ordinal: u32) -> Block {
        Block::Gap(Gap {
            id: GapId::new(unit, NonZeroU32::new(ordinal).unwrap()),
            origin: OcrOrigin::Image(AssetId(asset)),
            reasons: Vec::new(),
        })
    }

    fn list(items: Vec<Vec<Block>>) -> Block {
        let items = items.into_iter().map(|blocks| ListItem { blocks, marker_label: None });
        Block::List(List { marker: MarkerKind::Bullet, start: 1, items: items.collect() })
    }

    fn table(cells: Vec<Vec<Block>>) -> Block {
        Block::Table(Table::from_rows(vec![cells.into_iter().map(Cell::new).collect()], 0))
    }

    fn note(id: &str, blocks: Vec<Block>) -> Note {
        Note { id: id.into(), kind: NoteKind::Footnote, blocks }
    }

    fn assets(bytes: &[&[u8]]) -> Vec<Asset> {
        bytes
            .iter()
            .enumerate()
            .map(|(i, bytes)| Asset {
                id: AssetId(i),
                media_type: "image/png".into(),
                origin_part: String::new(),
                bytes: bytes.to_vec(),
            })
            .collect()
    }

    fn left(document: &mut Document) -> crate::diagnostics::Diagnostics {
        let limits = Limits::default();
        let parse_ctx = ParseCtx::new(&limits);
        leave_image_gaps(&parse_ctx, document).unwrap();
        parse_ctx.into_diagnostics()
    }

    #[test]
    fn a_gap_follows_its_innermost_block_numbered_in_document_order_within_its_unit() {
        let (body, note_region) = (Region::Body, Region::Note);
        let [one, two] = [Some(slide(1)), Some(slide(2))];
        let heading = |inlines| Block::Heading { level: 1, anchor: None, content: inlines };
        let link = |content| Inline::Link {
            content,
            target: crate::model::LinkTarget::External("https://example.org".into()),
        };
        let mut document = Document {
            blocks: vec![
                para(vec![Inline::plain("a"), image(0, body, one), image(1, body, one)]),
                Block::BlockQuote(vec![para(vec![image(0, body, one)])]),
                list(vec![
                    vec![para(vec![image(0, body, two)])],
                    vec![table(vec![
                        vec![heading(vec![image(1, body, two)])],
                        vec![para(vec![image(0, body, two)])],
                    ])],
                ]),
                para(vec![link(vec![link(vec![image(0, body, two)])])]),
            ],
            notes: vec![
                note("n", vec![para(vec![image(0, note_region, one)])]),
                note("n", vec![para(vec![image(0, note_region, one)])]),
                note("m", vec![para(vec![image(1, note_region, two)])]),
            ],
            assets: assets(&[PNG, JPEG]),
        };
        let mut expected = document.clone();
        expected.blocks = vec![
            expected.blocks[0].clone(),
            gap(0, one, 1),
            gap(1, one, 2),
            Block::BlockQuote(vec![para(vec![image(0, body, one)]), gap(0, one, 3)]),
            list(vec![
                vec![para(vec![image(0, body, two)]), gap(0, two, 1)],
                vec![table(vec![
                    vec![heading(vec![image(1, body, two)]), gap(1, two, 2)],
                    vec![para(vec![image(0, body, two)]), gap(0, two, 3)],
                ])],
            ]),
            expected.blocks[3].clone(),
            gap(0, two, 4),
        ];
        expected.notes[0].blocks.push(gap(0, one, 4));
        expected.notes[2].blocks.push(gap(1, two, 5));
        assert!(left(&mut document).is_empty());
        assert_eq!(document, expected);
    }

    #[test]
    fn every_image_but_an_embedded_png_or_jpeg_counts_where_it_stands() {
        let unread = |source: ImageSource| Inline::Image {
            alt: String::new(),
            source,
            origin: ImageOrigin::new(Region::Note, Some(slide(3))),
        };
        let cases = [
            (image(0, Region::Body, None), 1, None),
            (image(1, Region::Body, None), 1, None),
            (image(2, Region::Body, Some(slide(2))), 0, Some((Region::Body, 2))),
            (image(3, Region::Note, None), 0, Some((Region::Note, 0))),
            (image(9, Region::Body, None), 0, Some((Region::Body, 0))),
            (
                unread(ImageSource::External("https://example.org/a.png".into())),
                0,
                Some((Region::Note, 3)),
            ),
            (unread(ImageSource::Unavailable), 0, Some((Region::Note, 3))),
            (unread(ImageSource::Unread), 0, Some((Region::Note, 3))),
        ];
        for (inline, gaps, counted) in cases {
            let mut document = Document {
                blocks: vec![para(vec![inline.clone()])],
                assets: assets(&[PNG, JPEG, b"GIF89a", b"\x89PNG"]),
                ..Document::default()
            };
            let diagnostics = left(&mut document);
            assert_eq!(document.blocks.len(), 1 + gaps, "{inline:?}");
            let rows: Vec<_> = diagnostics
                .iter()
                .map(|d| (d.code, d.region, d.unit.map_or(0, NonZeroU32::get), d.count))
                .collect();
            let expected: Vec<_> = counted
                .map(|(region, unit)| (DiagnosticCode::ImageNotOcred, region, unit, 1))
                .into_iter()
                .collect();
            assert_eq!(rows, expected, "{inline:?}");
        }
    }

    #[test]
    fn the_walk_charges_each_block_and_inline() {
        let link = Inline::Link {
            content: vec![image(0, Region::Body, None)],
            target: crate::model::LinkTarget::Relative("a".into()),
        };
        let mut document = Document {
            blocks: vec![
                para(vec![Inline::plain("a"), link]),
                list(vec![vec![para(vec![image(0, Region::Body, None)])]]),
            ],
            assets: assets(&[PNG]),
            ..Document::default()
        };
        let limits = Limits::default();
        let parse_ctx = ParseCtx::new(&limits);
        leave_image_gaps(&parse_ctx, &mut document).unwrap();
        assert_eq!(parse_ctx.ledger().spent(), 2 + 3 + 1 + 1);
    }
}
