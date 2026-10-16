//! Embedded-asset accumulation under the retained-bytes cap.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{Asset, AssetId, ImageSource};
use crate::package::Package;
use crate::package::relationships::{Relationships, TargetMode, rel_target_bytes};
use std::cell::RefCell;

pub struct AssetSink {
    pub assets: Vec<Asset>,
    /// Origin part -> retained asset, so repeated references to the same
    /// part share one asset instead of duplicating its bytes (and falsely
    /// accumulating against the cap).
    by_part: std::collections::HashMap<String, AssetId>,
    total: u64,
    max_total: u64,
}

impl AssetSink {
    pub fn new(parse_ctx: &ParseCtx) -> Self {
        AssetSink {
            assets: Vec::new(),
            by_part: std::collections::HashMap::new(),
            total: 0,
            max_total: parse_ctx.limits().max_asset_total_bytes,
        }
    }

    /// Retain an asset's bytes (copied only when the part is not already
    /// retained). Crossing the retained-bytes cap is a hard error.
    pub fn add(
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
pub fn rel_image_source(
    pkg: &Package,
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
    match rel_target_bytes(pkg, rels, base_part, rel_id)? {
        Some((part, bytes)) => {
            let media = media_type_for(&part);
            let id = assets.borrow_mut().add(media, part, &bytes)?;
            Ok(Some(ImageSource::Asset(id)))
        }
        None => Ok(None),
    }
}

/// MIME type from a part path's extension.
pub fn media_type_for(part: &str) -> String {
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
        // Preload the counter to just under the cap (avoids large real
        // allocations, which would feed the R22 memory test's counter).
        let mut sink = AssetSink::new(&ParseCtx::default());
        sink.total = sink.max_total - 10;
        for _ in 0..8 {
            assert!(sink.add("image/png".into(), "media/big.png".into(), &[0; 8]).is_ok());
        }
    }

    #[test]
    fn crossing_the_cap_is_a_hard_error() {
        let mut sink = AssetSink::new(&ParseCtx::default());
        sink.total = sink.max_total - 10;
        let err = sink.add("image/png".into(), "media/big.png".into(), &[0; 11]).unwrap_err();
        assert!(matches!(err, ConvertError::ResourceLimit { limit: Limit::AssetTotalBytes, .. }));
    }

    #[test]
    fn the_cap_is_the_contexts() {
        let limits =
            crate::limits::Limits { max_asset_total_bytes: 10, ..crate::limits::Limits::default() };
        let mut sink = AssetSink::new(&ParseCtx::new(&limits));
        assert!(sink.add("image/png".into(), "media/a.png".into(), &[0; 10]).is_ok());
        let err = sink.add("image/png".into(), "media/b.png".into(), &[0; 1]).unwrap_err();
        assert!(matches!(
            err,
            ConvertError::ResourceLimit { limit: Limit::AssetTotalBytes, max: 10 }
        ));
    }
}
