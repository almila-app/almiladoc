//! `Converter` and what its calls take and return.

use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use almiladoc::model::{GapId, GapReason, OcrOrigin};
use almiladoc::{
    ConvertError, Format, GapError, GapFill, Input, PageSelection, PageVerdict, RasterSpec,
};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::sync::MutexExt;
use pyo3::types::{PyBytes, PyInt, PyString, PyTuple};

use crate::config::Config;
use crate::diagnostics::diagnostic_tuple;
use crate::errors::{Failure, config_error, convert_error, gap_error};
use crate::input::{Admission, buffer, read_file};
use crate::ocr::{OcrHttp, OcrRequest, OcrSummary};

/// A flag any thread sets to cancel the calls it was passed to.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct CancelToken(pub(crate) almiladoc::CancelToken);

#[pymethods]
impl CancelToken {
    #[new]
    fn new() -> Self {
        Self(almiladoc::CancelToken::new())
    }

    /// Cancel every call holding this token, from any thread: each raises
    /// `CancelledError` at its next unit of work, a PDF's between its pages,
    /// as do `render`, `to_json`, `fill_gap` and `ocr_requests` on a `Parsed`
    /// a parse holding it made.
    fn cancel(&self) {
        self.0.cancel();
    }

    /// True once `cancel` was called.
    #[getter]
    fn cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

/// The almiladoc version and the configuration fingerprint that together
/// fix a conversion's output.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct Identity(almiladoc::EngineIdentity);

#[pymethods]
impl Identity {
    /// The almiladoc version.
    #[getter]
    fn version(&self) -> &'static str {
        self.0.version
    }

    /// The lowercase hex SHA-256 of the `Config` fields that shape output,
    /// all but `limits` and `threads`.
    #[getter]
    fn config_fingerprint(&self) -> &str {
        &self.0.config_fingerprint
    }

    fn __repr__(&self) -> String {
        format!(
            "Identity(version='{}', config_fingerprint='{}')",
            self.0.version, self.0.config_fingerprint
        )
    }
}

/// The result of a conversion: its Markdown, source units and diagnostics.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct Conversion {
    /// The GitHub-Flavored Markdown.
    #[pyo3(get)]
    markdown: Py<PyString>,
    /// Every `Diagnostic` of the conversion.
    #[pyo3(get)]
    diagnostics: Py<PyTuple>,
    /// Source units as `(kind, ordinal, start, end)`, with Unicode string offsets.
    #[pyo3(get)]
    units: Py<PyTuple>,
    /// True when no content was lost and every gap was filled; under
    /// `OnLoss.FAIL` only an image's gap not filled makes it false.
    #[pyo3(get)]
    is_complete: bool,
}

impl Conversion {
    fn new(py: Python<'_>, conversion: &almiladoc::Conversion) -> PyResult<Self> {
        let mut characters = conversion.markdown().char_indices().peekable();
        let mut offset = 0usize;
        let units = conversion.units().iter().map(|span| {
            while characters.next_if(|(byte, _)| *byte < span.bytes.start).is_some() {
                offset += 1;
            }
            let start = offset;
            while characters.next_if(|(byte, _)| *byte < span.bytes.end).is_some() {
                offset += 1;
            }
            (span.unit.kind.as_str(), span.unit.ordinal.get(), start, offset)
        });
        let units = PyTuple::new(py, units)?.unbind();
        Ok(Self {
            units,
            markdown: PyString::new(py, conversion.markdown()).unbind(),
            diagnostics: diagnostic_tuple(py, conversion.diagnostics())?.unbind(),
            is_complete: conversion.is_complete(),
        })
    }
}

/// A gap as `(unit_kind, unit_ordinal, ordinal, page, asset, reasons)`.
pub(crate) type GapRow =
    (Option<&'static str>, Option<u32>, u32, Option<u32>, Option<usize>, Vec<&'static str>);

/// The row of the gap `id` names, which OCR reads `origin` to fill for `reasons`.
fn gap_row(id: GapId, origin: OcrOrigin, reasons: &[GapReason]) -> GapRow {
    let (page, asset) = match origin {
        OcrOrigin::Page(page) => (Some(page.get()), None),
        OcrOrigin::Image(asset) => (None, Some(asset.0)),
        _ => (None, None),
    };
    let unit = id.unit();
    let reasons = reasons.iter().map(|reason| reason.as_str()).collect();
    let (kind, unit) = (unit.map(|u| u.kind.as_str()), unit.map(|u| u.ordinal.get()));
    (kind, unit, id.ordinal().get(), page, asset, reasons)
}

/// What `Converter.inspect` reports of a PDF.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct Inspection {
    /// The pages the document holds.
    #[pyo3(get)]
    page_count: u32,
    /// Every page in order as `(page, verdict, reasons)`: its 1-based number,
    /// `text`, `no_text` or `needs_ocr`, and for `needs_ocr` why its own
    /// text does not serve.
    #[pyo3(get)]
    pages: Py<PyTuple>,
    /// Every `Diagnostic` a parse of the document reports.
    #[pyo3(get)]
    diagnostics: Py<PyTuple>,
}

impl Inspection {
    fn new(py: Python<'_>, inspection: &almiladoc::Inspection) -> PyResult<Self> {
        let pages = inspection.pages().iter().map(|page| {
            let reasons: Vec<&str> = match &page.verdict {
                PageVerdict::NeedsOcr { reasons } => reasons.iter().map(|r| r.as_str()).collect(),
                _ => Vec::new(),
            };
            (page.page.get(), page.verdict.as_str(), reasons)
        });
        Ok(Self {
            page_count: inspection.page_count(),
            pages: PyTuple::new(py, pages)?.unbind(),
            diagnostics: diagnostic_tuple(py, inspection.diagnostics())?.unbind(),
        })
    }
}

/// A gap's row as `Parsed.gaps` gives it.
type GapKey<'py> = (
    Option<String>,
    Option<Bound<'py, PyInt>>,
    Bound<'py, PyInt>,
    Bound<'py, PyAny>,
    Bound<'py, PyAny>,
    Bound<'py, PyAny>,
);

/// A gap's unit, as its kind's name and ordinal, and its ordinal.
type GapName = (Option<String>, Option<u32>, u32);

/// A parsed document, keeping the `Config` it was parsed under and what OCR
/// gave its gaps: its model and diagnostics as JSON, the bytes of the assets
/// it embeds, and its Markdown through `render`.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct Parsed {
    parsed: Mutex<almiladoc::Parsed>,
    ids: HashMap<GapName, GapId>,
}

impl Parsed {
    fn new(parsed: almiladoc::Parsed) -> Self {
        let ids = parsed
            .gaps()
            .map(|gap| {
                let unit = gap.id.unit();
                let kind = unit.map(|unit| unit.kind.as_str().to_owned());
                ((kind, unit.map(|unit| unit.ordinal.get()), gap.id.ordinal().get()), gap.id)
            })
            .collect();
        Self { parsed: Mutex::new(parsed), ids }
    }

    fn lock(&self, py: Python<'_>) -> MutexGuard<'_, almiladoc::Parsed> {
        self.parsed.lock_py_attached(py).unwrap_or_else(PoisonError::into_inner)
    }

    /// The id of the gap `key` names, an ordinal past `u32` naming none.
    fn gap_id(&self, key: GapKey<'_>) -> Result<GapId, GapError> {
        let (kind, unit, ordinal, _, _, _) = key;
        let unit = unit.map(|unit| unit.extract::<u32>()).transpose();
        let (Ok(unit), Ok(ordinal)) = (unit, ordinal.extract::<u32>()) else {
            return Err(GapError::UnknownGap);
        };
        self.ids.get(&(kind, unit, ordinal)).copied().ok_or(GapError::UnknownGap)
    }

    /// The ids of the gaps `rows` name, rows naming none passed over.
    fn gap_ids(&self, rows: Option<Vec<GapKey<'_>>>) -> Option<Vec<GapId>> {
        rows.map(|rows| rows.into_iter().filter_map(|row| self.gap_id(row).ok()).collect())
    }
}

#[pymethods]
impl Parsed {
    /// Render the document to Markdown, as `Converter.convert` does, with
    /// the parse's diagnostics and each filled gap's text in its place; a
    /// page's gap not filled under `OnLoss.FAIL` raises `NeedsOcrError`, and
    /// then a loss `IncompleteError`.
    fn render(&self, py: Python<'_>) -> PyResult<Conversion> {
        let parsed = self.lock(py);
        let parsed = &*parsed;
        let rendered = py.detach(|| parsed.render()).map_err(|e| convert_error(py, &e))?;
        Conversion::new(py, &rendered)
    }

    /// Fill `gap`, a row of `gaps`, with the Markdown OCR read for it, plain
    /// text included, with the GIL released: raw HTML in it stays text, a
    /// link its label and an image its alt. A gap no row of `gaps` names
    /// raises `KeyError`, one filled or failed already `ValueError`, Markdown
    /// past `Limits.max_ocr_text_bytes`, nested past `max_nesting_depth` or
    /// whose tables may pass `max_grid_slots` `GapRefused`, and work past
    /// `max_work` `ResourceLimitError`; each leaves the document as it was.
    fn fill_gap(&self, py: Python<'_>, gap: GapKey<'_>, markdown: String) -> PyResult<()> {
        let id = self.gap_id(gap).map_err(|e| gap_error(py, &e))?;
        let mut parsed = self.lock(py);
        let parsed: &mut almiladoc::Parsed = &mut parsed;
        py.detach(|| parsed.fill_gap(id, GapFill::Markdown(markdown)))
            .map_err(|e| gap_error(py, &e))
    }

    /// Record that OCR of `gap`, a row of `gaps`, failed: `render` writes
    /// `<!-- ocr-failed page N -->` in a page's place and nothing after an
    /// image, and the diagnostics count `ocr_failed` for it instead of
    /// `ocr_gap`. A gap no row names raises
    /// `KeyError`, and one filled or failed already `ValueError`.
    fn fail_gap(&self, py: Python<'_>, gap: GapKey<'_>) -> PyResult<()> {
        let id = self.gap_id(gap).map_err(|e| gap_error(py, &e))?;
        self.lock(py).fail_gap(id).map_err(|e| gap_error(py, &e))
    }

    /// What the parse could not carry into the document, and what rendering
    /// it drops, as a tuple of `Diagnostic`: `ocr_gap` for each gap not yet
    /// filled or failed, `ocr_failed` for each failed one, and
    /// `raster_incomplete` for what the image of a filled page's latest
    /// request left out.
    #[getter]
    fn diagnostics<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        diagnostic_tuple(py, self.lock(py).diagnostics())
    }

    /// The document and its diagnostics as one JSON object,
    /// `{schema_version, identity, document, diagnostics}`, written with the
    /// GIL released. A loss does not raise: the diagnostics say what the
    /// document lacks. Each level of the document's nesting (a table in a
    /// cell, a list in a list), which `Limits.max_nesting_depth` bounds, nests
    /// the JSON at most seven levels deeper: a document nested past about 140
    /// levels writes JSON over 1000 levels deep, which `json.loads` on
    /// Python 3.11 reads only under a raised recursion limit.
    fn to_json(&self, py: Python<'_>) -> PyResult<String> {
        let parsed = self.lock(py);
        let parsed = &*parsed;
        py.detach(|| parsed.to_json()).map_err(|e| convert_error(py, &e))
    }

    /// Every gap of the document in document order, filled and failed ones
    /// included, as `(unit_kind, unit_ordinal, ordinal, page, asset, reasons)`:
    /// the unit it stands in, both `None` in a document without units; its
    /// place among that unit's gaps, or the document's; the 1-based PDF page
    /// or the id of the image, an index of `assets`, OCR reads to fill it;
    /// and why a page's own text does not serve.
    #[getter]
    fn gaps(&self, py: Python<'_>) -> Vec<GapRow> {
        self.lock(py).gaps().map(|gap| gap_row(gap.id, gap.origin, &gap.reasons)).collect()
    }

    /// An `OcrRequest` for each gap of `gaps`, rows of `Parsed.gaps`, or every
    /// gap when `None`, that is not yet filled or failed and asks for an
    /// image, in document order, made with the GIL released: an OCR run
    /// elsewhere, in another process too, reads each and passes its text to
    /// `fill_gap(request.gap, text)`, or calls `fail_gap(request.gap)`. An
    /// embedded image is copied and a page painted as a PNG at the largest
    /// dpi up to `dpi` at which its media box fits `max_pixels`, a page that
    /// cannot be painted asking for none, and a row naming no unsettled gap is passed
    /// over. A `dpi` outside 50 to 600 or `max_pixels` outside 1048576 to
    /// 41943040 raises `ConfigError` with `limit` `raster`, an int that fits
    /// neither `OverflowError`, and work past `max_work` `ResourceLimitError`.
    #[pyo3(signature = (gaps = None, *, dpi = 150, max_pixels = 12_845_056))]
    fn ocr_requests(
        &self,
        py: Python<'_>,
        gaps: Option<Vec<GapKey<'_>>>,
        dpi: u32,
        max_pixels: u64,
    ) -> PyResult<Vec<OcrRequest>> {
        let mut spec = RasterSpec::default();
        (spec.dpi, spec.max_pixels) = (dpi, max_pixels);
        let ids = self.gap_ids(gaps);
        let mut parsed = self.lock(py);
        let parsed: &mut almiladoc::Parsed = &mut parsed;
        let requests =
            py.detach(|| parsed.ocr_requests(&spec, ids.as_deref())).map_err(|e| match e {
                ConvertError::InvalidConfig(error @ almiladoc::ConfigError::Raster) => {
                    config_error(py, &error)
                }
                e => convert_error(py, &e),
            })?;
        requests
            .iter()
            .map(|request| {
                let row = gap_row(request.gap, request.origin, &request.reasons);
                OcrRequest::of(py, row, request)
            })
            .collect()
    }

    /// Read the image of each gap of `gaps`, rows of `Parsed.gaps`, or of every
    /// gap when `None`, that `ocr_requests` would ask for, through `engine`, with
    /// the GIL released: in batches that fill or fail each gap as `fill_gap` and
    /// `fail_gap` do, a fill a limit refuses failing its gap, until a batch
    /// holds a gap the engine did not attempt, which stays for a later call.
    /// Work past `max_work` raises `ResourceLimitError`, and a cancel of the
    /// parse's token `CancelledError`, each keeping what earlier batches
    /// settled.
    #[pyo3(signature = (engine, gaps = None))]
    fn ocr(
        &self,
        py: Python<'_>,
        engine: &Bound<'_, OcrHttp>,
        gaps: Option<Vec<GapKey<'_>>>,
    ) -> PyResult<OcrSummary> {
        let (engine, ids) = (&engine.get().0, self.gap_ids(gaps));
        let mut parsed = self.lock(py);
        let parsed: &mut almiladoc::Parsed = &mut parsed;
        py.detach(|| parsed.ocr(engine, ids.as_deref()))
            .map(OcrSummary::from)
            .map_err(|e| convert_error(py, &e))
    }

    /// Every embedded asset as `(id, media_type, data)`, `id` being the
    /// index an image's `asset` source names.
    fn assets<'py>(&self, py: Python<'py>) -> Vec<(usize, String, Bound<'py, PyBytes>)> {
        self.lock(py)
            .document()
            .assets
            .iter()
            .map(|asset| (asset.id.0, asset.media_type.clone(), PyBytes::new(py, &asset.bytes)))
            .collect()
    }
}

/// Converts documents under one `Config`, `Config()` when none is given;
/// share it across threads and calls. Each call releases the GIL while it
/// works, so a cancellation comes only through its `CancelToken`, never
/// from Ctrl-C.
///
/// Without a `format`, a call detects it from the content once the input
/// is admitted; a `format` the content contradicts raises
/// `UnsupportedError` with `kind` `format_mismatch` and the `detected`
/// format to retry with.
///
/// `ocr_pages`, `"all"` or a set of 1-based pages, makes each selected PDF
/// page a gap with reason `caller_requested` whatever its text, discarded
/// without loss; a page past the last raises `InvalidConfigError`, any other
/// page outside 1 to 4294967295 `ValueError`, and any other format ignores it.
///
/// `ocr`, an `OcrHttp`, reads a conversion's gaps before it renders, as
/// `parse`, `Parsed.ocr` and `Parsed.render` do in turn.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct Converter {
    inner: almiladoc::Converter,
    max_input_bytes: u64,
}

#[pymethods]
impl Converter {
    #[new]
    #[pyo3(signature = (config = None))]
    fn new(py: Python<'_>, config: Option<&Bound<'_, Config>>) -> PyResult<Self> {
        let config =
            config.map_or_else(almiladoc::Config::default, |config| config.get().0.clone());
        let max_input_bytes = config.limits.max_input_bytes;
        let inner = almiladoc::Converter::new(config).map_err(|e| config_error(py, &e))?;
        Ok(Self { inner, max_input_bytes })
    }

    /// The `Identity` that fixes this converter's output.
    #[getter]
    fn identity(&self) -> Identity {
        Identity(self.inner.identity().clone())
    }

    /// Convert `data`, `bytes` or any contiguous byte buffer, to Markdown.
    /// `bytes` is read in place; any other buffer is copied once, after its
    /// length is checked against `max_input_bytes`. CSV carries no
    /// signature, so it has to be named.
    #[pyo3(signature = (data, format = None, *, cancel = None, ocr_pages = None, ocr = None))]
    fn convert(
        &self,
        py: Python<'_>,
        data: &Bound<'_, PyAny>,
        format: Option<&str>,
        cancel: Option<&Bound<'_, CancelToken>>,
        ocr_pages: Option<&Bound<'_, PyAny>>,
        ocr: Option<&Bound<'_, OcrHttp>>,
    ) -> PyResult<Conversion> {
        let format = format.map(parse_format).transpose()?;
        let ocr_pages = page_selection(ocr_pages)?;
        let (cancel, engine) = (cancel.map(|token| &token.get().0), ocr.map(|ocr| &ocr.get().0));
        let data = buffer(py, data, &self.admission(cancel))?;
        let bytes = data.as_bytes();
        let converted = py
            .detach(|| {
                self.call(bytes, format, None, cancel, ocr_pages, |converter, input| {
                    convert(converter, input, engine)
                })
            })
            .map_err(|f| f.raise(py))?;
        Conversion::new(py, &converted)
    }

    /// Parse `data`, as `convert` takes it, into the document model; a PDF's
    /// pages that await OCR and, under `ImageOcr.ALL`, each embedded PNG or
    /// JPEG are its `gaps`.
    #[pyo3(signature = (data, format = None, *, cancel = None, ocr_pages = None))]
    fn parse(
        &self,
        py: Python<'_>,
        data: &Bound<'_, PyAny>,
        format: Option<&str>,
        cancel: Option<&Bound<'_, CancelToken>>,
        ocr_pages: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Parsed> {
        let format = format.map(parse_format).transpose()?;
        let ocr_pages = page_selection(ocr_pages)?;
        let cancel = cancel.map(|token| &token.get().0);
        let data = buffer(py, data, &self.admission(cancel))?;
        let bytes = data.as_bytes();
        py.detach(|| self.call(bytes, format, None, cancel, ocr_pages, almiladoc::Converter::parse))
            .map(Parsed::new)
            .map_err(|f| f.raise(py))
    }

    /// Inspect a PDF, as `convert` takes it, without rendering it: its pages,
    /// each with the verdict `parse` rules for it, and that parse's
    /// diagnostics. Any other format raises `UnsupportedError` with `kind`
    /// `not_inspectable`.
    #[pyo3(signature = (data, format = None, *, cancel = None, ocr_pages = None))]
    fn inspect(
        &self,
        py: Python<'_>,
        data: &Bound<'_, PyAny>,
        format: Option<&str>,
        cancel: Option<&Bound<'_, CancelToken>>,
        ocr_pages: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Inspection> {
        let format = format.map(parse_format).transpose()?;
        let ocr_pages = page_selection(ocr_pages)?;
        let cancel = cancel.map(|token| &token.get().0);
        let data = buffer(py, data, &self.admission(cancel))?;
        let bytes = data.as_bytes();
        let inspected = py
            .detach(|| {
                self.call(bytes, format, None, cancel, ocr_pages, almiladoc::Converter::inspect)
            })
            .map_err(|f| f.raise(py))?;
        Inspection::new(py, &inspected)
    }

    /// The format `data`'s content names, detected as a call without a
    /// `format` detects it: under this converter's limits and `cancel`, once
    /// `data` is admitted as `convert` admits it. `None` for CSV, which
    /// carries no signature, and for anything unrecognized.
    #[pyo3(signature = (data, *, cancel = None))]
    fn sniff(
        &self,
        py: Python<'_>,
        data: &Bound<'_, PyAny>,
        cancel: Option<&Bound<'_, CancelToken>>,
    ) -> PyResult<Option<&'static str>> {
        let cancel = cancel.map(|token| &token.get().0);
        let data = buffer(py, data, &self.admission(cancel))?;
        let bytes = data.as_bytes();
        py.detach(|| self.inner.sniff(bytes, cancel))
            .map(|format| format.map(Format::as_str))
            .map_err(|e| convert_error(py, &e))
    }

    /// Convert the regular file at `path` to Markdown. Its size is checked
    /// against `max_input_bytes` before it is read, and a directory raises
    /// `OSError` unopened, as does a FIFO or device on Unix. Without a
    /// `format`, the content names it, else the path's extension does.
    #[pyo3(signature = (path, format = None, *, cancel = None, ocr_pages = None, ocr = None))]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "pyo3 extracts arguments by value, and a borrowed `&Path` would refuse the os.PathLike the stub accepts"
    )]
    fn convert_path(
        &self,
        py: Python<'_>,
        path: PathBuf,
        format: Option<&str>,
        cancel: Option<&Bound<'_, CancelToken>>,
        ocr_pages: Option<&Bound<'_, PyAny>>,
        ocr: Option<&Bound<'_, OcrHttp>>,
    ) -> PyResult<Conversion> {
        let format = format.map(parse_format).transpose()?;
        let ocr_pages = page_selection(ocr_pages)?;
        let (cancel, engine) = (cancel.map(|token| &token.get().0), ocr.map(|ocr| &ocr.get().0));
        let converted = py
            .detach(|| {
                let data = read_file(&path, &self.admission(cancel))?;
                let fallback = extension_format(&path);
                self.call(&data, format, fallback, cancel, ocr_pages, |converter, input| {
                    convert(converter, input, engine)
                })
            })
            .map_err(|f| f.raise(py))?;
        Conversion::new(py, &converted)
    }
}

impl Converter {
    fn admission<'t>(&self, cancel: Option<&'t almiladoc::CancelToken>) -> Admission<'t> {
        Admission { max_input_bytes: self.max_input_bytes, cancel }
    }

    /// `run` on admitted `bytes` as `format`, else as the format their
    /// content names, else as `fallback`.
    fn call<T>(
        &self,
        bytes: &[u8],
        format: Option<Format>,
        fallback: Option<Format>,
        cancel: Option<&almiladoc::CancelToken>,
        ocr_pages: PageSelection,
        run: impl FnOnce(&almiladoc::Converter, Input<'_>) -> Result<T, ConvertError>,
    ) -> Result<T, Failure> {
        let input = format
            .map_or_else(|| Input::auto(bytes, fallback), |format| Input::new(bytes, format))
            .with_ocr_pages(ocr_pages);
        let input = match cancel {
            Some(token) => input.with_cancel(token.clone()),
            None => input,
        };
        Ok(run(&self.inner, input)?)
    }
}

/// The format `data`'s content names, detected under the default limits;
/// `None` for CSV, which carries no signature, and for anything
/// unrecognized. `bytes` is read in place and any other contiguous byte
/// buffer copied once, after its length is checked against the default
/// `max_input_bytes`.
#[pyfunction]
pub(crate) fn sniff(py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Option<&'static str>> {
    let admission =
        Admission { max_input_bytes: almiladoc::Limits::default().max_input_bytes, cancel: None };
    let data = buffer(py, data, &admission)?;
    let bytes = data.as_bytes();
    admission.check(bytes.len() as u64).map_err(|f| f.raise(py))?;
    Ok(py.detach(|| Format::sniff(bytes)).map(Format::as_str))
}

/// `None`, `"all"` or an iterable of 1-based page numbers, as `PageSelection`.
fn page_selection(ocr_pages: Option<&Bound<'_, PyAny>>) -> PyResult<PageSelection> {
    let refused =
        || PyValueError::new_err("ocr_pages is None, \"all\" or a set of pages 1 to 4294967295");
    let Some(ocr_pages) = ocr_pages else { return Ok(PageSelection::None) };
    if let Ok(name) = ocr_pages.cast::<PyString>() {
        return if name.to_str()? == "all" { Ok(PageSelection::All) } else { Err(refused()) };
    }
    ocr_pages
        .try_iter()?
        .map(|page| {
            let page = page?;
            let page: u32 = page
                .extract()
                .map_err(|error| if page.is_instance_of::<PyInt>() { refused() } else { error })?;
            NonZeroU32::new(page).ok_or_else(refused)
        })
        .collect::<PyResult<BTreeSet<_>>>()
        .map(PageSelection::Pages)
}

/// `input` converted, `engine` reading its gaps' images when given.
fn convert(
    converter: &almiladoc::Converter,
    input: Input<'_>,
    engine: Option<&almiladoc::OcrHttp>,
) -> Result<almiladoc::Conversion, ConvertError> {
    match engine {
        Some(engine) => converter.convert_with_ocr(input, engine),
        None => converter.convert(input),
    }
}

fn parse_format(name: &str) -> PyResult<Format> {
    name.parse().map_err(|e: almiladoc::ParseFormatError| PyValueError::new_err(e.to_string()))
}

fn extension_format(path: &Path) -> Option<Format> {
    path.extension().and_then(|ext| ext.to_str()).and_then(Format::from_extension)
}
