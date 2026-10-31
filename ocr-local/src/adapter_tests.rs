use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Cursor;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use almiladoc::{
    CancelToken, Converter, Format, GapFill, ImageOcr, Input, OcrEngine, OcrFailure, OcrImage,
    OcrOutcome, OnLoss, RasterSpec,
};
use image::{DynamicImage, ImageFormat, RgbImage};
use oar_ocr_core::{domain::TextRegion, processors::BoundingBox};

use crate::{
    Error, Limits, LocalOcr, Page,
    adapter::{Recognizer, located},
    output,
};

struct Fake {
    pages: VecDeque<Result<Page, Error>>,
    calls: Arc<AtomicUsize>,
    cancel: bool,
}

impl Recognizer for Fake {
    fn page(&mut self, image: RgbImage, _: &Limits, cancel: &CancelToken) -> Result<Page, Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.cancel {
            cancel.cancel();
        }
        self.pages.pop_front().expect("one producer response per requested image").map(
            |mut page| {
                if page.lines.is_empty() {
                    (page.width, page.height) = image.dimensions();
                }
                page
            },
        )
    }
}

fn image(format: ImageFormat) -> OcrImage {
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(RgbImage::new(10, 10)).write_to(&mut encoded, format).unwrap();
    OcrImage::new(encoded.into_inner(), None).unwrap()
}

fn page() -> Page {
    let regions = [("# raw second", [5.0, 5.0, 9.0, 8.0]), ("İ *first*", [1.0, 1.0, 4.0, 3.0])];
    output::page(
        regions.into_iter().map(|(text, [l, t, r, b])| {
            Ok(TextRegion::with_recognition(
                BoundingBox::from_coords(l, t, r, b),
                Some(Arc::from(text)),
                Some(0.8),
            ))
        }),
        10,
        10,
        &Limits::default(),
    )
    .unwrap()
}

fn local(pages: Vec<Result<Page, Error>>, cancel: bool) -> (LocalOcr, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (
        LocalOcr {
            backend: Mutex::new(Box::new(Fake {
                pages: pages.into(),
                calls: Arc::clone(&calls),
                cancel,
            })),
            limits: Limits::default(),
            raster: RasterSpec::default(),
        },
        calls,
    )
}

fn parsed(count: usize) -> almiladoc::Parsed {
    let picture: String =
        image(ImageFormat::Png).bytes.iter().fold(String::new(), |mut text, byte| {
            write!(text, "{byte:02x}").unwrap();
            text
        });
    let rtf = format!("{{\\rtf1 {}}}", format!("{{\\pict\\pngblip {picture}}}").repeat(count));
    let mut config = almiladoc::Config::default();
    config.image_ocr = ImageOcr::All;
    config.on_loss = OnLoss::Report;
    Converter::new(config).unwrap().parse(Input::new(rtf.as_bytes(), Format::Rtf)).unwrap()
}

#[test]
fn located_output_preserves_text_and_detector_order_and_normalizes_only_boxes() {
    for format in [ImageFormat::Png, ImageFormat::Jpeg] {
        let (engine, calls) = local(vec![Ok(page())], false);
        let result = engine.read(&image(format), 1024, &CancelToken::new());
        let OcrOutcome::Fill(GapFill::Lines(lines)) = result else {
            panic!("expected located lines")
        };
        assert_eq!(
            lines.iter().map(almiladoc::OcrLine::text).collect::<Vec<_>>(),
            ["# raw second", "İ *first*"]
        );
        assert_eq!(lines[0].bounds(), [0.5, 0.5, 0.9, 0.8]);
        assert_eq!(lines[1].bounds(), [0.1, 0.1, 0.4, 0.3]);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(engine.batch_size(), 1);
    }
}

#[test]
fn only_core_lines_orders_and_escapes_the_producer_text() {
    let (engine, _) = local(vec![Ok(page())], false);
    let mut parsed = parsed(1);
    assert_eq!(parsed.ocr(&engine, None).unwrap().filled, 1);
    let rendered = parsed.render().unwrap();
    let markdown = rendered.markdown();
    assert!(markdown.find("first").unwrap() < markdown.find("second").unwrap());
    let events: Vec<_> = pulldown_cmark::Parser::new(markdown).collect();
    let literal: String = events
        .iter()
        .filter_map(|event| match event {
            pulldown_cmark::Event::Text(text) => Some(text.as_ref()),
            _ => None,
        })
        .collect();
    assert!(literal.contains("İ *first*"));
    assert!(!events.iter().any(|event| matches!(
        event,
        pulldown_cmark::Event::Start(
            pulldown_cmark::Tag::Emphasis | pulldown_cmark::Tag::Heading { .. }
        )
    )));
}

#[test]
fn zero_detections_settles_blank_through_core_but_missing_recognition_never_does() {
    let (engine, calls) = local(vec![Ok(Page { width: 10, height: 10, lines: vec![] })], false);
    let converter = Converter::new(almiladoc::Config::default()).unwrap();
    let input =
        Input::new(include_bytes!("../../tests/fixtures/pdf/handmade-blank.pdf"), Format::Pdf)
            .with_ocr_pages(almiladoc::PageSelection::All);
    let mut parsed = converter.parse(input).unwrap();
    let summary = parsed.ocr(&engine, None).unwrap();
    assert_eq!((summary.filled, summary.failed, summary.unattempted), (1, 0, 0));
    assert_eq!(parsed.render().unwrap().markdown(), "");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let (engine, _) = local(vec![Err(Error::EmptyRecognition)], false);
    assert_eq!(
        engine.read(&image(ImageFormat::Png), 1024, &CancelToken::new()),
        OcrOutcome::Failed(OcrFailure::default())
    );
}

#[test]
fn fill_budget_includes_geometry_and_every_owned_text_byte() {
    let bytes: u64 = 2 * almiladoc::OcrLine::BOX_BYTES + 12 + 10;
    assert_eq!(page().lines.iter().map(|line| line.text().len()).sum::<usize>(), 22);
    for (budget, accepted) in [(bytes, true), (bytes - 1, false)] {
        let (engine, _) = local(vec![Ok(page())], false);
        let result = engine.read(&image(ImageFormat::Png), budget, &CancelToken::new());
        assert_eq!(matches!(result, OcrOutcome::Fill(GapFill::Lines(_))), accepted);
        if !accepted {
            assert_eq!(result, OcrOutcome::Failed(OcrFailure::default()));
        }
    }
}

#[test]
fn bad_images_fail_before_any_producer_call() {
    let good = image(ImageFormat::Png);
    let broken = OcrImage::new(b"\x89PNG\r\n\x1a\ninvalid".to_vec(), None).unwrap();
    let mut wrong_mime = good.clone();
    wrong_mime.media_type = "image/jpeg";
    for image in [broken, wrong_mime] {
        let (engine, calls) = local(vec![], false);
        assert_eq!(
            engine.read(&image, 1024, &CancelToken::new()),
            OcrOutcome::Failed(OcrFailure::default())
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
    for change in [
        |limits: &mut Limits| limits.max_image_bytes = 1,
        |limits: &mut Limits| limits.max_pixels = 99,
        |limits: &mut Limits| limits.max_decode_bytes = 699,
        |limits: &mut Limits| limits.max_dimension = 9,
    ] {
        let (mut engine, calls) = local(vec![], false);
        change(&mut engine.limits);
        assert_eq!(
            engine.read(&good, 1024, &CancelToken::new()),
            OcrOutcome::Failed(OcrFailure::default())
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn runtime_faults_stop_the_batch_while_per_image_refusals_keep_exact_cardinality() {
    let mut requests = parsed(3).ocr_requests(&RasterSpec::default(), None).unwrap();
    assert_eq!(requests.len(), 3);
    requests[0].image = OcrImage::new(b"\x89PNG\r\n\x1a\ninvalid".to_vec(), None).unwrap();
    let (engine, calls) = local(vec![Ok(page()), Ok(page())], false);
    let outcomes = engine.recognize(&requests, &CancelToken::new());
    assert_eq!(outcomes.len(), 3);
    assert_eq!(outcomes[0], OcrOutcome::Failed(OcrFailure::default()));
    assert!(
        outcomes[1..].iter().all(|outcome| matches!(outcome, OcrOutcome::Fill(GapFill::Lines(_))))
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    requests[0].image = image(ImageFormat::Png);
    for error in
        [Error::Recognition, Error::InvalidOutput, Error::Runtime, Error::Model, Error::Pool]
    {
        let (engine, calls) = local(vec![Err(error)], false);
        assert_eq!(
            engine.recognize(&requests, &CancelToken::new()),
            vec![OcrOutcome::Unattempted(OcrFailure::default()); 3]
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn crop_limit_refusal_fails_only_its_image_and_keeps_later_requests() {
    let points = [(0.0, 7000.0), (2.0, 0.0), (3.0, 7000.0), (2.0, 8192.0)]
        .map(|(x, y)| oar_ocr_core::processors::Point::new(x, y));
    let error = crate::stages::crop(&RgbImage::new(3, 8192), &points, &Limits::default(), || {
        panic!("oversized crops must be refused before allocation")
    })
    .unwrap_err();
    let requests = parsed(3).ocr_requests(&RasterSpec::default(), None).unwrap();
    let (engine, calls) = local(vec![Err(error), Ok(page()), Ok(page())], false);
    let outcomes = engine.recognize(&requests, &CancelToken::new());
    assert_eq!(outcomes.len(), 3);
    assert_eq!(outcomes[0], OcrOutcome::Failed(OcrFailure::default()));
    assert!(
        outcomes[1..].iter().all(|outcome| matches!(outcome, OcrOutcome::Fill(GapFill::Lines(_))))
    );
    assert_eq!(calls.load(Ordering::Relaxed), 3);
}

#[test]
fn cancellation_before_decode_after_native_return_and_busy_engine_never_fill() {
    let requests = parsed(3).ocr_requests(&RasterSpec::default(), None).unwrap();
    for during in [false, true] {
        let cancel = CancelToken::new();
        if !during {
            cancel.cancel();
        }
        let (engine, calls) = local(vec![Ok(page())], during);
        assert_eq!(
            engine.recognize(&requests, &cancel),
            vec![OcrOutcome::Unattempted(OcrFailure::default()); 3]
        );
        assert_eq!(calls.load(Ordering::Relaxed), usize::from(during));
    }
    let (engine, _) = local(vec![Err(Error::EmptyRecognition)], true);
    assert_eq!(
        engine.read(&image(ImageFormat::Png), 1024, &CancelToken::new()),
        OcrOutcome::Unattempted(OcrFailure::default())
    );
    let (engine, calls) = local(vec![], false);
    let _held = engine.backend.lock().unwrap();
    assert_eq!(
        engine.recognize(&requests, &CancelToken::new()),
        vec![OcrOutcome::Unattempted(OcrFailure::default()); 3]
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn dimensions_geometry_line_and_text_limits_refuse_atomic_output() {
    let cancel = CancelToken::new();
    let mut limits = Limits::default();
    let mut wrong = page();
    wrong.width = 9;
    assert_eq!(located(wrong, (10, 10), 1024, &limits, &cancel), Err(Error::InvalidOutput));
    let mut wrong = page();
    wrong.width = 5;
    assert_eq!(located(wrong, (5, 10), 1024, &limits, &cancel), Err(Error::InvalidOutput));
    limits.max_lines = 1;
    assert_eq!(located(page(), (10, 10), 1024, &limits, &cancel), Err(Error::OutputLimit));
    limits.max_lines = 2;
    limits.max_text_bytes = 21;
    assert_eq!(located(page(), (10, 10), 1024, &limits, &cancel), Err(Error::OutputLimit));
}

#[test]
fn construction_refuses_raster_before_reading_any_model_and_holds_the_buffer_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let models = crate::ModelFiles {
        detector: crate::FileSpec::new(dir.path().join("detector"), [1; 32]),
        recognizer: crate::FileSpec::new(dir.path().join("recognizer"), [2; 32]),
        dictionary: crate::FileSpec::new(dir.path().join("dictionary"), [3; 32]),
    };
    let runtime = crate::Runtime::test_handle();
    let mut raster = RasterSpec::default();
    raster.max_pixels = 1 << 20;
    let mut config = crate::Config::default();
    config.limits.max_pixels = raster.max_pixels;
    config.limits.max_decode_bytes = 10 * raster.max_pixels;
    for change in [
        |c: &mut crate::Config| c.limits.max_pixels -= 1,
        |c: &mut crate::Config| c.limits.max_decode_bytes -= 1,
    ] {
        let mut refused = config.clone();
        change(&mut refused);
        assert_eq!(
            LocalOcr::new(&runtime, &models, refused, raster).unwrap_err(),
            Error::Config("raster")
        );
    }
    let mut invalid = raster;
    invalid.dpi = 0;
    assert_eq!(
        LocalOcr::new(&runtime, &models, config.clone(), invalid).unwrap_err(),
        Error::Config("raster")
    );
    let mut invalid = config.clone();
    invalid.threads = 0;
    assert_eq!(
        LocalOcr::new(&runtime, &models, invalid, raster).unwrap_err(),
        Error::Config("threads")
    );
    assert_eq!(
        LocalOcr::new(&runtime, &models, config, raster).unwrap_err(),
        Error::FileUnreadable(crate::FileRole::Detector)
    );
}
