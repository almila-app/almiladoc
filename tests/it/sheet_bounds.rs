//! A sheet's grid spans its content-bearing cells at default `Limits`: a
//! whitespace-only cell widens nothing, whether it sits alone at the grid's
//! last row or fills a blank column down to it, while content in opposite
//! corners still exceeds `max_grid_slots`. An xlsx worksheet's rows are
//! read one at a time, so `max_xml_nodes` bounds a row and the structure
//! around the rows, never the row count.

use crate::build::{ole, relationships};
use crate::support::{configured, fixture, zip_package};
use almiladoc::model::{Block, Table, UnitKind};
use almiladoc::{ConvertError, Converter, Format, Input, Limit, Parsed};
use std::assert_matches;
use std::fmt::Write as _;

const XLSB_ROWS: u32 = 1 << 20;
const XLS_ROWS: u32 = 1 << 16;

fn parse(converter: &Converter, bytes: &[u8]) -> Result<Parsed, ConvertError> {
    converter.parse(Input::new(bytes, Format::Excel))
}

fn tables(parsed: &Parsed) -> Vec<String> {
    let blocks = &parsed.document().blocks;
    blocks.iter().filter(|b| matches!(b, Block::Table(_))).map(|b| format!("{b:?}")).collect()
}

fn assert_exceeds(limit: Limit, name: &str, parsed: Result<Parsed, ConvertError>) {
    assert_matches!(
        parsed.err(),
        Some(ConvertError::ResourceLimit { limit: tripped, .. }) if tripped == limit,
        "{name}"
    );
}

fn assert_content_table(name: &str, parsed: Result<Parsed, ConvertError>) -> Parsed {
    let parsed = parsed.unwrap_or_else(|e| panic!("{name}: {e:?}"));
    assert!(parsed.diagnostics().is_empty(), "{name}: {:?}", parsed.diagnostics());
    let tables: Vec<_> = parsed
        .document()
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Table(t) => Some((t.grid.len(), t.grid.iter().map(Vec::len).max())),
            _ => None,
        })
        .collect();
    assert_eq!(tables, [(10, Some(5))], "{name}");
    parsed
}

fn sheet_table(parsed: &Parsed) -> &Table {
    let [Block::UnitStart(unit), Block::Table(table)] = parsed.document().blocks.as_slice() else {
        panic!("{:?}", parsed.document().blocks);
    };
    assert_eq!((unit.kind, unit.ordinal.get()), (UnitKind::Sheet, 1));
    table
}

/// A 10 by 5 table, with a single space in column F of every row.
fn filled_down(row: u32) -> Vec<(u16, String)> {
    let content = (0..5u8)
        .filter(|_| row < 10)
        .map(|col| (u16::from(col), format!("{}{}", char::from(b'A' + col), row + 1)));
    content.chain([(5, " ".to_string())]).collect()
}

/// Content in the first cell and in the grid's far corner.
fn corners(last_row: u32, last_col: u16) -> impl Fn(u32) -> Vec<(u16, String)> {
    move |row| match row {
        0 => vec![(0, "x".to_string())],
        _ if row == last_row => vec![(last_col, "x".to_string())],
        _ => Vec::new(),
    }
}

#[test]
fn a_whitespace_cell_at_the_last_row_leaves_the_table_its_content() {
    let converter = configured(|_| {});
    for name in ["xlsx", "xlsb", "xls"].map(|ext| format!("{ext}/handmade-trailing-space.{ext}")) {
        assert_content_table(&name, parse(&converter, &fixture(&name)));
    }
}

#[test]
fn a_blank_column_filled_down_to_the_last_row_leaves_the_table_its_content() {
    let converter = configured(|_| {});
    let xlsb = assert_content_table("xlsb", parse(&converter, &xlsb(XLSB_ROWS, filled_down)));
    let xlsx = parse(&converter, &xlsx(&sheet_data(XLSB_ROWS, filled_down), None));
    let xlsx = assert_content_table("xlsx", xlsx);
    assert_eq!(tables(&xlsx), tables(&xlsb));
    assert_content_table("xls", parse(&converter, &xls(XLS_ROWS, filled_down)));
}

#[test]
fn an_xlsx_of_a_hundred_thousand_rows_of_ten_values_converts() {
    let values =
        |row: u32| (0..10u16).map(|col| (col, format!("{}", row * 10 + u32::from(col)))).collect();
    let converter = configured(|_| {});
    let parsed = parse(&converter, &xlsx(&sheet_data(100_000, values), None)).unwrap();
    assert!(parsed.diagnostics().is_empty(), "{:?}", parsed.diagnostics());
    let table = sheet_table(&parsed);
    assert_eq!((table.grid.len(), table.grid[99_999].len()), (100_000, 10));
}

#[test]
fn many_small_rows_fit_a_node_limit_one_row_cannot_pass() {
    let converter = configured(|config| config.limits.max_xml_nodes = 40);
    let one_value = |row: u32| vec![(0, format!("{row}"))];
    let parsed = parse(&converter, &xlsx(&sheet_data(100, one_value), None)).unwrap();
    assert_eq!(sheet_table(&parsed).grid.len(), 100);
    let wide = |_| (0..20u16).map(|col| (col, "1".to_string())).collect();
    assert_exceeds(Limit::XmlNodes, "row", parse(&converter, &xlsx(&sheet_data(1, wide), None)));
}

#[test]
fn structure_beside_the_rows_and_other_parts_keep_the_node_limit() {
    let converter = configured(|config| config.limits.max_xml_nodes = 40);
    let one_value = |_| vec![(0, "1".to_string())];
    let cols: String =
        (1..=40).map(|c| format!(r#"<col min="{c}" max="{c}" width="9"/>"#)).collect();
    let around = format!("<cols>{cols}</cols>{}", sheet_data(1, one_value));
    assert_exceeds(Limit::XmlNodes, "cols", parse(&converter, &xlsx(&around, None)));
    let strings = "<si><t>a</t></si>".repeat(20);
    let shared = xlsx(&sheet_data(1, one_value), Some(&strings));
    assert_exceeds(Limit::XmlNodes, "shared strings", parse(&converter, &shared));
}

#[test]
fn content_in_opposite_corners_still_exceeds_the_grid_slots() {
    let converter = configured(|_| {});
    let corners_xlsx = xlsx(&sheet_data(XLSB_ROWS, corners(XLSB_ROWS - 1, 16_383)), None);
    assert_exceeds(Limit::GridSlots, "xlsx", parse(&converter, &corners_xlsx));
    let corners_xlsb = xlsb(XLSB_ROWS, corners(XLSB_ROWS - 1, 16_383));
    assert_exceeds(Limit::GridSlots, "xlsb", parse(&converter, &corners_xlsb));
    let corners_xls = xls(XLS_ROWS, corners(XLS_ROWS - 1, 255));
    assert_exceeds(Limit::GridSlots, "xls", parse(&converter, &corners_xls));
}

fn xlsb_rec(out: &mut Vec<u8>, id: u16, payload: &[u8]) {
    if id < 0x80 {
        out.push(u8::try_from(id).unwrap());
    } else {
        out.extend_from_slice(&[
            u8::try_from(id & 0x7F).unwrap() | 0x80,
            u8::try_from(id >> 7).unwrap(),
        ]);
    }
    let mut size = payload.len();
    while size >= 0x80 {
        out.push(u8::try_from(size & 0x7F).unwrap() | 0x80);
        size >>= 7;
    }
    out.push(u8::try_from(size).unwrap());
    out.extend_from_slice(payload);
}

fn xlsb_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&u32::try_from(s.encode_utf16().count()).unwrap().to_le_bytes());
    out.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
}

fn xlsb(rows: u32, cells: impl Fn(u32) -> Vec<(u16, String)>) -> Vec<u8> {
    let mut sheet = Vec::new();
    for row in 0..rows {
        let mut header = row.to_le_bytes().to_vec();
        header.extend_from_slice(&[0; 13]);
        xlsb_rec(&mut sheet, 0, &header);
        for (col, text) in cells(row) {
            let mut cell = u32::from(col).to_le_bytes().to_vec();
            cell.extend_from_slice(&[0; 4]);
            xlsb_str(&mut cell, &text);
            xlsb_rec(&mut sheet, 6, &cell);
        }
    }
    let mut file_version = vec![0; 8];
    xlsb_str(&mut file_version, "");
    let mut bundle_sheet = vec![0, 0, 0, 0, 1, 0, 0, 0];
    xlsb_str(&mut bundle_sheet, "rId1");
    xlsb_str(&mut bundle_sheet, "Filled");
    let mut workbook = Vec::new();
    xlsb_rec(&mut workbook, 153, &file_version);
    xlsb_rec(&mut workbook, 156, &bundle_sheet);
    zip_package(&[
        (
            "_rels/.rels",
            relationships(&[("rId1", "officeDocument", "xl/workbook.bin")]).into_bytes(),
        ),
        ("xl/workbook.bin", workbook),
        (
            "xl/_rels/workbook.bin.rels",
            relationships(&[("rId1", "worksheet", "worksheets/sheet1.bin")]).into_bytes(),
        ),
        ("xl/worksheets/sheet1.bin", sheet),
    ])
}

/// The column letters of a zero-based column.
fn column(col: u16) -> String {
    let mut n = u32::from(col) + 1;
    let mut letters = Vec::new();
    while n > 0 {
        let rem = (n - 1) % 26;
        letters.push(char::from(b'A' + u8::try_from(rem).unwrap_or_default()));
        n = (n - 1) / 26;
    }
    letters.iter().rev().collect()
}

/// A `sheetData` of `rows` rows, one per line: a numeric text as a value,
/// any other as an inline string.
fn sheet_data(rows: u32, cells: impl Fn(u32) -> Vec<(u16, String)>) -> String {
    let mut out = String::from("<sheetData>");
    for row in 0..rows {
        let r = row + 1;
        write!(out, "\n<row r=\"{r}\">").unwrap();
        for (col, text) in cells(row) {
            let at = format!("{}{r}", column(col));
            if text.parse::<f64>().is_ok() {
                write!(out, r#"<c r="{at}"><v>{text}</v></c>"#).unwrap();
            } else {
                write!(
                    out,
                    r#"<c r="{at}" t="inlineStr"><is><t xml:space="preserve">{text}</t></is></c>"#
                )
                .unwrap();
            }
        }
        out.push_str("</row>");
    }
    out.push_str("\n</sheetData>");
    out
}

/// A one-sheet xlsx whose worksheet holds `body`, with a shared string table
/// of `shared` items where given.
fn xlsx(body: &str, shared: Option<&str>) -> Vec<u8> {
    const SML: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    let workbook = format!(
        r#"<?xml version="1.0"?><workbook xmlns="{SML}" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Filled" sheetId="1" r:id="rId1"/></sheets></workbook>"#
    );
    let worksheet = format!(r#"<?xml version="1.0"?><worksheet xmlns="{SML}">{body}</worksheet>"#);
    let mut workbook_rels = vec![("rId1", "worksheet", "worksheets/sheet1.xml")];
    let mut parts = vec![
        (
            "_rels/.rels",
            relationships(&[("rId1", "officeDocument", "xl/workbook.xml")]).into_bytes(),
        ),
        ("xl/workbook.xml", workbook.into_bytes()),
        ("xl/worksheets/sheet1.xml", worksheet.into_bytes()),
    ];
    if let Some(items) = shared {
        workbook_rels.push(("rId2", "sharedStrings", "sharedStrings.xml"));
        let sst = format!(r#"<?xml version="1.0"?><sst xmlns="{SML}">{items}</sst>"#);
        parts.push(("xl/sharedStrings.xml", sst.into_bytes()));
    }
    parts.push(("xl/_rels/workbook.xml.rels", relationships(&workbook_rels).into_bytes()));
    zip_package(&parts)
}

fn biff(out: &mut Vec<u8>, kind: u16, body: &[u8]) {
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
    out.extend_from_slice(body);
}

fn biff_bof(out: &mut Vec<u8>, substream: u16) {
    let mut body = [0; 16];
    body[..2].copy_from_slice(&0x0600u16.to_le_bytes());
    body[2..4].copy_from_slice(&substream.to_le_bytes());
    biff(out, 0x0809, &body);
}

fn xls(rows: u32, cells: impl Fn(u32) -> Vec<(u16, String)>) -> Vec<u8> {
    let mut sheet = Vec::new();
    biff_bof(&mut sheet, 0x0010);
    for row in 0..rows {
        for (col, text) in cells(row) {
            let mut label = Vec::new();
            for field in [u16::try_from(row).unwrap(), col, 0, u16::try_from(text.len()).unwrap()] {
                label.extend_from_slice(&field.to_le_bytes());
            }
            label.push(0);
            label.extend_from_slice(text.as_bytes());
            biff(&mut sheet, 0x0204, &label);
        }
    }
    biff(&mut sheet, 0x000A, &[]);
    let name = b"Filled";
    let mut globals = Vec::new();
    biff_bof(&mut globals, 0x0005);
    biff(&mut globals, 0x00E0, &[0; 20]);
    let sheet_at = globals.len() + 4 + 8 + name.len() + 4;
    let mut boundsheet = u32::try_from(sheet_at).unwrap().to_le_bytes().to_vec();
    boundsheet.extend_from_slice(&[0, 0, u8::try_from(name.len()).unwrap(), 0]);
    boundsheet.extend_from_slice(name);
    biff(&mut globals, 0x0085, &boundsheet);
    biff(&mut globals, 0x000A, &[]);
    globals.extend_from_slice(&sheet);
    ole(&[("Workbook", &globals)])
}
