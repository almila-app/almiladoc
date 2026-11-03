use std::assert_matches;
use std::fmt::Write as _;
use std::num::NonZeroU32;

use crate::bounds::least_work;
use crate::build::docx;
use crate::support::{configured, converter, corpus, fixture, limited, reported, reporting, trips};
use almiladoc::model::{GapReason, OcrOrigin, UnitKind};
use almiladoc::{
    ConvertError, Converter, DiagnosticCode, Diagnostics, Format, GapFill, HeaderFooterPolicy,
    Input, Limit, LossClass, OnLoss, PageSelection, PageVerdict, PdfScanPolicy, Region,
    UnsupportedKind,
};

const VERDICTS: &[(&str, &[&str])] = &[
    ("handmade-blank", &["no_text"]),
    ("handmade-blank-state", &["no_text"]),
    ("handmade-header-only", &["text", "text", "text"]),
    ("handmade-image-only", &["needs_ocr image_backed"]),
    ("handmade-jpeg-scan", &["needs_ocr image_backed"]),
    ("handmade-mesh-shading", &["no_text"]),
    ("handmade-invisible-mode3-scan", &["needs_ocr invisible_text"]),
    ("handmade-invisible-mode3-alone", &["needs_ocr invisible_text"]),
    ("handmade-invisible-clipped", &["needs_ocr invisible_text"]),
    ("handmade-invisible-clipped-scan", &["needs_ocr invisible_text image_backed"]),
    ("handmade-inkless-spaces", &["no_text"]),
    ("handmade-inkless-spaces-scan", &["needs_ocr image_backed"]),
    ("handmade-inkless-mode3-scan", &["needs_ocr invisible_text"]),
    ("handmade-sparse-visible", &["text"]),
    ("handmade-stamped-scan", &["needs_ocr image_backed"; 3]),
    ("handmade-stamped-scan-single", &["needs_ocr image_backed"]),
    ("handmade-stamped-scan-hires", &["needs_ocr image_backed"; 3]),
    ("handmade-stamped-scan-single-hires", &["needs_ocr image_backed"]),
    ("handmade-partial-scan-margin", &["needs_ocr image_backed"]),
    ("handmade-partial-scan-stamped", &["needs_ocr image_backed"; 3]),
    ("handmade-plate-folio", &["needs_ocr image_backed"; 3]),
    ("handmade-figure-plates", &["text"; 5]),
    ("handmade-chapter-dividers", &["text"; 3]),
    ("handmade-logo-header", &["text"; 3]),
    ("handmade-midpage-stamped-scan", &["text"]),
    ("handmade-strip-scan-stamped", &["text"]),
    ("handmade-clipped-scan-text", &["text"]),
    ("handmade-zero-area-crop-scan", &["text"; 3]),
    ("handmade-scan-running-head", &["text"; 3]),
    ("handmade-tiled-scan", &["needs_ocr image_backed"]),
    ("handmade-tiled-scan-layer", &["needs_ocr invisible_text"]),
    (
        "handmade-five-page",
        &["text", "needs_ocr image_backed", "text", "text", "needs_ocr image_backed"],
    ),
    (
        "handmade-five-page-garbled",
        &["text", "needs_ocr image_backed", "text", "text", "needs_ocr image_backed"],
    ),
    ("handmade-garbled-majority", &["text", "needs_ocr garbled_text", "text"]),
    ("handmade-garbled-unmapped", &["text"]),
    ("handmade-vector-glyphs", &["needs_ocr vector_drawn"]),
    ("handmade-figure-cover-hires", &["text"]),
    ("handmade-figure-large", &["text"]),
    ("handmade-figure-cover", &["text"]),
    ("handmade-census-cover", &["text"]),
    ("handmade-mixed", &["text", "needs_ocr image_backed"]),
    ("handmade-scanned", &["needs_ocr image_backed"; 2]),
    ("handmade-inline-image-no-colorspace", &["needs_ocr image_backed"]),
    ("handmade-ligatures", &["text"]),
    ("handmade-arabic", &["text"]),
    ("handmade-predefined-cmap", &["text"]),
    ("handmade-legacy-cmap", &["text"]),
    ("text", &["text", "text"]),
];

const COVERAGE_FIRST: &[(&str, &[&str])] = &[
    ("handmade-invisible-mode3-scan", &["needs_ocr invisible_text image_backed"]),
    ("handmade-inkless-mode3-scan", &["needs_ocr invisible_text image_backed"]),
    ("handmade-midpage-stamped-scan", &["needs_ocr image_backed"]),
    ("handmade-strip-scan-stamped", &["needs_ocr image_backed"]),
    ("handmade-zero-area-crop-scan", &["needs_ocr image_backed"; 3]),
    ("handmade-scan-running-head", &["text", "text", "needs_ocr image_backed"]),
    (
        "handmade-figure-plates",
        &["text", "text", "text", "needs_ocr image_backed", "needs_ocr image_backed"],
    ),
    ("handmade-chapter-dividers", &["needs_ocr image_backed"; 3]),
    ("handmade-tiled-scan-layer", &["needs_ocr invisible_text image_backed"]),
    ("handmade-figure-cover-hires", &["needs_ocr image_backed"]),
    ("handmade-figure-cover", &["needs_ocr image_backed"]),
    ("handmade-census-cover", &["needs_ocr image_backed"]),
];

fn pdf_fixture(name: &str) -> Vec<u8> {
    fixture(&format!("pdf/{name}.pdf"))
}

fn verdicts(bytes: &[u8]) -> Vec<String> {
    verdicts_by(&converter(1), bytes)
}

fn verdicts_by(converter: &Converter, bytes: &[u8]) -> Vec<String> {
    let inspection = converter.inspect(Input::new(bytes, Format::Pdf)).unwrap();
    inspection
        .pages()
        .iter()
        .map(|page| match &page.verdict {
            PageVerdict::NeedsOcr { reasons } => reasons
                .iter()
                .fold(String::from("needs_ocr"), |out, reason| out + " " + reason.as_str()),
            verdict => verdict.as_str().to_string(),
        })
        .collect()
}

#[test]
fn every_page_of_every_case_has_its_pinned_verdict() {
    for (name, expected) in VERDICTS {
        assert_eq!(verdicts(&pdf_fixture(name)), *expected, "{name}");
    }
    let pinned = corpus().into_iter().filter(|path| path.extension().is_some_and(|e| e == "pdf"));
    assert_eq!(pinned.count(), VERDICTS.len(), "every PDF fixture has its verdicts pinned");
}

fn coverage_first(on_loss: OnLoss) -> Converter {
    configured(|config| {
        config.pdf_scans = PdfScanPolicy::CoverageFirst;
        config.on_loss = on_loss;
    })
}

#[test]
fn coverage_first_flags_pages_half_covered_by_images_and_leaves_every_other_verdict() {
    let pinned = |name| VERDICTS.iter().any(|(pinned, _)| *pinned == name);
    assert!(COVERAGE_FIRST.iter().all(|(name, _)| pinned(*name)));
    let converter = coverage_first(OnLoss::Fail);
    for (name, default) in VERDICTS {
        let expected = COVERAGE_FIRST
            .iter()
            .find_map(|(flagged, verdicts)| (flagged == name).then_some(*verdicts))
            .unwrap_or(default);
        assert_eq!(verdicts_by(&converter, &pdf_fixture(name)), expected, "{name}");
    }
}

#[test]
fn coverage_first_replaces_the_native_text_of_a_scanned_page_with_a_gap() {
    let bytes = pdf_fixture("handmade-midpage-stamped-scan");
    let input = || Input::new(&bytes, Format::Pdf);
    let gap = coverage_first(OnLoss::Report).convert(input()).unwrap();
    assert_eq!(gap.markdown(), "<!-- needs-ocr page 1 -->\n");
    assert_eq!(rows(gap.diagnostics()), [(DiagnosticCode::OcrGap, Some(1))]);
    let error = coverage_first(OnLoss::Fail).convert(input()).unwrap_err();
    assert_matches!(&error, ConvertError::NeedsOcr { pages, page_count: 1, .. } if pages == &[1]);
}

#[test]
fn a_selected_page_keeps_its_coverage_first_reason_before_the_callers() {
    use GapReason::{CallerRequested as Asked, ImageBacked as Scanned};
    let bytes = pdf_fixture("handmade-figure-plates");
    let pages = selecting(&[1, 4]);
    let inspection = coverage_first(OnLoss::Fail).inspect(pdf_input(&bytes, &pages)).unwrap();
    let needs: Vec<_> = inspection
        .pages()
        .iter()
        .filter_map(|page| match &page.verdict {
            PageVerdict::NeedsOcr { reasons } => Some((page.page.get(), reasons.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(needs, [(1, vec![Asked]), (4, vec![Scanned, Asked]), (5, vec![Scanned])]);
}

#[test]
fn running_heads_are_judged_across_the_pages_coverage_first_sends_to_ocr() {
    let bytes = pdf_fixture("handmade-scan-running-head");
    let conversion =
        coverage_first(OnLoss::Report).convert(Input::new(&bytes, Format::Pdf)).unwrap();
    assert_eq!(
        conversion.markdown(),
        "Interviews were transcribed and coded by two raters working independently, and \
         disagreements were settled by discussion.\n\n\
         The coding frame followed the categories of the pilot study, with two additions that \
         the pilot had not anticipated at all.\n\n\
         <!-- needs-ocr page 3 -->\n"
    );
    let codes: Vec<_> = conversion
        .diagnostics()
        .iter()
        .map(|d| (d.code, d.unit.map(NonZeroU32::get), d.count))
        .collect();
    assert_eq!(
        codes,
        [
            (DiagnosticCode::OcrGap, Some(3), 1),
            (DiagnosticCode::HeaderOmitted, Some(1), 1),
            (DiagnosticCode::FooterOmitted, Some(1), 1),
        ]
    );
}

#[test]
fn unfilled_gaps_fail_needs_ocr_before_a_loss_fails_incomplete() {
    let bytes = pdf_fixture("handmade-five-page-garbled");
    let error = converter(1).convert(Input::new(&bytes, Format::Pdf)).unwrap_err();
    assert_matches!(&error, ConvertError::NeedsOcr { pages, page_count: 5, .. } if pages == &[2, 5]);
    let parsed = converter(1).parse(Input::new(&bytes, Format::Pdf)).unwrap();
    assert!(parsed.diagnostics().has_loss());
    assert_eq!(parsed.render().unwrap_err(), error);

    let reported = reporting().convert(Input::new(&bytes, Format::Pdf)).unwrap();
    assert_eq!(reported.markdown(), FIVE_PAGES);
    assert!(!reported.is_complete());
    let rows: Vec<_> = reported
        .diagnostics()
        .iter()
        .map(|d| (d.code, d.class, d.unit.map(NonZeroU32::get), d.count))
        .collect();
    assert_eq!(
        rows,
        [
            (DiagnosticCode::OcrGap, LossClass::Deferred, Some(2), 1),
            (DiagnosticCode::OcrGap, LossClass::Deferred, Some(5), 1),
            (DiagnosticCode::GarbledFontEncoding, LossClass::Lost, Some(3), 1),
        ]
    );
}

const FIVE_PAGES: &str = "Text on the first page\n\n<!-- needs-ocr page 2 -->\n\n\
                          Text on the first page\n\nText on the first page\n\n\
                          <!-- needs-ocr page 5 -->\n";

const GARBLED_LOSS: (DiagnosticCode, Region, Option<u32>, u64) =
    (DiagnosticCode::GarbledFontEncoding, Region::Body, Some(1), 1);

#[test]
fn a_mostly_garbled_page_awaits_ocr_before_a_page_with_a_few_garbled_words_fails_incomplete() {
    let bytes = pdf_fixture("handmade-garbled-majority");
    let error = converter(1).convert(Input::new(&bytes, Format::Pdf)).unwrap_err();
    assert_matches!(&error, ConvertError::NeedsOcr { pages, page_count: 3, .. } if pages == &[2]);

    let mut parsed = converter(1).parse(Input::new(&bytes, Format::Pdf)).unwrap();
    let gaps: Vec<_> = parsed.gaps().map(|gap| (gap.id, gap.reasons.clone())).collect();
    let [(id, reasons)] = gaps.as_slice() else { panic!("{gaps:?}") };
    assert_eq!(reasons, &[GapReason::GarbledText]);
    parsed.fill_gap(*id, GapFill::Markdown("Appendix tables".into())).unwrap();
    let Err(ConvertError::Incomplete { diagnostics, .. }) = parsed.render() else {
        panic!("the page below the share passed fail");
    };
    assert_eq!(reported(&diagnostics), [GARBLED_LOSS]);
}

#[test]
fn an_unfilled_garbled_gap_under_report_drops_its_readable_text_and_reports_only_the_gap() {
    let bytes = pdf_fixture("handmade-garbled-majority");
    let conversion = reporting().convert(Input::new(&bytes, Format::Pdf)).unwrap();
    let markdown = conversion.markdown();
    assert!(markdown.contains("<!-- needs-ocr page 2 -->"), "{markdown}");
    assert!(!markdown.contains("Appendix"), "{markdown}");
    assert!(markdown.starts_with("Interviews were transcribed"), "{markdown}");
    assert!(markdown.ends_with("Text on the first page\n"), "{markdown}");
    let gap = (DiagnosticCode::OcrGap, Region::Body, Some(2), 1);
    assert_eq!(reported(conversion.diagnostics()), [gap, GARBLED_LOSS]);
    let classes: Vec<_> = conversion.diagnostics().iter().map(|d| d.class).collect();
    assert_eq!(classes, [LossClass::Deferred, LossClass::Lost]);
}

#[test]
fn unmapped_codes_whose_glyphs_show_neither_gap_a_page_nor_cost_a_loss() {
    let bytes = pdf_fixture("handmade-garbled-unmapped");
    let conversion = converter(1).convert(Input::new(&bytes, Format::Pdf)).unwrap();
    assert!(conversion.markdown().starts_with("Interviews were transcribed"));
    assert_eq!(reported(conversion.diagnostics()), []);
}

#[test]
fn a_page_of_text_awaiting_no_ocr_beside_gaps_is_kept_whole_and_incomplete() {
    let bytes = pdf_fixture("handmade-five-page");
    let reported = reporting().convert(Input::new(&bytes, Format::Pdf)).unwrap();
    assert_eq!(reported.markdown(), FIVE_PAGES);
    assert!(!reported.is_complete());
    let parsed = reporting().parse(Input::new(&bytes, Format::Pdf)).unwrap();
    assert!(!parsed.render().unwrap().is_complete());
    let text = reporting().convert(Input::new(&pdf_fixture("handmade-blank"), Format::Pdf));
    assert!(text.unwrap().is_complete());
}

#[test]
fn every_page_is_a_unit_in_order_and_inspect_agrees_with_the_parse() {
    let bytes = pdf_fixture("handmade-five-page");
    let conversion = reporting().convert(Input::new(&bytes, Format::Pdf)).unwrap();
    let units: Vec<_> =
        conversion.units().iter().map(|span| (span.unit.kind, span.unit.ordinal.get())).collect();
    assert_eq!(units, (1..=5).map(|n| (UnitKind::Page, n)).collect::<Vec<_>>());
    let page_three = &conversion.units()[2].bytes;
    assert_eq!(conversion.markdown().get(page_three.clone()), Some("Text on the first page"));

    let parsed = reporting().parse(Input::new(&bytes, Format::Pdf)).unwrap();
    let gaps: Vec<_> = parsed
        .gaps()
        .map(|gap| {
            (
                gap.id.unit().unwrap().ordinal.get(),
                gap.id.ordinal().get(),
                gap.origin,
                gap.reasons.clone(),
            )
        })
        .collect();
    let page = |n| OcrOrigin::Page(NonZeroU32::new(n).unwrap());
    let scanned = || vec![GapReason::ImageBacked];
    assert_eq!(gaps, [(2, 1, page(2), scanned()), (5, 1, page(5), scanned())]);

    let inspection = reporting().inspect(Input::new(&bytes, Format::Pdf)).unwrap();
    assert_eq!(inspection.page_count(), 5);
    assert_eq!(inspection.diagnostics(), parsed.diagnostics());
    for (inspected, (kind, ordinal)) in inspection.pages().iter().zip(units) {
        assert_eq!((UnitKind::Page, inspected.page.get()), (kind, ordinal));
        let gap = parsed.gaps().find(|gap| gap.origin == OcrOrigin::Page(inspected.page));
        match (&inspected.verdict, gap) {
            (PageVerdict::NeedsOcr { reasons }, Some(gap)) => assert_eq!(reasons, &gap.reasons),
            (PageVerdict::Text, None) => {}
            other => panic!("page {ordinal}: {other:?}"),
        }
    }
}

fn selecting(pages: &[u32]) -> PageSelection {
    PageSelection::Pages(pages.iter().map(|&page| NonZeroU32::new(page).unwrap()).collect())
}

fn pdf_input<'a>(bytes: &'a [u8], pages: &PageSelection) -> Input<'a> {
    Input::new(bytes, Format::Pdf).with_ocr_pages(pages.clone())
}

fn rows(diagnostics: &Diagnostics) -> Vec<(DiagnosticCode, Option<u32>)> {
    diagnostics.iter().map(|d| (d.code, d.unit.map(NonZeroU32::get))).collect()
}

#[test]
fn a_selected_page_becomes_a_gap_whatever_its_text_and_keeps_its_own_reasons() {
    use GapReason::{CallerRequested as Asked, ImageBacked as Scanned};
    let bytes = pdf_fixture("handmade-five-page");
    type Gaps<'a> = &'a [(u32, &'a [GapReason])];
    let cases: [(PageSelection, Gaps<'_>, &str); 3] = [
        (
            PageSelection::All,
            &[
                (1, &[Asked]),
                (2, &[Scanned, Asked]),
                (3, &[Asked]),
                (4, &[Asked]),
                (5, &[Scanned, Asked]),
            ],
            "<!-- needs-ocr page 1 -->\n\n<!-- needs-ocr page 2 -->\n\n<!-- needs-ocr page 3 -->\n\n\
             <!-- needs-ocr page 4 -->\n\n<!-- needs-ocr page 5 -->\n",
        ),
        (
            selecting(&[1, 3]),
            &[(1, &[Asked]), (2, &[Scanned]), (3, &[Asked]), (5, &[Scanned])],
            "<!-- needs-ocr page 1 -->\n\n<!-- needs-ocr page 2 -->\n\n<!-- needs-ocr page 3 -->\n\n\
             Text on the first page\n\n<!-- needs-ocr page 5 -->\n",
        ),
        (selecting(&[5]), &[(2, &[Scanned]), (5, &[Scanned, Asked])], FIVE_PAGES),
    ];
    for (pages, gaps, markdown) in cases {
        let parsed = reporting().parse(pdf_input(&bytes, &pages)).unwrap();
        let found: Vec<_> = parsed
            .gaps()
            .map(|gap| (gap.id.unit().unwrap().ordinal.get(), gap.reasons.clone()))
            .collect();
        let expected: Vec<_> = gaps.iter().map(|(page, why)| (*page, why.to_vec())).collect();
        assert_eq!(found, expected, "{pages:?}");
        let gap_units = gaps.iter().map(|(page, _)| (DiagnosticCode::OcrGap, Some(*page)));
        assert_eq!(rows(parsed.diagnostics()), gap_units.collect::<Vec<_>>(), "{pages:?}");
        let conversion = reporting().convert(pdf_input(&bytes, &pages)).unwrap();
        assert_eq!(conversion.markdown(), markdown, "{pages:?}");

        let inspection = reporting().inspect(pdf_input(&bytes, &pages)).unwrap();
        let needs: Vec<_> = inspection
            .pages()
            .iter()
            .filter_map(|page| match &page.verdict {
                PageVerdict::NeedsOcr { reasons } => Some((page.page.get(), reasons.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(needs, expected, "{pages:?}");
        assert_eq!(inspection.diagnostics(), parsed.diagnostics(), "{pages:?}");
    }
}

#[test]
fn a_selected_page_discards_its_text_without_reporting_its_loss() {
    let bytes = pdf_fixture("handmade-five-page-garbled");
    let parsed = reporting().parse(pdf_input(&bytes, &selecting(&[3]))).unwrap();
    let gaps = [2, 3, 5].map(|page| (DiagnosticCode::OcrGap, Some(page)));
    assert_eq!(rows(parsed.diagnostics()), gaps);
    let error = converter(1).convert(pdf_input(&bytes, &selecting(&[3]))).unwrap_err();
    assert_matches!(&error, ConvertError::NeedsOcr { pages, page_count: 5, .. } if pages == &[2, 3, 5]);
}

#[test]
fn a_selected_blank_page_becomes_a_gap() {
    let bytes = pdf_fixture("handmade-blank");
    let parsed = reporting().parse(pdf_input(&bytes, &PageSelection::All)).unwrap();
    let found: Vec<_> = parsed.gaps().map(|gap| gap.reasons.clone()).collect();
    assert_eq!(found, [vec![GapReason::CallerRequested]]);
    let reported = reporting().convert(pdf_input(&bytes, &PageSelection::All)).unwrap();
    assert!(reported.markdown().contains("<!-- needs-ocr page 1 -->"), "{}", reported.markdown());
    let error = converter(1).convert(pdf_input(&bytes, &PageSelection::All)).unwrap_err();
    assert_matches!(&error, ConvertError::NeedsOcr { pages, page_count: 1, .. } if pages == &[1]);
}

#[test]
fn a_page_past_the_last_is_refused_before_any_page_is_read() {
    fn refused<T: std::fmt::Debug>(result: Result<T, ConvertError>) {
        let error = result.unwrap_err();
        assert_eq!(error.code(), "page_out_of_range");
        assert_matches!(error, ConvertError::PageOutOfRange { page: 6, page_count: 5, .. });
    }
    let bytes = pdf_fixture("handmade-five-page");
    let past = selecting(&[2, 6]);
    refused(converter(1).convert(pdf_input(&bytes, &past)));
    refused(converter(1).inspect(pdf_input(&bytes, &past)));
    refused(converter(1).parse(pdf_input(&bytes, &past)));
    refused(limited(|limits| limits.max_work = 2).parse(pdf_input(&bytes, &past)));
}

#[test]
fn other_formats_ignore_the_selection() {
    let csv = b"name,year\nAda,1843\n";
    let input = Input::new(csv, Format::Csv).with_ocr_pages(selecting(&[7]));
    assert_eq!(
        converter(1).convert(input).unwrap().markdown(),
        "| name | year |\n| --- | --- |\n| Ada | 1843 |\n"
    );
}

#[test]
fn only_a_pdf_is_inspected() {
    let bytes = docx("<w:p><w:r><w:t>text</w:t></w:r></w:p>");
    let error = converter(1).inspect(Input::new(&bytes, Format::Docx)).unwrap_err();
    assert_eq!(error, ConvertError::from(UnsupportedKind::NotInspectable));
    let auto = converter(1).inspect(Input::auto(&bytes, None)).unwrap_err();
    assert_eq!(auto, error);
}

#[test]
fn a_lowered_max_pages_refuses_the_document_before_any_page() {
    let bytes = pdf_fixture("handmade-five-page");
    let converter = limited(|limits| limits.max_pages = 4);
    assert!(trips(&converter.convert(Input::new(&bytes, Format::Pdf)), Limit::Pages, 4));
    assert!(trips(&converter.inspect(Input::new(&bytes, Format::Pdf)), Limit::Pages, 4));
    limited(|limits| limits.max_pages = 5).convert(Input::new(&bytes, Format::Pdf)).unwrap();
    let starved = limited(|limits| (limits.max_pages, limits.max_work) = (4, 2));
    assert!(trips(&starved.parse(Input::new(&bytes, Format::Pdf)), Limit::Pages, 4));
}

#[test]
fn a_pdf_without_pages_is_malformed() {
    let error = converter(1).convert(Input::new(&pdf_pages(&[]), Format::Pdf)).unwrap_err();
    assert_matches!(error, ConvertError::Malformed { part: None, reason: "PDF has no pages", .. });
}

#[test]
fn each_page_its_blocks_and_their_text_charge_work() {
    let least = |pages: &[&str]| {
        let bytes = pdf_pages(pages);
        least_work(|converter| converter.parse(Input::new(&bytes, Format::Pdf)).is_ok())
    };
    let text = "BT /F1 12 Tf 72 700 Td (Text) Tj ET";
    assert_eq!(least(&[text, ""]) - least(&[text]), 1);
    let more = "BT /F1 12 Tf 72 700 Td (More) Tj ET";
    assert_eq!(least(&[text, more]) - least(&[text, ""]), 1 + 4 + 1 + 4 + 1);
}

#[test]
fn a_figure_on_a_text_page_reports_its_omission_and_passes_fail() {
    let bytes = pdf_fixture("handmade-figure-large");
    let conversion = converter(1).convert(Input::new(&bytes, Format::Pdf)).unwrap();
    assert!(conversion.markdown().ends_with("Figure 1. Coding agreement by round and rater.\n"));
    let codes: Vec<_> = conversion
        .diagnostics()
        .iter()
        .map(|d| (d.code, d.unit.map(NonZeroU32::get), d.count))
        .collect();
    assert_eq!(codes, [(DiagnosticCode::ImageOmitted, Some(1), 1)]);
}

#[test]
fn a_page_holding_only_running_heads_and_folios_is_text_without_them() {
    let bytes = pdf_fixture("handmade-header-only");
    let conversion = converter(1).convert(Input::new(&bytes, Format::Pdf)).unwrap();
    let markdown = conversion.markdown();
    assert!(!markdown.contains("Journal"), "{markdown}");
    assert_eq!(markdown.matches("Interviews were transcribed").count(), 2, "{markdown}");
    let spans: Vec<_> = conversion.units().iter().map(|span| span.bytes.len()).collect();
    assert!(spans[0] > 0 && spans[1] == 0 && spans[2] > 0, "{spans:?}");
}

#[test]
fn running_heads_are_judged_across_selected_pages_too() {
    let bytes = pdf_fixture("handmade-header-only");
    let conversion = reporting().convert(pdf_input(&bytes, &selecting(&[1]))).unwrap();
    let markdown = conversion.markdown();
    assert!(markdown.starts_with("<!-- needs-ocr page 1 -->\n\nInterviews were transcribed"));
    assert!(!markdown.contains("Journal"), "{markdown}");
    assert_eq!(markdown.matches("Interviews were transcribed").count(), 1, "{markdown}");
    let omitted = [DiagnosticCode::HeaderOmitted, DiagnosticCode::FooterOmitted]
        .into_iter()
        .flat_map(|code| [2, 3].map(|page| (code, Some(page))));
    assert_eq!(
        rows(conversion.diagnostics()),
        [(DiagnosticCode::OcrGap, Some(1))].into_iter().chain(omitted).collect::<Vec<_>>()
    );
}

#[test]
fn a_heading_or_number_that_changes_from_page_to_page_is_kept() {
    let page = |(head, size, body): &(&str, u32, &str)| {
        format!(
            "BT /F1 {size} Tf 72 720 Td ({head}) Tj ET\n\
             BT /F1 11 Tf 72 680 Td ({body}) Tj ET\nBT /F1 11 Tf 72 660 Td ({body} again) Tj ET"
        )
    };
    let cases: [&[(&str, u32, &str)]; 7] = [
        &[("Chapter 1", 24, "Opening words"), ("Chapter 2", 24, "Later words")],
        &[("Revenue 100", 11, "Opening words"), ("Revenue 250", 11, "Later words")],
        &[
            ("Chapter 1", 11, "Opening words"),
            ("Interlude", 11, "Middle words"),
            ("Chapter 2", 11, "Later words"),
        ],
        &[("Chapter 1", 11, "Opening words"), ("Chapter 2", 11, "Later words")],
        &[("Table 1", 11, "Opening words"), ("Table 2", 11, "Later words")],
        &[
            ("Question 1", 11, "Opening words"),
            ("Question 2", 11, "Middle words"),
            ("Question 3", 11, "Later words"),
        ],
        &[("Participant ID", 11, "Opening words"), ("Participant ID", 11, "Opening words")],
    ];
    for pages in cases {
        let pages: Vec<String> = pages.iter().map(page).collect();
        let pages: Vec<&str> = pages.iter().map(String::as_str).collect();
        let conversion = converter(1).convert(Input::new(&pdf_pages(&pages), Format::Pdf)).unwrap();
        let markdown = conversion.markdown();
        for page in &pages {
            let shown = page.split(['(', ')']).skip(1).step_by(2);
            for text in shown {
                let times = pages.iter().filter(|other| other.contains(text)).count();
                assert!(markdown.matches(text).count() >= times, "{text}: {markdown}");
            }
        }
        assert!(conversion.diagnostics().is_empty(), "{markdown}");
    }
}

#[test]
fn the_engines_text_keeps_its_characters_under_fail() {
    for (name, text) in [
        ("handmade-ligatures", "The model classifies the input after it is stated."),
        ("handmade-arabic", "يقرأ الطالب الكتاب (الفصل 12) في المكتبة كل يوم"),
        (
            "handmade-predefined-cmap",
            "이 문서는 한국어 글꼴의 문자 코드를 유니코드로 읽는 시험입니다.",
        ),
        ("handmade-legacy-cmap", "이 문서는 한국어 글꼴의 옛 코드를 읽는 시험입니다."),
        ("text", "Family emoji: 👨\u{200d}👩\u{200d}👧"),
    ] {
        let conversion = converter(1).convert(Input::new(&pdf_fixture(name), Format::Pdf)).unwrap();
        assert!(conversion.markdown().contains(text), "{name}: {}", conversion.markdown());
    }
}

#[test]
fn pdf_output_is_identical_across_runs_and_threads() {
    let at = |threads| {
        configured(|config| {
            config.threads = threads;
            config.on_loss = OnLoss::Report;
        })
    };
    let (one, four) = (at(1), at(4));
    for (name, _) in VERDICTS {
        let bytes = pdf_fixture(name);
        let run = |converter: &Converter| {
            let parsed = converter.parse(Input::new(&bytes, Format::Pdf)).unwrap();
            let converted = converter.convert(Input::new(&bytes, Format::Pdf)).unwrap();
            (parsed.document().clone(), converted)
        };
        let first = run(&one);
        assert_eq!(first, run(&one), "{name}");
        assert_eq!(first, run(&four), "{name}");
    }
}

pub(crate) fn pdf_pages(pages: &[&str]) -> Vec<u8> {
    let pages: Vec<Vec<(&str, &str)>> = pages.iter().map(|content| vec![("", *content)]).collect();
    let pages: Vec<&[(&str, &str)]> = pages.iter().map(Vec::as_slice).collect();
    pdf_tree(&pages, &|kids, count| format!("<< /Type /Pages /Kids [{kids}] /Count {count} >>"))
}

fn pdf_tree(pages: &[&[(&str, &str)]], tree: &dyn Fn(&str, usize) -> String) -> Vec<u8> {
    let mut next = 5;
    let mut kids = Vec::new();
    let mut page_objects = Vec::new();
    for streams in pages {
        kids.push(format!("{next} 0 R"));
        let contents: Vec<String> =
            (1..=streams.len()).map(|i| format!("{} 0 R", next + i)).collect();
        page_objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents [{}] \
             /Resources << /Font << /F1 3 0 R /F2 4 0 R \
             /F3 << /Type /Font /Subtype /Type1 /BaseFont /Courier >> >> >> >>",
            contents.join(" ")
        ));
        for (extra, content) in *streams {
            page_objects.push(format!(
                "<< /Length {} {extra} >>\nstream\n{content}\nendstream",
                content.len()
            ));
        }
        next += 1 + streams.len();
    }
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        tree(&kids.join(" "), pages.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        "<< /Type /Font /Subtype /Type0 /BaseFont /Unresolved /Encoding /Identity-H \
         /DescendantFonts [<< /Type /Font /Subtype /CIDFontType2 /BaseFont /Unresolved \
         /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> >>] >>"
            .to_string(),
    ];
    objects.extend(page_objects);
    write_pdf(&objects)
}

pub(crate) fn write_pdf(objects: &[String]) -> Vec<u8> {
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        write!(out, "{} 0 obj\n{object}\nendobj\n", i + 1).unwrap();
    }
    let xref = out.len();
    write!(out, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).unwrap();
    for offset in offsets {
        writeln!(out, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    )
    .unwrap();
    out.into_bytes()
}

pub(crate) fn painted(resources: &str, contents: &str, extra: &[&str]) -> Vec<u8> {
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

pub(crate) fn plain(content: &str) -> String {
    format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len())
}

const TEXT: &str = "BT /F1 12 Tf 72 700 Td (Text) Tj ET";

#[test]
fn pages_the_page_tree_declares_and_lacks_are_unit_unreadable() {
    for (tree, missing) in [
        ("<< /Type /Pages /Kids [5 0 R] /Count 2 >>", 1),
        ("<< /Type /Pages /Kids [5 0 R] /Count 4 >>", 3),
        ("<< /Type /Pages /Kids [5 0 R 99 0 R] /Count 1 >>", 1),
    ] {
        let bytes = pdf_tree(&[&[("", TEXT)]], &|_, _| tree.to_string());
        let error = converter(1).convert(Input::new(&bytes, Format::Pdf)).unwrap_err();
        assert_matches!(&error, ConvertError::Incomplete { .. }, "{tree}");
        let reported = reporting().convert(Input::new(&bytes, Format::Pdf)).unwrap();
        assert_eq!(reported.markdown(), "Text\n");
        let rows: Vec<_> =
            reported.diagnostics().iter().map(|d| (d.code, d.unit, d.count)).collect();
        assert_eq!(rows, [(DiagnosticCode::UnitUnreadable, None, missing)], "{tree}");
    }
}

#[test]
fn an_undecodable_content_stream_makes_its_page_a_gap_beside_text_or_alone() {
    let broken = ("/Filter /FlateDecode", "not a deflate stream at all");
    for streams in [&[broken][..], &[("", TEXT), broken][..]] {
        let bytes = pdf_tree(&[streams, &[("", TEXT)]], &|kids, count| {
            format!("<< /Type /Pages /Kids [{kids}] /Count {count} >>")
        });
        assert_eq!(verdicts(&bytes), ["needs_ocr unreadable_content", "text"]);
        let error = converter(1).convert(Input::new(&bytes, Format::Pdf)).unwrap_err();
        assert_matches!(&error, ConvertError::NeedsOcr { pages, page_count: 2, .. } if pages == &[1]);
        let parsed = reporting().parse(Input::new(&bytes, Format::Pdf)).unwrap();
        let gaps: Vec<_> = parsed.gaps().map(|gap| gap.reasons.clone()).collect();
        assert_eq!(gaps, [vec![GapReason::UnreadableContent]]);
        let reported = parsed.render().unwrap();
        assert_eq!(reported.markdown(), "<!-- needs-ocr page 1 -->\n\nText\n");
    }
}

fn pdf(content: &str) -> Vec<u8> {
    pdf_pages(&[content])
}

fn pdf_escaped(text: &str) -> String {
    text.replace('\\', "\\\\").replace('(', "\\(").replace(')', "\\)")
}

#[test]
fn a_pages_text_is_escaped_as_a_word_documents_is() {
    let lines =
        ["<script>alert(1)</script>", "[x](javascript:alert(1))", "<!-- needs-ocr page 9 -->"];
    let pages: Vec<String> = lines
        .iter()
        .map(|line| format!("BT /F1 12 Tf 72 700 Td ({}) Tj ET", pdf_escaped(line)))
        .collect();
    let pages: Vec<&str> = pages.iter().map(String::as_str).collect();
    let from_pdf = converter(1).convert(Input::new(&pdf_pages(&pages), Format::Pdf)).unwrap();
    let body: String = lines
        .iter()
        .map(|line| {
            format!("<w:p><w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>", xml(line))
        })
        .collect();
    let from_docx = converter(1).convert(Input::new(&docx(&body), Format::Docx)).unwrap();
    assert_eq!(from_pdf.markdown(), from_docx.markdown());
    assert_eq!(from_pdf.units().len(), 3);
}

#[test]
fn a_number_set_against_its_first_word_opens_a_list_item() {
    let content = "BT /F1 12 Tf 72 700 Td (1. x) Tj ET\nBT /F1 12 Tf 72 620 Td (2. y) Tj ET\n";
    let conversion = converter(1).convert(Input::new(&pdf(content), Format::Pdf)).unwrap();
    assert_eq!(conversion.markdown(), "1. x\n2. y\n");
}

#[test]
fn lines_set_in_courier_are_a_code_block_of_the_pages_text() {
    let content = "BT /F3 10 Tf 72 700 Td (fn main\\(\\) {) Tj 24 -12 Td (run\\(\\);) Tj \
                   -24 -12 Td (}) Tj ET";
    let bytes = pdf(content);
    assert_eq!(verdicts(&bytes), ["text"]);
    let conversion = converter(1).convert(Input::new(&bytes, Format::Pdf)).unwrap();
    assert_eq!(conversion.markdown(), "```\nfn main() {\n    run();\n}\n```\n");
}

#[test]
fn a_code_line_on_every_page_is_never_a_running_head() {
    let page = |body: &str| format!("BT /F3 10 Tf 72 740 Td (let x = 1;) Tj ET\n{body}");
    let pages = [page(TEXT), page("BT /F1 12 Tf 72 700 Td (More) Tj ET")];
    let pages: Vec<&str> = pages.iter().map(String::as_str).collect();
    let conversion = converter(1).convert(Input::new(&pdf_pages(&pages), Format::Pdf)).unwrap();
    assert_eq!(
        conversion.markdown(),
        "```\nlet x = 1;\n```\n\nText\n\n```\nlet x = 1;\n```\n\nMore\n"
    );
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[test]
fn raised_lowered_and_struck_runs_keep_their_marks() {
    let pages = [
        "BT /F1 12 Tf 72 700 Td (E = mc) Tj /F1 7 Tf 4 Ts (2) Tj ET",
        "BT /F1 12 Tf 72 700 Td (H) Tj /F1 7 Tf -3 Ts (2) Tj /F1 12 Tf 0 Ts (O) Tj ET",
        "BT /F1 12 Tf 72 700 Td (gone) Tj ET\n72 704 m 98.7 704 l S",
    ];
    let conversion = converter(1).convert(Input::new(&pdf_pages(&pages), Format::Pdf)).unwrap();
    assert_eq!(conversion.markdown(), "E = mc<sup>2</sup>\n\nH<sub>2</sub>O\n\n~~gone~~\n");
}

#[test]
fn parse_then_render_converts_a_pdf_as_convert_does() {
    let bytes = pdf_fixture("handmade-mixed");
    let converted = configured(|_| {}).convert(Input::new(&bytes, Format::Pdf));
    assert_matches!(
        converted,
        Err(ConvertError::NeedsOcr { pages, page_count: 2, .. }) if pages == [2]
    );
    let parsed = reporting().parse(Input::new(&bytes, Format::Pdf)).unwrap();
    assert_eq!(
        parsed.render().unwrap(),
        reporting().convert(Input::new(&bytes, Format::Pdf)).unwrap()
    );
}

#[test]
fn a_garbled_run_beside_readable_text_is_lost_and_fails_incomplete() {
    let content = "BT /F1 12 Tf 72 700 Td (Readable text) Tj ET\n\
                   BT /F2 12 Tf 72 600 Td <0102030405060708> Tj ET\n";
    let bytes = pdf(content);
    assert_eq!(verdicts(&bytes), ["text"]);
    let Err(ConvertError::Incomplete { diagnostics, .. }) =
        converter(1).convert(Input::new(&bytes, Format::Pdf))
    else {
        panic!("a lost run passed fail");
    };
    let rows: Vec<_> = diagnostics.iter().map(|d| (d.code, d.unit.map(NonZeroU32::get))).collect();
    assert_eq!(rows, [(DiagnosticCode::GarbledFontEncoding, Some(1))]);
    let reported = reporting().convert(Input::new(&bytes, Format::Pdf)).unwrap();
    assert_eq!(reported.markdown(), "Readable text\n");
}

fn partly_mapped_pdf(before: &str, left_out: &[u16], after: &str, clip: Option<&str>) -> Vec<u8> {
    let mut chars: Vec<char> = before.chars().chain(after.chars()).collect();
    chars.sort_unstable();
    chars.dedup();
    let code = |c: char| chars.iter().position(|d| *d == c).unwrap() + 1;
    let hex = |text: &str| text.chars().map(|c| format!("{:04X}", code(c))).collect::<String>();
    let codes: String = left_out.iter().map(|code| format!("{code:04X}")).collect();
    let content = match clip {
        None => format!("BT /F1 12 Tf 20 700 Td <{}{codes}{}> Tj ET", hex(before), hex(after)),
        Some(clip) => format!(
            "BT /F1 12 Tf 20 700 Td <{}{}> Tj ET\n\
             q {clip} BT /F1 12 Tf 20 600 Td <{}{codes}> Tj ET Q",
            hex(before),
            hex(after),
            hex(before)
        ),
    };
    let mut cmap = format!(
        "/CIDInit /ProcSet findresource begin 12 dict begin begincmap /CMapName /T def \
         1 begincodespacerange <0000> <FFFF> endcodespacerange\n{} beginbfchar\n",
        chars.len()
    );
    for &c in &chars {
        writeln!(cmap, "<{:04X}> <{:04X}>", code(c), u32::from(c)).unwrap();
    }
    cmap.push_str("endbfchar endcmap CMapName currentdict /CMap defineresource pop end end");
    let stream = |body: &str| format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len());
    write_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [4 0 R] /Count 1 >>".to_string(),
        stream(&content),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 3 0 R \
         /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_string(),
        "<< /Type /Font /Subtype /Type0 /BaseFont /Arial /Encoding /Identity-H \
         /DescendantFonts [6 0 R] /ToUnicode 7 0 R >>"
            .to_string(),
        "<< /Type /Font /Subtype /CIDFontType2 /BaseFont /Arial /CIDSystemInfo \
         << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /DW 500 >>"
            .to_string(),
        stream(&cmap),
    ])
}

#[test]
fn a_code_a_composite_font_leaves_out_beside_codes_it_maps_is_garbled_font_encoding() {
    let (before, after) = ("The board approved the plan ", "after a long review.");
    let mapped = partly_mapped_pdf(before, &[], after, None);
    let conversion = converter(1).convert(Input::new(&mapped, Format::Pdf)).unwrap();
    assert_eq!(conversion.markdown(), format!("{before}{after}\n"));
    for (left_out, after) in [(&[0x0200][..], after), (&[0x0200, 0x0300], after), (&[0x0200], "")] {
        let bytes = partly_mapped_pdf(before, left_out, after, None);
        let Err(ConvertError::Incomplete { diagnostics, .. }) =
            converter(1).convert(Input::new(&bytes, Format::Pdf))
        else {
            panic!("a left-out code passed fail: {left_out:?}");
        };
        let rows: Vec<_> = diagnostics
            .iter()
            .map(|d| (d.code, d.class, d.unit.map(NonZeroU32::get), d.count))
            .collect();
        let count = u64::try_from(left_out.len()).unwrap();
        let lost = (DiagnosticCode::GarbledFontEncoding, LossClass::Lost, Some(1), count);
        assert_eq!(rows, [lost], "{left_out:?}");
        let reported = reporting().convert(Input::new(&bytes, Format::Pdf)).unwrap();
        let text = format!("{before}{after}");
        assert_eq!(reported.markdown(), format!("{}\n", text.trim_end()), "{left_out:?}");
    }
}

#[test]
fn unreadable_codes_and_runs_hidden_by_the_clip_or_page_report_no_loss() {
    for clip in ["0 0 10 10 re W n", "1 0 0 1 1000 0 cm"] {
        let content = format!(
            "BT /F1 12 Tf 72 700 Td (Readable text) Tj ET\n\
             q {clip} BT /F2 12 Tf 72 600 Td <0102030405060708> Tj ET Q"
        );
        let suppressed = pdf(&content);
        let partly_mapped = partly_mapped_pdf("Readable text", &[0x0200], "", Some(clip));
        for bytes in [suppressed, partly_mapped] {
            let converted = converter(1).convert(Input::new(&bytes, Format::Pdf)).unwrap();
            assert_eq!(converted.markdown(), "Readable text\n", "{clip}");
            assert_eq!(converted.diagnostics().iter().count(), 0, "{clip}");
            assert_eq!(verdicts(&bytes), ["text"], "{clip}");
        }
    }
}

fn helvetica(entries: &str) -> String {
    format!("/Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica {entries} >> >>")
}

fn helvetica_page(entries: &str, content: &str, extra: &[&str]) -> Vec<u8> {
    painted(&helvetica(entries), &plain(content), extra)
}

#[test]
fn a_differences_code_outside_a_byte_names_nothing_until_the_next_code() {
    for (differences, shown, expected) in
        [("4294967361 /B", "A", "A\n"), ("-1 /a /b 66 /c", "AB", "Ac\n")]
    {
        let font = format!("/Encoding << /Differences [{differences}] >>");
        let content = format!("BT /F1 12 Tf 72 700 Td ({shown}) Tj ET");
        let bytes = helvetica_page(&font, &content, &[]);
        let conversion = converter(1).convert(Input::new(&bytes, Format::Pdf));
        assert_eq!(conversion.unwrap().markdown(), expected, "{differences}");
    }
}

#[test]
fn a_cmap_entry_that_overflows_or_is_not_hex_is_skipped_and_the_rest_still_maps() {
    let cmap = |codespace: &str, entries: &str| {
        plain(&format!(
            "/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
             1 begincodespacerange {codespace} endcodespacerange {entries} \
             endcmap CMapName currentdict /CMap defineresource pop end end"
        ))
    };
    let simple = |entries: &str| {
        let font = "/Encoding /WinAnsiEncoding /ToUnicode 5 0 R";
        let content = "BT /F1 12 Tf 72 700 Td (AB) Tj ET";
        helvetica_page(font, content, &[&cmap("<00> <FF>", entries)])
    };
    let composite = painted(
        "/Font << /F1 5 0 R >>",
        &plain("BT /F1 12 Tf 72 700 Td <00010041> Tj ET"),
        &[
            "<< /Type /Font /Subtype /Type0 /BaseFont /Arial /Encoding 6 0 R \
             /DescendantFonts [7 0 R] /ToUnicode 8 0 R >>",
            &cmap("<0000> <FFFF>", "1 begincidrange <0000> <0001> 4294967295 endcidrange"),
            "<< /Type /Font /Subtype /CIDFontType2 /BaseFont /Arial \
             /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> >>",
            &cmap("<0000> <FFFF>", "2 beginbfchar <0001> <0058> <0041> <0041> endbfchar"),
        ],
    );
    for (bytes, expected) in [
        (simple("2 beginbfrange <00> <01> <FFFFFFFF> <41> <42> <0061> endbfrange"), "ab\n"),
        (simple("2 beginbfchar <41> <000\u{FFFD}> <42> <0062> endbfchar"), "Ab\n"),
        (composite, "XA\n"),
    ] {
        let conversion = converter(1).convert(Input::new(&bytes, Format::Pdf)).unwrap();
        assert_eq!(conversion.markdown(), expected);
        assert!(conversion.diagnostics().is_empty(), "{expected}");
    }
}

#[test]
fn arrays_nested_past_the_parser_bound_lose_only_their_own_object() {
    let nested = "[".repeat(100_000) + &"]".repeat(100_000);
    let image = format!(
        "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray \
         /BitsPerComponent 8 /Decode {nested} /Length 1 >>\nstream\na\nendstream"
    );
    let operand = helvetica_page("", &format!("{TEXT}\nBT {nested} TJ ET"), &[]);
    let dictionary = painted(
        &format!("{} /XObject << /Im1 5 0 R >>", helvetica("")),
        &plain(&format!("q 9 0 0 9 9 9 cm /Im1 Do Q\n{TEXT}")),
        &[&image],
    );
    let omitted = (DiagnosticCode::ImageOmitted, Region::Body, Some(1), 1);
    for (bytes, diagnostics) in [(operand, vec![]), (dictionary, vec![omitted])] {
        let conversion = converter(1).convert(Input::new(&bytes, Format::Pdf)).unwrap();
        assert_eq!(conversion.markdown(), "Text\n");
        assert_eq!(reported(conversion.diagnostics()), diagnostics);
    }
}

#[test]
fn a_form_whose_subtype_is_an_indirect_name_shows_its_text() {
    let inside = "BT /F1 12 Tf 72 700 Td (Inside) Tj ET";
    let form = format!(
        "<< /Type /XObject /Subtype 6 0 R /BBox [0 0 612 792] /Resources << {} >> /Length {} >>\n\
         stream\n{inside}\nendstream",
        helvetica(""),
        inside.len()
    );
    let bytes = painted(
        &format!("{} /XObject << /Fm1 5 0 R >>", helvetica("")),
        &plain("q /Fm1 Do Q"),
        &[&form, "/Form"],
    );
    assert_eq!(verdicts(&bytes), ["text"]);
    let conversion = converter(1).convert(Input::new(&bytes, Format::Pdf));
    assert_eq!(conversion.unwrap().markdown(), "Inside\n");
}

const TARGET: &str = "https://example.test/paper";

const PROSE: &str = "BT /F1 12 Tf 72 700 Td (Link) Tj ET";
const OVER_PROSE: &str = "72 696 132 712";

fn annotated(content: &str, links: &[(&str, &str)]) -> Vec<u8> {
    let annots: String = links
        .iter()
        .map(|(rect, target)| {
            format!("<< /Subtype /Link /Rect [{rect}] /A << /S /URI /URI ({target}) >> >>")
        })
        .collect();
    write_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
             /Annots [{annots}] \
             /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> \
             /F3 << /Type /Font /Subtype /Type1 /BaseFont /Courier >> >> >> >>"
        ),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
    ])
}

#[test]
fn a_uri_link_over_prose_is_kept_and_over_code_it_is_lost_and_fails_incomplete() {
    let prose = annotated(PROSE, &[(OVER_PROSE, TARGET)]);
    let kept = converter(1).convert(Input::new(&prose, Format::Pdf)).unwrap();
    assert_eq!(kept.markdown(), format!("[Link]({TARGET})\n"));
    assert!(kept.diagnostics().is_empty());

    let code = annotated("BT /F3 10 Tf 72 700 Td (let x = 1;) Tj ET", &[(OVER_PROSE, TARGET)]);
    let Err(ConvertError::Incomplete { diagnostics, .. }) =
        converter(1).convert(Input::new(&code, Format::Pdf))
    else {
        panic!("a link left unread passed fail");
    };
    let lost = [(DiagnosticCode::LinkTargetUnread, Region::Body, Some(1), 1)];
    assert_eq!(reported(&diagnostics), lost);
    let report = reporting().convert(Input::new(&code, Format::Pdf)).unwrap();
    assert_eq!(report.markdown(), "```\nlet x = 1;\n```\n");
    let asked = reporting().convert(pdf_input(&code, &PageSelection::All)).unwrap();
    let gap = [(DiagnosticCode::OcrGap, Region::Body, Some(1), 1)];
    assert_eq!(reported(asked.diagnostics()), gap);
}

#[test]
fn a_link_over_code_loses_only_its_own_target_and_reports_its_page_once() {
    let content = format!("{PROSE}\nBT /F3 10 Tf 72 600 Td (let x = 1;) Tj ET");
    let links = [(OVER_PROSE, TARGET), ("72 596 140 612", "https://example.test/code")];
    let bytes = annotated(&content, &links);
    let conversion = reporting().convert(Input::new(&bytes, Format::Pdf)).unwrap();
    assert_eq!(conversion.markdown(), format!("[Link]({TARGET})\n\n```\nlet x = 1;\n```\n"));
    let lost = [(DiagnosticCode::LinkTargetUnread, Region::Body, Some(1), 1)];
    assert_eq!(reported(conversion.diagnostics()), lost);
}

#[test]
fn a_pdf_uri_link_with_an_unsafe_scheme_keeps_its_label() {
    for target in ["ftp://example.test/f", "tel:+15550100", "javascript:void(0)"] {
        let bytes = annotated(PROSE, &[(OVER_PROSE, target)]);
        let conversion = reporting().convert(Input::new(&bytes, Format::Pdf)).unwrap();
        assert_eq!(conversion.markdown(), "Link\n", "{target}");
        let refused = [(DiagnosticCode::UnsafeLink, Region::Body, None, 1)];
        assert_eq!(reported(conversion.diagnostics()), refused, "{target}");
    }
}

#[test]
fn each_link_a_page_keeps_charges_work_by_its_targets_length() {
    let target = format!("https://example.test/{}", "a".repeat(8000));
    let links = 18;
    let lines: String = (0..links)
        .map(|line| {
            let y = 700 - 14 * line;
            format!("BT /F1 12 Tf 72 {y} Td (Link) Tj ET\nBT /F1 12 Tf 97 {y} Td (plain) Tj ET\n")
        })
        .collect();
    let linked = annotated(&lines, &[("0 0 95 792", &target)]);
    let kept = reporting().convert(Input::new(&linked, Format::Pdf)).unwrap();
    assert_eq!(kept.markdown().matches(&target).count(), links as usize);
    let plain = annotated(&lines, &[]);
    let least = least_work(|converter| converter.parse(Input::new(&plain, Format::Pdf)).is_ok());
    let max = least + (links - 1) * target.len() as u64;
    let refusal = limited(|limits| limits.max_work = max).parse(Input::new(&linked, Format::Pdf));
    assert!(trips(&refusal, Limit::Work, max), "{:?}", refusal.err());
}

#[test]
fn a_bare_margin_page_number_the_engine_removes_is_one_omitted_footer_for_its_page() {
    let page = |folio: &str| format!("BT /F1 12 Tf 72 400 Td (Body) Tj ET\n{folio}");
    let pages = [
        page("BT /F1 12 Tf 300 30 Td (Page 12) Tj ET"),
        page("BT /F1 12 Tf 300 770 Td (7) Tj ET"),
        page("BT /F1 12 Tf 300 300 Td (8) Tj ET"),
        page("BT /F1 12 Tf 580 200 Td (9) Tj ET"),
    ];
    let pages: Vec<&str> = pages.iter().map(String::as_str).collect();
    let bytes = pdf_pages(&pages);
    for policy in [HeaderFooterPolicy::Omit, HeaderFooterPolicy::Emit] {
        let converter = configured(|config| {
            config.headers_footers = policy;
            config.on_loss = OnLoss::Report;
        });
        let conversion = converter.convert(Input::new(&bytes, Format::Pdf)).unwrap();
        assert_eq!(conversion.markdown(), "Body\n\nBody\n\nBody\n\n8\n\nBody\n", "{policy:?}");
        let footer = |page| (DiagnosticCode::FooterOmitted, Region::Footer, Some(page), 1);
        let footers = [footer(1), footer(2), footer(4)];
        assert_eq!(reported(conversion.diagnostics()), footers, "{policy:?}");
        let asked = converter.convert(pdf_input(&bytes, &selecting(&[1]))).unwrap();
        let gap = (DiagnosticCode::OcrGap, Region::Body, Some(1), 1);
        assert_eq!(reported(asked.diagnostics()), [gap, footer(2), footer(4)], "{policy:?}");
    }
}

#[test]
fn a_page_whose_only_text_is_a_folio_the_engine_removes_is_blank_beside_its_omitted_footer() {
    let folio = "BT /F1 10 Tf 500 20 Td (362) Tj ET";
    let bytes = pdf(folio);
    for policy in [HeaderFooterPolicy::Omit, HeaderFooterPolicy::Emit] {
        let converter = configured(|config| config.headers_footers = policy);
        let conversion = converter.convert(Input::new(&bytes, Format::Pdf)).unwrap();
        assert_eq!(conversion.markdown(), "", "{policy:?}");
        let footer = [(DiagnosticCode::FooterOmitted, Region::Footer, Some(1), 1)];
        assert_eq!(reported(conversion.diagnostics()), footer, "{policy:?}");
    }
    assert_eq!(verdicts(&bytes), ["no_text"]);
    let garbled = pdf(&format!("BT /F2 12 Tf 72 700 Td <0001> Tj ET\n{folio}"));
    assert_eq!(verdicts(&garbled), ["needs_ocr garbled_text"]);
}

const SCAN: &str = "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray \
                    /BitsPerComponent 8 /Length 1 >>\nstream\na\nendstream";

fn scanned_pages(contents: &[String]) -> Vec<u8> {
    let kids: Vec<String> = (0..contents.len()).map(|at| format!("{} 0 R", 5 + 2 * at)).collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids.join(" "), contents.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        SCAN.to_owned(),
    ];
    for (at, content) in contents.iter().enumerate() {
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R \
             /Resources << /Font << /F1 3 0 R >> /XObject << /Scan 4 0 R >> >> >>",
            6 + 2 * at
        ));
        objects.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()));
    }
    write_pdf(&objects)
}

fn stamped_scans(stamp: &str) -> Vec<u8> {
    let pages: Vec<String> = (1..=3)
        .map(|folio| {
            format!(
                "q 612 0 0 475 0 0 cm /Scan Do Q\nBT /F1 10 Tf 72 300 Td ({stamp}) Tj ET\n\
                 BT /F1 9 Tf 300 30 Td ({folio}) Tj ET"
            )
        })
        .collect();
    scanned_pages(&pages)
}

#[test]
fn a_stamp_alone_on_partial_scans_is_furniture_judged_at_a_charge_by_its_key() {
    let short = stamped_scans("PROCEEDINGS 1923");
    let longer = "X".repeat(60);
    let long = stamped_scans(&format!("PROCEEDINGS {longer} 1923"));
    for bytes in [&short, &long] {
        assert_eq!(verdicts(bytes), ["needs_ocr image_backed"; 3]);
    }
    let least = |bytes: &[u8]| {
        least_work(|converter| converter.parse(Input::new(bytes, Format::Pdf)).is_ok())
    };
    let (short_work, long_work) = (least(&short), least(&long));
    let read_once = 3 * (longer.len() as u64 + 1);
    assert!(long_work >= short_work + 2 * read_once, "{short_work} {long_work}");
    let refusal =
        limited(|limits| limits.max_work = long_work - 1).parse(Input::new(&long, Format::Pdf));
    assert!(trips(&refusal, Limit::Work, long_work - 1), "{:?}", refusal.err());
}
