use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;

use crate::error::ConvertError;
use crate::formats::Source;
use crate::ledger::Ledger;
use crate::model::{Asset, Gap, GapId, GapReason, OcrOrigin};
use crate::ocr::images::{dimensions, media_type};
use crate::ocr::raster::{Painted, RasterFidelity, whole_dpi};
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
    /// The resolution a page was painted at, in whole dots per inch from one to `u32::MAX`,
    /// rounded and clamped when the page was painted at an exact long edge; `None` for an
    /// embedded image.
    pub dpi: Option<u32>,
    /// The pixel width of a painted page, or of an embedded image as its header declares it;
    /// `None` when that header does not read.
    pub width: Option<u32>,
    /// The pixel height of a painted page, or of an embedded image as its header declares it;
    /// `None` when that header does not read.
    pub height: Option<u32>,
    /// What painting a page left out; `None` for an embedded image, which is not painted.
    pub fidelity: Option<RasterFidelity>,
}

impl OcrImage {
    /// The image `bytes` hold, painted at `dpi` if a page, its size as its header declares it
    /// and its fidelity unknown; `None` unless they open with the PNG or JPEG signature.
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
        let (width, height) = dimensions(&bytes, media_type).unzip();
        Some(Self { bytes, media_type, dpi, width, height, fidelity: None })
    }
}

impl fmt::Debug for OcrImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OcrImage")
            .field("media_type", &self.media_type)
            .field("bytes", &self.bytes.len())
            .field("dpi", &self.dpi)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fidelity", &self.fidelity)
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

/// A gap [`Parsed::ocr_requests`](crate::Parsed::ocr_requests) could not make an image for,
/// which stays unfilled.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OcrRefusal {
    /// The gap the image would have filled.
    pub gap: GapId,
    /// What the image would have been of.
    pub origin: OcrOrigin,
    /// Why the source's own text does not serve, as the gap holds them.
    pub reasons: Vec<GapReason>,
    /// Why there is no image.
    pub kind: RefusalKind,
}

/// Why a gap has no image: closed, so it carries no text of the painter's error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RefusalKind {
    /// The page cannot be painted: it has no usable box, or the painter failed on it.
    Unpaintable,
    /// The exact long edge would paint more pixels than [`RasterSpec::max_pixels`] allows.
    OverPixels {
        /// The pixels the page would have held.
        pixels: u64,
    },
    /// The encoded image holds more bytes than [`RasterSpec::max_bytes`] allows.
    OverBytes {
        /// The bytes the encoded image holds.
        bytes: u64,
    },
}

impl RefusalKind {
    /// The kind's stable `snake_case` name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unpaintable => "unpaintable",
            Self::OverPixels { .. } => "over_pixels",
            Self::OverBytes { .. } => "over_bytes",
        }
    }
}

/// How a gap's image is had: an embedded one copied, a page to paint at a dpi, or no image.
enum Plan {
    Image(OcrImage),
    Page { page: NonZeroU32, dpi: f32 },
    Refused(RefusalKind),
}

/// The request or refusal of each of `gaps` that asks for an image, in order, charged to
/// `ledger` as [`crate::Parsed::ocr_requests`] says.
pub(crate) fn requests(
    gaps: &[Gap],
    inputs: Inputs<'_>,
    spec: &RasterSpec,
    ledger: &Ledger<'_>,
    max_text_bytes: u64,
) -> Result<Vec<Result<OcrRequest, OcrRefusal>>, ConvertError> {
    let mut plans = Vec::with_capacity(gaps.len());
    for gap in gaps {
        let plan = match gap.origin {
            OcrOrigin::Page(page) => Some(
                raster::plan(inputs.source, page, spec, ledger)?
                    .map_or_else(Plan::Refused, |dpi| Plan::Page { page, dpi }),
            ),
            _ => embedded(gap, inputs.assets, spec, ledger)?,
        };
        plans.extend(plan.map(|plan| (gap, plan)));
    }
    let shared = ledger.share();
    let painted = map_ordered(&plans, |(_, plan)| match *plan {
        Plan::Page { page, dpi } => raster::paint(inputs.source, page, dpi, &shared),
        Plan::Image(_) | Plan::Refused(_) => Ok(None),
    });
    ledger.settle(shared);
    let mut made = Vec::with_capacity(plans.len());
    for ((gap, plan), painted) in plans.into_iter().zip(painted) {
        let image = match (plan, painted?) {
            (Plan::Image(image), _) => Ok(image),
            (Plan::Refused(kind), _) => Err(kind),
            (Plan::Page { dpi, .. }, Some(painted)) => page_image(painted, dpi, spec, ledger)?,
            (Plan::Page { .. }, None) => Err(RefusalKind::Unpaintable),
        };
        ledger.charge(0)?;
        let (id, origin, reasons) = (gap.id, gap.origin, gap.reasons.clone());
        made.push(match image {
            Ok(image) => Ok(OcrRequest { gap: id, origin, reasons, image, max_text_bytes }),
            Err(kind) => Err(OcrRefusal { gap: id, origin, reasons, kind }),
        });
    }
    Ok(made)
}

/// `painted` as a page's image, its PNG bytes charged to `ledger` even when `spec` refuses them.
fn page_image(
    painted: Painted,
    dpi: f32,
    spec: &RasterSpec,
    ledger: &Ledger<'_>,
) -> Result<Result<OcrImage, RefusalKind>, ConvertError> {
    let bytes = byte_count(painted.png.len());
    ledger.charge(byte_units(bytes))?;
    if over(spec, bytes) {
        return Ok(Err(RefusalKind::OverBytes { bytes }));
    }
    Ok(Ok(OcrImage {
        bytes: Arc::from(painted.png),
        media_type: "image/png",
        dpi: Some(whole_dpi(dpi)),
        width: Some(painted.width),
        height: Some(painted.height),
        fidelity: Some(painted.fidelity),
    }))
}

/// `gap`'s embedded PNG or JPEG copied, its bytes charged to `ledger`, or refused uncopied when
/// `spec` bounds it.
fn embedded(
    gap: &Gap,
    assets: &[Asset],
    spec: &RasterSpec,
    ledger: &Ledger<'_>,
) -> Result<Option<Plan>, ConvertError> {
    let Some((bytes, media_type)) = image_of(gap, assets) else { return Ok(None) };
    let len = byte_count(bytes.len());
    if over(spec, len) {
        return Ok(Some(Plan::Refused(RefusalKind::OverBytes { bytes: len })));
    }
    ledger.charge(byte_units(len))?;
    let (width, height) = dimensions(bytes, media_type).unzip();
    Ok(Some(Plan::Image(OcrImage {
        bytes: Arc::from(bytes),
        media_type,
        dpi: None,
        width,
        height,
        fidelity: None,
    })))
}

fn over(spec: &RasterSpec, bytes: u64) -> bool {
    spec.max_bytes.is_some_and(|max| bytes > max.get())
}

fn byte_count(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// Units of work `bytes` bytes of image cost.
fn byte_units(bytes: u64) -> u64 {
    bytes.div_ceil(BYTES_PER_UNIT)
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

    type Made = Result<OcrRequest, OcrRefusal>;

    fn made(
        gap: &Gap,
        assets: &[Asset],
        source: &Source,
        ledger: &Ledger<'_>,
    ) -> Result<Option<Made>, ConvertError> {
        let gaps = [gap.clone()];
        let inputs = Inputs { assets, source };
        Ok(requests(&gaps, inputs, &RasterSpec::default(), ledger, 7)?.into_iter().next())
    }

    fn requested(
        gap: &Gap,
        assets: &[Asset],
        ledger: &Ledger<'_>,
    ) -> Result<Option<Made>, ConvertError> {
        made(gap, assets, &Source::default(), ledger)
    }

    #[test]
    fn an_image_request_copies_its_asset_at_one_unit_per_sixteen_bytes() {
        let png = [b"\x89PNG\r\n\x1a\n".as_slice(), &[0; 25]].concat();
        let ledger = Ledger::new(u64::MAX, None);
        let made = requested(&gap(OcrOrigin::Image(AssetId(0))), &[asset(&png)], &ledger);
        let made = made.unwrap().unwrap().unwrap();
        assert_eq!((&*made.image.bytes, made.image.media_type), (png.as_slice(), "image/png"));
        assert_eq!((made.max_text_bytes, ledger.spent()), (7, 3));
        assert_eq!(
            format!("{:?}", made.image),
            r#"OcrImage { media_type: "image/png", bytes: 33, dpi: None, width: None, height: None, fidelity: None }"#
        );
        let short = Ledger::new(2, None);
        let refused = requested(&gap(OcrOrigin::Image(AssetId(0))), &[asset(&png)], &short);
        assert_eq!(refused, Err(ConvertError::limit(Limit::Work, 2)));
    }

    #[test]
    fn no_image_no_request() {
        let cases = [
            (gap(OcrOrigin::Image(AssetId(0))), b"GIF89a".as_slice()),
            (gap(OcrOrigin::Image(AssetId(1))), b"\xff\xd8\xff".as_slice()),
        ];
        for (gap, bytes) in cases {
            let ledger = Ledger::new(u64::MAX, None);
            assert_eq!(requested(&gap, &[asset(bytes)], &ledger), Ok(None), "{gap:?}");
            assert_eq!(ledger.spent(), 0);
        }
    }

    #[test]
    fn each_refusal_kind_has_its_own_snake_case_name_and_no_other_text() {
        for (kind, name, debug) in [
            (RefusalKind::Unpaintable, "unpaintable", "Unpaintable"),
            (RefusalKind::OverPixels { pixels: 7 }, "over_pixels", "OverPixels { pixels: 7 }"),
            (RefusalKind::OverBytes { bytes: 9 }, "over_bytes", "OverBytes { bytes: 9 }"),
        ] {
            assert_eq!((kind.as_str(), format!("{kind:?}")), (name, debug.to_owned()));
        }
    }

    #[cfg(feature = "raster")]
    mod pages {
        use super::*;
        use crate::formats::pdf::PageBox;

        const LETTER: Option<PageBox> = Some(PageBox { width: 612.0, height: 792.0 });
        const LETTER_PIXELS: u64 = 1275 * 1650;

        fn painted(page: u32, boxes: Vec<Option<PageBox>>) -> (Made, u64) {
            let gap = gap(OcrOrigin::Page(NonZeroU32::new(page).unwrap()));
            let ledger = Ledger::new(u64::MAX, None);
            let source = raster::tests::five_page(boxes);
            let made = made(&gap, &[], &source, &ledger).unwrap().unwrap();
            (made, ledger.spent())
        }

        #[test]
        fn a_page_that_cannot_be_painted_is_an_unpaintable_refusal_whatever_stops_it() {
            let letters = || vec![LETTER; 6];
            let size = |width, height| Some(PageBox { width, height });
            for (page, boxes, spent, why) in [
                (6, letters(), LETTER_PIXELS.div_ceil(128), "unpdf has no such page"),
                (2, vec![LETTER, None], 0, "the page was never read"),
                (1, vec![size(612.0, 0.0)], 0, "no height"),
                (1, vec![size(0.0, 792.0)], 0, "no width"),
                (1, vec![size(-612.0, 792.0)], 0, "a negative width"),
                (1, vec![size(612.0, -792.0)], 0, "a negative height"),
                (1, vec![size(f32::NAN, 792.0)], 0, "not a number"),
            ] {
                let (made, charged) = painted(page, boxes);
                let refusal = made.unwrap_err();
                assert_eq!(refusal.kind, RefusalKind::Unpaintable, "{why}");
                assert_eq!(refusal.origin, OcrOrigin::Page(NonZeroU32::new(page).unwrap()));
                assert_eq!(charged, spent, "{why}");
            }
        }
    }
}
