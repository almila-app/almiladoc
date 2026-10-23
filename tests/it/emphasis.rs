//! Emphasis read from Word runs, and Markdown that a parser reads back with the same text and
//! marks.

use crate::style::{docx, markdown, para, run};
use almiladoc::Format;

fn word(body: &str) -> String {
    markdown(&docx("", body), Format::Docx)
}

#[test]
fn quoted_bold_beside_letters_reads_back_bold_in_a_paragraph_and_a_cell() {
    let runs = [run("", "a"), run("<w:b/>", "\"foo\""), run("", "b")].concat();
    assert_eq!(word(&para("", &runs)), "&#97;**\"foo\"**&#98;\n");
    let cell = format!("<w:tbl><w:tr><w:tc>{}</w:tc></w:tr></w:tbl>", para("", &runs));
    assert_eq!(word(&cell), "|  |\n| --- |\n| &#97;**\"foo\"**&#98; |\n");
}

#[test]
fn neighbouring_runs_share_their_common_marks() {
    let runs = [
        run("<w:b/>", "Note "),
        run("<w:b/><w:i/>", "key term"),
        run("<w:b/>", " here."),
        run("<w:strike/><w:i/>", " -"),
        run("<w:strike/>", "~"),
    ]
    .concat();
    assert_eq!(word(&para("", &runs)), "**Note _key term_ here.** ~~_-_\\~~~\n");
}

#[test]
fn bold_across_line_breaks_closes_before_each_and_reopens_after() {
    let breaks =
        "<w:r><w:rPr><w:b/></w:rPr><w:t>a</w:t><w:br/><w:t>b</w:t><w:br/><w:br/><w:t>c</w:t></w:r>";
    assert_eq!(word(&format!("<w:p>{breaks}</w:p>")), "**a**\\\n**b**\\\n\n**c**\n");
}
