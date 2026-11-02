//! OTSL table markup, the cell tags and `<nl>` row breaks that vision models write in place of
//! Markdown tables, read into model tables.

use std::collections::{HashMap, VecDeque};
use std::iter::Peekable;
use std::mem;
use std::ops::Range;

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{Block, Cell, GridBuilder, Inline, Table};
use crate::shared::tag_scan;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Origin,
    Header,
    Left,
    Up,
    Cross,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Cell(Kind),
    Break,
}

const TAGS: [(&[u8], Tag); 9] = [
    (b"<fcel>", Tag::Cell(Kind::Origin)),
    (b"<ecel>", Tag::Cell(Kind::Origin)),
    (b"<rhed>", Tag::Cell(Kind::Origin)),
    (b"<srow>", Tag::Cell(Kind::Origin)),
    (b"<ched>", Tag::Cell(Kind::Header)),
    (b"<lcel>", Tag::Cell(Kind::Left)),
    (b"<ucel>", Tag::Cell(Kind::Up)),
    (b"<xcel>", Tag::Cell(Kind::Cross)),
    (b"<nl>", Tag::Break),
];

fn tag_at(text: &[u8]) -> Option<(Tag, usize)> {
    TAGS.iter()
        .find(|(source, _)| text.starts_with(source))
        .map(|&(source, tag)| (tag, source.len()))
}

pub(crate) fn edged(text: &str) -> bool {
    let text = text.as_bytes();
    tag_at(text).is_some() || TAGS.iter().any(|(source, _)| text.ends_with(source))
}

pub(crate) fn mentions(markdown: &str) -> bool {
    let bytes = markdown.as_bytes();
    markdown
        .match_indices('<')
        .any(|(at, _)| matches!(bytes.get(at..).and_then(tag_at), Some((Tag::Cell(_), _))))
}

#[derive(Clone, Copy)]
pub(crate) struct Token {
    tag: Tag,
    start: usize,
    end: usize,
}

#[derive(Default)]
pub(crate) struct Layout {
    pub(crate) verbatim: Vec<Range<usize>>,
    pub(crate) cuts: Vec<usize>,
    pub(crate) ends: Vec<usize>,
}

pub(crate) fn tokens<'a>(
    text: &str,
    layout: &'a Layout,
) -> Peekable<impl Iterator<Item = Token> + 'a> {
    Scan::new(text, layout).peekable()
}

struct Scan<'a> {
    live: Vec<u8>,
    cuts: &'a [usize],
    at: usize,
    ticks: Ticks,
}

impl Iterator for Scan<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Token> {
        loop {
            let ahead = self.live.get(self.at..)?;
            let start = self.at + ahead.iter().position(|b| matches!(b, b'<' | b'`' | b'\\'))?;
            let rest = self.live.get(start..)?;
            self.at = match rest {
                [b'\\', escaped, ..] if escaped.is_ascii_punctuation() => start + 2,
                [b'`', ..] => self.code_span(start),
                [b'<', after @ ..] => {
                    if let Some(len) = [&b"!-->"[..], b"!--->"]
                        .into_iter()
                        .find_map(|empty| after.starts_with(empty).then_some(empty.len()))
                    {
                        start + 1 + len
                    } else if let Some(past) = tag_scan::past_markup(after) {
                        self.live.len() - past.len()
                    } else if let Some((tag, len)) = tag_at(rest) {
                        self.at = start + len;
                        return Some(Token { tag, start, end: self.at });
                    } else {
                        start + 1 + html_tag_len(after).max(autolink_len(after))
                    }
                }
                _ => start + 1,
            };
        }
    }
}

impl<'a> Scan<'a> {
    fn new(text: &str, layout: &'a Layout) -> Self {
        let mut live = text.as_bytes().to_vec();
        for range in &layout.verbatim {
            for byte in live.get_mut(range.clone()).into_iter().flatten() {
                if !matches!(byte, b'\n' | b'\r') {
                    *byte = b' ';
                }
            }
        }
        Self { live, cuts: &layout.cuts, at: 0, ticks: Ticks::default() }
    }

    fn code_span(&mut self, start: usize) -> usize {
        let len = ticks_at(&self.live, start);
        if !self.ticks.segment.contains(&start) {
            let cut = self.cuts.partition_point(|&cut| cut <= start);
            let leaf = self.cuts.get(cut).copied().unwrap_or(self.live.len());
            let end = start + paragraph_end(self.live.get(start..leaf).unwrap_or_default());
            self.ticks = Ticks::of(&self.live, start..end);
        }
        let Some(closers) = self.ticks.runs.get_mut(&len) else {
            return start + len;
        };
        while closers.pop_front_if(|close| *close <= start).is_some() {}
        closers.pop_front().map_or(start + len, |close| close + len)
    }
}

#[derive(Default)]
struct Ticks {
    segment: Range<usize>,
    runs: HashMap<usize, VecDeque<usize>>,
}

impl Ticks {
    fn of(live: &[u8], segment: Range<usize>) -> Self {
        let mut runs: HashMap<_, VecDeque<_>> = HashMap::new();
        let mut at = segment.start;
        while let Some(step) =
            live.get(at..segment.end).and_then(|rest| rest.iter().position(|&b| b == b'`'))
        {
            let start = at + step;
            let len = ticks_at(live, start);
            runs.entry(len).or_default().push_back(start);
            at = start + len;
        }
        Self { segment, runs }
    }
}

fn html_tag_len(after: &[u8]) -> usize {
    let mut at =
        word(after, 0, u8::is_ascii_alphabetic, |b| b.is_ascii_alphanumeric() || *b == b'-');
    if at == 0 {
        return 0;
    }
    loop {
        let spaced = space(after, &mut at);
        let name = word(
            after,
            at,
            |b| b.is_ascii_alphabetic() || matches!(b, b'_' | b':'),
            |b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'),
        );
        if !spaced || name == at {
            break;
        }
        at = name;
        let mut equals = at;
        space(after, &mut equals);
        if after.get(equals) == Some(&b'=') {
            equals += 1;
            space(after, &mut equals);
            let Some(end) = value(after, equals) else { return 0 };
            at = end;
        }
    }
    at += usize::from(after.get(at) == Some(&b'/'));
    if after.get(at) == Some(&b'>') { at + 1 } else { 0 }
}

fn word(bytes: &[u8], at: usize, first: impl Fn(&u8) -> bool, rest: impl Fn(&u8) -> bool) -> usize {
    if !bytes.get(at).is_some_and(first) {
        return at;
    }
    at + 1 + bytes.get(at + 1..).map_or(0, |tail| tail.iter().take_while(|b| rest(b)).count())
}

fn space(bytes: &[u8], at: &mut usize) -> bool {
    let blank = |b: &&u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    let run = bytes.get(*at..).map_or(0, |rest| rest.iter().take_while(blank).count());
    let len = paragraph_end(bytes.get(*at..*at + run).unwrap_or_default());
    *at += len;
    len > 0
}

fn value(bytes: &[u8], at: usize) -> Option<usize> {
    if let quote @ (b'"' | b'\'') = bytes.get(at)? {
        let body = bytes.get(at + 1..)?;
        let len = body.iter().position(|b| b == quote)?;
        return (paragraph_end(body.get(..len)?) == len).then_some(at + len + 2);
    }
    let unquoted = |b: &&u8| {
        !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'=' | b'<' | b'>' | b'`')
    };
    let len = bytes.get(at..)?.iter().take_while(unquoted).count();
    (len > 0).then_some(at + len)
}

fn autolink_len(after: &[u8]) -> usize {
    let scheme = word(after, 0, u8::is_ascii_alphabetic, |b| {
        b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-')
    });
    let body = |at: usize, allowed: fn(&u8) -> bool| {
        let len = after.get(at..).map_or(0, |rest| rest.iter().take_while(|b| allowed(b)).count());
        (after.get(at + len) == Some(&b'>')).then_some(at + len + 1)
    };
    if (MIN_SCHEME..=MAX_SCHEME).contains(&scheme) && after.get(scheme) == Some(&b':') {
        let uri = |b: &u8| !b.is_ascii_control() && !matches!(b, b' ' | b'<' | b'>');
        return body(scheme + 1, uri).unwrap_or(0);
    }
    let local = after.iter().take_while(|b| b.is_ascii_alphanumeric() || EMAIL_LOCAL.contains(b));
    let at = local.count();
    if after.get(at) != Some(&b'@') {
        return 0;
    }
    let domain = body(at + 1, |b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'));
    let labels = after.get(at + 1..domain.map_or(at + 1, |end| end - 1)).unwrap_or_default();
    let valid = |label: &[u8]| {
        (1..=MAX_LABEL).contains(&label.len()) && !label.starts_with(b"-") && !label.ends_with(b"-")
    };
    domain.filter(|_| labels.split(|&b| b == b'.').all(valid)).unwrap_or(0)
}

const MIN_SCHEME: usize = 2;
const MAX_SCHEME: usize = 32;
const MAX_LABEL: usize = 63;
const EMAIL_LOCAL: &[u8] = b".!#$%&'*+/=?^_`{|}~-";

fn ticks_at(live: &[u8], at: usize) -> usize {
    live.get(at..).map_or(0, |rest| rest.iter().take_while(|&&b| b == b'`').count())
}

struct Part {
    kind: Kind,
    text: Range<usize>,
}

pub(crate) struct Run {
    rows: Vec<Vec<Part>>,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

fn paragraph_end(bytes: &[u8]) -> usize {
    let mut at = 0;
    while let Some(step) =
        bytes.get(at..).and_then(|rest| rest.iter().position(|b| matches!(b, b'\n' | b'\r')))
    {
        let end = at + step;
        let crlf = bytes.get(end..).is_some_and(|rest| rest.starts_with(b"\r\n"));
        let next = end + if crlf { 2 } else { 1 };
        let after =
            bytes.get(next..).and_then(|rest| rest.iter().find(|b| !matches!(b, b' ' | b'\t')));
        if matches!(after, Some(b'\n' | b'\r')) {
            return end;
        }
        at = next;
    }
    bytes.len()
}

fn scanned<'a>(
    ctx: &ParseCtx<'_>,
    text: &'a str,
    from: usize,
    to: usize,
) -> Result<&'a str, ConvertError> {
    let gap = text.get(from..to).ok_or(ConvertError::Internal)?;
    ctx.charge(gap.len() as u64)?;
    Ok(gap)
}

fn cell_len(ends: &[usize], from: usize, text: &str) -> usize {
    let block =
        ends.get(ends.partition_point(|&end| end < from)).map_or(usize::MAX, |end| end - from);
    paragraph_end(text.as_bytes()).min(block)
}

pub(crate) fn run(
    ctx: &ParseCtx<'_>,
    markdown: &str,
    region: Range<usize>,
    ends: &[usize],
    tokens: &mut Peekable<impl Iterator<Item = Token>>,
) -> Result<Option<Run>, ConvertError> {
    let stray = |token: &Token| {
        token.start < region.start || (token.tag == Tag::Break && region.contains(&token.start))
    };
    while tokens.next_if(stray).is_some() {}
    let Some(start) = tokens.peek().map(|token| token.start).filter(|at| region.contains(at))
    else {
        return Ok(None);
    };
    let gap = |from, to| scanned(ctx, markdown, from, to);
    let (mut rows, mut row) = (Vec::new(), Vec::new());
    let (mut open, mut resume) = (None, start);
    while let Some(&token) = tokens.peek() {
        ctx.charge((token.end - token.start) as u64 + 1)?;
        if let Some((kind, from)) = open.take() {
            let between = gap(from, token.start)?;
            let cut = cell_len(ends, from, between);
            row.push(Part { kind, text: from..from + cut });
            if cut < between.len() {
                rows.push(row);
                return Ok(Some(Run { rows, start, end: from + cut }));
            }
        } else if token.tag == Tag::Break || !gap(resume, token.start)?.trim().is_empty() {
            return Ok(Some(Run { rows, start, end: resume }));
        }
        tokens.next();
        match token.tag {
            Tag::Cell(kind) => open = Some((kind, token.end)),
            Tag::Break => {
                rows.push(mem::take(&mut row));
                resume = token.end;
            }
        }
    }
    let end = match open {
        Some((kind, from)) => {
            let tail = gap(from, markdown.len())?;
            let cut = cell_len(ends, from, tail);
            row.push(Part { kind, text: from..from + cut });
            rows.push(row);
            from + cut
        }
        None => resume,
    };
    Ok(Some(Run { rows, start, end }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Origin { header: bool },
    Left,
    Up,
    Cross,
}

fn slot(markdown: &str, part: &Part) -> Slot {
    let blank = markdown.get(part.text.clone()).is_none_or(|text| text.trim().is_empty());
    match part.kind {
        Kind::Origin => Slot::Origin { header: false },
        Kind::Header => Slot::Origin { header: true },
        _ if !blank => Slot::Origin { header: false },
        Kind::Left => Slot::Left,
        Kind::Up => Slot::Up,
        Kind::Cross => Slot::Cross,
    }
}

fn extent(
    ctx: &ParseCtx<'_>,
    slots: &[Vec<Slot>],
    row: usize,
    col: usize,
) -> Result<(u32, u32), ConvertError> {
    let at = |row: usize, col: usize| slots.get(row).and_then(|cells| cells.get(col));
    let mut cols = 1;
    while at(row, col + cols) == Some(&Slot::Left) {
        ctx.charge(1)?;
        cols += 1;
    }
    let mut rows = 1;
    'below: loop {
        for offset in 0..cols {
            ctx.charge(1)?;
            let wanted = if offset == 0 { Slot::Up } else { Slot::Cross };
            if at(row + rows, col + offset) != Some(&wanted) {
                break 'below;
            }
        }
        rows += 1;
    }
    let span = |count| u32::try_from(count).unwrap_or(u32::MAX);
    Ok((span(cols), span(rows)))
}

pub(crate) fn table(
    ctx: &ParseCtx<'_>,
    markdown: &str,
    run: &Run,
    depth: u64,
    mut inlines: impl FnMut(&str) -> Result<Vec<Inline>, ConvertError>,
) -> Result<Table, ConvertError> {
    let max = ctx.limits().max_nesting_depth;
    if depth >= max {
        return Err(ConvertError::limit(Limit::NestingDepth, max));
    }
    let slots: Vec<Vec<Slot>> =
        run.rows.iter().map(|row| row.iter().map(|part| slot(markdown, part)).collect()).collect();
    let width = slots.iter().map(Vec::len).max().unwrap_or(0);
    let mut builder = GridBuilder::new(ctx);
    let (mut headers, mut leading) = (0, true);
    for (r, (cells, parts)) in slots.iter().zip(&run.rows).enumerate() {
        builder.next_row();
        let mut header = true;
        for (c, (cell, part)) in cells.iter().zip(parts).enumerate() {
            let Slot::Origin { header: head } = *cell else {
                header &= builder.covered()?;
                continue;
            };
            let (cols, rows) = extent(ctx, &slots, r, c)?;
            let text = markdown.get(part.text.clone()).ok_or(ConvertError::Internal)?;
            let blocks = if text.trim().is_empty() {
                Vec::new()
            } else {
                vec![Block::Paragraph(inlines(text)?)]
            };
            builder.place(Cell::spanning(blocks, cols, rows))?;
            header &= head;
        }
        for _ in cells.len()..width {
            builder.place(Cell::default())?;
        }
        leading &= header;
        headers += usize::from(leading);
    }
    let mut table = builder.finish_preserving_rows()?;
    table.header_rows = headers;
    Ok(table)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use crate::CancelToken;
    use crate::ctx::ParseCtx;
    use crate::error::ConvertError;
    use crate::limits::{Limit, Limits};
    use crate::model::{Block, CellSlot, Inline, Style, Table};
    use crate::shared::markdown;

    fn read(text: &str) -> Vec<Block> {
        markdown::blocks(&ParseCtx::default(), text, 0).unwrap()
    }

    fn table(text: &str) -> Table {
        match read(text).as_slice() {
            [Block::Table(table)] => table.clone(),
            blocks => panic!("{text:?} -> {blocks:?}"),
        }
    }

    fn rows(table: &Table) -> Vec<String> {
        let slot = |slot: &CellSlot| match slot {
            CellSlot::Origin(cell) => {
                let text: String = cell.blocks.iter().map(Block::plain_text).collect();
                let text = if text.is_empty() { "()".to_owned() } else { text };
                if cell.row_span > 1 || cell.col_span > 1 {
                    format!("{text}[{}x{}]", cell.row_span, cell.col_span)
                } else {
                    text
                }
            }
            CellSlot::Covered { .. } => "~".to_owned(),
        };
        let row = |row: &Vec<CellSlot>| row.iter().map(slot).collect::<Vec<_>>().join(" | ");
        table.grid.iter().map(row).collect()
    }

    fn outline(blocks: &[Block]) -> Vec<String> {
        let block = |block: &Block| match block {
            Block::Table(table) => format!("table {}", rows(table).join(" // ")),
            block => format!("text {}", block.plain_text()),
        };
        blocks.iter().map(block).collect()
    }

    fn cell(text: &str) -> Vec<Inline> {
        let table = table(&format!("<fcel>{text}<nl>"));
        match table.origins().next().map(|cell| cell.blocks.as_slice()) {
            Some([Block::Paragraph(inlines)]) => inlines.clone(),
            other => panic!("{text:?} -> {other:?}"),
        }
    }

    #[test]
    fn a_rectangle_of_continuations_spans_the_cell_it_hangs_from() {
        let cases = [
            ("<fcel>Sample<lcel><nl><fcel>A<fcel>12.4<nl>", vec!["Sample[1x2] | ~", "A | 12.4"]),
            (
                "<fcel>S<lcel><nl><ucel><xcel><nl><fcel>a<fcel>b<nl>",
                vec!["S[2x2] | ~", "~ | ~", "a | b"],
            ),
            ("<fcel>S<fcel>T<nl><ucel><fcel>U<nl>", vec!["S[2x1] | T", "~ | U"]),
            ("<fcel>S<lcel><nl><ucel><fcel>x<nl>", vec!["S[1x2] | ~", "() | x"]),
            ("<fcel>A<lcel><nl><ucel><nl>", vec!["A[1x2] | ~", "() | ()"]),
            ("<srow>Section<lcel><nl><rhed>Row<fcel>1<nl>", vec!["Section[1x2] | ~", "Row | 1"]),
            ("<ecel><lcel><nl><fcel>a<fcel>b<nl>", vec!["()[1x2] | ~", "a | b"]),
            (
                "<fcel>A<lcel><fcel>B<nl><ucel><xcel><ucel><nl>",
                vec!["A[2x2] | ~ | B[2x1]", "~ | ~ | ~"],
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(rows(&table(text)), expected, "{text}");
        }
    }

    #[test]
    fn a_covered_position_points_back_at_its_origin() {
        let table = table("<fcel>S<lcel><nl><ucel><xcel><nl>");
        let covered = [(0, 1), (1, 0), (1, 1)];
        for (row, col) in covered {
            assert_matches!(
                table.grid[row][col],
                CellSlot::Covered { origin_row: 0, origin_col: 0 },
                "{row},{col}"
            );
        }
    }

    #[test]
    fn a_repair_keeps_every_word_the_model_wrote() {
        let cases = [
            ("a continuation with text is a cell", "<fcel>a<lcel>x<nl>", vec!["a | x"]),
            ("a continuation first in a row", "<ucel><fcel>b<nl>", vec!["() | b"]),
            ("a continuation outside a rectangle", "<fcel>a<xcel><nl>", vec!["a | ()"]),
            ("an empty cell with text", "<ecel>note<fcel>b<nl>", vec!["note | b"]),
            (
                "a continuation with text inside a rectangle",
                "<fcel>S<lcel><nl><ucel>y<xcel><nl>",
                vec!["S[1x2] | ~", "y | ()"],
            ),
            (
                "short rows",
                "<fcel>a<fcel>b<fcel>c<nl><fcel>d<nl>",
                vec!["a | b | c", "d | () | ()"],
            ),
            ("a last row without its break", "<fcel>A<nl><fcel>B<fcel>C", vec!["A | ()", "B | C"]),
            (
                "an empty last row under a span",
                "<fcel>S<fcel>T<nl><ucel><ecel><nl>",
                vec!["S[2x1] | T", "~ | ()"],
            ),
        ];
        for (case, text, expected) in cases {
            assert_eq!(rows(&table(text)), expected, "{case}");
        }
    }

    #[test]
    fn leading_rows_of_column_headers_are_the_header() {
        let cases = [
            ("<ched>A<ched>B<nl><fcel>1<fcel>2<nl>", 1),
            ("<ched>N<ched>S<lcel><nl><ucel><ched>M<ched>D<nl><fcel>a<fcel>1<fcel>2<nl>", 2),
            ("<ched>A<ched>B<nl>", 1),
            ("<fcel>A<nl><ched>B<nl>", 0),
            ("<ched>A<fcel>B<nl>", 0),
            ("<rhed>A<fcel>B<nl>", 0),
            ("<srow>A<fcel>B<nl>", 0),
            ("<ched>A<ucel><nl><fcel>1<fcel>2<nl>", 0),
        ];
        for (text, header_rows) in cases {
            assert_eq!(table(text).header_rows, header_rows, "{text}");
        }
        assert_eq!(
            rows(&table(
                "<ched>N<ched>S<lcel><nl><ucel><ched>M<ched>D<nl><fcel>a<fcel>1<fcel>2<nl>"
            )),
            ["N[2x1] | S[1x2] | ~", "~ | M | D", "a | 1 | 2"]
        );
    }

    #[test]
    fn a_run_ends_where_the_model_stops_writing_cells() {
        let cases = [
            (
                "text above in the same paragraph",
                "Table 1. Results\n<fcel>A<fcel>B<nl><fcel>1<fcel>2<nl>",
                vec!["text Table 1. Results", "table A | B // 1 | 2"],
            ),
            (
                "text on the line of the first tag",
                "Intro <fcel>A<nl>",
                vec!["text Intro", "table A"],
            ),
            ("text below", "<fcel>A<nl>Trailing text", vec!["table A", "text Trailing text"]),
            ("a tag on a line of its own", "<fcel>\nA\n<fcel>\nB\n<nl>\n", vec!["table A | B"]),
            ("rows apart by a blank line", "<fcel>A<nl>\n\n<fcel>B<nl>", vec!["table A // B"]),
            ("a row with no break", "<fcel>A<fcel>B\n\nNext", vec!["table A | B", "text Next"]),
            (
                "a paragraph between two runs",
                "<fcel>A<nl>\n\ntext\n\n<fcel>B<nl>",
                vec!["table A", "text text", "table B"],
            ),
            (
                "a second break",
                "<fcel>a<nl><nl><fcel>b<nl>",
                vec!["table a", "text <nl>", "table b"],
            ),
            ("a blank line in a cell", "<fcel>A\n\nB<nl>", vec!["table A", "text B<nl>"]),
            ("a line of spaces and tabs", "<fcel>A\n \t\nB<nl>", vec!["table A", "text B<nl>"]),
            ("a cell over two lines", "<fcel>A\r\nB<nl>", vec!["table A B"]),
            ("a table below a quote", "> q\n\n<fcel>A<nl>", vec!["text q", "table A"]),
            ("a table below a list", "- a\n\n<fcel>A<nl>", vec!["text a", "table A"]),
            (
                "a comment holding a break",
                "<fcel>a <!-- <nl> --> b<nl>",
                vec!["table a <!-- <nl> --> b"],
            ),
            (
                "a different line ending",
                "<fcel>A<nl>\r\n\r\n<fcel>B\r\n\r\nC",
                vec!["table A // B", "text C"],
            ),
        ];
        for (case, text, expected) in cases {
            assert_eq!(outline(&read(text)), expected, "{case}");
        }
    }

    #[test]
    fn tags_in_code_quotes_lists_headings_comments_and_escapes_stay_text() {
        let literal = "text <fcel>A<nl>";
        let cases = [
            ("<!--\n<fcel>A<nl>\n-->", vec!["text <!--\n<fcel>A<nl>\n-->"]),
            ("x <!-- y\n\n<fcel>A<nl>\n\nz --> w", vec!["text x <!-- y", literal, "text z --> w"]),
            ("x <? y\n\n<fcel>A<nl>", vec!["text x <? y", literal]),
            ("`<fcel>A<nl>`", vec![literal]),
            ("```\n<fcel>A<nl>\n```", vec!["text <fcel>A<nl>\n"]),
            ("> <fcel>A<nl>", vec![literal]),
            ("- <fcel>A<nl>", vec![literal]),
            ("- a\n\n  <fcel>A<nl>", vec!["text a <fcel>A<nl>"]),
            ("# <fcel>A<nl>", vec![literal]),
            ("p\n\n> <fcel>A<nl>", vec!["text p", literal]),
            ("p\n\n- <fcel>A<nl>", vec!["text p", literal]),
            ("p\n\n# <fcel>A<nl>", vec!["text p", literal]),
            ("\\<fcel>A<nl>", vec![literal]),
            ("&lt;fcel&gt;A", vec!["text <fcel>A"]),
            ("<fcel A>", vec!["text <fcel A>"]),
            ("<!-- <fcel>A<nl> -->", vec!["text <!-- <fcel>A<nl> -->"]),
        ];
        for (text, expected) in cases {
            assert_eq!(outline(&read(text)), expected, "{text}");
        }
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of cell texts and the inlines read")]
    fn a_cell_is_one_line_of_inline_markdown_whatever_block_it_looks_like() {
        let literal =
            ["-", "+", "***", "> 0.05", "# of cases", "1. x", "# #", "A #", "A ##", "a|b"];
        for text in literal {
            assert_eq!(cell(text), [Inline::plain(text)], "{text}");
        }
        let bold = Style { bold: true, ..Style::PLAIN };
        let code = Style { code: true, ..Style::PLAIN };
        let cases = [
            (" a\n  b ", vec![Inline::plain("a b")]),
            ("e&#x301;", vec![Inline::plain("\u{e9}")]),
            ("a\u{a0}b", vec![Inline::plain("a\u{a0}b")]),
            ("\\|", vec![Inline::plain("|")]),
            ("\\#", vec![Inline::plain("#")]),
            ("<b>x</b>", vec![Inline::plain("<b>x</b>")]),
            (
                "**b** and `c`",
                vec![
                    Inline::Text { text: "b".into(), style: bold },
                    Inline::plain(" and "),
                    Inline::Text { text: "c".into(), style: code },
                ],
            ),
            (
                "$x^2$ and \\(y\\)",
                vec![Inline::Math("x^2".into()), Inline::plain(" and "), Inline::Math("y".into())],
            ),
            ("$$z$$", vec![Inline::Math("z".into())]),
            ("\\[z\\]", vec![Inline::Math("z".into())]),
            ("$\\|x\\|$", vec![Inline::Math("\\|x\\|".into())]),
            ("`a  b`", vec![Inline::Text { text: "a  b".into(), style: code }]),
            ("`a\t b`", vec![Inline::Text { text: "a\t b".into(), style: code }]),
            ("a \t b", vec![Inline::plain("a \t b")]),
            ("`a \n\t b`", vec![Inline::Text { text: "a  \t b".into(), style: code }]),
            ("a \n b", vec![Inline::plain("a b")]),
            ("a \t\n b", vec![Inline::plain("a"), Inline::LineBreak, Inline::plain("b")]),
            ("a \r b", vec![Inline::plain("a b")]),
            ("a  ", vec![Inline::plain("a")]),
            ("a  \n", vec![Inline::plain("a")]),
            ("a&ensp;b", vec![Inline::plain("a\u{2002}b")]),
            (
                "a&ensp;b  \nc",
                vec![Inline::plain("a\u{2002}b"), Inline::LineBreak, Inline::plain("c")],
            ),
            (
                "a\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{205f}\u{3000}b  \nc",
                vec![
                    Inline::plain(
                        "a\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{205f}\u{3000}b",
                    ),
                    Inline::LineBreak,
                    Inline::plain("c"),
                ],
            ),
            ("a <b  \nx> c", vec![Inline::plain("a <b   x> c")]),
            ("a  \nb", vec![Inline::plain("a"), Inline::LineBreak, Inline::plain("b")]),
            ("a \\\n b", vec![Inline::plain("a "), Inline::LineBreak, Inline::plain("b")]),
            ("a\\\\\nb", vec![Inline::plain("a\\ b")]),
            (
                "**a  \nb**",
                vec![
                    Inline::Text { text: "a".into(), style: bold },
                    Inline::LineBreak,
                    Inline::Text { text: "b".into(), style: bold },
                ],
            ),
            ("`a  \nb`", vec![Inline::Text { text: "a   b".into(), style: code }]),
            (
                "a\u{2002}b  \nc",
                vec![Inline::plain("a\u{2002}b"), Inline::LineBreak, Inline::plain("c")],
            ),
            ("$a<lcel>b$", vec![Inline::Math("a<lcel>b".into())]),
            ("\\(a<lcel>b\\)", vec![Inline::Math("a<lcel>b".into())]),
            ("$$a<lcel>b$$", vec![Inline::Math("a<lcel>b".into())]),
            ("$a  \nb$", vec![Inline::Math("a   b".into())]),
            ("\\(a  \nb\\)", vec![Inline::Math("a   b".into())]),
            (
                "x <!--> `a \n b`",
                vec![Inline::plain("x <!--> "), Inline::Text { text: "a   b".into(), style: code }],
            ),
            (
                "x <!-- `a \n b`",
                vec![
                    Inline::plain("x <!-- "),
                    Inline::Text { text: "a   b".into(), style: code },
                    Inline::plain("<nl>"),
                ],
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(cell(text), expected, "{text:?}");
        }
    }

    #[test]
    fn tags_in_a_cells_code_or_escaped_are_its_text() {
        let code = Style { code: true, ..Style::PLAIN };
        let cases = [
            (
                "a `<lcel>` b",
                vec![
                    Inline::plain("a "),
                    Inline::Text { text: "<lcel>".into(), style: code },
                    Inline::plain(" b"),
                ],
            ),
            ("``a`<fcel>b``", vec![Inline::Text { text: "a`<fcel>b".into(), style: code }]),
            ("a \\<fcel>b\\<nl>", vec![Inline::plain("a <fcel>b<nl>")]),
        ];
        for (text, expected) in cases {
            assert_eq!(cell(text), expected, "{text:?}");
        }
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of fills and the blocks read")]
    fn markup_in_code_or_escaped_opens_nothing_and_hides_no_tag() {
        let cases = [
            ("```\n<!--\n```\n\n<fcel>A<nl>", vec!["text <!--\n", "table A"]),
            ("    <?\n\n<fcel>A<nl>", vec!["text <?\n", "table A"]),
            ("`<![CDATA[`\n\n<fcel>A<nl>", vec!["text <![CDATA[", "table A"]),
            ("\\<!--\n\n<fcel>A<nl>", vec!["text <!--", "table A"]),
            ("`x\n\n<fcel>A`<nl>", vec!["text `x", "table A`"]),
            ("# a `b\n<fcel>c`<nl>", vec!["text a `b", "table c`"]),
            ("a `b\n***\n<fcel>c`<nl>", vec!["text a `b", "text ", "table c`"]),
            ("# a `b\n<div><fcel>c`<nl>", vec!["text a `b", "text <div>", "table c`"]),
            (
                "<div>\n`p\n`a<fcel>B`<nl>\n</div>",
                vec!["text <div>\n`p\n`a", "table B`", "text </div>"],
            ),
            ("<pre>`x\n\n<fcel>A`<nl></pre>", vec!["text <pre>`x", "table A`", "text </pre>"]),
            ("<fcel>a`b<fcel>c<nl>", vec!["table a`b | c"]),
            ("<fcel>`a``<fcel>b`<nl>", vec!["table a``<fcel>b"]),
            ("<fcel>\\`<lcel>`<nl>", vec!["table ` | `"]),
            ("<fcel>a\\\\<fcel>b<nl>", vec!["table a\\ | b"]),
            ("<fcel>\n`<lcel>`\n<nl>", vec!["table <lcel>"]),
            ("<fcel>a <!-- ` --> b<fcel>c`<nl>", vec!["table a <!-- ` --> b | c`"]),
            ("<fcel><a title=\"`\">b<fcel>c`<nl>", vec!["table <a title=\"`\">b | c`"]),
            ("<fcel>a <b `<lcel>`<nl>", vec!["table a <b <lcel>"]),
            ("<fcel>a <b <lcel>x<nl>", vec!["table a <b | x"]),
            ("<fcel>a <b `> <lcel> `<nl>", vec!["table a <b > <lcel> "]),
            ("<fcel>a <b x=`> <lcel> `<nl>", vec!["table a <b x=> <lcel> "]),
            ("<fcel>a <\"`\"> <lcel>`<nl>", vec!["table a <\"\"> <lcel>"]),
            ("<fcel>a <b \"`\"> <lcel> `<nl>", vec!["table a <b \"\"> <lcel> "]),
            ("<fcel>a <b x=y\"`\"> <lcel> `<nl>", vec!["table a <b x=y\"\"> <lcel> "]),
            ("<fcel>a <b=\"`\"> <lcel> `<nl>", vec!["table a <b=\"\"> <lcel> "]),
            ("<fcel>a <b x=\"1\"y=\"`\"> <lcel> `<nl>", vec!["table a <b x=\"1\"y=\"\"> <lcel> "]),
            ("<fcel><b x = '`' y\r\n/>b<fcel>c`<nl>", vec!["table <b x = '`' y />b | c`"]),
            ("<fcel>a <1 x=\"`\"> <lcel> `<nl>", vec!["table a <1 x=\"\"> <lcel> "]),
            ("<fcel><my-b x=\"`\">b<fcel>c`<nl>", vec!["table <my-b x=\"`\">b | c`"]),
            (
                "<fcel><b :a _b.c-d:e_f=\"`\">b<fcel>c`<nl>",
                vec!["table <b :a _b.c-d:e_f=\"`\">b | c`"],
            ),
            ("<fcel>a <b =\"`\"> <lcel> `<nl>", vec!["table a <b =\"\"> <lcel> "]),
            ("<fcel>a <b x=y<lcel>z<nl>", vec!["table a <b x=y | z"]),
            ("<fcel><b x=\"<lcel>\">z<nl>", vec!["table <b x=\"<lcel>\">z"]),
            ("<fcel>a <b x=\"<i>`\"> <lcel> `<nl>", vec!["table a <b x=\"<i>`\"> | `"]),
            (
                "<fcel>a <b x=\"<lcel>\n\n\">b<nl>",
                vec!["table a <b x=\"[1x2] | ~", "text \">b<nl>"],
            ),
            ("<fcel>a <b\n\nx=\"<lcel>\">", vec!["table a <b", "text x=\"", "table \">"]),
            ("<fcel>a <http://x`y> <lcel> `<nl>", vec!["table a http://x`y | `"]),
            ("<fcel>a <x`y@z.test> <lcel> `<nl>", vec!["table a x`y@z.test | `"]),
            ("<fcel>a <h:x`y> <lcel> `<nl>", vec!["table a <h:xy> <lcel> "]),
            ("<fcel>a <x`y@-z.test> <lcel> `<nl>", vec!["table a <xy@-z.test> <lcel> "]),
            ("<fcel>a <x`y@z-.test> <lcel> `<nl>", vec!["table a <xy@z-.test> <lcel> "]),
            ("<fcel>a <a+b.c-d:x`y> <lcel> `<nl>", vec!["table a a+b.c-d:x`y | `"]),
            (
                "<fcel>a <aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:x`y> <lcel> `<nl>",
                vec!["table a <aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:xy> <lcel> "],
            ),
            (
                "<fcel>a <x`y@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.test> <lcel> `<nl>",
                vec![
                    "table a <xy@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.test> <lcel> ",
                ],
            ),
            ("<fcel>a <http://x `y> <lcel> `<nl>", vec!["table a <http://x y> <lcel> "]),
            ("<pre><fcel>A\n</pre>\n```\nx\n```", vec!["text <pre>", "table A </pre>", "text x\n"]),
            ("x <!--> y\n\n<fcel>A<nl>", vec!["text x <!--> y", "table A"]),
            ("x <!---> y\n\n<fcel>A<nl>", vec!["text x <!---> y", "table A"]),
            ("Intro <fcel>$a<lcel>b$<nl>", vec!["text Intro", "table a<lcel>b"]),
            (
                "<fcel>Price<fcel>Cost<nl><fcel>$5<fcel>$10<nl><fcel>$7 <fcel>$2 <nl>",
                vec!["table Price | Cost // $5 | $10 // $7 | $2"],
            ),
            (
                "<fcel>Revenue<fcel>$<fcel>100<nl><fcel>Cost<fcel>$<fcel>50<nl>",
                vec!["table Revenue | $ | 100 // Cost | $ | 50"],
            ),
            (
                "<fcel>Revenue<fcel>$<fcel>100<nl>\n<fcel>Cost<fcel>$<fcel>50<nl>",
                vec!["table Revenue | $ | 100 // Cost | $ | 50"],
            ),
            ("<fcel>5$<fcel>6$<nl>", vec!["table 5$ | 6$"]),
            ("<fcel>$5<fcel>$(10)<nl>", vec!["table $5 | $(10)"]),
            ("<fcel>$5<fcel>$ 10<nl>", vec!["table $5 | $ 10"]),
            ("<fcel>$5<fcel>US$10<nl>", vec!["table $5 | US$10"]),
            ("<fcel>x\n\\[\na<lcel>b\n\\]<nl>", vec!["table x a<lcel>b"]),
            ("<fcel>\n$a<lcel>b$\n<nl>", vec!["table a<lcel>b"]),
            ("<fcel>\n\\(a<lcel>b\\)\n<nl>", vec!["table a<lcel>b"]),
            ("<fcel>\n$$a<lcel>b$$\n<nl>", vec!["table a<lcel>b"]),
            ("<fcel>\n$5<fcel>$10<nl>", vec!["table $5 | $10"]),
            ("<fcel>\nx<fcel>$a<lcel>b$<fcel>c<nl>", vec!["table x | a<lcel>b | c"]),
            ("<fcel>\r```\r$a<lcel>b$<nl>", vec!["table ``` a<lcel>b"]),
            (
                "<div><fcel>A<nl>text<fcel>B<nl></div>",
                vec!["text <div>", "table A", "text text", "table B", "text </div>"],
            ),
            ("<fcel>a\\\n\nb", vec!["table a\\", "text b"]),
            ("Intro <fcel>\\(a<lcel>b\\)<nl>", vec!["text Intro", "table a<lcel>b"]),
            ("intro <fcel>A\n```\nx\ny\n```", vec!["text intro", "table A", "text x\ny\n"]),
            ("<fcel>A\n- b", vec!["table A", "text b"]),
            ("<fcel>A\n# B", vec!["table A", "text B"]),
            ("<fcel>A<nl>\n```\nx\n```", vec!["table A", "text x\n"]),
            ("<fcel>a < `b> <lcel>`<nl>", vec!["table a < b> <lcel>"]),
        ];
        for (text, expected) in cases {
            assert_eq!(outline(&read(text)), expected, "{text:?}");
        }
    }

    #[test]
    fn a_paragraphs_backtick_runs_are_indexed_once() {
        let text = "`a` ``b`` `c`<nl>\n\n`d`";
        let layout = super::Layout::default();
        let mut scan = super::Scan::new(text, &layout);
        assert!(scan.next().is_some());
        assert_eq!(scan.ticks.segment, 0..17);
    }

    #[test]
    fn a_table_nests_one_level_within_the_depth_left() {
        let limits = Limits { max_nesting_depth: 3, ..Limits::default() };
        let ctx = ParseCtx::new(&limits);
        assert_eq!(markdown::blocks(&ctx, "<fcel>a<nl>", 2).map(|blocks| blocks.len()), Ok(1));
        assert_matches!(
            markdown::blocks(&ctx, "<fcel>a<nl>", 3),
            Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 3 })
        );
    }

    #[test]
    fn each_limit_refuses_with_its_own_error() {
        let ragged = format!("<fcel>a<fcel>b<fcel>c<fcel>d<nl>{}", "<fcel>x<nl>".repeat(3));
        let area = "<fcel><lcel><lcel><nl><ucel><xcel><xcel><nl>";
        let cases = [
            (ragged.as_str(), Limits { max_grid_slots: 16, ..Limits::default() }, None),
            (
                ragged.as_str(),
                Limits { max_grid_slots: 15, ..Limits::default() },
                Some(Limit::GridSlots),
            ),
            (area, Limits { max_expansion: 5, ..Limits::default() }, None),
            (area, Limits { max_expansion: 4, ..Limits::default() }, Some(Limit::Expansion)),
            (area, Limits { max_ocr_text_bytes: 44, ..Limits::default() }, None),
            (
                area,
                Limits { max_ocr_text_bytes: 43, ..Limits::default() },
                Some(Limit::OcrTextBytes),
            ),
        ];
        for (text, limits, refused) in cases {
            let read = markdown::blocks(&ParseCtx::new(&limits), text, 0);
            let Some(limit) = refused else {
                assert_eq!(read.map(|blocks| blocks.len()), Ok(1), "{limits:?}");
                continue;
            };
            let error = read.unwrap_err();
            assert_matches!(error, ConvertError::ResourceLimit { limit: got, .. } if got == limit);
        }
    }

    #[test]
    fn work_and_cancellation_stop_a_run_before_any_grid_is_built() {
        let bomb = format!("<fcel>x{}", "<lcel>".repeat(100_000));
        let max = bomb.len() as u64 + 1000;
        let limits = Limits { max_work: max, ..Limits::default() };
        let ctx = ParseCtx::new(&limits);
        assert_eq!(markdown::blocks(&ctx, &bomb, 0), Err(ConvertError::limit(Limit::Work, max)));
        assert!(ctx.ledger().spent() < max + 8, "{}", ctx.ledger().spent());
        assert_eq!(ctx.grid_slots_left(), limits.max_grid_slots);
        let cancel = CancelToken::new();
        cancel.cancel();
        let ctx = ParseCtx::default().with_cancel(Some(&cancel));
        assert_eq!(markdown::blocks(&ctx, "<fcel>a<nl>", 0), Err(ConvertError::Cancelled));
    }

    #[test]
    fn a_scan_left_open_is_read_once_however_many_runs_follow_it() {
        for opener in ["<?", "<!--", "<![CDATA[", "<!DOCTYPE ["] {
            let unit = format!("<fcel>b<nl>y {opener}\n\n");
            let text = unit.repeat((256 << 10) / unit.len());
            let limits = Limits { max_work: 2 * text.len() as u64, ..Limits::default() };
            let blocks = markdown::blocks(&ParseCtx::new(&limits), &text, 0).unwrap();
            let tables = blocks.iter().filter(|block| matches!(block, Block::Table(_))).count();
            assert_eq!(tables, 1, "{opener}");
        }
    }

    #[test]
    fn the_bytes_the_tags_the_spans_and_the_positions_are_each_charged() {
        let cases = [
            ("<fcel><lcel><nl><ucel><xcel><nl>", 32 + (7 + 7 + 5 + 7 + 7 + 5) + 4 + 4),
            ("<fcel>  <ecel>\n<nl>\n\n<ecel><nl>", 31 + (7 + 7 + 5 + 7 + 5) + (2 + 1 + 2) + 3 + 4),
            ("<fcel>a<nl>", 11 + (7 + 5) + 1 + 1 + 1 + 5),
            ("x <nl>", 4),
        ];
        for (text, spent) in cases {
            let ctx = ParseCtx::default();
            markdown::blocks(&ctx, text, 0).unwrap();
            assert_eq!(ctx.ledger().spent(), spent, "{text:?}");
        }
    }

    #[test]
    fn tables_beside_opted_in_html_tables_and_literal_cell_text() {
        let html = "<table><tr><td>h</td></tr></table>";
        let text = format!("{html}\n\n<fcel>A<lcel><nl><fcel><table><nl>");
        let blocks = markdown::blocks_with_tables(&ParseCtx::default(), &text, 0).unwrap();
        assert_eq!(outline(&blocks), ["table h", "table A[1x2] | ~ // <table> | ()"]);
    }

    const PIECES: &[&str] = &[
        "<fcel>", "<ecel>", "<ched>", "<rhed>", "<srow>", "<lcel>", "<ucel>", "<xcel>", "<nl>",
        "1", "2", "3", " ", "\n", "\n\n", "\r\n",
    ];

    fn digits(text: &str) -> String {
        text.chars().filter(char::is_ascii_digit).collect()
    }

    fn well_formed(table: &Table) -> bool {
        let width = table.grid.first().map_or(0, Vec::len);
        let origin = |row: usize, col: usize| table.grid.get(row)?.get(col)?.origin();
        table.grid.iter().all(|row| row.len() == width)
            && table.grid.iter().enumerate().all(|(r, row)| {
                row.iter().enumerate().all(|(c, slot)| match slot {
                    CellSlot::Origin(_) => true,
                    &CellSlot::Covered { origin_row, origin_col } => origin(origin_row, origin_col)
                        .is_some_and(|cell| {
                            r < origin_row + cell.row_span as usize
                                && c < origin_col + cell.col_span as usize
                        }),
                })
            })
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(2048))]

        #[test]
        fn every_digit_the_model_wrote_survives_in_order_in_rectangular_tables(
            pieces in proptest::collection::vec(proptest::sample::select(PIECES), 0..48)
        ) {
            let text = pieces.concat();
            let blocks = markdown::blocks(&ParseCtx::default(), &text, 0).unwrap();
            let shown: Vec<_> = blocks.iter().map(Block::plain_text).collect();
            proptest::prop_assert_eq!(digits(&shown.join(" ")), digits(&text), "{:?}", text);
            for block in &blocks {
                if let Block::Table(table) = block {
                    proptest::prop_assert!(well_formed(table), "{:?}", text);
                }
            }
        }
    }
}
