//! What a block's rendering holds, read from the model before anything is
//! written: whether it shows anything, and whether it holds an empty line,
//! which makes the list item it sits alone in loose.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::Hash;
use std::ptr;

use crate::ImagePolicy;
use crate::error::ConvertError;
use crate::model::{Block, CellSlot, Inline, List, OcrOrigin, Style};
use crate::render::markdown::escape::InlineContext;
use crate::render::markdown::inline::{Label, Norm, normalize, render_inlines, url_label};
use crate::render::markdown::sink::{Meter, Text};
use crate::render::markdown::{Ctx, unwrap};

#[derive(Default)]
struct Lines {
    line_shown: bool,
    shown: bool,
    gap: bool,
    blank: bool,
}

impl Lines {
    fn text(&mut self, text: &str) {
        for c in text.chars() {
            match c {
                '\n' => self.line_break(),
                c if !c.is_whitespace() => self.line_shown = true,
                _ => {}
            }
        }
    }

    fn mark(&mut self) {
        self.line_shown = true;
    }

    fn line_break(&mut self) {
        if self.line_shown {
            self.blank |= self.gap;
            self.shown = true;
            self.gap = false;
        } else {
            self.gap |= self.shown;
        }
        self.line_shown = false;
    }

    /// Whether any line shows, and whether an empty line sits between two
    /// that do.
    fn finish(mut self) -> (bool, bool) {
        self.line_break();
        (self.shown, self.blank)
    }
}

/// Read the lines rendering `items` in a paragraph writes into `lines`.
fn trace(items: &[Norm<'_>], lines: &mut Lines) {
    for item in items {
        match item {
            Norm::Text { text, style } if *style == Style::PLAIN => lines.text(text),
            Norm::Link { label: Label::Content(items), .. } => {
                lines.mark();
                trace(items, lines);
                lines.mark();
            }
            Norm::LineBreak => lines.line_break(),
            Norm::Text { .. }
            | Norm::Link { .. }
            | Norm::Image { .. }
            | Norm::NoteRef(_)
            | Norm::Anchor(_)
            | Norm::Math(_)
            | Norm::Checkbox(_) => lines.mark(),
        }
    }
}

/// Whether rendering `inlines` in `ctx` writes anything but whitespace;
/// remembered for each link's content, which every enclosing link asks
/// about again, and charged one unit per inline the first time it is read.
pub(super) fn nonspace(
    inlines: &[Inline],
    ctx: InlineContext,
    rc: &Ctx<'_>,
    meter: &Meter<'_>,
) -> Result<bool, ConvertError> {
    remembered(&rc.shapes.nonspace, (inlines.as_ptr(), ctx), || {
        any(inlines, |inline| {
            meter.charge(1)?;
            Ok(match inline {
                Inline::Text { text, .. } => !text.trim().is_empty(),
                Inline::Link { content, target } => {
                    nonspace(content, ctx, rc, meter)? || url_label(target).is_some()
                }
                Inline::Anchor(id) => rc.anchors.html_id(id).is_some(),
                Inline::Math(tex) => !tex.trim().is_empty(),
                Inline::Checkbox(_) => true,
                Inline::Image { .. } => rc.images != ImagePolicy::Omit,
                Inline::NoteRef(id) => rc.nums.contains_key(id),
                Inline::LineBreak => ctx == InlineContext::Block,
            })
        })
    })
}

/// Whether a paragraph of `inlines` shows anything, and whether it holds an
/// empty line.
fn paragraph(
    inlines: &[Inline],
    rc: &Ctx<'_>,
    meter: &Meter<'_>,
) -> Result<(bool, bool), ConvertError> {
    let mut lines = Lines::default();
    let items = normalize(inlines, InlineContext::Block, Style::PLAIN, rc, meter)?;
    trace(&items, &mut lines);
    Ok(lines.finish())
}

/// Whether `block` renders anything.
pub(super) fn shows(block: &Block, rc: &Ctx<'_>, meter: &Meter<'_>) -> Result<bool, ConvertError> {
    Ok(match block {
        Block::Paragraph(inlines) => paragraph(inlines, rc, meter)?.0,
        Block::Heading { content, .. } => nonspace(content, InlineContext::Heading, rc, meter)?,
        Block::List(list) => !list.items.is_empty(),
        Block::Table(table) => {
            any(unwrap(table, rc, meter)?.grid.iter().flatten(), |slot| match slot {
                CellSlot::Origin(cell) => any(&cell.blocks, |b| shows_in_cell(b, rc, meter)),
                CellSlot::Covered { .. } => Ok(true),
            })?
        }
        Block::BlockQuote(blocks) => any(blocks, |b| shows(b, rc, meter))?,
        Block::Gap(gap) => match rc.fills.filled(gap) {
            Some(blocks) => any(blocks, |b| shows(b, rc, meter))?,
            None => matches!(gap.origin, OcrOrigin::Page(_)),
        },
        Block::CodeBlock { .. } | Block::Rule => true,
        Block::UnitStart(_) => false,
        Block::Math(tex) => !tex.trim().is_empty(),
    })
}

/// Whether `test` holds for any of `items`, stopping at the first that does or fails.
pub(super) fn any<T>(
    items: impl IntoIterator<Item = T>,
    mut test: impl FnMut(T) -> Result<bool, ConvertError>,
) -> Result<bool, ConvertError> {
    for item in items {
        if test(item)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `block` adds anything to the table cell it is flattened into.
fn shows_in_cell(block: &Block, rc: &Ctx<'_>, meter: &Meter<'_>) -> Result<bool, ConvertError> {
    match block {
        Block::Heading { content: inlines, .. } | Block::Paragraph(inlines) => {
            nonspace(inlines, InlineContext::TableCell, rc, meter)
        }
        Block::List(list) => {
            any(&list.items, |item| any(&item.blocks, |b| shows_in_cell(b, rc, meter)))
        }
        Block::Table(table) => {
            any(table.origins(), |cell| any(&cell.blocks, |b| shows_in_cell(b, rc, meter)))
        }
        Block::BlockQuote(blocks) => any(blocks, |b| shows_in_cell(b, rc, meter)),
        Block::CodeBlock { text: source, .. } | Block::Math(source) => {
            Ok(!source.trim().is_empty())
        }
        Block::Gap(gap) => {
            any(rc.fills.filled(gap).unwrap_or_default(), |b| shows_in_cell(b, rc, meter))
        }
        Block::Rule | Block::UnitStart(_) => Ok(false),
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
pub(super) fn loose(list: &List, rc: &Ctx<'_>, meter: &Meter<'_>) -> Result<bool, ConvertError> {
    remembered(&rc.shapes.loose, ptr::from_ref(list), || {
        any(&list.items, |item| {
            let mut placed = item.blocks.iter().flat_map(|block| placed(block, rc));
            Ok(match (placed.next(), placed.next()) {
                (None, _) => false,
                (Some(only), None) => blank(only, rc, meter)?,
                _ => true,
            })
        })
    })
}

/// What `block` writes in its place: a filled gap its fill, an image gap not filled nothing.
fn placed<'d>(block: &'d Block, rc: &Ctx<'d>) -> &'d [Block] {
    match block {
        Block::Gap(gap) => match rc.fills.filled(gap) {
            Some(blocks) => blocks,
            None if matches!(gap.origin, OcrOrigin::Image(_)) => &[],
            None => std::slice::from_ref(block),
        },
        _ => std::slice::from_ref(block),
    }
}

fn blank_list(list: &List, rc: &Ctx<'_>, meter: &Meter<'_>) -> Result<bool, ConvertError> {
    remembered(&rc.shapes.blank, ptr::from_ref(list), || {
        if list.items.len() > 1 && loose(list, rc, meter)? {
            return Ok(true);
        }
        any(&list.items, |item| blank_body(&item.blocks, rc, meter))
    })
}

fn remembered<K: Eq + Hash>(
    memo: &RefCell<HashMap<K, bool>>,
    key: K,
    compute: impl FnOnce() -> Result<bool, ConvertError>,
) -> Result<bool, ConvertError> {
    if let Some(&known) = memo.borrow().get(&key) {
        return Ok(known);
    }
    let value = compute()?;
    memo.borrow_mut().insert(key, value);
    Ok(value)
}

/// Whether `blocks`, joined by empty lines, render an empty line.
fn blank_body(blocks: &[Block], rc: &Ctx<'_>, meter: &Meter<'_>) -> Result<bool, ConvertError> {
    let mut showing = 0;
    for block in blocks {
        if blank(block, rc, meter)? {
            return Ok(true);
        }
        if shows(block, rc, meter)? {
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
fn blank(block: &Block, rc: &Ctx<'_>, meter: &Meter<'_>) -> Result<bool, ConvertError> {
    Ok(match block {
        Block::Paragraph(inlines) => paragraph(inlines, rc, meter)?.1,
        Block::Heading { content, .. } => {
            let mut text = Text::new(meter);
            render_inlines(content, InlineContext::Heading, rc, &mut text)?;
            has_empty_line(["# ", text.as_str().trim()])
        }
        Block::List(list) => blank_list(list, rc, meter)?,
        Block::CodeBlock { lang, text } => has_empty_line([
            "```",
            lang.as_deref().unwrap_or(""),
            "\n",
            text.trim_end_matches('\n'),
            "\n```",
        ]),
        Block::Math(tex) => !tex.trim().is_empty() && has_empty_line(["$$\n", tex.trim(), "\n$$"]),
        Block::Gap(gap) => match rc.fills.filled(gap) {
            Some(blocks) => blank_body(blocks, rc, meter)?,
            None => false,
        },
        Block::Table(_) | Block::BlockQuote(_) | Block::Rule | Block::UnitStart(_) => false,
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
#[expect(clippy::disallowed_methods, reason = "the tests walk the fixture tree")]
mod tests {
    use super::*;
    use crate::ctx::ParseCtx;
    use crate::ledger::Ledger;
    use crate::model::{
        Cell, Document, ImageSource, LinkTarget, ListItem, MarkerKind, Note, NoteKind, Table,
        VerticalPosition,
    };
    use crate::ocr::GapFills;
    use crate::render::markdown::sink::Sink;
    use crate::render::markdown::table::render_cell;
    use crate::render::markdown::write_block;
    use std::assert_matches;

    fn render_block(block: &Block, rc: &Ctx<'_>) -> Option<String> {
        let ledger = Ledger::new(u64::MAX, None);
        let meter = Meter::new(&ledger, u64::MAX);
        let mut sink = Sink::new(&meter);
        write_block(block, rc, &mut sink, None).ok()?;
        sink.finish().ok().filter(|markdown| !markdown.is_empty())
    }

    /// Hold every fact read from the model against the rendering of each
    /// block and table cell of `doc`, nested ones included.
    fn agrees_with_rendering(doc: &Document, name: &str) {
        let ledger = Ledger::new(u64::MAX, None);
        let meter = Meter::new(&ledger, u64::MAX);
        let fills = GapFills::default();
        let rc = Ctx::of(doc, &fills, ImagePolicy::Placeholder, &meter).unwrap();
        let mut blocks: Vec<&Block> = doc.blocks.iter().collect();
        blocks.extend(doc.notes.iter().flat_map(|note| &note.blocks));
        while let Some(block) = blocks.pop() {
            let markdown = render_block(block, &rc);
            assert_eq!(shows(block, &rc, &meter).unwrap(), markdown.is_some(), "{name}: {block:?}");
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
                    for cell in table.origins() {
                        let mut text = Text::new(&meter);
                        render_cell(cell, &rc, &mut text).unwrap();
                        let flattened =
                            any(&cell.blocks, |b| shows_in_cell(b, &rc, &meter)).unwrap();
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

    fn table(cells: Vec<Vec<Block>>) -> Block {
        let row = cells.into_iter().map(Cell::new).collect();
        Block::Table(Table::from_rows(vec![row], 0))
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the table of crafted blocks checked as one document"
    )]
    fn what_the_model_says_a_block_renders_is_what_it_renders() {
        let bold = Style { bold: true, ..Style::PLAIN };
        let code = Style { code: true, ..Style::PLAIN };
        let raised = Style { position: VerticalPosition::Superscript, ..Style::PLAIN };
        let lowered_code = Style { position: VerticalPosition::Subscript, ..code };
        let external = || LinkTarget::External("https://example.com".into());
        let image = |alt: &str, source| Inline::Image {
            origin: crate::model::ImageOrigin::new(crate::Region::Body, None),
            alt: alt.into(),
            source,
        };
        let mut paragraphs: Vec<Vec<Inline>> = vec![
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
                link(vec![plain("\n")], LinkTarget::anchor("nowhere".into())),
                plain("b"),
            ],
            vec![
                plain("a\n"),
                link(vec![plain("\n")], LinkTarget::anchor("t".into())),
                plain("\nb"),
            ],
            vec![Inline::Anchor("t".into()), plain("\n\n")],
            vec![
                plain("a\n"),
                link(vec![plain(" ")], LinkTarget::anchor(String::new())),
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
            vec![styled("a\n\nb", raised)],
            vec![styled("a\n\nb", lowered_code)],
            vec![plain("x"), styled("\n\n y", raised), plain("\n")],
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
        let refused = || Inline::Link {
            content: vec![plain(" ")],
            target: LinkTarget::External("vbscript:x".into()),
        };
        let unplaced = || Inline::Link {
            content: vec![plain(" ")],
            target: LinkTarget::Unit(crate::model::Unit::new(
                crate::model::UnitKind::Slide,
                std::num::NonZeroU32::MIN,
            )),
        };
        paragraphs.extend([vec![refused()], vec![unplaced()]]);
        let other = || LinkTarget::External("https://b.test".into());
        paragraphs.extend([
            vec![link(vec![plain("\n"), link(vec![plain("x")], other())], external())],
            vec![link(vec![link(vec![plain("x")], other()), plain("\n\n")], external())],
            vec![link(
                vec![plain("a\n"), link(vec![plain("\n")], other()), plain("\nb")],
                external(),
            )],
            vec![link(vec![plain("a\n\n"), link(vec![plain("b")], other())], external())],
            vec![link(vec![plain(" "), link(vec![plain(" ")], other())], external())],
            vec![link(vec![plain("\n"), refused(), plain("\n")], external())],
        ]);
        let headings: Vec<Vec<Inline>> = vec![
            vec![refused()],
            vec![unplaced()],
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
            table(vec![vec![text("a"), text("b")]]),
            table(vec![vec![text("a")]]),
            table(vec![vec![empty()]]),
            Block::BlockQuote(vec![text("a"), text("b")]),
            Block::BlockQuote(vec![empty()]),
            table(vec![vec![empty()], vec![]]),
            table(vec![
                vec![Block::Paragraph(vec![Inline::LineBreak])],
                vec![Block::Paragraph(vec![plain("\\")])],
                vec![Block::CodeBlock { lang: None, text: " ".into() }, Block::Rule],
                vec![table(vec![vec![empty()], vec![empty()]])],
                vec![list(vec![item(vec![empty()]), item(vec![])])],
                vec![Block::BlockQuote(vec![Block::heading(1, vec![Inline::LineBreak])])],
                vec![Block::Math(" ".into())],
            ]),
        ];
        let parse_ctx = ParseCtx::default();
        let mut covered = crate::model::GridBuilder::new(&parse_ctx);
        covered.next_row();
        let unshown = Block::Paragraph(vec![Inline::NoteRef("missing".into())]);
        covered.place(Cell { col_span: 2, ..Cell::new(vec![unshown]) }).unwrap();
        let covered = Block::Table(covered.finish().unwrap());
        assert_matches!(&covered, Block::Table(table) if !table.grid.is_empty());
        containers.push(list(vec![item(vec![list(vec![item(vec![covered.clone(), text("x")])])])]));
        containers.push(covered);
        for leaf in &leaves {
            containers.push(list(vec![item(vec![leaf.clone()]), item(vec![text("next")])]));
            containers.push(list(vec![item(vec![list(vec![item(vec![leaf.clone()])])])]));
            containers.push(table(vec![vec![leaf.clone()]]));
            containers.push(table(vec![vec![leaf.clone(), empty()]]));
            containers.push(Block::BlockQuote(vec![leaf.clone()]));
        }
        leaves.append(&mut containers);
        let note = Note { id: "n".into(), kind: NoteKind::Footnote, blocks: vec![text("N")] };
        let doc = Document {
            blocks: leaves,
            notes: vec![note],
            assets: Vec::new(),
            ..Document::default()
        };
        agrees_with_rendering(&doc, "crafted");
    }

    #[test]
    fn link_content_is_remembered_for_each_context_it_is_read_in() {
        let ledger = Ledger::new(u64::MAX, None);
        let meter = Meter::new(&ledger, u64::MAX);
        let doc = Document::default();
        let fills = GapFills::default();
        let rc = Ctx::of(&doc, &fills, ImagePolicy::Placeholder, &meter).unwrap();
        let content = [Inline::LineBreak];
        for (label, ctx, shown) in [
            ("block", InlineContext::Block, true),
            ("heading", InlineContext::Heading, false),
            ("block again", InlineContext::Block, true),
            ("table cell", InlineContext::TableCell, false),
        ] {
            assert_eq!(nonspace(&content, ctx, &rc, &meter).unwrap(), shown, "{label}");
        }
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
                    .or_else(|| crate::Format::from_extension(extension));
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
