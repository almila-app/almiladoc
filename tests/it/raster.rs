use std::assert_matches;

use crate::pdf::{painted, plain, write_pdf};
use crate::support::{
    all_requests, bounded, configured, converter, fixture, limited, reported, trips,
};
use almiladoc::model::OcrOrigin;
use almiladoc::{
    CancelToken, ConfigError, ConvertError, Converter, DiagnosticCode, Format, GapFill, Input,
    Limit, Limits, OcrEngine, OcrOutcome, OcrRequest, OcrSummary, PageSelection, Parsed,
    RasterFidelity, RasterSize, RasterSpec, RefusalKind, Region,
};

fn spec(dpi: u32, max_pixels: u64) -> RasterSpec {
    let mut spec = RasterSpec::default();
    (spec.size, spec.max_pixels) = (RasterSize::Dpi(dpi), max_pixels);
    spec
}

fn long_edge(edge: u32) -> RasterSpec {
    let mut spec = RasterSpec::default();
    spec.size = RasterSize::LongEdge(edge);
    spec
}

fn png_size(png: &[u8]) -> (u32, u32) {
    let word = |at: usize| u32::from_be_bytes(png[at..at + 4].try_into().unwrap());
    assert_eq!(&png[12..16], b"IHDR");
    (word(16), word(20))
}

fn requested(parsed: &mut Parsed, spec: &RasterSpec) -> Vec<(u32, Option<u32>, (u32, u32))> {
    all_requests(parsed, spec)
        .iter()
        .map(|request| {
            let OcrOrigin::Page(page) = request.origin else { panic!("{request:?}") };
            assert_eq!(request.image.media_type, "image/png");
            let (width, height) = png_size(&request.image.bytes);
            assert_eq!((request.image.width, request.image.height), (Some(width), Some(height)));
            (page.get(), request.image.dpi, (width, height))
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
        let requests = all_requests(&mut parsed, &default);
        requests.into_iter().map(|request| request.image.bytes).collect::<Vec<_>>()
    };
    let once = pngs(1);
    assert_eq!(once, pngs(1));
    assert_eq!(once, pngs(4));
}

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
fn a_dpi_page_fits_the_cap_by_its_media_box_and_paints_its_crop_box() {
    let cropped = page_pdf("/MediaBox [0 0 3000 3000] /CropBox [0 0 612 792]");
    let found = requested(&mut every_page(&converter(1), &cropped), &RasterSpec::default());
    assert_eq!(found, [(1, Some(86), (731, 946))]);
}

#[test]
fn a_dpi_page_fits_the_cap_by_its_pixels_each_side_rounded_up() {
    let bytes = page_pdf("/MediaBox [0 0 700 900]");
    for (max_pixels, dpi, size) in [
        (1459 * 1875, 150, (1458, 1875)),
        (1459 * 1875 - 1, 149, (1449, 1863)),
        (1458 * 1875, 149, (1449, 1863)),
    ] {
        let found = requested(&mut every_page(&converter(1), &bytes), &spec(150, max_pixels));
        assert_eq!(found, [(1, Some(dpi), size)], "{max_pixels}");
    }
}

#[test]
fn a_parse_sizes_no_page_however_many_await_ocr() {
    let parse = |pages| {
        let bytes = filled_pages(72, 1, pages);
        crate::bounds::least_work(|converter| {
            let input = Input::new(&bytes, Format::Pdf).with_ocr_pages(PageSelection::All);
            converter.parse(input).is_ok()
        })
    };
    let (one, two, three) = (parse(1), parse(2), parse(3));
    assert_eq!(three - two, two - one, "{one} {two} {three}");
}

#[test]
fn a_long_edge_page_has_exactly_that_edge_whatever_its_box_and_rotation() {
    const MEDIA: &str = "/MediaBox [0 0 612 792]";
    const CROP: &str = "/CropBox [0 0 306 792]";
    for (entries, edge, dpi, size) in [
        (MEDIA.to_owned(), 1024, 93, (791, 1024)),
        ("/MediaBox [0 0 792 612]".to_owned(), 1024, 93, (1024, 791)),
        (format!("{MEDIA} /Rotate 90"), 1024, 93, (1024, 791)),
        (format!("{MEDIA} /Rotate 180"), 1024, 93, (791, 1024)),
        (format!("{MEDIA} /Rotate 270"), 1024, 93, (1024, 791)),
        (format!("{MEDIA} {CROP}"), 1024, 93, (396, 1024)),
        (format!("{MEDIA} {CROP} /Rotate 90"), 1024, 93, (1024, 396)),
        ("/MediaBox [0 0 3000 3000] /CropBox [0 0 612 792]".to_owned(), 2048, 186, (1583, 2048)),
        ("/MediaBox [0 0 300 396]".to_owned(), 512, 93, (388, 512)),
        ("/MediaBox [0 0 500 500]".to_owned(), 800, 115, (800, 800)),
        ("/MediaBox [0 0 1 1000]".to_owned(), 64, 5, (1, 64)),
        ("/MediaBox [0 0 0.00000000001 0.0000000000001]".to_owned(), 64, u32::MAX, (64, 1)),
    ] {
        let mut parsed = every_page(&converter(1), &page_pdf(&entries));
        let found = requested(&mut parsed, &long_edge(edge));
        assert_eq!(found, [(1, Some(dpi), size)], "{entries} {edge}");
    }
}

#[test]
fn a_long_edge_over_the_pixel_cap_is_an_over_pixels_refusal_and_is_never_painted_smaller() {
    let bytes = page_pdf("/MediaBox [0 0 612 792]");
    let mut parsed = every_page(&converter(1), &bytes);
    let gap = parsed.gaps().next().unwrap().id;
    let pixels = 4096 * 3165;
    let over = parsed.ocr_requests(&long_edge(4096), None).unwrap();
    let [Err(refusal)] = &over[..] else { panic!("{over:?}") };
    assert_eq!((refusal.gap, refusal.kind), (gap, RefusalKind::OverPixels { pixels }));
    assert_eq!(refusal.origin, OcrOrigin::Page(1.try_into().unwrap()));
    let mut roomy = long_edge(4096);
    roomy.max_pixels = pixels;
    assert_eq!(requested(&mut parsed, &roomy), [(1, Some(372), (3165, 4096))]);
    roomy.max_pixels = pixels - 1;
    let over = parsed.ocr_requests(&roomy, None).unwrap();
    assert_matches!(&over[..], [Err(refusal)] if refusal.kind == RefusalKind::OverPixels { pixels });
}

#[test]
fn an_engine_asking_an_over_pixel_long_edge_reads_nothing_and_the_refusal_paints_nothing() {
    struct Exact(RasterSpec);
    impl OcrEngine for Exact {
        fn raster(&self) -> RasterSpec {
            self.0
        }
        fn recognize(&self, _: &[OcrRequest], _: &CancelToken) -> Vec<OcrOutcome> {
            panic!("a refused page is read by no engine")
        }
    }
    let bytes = page_pdf("/MediaBox [0 0 612 792]");
    let mut parsed = every_page(&converter(1), &bytes);
    let mut expected = OcrSummary::default();
    expected.refused = 1;
    assert_eq!(parsed.ocr(&Exact(long_edge(4096)), None), Ok(expected));
    assert_matches!(parsed.render(), Err(ConvertError::NeedsOcr { pages, .. }) if pages == [1]);
    let work = |ask: Option<RasterSpec>| {
        crate::bounds::least_work(|converter| {
            let input = Input::new(&bytes, Format::Pdf).with_ocr_pages(PageSelection::All);
            let parsed = converter.parse(input);
            parsed.is_ok_and(|mut parsed| {
                ask.is_none_or(|spec| parsed.ocr_requests(&spec, None).is_ok())
            })
        })
    };
    let one_page_lookup = 1;
    assert_eq!(work(Some(long_edge(4096))), work(None) + one_page_lookup);
    assert!(work(Some(RasterSpec::default())) > work(Some(long_edge(4096))));
}

#[test]
fn a_long_edge_request_looks_each_page_up_at_a_unit_per_page_of_the_document() {
    let bytes = filled_pages(72, 1, 3);
    let work = |ask: Option<RasterSpec>| {
        crate::bounds::least_work(|converter| {
            let input = Input::new(&bytes, Format::Pdf).with_ocr_pages(PageSelection::All);
            converter.parse(input).is_ok_and(|mut parsed| {
                ask.is_none_or(|spec| parsed.ocr_requests(&spec, None).is_ok())
            })
        })
    };
    let (lookups, pages) = (3, 3);
    assert_eq!(work(Some(long_edge(16_384))), work(None) + lookups * pages);
}

#[test]
fn a_dpi_page_is_the_page_unpdf_paints_at_that_dpi() {
    use unpdf::parser::raster::{PageRegion, RasterOptions};
    let bytes = fixture("pdf/handmade-five-page.pdf");
    let options = RasterOptions { dpi: 150.0, region: PageRegion::Crop };
    let direct = unpdf::PdfParser::from_bytes(&bytes).unwrap().render_page(2, &options).unwrap();
    let mut parsed = converter(1).parse(Input::new(&bytes, Format::Pdf)).unwrap();
    let requests = all_requests(&mut parsed, &RasterSpec::default());
    assert_eq!(&*requests[0].image.bytes, direct.to_png().as_slice());
}

#[test]
fn a_spec_out_of_range_is_refused_and_changes_nothing() {
    let bytes = fixture("pdf/handmade-five-page.pdf");
    let mut parsed = converter(1).parse(Input::new(&bytes, Format::Pdf)).unwrap();
    for refused in [
        spec(49, 1 << 20),
        spec(601, 1 << 20),
        spec(150, (1 << 20) - 1),
        spec(150, (40 << 20) + 1),
        long_edge(63),
        long_edge(16_385),
    ] {
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
    for request in all_requests(&mut by_hand, &RasterSpec::default()) {
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
    let [request] = all_requests(&mut unlimited, &RasterSpec::default()).try_into().unwrap();
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

fn painting_losses(bytes: &[u8]) -> Vec<(DiagnosticCode, Region, Option<u32>, u64)> {
    let input = || Input::new(bytes, Format::Pdf).with_ocr_pages(PageSelection::All);
    let engine = Pages { cancels: None, calls: std::sync::Mutex::default() };
    let mut run = converter(1).parse(input()).unwrap();
    run.ocr(&engine, None).unwrap();
    let mut by_hand = converter(1).parse(input()).unwrap();
    for request in all_requests(&mut by_hand, &RasterSpec::default()) {
        by_hand.fill_gap(request.gap, GapFill::Markdown("page".into())).unwrap();
    }
    assert_eq!(reported(run.diagnostics()), reported(by_hand.diagnostics()));
    let lost = reported(run.diagnostics());
    let rendered = converter(1).convert_with_ocr(input(), &engine);
    let incomplete = matches!(rendered, Err(ConvertError::Incomplete { .. }));
    assert_eq!(incomplete, !lost.is_empty(), "{rendered:?}");
    lost
}

fn korean_run(placement: &str) -> Vec<u8> {
    let font = "<< /Type /Font /Subtype /Type0 /BaseFont /HYSMyeongJo-Medium /Encoding /Identity-H \
        /DescendantFonts [6 0 R] >>";
    let descendant = "<< /Type /Font /Subtype /CIDFontType0 /BaseFont /HYSMyeongJo-Medium \
        /CIDSystemInfo << /Registry (Adobe) /Ordering (Korea1) /Supplement 2 >> /DW 1000 \
        /FontDescriptor 7 0 R >>";
    let descriptor = "<< /Type /FontDescriptor /FontName /HYSMyeongJo-Medium /Flags 4 \
        /FontBBox [0 -120 1000 880] /ItalicAngle 0 /Ascent 880 /Descent -120 /CapHeight 700 \
        /StemV 80 >>";
    let content = format!("{placement} BT /F1 10 Tf 72 700 Td <0A490001> Tj ET");
    painted("/Font << /F1 5 0 R >>", &plain(&content), &[font, descendant, descriptor])
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
    let uncolored = painted("", &plain("/Unknown cs 0.7 sc 100 100 200 200 re f"), &[]);
    for (bytes, expected, fidelity) in [
        (fixture("pdf/handmade-predefined-cmap.pdf"), lost(10), [10, 0, 0, 0, 0, 0, 0, 0, 0]),
        (korean_run(""), lost(1), [1, 0, 0, 0, 0, 0, 0, 0, 0]),
        (korean_run("0 600 612 192 re W n"), lost(1), [1, 0, 0, 0, 0, 0, 0, 0, 0]),
        (korean_run("0 0 10 10 re W n"), Vec::new(), [0; 9]),
        (korean_run("1 0 0 1 1000 0 cm"), Vec::new(), [0; 9]),
        (undecodable, lost(1), [0, 0, 0, 0, 0, 1, 0, 0, 0]),
        (image, lost(1), [0, 1, 0, 0, 0, 0, 0, 0, 0]),
        (inline, lost(1), [0, 0, 1, 0, 0, 0, 0, 0, 0]),
        (masked, Vec::new(), [0, 0, 0, 0, 1, 0, 0, 0, 0]),
        (fixture("pdf/handmade-jpeg-scan.pdf"), Vec::new(), [0; 9]),
        (fixture("pdf/handmade-mesh-shading.pdf"), Vec::new(), [0, 0, 0, 1, 0, 0, 0, 0, 0]),
        (page_pdf("/MediaBox [0 0 612 792]"), Vec::new(), [0, 0, 0, 0, 0, 0, 1, 0, 0]),
        (uncolored, Vec::new(), [0, 0, 0, 0, 0, 0, 0, 1, 0]),
        (highlighted(), Vec::new(), [0, 0, 0, 0, 0, 0, 0, 0, 1]),
    ] {
        assert_eq!(painting_losses(&bytes), expected);
        assert_eq!(fidelity_of(&bytes), fidelity);
    }
}

fn highlighted() -> Vec<u8> {
    let form = |entries: &str, body: &str| {
        let entries = format!("/Type /XObject /Subtype /Form /BBox [72 695 272 725] {entries}");
        format!("<< {entries} /Length {} >>\nstream\n{body}\nendstream", body.len())
    };
    let resources = "/ExtGState << /G << /BM /Multiply >> >> /XObject << /Fm 6 0 R >>";
    write_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
         /Annots [<< /Subtype /Highlight /Rect [72 695 272 725] /AP << /N 5 0 R >> >>] >>"
            .to_owned(),
        plain("0 0 1 rg 72 700 200 20 re f"),
        form(&format!("/Resources << {resources} >>"), "/G gs /Fm Do"),
        form("/Group << /S /Transparency /I false >>", "1 1 0 rg 72 695 200 30 re f"),
    ])
}

fn fidelity_of(bytes: &[u8]) -> [u32; 9] {
    let mut parsed = every_page(&converter(1), bytes);
    let [request] = all_requests(&mut parsed, &RasterSpec::default()).try_into().unwrap();
    let f = request.image.fidelity.unwrap();
    [
        f.text_runs,
        f.images,
        f.inline_images,
        f.shadings,
        f.soft_masks,
        f.content_streams,
        f.substituted_text_runs,
        f.colors,
        f.annotations,
    ]
}

#[test]
fn a_fill_reports_its_latest_request_s_painting_and_a_failure_none() {
    let bytes = fixture("pdf/handmade-predefined-cmap.pdf");
    let default = RasterSpec::default();
    let mut filled = every_page(&converter(1), &bytes);
    let [request] = all_requests(&mut filled, &default).try_into().unwrap();
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

fn unpaintable_second_page() -> Vec<u8> {
    let content = plain("BT /F1 24 Tf 72 400 Td (Painted) Tj ET");
    write_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources \
         << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> >>"
            .to_owned(),
        content,
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 0 0] /Contents 99 0 R >>".to_owned(),
    ])
}

#[test]
fn a_gap_page_that_cannot_be_painted_is_a_typed_refusal_in_its_place() {
    let mut parsed = every_page(&converter(1), &unpaintable_second_page());
    let made = parsed.ocr_requests(&RasterSpec::default(), None).unwrap();
    let gaps: Vec<_> = parsed.gaps().cloned().collect();
    assert_eq!(gaps.len(), 2);
    assert_eq!(made.len(), 2);
    assert_matches!(&made[0], Ok(request) if request.gap == gaps[0].id);
    let refusal = made[1].as_ref().unwrap_err();
    assert_eq!(refusal.gap, gaps[1].id);
    assert_eq!(refusal.origin, OcrOrigin::Page(2.try_into().unwrap()));
    assert_eq!(refusal.reasons, gaps[1].reasons);
    assert_eq!(refusal.kind, RefusalKind::Unpaintable);
}

#[test]
fn an_engine_never_sees_a_refused_page_which_stays_a_gap_a_later_fill_settles() {
    let mut parsed = every_page(&converter(1), &unpaintable_second_page());
    let second = parsed.gaps().map(|gap| gap.id).nth(1).unwrap();
    let engine = Pages { cancels: None, calls: std::sync::Mutex::default() };
    let mut expected = OcrSummary::default();
    (expected.filled, expected.refused) = (1, 1);
    let summary = parsed.ocr(&engine, None).unwrap();
    assert_eq!(summary, expected);
    assert_eq!(
        summary.filled + summary.failed + summary.unattempted + summary.refused,
        parsed.gaps().count()
    );
    assert_eq!(*engine.calls.lock().unwrap(), [1]);
    let gap = [(DiagnosticCode::OcrGap, Region::Body, Some(2), 1)];
    assert_eq!(reported(parsed.diagnostics()), gap);
    assert_matches!(
        parsed.render(),
        Err(ConvertError::NeedsOcr { pages, page_count: 2, .. }) if pages == [2]
    );
    parsed.fill_gap(second, GapFill::Markdown("later".into())).unwrap();
    assert_eq!(parsed.render().unwrap().markdown(), "page 1\n\nlater\n");
}

#[test]
fn a_png_over_the_byte_bound_is_refused_after_its_work_is_charged_and_one_at_it_is_not() {
    let bytes = page_pdf("/MediaBox [0 0 612 792]");
    let mut parsed = every_page(&converter(1), &bytes);
    let unbounded = parsed.ocr_requests(&RasterSpec::default(), None).unwrap();
    let [Ok(request)] = &unbounded[..] else { panic!("{unbounded:?}") };
    let len = u64::try_from(request.image.bytes.len()).unwrap();
    assert_eq!(parsed.ocr_requests(&bounded(len), None).unwrap(), unbounded);
    let over = parsed.ocr_requests(&bounded(len - 1), None).unwrap();
    let [Err(refusal)] = &over[..] else { panic!("{over:?}") };
    assert_eq!(refusal.kind, RefusalKind::OverBytes { bytes: len });
    assert_eq!(refusal.gap, request.gap);
    let work = |spec: RasterSpec| {
        crate::bounds::least_work(|converter| {
            let input = Input::new(&bytes, Format::Pdf).with_ocr_pages(PageSelection::All);
            converter.parse(input).is_ok_and(|mut parsed| parsed.ocr_requests(&spec, None).is_ok())
        })
    };
    assert_eq!(work(bounded(len - 1)), work(RasterSpec::default()));
}

#[test]
fn a_gap_refused_after_a_painted_request_reports_none_of_the_earlier_painting() {
    let bytes = fixture("pdf/handmade-predefined-cmap.pdf");
    let mut parsed = every_page(&converter(1), &bytes);
    let [request] = all_requests(&mut parsed, &RasterSpec::default()).try_into().unwrap();
    assert_eq!(request.image.fidelity.map(RasterFidelity::lost), Some(10));
    let refused = parsed.ocr_requests(&bounded(1), None).unwrap();
    assert_matches!(&refused[..], [Err(refusal)] if refusal.gap == request.gap);
    parsed.fill_gap(request.gap, GapFill::Markdown("page".into())).unwrap();
    assert_eq!(reported(parsed.diagnostics()), Vec::new());
}

#[test]
fn what_painting_does_not_observe_is_named_wherever_fidelity_is_documented() {
    const UNOBSERVED: [&str; 2] = [
        "an annotation without an appearance stream, or whose `/as` selects none, is neither \
         painted nor counted",
        "nozoom/norotate are not applied",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for path in [
        "src/ocr/raster.rs",
        "python/src/ocr.rs",
        "python/almiladoc/_almiladoc.pyi",
        "docs/support.md",
        "docs/adr/0002-ocr-port.md",
    ] {
        let text = std::fs::read_to_string(root.join(path)).unwrap().to_lowercase();
        let words: Vec<_> = text.split_whitespace().filter(|word| *word != "///").collect();
        let text = words.join(" ");
        for unobserved in UNOBSERVED {
            assert!(text.contains(unobserved), "{path}: {unobserved}");
        }
    }
}
