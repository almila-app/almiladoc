//! `Converter` and what its calls take and return.

use std::path::{Path, PathBuf};

use almiladoc::model::OcrOrigin;
use almiladoc::{ConvertError, Format, Input, PageVerdict};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString, PyTuple};

use crate::config::Config;
use crate::diagnostics::diagnostic_tuple;
use crate::errors::{Failure, config_error, convert_error};
use crate::input::{Admission, buffer, read_file};

/// A flag any thread sets to cancel the calls it was passed to.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct CancelToken(almiladoc::CancelToken);

#[pymethods]
impl CancelToken {
    #[new]
    fn new() -> Self {
        Self(almiladoc::CancelToken::new())
    }

    /// Cancel every call holding this token, from any thread: each raises
    /// `CancelledError` at its next unit of work, a PDF's between its pages.
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
    /// True when no content was lost and no page awaits OCR; only
    /// `OnLoss.REPORT` returns a conversion for which it is false.
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

/// A gap as `(unit_kind, unit_ordinal, ordinal, page, reasons)`.
type GapRow = (&'static str, u32, u32, Option<u32>, Vec<&'static str>);

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

/// A parsed document, keeping the `Config` it was parsed under: its model
/// and diagnostics as JSON, the bytes of the assets it embeds, and its
/// Markdown through `render`.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct Parsed(almiladoc::Parsed);

#[pymethods]
impl Parsed {
    /// Render the document to Markdown, as `Converter.convert` does, with
    /// the parse's diagnostics; a loss under `OnLoss.FAIL` raises
    /// `IncompleteError`.
    fn render(&self, py: Python<'_>) -> PyResult<Conversion> {
        let rendered = py.detach(|| self.0.render()).map_err(|e| convert_error(py, &e))?;
        Conversion::new(py, &rendered)
    }

    /// What the parse could not carry into the document, and what rendering
    /// it drops, as a tuple of `Diagnostic`.
    #[getter]
    fn diagnostics<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        diagnostic_tuple(py, self.0.diagnostics())
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
        py.detach(|| self.0.to_json()).map_err(|e| convert_error(py, &e))
    }

    /// Every gap the document awaits OCR in, in document order, as
    /// `(unit_kind, unit_ordinal, ordinal, page, reasons)`: the unit it stands
    /// in, its place among that unit's gaps, the 1-based PDF page OCR reads to
    /// fill it, and why the page's own text does not serve.
    #[getter]
    fn gaps(&self) -> Vec<GapRow> {
        self.0
            .gaps()
            .map(|gap| {
                let page = match gap.origin {
                    OcrOrigin::Page(page) => Some(page.get()),
                    _ => None,
                };
                let unit = gap.id.unit();
                let reasons = gap.reasons.iter().map(|reason| reason.as_str()).collect();
                (unit.kind.as_str(), unit.ordinal.get(), gap.id.ordinal().get(), page, reasons)
            })
            .collect()
    }

    /// Every embedded asset as `(id, media_type, data)`, `id` being the
    /// index an image's `asset` source names.
    fn assets<'py>(&self, py: Python<'py>) -> Vec<(usize, &str, Bound<'py, PyBytes>)> {
        self.0
            .document()
            .assets
            .iter()
            .map(|asset| (asset.id.0, asset.media_type.as_str(), PyBytes::new(py, &asset.bytes)))
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
    #[pyo3(signature = (data, format = None, *, cancel = None))]
    fn convert(
        &self,
        py: Python<'_>,
        data: &Bound<'_, PyAny>,
        format: Option<&str>,
        cancel: Option<&Bound<'_, CancelToken>>,
    ) -> PyResult<Conversion> {
        let format = format.map(parse_format).transpose()?;
        let cancel = cancel.map(|token| &token.get().0);
        let data = buffer(py, data, &self.admission(cancel))?;
        let bytes = data.as_bytes();
        let converted = py
            .detach(|| self.call(bytes, format, None, cancel, almiladoc::Converter::convert))
            .map_err(|f| f.raise(py))?;
        Conversion::new(py, &converted)
    }

    /// Parse `data`, as `convert` takes it, into the document model; a PDF's
    /// pages that await OCR are its `gaps`.
    #[pyo3(signature = (data, format = None, *, cancel = None))]
    fn parse(
        &self,
        py: Python<'_>,
        data: &Bound<'_, PyAny>,
        format: Option<&str>,
        cancel: Option<&Bound<'_, CancelToken>>,
    ) -> PyResult<Parsed> {
        let format = format.map(parse_format).transpose()?;
        let cancel = cancel.map(|token| &token.get().0);
        let data = buffer(py, data, &self.admission(cancel))?;
        let bytes = data.as_bytes();
        py.detach(|| self.call(bytes, format, None, cancel, almiladoc::Converter::parse))
            .map(Parsed)
            .map_err(|f| f.raise(py))
    }

    /// Inspect a PDF, as `convert` takes it, without rendering it: its pages,
    /// each with the verdict `parse` rules for it, and that parse's
    /// diagnostics. Any other format raises `UnsupportedError` with `kind`
    /// `not_inspectable`.
    #[pyo3(signature = (data, format = None, *, cancel = None))]
    fn inspect(
        &self,
        py: Python<'_>,
        data: &Bound<'_, PyAny>,
        format: Option<&str>,
        cancel: Option<&Bound<'_, CancelToken>>,
    ) -> PyResult<Inspection> {
        let format = format.map(parse_format).transpose()?;
        let cancel = cancel.map(|token| &token.get().0);
        let data = buffer(py, data, &self.admission(cancel))?;
        let bytes = data.as_bytes();
        let inspected = py
            .detach(|| self.call(bytes, format, None, cancel, almiladoc::Converter::inspect))
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
    #[pyo3(signature = (path, format = None, *, cancel = None))]
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
    ) -> PyResult<Conversion> {
        let format = format.map(parse_format).transpose()?;
        let cancel = cancel.map(|token| &token.get().0);
        let converted = py
            .detach(|| {
                let data = read_file(&path, &self.admission(cancel))?;
                let fallback = extension_format(&path);
                self.call(&data, format, fallback, cancel, almiladoc::Converter::convert)
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
        run: impl FnOnce(&almiladoc::Converter, Input<'_>) -> Result<T, ConvertError>,
    ) -> Result<T, Failure> {
        let input =
            format.map_or_else(|| Input::auto(bytes, fallback), |format| Input::new(bytes, format));
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

fn parse_format(name: &str) -> PyResult<Format> {
    name.parse().map_err(|e: almiladoc::ParseFormatError| PyValueError::new_err(e.to_string()))
}

fn extension_format(path: &Path) -> Option<Format> {
    path.extension().and_then(|ext| ext.to_str()).and_then(Format::from_extension)
}
