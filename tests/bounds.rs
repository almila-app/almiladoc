//! Every recursion over input nesting converts at the deepest
//! `max_nesting_depth` a converter accepts, on the pool's stack, and refuses
//! one level more as `ResourceLimit`; the legacy binary readers' record
//! nesting and optional streams stay within their limits; rendering stays
//! within the output and work limits.

use almiladoc::{Config, ConvertError, Converter, Format, Input, Limit, Limits, OnLoss};

const CEILING: u64 = Limits::MAX_NESTING_CEILING;

const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:v="urn:schemas-microsoft-com:vml""#;
const ODF: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:xlink="http://www.w3.org/1999/xlink""#;
const PML: &str = r#"xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

/// One recursion's input: `units` nested levels sit inside `wrap` levels of
/// fixed structure, each level costing `per` of the limit.
struct Case {
    name: &'static str,
    format: Format,
    wrap: u64,
    per: u64,
    build: fn(usize) -> Vec<u8>,
}

const CASES: &[Case] = &[
    Case { name: "docx tables", format: Format::Docx, wrap: 5, per: 3, build: docx_tables },
    Case { name: "docx hyperlinks", format: Format::Docx, wrap: 5, per: 1, build: docx_links },
    Case {
        name: "docx empty links",
        format: Format::Docx,
        wrap: 5,
        per: 1,
        build: docx_empty_links,
    },
    Case { name: "docx smart tags", format: Format::Docx, wrap: 5, per: 1, build: docx_smart_tags },
    Case { name: "docx run branches", format: Format::Docx, wrap: 5, per: 2, build: docx_branches },
    Case { name: "docx row wrappers", format: Format::Docx, wrap: 8, per: 1, build: docx_rows },
    Case { name: "docx drawing", format: Format::Docx, wrap: 5, per: 1, build: docx_drawing },
    Case { name: "docx math", format: Format::Docx, wrap: 6, per: 2, build: docx_math },
    Case { name: "odt lists", format: Format::Odt, wrap: 4, per: 2, build: odt_lists },
    Case { name: "odt spans", format: Format::Odt, wrap: 4, per: 1, build: odt_spans },
    Case { name: "odt tables", format: Format::Odt, wrap: 4, per: 3, build: odt_tables },
    Case { name: "odt links", format: Format::Odt, wrap: 4, per: 1, build: odt_links },
    Case { name: "odt row groups", format: Format::Odt, wrap: 7, per: 1, build: odt_row_groups },
    Case { name: "odt cell links", format: Format::Odt, wrap: 7, per: 1, build: odt_cell_links },
    Case { name: "odp groups", format: Format::Odp, wrap: 7, per: 1, build: odp_groups },
    Case { name: "pptx groups", format: Format::Pptx, wrap: 8, per: 1, build: pptx_groups },
    Case { name: "epub lists", format: Format::Epub, wrap: 2, per: 2, build: epub_lists },
    Case { name: "epub divs", format: Format::Epub, wrap: 3, per: 1, build: epub_divs },
    Case { name: "epub quotes", format: Format::Epub, wrap: 3, per: 1, build: epub_quotes },
    Case { name: "epub links", format: Format::Epub, wrap: 3, per: 1, build: epub_links },
    Case { name: "epub item links", format: Format::Epub, wrap: 5, per: 1, build: epub_item_links },
    Case {
        name: "epub item anchor links",
        format: Format::Epub,
        wrap: 5,
        per: 1,
        build: epub_item_anchor_links,
    },
    Case {
        name: "epub item quotes",
        format: Format::Epub,
        wrap: 7,
        per: 1,
        build: epub_item_quotes,
    },
    Case { name: "epub bold links", format: Format::Epub, wrap: 3, per: 2, build: epub_bold_links },
    Case {
        name: "epub heading links",
        format: Format::Epub,
        wrap: 3,
        per: 1,
        build: epub_heading_links,
    },
    Case { name: "epub math", format: Format::Epub, wrap: 5, per: 1, build: epub_math },
    Case { name: "rtf groups", format: Format::Rtf, wrap: 1, per: 1, build: rtf_groups },
    Case { name: "rtf fields", format: Format::Rtf, wrap: 1, per: 2, build: rtf_fields },
    Case { name: "rtf math", format: Format::Rtf, wrap: 3, per: 2, build: rtf_math },
    Case { name: "doc links", format: Format::Doc, wrap: 0, per: 1, build: doc_links },
];

fn converter(max_nesting_depth: u64) -> Converter {
    let mut config = Config::default();
    config.limits.max_nesting_depth = max_nesting_depth;
    config.on_loss = OnLoss::Report;
    Converter::new(config).unwrap()
}

fn refused(result: Result<almiladoc::Conversion, ConvertError>, max: u64) -> bool {
    matches!(result, Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: m, .. }) if m == max)
}

#[test]
fn every_recursion_converts_at_the_ceiling_and_refuses_one_level_more() {
    let converter = converter(CEILING);
    for case in CASES {
        let units = usize::try_from((CEILING - case.wrap) / case.per).unwrap();
        let deepest = (case.build)(units);
        let converted = converter.convert(Input::new(&deepest, case.format));
        assert!(converted.is_ok(), "{} at {units}: {converted:?}", case.name);
        let deeper = (case.build)(units + 1);
        let refusal = converter.convert(Input::new(&deeper, case.format));
        assert!(refused(refusal, CEILING), "{} at {}", case.name, units + 1);
    }
}

#[test]
fn doc_fields_read_the_configured_limit() {
    let doc = word_document(&doc_fields("PAGE", 9));
    assert!(converter(9).convert(Input::new(&doc, Format::Doc)).is_ok());
    assert!(refused(converter(8).convert(Input::new(&doc, Format::Doc)), 8));
}

#[test]
fn rtf_fields_open_in_one_group_read_the_configured_limit() {
    let rtf = rtf_flat_fields("PAGE", 9);
    assert!(converter(9).convert(Input::new(&rtf, Format::Rtf)).is_ok());
    assert!(refused(converter(8).convert(Input::new(&rtf, Format::Rtf)), 8));
}

#[test]
fn rtf_groups_read_the_configured_limit() {
    let rtf = rtf_groups(9);
    assert!(converter(10).convert(Input::new(&rtf, Format::Rtf)).is_ok());
    assert!(refused(converter(9).convert(Input::new(&rtf, Format::Rtf)), 9));
}

#[test]
fn fields_nested_far_past_the_limit_are_refused_while_parsing() {
    let fields = 300_000;
    let refused_at_default = |bytes: &[u8], format| {
        let max = Limits::default().max_nesting_depth;
        refused(converter(max).convert(Input::new(bytes, format)), max)
    };
    assert!(refused_at_default(&rtf_fields(fields), Format::Rtf));
    assert!(refused_at_default(&rtf_flat_fields("HYPERLINK x", fields), Format::Rtf));
    assert!(refused_at_default(&doc_links(fields), Format::Doc));
}

#[test]
fn a_document_nested_past_the_limit_is_refused_before_rendering() {
    let rtf = rtf_list(8);
    let converter = |max| converter(max).parse(Input::new(&rtf, Format::Rtf)).map(drop);
    assert!(converter(9).is_ok());
    let refusal = converter(8);
    assert!(
        matches!(
            refusal,
            Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 8, .. })
        ),
        "{refusal:?}"
    );
}

#[test]
fn a_chain_of_notes_each_referencing_the_next_numbers_in_reference_order() {
    let notes = 100_000;
    let note = |id: usize| {
        let reference = if id + 1 < notes {
            format!(r#"<w:r><w:footnoteReference w:id="{}"/></w:r>"#, id + 1)
        } else {
            String::new()
        };
        format!(
            r#"<w:footnote w:id="{id}"><w:p><w:r><w:t>n{id}</w:t></w:r>{reference}</w:p></w:footnote>"#
        )
    };
    let footnotes: String = (0..notes).map(note).collect();
    let footnotes = format!("<w:footnotes {W}>{footnotes}</w:footnotes>");
    let body = r#"<w:p><w:r><w:t>body</w:t></w:r><w:r><w:footnoteReference w:id="0"/></w:r></w:p>"#;
    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/></Relationships>"#;
    let bytes = zip(&[
        ("word/document.xml", docx_document(body).as_str()),
        ("word/_rels/document.xml.rels", rels),
        ("word/footnotes.xml", footnotes.as_str()),
    ]);
    let markdown = converter(Limits::default().max_nesting_depth)
        .convert(Input::new(&bytes, Format::Docx))
        .unwrap()
        .into_markdown();
    assert!(
        markdown.starts_with("body[^1]\n\n[^1]: n0[^2]\n\n[^2]: n1[^3]\n"),
        "{}",
        &markdown[..80]
    );
    assert!(markdown.ends_with(&format!("[^{notes}]: n{}\n", notes - 1)));
}

fn limited(set: impl FnOnce(&mut Limits)) -> Converter {
    let mut config = Config::default();
    set(&mut config.limits);
    config.on_loss = OnLoss::Report;
    Converter::new(config).unwrap()
}

/// A PowerPoint stream read in stream order whose notes containers nest
/// `depth` deep, each the only child of the one around it.
fn ppt_nested_notes(depth: usize) -> Vec<u8> {
    let mut stream = Vec::with_capacity(depth * 8);
    for level in (0..depth).rev() {
        let len = u32::try_from(level * 8).unwrap();
        stream.extend_from_slice(
            &[&0x000Fu16.to_le_bytes()[..], &0x03F0u16.to_le_bytes(), &len.to_le_bytes()].concat(),
        );
    }
    let mut file = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
    std::io::Write::write_all(&mut file.create_stream("PowerPoint Document").unwrap(), &stream)
        .unwrap();
    file.flush().unwrap();
    file.into_inner().into_inner()
}

#[test]
fn notes_nested_in_a_stream_order_read_stop_at_max_record_depth() {
    let refused_depth = |result: Result<almiladoc::Conversion, ConvertError>, max: u64| matches!(result, Err(ConvertError::ResourceLimit { limit: Limit::RecordDepth, max: m, .. }) if m == max);
    let max = Limits::default().max_record_depth;
    let deep = ppt_nested_notes(200_000);
    assert!(refused_depth(limited(|_| {}).convert(Input::new(&deep, Format::Ppt)), max));
    let notes = ppt_nested_notes(8);
    let at = |max_record_depth| {
        limited(|limits| limits.max_record_depth = max_record_depth)
            .convert(Input::new(&notes, Format::Ppt))
    };
    assert!(at(9).is_ok());
    assert!(refused_depth(at(8), 8));
}

#[test]
fn a_doc_data_stream_past_max_entry_bytes_is_a_limit() {
    let doc = word_document(b"x\r");
    let mut file = cfb::CompoundFile::open(std::io::Cursor::new(doc)).unwrap();
    std::io::Write::write_all(&mut file.create_stream("Data").unwrap(), &[0u8; 4096]).unwrap();
    file.flush().unwrap();
    let doc = file.into_inner().into_inner();
    let at = |max_entry_bytes| {
        limited(|limits| limits.max_entry_bytes = max_entry_bytes)
            .convert(Input::new(&doc, Format::Doc))
    };
    assert!(at(4096).is_ok());
    let refusal = at(4095);
    assert!(
        matches!(
            refusal,
            Err(ConvertError::ResourceLimit { limit: Limit::EntryBytes, max: 4095, .. })
        ),
        "{refusal:?}"
    );
}

fn csv_records() -> Vec<u8> {
    (0..50).map(|i| format!("cell {i},value {i}\n")).collect::<String>().into_bytes()
}

fn refused_with(
    result: Result<almiladoc::Conversion, ConvertError>,
    limit: Limit,
    max: u64,
) -> bool {
    matches!(result, Err(ConvertError::ResourceLimit { limit: l, max: m, .. }) if l == limit && m == max)
}

#[test]
fn convert_and_render_stop_at_max_output_bytes_and_a_parse_writes_none() {
    let csv = csv_records();
    let input = || Input::new(&csv, Format::Csv);
    let markdown = limited(|_| {}).convert(input()).unwrap().into_markdown();
    let len = markdown.len() as u64;
    let at = |max| limited(|limits| limits.max_output_bytes = max);
    assert_eq!(at(len).convert(input()).unwrap().markdown(), markdown);
    assert_eq!(at(len).parse(input()).unwrap().render().unwrap().markdown(), markdown);
    assert!(refused_with(at(len - 1).convert(input()), Limit::OutputBytes, len - 1));
    let parsed = at(1).parse(input()).unwrap();
    assert!(refused_with(parsed.render(), Limit::OutputBytes, 1));
}

#[cfg(feature = "pdf")]
#[test]
fn a_pdf_stops_at_max_output_bytes() {
    let pdf =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pdf/text.pdf")).unwrap();
    let input = || Input::new(&pdf, Format::Pdf);
    let len = limited(|_| {}).convert(input()).unwrap().markdown().len() as u64;
    let at = |max| limited(|limits| limits.max_output_bytes = max).convert(input());
    assert!(at(len).is_ok());
    assert!(refused_with(at(len - 1), Limit::OutputBytes, len - 1));
}

/// The least `max_work` under which `passes` holds, which holds for every
/// larger one.
fn least_work(passes: impl Fn(Converter) -> bool) -> u64 {
    let (mut low, mut high) = (0, 1 << 20);
    while low < high {
        let mid = low + (high - low) / 2;
        if passes(limited(|limits| limits.max_work = mid)) { high = mid } else { low = mid + 1 }
    }
    low
}

#[test]
fn rendering_charges_a_unit_of_work_per_64_bytes_on_top_of_the_parse() {
    let csv = csv_records();
    let input = || Input::new(&csv, Format::Csv);
    let len = limited(|_| {}).convert(input()).unwrap().markdown().len() as u64;
    let parse = least_work(|converter| converter.parse(input()).is_ok());
    let convert = least_work(|converter| converter.convert(input()).is_ok());
    assert!(parse > 0 && len >= 64, "{parse} {len}");
    assert_eq!(convert, parse + len / 64);
    let parsed = limited(|limits| limits.max_work = convert - 1).parse(input()).unwrap();
    assert!(refused_with(parsed.render(), Limit::Work, convert - 1));
    let parsed = limited(|limits| limits.max_work = convert).parse(input()).unwrap();
    assert!(parsed.render().is_ok());
}

#[test]
fn a_ragged_csv_writes_the_blank_cells_of_its_short_rows_against_max_output_bytes() {
    let fields = 100_000;
    let mut csv = "a,".repeat(fields - 1);
    csv.push_str("a\n");
    csv.push_str(&"a\n".repeat(fields));
    let refusal = limited(|_| {}).convert(Input::new(csv.as_bytes(), Format::Csv));
    let max = Limits::default().max_output_bytes;
    assert!(refused_with(refusal, Limit::OutputBytes, max));
}

fn zip(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, text) in parts {
        writer.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(&mut writer, text.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn nest(open: &str, close: &str, units: usize, inner: &str) -> String {
    format!("{}{inner}{}", open.repeat(units), close.repeat(units))
}

fn docx_document(body: &str) -> String {
    format!("<w:document {W}><w:body>{body}</w:body></w:document>")
}

fn docx(body: &str) -> Vec<u8> {
    zip(&[("word/document.xml", &docx_document(body))])
}

const DOCX_TEXT: &str = "<w:r><w:t>x</w:t></w:r>";

fn docx_tables(units: usize) -> Vec<u8> {
    docx(&nest(
        "<w:tbl><w:tr><w:tc>",
        "</w:tc></w:tr></w:tbl>",
        units,
        &format!("<w:p>{DOCX_TEXT}</w:p>"),
    ))
}

fn docx_links(units: usize) -> Vec<u8> {
    let links = nest(r##"<w:hyperlink w:anchor="nowhere">"##, "</w:hyperlink>", units, DOCX_TEXT);
    docx(&format!("<w:p>{links}</w:p>"))
}

fn docx_empty_links(units: usize) -> Vec<u8> {
    let links = nest(r#"<w:hyperlink w:anchor="">"#, "</w:hyperlink>", units, DOCX_TEXT);
    docx(&format!("<w:p>{links}</w:p>"))
}

fn docx_smart_tags(units: usize) -> Vec<u8> {
    docx(&format!("<w:p>{}</w:p>", nest("<w:smartTag>", "</w:smartTag>", units, DOCX_TEXT)))
}

fn docx_branches(units: usize) -> Vec<u8> {
    let branches = nest(
        "<mc:AlternateContent><mc:Fallback>",
        "</mc:Fallback></mc:AlternateContent>",
        units,
        "<w:t>x</w:t>",
    );
    docx(&format!("<w:p><w:r>{branches}</w:r></w:p>"))
}

fn docx_rows(units: usize) -> Vec<u8> {
    let cell = format!("<w:tc><w:p>{DOCX_TEXT}</w:p></w:tc>");
    docx(&format!(
        "<w:tbl><w:tr>{}</w:tr></w:tbl>",
        nest("<w:customXml>", "</w:customXml>", units, &cell)
    ))
}

fn docx_drawing(units: usize) -> Vec<u8> {
    let shapes = nest("<v:group>", "</v:group>", units, r#"<v:textpath string="x"/>"#);
    docx(&format!("<w:p><w:r><w:pict>{shapes}</w:pict></w:r></w:p>"))
}

fn docx_math(units: usize) -> Vec<u8> {
    let math = nest("<m:f><m:num>", "</m:num></m:f>", units, "<m:r><m:t>x</m:t></m:r>");
    docx(&format!("<w:p><m:oMath>{math}</m:oMath></w:p>"))
}

fn odf(body: &str) -> Vec<u8> {
    let content = format!(
        "<office:document-content {ODF}><office:body>{body}</office:body></office:document-content>"
    );
    zip(&[("content.xml", &content)])
}

fn odt_lists(units: usize) -> Vec<u8> {
    let lists = nest(
        "<text:list><text:list-item>",
        "</text:list-item></text:list>",
        units,
        "<text:p>x</text:p>",
    );
    odf(&format!("<office:text>{lists}</office:text>"))
}

fn odt_spans(units: usize) -> Vec<u8> {
    let spans = nest("<text:span>", "</text:span>", units, "x");
    odf(&format!("<office:text><text:p>{spans}</text:p></office:text>"))
}

fn odt_tables(units: usize) -> Vec<u8> {
    let tables = nest(
        "<table:table><table:table-row><table:table-cell>",
        "</table:table-cell></table:table-row></table:table>",
        units,
        "<text:p>x</text:p>",
    );
    odf(&format!("<office:text>{tables}</office:text>"))
}

const ODF_LINK: &str = r##"<text:a xlink:href="#nowhere">"##;

fn odt_links(units: usize) -> Vec<u8> {
    let links = nest(ODF_LINK, "</text:a>", units, "x");
    odf(&format!("<office:text><text:p>{links}</text:p></office:text>"))
}

fn odt_row_groups(units: usize) -> Vec<u8> {
    let row = "<table:table-row><table:table-cell><text:p>x</text:p></table:table-cell></table:table-row>";
    let groups = nest("<table:table-row-group>", "</table:table-row-group>", units, row);
    odf(&format!("<office:text><table:table>{groups}</table:table></office:text>"))
}

fn odt_cell_links(units: usize) -> Vec<u8> {
    let links = nest(ODF_LINK, "</text:a>", units, "x");
    odf(&format!(
        "<office:text><table:table><table:table-row><table:table-cell><text:p>{links}</text:p></table:table-cell></table:table-row></table:table></office:text>"
    ))
}

fn odp_groups(units: usize) -> Vec<u8> {
    let frame = "<draw:frame><draw:text-box><text:p>x</text:p></draw:text-box></draw:frame>";
    let groups = nest("<draw:g>", "</draw:g>", units, frame);
    odf(&format!("<office:presentation><draw:page>{groups}</draw:page></office:presentation>"))
}

fn pptx_groups(units: usize) -> Vec<u8> {
    let shape = "<p:sp><p:txBody><a:p><a:r><a:t>x</a:t></a:r></a:p></p:txBody></p:sp>";
    let tree = nest("<p:grpSp>", "</p:grpSp>", units, shape);
    let presentation = format!(
        r#"<p:presentation {PML}><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#
    );
    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#;
    let slide = format!("<p:sld {PML}><p:cSld><p:spTree>{tree}</p:spTree></p:cSld></p:sld>");
    zip(&[
        ("ppt/presentation.xml", &presentation),
        ("ppt/_rels/presentation.xml.rels", rels),
        ("ppt/slides/slide1.xml", &slide),
    ])
}

fn epub(body: &str) -> Vec<u8> {
    let container = r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
    let opf = r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0"><manifest><item id="c" href="c.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c"/></spine></package>"#;
    let chapter =
        format!(r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>{body}</body></html>"#);
    zip(&[
        ("mimetype", "application/epub+zip"),
        ("META-INF/container.xml", container),
        ("content.opf", opf),
        ("c.xhtml", &chapter),
    ])
}

fn epub_lists(units: usize) -> Vec<u8> {
    epub(&nest("<ul><li>", "</li></ul>", units, "x"))
}

fn epub_divs(units: usize) -> Vec<u8> {
    epub(&nest("<div>", "</div>", units, "<p>x</p>"))
}

fn epub_quotes(units: usize) -> Vec<u8> {
    epub(&nest("<blockquote>", "</blockquote>", units, "<p>x</p>"))
}

const EPUB_LINK: &str = r##"<a href="#nowhere">"##;

fn epub_links(units: usize) -> Vec<u8> {
    epub(&format!("<p>{}</p>", nest(EPUB_LINK, "</a>", units, "x")))
}

fn epub_item_links(units: usize) -> Vec<u8> {
    epub(&format!("<ul><li><p>{}</p></li></ul>", nest(EPUB_LINK, "</a>", units, "x")))
}

fn epub_item_anchor_links(units: usize) -> Vec<u8> {
    let links = nest(r##"<a href="#t">"##, "</a>", units, "x");
    epub(&format!(r#"<p id="t">t</p><ul><li><p>{links}</p></li></ul>"#))
}

fn epub_item_quotes(units: usize) -> Vec<u8> {
    let quotes = nest("<blockquote>", "</blockquote>", units, "<p>x</p>");
    epub(&format!("<ul><li><ul><li>{quotes}</li></ul></li></ul>"))
}

fn epub_bold_links(units: usize) -> Vec<u8> {
    epub(&format!("<p>{}</p>", nest(&format!("{EPUB_LINK}<b>"), "</b></a>", units, "x")))
}

fn epub_heading_links(units: usize) -> Vec<u8> {
    epub(&format!("<h1>{}</h1>", nest(EPUB_LINK, "</a>", units, "x")))
}

fn epub_math(units: usize) -> Vec<u8> {
    let rows = nest("<mrow>", "</mrow>", units, "<mi>x</mi>");
    epub(&format!(r#"<p><math xmlns="http://www.w3.org/1998/Math/MathML">{rows}</math></p>"#))
}

fn rtf(body: &str) -> Vec<u8> {
    format!("{{\\rtf1\\ansi {body}\\par}}").into_bytes()
}

fn rtf_groups(units: usize) -> Vec<u8> {
    rtf(&nest("{", "}", units, "x"))
}

fn rtf_fields(units: usize) -> Vec<u8> {
    rtf(&nest(r#"{\field{\*\fldinst HYPERLINK "https://example.com"}{\fldrslt "#, "}}", units, "x"))
}

fn rtf_flat_fields(instruction: &str, fields: usize) -> Vec<u8> {
    let field = format!(r"\field{{\*\fldinst {instruction}}}\fldrslt ");
    rtf(&format!("{}x", field.repeat(fields)))
}

fn rtf_math(units: usize) -> Vec<u8> {
    let math = nest(r"{\mf{\mnum ", "}}", units, r"{\mr x}");
    rtf(&format!(r"{{\mmath{{\*\moMath {math}}}}}"))
}

/// A paragraph of `fields` nested Word fields of `instruction` around `x`.
fn doc_fields(instruction: &str, fields: usize) -> Vec<u8> {
    let begin = format!("\x13{instruction}\x14");
    [begin.repeat(fields).as_bytes(), b"x", "\x15".repeat(fields).as_bytes(), b"\r"].concat()
}

fn doc_links(units: usize) -> Vec<u8> {
    word_document(&doc_fields("HYPERLINK x", units))
}

/// A Word 97 document of one compressed text piece: the FIB, then `text` as
/// code page 1252 at offset 0x400.
fn word_document(text: &[u8]) -> Vec<u8> {
    let mut stream = vec![0u8; 0x400];
    let mut put = |at: usize, bytes: &[u8]| stream[at..at + bytes.len()].copy_from_slice(bytes);
    put(0x00, &0xA5ECu16.to_le_bytes());
    put(0x02, &0x00C1u16.to_le_bytes());
    put(0x06, &0x0409u16.to_le_bytes());
    let end = u32::try_from(0x400 + text.len()).unwrap();
    put(0x18, &0x400u32.to_le_bytes());
    put(0x1C, &end.to_le_bytes());
    put(0x4C, &u32::try_from(text.len()).unwrap().to_le_bytes());
    stream.extend_from_slice(text);
    let mut file = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
    std::io::Write::write_all(&mut file.create_stream("WordDocument").unwrap(), &stream).unwrap();
    file.flush().unwrap();
    file.into_inner().into_inner()
}

fn rtf_list(levels: usize) -> Vec<u8> {
    let level = r"{\listlevel\levelnfc23{\leveltext\'01舦 ?;}}";
    let table = format!(
        r"{{\*\listtable{{\list\listtemplateid1{}\listid1}}}}{{\*\listoverridetable{{\listoverride\listid1\ls1}}}}",
        level.repeat(9)
    );
    let paragraphs: String =
        (0..=levels).map(|level| format!(r"\pard\ls1\ilvl{level} item {level}\par ")).collect();
    format!("{{\\rtf1\\ansi {table}{paragraphs}}}").into_bytes()
}
