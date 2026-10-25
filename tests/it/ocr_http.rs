//! `OcrHttp` against a loopback server that answers each connection from a script.
#![expect(clippy::disallowed_types, reason = "a loopback server stands in for the vision server")]

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::build::{docx_with, relationships};
use crate::support::{configured, fixture};
use almiladoc::{
    CancelToken, Format, GapFill, ImageOcr, Input, OcrEngine, OcrFailure, OcrHttp, OcrHttpConfig,
    OcrOutcome, OcrRequest, OnLoss, Parsed, RasterSpec,
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
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
    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    while !head.ends_with("\r\n\r\n") {
        assert_ne!(reader.read_line(&mut head).unwrap(), 0, "{head}");
    }
    let mut writer = stream;
    if let Answer::Early(code) = answer {
        let early =
            format!("HTTP/1.1 {code} Early\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        writer.write_all(early.as_bytes()).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        thread::sleep(Duration::from_millis(200));
        return Seen { head, body: None };
    }
    if head.to_ascii_lowercase().contains("\r\nexpect: 100-continue\r\n") {
        writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").unwrap();
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
    std::io::Read::read_exact(&mut reader, &mut body).unwrap();
    if let Answer::Reply(code, headers, body) = answer {
        let head = format!(
            "HTTP/1.1 {code} Scripted\r\ncontent-length: {}\r\nconnection: close\r\n{headers}\r\n",
            body.len()
        );
        writer.write_all(head.as_bytes()).unwrap();
        writer.write_all(&body).unwrap();
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

/// The rich DOCX fixture with its image a gap, and that image's request four times over.
fn images(max_ocr_text_bytes: u64) -> (Parsed, Vec<OcrRequest>) {
    let converter = configured(|config| {
        config.image_ocr = ImageOcr::All;
        config.on_loss = OnLoss::Report;
        config.limits.max_ocr_text_bytes = max_ocr_text_bytes;
    });
    let mut parsed =
        converter.parse(Input::new(&fixture("docx/handmade-rich.docx"), Format::Docx)).unwrap();
    let [request] = parsed.ocr_requests(&RasterSpec::default(), None).unwrap().try_into().unwrap();
    (parsed, vec![request; 4])
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
        .recognize(&[&requests[..], &requests[..]].concat(), &CancelToken::new());
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
    let outcomes = engine(&closed, |_| {}).recognize(&requests, &CancelToken::new());
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
