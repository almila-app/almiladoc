//! Each abuse input trips its limit at default `Limits`: a committed fixture
//! named `<limit>--<shape>--errors.<ext>`, or an input over 1 MiB built here.
//! An ordinary input as large as `max_input_bytes` converts.

use crate::build::{epub, odf, ole, record};
use crate::support::{
    configured, converter, count, fixture_root, format_of, limit_mut, limited, trips, walk,
    zip_package,
};
use almiladoc::{ConvertError, DiagnosticCode, Format, Input, Limit, Limits};
use std::assert_matches;
use std::io::Cursor;

#[test]
fn every_abuse_fixture_trips_the_limit_its_name_gives() {
    let mut files = Vec::new();
    walk(&fixture_root().join("abuse"), &mut files);
    assert_eq!(files.len(), 18, "abuse fixture set");
    let converter = converter(1);
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let (limit, _) = name.split_once("--").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let format = format_of(&path, &bytes).unwrap();
        let error = converter.convert(Input::new(&bytes, format)).unwrap_err();
        assert_matches!(
            &error,
            ConvertError::ResourceLimit { limit: l, max, .. }
                if l.as_str() == limit && *max == *limit_mut(&mut Limits::default(), *l),
            "{name}"
        );
    }
}

#[test]
fn an_input_past_max_input_bytes_is_refused_before_any_parse() {
    let max = Limits::default().max_input_bytes;
    let over = vec![0u8; usize::try_from(max).unwrap() + 1];
    let converter = converter(1);
    let parsed = converter.parse(Input::new(&over, Format::Docx)).map(drop);
    let converted = converter.convert(Input::new(&over, Format::Docx)).map(drop);
    for refusal in [parsed, converted] {
        assert!(trips(&refusal, Limit::InputBytes, max), "{refusal:?}");
    }
    let at_max = &over[..over.len() - 1];
    let error = converter.convert(Input::new(at_max, Format::Docx)).unwrap_err();
    assert_matches!(error, ConvertError::Malformed { .. });
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
    ole(&[("PowerPoint Document", records)])
}

#[test]
fn ppt_links_each_over_one_long_text_trip_max_work() {
    let max = Limits::default().max_work;
    let units: u32 = 1 << 22;
    let text = record(0, 0x0FA0, &b"a\0".repeat(units as usize));
    let mut info = [0u8; 16];
    info[4] = 1;
    info[8] = 4;
    let range = [0u32.to_le_bytes(), units.to_le_bytes()].concat();
    let link = [record(0, 0x0FF3, &info), record(0, 0x0FDF, &range)].concat();
    let links = usize::try_from(max / (2 * u64::from(units)) + 1).unwrap();
    let deck = ppt(&[text, link.repeat(links)].concat());
    assert!(deck.len() < 10 << 20);
    let refusal = converter(1).convert(Input::new(&deck, Format::Ppt));
    assert!(trips(&refusal, Limit::Work, max), "{refusal:?}");
}

#[test]
fn a_ppt_stream_of_max_records_empty_atoms_trips_max_records() {
    let max = Limits::default().max_records;
    let deck = ppt(&record(0, 0, &[]).repeat(usize::try_from(max + 1).unwrap()));
    let refusal = converter(1).convert(Input::new(&deck, Format::Ppt));
    assert!(trips(&refusal, Limit::Records, max), "{refusal:?}");
}

#[test]
fn a_csv_cell_escaped_past_max_output_bytes_trips_it() {
    let max = Limits::default().max_output_bytes;
    // `&#` renders as `&amp;#`, three bytes of Markdown a byte of input.
    let csv = b"&#".repeat(usize::try_from(max / 6 + 1).unwrap());
    assert!((csv.len() as u64) < Limits::default().max_input_bytes);
    let refusal = converter(1).convert(Input::new(&csv, Format::Csv));
    assert!(trips(&refusal, Limit::OutputBytes, max), "{refusal:?}");
}

#[test]
fn odf_tables_each_under_max_expansion_text_bytes_trip_it_together() {
    let max = Limits::default().max_expansion_text_bytes;
    let (text_bytes, columns) = (40_000u64, 1_000u64);
    let duplicated = text_bytes * (columns - 1);
    assert!(duplicated < max && 2 * duplicated > max);
    let table = format!(
        r#"<table:table><table:table-row><table:table-cell table:number-columns-repeated="{columns}"><text:p>{}</text:p></table:table-cell></table:table-row></table:table>"#,
        "x".repeat(usize::try_from(text_bytes).unwrap())
    );
    let one = odf(&format!("<office:text>{table}</office:text>"));
    let two = odf(&format!("<office:text>{table}{table}</office:text>"));
    converter(1).parse(Input::new(&one, Format::Odt)).unwrap();
    let refusal = converter(1).convert(Input::new(&two, Format::Odt));
    assert!(trips(&refusal, Limit::ExpansionTextBytes, max), "{refusal:?}");
}

#[test]
fn every_odf_page_and_table_counts_against_max_pages() {
    let table = |name: &str| {
        format!(
            r#"<table:table table:name="{name}"><table:table-row><table:table-cell><text:p>{name}</text:p></table:table-cell></table:table-row></table:table>"#
        )
    };
    let tables =
        format!("<office:spreadsheet>{}</office:spreadsheet>", ["A", "B", "C"].map(table).concat());
    let cases = [
        (
            odf("<office:presentation><draw:page/><draw:page/><draw:page/></office:presentation>"),
            Format::Odp,
        ),
        (odf(&tables), Format::Ods),
    ];
    for (bytes, format) in cases {
        let at = |max_pages| {
            configured(|config| config.limits.max_pages = max_pages)
                .convert(Input::new(&bytes, format))
                .map(drop)
        };
        assert!(at(3).is_ok(), "{format:?}");
        let refusal = at(2);
        assert!(trips(&refusal, Limit::Pages, 2), "{format:?}: {refusal:?}");
    }
}

const NOISE: usize = 128 << 10;
const LOCAL_HEADER_CRC: u64 = 14;
const CENTRAL_HEADER_CRC: u64 = 16;

/// `len` bytes that deflate cannot shrink, the same on every run.
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 32).to_le_bytes()[0]
        })
        .collect()
}

/// A zip of one entry of `len` zero bytes ending in noise, which keeps its
/// compression ratio under `max_compression_ratio`.
fn picture_zip(len: usize) -> Vec<u8> {
    let payload = [vec![0u8; len - NOISE], noise(NOISE)].concat();
    zip_package(&[("picture", payload)])
}

fn fail_checksums(data: &mut [u8], names: &[String]) {
    let mut at = Vec::new();
    let mut archive = zip::ZipArchive::new(Cursor::new(&*data)).unwrap();
    for name in names {
        let index = archive.index_for_name(name).unwrap();
        let file = archive.by_index_raw(index).unwrap();
        at.extend([
            file.header_start() + LOCAL_HEADER_CRC,
            file.central_header_start() + CENTRAL_HEADER_CRC,
        ]);
    }
    drop(archive);
    for start in at {
        let start = usize::try_from(start).unwrap();
        data[start..start + 4].iter_mut().for_each(|byte| *byte = !*byte);
    }
}

/// An EPUB of `entries` copies of the one entry of `picture`, each failing its
/// checksum and named `p<i>.png`, whose chapter holds an `<img>` per index of
/// `references`.
fn corrupt_picture_epub(picture: &[u8], entries: usize, references: &[usize]) -> Vec<u8> {
    let body: String = references.iter().map(|i| format!(r#"<img src="p{i}.png"/>"#)).collect();
    let names: Vec<String> = (0..entries).map(|i| format!("p{i}.png")).collect();
    let mut source = zip::ZipArchive::new(Cursor::new(picture)).unwrap();
    let mut writer = zip::ZipWriter::new_append(Cursor::new(epub(&body))).unwrap();
    for name in &names {
        writer.raw_copy_file_rename(source.by_index_raw(0).unwrap(), name).unwrap();
    }
    let mut data = writer.finish().unwrap().into_inner();
    fail_checksums(&mut data, &names);
    data
}

#[test]
fn distinct_corrupt_entries_past_max_total_bytes_trip_it() {
    let limits = Limits::default();
    let entries = usize::try_from(limits.max_total_bytes / limits.max_entry_bytes).unwrap() + 1;
    let picture = picture_zip(usize::try_from(limits.max_entry_bytes).unwrap());
    let epub = corrupt_picture_epub(&picture, entries, &(0..entries).collect::<Vec<_>>());
    assert!(epub.len() > 1 << 20, "{} bytes", epub.len());
    let refusal = converter(1).convert(Input::new(&epub, Format::Epub));
    assert!(trips(&refusal, Limit::TotalBytes, limits.max_total_bytes), "{refusal:?}");
}

#[test]
fn references_to_one_corrupt_entry_inflate_it_once_and_each_reports_its_loss() {
    let (kib, references) = (512u64, 100);
    let picture = picture_zip(usize::try_from(kib << 10).unwrap());
    let epub = corrupt_picture_epub(&picture, 1, &vec![0; references]);
    let converter = limited(|limits| limits.max_work = 2 * kib);
    let parsed = converter.parse(Input::new(&epub, Format::Epub)).unwrap();
    let lost = count(parsed.diagnostics(), |d| d.code == DiagnosticCode::ImageUnavailable);
    assert_eq!(lost, u64::try_from(references).unwrap());
}
