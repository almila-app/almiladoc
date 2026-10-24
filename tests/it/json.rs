//! The JSON a parse serialises to: its shape on a fixture, its output and
//! work limits, its losses and its determinism across threads.

use crate::bounds::{csv_records, least_work};
use crate::support::{converter, corpus, fixture, format_of, limited, trips};
use almiladoc::{ConvertError, Format, Input, Limit};

fn redacted(json: &str) -> String {
    let version = format!(r#""version":"{}""#, env!("CARGO_PKG_VERSION"));
    assert_eq!(json.matches(&version).count(), 1, "{json:.200}");
    json.replacen(&version, r#""version":"[version]""#, 1)
}

fn lines(json: &str) -> String {
    json.replace(r#"{"kind":"#, "\n{\"kind\":").replace(r#"{"code":"#, "\n{\"code\":")
}

#[test]
fn a_document_serialises_with_its_assets_measured_and_its_version_redacted() {
    let bytes = fixture("docx/handmade-rich.docx");
    let json = converter(1).parse(Input::new(&bytes, Format::Docx)).unwrap().to_json().unwrap();
    insta::assert_snapshot!("handmade_rich_docx", lines(&redacted(&json)));
}

#[test]
fn a_lossy_parse_serialises_under_fail_with_its_diagnostics() {
    let parsed = converter(1).parse(Input::new(br"{\rtf1 a", Format::Rtf)).unwrap();
    assert!(matches!(parsed.render(), Err(ConvertError::Incomplete { .. })));
    assert_eq!(
        redacted(&parsed.to_json().unwrap()),
        concat!(
            r#"{"schema_version":4,"#,
            r#""identity":{"version":"[version]","config_fingerprint":"64793a7a830129ecbef0c5e222bb22b493b88b9dde158117a8a41b78855b62e5"},"#,
            r#""document":{"blocks":[{"kind":"paragraph","value":[{"kind":"text","value":{"text":"a","#,
            r#""style":{"bold":false,"italic":false,"strike":false,"code":false,"position":"baseline"}}}]}],"#,
            r#""notes":[],"assets":[]},"#,
            r#""diagnostics":[{"code":"unclosed_structure","class":"lost","region":"body","unit":null,"count":1}]}"#,
        )
    );
}

#[test]
fn to_json_stops_at_max_output_bytes_past_its_exact_length() {
    let csv = csv_records();
    let input = || Input::new(&csv, Format::Csv);
    let json = limited(|_| {}).parse(input()).unwrap().to_json().unwrap();
    let len = json.len() as u64;
    let at =
        |max| limited(|limits| limits.max_output_bytes = max).parse(input()).unwrap().to_json();
    assert_eq!(at(len), Ok(json));
    assert!(trips(&at(len - 1), Limit::OutputBytes, len - 1));
}

#[test]
fn to_json_charges_a_unit_of_work_per_64_bytes_on_top_of_the_parse() {
    let csv = csv_records();
    let input = || Input::new(&csv, Format::Csv);
    let len = limited(|_| {}).parse(input()).unwrap().to_json().unwrap().len() as u64;
    let parse = least_work(|converter| converter.parse(input()).is_ok());
    let serialise = least_work(|converter| {
        converter.parse(input()).and_then(|parsed| parsed.to_json()).is_ok()
    });
    assert!(parse > 0 && len >= 64, "{parse} {len}");
    assert_eq!(serialise, parse + len / 64);
    let parsed = limited(|limits| limits.max_work = serialise - 1).parse(input()).unwrap();
    assert!(trips(&parsed.to_json(), Limit::Work, serialise - 1));
}

#[test]
fn one_and_three_threads_parse_and_serialise_every_fixture_identically() {
    let (one, three) = (converter(1), converter(3));
    let mut compared = 0;
    for path in corpus() {
        let bytes = std::fs::read(&path).unwrap();
        let Some(format) = format_of(&path, &bytes) else {
            continue;
        };
        let parsed = |converter: &almiladoc::Converter| converter.parse(Input::new(&bytes, format));
        let (first, second) = (parsed(&one), parsed(&three));
        assert_eq!(
            first.as_ref().map(almiladoc::Parsed::document),
            second.as_ref().map(almiladoc::Parsed::document),
            "{}",
            path.display()
        );
        let json = first.and_then(|parsed| parsed.to_json());
        compared += usize::from(json.is_ok());
        assert_eq!(json, second.and_then(|parsed| parsed.to_json()), "{}", path.display());
    }
    assert!(compared > 40, "{compared} fixtures serialised");
}
