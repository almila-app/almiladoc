//! `OcrRequest`, a gap's image exported for an OCR run elsewhere, and `OcrHttp`, the
//! engine that reads it.

use std::num::NonZeroU64;
use std::time::SystemTime;

use almiladoc::{
    ConfigError, GapFill, ImageDetail, MessageLayout, OcrFailureKind, OcrHttpConfig, OcrImage,
    OcrOutcome, OcrReport, RasterSpec, RefusalKind, RetryAfter, TierFallback, TokenLimitField,
};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyString, PyTuple, PyType};
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

/// A raster spec of `dpi`, `max_pixels` and `max_bytes`; a zero `max_bytes` raises `ConfigError`
/// with limit `raster`, as an out-of-range dpi does when the spec is used.
pub(crate) fn raster_spec(
    py: Python<'_>,
    dpi: u32,
    max_pixels: u64,
    max_bytes: Option<u64>,
) -> PyResult<RasterSpec> {
    let zero = || config_error(py, &ConfigError::Raster);
    let max_bytes = max_bytes.map(|bytes| NonZeroU64::new(bytes).ok_or_else(zero)).transpose()?;
    let mut spec = RasterSpec::default();
    (spec.dpi, spec.max_pixels, spec.max_bytes) = (dpi, max_pixels, max_bytes);
    Ok(spec)
}

/// What painting a page left out or replaced, by kind, which pickles: `text_runs`,
/// `images`, `inline_images`, `shadings`, `soft_masks` and `content_streams`
/// not painted or not applied as set, and `substituted_text_runs` painted in a
/// standard face standing in for the page's font. `lost` is what
/// `raster_incomplete` counts: every kind but shadings, soft masks and
/// substituted runs.
/// Annotation appearances, Lab/DeviceN colours and missing stand-in glyphs are not
/// observed, so zero counts are not a completeness claim.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct RasterFidelity(almiladoc::RasterFidelity);

#[pymethods]
impl RasterFidelity {
    #[new]
    #[pyo3(signature = (
        text_runs = 0, images = 0, inline_images = 0, shadings = 0, soft_masks = 0,
        content_streams = 0, substituted_text_runs = 0,
    ))]
    fn new(
        text_runs: u32,
        images: u32,
        inline_images: u32,
        shadings: u32,
        soft_masks: u32,
        content_streams: u32,
        substituted_text_runs: u32,
    ) -> Self {
        let mut fidelity = almiladoc::RasterFidelity::default();
        (fidelity.text_runs, fidelity.images, fidelity.inline_images) =
            (text_runs, images, inline_images);
        (fidelity.shadings, fidelity.soft_masks) = (shadings, soft_masks);
        (fidelity.content_streams, fidelity.substituted_text_runs) =
            (content_streams, substituted_text_runs);
        Self(fidelity)
    }

    /// Text runs not painted.
    #[getter]
    fn text_runs(&self) -> u32 {
        self.0.text_runs
    }

    /// Images not painted.
    #[getter]
    fn images(&self) -> u32 {
        self.0.images
    }

    /// Inline images not painted.
    #[getter]
    fn inline_images(&self) -> u32 {
        self.0.inline_images
    }

    /// Shadings and pattern colours not painted.
    #[getter]
    fn shadings(&self) -> u32 {
        self.0.shadings
    }

    /// Soft masks not applied as set.
    #[getter]
    fn soft_masks(&self) -> u32 {
        self.0.soft_masks
    }

    /// Content streams that could not be decoded.
    #[getter]
    fn content_streams(&self) -> u32 {
        self.0.content_streams
    }

    /// Text runs painted in a standard face standing in for the page's font.
    #[getter]
    fn substituted_text_runs(&self) -> u32 {
        self.0.substituted_text_runs
    }

    /// What the painting dropped from the page, as `raster_incomplete` counts it.
    #[getter]
    fn lost(&self) -> u64 {
        self.0.lost()
    }

    fn __getnewargs__(&self) -> (u32, u32, u32, u32, u32, u32, u32) {
        let f = &self.0;
        (
            f.text_runs,
            f.images,
            f.inline_images,
            f.shadings,
            f.soft_masks,
            f.content_streams,
            f.substituted_text_runs,
        )
    }

    fn __repr__(&self) -> String {
        let f = &self.0;
        format!(
            "RasterFidelity(text_runs={}, images={}, inline_images={}, shadings={}, \
             soft_masks={}, content_streams={}, substituted_text_runs={})",
            f.text_runs,
            f.images,
            f.inline_images,
            f.shadings,
            f.soft_masks,
            f.content_streams,
            f.substituted_text_runs,
        )
    }
}

/// One gap's image for an OCR run elsewhere, which pickles: `gap`, the row of
/// `Parsed.gaps` that `Parsed.fill_gap` and `Parsed.fail_gap` take; `image`,
/// its PNG or JPEG bytes; `media_type`, `image/png` or `image/jpeg`;
/// `max_text_bytes`, the most bytes of Markdown a fill of it takes; `dpi`,
/// `width` and `height`, the resolution and pixel size a page was painted at,
/// and `fidelity`, what the painting left out, each `None` for an embedded image.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct OcrRequest {
    gap: Py<PyTuple>,
    image: Py<PyBytes>,
    media_type: String,
    max_text_bytes: u64,
    dpi: Option<u32>,
    width: Option<u32>,
    height: Option<u32>,
    fidelity: Option<RasterFidelity>,
}

impl OcrRequest {
    pub(crate) fn of(
        py: Python<'_>,
        gap: GapRow,
        request: &almiladoc::OcrRequest,
    ) -> PyResult<Self> {
        let image = &request.image;
        Ok(Self {
            gap: gap.into_pyobject(py)?.unbind(),
            image: PyBytes::new(py, &image.bytes).unbind(),
            media_type: image.media_type.to_owned(),
            max_text_bytes: request.max_text_bytes,
            dpi: image.dpi,
            width: image.width,
            height: image.height,
            fidelity: image.fidelity.map(RasterFidelity),
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
    #[pyo3(signature = (
        gap, image, media_type, max_text_bytes, dpi = None, width = None, height = None,
        fidelity = None,
    ))]
    #[expect(
        clippy::too_many_arguments,
        reason = "each field of the Rust request's image is one argument"
    )]
    fn new(
        gap: Bound<'_, PyTuple>,
        image: Bound<'_, PyBytes>,
        media_type: String,
        max_text_bytes: u64,
        dpi: Option<u32>,
        width: Option<u32>,
        height: Option<u32>,
        fidelity: Option<&Bound<'_, RasterFidelity>>,
    ) -> Self {
        Self {
            gap: gap.unbind(),
            image: image.unbind(),
            media_type,
            max_text_bytes,
            dpi,
            width,
            height,
            fidelity: fidelity.map(|fidelity| fidelity.get().clone()),
        }
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

    /// The pixel width of a painted page; `None` for an embedded image.
    #[getter]
    fn width(&self) -> Option<u32> {
        self.width
    }

    /// The pixel height of a painted page; `None` for an embedded image.
    #[getter]
    fn height(&self) -> Option<u32> {
        self.height
    }

    /// What painting a page left out; `None` for an embedded image.
    #[getter]
    fn fidelity(&self) -> Option<RasterFidelity> {
        self.fidelity.clone()
    }

    #[expect(
        clippy::type_complexity,
        reason = "pickle's reduce tuple of a class and its arguments"
    )]
    fn __reduce__<'py>(
        slf: &Bound<'py, Self>,
    ) -> (
        Bound<'py, PyType>,
        (
            Bound<'py, PyTuple>,
            Bound<'py, PyBytes>,
            String,
            u64,
            Option<u32>,
            Option<u32>,
            Option<u32>,
            Option<RasterFidelity>,
        ),
    ) {
        let py = slf.py();
        let this = slf.get();
        let (media_type, max_text_bytes) = (this.media_type.clone(), this.max_text_bytes);
        let arguments = (
            this.gap(py),
            this.image(py),
            media_type,
            max_text_bytes,
            this.dpi,
            this.width,
            this.height,
            this.fidelity(),
        );
        (slf.get_type(), arguments)
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let fidelity =
            self.fidelity.as_ref().map_or_else(|| "None".to_owned(), RasterFidelity::__repr__);
        Ok(format!(
            "OcrRequest(gap={}, media_type={:?}, image=<{} bytes>, max_text_bytes={}, dpi={}, \
             width={}, height={}, fidelity={fidelity})",
            self.gap.bind(py).repr()?,
            self.media_type,
            self.image.bind(py).as_bytes().len(),
            self.max_text_bytes,
            optional(self.dpi),
            optional(self.width),
            optional(self.height),
        ))
    }
}

/// A gap that has no image to read, which pickles and stays unfilled: `gap`,
/// the row of `Parsed.gaps` it stands for, and `kind`, `unpaintable` for a
/// page that cannot be painted or `over_bytes` for an image over `max_bytes`,
/// whose `bytes` are the size it would have held; `None` for `unpaintable`.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct OcrRefusal {
    gap: Py<PyTuple>,
    kind: &'static str,
    bytes: Option<u64>,
}

impl OcrRefusal {
    pub(crate) fn of(
        py: Python<'_>,
        gap: GapRow,
        refusal: &almiladoc::OcrRefusal,
    ) -> PyResult<Self> {
        let bytes = match refusal.kind {
            RefusalKind::OverBytes { bytes } => Some(bytes),
            _ => None,
        };
        Ok(Self { gap: gap.into_pyobject(py)?.unbind(), kind: refusal.kind.as_str(), bytes })
    }
}

#[pymethods]
impl OcrRefusal {
    #[new]
    #[pyo3(signature = (gap, kind, bytes = None))]
    fn new(gap: Bound<'_, PyTuple>, kind: &str, bytes: Option<u64>) -> PyResult<Self> {
        let over_bytes = RefusalKind::OverBytes { bytes: 0 }.as_str();
        let kind = [RefusalKind::Unpaintable.as_str(), over_bytes]
            .into_iter()
            .find(|known| *known == kind)
            .ok_or_else(|| PyValueError::new_err("an OcrRefusal kind names no refusal"))?;
        if (kind == over_bytes) != bytes.is_some() {
            return Err(PyValueError::new_err("only an over_bytes refusal has bytes"));
        }
        Ok(Self { gap: gap.unbind(), kind, bytes })
    }

    /// The row of `Parsed.gaps` that has no image.
    #[getter]
    fn gap<'py>(&self, py: Python<'py>) -> Bound<'py, PyTuple> {
        self.gap.bind(py).clone()
    }

    /// `unpaintable` or `over_bytes`.
    #[getter]
    fn kind(&self) -> &'static str {
        self.kind
    }

    /// The bytes the encoded image would have held, for `over_bytes`.
    #[getter]
    fn bytes(&self) -> Option<u64> {
        self.bytes
    }

    fn __getnewargs__<'py>(
        &self,
        py: Python<'py>,
    ) -> (Bound<'py, PyTuple>, &'static str, Option<u64>) {
        (self.gap(py), self.kind, self.bytes)
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        Ok(format!(
            "OcrRefusal(gap={}, kind='{}', bytes={})",
            self.gap.bind(py).repr()?,
            self.kind,
            optional(self.bytes),
        ))
    }
}

/// Reads gaps' images through an OpenAI-compatible vision server, posting
/// each to `base_url`'s `chat/completions` with `model`: for `Parsed.ocr`, the
/// `ocr` of `Converter.convert`, or `recognize`. `api_key` goes only over
/// `https` or to loopback; `prompt`, `None` for the default, asks for each
/// image's Markdown; `layout` places it: `image_then_text` or
/// `text_then_image` in one user message, or `developer_then_image` or
/// `system_then_image` in a message of its own before the image's;
/// `extra_body` adds JSON fields to each request; `max_tokens` is sent as
/// `token_limit_field`, `max_tokens` or `max_completion_tokens`, and
/// `temperature`, 0 to 2, unless `None`; `image_detail` sends `low`, `high`
/// or `auto` with each image; a reply must name `answer_model` when one is
/// set; the first of up to 4 `service_tiers` is sent, and a 429 whose error
/// code is among `tier_fallback`'s codes or whose message starts with its
/// prefix moves the request once to the next; `preserve_table_spans` makes
/// a reply a `fill_gap_tables` fill, failing a gap below the top level;
/// a page is painted as
/// `Parsed.ocr_requests` paints it at `dpi` and `max_pixels`, and `Parsed.ocr`
/// refuses an image over `max_bytes`, `None` for no bound; a request the
/// server cannot take yet is tried up to `retries` more times, at most 10;
/// and `concurrency`, 1 to 16, requests go out at once. A field out of range
/// raises `ConfigError` naming it. No socket opens before a request and no
/// redirect is followed. `root_certificates_pem` replaces Mozilla roots,
/// including for an HTTPS proxy; `proxy` explicitly selects an HTTP(S)
/// CONNECT proxy and requires HTTPS when `api_key` is set.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct OcrHttp(pub(crate) almiladoc::OcrHttp);

#[pymethods]
impl OcrHttp {
    #[new]
    #[pyo3(signature = (
        base_url, model, *, api_key = None, prompt = None, layout = "image_then_text",
        extra_body = None, max_tokens = 8192, token_limit_field = "max_tokens",
        temperature = Some(0.0), image_detail = None, answer_model = None,
        service_tiers = Vec::new(), tier_fallback = None, preserve_table_spans = false, dpi = 150,
        max_pixels = 12_845_056, max_bytes = None, retries = 3, concurrency = 1,
        root_certificates_pem = None, proxy = None,
    ), text_signature = "(base_url, model, *, api_key=None, prompt=None, \
        layout='image_then_text', extra_body=None, max_tokens=8192, \
        token_limit_field='max_tokens', temperature=0.0, image_detail=None, answer_model=None, \
        service_tiers=(), tier_fallback=None, preserve_table_spans=False, dpi=150, \
        max_pixels=12845056, max_bytes=None, retries=3, concurrency=1, \
        root_certificates_pem=None, proxy=None)")]
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
        layout: &str,
        extra_body: Option<&Bound<'_, PyDict>>,
        max_tokens: u32,
        token_limit_field: &str,
        temperature: Option<f32>,
        image_detail: Option<&str>,
        answer_model: Option<String>,
        service_tiers: Vec<String>,
        tier_fallback: Option<(Vec<String>, String)>,
        preserve_table_spans: bool,
        dpi: u32,
        max_pixels: u64,
        max_bytes: Option<u64>,
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
        config.layout = choose(py, "layout", layout, &LAYOUTS)?;
        if let Some(body) = extra_body {
            config.extra_body =
                json_object(py, body).ok_or_else(|| ocr_http_refusal(py, "extra_body"))?;
        }
        config.token_limit_field = choose(py, "token_limit_field", token_limit_field, &FIELDS)?;
        config.image_detail =
            image_detail.map(|detail| choose(py, "image_detail", detail, &DETAILS)).transpose()?;
        (config.max_tokens, config.temperature) = (max_tokens, temperature);
        (config.answer_model, config.service_tiers) = (answer_model, service_tiers);
        config.tier_fallback =
            tier_fallback.map(|(codes, prefix)| TierFallback::new(codes, prefix));
        config.preserve_table_spans = preserve_table_spans;
        config.raster = raster_spec(py, dpi, max_pixels, max_bytes)?;
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

    /// An `OcrResult` for each of `requests`, in order, with the GIL released
    /// once their images are copied. Once one is not attempted or `cancel` is
    /// cancelled, no later request is sent, and a cancel ends those in flight.
    /// A request whose image is not its `media_type` raises `ValueError`.
    #[pyo3(signature = (requests, *, cancel = None))]
    fn recognize(
        &self,
        py: Python<'_>,
        requests: Vec<Bound<'_, OcrRequest>>,
        cancel: Option<&Bound<'_, CancelToken>>,
    ) -> PyResult<Vec<OcrResult>> {
        let images = requests
            .into_iter()
            .map(|request| request.get().to_read(py))
            .collect::<PyResult<Vec<_>>>()?;
        let cancel = cancel.map_or_else(almiladoc::CancelToken::new, |token| token.get().0.clone());
        let reports = py.detach(|| self.0.recognize_images(&images, &cancel));
        Ok(reports.into_iter().map(OcrResult::from).collect())
    }

    fn __repr__(&self) -> String {
        format!("{:?}", self.0)
    }
}

const LAYOUTS: [(&str, MessageLayout); 4] = [
    ("image_then_text", MessageLayout::ImageThenText),
    ("text_then_image", MessageLayout::TextThenImage),
    ("developer_then_image", MessageLayout::DeveloperThenImage),
    ("system_then_image", MessageLayout::SystemThenImage),
];
const FIELDS: [(&str, TokenLimitField); 2] = [
    ("max_tokens", TokenLimitField::MaxTokens),
    ("max_completion_tokens", TokenLimitField::MaxCompletionTokens),
];
const DETAILS: [(&str, ImageDetail); 3] =
    [("low", ImageDetail::Low), ("high", ImageDetail::High), ("auto", ImageDetail::Auto)];

/// The option `value` names, else `ConfigError` naming `field`.
fn choose<T: Copy>(
    py: Python<'_>,
    field: &'static str,
    value: &str,
    options: &[(&str, T)],
) -> PyResult<T> {
    options
        .iter()
        .find_map(|&(name, option)| (name == value).then_some(option))
        .ok_or_else(|| ocr_http_refusal(py, field))
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

/// What OCR made of one request, which pickles and holds no reply text but
/// its fill: `text`, the Markdown that fills its gap, through
/// `Parsed.fill_gap_tables` when `tables`, else `Parsed.fill_gap`; or
/// `failure`; with `usage`, the tokens a reply billed, `realized_tier`, the
/// index in `service_tiers` of the tier a reply was served on, and
/// `attempts`, each request sent.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct OcrResult {
    read: Result<(String, bool), OcrFailure>,
    usage: Option<TokenUsage>,
    realized_tier: Option<u8>,
    attempts: Vec<OcrAttempt>,
}

impl From<OcrReport> for OcrResult {
    fn from(report: OcrReport) -> Self {
        let failure = |attempted, why: almiladoc::OcrFailure| {
            let (kind, status, retry_after) = (why.kind, why.status, why.retry_after);
            Err(OcrFailure { attempted, kind, status, retry_after })
        };
        let read = match report.outcome {
            OcrOutcome::Fill(GapFill::Markdown(text)) => Ok((text, false)),
            OcrOutcome::Fill(GapFill::MarkdownTables(text)) => Ok((text, true)),
            OcrOutcome::Failed(why) => failure(true, why),
            OcrOutcome::Unattempted(why) => failure(false, why),
            _ => failure(false, almiladoc::OcrFailure::new(OcrFailureKind::Protocol, None)),
        };
        let attempts = report.attempts.iter().map(|a| OcrAttempt(a.tier, a.status)).collect();
        let usage = report.usage.map(|u| TokenUsage(u.input_uncached, u.input_cached, u.output));
        Self { read, usage, realized_tier: report.realized_tier, attempts }
    }
}

#[pymethods]
impl OcrResult {
    #[new]
    #[pyo3(signature = (
        *, text = None, tables = false, failure = None, usage = None, realized_tier = None,
        attempts = Vec::new(),
    ), text_signature = "(*, text=None, tables=False, failure=None, usage=None, \
        realized_tier=None, attempts=())")]
    fn new(
        text: Option<String>,
        tables: bool,
        failure: Option<&Bound<'_, OcrFailure>>,
        usage: Option<&Bound<'_, TokenUsage>>,
        realized_tier: Option<u8>,
        attempts: Vec<Bound<'_, OcrAttempt>>,
    ) -> PyResult<Self> {
        let read = match (text, failure) {
            (Some(text), None) => Ok((text, tables)),
            (None, Some(failure)) if !tables => Err(failure.get().clone()),
            _ => return Err(PyValueError::new_err("an OcrResult holds a text or a failure")),
        };
        let attempts = attempts.into_iter().map(|attempt| attempt.get().clone()).collect();
        Ok(Self { read, usage: usage.map(|u| u.get().clone()), realized_tier, attempts })
    }

    /// The Markdown that fills the gap; `None` with a failure.
    #[getter]
    fn text(&self) -> Option<&str> {
        self.read.as_ref().ok().map(|(text, _)| text.as_str())
    }

    /// Whether `text` fills through `Parsed.fill_gap_tables`.
    #[getter]
    fn tables(&self) -> bool {
        self.read.as_ref().is_ok_and(|(_, tables)| *tables)
    }

    /// Why no text was read; `None` with a text.
    #[getter]
    fn failure(&self) -> Option<OcrFailure> {
        self.read.as_ref().err().cloned()
    }

    /// The tokens a reply billed, a refused one included, when it named them.
    #[getter]
    fn usage(&self) -> Option<TokenUsage> {
        self.usage.clone()
    }

    /// The index in `service_tiers` of the tier a reply was served on.
    #[getter]
    fn realized_tier(&self) -> Option<u8> {
        self.realized_tier
    }

    /// Each request sent, in order.
    #[getter]
    fn attempts(&self) -> Vec<OcrAttempt> {
        self.attempts.clone()
    }

    fn __getnewargs_ex__<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<(Bound<'py, PyTuple>, Bound<'py, PyDict>)> {
        let arguments = PyDict::new(py);
        arguments.set_item("text", self.text())?;
        arguments.set_item("tables", self.tables())?;
        arguments.set_item("failure", self.failure())?;
        arguments.set_item("usage", self.usage())?;
        arguments.set_item("realized_tier", self.realized_tier)?;
        arguments.set_item("attempts", self.attempts())?;
        Ok((PyTuple::empty(py), arguments))
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let read = match &self.read {
            Ok((text, tables)) => {
                format!("text=<{} bytes>, tables={}", text.len(), py_bool(*tables))
            }
            Err(failure) => format!("failure={}", failure.__repr__(py)?),
        };
        let usage = self.usage.as_ref().map_or_else(|| "None".to_owned(), TokenUsage::__repr__);
        let attempts: Vec<_> = self.attempts.iter().map(OcrAttempt::__repr__).collect();
        Ok(format!(
            "OcrResult({read}, usage={usage}, realized_tier={}, attempts=[{}])",
            optional(self.realized_tier),
            attempts.join(", ")
        ))
    }
}

/// Why OCR read no text for a request, which pickles: `kind`, what kept it
/// from text, one of `rejected`, `too_large`, `refused`, `unfinished`,
/// `no_text`, `protocol`, `wrong_model`, `busy`, `unreachable` or
/// `cancelled`; `attempted` false when it was not tried, which leaves its
/// gap for a later call, and true when it failed, which fails its gap;
/// `status`, the HTTP status of its last attempt, if it had one; and
/// `retry_after`, the longest wait a server asked for, as seconds or an
/// aware UTC `datetime`, a later wait of the other form replacing it.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct OcrFailure {
    attempted: bool,
    kind: OcrFailureKind,
    status: Option<u16>,
    retry_after: Option<RetryAfter>,
}

#[pymethods]
impl OcrFailure {
    #[new]
    #[pyo3(signature = (*, kind, attempted = true, status = None, retry_after = None))]
    fn new(
        kind: &str,
        attempted: bool,
        status: Option<u16>,
        retry_after: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let kind = OcrFailureKind::ALL
            .iter()
            .copied()
            .find(|known| known.as_str() == kind)
            .ok_or_else(|| PyValueError::new_err("an OcrFailure kind names no failure"))?;
        let retry_after = retry_after
            .map(|after| match after.extract::<u64>() {
                Ok(_) if after.is_instance_of::<PyBool>() => {
                    Err(PyTypeError::new_err("retry_after is seconds or an aware datetime"))
                }
                Ok(seconds) => Ok(RetryAfter::Seconds(seconds)),
                Err(_) => after.extract::<SystemTime>().map(RetryAfter::At),
            })
            .transpose()?;
        Ok(Self { attempted, kind, status, retry_after })
    }

    /// What kept the request from text.
    #[getter]
    fn kind(&self) -> &'static str {
        self.kind.as_str()
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

    /// The longest wait a server asked for, a later wait of the other form
    /// replacing it: seconds, or an aware UTC `datetime`.
    #[getter]
    fn retry_after<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        self.retry_after
            .map(|after| match after {
                RetryAfter::Seconds(seconds) => Ok(seconds.into_pyobject(py)?.into_any()),
                RetryAfter::At(time) => Ok(time.into_pyobject(py)?.into_any()),
            })
            .transpose()
    }

    fn __getnewargs_ex__<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<(Bound<'py, PyTuple>, Bound<'py, PyDict>)> {
        let arguments = PyDict::new(py);
        arguments.set_item("kind", self.kind())?;
        arguments.set_item("attempted", self.attempted)?;
        arguments.set_item("status", self.status)?;
        arguments.set_item("retry_after", self.retry_after(py)?)?;
        Ok((PyTuple::empty(py), arguments))
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let after = self.retry_after(py)?;
        let after =
            after.map_or_else(|| Ok("None".to_owned()), |a| a.repr().map(|r| r.to_string()))?;
        Ok(format!(
            "OcrFailure(kind='{}', attempted={}, status={}, retry_after={after})",
            self.kind(),
            py_bool(self.attempted),
            optional(self.status),
        ))
    }
}

/// The tokens one reply billed, which pickles.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct TokenUsage(u64, u64, u64);

#[pymethods]
impl TokenUsage {
    #[new]
    fn new(input_uncached: u64, input_cached: u64, output: u64) -> Self {
        Self(input_uncached, input_cached, output)
    }

    /// Prompt tokens not read from a cache.
    #[getter]
    fn input_uncached(&self) -> u64 {
        self.0
    }

    /// Prompt tokens read from a cache.
    #[getter]
    fn input_cached(&self) -> u64 {
        self.1
    }

    /// Reply tokens.
    #[getter]
    fn output(&self) -> u64 {
        self.2
    }

    fn __getnewargs__(&self) -> (u64, u64, u64) {
        (self.0, self.1, self.2)
    }

    fn __repr__(&self) -> String {
        format!("TokenUsage(input_uncached={}, input_cached={}, output={})", self.0, self.1, self.2)
    }
}

/// One request sent for an image, which pickles: `tier`, the index in
/// `service_tiers` of the tier asked for, and `status`, the reply's HTTP
/// status, when one arrived.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct OcrAttempt(Option<u8>, Option<u16>);

#[pymethods]
impl OcrAttempt {
    #[new]
    #[pyo3(signature = (tier = None, status = None))]
    fn new(tier: Option<u8>, status: Option<u16>) -> Self {
        Self(tier, status)
    }

    /// The index in `service_tiers` of the tier asked for.
    #[getter]
    fn tier(&self) -> Option<u8> {
        self.0
    }

    /// The reply's HTTP status, when one arrived.
    #[getter]
    fn status(&self) -> Option<u16> {
        self.1
    }

    fn __getnewargs__(&self) -> (Option<u8>, Option<u16>) {
        (self.0, self.1)
    }

    fn __repr__(&self) -> String {
        format!("OcrAttempt(tier={}, status={})", optional(self.0), optional(self.1))
    }
}

const fn py_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

fn optional(value: Option<impl ToString>) -> String {
    value.map_or_else(|| "None".to_owned(), |value| value.to_string())
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
    /// Gaps left unfilled because no image could be made of them: a page
    /// that cannot be painted, or an image over `max_bytes`.
    #[pyo3(get)]
    refused: usize,
}

impl From<almiladoc::OcrSummary> for OcrSummary {
    fn from(summary: almiladoc::OcrSummary) -> Self {
        Self {
            filled: summary.filled,
            failed: summary.failed,
            unattempted: summary.unattempted,
            refused: summary.refused,
        }
    }
}

#[pymethods]
impl OcrSummary {
    fn __repr__(&self) -> String {
        format!(
            "OcrSummary(filled={}, failed={}, unattempted={}, refused={})",
            self.filled, self.failed, self.unattempted, self.refused
        )
    }
}
