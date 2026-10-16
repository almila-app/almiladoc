//! Each abuse input trips its limit at default `Limits`: a committed fixture
//! named `<limit>--<shape>--errors.<ext>`, or an input over 1 MiB built here.

mod common;

use almiladoc::{ConvertError, Format, Input, Limit, Limits};
use common::{converter, fixture_root, format_of, walk};

fn default_max(name: &str) -> u64 {
    let limits = Limits::default();
    match name {
        "max_entry_bytes" => limits.max_entry_bytes,
        "max_total_bytes" => limits.max_total_bytes,
        "max_entry_count" => limits.max_entry_count,
        "max_nesting_depth" => limits.max_nesting_depth,
        "max_xml_nodes" => limits.max_xml_nodes,
        "max_expansion" => limits.max_expansion,
        "max_expansion_text_bytes" => limits.max_expansion_text_bytes,
        "max_asset_total_bytes" => limits.max_asset_total_bytes,
        "max_record_depth" => limits.max_record_depth,
        "max_grid_slots" => limits.max_grid_slots,
        "max_compression_ratio" => limits.max_compression_ratio,
        other => panic!("no abuse fixture is expected to trip {other}"),
    }
}

#[test]
fn every_abuse_fixture_trips_the_limit_its_name_gives() {
    let mut files = Vec::new();
    walk(&fixture_root().join("abuse"), &mut files);
    assert_eq!(files.len(), 15, "abuse fixture set");
    let converter = converter(1);
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let (limit, _) = name.split_once("--").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let format = format_of(&path, &bytes).unwrap();
        let error = converter.convert(Input::new(&bytes, format)).unwrap_err();
        assert!(
            matches!(&error, ConvertError::ResourceLimit { limit: l, max, .. }
                if l.as_str() == limit && *max == default_max(limit)),
            "{name}: {error:?}"
        );
    }
}

#[test]
fn an_input_past_max_input_bytes_is_refused_before_any_parse() {
    let max = Limits::default().max_input_bytes;
    let over = vec![0u8; usize::try_from(max).unwrap() + 1];
    let converter = converter(1);
    let parsed = converter.parse(Input::new(&over, Format::Docx)).unwrap_err();
    let converted = converter.convert(Input::new(&over, Format::Docx)).unwrap_err();
    for error in [parsed, converted] {
        assert!(
            matches!(error, ConvertError::ResourceLimit { limit: Limit::InputBytes, max: m, .. } if m == max),
            "{error:?}"
        );
    }
    let at_max = &over[..over.len() - 1];
    let error = converter.convert(Input::new(at_max, Format::Docx)).unwrap_err();
    assert!(matches!(error, ConvertError::Malformed { .. }), "{error:?}");
}

#[test]
fn an_rtf_control_word_flood_trips_max_work() {
    let max_input = usize::try_from(Limits::default().max_input_bytes).unwrap();
    let header = b"{\\rtf1 ".as_slice();
    let flood = [header, &b"\\b".repeat((max_input - header.len()) / 2)].concat();
    let error = converter(1).convert(Input::new(&flood, Format::Rtf)).unwrap_err();
    let max = Limits::default().max_work;
    assert!(
        matches!(error, ConvertError::ResourceLimit { limit: Limit::Work, max: m, .. } if m == max),
        "{error:?}"
    );
}
