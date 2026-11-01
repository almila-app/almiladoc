//! Section headers and footers: `PlcfSed` ([MS-DOC] 2.8.26) bounds the sections and points at
//! each `Sepx` (2.9.253), whose `sprmSFTitlePage` (2.6.4) turns on its first-page variant;
//! `Dop.fFacingPages` (2.7.2 `DopBase`) turns on the even-page ones; `PlcfHdd` (2.8.10) holds
//! six stories per section after the six separator stories, an empty one inheriting the story
//! the section before it shows.

use super::assemble::Assembler;
use super::{Fib, Plc, Stories, read_plc, sprm};
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Block, HeaderFooterRegion as Side, HeaderFooterVariant as Variant};
use crate::shared::binary::{get_u16, get_u32};
use crate::shared::sides::{SideStory, SideTimeline};
use std::collections::HashMap;
use std::ops::Range;

const SED_SIZE: usize = 12;
const SED_FC_SEPX: usize = 2;
const NO_SEPX: u32 = 0xFFFF_FFFF;
const SPRM_S_F_TITLE_PAGE: u16 = 0x300A;
const F_FACING_PAGES: u8 = 0x01;
const SEPARATOR_STORIES: usize = 6;
const STORIES_PER_SECTION: usize = 6;
const SLOTS: [(Side, [usize; 3]); 2] = [(Side::Header, [0, 1, 4]), (Side::Footer, [2, 3, 5])];

const FLD_SIZE: usize = 2;
const FLD_CH: u8 = 0x1F;
const FIELD_BEGIN: u8 = 0x13;
const FIELD_SEPARATOR: u8 = 0x14;
const FIELD_END: u8 = 0x15;
const F_LOCKED: u8 = 0x10;

#[derive(Default)]
pub(super) enum FieldLocks<'t> {
    #[default]
    Absent,
    Unpaired {
        table: Plc<'t>,
        hdd: Plc<'t>,
        headers: Range<usize>,
    },
    Read {
        headers: Range<usize>,
        fields: HashMap<usize, (usize, bool)>,
    },
    Unread,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Lock {
    Unlocked,
    Locked,
    Unread,
}

#[derive(Default)]
struct StoryFields {
    open: Vec<usize>,
    paired: Vec<(usize, usize, bool)>,
    broken: bool,
}

impl StoryFields {
    fn commit(&mut self, base: usize, fields: &mut HashMap<usize, (usize, bool)>) {
        let Self { open, paired, broken } = std::mem::take(self);
        if !broken && open.is_empty() {
            fields.extend(paired.into_iter().map(|(b, e, locked)| (base + b, (base + e, locked))));
        }
    }
}

impl<'t> FieldLocks<'t> {
    /// The field table of the header document, read without pairing: `PlcfFldHdr`, a [MS-DOC] `PlcFld` holding a `Fld` for each field delimiter, whose last CP locates nothing.
    fn of(layout: &Layout<'t>, hdd: Option<Plc<'t>>, headers: &Range<usize>) -> Self {
        let Some(hdd) = hdd else { return Self::Absent };
        match read_plc(layout.fib, layout.table, Fib::FC_PLCF_FLD_HDR, FLD_SIZE) {
            None => Self::Unread,
            Some(table) if table.n == 0 => Self::Absent,
            Some(table) => Self::Unpaired { table, hdd, headers: headers.clone() },
        }
    }

    pub(super) fn pair(&mut self, parse_ctx: &ParseCtx<'_>) -> Result<(), ConvertError> {
        if let Self::Unpaired { table, hdd, headers } = self {
            let paired = pair_fields(parse_ctx, table, hdd, headers)?;
            *self = paired;
        }
        Ok(())
    }
}

fn pair_fields<'t>(
    parse_ctx: &ParseCtx<'_>,
    plc: &Plc<'_>,
    hdd: &Plc<'_>,
    headers: &Range<usize>,
) -> Result<FieldLocks<'t>, ConvertError> {
    let (mut fields, mut story, mut held) = (HashMap::new(), 0, StoryFields::default());
    for i in 0..plc.n {
        parse_ctx.charge(1)?;
        let cp = plc.cp(i);
        let Some(&[fldch, grffld]) = plc.data(i).first_chunk::<FLD_SIZE>() else {
            return Ok(FieldLocks::Unread);
        };
        if i > 0 && cp <= plc.cp(i - 1) {
            return Ok(FieldLocks::Unread);
        }
        while story < hdd.n && hdd.cp(story + 1) <= cp {
            parse_ctx.charge(1)?;
            held.commit(headers.start, &mut fields);
            story += 1;
        }
        match fldch & FLD_CH {
            FIELD_BEGIN => held.open.push(cp),
            FIELD_SEPARATOR if !held.open.is_empty() => {}
            FIELD_END => match held.open.pop() {
                Some(begin) => held.paired.push((begin, cp, grffld & F_LOCKED != 0)),
                None => held.broken = true,
            },
            _ => held.broken = true,
        }
    }
    held.commit(headers.start, &mut fields);
    Ok(FieldLocks::Read { headers: headers.clone(), fields })
}

pub(super) struct Layout<'d> {
    pub(super) fib: Fib<'d>,
    pub(super) table: &'d [u8],
    pub(super) word_doc: &'d [u8],
    pub(super) stories: &'d Stories,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Setting {
    Off,
    On,
    Unread,
}

impl Setting {
    fn of(on: bool) -> Self {
        if on { Self::On } else { Self::Off }
    }
}

fn sections(
    parse_ctx: &ParseCtx<'_>,
    layout: &Layout<'_>,
) -> Result<Vec<(Range<usize>, Setting)>, ConvertError> {
    let main = layout.stories.main();
    let Some(sed) = read_plc(layout.fib, layout.table, Fib::FC_PLCF_SED, SED_SIZE) else {
        return Ok(vec![(main, Setting::Unread)]);
    };
    if sed.n == 0 {
        return Ok(vec![(main, Setting::Off)]);
    }
    let mut read: HashMap<u32, Setting> = HashMap::new();
    let mut sections = Vec::with_capacity(sed.n);
    let mut start = 0;
    for i in 0..sed.n {
        parse_ctx.charge(1)?;
        let end = if i + 1 == sed.n { main.end } else { sed.cp(i + 1).clamp(start, main.end) };
        let title = match get_u32(sed.data(i), SED_FC_SEPX) {
            None => Setting::Unread,
            Some(NO_SEPX) => Setting::Off,
            Some(fc) => {
                if let Some(&setting) = read.get(&fc) {
                    setting
                } else {
                    let setting = title_page(parse_ctx, layout.word_doc, fc as usize)?;
                    read.insert(fc, setting);
                    setting
                }
            }
        };
        sections.push((start..end, title));
        start = end;
    }
    Ok(sections)
}

fn title_page(
    parse_ctx: &ParseCtx<'_>,
    word_doc: &[u8],
    at: usize,
) -> Result<Setting, ConvertError> {
    let Some(cb) = get_u16(word_doc, at).map(usize::from) else { return Ok(Setting::Unread) };
    let Some(grpprl) = word_doc.get(at + 2..).and_then(|rest| rest.get(..cb)) else {
        return Ok(Setting::Unread);
    };
    parse_ctx.charge(grpprl.len() as u64)?;
    Ok(Setting::of(sprm::sets_flag(grpprl, SPRM_S_F_TITLE_PAGE)))
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Slot {
    Empty,
    Text(Range<usize>),
    Unread,
}

pub(super) fn body<'w>(
    parse_ctx: &ParseCtx<'_>,
    layout: &Layout<'w>,
    assembler: &Assembler<'_, 'w>,
) -> Result<Vec<Block>, ConvertError> {
    let main = layout.stories.main();
    if !parse_ctx.headers_footers_requested() || layout.stories.hdd == 0 {
        return Ok(assembler.build_blocks(&main, &main, Region::Body)?.unwrap_or_default());
    }
    let sections = sections(parse_ctx, layout)?;
    parse_ctx.charge(sections.len() as u64)?;
    let facing = match layout.fib.table_range(layout.table, Fib::FC_DOP) {
        Some(dop) => {
            dop.first().map_or(Setting::Unread, |&first| Setting::of(first & F_FACING_PAGES != 0))
        }
        None => Setting::Unread,
    };
    let hdd = read_plc(layout.fib, layout.table, Fib::FC_PLCF_HDD, 0);
    if hdd.is_none() {
        parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Header);
    }
    let headers = layout.stories.headers();
    assembler.field_locks.replace(FieldLocks::of(layout, hdd, &headers));
    let mut publisher = Publisher {
        parse_ctx,
        assembler,
        headers: headers.clone(),
        read: HashMap::new(),
        timeline: SideTimeline::default(),
        blocks: Vec::new(),
    };
    let mut shown: [Option<Slot>; STORIES_PER_SECTION] = Default::default();
    for (index, (range, title)) in sections.into_iter().enumerate() {
        let base = SEPARATOR_STORIES + index * STORIES_PER_SECTION;
        for (k, held) in shown.iter_mut().enumerate() {
            parse_ctx.charge(1)?;
            match slot_at(hdd.as_ref(), &headers, base + k) {
                Slot::Empty => {}
                named => *held = Some(named),
            }
        }
        publisher.section(&shown, (title, facing))?;
        if let Some(body) = assembler.build_blocks(&main, &range, Region::Body)? {
            publisher.blocks.extend(body);
        }
    }
    Ok(publisher.blocks)
}

fn slot_at(hdd: Option<&Plc<'_>>, headers: &Range<usize>, index: usize) -> Slot {
    let Some(hdd) = hdd.filter(|hdd| index < hdd.n) else { return Slot::Empty };
    let (from, to) = (hdd.cp(index), hdd.cp(index + 1));
    let span = headers.start.saturating_add(from)..headers.start.saturating_add(to);
    match from.cmp(&to) {
        std::cmp::Ordering::Equal => Slot::Empty,
        std::cmp::Ordering::Less if span.end <= headers.end => Slot::Text(span),
        _ => Slot::Unread,
    }
}

/// The blocks so far, the stories read once each, and the values in force.
struct Publisher<'p, 'c, 'w> {
    parse_ctx: &'p ParseCtx<'p>,
    assembler: &'p Assembler<'c, 'w>,
    headers: Range<usize>,
    read: HashMap<(Slot, Side, Variant), SideStory>,
    timeline: SideTimeline,
    blocks: Vec<Block>,
}

impl Publisher<'_, '_, '_> {
    fn section(
        &mut self,
        shown: &[Option<Slot>; STORIES_PER_SECTION],
        (title, facing): (Setting, Setting),
    ) -> Result<(), ConvertError> {
        let Self { parse_ctx, assembler, headers, read, timeline, blocks } = self;
        for (side, [even, odd, first]) in SLOTS {
            let defaulted = shown.get(odd).is_some_and(Option::is_some);
            for (variant, at, setting) in [
                (Variant::Default, odd, Setting::On),
                (Variant::First, first, title),
                (Variant::Even, even, facing),
            ] {
                let chosen = shown.get(at).cloned().flatten();
                let known;
                let story = match (setting, chosen) {
                    (Setting::Off, _) | (Setting::Unread, None) => {
                        known = SideStory::known(side, variant, None);
                        &known
                    }
                    (_, Some(slot)) => {
                        let key = (slot, side, variant);
                        if !read.contains_key(&key) {
                            let story = read_story(
                                parse_ctx,
                                assembler,
                                headers,
                                (side, variant),
                                &key.0,
                                setting,
                            )?;
                            read.insert(key.clone(), story);
                        }
                        read.get(&key).ok_or(ConvertError::Internal)?
                    }
                    (Setting::On, None) => {
                        let blank = variant == Variant::Default || defaulted;
                        known = SideStory::known(side, variant, blank.then(Vec::new));
                        &known
                    }
                };
                timeline.publish(parse_ctx, story, blocks)?;
            }
        }
        Ok(())
    }
}

fn read_story(
    parse_ctx: &ParseCtx<'_>,
    assembler: &Assembler<'_, '_>,
    headers: &Range<usize>,
    (side, variant): (Side, Variant),
    slot: &Slot,
    setting: Setting,
) -> Result<SideStory, ConvertError> {
    let region = side.region();
    SideStory::read(parse_ctx, side, variant, || {
        if setting == Setting::Unread {
            parse_ctx.emit(DiagnosticCode::PartUnreadable, region);
        }
        match slot {
            Slot::Text(text) => {
                Ok(Some(assembler.side_blocks(headers, text, region)?.unwrap_or_default()))
            }
            Slot::Unread => {
                parse_ctx.emit(DiagnosticCode::PartUnreadable, region);
                Ok(None)
            }
            Slot::Empty => Ok(Some(Vec::new())),
        }
    })
}
