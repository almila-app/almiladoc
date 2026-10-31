//! Legacy Word 97-2003 binary (.doc), implementing the published resolution
//! algorithms: OLE2 container, FIB, piece table with `Prm`s, CHPX/PAPX runs
//! applied over the STSH style chains in specification order, and the
//! PlfLst/PlfLfo list tables. Character positions are counted in UTF-16
//! units, matching the CP-indexed PLC structures.

mod assemble;
mod fkp;
mod fonts;
mod lists;
mod pieces;
#[deny(
    clippy::indexing_slicing,
    reason = "a binary record reader reads through checked views (ADR 0001 decision 16)"
)]
mod sprm;
mod stsh;
mod text;

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Document, Note, NoteKind};
use crate::shared::binary::{
    OptionalStream, get_u16, get_u32, optional_ole_stream, read_ole_stream,
};
use crate::shared::officeart::{self, record_at};
use assemble::Assembler;
use fkp::{FkpKind, parse_fkps};
use pieces::{Coverage, PieceTable};
use std::collections::HashMap;
use std::io::Cursor;
use std::ops::Range;
use text::TextView;

pub(super) fn parse(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Document, ConvertError> {
    let mut ole = cfb::CompoundFile::open(Cursor::new(bytes))
        .map_err(|_| ConvertError::malformed("not an OLE2 compound file"))?;

    let word_doc = read_ole_stream(parse_ctx, &mut ole, "WordDocument")?;
    let fib = Fib::read(&word_doc)?;
    let flags = fib.u16(Fib::FLAGS);
    if flags & Fib::F_ENCRYPTED != 0 {
        return Err(ConvertError::Encrypted);
    }
    let (table_name, other_table) =
        if flags & Fib::F_WHICH_TBL_STM != 0 { ("1Table", "0Table") } else { ("0Table", "1Table") };
    let table = match optional_ole_stream(parse_ctx, &mut ole, table_name)? {
        OptionalStream::Absent => optional_ole_stream(parse_ctx, &mut ole, other_table)?.or_empty(),
        stream => stream.or_empty(),
    };

    let stories = Stories::read(fib)?;
    let lcb_clx = fib.size(Fib::LCB_CLX);

    // [MS-DOC] 2.5.6 FibRgFcLcb97: lcbClx MUST be greater than zero.
    if lcb_clx == 0 {
        return Err(ConvertError::malformed_part("WordDocument", "no piece table"));
    }
    let coverage = Coverage { terminal_cp: stories.terminal_cp()?, cb_mac: fib.size(Fib::CB_MAC) };
    let pieces = PieceTable::parse(parse_ctx, &table, fib.size(Fib::FC_CLX), lcb_clx, coverage)?;
    let text = TextView::new(&word_doc, pieces);

    let data = optional_ole_stream(parse_ctx, &mut ole, "Data")?.or_empty();
    let chpx_runs =
        parse_fkps(parse_ctx, fib, &word_doc, &table, Fib::FC_PLCF_BTE_CHPX, FkpKind::Chpx, &data)?;
    let papx_runs =
        parse_fkps(parse_ctx, fib, &word_doc, &table, Fib::FC_PLCF_BTE_PAPX, FkpKind::Papx, &data)?;
    let stylesheet = stsh::parse(parse_ctx, fib, &table, fib.size(Fib::LCB_PLF_LST) > 0)?;
    let fonts = fonts::parse(parse_ctx, fib, &table)?;
    let list_tables = lists::parse(parse_ctx, fib, &table);
    report_unread_stories(parse_ctx, fib, &table, &stories);
    let (note_refs, note_ranges) = note_layout(parse_ctx, fib, &table, &stories);
    let shapes = floating_shapes(parse_ctx, fib, &table)?;
    let assembler = Assembler::new(
        parse_ctx,
        text,
        chpx_runs,
        papx_runs,
        stylesheet,
        fonts,
        list_tables,
        note_refs,
        shapes,
        data,
    );
    let main = stories.main();
    let blocks = assembler.build_blocks(&main, &main, Region::Body)?.unwrap_or_default();
    let mut notes = Vec::new();
    for (range, story, id, kind) in note_ranges {
        if range.is_empty() || assembler.dropped_notes.borrow().contains(&id) {
            continue;
        }
        if let Some(blocks) = assembler.build_blocks(&story, &range, Region::Note)? {
            notes.push(Note { id, kind, blocks });
        }
    }
    let assets = assembler.assets.into_inner().assets;
    Ok(Document { blocks, notes, assets, ..Document::default() })
}

/// A note's body range, the story that holds it, its id and its kind.
type NoteRange = (Range<usize>, Range<usize>, String, NoteKind);

/// The id of the note each reference CP names, and each note's body.
fn note_layout(
    parse_ctx: &ParseCtx<'_>,
    fib: Fib<'_>,
    table: &[u8],
    stories: &Stories,
) -> (HashMap<usize, String>, Vec<NoteRange>) {
    let mut note_refs = HashMap::new();
    let mut note_ranges = Vec::new();
    for (ref_off, txt_off, story, prefix, kind) in [
        (Fib::FC_PLCFFND_REF, Fib::FC_PLCFFND_TXT, stories.footnotes(), "fn", NoteKind::Footnote),
        (Fib::FC_PLCFEND_REF, Fib::FC_PLCFEND_TXT, stories.endnotes(), "en", NoteKind::Endnote),
    ] {
        let [refs, txts] = [(ref_off, 2), (txt_off, 0)].map(|(fib_off, data_size)| {
            read_plc(fib, table, fib_off, data_size).unwrap_or_else(|| {
                parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Note);
                Plc::EMPTY
            })
        });
        let at = |k| story.start.saturating_add(txts.cp(k)).min(story.end);
        for i in 0..refs.n {
            let id = format!("{prefix}{i}");
            if i < txts.n {
                note_ranges.push((at(i)..at(i + 1), story.clone(), id.clone(), kind));
            }
            note_refs.insert(refs.cp(i), id);
        }
    }
    (note_refs, note_ranges)
}

/// The story lengths of `FibRgLw97`, each story's text following the one
/// before it ([MS-DOC] 2.3.1-2.3.7); `reserved3` holds none.
struct Stories {
    text: usize,
    ftn: usize,
    hdd: usize,
    atn: usize,
    edn: usize,
    txbx: usize,
    hdr_txbx: usize,
}

impl Stories {
    /// The story lengths of `fib`; a negative one is `Malformed`.
    fn read(fib: Fib<'_>) -> Result<Self, ConvertError> {
        let ccp = |off| {
            get_u32(fib.0, off)
                .and_then(|value| i32::try_from(value).ok())
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| {
                    ConvertError::malformed_part("WordDocument", "negative story length")
                })
        };
        Ok(Self {
            text: ccp(Fib::CCP_TEXT)?,
            ftn: ccp(Fib::CCP_FTN)?,
            hdd: ccp(Fib::CCP_HDD)?,
            atn: ccp(Fib::CCP_ATN)?,
            edn: ccp(Fib::CCP_EDN)?,
            txbx: ccp(Fib::CCP_TXBX)?,
            hdr_txbx: ccp(Fib::CCP_HDR_TXBX)?,
        })
    }

    fn main(&self) -> Range<usize> {
        0..self.text
    }

    fn footnotes(&self) -> Range<usize> {
        self.text..self.text + self.ftn
    }

    fn endnotes(&self) -> Range<usize> {
        let start = self.text + self.ftn + self.hdd + self.atn;
        start..start + self.edn
    }

    /// The last CP of `PlcPcd` ([MS-DOC] 2.8.35): the sum of the stories, and
    /// one more when any but the main document holds text.
    fn terminal_cp(&self) -> Result<usize, ConvertError> {
        let others = [self.ftn, self.hdd, self.atn, self.edn, self.txbx, self.hdr_txbx];
        let any = others.iter().any(|&len| len > 0);
        others
            .into_iter()
            .try_fold(self.text + usize::from(any), usize::checked_add)
            .ok_or_else(|| ConvertError::malformed_part("WordDocument", "stories too long"))
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

    /// `None` unless `bytes` hold n+1 CPs and n `data_size` records, n >= 1.
    fn new(bytes: &'t [u8], data_size: usize) -> Option<Self> {
        let records = bytes.len().checked_sub(4).filter(|_| bytes.len() >= 8)?;
        let stride = 4 + data_size;
        (records % stride == 0).then_some(Self { bytes, n: records / stride, data_size })
    }

    fn cp(&self, i: usize) -> usize {
        get_u32(self.bytes, i * 4).unwrap_or(0) as usize
    }

    fn data(&self, i: usize) -> &'t [u8] {
        let at = (self.n + 1) * 4 + i * self.data_size;
        self.bytes.get(at..).and_then(|rest| rest.get(..self.data_size)).unwrap_or_default()
    }
}

/// The bytes of a Word 97 FIB through `fibRgFcLcb97`, which holds every
/// field this reader reads.
const FIB_MIN: usize = 0x382;

/// The FIB at the start of the WordDocument stream, read whole.
#[derive(Clone, Copy)]
struct Fib<'a>(&'a [u8; FIB_MIN]);

impl<'a> Fib<'a> {
    const W_IDENT: u16 = 0xA5EC;
    /// [MS-DOC] 2.5.2 `FibBase`: the bit fields `fDot` through `fObfuscated`.
    const FLAGS: usize = 0x0A;
    const F_ENCRYPTED: u16 = 0x0100;
    const F_WHICH_TBL_STM: u16 = 0x0200;
    const CB_MAC: usize = 0x40;
    const CCP_TEXT: usize = 0x4C;
    const CCP_FTN: usize = 0x50;
    const CCP_HDD: usize = 0x54;
    const CCP_ATN: usize = 0x5C;
    const CCP_EDN: usize = 0x60;
    const CCP_TXBX: usize = 0x64;
    const CCP_HDR_TXBX: usize = 0x68;
    const FC_STSHF: usize = 0xA2;
    const FC_PLCFFND_REF: usize = 0xAA;
    const FC_PLCFFND_TXT: usize = 0xB2;
    const FC_PLCFAND_REF: usize = 0xBA;
    const FC_PLCF_HDD: usize = 0xF2;
    const FC_PLCF_BTE_CHPX: usize = 0xFA;
    const FC_PLCF_BTE_PAPX: usize = 0x102;
    const FC_STTBF_FFN: usize = 0x112;
    const FC_CLX: usize = 0x1A2;
    const LCB_CLX: usize = 0x1A6;
    const FC_PLC_SPA_MOM: usize = 0x1DA;
    const FC_PLCFEND_REF: usize = 0x20A;
    const FC_PLCFEND_TXT: usize = 0x212;
    const FC_DGG_INFO: usize = 0x22A;
    const FC_PLCFTXBX_TXT: usize = 0x25A;
    const FC_PLF_LST: usize = 0x2E2;
    const LCB_PLF_LST: usize = 0x2E6;
    const FC_PLF_LFO: usize = 0x2EA;
    const LCB_PLF_LFO: usize = 0x2EE;

    /// The FIB of `word_doc`; a stream too short for one or without its
    /// magic is `Malformed`.
    fn read(word_doc: &'a [u8]) -> Result<Self, ConvertError> {
        if get_u16(word_doc, 0) != Some(Self::W_IDENT) {
            return Err(ConvertError::malformed_part("WordDocument", "invalid FIB magic"));
        }
        word_doc
            .first_chunk::<FIB_MIN>()
            .map(Self)
            .ok_or_else(|| ConvertError::malformed_part("WordDocument", "FIB cut short"))
    }

    /// The field at `off`, which lies inside the FIB.
    fn u16(self, off: usize) -> u16 {
        get_u16(self.0, off).unwrap_or_default()
    }

    /// The 32-bit position, length or count at `off`, which lies inside
    /// the FIB.
    fn size(self, off: usize) -> usize {
        get_u32(self.0, off).and_then(|v| usize::try_from(v).ok()).unwrap_or_default()
    }

    /// The table-stream bytes the `fc`/`lcb` pair at `off` names: empty when
    /// `lcb` is 0, `None` when they do not lie in the table stream.
    fn table_range(self, table: &[u8], off: usize) -> Option<&[u8]> {
        let (fc, lcb) = (self.size(off), self.size(off + 4));
        if lcb == 0 {
            return Some(&[]);
        }
        table.get(fc..fc.saturating_add(lcb))
    }
}

/// The PLC whose FIB `fc`/`lcb` pair sits at `fib_off`: empty when `lcb` is
/// 0, `None` when it does not lie in the table stream or holds no whole
/// number of records.
fn read_plc<'t>(
    fib: Fib<'_>,
    table: &'t [u8],
    fib_off: usize,
    data_size: usize,
) -> Option<Plc<'t>> {
    let bytes = fib.table_range(table, fib_off)?;
    if bytes.is_empty() {
        return Some(Plc::EMPTY);
    }
    Plc::new(bytes, data_size)
}

/// Count the header and footer stories and the comments, which this reader
/// does not render, and the body text-box stories, which it never reads;
/// a story whose table does not read counts once.
fn report_unread_stories(parse_ctx: &ParseCtx<'_>, fib: Fib<'_>, table: &[u8], stories: &Stories) {
    if let Some(hdd) = read_plc(fib, table, Fib::FC_PLCF_HDD, 0) {
        let sections = hdd.n.saturating_sub(1).saturating_sub(6) / 6;
        for story in (6..6 + sections * 6).filter(|&i| hdd.cp(i + 1) > hdd.cp(i)) {
            match (story - 6) % 6 {
                0 | 1 | 4 => parse_ctx.emit(DiagnosticCode::HeaderOmitted, Region::Header),
                _ => parse_ctx.emit(DiagnosticCode::FooterOmitted, Region::Footer),
            }
        }
    } else if stories.hdd > 0 {
        parse_ctx.emit(DiagnosticCode::HeaderOmitted, Region::Header);
    }
    if let Some(comments) = read_plc(fib, table, Fib::FC_PLCFAND_REF, 30) {
        parse_ctx.emit_count(DiagnosticCode::CommentOmitted, Region::Comment, comments.n as u64);
    } else if stories.atn > 0 {
        parse_ctx.emit(DiagnosticCode::CommentOmitted, Region::Comment);
    }
    if stories.txbx > 0 {
        let boxes = read_plc(fib, table, Fib::FC_PLCFTXBX_TXT, 22).map_or(0, |txbx| {
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
    parse_ctx: &ParseCtx<'_>,
    fib: Fib<'_>,
    table: &[u8],
) -> Result<HashMap<usize, ShapeKind>, ConvertError> {
    let Some(spa) = read_plc(fib, table, Fib::FC_PLC_SPA_MOM, 26).filter(|spa| spa.n > 0) else {
        return Ok(HashMap::new());
    };
    let kinds = fib
        .table_range(table, Fib::FC_DGG_INFO)
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
    let dgg = record_at(content, 0)?.body;
    let mut rest = content.get(8 + dgg.len()..)?;
    loop {
        let (&dgglbl, drawing) = rest.split_first()?;
        let body = record_at(drawing, 0)?.body;
        let (container, next) = drawing.split_at_checked(8 + body.len())?;
        if dgglbl == 0 {
            return Some(container);
        }
        rest = next;
    }
}

/// Each shape's kind by `spid` (0 when it has none), over every shape
/// container in the OfficeArt records of `run`.
fn shape_kinds(
    parse_ctx: &ParseCtx<'_>,
    run: &[u8],
) -> Result<HashMap<u32, ShapeKind>, ConvertError> {
    let mut kinds = HashMap::new();
    let mut pending = vec![run];
    while let Some(container) = pending.pop() {
        for record in officeart::records(container) {
            parse_ctx.charge(1)?;
            if record.rec_type == officeart::SP_CONTAINER {
                let mut spid = None;
                let mut kind = ShapeKind::Graphic;
                for child in officeart::records(record.body) {
                    parse_ctx.charge(1)?;
                    match child.rec_type {
                        officeart::FSP => spid = get_u32(child.body, 0),
                        officeart::FOPT | officeart::TERTIARY_FOPT => {
                            kind = shape_kind(kind, child.instance(), child.body);
                        }
                        _ => {}
                    }
                }
                kinds.insert(spid.unwrap_or(0), kind);
            } else if record.version() == officeart::CONTAINER_VERSION {
                pending.push(record.body);
            }
        }
    }
    Ok(kinds)
}

/// `kind` updated by an OfficeArt property table of `count` entries: a
/// `pib` makes a picture, an `lTxid` a text box.
fn shape_kind(kind: ShapeKind, count: u16, props: &[u8]) -> ShapeKind {
    officeart::properties(count, props).fold(kind, |kind, (id, _)| match (id, kind) {
        (officeart::PIB, _) => ShapeKind::Picture,
        (officeart::LTXID, ShapeKind::Graphic) => ShapeKind::TextBox,
        _ => kind,
    })
}

#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    clippy::disallowed_methods,
    reason = "the tests read fixtures and build records from known small sizes"
)]
mod tests {
    use super::*;
    use crate::limits::Limit;
    use crate::model::Block;
    use crate::shared::officeart::record;
    use crate::shared::test_support::diagnostic_rows;
    use sprm::{PapDelta, apply_pap_sprms};
    use std::assert_matches;

    #[test]
    fn repeated_huge_paragraph_properties_are_charged_on_each_replay() {
        use crate::limits::Limits;
        let grpprl = [0x46, 0x66, 0, 0, 0, 0, 0x46, 0x66, 0, 0, 0, 0, 0x16, 0x24, 1];
        let mut data = 0x3FA2u16.to_le_bytes().to_vec();
        data.extend([0x16, 0x24, 1].repeat(0x3FA2 / 3));
        let doc = DocBuilder::new(b"123456789\rabcdefghj\r")
            .papx(&[(10, &grpprl), (10, &grpprl)])
            .data(data);
        let stream_len = u32::try_from(doc.word_doc.len()).unwrap();
        let doc = doc.fib(0x40, stream_len);
        let expected = 2 + 2 * 2 * 0x3FA2;
        for max_work in [expected, expected - 1] {
            let limits = Limits { max_work, ..Limits::DEFAULT };
            let parse_ctx = ParseCtx::new(&limits);
            let runs = parse_fkps(
                &parse_ctx,
                Fib::read(&doc.word_doc).unwrap(),
                &doc.word_doc,
                &doc.table,
                Fib::FC_PLCF_BTE_PAPX,
                FkpKind::Papx,
                doc.data.as_deref().unwrap(),
            );
            if max_work == expected {
                let runs = runs.unwrap();
                assert_eq!(runs.len(), 2);
                assert!(runs.iter().all(|run| run.props.pap.in_table == Some(true)));
            } else {
                assert_matches!(runs.map(drop), Err(ConvertError::ResourceLimit { limit: Limit::Work, max }) if max == max_work);
            }
            assert_eq!(parse_ctx.fuel(), expected);
        }
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
        apply_pap_sprms(&ParseCtx::default(), &grpprl, &[], &mut delta).unwrap();
        let tap = delta.tap.expect("TAP parsed");
        assert_eq!(tap.boundaries, vec![0, 4000, 8000]);
        assert!(tap.cells[0].vert_restart && !tap.cells[0].vert_cont);
        assert!(tap.cells[1].vert_cont && !tap.cells[1].vert_restart);
        let short = [0x08, 0xD6, 6, 0, 2, 0, 0, 0xA0, 0x0F];
        let mut delta = PapDelta::default();
        apply_pap_sprms(&ParseCtx::default(), &short, &[], &mut delta).unwrap();
        assert!(delta.tap.is_none());
    }

    #[test]
    fn huge_paragraph_properties_apply_their_grpprl() {
        let grpprl = [0x46, 0x66, 0, 0, 0, 0];
        let data = [3, 0, 0x16, 0x24, 1];
        let parse_ctx = ParseCtx::default();
        let mut delta = PapDelta::default();
        apply_pap_sprms(&parse_ctx, &grpprl, &data, &mut delta).unwrap();
        assert_eq!((delta.in_table, parse_ctx.fuel()), (Some(true), 3));
    }

    pub(super) fn picture_data(inflated: usize) -> Vec<u8> {
        let blip = officeart::deflated_metafile_blip(inflated);
        let lcb = u32::try_from(blip.len() + 6).unwrap();
        [&lcb.to_le_bytes()[..], &6u16.to_le_bytes(), &blip].concat()
    }

    pub(super) const PIC_LOCATION_CHPX: &[u8] = &[0x03, 0x6A, 0, 0, 0, 0];

    const TEXT_FC: usize = 0x800;

    /// Where a piece of no text points: past every stream a case builds.
    const PAST_STREAM: u32 = 0x10000;

    /// A Word 97 DOC whose main text is `text` in one compressed piece, with
    /// the FIB fields, table-stream structures and Data stream a case adds.
    /// Its piece table is written last, a piece of zero bytes after the
    /// pieces covering what the stories declare past them.
    struct DocBuilder {
        word_doc: Vec<u8>,
        table: Vec<u8>,
        data: Option<Vec<u8>>,
        prcs: Vec<Vec<u8>>,
        /// `(cp_end, fc, prm)` of each piece.
        pieces: Vec<(u32, u32, u16)>,
        cb_mac: Option<u32>,
        /// Whether the FIB names a piece table.
        clx: bool,
    }

    impl DocBuilder {
        fn new(text: &[u8]) -> Self {
            let len = u32::try_from(text.len()).unwrap();
            let doc = Self {
                word_doc: vec![0u8; TEXT_FC],
                table: Vec::new(),
                data: None,
                prcs: Vec::new(),
                pieces: Vec::new(),
                cb_mac: None,
                clx: true,
            };
            // [MS-DOC] 2.5.1 Fib and 2.5.2 FibBase for nFib 0x00C1: fExtChar, nFibBack, csw,
            // cslw and cbRgFcLcb; cswNew at FIB_MIN stays 0.
            [(0x00, 0xA5EC), (0x02, 0x00C1), (0x0A, 0x1000), (0x0C, 0x00BF)]
                .into_iter()
                .chain([(0x20, 0x000E), (0x3E, 0x0016), (0x98, 0x005D)])
                .fold(doc, |doc, (off, value)| doc.fib16(off, value))
                .pieces(&[(len, text, false, 0)])
                .fib(0x4C, len)
        }

        fn fib(mut self, off: usize, value: u32) -> Self {
            self.word_doc[off..off + 4].copy_from_slice(&value.to_le_bytes());
            self
        }

        fn fib16(mut self, off: usize, value: u16) -> Self {
            self.word_doc[off..off + 2].copy_from_slice(&value.to_le_bytes());
            self
        }

        fn lid(self, lid: u16) -> Self {
            self.fib16(0x06, lid)
        }

        /// `fFarEast` set beside the other FIB flags, and `lidFE`.
        fn far_east(self, lid_fe: u16) -> Self {
            let flags = get_u16(&self.word_doc, Fib::FLAGS).unwrap() | 0x4000;
            self.fib16(Fib::FLAGS, flags).fib16(0x3C, lid_fe)
        }

        /// `bytes` appended to the table stream, named by the FIB pair at `off`.
        fn table(mut self, off: usize, bytes: &[u8]) -> Self {
            let fc = u32::try_from(self.table.len()).unwrap();
            self.table.extend_from_slice(bytes);
            self.fib(off, fc).fib(off + 4, u32::try_from(bytes.len()).unwrap())
        }

        /// A piece table of `(cp_end, text, unicode, prm)` pieces whose text
        /// is appended to the WordDocument stream.
        fn pieces(self, pieces: &[(u32, &[u8], bool, u16)]) -> Self {
            self.clx(&[], pieces)
        }

        /// [`Self::pieces`] after the property modifiers `prcs`, which a
        /// complex `prm` names by index.
        fn clx(mut self, prcs: &[&[u8]], pieces: &[(u32, &[u8], bool, u16)]) -> Self {
            self.prcs = prcs.iter().map(|grpprl| grpprl.to_vec()).collect();
            self.pieces.clear();
            for &(cp_end, text, unicode, prm) in pieces {
                let fc = match text {
                    [] => PAST_STREAM,
                    _ => u32::try_from(self.word_doc.len()).unwrap(),
                };
                self.word_doc.extend_from_slice(text);
                let fc = if unicode { fc } else { (fc * 2) | 0x4000_0000 };
                self.pieces.push((cp_end, fc, prm));
            }
            self
        }

        /// `cbMac` past the stream, so that the pieces of no text lie inside
        /// it and outside the stream.
        fn truncated(self) -> Self {
            Self { cb_mac: Some(2 * PAST_STREAM), ..self }
        }

        /// The last CP the FIB's stories declare.
        fn terminal_cp(&self) -> u32 {
            let ccp = |off| get_u32(&self.word_doc, off).unwrap();
            let others = [0x50, 0x54, 0x5C, 0x60, 0x64, 0x68].map(ccp);
            ccp(0x4C) + others.iter().sum::<u32>() + u32::from(others.iter().any(|&n| n > 0))
        }

        /// The piece table written, its last piece reaching the terminal CP.
        fn covered(mut self) -> Self {
            if !self.clx {
                return self;
            }
            let last = self.pieces.last().map_or(0, |&(cp_end, ..)| cp_end);
            let terminal = self.terminal_cp();
            if last < terminal {
                assert!(
                    FIB_MIN + (terminal - last) as usize <= 0x400,
                    "filler past the FIB's zeros"
                );
                self.pieces.push((terminal, (FIB_MIN as u32 * 2) | 0x4000_0000, 0));
            }
            let cps: Vec<u32> = [0].into_iter().chain(self.pieces.iter().map(|p| p.0)).collect();
            let pcds: Vec<(u32, u16)> = self.pieces.iter().map(|&(_, fc, prm)| (fc, prm)).collect();
            let prcs: Vec<&[u8]> = self.prcs.iter().map(Vec::as_slice).collect();
            let table = clx(&prcs, &cps, &pcds);
            self.table(0x1A2, &table)
        }

        /// One CHPX FKP page giving the text `grpprl`.
        fn chpx(self, grpprl: &[u8]) -> Self {
            let bytes = u32::try_from(self.word_doc.len() - TEXT_FC).unwrap();
            self.chpx_runs(&[(bytes, grpprl)])
        }

        /// One CHPX FKP page giving each run of `(bytes, grpprl)` the next
        /// `bytes` of the text and its sprms.
        fn chpx_runs(self, runs: &[(u32, &[u8])]) -> Self {
            let entries: Vec<_> = runs
                .iter()
                .map(|&(bytes, grpprl)| match u8::try_from(grpprl.len()).unwrap() {
                    0 => (bytes, Vec::new()),
                    cb => (bytes, [&[cb][..], grpprl].concat()),
                })
                .collect();
            self.fkp(2, 0xFA, 1, &entries)
        }

        /// One PAPX FKP page giving each run of `(bytes, grpprl)` the next
        /// `bytes` of the main text and its sprms.
        fn papx(self, runs: &[(u32, &[u8])]) -> Self {
            let styled: Vec<_> = runs.iter().map(|&(bytes, grpprl)| (bytes, 0, grpprl)).collect();
            self.styled_papx(&styled)
        }

        /// [`Self::papx`] with each run's paragraph style `istd`.
        fn styled_papx(self, runs: &[(u32, u16, &[u8])]) -> Self {
            let entries: Vec<_> = runs
                .iter()
                .map(|&(bytes, istd, grpprl)| {
                    let papx = [&istd.to_le_bytes()[..], grpprl].concat();
                    let cb = u8::try_from(papx.len() / 2 + 1).unwrap();
                    let head = if papx.len() % 2 == 1 { vec![cb] } else { vec![0, cb - 1] };
                    (bytes, [head, papx].concat())
                })
                .collect();
            self.fkp(3, 0x102, 13, &entries)
        }

        /// FKP page `pn`, named by the bin table at FIB `fib_off`, giving each
        /// run of `(bytes, entry)` the next `bytes` of the text and its entry,
        /// none when empty, through offsets `rgb` bytes apart.
        fn fkp(mut self, pn: usize, fib_off: usize, rgb: usize, runs: &[(u32, Vec<u8>)]) -> Self {
            let fkp = &mut self.word_doc[pn * 512..(pn + 1) * 512];
            let mut fc = TEXT_FC as u32;
            let mut at = 0x100;
            for (k, (bytes, entry)) in runs.iter().enumerate() {
                fkp[k * 4..k * 4 + 4].copy_from_slice(&fc.to_le_bytes());
                if !entry.is_empty() {
                    fkp[(runs.len() + 1) * 4 + k * rgb] = u8::try_from(at / 2).unwrap();
                    fkp[at..at + entry.len()].copy_from_slice(entry);
                    at += entry.len().next_multiple_of(2);
                }
                fc += bytes;
            }
            fkp[runs.len() * 4..runs.len() * 4 + 4].copy_from_slice(&fc.to_le_bytes());
            fkp[511] = u8::try_from(runs.len()).unwrap();
            self.table(fib_off, &words(&[TEXT_FC as u32, fc, pn as u32]))
        }

        /// One simple list whose `PlfLst` is `lcb` bytes, its level `lvl`
        /// after them, and the overrides `plf_lfo`.
        fn lists(self, lcb: usize, lvl: &[u8], plf_lfo: &[u8]) -> Self {
            let mut plf = vec![0u8; lcb];
            plf[0] = 1;
            plf[2 + 26] = 0x01;
            let lcb = u32::try_from(lcb).unwrap();
            self.table(0x2E2, &[&plf[..], lvl].concat()).fib(0x2E6, lcb).table(0x2EA, plf_lfo)
        }

        /// A footnote story of `len` CPs after the main text, its reference
        /// at CP 0.
        fn footnote(self, len: u32) -> Self {
            self.fib(0x50, len)
                .table(0xAA, &[words(&[0, 1]), vec![0, 0]].concat())
                .table(0xB2, &words(&[0, len]))
        }

        /// An endnote story of `len` CPs, its reference at CP 0.
        fn endnote(self, len: u32) -> Self {
            self.fib(0x60, len)
                .table(0x20A, &[words(&[0, 1]), vec![0, 0]].concat())
                .table(0x212, &words(&[0, len]))
        }

        fn data(self, data: Vec<u8>) -> Self {
            Self { data: Some(data), ..self }
        }

        /// The compound file, its FIB's `cbMac` the WordDocument stream's
        /// length unless a case sets one.
        fn bytes(self) -> Vec<u8> {
            use std::io::Write;
            let doc = self.covered();
            let cb_mac = doc.cb_mac.unwrap_or_else(|| u32::try_from(doc.word_doc.len()).unwrap());
            let doc = doc.fib(0x40, cb_mac);
            let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
            let streams = [("WordDocument", Some(doc.word_doc)), ("0Table", Some(doc.table))];
            for (name, bytes) in streams.into_iter().chain([("Data", doc.data)]) {
                if let Some(bytes) = bytes {
                    ole.create_stream(name).unwrap().write_all(&bytes).unwrap();
                }
            }
            ole.into_inner().into_inner()
        }
    }

    /// An STSH of paragraph styles `(sti, istd_base, chpx)` from istd 0.
    fn stsh(styles: &[(u16, u16, &[u8])]) -> Vec<u8> {
        let rows: Vec<_> = styles
            .iter()
            .map(|&(sti, istd_base, chpx)| (StyleKind::Paragraph, sti, istd_base, chpx))
            .collect();
        style_sheet(&rows)
    }

    #[derive(Clone, Copy)]
    enum StyleKind {
        Paragraph,
        Character,
    }

    use StyleKind::{Character as Char, Paragraph as Para};

    /// An STSH of styles `(kind, sti, istd_base, chpx)` from istd 0.
    fn style_sheet(styles: &[(StyleKind, u16, u16, &[u8])]) -> Vec<u8> {
        let cstd = u16::try_from(styles.len()).unwrap();
        let mut stsh = [4u16, cstd, 10].map(u16::to_le_bytes).concat();
        for &(kind, sti, istd_base, chpx) in styles {
            let (sgc, cupx) = match kind {
                StyleKind::Paragraph => (1, 2),
                StyleKind::Character => (2, 1),
            };
            let mut std =
                [sti, sgc | (istd_base << 4), cupx, 0, 0, 0, 0].map(u16::to_le_bytes).concat();
            if matches!(kind, StyleKind::Paragraph) {
                std.extend([2u16, 0].map(u16::to_le_bytes).concat());
            }
            std.extend(u16::try_from(chpx.len()).unwrap().to_le_bytes());
            std.extend_from_slice(chpx);
            std.resize(std.len().next_multiple_of(2), 0);
            stsh.extend(u16::try_from(std.len()).unwrap().to_le_bytes());
            stsh.extend(std);
        }
        stsh
    }

    /// An FFN naming `name`, its alternate name absent.
    pub(super) fn ffn(name: &str) -> Vec<u8> {
        let mut ffn = vec![0u8; fonts::FFN_NAME];
        ffn.extend(name.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
        ffn
    }

    /// [MS-DOC] FFN: the offset of `chs`, past `prq` and `wWeight`.
    const FFN_CHS: usize = 3;
    /// [MS-DOC] FFN `chs`: the character set a symbol font declares.
    pub(super) const SYMBOL_CHARSET: u8 = 2;

    /// An FFN naming `name` in the character set `chs`.
    pub(super) fn ffn_in(name: &str, chs: u8) -> Vec<u8> {
        let mut ffn = ffn(name);
        ffn[FFN_CHS] = chs;
        ffn
    }

    /// A `SttbfFfn` of the `entries`, each prefixed by its length.
    pub(super) fn sttbf_ffn(entries: &[Vec<u8>]) -> Vec<u8> {
        let count = u16::try_from(entries.len()).unwrap();
        let mut sttb = [count, 0].map(u16::to_le_bytes).concat();
        for entry in entries {
            sttb.push(u8::try_from(entry.len()).unwrap());
            sttb.extend_from_slice(entry);
        }
        sttb
    }

    pub(super) fn words(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    pub(super) fn utf16(units: &[u16]) -> Vec<u8> {
        units.iter().flat_map(|u| u.to_le_bytes()).collect()
    }

    /// A Clx of the `prcs` and a `PlcPcd` over the CPs `cps` whose pieces are `(fc, prm)`.
    pub(super) fn clx(prcs: &[&[u8]], cps: &[u32], pcds: &[(u32, u16)]) -> Vec<u8> {
        let prcs = prcs.iter().flat_map(|grpprl| {
            let cb = u16::try_from(grpprl.len()).unwrap().to_le_bytes();
            [&[1u8][..], &cb, grpprl].concat()
        });
        let pcds = pcds
            .iter()
            .flat_map(|&(fc, prm)| [&[0u8, 0][..], &fc.to_le_bytes(), &prm.to_le_bytes()].concat());
        let plc: Vec<u8> = words(cps).into_iter().chain(pcds).collect();
        let len = u32::try_from(plc.len()).unwrap().to_le_bytes();
        prcs.chain([2]).chain(len).chain(plc).collect()
    }

    /// A shape container for `spid` with the property ids `props`.
    fn shape(spid: u32, props: &[u16]) -> Vec<u8> {
        let table: Vec<u8> =
            props.iter().flat_map(|id| [&id.to_le_bytes()[..], &[0; 4]].concat()).collect();
        let sp = record(0x0002, 0xF00A, &[&spid.to_le_bytes()[..], &[0; 4]].concat());
        let opt = record((u16::try_from(props.len()).unwrap() << 4) | 3, 0xF00B, &table);
        record(0x000F, 0xF004, &[sp, opt].concat())
    }

    /// A floating shape anchored at CP 0 of `"\x08\r"`, drawn by `drawn`.
    fn floating(spid: u32, drawn: &[u8]) -> Vec<u8> {
        anchoring(DocBuilder::new(b"\x08\r"), 0, spid, &[(0, drawn)]).bytes()
    }

    /// `doc` with a floating shape anchored at CP `cp`, over the drawings
    /// `(dgglbl, drawn)`.
    fn anchoring(doc: DocBuilder, cp: u32, spid: u32, drawings: &[(u8, &[u8])]) -> DocBuilder {
        let mut dgg = record(0x000F, 0xF000, &[]);
        for &(dgglbl, drawn) in drawings {
            dgg.push(dgglbl);
            dgg.extend(record(0x000F, 0xF002, &record(0x000F, 0xF003, drawn)));
        }
        let spa = [words(&[cp, cp + 1, spid]), vec![0; 22]].concat();
        doc.table(0x1DA, &spa).table(0x22A, &dgg)
    }

    /// A document of `text` whose floating shape, drawn by `drawn`, is anchored at CP `cp`.
    fn anchored_at(cp: u32, text: &[u8], drawn: &[u8]) -> Vec<u8> {
        anchoring(DocBuilder::new(text), cp, 7, &[(0, drawn)]).bytes()
    }

    const SHAPE_FIELD: &[u8] = b"\x13 SHAPE \x14\x01\x15\r";

    /// Inline picture data whose art is `art`, for every `\x01` of `text`.
    fn inline_art(text: &[u8], art: &[u8]) -> Vec<u8> {
        let data = [&words(&[6 + u32::try_from(art.len()).unwrap()])[..], &[6, 0], art].concat();
        DocBuilder::new(text).chpx(PIC_LOCATION_CHPX).data(data).bytes()
    }

    /// A document whose main text is the UTF-16 of `text`, with `~` an
    /// unpaired surrogate.
    fn damaged(text: &str) -> Vec<u8> {
        pieced(&[Some(text)]).bytes()
    }

    /// A main document of a UTF-16 piece for each part, with `~` an unpaired
    /// surrogate, and a piece of one CP outside the stream for each `None`.
    fn pieced(parts: &[Option<&str>]) -> DocBuilder {
        let texts: Vec<Vec<u8>> = parts
            .iter()
            .map(|part| {
                let units = part.unwrap_or_default().encode_utf16();
                units
                    .map(|u| if u == u16::from(b'~') { 0xDC00 } else { u })
                    .flat_map(u16::to_le_bytes)
                    .collect()
            })
            .collect();
        let mut cp = 0;
        let pieces: Vec<(u32, &[u8], bool, u16)> = texts
            .iter()
            .map(|text| {
                cp += u32::try_from(text.len() / 2).unwrap().max(1);
                (cp, text.as_slice(), true, 0)
            })
            .collect();
        DocBuilder::new(b"").fib(0x4C, cp).pieces(&pieces).truncated()
    }

    fn fuel(bytes: &[u8]) -> u64 {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, bytes).unwrap();
        parse_ctx.fuel()
    }

    type Reported = (DiagnosticCode, Region, u64);

    fn reported(bytes: &[u8]) -> Vec<Reported> {
        parsed_with(bytes).1
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of losses by story")]
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

        let chpx_with = |flag: u8| [&[flag, 0x08, 1][..], PIC_LOCATION_CHPX].concat();
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
                "a piece outside the stream",
                DocBuilder::new(b"").fib(0x4C, 3).pieces(&[(3, b"", false, 0)]).truncated().bytes(),
                vec![(TruncatedRecords, Region::Body, 1)],
            ),
            (
                "a footnote piece outside the stream",
                DocBuilder::new(b"")
                    .fib(0x4C, 2)
                    .footnote(2)
                    .pieces(&[(2, b"a\r", false, 0), (4, b"", true, 0)])
                    .truncated()
                    .bytes(),
                vec![(TruncatedRecords, Region::Note, 1), (DanglingReference, Region::Body, 1)],
            ),
            (
                "a header piece outside the stream",
                DocBuilder::new(b"")
                    .fib(0x4C, 2)
                    .fib(0x54, 2)
                    .pieces(&[(2, b"a\r", false, 0), (4, b"", true, 0)])
                    .truncated()
                    .bytes(),
                vec![],
            ),
            (
                "a piece outside the stream from a header into an endnote",
                DocBuilder::new(b"")
                    .fib(0x4C, 2)
                    .fib(0x54, 2)
                    .endnote(2)
                    .pieces(&[(2, b"a\r", false, 0), (6, b"", true, 0)])
                    .truncated()
                    .bytes(),
                vec![(TruncatedRecords, Region::Note, 1), (DanglingReference, Region::Body, 1)],
            ),
            (
                "a header piece outside the stream, then endnote text outside it",
                DocBuilder::new(b"")
                    .fib(0x4C, 2)
                    .fib(0x54, 4)
                    .endnote(2)
                    .pieces(&[(2, b"a\r", false, 0), (6, b"", true, 0), (8, b"", true, 0)])
                    .truncated()
                    .bytes(),
                vec![(TruncatedRecords, Region::Note, 1), (DanglingReference, Region::Body, 1)],
            ),
            (
                "endnote text outside the stream that no reference reaches",
                DocBuilder::new(b"")
                    .fib(0x4C, 2)
                    .fib(0x60, 2)
                    .pieces(&[(2, b"a\r", false, 0), (4, b"", true, 0)])
                    .truncated()
                    .bytes(),
                vec![],
            ),
            (
                "an unpaired surrogate in an anchor link's id",
                damaged("\x13 HYPERLINK \\l \"a~\" \x14a\x15\r"),
                vec![(DanglingReference, Region::Body, 1)],
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
                "a header table one entry short of its first section",
                DocBuilder::new(b"a\r").fib(0x54, 8).table(0xF2, &hdd[..52]).bytes(),
                vec![],
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
            (
                "an embedded object in a field instruction",
                DocBuilder::new(b"\x13 EMBED \x01\x14\x15\r").chpx(&chpx_with(0x0A)).bytes(),
                vec![],
            ),
            (
                "an embedded object in a field result",
                DocBuilder::new(b"\x13 EMBED x \x14\x01\x15\r").chpx(&chpx_with(0x0A)).bytes(),
                vec![(EmbeddedObject, Region::Body, 1)],
            ),
            (
                "an embedded object in a field result inside a field instruction",
                DocBuilder::new(b"\x13 IF \x13 EMBED x \x14\x01\x15 \x14\x15\r")
                    .chpx(&chpx_with(0x0A))
                    .bytes(),
                vec![],
            ),
            ("field data", DocBuilder::new(b"\x01\r").chpx(&chpx_with(0x06)).bytes(), vec![]),
            (
                "a picture with no Data stream",
                DocBuilder::new(b"\x01\r").chpx(PIC_LOCATION_CHPX).bytes(),
                vec![(ImageUnavailable, Region::Body, 1)],
            ),
            (
                "a picture whose embedded-object flag is off",
                DocBuilder::new(b"\x01\r")
                    .chpx(&[&[0x0A, 0x08, 0][..], PIC_LOCATION_CHPX].concat())
                    .bytes(),
                vec![(ImageUnavailable, Region::Body, 1)],
            ),
            (
                "a picture in a footnote with no Data stream",
                DocBuilder::new(b"a\r\x01\r")
                    .fib(0x4C, 2)
                    .footnote(2)
                    .chpx(PIC_LOCATION_CHPX)
                    .bytes(),
                vec![(ImageUnavailable, Region::Note, 1)],
            ),
            (
                "a floating shape's inline stand-in with no Data stream",
                DocBuilder::new(SHAPE_FIELD).chpx(PIC_LOCATION_CHPX).bytes(),
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
                anchoring(
                    DocBuilder::new(b"\x08\r"),
                    0,
                    7,
                    &[(1, &shape(7, &[0x0181])), (0, &shape(7, &[0x4104]))],
                )
                .bytes(),
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
                "a floating picture anchored in a field instruction",
                anchored_at(6, b"\x13 REF \x08 \x14x\x15\r", &shape(7, &[0x4104])),
                vec![],
            ),
            (
                "a floating shape anchored in a field instruction",
                anchored_at(6, b"\x13 REF \x08 \x14x\x15\r", &shape(7, &[0x0181])),
                vec![],
            ),
            (
                "a floating shape no drawing holds, anchored in a field instruction",
                anchored_at(6, b"\x13 REF \x08 \x14x\x15\r", &shape(8, &[0x4104])),
                vec![],
            ),
            (
                "a floating picture anchored in a field result",
                anchored_at(7, b"\x13 REF \x14\x08\x15\r", &shape(7, &[0x4104])),
                vec![(ImageUnplaced, Region::Body, 1)],
            ),
            (
                "a floating shape anchored in a field result",
                anchored_at(7, b"\x13 REF \x14\x08\x15\r", &shape(7, &[0x0181])),
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
                "a check box that closes with no separator",
                DocBuilder::new(b"\x13 FORMCHECKBOX \x15\r").bytes(),
                vec![(FormControlUnread, Region::Body, 1)],
            ),
            (
                "a check box with no separator, a picture in its instruction",
                DocBuilder::new(b"\x13 FORMCHECKBOX \x01\x15\r").chpx(PIC_LOCATION_CHPX).bytes(),
                vec![(FormControlUnread, Region::Body, 1)],
            ),
            (
                "a check box in a field result",
                DocBuilder::new(b"\x13 REF a \x14\x13 FORMCHECKBOX \x14\x15\x15\r").bytes(),
                vec![(FormControlUnread, Region::Body, 1)],
            ),
            (
                "a check box in a field instruction",
                DocBuilder::new(b"\x13 IF \x13 FORMCHECKBOX \x14\x15 \x14x\x15\r").bytes(),
                vec![],
            ),
            (
                "a check box in a field result inside a field instruction",
                DocBuilder::new(
                    b"\x13 IF \x13 REF a \x14\x13 FORMCHECKBOX \x14\x15\x15 \x14x\x15\r",
                )
                .bytes(),
                vec![],
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
                vec![(PartUnreadable, Region::Body, 1)],
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
    fn a_damaged_list_level_is_reported_where_a_label_shows_it() {
        use DiagnosticCode::InvalidEncoding;
        let mut lvl = vec![0u8; 28];
        lvl[0] = 1;
        lvl[6] = 4;
        lvl.extend_from_slice(&5u16.to_le_bytes());
        lvl.extend(utf16(&[0xD835, 0xDC00, 0xDC00, 0x0000, u16::from(b'.')]));
        let numbered = |ilfo: u8| vec![0x0B, 0x46, ilfo, 0, 0x0A, 0x26, 0];
        let heading = [numbered(1), vec![0x40, 0x26, 0]].concat();
        let one_lfo = words(&[1, 0, 0, 0, 0, u32::MAX]);
        let doc = |text: &[u8]| DocBuilder::new(text).lists(30, &lvl, &one_lfo);
        let overriding = [
            words(&[2, 0, 0, 0, 0, 0, 0, 0]),
            vec![1, 0, 0, 0],
            words(&[u32::MAX, u32::MAX, 0]),
            vec![0x20, 0, 0, 0],
            lvl.clone(),
        ]
        .concat();
        let cases: [(&str, Vec<u8>, Vec<Reported>); 4] = [
            (
                "two body paragraphs and a footnote",
                doc(b"a\rb\rc\r").papx(&[(6, &numbered(1))]).fib(0x4C, 4).footnote(2).bytes(),
                vec![(InvalidEncoding, Region::Body, 1), (InvalidEncoding, Region::Note, 1)],
            ),
            (
                "a numbered heading",
                doc(b"a\r").papx(&[(2, &heading)]).bytes(),
                vec![(InvalidEncoding, Region::Body, 1)],
            ),
            (
                "a list level and an override level at one offset of their tables",
                DocBuilder::new(b"a\rb\r")
                    .lists(overriding.len() - lvl.len(), &lvl, &overriding)
                    .papx(&[(2, &numbered(1)), (2, &numbered(2))])
                    .bytes(),
                vec![(InvalidEncoding, Region::Body, 2)],
            ),
            ("no paragraph", doc(b"a\r").bytes(), vec![]),
        ];
        let converter = |on_loss| {
            crate::Converter::new(crate::Config { on_loss, ..Default::default() }).unwrap()
        };
        for (case, bytes, expected) in cases {
            assert_eq!(reported(&bytes), expected, "{case}");
            let input = crate::Input::new(&bytes, crate::Format::Doc);
            let markdown = converter(crate::OnLoss::Report).convert(input.clone()).unwrap();
            let shown = markdown.markdown().contains("\u{1D400}\u{FFFD}1.");
            assert_eq!(shown, !expected.is_empty(), "{case}");
            assert_eq!(converter(crate::OnLoss::Fail).convert(input).is_ok(), !shown, "{case}");
        }
    }

    #[test]
    fn every_drawing_record_walked_is_charged_once() {
        let run = record(0x000F, 0xF003, &[shape(1, &[]), shape(2, &[0x0104])].concat());
        let parse_ctx = ParseCtx::default();
        let kinds = shape_kinds(&parse_ctx, &run).unwrap();
        assert_eq!(kinds, HashMap::from([(1, ShapeKind::Graphic), (2, ShapeKind::Picture)]));
        assert_eq!(parse_ctx.fuel(), 7);
        let limits = crate::limits::Limits { max_work: 6, ..Default::default() };
        assert_matches!(
            shape_kinds(&ParseCtx::new(&limits), &run),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 6 })
        );
    }

    #[test]
    fn an_inline_picture_past_the_contexts_asset_total_is_a_limit() {
        let bytes =
            DocBuilder::new(b"\x01\r").chpx(PIC_LOCATION_CHPX).data(picture_data(4096)).bytes();
        let limits = crate::limits::Limits { max_asset_total_bytes: 4096, ..Default::default() };
        let doc = parse(&ParseCtx::new(&limits), &bytes).unwrap();
        assert_eq!(doc.assets[0].bytes.len(), 4096);
        let limits = crate::limits::Limits { max_asset_total_bytes: 4095, ..Default::default() };
        let err = parse(&ParseCtx::new(&limits), &bytes).unwrap_err();
        assert_matches!(
            err,
            ConvertError::ResourceLimit { limit: Limit::AssetTotalBytes, max: 4095 }
        );
    }

    #[test]
    fn a_picture_in_a_field_instruction_adds_no_asset() {
        let placed = vec![(DiagnosticCode::ImagePlaceholder, Region::Body, 1)];
        let cases: [(&str, &[u8], usize, Vec<Reported>); 3] = [
            ("in an instruction", b"\x13 INCLUDEPICTURE \x01 \x14\x15\r", 0, vec![]),
            (
                "in a result inside an instruction",
                b"\x13 IF \x13 INCLUDEPICTURE x \x14\x01\x15 \x14\x15\r",
                0,
                vec![],
            ),
            ("in a result", b"\x13 INCLUDEPICTURE x \x14\x01\x15\r", 1, placed),
        ];
        for (case, text, assets, expected) in cases {
            let bytes =
                DocBuilder::new(text).chpx(PIC_LOCATION_CHPX).data(picture_data(64)).bytes();
            let (doc, reported) = parsed_with(&bytes);
            assert_eq!((doc.assets.len(), reported), (assets, expected), "{case}");
        }
    }

    #[test]
    fn a_word_document_that_is_no_stream_is_malformed_and_one_absent_is_missing() {
        let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        ole.create_storage("WordDocument").unwrap();
        let storage = ole.into_inner().into_inner();
        let absent = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        let absent = absent.into_inner().into_inner();
        let error = |bytes: &[u8]| parse(&ParseCtx::default(), bytes).unwrap_err();
        assert_matches!(error(&storage), ConvertError::Malformed { .. });
        assert_matches!(error(&absent), ConvertError::MissingPart { .. });
    }

    fn ole_stream(ole: &mut cfb::CompoundFile<Cursor<&[u8]>>, name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut ole.open_stream(name).unwrap(), &mut out).unwrap();
        out
    }

    #[test]
    fn a_failure_in_the_paragraph_run_pass_propagates() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/doc/text.doc");
        let bytes = std::fs::read(path).unwrap();
        let mut ole = cfb::CompoundFile::open(Cursor::new(bytes.as_slice())).unwrap();
        let word_doc = ole_stream(&mut ole, "WordDocument");
        let flags = get_u16(&word_doc, 0x0A).unwrap();
        let table = ole_stream(&mut ole, if flags & 0x0200 != 0 { "1Table" } else { "0Table" });
        let fib = Fib::read(&word_doc).unwrap();
        let fuel = |fib_off, kind| {
            let parse_ctx = ParseCtx::default();
            parse_fkps(&parse_ctx, fib, &word_doc, &table, fib_off, kind, &[]).unwrap();
            parse_ctx.fuel()
        };
        let text = {
            let parse_ctx = ParseCtx::default();
            let clx = (fib.size(Fib::FC_CLX), fib.size(Fib::LCB_CLX));
            let coverage = Coverage {
                terminal_cp: Stories::read(fib).unwrap().terminal_cp().unwrap(),
                cb_mac: fib.size(Fib::CB_MAC),
            };
            PieceTable::parse(&parse_ctx, &table, clx.0, clx.1, coverage).unwrap();
            parse_ctx.fuel()
        };
        let chpx = fuel(0xFA, FkpKind::Chpx);
        assert!(text > 0 && fuel(0x102, FkpKind::Papx) > 0);
        let before_paragraphs = text + chpx;
        let limits = crate::limits::Limits {
            max_work: before_paragraphs,
            ..crate::limits::Limits::default()
        };
        let err = parse(&ParseCtx::new(&limits), &bytes).unwrap_err();
        assert_matches!(
            err,
            ConvertError::ResourceLimit { limit: Limit::Work, max }
                if max == before_paragraphs
        );
    }

    #[test]
    fn a_style_cycle_converts_one_way_on_every_call_and_thread_count() {
        let styles = stsh(&[(0, 0x0FFF, &[]), (0x0FFE, 2, &[]), (1, 1, &[])]);
        let doc = DocBuilder::new(b"a\rb\r")
            .table(0xA2, &styles)
            .styled_papx(&[(2, 1, &[]), (2, 2, &[])])
            .bytes();
        for threads in [1, 4, 1, 4] {
            let config = crate::Config { threads, ..Default::default() };
            let converted = crate::Converter::new(config)
                .unwrap()
                .convert(crate::Input::new(&doc, crate::Format::Doc))
                .unwrap();
            assert_eq!(converted.markdown(), "a\n\n# b\n", "{threads} threads");
        }
        assert_eq!(reported(&doc), [(DiagnosticCode::StyleCycle, Region::Container, 1)]);
    }

    fn word_document_alone(word_doc: &[u8]) -> Result<Document, ConvertError> {
        use std::io::Write;
        let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        ole.create_stream("WordDocument").unwrap().write_all(word_doc).unwrap();
        parse(&ParseCtx::default(), &ole.into_inner().into_inner())
    }

    #[test]
    fn a_word_6_fib_fails_on_its_magic_before_its_piece_table() {
        let word_doc = [&0xA5DCu16.to_le_bytes()[..], &[0; FIB_MIN - 2]].concat();
        assert_eq!(
            word_document_alone(&word_doc).unwrap_err(),
            ConvertError::malformed_part("WordDocument", "invalid FIB magic")
        );
    }

    #[test]
    fn a_word_document_stream_shorter_than_its_fib_is_malformed() {
        let doc = word_document_alone;
        let fib = |len: usize| [&[0xEC, 0xA5][..], &vec![0; len - 2]].concat();
        for word_doc in [vec![0xEC, 0xA5, 0, 0], fib(0x381)] {
            let refused = doc(&word_doc);
            assert_eq!(
                refused.unwrap_err(),
                ConvertError::malformed_part("WordDocument", "FIB cut short"),
                "{}",
                word_doc.len()
            );
        }
        assert_eq!(
            doc(&fib(0x382)).unwrap_err(),
            ConvertError::malformed_part("WordDocument", "no piece table")
        );
    }

    /// `text` whose paragraph properties are `papx`, under one bulleted list of `lsid`.
    fn bulleted_doc(text: &[u8], lsid: u32, papx: &[(u32, &[u8])]) -> Vec<u8> {
        let mut plf = [0u8; 30];
        plf[0] = 1;
        plf[2..6].copy_from_slice(&lsid.to_le_bytes());
        plf[28] = 0x01;
        let mut bullet = [0u8; 30];
        bullet[4] = 23;
        DocBuilder::new(text)
            .table(0x2E2, &[&plf[..], &bullet].concat())
            .fib(0x2E6, 30)
            .table(0x2EA, &words(&[1, lsid, 0, 0, 0, u32::MAX]))
            .papx(papx)
            .bytes()
    }

    #[test]
    fn an_unknown_lfo_keeps_its_own_list_beside_the_lsid_its_stand_in_once_took() {
        let bulleted = |ilfo: u8| vec![0x0B, 0x46, ilfo, 0, 0x0A, 0x26, 0];
        let papx: [(u32, &[u8]); 2] = [(2, &bulleted(1)), (2, &bulleted(2))];
        let doc = bulleted_doc(b"a\rb\r", u32::MAX ^ 2, &papx);
        let parsed = parse(&ParseCtx::default(), &doc).unwrap();
        let [Block::List(known), Block::List(unknown)] = parsed.blocks.as_slice() else {
            panic!("{:?}", parsed.blocks)
        };
        assert_eq!((known.items.len(), unknown.items.len()), (1, 1));
    }

    #[test]
    fn a_heading_in_a_bulleted_list_carries_no_label() {
        let heading = [0x0B, 0x46, 1, 0, 0x0A, 0x26, 0, 0x40, 0x26, 0];
        let doc = bulleted_doc(b"a\r", 0, &[(2, &heading)]);
        assert_eq!(converted(&doc), ("# a\n".into(), vec![]));
    }

    const DELETED: &[u8] = &[0x00, 0x08, 1];
    const HIDDEN: &[u8] = &[0x3C, 0x08, 1];
    const SHOWN: &[u8] = &[0x3C, 0x08, 0];
    const BOLD: &[u8] = &[0x35, 0x08, 1];
    const ITALIC: &[u8] = &[0x36, 0x08, 1];
    const BOLD_SUPER: &[u8] = &[0x35, 0x08, 1, 0x48, 0x2A, 1];
    const BASELINE: &[u8] = &[0x48, 0x2A, 0];
    const SUBSCRIPT: &[u8] = &[0x48, 0x2A, 2];
    const INVERT_BOLD: &[u8] = &[0x35, 0x08, 0x81];
    const IN_TABLE: &[u8] = &[0x16, 0x24, 1];
    const ROW_END: &[u8] = &[0x16, 0x24, 1, 0x17, 0x24, 1];

    fn cistd(istd: u8) -> [u8; 4] {
        [0x30, 0x4A, istd, 0]
    }

    /// `a` as one paragraph of style `istd` under `styles`, with run properties `chpx`.
    fn run_in(styles: &[u8], istd: u16, chpx: &[u8]) -> (String, Vec<Reported>) {
        let doc = DocBuilder::new(b"a\r").table(0xA2, styles).styled_papx(&[(2, istd, &[])]);
        converted(&doc.chpx(chpx).bytes())
    }

    /// `a` as the one piece of a paragraph in style 0, whose complex modifier is `prc`.
    fn piece_in(styles: &[u8], chpx: &[u8], prc: &[u8]) -> (String, Vec<Reported>) {
        let doc = DocBuilder::new(b"").fib(0x4C, 2).table(0xA2, styles);
        converted(&doc.clx(&[prc], &[(2, b"a\r", false, 1)]).chpx(chpx).bytes())
    }

    type TextRuns<'a> = &'a [(&'a str, &'a [u8])];

    /// A document whose main text is the runs' text.
    fn runs_doc(runs: TextRuns<'_>) -> DocBuilder {
        let text: String = runs.iter().map(|&(text, _)| text).collect();
        let chpx: Vec<(u32, &[u8])> = runs
            .iter()
            .map(|&(text, grpprl)| (u32::try_from(text.len()).unwrap(), grpprl))
            .collect();
        DocBuilder::new(text.as_bytes()).chpx_runs(&chpx)
    }

    fn parsed_with(bytes: &[u8]) -> (Document, Vec<Reported>) {
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, bytes).unwrap();
        let diagnostics =
            crate::converter::diagnostics_of(parse_ctx, &doc, crate::ImagePolicy::Placeholder);
        (doc, diagnostic_rows(&diagnostics.unwrap()))
    }

    fn converted(bytes: &[u8]) -> (String, Vec<Reported>) {
        let (doc, reported) = parsed_with(bytes);
        (crate::render::markdown::document_to_markdown(&doc), reported)
    }

    #[test]
    fn vertical_position_runs_set_and_reset_without_toggle_semantics() {
        let doc = runs_doc(&[
            ("x", &[]),
            ("2", &[0x48, 0x2A, 1]),
            ("H", &[0x48, 0x2A, 0]),
            ("2", &[0x48, 0x2A, 2]),
            ("O\r", &[]),
        ]);
        assert_eq!(converted(&doc.bytes()), ("x<sup>2</sup>H<sub>2</sub>O\n".into(), vec![]));
        for invalid in [3, 0x80, 0x81, 0xFF] {
            let modifiers = [0x48, 0x2A, 2, 0x48, 0x2A, invalid];
            let doc = runs_doc(&[("a", &modifiers), ("b\r", &[])]);
            assert_eq!(converted(&doc.bytes()), ("<sub>a</sub>b\n".into(), vec![]));
        }
    }

    #[test]
    fn vertical_position_styles_inherit_and_direct_formatting_overrides() {
        let raised: &[u8] = &[0x48, 0x2A, 1];
        let lowered: &[u8] = &[0x48, 0x2A, 2];
        let baseline: &[u8] = &[0x48, 0x2A, 0];
        let styles = style_sheet(&[
            (Para, 0, 0x0FFF, raised),
            (Para, 0x0FFE, 0, &[]),
            (Para, 0x0FFE, 1, lowered),
            (Para, 0x0FFE, 2, baseline),
            (Char, 0x0FFE, 0x0FFF, lowered),
        ]);
        for (istd, chpx, expected) in [
            (1, &[][..], "<sup>a</sup>\n"),
            (2, &[][..], "<sub>a</sub>\n"),
            (3, &[][..], "a\n"),
            (1, lowered, "<sub>a</sub>\n"),
            (2, raised, "<sup>a</sup>\n"),
            (1, baseline, "a\n"),
            (1, &[0x30, 0x4A, 4, 0][..], "<sub>a</sub>\n"),
            (1, &[0x30, 0x4A, 4, 0, 0x48, 0x2A, 0][..], "a\n"),
        ] {
            assert_eq!(run_in(&styles, istd, chpx), (expected.into(), vec![]), "{istd}: {chpx:?}");
        }
    }

    #[test]
    fn vertical_position_piece_modifiers_override_character_runs() {
        let pieces: [(u32, &[u8], bool, u16); 4] = [
            (1, b"a", false, (0x68 << 1) | (2 << 8)),
            (2, b"b", false, 1),
            (3, b"c", false, 0x68 << 1),
            (4, b"\r", false, 0),
        ];
        let doc = DocBuilder::new(b"")
            .fib(0x4C, 4)
            .clx(&[&[0x48, 0x2A, 1]], &pieces)
            .chpx(&[0x48, 0x2A, 2]);
        assert_eq!(converted(&doc.bytes()), ("<sub>a</sub><sup>b</sup>c\n".into(), vec![]));
    }

    #[test]
    fn vertical_position_character_styles_resolve_before_run_properties() {
        let styles = style_sheet(&[
            (Para, 0, 0x0FFF, &[]),
            (Char, 0x0FFE, 0x0FFF, &[0x48, 0x2A, 1]),
            (Char, 0x0FFE, 1, &[]),
        ]);
        for (modifier, expected) in [
            (&[][..], "<sup>a</sup>\n"),
            (&[0x48, 0x2A, 0][..], "a\n"),
            (&[0x48, 0x2A, 2][..], "<sub>a</sub>\n"),
        ] {
            let chpx = [&[0x30, 0x4A, 2, 0][..], modifier].concat();
            assert_eq!(run_in(&styles, 0, &chpx), (expected.into(), vec![]));
        }
    }

    #[test]
    fn vertical_position_keeps_note_references_and_geometric_offsets_separate() {
        let doc = runs_doc(&[("\x02", &[0x48, 0x2A, 1]), ("a\rn\r", &[])]).fib(0x4C, 3).footnote(2);
        assert_eq!(converted(&doc.bytes()), ("[^1]a\n\n[^1]: n\n".into(), vec![]));
        let doc = DocBuilder::new(b"a\r").chpx(&[0x45, 0x48, 6, 0]);
        assert_eq!(converted(&doc.bytes()), ("a\n".into(), vec![]));
    }

    #[test]
    fn a_character_style_adds_to_the_paragraph_style_it_sits_in() {
        let styles = style_sheet(&[
            (Para, 0, 0x0FFF, BOLD_SUPER),
            (Char, 0x0FFE, 0x0FFF, ITALIC),
            (Char, 0x0FFE, 0x0FFF, &[]),
            (Char, 0x0FFE, 0x0FFF, BASELINE),
            (Char, 0x0FFE, 0x0FFF, SUBSCRIPT),
        ]);
        let cases: [(&str, Vec<u8>, &str); 7] = [
            ("no character style", vec![], "<sup>**a**</sup>"),
            ("italic only", cistd(1).into(), "<sup>**_a_**</sup>"),
            ("nothing set", cistd(2).into(), "<sup>**a**</sup>"),
            ("explicit baseline", cistd(3).into(), "**a**"),
            ("subscript", cistd(4).into(), "<sub>**a**</sub>"),
            ("direct position after", [&cistd(2)[..], SUBSCRIPT].concat(), "<sub>**a**</sub>"),
            ("direct position before", [SUBSCRIPT, &cistd(2)].concat(), "<sup>**a**</sup>"),
        ];
        for (case, chpx, shown) in cases {
            assert_eq!(run_in(&styles, 0, &chpx), (format!("{shown}\n"), vec![]), "{case}");
        }
    }

    #[test]
    fn a_piece_modifier_follows_the_run_and_a_style_in_it_resets_the_run() {
        let styles = style_sheet(&[(Para, 0, 0x0FFF, &[]), (Char, 0x0FFE, 0x0FFF, ITALIC)]);
        let pieces: [(u32, &[u8], bool, u16); 3] = [
            (2, b"a\r", false, (0x68 << 1) | (2 << 8)),
            (4, b"b\r", false, 1),
            (6, b"c\r", false, 0),
        ];
        let doc = DocBuilder::new(b"").fib(0x4C, 6).table(0xA2, &styles);
        let doc = doc.clx(&[&cistd(1)], &pieces).chpx(BOLD);
        let expected = "<sub>**a**</sub>\n\n_b_\n\n**c**\n";
        assert_eq!(converted(&doc.bytes()), (expected.into(), vec![]));
    }

    #[test]
    fn toggles_resolve_against_the_paragraph_and_character_style_in_force_at_their_position() {
        let styles = style_sheet(&[
            (Para, 0, 0x0FFF, BOLD),
            (Char, 0x0FFE, 0x0FFF, ITALIC),
            (Char, 0x0FFE, 0x0FFF, INVERT_BOLD),
            (Para, 0x0FFE, 0x0FFF, &[]),
        ]);
        let cases: [(&str, u16, Vec<u8>, &str); 8] = [
            ("inverts the paragraph's bold", 0, [&cistd(1)[..], INVERT_BOLD].concat(), "_a_"),
            (
                "matches the paragraph's bold",
                0,
                [&cistd(1)[..], &[0x35, 8, 0x80]].concat(),
                "**_a_**",
            ),
            ("direct bold before the reset", 3, [BOLD, &cistd(1)].concat(), "_a_"),
            ("direct bold after the reset", 3, [&cistd(1)[..], BOLD].concat(), "**_a_**"),
            ("matches an inverted base", 0, [&cistd(2)[..], &[0x35, 8, 0x80]].concat(), "a"),
            ("inverts an inverted base", 0, [&cistd(2)[..], INVERT_BOLD].concat(), "**a**"),
            ("inverts it over a plain paragraph", 3, [&cistd(2)[..], INVERT_BOLD].concat(), "a"),
            ("inverts directly", 3, INVERT_BOLD.to_vec(), "**a**"),
        ];
        for (case, istd, chpx, shown) in cases {
            assert_eq!(run_in(&styles, istd, &chpx), (format!("{shown}\n"), vec![]), "{case}");
        }
    }

    #[test]
    fn style_layers_compose_child_after_parent_and_one_layer_resolves_once() {
        let twice = [INVERT_BOLD, INVERT_BOLD].concat();
        let invalid_after = [INVERT_BOLD, &[0x35, 8, 0x7F]].concat();
        let on_then_invalid = [BOLD, &[0x35, 8, 2]].concat();
        let styles = style_sheet(&[
            (Para, 0, 0x0FFF, &[]),
            (Para, 0x0FFE, 0x0FFF, BOLD),
            (Char, 0x0FFE, 0x0FFF, INVERT_BOLD),
            (Char, 0x0FFE, 2, INVERT_BOLD),
            (Char, 0x0FFE, 3, &[]),
            (Char, 0x0FFE, 0x0FFF, &twice),
            (Char, 0x0FFE, 0x0FFF, &invalid_after),
            (Char, 0x0FFE, 0x0FFF, &on_then_invalid),
        ]);
        let layers = [
            ("one inversion", cistd(2).to_vec(), "**a**", "a"),
            ("an inversion inherited and repeated", cistd(3).to_vec(), "a", "**a**"),
            ("an empty layer below them", cistd(4).to_vec(), "a", "**a**"),
            ("two operands in one layer", cistd(5).to_vec(), "**a**", "a"),
            ("an invalid operand after a valid one", cistd(6).to_vec(), "**a**", "a"),
            ("an invalid operand after bold on", cistd(7).to_vec(), "**a**", "**a**"),
            ("two direct operands", twice, "**a**", "a"),
            ("a direct invalid operand after a valid one", invalid_after, "**a**", "a"),
        ];
        for (case, chpx, on_plain, on_bold) in layers {
            for (paragraph, shown) in [(0, on_plain), (1, on_bold)] {
                let expected = (format!("{shown}\n"), vec![]);
                assert_eq!(run_in(&styles, paragraph, &chpx), expected, "{case} over {paragraph}");
            }
        }
    }

    #[test]
    fn a_style_reset_keeps_a_deletion_and_clears_hiding() {
        use DiagnosticCode::{HiddenOmitted, RevisionOmitted};
        let styles = style_sheet(&[(Para, 0, 0x0FFF, &[]), (Char, 0x0FFE, 0x0FFF, ITALIC)]);
        let reset = cistd(1);
        let cases: [(&str, Vec<u8>, &str, Vec<Reported>); 5] = [
            ("deleted before", [DELETED, &reset].concat(), "", omitted(RevisionOmitted, 1)),
            ("deleted after", [&reset[..], DELETED].concat(), "", omitted(RevisionOmitted, 1)),
            ("hidden before", [HIDDEN, &reset].concat(), "_a_\n", vec![]),
            ("hidden after", [&reset[..], HIDDEN].concat(), "", omitted(HiddenOmitted, 1)),
            (
                "inverted after",
                [&reset[..], &[0, 8, 0x81]].concat(),
                "",
                omitted(RevisionOmitted, 1),
            ),
        ];
        for (case, chpx, shown, reported) in cases {
            assert_eq!(run_in(&styles, 0, &chpx), (shown.into(), reported), "{case}");
        }
        for (case, chpx, shown, reported) in [
            ("deleted", DELETED, "", omitted(RevisionOmitted, 1)),
            ("hidden", HIDDEN, "_a_\n", vec![]),
        ] {
            assert_eq!(
                piece_in(&styles, chpx, &reset),
                (shown.into(), reported),
                "{case} in a piece's"
            );
        }
    }

    #[test]
    fn a_style_reference_to_no_character_style_resets_and_is_reported_in_its_story() {
        let styles = style_sheet(&[
            (Para, 0, 0x0FFF, BOLD),
            (Para, 0x0FFE, 0x0FFF, ITALIC),
            (Char, 0x0FFE, 0x0FFF, ITALIC),
            (Para, 0x0FFE, 0x0FFF, &[]),
        ]);
        let unreadable =
            |region, count| vec![(DiagnosticCode::FormattingUnreadable, region, count)];
        for (case, target) in [("a paragraph style", 1), ("a style past the sheet", 9)] {
            let chpx = [SUBSCRIPT, &cistd(target)].concat();
            let expected = ("**a**\n".into(), unreadable(Region::Body, 1));
            assert_eq!(run_in(&styles, 0, &chpx), expected, "{case}");
        }
        let missing = cistd(9);
        let doc = runs_doc(&[("a", &missing), ("b", &missing), ("c\r", &cistd(2))])
            .table(0xA2, &styles)
            .styled_papx(&[(4, 3, &[])]);
        assert_eq!(converted(&doc.bytes()), ("a&#98;_c_\n".into(), unreadable(Region::Body, 2)));
        let doc = runs_doc(&[("\x02a\r", &[]), ("n\r", &missing)]).fib(0x4C, 3).footnote(2);
        assert_eq!(converted(&doc.bytes()).1, unreadable(Region::Note, 1));
    }

    const FORBIDDEN_IN_STYLES: [(&str, &[u8]); 21] = [
        ("sprmCFRMarkDel", DELETED),
        ("sprmCFRMarkIns", &[0x01, 0x08, 1]),
        ("sprmCFFldVanish", &[0x02, 0x08, 1]),
        ("sprmCFData", &[0x06, 0x08, 1]),
        ("sprmCFOle2", &[0x0A, 0x08, 1]),
        ("sprmCFSpecVanish", &[0x18, 0x08, 1]),
        ("sprmCFSpec", &[0x55, 0x08, 1]),
        ("sprmCIstd", &[0x30, 0x4A, 1, 0]),
        ("sprmCIstdPermute", &[0x31, 0xCA, 1, 0]),
        ("sprmCPlain", &[0x33, 0x2A, 0]),
        ("sprmCMajority", &[0x47, 0xCA, 1, 0]),
        ("sprmCDispFldRMark", &[0x62, 0xCA, 1, 0]),
        ("sprmCIdslRMarkDel", &[0x67, 0x48, 0, 0]),
        ("sprmCLbcCRJ", &[0x79, 0x28, 0]),
        ("sprmCPbiIBullet", &[0x87, 0x68, 0, 0, 0, 0]),
        ("sprmCPbiGrf", &[0x88, 0x48, 0, 0]),
        ("sprmCPicLocation", &[0x03, 0x6A, 0, 0, 0, 0]),
        ("sprmCSymbol", &[0x09, 0x6A, 0, 0, 0x62, 0]),
        ("sprmCIdctHint", &[0x6F, 0x28, 2]),
        ("sprmCFBiDi", &[0x5A, 0x08, 1]),
        ("sprmCFComplexScripts", &[0x82, 0x08, 1]),
    ];

    #[test]
    fn a_style_ignores_the_properties_the_format_forbids_there_and_is_reported_once() {
        let unreadable = vec![(DiagnosticCode::FormattingUnreadable, Region::Container, 1)];
        for (name, forbidden) in FORBIDDEN_IN_STYLES {
            let chpx = [BOLD, forbidden, forbidden].concat();
            let paragraph = style_sheet(&[(Para, 0, 0x0FFF, &chpx)]);
            let character = style_sheet(&[(Para, 0, 0x0FFF, &[]), (Char, 0x0FFE, 0x0FFF, &chpx)]);
            for (case, styles, run) in
                [("paragraph", paragraph, &[][..]), ("character", character, &cistd(1)[..])]
            {
                let expected = ("**a**\n".into(), unreadable.clone());
                assert_eq!(run_in(&styles, 0, run), expected, "{name} in a {case} style");
            }
        }
    }

    #[test]
    fn a_deletion_in_a_style_is_no_base_for_a_deletion_toggle() {
        let styles = style_sheet(&[(Para, 0, 0x0FFF, DELETED), (Char, 0x0FFE, 0x0FFF, DELETED)]);
        let (shown, reported) = run_in(&styles, 0, &[&cistd(1)[..], &[0, 8, 0x81]].concat());
        let mut expected = vec![
            (DiagnosticCode::RevisionOmitted, Region::Body, 1),
            (DiagnosticCode::FormattingUnreadable, Region::Container, 2),
        ];
        expected.sort();
        assert_eq!((shown.as_str(), reported), ("", expected));
    }

    #[test]
    fn each_style_that_carries_a_forbidden_property_is_reported_and_a_valid_one_is_not() {
        let styles = style_sheet(&[
            (Para, 0, 0x0FFF, &[BOLD, DELETED].concat()),
            (Para, 0x0FFE, 0, ITALIC),
            (Char, 0x0FFE, 0x0FFF, &[BOLD, &[0x01, 0x08, 1][..]].concat()),
            (Char, 0x0FFE, 2, &[]),
            (Char, 0x0FFE, 0x0FFF, ITALIC),
        ]);
        let (shown, reported) = run_in(&styles, 1, &cistd(4));
        let unreadable = (DiagnosticCode::FormattingUnreadable, Region::Container, 2);
        assert_eq!((shown.as_str(), reported), ("**_a_**\n", vec![unreadable]));
    }

    #[test]
    fn a_heading_rebases_its_paragraph_style_alone_not_a_character_styles_emphasis() {
        let styles = style_sheet(&[(Para, 1, 0x0FFF, BOLD), (Char, 0x0FFE, 0x0FFF, ITALIC)]);
        assert_eq!(run_in(&styles, 0, &cistd(1)), ("# _a_\n".into(), vec![]));
    }

    #[test]
    fn a_piece_modifier_toggle_resolves_against_the_style_its_run_applied() {
        let styles = style_sheet(&[(Para, 0, 0x0FFF, BOLD), (Char, 0x0FFE, 0x0FFF, INVERT_BOLD)]);
        assert_eq!(piece_in(&styles, &cistd(1), INVERT_BOLD), ("**a**\n".into(), vec![]));
        assert_eq!(piece_in(&styles, &[], INVERT_BOLD), ("a\n".into(), vec![]));
    }

    #[test]
    fn a_child_style_layer_applies_after_its_parent() {
        use DiagnosticCode::HiddenOmitted;
        let cases: [(u8, &str, Vec<Reported>); 4] = [
            (0x35, "**a**\n", vec![]),
            (0x36, "_a_\n", vec![]),
            (0x37, "~~a~~\n", vec![]),
            (0x3C, "", omitted(HiddenOmitted, 1)),
        ];
        for (sprm, on, reported) in cases {
            let (set, invert) = ([sprm, 0x08, 1], [sprm, 0x08, 0x81]);
            let styles = style_sheet(&[
                (Para, 0, 0x0FFF, &[]),
                (Char, 0x0FFE, 0x0FFF, &set),
                (Char, 0x0FFE, 1, &invert),
                (Char, 0x0FFE, 0x0FFF, &invert),
                (Char, 0x0FFE, 3, &set),
            ]);
            let off = ("a\n".to_owned(), vec![]);
            assert_eq!(run_in(&styles, 0, &cistd(2)), off, "{sprm:#x} set, then inverted");
            assert_eq!(
                run_in(&styles, 0, &cistd(4)),
                (on.into(), reported),
                "{sprm:#x} inverted, then set"
            );
        }
    }

    fn parsed_under(
        max_work: u64,
        bytes: &[u8],
    ) -> (Result<Document, ConvertError>, Vec<Reported>) {
        let limits = crate::limits::Limits { max_work, ..Default::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let parsed = parse(&parse_ctx, bytes);
        (parsed, diagnostic_rows(&parse_ctx.diagnostics()))
    }

    #[test]
    fn each_run_and_paragraph_charges_the_property_modifiers_it_replays() {
        let doc = |prc: &[u8], chpx: &[u8]| {
            let doc = DocBuilder::new(b"").fib(0x4C, 4).clx(&[prc], &[(4, b"abc\r", false, 1)]);
            doc.chpx_runs(&[(1, chpx), (1, chpx), (2, chpx)]).bytes()
        };
        let plain = fuel(&doc(&[], &[]));
        assert_eq!(fuel(&doc(&[], BOLD)), plain + 3 * 3, "three runs");
        let modifier = BOLD.repeat(1000);
        let replayed = doc(&modifier, &[]);
        let three_runs_and_a_paragraph = 4 * 3000;
        assert_eq!(fuel(&replayed), plain + three_runs_and_a_paragraph);
        let (parsed, _) = parsed_under(plain + 3 * 3000, &replayed);
        assert_matches!(parsed, Err(ConvertError::ResourceLimit { limit: Limit::Work, .. }));
    }

    #[test]
    fn a_style_sheet_past_the_work_left_stops_the_parse_before_a_later_part_is_read() {
        let chpx = BOLD.repeat(80);
        let mut rows = vec![(Char, 0x0FFE, 0x0FFF, &chpx[..]); 200];
        rows[0] = (Para, 0, 0x0FFF, &[]);
        let styles = style_sheet(&rows);
        let comment = [words(&[0, 1]), vec![0; 30]].concat();
        let doc = |styles: &[u8]| {
            DocBuilder::new(b"a\r").table(0xA2, styles).table(0xBA, &comment).bytes()
        };
        let unstyled = fuel(&doc(&[]));
        let commented = vec![(DiagnosticCode::CommentOmitted, Region::Comment, 1)];
        assert_eq!(parsed_under(u64::MAX, &doc(&styles)).1, commented);
        for work_left in [chpx.len() as u64 * 100, styles.len() as u64] {
            let (parsed, reported) = parsed_under(unstyled + work_left, &doc(&styles));
            assert_matches!(parsed, Err(ConvertError::ResourceLimit { limit: Limit::Work, .. }));
            assert_eq!(reported, [], "{work_left}");
        }
    }

    fn omitted(code: DiagnosticCode, count: u64) -> Vec<Reported> {
        vec![(code, Region::Body, count)]
    }

    #[test]
    fn deleted_text_drops_while_inserted_and_struck_text_stays() {
        let doc = runs_doc(&[
            ("old", DELETED),
            ("also old", &[0x00, 0x08, 0x81]),
            ("new", &[0x01, 0x08, 1]),
            ("discard", &[0x01, 0x08, 1, 0x00, 0x08, 1]),
            (" ", &[]),
            ("strike", &[0x37, 0x08, 1]),
            ("\r", &[]),
        ]);
        let expected = ("new ~~strike~~\n".into(), omitted(DiagnosticCode::RevisionOmitted, 2));
        assert_eq!(converted(&doc.bytes()), expected);
    }

    #[test]
    fn a_deletion_toggle_resolves_against_a_style_that_never_deletes() {
        let doc = runs_doc(&[
            ("off", &[0x00, 0x08, 0]),
            ("same", &[0x00, 0x08, 0x80]),
            ("inverse", &[0x00, 0x08, 0x81]),
            ("on", DELETED),
            ("default", &[]),
            ("\r", &[0x00, 0x08, 0]),
        ]);
        let expected = ("offsamedefault\n".into(), omitted(DiagnosticCode::RevisionOmitted, 1));
        assert_eq!(converted(&doc.bytes()), expected);
    }

    #[test]
    fn a_piece_modifier_overrides_its_character_run() {
        let deleting = (0x41 << 1) | (1 << 8);
        let pieces: [(u32, &[u8], bool, u16); 4] = [
            (3, b"old", false, deleting),
            (7, b"keep", false, 1),
            (14, b"discard", false, 3),
            (15, b"\r", false, 0),
        ];
        let doc = DocBuilder::new(b"")
            .fib(0x4C, 15)
            .clx(&[&[0x00, 0x08, 0], DELETED], &pieces)
            .chpx_runs(&[(3, &[]), (4, DELETED), (8, &[])]);
        let expected = ("keep\n".into(), omitted(DiagnosticCode::RevisionOmitted, 2));
        assert_eq!(converted(&doc.bytes()), expected);
    }

    #[test]
    fn a_piece_toggle_resolves_against_the_style_not_the_run() {
        let pieces: [(u32, &[u8], bool, u16); 3] = [
            (4, b"keep", false, (0x41 << 1) | (0x80 << 8)),
            (7, b"old", false, 1),
            (8, b"\r", false, 0),
        ];
        let doc = DocBuilder::new(b"")
            .fib(0x4C, 8)
            .clx(&[&[0x00, 0x08, 0x81]], &pieces)
            .chpx_runs(&[(7, DELETED), (1, &[])]);
        let expected = ("keep\n".into(), omitted(DiagnosticCode::RevisionOmitted, 1));
        assert_eq!(converted(&doc.bytes()), expected);
    }

    #[test]
    fn a_deleted_mark_joins_across_astral_characters() {
        let runs = ["当前😀", "旧内容𠀀\r", "正文𠀀\r"]
            .map(|s| utf16(&s.encode_utf16().collect::<Vec<_>>()));
        let text = runs.concat();
        let units = u32::try_from(text.len() / 2).unwrap();
        let [kept, deleted, next] = runs.map(|run| u32::try_from(run.len()).unwrap());
        let doc = DocBuilder::new(b"")
            .fib(0x4C, units)
            .pieces(&[(units, &text, true, 0)])
            .chpx_runs(&[(kept, &[]), (deleted, DELETED), (next, &[])]);
        let expected = ("当前😀正文𠀀\n".into(), omitted(DiagnosticCode::RevisionOmitted, 1));
        assert_eq!(converted(&doc.bytes()), expected);
    }

    #[test]
    fn deleted_field_delimiters_keep_fields_balanced() {
        let cases: [(TextRuns<'_>, &str); 3] = [
            (
                &[("before ", &[]), ("\x13 DATE \x14obsolete\x15", DELETED), ("after\r", &[])],
                "before after\n",
            ),
            (&[("\x13 DATE \x14", &[]), ("old\x15", DELETED), ("after\r", &[])], "after\n"),
            (
                &[
                    ("\x13 IF 1 = 1 \x14kept ", &[]),
                    ("\x13 DATE \x14old\x15", DELETED),
                    ("result\x15 after\r", &[]),
                ],
                "kept result after\n",
            ),
        ];
        for (runs, shown) in cases {
            let expected = (shown.to_string(), omitted(DiagnosticCode::RevisionOmitted, 1));
            assert_eq!(converted(&runs_doc(runs).bytes()), expected, "{shown}");
        }
    }

    #[test]
    fn partial_and_nested_deletions_inside_a_link_keep_the_link() {
        let doc = runs_doc(&[
            ("\x13 HYPERLINK \"https://example.com\" \x14current ", &[]),
            ("old ", DELETED),
            ("\x13 DATE \x14obsolete\x15", DELETED),
            ("label\x15\r", &[]),
        ]);
        let shown = "[current label](https://example.com)\n";
        let expected = (shown.into(), omitted(DiagnosticCode::RevisionOmitted, 1));
        assert_eq!(converted(&doc.bytes()), expected);
    }

    #[test]
    fn a_deleted_cell_keeps_its_place_in_the_row() {
        let doc = runs_doc(&[("left\x07", &[]), ("old\x07", DELETED), ("right\x07", &[])]);
        let shown = "|  |  |  |\n| --- | --- | --- |\n| left |  | right |\n";
        let expected = (shown.into(), omitted(DiagnosticCode::RevisionOmitted, 1));
        assert_eq!(converted(&doc.bytes()), expected);
    }

    #[test]
    fn deleted_cells_and_row_marks_keep_the_grid() {
        let doc = runs_doc(&[
            ("A\x07", &[]),
            ("B\x07", &[]),
            ("\x07", &[]),
            ("old\x07", DELETED),
            ("C\x07", &[]),
            ("\x07", DELETED),
        ])
        .papx(&[
            (2, IN_TABLE),
            (2, IN_TABLE),
            (1, ROW_END),
            (4, IN_TABLE),
            (2, IN_TABLE),
            (1, ROW_END),
        ]);
        let shown = "| A | B |\n| --- | --- |\n|  | C |\n";
        let expected = (shown.into(), omitted(DiagnosticCode::RevisionOmitted, 1));
        assert_eq!(converted(&doc.bytes()), expected);
    }

    #[test]
    fn a_deleted_nested_cell_mark_still_ends_its_paragraph() {
        let inner: &[u8] = &[0x16, 0x24, 1, 0x4B, 0x24, 1];
        let doc = runs_doc(&[("first", &[]), ("\r", DELETED), ("second\x07", &[])]).papx(&[
            (5, IN_TABLE),
            (1, inner),
            (7, IN_TABLE),
        ]);
        let shown = "|  |\n| --- |\n| first<br>second |\n";
        assert_eq!(converted(&doc.bytes()), (shown.into(), vec![]));
    }

    #[test]
    fn a_note_whose_reference_drops_is_not_parsed() {
        use DiagnosticCode::{HiddenOmitted, ImageUnavailable, RevisionOmitted};
        for (mark, dropped) in
            [(&[][..], None), (DELETED, Some(RevisionOmitted)), (HIDDEN, Some(HiddenOmitted))]
        {
            let doc = runs_doc(&[("\x02", mark), ("text\r", &[]), ("n\x01\r", PIC_LOCATION_CHPX)])
                .fib(0x4C, 6)
                .footnote(3);
            let (parsed, reported) = parsed_with(&doc.bytes());
            let expected = match dropped {
                None => {
                    ("[^1]text\n\n[^1]: n\\[Image\\]\n", vec![(ImageUnavailable, Region::Note, 1)])
                }
                Some(code) => ("text\n", omitted(code, 1)),
            };
            let shown = crate::render::markdown::document_to_markdown(&parsed);
            assert_eq!(
                (parsed.notes.len(), shown.as_str(), reported),
                (usize::from(dropped.is_none()), expected.0, expected.1)
            );
        }
    }

    #[test]
    fn vanish_hides_text_directly_and_through_its_style() {
        let styles = stsh(&[(0, 0x0FFF, &[]), (0x0FFE, 0, HIDDEN)]);
        let cases: [(&str, u16, &[u8], &str); 12] = [
            ("on", 0, HIDDEN, "ac"),
            ("off in a hidden style", 1, SHOWN, "abc"),
            ("0x81 over a hidden style", 1, &[0x3C, 0x08, 0x81], "abc"),
            ("0x80 over a hidden style", 1, &[0x3C, 0x08, 0x80], "ac"),
            ("0x81 over a shown style", 0, &[0x3C, 0x08, 0x81], "ac"),
            ("0x81 twice over a shown style", 0, &[0x3C, 0x08, 0x81, 0x3C, 0x08, 0x81], "ac"),
            ("on then off", 0, &[0x3C, 0x08, 1, 0x3C, 0x08, 0], "abc"),
            ("off then on", 0, &[0x3C, 0x08, 0, 0x3C, 0x08, 1], "ac"),
            ("no toggle value over a hidden style", 1, &[0x3C, 0x08, 0x02], "ac"),
            ("no toggle value over a shown style", 0, &[0x3C, 0x08, 0x7F], "abc"),
            ("an operand cut short", 0, &[0x3C, 0x08], "abc"),
            ("no run formatting in a hidden style", 1, &[], "ac"),
        ];
        for (case, istd, grpprl, shown) in cases {
            let doc = runs_doc(&[("a", SHOWN), ("b", grpprl), ("c\r", SHOWN)])
                .table(0xA2, &styles)
                .styled_papx(&[(4, istd, &[])]);
            let hidden =
                if shown == "ac" { omitted(DiagnosticCode::HiddenOmitted, 1) } else { vec![] };
            assert_eq!(converted(&doc.bytes()), (format!("{shown}\n"), hidden), "{case}");
        }
    }

    #[test]
    fn deleted_wins_over_hidden() {
        use DiagnosticCode::{HiddenOmitted, RevisionOmitted};
        let doc = runs_doc(&[
            ("a", &[]),
            ("b", &[0x3C, 0x08, 1, 0x00, 0x08, 1]),
            ("c", HIDDEN),
            ("d\r", &[]),
        ]);
        let expected = vec![(RevisionOmitted, Region::Body, 1), (HiddenOmitted, Region::Body, 1)];
        assert_eq!(converted(&doc.bytes()), ("ad\n".into(), expected));
    }

    #[test]
    fn character_properties_follow_each_run_paragraph_and_piece_boundary() {
        let styles = stsh(&[(0, 0x0FFF, &[]), (0x0FFE, 0, HIDDEN)]);
        let bold = (0x55 << 1) | (1 << 8);
        let inverting: &[u8] = &[0x3C, 0x08, 0x81];
        for ((chpx, papx, piece), shown) in [
            ((2, 4, 5), "abe**f**"),
            ((4, 2, 5), "abe**f**"),
            ((1, 3, 2), "a**def**"),
            ((5, 1, 3), "a**f**"),
        ] {
            let text = b"abcdef\r";
            let (before, after) = text.split_at(piece);
            let doc = DocBuilder::new(b"")
                .fib(0x4C, 7)
                .table(0xA2, &styles)
                .pieces(&[
                    (u32::try_from(piece).unwrap(), before, false, 0),
                    (7, after, false, bold),
                ])
                .chpx_runs(&[(chpx, &[]), (7 - chpx, inverting)])
                .styled_papx(&[(papx, 0, &[]), (7 - papx, 1, &[])]);
            let expected = (format!("{shown}\n"), omitted(DiagnosticCode::HiddenOmitted, 1));
            assert_eq!(converted(&doc.bytes()), expected, "{chpx} {papx} {piece}");
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the table of dropped ranges and what their paragraphs show"
    )]
    fn a_dropped_range_counts_once_per_paragraph_when_it_holds_shown_content() {
        use DiagnosticCode::HiddenOmitted;
        let hidden = |count| omitted(HiddenOmitted, count);
        let picture = [HIDDEN, PIC_LOCATION_CHPX].concat();
        let object = [HIDDEN, &[0x0A, 0x08, 1], PIC_LOCATION_CHPX].concat();
        let undecodable = utf16(&[0x61, 0xDC00, 0x0D]);
        let cases: Vec<(&str, DocBuilder, &str, Vec<Reported>)> = vec![
            (
                "whitespace",
                runs_doc(&[("a", &[]), (" \t\x0b", HIDDEN), ("b\r", &[])]),
                "ab\n",
                vec![],
            ),
            ("a mark alone", runs_doc(&[("a", &[]), ("\r", HIDDEN), ("b\r", &[])]), "ab\n", vec![]),
            (
                "a range a joined mark ends",
                runs_doc(&[("a", &[]), ("b\rc", HIDDEN), ("d\r", &[])]),
                "ad\n",
                hidden(2),
            ),
            (
                "ranges a shown field delimiter parts",
                runs_doc(&[
                    ("a\x13 PAGE \x14", &[]),
                    ("x", HIDDEN),
                    ("\x15", &[]),
                    ("y", HIDDEN),
                    ("b\r", &[]),
                ]),
                "ab\n",
                hidden(2),
            ),
            (
                "a range across a hidden field delimiter",
                runs_doc(&[("a\x13 PAGE \x14", &[]), ("x\x15y", HIDDEN), ("b\r", &[])]),
                "ab\n",
                hidden(1),
            ),
            (
                "a field with no result",
                runs_doc(&[("a", &[]), ("\x13 PAGE \x14\x15", HIDDEN), ("b\r", &[])]),
                "ab\n",
                vec![],
            ),
            (
                "a field nested in an instruction",
                runs_doc(&[
                    ("a", &[]),
                    ("\x13 IF \x13 PAGE \x14x\x15 \x14\x15", HIDDEN),
                    ("b\r", &[]),
                ]),
                "ab\n",
                vec![],
            ),
            (
                "a link inside a field result",
                runs_doc(&[
                    ("a\x13 IF 1 = 1 \x14\x13 HYPERLINK \"http://x/\" \x14", &[]),
                    ("label", HIDDEN),
                    ("\x15 kept\x15\r", &[]),
                ]),
                "a kept\n",
                hidden(1),
            ),
            (
                "a picture with no Data stream",
                runs_doc(&[("a", &[]), ("\x01", &picture), ("\r", &[])]),
                "a\n",
                hidden(1),
            ),
            (
                "an embedded object",
                runs_doc(&[("a", &[]), ("\x01", &object), ("\r", &[])]),
                "a\n",
                hidden(1),
            ),
            (
                "a floating picture",
                anchoring(
                    runs_doc(&[("\x08", HIDDEN), ("a\r", &[])]),
                    0,
                    7,
                    &[(0, &shape(7, &[0x4104]))],
                ),
                "a\n",
                hidden(1),
            ),
            (
                "an undecodable character",
                DocBuilder::new(b"")
                    .fib(0x4C, 3)
                    .pieces(&[(3, &undecodable, true, 0)])
                    .chpx_runs(&[(2, &[]), (2, HIDDEN), (2, &[])]),
                "a\n",
                hidden(1),
            ),
            (
                "a check box",
                runs_doc(&[("a", &[]), ("\x13 FORMCHECKBOX \x14\x15", HIDDEN), ("\r", &[])]),
                "a\n",
                vec![],
            ),
            (
                "a footnote",
                runs_doc(&[("\x02a\r", &[]), ("b", HIDDEN), ("c\r", &[])]).fib(0x4C, 3).footnote(3),
                "[^1]a\n\n[^1]: c\n",
                vec![(HiddenOmitted, Region::Note, 1)],
            ),
            (
                "whitespace and an optional hyphen",
                runs_doc(&[("a", &[]), (" \x1f ", HIDDEN), ("b\r", &[])]),
                "ab\n",
                vec![],
            ),
            (
                "a non-breaking hyphen",
                runs_doc(&[("a", &[]), ("\x1e", HIDDEN), ("b\r", &[])]),
                "ab\n",
                hidden(1),
            ),
            (
                "whitespace after a joined mark",
                runs_doc(&[("a", &[]), ("b\r ", HIDDEN), ("c\r", &[])]),
                "ac\n",
                hidden(1),
            ),
            ("a range the story ends", runs_doc(&[("a\r", &[]), ("b", HIDDEN)]), "a\n", hidden(1)),
        ];
        for (case, doc, shown, expected) in cases {
            assert_eq!(converted(&doc.bytes()), (shown.to_string(), expected), "{case}");
        }
    }

    #[test]
    fn properties_resolve_once_per_segment_not_per_character() {
        let work = |runs: TextRuns<'_>| {
            let text: usize = runs.iter().map(|(text, _)| text.len()).sum();
            fuel(&runs_doc(runs).bytes()) - u64::try_from(text).unwrap()
        };
        let one = work(&[("a", HIDDEN), ("\r", &[])]);
        assert_eq!(work(&[("abcdefgh", HIDDEN), ("\r", &[])]), one);
        let three = work(&[("a", HIDDEN), ("b", SHOWN), ("c", HIDDEN), ("\r", &[])]);
        let chpx = 3;
        assert_eq!(three, one + 2 + 2 * (1 + chpx), "two more FKP runs, segments and CHPXs");
    }

    #[test]
    fn a_dropped_mark_joins_the_next_paragraph_and_takes_its_properties() {
        let styles = stsh(&[(0, 0x0FFF, &[]), (1, 0, &[])]);
        let joined = |first, second| {
            let doc = runs_doc(&[("a", &[]), ("\r", HIDDEN), ("b\r", &[])])
                .table(0xA2, &styles)
                .styled_papx(&[(2, first, &[]), (2, second, &[])]);
            converted(&doc.bytes())
        };
        assert_eq!(joined(0, 1), ("# ab\n".into(), vec![]));
        assert_eq!(joined(1, 0), ("ab\n".into(), vec![]));
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of dropped paragraph marks by container")]
    fn a_dropped_mark_joins_only_within_its_container() {
        let styles = stsh(&[(0, 0x0FFF, &[]), (1, 0, &[])]);
        let nested: &[u8] = &[0x16, 0x24, 1, 0x49, 0x66, 2, 0, 0, 0, 0x4B, 0x24, 1];
        let nested_row: &[u8] = &[0x16, 0x24, 1, 0x49, 0x66, 2, 0, 0, 0, 0x4C, 0x24, 1];
        let cases: Vec<(&str, DocBuilder, &str)> = vec![
            (
                "before a table of cell marks alone",
                runs_doc(&[("a", &[]), ("\r", HIDDEN), ("b\x07", &[])]),
                "a\n\n|  |\n| --- |\n| b |\n",
            ),
            (
                "before a table",
                runs_doc(&[("a", &[]), ("\r", HIDDEN), ("b\x07", &[]), ("\x07", &[])]).papx(&[
                    (2, &[]),
                    (2, IN_TABLE),
                    (1, ROW_END),
                ]),
                "a\n\n|  |\n| --- |\n| b |\n",
            ),
            (
                "inside a cell",
                runs_doc(&[("a", &[]), ("\r", HIDDEN), ("b\x07", &[]), ("\x07", &[])]).papx(&[
                    (2, IN_TABLE),
                    (2, IN_TABLE),
                    (1, ROW_END),
                ]),
                "|  |\n| --- |\n| ab |\n",
            ),
            (
                "out of a table",
                runs_doc(&[("a", &[]), ("\r", HIDDEN), ("b\r", &[])])
                    .papx(&[(2, IN_TABLE), (2, &[])]),
                "|  |\n| --- |\n| a |\n\nb\n",
            ),
            (
                "into a nested table",
                runs_doc(&[
                    ("a", &[]),
                    ("\r", HIDDEN),
                    ("b\r", &[]),
                    ("\r", &[]),
                    ("\x07", &[]),
                    ("\x07", &[]),
                ])
                .papx(&[
                    (2, IN_TABLE),
                    (2, nested),
                    (1, nested_row),
                    (1, IN_TABLE),
                    (1, ROW_END),
                ]),
                "|  |\n| --- |\n| a<br>b |\n",
            ),
            (
                "before a row mark",
                runs_doc(&[("a\x07", &[]), ("b", &[]), ("\r", HIDDEN), ("\x07", &[])]).papx(&[
                    (2, IN_TABLE),
                    (2, IN_TABLE),
                    (1, ROW_END),
                ]),
                "| a |\n| --- |\n| b |\n",
            ),
            (
                "at a row end held over text",
                runs_doc(&[("x\x07", &[]), ("a", &[]), ("\r", HIDDEN), ("b\x07", &[])]).papx(&[
                    (2, IN_TABLE),
                    (2, ROW_END),
                    (2, ROW_END),
                ]),
                "| x |\n| --- |\n| a |\n",
            ),
            (
                "at a nested row end",
                runs_doc(&[("a", &[]), ("\r", HIDDEN), ("b\r", &[]), ("\x07", &[]), ("\x07", &[])])
                    .papx(&[(2, nested_row), (2, nested), (1, IN_TABLE), (1, ROW_END)]),
                "|  |\n| --- |\n| a<br>b |\n",
            ),
            (
                "before a page break",
                runs_doc(&[("a", &[]), ("\r", HIDDEN), ("b\x0c", &[]), ("c\r", &[])]),
                "ab\n\nc\n",
            ),
            (
                "at a page break",
                runs_doc(&[("a", &[]), ("\x0c", HIDDEN), ("b\r", &[])]),
                "a\n\nb\n",
            ),
            (
                "at a column break",
                runs_doc(&[("a", &[]), ("\x0e", HIDDEN), ("b\r", &[])]),
                "a\n\nb\n",
            ),
            (
                "at the end of the main text",
                runs_doc(&[("\x02a\rb", &[]), ("\r", HIDDEN), ("c\r", &[])])
                    .fib(0x4C, 5)
                    .footnote(2)
                    .table(0xA2, &styles)
                    .styled_papx(&[(3, 0, &[]), (2, 1, &[]), (2, 0, &[])]),
                "[^1]a\n\n# b\n\n[^1]: c\n",
            ),
        ];
        for (case, doc, shown) in cases {
            assert_eq!(converted(&doc.bytes()), (shown.to_string(), vec![]), "{case}");
        }
    }

    #[test]
    fn a_check_box_whose_end_drops_reports_nothing_unread() {
        use DiagnosticCode::FormControlUnread;
        for (end, expected) in
            [(&[][..], vec![(FormControlUnread, Region::Body, 1)]), (HIDDEN, vec![])]
        {
            let doc = runs_doc(&[("a\x13 FORMCHECKBOX \x14", &[]), ("\x15", end), ("\r", &[])]);
            assert_eq!(converted(&doc.bytes()), ("a\n".into(), expected));
        }
    }

    #[test]
    fn an_instruction_deep_in_field_results_counts_nothing_it_hides() {
        let open = format!("{}\x13 PAGE ", "\x13 IF 1 = 1 \x14".repeat(255));
        let hidden = "x".repeat(10_000);
        let close = format!("{}\r", "\x15".repeat(256));
        let doc = runs_doc(&[
            (&open, &[]),
            (&hidden, HIDDEN),
            ("\x14", &[]),
            ("y", HIDDEN),
            (&close, &[]),
        ]);
        assert_eq!(
            converted(&doc.bytes()),
            (String::new(), omitted(DiagnosticCode::HiddenOmitted, 1))
        );
    }

    #[test]
    fn text_after_a_closed_instruction_counts_what_it_hides() {
        for field in ["\x13 PAGE \x15", "\x13 IF \x14r\x14s\x15", "\x13 A \x13 B \x14\x14\x15\x15"]
        {
            let doc = runs_doc(&[(field, &[]), ("x", HIDDEN), ("\r", &[])]);
            let hidden = omitted(DiagnosticCode::HiddenOmitted, 1);
            assert_eq!(converted(&doc.bytes()).1, hidden, "{field:?}");
        }
    }

    #[test]
    fn toggles_resolve_against_the_whole_style_chain() {
        let inverted: &[u8] = &[0x3C, 0x08, 0x81];
        let styles = stsh(&[(0, 0x0FFF, &[]), (0x0FFE, 0, HIDDEN), (0x0FFE, 1, inverted)]);
        for (grpprl, shown) in [(&[][..], "abc"), (inverted, "ac")] {
            let doc = runs_doc(&[("a", &[]), ("b", grpprl), ("c\r", &[])])
                .table(0xA2, &styles)
                .styled_papx(&[(4, 2, &[])]);
            let dropped =
                if shown == "ac" { omitted(DiagnosticCode::HiddenOmitted, 1) } else { vec![] };
            assert_eq!(converted(&doc.bytes()), (format!("{shown}\n"), dropped));
        }
    }

    #[test]
    fn a_piece_modifier_hides_its_text() {
        let pieces: [(u32, &[u8], bool, u16); 3] = [
            (4, b"keep", false, 1),
            (7, b"old", false, (0x5C << 1) | (1 << 8)),
            (8, b"\r", false, 0),
        ];
        let doc = DocBuilder::new(b"").fib(0x4C, 8).pieces(&pieces).chpx_runs(&[(8, &[])]);
        let expected = ("keep\n".into(), omitted(DiagnosticCode::HiddenOmitted, 1));
        assert_eq!(converted(&doc.bytes()), expected);
    }

    #[test]
    fn compressed_text_reads_the_specified_characters_under_every_language_id() {
        let text = b"\x93x\x94\xE0\r";
        for (language, doc) in [
            ("Western", DocBuilder::new(text).lid(0x0409)),
            ("Russian", DocBuilder::new(text).lid(0x0419)),
            ("Greek", DocBuilder::new(text).lid(0x0408)),
            ("Japanese", DocBuilder::new(text).far_east(0x0411)),
            ("Korean", DocBuilder::new(text).far_east(0x0412)),
            ("Simplified Chinese", DocBuilder::new(text).far_east(0x0804)),
        ] {
            assert_eq!(
                converted(&doc.bytes()),
                ("\u{201C}x\u{201D}\u{E0}\n".into(), vec![]),
                "{language}"
            );
        }
    }

    #[test]
    fn a_far_east_document_keeps_each_compressed_byte_a_position_of_its_own() {
        let bold: &[u8] = &[0x35, 0x08, 0x01];
        let plain: &[u8] = &[];
        for (text, bold_at, shown) in [
            (&b"\x81\x82\r"[..], 1, "**\u{201A}**\n"),
            (b"\x81\x82\r", 0, "\u{201A}\n"),
            (b"\x83\x40\r", 1, "&#402;**@**\n"),
            (b"\x83\x40\r", 0, "**\u{192}**@\n"),
        ] {
            let runs: Vec<(u32, &[u8])> =
                (0..3).map(|cp| (1, if cp == bold_at { bold } else { plain })).collect();
            let doc = DocBuilder::new(text).far_east(0x0411).chpx_runs(&runs);
            assert_eq!(converted(&doc.bytes()), (shown.into(), vec![]), "{text:x?} {bold_at}");
        }
    }

    #[test]
    fn compressed_bytes_the_specification_leaves_unmapped_are_controls_the_text_omits() {
        for unmapped in [0x80u8, 0x81, 0x8D, 0x8E, 0x8F, 0x90, 0x9D, 0x9E] {
            let doc = DocBuilder::new(&[b'a', unmapped, b'b', b'\r']);
            assert_eq!(converted(&doc.bytes()), ("ab\n".into(), vec![]), "{unmapped:#x}");
        }
    }

    #[test]
    fn the_builder_writes_the_fib_counts_of_word_97_and_its_stream_length() {
        let doc = DocBuilder::new(b"ab\r").far_east(0x0411).bytes();
        let mut ole = cfb::CompoundFile::open(Cursor::new(doc.as_slice())).unwrap();
        let word_doc = ole_stream(&mut ole, "WordDocument");
        let fields = [0x00, 0x02, 0x0A, 0x0C, 0x20, 0x3C, 0x3E, 0x98, FIB_MIN];
        assert_eq!(
            fields.map(|off| get_u16(&word_doc, off)),
            [0xA5EC, 0x00C1, 0x5000, 0x00BF, 0x000E, 0x0411, 0x0016, 0x005D, 0].map(Some)
        );
        assert_eq!(get_u32(&word_doc, 0x40), u32::try_from(word_doc.len()).ok());
        let table = ole_stream(&mut ole, "0Table");
        let fib = Fib::read(&word_doc).unwrap();
        let clx = (fib.size(Fib::FC_CLX), fib.size(Fib::LCB_CLX));
        let coverage = Coverage { terminal_cp: 3, cb_mac: word_doc.len() };
        let pieces =
            PieceTable::parse(&ParseCtx::default(), &table, clx.0, clx.1, coverage).unwrap();
        let pieces: Vec<_> = (0..pieces.len())
            .map(|i| pieces.pcd(i))
            .map(|pcd| (pcd.cp_range(), pcd.fc, pcd.compressed))
            .collect();
        assert_eq!(pieces, [(0..3, TEXT_FC, true)]);
    }

    #[test]
    fn a_word_97_document_without_a_piece_table_is_malformed() {
        let doc = DocBuilder { clx: false, ..DocBuilder::new(b"a\r") }.bytes();
        assert_eq!(
            parse(&ParseCtx::default(), &doc).unwrap_err(),
            ConvertError::malformed_part("WordDocument", "no piece table")
        );
    }

    #[test]
    fn code_style_pictures_survive_real_doc_parsing_and_cells() {
        use crate::{Config, Converter, Format, ImagePolicy, Input, OnLoss};
        let styles = code_styles();
        let text = b"before\x01after\r";
        for asset in [false, true] {
            for cell in [false, true] {
                let mut doc = DocBuilder::new(text)
                    .table(0xA2, &styles)
                    .styled_papx(&[(text.len() as u32, 0, if cell { ROW_END } else { &[] })])
                    .chpx(PIC_LOCATION_CHPX);
                if asset {
                    doc = doc.data(picture_data(1));
                }
                let bytes = doc.bytes();
                for images in [ImagePolicy::Placeholder, ImagePolicy::AltText, ImagePolicy::Omit] {
                    let config = Config { images, on_loss: OnLoss::Report, ..Config::default() };
                    let converter = Converter::new(config).unwrap();
                    let parsed = converter.parse(Input::new(&bytes, Format::Doc)).unwrap();
                    let result = parsed.render().unwrap();
                    assert_eq!(result, converter.convert(Input::new(&bytes, Format::Doc)).unwrap());
                    let count: u64 = result
                        .diagnostics()
                        .iter()
                        .filter(|d| d.code == DiagnosticCode::ImageUnavailable)
                        .map(|d| d.count)
                        .sum();
                    assert_eq!(count, u64::from(!asset && images == ImagePolicy::Placeholder));
                    assert_eq!(result.markdown().contains("Image"), images != ImagePolicy::Omit);
                    assert_eq!(
                        result.markdown().contains("asset:0"),
                        asset && images == ImagePolicy::Placeholder
                    );
                    assert!(
                        result.markdown().contains("before") && result.markdown().contains("after")
                    );
                    let strict = Converter::new(Config { images, ..Config::default() }).unwrap();
                    assert_eq!(
                        strict.convert(Input::new(&bytes, Format::Doc)).is_err(),
                        !asset && images == ImagePolicy::Placeholder
                    );
                }
            }
        }
    }

    /// A style sheet whose paragraph style 0 is a code block.
    fn code_styles() -> Vec<u8> {
        let mut record = [0x0FFEu16, 0xFFF1, 2, 0, 0].map(u16::to_le_bytes).concat();
        let name: Vec<u16> = "Source Code".encode_utf16().collect();
        record.extend(u16::try_from(name.len()).unwrap().to_le_bytes());
        record.extend(utf16(&name));
        record.extend(0u16.to_le_bytes());
        record.extend([2u16, 0, 0].map(u16::to_le_bytes).concat());
        let mut styles = [4u16, 1, 10].map(u16::to_le_bytes).concat();
        styles.extend(u16::try_from(record.len()).unwrap().to_le_bytes());
        styles.extend(record);
        styles
    }

    #[test]
    fn damaged_anchor_id_is_silent() {
        use DiagnosticCode::{DanglingReference, InvalidEncoding};
        let anchor = damaged("\x13 HYPERLINK \\l \"a~\" \x14a\x15\r");
        assert_eq!(reported(&anchor), [(DanglingReference, Region::Body, 1)]);
        let fragment = damaged("\x13 HYPERLINK \"http://x/\" \\l \"a~\" \x14a\x15\r");
        assert_eq!(reported(&fragment), [(InvalidEncoding, Region::Body, 1)]);
    }

    #[test]
    fn code_styled_damaged_safe_target_reports_once() {
        let doc = pieced(&[Some("\x13 HYPERLINK \"http://x/~\" \x14a\x15\r")])
            .table(0xA2, &code_styles());
        let (shown, reported) = converted(&doc.bytes());
        assert!(shown.contains("](http://x/"), "{shown}");
        assert_eq!(reported, [(DiagnosticCode::InvalidEncoding, Region::Body, 1)]);
    }

    #[test]
    fn code_styled_damaged_unsafe_target_is_silent() {
        let doc = pieced(&[Some("\x13 HYPERLINK \"javascript:x~\" \x14a\x15\r")])
            .table(0xA2, &code_styles());
        let (shown, reported) = converted(&doc.bytes());
        assert!(!shown.contains("javascript"), "{shown}");
        assert_eq!(reported, [(DiagnosticCode::UnsafeLink, Region::Body, 1)]);
    }

    #[test]
    fn a_tooltip_and_a_literal_replacement_character_in_a_target_are_silent() {
        let doc = damaged("\x13 HYPERLINK \"http://x/\u{FFFD}\" \\o \"~\" \x14a\x15\r");
        assert_eq!(reported(&doc), []);
    }

    #[test]
    fn target_damage_counts_each_piece_whose_text_lies_inside_the_target() {
        use DiagnosticCode::InvalidEncoding;
        let cases: [(&str, &[Option<&str>], u64); 5] = [
            ("one piece", &[Some("\x13 HYPERLINK \"http://x/~~\" \x14a\x15\r")], 1),
            (
                "the target over two pieces",
                &[Some("\x13 HYPERLINK \"http://x/~"), Some("~\" \x14a\x15\r")],
                2,
            ),
            (
                "the target and a switch in two pieces",
                &[Some("\x13 HYPERLINK \"http://x/\" \\o \"~"), Some("~\" \\l \"~\" \x14a\x15\r")],
                1,
            ),
            (
                "a nested field's text in the instruction",
                &[Some("\x13 HYPERLINK \"http://x/\x13 PAGE \x14~\x15y\" \x14a\x15\r")],
                0,
            ),
            (
                "a nested field's damage in another piece before the target's",
                &[Some("\x13 HYPERLINK \"http://x/\x13\x14~\x15"), Some("a~bc\" \x14a\x15\r")],
                1,
            ),
        ];
        for (case, parts, count) in cases {
            let expected =
                if count == 0 { vec![] } else { vec![(InvalidEncoding, Region::Body, count)] };
            assert_eq!(reported(&pieced(parts).bytes()), expected, "{case}");
        }
    }

    #[test]
    fn gap_unwrapped_link_keeps_label_damage_only() {
        use DiagnosticCode::{InvalidEncoding, TruncatedRecords};
        let target = Some("\x13 HYPERLINK \"http://x/~\" \x14");
        let (whole, reported) =
            converted(&pieced(&[target, Some("l~bel"), Some("\x15\r")]).bytes());
        assert!(whole.starts_with("[l\u{FFFD}bel]("), "{whole}");
        assert_eq!(reported, [(InvalidEncoding, Region::Body, 2)]);
        let (cut, reported) =
            converted(&pieced(&[target, Some("l~bel"), None, Some("\x15\r")]).bytes());
        assert_eq!(cut, "l\u{FFFD}bel\n");
        assert_eq!(
            reported,
            [(TruncatedRecords, Region::Body, 1), (InvalidEncoding, Region::Body, 1)]
        );
    }

    #[test]
    fn gap_after_completed_inner_link_matches_its_target_damage() {
        use DiagnosticCode::{InvalidEncoding, TruncatedRecords};
        let inner = Some("\x13 IF 1 = 1 \x14\x13 HYPERLINK \"http://x/~\" \x14label\x15");
        let (shown, reported) = converted(&pieced(&[inner, None, Some(" after\x15\r")]).bytes());
        let wrapped = shown.starts_with("[label](http://x/");
        assert!(wrapped && shown.ends_with(" after\n"), "{shown}");
        let damage = reported.contains(&(InvalidEncoding, Region::Body, 1));
        assert_eq!(damage, wrapped);
        assert!(reported.contains(&(TruncatedRecords, Region::Body, 1)));
    }

    #[test]
    fn text_after_a_gap_shows_plainly_and_unmatched_field_ends_are_ignored() {
        use DiagnosticCode::TruncatedRecords;
        let cases: [(&str, &[Option<&str>], &str); 5] = [
            (
                "an open instruction",
                &[Some("a\x13 PAGE "), None, Some("visible\x15 b\r")],
                "avisible b\n",
            ),
            (
                "an open result over an open nested instruction",
                &[Some("\x13 IF 1 = 1 \x14kept \x13 PAGE "), None, Some("x\x14y\x15z\x15\r")],
                "kept xyz\n",
            ),
            (
                "a field that begins after the gap",
                &[Some("\x13 IF 1 = 1 \x14a"), None, Some("\x15\x13 PAGE \x141\x15\r")],
                "a1\n",
            ),
            ("a separator alone", &[Some("a"), None, Some("\x14b\x15\r")], "ab\n"),
            (
                "a result inside an open instruction",
                &[Some("a\x13 IF \x13 PAGE \x14r"), None, Some("b\r")],
                "ab\n",
            ),
        ];
        for (case, parts, shown) in cases {
            let expected = (shown.to_owned(), vec![(TruncatedRecords, Region::Body, 1)]);
            assert_eq!(converted(&pieced(parts).bytes()), expected, "{case}");
        }
    }

    #[test]
    fn each_gap_reports_once_per_piece_and_region_it_is_visited_in() {
        use DiagnosticCode::TruncatedRecords;
        let doc = pieced(&[Some("a"), None, Some("b\rc"), None, Some("\r")]);
        assert_eq!(
            converted(&doc.bytes()),
            ("ab\n\nc\n".into(), vec![(TruncatedRecords, Region::Body, 2)])
        );
    }

    #[test]
    fn notes_no_reference_reaches_are_never_read() {
        use DiagnosticCode::{DanglingReference, HiddenOmitted, RevisionOmitted, TruncatedRecords};
        for (mark, expected) in [
            (
                &[][..],
                vec![(TruncatedRecords, Region::Note, 1), (DanglingReference, Region::Body, 1)],
            ),
            (DELETED, vec![(RevisionOmitted, Region::Body, 1)]),
            (HIDDEN, vec![(HiddenOmitted, Region::Body, 1)]),
        ] {
            let doc = DocBuilder::new(b"")
                .fib(0x4C, 3)
                .footnote(2)
                .pieces(&[(3, b"\x02a\r", false, 0), (5, b"", true, 0)])
                .chpx_runs(&[(1, mark), (2, &[])])
                .truncated();
            let mut reported = converted(&doc.bytes()).1;
            reported.sort();
            let mut expected = expected;
            expected.sort();
            assert_eq!(reported, expected, "{mark:?}");
        }
    }

    #[test]
    fn a_piece_two_notes_share_reports_its_damage_once_in_their_region() {
        let refs = [words(&[0, 1, 2]), vec![0; 4]].concat();
        let notes = utf16(&[u16::from(b'x'), 0xDC00, 0x0D, 0xDC00, 0x0D]);
        let doc = DocBuilder::new(b"")
            .fib(0x4C, 3)
            .fib(0x50, 5)
            .table(0xAA, &refs)
            .table(0xB2, &words(&[0, 3, 5]))
            .pieces(&[(3, b"\x02\x02\r", false, 0), (8, &notes, true, 0)]);
        let (shown, reported) = converted(&doc.bytes());
        assert_eq!(shown, "[^1][^2]\n\n[^1]: x\u{FFFD}\n\n[^2]: \u{FFFD}\n");
        assert_eq!(reported, [(DiagnosticCode::InvalidEncoding, Region::Note, 1)]);
    }

    /// A pair and a paragraph mark under the CHPX `pair`, with a footnote's reference on
    /// the pair's trailing unit.
    fn a_reference_inside_a_pair(pair: &[u8]) -> (String, Vec<Reported>) {
        let main = utf16(&[0xD83D, 0xDE00, 0x0D]);
        let doc = DocBuilder::new(b"")
            .fib(0x4C, 3)
            .fib(0x50, 2)
            .table(0xAA, &[words(&[1, 2]), vec![0, 0]].concat())
            .table(0xB2, &words(&[0, 2]))
            .pieces(&[(3, &main, true, 0), (5, b"n\r", false, 0)])
            .chpx_runs(&[(4, pair), (2, &[])]);
        converted(&doc.bytes())
    }

    #[test]
    fn a_reference_inside_a_pair_is_reported_and_leaves_the_text_whole() {
        let (shown, reported) = a_reference_inside_a_pair(&[]);
        assert!(shown.starts_with("\u{1F600}\n"), "{shown}");
        assert!(
            reported.contains(&(DiagnosticCode::PartUnreadable, Region::Note, 1)),
            "{reported:?}"
        );
    }

    #[test]
    fn a_reference_inside_a_dropped_pair_drops_its_note_silently() {
        for (pair, code) in
            [(DELETED, DiagnosticCode::RevisionOmitted), (HIDDEN, DiagnosticCode::HiddenOmitted)]
        {
            assert_eq!(a_reference_inside_a_pair(pair), (String::new(), omitted(code, 1)));
        }
    }

    #[test]
    fn a_reference_inside_a_pair_in_a_field_instruction_is_not_reported() {
        let main: Vec<u16> = "\x13 X \u{1F600}\x14a\x15\r".encode_utf16().collect();
        let doc = DocBuilder::new(b"")
            .fib(0x4C, 10)
            .fib(0x50, 2)
            .table(0xAA, &[words(&[5, 6]), vec![0, 0]].concat())
            .table(0xB2, &words(&[0, 2]))
            .pieces(&[(10, &utf16(&main), true, 0), (12, b"n\r", false, 0)]);
        assert_eq!(converted(&doc.bytes()), ("a\n\n[^1]: n\n".into(), vec![]));
    }

    #[test]
    fn a_target_replay_skips_what_the_instruction_never_held() {
        use DiagnosticCode::InvalidEncoding;
        let url = "\x13 HYPERLINK \"http://x/";
        let deleted = pieced(&[Some(url), Some("ddd"), Some("~\" \x14a\x15\r")]);
        let prefix = 2 * u32::try_from(url.len()).unwrap();
        let deleted = deleted.chpx_runs(&[(prefix, &[]), (2 * 3, DELETED), (2 * 6, &[])]);
        assert_eq!(reported(&deleted.bytes()), [(InvalidEncoding, Region::Body, 1)], "deleted");
        let main: Vec<u16> = format!("{url}\u{20AC}~\" \x14a\x15\r")
            .encode_utf16()
            .map(|unit| if unit == u16::from(b'~') { 0xDC00 } else { unit })
            .collect();
        let at = u32::try_from(url.len()).unwrap();
        let len = u32::try_from(main.len()).unwrap();
        let noted = DocBuilder::new(b"")
            .fib(0x4C, len)
            .fib(0x50, 2)
            .table(0xAA, &[words(&[at, len]), vec![0, 0]].concat())
            .table(0xB2, &words(&[0, 2]))
            .pieces(&[(len, &utf16(&main), true, 0), (len + 2, b"n\r", false, 0)]);
        let reported = reported(&noted.bytes());
        assert!(reported.contains(&(InvalidEncoding, Region::Body, 1)), "{reported:?}");
    }

    #[test]
    fn a_surrogate_pair_split_over_two_pieces_is_one_character_styled_by_its_leading_unit() {
        let doc = DocBuilder::new(b"")
            .fib(0x4C, 5)
            .pieces(&[
                (2, &utf16(&[0x61, 0xD83D]), true, 0),
                (5, &utf16(&[0xDE00, 0x62, 0x0D]), true, 0),
            ])
            .chpx_runs(&[(2, &[]), (2, &[]), (4, BOLD), (2, &[])]);
        assert_eq!(converted(&doc.bytes()), ("a\u{1F600}**b**\n".into(), vec![]));
        let bold_trailing = DocBuilder::new(b"")
            .fib(0x4C, 4)
            .pieces(&[(4, &utf16(&[0x61, 0xD83D, 0xDE00, 0x0D]), true, 0)])
            .chpx_runs(&[(4, &[]), (4, BOLD)]);
        assert_eq!(converted(&bold_trailing.bytes()), ("a\u{1F600}\n".into(), vec![]));
    }

    #[test]
    fn the_stories_place_the_endnotes_after_the_comments_and_reserved3_holds_none() {
        let doc =
            DocBuilder::new(b"\x02\rnn\re\r").fib(0x4C, 2).fib(0x58, 2).fib(0x5C, 3).endnote(2);
        let doc = doc.table(0xBA, &[words(&[0, 1]), vec![0; 30]].concat());
        let (shown, reported) = converted(&doc.bytes());
        assert_eq!(shown, "[^1]\n\n[^1]: e\n");
        assert_eq!(reported, [(DiagnosticCode::CommentOmitted, Region::Comment, 1)]);
        let boxes = || DocBuilder::new(b"a\r").fib(0x68, 3);
        assert_eq!(converted(&boxes().bytes()).0, "a\n");
        let mut short = boxes().covered();
        short.word_doc[0x68] = 2;
        assert_eq!(
            parse(&ParseCtx::default(), &DocBuilder { clx: false, ..short }.bytes()).unwrap_err(),
            ConvertError::malformed("piece table misses the stories")
        );
    }

    #[test]
    fn story_lengths_past_a_signed_count_are_malformed() {
        let mut word_doc = [&[0xEC, 0xA5][..], &[0; FIB_MIN - 2]].concat();
        word_doc[0x50..0x54].copy_from_slice(&0x8000_0000u32.to_le_bytes());
        assert_eq!(
            word_document_alone(&word_doc).unwrap_err(),
            ConvertError::malformed_part("WordDocument", "negative story length")
        );
    }

    #[test]
    fn header_aliases_are_never_read_so_their_text_costs_only_their_pieces() {
        let fuel_of = |aliases: u32| {
            let pieces: Vec<(u32, &[u8], bool, u16)> = [(2, &b"a\r"[..], false, 0)]
                .into_iter()
                .chain((1..=aliases).map(|i| (2 + i * 1000, &[0xDCu8, 0][..], true, 0)))
                .collect();
            let doc = DocBuilder::new(b"").fib(0x4C, 2).fib(0x54, aliases * 1000).pieces(&pieces);
            let bytes = doc.truncated().bytes();
            let (parsed, reported) = parsed_with(&bytes);
            assert_eq!(crate::render::markdown::document_to_markdown(&parsed), "a\n");
            assert_eq!(reported, []);
            fuel(&bytes)
        };
        assert_eq!(fuel_of(4) - fuel_of(1), 3);
    }

    #[test]
    fn each_damaged_piece_recorded_and_lifted_is_charged() {
        assert_eq!(fuel(&damaged("x~\r")) - fuel(&damaged("x\u{E9}\r")), 2);
    }

    const TIMES: u8 = 0;
    const SYMBOL: u8 = 1;
    const WINGDINGS: u8 = 2;
    const MARLETT: u8 = 3;

    fn font_table() -> Vec<u8> {
        let symbol_set =
            ["Symbol", "Wingdings", "Marlett"].map(|name| ffn_in(name, SYMBOL_CHARSET));
        sttbf_ffn(&[&[ffn("Times New Roman")][..], &symbol_set].concat())
    }

    /// `sprmCRgFtc0`, `sprmCRgFtc1` and `sprmCRgFtc2` naming `ftc`.
    fn in_font(ftc: u8) -> [u8; 12] {
        [0x4F, 0x4A, ftc, 0, 0x50, 0x4A, ftc, 0, 0x51, 0x4A, ftc, 0]
    }

    fn ftc0(ftc: u8) -> [u8; 4] {
        [0x4F, 0x4A, ftc, 0]
    }

    fn c_symbol(ftc: u8, xchar: u16) -> Vec<u8> {
        [&[0x09, 0x6A, ftc, 0][..], &xchar.to_le_bytes()].concat()
    }

    /// A document whose main text is the UTF-16 runs' text, without a font table.
    fn unicode_runs(runs: TextRuns<'_>) -> DocBuilder {
        let units = |text: &str| u32::try_from(text.encode_utf16().count()).unwrap();
        let text: Vec<u8> = runs
            .iter()
            .flat_map(|&(text, _)| text.encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect();
        let cps = runs.iter().map(|&(text, _)| units(text)).sum();
        let chpx: Vec<(u32, &[u8])> =
            runs.iter().map(|&(text, grpprl)| (2 * units(text), grpprl)).collect();
        let doc = DocBuilder::new(b"").fib(Fib::CCP_TEXT, cps);
        doc.pieces(&[(cps, &text, true, 0)]).chpx_runs(&chpx)
    }

    fn with_fonts(doc: DocBuilder) -> (String, Vec<Reported>) {
        converted(&doc.table(Fib::FC_STTBF_FFN, &font_table()).bytes())
    }

    #[test]
    fn a_symbol_font_draws_its_codes_and_a_text_font_keeps_its_text() {
        let symbol = c_symbol(SYMBOL, 0xF06D);
        let doc = unicode_runs(&[
            ("(", &symbol),
            (" \u{b1} ", &in_font(TIMES)),
            ("\u{f06d}", &in_font(TIMES)),
            ("a ", &in_font(MARLETT)),
            ("\u{f0fe}\u{f06f}", &in_font(WINGDINGS)),
            ("m\r", &in_font(SYMBOL)),
        ]);
        let lost = vec![(DiagnosticCode::UnmappedSymbol, Region::Body, 1)];
        assert_eq!(with_fonts(doc), ("\u{3bc} \u{b1} a [x] [ ] \u{3bc}\n".into(), lost));
    }

    #[test]
    fn hidden_and_deleted_symbol_codes_report_only_their_omission() {
        for (props, code) in
            [(HIDDEN, DiagnosticCode::HiddenOmitted), (DELETED, DiagnosticCode::RevisionOmitted)]
        {
            let unmapped_in = |ftc| [props, &in_font(ftc)].concat();
            let (symbol, times) = (unmapped_in(SYMBOL), unmapped_in(TIMES));
            let doc = unicode_runs(&[("a", &[]), ("\u{f0ff}", &symbol), ("\u{f06d}\r", &times)]);
            assert_eq!(with_fonts(doc), ("a\n".into(), vec![(code, Region::Body, 1)]));
        }
    }

    #[test]
    fn a_field_instruction_and_a_note_mark_in_a_symbol_font_stay_as_written() {
        let link = "\x13 HYPERLINK \"www.example.org\" \x14label\x15\r";
        let (shown, reported) = with_fonts(unicode_runs(&[(link, &in_font(SYMBOL))]));
        assert_eq!(shown, "[\u{3bb}\u{3b1}\u{3b2}\u{3b5}\u{3bb}](www.example.org)\n");
        assert_eq!(reported, []);
        let symbol = in_font(SYMBOL);
        let note = runs_doc(&[("\x02", &symbol), ("a\r", &[]), ("n\r", &symbol)]);
        let (shown, reported) = with_fonts(note.fib(Fib::CCP_TEXT, 3).footnote(2));
        assert_eq!((shown.as_str(), reported), ("[^1]a\n\n[^1]: \u{3bd}\n", vec![]));
    }

    /// A case, its style sheet, run and piece modifiers, and what `m` shows.
    type LayerRow = (&'static str, Vec<u8>, Vec<u8>, Vec<u8>, &'static str);

    /// `m` as the one piece of a paragraph in style 0 under `styles`, its run `chpx` and its
    /// complex modifier `prc`, set in the font table.
    fn styled_m(styles: &[u8], chpx: &[u8], prc: &[u8]) -> String {
        let doc = DocBuilder::new(b"").fib(Fib::CCP_TEXT, 2).table(Fib::FC_STSHF, styles);
        let (shown, reported) = with_fonts(doc.clx(&[prc], &[(2, b"m\r", false, 1)]).chpx(chpx));
        assert_eq!(reported, []);
        shown
    }

    #[test]
    fn each_layer_overrides_the_font_below_and_plain_restores_the_paragraphs() {
        let (symbol, times) = (ftc0(SYMBOL), ftc0(TIMES));
        let styles = |paragraph: &[u8], character: &[u8]| {
            style_sheet(&[(Para, 0, 0x0FFF, paragraph), (Char, 0x0FFE, 0x0FFF, character)])
        };
        let plain = [0x33, 0x2A, 0];
        let beta = c_symbol(SYMBOL, 0xF062);
        let rows: [LayerRow; 12] = [
            ("paragraph style", styles(&symbol, &[]), vec![], vec![], "\u{3bc}"),
            ("character style", styles(&times, &symbol), cistd(1).into(), vec![], "\u{3bc}"),
            ("character over paragraph", styles(&symbol, &times), cistd(1).into(), vec![], "m"),
            ("run", styles(&times, &[]), symbol.into(), vec![], "\u{3bc}"),
            ("run over style", styles(&[], &symbol), [cistd(1), times].concat(), vec![], "m"),
            ("piece", styles(&[], &[]), times.into(), symbol.into(), "\u{3bc}"),
            ("plain", styles(&times, &[]), [&symbol[..], &plain].concat(), vec![], "m"),
            ("plain in a piece", styles(&symbol, &[]), times.into(), plain.into(), "\u{3bc}"),
            (
                "plain clears",
                styles(BOLD, ITALIC),
                [&cistd(1)[..], &plain].concat(),
                vec![],
                "**m**",
            ),
            (
                "a toggle after plain resolves against the paragraph",
                styles(BOLD, ITALIC),
                [&cistd(1)[..], &plain, &[0x36, 0x08, 0x81]].concat(),
                vec![],
                "**_m_**",
            ),
            ("symbol, plain", styles(&[], &[]), [&beta[..], &plain].concat(), vec![], "\u{3b2}"),
            ("symbol, style", styles(&[], &[]), beta.clone(), cistd(1).into(), "\u{3b2}"),
        ];
        for (case, styles, chpx, prc, expected) in rows {
            assert_eq!(styled_m(&styles, &chpx, &prc), format!("{expected}\n"), "{case}");
        }
        let doc = DocBuilder::new(b"m\r").chpx(&[DELETED, &plain].concat());
        let deleted = vec![(DiagnosticCode::RevisionOmitted, Region::Body, 1)];
        assert_eq!(with_fonts(doc), (String::new(), deleted));
    }

    #[test]
    fn direct_script_properties_draw_in_the_bidi_font_and_outlast_plain_and_a_style() {
        let symbol_bi = [0x5E, 0x4A, SYMBOL, 0];
        let styles = |paragraph: &[u8], character: &[u8]| {
            style_sheet(&[(Para, 0, 0x0FFF, paragraph), (Char, 0x0FFE, 0x0FFF, character)])
        };
        let (plain, bidi, complex) = ([0x33, 0x2A, 0], [0x5A, 0x08, 1], [0x82, 0x08, 1]);
        let hint = |idct: u8| [0x6F, 0x28, idct];
        let rows: [LayerRow; 7] = [
            ("bidi font", styles(&[], &[]), [&symbol_bi[..], &bidi].concat(), vec![], "\u{3bc}"),
            ("bidi font unused", styles(&[], &[]), symbol_bi.into(), vec![], "m"),
            (
                "complex hint",
                styles(&[], &[]),
                [&symbol_bi[..], &hint(2)].concat(),
                vec![],
                "\u{3bc}",
            ),
            ("east asian hint", styles(&[], &[]), [&symbol_bi[..], &hint(1)].concat(), vec![], "m"),
            (
                "complex, plain",
                styles(&symbol_bi, &[]),
                [complex, plain].concat(),
                vec![],
                "\u{3bc}",
            ),
            (
                "bidi, style",
                styles(&[], &symbol_bi),
                [&bidi[..], &cistd(1)].concat(),
                vec![],
                "\u{3bc}",
            ),
            (
                "hint, style",
                styles(&[], &symbol_bi),
                [&hint(2)[..], &cistd(1)].concat(),
                vec![],
                "\u{3bc}",
            ),
        ];
        for (case, styles, chpx, prc, expected) in rows {
            assert_eq!(styled_m(&styles, &chpx, &prc), format!("{expected}\n"), "{case}");
        }
    }

    /// A case, its Stshi length and default fonts, its text and run modifiers, and what shows.
    type DefaultRow = (&'static str, u16, [u16; 4], &'static str, &'static [u8], &'static str);

    /// An STSH of no styles whose Stshi is `cb_stshi` bytes of a 20-byte one, followed by
    /// what remains of it, naming the default fonts `ftcs`.
    fn default_fonts(cb_stshi: u16, ftcs: [u16; 4]) -> Vec<u8> {
        let stshi: Vec<u8> =
            [0, 10, 0, 0, 0, 0].into_iter().chain(ftcs).flat_map(u16::to_le_bytes).collect();
        [&cb_stshi.to_le_bytes()[..], &stshi].concat()
    }

    #[test]
    fn the_style_sheet_names_each_slots_default_font_and_the_bidi_one_only_when_it_holds_it() {
        const BIDI: &[u8] = &[0x5A, 0x08, 1];
        const EAST_ASIAN: &[u8] = &[0x6F, 0x28, 1];
        let symbol_in = |slot: usize| {
            let mut ftcs = [0; 4];
            ftcs[slot] = u16::from(SYMBOL);
            ftcs
        };
        let rows: [DefaultRow; 9] = [
            ("ascii", 18, symbol_in(0), "m", &[], "\u{3bc}"),
            ("ascii past the Stshi", 12, symbol_in(0), "m", &[], "m"),
            ("east asian", 18, symbol_in(1), "\u{f06d}", EAST_ASIAN, "\u{3bc}"),
            ("other", 18, symbol_in(2), "\u{f06d}", &[], "\u{3bc}"),
            ("other is not ascii", 18, symbol_in(2), "m", &[], "m"),
            ("bidi", 20, symbol_in(3), "m", BIDI, "\u{3bc}"),
            ("bidi past the Stshi", 18, symbol_in(3), "m", BIDI, "m"),
            ("bidi off", 20, symbol_in(3), "m", &[], "m"),
            ("complex scripts", 20, symbol_in(3), "m", &[0x82, 0x08, 1], "\u{3bc}"),
        ];
        for (case, cb_stshi, ftcs, text, chpx, expected) in rows {
            let doc = unicode_runs(&[(text, chpx), ("\r", &[])]);
            let doc = doc.table(Fib::FC_STSHF, &default_fonts(cb_stshi, ftcs));
            let (shown, reported) = with_fonts(doc);
            assert_eq!((shown, reported), (format!("{expected}\n"), vec![]), "{case}");
        }
    }

    #[test]
    fn an_unresolved_font_keeps_its_text_and_is_reported_once_per_region() {
        let table = sttbf_ffn(&[ffn("Times New Roman"), vec![0; 3]]);
        let unresolved = in_font(1);
        let past = in_font(7);
        let doc = unicode_runs(&[
            ("a ", &unresolved),
            ("b", &[0x51, 0x4A, 5, 0]),
            ("c", &unresolved),
            (" \r", &past),
            ("\u{f06d}\r", &past),
        ]);
        let (shown, reported) = converted(&doc.table(Fib::FC_STTBF_FFN, &table).bytes());
        let mut expected = vec![
            (DiagnosticCode::FormattingUnreadable, Region::Body, 1),
            (DiagnosticCode::UnmappedSymbol, Region::Body, 1),
        ];
        expected.sort();
        assert_eq!((shown.as_str(), reported), ("a bc\n", expected));
        let doc = unicode_runs(&[("a", &past), ("\u{b1}\r", &unresolved)]);
        assert_eq!(converted(&doc.bytes()), ("a\u{b1}\n".into(), vec![]));
    }

    #[test]
    fn an_entry_ending_before_one_unit_of_name_is_unresolved_on_visible_text() {
        let unreadable = vec![(DiagnosticCode::FormattingUnreadable, Region::Body, 1)];
        let mut one_unit = ffn("S");
        one_unit.truncate(fonts::FFN_NAME + 2);
        let rows = [
            ("no name", vec![0; fonts::FFN_NAME], unreadable.clone()),
            ("half a unit", vec![0; fonts::FFN_NAME + 1], unreadable),
            ("one unit", one_unit, vec![]),
            ("empty name", ffn(""), vec![]),
        ];
        for (case, entry, expected) in rows {
            let table = sttbf_ffn(&[ffn("Times New Roman"), entry]);
            let doc = unicode_runs(&[("a\r", &in_font(1))]).table(Fib::FC_STTBF_FFN, &table);
            assert_eq!(converted(&doc.bytes()), ("a\n".into(), expected), "{case}");
        }
    }
}
