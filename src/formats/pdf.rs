use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::num::NonZeroU32;
use std::ops::ControlFlow;

use unpdf::model::{ListStyle, NumberStyle};
use unpdf::parser::backend::TextSuppression;
#[cfg(feature = "raster")]
use unpdf::parser::raster::{PageRegion, PageSize};
use unpdf::{
    Block as PdfBlock, ErrorMode, InlineContent, Page, PageSelection, PageStreamOptions, Paragraph,
    ParseEvent, PdfParser, Table as PdfTable, TextStyle,
};

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::inspect::PageVerdict;
use crate::limits::Limit;
use crate::model::HeaderFooterRegion as Side;
use crate::model::{
    Block, Cell, Document, Gap, GapId, GapReason, GridBuilder, Inline, List, ListItem, MarkerKind,
    OcrOrigin, Style, UnitKind, VerticalPosition,
};
use crate::shared::fields::classify_rel_target;
use crate::shared::sides::{PageSides, SideTimeline};

const SCAN_COVERAGE: f32 = 0.9;
const PARTIAL_SCAN_COVERAGE: f32 = 0.5;
const HALF_PAGE: f32 = 0.5;
const RUNNING_HEAD_CHARS: usize = 80;
const GARBLED_MIN_CODES: u64 = 40;
const GARBLED_LOST_PER_PAINTED: u64 = 2;

pub(crate) struct Read {
    pub(crate) document: Document,
    pub(crate) verdicts: Vec<PageVerdict>,
    #[cfg(feature = "raster")]
    pub(crate) source: PdfSource,
}

#[cfg(feature = "raster")]
pub(crate) struct PdfSource {
    pub(crate) parser: PdfParser,
    pub(crate) boxes: Vec<Option<PageBox>>,
}

#[cfg(feature = "raster")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PageBox {
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[cfg(feature = "raster")]
impl PdfSource {
    pub(crate) fn page_box(&self, page: NonZeroU32) -> Option<PageBox> {
        *self.boxes.get(usize::try_from(page.get() - 1).ok()?)?
    }

    pub(crate) fn page_count(&self) -> usize {
        self.boxes.len()
    }

    pub(crate) fn displayed_size(&self, page: NonZeroU32) -> Option<PageSize> {
        self.parser.page_size(page.get(), PageRegion::Crop).ok()
    }
}

#[cfg(feature = "raster")]
impl PageBox {
    fn of(page: &Page) -> Self {
        Self { width: page.width, height: page.height }
    }
}

pub(crate) fn read(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Read, ConvertError> {
    let Extracted { pages, missing, parser } = extract(parse_ctx, bytes)?;
    let furniture = Furniture::of(parse_ctx, &pages)?;
    let mut sides = parse_ctx.headers_footers_requested().then(SideTimeline::default);
    let mut document = Document::default();
    let mut verdicts = Vec::with_capacity(pages.len());
    for (index, (page, &furnished)) in pages.iter().zip(&furniture.pages).enumerate() {
        let scope = parse_ctx.begin_unit(UnitKind::Page, index)?;
        let unit = scope.unit().model();
        document.blocks.push(Block::UnitStart(unit));
        let verdict = page_verdict(page.as_ref(), furnished, parse_ctx.scans_by_coverage());
        let verdict =
            if parse_ctx.ocr_requested(unit.ordinal) { requested(verdict) } else { verdict };
        if let PageVerdict::NeedsOcr { reasons } = &verdict {
            let id = GapId::new(Some(unit), NonZeroU32::MIN);
            let origin = OcrOrigin::Page(unit.ordinal);
            document.blocks.push(Block::Gap(Gap { id, origin, reasons: reasons.clone() }));
        } else if let Some(read) = page {
            let figures = u64::from(read.page.image_op_count);
            parse_ctx.emit_count(DiagnosticCode::ImageOmitted, Region::Body, figures);
            garbled(parse_ctx, &read.page);
            left_out(parse_ctx, &read.page);
            map_page(parse_ctx, read, &furniture, sides.as_mut(), &mut document.blocks)?;
        }
        verdicts.push(verdict);
    }
    parse_ctx.emit_count(DiagnosticCode::UnitUnreadable, Region::Body, missing);
    #[cfg(feature = "raster")]
    let source = PdfSource {
        boxes: pages.iter().map(|read| read.as_ref().map(|read| PageBox::of(&read.page))).collect(),
        parser,
    };
    #[cfg(not(feature = "raster"))]
    drop(parser);
    Ok(Read {
        document,
        verdicts,
        #[cfg(feature = "raster")]
        source,
    })
}

struct Extracted {
    pages: Vec<Option<ReadPage>>,
    missing: u64,
    parser: PdfParser,
}

struct ReadPage {
    page: Page,
    texts: Vec<Text>,
}

struct Text {
    index: usize,
    key: String,
    numbers: Vec<String>,
    chars: usize,
    running_head: bool,
}

fn extract(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Extracted, ConvertError> {
    let parser = PdfParser::from_bytes(bytes).map_err(map_error)?;
    let page_count = parser.page_count();
    if page_count == 0 {
        return Err(ConvertError::malformed("PDF has no pages"));
    }
    let max = parse_ctx.limits().max_pages;
    if u64::from(page_count) > max {
        return Err(ConvertError::limit(Limit::Pages, max));
    }
    parse_ctx.admit_ocr_pages(page_count)?;
    let mut pages = Vec::new();
    pages.resize_with(usize::try_from(page_count).unwrap_or(usize::MAX), || None);
    let mut failed = Ok(());
    let quality = parser
        .for_each_page(sequential(), |event| {
            let page = match event {
                ParseEvent::PageParsed(page) => Some(page),
                ParseEvent::PageFailed { .. } => None,
                _ => return ControlFlow::Continue(()),
            };
            failed = parse_ctx
                .charge(1)
                .and_then(|()| page.map_or(Ok(()), |page| keep(parse_ctx, page, &mut pages)));
            if failed.is_err() { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
        })
        .map_err(map_error)?;
    failed?;
    let missing = if quality.pages_incomplete {
        u64::from(quality.declared_page_count.unwrap_or(0).saturating_sub(page_count)).max(1)
    } else {
        0
    };
    Ok(Extracted { pages, missing, parser })
}

fn keep(
    parse_ctx: &ParseCtx<'_>,
    page: Page,
    pages: &mut [Option<ReadPage>],
) -> Result<(), ConvertError> {
    let mut texts = Vec::new();
    for (index, block) in page.elements.iter().enumerate() {
        parse_ctx.charge(1)?;
        let Some((text, plain)) = text_of(block) else { continue };
        let retained = text.len().saturating_add(link_bytes(block));
        parse_ctx.charge(u64::try_from(retained).unwrap_or(u64::MAX))?;
        let chars = text.chars().filter(|c| !c.is_whitespace()).count();
        if chars > 0 {
            let running_head = plain && chars <= RUNNING_HEAD_CHARS;
            let (key, numbers) = (furniture_key(&text), numbers(&text));
            texts.push(Text { index, key, numbers, chars, running_head });
        }
    }
    let slot = page.number.checked_sub(1).and_then(|i| usize::try_from(i).ok());
    if let Some(slot) = slot.and_then(|i| pages.get_mut(i)) {
        *slot = Some(ReadPage { page, texts });
    }
    Ok(())
}

fn sequential() -> PageStreamOptions {
    PageStreamOptions {
        error_mode: ErrorMode::Lenient,
        extract_text: true,
        extract_resources: false,
        min_image_dimension: 64,
        pages: PageSelection::All,
        password: None,
        parallel: false,
        suppress_low_confidence_ocr: true,
        window_size: 1,
        emit_progress_every: u32::MAX,
    }
}

fn text_of(block: &PdfBlock) -> Option<(String, bool)> {
    match block {
        PdfBlock::Paragraph(paragraph) => {
            Some((paragraph.plain_text(), paragraph.style.heading_level.is_none()))
        }
        PdfBlock::Table(table) => Some((table.plain_text(), false)),
        PdfBlock::Code { content } => Some((content.clone(), false)),
        PdfBlock::Raw { content } => Some((content.clone(), true)),
        _ => None,
    }
}

fn link_bytes(block: &PdfBlock) -> usize {
    let (paragraph, table) = match block {
        PdfBlock::Paragraph(paragraph) => (Some(paragraph), None),
        PdfBlock::Table(table) => (None, Some(table)),
        _ => (None, None),
    };
    let cells = table
        .into_iter()
        .flat_map(|table| &table.rows)
        .flat_map(|row| &row.cells)
        .flat_map(|cell| &cell.content);
    paragraph
        .into_iter()
        .chain(cells)
        .flat_map(|paragraph| &paragraph.content)
        .map(|inline| match inline {
            InlineContent::Link { url, .. } => url.len(),
            _ => 0,
        })
        .sum()
}

fn furniture_key(text: &str) -> String {
    let kept: String =
        text.chars().filter(|c| !c.is_ascii_digit()).flat_map(char::to_lowercase).collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn numbers(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .map(|run| {
            let value = run.trim_start_matches('0');
            if value.is_empty() { "0" } else { value }.to_owned()
        })
        .collect()
}

fn digits(numbers: &[String]) -> usize {
    numbers.iter().map(String::len).sum()
}

struct Furniture {
    running: BTreeSet<String>,
    pages: Vec<Furnished>,
}

#[derive(Clone, Copy, Default)]
struct Furnished {
    all_repeated: bool,
    all_running: bool,
}

#[derive(Default)]
struct Seen<'a> {
    agreeing: Vec<Sighting<'a>>,
    clashing: Vec<usize>,
}

struct Sighting<'a> {
    slot: usize,
    numbers: &'a [String],
    beside_other_text: bool,
}

impl Seen<'_> {
    fn slots(&self) -> impl Iterator<Item = usize> + '_ {
        self.agreeing.iter().map(|sighting| sighting.slot).chain(self.clashing.iter().copied())
    }

    fn weight(&self) -> usize {
        let numbers: usize = self.agreeing.iter().map(|sighting| digits(sighting.numbers)).sum();
        numbers + self.agreeing.len() + self.clashing.len()
    }
}

impl Furniture {
    fn of(parse_ctx: &ParseCtx<'_>, pages: &[Option<ReadPage>]) -> Result<Self, ConvertError> {
        let mut seen: BTreeMap<&str, Seen<'_>> = BTreeMap::new();
        let mut furnished = vec![Furnished::default(); pages.len()];
        let mut bodied = vec![false; pages.len()];
        for (slot, read) in pages.iter().enumerate() {
            let Some(read) = read else { continue };
            let agrees = index_page(parse_ctx, slot, read, &mut seen)?;
            let plain = read.texts.iter().all(|text| text.running_head);
            furnished[slot] = Furnished { all_repeated: true, all_running: agrees && plain };
            bodied[slot] = !plain;
        }
        let text_pages = pages.iter().flatten().filter(|read| !read.texts.is_empty()).count();
        let mut candidates = Vec::new();
        for (key, on) in &seen {
            parse_ctx.charge(u64::try_from(on.weight()).unwrap_or(u64::MAX))?;
            let agreeing = on.agreeing.len();
            let recurs = agreeing >= 2
                && agreeing * 2 >= text_pages
                && numbers_hold(&on.agreeing, key.is_empty());
            let beside = on.agreeing.iter().filter(|sighting| sighting.beside_other_text).count();
            let candidate = recurs && beside >= 2;
            for slot in on.slots() {
                furnished[slot].all_repeated &= agreeing + on.clashing.len() >= 2;
                furnished[slot].all_running &= recurs;
                bodied[slot] |= !candidate;
            }
            if candidate {
                candidates.push((*key, on));
            }
        }
        let running = candidates
            .into_iter()
            .filter(|(_, on)| on.slots().any(|slot| bodied[slot]))
            .map(|(key, _)| key.to_owned())
            .collect();
        Ok(Self { running, pages: furnished })
    }

    fn strips(&self, text: &Text) -> bool {
        text.running_head && self.running.contains(&text.key)
    }
}

fn index_page<'a>(
    parse_ctx: &ParseCtx<'_>,
    slot: usize,
    read: &'a ReadPage,
    seen: &mut BTreeMap<&'a str, Seen<'a>>,
) -> Result<bool, ConvertError> {
    let mut first: BTreeMap<&str, (&Text, bool)> = BTreeMap::new();
    for text in &read.texts {
        let traversed = text.key.len() + digits(&text.numbers);
        parse_ctx.charge(u64::try_from(traversed).unwrap_or(u64::MAX))?;
        match first.entry(text.key.as_str()) {
            Entry::Vacant(entry) => {
                entry.insert((text, false));
            }
            Entry::Occupied(mut entry) => {
                let (kept, clashes) = entry.get_mut();
                *clashes |= kept.numbers != text.numbers;
            }
        }
    }
    let beside_other_text = first.len() > 1;
    let agrees = first.values().all(|&(_, clashes)| !clashes);
    for (key, (text, clashes)) in first {
        let on = seen.entry(key).or_default();
        if clashes {
            on.clashing.push(slot);
        } else {
            on.agreeing.push(Sighting { slot, numbers: &text.numbers, beside_other_text });
        }
    }
    Ok(agrees)
}

fn numbers_hold(on: &[Sighting<'_>], folio: bool) -> bool {
    let Some((first, rest)) = on.split_first() else { return false };
    if rest.iter().any(|sighting| sighting.numbers.len() != first.numbers.len()) {
        return false;
    }
    first.numbers.iter().enumerate().all(|(at, number)| {
        let offset = |sighting: &Sighting<'_>| {
            let value: i128 = sighting.numbers.get(at)?.parse().ok()?;
            value.checked_sub(i128::try_from(sighting.slot).ok()?)
        };
        let stepping =
            || offset(first).is_some_and(|step| rest.iter().all(|on| offset(on) == Some(step)));
        rest.iter().all(|sighting| sighting.numbers.get(at) == Some(number))
            || (folio && stepping())
    })
}

fn page_verdict(page: Option<&ReadPage>, furnished: Furnished, by_coverage: bool) -> PageVerdict {
    page.map_or_else(
        || PageVerdict::NeedsOcr { reasons: vec![GapReason::UnreadableContent] },
        |read| verdict(read, furnished, by_coverage),
    )
}

fn verdict(read: &ReadPage, furnished: Furnished, by_coverage: bool) -> PageVerdict {
    let text_chars = read.texts.iter().map(|text| text.chars).sum();
    let reasons = reasons(&read.page, text_chars, scanned(&read.page, furnished, by_coverage));
    if !reasons.is_empty() {
        PageVerdict::NeedsOcr { reasons }
    } else if text_chars == 0 {
        PageVerdict::Blank
    } else {
        PageVerdict::Text
    }
}

fn scanned(page: &Page, furnished: Furnished, by_coverage: bool) -> bool {
    if by_coverage && page.image_coverage >= HALF_PAGE {
        return true;
    }
    let margin_only =
        page.painted_text_chars > 0 && page.margin_text_chars == page.painted_text_chars;
    let furniture_only = if page.image_coverage >= SCAN_COVERAGE {
        furnished.all_repeated
    } else {
        furnished.all_running
    };
    page.image_coverage >= PARTIAL_SCAN_COVERAGE && (margin_only || furniture_only)
}

fn reasons(page: &Page, text_chars: usize, scanned: bool) -> Vec<GapReason> {
    let invisible = u64::from(page.invisible_text_chars);
    let shown = invisible + u64::from(page.painted_text_chars);
    let textless = text_chars == 0;
    let mut reasons: Vec<GapReason> = [
        (GapReason::UnreadableContent, page.undecodable_content_streams > 0),
        (
            GapReason::InvisibleText,
            page.ocr_text_suppressed
                || (shown > 0 && invisible * 10 >= shown * 9)
                || (textless && page.clipped_text_runs > 0),
        ),
        (
            GapReason::GarbledText,
            (page.suppressed_text_runs > 0 && textless) || mostly_garbled(page),
        ),
        (GapReason::ImageBacked, page.image_op_count > 0 && (textless || scanned)),
        (GapReason::VectorDrawn, textless && page.image_op_count == 0 && page.fill_op_count > 0),
    ]
    .into_iter()
    .filter_map(|(reason, holds)| holds.then_some(reason))
    .collect();
    let blank_runs_only =
        page.blank_text_runs > 0 && page.clipped_text_runs == 0 && page.suppressed_text_runs == 0;
    let folio_only =
        page.omitted_page_number_chars > 0 && u64::from(page.omitted_page_number_chars) >= shown;
    if reasons.is_empty() && textless && page.text_op_count > 0 && !blank_runs_only && !folio_only {
        reasons.push(GapReason::UnreadableText);
    }
    reasons
}

fn mostly_garbled(page: &Page) -> bool {
    let unmapped: u64 = page
        .unreadable_fonts
        .iter()
        .filter(|font| font.reason == TextSuppression::UnmappedCodes)
        .map(|font| u64::from(font.codes))
        .sum();
    let suppressed = u64::from(page.suppressed_text_codes).saturating_sub(unmapped);
    let lost = suppressed + u64::from(page.undecoded_text_codes);
    lost >= GARBLED_MIN_CODES
        && lost >= u64::from(page.painted_text_chars) * GARBLED_LOST_PER_PAINTED
}

fn requested(verdict: PageVerdict) -> PageVerdict {
    let mut reasons = match verdict {
        PageVerdict::NeedsOcr { reasons } => reasons,
        PageVerdict::Text | PageVerdict::Blank => Vec::new(),
    };
    reasons.push(GapReason::CallerRequested);
    PageVerdict::NeedsOcr { reasons }
}

fn garbled(parse_ctx: &ParseCtx<'_>, page: &Page) {
    let count = page
        .unreadable_fonts
        .iter()
        .filter(|font| font.reason != TextSuppression::UnmappedCodes)
        .fold(u64::from(page.undecoded_text_codes), |count, font| {
            count.saturating_add(u64::try_from(font.runs).unwrap_or(u64::MAX))
        });
    parse_ctx.emit_count(DiagnosticCode::GarbledFontEncoding, Region::Body, count);
}

fn left_out(parse_ctx: &ParseCtx<'_>, page: &Page) {
    if page.link_annotations_incomplete {
        parse_ctx.emit(DiagnosticCode::LinkTargetUnread, Region::Body);
    }
    if page.omitted_page_number_chars > 0 {
        parse_ctx.emit(DiagnosticCode::FooterOmitted, Region::Footer);
    }
}

fn map_page(
    parse_ctx: &ParseCtx<'_>,
    read: &ReadPage,
    furniture: &Furniture,
    timeline: Option<&mut SideTimeline>,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    let texts = &read.texts;
    let lead = texts.iter().take_while(|text| furniture.strips(text)).count();
    let trail = texts[lead..].iter().rev().take_while(|text| furniture.strips(text)).count();
    let dropped: BTreeMap<usize, Side> = texts[..lead]
        .iter()
        .map(|text| (text.index, Side::Header))
        .chain(texts[texts.len() - trail..].iter().map(|text| (text.index, Side::Footer)))
        .collect();
    let mut sides = PageSides::default();
    let mut body = Vec::new();
    for (index, element) in read.page.elements.iter().enumerate() {
        match (dropped.get(&index), timeline.is_some()) {
            (Some(&side), true) => sides.read(parse_ctx, side, || {
                let mut out = Vec::new();
                map_block(parse_ctx, element, &mut out)?;
                Ok(out)
            })?,
            (Some(Side::Header), false) => {
                parse_ctx.emit(DiagnosticCode::HeaderOmitted, Region::Header);
            }
            (Some(_), false) => parse_ctx.emit(DiagnosticCode::FooterOmitted, Region::Footer),
            (None, _) => map_block(parse_ctx, element, &mut body)?,
        }
    }
    if let Some(timeline) = timeline {
        sides.publish(parse_ctx, timeline, blocks)?;
    }
    blocks.append(&mut body);
    Ok(())
}

fn map_block(
    parse_ctx: &ParseCtx<'_>,
    block: &PdfBlock,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    match block {
        PdfBlock::Paragraph(paragraph) => map_paragraph(parse_ctx, paragraph, blocks)?,
        PdfBlock::Table(table) => blocks.extend(map_table(parse_ctx, table)?),
        PdfBlock::HorizontalRule => blocks.push(Block::Rule),
        PdfBlock::Code { content } => {
            blocks.push(Block::CodeBlock { lang: None, text: content.clone() });
        }
        PdfBlock::Raw { content } => blocks.push(Block::Paragraph(vec![Inline::plain(content)])),
        PdfBlock::Image { .. } | PdfBlock::PageBreak | PdfBlock::SectionBreak => {}
    }
    Ok(())
}

fn map_paragraph(
    parse_ctx: &ParseCtx<'_>,
    paragraph: &Paragraph,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    let content = inlines(parse_ctx, &paragraph.content)?;
    match (&paragraph.style.list_info, paragraph.style.heading_level) {
        (Some(info), _) => {
            let (marker, start) = match info.style {
                ListStyle::Unordered { .. } => (MarkerKind::Bullet, 1),
                ListStyle::Ordered { start, number_style } => (marker(number_style), start),
            };
            push_item(blocks, marker, u64::from(info.item_number.unwrap_or(start)), content);
        }
        (None, Some(level)) => blocks.push(Block::heading(level, content)),
        (None, None) => blocks.push(Block::Paragraph(content)),
    }
    Ok(())
}

fn marker(style: NumberStyle) -> MarkerKind {
    match style {
        NumberStyle::Decimal => MarkerKind::Decimal,
        NumberStyle::LowerAlpha => MarkerKind::LowerAlpha,
        NumberStyle::UpperAlpha => MarkerKind::UpperAlpha,
        NumberStyle::LowerRoman => MarkerKind::LowerRoman,
        NumberStyle::UpperRoman => MarkerKind::UpperRoman,
    }
}

fn push_item(blocks: &mut Vec<Block>, marker: MarkerKind, number: u64, content: Vec<Inline>) {
    let item = ListItem { blocks: vec![Block::Paragraph(content)], marker_label: None };
    if let Some(Block::List(list)) = blocks.last_mut()
        && list.marker == marker
        && (!marker.ordered() || list.start.saturating_add(list.items.len() as u64) == number)
    {
        list.items.push(item);
        return;
    }
    blocks.push(Block::List(List { marker, start: number, items: vec![item] }));
}

fn inlines(
    parse_ctx: &ParseCtx<'_>,
    content: &[InlineContent],
) -> Result<Vec<Inline>, ConvertError> {
    let mut out = Vec::with_capacity(content.len());
    for inline in content {
        parse_ctx.charge(1)?;
        match inline {
            InlineContent::Text(run) => out.push(styled(&run.text, &run.style)),
            InlineContent::LineBreak => out.push(Inline::LineBreak),
            InlineContent::Link { text, url, style: label, .. } => out.push(Inline::Link {
                content: vec![styled(text, label)],
                target: classify_rel_target(true, url),
            }),
            InlineContent::Image { .. } => {}
        }
    }
    Ok(out)
}

fn styled(text: &str, from: &TextStyle) -> Inline {
    Inline::Text { text: text.to_owned(), style: style(from) }
}

fn style(style: &TextStyle) -> Style {
    let position = if style.superscript {
        VerticalPosition::Superscript
    } else if style.subscript {
        VerticalPosition::Subscript
    } else {
        VerticalPosition::Baseline
    };
    Style {
        bold: style.bold,
        italic: style.italic,
        strike: style.strikethrough,
        position,
        ..Style::PLAIN
    }
}

fn map_table(parse_ctx: &ParseCtx<'_>, table: &PdfTable) -> Result<Option<Block>, ConvertError> {
    let mut builder = GridBuilder::new(parse_ctx);
    for row in &table.rows {
        builder.next_row();
        for cell in &row.cells {
            let mut blocks = Vec::new();
            for paragraph in &cell.content {
                map_paragraph(parse_ctx, paragraph, &mut blocks)?;
            }
            let (cols, rows) = (u32::from(cell.colspan), u32::from(cell.rowspan));
            builder.place(Cell::spanning(blocks, cols, rows))?;
        }
    }
    let mut grid = builder.finish()?;
    if grid.grid.is_empty() {
        return Ok(None);
    }
    grid.header_rows = usize::from(table.header_rows).min(grid.grid.len());
    Ok(Some(Block::Table(grid)))
}

fn map_error(e: unpdf::Error) -> ConvertError {
    match e {
        unpdf::Error::Encrypted | unpdf::Error::InvalidPassword => ConvertError::Encrypted,
        unpdf::Error::Io(e) if e.kind() == ErrorKind::OutOfMemory => {
            ConvertError::limit(Limit::Allocation, 0)
        }
        unpdf::Error::UnknownFormat => ConvertError::malformed("not a PDF"),
        _ => ConvertError::malformed("unreadable PDF"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::test_support::diagnostic_rows;
    use std::assert_matches;
    use unpdf::{ListInfo, TextRun, UnreadableFont};

    fn read_page(page: Page) -> ReadPage {
        let mut slot = [None];
        keep(&ParseCtx::default(), Page { number: 1, ..page }, &mut slot).unwrap();
        slot.into_iter().next().flatten().unwrap()
    }

    fn page(texts: &[&str]) -> Page {
        let mut page = Page::new(1, 612.0, 792.0);
        for text in texts {
            page.add_paragraph(Paragraph::with_text(*text));
        }
        page
    }

    fn judged(pages: &[Option<ReadPage>]) -> Vec<PageVerdict> {
        judged_by(pages, ParseCtx::default().scans_by_coverage())
    }

    fn judged_by(pages: &[Option<ReadPage>], by_coverage: bool) -> Vec<PageVerdict> {
        let furniture = Furniture::of(&ParseCtx::default(), pages).unwrap();
        let judge = |(read, &furnished): (&Option<ReadPage>, _)| {
            page_verdict(read.as_ref(), furnished, by_coverage)
        };
        pages.iter().zip(&furniture.pages).map(judge).collect()
    }

    fn ruled(page: Page) -> PageVerdict {
        ruled_by(page, ParseCtx::default().scans_by_coverage())
    }

    fn ruled_by(page: Page, by_coverage: bool) -> PageVerdict {
        judged_by(&[Some(read_page(page))], by_coverage).remove(0)
    }

    fn needs_ocr(reasons: &[GapReason]) -> PageVerdict {
        PageVerdict::NeedsOcr { reasons: reasons.to_vec() }
    }

    type Case = (&'static [&'static str], fn(&mut Page), PageVerdict);

    #[test]
    fn each_fact_rules_its_reason() {
        use GapReason::*;
        let with = |texts: &[&str], set: fn(&mut Page)| {
            let mut page = page(texts);
            set(&mut page);
            ruled(page)
        };
        let cases: [Case; 21] = [
            (&[], |_| {}, PageVerdict::Blank),
            (&["  "], |_| {}, PageVerdict::Blank),
            (&["Body"], |_| {}, PageVerdict::Text),
            (&[], |p| p.undecodable_content_streams = 1, needs_ocr(&[UnreadableContent])),
            (&["Body"], |p| p.undecodable_content_streams = 1, needs_ocr(&[UnreadableContent])),
            (&["Body"], |p| p.ocr_text_suppressed = true, needs_ocr(&[InvisibleText])),
            (
                &["Body"],
                |p| (p.invisible_text_chars, p.painted_text_chars) = (9, 1),
                needs_ocr(&[InvisibleText]),
            ),
            (
                &["Body"],
                |p| (p.invisible_text_chars, p.painted_text_chars) = (8, 1),
                PageVerdict::Text,
            ),
            (&[], |p| p.clipped_text_runs = 1, needs_ocr(&[InvisibleText])),
            (&["Body"], |p| p.clipped_text_runs = 1, PageVerdict::Text),
            (&[], |p| p.suppressed_text_runs = 1, needs_ocr(&[GarbledText])),
            (&["Body"], |p| p.suppressed_text_runs = 1, PageVerdict::Text),
            (&[], |p| p.image_op_count = 1, needs_ocr(&[ImageBacked])),
            (&[], |p| p.fill_op_count = 1, needs_ocr(&[VectorDrawn])),
            (&["Body"], |p| p.fill_op_count = 1, PageVerdict::Text),
            (&["Body"], |p| p.text_op_count = 1, PageVerdict::Text),
            (&[], |p| (p.fill_op_count, p.image_op_count) = (1, 1), needs_ocr(&[ImageBacked])),
            (&[], |p| p.text_op_count = 1, needs_ocr(&[UnreadableText])),
            (&[], |p| (p.text_op_count, p.blank_text_runs) = (1, 1), PageVerdict::Blank),
            (
                &[],
                |p| {
                    (p.text_op_count, p.painted_text_chars, p.omitted_page_number_chars) =
                        (1, 3, 3);
                },
                PageVerdict::Blank,
            ),
            (
                &[],
                |p| {
                    (p.text_op_count, p.painted_text_chars, p.omitted_page_number_chars) =
                        (1, 4, 3);
                },
                needs_ocr(&[UnreadableText]),
            ),
        ];
        for (i, (texts, set, expected)) in cases.into_iter().enumerate() {
            assert_eq!(with(texts, set), expected, "case {i}");
        }
    }

    #[test]
    fn raw_text_reads_as_a_pages_text() {
        let mut page = page(&[]);
        page.elements.push(PdfBlock::Raw { content: "x".into() });
        assert_eq!(ruled(page), PageVerdict::Text);
    }

    #[test]
    fn whitespace_runs_count_as_blank_only_when_nothing_else_drew_no_text() {
        use GapReason::*;
        for (clipped, suppressed, expected) in [
            (1, 0, needs_ocr(&[InvisibleText])),
            (0, 1, needs_ocr(&[GarbledText])),
            (0, 0, PageVerdict::Blank),
        ] {
            let mut page = page(&[]);
            (page.text_op_count, page.blank_text_runs) = (1, 1);
            (page.clipped_text_runs, page.suppressed_text_runs) = (clipped, suppressed);
            assert_eq!(ruled(page), expected, "{clipped} {suppressed}");
        }
    }

    #[test]
    fn text_over_half_a_page_is_a_scan_only_when_it_is_furniture_or_margin() {
        let scan = |coverage: f32, margin: u32| {
            let mut page = page(&["Body"]);
            page.image_op_count = 1;
            page.image_coverage = coverage;
            (page.painted_text_chars, page.margin_text_chars) = (4, margin);
            ruled(page)
        };
        let scanned = needs_ocr(&[GapReason::ImageBacked]);
        for coverage in [1.0, 0.89, 0.5] {
            assert_eq!(scan(coverage, 4), scanned, "{coverage}");
        }
        assert_eq!(scan(0.49, 4), PageVerdict::Text);
        assert_eq!(scan(1.0, 3), PageVerdict::Text);
        assert_eq!(scan(0.5, 3), PageVerdict::Text);
        let mut zero = page(&["Body"]);
        (zero.image_op_count, zero.image_coverage) = (1, 1.0);
        assert_eq!(ruled(zero), PageVerdict::Text);

        let stamp = |n: &str| {
            let mut page = page(&["PROCEEDINGS 1923", n]);
            (page.image_op_count, page.image_coverage) = (1, 1.0);
            read_page(page)
        };
        let mut covered = page(&["Other", "PROCEEDINGS 1923"]);
        (covered.image_op_count, covered.image_coverage) = (1, 1.0);
        let body = read_page(page(&["Body"]));
        let pages = [Some(stamp("17")), Some(stamp("18")), Some(body), Some(read_page(covered))];
        let scanned = needs_ocr(&[GapReason::ImageBacked]);
        assert_eq!(
            judged(&pages),
            [scanned.clone(), scanned, PageVerdict::Text, PageVerdict::Text]
        );
    }

    fn image_backed_pages(pages: &[&[&str]], coverage: f32) -> Vec<bool> {
        image_backed_pages_by(pages, coverage, false)
    }

    fn image_backed_pages_by(pages: &[&[&str]], coverage: f32, by_coverage: bool) -> Vec<bool> {
        let pages: Vec<_> = pages
            .iter()
            .map(|texts| {
                let mut page = page(texts);
                (page.image_op_count, page.image_coverage) = (1, coverage);
                Some(read_page(page))
            })
            .collect();
        let image_backed = needs_ocr(&[GapReason::ImageBacked]);
        judged_by(&pages, by_coverage).into_iter().map(|verdict| verdict == image_backed).collect()
    }

    #[test]
    fn below_nine_tenths_only_running_heads_and_folios_are_furniture() {
        let stamps: [&[&str]; 3] =
            [&["PROCEEDINGS 1923", "17"], &["PROCEEDINGS 1923", "18"], &["PROCEEDINGS 1923", "19"]];
        assert_eq!(image_backed_pages(&stamps, 0.5), [true; 3]);
        assert_eq!(image_backed_pages(&stamps, 0.49), [false; 3]);
        let over_body: [&[&str]; 3] = [&["Body", "PROCEEDINGS 1923", "17"], stamps[1], stamps[2]];
        assert_eq!(image_backed_pages(&over_body, 0.6), [false, true, true]);
        let tick: [&[&str]; 3] = [&["PROCEEDINGS 1923", "17", "40"], stamps[1], stamps[2]];
        assert_eq!(image_backed_pages(&tick, 0.6), [false, true, true]);
        let legend = "L".repeat(RUNNING_HEAD_CHARS + 1);
        let legends: [&[&str]; 3] = [&[&legend, "17"], &[&legend, "18"], &[&legend, "19"]];
        assert_eq!(image_backed_pages(&legends, 0.6), [false; 3]);
        let dividers: [&[&str]; 3] =
            [&["Chapter 1", "Handbook"], &["Chapter 2", "Handbook"], &["Chapter 3", "Handbook"]];
        assert_eq!(image_backed_pages(&dividers, 0.89), [false; 3]);
        assert_eq!(image_backed_pages(&dividers, 0.9), [true; 3]);
    }

    fn covered_page(texts: &[&str], coverage: f32, margin_chars: u32) -> Page {
        let mut page = page(texts);
        page.image_op_count = 1;
        page.image_coverage = coverage;
        (page.painted_text_chars, page.margin_text_chars) = (4, margin_chars);
        page
    }

    #[test]
    fn coverage_first_sends_a_page_half_covered_by_images_to_ocr_whatever_text_it_holds() {
        let rows: [(&[&str], f32, bool, bool); 6] = [
            (&["Body"], 0.5, false, true),
            (&["Body"], 0.6, false, true),
            (&["Body"], 0.49, false, false),
            (&["Body"], 1.0, false, true),
            (&["Body"], 0.0, false, false),
            (&[], 0.49, true, true),
        ];
        let ruling = |scanned| {
            if scanned { needs_ocr(&[GapReason::ImageBacked]) } else { PageVerdict::Text }
        };
        for (texts, coverage, by_default, coverage_first) in rows {
            let page = || covered_page(texts, coverage, 0);
            assert_eq!(ruled(page()), ruling(by_default), "{texts:?} {coverage}");
            let found = ruled_by(page(), true);
            assert_eq!(found, ruling(coverage_first), "{texts:?} {coverage}");
        }
    }

    #[test]
    fn coverage_first_flags_every_page_the_conservative_rule_flags() {
        let scanned = needs_ocr(&[GapReason::ImageBacked]);
        let shapes: [(&[&str], u32); 3] = [(&[], 0), (&["Body"], 0), (&["Body"], 4)];
        for coverage in [0.0, 0.3, 0.49, 0.5, 0.75, 0.89, 0.9, 1.0] {
            for (texts, margin) in shapes {
                let flagged = |by_coverage| {
                    ruled_by(covered_page(texts, coverage, margin), by_coverage) == scanned
                };
                let (conservative, coverage_first) = (flagged(false), flagged(true));
                assert!(!conservative || coverage_first, "{texts:?} {margin} {coverage}");
            }
        }
        let stamps: [&[&str]; 3] =
            [&["PROCEEDINGS 1923", "17"], &["PROCEEDINGS 1923", "18"], &["PROCEEDINGS 1923", "19"]];
        for by_coverage in [false, true] {
            assert_eq!(
                image_backed_pages_by(&stamps, 0.5, by_coverage),
                [true; 3],
                "{by_coverage}"
            );
        }
    }

    #[test]
    fn a_stamp_alone_on_each_partial_scan_is_furniture_only_where_it_recurs() {
        let alone: [&[&str]; 3] = [&["PROCEEDINGS 1923"]; 3];
        assert_eq!(image_backed_pages(&alone, 0.6), [true; 3]);
        assert_eq!(image_backed_pages(&[&["PROCEEDINGS 1923"]], 0.6), [false]);
        let once: [&[&str]; 3] = [&["PROCEEDINGS 1923"], &["Plate one"], &["Plate two"]];
        assert_eq!(image_backed_pages(&once, 0.6), [false; 3]);
        let few: [&[&str]; 5] = [
            &["PROCEEDINGS 1923"],
            &["PROCEEDINGS 1923"],
            &["Plate one"],
            &["Plate two"],
            &["Map"],
        ];
        assert_eq!(image_backed_pages(&few, 0.6), [false; 5]);
    }

    #[test]
    fn copies_of_a_text_whose_numbers_agree_are_one_sighting_on_their_page() {
        let twice: [&[&str]; 3] = [&["PROCEEDINGS 1923", "PROCEEDINGS 1923"]; 3];
        assert_eq!(image_backed_pages(&twice, 0.6), [true; 3]);
        assert_eq!(image_backed_pages(&twice, 0.49), [false; 3]);
        let labels: [&[&str]; 3] = [&["Label", "Label"]; 3];
        assert_eq!(image_backed_pages(&labels, 0.0033), [false; 3]);
        let born_digital: Vec<_> =
            labels.iter().map(|texts| Some(read_page(page(texts)))).collect();
        assert_eq!(judged(&born_digital), vec![PageVerdict::Text; 3]);
        let heads: [&[&str]; 2] = [&["Head", "Head", "Body"], &["Head", "Other"]];
        assert_eq!(kept(&heads, 0), paragraphs(&["Body"]));
    }

    #[test]
    fn a_page_repeating_a_key_with_other_numbers_is_judged_alike_in_either_order() {
        let stamps: [&[&str]; 2] = [&["PROCEEDINGS 1923", "18"], &["PROCEEDINGS 1923", "19"]];
        let heads: [&[&str]; 2] = [&["Head", "Other", "4"], &["Head", "More", "5"]];
        for (tick, body) in [(["17", "40"], ["3", "40"]), (["40", "17"], ["40", "3"])] {
            let ticked: [&[&str]; 3] =
                [&["PROCEEDINGS 1923", tick[0], tick[1]], stamps[0], stamps[1]];
            assert_eq!(image_backed_pages(&ticked, 0.6), [false, true, true], "{tick:?}");
            let ruled: [&[&str]; 3] = [&["Head", "Body", body[0], body[1]], heads[0], heads[1]];
            assert_eq!(kept(&ruled, 1), paragraphs(&["Other"]), "{body:?}");
        }
    }

    #[test]
    fn numbers_past_the_largest_integer_keep_their_value() {
        let captions: [&[&str]; 3] = [
            &["Figure S18446744073709551616", "Source: Archive"],
            &["Figure S18446744073709551617", "Source: Archive"],
            &["Figure S18446744073709551618", "Source: Archive"],
        ];
        assert_eq!(image_backed_pages(&captions, 0.6), [false; 3]);
        let caption = "Figure S18446744073709551616";
        let heads: [&[&str]; 2] = [&[caption, "Body"], &["Figure S18446744073709551617", "Other"]];
        assert_eq!(kept(&heads, 0), paragraphs(&[caption, "Body"]));
        let same: [&[&str]; 2] =
            [&["Report 18446744073709551616", "Body"], &["Report 018446744073709551616", "Other"]];
        assert_eq!(kept(&same, 0), paragraphs(&["Body"]));
    }

    #[test]
    fn a_furniture_key_drops_digits_case_and_spacing() {
        assert_eq!(furniture_key("  Journal  of Tests, Vol. 7 "), "journal of tests, vol.");
        assert_eq!(furniture_key("12"), "");
    }

    fn mapped(pages: &[&[&str]], at: usize) -> (Vec<Block>, Vec<(DiagnosticCode, Region, u64)>) {
        let pages: Vec<_> = pages.iter().map(|texts| Some(read_page(page(texts)))).collect();
        let parse_ctx = ParseCtx::default();
        let furniture = Furniture::of(&parse_ctx, &pages).unwrap();
        let mut blocks = Vec::new();
        map_page(&parse_ctx, pages[at].as_ref().unwrap(), &furniture, None, &mut blocks).unwrap();
        (blocks, diagnostic_rows(&parse_ctx.into_diagnostics()))
    }

    #[test]
    fn under_emit_a_page_writes_its_running_head_and_folio_as_painted_and_a_page_without_them_clears_them()
     {
        let pages = furnished();
        let furniture = Furniture::of(&ParseCtx::default(), &pages).unwrap();
        let limits = crate::limits::Limits::default();
        let parse_ctx =
            ParseCtx::new(&limits).with_headers_footers(crate::HeaderFooterPolicy::Emit);
        let mut timeline = SideTimeline::default();
        let mut blocks = Vec::new();
        for (index, read) in pages.iter().enumerate() {
            let _unit = parse_ctx.begin_unit(UnitKind::Page, index).unwrap();
            map_page(
                &parse_ctx,
                read.as_ref().unwrap(),
                &furniture,
                Some(&mut timeline),
                &mut blocks,
            )
            .unwrap();
        }
        let markdown = crate::render::markdown::document_to_markdown(&Document {
            blocks,
            ..Document::default()
        });
        let side = |n: u32, region: &str, label: &str, text: &str| {
            let body = if text.is_empty() { String::new() } else { format!(">\n> {text}\n") };
            format!("> <a id=\"almiladoc:{region}:default:{n}\"></a>{label} (default):\n{body}\n")
        };
        assert_eq!(
            markdown,
            [
                side(1, "header", "Header", "**Journal**"),
                side(2, "footer", "Footer", "**42**"),
                "Body one\n\n".into(),
                side(3, "footer", "Footer", "**43**"),
                "Body two\n\n".into(),
                side(4, "header", "Header", ""),
                side(5, "footer", "Footer", ""),
                side(6, "header", "Header", "**Journal**"),
                side(7, "footer", "Footer", "**45**"),
                "Body four\n".into(),
            ]
            .concat()
        );
        assert_eq!(
            diagnostic_rows::<(DiagnosticCode, Region, u64)>(&parse_ctx.into_diagnostics()),
            []
        );
    }

    /// Four pages: a bold running head and folio about two bodies, a blank page, then both
    /// again.
    fn furnished() -> [Option<ReadPage>; 4] {
        let styled = |head: &str, body: &str, folio: &str| {
            let mut page = page(&[]);
            let mut bold = Paragraph::new();
            bold.add_run(TextRun::bold(head));
            page.add_paragraph(bold);
            page.add_paragraph(Paragraph::with_text(body));
            let mut number = Paragraph::new();
            number.add_run(TextRun::bold(folio));
            page.add_paragraph(number);
            Some(read_page(page))
        };
        [
            styled("Journal", "Body one", "42"),
            styled("Journal", "Body two", "43"),
            Some(read_page(page(&[]))),
            styled("Journal", "Body four", "45"),
        ]
    }

    fn paragraphs(texts: &[&str]) -> Vec<Block> {
        texts.iter().map(|text| Block::Paragraph(vec![Inline::plain(*text)])).collect()
    }

    #[test]
    fn running_heads_and_folios_leading_and_trailing_a_page_are_dropped_and_reported() {
        use DiagnosticCode::{FooterOmitted, HeaderOmitted};
        let head = "Journal of Tests, Vol. 7";
        let pages: [&[&str]; 3] = [
            &[head, "Chapter 1", "Own", "More", "3"],
            &[head, "Other", "4"],
            &[head, "Chapter 2", "Last", "5"],
        ];
        let (blocks, reported) = mapped(&pages, 0);
        assert_eq!(blocks, paragraphs(&["Chapter 1", "Own", "More"]));
        assert_eq!(
            reported,
            [(HeaderOmitted, Region::Header, 1), (FooterOmitted, Region::Footer, 1)]
        );
        assert_eq!(mapped(&pages, 2).0, paragraphs(&["Chapter 2", "Last"]));
        let alone: [&[&str]; 3] = [&[head, "Body", "3"], &[head, "4"], &[head, "Other", "5"]];
        assert_eq!(mapped(&alone, 1), (vec![], vec![(HeaderOmitted, Region::Header, 2)]));
        assert_eq!(mapped(&[&["Body"], &["Other"]], 0), (paragraphs(&["Body"]), vec![]));
        assert_eq!(
            mapped(&[&["Body", "7"], &["Other", "  "]], 0),
            (paragraphs(&["Body", "7"]), vec![])
        );
    }

    fn kept(pages: &[&[&str]], at: usize) -> Vec<Block> {
        mapped(pages, at).0
    }

    #[test]
    fn text_that_recurs_is_kept_unless_it_runs_through_the_pages_as_a_head_or_folio() {
        let long = "word ".repeat(21);
        let long = long.as_str();
        let cases: [(&[&[&str]], &[&str]); 10] = [
            (&[&["Head", "Body"], &["Head", "Other"]], &["Body"]),
            (&[&["Head 7", "Body"], &["Head 7", "Other"]], &["Body"]),
            (&[&["Head 7", "Body"], &["Head 8", "Other"]], &["Head 7", "Body"]),
            (&[&["Chapter 1", "Body"], &["Body 2"], &["Chapter 2", "Other"]], &["Chapter 1"]),
            (&[&["Revenue 100", "Body"], &["Revenue 200", "Other"]], &["Revenue 100", "Body"]),
            (&[&["Vol. 7", "Body"], &["Vol. 7 8", "Other"]], &["Vol. 7", "Body"]),
            (&[&[long, "Body"], &[long, "Other"]], &[long, "Body"]),
            (&[&["Head"], &["Head"], &["Head"]], &["Head"]),
            (&[&["Head", "Body"], &["Head"]], &["Head", "Body"]),
            (&[&["Head", "Body"], &["Head", "Other"], &["A"], &["B"], &["C"]], &["Head", "Body"]),
        ];
        for (i, (pages, expected)) in cases.into_iter().enumerate() {
            let blocks = kept(pages, 0);
            assert!(blocks.starts_with(&paragraphs(expected)), "case {i}: {blocks:?}");
        }
        let short = "word ".repeat(20);
        assert_eq!(kept(&[&[&short, "Body"], &[&short, "Other"]], 0), paragraphs(&["Body"]));
        let five: [&[&str]; 5] =
            [&["Head", "Body"], &["Head", "Other"], &["Head", "More"], &["A"], &["B"]];
        assert_eq!(kept(&five, 0), paragraphs(&["Body"]));
        assert_eq!(kept(&[&["Body", "3"], &["Other", "4"]], 0), paragraphs(&["Body"]));
        assert_eq!(kept(&[&["Body", "3"], &["Other", "5"]], 0), paragraphs(&["Body", "3"]));
    }

    #[test]
    fn recurring_text_runs_only_where_a_page_also_holds_text_that_does_not() {
        let long = "word ".repeat(21);
        let twice: [&[&str]; 2] = [&["Head", "Body"], &["Head", "Body"]];
        assert_eq!(mapped(&twice, 0), (paragraphs(&["Head", "Body"]), vec![]));
        let beside: [&[&str]; 3] = [&["Head", "Body"], &["Head", "Body"], &["Other"]];
        assert_eq!(kept(&beside, 1), paragraphs(&["Head", "Body"]));
        let third: [&[&str]; 3] = [&["Head", "Body"], &["Head", "Body"], &["Head", "Third"]];
        assert_eq!(kept(&third, 0), paragraphs(&["Body"]));
        assert_eq!(kept(&third, 2), paragraphs(&["Third"]));
        let long_body: [&[&str]; 2] = [&["Head", &long], &["Head", &long]];
        assert_eq!(kept(&long_body, 0), paragraphs(&[&long]));
    }

    #[test]
    fn a_heading_or_a_table_is_never_a_running_head() {
        use unpdf::{TableCell, TableRow};
        let beside = |block: fn() -> PdfBlock, texts: [&str; 2]| {
            let pages: Vec<_> = texts
                .into_iter()
                .map(|text| {
                    let mut page = page(&[]);
                    page.elements.push(block());
                    page.add_paragraph(Paragraph::with_text(text));
                    Some(read_page(page))
                })
                .collect();
            let furniture = Furniture::of(&ParseCtx::default(), &pages).unwrap();
            let mut blocks = Vec::new();
            map_page(
                &ParseCtx::default(),
                pages[0].as_ref().unwrap(),
                &furniture,
                None,
                &mut blocks,
            )
            .unwrap();
            blocks.len()
        };
        let with = |block: fn() -> PdfBlock| beside(block, ["Body", "Other"]);
        let heading = || PdfBlock::Paragraph(Paragraph::heading("Chapter", 1));
        assert_eq!(with(heading), 2);
        assert_eq!(beside(heading, ["Head", "Head"]), 1);
        let table = || {
            let mut table = PdfTable::new();
            table.add_row(TableRow::new(vec![TableCell::text("Head")]));
            PdfBlock::Table(table)
        };
        assert_eq!(with(table), 2);
        assert_eq!(with(|| PdfBlock::Raw { content: "Head".into() }), 1);
        assert_eq!(with(|| PdfBlock::Paragraph(Paragraph::with_text("Head"))), 1);
    }

    #[test]
    fn a_number_is_each_digit_run_of_a_text() {
        assert_eq!(numbers("Vol. 7, pp. 12-130"), ["7", "12", "130"]);
        assert_eq!(numbers("none"), [] as [&str; 0]);
        assert_eq!(numbers("007 000"), ["7", "0"]);
        assert_eq!(numbers("99999999999999999999"), ["99999999999999999999"]);
    }

    #[test]
    fn a_page_the_engine_could_not_read_needs_ocr_for_its_content() {
        let pages = [None, Some(read_page(page(&["Body"])))];
        let unreadable = needs_ocr(&[GapReason::UnreadableContent]);
        assert_eq!(judged(&pages), [unreadable, PageVerdict::Text]);
    }

    fn item(style: ListStyle, number: Option<u32>, text: &str) -> Paragraph {
        let mut paragraph = Paragraph::with_text(text);
        paragraph.style.list_info = Some(ListInfo { style, level: 0, item_number: number });
        paragraph
    }

    fn decimal() -> ListStyle {
        ListStyle::Ordered { start: 1, number_style: NumberStyle::Decimal }
    }

    #[test]
    fn consecutive_items_of_one_list_join_it() {
        let bullet = ListStyle::Unordered { marker: '•' };
        let roman = ListStyle::Ordered { start: 4, number_style: NumberStyle::UpperRoman };
        let paragraphs = [
            item(bullet.clone(), None, "a"),
            item(bullet, None, "b"),
            item(decimal(), Some(2), "c"),
            item(decimal(), Some(3), "d"),
            item(decimal(), Some(5), "e"),
            item(roman, None, "f"),
            Paragraph::heading("g", 2),
            Paragraph::with_text("h"),
        ];
        let parse_ctx = ParseCtx::default();
        let mut blocks = Vec::new();
        for paragraph in &paragraphs {
            map_paragraph(&parse_ctx, paragraph, &mut blocks).unwrap();
        }
        let lists: Vec<_> = blocks
            .iter()
            .map(|block| match block {
                Block::List(list) => (list.marker, list.start, list.items.len()),
                Block::Heading { level, .. } => (MarkerKind::Bullet, u64::from(*level), 0),
                _ => (MarkerKind::Bullet, 0, 0),
            })
            .collect();
        assert_eq!(
            lists,
            [
                (MarkerKind::Bullet, 1, 2),
                (MarkerKind::Decimal, 2, 2),
                (MarkerKind::Decimal, 5, 1),
                (MarkerKind::UpperRoman, 4, 1),
                (MarkerKind::Bullet, 2, 0),
                (MarkerKind::Bullet, 0, 0),
            ]
        );
    }

    #[test]
    fn each_number_style_keeps_its_marker() {
        let styles = [
            NumberStyle::Decimal,
            NumberStyle::LowerAlpha,
            NumberStyle::UpperAlpha,
            NumberStyle::LowerRoman,
            NumberStyle::UpperRoman,
        ];
        assert_eq!(
            styles.map(marker),
            [
                MarkerKind::Decimal,
                MarkerKind::LowerAlpha,
                MarkerKind::UpperAlpha,
                MarkerKind::LowerRoman,
                MarkerKind::UpperRoman,
            ]
        );
    }

    #[test]
    fn a_run_keeps_its_emphasis_and_script() {
        let run = |set: fn(&mut TextStyle)| {
            let mut style = TextStyle::default();
            set(&mut style);
            InlineContent::Text(TextRun { text: "x".into(), style })
        };
        let content = [
            run(|_| {}),
            run(|s| (s.bold, s.italic) = (true, true)),
            run(|s| s.strikethrough = true),
            run(|s| s.superscript = true),
            run(|s| s.subscript = true),
            run(|s| s.underline = true),
        ];
        let styles: Vec<Style> = inlines(&ParseCtx::default(), &content)
            .unwrap()
            .into_iter()
            .map(|inline| match inline {
                Inline::Text { style, .. } => style,
                other => panic!("{other:?}"),
            })
            .collect();
        let plain = Style::PLAIN;
        assert_eq!(
            styles,
            [
                plain,
                Style { bold: true, italic: true, ..plain },
                Style { strike: true, ..plain },
                Style { position: VerticalPosition::Superscript, ..plain },
                Style { position: VerticalPosition::Subscript, ..plain },
                plain,
            ]
        );
    }

    #[test]
    fn a_link_keeps_its_label_and_a_rule_and_raw_text_map() {
        let parse_ctx = ParseCtx::default();
        let content = [
            InlineContent::Link {
                text: "site".into(),
                url: "https://example.org".into(),
                title: None,
                style: TextStyle { bold: true, ..TextStyle::default() },
            },
            InlineContent::Image { resource_id: "Im1".into(), alt_text: Some("alt".into()) },
            InlineContent::LineBreak,
        ];
        assert_eq!(
            inlines(&parse_ctx, &content).unwrap(),
            [
                Inline::Link {
                    content: vec![Inline::Text {
                        text: "site".into(),
                        style: Style { bold: true, ..Style::PLAIN },
                    }],
                    target: crate::model::LinkTarget::External("https://example.org".into()),
                },
                Inline::LineBreak,
            ]
        );
        let mut blocks = Vec::new();
        map_block(&parse_ctx, &PdfBlock::image("Im2"), &mut blocks).unwrap();
        map_block(&parse_ctx, &PdfBlock::HorizontalRule, &mut blocks).unwrap();
        map_block(&parse_ctx, &PdfBlock::PageBreak, &mut blocks).unwrap();
        map_block(&parse_ctx, &PdfBlock::Raw { content: "raw".into() }, &mut blocks).unwrap();
        assert_eq!(blocks, [Block::Rule, Block::Paragraph(vec![Inline::plain("raw")])]);
        assert!(parse_ctx.into_diagnostics().is_empty());
    }

    #[test]
    fn a_table_keeps_its_spans_and_header_rows() {
        use unpdf::{TableCell, TableRow};
        let mut table = PdfTable::with_header(1);
        table.add_row(TableRow::header(vec![TableCell::text("wide").colspan(2)]));
        table.add_row(TableRow::new(vec![TableCell::text("a"), TableCell::text("b")]));
        let Some(Block::Table(grid)) = map_table(&ParseCtx::default(), &table).unwrap() else {
            panic!("no table");
        };
        assert_eq!((grid.grid.len(), grid.header_rows), (2, 1));
        assert_matches!(&grid.grid[0][..], [crate::model::CellSlot::Origin(cell), _] if cell.col_span == 2);
        assert_eq!(map_table(&ParseCtx::default(), &PdfTable::new()).unwrap(), None);

        let mut tall = PdfTable::with_header(3);
        tall.add_row(TableRow::new(vec![TableCell::text("down").rowspan(2), TableCell::text("b")]));
        tall.add_row(TableRow::new(vec![TableCell::text("c")]));
        let Some(Block::Table(grid)) = map_table(&ParseCtx::default(), &tall).unwrap() else {
            panic!("no table");
        };
        assert_eq!(grid.header_rows, 2);
        let text = |slot: &crate::model::CellSlot| match slot {
            crate::model::CellSlot::Origin(cell) => (cell.row_span, format!("{:?}", cell.blocks)),
            other => (0, format!("{other:?}")),
        };
        assert_eq!(text(&grid.grid[0][0]).0, 2);
        assert!(text(&grid.grid[0][0]).1.contains("down"));
        assert!(!matches!(grid.grid[1][0], crate::model::CellSlot::Origin(_)));
        assert!(text(&grid.grid[1][1]).1.contains("\"c\""));
    }

    #[test]
    fn links_in_table_cells_charge_work_by_their_targets_length() {
        use crate::limits::Limits;
        use unpdf::{TableCell, TableRow};
        let url = format!("https://example.test/{}", "a".repeat(8000));
        let linked = || {
            let mut paragraph = Paragraph::new();
            paragraph.content.push(InlineContent::Link {
                text: "a".into(),
                url: url.clone(),
                title: None,
                style: TextStyle::default(),
            });
            TableCell::with_content(vec![paragraph])
        };
        let mut table = PdfTable::new();
        for _ in 0..4 {
            table.add_row(TableRow::new(vec![linked(), linked()]));
        }
        let mut page = Page::new(1, 612.0, 792.0);
        page.add_table(table);
        let limits = Limits { max_work: 8 * 8000, ..Limits::default() };
        let err = keep(&ParseCtx::new(&limits), page, &mut [None]).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Work, .. });
    }

    #[test]
    fn codes_a_font_gives_no_character_are_not_garbled() {
        let font =
            |name: &str, reason, runs| UnreadableFont { name: name.into(), reason, runs, codes: 0 };
        let mut page = page(&["Body"]);
        page.unreadable_fonts = vec![
            font("SegoeUI", TextSuppression::UnmappedCodes, 2),
            font("F1", TextSuppression::CompositeUnresolved, 3),
            font("F2", TextSuppression::BinaryDensity, 1),
        ];
        let parse_ctx = ParseCtx::default();
        garbled(&parse_ctx, &page);
        assert_eq!(
            diagnostic_rows::<(DiagnosticCode, Region, u64)>(&parse_ctx.into_diagnostics()),
            [(DiagnosticCode::GarbledFontEncoding, Region::Body, 4)]
        );
        page.unreadable_fonts.truncate(1);
        let parse_ctx = ParseCtx::default();
        garbled(&parse_ctx, &page);
        assert!(parse_ctx.into_diagnostics().is_empty());
        page.undecoded_text_codes = 2;
        let parse_ctx = ParseCtx::default();
        garbled(&parse_ctx, &page);
        assert_eq!(
            diagnostic_rows::<(DiagnosticCode, Region, u64)>(&parse_ctx.into_diagnostics()),
            [(DiagnosticCode::GarbledFontEncoding, Region::Body, 2)]
        );
    }

    fn lost_page(painted: u32, fonts: &[(TextSuppression, u32)], undecoded: u32) -> Page {
        let mut page = page(&["Body"]);
        page.painted_text_chars = painted;
        page.undecoded_text_codes = undecoded;
        page.suppressed_text_runs = fonts.len();
        page.suppressed_text_codes =
            fonts.iter().fold(0, |total: u32, &(_, codes)| total.saturating_add(codes));
        page.unreadable_fonts = fonts
            .iter()
            .map(|&(reason, codes)| UnreadableFont { name: "F".into(), reason, runs: 1, codes })
            .collect();
        page
    }

    #[test]
    fn a_page_with_twice_as_many_lost_codes_as_painted_characters_needs_ocr_beside_its_text() {
        use TextSuppression::{BinaryDensity, CompositeUnresolved, UnmappedCodes};
        let garbled = needs_ocr(&[GapReason::GarbledText]);
        type Fonts = &'static [(TextSuppression, u32)];
        let cases: [(&str, u32, Fonts, u32, PageVerdict); 13] = [
            ("twice the painted", 30, &[(CompositeUnresolved, 60)], 0, garbled.clone()),
            ("one under twice", 30, &[(CompositeUnresolved, 59)], 0, PageVerdict::Text),
            ("as many as painted", 100, &[(CompositeUnresolved, 100)], 0, PageVerdict::Text),
            ("the minimum", 5, &[(CompositeUnresolved, 40)], 0, garbled.clone()),
            ("one under the minimum", 5, &[(CompositeUnresolved, 39)], 0, PageVerdict::Text),
            ("binary density", 30, &[(BinaryDensity, 60)], 0, garbled.clone()),
            ("unmapped alone", 30, &[(UnmappedCodes, 100)], 0, PageVerdict::Text),
            (
                "unmapped beside fewer lost",
                30,
                &[(UnmappedCodes, 60), (CompositeUnresolved, 40)],
                0,
                PageVerdict::Text,
            ),
            (
                "unmapped codes past the saturated total",
                30,
                &[(UnmappedCodes, u32::MAX), (UnmappedCodes, u32::MAX)],
                0,
                PageVerdict::Text,
            ),
            ("undecoded", 30, &[], 60, garbled.clone()),
            ("one under twice undecoded", 30, &[], 59, PageVerdict::Text),
            ("suppressed with undecoded", 30, &[(CompositeUnresolved, 30)], 30, garbled),
            (
                "suppressed without undecoded",
                30,
                &[(CompositeUnresolved, 30)],
                0,
                PageVerdict::Text,
            ),
        ];
        for (label, painted, fonts, undecoded, expected) in cases {
            assert_eq!(ruled(lost_page(painted, fonts, undecoded)), expected, "{label}");
        }
    }

    #[test]
    fn engine_errors_carry_no_engine_text() {
        let oom = map_error(unpdf::Error::Io(ErrorKind::OutOfMemory.into()));
        assert_matches!(oom, ConvertError::ResourceLimit { limit: Limit::Allocation, max: 0 });
        for e in [unpdf::Error::Encrypted, unpdf::Error::InvalidPassword] {
            assert_matches!(map_error(e), ConvertError::Encrypted);
        }
        for e in [
            unpdf::Error::Io(ErrorKind::InvalidData.into()),
            unpdf::Error::UnknownFormat,
            unpdf::Error::PdfParse("\u{1b}[2J secret".into()),
            unpdf::Error::Corrupted("\u{1b}[2J secret".into()),
        ] {
            let mapped = map_error(e);
            assert_matches!(mapped, ConvertError::Malformed { part: None, .. });
            assert!(!mapped.to_string().contains("secret"));
        }
    }

    #[test]
    fn extraction_starts_no_thread() {
        let options = sequential();
        let measured = unpdf::ParseOptions::default().sequential();
        assert!(!options.parallel && !measured.parallel);
        assert_matches!(options.error_mode, ErrorMode::Lenient);
        assert_eq!(
            (options.extract_text, options.extract_resources, options.min_image_dimension),
            (measured.extract_text, measured.extract_resources, measured.min_image_dimension)
        );
        assert_eq!(options.suppress_low_confidence_ocr, measured.suppress_low_confidence_ocr);
        assert!(options.extract_text && options.suppress_low_confidence_ocr);
        assert!(!options.extract_resources && options.min_image_dimension == 64);
        assert_matches!(
            (&options.pages, &measured.pages),
            (PageSelection::All, PageSelection::All)
        );
        assert_eq!((options.password, measured.password), (None, None));
    }
}
