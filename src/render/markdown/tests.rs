use super::{
    Ctx, RenderLoss, Unread, check_nesting, document_to_markdown, render_block, render_losses,
};
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Diagnostics, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{
    AnchorId, Block, Cell, Document, GridBuilder, ImageSource, Inline, LinkTarget, List, ListItem,
    MarkerKind, Note, NoteKind, Style, Table, TableKind,
};
use std::assert_matches;

fn doc(blocks: Vec<Block>) -> String {
    document_to_markdown(&Document { blocks, notes: Vec::new(), assets: Vec::new() })
}

fn styled(text: &str, style: Style) -> Inline {
    Inline::Text { text: text.into(), style }
}

fn heading(level: u8, text: &str) -> Block {
    Block::heading(level, vec![Inline::plain(text)])
}

fn table_from(rows: Vec<Vec<Cell>>, header_rows: usize) -> Block {
    Block::Table(Table::from_rows(rows, header_rows, TableKind::Data))
}

const BOLD: Style = Style { bold: true, italic: false, strike: false, code: false };
const ITALIC: Style = Style { bold: false, italic: true, strike: false, code: false };

#[test]
fn heading_and_paragraph() {
    let md = doc(vec![heading(2, "Title"), Block::Paragraph(vec![Inline::plain("Hello world.")])]);
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
        Block::Paragraph(vec![Inline::plain("Plans at $20 or $17.50.")]),
        Block::Paragraph(vec![Inline::plain("Pair $x with y$ here.")]),
    ]);
    assert_eq!(
        md,
        "Costs \\$5 or \\$6, and $x_1 < y$ holds.\n\n$$\n\\sum_{i=1}^{n} i \\$\n$$\n\n\
         Plans at $20 or $17.50.\n\nPair \\$x with y$ here.\n"
    );
}

#[test]
fn math_in_a_table_cell_escapes_pipes() {
    let cell = |inlines| Cell { blocks: vec![Block::Paragraph(inlines)], col_span: 1, row_span: 1 };
    let md = doc(vec![table_from(
        vec![vec![cell(vec![Inline::plain("abs")]), cell(vec![Inline::Math("|x|".into())])]],
        0,
    )]);
    assert!(md.contains("| $\\|x\\|$ |"), "{md}");
}

#[test]
fn paragraph_math_keeps_pipes_and_an_escaped_dollar() {
    let md =
        doc(vec![Block::Paragraph(vec![Inline::plain("a "), Inline::Math("|x| a\\$b".into())])]);
    assert_eq!(md, "a $|x| a\\$b$\n");
}

#[test]
fn a_styled_dollar_closes_a_dollar_before_it() {
    let md = doc(vec![Block::Paragraph(vec![
        Inline::plain("cost $x"),
        Inline::LineBreak,
        styled("y$", BOLD),
        Inline::plain(" z"),
    ])]);
    assert_eq!(md, "cost \\$x\\\n**y$** z\n");
}

#[test]
fn escapes_paired_syntax_chars() {
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("a *bold* _it_ ~st~ `code`")])]);
    assert_eq!(md, "a \\*bold* \\_it_ \\~st~ \\`code`\n");
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("see [really] and <b>hi</b>")])]);
    assert_eq!(md, "see \\[really] and \\<b>hi\\</b>\n");
}

#[test]
fn lone_syntax_chars_left_alone() {
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("2 * 3 = 6 and 5*6 #tag")])]);
    assert_eq!(md, "2 * 3 = 6 and 5*6 #tag\n");
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("x < 5, ~10%, file_name, a[1")])]);
    assert_eq!(md, "x < 5, ~10%, file_name, a[1\n");
}

#[test]
fn partners_that_cannot_close_leave_delimiters_raw() {
    // The space-padded `*` is not right-flanking, so the opener is inert.
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("a *b 2 * 3")])]);
    assert_eq!(md, "a *b 2 * 3\n");
    // Same across a run seam.
    let md = doc(vec![Block::Paragraph(vec![
        Inline::plain("a *"),
        Inline::LineBreak,
        Inline::plain("2 * 3"),
    ])]);
    assert_eq!(md, "a *\\\n2 * 3\n");
    // Intraword underscores can neither open nor close.
    let md = doc(vec![Block::Paragraph(vec![
        Inline::plain("a _"),
        Inline::LineBreak,
        Inline::plain("snake_case"),
    ])]);
    assert_eq!(md, "a _\\\nsnake_case\n");
    // A `*` after punctuation and before a word character is not
    // right-flanking either.
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("a *b .*c")])]);
    assert_eq!(md, "a *b .*c\n");
    // Flanking is judged at the delimiter run's edges: `__` between
    // letters is intraword even though each `_` neighbours the other.
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("a _x foo__bar")])]);
    assert_eq!(md, "a _x foo__bar\n");
}

#[test]
fn intraword_underscores_unescaped() {
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("snake_case_name vs _lead_")])]);
    assert_eq!(md, "snake_case_name vs \\_lead_\n");
}

#[test]
fn escapes_line_start_only_chars() {
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("- not a list")])]);
    assert_eq!(md, "\\- not a list\n");
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("1. not a list")])]);
    assert_eq!(md, "1\\. not a list\n");
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("take 2. then rest")])]);
    assert_eq!(md, "take 2. then rest\n");
}

#[test]
fn line_start_lookalikes_unescaped() {
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("-5°C at dawn")])]);
    assert_eq!(md, "-5°C at dawn\n");
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("1.5 million users")])]);
    assert_eq!(md, "1.5 million users\n");
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("#hashtag first")])]);
    assert_eq!(md, "#hashtag first\n");
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("--- ruled")])]);
    assert_eq!(md, "--- ruled\n");
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("---")])]);
    assert_eq!(md, "\\---\n");
}

#[test]
fn negative_number_in_cell_unescaped() {
    let md = doc(vec![table_from(
        vec![vec![
            Cell::from_inlines(vec![Inline::plain("-42")]),
            Cell::from_inlines(vec![Inline::plain("x")]),
        ]],
        0,
    )]);
    assert_eq!(md, "|  |  |\n| --- | --- |\n| -42 | x |\n");
}

#[test]
fn trailing_delimiter_before_styled_run_escaped() {
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("star*"), styled("x", BOLD)])]);
    assert_eq!(md, "star\\***x**\n");
}

#[test]
fn delimiters_do_not_pair_across_run_seams() {
    // Two lines each ending in a backtick must not form a code span (#45).
    let md = doc(vec![Block::Paragraph(vec![
        Inline::plain("a `"),
        Inline::LineBreak,
        Inline::plain("b `"),
    ])]);
    assert_eq!(md, "a \\`\\\nb `\n");
    // Emphasis pairs across a hard break just as code spans do.
    let md = doc(vec![Block::Paragraph(vec![
        Inline::plain("a *"),
        Inline::LineBreak,
        Inline::plain("b*"),
    ])]);
    assert_eq!(md, "a \\*\\\nb*\n");
    // A raw backtick pairs with a later code span's fence.
    let md = doc(vec![Block::Paragraph(vec![
        Inline::plain("a `"),
        Inline::LineBreak,
        styled("x", Style { code: true, ..Style::PLAIN }),
    ])]);
    assert_eq!(md, "a \\`\\\n`x`\n");
}

#[test]
fn unresolved_link_fallback_counts_toward_seam_pairing() {
    // The fallback text supplies the `]` that pairs with the earlier `[`.
    let md = doc(vec![Block::Paragraph(vec![
        Inline::plain("[click"),
        Inline::LineBreak,
        Inline::Link {
            content: vec![Inline::plain("here]")],
            target: LinkTarget::Anchor("nowhere".into()),
        },
        Inline::plain("(https://e.test)"),
    ])]);
    assert_eq!(md, "\\[click\\\nhere](https://e.test)\n");
    // Same seam with an emphasis delimiter in the fallback.
    let md = doc(vec![Block::Paragraph(vec![
        Inline::plain("a *"),
        Inline::LineBreak,
        Inline::Link {
            content: vec![Inline::plain("b*")],
            target: LinkTarget::Anchor("nowhere".into()),
        },
    ])]);
    assert_eq!(md, "a \\*\\\nb*\n");
}

#[test]
fn escaped_backtick_in_later_run_still_pairs() {
    // A styled run's backtick is emitted as `\\``, yet still closes a span
    // an earlier raw backtick opens: code spans ignore backslash escapes.
    let md = doc(vec![Block::Paragraph(vec![
        Inline::plain("a `"),
        Inline::LineBreak,
        styled("x`y", BOLD),
    ])]);
    assert_eq!(md, "a \\`\\\n**x\\`y**\n");
    // A whitespace-only code run loses its styling and emits no fence.
    let md = doc(vec![Block::Paragraph(vec![
        Inline::plain("a `"),
        Inline::LineBreak,
        Inline::Link {
            content: vec![styled("  ", Style { code: true, ..Style::PLAIN }), Inline::plain("x")],
            target: LinkTarget::External("https://e.test".into()),
        },
    ])]);
    assert_eq!(md, "a `\\\n[  x](https://e.test)\n");
}

#[test]
fn bold_trailing_space_moved_out() {
    let md = doc(vec![Block::Paragraph(vec![styled("bold ", BOLD), Inline::plain("plain")])]);
    assert_eq!(md, "**bold** plain\n");
}

#[test]
fn adjacent_same_style_runs_merged() {
    let md = doc(vec![Block::Paragraph(vec![styled("bo", BOLD), styled("ld", BOLD)])]);
    assert_eq!(md, "**bold**\n");
}

#[test]
fn bold_italic_combo() {
    let md = doc(vec![Block::Paragraph(vec![styled(
        "both",
        Style { bold: true, italic: true, strike: false, code: false },
    )])]);
    assert_eq!(md, "***both***\n");
}

#[test]
fn whitespace_only_run_loses_styling() {
    let md = doc(vec![Block::Paragraph(vec![
        styled("a", BOLD),
        styled(" ", ITALIC),
        styled("b", BOLD),
    ])]);
    assert_eq!(md, "**a b**\n");
}

#[test]
fn link_render() {
    let md = doc(vec![Block::Paragraph(vec![Inline::Link {
        content: vec![Inline::plain("site")],
        target: LinkTarget::External("https://example.com/a(b)".into()),
    }])]);
    assert_eq!(md, "[site](<https://example.com/a(b)>)\n");
}

#[test]
fn relative_links_preserved() {
    let md = doc(vec![Block::Paragraph(vec![Inline::Link {
        content: vec![Inline::plain("next")],
        target: LinkTarget::Relative("chapter2.xhtml".into()),
    }])]);
    assert_eq!(md, "[next](chapter2.xhtml)\n");
    let md = doc(vec![Block::Paragraph(vec![Inline::Link {
        content: vec![Inline::plain("mail")],
        target: LinkTarget::External("mailto:a@b.c".into()),
    }])]);
    assert_eq!(md, "[mail](mailto:a@b.c)\n");
}

#[test]
fn unresolved_anchor_degrades_to_text() {
    let md = doc(vec![Block::Paragraph(vec![Inline::Link {
        content: vec![Inline::plain("note")],
        target: LinkTarget::Anchor("nowhere".into()),
    }])]);
    assert_eq!(md, "note\n");
}

#[test]
fn sourceless_image_renders_alt_text() {
    let md = doc(vec![Block::Paragraph(vec![Inline::Image {
        alt: "chart".into(),
        source: ImageSource::Unavailable,
    }])]);
    assert_eq!(md, "chart\n");
}

#[test]
fn composite_marker_labels_are_escaped() {
    // Source-derived labels must not alter Markdown structure.
    let list = crate::model::List {
        marker: crate::model::MarkerKind::Decimal,
        start: 1,
        items: vec![crate::model::ListItem {
            blocks: vec![Block::Paragraph(vec![Inline::plain("x")])],
            marker_label: Some("#1\n*a*".into()),
        }],
    };
    let md = doc(vec![Block::List(list)]);
    assert_eq!(md.lines().count(), 1, "control characters must not split the marker");
    assert!(!md.contains("*a*"), "emphasis in a label must be escaped: {md}");
    assert!(md.starts_with("- "), "labelled items render as literal markers: {md}");
}

#[test]
fn layout_table_unwrapped() {
    let parse_ctx = ParseCtx::default();
    let mut b = GridBuilder::new(&parse_ctx);
    b.next_row();
    b.place(Cell::new(vec![heading(1, "Boxed"), Block::Paragraph(vec![Inline::plain("body")])]))
        .unwrap();
    let md = doc(vec![Block::Table(b.finish(TableKind::Layout).unwrap())]);
    assert_eq!(md, "# Boxed\n\nbody\n");
}

#[test]
fn data_table_1x1_not_unwrapped() {
    let md = doc(vec![table_from(vec![vec![Cell::from_inlines(vec![Inline::plain("x")])]], 0)]);
    assert_eq!(md, "|  |\n| --- |\n| x |\n");
}

#[test]
fn trailing_empty_rows_and_columns_trimmed() {
    let empty = Cell::default;
    let filled = |s: &str| Cell::from_inlines(vec![Inline::plain(s)]);
    let md = doc(vec![table_from(
        vec![
            vec![filled("a"), filled("b"), empty()],
            vec![filled("c"), empty(), empty()],
            vec![empty(), empty(), empty()],
        ],
        1,
    )]);
    assert_eq!(md, "| a | b |\n| --- | --- |\n| c |  |\n");
}

#[test]
fn table_basic() {
    let md = doc(vec![table_from(
        vec![
            vec![
                Cell::from_inlines(vec![Inline::plain("Name")]),
                Cell::from_inlines(vec![Inline::plain("Age")]),
            ],
            vec![
                Cell::from_inlines(vec![Inline::plain("Ann | Bob")]),
                Cell::from_inlines(vec![Inline::plain("30")]),
            ],
        ],
        1,
    )]);
    assert_eq!(md, "| Name | Age |\n| --- | --- |\n| Ann \\| Bob | 30 |\n");
}

#[test]
fn table_headerless_and_ragged() {
    let md = doc(vec![table_from(
        vec![
            vec![Cell::from_inlines(vec![Inline::plain("a")])],
            vec![
                Cell::from_inlines(vec![Inline::plain("b")]),
                Cell::from_inlines(vec![Inline::plain("c")]),
            ],
        ],
        0,
    )]);
    assert_eq!(md, "|  |  |\n| --- | --- |\n| a |  |\n| b | c |\n");
}

#[test]
fn table_multiparagraph_cell() {
    let cell = Cell::new(vec![
        Block::Paragraph(vec![Inline::plain("one")]),
        Block::Paragraph(vec![Inline::plain("two")]),
    ]);
    let md =
        doc(vec![table_from(vec![vec![cell, Cell::from_inlines(vec![Inline::plain("x")])]], 0)]);
    assert_eq!(md, "|  |  |\n| --- | --- |\n| one<br>two | x |\n");
}

#[test]
fn merged_cells_render_blank_covered_positions() {
    let parse_ctx = ParseCtx::default();
    let mut b = GridBuilder::new(&parse_ctx);
    b.next_row();
    b.place(Cell::spanning(vec![Block::Paragraph(vec![Inline::plain("wide")])], 2, 1)).unwrap();
    b.place(Cell::from_inlines(vec![Inline::plain("end")])).unwrap();
    b.next_row();
    for t in ["a", "b", "c"] {
        b.place(Cell::from_inlines(vec![Inline::plain(t)])).unwrap();
    }
    let mut table = b.finish(TableKind::Data).unwrap();
    table.header_rows = 1;
    let md = doc(vec![Block::Table(table)]);
    assert_eq!(md, "| wide |  | end |\n| --- | --- | --- |\n| a | b | c |\n");
}

#[test]
fn trailing_covered_columns_are_preserved() {
    let parse_ctx = ParseCtx::default();
    let mut b = GridBuilder::new(&parse_ctx);
    b.next_row();
    b.place(Cell::spanning(vec![Block::Paragraph(vec![Inline::plain("wide")])], 3, 1)).unwrap();
    let mut table = b.finish(TableKind::Data).unwrap();
    table.header_rows = 1;
    let md = doc(vec![Block::Table(table)]);
    assert_eq!(md, "| wide |  |  |\n| --- | --- | --- |\n");
}

#[test]
fn url_pipes_cannot_split_table_cells() {
    let cell = Cell::from_inlines(vec![Inline::Link {
        content: Vec::new(),
        target: LinkTarget::External("https://e.test/a|b".into()),
    }]);
    let md = doc(vec![table_from(vec![vec![cell]], 0)]);
    assert_eq!(md, "|  |\n| --- |\n| [https://e.test/a\\|b](https://e.test/a%7Cb) |\n");
}

#[test]
fn code_span_pipes_cannot_split_table_cells() {
    let code = |t: &str| {
        Cell::from_inlines(vec![Inline::Text {
            text: t.into(),
            style: Style { code: true, ..Style::PLAIN },
        }])
    };
    let md = doc(vec![table_from(vec![vec![code("a | b"), code(r"a \| b")]], 0)]);
    assert_eq!(md, concat!("|  |  |\n", "| --- | --- |\n", r"| `a \| b` | `a \\\| b` |", "\n"));
    let md = doc(vec![table_from(vec![vec![code("a\\\n| b")]], 0)]);
    assert_eq!(md, concat!("|  |\n", "| --- |\n", r"| `a\ \| b` |", "\n"));
}

#[test]
fn a_cell_drops_blank_parts_lines_and_rows() {
    let text = |t: &str| Block::Paragraph(vec![Inline::plain(t)]);
    let nested = Table::from_rows(
        vec![
            vec![Cell::new(vec![text(" ")]), Cell::new(vec![text(" ")])],
            vec![Cell::new(vec![text("x")]), Cell::new(vec![text("y")])],
        ],
        0,
        TableKind::Data,
    );
    let cell = Cell::new(vec![
        text("a"),
        text(" "),
        Block::Paragraph(vec![
            Inline::plain("a"),
            Inline::LineBreak,
            Inline::LineBreak,
            Inline::plain("b"),
        ]),
        Block::Table(nested),
    ]);
    let md = doc(vec![table_from(vec![vec![cell]], 0)]);
    assert_eq!(md, "|  |\n| --- |\n| a<br>a<br>b<br>x / y |\n");
}

#[test]
fn a_ragged_table_renders_as_its_padded_twin() {
    let cell = |t: &str| Cell::from_inlines(vec![Inline::plain(t)]);
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
    let text = "x".repeat(10_000);
    let mut cell = Cell::from_inlines(vec![Inline::plain(&text)]);
    for _ in 0..100 {
        let table = Table::from_rows(vec![vec![cell]], 0, TableKind::Data);
        cell = Cell::new(vec![Block::Table(table)]);
    }
    let document = Document::default();
    let ledger = crate::ledger::Ledger::new(u64::MAX, None);
    let meter = super::Meter::new(&ledger, u64::MAX);
    let mut out = super::sink::Text::new(&meter);
    super::table::render_cell(&cell, &Ctx::of(&document), &mut out).unwrap();
    assert_eq!(out.as_str(), text);
    assert!(meter.admitted() <= text.len() as u64 + 100 * 8, "{}", meter.admitted());
}

#[test]
fn a_cell_part_taken_back_keeps_the_space_held_before_it() {
    let paragraph = |inlines| Block::Paragraph(inlines);
    let item = ListItem {
        blocks: vec![
            paragraph(vec![Inline::LineBreak]),
            paragraph(vec![Inline::plain(" y ")]),
            Block::heading(2, vec![Inline::plain(" \n ")]),
            Block::heading(2, vec![Inline::plain(" h \n\n i ")]),
        ],
        marker_label: None,
    };
    let list = Block::List(List { marker: MarkerKind::Bullet, start: 1, items: vec![item] });
    let nested = Table::from_rows(
        vec![vec![Cell::new(vec![paragraph(vec![Inline::plain(" \n ")])]), Cell::new(vec![list])]],
        0,
        TableKind::Data,
    );
    let cell = Cell::new(vec![paragraph(vec![Inline::plain("a  ")]), Block::Table(nested)]);
    let md = doc(vec![table_from(vec![vec![cell]], 0)]);
    assert_eq!(md, "|  |\n| --- |\n| a  <br> / •  y  **h<br>i** |\n");
}

#[test]
fn a_line_break_after_a_nested_table_in_a_list_item_breaks_the_cell_line() {
    let inner = Table::from_rows(
        vec![vec![Cell::from_inlines(vec![Inline::plain("x")])]],
        0,
        TableKind::Data,
    );
    let item = ListItem {
        blocks: vec![
            Block::Table(inner),
            Block::Paragraph(vec![Inline::LineBreak, Inline::plain("y")]),
        ],
        marker_label: None,
    };
    let list = Block::List(List { marker: MarkerKind::Bullet, start: 1, items: vec![item] });
    assert_eq!(
        doc(vec![table_from(vec![vec![Cell::new(vec![list])]], 0)]),
        "|  |\n| --- |\n| • x<br>y |\n"
    );
}

#[test]
fn whitespace_a_cell_shows_counts_once_against_the_maximum() {
    let text = format!("a{}|b", " ".repeat(100));
    let document = Document {
        blocks: vec![table_from(vec![vec![Cell::from_inlines(vec![Inline::plain(&text)])]], 0)],
        notes: Vec::new(),
        assets: Vec::new(),
    };
    let md = super::document_to_markdown(&document);
    assert_eq!(md, format!("|  |\n| --- |\n| a{}\\|b |\n", " ".repeat(100)));
    let ledger = crate::ledger::Ledger::new(u64::MAX, None);
    let len = md.len() as u64;
    assert_eq!(super::render(&document, &super::Meter::new(&ledger, len)).unwrap(), md);
    assert_matches!(
        super::render(&document, &super::Meter::new(&ledger, len - 1)),
        Err(ConvertError::ResourceLimit { limit: Limit::OutputBytes, .. })
    );
}

fn refused_on_what_it_holds(blocks: Vec<Block>, notes: Vec<Note>) {
    let document = Document { blocks, notes, assets: Vec::new() };
    let len = super::document_to_markdown(&document).len() as u64;
    let ledger = crate::ledger::Ledger::new(u64::MAX, None);
    let rendered = super::render(&document, &super::Meter::new(&ledger, len));
    assert_matches!(
        rendered,
        Err(ConvertError::ResourceLimit { limit: Limit::OutputBytes, .. }),
        "{len}"
    );
}

#[test]
fn held_paragraphs_headings_markers_and_note_labels_count_against_the_maximum() {
    refused_on_what_it_holds(vec![Block::Paragraph(vec![Inline::plain("abc   ")])], vec![]);
    refused_on_what_it_holds(vec![heading(1, "abc        ")], vec![]);
    let silent = Note {
        id: "n".into(),
        kind: NoteKind::Footnote,
        blocks: vec![Block::heading(1, vec![Inline::plain(" ")])],
    };
    refused_on_what_it_holds(vec![Block::Paragraph(vec![Inline::plain("abc")])], vec![silent]);
    let item =
        ListItem { blocks: vec![Block::Paragraph(vec![Inline::plain("  ")])], marker_label: None };
    refused_on_what_it_holds(
        vec![Block::List(List { marker: MarkerKind::Bullet, start: 1, items: vec![item] })],
        vec![],
    );
}

#[test]
fn notes_without_a_body_open_the_document() {
    let note = Note {
        id: "n".into(),
        kind: NoteKind::Footnote,
        blocks: vec![Block::Paragraph(vec![Inline::plain("N")])],
    };
    let md = document_to_markdown(&Document { blocks: vec![], notes: vec![note], assets: vec![] });
    assert_eq!(md, "[^1]: N\n");
}

#[test]
fn url_angle_brackets_are_encoded_without_bracketing() {
    let md = doc(vec![Block::Paragraph(vec![Inline::Link {
        content: vec![Inline::plain("link")],
        target: LinkTarget::External("https://e.test/a<b>c".into()),
    }])]);
    assert_eq!(
        md,
        "[link](https://e.test/a%3Cb%3Ec)
"
    );
}

#[test]
fn url_controls_cannot_split_the_document() {
    let md = doc(vec![Block::Paragraph(vec![Inline::Link {
        content: vec![Inline::plain("link")],
        target: LinkTarget::External("https://e.test/a\nb".into()),
    }])]);
    assert_eq!(md, "[link](https://e.test/a%0Ab)\n");
}

#[test]
fn nested_list() {
    let md = doc(vec![Block::List(List {
        marker: MarkerKind::Bullet,
        start: 1,
        items: vec![ListItem {
            blocks: vec![
                Block::Paragraph(vec![Inline::plain("outer")]),
                Block::List(List {
                    marker: MarkerKind::Decimal,
                    start: 3,
                    items: vec![ListItem {
                        blocks: vec![Block::Paragraph(vec![Inline::plain("inner")])],
                        marker_label: None,
                    }],
                }),
            ],
            marker_label: None,
        }],
    })]);
    assert_eq!(md, "- outer\n\n  3. inner\n");
}

#[test]
fn roman_and_alpha_markers_render_literally() {
    let item = |text: &str| ListItem {
        blocks: vec![Block::Paragraph(vec![Inline::plain(text)])],
        marker_label: None,
    };
    let md = doc(vec![
        Block::List(List {
            marker: MarkerKind::LowerRoman,
            start: 3,
            items: vec![item("third"), item("fourth")],
        }),
        Block::List(List {
            marker: MarkerKind::UpperAlpha,
            start: 27,
            items: vec![item("double letters")],
        }),
    ]);
    assert_eq!(md, "- iii. third\n- iv. fourth\n\n- AA. double letters\n");
}

#[test]
fn code_span_with_backticks() {
    let md = doc(vec![Block::Paragraph(vec![styled("a`b", Style { code: true, ..Style::PLAIN })])]);
    assert_eq!(md, "``a`b``\n");
}

#[test]
fn hard_break() {
    let md = doc(vec![Block::Paragraph(vec![
        Inline::plain("line one"),
        Inline::LineBreak,
        Inline::plain("line two"),
    ])]);
    assert_eq!(md, "line one\\\nline two\n");
}

#[test]
fn line_start_escape_after_hard_break() {
    let md = doc(vec![Block::Paragraph(vec![
        Inline::plain("intro"),
        Inline::LineBreak,
        Inline::plain("- dash"),
    ])]);
    assert_eq!(md, "intro\\\n\\- dash\n");
}

#[test]
fn blockquote() {
    let md = doc(vec![Block::BlockQuote(vec![Block::Paragraph(vec![Inline::plain("quoted")])])]);
    assert_eq!(md, "> quoted\n");
}

#[test]
fn empty_paragraphs_dropped() {
    let md = doc(vec![
        Block::Paragraph(vec![Inline::plain("  ")]),
        Block::Paragraph(vec![]),
        Block::Paragraph(vec![Inline::plain("real")]),
    ]);
    assert_eq!(md, "real\n");
}

#[test]
fn entity_escaped_but_plain_ampersand_kept() {
    let md = doc(vec![Block::Paragraph(vec![Inline::plain("A & B &amp; C")])]);
    assert_eq!(md, "A & B &amp;amp; C\n");
}

fn note(id: &str, blocks: Vec<Block>) -> Note {
    Note { id: id.into(), kind: NoteKind::Footnote, blocks }
}

#[test]
fn footnotes() {
    let md = document_to_markdown(&Document {
        blocks: vec![Block::Paragraph(vec![
            Inline::plain("Claim."),
            Inline::NoteRef("b".into()),
            Inline::plain(" More."),
            Inline::NoteRef("a".into()),
        ])],
        notes: vec![
            note("a", vec![Block::Paragraph(vec![Inline::plain("Second note.")])]),
            note(
                "b",
                vec![
                    Block::Paragraph(vec![Inline::plain("First note.")]),
                    Block::Paragraph(vec![Inline::plain("With a second paragraph.")]),
                ],
            ),
        ],
        assets: Vec::new(),
    });
    assert_eq!(
        md,
        "Claim.[^1] More.[^2]\n\n[^1]: First note.\n\n    With a second paragraph.\n\n[^2]: Second note.\n"
    );
}

#[test]
fn empty_and_unreferenced_notes() {
    let md = document_to_markdown(&Document {
        blocks: vec![Block::Paragraph(vec![
            Inline::plain("Text"),
            Inline::NoteRef("empty".into()),
        ])],
        notes: vec![
            note("empty", vec![Block::Paragraph(vec![])]),
            note("orphan", vec![Block::Paragraph(vec![Inline::plain("Kept.")])]),
        ],
        assets: Vec::new(),
    });
    assert_eq!(md, "Text\n\n[^1]: Kept.\n");
}

#[test]
fn duplicate_note_ids_render_one_definition() {
    let md = document_to_markdown(&Document {
        blocks: vec![Block::Paragraph(vec![Inline::plain("Text"), Inline::NoteRef("a".into())])],
        notes: vec![
            note("a", vec![Block::Paragraph(vec![Inline::plain("First wins.")])]),
            note("a", vec![Block::Paragraph(vec![Inline::plain("Duplicate dropped.")])]),
        ],
        assets: Vec::new(),
    });
    assert_eq!(md, "Text[^1]\n\n[^1]: First wins.\n");
}

#[test]
fn blank_duplicate_note_does_not_suppress_a_later_definition() {
    let md = document_to_markdown(&Document {
        blocks: vec![Block::Paragraph(vec![Inline::plain("Text"), Inline::NoteRef("a".into())])],
        notes: vec![
            note("a", vec![Block::Paragraph(Vec::new())]),
            note("a", vec![Block::Paragraph(vec![Inline::plain("Usable.")])]),
        ],
        assets: Vec::new(),
    });
    assert_eq!(md, "Text[^1]\n\n[^1]: Usable.\n");
}

#[test]
fn an_empty_item_keeps_its_marker_and_the_numbering() {
    let item = |text: &str| ListItem {
        blocks: if text.is_empty() {
            Vec::new()
        } else {
            vec![Block::Paragraph(vec![Inline::plain(text)])]
        },
        marker_label: None,
    };
    let md = doc(vec![Block::List(List {
        marker: MarkerKind::Decimal,
        start: 1,
        items: vec![item("one"), item(""), item("three")],
    })]);
    assert_eq!(
        md,
        "1. one
2. 
3. three
"
    );
}

#[test]
fn task_list() {
    let md = doc(vec![Block::List(List {
        marker: MarkerKind::Bullet,
        start: 1,
        items: vec![
            ListItem {
                blocks: vec![Block::Paragraph(vec![Inline::Checkbox(true), Inline::plain("done")])],
                marker_label: None,
            },
            ListItem {
                blocks: vec![Block::Paragraph(vec![
                    Inline::Checkbox(false),
                    Inline::plain(" todo"),
                ])],
                marker_label: None,
            },
        ],
    })]);
    assert_eq!(md, "- [x] done\n- [ ] todo\n");
}

#[test]
fn checkbox_in_a_table_cell() {
    let md = doc(vec![table_from(
        vec![vec![
            Cell::from_inlines(vec![Inline::Checkbox(true)]),
            Cell::from_inlines(vec![Inline::Checkbox(false), Inline::plain("Wall")]),
        ]],
        0,
    )]);
    assert_eq!(md, "|  |  |\n| --- | --- |\n| [x] | [ ] Wall |\n");
}

// ---------------------------------------------------------------------------
// New contexts: link labels, image alt, anchors

#[test]
fn link_label_bracket_escaped() {
    let md = doc(vec![Block::Paragraph(vec![Inline::Link {
        content: vec![Inline::plain("x]")],
        target: LinkTarget::External("https://e.com".into()),
    }])]);
    assert_eq!(md, "[x\\]](https://e.com)\n");
}

#[test]
fn image_alt_brackets_and_backslash_escaped() {
    let md = doc(vec![Block::Paragraph(vec![Inline::Image {
        alt: "a[b]c\\".into(),
        source: ImageSource::External("https://e.com/i.png".into()),
    }])]);
    assert_eq!(md, "![a\\[b\\]c\\\\](https://e.com/i.png)\n");
}

#[test]
fn anchor_on_plain_paragraph_round_trips() {
    let mark: AnchorId = "My Mark".into();
    let md = doc(vec![
        Block::Paragraph(vec![Inline::Anchor(mark.clone()), Inline::plain("Target here.")]),
        Block::Paragraph(vec![Inline::Link {
            content: vec![Inline::plain("jump")],
            target: LinkTarget::Anchor(mark),
        }]),
    ]);
    assert_eq!(md, "<a id=\"my-mark\"></a>Target here.\n\n[jump](#my-mark)\n");
}

#[test]
fn unreferenced_anchor_renders_nothing() {
    // An anchor no link targets is unreachable: producers mark up far more
    // positions than they reference.
    let md = doc(vec![Block::Paragraph(vec![
        Inline::Anchor("standalone-mark".into()),
        Inline::plain("No link points here."),
    ])]);
    assert_eq!(md, "No link points here.\n");
}

#[test]
fn heading_coincident_anchor_uses_slug() {
    let md = doc(vec![
        Block::Heading {
            level: 2,
            anchor: Some("bm1".into()),
            content: vec![Inline::plain("Section Two")],
        },
        Block::Paragraph(vec![Inline::Link {
            content: vec![Inline::plain("go")],
            target: LinkTarget::Anchor("bm1".into()),
        }]),
    ]);
    assert_eq!(md, "## Section Two\n\n[go](#section-two)\n");
}

#[test]
fn duplicate_heading_slugs_deduped() {
    let md = doc(vec![
        heading(1, "Same"),
        Block::Heading { level: 1, anchor: Some("x".into()), content: vec![Inline::plain("Same")] },
        Block::Paragraph(vec![Inline::Link {
            content: vec![Inline::plain("second")],
            target: LinkTarget::Anchor("x".into()),
        }]),
    ]);
    assert_eq!(md, "# Same\n\n# Same\n\n[second](#same-1)\n");
}

#[test]
fn code_block_in_cell_uses_code_span() {
    let cell = Cell::new(vec![Block::CodeBlock { lang: None, text: "let `x` = 1;".into() }]);
    let md = doc(vec![table_from(vec![vec![cell]], 0)]);
    assert_eq!(md, "|  |\n| --- |\n| ``let `x` = 1;`` |\n");
}

#[test]
fn code_block_keeps_its_language_hint() {
    let md = doc(vec![Block::CodeBlock { lang: Some("rust".into()), text: "fn main() {}".into() }]);
    assert_eq!(md, "```rust\nfn main() {}\n```\n");
}

#[test]
fn cell_edge_whitespace_is_trimmed() {
    // Cell padding from spreadsheets and CSV is layout, and the table's
    // own padding would swallow it anyway.
    let md = doc(vec![table_from(
        vec![vec![
            Cell::from_inlines(vec![Inline::plain("  padded\t")]),
            Cell::from_inlines(vec![Inline::plain("plain")]),
        ]],
        0,
    )]);
    assert_eq!(md, "|  |  |\n| --- | --- |\n| padded | plain |\n");
}

#[test]
fn rendering_reports_what_it_loses() {
    use DiagnosticCode::{DanglingReference as DANGLING, DuplicateNote as DUPLICATE};
    const READ: Unread = Unread { notes: false, units: false };
    let para = |inlines: Vec<Inline>| Block::Paragraph(inlines);
    let text = |t: &str| para(vec![Inline::plain(t)]);
    let note_ref = |id: &str| Inline::NoteRef(id.into());
    let link = |id: &str| Inline::Link {
        content: vec![Inline::plain("label")],
        target: LinkTarget::Anchor(id.into()),
    };
    let cell = Cell::new;
    let empty_table =
        || Block::Table(Table::from_rows(vec![vec![cell(Vec::new())]], 0, TableKind::Data));
    type Case = (&'static str, Vec<Block>, Vec<Note>, Unread, Vec<RenderLoss>);
    let cases: Vec<Case> = vec![
        (
            "a second body under a defined note id",
            vec![para(vec![note_ref("a")])],
            vec![note("a", vec![text("First.")]), note("a", vec![text("Second.")])],
            READ,
            vec![(DUPLICATE, Region::Note, 1)],
        ),
        (
            "a second body that renders empty",
            vec![para(vec![note_ref("a")])],
            vec![note("a", vec![text("First.")]), note("a", vec![empty_table()])],
            READ,
            vec![],
        ),
        (
            "a link to an anchor that exists nowhere",
            vec![para(vec![link("nowhere")])],
            vec![],
            READ,
            vec![(DANGLING, Region::Body, 1)],
        ),
        (
            "a link to an anchor that exists",
            vec![para(vec![Inline::Anchor("here".into()), link("here")])],
            vec![],
            READ,
            vec![],
        ),
        ("a link with an empty anchor", vec![para(vec![link("")])], vec![], READ, vec![]),
        (
            "a reference with no note",
            vec![para(vec![note_ref("missing")])],
            vec![],
            READ,
            vec![(DANGLING, Region::Body, 1)],
        ),
        (
            "a reference to a note",
            vec![para(vec![note_ref("a")])],
            vec![note("a", vec![text("Body.")])],
            READ,
            vec![],
        ),
        (
            "references and links in every kind of block",
            vec![
                Block::heading(1, vec![note_ref("h")]),
                Block::List(List {
                    marker: MarkerKind::Bullet,
                    start: 1,
                    items: vec![ListItem {
                        blocks: vec![para(vec![link("l")])],
                        marker_label: None,
                    }],
                }),
                Block::Table(Table::from_rows(
                    vec![vec![cell(vec![para(vec![note_ref("t")])])]],
                    0,
                    TableKind::Data,
                )),
                Block::BlockQuote(vec![para(vec![Inline::Link {
                    content: vec![note_ref("q"), link("inner")],
                    target: LinkTarget::External("https://example.com".into()),
                }])]),
            ],
            vec![],
            READ,
            vec![(DANGLING, Region::Body, 5)],
        ),
        (
            "a reference and a link in a note body",
            vec![para(vec![note_ref("a")])],
            vec![note("a", vec![para(vec![Inline::plain("Body"), note_ref("x"), link("y")])])],
            READ,
            vec![(DANGLING, Region::Note, 2)],
        ),
        (
            "a reference in a dropped second body",
            vec![para(vec![note_ref("a")])],
            vec![
                note("a", vec![text("First.")]),
                note("a", vec![para(vec![Inline::plain("Second"), note_ref("x")])]),
            ],
            READ,
            vec![(DUPLICATE, Region::Note, 1)],
        ),
        (
            "a reference in a note that renders no definition",
            vec![text("Body.")],
            vec![note("a", vec![para(vec![note_ref("x")])])],
            READ,
            vec![],
        ),
        (
            "references whose notes part the parse counted unreadable",
            vec![para(vec![note_ref("missing"), link("nowhere")])],
            vec![note("a", vec![para(vec![Inline::plain("Body"), note_ref("x")])])],
            Unread { notes: true, units: false },
            vec![(DANGLING, Region::Body, 1)],
        ),
        (
            "links whose anchors a unit the parse counted unreadable held",
            vec![para(vec![note_ref("missing"), link("nowhere")])],
            vec![note("a", vec![para(vec![Inline::plain("Body"), link("y")])])],
            Unread { notes: false, units: true },
            vec![(DANGLING, Region::Body, 1)],
        ),
    ];
    for (name, blocks, notes, unread, expected) in cases {
        let document = Document { blocks, notes, assets: Vec::new() };
        assert_eq!(render_losses(&document, unread), expected, "{name}");
    }
}

#[test]
fn the_causes_that_silence_render_losses_are_read_from_the_parse() {
    use DiagnosticCode::{PartUnreadable, UnitUnreadable};
    let cases = [
        ("none", vec![], (false, false)),
        ("a notes part", vec![(PartUnreadable, Region::Note)], (true, false)),
        ("a body part", vec![(PartUnreadable, Region::Body)], (false, false)),
        ("a unit", vec![(UnitUnreadable, Region::Body)], (false, true)),
        ("a unit in notes", vec![(UnitUnreadable, Region::Note)], (false, true)),
        (
            "both",
            vec![(UnitUnreadable, Region::Body), (PartUnreadable, Region::Note)],
            (true, true),
        ),
    ];
    for (name, reported, expected) in cases {
        let mut diagnostics = Diagnostics::default();
        for (code, region) in reported {
            diagnostics.add(code, region, None, 1);
        }
        let unread = Unread::of(&diagnostics);
        assert_eq!((unread.notes, unread.units), expected, "{name}");
    }
}

#[test]
fn every_container_nests_to_the_limit_and_no_further() {
    let paragraph = |inlines| Block::Paragraph(inlines);
    let wraps: [fn(Block) -> Block; 4] = [
        |inner| {
            Block::List(List {
                marker: MarkerKind::Bullet,
                start: 1,
                items: vec![ListItem { blocks: vec![inner], marker_label: None }],
            })
        },
        |inner| table_from(vec![vec![Cell::new(vec![inner])]], 0),
        |inner| Block::BlockQuote(vec![inner]),
        |inner| {
            let Block::Paragraph(content) = inner else { unreachable!() };
            let target = LinkTarget::External("https://example.com".into());
            Block::Paragraph(vec![Inline::Link { content, target }])
        },
    ];
    let refused = |doc: &Document| {
        matches!(
            check_nesting(doc, 4),
            Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 4 })
        )
    };
    for (case, wrap) in wraps.iter().enumerate() {
        let nested = |depth| {
            let mut block = paragraph(vec![Inline::plain("x")]);
            for _ in 0..depth {
                block = wrap(block);
            }
            block
        };
        let body = |depth| Document { blocks: vec![nested(depth)], ..Document::default() };
        let notes =
            |depth| Document { notes: vec![note("1", vec![nested(depth)])], ..Document::default() };
        assert!(check_nesting(&body(4), 4).is_ok(), "{case}");
        assert!(check_nesting(&notes(4), 4).is_ok(), "{case}");
        assert!(refused(&body(5)), "{case}");
        assert!(refused(&notes(5)), "{case}");
    }
}

#[test]
fn notes_number_in_the_order_their_first_references_are_read() {
    let reference = |id: &str| Inline::NoteRef(id.into());
    let text = |t: &str| Block::Paragraph(vec![Inline::plain(t)]);
    let target = LinkTarget::External("https://example.com".into());
    let md = document_to_markdown(&Document {
        blocks: vec![
            Block::Paragraph(vec![
                Inline::Link { content: vec![reference("a")], target },
                reference("d"),
            ]),
            Block::BlockQuote(vec![Block::Paragraph(vec![reference("c")])]),
        ],
        notes: vec![
            note("d", vec![text("D")]),
            note("c", vec![text("C")]),
            note("b", vec![text("B")]),
            note("a", vec![Block::Paragraph(vec![Inline::plain("A"), reference("b")])]),
        ],
        assets: Vec::new(),
    });
    assert_eq!(
        md,
        "[[^1]](https://example.com)[^3]\n\n> [^4]\n\n[^1]: A[^2]\n\n[^2]: B\n\n[^3]: D\n\n[^4]: C\n"
    );
}

#[test]
fn each_nested_link_is_looked_ahead_at_once() {
    let depth = 300;
    let bold = Style { bold: true, ..Style::PLAIN };
    let unresolved = LinkTarget::Anchor("nowhere".into());
    let external = LinkTarget::External("https://example.com".into());
    let shapes = [
        ("docx unresolved", Inline::plain("x"), unresolved.clone()),
        ("epub unresolved bold", Inline::Text { text: "x".into(), style: bold }, unresolved),
        ("external", Inline::plain("x"), external),
    ];
    for (name, leaf, target) in shapes {
        let mut content = vec![leaf];
        for _ in 0..depth {
            content = vec![Inline::Link { content, target: target.clone() }];
        }
        let doc = Document { blocks: vec![Block::Paragraph(content)], ..Document::default() };
        let rc = Ctx::of(&doc);
        assert!(render_block(&doc.blocks[0], &rc).is_some(), "{name}");
        assert_eq!(rc.links.computed.get(), depth, "{name}");
    }
}

#[test]
fn a_link_without_a_target_drops_content_that_shows_nothing() {
    let untargeted = |content| Inline::Link { content, target: LinkTarget::Anchor(String::new()) };
    let between = |inline| {
        Block::Paragraph(vec![Inline::plain("a"), untargeted(vec![inline]), Inline::plain("b")])
    };
    let md = document_to_markdown(&Document {
        blocks: vec![
            between(Inline::plain(" ")),
            between(Inline::LineBreak),
            between(Inline::Anchor("t".into())),
            between(Inline::plain(" c ")),
            Block::Paragraph(vec![Inline::Link {
                content: vec![Inline::plain("to t")],
                target: LinkTarget::Anchor("t".into()),
            }]),
        ],
        ..Document::default()
    });
    assert_eq!(md, "ab\n\nab\n\nab\n\na c b\n\n[to t](#t)\n");
}
