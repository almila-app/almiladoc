//! Legacy Word 97-2003 binary (.doc), implementing the published resolution
//! algorithms: OLE2 container, FIB, piece table with `Prm`s, CHPX/PAPX runs
//! applied over the STSH style chains in specification order, and the
//! PlfLst/PlfLfo list tables. Character positions are counted in UTF-16
//! units, matching the CP-indexed PLC structures.

mod lists;
mod sprm;
mod stsh;

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{
    Block, Document, ImageSource, Inline, LinkTarget, Note, NoteKind, Style, inlines_are_empty,
    inlines_to_plain_text,
};
use crate::shared::assets::AssetSink;
use crate::shared::binary::{
    OptionalStream, get_u16, get_u32, optional_ole_stream, read_ole_stream, utf16le_units,
};
use crate::shared::blockstyle::StyledRun;
use crate::shared::delta::rebase_emphasis;
use crate::shared::fields::{FieldFrame, field_result, hyperlink_target};
use crate::shared::grid::{CellProp, GridRow, build_edge_table};
use crate::shared::list::MarkerKind;
use crate::shared::list::{ListEntry, ListKey, flush_list};
use crate::shared::officeart::{record_at, records};
use lists::{LEVELS, ListDef, Lists};
use sprm::{PapDelta, Tap, apply_chpx, apply_pap_sprms, chpx_istd};
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use stsh::Stylesheet;

pub fn parse(parse_ctx: &ParseCtx, bytes: &[u8]) -> Result<Document, ConvertError> {
    let cursor = Cursor::new(bytes);
    let mut ole = cfb::CompoundFile::open(cursor)
        .map_err(|_| ConvertError::malformed("not an OLE2 compound file"))?;

    let word_doc = read_ole_stream(parse_ctx, &mut ole, "WordDocument")?;
    if get_u16(&word_doc, 0) != Some(0xA5EC) {
        return Err(ConvertError::malformed_part("WordDocument", "invalid FIB magic"));
    }
    let flags = get_u16(&word_doc, 0x0A).unwrap_or(0);
    if flags & 0x0100 != 0 {
        return Err(ConvertError::Encrypted);
    }
    let (table_name, other_table) =
        if flags & 0x0200 != 0 { ("1Table", "0Table") } else { ("0Table", "1Table") };
    let table = match optional_ole_stream(parse_ctx, &mut ole, table_name)? {
        OptionalStream::Absent => optional_ole_stream(parse_ctx, &mut ole, other_table)?.or_empty(),
        stream => stream.or_empty(),
    };

    let ccp = |off| get_u32(&word_doc, off).unwrap_or(0) as usize;
    let (ccp_text, ccp_ftn, ccp_hdd) = (ccp(0x4C), ccp(0x50), ccp(0x54));
    let (ccp_mcr, ccp_atn, ccp_edn) = (ccp(0x58), ccp(0x5C), ccp(0x60));
    let fc_clx = get_u32(&word_doc, 0x1A2).unwrap_or(0) as usize;
    let lcb_clx = get_u32(&word_doc, 0x1A6).unwrap_or(0) as usize;

    let (pieces, prcs) = if lcb_clx > 0 {
        parse_clx(parse_ctx, &table, fc_clx, lcb_clx)?
    } else {
        (legacy_single_piece(&word_doc), Vec::new())
    };
    let total_cp = ccp_text + ccp_ftn + ccp_hdd + ccp_mcr + ccp_atn + ccp_edn;
    let edn_base = ccp_text + ccp_ftn + ccp_hdd + ccp_mcr + ccp_atn;
    let stories = Stories {
        main_end: ccp_text,
        notes: [ccp_text..ccp_text + ccp_ftn, edn_base..edn_base + ccp_edn],
    };
    // Compressed (8-bit) piece text decodes in the document's ANSI code
    // page: FibBase.lid, or FibRgW97.lidFE when fFarEast is set.
    let lid = if flags & 0x4000 != 0 {
        get_u16(&word_doc, 0x3C).filter(|&l| l != 0).or_else(|| get_u16(&word_doc, 0x06))
    } else {
        get_u16(&word_doc, 0x06)
    };
    let encoding = lid_encoding(lid.unwrap_or(0));
    let text = extract_text(parse_ctx, &word_doc, &pieces, total_cp, encoding, &stories);

    let data = optional_ole_stream(parse_ctx, &mut ole, "Data")?.or_empty();
    let chpx_runs = parse_fkps(parse_ctx, &word_doc, &table, 0xFA, FkpKind::Chpx, &data)?;
    let papx_runs = parse_fkps(parse_ctx, &word_doc, &table, 0x102, FkpKind::Papx, &data)?;
    let has_lists = get_u32(&word_doc, 0x2E6).is_some_and(|lcb| lcb > 0);
    let stylesheet = stsh::parse(parse_ctx, &word_doc, &table, has_lists);
    let list_tables = lists::parse(parse_ctx, &word_doc, &table);
    report_unread_stories(parse_ctx, &word_doc, &table, [ccp_hdd, ccp_atn, ccp(0x64)]);

    // Note references (CP-indexed) and note body ranges.
    let mut note_refs: HashMap<usize, String> = HashMap::new();
    let mut note_ranges: Vec<(usize, usize, String, NoteKind)> = Vec::new();
    for (ref_off, txt_off, base, prefix, kind) in [
        (0xAA, 0xB2, ccp_text, "fn", NoteKind::Footnote),
        (0x20A, 0x212, edn_base, "en", NoteKind::Endnote),
    ] {
        let [refs, txts] = [(ref_off, 2), (txt_off, 0)].map(|(fib_off, data_size)| {
            read_plc(&word_doc, &table, fib_off, data_size).unwrap_or_else(|| {
                parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Note);
                Plc::EMPTY
            })
        });
        for i in 0..refs.n {
            note_refs.insert(text.index_of_cp(refs.cp(i)), format!("{prefix}{i}"));
            if i < txts.n {
                let lo = text.index_of_cp(base + txts.cp(i));
                let hi = text.index_of_cp(base + txts.cp(i + 1));
                note_ranges.push((lo, hi, format!("{prefix}{i}"), kind));
            }
        }
    }
    let main_end = text.index_of_cp(ccp_text);

    let piece_prcs: Vec<Option<usize>> = pieces.iter().map(|p| p.prm_prc).collect();
    let shapes = floating_shapes(parse_ctx, &word_doc, &table)?;
    let assembler = Assembler {
        parse_ctx,
        text,
        chpx: Runs::new(chpx_runs),
        papx: Runs::new(papx_runs),
        stylesheet,
        lists: list_tables,
        prcs,
        piece_prcs,
        note_refs,
        shapes,
        counters: std::cell::RefCell::new(Counters::default()),
        damage_reported: std::cell::RefCell::new(HashSet::new()),
        data,
        assets: std::cell::RefCell::new(AssetSink::new(parse_ctx)),
    };
    let blocks = assembler.build_blocks(0, main_end, Region::Body)?;
    let mut notes = Vec::new();
    for (lo, hi, id, kind) in note_ranges {
        let lo = lo.min(assembler.text.chars.len());
        let hi = hi.min(assembler.text.chars.len());
        if lo >= hi {
            continue;
        }
        notes.push(Note { id, kind, blocks: assembler.build_blocks(lo, hi, Region::Note)? });
    }
    let assets = std::mem::take(&mut assembler.assets.borrow_mut().assets);
    Ok(Document { blocks, notes, assets })
}

/// The main document's and the notes' CP ranges: the stories this reader
/// renders.
struct Stories {
    main_end: usize,
    notes: [std::ops::Range<usize>; 2],
}

impl Stories {
    /// The rendered regions the CPs `cps` reach.
    fn regions(&self, cps: std::ops::Range<usize>) -> impl Iterator<Item = Region> {
        let reaches =
            |story: &std::ops::Range<usize>| cps.start < story.end && story.start < cps.end;
        let body = reaches(&(0..self.main_end)).then_some(Region::Body);
        let note = self.notes.iter().any(reaches).then_some(Region::Note);
        body.into_iter().chain(note)
    }
}

/// A PLC from the table stream: `n + 1` CPs, then `n` data elements.
struct Plc<'t> {
    bytes: &'t [u8],
    n: usize,
    data_size: usize,
}

impl<'t> Plc<'t> {
    const EMPTY: Plc<'static> = Plc { bytes: &[], n: 0, data_size: 0 };

    fn cp(&self, i: usize) -> usize {
        get_u32(self.bytes, i * 4).unwrap_or(0) as usize
    }

    fn data(&self, i: usize) -> &'t [u8] {
        let at = (self.n + 1) * 4 + i * self.data_size;
        self.bytes.get(at..).and_then(|rest| rest.get(..self.data_size)).unwrap_or_default()
    }
}

/// The PLC whose FIB `fc`/`lcb` pair sits at `fib_off`: empty when `lcb` is
/// 0, `None` when it does not lie in the table stream or holds no whole
/// number of records.
fn read_plc<'t>(
    word_doc: &[u8],
    table: &'t [u8],
    fib_off: usize,
    data_size: usize,
) -> Option<Plc<'t>> {
    let bytes = table_range(word_doc, table, fib_off)?;
    if bytes.is_empty() {
        return Some(Plc::EMPTY);
    }
    let records = bytes.len().checked_sub(4).filter(|_| bytes.len() >= 8)?;
    let n = records / (4 + data_size);
    (records % (4 + data_size) == 0).then_some(Plc { bytes, n, data_size })
}

/// The table-stream bytes the FIB `fc`/`lcb` pair at `fib_off` names: empty
/// when `lcb` is 0, `None` when they do not lie in the table stream.
fn table_range<'t>(word_doc: &[u8], table: &'t [u8], fib_off: usize) -> Option<&'t [u8]> {
    let fc = get_u32(word_doc, fib_off).unwrap_or(0) as usize;
    let lcb = get_u32(word_doc, fib_off + 4).unwrap_or(0) as usize;
    if lcb == 0 {
        return Some(&[]);
    }
    table.get(fc..fc.saturating_add(lcb))
}

/// Count the header and footer stories and the comments, which this reader
/// does not render, and the body text-box stories, which it never reads,
/// from the story lengths `ccpHdd`, `ccpAtn` and `ccpTxbx`; a story whose
/// table does not read counts once.
fn report_unread_stories(parse_ctx: &ParseCtx, word_doc: &[u8], table: &[u8], ccps: [usize; 3]) {
    let [ccp_hdd, ccp_atn, ccp_txbx] = ccps;
    match read_plc(word_doc, table, 0xF2, 0) {
        Some(hdd) => {
            let stories = hdd.n.saturating_sub(1);
            let sections = stories.saturating_sub(6) / 6;
            for story in (6..6 + sections * 6).filter(|&i| hdd.cp(i + 1) > hdd.cp(i)) {
                match (story - 6) % 6 {
                    0 | 1 | 4 => parse_ctx.emit(DiagnosticCode::HeaderOmitted, Region::Header),
                    _ => parse_ctx.emit(DiagnosticCode::FooterOmitted, Region::Footer),
                }
            }
        }
        None if ccp_hdd > 0 => parse_ctx.emit(DiagnosticCode::HeaderOmitted, Region::Header),
        None => {}
    }
    match read_plc(word_doc, table, 0xBA, 30) {
        Some(comments) => {
            parse_ctx.emit_count(DiagnosticCode::CommentOmitted, Region::Comment, comments.n as u64)
        }
        None if ccp_atn > 0 => parse_ctx.emit(DiagnosticCode::CommentOmitted, Region::Comment),
        None => {}
    }
    if ccp_txbx > 0 {
        let boxes = read_plc(word_doc, table, 0x25A, 22).map_or(0, |txbx| {
            (0..txbx.n.saturating_sub(1)).filter(|&i| get_u16(txbx.data(i), 8) == Some(0)).count()
        });
        parse_ctx.emit_count(DiagnosticCode::TextBoxUnread, Region::Body, boxes.max(1) as u64);
    }
}

/// What a floating shape the main document anchors shows.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ShapeKind {
    Picture,
    TextBox,
    Graphic,
}

/// The kind of shape each main-document anchor CP names, from `PlcSpaMom`
/// and the main drawing of the `OfficeArtContent`.
fn floating_shapes(
    parse_ctx: &ParseCtx,
    word_doc: &[u8],
    table: &[u8],
) -> Result<HashMap<usize, ShapeKind>, ConvertError> {
    let Some(spa) = read_plc(word_doc, table, 0x1DA, 26).filter(|spa| spa.n > 0) else {
        return Ok(HashMap::new());
    };
    let kinds = table_range(word_doc, table, 0x22A)
        .and_then(main_drawing)
        .map_or(Ok(HashMap::new()), |drawing| shape_kinds(parse_ctx, drawing))?;
    Ok((0..spa.n)
        .filter_map(|i| {
            let spid = get_u32(spa.data(i), 0)?;
            Some((spa.cp(i), kinds.get(&spid).copied().unwrap_or(ShapeKind::Graphic)))
        })
        .collect())
}

/// The main document's `OfficeArtDgContainer` in an `OfficeArtContent`: the
/// drawing whose `dgglbl` is 0, after the `OfficeArtDggContainer`.
fn main_drawing(content: &[u8]) -> Option<&[u8]> {
    let (_, _, dgg) = record_at(content, 0)?;
    let mut rest = content.get(8 + dgg.len()..)?;
    loop {
        let (&dgglbl, drawing) = rest.split_first()?;
        let (_, _, body) = record_at(drawing, 0)?;
        let (container, next) = drawing.split_at_checked(8 + body.len())?;
        if dgglbl == 0 {
            return Some(container);
        }
        rest = next;
    }
}

/// Each shape's kind by `spid` (0 when it has none), over every shape
/// container in the OfficeArt records of `run`.
fn shape_kinds(parse_ctx: &ParseCtx, run: &[u8]) -> Result<HashMap<u32, ShapeKind>, ConvertError> {
    let mut kinds = HashMap::new();
    let mut pending = vec![run];
    while let Some(container) = pending.pop() {
        for (ver_inst, rec_type, body) in records(container) {
            parse_ctx.charge(1)?;
            if rec_type == 0xF004 {
                let mut spid = None;
                let mut kind = ShapeKind::Graphic;
                for (ver_inst, rec_type, body) in records(body) {
                    parse_ctx.charge(1)?;
                    match rec_type {
                        0xF00A => spid = get_u32(body, 0),
                        0xF00B | 0xF122 => kind = shape_kind(kind, ver_inst >> 4, body),
                        _ => {}
                    }
                }
                kinds.insert(spid.unwrap_or(0), kind);
            } else if ver_inst & 0xF == 0xF {
                pending.push(body);
            }
        }
    }
    Ok(kinds)
}

/// `kind` updated by an OfficeArt property table of `count` entries: a
/// `pib` makes a picture, an `lTxid` a text box.
fn shape_kind(kind: ShapeKind, count: u16, props: &[u8]) -> ShapeKind {
    (0..usize::from(count)).filter_map(|i| get_u16(props, i * 6)).fold(kind, |kind, id| {
        match (id & 0x3FFF, kind) {
            (0x0104, _) => ShapeKind::Picture,
            (0x0080, ShapeKind::Graphic) => ShapeKind::TextBox,
            _ => kind,
        }
    })
}

// ---------------------------------------------------------------------------
// Piece table

struct Piece {
    cp_start: usize,
    cp_end: usize,
    fc: usize,
    compressed: bool,
    /// Property modifier: an index into the Clx `Prc` array, when complex.
    prm_prc: Option<usize>,
}

fn parse_clx(
    parse_ctx: &ParseCtx,
    table: &[u8],
    fc: usize,
    lcb: usize,
) -> Result<(Vec<Piece>, Vec<Vec<u8>>), ConvertError> {
    let clx = table
        .get(fc..)
        .and_then(|rest| rest.get(..lcb))
        .ok_or_else(|| ConvertError::malformed("Clx out of bounds"))?;
    let mut prcs: Vec<Vec<u8>> = Vec::new();
    let mut pos = 0;
    loop {
        let rest = clx.get(pos..).ok_or_else(|| ConvertError::malformed("malformed Clx"))?;
        match rest.first() {
            Some(1) => {
                let cb =
                    get_u16(rest, 1).ok_or_else(|| ConvertError::malformed("bad Prc"))? as usize;
                if let Some(grpprl) = rest.get(3..).and_then(|payload| payload.get(..cb)) {
                    prcs.push(grpprl.to_vec());
                }
                pos += 3 + cb;
            }
            Some(2) => {
                let lcb_plc =
                    get_u32(rest, 1).ok_or_else(|| ConvertError::malformed("bad Pcdt"))? as usize;
                let plc = rest
                    .get(5..)
                    .and_then(|payload| payload.get(..lcb_plc))
                    .ok_or_else(|| ConvertError::malformed("PlcPcd out of bounds"))?;
                let pieces = parse_plc_pcd(parse_ctx, plc, &mut prcs)?;
                return Ok((pieces, prcs));
            }
            _ => return Err(ConvertError::malformed("malformed Clx")),
        }
    }
}

fn parse_plc_pcd(
    parse_ctx: &ParseCtx,
    plc: &[u8],
    prcs: &mut Vec<Vec<u8>>,
) -> Result<Vec<Piece>, ConvertError> {
    if plc.len() < 4 + 12 {
        return Err(ConvertError::malformed("empty piece table"));
    }
    let n = (plc.len() - 4) / 12;
    let mut pieces = Vec::with_capacity(n);
    for i in 0..n {
        let cp_start =
            get_u32(plc, i * 4).ok_or_else(|| ConvertError::malformed("bad cp"))? as usize;
        let cp_end =
            get_u32(plc, (i + 1) * 4).ok_or_else(|| ConvertError::malformed("bad cp"))? as usize;
        let pcd_off = (n + 1) * 4 + i * 8;
        let fc_raw = get_u32(plc, pcd_off + 2).ok_or_else(|| ConvertError::malformed("bad pcd"))?;
        let prm = get_u16(plc, pcd_off + 6).unwrap_or(0);
        let compressed = fc_raw & 0x4000_0000 != 0;
        let fc = (fc_raw & 0x3FFF_FFFF) as usize;
        let fc = if compressed { fc / 2 } else { fc };
        // Prm: the complex form points into the Prc array; the compressed
        // form (Prm0) carries one (isprm, val) pair that decodes to a
        // one-sprm grpprl.
        let prm_prc = if prm & 1 != 0 {
            let idx = (prm >> 1) as usize;
            (idx < prcs.len()).then_some(idx)
        } else if prm != 0 {
            match prm0_grpprl(prm) {
                Some(grpprl) => {
                    prcs.push(grpprl);
                    Some(prcs.len() - 1)
                }
                None => {
                    parse_ctx.emit(DiagnosticCode::FormattingUnreadable, Region::Body);
                    None
                }
            }
        } else {
            None
        };
        pieces.push(Piece { cp_start, cp_end, fc, compressed, prm_prc });
    }
    Ok(pieces)
}

/// Decode a `Prm0` (compressed piece Prm): the 7-bit `isprm` selects a Sprm
/// per the [MS-DOC] Prm0 table, `val` is its one-byte operand. Only sprms
/// whose properties the converter models materialize; the rest have no
/// representable effect.
fn prm0_grpprl(prm: u16) -> Option<Vec<u8>> {
    let isprm = (prm >> 1) & 0x7F;
    let val = (prm >> 8) as u8;
    let sprm: u16 = match isprm {
        0x0C => 0x260A, // sprmPIlvl
        0x18 => 0x2416, // sprmPFInTable
        0x19 => 0x2417, // sprmPFTtp
        0x55 => 0x0835, // sprmCFBold
        0x56 => 0x0836, // sprmCFItalic
        0x57 => 0x0837, // sprmCFStrike
        0x78 => 0x2640, // sprmPOutLvl
        _ => return None,
    };
    Some(vec![(sprm & 0xFF) as u8, (sprm >> 8) as u8, val])
}

fn legacy_single_piece(word_doc: &[u8]) -> Vec<Piece> {
    let fc_min = get_u32(word_doc, 0x18).unwrap_or(0) as usize;
    let fc_mac = get_u32(word_doc, 0x1C).unwrap_or(0) as usize;
    if fc_mac <= fc_min {
        return Vec::new();
    }
    vec![Piece {
        cp_start: 0,
        cp_end: fc_mac - fc_min,
        fc: fc_min,
        compressed: true,
        prm_prc: None,
    }]
}

// ---------------------------------------------------------------------------
// Text extraction: CPs count UTF-16 units, chars are decoded scalars.

struct TextStream {
    chars: Vec<char>,
    fcs: Vec<u32>,
    /// CP of each char (running UTF-16 unit count).
    cps: Vec<u32>,
    /// Piece index of each char, for Prm application.
    piece_of: Vec<u32>,
    /// Whether each char stands in for bytes that did not decode.
    replaced: Vec<bool>,
}

impl TextStream {
    /// First char index at or after the given CP (`chars.len()` when the CP
    /// is past the end).
    fn index_of_cp(&self, cp: usize) -> usize {
        self.cps.partition_point(|&c| (c as usize) < cp)
    }
}

/// The ANSI code page for a Word language id (MS-LCID primary language;
/// Chinese needs the region to pick Simplified vs Traditional).
fn lid_encoding(lid: u16) -> &'static encoding_rs::Encoding {
    use encoding_rs::*;
    match lid & 0x03FF {
        0x11 => SHIFT_JIS, // 932
        0x12 => EUC_KR,    // 949
        0x04 => match lid {
            0x0404 | 0x0C04 | 0x1404 | 0x7C04 => BIG5, // 950
            _ => GBK,                                  // 936
        },
        0x01 | 0x20 | 0x29 => WINDOWS_1256, // Arabic script
        0x02 | 0x19 | 0x22 | 0x23 => WINDOWS_1251, // Cyrillic
        0x05 | 0x0E | 0x15 | 0x18 | 0x1A | 0x1B | 0x24 => WINDOWS_1250,
        0x08 => WINDOWS_1253,        // Greek
        0x0D => WINDOWS_1255,        // Hebrew
        0x1E => WINDOWS_874,         // Thai
        0x1F | 0x2C => WINDOWS_1254, // Turkic
        0x25..=0x27 => WINDOWS_1257, // Baltic
        0x2A => WINDOWS_1258,        // Vietnamese
        _ => WINDOWS_1252,
    }
}

/// Whether `b` starts a two-byte sequence in a double-byte code page.
fn is_lead_byte(enc: &'static encoding_rs::Encoding, b: u8) -> bool {
    use encoding_rs::*;
    if enc == SHIFT_JIS {
        matches!(b, 0x81..=0x9F | 0xE0..=0xFC)
    } else if enc == GBK || enc == BIG5 || enc == EUC_KR {
        (0x81..=0xFE).contains(&b)
    } else {
        false
    }
}

/// The text of the pieces up to `total_cp`. A piece in a rendered story
/// whose bytes lie outside the stream is `truncated_records`.
fn extract_text(
    parse_ctx: &ParseCtx,
    word_doc: &[u8],
    pieces: &[Piece],
    total_cp: usize,
    encoding: &'static encoding_rs::Encoding,
    stories: &Stories,
) -> TextStream {
    let mut text = TextStream {
        chars: Vec::new(),
        fcs: Vec::new(),
        cps: Vec::new(),
        piece_of: Vec::new(),
        replaced: Vec::new(),
    };
    let mut cp = 0usize;
    for (piece_idx, piece) in pieces.iter().enumerate() {
        if cp >= total_cp {
            break;
        }
        let len = piece.cp_end.saturating_sub(piece.cp_start).min(total_cp - cp);
        let byte_len = if piece.compressed { Some(len) } else { len.checked_mul(2) };
        let Some(bytes) =
            byte_len.and_then(|n| word_doc.get(piece.fc..).and_then(|rest| rest.get(..n)))
        else {
            for region in stories.regions(cp..cp + len) {
                parse_ctx.emit(DiagnosticCode::TruncatedRecords, region);
            }
            cp += len;
            continue;
        };
        let mut push = |c: char, fc: usize, cp: usize, replaced: bool| {
            text.chars.push(c);
            text.fcs.push(fc as u32);
            text.cps.push(cp as u32);
            text.piece_of.push(piece_idx as u32);
            text.replaced.push(replaced);
        };
        if piece.compressed {
            // Byte-accurate accounting: in a compressed piece each CP is one
            // byte, so a double-byte character occupies two CPs and its FC is
            // the lead byte's.
            let mut i = 0usize;
            while i < bytes.len() {
                let seq =
                    if is_lead_byte(encoding, bytes[i]) && i + 1 < bytes.len() { 2 } else { 1 };
                let (s, replaced) = encoding.decode_without_bom_handling(&bytes[i..i + seq]);
                for c in s.chars() {
                    push(c, piece.fc + i, cp, replaced);
                }
                cp += seq;
                i += seq;
            }
        } else {
            let mut unit_idx = 0usize;
            for unit in char::decode_utf16(utf16le_units(bytes)) {
                let replaced = unit.is_err();
                let c = unit.unwrap_or(char::REPLACEMENT_CHARACTER);
                push(c, piece.fc + unit_idx * 2, cp, replaced);
                unit_idx += c.len_utf16();
                cp += c.len_utf16();
            }
        }
    }
    text
}

// ---------------------------------------------------------------------------
// Formatting runs (CHPX / PAPX out of FKP pages)

#[derive(Clone, Copy, PartialEq)]
enum FkpKind {
    Chpx,
    Papx,
}

#[derive(Default)]
struct RunProps {
    /// Raw CHPX grpprl (application depends on the style base).
    chpx: Vec<u8>,
    istd: u16,
    pap: PapDelta,
}

struct Run {
    fc_start: u32,
    fc_end: u32,
    props: RunProps,
}

struct Runs {
    runs: Vec<Run>,
}

impl Runs {
    fn new(mut runs: Vec<Run>) -> Self {
        runs.sort_by_key(|r| r.fc_start);
        Runs { runs }
    }

    fn lookup(&self, fc: u32) -> Option<&RunProps> {
        let idx = self.runs.partition_point(|r| r.fc_start <= fc);
        if idx == 0 {
            return None;
        }
        let run = &self.runs[idx - 1];
        (fc < run.fc_end).then_some(&run.props)
    }
}

fn parse_fkps(
    parse_ctx: &ParseCtx,
    word_doc: &[u8],
    table: &[u8],
    fib_off: usize,
    kind: FkpKind,
    data: &[u8],
) -> Result<Vec<Run>, ConvertError> {
    let mut runs = Vec::new();
    let Some(bin_table) = read_plc(word_doc, table, fib_off, 4) else {
        parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Body);
        return Ok(runs);
    };
    for i in 0..bin_table.n {
        let pn = get_u32(bin_table.data(i), 0).unwrap_or(0) & 0x3F_FFFF;
        let page = (pn as usize)
            .checked_mul(512)
            .and_then(|off| word_doc.get(off..))
            .and_then(|rest| rest.get(..512));
        match page {
            Some(page) => parse_fkp_page(parse_ctx, page, kind, data, &mut runs)?,
            None => parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Body),
        }
    }
    Ok(runs)
}

fn parse_fkp_page(
    parse_ctx: &ParseCtx,
    page: &[u8],
    kind: FkpKind,
    data: &[u8],
    runs: &mut Vec<Run>,
) -> Result<(), ConvertError> {
    let count = page[511] as usize;
    if count == 0 {
        return Ok(());
    }
    let entry_size = if kind == FkpKind::Papx { 13 } else { 1 };
    for k in 0..count {
        parse_ctx.charge(1)?;
        let Some(fc_start) = get_u32(page, k * 4) else {
            continue;
        };
        let Some(fc_end) = get_u32(page, (k + 1) * 4) else {
            continue;
        };
        let b_offset_pos = (count + 1) * 4 + k * entry_size;
        let Some(&b_offset) = page.get(b_offset_pos) else {
            continue;
        };
        let mut props = RunProps::default();
        if b_offset != 0 {
            let off = b_offset as usize * 2;
            match kind {
                FkpKind::Chpx => {
                    if let Some(&cb) = page.get(off)
                        && let Some(grpprl) = page
                            .get(off..)
                            .and_then(|rest| rest.get(1..))
                            .and_then(|rest| rest.get(..cb as usize))
                    {
                        props.chpx = grpprl.to_vec();
                    }
                }
                FkpKind::Papx => {
                    if let Some(&cb) = page.get(off) {
                        let (start, len) = if cb == 0 {
                            let cb2 = page.get(off + 1).copied().unwrap_or(0) as usize;
                            (off + 2, cb2 * 2)
                        } else {
                            (off + 1, cb as usize * 2 - 1)
                        };
                        if let Some(grpprl) = page.get(start..).and_then(|rest| rest.get(..len))
                            && grpprl.len() >= 2
                        {
                            props.istd = u16::from_le_bytes([grpprl[0], grpprl[1]]);
                            apply_pap_sprms(&grpprl[2..], data, &mut props.pap);
                        }
                    }
                }
            }
        }
        runs.push(Run { fc_start, fc_end, props });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Numbering counters ([MS-DOC] number sequence semantics)

#[derive(Default)]
struct Counters {
    /// Numbering state keyed by list identity (`lsid`): every LFO
    /// referencing the same list continues its sequence.
    state: HashMap<u32, ([u64; LEVELS], [bool; LEVELS])>,
    /// (ilfo, level) pairs whose first-use start-at override has fired.
    overridden: std::collections::HashSet<(u16, usize)>,
}

impl Counters {
    /// Advance the sequence for a paragraph numbered by (`ilfo`, `level`)
    /// and return its effective number plus, when the level's number text is
    /// not reproducible from the marker kind alone, the composite label.
    fn next(&mut self, ilfo: u16, list: &ListDef, level: usize) -> (u64, Option<String>) {
        let level = level.min(LEVELS - 1);
        let def = &list.levels[level];
        let first_use = self.overridden.insert((ilfo, level));
        let (values, started) = self.state.entry(list.lsid).or_default();
        let value = match list.start_override[level] {
            // An LFO start-at override restarts the shared sequence at its
            // value the first time that LFO numbers this level.
            Some(s) if first_use => s,
            _ if started[level] => values[level].saturating_add(1),
            _ => def.start,
        };
        values[level] = value;
        started[level] = true;
        // Using this level restarts deeper levels according to their own
        // restart rules (fNoRestart/ilvlRestartLim).
        for (deeper, deeper_def) in list.levels.iter().enumerate().skip(level + 1) {
            let triggered = match deeper_def.restart {
                None => true,
                Some(lim) => (level as u32) < lim,
            };
            if triggered {
                started[deeper] = false;
            }
        }
        let label = composite_label(list, values, started, level);
        (value, label)
    }
}

/// Render a level's number text against the current sequence values;
/// `None` when it matches the default label the renderer produces anyway.
fn composite_label(
    list: &ListDef,
    values: &[u64; LEVELS],
    started: &[bool; LEVELS],
    level: usize,
) -> Option<String> {
    let def = &list.levels[level];
    let marker = def.marker?;
    crate::shared::numbering::composite_label(
        &def.pattern,
        marker,
        values[level],
        |l| list.levels[l.min(LEVELS - 1)].marker.unwrap_or(MarkerKind::Decimal),
        |l| {
            let l = l.min(LEVELS - 1);
            if started[l] { values[l] } else { list.levels[l].start }
        },
    )
}

// ---------------------------------------------------------------------------
// Assembly: text stream + formatting runs -> model

struct Assembler<'c> {
    parse_ctx: &'c ParseCtx<'c>,
    text: TextStream,
    chpx: Runs,
    papx: Runs,
    stylesheet: Stylesheet,
    lists: Lists,
    prcs: Vec<Vec<u8>>,
    piece_prcs: Vec<Option<usize>>,
    note_refs: HashMap<usize, String>,
    /// The floating shape each main-document anchor CP names.
    shapes: HashMap<usize, ShapeKind>,
    counters: std::cell::RefCell<Counters>,
    /// The pieces and regions already reported `invalid_encoding`.
    damage_reported: std::cell::RefCell<HashSet<(u32, Region)>>,
    /// The Data stream, for `sprmCPicLocation` picture payloads.
    data: Vec<u8>,
    assets: std::cell::RefCell<AssetSink>,
}

/// A paragraph's resolved properties: the style chain's contribution merged
/// with the PAPX and any piece `Prm`.
struct EffectivePap {
    istd: u16,
    effective: PapDelta,
}

/// An open field and the pieces whose undecodable text its instruction and
/// its result hold.
#[derive(Default)]
struct OpenField {
    frame: FieldFrame,
    instr_damage: Vec<u32>,
    result_damage: Vec<u32>,
}

struct ParaBuilder {
    inlines: Vec<Inline>,
    fields: Vec<OpenField>,
    text: String,
    style: Style,
    /// The pieces whose undecodable text `text` holds.
    text_damage: Vec<u32>,
    /// The pieces whose undecodable text `inlines` holds.
    damage: Vec<u32>,
}

impl ParaBuilder {
    fn new() -> Self {
        ParaBuilder {
            inlines: Vec::new(),
            fields: Vec::new(),
            text: String::new(),
            style: Style::PLAIN,
            text_damage: Vec::new(),
            damage: Vec::new(),
        }
    }

    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        let damage = std::mem::take(&mut self.text_damage);
        let inline = Inline::Text { text, style: self.style };
        match self.fields.last_mut() {
            Some(f) if !f.frame.in_result => {
                f.frame.instr.push_str(&inlines_to_plain_text(&[inline]));
                f.instr_damage.extend(damage);
            }
            Some(f) => {
                f.frame.inlines.push(inline);
                f.result_damage.extend(damage);
            }
            None => {
                self.inlines.push(inline);
                self.damage.extend(damage);
            }
        }
    }

    /// Push `c`, which stands in for undecodable bytes of piece `damaged`
    /// when one is given.
    fn push_char(&mut self, c: char, style: Style, damaged: Option<u32>) {
        if style != self.style {
            self.flush_text();
            self.style = style;
        }
        self.text.push(c);
        self.text_damage.extend(damaged);
    }

    fn push_inline(&mut self, inline: Inline) {
        self.flush_text();
        match self.fields.last_mut() {
            Some(f) if !f.frame.in_result => {}
            Some(f) => f.frame.inlines.push(inline),
            None => self.inlines.push(inline),
        }
    }

    fn field_begin(&mut self) {
        self.flush_text();
        self.fields.push(OpenField::default());
    }

    fn field_separate(&mut self) {
        self.flush_text();
        if let Some(f) = self.fields.last_mut() {
            f.frame.in_result = true;
        }
    }

    /// Whether the innermost open field is in the result of a `SHAPE`
    /// field, the inline stand-in of the floating shape its `\x08` anchors.
    fn in_shape_result(&self) -> bool {
        self.fields.last().is_some_and(|f| {
            let keyword = f.frame.instr.split_whitespace().next().unwrap_or_default();
            f.frame.in_result && keyword.eq_ignore_ascii_case("SHAPE")
        })
    }

    fn field_end(&mut self) {
        self.flush_text();
        let Some(OpenField { frame, instr_damage, mut result_damage }) = self.fields.pop() else {
            return;
        };
        let target_damaged = matches!(
            hyperlink_target(&frame.instr),
            Some(LinkTarget::External(t) | LinkTarget::Relative(t) | LinkTarget::Anchor(t))
                if t.contains(char::REPLACEMENT_CHARACTER)
        );
        if target_damaged && !inlines_are_empty(&frame.inlines) {
            result_damage.extend(instr_damage);
        }
        let inlines = field_result(&frame.instr, frame.inlines);
        match self.fields.last_mut() {
            Some(f) if !f.frame.in_result => {}
            Some(f) => {
                f.frame.inlines.extend(inlines);
                f.result_damage.extend(result_damage);
            }
            None => {
                self.inlines.extend(inlines);
                self.damage.extend(result_damage);
            }
        }
    }

    /// The paragraph's inlines, and the pieces whose undecodable text they
    /// hold.
    fn finish(mut self) -> (Vec<Inline>, Vec<u32>) {
        self.flush_text();
        while !self.fields.is_empty() {
            self.field_end();
        }
        (self.inlines, self.damage)
    }
}

impl Assembler<'_> {
    fn build_blocks(
        &self,
        lo: usize,
        hi: usize,
        region: Region,
    ) -> Result<Vec<Block>, ConvertError> {
        let mut blocks: Vec<Block> = Vec::new();
        let mut list_run: Vec<ListEntry> = Vec::new();
        let mut styled = StyledRun::default();
        let mut cell_blocks: Vec<Block> = Vec::new();
        let mut cell_styled = StyledRun::default();
        let mut row: Vec<Vec<Block>> = Vec::new();
        let mut table_rows: Vec<DocRow> = Vec::new();
        let mut para = ParaBuilder::new();

        let mut i = lo;
        while i < hi.min(self.text.chars.len()) {
            let c = self.text.chars[i];
            let fc = self.text.fcs[i];
            if let Some(id) = self.note_refs.get(&i) {
                para.push_inline(Inline::NoteRef(id.clone()));
                i += 1;
                continue;
            }
            match c {
                '\r' | '\u{7}' | '\u{c}' | '\u{e}' => {
                    let pap = self.effective_pap(fc, i);
                    let (inlines, damage) =
                        std::mem::replace(&mut para, ParaBuilder::new()).finish();
                    self.report_damage(&damage, region);
                    let is_cell_mark = c == '\u{7}';
                    if pap.effective.in_table.unwrap_or(false) || is_cell_mark {
                        // A table is a hard boundary for top-level list and
                        // styled runs, even while its rows are accumulated.
                        styled.flush(&mut blocks);
                        flush_list(&mut blocks, &mut list_run);
                        // Nested-table content (table depth > 1, inner
                        // cell/row terminators) flattens into the outer
                        // cell as paragraphs.
                        let inner = pap.effective.itap.unwrap_or(1) > 1
                            || pap.effective.inner_cell.unwrap_or(false)
                            || pap.effective.inner_ttp.unwrap_or(false);
                        if inner {
                            self.emit_cell_paragraph(
                                &pap,
                                inlines,
                                &mut cell_blocks,
                                &mut cell_styled,
                            );
                        } else if is_cell_mark && pap.effective.ttp.unwrap_or(false) {
                            cell_styled.flush(&mut cell_blocks);
                            // Row end: the TTP mark's PAPX carries the TAP
                            // (boundaries, merge flags, header row).
                            if !row.is_empty() {
                                table_rows.push(DocRow {
                                    cells: std::mem::take(&mut row),
                                    tap: pap.effective.tap.clone(),
                                });
                            }
                        } else if is_cell_mark {
                            self.emit_cell_paragraph(
                                &pap,
                                inlines,
                                &mut cell_blocks,
                                &mut cell_styled,
                            );
                            cell_styled.flush(&mut cell_blocks);
                            row.push(std::mem::take(&mut cell_blocks));
                        } else {
                            self.emit_cell_paragraph(
                                &pap,
                                inlines,
                                &mut cell_blocks,
                                &mut cell_styled,
                            );
                        }
                    } else {
                        self.flush_table(&mut blocks, &mut table_rows, &mut row, &mut cell_blocks)?;
                        self.emit_paragraph(&pap, inlines, &mut blocks, &mut list_run, &mut styled);
                    }
                }
                '\u{b}' => para.push_inline(Inline::LineBreak),
                '\u{13}' => para.field_begin(),
                '\u{14}' => para.field_separate(),
                '\u{15}' => {
                    if para.fields.last().is_some_and(|f| is_choice_form_field(&f.frame.instr)) {
                        self.parse_ctx.emit(DiagnosticCode::FormControlUnread, region);
                    }
                    para.field_end();
                }
                '\t' => {
                    let style = self.char_style(fc, i);
                    para.push_char(' ', style, None);
                }
                '\u{1e}' => {
                    let style = self.char_style(fc, i);
                    para.push_char('-', style, None);
                }
                // An embedded object's location names its ObjectPool
                // storage, not a picture.
                '\u{1}' if self.chpx.lookup(fc).is_some_and(|p| sets_flag(&p.chpx, 0x080A)) => {
                    self.parse_ctx.emit(DiagnosticCode::EmbeddedObject, region);
                }
                // Field data (a hyperlink's or form field's), not a picture.
                '\u{1}' if self.chpx.lookup(fc).is_some_and(|p| sets_flag(&p.chpx, 0x0806)) => {}
                // Inline picture special character: extract the payload
                // pointed at by sprmCPicLocation.
                '\u{1}' => {
                    if let Some(image) = self.picture_at(fc, region, para.in_shape_result())? {
                        para.push_inline(image);
                    }
                }
                '\u{8}' => {
                    let cp = self.text.cps.get(i).map_or(0, |&cp| cp as usize);
                    match self.shapes.get(&cp).copied().unwrap_or(ShapeKind::Graphic) {
                        ShapeKind::Picture => {
                            self.parse_ctx.emit(DiagnosticCode::ImageUnplaced, region);
                        }
                        ShapeKind::Graphic => {
                            self.parse_ctx.emit(DiagnosticCode::GraphicUnread, region);
                        }
                        ShapeKind::TextBox => {}
                    }
                }
                '\u{2}' | '\u{5}' | '\u{1f}' => {}
                c if c.is_control() => {}
                c => {
                    let style = self.char_style(fc, i);
                    let damaged = self.text.replaced.get(i).is_some_and(|&r| r);
                    para.push_char(c, style, damaged.then(|| self.text.piece_of[i]));
                }
            }
            i += 1;
        }
        let (inlines, damage) = para.finish();
        self.report_damage(&damage, region);
        cell_styled.flush(&mut cell_blocks);
        self.flush_table(&mut blocks, &mut table_rows, &mut row, &mut cell_blocks)?;
        if !inlines_are_empty(&inlines) {
            styled.flush(&mut blocks);
            flush_list(&mut blocks, &mut list_run);
            blocks.push(Block::Paragraph(inlines));
        }
        styled.flush(&mut blocks);
        flush_list(&mut blocks, &mut list_run);
        Ok(blocks)
    }

    /// Effective character style in specification order: paragraph/character
    /// style chain -> CHPX (toggles vs the style base) -> piece Prm.
    fn char_style(&self, fc: u32, char_index: usize) -> Style {
        let para_istd = self.papx.lookup(fc).map(|p| p.istd).unwrap_or(0);
        let chpx = self.chpx.lookup(fc).map(|p| p.chpx.as_slice()).unwrap_or(&[]);
        let istd = chpx_istd(chpx).unwrap_or(para_istd);
        let base = self.stylesheet.get(istd).chp;
        let mut style = apply_chpx(chpx, base, base);
        if let Some(&piece_idx) = self.text.piece_of.get(char_index)
            && let Some(piece_prc) = self.piece_prm(piece_idx as usize)
        {
            style = apply_chpx(piece_prc, style, base);
        }
        style
    }

    fn piece_prm(&self, piece_idx: usize) -> Option<&[u8]> {
        let prc_idx = (*self.piece_prcs.get(piece_idx)?)?;
        self.prcs.get(prc_idx).map(Vec::as_slice)
    }

    /// Paragraph properties in specification order: style chain -> PAPX ->
    /// piece Prm.
    fn effective_pap(&self, fc: u32, char_index: usize) -> EffectivePap {
        let (istd, papx_delta) = match self.papx.lookup(fc) {
            Some(p) => (p.istd, p.pap.clone()),
            None => (0, PapDelta::default()),
        };
        let style = self.stylesheet.get(istd);
        let mut effective = style.pap.clone().merge(papx_delta);
        if let Some(&piece_idx) = self.text.piece_of.get(char_index)
            && let Some(prm) = self.piece_prm(piece_idx as usize)
        {
            let mut prm_delta = PapDelta::default();
            apply_pap_sprms(prm, &[], &mut prm_delta);
            effective = effective.merge(prm_delta);
        }
        EffectivePap { istd, effective }
    }

    fn emit_paragraph(
        &self,
        pap: &EffectivePap,
        inlines: Vec<Inline>,
        blocks: &mut Vec<Block>,
        list_run: &mut Vec<ListEntry>,
        styled: &mut StyledRun,
    ) {
        let style = self.stylesheet.get(pap.istd);
        // A styled container absorbs its blank paragraphs: they are the
        // blank lines of a code block.
        if let Some(block) = style.block {
            flush_list(blocks, list_run);
            styled.push(block, inlines, blocks);
            return;
        }
        if inlines_are_empty(&inlines) {
            styled.flush(blocks);
            flush_list(blocks, list_run);
            return;
        }
        let heading = style.heading.or(pap.effective.outline.flatten());
        if let Some(level) = heading {
            styled.flush(blocks);
            flush_list(blocks, list_run);
            let mut content = inlines;
            rebase_emphasis(&mut content, style.chp);
            // A numbered heading advances its sequence and keeps its number
            // visible - headings have no native numbering in the output.
            if let Some(label) = self.heading_label(pap) {
                content.insert(0, Inline::Text { text: label, style: Style::PLAIN });
            }
            blocks.push(Block::Heading { level, anchor: None, content });
            return;
        }
        // ilfo 0xF801 marks a paragraph whose list numbering is suppressed.
        let ilfo = pap.effective.ilfo.unwrap_or(0);
        if ilfo != 0 && ilfo != 0xF801 {
            let ilvl = pap.effective.ilvl.unwrap_or(0) as usize;
            let fallback;
            let list = match self.lists.get(ilfo) {
                Some(list) => list,
                None => {
                    fallback = ListDef::unknown(ilfo);
                    &fallback
                }
            };
            let def = &list.levels[ilvl.min(LEVELS - 1)];
            if let Some(marker) = def.marker {
                let (number, label) = if marker.ordered() {
                    self.counters.borrow_mut().next(ilfo, list, ilvl)
                } else {
                    (0, None)
                };
                styled.flush(blocks);
                list_run.push(ListEntry {
                    level: ilvl,
                    key: ListKey { instance: list.lsid as u64, marker },
                    number,
                    label,
                    blocks: vec![Block::Paragraph(inlines)],
                });
                return;
            }
            // Marker "none": numbering suppressed, plain paragraph.
        }
        styled.flush(blocks);
        flush_list(blocks, list_run);
        blocks.push(Block::Paragraph(inlines));
    }

    /// Emit one paragraph into the current table cell. Styled runs are
    /// scoped to the cell and are flushed before ordinary cell content.
    fn emit_cell_paragraph(
        &self,
        pap: &EffectivePap,
        inlines: Vec<Inline>,
        blocks: &mut Vec<Block>,
        styled: &mut StyledRun,
    ) {
        if let Some(block) = self.stylesheet.get(pap.istd).block {
            styled.push(block, inlines, blocks);
        } else {
            styled.flush(blocks);
            if !inlines_are_empty(&inlines) {
                blocks.push(Block::Paragraph(inlines));
            }
        }
    }

    /// `invalid_encoding` in `region` once for each of `pieces` whose
    /// undecodable text reached the output.
    fn report_damage(&self, pieces: &[u32], region: Region) {
        let mut reported = self.damage_reported.borrow_mut();
        for &piece in pieces {
            if reported.insert((piece, region)) {
                self.parse_ctx.emit(DiagnosticCode::InvalidEncoding, region);
            }
        }
    }

    /// Extract the inline picture whose PICF + OfficeArt data the character's
    /// CHPX points at (`sprmCPicLocation`), retaining it as an asset; one
    /// that yields none is `image_unavailable` in `region`, unless it is a
    /// `SHAPE` field's stand-in (`stand_in`) for a shape that is no picture.
    fn picture_at(
        &self,
        fc: u32,
        region: Region,
        stand_in: bool,
    ) -> Result<Option<Inline>, ConvertError> {
        let Some(offset) = self.chpx.lookup(fc).and_then(|p| sprm::chpx_pic_location(&p.chpx))
        else {
            return Ok(None);
        };
        let offset = offset as usize;
        let picf = get_u32(&self.data, offset).zip(get_u16(&self.data, offset + 4)).and_then(
            |(lcb, cb_header)| {
                let picf = self.data.get(offset..offset.saturating_add(lcb as usize))?;
                Some(&picf[usize::from(cb_header).min(picf.len())..])
            },
        );
        let max_bytes = self.parse_ctx.limits().max_entry_bytes;
        let blip = match picf {
            Some(art) => crate::shared::officeart::first_blip(art, max_bytes)?,
            None => None,
        };
        let Some(blip) = blip else {
            if !stand_in || self.names_a_picture(picf)? {
                self.parse_ctx.emit(DiagnosticCode::ImageUnavailable, region);
            }
            return Ok(None);
        };
        let part = format!("data/pic{offset}.{}", blip.extension);
        let id = self.assets.borrow_mut().add(blip.media_type.to_string(), part, &blip.bytes)?;
        Ok(Some(Inline::Image { alt: String::new(), source: ImageSource::Asset(id) }))
    }

    /// Whether a `SHAPE` field's stand-in that yields no picture stood for
    /// one: data that does not read, or a shape with a `pib`, or no shape at
    /// all. Any other shape is reported at the field's floating anchor.
    fn names_a_picture(&self, art: Option<&[u8]>) -> Result<bool, ConvertError> {
        let Some(art) = art else {
            return Ok(true);
        };
        let kinds = shape_kinds(self.parse_ctx, art)?;
        Ok(kinds.is_empty() || kinds.values().any(|&kind| kind == ShapeKind::Picture))
    }

    /// The visible number label of a numbered heading paragraph, with its
    /// trailing separator; advances the list sequence.
    fn heading_label(&self, pap: &EffectivePap) -> Option<String> {
        let ilfo = pap.effective.ilfo.unwrap_or(0);
        if ilfo == 0 || ilfo == 0xF801 {
            return None;
        }
        let ilvl = pap.effective.ilvl.unwrap_or(0) as usize;
        let list = self.lists.get(ilfo)?;
        let def = &list.levels[ilvl.min(LEVELS - 1)];
        let marker = def.marker?;
        if !marker.ordered() {
            return None;
        }
        let (number, label) = self.counters.borrow_mut().next(ilfo, list, ilvl);
        Some(format!("{} ", label.unwrap_or_else(|| marker.label(number))))
    }

    fn flush_table(
        &self,
        blocks: &mut Vec<Block>,
        table_rows: &mut Vec<DocRow>,
        row: &mut Vec<Vec<Block>>,
        cell_blocks: &mut Vec<Block>,
    ) -> Result<(), ConvertError> {
        if !cell_blocks.is_empty() {
            row.push(std::mem::take(cell_blocks));
        }
        if !row.is_empty() {
            table_rows.push(DocRow { cells: std::mem::take(row), tap: None });
        }
        if table_rows.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(table_rows).into_iter().map(DocRow::into_grid_row).collect();
        if let Some(block) = build_edge_table(self.parse_ctx, rows)? {
            blocks.push(block);
        }
        Ok(())
    }
}

/// Whether a CHPX sets the one-byte flag `flag`: `sprmCFOle2` marks an
/// embedded object, `sprmCFData` field data.
fn sets_flag(chpx: &[u8], flag: u16) -> bool {
    let mut set = false;
    sprm::walk_sprms(chpx, |sprm, operand| {
        if sprm == flag {
            set = operand.first().is_some_and(|&v| v != 0);
        }
    });
    set
}

/// Whether a field instruction is a check box or drop-down form field,
/// whose state lives only in its form-field data.
fn is_choice_form_field(instr: &str) -> bool {
    let keyword = instr.split_whitespace().next().unwrap_or_default();
    keyword.eq_ignore_ascii_case("FORMCHECKBOX") || keyword.eq_ignore_ascii_case("FORMDROPDOWN")
}

/// One accumulated table row: cell contents plus the TAP from the row-end
/// mark (absent for rows without `sprmTDefTable`).
struct DocRow {
    cells: Vec<Vec<Block>>,
    tap: Option<Tap>,
}

impl DocRow {
    fn into_grid_row(self) -> GridRow {
        let header = self.tap.as_ref().is_some_and(|t| t.header);
        let tap = self.tap.unwrap_or_default();
        let cells = self
            .cells
            .into_iter()
            .enumerate()
            .map(|(k, blocks)| {
                let tc = tap.cells.get(k).copied().unwrap_or_default();
                // Without a TAP boundary, a synthetic index-based edge keeps
                // the flat column behavior.
                let right =
                    tap.boundaries.get(k + 1).map(|&b| b as i64).unwrap_or((k as i64 + 1) * 1000);
                let prop = CellProp {
                    merge_first: tc.horz_first,
                    merge_cont: tc.horz_cont,
                    vmerge_first: tc.vert_restart,
                    vmerge_cont: tc.vert_cont,
                    right,
                };
                (blocks, prop)
            })
            .collect();
        GridRow { cells, header }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::numbering::NumberText;
    use lists::LevelDef;

    fn list(lsid: u32) -> ListDef {
        let mut levels: [LevelDef; LEVELS] = std::array::from_fn(|_| LevelDef::default());
        levels[0].marker = Some(MarkerKind::Decimal);
        levels[1].marker = Some(MarkerKind::LowerAlpha);
        ListDef { lsid, levels, start_override: [None; LEVELS] }
    }

    #[test]
    fn shared_lsid_continues_across_ilfos() {
        let mut c = Counters::default();
        let l = list(7);
        assert_eq!(c.next(1, &l, 0).0, 1);
        assert_eq!(c.next(2, &l, 0).0, 2);
    }

    #[test]
    fn first_use_override_restarts_the_shared_sequence() {
        let mut c = Counters::default();
        let l = list(7);
        let mut over = list(7);
        over.start_override[0] = Some(10);
        assert_eq!(c.next(1, &l, 0).0, 1);
        assert_eq!(c.next(1, &l, 0).0, 2);
        assert_eq!(c.next(2, &over, 0).0, 10); // fires on first use only
        assert_eq!(c.next(2, &over, 0).0, 11);
        assert_eq!(c.next(1, &l, 0).0, 12); // same lsid: sequence is shared
    }

    #[test]
    fn no_restart_level_survives_shallower_items() {
        let mut c = Counters::default();
        let mut l = list(7);
        l.levels[1].restart = Some(0); // fNoRestart, never restart
        assert_eq!(c.next(1, &l, 0).0, 1);
        assert_eq!(c.next(1, &l, 1).0, 1);
        assert_eq!(c.next(1, &l, 0).0, 2);
        assert_eq!(c.next(1, &l, 1).0, 2); // continues past the level-0 item
        let restarting = list(8);
        assert_eq!(c.next(3, &restarting, 0).0, 1);
        assert_eq!(c.next(3, &restarting, 1).0, 1);
        assert_eq!(c.next(3, &restarting, 0).0, 2);
        assert_eq!(c.next(3, &restarting, 1).0, 1); // default rule restarts
    }

    #[test]
    fn restart_limit_distinguishes_trigger_levels() {
        let mut c = Counters::default();
        let mut l = list(7);
        l.levels[2].marker = Some(MarkerKind::Decimal);
        l.levels[2].restart = Some(2); // restart only after levels 0 or 1
        assert_eq!(c.next(1, &l, 2).0, 1);
        c.next(1, &l, 1); // level 1 < 2: triggers a restart
        assert_eq!(c.next(1, &l, 2).0, 1);
        assert_eq!(c.next(1, &l, 2).0, 2);
    }

    #[test]
    fn composite_label_renders_parent_numbers() {
        let mut c = Counters::default();
        let mut l = list(7);
        l.levels[1].pattern.text = vec![
            NumberText::Level(0),
            NumberText::Literal("-".into()),
            NumberText::Level(1),
            NumberText::Literal(")".into()),
        ];
        c.next(1, &l, 0);
        let (n, label) = c.next(1, &l, 1);
        assert_eq!(n, 1);
        assert_eq!(label.as_deref(), Some("1-a)"));
    }

    #[test]
    fn default_number_text_produces_no_label() {
        let mut c = Counters::default();
        let mut l = list(7);
        l.levels[0].pattern.text = vec![NumberText::Level(0), NumberText::Literal(".".into())];
        assert_eq!(c.next(1, &l, 0), (1, None));
    }

    #[test]
    fn prm0_decodes_modeled_sprms() {
        // isprm 0x55 = sprmCFBold (0x0835), val 0x01, fComplex clear.
        let prm = (0x55u16 << 1) | (0x01 << 8);
        assert_eq!(prm0_grpprl(prm), Some(vec![0x35, 0x08, 0x01]));
        // isprm 0x18 = sprmPFInTable (0x2416).
        let prm = (0x18u16 << 1) | (0x01 << 8);
        assert_eq!(prm0_grpprl(prm), Some(vec![0x16, 0x24, 0x01]));
        // isprm 0x05 (sprmPJc) is outside the converted model.
        assert_eq!(prm0_grpprl(0x05 << 1), None);
    }

    #[test]
    fn tdef_table_parses_boundaries_and_merge_flags() {
        // sprmTDefTable operand: cb, 2 columns, 3 boundaries, 2 TC80s where
        // the second cell continues a vertical merge.
        let mut operand: Vec<u8> = Vec::new();
        operand.extend_from_slice(&0u16.to_le_bytes()); // cb (unused here)
        operand.push(2); // columns
        for b in [0i16, 4000, 8000] {
            operand.extend_from_slice(&b.to_le_bytes());
        }
        // TC80: tcgrf + wWidth + 4 borders.
        let tc80 = |tcgrf: u16| {
            let mut v = tcgrf.to_le_bytes().to_vec();
            v.extend_from_slice(&[0; 18]);
            v
        };
        operand.extend_from_slice(&tc80(0x3 << 5)); // vertMerge = fvmRestart
        operand.extend_from_slice(&tc80(0x1 << 5)); // vertMerge = fvmMerge
        let mut delta = PapDelta::default();
        let mut grpprl = vec![0x08, 0xD6]; // sprmTDefTable
        grpprl.extend_from_slice(&((operand.len() - 1) as u16).to_le_bytes());
        grpprl.extend_from_slice(&operand[2..]);
        apply_pap_sprms(&grpprl, &[], &mut delta);
        let tap = delta.tap.expect("TAP parsed");
        assert_eq!(tap.boundaries, vec![0, 4000, 8000]);
        assert!(tap.cells[0].vert_restart && !tap.cells[0].vert_cont);
        assert!(tap.cells[1].vert_cont && !tap.cells[1].vert_restart);
    }

    fn assembler<'c>(parse_ctx: &'c ParseCtx<'c>, runs: Vec<Run>, data: Vec<u8>) -> Assembler<'c> {
        Assembler {
            parse_ctx,
            text: TextStream {
                chars: Vec::new(),
                fcs: Vec::new(),
                cps: Vec::new(),
                piece_of: Vec::new(),
                replaced: Vec::new(),
            },
            chpx: Runs::new(runs),
            papx: Runs::new(Vec::new()),
            stylesheet: Stylesheet::default(),
            lists: Lists::default(),
            prcs: Vec::new(),
            piece_prcs: Vec::new(),
            note_refs: HashMap::new(),
            shapes: HashMap::new(),
            counters: std::cell::RefCell::new(Counters::default()),
            damage_reported: std::cell::RefCell::new(HashSet::new()),
            data,
            assets: std::cell::RefCell::new(AssetSink::new(parse_ctx)),
        }
    }

    fn picture_data(inflated: usize) -> Vec<u8> {
        let mut data = 0u32.to_le_bytes().to_vec();
        data.extend_from_slice(&6u16.to_le_bytes());
        data.extend_from_slice(&crate::shared::officeart::deflated_metafile_blip(inflated));
        let lcb = u32::try_from(data.len()).unwrap();
        data[..4].copy_from_slice(&lcb.to_le_bytes());
        data
    }

    fn pic_location_chpx() -> Vec<u8> {
        let mut chpx = vec![0x03, 0x6A];
        chpx.extend_from_slice(&0u32.to_le_bytes());
        chpx
    }

    const TEXT_FC: usize = 0x600;

    /// A DOC whose main text is `text` in one compressed legacy piece, with
    /// the FIB fields, table-stream structures and Data stream a case adds.
    struct DocBuilder {
        word_doc: Vec<u8>,
        table: Vec<u8>,
        data: Option<Vec<u8>>,
    }

    impl DocBuilder {
        fn new(text: &[u8]) -> Self {
            let len = u32::try_from(text.len()).unwrap();
            let mut word_doc = vec![0u8; TEXT_FC];
            word_doc[..2].copy_from_slice(&0xA5ECu16.to_le_bytes());
            word_doc.extend_from_slice(text);
            let doc = DocBuilder { word_doc, table: Vec::new(), data: None };
            doc.fib(0x18, TEXT_FC as u32).fib(0x1C, TEXT_FC as u32 + len).fib(0x4C, len)
        }

        fn fib(mut self, off: usize, value: u32) -> Self {
            self.word_doc[off..off + 4].copy_from_slice(&value.to_le_bytes());
            self
        }

        /// `bytes` appended to the table stream, named by the FIB pair at `off`.
        fn table(mut self, off: usize, bytes: &[u8]) -> Self {
            let fc = u32::try_from(self.table.len()).unwrap();
            self.table.extend_from_slice(bytes);
            self.fib(off, fc).fib(off + 4, u32::try_from(bytes.len()).unwrap())
        }

        /// A piece table of `(cp_end, text, unicode, prm)` pieces whose text
        /// is appended to the WordDocument stream.
        fn pieces(mut self, pieces: &[(u32, &[u8], bool, u16)]) -> Self {
            let mut cps = vec![0u32];
            let mut pcds = Vec::new();
            for &(cp_end, text, unicode, prm) in pieces {
                let fc = u32::try_from(self.word_doc.len()).unwrap();
                self.word_doc.extend_from_slice(text);
                let fc = if unicode { fc } else { (fc * 2) | 0x4000_0000 };
                cps.push(cp_end);
                pcds.extend([&[0u8, 0][..], &fc.to_le_bytes(), &prm.to_le_bytes()].concat());
            }
            let plc = [words(&cps), pcds].concat();
            let len = u32::try_from(plc.len()).unwrap().to_le_bytes();
            self.table(0x1A2, &[&[2u8][..], &len, &plc].concat())
        }

        /// One CHPX FKP page giving the main text `grpprl`.
        fn chpx(mut self, grpprl: &[u8]) -> Self {
            let page = 2 * 512;
            let end = u32::try_from(self.word_doc.len()).unwrap();
            let fkp = &mut self.word_doc[page..page + 512];
            fkp[..8].copy_from_slice(&words(&[TEXT_FC as u32, end]));
            fkp[8] = 0x80;
            fkp[0x100] = u8::try_from(grpprl.len()).unwrap();
            fkp[0x101..0x101 + grpprl.len()].copy_from_slice(grpprl);
            fkp[511] = 1;
            self.table(0xFA, &words(&[TEXT_FC as u32, end, 2]))
        }

        /// A footnote story of `len` CPs after the main text, its reference
        /// at CP 0.
        fn footnote(self, len: u32) -> Self {
            self.fib(0x50, len)
                .table(0xAA, &[words(&[0, 1]), vec![0, 0]].concat())
                .table(0xB2, &words(&[0, len]))
        }

        fn data(self, data: Vec<u8>) -> Self {
            DocBuilder { data: Some(data), ..self }
        }

        fn bytes(self) -> Vec<u8> {
            use std::io::Write;
            let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
            let streams = [("WordDocument", Some(self.word_doc)), ("0Table", Some(self.table))];
            for (name, bytes) in streams.into_iter().chain([("Data", self.data)]) {
                if let Some(bytes) = bytes {
                    ole.create_stream(name).unwrap().write_all(&bytes).unwrap();
                }
            }
            ole.into_inner().into_inner()
        }
    }

    fn words(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn utf16(units: &[u16]) -> Vec<u8> {
        units.iter().flat_map(|u| u.to_le_bytes()).collect()
    }

    fn doc_with_inline_picture(inflated: usize) -> Vec<u8> {
        DocBuilder::new(b"\x01\r").chpx(&pic_location_chpx()).data(picture_data(inflated)).bytes()
    }

    fn record(ver_inst: u16, rec_type: u16, body: &[u8]) -> Vec<u8> {
        let len = u32::try_from(body.len()).unwrap().to_le_bytes();
        [&ver_inst.to_le_bytes()[..], &rec_type.to_le_bytes(), &len, body].concat()
    }

    /// A shape container for `spid` with the property ids `props`.
    fn shape(spid: u32, props: &[u16]) -> Vec<u8> {
        let table: Vec<u8> =
            props.iter().flat_map(|id| [&id.to_le_bytes()[..], &[0; 4]].concat()).collect();
        let sp = record(0x0002, 0xF00A, &[&spid.to_le_bytes()[..], &[0; 4]].concat());
        let opt = record(u16::try_from(props.len()).unwrap() << 4 | 3, 0xF00B, &table);
        record(0x000F, 0xF004, &[sp, opt].concat())
    }

    /// A floating shape anchored at CP 0 of `"\x08\r"`, drawn by `drawn`.
    fn floating(spid: u32, drawn: &[u8]) -> Vec<u8> {
        floating_in(spid, &[(0, drawn)])
    }

    /// A floating shape anchored at CP 0 of `"\x08\r"`, over the drawings
    /// `(dgglbl, drawn)`.
    fn floating_in(spid: u32, drawings: &[(u8, &[u8])]) -> Vec<u8> {
        let mut dgg = record(0x000F, 0xF000, &[]);
        for &(dgglbl, drawn) in drawings {
            dgg.push(dgglbl);
            dgg.extend(record(0x000F, 0xF002, &record(0x000F, 0xF003, drawn)));
        }
        let spa = [words(&[0, 1, spid]), vec![0; 22]].concat();
        DocBuilder::new(b"\x08\r").table(0x1DA, &spa).table(0x22A, &dgg).bytes()
    }

    const SHAPE_FIELD: &[u8] = b"\x13 SHAPE \x14\x01\x15\r";

    /// Inline picture data whose art is `art`, for every `\x01` of `text`.
    fn inline_art(text: &[u8], art: &[u8]) -> Vec<u8> {
        let data = [&words(&[6 + u32::try_from(art.len()).unwrap()])[..], &[6, 0], art].concat();
        DocBuilder::new(text).chpx(&pic_location_chpx()).data(data).bytes()
    }

    /// A document whose main text is the UTF-16 of `text`, with `~` an
    /// unpaired surrogate.
    fn damaged(text: &str) -> Vec<u8> {
        let units: Vec<u16> =
            text.encode_utf16().map(|u| if u == u16::from(b'~') { 0xDC00 } else { u }).collect();
        let len = u32::try_from(units.len()).unwrap();
        DocBuilder::new(b"").fib(0x4C, len).pieces(&[(len, &utf16(&units), true, 0)]).bytes()
    }

    fn fuel(bytes: &[u8]) -> u64 {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, bytes).unwrap();
        parse_ctx.fuel()
    }

    type Reported = (DiagnosticCode, Region, u64);

    fn reported(bytes: &[u8]) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, bytes).unwrap();
        parse_ctx.diagnostics().iter().map(|d| (d.code, d.region, d.count)).collect()
    }

    #[test]
    fn each_loss_is_reported_in_its_story() {
        use DiagnosticCode::*;
        let unpaired = utf16(&[0x61, 0xD800, 0x62, 0x0D]);
        let hdd = words(&[0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 4, 5, 5, 6, 8]);
        let ftxbxs = |reusable: u16| {
            [&[0u8; 4][..], &1u32.to_le_bytes(), &reusable.to_le_bytes(), &[0; 12]].concat()
        };
        let txbx = [words(&[0, 2, 4, 6, 8]), ftxbxs(0), ftxbxs(1), ftxbxs(0), ftxbxs(0)].concat();
        let field = |name: &str| format!("\x13{name}\x14\x15\r").into_bytes();
        let lfo = [&words(&[1, 0, 0, 0])[..], &[1, 0, 0, 0]].concat();
        let in_footnote = |story: &[u8]| {
            let len = u32::try_from(story.len()).unwrap();
            DocBuilder::new(&[b"a\r", story].concat()).fib(0x4C, 2).footnote(len).bytes()
        };

        let chpx_with = |flag: u8| [&[flag, 0x08, 1][..], &pic_location_chpx()].concat();
        let cases: Vec<(&str, Vec<u8>, Vec<Reported>)> = vec![
            (
                "body text with an unpaired surrogate",
                DocBuilder::new(b"").fib(0x4C, 4).pieces(&[(4, &unpaired, true, 0)]).bytes(),
                vec![(InvalidEncoding, Region::Body, 1)],
            ),
            (
                "footnote text with an unpaired surrogate",
                DocBuilder::new(b"")
                    .fib(0x4C, 2)
                    .footnote(2)
                    .pieces(&[(2, b"a\r", false, 0), (4, &utf16(&[0xDC00, 0x0D]), true, 0)])
                    .bytes(),
                vec![(InvalidEncoding, Region::Note, 1)],
            ),
            (
                "header text, never rendered, with an unpaired surrogate",
                DocBuilder::new(b"")
                    .fib(0x4C, 2)
                    .fib(0x54, 2)
                    .pieces(&[(2, b"a\r", false, 0), (4, &utf16(&[0xDC00, 0x0D]), true, 0)])
                    .bytes(),
                vec![],
            ),
            (
                "an unpaired surrogate in a discarded field instruction",
                damaged("\x13 PAGE ~\x14 1\x15\r"),
                vec![],
            ),
            (
                "an unpaired surrogate in a hyperlink's target",
                damaged("\x13 HYPERLINK \"http://x/~\" \x14a\x15\r"),
                vec![(InvalidEncoding, Region::Body, 1)],
            ),
            (
                "an unpaired surrogate in a hyperlink's tooltip",
                damaged("\x13 HYPERLINK \"http://x/\" \\o \"~\" \x14a\x15\r"),
                vec![],
            ),
            (
                "an unpaired surrogate in a field result",
                damaged("\x13 PAGE \x14~\x15\r"),
                vec![(InvalidEncoding, Region::Body, 1)],
            ),
            (
                "an unpaired surrogate in a nested field's result",
                damaged("\x13 HYPERLINK \"http://x/\" \x14\x13 PAGE \x14~\x15\x15\r"),
                vec![(InvalidEncoding, Region::Body, 1)],
            ),
            (
                "a broken double-byte sequence",
                DocBuilder::new(b"\x81\x20\r").fib(0x06, 0x0411).bytes(),
                vec![(InvalidEncoding, Region::Body, 1)],
            ),
            (
                "a piece outside the stream",
                DocBuilder::new(b"ab\r").fib(0x18, 0x10_0000).fib(0x1C, 0x10_0003).bytes(),
                vec![(TruncatedRecords, Region::Body, 1)],
            ),
            (
                "a footnote piece outside the stream",
                DocBuilder::new(b"")
                    .fib(0x4C, 2)
                    .footnote(2)
                    .pieces(&[(2, b"a\r", false, 0), (4, b"", true, 0)])
                    .bytes(),
                vec![(TruncatedRecords, Region::Note, 1)],
            ),
            (
                "a header piece outside the stream",
                DocBuilder::new(b"")
                    .fib(0x4C, 2)
                    .fib(0x54, 2)
                    .pieces(&[(2, b"a\r", false, 0), (4, b"", true, 0)])
                    .bytes(),
                vec![],
            ),
            (
                "a piece outside the stream from a header into the endnotes",
                DocBuilder::new(b"")
                    .fib(0x4C, 2)
                    .fib(0x54, 2)
                    .fib(0x60, 2)
                    .pieces(&[(2, b"a\r", false, 0), (6, b"", true, 0)])
                    .bytes(),
                vec![(TruncatedRecords, Region::Note, 1)],
            ),
            (
                "a header piece outside the stream, then endnote text",
                DocBuilder::new(b"")
                    .fib(0x4C, 2)
                    .fib(0x54, 4)
                    .fib(0x60, 2)
                    .pieces(&[(2, b"a\r", false, 0), (6, b"", true, 0), (8, b"", true, 0)])
                    .bytes(),
                vec![(TruncatedRecords, Region::Note, 1)],
            ),
            (
                "an unpaired surrogate in an anchor link",
                damaged("\x13 HYPERLINK \\l \"a~\" \x14a\x15\r"),
                vec![(InvalidEncoding, Region::Body, 1)],
            ),
            (
                "an unpaired surrogate in a relative link",
                damaged("\x13 HYPERLINK \"a~\" \x14a\x15\r"),
                vec![(InvalidEncoding, Region::Body, 1)],
            ),
            (
                "a formatting bin table of no whole record",
                DocBuilder::new(b"a\r").table(0xFA, &words(&[0, 2])).bytes(),
                vec![(PartUnreadable, Region::Body, 1)],
            ),
            (
                "a Prm0 with no model field",
                DocBuilder::new(b"").fib(0x4C, 2).pieces(&[(2, b"a\r", false, 0x05 << 1)]).bytes(),
                vec![(FormattingUnreadable, Region::Body, 1)],
            ),
            (
                "one section's odd and first headers and odd footer, then a story of no full section",
                DocBuilder::new(b"a\r").fib(0x54, 8).table(0xF2, &hdd).bytes(),
                vec![(HeaderOmitted, Region::Header, 2), (FooterOmitted, Region::Footer, 1)],
            ),
            (
                "two comments",
                DocBuilder::new(b"a\r")
                    .table(0xBA, &[words(&[0, 1, 2]), vec![0; 60]].concat())
                    .bytes(),
                vec![(CommentOmitted, Region::Comment, 2)],
            ),
            (
                "an unreadable comment table, with no comment text",
                DocBuilder::new(b"a\r").fib(0xBA, 999).fib(0xBE, 72).bytes(),
                vec![],
            ),
            (
                "a comment table with a stray byte",
                DocBuilder::new(b"a\r")
                    .fib(0x5C, 2)
                    .table(0xBA, &[words(&[0, 1]), vec![0; 30], vec![0]].concat())
                    .bytes(),
                vec![(CommentOmitted, Region::Comment, 1)],
            ),
            (
                "a comment table that does not read",
                DocBuilder::new(b"a\r").fib(0x5C, 2).fib(0xBA, 999).fib(0xBE, 72).bytes(),
                vec![(CommentOmitted, Region::Comment, 1)],
            ),
            (
                "a header table with a stray byte",
                DocBuilder::new(b"a\r")
                    .fib(0x54, 8)
                    .table(0xF2, &[hdd.clone(), vec![0]].concat())
                    .bytes(),
                vec![(HeaderOmitted, Region::Header, 1)],
            ),
            (
                "a header table that does not read, with no header text",
                DocBuilder::new(b"a\r").fib(0xF2, 999).fib(0xF6, 60).bytes(),
                vec![],
            ),
            (
                "two text boxes in use, one reusable, and the final range",
                DocBuilder::new(b"a\r").fib(0x64, 6).table(0x25A, &txbx).bytes(),
                vec![(TextBoxUnread, Region::Body, 2)],
            ),
            (
                "text-box text with no text-box table",
                DocBuilder::new(b"a\r").fib(0x64, 6).bytes(),
                vec![(TextBoxUnread, Region::Body, 1)],
            ),
            (
                "text boxes whose table does not read",
                DocBuilder::new(b"a\r").fib(0x64, 6).fib(0x25A, 999).fib(0x25E, 56).bytes(),
                vec![(TextBoxUnread, Region::Body, 1)],
            ),
            (
                "a text-box table that does not read, with no text-box text",
                DocBuilder::new(b"a\r").fib(0x25A, 999).fib(0x25E, 56).bytes(),
                vec![],
            ),
            (
                "an embedded object",
                DocBuilder::new(b"\x01\r").chpx(&chpx_with(0x0A)).bytes(),
                vec![(EmbeddedObject, Region::Body, 1)],
            ),
            ("field data", DocBuilder::new(b"\x01\r").chpx(&chpx_with(0x06)).bytes(), vec![]),
            (
                "a picture with no Data stream",
                DocBuilder::new(b"\x01\r").chpx(&pic_location_chpx()).bytes(),
                vec![(ImageUnavailable, Region::Body, 1)],
            ),
            (
                "a picture whose embedded-object flag is off",
                DocBuilder::new(b"\x01\r")
                    .chpx(&[&[0x0A, 0x08, 0][..], &pic_location_chpx()].concat())
                    .bytes(),
                vec![(ImageUnavailable, Region::Body, 1)],
            ),
            (
                "a picture in a footnote with no Data stream",
                DocBuilder::new(b"a\r\x01\r")
                    .fib(0x4C, 2)
                    .footnote(2)
                    .chpx(&pic_location_chpx())
                    .bytes(),
                vec![(ImageUnavailable, Region::Note, 1)],
            ),
            (
                "a floating shape's inline stand-in with no Data stream",
                DocBuilder::new(SHAPE_FIELD).chpx(&pic_location_chpx()).bytes(),
                vec![(ImageUnavailable, Region::Body, 1)],
            ),
            (
                "a floating shape's inline stand-in whose art holds no shape",
                inline_art(SHAPE_FIELD, &[]),
                vec![(ImageUnavailable, Region::Body, 1)],
            ),
            (
                "a picture with no blip",
                inline_art(b"\x01\r", &[]),
                vec![(ImageUnavailable, Region::Body, 1)],
            ),
            (
                "an inline shape that is no picture",
                inline_art(b"\x01\r", &shape(1, &[0x0080])),
                vec![(ImageUnavailable, Region::Body, 1)],
            ),
            (
                "a floating text box's inline stand-in",
                inline_art(SHAPE_FIELD, &shape(1, &[0x0080])),
                vec![],
            ),
            (
                "an inline shape that is no picture in another field's result",
                inline_art(b"\x13 INCLUDEPICTURE x \x14\x01\x15\r", &shape(1, &[0x0080])),
                vec![(ImageUnavailable, Region::Body, 1)],
            ),
            (
                "a floating picture's inline stand-in with no blip",
                inline_art(SHAPE_FIELD, &shape(1, &[0x4104])),
                vec![(ImageUnavailable, Region::Body, 1)],
            ),
            (
                "an embedded object in a footnote",
                DocBuilder::new(b"a\r\x01\r")
                    .fib(0x4C, 2)
                    .footnote(2)
                    .chpx(&chpx_with(0x0A))
                    .bytes(),
                vec![(EmbeddedObject, Region::Note, 1)],
            ),
            (
                "a check box in a footnote",
                in_footnote(&field(" FORMCHECKBOX ")),
                vec![(FormControlUnread, Region::Note, 1)],
            ),
            (
                "a floating picture",
                floating(7, &shape(7, &[0x4104])),
                vec![(ImageUnplaced, Region::Body, 1)],
            ),
            ("a floating text box", floating(7, &shape(7, &[0x0080])), vec![]),
            (
                "a floating picture in the main drawing after a header drawing",
                floating_in(7, &[(1, &shape(7, &[0x0181])), (0, &shape(7, &[0x4104]))]),
                vec![(ImageUnplaced, Region::Body, 1)],
            ),
            (
                "a floating shape",
                floating(7, &shape(7, &[0x0181])),
                vec![(GraphicUnread, Region::Body, 1)],
            ),
            (
                "a floating shape no drawing holds",
                floating(7, &shape(8, &[0x4104])),
                vec![(GraphicUnread, Region::Body, 1)],
            ),
            (
                "a check box and a drop-down, then a text form field",
                DocBuilder::new(
                    &[field(" FORMCHECKBOX "), field("formdropdown"), field("FORMTEXT")].concat(),
                )
                .bytes(),
                vec![(FormControlUnread, Region::Body, 2)],
            ),
            (
                "list tables outside the table stream",
                DocBuilder::new(b"a\r").fib(0x2E2, 999).fib(0x2E6, 30).bytes(),
                vec![(PartUnreadable, Region::Body, 1)],
            ),
            (
                "list tables with no overrides",
                DocBuilder::new(b"a\r").table(0x2E2, &[0, 0]).bytes(),
                vec![],
            ),
            (
                "list tables whose range passes the end of the table stream",
                DocBuilder::new(b"a\r")
                    .table(0x2E2, &[0, 0])
                    .fib(0x2E6, 30)
                    .table(0x2EA, &[0; 4])
                    .bytes(),
                vec![(PartUnreadable, Region::Body, 1)],
            ),
            (
                "a list override cut short",
                DocBuilder::new(b"a\r").table(0x2E2, &[0, 0]).table(0x2EA, &lfo).bytes(),
                vec![(PartUnreadable, Region::Body, 1)],
            ),
            (
                "list overrides outside the table stream",
                DocBuilder::new(b"a\r").table(0x2E2, &[0, 0]).fib(0x2EA, 999).fib(0x2EE, 4).bytes(),
                vec![(PartUnreadable, Region::Body, 1)],
            ),
            (
                "a style record that does not parse",
                DocBuilder::new(b"a\r").table(0xA2, &[4, 0, 1, 0, 10, 0, 4, 0, 0, 0, 0, 0]).bytes(),
                vec![(StylePartUnreadable, Region::Container, 1)],
            ),
            (
                "a style sheet outside the table stream",
                DocBuilder::new(b"a\r").fib(0xA2, 999).fib(0xA6, 10).bytes(),
                vec![(StylePartUnreadable, Region::Container, 1)],
            ),
            (
                "a style sheet outside the table stream, beside list tables",
                DocBuilder::new(b"a\r")
                    .fib(0xA2, 999)
                    .fib(0xA6, 10)
                    .fib(0x2E2, 999)
                    .fib(0x2E6, 30)
                    .bytes(),
                vec![(PartUnreadable, Region::Body, 2)],
            ),
            (
                "a character bin table outside the table stream",
                DocBuilder::new(b"a\r").fib(0xFA, 999).fib(0xFE, 12).bytes(),
                vec![(PartUnreadable, Region::Body, 1)],
            ),
            (
                "a paragraph bin table too short to hold a page",
                DocBuilder::new(b"a\r").table(0x102, &[0; 4]).bytes(),
                vec![(PartUnreadable, Region::Body, 1)],
            ),
            (
                "an FKP page outside the stream",
                DocBuilder::new(b"a\r").table(0x102, &words(&[0, 2, 900])).bytes(),
                vec![(PartUnreadable, Region::Body, 1)],
            ),
            (
                "footnote and endnote tables outside the table stream",
                DocBuilder::new(b"a\r")
                    .fib(0xAA, 999)
                    .fib(0xAE, 10)
                    .fib(0x212, 999)
                    .fib(0x216, 8)
                    .bytes(),
                vec![(PartUnreadable, Region::Note, 2)],
            ),
        ];
        for (case, bytes, expected) in cases {
            assert_eq!(reported(&bytes), expected, "{case}");
        }
    }

    #[test]
    fn both_drawing_walks_charge_the_parses_context() {
        let undrawn = fuel(&floating(7, &[]));
        assert_eq!(fuel(&floating(7, &shape(7, &[0x0080]))), undrawn + 3);
        let stand_in = |art: &[u8]| fuel(&inline_art(SHAPE_FIELD, art));
        assert_eq!(stand_in(&shape(1, &[0x0080])), stand_in(&[]) + 3);
    }

    #[test]
    fn every_drawing_record_walked_is_charged_once() {
        let run = record(0x000F, 0xF003, &[shape(1, &[]), shape(2, &[0x0104])].concat());
        let parse_ctx = ParseCtx::default();
        let kinds = shape_kinds(&parse_ctx, &run).unwrap();
        assert_eq!(kinds, HashMap::from([(1, ShapeKind::Graphic), (2, ShapeKind::Picture)]));
        assert_eq!(parse_ctx.fuel(), 7);
        let limits = crate::limits::Limits { max_work: 6, ..Default::default() };
        assert!(shape_kinds(&ParseCtx::new(&limits), &run).is_err());
    }

    fn inline_picture_bytes(
        limits: &crate::limits::Limits,
        inflated: usize,
    ) -> Result<usize, ConvertError> {
        let parse_ctx = ParseCtx::new(limits);
        let run = Run {
            fc_start: 0,
            fc_end: 2,
            props: RunProps { chpx: pic_location_chpx(), ..RunProps::default() },
        };
        let assembler = assembler(&parse_ctx, vec![run], picture_data(inflated));
        assert!(assembler.picture_at(0, Region::Body, false)?.is_some());
        Ok(assembler.assets.borrow().assets[0].bytes.len())
    }

    #[test]
    fn an_inline_picture_inflating_past_the_contexts_entry_bytes_is_a_limit() {
        let limits = crate::limits::Limits { max_entry_bytes: 4096, ..Default::default() };
        assert_eq!(inline_picture_bytes(&limits, 4096).unwrap(), 4096);
        let limits = crate::limits::Limits { max_entry_bytes: 4095, ..Default::default() };
        let err = inline_picture_bytes(&limits, 4096).unwrap_err();
        assert!(matches!(
            err,
            ConvertError::ResourceLimit { limit: crate::limits::Limit::EntryBytes, max: 4095 }
        ));
    }

    #[test]
    fn an_inline_picture_past_the_contexts_asset_total_is_a_limit() {
        let bytes = doc_with_inline_picture(4096);
        let limits = crate::limits::Limits { max_asset_total_bytes: 4096, ..Default::default() };
        let doc = parse(&ParseCtx::new(&limits), &bytes).unwrap();
        assert_eq!(doc.assets[0].bytes.len(), 4096);
        let limits = crate::limits::Limits { max_asset_total_bytes: 4095, ..Default::default() };
        let err = parse(&ParseCtx::new(&limits), &bytes).unwrap_err();
        assert!(matches!(
            err,
            ConvertError::ResourceLimit { limit: crate::limits::Limit::AssetTotalBytes, max: 4095 }
        ));
    }

    fn flush_spanning_table(max_expansion: u64) -> Result<Vec<Block>, ConvertError> {
        let limits = crate::limits::Limits { max_expansion, ..Default::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let row = |boundaries: Vec<i16>| {
            let cells =
                vec![vec![Block::Paragraph(vec![Inline::plain("x")])]; boundaries.len() - 1];
            let tap = Tap { boundaries, ..Tap::default() };
            DocRow { cells, tap: Some(tap) }
        };
        let mut table_rows = vec![row(vec![0, 1000, 2000, 3000]), row(vec![0, 3000])];
        let mut blocks = Vec::new();
        assembler(&parse_ctx, Vec::new(), Vec::new()).flush_table(
            &mut blocks,
            &mut table_rows,
            &mut Vec::new(),
            &mut Vec::new(),
        )?;
        Ok(blocks)
    }

    #[test]
    fn a_table_cell_spanning_past_the_contexts_expansion_is_a_limit() {
        assert_eq!(flush_spanning_table(2).unwrap().len(), 1);
        let err = flush_spanning_table(1).unwrap_err();
        assert!(matches!(
            err,
            ConvertError::ResourceLimit { limit: crate::limits::Limit::Expansion, max: 1 }
        ));
    }

    #[test]
    fn every_fkp_run_is_charged_once() {
        let mut word_doc = vec![0u8; 1024];
        for (k, fc) in [0u32, 10, 20, 30].into_iter().enumerate() {
            word_doc[512 + k * 4..512 + k * 4 + 4].copy_from_slice(&fc.to_le_bytes());
        }
        word_doc[1023] = 3;
        word_doc[0xFE..0x102].copy_from_slice(&20u32.to_le_bytes());
        let table: Vec<u8> = [0u32, 30, 60, 1, 1].into_iter().flat_map(u32::to_le_bytes).collect();
        let parse_ctx = ParseCtx::default();
        let runs = parse_fkps(&parse_ctx, &word_doc, &table, 0xFA, FkpKind::Chpx, &[]).unwrap();
        assert_eq!((runs.len(), parse_ctx.fuel()), (6, 6));
    }

    #[test]
    fn a_word_document_that_is_no_stream_is_malformed_and_one_absent_is_missing() {
        let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        ole.create_storage("WordDocument").unwrap();
        let storage = ole.into_inner().into_inner();
        let absent = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        let absent = absent.into_inner().into_inner();
        let code = |bytes: &[u8]| parse(&ParseCtx::default(), bytes).unwrap_err().code();
        assert_eq!(code(&storage), "malformed");
        assert_eq!(code(&absent), "missingPart");
    }

    fn ole_stream(ole: &mut cfb::CompoundFile<std::io::Cursor<&[u8]>>, name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut ole.open_stream(name).unwrap(), &mut out).unwrap();
        out
    }

    #[test]
    fn a_failure_in_the_paragraph_run_pass_propagates() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/doc/text.doc");
        let bytes = std::fs::read(path).unwrap();
        let mut ole = cfb::CompoundFile::open(std::io::Cursor::new(bytes.as_slice())).unwrap();
        let word_doc = ole_stream(&mut ole, "WordDocument");
        let flags = get_u16(&word_doc, 0x0A).unwrap();
        let table = ole_stream(&mut ole, if flags & 0x0200 != 0 { "1Table" } else { "0Table" });
        let fuel = |fib_off, kind| {
            let parse_ctx = ParseCtx::default();
            parse_fkps(&parse_ctx, &word_doc, &table, fib_off, kind, &[]).unwrap();
            parse_ctx.fuel()
        };
        let chpx = fuel(0xFA, FkpKind::Chpx);
        assert!(fuel(0x102, FkpKind::Papx) > 0);
        let limits = crate::limits::Limits { max_work: chpx, ..crate::limits::Limits::default() };
        let err = parse(&ParseCtx::new(&limits), &bytes).unwrap_err();
        assert!(
            matches!(err, ConvertError::ResourceLimit { limit: crate::limits::Limit::Work, max }
                if max == chpx),
            "{err:?}"
        );
    }
}
