//! In-house legacy Excel reader (.xls): OLE2 compound file holding a BIFF
//! record stream ([MS-XLS]). BIFF8 is the target; BIFF5/BIFF7 streams
//! degrade to their byte-string record layouts instead of erroring. Cell
//! values resolve their number format through the same engine and shared
//! grid as the SpreadsheetML reader, so a workbook saved as .xls and as
//! .xlsx converts identically.

use super::controls::Checkbox;
use super::grid::{
    CellFormat, CellRange, CellRef, CellXfs, DateSystem, GridBounds, LinkTarget, SheetContent,
    SheetLink, UnreadFormats, build_table, format_as_text, render_number, resolve_format,
};
use super::numfmt::NumberFormat;
use super::{error_literal, rk_number};
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{Block, Document, Inline, UnitKind};
use crate::shared::binary::{
    ByteReader, decode_utf16, get_u16, get_u32, read_ole_stream, utf16le_units,
};
use crate::shared::codepage::{self, Codepage, TextLoss};
use crate::shared::officeart;
use crate::shared::text::clean_text;
use std::collections::{BTreeSet, HashMap};
use std::io::Cursor;
use std::ops::RangeInclusive;
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

// [MS-XLS] 2.4.126: the ifmt a FORMAT record can declare.
const FORMAT_RECORD_IDS: &[RangeInclusive<u32>] = &[5..=8, 23..=26, 41..=44, 63..=66, 164..=392];

/// BOF `dt` value for a worksheet (or dialog sheet) substream.
const WORKSHEET_SUBSTREAM: u16 = 0x0010;

/// The BIFF8 grid is 256 columns; a larger column index is not a real cell.
const BIFF8_COLS: u32 = 256;

/// [MS-XLS] Rw: a BIFF8 row index is two bytes, so a sheet has 65,536 rows.
pub(super) const BIFF8_GRID: GridBounds = GridBounds { rows: 65_536, cols: BIFF8_COLS };

pub(super) fn parse(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Document, ConvertError> {
    let mut ole = cfb::CompoundFile::open(Cursor::new(bytes))
        .map_err(|_| ConvertError::malformed("not an OLE2 compound file"))?;
    let data = workbook_stream(parse_ctx, &mut ole)?;
    let mut records = RecordCount::new(parse_ctx);
    let globals = read_globals(&data, &mut records)?;

    let shown = globals.sheets.iter().filter(|s| matches!(s, Listed::Shown(_))).count();
    let mut doc = Document::default();
    let mut failed = 0usize;
    for (position, listed) in globals.sheets.iter().enumerate() {
        let unit = parse_ctx.begin_unit(UnitKind::Sheet, position)?;
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
        doc.blocks.push(Block::UnitStart(unit.unit().model()));
        let Some(table) = build_table(parse_ctx, content)? else {
            continue;
        };
        if shown > 1 {
            if sheet.name_loss.invalid {
                parse_ctx.emit(DiagnosticCode::InvalidEncoding, Region::Body);
            }
            if sheet.name_loss.assumed {
                parse_ctx.emit(DiagnosticCode::EncodingAssumed, Region::Body);
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
fn workbook_stream(
    parse_ctx: &ParseCtx<'_>,
    ole: &mut cfb::CompoundFile<Cursor<&[u8]>>,
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

type Record<'a> = (u16, &'a [u8], usize);

/// The record at `pos`: type, payload, and the position after it. `None` on
/// a truncated header or payload.
fn record_at(data: &[u8], pos: usize) -> Option<Record<'_>> {
    let rec_type = get_u16(data, pos)?;
    let len = get_u16(data, pos.checked_add(2)?)? as usize;
    let body_at = pos.checked_add(4)?;
    let body = data.get(body_at..body_at.checked_add(len)?)?;
    Some((rec_type, body, body_at + len))
}

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
        Self { parse_ctx, seen: 0, max: parse_ctx.limits().max_records, cut_at: None }
    }

    /// Reports a substream that stopped at `pos` without its EOF, unless
    /// `next` already reported the cut it stopped at.
    fn report_unended(&self, pos: usize) {
        if self.cut_at != Some(pos) {
            self.parse_ctx.emit(DiagnosticCode::TruncatedRecords, Region::Body);
        }
    }

    /// `record_at` charging the stream-wide record budget.
    fn next<'a>(&mut self, data: &'a [u8], pos: usize) -> Result<Option<Record<'a>>, ConvertError> {
        let Some(rec) = record_at(data, pos) else {
            // Trailing bytes too short to form a record: the stream was cut, so
            // whatever was read is partial rather than complete.
            if pos < data.len() && self.cut_at.replace(pos) != Some(pos) {
                self.parse_ctx.emit(DiagnosticCode::TruncatedRecords, Region::Body);
            }
            return Ok(None);
        };
        self.charge()?;
        Ok(Some(rec))
    }

    fn charge(&mut self) -> Result<(), ConvertError> {
        self.seen += 1;
        if self.seen > self.max {
            return Err(ConvertError::limit(Limit::Records, self.max));
        }
        Ok(())
    }
}

/// The walk over one substream's own records: nested substreams are skipped,
/// and it stops at the substream's EOF or where another substream starts.
struct Substream {
    pos: usize,
    depth: usize,
    ended: bool,
}

impl Substream {
    fn after_bof(pos: usize) -> Self {
        Self { pos, depth: 1, ended: false }
    }

    fn next<'a>(
        &mut self,
        data: &'a [u8],
        starts: &BTreeSet<usize>,
        records: &mut RecordCount<'_>,
    ) -> Result<Option<(u16, &'a [u8])>, ConvertError> {
        while !starts.contains(&self.pos)
            && let Some((rec_type, body, next)) = records.next(data, self.pos)?
        {
            self.pos = next;
            match rec_type {
                BOF => self.depth += 1,
                EOF_REC if self.depth == 1 => {
                    self.ended = true;
                    break;
                }
                EOF_REC => self.depth -= 1,
                _ if self.depth == 1 => return Ok(Some((rec_type, body))),
                _ => {}
            }
        }
        Ok(None)
    }

    /// `body`, the record `next` just returned, with the bodies of the CONTINUE records after
    /// it up to where another substream starts; the walk charges each one and moves past it.
    fn continued<'a>(
        &mut self,
        data: &'a [u8],
        body: &'a [u8],
        starts: &BTreeSet<usize>,
        records: &mut RecordCount<'_>,
    ) -> Result<Vec<&'a [u8]>, ConvertError> {
        let mut segs = vec![body];
        while !starts.contains(&self.pos)
            && let Some((CONTINUE, cont, next)) = record_at(data, self.pos)
        {
            records.charge()?;
            segs.push(cont);
            self.pos = next;
        }
        Ok(segs)
    }

    fn finish(self, records: &RecordCount<'_>) {
        if !self.ended {
            records.report_unended(self.pos);
        }
    }
}

/// Reader over a record's segments (base body plus CONTINUE bodies). Fixed
/// fields never straddle a segment boundary, but may start exactly on one;
/// only string character data crosses boundaries, and the string readers
/// handle the repeated option-flags byte themselves.
struct SegReader<'a> {
    later: std::slice::Iter<'a, &'a [u8]>,
    current: ByteReader<'a>,
}

impl<'a> SegReader<'a> {
    fn new(segs: &'a [&'a [u8]]) -> Self {
        let mut later = segs.iter();
        let current = ByteReader::new(later.next().copied().unwrap_or_default());
        Self { later, current }
    }

    /// Bytes left in the current segment.
    fn in_seg(&self) -> usize {
        self.current.rest().len()
    }

    /// Hop over exhausted segments so a field can start at a boundary.
    fn normalize(&mut self) {
        while self.in_seg() == 0
            && let Some(&next) = self.later.next()
        {
            self.current = ByteReader::new(next);
        }
    }

    /// Move to the start of the next segment; `None` when there is none.
    fn next_seg(&mut self) -> Option<()> {
        self.current = ByteReader::new(self.later.next()?);
        Some(())
    }

    /// `n` bytes from within one segment.
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        self.normalize();
        self.current.take(n)
    }

    fn u8(&mut self) -> Option<u8> {
        self.normalize();
        self.current.u8()
    }

    fn u16(&mut self) -> Option<u16> {
        self.normalize();
        self.current.u16()
    }

    fn u32(&mut self) -> Option<u32> {
        self.normalize();
        self.current.u32()
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
            self.current.skip(step)?;
            n -= step;
        }
        Some(())
    }
}

/// The width of a string's character count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LengthPrefix {
    U8,
    U16,
}

impl LengthPrefix {
    fn read(self, r: &mut SegReader<'_>) -> Option<usize> {
        match self {
            Self::U8 => r.u8().map(usize::from),
            Self::U16 => r.u16().map(usize::from),
        }
    }
}

/// Whether a BIFF8 string may carry formatting runs and phonetic data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runs {
    Plain,
    Rich,
}

/// A BIFF8 Unicode string: XLUnicodeString, ShortXLUnicodeString (`U8`),
/// or XLUnicodeRichExtendedString (`Rich`). Character data may continue
/// into following segments; at every such boundary the option-flags byte is
/// repeated and the encoding can switch between 8-bit compressed and 16-bit
/// UTF-16, so it is re-read rather than carried over. Rich runs and
/// phonetic data are skipped, matching the xlsx reader's handling of `rPh`.
/// The text comes with whether a unit did not decode.
fn read_biff8_string(
    r: &mut SegReader<'_>,
    length: LengthPrefix,
    runs: Runs,
) -> Option<(String, bool)> {
    let cch = length.read(r)?;
    let flags = r.u8()?;
    let mut wide = flags & 0x01 != 0;
    let rich = runs == Runs::Rich;
    let runs = if rich && flags & 0x08 != 0 { usize::from(r.u16()?) } else { 0 };
    let ext = if rich && flags & 0x04 != 0 { usize::try_from(r.u32()?).ok()? } else { 0 };
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
    let _: Option<()> = r.skip(runs * 4).and_then(|()| r.skip(ext));
    Some(decode_utf16(units))
}

/// A BIFF5/BIFF7 byte string (no flags byte), decoded per the workbook's
/// CODEPAGE record, with what the text lost.
fn read_byte_string(
    r: &mut SegReader<'_>,
    length: LengthPrefix,
    codepage: Codepage,
) -> Option<(String, TextLoss)> {
    let cch = length.read(r)?;
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
    Some(codepage.decode(&bytes))
}

/// The code page a CODEPAGE record names; BIFF5 writes 32768 for Mac Roman
/// and 32769 for windows-1252.
fn workbook_codepage(cp: u16) -> Codepage {
    codepage::windows(match cp {
        32768 => 10000,
        32769 => 1252,
        cp => cp,
    })
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
    name_loss: TextLoss,
    /// Absolute stream offset of the sheet substream's BOF record.
    offset: usize,
}

struct Globals {
    biff8: bool,
    dates: DateSystem,
    encoding: Codepage,
    sst: Vec<(String, TextLoss)>,
    /// The XF table in record order; a cell's ixfe indexes it directly.
    xfs: CellXfs,
    /// Whether each XF's format code was decoded in a guessed code page.
    assumed_xfs: Vec<bool>,
    sheets: Vec<Listed>,
    /// Where each BOUNDSHEET says its substream starts, hidden sheets and
    /// VBA modules included: a substream without its EOF ends where another
    /// starts.
    starts: BTreeSet<usize>,
}

impl Globals {
    fn new(biff8: bool) -> Self {
        Self {
            biff8,
            dates: DateSystem::Since1900,
            encoding: codepage::windows(1252),
            sst: Vec::new(),
            xfs: CellXfs::new(Vec::new()),
            assumed_xfs: Vec::new(),
            sheets: Vec::new(),
            starts: BTreeSet::new(),
        }
    }

    fn format(&self, ixfe: u16) -> CellFormat {
        self.xfs.get(usize::from(ixfe))
    }

    /// Whether the format at `ixfe` was decoded in a guessed code page and
    /// `shows` finds a non-ASCII literal in the section a value selects.
    fn format_assumed(&self, ixfe: u16, shows: impl FnOnce(&NumberFormat) -> bool) -> bool {
        self.assumed_xfs.get(usize::from(ixfe)).is_some_and(|&assumed| assumed)
            && matches!(self.format(ixfe), CellFormat::Fmt(format) if shows(&format))
    }

    fn read_string(
        &self,
        r: &mut SegReader<'_>,
        length: LengthPrefix,
    ) -> Option<(String, TextLoss)> {
        if self.biff8 {
            let (text, invalid) = read_biff8_string(r, length, Runs::Plain)?;
            Some((text, TextLoss { invalid, assumed: false }))
        } else {
            read_byte_string(r, length, self.encoding)
        }
    }

    fn list_sheet(&mut self, body: &[u8]) {
        if let Some(start) = get_u32(body, 0) {
            self.starts.insert(start as usize);
        }
        if let Some(sheet) = read_boundsheet(body, self) {
            self.sheets.push(sheet);
        }
    }

    /// A FORMAT record's id and code, else the id when only the code does not read.
    fn format_code(&self, body: &[u8]) -> Result<(u32, (String, TextLoss)), Option<u32>> {
        let mut body = ByteReader::new(body);
        let id = body.u16().map(u32::from).ok_or(None)?;
        let segs = [body.rest()];
        let length = if self.biff8 { LengthPrefix::U16 } else { LengthPrefix::U8 };
        let code = self.read_string(&mut SegReader::new(&segs), length);
        code.map(|code| (id, code)).ok_or(Some(id))
    }
}

/// The workbook globals substream: SST, FORMAT/XF tables, sheet directory,
/// date system, encryption marker. A workbook listing no sheet is
/// `Malformed`; globals that end without their EOF were cut short.
fn read_globals(data: &[u8], records: &mut RecordCount<'_>) -> Result<Globals, ConvertError> {
    let Some((rec_type, body, pos)) = record_at(data, 0) else {
        return Err(ConvertError::malformed("empty workbook stream"));
    };
    if rec_type != BOF {
        return Err(ConvertError::malformed("workbook stream does not start with a BOF record"));
    }
    let mut globals = Globals::new(get_u16(body, 0) == Some(0x0600));
    let mut formats: HashMap<u32, (String, TextLoss)> = HashMap::new();
    let mut unread = UnreadFormats::declaring(FORMAT_RECORD_IDS);
    let mut xf_ifmts: Vec<Option<u16>> = Vec::new();
    let mut walk = Substream::after_bof(pos);
    while let Some((rec_type, body)) = walk.next(data, &globals.starts, records)? {
        match rec_type {
            FILEPASS => return Err(ConvertError::Encrypted),
            CODEPAGE => {
                if let Some(cp) = get_u16(body, 0) {
                    globals.encoding = workbook_codepage(cp);
                }
            }
            DATEMODE => globals.dates = DateSystem::from_date1904(get_u16(body, 0) == Some(1)),
            BOUNDSHEET => globals.list_sheet(body),
            FORMAT => match globals.format_code(body) {
                Ok((id, code)) => {
                    formats.insert(id, code);
                }
                Err(Some(id)) => unread.note(id),
                Err(None) => unread.note_anonymous(None),
            },
            XF => {
                // ixfe is 16-bit, so the table never usefully exceeds it.
                if u16::try_from(xf_ifmts.len()).is_ok() {
                    xf_ifmts.push(get_u16(body, 2));
                }
            }
            SST if globals.biff8 => {
                globals.sst = read_sst(&walk.continued(data, body, &globals.starts, records)?);
            }
            _ => {}
        }
    }
    if globals.sheets.is_empty() {
        return Err(ConvertError::malformed("the workbook lists no sheet"));
    }
    walk.finish(records);
    let (xfs, assumed_xfs) = resolve_xfs(&formats, &unread, &xf_ifmts);
    Ok(Globals { xfs, assumed_xfs, ..globals })
}

/// The XF table, resolved after the pass because FORMAT records are not
/// ordered relative to the XFs that reference them.
fn resolve_xfs(
    formats: &HashMap<u32, (String, TextLoss)>,
    unread: &UnreadFormats,
    xf_ifmts: &[Option<u16>],
) -> (CellXfs, Vec<bool>) {
    let mut cache: HashMap<u16, CellFormat> = HashMap::new();
    let (xfs, assumed_xfs) = xf_ifmts
        .iter()
        .map(|&ifmt| {
            let Some(ifmt) = ifmt else {
                return ((CellFormat::unread(), false), false);
            };
            let id = u32::from(ifmt);
            let entry = formats.get(&id);
            let format = cache.entry(ifmt).or_insert_with(|| {
                resolve_format(id, entry.map(|(code, _)| code.as_str()), unread)
            });
            let loss = entry.map_or_else(TextLoss::default, |&(_, loss)| loss);
            ((format.clone(), loss.invalid), loss.assumed)
        })
        .unzip();
    (CellXfs::new(xfs), assumed_xfs)
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
        let segs = [body.get(6..)?];
        let (name, name_loss) =
            globals.read_string(&mut SegReader::new(&segs), LengthPrefix::U8)?;
        Some(Listed::Shown(BoundSheet { name: clean_text(&name), name_loss, offset }))
    };
    Some(read().unwrap_or(Listed::Unreadable))
}

/// The shared string table (trap 1: strings routinely split across
/// CONTINUE records, mid-character-data). A malformed tail keeps the
/// entries read so far; LABELSST lookups past them stay empty and report
/// the cell as unreadable.
fn read_sst(segs: &[&[u8]]) -> Vec<(String, TextLoss)> {
    let mut r = SegReader::new(segs);
    let (Some(_total), Some(unique)) = (r.u32(), r.u32()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while out.len() < unique as usize {
        let Some((text, invalid)) = read_biff8_string(&mut r, LengthPrefix::U16, Runs::Rich) else {
            break;
        };
        out.push((clean_text(&text), TextLoss { invalid, assumed: false }));
    }
    out
}

/// A cell record's leading cell, which may lie past the grid, its ixfe, and
/// the reader past that header.
fn cell_ref(body: &[u8]) -> Option<(CellRef, u16, ByteReader<'_>)> {
    let mut header = ByteReader::new(body);
    let row = u32::from(header.u16()?);
    let col = u32::from(header.u16()?);
    let ixfe = header.u16()?;
    Some((CellRef::new(row, col), ixfe, header))
}

/// One worksheet substream into the shared `SheetContent` shape. `Ok(None)`
/// means the substream is missing or not a worksheet (chart or macro
/// sheets), which the caller counts as unreadable.
fn read_sheet(
    data: &[u8],
    globals: &Globals,
    offset: usize,
    records: &mut RecordCount<'_>,
) -> Result<Option<SheetContent>, ConvertError> {
    let Some((rec_type, body, pos)) = record_at(data, offset) else {
        return Ok(None);
    };
    if rec_type != BOF || get_u16(body, 2) != Some(WORKSHEET_SUBSTREAM) {
        return Ok(None);
    }
    let mut sheet = SheetReader::new(records.parse_ctx, globals);
    let mut walk = Substream::after_bof(pos);
    while let Some((rec_type, body)) = walk.next(data, &globals.starts, records)? {
        match rec_type {
            ROW => sheet.row(body),
            COLINFO => sheet.colinfo(body),
            MERGEDCELLS => sheet.merged(body),
            LABELSST => sheet.label_sst(body)?,
            NUMBER => sheet.number(body)?,
            RK => sheet.rk(body)?,
            MULRK => sheet.mulrk(body)?,
            BOOLERR => sheet.bool_err(body)?,
            FORMULA => sheet.formula(body)?,
            OBJ if globals.biff8 => sheet.object(body)?,
            NOTE => records.parse_ctx.emit(DiagnosticCode::CommentOmitted, Region::Comment),
            HEADER | FOOTER | HEADER_FOOTER => sheet.header_footer(rec_type, body),
            HLINK => sheet.link(body),
            LABEL | RSTRING | TXO | STRING => {
                let segs = walk.continued(data, body, &globals.starts, records)?;
                match rec_type {
                    LABEL | RSTRING => sheet.label(body, &segs)?,
                    STRING => sheet.string(&segs)?,
                    _ => sheet.text_object(&segs)?,
                }
            }
            MSODRAWING if globals.biff8 => {
                let segs = walk.continued(data, body, &globals.starts, records)?;
                sheet.drawing(body, &segs);
            }
            _ => {}
        }
    }
    walk.finish(records);
    Ok(Some(sheet.out))
}

/// The walk state of one worksheet substream, with one method per record family.
struct SheetReader<'g, 'p> {
    parse_ctx: &'p ParseCtx<'p>,
    globals: &'g Globals,
    out: SheetContent,
    /// A FORMULA whose cached value is a string: its cell and ixfe waiting
    /// for the STRING record that carries the text.
    pending: Option<(CellRef, u16)>,
    /// A drawing object's shape arrives in MSODRAWING ahead of its OBJ, and
    /// its text in the TXO after it.
    shape: Option<Shape>,
    text_of: Option<TextOf>,
}

impl<'g, 'p> SheetReader<'g, 'p> {
    fn new(parse_ctx: &'p ParseCtx<'p>, globals: &'g Globals) -> Self {
        let out = SheetContent::on(
            BIFF8_GRID,
            parse_ctx.grid_slots_left(),
            parse_ctx.limits().max_grid_slots,
        );
        Self { parse_ctx, globals, out, pending: None, shape: None, text_of: None }
    }

    fn row(&mut self, body: &[u8]) {
        if body.get(12).is_some_and(|flags| flags & 0x20 != 0)
            && let Some(row) = get_u16(body, 0)
        {
            self.out.hide_row(u32::from(row));
        }
    }

    fn colinfo(&mut self, body: &[u8]) {
        if let (Some(first), Some(last), Some(flags)) =
            (get_u16(body, 0), get_u16(body, 2), get_u16(body, 8))
            && flags & 0x01 != 0
            && first <= last
        {
            self.out.hide_cols(u32::from(first), u32::from(last));
        }
    }

    fn merged(&mut self, body: &[u8]) {
        for range in merged_ranges(body) {
            self.out.merge(range);
        }
    }

    fn label_sst(&mut self, body: &[u8]) -> Result<(), ConvertError> {
        if let Some((at, ixfe, mut rest)) = cell_ref(body)
            && let Some(isst) = rest.u32()
        {
            let text = usize::try_from(isst).ok().and_then(|i| self.globals.sst.get(i)).cloned();
            self.put_string(at, ixfe, text)?;
        }
        Ok(())
    }

    fn number(&mut self, body: &[u8]) -> Result<(), ConvertError> {
        if let Some((at, ixfe, mut rest)) = cell_ref(body)
            && let Some(n) = rest.f64()
        {
            self.put_number(at, ixfe, n)?;
        }
        Ok(())
    }

    fn rk(&mut self, body: &[u8]) -> Result<(), ConvertError> {
        if let Some((at, ixfe, mut rest)) = cell_ref(body)
            && let Some(rk) = rest.u32()
        {
            self.put_number(at, ixfe, rk_number(rk))?;
        }
        Ok(())
    }

    fn mulrk(&mut self, body: &[u8]) -> Result<(), ConvertError> {
        for (at, ixfe, rk) in mulrk_cells(body) {
            self.put_number(at, ixfe, rk_number(rk))?;
        }
        Ok(())
    }

    fn bool_err(&mut self, body: &[u8]) -> Result<(), ConvertError> {
        if let Some((at, _, mut rest)) = cell_ref(body)
            && let (Some(value), Some(is_err)) = (rest.u8(), rest.u8())
        {
            let text = match is_err {
                0 | 1 => bool_or_error(is_err == 1, value),
                _ => None,
            };
            self.put_literal(at, text)?;
        }
        Ok(())
    }

    fn formula(&mut self, body: &[u8]) -> Result<(), ConvertError> {
        let Some((at, ixfe, mut rest)) = cell_ref(body) else {
            return Ok(());
        };
        let Some(value) = rest.clone().array::<8>() else {
            return Ok(());
        };
        if value[6] == 0xFF && value[7] == 0xFF {
            match value[0] {
                0x00 => self.pending = Some((at, ixfe)),
                0x01 | 0x02 => self.put_literal(at, bool_or_error(value[0] == 0x02, value[2]))?,
                // 0x03 is a blank string result.
                _ => {}
            }
        } else if let Some(n) = rest.f64() {
            self.put_number(at, ixfe, n)?;
        }
        Ok(())
    }

    fn label(&mut self, body: &[u8], segs: &[&[u8]]) -> Result<(), ConvertError> {
        if let Some((at, ixfe, _)) = cell_ref(body) {
            let mut r = SegReader::new(segs);
            // RSTRING's trailing rich runs are formatting, not text.
            let text = r.skip(6).and_then(|()| self.globals.read_string(&mut r, LengthPrefix::U16));
            self.put_string(at, ixfe, text)?;
        }
        Ok(())
    }

    fn string(&mut self, segs: &[&[u8]]) -> Result<(), ConvertError> {
        if let Some((at, ixfe)) = self.pending.take() {
            let text = self.globals.read_string(&mut SegReader::new(segs), LengthPrefix::U16);
            self.put_string(at, ixfe, text)?;
        }
        Ok(())
    }

    fn link(&mut self, body: &[u8]) {
        match hlink(body) {
            Some(HlinkRecord::External(link)) => self.out.links.push(link),
            Some(HlinkRecord::InWorkbook) => {}
            None => self.parse_ctx.emit(DiagnosticCode::LinkTargetUnread, Region::Body),
        }
    }

    /// An MSODRAWING: the shape the next OBJ draws, and whether a text box stays open.
    fn drawing(&mut self, body: &[u8], segs: &[&[u8]]) {
        self.shape = if segs.len() == 1 { last_shape(body) } else { last_shape(&segs.concat()) };
        if !opens_text_box(body) {
            self.text_of = None;
        }
    }

    /// An OBJ: a checkbox, a form control, a picture, a chart or a shape.
    fn object(&mut self, body: &[u8]) -> Result<(), ConvertError> {
        self.text_of = None;
        let shape = self.shape.take();
        let hidden = shape.as_ref().is_some_and(|s| s.hidden);
        let anchor = shape.map(|s| s.anchor).filter(|at| at.col < BIFF8_COLS);
        let parse_ctx = self.parse_ctx;
        match (obj_kind(body), anchor) {
            (Some(Obj::Checkbox(_) | Obj::Control), _) if hidden => {
                parse_ctx.emit(DiagnosticCode::HiddenOmitted, Region::Body);
            }
            (Some(Obj::Checkbox(Some(checked))), Some(at)) => {
                let checkbox = Checkbox { checked, caption: String::new() };
                self.out.checkboxes.entry(at).or_default().push(checkbox);
                self.text_of = Some(TextOf::Checkbox(at));
            }
            (Some(Obj::Checkbox(None)), Some(at)) => {
                self.out.lose(at, DiagnosticCode::FormControlUnread)?;
            }
            (Some(Obj::Checkbox(_) | Obj::Control), _) => {
                parse_ctx.emit(DiagnosticCode::FormControlUnread, Region::Body);
            }
            (Some(Obj::Picture), _) if !hidden => {
                parse_ctx.image_loss(crate::ctx::ImageLoss::Unplaced, Region::Body);
            }
            (Some(Obj::Chart), _) if !hidden => {
                parse_ctx.emit(DiagnosticCode::GraphicUnread, Region::Body);
            }
            (Some(Obj::Shape), _) if !hidden => self.text_of = Some(TextOf::Shape),
            _ => {}
        }
        Ok(())
    }

    /// A TXO: a checkbox's caption, or a shown shape's text the grid does not hold.
    fn text_object(&mut self, segs: &[&[u8]]) -> Result<(), ConvertError> {
        match self.text_of.take() {
            Some(TextOf::Checkbox(at)) => {
                if let Some((caption, replaced)) = txo_text(segs)
                    && let Some(b) = self.out.checkboxes.get_mut(&at).and_then(|v| v.last_mut())
                {
                    b.caption = clean_text(&caption);
                    if replaced {
                        self.out.lose(at, DiagnosticCode::InvalidEncoding)?;
                    }
                }
            }
            Some(TextOf::Shape)
                if txo_text(segs).is_none_or(|(text, _)| !text.trim().is_empty()) =>
            {
                self.parse_ctx.emit(DiagnosticCode::GraphicUnread, Region::Body);
            }
            _ => {}
        }
        Ok(())
    }

    fn header_footer(&mut self, rec_type: u16, body: &[u8]) {
        for region in header_footer_regions(rec_type, body, self.globals.biff8) {
            self.out.omit_header_footer(region);
        }
    }

    fn put_number(&mut self, at: CellRef, ixfe: u16, n: f64) -> Result<(), ConvertError> {
        let rendered = render_number(&self.globals.format(ixfe), n, self.globals.dates);
        let assumed = self.globals.format_assumed(ixfe, |format| format.number_shows_non_ascii(n));
        self.put_rendered(at, ixfe, rendered, TextLoss { invalid: false, assumed })
    }

    fn put_string(
        &mut self,
        at: CellRef,
        ixfe: u16,
        text: Option<(String, TextLoss)>,
    ) -> Result<(), ConvertError> {
        let Some((text, mut loss)) = text else {
            return self.out.lose(at, DiagnosticCode::CellValueUnreadable);
        };
        let format = self.globals.format(ixfe);
        let rendered = format_as_text(&format, &clean_text(&text));
        if (loss.assumed || loss.invalid)
            && matches!(&format, CellFormat::Fmt(f) if !f.text_shows_value())
        {
            loss = TextLoss::default();
        }
        loss.assumed |= self.globals.format_assumed(ixfe, NumberFormat::text_shows_non_ascii);
        self.put_rendered(at, ixfe, rendered, loss)
    }

    fn put_rendered(
        &mut self,
        at: CellRef,
        ixfe: u16,
        (text, loss): (String, Option<DiagnosticCode>),
        decoded: TextLoss,
    ) -> Result<(), ConvertError> {
        let replaced = decoded.invalid || self.globals.xfs.replaced_in(usize::from(ixfe), &text);
        self.out.put(at, text)?;
        let decoding = [
            replaced.then_some(DiagnosticCode::InvalidEncoding),
            decoded.assumed.then_some(DiagnosticCode::EncodingAssumed),
        ];
        for code in loss.into_iter().chain(decoding.into_iter().flatten()) {
            self.out.lose(at, code)?;
        }
        Ok(())
    }

    fn put_literal(&mut self, at: CellRef, text: Option<&str>) -> Result<(), ConvertError> {
        let Some(text) = text else {
            return self.out.lose(at, DiagnosticCode::CellValueUnreadable);
        };
        self.out.put(at, text.to_string())?;
        Ok(())
    }
}

/// The merged ranges a MERGEDCELLS body lists, up to its count or its end.
fn merged_ranges(body: &[u8]) -> impl Iterator<Item = CellRange> + '_ {
    let count = usize::from(get_u16(body, 0).unwrap_or(0)).min(body.len().saturating_sub(2) / 8);
    (0..count).map_while(move |i| {
        let at = 2 + i * 8;
        let (r1, r2) = (get_u16(body, at)?, get_u16(body, at + 2)?);
        let (c1, c2) = (get_u16(body, at + 4)?, get_u16(body, at + 6)?);
        Some(CellRange::spanning(
            CellRef::new(u32::from(r1), u32::from(c1)),
            CellRef::new(u32::from(r2), u32::from(c2)),
        ))
    })
}

/// The cells of a MULRK body: rw, colFirst, then 6-byte (ixfe, RK) pairs; colLast is
/// redundant with the record length.
fn mulrk_cells(body: &[u8]) -> impl Iterator<Item = (CellRef, u16, u32)> + '_ {
    let start = get_u16(body, 0).zip(get_u16(body, 2));
    let pairs = if start.is_some() { body.len().saturating_sub(6) / 6 } else { 0 };
    let (row, col_first) = start.unwrap_or_default();
    (0..pairs).zip(u32::from(col_first)..).map_while(move |(i, col)| {
        let (ixfe, rk) = (get_u16(body, 4 + i * 6)?, get_u32(body, 6 + i * 6)?);
        Some((CellRef::new(u32::from(row), col), ixfe, rk))
    })
}

/// A BOOLERR or cached FORMULA result: `TRUE`, `FALSE` or an error literal; `None` for an
/// error code without one.
fn bool_or_error(is_err: bool, value: u8) -> Option<&'static str> {
    match (is_err, value) {
        (true, code) => error_literal(code),
        (false, 0) => Some("FALSE"),
        (false, _) => Some("TRUE"),
    }
}

/// The regions a HEADER, FOOTER or `HEADER_FOOTER` body holds a non-empty string for.
fn header_footer_regions(
    rec_type: u16,
    body: &[u8],
    biff8: bool,
) -> impl Iterator<Item = Region> + '_ {
    let fields: &[(usize, Region)] = match rec_type {
        HEADER => &[(0, Region::Header)],
        FOOTER => &[(0, Region::Footer)],
        // cchHeaderEven, cchFooterEven, cchHeaderFirst, cchFooterFirst
        _ => &[
            (30, Region::Header),
            (32, Region::Footer),
            (34, Region::Header),
            (36, Region::Footer),
        ],
    };
    let single = matches!(rec_type, HEADER | FOOTER);
    fields.iter().filter_map(move |&(at, region)| {
        let chars =
            if single && !biff8 { body.first().map(|&n| u16::from(n)) } else { get_u16(body, at) };
        chars.is_some_and(|n| n > 0).then_some(region)
    })
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
    officeart::record_at(body, 0).is_some_and(|record| record.rec_type == CLIENT_TEXTBOX)
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
    const CLIENT_ANCHOR: u16 = 0xF010;
    // Group shape boolean properties: fHidden with its use bit.
    const PID_GROUP_SHAPE: u16 = 0x03BF;
    const F_HIDDEN: u32 = 0x0000_0002;
    const F_USE_HIDDEN: u32 = 0x0002_0000;
    let mut anchor = None;
    let mut hidden = false;
    let mut off = 0usize;
    while let Some(record) = officeart::record_at(data, off) {
        let officeart::Record { rec_type, body, .. } = record;
        if record.version() == officeart::CONTAINER_VERSION {
            if rec_type == officeart::SP_CONTAINER {
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
            officeart::FOPT => {
                for (pid, op) in officeart::properties(record.instance(), body) {
                    if pid == PID_GROUP_SHAPE && op & F_USE_HIDDEN != 0 {
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
    let mut r = ByteReader::new(body);
    let mut ot = None;
    let mut state = None;
    let mut interface = false;
    while let (Some(ft), Some(cb)) = (r.u16(), r.u16()) {
        if ft == FT_END {
            break;
        }
        if ft == FT_LBS_DATA {
            interface = cb != 0 && interface_dropdown(r.rest());
            break;
        }
        let Some(data) = r.take(usize::from(cb)) else {
            break;
        };
        match ft {
            FT_CMO => ot = get_u16(data, 0),
            FT_CBLS_DATA => state = get_u16(data, 0),
            _ => {}
        }
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

enum HlinkRecord {
    /// A location within the workbook, which no target carries.
    InWorkbook,
    /// A moniker's target over the record's range, [`LinkTarget::Unknown`]
    /// when the moniker does not read.
    External(SheetLink),
}

/// An HLINK record, or `None` for one too short for its range or flags
/// ([MS-XLS] 2.4.140, [MS-OSHARED] 2.3.7.1).
fn hlink(body: &[u8]) -> Option<HlinkRecord> {
    const HAS_MONIKER: u32 = 0x01;
    let (r1, r2) = (u32::from(get_u16(body, 0)?), u32::from(get_u16(body, 2)?));
    let (c1, c2) = (u32::from(get_u16(body, 4)?), u32::from(get_u16(body, 6)?));
    let flags = get_u32(body, 28)?;
    if flags & HAS_MONIKER == 0 {
        return Some(HlinkRecord::InWorkbook);
    }
    let target = body.get(32..).and_then(|data| hlink_target(data, flags));
    let range = CellRange::spanning(CellRef::new(r1, c1), CellRef::new(r2, c2));
    let target = target.map_or(LinkTarget::Unknown, |url| LinkTarget::Url(Rc::from(url)));
    Some(HlinkRecord::External(SheetLink { range, target }))
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
            units.extend(utf16le_units(chars).take(take));
            remaining -= take;
        } else {
            let take = chars.len().min(remaining);
            units.extend(chars.iter().take(take).map(|&b| u16::from(b)));
            remaining -= take;
        }
    }
    Some(decode_utf16(units))
}

#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::unreadable_literal,
    reason = "fixture records take known small sizes in fixed-width fields and values as stored"
)]
mod tests {
    use super::super::test_support::{Reported, first_table, in_unit, origin, reported, texts};
    use super::*;
    use crate::shared::officeart::record;
    use crate::shared::test_support::unit_starts;
    use std::assert_matches;
    use std::io::Write;

    fn le16(words: &[u16]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    fn rec(rec_type: u16, body: &[u8]) -> Vec<u8> {
        [&le16(&[rec_type, body.len() as u16])[..], body].concat()
    }

    fn bof(dt: u16, vers: u16) -> Vec<u8> {
        rec(BOF, &[le16(&[vers, dt]), vec![0; 12]].concat())
    }

    /// XLUnicodeString, compressed ASCII.
    fn ustr(s: &str) -> Vec<u8> {
        [&le16(&[s.len() as u16])[..], &[0], s.as_bytes()].concat()
    }

    /// ShortXLUnicodeString, compressed ASCII.
    fn short_ustr(s: &str) -> Vec<u8> {
        [&[s.len() as u8, 0][..], s.as_bytes()].concat()
    }

    fn cell6(row: u16, col: u16, ixfe: u16) -> Vec<u8> {
        le16(&[row, col, ixfe])
    }

    fn cell_rec(rec_type: u16, row: u16, col: u16, ixfe: u16, payload: &[u8]) -> Vec<u8> {
        rec(rec_type, &[&cell6(row, col, ixfe)[..], payload].concat())
    }

    fn labelsst(row: u16, col: u16, ixfe: u16, isst: u32) -> Vec<u8> {
        cell_rec(LABELSST, row, col, ixfe, &isst.to_le_bytes())
    }

    fn label(row: u16, col: u16, ixfe: u16, s: &str) -> Vec<u8> {
        cell_rec(LABEL, row, col, ixfe, &ustr(s))
    }

    fn number(row: u16, col: u16, ixfe: u16, v: f64) -> Vec<u8> {
        cell_rec(NUMBER, row, col, ixfe, &v.to_le_bytes())
    }

    fn rk_cell(row: u16, col: u16, ixfe: u16, rk: u32) -> Vec<u8> {
        cell_rec(RK, row, col, ixfe, &rk.to_le_bytes())
    }

    fn formula(row: u16, col: u16, ixfe: u16, value: [u8; 8]) -> Vec<u8> {
        cell_rec(FORMULA, row, col, ixfe, &[&value[..], &[0; 6]].concat())
    }

    fn mulrk_rec(row: u16, first_col: u16, cells: &[(u16, u32)]) -> Vec<u8> {
        let runs =
            cells.iter().map(|&(ixfe, rk)| [le16(&[ixfe]), rk.to_le_bytes().to_vec()].concat());
        let last_col = first_col + cells.len() as u16 - 1;
        rec(MULRK, &[le16(&[row, first_col]), runs.flatten().collect(), le16(&[last_col])].concat())
    }

    fn xf(ifmt: u16) -> Vec<u8> {
        rec(XF, &[le16(&[0, ifmt]), vec![0; 16]].concat())
    }

    fn merged(ranges: &[[u16; 4]]) -> Vec<u8> {
        rec(MERGEDCELLS, &le16(&[&[ranges.len() as u16][..], &ranges.concat()].concat()))
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
        unended_sheets: Vec<usize>,
        module_sheet: Option<usize>,
        substreams_reversed: bool,
    }

    fn format_record(ifmt: u16, code: &str) -> Vec<u8> {
        rec(FORMAT, &[le16(&[ifmt]), ustr(code)].concat())
    }

    fn records_in(stream: &[u8]) -> u64 {
        let mut pos = 0;
        let mut count = 0;
        while let Some((_, _, next)) = record_at(stream, pos) {
            count += 1;
            pos = next;
        }
        count
    }

    fn parse_within(stream: &[u8], max_records: u64) -> Result<Document, ConvertError> {
        let limits = crate::limits::Limits { max_records, ..crate::limits::Limits::default() };
        parse(&ParseCtx::new(&limits), &ole_with("Workbook", stream))
    }

    impl Wb {
        fn build(&self) -> Vec<u8> {
            ole_with("Workbook", &self.stream().0)
        }

        /// The BIFF stream and the offset of each sheet substream in it.
        fn stream(&self) -> (Vec<u8>, Vec<usize>) {
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
            for &ifmt in &self.xfs {
                stream.extend(xf(ifmt));
            }
            for raw in &self.sst {
                stream.extend(raw);
            }
            let mut patch_at = Vec::new();
            for (i, (name, state, _)) in self.sheets.iter().enumerate() {
                let mut body = vec![0u8; 4];
                body.push(*state);
                body.push(if self.module_sheet == Some(i) { 0x06 } else { 0 });
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
            let mut starts = vec![0; self.sheets.len()];
            let mut order: Vec<usize> = (0..self.sheets.len()).collect();
            if self.substreams_reversed {
                order.reverse();
            }
            for i in order {
                let (_, _, records) = &self.sheets[i];
                starts[i] = stream.len();
                let offset = (stream.len() as u32).to_le_bytes();
                stream[patch_at[i]..patch_at[i] + 4].copy_from_slice(&offset);
                stream.extend(bof(WORKSHEET_SUBSTREAM, 0x0600));
                stream.extend_from_slice(records);
                if !self.unended_sheets.contains(&i) {
                    stream.extend(rec(EOF_REC, &[]));
                }
            }
            stream.truncate(stream.len() - self.cut);
            (stream, starts)
        }
    }

    fn one_sheet(records: Vec<u8>) -> Wb {
        Wb { xfs: vec![0], sheets: vec![("S", 0, records)], ..Wb::default() }
    }

    fn grid(wb: &Wb) -> Vec<Vec<String>> {
        texts(first_table(&parse(&ParseCtx::default(), &wb.build()).unwrap()))
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
        assert_eq!(grid(&wb), vec![vec!["HELLO WORLD"]]);
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
        assert_eq!(grid(&wb), vec![vec!["ab", "ok"]]);
    }

    #[test]
    fn rk_and_mulrk_encodings_decode() {
        // Trap 3: RK packs an integer or a truncated double, with a
        // divide-by-100 bit; MULRK packs a run of them, each with its own
        // format index.
        let mut records = rk_cell(0, 0, 0, rk_from_f64(1234.5));
        records.extend(rk_cell(0, 1, 0, rk_from_int(15550) | 1)); // 155.5
        records.extend(mulrk_rec(0, 2, &[(0, rk_from_int(7)), (1, rk_from_f64(0.5))]));
        let wb = Wb {
            formats: vec![(164, "0%")],
            xfs: vec![0, 164],
            sheets: vec![("S", 0, records)],
            ..Wb::default()
        };
        assert_eq!(grid(&wb), vec![vec!["1234.5", "155.5", "7", "50%"]]);
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
        assert_eq!(grid(&wb), vec![vec!["15.5%", "$1,234.50", "9,876,543"]]);
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
        assert_eq!(grid(&dated(false)), vec![vec!["2026-03-15"]]);
        assert_eq!(grid(&dated(true)), vec![vec!["1904-04-10"]]);
    }

    #[test]
    fn hidden_rows_columns_and_sheets_are_omitted() {
        let records = [
            label(0, 0, 0, "a"),
            label(0, 1, 0, "hidden col"),
            label(0, 2, 0, "c"),
            label(1, 0, 0, "hidden row"),
            label(2, 0, 0, "d"),
            hidden_row(1),
            hidden_cols(1, 1),
        ]
        .concat();
        let wb = Wb {
            xfs: vec![0],
            sheets: vec![("Shown", 0, records), ("Secret", 1, label(0, 0, 0, "secret"))],
            ..Wb::default()
        };
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_matches!(doc.blocks.as_slice(), [Block::UnitStart(unit), Block::Table(_)] if unit.ordinal.get() == 1, "hidden sheet must add no unit, heading or table");
        assert_eq!(texts(first_table(&doc)), vec![vec!["a", "c"], vec!["d", ""]]);
    }

    #[test]
    fn a_merge_past_the_content_is_clamped_to_it() {
        let wb = one_sheet([label(0, 5, 0, "wide"), merged(&[[0, 2, 5, 14]])].concat());
        assert_eq!(grid(&wb), vec![vec!["wide"]]);
        assert_eq!(diagnose(&wb), []);
    }

    #[test]
    fn a_cell_a_merge_clamped_to_the_content_covers_shows_nothing() {
        let records = [
            label(1, 0, 0, "a"),
            label(1, 1, 0, "hidden"),
            label(1, 2, 0, "c"),
            merged(&[[0, 1, 1, 1]]),
        ]
        .concat();
        let wb = one_sheet(records);
        assert_eq!(grid(&wb), vec![vec!["a", "", "c"]]);
        assert_eq!(diagnose(&wb), []);
        let records = [
            label(2, 0, 0, "a"),
            label(2, 1, 0, "hidden"),
            label(2, 3, 0, "d"),
            merged(&[[1, 2, 1, 2]]),
        ]
        .concat();
        assert_eq!(grid(&one_sheet(records)), vec![vec!["a", "", "<covered>", "d"]]);
    }

    #[test]
    fn a_cell_past_the_biff8_columns_is_unreadable_only_where_its_text_shows() {
        let mut records = label(0, 0, 0, "a");
        records.extend(label(0, 256, 0, "past"));
        records.extend(label(0, 257, 0, " "));
        records.extend(number(1, 300, 0, 1.0));
        records.extend(mulrk_rec(2, 256, &[(0, rk_from_int(7)); 2]));
        records.extend(cell_rec(BOOLERR, 0, 258, 0, &[0x99, 1]));
        records.extend(formula(0, 259, 0, [2, 0, 0x99, 0, 0, 0, 0xFF, 0xFF]));
        records.extend(cell_rec(LABEL, 0, 260, 0, &[9, 0, 0, b'a']));
        records.extend(label(3, 256, 0, "past in a hidden row"));
        records.extend(cell_rec(BOOLERR, 3, 258, 0, &[0x99, 1]));
        records.extend(hidden_row(3));
        let wb = one_sheet(records);
        assert_eq!(grid(&wb), vec![vec!["a"]]);
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::CellValueUnreadable, 1, 7),
                in_unit(DiagnosticCode::HiddenOmitted, 1, 1)
            ]
        );
    }

    fn hidden_cols(first: u16, last: u16) -> Vec<u8> {
        rec(COLINFO, &le16(&[first, last, 0, 0, 0x01, 0]))
    }

    fn hidden_row(row: u16) -> Vec<u8> {
        rec(ROW, &le16(&[row, 0, 0, 0, 0, 0, 0x20, 0]))
    }

    #[test]
    fn cells_and_losses_held_as_the_sheet_streams_count_against_the_grid_slots_left() {
        use crate::limits::{Limit, Limits};
        let limits = Limits { max_grid_slots: 5, ..Limits::default() };
        let placed = [label(0, 0, 0, "a"), label(0, 1, 0, "b")].concat();
        let run = |records: Vec<u8>| {
            let sheets = vec![("A", 0, placed.clone()), ("B", 0, records)];
            parse(&ParseCtx::new(&limits), &Wb { xfs: vec![0], sheets, ..Wb::default() }.build())
        };
        let unread: fn(u16) -> Vec<u8> = |col| labelsst(0, col, 0, 9);
        let hidden = |col: u16| [hidden_row(0), label(0, col, 0, "x")].concat();
        let past = |col: u16| label(0, 256 + col, 0, "x");
        for (case, cell) in [("lose", unread), ("put", hidden), ("put past the grid", past)] {
            let records = |cells: u16| (0..cells).flat_map(cell).collect::<Vec<u8>>();
            assert!(run(records(3)).is_ok(), "{case}");
            let refused = run(records(4)).map(drop);
            assert_matches!(
                refused,
                Err(ConvertError::ResourceLimit { limit: Limit::GridSlots, max: 5 }),
                "{case}"
            );
        }
    }

    #[test]
    fn a_hidden_column_past_the_biff8_columns_reports_its_content_as_hidden_only() {
        let records = [hidden_cols(256, 300), label(0, 0, 0, "a"), label(0, 256, 0, "past")];
        let wb = one_sheet(records.concat());
        assert_eq!(grid(&wb), vec![vec!["a"]]);
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::HiddenOmitted, 1, 1)]);
    }

    #[test]
    fn a_merge_moved_off_a_hidden_origin_replaces_the_covered_text_it_lands_on() {
        let run = |records: Vec<u8>| grid(&one_sheet(records));
        let below = [
            hidden_row(0),
            label(1, 0, 0, "covered"),
            label(1, 1, 0, "y"),
            merged(&[[0, 2, 0, 0]]),
        ];
        assert_eq!(run(below.concat()), vec![vec!["y"]]);
        let beside = [
            hidden_cols(0, 0),
            label(0, 1, 0, "covered"),
            label(0, 2, 0, "y"),
            merged(&[[0, 0, 0, 1]]),
        ];
        assert_eq!(run(beside.concat()), vec![vec!["y"]]);
        let kept = [
            hidden_row(0),
            label(0, 0, 0, "kept"),
            label(1, 0, 0, "covered"),
            label(1, 1, 0, "y"),
            merged(&[[0, 2, 0, 0]]),
        ];
        assert_eq!(run(kept.concat()), vec![vec!["kept", "y"]]);
    }

    #[test]
    fn a_merge_past_the_biff8_columns_is_clamped_and_one_outside_them_dropped() {
        let ranges = merged(&[[0, 0, 250, 300], [0, 0, 256, 300]]);
        let wb = one_sheet([label(0, 250, 0, "wide"), ranges].concat());
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        let table = first_table(&doc);
        let cell = origin(table);
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
        assert_eq!(grid(&one_sheet(records)), vec![vec!["2.5", "calc", "TRUE", "#REF!"]]);
    }

    #[test]
    fn a_formula_value_needs_all_eight_bytes() {
        let cut = |col: u16, value: [u8; 8], len: usize| {
            let body = [cell6(0, col, 0), value.to_vec()].concat();
            rec(FORMULA, &body[..len])
        };
        let records = [
            cut(0, 2.5f64.to_le_bytes(), 14),
            cut(1, 2.5f64.to_le_bytes(), 13),
            cut(2, [0x01, 0, 1, 0, 0, 0, 0xFF, 0xFF], 13),
            cut(3, [0x01, 0, 1, 0, 0, 0, 0xFF, 0xFF], 14),
        ]
        .concat();
        assert_eq!(grid(&one_sheet(records)), vec![vec!["2.5", "", "", "TRUE"]]);
    }

    #[test]
    fn a_format_record_too_short_for_its_code_defines_nothing() {
        let globals = [rec(FORMAT, &[]), rec(FORMAT, &[0xA4]), rec(FORMAT, &[0xA4, 0])].concat();
        let wb = Wb {
            formats: vec![(0xA4, "0.00")],
            xfs: vec![0xA4],
            globals,
            ..one_sheet(number(0, 0, 0, 2.5))
        };
        assert_eq!(grid(&wb), vec![vec!["2.50"]]);
    }

    #[test]
    fn a_text_box_takes_only_the_characters_it_declares() {
        let base = [vec![0u8; 10], 1u16.to_le_bytes().to_vec(), vec![0u8; 6]].concat();
        assert_eq!(txo_text(&[&base, &[0x01, b'a', 0, b'b', 0]]), Some(("a".to_string(), false)));
        assert_eq!(txo_text(&[&base, &[0x00, b'a', b'b']]), Some(("a".to_string(), false)));
    }

    #[test]
    fn a_fixed_field_may_start_on_a_segment_boundary_but_never_straddle_one() {
        let mut r = SegReader::new(&[&[1, 2], &[], &[3, 4, 5]]);
        assert_eq!(r.u16(), Some(0x0201));
        assert_eq!(r.u16(), Some(0x0403));
        assert_eq!(r.u16(), None);
        assert_eq!(r.u8(), Some(5));
        assert_eq!((r.u8(), r.next_seg()), (None, None));
        let mut r = SegReader::new(&[&[1, 2, 3], &[4, 5, 6, 7]]);
        assert_eq!(r.u8(), Some(1));
        assert_eq!(r.u32(), None);
        assert_eq!(r.u16(), Some(0x0302));
        assert_eq!(r.u32(), Some(0x0706_0504));
        let mut r = SegReader::new(&[&[1, 2], &[3, 4, 5]]);
        assert_eq!(r.skip(3), Some(()));
        assert_eq!(r.u8(), Some(4));
        assert_eq!(r.skip(2), None);
        let mut r = SegReader::new(&[&[1, 2], &[], &[3]]);
        assert_eq!((r.u8(), r.next_seg(), r.in_seg()), (Some(1), Some(()), 0));
        assert_eq!(r.u8(), Some(3));
        let mut r = SegReader::new(&[]);
        assert_eq!((r.u8(), r.in_seg(), r.skip(0), r.skip(1)), (None, 0, Some(()), None));
    }

    #[test]
    fn boolerr_renders_bool_and_error_literals() {
        let cell = |col: u16, value: u8, is_err: u8| cell_rec(BOOLERR, 0, col, 0, &[value, is_err]);
        let records = [cell(0, 1, 0), cell(1, 0, 0), cell(2, 0x07, 1)].concat();
        assert_eq!(grid(&one_sheet(records)), vec![vec!["TRUE", "FALSE", "#DIV/0!"]]);
    }

    #[test]
    fn a_boolerr_with_an_unknown_flag_is_unreadable_and_a_short_one_is_skipped() {
        let mut records = label(0, 0, 0, "a");
        records.extend(cell_rec(BOOLERR, 1, 0, 0, &[0x07, 2]));
        records.extend(cell_rec(BOOLERR, 2, 0, 0, &[0x07]));
        records.extend(label(3, 0, 0, "b"));
        let wb = one_sheet(records);
        assert_eq!(grid(&wb), vec![vec!["a"], vec![""], vec![""], vec!["b"]]);
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::CellValueUnreadable, 1, 1)]);
    }

    #[test]
    fn a_shared_string_table_reads_only_the_strings_it_declares() {
        let mut sst = 5u32.to_le_bytes().to_vec();
        sst.extend(1u32.to_le_bytes());
        sst.extend(ustr("a"));
        sst.extend(ustr("b"));
        let mut records = labelsst(0, 0, 0, 0);
        records.extend(labelsst(0, 1, 0, 1));
        records.extend(label(0, 2, 0, "c"));
        let wb = Wb { sst: vec![rec(SST, &sst)], ..one_sheet(records) };
        assert_eq!(grid(&wb), vec![vec!["a", "", "c"]]);
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::CellValueUnreadable, 1, 1)]);
    }

    #[test]
    fn a_string_record_with_no_formula_waiting_for_it_is_ignored() {
        let mut records = formula(0, 0, 0, [0, 0, 0, 0, 0, 0, 0xFF, 0xFF]);
        records.extend(rec(STRING, &ustr("calc")));
        records.extend(rec(STRING, &ustr("again")));
        assert_eq!(grid(&one_sheet(records)), vec![vec!["calc"]]);
    }

    #[test]
    fn a_formula_number_with_one_marker_byte_is_still_a_number() {
        let number = f64::from_bits(0x3FFF_FFFF_FFFF_FFFF).to_le_bytes();
        assert_eq!(grid(&one_sheet(formula(0, 0, 0, number))), vec![vec!["2"]]);
    }

    #[test]
    fn filepass_means_encrypted() {
        let wb = Wb { filepass: true, ..one_sheet(label(0, 0, 0, "x")) };
        assert_matches!(parse(&ParseCtx::default(), &wb.build()), Err(ConvertError::Encrypted));
    }

    /// An MSODRAWING body: one SpContainer holding a client anchor that
    /// starts at (row, col), plus an OPT hiding the shape when asked.
    fn msodrawing(row: u16, col: u16, hidden: bool) -> Vec<u8> {
        let anchor = le16(&[0, col, 0, row, 0, col + 1, 0, row + 1, 0]);
        let hiding =
            if hidden { record(0x0013, 0xF00B, &le16(&[0x03BF, 2, 2])) } else { Vec::new() };
        let children = [hiding, record(0, 0xF010, &anchor)].concat();
        rec(MSODRAWING, &record(0x000F, 0xF004, &children))
    }

    /// An OBJ record for a form control of type `ot` carrying `state`.
    fn obj(ot: u16, state: u16) -> Vec<u8> {
        let header = le16(&[0x0015, 0x0012, ot, 1]);
        rec(OBJ, &[header, vec![0; 14], le16(&[0x0012, 8, state]), vec![0; 10]].concat())
    }

    /// A TXO record with its text and formatting-run CONTINUE records.
    fn txo(text: &str) -> Vec<u8> {
        let body = le16(&[0, 0, 0, 0, 0, text.len() as u16, 16, 0, 0]);
        let chars = [&[0x00][..], text.as_bytes()].concat();
        [rec(TXO, &body), rec(CONTINUE, &chars), rec(CONTINUE, &[0; 16])].concat()
    }

    /// The MSODRAWING that carries an OBJ's text box into the TXO after it.
    fn client_textbox() -> Vec<u8> {
        rec(MSODRAWING, &record(0, 0xF00D, &[]))
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
        assert_eq!(grid(&one_sheet(records)), vec![vec!["14", "", "", "[x] Roof", "[ ]"]]);
    }

    fn diagnose(wb: &Wb) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, &wb.build()).unwrap();
        reported(parse_ctx)
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
        let doc = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_matches!(doc.blocks.as_slice(), [Block::UnitStart(unit), Block::Heading { .. }, Block::Table(_)] if unit.ordinal.get() == 1, "unreadable sheets add no unit");
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
    fn a_shown_or_hidden_sheet_holds_its_number_against_max_pages() {
        use crate::limits::Limits;
        for hidden in [[0, 1, 0], [0, 0, 1]] {
            let sheet = |(name, flag)| (name, flag, number(0, 0, 0, 1.0));
            let sheets = ["A", "B", "C"].into_iter().zip(hidden).map(sheet).collect();
            let bytes = Wb { xfs: vec![0], sheets, ..Wb::default() }.build();
            let at = |max_pages| {
                let limits = Limits { max_pages, ..Limits::DEFAULT };
                parse(&ParseCtx::new(&limits), &bytes)
            };
            assert!(at(3).is_ok(), "{hidden:?}");
            assert_matches!(
                at(2),
                Err(ConvertError::ResourceLimit { limit: Limit::Pages, max: 2 }),
                "{hidden:?}"
            );
        }
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
        records.extend(cell_rec(BOOLERR, 1, 0, 0, &[0x99, 1]));
        records.extend(formula(2, 0, 0, [2, 0, 0x99, 0, 0, 0, 0xFF, 0xFF]));
        records.extend(cell_rec(LABEL, 3, 0, 0, &[9, 0, 0, b'a']));
        records.extend(cell_rec(LABEL, 4, 0, 0, &[1, 0, 1, 0x00, 0xDC]));
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
        records.extend(cell_rec(LABEL, 0, 2, 0, &[1, 0, 1, 0x00, 0xDC]));
        records.extend(cell_rec(BOOLERR, 3, 2, 0, &[0x99, 1]));
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
        let book = biff5(Some(1253), &[], &[(b"S", biff5_label(0, 0, 0, &[0xAA, 0xAB]))]);
        assert_eq!(
            converted(&book),
            (
                vec![vec![vec!["\u{FFFD}\u{AB}".into()]]],
                vec![in_unit(DiagnosticCode::InvalidEncoding, 1, 1)]
            )
        );
    }

    /// A BIFF5 workbook: its CODEPAGE record when one is given, `globals`
    /// verbatim, then one shown sheet per (name bytes, records).
    fn biff5(codepage: Option<u16>, globals: &[u8], sheets: &[(&[u8], Vec<u8>)]) -> Vec<u8> {
        let mut stream = bof(0x0005, 0x0500);
        if let Some(cp) = codepage {
            stream.extend(rec(CODEPAGE, &cp.to_le_bytes()));
        }
        stream.extend_from_slice(globals);
        let mut patches = Vec::new();
        for (name, _) in sheets {
            let mut boundsheet = vec![0u8; 6];
            boundsheet.push(name.len() as u8);
            boundsheet.extend_from_slice(name);
            patches.push(stream.len() + 4);
            stream.extend(rec(BOUNDSHEET, &boundsheet));
        }
        stream.extend(rec(EOF_REC, &[]));
        for ((_, records), at) in sheets.iter().zip(patches) {
            let offset = (stream.len() as u32).to_le_bytes();
            stream[at..at + 4].copy_from_slice(&offset);
            stream.extend(bof(WORKSHEET_SUBSTREAM, 0x0500));
            stream.extend_from_slice(records);
            stream.extend(rec(EOF_REC, &[]));
        }
        ole_with("Book", &stream)
    }

    fn biff5_label(row: u16, col: u16, ixfe: u16, bytes: &[u8]) -> Vec<u8> {
        cell_rec(LABEL, row, col, ixfe, &[&le16(&[bytes.len() as u16])[..], bytes].concat())
    }

    fn converted(book: &[u8]) -> (Vec<Vec<Vec<String>>>, Vec<Reported>) {
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, book).unwrap();
        (tables_of(&doc), reported(parse_ctx))
    }

    #[test]
    fn a_biff5_label_reads_in_its_code_page_with_no_diagnostic() {
        for (codepage, byte, text) in [
            (Some(1252), 0xE9, "\u{e9}"),
            (Some(10000), 0x8E, "\u{e9}"),
            (Some(32768), 0x8E, "\u{e9}"),
            (Some(10007), 0xE0, "\u{430}"),
            (Some(437), 0x82, "\u{e9}"),
            (Some(850), 0xD0, "\u{f0}"),
            (Some(65001), 0x41, "A"),
            (Some(32769), 0x80, "\u{20ac}"),
            (Some(1251), 0xE0, "\u{430}"),
            (None, 0x80, "\u{20ac}"),
        ] {
            let book = biff5(codepage, &[], &[(b"S", biff5_label(0, 0, 0, &[byte]))]);
            assert_eq!(converted(&book), (vec![vec![vec![text.into()]]], vec![]), "{codepage:?}");
        }
    }

    #[test]
    fn a_biff5_label_in_an_assumed_code_page_reports_in_its_unit() {
        let sheets: [(&[u8], _); 2] =
            [(b"A", biff5_label(0, 0, 0, b"ok")), (b"B", biff5_label(0, 0, 0, &[0xE9]))];
        let (tables, reported) = converted(&biff5(Some(1361), &[], &sheets));
        assert_eq!(tables[1], [["\u{e9}"]]);
        assert_eq!(reported, [in_unit(DiagnosticCode::EncodingAssumed, 2, 1)]);
        let ascii = biff5(Some(1361), &[], &[(b"S", biff5_label(0, 0, 0, b"ok"))]);
        assert_eq!(converted(&ascii).1, []);
    }

    #[test]
    fn a_biff5_rich_string_in_an_assumed_code_page_reports_in_its_unit() {
        let records = cell_rec(RSTRING, 0, 0, 0, &[1, 0, 0xE9, 1, 0, 0]);
        let (tables, reported) = converted(&biff5(Some(1361), &[], &[(b"S", records)]));
        assert_eq!(tables, [[["\u{e9}"]]]);
        assert_eq!(reported, [in_unit(DiagnosticCode::EncodingAssumed, 1, 1)]);
    }

    #[test]
    fn the_last_codepage_record_reads_the_strings_after_it() {
        let late = rec(CODEPAGE, &10000u16.to_le_bytes());
        let book = biff5(Some(1251), &late, &[(b"S", biff5_label(0, 0, 0, &[0x8E]))]);
        assert_eq!(converted(&book), (vec![vec![vec!["\u{e9}".into()]]], vec![]));
        let globals = [biff5_formats(&[b"0\"\x80\""]), rec(CODEPAGE, &le16(&[1361]))].concat();
        let records = [number(0, 0, 1, 2.0), biff5_label(0, 1, 1, &[0xE9])].concat();
        let (tables, reported) = converted(&biff5(None, &globals, &[(b"S", records)]));
        assert_eq!(tables, [[["2\u{20ac}", "\u{e9}"]]]);
        assert_eq!(reported, [in_unit(DiagnosticCode::EncodingAssumed, 1, 1)]);
    }

    #[test]
    fn a_biff5_formula_string_in_an_assumed_code_page_reports_in_its_unit() {
        let mut records = formula(0, 0, 0, [0, 0, 0, 0, 0, 0, 0xFF, 0xFF]);
        records.extend(rec(STRING, &[1, 0, 0xE9]));
        let (tables, reported) = converted(&biff5(Some(1361), &[], &[(b"S", records)]));
        assert_eq!(tables, [[["\u{e9}"]]]);
        assert_eq!(reported, [in_unit(DiagnosticCode::EncodingAssumed, 1, 1)]);
    }

    #[test]
    fn a_shown_biff5_sheet_name_in_an_assumed_code_page_reports_with_its_heading() {
        let cell = || biff5_label(0, 0, 0, b"x");
        let two = biff5(Some(1361), &[], &[(b"A", cell()), (&[b'N', 0xE9], cell())]);
        let parse_ctx = ParseCtx::default();
        let markdown =
            crate::render::markdown::document_to_markdown(&parse(&parse_ctx, &two).unwrap());
        assert!(markdown.contains("## N\u{e9}"), "{markdown}");
        assert_eq!(reported(parse_ctx), [in_unit(DiagnosticCode::EncodingAssumed, 2, 1)]);
        let alone = biff5(Some(1361), &[], &[(&[b'N', 0xE9], cell())]);
        assert_eq!(converted(&alone).1, []);
    }

    #[test]
    fn a_biff5_format_code_in_an_assumed_code_page_reports_only_where_its_section_shows() {
        let globals =
            biff5_formats(&[b"0\" \xE9\";0\" x\";0;\"\xE9\"@", b"0%\" \xE9\"", b"yyyy\" \xE9\""]);
        let shown = |records: Vec<u8>| converted(&biff5(Some(1361), &globals, &[(b"S", records)]));
        let assumed = vec![in_unit(DiagnosticCode::EncodingAssumed, 1, 1)];
        assert_eq!(
            shown(number(0, 0, 1, 2.0)),
            (vec![vec![vec!["2 \u{e9}".into()]]], assumed.clone())
        );
        assert_eq!(
            shown(biff5_label(0, 0, 1, b"t")),
            (vec![vec![vec!["\u{e9}t".into()]]], assumed)
        );
        assert_eq!(shown(number(0, 0, 1, -2.0)), (vec![vec![vec!["2 x".into()]]], vec![]));
        assert_eq!(shown(number(0, 0, 1, 0.0)), (vec![vec![vec!["0".into()]]], vec![]));
        assert_eq!(shown(number(0, 0, 0, 2.0)), (vec![vec![vec!["2".into()]]], vec![]));
        let general = super::super::format_float(f64::MAX);
        assert_eq!(shown(number(0, 0, 2, f64::MAX)), (vec![vec![vec![general]]], vec![]));
        assert_eq!(shown(number(0, 0, 3, 2.0)), (vec![vec![vec!["1900-01-02".into()]]], vec![]));
    }

    /// FORMAT records for `codes` at 164 on, each with an XF after XF 0.
    fn biff5_formats(codes: &[&[u8]]) -> Vec<u8> {
        let formats = (164u16..).zip(codes).map(|(ifmt, code)| {
            let format = [&le16(&[ifmt])[..], &[code.len() as u8], code].concat();
            [rec(FORMAT, &format), xf(ifmt)].concat()
        });
        [xf(0)].into_iter().chain(formats).flatten().collect()
    }

    #[test]
    fn a_biff5_string_its_format_hides_reports_no_guessed_code_page() {
        let globals = biff5_formats(&[b";;;"]);
        let mut records = biff5_label(0, 0, 0, &[0xE9]);
        records.extend(biff5_label(1, 0, 1, &[0xE9]));
        let (tables, reported) = converted(&biff5(Some(1361), &globals, &[(b"S", records)]));
        assert_eq!(tables, [[["\u{e9}"]]]);
        assert_eq!(reported, [in_unit(DiagnosticCode::EncodingAssumed, 1, 1)]);
    }

    #[test]
    fn a_biff5_string_its_format_hides_reports_no_invalid_byte() {
        let globals = biff5_formats(&[b";;;"]);
        let mut records = biff5_label(0, 0, 0, &[0xA5]);
        records.extend(biff5_label(1, 0, 1, &[0xA5]));
        let (tables, reported) = converted(&biff5(Some(28593), &globals, &[(b"S", records)]));
        assert_eq!(tables, [[["\u{FFFD}"]]]);
        assert_eq!(reported, [in_unit(DiagnosticCode::InvalidEncoding, 1, 1)]);
    }

    #[test]
    fn a_biff5_format_literal_guess_reports_though_cleaning_changes_it() {
        let globals = biff5_formats(&[b"0\"\xA0x\";0\"\xAD\""]);
        let shown = |n: f64| {
            let book = biff5(Some(1361), &globals, &[(b"S", number(0, 0, 1, n))]);
            converted(&book)
        };
        let assumed = vec![in_unit(DiagnosticCode::EncodingAssumed, 1, 1)];
        assert_eq!(shown(2.0), (vec![vec![vec!["2 x".into()]]], assumed.clone()));
        assert_eq!(shown(-2.0), (vec![vec![vec!["2".into()]]], assumed));
    }

    #[test]
    fn biff8_strings_never_report_an_assumed_code_page() {
        let mut records = label(0, 0, 0, "ok");
        records.extend(cell_rec(LABEL, 0, 1, 0, &[1, 0, 0, 0xE9]));
        let wb = Wb { globals: rec(CODEPAGE, &1361u16.to_le_bytes()), ..one_sheet(records) };
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["ok", "\u{e9}"]]);
        assert_eq!(reported(parse_ctx), []);
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
        let cmo = [le16(&[0x0015, 0x0012, 0x0014, 1]), vec![0; 14]].concat();
        let lbs_data = [le16(&[0x0013, 0x1FEE]), vec![0; 6]].concat();
        let list = [u8::from(lct != 0), lct, 0, 0, 0x02, 0, 8, 0, 20, 0, 0, 0, 0];
        rec(OBJ, &[cmo, lbs_data, list.to_vec()].concat())
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

    #[test]
    fn a_footer_with_text_is_reported_as_omitted() {
        let records = [label(0, 0, 0, "x"), rec(FOOTER, &ustr("&P"))].concat();
        assert_eq!(diagnose(&one_sheet(records)), [in_unit(DiagnosticCode::FooterOmitted, 1, 1)]);
    }

    #[test]
    fn a_form_control_the_grid_cannot_place_is_reported_as_unread() {
        let mut no_list_data = dropdown(3);
        no_list_data[28..30].copy_from_slice(&0u16.to_le_bytes());
        for (case, anchor_col, object) in [
            ("a checkbox past the BIFF8 columns", 256, obj(0x0B, 1)),
            ("a drop-down with no list data", 5, no_list_data),
        ] {
            let mut records = label(0, 0, 0, "x");
            records.extend(msodrawing(0, anchor_col, false));
            records.extend(object);
            let reported = diagnose(&one_sheet(records));
            assert_eq!(reported, [in_unit(DiagnosticCode::FormControlUnread, 1, 1)], "{case}");
        }
    }

    #[test]
    fn a_biff5_sheet_does_not_read_its_obj_records() {
        let records = [biff5_label(0, 0, 0, b"x"), obj(0x0B, 1)].concat();
        let book = biff5(None, &[], &[(b"S", records)]);
        assert_eq!(converted(&book), (vec![vec![vec!["x".into()]]], vec![]));
    }

    #[test]
    fn a_new_shape_container_forgets_the_previous_shapes_anchor_and_hidden_state() {
        let shape = |row, col, hidden| msodrawing(row, col, hidden)[4..].to_vec();
        let placed = |data: &[u8]| last_shape(data).map(|s| (s.anchor, s.hidden));
        let empty_container = vec![0x0F, 0x00, 0x04, 0xF0, 0, 0, 0, 0];
        let hidden_then_shown = [shape(0, 3, true), shape(1, 4, false)].concat();
        assert_eq!(placed(&hidden_then_shown), Some((CellRef::new(1, 4), false)));
        let anchored_then_empty = [shape(0, 3, false), empty_container].concat();
        assert_eq!(placed(&anchored_then_empty), None);
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
    fn an_hlink_record_links_within_the_workbook_or_outside_it_or_is_too_short() {
        let record = hlink_over([3, 1], [4, 2], None, "https://example.org/");
        let body = &record[4..];
        let Some(HlinkRecord::External(link)) = hlink(body) else {
            panic!("a record with a moniker is an external link")
        };
        assert_eq!(link.range, CellRange::spanning(CellRef::new(3, 4), CellRef::new(1, 2)));
        let LinkTarget::Url(url) = &link.target else { panic!("the moniker reads") };
        assert_eq!(&**url, "https://example.org/");

        let mut location = body.to_vec();
        location[28] = 0x08;
        assert!(matches!(hlink(&location), Some(HlinkRecord::InWorkbook)));

        let Some(HlinkRecord::External(unread)) = hlink(&body[..32]) else {
            panic!("a moniker that does not read is still an external link")
        };
        assert!(matches!(unread.target, LinkTarget::Unknown));

        assert!(hlink(&body[..31]).is_none(), "the flags are cut short");
        assert!(hlink(&body[..7]).is_none(), "the range is cut short");
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
        let format = [le16(&[164, code.len() as u16]), vec![1], le16(&code)].concat();
        let records = [
            number(0, 0, 1, 2.0),
            number(0, 1, 0, 3.0),
            label(0, 2, 1, "text"),
            cell_rec(LABEL, 0, 3, 1, &[1, 0, 1, 0x00, 0xDC]),
            hidden_row(1),
            number(1, 0, 1, 4.0),
        ]
        .concat();
        let wb = Wb {
            xfs: vec![0, 164],
            globals: rec(FORMAT, &format),
            sheets: vec![("S", 0, records)],
            ..Wb::default()
        };
        assert_eq!(grid(&wb), vec![vec!["2 \u{FFFD}", "3", "text", "\u{FFFD}"]]);
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
        assert_eq!(grid(&wb), vec![vec!["2", "2"]]);
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
        assert_eq!(grid(&wb), vec![vec!["2", "2", "2"]]);
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
        assert_eq!(grid(&wb), vec![vec!["t", "t"]]);
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::NumberFormatUnread, 1, 1)]);
    }

    #[test]
    fn a_replaced_string_under_an_unread_text_section_reports_both() {
        let decorated = format!("0;0;0;\"{}\"@", "x".repeat(247));
        let mut records = cell_rec(LABEL, 0, 0, 1, &[2, 0, 1, 0x74, 0x00, 0x00, 0xD8]);
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
        assert_eq!(grid(&wb), vec![vec!["t\u{FFFD}"; 2]]);
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::InvalidEncoding, 1, 2),
                in_unit(DiagnosticCode::NumberFormatUnread, 1, 2),
            ]
        );
    }

    const UNREAD: DiagnosticCode = DiagnosticCode::NumberFormatUnread;

    fn format_row(wb: &Wb) -> (Vec<Vec<String>>, Vec<Reported>) {
        (grid(wb), diagnose(wb))
    }

    fn format_cells(ixfes: &[u16]) -> Vec<u8> {
        let mut records = Vec::new();
        for (col, &ixfe) in (0..).zip(ixfes) {
            records.extend(number(0, col, ixfe, 2.5));
        }
        records
    }

    fn unread_format(ifmt: u16, tail: &[u8]) -> Vec<u8> {
        rec(FORMAT, &[le16(&[ifmt]), tail.to_vec()].concat())
    }

    #[test]
    fn a_format_declaration_that_does_not_read_hides_what_its_id_shows() {
        let mut records = format_cells(&[1, 0]);
        records.extend(label(0, 2, 1, "t"));
        for (case, declaration) in [
            ("an id with no string", unread_format(164, &[])),
            ("a string cut short", unread_format(164, &[3, 0, 0, b'A', b'B'])),
        ] {
            let wb = Wb {
                globals: declaration,
                xfs: vec![0, 164],
                sheets: vec![("S", 0, records.clone())],
                ..Wb::default()
            };
            assert_eq!(
                format_row(&wb),
                (vec![vec!["2.5".into(), "2.5".into(), "t".into()]], vec![in_unit(UNREAD, 1, 2)]),
                "{case}"
            );
        }
    }

    #[test]
    fn a_format_code_that_reads_wins_over_a_declaration_that_does_not_in_either_order() {
        let readable = format_record(164, "0.00");
        let unread = unread_format(164, &[]);
        for (case, globals) in [
            ("unread first", [unread.clone(), readable.clone()].concat()),
            ("unread last", [readable, unread].concat()),
        ] {
            let wb = Wb {
                globals,
                xfs: vec![0, 164],
                sheets: vec![("S", 0, format_cells(&[1]))],
                ..Wb::default()
            };
            assert_eq!(format_row(&wb), (vec![vec!["2.50".into()]], vec![]), "{case}");
        }
    }

    #[test]
    fn a_declaration_that_does_not_read_leaves_the_other_ids_their_codes() {
        let wb = Wb {
            globals: [
                format_record(165, "0.00"),
                unread_format(164, &[]),
                format_record(166, "0.0"),
            ]
            .concat(),
            xfs: vec![0, 164, 165, 166, 14],
            sheets: vec![("S", 0, format_cells(&[1, 2, 3, 4]))],
            ..Wb::default()
        };
        assert_eq!(
            format_row(&wb),
            (
                vec![vec!["2.5".into(), "2.50".into(), "2.5".into(), "1900-01-02".into()]],
                vec![in_unit(UNREAD, 1, 1)]
            )
        );
    }

    #[test]
    fn a_declaration_without_an_id_reaches_only_the_ids_a_format_record_can_declare() {
        let wb = Wb {
            formats: vec![(164, "0.00")],
            globals: rec(FORMAT, &[0xA4]),
            xfs: vec![0, 164, 165, 14, 100],
            sheets: vec![("S", 0, format_cells(&[0, 1, 2, 3, 4]))],
            ..Wb::default()
        };
        assert_eq!(
            format_row(&wb),
            (
                vec![vec![
                    "2.5".into(),
                    "2.50".into(),
                    "2.5".into(),
                    "1900-01-02".into(),
                    "2.5".into()
                ]],
                vec![in_unit(UNREAD, 1, 1), in_unit(DiagnosticCode::NumberFormatFallback, 1, 1)]
            )
        );
    }

    #[test]
    fn a_declaration_without_an_id_reaches_each_range_of_ids_a_format_record_can_declare() {
        let ids = [
            (4, false),
            (5, true),
            (8, true),
            (9, false),
            (22, false),
            (23, true),
            (26, true),
            (27, false),
            (40, false),
            (41, true),
            (44, true),
            (45, false),
            (62, false),
            (63, true),
            (66, true),
            (67, false),
            (163, false),
            (164, true),
            (392, true),
            (393, false),
        ];
        for (ifmt, reached) in ids {
            let wb = Wb {
                globals: rec(FORMAT, &[0xA4]),
                xfs: vec![ifmt],
                sheets: vec![("S", 0, label(0, 0, 0, "t"))],
                ..Wb::default()
            };
            let reports = if reached { vec![in_unit(UNREAD, 1, 1)] } else { vec![] };
            assert_eq!(format_row(&wb), (vec![vec!["t".into()]], reports), "ifmt {ifmt}");
        }
    }

    #[test]
    fn an_xf_record_that_does_not_read_keeps_its_place_and_hides_what_its_format_shows() {
        let mut records = format_cells(&[1, 2]);
        records.extend(label(0, 2, 1, "t"));
        let wb = Wb {
            formats: vec![(164, "0.00")],
            globals: [rec(XF, &[0, 0, 1]), xf(164)].concat(),
            xfs: vec![0],
            sheets: vec![("S", 0, records)],
            ..Wb::default()
        };
        assert_eq!(
            format_row(&wb),
            (vec![vec!["2.5".into(), "2.50".into(), "t".into()]], vec![in_unit(UNREAD, 1, 2)])
        );
    }

    #[test]
    fn a_workbook_listing_no_sheet_is_malformed() {
        let wb = Wb { xfs: vec![0], ..Wb::default() };
        let error = parse(&ParseCtx::default(), &wb.build()).unwrap_err();
        assert_matches!(error, ConvertError::Malformed { .. });
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
        assert_eq!(
            diagnose(&cut),
            [
                (DiagnosticCode::TruncatedRecords, Region::Body, None, 1),
                in_unit(DiagnosticCode::TruncatedRecords, 1, 1),
            ]
        );
        assert_eq!(diagnose(&one_sheet(label(0, 0, 0, "a"))), []);
    }

    #[test]
    fn globals_cut_mid_record_report_the_cut_once() {
        let wb = |cut| Wb {
            globals: rec(DATEMODE, &[0, 0]),
            globals_unended: true,
            cut,
            ..one_sheet(label(0, 0, 0, "a"))
        };
        let (stream, starts) = wb(0).stream();
        let dropped_sheet_and_a_byte = stream.len() - starts[0] + 1;
        let parse_ctx = ParseCtx::default();
        let error = parse(&parse_ctx, &wb(dropped_sheet_and_a_byte).build()).unwrap_err();
        assert_matches!(
            error,
            ConvertError::Malformed { reason: "no sheet in the workbook could be read", .. }
        );
        assert_eq!(
            reported(parse_ctx),
            [
                in_unit(DiagnosticCode::UnitUnreadable, 1, 1),
                (DiagnosticCode::TruncatedRecords, Region::Body, None, 1),
            ]
        );
    }

    fn cells(text: &str) -> Vec<u8> {
        (0..3).flat_map(|col| label(0, col, 0, text)).collect()
    }

    fn tables_of(doc: &Document) -> Vec<Vec<Vec<String>>> {
        doc.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Table(t) => Some(texts(t)),
                _ => None,
            })
            .collect()
    }

    fn two_sheets(first: Vec<u8>, second_state: u8) -> Wb {
        Wb {
            xfs: vec![0],
            sheets: vec![("A", 0, first), ("B", second_state, cells("b"))],
            unended_sheets: vec![0],
            ..Wb::default()
        }
    }

    #[test]
    fn globals_without_their_eof_charge_each_record_once() {
        let unended = |globals: Vec<u8>| Wb {
            globals,
            globals_unended: true,
            unended_sheets: Vec::new(),
            ..two_sheets(cells("a"), 0)
        };
        for wb in [unended(Vec::new()), unended(bof(0x0020, 0x0600))] {
            let (stream, _) = wb.stream();
            let doc = parse_within(&stream, records_in(&stream)).unwrap();
            assert_eq!(tables_of(&doc), [vec![vec!["a"; 3]], vec![vec!["b"; 3]]]);
        }
    }

    #[test]
    fn the_record_budget_charges_each_record_read_once() {
        let sst = vec![rec(SST, &[0; 8]), rec(CONTINUE, &[])];
        let drawing = split_at(&msodrawing(0, 1, false), 12);
        let records = [label(0, 0, 0, "a"), drawing, obj(0x0B, 1), client_textbox(), txo("b")];
        let (stream, _) = Wb { sst, ..one_sheet(records.concat()) }.stream();
        let charged = records_in(&stream) - 2;
        parse_within(&stream, charged).unwrap();
        assert_matches!(
            parse_within(&stream, charged - 1),
            Err(ConvertError::ResourceLimit { limit: Limit::Records, max }) if max == charged - 1
        );
    }

    #[test]
    fn a_sheet_without_its_eof_reports_the_stream_truncated() {
        let dropped = Wb { cut: 4, ..one_sheet(label(0, 0, 0, "a")) };
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, &dropped.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["a"]]);
        assert_eq!(reported(parse_ctx), [in_unit(DiagnosticCode::TruncatedRecords, 1, 1)]);
        let cut = Wb { cut: 1, ..one_sheet(label(0, 0, 0, "a")) };
        assert_eq!(diagnose(&cut), [in_unit(DiagnosticCode::TruncatedRecords, 1, 1)]);
    }

    #[test]
    fn a_sheet_without_its_eof_ends_where_the_next_sheet_starts() {
        let wb = two_sheets(cells("a"), 0);
        let (stream, _) = wb.stream();
        let doc = parse_within(&stream, records_in(&stream)).unwrap();
        assert_eq!(tables_of(&doc), [vec![vec!["a"; 3]], vec![vec!["b"; 3]]]);
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::TruncatedRecords, 1, 1)]);
    }

    #[test]
    fn a_sheet_reports_its_own_end_after_another_sheet_hit_the_cut() {
        let wb = Wb {
            cut: 1,
            substreams_reversed: true,
            unended_sheets: vec![1],
            ..two_sheets(cells("a"), 0)
        };
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::TruncatedRecords, 1, 1),
                in_unit(DiagnosticCode::TruncatedRecords, 2, 1),
            ]
        );
    }

    #[test]
    fn a_nested_substream_without_its_eof_ends_where_the_next_sheet_starts() {
        let chart = [bof(0x0020, 0x0600), label(0, 0, 0, "chart")].concat();
        let wb = two_sheets([cells("a"), chart].concat(), 0);
        let (stream, _) = wb.stream();
        let doc = parse_within(&stream, records_in(&stream)).unwrap();
        assert_eq!(tables_of(&doc), [vec![vec!["a"; 3]], vec![vec!["b"; 3]]]);
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::TruncatedRecords, 1, 1)]);
    }

    #[test]
    fn a_nested_substream_with_its_eof_hands_the_sheet_back_its_records() {
        let chart = [bof(0x0020, 0x0600), label(0, 1, 0, "chart"), rec(EOF_REC, &[])].concat();
        let wb = one_sheet([label(0, 0, 0, "a"), chart, label(0, 2, 0, "c")].concat());
        assert_eq!(grid(&wb), vec![vec!["a", "", "c"]]);
        assert_eq!(diagnose(&wb), []);
    }

    #[test]
    fn a_substream_without_its_eof_charges_only_the_records_it_reads() {
        let module = |first| Wb { module_sheet: Some(1), ..two_sheets(first, 0) };
        let drawing = [cells("a"), msodrawing(0, 3, false)].concat();
        let sst = Wb {
            xfs: vec![0],
            sheets: vec![("H", 1, cells("h"))],
            globals: rec(SST, &[0; 8]),
            globals_unended: true,
            ..Wb::default()
        };
        let a = vec![vec![vec!["a"; 3]]];
        for (case, wb, shown) in [
            ("a label before a hidden sheet", two_sheets(cells("a"), 1), a.clone()),
            ("a label before a module", module(cells("a")), a.clone()),
            ("a drawing before a module", module(drawing), a),
            ("an SST before a hidden sheet", sst, Vec::new()),
        ] {
            let (stream, starts) = wb.stream();
            let uncharged_bofs = starts.len() as u64;
            let read = records_in(&stream[..starts[starts.len() - 1]]) - uncharged_bofs;
            let doc = parse_within(&stream, read).unwrap();
            assert_eq!(tables_of(&doc), shown, "{case}");
            assert_matches!(
                parse_within(&stream, read - 1),
                Err(ConvertError::ResourceLimit { limit: Limit::Records, max }) if max == read - 1,
                "{case}"
            );
        }
    }

    #[test]
    fn a_continue_where_another_substream_starts_joins_no_record() {
        let records = [label(0, 0, 0, "a"), rec(CONTINUE, &[0; 2]), label(0, 1, 0, "x")];
        let wb = |module_at: usize| {
            let start = u32::try_from(module_at).unwrap().to_le_bytes();
            Wb {
                globals: rec(BOUNDSHEET, &[&start[..], &[0, 0x06], &short_ustr("M")].concat()),
                unended_sheets: vec![0],
                ..one_sheet(records.concat())
            }
        };
        let (_, starts) = wb(0).stream();
        let continue_at = starts[0] + bof(WORKSHEET_SUBSTREAM, 0x0600).len() + records[0].len();
        let wb = wb(continue_at);
        assert_eq!(grid(&wb), vec![vec!["a"]]);
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::TruncatedRecords, 1, 1)]);
    }

    #[test]
    fn a_workbook_whose_only_sheet_is_a_module_is_malformed() {
        let wb = Wb { module_sheet: Some(0), ..one_sheet(cells("m")) };
        let error = parse(&ParseCtx::default(), &wb.build()).unwrap_err();
        assert_matches!(
            error,
            ConvertError::Malformed { reason: "the workbook lists no sheet", .. }
        );
    }

    #[test]
    fn a_link_covers_its_whole_range() {
        let mut records = label(0, 0, 0, "see here");
        let mut link = hlink_record(0, 0, "https://example.org/paper");
        link[4 + 2..4 + 4].copy_from_slice(&2u16.to_le_bytes());
        link[4 + 6..4 + 8].copy_from_slice(&1u16.to_le_bytes());
        records.extend(link);
        records.extend(hidden_row(0));
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
    fn record_bodies_decode_into_their_cells_ranges_and_regions() {
        let mut mulrk = [3u16.to_le_bytes(), 5u16.to_le_bytes(), 7u16.to_le_bytes()].concat();
        mulrk.extend(0x0000_0006u32.to_le_bytes());
        mulrk.extend(8u16.to_le_bytes());
        mulrk.extend(0x0000_000Au32.to_le_bytes());
        mulrk.extend(6u16.to_le_bytes());
        let cells: Vec<_> = mulrk_cells(&mulrk).collect();
        assert_eq!(cells, [(CellRef::new(3, 5), 7, 6), (CellRef::new(3, 6), 8, 10)]);
        assert_eq!(mulrk_cells(&mulrk[..3]).count(), 0);
        let merged = [2u16, 1, 2, 0, 3, 9, 9, 9, 9].map(u16::to_le_bytes).concat();
        let ranges: Vec<_> = merged_ranges(&merged).collect();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], CellRange::spanning(CellRef::new(1, 0), CellRef::new(2, 3)));
        assert_eq!(merged_ranges(&merged[..12]).count(), 1);
        let mut hf = vec![0u8; 38];
        hf[30] = 1;
        hf[36] = 2;
        assert_eq!(
            header_footer_regions(HEADER_FOOTER, &hf, true).collect::<Vec<_>>(),
            [Region::Header, Region::Footer]
        );
        assert_eq!(header_footer_regions(HEADER_FOOTER, &hf[..31], true).count(), 0);
        assert_eq!(
            header_footer_regions(FOOTER, &[2, 0], true).collect::<Vec<_>>(),
            [Region::Footer]
        );
        assert_eq!(header_footer_regions(HEADER, &[0, 0], true).count(), 0);
        assert_eq!(header_footer_regions(HEADER, &[0], true).count(), 0);
        assert_eq!(header_footer_regions(HEADER, &[0, 1], false).count(), 0);
        assert_eq!(
            header_footer_regions(HEADER, &[1], false).collect::<Vec<_>>(),
            [Region::Header]
        );
        assert_eq!(bool_or_error(true, 0x07), Some("#DIV/0!"));
        assert_eq!(bool_or_error(false, 0), Some("FALSE"));
        assert_eq!(bool_or_error(false, 5), Some("TRUE"));
        assert_eq!(bool_or_error(true, 0x99), None);
    }

    #[test]
    fn a_string_reads_its_prefix_width_and_skips_a_rich_trailer() {
        let rich: &[u8] = &[1, 0, 0x08, 1, 0, b'a', 0, 0, 0, 0, 9];
        for (case, prefix, runs, bytes, text, next) in [
            ("an 8-bit count", LengthPrefix::U8, Runs::Plain, &[2, 0, b'a', b'b', 9][..], "ab", 9),
            ("a 16-bit count", LengthPrefix::U16, Runs::Plain, &[2, 0, 0, b'a', b'b', 9], "ab", 9),
            ("a rich trailer", LengthPrefix::U16, Runs::Rich, rich, "a", 9),
            ("a rich trailer read as plain", LengthPrefix::U16, Runs::Plain, rich, "\u{1}", 0),
        ] {
            let segs = [bytes];
            let mut r = SegReader::new(&segs);
            let decoded = read_biff8_string(&mut r, prefix, runs);
            assert_eq!(decoded, Some((text.into(), false)), "{case}");
            assert_eq!(r.u8(), Some(next), "{case}");
        }
        let byte: [&[u8]; 1] = [&[2, 0, b'o', b'k']];
        let mut r = SegReader::new(&byte);
        let decoded = read_byte_string(&mut r, LengthPrefix::U16, codepage::windows(1252));
        assert_eq!(decoded, Some(("ok".into(), TextLoss::default())));
    }

    #[test]
    fn a_string_says_whether_it_decoded() {
        let invalid = TextLoss { invalid: true, assumed: false };
        let assumed = TextLoss { invalid: false, assumed: true };
        for (case, bytes, code_page, text, loss) in [
            ("an undefined byte", [2, 0xAA, 0xAB], 1253, "\u{FFFD}\u{AB}", invalid),
            ("plain ASCII", [2, b'o', b'k'], 1253, "ok", TextLoss::default()),
            ("a guessed code page", [2, 0xFF, 0xFE], 1361, "\u{FF}\u{FE}", assumed),
        ] {
            let segs: [&[u8]; 1] = [&bytes];
            let mut r = SegReader::new(&segs);
            let decoded = read_byte_string(&mut r, LengthPrefix::U8, codepage::windows(code_page));
            assert_eq!(decoded, Some((text.to_string(), loss)), "{case}");
        }
        let base = [vec![0u8; 10], 2u16.to_le_bytes().to_vec(), vec![0u8; 6]].concat();
        let chars = [0x01, 0x00, 0xD8, 0x00, 0xDC];
        assert_eq!(txo_text(&[&base, &chars]), Some(("\u{10000}".to_string(), false)));
    }
    #[test]
    fn empty_readable_sheets_keep_their_original_positions_after_hidden_sheets() {
        let wb = Wb {
            sheets: vec![
                ("Hidden", 1, vec![]),
                ("Empty", 0, vec![]),
                ("Shown", 0, label(0, 0, 0, "kept")),
            ],
            ..Wb::default()
        };
        let document = parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(unit_starts(&document), [(UnitKind::Sheet, 2), (UnitKind::Sheet, 3)]);
    }
}
