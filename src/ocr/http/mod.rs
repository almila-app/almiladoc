mod wire;

use core::net::IpAddr;
use std::fmt;
use std::io::{ErrorKind, Read as _};
use std::time::Duration;

use serde_json::{Map, Value};
use ureq::config::ConfigBuilder;
use ureq::http::header::{AUTHORIZATION, CONTENT_TYPE, EXPECT, RETRY_AFTER};
use ureq::http::{Response, Uri};
use ureq::typestate::AgentScope;
use ureq::{Agent, Body};

pub use wire::{ImageDetail, MessageLayout, TierFallback, TokenLimitField};

use crate::config::ConfigError;
use crate::ledger::CancelToken;
use crate::ocr::{
    GapFill, OcrEngine, OcrFailure, OcrFailureKind, OcrImage, OcrOutcome, OcrRequest, RasterSpec,
    RetryAfter,
};
use crate::pool::{Spawn, fan_out, on_a_thread, wait, watched};

/// How [`OcrHttp`] reaches an OpenAI-compatible vision server.
///
/// # Examples
///
/// ```
/// use almiladoc::{OcrHttp, OcrHttpConfig};
///
/// let mut config = OcrHttpConfig::new("http://127.0.0.1:8000/v1", "vision-model");
/// config.api_key = Some("secret".into());
/// let engine = OcrHttp::new(config)?;
/// assert!(!format!("{engine:?}").contains("secret"));
/// assert_eq!(engine.root_certificates_pem(), None);
/// assert_eq!(engine.proxy(), None);
/// # Ok::<(), almiladoc::ConfigError>(())
/// ```
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub struct OcrHttpConfig {
    /// The API's base URL, `http` or `https` with no user, query or fragment; each image is
    /// posted to its `chat/completions`.
    pub base_url: String,
    /// The model the server runs.
    pub model: String,
    /// A bearer token, sent only over `https` or directly to a loopback host.
    pub api_key: Option<String>,
    /// A certificate-only PEM bundle replacing Mozilla roots, at most [`Self::MAX_ROOT_PEM_BYTES`]
    /// bytes and 256 certificates; it also authenticates an HTTPS proxy.
    pub root_certificates_pem: Option<Vec<u8>>,
    /// An explicit HTTP(S) CONNECT proxy URI, including optional credentials, at most
    /// [`Self::MAX_PROXY_BYTES`] bytes; absent means direct connections, regardless of environment.
    pub proxy: Option<String>,
    /// What the model is asked of each image; [`OcrHttpConfig::DEFAULT_PROMPT`] by default.
    pub prompt: String,
    /// How the messages carry the prompt and the image; the image, then the prompt, by default.
    pub layout: MessageLayout,
    /// More top-level fields of each request body, none of `messages`, `model`, `max_tokens`,
    /// `max_completion_tokens`, `temperature`, `n`, `stream` and `service_tier`.
    pub extra_body: Map<String, Value>,
    /// The most tokens a reply holds; 8192 by default.
    pub max_tokens: u32,
    /// The body field `max_tokens` is sent as; `max_tokens` by default.
    pub token_limit_field: TokenLimitField,
    /// The sampling temperature, 0 to 2, or `None` to send none; 0 by default.
    pub temperature: Option<f32>,
    /// The `image_url.detail` sent with each image; none by default.
    pub image_detail: Option<ImageDetail>,
    /// The model a reply must name to be read; any by default.
    pub answer_model: Option<String>,
    /// Service tiers to ask for, at most 4 tokens of up to 32 ASCII letters, digits, `-` or
    /// `_`; the first is sent, and none when empty, as by default.
    pub service_tiers: Vec<String>,
    /// Which `429` replies move a request to its next service tier, each matching one again
    /// until the last; none by default.
    pub tier_fallback: Option<TierFallback>,
    /// Whether a reply fills as [`GapFill::MarkdownTables`], keeping the spans of its HTML and
    /// OTSL tables as HTML at a top-level gap, rather than [`GapFill::Markdown`]; false by default.
    pub preserve_table_spans: bool,
    /// How large an image the server takes: what [`Parsed::ocr`](crate::Parsed::ocr) paints a
    /// page at and refuses an image over.
    pub raster: RasterSpec,
    /// Attempts after the first for a request the server could not take yet, at most 10; 3 by
    /// default.
    pub retries: u32,
    /// Requests in flight at once, 1 to 16; 1 by default. A batch holds twice as many.
    pub concurrency: usize,
}

impl OcrHttpConfig {
    /// The prompt a config starts with.
    pub const DEFAULT_PROMPT: &str = "Transcribe all text in this image as Markdown, keeping \
        its headings, lists and tables, and reply with the Markdown alone.";
    /// The largest caller-supplied PEM trust bundle.
    pub const MAX_ROOT_PEM_BYTES: usize = 1 << 20;
    /// The largest caller-supplied proxy URI.
    pub const MAX_PROXY_BYTES: usize = 8 << 10;
    /// The most service tiers a config names.
    pub const MAX_SERVICE_TIERS: usize = 4;
    const MAX_ROOTS: usize = 256;
    const MAX_RETRIES: u32 = 10;
    const MAX_CONCURRENCY: usize = 16;
    const MAX_TIER_BYTES: usize = 32;
    const MAX_FALLBACK_CODES: usize = 16;
    const MAX_FALLBACK_BYTES: usize = 256;

    /// The defaults for `model` behind `base_url`.
    #[must_use]
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: None,
            root_certificates_pem: None,
            proxy: None,
            prompt: Self::DEFAULT_PROMPT.into(),
            layout: MessageLayout::default(),
            extra_body: Map::new(),
            max_tokens: 8192,
            token_limit_field: TokenLimitField::default(),
            temperature: Some(0.0),
            image_detail: None,
            answer_model: None,
            service_tiers: Vec::new(),
            tier_fallback: None,
            preserve_table_spans: false,
            raster: RasterSpec::default(),
            retries: 3,
            concurrency: 1,
        }
    }
}

impl fmt::Debug for OcrHttpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OcrHttpConfig")
            .field("base_url", &Redacted(&self.base_url))
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("root_certificates_pem", &self.root_certificates_pem.as_ref().map(Vec::len))
            .field("proxy", &self.proxy.as_ref().map(|_| "<redacted>"))
            .field("prompt", &format_args!("<{} bytes>", self.prompt.len()))
            .field("layout", &self.layout)
            .field("extra_body", &self.extra_body.keys().collect::<Vec<_>>())
            .field("max_tokens", &self.max_tokens)
            .field("token_limit_field", &self.token_limit_field)
            .field("temperature", &self.temperature)
            .field("image_detail", &self.image_detail)
            .field("answer_model", &self.answer_model)
            .field("service_tiers", &self.service_tiers)
            .field("tier_fallback", &self.tier_fallback)
            .field("preserve_table_spans", &self.preserve_table_spans)
            .field("raster", &self.raster)
            .field("retries", &self.retries)
            .field("concurrency", &self.concurrency)
            .finish()
    }
}

/// A URL shown with its scheme, host, port and path alone.
struct Redacted<'a>(&'a str);

impl fmt::Debug for Redacted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Ok(uri) = self.0.parse::<Uri>() else { return f.write_str("<unparsed>") };
        let (scheme, host) = (uri.scheme_str().unwrap_or_default(), uri.host().unwrap_or_default());
        let port = uri.port_u16().map(|port| format!(":{port}")).unwrap_or_default();
        write!(f, "\"{scheme}://{host}{port}{}\"", uri.path())
    }
}

/// An [`OcrEngine`] that asks an OpenAI-compatible vision server for each image's Markdown,
/// one image per request and up to its `concurrency` at once, on the calling thread and scoped
/// ones that end before [`OcrEngine::recognize`] returns; once a request is left unattempted or
/// the token is cancelled, the batch takes no further request, those taken keeping their outcome.
///
/// Each request asks the server to answer before its body is sent (`Expect: 100-continue`),
/// sending it after a second without an answer. A request the server could not take yet (a
/// reset before its response, or 408, 429, 500, 502, 503 or 504) is retried after 0.5 s
/// doubling to 30 s, or the `Retry-After` seconds up to 60 s, a cancel ending the wait; a 429
/// that [`OcrHttpConfig::tier_fallback`] matches moves to the next service tier at once instead.
/// A 413, a reply past its cap, a refusal or an unfinished reply fails its gap; any other
/// failure leaves it unattempted, its [`OcrFailureKind`] naming why. It follows no redirect,
/// uses only an explicitly configured proxy, verifies TLS against Mozilla or the caller's
/// replacement roots on rustls' process-wide provider or ring, and reads a reply body of fewer
/// than six times the request's `max_text_bytes` plus 64 KiB. Its timeouts are fixed: 10 s
/// to connect, 60 s to send the request, 60 s to send its body, 300 s to receive the response
/// and 60 s to receive its body.
///
/// Each call opens connections of its own, which a cancel closes at once, so the requests in
/// flight end as [`OcrFailureKind::Cancelled`]; a connect in progress first runs on to its
/// 10 s timeout, and a name lookup, which has none, for as long as the system resolver takes.
/// A call the system grants no thread to watch its token sends nothing, leaving each request
/// [`OcrFailureKind::Busy`].
/// ureq logs each request's head, the key included, at `log`'s trace level: a
/// process whose logger keeps ureq's trace records the key.
pub struct OcrHttp {
    config: ureq::config::Config,
    endpoint: Uri,
    authorization: Option<String>,
    root_certificates_pem: Option<Vec<u8>>,
    proxy: Option<String>,
    template: wire::Template,
    answer_model: Option<String>,
    service_tiers: Vec<String>,
    tier_fallback: Option<TierFallback>,
    preserve_table_spans: bool,
    raster: RasterSpec,
    retries: u32,
    concurrency: usize,
}

impl fmt::Debug for OcrHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OcrHttp")
            .field("endpoint", &Redacted(&self.endpoint.to_string()))
            .field("authorized", &self.authorization.is_some())
            .field("service_tiers", &self.service_tiers)
            .field("raster", &self.raster)
            .field("retries", &self.retries)
            .field("concurrency", &self.concurrency)
            .finish_non_exhaustive()
    }
}

/// What [`OcrHttp::recognize_images`] made of one image, holding no text of a reply but its fill.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OcrReport {
    /// What [`OcrEngine::recognize`] gives for the image.
    pub outcome: OcrOutcome,
    /// The tokens a reply billed, a refused one included, when it named them validly.
    pub usage: Option<TokenUsage>,
    /// The index in [`OcrHttpConfig::service_tiers`] of the tier a reply was served on.
    pub realized_tier: Option<u8>,
    /// Each request sent for the image, in order.
    pub attempts: Vec<OcrAttempt>,
}

impl OcrReport {
    fn untried(kind: OcrFailureKind) -> Self {
        let outcome = OcrOutcome::Unattempted(OcrFailure::new(kind, None));
        Self { outcome, usage: None, realized_tier: None, attempts: Vec::new() }
    }
}

/// The tokens one reply billed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct TokenUsage {
    /// Prompt tokens not read from a cache.
    pub input_uncached: u64,
    /// Prompt tokens read from a cache.
    pub input_cached: u64,
    /// Reply tokens.
    pub output: u64,
}

/// One request sent for an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct OcrAttempt {
    /// The index in [`OcrHttpConfig::service_tiers`] of the tier asked for.
    pub tier: Option<u8>,
    /// The reply's HTTP status, when one arrived.
    pub status: Option<u16>,
}

const FIRST_DELAY: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(30);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

impl OcrHttp {
    /// An engine for `config`, opening no socket.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Raster`] when its raster spec is out of range, and
    /// [`ConfigError::OcrHttp`] naming the first field refused: `base_url` that is not `http`
    /// or `https` with a host and no user, query or fragment, an empty `model`, `api_key` that
    /// is empty, holds anything but visible ASCII, or would go over `http` to a host other than
    /// loopback, `extra_body` naming a field the engine sets or reads, `max_tokens` of 0,
    /// `temperature` outside 0 to 2, `retries` above 10, `concurrency` outside 1 to 16, an
    /// empty `answer_model`, `service_tiers` past their count or token form, `tier_fallback`
    /// without a later tier, with neither a code nor a prefix, or past 16 codes or 256 bytes
    /// per code or prefix; `root_certificates_pem` empty, oversized, or holding any item other
    /// than a structurally valid certificate; `proxy` oversized or not an HTTP(S) URI with only
    /// an authority and optional `/`; or `api_key` combined with a proxy for an HTTP endpoint.
    pub fn new(config: OcrHttpConfig) -> Result<Self, ConfigError> {
        config.raster.validate()?;
        let refuse = |field| ConfigError::OcrHttp { field };
        let (endpoint, secure) = endpoint(&config.base_url).ok_or(refuse("base_url"))?;
        if config.model.is_empty() {
            return Err(refuse("model"));
        }
        let authorization = match &config.api_key {
            Some(key) if key.is_empty() || !key.bytes().all(|b| b.is_ascii_graphic()) => {
                return Err(refuse("api_key"));
            }
            Some(_)
                if !secure
                    || (config.proxy.is_some() && endpoint.scheme_str() != Some("https")) =>
            {
                return Err(refuse("api_key"));
            }
            Some(key) => Some(format!("Bearer {key}")),
            None => None,
        };
        if let Some(field) = refused_field(&config) {
            return Err(refuse(field));
        }
        let (tls, proxy) = transport_config(&config)?;
        let mut fields = config.extra_body;
        fields.insert("model".into(), config.model.into());
        fields.insert(config.token_limit_field.name().into(), config.max_tokens.into());
        if let Some(temperature) = config.temperature {
            fields.insert("temperature".into(), temperature.into());
        }
        let (prompt, layout, detail) = (config.prompt, config.layout, config.image_detail);
        Ok(Self {
            config: configured(Agent::config_builder()).tls_config(tls).proxy(proxy).build(),
            endpoint,
            authorization,
            root_certificates_pem: config.root_certificates_pem,
            proxy: config.proxy,
            template: wire::Template { fields, prompt, layout, detail },
            answer_model: config.answer_model,
            service_tiers: config.service_tiers,
            tier_fallback: config.tier_fallback,
            preserve_table_spans: config.preserve_table_spans,
            raster: config.raster,
            retries: config.retries,
            concurrency: config.concurrency,
        })
    }

    /// The configured replacement trust bundle, absent for Mozilla roots.
    #[must_use]
    pub fn root_certificates_pem(&self) -> Option<&[u8]> {
        self.root_certificates_pem.as_deref()
    }

    /// The configured proxy URI, including any caller-supplied credentials.
    #[must_use]
    pub fn proxy(&self) -> Option<&str> {
        self.proxy.as_deref()
    }

    /// What the server makes of each of `images`, each with the most bytes of Markdown its fill
    /// takes, read as [`OcrEngine::recognize`] reads requests, with what each request billed and
    /// tried: for images outside a parse, such as the requests of a parse in another process.
    ///
    /// # Examples
    ///
    /// ```
    /// use almiladoc::{
    ///     CancelToken, OcrFailure, OcrFailureKind, OcrHttp, OcrHttpConfig, OcrImage, OcrOutcome,
    /// };
    ///
    /// let engine = OcrHttp::new(OcrHttpConfig::new("http://127.0.0.1:1/v1", "vision-model"))?;
    /// let image = OcrImage::new(*b"\xff\xd8\xff\xe0", None).ok_or("not an image")?;
    /// let [report] = engine.recognize_images(&[(image, 4096)], &CancelToken::new()).try_into()
    ///     .map_err(|_| "one report")?;
    /// let unreachable = OcrFailure::new(OcrFailureKind::Unreachable, None);
    /// assert_eq!(report.outcome, OcrOutcome::Unattempted(unreachable));
    /// assert_eq!((report.usage, report.attempts.len()), (None, 1));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn recognize_images(
        &self,
        images: &[(OcrImage, u64)],
        cancel: &CancelToken,
    ) -> Vec<OcrReport> {
        self.read_all(on_a_thread, images, |(image, max)| (image, *max), cancel)
    }

    /// The report of each of `items`, whose image and text limit `part` gives, on an agent of
    /// this call's own that a cancel shuts down from the watcher `spawn` starts; with none, each
    /// is left unattempted as [`OcrFailureKind::Busy`] before any is sent.
    fn read_all<T: Sync>(
        &self,
        spawn: Spawn,
        items: &[T],
        part: impl Fn(&T) -> (&OcrImage, u64) + Sync,
        cancel: &CancelToken,
    ) -> Vec<OcrReport> {
        let agent = Agent::new_with_config(self.config.clone());
        let read = watched(
            spawn,
            cancel,
            || agent.clone().shutdown(),
            || {
                fan_out(items, self.concurrency, |item| {
                    if cancel.is_cancelled() {
                        return (OcrReport::untried(OcrFailureKind::Cancelled), true);
                    }
                    let (image, max_text_bytes) = part(item);
                    let report = self.read(&agent, image, max_text_bytes, cancel);
                    let stop = matches!(report.outcome, OcrOutcome::Unattempted(_));
                    (report, stop)
                })
            },
        );
        let Some(read) = read else {
            return items.iter().map(|_| OcrReport::untried(OcrFailureKind::Busy)).collect();
        };
        let stopped = read
            .iter()
            .flatten()
            .find_map(|report| match report.outcome {
                OcrOutcome::Unattempted(failure) => Some(failure.kind),
                _ => None,
            })
            .unwrap_or(OcrFailureKind::Cancelled);
        read.into_iter()
            .map(|report| report.unwrap_or_else(|| OcrReport::untried(stopped)))
            .collect()
    }

    fn read(
        &self,
        agent: &Agent,
        image: &OcrImage,
        max_text_bytes: u64,
        cancel: &CancelToken,
    ) -> OcrReport {
        let cap = max_text_bytes.saturating_mul(6).saturating_add(64 << 10);
        let tier_name = |tier: usize| self.service_tiers.get(tier).map(String::as_str);
        let (mut tier, mut retried, mut pause) = (0, 0, None);
        let mut body = self.template.body(image, tier_name(tier));
        let mut report = OcrReport::untried(OcrFailureKind::Cancelled);
        let (mut status, mut retry_after) = (None, None);
        while pause.take().is_none_or(|delay| wait(delay, cancel)) && !cancel.is_cancelled() {
            let fallback =
                self.tier_fallback.as_ref().filter(|_| tier + 1 < self.service_tiers.len());
            let exchange = self.post(agent, &body, cap, fallback, cancel);
            let sent = tier_name(tier).and_then(|_| u8::try_from(tier).ok());
            report.attempts.push(OcrAttempt { tier: sent, status: exchange.status });
            (status, retry_after) =
                (exchange.status, wire::longer(retry_after, exchange.retry_after));
            (report.usage, report.realized_tier) = (exchange.usage, exchange.tier);
            let failure = move |kind| OcrFailure::new(kind, status).with_retry_after(retry_after);
            report.outcome = match exchange.step {
                Step::Read(text) if self.preserve_table_spans => {
                    OcrOutcome::Fill(GapFill::MarkdownTables(text))
                }
                Step::Read(text) => OcrOutcome::Fill(GapFill::Markdown(text)),
                Step::Failed(kind) => OcrOutcome::Failed(failure(kind)),
                Step::Stopped(kind) => OcrOutcome::Unattempted(failure(kind)),
                Step::Later(kind) if retried == self.retries => {
                    OcrOutcome::Unattempted(failure(kind))
                }
                Step::Later(_) => {
                    pause = Some(delay(retried, exchange.retry_after));
                    retried += 1;
                    continue;
                }
                Step::NextTier => {
                    tier += 1;
                    body = self.template.body(image, tier_name(tier));
                    continue;
                }
            };
            return report;
        }
        let cancelled = OcrFailure::new(OcrFailureKind::Cancelled, status);
        report.outcome = OcrOutcome::Unattempted(cancelled.with_retry_after(retry_after));
        report
    }

    fn post(
        &self,
        agent: &Agent,
        body: &[u8],
        cap: u64,
        fallback: Option<&TierFallback>,
        cancel: &CancelToken,
    ) -> Exchange {
        let mut post = agent
            .post(&self.endpoint)
            .header(CONTENT_TYPE, "application/json")
            .header(EXPECT, "100-continue");
        if let Some(authorization) = &self.authorization {
            post = post.header(AUTHORIZATION, authorization);
        }
        let mut response = match post.send(body) {
            Ok(response) => response,
            Err(_) if cancel.is_cancelled() => {
                return Exchange::bare(Step::Stopped(OcrFailureKind::Cancelled), None);
            }
            Err(ureq::Error::Io(error)) if reset(error.kind()) => {
                return Exchange::bare(Step::Later(OcrFailureKind::Unreachable), None);
            }
            Err(ureq::Error::Protocol(_)) => {
                return Exchange::bare(Step::Stopped(OcrFailureKind::Protocol), None);
            }
            Err(_) => return Exchange::bare(Step::Stopped(OcrFailureKind::Unreachable), None),
        };
        let code = response.status().as_u16();
        let retry_after = matches!(code, 408 | 429 | 500..)
            .then(|| response.headers().get(RETRY_AFTER)?.to_str().ok().and_then(wire::retry_after))
            .flatten();
        let step = match code {
            _ if cancel.is_cancelled() => Step::Stopped(OcrFailureKind::Cancelled),
            200..=299 => return self.answer(&mut response, cap, cancel),
            429 if let Some(fallback) = fallback => {
                let head = error_head(&mut response);
                if cancel.is_cancelled() {
                    Step::Stopped(OcrFailureKind::Cancelled)
                } else if fallback.matches(&head) {
                    Step::NextTier
                } else {
                    Step::Later(OcrFailureKind::Busy)
                }
            }
            408 | 429 | 500 | 502..=504 => Step::Later(OcrFailureKind::Busy),
            413 => Step::Failed(OcrFailureKind::TooLarge),
            400..=499 => Step::Stopped(OcrFailureKind::Rejected),
            500.. => Step::Stopped(OcrFailureKind::Busy),
            _ => Step::Stopped(OcrFailureKind::Protocol),
        };
        Exchange { step, status: Some(code), retry_after, usage: None, tier: None }
    }

    /// What a successful `response` says, trusted only while the token is not cancelled, as a
    /// shut-down connection ends a close-delimited body early.
    fn answer(&self, response: &mut Response<Body>, cap: u64, cancel: &CancelToken) -> Exchange {
        let status = Some(response.status().as_u16());
        let read = response.body_mut().with_config().limit(cap).read_to_vec();
        let bare = |step| Exchange::bare(step, status);
        if cancel.is_cancelled() {
            return bare(Step::Stopped(OcrFailureKind::Cancelled));
        }
        let body = match read {
            Ok(body) => body,
            Err(ureq::Error::BodyExceedsLimit(_)) => {
                return bare(Step::Failed(OcrFailureKind::TooLarge));
            }
            Err(ureq::Error::Io(_) | ureq::Error::Timeout(_)) => {
                return bare(Step::Stopped(OcrFailureKind::Unreachable));
            }
            Err(_) => return bare(Step::Stopped(OcrFailureKind::Protocol)),
        };
        let reply = wire::reply(&body, self.answer_model.as_deref(), &self.service_tiers);
        let step = match reply.answer {
            Ok(text) => Step::Read(text),
            Err(kind @ (OcrFailureKind::Refused | OcrFailureKind::Unfinished)) => {
                Step::Failed(kind)
            }
            Err(kind) => Step::Stopped(kind),
        };
        Exchange { step, status, retry_after: None, usage: reply.usage, tier: reply.tier }
    }
}

impl OcrEngine for OcrHttp {
    fn batch_size(&self) -> usize {
        self.concurrency * 2
    }

    fn raster(&self) -> RasterSpec {
        self.raster
    }

    fn recognize(&self, requests: &[OcrRequest], cancel: &CancelToken) -> Vec<OcrOutcome> {
        let reports = self.read_all(
            on_a_thread,
            requests,
            |request| (&request.image, request.max_text_bytes),
            cancel,
        );
        reports.into_iter().map(|report| report.outcome).collect()
    }
}

/// One attempt's result.
struct Exchange {
    step: Step,
    status: Option<u16>,
    retry_after: Option<RetryAfter>,
    usage: Option<TokenUsage>,
    tier: Option<u8>,
}

impl Exchange {
    const fn bare(step: Step, status: Option<u16>) -> Self {
        Self { step, status, retry_after: None, usage: None, tier: None }
    }
}

enum Step {
    Read(String),
    Failed(OcrFailureKind),
    Stopped(OcrFailureKind),
    Later(OcrFailureKind),
    NextTier,
}

/// The first [`TierFallback::ERROR_HEAD_BYTES`] of `response`'s body, as many as arrive.
fn error_head(response: &mut Response<Body>) -> Vec<u8> {
    let mut head = Vec::new();
    let reader = response.body_mut().as_reader();
    let _partial = reader.take(TierFallback::ERROR_HEAD_BYTES).read_to_end(&mut head);
    head
}

/// The first field of `config` that [`OcrHttp::new`] refuses past its endpoint, model and key,
/// before its transport.
fn refused_field(config: &OcrHttpConfig) -> Option<&'static str> {
    let token = |text: &str, most| {
        (1..=most).contains(&text.len())
            && text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    };
    let fallback = |fallback: &TierFallback| {
        config.service_tiers.len() >= 2
            && (!fallback.codes.is_empty() || !fallback.message_prefix.is_empty())
            && fallback.codes.len() <= OcrHttpConfig::MAX_FALLBACK_CODES
            && fallback.message_prefix.len() <= OcrHttpConfig::MAX_FALLBACK_BYTES
            && fallback.codes.iter().all(|code| {
                (1..=OcrHttpConfig::MAX_FALLBACK_BYTES).contains(&code.len())
                    && code.bytes().all(|b| b.is_ascii_graphic())
            })
    };
    [
        (wire::RESERVED.iter().any(|field| config.extra_body.contains_key(*field)), "extra_body"),
        (config.max_tokens == 0, "max_tokens"),
        (config.temperature.is_some_and(|t| !(0.0..=2.0).contains(&t)), "temperature"),
        (config.retries > OcrHttpConfig::MAX_RETRIES, "retries"),
        (!(1..=OcrHttpConfig::MAX_CONCURRENCY).contains(&config.concurrency), "concurrency"),
        (config.answer_model.as_ref().is_some_and(String::is_empty), "answer_model"),
        (
            config.service_tiers.len() > OcrHttpConfig::MAX_SERVICE_TIERS
                || !config.service_tiers.iter().all(|t| token(t, OcrHttpConfig::MAX_TIER_BYTES)),
            "service_tiers",
        ),
        (config.tier_fallback.as_ref().is_some_and(|f| !fallback(f)), "tier_fallback"),
    ]
    .into_iter()
    .find_map(|(refused, field)| refused.then_some(field))
}

/// `base_url`'s `chat/completions` endpoint, and whether a key may travel to it.
fn endpoint(base_url: &str) -> Option<(Uri, bool)> {
    let uri: Uri = base_url.parse().ok()?;
    let authority = uri.authority()?;
    let secure = match uri.scheme_str()? {
        "https" => true,
        "http" => loopback(authority.host()),
        _ => return None,
    };
    if authority.as_str().contains('@') || uri.query().is_some() || base_url.contains('#') {
        return None;
    }
    let path = format!("{}/chat/completions", uri.path().trim_end_matches('/'));
    let endpoint = Uri::builder()
        .scheme(uri.scheme_str()?)
        .authority(authority.as_str())
        .path_and_query(path)
        .build()
        .ok()?;
    Some((endpoint, secure))
}

fn loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn transport_config(
    config: &OcrHttpConfig,
) -> Result<(ureq::tls::TlsConfig, Option<ureq::Proxy>), ConfigError> {
    let roots = trust_roots(config.root_certificates_pem.as_deref())?;
    let proxy = config
        .proxy
        .as_deref()
        .map(|value| explicit_proxy(value).ok_or(ConfigError::OcrHttp { field: "proxy" }))
        .transpose()?;
    Ok((ureq::tls::TlsConfig::builder().root_certs(roots).build(), proxy))
}

fn explicit_proxy(value: &str) -> Option<ureq::Proxy> {
    if value.len() > OcrHttpConfig::MAX_PROXY_BYTES || value.contains('#') {
        return None;
    }
    let uri: Uri = value.parse().ok()?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.query().is_some()
        || !matches!(uri.path(), "" | "/")
        || uri.host()?.is_empty()
    {
        return None;
    }
    let authority = uri.authority()?.as_str().rsplit('@').next()?;
    let port = authority.strip_prefix(uri.host()?)?;
    if !port.is_empty() {
        let port = port.strip_prefix(':')?;
        if !port.bytes().all(|byte| byte.is_ascii_digit()) || port.parse::<u16>().ok()? == 0 {
            return None;
        }
    }
    ureq::Proxy::new(value).ok()
}

fn trust_roots(pem: Option<&[u8]>) -> Result<ureq::tls::RootCerts, ConfigError> {
    use ureq::tls::{Certificate, RootCerts};
    let Some(mut rest) = pem else { return Ok(RootCerts::WebPki) };
    let refused = || ConfigError::OcrHttp { field: "root_certificates_pem" };
    if rest.len() > OcrHttpConfig::MAX_ROOT_PEM_BYTES {
        return Err(refused());
    }
    const BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";
    const END: &[u8] = b"-----END CERTIFICATE-----";
    let mut certificates = Vec::new();
    let mut validation = rustls::RootCertStore::empty();
    while !rest.trim_ascii().is_empty() {
        rest = rest.trim_ascii_start();
        let body = rest.strip_prefix(BEGIN).ok_or_else(refused)?;
        let end = body.windows(END.len()).position(|part| part == END).ok_or_else(refused)?;
        if !body[..end].iter().all(|byte| {
            byte.is_ascii_alphanumeric()
                || byte.is_ascii_whitespace()
                || matches!(byte, b'+' | b'/' | b'=')
        }) {
            return Err(refused());
        }
        let consumed = BEGIN.len() + end + END.len();
        let certificate = Certificate::from_pem(&rest[..consumed]).map_err(|_| refused())?;
        validation
            .add(rustls::pki_types::CertificateDer::from(certificate.der()))
            .map_err(|_| refused())?;
        certificates.push(certificate);
        if certificates.len() > OcrHttpConfig::MAX_ROOTS {
            return Err(refused());
        }
        rest = &rest[consumed..];
    }
    if certificates.is_empty() {
        return Err(refused());
    }
    Ok(certificates.into())
}

/// `builder` defaults to direct connections, no redirect, statuses as responses,
/// and no resolve, per-call or overall timeout, each of which would resolve on a thread of its
/// own.
fn configured(builder: ConfigBuilder<AgentScope>) -> ConfigBuilder<AgentScope> {
    builder
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_send_request(Some(Duration::from_secs(60)))
        .timeout_send_body(Some(Duration::from_secs(60)))
        .timeout_recv_response(Some(Duration::from_secs(300)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
}

/// Whether an I/O error before the response means the connection dropped.
fn reset(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof
    )
}

/// The wait after attempt `attempt`, counted from 0: `retry_after`'s seconds up to 60, or else
/// 0.5 s doubling to 30 s.
fn delay(attempt: u32, retry_after: Option<RetryAfter>) -> Duration {
    match retry_after {
        Some(RetryAfter::Seconds(seconds)) => Duration::from_secs(seconds).min(MAX_RETRY_AFTER),
        _ => FIRST_DELAY.saturating_mul(1 << attempt.min(6)).min(MAX_DELAY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(base_url: &str) -> OcrHttpConfig {
        OcrHttpConfig::new(base_url, "m")
    }

    fn refused(config: OcrHttpConfig) -> Option<&'static str> {
        match OcrHttp::new(config) {
            Err(ConfigError::OcrHttp { field }) => Some(field),
            _ => None,
        }
    }

    #[test]
    fn a_base_url_takes_a_host_and_no_user_query_or_fragment() {
        for base in
            ["http://127.0.0.1:8000/v1", "https://example.com/v1/", "http://[::1]:9/", "https://h"]
        {
            assert_eq!(refused(config(base)), None, "{base}");
        }
        for base in [
            "",
            "127.0.0.1:8000",
            "ftp://h/v1",
            "http:///v1",
            "https://u:p@h/v1",
            "https://h/v1?k=1",
            "https://h/v1#f",
        ] {
            assert_eq!(refused(config(base)), Some("base_url"), "{base}");
        }
        let (endpoint, _) = endpoint("https://h:8443/v1/").unwrap();
        assert_eq!(endpoint.to_string(), "https://h:8443/v1/chat/completions");
    }

    #[test]
    fn a_key_travels_only_over_https_or_to_loopback() {
        for (base, key, field) in [
            ("http://127.0.0.1/v1", "k", None),
            ("http://127.8.0.1/v1", "k", None),
            ("http://[::1]/v1", "k", None),
            ("http://LocalHost/v1", "k", None),
            ("https://192.0.2.1/v1", "k", None),
            ("http://192.0.2.1/v1", "k", Some("api_key")),
            ("http://127.0.0.1.example/v1", "k", Some("api_key")),
            ("https://h/v1", "", Some("api_key")),
            ("https://h/v1", "a b", Some("api_key")),
            ("https://h/v1", "k\n", Some("api_key")),
        ] {
            let mut config = config(base);
            config.api_key = Some(key.into());
            assert_eq!(refused(config), field, "{base} {key:?}");
        }
        assert_eq!(refused(config("http://192.0.2.1/v1")), None);
    }

    #[test]
    fn each_other_field_is_checked() {
        let base = "https://h/v1";
        let mut empty = config(base);
        empty.model = String::new();
        let reserved = wire::RESERVED.map(|field| {
            let mut reserved = config(base);
            reserved.extra_body.insert(field.into(), Value::from("other"));
            reserved
        });
        let mut tokens = config(base);
        tokens.max_tokens = 0;
        let mut hot = config(base);
        hot.temperature = Some(2.5);
        let mut unknown = config(base);
        unknown.temperature = Some(f32::NAN);
        let mut unnamed = config(base);
        unnamed.answer_model = Some(String::new());
        let mut retries = config(base);
        retries.retries = 11;
        let mut widest = config(base);
        widest.concurrency = 16;
        assert_eq!(refused(widest), None);
        let concurrency = [0, 17].map(|concurrency| {
            let mut refused = config(base);
            refused.concurrency = concurrency;
            (refused, "concurrency")
        });
        for (config, field) in [
            (empty, "model"),
            (tokens, "max_tokens"),
            (hot, "temperature"),
            (unknown, "temperature"),
            (retries, "retries"),
            (unnamed, "answer_model"),
        ]
        .into_iter()
        .chain(reserved.map(|config| (config, "extra_body")))
        .chain(concurrency)
        {
            assert_eq!(refused(config), Some(field));
        }
        let mut raster = config(base);
        raster.raster.dpi = 601;
        assert_eq!(OcrHttp::new(raster).err(), Some(ConfigError::Raster));
    }

    #[test]
    fn service_tiers_and_their_fallback_are_checked() {
        let base = "https://h/v1";
        let tiers = |names: &[&str]| {
            let mut config = config(base);
            config.service_tiers = names.iter().map(|&tier| tier.into()).collect();
            config
        };
        let fallback = |names: &[&str], codes: &[&str], prefix: &str| {
            let mut config = tiers(names);
            let codes = codes.iter().map(|&code| code.into()).collect();
            config.tier_fallback = Some(TierFallback::new(codes, prefix));
            config
        };
        let mut open = fallback(&["flex", "default", "a", "b-_9"], &["c"; 16], &"p".repeat(256));
        open.temperature = None;
        open.answer_model = Some("m".into());
        assert_eq!(refused(open), None);
        for (config, field) in [
            (tiers(&["a"; 5]), "service_tiers"),
            (tiers(&[""]), "service_tiers"),
            (tiers(&["flex tier"]), "service_tiers"),
            (tiers(&[&"t".repeat(33)]), "service_tiers"),
            (fallback(&["flex"], &["c"], ""), "tier_fallback"),
            (fallback(&["flex", "default"], &[], ""), "tier_fallback"),
            (fallback(&["flex", "default"], &["c"; 17], ""), "tier_fallback"),
            (fallback(&["flex", "default"], &["a b"], ""), "tier_fallback"),
            (fallback(&["flex", "default"], &[], &"p".repeat(257)), "tier_fallback"),
        ] {
            assert_eq!(refused(config), Some(field));
        }
    }

    #[test]
    fn no_debug_holds_a_key_a_prompt_or_a_url_s_user_or_query() {
        let mut config = config("https://user:pass@h:8443/v1?sig=qq");
        config.api_key = Some("sk-secret".into());
        config.prompt = "private prompt".into();
        config.extra_body.insert("top_p".into(), Value::from("hidden"));
        config.tier_fallback = Some(TierFallback::new(vec!["busy".into()], "private message"));
        let shown = format!("{config:?}");
        for hidden in [
            "sk-secret",
            "private prompt",
            "user",
            "pass",
            "sig",
            "qq",
            "hidden",
            "private message",
        ] {
            assert!(!shown.contains(hidden), "{hidden}: {shown}");
        }
        assert!(shown.contains("\"https://h:8443/v1\"") && shown.contains("top_p"), "{shown}");
        let mut config = OcrHttpConfig::new("https://h/v1", "m");
        config.api_key = Some("sk-secret".into());
        let shown = format!("{:?}", OcrHttp::new(config).unwrap());
        assert!(!shown.contains("sk-secret") && shown.contains("authorized: true"), "{shown}");
    }

    #[test]
    fn the_agent_drops_a_proxy_follows_no_redirect_resolves_on_no_thread_and_verifies_tls() {
        let proxy = ureq::Proxy::new("http://127.0.0.1:1").unwrap();
        let config = configured(Agent::config_builder().proxy(Some(proxy))).build();
        assert!(config.proxy().is_none());
        let timeouts = config.timeouts();
        assert_eq!((timeouts.global, timeouts.per_call, timeouts.resolve), (None, None, None));
        assert_eq!(config.max_redirects(), 0);
        assert!(!config.http_status_as_error());
        let tls = config.tls_config();
        assert_eq!(tls.provider(), ureq::tls::TlsProvider::Rustls);
        assert!(matches!(tls.root_certs(), ureq::tls::RootCerts::WebPki));
        assert!(!tls.disable_verification());
    }

    #[test]
    fn a_wait_takes_retry_after_up_to_a_minute_or_doubles_to_thirty_seconds() {
        let seconds = |attempt, after: Option<&str>| {
            delay(attempt, after.and_then(wire::retry_after)).as_secs_f64()
        };
        for (attempt, after, waited) in [
            (0, None, 0.5),
            (1, None, 1.0),
            (5, None, 16.0),
            (6, None, 30.0),
            (10, None, 30.0),
            (0, Some(" 7 "), 7.0),
            (0, Some("0"), 0.0),
            (3, Some("3600"), 60.0),
            (1, Some("Wed, 21 Oct 2026 07:28:00 GMT"), 1.0),
        ] {
            assert_eq!(seconds(attempt, after), waited, "{attempt} {after:?}");
        }
    }

    #[test]
    fn a_call_without_a_watcher_sends_nothing_and_leaves_each_request_busy() {
        let engine = OcrHttp::new(config("http://localhost:1/v1")).unwrap();
        let image = OcrImage::new(*b"\xff\xd8\xff\xe0", None).unwrap();
        let no_thread: Spawn = |_, _| Err(ErrorKind::WouldBlock.into());
        let images = [(image.clone(), 64), (image, 64)];
        let reports =
            engine.read_all(no_thread, &images, |(image, max)| (image, *max), &CancelToken::new());
        assert_eq!(reports, vec![OcrReport::untried(OcrFailureKind::Busy); 2]);
    }

    #[test]
    fn only_a_dropped_connection_counts_as_a_reset() {
        for (kind, dropped) in [
            (ErrorKind::ConnectionReset, true),
            (ErrorKind::ConnectionAborted, true),
            (ErrorKind::BrokenPipe, true),
            (ErrorKind::UnexpectedEof, true),
            (ErrorKind::ConnectionRefused, false),
            (ErrorKind::TimedOut, false),
            (ErrorKind::Other, false),
        ] {
            assert_eq!(reset(kind), dropped, "{kind:?}");
        }
    }
    #[test]
    fn only_explicit_http_proxies_are_accepted_and_never_shown() {
        for uri in [
            "",
            "proxy:80",
            "ftp://h",
            "socks5://h",
            "http://h:0",
            "http://h:99999",
            "http://h:abc",
            "http://h:+80",
            "http://h/path",
            "http://h?q=private",
            "http://h#private",
            "http://u:%FF@h",
        ] {
            let mut config = config("https://h/v1");
            config.proxy = Some(uri.into());
            assert_eq!(refused(config), Some("proxy"), "{uri}");
        }
        for uri in ["http://proxy", "https://user:pass@proxy:8080/", "http://[::1]:80"] {
            let mut config = config("https://h/v1");
            config.proxy = Some(uri.into());
            let engine = OcrHttp::new(config).unwrap();
            assert_eq!(engine.proxy(), Some(uri));
            assert_eq!(engine.root_certificates_pem(), None);
        }
        let mut config = config("http://localhost/v1");
        config.api_key = Some("sk-secret".into());
        config.proxy = Some("https://user:pass@proxy".into());
        assert_eq!(refused(config.clone()), Some("api_key"));
        config.root_certificates_pem = Some(b"private roots".to_vec());
        let shown = format!("{config:?}");
        for secret in ["sk-secret", "user", "pass", "private roots"] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }
}
