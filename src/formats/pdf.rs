//! PDF through unpdf, one page at a time, each page a unit. A page's verdict is ruled from the
//! facts unpdf reports of it: one whose text does not stand for what it shows becomes a gap
//! awaiting OCR, and every other page is mapped into the model without the running heads and
//! folios that recur across its pages.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::num::NonZeroU32;
use std::ops::ControlFlow;

use unpdf::model::{ListStyle, NumberStyle};
use unpdf::parser::backend::TextSuppression;
use unpdf::{
    Block as PdfBlock, ErrorMode, InlineContent, Page, PageSelection, PageStreamOptions, Paragraph,
    ParseEvent, PdfParser, Table as PdfTable, TextStyle,
};

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::inspect::PageVerdict;
use crate::limits::Limit;
use crate::model::{
    Block, Cell, Document, Gap, GapId, GapReason, GridBuilder, Inline, List, ListItem, MarkerKind,
    OcrOrigin, Style, UnitKind, VerticalPosition,
};
use crate::shared::fields::classify_rel_target;

/// The share of a page an image covers from which furniture or margin text over it is a scan's.
const SCAN_COVERAGE: f32 = 0.9;
/// The most non-space characters a running head or folio holds.
const RUNNING_HEAD_CHARS: usize = 80;

/// A parsed PDF: its document, each page's verdict in page order, and what its pages are
/// painted from.
pub(crate) struct Read {
    pub(crate) document: Document,
    pub(crate) verdicts: Vec<PageVerdict>,
    #[cfg(feature = "raster")]
    pub(crate) source: PdfSource,
}

/// unpdf's parser of a PDF, and each page's box as parsed, `None` for a page that did not
/// parse.
#[cfg(feature = "raster")]
pub(crate) struct PdfSource {
    pub(crate) parser: PdfParser,
    pub(crate) boxes: Vec<Option<PageBox>>,
}

/// A page's media box in points and its `/Rotate`, clockwise degrees.
#[cfg(feature = "raster")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PageBox {
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) rotation: u16,
}

#[cfg(feature = "raster")]
impl PdfSource {
    /// The box of 1-based `page`, read again with no text for a page the parse could not read.
    pub(crate) fn page_box(&self, page: NonZeroU32) -> Option<PageBox> {
        let index = usize::try_from(page.get() - 1).ok()?;
        if let Some(parsed) = self.boxes.get(index)?.as_ref() {
            return Some(*parsed);
        }
        let options = PageStreamOptions {
            extract_text: false,
            pages: PageSelection::Pages(vec![page.get()]),
            ..sequential()
        };
        let mut found = None;
        let _: Result<_, _> = self.parser.for_each_page(options, |event| {
            if let ParseEvent::PageParsed(parsed) = event {
                found = Some(PageBox::of(&parsed));
            }
            ControlFlow::Continue(())
        });
        found
    }
}

#[cfg(feature = "raster")]
impl PageBox {
    fn of(page: &Page) -> Self {
        Self { width: page.width, height: page.height, rotation: page.rotation }
    }
}

/// The document, each page's verdict and what its pages are painted from.
pub(crate) fn read(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Read, ConvertError> {
    let Extracted { pages, missing, parser } = extract(parse_ctx, bytes)?;
    let furniture = Furniture::of(&pages);
    let mut document = Document::default();
    let mut verdicts = Vec::with_capacity(pages.len());
    for (index, page) in pages.iter().enumerate() {
        let scope = parse_ctx.begin_unit(UnitKind::Page, index)?;
        let unit = scope.unit().model();
        document.blocks.push(Block::UnitStart(unit));
        let verdict = page_verdict(page.as_ref(), &furniture);
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
            map_page(parse_ctx, read, &furniture, &mut document.blocks)?;
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
    numbers: Vec<u64>,
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
        parse_ctx.charge(u64::try_from(text.len()).unwrap_or(u64::MAX))?;
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

/// A block's text, and whether it is plain text a running head could be: no heading, table or code block.
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

/// What a running head or folio keeps from page to page: its words, lowercased, without
/// digits.
fn furniture_key(text: &str) -> String {
    let kept: String =
        text.chars().filter(|c| !c.is_ascii_digit()).flat_map(char::to_lowercase).collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn numbers(text: &str) -> Vec<u64> {
    text.split(|c: char| !c.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .map(|run| run.parse().unwrap_or(u64::MAX))
        .collect()
}

/// The pages each furniture key is found on, and the keys that are running heads or folios.
struct Furniture {
    pages_with: BTreeMap<String, usize>,
    running: BTreeSet<String>,
}

/// A furniture key's first text on one page.
struct Sighting<'a> {
    page: u64,
    numbers: &'a [u64],
    beside_other_text: bool,
}

impl Furniture {
    fn of(pages: &[Option<ReadPage>]) -> Self {
        let mut sightings: BTreeMap<&str, Vec<Sighting<'_>>> = BTreeMap::new();
        for (page, read) in (1_u64..).zip(pages) {
            let Some(read) = read else { continue };
            let mut first: BTreeMap<&str, &Text> = BTreeMap::new();
            for text in &read.texts {
                first.entry(text.key.as_str()).or_insert(text);
            }
            let beside_other_text = first.len() > 1;
            for (key, text) in first {
                let numbers = text.numbers.as_slice();
                let sighting = Sighting { page, numbers, beside_other_text };
                sightings.entry(key).or_default().push(sighting);
            }
        }
        let text_pages = pages.iter().flatten().filter(|read| !read.texts.is_empty()).count();
        let pages_with = sightings.iter().map(|(key, on)| ((*key).to_owned(), on.len())).collect();
        let candidates: BTreeSet<&str> = sightings
            .iter()
            .filter(|(key, on)| runs_through(key, on, text_pages))
            .map(|(key, _)| *key)
            .collect();
        let running = Self::beside_body(pages, &candidates);
        Self { pages_with, running }
    }

    /// The candidate keys found on some page that also holds a text no candidate strips.
    fn beside_body(pages: &[Option<ReadPage>], candidates: &BTreeSet<&str>) -> BTreeSet<String> {
        let strippable = |text: &Text| text.running_head && candidates.contains(text.key.as_str());
        pages
            .iter()
            .flatten()
            .filter(|read| read.texts.iter().any(|text| !strippable(text)))
            .flat_map(|read| &read.texts)
            .filter(|text| candidates.contains(text.key.as_str()))
            .map(|text| text.key.clone())
            .collect()
    }

    fn repeats(&self, text: &Text) -> bool {
        self.pages_with.get(&text.key).is_some_and(|&pages| pages >= 2)
    }

    fn strips(&self, text: &Text) -> bool {
        text.running_head && self.running.contains(&text.key)
    }
}

/// Whether a key seen `on` these of `text_pages` pages with text may be a running head or folio:
/// on half of them, beside other text on two, its numbers fixed, or a bare folio's in step with
/// the page.
fn runs_through(key: &str, on: &[Sighting<'_>], text_pages: usize) -> bool {
    on.len() * 2 >= text_pages
        && on.iter().filter(|sighting| sighting.beside_other_text).count() >= 2
        && numbers_hold(on, key.is_empty())
}

fn numbers_hold(on: &[Sighting<'_>], folio: bool) -> bool {
    let Some((first, rest)) = on.split_first() else { return false };
    if rest.iter().any(|sighting| sighting.numbers.len() != first.numbers.len()) {
        return false;
    }
    first.numbers.iter().enumerate().all(|(at, &number)| {
        let value = |sighting: &Sighting<'_>| sighting.numbers.get(at).copied().unwrap_or(number);
        let offset =
            |sighting: &Sighting<'_>| i128::from(value(sighting)) - i128::from(sighting.page);
        rest.iter().all(|sighting| value(sighting) == number)
            || (folio && rest.iter().all(|sighting| offset(sighting) == offset(first)))
    })
}

fn page_verdict(page: Option<&ReadPage>, furniture: &Furniture) -> PageVerdict {
    page.map_or_else(
        || PageVerdict::NeedsOcr { reasons: vec![GapReason::UnreadableContent] },
        |read| verdict(read, furniture),
    )
}

fn verdict(read: &ReadPage, furniture: &Furniture) -> PageVerdict {
    let text_chars = read.texts.iter().map(|text| text.chars).sum();
    let furniture_only = read.texts.iter().all(|text| furniture.repeats(text));
    let reasons = reasons(&read.page, text_chars, furniture_only);
    if !reasons.is_empty() {
        PageVerdict::NeedsOcr { reasons }
    } else if text_chars == 0 {
        PageVerdict::Blank
    } else {
        PageVerdict::Text
    }
}

fn reasons(page: &Page, text_chars: usize, furniture_only: bool) -> Vec<GapReason> {
    let invisible = u64::from(page.invisible_text_chars);
    let shown = invisible + u64::from(page.painted_text_chars);
    let textless = text_chars == 0;
    let margin_only =
        page.painted_text_chars > 0 && page.margin_text_chars == page.painted_text_chars;
    let scanned = page.image_coverage >= SCAN_COVERAGE && (furniture_only || margin_only);
    let mut reasons: Vec<GapReason> = [
        (GapReason::UnreadableContent, page.undecodable_content_streams > 0),
        (
            GapReason::InvisibleText,
            page.ocr_text_suppressed
                || (shown > 0 && invisible * 10 >= shown * 9)
                || (textless && page.clipped_text_runs > 0),
        ),
        (GapReason::GarbledText, page.suppressed_text_runs > 0 && textless),
        (GapReason::ImageBacked, page.image_op_count > 0 && (textless || scanned)),
        (GapReason::VectorDrawn, textless && page.image_op_count == 0 && page.fill_op_count > 0),
    ]
    .into_iter()
    .filter_map(|(reason, holds)| holds.then_some(reason))
    .collect();
    let blank_runs_only =
        page.blank_text_runs > 0 && page.clipped_text_runs == 0 && page.suppressed_text_runs == 0;
    if reasons.is_empty() && textless && page.text_op_count > 0 && !blank_runs_only {
        reasons.push(GapReason::UnreadableText);
    }
    reasons
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

fn map_page(
    parse_ctx: &ParseCtx<'_>,
    read: &ReadPage,
    furniture: &Furniture,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    let texts = &read.texts;
    let lead = texts.iter().take_while(|text| furniture.strips(text)).count();
    let trail = texts[lead..].iter().rev().take_while(|text| furniture.strips(text)).count();
    let dropped: BTreeMap<usize, DiagnosticCode> = texts[..lead]
        .iter()
        .map(|text| (text.index, DiagnosticCode::HeaderOmitted))
        .chain(
            texts[texts.len() - trail..].iter().map(|t| (t.index, DiagnosticCode::FooterOmitted)),
        )
        .collect();
    for (index, element) in read.page.elements.iter().enumerate() {
        match dropped.get(&index) {
            Some(&code) if code == DiagnosticCode::HeaderOmitted => {
                parse_ctx.emit(code, Region::Header);
            }
            Some(&code) => parse_ctx.emit(code, Region::Footer),
            None => map_block(parse_ctx, element, blocks)?,
        }
    }
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
            InlineContent::Text(run) => {
                out.push(Inline::Text { text: run.text.clone(), style: style(&run.style) });
            }
            InlineContent::LineBreak => out.push(Inline::LineBreak),
            InlineContent::Link { text, url, .. } => out.push(Inline::Link {
                content: vec![Inline::plain(text)],
                target: classify_rel_target(true, url),
            }),
            InlineContent::Image { .. } => {}
        }
    }
    Ok(out)
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

    fn ruled(page: Page) -> PageVerdict {
        let read = read_page(page);
        let furniture = Furniture::of(&[Some(read_page(Page::new(1, 612.0, 792.0)))]);
        verdict(&read, &furniture)
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
        let cases: [Case; 19] = [
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
    fn text_over_a_page_sized_image_is_a_scan_only_when_it_is_furniture_or_margin() {
        let scan = |coverage: f32, margin: u32| {
            let mut page = page(&["Body"]);
            page.image_op_count = 1;
            page.image_coverage = coverage;
            (page.painted_text_chars, page.margin_text_chars) = (4, margin);
            ruled(page)
        };
        assert_eq!(scan(1.0, 4), needs_ocr(&[GapReason::ImageBacked]));
        assert_eq!(scan(0.89, 4), PageVerdict::Text);
        assert_eq!(scan(1.0, 3), PageVerdict::Text);
        let mut zero = page(&["Body"]);
        (zero.image_op_count, zero.image_coverage) = (1, 1.0);
        assert_eq!(ruled(zero), PageVerdict::Text);

        let stamp = |n: &str| {
            let mut page = page(&["PROCEEDINGS 1923", n]);
            (page.image_op_count, page.image_coverage) = (1, 1.0);
            read_page(page)
        };
        let pages = [Some(stamp("17")), Some(stamp("18")), Some(read_page(page(&["Body"])))];
        let furniture = Furniture::of(&pages);
        let [Some(first), _, Some(body)] = &pages else { panic!() };
        assert_eq!(verdict(first, &furniture), needs_ocr(&[GapReason::ImageBacked]));
        let mut covered = page(&["Body", "PROCEEDINGS 1923"]);
        (covered.image_op_count, covered.image_coverage) = (1, 1.0);
        assert_eq!(verdict(&read_page(covered), &furniture), PageVerdict::Text);
        assert_eq!(verdict(body, &furniture), PageVerdict::Text);
    }

    #[test]
    fn a_furniture_key_drops_digits_case_and_spacing() {
        assert_eq!(furniture_key("  Journal  of Tests, Vol. 7 "), "journal of tests, vol.");
        assert_eq!(furniture_key("12"), "");
    }

    fn mapped(pages: &[&[&str]], at: usize) -> (Vec<Block>, Vec<(DiagnosticCode, Region, u64)>) {
        let pages: Vec<_> = pages.iter().map(|texts| Some(read_page(page(texts)))).collect();
        let furniture = Furniture::of(&pages);
        let parse_ctx = ParseCtx::default();
        let mut blocks = Vec::new();
        map_page(&parse_ctx, pages[at].as_ref().unwrap(), &furniture, &mut blocks).unwrap();
        (blocks, diagnostic_rows(&parse_ctx.into_diagnostics()))
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
        let with = |block: fn() -> PdfBlock| {
            let pages: Vec<_> = ["Body", "Other"]
                .into_iter()
                .map(|text| {
                    let mut page = page(&[]);
                    page.elements.push(block());
                    page.add_paragraph(Paragraph::with_text(text));
                    Some(read_page(page))
                })
                .collect();
            let furniture = Furniture::of(&pages);
            let mut blocks = Vec::new();
            map_page(&ParseCtx::default(), pages[0].as_ref().unwrap(), &furniture, &mut blocks)
                .unwrap();
            blocks.len()
        };
        assert_eq!(with(|| PdfBlock::Paragraph(Paragraph::heading("Chapter", 1))), 2);
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
        assert_eq!(numbers("Vol. 7, pp. 12-130"), [7, 12, 130]);
        assert_eq!(numbers("none"), [] as [u64; 0]);
        assert_eq!(numbers("99999999999999999999"), [u64::MAX]);
    }

    #[test]
    fn a_page_the_engine_could_not_read_needs_ocr_for_its_content() {
        let furniture = Furniture::of(&[]);
        assert_eq!(page_verdict(None, &furniture), needs_ocr(&[GapReason::UnreadableContent]));
        let read = read_page(page(&["Body"]));
        assert_eq!(page_verdict(Some(&read), &furniture), PageVerdict::Text);
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
            },
            InlineContent::Image { resource_id: "Im1".into(), alt_text: Some("alt".into()) },
            InlineContent::LineBreak,
        ];
        assert_eq!(
            inlines(&parse_ctx, &content).unwrap(),
            [
                Inline::Link {
                    content: vec![Inline::plain("site")],
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
    fn codes_a_font_gives_no_character_are_not_garbled() {
        let font = |name: &str, reason, runs| UnreadableFont { name: name.into(), reason, runs };
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
