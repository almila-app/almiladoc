//! What a block's rendering holds, read from the model before anything is
//! written: whether it shows anything, and whether it holds an empty line,
//! which makes the list item it sits alone in loose.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::error::ConvertError;
use crate::model::{Block, CellSlot, ImageSource, Inline, LinkTarget, List, Style};
use crate::render::markdown::escape::InlineContext;
use crate::render::markdown::inline::{Norm, normalize, render_inlines};
use crate::render::markdown::sink::{Meter, Text};
use crate::render::markdown::{Ctx, layout_cell};

/// Where a paragraph's rendered lines stand, read character by character
/// in render order. A trimmed line shows nothing when all it holds is one
/// run of backslashes (a hard break's, an escaped backslash's), so a line
/// shows once it holds any other character, or a second backslash run after
/// whitespace.
#[derive(Default)]
struct Lines {
    line: Line,
    run: Run,
    shown: bool,
    gap: bool,
    blank: bool,
}

#[derive(Default, PartialEq)]
enum Line {
    #[default]
    Empty,
    Shown,
}

#[derive(Default, PartialEq)]
enum Run {
    #[default]
    None,
    In,
    After,
}

impl Lines {
    fn text(&mut self, text: &str) {
        for c in text.chars() {
            match c {
                '\n' => self.line_break(),
                '\\' if self.run == Run::After => self.line = Line::Shown,
                '\\' => self.run = Run::In,
                c if c.is_whitespace() => {
                    if self.run == Run::In {
                        self.run = Run::After;
                    }
                }
                _ => self.line = Line::Shown,
            }
        }
    }

    fn mark(&mut self) {
        self.line = Line::Shown;
    }

    fn line_break(&mut self) {
        if self.line == Line::Shown {
            self.blank |= self.gap;
            self.shown = true;
            self.gap = false;
        } else {
            self.gap |= self.shown;
        }
        self.line = Line::Empty;
        self.run = Run::None;
    }

    /// Whether any line shows, and whether an empty line sits between two
    /// that do.
    fn finish(mut self) -> (bool, bool) {
        self.line_break();
        (self.shown, self.blank)
    }
}

/// Read the lines rendering `inlines` in a paragraph writes into `lines`.
fn trace(inlines: &[Inline], rc: &Ctx, lines: &mut Lines) {
    for run in &normalize(inlines, rc) {
        match run {
            Norm::Text { text, style } => {
                let core = text.trim();
                if *style == Style::PLAIN || core.is_empty() {
                    lines.text(text);
                    continue;
                }
                lines.text(text.strip_suffix(text.trim_start()).unwrap_or_default());
                lines.mark();
                if !style.code {
                    lines.text(core);
                    lines.mark();
                }
                lines.text(text.strip_prefix(text.trim_end()).unwrap_or_default());
            }
            Norm::Link { content, target } => match target {
                LinkTarget::Anchor(id) if rc.anchors.fragment(id).is_none() => {
                    trace(content, rc, lines);
                }
                _ if nonspace(content, InlineContext::Block, rc) => {
                    lines.mark();
                    trace(content, rc, lines);
                    lines.mark();
                }
                LinkTarget::Anchor(_) => {}
                LinkTarget::External(_) | LinkTarget::Relative(_) => lines.mark(),
            },
            Norm::Image { alt, source } => {
                let external = matches!(source, ImageSource::External(_));
                if external {
                    lines.mark();
                }
                lines.text(alt.trim());
                if external {
                    lines.mark();
                }
            }
            Norm::NoteRef(id) if !rc.nums.contains_key(*id) => {}
            Norm::NoteRef(_) | Norm::Anchor(_) | Norm::Math(_) | Norm::Checkbox(_) => lines.mark(),
            Norm::LineBreak => lines.text("\\\n"),
        }
    }
}

/// Whether rendering `inlines` in `ctx` writes anything but whitespace;
/// remembered for each link's content, which every enclosing link asks
/// about again.
fn nonspace(inlines: &[Inline], ctx: InlineContext, rc: &Ctx) -> bool {
    let key = (inlines.as_ptr(), ctx);
    if let Some(&known) = rc.shapes.nonspace.borrow().get(&key) {
        return known;
    }
    let value = normalize(inlines, rc).iter().any(|run| match run {
        Norm::Text { text, .. } => !text.trim().is_empty(),
        Norm::Link { target: LinkTarget::Anchor(_), content } => nonspace(content, ctx, rc),
        Norm::Link { .. } | Norm::Anchor(_) | Norm::Math(_) | Norm::Checkbox(_) => true,
        Norm::Image { alt, source } => {
            matches!(source, ImageSource::External(_)) || !alt.trim().is_empty()
        }
        Norm::NoteRef(id) => rc.nums.contains_key(*id),
        Norm::LineBreak => ctx == InlineContext::Block,
    });
    rc.shapes.nonspace.borrow_mut().insert(key, value);
    value
}

/// Whether a paragraph of `inlines` shows anything, and whether it holds an
/// empty line.
fn paragraph(inlines: &[Inline], rc: &Ctx) -> (bool, bool) {
    let mut lines = Lines::default();
    trace(inlines, rc, &mut lines);
    lines.finish()
}

/// Whether `block` renders anything.
pub(super) fn shows(block: &Block, rc: &Ctx) -> bool {
    match block {
        Block::Paragraph(inlines) => paragraph(inlines, rc).0,
        Block::Heading { content, .. } => nonspace(content, InlineContext::Heading, rc),
        Block::List(list) => !list.items.is_empty(),
        Block::Table(table) => match layout_cell(table) {
            Some(cell) => cell.blocks.iter().any(|b| shows(b, rc)),
            None => table.grid.iter().flatten().any(|slot| match slot {
                CellSlot::Origin(cell) => cell.blocks.iter().any(|b| shows_in_cell(b, rc)),
                CellSlot::Covered { .. } => true,
            }),
        },
        Block::BlockQuote(blocks) => blocks.iter().any(|b| shows(b, rc)),
        Block::CodeBlock { .. } | Block::Rule => true,
        Block::Math(tex) => !tex.trim().is_empty(),
    }
}

/// Whether `block` adds anything to the table cell it is flattened into.
fn shows_in_cell(block: &Block, rc: &Ctx) -> bool {
    match block {
        Block::Heading { content: inlines, .. } | Block::Paragraph(inlines) => {
            nonspace(inlines, InlineContext::TableCell, rc)
        }
        Block::List(list) => {
            list.items.iter().any(|item| item.blocks.iter().any(|b| shows_in_cell(b, rc)))
        }
        Block::Table(table) => table.grid.iter().flatten().any(|slot| {
            matches!(slot, CellSlot::Origin(cell) if cell.blocks.iter().any(|b| shows_in_cell(b, rc)))
        }),
        Block::BlockQuote(blocks) => blocks.iter().any(|b| shows_in_cell(b, rc)),
        Block::CodeBlock { text: source, .. } | Block::Math(source) => !source.trim().is_empty(),
        Block::Rule => false,
    }
}

/// What was read so far, by address: whether each list's items are
/// separated by an empty line, whether each list renders one anywhere, and
/// whether each run of link content writes anything but whitespace where it
/// is rendered.
#[derive(Default)]
pub(super) struct Shapes {
    loose: RefCell<HashMap<*const List, bool>>,
    blank: RefCell<HashMap<*const List, bool>>,
    nonspace: RefCell<HashMap<(*const Inline, InlineContext), bool>>,
}

/// Whether an empty line separates the items of `list`: an item holds more
/// than one block, or its one block renders an empty line.
pub(super) fn loose(list: &List, rc: &Ctx, meter: &Meter<'_>) -> Result<bool, ConvertError> {
    remembered(&rc.shapes.loose, list, || {
        for item in &list.items {
            match item.blocks.as_slice() {
                [] => {}
                [only] if !blank(only, rc, meter)? => {}
                _ => return Ok(true),
            }
        }
        Ok(false)
    })
}

fn blank_list(list: &List, rc: &Ctx, meter: &Meter<'_>) -> Result<bool, ConvertError> {
    remembered(&rc.shapes.blank, list, || {
        if list.items.len() > 1 && loose(list, rc, meter)? {
            return Ok(true);
        }
        for item in &list.items {
            if blank_body(&item.blocks, rc, meter)? {
                return Ok(true);
            }
        }
        Ok(false)
    })
}

fn remembered(
    memo: &RefCell<HashMap<*const List, bool>>,
    list: &List,
    compute: impl FnOnce() -> Result<bool, ConvertError>,
) -> Result<bool, ConvertError> {
    let key = std::ptr::from_ref(list);
    if let Some(&known) = memo.borrow().get(&key) {
        return Ok(known);
    }
    let value = compute()?;
    memo.borrow_mut().insert(key, value);
    Ok(value)
}

/// Whether `blocks`, joined by empty lines, render an empty line.
fn blank_body(blocks: &[Block], rc: &Ctx, meter: &Meter<'_>) -> Result<bool, ConvertError> {
    let mut showing = 0;
    for block in blocks {
        if blank(block, rc, meter)? {
            return Ok(true);
        }
        if shows(block, rc) {
            showing += 1;
            if showing > 1 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Whether `block` renders an empty line. A heading's is read from its
/// rendering, held and dropped.
fn blank(block: &Block, rc: &Ctx, meter: &Meter<'_>) -> Result<bool, ConvertError> {
    Ok(match block {
        Block::Paragraph(inlines) => paragraph(inlines, rc).1,
        Block::Heading { content, .. } => {
            let mut text = Text::new(meter);
            render_inlines(content, InlineContext::Heading, rc, &mut text)?;
            has_empty_line(["# ", text.as_str().trim()])
        }
        Block::List(list) => blank_list(list, rc, meter)?,
        Block::Table(table) => match layout_cell(table) {
            Some(cell) => blank_body(&cell.blocks, rc, meter)?,
            None => false,
        },
        Block::CodeBlock { lang, text } => has_empty_line([
            "```",
            lang.as_deref().unwrap_or(""),
            "\n",
            text.trim_end_matches('\n'),
            "\n```",
        ]),
        Block::Math(tex) => !tex.trim().is_empty() && has_empty_line(["$$\n", tex.trim(), "\n$$"]),
        Block::BlockQuote(_) | Block::Rule => false,
    })
}

/// Whether `pieces`, joined, hold a line break after an empty line or after
/// a line of one carriage return, which a line split reads as empty.
fn has_empty_line<'a>(pieces: impl IntoIterator<Item = &'a str>) -> bool {
    let (mut len, mut cr) = (0usize, false);
    for c in pieces.into_iter().flat_map(str::chars) {
        if c == '\n' {
            if len == 0 || (len == 1 && cr) {
                return true;
            }
            len = 0;
        } else {
            len += 1;
            cr = c == '\r';
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::ParseCtx;
    use crate::ledger::Ledger;
    use crate::model::{Cell, Document, ListItem, MarkerKind, Note, NoteKind, Table, TableKind};
    use crate::render::markdown::render_block;
    use crate::render::markdown::table::render_cell;
    use std::assert_matches;

    /// Hold every fact read from the model against the rendering of each
    /// block and table cell of `doc`, nested ones included.
    fn agrees_with_rendering(doc: &Document, name: &str) {
        let rc = Ctx::of(doc);
        let ledger = Ledger::new(u64::MAX, None);
        let meter = Meter::new(&ledger, u64::MAX);
        let mut blocks: Vec<&Block> = doc.blocks.iter().collect();
        blocks.extend(doc.notes.iter().flat_map(|note| &note.blocks));
        while let Some(block) = blocks.pop() {
            let markdown = render_block(block, &rc);
            assert_eq!(shows(block, &rc), markdown.is_some(), "{name}: {block:?}");
            let markdown = markdown.unwrap_or_default();
            let empty_line = |md: &str| md.lines().skip(1).any(str::is_empty);
            assert_eq!(blank(block, &rc, &meter).unwrap(), empty_line(&markdown), "{name}");
            match block {
                Block::List(list) => {
                    let spaced = list.items.iter().any(|item| match item.blocks.as_slice() {
                        [] => false,
                        [only] => render_block(only, &rc).is_some_and(|md| empty_line(&md)),
                        _ => true,
                    });
                    assert_eq!(loose(list, &rc, &meter).unwrap(), spaced, "{name}: {markdown:?}");
                    blocks.extend(list.items.iter().flat_map(|item| &item.blocks));
                }
                Block::BlockQuote(inner) => blocks.extend(inner),
                Block::Table(table) => {
                    for cell in table.grid.iter().flatten().filter_map(|slot| match slot {
                        CellSlot::Origin(cell) => Some(cell),
                        CellSlot::Covered { .. } => None,
                    }) {
                        let mut text = Text::new(&meter);
                        render_cell(cell, &rc, &mut text).unwrap();
                        let flattened = cell.blocks.iter().any(|b| shows_in_cell(b, &rc));
                        assert_eq!(flattened, !text.as_str().is_empty(), "{name}: {cell:?}");
                        blocks.extend(&cell.blocks);
                    }
                }
                _ => {}
            }
        }
    }

    fn plain(text: &str) -> Inline {
        Inline::plain(text)
    }

    fn styled(text: &str, style: Style) -> Inline {
        Inline::Text { text: text.into(), style }
    }

    fn link(content: Vec<Inline>, target: LinkTarget) -> Inline {
        Inline::Link { content, target }
    }

    fn item(blocks: Vec<Block>) -> ListItem {
        ListItem { blocks, marker_label: None }
    }

    fn list(items: Vec<ListItem>) -> Block {
        Block::List(List { marker: MarkerKind::Bullet, start: 1, items })
    }

    fn table(cells: Vec<Vec<Block>>, kind: TableKind) -> Block {
        let row = cells.into_iter().map(Cell::new).collect();
        Block::Table(Table::from_rows(vec![row], 0, kind))
    }

    #[test]
    fn what_the_model_says_a_block_renders_is_what_it_renders() {
        let bold = Style { bold: true, ..Style::PLAIN };
        let code = Style { code: true, ..Style::PLAIN };
        let external = || LinkTarget::External("https://example.com".into());
        let image = |alt: &str, source| Inline::Image { alt: alt.into(), source };
        let paragraphs: Vec<Vec<Inline>> = vec![
            vec![plain("a"), Inline::LineBreak, Inline::LineBreak, plain("b")],
            vec![plain("a"), Inline::LineBreak, plain("b")],
            vec![plain("a\n\nb")],
            vec![plain("\n \n\\\na\n\n")],
            vec![plain("a\n \\ \nb")],
            vec![plain("a\n\\\\\n\nb")],
            vec![plain("\\")],
            vec![plain(" \\ \\ ")],
            vec![plain("a\n\\\\ \\\\\nb")],
            vec![plain("\\ "), Inline::LineBreak, plain("b")],
            vec![plain("\\"), Inline::LineBreak, Inline::LineBreak, plain("b")],
            vec![plain("  ")],
            vec![Inline::LineBreak, Inline::LineBreak],
            vec![link(vec![plain("\n\n")], external())],
            vec![
                plain("a"),
                link(vec![Inline::LineBreak, Inline::LineBreak], external()),
                plain("b"),
            ],
            vec![
                plain("a\n"),
                link(vec![plain("\n")], LinkTarget::Anchor("nowhere".into())),
                plain("b"),
            ],
            vec![
                plain("a\n"),
                link(vec![plain("\n")], LinkTarget::Anchor("t".into())),
                plain("\nb"),
            ],
            vec![Inline::Anchor("t".into()), plain("\n\n")],
            vec![
                plain("a\n"),
                link(vec![plain(" ")], LinkTarget::Anchor(String::new())),
                plain("\nb"),
            ],
            vec![Inline::NoteRef("missing".into()), plain("\n")],
            vec![plain("a\n"), Inline::NoteRef("missing".into()), plain("\nb")],
            vec![Inline::NoteRef("n".into())],
            vec![image("  ", ImageSource::Unavailable)],
            vec![image("\\", ImageSource::Unavailable)],
            vec![image("a\n\nb", ImageSource::Unavailable)],
            vec![image("a\n\nb", ImageSource::External("https://example.com/i.png".into()))],
            vec![styled("a\n\nb", bold)],
            vec![styled("a\n\nb", code)],
            vec![plain("x"), styled("\n\n y", bold)],
            vec![styled(" \n", bold), plain("x")],
            vec![Inline::Math("a\n\nb".into())],
            vec![Inline::Checkbox(true)],
            vec![Inline::Anchor("unlinked".into())],
            vec![styled("a\n\\", bold), plain("\nb")],
            vec![plain("a\n"), styled("\\\nb", bold)],
            vec![plain("a\n"), link(vec![plain("\\\nb")], external())],
            vec![link(vec![plain("a\n\\")], external()), plain("\nb")],
            vec![
                plain("a\n"),
                image("\\\nb", ImageSource::External("https://example.com/i.png".into())),
            ],
            vec![
                image("a\n\\", ImageSource::External("https://example.com/i.png".into())),
                plain("\nb"),
            ],
        ];
        let headings: Vec<Vec<Inline>> = vec![
            vec![plain("\\")],
            vec![Inline::LineBreak],
            vec![plain("a\n\nb")],
            vec![plain("a\n\r\nb")],
            vec![plain("a\n \nb")],
        ];
        let mut leaves: Vec<Block> = paragraphs.into_iter().map(Block::Paragraph).collect();
        leaves.extend(headings.into_iter().map(|content| Block::heading(1, content)));
        for text in ["", "a\n\nb", "a\r\n\r\nb", "a\nb", "\n\na"] {
            leaves.push(Block::CodeBlock { lang: None, text: text.into() });
        }
        leaves.push(Block::CodeBlock { lang: Some("x\n\ny".into()), text: "a".into() });
        for tex in ["a\n\nb", "  ", "a\nb"] {
            leaves.push(Block::Math(tex.into()));
        }
        leaves.push(Block::Rule);
        let empty = || Block::Paragraph(vec![plain(" ")]);
        let text = |t: &str| Block::Paragraph(vec![plain(t)]);
        let mut containers = vec![
            list(vec![item(vec![list(vec![
                item(vec![text("a")]),
                item(vec![text("b"), text("c")]),
            ])])]),
            list(vec![item(vec![empty(), empty()])]),
            list(vec![item(vec![empty(), text("a")]), item(vec![text("b")])]),
            list(vec![item(vec![]), item(vec![text("b")])]),
            table(vec![vec![text("a"), text("b")]], TableKind::Layout),
            table(vec![vec![text("a")]], TableKind::Layout),
            table(vec![vec![empty()]], TableKind::Layout),
            Block::BlockQuote(vec![text("a"), text("b")]),
            Block::BlockQuote(vec![empty()]),
            table(vec![vec![empty()], vec![]], TableKind::Data),
            table(
                vec![
                    vec![Block::Paragraph(vec![Inline::LineBreak])],
                    vec![Block::Paragraph(vec![plain("\\")])],
                    vec![Block::CodeBlock { lang: None, text: " ".into() }, Block::Rule],
                    vec![table(vec![vec![empty()], vec![empty()]], TableKind::Data)],
                    vec![list(vec![item(vec![empty()]), item(vec![])])],
                    vec![Block::BlockQuote(vec![Block::heading(1, vec![Inline::LineBreak])])],
                    vec![Block::Math(" ".into())],
                ],
                TableKind::Data,
            ),
        ];
        let parse_ctx = crate::ctx::ParseCtx::default();
        let mut covered = crate::model::GridBuilder::new(&parse_ctx);
        covered.next_row();
        let unshown = Block::Paragraph(vec![Inline::NoteRef("missing".into())]);
        covered.place(Cell { col_span: 2, ..Cell::new(vec![unshown]) }).unwrap();
        let covered = Block::Table(covered.finish(TableKind::Data).unwrap());
        assert_matches!(&covered, Block::Table(table) if !table.grid.is_empty());
        containers.push(list(vec![item(vec![list(vec![item(vec![covered.clone(), text("x")])])])]));
        containers.push(covered);
        for leaf in &leaves {
            containers.push(list(vec![item(vec![leaf.clone()]), item(vec![text("next")])]));
            containers.push(list(vec![item(vec![list(vec![item(vec![leaf.clone()])])])]));
            containers.push(table(vec![vec![leaf.clone()]], TableKind::Layout));
            containers.push(table(vec![vec![leaf.clone(), empty()]], TableKind::Data));
            containers.push(Block::BlockQuote(vec![leaf.clone()]));
        }
        leaves.append(&mut containers);
        let note = Note { id: "n".into(), kind: NoteKind::Footnote, blocks: vec![text("N")] };
        let doc = Document { blocks: leaves, notes: vec![note], assets: Vec::new() };
        agrees_with_rendering(&doc, "crafted");
    }

    #[test]
    fn what_the_model_says_each_corpus_block_renders_is_what_it_renders() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let mut dirs = vec![root.clone()];
        let mut checked = 0;
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if path != root.join("abuse") {
                        dirs.push(path);
                    }
                    continue;
                }
                let bytes = std::fs::read(&path).unwrap();
                let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                let format = crate::Format::sniff(&bytes)
                    .or_else(|| crate::Format::from_extension(extension))
                    .filter(|format| *format != crate::Format::Pdf);
                let Some(format) = format else { continue };
                let parse_ctx = ParseCtx::default();
                if let Ok(doc) = crate::formats::parse(&parse_ctx, &bytes, format) {
                    agrees_with_rendering(&doc, &path.display().to_string());
                    checked += 1;
                }
            }
        }
        assert!(checked > 60, "{checked} documents checked");
    }
}
