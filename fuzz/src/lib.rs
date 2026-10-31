//! The converters the fuzz targets run, the packages around one fuzzed part, and the determinism
//! check the part-injection targets run.

use almiladoc::{Config, Conversion, ConvertError, Converter, Format, Input};
use std::io::{Cursor, Write};
use zip::result::ZipResult;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

pub use libfuzzer_sys::fuzz_target;

/// Grid positions a fuzzed conversion admits. A grid at the default limit, about 4 million
/// positions, is linear and charged yet fills libFuzzer's 10 s slow-unit bound under the
/// sanitizers; below it the bound still catches work that grows faster than its input.
pub const MAX_GRID_SLOTS: u64 = 500_000;

/// Work a target that paints pages admits. At the default, painting runs to about 69 billion
/// pixels before it is stopped, which fills libFuzzer's 10 s slow-unit bound under the
/// sanitizers; below it the bound still catches painting that is charged too little.
pub const MAX_PAINTING_WORK: u64 = 1 << 20;

/// The converter every target that does not paint runs: the default configuration with
/// [`MAX_GRID_SLOTS`].
///
/// # Panics
///
/// If that configuration is invalid, so a broken harness fails every run instead of skipping it.
#[must_use]
pub fn converter() -> Converter {
    Converter::new(config()).expect("the fuzz configuration is valid")
}

/// [`converter`] with `max_work` lowered to [`MAX_PAINTING_WORK`], for the targets that paint.
///
/// # Panics
///
/// As [`converter`] does.
#[must_use]
pub fn painting_converter() -> Converter {
    let mut config = config();
    config.limits.max_work = MAX_PAINTING_WORK;
    Converter::new(config).expect("the fuzz configuration is valid")
}

fn config() -> Config {
    let mut config = Config::default();
    config.limits.max_grid_slots = MAX_GRID_SLOTS;
    config
}

/// Converts `data` as `format`; a conversion may fail with a typed error but never panic, hang or
/// exhaust memory.
pub fn convert(data: &[u8], format: Format) {
    let _: Result<Conversion, ConvertError> = converter().convert(Input::new(data, format));
}

macro_rules! content_types {
    ($($part:literal => $kind:literal),+) => {
        concat!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">"#,
            r#"<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>"#,
            r#"<Default Extension="xml" ContentType="application/xml"/>"#,
            $(
                r#"<Override PartName="/"#, $part,
                r#"" ContentType="application/vnd.openxmlformats-officedocument."#, $kind, r#"+xml"/>"#
            ),+,
            "</Types>"
        )
    };
}

macro_rules! relationships {
    ($($id:literal $kind:literal $target:literal),+) => {
        concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            $(
                r#"<Relationship Id=""#, $id,
                r#"" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/"#, $kind,
                r#"" Target=""#, $target, r#""/>"#
            ),+,
            "</Relationships>"
        )
    };
}

/// A package whose parts are fixed except the one a fuzz target fills.
pub struct Template {
    /// The fuzz target's name, which names its `seeds/` and `regressions/` directories.
    pub name: &'static str,
    format: Format,
    fuzzed: &'static str,
    fixed: &'static [(&'static str, &'static str)],
}

/// The body of a Word document in a package whose content types name it the main part.
pub const DOCX_BODY: Template = Template {
    name: "docx_body",
    format: Format::Docx,
    fuzzed: "word/document.xml",
    fixed: &[
        (
            "[Content_Types].xml",
            content_types!(
                "word/document.xml" => "wordprocessingml.document.main",
                "word/styles.xml" => "wordprocessingml.styles"
            ),
        ),
        ("_rels/.rels", relationships!("rId1" "officeDocument" "word/document.xml")),
        (
            "word/styles.xml",
            r#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:rPrDefault><w:rPr/></w:rPrDefault></w:docDefaults></w:styles>"#,
        ),
    ],
};

/// The first slide of a presentation whose layout and master the slide inherits from.
pub const PPTX_SLIDE: Template = Template {
    name: "pptx_slide",
    format: Format::Pptx,
    fuzzed: "ppt/slides/slide1.xml",
    fixed: &[
        (
            "[Content_Types].xml",
            content_types!(
                "ppt/presentation.xml" => "presentationml.presentation.main",
                "ppt/slides/slide1.xml" => "presentationml.slide",
                "ppt/slideLayouts/slideLayout1.xml" => "presentationml.slideLayout",
                "ppt/slideMasters/slideMaster1.xml" => "presentationml.slideMaster"
            ),
        ),
        ("_rels/.rels", relationships!("rId1" "officeDocument" "ppt/presentation.xml")),
        (
            "ppt/presentation.xml",
            r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#,
        ),
        ("ppt/_rels/presentation.xml.rels", relationships!("rId1" "slide" "slides/slide1.xml")),
        (
            "ppt/slides/_rels/slide1.xml.rels",
            relationships!("rId1" "slideLayout" "../slideLayouts/slideLayout1.xml"),
        ),
        (
            "ppt/slideLayouts/slideLayout1.xml",
            r#"<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:p><a:endParaRPr/></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:cNvPr id="3" name="Body"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle><a:lvl1pPr><a:buAutoNum type="arabicPeriod"/></a:lvl1pPr></a:lstStyle><a:p><a:endParaRPr/></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sldLayout>"#,
        ),
        (
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            relationships!("rId1" "slideMaster" "../slideMasters/slideMaster1.xml"),
        ),
        (
            "ppt/slideMasters/slideMaster1.xml",
            r#"<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/></p:spTree></p:cSld><p:txStyles><p:titleStyle><a:lvl1pPr><a:buNone/><a:defRPr b="1"/></a:lvl1pPr></p:titleStyle><p:bodyStyle><a:lvl1pPr><a:buChar char="&#8226;"/></a:lvl1pPr><a:lvl2pPr><a:buChar char="-"/><a:defRPr i="1"/></a:lvl2pPr></p:bodyStyle><p:otherStyle/></p:txStyles></p:sldMaster>"#,
        ),
    ],
};

/// The first worksheet of a workbook with the styles and shared strings its cells point into.
pub const XLSX_SHEET: Template = Template {
    name: "xlsx_sheet",
    format: Format::Excel,
    fuzzed: "xl/worksheets/sheet1.xml",
    fixed: &[
        (
            "[Content_Types].xml",
            content_types!(
                "xl/workbook.xml" => "spreadsheetml.sheet.main",
                "xl/worksheets/sheet1.xml" => "spreadsheetml.worksheet",
                "xl/styles.xml" => "spreadsheetml.styles",
                "xl/sharedStrings.xml" => "spreadsheetml.sharedStrings"
            ),
        ),
        ("_rels/.rels", relationships!("rId1" "officeDocument" "xl/workbook.xml")),
        (
            "xl/workbook.xml",
            r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            relationships!(
                "rId1" "worksheet" "worksheets/sheet1.xml",
                "rId2" "styles" "styles.xml",
                "rId3" "sharedStrings" "sharedStrings.xml"
            ),
        ),
        (
            "xl/styles.xml",
            r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="1"><numFmt numFmtId="164" formatCode="&quot;$&quot;#,##0.00"/></numFmts><cellXfs count="7"><xf numFmtId="0"/><xf numFmtId="9"/><xf numFmtId="164"/><xf numFmtId="3"/><xf numFmtId="14"/><xf numFmtId="46"/><xf numFmtId="0"/></cellXfs></styleSheet>"#,
        ),
        (
            "xl/sharedStrings.xml",
            r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="20" uniqueCount="20"><si><t>a</t></si><si><t>b</t></si><si><t>c</t></si><si><t>d</t></si><si><t>e</t></si><si><t>f</t></si><si><t>g</t></si><si><t>h</t></si><si><t>i</t></si><si><t>j</t></si><si><t>k</t></si><si><t>l</t></si><si><t>m</t></si><si><t>n</t></si><si><t>o</t></si><si><t>p</t></si><si><t>q</t></si><si><t>r</t></si><si><t>s</t></si><si><t>t</t></si></sst>"#,
        ),
    ],
};

/// The `content.xml` of an OpenDocument Text package, whose body element may name any ODF kind.
pub const ODF_CONTENT: Template = Template {
    name: "odf_content",
    format: Format::Odt,
    fuzzed: "content.xml",
    fixed: &[
        ("mimetype", "application/vnd.oasis.opendocument.text"),
        ("Pictures/TablePreview1.svm", "preview"),
        ("Pictures/100000010000000100000001C4EE90A9.png", "picture"),
    ],
};

/// Every part-injection template.
pub const TEMPLATES: [&Template; 4] = [&DOCX_BODY, &PPTX_SLIDE, &XLSX_SHEET, &ODF_CONTENT];

/// An archive of `parts`, in order, written with `options`.
///
/// # Errors
///
/// The archive writer's error when it cannot take a part.
pub fn archive(parts: &[(&str, &[u8])], options: SimpleFileOptions) -> ZipResult<Vec<u8>> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for &(name, body) in parts {
        zip.start_file(name, options)?;
        zip.write_all(body)?;
    }
    Ok(zip.finish()?.into_inner())
}

/// A stored archive of `parts`, in order.
///
/// # Errors
///
/// The archive writer's error when it cannot take a part.
pub fn stored(parts: &[(&str, &[u8])]) -> ZipResult<Vec<u8>> {
    archive(parts, SimpleFileOptions::default().compression_method(CompressionMethod::Stored))
}

impl Template {
    /// The package with `fuzzed` as the template's one fuzzed part.
    ///
    /// # Errors
    ///
    /// The archive writer's error when it cannot take a part.
    pub fn package(&self, fuzzed: &[u8]) -> ZipResult<Vec<u8>> {
        let mut parts: Vec<(&str, &[u8])> =
            self.fixed.iter().map(|&(name, body)| (name, body.as_bytes())).collect();
        parts.push((self.fuzzed, fuzzed));
        stored(&parts)
    }

    /// Converts the package around `fuzzed` twice with one converter, and parses and serialises
    /// it to JSON twice.
    ///
    /// # Panics
    ///
    /// Unless both conversions give equal Markdown and diagnostics, and both serialisations equal
    /// JSON, or fail with the same error.
    pub fn check(&self, fuzzed: &[u8]) {
        let Ok(package) = self.package(fuzzed) else {
            return;
        };
        let converter = converter();
        let input = Input::new(&package, self.format);
        let convert = || converter.convert(input.clone());
        assert_eq!(convert(), convert());
        let json = || converter.parse(input.clone()).and_then(|parsed| parsed.to_json());
        assert_eq!(json(), json());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use almiladoc::{DiagnosticCode, Limit};
    use std::fs;
    use std::path::Path;

    fn seeds(template: &Template) -> Vec<(String, Vec<u8>)> {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds").join(template.name);
        let mut seeds: Vec<_> = fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_file())
            .map(|path| {
                (path.file_name().unwrap().to_string_lossy().into_owned(), fs::read(&path).unwrap())
            })
            .collect();
        seeds.sort();
        seeds
    }

    const EXPECTED: [(&str, &[DiagnosticCode], &str); 9] = [
        ("docx_body/handmade-math.xml", &[], r"\sqrt{b^{2}-4ac}"),
        ("docx_body/handmade-tables.xml", &[], "| Head A | Head B | Head C |"),
        ("pptx_slide/handmade-inherit.xml", &[], "1. Roman three bold via master\n\n2. Roman four"),
        ("pptx_slide/pres.xml", &[], "- Top level point\n\n  - Nested detail"),
        ("xlsx_sheet/handmade-merged.xml", &[], "| Merged across |  | padded |"),
        (
            "xlsx_sheet/sheet.xml",
            &[DiagnosticCode::HeaderOmitted, DiagnosticCode::FooterOmitted],
            "| d | 16% | e |\n| f | $1,234.50 | g |\n| h | 9,876,543 | i |\n| j | 2026-03-15 | k |",
        ),
        ("odf_content/pres.xml", &[], "| North | 42 |"),
        ("odf_content/sheet.xml", &[], "| Currency | $1,234.50 | dollars |"),
        (
            "odf_content/text.xml",
            &[DiagnosticCode::ImagePlaceholder],
            "Inline image: ![tiny red dot](asset:0) done.",
        ),
    ];

    #[test]
    fn every_seed_injected_converts_with_its_support_parts_read() {
        let converter = converter();
        let mut injected = 0;
        for template in TEMPLATES {
            let seeds = seeds(template);
            assert!(!seeds.is_empty(), "{} has no seed", template.name);
            for (file, seed) in seeds {
                let name = format!("{}/{file}", template.name);
                let &(_, codes, needle) = EXPECTED
                    .iter()
                    .find(|(seed, ..)| *seed == name)
                    .unwrap_or_else(|| panic!("{name} has no expected row"));
                let package = template.package(&seed).unwrap();
                let converted = converter.convert(Input::new(&package, template.format));
                let conversion = converted.unwrap_or_else(|error| panic!("{name}: {error}"));
                let reported: Vec<_> = conversion.diagnostics().iter().map(|d| d.code).collect();
                assert_eq!(reported, codes, "{name}");
                assert!(
                    conversion.markdown().contains(needle),
                    "{name}:\n{}",
                    conversion.markdown()
                );
                injected += 1;
            }
        }
        assert_eq!(injected, EXPECTED.len());
    }

    const MAX_DICTIONARY_WORD: usize = 64;

    fn dictionary_word(line: &str) -> Option<Vec<u8>> {
        let line = line.trim_matches(|c: char| c.is_ascii_whitespace());
        let quoted = line.strip_suffix('"')?;
        let mut bytes = quoted[quoted.find('"')? + 1..].bytes();
        let mut word = Vec::new();
        while let Some(byte) = bytes.next() {
            match byte {
                b'\\' => match bytes.next()? {
                    escaped @ (b'\\' | b'"') => word.push(escaped),
                    b'x' => {
                        let mut nibble = || char::from(bytes.next()?).to_digit(16);
                        let (high, low) = (nibble()?, nibble()?);
                        word.push(u8::try_from((high << 4) | low).ok()?);
                    }
                    _ => return None,
                },
                b' '..=b'~' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r' => word.push(byte),
                _ => return None,
            }
        }
        Some(word)
    }

    fn prefix(word: &str) -> Option<&str> {
        let (prefix, local) = word.trim_start_matches(['<', '/']).split_once(':')?;
        let qualified = !prefix.is_empty()
            && prefix.bytes().all(|b| b.is_ascii_alphanumeric())
            && local.starts_with(|c: char| c.is_ascii_alphabetic())
            && !["xmlns", "xml", "urn", "http"].contains(&prefix);
        qualified.then_some(prefix)
    }

    #[test]
    fn every_dictionary_word_parses_fits_and_has_its_prefix_declared() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("dicts");
        for template in TEMPLATES {
            let path = directory.join(format!("{}.dict", template.name));
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let words: Vec<String> = text
                .lines()
                .filter(|line| !line.trim_start().is_empty() && !line.trim_start().starts_with('#'))
                .map(|line| {
                    let word = dictionary_word(line)
                        .unwrap_or_else(|| panic!("{}: unparsed {line}", template.name));
                    assert!(
                        !word.is_empty() && word.len() <= MAX_DICTIONARY_WORD,
                        "{}: {line}",
                        template.name
                    );
                    String::from_utf8_lossy(&word).into_owned()
                })
                .collect();
            let declared: Vec<&str> = words
                .iter()
                .filter_map(|word| {
                    word.strip_prefix("xmlns:")?.split_once('=').map(|(prefix, _)| prefix)
                })
                .collect();
            for word in &words {
                if let Some(prefix) = prefix(word) {
                    assert!(
                        declared.contains(&prefix),
                        "{}: {word} has no xmlns:{prefix}",
                        template.name
                    );
                }
            }
        }
    }

    #[test]
    fn the_dictionary_check_reads_libfuzzer_escapes_and_xml_prefixes() {
        assert_eq!(dictionary_word(r#"kw="a\"b\\c\x41""#), Some(b"a\"b\\cA".to_vec()));
        assert_eq!(dictionary_word("\"\\xEF\\xBB\\xBF\""), Some(vec![0xEF, 0xBB, 0xBF]));
        for malformed in
            [r#""\q""#, r#""\xG1""#, r#""\x4G""#, r#""\x4""#, "\"\u{e9}\"", "noquote", "\"open"]
        {
            assert_eq!(dictionary_word(malformed), None, "{malformed}");
        }
        assert_eq!(prefix("<w:p>"), Some("w"));
        assert_eq!(prefix("</a14:m"), Some("a14"));
        assert_eq!(prefix(r#"r:embed=""#), Some("r"));
        for unqualified in
            ["xmlns:w=\"", "xml:space", "urn:oasis:names", "http://x", "office:1.0\"", "t=\"s\""]
        {
            assert_eq!(prefix(unqualified), None, "{unqualified}");
        }
    }

    #[test]
    fn a_seed_cut_short_or_corrupted_still_converts_deterministically() {
        for template in TEMPLATES {
            for (_, seed) in seeds(template) {
                let step = seed.len() / 24 + 1;
                for end in (0..seed.len()).step_by(step) {
                    template.check(&seed[..end]);
                }
                let mut corrupt = seed;
                for at in (0..corrupt.len()).step_by(step) {
                    corrupt[at] = corrupt[at].wrapping_add(0x55);
                    template.check(&corrupt);
                }
            }
        }
    }

    #[test]
    fn hostile_parts_convert_deterministically() {
        let parts: [Vec<u8>; 12] = [
            Vec::new(),
            b"\xEF\xBB\xBF".to_vec(),
            b"\xFF\xFE<\0".to_vec(),
            vec![0xFF; 64],
            b"<?xml version=\"1.0\" encoding=\"nonesuch\"?><a/>".to_vec(),
            b"<!DOCTYPE a [<!ENTITY e \"&e;\">]><a>&e;</a>".to_vec(),
            b"<a b=\"1\" b=\"2\" xmlns:p=\"\" p:q=\"3\"/>".to_vec(),
            b"<a>&#0;&#xD800;&#x110000;&bogus;<![CDATA[".to_vec(),
            b"<a><b></a></b>".to_vec(),
            "<a>".repeat(255).into_bytes(),
            "<a>".repeat(257).into_bytes(),
            format!("<a {}/>", "x=\"1\" ".repeat(10_000)).into_bytes(),
        ];
        for template in TEMPLATES {
            for part in &parts {
                template.check(part);
            }
        }
    }

    #[test]
    fn a_grid_past_the_fuzz_limit_is_refused() {
        let sheet = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="600000"><c r="A600000"><v>2</v></c></row></sheetData></worksheet>"#;
        let package = XLSX_SHEET.package(sheet.as_bytes()).unwrap();
        let converted = converter().convert(Input::new(&package, Format::Excel));
        assert!(
            matches!(
                converted,
                Err(ConvertError::ResourceLimit {
                    limit: Limit::GridSlots,
                    max: MAX_GRID_SLOTS,
                    ..
                })
            ),
            "{converted:?}"
        );
    }

    #[test]
    fn painting_past_the_fuzz_limit_is_refused() {
        use almiladoc::{PageSelection, RasterSpec};
        use std::fmt::Write as _;

        let content = "0 0 1 rg 0 0 1440 1440 re f\n".repeat(200);
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1440 1440] /Contents 4 0 R >>".to_owned(),
            format!("<< /Length {} >>\nstream\n{content}endstream", content.len()),
        ];
        let mut pdf = "%PDF-1.7\n".to_owned();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            writeln!(pdf, "{} 0 obj\n{object}\nendobj", index + 1).unwrap();
        }
        let xref = pdf.len();
        writeln!(pdf, "xref\n0 5\n0000000000 65535 f ").unwrap();
        for offset in offsets {
            writeln!(pdf, "{offset:010} 00000 n ").unwrap();
        }
        writeln!(pdf, "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF").unwrap();
        let input = Input::new(pdf.as_bytes(), Format::Pdf).with_ocr_pages(PageSelection::All);
        let mut parsed = painting_converter().parse(input).unwrap();
        let mut spec = RasterSpec::default();
        (spec.dpi, spec.max_pixels) = (50, 1 << 20);
        let refused = parsed.ocr_requests(&spec, None);
        assert!(
            matches!(
                refused,
                Err(ConvertError::ResourceLimit { limit: Limit::Work, max: MAX_PAINTING_WORK, .. })
            ),
            "{refused:?}"
        );
    }
}
