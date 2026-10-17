//! Legacy PowerPoint 97-2003 binary (.ppt): OLE2 container, record stream.
//! Slides resolve through the persist directory (the only default path);
//! text comes from TextHeaderAtom + TextCharsAtom/TextBytesAtom with
//! StyleTextPropAtom runs and TxMasterStyleAtom defaults applied. Raw
//! stream-order scanning exists only as an explicitly labelled recovery for
//! files whose persist directory is unusable. Speaker notes are included
//! (fixed policy), rendered as a quote after their slide.

mod styletext;

use crate::ctx::{ParseCtx, UnitScope};
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{Asset, Block, Document, Inline, Style, inlines_are_empty};
use crate::shared::binary::{
    OptionalStream, decode_utf16, get_u16, get_u32, optional_ole_stream, read_ole_stream,
    utf16le_units,
};
use crate::shared::delta::{StyleDelta, rebase_emphasis};
use crate::shared::link_label::{label_is_absolute_url, label_shows_target};
use crate::shared::list::{ListEntry, ListKey, MarkerKind, flush_list};
use crate::shared::officeart::{self, record_at, records};
use crate::shared::text::clean_text;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Cursor;
use styletext::{CharProps, MasterLevel, StyleRuns};

/// The slide that owns a notes page: its `NotesAtom`'s `slideIdRef`, 0 for none.
fn notes_owner(notes: &[u8]) -> Option<u32> {
    let (.., atom) = records(notes).find(|&(_, t, _)| t == 0x03F1)?;
    get_u32(atom, 0).filter(|&id| id != 0)
}

/// One master's per-text-type level defaults, keyed by TxMasterStyleAtom
/// instance (the text type).
type MasterStyles = HashMap<u16, Vec<MasterLevel>>;

pub fn parse(parse_ctx: &ParseCtx, bytes: &[u8]) -> Result<Document, ConvertError> {
    let cursor = Cursor::new(bytes);
    let mut ole = cfb::CompoundFile::open(cursor)
        .map_err(|_| ConvertError::malformed("not an OLE2 compound file"))?;
    let data = read_ole_stream(parse_ctx, &mut ole, "PowerPoint Document")?;
    let current_user = optional_ole_stream(parse_ctx, &mut ole, "Current User")?.or_empty();
    if get_u32(&current_user, 12) == Some(0xF3D1_C4DF) {
        return Err(ConvertError::Encrypted);
    }

    let mut ex = Extractor::new(parse_ctx);
    if !ex.parse_slides(&data, &current_user)? {
        // Labelled recovery path: the persist directory is unusable, so text
        // is taken in raw stream order (may include superseded edits).
        parse_ctx.emit(DiagnosticCode::RecoveredStreamOrder, Region::Body);
        ex = Extractor::new(parse_ctx);
        ex.recovering = true;
        let mut documents = scan(parse_ctx, &data)?.into_iter().filter(|&(_, t, _)| t == 0x03E8);
        if let Some((.., document)) = documents.next_back() {
            ex.read_document(document)?;
        }
        ex.walk(&data)?;
        ex.end_segment(None)?;
    }
    if ex.encrypted {
        return Err(ConvertError::Encrypted);
    }
    let pictures = collect_pictures(parse_ctx, &mut ole)?;
    ex.report_pictures(&pictures);
    ex.report_masters()?;
    Ok(Document { blocks: ex.into_blocks()?, notes: Vec::new(), assets: pictures.assets })
}

/// The deck's pictures, kept as document-level assets, and what keeps a
/// slide from showing one.
#[derive(Debug, Default)]
struct Pictures {
    assets: Vec<Asset>,
    /// Offsets of the `Pictures` records that do not decode.
    undecodable: HashSet<usize>,
    unreadable: bool,
}

/// Retain the deck's embedded pictures from the `Pictures` stream (OfficeArt
/// `BStore` file blocks), reporting a stream that does not read.
fn collect_pictures<R: std::io::Read + std::io::Seek>(
    parse_ctx: &ParseCtx,
    ole: &mut cfb::CompoundFile<R>,
) -> Result<Pictures, ConvertError> {
    let pictures = match optional_ole_stream(parse_ctx, ole, "Pictures")? {
        OptionalStream::Read(pictures) => pictures,
        OptionalStream::Absent => return Ok(Pictures::default()),
        OptionalStream::Unreadable => {
            parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Body);
            return Ok(Pictures { unreadable: true, ..Pictures::default() });
        }
    };
    let mut sink = crate::shared::assets::AssetSink::new(parse_ctx);
    let mut undecodable = HashSet::new();
    let limits = parse_ctx.limits();
    let cap = limits.max_entry_bytes;
    let mut pos = 0usize;
    let mut index = 0u64;
    while let Some((ver_inst, rec_type, body)) = officeart::record_at(&pictures, pos) {
        let offset = pos;
        pos += 8 + body.len();
        index += 1;
        parse_ctx.charge(1)?;
        if index > limits.max_records {
            return Err(ConvertError::limit(Limit::Records, limits.max_records));
        }
        let blip = match rec_type {
            0xF007 => officeart::fbse_blip(body, cap),
            _ => officeart::decode_blip(ver_inst, rec_type, body, cap),
        }?;
        match blip {
            Some(blip) => {
                sink.add(
                    blip.media_type.to_string(),
                    format!("pictures/{index}.{}", blip.extension),
                    &blip.bytes,
                )?;
            }
            None => {
                undecodable.insert(offset);
            }
        }
    }
    Ok(Pictures { assets: sink.assets, undecodable, unreadable: false })
}

fn check_record_depth(parse_ctx: &ParseCtx, depth: usize) -> Result<(), ConvertError> {
    let max = parse_ctx.limits().max_record_depth;
    if depth as u64 >= max {
        return Err(ConvertError::limit(Limit::RecordDepth, max));
    }
    Ok(())
}

/// A record's (verAndInstance, recType, body).
type Record<'a> = (u16, u16, &'a [u8]);

/// The records of `data`, each charged as one unit of work.
fn scan<'a>(parse_ctx: &ParseCtx, data: &'a [u8]) -> Result<Vec<Record<'a>>, ConvertError> {
    records(data).map(|record| parse_ctx.charge(1).map(|()| record)).collect()
}

/// The bodies of the records of `rec_type` in `data`, every record charged.
fn children<'a>(
    parse_ctx: &ParseCtx,
    data: &'a [u8],
    rec_type: u16,
) -> Result<Vec<&'a [u8]>, ConvertError> {
    let found = scan(parse_ctx, data)?.into_iter().filter(|&(_, t, _)| t == rec_type);
    Ok(found.map(|(.., body)| body).collect())
}

const FSP_PATRIARCH: u32 = 0x0004;
const FSP_BACKGROUND: u32 = 0x0400;

/// What a master shows on the slides that show it.
#[derive(Default)]
struct MasterFacts {
    /// A shape other than the patriarch, the background and placeholders.
    objects: bool,
    /// A background filled with a picture or texture.
    picture_background: bool,
    shown: MasterUse,
    /// A title master's main master and what of it the title master shows.
    follows: Option<(u32, MasterUse)>,
}

impl MasterFacts {
    /// Whether the slides showing `shown` of this master show something no
    /// output carries.
    fn omits(&self, shown: MasterUse) -> bool {
        let own_background = !self.follows.is_some_and(|(_, own)| own.background);
        self.objects && shown.objects
            || self.picture_background && own_background && shown.background
    }
}

/// What of a master its slides show: `SlideAtom.slideFlags`' `fMasterObjects`
/// and `fMasterBackground`.
#[derive(Default, Clone, Copy)]
struct MasterUse {
    objects: bool,
    background: bool,
}

impl MasterUse {
    fn from_flags(flags: u16) -> MasterUse {
        MasterUse { objects: flags & 0x0001 != 0, background: flags & 0x0004 != 0 }
    }

    fn add(&mut self, flags: u16) {
        *self = self.or(MasterUse::from_flags(flags));
    }

    fn or(self, other: MasterUse) -> MasterUse {
        MasterUse {
            objects: self.objects || other.objects,
            background: self.background || other.background,
        }
    }

    fn and(self, other: MasterUse) -> MasterUse {
        MasterUse {
            objects: self.objects && other.objects,
            background: self.background && other.background,
        }
    }
}

/// A shape's `fillType`, `fillBlip` and whether `fFilled` is cleared.
#[derive(Default, Clone, Copy)]
struct Fill {
    kind: Option<u32>,
    blip: Option<u32>,
    unfilled: bool,
}

impl Fill {
    fn read(&mut self, (id, value): (u16, u32)) {
        match id {
            0x0180 => self.kind = Some(value),
            0x0186 => self.blip = Some(value),
            0x01BF => self.unfilled = value & 1 << 20 != 0 && value & 1 << 4 == 0,
            _ => {}
        }
    }

    /// The `pib` of the picture or texture the shape is filled with.
    fn picture(&self) -> Option<u32> {
        let shown = matches!(self.kind, Some(2 | 3)) && !self.unfilled;
        self.blip.filter(|&pib| shown && pib != 0)
    }
}

/// A `BStore` entry: its `cRef` and the `Pictures` offset of its file block.
#[derive(Clone, Copy)]
struct StoreEntry {
    refs: u32,
    offset: Option<usize>,
}

/// A master's shape: its `FSP` flags, whether it is a placeholder, and its fill.
#[derive(Default)]
struct MasterShape {
    flags: u32,
    placeholder: bool,
    fill: Fill,
}

impl MasterShape {
    fn is_object(&self) -> bool {
        self.flags & FSP_PATRIARCH == 0 && !self.placeholder
    }

    fn has_picture_fill(&self) -> bool {
        self.fill.picture().is_some()
    }
}

fn master_shape(parse_ctx: &ParseCtx, container: &[u8]) -> Result<MasterShape, ConvertError> {
    let mut shape = MasterShape::default();
    for (ver_inst, rec_type, body) in scan(parse_ctx, container)? {
        match rec_type {
            0xF00A => shape.flags = get_u32(body, 4).unwrap_or(0),
            0xF00B | 0xF122 => {
                officeart::properties(ver_inst >> 4, body).for_each(|prop| shape.fill.read(prop));
            }
            0xF011 => shape.placeholder |= !children(parse_ctx, body, 0x0BC3)?.is_empty(),
            _ => {}
        }
    }
    Ok(shape)
}

/// A slide's drawn shape: its picture, whether that picture is the preview of
/// an object or control reported in its stead, its fill, whether it is the
/// page's background, its click hyperlinks, whether text was read in it and
/// which outline text block it shows.
#[derive(Default)]
struct ShapeFacts {
    pib: Option<u32>,
    preview: bool,
    fill: Fill,
    background: bool,
    links: Vec<u32>,
    text: bool,
    outline: Option<usize>,
}

/// A listed slide or notes page: its container when the persist reference
/// resolves to one, and its pairing id.
struct Page<'a> {
    container: Option<&'a [u8]>,
    id: Option<u32>,
}

/// A text shape being accumulated: header type, text, then styling.
struct PendingShape {
    tx_type: u8,
    text: String,
    units: Vec<u16>,
    styles: Option<StyleRuns>,
}

impl PendingShape {
    fn new(tx_type: u8) -> Self {
        PendingShape { tx_type, text: String::new(), units: Vec::new(), styles: None }
    }
}

struct Extractor<'c> {
    parse_ctx: &'c ParseCtx<'c>,
    /// Finished segments: (blocks, pairing id, is_notes).
    segments: Vec<(Vec<Block>, Option<u32>, bool)>,
    current: Vec<Block>,
    current_is_notes: bool,
    list_run: Vec<ListEntry>,
    pending: Option<PendingShape>,
    /// Master style tables in master-list order: (masterId, styles).
    masters: Vec<(u32, MasterStyles)>,
    /// Index into `masters` for the slide being extracted (0 fallback).
    active_master: usize,
    shape_counter: u64,
    encrypted: bool,
    /// Raw-stream recovery: no persist lists, so notes descend inline.
    recovering: bool,
    /// Records visited across the whole extraction, capped.
    records: u64,
    /// Each `ExHyperlink`'s target by id, empty for an internal jump.
    links: HashMap<u32, String>,
    /// What each external object the slides name by id loses.
    objects: HashMap<u32, DiagnosticCode>,
    /// The `BStore` entries, in `pib` order.
    bstore: Vec<StoreEntry>,
    shape: Option<ShapeFacts>,
    /// The hyperlink whose text range has not been read yet.
    link: Option<u32>,
    /// The 0-based unit the page being walked reports to.
    unit: Option<usize>,
    /// Pictures the slides place: (unit, region, pib).
    placed: BTreeSet<(Option<usize>, Region, u32)>,
    /// Each main and title master's facts by masterId, in master-list order.
    master_facts: Vec<(u32, MasterFacts)>,
    /// The slide being walked has its own background, not its master's.
    own_background: bool,
    /// The page's outline text blocks in `TextHeaderAtom` order.
    outline: Vec<String>,
    /// The slide list's outline text of the page is being read.
    reading_outline: bool,
    /// What slides show of masters they cannot name, as in recovery.
    recovered_use: MasterUse,
    recovered_slides: usize,
}

/// The persist-resolved layout of the presentation: slide/notes lists from
/// the current DocumentContainer, persist id -> offset for every container.
struct DocLayout<'a> {
    persist: HashMap<u32, usize>,
    document: &'a [u8],
    slide_list: &'a [u8],
    notes_list: Option<&'a [u8]>,
    master_list: Option<&'a [u8]>,
}

/// Resolve the UserEditAtom chain into the persist directory and find the
/// DocumentContainer's SlideListWithText instances. `None` means the persist
/// directory is unusable and the caller falls back to raw-order recovery.
fn locate_document<'a>(data: &'a [u8], current_user: &[u8]) -> Option<DocLayout<'a>> {
    let mut persist: HashMap<u32, usize> = HashMap::new();
    let mut doc_persist: Option<u32> = None;
    let mut edit_off = get_u32(current_user, 16)? as usize;
    for _ in 0..100 {
        if edit_off == 0 {
            break;
        }
        let (_, rec_type, body) = record_at(data, edit_off)?;
        if rec_type != 0x0FF5 {
            return None;
        }
        if doc_persist.is_none() {
            doc_persist = get_u32(body, 16);
        }
        let dir_off = get_u32(body, 12)? as usize;
        if let Some((_, 0x1772, dir)) = record_at(data, dir_off) {
            let mut pos = 0;
            while pos + 4 <= dir.len() {
                let head = get_u32(dir, pos)?;
                let id = head & 0xF_FFFF;
                let count = (head >> 20) as usize;
                pos += 4;
                for k in 0..count {
                    // Newer edits win: keep the first offset seen.
                    persist.entry(id + k as u32).or_insert(get_u32(dir, pos)? as usize);
                    pos += 4;
                }
            }
        }
        let prev = get_u32(body, 8)? as usize;
        if prev == edit_off {
            break;
        }
        edit_off = prev;
    }

    let doc_off = *persist.get(&doc_persist?)?;
    let Some((_, 0x03E8, doc)) = record_at(data, doc_off) else {
        return None;
    };
    let (.., slide_list) =
        records(doc).find(|&(ver_inst, rec_type, _)| rec_type == 0x0FF0 && ver_inst >> 4 == 0)?;
    let notes_list = records(doc)
        .find(|&(ver_inst, rec_type, _)| rec_type == 0x0FF0 && ver_inst >> 4 == 2)
        .map(|(.., body)| body);
    let master_list = records(doc)
        .find(|&(ver_inst, rec_type, _)| rec_type == 0x0FF0 && ver_inst >> 4 == 1)
        .map(|(.., body)| body);
    Some(DocLayout { persist, document: doc, slide_list, notes_list, master_list })
}

/// One master's TxMasterStyleAtoms, keyed by text-type instance, reporting
/// the level defaults that do not parse.
fn master_styles(parse_ctx: &ParseCtx, master: &[u8]) -> MasterStyles {
    let mut styles = MasterStyles::new();
    for (ver_inst, rec_type, body) in records(master) {
        let instance = ver_inst >> 4;
        if rec_type != 0x0FA3 || styles.contains_key(&instance) {
            continue;
        }
        let (levels, complete) = styletext::parse_master_style(body, instance);
        if !complete {
            parse_ctx.emit(DiagnosticCode::FormattingUnreadable, Region::Body);
        }
        styles.insert(instance, levels);
    }
    styles
}

/// Main masters (`0x03F8`) and title masters (`0x03EE`) in master-list order
/// (MasterPersistAtoms: persistIdRef at 0, masterId at 12), as (masterId,
/// record type, container); falls back to a persist-directory scan for main
/// masters when the list is absent so single-master decks still get their
/// defaults.
fn collect_masters<'a>(
    master_list: Option<&[u8]>,
    persist: &HashMap<u32, usize>,
    data: &'a [u8],
) -> Vec<(u32, u16, &'a [u8])> {
    let mut out = Vec::new();
    if let Some(list) = master_list {
        for (_, rec_type, body) in records(list) {
            if rec_type != 0x03F3 {
                continue;
            }
            let (Some(persist_ref), Some(master_id)) = (get_u32(body, 0), get_u32(body, 12)) else {
                continue;
            };
            if let Some(&off) = persist.get(&persist_ref)
                && let Some((_, rec_type @ (0x03F8 | 0x03EE), master)) = record_at(data, off)
            {
                out.push((master_id, rec_type, master));
            }
        }
    }
    if out.is_empty() {
        let mut offs: Vec<usize> = persist.values().copied().collect();
        offs.sort_unstable();
        for off in offs {
            if let Some((_, 0x03F8, master)) = record_at(data, off) {
                out.push((0, 0x03F8, master));
            }
        }
    }
    out
}

impl<'c> Extractor<'c> {
    fn new(parse_ctx: &'c ParseCtx<'c>) -> Self {
        Extractor {
            parse_ctx,
            segments: Vec::new(),
            current: Vec::new(),
            current_is_notes: false,
            list_run: Vec::new(),
            pending: None,
            masters: Vec::new(),
            active_master: 0,
            shape_counter: 0,
            encrypted: false,
            recovering: false,
            records: 0,
            links: HashMap::new(),
            objects: HashMap::new(),
            bstore: Vec::new(),
            shape: None,
            link: None,
            unit: None,
            placed: BTreeSet::new(),
            master_facts: Vec::new(),
            own_background: false,
            outline: Vec::new(),
            reading_outline: false,
            recovered_use: MasterUse::default(),
            recovered_slides: 0,
        }
    }

    fn region(&self) -> Region {
        if self.current_is_notes { Region::Note } else { Region::Body }
    }

    /// Walk slides in presentation order: the UserEditAtom chain yields the
    /// persist directory, the DocumentContainer's SlideListWithText yields
    /// slide order and outline text, each slide container its own textboxes.
    /// `Ok(false)` means the persist directory was unusable.
    fn parse_slides(&mut self, data: &[u8], current_user: &[u8]) -> Result<bool, ConvertError> {
        let Some(layout) = locate_document(data, current_user) else {
            return Ok(false);
        };
        self.read_document(layout.document)?;
        let masters = collect_masters(layout.master_list, &layout.persist, data);
        for &(id, _, master) in &masters {
            self.read_master(id, master)?;
        }
        let main = masters.into_iter().filter(|&(_, rec_type, _)| rec_type == 0x03F8);
        let parse_ctx = self.parse_ctx;
        self.masters = main.map(|(id, _, m)| (id, master_styles(parse_ctx, m))).collect();
        let units =
            self.walk_slide_list(layout.slide_list, &layout.persist, data, false, &HashMap::new())?;
        if let Some(notes_list) = layout.notes_list {
            self.walk_slide_list(notes_list, &layout.persist, data, true, &units)?;
        }
        Ok(true)
    }

    /// Walk one `SlideListWithText`, each page in its unit: a slide by its place
    /// in the list, a notes page in its owning slide's unit (`slide_units`).
    /// Returns the unit of each slide id.
    fn walk_slide_list(
        &mut self,
        list: &[u8],
        persist: &HashMap<u32, usize>,
        data: &[u8],
        is_notes: bool,
        slide_units: &HashMap<u32, usize>,
    ) -> Result<HashMap<u32, usize>, ConvertError> {
        let parse_ctx = self.parse_ctx;
        let container_type = if is_notes { 0x03F0 } else { 0x03EE };
        let mut units = HashMap::new();
        let mut position = 0usize;
        let mut scope = None;
        let mut page: Option<Page> = None;
        for (ver_inst, rec_type, body) in records(list) {
            match rec_type {
                // SlidePersistAtom: the next slide/notes page begins.
                0x03F3 => {
                    let id = self.finish_slide(page.take())?;
                    self.end_segment(id)?;
                    drop(scope.take());
                    self.outline.clear();
                    self.reading_outline = true;
                    self.current_is_notes = is_notes;
                    let container = get_u32(body, 0)
                        .and_then(|p| persist.get(&p))
                        .and_then(|&off| record_at(data, off))
                        .filter(|&(_, t, _)| t == container_type)
                        .map(|(.., container)| container);
                    let id = if is_notes {
                        container.and_then(notes_owner)
                    } else {
                        get_u32(body, 12).filter(|&id| id != 0)
                    };
                    self.unit = if is_notes {
                        id.and_then(|id| slide_units.get(&id).copied())
                    } else {
                        if let Some(id) = id {
                            units.entry(id).or_insert(position);
                        }
                        let unit = position;
                        position = position.saturating_add(1);
                        Some(unit)
                    };
                    scope = self.unit.map(|unit| parse_ctx.enter_unit(unit));
                    self.own_background = false;
                    if !is_notes {
                        self.select_master(container);
                    }
                    page = Some(Page { container, id });
                }
                _ => self.record(ver_inst, rec_type, body)?,
            }
        }
        let id = self.finish_slide(page)?;
        self.end_segment(id)?;
        drop(scope);
        self.unit = None;
        Ok(units)
    }

    /// Emit a page's own textboxes after its outline text, or report it
    /// unreadable. Returns the segment's pairing id.
    fn finish_slide(&mut self, page: Option<Page>) -> Result<Option<u32>, ConvertError> {
        self.reading_outline = false;
        let Some(page) = page else {
            return Ok(None);
        };
        match page.container {
            Some(container) => self.walk(container)?,
            None => self.parse_ctx.emit(DiagnosticCode::UnitUnreadable, self.region()),
        }
        Ok(page.id)
    }

    /// Read the `DocumentContainer`'s external objects and picture store, which
    /// slides name by id.
    fn read_document(&mut self, document: &[u8]) -> Result<(), ConvertError> {
        let parse_ctx = self.parse_ctx;
        for (_, rec_type, body) in scan(parse_ctx, document)? {
            match rec_type {
                0x0409 => self.read_objects(body)?,
                0x040B => {
                    for group in children(parse_ctx, body, 0xF000)? {
                        for store in children(parse_ctx, group, 0xF001)? {
                            let entries = scan(parse_ctx, store)?.into_iter();
                            self.bstore = entries
                                .map(|(_, t, fbse)| StoreEntry {
                                    refs: get_u32(fbse, 24).filter(|_| t == 0xF007).unwrap_or(0),
                                    offset: get_u32(fbse, 28)
                                        .filter(|_| t == 0xF007)
                                        .and_then(|offset| usize::try_from(offset).ok()),
                                })
                                .collect();
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Read an `ExObjList`: hyperlink targets, and what each OLE object,
    /// control and media clip the slides name by id loses.
    fn read_objects(&mut self, list: &[u8]) -> Result<(), ConvertError> {
        let parse_ctx = self.parse_ctx;
        for (_, rec_type, body) in scan(parse_ctx, list)? {
            let (code, id_atom, id_at) = match rec_type {
                0x0FD7 => (None, 0x0FD3, 0),
                0x0FCC | 0x0FCE => (Some(DiagnosticCode::EmbeddedObject), 0x0FC3, 8),
                0x0FEE => (Some(DiagnosticCode::FormControlUnread), 0x0FC3, 8),
                0x1006 | 0x1007 | 0x100D..=0x1010 => {
                    (Some(DiagnosticCode::GraphicUnread), 0x1004, 0)
                }
                _ => continue,
            };
            let mut entries = scan(parse_ctx, body)?;
            if let Some(&(.., video)) = entries.iter().find(|&&(_, t, _)| t == 0x1005) {
                entries.extend(scan(parse_ctx, video)?);
            }
            let atom = |t: u16, instance: u16| {
                entries.iter().find(|&&(v, rt, _)| rt == t && v >> 4 == instance).map(|e| e.2)
            };
            let Some(id) = atom(id_atom, 0).and_then(|a| get_u32(a, id_at)) else {
                continue;
            };
            match code {
                Some(code) => _ = self.objects.insert(id, code),
                None => {
                    let target = atom(0x0FBA, 1).map(|s| decode_utf16(utf16le_units(s)).0);
                    self.links.insert(id, target.unwrap_or_default());
                }
            }
        }
        Ok(())
    }

    /// Note what the master `id` repeats on the slides that show it, and for a
    /// title master, which main master it follows and what of it it shows.
    fn read_master(&mut self, id: u32, master: &[u8]) -> Result<(), ConvertError> {
        let parse_ctx = self.parse_ctx;
        let mut facts = MasterFacts::default();
        for (_, rec_type, drawing) in scan(parse_ctx, master)? {
            if rec_type == 0x03EF {
                let main = get_u32(drawing, 12).filter(|&main| main != 0);
                let own = MasterUse::from_flags(get_u16(drawing, 20).unwrap_or(0));
                facts.follows = main.map(|main| (main, own));
            }
            if rec_type != 0x040C {
                continue;
            }
            for group in children(parse_ctx, drawing, 0xF002)? {
                for (_, rec_type, body) in scan(parse_ctx, group)? {
                    match rec_type {
                        0xF003 => {
                            for (_, rec_type, child) in scan(parse_ctx, body)? {
                                facts.objects |= match rec_type {
                                    0xF003 => true,
                                    0xF004 => master_shape(parse_ctx, child)?.is_object(),
                                    _ => false,
                                };
                            }
                        }
                        0xF004 => {
                            facts.picture_background |=
                                master_shape(parse_ctx, body)?.has_picture_fill();
                        }
                        _ => {}
                    }
                }
            }
        }
        self.master_facts.push((id, facts));
        Ok(())
    }

    /// The facts of the master `id`, else of the first main master.
    fn master_facts_mut(&mut self, id: Option<u32>) -> Option<&mut MasterFacts> {
        let fallback = self.masters.first().map(|&(id, _)| id);
        let at = |id: u32| self.master_facts.iter().position(|&(master, _)| master == id);
        let index = id.and_then(at).or_else(|| fallback.and_then(at))?;
        self.master_facts.get_mut(index).map(|(_, facts)| facts)
    }

    /// Report each master whose shapes or background picture its slides
    /// show, directly or through a title master, and no output carries,
    /// charging each master once.
    fn report_masters(&self) -> Result<(), ConvertError> {
        let masters = u64::try_from(self.master_facts.len()).unwrap_or(u64::MAX);
        self.parse_ctx.charge(masters)?;
        let mut through_titles: HashMap<u32, MasterUse> = HashMap::new();
        for (_, title) in &self.master_facts {
            if let Some((main, own)) = title.follows {
                let shown = through_titles.entry(main).or_default();
                *shown = shown.or(title.shown.and(own));
            }
        }
        let shown = self.master_facts.iter().filter(|&(id, master)| {
            let titles = through_titles.get(id).copied().unwrap_or_default();
            master.omits(master.shown.or(self.recovered_use).or(titles))
        });
        self.parse_ctx.emit_count(
            DiagnosticCode::HeaderOmitted,
            Region::Header,
            u64::try_from(shown.count()).unwrap_or(u64::MAX),
        );
        Ok(())
    }

    /// Report each picture the slides show: once per page that places it, or
    /// in recovery once per `BStore` entry with references, unless its record
    /// does not decode, which is reported once instead.
    fn report_pictures(&self, pictures: &Pictures) {
        if pictures.unreadable {
            return;
        }
        let shown: Vec<(Option<usize>, Region, Option<usize>)> = if self.recovering {
            let referenced = self.bstore.iter().filter(|entry| entry.refs > 0);
            referenced.map(|entry| (None, Region::Body, entry.offset)).collect()
        } else {
            let offset = |pib: u32| {
                let entry = usize::try_from(pib).ok().and_then(|pib| pib.checked_sub(1));
                entry.and_then(|i| self.bstore.get(i)).and_then(|entry| entry.offset)
            };
            self.placed.iter().map(|&(unit, region, pib)| (unit, region, offset(pib))).collect()
        };
        let mut unavailable = BTreeSet::new();
        for (unit, region, offset) in shown {
            match offset.filter(|offset| pictures.undecodable.contains(offset)) {
                Some(offset) => _ = unavailable.insert(offset),
                None => {
                    let _unit = unit.map(|unit| self.parse_ctx.enter_unit(unit));
                    self.parse_ctx.emit(DiagnosticCode::ImageUnplaced, region);
                }
            }
        }
        let count = u64::try_from(unavailable.len()).unwrap_or(u64::MAX);
        self.parse_ctx.emit_count(DiagnosticCode::ImageUnavailable, Region::Body, count);
    }

    fn end_segment(&mut self, id: Option<u32>) -> Result<(), ConvertError> {
        self.settle_link()?;
        self.finish_shape()?;
        self.flush_shape();
        flush_list(&mut self.current, &mut self.list_run);
        if !self.current.is_empty() {
            let blocks = std::mem::take(&mut self.current);
            self.segments.push((blocks, id, self.current_is_notes));
        }
        Ok(())
    }

    fn into_blocks(mut self) -> Result<Vec<Block>, ConvertError> {
        self.end_segment(None)?;
        let mut slides: Vec<(Option<u32>, Vec<Block>)> = Vec::new();
        let mut notes: Vec<(Option<u32>, Vec<Block>)> = Vec::new();
        for (blocks, id, is_notes) in self.segments {
            if is_notes {
                notes.push((id, blocks));
            } else {
                slides.push((id, blocks));
            }
        }
        // Notes pages pair to slides by their stored slide id, not by list
        // position: the notes list may be sparse (notes on only some
        // slides), which order-based zipping would misattribute.
        let mut used = vec![false; notes.len()];
        let mut out = Vec::new();
        for (sid, blocks) in slides {
            out.extend(blocks);
            for (i, (nid, nblocks)) in notes.iter_mut().enumerate() {
                if !used[i] && sid.is_some() && *nid == sid {
                    used[i] = true;
                    out.push(Block::BlockQuote(std::mem::take(nblocks)));
                }
            }
        }
        // Notes without a resolvable owner keep document order at the end.
        for (used, (_, nblocks)) in used.into_iter().zip(notes) {
            if !used && !nblocks.is_empty() {
                out.push(Block::BlockQuote(nblocks));
            }
        }
        Ok(out)
    }

    /// Pick the master the slide references (SlideAtom.masterIdRef at
    /// offset 12); the first listed master is the deterministic fallback.
    fn select_master(&mut self, slide: Option<&[u8]>) {
        self.active_master = slide
            .and_then(|body| records(body).find(|&(_, rec_type, _)| rec_type == 0x03EF))
            .and_then(|(.., atom)| get_u32(atom, 12))
            .and_then(|mid| self.masters.iter().position(|&(id, _)| id == mid))
            .unwrap_or(0);
    }

    /// Iterative container walk over an explicit stack with fixed depth and
    /// record-count bounds — nesting or record counts beyond any real
    /// presentation are attack shapes and hard-fail.
    fn walk(&mut self, data: &[u8]) -> Result<(), ConvertError> {
        let parse_ctx = self.parse_ctx;
        let mut stack: Vec<(&[u8], usize, u16, Option<UnitScope<'_>>)> = vec![(data, 0, 0, None)];
        while let Some((buf, pos, ..)) = stack.last_mut() {
            let Some((ver_inst, rec_type, body)) = record_at(buf, *pos) else {
                match stack.pop() {
                    Some((.., 0x03F0, _)) => {
                        self.end_segment(None)?;
                        self.current_is_notes = false;
                    }
                    Some((.., Some(_))) => {
                        self.settle_link()?;
                        self.finish_shape()?;
                    }
                    _ => {}
                }
                continue;
            };
            *pos += 8 + body.len();
            self.charge_record()?;
            if ver_inst & 0xF != 0xF {
                let in_client_data = stack.iter().any(|&(_, _, parent, _)| parent == 0xF011);
                self.atom(ver_inst, rec_type, body, in_client_data)?;
                continue;
            }
            match rec_type {
                // CryptSession10Container: the stream is encrypted.
                0x2F14 => self.encrypted = true,
                0x03F8 if self.recovering => self.read_master(0, body)?,
                // Notes containers are walked via the notes list; recovery
                // has no lists, so their text is taken inline (as notes).
                // A NotesAtom slideIdRef in the masters range (high bit
                // set) marks the notes master: template chrome, excluded.
                0x03F0 if self.recovering => {
                    if !notes_owner(body).is_some_and(|id| id & 0x8000_0000 != 0) {
                        check_record_depth(parse_ctx, stack.len())?;
                        self.end_segment(None)?;
                        self.current_is_notes = true;
                        stack.push((body, 0, rec_type, None));
                    }
                }
                // Notes, master, and handout containers are walked via their
                // own lists, not inline.
                0x03F0 | 0x03F8 | 0x0FC9 => {}
                // Only instance 0 of SlideListWithText holds slide text here.
                0x0FF0 if ver_inst >> 4 != 0 => {}
                _ => {
                    check_record_depth(parse_ctx, stack.len())?;
                    let unit = (self.recovering && rec_type == 0x03EE).then(|| {
                        self.recovered_slides = self.recovered_slides.saturating_add(1);
                        parse_ctx.enter_unit(self.recovered_slides - 1)
                    });
                    stack.push((body, 0, rec_type, unit));
                }
            }
        }
        Ok(())
    }

    /// One record outside `walk` (slide-list traversal): containers descend
    /// through the bounded walk, atoms extract directly.
    fn record(&mut self, ver_inst: u16, rec_type: u16, body: &[u8]) -> Result<(), ConvertError> {
        self.charge_record()?;
        if ver_inst & 0xF == 0xF {
            match rec_type {
                0x2F14 => self.encrypted = true,
                0x03F0 | 0x03F8 | 0x0FC9 => {}
                0x0FF0 if ver_inst >> 4 != 0 => {}
                _ => self.walk(body)?,
            }
        } else {
            self.atom(ver_inst, rec_type, body, false)?;
        }
        Ok(())
    }

    fn charge_record(&mut self) -> Result<(), ConvertError> {
        self.parse_ctx.charge(1)?;
        self.records += 1;
        let max = self.parse_ctx.limits().max_records;
        if self.records > max {
            return Err(ConvertError::limit(Limit::Records, max));
        }
        Ok(())
    }

    /// One atom; `in_client_data` when it sits in a shape's `OfficeArtClientData`.
    fn atom(
        &mut self,
        ver_inst: u16,
        rec_type: u16,
        body: &[u8],
        in_client_data: bool,
    ) -> Result<(), ConvertError> {
        let region = self.region();
        match rec_type {
            // TextHeaderAtom: a new text shape begins.
            0x0F9F => {
                self.settle_link()?;
                self.flush_shape();
                self.pending = Some(PendingShape::new(body.first().copied().unwrap_or(1)));
                if let Some(shape) = &mut self.shape {
                    shape.text = true;
                }
                if self.reading_outline {
                    self.outline.push(String::new());
                }
            }
            // OutlineTextRefAtom: the shape shows a block of the page's outline text.
            0x0F9E => {
                if let Some(shape) = &mut self.shape {
                    shape.outline = get_u32(body, 0).and_then(|i| usize::try_from(i).ok());
                }
            }
            // TextCharsAtom: UTF-16LE.
            0x0FA0 => {
                let (text, damaged) = decode_utf16(utf16le_units(body));
                if damaged {
                    self.parse_ctx.emit(DiagnosticCode::InvalidEncoding, region);
                }
                self.push_text(text);
            }
            // TextBytesAtom: low bytes of UTF-16 code units.
            0x0FA8 => {
                let text: String = body.iter().map(|&b| b as char).collect();
                self.push_text(text);
            }
            // StyleTextPropAtom: styling for the pending shape's text.
            0x0FA1 => {
                if let Some(pending) = &mut self.pending {
                    let (styles, complete) = styletext::parse_style_text(body, pending.units.len());
                    pending.styles = Some(styles);
                    if !complete {
                        self.parse_ctx.emit(DiagnosticCode::FormattingUnreadable, region);
                    }
                }
            }
            // SlideAtom: what of its master the slide shows.
            0x03EF => {
                let flags = get_u16(body, 20).unwrap_or(0);
                self.own_background = flags & 0x0004 == 0;
                if self.recovering {
                    self.recovered_use.add(flags);
                } else if let Some(master) = self.master_facts_mut(get_u32(body, 12)) {
                    master.shown.add(flags);
                }
            }
            // OfficeArtFSP: a drawn shape begins.
            0xF00A => {
                self.settle_link()?;
                self.finish_shape()?;
                let flags = get_u32(body, 4).unwrap_or(0);
                self.shape = Some(ShapeFacts {
                    background: flags & FSP_BACKGROUND != 0,
                    ..Default::default()
                });
            }
            0xF00B | 0xF122 => {
                if let Some(shape) = &mut self.shape {
                    for (id, value) in officeart::properties(ver_inst >> 4, body) {
                        if id == 0x0104 {
                            shape.pib = Some(value);
                        }
                        shape.fill.read((id, value));
                    }
                }
            }
            // ExObjRefAtom: the shape shows an external object.
            0x0BC1 => {
                if let Some(&code) = get_u32(body, 0).and_then(|id| self.objects.get(&id)) {
                    self.parse_ctx.emit(code, region);
                    if let Some(shape) = &mut self.shape {
                        shape.preview |= code != DiagnosticCode::GraphicUnread;
                    }
                }
            }
            // InteractiveInfoAtom: action 4 follows a hyperlink, the shape's own
            // under its client data, else a text range's.
            0x0FF3 => {
                let link = get_u32(body, 4).filter(|_| body.get(8) == Some(&4));
                if in_client_data {
                    if let (Some(shape), Some(id)) = (&mut self.shape, link) {
                        shape.links.push(id);
                    }
                } else {
                    self.settle_link()?;
                    self.link = link;
                }
            }
            // TxInteractiveInfoAtom: the text range the last hyperlink covers.
            0x0FDF => {
                if let Some(id) = self.link.take() {
                    let label = self.pending_range(get_u32(body, 0), get_u32(body, 4))?;
                    self.report_link(id, &label)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The pending shape's text in the UTF-16 range `begin..end`, charged per unit.
    fn pending_range(&self, begin: Option<u32>, end: Option<u32>) -> Result<String, ConvertError> {
        let (Some(pending), Some(begin), Some(end)) = (&self.pending, begin, end) else {
            return Ok(String::new());
        };
        let (Ok(begin), Ok(end)) = (usize::try_from(begin), usize::try_from(end)) else {
            return Ok(String::new());
        };
        let range = pending.units.get(begin..end.min(pending.units.len())).unwrap_or_default();
        self.parse_ctx.charge(range.len() as u64)?;
        Ok(decode_utf16(range.iter().copied()).0)
    }

    /// Report a hyperlink whose label does not show its external target.
    fn report_link(&self, id: u32, label: &str) -> Result<(), ConvertError> {
        let target = self.links.get(&id);
        let compared = label.len().saturating_add(target.map_or(0, String::len));
        self.parse_ctx.charge(compared as u64)?;
        let lost = match target {
            Some(target) => !target.is_empty() && !label_shows_target(label, target),
            None => !label_is_absolute_url(label),
        };
        if lost {
            self.parse_ctx.emit(DiagnosticCode::LinkTargetUnread, self.region());
        }
        Ok(())
    }

    /// Report a hyperlink that no text range followed: it labels nothing.
    fn settle_link(&mut self) -> Result<(), ConvertError> {
        match self.link.take() {
            Some(id) => self.report_link(id, ""),
            None => Ok(()),
        }
    }

    /// Keep the finished shape's picture, unless it previews a reported object,
    /// and its picture fill, unless it is a background the slide does not show;
    /// report its click hyperlinks against the text read in it.
    fn finish_shape(&mut self) -> Result<(), ConvertError> {
        let Some(shape) = self.shape.take() else {
            return Ok(());
        };
        let region = self.region();
        if let Some(pib) = shape.pib.filter(|&pib| pib != 0 && !shape.preview) {
            self.placed.insert((self.unit, region, pib));
        }
        if let Some(pib) = shape.fill.picture()
            && (!shape.background || self.own_background)
        {
            self.placed.insert((self.unit, region, pib));
        }
        let outline = shape.outline.and_then(|i| self.outline.get(i));
        let label = match (&self.pending, shape.text) {
            (Some(pending), true) => pending.text.as_str(),
            _ => outline.map_or("", String::as_str),
        };
        for id in shape.links {
            self.report_link(id, label)?;
        }
        Ok(())
    }

    fn push_text(&mut self, text: String) {
        if self.reading_outline
            && let Some(block) = self.outline.last_mut()
        {
            block.push_str(&text);
        }
        let pending = self.pending.get_or_insert_with(|| PendingShape::new(1));
        pending.units.extend(text.encode_utf16());
        pending.text.push_str(&text);
    }

    /// Emit the pending shape: paragraphs split on CR, styled by the
    /// character runs, listed by paragraph depth/bullet with master defaults.
    fn flush_shape(&mut self) {
        let Some(shape) = self.pending.take() else {
            return;
        };
        if shape.text.is_empty() {
            return;
        }
        self.shape_counter += 1;
        let shape_id = self.shape_counter;
        let is_title = matches!(shape.tx_type, 0 | 6);
        let styles = shape.styles.unwrap_or_default();
        // The active master's per-level defaults for this text type; local
        // exceptions are tri-state and resolve over these.
        let master_levels: Vec<MasterLevel> = self
            .masters
            .get(self.active_master)
            .and_then(|(_, m)| m.get(&(shape.tx_type as u16)))
            .cloned()
            .unwrap_or_default();
        let level_default =
            |depth: u16| master_levels.get(depth as usize).copied().unwrap_or_default();

        // Cursors over the style runs, counted in UTF-16 units.
        let mut char_runs = styles.chars.iter();
        let mut char_run: Option<&CharProps> = char_runs.next();
        let mut char_left = char_run.map(|r| r.count).unwrap_or(usize::MAX);
        let mut para_runs = styles.paragraphs.iter();
        let mut para_run = para_runs.next();
        let mut para_left = para_run.map(|r| r.count).unwrap_or(usize::MAX);

        let mut paragraphs: Vec<(Vec<Inline>, u16, Option<bool>)> = Vec::new();
        let mut inlines: Vec<Inline> = Vec::new();
        let mut run_text = String::new();
        let mut run_style = Style::PLAIN;
        let para_props = |r: Option<&styletext::ParaProps>| match r {
            Some(p) => (p.depth, p.bullet),
            None => (0, None),
        };

        for c in shape.text.chars() {
            let d = level_default(para_props(para_run).0);
            let style = Style {
                bold: char_run.and_then(|r| r.bold).or(d.bold).unwrap_or(false),
                italic: char_run.and_then(|r| r.italic).or(d.italic).unwrap_or(false),
                strike: false,
                code: false,
            };
            if c == '\r' {
                if !run_text.is_empty() {
                    let text = clean_text(&std::mem::take(&mut run_text));
                    if !text.is_empty() {
                        inlines.push(Inline::Text { text, style: run_style });
                    }
                }
                let (depth, bullet) = para_props(para_run);
                paragraphs.push((std::mem::take(&mut inlines), depth, bullet));
            } else if c == '\u{b}' {
                if !run_text.is_empty() {
                    let text = clean_text(&std::mem::take(&mut run_text));
                    if !text.is_empty() {
                        inlines.push(Inline::Text { text, style: run_style });
                    }
                }
                inlines.push(Inline::LineBreak);
            } else {
                if style != run_style && !run_text.is_empty() {
                    let text = clean_text(&std::mem::take(&mut run_text));
                    if !text.is_empty() {
                        inlines.push(Inline::Text { text, style: run_style });
                    }
                }
                run_style = style;
                run_text.push(c);
            }
            // Advance run cursors by the character's UTF-16 width.
            let width = c.len_utf16();
            char_left = char_left.saturating_sub(width);
            if char_left == 0 {
                char_run = char_runs.next();
                char_left = char_run.map(|r| r.count).unwrap_or(usize::MAX);
            }
            para_left = para_left.saturating_sub(width);
            if para_left == 0 {
                para_run = para_runs.next();
                para_left = para_run.map(|r| r.count).unwrap_or(usize::MAX);
            }
        }
        if !run_text.is_empty() {
            let text = clean_text(&run_text);
            if !text.is_empty() {
                inlines.push(Inline::Text { text, style: run_style });
            }
        }
        if !inlines.is_empty() {
            let (depth, bullet) = para_props(para_run);
            paragraphs.push((inlines, depth, bullet));
        }

        for (mut inlines, depth, bullet) in paragraphs {
            if inlines_are_empty(&inlines) {
                flush_list(&mut self.current, &mut self.list_run);
                continue;
            }
            if is_title {
                flush_list(&mut self.current, &mut self.list_run);
                let d = level_default(depth);
                let base = StyleDelta { bold: d.bold, italic: d.italic, ..Default::default() };
                rebase_emphasis(&mut inlines, base.resolve());
                let anchor = Some(crate::model::inlines_to_plain_text(&inlines));
                self.current.push(Block::Heading { level: 2, anchor, content: inlines });
                continue;
            }
            let bullet = bullet.or(level_default(depth).bullet).unwrap_or(false);
            if bullet {
                self.list_run.push(ListEntry {
                    level: depth as usize,
                    key: ListKey { instance: shape_id, marker: MarkerKind::Bullet },
                    number: 0,
                    label: None,
                    blocks: vec![Block::Paragraph(inlines)],
                });
            } else {
                flush_list(&mut self.current, &mut self.list_run);
                self.current.push(Block::Paragraph(inlines));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::Limits;
    use std::assert_matches;
    use std::io::Write;

    fn deflated_metafile_picture(inflated: usize) -> Vec<u8> {
        let record = crate::shared::officeart::deflated_metafile_blip(inflated);
        let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        ole.create_stream("Pictures").unwrap().write_all(&record).unwrap();
        ole.into_inner().into_inner()
    }

    #[test]
    fn a_picture_inflating_past_the_contexts_entry_bytes_is_a_limit() {
        let bytes = deflated_metafile_picture(4096);
        let mut ole = cfb::CompoundFile::open(Cursor::new(bytes)).unwrap();
        let limits = Limits { max_entry_bytes: 4096, ..Limits::default() };
        let assets = collect_pictures(&ParseCtx::new(&limits), &mut ole).unwrap().assets;
        assert_eq!(assets[0].bytes.len(), 4096);
        let limits = Limits { max_entry_bytes: 4095, ..Limits::default() };
        let err = collect_pictures(&ParseCtx::new(&limits), &mut ole).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::EntryBytes, max: 4095 });
    }

    #[test]
    fn every_picture_record_counts_against_max_records_and_work() {
        let records = [0xF01Au16, 0xF01D, 0xF01E].map(|rec_type| record(0x0000, rec_type, b"x"));
        let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        ole.create_stream("Pictures").unwrap().write_all(&records.concat()).unwrap();
        let mut ole = cfb::CompoundFile::open(Cursor::new(ole.into_inner().into_inner())).unwrap();
        let limits = Limits { max_records: 3, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        assert_eq!(collect_pictures(&parse_ctx, &mut ole).unwrap().undecodable.len(), 3);
        assert_eq!(parse_ctx.fuel(), 3);
        let limits = Limits { max_records: 2, ..Limits::default() };
        let err = collect_pictures(&ParseCtx::new(&limits), &mut ole).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Records, max: 2 });
        let limits = Limits { max_work: 2, ..Limits::default() };
        let err = collect_pictures(&ParseCtx::new(&limits), &mut ole).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Work, max: 2 });
    }

    #[test]
    fn pictures_past_the_contexts_asset_total_are_a_limit() {
        let bytes = deflated_metafile_picture(4096);
        let mut ole = cfb::CompoundFile::open(Cursor::new(bytes)).unwrap();
        let limits = Limits { max_asset_total_bytes: 4096, ..Limits::default() };
        let assets = collect_pictures(&ParseCtx::new(&limits), &mut ole).unwrap().assets;
        assert_eq!(assets[0].bytes.len(), 4096);
        let limits = Limits { max_asset_total_bytes: 4095, ..Limits::default() };
        let err = collect_pictures(&ParseCtx::new(&limits), &mut ole).unwrap_err();
        assert_matches!(
            err,
            ConvertError::ResourceLimit { limit: Limit::AssetTotalBytes, max: 4095 }
        );
    }

    fn record(ver_inst: u16, rec_type: u16, body: &[u8]) -> Vec<u8> {
        let len = u32::try_from(body.len()).unwrap();
        [&ver_inst.to_le_bytes()[..], &rec_type.to_le_bytes(), &len.to_le_bytes(), body].concat()
    }

    #[test]
    fn every_record_walked_is_charged_once() {
        let atom = record(0x0000, 0x0FA8, b"ab");
        let container = record(0x000F, 0x03EE, &[atom.clone(), atom.clone()].concat());
        let data = [container, atom].concat();
        let parse_ctx = ParseCtx::default();
        Extractor::new(&parse_ctx).walk(&data).unwrap();
        assert_eq!(parse_ctx.fuel(), 4);
    }

    #[test]
    fn a_notes_page_read_in_stream_order_ends_with_its_container() {
        let text = |text: &str| record(0, 0x0FA0, &utf16(text));
        let data = [notes(0, &[text("n")]), text("s")].concat();
        let parse_ctx = ParseCtx::default();
        let mut ex = Extractor::new(&parse_ctx);
        ex.recovering = true;
        ex.walk(&data).unwrap();
        ex.end_segment(None).unwrap();
        let pages: Vec<bool> = ex.segments.iter().map(|&(_, _, is_notes)| is_notes).collect();
        assert_eq!(pages, [true, false]);
    }

    fn atom32(rec_type: u16, words: &[u32]) -> Vec<u8> {
        let body: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        record(0, rec_type, &body)
    }

    fn container(instance: u16, rec_type: u16, children: &[Vec<u8>]) -> Vec<u8> {
        record(instance << 4 | 0xF, rec_type, &children.concat())
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn pages(instance: u16, pages: &[(u32, u32)]) -> Vec<u8> {
        let atoms: Vec<Vec<u8>> =
            pages.iter().map(|&(persist, id)| atom32(0x03F3, &[persist, 0, 0, id, 0])).collect();
        container(instance, 0x0FF0, &atoms)
    }

    fn slide(flags: u32, content: &[Vec<u8>]) -> Vec<u8> {
        slide_on(0, flags, content)
    }

    fn slide_on(master: u32, flags: u32, content: &[Vec<u8>]) -> Vec<u8> {
        let atom = atom32(0x03EF, &[0, 0, 0, master, 0, flags]);
        container(0, 0x03EE, &[&[atom][..], content].concat())
    }

    fn notes(owner: u32, content: &[Vec<u8>]) -> Vec<u8> {
        container(0, 0x03F0, &[&[atom32(0x03F1, &[owner, 0])][..], content].concat())
    }

    fn drawing(shapes: &[Vec<u8>]) -> Vec<u8> {
        container(0, 0x040C, &[container(0, 0xF002, &[container(0, 0xF003, shapes)])])
    }

    fn shape(flags: u32, props: &[(u16, u32)], client: &[Vec<u8>]) -> Vec<u8> {
        text_shape(flags, props, client, &[])
    }

    fn text_shape(
        flags: u32,
        props: &[(u16, u32)],
        client: &[Vec<u8>],
        textbox: &[Vec<u8>],
    ) -> Vec<u8> {
        let table: Vec<u8> = props
            .iter()
            .flat_map(|&(id, value)| [&id.to_le_bytes()[..], &value.to_le_bytes()].concat())
            .collect();
        let count = u16::try_from(props.len()).unwrap();
        let fsp = [1u32.to_le_bytes(), flags.to_le_bytes()].concat();
        let opt = record(count << 4 | 3, 0xF00B, &table);
        let client = container(0, 0xF011, client);
        let textbox = (!textbox.is_empty()).then(|| container(0, 0xF00D, textbox));
        let children = [record(2, 0xF00A, &fsp), opt, client, textbox.unwrap_or_default()];
        container(0, 0xF004, &children)
    }

    fn picture(pib: u32) -> Vec<u8> {
        shape(0x0A00, &[(0x4104, pib)], &[])
    }

    fn tertiary_picture(pib: u32) -> Vec<u8> {
        let fsp = [1u32.to_le_bytes(), 0x0A00u32.to_le_bytes()].concat();
        let table = [&0x4104u16.to_le_bytes()[..], &pib.to_le_bytes()].concat();
        container(0, 0xF004, &[record(2, 0xF00A, &fsp), record(1 << 4 | 3, 0xF122, &table)])
    }

    fn text(units: &[u16]) -> Vec<u8> {
        let chars: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        [atom32(0x0F9F, &[1]), record(0, 0x0FA0, &chars)].concat()
    }

    fn link(action: u32, id: u32, range: Option<(u32, u32)>) -> Vec<u8> {
        let info = container(0, 0x0FF2, &[atom32(0x0FF3, &[0, id, action, 8])]);
        let range = range.map(|(begin, end)| atom32(0x0FDF, &[begin, end]));
        [info, range.unwrap_or_default()].concat()
    }

    fn hyperlink(id: u32, target: &str, location: &str) -> Vec<u8> {
        let string = |instance: u16, text: &str| record(instance << 4, 0x0FBA, &utf16(text));
        container(0, 0x0FD7, &[atom32(0x0FD3, &[id]), string(1, target), string(3, location)])
    }

    fn ole(id: u32) -> Vec<u8> {
        ole_as(0x0FCC, id)
    }

    fn ole_as(rec_type: u16, id: u32) -> Vec<u8> {
        container(0, rec_type, &[atom32(0x0FCD, &[0, 0]), atom32(0x0FC3, &[0, 0, id, 0, 0, 0])])
    }

    fn media(rec_type: u16, id: u32) -> Vec<u8> {
        let atom = atom32(0x1004, &[id, 0]);
        match rec_type {
            0x1006 | 0x1007 => container(0, rec_type, &[container(0, 0x1005, &[atom])]),
            _ => container(0, rec_type, &[atom]),
        }
    }

    fn bstore(offsets: &[u32]) -> Vec<u8> {
        let entries: Vec<(u32, u32)> = offsets.iter().map(|&offset| (1, offset)).collect();
        referenced_bstore(&entries)
    }

    fn referenced_bstore(entries: &[(u32, u32)]) -> Vec<u8> {
        let entries: Vec<Vec<u8>> = entries
            .iter()
            .map(|&(refs, offset)| {
                let fbse = [&[0; 24][..], &refs.to_le_bytes(), &offset.to_le_bytes(), &[0; 4]];
                record(0x0062, 0xF007, &fbse.concat())
            })
            .collect();
        container(0, 0x040B, &[container(0, 0xF000, &[container(1, 0xF001, &entries)])])
    }

    fn png(decodes: bool) -> Vec<u8> {
        record(0x6E00, 0xF01E, if decodes { &[0; 21] } else { &[0; 4] })
    }

    fn master(shapes: &[Vec<u8>], fill: u32) -> Vec<u8> {
        title_master(0, 0, shapes, &[(0x0180, fill), (0x4186, 1)])
    }

    /// A main master, or with a `main` id a title master that shows `flags` of it.
    fn title_master(main: u32, flags: u32, shapes: &[Vec<u8>], fill: &[(u16, u32)]) -> Vec<u8> {
        let patriarch = shape(0x0005, &[], &[]);
        let group = container(0, 0xF003, &[&[patriarch][..], shapes].concat());
        let rec_type = if main == 0 { 0x03F8 } else { 0x03EE };
        let slide_atom = atom32(0x03EF, &[0, 0, 0, main, 0, flags]);
        container(0, rec_type, &[slide_atom, page_drawing(group, fill)])
    }

    fn page_drawing(group: Vec<u8>, background: &[(u16, u32)]) -> Vec<u8> {
        let background = shape(0x0C00, background, &[]);
        container(0, 0x040C, &[container(0, 0xF002, &[group, background])])
    }

    fn placeholder() -> Vec<u8> {
        shape(0x0A00, &[], &[atom32(0x0BC3, &[0, 0])])
    }

    /// A deck whose persist object 1 is a DocumentContainer of `doc` and whose
    /// objects 2 on are `objects`; unpersisted, its current edit is unusable.
    fn deck(
        doc: &[Vec<u8>],
        objects: &[Vec<u8>],
        pictures: OptionalStream,
        persisted: bool,
    ) -> Vec<u8> {
        let document = container(0, 0x03E8, doc);
        let mut stream = Vec::new();
        let mut offsets = Vec::new();
        for object in std::iter::once(&document).chain(objects) {
            offsets.push(u32::try_from(stream.len()).unwrap());
            stream.extend_from_slice(object);
        }
        let count = u32::try_from(offsets.len()).unwrap();
        let directory = u32::try_from(stream.len()).unwrap();
        stream.extend(atom32(0x1772, &[&[1 | count << 20][..], &offsets].concat()));
        let edit = u32::try_from(stream.len()).unwrap();
        stream.extend(atom32(0x0FF5, &[0, 0, 0, directory, 1, count + 1, 0]));
        let edit = if persisted { edit } else { 0 };
        let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        let user = atom32(0x0FF6, &[20, 0xE391_C05F, edit]);
        ole.create_stream("Current User").unwrap().write_all(&user).unwrap();
        ole.create_stream("PowerPoint Document").unwrap().write_all(&stream).unwrap();
        match pictures {
            OptionalStream::Read(bytes) => {
                ole.create_stream("Pictures").unwrap().write_all(&bytes).unwrap();
            }
            OptionalStream::Unreadable => ole.create_storage("Pictures").unwrap(),
            OptionalStream::Absent => {}
        }
        ole.into_inner().into_inner()
    }

    type Reported = (DiagnosticCode, Region, Option<u32>, u64);

    fn reported(bytes: &[u8]) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, bytes).unwrap();
        let diagnostics = parse_ctx.diagnostics();
        diagnostics.iter().map(|d| (d.code, d.region, d.unit.map(|u| u.get()), d.count)).collect()
    }

    #[test]
    fn each_loss_is_reported_in_its_slide() {
        use DiagnosticCode::*;
        use OptionalStream::{Absent, Read, Unreadable};
        use Region::{Body, Header, Note};
        let two_slides = pages(0, &[(2, 256), (3, 257)]);
        let one_slide = || pages(0, &[(2, 256)]);
        let pictured = |pictures: OptionalStream| {
            let shapes = drawing(&[picture(1), picture(2)]);
            deck(&[one_slide(), bstore(&[0, 12])], &[slide(0, &[shapes])], pictures, true)
        };
        let unpaired = || text(&[0x61, 0xD800, 0x0D]);
        let units = |label: &str| label.encode_utf16().collect::<Vec<u16>>();
        let hyperlinks = || {
            let links =
                [hyperlink(1, "https://example.org/a", ""), hyperlink(2, "", "256,1,Slide 1")];
            container(0, 0x0409, &links)
        };
        let linked_shapes = |shapes: &[Vec<u8>]| {
            deck(&[one_slide(), hyperlinks()], &[slide(0, &[drawing(shapes)])], Absent, true)
        };
        let linked = |id: u32, label: &str, range: Option<(u32, u32)>| {
            let textbox = [text(&units(label)), link(4, id, range)];
            linked_shapes(&[text_shape(0x0A00, &[], &[], &textbox)])
        };
        let clicked = |id: u32, label: &str| {
            let textbox = [text(&units(label))];
            linked_shapes(&[text_shape(0x0A00, &[], &[link(4, id, None)], &textbox)])
        };
        let filled = |flags: u32, drawing: Vec<u8>| {
            let doc = [one_slide(), bstore(&[0])];
            deck(&doc, &[slide(flags, &[drawing])], Read(png(true)), true)
        };
        let fill_shape = |props: &[(u16, u32)]| drawing(&[shape(0x0A00, props, &[])]);
        let picture_fill = [(0x0180, 3), (0x4186, 1)];
        let titled = |flags: u32, main: Vec<u8>, title: Vec<u8>| {
            let doc = [one_slide(), pages(1, &[(3, 1), (4, 2)])];
            deck(&doc, &[slide_on(2, flags, &[]), main, title], Absent, true)
        };
        let styled = |style: &[u8]| {
            let content = [text(&[0x61, 0x62, 0x0D]), record(0, 0x0FA1, style)].concat();
            deck(&[one_slide()], &[slide(0, &[content])], Absent, true)
        };
        let noted = |notes_list: Vec<u8>, owner: u32| {
            let objects = [slide(0, &[]), slide(0, &[]), notes(owner, &[unpaired()])];
            deck(&[two_slides.clone(), notes_list], &objects, Absent, true)
        };
        let mastered = |shapes: &[Vec<u8>], fill: u32, flags: u32| {
            let doc = [one_slide(), pages(1, &[(3, 1)])];
            deck(&doc, &[slide(flags, &[]), master(shapes, fill)], Absent, true)
        };
        let objects = |list: &[Vec<u8>], shapes: &[Vec<u8>]| {
            let doc = [one_slide(), container(0, 0x0409, list), bstore(&[0])];
            deck(&doc, &[slide(0, &[drawing(shapes)])], Read(png(true)), true)
        };
        let showing = |id: u32| shape(0x0A10, &[(0x4104, 1)], &[atom32(0x0BC1, &[id])]);
        let words = |words: &[u32]| words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
        let outlined = |index: u32| {
            let list = [atom32(0x03F3, &[2, 0, 0, 256, 0]), text(&units("see here\r"))];
            let list = [&list[..], &[text(&units("https://example.org/a\r"))]].concat();
            let reference = [atom32(0x0F9E, &[index])];
            let shape = text_shape(0x0A00, &[], &[link(4, 1, None)], &reference);
            let doc = [container(0, 0x0FF0, &list), hyperlinks()];
            deck(&doc, &[slide(0, &[drawing(&[shape])])], Absent, true)
        };
        let master_styled = |levels: &[u8]| {
            let style = record(1 << 4, 0x0FA3, &[&[2, 0][..], levels].concat());
            let master = container(0, 0x03F8, &[atom32(0x03EF, &[0; 6]), style]);
            deck(&[one_slide(), pages(1, &[(3, 1)])], &[slide(0, &[]), master], Absent, true)
        };
        let recovered_link = |slides: &[Vec<u8>]| {
            let clicked = text_shape(0x0A00, &[], &[link(4, 1, None)], &[text(&units("see\r"))]);
            let first = slide(0, &[drawing(&[clicked])]);
            deck(&[one_slide(), hyperlinks()], &[&[first][..], slides].concat(), Absent, false)
        };
        let cases: Vec<(&str, Vec<u8>, Vec<Reported>)> = vec![
            (
                "a picture two shapes of one slide place, and another slide's",
                deck(
                    &[two_slides.clone(), bstore(&[0])],
                    &[
                        slide(0, &[drawing(&[picture(1), picture(1)])]),
                        slide(0, &[drawing(&[picture(1)])]),
                    ],
                    Read(png(true)),
                    true,
                ),
                vec![(ImageUnplaced, Body, Some(1), 1), (ImageUnplaced, Body, Some(2), 1)],
            ),
            (
                "a picture whose record does not decode",
                pictured(Read([png(false), png(true)].concat())),
                vec![(ImageUnavailable, Body, None, 1), (ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "a Pictures stream that does not read",
                pictured(Unreadable),
                vec![(PartUnreadable, Body, None, 1)],
            ),
            ("no Pictures stream", pictured(Absent), vec![(ImageUnplaced, Body, Some(1), 2)]),
            (
                "an OLE object and its preview",
                objects(&[ole(7)], &[showing(7)]),
                vec![(EmbeddedObject, Body, Some(1), 1)],
            ),
            (
                "a linked OLE object and its preview",
                objects(&[ole_as(0x0FCE, 7)], &[showing(7)]),
                vec![(EmbeddedObject, Body, Some(1), 1)],
            ),
            (
                "an ActiveX control and its preview",
                objects(&[ole_as(0x0FEE, 7)], &[showing(7)]),
                vec![(FormControlUnread, Body, Some(1), 1)],
            ),
            (
                "a movie and its poster frame",
                objects(&[media(0x1007, 7)], &[showing(7)]),
                vec![(GraphicUnread, Body, Some(1), 1), (ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "an AVI movie and a sound",
                objects(&[media(0x1006, 7), media(0x1010, 8)], &[showing(7), showing(8)]),
                vec![(GraphicUnread, Body, Some(1), 2), (ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "a shape naming no listed object",
                objects(&[ole(7)], &[showing(9)]),
                vec![(ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "a picture in a tertiary property table",
                objects(&[], &[tertiary_picture(1)]),
                vec![(ImageUnplaced, Body, Some(1), 1)],
            ),
            ("a shape with no picture", objects(&[], &[picture(0)]), vec![]),
            (
                "a hyperlink whose label hides its target",
                linked(1, "see here\r", Some((0, 8))),
                vec![(LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "a hyperlink whose label shows its target",
                linked(1, "see https://example.org/a\r", Some((4, 25))),
                vec![],
            ),
            (
                "a hyperlink whose label shows its target without the scheme",
                linked(1, "example.org/a\r", Some((0, 13))),
                vec![],
            ),
            (
                "an unknown hyperlink under a label without a scheme",
                linked(5, "example.org/a\r", Some((0, 13))),
                vec![(LinkTargetUnread, Body, Some(1), 1)],
            ),
            ("a hyperlink to a slide", linked(2, "see here\r", Some((0, 8))), vec![]),
            (
                "a jump that follows no hyperlink",
                linked_shapes(&[text_shape(
                    0x0A00,
                    &[],
                    &[],
                    &[text(&[0x61, 0x0D]), link(3, 5, Some((0, 1)))],
                )]),
                vec![],
            ),
            (
                "an unknown hyperlink",
                linked(5, "see here\r", Some((0, 8))),
                vec![(LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "an unknown hyperlink under a URL",
                linked(5, "https://x.org/\r", Some((0, 14))),
                vec![],
            ),
            (
                "a text hyperlink with no text range",
                linked(1, "https://example.org/a\r", None),
                vec![(LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "a hyperlink in outline text whose label hides its target",
                deck(
                    &[
                        container(
                            0,
                            0x0FF0,
                            &[
                                atom32(0x03F3, &[2, 0, 0, 256, 0]),
                                text(&units("see here\r")),
                                link(4, 1, Some((0, 8))),
                            ],
                        ),
                        hyperlinks(),
                    ],
                    &[slide(0, &[])],
                    Absent,
                    true,
                ),
                vec![(LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "a text box whose whole text is its click link",
                clicked(1, "https://example.org/a\r"),
                vec![],
            ),
            (
                "a placeholder after a text box, its outline text its click link",
                deck(
                    &[
                        container(
                            0,
                            0x0FF0,
                            &[
                                atom32(0x03F3, &[2, 0, 0, 256, 0]),
                                text(&units("https://example.org/a\r")),
                            ],
                        ),
                        hyperlinks(),
                    ],
                    &[slide(
                        0,
                        &[drawing(&[
                            text_shape(0x0A00, &[], &[], &[text(&units("note\r"))]),
                            text_shape(0x0A00, &[], &[link(4, 1, None)], &[atom32(0x0F9E, &[0])]),
                        ])],
                    )],
                    Absent,
                    true,
                ),
                vec![],
            ),
            ("a placeholder whose outline text is its click link", outlined(1), vec![]),
            (
                "a placeholder whose outline text hides its click link",
                outlined(0),
                vec![(LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "a hyperlink range past the text's end",
                linked(1, "https://example.org/a", Some((0, 22))),
                vec![],
            ),
            (
                "a recovered slide's click link",
                recovered_link(&[]),
                vec![(RecoveredStreamOrder, Body, None, 1), (LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "a recovered slide's click link, before another slide's shape",
                recovered_link(&[slide(0, &[drawing(&[shape(0x0A00, &[], &[])])])]),
                vec![(RecoveredStreamOrder, Body, None, 1), (LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "a text box whose click link its text hides",
                clicked(1, "see here\r"),
                vec![(LinkTargetUnread, Body, Some(1), 1)],
            ),
            ("a text box whose click link jumps to a slide", clicked(2, "see here\r"), vec![]),
            (
                "a picture with a click link and no text",
                linked_shapes(&[shape(0x0A00, &[(0x4104, 1)], &[link(4, 1, None)])]),
                vec![(ImageUnplaced, Body, Some(1), 1), (LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "a click link on a shape after a text box showing its URL",
                linked_shapes(&[
                    text_shape(0x0A00, &[], &[], &[text(&units("https://example.org/a\r"))]),
                    shape(0x0A00, &[], &[link(4, 1, None)]),
                ]),
                vec![(LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "a click link on a shape before a text box showing its URL",
                linked_shapes(&[
                    shape(0x0A00, &[], &[link(4, 1, None)]),
                    text_shape(0x0A00, &[], &[], &[text(&units("https://example.org/a\r"))]),
                ]),
                vec![(LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "a shape filled with a picture",
                filled(0, fill_shape(&picture_fill)),
                vec![(ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "a shape filled with a texture",
                filled(0, fill_shape(&[(0x0180, 2), (0x4186, 1)])),
                vec![(ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "a shape filled with a pattern",
                filled(0, fill_shape(&[(0x0180, 1), (0x4186, 1)])),
                vec![],
            ),
            (
                "a picture fill naming no picture",
                filled(0, fill_shape(&[(0x0180, 3), (0x4186, 0)])),
                vec![],
            ),
            (
                "a picture fill switched off",
                filled(0, fill_shape(&[(0x0180, 3), (0x4186, 1), (0x01BF, 1 << 20)])),
                vec![],
            ),
            (
                "a picture fill switched on",
                filled(0, fill_shape(&[(0x0180, 3), (0x4186, 1), (0x01BF, 1 << 20 | 1 << 4)])),
                vec![(ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "a picture fill whose fFilled is unset",
                filled(0, fill_shape(&[(0x0180, 3), (0x4186, 1), (0x01BF, 0)])),
                vec![(ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "a notes page's picture background",
                deck(
                    &[one_slide(), pages(2, &[(3, 300)]), bstore(&[0])],
                    &[
                        slide(0, &[]),
                        notes(256, &[page_drawing(container(0, 0xF003, &[]), &picture_fill)]),
                    ],
                    Read(png(true)),
                    true,
                ),
                vec![],
            ),
            (
                "a slide's own picture background",
                filled(0, page_drawing(container(0, 0xF003, &[]), &picture_fill)),
                vec![(ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "a slide's picture background hidden by its master's",
                filled(4, page_drawing(container(0, 0xF003, &[]), &picture_fill)),
                vec![],
            ),
            (
                "text with an unpaired surrogate",
                deck(&[one_slide()], &[slide(0, &[unpaired()])], Absent, true),
                vec![(InvalidEncoding, Body, Some(1), 1)],
            ),
            (
                "an unparseable paragraph run",
                styled(&[&words(&[4])[..], &[0, 0], &words(&[1])].concat()),
                vec![(FormattingUnreadable, Body, Some(1), 1)],
            ),
            (
                "an unparseable character run",
                styled(&[&words(&[4])[..], &[0, 0], &words(&[0, 4, 1])].concat()),
                vec![(FormattingUnreadable, Body, Some(1), 1)],
            ),
            (
                "a zero-count paragraph run before the text's end",
                styled(&[&words(&[0])[..], &[0, 0], &words(&[0, 4, 0])].concat()),
                vec![(FormattingUnreadable, Body, Some(1), 1)],
            ),
            (
                "a zero-count character run before the text's end",
                styled(&[&words(&[4])[..], &[0, 0], &words(&[0, 0, 0])].concat()),
                vec![(FormattingUnreadable, Body, Some(1), 1)],
            ),
            (
                "a paragraph run declaring more tab stops than it holds",
                styled(
                    &[
                        &words(&[4])[..],
                        &[0, 0],
                        &words(&[1 << 20]),
                        &[0xFF, 0xFF],
                        &words(&[0, 0]),
                    ]
                    .concat(),
                ),
                vec![(FormattingUnreadable, Body, Some(1), 1)],
            ),
            (
                "master styles whose second level is cut",
                master_styled(&words(&[0, 0, 0])),
                vec![(FormattingUnreadable, Body, None, 1)],
            ),
            ("master styles whose levels parse", master_styled(&words(&[0, 0, 0, 0])), vec![]),
            (
                "runs that cover the text",
                styled(&[&words(&[4])[..], &[0, 0], &words(&[0, 4, 0])].concat()),
                vec![],
            ),
            (
                "a paragraph run cut in its header",
                styled(&[&words(&[4])[..], &[0]].concat()),
                vec![(FormattingUnreadable, Body, Some(1), 1)],
            ),
            (
                "paragraph runs and no character run",
                styled(&[&words(&[4])[..], &[0, 0], &words(&[0])].concat()),
                vec![(FormattingUnreadable, Body, Some(1), 1)],
            ),
            (
                "a character run cut in its count",
                styled(&[&words(&[4])[..], &[0, 0], &words(&[0]), &[4, 0]].concat()),
                vec![(FormattingUnreadable, Body, Some(1), 1)],
            ),
            (
                "a listed slide that does not resolve, before one that does",
                deck(&[pages(0, &[(9, 256), (2, 257)])], &[slide(0, &[unpaired()])], Absent, true),
                vec![(UnitUnreadable, Body, Some(1), 1), (InvalidEncoding, Body, Some(2), 1)],
            ),
            (
                "notes of the second slide",
                noted(pages(2, &[(4, 300)]), 257),
                vec![(InvalidEncoding, Note, Some(2), 1)],
            ),
            (
                "notes of no slide",
                noted(pages(2, &[(4, 300)]), 999),
                vec![(InvalidEncoding, Note, None, 1)],
            ),
            (
                "notes that do not resolve",
                noted(pages(2, &[(9, 300)]), 257),
                vec![(UnitUnreadable, Note, None, 1)],
            ),
            (
                "a master logo the slides show",
                mastered(&[picture(1)], 0, 1),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            ("a master logo no slide shows", mastered(&[picture(1)], 0, 0), vec![]),
            ("a master of placeholders", mastered(&[placeholder()], 0, 1), vec![]),
            (
                "a master group the slides show",
                mastered(&[container(0, 0xF003, &[])], 0, 1),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "a master picture background",
                mastered(&[], 3, 4),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "a master texture background",
                mastered(&[], 2, 4),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            ("a master picture background the slide overrides", mastered(&[], 3, 1), vec![]),
            (
                "a master picture background switched off",
                titled(
                    4,
                    title_master(0, 0, &[], &[(0x0180, 3), (0x4186, 1), (0x01BF, 1 << 20)]),
                    title_master(1, 4, &[], &[]),
                ),
                vec![],
            ),
            (
                "a master picture background naming no picture",
                titled(4, title_master(0, 0, &[], &[(0x0180, 3)]), title_master(1, 4, &[], &[])),
                vec![],
            ),
            (
                "a title master's logo the slide shows",
                titled(1, master(&[placeholder()], 0), title_master(1, 0, &[picture(1)], &[])),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "a title master's logo the slide hides",
                titled(0, master(&[picture(1)], 0), title_master(1, 1, &[picture(1)], &[])),
                vec![],
            ),
            (
                "a main master's logo its title master shows",
                titled(1, master(&[picture(1)], 0), title_master(1, 1, &[placeholder()], &[])),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "a main master's logo its title master hides",
                titled(1, master(&[picture(1)], 0), title_master(1, 0, &[placeholder()], &[])),
                vec![],
            ),
            (
                "a main master's picture background its title master shows",
                titled(4, master(&[], 3), title_master(1, 4, &[], &[])),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "a title master's own picture background",
                titled(4, master(&[], 0), title_master(1, 0, &[], &picture_fill)),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "a title master's picture background under its main master's",
                titled(4, master(&[], 0), title_master(1, 4, &[], &picture_fill)),
                vec![],
            ),
            (
                "a master logo on a master no slide follows",
                deck(
                    &[one_slide(), pages(1, &[(3, 1), (4, 2)])],
                    &[slide_on(1, 5, &[]), master(&[placeholder()], 3), master(&[picture(1)], 3)],
                    Absent,
                    true,
                ),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "a master logo on the first master, for a slide naming none",
                deck(
                    &[one_slide(), pages(1, &[(3, 1), (4, 2)])],
                    &[slide(1, &[]), master(&[picture(1)], 0), master(&[placeholder()], 0)],
                    Absent,
                    true,
                ),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "an undecodable picture only a master shows",
                deck(
                    &[one_slide(), pages(1, &[(3, 1)]), bstore(&[0])],
                    &[slide(1, &[]), master(&[picture(1)], 0)],
                    Read(png(false)),
                    true,
                ),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "a deck recovered in stream order",
                deck(
                    &[one_slide(), referenced_bstore(&[(1, 0), (1, 12), (0, 12), (2, 12)])],
                    &[
                        master(&[picture(1)], 0),
                        master(&[picture(1)], 0),
                        slide(1, &[drawing(&[picture(1), picture(2), picture(3)])]),
                        slide(0, &[unpaired()]),
                        notes(256, &[unpaired()]),
                    ],
                    Read([png(false), png(true)].concat()),
                    false,
                ),
                vec![
                    (RecoveredStreamOrder, Body, None, 1),
                    (ImageUnavailable, Body, None, 1),
                    (ImageUnplaced, Body, None, 2),
                    (HeaderOmitted, Header, None, 2),
                    (InvalidEncoding, Body, Some(2), 1),
                    (InvalidEncoding, Note, None, 1),
                ],
            ),
        ];
        for (case, bytes, mut expected) in cases {
            expected.sort();
            assert_eq!(reported(&bytes), expected, "{case}");
        }
    }

    #[test]
    fn reporting_masters_charges_each_master_once() {
        let report = |parse_ctx: &ParseCtx| {
            let mut ex = Extractor::new(parse_ctx);
            ex.master_facts = (0..3).map(|id| (id, MasterFacts::default())).collect();
            ex.report_masters()
        };
        let parse_ctx = ParseCtx::default();
        report(&parse_ctx).unwrap();
        assert_eq!(parse_ctx.fuel(), 3);
        let limits = Limits { max_work: 2, ..Limits::default() };
        assert_matches!(
            report(&ParseCtx::new(&limits)),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 2 })
        );
    }

    #[test]
    fn title_masters_carry_facts_but_no_styles() {
        let doc = [pages(0, &[(2, 256)]), pages(1, &[(3, 1), (4, 2)])];
        let objects = [slide_on(2, 1, &[]), master(&[], 0), title_master(1, 1, &[], &[])];
        let bytes = deck(&doc, &objects, OptionalStream::Absent, true);
        let mut ole = cfb::CompoundFile::open(Cursor::new(bytes)).unwrap();
        let parse_ctx = ParseCtx::default();
        let data = read_ole_stream(&parse_ctx, &mut ole, "PowerPoint Document").unwrap();
        let user = read_ole_stream(&parse_ctx, &mut ole, "Current User").unwrap();
        let mut ex = Extractor::new(&parse_ctx);
        assert!(ex.parse_slides(&data, &user).unwrap());
        assert_eq!(ex.masters.iter().map(|&(id, _)| id).collect::<Vec<_>>(), [1]);
        let facts: Vec<_> =
            ex.master_facts.iter().map(|(id, f)| (*id, f.follows.is_some())).collect();
        assert_eq!(facts, [(1, false), (2, true)]);
    }

    #[test]
    fn every_record_the_document_and_master_reads_scan_is_charged_once() {
        let list = container(0, 0x0409, &[hyperlink(1, "a", ""), ole(2)]);
        let document = [list, bstore(&[0])].concat();
        let master = master(&[picture(1), placeholder()], 3)[8..].to_vec();
        type Read = fn(&mut Extractor, &[u8]) -> Result<(), ConvertError>;
        let reads: [(&[u8], u64, Read); 2] = [
            (&document, 12, |ex, data| ex.read_document(data)),
            (&master, 21, |ex, data| ex.read_master(1, data)),
        ];
        for (data, records, read) in reads {
            let parse_ctx = ParseCtx::default();
            read(&mut Extractor::new(&parse_ctx), data).unwrap();
            assert_eq!(parse_ctx.fuel(), records);
            let limits = Limits { max_work: records - 1, ..Limits::default() };
            let parse_ctx = ParseCtx::new(&limits);
            assert_matches!(
                read(&mut Extractor::new(&parse_ctx), data),
                Err(ConvertError::ResourceLimit { limit: Limit::Work, max }) if max == records - 1
            );
        }
    }

    #[test]
    fn a_recovered_deck_charges_the_scan_for_its_document_once() {
        let bytes = deck(&[pages(0, &[(2, 256)])], &[slide(0, &[])], OptionalStream::Absent, false);
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, &bytes).unwrap();
        let fuel = parse_ctx.fuel();
        assert_eq!(fuel, 4 + 1 + 7);
        let limits = Limits { max_work: fuel - 1, ..Limits::default() };
        assert_matches!(
            parse(&ParseCtx::new(&limits), &bytes),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max }) if max == fuel - 1
        );
    }

    #[test]
    fn each_hyperlink_charges_its_label_and_target() {
        let label: Vec<u16> = "see here\r".encode_utf16().collect();
        let links: Vec<Vec<u8>> = (0..3).map(|_| link(4, 1, Some((0, 8)))).collect();
        let content = [&[text(&label)][..], &links].concat().concat();
        let walk = |parse_ctx: &ParseCtx| {
            let mut ex = Extractor::new(parse_ctx);
            ex.links.insert(1, "https://example.org/a".into());
            ex.walk(&content)
        };
        let parse_ctx = ParseCtx::default();
        walk(&parse_ctx).unwrap();
        let records = 2 + 3 * 3;
        assert_eq!(parse_ctx.fuel(), records + 3 * (8 + 8 + 21));
        let max = parse_ctx.fuel() - 1;
        let limits = Limits { max_work: max, ..Limits::default() };
        assert_matches!(
            walk(&ParseCtx::new(&limits)),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max: m }) if m == max
        );
    }
}
