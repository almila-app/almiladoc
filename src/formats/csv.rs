//! CSV and friends (semicolon, tab, pipe delimited).
//!
//! Field content is preserved as written (RFC 4180: spaces are part of the
//! field); only control characters are cleaned. Encoding is detected from
//! the BOM (UTF-8, UTF-16LE/BE), then UTF-8, then Windows-1252. The
//! delimiter is chosen by trial-parsing candidates and scoring record
//! consistency, so delimiters inside quoted fields don't skew the choice. The
//! format marks no header row, so the shape of the data decides whether the
//! first record is one.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Block, Cell, Document, Inline, Table, TableKind};
use crate::shared::header::resolve_header_rows;
use crate::shared::text::clean_text;
use csv::{ReaderBuilder, StringRecord};
use std::borrow::Cow;

pub fn parse(parse_ctx: &ParseCtx, bytes: &[u8]) -> Result<Document, ConvertError> {
    let (text, loss) = decode(bytes);
    if let Some(code) = loss {
        parse_ctx.emit(code, Region::Body);
    }
    let delimiter = sniff_delimiter(&text);

    let mut reader = ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delimiter)
        .from_reader(text.as_bytes());

    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut record = StringRecord::new();
    loop {
        match reader.read_record(&mut record) {
            Ok(true) => {}
            Ok(false) => break,
            // Unreachable while the input is a `str` read flexibly: no
            // I/O, UTF-8 or record-length error can arise.
            Err(_) => {
                parse_ctx.emit(DiagnosticCode::CellValueUnreadable, Region::Body);
                continue;
            }
        }
        parse_ctx.charge(record.len() as u64)?;
        let cells: Vec<Cell> =
            record.iter().map(|f| Cell::from_inlines(vec![Inline::plain(clean_text(f))])).collect();
        rows.push(cells);
    }

    let mut doc = Document::default();
    let mut table = Table::from_rows(rows, 0, TableKind::Data);
    table.header_rows = resolve_header_rows(&table, 0);
    if !table.grid.is_empty() {
        doc.blocks.push(Block::Table(table));
    }
    Ok(doc)
}

/// The text and what decoding it cost: `invalid_encoding` when a UTF-16
/// file holds bytes that do not decode as UTF-16, `encoding_assumed` when
/// bytes that are not UTF-8 are read as windows-1252.
fn decode(bytes: &[u8]) -> (Cow<'_, str>, Option<DiagnosticCode>) {
    let utf16 = match bytes {
        [0xFF, 0xFE, ..] => Some(encoding_rs::UTF_16LE),
        [0xFE, 0xFF, ..] => Some(encoding_rs::UTF_16BE),
        _ => None,
    };
    if let Some(encoding) = utf16 {
        let (text, _, replaced) = encoding.decode(bytes);
        return (
            Cow::Owned(text.into_owned()),
            replaced.then_some(DiagnosticCode::InvalidEncoding),
        );
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => (Cow::Borrowed(s), None),
        Err(_) => {
            // windows-1252 maps every byte, so the guess replaces nothing it
            // can detect.
            let text = encoding_rs::WINDOWS_1252.decode_without_bom_handling(bytes).0;
            (Cow::Owned(text.into_owned()), Some(DiagnosticCode::EncodingAssumed))
        }
    }
}

/// Trial-parse each candidate over the leading records and score by field
/// consistency; comma wins ties.
fn sniff_delimiter(text: &str) -> u8 {
    const CANDIDATES: [u8; 4] = *b",;\t|";
    let mut best = (b',', 0u32);
    for d in CANDIDATES {
        let mut reader = ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .delimiter(d)
            .from_reader(text.as_bytes());
        // Sample complete records (the reader is streaming, so this reads
        // only as much input as the records span): sampling physical lines
        // would cut a quoted multiline field in half.
        let counts: Vec<usize> =
            reader.records().map_while(Result::ok).take(20).map(|r| r.len()).collect();
        if counts.is_empty() {
            continue;
        }
        // Modal field count and how dominant it is.
        let mut tally: std::collections::HashMap<usize, u32> = std::collections::HashMap::new();
        for &c in &counts {
            *tally.entry(c).or_default() += 1;
        }
        // Frequency ties break toward the wider record shape so the choice
        // never depends on hash-map iteration order.
        let (&modal, &freq) = tally.iter().max_by_key(|&(&count, &f)| (f, count)).unwrap();
        if modal < 2 {
            continue; // a delimiter that never splits carries no signal
        }
        // Consistency first, then wider records break the tie.
        let score = freq * 1000 + modal.min(500) as u32;
        if score > best.1 {
            best = (d, score);
        }
    }
    best.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::LossClass;

    #[test]
    fn quoted_fields_keep_padding() {
        let doc = parse(&ParseCtx::default(), b"a,b\n\"  padded  \",x\n").unwrap();
        let Block::Table(t) = &doc.blocks[0] else { panic!() };
        let crate::model::CellSlot::Origin(cell) = &t.grid[1][0] else { panic!() };
        let Block::Paragraph(inlines) = &cell.blocks[0] else { panic!() };
        let crate::model::Inline::Text { text, .. } = &inlines[0] else { panic!() };
        assert_eq!(text, "  padded  ");
    }

    #[test]
    fn quoted_delimiters_do_not_skew_sniffing() {
        // Semicolon-delimited with commas inside quoted fields.
        assert_eq!(sniff_delimiter("a;b;c\n\"1,5\";\"2,5\";x\n\"3,0\";y;z\n"), b';');
        assert_eq!(sniff_delimiter("a,b,c\n1,2,3\n"), b',');
        assert_eq!(sniff_delimiter("a\tb\n1\t2\n"), b'\t');
    }

    #[test]
    fn multiline_quoted_field_does_not_break_sniffing() {
        // A quoted field spanning more physical lines than the old 20-line
        // sample window; the record-based sample must still see semicolons.
        let long_field = format!("\"{}\"", vec!["line"; 30].join("\n"));
        let text = format!("a;b;c\n{long_field};2;3\nx;y;z\n");
        assert_eq!(sniff_delimiter(&text), b';');
    }

    #[test]
    fn utf16_bom_decodes() {
        let mut bytes = vec![0xFF, 0xFE];
        for u in "x,y\ncafé,90\n".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let doc = parse(&ParseCtx::default(), &bytes).unwrap();
        assert!(!doc.blocks.is_empty());
    }

    #[test]
    fn every_field_is_charged_once() {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, b"a,b\n1,2,3\n").unwrap();
        assert_eq!(parse_ctx.fuel(), 5);
    }

    proptest::proptest! {
        #[test]
        fn arbitrary_bytes_never_hold_more_cells_than_the_fields_charged(
            bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512),
        ) {
            let parse_ctx = ParseCtx::default();
            if let Ok(doc) = parse(&parse_ctx, &bytes) {
                let cells: usize = doc
                    .blocks
                    .iter()
                    .map(|block| match block {
                        Block::Table(table) => table.grid.iter().map(Vec::len).sum(),
                        _ => 0,
                    })
                    .sum();
                proptest::prop_assert!(cells as u64 <= parse_ctx.fuel());
            }
        }
    }

    #[test]
    fn a_decoding_loss_reports_once_per_file() {
        let utf16 = |bom: [u8; 2], unit: fn(u16) -> [u8; 2]| {
            let units =
                "a,b\n".encode_utf16().chain([0xD800, 0xDC01, 0xDC00, u16::from(b','), 0xD800]);
            bom.into_iter().chain(units.flat_map(unit)).collect::<Vec<u8>>()
        };
        for (bytes, code) in [
            (utf16([0xFF, 0xFE], u16::to_le_bytes), Some(DiagnosticCode::InvalidEncoding)),
            (utf16([0xFE, 0xFF], u16::to_be_bytes), Some(DiagnosticCode::InvalidEncoding)),
            (b"\xfeaf,\xe7ay\n".to_vec(), Some(DiagnosticCode::EncodingAssumed)),
            ("şef,çay\n".as_bytes().to_vec(), None),
        ] {
            let parse_ctx = ParseCtx::default();
            parse(&parse_ctx, &bytes).unwrap();
            let reported: Vec<_> = parse_ctx
                .into_diagnostics()
                .iter()
                .map(|d| (d.code, d.class, d.region, d.unit, d.count))
                .collect();
            let expected: Vec<_> =
                code.map(|c| (c, LossClass::Lost, Region::Body, None, 1)).into_iter().collect();
            assert_eq!(reported, expected, "{bytes:?}");
        }
    }
}
