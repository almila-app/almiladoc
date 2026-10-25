use std::num::NonZeroU32;
use std::ops::RangeInclusive;

use crate::config::ConfigError;
use crate::error::ConvertError;
use crate::formats::Source;
use crate::ledger::Ledger;

/// How large an image of a page an [`OcrEngine`](crate::OcrEngine) takes: the resolution a
/// page is painted at, unless its pixels would pass `max_pixels`.
///
/// # Examples
///
/// ```
/// use almiladoc::{ConfigError, RasterSpec};
///
/// let mut spec = RasterSpec::default();
/// assert_eq!((spec.dpi, spec.max_pixels), (150, 12_845_056));
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
}

impl RasterSpec {
    /// The most pixels a spec may let one page's image hold: 40 Mpx.
    pub const MAX_PIXELS_CEILING: u64 = 40 << 20;
    const DPI: RangeInclusive<u32> = 50..=600;
    const MIN_PIXELS: u64 = 1 << 20;

    /// Check the dpi and the pixel cap against their ranges.
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
        Self { dpi: 150, max_pixels: 12_845_056 }
    }
}

/// Pixels of a painted page per unit of [`Limits::max_work`](crate::Limits::max_work).
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

/// A page painted as an RGB PNG, and how many of its text runs, images, inline images and
/// content streams the painting left out.
pub(crate) struct Painted {
    pub(crate) png: Vec<u8>,
    pub(crate) unpainted: u64,
}

/// 1-based `page` of `source` painted at `dpi`, its crop box turned by its `/Rotate`; `None`
/// when unpdf cannot paint it.
#[cfg(feature = "raster")]
pub(crate) fn paint(source: &Source, page: NonZeroU32, dpi: u32) -> Option<Painted> {
    use unpdf::parser::raster::{PageRegion, RasterOptions};
    let options =
        RasterOptions { dpi: f32::from(u16::try_from(dpi).ok()?), region: PageRegion::Crop };
    let painted = source.pdf.as_ref()?.parser.render_page(page.get(), &options).ok()?;
    let gaps = painted.gaps;
    let unpainted =
        [gaps.text_runs, gaps.images, gaps.inline_images, gaps.undecodable_content_streams];
    Some(Painted { png: painted.to_png(), unpainted: unpainted.into_iter().map(u64::from).sum() })
}

#[cfg(not(feature = "raster"))]
pub(crate) fn paint(_: &Source, _: NonZeroU32, _: u32) -> Option<Painted> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(dpi: u32, max_pixels: u64) -> RasterSpec {
        RasterSpec { dpi, max_pixels }
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
}
