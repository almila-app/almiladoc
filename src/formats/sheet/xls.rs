//! In-house legacy Excel reader (.xls): OLE2 compound file holding a BIFF
//! record stream ([MS-XLS]). BIFF8 is the target; BIFF5/BIFF7 streams
//! degrade to their byte-string record layouts instead of erroring. Cell
//! values resolve their number format through the same engine and shared
//! grid as the SpreadsheetML reader, so a workbook saved as .xls and as
//! .xlsx converts identically.

use super::controls::Checkbox;
use super::grid::{
    CellFormat, CellRange, CellRef, CellXfs, DateSystem, GridBounds, LinkTarget, SheetContent,
    SheetLink, build_table, format_as_text, render_number, resolve_format,
};
use super::{error_literal, rk_number};
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{Block, Document, Inline};
use crate::shared::binary::{decode_utf16, get_u16, get_u32, read_ole_stream, utf16le_units};
use crate::shared::officeart;
use crate::shared::text::clean_text;
use std::collections::HashMap;
use std::io::Cursor;
use std::rc::Rc;

// Record types ([MS-XLS] 2.3.2), BIFF5-BIFF8 numbering.
const BOF: u16 = 0x0809;
const EOF_REC: u16 = 0x000A;
const FILEPASS: u16 = 0x002F;
const CODEPAGE: u16 = 0x0042;
const DATEMODE: u16 = 0x0022;
const BOUNDSHEET: u16 = 0x0085;
const SST: u16 = 0x00FC;
const CONTINUE: u16 = 0x003C;
const FORMAT: u16 = 0x041E;
const XF: u16 = 0x00E0;
const ROW: u16 = 0x0208;
const COLINFO: u16 = 0x007D;
const MERGEDCELLS: u16 = 0x00E5;
const LABELSST: u16 = 0x00FD;
const LABEL: u16 = 0x0204;
const RSTRING: u16 = 0x00D6;
const NUMBER: u16 = 0x0203;
const RK: u16 = 0x027E;
const MULRK: u16 = 0x00BD;
const BOOLERR: u16 = 0x0205;
const FORMULA: u16 = 0x0006;
const STRING: u16 = 0x0207;
const MSODRAWING: u16 = 0x00EC;
const OBJ: u16 = 0x005D;
const TXO: u16 = 0x01B6;
const NOTE: u16 = 0x001C;
const HEADER: u16 = 0x0014;
const FOOTER: u16 = 0x0015;
const HEADER_FOOTER: u16 = 0x089C;
const HLINK: u16 = 0x01B8;

/// BOF `dt` value for a worksheet (or dialog sheet) substream.
const WORKSHEET_SUBSTREAM: u16 = 0x0010;

/// The BIFF8 grid is 256 columns; a larger column index is not a real cell.
const BIFF8_COLS: u32 = 256;

/// [MS-XLS] Rw: a BIFF8 row index is two bytes, so a sheet has 65,536 rows.
pub(super) const BIFF8_GRID: GridBounds = GridBounds { rows: 65_536, cols: BIFF8_COLS };

pub(super) fn parse(parse_ctx: &ParseCtx, bytes: &[u8]) -> Result<Document, ConvertError> {
    let mut ole = cfb::CompoundFile::open(Cursor::new(bytes))
        .map_err(|_| ConvertError::malformed("not an OLE2 compound file"))?;
    let data = workbook_stream(parse_ctx, &mut ole)?;
    let mut records = RecordCount::new(parse_ctx);
    let globals = read_globals(&data, &mut records)?;

    let shown = globals.sheets.iter().filter(|s| matches!(s, Listed::Shown(_))).count();
    let mut doc = Document::default();
    let mut failed = 0usize;
    // One budget for the workbook, so sheets cannot multiply the cap.
    let mut slots = 0u64;
    for (position, listed) in globals.sheets.iter().enumerate() {
        let _unit = parse_ctx.enter_unit(position);
        let sheet = match listed {
            Listed::Shown(sheet) => sheet,
            Listed::Hidden => {
                parse_ctx.emit(DiagnosticCode::HiddenOmitted, Region::Body);
                continue;
            }
            Listed::Unreadable => {
                parse_ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body);
                continue;
            }
        };
        let Some(content) = read_sheet(&data, &globals, sheet.offset, &mut records)? else {
            parse_ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body);
            failed += 1;
            continue;
        };
        let Some(table) = build_table(parse_ctx, content, &mut slots)? else {
            continue;
        };
        if shown > 1 {
            if sheet.name_replaced {
                parse_ctx.emit(DiagnosticCode::InvalidEncoding, Region::Body);
            }
            doc.blocks.push(Block::heading(2, vec![Inline::plain(sheet.name.clone())]));
        }
        doc.blocks.push(Block::Table(table));
    }
    if shown > 0 && failed == shown {
        return Err(ConvertError::malformed("no sheet in the workbook could be read"));
    }
    Ok(doc)
}

/// The BIFF stream: `Workbook` (BIFF8) or `Book` (older BIFF), matched
/// case-insensitively like detection because producers vary.
fn workbook_stream<R: std::io::Read + std::io::Seek>(
    parse_ctx: &ParseCtx,
    ole: &mut cfb::CompoundFile<R>,
) -> Result<Vec<u8>, ConvertError> {
    let name = ole
        .read_root_storage()
        .find(|e| {
            e.is_stream()
                && (e.name().eq_ignore_ascii_case("Workbook")
                    || e.name().eq_ignore_ascii_case("Book"))
        })
        .map(|e| e.name().to_string())
        .ok_or_else(|| ConvertError::missing_part("Workbook"))?;
    read_ole_stream(parse_ctx, ole, &name)
}

/// The record at `pos`: type, payload, and the position after it. `None` on
/// a truncated header or payload.
fn record_at(data: &[u8], pos: usize) -> Option<(u16, &[u8], usize)> {
    let rec_type = get_u16(data, pos)?;
    let len = get_u16(data, pos.checked_add(2)?)? as usize;
    let body_at = pos.checked_add(4)?;
    let body = data.get(body_at..body_at.checked_add(len)?)?;
    Some((rec_type, body, body_at + len))
}

/// A record: type, payload, position after it.
type Record<'a> = (u16, &'a [u8], usize);

/// Records read from the workbook stream, against `max_records`, and the
/// context they report to.
struct RecordCount<'c> {
    parse_ctx: &'c ParseCtx<'c>,
    seen: u64,
    max: u64,
    /// Where the stream was found cut, so the cut reports once.
    cut_at: Option<usize>,
}

impl<'c> RecordCount<'c> {
    fn new(parse_ctx: &'c ParseCtx<'c>) -> Self {
        RecordCount { parse_ctx, seen: 0, max: parse_ctx.limits().max_records, cut_at: None }
    }
}

/// `record_at` charging the stream-wide record budget.
fn next_record<'a>(
    data: &'a [u8],
    pos: usize,
    records: &mut RecordCount,
) -> Result<Option<Record<'a>>, ConvertError> {
    let Some(rec) = record_at(data, pos) else {
        // Trailing bytes too short to form a record: the stream was cut, so
        // whatever was read is partial rather than complete.
        if pos < data.len() && records.cut_at.replace(pos) != Some(pos) {
            records.parse_ctx.emit(DiagnosticCode::TruncatedRecords, Region::Body);
        }
        return Ok(None);
    };
    records.seen += 1;
    if records.seen > records.max {
        return Err(ConvertError::limit(Limit::Records, records.max));
    }
    Ok(Some(rec))
}

/// The record body at `pos - body.len() - 4` plus the bodies of the
/// CONTINUE records that immediately follow it, and the position after them.
fn continued<'a>(
    data: &'a [u8],
    body: &'a [u8],
    mut pos: usize,
    records: &mut RecordCount,
) -> Result<(Vec<&'a [u8]>, usize), ConvertError> {
    let mut segs = vec![body];
    while let Some((rec_type, cont, next)) = next_record(data, pos, records)? {
        if rec_type != CONTINUE {
            records.seen -= 1;
            break;
        }
        segs.push(cont);
        pos = next;
    }
    Ok((segs, pos))
}

/// Reader over a record's segments (base body plus CONTINUE bodies). Fixed
/// fields never straddle a segment boundary, but may start exactly on one;
/// only string character data crosses boundaries, and the string readers
/// handle the repeated option-flags byte themselves.
struct SegReader<'a> {
    segs: Vec<&'a [u8]>,
    seg: usize,
    off: usize,
}

impl<'a> SegReader<'a> {
    fn new(segs: Vec<&'a [u8]>) -> SegReader<'a> {
        SegReader { segs, seg: 0, off: 0 }
    }

    /// Bytes left in the current segment.
    fn in_seg(&self) -> usize {
        self.segs.get(self.seg).map_or(0, |s| s.len() - self.off)
    }

    /// Hop over exhausted segments so a field can start at a boundary.
    fn normalize(&mut self) {
        while self.seg < self.segs.len() && self.in_seg() == 0 {
            self.seg += 1;
            self.off = 0;
        }
    }

    /// Move to the start of the next segment; `None` when there is none.
    fn next_seg(&mut self) -> Option<()> {
        if self.seg + 1 < self.segs.len() {
            self.seg += 1;
            self.off = 0;
            Some(())
        } else {
            None
        }
    }

    /// `n` bytes from within one segment.
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        self.normalize();
        let seg = self.segs.get(self.seg)?;
        let out = seg.get(self.off..self.off.checked_add(n)?)?;
        self.off += n;
        Some(out)
    }

    fn u8(&mut self) -> Option<u8> {
        self.bytes(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.bytes(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        self.bytes(4).and_then(|b| Some(u32::from_le_bytes(b.try_into().ok()?)))
    }

    /// Skip `n` bytes across segment boundaries (non-character data carries
    /// no repeated flags byte).
    fn skip(&mut self, mut n: usize) -> Option<()> {
        while n > 0 {
            self.normalize();
            let step = self.in_seg().min(n);
            if step == 0 {
                return None;
            }
            self.off += step;
            n -= step;
        }
        Some(())
    }
}

/// A BIFF8 Unicode string: XLUnicodeString, ShortXLUnicodeString (`short`),
/// or XLUnicodeRichExtendedString (`rich`). Character data may continue
/// into following segments; at every such boundary the option-flags byte is
/// repeated and the encoding can switch between 8-bit compressed and 16-bit
/// UTF-16, so it is re-read rather than carried over. Rich runs and
/// phonetic data are skipped, matching the xlsx reader's handling of `rPh`.
/// The text comes with whether a unit did not decode.
fn read_biff8_string(r: &mut SegReader, short: bool, rich: bool) -> Option<(String, bool)> {
    let cch = if short { r.u8()? as usize } else { r.u16()? as usize };
    let flags = r.u8()?;
    let mut wide = flags & 0x01 != 0;
    let runs = if rich && flags & 0x08 != 0 { r.u16()? as usize } else { 0 };
    let ext = if rich && flags & 0x04 != 0 { r.u32()? as usize } else { 0 };
    let mut units: Vec<u16> = Vec::new();
    let mut remaining = cch;
    while remaining > 0 {
        if r.in_seg() == 0 {
            r.next_seg()?;
            wide = r.u8()? & 0x01 != 0;
        }
        let unit = if wide { 2 } else { 1 };
        let take = (r.in_seg() / unit).min(remaining);
        if take == 0 {
            // A dangling half character: outside the format.
            return None;
        }
        let bytes = r.bytes(take * unit)?;
        if wide {
            units.extend(utf16le_units(bytes));
        } else {
            units.extend(bytes.iter().map(|&b| u16::from(b)));
        }
        remaining -= take;
    }
    // A truncated trailer loses only the strings after this one; the reader
    // then runs dry and the caller stops.
    let _ = r.skip(runs * 4).and_then(|()| r.skip(ext));
    Some(decode_utf16(units))
}

/// A BIFF5/BIFF7 byte string (no flags byte), decoded per the workbook's
/// CODEPAGE record, with whether a byte did not decode.
fn read_byte_string(
    r: &mut SegReader,
    short: bool,
    encoding: &'static encoding_rs::Encoding,
) -> Option<(String, bool)> {
    let cch = if short { r.u8()? as usize } else { r.u16()? as usize };
    let mut bytes = Vec::with_capacity(cch);
    let mut remaining = cch;
    while remaining > 0 {
        r.normalize();
        let take = r.in_seg().min(remaining);
        if take == 0 {
            return None;
        }
        bytes.extend_from_slice(r.bytes(take)?);
        remaining -= take;
    }
    let (text, replaced) = encoding.decode_without_bom_handling(&bytes);
    Some((text.into_owned(), replaced))
}

/// ANSI code page from the CODEPAGE record, for BIFF5 byte strings (BIFF8
/// strings carry their own encoding flag).
fn codepage_encoding(cp: u16) -> &'static encoding_rs::Encoding {
    use encoding_rs::*;
    match cp {
        874 => WINDOWS_874,
        932 => SHIFT_JIS,
        936 => GBK,
        949 => EUC_KR,
        950 => BIG5,
        1250 => WINDOWS_1250,
        1251 => WINDOWS_1251,
        1253 => WINDOWS_1253,
        1254 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        1257 => WINDOWS_1257,
        1258 => WINDOWS_1258,
        _ => WINDOWS_1252,
    }
}

/// A sheet the workbook's directory lists, in source order.
enum Listed {
    Shown(BoundSheet),
    Hidden,
    /// A record too short for its offset, state or name, or whose name does
    /// not read.
    Unreadable,
}

struct BoundSheet {
    name: String,
    /// The name needed a replacement character.
    name_replaced: bool,
    /// Absolute stream offset of the sheet substream's BOF record.
    offset: usize,
}

struct Globals {
    biff8: bool,
    dates: DateSystem,
    encoding: &'static encoding_rs::Encoding,
    /// Each shared string with whether it needed a replacement character.
    sst: Vec<(String, bool)>,
    /// The XF table in record order; a cell's ixfe indexes it directly.
    xfs: CellXfs,
    sheets: Vec<Listed>,
}

impl Globals {
    fn format(&self, ixfe: u16) -> CellFormat {
        self.xfs.get(usize::from(ixfe))
    }

    fn read_string(&self, r: &mut SegReader, short: bool) -> Option<(String, bool)> {
        if self.biff8 {
            read_biff8_string(r, short, false)
        } else {
            read_byte_string(r, short, self.encoding)
        }
    }
}

/// The workbook globals substream: SST, FORMAT/XF tables, sheet directory,
/// date system, encryption marker. A workbook listing no sheet is
/// `Malformed`; globals that end without their EOF were cut short.
fn read_globals(data: &[u8], records: &mut RecordCount) -> Result<Globals, ConvertError> {
    let Some((rec_type, body, mut pos)) = record_at(data, 0) else {
        return Err(ConvertError::malformed("empty workbook stream"));
    };
    if rec_type != BOF {
        return Err(ConvertError::malformed("workbook stream does not start with a BOF record"));
    }
    let mut globals = Globals {
        biff8: get_u16(body, 0) == Some(0x0600),
        dates: DateSystem::Since1900,
        encoding: encoding_rs::WINDOWS_1252,
        sst: Vec::new(),
        xfs: CellXfs::new(Vec::new()),
        sheets: Vec::new(),
    };
    let mut formats: HashMap<u32, (String, bool)> = HashMap::new();
    let mut xf_ifmts: Vec<u16> = Vec::new();
    let mut depth = 1usize;
    let mut ended = false;
    while let Some((rec_type, body, next)) = next_record(data, pos, records)? {
        pos = next;
        match rec_type {
            BOF => depth += 1,
            EOF_REC => {
                if depth == 1 {
                    ended = true;
                    break;
                }
                depth -= 1;
            }
            _ if depth > 1 => {}
            FILEPASS => return Err(ConvertError::Encrypted),
            CODEPAGE => {
                if let Some(cp) = get_u16(body, 0) {
                    globals.encoding = codepage_encoding(cp);
                }
            }
            DATEMODE => globals.dates = DateSystem::from_date1904(get_u16(body, 0) == Some(1)),
            BOUNDSHEET => {
                if let Some(sheet) = read_boundsheet(body, &globals) {
                    globals.sheets.push(sheet);
                }
            }
            FORMAT => {
                let Some(ifmt) = get_u16(body, 0) else {
                    continue;
                };
                let mut r = SegReader::new(vec![&body[2..]]);
                // BIFF5 FORMAT carries a one-byte length, BIFF8 a two-byte.
                if let Some(code) = globals.read_string(&mut r, !globals.biff8) {
                    formats.insert(u32::from(ifmt), code);
                }
            }
            XF => {
                // ixfe is 16-bit, so the table never usefully exceeds it.
                if xf_ifmts.len() <= usize::from(u16::MAX)
                    && let Some(ifmt) = get_u16(body, 2)
                {
                    xf_ifmts.push(ifmt);
                }
            }
            SST if globals.biff8 => {
                let (segs, after) = continued(data, body, pos, records)?;
                globals.sst = read_sst(&segs);
                pos = after;
            }
            _ => {}
        }
    }
    if globals.sheets.is_empty() {
        return Err(ConvertError::malformed("the workbook lists no sheet"));
    }
    if !ended && records.cut_at.is_none() {
        records.parse_ctx.emit(DiagnosticCode::TruncatedRecords, Region::Body);
    }
    // Resolved after the pass: FORMAT records are not ordered relative to
    // the XFs that reference them.
    let custom: HashMap<u32, &str> =
        formats.iter().map(|(&id, (code, _))| (id, code.as_str())).collect();
    let mut cache: HashMap<u16, CellFormat> = HashMap::new();
    globals.xfs = CellXfs::new(
        xf_ifmts
            .iter()
            .map(|&ifmt| {
                let id = u32::from(ifmt);
                let format = cache.entry(ifmt).or_insert_with(|| resolve_format(id, &custom));
                (format.clone(), formats.get(&id).is_some_and(|&(_, replaced)| replaced))
            })
            .collect(),
    );
    Ok(globals)
}

/// BOUNDSHEET: substream offset, hidden state, sheet type, name. Chart and
/// macro sheets stay listed (their substreams fail the worksheet check and
/// count as unreadable, matching the xlsx reader's treatment of their
/// parts); VBA modules are not sheets in any container and are dropped
/// (`None`). A hidden sheet is hidden whatever else its record holds.
fn read_boundsheet(body: &[u8], globals: &Globals) -> Option<Listed> {
    if body.get(5) == Some(&0x06) {
        return None;
    }
    let read = || {
        if body.get(4)? & 0x03 != 0 {
            return Some(Listed::Hidden);
        }
        let offset = get_u32(body, 0)? as usize;
        let mut r = SegReader::new(vec![body.get(6..)?]);
        let (name, name_replaced) = globals.read_string(&mut r, true)?;
        Some(Listed::Shown(BoundSheet { name: clean_text(&name), name_replaced, offset }))
    };
    Some(read().unwrap_or(Listed::Unreadable))
}

/// The shared string table (trap 1: strings routinely split across
/// CONTINUE records, mid-character-data). A malformed tail keeps the
/// entries read so far; LABELSST lookups past them stay empty and report
/// the cell as unreadable.
fn read_sst(segs: &[&[u8]]) -> Vec<(String, bool)> {
    let mut r = SegReader::new(segs.to_vec());
    let unique = match (r.u32(), r.u32()) {
        (Some(_total), Some(unique)) => unique as usize,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    while out.len() < unique {
        let Some((text, replaced)) = read_biff8_string(&mut r, false, true) else {
            break;
        };
        out.push((clean_text(&text), replaced));
    }
    out
}

/// A cell record's leading cell, which may lie past the grid, and ixfe.
fn cell_ref(body: &[u8]) -> Option<(CellRef, u16)> {
    let row = u32::from(get_u16(body, 0)?);
    let col = u32::from(get_u16(body, 2)?);
    Some((CellRef::new(row, col), get_u16(body, 4)?))
}

fn get_f64(body: &[u8], off: usize) -> Option<f64> {
    Some(f64::from_le_bytes(body.get(off..off.checked_add(8)?)?.try_into().ok()?))
}

/// One worksheet substream into the shared `SheetContent` shape. `Ok(None)`
/// means the substream is missing or not a worksheet (chart or macro
/// sheets), which the caller counts as unreadable.
fn read_sheet(
    data: &[u8],
    globals: &Globals,
    offset: usize,
    records: &mut RecordCount,
) -> Result<Option<SheetContent>, ConvertError> {
    let Some((rec_type, body, mut pos)) = record_at(data, offset) else {
        return Ok(None);
    };
    if rec_type != BOF || get_u16(body, 2) != Some(WORKSHEET_SUBSTREAM) {
        return Ok(None);
    }
    let mut out = SheetContent::on(BIFF8_GRID);
    let mut depth = 1usize;
    // A FORMULA whose cached value is a string: its cell and ixfe waiting
    // for the STRING record that carries the text.
    let mut pending: Option<(CellRef, u16)> = None;
    // A drawing object's shape arrives in MSODRAWING ahead of its OBJ, and
    // its text in the TXO after it.
    let mut shape: Option<Shape> = None;
    let mut text_of: Option<TextOf> = None;
    let parse_ctx = records.parse_ctx;
    while let Some((rec_type, body, next)) = next_record(data, pos, records)? {
        pos = next;
        match rec_type {
            BOF => depth += 1,
            EOF_REC => {
                if depth == 1 {
                    break;
                }
                depth -= 1;
            }
            _ if depth > 1 => {}
            ROW => {
                if body.get(12).is_some_and(|flags| flags & 0x20 != 0)
                    && let Some(row) = get_u16(body, 0)
                {
                    out.hide_row(u32::from(row));
                }
            }
            COLINFO => {
                if let (Some(first), Some(last), Some(flags)) =
                    (get_u16(body, 0), get_u16(body, 2), get_u16(body, 8))
                    && flags & 0x01 != 0
                    && first <= last
                {
                    out.hide_cols(u32::from(first), u32::from(last));
                }
            }
            MERGEDCELLS => {
                let count = usize::from(get_u16(body, 0).unwrap_or(0))
                    .min(body.len().saturating_sub(2) / 8);
                for i in 0..count {
                    let at = 2 + i * 8;
                    let (Some(r1), Some(r2), Some(c1), Some(c2)) = (
                        get_u16(body, at),
                        get_u16(body, at + 2),
                        get_u16(body, at + 4),
                        get_u16(body, at + 6),
                    ) else {
                        break;
                    };
                    let range = CellRange::spanning(
                        CellRef::new(u32::from(r1), u32::from(c1)),
                        CellRef::new(u32::from(r2), u32::from(c2)),
                    );
                    out.merge(parse_ctx, range);
                }
            }
            LABELSST => {
                if let Some((at, ixfe)) = cell_ref(body)
                    && let Some(isst) = get_u32(body, 6)
                {
                    let text = globals.sst.get(isst as usize).cloned();
                    put_string(&mut out, at, globals, ixfe, text);
                }
            }
            LABEL | RSTRING => {
                let (segs, after) = continued(data, body, pos, records)?;
                pos = after;
                if let Some((at, ixfe)) = cell_ref(body) {
                    // RSTRING's trailing rich runs are formatting, not text.
                    let text = string_reader(&segs, 6)
                        .and_then(|mut r| globals.read_string(&mut r, false));
                    put_string(&mut out, at, globals, ixfe, text);
                }
            }
            NUMBER => {
                if let Some((at, ixfe)) = cell_ref(body)
                    && let Some(n) = get_f64(body, 6)
                {
                    put_number(&mut out, at, globals, ixfe, n);
                }
            }
            RK => {
                if let Some((at, ixfe)) = cell_ref(body)
                    && let Some(rk) = get_u32(body, 6)
                {
                    let n = rk_number(rk);
                    put_number(&mut out, at, globals, ixfe, n);
                }
            }
            MULRK => {
                // rw, colFirst, then 6-byte (ixfe, RK) pairs; colLast is
                // redundant with the record length.
                if let (Some(row), Some(col_first)) = (get_u16(body, 0), get_u16(body, 2)) {
                    for i in 0..body.len().saturating_sub(6) / 6 {
                        let (Some(ixfe), Some(rk)) =
                            (get_u16(body, 4 + i * 6), get_u32(body, 6 + i * 6))
                        else {
                            break;
                        };
                        let col = u32::from(col_first) + i as u32;
                        let at = CellRef::new(u32::from(row), col);
                        put_number(&mut out, at, globals, ixfe, rk_number(rk));
                    }
                }
            }
            BOOLERR => {
                if let Some((at, _ixfe)) = cell_ref(body)
                    && let (Some(&value), Some(&is_err)) = (body.get(6), body.get(7))
                {
                    let text = match (is_err, value) {
                        (0, 0) => Some("FALSE"),
                        (0, _) => Some("TRUE"),
                        (1, code) => error_literal(code),
                        _ => None,
                    };
                    match text {
                        Some(text) => {
                            out.put(at, text.to_string());
                        }
                        None => out.lose(at, DiagnosticCode::CellValueUnreadable),
                    }
                }
            }
            FORMULA => {
                if let Some((at, ixfe)) = cell_ref(body)
                    && let Some(value) = body.get(6..14)
                {
                    if value[6] == 0xFF && value[7] == 0xFF {
                        match value[0] {
                            0x00 => pending = Some((at, ixfe)),
                            0x01 => {
                                let text = if value[2] == 0 { "FALSE" } else { "TRUE" };
                                out.put(at, text.to_string());
                            }
                            0x02 => match error_literal(value[2]) {
                                Some(text) => {
                                    out.put(at, text.to_string());
                                }
                                None => out.lose(at, DiagnosticCode::CellValueUnreadable),
                            },
                            // 0x03 is a blank string result.
                            _ => {}
                        }
                    } else if let Some(n) = get_f64(body, 6) {
                        put_number(&mut out, at, globals, ixfe, n);
                    }
                }
            }
            MSODRAWING if globals.biff8 => {
                let (segs, after) = continued(data, body, pos, records)?;
                pos = after;
                shape = if segs.len() == 1 { last_shape(body) } else { last_shape(&segs.concat()) };
                if !opens_text_box(body) {
                    text_of = None;
                }
            }
            OBJ if globals.biff8 => {
                text_of = None;
                let shape = shape.take();
                let hidden = shape.as_ref().is_some_and(|s| s.hidden);
                match obj_kind(body) {
                    Some(Obj::Checkbox(_) | Obj::Control) if hidden => {
                        parse_ctx.emit(DiagnosticCode::HiddenOmitted, Region::Body)
                    }
                    Some(Obj::Checkbox(Some(checked))) => match shape {
                        Some(Shape { anchor: at, .. }) if at.col < BIFF8_COLS => {
                            let caption = String::new();
                            out.checkboxes
                                .entry(at)
                                .or_default()
                                .push(Checkbox { checked, caption });
                            text_of = Some(TextOf::Checkbox(at));
                        }
                        _ => parse_ctx.emit(DiagnosticCode::FormControlUnread, Region::Body),
                    },
                    Some(Obj::Checkbox(None)) => match shape {
                        Some(Shape { anchor, .. }) if anchor.col < BIFF8_COLS => {
                            out.lose(anchor, DiagnosticCode::FormControlUnread)
                        }
                        _ => parse_ctx.emit(DiagnosticCode::FormControlUnread, Region::Body),
                    },
                    Some(Obj::Control) => {
                        parse_ctx.emit(DiagnosticCode::FormControlUnread, Region::Body)
                    }
                    Some(Obj::Picture) if !hidden => {
                        parse_ctx.emit(DiagnosticCode::ImageUnplaced, Region::Body)
                    }
                    Some(Obj::Chart) if !hidden => {
                        parse_ctx.emit(DiagnosticCode::GraphicUnread, Region::Body)
                    }
                    Some(Obj::Shape) if !hidden => text_of = Some(TextOf::Shape),
                    _ => {}
                }
            }
            TXO => {
                let (segs, after) = continued(data, body, pos, records)?;
                pos = after;
                match text_of.take() {
                    Some(TextOf::Checkbox(at)) => {
                        if let Some((caption, replaced)) = txo_text(&segs)
                            && let Some(b) = out.checkboxes.get_mut(&at).and_then(|v| v.last_mut())
                        {
                            b.caption = clean_text(&caption);
                            if replaced {
                                out.lose(at, DiagnosticCode::InvalidEncoding);
                            }
                        }
                    }
                    Some(TextOf::Shape)
                        if txo_text(&segs).is_none_or(|(text, _)| !text.trim().is_empty()) =>
                    {
                        parse_ctx.emit(DiagnosticCode::GraphicUnread, Region::Body);
                    }
                    _ => {}
                }
            }
            STRING => {
                let (segs, after) = continued(data, body, pos, records)?;
                pos = after;
                if let Some((at, ixfe)) = pending.take() {
                    let text = string_reader(&segs, 0)
                        .and_then(|mut r| globals.read_string(&mut r, false));
                    put_string(&mut out, at, globals, ixfe, text);
                }
            }
            NOTE => parse_ctx.emit(DiagnosticCode::CommentOmitted, Region::Comment),
            HEADER | FOOTER => {
                let chars = if globals.biff8 {
                    get_u16(body, 0)
                } else {
                    body.first().map(|&n| u16::from(n))
                };
                if chars.is_some_and(|n| n > 0) {
                    omit_header_footer(parse_ctx, rec_type == HEADER);
                }
            }
            HEADER_FOOTER => {
                // cchHeaderEven, cchFooterEven, cchHeaderFirst, cchFooterFirst
                for (at, header) in [(30, true), (32, false), (34, true), (36, false)] {
                    if get_u16(body, at).is_some_and(|n| n > 0) {
                        omit_header_footer(parse_ctx, header);
                    }
                }
            }
            HLINK => match hlink(body) {
                Some(Some(link)) => out.links.push(link),
                Some(None) => {}
                None => parse_ctx.emit(DiagnosticCode::LinkTargetUnread, Region::Body),
            },
            _ => {}
        }
    }
    Ok(Some(out))
}

/// The drawing object a TXO's text belongs to.
enum TextOf {
    /// A checkbox's caption, by the cell it is anchored in.
    Checkbox(CellRef),
    /// A shown shape's text, which the grid does not hold.
    Shape,
}

/// Whether an MSODRAWING body opens with the client text box that carries
/// the previous OBJ's text into its TXO ([MS-XLS] TxO), rather than a new
/// shape.
fn opens_text_box(body: &[u8]) -> bool {
    const CLIENT_TEXTBOX: u16 = 0xF00D;
    officeart::record_at(body, 0).is_some_and(|(_, rec_type, _)| rec_type == CLIENT_TEXTBOX)
}

/// What an OBJ needs from its shape: the cell the client anchor starts in,
/// and whether the shape is hidden.
struct Shape {
    anchor: CellRef,
    hidden: bool,
}

/// The last shape in an MSODRAWING body. Containers are entered rather
/// than skipped, so the scan stays linear whatever the nesting.
fn last_shape(data: &[u8]) -> Option<Shape> {
    const SP_CONTAINER: u16 = 0xF004;
    const OPT: u16 = 0xF00B;
    const CLIENT_ANCHOR: u16 = 0xF010;
    // Group shape boolean properties: fHidden with its use bit.
    const PID_GROUP_SHAPE: u16 = 0x03BF;
    const F_HIDDEN: u32 = 0x0000_0002;
    const F_USE_HIDDEN: u32 = 0x0002_0000;
    let mut anchor = None;
    let mut hidden = false;
    let mut off = 0usize;
    while let Some((ver_inst, rec_type, body)) = officeart::record_at(data, off) {
        if ver_inst & 0x000F == 0x000F {
            if rec_type == SP_CONTAINER {
                anchor = None;
                hidden = false;
            }
            off += 8;
            continue;
        }
        match rec_type {
            CLIENT_ANCHOR => {
                if let (Some(col), Some(row)) = (get_u16(body, 2), get_u16(body, 6)) {
                    anchor = Some(CellRef::new(u32::from(row), u32::from(col)));
                }
            }
            OPT => {
                for i in 0..usize::from(ver_inst >> 4) {
                    let (Some(pid), Some(op)) = (get_u16(body, i * 6), get_u32(body, i * 6 + 2))
                    else {
                        break;
                    };
                    if pid & 0x3FFF == PID_GROUP_SHAPE && op & F_USE_HIDDEN != 0 {
                        hidden = op & F_HIDDEN != 0;
                    }
                }
            }
            _ => {}
        }
        off += 8 + body.len();
    }
    anchor.map(|anchor| Shape { anchor, hidden })
}

/// What an OBJ record draws, by its FtCmo object type.
enum Obj {
    /// A checkbox and its state, `None` for the mixed state, which has no
    /// token.
    Checkbox(Option<bool>),
    /// Another form control: a button, option button, edit box, label,
    /// spinner, scroll bar, list, group box or drop-down.
    Control,
    Picture,
    Chart,
    /// A text box or a drawn shape that can hold text: a rectangle, oval,
    /// polygon or OfficeArt shape.
    Shape,
}

/// The kind of an OBJ record's object; `None` for one this reader does not
/// report, a drop-down the application draws for its own interface
/// included. A subrecord running past the record ends the walk with what it
/// read: FtLbsData, always last, holds a lower bound there ([MS-XLS]
/// 2.5.149).
fn obj_kind(body: &[u8]) -> Option<Obj> {
    const FT_END: u16 = 0x0000;
    const FT_CMO: u16 = 0x0015;
    const FT_CBLS_DATA: u16 = 0x0012;
    const FT_LBS_DATA: u16 = 0x0013;
    let mut off = 0usize;
    let mut ot = None;
    let mut state = None;
    let mut interface = false;
    while let (Some(ft), Some(cb)) = (get_u16(body, off), get_u16(body, off + 2)) {
        let start = off + 4;
        if ft == FT_END {
            break;
        }
        if ft == FT_LBS_DATA {
            interface = cb != 0 && interface_dropdown(body.get(start..).unwrap_or_default());
            break;
        }
        let Some(data) = body.get(start..start + usize::from(cb)) else {
            break;
        };
        match ft {
            FT_CMO => ot = get_u16(data, 0),
            FT_CBLS_DATA => state = get_u16(data, 0),
            _ => {}
        }
        off = start + usize::from(cb);
    }
    Some(match ot? {
        0x000B => Obj::Checkbox(match state {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        }),
        0x0012 | 0x0014 if interface => return None,
        0x0007 | 0x000C | 0x000D | 0x000E | 0x0010 | 0x0011 | 0x0012 | 0x0013 | 0x0014 => {
            Obj::Control
        }
        0x0008 => Obj::Picture,
        0x0005 => Obj::Chart,
        0x0002 | 0x0003 | 0x0006 | 0x0009 | 0x001E => Obj::Shape,
        _ => return None,
    })
}

/// Whether FtLbsData after its `ft` and `cbFContinued` names a behaviour
/// class (`lct`, read when `fUseCB` is set) other than a regular sheet
/// drop-down: an AutoFilter, PivotTable, AutoComplete, data validation or
/// table total-row drop-down, which the application draws itself.
fn interface_dropdown(lbs: &[u8]) -> bool {
    let flags_at = get_u16(lbs, 0).map(|cb_fmla| 2 + usize::from(cb_fmla) + 4);
    flags_at
        .and_then(|at| get_u16(lbs, at))
        .is_some_and(|flags| flags & 0x0001 != 0 && flags >> 8 != 0)
}

fn omit_header_footer(parse_ctx: &ParseCtx, header: bool) {
    if header {
        parse_ctx.emit(DiagnosticCode::HeaderOmitted, Region::Header);
    } else {
        parse_ctx.emit(DiagnosticCode::FooterOmitted, Region::Footer);
    }
}

/// An HLINK record's range with its external target: `Some(None)` for a
/// link within the workbook, [`LinkTarget::Unknown`] for a moniker that
/// does not read, and `None` for a record too short for its range or flags
/// ([MS-XLS] 2.4.140, [MS-OSHARED] 2.3.7.1).
fn hlink(body: &[u8]) -> Option<Option<SheetLink>> {
    const HAS_MONIKER: u32 = 0x01;
    let (r1, r2) = (u32::from(get_u16(body, 0)?), u32::from(get_u16(body, 2)?));
    let (c1, c2) = (u32::from(get_u16(body, 4)?), u32::from(get_u16(body, 6)?));
    let flags = get_u32(body, 28)?;
    if flags & HAS_MONIKER == 0 {
        return Some(None);
    }
    let target = body.get(32..).and_then(|data| hlink_target(data, flags));
    let range = CellRange::spanning(CellRef::new(r1, c1), CellRef::new(r2, c2));
    let target = target.map_or(LinkTarget::Unknown, |url| LinkTarget::Url(Rc::from(url)));
    Some(Some(SheetLink { range, target }))
}

/// The moniker's target after the optional display and frame names.
fn hlink_target(data: &[u8], flags: u32) -> Option<String> {
    const HAS_DISPLAY_NAME: u32 = 0x10;
    const HAS_FRAME_NAME: u32 = 0x80;
    const MONIKER_SAVED_AS_STR: u32 = 0x100;
    let string_at = |pos: usize| -> Option<(&[u8], usize)> {
        let len = usize::try_from(get_u32(data, pos)?).ok()?.checked_mul(2)?;
        let start = pos.checked_add(4)?;
        let end = start.checked_add(len)?;
        Some((data.get(start..end)?, end))
    };
    let mut pos = 0usize;
    for flag in [HAS_DISPLAY_NAME, HAS_FRAME_NAME] {
        if flags & flag != 0 {
            pos = string_at(pos)?.1;
        }
    }
    if flags & MONIKER_SAVED_AS_STR != 0 {
        return utf16_until_nul(string_at(pos)?.0);
    }
    moniker_target(data.get(pos..)?)
}

/// The target a URL or file moniker names ([MS-OSHARED] 2.3.7.6, 2.3.7.8).
fn moniker_target(data: &[u8]) -> Option<String> {
    const URL_MONIKER: [u8; 16] = [
        0xE0, 0xC9, 0xEA, 0x79, 0xF9, 0xBA, 0xCE, 0x11, 0x8C, 0x82, 0x00, 0xAA, 0x00, 0x4B, 0xA9,
        0x0B,
    ];
    const FILE_MONIKER: [u8; 16] = [0x03, 0x03, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];
    let (clsid, rest) = data.split_at_checked(16)?;
    if clsid == URL_MONIKER {
        let len = usize::try_from(get_u32(rest, 0)?).ok()?;
        utf16_until_nul(rest.get(4..)?.get(..len)?)
    } else if clsid == FILE_MONIKER {
        let len = usize::try_from(get_u32(rest, 2)?).ok()?;
        let path = rest.get(6..)?.get(..len)?;
        let path = path.split(|&b| b == 0).next()?;
        Some(path.iter().map(|&b| char::from(b)).collect())
    } else {
        None
    }
}

/// UTF-16LE text up to its terminating NUL; `None` when it does not decode.
fn utf16_until_nul(bytes: &[u8]) -> Option<String> {
    let units: Vec<u16> = utf16le_units(bytes).take_while(|&u| u != 0).collect();
    String::from_utf16(&units).ok()
}

/// The text of a TXO record: `cchText` characters spread over the CONTINUE
/// records after it, each opening with its own encoding flag byte; with
/// whether a unit did not decode.
fn txo_text(segs: &[&[u8]]) -> Option<(String, bool)> {
    let mut remaining = usize::from(get_u16(segs.first()?, 10)?);
    let mut units: Vec<u16> = Vec::with_capacity(remaining);
    for seg in segs.iter().skip(1) {
        if remaining == 0 {
            break;
        }
        let (&flags, chars) = seg.split_first()?;
        if flags & 0x01 != 0 {
            let take = (chars.len() / 2).min(remaining);
            units.extend(utf16le_units(&chars[..take * 2]));
            remaining -= take;
        } else {
            let take = chars.len().min(remaining);
            units.extend(chars[..take].iter().map(|&b| u16::from(b)));
            remaining -= take;
        }
    }
    Some(decode_utf16(units))
}

fn put_number(out: &mut SheetContent, at: CellRef, globals: &Globals, ixfe: u16, n: f64) {
    let rendered = render_number(&globals.format(ixfe), n, globals.dates);
    put_rendered(out, at, globals, ixfe, rendered, false);
}

fn put_string(
    out: &mut SheetContent,
    at: CellRef,
    globals: &Globals,
    ixfe: u16,
    text: Option<(String, bool)>,
) {
    match text {
        Some((text, replaced)) => {
            let rendered = format_as_text(&globals.format(ixfe), &clean_text(&text));
            put_rendered(out, at, globals, ixfe, rendered, replaced);
        }
        None => out.lose(at, DiagnosticCode::CellValueUnreadable),
    }
}

/// Record a cell's text, what rendering it lost, and a replacement
/// character its string decoded with or its format code put in it.
fn put_rendered(
    out: &mut SheetContent,
    at: CellRef,
    globals: &Globals,
    ixfe: u16,
    (text, loss): (String, Option<DiagnosticCode>),
    replaced: bool,
) {
    let replaced = replaced || globals.xfs.replaced_in(usize::from(ixfe), &text);
    out.put(at, text);
    for code in loss.into_iter().chain(replaced.then_some(DiagnosticCode::InvalidEncoding)) {
        out.lose(at, code);
    }
}

/// A reader over `segs` with the first segment's leading `skip` bytes (the
/// cell header before the string) removed.
fn string_reader<'a>(segs: &'a [&'a [u8]], skip: usize) -> Option<SegReader<'a>> {
    let mut parts: Vec<&[u8]> = Vec::with_capacity(segs.len());
    parts.push(segs.first()?.get(skip..)?);
    parts.extend(&segs[1..]);
    Some(SegReader::new(parts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CellSlot, Table, inlines_to_plain_text};
    use std::io::Write;

    fn rec(rec_type: u16, body: &[u8]) -> Vec<u8> {
        let mut out = rec_type.to_le_bytes().to_vec();
        out.extend((body.len() as u16).to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    fn bof(dt: u16, vers: u16) -> Vec<u8> {
        let mut body = vec![0u8; 16];
        body[..2].copy_from_slice(&vers.to_le_bytes());
        body[2..4].copy_from_slice(&dt.to_le_bytes());
        rec(BOF, &body)
    }

    /// XLUnicodeString, compressed ASCII.
    fn ustr(s: &str) -> Vec<u8> {
        let mut out = (s.len() as u16).to_le_bytes().to_vec();
        out.push(0);
        out.extend_from_slice(s.as_bytes());
        out
    }

    /// ShortXLUnicodeString, compressed ASCII.
    fn short_ustr(s: &str) -> Vec<u8> {
        let mut out = vec![s.len() as u8, 0];
        out.extend_from_slice(s.as_bytes());
        out
    }

    fn cell6(row: u16, col: u16, ixfe: u16) -> Vec<u8> {
        [row, col, ixfe].iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn labelsst(row: u16, col: u16, ixfe: u16, isst: u32) -> Vec<u8> {
        let mut body = cell6(row, col, ixfe);
        body.extend(isst.to_le_bytes());
        rec(LABELSST, &body)
    }

    fn label(row: u16, col: u16, ixfe: u16, s: &str) -> Vec<u8> {
        let mut body = cell6(row, col, ixfe);
        body.extend(ustr(s));
        rec(LABEL, &body)
    }

    fn number(row: u16, col: u16, ixfe: u16, v: f64) -> Vec<u8> {
        let mut body = cell6(row, col, ixfe);
        body.extend(v.to_le_bytes());
        rec(NUMBER, &body)
    }

    fn rk_cell(row: u16, col: u16, ixfe: u16, rk: u32) -> Vec<u8> {
        let mut body = cell6(row, col, ixfe);
        body.extend(rk.to_le_bytes());
        rec(RK, &body)
    }

    fn formula(row: u16, col: u16, ixfe: u16, value: [u8; 8]) -> Vec<u8> {
        let mut body = cell6(row, col, ixfe);
        body.extend(value);
        body.extend([0u8; 6]);
        rec(FORMULA, &body)
    }

    fn rk_from_f64(v: f64) -> u32 {
        ((v.to_bits() >> 32) as u32) & !3
    }

    fn rk_from_int(v: i32) -> u32 {
        ((v as u32) << 2) | 2
    }

    fn ole_with(name: &str, data: &[u8]) -> Vec<u8> {
        let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        ole.create_stream(name).unwrap().write_all(data).unwrap();
        ole.into_inner().into_inner()
    }

    /// Assemble a BIFF8 workbook: globals plus one substream per sheet,
    /// with each BOUNDSHEET's lbPlyPos patched to the real offset.
    #[derive(Default)]
    struct Wb {
        date1904: bool,
        filepass: bool,
        /// FORMAT records: (ifmt, code).
        formats: Vec<(u16, &'static str)>,
        /// XF records in table order, each carrying its ifmt.
        xfs: Vec<u16>,
        /// Raw SST records (the SST plus any CONTINUE records).
        sst: Vec<Vec<u8>>,
        /// (name, hsState, substream records without BOF/EOF).
        sheets: Vec<(&'static str, u8, Vec<u8>)>,
        /// The sheet whose name is written in UTF-16 with a lone surrogate
        /// after it.
        broken_name: Option<usize>,
        /// Further globals records after the sheet directory, verbatim.
        globals: Vec<u8>,
        /// Bytes cut from the end of the stream.
        cut: usize,
        /// The globals substream has no EOF of its own.
        globals_unended: bool,
    }

    fn format_record(ifmt: u16, code: &str) -> Vec<u8> {
        let mut body = ifmt.to_le_bytes().to_vec();
        body.extend(ustr(code));
        rec(FORMAT, &body)
    }

    impl Wb {
        fn build(&self) -> Vec<u8> {
            let mut stream = bof(0x0005, 0x0600);
            if self.filepass {
                stream.extend(rec(FILEPASS, &[0u8; 6]));
            }
            if self.date1904 {
                stream.extend(rec(DATEMODE, &1u16.to_le_bytes()));
            }
            for &(ifmt, code) in &self.formats {
                stream.extend(format_record(ifmt, code));
            }
            for ifmt in &self.xfs {
                let mut body = vec![0u8; 20];
                body[2..4].copy_from_slice(&ifmt.to_le_bytes());
                stream.extend(rec(XF, &body));
            }
            for raw in &self.sst {
                stream.extend(raw);
            }
            let mut patch_at = Vec::new();
            for (i, (name, state, _)) in self.sheets.iter().enumerate() {
                let mut body = vec![0u8; 4];
                body.push(*state);
                body.push(0);
                if self.broken_name == Some(i) {
                    body.extend([name.len() as u8 + 1, 1]);
                    body.extend(name.encode_utf16().chain([0xD800]).flat_map(u16::to_le_bytes));
                } else {
                    body.extend(short_ustr(name));
                }
                patch_at.push(stream.len() + 4);
                stream.extend(rec(BOUNDSHEET, &body));
            }
            stream.extend_from_slice(&self.globals);
            if !self.globals_unended {
                stream.extend(rec(EOF_REC, &[]));
            }
            for (i, (_, _, records)) in self.sheets.iter().enumerate() {
                let offset = (stream.len() as u32).to_le_bytes();
                stream[patch_at[i]..patch_at[i] + 4].copy_from_slice(&offset);
                stream.extend(bof(WORKSHEET_SUBSTREAM, 0x0600));
                stream.extend_from_slice(records);
                stream.extend(rec(EOF_REC, &[]));
            }
            stream.truncate(stream.len() - self.cut);
            ole_with("Workbook", &stream)
        }
    }

    fn one_sheet(records: Vec<u8>) -> Wb {
        Wb { xfs: vec![0], sheets: vec![("S", 0, records)], ..Wb::default() }
    }

    fn first_table(doc: &Document) -> &Table {
        match doc.blocks.iter().find_map(|b| match b {
            Block::Table(t) => Some(t),
            _ => None,
        }) {
            Some(t) => t,
            None => panic!("expected a table, got {:?}", doc.blocks),
        }
    }

    fn texts(table: &Table) -> Vec<Vec<String>> {
        table
            .grid
            .iter()
            .map(|row| {
                row.iter()
                    .map(|slot| match slot {
                        CellSlot::Origin(cell) => cell
                            .blocks
                            .iter()
                            .filter_map(|b| match b {
                                Block::Paragraph(i) => Some(inlines_to_plain_text(i)),
                                _ => None,
                            })
                            .collect(),
                        CellSlot::Covered { .. } => "<covered>".to_string(),
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn sst_string_split_mid_word_re_reads_the_flags_byte() {
        // Trap 1: the string continues into a CONTINUE record and switches
        // from 8-bit compressed to 16-bit encoding at the boundary, marked
        // by the repeated option-flags byte.
        let mut base = 1u32.to_le_bytes().to_vec();
        base.extend(1u32.to_le_bytes());
        base.extend(11u16.to_le_bytes());
        base.push(0x00);
        base.extend(b"HELLO");
        let mut cont = vec![0x01];
        for unit in " WORLD".encode_utf16() {
            cont.extend(unit.to_le_bytes());
        }
        let wb = Wb {
            sst: vec![rec(SST, &base), rec(CONTINUE, &cont)],
            ..one_sheet(labelsst(0, 0, 0, 0))
        };
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["HELLO WORLD"]]);
    }

    #[test]
    fn rich_and_phonetic_headers_do_not_misalign_the_table() {
        // Trap 2: the optional rich-run count and phonetic size headers and
        // their trailing data must be consumed, or the next string reads
        // from the middle of them.
        let mut body = 2u32.to_le_bytes().to_vec();
        body.extend(2u32.to_le_bytes());
        body.extend(2u16.to_le_bytes());
        body.push(0x0C); // fRichSt | fExtSt
        body.extend(1u16.to_le_bytes()); // cRun
        body.extend(4u32.to_le_bytes()); // cbExtRst
        body.extend(b"ab");
        body.extend([0xAA; 4]); // the rich run
        body.extend([0xBB; 4]); // the phonetic block
        body.extend(ustr("ok"));
        let mut records = labelsst(0, 0, 0, 0);
        records.extend(labelsst(0, 1, 0, 1));
        let wb = Wb { sst: vec![rec(SST, &body)], ..one_sheet(records) };
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["ab", "ok"]]);
    }

    #[test]
    fn rk_and_mulrk_encodings_decode() {
        // Trap 3: RK packs an integer or a truncated double, with a
        // divide-by-100 bit; MULRK packs a run of them, each with its own
        // format index.
        let mut records = rk_cell(0, 0, 0, rk_from_f64(1234.5));
        records.extend(rk_cell(0, 1, 0, rk_from_int(15550) | 1)); // 155.5
        let mut mulrk = cell6(0, 2, 0)[..4].to_vec(); // rw, colFirst
        mulrk.extend(0u16.to_le_bytes()); // ixfe General
        mulrk.extend(rk_from_int(7).to_le_bytes());
        mulrk.extend(1u16.to_le_bytes()); // ixfe of the percent XF
        mulrk.extend(rk_from_f64(0.5).to_le_bytes());
        mulrk.extend(3u16.to_le_bytes()); // colLast
        records.extend(rec(MULRK, &mulrk));
        let wb = Wb {
            formats: vec![(164, "0%")],
            xfs: vec![0, 164],
            sheets: vec![("S", 0, records)],
            ..Wb::default()
        };
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["1234.5", "155.5", "7", "50%"]]);
    }

    #[test]
    fn number_formats_apply_to_numeric_cells() {
        // Trap 4: the cell's ixfe indexes the XF table, whose ifmt resolves
        // against FORMAT records first and the built-in table second (ifmt
        // 3 has no FORMAT record here).
        let mut records = number(0, 0, 1, 0.155);
        records.extend(number(0, 1, 2, 1234.5));
        records.extend(number(0, 2, 3, 9876543.0));
        let wb = Wb {
            formats: vec![(164, "0.0%"), (165, "\"$\"#,##0.00")],
            xfs: vec![0, 164, 165, 3],
            sheets: vec![("S", 0, records)],
            ..Wb::default()
        };
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["15.5%", "$1,234.50", "9,876,543"]]);
    }

    #[test]
    fn date_serials_render_iso_in_both_date_systems() {
        let dated = |date1904| Wb {
            date1904,
            formats: vec![(164, "yyyy-mm-dd")],
            xfs: vec![164],
            sheets: vec![("S", 0, number(0, 0, 0, if date1904 { 100.0 } else { 46096.0 }))],
            ..Wb::default()
        };
        let doc = parse(&ParseCtx::default(), &dated(false).build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["2026-03-15"]]);
        let doc = parse(&ParseCtx::default(), &dated(true).build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["1904-04-10"]]);
    }

    #[test]
    fn hidden_rows_columns_and_sheets_are_omitted() {
        let mut records = label(0, 0, 0, "a");
        records.extend(label(0, 1, 0, "hidden col"));
        records.extend(label(0, 2, 0, "c"));
        records.extend(label(1, 0, 0, "hidden row"));
        records.extend(label(2, 0, 0, "d"));
        let mut row = vec![0u8; 16];
        row[..2].copy_from_slice(&1u16.to_le_bytes());
        row[12] = 0x20; // fDyZero
        records.extend(rec(ROW, &row));
        let mut colinfo = vec![0u8; 12];
        colinfo[..2].copy_from_slice(&1u16.to_le_bytes());
        colinfo[2..4].copy_from_slice(&1u16.to_le_bytes());
        colinfo[8] = 0x01; // fHidden
        records.extend(rec(COLINFO, &colinfo));
        let wb = Wb {
            xfs: vec![0],
            sheets: vec![("Shown", 0, records), ("Secret", 1, label(0, 0, 0, "secret"))],
            ..Wb::default()
        };
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(doc.blocks.len(), 1, "hidden sheet must add no heading and no table");
        assert_eq!(texts(first_table(&doc)), vec![vec!["a", "c"], vec!["d", ""]]);
    }

    #[test]
    fn a_merge_past_the_content_is_clamped_to_it() {
        let mut records = label(0, 5, 0, "wide");
        let mut merged = 1u16.to_le_bytes().to_vec();
        for v in [0u16, 2, 5, 14] {
            merged.extend(v.to_le_bytes());
        }
        records.extend(rec(MERGEDCELLS, &merged));
        let wb = one_sheet(records);
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["wide"]]);
        assert_eq!(diagnose(&wb), []);
    }

    #[test]
    fn a_cell_a_merge_clamped_to_the_content_covers_shows_nothing() {
        let merged = |v: [u16; 4]| {
            let body: Vec<u8> = [1u16].iter().chain(&v).flat_map(|v| v.to_le_bytes()).collect();
            rec(MERGEDCELLS, &body)
        };
        let records = [
            label(1, 0, 0, "a"),
            label(1, 1, 0, "hidden"),
            label(1, 2, 0, "c"),
            merged([0, 1, 1, 1]),
        ]
        .concat();
        let wb = one_sheet(records);
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["a", "", "c"]]);
        assert_eq!(diagnose(&wb), []);
        let records = [
            label(2, 0, 0, "a"),
            label(2, 1, 0, "hidden"),
            label(2, 3, 0, "d"),
            merged([1, 2, 1, 2]),
        ]
        .concat();
        let doc = parse(&ParseCtx::default(), &one_sheet(records).build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["a", "", "<covered>", "d"]]);
    }

    #[test]
    fn a_cell_past_the_biff8_columns_is_unreadable_only_where_its_text_shows() {
        let mut records = label(0, 0, 0, "a");
        records.extend(label(0, 256, 0, "past"));
        records.extend(label(0, 257, 0, " "));
        records.extend(number(1, 300, 0, 1.0));
        let mut mulrk = cell6(2, 256, 0)[..4].to_vec();
        for _ in 0..2 {
            mulrk.extend(0u16.to_le_bytes());
            mulrk.extend(rk_from_int(7).to_le_bytes());
        }
        mulrk.extend(257u16.to_le_bytes());
        records.extend(rec(MULRK, &mulrk));
        records.extend(rec(BOOLERR, &[cell6(0, 258, 0), vec![0x99, 1]].concat()));
        records.extend(formula(0, 259, 0, [2, 0, 0x99, 0, 0, 0, 0xFF, 0xFF]));
        records.extend(rec(LABEL, &[cell6(0, 260, 0), vec![9, 0, 0, b'a']].concat()));
        records.extend(label(3, 256, 0, "past in a hidden row"));
        records.extend(rec(BOOLERR, &[cell6(3, 258, 0), vec![0x99, 1]].concat()));
        records.extend(hidden_row(3));
        let wb = one_sheet(records);
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["a"]]);
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::CellValueUnreadable, 1, 7),
                in_unit(DiagnosticCode::HiddenOmitted, 1, 1)
            ]
        );
    }

    fn hidden_cols(first: u16, last: u16) -> Vec<u8> {
        let mut colinfo = vec![0u8; 12];
        colinfo[..2].copy_from_slice(&first.to_le_bytes());
        colinfo[2..4].copy_from_slice(&last.to_le_bytes());
        colinfo[8] = 0x01;
        rec(COLINFO, &colinfo)
    }

    fn hidden_row(row: u16) -> Vec<u8> {
        let mut body = vec![0u8; 16];
        body[..2].copy_from_slice(&row.to_le_bytes());
        body[12] = 0x20;
        rec(ROW, &body)
    }

    #[test]
    fn a_hidden_column_past_the_biff8_columns_reports_its_content_as_hidden_only() {
        let records = [hidden_cols(256, 300), label(0, 0, 0, "a"), label(0, 256, 0, "past")];
        let wb = one_sheet(records.concat());
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["a"]]);
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::HiddenOmitted, 1, 1)]);
    }

    #[test]
    fn a_merge_moved_off_a_hidden_origin_replaces_the_covered_text_it_lands_on() {
        let merged = |v: [u16; 4]| {
            let body: Vec<u8> = [1u16].iter().chain(&v).flat_map(|v| v.to_le_bytes()).collect();
            rec(MERGEDCELLS, &body)
        };
        let run = |records: Vec<u8>| {
            texts(first_table(&parse(&ParseCtx::default(), &one_sheet(records).build()).unwrap()))
        };
        let below =
            [hidden_row(0), label(1, 0, 0, "covered"), label(1, 1, 0, "y"), merged([0, 2, 0, 0])];
        assert_eq!(run(below.concat()), vec![vec!["y"]]);
        let beside = [
            hidden_cols(0, 0),
            label(0, 1, 0, "covered"),
            label(0, 2, 0, "y"),
            merged([0, 0, 0, 1]),
        ];
        assert_eq!(run(beside.concat()), vec![vec!["y"]]);
        let kept = [
            hidden_row(0),
            label(0, 0, 0, "kept"),
            label(1, 0, 0, "covered"),
            label(1, 1, 0, "y"),
            merged([0, 2, 0, 0]),
        ];
        assert_eq!(run(kept.concat()), vec![vec!["kept", "y"]]);
    }

    #[test]
    fn a_merge_past_the_biff8_columns_is_clamped_and_one_outside_them_dropped() {
        let mut records = label(0, 250, 0, "wide");
        let mut merged = 2u16.to_le_bytes().to_vec();
        for v in [0u16, 0, 250, 300, 0, 0, 256, 300] {
            merged.extend(v.to_le_bytes());
        }
        records.extend(rec(MERGEDCELLS, &merged));
        let wb = one_sheet(records);
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        let table = first_table(&doc);
        let CellSlot::Origin(cell) = &table.grid[0][0] else {
            panic!("expected the merge origin at (0,0)");
        };
        assert_eq!((table.grid[0].len(), cell.col_span), (1, 1));
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::TableSpanRepaired, 1, 2)]);
    }

    #[test]
    fn formula_cached_values_render() {
        let mut records = formula(0, 0, 0, 2.5f64.to_le_bytes());
        records.extend(formula(0, 1, 0, [0x00, 0, 0, 0, 0, 0, 0xFF, 0xFF]));
        records.extend(rec(STRING, &ustr("calc")));
        records.extend(formula(0, 2, 0, [0x01, 0, 1, 0, 0, 0, 0xFF, 0xFF]));
        records.extend(formula(0, 3, 0, [0x02, 0, 0x17, 0, 0, 0, 0xFF, 0xFF]));
        let doc = parse(&ParseCtx::default(), &one_sheet(records).build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["2.5", "calc", "TRUE", "#REF!"]]);
    }

    #[test]
    fn boolerr_renders_bool_and_error_literals() {
        let cell = |col: u16, value: u8, is_err: u8| {
            let mut body = cell6(0, col, 0);
            body.extend([value, is_err]);
            rec(BOOLERR, &body)
        };
        let mut records = cell(0, 1, 0);
        records.extend(cell(1, 0, 0));
        records.extend(cell(2, 0x07, 1));
        let doc = parse(&ParseCtx::default(), &one_sheet(records).build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["TRUE", "FALSE", "#DIV/0!"]]);
    }

    #[test]
    fn filepass_means_encrypted() {
        let wb = Wb { filepass: true, ..one_sheet(label(0, 0, 0, "x")) };
        assert!(matches!(parse(&ParseCtx::default(), &wb.build()), Err(ConvertError::Encrypted)));
    }

    #[test]
    fn biff5_byte_strings_degrade_gracefully() {
        // A BIFF5 stream in a `Book` container: one-byte-length boundsheet
        // names and codepage-encoded LABEL text, no SST.
        let mut stream = bof(0x0005, 0x0500);
        stream.extend(rec(CODEPAGE, &1252u16.to_le_bytes()));
        let mut xf = vec![0u8; 16];
        xf[2..4].copy_from_slice(&0u16.to_le_bytes());
        stream.extend(rec(XF, &xf));
        let mut boundsheet = vec![0u8; 6];
        boundsheet.extend([1, b'S']);
        let patch_at = stream.len() + 4;
        stream.extend(rec(BOUNDSHEET, &boundsheet));
        stream.extend(rec(EOF_REC, &[]));
        let offset = (stream.len() as u32).to_le_bytes();
        stream[patch_at..patch_at + 4].copy_from_slice(&offset);
        stream.extend(bof(WORKSHEET_SUBSTREAM, 0x0500));
        let mut body = cell6(0, 0, 0);
        body.extend(6u16.to_le_bytes());
        body.extend(b"l\xE9gacy"); // cp1252 e-acute
        stream.extend(rec(LABEL, &body));
        stream.extend(rec(EOF_REC, &[]));
        let doc = parse(&ParseCtx::default(), &ole_with("Book", &stream)).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["l\u{e9}gacy"]]);
    }

    /// An MSODRAWING body: one SpContainer holding a client anchor that
    /// starts at (row, col), plus an OPT hiding the shape when asked.
    fn msodrawing(row: u16, col: u16, hidden: bool) -> Vec<u8> {
        let mut anchor = vec![0u8; 18];
        anchor[2..4].copy_from_slice(&col.to_le_bytes());
        anchor[6..8].copy_from_slice(&row.to_le_bytes());
        anchor[10..12].copy_from_slice(&(col + 1).to_le_bytes());
        anchor[14..16].copy_from_slice(&(row + 1).to_le_bytes());
        let mut children = Vec::new();
        if hidden {
            children.extend_from_slice(&0x0013u16.to_le_bytes()); // one property
            children.extend_from_slice(&0xF00Bu16.to_le_bytes());
            children.extend_from_slice(&6u32.to_le_bytes());
            children.extend_from_slice(&0x03BFu16.to_le_bytes());
            children.extend_from_slice(&0x0002_0002u32.to_le_bytes());
        }
        children.extend_from_slice(&0u16.to_le_bytes());
        children.extend_from_slice(&0xF010u16.to_le_bytes());
        children.extend_from_slice(&(anchor.len() as u32).to_le_bytes());
        children.extend(anchor);
        let mut body = Vec::new();
        body.extend_from_slice(&0x000Fu16.to_le_bytes());
        body.extend_from_slice(&0xF004u16.to_le_bytes());
        body.extend_from_slice(&(children.len() as u32).to_le_bytes());
        body.extend(children);
        rec(MSODRAWING, &body)
    }

    /// An OBJ record for a form control of type `ot` carrying `state`.
    fn obj(ot: u16, state: u16) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&0x0015u16.to_le_bytes());
        body.extend_from_slice(&0x0012u16.to_le_bytes());
        body.extend_from_slice(&ot.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&[0u8; 14]);
        body.extend_from_slice(&0x0012u16.to_le_bytes());
        body.extend_from_slice(&8u16.to_le_bytes());
        body.extend_from_slice(&state.to_le_bytes());
        body.extend_from_slice(&[0u8; 6]);
        body.extend_from_slice(&[0u8; 4]);
        rec(OBJ, &body)
    }

    /// A TXO record with its text and formatting-run CONTINUE records.
    fn txo(text: &str) -> Vec<u8> {
        let mut body = vec![0u8; 18];
        body[10..12].copy_from_slice(&(text.len() as u16).to_le_bytes());
        body[12..14].copy_from_slice(&16u16.to_le_bytes());
        let mut out = rec(TXO, &body);
        let mut chars = vec![0x00];
        chars.extend_from_slice(text.as_bytes());
        out.extend(rec(CONTINUE, &chars));
        out.extend(rec(CONTINUE, &[0u8; 16]));
        out
    }

    /// The MSODRAWING that carries an OBJ's text box into the TXO after it.
    fn client_textbox() -> Vec<u8> {
        rec(MSODRAWING, &[0, 0, 0x0D, 0xF0, 0, 0, 0, 0])
    }

    /// Re-frame a record as its first `at` payload bytes plus a CONTINUE
    /// carrying the rest.
    fn split_at(record: &[u8], at: usize) -> Vec<u8> {
        let rec_type = u16::from_le_bytes([record[0], record[1]]);
        let payload = &record[4..];
        let mut out = rec(rec_type, &payload[..at]);
        out.extend(rec(CONTINUE, &payload[at..]));
        out
    }

    #[test]
    fn form_control_checkboxes_land_in_their_anchor_cell() {
        let mut records = label(20, 0, 0, "14");
        records.extend(split_at(&msodrawing(20, 3, false), 12));
        records.extend(obj(0x0B, 1));
        records.extend(client_textbox());
        records.extend(txo("Roof"));
        records.extend(msodrawing(20, 4, false));
        records.extend(obj(0x0B, 0));
        // A text box is a drawing object too, and must not be read as a box.
        records.extend(msodrawing(20, 5, false));
        records.extend(obj(0x06, 1));
        records.extend(txo("note"));
        records.extend(msodrawing(20, 6, true));
        records.extend(obj(0x0B, 1));
        let doc = parse(&ParseCtx::default(), &one_sheet(records).build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["14", "", "", "[x] Roof", "[ ]"]]);
    }

    type Reported = (DiagnosticCode, Region, Option<u32>, u64);

    fn diagnose(wb: &Wb) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, &wb.build()).unwrap();
        reported(parse_ctx)
    }

    fn reported(parse_ctx: ParseCtx) -> Vec<Reported> {
        let unit = |d: &crate::diagnostics::Diagnostic| d.unit.map(std::num::NonZeroU32::get);
        parse_ctx.into_diagnostics().iter().map(|d| (d.code, d.region, unit(&d), d.count)).collect()
    }

    fn in_unit(code: DiagnosticCode, unit: u32, count: u64) -> Reported {
        let region = match code {
            DiagnosticCode::HeaderOmitted => Region::Header,
            DiagnosticCode::FooterOmitted => Region::Footer,
            DiagnosticCode::CommentOmitted => Region::Comment,
            _ => Region::Body,
        };
        (code, region, Some(unit), count)
    }

    #[test]
    fn each_sheet_site_reports_its_loss_in_its_sheets_unit() {
        let mut far = 0x00FF_FFFFu32.to_le_bytes().to_vec();
        far.extend([0, 0]);
        far.extend(short_ustr("Far"));
        let hidden_cut_short = [0, 0, 0, 0, 1, 0, 5, 0, b'a'];
        let shown_cut_short = [0, 0, 0, 0, 0, 0, 5, 0, b'a'];
        let module = [0, 0, 0, 0, 0, 6, 1, 0, b'm'];
        let wb = Wb {
            xfs: vec![0],
            sheets: vec![("A", 0, number(0, 0, 0, 1.0)), ("H", 1, number(0, 0, 0, 1.0))],
            globals: [
                rec(BOUNDSHEET, &[0, 0, 0]),
                rec(BOUNDSHEET, &module),
                rec(BOUNDSHEET, &far),
                rec(BOUNDSHEET, &hidden_cut_short),
                rec(BOUNDSHEET, &shown_cut_short),
            ]
            .concat(),
            ..Wb::default()
        };
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::UnitUnreadable, 3, 1),
                in_unit(DiagnosticCode::UnitUnreadable, 4, 1),
                in_unit(DiagnosticCode::UnitUnreadable, 6, 1),
                in_unit(DiagnosticCode::HiddenOmitted, 2, 1),
                in_unit(DiagnosticCode::HiddenOmitted, 5, 1),
            ]
        );
    }

    #[test]
    fn a_shown_sheet_name_that_does_not_decode_reports_with_its_heading() {
        let sheets = vec![("A", 0, number(0, 0, 0, 1.0)), ("B", 0, number(0, 0, 0, 1.0))];
        let wb = Wb { xfs: vec![0], sheets, broken_name: Some(1), ..Wb::default() };
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::InvalidEncoding, 2, 1)]);
        let alone = Wb { broken_name: Some(0), ..one_sheet(number(0, 0, 0, 1.0)) };
        assert_eq!(diagnose(&alone), []);
        let hidden = Wb {
            xfs: vec![0],
            sheets: vec![("A", 0, number(0, 0, 0, 1.0)), ("H", 1, number(0, 0, 0, 1.0))],
            broken_name: Some(1),
            ..Wb::default()
        };
        assert_eq!(diagnose(&hidden), [in_unit(DiagnosticCode::HiddenOmitted, 2, 1)]);
    }

    #[test]
    fn a_cell_record_that_does_not_read_reports_it() {
        let mut records = labelsst(0, 0, 0, 9);
        records.extend(rec(BOOLERR, &[cell6(1, 0, 0), vec![0x99, 1]].concat()));
        records.extend(formula(2, 0, 0, [2, 0, 0x99, 0, 0, 0, 0xFF, 0xFF]));
        records.extend(rec(LABEL, &[cell6(3, 0, 0), vec![9, 0, 0, b'a']].concat()));
        records.extend(rec(LABEL, &[cell6(4, 0, 0), vec![1, 0, 1, 0x00, 0xDC]].concat()));
        records.extend(formula(5, 0, 0, [0, 0, 0, 0, 0, 0, 0xFF, 0xFF]));
        records.extend(rec(STRING, &[9, 0, 0]));
        records.extend(number(6, 0, 7, 1.0));
        records.extend(labelsst(7, 0, 0, 1));
        records.extend(labelsst(8, 0, 0, 0));
        records.extend(label(9, 0, 0, "last"));
        let mut sst = 3u32.to_le_bytes().to_vec();
        sst.extend(3u32.to_le_bytes());
        sst.extend(ustr("ok"));
        sst.extend([1, 0, 1, 0x00, 0xD8]);
        sst.extend([1, 0, 1, 0x00, 0xDC]);
        let wb = Wb { sst: vec![rec(SST, &sst)], cut: 1, ..one_sheet(records) };
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::CellValueUnreadable, 1, 5),
                in_unit(DiagnosticCode::TruncatedRecords, 1, 1),
                in_unit(DiagnosticCode::InvalidEncoding, 1, 2),
                in_unit(DiagnosticCode::NumberFormatFallback, 1, 1),
            ]
        );
    }

    #[test]
    fn a_hidden_cell_reports_only_that_it_is_hidden() {
        let mut records = label(0, 0, 0, "shown");
        records.extend(hidden_row(1));
        records.extend(hidden_cols(2, 2));
        records.extend(labelsst(1, 0, 0, 9));
        records.extend(number(1, 1, 7, 1.0));
        records.extend(rec(LABEL, &[cell6(0, 2, 0), vec![1, 0, 1, 0x00, 0xDC]].concat()));
        records.extend(rec(BOOLERR, &[cell6(3, 2, 0), vec![0x99, 1]].concat()));
        records.extend(msodrawing(1, 3, false));
        records.extend(obj(0x0B, 1));
        let mut caption =
            rec(TXO, &[vec![0u8; 10], 1u16.to_le_bytes().to_vec(), vec![0; 6]].concat());
        caption.extend(rec(CONTINUE, &[1, 0x00, 0xD8]));
        records.extend(caption);
        records.extend(msodrawing(1, 4, false));
        records.extend(obj(0x0B, 2));
        records.extend(msodrawing(3, 2, false));
        records.extend(obj(0x0B, 2));
        assert_eq!(diagnose(&one_sheet(records)), [in_unit(DiagnosticCode::HiddenOmitted, 1, 2)]);
    }

    #[test]
    fn a_biff5_string_that_does_not_decode_reports_in_its_cell() {
        let mut stream = bof(0x0005, 0x0500);
        stream.extend(rec(CODEPAGE, &1253u16.to_le_bytes()));
        let mut boundsheet = vec![0u8; 6];
        boundsheet.extend([1, b'S']);
        let patch_at = stream.len() + 4;
        stream.extend(rec(BOUNDSHEET, &boundsheet));
        stream.extend(rec(EOF_REC, &[]));
        let offset = (stream.len() as u32).to_le_bytes();
        stream[patch_at..patch_at + 4].copy_from_slice(&offset);
        stream.extend(bof(WORKSHEET_SUBSTREAM, 0x0500));
        stream.extend(rec(LABEL, &[cell6(0, 0, 0), vec![2, 0, 0xAA, 0xAB]].concat()));
        stream.extend(rec(EOF_REC, &[]));
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, &ole_with("Book", &stream)).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["\u{FFFD}\u{AB}"]]);
        assert_eq!(reported(parse_ctx), [in_unit(DiagnosticCode::InvalidEncoding, 1, 1)]);
    }

    #[test]
    fn a_checkbox_caption_that_does_not_decode_reports_in_its_cell() {
        let mut records = label(0, 0, 0, "x");
        records.extend(msodrawing(0, 1, false));
        records.extend(obj(0x0B, 1));
        let mut caption =
            rec(TXO, &[vec![0u8; 10], 2u16.to_le_bytes().to_vec(), vec![0; 6]].concat());
        caption.extend(rec(CONTINUE, &[1, 0x00, 0xD8, b'a', 0]));
        records.extend(caption);
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, &one_sheet(records).build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["x", "[x] \u{FFFD}a"]]);
        assert_eq!(reported(parse_ctx), [in_unit(DiagnosticCode::InvalidEncoding, 1, 1)]);
    }

    /// An OBJ record for a drop-down whose FtLbsData claims more bytes than
    /// the record holds, as Excel and Apache POI write it, of behaviour class
    /// `lct` (0 for a form control).
    fn dropdown(lct: u8) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&0x0015u16.to_le_bytes());
        body.extend_from_slice(&0x0012u16.to_le_bytes());
        body.extend_from_slice(&0x0014u16.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&[0u8; 14]);
        body.extend_from_slice(&0x0013u16.to_le_bytes());
        body.extend_from_slice(&0x1FEEu16.to_le_bytes());
        body.extend_from_slice(&[0u8; 6]);
        body.extend_from_slice(&[u8::from(lct != 0), lct]);
        body.extend_from_slice(&[0u8; 2]);
        body.extend_from_slice(&[0x02, 0, 8, 0, 20, 0, 0, 0, 0]);
        rec(OBJ, &body)
    }

    #[test]
    fn drawing_objects_notes_headers_and_links_are_reported() {
        let mut records = label(0, 1, 0, "see here");
        records.extend(label(1, 0, 0, "https://example.org/"));
        for (hidden, ot, state) in [
            (true, 0x0B, 1),
            (false, 0x0B, 2),
            (false, 0x0C, 0),
            (true, 0x0C, 0),
            (false, 0x08, 0),
            (true, 0x08, 0),
            (false, 0x05, 0),
        ] {
            records.extend(msodrawing(0, 3, hidden));
            records.extend(obj(ot, state));
        }
        records.extend(obj(0x0B, 1));
        records.extend(msodrawing(0, 4, false));
        records.extend(dropdown(0));
        records.extend(msodrawing(0, 5, false));
        records.extend(dropdown(3));
        records.extend(rec(NOTE, &[0u8; 12]));
        records.extend(rec(HEADER, &ustr("&CTitle")));
        records.extend(rec(FOOTER, &[]));
        records.extend(hlink_record(0, 1, "https://example.org/paper"));
        records.extend(hlink_record(1, 0, "https://example.org/"));
        let mut internal = hlink_record(0, 2, "");
        internal[4 + 28] = 0x08;
        records.extend(internal);
        records.extend(rec(HLINK, &[0u8; 20]));
        assert_eq!(
            diagnose(&one_sheet(records)),
            [
                in_unit(DiagnosticCode::ImageUnplaced, 1, 1),
                in_unit(DiagnosticCode::GraphicUnread, 1, 1),
                in_unit(DiagnosticCode::LinkTargetUnread, 1, 2),
                in_unit(DiagnosticCode::FormControlUnread, 1, 4),
                in_unit(DiagnosticCode::HeaderOmitted, 1, 1),
                in_unit(DiagnosticCode::CommentOmitted, 1, 1),
                in_unit(DiagnosticCode::HiddenOmitted, 1, 2),
            ]
        );
    }

    /// An HLINK record over one cell whose URL moniker names `url`.
    fn hlink_record(row: u16, col: u16, url: &str) -> Vec<u8> {
        hlink_over([row, row], [col, col], None, url)
    }

    /// An HLINK record over `rows` and `cols` as written, whose URL moniker
    /// names `url`, after a display name when one is given.
    fn hlink_over(rows: [u16; 2], cols: [u16; 2], display: Option<&str>, url: &str) -> Vec<u8> {
        let utf16 = |s: &str| -> Vec<u8> {
            let units: Vec<u8> = s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
            [((units.len() / 2) as u32).to_le_bytes().to_vec(), units].concat()
        };
        let [r1, r2] = rows;
        let [c1, c2] = cols;
        let mut body: Vec<u8> = [r1, r2, c1, c2].iter().flat_map(|v| v.to_le_bytes()).collect();
        body.extend([0u8; 16]);
        body.extend(2u32.to_le_bytes());
        body.extend(if display.is_some() { 0x17u32 } else { 0x03 }.to_le_bytes());
        if let Some(name) = display {
            body.extend(utf16(name));
        }
        body.extend([
            0xE0, 0xC9, 0xEA, 0x79, 0xF9, 0xBA, 0xCE, 0x11, 0x8C, 0x82, 0x00, 0xAA, 0x00, 0x4B,
            0xA9, 0x0B,
        ]);
        let units: Vec<u8> = url.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        body.extend((units.len() as u32).to_le_bytes());
        body.extend(units);
        rec(HLINK, &body)
    }

    #[test]
    fn a_link_reads_its_target_past_its_display_name() {
        let mut records = label(0, 1, 0, "www.example.org");
        records.extend(label(1, 2, 0, "see here"));
        let target = "http://www.example.org/";
        records.extend(hlink_over([1, 0], [1, 1], Some("www.example.org"), target));
        records.extend(hlink_over([2, 1], [3, 2], Some("see here"), target));
        assert_eq!(
            diagnose(&one_sheet(records)),
            [in_unit(DiagnosticCode::LinkTargetUnread, 1, 1)]
        );
    }

    #[test]
    fn a_header_footer_record_reports_each_of_its_texts() {
        let header_footer = |set: usize| {
            let mut body = vec![0u8; 38];
            body[28] = 0x0D;
            for (i, at) in [30, 32, 34, 36].into_iter().enumerate().take(set) {
                body[at] = 5 + i as u8;
            }
            rec(HEADER_FOOTER, &body)
        };
        let records: Vec<u8> = (0..=4).flat_map(header_footer).collect();
        assert_eq!(
            diagnose(&one_sheet(records)),
            [
                in_unit(DiagnosticCode::HeaderOmitted, 1, 6),
                in_unit(DiagnosticCode::FooterOmitted, 1, 4),
            ]
        );
    }

    #[test]
    fn a_shape_reports_only_shown_text() {
        let mut records = label(0, 0, 0, "x");
        records.extend(msodrawing(0, 1, false));
        records.extend(obj(0x02, 0));
        records.extend(client_textbox());
        records.extend(txo("Total"));
        records.extend(msodrawing(0, 2, false));
        records.extend(obj(0x06, 0));
        records.extend(client_textbox());
        records.extend(txo(""));
        records.extend(msodrawing(0, 3, true));
        records.extend(obj(0x03, 0));
        records.extend(txo("hidden"));
        records.extend(msodrawing(0, 4, false));
        records.extend(obj(0x1E, 0));
        records.extend(txo("  "));
        for (col, ot) in [(5, 0x03), (6, 0x06), (7, 0x09), (8, 0x1E)] {
            records.extend(msodrawing(0, col, false));
            records.extend(obj(ot, 0));
            records.extend(client_textbox());
            records.extend(txo("Note"));
        }
        assert_eq!(diagnose(&one_sheet(records)), [in_unit(DiagnosticCode::GraphicUnread, 1, 5)]);
    }

    #[test]
    fn a_format_code_that_does_not_decode_reports_where_it_shows() {
        let code: Vec<u16> =
            "0\" ".encode_utf16().chain([0xD800]).chain("\"".encode_utf16()).collect();
        let mut format = 164u16.to_le_bytes().to_vec();
        format.extend((code.len() as u16).to_le_bytes());
        format.push(1);
        format.extend(code.iter().flat_map(|u| u.to_le_bytes()));
        let mut row = vec![0u8; 16];
        row[..2].copy_from_slice(&1u16.to_le_bytes());
        row[12] = 0x20;
        let mut records = number(0, 0, 1, 2.0);
        records.extend(number(0, 1, 0, 3.0));
        records.extend(label(0, 2, 1, "text"));
        records.extend(rec(LABEL, &[cell6(0, 3, 1), vec![1, 0, 1, 0x00, 0xDC]].concat()));
        records.extend(rec(ROW, &row));
        records.extend(number(1, 0, 1, 4.0));
        let wb = Wb {
            xfs: vec![0, 164],
            globals: rec(FORMAT, &format),
            sheets: vec![("S", 0, records)],
            ..Wb::default()
        };
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["2 \u{FFFD}", "3", "text", "\u{FFFD}"]]);
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::InvalidEncoding, 1, 2),
                in_unit(DiagnosticCode::HiddenOmitted, 1, 1),
            ]
        );
    }

    #[test]
    fn a_format_code_past_the_binary_cap_falls_back_by_what_it_shows() {
        let mut records = number(0, 0, 1, 2.0);
        records.extend(number(0, 1, 2, 2.0));
        let text = format!("0\"{}\"", "x".repeat(253));
        let wb = Wb {
            globals: [format_record(164, &"0".repeat(256)), format_record(165, &text)].concat(),
            xfs: vec![0, 164, 165],
            sheets: vec![("S", 0, records)],
            ..Wb::default()
        };
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["2", "2"]]);
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::NumberFormatUnread, 1, 1),
                in_unit(DiagnosticCode::NumberFormatFallback, 1, 1),
            ]
        );
    }

    #[test]
    fn a_number_under_a_code_past_the_binary_cap_is_judged_by_its_numeric_sections() {
        let x = "x".repeat(260);
        let codes = [
            format!("0\"{x}\";0;0;@"),
            format!("0;0;0;\"{x}\"@"),
            format!("{};\"a\"@", "0".repeat(300)),
        ];
        let mut records = number(0, 0, 1, 2.0);
        records.extend(number(0, 1, 2, 2.0));
        records.extend(number(0, 2, 3, 2.0));
        let wb = Wb {
            globals: [
                format_record(164, &codes[0]),
                format_record(165, &codes[1]),
                format_record(166, &codes[2]),
            ]
            .concat(),
            xfs: vec![0, 164, 165, 166],
            sheets: vec![("S", 0, records)],
            ..Wb::default()
        };
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["2", "2", "2"]]);
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::NumberFormatUnread, 1, 1),
                in_unit(DiagnosticCode::NumberFormatFallback, 1, 2),
            ]
        );
    }

    #[test]
    fn a_text_section_past_the_binary_cap_reports_only_what_it_decorates() {
        let decorated = format!("0;0;0;\"{}\"@", "x".repeat(247));
        let plain = format!("{};@", "0".repeat(254));
        let mut records = label(0, 0, 1, "t");
        records.extend(label(0, 1, 2, "t"));
        let wb = Wb {
            globals: [format_record(164, &decorated), format_record(165, &plain)].concat(),
            xfs: vec![0, 164, 165],
            sheets: vec![("S", 0, records)],
            ..Wb::default()
        };
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["t", "t"]]);
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::NumberFormatUnread, 1, 1)]);
    }

    #[test]
    fn a_replaced_string_under_an_unread_text_section_reports_both() {
        let decorated = format!("0;0;0;\"{}\"@", "x".repeat(247));
        let mut records =
            rec(LABEL, &[cell6(0, 0, 1), vec![2, 0, 1, 0x74, 0x00, 0x00, 0xD8]].concat());
        records.extend(labelsst(0, 1, 1, 0));
        let mut sst = 1u32.to_le_bytes().to_vec();
        sst.extend(1u32.to_le_bytes());
        sst.extend([2, 0, 1, 0x74, 0x00, 0x00, 0xD8]);
        let wb = Wb {
            globals: format_record(164, &decorated),
            xfs: vec![0, 164],
            sst: vec![rec(SST, &sst)],
            sheets: vec![("S", 0, records)],
            ..Wb::default()
        };
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["t\u{FFFD}"; 2]]);
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::InvalidEncoding, 1, 2),
                in_unit(DiagnosticCode::NumberFormatUnread, 1, 2),
            ]
        );
    }

    #[test]
    fn a_workbook_listing_no_sheet_is_malformed() {
        let wb = Wb { xfs: vec![0], ..Wb::default() };
        let error = parse(&ParseCtx::default(), &wb.build()).unwrap_err();
        assert!(matches!(error, ConvertError::Malformed { .. }), "{error:?}");
    }

    #[test]
    fn globals_without_their_eof_report_the_stream_truncated() {
        let wb = Wb { globals_unended: true, ..one_sheet(label(0, 0, 0, "a")) };
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["a"]]);
        assert_eq!(
            reported(parse_ctx),
            [(DiagnosticCode::TruncatedRecords, Region::Body, None, 1)]
        );
        let cut = Wb { cut: 1, ..wb };
        assert_eq!(diagnose(&cut), [(DiagnosticCode::TruncatedRecords, Region::Body, None, 1)]);
        assert_eq!(diagnose(&one_sheet(label(0, 0, 0, "a"))), []);
    }

    #[test]
    fn a_link_covers_its_whole_range() {
        let mut records = label(0, 0, 0, "see here");
        let mut link = hlink_record(0, 0, "https://example.org/paper");
        link[4 + 2..4 + 4].copy_from_slice(&2u16.to_le_bytes());
        link[4 + 6..4 + 8].copy_from_slice(&1u16.to_le_bytes());
        records.extend(link);
        let mut row = vec![0u8; 16];
        row[12] = 0x20;
        records.extend(rec(ROW, &row));
        records.extend(label(1, 0, 0, "https://example.org/paper"));
        assert_eq!(diagnose(&one_sheet(records)), [in_unit(DiagnosticCode::HiddenOmitted, 1, 1)]);
    }

    #[test]
    fn a_file_moniker_names_its_path() {
        let mut data = vec![0x03, 0x03, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];
        data.extend(0u16.to_le_bytes());
        data.extend(9u32.to_le_bytes());
        data.extend(b"data.csv\0");
        assert_eq!(moniker_target(&data).as_deref(), Some("data.csv"));
        assert_eq!(moniker_target(&[0u8; 16]), None);
    }

    #[test]
    fn a_string_says_whether_it_decoded() {
        let bytes = [2u8, 0xAA, 0xAB];
        let mut r = SegReader::new(vec![&bytes]);
        let decoded = read_byte_string(&mut r, true, encoding_rs::WINDOWS_1253);
        assert_eq!(decoded, Some(("\u{FFFD}\u{AB}".to_string(), true)));
        let bytes = [2u8, b'o', b'k'];
        let mut r = SegReader::new(vec![&bytes]);
        let decoded = read_byte_string(&mut r, true, encoding_rs::WINDOWS_1253);
        assert_eq!(decoded, Some(("ok".to_string(), false)));
        let base = [vec![0u8; 10], 2u16.to_le_bytes().to_vec(), vec![0u8; 6]].concat();
        let chars = [0x01, 0x00, 0xD8, 0x00, 0xDC];
        assert_eq!(txo_text(&[&base, &chars]), Some(("\u{10000}".to_string(), false)));
    }
}
