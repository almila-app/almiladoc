//! Untrusted Markdown, such as a vision model's reply, read into blocks that keep its text and
//! structure and nothing that acts: raw HTML stays text, a link its label and an image its alt, and
//! TeX math (`\(..\)`, `\[..\]`, `$..$`, `$$..$$`, a `math` fence) becomes math.

use std::mem;
use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};
use unicode_normalization::UnicodeNormalization;

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{Block, Cell, Inline, List, ListItem, MarkerKind, Style, Table};
use crate::shared::otsl;
use crate::shared::text::trim_tex;

const OPTIONS: Options =
    Options::ENABLE_TABLES.union(Options::ENABLE_STRIKETHROUGH).union(Options::ENABLE_MATH);

pub(crate) fn blocks(
    ctx: &ParseCtx<'_>,
    markdown: &str,
    depth: u64,
) -> Result<Vec<Block>, ConvertError> {
    read(ctx, markdown, depth, Mode::Blocks)
}

pub(crate) fn blocks_with_tables(
    ctx: &ParseCtx<'_>,
    markdown: &str,
    depth: u64,
) -> Result<Vec<Block>, ConvertError> {
    read(ctx, markdown, depth, Mode::Tables)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Blocks,
    Tables,
    Cell,
}

fn read(
    ctx: &ParseCtx<'_>,
    markdown: &str,
    depth: u64,
    mode: Mode,
) -> Result<Vec<Block>, ConvertError> {
    let limits = ctx.limits();
    if markdown.len() as u64 > limits.max_ocr_text_bytes {
        return Err(ConvertError::limit(Limit::OcrTextBytes, limits.max_ocr_text_bytes));
    }
    if mode == Mode::Tables {
        ctx.charge(markdown.len() as u64)?;
    }
    let room = limits.max_nesting_depth.saturating_sub(depth);
    if lines(markdown).map(line_nesting).max().unwrap_or(0) > room {
        return Err(ConvertError::limit(Limit::NestingDepth, limits.max_nesting_depth));
    }
    ctx.admit_grid_positions(table_cells(markdown))?;
    if otsl::mentions(markdown) {
        return with_otsl(ctx, markdown, depth, mode);
    }
    segment(ctx, markdown, depth, mode)
}

fn segment(
    ctx: &ParseCtx<'_>,
    markdown: &str,
    depth: u64,
    mode: Mode,
) -> Result<Vec<Block>, ConvertError> {
    if let Some(Rewritten { text, marks, .. }) = rewrite(ctx, markdown)? {
        match build(ctx, &text, marks, depth, mode) {
            Ok((blocks, true)) => return Ok(blocks),
            Err(error) if error.is_fatal() => return Err(error),
            _ => {}
        }
    }
    build(ctx, markdown, Marks::default(), depth, mode).map(|(blocks, _)| blocks)
}

fn with_otsl(
    ctx: &ParseCtx<'_>,
    markdown: &str,
    depth: u64,
    mode: Mode,
) -> Result<Vec<Block>, ConvertError> {
    ctx.charge(markdown.len() as u64)?;
    let (regions, html, mut layout) = otsl_layout(markdown);
    if let Some(rewritten) = rewrite(ctx, markdown)? {
        layout.verbatim.extend(rewritten.sources);
    }
    for block in html {
        let source = markdown.get(block.clone()).ok_or(ConvertError::Internal)?;
        if otsl::mentions(source) {
            let math = continued_math(ctx, source)?;
            layout
                .verbatim
                .extend(math.into_iter().map(|at| at.start + block.start..at.end + block.start));
        }
    }
    let mut tokens = otsl::tokens(markdown, &layout);
    let (mut blocks, mut from) = (Vec::new(), 0);
    for region in regions {
        while let Some(run) = otsl::run(ctx, markdown, region.clone(), &layout.ends, &mut tokens)? {
            let above = markdown.get(from..run.start).ok_or(ConvertError::Internal)?;
            blocks.extend(segment(ctx, above, depth, mode)?);
            let table = otsl::table(ctx, markdown, &run, depth, |text| cell(ctx, text, depth))?;
            blocks.push(Block::Table(table));
            from = run.end;
        }
    }
    let below = markdown.get(from..).ok_or(ConvertError::Internal)?;
    blocks.extend(segment(ctx, below, depth, mode)?);
    Ok(blocks)
}

fn otsl_layout(markdown: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>, otsl::Layout) {
    let (mut regions, mut html, mut layout) = (Vec::new(), Vec::new(), otsl::Layout::default());
    let (mut containers, mut top) = (0_usize, false);
    for (event, range) in Parser::new_ext(markdown, OPTIONS).into_offset_iter() {
        match event {
            Event::Start(Tag::BlockQuote(_) | Tag::Item) => containers += 1,
            Event::End(TagEnd::BlockQuote(_) | TagEnd::Item) => {
                containers = containers.saturating_sub(1);
            }
            Event::Start(ref tag @ (Tag::Paragraph | Tag::HtmlBlock)) => {
                top = containers == 0;
                if matches!(tag, Tag::HtmlBlock) {
                    html.push(range.clone());
                }
                layout.cuts.push(range.start);
                layout.ends.push(range.end);
            }
            Event::End(TagEnd::Paragraph | TagEnd::HtmlBlock) => top = false,
            Event::Start(Tag::CodeBlock(_)) => layout.verbatim.push(range),
            Event::InlineMath(_) | Event::DisplayMath(_) => {
                if protected(markdown, &event, &range) {
                    layout.verbatim.push(range);
                }
            }
            Event::InlineHtml(_) | Event::Html(_) if top => regions.push(range),
            _ => {}
        }
    }
    (regions, html, layout)
}

fn protected(markdown: &str, event: &Event<'_>, range: &Range<usize>) -> bool {
    match event {
        Event::InlineMath(tex) => {
            !otsl::edged(tex) && !markdown.as_bytes().get(range.end).is_some_and(u8::is_ascii_digit)
        }
        Event::DisplayMath(_) => true,
        _ => false,
    }
}

const LEAD: &str = "x";
const CONTINUATION: &str = "\n    ";

/// Indents text under a lead line so that no line of it can open a Markdown block.
fn continued(ctx: &ParseCtx<'_>, text: &str) -> Result<String, ConvertError> {
    let verbatim = inline_verbatim(ctx, text)?;
    let mut out = String::with_capacity(text.len() + LEAD.len() + CONTINUATION.len());
    out.push_str(LEAD);
    out.push_str(CONTINUATION);
    let (mut from, mut cursor) = (0, 0);
    for (at, _) in text.match_indices('\n') {
        out.push_str(text.get(from..at).ok_or(ConvertError::Internal)?);
        out.push_str(if inside(&verbatim, &mut cursor, at) { " " } else { CONTINUATION });
        from = at + 1;
    }
    out.push_str(text.get(from..).ok_or(ConvertError::Internal)?);
    Ok(out)
}

fn continued_math(ctx: &ParseCtx<'_>, source: &str) -> Result<Vec<Range<usize>>, ConvertError> {
    let bytes = source.as_bytes();
    let text: String = source
        .char_indices()
        .map(|(at, c)| if c == '\r' && bytes.get(at + 1) != Some(&b'\n') { '\n' } else { c })
        .collect();
    let probe = continued(ctx, &text)?;
    let lines: Vec<usize> = probe.match_indices('\n').map(|(at, _)| at + 1).collect();
    let indent = CONTINUATION.len() - 1;
    let back = |at: usize| {
        let line = lines.partition_point(|&start| start <= at);
        at.saturating_sub(LEAD.len() + 1 + line * indent)
    };
    let mut math: Vec<Range<usize>> = Parser::new_ext(&probe, OPTIONS)
        .into_offset_iter()
        .filter(|(event, range)| protected(&probe, event, range))
        .map(|(_, range)| range)
        .collect();
    if let Some(rewritten) = rewrite(ctx, &probe)? {
        math.extend(rewritten.sources);
    }
    Ok(math.into_iter().map(|range| back(range.start)..back(range.end)).collect())
}

fn cell(ctx: &ParseCtx<'_>, text: &str, depth: u64) -> Result<Vec<Inline>, ConvertError> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let paragraph = continued(ctx, text.trim_matches([' ', '\t', '\n']))?;
    let mut blocks = segment(ctx, &paragraph, depth, Mode::Cell)?;
    let [Block::Paragraph(inlines)] = blocks.as_mut_slice() else {
        return Err(ConvertError::Internal);
    };
    let Some(Inline::Text { text: lead, .. }) = inlines.first_mut() else {
        return Err(ConvertError::Internal);
    };
    let lead_len = LEAD.len() + 1;
    if !lead.starts_with(LEAD) || lead.len() < lead_len {
        return Err(ConvertError::Internal);
    }
    lead.drain(..lead_len);
    if lead.is_empty() {
        inlines.remove(0);
    }
    Ok(mem::take(inlines))
}

fn inline_verbatim(ctx: &ParseCtx<'_>, text: &str) -> Result<Vec<Range<usize>>, ConvertError> {
    let heading = format!("# {}", text.replace('\n', " "));
    let mut verbatim: Vec<Range<usize>> = Parser::new_ext(&heading, OPTIONS)
        .into_offset_iter()
        .filter(|(event, _)| {
            matches!(
                event,
                Event::Code(_)
                    | Event::InlineMath(_)
                    | Event::DisplayMath(_)
                    | Event::InlineHtml(_)
            )
        })
        .map(|(_, range)| range)
        .collect();
    if let Some(rewritten) = rewrite(ctx, &heading)? {
        verbatim.extend(rewritten.sources);
    }
    verbatim.sort_unstable_by_key(|range| range.start);
    Ok(verbatim
        .into_iter()
        .map(|range| range.start.saturating_sub(2)..range.end.saturating_sub(2))
        .collect())
}

fn build(
    ctx: &ParseCtx<'_>,
    markdown: &str,
    mut marks: Marks,
    depth: u64,
    mode: Mode,
) -> Result<(Vec<Block>, bool), ConvertError> {
    let mut builder = Builder {
        depth,
        max_depth: ctx.limits().max_nesting_depth,
        inline_display: mode == Mode::Cell,
        ..Builder::default()
    };
    let (mut raw, mut cost) = (None, Cost::default());
    for (event, range) in Parser::new_ext(markdown, OPTIONS).into_offset_iter() {
        ctx.charge(cost.of(markdown, &event, &range))?;
        let Some(event) = marks.translate(event, &range) else {
            continue;
        };
        if mode == Mode::Tables {
            let source = markdown.get(range.clone()).ok_or(ConvertError::Internal)?;
            let prefix = markdown.get(..range.start).ok_or(ConvertError::Internal)?;
            table_event(ctx, &mut builder, &mut raw, event, source, prefix)?;
        } else {
            builder.event(event)?;
        }
    }
    builder.flush();
    Ok((builder.root, !marks.lost))
}

fn table_event(
    ctx: &ParseCtx<'_>,
    builder: &mut Builder,
    raw: &mut Option<String>,
    event: Event<'_>,
    source: &str,
    prefix: &str,
) -> Result<(), ConvertError> {
    match event {
        Event::Start(Tag::HtmlBlock) => *raw = Some(String::new()),
        Event::Html(text) if raw.is_some() => {
            if let Some(raw) = raw {
                raw.push_str(&text);
            }
        }
        Event::End(TagEnd::HtmlBlock) => {
            let text = raw.take().ok_or(ConvertError::Internal)?;
            if super::ocr_table::mentions_table(ctx, &text)? {
                if !builder.frames.is_empty() {
                    return Err(ConvertError::malformed("OCR tables must be top-level blocks"));
                }
                builder.room()?;
                builder.flush();
                for block in super::ocr_table::blocks(ctx, &text)? {
                    builder.push(block);
                }
            } else {
                builder.event(Event::Start(Tag::HtmlBlock))?;
                builder.event(Event::Html(text.into()))?;
                builder.event(Event::End(TagEnd::HtmlBlock))?;
            }
        }
        Event::InlineHtml(ref text) if super::ocr_table::mentions_table(ctx, text)? => {
            return Err(ConvertError::malformed("OCR tables must be standalone blocks"));
        }
        Event::Text(ref text)
            if text.contains('<')
                && !builder.in_raw()
                && super::ocr_table::unescaped_table(ctx, source, prefix)? =>
        {
            return Err(ConvertError::malformed("invalid OCR table markup"));
        }
        event => builder.event(event)?,
    }
    Ok(())
}

fn table_cells(markdown: &str) -> u64 {
    let (mut cells, mut below) = (0_u64, 0_u64);
    for line in lines(markdown) {
        if line.chars().all(|c| matches!(c, ' ' | '\t')) {
            below = 0;
            continue;
        }
        if line.contains('|')
            && line.contains('-')
            && line.chars().all(|c| matches!(c, ' ' | '\t' | '|' | '-' | ':' | '>'))
        {
            let columns = line.matches('|').count() as u64 + 1;
            cells = cells.saturating_add(columns.saturating_mul(below + 1));
        }
        below += 1;
    }
    cells
}

fn lines(markdown: &str) -> impl Iterator<Item = &str> {
    markdown.rsplit('\n').flat_map(|line| line.strip_suffix('\r').unwrap_or(line).rsplit('\r'))
}

fn line_nesting(line: &str) -> u64 {
    let (mut levels, mut columns) = (0_u64, 0_u64);
    let mut rest = line.as_bytes();
    loop {
        rest = match rest {
            [b' ', tail @ ..] => {
                columns += 1;
                tail
            }
            [b'\t', tail @ ..] => {
                columns += 4;
                tail
            }
            [b'>', tail @ ..] => {
                levels += 1;
                separated(tail)
            }
            _ => match list_marker(rest) {
                Some(tail) => {
                    levels += 1;
                    separated(tail)
                }
                None => break,
            },
        };
    }
    levels + columns / 2
}

fn list_marker(line: &[u8]) -> Option<&[u8]> {
    let digits = line.iter().take(10).take_while(|b| b.is_ascii_digit()).count();
    let tail = match (line, line.get(digits..)) {
        ([b'-' | b'+' | b'*', tail @ ..], _) => tail,
        (_, Some([b'.' | b')', tail @ ..])) if (1..=9).contains(&digits) => tail,
        _ => return None,
    };
    matches!(tail, [] | [b' ' | b'\t', ..]).then_some(tail)
}

fn separated(tail: &[u8]) -> &[u8] {
    match tail {
        [b' ' | b'\t', rest @ ..] => rest,
        _ => tail,
    }
}

/// pulldown's math option splits text at each `$` it leaves as text; such a split costs nothing,
/// so text without math costs the events a parse without the option yields.
#[derive(Default)]
struct Cost {
    last: Option<Piece>,
}

impl Cost {
    fn of(&mut self, markdown: &str, event: &Event<'_>, range: &Range<usize>) -> u64 {
        let Event::Text(text) = event else {
            self.last = None;
            return 1;
        };
        let piece = Piece::of(markdown, text, range);
        let split = self.last.is_some_and(|last| {
            last.end == range.start
                && ((last.dollar && piece.plain_start) || (piece.dollar && last.plain_end))
        });
        self.last = Some(piece);
        u64::from(!split)
    }
}

#[derive(Clone, Copy)]
struct Piece {
    end: usize,
    dollar: bool,
    plain_start: bool,
    plain_end: bool,
}

impl Piece {
    fn of(markdown: &str, text: &str, range: &Range<usize>) -> Self {
        let bytes = markdown.as_bytes();
        let source = markdown.get(range.clone()).unwrap_or_default();
        let dollar = source.starts_with('$');
        let plain = |at| dollar || (source == text && !delimiter(bytes, at));
        Self {
            end: range.end,
            dollar,
            plain_start: plain(range.start),
            plain_end: plain(range.end.saturating_sub(1)),
        }
    }
}

fn escaped(bytes: &[u8], at: usize) -> bool {
    run_before(bytes, at, b'\\') % 2 == 1
}

/// Whether pulldown-cmark 0.13 reads the byte at `at` as a delimiter, a text node of its own.
fn delimiter(bytes: &[u8], at: usize) -> bool {
    match bytes.get(at) {
        Some(b'`') => true,
        Some(b'*' | b'_' | b'[' | b']') => !escaped(bytes, at),
        Some(b'<') => !escaped(bytes, at) && bytes.get(at + 1) != Some(&b'\\'),
        Some(b'!') => bytes.get(at + 1) == Some(&b'['),
        Some(b'~') => {
            let start = at - run_before(bytes, at, b'~');
            let first = start + usize::from(escaped(bytes, start));
            at >= first && at + run_from(bytes, at, b'~') - first <= MAX_TILDE_DELIMITER
        }
        _ => false,
    }
}

const MAX_TILDE_DELIMITER: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Inline,
    Display,
    Fence,
    Pad,
}

#[derive(Default)]
struct Marks {
    spans: Vec<(usize, Mark)>,
    read: usize,
    lost: bool,
}

impl Marks {
    /// A mark no event reaches sat in a destination or definition, which nothing shows; one
    /// inside text or code it did not open means the rewrite was read back differently.
    fn translate<'a>(&mut self, event: Event<'a>, range: &Range<usize>) -> Option<Event<'a>> {
        if matches!(event, Event::End(_)) {
            return Some(event);
        }
        while self.spans.get(self.read).is_some_and(|&(at, _)| at < range.start) {
            self.read += 1;
        }
        let Some(&(at, mark)) = self.spans.get(self.read) else {
            return Some(event);
        };
        let opens = at == range.start;
        let event = match (event, mark) {
            (Event::Code(tex), Mark::Inline) if opens && !tex.trim().is_empty() => {
                Some(Event::InlineMath(tex))
            }
            (Event::Code(tex), Mark::Display) if opens && !tex.trim().is_empty() => {
                Some(Event::DisplayMath(tex))
            }
            (Event::Text(pad), Mark::Pad) if opens && *pad == *" " => None,
            (event @ Event::Start(Tag::CodeBlock(_)), Mark::Fence) if opens => Some(event),
            (event, _) => {
                self.lost |= !matches!(event, Event::Start(_)) && at < range.end;
                return Some(event);
            }
        };
        self.read += 1;
        event
    }
}

struct Rewritten {
    text: String,
    marks: Marks,
    sources: Vec<Range<usize>>,
}

fn rewrite(ctx: &ParseCtx<'_>, markdown: &str) -> Result<Option<Rewritten>, ConvertError> {
    let delimited = markdown
        .as_bytes()
        .windows(2)
        .any(|pair| matches!(pair, [b'\\', b'(' | b'['] | [b'$', b'$']));
    if !delimited {
        return Ok(None);
    }
    ctx.charge(markdown.len() as u64)?;
    let mut scan = Scan {
        ctx,
        text: markdown,
        zones: Zones::of(markdown),
        out: String::new(),
        copied: 0,
        next_tail: 0,
        marks: Vec::new(),
        sources: Vec::new(),
        shut: [0; 4],
        steps: 0,
    };
    scan.run()?;
    if scan.copied == 0 {
        return Ok(None);
    }
    scan.copy(markdown.len(), markdown.len())?;
    let marks = Marks { spans: scan.marks, ..Marks::default() };
    Ok(Some(Rewritten { text: scan.out, marks, sources: scan.sources }))
}

#[derive(Default)]
struct Zones {
    cuts: Vec<usize>,
    dead: Vec<Range<usize>>,
    items: Vec<(usize, usize)>,
    tails: Vec<Range<usize>>,
}

struct Label {
    kind: LinkType,
    text: usize,
    end: usize,
}

impl Zones {
    fn of(markdown: &str) -> Self {
        let mut zones = Self::default();
        let (mut items, mut labels) = (Vec::new(), Vec::new());
        for (event, range) in Parser::new_ext(markdown, OPTIONS).into_offset_iter() {
            zones.nest(markdown, &mut items, &event, &range);
            zones.label(markdown, &mut labels, &event, &range);
            match event {
                Event::Start(tag) => {
                    if matches!(
                        tag,
                        Tag::CodeBlock(_)
                            | Tag::HtmlBlock
                            | Tag::Link { link_type: LinkType::Autolink | LinkType::Email, .. }
                    ) {
                        zones.dead.push(range.clone());
                    }
                    if !matches!(
                        tag,
                        Tag::Emphasis
                            | Tag::Strong
                            | Tag::Strikethrough
                            | Tag::Link { .. }
                            | Tag::Image { .. }
                    ) {
                        zones.cuts.push(range.start);
                    }
                }
                Event::End(tag) => {
                    if !matches!(
                        tag,
                        TagEnd::Emphasis
                            | TagEnd::Strong
                            | TagEnd::Strikethrough
                            | TagEnd::Link
                            | TagEnd::Image
                    ) {
                        zones.cuts.push(range.end);
                    }
                }
                Event::Rule => zones.cuts.extend([range.start, range.end]),
                Event::Code(_)
                | Event::InlineHtml(_)
                | Event::InlineMath(_)
                | Event::DisplayMath(_) => zones.dead.push(range),
                _ => {}
            }
        }
        zones.cuts.sort_unstable();
        zones.dead.sort_unstable_by_key(|range| range.start);
        zones.tails.sort_unstable_by_key(|range| range.start);
        zones
    }

    fn nest(
        &mut self,
        markdown: &str,
        open: &mut Vec<usize>,
        event: &Event<'_>,
        range: &Range<usize>,
    ) {
        let at = match event {
            Event::Start(Tag::Item) => {
                open.push(content_column(markdown, range.start));
                range.start
            }
            Event::End(TagEnd::Item) => {
                open.pop();
                range.end
            }
            _ => return,
        };
        self.items.push((at, open.last().copied().unwrap_or(0)));
    }

    fn label(
        &mut self,
        markdown: &str,
        open: &mut Vec<Label>,
        event: &Event<'_>,
        range: &Range<usize>,
    ) {
        match event {
            Event::Start(Tag::Link { link_type, .. } | Tag::Image { link_type, .. }) => {
                let image = matches!(event, Event::Start(Tag::Image { .. }));
                let collapsed = usize::from(*link_type == LinkType::Collapsed);
                let (text, end) = (range.start + 1 + usize::from(image), range.end + 2 * collapsed);
                open.push(Label { kind: *link_type, text, end });
                return;
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                let Some(Label { kind, text, end }) = open.pop() else {
                    return;
                };
                let tail = markdown.get(text..end).unwrap_or_default();
                let inline = tail == "]" || (tail.starts_with("][") && tail.ends_with(']'));
                if matches!(kind, LinkType::Reference | LinkType::Collapsed | LinkType::Shortcut)
                    && inline
                {
                    self.tails.push(text..end);
                }
                if let Some(outer) = open.last_mut() {
                    outer.text = outer.text.max(end);
                }
                return;
            }
            _ => {}
        }
        if let Some(inner) = open.last_mut() {
            inner.text = inner.text.max(range.end);
        }
    }

    fn next_cut(&self, at: usize) -> Option<usize> {
        self.cuts.get(self.cuts.partition_point(|&cut| cut <= at)).copied()
    }

    fn last_cut(&self, at: usize) -> usize {
        let next = self.cuts.partition_point(|&cut| cut <= at);
        next.checked_sub(1).and_then(|last| self.cuts.get(last)).copied().unwrap_or(0)
    }

    fn item_column(&self, at: usize) -> usize {
        let next = self.items.partition_point(|&(start, _)| start <= at);
        next.checked_sub(1).and_then(|last| self.items.get(last)).map_or(0, |&(_, column)| column)
    }
}

fn content_column(markdown: &str, item: usize) -> usize {
    let bytes = markdown.as_bytes();
    let (before, rest) = bytes.split_at_checked(item).unwrap_or_default();
    let line = before.iter().rposition(|b| matches!(b, b'\n' | b'\r')).map_or(0, |end| end + 1);
    let indent = rest.iter().take_while(|b| matches!(b, b' ' | b'\t')).count();
    let (lead, rest) = rest.split_at_checked(indent).unwrap_or_default();
    let width = rest.iter().take_while(|b| b.is_ascii_digit()).count() + 1;
    let marker = columns(columns(0, before.get(line..).unwrap_or_default()), lead);
    let after = columns(marker, rest.get(..width).unwrap_or_default());
    let tail = rest.get(width..).unwrap_or_default();
    let gap = tail.iter().take_while(|b| matches!(b, b' ' | b'\t')).count();
    let spaces = columns(after, tail.get(..gap).unwrap_or_default()) - after;
    let blank = matches!(tail.get(gap), None | Some(b'\n' | b'\r'));
    after + if blank || spaces > CODE_INDENT { 1 } else { spaces }
}

const TAB_STOP: usize = 4;

fn columns(start: usize, bytes: &[u8]) -> usize {
    bytes.iter().fold(start, |column, &byte| advance(column, byte))
}

fn advance(column: usize, byte: u8) -> usize {
    if byte == b'\t' { column + TAB_STOP - column % TAB_STOP } else { column + 1 }
}

fn inside(dead: &[Range<usize>], cursor: &mut usize, at: usize) -> bool {
    while dead.get(*cursor).is_some_and(|range| range.end <= at) {
        *cursor += 1;
    }
    dead.get(*cursor).is_some_and(|range| range.start <= at)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Delim {
    Paren,
    Bracket,
    Dollar,
}

impl Delim {
    fn closes(self, byte: Option<&u8>) -> bool {
        matches!((self, byte), (Self::Paren, Some(b')')) | (Self::Bracket, Some(b']')))
    }

    fn slot(self, display: bool) -> usize {
        match (self, display) {
            (Self::Paren, _) => 0,
            (Self::Bracket, false) => 1,
            (Self::Bracket, true) => 2,
            (Self::Dollar, _) => 3,
        }
    }
}

const MAX_FENCE_INDENT: usize = 3;

/// The containers a line's prefix opens: quote markers, the column their content starts at
/// (`base`, with the innermost list item's `item` column), and the prefix's width and length.
#[derive(Clone, Copy)]
struct Margin {
    item: usize,
    base: usize,
    quotes: usize,
    column: usize,
    len: usize,
}
const CODE_INDENT: usize = MAX_FENCE_INDENT + 1;
const MIN_FENCE_TICKS: usize = 3;
const SPACE_ENTITY: &str = "&#32;";

struct Scan<'a> {
    ctx: &'a ParseCtx<'a>,
    text: &'a str,
    zones: Zones,
    out: String,
    copied: usize,
    next_tail: usize,
    marks: Vec<(usize, Mark)>,
    sources: Vec<Range<usize>>,
    shut: [usize; 4],
    steps: u64,
}

impl<'a> Scan<'a> {
    fn run(&mut self) -> Result<(), ConvertError> {
        let bytes = self.text.as_bytes();
        let (mut at, mut cursor) = (0, 0);
        while let Some(step) = bytes
            .get(at..)
            .and_then(|rest| rest.iter().position(|&byte| byte == b'\\' || byte == b'$'))
        {
            at += step;
            if inside(&self.zones.dead, &mut cursor, at) {
                at = self.zones.dead.get(cursor).map_or(at + 1, |range| range.end.max(at + 1));
            } else if bytes.get(at) == Some(&b'$') {
                at = self.dollar(at)?;
            } else {
                at = self.backslash(at)?;
            }
            self.ctx.charge(mem::take(&mut self.steps))?;
        }
        Ok(())
    }

    fn backslash(&mut self, at: usize) -> Result<usize, ConvertError> {
        let bytes = self.text.as_bytes();
        let run = run_from(bytes, at, b'\\');
        let next = at + run;
        if run.is_multiple_of(2) {
            return Ok(next);
        }
        match bytes.get(next) {
            Some(b'(') => self.open(next - 1, Delim::Paren),
            Some(b'[') => self.open(next - 1, Delim::Bracket),
            _ => Ok(next),
        }
    }

    fn dollar(&mut self, at: usize) -> Result<usize, ConvertError> {
        if self.text.as_bytes().get(at + 1) == Some(&b'$') {
            self.open(at, Delim::Dollar)
        } else {
            Ok(at + 1)
        }
    }

    fn open(&mut self, at: usize, delim: Delim) -> Result<usize, ConvertError> {
        let resume = at + 2;
        let display = delim.slot(true);
        if delim != Delim::Paren
            && at >= self.shut[display]
            && let Some(prefix) =
                self.line_prefix(at).filter(|prefix| self.indent(at, prefix) <= MAX_FENCE_INDENT)
        {
            let margin = self.margin(at, prefix);
            let outer = Margin { item: margin.item.min(margin.column), ..margin };
            match self.close(resume, delim, self.text.len(), Some(outer)) {
                Ok(end) => {
                    if let Some(after) = self.fence(at, end, prefix)? {
                        return Ok(after);
                    }
                }
                Err(stop) => self.shut[display] = stop,
            }
        }
        if delim == Delim::Dollar {
            return Ok(resume);
        }
        Ok(self.inline(at, delim)?.unwrap_or(resume))
    }

    fn inline(&mut self, at: usize, delim: Delim) -> Result<Option<usize>, ConvertError> {
        let slot = delim.slot(false);
        if at < self.shut[slot] {
            return Ok(None);
        }
        let limit = self.zones.next_cut(at).unwrap_or(self.text.len());
        match self.close(at + 2, delim, limit, None) {
            Ok(end) => {
                let after = self.span(at, end, delim)?;
                if after.is_none() {
                    self.shut[slot] = end + 2;
                }
                Ok(after)
            }
            Err(stop) => {
                self.shut[slot] = stop;
                Ok(None)
            }
        }
    }

    fn close(
        &mut self,
        from: usize,
        delim: Delim,
        limit: usize,
        outer: Option<Margin>,
    ) -> Result<usize, usize> {
        let bytes = self.text.as_bytes();
        let dead = &self.zones.dead;
        let mut cursor = dead.partition_point(|range| range.end <= from);
        let mut at = from;
        let found = loop {
            if at + 2 > limit {
                break Err(limit);
            }
            match bytes.get(at) {
                Some(b'\n' | b'\r') => {
                    at += if matches!(bytes.get(at..at + 2), Some(b"\r\n")) { 2 } else { 1 };
                    if outer.is_some_and(|outer| !self.continues(at, outer)) {
                        break Err(at);
                    }
                }
                Some(b'\\') => {
                    let run = run_from(bytes, at, b'\\');
                    let next = at + run;
                    let odd = run % 2 == 1;
                    if odd && delim.closes(bytes.get(next)) && !inside(dead, &mut cursor, at) {
                        break Ok(next - 1);
                    }
                    at = if odd && bytes.get(next) == Some(&b'$') { next + 1 } else { next };
                }
                Some(b'$')
                    if delim == Delim::Dollar
                        && bytes.get(at + 1) == Some(&b'$')
                        && !inside(dead, &mut cursor, at) =>
                {
                    break Ok(at);
                }
                Some(_) => at += 1,
                None => break Err(limit),
            }
        };
        self.steps += (at - from) as u64;
        found
    }

    fn continues(&self, line: usize, outer: Margin) -> bool {
        let text = self.text.get(line..).unwrap_or_default();
        let text = text.split(['\n', '\r']).next().unwrap_or_default();
        let margin = self.margin(line, text);
        margin.quotes >= outer.quotes
            && margin.column >= outer.item
            && text.get(margin.len..).is_some_and(|rest| !rest.trim().is_empty())
    }

    fn line_prefix(&self, at: usize) -> Option<&'a str> {
        let text = self.text;
        let before = text.as_bytes().get(..at)?;
        let start = match before.iter().rposition(|b| !matches!(b, b' ' | b'\t' | b'>')) {
            None => 0,
            Some(last) if matches!(before.get(last), Some(b'\n' | b'\r')) => last + 1,
            Some(_) => return None,
        };
        text.get(start..at)
    }

    fn base(&self, at: usize, prefix: &str) -> usize {
        self.margin(at, prefix).base
    }

    fn margin(&self, at: usize, line: &str) -> Margin {
        let item = self.zones.item_column(at);
        let bytes = line.as_bytes();
        let mut margin = Margin { item, base: item, quotes: 0, column: 0, len: bytes.len() };
        let mut quote = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            match byte {
                b' ' | b'\t' => {}
                b'>' if margin.column <= quote.max(item) + MAX_FENCE_INDENT => {
                    let spaced = matches!(bytes.get(index + 1), Some(b' ' | b'\t'));
                    quote = margin.column + 1 + usize::from(spaced);
                    margin.quotes += 1;
                }
                _ => {
                    margin.len = index;
                    break;
                }
            }
            margin.column = advance(margin.column, byte);
        }
        margin.base = quote.max(item);
        margin
    }

    fn blank(&self, from: usize, to: usize) -> bool {
        let Some(inner) = self.text.get(from..to) else {
            return false;
        };
        let mut lines = inner.split(['\n', '\r']);
        let first = lines.next().unwrap_or_default();
        let mut at = from + first.len();
        first.trim().is_empty()
            && lines.all(|line| {
                at += 1;
                let rest = line.get(self.margin(at, line).len..).unwrap_or_default();
                at += line.len();
                rest.trim().is_empty()
            })
    }

    fn indent(&self, at: usize, prefix: &str) -> usize {
        columns(0, prefix.as_bytes()).saturating_sub(self.base(at, prefix))
    }

    fn span(&mut self, at: usize, end: usize, delim: Delim) -> Result<Option<usize>, ConvertError> {
        let text = self.text;
        let inner = text.get(at + 2..end).ok_or(ConvertError::Internal)?;
        if self.blank(at + 2, end) {
            return Ok(None);
        }
        self.copy(at, self.zones.last_cut(at))?;
        if self.out.ends_with('`') {
            self.pad(" ")?;
        }
        let ticks = span_ticks(inner);
        // A line opening with three backticks and holding no other backtick opens a code fence.
        let fenced = ticks >= MIN_FENCE_TICKS && inner.contains(['\n', '\r']);
        if fenced {
            self.pad(SPACE_ENTITY)?;
        }
        let indent = match self.line_prefix(end) {
            Some(last) if fenced => {
                (self.base(end, last) + CODE_INDENT).saturating_sub(columns(0, last.as_bytes()))
            }
            _ => 0,
        };
        let kind = if delim == Delim::Paren { Mark::Inline } else { Mark::Display };
        self.marks.push((self.out.len(), kind));
        let bar = "`".repeat(ticks);
        let space = if inner.starts_with('`') || inner.ends_with('`') { " " } else { "" };
        self.emit(&[bar.as_str(), space, inner, " ".repeat(indent).as_str(), space, bar.as_str()])?;
        if text.as_bytes().get(end + 2) == Some(&b'`') {
            self.pad(" ")?;
        }
        self.sources.push(at..end + 2);
        self.copied = end + 2;
        Ok(Some(self.copied))
    }

    fn copy(&mut self, to: usize, escapes: usize) -> Result<(), ConvertError> {
        while let Some(tail) = self.zones.tails.get(self.next_tail).cloned() {
            if tail.start < self.copied {
                self.next_tail += 1;
                continue;
            }
            if tail.end > to {
                break;
            }
            self.copy_escaped(tail.start, escapes)?;
            // Written inline: a label with an escaped backtick no longer matches its definition.
            self.emit(&["]()"])?;
            (self.copied, self.next_tail) = (tail.end, self.next_tail + 1);
        }
        self.copy_escaped(to, escapes)
    }

    fn copy_escaped(&mut self, to: usize, escapes: usize) -> Result<(), ConvertError> {
        let (text, bytes) = (self.text, self.text.as_bytes());
        let leaf = escapes.max(self.copied).min(to);
        let mut cursor = self.zones.dead.partition_point(|range| range.end <= leaf);
        let mut from = self.copied;
        // A backtick run left as text before a span in its block would pair with a fence as long.
        for tick in (leaf..to).filter(|&tick| bytes.get(tick) == Some(&b'`')) {
            if !escaped(bytes, tick) && !inside(&self.zones.dead, &mut cursor, tick) {
                self.emit(&[text.get(from..tick).ok_or(ConvertError::Internal)?, "\\"])?;
                from = tick;
            }
        }
        self.emit(&[text.get(from..to).ok_or(ConvertError::Internal)?])?;
        self.copied = to;
        Ok(())
    }

    fn pad(&mut self, pad: &str) -> Result<(), ConvertError> {
        self.marks.push((self.out.len(), Mark::Pad));
        self.emit(&[pad])
    }

    fn emit(&mut self, pieces: &[&str]) -> Result<(), ConvertError> {
        self.ctx.charge(mem::take(&mut self.steps))?;
        self.ctx.charge(pieces.iter().map(|piece| piece.len() as u64).sum())?;
        self.out.extend(pieces.iter().copied());
        Ok(())
    }

    fn fence(
        &mut self,
        at: usize,
        end: usize,
        prefix: &str,
    ) -> Result<Option<usize>, ConvertError> {
        let text = self.text;
        let inner = text.get(at + 2..end).ok_or(ConvertError::Internal)?;
        if self.blank(at + 2, end) {
            return Ok(None);
        }
        let widest = inner.split(|c| c != '`').map(str::len).max().unwrap_or(0);
        let bar = "`".repeat((widest + 1).max(MIN_FENCE_TICKS));
        let rest = text.get(end + 2..).ok_or(ConvertError::Internal)?;
        let tail = rest.split(['\n', '\r']).next().unwrap_or_default();
        let (line, indent) = if tail.trim().is_empty() { ("", "") } else { ("\n", prefix) };
        self.copy(at, at)?;
        self.marks.push((self.out.len(), Mark::Fence));
        self.emit(&[
            bar.as_str(),
            "math\n",
            prefix,
            inner,
            "\n",
            prefix,
            bar.as_str(),
            line,
            indent,
        ])?;
        self.sources.push(at..end + 2);
        self.copied = end + 2;
        Ok(Some(self.copied))
    }
}

fn run_from(bytes: &[u8], at: usize, byte: u8) -> usize {
    bytes.get(at..).map_or(0, |rest| rest.iter().take_while(|&&b| b == byte).count())
}

fn run_before(bytes: &[u8], at: usize, byte: u8) -> usize {
    bytes.get(..at).map_or(0, |before| before.iter().rev().take_while(|&&b| b == byte).count())
}

fn span_ticks(markdown: &str) -> usize {
    // Runs of every length 1..=k need k(k+1)/2 backticks, so some length below the bound is free.
    let mut seen = vec![false; (2 * markdown.len()).isqrt() + 2];
    for run in markdown.split(|c| c != '`') {
        if let Some(seen) = seen.get_mut(run.len()) {
            *seen = true;
        }
    }
    seen.iter().skip(1).position(|&seen| !seen).map_or(1, |gap| gap + 1)
}

fn trim_edges(inlines: &mut Vec<Inline>) {
    while let Some(Inline::Text { text, .. }) = inlines.last_mut() {
        text.truncate(text.trim_end().len());
        if !text.is_empty() {
            break;
        }
        inlines.pop();
    }
    let mut blank = 0;
    for inline in inlines.iter_mut() {
        let Inline::Text { text, .. } = inline else {
            break;
        };
        text.drain(..text.len() - text.trim_start().len());
        if !text.is_empty() {
            break;
        }
        blank += 1;
    }
    inlines.drain(..blank);
}

enum Frame {
    Quote(Vec<Block>),
    List { start: Option<u64>, items: Vec<ListItem> },
    Item(Vec<Block>),
    Table { rows: Vec<Vec<Cell>>, head: usize },
    Row(Vec<Cell>),
}

#[derive(Clone, Copy)]
enum Leaf {
    Paragraph,
    Heading(u8),
    Code,
    Math,
    Html,
    Cell,
}

#[derive(Default)]
struct Builder {
    root: Vec<Block>,
    frames: Vec<Frame>,
    leaf: Option<Leaf>,
    inlines: Vec<Inline>,
    raw: String,
    bold: u32,
    italic: u32,
    strike: u32,
    split: bool,
    inline_display: bool,
    depth: u64,
    max_depth: u64,
}

impl Builder {
    fn event(&mut self, event: Event<'_>) -> Result<(), ConvertError> {
        match event {
            Event::Start(tag) => self.start(&tag)?,
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::Html(text) if self.in_raw() => self.raw.push_str(&text),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.text(&text, self.style());
            }
            Event::Code(text) => self.text(&text, Style { code: true, ..self.style() }),
            Event::InlineMath(tex) if tex.trim().is_empty() => self.blank_math("$", &tex),
            Event::DisplayMath(tex) if tex.trim().is_empty() => self.blank_math("$$", &tex),
            Event::InlineMath(tex) => self.inline_math(&tex),
            Event::DisplayMath(tex) => self.display_math(&tex),
            Event::SoftBreak => self.text(" ", self.style()),
            Event::HardBreak => self.inlines.push(Inline::LineBreak),
            Event::Rule => {
                self.flush();
                self.push(Block::Rule);
            }
            _ => {}
        }
        Ok(())
    }

    fn start(&mut self, tag: &Tag<'_>) -> Result<(), ConvertError> {
        match tag {
            Tag::Paragraph => self.open_leaf(Leaf::Paragraph),
            Tag::Heading { level, .. } => self.open_leaf(Leaf::Heading(*level as u8)),
            Tag::CodeBlock(CodeBlockKind::Fenced(info))
                if info.split_ascii_whitespace().next() == Some("math") =>
            {
                self.open_leaf(Leaf::Math);
            }
            Tag::CodeBlock(_) => self.open_leaf(Leaf::Code),
            Tag::HtmlBlock => self.open_leaf(Leaf::Html),
            Tag::TableCell => self.open_leaf(Leaf::Cell),
            Tag::BlockQuote(_) => self.open(Frame::Quote(Vec::new()))?,
            Tag::List(start) => self.open(Frame::List { start: *start, items: Vec::new() })?,
            Tag::Table(_) => self.open(Frame::Table { rows: Vec::new(), head: 0 })?,
            Tag::Item => {
                self.flush();
                self.frames.push(Frame::Item(Vec::new()));
            }
            Tag::TableHead => {
                if let Some(Frame::Table { head, .. }) = self.frames.last_mut() {
                    *head = 1;
                }
                self.frames.push(Frame::Row(Vec::new()));
            }
            Tag::TableRow => self.frames.push(Frame::Row(Vec::new())),
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::TableCell => self.close_leaf(),
            TagEnd::BlockQuote(_)
            | TagEnd::List(_)
            | TagEnd::Table
            | TagEnd::Item
            | TagEnd::TableHead
            | TagEnd::TableRow => self.close(),
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            _ => {}
        }
    }

    fn style(&self) -> Style {
        Style {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            ..Style::PLAIN
        }
    }

    fn text(&mut self, text: &str, style: Style) {
        match self.inlines.last_mut() {
            Some(Inline::Text { text: last, style: last_style }) if *last_style == style => {
                last.push_str(text);
            }
            _ => self.inlines.push(Inline::Text { text: text.to_owned(), style }),
        }
    }

    fn open_leaf(&mut self, leaf: Leaf) {
        self.flush();
        self.leaf = Some(leaf);
    }

    fn room(&self) -> Result<(), ConvertError> {
        if self.depth >= self.max_depth {
            return Err(ConvertError::limit(Limit::NestingDepth, self.max_depth));
        }
        Ok(())
    }

    fn open(&mut self, frame: Frame) -> Result<(), ConvertError> {
        self.flush();
        self.room()?;
        self.depth += 1;
        self.frames.push(frame);
        Ok(())
    }

    fn flush(&mut self) {
        if self.leaf.is_none() && (!self.inlines.is_empty() || self.split) {
            self.paragraph_rest();
        }
    }

    fn in_raw(&self) -> bool {
        matches!(self.leaf, Some(Leaf::Code | Leaf::Math | Leaf::Html))
    }

    fn paragraph_rest(&mut self) {
        let trim = mem::take(&mut self.split);
        self.paragraph(trim);
    }

    fn paragraph(&mut self, trim: bool) {
        let mut inlines = self.take_inlines();
        if trim {
            trim_edges(&mut inlines);
            if inlines.is_empty() {
                return;
            }
        }
        self.push(Block::Paragraph(inlines));
    }

    fn blank_math(&mut self, delimiter: &str, tex: &str) {
        let text = [delimiter, &tex.replace(['\n', '\r'], " "), delimiter].concat();
        self.text(&text, self.style());
    }

    fn inline_math(&mut self, tex: &str) {
        self.inlines.push(Inline::Math(trim_tex(tex).nfc().collect()));
    }

    fn display_math(&mut self, tex: &str) {
        if !self.inline_display && matches!(self.leaf, None | Some(Leaf::Paragraph)) {
            self.paragraph(true);
            self.push(Block::Math(trim_tex(tex).nfc().collect()));
            self.split = true;
        } else {
            self.inline_math(tex);
        }
    }

    fn take_inlines(&mut self) -> Vec<Inline> {
        let mut inlines = mem::take(&mut self.inlines);
        for inline in &mut inlines {
            if let Inline::Text { text, .. } = inline {
                *text = text.nfc().collect();
            }
        }
        inlines
    }

    fn close_leaf(&mut self) {
        let raw: String = mem::take(&mut self.raw).nfc().collect();
        let block = match self.leaf.take() {
            Some(Leaf::Heading(level)) => Block::heading(level, self.take_inlines()),
            Some(Leaf::Math) if !raw.trim().is_empty() => Block::Math(trim_tex(&raw).to_owned()),
            Some(Leaf::Code | Leaf::Math) => Block::CodeBlock { lang: None, text: raw },
            Some(Leaf::Html) => {
                Block::Paragraph(vec![Inline::plain(raw.trim_end_matches(['\n', '\r']))])
            }
            Some(Leaf::Cell) => {
                let inlines = self.take_inlines();
                if let Some(Frame::Row(cells)) = self.frames.last_mut() {
                    cells.push(Cell::new(vec![Block::Paragraph(inlines)]));
                }
                return;
            }
            Some(Leaf::Paragraph) | None => {
                self.paragraph_rest();
                return;
            }
        };
        self.push(block);
    }

    fn close(&mut self) {
        self.flush();
        let block = match self.frames.pop() {
            Some(Frame::Quote(blocks)) => Block::BlockQuote(blocks),
            Some(Frame::List { start, items }) => Block::List(List {
                marker: if start.is_some() { MarkerKind::Decimal } else { MarkerKind::Bullet },
                start: start.unwrap_or(1),
                items,
            }),
            Some(Frame::Table { rows, head }) => Block::Table(Table::from_rows(rows, head)),
            Some(Frame::Item(blocks)) => {
                if let Some(Frame::List { items, .. }) = self.frames.last_mut() {
                    items.push(ListItem { blocks, marker_label: None });
                }
                return;
            }
            Some(Frame::Row(cells)) => {
                if let Some(Frame::Table { rows, .. }) = self.frames.last_mut() {
                    rows.push(cells);
                }
                return;
            }
            None => return,
        };
        self.depth = self.depth.saturating_sub(1);
        self.push(block);
    }

    fn push(&mut self, block: Block) {
        match self.frames.last_mut() {
            Some(Frame::Quote(blocks) | Frame::Item(blocks)) => blocks.push(block),
            _ => self.root.push(block),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::Limits;
    use std::assert_matches;

    fn read(markdown: &str) -> Vec<Block> {
        blocks(&ParseCtx::default(), markdown, 0).unwrap()
    }

    fn para(text: &str) -> Block {
        Block::Paragraph(vec![Inline::plain(text)])
    }

    fn styled(text: &str, style: Style) -> Inline {
        Inline::Text { text: text.into(), style }
    }

    fn list(marker: MarkerKind, start: u64, items: Vec<Vec<Block>>) -> Block {
        let items =
            items.into_iter().map(|blocks| ListItem { blocks, marker_label: None }).collect();
        Block::List(List { marker, start, items })
    }

    #[test]
    fn what_acts_is_kept_as_its_text() {
        let cases: [(&str, Vec<Block>); 6] = [
            ("<script>", vec![para("<script>")]),
            ("x <b>y</b>", vec![para("x <b>y</b>")]),
            ("[a](javascript:x)", vec![para("a")]),
            ("![a](http://x)", vec![para("a")]),
            (
                "[*a*](x)",
                vec![Block::Paragraph(vec![styled("a", Style { italic: true, ..Style::PLAIN })])],
            ),
            (
                "<table><tr><td>x</td></tr></table>\n\n",
                vec![para("<table><tr><td>x</td></tr></table>")],
            ),
        ];
        for (markdown, expected) in cases {
            assert_eq!(read(markdown), expected, "{markdown}");
        }
    }

    #[test]
    fn structure_and_marks_are_kept() {
        let bold = Style { bold: true, ..Style::PLAIN };
        let struck = Style { strike: true, ..Style::PLAIN };
        let code = Style { code: true, ..Style::PLAIN };
        let cases: [(&str, Vec<Block>); 8] = [
            ("## H\n\n***", vec![Block::heading(2, vec![Inline::plain("H")]), Block::Rule]),
            (
                "**a** ~~b~~ `c`\\\nd",
                vec![Block::Paragraph(vec![
                    styled("a", bold),
                    Inline::plain(" "),
                    styled("b", struck),
                    Inline::plain(" "),
                    styled("c", code),
                    Inline::LineBreak,
                    Inline::plain("d"),
                ])],
            ),
            ("a\nb", vec![para("a b")]),
            (
                "```rust\nfn f() {}\n```",
                vec![Block::CodeBlock { lang: None, text: "fn f() {}\n".into() }],
            ),
            ("> a", vec![Block::BlockQuote(vec![para("a")])]),
            (
                "- a\n  - b\n- c",
                vec![list(
                    MarkerKind::Bullet,
                    1,
                    vec![
                        vec![para("a"), list(MarkerKind::Bullet, 1, vec![vec![para("b")]])],
                        vec![para("c")],
                    ],
                )],
            ),
            (
                "3. a\n\n   b\n4. c",
                vec![list(
                    MarkerKind::Decimal,
                    3,
                    vec![vec![para("a"), para("b")], vec![para("c")]],
                )],
            ),
            (
                "| x | y |\n| - | - |\n| 1 |",
                vec![Block::Table(Table::from_rows(
                    vec![
                        vec![Cell::new(vec![para("x")]), Cell::new(vec![para("y")])],
                        vec![Cell::new(vec![para("1")]), Cell::new(vec![Block::Paragraph(vec![])])],
                    ],
                    1,
                ))],
            ),
        ];
        for (markdown, expected) in cases {
            assert_eq!(read(markdown), expected, "{markdown}");
        }
    }

    #[test]
    fn a_tight_item_s_text_closes_before_the_block_that_follows_it() {
        let cases: [(&str, Vec<Block>); 2] = [
            (
                "- a\n  ## h",
                vec![list(
                    MarkerKind::Bullet,
                    1,
                    vec![vec![para("a"), Block::heading(2, vec![Inline::plain("h")])]],
                )],
            ),
            ("- a\n  ***", vec![list(MarkerKind::Bullet, 1, vec![vec![para("a"), Block::Rule]])]),
        ];
        for (markdown, expected) in cases {
            assert_eq!(read(markdown), expected, "{markdown}");
        }
    }

    #[test]
    fn text_is_composed_after_it_is_parsed_and_merged() {
        assert_eq!(read("e&#x301;"), [para("\u{e9}")]);
        assert_eq!(read(">\u{338}a"), [Block::BlockQuote(vec![para("\u{338}a")])]);
        assert_eq!(
            read("    e\u{301}\n"),
            [Block::CodeBlock { lang: None, text: "\u{e9}\n".into() }]
        );
    }

    #[test]
    fn nesting_counts_from_the_container_and_refuses_past_the_limit() {
        let limits = Limits { max_nesting_depth: 3, ..Limits::default() };
        let ctx = ParseCtx::new(&limits);
        let nested = |markdown: &str, depth| blocks(&ctx, markdown, depth);
        for markdown in ["> > > a", "- > > a", "> - - a", "> > | a |\n> > | - |"] {
            assert_eq!(nested(markdown, 0).map(|blocks| blocks.len()), Ok(1), "{markdown}");
            assert_matches!(
                nested(markdown, 1),
                Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 3 }),
                "{markdown}"
            );
        }
        assert_eq!(nested("> a\n\n> b\n\n> c\n\n> d", 2).map(|blocks| blocks.len()), Ok(4));
    }

    #[test]
    fn nesting_past_the_limit_is_refused_before_the_parse() {
        let lazy = ">".repeat(512 * 1024) + "a\n" + &"b\n".repeat(256 * 1024);
        let listed = format!("{}a\n{}", "- ".repeat(256 * 1024), "b\n".repeat(256 * 1024));
        for markdown in [&lazy, &listed].map(|text| text.get(..1 << 20).unwrap()) {
            let ctx = ParseCtx::default();
            assert_matches!(
                blocks(&ctx, markdown, 0),
                Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 256 })
            );
            assert_eq!(ctx.ledger().spent(), 0);
        }
        let quoted = ">".repeat(255);
        let ctx = ParseCtx::default();
        assert_matches!(
            blocks(&ctx, &quoted, 2),
            Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 256 })
        );
        assert_eq!(ctx.ledger().spent(), 0);
        assert_matches!(blocks(&ParseCtx::default(), &quoted, 1), Ok(_));
    }

    #[test]
    fn line_nesting_counts_each_marker_and_two_columns_of_indentation() {
        let cases = [
            ("a", 0),
            ("> > > a", 3),
            (">>a", 2),
            ("- + * a", 3),
            ("1. 2) 123456789. a", 3),
            ("1234567890. a", 0),
            ("1.a", 0),
            ("-a", 0),
            ("-", 1),
            ("-\ta", 1),
            (">\t> a", 2),
            ("  - a", 2),
            ("   - a", 2),
            ("\t- a", 3),
            ("> -   a", 3),
            ("x > a", 0),
        ];
        for (line, levels) in cases {
            assert_eq!(line_nesting(line), levels, "{line:?}");
        }
    }

    #[test]
    fn a_fill_past_its_byte_limit_is_refused_before_it_is_read() {
        let limits = Limits { max_ocr_text_bytes: 3, ..Limits::default() };
        let ctx = ParseCtx::new(&limits);
        assert_eq!(blocks(&ctx, "abc", 0), Ok(vec![para("abc")]));
        let spent = ctx.ledger().spent();
        assert_matches!(
            blocks(&ctx, "abcd", 0),
            Err(ConvertError::ResourceLimit { limit: Limit::OcrTextBytes, max: 3 })
        );
        assert_eq!(ctx.ledger().spent(), spent);
    }

    #[test]
    fn each_event_is_charged() {
        let ctx = ParseCtx::default();
        blocks(&ctx, "a *b*", 0).unwrap();
        assert_eq!(ctx.ledger().spent(), 6);
    }

    fn tex(text: &str) -> Inline {
        Inline::Math(text.into())
    }

    fn block_tex(text: &str) -> Block {
        Block::Math(text.into())
    }

    fn code(text: &str) -> Inline {
        styled(text, Style { code: true, ..Style::PLAIN })
    }

    fn code_block(text: &str) -> Block {
        Block::CodeBlock { lang: None, text: text.into() }
    }

    #[test]
    fn dollar_math_is_read_whole_between_its_delimiters() {
        assert_eq!(
            read("$a*b$ and $c*d$"),
            [Block::Paragraph(vec![tex("a*b"), Inline::plain(" and "), tex("c*d")])]
        );
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of inline spans by content")]
    fn inline_tex_delimiters_become_math_whatever_their_content() {
        let cases: [(&str, Vec<Inline>); 27] = [
            ("\\(a_1 * b_2\\)", vec![tex("a_1 * b_2")]),
            (
                "Area \\(a_1 * b_2\\) and",
                vec![Inline::plain("Area "), tex("a_1 * b_2"), Inline::plain(" and")],
            ),
            ("\\(\\left\\{ x \\right.\\)", vec![tex("\\left\\{ x \\right.")]),
            ("\\(\\text{\\$5}\\)", vec![tex("\\text{\\$5}")]),
            ("\\(f[x](y)\\)", vec![tex("f[x](y)")]),
            ("[\\(x\\)](u)", vec![tex("x")]),
            ("\\(x\\\\\\)", vec![tex("x\\\\")]),
            ("\\(a `b` c\\) `d`", vec![tex("a `b` c"), Inline::plain(" "), code("d")]),
            ("\\(a `\\)` b\\)", vec![tex("a `\\)` b")]),
            ("\\(`x\\)", vec![tex("`x")]),
            ("\\(x`\\)", vec![tex("x`")]),
            ("\\(e\u{301}\\)", vec![tex("\u{e9}")]),
            ("\\(x\\ \\)", vec![tex("x\\ ")]),
            ("\\(a\nb\\)", vec![tex("a b")]),
            ("$a \\(b\\) c$ \\(z\\)", vec![tex("a \\(b\\) c"), Inline::plain(" "), tex("z")]),
            ("\\(a\\)\\(b\\)", vec![tex("a"), tex("b")]),
            ("`a`\\(x\\)", vec![code("a"), tex("x")]),
            ("\\(x\\)`", vec![tex("x"), Inline::plain("`")]),
            (
                "`a` ``b`` \\(x\\)",
                vec![code("a"), Inline::plain(" "), code("b"), Inline::plain(" "), tex("x")],
            ),
            (
                "``quoted'' and `c` with \\(x\\)",
                vec![
                    Inline::plain("``quoted'' and "),
                    code("c"),
                    Inline::plain(" with "),
                    tex("x"),
                ],
            ),
            (
                "`a` ``b`` \\(`c`\\)\\(d\\)",
                vec![
                    code("a"),
                    Inline::plain(" "),
                    code("b"),
                    Inline::plain(" "),
                    tex("`c`"),
                    tex("d"),
                ],
            ),
            (
                "`a` ``b``\n\\(x\ny\\) \\(z\\)",
                vec![
                    code("a"),
                    Inline::plain(" "),
                    code("b"),
                    Inline::plain(" "),
                    tex("x y"),
                    Inline::plain(" "),
                    tex("z"),
                ],
            ),
            ("a ` b \\(x\\)", vec![Inline::plain("a ` b "), tex("x")]),
            ("a `\\(x\\)", vec![Inline::plain("a `"), tex("x")]),
            ("\\(a`b\\) \\(y\\)", vec![tex("a`b"), Inline::plain(" "), tex("y")]),
            ("\\(x\\) ` \\(y\\)", vec![tex("x"), Inline::plain(" ` "), tex("y")]),
            ("``` a \\(`x` ``y``\\)", vec![Inline::plain("``` a "), tex("`x` ``y``")]),
        ];
        for (markdown, expected) in cases {
            assert_eq!(read(markdown), [Block::Paragraph(expected)], "{markdown:?}");
        }
    }

    #[test]
    fn display_math_is_a_block_alone_and_splits_the_paragraph_around_it() {
        let cases: [(&str, Vec<Block>); 8] = [
            ("\\[ E = mc^2 \\tag{3} \\]", vec![block_tex("E = mc^2 \\tag{3}")]),
            ("a \\[ x \\] b", vec![para("a"), block_tex("x"), para("b")]),
            ("\\[ x \\]\n\\[ y \\]", vec![block_tex("x"), block_tex("y")]),
            ("$$ x $$ b", vec![block_tex("x"), para("b")]),
            (
                "x $$ a \\(b\\) $$ \\(z\\)",
                vec![para("x"), block_tex("a \\(b\\)"), Block::Paragraph(vec![tex("z")])],
            ),
            ("# a \\[ x \\]", vec![Block::heading(1, vec![Inline::plain("a "), tex("x")])]),
            ("- \\[ x \\]", vec![list(MarkerKind::Bullet, 1, vec![vec![block_tex("x")]])]),
            ("> \\[ x \\]", vec![Block::BlockQuote(vec![block_tex("x")])]),
        ];
        for (markdown, expected) in cases {
            assert_eq!(read(markdown), expected, "{markdown:?}");
        }
    }

    #[test]
    fn math_in_a_table_cell_stays_inline_and_reads_an_escaped_pipe() {
        let cell = |inlines| Cell::new(vec![Block::Paragraph(inlines)]);
        let table = |body| {
            Block::Table(Table::from_rows(vec![vec![cell(vec![Inline::plain("h")])], body], 1))
        };
        assert_eq!(read("| h |\n| - |\n| \\[ x \\] |"), [table(vec![cell(vec![tex("x")])])]);
        assert_eq!(
            read("| h |\n| - |\n| \\(a \\| b\\) |"),
            [table(vec![cell(vec![tex("a | b")])])]
        );
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the table of displays by the lines and containers they cross"
    )]
    fn a_display_opened_at_a_line_start_runs_to_its_closer_over_lines_that_start_blocks() {
        let nested = |blocks| {
            list(MarkerKind::Bullet, 1, vec![vec![para("a"), list(MarkerKind::Bullet, 1, blocks)]])
        };
        let cases: [(&str, Vec<Block>); 34] = [
            ("\\[\na &= b\n- c\n\\]", vec![block_tex("a &= b\n- c")]),
            ("$$\nx\n+ y\n$$", vec![block_tex("x\n+ y")]),
            ("$$\n\\sum_{i} x_i \\\\ y\n$$", vec![block_tex("\\sum_{i} x_i \\\\ y")]),
            (
                "\\[\n\\begin{aligned} a &= b \\\\ c \\end{aligned}\n\\]",
                vec![block_tex("\\begin{aligned} a &= b \\\\ c \\end{aligned}")],
            ),
            ("\\[ a\n# b\nc \\]", vec![block_tex("a\n# b\nc")]),
            ("a\n\\[\nx\n\\]\nb", vec![para("a"), block_tex("x"), para("b")]),
            ("\\[\nx\n\\] tail", vec![block_tex("x"), para("tail")]),
            ("\\[\n`x`\n\\]", vec![block_tex("`x`")]),
            ("\\[\na ``` b\n\\]", vec![block_tex("a ``` b")]),
            ("\\[\na `\\]` b\n\\]", vec![block_tex("a `\\]` b")]),
            ("$$\n```\n$$\n```\n$$", vec![block_tex("```\n$$\n```")]),
            ("$$\nx \\$$ y\n- z\n$$", vec![block_tex("x \\$$ y\n- z")]),
            ("\\[\n```\nx\n```\n\\]", vec![block_tex("```\nx\n```")]),
            ("> \\[\nx\n\\]", vec![Block::BlockQuote(vec![block_tex("x")])]),
            ("\\[\r\nx\r\n\\]", vec![block_tex("x")]),
            (
                "- item\n\n  \\[\n  a\n  - c\n  \\]",
                vec![list(MarkerKind::Bullet, 1, vec![vec![para("item"), block_tex("a\n- c")]])],
            ),
            ("> \\[\n> a\n> - c\n> \\]", vec![Block::BlockQuote(vec![block_tex("a\n- c")])]),
            ("a\n        \\[\n        x\n        \\]", vec![para("a"), block_tex("x")]),
            (" \\[\na &= b\n- c\n\\]", vec![block_tex("a &= b\n- c")]),
            ("\\[\n   a\n- c\n \\]", vec![block_tex("a\n- c")]),
            ("> \\[\n>a\n> - c\n>\\]", vec![Block::BlockQuote(vec![block_tex("a\n- c")])]),
            (">\\[\n> a\n>- c\n> \\]", vec![Block::BlockQuote(vec![block_tex("a\n- c")])]),
            (
                "- a\n  - b\n\n    \\[\n    x\n    - y\n    \\]",
                vec![nested(vec![vec![para("b"), block_tex("x\n- y")]])],
            ),
            (
                "10. a\n\n    \\[\n    x\n    - y\n    \\]",
                vec![list(MarkerKind::Decimal, 10, vec![vec![para("a"), block_tex("x\n- y")]])],
            ),
            (
                ">    \\[\n>    x\n>    - y\n>    \\]",
                vec![Block::BlockQuote(vec![block_tex("x\n- y")])],
            ),
            (
                "- a\n  - b\n        \\[\n        x\n        \\]",
                vec![nested(vec![vec![para("b"), block_tex("x")]])],
            ),
            (
                "10. a\n\n       \\[\n       x\n       - y\n       \\]",
                vec![list(MarkerKind::Decimal, 10, vec![vec![para("a"), block_tex("x\n- y")]])],
            ),
            (
                "-\n     \\[\n     x\n     - y\n     \\]",
                vec![list(MarkerKind::Bullet, 1, vec![vec![block_tex("x\n- y")]])],
            ),
            (
                "-     a\n\n  b\n      \\[\n      x\n      \\]",
                vec![list(
                    MarkerKind::Bullet,
                    1,
                    vec![vec![code_block("a\n"), para("b"), block_tex("x")]],
                )],
            ),
            (
                "1. a\n    - b\n\n        \\[\n        x\n        - y\n        \\]",
                vec![list(
                    MarkerKind::Decimal,
                    1,
                    vec![vec![
                        para("a"),
                        list(MarkerKind::Bullet, 1, vec![vec![para("b"), block_tex("x\n- y")]]),
                    ]],
                )],
            ),
            (
                "  10. a\n\n      \\[\n      x\n      - y\n      \\]",
                vec![list(MarkerKind::Decimal, 10, vec![vec![para("a"), block_tex("x\n- y")]])],
            ),
            (
                "a\n    >\\[\n    >x\n    >- y\n    >\\]",
                vec![para("a >"), block_tex(">x     >- y     >")],
            ),
            (
                "a\n    >\\[x\\]\n\\(y\\)",
                vec![para("a >"), block_tex("x"), Block::Paragraph(vec![tex("y")])],
            ),
            (
                "- a\n  - b\n\n  c\n      \\[\n      x\n      \\]",
                vec![list(
                    MarkerKind::Bullet,
                    1,
                    vec![vec![
                        para("a"),
                        list(MarkerKind::Bullet, 1, vec![vec![para("b")]]),
                        para("c"),
                        block_tex("x"),
                    ]],
                )],
            ),
        ];
        for (markdown, expected) in cases {
            assert_eq!(read(markdown), expected, "{markdown:?}");
        }
    }

    #[test]
    fn a_math_fence_is_math_and_any_other_fence_stays_code() {
        let cases: [(&str, Vec<Block>); 5] = [
            ("```math\n\\alpha_1\n```", vec![block_tex("\\alpha_1")]),
            ("```math\n \n```", vec![code_block(" \n")]),
            ("~~~math\n\\alpha_1\n~~~", vec![block_tex("\\alpha_1")]),
            ("```math\ne\u{301}\n```", vec![block_tex("\u{e9}")]),
            ("```latex\n\\alpha_1\n```", vec![code_block("\\alpha_1\n")]),
        ];
        for (markdown, expected) in cases {
            assert_eq!(read(markdown), expected, "{markdown:?}");
        }
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of runs that stay text")]
    fn what_is_not_a_pair_in_running_text_stays_the_text_it_was() {
        let cases: [(&str, Vec<Block>); 27] = [
            ("`\\(x\\)`", vec![Block::Paragraph(vec![code("\\(x\\)")])]),
            ("``a ` \\(x\\)``", vec![Block::Paragraph(vec![code("a ` \\(x\\)")])]),
            ("```\n\\(x\\)\n```", vec![code_block("\\(x\\)\n")]),
            (
                "- ```\n  \\(x\\)\n  ```\n\n\\(y\\)",
                vec![
                    list(MarkerKind::Bullet, 1, vec![vec![code_block("\\(x\\)\n")]]),
                    Block::Paragraph(vec![tex("y")]),
                ],
            ),
            ("    \\(x\\)", vec![code_block("\\(x\\)")]),
            ("\\\\(x\\\\)", vec![para("\\(x\\)")]),
            ("\\\\(x\\) \\(z\\)", vec![Block::Paragraph(vec![Inline::plain("\\(x) "), tex("z")])]),
            (
                "`\\(x` y\\) \\(z\\)",
                vec![Block::Paragraph(vec![code("\\(x"), Inline::plain(" y) "), tex("z")])],
            ),
            ("\\(x", vec![para("(x")]),
            ("\\(a \\(b \\(c", vec![para("(a (b (c")]),
            ("\\( \\)", vec![para("( )")]),
            ("> \\(\n> \\)", vec![Block::BlockQuote(vec![para("( )")])]),
            ("$$ $$", vec![para("$$ $$")]),
            ("$\u{a0}$", vec![para("$\u{a0}$")]),
            ("\\[\n\\]", vec![para("[ ]")]),
            ("\\[\n\n\\]", vec![para("["), para("]")]),
            ("$$\nx\n- y", vec![para("$$ x"), list(MarkerKind::Bullet, 1, vec![vec![para("y")]])]),
            ("<div>\\(x\\)</div>", vec![para("<div>\\(x\\)</div>")]),
            (
                "<http://a/\\(b\\)> \\(z\\)",
                vec![Block::Paragraph(vec![Inline::plain("http://a/\\(b\\) "), tex("z")])],
            ),
            (
                "a <span title=\"\\(x\\)\">b \\(z\\)",
                vec![Block::Paragraph(vec![
                    Inline::plain("a <span title=\"\\(x\\)\">b "),
                    tex("z"),
                ])],
            ),
            (
                "- a\n  - b\n\n        \\[\n        x\n        \\]",
                vec![list(
                    MarkerKind::Bullet,
                    1,
                    vec![vec![
                        para("a"),
                        list(
                            MarkerKind::Bullet,
                            1,
                            vec![vec![para("b"), code_block("\\[\nx\n\\]")]],
                        ),
                    ]],
                )],
            ),
            ("\\( \\[ x \\]", vec![para("("), block_tex("x")]),
            (
                "\\[\na\n\n- c\n\\]",
                vec![para("[ a"), list(MarkerKind::Bullet, 1, vec![vec![para("c ]")]])],
            ),
            ("> \\[\n> a\n>\n> b\n> \\]", vec![Block::BlockQuote(vec![para("[ a"), para("b ]")])]),
            (
                "> \\[\n> \\]\n\n\\(y\\)",
                vec![Block::BlockQuote(vec![para("[ ]")]), Block::Paragraph(vec![tex("y")])],
            ),
            (
                "- > \\[\n  > \\]\n\n\\(y\\)",
                vec![
                    list(MarkerKind::Bullet, 1, vec![vec![Block::BlockQuote(vec![para("[ ]")])]]),
                    Block::Paragraph(vec![tex("y")]),
                ],
            ),
            ("$${$$$[$$\\[$\\]", vec![para("$${$"), block_tex("["), para("[$]")]),
        ];
        for (markdown, expected) in cases {
            assert_eq!(read(markdown), expected, "{markdown:?}");
        }
    }

    #[test]
    fn a_span_needing_a_long_fence_still_spans_lines_without_opening_a_code_block() {
        let spaced =
            || Block::Paragraph(vec![Inline::plain("x "), tex("`a` ``b``"), Inline::plain(" y")]);
        let cases: [(&str, Vec<Block>); 9] = [
            ("\\(c\n`a` ``b``\\)", vec![Block::Paragraph(vec![tex("c `a` ``b``")])]),
            (
                "- \\(c\n  `a` ``b``\\)",
                vec![list(
                    MarkerKind::Bullet,
                    1,
                    vec![vec![Block::Paragraph(vec![tex("c `a` ``b``")])]],
                )],
            ),
            (
                "a\n\\(c\n`a` ``b``\\)",
                vec![Block::Paragraph(vec![Inline::plain("a "), tex("c `a` ``b``")])],
            ),
            (
                "> \\(c\n> `a` ``b``\\)",
                vec![Block::BlockQuote(vec![Block::Paragraph(vec![tex("c `a` ``b``")])])],
            ),
            (
                "x \\(`a` ``b``\nc\\) y",
                vec![Block::Paragraph(vec![
                    Inline::plain("x "),
                    tex("`a` ``b`` c"),
                    Inline::plain(" y"),
                ])],
            ),
            ("`d`\\(c\n`a` ``b``\\)", vec![Block::Paragraph(vec![code("d"), tex("c `a` ``b``")])]),
            ("x \\(`a` ``b``\n\\) y", vec![spaced()]),
            ("> x \\(`a` ``b``\n> \\) y", vec![Block::BlockQuote(vec![spaced()])]),
            (
                "- a\n  - x \\(`a` ``b``\n    \\) y",
                vec![list(
                    MarkerKind::Bullet,
                    1,
                    vec![vec![para("a"), list(MarkerKind::Bullet, 1, vec![vec![spaced()]])]],
                )],
            ),
        ];
        for (markdown, expected) in cases {
            assert_eq!(read(markdown), expected, "{markdown:?}");
        }
    }

    #[test]
    fn a_reference_link_before_math_keeps_its_label_whatever_its_backticks() {
        let cases: [(&str, Vec<Inline>); 7] = [
            ("[a`b] \\(x\\)\n\n[a`b]: /u", vec![Inline::plain("a`b "), tex("x")]),
            (
                "[t `c`][a`b] \\(x\\)\n\n[a`b]: /u",
                vec![Inline::plain("t "), code("c"), Inline::plain(" "), tex("x")],
            ),
            ("![a`b][] \\(x\\)\n\n[a`b]: /u", vec![Inline::plain("a`b "), tex("x")]),
            ("[a`]: \\(u\\)\n\n[a`]", vec![Inline::plain("a`")]),
            ("[\\(x\\) a`b][r]\n\n[r]: /u", vec![tex("x"), Inline::plain(" a`b")]),
            ("[![a`][i]][l] \\(x\\)\n\n[i]: /i\n[l]: /l", vec![Inline::plain("a` "), tex("x")]),
            ("[a\\`b] \\(x\\)\n\n[a`b]: /u", vec![Inline::plain("[a`b] "), tex("x")]),
        ];
        for (markdown, expected) in cases {
            assert_eq!(read(markdown), [Block::Paragraph(expected)], "{markdown:?}");
        }
    }

    #[test]
    fn a_pair_in_a_destination_or_definition_leaves_with_it() {
        let cases: [(&str, Vec<Block>); 4] = [
            ("[a](\\(x\\))", vec![para("a")]),
            (
                "[a](\\(x\\)) and \\(y\\)",
                vec![Block::Paragraph(vec![Inline::plain("a and "), tex("y")])],
            ),
            ("[x]: \\(u\\)\n\\(y\\)", vec![Block::Paragraph(vec![tex("y")])]),
            ("\\(x\\)\n\n[\\(y\\)]: /u", vec![Block::Paragraph(vec![tex("x")])]),
        ];
        for (markdown, expected) in cases {
            assert_eq!(read(markdown), expected, "{markdown:?}");
        }
    }

    #[test]
    fn a_pair_never_spans_the_blocks_between_its_delimiters() {
        let tail = || Block::Paragraph(vec![Inline::plain("y) "), tex("z")]);
        let cases: [(&str, Vec<Block>); 8] = [
            (
                "- a \\(x\n- y\\) \\(z\\)",
                vec![list(MarkerKind::Bullet, 1, vec![vec![para("a (x")], vec![tail()]])],
            ),
            (
                "# a \\(x\ny\\) \\(z\\)",
                vec![Block::heading(1, vec![Inline::plain("a (x")]), tail()],
            ),
            (
                "\\(x\n- y\\) \\(z\\)",
                vec![para("(x"), list(MarkerKind::Bullet, 1, vec![vec![tail()]])],
            ),
            ("\\(x\n---\ny\\) \\(z\\)", vec![Block::heading(2, vec![Inline::plain("(x")]), tail()]),
            ("\\(x\n***\ny\\) \\(z\\)", vec![para("(x"), Block::Rule, tail()]),
            (
                "- a \\(x\n  ***\n  y\\) \\(z\\)",
                vec![list(MarkerKind::Bullet, 1, vec![vec![para("a (x"), Block::Rule, tail()]])],
            ),
            (
                "- a \\(x\n  ## y\\) \\(z\\)",
                vec![list(
                    MarkerKind::Bullet,
                    1,
                    vec![vec![
                        para("a (x"),
                        Block::heading(2, vec![Inline::plain("y) "), tex("z")]),
                    ]],
                )],
            ),
            (
                "- # a \\(x\n  y\\) \\(z\\)",
                vec![list(
                    MarkerKind::Bullet,
                    1,
                    vec![vec![Block::heading(1, vec![Inline::plain("a (x")]), tail()]],
                )],
            ),
        ];
        for (markdown, expected) in cases {
            assert_eq!(read(markdown), expected, "{markdown:?}");
        }
        let row = vec![Cell::new(vec![para("(x")]), Cell::new(vec![tail()])];
        assert_eq!(
            read("| \\(x | y\\) \\(z\\) |\n| - | - |"),
            [Block::Table(Table::from_rows(vec![row], 1))]
        );
    }

    #[test]
    fn math_resumes_after_the_html_block_or_fence_that_hid_it() {
        assert_eq!(
            read("<div>\n\\(x\\)\n\n\\(y\\)"),
            [para("<div>\n\\(x\\)"), Block::Paragraph(vec![tex("y")])]
        );
        assert_eq!(
            read("```\nx\n```\n\\(y\\)"),
            [code_block("x\n"), Block::Paragraph(vec![tex("y")])]
        );
    }

    #[test]
    fn a_rewrite_charges_the_bytes_it_scans_and_makes_beside_its_events() {
        let ctx = ParseCtx::default();
        blocks(&ctx, "\\(x\\)", 0).unwrap();
        let (input, searched, rewritten, events) = (5, 1, 3, 3);
        assert_eq!(ctx.ledger().spent(), input + searched + rewritten + events);
        let native = ParseCtx::default();
        blocks(&native, "$x$", 0).unwrap();
        assert_eq!(native.ledger().spent(), 3);
        for unpaired in ["\\(x", "$$ $$"] {
            let ctx = ParseCtx::default();
            blocks(&ctx, unpaired, 0).unwrap();
            let events =
                Parser::new_ext(unpaired, OPTIONS.difference(Options::ENABLE_MATH)).count();
            assert_eq!(ctx.ledger().spent(), (unpaired.len() + events) as u64, "{unpaired:?}");
        }
    }

    #[test]
    fn a_dollar_left_as_text_costs_the_events_it_cost_without_math() {
        let cases = [
            ("$5 and $6", 3),
            ("price $5.", 3),
            ("cost: $5, $6, $7 and $8", 3),
            ("*$5", 4),
            ("\\*$5", 3),
            ("`$5", 4),
            ("$]", 4),
            ("<$5", 4),
            ("$<\\x", 3),
            ("$![x", 5),
            ("!$5", 3),
            ("~$5", 4),
            ("a~~~$5", 3),
            ("\\~~~$5", 6),
            ("&amp;$5", 4),
        ];
        for (markdown, events) in cases {
            let ctx = ParseCtx::default();
            blocks(&ctx, markdown, 0).unwrap();
            let plain = Parser::new_ext(markdown, OPTIONS.difference(Options::ENABLE_MATH)).count();
            assert_eq!((ctx.ledger().spent(), plain as u64), (events, events), "{markdown:?}");
        }
    }

    const PIECES: &[&str] = &[
        "$", "$$", "*", "_", "~", "~~", "`", "``", "```\n", "\n```", "[", "]", "](", "!", "![",
        "<", ">", "<b>", "</b>", "<!--", "&", "&amp;", "&#36;", ";", "\\", "\\$", "\\*", "\\`",
        "\\~", " ", "\t", "\n", "\n\n", "\r\n", "|", "| - |\n", "-", "- ", "> ", "1. ", "    ",
        "#", "# ", " #", "\n## ", "(", ")", "{", "}", "^", "a", "1", ":", ".", "\0", "http://a",
    ];

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(2048))]
        #[test]
        fn text_without_math_costs_the_events_a_parse_without_math_yields(
            pieces in proptest::collection::vec(proptest::sample::select(PIECES), 1..32)
        ) {
            let markdown = pieces.concat();
            let events: Vec<_> = Parser::new_ext(&markdown, OPTIONS).into_offset_iter().collect();
            let math = events
                .iter()
                .any(|(event, _)| matches!(event, Event::InlineMath(_) | Event::DisplayMath(_)));
            let mut cost = Cost::default();
            let spent: u64 = events.iter().map(|(event, range)| cost.of(&markdown, event, range)).sum();
            let plain = Parser::new_ext(&markdown, OPTIONS.difference(Options::ENABLE_MATH)).count();
            if !math {
                proptest::prop_assert_eq!(spent, plain as u64, "{:?}", markdown);
            }
        }
    }

    #[test]
    fn a_scan_is_charged_by_its_bytes_and_hostile_openers_stay_linear_and_deterministic() {
        const SIZE: usize = 1 << 20;
        let lone = format!("\\({}", "a".repeat(SIZE - 2));
        let pairs = "\\(a\\)".repeat(SIZE / 5);
        let hostile = [
            lone,
            "\\( ".repeat(SIZE / 3),
            "\\[\n".repeat(SIZE / 3),
            "$$\n".repeat(SIZE / 3),
            "\\( \\[ ".repeat(SIZE / 6),
            "\\) \\] ".repeat(SIZE / 6),
            pairs.clone(),
        ];
        let linear = Limits { max_work: 8 << 20, ..Limits::default() };
        for text in &hostile {
            let ctx = ParseCtx::new(&linear);
            assert_matches!(blocks(&ctx, text, 0), Ok(_));
            let max = (text.len() as u64 + 1000).min(ctx.ledger().spent() - 1);
            let tight = Limits { max_work: max, ..Limits::default() };
            let run = || {
                let ctx = ParseCtx::new(&tight);
                (blocks(&ctx, text, 0), ctx.ledger().spent())
            };
            let first = run();
            assert_eq!(first.0, Err(ConvertError::limit(Limit::Work, max)), "{:?}", text.get(..12));
            assert_eq!(run(), first);
        }
        let max = pairs.len() as u64 + 1000;
        let limits = Limits { max_work: max, ..Limits::default() };
        let ctx = ParseCtx::new(&limits);
        assert_eq!(blocks(&ctx, &pairs, 0), Err(ConvertError::limit(Limit::Work, max)));
        assert_eq!(ctx.ledger().spent(), max + 1, "the scan stops at the opener past the limit");
    }

    const SPANS: usize = 100_000;

    fn fenced_runs_then_spans() -> String {
        let runs: Vec<_> = (1..=1000).map(|run| "`".repeat(run)).collect();
        format!("~~~\n{}\n~~~\n{}", runs.join(" "), "\\(x\\)".repeat(SPANS))
    }

    #[test]
    fn a_span_fence_is_sized_by_its_own_body_so_a_rewrite_stays_linear() {
        let hostile = fenced_runs_then_spans();
        let (delimiters, fences, pads) = (4 * SPANS, 2 * SPANS, SPANS - 1);
        assert_eq!(
            rewrite(&ParseCtx::default(), &hostile).map(|out| out.map(|out| out.text.len())),
            Ok(Some(hostile.len() - delimiters + fences + pads))
        );
        assert_eq!(read(&hostile).get(1), Some(&Block::Paragraph(vec![tex("x"); SPANS])));
    }

    #[test]
    fn a_rewrite_trips_the_work_limit_before_it_writes_past_it() {
        let hostile = fenced_runs_then_spans();
        let max = hostile.len() as u64 * 7 / 4;
        let limits = Limits { max_work: max, ..Limits::default() };
        let ctx = ParseCtx::new(&limits);
        assert_eq!(rewrite(&ctx, &hostile).err(), Some(ConvertError::limit(Limit::Work, max)));
        let spent = ctx.ledger().spent();
        assert!(spent <= max + 3, "one span past the limit at most, not {}", spent - max);
    }

    #[test]
    fn table_cells_bound_every_row_up_to_a_blank_line_whatever_its_line_endings() {
        let cases = [
            ("a\n\n| x | y |\n| - | - |\n1\n2\n\n3", 4 * 3),
            ("| x | y |\r\n|---|---|\r\n1\r\n2", 4 * 3),
            ("x|y\r-|-\r1\r \r2", 2 * 2),
            ("> x|y\n> -|:-:\n> 1", 2 * 2),
            ("- a\n- -\n| x |", 0),
            ("x | y\n- | - | 2", 0),
            ("x\n|-\n|-\n1", 2 * 3 + 2 * 2),
            ("| |\n| |\n1", 0),
            ("x|y\n-|-\n1\n\t\n2", 2 * 2),
        ];
        for (markdown, cells) in cases {
            assert_eq!(table_cells(markdown), cells, "{markdown:?}");
        }
    }

    #[test]
    fn tables_padded_past_the_grid_slots_are_refused_before_the_parse() {
        let table = format!("{}\n{}\n{}", "|a".repeat(64), "|-".repeat(64), "a\n".repeat(63));
        let limits = Limits { max_grid_slots: 65 * 64, ..Limits::default() };
        let ctx = ParseCtx::new(&limits);
        assert_eq!(blocks(&ctx, &table, 0).map(|blocks| blocks.len()), Ok(1));
        assert_matches!(
            blocks(&ParseCtx::new(&limits), &format!("{table}a"), 0),
            Err(ConvertError::ResourceLimit { limit: Limit::GridSlots, .. })
        );
    }
}
