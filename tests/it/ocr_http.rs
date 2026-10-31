//! `OcrHttp` against a loopback server that answers each connection from a script.
#![expect(
    clippy::disallowed_types,
    reason = "a loopback server stands in for the vision server, and a child test process carries a proxy environment"
)]

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::build::{docx_with, relationships};
use crate::support::{configured, fixture};
use almiladoc::{
    CancelToken, Format, GapFill, ImageOcr, Input, OcrEngine, OcrFailure, OcrHttp, OcrHttpConfig,
    OcrImage, OcrOutcome, OcrRequest, OnLoss, Parsed, RasterSpec,
};
use serde_json::{Value, json};

/// A scripted answer: a status with extra header lines and a body once the request is read, the
/// connection closed unanswered, or a status sent on the request's head, its body left unread.
enum Answer {
    Reply(u16, &'static str, Vec<u8>),
    Close,
    Early(u16),
}

fn reply(content: &str) -> Answer {
    let body =
        json!({ "choices": [{ "message": { "content": content }, "finish_reason": "stop" }] });
    Answer::Reply(200, "", body.to_string().into_bytes())
}

fn status(code: u16, headers: &'static str) -> Answer {
    Answer::Reply(code, headers, Vec::new())
}

/// A request as the server read it: its head, and its body unless it was left unread; a
/// connection past the script is seen with an empty head.
struct Seen {
    head: String,
    body: Option<Value>,
}

struct Server {
    base_url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    serving: JoinHandle<()>,
}

impl Server {
    fn start(answers: Vec<Answer>) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (seen, stop) = (Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicBool::new(false)));
        let (log, stopped) = (Arc::clone(&seen), Arc::clone(&stop));
        let serving = thread::spawn(move || {
            let mut answers = answers.into_iter();
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let seen = answers
                            .next()
                            .map_or_else(Seen::unscripted, |answer| serve(&stream, answer));
                        log.lock().unwrap().push(seen);
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        });
        Self { base_url, seen, stop, serving }
    }

    /// Every connection served, its last given a moment to arrive.
    fn finish(self) -> Vec<Seen> {
        thread::sleep(Duration::from_millis(50));
        self.stop.store(true, Ordering::Relaxed);
        self.serving.join().unwrap();
        Arc::into_inner(self.seen).unwrap().into_inner().unwrap()
    }
}

impl Seen {
    fn unscripted() -> Self {
        Self { head: String::new(), body: None }
    }
}

fn serve(stream: &TcpStream, answer: Answer) -> Seen {
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let early = matches!(answer, Answer::Early(_));
    let seen = serve_on(&mut &*stream, answer);
    if early {
        stream.shutdown(Shutdown::Write).unwrap();
        thread::sleep(Duration::from_millis(200));
    }
    seen
}

fn read_head(reader: &mut impl BufRead) -> std::io::Result<String> {
    let mut head = String::new();
    while !head.ends_with("\r\n\r\n") {
        if reader.read_line(&mut head)? == 0 {
            return Err(ErrorKind::UnexpectedEof.into());
        }
    }
    Ok(head)
}

fn serve_on(stream: &mut (impl Read + Write), answer: Answer) -> Seen {
    let mut reader = BufReader::new(stream);
    let head = read_head(&mut reader).unwrap();
    if let Answer::Early(code) = answer {
        let early =
            format!("HTTP/1.1 {code} Early\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        reader.get_mut().write_all(early.as_bytes()).unwrap();
        reader.get_mut().flush().unwrap();
        return Seen { head, body: None };
    }
    if head.to_ascii_lowercase().contains("\r\nexpect: 100-continue\r\n") {
        reader.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n").unwrap();
        reader.get_mut().flush().unwrap();
    }
    let length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|n| n.trim().parse().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    if let Answer::Reply(code, headers, body) = answer {
        let head = format!(
            "HTTP/1.1 {code} Scripted\r\ncontent-length: {}\r\nconnection: close\r\n{headers}\r\n",
            body.len()
        );
        reader.get_mut().write_all(head.as_bytes()).unwrap();
        reader.get_mut().write_all(&body).unwrap();
        reader.get_mut().flush().unwrap();
    }
    Seen { head, body: Some(serde_json::from_slice(&body).unwrap()) }
}

/// A document showing one PNG of `size` bytes that do not compress.
fn noisy_picture(size: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let noise = std::iter::repeat_with(|| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state.to_le_bytes()[0]
    });
    let png: Vec<u8> = b"\x89PNG\r\n\x1a\n".iter().copied().chain(noise).take(size).collect();
    let body = r#"<w:p><w:r><w:drawing><a:blip r:embed="png"/></w:drawing></w:r></w:p>"#;
    let rels = relationships(&[("png", "image", "media/a.png")]);
    docx_with(
        body,
        &[("word/_rels/document.xml.rels", rels.as_bytes()), ("word/media/a.png", &png)],
    )
}

fn engine(base_url: &str, set: impl FnOnce(&mut OcrHttpConfig)) -> OcrHttp {
    let mut config = OcrHttpConfig::new(base_url, "vision");
    set(&mut config);
    OcrHttp::new(config).unwrap()
}

/// The rich DOCX fixture with its image a gap, and that image's request six times over.
fn images(max_ocr_text_bytes: u64) -> (Parsed, Vec<OcrRequest>) {
    let converter = configured(|config| {
        config.image_ocr = ImageOcr::All;
        config.on_loss = OnLoss::Report;
        config.limits.max_ocr_text_bytes = max_ocr_text_bytes;
    });
    let mut parsed =
        converter.parse(Input::new(&fixture("docx/handmade-rich.docx"), Format::Docx)).unwrap();
    let [request] = parsed.ocr_requests(&RasterSpec::default(), None).unwrap().try_into().unwrap();
    (parsed, vec![request; 6])
}

fn one(engine: &OcrHttp, request: &OcrRequest) -> OcrOutcome {
    let [outcome] =
        engine.recognize(std::slice::from_ref(request), &CancelToken::new()).try_into().unwrap();
    outcome
}

fn failed(status: Option<u16>) -> OcrOutcome {
    OcrOutcome::Failed(OcrFailure::new(status))
}

fn unattempted(status: Option<u16>) -> OcrOutcome {
    OcrOutcome::Unattempted(OcrFailure::new(status))
}

fn filled(text: &str) -> OcrOutcome {
    OcrOutcome::Fill(GapFill::Markdown(text.into()))
}

#[test]
fn a_reply_fills_its_gap_and_a_request_holds_the_model_one_image_and_the_prompt() {
    let (mut parsed, requests) = images(1 << 20);
    let server = Server::start(vec![reply("<think>x</think>\n# A")]);
    let summary = parsed.ocr(&engine(&server.base_url, |_| {}), None).unwrap();
    assert_eq!((summary.filled, summary.failed, summary.unattempted), (1, 0, 0));
    assert!(parsed.render().unwrap().markdown().contains("\n# A\n"));
    let [seen] = server.finish().try_into().ok().unwrap();
    assert!(seen.head.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"), "{}", seen.head);
    assert!(!seen.head.to_ascii_lowercase().contains("authorization"));
    let body = seen.body.unwrap();
    let parts = &body["messages"][0]["content"];
    assert_eq!(
        (&body["model"], &body["max_tokens"], &body["temperature"]),
        (&json!("vision"), &json!(8192), &json!(0.0))
    );
    assert_eq!(parts.as_array().map(Vec::len), Some(2));
    assert!(
        parts[0]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,iVBOR")
    );
    assert_eq!(parts[1]["text"], json!(OcrHttpConfig::DEFAULT_PROMPT));

    let server = Server::start(vec![reply("image")]);
    let keyed = engine(&server.base_url, |config| {
        config.api_key = Some("sk-local".into());
        config.image_first = false;
        config.extra_body.insert("top_p".into(), json!(0.9));
    });
    assert_eq!(one(&keyed, &requests[0]), filled("image"));
    let [seen] = server.finish().try_into().ok().unwrap();
    assert!(seen.head.contains("authorization: Bearer sk-local\r\n"), "{}", seen.head);
    let body = seen.body.unwrap();
    assert_eq!(
        (&body["messages"][0]["content"][0]["type"], &body["top_p"]),
        (&json!("text"), &json!(0.9))
    );
}

#[test]
fn a_reply_without_markdown_fails_its_gap_and_the_batch_goes_on() {
    let (_, requests) = images(1024);
    let length =
        json!({ "choices": [{ "message": { "content": "# A" }, "finish_reason": "length" }] });
    let null = json!({ "choices": [{ "message": { "content": null }, "finish_reason": "stop" }] });
    let cap = 6 * 1024 + (64 << 10);
    let answers = vec![
        reply("<think>never closed"),
        Answer::Reply(200, "", length.to_string().into_bytes()),
        Answer::Reply(200, "", null.to_string().into_bytes()),
        reply(&"a".repeat(cap)),
        status(400, ""),
        status(413, ""),
        status(422, ""),
        reply(&"a".repeat(cap - 200)),
    ];
    let server = Server::start(answers);
    let outcomes = engine(&server.base_url, |_| {})
        .recognize(&[&requests[..4], &requests[..4]].concat(), &CancelToken::new());
    let mut expected: Vec<_> =
        [200, 200, 200, 200, 400, 413, 422].map(|code| failed(Some(code))).into();
    expected.push(filled(&"a".repeat(cap - 200)));
    assert_eq!(outcomes, expected);
    assert_eq!(server.finish().len(), 8);
}

#[test]
fn a_status_sent_before_a_large_body_is_read_still_reaches_its_gap() {
    let converter = configured(|config| config.image_ocr = ImageOcr::All);
    let large = noisy_picture(8 << 20);
    let mut parsed = converter.parse(Input::new(&large, Format::Docx)).unwrap();
    let [request] = parsed.ocr_requests(&RasterSpec::default(), None).unwrap().try_into().unwrap();
    let server = Server::start(vec![Answer::Early(413)]);
    assert_eq!(one(&engine(&server.base_url, |_| {}), &request), failed(Some(413)));
    let seen = server.finish();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].head.to_ascii_lowercase().contains("expect: 100-continue"), "{}", seen[0].head);
}

#[test]
fn a_long_reply_fills_under_a_raised_limit() {
    let (mut parsed, requests) = images(2 << 20);
    let long = "word ".repeat(300_000);
    let server = Server::start(vec![reply(&long)]);
    assert_eq!(one(&engine(&server.base_url, |_| {}), &requests[0]), filled(&long));
    parsed.fill_gap(requests[0].gap, GapFill::Markdown(long)).unwrap();
    server.finish();
}

#[test]
fn each_status_the_server_cannot_take_yet_is_retried() {
    let (_, requests) = images(1 << 20);
    for code in [408, 429, 500, 502, 503, 504] {
        let server = Server::start(vec![status(code, "retry-after: 0\r\n"), reply("x")]);
        assert_eq!(one(&engine(&server.base_url, |_| {}), &requests[0]), filled("x"), "{code}");
        assert_eq!(server.finish().len(), 2, "{code}");
    }
    for code in [501, 505] {
        let server = Server::start(vec![status(code, ""), reply("x")]);
        assert_eq!(one(&engine(&server.base_url, |_| {}), &requests[0]), unattempted(Some(code)));
        assert_eq!(server.finish().len(), 1, "{code}");
    }
}

#[test]
fn a_retry_waits_its_backoff_or_retry_after_and_stops_at_its_count() {
    let (_, requests) = images(1 << 20);
    let server = Server::start(vec![status(503, ""), Answer::Close, reply("third")]);
    let start = Instant::now();
    assert_eq!(one(&engine(&server.base_url, |_| {}), &requests[0]), filled("third"));
    assert!(start.elapsed() >= Duration::from_millis(1500), "{:?}", start.elapsed());
    assert_eq!(server.finish().len(), 3);

    let server = Server::start(vec![status(429, "retry-after: 1\r\n"), reply("later")]);
    let start = Instant::now();
    assert_eq!(one(&engine(&server.base_url, |_| {}), &requests[0]), filled("later"));
    assert!(start.elapsed() >= Duration::from_secs(1));
    server.finish();

    let server =
        Server::start(vec![status(502, "retry-after: 0\r\n"), status(504, ""), reply("x")]);
    let once = engine(&server.base_url, |config| config.retries = 1);
    assert_eq!(one(&once, &requests[0]), unattempted(Some(504)));
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn images_outside_a_parse_read_under_their_own_text_limits() {
    let (_, requests) = images(1 << 20);
    let image = OcrImage::new(requests[0].image.bytes.to_vec(), None).unwrap();
    assert_eq!(image, requests[0].image);
    let jpeg = OcrImage::new(*b"\xff\xd8\xff\xe0", Some(150)).unwrap();
    assert_eq!((jpeg.media_type, jpeg.dpi), ("image/jpeg", Some(150)));
    assert_eq!(OcrImage::new(*b"GIF89a", None), None);
    let long = "a".repeat(6 * 1024 + (64 << 10));
    let server = Server::start(vec![reply(&long), reply(&long), Answer::Close]);
    let images = [(1 << 20), 1024, 1024, 1024].map(|max| (image.clone(), max));
    let outcomes = engine(&server.base_url, |config| config.retries = 0)
        .recognize_images(&images, &CancelToken::new());
    assert_eq!(outcomes, [filled(&long), failed(Some(200)), unattempted(None), unattempted(None)]);
    assert_eq!(server.finish().len(), 3);
}

#[test]
fn a_redirect_or_another_status_stops_the_batch_after_one_attempt() {
    let (_, requests) = images(1 << 20);
    for code in [302, 401, 404] {
        let location = status(code, "location: http://192.0.2.1/v1\r\n");
        let server = Server::start(vec![location, reply("second")]);
        let outcomes =
            engine(&server.base_url, |_| {}).recognize(&requests[..2], &CancelToken::new());
        assert_eq!(outcomes, [unattempted(Some(code)), unattempted(None)], "{code}");
        assert_eq!(server.finish().len(), 1, "{code}");
    }
}

#[test]
fn an_unreachable_server_leaves_a_batch_of_four_unattempted_after_one_attempt() {
    let (_, requests) = images(1 << 20);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = format!("http://{}/v1", listener.local_addr().unwrap());
    drop(listener);
    let outcomes = engine(&closed, |_| {}).recognize(&requests[..4], &CancelToken::new());
    assert_eq!(outcomes, vec![unattempted(None); 4]);
}

#[test]
fn a_cancel_ends_a_wait_before_a_retry() {
    let (_, requests) = images(1 << 20);
    let server = Server::start(vec![status(503, "retry-after: 30\r\n")]);
    let engine = engine(&server.base_url, |_| {});
    let cancel = CancelToken::new();
    let start = Instant::now();
    let outcomes = thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(300));
            cancel.cancel();
        });
        engine.recognize(&requests[..2], &cancel)
    });
    assert_eq!(outcomes, [unattempted(Some(503)), unattempted(None)]);
    assert!(start.elapsed() < Duration::from_millis(400 + 100), "{:?}", start.elapsed());
    assert_eq!(server.finish().len(), 1);
}

/// A server that answers every connection with `reply("x")` after `stall`, on a thread each,
/// counting connections and the most open at once.
struct Stalling {
    base_url: String,
    counts: Arc<Mutex<(usize, usize, usize)>>,
    stop: Arc<AtomicBool>,
    serving: JoinHandle<()>,
}

/// Count `stream` in, wait `stall`, count it out and answer it.
fn stall_and_serve(stream: &TcpStream, counts: &Mutex<(usize, usize, usize)>, stall: Duration) {
    {
        let mut counts = counts.lock().unwrap();
        (counts.0, counts.1) = (counts.0 + 1, counts.1 + 1);
        counts.2 = counts.2.max(counts.1);
    }
    thread::sleep(stall);
    counts.lock().unwrap().1 -= 1;
    serve(stream, reply("x"));
}

impl Stalling {
    fn start(stall: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (counts, stop) = (Arc::new(Mutex::new((0, 0, 0))), Arc::new(AtomicBool::new(false)));
        let (shared, stopped) = (Arc::clone(&counts), Arc::clone(&stop));
        let serving = thread::spawn(move || {
            thread::scope(|scope| {
                while !stopped.load(Ordering::Relaxed) {
                    let Ok((stream, _)) = listener.accept() else {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    let shared = Arc::clone(&shared);
                    scope.spawn(move || stall_and_serve(&stream, &shared, stall));
                }
            });
        });
        Self { base_url, counts, stop, serving }
    }

    /// Connections served and the most open at once.
    fn finish(self) -> (usize, usize) {
        thread::sleep(Duration::from_millis(50));
        self.stop.store(true, Ordering::Relaxed);
        self.serving.join().unwrap();
        let (served, _, most) = *self.counts.lock().unwrap();
        (served, most)
    }
}

#[test]
fn a_batch_keeps_its_concurrency_in_flight() {
    let (_, requests) = images(1 << 20);
    let server = Stalling::start(Duration::from_millis(150));
    let three = engine(&server.base_url, |config| config.concurrency = 3);
    assert_eq!(three.batch_size(), 6);
    let outcomes = three.recognize(&requests, &CancelToken::new());
    assert_eq!(outcomes, vec![filled("x"); 6]);
    assert_eq!(server.finish(), (6, 3));
}

#[test]
fn a_cancel_during_a_stalled_response_attempts_no_later_request() {
    let (_, requests) = images(1 << 20);
    let server = Stalling::start(Duration::from_millis(400));
    let engine = engine(&server.base_url, |_| {});
    let cancel = CancelToken::new();
    let start = Instant::now();
    let outcomes = thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(100));
            cancel.cancel();
        });
        engine.recognize(&requests[..2], &cancel)
    });
    assert!(start.elapsed() >= Duration::from_millis(400), "{:?}", start.elapsed());
    assert_eq!(outcomes, [filled("x"), unattempted(None)]);
    assert_eq!(server.finish(), (1, 1));
}

/// A self-signed leaf certificate for `localhost` and 127.0.0.1, and its P-256 key, made for
/// this test alone.
const TEST_CERTIFICATE: &str = "MIIBlzCCAT6gAwIBAgIUFtBbo2vzkNbNURHK5cD7G4OGBN8wCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTAyNjE0MjQwNFoYDzIxMjYxMDAyMTQyNDA0WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAScHAB6OhX7qzE599KAWwj3OnEVtz+PJ8DJgtPPhxDcg4UOsP5zw3WcJrejnazkScXCZUheHTwCEvgZ3fTQk1yso2wwajAdBgNVHQ4EFgQU0wGBjld5606LRkEE6PRwRox8c60wHwYDVR0jBBgwFoAU0wGBjld5606LRkEE6PRwRox8c60wDAYDVR0TAQH/BAIwADAaBgNVHREEEzARhwR/AAABgglsb2NhbGhvc3QwCgYIKoZIzj0EAwIDRwAwRAIgNRqqjhzfSrkcXkmvBs683jN66QW4PbSf+xfCk89TANYCIB2OjlMrLj3RGkm6W4nM+FRjMosWkK55/GO9iln4EBmn";
const TEST_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgSTj+2rUJ2sipq927rIi6rz3ALuxB+/8pe2JmNGCgYKKhRANCAAScHAB6OhX7qzE599KAWwj3OnEVtz+PJ8DJgtPPhxDcg4UOsP5zw3WcJrejnazkScXCZUheHTwCEvgZ3fTQk1ys";

#[test]
fn a_server_whose_certificate_no_root_signs_leaves_its_gap_after_one_connection() {
    use base64::Engine as _;
    use rustls::pki_types::{CertificateDer, ServerName};
    let decode = |text| base64::engine::general_purpose::STANDARD.decode(text).unwrap();
    let certificate = CertificateDer::from(decode(TEST_CERTIFICATE));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = tls_config();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate).unwrap();
    let trusting = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let requests = images(1 << 20).1;
    let (stop, deadline) =
        (Arc::new(AtomicBool::new(false)), Instant::now() + Duration::from_secs(10));
    let stopped = Arc::clone(&stop);
    let serving = thread::spawn(move || {
        let mut handshakes = Vec::new();
        while !stopped.load(Ordering::Relaxed) && Instant::now() < deadline {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut connection = rustls::ServerConnection::new(Arc::clone(&tls)).unwrap();
            handshakes.push(connection.complete_io(&mut stream).is_ok());
        }
        handshakes
    });
    let name = ServerName::try_from("127.0.0.1").unwrap();
    let mut client = rustls::ClientConnection::new(Arc::new(trusting), name).unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    while client.is_handshaking() {
        client.complete_io(&mut stream).unwrap();
    }
    drop(stream);
    let engine = engine(&format!("https://{address}/v1"), |config| config.retries = 1);
    assert_eq!(one(&engine, &requests[0]), unattempted(None));
    stop.store(true, Ordering::Relaxed);
    assert_eq!(serving.join().unwrap(), [true, false]);
}

/// Set by the parent of the child run of
/// `a_proxy_in_the_environment_changes_no_outcome`.
const PROXIED_CHILD: &str = "ALMILADOC_PROXIED_CHILD";

#[test]
fn a_proxy_in_the_environment_changes_no_outcome() {
    if std::env::var(PROXIED_CHILD).as_deref() == Ok("explicit") {
        caller_roots_and_both_connect_proxy_schemes_are_verified();
        return;
    }
    if std::env::var_os(PROXIED_CHILD).is_some() {
        let (_, requests) = images(1 << 20);
        let server = Server::start(vec![reply("direct")]);
        assert_eq!(one(&engine(&server.base_url, |_| {}), &requests[0]), filled("direct"));
        assert_eq!(server.finish().len(), 1);
        return;
    }
    let dead = "http://127.0.0.1:1";
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "ocr_http::a_proxy_in_the_environment_changes_no_outcome"])
        .env(PROXIED_CHILD, "1")
        .envs(
            ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"]
                .map(|name| (name, dead)),
        )
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .unwrap();
    let printed = String::from_utf8_lossy(&child.stdout);
    assert!(child.status.success() && printed.contains("1 passed"), "{printed}");
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "ocr_http::a_proxy_in_the_environment_changes_no_outcome"])
        .env(PROXIED_CHILD, "explicit")
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
        .output()
        .unwrap();
    let printed = String::from_utf8_lossy(&child.stdout);
    assert!(child.status.success() && printed.contains("1 passed"), "{printed}");
}

fn pem(der: &[u8]) -> Vec<u8> {
    use base64::Engine as _;
    format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        base64::engine::general_purpose::STANDARD.encode(der)
    )
    .into_bytes()
}

fn test_certificate() -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(TEST_CERTIFICATE).unwrap()
}

fn wrong_certificate() -> Vec<u8> {
    let mut der = test_certificate();
    for at in 0..der.len().saturating_sub(8) {
        if &der[at..at + 9] == b"localhost" {
            der[at..at + 9].copy_from_slice(b"otherhost");
        }
    }
    der
}

fn tls_config() -> Arc<rustls::ServerConfig> {
    use base64::Engine as _;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let key = base64::engine::general_purpose::STANDARD.decode(TEST_KEY).unwrap();
    Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(test_certificate())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )
        .unwrap(),
    )
}

trait Channel: Read + Write + Send {}
impl<T: Read + Write + Send> Channel for T {}

fn encrypted(mut stream: Box<dyn Channel>) -> Option<Box<dyn Channel>> {
    let mut tls = rustls::ServerConnection::new(tls_config()).unwrap();
    while tls.is_handshaking() {
        tls.complete_io(&mut stream).ok()?;
    }
    Some(Box::new(rustls::StreamOwned::new(tls, stream)))
}

#[derive(Clone, Copy)]
enum Route {
    Direct,
    Http(u16),
    Https(u16),
}

struct TlsPeer {
    base_url: String,
    proxy: Option<String>,
    serving: JoinHandle<Option<(String, Option<Seen>)>>,
}

impl TlsPeer {
    fn start(route: Route) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let proxy = match route {
            Route::Direct => None,
            Route::Http(_) => Some(format!("http://user:pass@{address}")),
            Route::Https(_) => Some(format!("https://user:pass@{address}")),
        };
        let base_url = if proxy.is_none() {
            format!("https://{address}/v1")
        } else {
            format!("https://{}:443/v1", address.ip())
        };
        let serving = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let stream = loop {
                if let Ok((stream, _)) = listener.accept() {
                    break stream;
                }
                if Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(5));
            };
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut transport: Box<dyn Channel> = Box::new(stream);
            if matches!(route, Route::Https(_)) {
                transport = encrypted(transport)?;
            }
            let mut proxy_head = String::new();
            if let Route::Http(code) | Route::Https(code) = route {
                let mut reader = BufReader::new(&mut transport);
                proxy_head = read_head(&mut reader).ok()?;
                write!(reader.get_mut(), "HTTP/1.1 {code} Proxy\r\ncontent-length: 0\r\n\r\n")
                    .ok()?;
                reader.get_mut().flush().ok()?;
                if code != 200 {
                    return Some((proxy_head, None));
                }
            }
            let mut transport = encrypted(transport)?;
            Some((proxy_head, Some(serve_on(&mut transport, reply("private")))))
        });
        Self { base_url, proxy, serving }
    }
}

#[test]
fn caller_roots_and_both_connect_proxy_schemes_are_verified() {
    let (_, requests) = images(1024);
    for route in [Route::Direct, Route::Http(200), Route::Https(200)] {
        let peer = TlsPeer::start(route);
        let bundle = [pem(&wrong_certificate()), pem(&test_certificate())].concat();
        let engine = engine(&peer.base_url, |config| {
            config.root_certificates_pem = Some(bundle.clone());
            config.proxy.clone_from(&peer.proxy);
            config.api_key = Some("sk-local".into());
            config.retries = 0;
        });
        assert_eq!(engine.root_certificates_pem(), Some(bundle.as_slice()));
        assert_eq!(engine.proxy(), peer.proxy.as_deref());
        let shown = format!("{engine:?}");
        assert!(!shown.contains("user") && !shown.contains("pass") && !shown.contains("sk-local"));
        assert_eq!(one(&engine, &requests[0]), filled("private"));
        let (proxy, seen) = peer.serving.join().unwrap().unwrap();
        let seen = seen.unwrap();
        assert!(!seen.head.to_ascii_lowercase().contains("proxy-authorization"));
        assert!(seen.head.to_ascii_lowercase().contains("authorization: bearer sk-local"));
        if !matches!(route, Route::Direct) {
            assert!(proxy.starts_with(&format!(
                "CONNECT {}:443 HTTP/1.1\r\n",
                std::net::Ipv4Addr::LOCALHOST
            )));
            assert!(proxy.contains("Proxy-Authorization: Basic dXNlcjpwYXNz"));
        }
    }
}

#[test]
fn a_wrong_anchor_hostname_or_proxy_refusal_leaves_the_request_unattempted() {
    let (_, requests) = images(1024);
    for (route, wrong_root, wrong_name) in [
        (Route::Direct, true, false),
        (Route::Https(200), true, false),
        (Route::Http(200), false, true),
        (Route::Http(407), false, false),
    ] {
        let peer = TlsPeer::start(route);
        let base = if wrong_name { "https://untrusted.invalid/v1" } else { &peer.base_url };
        let engine = engine(base, |config| {
            config.proxy.clone_from(&peer.proxy);
            config.root_certificates_pem =
                Some(pem(&if wrong_root { wrong_certificate() } else { test_certificate() }));
            config.retries = 0;
        });
        assert_eq!(one(&engine, &requests[0]), unattempted(None));
        let seen = peer.serving.join().unwrap();
        assert!(seen.is_none_or(|(_, http)| http.is_none()));
    }
}

#[test]
fn the_entire_certificate_bundle_is_refused_before_any_socket_opens() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("https://{}/v1", listener.local_addr().unwrap());
    let valid = pem(&test_certificate());
    let invalid = pem(&[0x30, 0x00]);
    let mut oversized = valid.clone();
    oversized.resize(OcrHttpConfig::MAX_ROOT_PEM_BYTES + 1, b' ');
    let mut trailing_der = test_certificate();
    trailing_der.push(0);
    for bundle in [
        Vec::new(),
        b"private junk".to_vec(),
        b" ".repeat(2 << 20),
        oversized,
        b"-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----".to_vec(),
        invalid.clone(),
        [valid.clone(), invalid].concat(),
        [valid.clone(), pem(&trailing_der)].concat(),
        [valid.clone(), b"trailing junk".to_vec()].concat(),
        valid.repeat(257),
    ] {
        let mut config = OcrHttpConfig::new(&base, "vision");
        config.root_certificates_pem = Some(bundle);
        assert!(matches!(
            OcrHttp::new(config),
            Err(almiladoc::ConfigError::OcrHttp { field: "root_certificates_pem", .. })
        ));
    }
    let mut padded = valid.clone();
    padded.resize(OcrHttpConfig::MAX_ROOT_PEM_BYTES, b' ');
    for bundle in [valid.clone(), valid.repeat(256), padded] {
        let mut config = OcrHttpConfig::new(&base, "vision");
        config.root_certificates_pem = Some(bundle.clone());
        let engine = OcrHttp::new(config).unwrap();
        assert_eq!(engine.root_certificates_pem(), Some(bundle.as_slice()));
    }
    assert!(matches!(listener.accept(), Err(error) if error.kind() == ErrorKind::WouldBlock));
}
