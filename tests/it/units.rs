use crate::support::{configured, converter, fixture, reporting, trips, zip_package};
use almiladoc::{CancelToken, ConvertError, Format, Input, Limit, UnsupportedKind};
use std::assert_matches;

fn odp(pages: &[&str]) -> Vec<u8> {
    let pages: String = pages.iter().map(|text| format!(
        "<draw:page><draw:frame><draw:text-box><text:p>{text}</text:p></draw:text-box></draw:frame></draw:page>"
    )).collect();
    let content = format!(
        "<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\"><office:body><office:presentation>{pages}</office:presentation></office:body></office:document-content>"
    );
    zip_package(&[
        ("mimetype", "application/vnd.oasis.opendocument.presentation"),
        ("content.xml", &content),
    ])
}

#[test]
fn slide_ranges_keep_empty_units_without_visible_separator_noise() {
    for pages in [vec!["", "İlk 🦀", "", "Son", ""], vec!["", "", ""]] {
        let bytes = odp(&pages);
        let conversion = converter(1).convert(Input::new(&bytes, Format::Odp)).unwrap();
        assert_eq!(conversion.units().len(), pages.len());
        assert_eq!(
            conversion.markdown().matches("---").count(),
            pages.iter().filter(|p| !p.is_empty()).count().saturating_sub(1)
        );
        for (span, text) in conversion.units().iter().zip(&pages) {
            assert_eq!(conversion.markdown().get(span.bytes.clone()).unwrap(), *text);
        }
        assert!(!conversion.markdown().starts_with("---"));
    }
}

#[test]
fn automatic_inputs_share_the_explicit_path_and_keep_fallback_secondary() {
    let converter = converter(1);
    let bytes = odp(&["slide"]);
    assert_eq!(
        converter.convert(Input::auto(&bytes, Some(Format::Csv))).unwrap(),
        converter.convert(Input::new(&bytes, Format::Odp)).unwrap()
    );
    assert_matches!(
        converter.convert(Input::auto(b"a,b\n", None)),
        Err(ConvertError::Unsupported { kind: UnsupportedKind::UnknownFormat, .. })
    );
    let csv = converter.convert(Input::auto(b"a,b\n", Some(Format::Csv))).unwrap();
    assert_eq!(csv.units().len(), 1);
    assert_matches!(
        converter.convert(Input::new(&bytes, Format::Docx)),
        Err(ConvertError::Unsupported {
            kind: UnsupportedKind::FormatMismatch { detected: Format::Odp, .. },
            ..
        })
    );
    let token = CancelToken::new();
    token.cancel();
    assert_matches!(
        converter.parse(Input::auto(&bytes, None).with_cancel(token)),
        Err(ConvertError::Cancelled)
    );
}

#[test]
fn automatic_input_is_admitted_before_detection() {
    let converter = configured(|config| config.limits.max_input_bytes = 3);
    let refusal = converter.convert(Input::auto(b"PK\x03\x04", None));
    assert!(trips(&refusal, Limit::InputBytes, 3), "{refusal:?}");
}

#[test]
fn every_readable_fixture_has_the_expected_kind_of_units() {
    for (path, format, kind) in [
        ("csv/sheet.csv", Format::Csv, "sheet"),
        ("odp/pres.odp", Format::Odp, "slide"),
        ("pptx/pres.pptx", Format::Pptx, "slide"),
        ("ppt/pres.ppt", Format::Ppt, "slide"),
        ("xlsx/sheet.xlsx", Format::Excel, "sheet"),
        ("xls/sheet.xls", Format::Excel, "sheet"),
        ("xlsb/handmade-sheet.xlsb", Format::Excel, "sheet"),
        ("ods/sheet.ods", Format::Ods, "sheet"),
        ("epub/book.epub", Format::Epub, "spine_item"),
    ] {
        let bytes = fixture(path);
        let conversion = converter(1).convert(Input::new(&bytes, format)).unwrap();
        assert!(!conversion.units().is_empty(), "{path}");
        assert!(conversion.units().iter().all(|s| s.unit.kind.as_str() == kind), "{path}");
    }
}

#[test]
fn nonempty_slide_bodies_have_visible_boundaries() {
    let bytes = odp(&["first", "last"]);
    let converted = converter(1).convert(Input::new(&bytes, Format::Odp)).unwrap();
    assert_eq!(converted.markdown(), "first\n\n---\n\nlast\n");
}

#[test]
fn recovered_slide_ranges_keep_their_own_lists_and_exclude_ownerless_notes() {
    let bytes = fixture("malformed/brokenpersist--recovers.ppt");
    let conversion = reporting().convert(Input::auto(&bytes, None)).unwrap();
    assert_eq!(conversion.units().len(), 2);
    let first = &conversion.markdown().get(conversion.units()[0].bytes.clone()).unwrap();
    let second = &conversion.markdown().get(conversion.units()[1].bytes.clone()).unwrap();
    assert!(first.contains("Top level point") && first.contains("Nested detail"));
    assert!(!first.contains("Numbers Slide") && !first.contains("Speaker note"));
    assert!(second.contains("Numbers Slide") && !second.contains("Second point with emphasis"));
    assert!(!second.contains("Second slide notes"));
}

#[test]
fn an_empty_csv_and_empty_ods_sheets_keep_zero_length_ranges() {
    let csv = converter(1).convert(Input::new(b"", Format::Csv)).unwrap();
    assert_eq!(csv.markdown(), "");
    assert_eq!(csv.units().len(), 1);
    assert!(csv.units()[0].bytes.is_empty());
    let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="One"/><table:table table:name="Two"/></office:spreadsheet></office:body></office:document-content>"#;
    let bytes = zip_package(&[
        ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
        ("content.xml", content),
    ]);
    let ods = converter(1).convert(Input::auto(&bytes, None)).unwrap();
    assert_eq!(ods.markdown(), "");
    assert_eq!(ods.units().len(), 2);
    assert!(ods.units().iter().all(|unit| unit.bytes.is_empty()));
}
