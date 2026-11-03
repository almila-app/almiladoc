use std::collections::{BTreeMap, HashSet};
use std::num::NonZeroU32;
use std::slice;

use crate::ctx::ParseCtx;
use crate::diagnostics::DiagnosticCode;
use crate::error::ConvertError;
use crate::model::{
    Asset, AssetId, Block, Cell, CellSlot, Document, Gap, GapId, HeaderFooter, ImageSource, Inline,
    ListItem, OcrOrigin, Unit,
};

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const JPEG_SIGNATURE: &[u8] = b"\xff\xd8\xff";
const PNG_CHUNK_LENGTH: usize = 4;
const PNG_HEADER_CHUNK: &[u8; 4] = b"IHDR";
const MAX_SIDE: u32 = (1 << 31) - 1;
const JPEG_SOI_LEN: usize = 2;
const JPEG_PREFIX: u8 = 0xff;
const JPEG_TEM: u8 = 0x01;
const JPEG_SOF0: u8 = 0xc0;
const JPEG_DHT: u8 = 0xc4;
const JPEG_JPG_EXTENSION: u8 = 0xc8;
const JPEG_DAC: u8 = 0xcc;
const JPEG_SOF15: u8 = 0xcf;
const JPEG_RST0: u8 = 0xd0;
const JPEG_SOI: u8 = 0xd8;
const JPEG_EOI: u8 = 0xd9;
const JPEG_SOS: u8 = 0xda;

/// Leave a gap after each block of `document`, side stories included, that shows an embedded PNG
/// or JPEG, numbered in document order within its image's unit, and count `image_not_ocred` for every other image;
/// a note whose id an earlier note took never renders, so it gains none.
pub(crate) fn leave_image_gaps(
    parse_ctx: &ParseCtx<'_>,
    document: &mut Document,
) -> Result<(), ConvertError> {
    let Document { blocks, notes, assets, .. } = document;
    let mut ids = HashSet::new();
    let selected: Vec<bool> = notes
        .iter()
        .map(|note| ids.insert((note.kind == crate::model::NoteKind::Comment, note.id.as_str())))
        .collect();
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
                        Block::BlockQuote(inner)
                        | Block::HeaderFooter(HeaderFooter { content: Some(inner), .. }) => {
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

/// The size the header of `bytes`, an image of `media_type`, declares, read in one forward pass
/// that the copy of `bytes` already prices; `None` when it holds none or a side out of range.
pub(crate) fn dimensions(bytes: &[u8], media_type: &str) -> Option<(u32, u32)> {
    let (width, height) =
        if media_type == "image/png" { png_dimensions(bytes)? } else { jpeg_dimensions(bytes)? };
    let valid = 1..=MAX_SIDE;
    (valid.contains(&width) && valid.contains(&height)).then_some((width, height))
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let after_length = bytes.get(PNG_SIGNATURE.len() + PNG_CHUNK_LENGTH..)?;
    let (name, rest) = after_length.split_first_chunk::<4>()?;
    let (width, rest) = rest.split_first_chunk::<4>()?;
    let (height, _) = rest.split_first_chunk::<4>()?;
    (name == PNG_HEADER_CHUNK).then(|| (u32::from_be_bytes(*width), u32::from_be_bytes(*height)))
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut rest = bytes.get(JPEG_SOI_LEN..)?;
    loop {
        let (&prefix, after) = rest.split_first()?;
        let (&marker, after) = after.split_first()?;
        if prefix != JPEG_PREFIX {
            return None;
        }
        match marker {
            JPEG_PREFIX => rest = rest.get(1..)?,
            JPEG_TEM | JPEG_RST0..=JPEG_SOI => rest = after,
            JPEG_EOI | JPEG_SOS => return None,
            _ => {
                let (length, payload) = after.split_first_chunk::<2>()?;
                if is_frame_header(marker) {
                    let (_precision, payload) = payload.split_first()?;
                    let (height, payload) = payload.split_first_chunk::<2>()?;
                    let (width, _) = payload.split_first_chunk::<2>()?;
                    return Some((
                        u32::from(u16::from_be_bytes(*width)),
                        u32::from(u16::from_be_bytes(*height)),
                    ));
                }
                rest = after.get(usize::from(u16::from_be_bytes(*length))..)?;
            }
        }
    }
}

fn is_frame_header(marker: u8) -> bool {
    (JPEG_SOF0..=JPEG_SOF15).contains(&marker)
        && !matches!(marker, JPEG_DHT | JPEG_JPG_EXTENSION | JPEG_DAC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Region;
    use crate::limits::Limits;
    use crate::model::{
        HeaderFooterRegion, HeaderFooterVariant, ImageOrigin, List, MarkerKind, Note, NoteKind,
        Table, UnitKind,
    };

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
    fn comments_and_notes_with_the_same_id_both_receive_image_gaps() {
        let mut comment = note("same", vec![para(vec![image(1, Region::Comment, None)])]);
        comment.kind = NoteKind::Comment;
        let mut document = Document {
            notes: vec![note("same", vec![para(vec![image(0, Region::Note, None)])]), comment],
            assets: assets(&[PNG, JPEG]),
            ..Document::default()
        };
        assert_eq!(left(&mut document).iter().count(), 0);
        assert_eq!(
            crate::ocr::gaps(&document).map(|located| located.region).collect::<Vec<_>>(),
            [Region::Note, Region::Comment]
        );
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
            metadata: None,
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
    fn a_side_story_gains_gaps_in_body_order_and_counts_its_unread_images_in_its_region() {
        let one = Some(slide(1));
        let footer = |content| {
            Block::HeaderFooter(HeaderFooter {
                page_style: None,
                region: HeaderFooterRegion::Footer,
                variant: HeaderFooterVariant::Default,
                content: Some(content),
            })
        };
        let mut document = Document {
            blocks: vec![
                para(vec![image(0, Region::Body, one)]),
                footer(vec![
                    Block::BlockQuote(vec![para(vec![image(1, Region::Footer, one)])]),
                    para(vec![image(2, Region::Footer, one)]),
                ]),
                para(vec![image(0, Region::Body, one)]),
            ],
            assets: assets(&[PNG, JPEG, b"GIF89a"]),
            ..Document::default()
        };
        let diagnostics = left(&mut document);
        assert_eq!(
            document.blocks,
            [
                para(vec![image(0, Region::Body, one)]),
                gap(0, one, 1),
                footer(vec![
                    Block::BlockQuote(vec![
                        para(vec![image(1, Region::Footer, one)]),
                        gap(1, one, 2),
                    ]),
                    para(vec![image(2, Region::Footer, one)]),
                ]),
                para(vec![image(0, Region::Body, one)]),
                gap(0, one, 3),
            ]
        );
        let rows: Vec<_> = diagnostics.iter().map(|d| (d.code, d.region, d.count)).collect();
        assert_eq!(rows, [(DiagnosticCode::ImageNotOcred, Region::Footer, 1)]);
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

    fn png(name: [u8; 4], width: u32, height: u32) -> Vec<u8> {
        let sizes = [width.to_be_bytes(), height.to_be_bytes()].concat();
        [PNG_SIGNATURE, &13_u32.to_be_bytes(), &name, &sizes, &[8, 2, 0, 0, 0]].concat()
    }

    fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let length = u16::try_from(payload.len() + 2).unwrap();
        [&[JPEG_PREFIX, marker][..], &length.to_be_bytes(), payload].concat()
    }

    fn frame(marker: u8, width: u16, height: u16) -> Vec<u8> {
        let fields = [&[8][..], &height.to_be_bytes(), &width.to_be_bytes(), &[1, 1, 0x11, 0]];
        segment(marker, &fields.concat())
    }

    fn jpeg(parts: &[&[u8]]) -> Vec<u8> {
        [&[JPEG_PREFIX, JPEG_SOI][..], &parts.concat()].concat()
    }

    #[test]
    fn a_png_declares_its_size_in_its_first_chunk_within_the_format_s_range() {
        let truncated = png(*b"IHDR", 3, 5)[..20].to_vec();
        for (bytes, expected) in [
            (png(*b"IHDR", 3, 5), Some((3, 5))),
            (png(*b"IHDR", 5, 3), Some((5, 3))),
            (png(*b"IHDR", (1 << 31) - 1, 1), Some(((1 << 31) - 1, 1))),
            (png(*b"IHDR", 1 << 31, 1), None),
            (png(*b"IHDR", 1, 1 << 31), None),
            (png(*b"IHDR", 0, 5), None),
            (png(*b"IHDR", 3, 0), None),
            (png(*b"IDAT", 3, 5), None),
            (truncated, None),
            (PNG_SIGNATURE.to_vec(), None),
        ] {
            assert_eq!(dimensions(&bytes, "image/png"), expected, "{bytes:?}");
        }
    }

    #[test]
    fn a_jpeg_declares_its_size_in_its_first_frame_header_found_by_segment_lengths() {
        let decoy = frame(JPEG_SOF0, 9, 9);
        let app = segment(0xe0, &decoy[..]);
        let real = frame(JPEG_SOF0, 7, 2);
        let fills = [JPEG_PREFIX; 3];
        for (bytes, expected, why) in [
            (jpeg(&[&real]), Some((7, 2)), "a header straight after SOI"),
            (jpeg(&[&app, &real]), Some((7, 2)), "a frame header inside another segment"),
            (jpeg(&[&fills, &real]), Some((7, 2)), "fill bytes before a marker"),
            (jpeg(&[&[JPEG_PREFIX, JPEG_TEM], &real]), Some((7, 2)), "a marker without length"),
            (jpeg(&[&[JPEG_PREFIX, JPEG_RST0], &real]), Some((7, 2)), "a restart marker"),
            (jpeg(&[&frame(JPEG_DHT, 9, 9), &real]), Some((7, 2)), "a Huffman table"),
            (jpeg(&[&frame(JPEG_JPG_EXTENSION, 9, 9), &real]), Some((7, 2)), "a JPG extension"),
            (jpeg(&[&frame(JPEG_DAC, 9, 9), &real]), Some((7, 2)), "an arithmetic table"),
            (jpeg(&[&frame(JPEG_SOF0, 0, 2)]), None, "no width"),
            (jpeg(&[&frame(JPEG_SOF0, 7, 0)]), None, "a height left to DNL"),
            (jpeg(&[&segment(JPEG_SOS, &[0, 0]), &real]), None, "a scan before any frame"),
            (jpeg(&[&[JPEG_PREFIX, JPEG_EOI, 0, 2], &real]), None, "the end of the image"),
            (jpeg(&[&[0xdb, 0, 0xff], &real]), None, "a segment without its prefix"),
            (jpeg(&[&[JPEG_PREFIX, 0xdb, 0, 0], &real]), None, "a length below its own two bytes"),
            (jpeg(&[&[JPEG_PREFIX, 0xdb, 0, 1], &real]), None, "a length of one"),
            (jpeg(&[&real[..8]]), None, "a truncated frame header"),
            (jpeg(&[&app[..6]]), None, "a segment past the bytes"),
            (vec![JPEG_PREFIX, JPEG_SOI], None, "no segment"),
        ] {
            assert_eq!(dimensions(&bytes, "image/jpeg"), expected, "{why}");
        }
    }

    #[test]
    fn every_frame_marker_declares_the_size_and_no_other_marker_does() {
        for marker in 0..=u8::MAX {
            let framed = matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf);
            let read = dimensions(&jpeg(&[&frame(marker, 7, 2)]), "image/jpeg");
            assert_eq!(read, framed.then_some((7, 2)), "{marker:#x}");
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
