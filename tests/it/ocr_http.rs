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
use crate::support::{all_requests, configured, fixture};
use almiladoc::{
    CancelToken, Format, GapFill, ImageDetail, ImageOcr, Input, MessageLayout, OcrEngine,
    OcrFailure, OcrFailureKind, OcrHttp, OcrHttpConfig, OcrImage, OcrOutcome, OcrReport,
    OcrRequest, OnLoss, Parsed, RasterSpec, RetryAfter, TierFallback, TokenLimitField,
};
use serde_json::{Value, json};

enum Answer {
    Reply(u16, &'static str, Vec<u8>),
    Close,
    Early(u16),
    Hang(Vec<u8>),
    Cancel(CancelToken, Box<Self>),
}

fn reply(content: &str) -> Answer {
    let body =
        json!({ "choices": [{ "message": { "content": content }, "finish_reason": "stop" }] });
    Answer::Reply(200, "", body.to_string().into_bytes())
}

fn status(code: u16, headers: &'static str) -> Answer {
    Answer::Reply(code, headers, Vec::new())
}

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
    respond(&mut reader, answer);
    Seen { head, body: Some(serde_json::from_slice(&body).unwrap()) }
}

fn respond(reader: &mut BufReader<impl Read + Write>, answer: Answer) {
    match answer {
        Answer::Reply(code, headers, body) => {
            let head = format!(
                "HTTP/1.1 {code} Scripted\r\ncontent-length: {}\r\nconnection: close\r\n{headers}\r\n",
                body.len()
            );
            reader.get_mut().write_all(head.as_bytes()).unwrap();
            reader.get_mut().write_all(&body).unwrap();
            reader.get_mut().flush().unwrap();
        }
        Answer::Hang(raw) => {
            reader.get_mut().write_all(&raw).unwrap();
            reader.get_mut().flush().unwrap();
            let _closed = reader.read(&mut [0; 1]);
        }
        Answer::Cancel(cancel, answer) => {
            cancel.cancel();
            respond(reader, *answer);
        }
        Answer::Close | Answer::Early(_) => {}
    }
}

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

fn images(max_ocr_text_bytes: u64) -> (Parsed, Vec<OcrRequest>) {
    let converter = configured(|config| {
        config.image_ocr = ImageOcr::All;
        config.on_loss = OnLoss::Report;
        config.limits.max_ocr_text_bytes = max_ocr_text_bytes;
    });
    let mut parsed =
        converter.parse(Input::new(&fixture("docx/handmade-rich.docx"), Format::Docx)).unwrap();
    let [request] = all_requests(&mut parsed, &RasterSpec::default()).try_into().unwrap();
    (parsed, vec![request; 6])
}

fn one(engine: &OcrHttp, request: &OcrRequest) -> OcrOutcome {
    let [outcome] =
        engine.recognize(std::slice::from_ref(request), &CancelToken::new()).try_into().unwrap();
    outcome
}

fn report(engine: &OcrHttp, request: &OcrRequest) -> OcrReport {
    let image = (request.image.clone(), request.max_text_bytes);
    let [report] = engine.recognize_images(&[image], &CancelToken::new()).try_into().unwrap();
    report
}

fn failed(kind: OcrFailureKind, status: Option<u16>) -> OcrOutcome {
    OcrOutcome::Failed(OcrFailure::new(kind, status))
}

fn unattempted(kind: OcrFailureKind, status: Option<u16>) -> OcrOutcome {
    OcrOutcome::Unattempted(OcrFailure::new(kind, status))
}

fn waiting(kind: OcrFailureKind, status: Option<u16>, after: RetryAfter) -> OcrOutcome {
    OcrOutcome::Unattempted(OcrFailure::new(kind, status).with_retry_after(Some(after)))
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
        config.layout = MessageLayout::TextThenImage;
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

fn answer(reply: &Value) -> Answer {
    Answer::Reply(200, "", reply.to_string().into_bytes())
}

fn finished(content: &Value, finish: &str) -> Value {
    json!({ "choices": [{ "message": { "content": content }, "finish_reason": finish }] })
}

#[test]
fn only_a_verdict_on_the_image_fails_its_gap_and_the_batch_goes_on() {
    let (_, requests) = images(1024);
    let refusal =
        json!({ "choices": [{ "message": { "refusal": "no" }, "finish_reason": "stop" }] });
    let cap = 6 * 1024 + (64 << 10);
    let answers = vec![
        answer(&finished(&json!("# A"), "length")),
        answer(&finished(&json!("# A"), "content_filter")),
        answer(&refusal),
        reply(&"a".repeat(cap)),
        status(413, ""),
        reply(&"a".repeat(cap - 200)),
    ];
    let server = Server::start(answers);
    let outcomes = engine(&server.base_url, |_| {}).recognize(&requests, &CancelToken::new());
    assert_eq!(
        outcomes,
        [
            failed(OcrFailureKind::Unfinished, Some(200)),
            failed(OcrFailureKind::Refused, Some(200)),
            failed(OcrFailureKind::Refused, Some(200)),
            failed(OcrFailureKind::TooLarge, Some(200)),
            failed(OcrFailureKind::TooLarge, Some(413)),
            filled(&"a".repeat(cap - 200)),
        ]
    );
    assert_eq!(server.finish().len(), 6);
}

#[test]
fn a_rejected_request_or_a_reply_without_text_leaves_its_gap_unattempted() {
    let (mut parsed, requests) = images(1 << 20);
    for (answer, kind, code) in [
        (status(400, ""), OcrFailureKind::Rejected, 400),
        (status(422, ""), OcrFailureKind::Rejected, 422),
        (reply("<think>never closed"), OcrFailureKind::NoText, 200),
        (answer(&finished(&Value::Null, "stop")), OcrFailureKind::NoText, 200),
        (Answer::Reply(200, "", b"not json".to_vec()), OcrFailureKind::Protocol, 200),
    ] {
        let server = Server::start(vec![answer, reply("unsent")]);
        let outcomes =
            engine(&server.base_url, |_| {}).recognize(&requests[..2], &CancelToken::new());
        assert_eq!(outcomes, [unattempted(kind, Some(code)), unattempted(kind, None)]);
        assert_eq!(server.finish().len(), 1);
    }
    let server = Server::start(vec![status(400, "")]);
    let summary = parsed.ocr(&engine(&server.base_url, |_| {}), None).unwrap();
    assert_eq!((summary.filled, summary.failed, summary.unattempted), (0, 0, 1));
    parsed.fill_gap(requests[0].gap, GapFill::Markdown("later".into())).unwrap();
    server.finish();
}

#[test]
fn a_status_sent_before_a_large_body_is_read_still_reaches_its_gap() {
    let converter = configured(|config| config.image_ocr = ImageOcr::All);
    let large = noisy_picture(8 << 20);
    let mut parsed = converter.parse(Input::new(&large, Format::Docx)).unwrap();
    let [request] = all_requests(&mut parsed, &RasterSpec::default()).try_into().unwrap();
    let server = Server::start(vec![Answer::Early(413)]);
    assert_eq!(
        one(&engine(&server.base_url, |_| {}), &request),
        failed(OcrFailureKind::TooLarge, Some(413))
    );
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
        let busy = unattempted(OcrFailureKind::Busy, Some(code));
        assert_eq!(one(&engine(&server.base_url, |_| {}), &requests[0]), busy);
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

    for code in [408, 429] {
        let server = Server::start(vec![status(code, "retry-after: 1\r\n"), reply("later")]);
        let start = Instant::now();
        assert_eq!(one(&engine(&server.base_url, |_| {}), &requests[0]), filled("later"));
        assert!(start.elapsed() >= Duration::from_secs(1), "{code}: {:?}", start.elapsed());
        server.finish();
    }

    let server =
        Server::start(vec![status(502, "retry-after: 0\r\n"), status(504, ""), reply("x")]);
    let once = engine(&server.base_url, |config| config.retries = 1);
    let busy = waiting(OcrFailureKind::Busy, Some(504), RetryAfter::Seconds(0));
    assert_eq!(one(&once, &requests[0]), busy);
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
    let reports = engine(&server.base_url, |config| config.retries = 0)
        .recognize_images(&images, &CancelToken::new());
    let outcomes: Vec<_> = reports.into_iter().map(|report| report.outcome).collect();
    let unreachable = unattempted(OcrFailureKind::Unreachable, None);
    let too_large = failed(OcrFailureKind::TooLarge, Some(200));
    assert_eq!(outcomes, [filled(&long), too_large, unreachable.clone(), unreachable]);
    assert_eq!(server.finish().len(), 3);
}

#[test]
fn a_redirect_or_another_status_stops_the_batch_after_one_attempt() {
    let (_, requests) = images(1 << 20);
    for (code, kind) in [
        (302, OcrFailureKind::Protocol),
        (401, OcrFailureKind::Rejected),
        (404, OcrFailureKind::Rejected),
    ] {
        let location = status(code, "location: http://192.0.2.1/v1\r\n");
        let server = Server::start(vec![location, reply("second")]);
        let outcomes =
            engine(&server.base_url, |_| {}).recognize(&requests[..2], &CancelToken::new());
        assert_eq!(outcomes, [unattempted(kind, Some(code)), unattempted(kind, None)], "{code}");
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
    assert_eq!(outcomes, vec![unattempted(OcrFailureKind::Unreachable, None); 4]);
}

fn cancelled_after(engine: &OcrHttp, requests: &[OcrRequest], after: Duration) -> Vec<OcrOutcome> {
    let cancel = CancelToken::new();
    thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(after);
            cancel.cancel();
        });
        engine.recognize(requests, &cancel)
    })
}

#[test]
fn a_cancel_ends_a_wait_before_a_retry() {
    let (_, requests) = images(1 << 20);
    let server = Server::start(vec![status(503, "retry-after: 30\r\n")]);
    let engine = engine(&server.base_url, |_| {});
    let start = Instant::now();
    let outcomes = cancelled_after(&engine, &requests[..2], Duration::from_millis(300));
    let cancelled = waiting(OcrFailureKind::Cancelled, Some(503), RetryAfter::Seconds(30));
    assert_eq!(outcomes, [cancelled, unattempted(OcrFailureKind::Cancelled, None)]);
    assert!(start.elapsed() < Duration::from_millis(400 + 100), "{:?}", start.elapsed());
    assert_eq!(server.finish().len(), 1);
}

struct Stalling {
    base_url: String,
    counts: Arc<Mutex<(usize, usize, usize)>>,
    stop: Arc<AtomicBool>,
    serving: JoinHandle<()>,
}

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
    assert_eq!(one(&engine, &requests[0]), unattempted(OcrFailureKind::Unreachable, None));
    stop.store(true, Ordering::Relaxed);
    assert_eq!(serving.join().unwrap(), [true, false]);
}

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
    fn start(route: Route, answer: Answer) -> Self {
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
            Some((proxy_head, Some(serve_on(&mut transport, answer))))
        });
        Self { base_url, proxy, serving }
    }
}

#[test]
fn caller_roots_and_both_connect_proxy_schemes_are_verified() {
    let (_, requests) = images(1024);
    for route in [Route::Direct, Route::Http(200), Route::Https(200)] {
        let peer = TlsPeer::start(route, reply("private"));
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
        let peer = TlsPeer::start(route, reply("private"));
        let base = if wrong_name { "https://untrusted.invalid/v1" } else { &peer.base_url };
        let engine = engine(base, |config| {
            config.proxy.clone_from(&peer.proxy);
            config.root_certificates_pem =
                Some(pem(&if wrong_root { wrong_certificate() } else { test_certificate() }));
            config.retries = 0;
        });
        assert_eq!(one(&engine, &requests[0]), unattempted(OcrFailureKind::Unreachable, None));
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

fn usage(report: &OcrReport) -> Option<(u64, u64, u64)> {
    report.usage.map(|usage| (usage.input_uncached, usage.input_cached, usage.output))
}

fn trace(report: &OcrReport) -> Vec<(Option<u8>, Option<u16>)> {
    report.attempts.iter().map(|attempt| (attempt.tier, attempt.status)).collect()
}

#[test]
fn a_body_takes_its_layout_token_field_temperature_detail_and_tier() {
    let (_, requests) = images(1 << 20);
    let server = Server::start(vec![reply("x"), reply("y")]);
    let openai = engine(&server.base_url, |config| {
        config.layout = MessageLayout::DeveloperThenImage;
        config.token_limit_field = TokenLimitField::MaxCompletionTokens;
        config.temperature = None;
        config.image_detail = Some(ImageDetail::High);
        config.service_tiers = vec!["flex".into(), "default".into()];
        config.prompt = "Read.".into();
    });
    assert_eq!(one(&openai, &requests[0]), filled("x"));
    let system = engine(&server.base_url, |config| config.layout = MessageLayout::SystemThenImage);
    assert_eq!(one(&system, &requests[0]), filled("y"));
    let [openai, system] = server.finish().try_into().ok().unwrap();
    let body = openai.body.unwrap();
    assert_eq!(
        (&body["max_completion_tokens"], body.get("max_tokens"), body.get("temperature")),
        (&json!(8192), None, None)
    );
    assert_eq!((&body["service_tier"], &body["model"]), (&json!("flex"), &json!("vision")));
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0], json!({ "role": "developer", "content": "Read." }));
    assert_eq!((&messages[1]["role"], messages.len()), (&json!("user"), 2));
    let [image] = messages[1]["content"].as_array().unwrap().clone().try_into().unwrap();
    assert_eq!(image["image_url"]["detail"], json!("high"));
    assert!(image["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));
    let body = system.body.unwrap();
    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!((&body["temperature"], body.get("service_tier")), (&json!(0.0), None));
    assert_eq!(body["messages"][1]["content"][0]["image_url"].get("detail"), None);
}

#[test]
fn a_reply_reports_its_usage_and_tier_and_a_wrong_model_leaves_its_gap() {
    let (_, requests) = images(1 << 20);
    let mut billed = finished(&json!("# A"), "length");
    billed["model"] = json!("vision-2");
    billed["service_tier"] = json!("flex");
    billed["usage"] = json!({
        "prompt_tokens": 10,
        "completion_tokens": 7,
        "prompt_tokens_details": { "cached_tokens": 4 }
    });
    let mut other = finished(&json!("# A"), "stop");
    other["model"] = json!("vision-1");
    other["service_tier"] = json!("priority");
    other["usage"] = billed["usage"].clone();
    let mut unnamed = finished(&json!("# A"), "stop");
    unnamed["usage"] = json!({ "prompt_tokens": 3, "completion_tokens": true });
    let server = Server::start(vec![answer(&billed), answer(&other), answer(&unnamed)]);
    let checked = engine(&server.base_url, |config| {
        config.answer_model = Some("vision-2".into());
        config.service_tiers = vec!["default".into(), "flex".into()];
    });
    let unfinished = report(&checked, &requests[0]);
    assert_eq!(unfinished.outcome, failed(OcrFailureKind::Unfinished, Some(200)));
    assert_eq!((usage(&unfinished), unfinished.realized_tier), (Some((6, 4, 7)), Some(1)));
    assert_eq!(trace(&unfinished), [(Some(0), Some(200))]);
    let wrong = report(&checked, &requests[0]);
    assert_eq!(wrong.outcome, unattempted(OcrFailureKind::WrongModel, Some(200)));
    assert_eq!((usage(&wrong), wrong.realized_tier), (Some((6, 4, 7)), None));
    let missing = report(&checked, &requests[0]);
    assert_eq!(missing.outcome, unattempted(OcrFailureKind::WrongModel, Some(200)));
    assert_eq!(usage(&missing), None);
    server.finish();
}

#[test]
fn retry_after_is_reported_uncapped_as_seconds_or_a_date_and_the_longest_kept() {
    let (_, requests) = images(1 << 20);
    let date = std::time::UNIX_EPOCH + Duration::from_mins(29_892_128);
    for (code, header, after) in [
        (408, "retry-after: 90\r\n", RetryAfter::Seconds(90)),
        (429, "retry-after: 120\r\n", RetryAfter::Seconds(120)),
        (503, "retry-after: Sun, 01 Nov 2026 10:08:00 GMT\r\n", RetryAfter::At(date)),
        (501, "retry-after: 7\r\n", RetryAfter::Seconds(7)),
    ] {
        let server = Server::start(vec![status(code, header)]);
        let once = engine(&server.base_url, |config| config.retries = 0);
        assert_eq!(one(&once, &requests[0]), waiting(OcrFailureKind::Busy, Some(code), after));
        server.finish();
    }
    let server = Server::start(vec![
        status(503, "retry-after: 1\r\n"),
        status(500, "retry-after: 0\r\n"),
        status(404, "retry-after: 99\r\n"),
    ]);
    let twice = engine(&server.base_url, |config| config.retries = 2);
    let start = Instant::now();
    let report = report(&twice, &requests[0]);
    assert!(start.elapsed() >= Duration::from_secs(1), "{:?}", start.elapsed());
    let rejected = waiting(OcrFailureKind::Rejected, Some(404), RetryAfter::Seconds(1));
    assert_eq!(report.outcome, rejected);
    assert_eq!(trace(&report), [(None, Some(503)), (None, Some(500)), (None, Some(404))]);
    server.finish();
}

fn flex_busy(message: &str) -> Answer {
    let body = json!({ "error": { "message": message, "type": "rate_limit", "code": null } });
    Answer::Reply(429, "retry-after: 5\r\n", body.to_string().into_bytes())
}

fn flex_once(config: &mut OcrHttpConfig) {
    config.retries = 0;
    config.service_tiers = vec!["flex".into(), "default".into()];
    config.tier_fallback = Some(TierFallback::new(Vec::new(), "Flex processing"));
}

#[test]
fn a_429_whose_error_names_the_fallback_moves_once_to_the_next_tier() {
    let (_, requests) = images(1 << 20);
    let secret = "Flex processing is temporarily unavailable for account org-secret";
    let mut served = finished(&json!("# Default"), "stop");
    served["service_tier"] = json!("default");
    let server = Server::start(vec![flex_busy(secret), answer(&served), flex_busy("Too many")]);
    let flex = engine(&server.base_url, flex_once);
    let moved = report(&flex, &requests[0]);
    assert_eq!((moved.outcome.clone(), moved.realized_tier), (filled("# Default"), Some(1)));
    assert_eq!(trace(&moved), [(Some(0), Some(429)), (Some(1), Some(200))]);
    let busy = report(&flex, &requests[0]);
    let after = RetryAfter::Seconds(5);
    assert_eq!(busy.outcome, waiting(OcrFailureKind::Busy, Some(429), after));
    assert_eq!(trace(&busy), [(Some(0), Some(429))]);
    let shown = format!("{moved:?} {busy:?} {flex:?}");
    for hidden in ["org-secret", "Flex processing", "Too many", "# Default"] {
        assert!(!shown.contains(hidden), "{hidden}: {shown}");
    }
    let seen = server.finish();
    let tiers: Vec<_> =
        seen.iter().map(|seen| seen.body.as_ref().unwrap()["service_tier"].clone()).collect();
    assert_eq!(tiers, [json!("flex"), json!("default"), json!("flex")]);
}

#[test]
fn a_cancel_while_a_429_error_is_read_leaves_its_gap_cancelled() {
    let (_, requests) = images(1 << 20);
    let head = "HTTP/1.1 429 Busy\r\nretry-after: 5\r\ncontent-length: 400\r\n\r\n";
    let stalled = format!(r#"{head}{{"error": {{"message": "Flex"#);
    let server = Server::start(vec![Answer::Hang(stalled.into_bytes())]);
    let start = Instant::now();
    let outcomes = cancelled_after(
        &engine(&server.base_url, flex_once),
        &requests[..2],
        Duration::from_millis(200),
    );
    assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
    let cancelled = waiting(OcrFailureKind::Cancelled, Some(429), RetryAfter::Seconds(5));
    assert_eq!(outcomes, [cancelled, unattempted(OcrFailureKind::Cancelled, None)]);
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn a_cancel_before_an_error_status_arrives_leaves_its_gap_cancelled() {
    let (_, requests) = images(1 << 20);
    for (code, headers, after) in
        [(429, "retry-after: 5\r\n", Some(RetryAfter::Seconds(5))), (413, "", None)]
    {
        let cancel = CancelToken::new();
        let answer = Answer::Cancel(cancel.clone(), Box::new(status(code, headers)));
        let server = Server::start(vec![answer]);
        let once = engine(&server.base_url, |config| config.retries = 0);
        let outcomes = once.recognize(&requests[..2], &cancel);
        let cancelled = |status, after| {
            let failure = OcrFailure::new(OcrFailureKind::Cancelled, status);
            OcrOutcome::Unattempted(failure.with_retry_after(after))
        };
        let either_race = [cancelled(Some(code), after), cancelled(None, None)];
        assert!(either_race.contains(&outcomes[0]), "{code}: {outcomes:?}");
        assert_eq!(outcomes[1], cancelled(None, None), "{code}");
        assert_eq!(server.finish().len(), 1, "{code}");
    }
}

#[test]
fn a_fill_keeps_its_table_spans_when_asked() {
    let (mut parsed, requests) = images(1 << 20);
    let table = "<table><tr><td colspan=\"2\">A</td></tr></table>";
    let server = Server::start(vec![reply(table)]);
    let tables = engine(&server.base_url, |config| config.preserve_table_spans = true);
    let fill = GapFill::MarkdownTables(table.into());
    assert_eq!(one(&tables, &requests[0]), OcrOutcome::Fill(fill.clone()));
    parsed.fill_gap(requests[0].gap, fill).unwrap();
    assert!(parsed.render().unwrap().markdown().contains("colspan=\"2\""));
    server.finish();
}

#[test]
fn a_reply_written_as_otsl_fills_a_table_whatever_the_settings() {
    let otsl = "<fcel>Sample<lcel><nl><fcel>A<fcel>12.4<nl>";
    for (preserve, table) in [
        (false, "|  |  |\n| --- | --- |\n| Sample |  |\n| A | 12.4 |"),
        (
            true,
            "<table><tr><td colspan=\"2\">Sample</td></tr><tr><td>A</td><td>12.4</td></tr></table>",
        ),
    ] {
        let (mut parsed, _) = images(1 << 20);
        let server = Server::start(vec![reply(otsl)]);
        let engine = engine(&server.base_url, |config| config.preserve_table_spans = preserve);
        let summary = parsed.ocr(&engine, None).unwrap();
        assert_eq!((summary.filled, summary.failed, summary.unattempted), (1, 0, 0));
        let markdown = parsed.render().unwrap().into_markdown();
        assert!(markdown.contains(table), "{markdown}");
        assert!(!markdown.contains("cel>") && !markdown.contains("<nl>"), "{markdown}");
        #[cfg(feature = "serde")]
        assert!(parsed.to_json().unwrap().contains("\"col_span\":2"));
        server.finish();
    }
}

#[test]
fn a_cancel_interrupts_a_request_in_flight_and_the_engine_reads_again() {
    let (_, requests) = images(1 << 20);
    let whole = finished(&json!("cut"), "stop");
    for (raw, code) in [
        (String::new(), None),
        ("HTTP/1.1 200 OK\r\ncontent-length: 500\r\n\r\n{".to_owned(), Some(200)),
        (format!("HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n{whole}"), Some(200)),
    ] {
        let server = Server::start(vec![Answer::Hang(raw.into_bytes()), reply("again")]);
        let engine = engine(&server.base_url, |_| {});
        let start = Instant::now();
        let outcomes = cancelled_after(&engine, &requests[..2], Duration::from_millis(200));
        assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
        let cancelled = |code| unattempted(OcrFailureKind::Cancelled, code);
        assert_eq!(outcomes, [cancelled(code), cancelled(None)], "{code:?}");
        assert_eq!(one(&engine, &requests[0]), filled("again"));
        assert_eq!(server.finish().len(), 2);
    }
}

#[test]
fn a_cancel_interrupts_a_request_in_flight_over_tls() {
    let (_, requests) = images(1024);
    let peer = TlsPeer::start(Route::Direct, Answer::Hang(Vec::new()));
    let engine = engine(&peer.base_url, |config| {
        config.root_certificates_pem = Some(pem(&test_certificate()));
    });
    let start = Instant::now();
    let outcomes = cancelled_after(&engine, &requests[..1], Duration::from_millis(200));
    assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
    assert_eq!(outcomes, [unattempted(OcrFailureKind::Cancelled, None)]);
    assert!(peer.serving.join().unwrap().is_some_and(|(_, seen)| seen.is_some()));
}

#[test]
fn python_quotes_the_default_prompt() {
    let quoted = format!("\"{}\"", OcrHttpConfig::DEFAULT_PROMPT);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for path in ["python/src/ocr.rs", "python/almiladoc/_almiladoc.pyi"] {
        let text = std::fs::read_to_string(root.join(path)).unwrap();
        let words: Vec<_> = text.split_whitespace().filter(|word| *word != "///").collect();
        assert!(words.join(" ").contains(&quoted), "{path}");
    }
}
