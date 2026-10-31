//! `OcrRequest`, a gap's image exported for an OCR run elsewhere, and `OcrHttp`, the
//! engine that reads it.

use almiladoc::{GapFill, OcrHttpConfig, OcrImage, OcrOutcome};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyString, PyTuple, PyType};
use serde_json::{Map, Value};

use crate::converter::{CancelToken, GapRow};
use crate::errors::{config_error, gap_error, ocr_http_refusal};

/// One literal horizontal line with finite normalized `[left, top, right, bottom]` bounds.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct OcrLine(pub(crate) almiladoc::OcrLine);

#[pymethods]
impl OcrLine {
    #[classattr]
    const BOX_BYTES: u64 = almiladoc::OcrLine::BOX_BYTES;

    #[new]
    fn new(py: Python<'_>, text: String, bounds: (f64, f64, f64, f64)) -> PyResult<Self> {
        almiladoc::OcrLine::new(text, [bounds.0, bounds.1, bounds.2, bounds.3])
            .map(Self)
            .map_err(|error| gap_error(py, &error))
    }

    /// The literal UTF-8 text.
    #[getter]
    fn text(&self) -> &str {
        self.0.text()
    }

    /// The normalized `[left, top, right, bottom]` image bounds.
    #[getter]
    fn bounds(&self) -> (f64, f64, f64, f64) {
        let [left, top, right, bottom] = self.0.bounds();
        (left, top, right, bottom)
    }

    fn __getnewargs__(&self) -> (String, (f64, f64, f64, f64)) {
        (self.text().to_owned(), self.bounds())
    }

    fn __repr__(&self) -> String {
        format!("{:?}", self.0)
    }
}

/// One gap's image for an OCR run elsewhere, which pickles: `gap`, the row of
/// `Parsed.gaps` that `Parsed.fill_gap` and `Parsed.fail_gap` take; `image`,
/// its PNG or JPEG bytes; `media_type`, `image/png` or `image/jpeg`;
/// `max_text_bytes`, the most bytes of Markdown a fill of it takes; and
/// `dpi`, the resolution a page was painted at, `None` for an embedded image.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct OcrRequest {
    gap: Py<PyTuple>,
    image: Py<PyBytes>,
    media_type: String,
    max_text_bytes: u64,
    dpi: Option<u32>,
}

impl OcrRequest {
    pub(crate) fn of(
        py: Python<'_>,
        gap: GapRow,
        request: &almiladoc::OcrRequest,
    ) -> PyResult<Self> {
        Ok(Self {
            gap: gap.into_pyobject(py)?.unbind(),
            image: PyBytes::new(py, &request.image.bytes).unbind(),
            media_type: request.image.media_type.to_owned(),
            max_text_bytes: request.max_text_bytes,
            dpi: request.image.dpi,
        })
    }

    /// This request's image and text limit, copied for an engine to read;
    /// `ValueError` unless the image opens with the signature `media_type` names.
    fn to_read(&self, py: Python<'_>) -> PyResult<(OcrImage, u64)> {
        OcrImage::new(self.image.bind(py).as_bytes(), self.dpi)
            .filter(|image| image.media_type == self.media_type)
            .map(|image| (image, self.max_text_bytes))
            .ok_or_else(|| PyValueError::new_err("an OcrRequest's image is not its media_type"))
    }
}

#[pymethods]
impl OcrRequest {
    #[new]
    #[pyo3(signature = (gap, image, media_type, max_text_bytes, dpi = None))]
    fn new(
        gap: Bound<'_, PyTuple>,
        image: Bound<'_, PyBytes>,
        media_type: String,
        max_text_bytes: u64,
        dpi: Option<u32>,
    ) -> Self {
        Self { gap: gap.unbind(), image: image.unbind(), media_type, max_text_bytes, dpi }
    }

    /// The row of `Parsed.gaps` this request reads the image of.
    #[getter]
    fn gap<'py>(&self, py: Python<'py>) -> Bound<'py, PyTuple> {
        self.gap.bind(py).clone()
    }

    /// The image's PNG or JPEG bytes.
    #[getter]
    fn image<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        self.image.bind(py).clone()
    }

    /// `image/png` or `image/jpeg`.
    #[getter]
    fn media_type(&self) -> &str {
        &self.media_type
    }

    /// The most bytes of Markdown a fill of this gap takes.
    #[getter]
    fn max_text_bytes(&self) -> u64 {
        self.max_text_bytes
    }

    /// The resolution a page was painted at; `None` for an embedded image.
    #[getter]
    fn dpi(&self) -> Option<u32> {
        self.dpi
    }

    #[expect(
        clippy::type_complexity,
        reason = "pickle's reduce tuple of a class and its arguments"
    )]
    fn __reduce__<'py>(
        slf: &Bound<'py, Self>,
    ) -> (Bound<'py, PyType>, (Bound<'py, PyTuple>, Bound<'py, PyBytes>, String, u64, Option<u32>))
    {
        let py = slf.py();
        let this = slf.get();
        let (media_type, max_text_bytes) = (this.media_type.clone(), this.max_text_bytes);
        (slf.get_type(), (this.gap(py), this.image(py), media_type, max_text_bytes, this.dpi))
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        Ok(format!(
            "OcrRequest(gap={}, media_type={:?}, image=<{} bytes>, max_text_bytes={}, dpi={})",
            self.gap.bind(py).repr()?,
            self.media_type,
            self.image.bind(py).as_bytes().len(),
            self.max_text_bytes,
            self.dpi.map_or_else(|| "None".to_owned(), |dpi| dpi.to_string()),
        ))
    }
}

/// Reads gaps' images through an OpenAI-compatible vision server, posting
/// each to `base_url`'s `chat/completions` with `model`: for `Parsed.ocr`, the
/// `ocr` of `Converter.convert`, or `recognize`. `api_key` goes only over
/// `https` or to loopback; `prompt`, `None` for the default, asks for each
/// image's Markdown, after the image when `image_first`; `extra_body` adds
/// JSON fields to each request; `max_tokens` and `temperature`, 0 to 2, set
/// the sampling; a page is painted as `Parsed.ocr_requests` paints it at
/// `dpi` and `max_pixels`; a request the server cannot take yet is tried up
/// to `retries` more times, at most 10; and `concurrency`, 1 to 16, requests
/// go out at once. A field out of range raises `ConfigError` naming it. No
/// socket opens before a request and no redirect is followed. `root_certificates_pem`
/// replaces Mozilla roots, including for an HTTPS proxy; `proxy` explicitly selects
/// an HTTP(S) CONNECT proxy and requires HTTPS when `api_key` is set.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct OcrHttp(pub(crate) almiladoc::OcrHttp);

#[pymethods]
impl OcrHttp {
    #[new]
    #[pyo3(signature = (
        base_url, model, *, api_key = None, prompt = None, image_first = true, extra_body = None,
        max_tokens = 8192, temperature = 0.0, dpi = 150, max_pixels = 12_845_056, retries = 3,
        concurrency = 1, root_certificates_pem = None, proxy = None,
    ))]
    #[expect(
        clippy::too_many_arguments,
        reason = "each field of Rust's `OcrHttpConfig` is one argument"
    )]
    fn new(
        py: Python<'_>,
        base_url: String,
        model: String,
        api_key: Option<String>,
        prompt: Option<String>,
        image_first: bool,
        extra_body: Option<&Bound<'_, PyDict>>,
        max_tokens: u32,
        temperature: f32,
        dpi: u32,
        max_pixels: u64,
        retries: u32,
        concurrency: usize,
        root_certificates_pem: Option<&Bound<'_, PyBytes>>,
        proxy: Option<&Bound<'_, PyString>>,
    ) -> PyResult<Self> {
        let mut config = OcrHttpConfig::new(base_url, model);
        config.api_key = api_key;
        config.root_certificates_pem = bounded_roots(py, root_certificates_pem)?;
        config.proxy = bounded_proxy(py, proxy)?;
        config.prompt = prompt.unwrap_or(config.prompt);
        config.image_first = image_first;
        if let Some(body) = extra_body {
            config.extra_body =
                json_object(py, body).ok_or_else(|| ocr_http_refusal(py, "extra_body"))?;
        }
        (config.max_tokens, config.temperature) = (max_tokens, temperature);
        (config.raster.dpi, config.raster.max_pixels) = (dpi, max_pixels);
        (config.retries, config.concurrency) = (retries, concurrency);
        almiladoc::OcrHttp::new(config).map(Self).map_err(|e| config_error(py, &e))
    }

    /// The caller's replacement PEM bundle, absent for Mozilla roots.
    #[getter]
    fn root_certificates_pem<'py>(&self, py: Python<'py>) -> Option<Bound<'py, PyBytes>> {
        self.0.root_certificates_pem().map(|pem| PyBytes::new(py, pem))
    }

    /// The configured proxy URI, including caller-supplied credentials.
    #[getter]
    fn proxy(&self) -> Option<&str> {
        self.0.proxy()
    }

    /// The text the server reads for each of `requests`, in order, with the
    /// GIL released once their images are copied: a `str` that fills its gap,
    /// or an `OcrFailure`. Once one is not attempted or `cancel` is
    /// cancelled, no later request is sent. A request whose image is not its
    /// `media_type` raises `ValueError`.
    #[pyo3(signature = (requests, *, cancel = None))]
    fn recognize<'py>(
        &self,
        py: Python<'py>,
        requests: Vec<Bound<'py, OcrRequest>>,
        cancel: Option<&Bound<'py, CancelToken>>,
    ) -> PyResult<Vec<Bound<'py, PyAny>>> {
        let images = requests
            .into_iter()
            .map(|request| request.get().to_read(py))
            .collect::<PyResult<Vec<_>>>()?;
        let cancel = cancel.map_or_else(almiladoc::CancelToken::new, |token| token.get().0.clone());
        let outcomes = py.detach(|| self.0.recognize_images(&images, &cancel));
        outcomes.into_iter().map(|outcome| answer(py, outcome)).collect()
    }

    fn __repr__(&self) -> String {
        format!("{:?}", self.0)
    }
}

fn bounded_roots(py: Python<'_>, value: Option<&Bound<'_, PyBytes>>) -> PyResult<Option<Vec<u8>>> {
    value
        .map(|value| {
            let bytes = value.as_bytes();
            if bytes.len() > OcrHttpConfig::MAX_ROOT_PEM_BYTES {
                return Err(ocr_http_refusal(py, "root_certificates_pem"));
            }
            Ok(bytes.to_vec())
        })
        .transpose()
}

fn bounded_proxy(py: Python<'_>, value: Option<&Bound<'_, PyString>>) -> PyResult<Option<String>> {
    value
        .map(|value| {
            if value.code_point_len().map_err(|_| ocr_http_refusal(py, "proxy"))?
                > OcrHttpConfig::MAX_PROXY_BYTES
            {
                return Err(ocr_http_refusal(py, "proxy"));
            }
            let value = value.to_str().map_err(|_| ocr_http_refusal(py, "proxy"))?;
            if value.len() > OcrHttpConfig::MAX_PROXY_BYTES {
                return Err(ocr_http_refusal(py, "proxy"));
            }
            Ok(value.to_owned())
        })
        .transpose()
}

/// `body` as a JSON object, `None` when JSON cannot hold it.
fn json_object(py: Python<'_>, body: &Bound<'_, PyDict>) -> Option<Map<String, Value>> {
    let options = PyDict::new(py);
    options.set_item("allow_nan", false).ok()?;
    let dumps = py.import("json").ok()?.getattr("dumps").ok()?;
    let text: String = dumps.call((body,), Some(&options)).ok()?.extract().ok()?;
    serde_json::from_str(&text).ok()
}

/// What `recognize` gives for `outcome`.
fn answer(py: Python<'_>, outcome: OcrOutcome) -> PyResult<Bound<'_, PyAny>> {
    let failure = |attempted, failure: almiladoc::OcrFailure| {
        Bound::new(py, OcrFailure { attempted, status: failure.status }).map(Bound::into_any)
    };
    match outcome {
        OcrOutcome::Fill(GapFill::Markdown(text)) => Ok(PyString::new(py, &text).into_any()),
        OcrOutcome::Unattempted(why) => failure(false, why),
        OcrOutcome::Failed(why) => failure(true, why),
        _ => failure(false, almiladoc::OcrFailure::default()),
    }
}

/// Why OCR read no text for a request, which pickles: `attempted` false when
/// it was not tried, which leaves its gap for a later call, and true when it
/// failed, which fails its gap; `status`, the HTTP status of its last
/// attempt, if it had one.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct OcrFailure {
    attempted: bool,
    status: Option<u16>,
}

#[pymethods]
impl OcrFailure {
    #[new]
    #[pyo3(signature = (*, attempted = true, status = None))]
    fn new(attempted: bool, status: Option<u16>) -> Self {
        Self { attempted, status }
    }

    /// False when the request was not tried, true when it failed.
    #[getter]
    fn attempted(&self) -> bool {
        self.attempted
    }

    /// The HTTP status of the last attempt, if it had one.
    #[getter]
    fn status(&self) -> Option<u16> {
        self.status
    }

    fn __getnewargs_ex__<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<(Bound<'py, PyTuple>, Bound<'py, PyDict>)> {
        let arguments = PyDict::new(py);
        arguments.set_item("attempted", self.attempted)?;
        arguments.set_item("status", self.status)?;
        Ok((PyTuple::empty(py), arguments))
    }

    fn __repr__(&self) -> String {
        let attempted = if self.attempted { "True" } else { "False" };
        let status = self.status.map_or_else(|| "None".to_owned(), |status| status.to_string());
        format!("OcrFailure(attempted={attempted}, status={status})")
    }
}

/// What one `Parsed.ocr` call did to the gaps it could request.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct OcrSummary {
    /// Gaps filled.
    #[pyo3(get)]
    filled: usize,
    /// Gaps failed: the engine failed them, or a limit refused their fill.
    #[pyo3(get)]
    failed: usize,
    /// Gaps left for a later call: the engine did not attempt them, or a
    /// request before theirs was not attempted.
    #[pyo3(get)]
    unattempted: usize,
}

impl From<almiladoc::OcrSummary> for OcrSummary {
    fn from(summary: almiladoc::OcrSummary) -> Self {
        Self { filled: summary.filled, failed: summary.failed, unattempted: summary.unattempted }
    }
}

#[pymethods]
impl OcrSummary {
    fn __repr__(&self) -> String {
        format!(
            "OcrSummary(filled={}, failed={}, unattempted={})",
            self.filled, self.failed, self.unattempted
        )
    }
}
