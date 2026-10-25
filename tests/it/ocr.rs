//! The caller's OCR filling or failing a PDF's gaps between parse and render.

use std::assert_matches;
use std::num::NonZeroU32;

use crate::bounds::least_work;
use crate::pdf::pdf_pages;
use crate::support::{configured, fixture, limit_mut, limited, reporting};
use almiladoc::model::GapId;
use almiladoc::{
    CancelToken, ConvertError, Converter, DiagnosticCode, Format, GapError, GapFill, Input, Limit,
    PageSelection, Parsed,
};

const FILL: &str = "# A\n\n| x |\n| - |\n| 1 |";

const FILLED: &str = "Text on the first page\n\n# A\n\n| x |\n| --- |\n| 1 |\n\n\
                      Text on the first page\n\nText on the first page\n\n\
                      # A\n\n| x |\n| --- |\n| 1 |\n";

fn five_pages(converter: &Converter) -> (Parsed, [GapId; 2]) {
    let bytes = fixture("pdf/handmade-five-page.pdf");
    let parsed = converter.parse(Input::new(&bytes, Format::Pdf)).unwrap();
    let ids: Vec<GapId> = parsed.gaps().map(|gap| gap.id).collect();
    (parsed, ids.try_into().unwrap())
}

fn markdown(text: &str) -> GapFill {
    GapFill::Markdown(text.to_owned())
}

fn rows(parsed: &Parsed) -> Vec<(DiagnosticCode, Option<u32>, u64)> {
    parsed.diagnostics().iter().map(|d| (d.code, d.unit.map(NonZeroU32::get), d.count)).collect()
}

#[test]
fn filled_pages_render_in_place_and_complete_the_conversion() {
    let (mut parsed, [two, five]) = five_pages(&configured(|_| {}));
    parsed.fill_gap(two, markdown(FILL)).unwrap();
    parsed.fill_gap(five, markdown(FILL)).unwrap();
    assert_eq!(rows(&parsed), []);
    assert!(format!("{parsed:?}").contains("state: Filled(2 blocks)"));
    let conversion = parsed.render().unwrap();
    assert_eq!(conversion.markdown(), FILLED);
    assert!(conversion.is_complete());
    let spans: Vec<_> = conversion
        .units()
        .iter()
        .map(|span| conversion.markdown().get(span.bytes.clone()).unwrap())
        .collect();
    let fill = "# A\n\n| x |\n| --- |\n| 1 |";
    let text = "Text on the first page";
    assert_eq!(spans, [text, fill, text, text, fill]);
}

#[test]
fn each_gap_awaits_its_own_fill_and_a_failed_one_writes_its_marker() {
    let (mut parsed, [two, five]) = five_pages(&configured(|_| {}));
    parsed.fill_gap(five, markdown("five")).unwrap();
    assert_matches!(parsed.render(), Err(ConvertError::NeedsOcr { pages, page_count: 5, .. }) if pages == [2]);
    parsed.fail_gap(two).unwrap();
    assert_eq!(rows(&parsed), [(DiagnosticCode::OcrFailed, Some(2), 1)]);
    assert_matches!(parsed.render(), Err(ConvertError::NeedsOcr { pages, .. }) if pages == [2]);

    let (mut parsed, [two, five]) = five_pages(&reporting());
    parsed.fill_gap(two, markdown("two")).unwrap();
    parsed.fail_gap(five).unwrap();
    let conversion = parsed.render().unwrap();
    assert_eq!(
        conversion.markdown(),
        "Text on the first page\n\ntwo\n\nText on the first page\n\nText on the first page\n\n\
         <!-- ocr-failed page 5 -->\n"
    );
    assert!(!conversion.is_complete());
    assert_eq!(conversion.diagnostics(), parsed.diagnostics());
}

#[cfg(feature = "serde")]
#[test]
fn the_json_holds_each_fill_in_its_gap_s_place_and_the_settled_diagnostics() {
    let (mut parsed, [two, five]) = five_pages(&reporting());
    parsed.fill_gap(two, markdown(FILL)).unwrap();
    parsed.fail_gap(five).unwrap();
    let json = parsed.to_json().unwrap();
    let text = |text: &str| {
        format!(
            r#"{{"kind":"text","value":{{"text":"{text}","style":{{"bold":false,"italic":false,"strike":false,"code":false,"position":"baseline"}}}}}}"#
        )
    };
    let cell = |content: &str| {
        format!(
            r#"{{"kind":"origin","value":{{"blocks":[{{"kind":"paragraph","value":[{}]}}],"col_span":1,"row_span":1}}}}"#,
            text(content)
        )
    };
    let heading = format!(
        r#"{{"kind":"heading","value":{{"level":1,"anchor":null,"content":[{}]}}}}"#,
        text("A")
    );
    let table = format!(
        r#"{{"kind":"table","value":{{"grid":[[{}],[{}]],"header_rows":1}}}}"#,
        cell("x"),
        cell("1")
    );
    assert!(json.contains(&format!("{heading},{table}")), "{json}");
    let five = r#"{"kind":"gap","value":{"id":{"unit":{"kind":"page","ordinal":5},"ordinal":1}"#;
    assert_eq!((json.matches(r#"{"kind":"gap""#).count(), json.matches(five).count()), (1, 1));
    assert!(
        json.ends_with(
            r#""diagnostics":[{"code":"ocr_failed","class":"deferred","region":"body","unit":5,"count":1}]}"#
        ),
        "{json}"
    );
}

#[test]
fn no_debug_form_holds_the_ocr_text() {
    let sentinel = "OCR-SENTINEL-7f3a";
    let (mut parsed, [two, five]) = five_pages(&configured(|_| {}));
    parsed.fill_gap(two, markdown(sentinel)).unwrap();
    parsed.fill_gap(five, markdown(sentinel)).unwrap();
    let conversion = parsed.render().unwrap();
    assert!(conversion.markdown().contains(sentinel));
    assert!(!format!("{parsed:?}").contains(sentinel));
    let debug = format!("{conversion:?}");
    assert!(!debug.contains(sentinel), "{debug}");
    assert!(debug.contains(&format!("markdown_bytes: {}", conversion.markdown().len())), "{debug}");
}

#[test]
fn a_settled_or_foreign_gap_is_refused_and_changes_nothing() {
    let (mut parsed, [two, five]) = five_pages(&reporting());
    let scanned = fixture("pdf/handmade-scanned.pdf");
    let foreign = reporting().parse(Input::new(&scanned, Format::Pdf)).unwrap();
    let foreign = foreign.gaps().next().unwrap().id;
    parsed.fill_gap(two, markdown("two")).unwrap();
    parsed.fail_gap(five).unwrap();
    let before = format!("{parsed:?}");
    let refusals = [
        parsed.fill_gap(two, markdown("again")),
        parsed.fail_gap(two),
        parsed.fill_gap(five, markdown("again")),
        parsed.fail_gap(five),
        parsed.fill_gap(foreign, markdown("x")),
        parsed.fail_gap(foreign),
    ];
    use GapError::{Settled, UnknownGap};
    assert_eq!(
        refusals,
        [Err(Settled), Err(Settled), Err(Settled), Err(Settled), Err(UnknownGap), Err(UnknownGap)]
    );
    assert_eq!(format!("{parsed:?}"), before);
}

#[test]
fn a_fill_past_a_limit_is_refused_and_the_gap_then_fills() {
    let long = "a".repeat(1024 * 1024 + 1);
    let deep = format!("{}a", "> ".repeat(257));
    let cases = [(long.as_str(), Limit::OcrTextBytes), (deep.as_str(), Limit::NestingDepth)];
    for (text, limit) in cases {
        let (mut parsed, [two, _]) = five_pages(&reporting());
        let before = format!("{parsed:?}");
        let refused = parsed.fill_gap(two, markdown(text));
        assert_matches!(refused, Err(GapError::Refused { limit: crossed, .. }) if crossed == limit);
        assert_eq!(format!("{parsed:?}"), before, "{limit}");
        parsed.fill_gap(two, markdown(text.get(1..).unwrap())).unwrap();
        parsed.render().unwrap();

        let raised = limited(|limits| *limit_mut(limits, limit) *= 2);
        let (mut parsed, [two, _]) = five_pages(&raised);
        parsed.fill_gap(two, markdown(text)).unwrap();
        parsed.render().unwrap();
    }
}

#[test]
fn a_fill_whose_tables_may_pass_the_grid_slots_is_refused() {
    let table = format!("|a|b\n|-|-\n{}", "a\n".repeat(9));
    let slots = |max| limited(|limits| limits.max_grid_slots = max);
    let (mut parsed, [two, _]) = five_pages(&slots(29));
    assert_matches!(
        parsed.fill_gap(two, markdown(&table)),
        Err(GapError::Refused { limit: Limit::GridSlots, .. })
    );
    let (mut parsed, [two, _]) = five_pages(&slots(30));
    parsed.fill_gap(two, markdown(&table)).unwrap();
}

#[test]
fn a_fill_past_the_work_left_fails_as_a_conversion_and_changes_nothing() {
    let bytes = fixture("pdf/handmade-five-page.pdf");
    let least = least_work(|converter| converter.parse(Input::new(&bytes, Format::Pdf)).is_ok());
    let (mut parsed, [two, _]) = five_pages(&limited(|limits| limits.max_work = least + 3));
    let before = format!("{parsed:?}");
    assert_matches!(
        parsed.fill_gap(two, markdown("a *b*")),
        Err(GapError::Convert(ConvertError::ResourceLimit { limit: Limit::Work, max, .. }))
            if max == least + 3
    );
    assert_eq!(format!("{parsed:?}"), before);
    parsed.fill_gap(two, markdown("a")).unwrap();
}

#[test]
fn the_same_fills_in_any_order_render_alike() {
    let fill = |order: [usize; 2]| {
        let (mut parsed, ids) = five_pages(&configured(|_| {}));
        for i in order {
            parsed.fill_gap(ids[i], markdown(["two", "five"][i])).unwrap();
        }
        parsed.render().unwrap()
    };
    let forward = fill([0, 1]);
    assert_eq!(fill([1, 0]), forward);
    assert!(forward.markdown().contains("two") && forward.markdown().ends_with("five\n"));
}

#[test]
fn a_gap_of_a_folded_unit_settles_its_own_count() {
    let bytes = pdf_pages(&[""; 66]);
    let selected = Input::new(&bytes, Format::Pdf).with_ocr_pages(PageSelection::All);
    let mut parsed = reporting().parse(selected).unwrap();
    let ids: Vec<GapId> = parsed.gaps().map(|gap| gap.id).collect();
    let gaps = |parsed: &Parsed| -> u64 {
        parsed
            .diagnostics()
            .iter()
            .filter(|d| d.code == DiagnosticCode::OcrGap)
            .map(|d| d.count)
            .sum()
    };
    assert_eq!(gaps(&parsed), 66);
    parsed.fail_gap(ids[65]).unwrap();
    parsed.fill_gap(ids[0], markdown("one")).unwrap();
    assert_eq!(gaps(&parsed), 64);
    let failed: Vec<_> =
        rows(&parsed).into_iter().filter(|row| row.0 == DiagnosticCode::OcrFailed).collect();
    assert_eq!(failed, [(DiagnosticCode::OcrFailed, Some(66), 1)]);
    for id in &ids[1..65] {
        parsed.fill_gap(*id, markdown("x")).unwrap();
    }
    assert_eq!(rows(&parsed), [(DiagnosticCode::OcrFailed, Some(66), 1)]);
    assert!(!parsed.render().unwrap().is_complete());
}

#[test]
fn each_fill_charges_its_events_on_from_the_work_before_it() {
    let bytes = fixture("pdf/handmade-five-page.pdf");
    let filling = |fills: usize| {
        least_work(|converter| {
            let Ok(mut parsed) = converter.parse(Input::new(&bytes, Format::Pdf)) else {
                return false;
            };
            let ids: Vec<GapId> = parsed.gaps().map(|gap| gap.id).take(fills).collect();
            ids.into_iter().all(|id| parsed.fill_gap(id, markdown("a")).is_ok())
        })
    };
    assert_eq!([filling(1) - filling(0), filling(2) - filling(0)], [3, 6]);
}

#[test]
fn a_cancelled_parse_refuses_every_later_call_but_a_failure() {
    let bytes = fixture("pdf/handmade-five-page.pdf");
    let token = CancelToken::new();
    let input = Input::new(&bytes, Format::Pdf).with_cancel(token.clone());
    let mut parsed = reporting().parse(input).unwrap();
    let [two, five] = parsed.gaps().map(|gap| gap.id).collect::<Vec<_>>().try_into().unwrap();
    parsed.fill_gap(two, markdown("two")).unwrap();
    token.cancel();
    assert_eq!(parsed.render(), Err(ConvertError::Cancelled));
    #[cfg(feature = "serde")]
    assert_eq!(parsed.to_json(), Err(ConvertError::Cancelled));
    assert_eq!(
        parsed.fill_gap(five, markdown("five")),
        Err(GapError::Convert(ConvertError::Cancelled))
    );
    assert_eq!(parsed.fail_gap(five), Ok(()));
    assert_eq!(parsed.clone().render(), Err(ConvertError::Cancelled));
}
