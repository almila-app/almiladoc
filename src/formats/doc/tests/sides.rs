//! Tests of the section headers and footers a document writes under `HeaderFooterPolicy::Emit`.

use super::*;
use crate::limits::Limits;

type Row = (DiagnosticCode, Region, u64);
/// A header's text, its field table, the header it writes and what it reports.
type Case<'a> = (&'a [u8], Option<Vec<u8>>, &'a str, &'a [Row]);

fn converted(bytes: &[u8], policy: HeaderFooterPolicy) -> (String, Vec<Row>) {
    let limits = Limits::default();
    let ctx = ParseCtx::new(&limits).with_headers_footers(policy);
    let document = parse(&ctx, bytes).unwrap();
    (markdown_of_document(&document), diagnostic_rows(&ctx.into_diagnostics()))
}

/// A document of `main` text then `headers` text, its header stories at the CPs `hdd` names,
/// and its sections, when `sections` is given, ending at those main CPs, the first showing
/// a first page.
fn sectioned(main: &[u8], headers: &[u8], hdd: &[u32], sections: Option<&[u32]>) -> Vec<u8> {
    let (main_len, hdd_len) = (main.len() as u32, headers.len() as u32);
    let text = [main, headers].concat();
    let mut doc = DocBuilder::new(b"")
        .fib(0x4C, main_len)
        .fib(0x54, hdd_len)
        .pieces(&[(main_len + hdd_len, &text, false, 0)])
        .table(0xF2, &words(hdd));
    if let Some(ends) = sections {
        let sepx_fc = doc.word_doc.len() as u32;
        doc.word_doc.extend_from_slice(&[0x03, 0x00, 0x0A, 0x30, 0x01]);
        let cps: Vec<u32> = [0].into_iter().chain(ends.iter().copied()).collect();
        let seds: Vec<u8> = (0..ends.len())
            .flat_map(|i| {
                let fc = if i == 0 { sepx_fc } else { u32::MAX };
                [&[0u8, 0][..], &fc.to_le_bytes(), &[0, 0], &[0; 4]].concat()
            })
            .collect();
        doc = doc.table(0xCA, &[words(&cps), seds].concat());
    }
    doc.bytes()
}

fn side(n: u32, region: &str, variant: &str, text: &str) -> String {
    let label = if region == "header" { "Header" } else { "Footer" };
    let body = if text.is_empty() { String::new() } else { format!(">\n> {text}\n") };
    format!("> <a id=\"almiladoc:{region}:{variant}:{n}\"></a>{label} ({variant}):\n{body}\n")
}

#[test]
fn each_section_shows_the_stories_it_names_and_inherits_those_it_leaves_empty() {
    let hdd = [0, 0, 0, 0, 0, 0, 0, 0, 5, 5, 10, 16, 16, 16, 23, 23, 23, 23, 23, 24];
    let bytes = sectioned(b"One\x0cTwo\r", b"Head\rFoot\rCover\rSecond\r\r", &hdd, Some(&[4, 8]));
    let (markdown, rows) = converted(&bytes, HeaderFooterPolicy::Emit);
    let uses_default = |n: u32, region: &str, label: &str| {
        format!("> <a id=\"almiladoc:{region}:first:{n}\"></a>{label} (first, uses default):\n\n")
    };
    assert_eq!(
        markdown,
        [
            side(1, "header", "default", "Head"),
            side(2, "header", "first", "Cover"),
            side(3, "footer", "default", "Foot"),
            side(4, "footer", "first", ""),
            "One\n\n".into(),
            side(5, "header", "default", "Second"),
            uses_default(6, "header", "Header"),
            uses_default(7, "footer", "Footer"),
            "Two\n".into(),
        ]
        .concat()
    );
    assert_eq!(rows, []);
    let (omitted, rows) = converted(&bytes, HeaderFooterPolicy::Omit);
    assert_eq!(omitted, "One\n\nTwo\n");
    assert_eq!(
        rows,
        [
            (DiagnosticCode::HeaderOmitted, Region::Header, 3),
            (DiagnosticCode::FooterOmitted, Region::Footer, 1)
        ]
    );
}

#[test]
fn a_header_page_field_writes_its_token_and_none_of_its_cached_result() {
    let hdd = [0, 0, 0, 0, 0, 0, 0, 0, 11, 11, 11, 11, 11, 12];
    let bytes = sectioned(b"Body\r", b"P \x13PAGE\x14\xff\x15\r\r", &hdd, None);
    let (markdown, rows) = converted(&bytes, HeaderFooterPolicy::Emit);
    assert_eq!(markdown, [side(1, "header", "default", "P {page}"), "Body\n".into()].concat());
    assert_eq!(rows, [(DiagnosticCode::FieldToken, Region::Header, 1)]);
}

#[test]
fn a_selector_or_span_that_does_not_read_costs_only_the_story_it_would_show() {
    let hdd = [0, 0, 0, 0, 0, 0, 0, 0, 5, 5, 5, 11, 11, 12];
    let header = b"Head\rCover\r\r";
    let mut corrupt_sepx = DocBuilder::new(b"").fib(0x4C, 5).fib(0x54, header.len() as u32);
    corrupt_sepx = corrupt_sepx
        .pieces(&[(5 + header.len() as u32, &[&b"Body\r"[..], header].concat(), false, 0)])
        .table(0xF2, &words(&hdd))
        .table(
            0xCA,
            &[words(&[0, 5]), vec![0, 0], 0x00FF_FFFFu32.to_le_bytes().to_vec(), vec![0; 6]]
                .concat(),
        );
    let (markdown, rows) = converted(&corrupt_sepx.bytes(), HeaderFooterPolicy::Emit);
    assert_eq!(
        markdown,
        [
            side(1, "header", "default", "Head"),
            side(2, "header", "first", "Cover"),
            "Body\n".into()
        ]
        .concat()
    );
    assert_eq!(rows, [(DiagnosticCode::PartUnreadable, Region::Header, 1)]);
    let past = [0, 0, 0, 0, 0, 0, 0, 0, 99, 99, 99, 99, 99, 99];
    let (markdown, rows) =
        converted(&sectioned(b"Body\r", b"Head\r\r", &past, None), HeaderFooterPolicy::Emit);
    assert_eq!(markdown, [side(1, "header", "default", ""), "Body\n".into()].concat());
    assert_eq!(rows, [(DiagnosticCode::PartUnreadable, Region::Header, 1)]);
}

#[test]
fn a_document_without_header_stories_reads_its_body_alone_whatever_its_section_data() {
    let bytes = DocBuilder::new(b"Body\r")
        .fib(0xCA, 999)
        .fib(0xCE, 40)
        .fib(0x192, 999)
        .fib(0x196, 8)
        .bytes();
    assert_eq!(converted(&bytes, HeaderFooterPolicy::Emit), ("Body\n".into(), Vec::new()));
}

#[test]
fn each_sepx_is_charged_by_its_length_once_however_many_sections_name_it() {
    let fuel = |sepx_len: usize, sections: usize| {
        let hdd = [0, 0, 0, 0, 0, 0, 0, 0, 5, 5, 5, 5, 5, 6];
        let mut doc = DocBuilder::new(b"").fib(0x4C, 8).fib(0x54, 6);
        doc = doc.pieces(&[(14, b"One\x0cTwo\rHead\r\r", false, 0)]).table(0xF2, &words(&hdd));
        let fc = doc.word_doc.len() as u32;
        let grpprl: Vec<u8> = std::iter::repeat_n([0x0A, 0x30, 0x00], sepx_len).flatten().collect();
        doc.word_doc.extend_from_slice(&(grpprl.len() as u16).to_le_bytes());
        doc.word_doc.extend_from_slice(&grpprl);
        let ends: Vec<u32> = if sections == 1 { vec![8] } else { vec![4, 8] };
        let cps: Vec<u32> = [0].into_iter().chain(ends.iter().copied()).collect();
        let seds: Vec<u8> =
            ends.iter().flat_map(|_| [&[0, 0][..], &fc.to_le_bytes(), &[0; 6]].concat()).collect();
        let bytes = doc.table(0xCA, &[words(&cps), seds].concat()).bytes();
        let limits = Limits::default();
        let ctx = ParseCtx::new(&limits).with_headers_footers(HeaderFooterPolicy::Emit);
        parse(&ctx, &bytes).unwrap();
        ctx.fuel()
    };
    assert!(fuel(1000, 1) >= fuel(1, 1) + 2997, "a Sepx is charged by its length");
    assert!(fuel(1000, 2) < fuel(1000, 1) + 1000, "a Sepx named twice is read once");
}

/// A `PlcfFldHdr` naming each field delimiter of `headers`, the `n`th end carrying `ends[n]`
/// as its `grffldEnd`, and `last` as its final CP.
fn field_table(headers: &[u8], ends: &[u8], last: u32) -> Vec<u8> {
    let delimiters: Vec<(u32, u8)> = (0u32..)
        .zip(headers.iter().copied())
        .filter(|&(_, ch)| matches!(ch, 0x13..=0x15))
        .collect();
    let mut ends = ends.iter().copied();
    let flds: Vec<u8> = delimiters
        .iter()
        .flat_map(|&(_, ch)| [ch, if ch == 0x15 { ends.next().unwrap_or(0) } else { 0 }])
        .collect();
    let cps: Vec<u32> = delimiters.iter().map(|&(cp, _)| cp).chain([last]).collect();
    [words(&cps), flds].concat()
}

/// A document whose one default header holds `headers`, with `fields` as its `PlcfFldHdr`.
fn fielded(headers: &[u8], fields: Option<Vec<u8>>) -> Vec<u8> {
    let n = headers.len() as u32;
    let hdd = [0, 0, 0, 0, 0, 0, 0, 0, n - 1, n - 1, n - 1, n - 1, n - 1, n];
    let text = [&b"Body\r"[..], headers].concat();
    let mut doc = DocBuilder::new(b"")
        .fib(0x4C, 5)
        .fib(0x54, n)
        .pieces(&[(5 + n, &text, false, 0)])
        .table(0xF2, &words(&hdd));
    if let Some(fields) = fields {
        doc = doc.table(0x122, &fields);
    }
    doc.bytes()
}

#[test]
fn a_locked_header_field_keeps_the_result_it_stores() {
    let date = b"D \x13DATE\x142.1.2020\x15\r\r".as_slice();
    let nested = b"\x13DATE\x14\x13PAGE\x143\x15\x15\r\r".as_slice();
    let twice = b"\x13DATE\x14a\x15 \x13DATE\x14b\x15\r\r".as_slice();
    let token = [(DiagnosticCode::FieldToken, Region::Header, 1)];
    let table =
        |headers: &[u8], ends: &[u8]| Some(field_table(headers, ends, headers.len() as u32));
    let cases: [Case<'_>; 9] = [
        (date, None, "D {date}", &token),
        (date, table(date, &[0x00]), "D {date}", &token),
        (date, table(date, &[0xEF]), "D {date}", &token),
        (date, table(date, &[0x10]), "D 2.1.2020", &[]),
        (date, Some(field_table(date, &[0x10], u32::MAX)), "D 2.1.2020", &[]),
        (nested, table(nested, &[0x00, 0x10]), "{page}", &token),
        (nested, table(nested, &[0x10, 0x00]), "{date}", &token),
        (twice, table(twice, &[0x00, 0x10]), "{date} b", &token),
        (twice, table(twice, &[0x10, 0x00]), "a {date}", &token),
    ];
    for (headers, fields, text, rows) in cases {
        let (markdown, reported) = converted(&fielded(headers, fields), HeaderFooterPolicy::Emit);
        assert_eq!(markdown, [side(1, "header", "default", text), "Body\n".into()].concat());
        assert_eq!(reported, rows, "{text}");
    }
}

#[test]
fn a_field_table_that_does_not_read_costs_only_the_fields_whose_token_relies_on_it() {
    let date = b"D \x13DATE\x142.1.2020\x15\r\r".as_slice();
    let kept = b"\x13REF x\x14kept\x15\r\r".as_slice();
    let unpaired =
        [words(&[0, 2, 7, 16, 19]), vec![0x15, 0, 0x13, 0, 0x14, 0, 0x15, 0x10]].concat();
    let mut elsewhere = field_table(date, &[0x10], date.len() as u32);
    elsewhere[..4].copy_from_slice(&1u32.to_le_bytes());
    let mut unordered = field_table(date, &[0x10], date.len() as u32);
    unordered[4..8].copy_from_slice(&0u32.to_le_bytes());
    let mut outside = field_table(date, &[0x10], date.len() as u32);
    outside[8..12].copy_from_slice(&99u32.to_le_bytes());
    let torn = [field_table(date, &[0x10], date.len() as u32), vec![0]].concat();
    let open = [words(&[2, 7, 16, 17, 19]), vec![0x13, 0, 0x14, 0, 0x15, 0x10, 0x13, 0]].concat();
    let inside = [words(&[2, 7, 12, 19]), vec![0x13, 0, 0x14, 0, 0x15, 0x10]].concat();
    let stray = b"\x14D \x13DATE\x142.1.2020\x15\r\r".as_slice();
    let unread = [
        (DiagnosticCode::PartUnreadable, Region::Header, 1),
        (DiagnosticCode::FieldToken, Region::Header, 1),
    ];
    for (name, headers, fields) in [
        ("an end before its begin", date, unpaired),
        ("no entry at the begin", date, elsewhere),
        ("unordered", date, unordered),
        ("past the header document", date, outside),
        ("a torn record", date, torn.clone()),
        ("a begin left open", date, open),
        ("an end inside the result text", date, inside),
        ("a separator outside any field", stray, field_table(stray, &[0x10], stray.len() as u32)),
    ] {
        let (markdown, rows) = converted(&fielded(headers, Some(fields)), HeaderFooterPolicy::Emit);
        assert_eq!(markdown, [side(1, "header", "default", "D {date}"), "Body\n".into()].concat());
        assert_eq!(rows, unread, "{name}");
    }
    let (markdown, rows) = converted(&fielded(kept, Some(torn)), HeaderFooterPolicy::Emit);
    assert_eq!(
        (markdown, rows),
        ([side(1, "header", "default", "kept"), "Body\n".into()].concat(), vec![])
    );
}

#[test]
fn a_field_record_that_does_not_nest_in_a_story_no_page_shows_costs_nothing() {
    let even = b"E \x13PAGE\x14 1\r".as_slice();
    let default = b"D \x13DATE\x142.1.2020\x15\r".as_slice();
    let headers = [even, default, b"\r"].concat();
    let (e, d) = (even.len() as u32, (even.len() + default.len()) as u32);
    let hdd = [0, 0, 0, 0, 0, 0, 0, e, d, d, d, d, d, d + 1];
    let n = headers.len() as u32;
    let bytes = DocBuilder::new(b"")
        .fib(0x4C, 5)
        .fib(0x54, n)
        .pieces(&[(5 + n, &[&b"Body\r"[..], &headers].concat(), false, 0)])
        .table(0xF2, &words(&hdd))
        .table(0x192, &[0, 0])
        .table(0x122, &field_table(&headers, &[0x10], n))
        .bytes();
    let (markdown, rows) = converted(&bytes, HeaderFooterPolicy::Emit);
    assert_eq!(
        (markdown, rows),
        ([side(1, "header", "default", "D 2.1.2020"), "Body\n".into()].concat(), vec![])
    );
}
