use super::*;
use proptest::prelude::*;

const BOTH_READERS: [Options; 2] = [Options::empty(), Options::ENABLE_TABLES];

fn html_and_links(md: &str, options: Options) -> (usize, usize) {
    Parser::new_ext(md, options).fold((0, 0), |(html, links), event| match event {
        Event::InlineHtml(tag) if tag.as_ref() == "<br>" => (html, links),
        Event::Html(_) | Event::InlineHtml(_) => (html + 1, links),
        Event::Start(Tag::Link { .. } | Tag::Image { .. }) => (html, links + 1),
        _ => (html, links),
    })
}

fn only_held_links_are_active(md: &str, held: usize) -> Result<(), TestCaseError> {
    for options in BOTH_READERS {
        prop_assert_eq!(html_and_links(md, options), (0, held), "{:?}", md);
    }
    Ok(())
}

#[test]
fn a_backtick_right_after_a_code_span_is_escaped_so_it_cannot_join_the_closing_fence() {
    let code = |text| styled(text, CODE);
    let bold_code = Style { bold: true, ..CODE };
    let site_link = |content| link(content, external("http://x.test"));
    for (name, inlines, expected, held) in [
        ("one backtick", vec![code("</a>"), Inline::plain("`")], "`</a>`\\`\n", 0),
        ("a run of two", vec![code("</a>"), Inline::plain("``x")], "`</a>`\\`\\`x\n", 0),
        ("a later backtick", vec![code("</a>"), Inline::plain("x`")], "`</a>`x`\n", 0),
        ("marks between", vec![styled("a", bold_code), Inline::plain("`")], "**`a`**`\n", 0),
        (
            "a link label",
            vec![site_link(vec![code("</a>"), Inline::plain("`")])],
            "[`</a>`\\`](http://x.test)\n",
            1,
        ),
        (
            "a label before a span",
            vec![site_link(vec![Inline::plain("L`")]), code("<")],
            "[L\\`](http://x.test)`<`\n",
            1,
        ),
        (
            "a label before text with a backtick",
            vec![site_link(vec![Inline::plain("L`")]), Inline::plain("x`")],
            "[L\\`](http://x.test)x`\n",
            1,
        ),
    ] {
        let md = paragraph(inlines);
        assert_eq!(md, expected, "{name}");
        only_held_links_are_active(&md, held).unwrap();
    }
}

#[test]
fn an_email_shaped_text_never_opens_an_autolink() {
    let plain = Inline::plain;
    for (name, inlines, expected, held) in [
        ("only punctuation", vec![plain("x <.-_@example.com> y")], "x \\<.-_@example.com> y\n", 0),
        ("no domain", vec![plain("<1@")], "\\<1@\n", 0),
        ("inequalities", vec![plain("p<0.05, n<5, a<-1")], "p<0.05, n<5, a<-1\n", 0),
        ("an empty local part", vec![plain("a <@example.com>")], "a <@example.com>\n", 0),
        (
            "through code",
            vec![plain("<1"), styled("x", CODE), plain("@example.com>")],
            "\\<1`x`@example.com>\n",
            0,
        ),
        (
            "into code",
            vec![plain("<"), styled("x", CODE), plain("@example.com>")],
            "\\<`x`@example.com>\n",
            0,
        ),
        (
            "through emphasis",
            vec![styled("<1", BOLD), plain("@example.com>")],
            "**\\<1**@example.com>\n",
            0,
        ),
        (
            "before a link",
            vec![plain("p<0.05"), link(vec![plain("n")], external("http://x.test"))],
            "p<0.05[n](http://x.test)\n",
            1,
        ),
    ] {
        let md = paragraph(inlines);
        assert_eq!(md, expected, "{name}");
        only_held_links_are_active(&md, held).unwrap();
    }
}

#[test]
fn only_the_characters_of_an_email_local_part_make_an_angle_an_autolink_opener() {
    for c in ".!#$%&'*+/=?^_`{|}~-0aZ".chars() {
        let md = paragraph(vec![Inline::plain(format!("<{c}@example.com>"))]);
        assert!(md.starts_with("\\<"), "{c}: {md:?}");
        only_held_links_are_active(&md, 0).unwrap();
    }
    for c in [',', ';', ':', '(', ')', '"', ' '] {
        let md = paragraph(vec![Inline::plain(format!("<{c}@example.com>"))]);
        assert!(md.starts_with('<'), "{c}: {md:?}");
    }
}

#[test]
fn text_in_separate_parts_of_a_cell_never_pairs_across_the_join() {
    let plain = |t: &str| Block::Paragraph(vec![Inline::plain(t)]);
    let code = Block::Paragraph(vec![styled("</a>", CODE)]);
    for (name, parts, expected) in [
        ("a link", vec![plain("[a"), plain("b](u)")], "|  |\n| --- |\n| \\[a<br>b](u) |\n"),
        ("a span", vec![plain("a `"), code], "|  |\n| --- |\n| a \\`<br>`</a>` |\n"),
        (
            "a list",
            vec![Block::List(List {
                marker: MarkerKind::Bullet,
                start: 1,
                items: vec![
                    ListItem { blocks: vec![plain("[a")], marker_label: None },
                    ListItem { blocks: vec![plain("b](u)")], marker_label: None },
                ],
            })],
            "|  |\n| --- |\n| • \\[a<br>• b](u) |\n",
        ),
        ("a lone bracket", vec![plain("a ["), plain("b")], "|  |\n| --- |\n| a \\[<br>b |\n"),
    ] {
        let md = doc(vec![Block::Table(Table::from_rows(vec![vec![Cell::new(parts)]], 0))]);
        assert_eq!(md, expected, "{name}");
        only_held_links_are_active(&md, 0).unwrap();
    }
}

#[test]
fn text_in_separate_cells_never_pairs_across_the_row_for_a_reader_without_tables() {
    let row =
        vec![cell("[a"), cell("`"), cell("b](u)"), Cell::from_inlines(vec![styled("x", CODE)])];
    let md = doc(vec![Block::Table(Table::from_rows(vec![row], 0))]);
    assert_eq!(md, "|  |  |  |  |\n| --- | --- | --- | --- |\n| \\[a | \\` | b](u) | `x` |\n");
    only_held_links_are_active(&md, 0).unwrap();
}

#[derive(Clone, Debug)]
enum Piece {
    Plain(String),
    Code(String),
    Bold(String),
    Italic(String),
    Break,
    Link(String),
    Image(String),
}

fn piece() -> impl Strategy<Value = Piece> {
    prop_oneof![
        4 => run().prop_map(Piece::Plain),
        3 => run().prop_map(Piece::Code),
        1 => run().prop_map(Piece::Bold),
        1 => run().prop_map(Piece::Italic),
        1 => Just(Piece::Break),
        2 => run().prop_map(Piece::Link),
        1 => run().prop_map(Piece::Image),
    ]
}

fn pieces() -> impl Strategy<Value = Vec<Piece>> {
    proptest::collection::vec(piece(), 1..6).prop_map(|mut pieces| {
        // Two hard breaks leave a blank line, which ends the paragraph.
        pieces.dedup_by(|b, a| matches!((a, b), (Piece::Break, Piece::Break)));
        pieces
    })
}

fn label() -> impl Strategy<Value = Option<String>> {
    // A blank label leaves the item's text four columns in, as indented code.
    proptest::option::of(run().prop_filter_map("a label shows", |run| {
        Some(run.trim().to_owned()).filter(|label| !label.is_empty())
    }))
}

#[derive(Clone, Debug)]
enum Part {
    Paragraph(Vec<Piece>),
    Heading(Vec<Piece>),
    List(Option<String>, Vec<Piece>),
    Code(String),
}

fn part() -> impl Strategy<Value = Part> {
    prop_oneof![
        4 => pieces().prop_map(Part::Paragraph),
        1 => pieces().prop_map(Part::Heading),
        2 => (label(), pieces()).prop_map(|(label, p)| Part::List(label, p)),
        1 => run().prop_map(Part::Code),
    ]
}

#[derive(Default)]
struct Built {
    links: usize,
}

impl Built {
    fn inlines(&mut self, pieces: &[Piece]) -> Vec<Inline> {
        pieces
            .iter()
            .map(|piece| match piece {
                Piece::Plain(text) => Inline::plain(text),
                Piece::Code(text) => styled(text, CODE),
                Piece::Bold(text) => styled(text, BOLD),
                Piece::Italic(text) => styled(text, ITALIC),
                Piece::Break => Inline::LineBreak,
                Piece::Link(text) => {
                    self.links += 1;
                    let url = format!("http://x.test/{}", self.links);
                    link(vec![Inline::plain(format!("L{text}"))], external(&url))
                }
                Piece::Image(alt) => image(alt, ImageSource::Unavailable),
            })
            .collect()
    }

    fn list(&mut self, label: Option<String>, pieces: &[Piece]) -> Block {
        let blocks = vec![Block::Paragraph(self.inlines(pieces))];
        let items = vec![ListItem { blocks, marker_label: label }];
        Block::List(List { marker: MarkerKind::Bullet, start: 1, items })
    }

    fn part(&mut self, part: &Part) -> Block {
        match part {
            Part::Paragraph(pieces) => Block::Paragraph(self.inlines(pieces)),
            Part::Heading(pieces) => Block::heading(1, self.inlines(pieces)),
            Part::List(label, pieces) => self.list(label.clone(), pieces),
            Part::Code(text) => Block::CodeBlock { lang: None, text: text.clone() },
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 4096,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]
    #[test]
    fn math_free_inlines_never_form_active_markup_the_model_did_not_hold(
        pieces in pieces(),
        container in 0..3_usize,
        label in label(),
    ) {
        let mut built = Built::default();
        let block = match container {
            0 => Block::Paragraph(built.inlines(&pieces)),
            1 => Block::heading(2, built.inlines(&pieces)),
            _ => built.list(label, &pieces),
        };
        only_held_links_are_active(&doc(vec![block]), built.links)?;
    }

    #[test]
    fn math_free_table_cells_never_form_active_markup_the_model_did_not_hold(
        cells in proptest::collection::vec(proptest::collection::vec(part(), 1..4), 1..7),
        width in 1..4_usize,
    ) {
        let mut built = Built::default();
        let cells: Vec<Cell> = cells
            .iter()
            .map(|parts| Cell::new(parts.iter().map(|part| built.part(part)).collect()))
            .collect();
        let mut rows = Vec::new();
        let mut cells = cells.into_iter().peekable();
        while cells.peek().is_some() {
            rows.push(cells.by_ref().take(width).collect());
        }
        let md = doc(vec![Block::Table(Table::from_rows(rows, 0))]);
        only_held_links_are_active(&md, built.links)?;
    }
}
