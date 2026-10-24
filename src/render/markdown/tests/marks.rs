use super::*;
use crate::ledger::CancelToken;

const BOLD_ITALIC: Style = Style { italic: true, ..BOLD };
const BOLD_STRIKE: Style = Style { strike: true, ..BOLD };
const ITALIC_STRIKE: Style = Style { strike: true, ..ITALIC };

#[test]
fn a_mark_change_keeps_the_shared_marks_open_and_closes_the_rest_in_reverse() {
    let plain = Inline::plain;
    for (runs, expected) in [
        (vec![styled("a", BOLD), styled("b", ITALIC)], "**a**_b_"),
        (vec![styled("a", ITALIC), styled("b", BOLD)], "_a_**b**"),
        (vec![styled("a", BOLD), styled("b", BOLD_ITALIC)], "**&#97;_b_**"),
        (vec![styled("a", BOLD_ITALIC), styled("b", BOLD)], "**_a_&#98;**"),
        (vec![styled("a", BOLD_STRIKE), styled("b", BOLD)], "~~**a**~~**b**"),
        (vec![styled("a", STRIKE), styled("b", BOLD_STRIKE)], "~~a**b**~~"),
        (vec![styled("-", ITALIC_STRIKE), styled("~", STRIKE)], "~~_-_\\~~~"),
        (vec![styled("a", BOLD), plain(" "), styled("b", ITALIC)], "**a** _b_"),
        (vec![styled("a", BOLD), plain(" "), styled("b", BOLD_ITALIC)], "**a _b_**"),
        (vec![styled("a", BOLD_ITALIC), plain(" "), styled("b", BOLD)], "**_a_ b**"),
        (vec![plain("a*"), styled("b", BOLD)], "a\\***b**"),
        (vec![styled("a", BOLD), plain("*b")], "**a**\\*b"),
        (vec![styled("a", ITALIC), plain("__b")], "_a_\\_\\_b"),
    ] {
        assert_eq!(paragraph(runs.clone()), format!("{expected}\n"), "{runs:?}");
    }
}

#[test]
fn a_delimiter_that_would_not_flank_gets_a_numeric_reference_beside_it() {
    let plain = Inline::plain;
    for (runs, expected) in [
        (vec![plain("a"), styled("\"foo\"", BOLD), plain("b")], "&#97;**\"foo\"**&#98;"),
        (vec![styled("\"foo\"", BOLD), plain(" b")], "**\"foo\"** b"),
        (vec![plain("ab"), styled("c", ITALIC)], "a&#98;_c_"),
        (vec![plain("a"), styled("\"b\"", STRIKE)], "&#97;~~\"b\"~~"),
        (vec![plain("x"), styled("b", STRIKE), plain("y")], "x~~b~~y"),
        (vec![plain("«"), styled("x", BOLD), plain("»")], "«**x**»"),
        (vec![plain("x"), styled("«y»", BOLD), plain("z")], "&#120;**«y»**&#122;"),
        (vec![plain("\u{1F600}"), styled("b", BOLD)], "\u{1F600}**b**"),
        (vec![plain("a"), styled("ⓐ", BOLD)], "&#97;**ⓐ**"),
        (vec![styled("ⓐ", BOLD), plain("b")], "**ⓐ**&#98;"),
        (vec![plain("x"), styled("ⓐ", ITALIC)], "&#120;_ⓐ_"),
        (vec![plain("x"), styled("\u{1F130}", BOLD), plain("y")], "&#120;**\u{1F130}**&#121;"),
    ] {
        let markdown = paragraph(runs.clone());
        assert_eq!(markdown, format!("{expected}\n"), "{runs:?}");
        assert_eq!(read_back(&markdown).1, marks_of(&runs), "{markdown}");
    }
}

#[test]
fn numeric_references_carry_every_scalar_back() {
    for c in [
        '\u{80}',
        '\u{9F}',
        '\u{FFFE}',
        '\u{FFFF}',
        '\u{10FFFF}',
        '\u{301}',
        '\u{200D}',
        'é',
        '\u{B}',
        '\u{85}',
        '\u{2028}',
        '\u{2029}',
    ] {
        let markdown = paragraph(vec![Inline::plain(format!("x{c}")), styled("\"y\"", BOLD)]);
        assert_eq!(markdown, format!("x&#{};**\"y\"**\n", u32::from(c)));
        assert_eq!(parsed_text(&markdown), format!("x{c}\"y\""), "{c:?}");
    }
    let nul = paragraph(vec![Inline::plain("x\0"), styled("\"y\"", BOLD)]);
    assert_eq!(nul, "x&#0;**\"y\"**\n");
    assert_eq!(parsed_text(&nul), "x\u{FFFD}\"y\"");
}

#[test]
fn marks_close_before_every_line_end_and_reopen_after_it() {
    for (runs, expected) in [
        (vec![styled("a\n\nb", BOLD)], "**a**\n\n**b**"),
        (vec![styled("a \nb", BOLD)], "**a**\n**b**"),
        (vec![styled("a", BOLD), Inline::plain(" \n "), styled("b", BOLD)], "**a**\n**b**"),
        (vec![styled("a\n\nb", SUP)], "<sup>a</sup>\n\n<sup>b</sup>"),
        (vec![styled("a", BOLD), Inline::LineBreak, styled("b", BOLD)], "**a**\\\n**b**"),
        (
            vec![styled("a", BOLD), Inline::LineBreak, Inline::LineBreak, styled("b", BOLD)],
            "**a**\\\n\n**b**",
        ),
        (vec![styled("a ", BOLD), Inline::LineBreak, styled("b", BOLD)], "**a** \\\n**b**"),
        (vec![styled("a\r\rb", BOLD)], "**a**\r\r**b**"),
        (vec![styled("a", BOLD), Inline::plain(" \r "), styled("b", BOLD)], "**a** \r **b**"),
    ] {
        assert_eq!(paragraph(runs.clone()), format!("{expected}\n"), "{runs:?}");
    }
    let cell = Cell::new(vec![Block::Paragraph(vec![
        styled("a", BOLD),
        Inline::LineBreak,
        styled("b", BOLD),
    ])]);
    assert_eq!(doc(vec![table_from(vec![vec![cell]], 1)]), "| **a**<br>**b** |\n| --- |\n");
    let heading = Block::heading(1, vec![styled("a", BOLD), Inline::LineBreak, styled("b", BOLD)]);
    assert_eq!(doc(vec![heading]), "# **a b**\n");
}

/// The marks a parser reads on each code span of `markdown`.
fn code_marks(markdown: &str) -> Vec<(String, Style)> {
    let mut open = Open::default();
    let mut spans = Vec::new();
    for event in Parser::new_ext(markdown, Options::ENABLE_STRIKETHROUGH) {
        match event {
            Event::Code(code) => spans.push((code.into_string(), open.style())),
            other => _ = open.track(&other),
        }
    }
    spans
}

#[test]
fn code_keeps_its_marks_outside_its_fences() {
    for bits in 0u8..8 {
        let marks = emphasis(bits);
        let code = Style { code: true, ..marks };
        for (before, after) in [("", ""), ("a", "b"), ("\"", "\""), (" ", " ")] {
            let mut runs = vec![styled("x`y", code)];
            if !before.is_empty() {
                runs.insert(0, Inline::plain(before));
                runs.push(Inline::plain(after));
            }
            let markdown = paragraph(runs);
            assert_eq!(code_marks(&markdown), [("x`y".to_string(), marks)], "{markdown}");
        }
    }
    let all = Style { code: true, bold: true, italic: true, strike: true, ..Style::PLAIN };
    assert_eq!(paragraph(vec![styled("x", all)]), "~~**_`x`_**~~\n");
    let bold_code = Style { code: true, ..BOLD };
    assert_eq!(
        paragraph(vec![Inline::plain("a"), styled("x", bold_code), Inline::plain("b")]),
        "&#97;**`x`**&#98;\n"
    );
    assert_eq!(paragraph(vec![styled("a", BOLD), styled("x", bold_code)]), "**a`x`**\n");
}

#[test]
fn a_heading_cell_is_bold_beneath_its_own_marks() {
    let heading = |content| {
        let cell = Cell::new(vec![Block::heading(2, content)]);
        doc(vec![table_from(vec![vec![cell]], 1)])
    };
    let plain = Inline::plain;
    assert_eq!(heading(vec![plain("a"), styled("b", BOLD), plain("c")]), "| **abc** |\n| --- |\n");
    assert_eq!(heading(vec![plain("a "), styled("b", ITALIC)]), "| **a _b_** |\n| --- |\n");
    assert_eq!(heading(vec![plain("  ")]), "");
    assert_eq!(
        heading(vec![plain("a "), link(vec![plain(" ")], external("https://a.test"))]),
        "| **a [https://a.test](https://a.test)** |\n| --- |\n"
    );
    assert_eq!(heading(vec![Inline::Checkbox(true), plain("done")]), "| **[x] done** |\n| --- |\n");
    assert_eq!(heading(vec![plain("a"), Inline::Math("x".into())]), "| **a$x$** |\n| --- |\n");
}

#[test]
fn a_link_whose_content_shows_nothing_is_labelled_with_its_url() {
    for content in [Inline::Anchor("unplaced".into()), Inline::Math(" ".into())] {
        assert_eq!(
            paragraph(vec![link(vec![content.clone()], external("https://a.test"))]),
            "[https://a.test](https://a.test)\n",
            "{content:?}"
        );
    }
}

#[test]
fn a_link_that_keeps_no_destination_gives_its_text_to_the_seams_around_it() {
    let refused = |content| link(content, external("javascript:x"));
    let missing = |content| link(content, LinkTarget::anchor("nowhere".into()));
    for wrap in [refused, missing] {
        let markdown = paragraph(vec![
            Inline::plain("a"),
            wrap(vec![styled("\"foo\"", BOLD)]),
            Inline::plain("b"),
        ]);
        assert_eq!(markdown, "&#97;**\"foo\"**&#98;\n");
    }
}

#[test]
fn an_image_written_as_its_label_joins_the_text_beside_it() {
    let document = Document {
        blocks: vec![Block::Paragraph(vec![
            styled("a", ITALIC),
            image("X", ImageSource::Unavailable),
            styled("b", ITALIC),
        ])],
        ..Document::default()
    };
    assert_eq!(rendered(&document, AltText), "_a_&#88;_b_\n");
    assert_eq!(rendered(&document, Placeholder), "_a_\\[X\\]_b_\n");
}

#[test]
fn a_nested_link_splits_the_link_around_it() {
    let (a, b) = (|| external("https://a.test"), || external("https://b.test"));
    let plain = Inline::plain;
    for (content, expected) in [
        (
            vec![plain("x"), link(vec![plain("y")], b()), plain("z")],
            "[x](https://a.test)[y](https://b.test)[z](https://a.test)",
        ),
        (vec![plain("x"), link(vec![plain("y")], a()), plain("z")], "[xyz](https://a.test)"),
        (vec![link(vec![plain("x")], b()), link(vec![plain("y")], b())], "[xy](https://b.test)"),
        (
            vec![plain("x"), link(vec![plain("y")], external("javascript:y")), plain("z")],
            "[xyz](https://a.test)",
        ),
        (
            vec![plain("x"), link(vec![plain("y")], LinkTarget::anchor("nowhere".into()))],
            "[xy](https://a.test)",
        ),
        (vec![plain(" "), link(vec![plain("y")], b())], " [y](https://b.test)"),
        (vec![plain(" "), link(vec![plain(" ")], b())], " [https://b.test](https://b.test)"),
        (
            vec![plain(" "), link(vec![plain(" ")], external("javascript:y"))],
            "[https://a.test](https://a.test)",
        ),
        (
            vec![styled("x", BOLD), link(vec![styled("y", BOLD)], b())],
            "[**x**](https://a.test)[**y**](https://b.test)",
        ),
    ] {
        let markdown = paragraph(vec![plain("p "), link(content.clone(), a())]);
        assert_eq!(markdown, format!("p {expected}\n"), "{content:?}");
        assert_eq!(parsed_destinations(&markdown).len(), expected.matches("](").count());
    }
    let siblings = paragraph(vec![link(vec![plain("x")], a()), link(vec![plain("y")], a())]);
    assert_eq!(siblings, "[x](https://a.test)[y](https://a.test)\n");
    let mut deep = vec![plain("z")];
    for depth in 0..60 {
        let target = if depth % 2 == 0 { a() } else { b() };
        deep = vec![plain("x"), link(deep, target)];
    }
    let markdown = paragraph(deep);
    assert_eq!(parsed_destinations(&markdown).len(), 60, "{markdown}");
    assert!(!markdown.contains("[["), "{markdown}");
}

#[test]
fn a_heading_slug_follows_the_split_links_it_renders() {
    let blank_inside = vec![link(
        vec![Inline::plain(" "), link(vec![Inline::plain(" ")], external("https://b.test"))],
        external("https://a.test"),
    )];
    let document = Document {
        blocks: vec![
            anchored(1, "h", blank_inside),
            Block::Paragraph(vec![link(vec![Inline::plain("to")], LinkTarget::anchor("h".into()))]),
        ],
        ..Document::default()
    };
    assert_eq!(
        rendered(&document, Placeholder),
        "# [https://b.test](https://b.test)\n\n[to](#httpsbtest)\n"
    );
}

#[test]
fn an_image_in_a_split_link_is_judged_inside_the_label_that_holds_it() {
    let picture = || image("I", ImageSource::External("javascript:i".into()));
    let inside = Document {
        blocks: vec![Block::Paragraph(vec![link(
            vec![picture(), link(vec![picture()], external("https://b.test"))],
            external("https://a.test"),
        )])],
        ..Document::default()
    };
    let empty = Document {
        blocks: vec![Block::Paragraph(vec![link(vec![picture()], external(""))])],
        ..Document::default()
    };
    let outside = Document {
        blocks: vec![Block::Paragraph(vec![link(
            vec![Inline::plain("x"), picture()],
            LinkTarget::anchor("nowhere".into()),
        )])],
        ..Document::default()
    };
    assert_eq!(rendered(&inside, Placeholder), "[I](https://a.test)[I](https://b.test)\n");
    assert_eq!(rendered(&outside, Placeholder), "xI\n");
    let code = |losses: &[RenderLoss]| {
        losses
            .iter()
            .map(|&(code, ..)| code)
            .filter(|code| {
                matches!(code, DiagnosticCode::ImageOmitted | DiagnosticCode::UnsafeLink)
            })
            .collect::<Vec<_>>()
    };
    let inside_losses = render_losses(&inside, READ, Placeholder);
    assert_eq!(code(&inside_losses), [DiagnosticCode::ImageOmitted]);
    assert!(
        inside_losses
            .iter()
            .any(|&(code, .., count)| code == DiagnosticCode::ImageOmitted && count == 2)
    );
    assert_eq!(code(&render_losses(&outside, READ, Placeholder)), [DiagnosticCode::UnsafeLink]);
    assert_eq!(rendered(&empty, Placeholder), "I\n");
    assert_eq!(code(&render_losses(&empty, READ, Placeholder)), [DiagnosticCode::UnsafeLink]);
}

#[test]
fn a_link_its_descendants_cover_keeps_its_anchor_addressable() {
    let document = Document {
        blocks: vec![
            Block::Paragraph(vec![link(
                vec![link(vec![Inline::plain("x")], external("https://b.test"))],
                LinkTarget::anchor("t".into()),
            )]),
            Block::Paragraph(vec![Inline::Anchor("t".into()), Inline::plain("there")]),
        ],
        ..Document::default()
    };
    assert_eq!(rendered(&document, Placeholder), "[x](https://b.test)\n\n<a id=\"t\"></a>there\n");
}

fn quoted() -> Document {
    let runs = vec![Inline::plain("a"), styled("\"foo\"", BOLD), Inline::plain("b")];
    Document { blocks: vec![Block::Paragraph(runs)], ..Document::default() }
}

#[test]
fn numeric_references_count_against_the_output_maximum() {
    let markdown = rendered(&quoted(), Placeholder);
    assert!(markdown.contains("&#97;"), "{markdown}");
    let len = markdown.len() as u64;
    render_at(&quoted(), u64::MAX, len, Placeholder).unwrap();
    assert_matches!(
        render_at(&quoted(), u64::MAX, len - 1, Placeholder),
        Err(ConvertError::ResourceLimit { limit: Limit::OutputBytes, .. })
    );
}

#[test]
fn every_inline_read_is_charged_even_where_it_writes_nothing() {
    let unlinked: Vec<Inline> = (0..10).map(|i| Inline::Anchor(format!("a{i}"))).collect();
    let document = Document { blocks: vec![Block::Paragraph(unlinked)], ..Document::default() };
    let at = |max| render_at(&document, max, u64::MAX, Placeholder);
    assert_eq!(at(10).unwrap().markdown, "");
    assert_matches!(at(9), Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 9 }));
    let cancel = CancelToken::new();
    cancel.cancel();
    let ledger = Ledger::new(u64::MAX, Some(&cancel));
    assert_matches!(
        render(&document, &GapFills::default(), &Meter::new(&ledger, u64::MAX), Placeholder),
        Err(ConvertError::Cancelled)
    );
    let omitted = |count| -> Vec<Inline> {
        (0..count).map(|_| image("I", ImageSource::Unavailable)).collect()
    };
    let fallbacks: [fn(Vec<Inline>) -> Inline; 2] = [
        |images| link(images, external("https://a.test")),
        |images| link(vec![link(images, external("https://b.test"))], external("https://a.test")),
    ];
    for fallback in fallbacks {
        let least = |count| {
            let document = Document {
                blocks: vec![Block::Paragraph(vec![fallback(omitted(count))])],
                ..Document::default()
            };
            least_work(|meter| render(&document, &GapFills::default(), meter, Omit).is_ok())
        };
        assert_eq!(least(20) - least(10), 10);
    }
}

#[test]
fn a_list_item_paragraph_is_charged_where_its_shape_is_read_and_where_it_is_written() {
    let item = |count| ListItem {
        blocks: vec![Block::Paragraph(
            (0..count)
                .map(|i| Inline::Anchor(format!("a{i}")))
                .chain([Inline::plain("x")])
                .collect(),
        )],
        marker_label: None,
    };
    let least = |count| {
        let list = List { marker: MarkerKind::Bullet, start: 1, items: vec![item(count)] };
        let document = Document { blocks: vec![Block::List(list)], ..Document::default() };
        least_work(|meter| render(&document, &GapFills::default(), meter, Placeholder).is_ok())
    };
    assert_eq!(least(20) - least(10), 2 * 10);
}

#[test]
fn the_loss_walk_is_charged_for_each_note_it_reads() {
    let least = |count| {
        let body = Block::Paragraph((0..count).map(|i| Inline::Anchor(format!("a{i}"))).collect());
        let document = Document { notes: vec![note("n", vec![body])], ..Document::default() };
        least_work(|meter| super::super::render_losses(&document, READ, Placeholder, meter).is_ok())
    };
    assert_eq!(least(20) - least(10), 10);
}

#[test]
fn a_link_label_is_charged_where_it_is_read_looked_ahead_at_and_written() {
    let label = vec![Inline::plain("a"), styled("b", BOLD)];
    let document = Document {
        blocks: vec![Block::Paragraph(vec![link(label, external("https://e.test"))])],
        ..Document::default()
    };
    let at = |max| render_at(&document, max, u64::MAX, Placeholder);
    let (read, shows) = (3, 1);
    let (outer, inner) = (1 + 2 + 1, 2 + 2);
    let work = read + shows + outer + inner;
    assert_eq!(at(work).unwrap().markdown, "[a**b**](https://e.test)\n");
    assert_matches!(at(work - 1), Err(ConvertError::ResourceLimit { limit: Limit::Work, .. }));
}

/// Each `&#N;` of `markdown` that a parser reads back as `expected` without it.
fn needless_references(markdown: &str, expected: &(String, Vec<(char, Style)>)) -> Vec<String> {
    let mut needless = Vec::new();
    let mut from = 0;
    while let Some(at) = markdown.get(from..).and_then(|rest| rest.find("&#")).map(|at| at + from) {
        let end = markdown.get(at..).and_then(|rest| rest.find(';')).map_or(at, |end| end + at);
        let scalar = markdown
            .get(at + 2..end)
            .and_then(|digits| digits.parse::<u32>().ok())
            .and_then(char::from_u32);
        if let Some(scalar) = scalar {
            let raw = format!(
                "{}{scalar}{}",
                markdown.get(..at).unwrap_or_default(),
                markdown.get(end + 1..).unwrap_or_default()
            );
            if read_back(&raw) == *expected {
                needless.push(raw);
            }
        }
        from = end + 1;
    }
    needless
}

proptest::proptest! {
    #[test]
    fn every_reference_is_needed_where_bold_and_italic_meet_ascii_and_letters(
        runs in proptest::collection::vec((r#"[a-c0-9é"(),.!?*_-]{1,3}"#, 0u8..4), 1..6),
    ) {
        let inlines: Vec<Inline> =
            runs.iter().map(|(text, bits)| styled(text, emphasis(*bits))).collect();
        let source: String = runs.iter().map(|(text, _)| text.as_str()).collect();
        let expected = (collapse_ws(source.trim()), marks_of(&inlines));
        let markdown = paragraph(inlines);
        proptest::prop_assert_eq!(&read_back(&markdown), &expected, "{:?}", markdown);
        let needless = needless_references(&markdown, &expected);
        proptest::prop_assert!(needless.is_empty(), "{:?} also reads back as {:?}", markdown, needless);
    }

    #[test]
    fn marks_beside_unicode_punctuation_symbols_and_marks_parse_back(
        runs in proptest::collection::vec(
            ("[a\u{e9}\u{ab}\u{bb}\u{201c}\u{201d}\u{2014}\u{a1}\u{bf}\u{20ac}\u{a9}\u{301}\u{200d}\u{1f600}\u{65e5}\u{24d0}\u{1f130}\u{a0}\"!~ ]{1,3}", 0u8..8),
            1..6,
        ),
    ) {
        let inlines: Vec<Inline> =
            runs.iter().map(|(text, bits)| styled(text, emphasis(*bits))).collect();
        let source: String = runs.iter().map(|(text, _)| text.as_str()).collect();
        let expected = (collapse_ws(source.trim()), marks_of(&inlines));
        let markdown = paragraph(inlines);
        proptest::prop_assert_eq!(read_back(&markdown), expected, "{:?}", markdown);
    }
}

#[test]
fn a_reference_on_a_one_character_run_is_judged_at_both_its_seams() {
    let cases = [
        (
            vec![Inline::plain("a"), styled("é", BOLD), styled("_", BOLD_ITALIC)],
            "&#97;**&#233;_\\__**",
        ),
        (
            vec![styled("a", ITALIC_STRIKE), styled("日", STRIKE), Inline::plain("é")],
            "~~_a_&#26085;~~&#233;",
        ),
    ];
    for (runs, expected) in cases {
        let markdown = paragraph(runs.clone());
        assert_eq!(markdown, format!("{expected}\n"));
        assert_eq!(read_back(&markdown).1, marks_of(&runs), "{markdown}");
    }
}
