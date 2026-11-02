use std::num::{NonZeroU32, NonZeroU64};
use std::ops::RangeInclusive;

use crate::config::ConfigError;
use crate::error::ConvertError;
use crate::formats::Source;
use crate::ledger::{Ledger, SharedLedger};
#[cfg(feature = "raster")]
use unpdf::parser::raster::RasterGaps;

/// How large an image an [`OcrEngine`](crate::OcrEngine) takes: the resolution a page is
/// painted at, unless its pixels would pass `max_pixels`, and the most bytes an encoded image
/// holds.
///
/// # Examples
///
/// ```
/// use std::num::NonZeroU64;
/// use almiladoc::{ConfigError, RasterSpec};
///
/// let mut spec = RasterSpec::default();
/// assert_eq!((spec.dpi, spec.max_pixels, spec.max_bytes), (150, 12_845_056, None));
/// spec.max_bytes = NonZeroU64::new(3 << 20);
/// assert_eq!(spec.validate(), Ok(()));
/// spec.dpi = 601;
/// assert_eq!(spec.validate(), Err(ConfigError::Raster));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RasterSpec {
    /// Dots per inch, 50 to 600: the most a page is painted at.
    pub dpi: u32,
    /// The most pixels one page's image holds, 1 Mpx to [`RasterSpec::MAX_PIXELS_CEILING`].
    pub max_pixels: u64,
    /// The most bytes one image holds as encoded, an embedded PNG or JPEG's and a painted
    /// page's PNG alike; a larger one is refused as
    /// [`RefusalKind::OverBytes`](crate::RefusalKind::OverBytes), never painted again smaller.
    /// `None` bounds none.
    pub max_bytes: Option<NonZeroU64>,
}

impl RasterSpec {
    /// The most pixels a spec may let one page's image hold: 40 Mpx.
    pub const MAX_PIXELS_CEILING: u64 = 40 << 20;
    const DPI: RangeInclusive<u32> = 50..=600;
    const MIN_PIXELS: u64 = 1 << 20;

    /// Check the dpi and the pixel cap against their ranges; the byte bound has none.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Raster`] when either is out of range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let pixels = Self::MIN_PIXELS..=Self::MAX_PIXELS_CEILING;
        if Self::DPI.contains(&self.dpi) && pixels.contains(&self.max_pixels) {
            Ok(())
        } else {
            Err(ConfigError::Raster)
        }
    }

    /// The largest whole dpi up to this spec's at which a box of `width` by `height` points,
    /// each side's pixels rounded up, holds at most `max_pixels`, and those pixels.
    #[cfg_attr(
        not(any(feature = "raster", test)),
        expect(dead_code, reason = "only painting fits pages")
    )]
    pub(crate) fn fit(&self, width: f32, height: f32) -> Option<(u32, u64)> {
        let side = |points: f32, dpi: u32| {
            let pixels = (f64::from(points) * f64::from(dpi) / POINTS_PER_INCH).ceil();
            (pixels >= 1.0).then(|| crate::shared::lossy::round_u64(pixels))
        };
        (1..=self.dpi).rev().find_map(|dpi| {
            let pixels = side(width, dpi)?.checked_mul(side(height, dpi)?)?;
            (pixels <= self.max_pixels).then_some((dpi, pixels))
        })
    }
}

/// Points in an inch, the unit a PDF page box is measured in.
const POINTS_PER_INCH: f64 = 72.0;

impl Default for RasterSpec {
    fn default() -> Self {
        Self { dpi: 150, max_pixels: 12_845_056, max_bytes: None }
    }
}

/// Pixels of a painted page, or of one step of its painting, per unit of
/// [`Limits::max_work`](crate::Limits::max_work).
#[cfg_attr(not(feature = "raster"), expect(dead_code, reason = "only painting charges pixels"))]
const PIXELS_PER_UNIT: u64 = 128;

/// The dpi 1-based `page` of `source` is painted at under `spec`, its pixels charged to
/// `ledger`; `None` when the source keeps no pages or no dpi fits the page.
#[cfg(feature = "raster")]
pub(crate) fn plan(
    source: &Source,
    page: NonZeroU32,
    spec: &RasterSpec,
    ledger: &Ledger<'_>,
) -> Result<Option<u32>, ConvertError> {
    let Some(pdf) = source.pdf.as_ref() else { return Ok(None) };
    let Some(page_box) = pdf.page_box(page) else { return Ok(None) };
    let Some((dpi, pixels)) = spec.fit(page_box.width, page_box.height) else { return Ok(None) };
    ledger.charge(pixels.div_ceil(PIXELS_PER_UNIT))?;
    Ok(Some(dpi))
}

#[cfg(not(feature = "raster"))]
#[expect(clippy::unnecessary_wraps, reason = "the signature painting has")]
pub(crate) fn plan(
    _: &Source,
    _: NonZeroU32,
    _: &RasterSpec,
    _: &Ledger<'_>,
) -> Result<Option<u32>, ConvertError> {
    Ok(None)
}

/// What painting a page left out or replaced, by kind, in runs, images or streams.
///
/// Annotation appearances, Lab/DeviceN colours and missing stand-in glyphs are not observed
/// yet, so a page with every count zero can still differ from its source in them.
///
/// # Examples
///
/// ```
/// use almiladoc::RasterFidelity;
///
/// let mut fidelity = RasterFidelity::default();
/// (fidelity.text_runs, fidelity.shadings, fidelity.substituted_text_runs) = (2, 5, 7);
/// assert_eq!(fidelity.lost(), 2);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct RasterFidelity {
    /// Text runs not painted: no font program or stand-in face, or a code that selects no glyph.
    pub text_runs: u32,
    /// Images not painted: a codec or colour space the painter lacks, or data that did not decode.
    pub images: u32,
    /// Inline images not painted.
    pub inline_images: u32,
    /// Shadings and pattern colours not painted.
    pub shadings: u32,
    /// Soft masks not applied as set.
    pub soft_masks: u32,
    /// Content streams that could not be decoded.
    pub content_streams: u32,
    /// Text runs painted in a standard face standing in for the page's own font: readable.
    pub substituted_text_runs: u32,
}

impl RasterFidelity {
    /// What the painting dropped from the page, as `raster_incomplete` counts it: text runs,
    /// images, inline images and content streams. Shadings, soft masks and substituted runs
    /// are decorative or readable, so they are not lost.
    #[must_use]
    pub fn lost(self) -> u64 {
        [self.text_runs, self.images, self.inline_images, self.content_streams]
            .into_iter()
            .map(u64::from)
            .sum()
    }
}

/// A page painted as an RGB PNG, its pixel size, and what the painting left out.
pub(crate) struct Painted {
    pub(crate) png: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fidelity: RasterFidelity,
}

/// 1-based `page` of `source` painted at `dpi`, its crop box turned by its `/Rotate`, each step
/// charged to `ledger` per [`PIXELS_PER_UNIT`] of its work; `None` when unpdf cannot paint it.
#[cfg(feature = "raster")]
pub(crate) fn paint(
    source: &Source,
    page: NonZeroU32,
    dpi: u32,
    ledger: &SharedLedger<'_>,
) -> Result<Option<Painted>, ConvertError> {
    paint_with_budget(source, page, dpi, |units| ledger.charge(units))
}

#[cfg(feature = "raster")]
fn paint_with_budget(
    source: &Source,
    page: NonZeroU32,
    dpi: u32,
    mut charge: impl FnMut(u64) -> Result<(), ConvertError>,
) -> Result<Option<Painted>, ConvertError> {
    use std::ops::ControlFlow;
    use unpdf::parser::raster::{PageRegion, RasterOptions, RasteredPage};
    let (Some(pdf), Ok(dpi)) = (source.pdf.as_ref(), u16::try_from(dpi)) else { return Ok(None) };
    let options = RasterOptions { dpi: f32::from(dpi), region: PageRegion::Crop };
    let mut charged = Ok(());
    let painted = pdf.parser.render_page_with(page.get(), &options, &mut |pixels| {
        charged = charge(pixels.div_ceil(PIXELS_PER_UNIT));
        if charged.is_ok() { ControlFlow::Continue(()) } else { ControlFlow::Break(()) }
    });
    charged?;
    let Ok(painted) = painted else { return Ok(None) };
    let png = painted.to_png();
    let RasteredPage { width, height, rgba: _, gaps, substituted_text_runs } = painted;
    let fidelity = fidelity(gaps, substituted_text_runs);
    Ok(Some(Painted { png, width, height, fidelity }))
}

#[cfg(feature = "raster")]
fn fidelity(gaps: RasterGaps, substituted_text_runs: u32) -> RasterFidelity {
    let RasterGaps {
        text_runs,
        images,
        inline_images,
        shadings,
        soft_masks,
        undecodable_content_streams: content_streams,
    } = gaps;
    RasterFidelity {
        text_runs,
        images,
        inline_images,
        shadings,
        soft_masks,
        content_streams,
        substituted_text_runs,
    }
}

#[cfg(not(feature = "raster"))]
#[expect(clippy::unnecessary_wraps, reason = "the signature painting has")]
pub(crate) fn paint(
    _: &Source,
    _: NonZeroU32,
    _: u32,
    _: &SharedLedger<'_>,
) -> Result<Option<Painted>, ConvertError> {
    Ok(None)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[cfg(feature = "raster")]
    pub(crate) fn five_page(boxes: Vec<Option<crate::formats::pdf::PageBox>>) -> Source {
        let bytes = include_bytes!("../../tests/fixtures/pdf/handmade-five-page.pdf");
        let parser = unpdf::parser::PdfParser::from_bytes(bytes).unwrap();
        Source { pdf: Some(crate::formats::pdf::PdfSource { parser, boxes }) }
    }

    fn spec(dpi: u32, max_pixels: u64) -> RasterSpec {
        RasterSpec { dpi, max_pixels, max_bytes: None }
    }

    #[test]
    fn a_spec_holds_its_dpi_and_pixels_within_their_ranges() {
        let ceiling = RasterSpec::MAX_PIXELS_CEILING;
        for (dpi, pixels, valid) in [
            (50, 1 << 20, true),
            (600, ceiling, true),
            (49, 1 << 20, false),
            (601, 1 << 20, false),
            (150, 1_048_575, false),
            (150, ceiling + 1, false),
        ] {
            let expected = if valid { Ok(()) } else { Err(ConfigError::Raster) };
            assert_eq!(spec(dpi, pixels).validate(), expected, "{dpi} {pixels}");
        }
        assert_eq!(RasterSpec::default().validate(), Ok(()));
    }

    #[test]
    fn a_page_takes_the_largest_dpi_its_pixels_fit_under_the_cap() {
        let default = RasterSpec::default();
        assert_eq!(default.fit(612.0, 792.0), Some((150, 1275 * 1650)));
        let (dpi, pixels) = default.fit(2725.0, 2089.0).unwrap();
        assert!(dpi < 150 && pixels <= 12_845_056, "{dpi} {pixels}");
        let one_more = spec(dpi + 1, 12_845_056).fit(2725.0, 2089.0);
        assert_eq!(one_more.map(|(fitted, _)| fitted), Some(dpi));
        assert_eq!(default.fit(0.0, 792.0), None);
        assert_eq!(default.fit(f32::MAX, f32::MAX), None);
        assert_eq!(default.fit(f32::NAN, 792.0), None);
    }

    #[cfg(feature = "raster")]
    fn source() -> Source {
        five_page(Vec::new())
    }

    #[cfg(feature = "raster")]
    fn painting(max: u64, cancel: Option<&crate::CancelToken>) -> (Result<(), ConvertError>, u64) {
        let source = source();
        let ledger = Ledger::new(max, cancel);
        let shared = ledger.share();
        let painted = paint(&source, NonZeroU32::MIN, 72, &shared).map(|painted| {
            assert!(painted.is_some_and(|painted| !painted.png.is_empty()));
        });
        ledger.settle(shared);
        (painted, ledger.spent())
    }

    #[test]
    fn a_fidelity_loses_only_the_kinds_that_drop_content() {
        let none = RasterFidelity::default();
        for (fidelity, lost) in [
            (RasterFidelity { text_runs: 1, ..none }, 1),
            (RasterFidelity { images: 2, ..none }, 2),
            (RasterFidelity { inline_images: 3, ..none }, 3),
            (RasterFidelity { content_streams: 4, ..none }, 4),
            (RasterFidelity { shadings: 5, ..none }, 0),
            (RasterFidelity { soft_masks: 6, ..none }, 0),
            (RasterFidelity { substituted_text_runs: 7, ..none }, 0),
            (
                RasterFidelity { text_runs: u32::MAX, images: u32::MAX, ..none },
                2 * u64::from(u32::MAX),
            ),
        ] {
            assert_eq!(fidelity.lost(), lost, "{fidelity:?}");
        }
    }

    #[cfg(feature = "raster")]
    #[test]
    fn each_unpdf_count_lands_in_its_own_fidelity_field() {
        let gaps = RasterGaps {
            text_runs: 1,
            images: 2,
            inline_images: 3,
            shadings: 4,
            soft_masks: 5,
            undecodable_content_streams: 6,
        };
        let expected = RasterFidelity {
            text_runs: 1,
            images: 2,
            inline_images: 3,
            shadings: 4,
            soft_masks: 5,
            content_streams: 6,
            substituted_text_runs: 7,
        };
        assert_eq!(fidelity(gaps, 7), expected);
    }

    #[cfg(feature = "raster")]
    #[test]
    fn a_painted_page_holds_its_pixel_size_and_what_it_left_out() {
        let ledger = Ledger::new(u64::MAX, None);
        let shared = ledger.share();
        let painted = paint(&source(), NonZeroU32::MIN, 72, &shared).unwrap().unwrap();
        ledger.settle(shared);
        let word = |at: usize| u32::from_be_bytes(painted.png[at..at + 4].try_into().unwrap());
        assert_eq!((painted.width, painted.height), (612, 792));
        assert_eq!((word(16), word(20)), (painted.width, painted.height));
        let substituted = RasterFidelity { substituted_text_runs: 1, ..RasterFidelity::default() };
        assert_eq!(painted.fidelity, substituted);
    }

    #[cfg(feature = "raster")]
    #[test]
    fn an_unpaintable_page_remains_recoverable() {
        let absent = NonZeroU32::new(6).unwrap();
        let mut steps = 0;
        let painted = paint_with_budget(&source(), absent, 72, |_| {
            steps += 1;
            Ok(())
        });
        assert_eq!(painted.map(|page| page.is_none()), Ok(true));
        assert_eq!(steps, 0);
    }

    #[cfg(feature = "raster")]
    #[test]
    fn a_failing_charge_stops_a_page_at_the_step_it_prices() {
        let (painted, whole) = painting(u64::MAX, None);
        assert_eq!(painted, Ok(()));
        let token = crate::CancelToken::new();
        token.cancel();
        let page = (612 * 792_u64).div_ceil(PIXELS_PER_UNIT);
        assert_eq!(painting(u64::MAX, Some(&token)), (Err(ConvertError::Cancelled), page));
        let half = whole / 2;
        let (limited, spent) = painting(half, None);
        assert_eq!(limited, Err(ConvertError::limit(crate::Limit::Work, half)));
        assert!(half < spent && spent < whole, "{spent} of {whole}");
    }

    #[cfg(feature = "raster")]
    #[test]
    fn cancellation_after_painting_begins_stops_the_next_step() {
        let token = crate::CancelToken::new();
        let ledger = Ledger::new(u64::MAX, Some(&token));
        let mut steps = 0;
        let painted = paint_with_budget(&source(), NonZeroU32::MIN, 72, |units| {
            steps += 1;
            if steps == 3 {
                token.cancel();
            }
            ledger.charge(units)
        });
        assert_eq!(painted.map(|_| ()), Err(ConvertError::Cancelled));
        assert_eq!(steps, 3);
        assert!(ledger.spent() < painting(u64::MAX, None).1);
    }
}
