mod images;
mod marks;
mod selection;

use super::{
    Ctx, Meter, RenderLoss, Rendered, Unread, check_nesting, document_to_markdown, render,
};
use crate::ImagePolicy::{self, AltText, Omit, Placeholder};
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Diagnostics, Region};
use crate::error::ConvertError;
use crate::ledger::Ledger;
use crate::limits::Limit;
use crate::model::{
    AnchorId, AssetId, Block, Cell, CellSlot, Document, GridBuilder, ImageOrigin, ImageSource,
    Inline, LinkTarget, List, ListItem, MarkerKind, Note, NoteKind, Style, Table, Unit, UnitKind,
    VerticalPosition,
};
use crate::ocr::GapFills;
use crate::shared::text::collapse_ws;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::assert_matches;
use std::num::NonZeroU32;

const BOLD: Style = Style { bold: true, ..Style::PLAIN };
const ITALIC: Style = Style { italic: true, ..Style::PLAIN };
const STRIKE: Style = Style { strike: true, ..Style::PLAIN };
const CODE: Style = Style { code: true, ..Style::PLAIN };
const SUP: Style = Style { position: VerticalPosition::Superscript, ..Style::PLAIN };
const SUB: Style = Style { position: VerticalPosition::Subscript, ..Style::PLAIN };
const READ: Unread<'static> = Unread { notes: false, targets: None };

fn with_notes(blocks: Vec<Block>, notes: Vec<Note>) -> String {
    document_to_markdown(&Document { blocks, notes, assets: Vec::new() })
}

fn doc(blocks: Vec<Block>) -> String {
    with_notes(blocks, Vec::new())
}

fn paragraph(inlines: Vec<Inline>) -> String {
    doc(vec![Block::Paragraph(inlines)])
}

fn styled(text: &str, style: Style) -> Inline {
    Inline::Text { text: text.into(), style }
}

fn text(t: &str) -> Block {
    Block::Paragraph(vec![Inline::plain(t)])
}

fn heading(level: u8, text: &str) -> Block {
    Block::heading(level, vec![Inline::plain(text)])
}

fn anchored(level: u8, id: &str, content: Vec<Inline>) -> Block {
    Block::Heading { level, anchor: Some(id.into()), content }
}

fn cell(t: &str) -> Cell {
    Cell::from_inlines(vec![Inline::plain(t)])
}

fn table_from(rows: Vec<Vec<Cell>>, header_rows: usize) -> Block {
    Block::Table(Table::from_rows(rows, header_rows))
}

fn covered() -> CellSlot {
    CellSlot::Covered { origin_row: 0, origin_col: 0 }
}

fn uneven_table(first: Inline, last: Inline) -> Block {
    let origin = |inline| CellSlot::Origin(Cell::from_inlines(vec![inline]));
    let grid = vec![vec![origin(first), covered()], vec![], vec![covered(), origin(last)]];
    Block::Table(Table { grid, header_rows: 0 })
}

fn frame(blocks: Vec<Block>) -> Block {
    table_from(vec![vec![Cell::new(blocks)]], 0)
}

fn item(blocks: Vec<Block>) -> ListItem {
    ListItem { blocks, marker_label: None }
}

fn bulleted(blocks: Vec<Block>) -> Block {
    Block::List(List { marker: MarkerKind::Bullet, start: 1, items: vec![item(blocks)] })
}

fn list_of(marker: MarkerKind, start: u64, labels: &[Option<&str>]) -> Block {
    let items = labels
        .iter()
        .zip(["a", "b", "c"])
        .map(|(label, t)| ListItem {
            blocks: vec![text(t)],
            marker_label: label.map(str::to_string),
        })
        .collect();
    Block::List(List { marker, start, items })
}

fn note(id: &str, blocks: Vec<Block>) -> Note {
    Note { id: id.into(), kind: NoteKind::Footnote, blocks }
}

fn external(url: &str) -> LinkTarget {
    LinkTarget::External(url.into())
}

fn link(content: Vec<Inline>, target: LinkTarget) -> Inline {
    Inline::Link { content, target }
}

fn labelled(label: &str, target: LinkTarget) -> Vec<Inline> {
    vec![link(vec![Inline::plain(label)], target)]
}

fn linked(label: &str, target: LinkTarget) -> String {
    paragraph(labelled(label, target))
}

fn image(alt: &str, source: ImageSource) -> Inline {
    Inline::Image { origin: ImageOrigin::new(Region::Body, None), alt: alt.into(), source }
}

fn unit(kind: UnitKind, n: u32) -> Unit {
    Unit::new(kind, NonZeroU32::new(n).unwrap())
}

fn emphasis(bits: u8) -> Style {
    Style { bold: bits & 1 != 0, italic: bits & 2 != 0, strike: bits & 4 != 0, ..Style::PLAIN }
}

fn render_at(
    document: &Document,
    max_work: u64,
    max_output: u64,
    images: ImagePolicy,
) -> Result<Rendered, ConvertError> {
    let ledger = Ledger::new(max_work, None);
    render(document, &GapFills::default(), &Meter::new(&ledger, max_output), images)
}

fn rendered(document: &Document, images: ImagePolicy) -> String {
    render_at(document, u64::MAX, u64::MAX, images).unwrap().markdown
}

fn least_work(passes: impl Fn(&Meter<'_>) -> bool) -> u64 {
    (0..1_000)
        .find(|&max| passes(&Meter::new(&Ledger::new(max, None), u64::MAX)))
        .expect("a budget under 1000 passes")
}

fn render_losses(document: &Document, unread: Unread<'_>, images: ImagePolicy) -> Vec<RenderLoss> {
    let ledger = Ledger::new(u64::MAX, None);
    super::render_losses(document, unread, images, &Meter::new(&ledger, u64::MAX)).unwrap()
}

#[test]
fn heading_and_paragraph() {
    let md = doc(vec![heading(2, "Title"), text("Hello world.")]);
    assert_eq!(md, "## Title\n\nHello world.\n");
}

#[test]
fn math_renders_in_dollar_delimiters_and_text_dollars_are_escaped() {
    let md = doc(vec![
        Block::Paragraph(vec![
            Inline::plain("Costs $5 or $6, and "),
            Inline::Math("x_1 < y".into()),
            Inline::plain(" holds."),
        ]),
        Block::Math("\\sum_{i=1}^{n} i $".into()),
        text("Plans at $20 or $17.50."),
        text("Pair $x with y$ here."),
    ]);
    assert_eq!(
        md,
        "Costs \\$5 or \\$6, and $x_1 < y$ holds.\n\n$$\n\\sum_{i=1}^{n} i \\$\n$$\n\n\
         Plans at $20 or $17.50.\n\nPair \\$x with y$ here.\n"
    );
}

#[test]
fn plain_text_is_escaped_only_where_a_parser_would_read_syntax() {
    for (source, expected) in [
        ("a *bold* _it_ ~st~ `code`", "a \\*bold* \\_it_ \\~st~ \\`code`\n"),
        ("see [really] and <b>hi</b>", "see \\[really] and \\<b>hi\\</b>\n"),
        ("2 * 3 = 6 and 5*6 #tag", "2 * 3 = 6 and 5*6 #tag\n"),
        ("x < 5, ~10%, file_name, a[1", "x < 5, ~10%, file_name, a[1\n"),
        ("snake_case_name vs _lead_", "snake_case_name vs \\_lead_\n"),
        ("- not a list", "\\- not a list\n"),
        ("1. not a list", "1\\. not a list\n"),
        ("take 2. then rest", "take 2. then rest\n"),
        (";\n~~a", ";\n~~a\n"),
        ("a ~~~", "a ~~~\n"),
        (";\n__", ";\n__\n"),
        (";\n_ _", ";\n_ _\n"),
        (";\n___ content", ";\n___ content\n"),
        ("-5°C at dawn", "-5°C at dawn\n"),
        ("1.5 million users", "1.5 million users\n"),
        ("#hashtag first", "#hashtag first\n"),
        ("--- ruled", "--- ruled\n"),
        ("---", "\\---\n"),
        ("A & B &amp; C", "A & B &amp;amp; C\n"),
        ("\\", "\\\\\n"),
        ("\\\\", "\\\\\\\\\n"),
        ("\\\\\\", "\\\\\\\\\\\\\n"),
    ] {
        assert_eq!(doc(vec![text(source)]), expected, "{source:?}");
    }
}

#[test]
fn internal_line_starts_preserve_literal_fences_and_thematic_breaks() {
    for (source, escaped) in [
        (";\n~~~", ";\n\\~\\~\\~"),
        (";\n~~~~", ";\n\\~\\~\\~\\~"),
        (";\n___", ";\n\\_\\_\\_"),
        (";\n_ _ _", ";\n\\_ _ _"),
        (";\n  ~~~x", ";\n\\~\\~\\~x"),
        (";\n```", ";\n\\`\\`\\`"),
    ] {
        let markdown = doc(vec![text(source)]);
        assert_eq!(markdown, format!("{escaped}\n"), "{source:?}");
        let (text, _) = read_back(&markdown);
        assert_eq!(text, collapse_ws(source.trim()));
    }
}

#[test]
fn literal_block_markers_round_trip_with_every_markdown_line_ending() {
    for source in [";\n___\r\nend", ";\n___\rend", ";\r___\rend", ";\r~~~\rend"] {
        let markdown = doc(vec![text(source)]);
        assert_eq!(read_back(&markdown).0, collapse_ws(source.trim()), "{markdown:?}");
    }
    for suffix in ["___\rend", "# title\rend", "- item\rend"] {
        let markdown = paragraph(vec![styled(";\r", ITALIC), Inline::plain(suffix)]);
        assert_eq!(
            read_back(&markdown).0,
            format!("; {}", collapse_ws(suffix.trim())),
            "{markdown:?}"
        );
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "the table of styled runs and their seams")]
fn styled_runs_and_run_seams_render_markdown_that_reads_back_as_written() {
    let plain = Inline::plain;
    let nowhere = || LinkTarget::anchor("nowhere".into());
    for (name, runs, expected) in [
        (
            "a styled dollar closes a dollar before it",
            vec![plain("cost $x"), Inline::LineBreak, styled("y$", BOLD), plain(" z")],
            "cost \\$x\\\n**y$** z\n",
        ),
        (
            "a space-padded `*` is not right-flanking, so the opener is inert",
            vec![plain("a *b 2 * 3")],
            "a *b 2 * 3\n",
        ),
        (
            "an inert opener stays inert across a run seam",
            vec![plain("a *"), Inline::LineBreak, plain("2 * 3")],
            "a *\\\n2 * 3\n",
        ),
        (
            "intraword underscores can neither open nor close",
            vec![plain("a _"), Inline::LineBreak, plain("snake_case")],
            "a _\\\nsnake_case\n",
        ),
        (
            "a `*` after punctuation and before a word character is not right-flanking",
            vec![plain("a *b .*c")],
            "a *b .*c\n",
        ),
        (
            "flanking is judged at the run's edges, so `__` between letters is intraword",
            vec![plain("a _x foo__bar")],
            "a _x foo__bar\n",
        ),
        (
            "a trailing delimiter before a styled run is escaped",
            vec![plain("star*"), styled("x", BOLD)],
            "star\\***x**\n",
        ),
        (
            "two lines each ending in a backtick do not form a code span (#45)",
            vec![plain("a `"), Inline::LineBreak, plain("b `")],
            "a \\`\\\nb `\n",
        ),
        (
            "emphasis pairs across a hard break just as code spans do",
            vec![plain("a *"), Inline::LineBreak, plain("b*")],
            "a \\*\\\nb*\n",
        ),
        (
            "a raw backtick pairs with a later code span's fence",
            vec![plain("a `"), Inline::LineBreak, styled("x", CODE)],
            "a \\`\\\n`x`\n",
        ),
        (
            "an unresolved link's fallback text supplies the `]` that pairs with an earlier `[`",
            vec![
                plain("[click"),
                Inline::LineBreak,
                link(vec![plain("here]")], nowhere()),
                plain("(https://e.test)"),
            ],
            "\\[click\\\nhere](https://e.test)\n",
        ),
        (
            "an unresolved link's fallback supplies an emphasis delimiter across the seam",
            vec![plain("a *"), Inline::LineBreak, link(vec![plain("b*")], nowhere())],
            "a \\*\\\nb*\n",
        ),
        (
            "a styled run's escaped backtick still closes a span an earlier raw backtick opens",
            vec![plain("a `"), Inline::LineBreak, styled("x`y", BOLD)],
            "a \\`\\\n**x\\`y**\n",
        ),
        (
            "a whitespace-only code run loses its styling and emits no fence",
            vec![
                plain("a `"),
                Inline::LineBreak,
                link(vec![styled("  ", CODE), plain("x")], external("https://e.test")),
            ],
            "a `\\\n[  x](https://e.test)\n",
        ),
        (
            "bold trailing space moves outside the markers",
            vec![styled("bold ", BOLD), plain("plain")],
            "**bold** plain\n",
        ),
        (
            "adjacent runs of one style merge",
            vec![styled("bo", BOLD), styled("ld", BOLD)],
            "**bold**\n",
        ),
        (
            "bold italic nests italic inside bold",
            vec![styled("both", Style { bold: true, italic: true, ..Style::PLAIN })],
            "**_both_**\n",
        ),
        (
            "a whitespace-only run loses its styling",
            vec![styled("a", BOLD), styled(" ", ITALIC), styled("b", BOLD)],
            "**a b**\n",
        ),
        (
            "whitespace between runs of different styles bridges nothing",
            vec![styled("a", BOLD), plain(" "), styled("b", ITALIC)],
            "**a** _b_\n",
        ),
        ("a code span widens its fence around a backtick", vec![styled("a`b", CODE)], "``a`b``\n"),
        (
            "a hard break ends the line with a backslash",
            vec![plain("line one"), Inline::LineBreak, plain("line two")],
            "line one\\\nline two\n",
        ),
        (
            "a line-start escape applies after a hard break",
            vec![plain("intro"), Inline::LineBreak, plain("- dash")],
            "intro\\\n\\- dash\n",
        ),
        ("a lone hard break is blank", vec![Inline::LineBreak], ""),
    ] {
        assert_eq!(paragraph(runs), expected, "{name}");
    }
}

#[test]
fn links_images_anchors_and_math_in_a_paragraph_render_their_destinations_and_labels() {
    for (name, inlines, expected) in [
        (
            "a url with parentheses is wrapped in angle brackets",
            labelled("site", external("https://example.com/a(b)")),
            "[site](<https://example.com/a(b)>)\n",
        ),
        (
            "a relative link is kept",
            labelled("next", LinkTarget::Relative("chapter2.xhtml".into())),
            "[next](chapter2.xhtml)\n",
        ),
        (
            "a mailto link is kept",
            labelled("mail", external("mailto:a@b.c")),
            "[mail](mailto:a@b.c)\n",
        ),
        (
            "an unresolved anchor degrades to its text",
            labelled("note", LinkTarget::anchor("nowhere".into())),
            "note\n",
        ),
        (
            "an unavailable image renders a visible marker",
            vec![image("chart", ImageSource::Unavailable)],
            "\\[chart\\]\n",
        ),
        (
            "a link label escapes its closing bracket",
            labelled("x]", external("https://e.com")),
            "[x\\]](https://e.com)\n",
        ),
        (
            "an image alt escapes brackets and backslashes",
            vec![image("a[b]c\\", ImageSource::External("https://e.com/i.png".into()))],
            "[a\\[b\\]c\\\\](https://e.com/i.png)\n",
        ),
        (
            "url angle brackets are encoded without bracketing",
            labelled("link", external("https://e.test/a<b>c")),
            "[link](https://e.test/a%3Cb%3Ec)\n",
        ),
        (
            "url controls cannot split the document",
            labelled("link", external("https://e.test/a\nb")),
            "[link](https://e.test/a%0Ab)\n",
        ),
        (
            "an anchor no link targets renders nothing",
            vec![Inline::Anchor("standalone-mark".into()), Inline::plain("No link points here.")],
            "No link points here.\n",
        ),
        (
            "math keeps its pipes and an escaped dollar",
            vec![Inline::plain("a "), Inline::Math("|x| a\\$b".into())],
            "a $|x| a\\$b$\n",
        ),
    ] {
        assert_eq!(paragraph(inlines), expected, "{name}");
    }
}

#[test]
fn composite_marker_labels_are_escaped() {
    // Source-derived labels must not alter Markdown structure.
    let list = List {
        marker: MarkerKind::Decimal,
        start: 1,
        items: vec![ListItem { blocks: vec![text("x")], marker_label: Some("#1\n*a*".into()) }],
    };
    let md = doc(vec![Block::List(list)]);
    assert_eq!(md.lines().count(), 1, "control characters must not split the marker");
    assert!(!md.contains("*a*"), "emphasis in a label must be escaped: {md}");
    assert!(md.starts_with("- "), "labelled items render as literal markers: {md}");
}

#[test]
#[expect(clippy::too_many_lines, reason = "the table of rows and their rendered GFM")]
fn tables_render_gfm_rows_with_each_cell_escaped_and_trimmed() {
    let code = |t: &str| Cell::from_inlines(vec![styled(t, CODE)]);
    let empty = Cell::default;
    for (name, rows, header_rows, expected) in [
        (
            "math in a cell escapes its pipes",
            vec![vec![cell("abs"), Cell::from_inlines(vec![Inline::Math("|x|".into())])]],
            0,
            "|  |  |\n| --- | --- |\n| abs | $\\|x\\|$ |\n",
        ),
        (
            "a negative number in a cell is left alone",
            vec![vec![cell("-42"), cell("x")]],
            0,
            "|  |  |\n| --- | --- |\n| -42 | x |\n",
        ),
        (
            "a one-cell data table is not unwrapped",
            vec![vec![cell("x")]],
            0,
            "|  |\n| --- |\n| x |\n",
        ),
        (
            "trailing empty rows and columns are trimmed",
            vec![
                vec![cell("a"), cell("b"), empty()],
                vec![cell("c"), empty(), empty()],
                vec![empty(), empty(), empty()],
            ],
            1,
            "| a | b |\n| --- | --- |\n| c |  |\n",
        ),
        (
            "a header row is followed by its rule, and a pipe in a cell is escaped",
            vec![vec![cell("Name"), cell("Age")], vec![cell("Ann | Bob"), cell("30")]],
            1,
            "| Name | Age |\n| --- | --- |\n| Ann \\| Bob | 30 |\n",
        ),
        (
            "a headerless ragged table is padded to its widest row",
            vec![vec![cell("a")], vec![cell("b"), cell("c")]],
            0,
            "|  |  |\n| --- | --- |\n| a |  |\n| b | c |\n",
        ),
        (
            "paragraphs of one cell are joined by a line break",
            vec![vec![Cell::new(vec![text("one"), text("two")]), cell("x")]],
            0,
            "|  |  |\n| --- | --- |\n| one<br>two | x |\n",
        ),
        (
            "a url pipe cannot split a cell",
            vec![vec![Cell::from_inlines(vec![link(Vec::new(), external("https://e.test/a|b"))])]],
            0,
            "|  |\n| --- |\n| [https://e.test/a\\|b](https://e.test/a%7Cb) |\n",
        ),
        (
            "code span pipes cannot split cells",
            vec![vec![code("a | b"), code(r"a \| b")]],
            0,
            concat!("|  |  |\n", "| --- | --- |\n", r"| `a \| b` | `a \\\| b` |", "\n"),
        ),
        (
            "a code span break before a pipe cannot split a cell",
            vec![vec![code("a\\\n| b")]],
            0,
            concat!("|  |\n", "| --- |\n", r"| `a\ \| b` |", "\n"),
        ),
        (
            "checkboxes in cells render as task markers",
            vec![vec![
                Cell::from_inlines(vec![Inline::Checkbox(true)]),
                Cell::from_inlines(vec![Inline::Checkbox(false), Inline::plain("Wall")]),
            ]],
            0,
            "|  |  |\n| --- | --- |\n| [x] | [ ] Wall |\n",
        ),
        (
            "a code block in a cell becomes a code span",
            vec![vec![Cell::new(vec![Block::CodeBlock {
                lang: None,
                text: "let `x` = 1;".into(),
            }])]],
            0,
            "|  |\n| --- |\n| ``let `x` = 1;`` |\n",
        ),
        (
            "edge whitespace of a cell is trimmed, being padding from spreadsheets and CSV",
            vec![vec![cell("  padded\t"), cell("plain")]],
            0,
            "|  |  |\n| --- | --- |\n| padded | plain |\n",
        ),
    ] {
        assert_eq!(doc(vec![table_from(rows, header_rows)]), expected, "{name}");
    }
}

#[test]
fn merged_cells_render_blank_covered_positions() {
    let parse_ctx = ParseCtx::default();
    let mut b = GridBuilder::new(&parse_ctx);
    b.next_row();
    b.place(Cell::spanning(vec![text("wide")], 2, 1)).unwrap();
    b.place(cell("end")).unwrap();
    b.next_row();
    for t in ["a", "b", "c"] {
        b.place(cell(t)).unwrap();
    }
    let mut table = b.finish().unwrap();
    table.header_rows = 1;
    let md = doc(vec![Block::Table(table)]);
    assert_eq!(md, "| wide |  | end |\n| --- | --- | --- |\n| a | b | c |\n");
}

#[test]
fn trailing_covered_columns_are_preserved() {
    let parse_ctx = ParseCtx::default();
    let mut b = GridBuilder::new(&parse_ctx);
    b.next_row();
    b.place(Cell::spanning(vec![text("wide")], 3, 1)).unwrap();
    let mut table = b.finish().unwrap();
    table.header_rows = 1;
    let md = doc(vec![Block::Table(table)]);
    assert_eq!(md, "| wide |  |  |\n| --- | --- | --- |\n");
}

#[test]
fn a_cell_drops_blank_parts_lines_and_rows() {
    let nested = table_from(vec![vec![cell(" "), cell(" ")], vec![cell("x"), cell("y")]], 0);
    let md = doc(vec![frame(vec![
        text("a"),
        text(" "),
        Block::Paragraph(vec![
            Inline::plain("a"),
            Inline::LineBreak,
            Inline::LineBreak,
            Inline::plain("b"),
        ]),
        nested,
    ])]);
    assert_eq!(md, "|  |\n| --- |\n| a<br>a<br>b<br>x / y |\n");
}

#[test]
fn a_ragged_table_renders_as_its_padded_twin() {
    let ragged = vec![
        vec![cell("h")],
        (0..37).map(|i| cell(&i.to_string())).collect(),
        Vec::new(),
        vec![cell("a"), cell(" "), cell("b")],
    ];
    let padded = ragged
        .iter()
        .map(|row| {
            let mut row = row.clone();
            row.resize_with(37, Cell::default);
            row
        })
        .collect();
    let md = doc(vec![table_from(ragged, 1)]);
    assert_eq!(md, doc(vec![table_from(padded, 1)]));
    let blank = |cells: usize| "  |".repeat(cells);
    let numbers: String = (0..37).map(|i| format!(" {i} |")).collect();
    assert_eq!(
        md,
        format!(
            "| h |{}\n|{}\n|{numbers}\n|{}\n| a |  | b |{}\n",
            blank(36),
            " --- |".repeat(37),
            blank(37),
            blank(34),
        )
    );
}

#[test]
fn a_nested_cell_holds_its_text_once_whatever_its_depth() {
    let long = "x".repeat(10_000);
    let mut nested = cell(&long);
    for _ in 0..100 {
        nested = Cell::new(vec![table_from(vec![vec![nested]], 0)]);
    }
    let document = Document::default();
    let ledger = Ledger::new(u64::MAX, None);
    let meter = Meter::new(&ledger, u64::MAX);
    let mut out = super::sink::Text::new(&meter);
    let fills = GapFills::default();
    let rc = Ctx::of(&document, &fills, Placeholder, &meter).unwrap();
    super::table::render_cell(&nested, &rc, &mut out).unwrap();
    assert_eq!(out.as_str(), long);
    assert!(meter.admitted() <= long.len() as u64 + 100 * 8, "{}", meter.admitted());
}

#[test]
fn a_cell_part_taken_back_keeps_the_space_held_before_it() {
    let list = bulleted(vec![
        Block::Paragraph(vec![Inline::LineBreak]),
        text(" y "),
        heading(2, " \n "),
        heading(2, " h \n\n i "),
    ]);
    let nested = table_from(vec![vec![cell(" \n "), Cell::new(vec![list])]], 0);
    let md = doc(vec![frame(vec![text("a  "), nested])]);
    assert_eq!(md, "|  |\n| --- |\n| a  <br> / •  y  **h**<br>**i** |\n");
}

#[test]
fn a_line_break_after_a_nested_table_in_a_list_item_breaks_the_cell_line() {
    let list = bulleted(vec![
        frame(vec![text("x")]),
        Block::Paragraph(vec![Inline::LineBreak, Inline::plain("y")]),
    ]);
    assert_eq!(doc(vec![frame(vec![list])]), "|  |\n| --- |\n| • x<br>y |\n");
}

#[test]
fn whitespace_a_cell_shows_counts_once_against_the_maximum() {
    let spaced = format!("a{}|b", " ".repeat(100));
    let document = Document { blocks: vec![frame(vec![text(&spaced)])], ..Document::default() };
    let md = document_to_markdown(&document);
    assert_eq!(md, format!("|  |\n| --- |\n| a{}\\|b |\n", " ".repeat(100)));
    let len = md.len() as u64;
    assert_eq!(render_at(&document, u64::MAX, len, Placeholder).unwrap().markdown, md);
    assert_matches!(
        render_at(&document, u64::MAX, len - 1, Placeholder),
        Err(ConvertError::ResourceLimit { limit: Limit::OutputBytes, .. })
    );
}

fn refused_on_what_it_holds(blocks: Vec<Block>, notes: Vec<Note>) {
    let document = Document { blocks, notes, ..Document::default() };
    let len = document_to_markdown(&document).len() as u64;
    assert_matches!(
        render_at(&document, u64::MAX, len, Placeholder),
        Err(ConvertError::ResourceLimit { limit: Limit::OutputBytes, .. }),
        "{len}"
    );
}

#[test]
fn held_paragraphs_headings_markers_and_note_labels_count_against_the_maximum() {
    refused_on_what_it_holds(vec![text("abc   ")], vec![]);
    refused_on_what_it_holds(vec![heading(1, "abc        ")], vec![]);
    let silent = note("n", vec![heading(1, " ")]);
    refused_on_what_it_holds(vec![text("abc")], vec![silent]);
    refused_on_what_it_holds(vec![bulleted(vec![text("  ")])], vec![]);
}

#[test]
fn notes_without_a_body_open_the_document() {
    let md = with_notes(Vec::new(), vec![note("n", vec![text("N")])]);
    assert_eq!(md, "[^1]: N\n");
}

#[test]
fn nested_list() {
    let inner =
        List { marker: MarkerKind::Decimal, start: 3, items: vec![item(vec![text("inner")])] };
    let md = doc(vec![bulleted(vec![text("outer"), Block::List(inner)])]);
    assert_eq!(md, "- outer\n\n  3. inner\n");
}

#[test]
fn roman_and_alpha_markers_render_literally() {
    let entry = |t: &str| item(vec![text(t)]);
    let md = doc(vec![
        Block::List(List {
            marker: MarkerKind::LowerRoman,
            start: 3,
            items: vec![entry("third"), entry("fourth")],
        }),
        Block::List(List {
            marker: MarkerKind::UpperAlpha,
            start: 27,
            items: vec![entry("double letters")],
        }),
    ]);
    assert_eq!(md, "- iii. third\n- iv. fourth\n\n* AA. double letters\n");
}

#[test]
fn blockquote() {
    let md = doc(vec![Block::BlockQuote(vec![text("quoted")])]);
    assert_eq!(md, "> quoted\n");
}

#[test]
fn empty_paragraphs_dropped() {
    let md = doc(vec![text("  "), Block::Paragraph(vec![]), text("real")]);
    assert_eq!(md, "real\n");
}

#[test]
fn footnotes() {
    let md = with_notes(
        vec![Block::Paragraph(vec![
            Inline::plain("Claim."),
            Inline::NoteRef("b".into()),
            Inline::plain(" More."),
            Inline::NoteRef("a".into()),
        ])],
        vec![
            note("a", vec![text("Second note.")]),
            note("b", vec![text("First note."), text("With a second paragraph.")]),
        ],
    );
    assert_eq!(
        md,
        "Claim.[^1] More.[^2]\n\n[^1]: First note.\n\n    With a second paragraph.\n\n[^2]: Second note.\n"
    );
}

#[test]
fn empty_and_unreferenced_notes() {
    let md = with_notes(
        vec![Block::Paragraph(vec![Inline::plain("Text"), Inline::NoteRef("empty".into())])],
        vec![note("empty", vec![Block::Paragraph(vec![])]), note("orphan", vec![text("Kept.")])],
    );
    assert_eq!(md, "Text\n\n[^1]: Kept.\n");
}

#[test]
fn duplicate_note_ids_render_one_definition() {
    let md = with_notes(
        vec![Block::Paragraph(vec![Inline::plain("Text"), Inline::NoteRef("a".into())])],
        vec![note("a", vec![text("First wins.")]), note("a", vec![text("Duplicate dropped.")])],
    );
    assert_eq!(md, "Text[^1]\n\n[^1]: First wins.\n");
}

#[test]
fn blank_first_note_definition_remains_canonical() {
    let md = with_notes(
        vec![Block::Paragraph(vec![Inline::plain("Text"), Inline::NoteRef("a".into())])],
        vec![note("a", vec![Block::Paragraph(Vec::new())]), note("a", vec![text("Usable.")])],
    );
    assert_eq!(md, "Text\n");
}

#[test]
fn an_empty_item_keeps_its_marker_and_the_numbering() {
    let entry = |t: &str| item(if t.is_empty() { Vec::new() } else { vec![text(t)] });
    let md = doc(vec![Block::List(List {
        marker: MarkerKind::Decimal,
        start: 1,
        items: vec![entry("one"), entry(""), entry("three")],
    })]);
    assert_eq!(md, "1. one\n2. \n3. three\n");
}

#[test]
fn task_list() {
    let md = doc(vec![Block::List(List {
        marker: MarkerKind::Bullet,
        start: 1,
        items: vec![
            item(vec![Block::Paragraph(vec![Inline::Checkbox(true), Inline::plain("done")])]),
            item(vec![Block::Paragraph(vec![Inline::Checkbox(false), Inline::plain(" todo")])]),
        ],
    })]);
    assert_eq!(md, "- [x] done\n- [ ] todo\n");
}

#[test]
fn anchor_on_plain_paragraph_round_trips() {
    let mark: AnchorId = "My Mark".into();
    let md = doc(vec![
        Block::Paragraph(vec![Inline::Anchor(mark.clone()), Inline::plain("Target here.")]),
        Block::Paragraph(vec![link(vec![Inline::plain("jump")], LinkTarget::anchor(mark))]),
    ]);
    assert_eq!(md, "<a id=\"my-mark\"></a>Target here.\n\n[jump](#my-mark)\n");
}

#[test]
fn heading_coincident_anchor_uses_slug() {
    let md = doc(vec![
        anchored(2, "bm1", vec![Inline::plain("Section Two")]),
        Block::Paragraph(vec![link(vec![Inline::plain("go")], LinkTarget::anchor("bm1".into()))]),
    ]);
    assert_eq!(md, "## Section Two\n\n[go](#section-two)\n");
}

#[test]
fn duplicate_heading_slugs_deduped() {
    let md = doc(vec![
        heading(1, "Same"),
        anchored(1, "x", vec![Inline::plain("Same")]),
        Block::Paragraph(vec![link(vec![Inline::plain("second")], LinkTarget::anchor("x".into()))]),
    ]);
    assert_eq!(md, "# Same\n\n# Same\n\n[second](#same-1)\n");
}

#[test]
fn a_heading_links_to_and_holds_the_anchors_paragraphs_link_to() {
    let to = |id: &str| link(vec![Inline::plain("go")], LinkTarget::anchor(id.into()));
    let target = |id: &str| Block::Paragraph(vec![Inline::Anchor(id.into()), Inline::plain("end")]);
    let cases = [
        (
            "a heading that links to a paragraph anchor",
            vec![Block::heading(1, vec![to("p")]), target("p")],
            "# [go](#p)\n\n<a id=\"p\"></a>end\n",
        ),
        (
            "an anchor in a heading that shows nothing, linked from a paragraph",
            vec![
                Block::heading(1, vec![Inline::Anchor("h".into())]),
                Block::Paragraph(vec![to("h")]),
            ],
            "# <a id=\"h\"></a>\n\n[go](#h)\n",
        ),
    ];
    for (name, blocks, expected) in cases {
        assert_eq!(doc(blocks), expected, "{name}");
    }
}

#[test]
fn code_block_keeps_its_language_hint() {
    let md = doc(vec![Block::CodeBlock { lang: Some("rust".into()), text: "fn main() {}".into() }]);
    assert_eq!(md, "```rust\nfn main() {}\n```\n");
}

#[test]
#[expect(clippy::too_many_lines, reason = "the table of rendering losses")]
fn rendering_reports_what_it_loses() {
    use DiagnosticCode::{DanglingReference as DANGLING, DuplicateNote as DUPLICATE};
    let note_ref = |id: &str| Inline::NoteRef(id.into());
    let to = |id: &str| link(vec![Inline::plain("label")], LinkTarget::anchor(id.into()));
    type Case = (&'static str, Vec<Block>, Vec<Note>, Unread<'static>, Vec<RenderLoss>);
    let cases: Vec<Case> = vec![
        (
            "a second body under a defined note id",
            vec![Block::Paragraph(vec![note_ref("a")])],
            vec![note("a", vec![text("First.")]), note("a", vec![text("Second.")])],
            READ,
            vec![(DUPLICATE, Region::Note, None, 1)],
        ),
        (
            "a second body that renders empty",
            vec![Block::Paragraph(vec![note_ref("a")])],
            vec![note("a", vec![text("First.")]), note("a", vec![frame(Vec::new())])],
            READ,
            vec![(DUPLICATE, Region::Note, None, 1)],
        ),
        (
            "a link to an anchor that exists nowhere",
            vec![Block::Paragraph(vec![to("nowhere")])],
            vec![],
            READ,
            vec![(DANGLING, Region::Body, None, 1)],
        ),
        (
            "a link to an anchor that exists",
            vec![Block::Paragraph(vec![Inline::Anchor("here".into()), to("here")])],
            vec![],
            READ,
            vec![],
        ),
        ("a link with an empty anchor", vec![Block::Paragraph(vec![to("")])], vec![], READ, vec![]),
        (
            "a reference with no note",
            vec![Block::Paragraph(vec![note_ref("missing")])],
            vec![],
            READ,
            vec![(DANGLING, Region::Body, None, 1)],
        ),
        (
            "a reference to a note",
            vec![Block::Paragraph(vec![note_ref("a")])],
            vec![note("a", vec![text("Body.")])],
            READ,
            vec![],
        ),
        (
            "links in the cells of a table whose rows differ in shape",
            vec![uneven_table(to("first"), to("last"))],
            vec![],
            READ,
            vec![(DANGLING, Region::Body, None, 2)],
        ),
        (
            "references and links in every kind of block",
            vec![
                Block::heading(1, vec![note_ref("h")]),
                bulleted(vec![Block::Paragraph(vec![to("l")])]),
                frame(vec![Block::Paragraph(vec![note_ref("t")])]),
                Block::BlockQuote(vec![Block::Paragraph(vec![link(
                    vec![note_ref("q"), to("inner")],
                    external("https://example.com"),
                )])]),
            ],
            vec![],
            READ,
            vec![(DANGLING, Region::Body, None, 5)],
        ),
        (
            "a reference and a link in a note body",
            vec![Block::Paragraph(vec![note_ref("a")])],
            vec![note(
                "a",
                vec![Block::Paragraph(vec![Inline::plain("Body"), note_ref("x"), to("y")])],
            )],
            READ,
            vec![(DANGLING, Region::Note, None, 2)],
        ),
        (
            "links the scheme gate refuses, in the body and in a shown note",
            vec![Block::Paragraph(vec![
                link(vec![Inline::plain("run")], LinkTarget::Relative(" javascript:x".into())),
                link(vec![Inline::plain("open")], external("file:///etc")),
                note_ref("a"),
            ])],
            vec![note(
                "a",
                vec![Block::Paragraph(vec![link(
                    vec![Inline::plain("img")],
                    external("data:text/html,x"),
                )])],
            )],
            READ,
            vec![
                (DiagnosticCode::UnsafeLink, Region::Body, None, 2),
                (DiagnosticCode::UnsafeLink, Region::Note, None, 1),
            ],
        ),
        (
            "a reference in a dropped second body",
            vec![Block::Paragraph(vec![note_ref("a")])],
            vec![
                note("a", vec![text("First.")]),
                note("a", vec![Block::Paragraph(vec![Inline::plain("Second"), note_ref("x")])]),
            ],
            READ,
            vec![(DUPLICATE, Region::Note, None, 1)],
        ),
        (
            "a reference in a note that renders no definition",
            vec![text("Body.")],
            vec![note("a", vec![Block::Paragraph(vec![note_ref("x")])])],
            READ,
            vec![],
        ),
        (
            "references whose notes part the parse counted unreadable",
            vec![Block::Paragraph(vec![note_ref("missing"), to("nowhere")])],
            vec![note("a", vec![Block::Paragraph(vec![Inline::plain("Body"), note_ref("x")])])],
            Unread { notes: true, targets: None },
            vec![(DANGLING, Region::Body, None, 1)],
        ),
        (
            "unitless anchor targets remain independent of unread units",
            vec![Block::Paragraph(vec![note_ref("missing"), to("nowhere")])],
            vec![note("a", vec![Block::Paragraph(vec![Inline::plain("Body"), to("y")])])],
            READ,
            vec![(DANGLING, Region::Body, None, 2), (DANGLING, Region::Note, None, 1)],
        ),
    ];
    for (name, blocks, notes, unread, expected) in cases {
        let document = Document { blocks, notes, ..Document::default() };
        assert_eq!(render_losses(&document, unread, Placeholder), expected, "{name}");
    }
}

#[test]
fn the_causes_that_silence_render_losses_are_read_from_the_parse() {
    use DiagnosticCode::{PartUnreadable, UnitUnreadable};
    let cases = [
        ("none", vec![], false),
        ("a notes part", vec![(PartUnreadable, Region::Note)], true),
        ("a body part", vec![(PartUnreadable, Region::Body)], false),
        ("a unit", vec![(UnitUnreadable, Region::Body)], false),
        ("a unit in notes", vec![(UnitUnreadable, Region::Note)], false),
        ("both", vec![(UnitUnreadable, Region::Body), (PartUnreadable, Region::Note)], true),
    ];
    for (name, reported, expected) in cases {
        let mut diagnostics = Diagnostics::default();
        for (code, region) in reported {
            diagnostics.add(code, region, None, 1);
        }
        let unread = Unread::of(&diagnostics);
        assert_eq!(unread.notes, expected, "{name}");
    }
}

#[test]
fn every_container_nests_to_the_limit_and_no_further() {
    let wraps: [fn(Block) -> Block; 4] = [
        |inner| bulleted(vec![inner]),
        |inner| frame(vec![inner]),
        |inner| Block::BlockQuote(vec![inner]),
        |inner| {
            let Block::Paragraph(content) = inner else { panic!("not a paragraph: {inner:?}") };
            let target = external("https://example.com");
            Block::Paragraph(vec![link(content, target)])
        },
    ];
    let refused = |doc: &Document| {
        matches!(
            check_nesting(doc, &GapFills::default(), 4),
            Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 4 })
        )
    };
    for (case, wrap) in wraps.iter().enumerate() {
        let nested = |depth| {
            let mut block = text("x");
            for _ in 0..depth {
                block = wrap(block);
            }
            block
        };
        let body = |depth| Document { blocks: vec![nested(depth)], ..Document::default() };
        let notes =
            |depth| Document { notes: vec![note("1", vec![nested(depth)])], ..Document::default() };
        assert!(check_nesting(&body(4), &GapFills::default(), 4).is_ok(), "{case}");
        assert!(check_nesting(&notes(4), &GapFills::default(), 4).is_ok(), "{case}");
        assert!(refused(&body(5)), "{case}");
        assert!(refused(&notes(5)), "{case}");
    }
}

#[test]
fn a_table_with_no_cell_holds_nothing_deeper_than_where_it_stands() {
    let in_lists = |count, inner| {
        let blocks = vec![(0..count).fold(inner, |block, _| bulleted(vec![block]))];
        Document { blocks, ..Document::default() }
    };
    let grid = |rows| Block::Table(Table { grid: rows, header_rows: 0 });
    let origin = || CellSlot::Origin(Cell::default());
    let too_deep = || Err(ConvertError::limit(Limit::NestingDepth, 4));
    let cases = [
        ("no row", grid(vec![]), Ok(())),
        ("an empty row", grid(vec![vec![]]), Ok(())),
        ("a covered position alone", grid(vec![vec![covered()]]), Ok(())),
        ("one cell", grid(vec![vec![origin()]]), too_deep()),
        (
            "a cell after a covered position and an empty row",
            grid(vec![vec![covered()], vec![], vec![origin()]]),
            too_deep(),
        ),
    ];
    for (name, table, expected) in cases {
        assert_eq!(check_nesting(&in_lists(4, table), &GapFills::default(), 4), expected, "{name}");
    }
    let deep_cell = Cell::new(vec![bulleted(vec![text("y")])]);
    let second_cell_too_deep = grid(vec![vec![origin(), CellSlot::Origin(deep_cell)]]);
    assert_eq!(
        check_nesting(&in_lists(3, second_cell_too_deep), &GapFills::default(), 4),
        too_deep()
    );
}

#[test]
fn notes_number_in_the_cell_order_of_a_table_whose_rows_differ_in_shape() {
    let reference = |id: &str| Inline::NoteRef(id.into());
    let notes = ["z", "y", "x"].map(|id| note(id, vec![text(&id.to_uppercase())])).to_vec();
    assert_eq!(
        with_notes(vec![uneven_table(reference("x"), reference("y"))], notes),
        "|  |  |\n| --- | --- |\n| [^1] |  |\n|  |  |\n|  | [^2] |\n\n[^1]: X\n\n[^2]: Y\n\n[^3]: Z\n"
    );
}

#[test]
fn notes_number_in_the_order_their_first_references_are_read() {
    let reference = |id: &str| Inline::NoteRef(id.into());
    let md = with_notes(
        vec![
            Block::Paragraph(vec![
                link(vec![reference("a")], external("https://example.com")),
                reference("d"),
            ]),
            Block::BlockQuote(vec![Block::Paragraph(vec![reference("c")])]),
        ],
        vec![
            note("d", vec![text("D")]),
            note("c", vec![text("C")]),
            note("b", vec![text("B")]),
            note("a", vec![Block::Paragraph(vec![Inline::plain("A"), reference("b")])]),
        ],
    );
    assert_eq!(
        md,
        "[[^1]](https://example.com)[^3]\n\n> [^4]\n\n[^1]: A[^2]\n\n[^2]: B\n\n[^3]: D\n\n[^4]: C\n"
    );
}

#[test]
fn nested_links_render_in_work_linear_in_their_depth() {
    let unresolved = LinkTarget::anchor("nowhere".into());
    let shapes = [
        ("docx unresolved", Inline::plain("x"), unresolved.clone(), "x\n"),
        ("epub unresolved bold", styled("x", BOLD), unresolved, "**x**\n"),
        (
            "external",
            Inline::plain("x"),
            external("https://example.com"),
            "[x](https://example.com)\n",
        ),
    ];
    for (name, leaf, target, expected) in shapes {
        let spent = |depth| {
            let mut content = vec![leaf.clone()];
            for _ in 0..depth {
                content = vec![link(content, target.clone())];
            }
            let doc = Document { blocks: vec![Block::Paragraph(content)], ..Document::default() };
            let ledger = Ledger::new(u64::MAX, None);
            let rendered =
                render(&doc, &GapFills::default(), &Meter::new(&ledger, u64::MAX), Placeholder)
                    .unwrap();
            assert_eq!(rendered.markdown, expected, "{name}");
            ledger.spent()
        };
        let (shallow, deep) = (spent(300), spent(600));
        assert!(shallow >= 300, "{name}: {shallow}");
        assert!(deep - shallow <= shallow + 10, "{name}: {shallow} then {deep}");
    }
}

#[test]
fn a_link_without_a_target_keeps_its_plain_content() {
    let untargeted = |content| link(content, LinkTarget::anchor(String::new()));
    let between = |inline| {
        Block::Paragraph(vec![Inline::plain("a"), untargeted(vec![inline]), Inline::plain("b")])
    };
    let md = doc(vec![
        between(Inline::plain(" ")),
        between(Inline::LineBreak),
        between(Inline::Anchor("t".into())),
        between(Inline::plain(" c ")),
        Block::Paragraph(vec![link(vec![Inline::plain("to t")], LinkTarget::anchor("t".into()))]),
    ]);
    assert_eq!(md, "a b\n\na\\\nb\n\na<a id=\"t\"></a>b\n\na c b\n\n[to t](#t)\n");
}

#[test]
fn a_refused_link_renders_its_label_alone() {
    for url in [" javascript:alert(1)", "JavaScript:x", "java\tscript:x", r"C:\x", r"\\srv\x"] {
        assert_eq!(linked("click", LinkTarget::Relative(url.into())), "click\n", "{url:?}");
    }
    assert_eq!(linked("x", external("data:text/html,x")), "x\n");
    assert_eq!(linked(" ", external("vbscript:x")), "");
    assert_eq!(linked("a", LinkTarget::Relative("../a.odt".into())), "[a](../a.odt)\n");
    let paired = paragraph(vec![
        Inline::plain("[click"),
        Inline::LineBreak,
        link(vec![Inline::plain("here]")], external("javascript:x")),
        Inline::plain("(https://e.test)"),
    ]);
    assert_eq!(paired, "\\[click\\\nhere](https://e.test)\n");
}

#[test]
fn a_destination_parses_back_to_its_exact_target() {
    for (url, written) in [
        (r"https://e.com/a\b", r"https://e.com/a\\b"),
        ("https://e.com/?a=1&copy;b", r"https://e.com/?a=1\&copy;b"),
        ("https://e.com/&#35;", r"https://e.com/\&#35;"),
        ("https://e.com/?a&b=1", "https://e.com/?a&b=1"),
    ] {
        assert_eq!(linked("x", external(url)), format!("[x]({written})\n"));
    }
}

fn parsed_destinations(markdown: &str) -> Vec<String> {
    Parser::new_ext(markdown, Options::ENABLE_TABLES)
        .filter_map(|event| match event {
            Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.into_string()),
            _ => None,
        })
        .collect()
}

proptest::proptest! {
    #[test]
    fn every_destination_a_parser_reads_back_is_the_target(
        path in r"[a-z0-9&#;\\|()<> %.:/?=_~-]{0,24}",
    ) {
        let url = format!("https://e.com/{path}");
        let to_url = || link(vec![Inline::plain("x")], external(&url));
        let expected = url.replace('<', "%3C").replace('>', "%3E").replace('|', "%7C");
        let body = paragraph(vec![to_url()]);
        proptest::prop_assert_eq!(parsed_destinations(&body), [expected.clone()]);
        let table = doc(vec![table_from(vec![vec![Cell::from_inlines(vec![to_url()])]], 1)]);
        proptest::prop_assert_eq!(parsed_destinations(&table), [expected]);
    }
}

#[test]
fn a_shift_wraps_its_run_outside_every_other_markup_with_its_edge_spaces_outside() {
    let plain = Inline::plain;
    assert_eq!(paragraph(vec![plain("10"), styled("-3", SUP), plain(" M")]), "10<sup>-3</sup> M\n");
    assert_eq!(paragraph(vec![plain("H"), styled(" 2 ", SUB), plain("O")]), "H <sub>2</sub> O\n");
    let all = Style { bold: true, italic: true, strike: true, ..SUP };
    assert_eq!(paragraph(vec![plain("x"), styled("n", all)]), "x<sup>~~**_n_**~~</sup>\n");
    let code = Style { code: true, bold: true, ..SUB };
    assert_eq!(paragraph(vec![plain("x"), styled("i`j", code)]), "x<sub>**``i`j``**</sub>\n");
    assert_eq!(
        paragraph(vec![plain("x"), styled("a", SUP), styled("b", SUB), styled("c", BOLD)]),
        "x<sup>a</sup><sub>b</sub>**c**\n"
    );
    assert_eq!(paragraph(vec![styled("a", SUP), plain(" "), styled("b", SUP)]), "<sup>a b</sup>\n");
    assert_eq!(paragraph(vec![plain("a"), styled(" ", SUP), plain("b")]), "a b\n");
}

#[test]
fn a_shifted_run_escapes_its_content_as_styled_text() {
    let md = paragraph(vec![Inline::plain("a"), styled("*b_<i>~`&amp;\\", SUP)]);
    assert_eq!(md, "a<sup>\\*b\\_\\<i>\\~\\`&amp;amp;\\\\</sup>\n");
    let heading = doc(vec![Block::heading(2, vec![Inline::plain("E = mc"), styled("2", SUP)])]);
    assert_eq!(heading, "## E = mc<sup>2</sup>\n");
    let labelled = paragraph(vec![link(
        vec![Inline::plain("x"), styled("[2]", SUP)],
        external("https://e.test"),
    )]);
    assert_eq!(labelled, "[x<sup>\\[2\\]</sup>](https://e.test)\n");
    let shifted = Cell::from_inlines(vec![styled("a|b", SUB)]);
    assert_eq!(doc(vec![table_from(vec![vec![shifted]], 1)]), "| <sub>a\\|b</sub> |\n| --- |\n");
}

fn marks_of(runs: &[Inline]) -> Vec<(char, Style)> {
    runs.iter()
        .filter_map(|run| match run {
            Inline::Text { text, style } => Some((text, *style)),
            _ => None,
        })
        .flat_map(|(text, style)| {
            text.chars().filter(|c| !c.is_whitespace()).map(move |c| (c, style))
        })
        .collect()
}

fn parsed_text(markdown: &str) -> String {
    Parser::new(markdown)
        .filter_map(|event| match event {
            Event::Text(text) => Some(text.into_string()),
            Event::SoftBreak => Some(" ".into()),
            _ => None,
        })
        .collect()
}

#[derive(Default)]
struct Open {
    bold: i32,
    italic: i32,
    strike: i32,
}

impl Open {
    fn track(&mut self, event: &Event<'_>) -> bool {
        match event {
            Event::Start(Tag::Strong) => self.bold += 1,
            Event::End(TagEnd::Strong) => self.bold -= 1,
            Event::Start(Tag::Emphasis) => self.italic += 1,
            Event::End(TagEnd::Emphasis) => self.italic -= 1,
            Event::Start(Tag::Strikethrough) => self.strike += 1,
            Event::End(TagEnd::Strikethrough) => self.strike -= 1,
            _ => return false,
        }
        true
    }

    fn style(&self) -> Style {
        Style {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            ..Style::PLAIN
        }
    }
}

/// The text a parser reads from `markdown` with whitespace runs collapsed, and the style it
/// reads on each other character.
fn read_back(markdown: &str) -> (String, Vec<(char, Style)>) {
    use std::fmt::Write as _;
    let mut text = String::new();
    let mut marked = Vec::new();
    let mut open = Open::default();
    let mut shifts: Vec<(&str, VerticalPosition)> = Vec::new();
    for event in Parser::new_ext(markdown, Options::ENABLE_STRIKETHROUGH) {
        if open.track(&event) {
            continue;
        }
        match event {
            Event::Start(Tag::Paragraph) | Event::End(TagEnd::Paragraph) => {}
            Event::InlineHtml(html) if html.as_ref() == "<sup>" => {
                shifts.push(("</sup>", VerticalPosition::Superscript));
            }
            Event::InlineHtml(html) if html.as_ref() == "<sub>" => {
                shifts.push(("</sub>", VerticalPosition::Subscript));
            }
            Event::InlineHtml(html)
                if shifts.last().is_some_and(|(close, _)| *close == html.as_ref()) =>
            {
                shifts.pop();
            }
            Event::Text(read) => {
                let position = shifts.last().map_or(VerticalPosition::Baseline, |&(_, p)| p);
                let style = Style { position, ..open.style() };
                text.push_str(&read);
                marked.extend(read.chars().filter(|c| !c.is_whitespace()).map(|c| (c, style)));
            }
            Event::SoftBreak => text.push('\n'),
            other => _ = write!(text, "{other:?}"),
        }
    }
    (collapse_ws(text.trim()), marked)
}

#[test]
fn shifted_open_brackets_cannot_form_links_across_style_boundaries() {
    for position in [VerticalPosition::Superscript, VerticalPosition::Subscript] {
        let shifted = Style { position, ..Style::PLAIN };
        let markdown =
            paragraph(vec![styled("[", shifted), Inline::plain("]("), Inline::plain(")")]);
        let (text, marked) = read_back(&markdown);
        assert_eq!(text, "[]()");
        assert_eq!(
            marked,
            [('[', shifted), (']', Style::PLAIN), ('(', Style::PLAIN), (')', Style::PLAIN)]
        );
    }
}

proptest::proptest! {
    #[test]
    fn styled_and_shifted_runs_parse_back_to_their_text_emphasis_and_tags(
        runs in proptest::collection::vec(
            (r"[a-c*_~`<>&;#!\[\]()\\ .-]{1,4}( |\n)?[a-c*_~`<>&#\\.-]{0,3}", 0u8..8, 0u8..3),
            1..6,
        ),
    ) {
        let inlines: Vec<Inline> = runs
            .iter()
            .map(|(text, bits, position)| {
                let position = match position {
                    0 => VerticalPosition::Baseline,
                    1 => VerticalPosition::Superscript,
                    _ => VerticalPosition::Subscript,
                };
                styled(text, Style { position, ..emphasis(*bits) })
            })
            .collect();
        let source: String = runs.iter().map(|(text, ..)| text.as_str()).collect();
        let shows = |line: &str| !line.trim().is_empty();
        proptest::prop_assume!(source.lines().all(shows));
        let expected = (collapse_ws(source.trim()), marks_of(&inlines));
        let markdown = paragraph(inlines);
        proptest::prop_assert_eq!(read_back(&markdown), expected, "{:?}", markdown);
    }
}

#[test]
fn unit_targets_have_separate_keys_and_empty_units_own_no_rendered_bytes() {
    let slide = |n| unit(UnitKind::Slide, n);
    let go = |target| link(vec![Inline::plain("go")], target);
    let document = Document {
        blocks: vec![
            Block::UnitStart(slide(1)),
            Block::Paragraph(vec![
                go(LinkTarget::Unit(slide(2))),
                go(LinkTarget::Unit(slide(3))),
                go(LinkTarget::anchor("slide-3".into())),
            ]),
            Block::UnitStart(slide(2)),
            Block::UnitStart(slide(3)),
            heading(1, "Slide 3"),
            Block::Paragraph(vec![Inline::Anchor("slide-3".into()), Inline::plain("İstanbul 🦀")]),
        ],
        notes: vec![note("n", vec![text("note body")])],
        ..Document::default()
    };
    let rendered = render_at(&document, u64::MAX, u64::MAX, Placeholder).unwrap();
    assert_eq!(rendered.units.len(), 3);
    assert_eq!(rendered.units[1].bytes.start, rendered.units[1].bytes.end);
    assert!(rendered.markdown.contains("[go](#slide-2)"));
    assert!(rendered.markdown.contains("[go](#slide-3-1)"));
    assert!(rendered.markdown.contains("[go](#slide-3-2)"));
    assert_eq!(rendered.markdown.matches("---").count(), 1);
    let third = &rendered.markdown.get(rendered.units[2].bytes.clone()).unwrap();
    assert!(third.starts_with("# Slide 3"));
    assert!(!third.contains("slide-3-2") && !third.contains("note body"));
    assert!(rendered.markdown.contains("<a id=\"slide-2\"></a>"));
    assert!(rendered.markdown.contains("<a id=\"slide-3-2\"></a>"));
}

#[test]
fn escaped_delimiter_runs_cannot_gain_a_closer_across_a_newline() {
    for source in [">*[\n**", "a* [\n***", "a_ [\n___"] {
        let markdown = doc(vec![text(source)]);
        assert_eq!(parsed_text(&markdown), source.replace('\n', " "), "{markdown}");
    }
}

/// What a GFM parser reads in `markdown`: every list, nested ones included,
/// in the order it opens, as its start (`None` when unordered) and item
/// count; the labels of the note references; how many note definitions and
/// links it holds.
#[derive(Debug, Default, PartialEq)]
struct Parsed {
    lists: Vec<(Option<u64>, usize)>,
    note_refs: Vec<String>,
    definitions: usize,
    links: usize,
}

fn parsed(markdown: &str) -> Parsed {
    let mut found = Parsed::default();
    let mut open: Vec<usize> = Vec::new();
    for event in Parser::new_ext(markdown, Options::ENABLE_FOOTNOTES | Options::ENABLE_TABLES) {
        match event {
            Event::Start(Tag::List(start)) => {
                open.push(found.lists.len());
                found.lists.push((start, 0));
            }
            Event::End(TagEnd::List(_)) => {
                open.pop();
            }
            Event::Start(Tag::Item) => {
                if let Some(&list) = open.last() {
                    found.lists[list].1 += 1;
                }
            }
            Event::FootnoteReference(label) => found.note_refs.push(label.into_string()),
            Event::Start(Tag::FootnoteDefinition(_)) => found.definitions += 1,
            Event::Start(Tag::Link { .. }) => found.links += 1,
            _ => {}
        }
    }
    found
}

#[test]
fn a_decimal_list_renders_ordered_when_every_label_is_its_own_ordinal() {
    use MarkerKind::{Bullet, Decimal, LowerRoman};
    let max = u64::MAX;
    for (list, expected, start) in [
        (list_of(Decimal, 1, &[None, None]), "1. a\n2. b\n", Some(1)),
        (list_of(Decimal, 1, &[Some("1"), Some("2")]), "1. a\n2. b\n", Some(1)),
        (list_of(Decimal, 4, &[Some("4)"), Some("5)")]), "4) a\n5) b\n", Some(4)),
        (list_of(Decimal, 1, &[None, Some("2)")]), "1. a\n2. b\n", Some(1)),
        (list_of(Decimal, 1, &[Some("1"), Some("2)")]), "1. a\n2. b\n", Some(1)),
        (list_of(Decimal, 1, &[Some("1-a)"), Some("2")]), "- 1-a) a\n- 2 b\n", None),
        (list_of(Decimal, 1, &[Some("3."), None]), "- 3\\. a\n- 2\\. b\n", None),
        (list_of(Decimal, 1, &[Some("2"), Some("1")]), "- 2 a\n- 1 b\n", None),
        (list_of(Decimal, 1, &[Some("1)."), None]), "- 1). a\n- 2\\. b\n", None),
        (list_of(LowerRoman, 3, &[None, Some("iv)")]), "- iii. a\n- iv) b\n", None),
        (list_of(Bullet, 1, &[None, Some("1")]), "- a\n- 1 b\n", None),
        (list_of(Decimal, 0, &[None, None]), "0. a\n1. b\n", Some(0)),
        (
            list_of(Decimal, 999_999_998, &[None, None]),
            "999999998. a\n999999999. b\n",
            Some(999_999_998),
        ),
        (
            list_of(Decimal, 999_999_999, &[None, None]),
            "- 999999999\\. a\n- 1000000000\\. b\n",
            None,
        ),
        (
            list_of(Decimal, max, &[None, None]),
            "- 18446744073709551615\\. a\n- 18446744073709551615\\. b\n",
            None,
        ),
    ] {
        let markdown = doc(vec![list]);
        assert_eq!(markdown, expected);
        assert_eq!(parsed(&markdown).lists, [(start, 2)], "{markdown}");
    }
}

#[test]
fn a_list_after_a_sibling_list_of_its_kind_takes_the_other_mark() {
    use MarkerKind::{Bullet, Decimal, LowerRoman};
    let silent = text("  ");
    for (blocks, expected) in [
        (
            vec![list_of(Decimal, 1, &[None, None]), list_of(Decimal, 10, &[None])],
            "1. a\n2. b\n\n10) a\n",
        ),
        (
            vec![
                list_of(Decimal, 1, &[None]),
                list_of(Decimal, 5, &[Some("5)")]),
                list_of(Decimal, 9, &[Some("9)")]),
            ],
            "1. a\n\n5) a\n\n9. a\n",
        ),
        (
            vec![list_of(LowerRoman, 1, &[None]), list_of(Bullet, 1, &[None, None])],
            "- i. a\n\n* a\n* b\n",
        ),
        (vec![list_of(Bullet, 1, &[None]), silent, list_of(Bullet, 1, &[None])], "- a\n\n* a\n"),
        (
            vec![list_of(Bullet, 1, &[None]), text("x"), list_of(Bullet, 1, &[None])],
            "- a\n\nx\n\n- a\n",
        ),
        (
            vec![
                list_of(Bullet, 1, &[None]),
                Block::BlockQuote(vec![list_of(Bullet, 1, &[None])]),
                list_of(Bullet, 1, &[None]),
            ],
            "- a\n\n> - a\n\n- a\n",
        ),
        (
            vec![
                bulleted(vec![list_of(Bullet, 1, &[None]), list_of(Bullet, 1, &[None])]),
                list_of(Bullet, 1, &[None]),
            ],
            "- - a\n\n  * a\n\n* a\n",
        ),
        (
            vec![list_of(LowerRoman, 1, &[None]), list_of(Bullet, 1, &[Some("*")])],
            "- i. a\n\n* \\* a\n",
        ),
    ] {
        let count = blocks.iter().filter(|b| matches!(b, Block::List(_))).count();
        let markdown = doc(blocks);
        assert_eq!(markdown, expected);
        let lists = parsed(&markdown).lists;
        assert!(lists.len() >= count, "{markdown}: {lists:?}");
    }
}

#[test]
fn the_mark_carries_across_units_until_a_break_or_an_anchor_is_written() {
    let start = |kind, n| Block::UnitStart(unit(kind, n));
    let bullets = || list_of(MarkerKind::Bullet, 1, &[None]);
    let spine = doc(vec![
        bullets(),
        start(UnitKind::SpineItem, 1),
        bullets(),
        start(UnitKind::SpineItem, 2),
        bullets(),
    ]);
    assert_eq!(spine, "- a\n\n* a\n\n- a\n");
    assert_eq!(parsed(&spine).lists.len(), 3);
    let slides =
        doc(vec![start(UnitKind::Slide, 1), bullets(), start(UnitKind::Slide, 2), bullets()]);
    assert_eq!(slides, "- a\n\n---\n\n- a\n");
    let linked = doc(vec![
        Block::Paragraph(vec![link(
            vec![Inline::plain("go")],
            LinkTarget::Unit(unit(UnitKind::SpineItem, 1)),
        )]),
        bullets(),
        start(UnitKind::SpineItem, 1),
        bullets(),
    ]);
    assert!(linked.ends_with("</a>\n\n- a\n"), "{linked}");
}

fn two_by_one() -> Block {
    table_from(vec![vec![cell("a"), cell("b")]], 0)
}

const TWO_BY_ONE: &str = "|  |  |\n| --- | --- |\n| a | b |\n";

#[test]
fn single_cell_wrapper_around_nested_table_unwrapped() {
    let empty = || text(" ");
    for (name, block) in [
        ("one cell", frame(vec![two_by_one()])),
        ("empty paragraphs aside", frame(vec![empty(), two_by_one(), empty()])),
        ("a leading empty row", table_from(vec![vec![], vec![Cell::new(vec![two_by_one()])]], 0)),
    ] {
        assert_eq!(doc(vec![block]), TWO_BY_ONE, "{name}");
    }
    let parse_ctx = ParseCtx::default();
    let mut spanned = GridBuilder::new(&parse_ctx);
    spanned.next_row();
    spanned.place(Cell { col_span: 2, ..Cell::new(vec![two_by_one()]) }).unwrap();
    let spanned = spanned.finish().unwrap();
    assert_eq!(spanned.grid[0].len(), 2);
    assert_eq!(doc(vec![Block::Table(spanned)]), TWO_BY_ONE);
}

#[test]
fn a_frame_showing_anything_beside_its_table_keeps_its_cell() {
    let anchor = |id: &str| Block::Paragraph(vec![Inline::Anchor(id.into())]);
    let to =
        Block::Paragraph(vec![link(vec![Inline::plain("to")], LinkTarget::anchor("t".into()))]);
    let labelled = frame(vec![text("Section A"), two_by_one(), text("End of section")]);
    assert_eq!(doc(vec![labelled]), "|  |\n| --- |\n| Section A<br>a / b<br>End of section |\n");
    assert_eq!(
        doc(vec![to, frame(vec![anchor("t"), two_by_one()])]),
        "[to](#t)\n\n|  |\n| --- |\n| <a id=\"t\"></a><br>a / b |\n"
    );
    assert_eq!(doc(vec![frame(vec![anchor("unlinked"), two_by_one()])]), TWO_BY_ONE);
    assert_eq!(
        doc(vec![frame(vec![two_by_one(), two_by_one()])]),
        "|  |\n| --- |\n| a / b<br>a / b |\n"
    );
    assert_eq!(
        doc(vec![frame(vec![list_of(MarkerKind::Bullet, 1, &[None])])]),
        "|  |\n| --- |\n| • a |\n"
    );
    let wider = table_from(vec![vec![Cell::new(vec![two_by_one()]), cell("right")]], 0);
    assert_eq!(doc(vec![wider]), "|  |  |\n| --- | --- |\n| a / b | right |\n");
}

#[test]
fn wrappers_unwrap_recursively() {
    let mid = frame(vec![two_by_one()]);
    let md = doc(vec![frame(vec![mid])]);
    assert_eq!(md, TWO_BY_ONE);
}

#[test]
fn a_wrapper_chain_at_the_nesting_limit_renders_unwrapped() {
    let mut block = two_by_one();
    for _ in 1..4 {
        block = frame(vec![block]);
    }
    let document = Document { blocks: vec![block], ..Document::default() };
    check_nesting(&document, &GapFills::default(), 4).unwrap();
    assert_eq!(document_to_markdown(&document), TWO_BY_ONE);
}

fn render_with_one_note(blocks: Vec<Block>, images: ImagePolicy) -> String {
    let document =
        Document { blocks, notes: vec![note("n", vec![text("N")])], ..Document::default() };
    rendered(&document, images)
}

fn assert_note_reference(name: &str, block: Block, images: ImagePolicy, expected: &str) {
    let markdown = render_with_one_note(vec![block], images);
    assert_eq!(markdown, format!("{expected}\n\n[^1]: N\n"), "{name}");
    let found = parsed(&markdown);
    assert_eq!(found.note_refs, ["1"], "{name}: {markdown}");
    assert_eq!((found.definitions, found.links), (1, 0), "{name}: {markdown}");
}

#[test]
#[expect(clippy::too_many_lines, reason = "the tables of what follows a note reference")]
fn text_right_after_a_note_reference_cannot_turn_it_into_syntax() {
    use Inline::LineBreak;
    let (plain, para) = (Inline::plain, Block::Paragraph);
    let reference = || Inline::NoteRef("n".into());
    let unsafe_link = |t: &str| link(vec![Inline::plain(t)], external("file:///x"));
    let framed = |inlines| frame(vec![para(inlines)]);
    let see = || vec![plain("See"), reference(), plain("(2020)")];
    let colon = || vec![reference(), plain(": x")];
    for (name, block, expected) in [
        ("paragraph", para(see()), "See[^1]\\(2020)"),
        ("heading", Block::heading(1, see()), "# See[^1]\\(2020)"),
        ("cell", framed(see()), "|  |\n| --- |\n| See[^1]\\(2020) |"),
        (
            "unsafe image source",
            para(vec![
                plain("a"),
                reference(),
                image("(2020)", ImageSource::External("vbscript:x".into())),
            ]),
            "a[^1]\\(2020)",
        ),
        ("degraded link", para(vec![reference(), unsafe_link("(2020)")]), "[^1]\\(2020)"),
        ("line start", para(colon()), "[^1]\\: x"),
        (
            "after a hard break",
            para(vec![plain("a"), LineBreak, reference(), plain(": x")]),
            "a\\\n[^1]\\: x",
        ),
        ("after a soft break", para(vec![plain("a\n"), reference(), plain(": x")]), "a\n[^1]\\: x"),
        (
            "after a carriage return",
            para(vec![plain("a\r"), reference(), plain(": x")]),
            "a\r[^1]\\: x",
        ),
        (
            "after a line break in a cell",
            framed(vec![reference(), LineBreak, plain("(x)")]),
            "|  |\n| --- |\n| [^1]<br>(x) |",
        ),
        ("after whitespace", para(vec![plain("  "), reference(), plain(": x")]), "[^1]\\: x"),
        ("in an item", bulleted(vec![para(colon())]), "- [^1]\\: x"),
        ("in a heading", Block::heading(1, colon()), "# [^1]: x"),
        ("degraded link at line start", para(vec![reference(), unsafe_link(": x")]), "[^1]\\: x"),
        (
            "silent anchor between",
            para(vec![plain("a"), reference(), Inline::Anchor("unlinked".into()), plain("(x)")]),
            "a[^1]\\(x)",
        ),
        ("space between", para(vec![reference(), plain(" (x)")]), "[^1] (x)"),
        ("mid-line colon", para(vec![plain("a "), reference(), plain(": b")]), "a [^1]: b"),
        ("colon in a cell", framed(vec![reference(), plain(": b")]), "|  |\n| --- |\n| [^1]: b |"),
        (
            "styled parenthesis",
            para(vec![plain("a"), reference(), styled("(x)", BOLD)]),
            "a[^1]**(x)**",
        ),
        (
            "parenthesis later in the run",
            para(vec![plain("a"), reference(), plain("x(y)")]),
            "a[^1]x(y)",
        ),
    ] {
        assert_note_reference(name, block, Placeholder, expected);
    }
    for (name, label, expected) in [
        ("image label", "(2020)", "[^1]\\(2020)"),
        ("image label at line start", ": x", "[^1]\\: x"),
    ] {
        let block = para(vec![reference(), image(label, ImageSource::Unavailable)]);
        assert_note_reference(name, block, AltText, expected);
    }
    let unnumbered = paragraph(vec![plain("a"), Inline::NoteRef("missing".into()), plain("(x)")]);
    assert_eq!(unnumbered, "a(x)\n");
    let to_anchor = render_with_one_note(
        vec![
            para(vec![link(vec![plain("to")], LinkTarget::anchor("t".into()))]),
            para(vec![plain("a"), reference(), Inline::Anchor("t".into()), plain("(x)")]),
        ],
        Placeholder,
    );
    assert_eq!(to_anchor, "[to](#t)\n\na[^1]<a id=\"t\"></a>(x)\n\n[^1]: N\n");
}

#[test]
fn note_references_on_one_line_render_in_linear_time() {
    let count = 100_000;
    let inlines = vec![Inline::NoteRef("n".into()); count];
    let started = std::time::Instant::now();
    let markdown = render_with_one_note(vec![Block::Paragraph(inlines)], Placeholder);
    let elapsed = started.elapsed();
    assert_eq!(markdown, format!("{}\n\n[^1]: N\n", "[^1]".repeat(count)));
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
}
