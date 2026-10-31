//! Native OCR integration with caller-verified CPU assets.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "support/logging.rs"]
mod test_logging;

use almiladoc::{
    CancelToken, Converter, Format, GapFill, ImageOcr, ImagePolicy, Input, OcrEngine, OcrFailure,
    OcrOutcome, OnLoss, RasterSpec,
};
use almiladoc_ocr_local::{
    Config, Error, FileRole, FileSpec, Limits, LocalOcr, ModelFiles, Runtime,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

fn read(path: &Path, cap: usize) -> Vec<u8> {
    let file = std::fs::File::open(path).unwrap_or_else(|_| panic!("native test input unreadable"));
    let mut bytes = Vec::new();
    file.take(cap as u64 + 1).read_to_end(&mut bytes).expect("native test input read failed");
    assert!(bytes.len() <= cap, "native test input exceeds its cap");
    bytes
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn expected_digest(value: &Value) -> [u8; 32] {
    let hex = value["sha256"].as_str().expect("native test digest missing");
    assert!(
        hex.len() == 64 && hex.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid native test digest"
    );
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).expect("invalid digest");
    }
    bytes
}

fn path(root: &Path, value: &Value) -> std::path::PathBuf {
    root.join(value["file"].as_str().expect("native test file missing"))
}

fn spec(root: &Path, value: &Value) -> FileSpec {
    FileSpec::new(path(root, value), expected_digest(value))
}

fn normal(text: &str) -> String {
    text.nfc().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parsed(bytes: &[u8]) -> almiladoc::Parsed {
    let mut hex = String::new();
    for byte in bytes {
        write!(&mut hex, "{byte:02x}").expect("image encoding failed");
    }
    let rtf = format!("{{\\rtf1 {{\\pict\\pngblip {hex}}}}}");
    let mut config = almiladoc::Config::default();
    config.image_ocr = ImageOcr::All;
    config.images = ImagePolicy::Omit;
    config.on_loss = OnLoss::Report;
    config.limits.max_ocr_text_bytes = 8192;
    Converter::new(config)
        .expect("core config failed")
        .parse(Input::new(rtf.as_bytes(), Format::Rtf))
        .expect("native image parse failed")
}

fn check_case(
    engine: &LocalOcr,
    raster: &RasterSpec,
    root: &Path,
    case: &Value,
) -> (bool, almiladoc::OcrRequest) {
    let bytes = read(&path(root, case), 1 << 20);
    assert!(digest(&bytes) == expected_digest(case), "native image digest mismatch");
    let expected = case["text"].as_str().expect("native expected text missing");
    assert!(expected.chars().count() <= 512, "native expected text exceeds its cap");
    let mut parsed = parsed(&bytes);
    let mut requests = parsed.ocr_requests(raster, None).expect("native request export failed");
    assert_eq!(requests.len(), 1);
    let request = requests.pop().expect("native request missing");
    let mut outcomes = engine.recognize(std::slice::from_ref(&request), &CancelToken::new());
    assert_eq!(outcomes.len(), 1);
    let OcrOutcome::Fill(GapFill::Lines(lines)) = outcomes.pop().expect("native outcome missing")
    else {
        panic!("native recognition refused");
    };
    let blank = expected.trim().is_empty();
    assert!(blank == lines.is_empty(), "native text/blank control mismatch");
    for line in &lines {
        let [left, top, right, bottom] = line.bounds();
        assert!(
            [left, top, right, bottom].iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v))
        );
        assert!(left < right && top < bottom && !line.text().trim().is_empty());
    }
    parsed.fill_gap(request.gap, GapFill::Lines(lines)).expect("native fill failed");
    let converted = parsed.render().expect("native render failed");
    let mut text = String::new();
    for event in pulldown_cmark::Parser::new(converted.markdown()) {
        match event {
            pulldown_cmark::Event::Text(value) => text.push_str(&value),
            pulldown_cmark::Event::SoftBreak
            | pulldown_cmark::Event::HardBreak
            | pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Paragraph) => text.push(' '),
            _ => {}
        }
    }
    assert!(normal(&text) == normal(expected), "native text does not match the control");
    (blank, request)
}

fn runtime(root: &Path, manifest: &Value, limits: &Limits) -> Runtime {
    let runtime_file = spec(root, &manifest["runtime"]);
    let mut wrong_digest = expected_digest(&manifest["runtime"]);
    wrong_digest[0] ^= 1;
    assert_eq!(
        Runtime::load(&FileSpec::new(path(root, &manifest["runtime"]), wrong_digest), limits)
            .unwrap_err(),
        Error::DigestMismatch(FileRole::Runtime)
    );
    let runtime = Runtime::load(&runtime_file, limits).expect("native runtime load failed");
    let _reused = Runtime::load(&runtime_file, limits).expect("same-digest runtime reuse failed");
    let directory = tempfile::tempdir().expect("native test directory failed");
    let conflict = directory.path().join("conflicting-runtime");
    std::fs::write(&conflict, b"different verified bytes").expect("native conflict fixture failed");
    assert_eq!(
        Runtime::load(&FileSpec::new(conflict, digest(b"different verified bytes")), limits)
            .unwrap_err(),
        Error::RuntimeConflict
    );
    runtime
}

fn check_vocabulary_mismatch(
    runtime: &Runtime,
    mut models: ModelFiles,
    config: Config,
    raster: RasterSpec,
    request: almiladoc::OcrRequest,
) {
    let directory = tempfile::tempdir().expect("native vocabulary directory failed");
    let wrong = directory.path().join("wrong-vocabulary");
    std::fs::write(&wrong, b"x\n").expect("native vocabulary fixture failed");
    models.dictionary = FileSpec::new(wrong, digest(b"x\n"));
    let incompatible =
        LocalOcr::new(runtime, &models, config, raster).expect("native mismatch setup failed");
    let result = incompatible.recognize(&[request], &CancelToken::new());
    assert!(
        result == vec![OcrOutcome::Unattempted(OcrFailure::default())],
        "incompatible vocabulary was not refused"
    );
}

#[test]
#[ignore = "requires ALMILADOC_OCR_NATIVE_INPUT with verified CPU assets and text/blank controls"]
fn verified_native_runtime_models_vocabulary_and_core_settlement() {
    let input =
        std::env::var_os("ALMILADOC_OCR_NATIVE_INPUT").expect("set ALMILADOC_OCR_NATIVE_INPUT");
    let input = Path::new(&input);
    let root = input.parent().expect("native input root missing");
    let manifest: Value =
        serde_json::from_slice(&read(input, 1 << 20)).expect("invalid native input manifest");
    let config = Config {
        threads: 1,
        limits: Limits {
            max_image_bytes: 1 << 20,
            max_dimension: 2048,
            max_pixels: 1 << 20,
            max_decode_bytes: 16 << 20,
            max_artifact_bytes: 64 << 20,
            max_dictionary_bytes: 1 << 20,
            max_lines: 128,
            max_text_bytes: 8192,
        },
    };
    let runtime = runtime(root, &manifest, &config.limits);
    let models = ModelFiles {
        detector: spec(root, &manifest["detector"]),
        recognizer: spec(root, &manifest["recognizer"]),
        dictionary: spec(root, &manifest["dictionary"]),
    };
    let mut raster = RasterSpec::default();
    raster.max_pixels = 1 << 20;
    let engine =
        LocalOcr::new(&runtime, &models, config.clone(), raster).expect("native models failed");
    let mut blank_seen = false;
    let mut text_request = None;
    let cases = manifest["cases"].as_array().expect("native controls missing");
    assert_eq!(cases.len(), 2);
    for case in cases {
        let (blank, request) = check_case(&engine, &raster, root, case);
        if blank {
            blank_seen = true;
        } else {
            text_request = Some(request);
        }
    }
    assert!(blank_seen && text_request.is_some(), "both native controls are required");
    check_vocabulary_mismatch(
        &runtime,
        models,
        config,
        raster,
        text_request.expect("text control missing"),
    );
}

fn native_setup<T, E>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|_| panic!("native privacy setup failed"))
}

fn verbose_session() -> ort::session::builder::SessionBuilder {
    let session = native_setup(ort::session::Session::builder());
    let session = native_setup(session.with_intra_threads(1));
    let session = native_setup(session.with_inter_threads(1));
    let session = native_setup(session.with_parallel_execution(false));
    let session = native_setup(session.with_log_level(ort::logging::LogLevel::Verbose));
    native_setup(session.with_log_verbosity(1))
}

fn loader_event(metadata: &tracing::Metadata<'_>) -> bool {
    metadata.target() == "ort" || metadata.target().starts_with("ort::")
}

fn native_message(metadata: &tracing::Metadata<'_>) -> bool {
    metadata.target() == "ort::logging"
}

#[test]
#[ignore = "requires ALMILADOC_OCR_NATIVE_INPUT; run alone in a fresh process"]
fn installed_runtime_logger_does_not_forward_native_messages_to_tracing() {
    let events = test_logging::install(loader_event);
    tracing::info!(target: "ort", privacy_probe = true);
    assert_eq!(events.count(), 1);
    let input =
        std::env::var_os("ALMILADOC_OCR_NATIVE_INPUT").expect("set ALMILADOC_OCR_NATIVE_INPUT");
    let input = Path::new(&input);
    let root = input.parent().expect("native input root missing");
    let manifest: Value =
        serde_json::from_slice(&read(input, 1 << 20)).expect("invalid native input manifest");
    let limits = Limits { max_artifact_bytes: 64 << 20, ..Limits::default() };
    events.watch(Some(loader_event));
    let _runtime = runtime(root, &manifest, &limits);
    let load_events = events.count();
    events.watch(None);
    assert_eq!(load_events, 0);
    let environment = native_setup(ort::environment::Environment::current());
    environment.set_log_level(ort::logging::LogLevel::Verbose);
    let model = read(&path(root, &manifest["detector"]), limits.max_artifact_bytes);
    assert!(
        digest(&model) == expected_digest(&manifest["detector"]),
        "native model digest mismatch"
    );
    let callbacks = Arc::new(AtomicUsize::new(0));
    let captured = Arc::clone(&callbacks);
    let mut control =
        native_setup(verbose_session().with_logger(Arc::new(move |_, _, _, _, _| {
            captured.fetch_add(1, Ordering::SeqCst);
        })));
    let session = native_setup(control.commit_from_memory(&model));
    drop(session);
    assert_ne!(callbacks.load(Ordering::SeqCst), 0);
    events.watch(Some(native_message));
    tracing::info!(target: "ort::logging", privacy_probe = true);
    assert_eq!(events.count(), 1);
    events.watch(Some(native_message));
    let session = native_setup(verbose_session().commit_from_memory(&model));
    drop(session);
    let native_events = events.count();
    events.watch(None);
    assert_eq!(native_events, 0);
}
