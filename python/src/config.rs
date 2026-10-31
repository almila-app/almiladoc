//! `Limits`, `Config` and `OnLoss`: what a `Converter` is built from, each
//! checked where it is made.

use std::hash::{Hash, Hasher};

use pyo3::prelude::*;

use crate::errors::config_error;

/// What a conversion does once some content was lost: `FAIL` raises
/// `IncompleteError`, `REPORT` returns the `Conversion`, whose
/// `is_complete` is false.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct OnLoss(almiladoc::OnLoss);

#[pymethods]
impl OnLoss {
    #[classattr]
    const FAIL: Self = Self(almiladoc::OnLoss::Fail);

    #[classattr]
    const REPORT: Self = Self(almiladoc::OnLoss::Report);

    fn __repr__(&self) -> &'static str {
        match self.0 {
            almiladoc::OnLoss::Fail => "OnLoss.FAIL",
            almiladoc::OnLoss::Report => "OnLoss.REPORT",
            _ => "OnLoss.<unknown>",
        }
    }
}

/// How images appear in Markdown without fetching their content.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct ImagePolicy(almiladoc::ImagePolicy);

#[pymethods]
impl ImagePolicy {
    #[classattr]
    const PLACEHOLDER: Self = Self(almiladoc::ImagePolicy::Placeholder);

    #[classattr]
    const ALT_TEXT: Self = Self(almiladoc::ImagePolicy::AltText);

    #[classattr]
    const OMIT: Self = Self(almiladoc::ImagePolicy::Omit);

    fn __repr__(&self) -> &'static str {
        match self.0 {
            almiladoc::ImagePolicy::Placeholder => "ImagePolicy.PLACEHOLDER",
            almiladoc::ImagePolicy::AltText => "ImagePolicy.ALT_TEXT",
            almiladoc::ImagePolicy::Omit => "ImagePolicy.OMIT",
            _ => "ImagePolicy.<unknown>",
        }
    }
}

/// Which images a parse leaves a gap after for the caller's OCR: `NONE`, or
/// `ALL`, every embedded PNG or JPEG judged by its bytes, each other image
/// counting `image_not_ocred`. The image itself still appears as
/// `ImagePolicy` says.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct ImageOcr(almiladoc::ImageOcr);

#[pymethods]
impl ImageOcr {
    #[classattr]
    const NONE: Self = Self(almiladoc::ImageOcr::None);

    #[classattr]
    const ALL: Self = Self(almiladoc::ImageOcr::All);

    fn __repr__(&self) -> &'static str {
        match self.0 {
            almiladoc::ImageOcr::None => "ImageOcr.NONE",
            almiladoc::ImageOcr::All => "ImageOcr.ALL",
            _ => "ImageOcr.<unknown>",
        }
    }
}

/// The safety limits of one `Converter`, frozen; each crossing raises
/// `ResourceLimitError` naming it, bar one `Parsed.fill_gap` crosses other
/// than `max_work`, which raises `GapRefused`. Every field is keyword-only, defaults to
/// almiladoc's own default, and must be at least 1; `max_nesting_depth` at
/// most 1024 and `max_pages` at most 4294967295. A field out of range raises
/// `ConfigError`.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq)]
pub(crate) struct Limits(pub(crate) almiladoc::Limits);

macro_rules! limits {
    ($($(#[doc = $doc:literal])+ $field:ident,)+) => {
        #[pymethods]
        impl Limits {
            #[new]
            #[pyo3(signature = (*, $($field = almiladoc::Limits::default().$field),+))]
            #[expect(
                clippy::too_many_arguments,
                reason = "each field of Rust's `Limits` is one keyword-only argument"
            )]
            fn new(py: Python<'_>, $($field: u64),+) -> PyResult<Self> {
                let mut config = almiladoc::Config::default();
                $(config.limits.$field = $field;)+
                config.validate().map_err(|e| config_error(py, &e))?;
                Ok(Self(config.limits))
            }

            $(
                $(#[doc = $doc])+
                #[getter]
                fn $field(&self) -> u64 {
                    self.0.$field
                }
            )+

            fn __repr__(&self) -> String {
                let fields = [$(format!("{}={}", stringify!($field), self.0.$field)),+];
                format!("Limits({})", fields.join(", "))
            }
        }

        fn hash_limits<H: Hasher>(limits: &almiladoc::Limits, state: &mut H) {
            $(limits.$field.hash(state);)+
        }
    };
}

limits! {
    /// Decompressed bytes of one archive entry or compound-file stream.
    max_entry_bytes,
    /// Decompressed bytes read from one archive in total.
    max_total_bytes,
    /// Entries in one archive.
    max_entry_count,
    /// Nesting depth of input structure and of the parsed document, at most
    /// 1024.
    max_nesting_depth,
    /// XML nodes one part or opted-in OCR table fragment holds at once.
    max_xml_nodes,
    /// Grid positions the tables of one parse materialize together.
    max_grid_slots,
    /// Content-bearing cells a span or repeat expansion produces per table.
    max_expansion,
    /// Text bytes repeat expansion duplicates per document.
    max_expansion_text_bytes,
    /// Bytes of embedded assets one document retains.
    max_asset_total_bytes,
    /// Nesting depth of binary record containers.
    max_record_depth,
    /// Binary records visited in one record stream.
    max_records,
    /// Bytes of one input, checked before it is copied or read.
    max_input_bytes,
    /// Ratio of an archive entry's decompressed to compressed size.
    max_compression_ratio,
    /// Markdown or JSON bytes one call writes.
    max_output_bytes,
    /// Work units one call charges, bounding its time deterministically.
    max_work,
    /// Slides, sheets, spine items and PDF pages one document holds, each
    /// numbered by its place in the document's list, at most 4294967295; a
    /// PDF declaring more is refused before any page is read.
    max_pages,
    /// Bytes of one gap's fill, past which `Parsed.fill_gap` raises
    /// `GapRefused`; located lines also count 32 bytes per box.
    max_ocr_text_bytes,
}

impl Hash for Limits {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_limits(&self.0, state);
    }
}

/// The configuration of one `Converter`, frozen and keyword-only: its
/// `Limits`, the threads each call runs on (1 to 64; no reader uses more
/// than one), what a loss does, how images appear and which leave gaps for
/// OCR. Omitted fields, and `None`, take almiladoc's defaults: `Limits()`,
/// 1 thread, `OnLoss.FAIL`, `ImagePolicy.PLACEHOLDER` and `ImageOcr.NONE`.
/// A thread count out of range raises `ConfigError`.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq)]
pub(crate) struct Config(pub(crate) almiladoc::Config);

#[pymethods]
impl Config {
    #[new]
    #[pyo3(signature = (*, limits = None, threads = almiladoc::Config::default().threads, on_loss = None, images = None, image_ocr = None))]
    fn new(
        py: Python<'_>,
        limits: Option<&Bound<'_, Limits>>,
        threads: usize,
        on_loss: Option<&Bound<'_, OnLoss>>,
        images: Option<&Bound<'_, ImagePolicy>>,
        image_ocr: Option<&Bound<'_, ImageOcr>>,
    ) -> PyResult<Self> {
        let mut config = almiladoc::Config::default();
        if let Some(limits) = limits {
            config.limits.clone_from(&limits.get().0);
        }
        config.threads = threads;
        if let Some(on_loss) = on_loss {
            config.on_loss = on_loss.get().0;
        }
        if let Some(images) = images {
            config.images = images.get().0;
        }
        if let Some(image_ocr) = image_ocr {
            config.image_ocr = image_ocr.get().0;
        }
        config.validate().map_err(|e| config_error(py, &e))?;
        Ok(Self(config))
    }

    /// The safety limits every call enforces.
    #[getter]
    fn limits(&self) -> Limits {
        Limits(self.0.limits.clone())
    }

    /// Threads each call runs on.
    #[getter]
    fn threads(&self) -> usize {
        self.0.threads
    }

    /// What a conversion does once some content was lost.
    #[getter]
    fn on_loss(&self) -> OnLoss {
        OnLoss(self.0.on_loss)
    }

    /// How images appear in Markdown.
    #[getter]
    fn images(&self) -> ImagePolicy {
        ImagePolicy(self.0.images)
    }

    /// Which images a parse leaves a gap after for OCR.
    #[getter]
    fn image_ocr(&self) -> ImageOcr {
        ImageOcr(self.0.image_ocr)
    }

    fn __repr__(&self) -> String {
        format!(
            "Config(limits={}, threads={}, on_loss={}, images={}, image_ocr={})",
            self.limits().__repr__(),
            self.0.threads,
            self.on_loss().__repr__(),
            self.images().__repr__(),
            self.image_ocr().__repr__()
        )
    }
}

impl Hash for Config {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_limits(&self.0.limits, state);
        self.0.threads.hash(state);
        self.0.on_loss.hash(state);
        self.0.images.hash(state);
        self.0.image_ocr.hash(state);
    }
}
