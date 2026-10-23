#![no_main]

use almiladoc::Format;
use almiladoc_fuzz::{archive, convert, fuzz_target};
use zip::write::SimpleFileOptions;

// A number format code, carried into the engine on a minimal workbook. The
// grammar is where the parsing complexity lives, and reaching it through a
// discovered styles part would cost the fuzzer most of its budget. Only the
// control characters XML forbids are dropped, so whitespace a code may carry
// still reaches the parser.
fuzz_target!(|data: &[u8]| {
    let code = String::from_utf8_lossy(data);
    let mut escaped = String::new();
    for c in code.chars().filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r')) {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            c => escaped.push(c),
        }
    }
    let styles = format!(
        r#"<?xml version="1.0"?><styleSheet xmlns="{SML}"><numFmts count="1"><numFmt numFmtId="164" formatCode="{escaped}"/></numFmts><cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="164" applyNumberFormat="1"/></cellXfs></styleSheet>"#
    );

    let parts: [(&str, &[u8]); 6] = [
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("xl/workbook.xml", WORKBOOK.as_bytes()),
        ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS.as_bytes()),
        ("xl/styles.xml", styles.as_bytes()),
        ("xl/worksheets/sheet1.xml", SHEET.as_bytes()),
    ];
    let Ok(bytes) = archive(&parts, SimpleFileOptions::default()) else {
        return;
    };
    convert(&bytes, Format::Excel);
});

const SML: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const CONTENT_TYPES: &str = r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#;
const RELS: &str = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
const WORKBOOK: &str = r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
const WORKBOOK_RELS: &str = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;
const SHEET: &str = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" s="1"><v>1234.5</v></c><c r="B1" s="1"><v>0.075</v></c><c r="C1" s="1" t="inlineStr"><is><t>text</t></is></c></row></sheetData></worksheet>"#;
