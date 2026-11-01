//! Tests of the master content a slide writes as its header under `HeaderFooterPolicy::Emit`.

use super::*;

fn emitted(bytes: &[u8]) -> (String, Vec<Reported>) {
    let limits = Limits::default();
    let ctx = ParseCtx::new(&limits).with_headers_footers(HeaderFooterPolicy::Emit);
    let document = parse(&ctx, bytes).unwrap();
    (markdown_of_document(&document), diagnostic_rows(&ctx.into_diagnostics()))
}

/// A master object holding `text` in a client text box.
fn texted(text: &str) -> Vec<u8> {
    let textbox = container(0, 0xF00D, &[atom32(0x0F9F, &[4]), record(0, 0x0FA8, text.as_bytes())]);
    container(0, 0xF004, &[record(2, 0xF00A, &[0, 0, 0, 0, 0, 0x0A, 0, 0]), textbox])
}

fn header(text: &str) -> String {
    let body = if text.is_empty() { String::new() } else { format!(">\n> {text}\n") };
    format!("> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n{body}")
}

#[test]
fn a_slide_writes_the_master_content_it_shows_and_counts_master_pictures_unplaced() {
    let unplaced = |count| vec![(DiagnosticCode::ImageUnplaced, Region::Header, Some(1), count)];
    let nested = container(0, 0xF003, &[shape(0x0005, &[], &[]), texted("One"), texted("Two")]);
    let paragraph = [&5u32.to_le_bytes()[..], &[0, 0], &0u32.to_le_bytes()].concat();
    let style = record(0, 0x0FA1, &[paragraph, char_run(5, 0x0001, &[0x0001])].concat());
    let bold = container(0, 0xF00D, &[atom32(0x0F9F, &[4]), record(0, 0x0FA8, b"Bold"), style]);
    let bold = container(0, 0xF004, &[record(2, 0xF00A, &[0, 0, 0, 0, 0, 0x0A, 0, 0]), bold]);
    let cases: Vec<(&str, Vec<u8>, String, Vec<Reported>)> = vec![
        ("styled master text", mastered(&[bold], 0, 1), header("**Bold**"), vec![]),
        (
            "master text the slide shows",
            mastered(&[texted("Logo"), placeholder()], 0, 1),
            header("Logo"),
            vec![],
        ),
        ("master text the slide hides", mastered(&[texted("Logo")], 0, 0), String::new(), vec![]),
        (
            "master text in a nested group, in source order",
            mastered(&[nested], 0, 1),
            [header("One"), ">\n> Two\n".into()].concat(),
            vec![],
        ),
        (
            "a master picture the slide shows",
            mastered(&[picture(1)], 0, 1),
            header(""),
            unplaced(1),
        ),
        ("a master picture the slide hides", mastered(&[picture(1)], 0, 0), String::new(), vec![]),
        ("a master picture background", mastered(&[], 3, 4), header(""), unplaced(1)),
        (
            "a main master's text its title master shows",
            titled(1, master(&[texted("Main")], 0), title_master(1, 1, &[texted("Title")], &[])),
            [header("Title"), ">\n> Main\n".into()].concat(),
            vec![],
        ),
        (
            "a main master's text its title master hides",
            titled(1, master(&[texted("Main")], 0), title_master(1, 0, &[texted("Title")], &[])),
            header("Title"),
            vec![],
        ),
        (
            "a slide naming an unknown master shows the first main master's",
            listed(
                &[pages(0, &[(2, 256)]), pages(1, &[(3, 1)])],
                &[slide_on(9, 1, &[]), master(&[texted("First")], 0)],
            ),
            header("First"),
            vec![],
        ),
    ];
    for (name, bytes, markdown, rows) in cases {
        assert_eq!(emitted(&bytes), (markdown, rows), "{name}");
    }
}
