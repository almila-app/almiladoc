use crate::build::{W, docx, docx_with};
use crate::support::{configured, count, fixture, reporting};
use almiladoc::{DiagnosticCode, Format, Input};

fn body_markdown(body: &str) -> String {
    let bytes = docx(body);
    let conversion = reporting().convert(Input::new(&bytes, Format::Docx)).unwrap();
    conversion.markdown().trim_end().to_owned()
}

fn markdown(run_content: &str) -> String {
    body_markdown(&format!("<w:p><w:r>{run_content}</w:r></w:p>"))
}

fn sym(font: &str, code: &str) -> String {
    format!(r#"<w:sym w:font="{font}" w:char="{code}"/><w:t>caption</w:t>"#)
}

#[test]
fn docx_symbol_checkboxes_render_their_state() {
    for (font, unchecked, checked) in [
        ("Wingdings 2", "00A3", "0052"),
        ("wingdings 2", "F0A3", "F052"),
        ("Wingdings 2", "A3", "52"),
        ("Wingdings", "006F", "00FE"),
        ("WINGDINGS", "F06F", "F0FE"),
        ("Wingdings", "6f", "fe"),
    ] {
        assert_eq!(markdown(&sym(font, checked)), "[x] caption", "{font} {checked}");
        assert_eq!(markdown(&sym(font, unchecked)), "[ ] caption", "{font} {unchecked}");
    }
}

#[test]
fn docx_checkbox_keeps_its_place_between_runs() {
    let content = r#"<w:t>Paid:</w:t><w:sym w:font="Wingdings 2" w:char="F052"/><w:t>yes</w:t><w:sym w:font="Wingdings 2" w:char="F0A3"/><w:t>no</w:t>"#;
    assert_eq!(markdown(content), "Paid:[x] yes[ ] no");
}

#[test]
fn docx_symbols_that_are_not_checkboxes_render_their_character_or_nothing() {
    for (attrs, drawn) in [
        (r#"w:font="Arial" w:char="0052""#, ""),
        (r#"w:font="Wingdings 2" w:char="00FE""#, ""),
        (r#"w:font="Wingdings" w:char="0052""#, "\u{263c}"),
        (r#"w:font="Wingdings 3" w:char="00FE""#, ""),
        (r#"w:font="Wingdings 3" w:char="F021""#, "\u{2b60}"),
        (r#"w:font="Webdings" w:char="0021""#, "\u{1f577}"),
        (r#"w:font="Zapf Dingbats" w:char="33""#, "\u{2713}"),
        (r#"w:font="Wingdings" w:char="F0FF""#, ""),
        (r#"w:font="Wingdings 2" w:char="0001""#, ""),
        (r#"w:font="Wingdings 2" w:char="F152""#, ""),
        (r#"w:font="Wingdings 2" w:char="EF52""#, ""),
        (r#"w:font="Wingdings 2" w:char="not-hex""#, ""),
        (r#"w:font="Wingdings 2" w:char="+A3""#, ""),
        (r#"w:font="Wingdings 2" w:char="+00A3""#, ""),
        (r#"w:font="Wingdings 2" w:char="000A3""#, ""),
        (r#"w:font="Wingdings 2" w:char="100A3""#, ""),
        (r#"w:font="Wingdings 2" w:char="""#, ""),
        (r#"w:font="Wingdings 2""#, ""),
        (r#"w:char="00A3""#, ""),
    ] {
        let content = format!("<w:t>before</w:t><w:sym {attrs}/><w:t>after</w:t>");
        assert_eq!(markdown(&content), format!("before{drawn}after"), "{attrs}");
    }
}

#[test]
fn docx_non_breaking_hyphens_render_in_the_run_style_and_soft_hyphens_stay_dropped() {
    for (label, content, expected) in [
        (
            "a non-breaking hyphen renders as a hyphen",
            "<w:t>Call 555</w:t><w:noBreakHyphen/><w:t>0123 by e</w:t><w:noBreakHyphen/><w:t>mail</w:t>",
            "Call 555-0123 by e-mail",
        ),
        (
            "a non-breaking hyphen keeps the run style",
            "<w:rPr><w:b/></w:rPr><w:t>e</w:t><w:noBreakHyphen/><w:t>mail</w:t>",
            "**e-mail**",
        ),
        (
            "a soft hyphen stays dropped",
            "<w:t>hyphen</w:t><w:softHyphen/><w:t>ation</w:t>",
            "hyphenation",
        ),
    ] {
        assert_eq!(markdown(content), expected, "{label}");
    }
}

#[test]
fn docx_heading_line_break_slugs_as_the_space_it_renders() {
    let body = r#"<w:p><w:hyperlink w:anchor="_Toc1"><w:r><w:t>go</w:t></w:r></w:hyperlink></w:p><w:p><w:pPr><w:outlineLvl w:val="0"/></w:pPr><w:bookmarkStart w:id="0" w:name="_Toc1"/><w:r><w:t>Part One</w:t><w:br/><w:t>Overview</w:t></w:r><w:bookmarkEnd w:id="0"/></w:p>"#;
    assert_eq!(body_markdown(body), "[go](#part-one-overview)\n\n# Part One Overview");
}

fn docx_with_styles(body: &str, styles: &str) -> Vec<u8> {
    let styles = format!("<w:styles {W}>{styles}</w:styles>");
    docx_with(body, &[("word/styles.xml", styles.as_bytes())])
}

fn reported(bytes: &[u8]) -> (String, u64) {
    let conversion = reporting().convert(Input::new(bytes, Format::Docx)).unwrap();
    let unmapped = count(conversion.diagnostics(), |d| d.code == DiagnosticCode::UnmappedSymbol);
    (conversion.markdown().trim_end().to_owned(), unmapped)
}

fn symbol_run(fonts: &str, text: &str) -> String {
    format!(
        r#"<w:p><w:r><w:rPr><w:rFonts {fonts}/></w:rPr><w:t xml:space="preserve">{text}</w:t></w:r></w:p>"#
    )
}

fn symbol_markdown(body: &str) -> (String, u64) {
    reported(&docx(body))
}

#[test]
fn docx_symbol_font_codes_read_as_their_letters() {
    let sym =
        |code: &str| format!(r#"<w:p><w:r><w:sym w:font="Symbol" w:char="{code}"/></w:r></w:p>"#);
    let run = |text: &str| symbol_run(r#"w:ascii="Symbol" w:hAnsi="Symbol""#, text);
    for body in [sym("F06D"), sym("006D"), sym("6d"), run("m"), run("\u{f06d}")] {
        assert_eq!(symbol_markdown(&body), ("\u{3bc}".to_owned(), 0), "{body}");
    }
    assert_eq!(
        symbol_markdown(&run("10 m\tm\u{3bc}")),
        ("10 \u{3bc}\t\u{3bc}\u{3bc}".to_owned(), 0)
    );
    assert_eq!(symbol_markdown(&run("\u{a0}\u{ad}")), ("\u{20ac}\u{2191}".to_owned(), 0));
}

#[test]
fn docx_symbol_codes_no_table_maps_are_dropped_and_counted() {
    let run = |text: &str| symbol_run(r#"w:ascii="Symbol" w:hAnsi="Symbol""#, text);
    assert_eq!(symbol_markdown(&run("a\u{f009}b`c")), ("\u{3b1}\u{3b2}\u{3c7}".to_owned(), 2));
    assert_eq!(symbol_markdown(&run("a\tb")), ("\u{3b1}\t\u{3b2}".to_owned(), 0));
    for code in ["0009", "03BC", "2022", "F10D", "zz", "1F06D", "F060", "F0F0", "00FF"] {
        let body = format!(
            r#"<w:p><w:r><w:t>a</w:t><w:sym w:font="Symbol" w:char="{code}"/><w:t>b</w:t></w:r></w:p>"#
        );
        assert_eq!(symbol_markdown(&body), ("ab".to_owned(), 1), "{code}");
    }
}

#[test]
fn docx_symbol_runs_fail_a_conversion_only_where_a_code_is_unmapped() {
    let converter = configured(|_| {});
    let bytes = docx(&symbol_run(r#"w:ascii="Symbol""#, "`"));
    let converted = converter.convert(Input::new(&bytes, Format::Docx));
    assert!(matches!(converted, Err(almiladoc::ConvertError::Incomplete { .. })));
    let bytes = docx(&symbol_run(r#"w:ascii="Symbol""#, "m"));
    let converted = converter.convert(Input::new(&bytes, Format::Docx)).unwrap();
    assert_eq!(converted.markdown().trim_end(), "\u{3bc}");
    let bytes = docx(&symbol_run(r#"w:ascii="Wingdings" w:hAnsi="Wingdings""#, "\u{fc}"));
    let converted = converter.convert(Input::new(&bytes, Format::Docx)).unwrap();
    assert_eq!(converted.markdown().trim_end(), "\u{2713}");
    let bytes = docx(&symbol_run(r#"w:ascii="Wingdings" w:hAnsi="Wingdings""#, "\u{ff}"));
    let converted = converter.convert(Input::new(&bytes, Format::Docx));
    assert!(matches!(converted, Err(almiladoc::ConvertError::Incomplete { .. })));
}

#[test]
fn docx_wingdings_tick_reads_as_a_check_mark_in_run_text_and_symbols() {
    let run = |text: &str| symbol_run(r#"w:ascii="Wingdings" w:hAnsi="Wingdings""#, text);
    let sym = r#"<w:p><w:r><w:sym w:font="Wingdings" w:char="F0FC"/></w:r></w:p>"#;
    for body in [run("\u{f0fc}"), run("\u{fc}"), sym.to_owned()] {
        assert_eq!(symbol_markdown(&body), ("\u{2713}".to_owned(), 0), "{body}");
    }
}

#[test]
fn docx_dingbat_runs_draw_their_boxes_and_characters() {
    let wingdings = |text: &str| symbol_run(r#"w:ascii="Wingdings" w:hAnsi="Wingdings""#, text);
    assert_eq!(
        symbol_markdown(&wingdings("\u{fe}Dan")),
        ("[x] \u{1f44e}\u{264b}\u{25fc}".to_owned(), 0)
    );
    assert_eq!(
        symbol_markdown(&wingdings("a b \u{2713}")),
        ("\u{264b} \u{264c} \u{2713}".to_owned(), 0)
    );
    assert_eq!(symbol_markdown(&wingdings("\u{ff}\u{f0ff}")), (String::new(), 2));
    let wingdings2 = symbol_run(r#"w:ascii="Wingdings 2" w:hAnsi="Wingdings 2""#, "R\u{a3}");
    assert_eq!(symbol_markdown(&wingdings2), ("[x] [ ]".to_owned(), 0));
    let ascii_only = symbol_run(r#"w:ascii="Wingdings""#, "J");
    assert_eq!(symbol_markdown(&ascii_only), ("\u{263a}".to_owned(), 0));
}

#[test]
fn docx_each_character_takes_the_font_of_its_slot() {
    let both = r#"w:ascii="Times New Roman" w:hAnsi="Symbol""#;
    assert_eq!(symbol_markdown(&symbol_run(both, "m\u{f06d}")), ("m\u{3bc}".to_owned(), 0));
    let hinted = r#"w:ascii="Arial" w:hAnsi="Arial" w:eastAsia="Symbol" w:hint="eastAsia""#;
    assert_eq!(
        symbol_markdown(&symbol_run(hinted, "\u{b1}\u{e9}\u{f06d}m")),
        ("\u{b1}\u{e9}\u{3bc}m".to_owned(), 0)
    );
    let unhinted = r#"w:ascii="Arial" w:hAnsi="Arial" w:eastAsia="Symbol""#;
    assert_eq!(symbol_markdown(&symbol_run(unhinted, "\u{f06d}")), ("\u{f06d}".to_owned(), 0));
    let default_east_asia =
        r#"w:ascii="Symbol" w:hAnsi="Symbol" w:eastAsia="Times New Roman" w:hint="eastAsia""#;
    assert_eq!(symbol_markdown(&symbol_run(default_east_asia, "\u{b4}")), ("\u{d7}".to_owned(), 0));
    let other_east_asia =
        r#"w:ascii="Symbol" w:hAnsi="Symbol" w:eastAsia="Arial" w:hint="eastAsia""#;
    assert_eq!(symbol_markdown(&symbol_run(other_east_asia, "\u{b4}")), ("\u{b4}".to_owned(), 0));
    let themed = r#"w:ascii="Symbol" w:asciiTheme="minorHAnsi" w:hAnsi="Symbol""#;
    assert_eq!(symbol_markdown(&symbol_run(themed, "m\u{e9}")), ("m\u{23a1}".to_owned(), 0));
    for complex in ["<w:cs/>", "<w:rtl/>"] {
        let body = format!(
            r#"<w:p><w:r><w:rPr><w:rFonts w:ascii="Symbol"/>{complex}</w:rPr><w:t>m</w:t></w:r></w:p>"#
        );
        assert_eq!(symbol_markdown(&body), ("m".to_owned(), 0), "{complex}");
    }
}

#[test]
fn docx_symbol_fonts_reach_a_run_through_every_level_of_the_hierarchy() {
    let symbol_rpr = r#"<w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr>"#;
    let defaults =
        format!("<w:docDefaults><w:rPrDefault>{symbol_rpr}</w:rPrDefault></w:docDefaults>");
    let char_style = format!(
        r#"<w:style w:type="character" w:styleId="Base">{symbol_rpr}</w:style><w:style w:type="character" w:styleId="Sym"><w:basedOn w:val="Base"/></w:style>"#
    );
    let para_style = format!(r#"<w:style w:type="paragraph" w:styleId="P">{symbol_rpr}</w:style>"#);
    let default_para = format!(
        r#"<w:style w:type="paragraph" w:default="1" w:styleId="Normal">{symbol_rpr}</w:style>"#
    );
    let times = r#"<w:rFonts w:ascii="Times New Roman" w:hAnsi="Times New Roman"/>"#;
    let cases = [
        (
            r#"<w:p><w:r><w:rPr><w:rStyle w:val="Sym"/></w:rPr><w:t>m</w:t></w:r></w:p>"#,
            &char_style,
        ),
        (r#"<w:p><w:pPr><w:pStyle w:val="P"/></w:pPr><w:r><w:t>m</w:t></w:r></w:p>"#, &para_style),
        ("<w:p><w:r><w:t>m</w:t></w:r></w:p>", &defaults),
        ("<w:p><w:r><w:t>m</w:t></w:r></w:p>", &default_para),
        (
            r#"<w:p><w:pPr><w:pStyle w:val="Missing"/></w:pPr><w:r><w:t>m</w:t></w:r></w:p>"#,
            &default_para,
        ),
    ];
    for (body, styles) in cases {
        assert_eq!(reported(&docx_with_styles(body, styles)), ("\u{3bc}".to_owned(), 0), "{body}");
        let direct = body.replace("<w:r><w:rPr>", &format!("<w:r><w:rPr>{times}"));
        let direct = if direct == body {
            body.replace("<w:r>", &format!("<w:r><w:rPr>{times}</w:rPr>"))
        } else {
            direct
        };
        assert_eq!(reported(&docx_with_styles(&direct, styles)), ("m".to_owned(), 0), "{direct}");
    }
}

#[test]
fn docx_the_default_paragraph_style_sits_between_document_defaults_and_the_run() {
    let styles = r#"<w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Symbol"/></w:rPr></w:rPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:rPr><w:rFonts w:ascii="Arial"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Plain"/>"#;
    let plain = "<w:p><w:r><w:t>m</w:t></w:r></w:p>";
    assert_eq!(reported(&docx_with_styles(plain, styles)), ("m".to_owned(), 0));
    let named = r#"<w:p><w:pPr><w:pStyle w:val="Plain"/></w:pPr><w:r><w:t>m</w:t></w:r></w:p>"#;
    assert_eq!(reported(&docx_with_styles(named, styles)), ("\u{3bc}".to_owned(), 0));
}

#[test]
fn docx_symbol_loss_counts_only_where_its_text_shows() {
    let lost = r#"<w:r><w:rPr><w:rFonts w:ascii="Symbol"/></w:rPr><w:t>`</w:t></w:r>"#;
    let begin = r#"<w:r><w:fldChar w:fldCharType="begin"/></w:r>"#;
    let separate = r#"<w:r><w:fldChar w:fldCharType="separate"/></w:r>"#;
    let end = r#"<w:r><w:fldChar w:fldCharType="end"/></w:r>"#;
    let sym = r#"<w:r><w:sym w:font="Symbol" w:char="F060"/></w:r>"#;
    let cases = [
        (format!("<w:p>{begin}{lost}{sym}{separate}<w:r><w:t>x</w:t></w:r>{end}</w:p>"), 0),
        (
            format!(
                "<w:p>{begin}<w:r><w:instrText>A</w:instrText></w:r>{begin}{lost}{separate}{lost}{end}{separate}<w:r><w:t>x</w:t></w:r>{end}</w:p>"
            ),
            0,
        ),
        (format!("<w:p>{begin}{separate}{lost}{sym}{end}</w:p>"), 2),
        (format!(r#"<w:p><w:hyperlink w:anchor="a">{lost}</w:hyperlink></w:p>"#), 1),
        (
            format!(
                r#"<w:p>{begin}<w:hyperlink w:anchor="a">{lost}</w:hyperlink>{separate}{end}</w:p>"#
            ),
            0,
        ),
        (
            format!(
                r#"<w:p>{begin}<w:fldSimple w:instr="Q">{sym}</w:fldSimple>{separate}{end}</w:p>"#
            ),
            0,
        ),
    ];
    for (body, expected) in cases {
        assert_eq!(symbol_markdown(&body).1, expected, "{body}");
    }
}

#[test]
fn docx_symbols_read_inside_links_fields_notes_and_alternate_content() {
    let sym = r#"<w:r><w:sym w:font="Symbol" w:char="F06D"/></w:r>"#;
    let hyperlink = format!(r#"<w:p><w:hyperlink w:anchor="a">{sym}</w:hyperlink></w:p>"#);
    assert_eq!(symbol_markdown(&hyperlink), ("\u{3bc}".to_owned(), 0));
    let field = format!(r#"<w:p><w:fldSimple w:instr="QUOTE">{sym}</w:fldSimple></w:p>"#);
    assert_eq!(symbol_markdown(&field), ("\u{3bc}".to_owned(), 0));
    let alternate = r#"<w:p xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><w:r><mc:AlternateContent><mc:Fallback><w:sym w:font="Symbol" w:char="F06D"/></mc:Fallback></mc:AlternateContent></w:r></w:p>"#;
    assert_eq!(symbol_markdown(alternate), ("\u{3bc}".to_owned(), 0));
    let footnotes = format!(
        r#"<w:footnotes {W}><w:footnote w:id="1"><w:p>{sym}</w:p></w:footnote></w:footnotes>"#
    );
    let document = r#"<w:p><w:r><w:t>a</w:t><w:footnoteReference w:id="1"/></w:r></w:p>"#;
    let bytes = docx_with(document, &[("word/footnotes.xml", footnotes.as_bytes())]);
    let (markdown, unmapped) = reported(&bytes);
    assert!(markdown.contains("\u{3bc}"), "{markdown}");
    assert_eq!(unmapped, 0);
}

#[test]
fn doc_symbol_fonts_read_as_rtf_reads_them() {
    let markdown = |path: &str, format| {
        let conversion = reporting().convert(Input::new(&fixture(path), format)).unwrap();
        conversion.markdown().to_owned()
    };
    let doc = markdown("doc/handmade-scientific.doc", Format::Doc);
    assert_eq!(doc, markdown("rtf/handmade-scientific.rtf", Format::Rtf));
    assert!(doc.contains("[x] Calibrated [ ] Replicated"), "{doc}");
}
