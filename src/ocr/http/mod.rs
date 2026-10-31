mod wire;

use core::net::IpAddr;
use std::fmt;
use std::io::ErrorKind;
use std::time::Duration;

use serde_json::{Map, Value};
use ureq::Agent;
use ureq::config::ConfigBuilder;
use ureq::http::Uri;
use ureq::http::header::{AUTHORIZATION, CONTENT_TYPE, EXPECT, RETRY_AFTER};
use ureq::typestate::AgentScope;

use crate::config::ConfigError;
use crate::ledger::CancelToken;
use crate::ocr::{GapFill, OcrEngine, OcrFailure, OcrImage, OcrOutcome, OcrRequest, RasterSpec};
use crate::pool::{fan_out, wait};

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
    /// Whether the image comes before the prompt in the message; true by default.
    pub image_first: bool,
    /// More top-level fields of each request body, none of `messages`, `model`, `max_tokens`,
    /// `temperature`, `n` and `stream`.
    pub extra_body: Map<String, Value>,
    /// The most tokens a reply holds; 8192 by default.
    pub max_tokens: u32,
    /// The sampling temperature, 0 to 2; 0 by default.
    pub temperature: f32,
    /// How large an image of a page the server takes.
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
    const MAX_ROOTS: usize = 256;
    const MAX_RETRIES: u32 = 10;
    const MAX_CONCURRENCY: usize = 16;

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
            image_first: true,
            extra_body: Map::new(),
            max_tokens: 8192,
            temperature: 0.0,
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
            .field("image_first", &self.image_first)
            .field("extra_body", &self.extra_body.keys().collect::<Vec<_>>())
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
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
/// doubling to 30 s, or the `Retry-After` seconds up to 60 s, a cancel ending the wait. A
/// request the server rejects (400, 413 or 422) or a reply that holds no Markdown fails its
/// gap; any other failure leaves its gap unattempted. It follows no redirect, uses only an explicitly
/// configured proxy, verifies TLS against Mozilla or the caller's replacement roots on rustls'
/// process-wide provider or ring, and reads a
/// reply body of under six times the request's `max_text_bytes` and 64 KiB. ureq logs each
/// request's head, the key included, at `log`'s trace level: a process whose logger keeps
/// ureq's trace records the key.
pub struct OcrHttp {
    agent: Agent,
    endpoint: Uri,
    authorization: Option<String>,
    root_certificates_pem: Option<Vec<u8>>,
    proxy: Option<String>,
    template: wire::Template,
    raster: RasterSpec,
    retries: u32,
    concurrency: usize,
}

impl fmt::Debug for OcrHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OcrHttp")
            .field("endpoint", &Redacted(&self.endpoint.to_string()))
            .field("authorized", &self.authorization.is_some())
            .field("raster", &self.raster)
            .field("retries", &self.retries)
            .field("concurrency", &self.concurrency)
            .finish_non_exhaustive()
    }
}

/// Status codes of a request the server could not take yet.
const RETRYABLE: [u16; 6] = [408, 429, 500, 502, 503, 504];
/// Status codes of a request the server rejects.
const REJECTED: [u16; 3] = [400, 413, 422];
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
    /// `temperature` outside 0 to 2, `retries` above 10, or `concurrency` outside 1 to 16;
    /// `root_certificates_pem` empty, oversized, or holding any item other than a structurally
    /// valid certificate; `proxy` oversized or not an HTTP(S) URI with only an authority and
    /// optional `/`; or `api_key` combined with a proxy for an HTTP endpoint.
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
        if wire::RESERVED.iter().any(|field| config.extra_body.contains_key(*field)) {
            return Err(refuse("extra_body"));
        }
        if config.max_tokens == 0 {
            return Err(refuse("max_tokens"));
        }
        if !(0.0..=2.0).contains(&config.temperature) {
            return Err(refuse("temperature"));
        }
        if config.retries > OcrHttpConfig::MAX_RETRIES {
            return Err(refuse("retries"));
        }
        if !(1..=OcrHttpConfig::MAX_CONCURRENCY).contains(&config.concurrency) {
            return Err(refuse("concurrency"));
        }
        let (tls, proxy) = transport_config(&config)?;
        let mut fields = config.extra_body;
        fields.insert("model".into(), config.model.into());
        fields.insert("max_tokens".into(), config.max_tokens.into());
        fields.insert("temperature".into(), config.temperature.into());
        let template =
            wire::Template { fields, prompt: config.prompt, image_first: config.image_first };
        Ok(Self {
            agent: configured(Agent::config_builder()).tls_config(tls).proxy(proxy).build().into(),
            endpoint,
            authorization,
            root_certificates_pem: config.root_certificates_pem,
            proxy: config.proxy,
            template,
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
    /// takes, read as [`OcrEngine::recognize`] reads requests: for images outside a parse, such
    /// as the requests of a parse in another process.
    ///
    /// # Examples
    ///
    /// ```
    /// use almiladoc::{CancelToken, OcrFailure, OcrHttp, OcrHttpConfig, OcrImage, OcrOutcome};
    ///
    /// let engine = OcrHttp::new(OcrHttpConfig::new("http://127.0.0.1:1/v1", "vision-model"))?;
    /// let image = OcrImage::new(*b"\xff\xd8\xff\xe0", None).ok_or("not an image")?;
    /// let outcomes = engine.recognize_images(&[(image, 4096)], &CancelToken::new());
    /// assert_eq!(outcomes, [OcrOutcome::Unattempted(OcrFailure::new(None))]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn recognize_images(
        &self,
        images: &[(OcrImage, u64)],
        cancel: &CancelToken,
    ) -> Vec<OcrOutcome> {
        self.read_all(images, |(image, max_text_bytes)| (image, *max_text_bytes), cancel)
    }

    /// The outcome of each of `items`, whose image and text limit `part` gives.
    fn read_all<T: Sync>(
        &self,
        items: &[T],
        part: impl Fn(&T) -> (&OcrImage, u64) + Sync,
        cancel: &CancelToken,
    ) -> Vec<OcrOutcome> {
        let unattempted = || OcrOutcome::Unattempted(OcrFailure::new(None));
        let read = fan_out(items, self.concurrency, |item| {
            if cancel.is_cancelled() {
                return (unattempted(), true);
            }
            let (image, max_text_bytes) = part(item);
            let outcome = self.read(image, max_text_bytes, cancel);
            let stop = matches!(outcome, OcrOutcome::Unattempted(_));
            (outcome, stop)
        });
        read.into_iter().map(|outcome| outcome.unwrap_or_else(unattempted)).collect()
    }

    /// What the server made of `image`, after retries.
    fn read(&self, image: &OcrImage, max_text_bytes: u64, cancel: &CancelToken) -> OcrOutcome {
        let body = self.template.body(image);
        let cap = max_text_bytes.saturating_mul(6).saturating_add(64 << 10);
        let (mut status, mut pause) = (None, None);
        for attempt in 0..=self.retries {
            if let Some(delay) = pause.take()
                && !wait(delay, cancel)
            {
                break;
            }
            match self.post(&body, cap) {
                Exchange::Read(text) => return OcrOutcome::Fill(GapFill::Markdown(text)),
                Exchange::Rejected(code) => return OcrOutcome::Failed(OcrFailure::new(code)),
                Exchange::Stopped(code) => return OcrOutcome::Unattempted(OcrFailure::new(code)),
                Exchange::Later(code, after) => {
                    status = code;
                    pause = Some(delay(attempt, after.as_deref()));
                }
            }
        }
        OcrOutcome::Unattempted(OcrFailure::new(status))
    }

    fn post(&self, body: &[u8], cap: u64) -> Exchange {
        let mut post = self
            .agent
            .post(&self.endpoint)
            .header(CONTENT_TYPE, "application/json")
            .header(EXPECT, "100-continue");
        if let Some(authorization) = &self.authorization {
            post = post.header(AUTHORIZATION, authorization);
        }
        let mut response = match post.send(body) {
            Ok(response) => response,
            Err(ureq::Error::Io(error)) if reset(error.kind()) => {
                return Exchange::Later(None, None);
            }
            Err(_) => return Exchange::Stopped(None),
        };
        let code = response.status().as_u16();
        if RETRYABLE.contains(&code) {
            let after = response.headers().get(RETRY_AFTER).and_then(|value| value.to_str().ok());
            return Exchange::Later(Some(code), after.map(str::to_owned));
        }
        if REJECTED.contains(&code) {
            return Exchange::Rejected(Some(code));
        }
        if !response.status().is_success() {
            return Exchange::Stopped(Some(code));
        }
        match response.body_mut().with_config().limit(cap).read_to_vec() {
            Ok(reply) => {
                wire::content(&reply).map_or(Exchange::Rejected(Some(code)), Exchange::Read)
            }
            Err(ureq::Error::BodyExceedsLimit(_)) => Exchange::Rejected(Some(code)),
            Err(_) => Exchange::Stopped(Some(code)),
        }
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
        self.read_all(requests, |request| (&request.image, request.max_text_bytes), cancel)
    }
}

/// One attempt's result.
enum Exchange {
    Read(String),
    Rejected(Option<u16>),
    Later(Option<u16>, Option<String>),
    Stopped(Option<u16>),
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
fn delay(attempt: u32, retry_after: Option<&str>) -> Duration {
    match retry_after.and_then(|value| value.trim().parse::<u64>().ok()) {
        Some(seconds) => Duration::from_secs(seconds).min(MAX_RETRY_AFTER),
        None => FIRST_DELAY.saturating_mul(1 << attempt.min(6)).min(MAX_DELAY),
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
        hot.temperature = 2.5;
        let mut unknown = config(base);
        unknown.temperature = f32::NAN;
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
    fn no_debug_holds_a_key_a_prompt_or_a_url_s_user_or_query() {
        let mut config = config("https://user:pass@h:8443/v1?sig=qq");
        config.api_key = Some("sk-secret".into());
        config.prompt = "private prompt".into();
        config.extra_body.insert("top_p".into(), Value::from("hidden"));
        let shown = format!("{config:?}");
        for hidden in ["sk-secret", "private prompt", "user", "pass", "sig", "qq", "hidden"] {
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
        let seconds = |attempt, after| delay(attempt, after).as_secs_f64();
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
