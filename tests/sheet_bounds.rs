//! A sheet's grid spans its content-bearing cells at default `Limits`: a
//! whitespace-only cell widens nothing, whether it sits alone at the grid's
//! last row or fills a blank column down to it, while content in opposite
//! corners still exceeds `max_grid_slots`.

use almiladoc::model::Block;
use almiladoc::{Config, ConvertError, Converter, Format, Input, Limit, Parsed};
use std::error::Error;
use std::io::{Cursor, Write};
use std::path::Path;

const XLSB_ROWS: u32 = 1 << 20;
const XLS_ROWS: u32 = 1 << 16;

type Built<T> = Result<T, Box<dyn Error>>;

fn parse(converter: &Converter, bytes: &[u8]) -> Result<Parsed, ConvertError> {
    converter.parse(Input::new(bytes, Format::Excel))
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
    assert!(
        matches!(err, Some(ConvertError::ResourceLimit { limit: Limit::GridSlots, .. })),
        "{name}: {err:?}"
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
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(&name);
        let parsed = parse(&converter, &std::fs::read(&path).unwrap());
        assert_content_table(&name, &parsed.unwrap_or_else(|e| panic!("{name}: {e:?}")));
    }
}

#[test]
fn a_blank_column_filled_down_to_the_last_row_leaves_the_table_its_content() {
    let converter = Converter::new(Config::default()).unwrap();
    let parsed = parse(&converter, &xlsb(XLSB_ROWS, filled_down).unwrap());
    assert_content_table("xlsb", &parsed.unwrap_or_else(|e| panic!("xlsb: {e:?}")));
    let parsed = parse(&converter, &xls(XLS_ROWS, filled_down).unwrap());
    assert_content_table("xls", &parsed.unwrap_or_else(|e| panic!("xls: {e:?}")));
}

#[test]
fn content_in_opposite_corners_still_exceeds_the_grid_slots() {
    let converter = Converter::new(Config::default()).unwrap();
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
    let rels = |kind: &str, target: &str| {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}" Target="{target}"/></Relationships>"#
        )
        .into_bytes()
    };
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in [
        ("_rels/.rels", rels("officeDocument", "xl/workbook.bin")),
        ("xl/workbook.bin", workbook),
        ("xl/_rels/workbook.bin.rels", rels("worksheet", "worksheets/sheet1.bin")),
        ("xl/worksheets/sheet1.bin", sheet),
    ] {
        zip.start_file(name, options)?;
        zip.write_all(&data)?;
    }
    Ok(zip.finish()?.into_inner())
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
