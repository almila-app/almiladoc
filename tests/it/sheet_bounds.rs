//! A sheet's grid spans its content-bearing cells at default `Limits`: a
//! whitespace-only cell widens nothing, whether it sits alone at the grid's
//! last row or fills a blank column down to it, while content in opposite
//! corners still exceeds `max_grid_slots`. An xlsx worksheet's rows are
//! read one at a time, so `max_xml_nodes` bounds a row and the structure
//! around the rows, never the row count.

use crate::support::{fixture, zip_package};
use almiladoc::model::Block;
use almiladoc::{Config, ConvertError, Converter, Format, Input, Limit, Parsed};
use std::assert_matches;
use std::error::Error;
use std::io::{Cursor, Write};

const XLSB_ROWS: u32 = 1 << 20;
const XLS_ROWS: u32 = 1 << 16;

type Built<T> = Result<T, Box<dyn Error>>;

fn parse(converter: &Converter, bytes: &[u8]) -> Result<Parsed, ConvertError> {
    converter.parse(Input::new(bytes, Format::Excel))
}

fn with_xml_nodes(max: u64) -> Converter {
    let mut config = Config::default();
    config.limits.max_xml_nodes = max;
    Converter::new(config).unwrap()
}

fn tables(parsed: &Parsed) -> Vec<String> {
    let blocks = &parsed.document().blocks;
    blocks.iter().filter(|b| matches!(b, Block::Table(_))).map(|b| format!("{b:?}")).collect()
}

fn assert_exceeds_xml_nodes(name: &str, parsed: Result<Parsed, ConvertError>) {
    let err = parsed.err();
    assert_matches!(
        err,
        Some(ConvertError::ResourceLimit { limit: Limit::XmlNodes, .. }),
        "{name}"
    );
}

fn assert_content_table(name: &str, parsed: &Parsed) {
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
}

fn assert_exceeds_grid_slots(name: &str, parsed: Result<Parsed, ConvertError>) {
    let err = parsed.err();
    assert_matches!(
        err,
        Some(ConvertError::ResourceLimit { limit: Limit::GridSlots, .. }),
        "{name}"
    );
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
    let converter = Converter::new(Config::default()).unwrap();
    for name in ["xlsx", "xlsb", "xls"].map(|ext| format!("{ext}/handmade-trailing-space.{ext}")) {
        let parsed = parse(&converter, &fixture(&name));
        assert_content_table(&name, &parsed.unwrap_or_else(|e| panic!("{name}: {e:?}")));
    }
}

#[test]
fn a_blank_column_filled_down_to_the_last_row_leaves_the_table_its_content() {
    let converter = Converter::new(Config::default()).unwrap();
    let xlsb = parse(&converter, &xlsb(XLSB_ROWS, filled_down).unwrap());
    let xlsb = xlsb.unwrap_or_else(|e| panic!("xlsb: {e:?}"));
    assert_content_table("xlsb", &xlsb);
    let xlsx = parse(&converter, &xlsx(&sheet_data(XLSB_ROWS, filled_down), None).unwrap());
    let xlsx = xlsx.unwrap_or_else(|e| panic!("xlsx: {e:?}"));
    assert_content_table("xlsx", &xlsx);
    assert_eq!(tables(&xlsx), tables(&xlsb));
    let parsed = parse(&converter, &xls(XLS_ROWS, filled_down).unwrap());
    assert_content_table("xls", &parsed.unwrap_or_else(|e| panic!("xls: {e:?}")));
}

#[test]
fn an_xlsx_of_a_hundred_thousand_rows_of_ten_values_converts() {
    let values =
        |row: u32| (0..10u16).map(|col| (col, format!("{}", row * 10 + u32::from(col)))).collect();
    let converter = Converter::new(Config::default()).unwrap();
    let parsed = parse(&converter, &xlsx(&sheet_data(100_000, values), None).unwrap()).unwrap();
    assert!(parsed.diagnostics().is_empty(), "{:?}", parsed.diagnostics());
    let [Block::Table(table)] = parsed.document().blocks.as_slice() else {
        panic!("{:?}", parsed.document().blocks.len());
    };
    assert_eq!((table.grid.len(), table.grid[99_999].len()), (100_000, 10));
}

#[test]
fn many_small_rows_fit_a_node_limit_one_row_cannot_pass() {
    let converter = with_xml_nodes(40);
    let one_value = |row: u32| vec![(0, format!("{row}"))];
    let parsed = parse(&converter, &xlsx(&sheet_data(100, one_value), None).unwrap()).unwrap();
    let [Block::Table(table)] = parsed.document().blocks.as_slice() else {
        panic!("{:?}", parsed.document().blocks.len());
    };
    assert_eq!(table.grid.len(), 100);
    let wide = |_| (0..20u16).map(|col| (col, "1".to_string())).collect();
    assert_exceeds_xml_nodes("row", parse(&converter, &xlsx(&sheet_data(1, wide), None).unwrap()));
}

#[test]
fn structure_beside_the_rows_and_other_parts_keep_the_node_limit() {
    let converter = with_xml_nodes(40);
    let one_value = |_| vec![(0, "1".to_string())];
    let cols: String =
        (1..=40).map(|c| format!(r#"<col min="{c}" max="{c}" width="9"/>"#)).collect();
    let around = format!("<cols>{cols}</cols>{}", sheet_data(1, one_value));
    assert_exceeds_xml_nodes("cols", parse(&converter, &xlsx(&around, None).unwrap()));
    let strings = "<si><t>a</t></si>".repeat(20);
    let shared = xlsx(&sheet_data(1, one_value), Some(&strings)).unwrap();
    assert_exceeds_xml_nodes("shared strings", parse(&converter, &shared));
}

#[test]
fn content_in_opposite_corners_still_exceeds_the_grid_slots() {
    let converter = Converter::new(Config::default()).unwrap();
    let corners_xlsx = xlsx(&sheet_data(XLSB_ROWS, corners(XLSB_ROWS - 1, 16_383)), None).unwrap();
    assert_exceeds_grid_slots("xlsx", parse(&converter, &corners_xlsx));
    let corners_xlsb = xlsb(XLSB_ROWS, corners(XLSB_ROWS - 1, 16_383)).unwrap();
    assert_exceeds_grid_slots("xlsb", parse(&converter, &corners_xlsb));
    let corners_xls = xls(XLS_ROWS, corners(XLS_ROWS - 1, 255)).unwrap();
    assert_exceeds_grid_slots("xls", parse(&converter, &corners_xls));
}

fn xlsb_rec(out: &mut Vec<u8>, id: u16, payload: &[u8]) -> Built<()> {
    if id < 0x80 {
        out.push(u8::try_from(id)?);
    } else {
        out.extend_from_slice(&[u8::try_from(id & 0x7F)? | 0x80, u8::try_from(id >> 7)?]);
    }
    let mut size = payload.len();
    while size >= 0x80 {
        out.push(u8::try_from(size & 0x7F)? | 0x80);
        size >>= 7;
    }
    out.push(u8::try_from(size)?);
    out.extend_from_slice(payload);
    Ok(())
}

fn xlsb_str(out: &mut Vec<u8>, s: &str) -> Built<()> {
    let units: Vec<u16> = s.encode_utf16().collect();
    out.extend_from_slice(&u32::try_from(units.len())?.to_le_bytes());
    units.iter().for_each(|u| out.extend_from_slice(&u.to_le_bytes()));
    Ok(())
}

fn xlsb(rows: u32, cells: impl Fn(u32) -> Vec<(u16, String)>) -> Built<Vec<u8>> {
    let mut sheet = Vec::new();
    for row in 0..rows {
        let mut header = row.to_le_bytes().to_vec();
        header.extend_from_slice(&[0; 13]);
        xlsb_rec(&mut sheet, 0, &header)?;
        for (col, text) in cells(row) {
            let mut cell = u32::from(col).to_le_bytes().to_vec();
            cell.extend_from_slice(&[0; 4]);
            xlsb_str(&mut cell, &text)?;
            xlsb_rec(&mut sheet, 6, &cell)?;
        }
    }
    let mut file_version = vec![0; 8];
    xlsb_str(&mut file_version, "")?;
    let mut bundle_sheet = vec![0, 0, 0, 0, 1, 0, 0, 0];
    xlsb_str(&mut bundle_sheet, "rId1")?;
    xlsb_str(&mut bundle_sheet, "Filled")?;
    let mut workbook = Vec::new();
    xlsb_rec(&mut workbook, 153, &file_version)?;
    xlsb_rec(&mut workbook, 156, &bundle_sheet)?;
    Ok(zip_package(&[
        ("_rels/.rels", rels(&[("officeDocument", "xl/workbook.bin")])),
        ("xl/workbook.bin", workbook),
        ("xl/_rels/workbook.bin.rels", rels(&[("worksheet", "worksheets/sheet1.bin")])),
        ("xl/worksheets/sheet1.bin", sheet),
    ]))
}

fn rels(targets: &[(&str, &str)]) -> Vec<u8> {
    let rels: String = targets
        .iter()
        .enumerate()
        .map(|(i, (kind, target))| {
            format!(
                r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}" Target="{target}"/>"#,
                i + 1
            )
        })
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
    )
    .into_bytes()
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
        out.push_str(&format!("\n<row r=\"{r}\">"));
        for (col, text) in cells(row) {
            let at = format!("{}{r}", column(col));
            if text.parse::<f64>().is_ok() {
                out.push_str(&format!(r#"<c r="{at}"><v>{text}</v></c>"#));
            } else {
                out.push_str(&format!(
                    r#"<c r="{at}" t="inlineStr"><is><t xml:space="preserve">{text}</t></is></c>"#
                ));
            }
        }
        out.push_str("</row>");
    }
    out.push_str("\n</sheetData>");
    out
}

/// A one-sheet xlsx whose worksheet holds `body`, with a shared string table
/// of `shared` items where given.
fn xlsx(body: &str, shared: Option<&str>) -> Built<Vec<u8>> {
    const SML: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    let workbook = format!(
        r#"<?xml version="1.0"?><workbook xmlns="{SML}" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Filled" sheetId="1" r:id="rId1"/></sheets></workbook>"#
    );
    let worksheet = format!(r#"<?xml version="1.0"?><worksheet xmlns="{SML}">{body}</worksheet>"#);
    let mut workbook_rels = vec![("worksheet", "worksheets/sheet1.xml")];
    let mut parts = vec![
        ("_rels/.rels", rels(&[("officeDocument", "xl/workbook.xml")])),
        ("xl/workbook.xml", workbook.into_bytes()),
        ("xl/worksheets/sheet1.xml", worksheet.into_bytes()),
    ];
    if let Some(items) = shared {
        workbook_rels.push(("sharedStrings", "sharedStrings.xml"));
        let sst = format!(r#"<?xml version="1.0"?><sst xmlns="{SML}">{items}</sst>"#);
        parts.push(("xl/sharedStrings.xml", sst.into_bytes()));
    }
    parts.push(("xl/_rels/workbook.xml.rels", rels(&workbook_rels)));
    Ok(zip_package(&parts))
}

fn biff(out: &mut Vec<u8>, kind: u16, body: &[u8]) -> Built<()> {
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&u16::try_from(body.len())?.to_le_bytes());
    out.extend_from_slice(body);
    Ok(())
}

fn biff_bof(out: &mut Vec<u8>, substream: u16) -> Built<()> {
    let mut body = [0; 16];
    body[..2].copy_from_slice(&0x0600u16.to_le_bytes());
    body[2..4].copy_from_slice(&substream.to_le_bytes());
    biff(out, 0x0809, &body)
}

fn xls(rows: u32, cells: impl Fn(u32) -> Vec<(u16, String)>) -> Built<Vec<u8>> {
    let mut sheet = Vec::new();
    biff_bof(&mut sheet, 0x0010)?;
    for row in 0..rows {
        for (col, text) in cells(row) {
            let mut label = Vec::new();
            for field in [u16::try_from(row)?, col, 0, u16::try_from(text.len())?] {
                label.extend_from_slice(&field.to_le_bytes());
            }
            label.push(0);
            label.extend_from_slice(text.as_bytes());
            biff(&mut sheet, 0x0204, &label)?;
        }
    }
    biff(&mut sheet, 0x000A, &[])?;
    let name = b"Filled";
    let mut globals = Vec::new();
    biff_bof(&mut globals, 0x0005)?;
    biff(&mut globals, 0x00E0, &[0; 20])?;
    let sheet_at = globals.len() + 4 + 8 + name.len() + 4;
    let mut boundsheet = u32::try_from(sheet_at)?.to_le_bytes().to_vec();
    boundsheet.extend_from_slice(&[0, 0, u8::try_from(name.len())?, 0]);
    boundsheet.extend_from_slice(name);
    biff(&mut globals, 0x0085, &boundsheet)?;
    biff(&mut globals, 0x000A, &[])?;
    globals.extend_from_slice(&sheet);
    let mut cfb = cfb::CompoundFile::create(Cursor::new(Vec::new()))?;
    cfb.create_stream("Workbook")?.write_all(&globals)?;
    cfb.flush()?;
    Ok(cfb.into_inner().into_inner())
}
