use crate::build::{W, relationships};
use crate::support::{configured, reported, zip_package};
use almiladoc::{DiagnosticCode, Format, HeaderFooterPolicy, Input, OnLoss, Region};

type Reported = Vec<(DiagnosticCode, Region, Option<u32>, u64)>;

fn emitted(bytes: &[u8], format: Format, policy: HeaderFooterPolicy) -> (String, Reported, usize) {
    let converter = configured(|config| {
        config.headers_footers = policy;
        config.on_loss = OnLoss::Report;
    });
    let parsed =
        converter.parse(Input::new(bytes, format)).unwrap_or_else(|error| panic!("{error:?}"));
    let assets = parsed.document().assets.len();
    let conversion = parsed.render().unwrap();
    (conversion.markdown().to_owned(), reported(conversion.diagnostics()), assets)
}

fn docx(body: &str, rels: &[(&str, &str, &str)], parts: &[(&str, String)]) -> Vec<u8> {
    let document = format!("<w:document {W}><w:body>{body}</w:body></w:document>");
    let mut entries = vec![
        ("word/document.xml".to_string(), document.into_bytes()),
        ("word/_rels/document.xml.rels".to_string(), relationships(rels).into_bytes()),
    ];
    entries.extend(parts.iter().map(|(name, xml)| ((*name).to_string(), xml.clone().into_bytes())));
    zip_package(&entries)
}

fn story(root: &str, content: &str) -> String {
    format!("<w:{root} {W}>{content}</w:{root}>")
}

fn para(text: &str) -> String {
    format!("<w:p><w:r><w:t xml:space=\"preserve\">{text}</w:t></w:r></w:p>")
}

fn complex_field(instr: &str, result: &str) -> String {
    let char = |kind| format!(r#"<w:r><w:fldChar w:fldCharType="{kind}"/></w:r>"#);
    format!(
        "{}<w:r><w:instrText xml:space=\"preserve\">{instr}</w:instrText></w:r>{}{result}{}",
        char("begin"),
        char("separate"),
        char("end")
    )
}

const SECTIONS_BODY: &str = concat!(
    r#"<w:p><w:r><w:t>One</w:t></w:r></w:p>"#,
    r#"<w:p><w:pPr><w:sectPr><w:headerReference w:type="default" r:id="h1"/>"#,
    r#"<w:headerReference w:type="first" r:id="f1"/><w:headerReference w:type="even" r:id="e1"/>"#,
    r#"<w:footerReference w:type="default" r:id="ft"/><w:titlePg/></w:sectPr></w:pPr>"#,
    r#"<w:r><w:t>End one</w:t></w:r></w:p>"#,
    r#"<w:p><w:r><w:t>Two</w:t></w:r></w:p>"#,
    r#"<w:p><w:pPr><w:sectPr><w:headerReference w:type="default" r:id="h2"/>"#,
    r#"<w:titlePg w:val="0"/></w:sectPr></w:pPr></w:p>"#,
    r#"<w:p><w:r><w:t>Three</w:t></w:r></w:p>"#,
    r#"<w:sectPr><w:titlePg/></w:sectPr>"#,
);

fn sections_docx() -> Vec<u8> {
    let rich = format!(
        "<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>Title</w:t></w:r><w:r><w:t xml:space=\"preserve\"> page </w:t></w:r>{}<w:r><w:t xml:space=\"preserve\"> of </w:t></w:r><w:fldSimple w:instr=\"NUMPAGES\"><w:r><w:t>9</w:t></w:r></w:fldSimple></w:p>{}",
        complex_field(" PAGE ", "<w:r><w:t>7</w:t></w:r>"),
        concat!(
            r#"<w:p><w:bookmarkStart w:id="0" w:name="top"/><w:hyperlink w:anchor="top">"#,
            r#"<w:r><w:t>top</w:t></w:r></w:hyperlink><w:r><w:t xml:space="preserve"> </w:t></w:r>"#,
            r#"<w:r><w:drawing><a:blip r:embed="png"/></w:drawing></w:r></w:p>"#
        ),
    );
    docx(
        SECTIONS_BODY,
        &[
            ("h1", "header", "h1.xml"),
            ("f1", "header", "f1.xml"),
            ("e1", "header", "e1.xml"),
            ("h2", "header", "h2.xml"),
            ("ft", "footer", "ft.xml"),
            ("set", "settings", "settings.xml"),
        ],
        &[
            ("word/h1.xml", story("hdr", &rich)),
            ("word/_rels/h1.xml.rels", relationships(&[("png", "image", "media/logo.png")])),
            ("word/media/logo.png", "\u{89}PNG".to_string()),
            ("word/f1.xml", story("hdr", &para("Cover"))),
            ("word/e1.xml", story("hdr", &para("Even"))),
            ("word/h2.xml", story("hdr", &para("Second"))),
            ("word/ft.xml", story("ftr", &para("Foot"))),
            ("word/settings.xml", format!("<w:settings {W}><w:evenAndOddHeaders/></w:settings>")),
        ],
    )
}

#[test]
fn a_word_section_shows_the_headers_it_names_or_inherits_with_rich_content_in_its_place() {
    let (markdown, rows, assets) =
        emitted(&sections_docx(), Format::Docx, HeaderFooterPolicy::Emit);
    assert_eq!(
        markdown,
        concat!(
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n",
            "> **Title** page {page} of {pages}\n>\n",
            "> <a id=\"almiladoc:header:default:1:top\"></a>[top](#almiladoc:header:default:1:top) ",
            "![Image](asset:0)\n\n",
            "> <a id=\"almiladoc:header:first:2\"></a>Header (first):\n>\n> Cover\n\n",
            "> <a id=\"almiladoc:header:even:3\"></a>Header (even):\n>\n> Even\n\n",
            "> <a id=\"almiladoc:footer:default:4\"></a>Footer (default):\n>\n> Foot\n\n",
            "> <a id=\"almiladoc:footer:first:5\"></a>Footer (first):\n\n",
            "> <a id=\"almiladoc:footer:even:6\"></a>Footer (even):\n\n",
            "One\n\nEnd one\n\n",
            "> <a id=\"almiladoc:header:default:7\"></a>Header (default):\n>\n> Second\n\n",
            "> <a id=\"almiladoc:header:first:8\"></a>Header (first, uses default):\n\n",
            "> <a id=\"almiladoc:footer:first:9\"></a>Footer (first, uses default):\n\n",
            "Two\n\n",
            "> <a id=\"almiladoc:header:first:10\"></a>Header (first):\n>\n> Cover\n\n",
            "> <a id=\"almiladoc:footer:first:11\"></a>Footer (first):\n\n",
            "Three\n",
        )
    );
    assert_eq!(assets, 1);
    assert_eq!(
        rows,
        [
            (DiagnosticCode::ImagePlaceholder, Region::Header, None, 1),
            (DiagnosticCode::FieldToken, Region::Header, None, 2),
        ]
    );
}

#[test]
fn omitted_word_sides_count_their_parts_without_reading_them() {
    let broken = docx(
        SECTIONS_BODY,
        &[("h1", "header", "h1.xml"), ("ft", "footer", "missing.xml")],
        &[("word/h1.xml", "<w:hdr".to_string())],
    );
    let (markdown, rows, _) = emitted(&broken, Format::Docx, HeaderFooterPolicy::Omit);
    assert_eq!(markdown, "One\n\nEnd one\n\nTwo\n\nThree\n");
    assert_eq!(
        rows,
        [
            (DiagnosticCode::HeaderOmitted, Region::Header, None, 4),
            (DiagnosticCode::FooterOmitted, Region::Footer, None, 1),
        ]
    );
}

fn one_header(content: &str) -> Vec<u8> {
    docx(
        r#"<w:p><w:r><w:t>Body</w:t></w:r></w:p><w:sectPr><w:headerReference w:type="default" r:id="h"/></w:sectPr>"#,
        &[("h", "header", "h.xml"), ("png", "image", "media/body.png")],
        &[
            ("word/h.xml", story("hdr", content)),
            ("word/_rels/h.xml.rels", relationships(&[("png", "image", "media/logo.png")])),
            ("word/media/logo.png", "\u{89}PNG".to_string()),
        ],
    )
}

#[test]
fn a_word_header_that_shows_nothing_still_reports_what_it_lost_or_omitted() {
    let blank = "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n\nBody\n";
    for (content, markdown, expected) in [
        (
            "<w:p><w:r><w:rPr><w:vanish/></w:rPr><w:t>secret</w:t></w:r></w:p>".to_string(),
            "Body\n",
            (DiagnosticCode::HiddenOmitted, Region::Header),
        ),
        (
            r#"<w:p><w:r><w:contentPart r:id="ink"/></w:r></w:p>"#.to_string(),
            blank,
            (DiagnosticCode::GraphicUnread, Region::Header),
        ),
        (
            "<w:p><w:r><w:t>Note</w:t></w:r><w:r><w:commentReference w:id=\"0\"/></w:r></w:p>"
                .to_string(),
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n> Note\n\nBody\n",
            (DiagnosticCode::CommentOmitted, Region::Comment),
        ),
    ] {
        let (rendered, rows, _) =
            emitted(&one_header(&content), Format::Docx, HeaderFooterPolicy::Emit);
        assert_eq!(rendered, markdown, "{content}");
        assert_eq!(rows, [(expected.0, expected.1, None, 1)], "{content}");
    }
    let missing = docx(
        r#"<w:p><w:r><w:t>Body</w:t></w:r></w:p><w:sectPr><w:headerReference w:type="default" r:id="h"/></w:sectPr>"#,
        &[("h", "header", "gone.xml")],
        &[],
    );
    let (rendered, rows, _) = emitted(&missing, Format::Docx, HeaderFooterPolicy::Emit);
    assert_eq!(rendered, blank);
    assert_eq!(rows, [(DiagnosticCode::PartUnreadable, Region::Header, None, 1)]);
}

#[test]
fn a_word_side_field_writes_its_token_and_nothing_its_cached_result_or_instruction_holds() {
    let damaged = r#"<w:r><w:contentPart r:id="ink"/></w:r><w:r><w:drawing><a:blip r:embed="png"/></w:drawing></w:r><w:r><w:ruby><w:rt><w:r><w:t>x</w:t></w:r></w:rt></w:ruby></w:r>"#;
    let content = format!(
        "<w:p>{}<w:r><w:t xml:space=\"preserve\"> / </w:t></w:r><w:fldSimple w:instr=\"NUMPAGES\">{damaged}</w:fldSimple><w:r><w:t xml:space=\"preserve\"> </w:t></w:r>{}<w:r><w:t xml:space=\"preserve\"> </w:t></w:r>{}</w:p>",
        complex_field("PAGE", damaged),
        complex_field("DATE", ""),
        complex_field("REF kept", "<w:r><w:t>kept</w:t></w:r>"),
    )
    .replace(
        "<w:instrText xml:space=\"preserve\">DATE</w:instrText>",
        &format!("<w:instrText xml:space=\"preserve\">DATE</w:instrText></w:r>{damaged}<w:r>"),
    );
    let (markdown, rows, assets) =
        emitted(&one_header(&content), Format::Docx, HeaderFooterPolicy::Emit);
    assert_eq!(
        markdown,
        "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n> {page} / {pages} {date} kept\n\nBody\n"
    );
    assert_eq!(rows, [(DiagnosticCode::FieldToken, Region::Header, None, 3)]);
    assert_eq!(assets, 0);
}

fn locked(field: &str, lock: &str) -> String {
    field.replacen(
        r#"w:fldCharType="begin""#,
        &format!(r#"w:fldCharType="begin" w:fldLock="{lock}""#),
        1,
    )
}

#[test]
fn a_locked_word_side_field_keeps_the_result_it_stores() {
    let cached = "<w:r><w:t>2.1.2020</w:t></w:r>";
    let simple = |lock: &str, instr: &str, result: &str| {
        format!(r#"<w:fldSimple w:instr="{instr}"{lock}>{result}</w:fldSimple>"#)
    };
    let token = |text: &str| {
        (text.to_string(), vec![(DiagnosticCode::FieldToken, Region::Header, None, 1)])
    };
    let kept = |text: &str| (text.to_string(), Vec::new());
    let date = complex_field("DATE", cached);
    let page = complex_field("PAGE", "<w:r><w:t>3</w:t></w:r>");
    for (field, expected) in [
        (simple("", "DATE", cached), token("{date}")),
        (simple(r#" w:fldLock="false""#, "DATE", cached), token("{date}")),
        (simple(r#" w:fldLock="0""#, "DATE", cached), token("{date}")),
        (simple(r#" w:fldLock="off""#, "DATE", cached), token("{date}")),
        (simple(r#" w:fldLock="true""#, "DATE", cached), kept("2.1.2020")),
        (simple(r#" w:fldLock="1""#, "DATE", cached), kept("2.1.2020")),
        (simple(r#" w:fldLock="on""#, "DATE", cached), kept("2.1.2020")),
        (
            simple(r#" w:fldLock="1""#, "FILENAME", "<w:r><w:t>report.docx</w:t></w:r>"),
            kept("report.docx"),
        ),
        (date.clone(), token("{date}")),
        (locked(&date, "false"), token("{date}")),
        (locked(&date, "true"), kept("2.1.2020")),
        (locked(&date, "1"), kept("2.1.2020")),
        (locked(&complex_field("DATE", &page), "true"), token("{page}")),
        (complex_field("DATE", &locked(&page, "true")), token("{date}")),
        (simple(r#" w:fldLock="true""#, "DATE", &page), token("{page}")),
    ] {
        let content = format!("<w:p>{field}</w:p>");
        let (markdown, rows, _) =
            emitted(&one_header(&content), Format::Docx, HeaderFooterPolicy::Emit);
        let header = format!(
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n> {}\n\nBody\n",
            expected.0
        );
        assert_eq!((markdown, rows), (header, expected.1), "{field}");
    }
    let rich = r#"<w:r><w:t>On </w:t></w:r><w:r><w:drawing><a:blip r:embed="png"/></w:drawing></w:r><w:r><w:contentPart r:id="ink"/></w:r>"#;
    for (lock, assets, rows) in [
        (
            "true",
            1,
            vec![
                (DiagnosticCode::ImagePlaceholder, Region::Header, None, 1),
                (DiagnosticCode::GraphicUnread, Region::Header, None, 1),
            ],
        ),
        ("false", 0, vec![(DiagnosticCode::FieldToken, Region::Header, None, 1)]),
    ] {
        let content = format!("<w:p>{}</w:p>", locked(&complex_field("DATE", rich), lock));
        let (_, reported, shown) =
            emitted(&one_header(&content), Format::Docx, HeaderFooterPolicy::Emit);
        assert_eq!((reported, shown), (rows, assets), "{lock}");
    }
}

#[test]
fn a_locked_rtf_side_field_keeps_the_result_it_stores() {
    for (field, markdown, rows) in [
        (r"{\field{\*\fldinst DATE }{\fldrslt 2.1.2020}}", "{date}", 1),
        (r"{\field\fldlock{\*\fldinst DATE }{\fldrslt 2.1.2020}}", "2.1.2020", 0),
        (r"{\field\flddirty\fldlock{\*\fldinst FILENAME }{\fldrslt r.rtf}}", "r.rtf", 0),
        (
            r"{\field\fldlock{\*\fldinst DATE }{\fldrslt {\field{\*\fldinst PAGE}{\fldrslt 3}}}}",
            "{page}",
            1,
        ),
        (
            r"{\field{\*\fldinst DATE }{\fldrslt {\field\fldlock{\*\fldinst PAGE}{\fldrslt 3}}}}",
            "{date}",
            1,
        ),
        (r"{\field{\*\fldinst DATE }{\fldrslt {\fldlock 2.1.2020}}}", "{date}", 1),
        (r"{\field{\*\fldinst {\fldlock DATE }}{\fldrslt 2.1.2020}}", "{date}", 1),
    ] {
        let source = format!(r"{{\rtf1{{\header \pard {field}\par}}\pard a\par}}");
        let expected = format!(
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n> {markdown}\n\na\n"
        );
        let tokens = if rows == 0 {
            vec![]
        } else {
            vec![(DiagnosticCode::FieldToken, Region::Header, None, rows)]
        };
        assert_eq!(rtf(&source), (expected, tokens, 0), "{field}");
    }
}

#[test]
fn word_side_anchors_and_body_anchors_never_resolve_one_another() {
    let bytes = docx(
        concat!(
            r#"<w:p><w:bookmarkStart w:id="1" w:name="top"/><w:hyperlink w:anchor="top"><w:r><w:t>up</w:t></w:r></w:hyperlink>"#,
            r#"<w:r><w:t xml:space="preserve"> </w:t></w:r><w:hyperlink w:anchor="side"><w:r><w:t>side</w:t></w:r></w:hyperlink></w:p>"#,
            r#"<w:sectPr><w:headerReference w:type="default" r:id="h"/></w:sectPr>"#
        ),
        &[("h", "header", "h.xml")],
        &[(
            "word/h.xml",
            story(
                "hdr",
                concat!(
                    r#"<w:p><w:bookmarkStart w:id="1" w:name="side"/><w:hyperlink w:anchor="top"><w:r><w:t>top</w:t></w:r></w:hyperlink>"#,
                    r#"<w:r><w:t xml:space="preserve"> </w:t></w:r><w:hyperlink w:anchor="side"><w:r><w:t>here</w:t></w:r></w:hyperlink></w:p>"#
                ),
            ),
        )],
    );
    let (markdown, rows, _) = emitted(&bytes, Format::Docx, HeaderFooterPolicy::Emit);
    assert_eq!(
        markdown,
        concat!(
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n",
            "> <a id=\"almiladoc:header:default:1:side\"></a>top [here](#almiladoc:header:default:1:side)\n\n",
            "<a id=\"top\"></a>[up](#top) side\n",
        )
    );
    assert_eq!(
        rows,
        [
            (DiagnosticCode::DanglingReference, Region::Body, None, 1),
            (DiagnosticCode::DanglingReference, Region::Header, None, 1),
        ]
    );
}

#[test]
fn a_section_break_whose_mark_a_revision_deletes_joins_its_section_to_the_next() {
    let bytes = docx(
        concat!(
            r#"<w:p><w:pPr><w:rPr><w:del w:id="9" w:author="a"/></w:rPr><w:sectPr>"#,
            r#"<w:headerReference w:type="default" r:id="h1"/></w:sectPr></w:pPr>"#,
            r#"<w:r><w:t>A</w:t></w:r></w:p><w:p><w:r><w:t>B</w:t></w:r></w:p>"#,
            r#"<w:sectPr><w:headerReference w:type="default" r:id="h2"/></w:sectPr>"#
        ),
        &[("h1", "header", "h1.xml"), ("h2", "header", "h2.xml")],
        &[
            ("word/h1.xml", story("hdr", &para("First"))),
            ("word/h2.xml", story("hdr", &para("Second"))),
        ],
    );
    let (markdown, rows, _) = emitted(&bytes, Format::Docx, HeaderFooterPolicy::Emit);
    assert_eq!(
        markdown,
        "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n> Second\n\nAB\n"
    );
    assert_eq!(rows, []);
}

const ODF_NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" xmlns:xlink="http://www.w3.org/1999/xlink""#;

fn odf_package(styles: &str, masters: &str, automatic: &str, body: &str) -> Vec<u8> {
    let styles = format!(
        "<office:document-styles {ODF_NS}><office:styles>{styles}</office:styles><office:master-styles>{masters}</office:master-styles></office:document-styles>"
    );
    let content = format!(
        "<office:document-content {ODF_NS}><office:automatic-styles>{automatic}</office:automatic-styles><office:body>{body}</office:body></office:document-content>"
    );
    zip_package(&[("content.xml", content), ("styles.xml", styles)])
}

#[test]
fn a_text_document_shows_the_sides_of_each_master_page_its_text_starts() {
    let styles = concat!(
        r#"<style:style style:name="Strong" style:family="text"><style:text-properties fo:font-weight="bold"/></style:style>"#,
        r#"<style:style style:name="Soft" style:family="text"><style:text-properties fo:font-style="italic"/></style:style>"#,
        r#"<style:style style:name="P1" style:family="paragraph" style:master-page-name="Cover"/>"#,
        r#"<style:style style:name="P2" style:family="paragraph" style:master-page-name="Landscape"/>"#,
    );
    let masters = concat!(
        r#"<style:master-page style:name="Standard"><style:header><text:p><text:span text:style-name="Strong">Run</text:span> "#,
        r#"<text:page-number>3</text:page-number>/<text:page-count>9</text:page-count></text:p>"#,
        r#"<text:list><text:list-item><text:p>item</text:p></text:list-item></text:list>"#,
        r#"<table:table><table:table-row><table:table-cell><text:p>cell</text:p></table:table-cell></table:table-row></table:table>"#,
        r#"</style:header><style:header-left><text:p>Left</text:p></style:header-left><style:header-first style:display="false"/>"#,
        r#"<style:footer><text:p><text:span text:style-name="Soft">foot</text:span></text:p></style:footer></style:master-page>"#,
        r#"<style:master-page style:name="Cover" style:next-style-name="Standard"><style:header><text:p>Cover</text:p></style:header></style:master-page>"#,
        r#"<style:master-page style:name="Landscape"><style:footer><text:p>Wide</text:p></style:footer></style:master-page>"#,
    );
    let body = r#"<office:text><text:p text:style-name="P1">One</text:p><text:p>Two</text:p><text:p text:style-name="P2">Three</text:p></office:text>"#;
    let bytes = odf_package(styles, masters, "", body);
    let (markdown, rows, _) = emitted(&bytes, Format::Odt, HeaderFooterPolicy::Emit);
    assert_eq!(
        markdown,
        concat!(
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default) of page style Cover:\n>\n> Cover\n\n",
            "> <a id=\"almiladoc:header:default:2\"></a>Header (default) of page style Standard:\n>\n",
            "> **Run** {page}/{pages}\n>\n> - item\n>\n> |  |\n> | --- |\n> | cell |\n\n",
            "> <a id=\"almiladoc:header:even:3\"></a>Header (even) of page style Standard:\n>\n> Left\n\n",
            "> <a id=\"almiladoc:footer:default:4\"></a>Footer (default) of page style Standard:\n>\n> _foot_\n\n",
            "One\n\nTwo\n\n",
            "> <a id=\"almiladoc:footer:default:5\"></a>Footer (default) of page style Landscape:\n>\n> Wide\n\n",
            "Three\n",
        )
    );
    assert_eq!(rows, [(DiagnosticCode::FieldToken, Region::Header, None, 2)]);
    let (omitted, rows, _) = emitted(&bytes, Format::Odt, HeaderFooterPolicy::Omit);
    assert_eq!(omitted, "One\n\nTwo\n\nThree\n");
    assert_eq!(
        rows,
        [
            (DiagnosticCode::HeaderOmitted, Region::Header, None, 2),
            (DiagnosticCode::FooterOmitted, Region::Footer, None, 2),
        ]
    );
}

#[test]
fn each_visible_sheet_shows_the_print_masters_it_reaches_once_under_their_names() {
    let masters = concat!(
        r#"<style:master-page style:name="Default" style:next-style-name="Tail"><style:header><style:region-left><text:p>L <text:sheet-name>S</text:sheet-name></text:p></style:region-left>"#,
        r#"<style:region-right><text:p><text:page-number>1</text:page-number></text:p></style:region-right></style:header>"#,
        r#"<style:footer style:display="false"><text:p>hidden foot</text:p></style:footer></style:master-page>"#,
        r#"<style:master-page style:name="Tail"><style:footer><text:p>tail</text:p></style:footer></style:master-page>"#,
        r#"<style:master-page style:name="Report"><style:header><text:p>Report</text:p></style:header><style:footer><text:p>F</text:p></style:footer></style:master-page>"#,
    );
    let automatic = concat!(
        r#"<style:style style:name="ta1" style:family="table" style:master-page-name="Default"/>"#,
        r#"<style:style style:name="ta2" style:family="table" style:master-page-name="Report"/>"#,
    );
    let sheet = |name: &str, style: &str| {
        format!(
            r#"<table:table table:name="{name}" table:style-name="{style}"><table:table-row><table:table-cell><text:p>{name}1</text:p></table:table-cell></table:table-row></table:table>"#
        )
    };
    let body = format!(
        "<office:spreadsheet>{}{}{}</office:spreadsheet>",
        sheet("A", "ta1"),
        sheet("B", "ta2"),
        sheet("C", "ta1")
    );
    let bytes = odf_package("", masters, automatic, &body);
    let (markdown, rows, _) = emitted(&bytes, Format::Ods, HeaderFooterPolicy::Emit);
    let table = |cell: &str| format!("|  |\n| --- |\n| {cell} |\n");
    let side = |n: usize, region: &str, label: &str, name: &str, text: &str| {
        format!(
            "> <a id=\"almiladoc:{region}:default:{n}\"></a>{label} (default) of page style {name}:\n>\n> {text}\n\n"
        )
    };
    assert_eq!(
        markdown,
        [
            "## A\n\n".into(),
            side(1, "header", "Header", "Default", "L {sheet}\n>\n> {page}"),
            side(2, "footer", "Footer", "Tail", "tail"),
            table("A1"),
            "\n## B\n\n".into(),
            side(3, "header", "Header", "Report", "Report"),
            side(4, "footer", "Footer", "Report", "F"),
            table("B1"),
            "\n## C\n\n".into(),
            table("C1"),
        ]
        .concat()
    );
    assert_eq!(rows, [(DiagnosticCode::FieldToken, Region::Header, Some(1), 2)]);
}

#[test]
fn a_slide_and_its_notes_page_show_the_running_text_they_declare_or_hold() {
    let body = concat!(
        r#"<office:presentation><presentation:footer-decl presentation:name="ftr1">Confidential</presentation:footer-decl>"#,
        r#"<presentation:date-time-decl presentation:name="dtd1" presentation:source="current-date"/>"#,
        r#"<draw:page draw:name="p1" presentation:use-footer-name="ftr1" presentation:use-date-time-name="dtd1">"#,
        r#"<draw:frame presentation:class="title"><draw:text-box><text:p>Hello</text:p></draw:text-box></draw:frame>"#,
        r#"<presentation:notes><draw:frame presentation:class="header"><draw:text-box><text:p>Handout</text:p></draw:text-box></draw:frame>"#,
        r#"<draw:frame><draw:text-box><text:p>Speak</text:p></draw:text-box></draw:frame></presentation:notes></draw:page>"#,
        r#"<draw:page draw:name="p2" presentation:use-footer-name="ftr1"><draw:frame presentation:class="page-number">"#,
        r#"<draw:text-box><text:p><text:page-number>2</text:page-number></text:p></draw:text-box></draw:frame></draw:page>"#,
        r#"</office:presentation>"#,
    );
    let bytes = odf_package("", "", "", body);
    let (markdown, rows, _) = emitted(&bytes, Format::Odp, HeaderFooterPolicy::Emit);
    assert_eq!(
        markdown,
        concat!(
            "> <a id=\"almiladoc:footer:default:1\"></a>Footer (default):\n>\n> Confidential\n>\n> {date}\n\n",
            "## Hello\n\n> > <a id=\"almiladoc:header:default:2\"></a>Header (default):\n> >\n> > Handout\n>\n> Speak\n\n",
            "---\n\n> <a id=\"almiladoc:footer:default:3\"></a>Footer (default):\n>\n> Confidential\n>\n> {page}\n",
        )
    );
    assert_eq!(
        rows,
        [
            (DiagnosticCode::FieldToken, Region::Footer, Some(1), 1),
            (DiagnosticCode::FieldToken, Region::Footer, Some(2), 1),
        ]
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "the table of master runs")]
fn every_master_a_page_run_reaches_shows_once_under_its_own_name() {
    let master = |name: &str, next: &str, content: &str| {
        let next = if next.is_empty() {
            String::new()
        } else {
            format!(r#" style:next-style-name="{next}""#)
        };
        format!(r#"<style:master-page style:name="{name}"{next}>{content}</style:master-page>"#)
    };
    let header = |text: &str| format!("<style:header><text:p>{text}</text:p></style:header>");
    let shown = |n: usize, variant: &str, name: &str, text: &str| {
        let body = if text.is_empty() { String::new() } else { format!(">\n> {text}\n") };
        format!(
            "> <a id=\"almiladoc:header:{variant}:{n}\"></a>Header ({variant}) of page style {name}:\n{body}\n"
        )
    };
    let styles = r#"<style:style style:name="PA" style:family="paragraph" style:master-page-name="A"/><style:style style:name="PB" style:family="paragraph" style:master-page-name="B"/>"#;
    let starts_a = r#"<office:text><text:p text:style-name="PA">x</text:p></office:text>"#;
    let nested = r#"<office:text><text:section><text:p text:style-name="PA">x</text:p></text:section><text:list><text:list-item><text:p text:style-name="PB">y</text:p></text:list-item></text:list></office:text>"#;
    for (masters, body, markdown) in [
        (
            [
                master("A", "B", &header("a")),
                master("B", "C", &header("b")),
                master("C", "", &header("c")),
            ]
            .concat(),
            starts_a,
            [
                shown(1, "default", "A", "a"),
                shown(2, "default", "B", "b"),
                shown(3, "default", "C", "c"),
                "x\n".into(),
            ]
            .concat(),
        ),
        (
            [master("A", "B", &header("a")), master("B", "A", &header("b"))].concat(),
            starts_a,
            [shown(1, "default", "A", "a"), shown(2, "default", "B", "b"), "x\n".into()].concat(),
        ),
        (
            master(
                "A",
                "",
                &format!("{}<style:header-first style:display=\"false\"/>", header("a")),
            ),
            starts_a,
            [shown(1, "default", "A", "a"), "x\n".into()].concat(),
        ),
        (
            master("A", "", &format!("{}<style:header-first/>", header("a"))),
            starts_a,
            [shown(1, "default", "A", "a"), shown(2, "first", "A", ""), "x\n".into()].concat(),
        ),
        (
            master("A", "", &header("a")),
            starts_a,
            [shown(1, "default", "A", "a"), "x\n".into()].concat(),
        ),
        (
            [master("A", "", &header("a")), master("B", "", &header("b"))].concat(),
            nested,
            [
                shown(1, "default", "A", "a"),
                "x\n\n".into(),
                shown(2, "default", "B", "b"),
                "- y\n".into(),
            ]
            .concat(),
        ),
    ] {
        let bytes = odf_package(styles, &masters, "", body);
        let (rendered, reported, _) = emitted(&bytes, Format::Odt, HeaderFooterPolicy::Emit);
        assert_eq!(rendered, markdown, "{masters}");
        assert_eq!(reported, [], "{masters}");
    }
}

const PML_NS: &str = r#"xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

fn pptx_deck(slides: &[(&str, Option<&str>)]) -> Vec<u8> {
    let ids: String =
        (0..slides.len()).map(|i| format!(r#"<p:sldId id="{}" r:id="s{i}"/>"#, 256 + i)).collect();
    let mut parts = vec![
        (
            "ppt/presentation.xml".to_string(),
            format!("<p:presentation {PML_NS}><p:sldIdLst>{ids}</p:sldIdLst></p:presentation>"),
        ),
        (
            "ppt/_rels/presentation.xml.rels".to_string(),
            relationships(
                &(0..slides.len())
                    .map(|i| (format!("s{i}"), format!("slides/slide{i}.xml")))
                    .collect::<Vec<_>>()
                    .iter()
                    .map(|(id, target)| (id.as_str(), "slide", target.as_str()))
                    .collect::<Vec<_>>(),
            ),
        ),
    ];
    let tree = |root: &str, shapes: &str| {
        format!("<p:{root} {PML_NS}><p:cSld><p:spTree>{shapes}</p:spTree></p:cSld></p:{root}>")
    };
    for (i, (shapes, notes)) in slides.iter().enumerate() {
        parts.push((format!("ppt/slides/slide{i}.xml"), tree("sld", shapes)));
        if let Some(notes) = notes {
            parts.push((
                format!("ppt/slides/_rels/slide{i}.xml.rels"),
                relationships(&[("n", "notesSlide", &format!("../notesSlides/notes{i}.xml"))]),
            ));
            parts.push((format!("ppt/notesSlides/notes{i}.xml"), tree("notes", notes)));
        }
    }
    zip_package(&parts)
}

fn placeholder(kind: &str, paragraphs: &str) -> String {
    let ph = if kind.is_empty() { String::new() } else { format!(r#"<p:ph type="{kind}"/>"#) };
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="s"/><p:cNvSpPr/><p:nvPr>{ph}</p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/>{paragraphs}</p:txBody></p:sp>"#
    )
}

#[test]
fn a_slide_and_its_notes_page_show_their_footer_and_header_placeholders() {
    let field = |kind: &str, cached: &str| {
        format!(r#"<a:p><a:fld id="{{1}}" type="{kind}"><a:t>{cached}</a:t></a:fld></a:p>"#)
    };
    let footers = |number: &str| {
        [
            placeholder("dt", &field("datetime1", "1/1/2026")),
            placeholder("ftr", r#"<a:p><a:r><a:rPr b="1"/><a:t>Draft</a:t></a:r></a:p>"#),
            placeholder("sldNum", &field("slidenum", number)),
        ]
        .concat()
    };
    let notes = [
        placeholder("hdr", "<a:p><a:r><a:t>Handout</a:t></a:r></a:p>"),
        placeholder("body", "<a:p><a:r><a:t>Speak</a:t></a:r></a:p>"),
    ]
    .concat();
    let one = format!(
        "{}{}",
        placeholder("title", "<a:p><a:r><a:t>Hello</a:t></a:r></a:p>"),
        footers("1")
    );
    let three = placeholder("", "<a:p><a:r><a:t>Three</a:t></a:r></a:p>");
    let bytes = pptx_deck(&[(&one, Some(&notes)), (&footers("2"), None), (&three, None)]);
    let (markdown, rows, _) = emitted(&bytes, Format::Pptx, HeaderFooterPolicy::Emit);
    assert_eq!(
        markdown,
        concat!(
            "> <a id=\"almiladoc:footer:default:1\"></a>Footer (default):\n>\n",
            "> {date}\n>\n> **Draft**\n>\n> {page}\n\n## Hello\n\n",
            "> > <a id=\"almiladoc:header:default:2\"></a>Header (default):\n> >\n> > Handout\n>\n> Speak\n\n",
            "---\n\n> <a id=\"almiladoc:footer:default:3\"></a>Footer (default):\n\nThree\n",
        )
    );
    assert_eq!(rows, [(DiagnosticCode::FieldToken, Region::Footer, Some(1), 2)]);
    let (omitted, rows, _) = emitted(&bytes, Format::Pptx, HeaderFooterPolicy::Omit);
    assert_eq!(omitted, "## Hello\n\n> Speak\n\n---\n\nThree\n");
    assert_eq!(
        rows,
        [
            (DiagnosticCode::HeaderOmitted, Region::Header, Some(1), 1),
            (DiagnosticCode::FooterOmitted, Region::Footer, Some(1), 3),
            (DiagnosticCode::FooterOmitted, Region::Footer, Some(2), 3),
        ]
    );
}

fn rtf(body: &str) -> (String, Reported, usize) {
    emitted(body.as_bytes(), Format::Rtf, HeaderFooterPolicy::Emit)
}

#[test]
fn an_rtf_section_shows_the_headers_it_defines_or_inherits_once_its_settings_are_read() {
    let source = concat!(
        r"{\rtf1\ansi\facingp{\fonttbl{\f0 Arial;}}{\stylesheet{\s1\b Strong;}}",
        r"\sectd{\header \pard\plain\s1 Head \chpgn\par}{\headerl \pard Left\par}",
        r"{\footerf \pard First foot\par}\titlepg",
        r"{\footer \pard {\field{\*\fldinst PAGE}{\fldrslt 9}} of {\field{\*\fldinst NUMPAGES}{\fldrslt 3}}\par}",
        r"\pard One\par\sect\sectd",
        r"{\header \pard\trowd\cellx1000 \pard\intbl cell\cell\row \pard after\par}",
        r"\pard Two\par}",
    );
    let (markdown, rows, _) = rtf(source);
    assert_eq!(
        rows,
        [
            (DiagnosticCode::FieldToken, Region::Header, None, 1),
            (DiagnosticCode::FieldToken, Region::Footer, None, 2)
        ]
    );
    assert_eq!(
        markdown,
        concat!(
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n> **Head {page}**\n\n",
            "> <a id=\"almiladoc:header:first:2\"></a>Header (first):\n\n",
            "> <a id=\"almiladoc:header:even:3\"></a>Header (even):\n>\n> Left\n\n",
            "> <a id=\"almiladoc:footer:default:4\"></a>Footer (default):\n>\n> {page} of {pages}\n\n",
            "> <a id=\"almiladoc:footer:first:5\"></a>Footer (first):\n>\n> First foot\n\n",
            "One\n\n",
            "> <a id=\"almiladoc:header:default:6\"></a>Header (default):\n>\n> |  |\n> | --- |\n> | cell |\n>\n> after\n\n",
            "> <a id=\"almiladoc:header:first:7\"></a>Header (first, uses default):\n\n",
            "> <a id=\"almiladoc:footer:first:8\"></a>Footer (first, uses default):\n\n",
            "Two\n",
        )
    );
}

#[test]
fn rtf_page_settings_choose_the_variants_only_from_the_body() {
    let header = |n: usize, variant: &str, text: &str| {
        let body = if text.is_empty() { String::new() } else { format!(">\n> {text}\n") };
        format!("> <a id=\"almiladoc:header:{variant}:{n}\"></a>Header ({variant}):\n{body}\n")
    };
    for (settings, groups, markdown) in [
        (
            r"\facingp",
            r"{\header A}{\headerr R}",
            [header(1, "default", "R"), header(2, "even", "A")].concat(),
        ),
        (r"\facingp", r"{\headerr R}", [header(1, "default", "R"), header(2, "even", "")].concat()),
        (r"\facingp0", r"{\header A}{\headerr R}{\headerl L}", header(1, "default", "A")),
        (r"\titlepg0", r"{\header A}{\headerf F}", header(1, "default", "A")),
        ("", r"{\header A\titlepg}{\headerf F}", header(1, "default", "A")),
        ("", r"{\info{\facingp}}{\header A}{\headerl L}", header(1, "default", "A")),
    ] {
        let source = format!(r"{{\rtf1{settings}{groups}\pard x\par}}");
        let (rendered, rows, _) = rtf(&source);
        assert_eq!((rendered, rows), (format!("{markdown}x\n"), Vec::new()), "{source}");
    }
}

#[test]
fn an_rtf_header_shows_only_where_its_holder_does_and_reads_nothing_it_does_not_show() {
    for (source, markdown, expected, assets) in [
        (
            r"{\rtf1{\headerf {\pict\pngblip 89504e470d0a1a0a}\par}{\header \pard H\par}\pard a\par}",
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n> H\n\na\n",
            vec![],
            0,
        ),
        (
            r"{\rtf1{\info{\header x}}{\field{\*\fldinst {\header y}}{\fldrslt r}}{\deleted{\header z}}a\par}",
            "ra\n",
            vec![(DiagnosticCode::HeaderOmitted, Region::Header, None, 1)],
            0,
        ),
        (
            r"{\rtf1\ansi\ansicpg932{\header {\field{\*\fldinst PAGE}{\fldrslt \'82{\pict\pngblip 89504e470d0a1a0a}}}\par}a\par}",
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n> {page}\n\na\n",
            vec![(DiagnosticCode::FieldToken, Region::Header, None, 1)],
            0,
        ),
        (
            r#"{\rtf1{\header {\*\bkmkstart top}T {\field{\*\fldinst HYPERLINK \\l "top"}{\fldrslt here}}\par}{\*\bkmkstart top}B {\field{\*\fldinst HYPERLINK \\l "top"}{\fldrslt up}}\par}"#,
            concat!(
                "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n",
                "> <a id=\"almiladoc:header:default:1:top\"></a>T [here](#almiladoc:header:default:1:top)\n\n",
                "<a id=\"top\"></a>B [up](#top)\n",
            ),
            vec![],
            0,
        ),
    ] {
        assert_eq!(rtf(source), (markdown.to_string(), expected, assets), "{source}");
    }
}

#[test]
#[cfg(feature = "pdf")]
fn a_pdf_s_running_heads_become_its_header_under_emit_and_skip_pages_ocr_replaces() {
    let bytes = crate::support::fixture("pdf/handmade-header-only.pdf");
    let (omitted, rows, _) = emitted(&bytes, Format::Pdf, HeaderFooterPolicy::Omit);
    assert!(!omitted.contains("Journal"), "{omitted}");
    assert_eq!(
        rows.iter().map(|row| row.0).collect::<Vec<_>>(),
        [DiagnosticCode::HeaderOmitted; 3]
    );
    let (markdown, rows, _) = emitted(&bytes, Format::Pdf, HeaderFooterPolicy::Emit);
    assert_eq!(markdown.matches("Header (default):").count(), 1, "{markdown}");
    assert!(
        markdown.starts_with("> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n> "),
        "{markdown}"
    );
    assert!(markdown.contains("Journal"), "{markdown}");
    assert_eq!(rows, []);
    let converter = configured(|config| {
        config.headers_footers = HeaderFooterPolicy::Emit;
        config.on_loss = OnLoss::Report;
    });
    let first = almiladoc::PageSelection::Pages([std::num::NonZeroU32::MIN].into_iter().collect());
    let conversion =
        converter.convert(Input::new(&bytes, Format::Pdf).with_ocr_pages(first)).unwrap();
    let markdown = conversion.markdown();
    assert!(markdown.starts_with("<!-- needs-ocr page 1 -->\n\n> <a id=\"almiladoc:header:default:1\"></a>Header (default):"), "{markdown}");
    assert_eq!(markdown.matches("Header (default):").count(), 1, "{markdown}");
    assert_eq!(
        reported(conversion.diagnostics()),
        [(DiagnosticCode::OcrGap, Region::Body, Some(1), 1)]
    );
}

#[test]
fn a_slide_writes_the_layout_and_master_content_it_shows_as_its_header() {
    let bytes = layered_deck();
    let (markdown, rows, assets) = emitted(&bytes, Format::Pptx, HeaderFooterPolicy::Emit);
    assert_eq!(
        markdown,
        concat!(
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n> ![Image](asset:0)\n>\n> Logo text\n\nOne\n\n---\n\n",
            "> <a id=\"almiladoc:header:default:2\"></a>Header (default):\n>\n> Logo text\n\nTwo\n",
        )
    );
    assert_eq!(assets, 1);
    assert_eq!(rows, [(DiagnosticCode::ImagePlaceholder, Region::Header, Some(1), 1)]);
    let (omitted, rows, _) = emitted(&bytes, Format::Pptx, HeaderFooterPolicy::Omit);
    assert_eq!(omitted, "One\n\n---\n\nTwo\n");
    assert_eq!(rows, [(DiagnosticCode::HeaderOmitted, Region::Header, None, 1)]);
}

#[test]
fn a_slide_writes_the_master_page_content_its_page_style_shows_as_its_header() {
    let styles = r#"<draw:fill-image draw:name="Paper" xlink:href="Pictures/paper.png"/>"#;
    let masters = concat!(
        r#"<style:master-page style:name="M" draw:style-name="mp"><draw:frame><draw:text-box><text:p>Logo</text:p></draw:text-box></draw:frame>"#,
        r#"<draw:frame presentation:class="title"><draw:text-box><text:p>Master title</text:p></draw:text-box></draw:frame></style:master-page>"#,
    );
    let automatic = concat!(
        r#"<style:style style:name="mp" style:family="drawing-page"><style:drawing-page-properties draw:fill="bitmap" draw:fill-image-name="Paper"/></style:style>"#,
        r#"<style:style style:name="plain" style:family="drawing-page"><style:drawing-page-properties presentation:background-objects-visible="false"/></style:style>"#,
    );
    let body = concat!(
        r#"<office:presentation><draw:page draw:name="a" draw:master-page-name="M"><draw:frame><draw:text-box><text:p>One</text:p></draw:text-box></draw:frame></draw:page>"#,
        r#"<draw:page draw:name="b" draw:master-page-name="M" draw:style-name="plain"><draw:frame><draw:text-box><text:p>Two</text:p></draw:text-box></draw:frame></draw:page></office:presentation>"#,
    );
    let mut parts = vec![
        (
            "content.xml".to_string(),
            format!("<office:document-content {ODF_NS}><office:automatic-styles>{automatic}</office:automatic-styles><office:body>{body}</office:body></office:document-content>").into_bytes(),
        ),
        (
            "styles.xml".to_string(),
            format!("<office:document-styles {ODF_NS}><office:styles>{styles}</office:styles><office:automatic-styles>{automatic}</office:automatic-styles><office:master-styles>{masters}</office:master-styles></office:document-styles>").into_bytes(),
        ),
    ];
    parts.push(("Pictures/paper.png".to_string(), b"\x89PNG\r\n\x1a\n".to_vec()));
    let bytes = zip_package(&parts);
    let (markdown, rows, assets) = emitted(&bytes, Format::Odp, HeaderFooterPolicy::Emit);
    assert_eq!(
        markdown,
        concat!(
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n> ![Image](asset:0)\n>\n> Logo\n\nOne\n\n---\n\n",
            "> <a id=\"almiladoc:header:default:2\"></a>Header (default):\n>\n> ![Image](asset:0)\n\nTwo\n",
        )
    );
    assert_eq!(assets, 1);
    assert_eq!(
        rows,
        [
            (DiagnosticCode::ImagePlaceholder, Region::Header, Some(1), 1),
            (DiagnosticCode::ImagePlaceholder, Region::Header, Some(2), 1),
        ]
    );
    let (omitted, rows, _) = emitted(&bytes, Format::Odp, HeaderFooterPolicy::Omit);
    assert_eq!(omitted, "One\n\n---\n\nTwo\n");
    assert_eq!(rows, [(DiagnosticCode::HeaderOmitted, Region::Header, None, 1)]);
}

#[test]
fn a_shown_master_s_picture_resolves_through_the_master_s_own_relationships() {
    let picture = r#"<p:pic><p:nvPicPr><p:cNvPr id="4" name="mark"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="img"/></p:blipFill><p:spPr/></p:pic>"#;
    let tree = |root: &str, shapes: &str| {
        format!("<p:{root} {PML_NS}><p:cSld><p:spTree>{shapes}</p:spTree></p:cSld></p:{root}>")
    };
    let parts = vec![
        (
            "ppt/presentation.xml".to_string(),
            format!(r#"<p:presentation {PML_NS}><p:sldIdLst><p:sldId id="256" r:id="s0"/></p:sldIdLst></p:presentation>"#).into_bytes(),
        ),
        ("ppt/_rels/presentation.xml.rels".to_string(), relationships(&[("s0", "slide", "slides/slide0.xml")]).into_bytes()),
        ("ppt/slides/slide0.xml".to_string(), tree("sld", "").into_bytes()),
        (
            "ppt/slides/_rels/slide0.xml.rels".to_string(),
            relationships(&[("l", "slideLayout", "../slideLayouts/layout1.xml"), ("img", "image", "../media/slide.png")]).into_bytes(),
        ),
        ("ppt/slideLayouts/layout1.xml".to_string(), tree("sldLayout", "").into_bytes()),
        (
            "ppt/slideLayouts/_rels/layout1.xml.rels".to_string(),
            relationships(&[("img", "image", "../media/layout.png"), ("m", "slideMaster", "../slideMasters/master1.xml")]).into_bytes(),
        ),
        ("ppt/slideMasters/master1.xml".to_string(), tree("sldMaster", picture).into_bytes()),
        ("ppt/slideMasters/_rels/master1.xml.rels".to_string(), relationships(&[("img", "image", "../media/master.png")]).into_bytes()),
        ("ppt/media/master.png".to_string(), b"\x89PNG master".to_vec()),
        ("ppt/media/layout.png".to_string(), b"\x89PNG layout".to_vec()),
        ("ppt/media/slide.png".to_string(), b"\x89PNG slide".to_vec()),
    ];
    let bytes = zip_package(&parts);
    let converter = configured(|config| {
        config.headers_footers = HeaderFooterPolicy::Emit;
        config.on_loss = OnLoss::Report;
    });
    let parsed = converter.parse(Input::new(&bytes, Format::Pptx)).unwrap();
    let assets: Vec<&[u8]> =
        parsed.document().assets.iter().map(|asset| asset.bytes.as_slice()).collect();
    assert_eq!(assets, [&b"\x89PNG master"[..]]);
    assert_eq!(
        parsed.render().unwrap().markdown(),
        "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n> ![Image](asset:0)\n"
    );
}

#[test]
fn a_theme_picture_background_resolves_through_the_theme_and_a_slide_s_own_overrides_it() {
    let bytes = themed_deck();
    let converter = configured(|config| {
        config.headers_footers = HeaderFooterPolicy::Emit;
        config.on_loss = OnLoss::Report;
    });
    let parsed = converter.parse(Input::new(&bytes, Format::Pptx)).unwrap();
    let assets: Vec<&[u8]> =
        parsed.document().assets.iter().map(|asset| asset.bytes.as_slice()).collect();
    assert_eq!(assets, [&b"\x89PNG theme"[..]]);
    assert_eq!(
        parsed.render().unwrap().markdown(),
        concat!(
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default):\n>\n> ![Image](asset:0)\n\n---\n\n",
            "> <a id=\"almiladoc:header:default:2\"></a>Header (default):\n",
        )
    );
    let (_, rows, _) = emitted(&bytes, Format::Pptx, HeaderFooterPolicy::Omit);
    assert_eq!(rows, [(DiagnosticCode::HeaderOmitted, Region::Header, None, 1)]);
}

fn themed_deck() -> Vec<u8> {
    let tree = |root: &str, background: &str| {
        format!("<p:{root} {PML_NS}><p:cSld>{background}<p:spTree/></p:cSld></p:{root}>")
    };
    let theme = r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><a:themeElements><a:fmtScheme><a:fillStyleLst/><a:bgFillStyleLst><a:solidFill/><a:blipFill><a:blip r:embed="img"/></a:blipFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>"#;
    let to_layout = relationships(&[("l", "slideLayout", "../slideLayouts/layout1.xml")]);
    let parts = vec![
        (
            "ppt/presentation.xml".to_string(),
            format!(r#"<p:presentation {PML_NS}><p:sldIdLst><p:sldId id="256" r:id="s0"/><p:sldId id="257" r:id="s1"/></p:sldIdLst></p:presentation>"#).into_bytes(),
        ),
        (
            "ppt/_rels/presentation.xml.rels".to_string(),
            relationships(&[("s0", "slide", "slides/slide0.xml"), ("s1", "slide", "slides/slide1.xml")]).into_bytes(),
        ),
        ("ppt/slides/slide0.xml".to_string(), tree("sld", "").into_bytes()),
        ("ppt/slides/slide1.xml".to_string(), tree("sld", "<p:bg><p:bgPr><a:solidFill/></p:bgPr></p:bg>").into_bytes()),
        ("ppt/slides/_rels/slide0.xml.rels".to_string(), to_layout.clone().into_bytes()),
        ("ppt/slides/_rels/slide1.xml.rels".to_string(), to_layout.into_bytes()),
        ("ppt/slideLayouts/layout1.xml".to_string(), tree("sldLayout", r#"<p:bg><p:bgRef idx="1002"/></p:bg>"#).into_bytes()),
        (
            "ppt/slideLayouts/_rels/layout1.xml.rels".to_string(),
            relationships(&[("m", "slideMaster", "../slideMasters/master1.xml")]).into_bytes(),
        ),
        ("ppt/slideMasters/master1.xml".to_string(), tree("sldMaster", "").into_bytes()),
        (
            "ppt/slideMasters/_rels/master1.xml.rels".to_string(),
            relationships(&[("t", "theme", "../theme/theme1.xml"), ("img", "image", "../media/master.png")]).into_bytes(),
        ),
        ("ppt/theme/theme1.xml".to_string(), theme.as_bytes().to_vec()),
        ("ppt/theme/_rels/theme1.xml.rels".to_string(), relationships(&[("img", "image", "../media/theme.png")]).into_bytes()),
        ("ppt/media/theme.png".to_string(), b"\x89PNG theme".to_vec()),
        ("ppt/media/master.png".to_string(), b"\x89PNG master".to_vec()),
    ];
    zip_package(&parts)
}

fn layered_deck() -> Vec<u8> {
    let text = |name: &str, ph: &str, body: &str| {
        format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="3" name="{name}"/><p:cNvSpPr/><p:nvPr>{ph}</p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>{body}</a:t></a:r></a:p></p:txBody></p:sp>"#
        )
    };
    let picture_bg =
        r#"<p:bg><p:bgPr><a:blipFill><a:blip r:embed="img"/></a:blipFill></p:bgPr></p:bg>"#;
    let layout = format!(
        r#"<p:sldLayout {PML_NS} showMasterSp="0"><p:cSld>{picture_bg}<p:spTree>{}{}</p:spTree></p:cSld></p:sldLayout>"#,
        text("logo", "", "Logo text"),
        text("title", r#"<p:ph type="title"/>"#, "Click to edit")
    );
    let master = format!(
        "<p:sldMaster {PML_NS}><p:cSld><p:spTree>{}</p:spTree></p:cSld></p:sldMaster>",
        text("art", "", "Master art")
    );
    let slide = |background: &str, body: &str| {
        format!(
            "<p:sld {PML_NS}><p:cSld>{background}<p:spTree>{}</p:spTree></p:cSld></p:sld>",
            text("b", "", body)
        )
    };
    let to_layout = relationships(&[("l", "slideLayout", "../slideLayouts/layout1.xml")]);
    let parts = vec![
        (
            "ppt/presentation.xml".to_string(),
            format!(
                r#"<p:presentation {PML_NS}><p:sldIdLst><p:sldId id="256" r:id="s0"/><p:sldId id="257" r:id="s1"/></p:sldIdLst></p:presentation>"#
            ),
        ),
        (
            "ppt/_rels/presentation.xml.rels".to_string(),
            relationships(&[
                ("s0", "slide", "slides/slide0.xml"),
                ("s1", "slide", "slides/slide1.xml"),
            ]),
        ),
        ("ppt/slides/slide0.xml".to_string(), slide("", "One")),
        (
            "ppt/slides/slide1.xml".to_string(),
            slide("<p:bg><p:bgPr><a:solidFill/></p:bgPr></p:bg>", "Two"),
        ),
        ("ppt/slides/_rels/slide0.xml.rels".to_string(), to_layout.clone()),
        ("ppt/slides/_rels/slide1.xml.rels".to_string(), to_layout),
        ("ppt/slideLayouts/layout1.xml".to_string(), layout),
        (
            "ppt/slideLayouts/_rels/layout1.xml.rels".to_string(),
            relationships(&[
                ("img", "image", "../media/bg.png"),
                ("m", "slideMaster", "../slideMasters/master1.xml"),
            ]),
        ),
        ("ppt/slideMasters/master1.xml".to_string(), master),
        ("ppt/media/bg.png".to_string(), "\u{89}PNG".to_string()),
    ];
    zip_package(&parts)
}

#[test]
fn libreoffice_s_shared_first_and_left_print_headers_use_the_default() {
    let bytes = crate::support::fixture("ods/sheet.ods");
    let (markdown, _, _) = emitted(&bytes, Format::Ods, HeaderFooterPolicy::Emit);
    assert!(markdown.contains("Header (default) of page style Default:"), "{markdown}");
    assert!(markdown.contains("Footer (default) of page style Default:"), "{markdown}");
    assert!(!markdown.contains("(first)") && !markdown.contains("(even)"), "{markdown}");
}

#[test]
fn a_shared_odf_variant_uses_the_default_and_an_empty_one_still_clears() {
    let masters = concat!(
        r#"<style:master-page style:name="Standard"><style:header><text:p>h</text:p></style:header>"#,
        r#"<style:header-left style:display="false"/><style:header-first style:display="false"/>"#,
        r#"<style:footer><text:p>f</text:p></style:footer><style:footer-left style:display="false"/>"#,
        r#"<style:footer-first/></style:master-page>"#,
    );
    let body = "<office:text><text:p>x</text:p></office:text>";
    let (markdown, _, _) =
        emitted(&odf_package("", masters, "", body), Format::Odt, HeaderFooterPolicy::Emit);
    assert_eq!(
        markdown,
        concat!(
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default) of page style Standard:\n>\n> h\n\n",
            "> <a id=\"almiladoc:footer:default:2\"></a>Footer (default) of page style Standard:\n>\n> f\n\n",
            "> <a id=\"almiladoc:footer:first:3\"></a>Footer (first) of page style Standard:\n\nx\n",
        )
    );
}

#[test]
fn an_odf_header_keeps_a_fixed_field_s_value_and_tokens_what_printing_resolves() {
    let token = |text: &str| {
        (text.to_string(), vec![(DiagnosticCode::FieldToken, Region::Header, None, 1)])
    };
    let kept = |text: &str| (text.to_string(), Vec::new());
    for (field, expected) in [
        (r#"<text:date text:fixed="true">2024-01-02</text:date>"#, kept("2024-01-02")),
        (r#"<text:date text:fixed="1">2024-01-02</text:date>"#, kept("2024-01-02")),
        (r#"<text:date text:fixed="false">2024-01-02</text:date>"#, token("{date}")),
        (r#"<text:time text:fixed="true">10:00</text:time>"#, kept("10:00")),
        (r#"<text:time text:fixed="0">10:00</text:time>"#, token("{time}")),
        (r#"<text:page-number text:fixed="true">7</text:page-number>"#, kept("7")),
        ("<text:page-number>7</text:page-number>", token("{page}")),
        (
            r#"<text:file-name text:fixed="true" text:display="name">report</text:file-name>"#,
            kept("report"),
        ),
        (r#"<text:file-name text:display="name">report</text:file-name>"#, token("{file}")),
        (
            r#"<text:file-name text:display="name-and-extension">r.odt</text:file-name>"#,
            token("{file}"),
        ),
        ("<text:file-name>r.odt</text:file-name>", token("{file}")),
        (r#"<text:file-name text:display="path">/docs</text:file-name>"#, token("{path}")),
        (r#"<text:file-name text:display="full">/docs/r.odt</text:file-name>"#, token("{path}")),
    ] {
        let masters = format!(
            r#"<style:master-page style:name="Standard"><style:header><text:p>{field}</text:p></style:header></style:master-page>"#
        );
        let body = "<office:text><text:p>x</text:p></office:text>";
        let (markdown, rows, _) =
            emitted(&odf_package("", &masters, "", body), Format::Odt, HeaderFooterPolicy::Emit);
        let header = format!(
            "> <a id=\"almiladoc:header:default:1\"></a>Header (default) of page style Standard:\n>\n> {}\n\nx\n",
            expected.0
        );
        assert_eq!((markdown, rows), (header, expected.1), "{field}");
    }
}

#[test]
fn a_master_page_no_body_flow_block_names_never_shows() {
    let header = |name: &str| {
        format!(
            r#"<style:master-page style:name="{name}"><style:header><text:p>{name}</text:p></style:header></style:master-page>"#
        )
    };
    let masters = ["First", "B", "C", "D", "E", "F"].map(header).concat();
    let styles = ["B", "C", "D", "E", "F"]
        .map(|name| format!(r#"<style:style style:name="P{name}" style:family="paragraph" style:master-page-name="{name}"/>"#))
        .concat();
    let body = concat!(
        r#"<office:text><text:tracked-changes><text:changed-region text:id="c1"><text:deletion>"#,
        r#"<text:p text:style-name="PB">gone</text:p></text:deletion></text:changed-region></text:tracked-changes>"#,
        r#"<text:p>One<text:note text:note-class="footnote"><text:note-body><text:p text:style-name="PE">n</text:p></text:note-body></text:note></text:p>"#,
        r#"<text:section text:name="s" text:display="none"><text:p text:style-name="PC">hidden</text:p></text:section>"#,
        r#"<text:p text:style-name="PD"><text:hidden-paragraph text:is-hidden="true"/>quiet</text:p>"#,
        r#"<table:table><table:table-row><table:table-cell><text:p text:style-name="PF">cell</text:p></table:table-cell></table:table-row></table:table>"#,
        r#"<text:p>Two</text:p></office:text>"#,
    );
    let bytes = odf_package(&styles, &masters, "", body);
    let (markdown, rows, _) = emitted(&bytes, Format::Odt, HeaderFooterPolicy::Emit);
    assert!(markdown.starts_with("> <a id=\"almiladoc:header:default:1\"></a>Header (default) of page style First:\n>\n> First\n\nOne"), "{markdown}");
    assert_eq!(markdown.matches("Header (default)").count(), 1, "{markdown}");
    let (_, omitted, _) = emitted(&bytes, Format::Odt, HeaderFooterPolicy::Omit);
    assert_eq!(
        rows,
        omitted
            .into_iter()
            .filter(|row| row.0 != DiagnosticCode::HeaderOmitted)
            .collect::<Vec<_>>()
    );
}
