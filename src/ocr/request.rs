use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;

use crate::error::ConvertError;
use crate::formats::Source;
use crate::ledger::{CancelToken, Ledger};
use crate::model::{Asset, Gap, GapId, GapReason, OcrOrigin};
use crate::ocr::images::media_type;
use crate::ocr::{RasterSpec, raster};
use crate::pool::map_ordered;

/// Bytes of image a request copies per unit of [`Limits::max_work`](crate::Limits::max_work).
const BYTES_PER_UNIT: u64 = 16;

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

impl OcrImage {
    /// The image `bytes` hold, painted at `dpi` if a page; `None` unless they open with the
    /// PNG or JPEG signature.
    ///
    /// # Examples
    ///
    /// ```
    /// use almiladoc::OcrImage;
    ///
    /// let image = OcrImage::new(*b"\xff\xd8\xff\xe0", None).ok_or("not an image")?;
    /// assert_eq!(image.media_type, "image/jpeg");
    /// assert_eq!(OcrImage::new(*b"GIF89a", None), None);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn new(bytes: impl Into<Arc<[u8]>>, dpi: Option<u32>) -> Option<Self> {
        let bytes = bytes.into();
        let media_type = media_type(&bytes)?;
        Some(Self { bytes, media_type, dpi })
    }
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
fn image_of<'a>(gap: &Gap, assets: &'a [Asset]) -> Option<(&'a [u8], &'static str)> {
    let OcrOrigin::Image(id) = gap.origin else { return None };
    let bytes = assets.get(id.0)?.bytes.as_slice();
    Some((bytes, media_type(bytes)?))
}

/// How a gap's image is had: an embedded one copied, or a page to paint at a dpi.
enum Plan {
    Image(OcrImage),
    Page { page: NonZeroU32, dpi: u32 },
}

/// The request of each of `gaps` that asks for an image, in order, with how much of its page
/// painting left out: embedded images copied, then pages painted under `spec` up to the
/// pool's threads at once, each charged to `ledger` (a page's pixels before the pages are
/// painted, every image's bytes at one unit per [`BYTES_PER_UNIT`]); a page unpdf cannot
/// paint asks for none.
pub(crate) fn requests(
    gaps: &[Gap],
    inputs: Inputs<'_>,
    spec: &RasterSpec,
    ledger: &Ledger<'_>,
    max_text_bytes: u64,
) -> Result<Vec<(OcrRequest, u64)>, ConvertError> {
    let mut plans = Vec::with_capacity(gaps.len());
    for gap in gaps {
        let plan = match gap.origin {
            OcrOrigin::Page(page) => {
                raster::plan(inputs.source, page, spec, ledger)?.map(|dpi| Plan::Page { page, dpi })
            }
            _ => embedded(gap, inputs.assets, ledger)?.map(Plan::Image),
        };
        plans.extend(plan.map(|plan| (gap, plan)));
    }
    let cancel = ledger.cancel();
    let painted = map_ordered(&plans, |(_, plan)| match *plan {
        Plan::Page { .. } if cancel.is_some_and(CancelToken::is_cancelled) => {
            Err(ConvertError::Cancelled)
        }
        Plan::Page { page, dpi } => Ok(raster::paint(inputs.source, page, dpi)),
        Plan::Image(_) => Ok(None),
    });
    let mut made = Vec::with_capacity(plans.len());
    for ((gap, plan), painted) in plans.into_iter().zip(painted) {
        let (image, unpainted) = match (plan, painted?) {
            (Plan::Image(image), _) => (image, 0),
            (Plan::Page { dpi, .. }, Some(painted)) => {
                ledger.charge(byte_units(painted.png.len()))?;
                let png = Arc::from(painted.png);
                (
                    OcrImage { bytes: png, media_type: "image/png", dpi: Some(dpi) },
                    painted.unpainted,
                )
            }
            (Plan::Page { .. }, None) => continue,
        };
        let (gap, origin, reasons) = (gap.id, gap.origin, gap.reasons.clone());
        made.push((OcrRequest { gap, origin, reasons, image, max_text_bytes }, unpainted));
    }
    Ok(made)
}

/// `gap`'s embedded PNG or JPEG copied, its bytes charged to `ledger`.
fn embedded(
    gap: &Gap,
    assets: &[Asset],
    ledger: &Ledger<'_>,
) -> Result<Option<OcrImage>, ConvertError> {
    let Some((bytes, media_type)) = image_of(gap, assets) else { return Ok(None) };
    ledger.charge(byte_units(bytes.len()))?;
    Ok(Some(OcrImage { bytes: Arc::from(bytes), media_type, dpi: None }))
}

/// Units of work `len` bytes of image cost.
fn byte_units(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX).div_ceil(BYTES_PER_UNIT)
}

#[cfg(test)]
mod tests {
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
        let (source, gaps) = (Source::default(), [gap.clone()]);
        let inputs = Inputs { assets, source: &source };
        let made = requests(&gaps, inputs, &RasterSpec::default(), ledger, 7)?;
        Ok(made.into_iter().next().map(|(request, _)| request))
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
