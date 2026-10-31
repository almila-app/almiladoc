//! Embedded-asset accumulation under the retained-bytes cap.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{Asset, AssetId, AssetPhase, Document, ImageSource, visit_asset_ids};
use crate::package::Package;
use crate::package::relationships::{Relationships, TargetMode, rel_target_bytes};
use std::cell::RefCell;
use std::collections::HashMap;

const OLE_OBJECT_MEDIA_TYPE: &str = "application/vnd.ms-ole-object";

pub(crate) fn order_assets(
    ctx: &ParseCtx<'_>,
    document: &mut Document,
) -> Result<(), ConvertError> {
    let mut order = AssetOrder::new(ctx, document.assets.len())?;
    if document.assets.is_empty() {
        return Ok(());
    }
    for (index, asset) in document.assets.iter().enumerate() {
        ctx.charge(1)?;
        if asset.id.0 != index {
            return Err(ConvertError::Internal);
        }
    }
    for phase in [AssetPhase::Main, AssetPhase::Side] {
        visit_asset_ids(document, phase, |work| ctx.charge(work), |id| order.assign(ctx, id))?;
    }
    for index in 0..document.assets.len() {
        order.assign(ctx, &mut AssetId(index))?;
    }
    order.permute(ctx, &mut document.assets)
}

struct AssetOrder {
    positions: Vec<usize>,
    next: usize,
}

impl AssetOrder {
    fn new(ctx: &ParseCtx<'_>, count: usize) -> Result<Self, ConvertError> {
        ctx.charge(count as u64)?;
        let mut positions = Vec::new();
        positions.try_reserve_exact(count).map_err(|_| {
            ConvertError::limit(Limit::Allocation, count.saturating_mul(size_of::<usize>()) as u64)
        })?;
        positions.resize(count, usize::MAX);
        Ok(Self { positions, next: 0 })
    }

    fn assign(&mut self, ctx: &ParseCtx<'_>, id: &mut AssetId) -> Result<(), ConvertError> {
        ctx.charge(1)?;
        let position = self.positions.get_mut(id.0).ok_or(ConvertError::Internal)?;
        if *position == usize::MAX {
            *position = self.next;
            self.next = self.next.checked_add(1).ok_or(ConvertError::Internal)?;
        }
        id.0 = *position;
        Ok(())
    }

    fn permute(&mut self, ctx: &ParseCtx<'_>, assets: &mut [Asset]) -> Result<(), ConvertError> {
        for index in 0..assets.len() {
            ctx.charge(1)?;
            loop {
                let target = *self.positions.get(index).ok_or(ConvertError::Internal)?;
                if target == index {
                    break;
                }
                self.positions.get(target).ok_or(ConvertError::Internal)?;
                ctx.charge(1)?;
                assets.swap(index, target);
                self.positions.swap(index, target);
            }
            ctx.charge(1)?;
            assets.get_mut(index).ok_or(ConvertError::Internal)?.id = AssetId(index);
        }
        Ok(())
    }
}

pub(crate) struct AssetSink {
    pub assets: Vec<Asset>,
    /// Origin part -> retained asset, so repeated references to the same
    /// part share one asset instead of duplicating its bytes (and falsely
    /// accumulating against the cap).
    by_part: HashMap<String, AssetId>,
    total: u64,
    max_total: u64,
}

impl AssetSink {
    pub(crate) fn new(parse_ctx: &ParseCtx<'_>) -> Self {
        Self {
            assets: Vec::new(),
            by_part: HashMap::new(),
            total: 0,
            max_total: parse_ctx.limits().max_asset_total_bytes,
        }
    }

    /// Retain an asset's bytes (copied only when the part is not already
    /// retained). Crossing the retained-bytes cap is a hard error.
    pub(crate) fn add(
        &mut self,
        media_type: String,
        origin_part: String,
        bytes: &[u8],
    ) -> Result<AssetId, ConvertError> {
        if let Some(&id) = self.by_part.get(&origin_part) {
            return Ok(id);
        }
        self.total = self.total.saturating_add(bytes.len() as u64);
        if self.total > self.max_total {
            return Err(ConvertError::limit(Limit::AssetTotalBytes, self.max_total));
        }
        let id = AssetId(self.assets.len());
        self.by_part.insert(origin_part.clone(), id);
        self.assets.push(Asset { id, media_type, origin_part, bytes: bytes.to_vec() });
        Ok(id)
    }
}

/// Resolve an image relationship to its source. External-mode relationships
/// carry the image's URL directly; internal-mode targets load from the
/// package and are retained as assets. Failures degrade to `None` per the
/// unified policy; fatal errors propagate.
pub(crate) fn rel_image_source(
    pkg: &Package<'_>,
    rels: &Relationships,
    base_part: &str,
    assets: &RefCell<AssetSink>,
    rel_id: &str,
) -> Result<Option<ImageSource>, ConvertError> {
    let Some(rel) = rels.get(rel_id) else {
        return Ok(None);
    };
    if rel.mode == TargetMode::External {
        return Ok((!rel.target.is_empty()).then(|| ImageSource::External(rel.target.clone())));
    }
    let Some((part, bytes)) = rel_target_bytes(pkg, rels, base_part, rel_id)? else {
        return Ok(None);
    };
    let media = media_type_for(&part);
    let id = assets.borrow_mut().add(media, part, &bytes)?;
    Ok(Some(ImageSource::Asset(id)))
}

/// Retain an embedded OLE object's payload as an asset, when its relationship resolves to a part.
pub(crate) fn retain_ole_payload(
    pkg: &Package<'_>,
    rels: &Relationships,
    base_part: &str,
    assets: &RefCell<AssetSink>,
    rel_id: Option<&str>,
) -> Result<(), ConvertError> {
    if let Some((part, bytes)) =
        rel_id.map(|id| rel_target_bytes(pkg, rels, base_part, id)).transpose()?.flatten()
    {
        assets.borrow_mut().add(OLE_OBJECT_MEDIA_TYPE.into(), part, &bytes)?;
    }
    Ok(())
}

/// MIME type from a part path's extension.
pub(crate) fn media_type_for(part: &str) -> String {
    let ext = part.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        "emf" => "image/emf",
        "wmf" => "image/wmf",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Block, Cell, Gap, GapId, GridBuilder, HeaderFooter, HeaderFooterRegion,
        HeaderFooterVariant, ImageOrigin, Inline, LinkTarget, List, ListItem, MarkerKind, Note,
        NoteKind, OcrOrigin, Region, Table,
    };
    use std::assert_matches;
    use std::num::NonZeroU32;

    fn image(id: usize) -> Inline {
        Inline::Image {
            alt: "image".into(),
            source: ImageSource::Asset(AssetId(id)),
            origin: ImageOrigin::new(Region::Body, None),
        }
    }

    fn images(ids: &[usize]) -> Block {
        Block::Paragraph(ids.iter().map(|id| image(*id)).collect())
    }

    fn side(content: Option<Vec<Block>>) -> Block {
        Block::HeaderFooter(HeaderFooter {
            region: HeaderFooterRegion::Header,
            variant: HeaderFooterVariant::Default,
            content,
        })
    }

    fn note(id: &str, blocks: Vec<Block>) -> Note {
        Note { id: id.into(), kind: NoteKind::Footnote, blocks }
    }

    fn document(blocks: Vec<Block>, notes: Vec<Note>, count: usize) -> Document {
        let mut sink = AssetSink::new(&ParseCtx::default());
        for id in 0..count {
            sink.add(format!("application/{id}"), format!("part/{id}"), &id.to_le_bytes()).unwrap();
        }
        Document { blocks, notes, assets: sink.assets, ..Document::default() }
    }

    fn assert_order(document: &mut Document, order: &[usize]) {
        let original = document.assets.clone();
        let pointers: Vec<_> = document.assets.iter().map(|asset| asset.bytes.as_ptr()).collect();
        order_assets(&ParseCtx::default(), document).unwrap();
        let expected: Vec<_> = order
            .iter()
            .enumerate()
            .map(|(index, old)| Asset { id: AssetId(index), ..original[*old].clone() })
            .collect();
        assert_eq!(document.assets, expected);
        for (asset, old) in document.assets.iter().zip(order) {
            assert_eq!(asset.bytes.as_ptr(), pointers[*old]);
        }
    }

    #[test]
    fn cycles_remap_repeated_references_and_preserve_unreferenced_payloads() {
        let mut doc = document(vec![images(&[2, 0, 1, 2])], Vec::new(), 5);
        assert_order(&mut doc, &[2, 0, 1, 3, 4]);
        assert_eq!(doc.blocks, vec![images(&[0, 1, 2, 0])]);
    }

    #[test]
    fn notes_keep_definition_order_and_sides_follow_every_main_story() {
        let blocks = vec![
            side(Some(vec![images(&[3]), side(Some(vec![images(&[0])]))])),
            images(&[2]),
            Block::Paragraph(vec![Inline::NoteRef("b".into()), Inline::NoteRef("a".into())]),
            side(None),
            side(Some(Vec::new())),
        ];
        let notes = vec![
            note("a", vec![side(Some(vec![images(&[6, 5])])), images(&[1])]),
            note("b", vec![images(&[4, 2])]),
        ];
        let mut doc = document(blocks, notes, 8);
        assert_order(&mut doc, &[2, 1, 4, 3, 0, 6, 5, 7]);
        assert_eq!(doc.blocks[0], side(Some(vec![images(&[3]), side(Some(vec![images(&[4])]))])));
        assert_eq!(doc.blocks[1], images(&[0]));
        assert_eq!(doc.notes[0], note("a", vec![side(Some(vec![images(&[5, 6])])), images(&[1])]));
        assert_eq!(doc.notes[1], note("b", vec![images(&[2, 0])]));
        assert_eq!(doc.blocks[3..], [side(None), side(Some(Vec::new()))]);
    }

    fn list(blocks: Vec<Block>) -> Block {
        Block::List(List {
            marker: MarkerKind::Bullet,
            start: 1,
            items: vec![ListItem::default(), ListItem { blocks, marker_label: Some("x".into()) }],
        })
    }

    fn containers(ids: &[usize]) -> Vec<Block> {
        let content = ids.iter().map(|id| image(*id)).collect();
        let linked = Inline::Link {
            content: vec![Inline::Link {
                content,
                target: LinkTarget::External("https://example.org/inner".into()),
            }],
            target: LinkTarget::Relative("outer".into()),
        };
        let heading = Block::Heading { level: 2, content: vec![linked], anchor: Some("h".into()) };
        let table =
            Table::from_rows(vec![Vec::new(), vec![Cell::new(vec![list(vec![heading])])]], 0);
        vec![Block::BlockQuote(vec![list(vec![Block::Table(table)])])]
    }

    #[test]
    fn every_nested_container_preserves_reference_order_and_fields() {
        let mut doc = document(containers(&[2, 0, 1]), Vec::new(), 4);
        assert_order(&mut doc, &[2, 0, 1, 3]);
        assert_eq!(doc.blocks, containers(&[0, 1, 2]));
    }

    #[test]
    fn side_context_survives_each_nested_container() {
        let blocks = vec![side(Some(containers(&[3, 0]))), images(&[2])];
        let mut doc = document(blocks, vec![note("n", vec![images(&[1])])], 5);
        assert_order(&mut doc, &[2, 1, 3, 0, 4]);
        assert_eq!(doc.blocks, vec![side(Some(containers(&[2, 3]))), images(&[0])]);
        assert_eq!(doc.notes, vec![note("n", vec![images(&[1])])]);
    }

    fn spanning_table(ids: &[usize]) -> Table {
        let ctx = ParseCtx::default();
        let mut grid = GridBuilder::new(&ctx);
        grid.place(Cell::spanning(vec![images(&ids[..1])], 2, 2)).unwrap();
        grid.next_row();
        grid.next_row();
        grid.next_row();
        grid.place(Cell::new(vec![images(&ids[1..])])).unwrap();
        grid.finish_preserving_rows().unwrap()
    }

    #[test]
    fn covered_slots_and_empty_rows_never_repeat_origin_cells() {
        let table = spanning_table(&[2, 0, 1]);
        assert_eq!(table.grid.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2, 0, 1]);
        let mut doc = document(vec![Block::Table(table)], Vec::new(), 4);
        assert_order(&mut doc, &[2, 0, 1, 3]);
        assert_eq!(doc.blocks, vec![Block::Table(spanning_table(&[0, 1, 2]))]);
    }

    #[test]
    fn mapping_allocation_refusal_is_a_resource_limit() {
        let limits = crate::Limits { max_work: u64::MAX, ..crate::Limits::default() };
        let count = usize::MAX;
        assert_eq!(
            AssetOrder::new(&ParseCtx::new(&limits), count).map(|_| ()),
            Err(ConvertError::limit(
                Limit::Allocation,
                count.saturating_mul(size_of::<usize>()) as u64,
            )),
        );
    }

    fn gap(ordinal: u32, origin: OcrOrigin) -> Block {
        Block::Gap(Gap {
            id: GapId::new(None, NonZeroU32::new(ordinal).unwrap()),
            origin,
            reasons: vec![crate::model::GapReason::CallerRequested],
        })
    }

    #[test]
    fn gap_origins_are_references_without_changing_gap_identity() {
        let blocks = vec![
            images(&[2]),
            gap(1, OcrOrigin::Image(AssetId(2))),
            images(&[0]),
            gap(2, OcrOrigin::Page(NonZeroU32::new(7).unwrap())),
            gap(3, OcrOrigin::Image(AssetId(3))),
        ];
        let mut doc = document(blocks, Vec::new(), 5);
        assert_order(&mut doc, &[2, 0, 3, 1, 4]);
        assert_eq!(
            doc.blocks,
            vec![
                images(&[0]),
                gap(1, OcrOrigin::Image(AssetId(0))),
                images(&[1]),
                gap(2, OcrOrigin::Page(NonZeroU32::new(7).unwrap())),
                gap(3, OcrOrigin::Image(AssetId(2))),
            ]
        );
    }

    #[test]
    fn equal_bytes_at_distinct_origins_keep_distinct_ids() {
        let mut doc = document(vec![images(&[1, 0, 1])], Vec::new(), 2);
        doc.assets[1].bytes = doc.assets[0].bytes.clone();
        assert_order(&mut doc, &[1, 0]);
        assert_eq!(doc.blocks, vec![images(&[0, 1, 0])]);
    }

    #[test]
    fn invalid_nonempty_asset_indices_are_internal_errors() {
        let mut inconsistent = document(Vec::new(), Vec::new(), 2);
        inconsistent.assets[1].id = AssetId(0);
        for mut doc in [
            inconsistent,
            document(vec![images(&[2])], Vec::new(), 2),
            document(vec![gap(1, OcrOrigin::Image(AssetId(3)))], Vec::new(), 2),
        ] {
            assert_eq!(order_assets(&ParseCtx::default(), &mut doc), Err(ConvertError::Internal));
        }
    }

    #[test]
    fn empty_identity_and_unreferenced_only_orders_stay_unchanged() {
        for mut doc in [
            Document::default(),
            document(vec![images(&[0])], Vec::new(), 1),
            document(vec![images(&[0, 1, 2])], Vec::new(), 3),
            document(Vec::new(), Vec::new(), 3),
        ] {
            let expected = doc.clone();
            order_assets(&ParseCtx::default(), &mut doc).unwrap();
            assert_eq!(doc, expected);
        }
    }

    #[test]
    fn ordering_charges_the_callers_remaining_work() {
        for (spent, max, expected) in [
            (0, 49, Ok(())),
            (0, 48, Err(ConvertError::limit(Limit::Work, 48))),
            (7, 56, Ok(())),
            (7, 55, Err(ConvertError::limit(Limit::Work, 55))),
        ] {
            let limits = crate::Limits { max_work: max, ..crate::Limits::default() };
            let ctx = ParseCtx::new(&limits).starting_at(spent);
            let mut doc = document(vec![images(&[2, 0, 1, 2])], Vec::new(), 5);
            assert_eq!(order_assets(&ctx, &mut doc), expected);
            assert_eq!(ctx.ledger().spent(), spent + 49);
        }
    }

    #[test]
    fn cancellation_is_checked_with_and_without_assets() {
        let token = crate::CancelToken::new();
        let ctx = crate::shared::test_support::cancelled(&token);
        for mut doc in [Document::default(), document(vec![images(&[1, 0])], Vec::new(), 2)] {
            assert_eq!(order_assets(&ctx, &mut doc), Err(ConvertError::Cancelled));
        }
    }

    #[test]
    fn ordering_prices_each_structural_walk_and_asset_only_work() {
        let cases = [
            ("unreferenced", document(Vec::new(), Vec::new(), 2), 14),
            ("note", document(Vec::new(), vec![note("n", Vec::new())], 2), 20),
            ("list", document(vec![list(Vec::new())], Vec::new(), 2), 38),
            ("table", document(vec![Block::Table(spanning_table(&[1, 0]))], Vec::new(), 2), 101),
            ("side", document(vec![side(Some(vec![images(&[1, 0])]))], Vec::new(), 2), 31),
            ("nested", document(containers(&[1, 0]), Vec::new(), 2), 123),
            ("gap", document(vec![gap(1, OcrOrigin::Image(AssetId(1)))], Vec::new(), 2), 20),
        ];
        for (label, original, work) in cases {
            for max in [work, work - 1] {
                let limits = crate::Limits { max_work: max, ..crate::Limits::default() };
                let ctx = ParseCtx::new(&limits);
                let mut doc = original.clone();
                let expected =
                    if max == work { Ok(()) } else { Err(ConvertError::limit(Limit::Work, max)) };
                assert_eq!(order_assets(&ctx, &mut doc), expected, "{label}: {max}");
                if max == work {
                    assert_eq!(ctx.ledger().spent(), work, "{label}");
                }
            }
        }
    }

    #[test]
    fn repeated_origin_parts_share_one_asset() {
        let mut sink = AssetSink::new(&ParseCtx::default());
        let a = sink.add("image/png".into(), "media/one.png".into(), &[1; 64]).unwrap();
        let b = sink.add("image/png".into(), "media/one.png".into(), &[1; 64]).unwrap();
        assert_eq!(a, b);
        assert_eq!(sink.assets.len(), 1);
        assert_eq!(sink.total, 64, "repeated references must not re-count bytes");
    }

    #[test]
    fn repeated_references_cannot_trip_the_cap() {
        let mut sink = AssetSink::new(&ParseCtx::default());
        sink.total = sink.max_total - 10;
        for _ in 0..8 {
            sink.add("image/png".into(), "media/big.png".into(), &[0; 8]).unwrap();
        }
    }

    #[test]
    fn crossing_the_cap_is_a_hard_error() {
        let mut sink = AssetSink::new(&ParseCtx::default());
        sink.total = sink.max_total - 10;
        let err = sink.add("image/png".into(), "media/big.png".into(), &[0; 11]).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::AssetTotalBytes, .. });
    }

    #[test]
    fn the_cap_is_the_contexts() {
        let limits =
            crate::limits::Limits { max_asset_total_bytes: 10, ..crate::limits::Limits::default() };
        let mut sink = AssetSink::new(&ParseCtx::new(&limits));
        sink.add("image/png".into(), "media/a.png".into(), &[0; 10]).unwrap();
        let err = sink.add("image/png".into(), "media/b.png".into(), &[0; 1]).unwrap_err();
        assert_matches!(
            err,
            ConvertError::ResourceLimit { limit: Limit::AssetTotalBytes, max: 10 }
        );
    }
}
