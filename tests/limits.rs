//! Each abuse input trips its limit at default `Limits`: a committed fixture
//! named `<limit>--<shape>--errors.<ext>`, or an input over 1 MiB built here.
//! An ordinary input as large as `max_input_bytes` converts.

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
    assert_eq!(files.len(), 17, "abuse fixture set");
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

/// An RTF of one picture in Word's hex form, 128 digits a line, filling
/// `max_input_bytes`: one token per byte.
fn hex_picture_rtf() -> Vec<u8> {
    let max_input = usize::try_from(Limits::default().max_input_bytes).unwrap();
    let head = b"{\\rtf1\\ansi\\ansicpg1252{\\fonttbl{\\f0 Calibri;}}\\pard Figure\\par{\\pict\\pngblip\\picw64\\pich64\r\n89504e470d0a1a0a".as_slice();
    let tail = b"}\\par}".as_slice();
    let line = [b"0".repeat(128).as_slice(), b"\r\n"].concat();
    let mut rtf = head.to_vec();
    while rtf.len() + line.len() + tail.len() <= max_input {
        rtf.extend_from_slice(&line);
    }
    rtf.resize(max_input - tail.len(), b'0');
    rtf.extend_from_slice(tail);
    rtf
}

#[test]
fn a_hex_picture_rtf_at_max_input_bytes_converts() {
    let rtf = hex_picture_rtf();
    let parsed = converter(1).parse(Input::new(&rtf, Format::Rtf)).unwrap();
    assert_eq!(parsed.document().assets.len(), 1);
}

/// A legacy PowerPoint file whose `PowerPoint Document` stream is `records`,
/// with no `Current User` stream, so it is read in stream order.
fn ppt(records: &[u8]) -> Vec<u8> {
    let mut file = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
    std::io::Write::write_all(&mut file.create_stream("PowerPoint Document").unwrap(), records)
        .unwrap();
    file.flush().unwrap();
    file.into_inner().into_inner()
}

fn atom(rec_type: u16, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len()).unwrap().to_le_bytes();
    [&0u16.to_le_bytes()[..], &rec_type.to_le_bytes(), &len, body].concat()
}

#[test]
fn ppt_links_each_over_one_long_text_trip_max_work() {
    let max = Limits::default().max_work;
    let units: u32 = 1 << 22;
    let text = atom(0x0FA0, &b"a\0".repeat(units as usize));
    let mut info = [0u8; 16];
    info[4] = 1;
    info[8] = 4;
    let range = [0u32.to_le_bytes(), units.to_le_bytes()].concat();
    let link = [atom(0x0FF3, &info), atom(0x0FDF, &range)].concat();
    let links = usize::try_from(max / (2 * u64::from(units)) + 1).unwrap();
    let deck = ppt(&[text, link.repeat(links)].concat());
    assert!(deck.len() < 10 << 20);
    let error = converter(1).convert(Input::new(&deck, Format::Ppt)).unwrap_err();
    assert!(
        matches!(error, ConvertError::ResourceLimit { limit: Limit::Work, max: m, .. } if m == max),
        "{error:?}"
    );
}

#[test]
fn a_ppt_stream_of_max_records_empty_atoms_trips_max_records() {
    let max = Limits::default().max_records;
    let deck = ppt(&atom(0, &[]).repeat(usize::try_from(max + 1).unwrap()));
    let error = converter(1).convert(Input::new(&deck, Format::Ppt)).unwrap_err();
    assert!(
        matches!(error, ConvertError::ResourceLimit { limit: Limit::Records, max: m, .. } if m == max),
        "{error:?}"
    );
}
