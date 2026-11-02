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

/// Whether supported headers and footers appear in the output.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct HeaderFooterPolicy(almiladoc::HeaderFooterPolicy);

#[pymethods]
impl HeaderFooterPolicy {
    #[classattr]
    const OMIT: Self = Self(almiladoc::HeaderFooterPolicy::Omit);
    #[classattr]
    const EMIT: Self = Self(almiladoc::HeaderFooterPolicy::Emit);
    fn __repr__(&self) -> &'static str {
        match self.0 {
            almiladoc::HeaderFooterPolicy::Omit => "HeaderFooterPolicy.OMIT",
            almiladoc::HeaderFooterPolicy::Emit => "HeaderFooterPolicy.EMIT",
            _ => "HeaderFooterPolicy.<unknown>",
        }
    }
}

/// Whether supported comments appear as isolated GFM footnotes.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct CommentPolicy(almiladoc::CommentPolicy);

#[pymethods]
impl CommentPolicy {
    #[classattr]
    const OMIT: Self = Self(almiladoc::CommentPolicy::Omit);
    #[classattr]
    const EMIT: Self = Self(almiladoc::CommentPolicy::Emit);
    fn __repr__(&self) -> &'static str {
        match self.0 {
            almiladoc::CommentPolicy::Omit => "CommentPolicy.OMIT",
            almiladoc::CommentPolicy::Emit => "CommentPolicy.EMIT",
            _ => "CommentPolicy.<unknown>",
        }
    }
}

/// Whether supported hidden source content appears in the output.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct HiddenPolicy(almiladoc::HiddenPolicy);

#[pymethods]
impl HiddenPolicy {
    #[classattr]
    const OMIT: Self = Self(almiladoc::HiddenPolicy::Omit);
    #[classattr]
    const EMIT: Self = Self(almiladoc::HiddenPolicy::Emit);
    fn __repr__(&self) -> &'static str {
        match self.0 {
            almiladoc::HiddenPolicy::Omit => "HiddenPolicy.OMIT",
            almiladoc::HiddenPolicy::Emit => "HiddenPolicy.EMIT",
            _ => "HiddenPolicy.<unknown>",
        }
    }
}

/// Which tracked text revision view appears in the output.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct RevisionPolicy(almiladoc::RevisionPolicy);

#[pymethods]
impl RevisionPolicy {
    #[classattr]
    const ACCEPT: Self = Self(almiladoc::RevisionPolicy::Accept);
    #[classattr]
    const REJECT: Self = Self(almiladoc::RevisionPolicy::Reject);
    fn __repr__(&self) -> &'static str {
        match self.0 {
            almiladoc::RevisionPolicy::Accept => "RevisionPolicy.ACCEPT",
            almiladoc::RevisionPolicy::Reject => "RevisionPolicy.REJECT",
            _ => "RevisionPolicy.<unknown>",
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

/// Which PDF pages a parse treats as scans, for the caller's OCR: `CONSERVATIVE`, an image and
/// no readable text, or only furniture or margin text over at least half the page;
/// `COVERAGE_FIRST`, those pages and every page at least half covered by images whatever native
/// text it holds, which OCR then replaces. Other formats ignore it.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct PdfScanPolicy(almiladoc::PdfScanPolicy);

#[pymethods]
impl PdfScanPolicy {
    #[classattr]
    const CONSERVATIVE: Self = Self(almiladoc::PdfScanPolicy::Conservative);

    #[classattr]
    const COVERAGE_FIRST: Self = Self(almiladoc::PdfScanPolicy::CoverageFirst);

    fn __repr__(&self) -> &'static str {
        match self.0 {
            almiladoc::PdfScanPolicy::Conservative => "PdfScanPolicy.CONSERVATIVE",
            almiladoc::PdfScanPolicy::CoverageFirst => "PdfScanPolicy.COVERAGE_FIRST",
            _ => "PdfScanPolicy.<unknown>",
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
    /// Decompressed bytes of one archive entry, OfficeArt picture or compound-file stream.
    max_entry_bytes,
    /// Decompressed bytes one parse inflates (archive entries and OfficeArt pictures).
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
    /// Ratio of an archive entry's or OfficeArt picture's decompressed to compressed size.
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
/// than one), what a loss does, how images appear, which leave gaps for
/// OCR and which PDF pages count as scans. Omitted fields, and `None`, take
/// almiladoc's defaults: `Limits()`, 1 thread, `OnLoss.FAIL`,
/// `ImagePolicy.PLACEHOLDER`, `ImageOcr.NONE`, `PdfScanPolicy.CONSERVATIVE`,
/// `HeaderFooterPolicy.OMIT`, `CommentPolicy.OMIT`, `HiddenPolicy.OMIT` and
/// `RevisionPolicy.ACCEPT`.
/// A thread count out of range raises `ConfigError`.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq)]
pub(crate) struct Config(pub(crate) almiladoc::Config);

#[pymethods]
impl Config {
    #[new]
    #[expect(
        clippy::too_many_arguments,
        reason = "Python keywords mirror the core configuration fields"
    )]
    #[pyo3(signature = (*, limits = None, threads = almiladoc::Config::default().threads, on_loss = None, images = None, image_ocr = None, pdf_scans = None, headers_footers = None, comments = None, hidden = None, revisions = None))]
    fn new(
        py: Python<'_>,
        limits: Option<&Bound<'_, Limits>>,
        threads: usize,
        on_loss: Option<&Bound<'_, OnLoss>>,
        images: Option<&Bound<'_, ImagePolicy>>,
        image_ocr: Option<&Bound<'_, ImageOcr>>,
        pdf_scans: Option<&Bound<'_, PdfScanPolicy>>,
        headers_footers: Option<&Bound<'_, HeaderFooterPolicy>>,
        comments: Option<&Bound<'_, CommentPolicy>>,
        hidden: Option<&Bound<'_, HiddenPolicy>>,
        revisions: Option<&Bound<'_, RevisionPolicy>>,
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
        if let Some(pdf_scans) = pdf_scans {
            config.pdf_scans = pdf_scans.get().0;
        }
        if let Some(policy) = hidden {
            config.hidden = policy.get().0;
        }
        if let Some(policy) = revisions {
            config.revisions = policy.get().0;
        }
        if let Some(policy) = comments {
            config.comments = policy.get().0;
        }
        if let Some(policy) = headers_footers {
            config.headers_footers = policy.get().0;
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

    /// Which PDF pages a parse treats as scans.
    #[getter]
    fn pdf_scans(&self) -> PdfScanPolicy {
        PdfScanPolicy(self.0.pdf_scans)
    }

    /// Whether the page headers and footers a document declares appear.
    #[getter]
    fn headers_footers(&self) -> HeaderFooterPolicy {
        HeaderFooterPolicy(self.0.headers_footers)
    }

    /// Whether supported source comments appear as isolated footnotes.
    #[getter]
    fn comments(&self) -> CommentPolicy {
        CommentPolicy(self.0.comments)
    }

    /// Whether supported hidden source content appears.
    #[getter]
    fn hidden(&self) -> HiddenPolicy {
        HiddenPolicy(self.0.hidden)
    }

    /// Which tracked text revision view appears.
    #[getter]
    fn revisions(&self) -> RevisionPolicy {
        RevisionPolicy(self.0.revisions)
    }

    fn __repr__(&self) -> String {
        format!(
            "Config(limits={}, threads={}, on_loss={}, images={}, image_ocr={}, pdf_scans={}, headers_footers={}, comments={}, hidden={}, revisions={})",
            self.limits().__repr__(),
            self.0.threads,
            self.on_loss().__repr__(),
            self.images().__repr__(),
            self.image_ocr().__repr__(),
            self.pdf_scans().__repr__(),
            self.headers_footers().__repr__(),
            self.comments().__repr__(),
            self.hidden().__repr__(),
            self.revisions().__repr__()
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
        self.0.pdf_scans.hash(state);
        self.0.headers_footers.hash(state);
        self.0.comments.hash(state);
        self.0.hidden.hash(state);
        self.0.revisions.hash(state);
    }
}
