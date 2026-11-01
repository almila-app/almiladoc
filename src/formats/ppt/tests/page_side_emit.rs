//! Tests of the headers and footers a page writes under `HeaderFooterPolicy::Emit`.

use super::*;
use DiagnosticCode::{FieldToken, InvalidEncoding};
use Region::Footer;

fn emitted(bytes: &[u8]) -> (String, Vec<Reported>) {
    let limits = Limits::default();
    let ctx = ParseCtx::new(&limits).with_headers_footers(HeaderFooterPolicy::Emit);
    let document = parse(&ctx, bytes).unwrap();
    (markdown_of_document(&document), diagnostic_rows(&ctx.into_diagnostics()))
}

fn label(n: u32, side: &str, title: &str) -> String {
    format!("> <a id=\"almiladoc:{side}:default:{n}\"></a>{title} (default):")
}

#[test]
fn a_slide_writes_the_footer_its_deck_or_own_settings_or_placeholders_show() {
    let two_slides = pages(0, &[(2, 256), (3, 257)]);
    let deck_footer = headers_footers(3, TODAY | FOOTER | SLIDE_NUMBER, &FOOTER_TEXT);
    let cases: Vec<(&str, Vec<u8>, String, Vec<Reported>)> = vec![
        (
            "the deck's footer, then a slide that turns it off",
            listed(
                &[two_slides.clone(), deck_footer.clone()],
                &[slide(0, &[]), slide(0, &[headers_footers(0, 0, &[])])],
            ),
            format!(
                "{}\n>\n> {{date}}\n>\n> Confidential\n>\n> {{page}}\n\n---\n\n{}\n",
                label(1, "footer", "Footer"),
                label(2, "footer", "Footer")
            ),
            vec![(FieldToken, Footer, Some(1), 2)],
        ),
        (
            "the same footer on the next slide",
            listed(&[two_slides, deck_footer], &[slide(0, &[]), slide(0, &[])]),
            format!(
                "{}\n>\n> {{date}}\n>\n> Confidential\n>\n> {{page}}\n",
                label(1, "footer", "Footer")
            ),
            vec![(FieldToken, Footer, Some(1), 2)],
        ),
        (
            "a user date and drawn slide number",
            on_slide(&[
                headers_footers(0, USER_DATE, &[(0, "May 2026")]),
                drawing(&[drawn(8, "No. *")]),
            ]),
            format!("{}\n>\n> May 2026\n>\n> No. {{page}}\n", label(1, "footer", "Footer")),
            vec![(FieldToken, Footer, Some(1), 1)],
        ),
        (
            "a footer string that does not decode",
            on_slide(&[page_side::unpaired_footer()]),
            format!("{}\n>\n> \u{fffd}\n", label(1, "footer", "Footer")),
            vec![(InvalidEncoding, Footer, Some(1), 1)],
        ),
    ];
    for (name, bytes, markdown, rows) in cases {
        let (rendered, reported) = emitted(&bytes);
        assert_eq!(rendered, markdown, "{name}");
        assert_eq!(reported, rows, "{name}");
    }
}

#[test]
fn a_notes_page_writes_its_header_inside_its_quote() {
    let bytes = on_notes(&[headers_footers(4, HEADER, &[(1, "Draft")])], &[]);
    let (rendered, reported) = emitted(&bytes);
    assert_eq!(rendered, format!("> {}\n> >\n> > Draft\n", label(1, "header", "Header")));
    assert_eq!(reported, []);
    let slide_header = flagged(HEADER, &[(1, "ignored")]);
    assert_eq!(emitted(&slide_header), (String::new(), Vec::new()));
}
