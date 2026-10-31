//! A PDF page gap's request carries its page painted as a PNG under the engine's raster spec.

use crate::pdf::write_pdf;
use crate::support::{configured, converter, fixture, limited, reported, trips};
use almiladoc::model::OcrOrigin;
use almiladoc::{
    CancelToken, ConfigError, ConvertError, Converter, DiagnosticCode, Format, GapFill, Input,
    Limit, Limits, OcrEngine, OcrOutcome, OcrRequest, PageSelection, Parsed, RasterSpec, Region,
};

fn spec(dpi: u32, max_pixels: u64) -> RasterSpec {
    let mut spec = RasterSpec::default();
    (spec.dpi, spec.max_pixels) = (dpi, max_pixels);
    spec
}

/// The width and height a PNG's IHDR chunk declares.
fn png_size(png: &[u8]) -> (u32, u32) {
    let word = |at: usize| u32::from_be_bytes(png[at..at + 4].try_into().unwrap());
    assert_eq!(&png[12..16], b"IHDR");
    (word(16), word(20))
}

fn requested(parsed: &mut Parsed, spec: &RasterSpec) -> Vec<(u32, Option<u32>, (u32, u32))> {
    let requests = parsed.ocr_requests(spec, None).unwrap();
    requests
        .iter()
        .map(|request| {
            let OcrOrigin::Page(page) = request.origin else { panic!("{request:?}") };
            assert_eq!(request.image.media_type, "image/png");
            (page.get(), request.image.dpi, png_size(&request.image.bytes))
        })
        .collect()
}

fn every_page(converter: &Converter, bytes: &[u8]) -> Parsed {
    let input = Input::new(bytes, Format::Pdf).with_ocr_pages(PageSelection::All);
    converter.parse(input).unwrap()
}

#[test]
fn a_page_gap_exports_its_page_at_the_spec_s_dpi() {
    let bytes = fixture("pdf/handmade-five-page.pdf");
    let mut parsed = converter(1).parse(Input::new(&bytes, Format::Pdf)).unwrap();
    let letter = (1275, 1650);
    let default = RasterSpec::default();
    assert_eq!(requested(&mut parsed, &default), [(2, Some(150), letter), (5, Some(150), letter)]);
    let pages: Vec<u32> = requested(&mut every_page(&converter(1), &bytes), &default)
        .into_iter()
        .map(|(page, ..)| page)
        .collect();
    assert_eq!(pages, [1, 2, 3, 4, 5]);
    let pngs = |threads| {
        let mut parsed = every_page(&converter(threads), &bytes);
        let requests = parsed.ocr_requests(&default, None).unwrap();
        requests.into_iter().map(|request| request.image.bytes).collect::<Vec<_>>()
    };
    let once = pngs(1);
    assert_eq!(once, pngs(1));
    assert_eq!(once, pngs(4));
}

/// A one-page PDF of a page dictionary with `entries` and a line of text.
fn page_pdf(entries: &str) -> Vec<u8> {
    let content = "BT /F1 24 Tf 72 400 Td (Painted) Tj ET";
    write_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        format!(
            "<< /Type /Page /Parent 2 0 R {entries} /Contents 4 0 R \
             /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> >>"
        ),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
    ])
}

#[test]
fn a_page_s_box_and_rotation_decide_its_image_s_size() {
    let cap = 12_845_056;
    let large = &requested(
        &mut every_page(&converter(1), &page_pdf("/MediaBox [0 0 2725 2089]")),
        &RasterSpec::default(),
    )[0];
    let (_, Some(dpi), (width, height)) = *large else { panic!("{large:?}") };
    assert!(dpi < 150 && u64::from(width) * u64::from(height) <= cap, "{large:?}");
    for (entries, size) in [
        ("/MediaBox [0 0 612 792]", (1275, 1650)),
        ("/MediaBox [0 0 612 792] /Rotate 90", (1650, 1275)),
        ("/MediaBox [0 0 612 792] /CropBox [0 0 306 792]", (638, 1650)),
    ] {
        let found =
            requested(&mut every_page(&converter(1), &page_pdf(entries)), &RasterSpec::default());
        assert_eq!(found, [(1, Some(150), size)], "{entries}");
    }
}

#[test]
fn a_spec_out_of_range_is_refused_and_changes_nothing() {
    let bytes = fixture("pdf/handmade-five-page.pdf");
    let mut parsed = converter(1).parse(Input::new(&bytes, Format::Pdf)).unwrap();
    for refused in
        [spec(49, 1 << 20), spec(601, 1 << 20), spec(150, (1 << 20) - 1), spec(150, (40 << 20) + 1)]
    {
        let refusal = parsed.ocr_requests(&refused, None);
        assert_eq!(refusal, Err(ConvertError::InvalidConfig(ConfigError::Raster)), "{refused:?}");
    }
    struct Wide;
    impl OcrEngine for Wide {
        fn raster(&self) -> RasterSpec {
            spec(601, 1 << 20)
        }
        fn recognize(&self, _: &[OcrRequest], _: &CancelToken) -> Vec<OcrOutcome> {
            panic!("a refused spec requests nothing")
        }
    }
    assert_eq!(parsed.ocr(&Wide, None), Err(ConvertError::InvalidConfig(ConfigError::Raster)));
    assert_eq!(parsed.gaps().count(), 2);
}

/// Reads each page as its number, and cancels `cancels` on its first call.
struct Pages {
    cancels: Option<CancelToken>,
    calls: std::sync::Mutex<Vec<u32>>,
}

impl OcrEngine for Pages {
    fn recognize(&self, requests: &[OcrRequest], _: &CancelToken) -> Vec<OcrOutcome> {
        if let Some(token) = &self.cancels {
            token.cancel();
        }
        requests
            .iter()
            .map(|request| {
                let OcrOrigin::Page(page) = request.origin else { panic!("{request:?}") };
                self.calls.lock().unwrap().push(page.get());
                OcrOutcome::Fill(GapFill::Markdown(format!("page {page}")))
            })
            .collect()
    }
}

#[test]
fn a_cancel_stops_the_pages_before_the_next_is_painted() {
    let pages: Vec<String> =
        (1..=20).map(|n| format!("BT /F1 12 Tf 72 700 Td (p{n}) Tj ET")).collect();
    let pages: Vec<&str> = pages.iter().map(String::as_str).collect();
    let bytes = crate::pdf::pdf_pages(&pages);
    let token = CancelToken::new();
    let input = Input::new(&bytes, Format::Pdf)
        .with_ocr_pages(PageSelection::All)
        .with_cancel(token.clone());
    let mut parsed = converter(1).parse(input).unwrap();
    let engine = Pages { cancels: Some(token), calls: std::sync::Mutex::default() };
    assert_eq!(parsed.ocr(&engine, None), Err(ConvertError::Cancelled));
    assert_eq!(*engine.calls.lock().unwrap(), [1]);
}

#[test]
fn an_engine_and_the_hand_run_requests_fill_pages_alike() {
    let bytes = fixture("pdf/handmade-five-page.pdf");
    let engine = || Pages { cancels: None, calls: std::sync::Mutex::default() };
    let converted =
        converter(1).convert_with_ocr(Input::new(&bytes, Format::Pdf), &engine()).unwrap();
    let mut by_hand = converter(1).parse(Input::new(&bytes, Format::Pdf)).unwrap();
    for request in by_hand.ocr_requests(&RasterSpec::default(), None).unwrap() {
        let OcrOrigin::Page(page) = request.origin else { panic!("{request:?}") };
        by_hand.fill_gap(request.gap, GapFill::Markdown(format!("page {page}"))).unwrap();
    }
    assert_eq!(converted.markdown(), by_hand.render().unwrap().markdown());
    assert!(
        converted.markdown().contains("page 2\n\n") && converted.markdown().ends_with("page 5\n")
    );
}

#[test]
fn a_page_s_pixels_are_charged_before_it_is_painted_its_steps_as_it_paints_and_its_png_after() {
    let bytes = page_pdf("/MediaBox [0 0 612 792]");
    let mut unlimited = every_page(&converter(1), &bytes);
    let [request] =
        unlimited.ocr_requests(&RasterSpec::default(), None).unwrap().try_into().unwrap();
    let png = u64::try_from(request.image.bytes.len()).unwrap();
    let (pixels, bytes_charged) = ((1275_u64 * 1650).div_ceil(128), png.div_ceil(16));
    let least = |passes: &dyn Fn(&mut Parsed) -> bool| {
        crate::bounds::least_work(|converter| {
            let input = Input::new(&bytes, Format::Pdf).with_ocr_pages(PageSelection::All);
            converter.parse(input).is_ok_and(|mut parsed| passes(&mut parsed))
        })
    };
    let parse = least(&|_| true);
    let full = least(&|parsed| parsed.ocr_requests(&RasterSpec::default(), None).is_ok());
    let painting = full - parse - pixels - bytes_charged;
    let (operators, glyphs) = (5, 7);
    assert!(painting >= pixels + operators + glyphs, "{painting}");
    for max in [parse + pixels - 1, full - 1] {
        let input = Input::new(&bytes, Format::Pdf).with_ocr_pages(PageSelection::All);
        let mut parsed = limited(|limits| limits.max_work = max).parse(input).unwrap();
        let refused = parsed.ocr_requests(&RasterSpec::default(), None);
        assert!(trips(&refused, Limit::Work, max), "{max}: {refused:?}");
    }
}

fn filled_pages(side: u32, fills: usize, page_count: usize) -> Vec<u8> {
    let kids: Vec<_> = (4..4 + page_count).map(|id| format!("{id} 0 R")).collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        format!("<< /Type /Pages /Kids [{}] /Count {page_count} >>", kids.join(" ")),
        plain(&format!("0 0 1 rg 0 0 {side} {side} re f\n").repeat(fills)),
    ];
    objects.extend((0..page_count).map(|_| {
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {side} {side}] /Contents 3 0 R >>")
    }));
    write_pdf(&objects)
}

#[test]
fn a_page_painting_its_whole_area_twenty_thousand_times_trips_max_work() {
    let max = Limits::default().max_work;
    let mut parsed = every_page(&converter(1), &filled_pages(1450, 20_000, 1));
    let refused = parsed.ocr_requests(&RasterSpec::default(), None);
    assert!(trips(&refused, Limit::Work, max), "{refused:?}");
}

#[test]
fn pages_on_any_thread_count_share_one_paint_budget() {
    let spec = RasterSpec::default();
    let one = filled_pages(72, 100, 1);
    let two = filled_pages(72, 100, 2);
    let one_page_work = crate::bounds::least_work(|converter| {
        let input = Input::new(&one, Format::Pdf).with_ocr_pages(PageSelection::All);
        converter.parse(input).is_ok_and(|mut parsed| parsed.ocr_requests(&spec, None).is_ok())
    });
    let max = one_page_work + one_page_work / 2;
    let images = |threads| requested(&mut every_page(&converter(threads), &two), &spec);
    assert_eq!(images(1), images(4));
    for threads in [1, 4] {
        let bounded = configured(|config| {
            config.threads = threads;
            config.limits.max_work = max;
        });
        let mut parsed = every_page(&bounded, &two);
        let refused = parsed.ocr_requests(&spec, None);
        assert!(trips(&refused, Limit::Work, max), "{threads}: {refused:?}");
    }
}

/// What filling every page of `bytes` reports beside its gaps, run by an engine and by hand.
fn painting_losses(bytes: &[u8]) -> Vec<(DiagnosticCode, Region, Option<u32>, u64)> {
    let input = || Input::new(bytes, Format::Pdf).with_ocr_pages(PageSelection::All);
    let engine = Pages { cancels: None, calls: std::sync::Mutex::default() };
    let mut run = converter(1).parse(input()).unwrap();
    run.ocr(&engine, None).unwrap();
    let mut by_hand = converter(1).parse(input()).unwrap();
    for request in by_hand.ocr_requests(&RasterSpec::default(), None).unwrap() {
        by_hand.fill_gap(request.gap, GapFill::Markdown("page".into())).unwrap();
    }
    assert_eq!(reported(run.diagnostics()), reported(by_hand.diagnostics()));
    let lost = reported(run.diagnostics());
    let rendered = converter(1).convert_with_ocr(input(), &engine);
    let incomplete = matches!(rendered, Err(ConvertError::Incomplete { .. }));
    assert_eq!(incomplete, !lost.is_empty(), "{rendered:?}");
    lost
}

/// A one-page PDF whose content stream is `contents`, over `resources`, with `extra` as objects
/// 5 on.
fn painted(resources: &str, contents: &str, extra: &[&str]) -> Vec<u8> {
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << {resources} >> \
             /Contents 4 0 R >>"
        ),
        contents.to_owned(),
    ];
    objects.extend(extra.iter().map(|object| (*object).to_owned()));
    write_pdf(&objects)
}

fn plain(content: &str) -> String {
    format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len())
}

const NOT_ZLIB: &str = "/Length 10 /Filter /FlateDecode >>\nstream\nnot zlib!!\nendstream";

#[test]
fn a_filled_page_reports_each_kind_its_painting_left_out() {
    let lost = |count| vec![(DiagnosticCode::RasterIncomplete, Region::Body, Some(1), count)];
    let undecodable = painted("", &format!("<< {NOT_ZLIB}"), &[]);
    let jpeg = "<< /Type /XObject /Subtype /Image /Width 8 /Height 8 /ColorSpace /DeviceGray \
        /BitsPerComponent 8 /Filter /DCTDecode /Length 10 >>\nstream\nnot a jpeg\nendstream";
    let image =
        painted("/XObject << /Im1 5 0 R >>", &plain("q 200 0 0 200 9 9 cm /Im1 Do Q"), &[jpeg]);
    let inline = painted("", &plain("q 100 0 0 100 9 9 cm BI /W 1 /H ID x EI Q"), &[]);
    let group = format!(
        "<< /Type /XObject /Subtype /Form /BBox [0 0 612 792] \
         /Group << /S /Transparency /CS /DeviceGray >> {NOT_ZLIB}"
    );
    let masked = painted(
        "/ExtGState << /G1 << /SMask << /Type /Mask /S /Luminosity /G 5 0 R >> >> >>",
        &plain("/G1 gs 0 0 1 rg 100 100 200 200 re f"),
        &[&group],
    );
    for (bytes, expected) in [
        (fixture("pdf/handmade-predefined-cmap.pdf"), lost(10)),
        (undecodable, lost(1)),
        (image, lost(1)),
        (inline, lost(1)),
        (masked, Vec::new()),
        (fixture("pdf/handmade-jpeg-scan.pdf"), Vec::new()),
        (fixture("pdf/handmade-mesh-shading.pdf"), Vec::new()),
    ] {
        assert_eq!(painting_losses(&bytes), expected);
    }
}

#[test]
fn a_fill_reports_its_latest_request_s_painting_and_a_failure_none() {
    let bytes = fixture("pdf/handmade-predefined-cmap.pdf");
    let default = RasterSpec::default();
    let mut filled = every_page(&converter(1), &bytes);
    let [request] = filled.ocr_requests(&default, None).unwrap().try_into().unwrap();
    filled.ocr_requests(&default, None).unwrap();
    filled.fill_gap(request.gap, GapFill::Markdown("page".into())).unwrap();
    let lost = [(DiagnosticCode::RasterIncomplete, Region::Body, Some(1), 10)];
    assert_eq!(reported(filled.diagnostics()), lost);
    let mut failed = every_page(&converter(1), &bytes);
    failed.ocr_requests(&default, None).unwrap();
    failed.fail_gap(request.gap).unwrap();
    let ocr_failed = [(DiagnosticCode::OcrFailed, Region::Body, Some(1), 1)];
    assert_eq!(reported(failed.diagnostics()), ocr_failed);
}
