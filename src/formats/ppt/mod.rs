mod fonts;
mod hidden;
mod records;
mod side;
mod styletext;

use crate::ctx::{AdmittedUnit, ImageLoss, ParseCtx, UnitScope};
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::HeaderFooterRegion as Side;
use crate::model::{
    Asset, Block, Document, Inline, Note, NoteKind, Style, UnitKind, charge_blocks_copy,
    inlines_are_empty,
};
use crate::shared::binary::{
    OptionalStream, decode_utf16, get_u16, get_u32, optional_ole_stream, read_ole_stream,
    utf16le_units,
};
use crate::shared::delta::{StyleDelta, rebase_emphasis};
use crate::shared::link_label::{label_is_absolute_url, label_shows_target};
use crate::shared::list::{ListEntry, ListKey, ListMark, MarkerKind, flush_list};
use crate::shared::officeart::{self, Record, record_at};
use crate::shared::sides::{PageSides, SideTimeline};
use crate::shared::typeface::{Route, Slot, Slots, run_inlines};
use fonts::FontCollection;
use records as rt;
use side::{Defaults, Kind, PageFacts, Settings};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Cursor;
use styletext::{MasterLevel, StyleCursor, StyleRuns, TextType};

fn notes_owner(notes: &[u8]) -> Option<u32> {
    let atom = officeart::records(notes).find(|r| r.rec_type == rt::NOTES_ATOM)?;
    get_u32(atom.body, 0).filter(|&id| id != 0)
}

type MasterStyles = HashMap<TextType, Vec<MasterLevel>>;

pub(super) fn parse(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Document, ConvertError> {
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
        parse_ctx.emit(DiagnosticCode::RecoveredStreamOrder, Region::Body);
        ex = Extractor::new(parse_ctx);
        ex.recovering = true;
        let mut documents =
            scan(parse_ctx, &data)?.into_iter().filter(|r| r.rec_type == rt::DOCUMENT);
        if let Some(document) = documents.next_back() {
            ex.read_document(document.body)?;
        }
        ex.walk(&data)?;
        ex.end_segment(None)?;
    }
    if ex.encrypted {
        return Err(ConvertError::Encrypted);
    }
    let pictures = collect_pictures(parse_ctx, &mut ole)?;
    ex.report_pictures(&pictures);
    if !parse_ctx.headers_footers_requested() || ex.recovering {
        ex.report_masters()?;
    }
    let notes = std::mem::take(&mut ex.comments);
    Ok(Document {
        blocks: ex.into_blocks()?,
        notes,
        assets: pictures.assets,
        ..Document::default()
    })
}

#[derive(Debug, Default)]
struct Pictures {
    assets: Vec<Asset>,
    undecodable: HashSet<usize>,
    unreadable: bool,
}

fn collect_pictures<R: std::io::Read + std::io::Seek>(
    parse_ctx: &ParseCtx<'_>,
    ole: &mut cfb::CompoundFile<R>,
) -> Result<Pictures, ConvertError> {
    let pictures = match optional_ole_stream(parse_ctx, ole, "Pictures")? {
        OptionalStream::Read(pictures) => pictures,
        OptionalStream::Absent => return Ok(Pictures::default()),
        OptionalStream::Unreadable => {
            return Ok(Pictures { unreadable: true, ..Pictures::default() });
        }
    };
    let mut sink = crate::shared::assets::AssetSink::new(parse_ctx);
    let mut undecodable = HashSet::new();
    let cap = parse_ctx.limits().max_entry_bytes;
    let mut pos = 0usize;
    let mut index = 0u64;
    while let Some(record) = record_at(&pictures, pos) {
        let offset = pos;
        pos += 8 + record.body.len();
        count_record(parse_ctx, &mut index)?;
        let blip = match record.rec_type {
            officeart::FBSE => officeart::fbse_blip(record.body, cap),
            _ => officeart::decode_blip(record, cap),
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

fn count_record(parse_ctx: &ParseCtx<'_>, seen: &mut u64) -> Result<(), ConvertError> {
    parse_ctx.charge(1)?;
    *seen += 1;
    let max = parse_ctx.limits().max_records;
    if *seen > max {
        return Err(ConvertError::limit(Limit::Records, max));
    }
    Ok(())
}

fn listed_elsewhere(record: Record<'_>) -> bool {
    match record.rec_type {
        rt::NOTES | rt::MAIN_MASTER | rt::HANDOUT => true,
        rt::SLIDE_LIST_WITH_TEXT => record.instance() != rt::SLIDE_LIST_INSTANCE,
        _ => false,
    }
}

fn check_record_depth(parse_ctx: &ParseCtx<'_>, depth: usize) -> Result<(), ConvertError> {
    let max = parse_ctx.limits().max_record_depth;
    if depth as u64 >= max {
        return Err(ConvertError::limit(Limit::RecordDepth, max));
    }
    Ok(())
}

fn scan<'a>(parse_ctx: &ParseCtx<'_>, data: &'a [u8]) -> Result<Vec<Record<'a>>, ConvertError> {
    officeart::records(data).map(|record| parse_ctx.charge(1).map(|()| record)).collect()
}

fn children<'a>(
    parse_ctx: &ParseCtx<'_>,
    data: &'a [u8],
    rec_type: u16,
) -> Result<impl Iterator<Item = &'a [u8]> + use<'a>, ConvertError> {
    let found = scan(parse_ctx, data)?.into_iter().filter(move |r| r.rec_type == rec_type);
    Ok(found.map(|r| r.body))
}

const FSP_PATRIARCH: u32 = 0x0004;
const FSP_BACKGROUND: u32 = 0x0400;

#[derive(Default)]
struct MasterFacts {
    objects: bool,
    content: Option<MasterContent>,
    picture_background: bool,
    shown: MasterUse,
    follows: Option<(u32, MasterUse)>,
}

impl MasterFacts {
    fn read_group(&mut self, parse_ctx: &ParseCtx<'_>, group: &[u8]) -> Result<(), ConvertError> {
        for Record { rec_type, body, .. } in scan(parse_ctx, group)? {
            match rec_type {
                officeart::SPGR_CONTAINER => {
                    for Record { rec_type, body: child, .. } in scan(parse_ctx, body)? {
                        self.objects |= match rec_type {
                            officeart::SPGR_CONTAINER => true,
                            officeart::SP_CONTAINER => master_shape(parse_ctx, child)?.is_object(),
                            _ => false,
                        };
                    }
                }
                officeart::SP_CONTAINER => {
                    self.picture_background |= master_shape(parse_ctx, body)?.has_picture_fill();
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn omits(&self, shown: MasterUse) -> bool {
        let own_background = !self.follows.is_some_and(|(_, own)| own.background);
        self.objects && shown.objects
            || self.picture_background && own_background && shown.background
    }
}

#[derive(Default, Clone, Copy)]
struct MasterUse {
    objects: bool,
    background: bool,
}

impl MasterUse {
    fn from_flags(flags: u16) -> Self {
        Self { objects: flags & 0x0001 != 0, background: flags & 0x0004 != 0 }
    }

    fn or(self, other: Self) -> Self {
        Self {
            objects: self.objects || other.objects,
            background: self.background || other.background,
        }
    }

    fn and(self, other: Self) -> Self {
        Self {
            objects: self.objects && other.objects,
            background: self.background && other.background,
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Fill {
    kind: Option<u32>,
    blip: Option<u32>,
    unfilled: bool,
}

impl Fill {
    fn read(&mut self, (id, value): (u16, u32)) {
        match id {
            officeart::FILL_TYPE => self.kind = Some(value),
            officeart::FILL_BLIP => self.blip = Some(value),
            officeart::FILL_STYLE_BOOLEAN_PROPERTIES => {
                self.unfilled = value & (1 << 20) != 0 && value & (1 << 4) == 0;
            }
            _ => {}
        }
    }

    fn picture(&self) -> Option<u32> {
        let shown = matches!(self.kind, Some(2 | 3)) && !self.unfilled;
        self.blip.filter(|&pib| shown && pib != 0)
    }
}

#[derive(Clone, Copy)]
struct StoreEntry {
    refs: u32,
    offset: Option<usize>,
}

impl StoreEntry {
    fn read(Record { rec_type, body, .. }: Record<'_>) -> Self {
        let fbse = (rec_type == officeart::FBSE).then_some(body);
        Self {
            refs: fbse.and_then(|b| get_u32(b, 24)).unwrap_or(0),
            offset: fbse.and_then(|b| get_u32(b, 28)?.try_into().ok()),
        }
    }
}

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

struct MasterContent {
    blocks: Vec<Block>,
    held: crate::diagnostics::Diagnostics,
    pictures: u64,
}

fn master_shape(parse_ctx: &ParseCtx<'_>, container: &[u8]) -> Result<MasterShape, ConvertError> {
    let mut shape = MasterShape::default();
    for record in scan(parse_ctx, container)? {
        let Record { rec_type, body, .. } = record;
        match rec_type {
            officeart::FSP => shape.flags = get_u32(body, 4).unwrap_or(0),
            officeart::FOPT | officeart::TERTIARY_FOPT => {
                officeart::properties(record.instance(), body)
                    .for_each(|prop| shape.fill.read(prop));
            }
            officeart::CLIENT_DATA => {
                shape.placeholder |=
                    children(parse_ctx, body, rt::PLACEHOLDER_ATOM)?.next().is_some();
            }
            _ => {}
        }
    }
    Ok(shape)
}

#[derive(Default)]
struct ShapeFacts {
    pib: Option<u32>,
    preview: bool,
    fill: Fill,
    background: bool,
    links: Vec<u32>,
    text: bool,
    outline: Option<usize>,
    side: Option<Kind>,
    placeholder: bool,
}

struct Page<'a> {
    container: Option<&'a [u8]>,
    id: Option<u32>,
    omitted: bool,
}

struct PendingShape {
    tx_type: TextType,
    side: Option<Kind>,
    master_placeholder: bool,
    text: String,
    units: Vec<u16>,
    styles: Option<StyleRuns>,
    omitted: bool,
}

impl PendingShape {
    fn new(tx_type: TextType, (side, master_placeholder): (Option<Kind>, bool)) -> Self {
        let (text, units, styles) = (String::new(), Vec::new(), None);
        Self { tx_type, side, master_placeholder, text, units, styles, omitted: false }
    }
}

type Segment = (Vec<Block>, Option<u32>, bool);

struct Extractor<'c> {
    parse_ctx: &'c ParseCtx<'c>,
    segments: Vec<Segment>,
    current: Vec<Block>,
    current_is_notes: bool,
    comments: Vec<Note>,
    omitted_slides: HashSet<u32>,
    omit_page: bool,
    list_run: Vec<ListEntry>,
    pending: Option<PendingShape>,
    masters: Vec<(u32, MasterStyles)>,
    active_master: usize,
    shape_counter: u64,
    encrypted: bool,
    /// Raw-stream recovery: no persist lists, so notes descend inline.
    recovering: bool,
    records: u64,
    links: HashMap<u32, String>,
    objects: HashMap<u32, DiagnosticCode>,
    bstore: Vec<StoreEntry>,
    shape: Option<ShapeFacts>,
    link: Option<u32>,
    unit: Option<AdmittedUnit>,
    placed: BTreeSet<(Option<AdmittedUnit>, Region, u32)>,
    master_facts: Vec<(u32, MasterFacts)>,
    own_background: bool,
    outline: Vec<String>,
    hidden_outline: BTreeSet<usize>,
    reading_outline: bool,
    recovered_use: MasterUse,
    recovered_slides: usize,
    fonts: FontCollection,
    defaults: Defaults,
    page: PageFacts,
    slide_sides: SideTimeline,
    slide_master: Option<(Option<u32>, MasterUse)>,
    master: Option<u64>,
}

struct DocLayout<'a> {
    persist: HashMap<u32, usize>,
    document: &'a [u8],
    slide_list: &'a [u8],
    notes_list: Option<&'a [u8]>,
    master_list: Option<&'a [u8]>,
}

fn locate_document<'a>(data: &'a [u8], current_user: &[u8]) -> Option<DocLayout<'a>> {
    let mut persist: HashMap<u32, usize> = HashMap::new();
    let mut doc_persist: Option<u32> = None;
    let mut edit_off = get_u32(current_user, 16)? as usize;
    for _ in 0..100 {
        if edit_off == 0 {
            break;
        }
        let Record { rec_type, body, .. } = record_at(data, edit_off)?;
        if rec_type != rt::USER_EDIT_ATOM {
            return None;
        }
        if doc_persist.is_none() {
            doc_persist = get_u32(body, 16);
        }
        let dir_off = get_u32(body, 12)? as usize;
        if let Some(Record { rec_type: rt::PERSIST_DIRECTORY_ATOM, body: dir, .. }) =
            record_at(data, dir_off)
        {
            let mut pos = 0;
            while pos + 4 <= dir.len() {
                let head = get_u32(dir, pos)?;
                let id = head & 0xF_FFFF;
                let count = head >> 20;
                pos += 4;
                for k in 0..count {
                    persist.entry(id + k).or_insert(get_u32(dir, pos)? as usize);
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
    let Some(Record { rec_type: rt::DOCUMENT, body: doc, .. }) = record_at(data, doc_off) else {
        return None;
    };
    let list = |instance: u16| {
        officeart::records(doc)
            .find(|r| r.rec_type == rt::SLIDE_LIST_WITH_TEXT && r.instance() == instance)
            .map(|r| r.body)
    };
    let slide_list = list(rt::SLIDE_LIST_INSTANCE)?;
    let notes_list = list(rt::NOTES_LIST_INSTANCE);
    let master_list = list(rt::MASTER_LIST_INSTANCE);
    Some(DocLayout { persist, document: doc, slide_list, notes_list, master_list })
}

fn master_styles(parse_ctx: &ParseCtx<'_>, master: &[u8]) -> MasterStyles {
    let mut styles = MasterStyles::new();
    for record in officeart::records(master) {
        let text_type = TextType(record.instance());
        if record.rec_type != rt::TEXT_MASTER_STYLE_ATOM || styles.contains_key(&text_type) {
            continue;
        }
        let (levels, complete) = styletext::parse_master_style(record.body, text_type);
        if !complete {
            parse_ctx.emit(DiagnosticCode::FormattingUnreadable, Region::Body);
        }
        styles.insert(text_type, levels);
    }
    styles
}

/// Main masters and title masters (`Slide` containers) in master-list order
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
        for Record { rec_type, body, .. } in officeart::records(list) {
            if rec_type != rt::SLIDE_PERSIST_ATOM {
                continue;
            }
            let (Some(persist_ref), Some(master_id)) = (get_u32(body, 0), get_u32(body, 12)) else {
                continue;
            };
            if let Some(&off) = persist.get(&persist_ref)
                && let Some(Record {
                    rec_type: rec_type @ (rt::MAIN_MASTER | rt::SLIDE),
                    body: master,
                    ..
                }) = record_at(data, off)
            {
                out.push((master_id, rec_type, master));
            }
        }
    }
    if out.is_empty() {
        #[expect(
            clippy::disallowed_methods,
            reason = "the offsets are collected into a sorted set"
        )]
        let offs: BTreeSet<usize> = persist.values().copied().collect();
        for off in offs {
            if let Some(Record { rec_type: rt::MAIN_MASTER, body: master, .. }) =
                record_at(data, off)
            {
                out.push((0, rt::MAIN_MASTER, master));
            }
        }
    }
    out
}

impl<'c> Extractor<'c> {
    fn new(parse_ctx: &'c ParseCtx<'c>) -> Self {
        Self {
            parse_ctx,
            segments: Vec::new(),
            current: Vec::new(),
            current_is_notes: false,
            comments: Vec::new(),
            omitted_slides: HashSet::new(),
            omit_page: false,
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
            hidden_outline: BTreeSet::new(),
            reading_outline: false,
            recovered_use: MasterUse::default(),
            recovered_slides: 0,
            fonts: FontCollection::default(),
            defaults: Defaults::default(),
            page: PageFacts::default(),
            slide_sides: SideTimeline::default(),
            slide_master: None,
            master: None,
        }
    }

    fn region(&self) -> Region {
        if self.master.is_some() {
            Region::Header
        } else if self.current_is_notes {
            Region::Note
        } else {
            Region::Body
        }
    }

    fn parse_slides(&mut self, data: &[u8], current_user: &[u8]) -> Result<bool, ConvertError> {
        let Some(layout) = locate_document(data, current_user) else {
            return Ok(false);
        };
        self.read_document(layout.document)?;
        let masters = collect_masters(layout.master_list, &layout.persist, data);
        for &(id, _, master) in &masters {
            self.read_master(id, master)?;
        }
        let main = masters.iter().filter(|&&(_, rec_type, _)| rec_type == rt::MAIN_MASTER);
        let parse_ctx = self.parse_ctx;
        self.masters = main.map(|&(id, _, m)| (id, master_styles(parse_ctx, m))).collect();
        if parse_ctx.headers_footers_requested() {
            for &(id, _, master) in &masters {
                self.walk_master(id, master)?;
            }
        }
        let units =
            self.walk_slide_list(layout.slide_list, &layout.persist, data, false, &HashMap::new())?;
        if let Some(notes_list) = layout.notes_list {
            self.walk_slide_list(notes_list, &layout.persist, data, true, &units)?;
        }
        Ok(true)
    }

    fn walk_slide_list(
        &mut self,
        list: &[u8],
        persist: &HashMap<u32, usize>,
        data: &[u8],
        is_notes: bool,
        slide_units: &HashMap<u32, AdmittedUnit>,
    ) -> Result<HashMap<u32, AdmittedUnit>, ConvertError> {
        let parse_ctx = self.parse_ctx;
        let container_type = if is_notes { rt::NOTES } else { rt::SLIDE };
        let mut units = HashMap::new();
        let mut position = 0usize;
        let mut scope = None;
        let mut page: Option<Page<'_>> = None;
        for record in officeart::records(list) {
            let body = record.body;
            match record.rec_type {
                rt::SLIDE_PERSIST_ATOM => {
                    self.finish_slide(page.take())?;
                    drop(scope.take());
                    self.outline.clear();
                    self.reading_outline = true;
                    self.current_is_notes = is_notes;
                    let container = get_u32(body, 0)
                        .and_then(|p| persist.get(&p))
                        .and_then(|&off| record_at(data, off))
                        .filter(|r| r.rec_type == container_type)
                        .map(|r| r.body);
                    let id = if is_notes {
                        container.and_then(notes_owner)
                    } else {
                        get_u32(body, 12).filter(|&id| id != 0)
                    };
                    scope = if is_notes {
                        let owner = id.and_then(|id| slide_units.get(&id).copied());
                        owner.map(|unit| parse_ctx.enter_admitted(unit))
                    } else {
                        let slide = parse_ctx.begin_unit(UnitKind::Slide, position)?;
                        position = position.saturating_add(1);
                        if let Some(id) = id {
                            units.entry(id).or_insert(slide.unit());
                        }
                        Some(slide)
                    };
                    self.unit = scope.as_ref().map(UnitScope::unit);
                    self.page_visibility(container, id, is_notes)?;
                    self.own_background = false;
                    if !is_notes {
                        self.select_master(container);
                    }
                    page = Some(Page { container, id, omitted: self.omit_page });
                }
                _ if self.omit_page => {}
                _ => self.record(record)?,
            }
        }
        self.finish_slide(page)?;
        drop(scope);
        self.unit = None;
        Ok(units)
    }

    fn page_visibility(
        &mut self,
        container: Option<&[u8]>,
        id: Option<u32>,
        is_notes: bool,
    ) -> Result<(), ConvertError> {
        self.omit_page = if is_notes {
            id.is_some_and(|id| self.omitted_slides.contains(&id))
        } else {
            !self.parse_ctx.hidden_requested()
                && match container {
                    Some(container) => hidden::slide(self.parse_ctx, container)?,
                    None => false,
                }
        };
        if self.omit_page && !is_notes {
            self.parse_ctx.emit(DiagnosticCode::HiddenOmitted, Region::Body);
            if let Some(id) = id {
                self.omitted_slides.insert(id);
            }
        }
        self.hidden_outline.clear();
        if !self.omit_page
            && !self.parse_ctx.hidden_requested()
            && let Some(container) = container
        {
            self.hidden_outline = hidden::outline(self.parse_ctx, container)?;
        }
        Ok(())
    }

    fn finish_slide(&mut self, page: Option<Page<'_>>) -> Result<(), ConvertError> {
        self.reading_outline = false;
        let Some(page) = page else {
            return self.end_segment(None);
        };
        if page.omitted {
            return Ok(());
        }
        match page.container {
            Some(container) => self.walk(container)?,
            None => self.parse_ctx.emit(DiagnosticCode::UnitUnreadable, self.region()),
        }
        self.close_shape()?;
        if page.container.is_some() {
            self.settle_page()?;
        }
        if !self.current_is_notes
            && (page.container.is_some() || !self.current.is_empty())
            && let Some(unit) = self.unit
        {
            self.current.insert(0, Block::UnitStart(unit.model()));
        }
        self.end_segment(page.id)
    }

    fn settle_page(&mut self) -> Result<(), ConvertError> {
        let notes = self.current_is_notes;
        let default = if notes { &self.defaults.notes } else { &self.defaults.slides };
        let mut page = std::mem::take(&mut self.page);
        if !self.parse_ctx.headers_footers_requested() {
            page.report(self.parse_ctx, default, notes);
            return Ok(());
        }
        let mut sides = page.sides(self.parse_ctx, default, notes)?;
        if !notes {
            self.master_header(&mut sides)?;
        }
        let mut blocks = Vec::new();
        let mut notes_timeline = SideTimeline::default();
        let timeline = if notes { &mut notes_timeline } else { &mut self.slide_sides };
        sides.publish(self.parse_ctx, timeline, &mut blocks)?;
        self.current.splice(0..0, blocks);
        let comments = u64::from(page.comments);
        self.parse_ctx.emit_count(DiagnosticCode::CommentOmitted, Region::Comment, comments);
        Ok(())
    }

    fn read_tag(&mut self, body: &[u8]) -> Result<(), ConvertError> {
        if self.parse_ctx.comments_requested() {
            self.read_comments(body)?;
        } else if side::tag_has_comment(self.parse_ctx, body)? {
            self.page.comments = true;
        }
        Ok(())
    }

    fn read_comments(&mut self, tag: &[u8]) -> Result<(), ConvertError> {
        const RECORD_HEADER_LEN: usize = 8;
        let Some(blob) = side::comment_blob(self.parse_ctx, tag)? else { return Ok(()) };
        self.close_shape()?;
        for comment in scan(self.parse_ctx, blob)? {
            count_record(self.parse_ctx, &mut self.records)?;
            if comment.rec_type != rt::COMMENT10 {
                continue;
            }
            let entries = scan(self.parse_ctx, comment.body)?;
            let mut consumed = 0;
            for entry in &entries {
                count_record(self.parse_ctx, &mut self.records)?;
                consumed += RECORD_HEADER_LEN + entry.body.len();
            }
            let text = entries.iter().find(|r| r.rec_type == rt::CSTRING && r.instance() == 1);
            let mut blocks = Vec::new();
            if let Some(text) = text {
                self.parse_ctx.charge(text.body.len() as u64)?;
                let (text, damaged) = decode_utf16(utf16le_units(text.body));
                if damaged {
                    self.parse_ctx.emit(DiagnosticCode::InvalidEncoding, Region::Comment);
                }
                blocks.extend(
                    text.split(['\r', '\n'])
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| Block::Paragraph(vec![Inline::plain(s.trim_end_matches('\0'))])),
                );
            } else if consumed < comment.body.len() {
                self.parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Comment);
            }
            let ordinal = self.unit.map_or(0, |unit| unit.model().ordinal.get());
            let id = format!("slide-{ordinal}-comment-{}", self.comments.len());
            self.current.push(Block::Paragraph(vec![Inline::CommentRef(id.clone())]));
            self.comments.push(Note { id, kind: NoteKind::Comment, blocks });
        }
        Ok(())
    }

    fn read_document(&mut self, document: &[u8]) -> Result<(), ConvertError> {
        let parse_ctx = self.parse_ctx;
        for record in scan(parse_ctx, document)? {
            let Record { rec_type, body, .. } = record;
            match rec_type {
                rt::HEADERS_FOOTERS => match record.instance() {
                    rt::SLIDE_HEADERS_FOOTERS_INSTANCE => {
                        self.defaults.slides = Settings::read(parse_ctx, body)?;
                    }
                    rt::NOTES_HEADERS_FOOTERS_INSTANCE => {
                        self.defaults.notes = Settings::read(parse_ctx, body)?;
                    }
                    _ => {}
                },
                rt::ENVIRONMENT => {
                    if let Some(collection) = children(parse_ctx, body, rt::FONT_COLLECTION)?.next()
                    {
                        self.fonts = FontCollection::read(parse_ctx, collection)?;
                    }
                }
                rt::EXTERNAL_OBJECT_LIST => self.read_objects(body)?,
                rt::DRAWING_GROUP => {
                    for group in children(parse_ctx, body, officeart::DGG_CONTAINER)? {
                        for store in children(parse_ctx, group, officeart::BSTORE_CONTAINER)? {
                            self.bstore =
                                scan(parse_ctx, store)?.into_iter().map(StoreEntry::read).collect();
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn read_objects(&mut self, list: &[u8]) -> Result<(), ConvertError> {
        let parse_ctx = self.parse_ctx;
        for Record { rec_type, body, .. } in scan(parse_ctx, list)? {
            let (code, id_atom, id_at) = match rec_type {
                rt::EXTERNAL_HYPERLINK => (None, rt::EXTERNAL_HYPERLINK_ATOM, 0),
                rt::EXTERNAL_OLE_EMBED | rt::EXTERNAL_OLE_LINK => {
                    (Some(DiagnosticCode::EmbeddedObject), rt::EXTERNAL_OLE_OBJECT_ATOM, 8)
                }
                rt::EXTERNAL_OLE_CONTROL => {
                    (Some(DiagnosticCode::FormControlUnread), rt::EXTERNAL_OLE_OBJECT_ATOM, 8)
                }
                rt::EXTERNAL_AVI_MOVIE
                | rt::EXTERNAL_MCI_MOVIE
                | rt::EXTERNAL_MIDI_AUDIO..=rt::EXTERNAL_WAV_AUDIO_LINK => {
                    (Some(DiagnosticCode::GraphicUnread), rt::EXTERNAL_MEDIA_ATOM, 0)
                }
                _ => continue,
            };
            let mut entries = scan(parse_ctx, body)?;
            if let Some(&Record { body: video, .. }) =
                entries.iter().find(|r| r.rec_type == rt::EXTERNAL_VIDEO)
            {
                entries.extend(scan(parse_ctx, video)?);
            }
            let atom = |t: u16, instance: u16| {
                entries.iter().find(|r| r.rec_type == t && r.instance() == instance).map(|r| r.body)
            };
            let Some(id) = atom(id_atom, 0).and_then(|a| get_u32(a, id_at)) else {
                continue;
            };
            if let Some(code) = code {
                self.objects.insert(id, code);
            } else {
                let target = atom(rt::CSTRING, 1).map(|s| decode_utf16(utf16le_units(s)).0);
                self.links.insert(id, target.unwrap_or_default());
            }
        }
        Ok(())
    }

    fn read_master(&mut self, id: u32, master: &[u8]) -> Result<(), ConvertError> {
        let parse_ctx = self.parse_ctx;
        let mut facts = MasterFacts::default();
        for Record { rec_type, body: drawing, .. } in scan(parse_ctx, master)? {
            if rec_type == rt::SLIDE_ATOM {
                let main = get_u32(drawing, 12).filter(|&main| main != 0);
                let own = MasterUse::from_flags(get_u16(drawing, 20).unwrap_or(0));
                facts.follows = main.map(|main| (main, own));
            }
            if rec_type != rt::DRAWING {
                continue;
            }
            for group in children(parse_ctx, drawing, officeart::DG_CONTAINER)? {
                facts.read_group(parse_ctx, group)?;
            }
        }
        self.master_facts.push((id, facts));
        Ok(())
    }

    fn walk_master(&mut self, id: u32, master: &[u8]) -> Result<(), ConvertError> {
        let parse_ctx = self.parse_ctx;
        let Some(index) = self.master_facts.iter().position(|&(master, _)| master == id) else {
            return Ok(());
        };
        let main =
            self.master_facts.get(index).and_then(|(_, facts)| facts.follows).map(|(main, _)| main);
        let mut styled = [id].into_iter().chain(main);
        self.active_master = styled
            .find_map(|id| self.masters.iter().position(|&(master, _)| master == id))
            .unwrap_or(0);
        let saved = (std::mem::take(&mut self.current), std::mem::take(&mut self.list_run));
        self.master = Some(0);
        let walked = parse_ctx.holding_reports(|| {
            for Record { rec_type, body, .. } in scan(parse_ctx, master)? {
                if rec_type == rt::DRAWING {
                    self.walk(body)?;
                }
            }
            self.close_shape()?;
            Ok(std::mem::take(&mut self.current))
        });
        let pictures = self.master.take().unwrap_or(0);
        (self.current, self.list_run) = saved;
        let (blocks, held) = walked?;
        if let Some((_, facts)) = self.master_facts.get_mut(index) {
            facts.content = Some(MasterContent { blocks, held, pictures });
        }
        Ok(())
    }

    fn master_facts_mut(&mut self, id: Option<u32>) -> Option<&mut MasterFacts> {
        let fallback = self.masters.first().map(|&(id, _)| id);
        let at = |id: u32| self.master_facts.iter().position(|&(master, _)| master == id);
        let index = id.and_then(at).or_else(|| fallback.and_then(at))?;
        self.master_facts.get_mut(index).map(|(_, facts)| facts)
    }

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

    fn report_pictures(&self, pictures: &Pictures) {
        let shown: Vec<(Option<AdmittedUnit>, Region, Option<usize>)> = if self.recovering {
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
            let _unit = unit.map(|unit| self.parse_ctx.enter_admitted(unit));
            if pictures.unreadable
                || offset.is_some_and(|offset| pictures.undecodable.contains(&offset))
            {
                if unavailable.insert((unit, region, offset)) {
                    self.parse_ctx.image_loss(ImageLoss::Unavailable, region);
                }
            } else {
                self.parse_ctx.image_loss(ImageLoss::Unplaced, region);
            }
        }
    }

    fn close_shape(&mut self) -> Result<(), ConvertError> {
        self.settle_link()?;
        self.finish_shape()?;
        self.flush_shape()?;
        flush_list(&mut self.current, &mut self.list_run);
        Ok(())
    }

    fn end_segment(&mut self, id: Option<u32>) -> Result<(), ConvertError> {
        self.close_shape()?;
        if !self.current.is_empty() {
            let blocks = std::mem::take(&mut self.current);
            self.segments.push((blocks, id, self.current_is_notes));
        }
        Ok(())
    }

    fn into_blocks(mut self) -> Result<Vec<Block>, ConvertError> {
        self.end_segment(None)?;
        let (mut notes, slides): (Vec<_>, Vec<_>) =
            self.segments.into_iter().partition(|&(.., is_notes)| is_notes);
        let slide_ids: HashSet<u32> = slides.iter().filter_map(|&(_, id, _)| id).collect();
        let quote = |(blocks, ..): Segment| Block::BlockQuote(blocks);
        let mut out: Vec<Block> = notes
            .extract_if(.., |&mut (_, id, _)| !id.is_some_and(|id| slide_ids.contains(&id)))
            .map(quote)
            .collect();
        for (blocks, id, _) in slides {
            out.extend(blocks);
            if let Some(id) = id {
                out.extend(notes.extract_if(.., |&mut (_, note, _)| note == Some(id)).map(quote));
            }
        }
        Ok(out)
    }

    fn select_master(&mut self, slide: Option<&[u8]>) {
        self.active_master = slide
            .and_then(|body| officeart::records(body).find(|r| r.rec_type == rt::SLIDE_ATOM))
            .and_then(|atom| get_u32(atom.body, 12))
            .and_then(|mid| self.masters.iter().position(|&(id, _)| id == mid))
            .unwrap_or(0);
    }

    fn walk(&mut self, data: &[u8]) -> Result<(), ConvertError> {
        let parse_ctx = self.parse_ctx;
        let mut stack: Vec<(&[u8], usize, u16, Option<UnitScope<'_>>)> = vec![(data, 0, 0, None)];
        while let Some((buf, pos, ..)) = stack.last_mut() {
            let Some(record) = record_at(buf, *pos) else {
                match stack.pop() {
                    Some((.., rt::NOTES, _)) => {
                        self.close_shape()?;
                        self.settle_page()?;
                        self.end_segment(None)?;
                        self.current_is_notes = false;
                    }
                    Some((.., Some(_))) => {
                        self.close_shape()?;
                        self.settle_page()?;
                        self.end_segment(None)?;
                        self.unit = None;
                    }
                    _ => {}
                }
                continue;
            };
            let Record { rec_type, body, .. } = record;
            *pos += 8 + body.len();
            count_record(parse_ctx, &mut self.records)?;
            if record.version() != officeart::CONTAINER_VERSION {
                let in_client_data =
                    stack.iter().any(|&(_, _, parent, _)| parent == officeart::CLIENT_DATA);
                self.atom(record, in_client_data)?;
                continue;
            }
            if self.skip_hidden(record)? {
                continue;
            }
            match rec_type {
                rt::CRYPT_SESSION10_CONTAINER => self.encrypted = true,
                rt::HEADERS_FOOTERS => {
                    if record.instance() == rt::PAGE_HEADERS_FOOTERS_INSTANCE {
                        self.page.own = Some(Settings::read(parse_ctx, body)?);
                    }
                }
                rt::PROG_BINARY_TAG => self.read_tag(body)?,
                rt::MAIN_MASTER if self.recovering => self.read_master(0, body)?,
                // Notes containers are walked via the notes list; recovery has no lists, so their text is taken inline (as notes).
                rt::NOTES if self.recovering => {
                    if notes_owner(body).is_none_or(|id| id & 0x8000_0000 == 0) {
                        check_record_depth(parse_ctx, stack.len())?;
                        self.end_segment(None)?;
                        self.current_is_notes = true;
                        stack.push((body, 0, rec_type, None));
                    }
                }
                _ if listed_elsewhere(record) => {}
                _ => {
                    check_record_depth(parse_ctx, stack.len())?;
                    let unit = self.open_unit(rec_type)?;
                    stack.push((body, 0, rec_type, unit));
                }
            }
        }
        Ok(())
    }

    fn skip_hidden(&mut self, record: Record<'_>) -> Result<bool, ConvertError> {
        if !self.parse_ctx.hidden_requested() && hidden::container(self.parse_ctx, record)? {
            self.close_shape()?;
            self.parse_ctx.emit(DiagnosticCode::HiddenOmitted, self.region());
            return Ok(true);
        }
        if self.recovering
            && record.rec_type == rt::SLIDE
            && !self.parse_ctx.hidden_requested()
            && hidden::slide(self.parse_ctx, record.body)?
        {
            self.end_segment(None)?;
            let _unit = self.parse_ctx.begin_unit(UnitKind::Slide, self.recovered_slides)?;
            self.recovered_slides = self.recovered_slides.saturating_add(1);
            self.parse_ctx.emit(DiagnosticCode::HiddenOmitted, Region::Body);
            return Ok(true);
        }
        Ok(false)
    }

    fn open_unit(&mut self, rec_type: u16) -> Result<Option<UnitScope<'c>>, ConvertError> {
        let unit = if self.recovering && rec_type == rt::SLIDE {
            self.end_segment(None)?;
            self.current_is_notes = false;
            self.recovered_slides = self.recovered_slides.saturating_add(1);
            Some(self.parse_ctx.begin_unit(UnitKind::Slide, self.recovered_slides - 1)?)
        } else {
            None
        };
        if let Some(scope) = &unit {
            self.unit = Some(scope.unit());
            self.current.push(Block::UnitStart(scope.unit().model()));
        }
        Ok(unit)
    }

    /// One record outside `walk` (slide-list traversal): containers descend
    /// through the bounded walk, atoms extract directly.
    fn record(&mut self, record: Record<'_>) -> Result<(), ConvertError> {
        count_record(self.parse_ctx, &mut self.records)?;
        if record.version() == officeart::CONTAINER_VERSION {
            match record.rec_type {
                rt::CRYPT_SESSION10_CONTAINER => self.encrypted = true,
                _ if listed_elsewhere(record) => {}
                _ => self.walk(record.body)?,
            }
        } else {
            self.atom(record, false)?;
        }
        Ok(())
    }

    fn atom(&mut self, record: Record<'_>, in_client_data: bool) -> Result<(), ConvertError> {
        let Record { rec_type, body, .. } = record;
        let region = self.region();
        match rec_type {
            rt::TEXT_HEADER_ATOM => {
                self.settle_link()?;
                self.flush_shape()?;
                let tx_type = body.first().map_or(TextType::BODY, |&t| TextType(t.into()));
                let mut pending = PendingShape::new(tx_type, self.drawn_side());
                pending.omitted =
                    self.reading_outline && self.hidden_outline.contains(&self.outline.len());
                self.pending = Some(pending);
                if let Some(shape) = &mut self.shape {
                    shape.text = true;
                }
                if self.reading_outline {
                    self.outline.push(String::new());
                }
            }
            rt::TEXT_CHARS_ATOM => {
                let (text, damaged) = decode_utf16(utf16le_units(body));
                self.push_text(&text);
                if damaged && !self.suppressed() {
                    self.parse_ctx.emit(DiagnosticCode::InvalidEncoding, region);
                }
            }
            rt::TEXT_BYTES_ATOM => {
                let text: String = body.iter().map(|&b| b as char).collect();
                self.push_text(&text);
            }
            rt::STYLE_TEXT_PROP_ATOM => {
                if let Some(pending) = &mut self.pending {
                    let (styles, complete) = styletext::parse_style_text(body, pending.units.len());
                    pending.styles = Some(styles);
                    if !complete && pending.side.is_none() && !pending.omitted {
                        self.parse_ctx.emit(DiagnosticCode::FormattingUnreadable, region);
                    }
                }
            }
            rt::SLIDE_ATOM => {
                let shown = MasterUse::from_flags(get_u16(body, 20).unwrap_or(0));
                self.own_background = !shown.background;
                if self.recovering {
                    self.recovered_use = self.recovered_use.or(shown);
                } else if let Some(master) = self.master_facts_mut(get_u32(body, 12)) {
                    master.shown = master.shown.or(shown);
                    if !self.current_is_notes {
                        self.slide_master = Some((get_u32(body, 12), shown));
                    }
                }
            }
            _ => self.shape_atom(record, in_client_data)?,
        }
        Ok(())
    }

    fn shape_atom(&mut self, record: Record<'_>, in_client_data: bool) -> Result<(), ConvertError> {
        let Record { rec_type, body, .. } = record;
        match rec_type {
            rt::OUTLINE_TEXT_REF_ATOM => {
                if let Some(shape) = &mut self.shape {
                    shape.outline = get_u32(body, 0).and_then(|i| usize::try_from(i).ok());
                }
            }
            rt::PLACEHOLDER_ATOM => {
                if let Some(shape) = &mut self.shape {
                    (shape.side, shape.placeholder) = (Kind::placed(body), true);
                }
            }
            officeart::FSP => {
                self.settle_link()?;
                self.finish_shape()?;
                let flags = get_u32(body, 4).unwrap_or(0);
                self.shape = Some(ShapeFacts {
                    background: flags & FSP_BACKGROUND != 0,
                    ..Default::default()
                });
            }
            officeart::FOPT | officeart::TERTIARY_FOPT => {
                if let Some(shape) = &mut self.shape {
                    for (id, value) in officeart::properties(record.instance(), body) {
                        if id == officeart::PIB {
                            shape.pib = Some(value);
                        }
                        shape.fill.read((id, value));
                    }
                }
            }
            rt::EXTERNAL_OBJECT_REF_ATOM => {
                if let Some(&code) = get_u32(body, 0).and_then(|id| self.objects.get(&id)) {
                    self.parse_ctx.emit(code, self.region());
                    if let Some(shape) = &mut self.shape {
                        shape.preview |= code != DiagnosticCode::GraphicUnread;
                    }
                }
            }
            rt::INTERACTIVE_INFO_ATOM => {
                let link = get_u32(body, 4).filter(|_| body.get(8) == Some(&4));
                if in_client_data {
                    if let (Some(shape), Some(id)) = (&mut self.shape, link) {
                        shape.links.push(id);
                    }
                } else {
                    self.settle_link()?;
                    self.link = link.filter(|_| !self.suppressed());
                }
            }
            rt::TEXT_INTERACTIVE_INFO_ATOM => {
                if let Some(id) = self.link.take() {
                    let label = self.pending_range(get_u32(body, 0), get_u32(body, 4))?;
                    self.report_link(id, &label)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

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

    fn settle_link(&mut self) -> Result<(), ConvertError> {
        match self.link.take() {
            Some(id) => self.report_link(id, ""),
            None => Ok(()),
        }
    }

    fn finish_shape(&mut self) -> Result<(), ConvertError> {
        let Some(shape) = self.shape.take() else {
            return Ok(());
        };
        if let Some(pictures) = &mut self.master {
            let placed = shape.pib.is_some_and(|pib| pib != 0 && !shape.preview)
                || shape.fill.picture().is_some() && !shape.background;
            *pictures += u64::from(placed && !shape.placeholder);
            return Ok(());
        }
        let region = self.region();
        if let Some(pib) = shape.pib.filter(|&pib| pib != 0 && !shape.preview) {
            self.placed.insert((self.unit, region, pib));
        }
        if let Some(pib) = shape.fill.picture()
            && (!shape.background || self.own_background)
        {
            self.placed.insert((self.unit, region, pib));
        }
        let label = match (&self.pending, shape.text) {
            (Some(pending), true) if pending.side.is_some() => return Ok(()),
            (Some(pending), true) => pending.text.as_str(),
            _ => shape.outline.and_then(|i| self.outline.get(i)).map_or("", String::as_str),
        };
        for id in shape.links {
            self.report_link(id, label)?;
        }
        Ok(())
    }

    fn suppressed(&self) -> bool {
        self.pending.as_ref().is_some_and(|pending| pending.side.is_some() || pending.omitted)
    }

    fn drawn_side(&self) -> (Option<Kind>, bool) {
        let shape = self.shape.as_ref();
        let placeholder = self.master.is_some() && shape.is_some_and(|shape| shape.placeholder);
        (shape.and_then(|shape| shape.side), placeholder)
    }

    fn push_text(&mut self, text: &str) {
        if self.reading_outline
            && let Some(block) = self.outline.last_mut()
        {
            block.push_str(text);
        }
        let side = self.drawn_side();
        let pending = self.pending.get_or_insert_with(|| PendingShape::new(TextType::BODY, side));
        pending.units.extend(text.encode_utf16());
        pending.text.push_str(text);
    }

    fn flush_shape(&mut self) -> Result<(), ConvertError> {
        let Some(shape) = self.pending.take() else {
            return Ok(());
        };
        if shape.text.is_empty() || shape.master_placeholder || shape.omitted {
            return Ok(());
        }
        if let Some(kind) = shape.side {
            let blocks = if self.parse_ctx.headers_footers_requested() {
                Some(self.parse_ctx.holding_reports(|| Ok(self.side_blocks(&shape, kind)))?)
            } else {
                None
            };
            self.page.draw(kind, &shape.text, blocks);
            return Ok(());
        }
        self.shape_counter += 1;
        let shape_id = self.shape_counter;
        let is_title = shape.tx_type.is_title();
        let styles = shape.styles.unwrap_or_default();
        let levels: &[MasterLevel] = self
            .masters
            .get(self.active_master)
            .and_then(|(_, m)| m.get(&shape.tx_type))
            .map_or(&[], Vec::as_slice);

        for (mut inlines, depth, bullet) in self.paragraphs(&shape.text, &styles, levels) {
            if inlines_are_empty(&inlines) {
                flush_list(&mut self.current, &mut self.list_run);
                continue;
            }
            if is_title {
                flush_list(&mut self.current, &mut self.list_run);
                let d = level_at(levels, depth).chars.delta;
                let base = StyleDelta { bold: d.bold, italic: d.italic, ..Default::default() };
                rebase_emphasis(&mut inlines, base.resolve());
                let anchor = Some(crate::model::inlines_to_plain_text(&inlines));
                self.current.push(Block::Heading { level: 2, anchor, content: inlines });
                continue;
            }
            let bullet = bullet.or(level_at(levels, depth).bullet).unwrap_or(false);
            if bullet {
                self.list_run.push(ListEntry {
                    mark: ListMark {
                        key: ListKey { instance: shape_id, marker: MarkerKind::Bullet },
                        level: depth as usize,
                        number: 0,
                        label: None,
                    },
                    blocks: vec![Block::Paragraph(inlines)],
                });
            } else {
                flush_list(&mut self.current, &mut self.list_run);
                self.current.push(Block::Paragraph(inlines));
            }
        }
        Ok(())
    }

    fn side_blocks(&self, shape: &PendingShape, kind: Kind) -> Vec<Block> {
        let unstyled = StyleRuns::default();
        let styles = shape.styles.as_ref().unwrap_or(&unstyled);
        let region = kind.side().region();
        let mut blocks = Vec::new();
        for (mut inlines, ..) in self.paragraphs(&shape.text, styles, &[]) {
            for inline in &mut inlines {
                if let Inline::Text { text, .. } = inline
                    && text.contains(side::FIELD)
                {
                    let token = side::field_token(kind).unwrap_or_default();
                    let marks = text.matches(side::FIELD).count() as u64;
                    if !token.is_empty() {
                        self.parse_ctx.emit_count(DiagnosticCode::FieldToken, region, marks);
                    }
                    *text = text.replace(side::FIELD, token);
                }
            }
            if !inlines_are_empty(&inlines) {
                blocks.push(Block::Paragraph(inlines));
            }
        }
        blocks
    }

    fn master_header(&mut self, sides: &mut PageSides) -> Result<(), ConvertError> {
        let Some((id, shown)) = self.slide_master.take() else { return Ok(()) };
        let own_background = self.own_background;
        let at = |id: u32| self.master_facts.iter().position(|&(master, _)| master == id);
        let fallback = self.masters.first().map(|&(id, _)| id);
        let Some(index) = id.and_then(at).or_else(|| fallback.and_then(at)) else {
            return Ok(());
        };
        let Some((_, master)) = self.master_facts.get(index) else { return Ok(()) };
        let follows = master.follows;
        let own_shown = MasterUse {
            background: shown.background && !follows.is_some_and(|(_, own)| own.background),
            ..shown
        };
        let mut chain = vec![(Some(index), own_shown)];
        if let Some((main, own)) = follows {
            let through = MasterUse {
                objects: shown.objects && own.objects,
                background: shown.background && own.background,
            };
            chain.push((at(main), through));
        }
        let parse_ctx = self.parse_ctx;
        for (index, used) in chain {
            self.parse_ctx.charge(1)?;
            let Some((_, facts)) = index.and_then(|index| self.master_facts.get(index)) else {
                continue;
            };
            sides.read(parse_ctx, Side::Header, || {
                let mut blocks = Vec::new();
                if let Some(content) =
                    facts.content.as_ref().filter(|_| used.objects && facts.objects)
                {
                    charge_blocks_copy(&content.blocks, |work| parse_ctx.charge(work))?;
                    parse_ctx.report_held(&content.held)?;
                    blocks.clone_from(&content.blocks);
                    for _ in 0..content.pictures {
                        parse_ctx.image_loss(ImageLoss::Unplaced, Region::Header);
                    }
                }
                if used.background && facts.picture_background && !own_background {
                    parse_ctx.image_loss(ImageLoss::Unplaced, Region::Header);
                }
                Ok(blocks)
            })?;
        }
        Ok(())
    }

    fn paragraphs(
        &self,
        source: &str,
        styles: &StyleRuns,
        levels: &[MasterLevel],
    ) -> Vec<(Vec<Inline>, u16, Option<bool>)> {
        self.paragraphs_in(source, styles, levels, self.region())
    }

    fn paragraphs_in(
        &self,
        source: &str,
        styles: &StyleRuns,
        levels: &[MasterLevel],
        region: Region,
    ) -> Vec<(Vec<Inline>, u16, Option<bool>)> {
        let (parse_ctx, fonts) = (self.parse_ctx, &self.fonts);
        let close_run =
            |text: &mut String, (style, route): (Style, Route), inlines: &mut Vec<_>| {
                if text.is_empty() {
                    return;
                }
                let (run, unmapped) = run_inlines(text, route, style);
                text.clear();
                parse_ctx.emit_count(DiagnosticCode::UnmappedSymbol, region, unmapped);
                inlines.extend(run);
            };
        let slot = |font_ref: Option<u16>| font_ref.map_or(Slot::Inherit, |r| fonts.slot(r));

        let mut runs = StyleCursor::new(styles);

        let mut paragraphs = Vec::new();
        let mut inlines = Vec::new();
        let mut run_text = String::new();
        let mut run_key = (Style::PLAIN, Route::default());
        for c in source.chars() {
            let (chars, para) = runs.current();
            let d = level_at(levels, para.depth);
            let chars = chars.map_or(d.chars, |style| style.or(d.chars));
            let style = chars.delta.resolve();
            let route = Slots { latin: slot(chars.font), symbol: slot(chars.symbol_font) }.route();
            if matches!(c, '\r' | '\u{b}') || (style, route) != run_key {
                close_run(&mut run_text, run_key, &mut inlines);
            }
            match c {
                '\r' => paragraphs.push((std::mem::take(&mut inlines), para.depth, para.bullet)),
                '\u{b}' => inlines.push(Inline::LineBreak),
                _ => {
                    run_key = (style, route);
                    run_text.push(c);
                }
            }
            runs.advance(c);
        }
        close_run(&mut run_text, run_key, &mut inlines);
        if !inlines.is_empty() {
            let (_, para) = runs.current();
            paragraphs.push((inlines, para.depth, para.bullet));
        }
        paragraphs
    }
}

fn level_at(levels: &[MasterLevel], depth: u16) -> MasterLevel {
    levels.get(usize::from(depth)).copied().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::converter::UnitSpan;
    use crate::limits::Limits;
    use crate::model::UnitKind::{self, Slide};
    use crate::shared::officeart::record;
    use crate::shared::test_support::{diagnostic_rows, unit_ordinals};
    use page_side::{
        FOOTER, FOOTER_TEXT, HEADER, SLIDE_NUMBER, TODAY, USER_DATE, binary_tag, by_default,
        comment, drawn, flagged, footer_of, headers_footers, on_notes, on_slide, recovered, tagged,
    };
    use std::assert_matches;
    use std::io::Write;
    use std::num::NonZero;

    use crate::HeaderFooterPolicy;
    mod master_emit;

    /// The Markdown `document` renders to.
    fn markdown_of_document(document: &Document) -> String {
        crate::render::markdown::document_to_markdown(document)
    }
    mod page_side;
    mod page_side_emit;
    mod page_side_reads;

    fn deflated_metafile_picture(inflated: usize) -> Vec<u8> {
        let record = officeart::deflated_metafile_blip(inflated);
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
    fn a_store_entry_reads_its_references_and_offset_from_a_blip_store_record_only() {
        let body = [&[0; 24][..], &2u32.to_le_bytes(), &12u32.to_le_bytes()].concat();
        for (case, rec_type, body, expected) in [
            ("a blip store entry", officeart::FBSE, &body[..], (2, Some(12))),
            ("another record", 0xF01A, &body[..], (0, None)),
            ("a body without references", officeart::FBSE, &body[..24], (0, None)),
            ("a body without an offset", officeart::FBSE, &body[..28], (2, None)),
        ] {
            let bytes = record(0x0062, rec_type, body);
            let entry = StoreEntry::read(record_at(&bytes, 0).unwrap());
            assert_eq!((entry.refs, entry.offset), expected, "{case}");
        }
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
    fn every_record_read_from_a_slide_list_is_charged_and_counted_once() {
        let bytes = record(0x0000, rt::TEXT_BYTES_ATOM, b"ab");
        let limits = Limits { max_records: 1, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let mut ex = Extractor::new(&parse_ctx);
        let atom = record_at(&bytes, 0).unwrap();
        ex.record(atom).unwrap();
        assert_eq!((parse_ctx.fuel(), ex.records), (1, 1));
        let err = ex.record(atom).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Records, max: 1 });
    }

    #[test]
    fn containers_with_their_own_lists_are_not_walked_in_line() {
        for (kind, instance, rec_type, walked) in [
            ("slide list", rt::SLIDE_LIST_INSTANCE, rt::SLIDE_LIST_WITH_TEXT, true),
            ("notes list", rt::NOTES_LIST_INSTANCE, rt::SLIDE_LIST_WITH_TEXT, false),
            ("master list", rt::MASTER_LIST_INSTANCE, rt::SLIDE_LIST_WITH_TEXT, false),
            ("notes", 0, rt::NOTES, false),
            ("main master", 0, rt::MAIN_MASTER, false),
            ("handout", 0, rt::HANDOUT, false),
        ] {
            let bytes = container(instance, rec_type, &[label("text")]);
            let parse_ctx = ParseCtx::default();
            let (mut by_record, mut by_walk) =
                (Extractor::new(&parse_ctx), Extractor::new(&parse_ctx));
            by_record.record(record_at(&bytes, 0).unwrap()).unwrap();
            by_walk.walk(&bytes).unwrap();
            for ex in [&mut by_record, &mut by_walk] {
                ex.end_segment(None).unwrap();
                assert_eq!(!ex.segments.is_empty(), walked, "{kind}");
            }
        }
    }

    #[test]
    fn a_slide_met_in_line_opens_a_unit_only_in_recovery() {
        let bytes = slide(0, &[]);
        for recovering in [false, true] {
            let parse_ctx = ParseCtx::default();
            let mut ex = Extractor::new(&parse_ctx);
            ex.recovering = recovering;
            ex.walk(&bytes).unwrap();
            assert_eq!(ex.recovered_slides, usize::from(recovering));
        }
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

    pub(super) fn words(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    pub(super) fn halves(halves: &[u16]) -> Vec<u8> {
        halves.iter().flat_map(|half| half.to_le_bytes()).collect()
    }

    fn atom32(rec_type: u16, fields: &[u32]) -> Vec<u8> {
        record(0, rec_type, &words(fields))
    }

    fn container(instance: u16, rec_type: u16, children: &[Vec<u8>]) -> Vec<u8> {
        record((instance << 4) | 0xF, rec_type, &children.concat())
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn page_atom(persist: u32, id: u32) -> Vec<u8> {
        atom32(0x03F3, &[persist, 0, 0, id, 0])
    }

    fn pages(instance: u16, pages: &[(u32, u32)]) -> Vec<u8> {
        let atoms: Vec<Vec<u8>> =
            pages.iter().map(|&(persist, id)| page_atom(persist, id)).collect();
        container(instance, 0x0FF0, &atoms)
    }

    fn outline(content: &[Vec<u8>]) -> Vec<u8> {
        container(0, 0x0FF0, &[&[page_atom(2, 256)][..], content].concat())
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
        let opt = record((count << 4) | 3, 0xF00B, &table);
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
        container(0, 0xF004, &[record(2, 0xF00A, &fsp), record((1 << 4) | 3, 0xF122, &table)])
    }

    fn text(units: &[u16]) -> Vec<u8> {
        [atom32(0x0F9F, &[1]), record(0, 0x0FA0, &halves(units))].concat()
    }

    fn typed(text_type: u32, text: &str) -> Vec<u8> {
        [atom32(0x0F9F, &[text_type]), record(0, 0x0FA0, &utf16(text))].concat()
    }

    fn label(text: &str) -> Vec<u8> {
        typed(1, text)
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

    /// A one-slide deck whose slide shows `flags` of its title master `title`, which follows
    /// the main master `main`.
    fn titled(flags: u32, main: Vec<u8>, title: Vec<u8>) -> Vec<u8> {
        let doc = [pages(0, &[(2, 256)]), pages(1, &[(3, 1), (4, 2)])];
        listed(&doc, &[slide_on(2, flags, &[]), main, title])
    }

    /// A one-slide deck whose slide shows `flags` of a main master of `shapes` and `fill`.
    fn mastered(shapes: &[Vec<u8>], fill: u32, flags: u32) -> Vec<u8> {
        let doc = [pages(0, &[(2, 256)]), pages(1, &[(3, 1)])];
        listed(&doc, &[slide(flags, &[]), master(shapes, fill)])
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
        stream.extend(atom32(0x1772, &[&[1 | (count << 20)][..], &offsets].concat()));
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

    /// A deck whose usable persist directory lists `objects`, with no Pictures stream.
    fn listed(doc: &[Vec<u8>], objects: &[Vec<u8>]) -> Vec<u8> {
        deck(doc, objects, OptionalStream::Absent, true)
    }

    fn hyperlinks() -> Vec<u8> {
        let links = [hyperlink(1, "https://example.org/a", ""), hyperlink(2, "", "256,1,Slide 1")];
        container(0, 0x0409, &links)
    }

    fn linked_shapes(shapes: &[Vec<u8>]) -> Vec<u8> {
        listed(&[pages(0, &[(2, 256)]), hyperlinks()], &[slide(0, &[drawing(shapes)])])
    }

    type Reported = (DiagnosticCode, Region, Option<u32>, u64);

    fn reported(bytes: &[u8]) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, bytes).unwrap();
        diagnostic_rows(&parse_ctx.diagnostics())
    }

    fn author_comment_deck(text: Option<Vec<u8>>) -> Vec<u8> {
        const COMMENT10_ATOM: u16 = 0x2EE1;
        let mut settings = 3_i32.to_le_bytes().to_vec();
        settings.extend(halves(&[2024, 1, 1, 1, 12, 34, 56, 0]));
        settings.extend(17_i32.to_le_bytes());
        settings.extend(29_i32.to_le_bytes());
        let mut children = vec![record(0, rt::CSTRING, &utf16("Ada"))];
        children.extend(text);
        children.push(record(0, COMMENT10_ATOM, &settings));
        let comment = container(0, rt::COMMENT10, &children);
        listed(&[pages(0, &[(2, 256)])], &[slide(0, &[binary_tag("___PPT10", &[comment])])])
    }

    #[test]
    fn binary_comments_allow_absent_optional_text_without_loss() {
        let bytes = author_comment_deck(None);
        let converter = crate::Converter::new(crate::Config {
            comments: crate::CommentPolicy::Emit,
            ..crate::Config::default()
        })
        .unwrap();
        let converted = converter.convert(crate::Input::new(&bytes, crate::Format::Ppt)).unwrap();
        assert_eq!(converted.markdown(), "");
        assert_eq!(diagnostic_rows::<Reported>(converted.diagnostics()), []);
    }

    #[test]
    fn truncated_binary_comment_text_remains_unread_content() {
        let mut text = record(1 << 4, rt::CSTRING, &utf16("unread"));
        text[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        let bytes = author_comment_deck(Some(text));
        let converter = crate::Converter::new(crate::Config {
            comments: crate::CommentPolicy::Emit,
            ..crate::Config::default()
        })
        .unwrap();
        let error = converter.convert(crate::Input::new(&bytes, crate::Format::Ppt)).unwrap_err();
        assert_matches!(error, ConvertError::Incomplete { diagnostics } if
            diagnostic_rows::<Reported>(&diagnostics) ==
                [(DiagnosticCode::PartUnreadable, Region::Comment, Some(1), 1)]);
    }

    #[test]
    fn requested_binary_comments_read_text_instance_and_keep_unit_identity() {
        let comment = container(
            0,
            rt::COMMENT10,
            &[
                record(0, rt::CSTRING, &utf16("decoy author")),
                record(1 << 4, rt::CSTRING, &utf16("comment body")),
                record(2 << 4, rt::CSTRING, &utf16("XX")),
            ],
        );
        let tag = binary_tag("___PPT10", &[comment]);
        let bytes = listed(
            &[pages(0, &[(2, 256), (3, 257)])],
            &[slide(0, std::slice::from_ref(&tag)), slide(0, &[tag])],
        );
        let ctx = ParseCtx::default().with_comments(crate::CommentPolicy::Emit);
        let document = parse(&ctx, &bytes).unwrap();
        assert_eq!(document.notes.len(), 2);
        assert_ne!(document.notes[0].id, document.notes[1].id);
        for note in &document.notes {
            assert_eq!(note.kind, NoteKind::Comment);
            assert_eq!(note.blocks, vec![Block::Paragraph(vec![Inline::plain("comment body")])]);
        }
        assert_eq!(diagnostic_rows::<Reported>(&ctx.diagnostics()), []);
    }

    fn hidden_slide_with(content: &[Vec<u8>]) -> Vec<u8> {
        let mut flags = [0; 16];
        flags[10] = 4;
        slide(0, &[&[record(0, 0x03F9, &flags)][..], content].concat())
    }

    #[test]
    fn hidden_slides_drop_their_outline_notes_and_comments_with_original_ordinals() {
        let list = container(
            0,
            0x0FF0,
            &[page_atom(2, 256), typed(4, "secret outline"), page_atom(3, 257)],
        );
        let bytes = listed(
            &[list, pages(2, &[(4, 0), (5, 0)])],
            &[
                hidden_slide_with(&[binary_tag("___PPT10", &[comment()])]),
                slide(0, &[label("shown")]),
                notes(256, &[label("secret note")]),
                notes(257, &[label("shown note")]),
            ],
        );
        let omit = ParseCtx::default().with_comments(crate::CommentPolicy::Emit);
        let document = parse(&omit, &bytes).unwrap();
        assert_eq!(unit_ordinals(&document), [2]);
        assert_eq!(
            crate::render::markdown::document_to_markdown(&document),
            "shown\n\n> shown note\n"
        );
        assert_eq!(
            diagnostic_rows::<Reported>(&omit.diagnostics()),
            [(DiagnosticCode::HiddenOmitted, Region::Body, Some(1), 1)]
        );
        let emit = ParseCtx::default().with_hidden(crate::HiddenPolicy::Emit);
        let document = parse(&emit, &bytes).unwrap();
        assert_eq!(unit_ordinals(&document), [1, 2]);
        assert!(
            crate::render::markdown::document_to_markdown(&document).contains("secret outline")
        );
    }

    #[test]
    fn hidden_shapes_drop_text_and_outline_references_unless_requested() {
        let hidden = text_shape(0x0A00, &[(0x03BF, 0x0002_0002)], &[], &[typed(4, "hidden text")]);
        let referenced = shape(0x0A00, &[(0x03BF, 0x0002_0002)], &[atom32(0x0F9E, &[0])]);
        let list = container(0, 0x0FF0, &[page_atom(2, 256), typed(4, "hidden outline")]);
        let bytes = listed(&[list], &[slide(0, &[drawing(&[hidden, referenced])])]);
        let omit = ParseCtx::default();
        let document = parse(&omit, &bytes).unwrap();
        assert_eq!(crate::render::markdown::document_to_markdown(&document), "");
        assert_eq!(
            diagnostic_rows::<Reported>(&omit.diagnostics()),
            [(DiagnosticCode::HiddenOmitted, Region::Body, Some(1), 2)]
        );
        let emit = ParseCtx::default().with_hidden(crate::HiddenPolicy::Emit);
        let document = parse(&emit, &bytes).unwrap();
        assert_eq!(
            crate::render::markdown::document_to_markdown(&document),
            "hidden outline\n\nhidden text\n"
        );
        assert_eq!(diagnostic_rows::<Reported>(&emit.diagnostics()), []);
    }

    #[test]
    fn hidden_outline_probe_leaves_specialized_side_containers_to_their_readers() {
        let mut nested = atom32(0x2B00, &[0]);
        for _ in 0..12 {
            nested = container(0, 0x2B02, &[nested]);
        }
        for side in [
            container(0, rt::HEADERS_FOOTERS, &[record(0, 0x0FDA, &[0; 4]), nested.clone()]),
            container(
                0,
                rt::PROG_BINARY_TAG,
                &[record(0, rt::CSTRING, &utf16("unrelated")), nested],
            ),
        ] {
            let bytes = listed(&[pages(0, &[(2, 256)])], &[slide(0, &[side])]);
            let limits = Limits { max_record_depth: 8, ..Limits::default() };
            let ctx = ParseCtx::new(&limits);
            let document = parse(&ctx, &bytes).unwrap();
            assert_eq!(crate::render::markdown::document_to_markdown(&document), "");
            assert_eq!(diagnostic_rows::<Reported>(&ctx.diagnostics()), []);
        }
    }

    #[test]
    fn hidden_groups_suppress_descendants_and_leave_clear_flags_visible() {
        let content = text_shape(0x0A00, &[], &[], &[typed(4, "inside")]);
        for (flags, hidden) in [(0x0002_0002, true), (0x0002_0000, false), (0x0000_0002, false)] {
            let parent = shape(1, &[(0x03BF, flags)], &[]);
            let group = container(0, 0xF003, &[parent, content.clone()]);
            let bytes = listed(&[pages(0, &[(2, 256)])], &[slide(0, &[drawing(&[group])])]);
            let omit = ParseCtx::default();
            let document = parse(&omit, &bytes).unwrap();
            assert_eq!(
                crate::render::markdown::document_to_markdown(&document),
                if hidden { "" } else { "inside\n" }
            );
            let expected = if hidden {
                vec![(DiagnosticCode::HiddenOmitted, Region::Body, Some(1), 1)]
            } else {
                vec![]
            };
            assert_eq!(diagnostic_rows::<Reported>(&omit.diagnostics()), expected);
            let emit = ParseCtx::default().with_hidden(crate::HiddenPolicy::Emit);
            let document = parse(&emit, &bytes).unwrap();
            assert_eq!(crate::render::markdown::document_to_markdown(&document), "inside\n");
        }
    }

    #[test]
    fn binary_comment_decode_damage_is_reported_only_when_requested() {
        let damaged =
            container(0, rt::COMMENT10, &[record(1 << 4, rt::CSTRING, &halves(&[0x61, 0xD800]))]);
        let bytes =
            listed(&[pages(0, &[(2, 256)])], &[slide(0, &[binary_tag("___PPT10", &[damaged])])]);
        for policy in [crate::CommentPolicy::Omit, crate::CommentPolicy::Emit] {
            let ctx = ParseCtx::default().with_comments(policy);
            let document = parse(&ctx, &bytes).unwrap();
            let code = if policy == crate::CommentPolicy::Emit {
                DiagnosticCode::InvalidEncoding
            } else {
                DiagnosticCode::CommentOmitted
            };
            assert_eq!(
                diagnostic_rows::<Reported>(&ctx.diagnostics()),
                [(code, Region::Comment, Some(1), 1)]
            );
            if policy == crate::CommentPolicy::Emit {
                assert_eq!(document.notes[0].blocks, [Block::Paragraph(vec![Inline::plain("a�")])]);
            }
        }
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of losses by slide")]
    fn each_loss_is_reported_in_its_slide() {
        use DiagnosticCode::*;
        use OptionalStream::{Absent, Read, Unreadable};
        use Region::{Body, Comment, Header, Note};
        let two_slides = pages(0, &[(2, 256), (3, 257)]);
        let one_slide = || pages(0, &[(2, 256)]);
        let pictured = |pictures: OptionalStream| {
            let shapes = drawing(&[picture(1), picture(2)]);
            deck(&[one_slide(), bstore(&[0, 12])], &[slide(0, &[shapes])], pictures, true)
        };
        let unpaired = || text(&[0x61, 0xD800, 0x0D]);
        let filled = |flags: u32, drawing: Vec<u8>| {
            let doc = [one_slide(), bstore(&[0])];
            deck(&doc, &[slide(flags, &[drawing])], Read(png(true)), true)
        };
        let fill_shape = |props: &[(u16, u32)]| drawing(&[shape(0x0A00, props, &[])]);
        let picture_fill = [(0x0180, 3), (0x4186, 1)];
        let styled = |style: &[u8]| {
            let content = [text(&[0x61, 0x62, 0x0D]), record(0, 0x0FA1, style)].concat();
            on_slide(&[content])
        };
        let noted = |notes_list: Vec<u8>, owner: u32| {
            let objects = [slide(0, &[]), slide(0, &[]), notes(owner, &[unpaired()])];
            listed(&[two_slides.clone(), notes_list], &objects)
        };
        let objects = |list: &[Vec<u8>], shapes: &[Vec<u8>]| {
            let doc = [one_slide(), container(0, 0x0409, list), bstore(&[0])];
            deck(&doc, &[slide(0, &[drawing(shapes)])], Read(png(true)), true)
        };
        let showing = |id: u32| shape(0x0A10, &[(0x4104, 1)], &[atom32(0x0BC1, &[id])]);
        let outlined = |index: u32| {
            let list = outline(&[label("see here\r"), label("https://example.org/a\r")]);
            let reference = [atom32(0x0F9E, &[index])];
            let shape = text_shape(0x0A00, &[], &[link(4, 1, None)], &reference);
            listed(&[list, hyperlinks()], &[slide(0, &[drawing(&[shape])])])
        };
        let master_styled = |levels: &[u8]| {
            let style = record(1 << 4, 0x0FA3, &[&[2, 0][..], levels].concat());
            let master = container(0, 0x03F8, &[atom32(0x03EF, &[0; 6]), style]);
            listed(&[one_slide(), pages(1, &[(3, 1)])], &[slide(0, &[]), master])
        };
        let recovered_link = |slides: &[Vec<u8>]| {
            let clicked = text_shape(0x0A00, &[], &[link(4, 1, None)], &[label("see\r")]);
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
                vec![(ImageUnavailable, Body, Some(1), 1), (ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "a Pictures stream that does not read",
                pictured(Unreadable),
                vec![(ImageUnavailable, Body, Some(1), 2)],
            ),
            ("no Pictures stream", pictured(Absent), vec![(ImageUnplaced, Body, Some(1), 2)]),
            (
                "an OLE object and its preview",
                objects(&[ole(7)], &[showing(7)]),
                vec![(EmbeddedObject, Body, Some(1), 1)],
            ),
            (
                "an OLE object and its preview on a notes page",
                listed(
                    &[one_slide(), container(0, 0x0409, &[ole(7)]), pages(2, &[(3, 300)])],
                    &[slide(0, &[]), notes(256, &[drawing(&[showing(7)])])],
                ),
                vec![(EmbeddedObject, Note, Some(1), 1)],
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
                "a hyperlink in outline text whose label hides its target",
                listed(
                    &[outline(&[label("see here\r"), link(4, 1, Some((0, 8)))]), hyperlinks()],
                    &[slide(0, &[])],
                ),
                vec![(LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "a placeholder after a text box, its outline text its click link",
                listed(
                    &[outline(&[label("https://example.org/a\r")]), hyperlinks()],
                    &[slide(
                        0,
                        &[drawing(&[
                            text_shape(0x0A00, &[], &[], &[label("note\r")]),
                            text_shape(0x0A00, &[], &[link(4, 1, None)], &[atom32(0x0F9E, &[0])]),
                        ])],
                    )],
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
                "a picture with a click link and no text",
                linked_shapes(&[shape(0x0A00, &[(0x4104, 1)], &[link(4, 1, None)])]),
                vec![(ImageUnplaced, Body, Some(1), 1), (LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "a click link on a shape after a text box showing its URL",
                linked_shapes(&[
                    text_shape(0x0A00, &[], &[], &[label("https://example.org/a\r")]),
                    shape(0x0A00, &[], &[link(4, 1, None)]),
                ]),
                vec![(LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "a click link on a shape before a text box showing its URL",
                linked_shapes(&[
                    shape(0x0A00, &[], &[link(4, 1, None)]),
                    text_shape(0x0A00, &[], &[], &[label("https://example.org/a\r")]),
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
                filled(0, fill_shape(&[(0x0180, 3), (0x4186, 1), (0x01BF, (1 << 20) | (1 << 4))])),
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
                on_slide(&[unpaired()]),
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
                listed(&[pages(0, &[(9, 256), (2, 257)])], &[slide(0, &[unpaired()])]),
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
            (
                "a master logo before a placeholder",
                mastered(&[picture(1), placeholder()], 0, 1),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "a master logo one of two slides shows",
                listed(
                    &[two_slides.clone(), pages(1, &[(4, 1)])],
                    &[slide(1, &[]), slide(0, &[]), master(&[picture(1)], 0)],
                ),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
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
                listed(
                    &[one_slide(), pages(1, &[(3, 1), (4, 2)])],
                    &[slide_on(1, 5, &[]), master(&[placeholder()], 3), master(&[picture(1)], 3)],
                ),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "a master logo on the first master, for a slide naming none",
                listed(
                    &[one_slide(), pages(1, &[(3, 1), (4, 2)])],
                    &[slide(1, &[]), master(&[picture(1)], 0), master(&[placeholder()], 0)],
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
            (
                "a footer string its flag shows",
                flagged(FOOTER, &FOOTER_TEXT),
                vec![footer_of(1, 1)],
            ),
            ("today's date", flagged(TODAY, &[]), vec![footer_of(1, 1)]),
            ("a user date with text", flagged(USER_DATE, &[(0, "1 May")]), vec![footer_of(1, 1)]),
            ("a slide number", flagged(SLIDE_NUMBER, &[]), vec![footer_of(1, 1)]),
            (
                "a notes page's header",
                on_notes(&[], &[headers_footers(0, HEADER, &[(1, "Draft")])]),
                vec![(HeaderOmitted, Header, Some(1), 1)],
            ),
            (
                "a slide's default its own container replaces",
                by_default(headers_footers(3, FOOTER, &FOOTER_TEXT), &[headers_footers(0, 0, &[])]),
                vec![],
            ),
            (
                "a comment under ___PPT10",
                tagged("___PPT10", &[comment()]),
                vec![(CommentOmitted, Comment, Some(1), 1)],
            ),
            ("a comment under another tag", tagged("___PPT9", &[comment()]), vec![]),
            (
                "a footer placeholder drawn on the slide",
                on_slide(&[drawing(&[drawn(9, "Confidential")])]),
                vec![footer_of(1, 1)],
            ),
            (
                "a recovered deck's defaults, containers and comments",
                recovered(&[
                    slide(0, &[]),
                    slide(
                        0,
                        &[
                            headers_footers(0, SLIDE_NUMBER, &[]),
                            binary_tag("___PPT10", &[comment()]),
                        ],
                    ),
                    notes(256, &[]),
                ]),
                vec![
                    (RecoveredStreamOrder, Body, None, 1),
                    footer_of(1, 1),
                    footer_of(1, 2),
                    (CommentOmitted, Comment, Some(2), 1),
                    (HeaderOmitted, Header, None, 1),
                ],
            ),
        ];
        for (case, bytes, mut expected) in cases {
            expected.sort();
            assert_eq!(reported(&bytes), expected, "{case}");
        }
    }

    #[test]
    fn a_hyperlink_is_reported_unread_where_the_text_it_covers_hides_its_target() {
        use {DiagnosticCode::LinkTargetUnread, Region::Body};
        let expected = |lost: bool| -> Vec<Reported> {
            if lost { vec![(LinkTargetUnread, Body, Some(1), 1)] } else { vec![] }
        };
        for (case, id, shown, range, lost) in [
            ("a hyperlink whose label hides its target", 1, "see here\r", Some((0, 8)), true),
            (
                "a hyperlink whose label shows its target",
                1,
                "see https://example.org/a\r",
                Some((4, 25)),
                false,
            ),
            (
                "a hyperlink whose label shows its target without the scheme",
                1,
                "example.org/a\r",
                Some((0, 13)),
                false,
            ),
            (
                "an unknown hyperlink under a label without a scheme",
                5,
                "example.org/a\r",
                Some((0, 13)),
                true,
            ),
            ("a hyperlink to a slide", 2, "see here\r", Some((0, 8)), false),
            ("an unknown hyperlink", 5, "see here\r", Some((0, 8)), true),
            ("an unknown hyperlink under a URL", 5, "https://x.org/\r", Some((0, 14)), false),
            ("a text hyperlink with no text range", 1, "https://example.org/a\r", None, true),
            (
                "a hyperlink range past the text's end",
                1,
                "https://example.org/a",
                Some((0, 22)),
                false,
            ),
        ] {
            let textbox = [label(shown), link(4, id, range)];
            let bytes = linked_shapes(&[text_shape(0x0A00, &[], &[], &textbox)]);
            assert_eq!(reported(&bytes), expected(lost), "{case}");
        }
        for (case, id, shown, lost) in [
            ("a text box whose whole text is its click link", 1, "https://example.org/a\r", false),
            ("a text box whose click link its text hides", 1, "see here\r", true),
            ("a text box whose click link jumps to a slide", 2, "see here\r", false),
        ] {
            let click = [link(4, id, None)];
            let bytes = linked_shapes(&[text_shape(0x0A00, &[], &click, &[label(shown)])]);
            assert_eq!(reported(&bytes), expected(lost), "{case}");
        }
    }

    #[test]
    fn reporting_masters_charges_each_master_once() {
        let report = |parse_ctx: &ParseCtx<'_>| {
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
        let bytes = listed(&doc, &objects);
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
    fn an_unknown_text_type_takes_the_master_defaults_of_its_own_value() {
        let defaults = |text_type: u16, bold: u8| {
            let depth: &[u8] = if text_type >= 5 { &[0, 0] } else { &[] };
            let level = [&[1, 0][..], depth, &[0; 4], &[1, 0, 0, 0], &[bold, 0]].concat();
            record(text_type << 4, 0x0FA3, &level)
        };
        let master =
            container(0, 0x03F8, &[atom32(0x03EF, &[0; 6]), defaults(1, 0), defaults(9, 1)]);
        let blocks = |text_type: u32| {
            let text = [atom32(0x0F9F, &[text_type]), record(0, 0x0FA0, &utf16("Nine"))];
            let objects =
                [slide(0, &[drawing(&[text_shape(0x0A00, &[], &[], &text)])]), master.clone()];
            let doc = [pages(0, &[(2, 256)]), pages(1, &[(3, 1)])];
            parse(&ParseCtx::default(), &listed(&doc, &objects)).unwrap().blocks
        };
        let paragraph = |bold: bool| {
            let style = Style { bold, ..Style::PLAIN };
            vec![
                Block::UnitStart(crate::model::Unit::new(Slide, std::num::NonZeroU32::MIN)),
                Block::Paragraph(vec![Inline::Text { text: "Nine".to_owned(), style }]),
            ]
        };
        assert_eq!(blocks(9), paragraph(true));
        assert_eq!(blocks(1), paragraph(false));
    }

    /// A deck of one text of `text_type` made of `paragraphs` (text, depth), whose master gives
    /// each level a default of `[bullet, style bits]`.
    fn with_master_levels(
        text_type: u16,
        levels: &[[u8; 2]],
        paragraphs: &[(&str, u16)],
    ) -> Vec<u8> {
        let level = |&[bullet, style]: &[u8; 2]| {
            [&1u32.to_le_bytes()[..], &[bullet, 0], &3u32.to_le_bytes(), &[style, 0]].concat()
        };
        let defaults = [
            &[u8::try_from(levels.len()).unwrap(), 0][..],
            &levels.iter().flat_map(level).collect::<Vec<_>>(),
        ];
        let defaults = record(text_type << 4, 0x0FA3, &defaults.concat());
        let master = container(0, 0x03F8, &[atom32(0x03EF, &[0; 6]), defaults]);
        let units = |text: &str| u32::try_from(text.encode_utf16().count() + 1).unwrap();
        let run = |&(text, depth): &(&str, u16)| {
            [&units(text).to_le_bytes()[..], &depth.to_le_bytes(), &0u32.to_le_bytes()].concat()
        };
        let text = paragraphs.iter().map(|&(text, _)| text).collect::<Vec<_>>().join("\r");
        let style = [
            &paragraphs.iter().flat_map(run).collect::<Vec<_>>()[..],
            &units(&text).to_le_bytes(),
            &[0; 4],
        ];
        let body = [
            atom32(0x0F9F, &[u32::from(text_type)]),
            record(0, 0x0FA0, &utf16(&text)),
            record(0, 0x0FA1, &style.concat()),
        ];
        let objects = [slide(0, &[drawing(&[text_shape(0x0A00, &[], &[], &body)])]), master];
        let doc = [pages(0, &[(2, 256)]), pages(1, &[(3, 1)])];
        listed(&doc, &objects)
    }

    #[test]
    fn a_paragraph_takes_the_master_defaults_of_its_own_depth() {
        for (case, levels, paragraphs, expected) in [
            (
                "a bold default at depth one",
                &[[0, 0], [0, 1]][..],
                &[("a", 0), ("b", 1)][..],
                "a\n\n**b**\n",
            ),
            ("a bullet default at depth one", &[[0, 0], [1, 0]], &[("b", 1)], "- b\n"),
        ] {
            let bytes = with_master_levels(1, levels, paragraphs);
            assert_eq!(markdown_of(&bytes), expected, "{case}");
        }
    }

    #[test]
    fn a_title_heading_carries_no_emphasis_its_master_default_gives() {
        let plain = Inline::Text { text: "t".to_owned(), style: Style::PLAIN };
        let heading =
            Block::Heading { level: 2, anchor: Some("t".to_owned()), content: vec![plain] };
        for (case, style) in [("bold", 1), ("italic", 2)] {
            let bytes = with_master_levels(0, &[[0, style]], &[("t", 0)]);
            let blocks = parse(&ParseCtx::default(), &bytes).unwrap().blocks;
            assert_eq!(blocks[1], heading, "{case}");
        }
    }

    #[test]
    fn every_record_the_document_and_master_reads_scan_is_charged_once() {
        let list = container(0, 0x0409, &[hyperlink(1, "a", ""), ole(2)]);
        let document = [list, bstore(&[0])].concat();
        let master = master(&[picture(1), placeholder()], 3)[8..].to_vec();
        let fonts = environment(&["Arial", "Symbol", "Wingdings"]);
        let defaults = [
            headers_footers(3, FOOTER, &FOOTER_TEXT),
            headers_footers(4, HEADER | FOOTER, &[(1, "Draft"), (2, "Confidential")]),
        ]
        .concat();
        type Read = fn(&mut Extractor<'_>, &[u8]) -> Result<(), ConvertError>;
        let reads: [(&[u8], u64, Read); 4] = [
            (&document, 12, |ex, data| ex.read_document(data)),
            (&fonts, 5, |ex, data| ex.read_document(data)),
            (&defaults, 2 + 2 + 3, |ex, data| ex.read_document(data)),
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
    fn a_recovered_deck_charges_its_document_scan_and_visibility_probe() {
        let bytes = deck(&[pages(0, &[(2, 256)])], &[slide(0, &[])], OptionalStream::Absent, false);
        for (policy, expected) in [(crate::HiddenPolicy::Omit, 13), (crate::HiddenPolicy::Emit, 12)]
        {
            let parse_ctx = ParseCtx::default().with_hidden(policy);
            parse(&parse_ctx, &bytes).unwrap();
            let fuel = parse_ctx.fuel();
            assert_eq!(fuel, expected);
            let limits = Limits { max_work: fuel - 1, ..Limits::default() };
            assert_matches!(
                parse(&ParseCtx::new(&limits).with_hidden(policy), &bytes),
                Err(ConvertError::ResourceLimit { limit: Limit::Work, max }) if max == fuel - 1
            );
        }
    }

    #[test]
    fn each_hyperlink_charges_its_label_and_target() {
        let links: Vec<Vec<u8>> = (0..3).map(|_| link(4, 1, Some((0, 8)))).collect();
        let content = [&[label("see here\r")][..], &links].concat().concat();
        let walk = |parse_ctx: &ParseCtx<'_>| {
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

    #[test]
    fn every_listed_or_recovered_slide_counts_against_max_pages() {
        let listing =
            listed(&[pages(0, &[(2, 256), (9, 257), (3, 258)])], &[slide(0, &[]), slide(0, &[])]);
        let recovered = deck(
            &[pages(0, &[(2, 256), (3, 257), (4, 258)])],
            &[slide(0, &[]), slide(0, &[]), slide(0, &[])],
            OptionalStream::Absent,
            false,
        );
        for (case, bytes) in [("listed", listing), ("recovered", recovered)] {
            let at =
                |max_pages| parse(&ParseCtx::new(&Limits { max_pages, ..Limits::DEFAULT }), &bytes);
            assert!(at(3).is_ok(), "{case}");
            assert_matches!(
                at(2),
                Err(ConvertError::ResourceLimit { limit: Limit::Pages, max: 2 }),
                "{case}"
            );
        }
    }

    #[test]
    fn notes_and_pictures_reenter_their_slides_unit_at_max_pages() {
        let unpaired = text(&[0x61, 0xD800, 0x0D]);
        let bytes = listed(
            &[pages(0, &[(2, 256), (3, 257)]), pages(2, &[(4, 300)]), bstore(&[0])],
            &[slide(0, &[]), slide(0, &[drawing(&[picture(1)])]), notes(257, &[unpaired])],
        );
        let parse_ctx = ParseCtx::new(&Limits { max_pages: 2, ..Limits::DEFAULT });
        parse(&parse_ctx, &bytes).unwrap();
        assert_eq!(
            diagnostic_rows::<Reported>(&parse_ctx.diagnostics()),
            [
                (DiagnosticCode::InvalidEncoding, Region::Note, Some(2), 1),
                (DiagnosticCode::ImageUnplaced, Region::Body, Some(2), 1)
            ]
        );
    }

    fn text_box(text: &str) -> Vec<u8> {
        drawing(&[text_shape(0x0A00, &[], &[], &[label(text)])])
    }

    fn bulleted(text: &str) -> Vec<u8> {
        let runs = u32::try_from(text.encode_utf16().count() + 1).unwrap();
        let style = [&runs.to_le_bytes()[..], &[0, 0], &1u32.to_le_bytes(), &[1, 0]];
        let chars = [runs.to_le_bytes(), 0u32.to_le_bytes()].concat();
        [label(text), record(0, 0x0FA1, &[&style.concat()[..], &chars].concat())].concat()
    }

    fn rendered(document: &Document) -> crate::render::markdown::Rendered {
        let ledger = crate::ledger::Ledger::new(u64::MAX, None);
        let meter = crate::render::markdown::Meter::new(&ledger, u64::MAX);
        let fills = crate::ocr::GapFills::default();
        crate::render::markdown::render(document, &fills, &meter, crate::ImagePolicy::Placeholder)
            .unwrap()
    }

    fn spans(rendered: &crate::render::markdown::Rendered) -> Vec<(UnitKind, u32, &str)> {
        let span = |s: &UnitSpan| {
            (s.unit.kind, s.unit.ordinal.get(), rendered.markdown.get(s.bytes.clone()).unwrap())
        };
        rendered.units.iter().map(span).collect()
    }

    #[test]
    fn recovered_units_settle_their_text_and_lists_before_the_next_milestone() {
        let bytes = deck(
            &[],
            &[slide(0, &[text_box("first")]), slide(0, &[text_box("second")])],
            OptionalStream::Absent,
            false,
        );
        let rendered = rendered(&parse(&ParseCtx::default(), &bytes).unwrap());
        assert_eq!(spans(&rendered), [(Slide, 1, "first"), (Slide, 2, "second")]);
    }

    #[test]
    fn listed_text_of_an_unreadable_slide_keeps_its_own_unit() {
        let atoms = [page_atom(2, 256), page_atom(9, 257), label("kept"), page_atom(3, 258)];
        let list = container(0, 0x0FF0, &atoms);
        let objects = [slide(0, &[text_box("first")]), slide(0, &[text_box("third")])];
        let bytes = listed(&[list], &objects);
        let ctx = ParseCtx::default();
        let rendered = rendered(&parse(&ctx, &bytes).unwrap());
        assert_eq!(
            spans(&rendered),
            [(Slide, 1, "first"), (Slide, 2, "kept"), (Slide, 3, "third")]
        );
        let unreadable: Vec<Option<u32>> = ctx
            .diagnostics()
            .iter()
            .filter(|d| d.code == DiagnosticCode::UnitUnreadable)
            .map(|d| d.unit.map(NonZero::get))
            .collect();
        assert_eq!(unreadable, [Some(2)]);
    }

    #[test]
    fn listed_text_before_the_first_page_settles_outside_its_unit() {
        let list = container(0, 0x0FF0, &[label("stray"), page_atom(2, 256)]);
        let bytes = listed(&[list], &[slide(0, &[text_box("first")])]);
        let rendered = rendered(&parse(&ParseCtx::default(), &bytes).unwrap());
        assert_eq!(spans(&rendered), [(Slide, 1, "first")]);
        let before = rendered.markdown.get(..rendered.units[0].bytes.start).unwrap();
        assert!(before.contains("stray"), "{before:?}");
    }

    #[test]
    fn recovered_text_before_a_slide_settles_outside_its_unit_in_stream_order() {
        let objects = [
            notes(0, &[label("note")]),
            container(0, 0x0FF0, &[bulleted("outline")]),
            slide(0, &[text_box("first")]),
            slide(0, &[text_box("second")]),
        ];
        let bytes = deck(&[], &objects, OptionalStream::Absent, false);
        let rendered = rendered(&parse(&ParseCtx::default(), &bytes).unwrap());
        assert_eq!(spans(&rendered), [(Slide, 1, "first"), (Slide, 2, "second")]);
        let before = rendered.markdown.get(..rendered.units[0].bytes.start).unwrap();
        let at = |text: &str| before.find(text).unwrap();
        assert!(at("> note") < at("- outline"), "{before:?}");
    }

    #[test]
    fn ownerless_notes_precede_the_first_unit_and_keep_no_diagnostic_unit() {
        let objects = [slide(0, &[]), notes(999, &[text(&[0x61, 0xD800])])];
        let bytes = listed(&[pages(0, &[(2, 256)]), pages(2, &[(3, 300)])], &objects);
        let ctx = ParseCtx::default();
        let document = parse(&ctx, &bytes).unwrap();
        assert_matches!(document.blocks.as_slice(), [Block::BlockQuote(_), Block::UnitStart(_)]);
        assert!(
            ctx.diagnostics()
                .iter()
                .any(|d| d.code == DiagnosticCode::InvalidEncoding && d.unit.is_none())
        );
        let rendered = rendered(&document);
        assert_eq!(spans(&rendered), [(Slide, 1, "")]);
        assert!(rendered.markdown.starts_with("> a"));
    }

    fn font_entity(instance: u16, face: &[u8]) -> Vec<u8> {
        let mut body = face.to_vec();
        body.resize(64, 0);
        record(instance << 4, 0x0FB7, &[&body[..], &[0; 4]].concat())
    }

    fn environment(faces: &[&str]) -> Vec<u8> {
        let entities: Vec<Vec<u8>> =
            faces.iter().zip(0..).map(|(face, i)| font_entity(i, &utf16(face))).collect();
        container(0, 0x03F2, &[container(0, 0x07D5, &entities)])
    }

    const FONT: u32 = 0x1_0000;
    const POSITION: u32 = 0x8_0000;
    const SYMBOL_FONT: u32 = 0x80_0000;

    fn char_run(count: u32, mask: u32, fields: &[u16]) -> Vec<u8> {
        [words(&[count, mask]), halves(fields)].concat()
    }

    fn plain_run(count: u32) -> Vec<u8> {
        char_run(count, 0, &[])
    }

    /// A body text of `text`, which ends in its paragraph mark, with character `runs` that
    /// cover it and the implicit final mark.
    fn styled(text: &str, runs: &[Vec<u8>]) -> Vec<u8> {
        let units = u32::try_from(text.encode_utf16().count() + 1).unwrap();
        let paragraph = [&units.to_le_bytes()[..], &[0, 0], &0u32.to_le_bytes()].concat();
        [label(text), record(0, 0x0FA1, &[paragraph, runs.concat()].concat())].concat()
    }

    fn deck_with(faces: &[&str], content: Vec<u8>, persisted: bool) -> Vec<u8> {
        let doc = [environment(faces), pages(0, &[(2, 256)])];
        deck(&doc, &[slide(0, &[content])], OptionalStream::Absent, persisted)
    }

    fn markdown_of(bytes: &[u8]) -> String {
        crate::render::markdown::document_to_markdown(&parse(&ParseCtx::default(), bytes).unwrap())
    }

    const FACES: [&str; 3] = ["Arial", "Symbol", "Wingdings"];

    #[test]
    fn a_run_reads_through_the_symbol_font_its_references_name() {
        let cases = [
            (
                "a typeface naming Symbol",
                "10 mm\r",
                vec![plain_run(3), char_run(1, FONT, &[1]), plain_run(3)],
                "10 \u{3bc}m",
            ),
            (
                "a private use code through the symbol typeface",
                "3 \u{f06d}s\r",
                vec![plain_run(2), char_run(1, FONT | SYMBOL_FONT, &[0, 1]), plain_run(3)],
                "3 \u{3bc}s",
            ),
            (
                "a private use code through the typeface without a symbol one",
                "\u{f06d}\r",
                vec![char_run(1, FONT, &[1]), plain_run(2)],
                "\u{3bc}",
            ),
            (
                "a private use code beside a plain symbol typeface",
                "\u{f06d}m\r",
                vec![char_run(2, FONT | SYMBOL_FONT, &[1, 0]), plain_run(2)],
                "\u{f06d}\u{3bc}",
            ),
            (
                "a typeface reference past the collection",
                "m\r",
                vec![char_run(1, FONT, &[3]), plain_run(2)],
                "m",
            ),
            (
                "a symbol typeface reference past the collection",
                "\u{f06d}\r",
                vec![char_run(1, SYMBOL_FONT, &[0xFFFF]), plain_run(2)],
                "\u{f06d}",
            ),
            (
                "a symbol typeface reference past the collection beside a Symbol typeface",
                "\u{f06d}\r",
                vec![char_run(1, FONT | SYMBOL_FONT, &[1, 0xFFFF]), plain_run(2)],
                "\u{f06d}",
            ),
            (
                "a checkmark and a ballot box",
                "\u{fc}\u{fe}\r",
                vec![char_run(2, FONT, &[2]), plain_run(2)],
                "\u{2713}[x]",
            ),
            ("a plain typeface", "m\r", vec![char_run(1, FONT, &[0]), plain_run(2)], "m"),
        ];
        for (case, text, runs, expected) in cases {
            let bytes = deck_with(&FACES, styled(text, &runs), true);
            assert_eq!(markdown_of(&bytes), format!("{expected}\n"), "{case}");
            assert_eq!(reported(&bytes), Vec::<Reported>::new(), "{case}");
        }
    }

    #[test]
    fn a_vertical_tab_breaks_the_line_and_an_astral_character_fills_two_units_of_its_run() {
        let content = styled("\u{1F600}b\u{b}c\r", &[char_run(2, 1, &[1]), plain_run(5)]);
        let blocks = parse(&ParseCtx::default(), &deck_with(&FACES, content, true)).unwrap().blocks;
        let text = |text: &str, bold| Inline::Text {
            text: text.to_owned(),
            style: Style { bold, ..Style::PLAIN },
        };
        let line =
            vec![text("\u{1F600}", true), text("b", false), Inline::LineBreak, text("c", false)];
        assert_eq!(blocks[1], Block::Paragraph(line));
    }

    fn boundary_text(value: &str, bits: u8) -> Inline {
        Inline::Text {
            text: value.to_owned(),
            style: Style { bold: bits & 1 != 0, italic: bits & 2 != 0, ..Style::PLAIN },
        }
    }

    fn assert_run_boundaries(
        case: &str,
        source: &str,
        paragraphs: &[(usize, u16, Option<bool>)],
        chars: &[(usize, u8)],
        expected: &[(Vec<Inline>, u16, Option<bool>)],
    ) {
        let runs = StyleRuns {
            paragraphs: paragraphs
                .iter()
                .map(|&(count, depth, bullet)| styletext::ParaProps { count, depth, bullet })
                .collect(),
            chars: chars
                .iter()
                .map(|&(count, bits)| styletext::CharProps {
                    count,
                    style: styletext::CharStyle {
                        delta: StyleDelta {
                            bold: Some(bits & 1 != 0),
                            italic: Some(bits & 2 != 0),
                            ..StyleDelta::default()
                        },
                        ..styletext::CharStyle::default()
                    },
                })
                .collect(),
        };
        let ctx = ParseCtx::default();
        assert_eq!(
            Extractor::new(&ctx).paragraphs(source, &runs, &[]).as_slice(),
            expected,
            "{case}"
        );
        assert_eq!(
            diagnostic_rows::<Reported>(&ctx.diagnostics()),
            Vec::<Reported>::new(),
            "{case}"
        );
    }

    #[test]
    fn character_and_paragraph_runs_keep_independent_utf16_boundaries() {
        let cases = [
            (
                "independent boundaries",
                "abc\r",
                &[(2, 1, Some(true)), (3, 2, Some(false))][..],
                &[(1, 1), (3, 2), (1, 0)][..],
                vec![(vec![boundary_text("a", 1), boundary_text("bc", 2)], 2, Some(false))],
            ),
            (
                "astral width",
                "\u{1f600}a\rb",
                &[(3, 1, Some(true)), (4, 2, Some(false))],
                &[(2, 1), (4, 2)],
                vec![
                    (vec![boundary_text("\u{1f600}", 1), boundary_text("a", 2)], 2, Some(false)),
                    (vec![boundary_text("b", 2)], 2, Some(false)),
                ],
            ),
            (
                "astral overrun",
                "\u{1f600}ab",
                &[(1, 1, Some(true)), (1, 2, Some(false)), (4, 3, None)],
                &[(1, 1), (1, 2), (3, 0)],
                vec![(
                    vec![
                        boundary_text("\u{1f600}", 1),
                        boundary_text("a", 2),
                        boundary_text("b", 0),
                    ],
                    3,
                    None,
                )],
            ),
        ];
        for (case, source, paragraphs, chars, expected) in cases {
            assert_run_boundaries(case, source, paragraphs, chars, &expected);
        }
    }

    #[test]
    fn empty_and_exhausted_runs_preserve_their_fallbacks() {
        let cases = [
            (
                "initial empty runs",
                "ab\r",
                &[(0, 4, Some(true)), (4, 7, Some(false))][..],
                &[(0, 1), (4, 2)][..],
                vec![(vec![boundary_text("a", 1), boundary_text("b", 2)], 7, Some(false))],
            ),
            (
                "exhausted runs",
                "ab",
                &[(1, 2, Some(true))],
                &[(1, 1)],
                vec![(vec![boundary_text("a", 1), boundary_text("b", 0)], 0, None)],
            ),
            ("absent runs", "ab\r", &[], &[], vec![(vec![boundary_text("ab", 0)], 0, None)]),
            (
                "retained empty runs",
                "ab",
                &[(1, 1, Some(true)), (0, 4, Some(false)), (4, 5, Some(true))],
                &[(1, 1), (0, 2), (4, 0)],
                vec![(vec![boundary_text("a", 1), boundary_text("b", 2)], 5, Some(true))],
            ),
            ("empty source", "", &[(0, 4, Some(true))], &[(0, 1)], vec![]),
        ];
        for (case, source, paragraphs, chars, expected) in cases {
            assert_run_boundaries(case, source, paragraphs, chars, &expected);
        }
    }

    #[test]
    fn paragraph_marks_and_line_breaks_advance_their_own_runs() {
        let cases = [
            (
                "unterminated tail",
                "a",
                &[(1, 2, Some(true)), (2, 5, Some(false))][..],
                &[(2, 1)][..],
                vec![(vec![boundary_text("a", 1)], 5, Some(false))],
            ),
            (
                "paragraph mark",
                "a\r",
                &[(2, 2, Some(true)), (2, 5, Some(false))],
                &[(3, 1)],
                vec![(vec![boundary_text("a", 1)], 2, Some(true))],
            ),
            (
                "line break",
                "a\u{b}bc\r",
                &[(1, 2, Some(true)), (5, 5, Some(false))],
                &[(2, 1), (4, 2)],
                vec![(
                    vec![boundary_text("a", 1), Inline::LineBreak, boundary_text("bc", 2)],
                    5,
                    Some(false),
                )],
            ),
        ];
        for (case, source, paragraphs, chars, expected) in cases {
            assert_run_boundaries(case, source, paragraphs, chars, &expected);
        }
    }

    #[test]
    fn a_deck_without_a_font_collection_names_no_face() {
        let content = styled("m\r", &[char_run(1, FONT, &[1]), plain_run(2)]);
        let bytes = on_slide(&[content]);
        assert_eq!(markdown_of(&bytes), "m\n");
        assert_eq!(reported(&bytes), Vec::<Reported>::new());
    }

    #[test]
    fn a_recovered_deck_reads_its_font_collection() {
        let content = styled("m\r", &[char_run(1, FONT, &[1]), plain_run(2)]);
        let bytes = deck_with(&FACES, content, false);
        assert_eq!(markdown_of(&bytes), "\u{3bc}\n");
    }

    #[test]
    fn a_font_reference_indexes_the_entries_in_order_whatever_their_instances() {
        let blob = record(0, 0x0FB8, b"embedded");
        let entities = [
            font_entity(5, &utf16("Arial")),
            blob.clone(),
            font_entity(2, &utf16("Symbol")),
            blob,
            font_entity(2, &utf16("Wingdings")),
        ];
        let fonts = container(0, 0x03F2, &[container(0, 0x07D5, &entities)]);
        let doc = [fonts, pages(0, &[(2, 256)])];
        let references = |font: u16| {
            let content = styled("\u{fc}\r", &[char_run(1, FONT, &[font]), plain_run(2)]);
            markdown_of(&listed(&doc, &[slide(0, &[content])]))
        };
        assert_eq!(references(0), "\u{fc}\n");
        assert_eq!(references(1), "\u{23ab}\n");
        assert_eq!(references(2), "\u{2713}\n");
        assert_eq!(references(3), "\u{fc}\n");
        assert_eq!(references(5), "\u{fc}\n");
    }

    #[test]
    fn a_face_name_ends_at_its_first_nul_or_the_end_of_its_entry() {
        let named = |entity: Vec<u8>| {
            let fonts = container(0, 0x03F2, &[container(0, 0x07D5, &[entity])]);
            let doc = [fonts, pages(0, &[(2, 256)])];
            let content = styled("m\r", &[char_run(1, FONT, &[0]), plain_run(2)]);
            markdown_of(&listed(&doc, &[slide(0, &[content])]))
        };
        let nul_then_text = [utf16("Symbol"), vec![0, 0], utf16("Arial")].concat();
        assert_eq!(named(font_entity(0, &nul_then_text)), "\u{3bc}\n");
        assert_eq!(named(record(0, 0x0FB7, &utf16("Symbol"))), "\u{3bc}\n");
        assert_eq!(named(font_entity(0, &utf16("Symbolx"))), "m\n");
        assert_eq!(named(font_entity(0, &[])), "m\n");
        assert_eq!(named(font_entity(0, &utf16(&"Symbol".repeat(6)))), "m\n");
        assert_eq!(named(record(0, 0x0FB7, &[])), "m\n");
    }

    #[test]
    fn a_position_raises_or_lowers_a_run_and_zero_cancels_the_masters() {
        let level = [&[1, 0][..], &[0; 4], &POSITION.to_le_bytes(), &30i16.to_le_bytes()].concat();
        let master =
            container(0, 0x03F8, &[atom32(0x03EF, &[0; 6]), record(1 << 4, 0x0FA3, &level)]);
        let runs = [
            plain_run(1),
            char_run(1, POSITION, &[0]),
            char_run(1, POSITION, &[(-25i16).cast_unsigned()]),
            char_run(1, POSITION, &[1]),
            plain_run(2),
        ];
        let body = styled("abcd\r", &runs);
        let converted = |master: Vec<u8>| {
            let doc = [pages(0, &[(2, 256)]), pages(1, &[(3, 1)])];
            let objects = [slide(0, std::slice::from_ref(&body)), master];
            markdown_of(&listed(&doc, &objects))
        };
        assert_eq!(converted(master), "<sup>a</sup>b<sub>c</sub><sup>d</sup>\n");
        let unstyled = container(0, 0x03F8, &[atom32(0x03EF, &[0; 6])]);
        assert_eq!(converted(unstyled), "ab<sub>c</sub><sup>d</sup>\n");
    }

    #[test]
    fn a_master_levels_font_references_are_inherited_by_a_run_and_replaced_by_its_own() {
        let converted = |mask: u32, refs: &[u16], text: &str, runs: &[Vec<u8>]| {
            let level =
                [&[1, 0][..], &[0; 4], &mask.to_le_bytes(), &char_run(0, 0, refs)[8..]].concat();
            let master =
                container(0, 0x03F8, &[atom32(0x03EF, &[0; 6]), record(1 << 4, 0x0FA3, &level)]);
            let doc = [environment(&FACES), pages(0, &[(2, 256)]), pages(1, &[(3, 1)])];
            let objects = [slide(0, &[styled(text, runs)]), master];
            markdown_of(&listed(&doc, &objects))
        };
        let typeface =
            converted(FONT, &[1], "mm\r", &[plain_run(1), char_run(1, FONT, &[0]), plain_run(2)]);
        assert_eq!(typeface, "\u{3bc}m\n");
        let symbol = converted(
            FONT | SYMBOL_FONT,
            &[0, 1],
            "\u{f06d}\u{f06d}\u{f06d}\r",
            &[plain_run(1), char_run(1, SYMBOL_FONT, &[0]), char_run(1, FONT, &[1]), plain_run(2)],
        );
        assert_eq!(symbol, "\u{3bc}\u{f06d}\u{3bc}\n");
    }

    #[test]
    fn runs_split_where_the_symbol_font_reading_them_differs_and_not_where_it_does_not() {
        let runs = [
            char_run(1, FONT, &[1]),
            char_run(1, FONT, &[0]),
            plain_run(1),
            char_run(1, SYMBOL_FONT, &[0]),
            plain_run(2),
        ];
        let bytes = deck_with(&FACES, styled("mmmm\r", &runs), true);
        let blocks = parse(&ParseCtx::default(), &bytes).unwrap().blocks;
        let text = |text: &str| Inline::Text { text: text.to_owned(), style: Style::PLAIN };
        assert_eq!(blocks[1], Block::Paragraph(vec![text("\u{3bc}"), text("mmm")]));
    }

    #[test]
    fn a_code_no_symbol_table_maps_is_reported_in_its_unit_and_region() {
        let body = || styled("\u{fc}\u{ff}\r", &[char_run(2, FONT, &[2]), plain_run(2)]);
        let doc = [environment(&FACES), pages(0, &[(2, 256)]), pages(2, &[(3, 300)])];
        let objects = [slide(0, &[body()]), notes(256, &[body()])];
        let bytes = listed(&doc, &objects);
        assert_eq!(
            reported(&bytes),
            [
                (DiagnosticCode::UnmappedSymbol, Region::Body, Some(1), 1),
                (DiagnosticCode::UnmappedSymbol, Region::Note, Some(1), 1)
            ]
        );
        assert_eq!(markdown_of(&bytes), "\u{2713}\n\n> \u{2713}\n");
    }
}
