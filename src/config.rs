//! What a [`Converter`](crate::Converter) is built from, and why it may refuse it.

use std::collections::BTreeSet;
use std::num::NonZeroU32;

use crate::diagnostics::OnLoss;
use crate::limits::{Limit, Limits};

/// The configuration of one [`Converter`](crate::Converter), checked by
/// [`Converter::new`](crate::Converter::new).
///
/// # Examples
///
/// Set fields on the default.
///
/// ```
/// use almiladoc::{Config, ConvertError, Converter, Format, ImagePolicy, Input, Limit};
///
/// let mut config = Config::default();
/// config.threads = 4;
/// config.images = ImagePolicy::AltText;
/// assert_eq!(config.images.as_str(), "alt_text");
/// config.limits.max_input_bytes = 4;
/// let converter = Converter::new(config)?;
///
/// let csv = b"name,year\nAda,1843\n";
/// let refused = converter.convert(Input::new(csv, Format::Csv));
/// assert!(matches!(
///     refused,
///     Err(ConvertError::ResourceLimit { limit: Limit::InputBytes, .. })
/// ));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// A struct literal is refused outside the crate:
///
/// ```compile_fail,E0639
/// use almiladoc::Config;
///
/// let config = Config { threads: 4, ..Config::default() };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    /// The safety limits every call enforces.
    pub limits: Limits,
    /// Threads each call's pool holds, from 1 to [`Config::MAX_THREADS`]; no
    /// reader works on more than one yet.
    pub threads: usize,
    /// What a conversion does once some content was lost.
    pub on_loss: OnLoss,
    /// How images appear in Markdown.
    pub images: ImagePolicy,
    /// Which images a parse leaves gaps after, for the caller's OCR.
    pub image_ocr: ImageOcr,
}

impl Config {
    /// The most threads one call may run on.
    pub const MAX_THREADS: usize = 64;

    /// Check every field against its range, as
    /// [`Converter::new`](crate::Converter::new) does.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the first field out of range: a thread count
    /// outside 1 to [`Config::MAX_THREADS`], a zero limit, or a limit above
    /// its ceiling.
    ///
    /// # Examples
    ///
    /// ```
    /// use almiladoc::{Config, ConfigError, Limit};
    ///
    /// let mut config = Config::default();
    /// assert_eq!(config.validate(), Ok(()));
    ///
    /// config.limits.max_work = 0;
    /// let refused = config.validate();
    /// assert!(matches!(refused, Err(ConfigError::ZeroLimit { limit: Limit::Work, .. })));
    /// ```
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=Self::MAX_THREADS).contains(&self.threads) {
            return Err(ConfigError::Threads { max: Self::MAX_THREADS });
        }
        for (limit, value) in self.limits.fields() {
            if value == 0 {
                return Err(ConfigError::ZeroLimit { limit });
            }
            let ceiling = Limits::ceiling(limit);
            if value > ceiling {
                return Err(ConfigError::AboveCeiling { limit, ceiling });
            }
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            threads: 1,
            on_loss: OnLoss::Fail,
            images: ImagePolicy::Placeholder,
            image_ocr: ImageOcr::None,
        }
    }
}

/// How an image appears without fetching or interpreting its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ImagePolicy {
    /// Retain its position as an asset reference, source link or visible marker.
    #[default]
    Placeholder,
    /// Render its alternative text, or `Image` when no text is supplied.
    AltText,
    /// Render nothing for the image.
    Omit,
}

impl ImagePolicy {
    /// The policy's stable `snake_case` name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Placeholder => "placeholder",
            Self::AltText => "alt_text",
            Self::Omit => "omit",
        }
    }
}

/// Which images a parse leaves a gap after, for the caller's OCR to fill with the text the
/// image shows; the image itself still appears as [`ImagePolicy`] says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ImageOcr {
    /// No image.
    #[default]
    None,
    /// Every embedded PNG or JPEG, judged by its bytes' signature; each other image counts
    /// `image_not_ocred`.
    All,
}

impl ImageOcr {
    /// The setting's stable `snake_case` name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::All => "all",
        }
    }
}

/// The PDF pages a parse makes gaps whatever their text layer, for the caller to OCR; any
/// other format ignores it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum PageSelection {
    /// No page.
    #[default]
    None,
    /// Every page.
    All,
    /// These one-based pages.
    Pages(BTreeSet<NonZeroU32>),
}

/// Why a [`Config`] or a call's option was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// A limit is zero, which no input could meet.
    #[error("{limit} must not be zero")]
    #[non_exhaustive]
    ZeroLimit {
        /// The limit set to zero.
        limit: Limit,
    },
    /// A limit is above the highest value it accepts.
    #[error("{limit} must not exceed {ceiling}")]
    #[non_exhaustive]
    AboveCeiling {
        /// The limit set too high.
        limit: Limit,
        /// The highest value it accepts.
        ceiling: u64,
    },
    /// `threads` is zero or above its maximum.
    #[error("threads must be between 1 and {max}")]
    #[non_exhaustive]
    Threads {
        /// [`Config::MAX_THREADS`].
        max: usize,
    },
    /// [`PageSelection::Pages`] names a page past the document's last.
    #[error("ocr_pages names page {page}, past the {page_count} pages the document holds")]
    #[non_exhaustive]
    PageOutOfRange {
        /// The highest page selected.
        page: u32,
        /// Pages in the document.
        page_count: u32,
    },
    /// A [`RasterSpec`](crate::RasterSpec)'s dpi is outside 50 to 600, or its `max_pixels`
    /// outside 1 Mpx to [`RasterSpec::MAX_PIXELS_CEILING`](crate::RasterSpec::MAX_PIXELS_CEILING).
    #[error("a raster spec takes 50 to 600 dpi and 1048576 to 41943040 pixels")]
    Raster,
    /// An `OcrHttpConfig` field that `OcrHttp::new` refuses.
    #[error("ocr-http refuses its {field}")]
    #[non_exhaustive]
    OcrHttp {
        /// The field's name, such as `base_url`.
        field: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_threads(threads: usize) -> Config {
        Config { threads, ..Config::default() }
    }

    #[test]
    fn the_default_is_valid_and_single_threaded() {
        let config = Config::default();
        assert_eq!(config.threads, 1);
        assert_eq!(config.limits, Limits::default());
        assert_eq!(config.on_loss, OnLoss::Fail);
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn threads_run_from_one_to_the_maximum() {
        for threads in [1, 3, Config::MAX_THREADS] {
            assert_eq!(with_threads(threads).validate(), Ok(()), "{threads}");
        }
        for threads in [0, Config::MAX_THREADS + 1] {
            assert_eq!(
                with_threads(threads).validate(),
                Err(ConfigError::Threads { max: 64 }),
                "{threads}"
            );
        }
    }

    #[test]
    fn every_zero_limit_is_named() {
        for (limit, _) in Limits::default().fields() {
            let field = field_of(limit);
            let mut config = Config::default();
            *field(&mut config.limits) = 0;
            assert_eq!(config.validate(), Err(ConfigError::ZeroLimit { limit }));
            *field(&mut config.limits) = 1;
            assert_eq!(config.validate(), Ok(()), "{limit} at 1");
        }
    }

    #[test]
    fn a_limit_stops_at_its_ceiling() {
        let ceilings = [(Limit::NestingDepth, 1024), (Limit::Pages, u64::from(u32::MAX))];
        for (limit, ceiling) in ceilings {
            let field = field_of(limit);
            let mut config = Config::default();
            *field(&mut config.limits) = ceiling;
            assert_eq!(config.validate(), Ok(()), "{limit} at {ceiling}");
            *field(&mut config.limits) = ceiling + 1;
            assert_eq!(config.validate(), Err(ConfigError::AboveCeiling { limit, ceiling }));
        }
        let mut config = Config::default();
        config.limits.max_work = u64::MAX;
        config.limits.max_output_bytes = u64::MAX;
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn messages_name_the_field() {
        let cases = [
            (ConfigError::ZeroLimit { limit: Limit::Work }, "max_work must not be zero"),
            (
                ConfigError::AboveCeiling { limit: Limit::NestingDepth, ceiling: 1024 },
                "max_nesting_depth must not exceed 1024",
            ),
            (ConfigError::Threads { max: 64 }, "threads must be between 1 and 64"),
            (
                ConfigError::PageOutOfRange { page: 6, page_count: 5 },
                "ocr_pages names page 6, past the 5 pages the document holds",
            ),
            (
                ConfigError::Raster,
                "a raster spec takes 50 to 600 dpi and 1048576 to 41943040 pixels",
            ),
            (ConfigError::OcrHttp { field: "base_url" }, "ocr-http refuses its base_url"),
        ];
        for (error, message) in cases {
            assert_eq!(error.to_string(), message);
        }
    }

    type LimitField = fn(&mut Limits) -> &mut u64;

    const FIELDS: [(Limit, LimitField); 17] = [
        (Limit::EntryBytes, |limits| &mut limits.max_entry_bytes),
        (Limit::TotalBytes, |limits| &mut limits.max_total_bytes),
        (Limit::EntryCount, |limits| &mut limits.max_entry_count),
        (Limit::NestingDepth, |limits| &mut limits.max_nesting_depth),
        (Limit::XmlNodes, |limits| &mut limits.max_xml_nodes),
        (Limit::GridSlots, |limits| &mut limits.max_grid_slots),
        (Limit::Expansion, |limits| &mut limits.max_expansion),
        (Limit::ExpansionTextBytes, |limits| &mut limits.max_expansion_text_bytes),
        (Limit::AssetTotalBytes, |limits| &mut limits.max_asset_total_bytes),
        (Limit::RecordDepth, |limits| &mut limits.max_record_depth),
        (Limit::Records, |limits| &mut limits.max_records),
        (Limit::InputBytes, |limits| &mut limits.max_input_bytes),
        (Limit::CompressionRatio, |limits| &mut limits.max_compression_ratio),
        (Limit::OutputBytes, |limits| &mut limits.max_output_bytes),
        (Limit::Work, |limits| &mut limits.max_work),
        (Limit::Pages, |limits| &mut limits.max_pages),
        (Limit::OcrTextBytes, |limits| &mut limits.max_ocr_text_bytes),
    ];

    fn field_of(limit: Limit) -> LimitField {
        FIELDS.iter().find(|(named, _)| *named == limit).unwrap().1
    }
}
