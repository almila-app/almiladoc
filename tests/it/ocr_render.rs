use std::assert_matches;
use std::time::Duration;

use crate::bounds::least_work;
use crate::support::{configured, converter, limit_mut, limited, trips};
use almiladoc::{CancelToken, ConvertError, GapFill, Limit, OcrLine};

const SPANS: &str = "<ched>N<ched>S<lcel><nl><ucel><ched>M<ched>D<nl><fcel>a<fcel>1<fcel>2<nl>";
const MERGED: &str = "<fcel>Sample<lcel><nl><fcel>A<fcel>12.4<nl>";

fn markdown(text: &str) -> GapFill {
    GapFill::Markdown(text.to_owned())
}

fn lines(text: &[(&str, [f64; 4])]) -> GapFill {
    GapFill::Lines(text.iter().map(|&(text, bounds)| OcrLine::new(text, bounds).unwrap()).collect())
}

fn columns() -> GapFill {
    lines(&[
        ("R2", [0.6, 0.17, 1.0, 0.22]),
        ("L1", [0.0, 0.1, 0.4, 0.15]),
        ("R1", [0.6, 0.1, 1.0, 0.15]),
        ("L2", [0.0, 0.17, 0.4, 0.22]),
        ("# *e\u{301}*", [0.0, 0.02, 1.0, 0.07]),
    ])
}

fn rendered(fill: GapFill) -> String {
    configured(|_| {}).render_ocr(fill, None).unwrap().into_markdown()
}

#[cfg(feature = "pdf")]
#[test]
fn a_fill_renders_alone_as_it_renders_in_the_page_it_fills() {
    use crate::support::fixture;
    use almiladoc::{Format, Input};

    let bytes = fixture("pdf/handmade-mixed.pdf");
    let source = format!("Rate \\(x_1\\), $y$ and\n\n\\[a=b \\tag{{1}}\\]\n\n{SPANS}\n\nTable 1.");
    for fill in [markdown(&source), GapFill::MarkdownTables(source.clone()), columns()] {
        let converter = configured(|_| {});
        let mut parsed = converter.parse(Input::new(&bytes, Format::Pdf)).unwrap();
        let gap = parsed.gaps().next().unwrap().id;
        parsed.fill_gap(gap, fill.clone()).unwrap();
        let page = parsed.render().unwrap();
        let span = page.units().last().unwrap().bytes.clone();
        let alone = converter.render_ocr(fill, None).unwrap();
        assert_eq!(alone.markdown(), format!("{}\n", page.markdown().get(span).unwrap()));
        assert!(alone.units().is_empty() && alone.diagnostics().is_empty());
        assert!(alone.is_complete());
    }
}

#[test]
fn each_fill_kind_renders_its_math_tables_and_columns() {
    let source = format!("At \\(x_1\\)\n\n\\[a=b \\tag{{1}}\\]\n\n{MERGED}");
    let flat = "At $x_1$\n\n$$\n    a=b \\tag{1}\n$$\n\n|  |  |\n| --- | --- |\n| Sample |  |\n| A | 12.4 |\n";
    assert_eq!(rendered(markdown(&source)), flat);
    let merged =
        "<table><tr><td colspan=\"2\">Sample</td></tr><tr><td>A</td><td>12.4</td></tr></table>";
    assert_eq!(
        rendered(GapFill::MarkdownTables(source)),
        format!("At $x_1$\n\n$$\n    a=b \\tag{{1}}\n$$\n\n{merged}\n")
    );
    let headed = rendered(GapFill::MarkdownTables(SPANS.to_owned()));
    assert!(headed.contains("<th rowspan=\"2\">N</th><th colspan=\"2\">S</th>"), "{headed}");
    assert_eq!(rendered(columns()), "\\# \\*é*\n\nL1 L2\n\nR1 R2\n");
}

#[test]
fn raw_html_and_links_come_back_as_escaped_text_and_labels() {
    let source = "[site](https://example.test/x) ![alt text](https://example.test/i.png) \
                  <img src=x onerror=y>\n\n<script>alert(1)</script>";
    for fill in [markdown(source), GapFill::MarkdownTables(source.to_owned())] {
        let output = rendered(fill);
        assert!(output.starts_with("site alt text \\<img src=x onerror=y>\n"), "{output}");
        assert!(output.ends_with("\\<script>alert(1)\\</script>\n"), "{output}");
        assert!(!output.contains("example.test"), "{output}");
        for options in [pulldown_cmark::Options::empty(), pulldown_cmark::Options::ENABLE_MATH] {
            let active = pulldown_cmark::Parser::new_ext(&output, options).any(|event| {
                matches!(
                    event,
                    pulldown_cmark::Event::Html(_)
                        | pulldown_cmark::Event::InlineHtml(_)
                        | pulldown_cmark::Event::Start(
                            pulldown_cmark::Tag::Link { .. } | pulldown_cmark::Tag::Image { .. }
                        )
                )
            });
            assert!(!active, "{output}");
        }
    }
}

#[test]
fn an_empty_or_blank_fill_renders_no_markdown_at_all() {
    let blank = [" ", "\n\n", " \t\n \n"];
    for text in std::iter::once("").chain(blank) {
        assert_eq!(rendered(markdown(text)), "", "{text:?}");
        assert_eq!(rendered(GapFill::MarkdownTables(text.to_owned())), "", "{text:?}");
    }
    assert_eq!(rendered(GapFill::Lines(Vec::new())), "");
    assert_eq!(rendered(lines(&[("  ", [0.0, 0.0, 1.0, 0.1])])), "");
}

#[test]
fn a_fill_past_a_limit_is_refused_naming_it_and_passes_at_the_limit() {
    let grid = format!("|a|b\n|-|-\n{}", "a\n".repeat(9));
    let colspan = r#"<table><tr><td colspan="3">x</td></tr></table>"#;
    let cases = [
        (markdown("123456789"), Limit::OcrTextBytes, 9),
        (lines(&[("123456789", [0.0, 0.0, 1.0, 0.1])]), Limit::OcrTextBytes, 41),
        (markdown("> > > a"), Limit::NestingDepth, 3),
        (markdown(&grid), Limit::GridSlots, 30),
        (GapFill::MarkdownTables(grid.clone()), Limit::GridSlots, 30),
        (markdown("<fcel>a<lcel><lcel><nl>"), Limit::Expansion, 2),
        (GapFill::MarkdownTables(colspan.into()), Limit::Expansion, 2),
        (GapFill::MarkdownTables("<table><tr><td>x</td></tr></table>".into()), Limit::XmlNodes, 4),
        (markdown("abcdefghij"), Limit::OutputBytes, 11),
    ];
    for (fill, limit, least) in cases {
        let at =
            |max| limited(|limits| *limit_mut(limits, limit) = max).render_ocr(fill.clone(), None);
        assert!(trips(&at(least - 1), limit, least - 1), "{limit}");
        at(least).unwrap();
    }
    let starved = limited(|limits| (limits.max_ocr_text_bytes, limits.max_work) = (8, 1));
    let refused = starved.render_ocr(markdown("123456789"), None);
    assert!(trips(&refused, Limit::OcrTextBytes, 8), "{refused:?}");
}

#[test]
fn work_is_charged_for_the_read_and_the_render_and_fails_the_call() {
    let fill = markdown("a *b* \\(c\\)\n\n| x |\n| - |\n| 1 |");
    let least = least_work(|converter| converter.render_ocr(fill.clone(), None).is_ok());
    assert_eq!(least, 103);
    let at = |max| limited(|limits| limits.max_work = max).render_ocr(fill.clone(), None);
    assert!(trips(&at(least - 1), Limit::Work, least - 1));
    assert_eq!(at(least).unwrap().markdown(), "a _b_ $c$\n\n| x |\n| --- |\n| 1 |\n");
}

#[test]
fn a_mebibyte_of_math_or_tag_bomb_exhausts_a_small_work_limit_every_time() {
    let bombs = ["\\( ".repeat(349_525), "<fcel>".repeat(174_762), "<fcel>a<lcel>".repeat(80_659)];
    let small = limited(|limits| limits.max_work = 10_000);
    for bomb in bombs {
        assert!(bomb.len() <= 1 << 20);
        for fill in [markdown(&bomb), GapFill::MarkdownTables(bomb.clone())] {
            let first = small.render_ocr(fill.clone(), None);
            assert!(trips(&first, Limit::Work, 10_000), "{first:?}");
            assert_eq!(small.render_ocr(fill, None), first);
        }
    }
}

#[test]
fn table_markup_the_reader_refuses_is_malformed_and_names_no_input() {
    let sentinel = "OCR-SENTINEL-4b1d";
    let refused = [
        format!("<table><tr><td onclick=\"{sentinel}\">x</td></tr></table>"),
        format!("> <table><tr><td>{sentinel}</td></tr></table>"),
        format!("<table><tr><td>{sentinel}</tr></table>"),
    ];
    for source in refused {
        let error =
            configured(|_| {}).render_ocr(GapFill::MarkdownTables(source), None).unwrap_err();
        assert_matches!(error, ConvertError::Malformed { .. });
        assert!(!format!("{error} {error:?}").contains(sentinel), "{error}");
    }
}

#[test]
fn a_cancelled_token_is_refused_before_the_fill_is_sized() {
    let token = CancelToken::new();
    token.cancel();
    let small = limited(|limits| limits.max_ocr_text_bytes = 4);
    for fill in [markdown("longer than four"), GapFill::MarkdownTables("longer".into()), columns()]
    {
        assert_eq!(small.render_ocr(fill, Some(&token)), Err(ConvertError::Cancelled));
    }
}

#[test]
fn a_token_cancelled_during_a_large_fill_stops_it() {
    let large = limited(|limits| {
        limits.max_ocr_text_bytes = 32 << 20;
        limits.max_work = u64::MAX;
        limits.max_output_bytes = u64::MAX;
    });
    let text = "word *emphasis* \\(x_1\\) [l](https://example.test)\n\n".repeat(160_000);
    for fill in [markdown(&text), GapFill::MarkdownTables(text.clone())] {
        let token = CancelToken::new();
        let result = std::thread::scope(|scope| {
            let running = scope.spawn(|| large.render_ocr(fill, Some(&token)));
            std::thread::sleep(Duration::from_millis(50));
            token.cancel();
            running.join().unwrap()
        });
        assert_eq!(result, Err(ConvertError::Cancelled));
    }
}

#[test]
fn equal_fills_render_equally_across_calls_and_thread_counts() {
    let source = format!("Rate \\(x_1\\)\n\n{SPANS}\n\n- a\n- b `c`\n\n| x |\n| - |\n| 1 |");
    for fill in [markdown(&source), GapFill::MarkdownTables(source.clone()), columns()] {
        let reference = converter(1).render_ocr(fill.clone(), None).unwrap();
        for threads in [1, 4, 1, 4] {
            assert_eq!(converter(threads).render_ocr(fill.clone(), None), Ok(reference.clone()));
        }
    }
}
