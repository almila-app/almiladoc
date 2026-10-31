//! Untrusted Markdown, such as a vision model's reply, read into blocks that keep its text and
//! structure and nothing that acts: raw HTML stays text, a link its label and an image its alt.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use unicode_normalization::UnicodeNormalization;

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{Block, Cell, Inline, List, ListItem, MarkerKind, Style, Table};

/// The blocks `markdown` holds, read for a container `depth` deep, each event charged to `ctx`.
pub(crate) fn blocks(
    ctx: &ParseCtx<'_>,
    markdown: &str,
    depth: u64,
) -> Result<Vec<Block>, ConvertError> {
    read(ctx, markdown, depth, false)
}

pub(crate) fn blocks_with_tables(
    ctx: &ParseCtx<'_>,
    markdown: &str,
    depth: u64,
) -> Result<Vec<Block>, ConvertError> {
    read(ctx, markdown, depth, true)
}

fn read(
    ctx: &ParseCtx<'_>,
    markdown: &str,
    depth: u64,
    tables: bool,
) -> Result<Vec<Block>, ConvertError> {
    let limits = ctx.limits();
    if markdown.len() as u64 > limits.max_ocr_text_bytes {
        return Err(ConvertError::limit(Limit::OcrTextBytes, limits.max_ocr_text_bytes));
    }
    if tables {
        ctx.charge(markdown.len() as u64)?;
    }
    let room = limits.max_nesting_depth.saturating_sub(depth);
    if lines(markdown).map(line_nesting).max().unwrap_or(0) > room {
        return Err(ConvertError::limit(Limit::NestingDepth, limits.max_nesting_depth));
    }
    ctx.admit_grid_positions(table_cells(markdown))?;
    let mut builder = Builder { depth, max_depth: limits.max_nesting_depth, ..Builder::default() };
    let mut raw = None;
    for (event, range) in
        Parser::new_ext(markdown, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH)
            .into_offset_iter()
    {
        ctx.charge(1)?;
        if tables {
            let source = markdown.get(range.clone()).ok_or(ConvertError::Internal)?;
            let prefix = markdown.get(..range.start).ok_or(ConvertError::Internal)?;
            table_event(ctx, &mut builder, &mut raw, event, source, prefix)?;
        } else {
            builder.event(event)?;
        }
    }
    builder.flush();
    Ok(builder.root)
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
                if builder.depth > 0 {
                    return Err(ConvertError::malformed("OCR tables must be top-level blocks"));
                }
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
                && !matches!(builder.leaf, Some(Leaf::Code))
                && super::ocr_table::unescaped_table(ctx, source, prefix)? =>
        {
            return Err(ConvertError::malformed("invalid OCR table markup"));
        }
        event => builder.event(event)?,
    }
    Ok(())
}

/// The most cells the tables of `markdown` can hold, short rows padded: the parser builds every
/// padding cell before its first event. A delimiter row of `n` pipes heads at most `n + 1`
/// columns over its header and every line up to the next blank one.
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

/// The lines of `markdown`, last first.
fn lines(markdown: &str) -> impl Iterator<Item = &str> {
    markdown.rsplit('\n').flat_map(|line| line.strip_suffix('\r').unwrap_or(line).rsplit('\r'))
}

/// The most containers `line`'s leading quote and list markers and indentation can open, which
/// bounds the parser's work per line before its first event.
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
    depth: u64,
    max_depth: u64,
}

impl Builder {
    fn event(&mut self, event: Event<'_>) -> Result<(), ConvertError> {
        match event {
            Event::Start(tag) => self.start(&tag)?,
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::Html(text)
                if matches!(self.leaf, Some(Leaf::Code | Leaf::Html)) =>
            {
                self.raw.push_str(&text);
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.text(&text, self.style());
            }
            Event::Code(text) => self.text(&text, Style { code: true, ..self.style() }),
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

    fn open(&mut self, frame: Frame) -> Result<(), ConvertError> {
        self.flush();
        if self.depth >= self.max_depth {
            return Err(ConvertError::limit(Limit::NestingDepth, self.max_depth));
        }
        self.depth += 1;
        self.frames.push(frame);
        Ok(())
    }

    /// Text a tight list item holds outside any paragraph, as one.
    fn flush(&mut self) {
        if self.leaf.is_none() && !self.inlines.is_empty() {
            let inlines = self.take_inlines();
            self.push(Block::Paragraph(inlines));
        }
    }

    fn take_inlines(&mut self) -> Vec<Inline> {
        let mut inlines = std::mem::take(&mut self.inlines);
        for inline in &mut inlines {
            if let Inline::Text { text, .. } = inline {
                *text = text.nfc().collect();
            }
        }
        inlines
    }

    fn close_leaf(&mut self) {
        let raw: String = std::mem::take(&mut self.raw).nfc().collect();
        let block = match self.leaf.take() {
            Some(Leaf::Heading(level)) => Block::heading(level, self.take_inlines()),
            Some(Leaf::Code) => Block::CodeBlock { lang: None, text: raw },
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
            Some(Leaf::Paragraph) | None => Block::Paragraph(self.take_inlines()),
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
