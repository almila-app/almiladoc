//! An OCR engine filling a parsed document's image gaps in batches, in-process or from
//! requests exported to another.

use std::num::NonZeroU32;
use std::sync::Mutex;

use crate::build::{docx_with, relationships};
use crate::support::{configured, fixture, trips};
use almiladoc::{
    CancelToken, Config, ConvertError, Converter, DiagnosticCode, Format, GapFill, ImageOcr, Input,
    Limit, OcrEngine, OcrFailure, OcrOutcome, OcrRequest, OcrSummary, OnLoss, Parsed,
};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Answers each call with its script's next row, one letter per request: `F`ills it with
/// `OCR` and its gap's ordinal, fai`L`s it or leaves it `U`nattempted; a missing row fills
/// every request, and a `P` panics.
struct Scripted {
    size: usize,
    script: Mutex<Vec<&'static str>>,
    calls: Mutex<Vec<Vec<OcrRequest>>>,
    cancels: Option<(usize, CancelToken)>,
}

impl Scripted {
    fn new(size: usize, script: &[&'static str]) -> Self {
        let script = script.iter().rev().copied().collect();
        Self { size, script: Mutex::new(script), calls: Mutex::default(), cancels: None }
    }

    fn calls(&self) -> Vec<Vec<u32>> {
        let calls = self.calls.lock().unwrap();
        calls.iter().map(|call| call.iter().map(|r| r.gap.ordinal().get()).collect()).collect()
    }

    fn requests(&self) -> Vec<OcrRequest> {
        self.calls.lock().unwrap().concat()
    }
}

fn read(request: &OcrRequest) -> String {
    format!("OCR {}", request.gap.ordinal())
}

impl OcrEngine for Scripted {
    fn batch_size(&self) -> usize {
        self.size
    }

    fn recognize(&self, requests: &[OcrRequest], _: &CancelToken) -> Vec<OcrOutcome> {
        let mut calls = self.calls.lock().unwrap();
        calls.push(requests.to_vec());
        if let Some((call, token)) = &self.cancels
            && *call == calls.len()
        {
            token.cancel();
        }
        let row = self.script.lock().unwrap().pop();
        let letters = row.map_or_else(|| "F".repeat(requests.len()), str::to_owned);
        assert_ne!(letters, "P", "an engine defect");
        letters
            .chars()
            .zip(requests)
            .map(|(letter, request)| match letter {
                'F' => OcrOutcome::Fill(GapFill::Markdown(read(request))),
                'L' => OcrOutcome::Failed(OcrFailure::new(Some(500))),
                _ => OcrOutcome::Unattempted(OcrFailure::new(None)),
            })
            .collect()
    }
}

/// A document of `count` paragraphs, each showing a PNG of `size` bytes.
fn pictures(count: usize, size: usize) -> Vec<u8> {
    let png = [PNG, &vec![0; size.saturating_sub(PNG.len())]].concat();
    let body: String = (1..=count)
        .map(|n| {
            format!(
                r#"<w:p><w:r><w:t>p{n}</w:t></w:r><w:r><w:drawing><a:blip r:embed="png"/></w:drawing></w:r></w:p>"#
            )
        })
        .collect();
    let rels = relationships(&[("png", "image", "media/a.png")]);
    docx_with(
        &body,
        &[("word/_rels/document.xml.rels", rels.as_bytes()), ("word/media/a.png", &png)],
    )
}

fn image_ocr(set: impl FnOnce(&mut Config)) -> Converter {
    configured(|config| {
        config.image_ocr = ImageOcr::All;
        set(config);
    })
}

fn parsed(converter: &Converter, bytes: &[u8]) -> Parsed {
    converter.parse(Input::new(bytes, Format::Docx)).unwrap()
}

/// The least `max_work` under which `bytes` parse with image OCR and `passes` holds of the parse.
fn least_work(bytes: &[u8], passes: &dyn Fn(&mut Parsed) -> bool) -> u64 {
    let (mut low, mut high) = (0, 1 << 20);
    while low < high {
        let mid = low + (high - low) / 2;
        let converter = image_ocr(|config| config.limits.max_work = mid);
        let parsed = converter.parse(Input::new(bytes, Format::Docx));
        if parsed.is_ok_and(|mut parsed| passes(&mut parsed)) { high = mid } else { low = mid + 1 }
    }
    low
}

fn summary(filled: usize, failed: usize, unattempted: usize) -> OcrSummary {
    let mut summary = OcrSummary::default();
    (summary.filled, summary.failed, summary.unattempted) = (filled, failed, unattempted);
    summary
}

fn counts(parsed: &Parsed, code: DiagnosticCode) -> u64 {
    parsed.diagnostics().iter().filter(|d| d.code == code).map(|d| d.count).sum()
}

const RICH_READ: &str = "Rich objects follow.\n\n**Quarterly Widgets**\n\n\
                         | Quarter | Widgets |\n| --- | --- |\n| Q1 | 10 |\n| Q2 | 14 |\n\n\
                         - Plan\n- Build\n- Ship\n\n\
                         ![tiny dot image](asset:0)\n\nOCR 1\n\n\
                         After the objects.\n";

#[test]
fn an_engine_and_a_one_call_conversion_agree() {
    let converter = image_ocr(|config| config.on_loss = OnLoss::Report);
    let rich = fixture("docx/handmade-rich.docx");
    let mut engined = parsed(&converter, &rich);
    assert_eq!(engined.ocr(&Scripted::new(1, &[]), None), Ok(summary(1, 0, 0)));
    let converted = converter
        .convert_with_ocr(Input::new(&rich, Format::Docx), &Scripted::new(1, &[]))
        .unwrap();
    assert_eq!(engined.render().unwrap().markdown(), RICH_READ);
    assert_eq!(converted.markdown(), RICH_READ);
}

#[test]
fn batches_of_three_over_seven_gaps_and_an_unattempted_gap_ending_the_call() {
    let bytes = pictures(7, 8);
    let mut whole = parsed(&image_ocr(|_| {}), &bytes);
    let engine = Scripted::new(3, &[]);
    assert_eq!(whole.ocr(&engine, None), Ok(summary(7, 0, 0)));
    assert_eq!(engine.calls(), [vec![1, 2, 3], vec![4, 5, 6], vec![7]]);

    let mut stopped = parsed(&image_ocr(|_| {}), &bytes);
    let engine = Scripted::new(3, &["FFF", "FUF"]);
    assert_eq!(stopped.ocr(&engine, None), Ok(summary(5, 0, 2)));
    assert_eq!(engine.calls(), [vec![1, 2, 3], vec![4, 5, 6]]);
    let engine = Scripted::new(3, &[]);
    assert_eq!(stopped.ocr(&engine, None), Ok(summary(2, 0, 0)));
    assert_eq!(engine.calls(), [vec![5, 7]]);
    assert_eq!(stopped.render().unwrap().markdown(), whole.render().unwrap().markdown());
}

#[test]
fn a_wrong_outcome_count_or_an_oversized_fill_fails_only_its_gaps() {
    let bytes = pictures(4, 8);
    let mut counted = parsed(&image_ocr(|_| {}), &bytes);
    assert_eq!(counted.ocr(&Scripted::new(3, &["FF"]), None), Ok(summary(1, 3, 0)));
    assert_eq!(counts(&counted, DiagnosticCode::OcrFailed), 3);

    struct Oversized;
    impl OcrEngine for Oversized {
        fn recognize(&self, requests: &[OcrRequest], _: &CancelToken) -> Vec<OcrOutcome> {
            let size = |request: &OcrRequest| match request.gap.ordinal().get() {
                2 => usize::try_from(request.max_text_bytes).unwrap() + 1,
                _ => 1,
            };
            requests
                .iter()
                .map(|r| OcrOutcome::Fill(GapFill::Markdown("a".repeat(size(r)))))
                .collect()
        }
    }
    let mut oversized = parsed(&image_ocr(|_| {}), &bytes);
    assert_eq!(oversized.ocr(&Oversized, None), Ok(summary(3, 1, 0)));
    assert_eq!(counts(&oversized, DiagnosticCode::OcrFailed), 1);
}

#[test]
fn a_cancel_in_recognize_keeps_earlier_fills_and_refuses_what_follows() {
    let bytes = pictures(4, 8);
    let token = CancelToken::new();
    let input = Input::new(&bytes, Format::Docx).with_cancel(token.clone());
    let mut parsed = image_ocr(|_| {}).parse(input).unwrap();
    let mut engine = Scripted::new(2, &[]);
    engine.cancels = Some((2, token));
    assert_eq!(parsed.ocr(&engine, None), Err(ConvertError::Cancelled));
    assert_eq!(counts(&parsed, DiagnosticCode::OcrGap), 2);
    assert_eq!(parsed.render(), Err(ConvertError::Cancelled));
    assert_eq!(parsed.ocr(&Scripted::new(1, &[]), None), Err(ConvertError::Cancelled));
}

#[test]
fn a_cancelled_parse_refuses_an_engine() {
    let csv = b"a,b\n1,2\n";
    let token = CancelToken::new();
    let input = Input::new(csv, Format::Csv).with_cancel(token.clone());
    let mut parsed = configured(|_| {}).parse(input).unwrap();
    token.cancel();
    let engine = Scripted::new(1, &[]);
    assert_eq!(parsed.ocr(&engine, None), Err(ConvertError::Cancelled));
}

#[test]
fn an_engine_panic_is_internal() {
    let mut parsed = parsed(&image_ocr(|_| {}), &pictures(2, 8));
    assert_eq!(parsed.ocr(&Scripted::new(2, &["P"]), None), Err(ConvertError::Internal));
}

#[test]
fn each_request_charges_its_image_at_one_unit_per_sixteen_bytes() {
    let bytes = pictures(2, 4000);
    let parse = least_work(&bytes, &|_| true);
    let requests =
        least_work(&bytes, &|parsed| parsed.ocr(&Scripted::new(2, &["LL"]), None).is_ok());
    assert_eq!(requests, parse + 2 * 4000_u64.div_ceil(16));
    let converter = image_ocr(|config| config.limits.max_work = requests - 1);
    let mut short = parsed(&converter, &bytes);
    let refused = short.ocr(&Scripted::new(1, &[]), None);
    assert!(trips(&refused, Limit::Work, requests - 1), "{refused:?}");
}

#[test]
fn a_request_carries_the_parse_s_fill_limit_and_the_image_s_bytes() {
    let converter = image_ocr(|config| config.limits.max_ocr_text_bytes = 3 << 20);
    let mut parsed = parsed(&converter, &pictures(2, 40));
    let [one, two] = parsed.gaps().map(|gap| gap.id).collect::<Vec<_>>().try_into().unwrap();
    let engine = Scripted::new(2, &["U"]);
    assert_eq!(parsed.ocr(&engine, Some(&[two])), Ok(summary(0, 0, 1)));
    let [request] = engine.requests().try_into().unwrap();
    assert_eq!((request.gap, request.max_text_bytes), (two, 3 << 20));
    assert_eq!((request.image.media_type, request.image.bytes.len()), ("image/png", 40));
    assert!(format!("{request:?}").contains("bytes: 40"), "{request:?}");
    parsed.fail_gap(one).unwrap();
    let engine = Scripted::new(2, &[]);
    assert_eq!(parsed.ocr(&engine, Some(&[one, two])), Ok(summary(1, 0, 0)));
    assert_eq!(engine.calls(), [vec![2]]);
    assert_eq!(one.ordinal(), NonZeroU32::MIN);
}

#[cfg(feature = "pdf")]
#[test]
fn a_page_gap_asks_for_no_image_yet() {
    let pdf = fixture("pdf/handmade-five-page.pdf");
    let mut parsed = configured(|_| {}).parse(Input::new(&pdf, Format::Pdf)).unwrap();
    let engine = Scripted::new(1, &[]);
    assert_eq!((parsed.ocr(&engine, None), engine.calls().len()), (Ok(summary(0, 0, 0)), 0));
}

#[test]
fn hand_run_requests_and_fills_render_the_engine_s_reading() {
    let converter = image_ocr(|config| config.on_loss = OnLoss::Report);
    let rich = fixture("docx/handmade-rich.docx");
    let mut by_hand = parsed(&converter, &rich);
    for request in by_hand.ocr_requests(None).unwrap() {
        by_hand.fill_gap(request.gap, GapFill::Markdown(read(&request))).unwrap();
    }
    assert_eq!(by_hand.ocr_requests(None), Ok(Vec::new()));
    assert_eq!(by_hand.render().unwrap().markdown(), RICH_READ);
}

#[test]
fn requests_pass_over_a_settled_gap_and_charge_their_copies() {
    let bytes = pictures(2, 4000);
    let converter = image_ocr(|config| config.limits.max_ocr_text_bytes = 3 << 20);
    let mut parsed = parsed(&converter, &bytes);
    let [one, two] = parsed.gaps().map(|gap| gap.id).collect::<Vec<_>>().try_into().unwrap();
    let requests = parsed.ocr_requests(None).unwrap();
    assert_eq!(
        requests.iter().map(|r| (r.gap, r.max_text_bytes)).collect::<Vec<_>>(),
        [(one, 3 << 20), (two, 3 << 20)]
    );
    parsed.fail_gap(one).unwrap();
    assert_eq!(parsed.ocr_requests(Some(&[one, two])).unwrap(), requests[1..]);
    let parse = least_work(&bytes, &|_| true);
    let copied = least_work(&bytes, &|parsed| parsed.ocr_requests(None).is_ok());
    assert_eq!(copied, parse + 2 * 4000_u64.div_ceil(16));
    let cancelled = CancelToken::new();
    let input = Input::new(&bytes, Format::Docx).with_cancel(cancelled.clone());
    let mut parsed = image_ocr(|_| {}).parse(input).unwrap();
    cancelled.cancel();
    assert_eq!(parsed.ocr_requests(None), Err(ConvertError::Cancelled));
}
