use almiladoc::{Config, Converter, Format, Input, OnLoss};
use std::error::Error;
use std::io::{Cursor, Write};

type TestResult = Result<(), Box<dyn Error>>;

const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

fn docx(body: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let document = format!(r#"<w:document {W}><w:body>{body}</w:body></w:document>"#);
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("word/document.xml", zip::write::SimpleFileOptions::default())?;
    zip.write_all(document.as_bytes())?;
    Ok(zip.finish()?.into_inner())
}

fn body_markdown(body: &str) -> Result<String, Box<dyn Error>> {
    let bytes = docx(body)?;
    let mut config = Config::default();
    config.on_loss = OnLoss::Report;
    let converter = Converter::new(config)?;
    let conversion = converter.convert(Input::new(&bytes, Format::Docx))?;
    Ok(conversion.markdown().trim_end().to_owned())
}

fn markdown(run_content: &str) -> Result<String, Box<dyn Error>> {
    body_markdown(&format!("<w:p><w:r>{run_content}</w:r></w:p>"))
}

fn sym(font: &str, code: &str) -> String {
    format!(r#"<w:sym w:font="{font}" w:char="{code}"/><w:t>caption</w:t>"#)
}

#[test]
fn docx_symbol_checkboxes_render_their_state() -> TestResult {
    for (font, unchecked, checked) in [
        ("Wingdings 2", "00A3", "0052"),
        ("wingdings 2", "F0A3", "F052"),
        ("Wingdings 2", "A3", "52"),
        ("Wingdings", "006F", "00FE"),
        ("WINGDINGS", "F06F", "F0FE"),
        ("Wingdings", "6f", "fe"),
    ] {
        assert_eq!(markdown(&sym(font, checked))?, "[x] caption", "{font} {checked}");
        assert_eq!(markdown(&sym(font, unchecked))?, "[ ] caption", "{font} {unchecked}");
    }
    Ok(())
}

#[test]
fn docx_checkbox_keeps_its_place_between_runs() -> TestResult {
    let content = r#"<w:t>Paid:</w:t><w:sym w:font="Wingdings 2" w:char="F052"/><w:t>yes</w:t><w:sym w:font="Wingdings 2" w:char="F0A3"/><w:t>no</w:t>"#;
    assert_eq!(markdown(content)?, "Paid:[x] yes[ ] no");
    Ok(())
}

#[test]
fn docx_symbols_that_are_not_checkboxes_render_nothing() -> TestResult {
    for attrs in [
        r#"w:font="Arial" w:char="0052""#,
        r#"w:font="Wingdings 2" w:char="00FE""#,
        r#"w:font="Wingdings" w:char="0052""#,
        r#"w:font="Wingdings 3" w:char="00FE""#,
        r#"w:font="Wingdings 2" w:char="0001""#,
        r#"w:font="Wingdings 2" w:char="F152""#,
        r#"w:font="Wingdings 2" w:char="EF52""#,
        r#"w:font="Wingdings 2" w:char="not-hex""#,
        r#"w:font="Wingdings 2" w:char="+A3""#,
        r#"w:font="Wingdings 2" w:char="+00A3""#,
        r#"w:font="Wingdings 2" w:char="000A3""#,
        r#"w:font="Wingdings 2" w:char="100A3""#,
        r#"w:font="Wingdings 2" w:char="""#,
        r#"w:font="Wingdings 2""#,
        r#"w:char="00A3""#,
    ] {
        let content = format!(r#"<w:t>before</w:t><w:sym {attrs}/><w:t>after</w:t>"#);
        assert_eq!(markdown(&content)?, "beforeafter", "{attrs}");
    }
    Ok(())
}

#[test]
fn docx_non_breaking_hyphen_renders_as_a_hyphen() -> TestResult {
    let content = r#"<w:t>Call 555</w:t><w:noBreakHyphen/><w:t>0123 by e</w:t><w:noBreakHyphen/><w:t>mail</w:t>"#;
    assert_eq!(markdown(content)?, "Call 555-0123 by e-mail");
    Ok(())
}

#[test]
fn docx_non_breaking_hyphen_keeps_the_run_style() -> TestResult {
    let content = r#"<w:rPr><w:b/></w:rPr><w:t>e</w:t><w:noBreakHyphen/><w:t>mail</w:t>"#;
    assert_eq!(markdown(content)?, "**e-mail**");
    Ok(())
}

#[test]
fn docx_soft_hyphen_stays_dropped() -> TestResult {
    let content = r#"<w:t>hyphen</w:t><w:softHyphen/><w:t>ation</w:t>"#;
    assert_eq!(markdown(content)?, "hyphenation");
    Ok(())
}

#[test]
fn docx_heading_line_break_slugs_as_the_space_it_renders() -> TestResult {
    let body = r#"<w:p><w:hyperlink w:anchor="_Toc1"><w:r><w:t>go</w:t></w:r></w:hyperlink></w:p><w:p><w:pPr><w:outlineLvl w:val="0"/></w:pPr><w:bookmarkStart w:id="0" w:name="_Toc1"/><w:r><w:t>Part One</w:t><w:br/><w:t>Overview</w:t></w:r><w:bookmarkEnd w:id="0"/></w:p>"#;
    assert_eq!(body_markdown(body)?, "[go](#part-one-overview)\n\n# Part One Overview");
    Ok(())
}
