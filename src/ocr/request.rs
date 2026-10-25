use std::fmt;
use std::sync::Arc;

use crate::error::ConvertError;
use crate::formats::Source;
use crate::ledger::Ledger;
use crate::model::{Asset, Gap, GapId, GapReason, OcrOrigin};
use crate::ocr::RasterSpec;
use crate::ocr::images::media_type;
use crate::ocr::raster::page_image;

/// Bytes of image a request copies per unit of [`Limits::max_work`](crate::Limits::max_work).
pub(crate) const BYTES_PER_UNIT: u64 = 16;

/// One gap's image for an [`OcrEngine`](crate::OcrEngine) to read, and how much text its
/// fill may hold.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OcrRequest {
    /// The gap the text fills.
    pub gap: GapId,
    /// What the image is of.
    pub origin: OcrOrigin,
    /// Why the source's own text does not serve, as the gap holds them.
    pub reasons: Vec<GapReason>,
    /// The image to read.
    pub image: OcrImage,
    /// The most bytes of Markdown a fill takes: the parse's
    /// [`Limits::max_ocr_text_bytes`](crate::Limits::max_ocr_text_bytes).
    pub max_text_bytes: u64,
}

/// An image to read: an embedded one as its source holds it, or a page painted as a PNG.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OcrImage {
    /// The encoded image.
    pub bytes: Arc<[u8]>,
    /// `image/png` or `image/jpeg`, from the bytes' signature.
    pub media_type: &'static str,
    /// The resolution a page was painted at; `None` for an embedded image.
    pub dpi: Option<u32>,
}

impl fmt::Debug for OcrImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OcrImage")
            .field("media_type", &self.media_type)
            .field("bytes", &self.bytes.len())
            .field("dpi", &self.dpi)
            .finish()
    }
}

/// What a parsed document makes its gaps' requests from.
#[derive(Clone, Copy)]
pub(crate) struct Inputs<'a> {
    pub(crate) assets: &'a [Asset],
    pub(crate) source: &'a Source,
}

impl Inputs<'_> {
    /// Whether `gap` asks for an image an engine can read: an embedded PNG or JPEG, or a page
    /// of a source kept to paint it.
    pub(crate) fn asks(self, gap: &Gap) -> bool {
        match gap.origin {
            OcrOrigin::Page(_) => self.source.paints(),
            _ => image_of(gap, self.assets).is_some(),
        }
    }
}

/// The image `gap` asks to be read, among `assets`: an embedded PNG or JPEG.
pub(crate) fn image_of<'a>(gap: &Gap, assets: &'a [Asset]) -> Option<(&'a [u8], &'static str)> {
    let OcrOrigin::Image(id) = gap.origin else { return None };
    let bytes = assets.get(id.0)?.bytes.as_slice();
    Some((bytes, media_type(bytes)?))
}

/// The request for `gap`: an embedded image copied, or a page painted under `spec`, each byte
/// charged to `ledger` at one unit per [`BYTES_PER_UNIT`]; `None` when it asks for no image
/// or its page cannot be painted.
pub(crate) fn request(
    gap: &Gap,
    inputs: Inputs<'_>,
    spec: &RasterSpec,
    ledger: &Ledger<'_>,
    max_text_bytes: u64,
) -> Result<Option<OcrRequest>, ConvertError> {
    let image = match gap.origin {
        OcrOrigin::Page(page) => page_image(inputs.source, page, spec, ledger)?,
        _ => match image_of(gap, inputs.assets) {
            Some((bytes, media_type)) => {
                ledger.charge(byte_units(bytes.len()))?;
                Some(OcrImage { bytes: Arc::from(bytes), media_type, dpi: None })
            }
            None => None,
        },
    };
    Ok(image.map(|image| OcrRequest {
        gap: gap.id,
        origin: gap.origin,
        reasons: gap.reasons.clone(),
        image,
        max_text_bytes,
    }))
}

/// Units of work `len` bytes of image cost.
pub(crate) fn byte_units(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX).div_ceil(BYTES_PER_UNIT)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::limits::Limit;
    use crate::model::AssetId;

    fn asset(bytes: &[u8]) -> Asset {
        Asset {
            id: AssetId(0),
            media_type: "image/gif".into(),
            origin_part: String::new(),
            bytes: bytes.to_vec(),
        }
    }

    fn gap(origin: OcrOrigin) -> Gap {
        Gap { id: GapId::new(None, NonZeroU32::MIN), origin, reasons: Vec::new() }
    }

    fn requested(
        gap: &Gap,
        assets: &[Asset],
        ledger: &Ledger<'_>,
    ) -> Result<Option<OcrRequest>, ConvertError> {
        let source = Source::default();
        request(gap, Inputs { assets, source: &source }, &RasterSpec::default(), ledger, 7)
    }

    #[test]
    fn an_image_request_copies_its_asset_at_one_unit_per_sixteen_bytes() {
        let png = [b"\x89PNG\r\n\x1a\n".as_slice(), &[0; 25]].concat();
        let ledger = Ledger::new(u64::MAX, None);
        let made = requested(&gap(OcrOrigin::Image(AssetId(0))), &[asset(&png)], &ledger);
        let made = made.unwrap().unwrap();
        assert_eq!((&*made.image.bytes, made.image.media_type), (png.as_slice(), "image/png"));
        assert_eq!((made.max_text_bytes, ledger.spent()), (7, 3));
        assert_eq!(
            format!("{:?}", made.image),
            r#"OcrImage { media_type: "image/png", bytes: 33, dpi: None }"#
        );
        let short = Ledger::new(2, None);
        let refused = requested(&gap(OcrOrigin::Image(AssetId(0))), &[asset(&png)], &short);
        assert_eq!(refused, Err(ConvertError::limit(Limit::Work, 2)));
    }

    #[test]
    fn no_image_no_request() {
        let page = OcrOrigin::Page(NonZeroU32::MIN);
        let cases = [
            (gap(page), b"\xff\xd8\xff".as_slice()),
            (gap(OcrOrigin::Image(AssetId(0))), b"GIF89a".as_slice()),
            (gap(OcrOrigin::Image(AssetId(1))), b"\xff\xd8\xff".as_slice()),
        ];
        for (gap, bytes) in cases {
            let ledger = Ledger::new(u64::MAX, None);
            assert_eq!(requested(&gap, &[asset(bytes)], &ledger), Ok(None), "{gap:?}");
            assert_eq!(ledger.spent(), 0);
        }
    }
}
