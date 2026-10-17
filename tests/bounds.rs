//! Every recursion over input nesting converts at the deepest
//! `max_nesting_depth` a converter accepts, on the pool's stack, and refuses
//! one level more as `ResourceLimit`; the legacy binary readers' record
//! nesting and optional streams stay within their limits; rendering stays
//! within the output and work limits.

use almiladoc::{Config, ConvertError, Converter, Format, Input, Limit, Limits, OnLoss};

const CEILING: u64 = Limits::MAX_NESTING_CEILING;

const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:v="urn:schemas-microsoft-com:vml""#;
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
    Case { name: "docx fields", format: Format::Docx, wrap: 0, per: 1, build: docx_link_fields },
    Case {
        name: "docx links and fields",
        format: Format::Docx,
        wrap: 0,
        per: 1,
        build: docx_links_and_fields,
    },
    Case {
        name: "docx unread links and fields",
        format: Format::Docx,
        wrap: 0,
        per: 1,
        build: docx_unread_links_and_fields,
    },
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
fn docx_fields_read_the_configured_limit() {
    let docx = docx_fields("PAGE", 9);
    assert!(converter(9).convert(Input::new(&docx, Format::Docx)).is_ok());
    assert!(refused(converter(8).convert(Input::new(&docx, Format::Docx)), 8));
}

#[test]
fn docx_simple_fields_nest_with_the_complex_fields_around_and_inside_them() {
    let (begin, end) = (field_begin("PAGE"), field_char("end"));
    let simple = r#"<w:fldSimple w:instr="PAGE">"#;
    let docx = docx(&format!(
        "<w:p>{}{}{}{DOCX_TEXT}{}{}{}</w:p>",
        begin.repeat(4),
        simple.repeat(2),
        begin.repeat(5),
        end.repeat(5),
        "</w:fldSimple>".repeat(2),
        end.repeat(4)
    ));
    assert!(converter(11).convert(Input::new(&docx, Format::Docx)).is_ok());
    assert!(refused(converter(10).convert(Input::new(&docx, Format::Docx)), 10));
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
    assert!(refused_at_default(&docx_fields("PAGE", 150_000), Format::Docx));
    let links = docx_links_around_fields(UNREAD_LINK, 250, 250);
    assert!(refused_at_default(&docx(&format!("<w:p>{links}</w:p>")), Format::Docx));
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

#[test]
fn a_cell_holds_only_the_whitespace_its_line_owes_against_max_output_bytes() {
    let tail: usize = 1_000;
    let line =
        format!(r#"<w:r><w:t xml:space="preserve">x{}</w:t><w:br/></w:r>"#, " ".repeat(tail));
    let cell =
        format!("<w:tbl><w:tr><w:tc><w:p>{}</w:p></w:tc></w:tr></w:tbl>", line.repeat(1_000));
    let docx = docx(&cell);
    let input = || Input::new(&docx, Format::Docx);
    let markdown = limited(|_| {}).convert(input()).unwrap().into_markdown();
    let len = markdown.len() as u64;
    assert!(markdown.contains("x<br>x") && len < 6_000, "{len}");
    let at = |max| limited(|limits| limits.max_output_bytes = max).convert(input());
    assert_eq!(at(len + tail as u64).unwrap().markdown(), markdown);
    assert!(refused_with(at(len), Limit::OutputBytes, len));
}

const CHART: &str = "http://schemas.openxmlformats.org/drawingml/2006/chart";

/// A chart of `categories` categories, its first series holding them and a
/// value, each of its `series` - 1 others one value.
fn chart(categories: usize, series: usize) -> String {
    let point = |i: usize| format!(r#"<c:pt idx="{i}"><c:v>{i}</c:v></c:pt>"#);
    let cats: String = (0..categories).map(point).collect();
    let value = format!("<c:val><c:numCache>{}</c:numCache></c:val>", point(0));
    let first = format!("<c:ser><c:cat><c:strCache>{cats}</c:strCache></c:cat>{value}</c:ser>");
    let rest = format!("<c:ser>{value}</c:ser>").repeat(series - 1);
    format!(
        r#"<c:chartSpace xmlns:c="{CHART}"><c:chart><c:plotArea><c:barChart>{first}{rest}</c:barChart></c:plotArea></c:chart></c:chartSpace>"#
    )
}

/// A docx and a pptx that each draw `part` as their one chart.
fn charted(part: &str) -> [(Vec<u8>, Format); 2] {
    let graphic = format!(
        r#"<a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:graphicData><c:chart xmlns:c="{CHART}" r:id="rId1"/></a:graphicData></a:graphic>"#
    );
    let rels = |target: &str| {
        format!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="{target}"/></Relationships>"#
        )
    };
    let body = format!("<w:p><w:r><w:drawing>{graphic}</w:drawing></w:r></w:p>");
    let docx = zip(&[
        ("word/document.xml", &docx_document(&body)),
        ("word/_rels/document.xml.rels", &rels("charts/chart1.xml")),
        ("word/charts/chart1.xml", part),
    ]);
    let pptx = pptx_with(
        &format!("<p:graphicFrame>{graphic}</p:graphicFrame>"),
        &[
            ("ppt/slides/_rels/slide1.xml.rels", &rels("../charts/chart1.xml")),
            ("ppt/charts/chart1.xml", part),
        ],
    );
    [(docx, Format::Docx), (pptx, Format::Pptx)]
}

#[test]
fn a_chart_table_is_admitted_against_max_grid_slots_before_it_is_built() {
    for (data, format) in charted(&chart(3, 3)) {
        let at =
            |max| limited(|limits| limits.max_grid_slots = max).convert(Input::new(&data, format));
        let markdown = at(16).unwrap().into_markdown();
        assert!(markdown.contains("| 0 | 0 | 0 | 0 |"), "{format:?}: {markdown}");
        assert!(refused_with(at(15), Limit::GridSlots, 15), "{format:?}");
    }
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

fn field_char(kind: &str) -> String {
    format!(r#"<w:r><w:fldChar w:fldCharType="{kind}"/></w:r>"#)
}

/// The runs that open a complex field of `instruction` up to its result.
fn field_begin(instruction: &str) -> String {
    let instr = format!("<w:r><w:instrText>{instruction}</w:instrText></w:r>");
    [field_char("begin"), instr, field_char("separate")].concat()
}

/// A paragraph of `fields` nested complex fields of `instruction` around `x`.
fn docx_fields(instruction: &str, fields: usize) -> Vec<u8> {
    let (begin, end) = (field_begin(instruction), field_char("end"));
    docx(&format!("<w:p>{}{DOCX_TEXT}{}</w:p>", begin.repeat(fields), end.repeat(fields)))
}

fn docx_link_fields(units: usize) -> Vec<u8> {
    docx_fields("HYPERLINK x", units)
}

const UNREAD_LINK: &str = r#"r:id="rId9""#;

/// A paragraph of `levels` nested levels alternating, outermost first,
/// between a `w:hyperlink` with attribute `link` and a complex HYPERLINK
/// field, around `x`.
fn docx_alternating_links(link: &str, levels: usize) -> Vec<u8> {
    let (begin, end) = (field_begin("HYPERLINK x"), field_char("end"));
    let level = |l: usize| match l % 2 {
        0 => (format!("<w:hyperlink {link}>"), "</w:hyperlink>".to_string()),
        _ => (begin.clone(), end.clone()),
    };
    let opens: String = (0..levels).map(|l| level(l).0).collect();
    let closes: String = (0..levels).rev().map(|l| level(l).1).collect();
    docx(&format!("<w:p>{opens}{DOCX_TEXT}{closes}</w:p>"))
}

fn docx_links_and_fields(levels: usize) -> Vec<u8> {
    docx_alternating_links(r#"w:anchor="a""#, levels)
}

fn docx_unread_links_and_fields(levels: usize) -> Vec<u8> {
    docx_alternating_links(UNREAD_LINK, levels)
}

/// `links` nested `w:hyperlink` elements with attribute `link`, each
/// holding `fields` nested complex HYPERLINK fields around the next.
fn docx_links_around_fields(link: &str, links: usize, fields: usize) -> String {
    let open = format!("<w:hyperlink {link}>{}", field_begin("HYPERLINK x").repeat(fields));
    let close = format!("{}</w:hyperlink>", field_char("end").repeat(fields));
    nest(&open, &close, links, DOCX_TEXT)
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
    pptx(&nest("<p:grpSp>", "</p:grpSp>", units, shape))
}

fn pptx(tree: &str) -> Vec<u8> {
    pptx_with(tree, &[])
}

fn pptx_with(tree: &str, parts: &[(&str, &str)]) -> Vec<u8> {
    let presentation = format!(
        r#"<p:presentation {PML}><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#
    );
    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#;
    let slide = format!("<p:sld {PML}><p:cSld><p:spTree>{tree}</p:spTree></p:cSld></p:sld>");
    let deck = [
        ("ppt/presentation.xml", presentation.as_str()),
        ("ppt/_rels/presentation.xml.rels", rels),
        ("ppt/slides/slide1.xml", &slide),
    ];
    zip(&[&deck, parts].concat())
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

fn docx_table(rows: &str) -> Vec<u8> {
    docx(&format!("<w:tbl>{rows}</w:tbl>"))
}

#[test]
fn every_table_counts_its_positions_against_max_grid_slots() {
    let tcs =
        |cells: &[&str]| cells.iter().map(|c| format!("<w:tc>{c}</w:tc>")).collect::<String>();
    let wide = r#"<w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p/>"#;
    let p = "<w:p><w:r><w:t>x</w:t></w:r></w:p>";
    let docx = docx_table(&format!("<w:tr>{}</w:tr><w:tr>{}</w:tr>", tcs(&[wide]), tcs(&[p, p])));
    let a_tc = |attrs: &str| {
        format!(r#"<a:tc {attrs}><a:txBody><a:p><a:r><a:t>x</a:t></a:r></a:p></a:txBody></a:tc>"#)
    };
    let pptx = pptx(&format!(
        r#"<p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tr>{}</a:tr><a:tr>{}{}</a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>"#,
        a_tc(r#"gridSpan="2""#),
        a_tc(""),
        a_tc("")
    ));
    let cell = "<table:table-cell><text:p>x</text:p></table:table-cell>";
    let odt = odf(&format!(
        r#"<office:text><table:table><table:table-row><table:table-cell table:number-columns-spanned="2"><text:p>x</text:p></table:table-cell><table:covered-table-cell/></table:table-row><table:table-row>{cell}{cell}</table:table-row></table:table></office:text>"#
    ));
    let epub =
        epub(r#"<table><tr><td colspan="2">x</td></tr><tr><td>x</td><td>x</td></tr></table>"#);
    let row = r"\trowd\cellx1000\cellx2000\intbl x\cell x\cell\row ";
    let rtf = rtf(&row.repeat(2));
    let cases =
        [("docx", docx, Format::Docx), ("pptx", pptx, Format::Pptx), ("odt", odt, Format::Odt)]
            .into_iter()
            .chain([("epub", epub, Format::Epub), ("rtf", rtf, Format::Rtf)]);
    for (name, bytes, format) in cases {
        let at =
            |max| limited(|limits| limits.max_grid_slots = max).convert(Input::new(&bytes, format));
        assert!(at(4).is_ok(), "{name}: {:?}", at(4).err());
        assert!(refused_with(at(3), Limit::GridSlots, 3), "{name}: {:?}", at(3).err());
    }
}

/// A DOCX of one table of `rows`, and the bytes of its document part.
fn docx_declared_positions(rows: &str) -> (Vec<u8>, u64) {
    let part = docx_document(&format!("<w:tbl>{rows}</w:tbl>"));
    (zip(&[("word/document.xml", &part)]), part.len() as u64)
}

#[test]
fn docx_spans_and_row_fillers_fail_on_the_grid_slots_within_linear_work() {
    let wide = r#"<w:tc><w:tcPr><w:gridSpan w:val="1000"/></w:tcPr><w:p/></w:tc>"#;
    let spans = docx_declared_positions(&format!("<w:tr>{}</w:tr>", wide.repeat(60_000)));
    let fillers = r#"<w:tr><w:trPr><w:gridBefore w:val="1000"/><w:gridAfter w:val="1000"/></w:trPr><w:tc><w:p/></w:tc></w:tr>"#;
    let fillers = docx_declared_positions(&fillers.repeat(10_000));
    let max = Limits::default().max_grid_slots;
    for (name, (docx, part_bytes)) in [("spans", spans), ("fillers", fillers)] {
        let work = part_bytes + max;
        let refusal =
            limited(|limits| limits.max_work = work).convert(Input::new(&docx, Format::Docx));
        assert!(refused_with(refusal, Limit::GridSlots, max), "{name}");
    }
}

#[test]
fn docx_positions_past_the_declared_grid_are_clamped_and_reported() {
    let docx = docx_table(
        r#"<w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid><w:tr><w:trPr><w:gridBefore w:val="5"/><w:gridAfter w:val="3"/></w:trPr><w:tc><w:tcPr><w:gridSpan w:val="4"/></w:tcPr><w:p><w:r><w:t>x</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:tcPr><w:gridSpan w:val="7"/></w:tcPr><w:p><w:r><w:t>y</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>z</w:t></w:r></w:p></w:tc></w:tr>"#,
    );
    let conversion = limited(|_| {}).convert(Input::new(&docx, Format::Docx)).unwrap();
    let repaired: Vec<_> =
        conversion.diagnostics().iter().map(|d| (d.code.as_str(), d.count)).collect();
    assert_eq!(repaired, [("table_span_repaired", 4)]);
    assert_eq!(
        conversion.markdown(),
        "|  |  |  |\n| --- | --- | --- |\n|  |  | x |\n| y |  | z |\n"
    );
}

#[test]
fn a_docx_merge_continuation_past_the_merge_above_stays_its_own_cell() {
    let p = |text: &str| format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>");
    let restart = format!(r#"<w:tc><w:tcPr><w:vMerge w:val="restart"/></w:tcPr>{}</w:tc>"#, p("a"));
    let continued = r#"<w:tc><w:tcPr><w:vMerge/></w:tcPr><w:p/></w:tc>"#;
    let docx = docx_table(&format!(
        "<w:tr>{restart}</w:tr><w:tr><w:tc>{}</w:tc>{continued}</w:tr>",
        p("b")
    ));
    let conversion = limited(|_| {}).convert(Input::new(&docx, Format::Docx)).unwrap();
    assert_eq!(conversion.markdown(), "|  |\n| --- |\n| a |\n| b |\n");
}

#[test]
fn a_span_past_the_supplied_rows_counts_each_position_it_reserves() {
    let odt = |rows: u32| {
        odf(&format!(
            r#"<office:text><table:table><table:table-row><table:table-cell table:number-rows-spanned="{rows}"><text:p>x</text:p></table:table-cell></table:table-row></table:table></office:text>"#
        ))
    };
    let work = |rows| least_work_of(&odt(rows), Format::Odt);
    assert_eq!(work(1000) - work(1), 999);
    let spanned = odt(1000);
    let at = |max| {
        limited(|limits| limits.max_grid_slots = max).convert(Input::new(&spanned, Format::Odt))
    };
    assert!(at(1000).is_ok());
    assert!(refused_with(at(999), Limit::GridSlots, 999));
}

#[test]
fn odf_repeated_nested_tables_count_every_copy_against_max_grid_slots() {
    let cell = "<table:table-cell><text:p>x</text:p></table:table-cell>";
    let row = format!("<table:table-row>{cell}{cell}</table:table-row>");
    let odt = odf(&format!(
        r#"<office:text><table:table><table:table-row table:number-rows-repeated="2"><table:table-cell table:number-columns-repeated="3"><table:table>{row}{row}</table:table></table:table-cell></table:table-row></table:table></office:text>"#
    ));
    let at =
        |max| limited(|limits| limits.max_grid_slots = max).convert(Input::new(&odt, Format::Odt));
    assert!(at(30).is_ok(), "{:?}", at(30).err());
    assert!(refused_with(at(29), Limit::GridSlots, 29), "{:?}", at(29).err());
}

#[test]
fn odf_repeated_cells_copy_the_tables_their_lists_hold_and_not_their_notes() {
    let pair = "<table:table><table:table-row><table:table-cell><text:p>x</text:p></table:table-cell><table:table-cell><text:p>x</text:p></table:table-cell></table:table-row></table:table>";
    let odt = odf(&format!(
        r#"<office:text><table:table><table:table-row table:number-rows-repeated="2"><table:table-cell table:number-columns-repeated="3"><text:list><text:list-item>{pair}</text:list-item></text:list><text:p>y<text:note text:id="n1"><text:note-body>{pair}</text:note-body></text:note></text:p></table:table-cell></table:table-row></table:table></office:text>"#
    ));
    let at = |set: fn(&mut Limits, u64), max| {
        limited(|limits| set(limits, max)).convert(Input::new(&odt, Format::Odt))
    };
    let slots: fn(&mut Limits, u64) = |limits, max| limits.max_grid_slots = max;
    let bytes: fn(&mut Limits, u64) = |limits, max| limits.max_expansion_text_bytes = max;
    assert!(at(slots, 20).is_ok(), "{:?}", at(slots, 20).err());
    assert!(refused_with(at(slots, 19), Limit::GridSlots, 19), "{:?}", at(slots, 19).err());
    assert!(at(bytes, 25).is_ok(), "{:?}", at(bytes, 25).err());
    assert!(
        refused_with(at(bytes, 24), Limit::ExpansionTextBytes, 24),
        "{:?}",
        at(bytes, 24).err()
    );
}

#[test]
fn odf_covered_markers_and_inner_gaps_charge_the_expansion_budget() {
    let row = |inner: &str| {
        let cell = "<table:table-cell><text:p>x</text:p></table:table-cell>";
        odf(&format!(
            "<office:text><table:table><table:table-row>{cell}{inner}{cell}</table:table-row></table:table></office:text>"
        ))
    };
    let gap = row(r#"<table:table-cell table:number-columns-repeated="20"/>"#);
    let covered = row(r#"<table:covered-table-cell table:number-columns-repeated="20"/>"#);
    for (name, odt) in [("gap", gap), ("covered", covered)] {
        let at = |max| {
            limited(|limits| limits.max_expansion = max).convert(Input::new(&odt, Format::Odt))
        };
        assert!(at(22).is_ok(), "{name}: {:?}", at(22).err());
        assert!(refused_with(at(10), Limit::Expansion, 10), "{name}: {:?}", at(10).err());
    }
}

fn least_work_of(bytes: &[u8], format: Format) -> u64 {
    least_work(|converter| converter.convert(Input::new(bytes, format)).is_ok())
}

#[test]
fn docx_numbering_charges_the_conversion_once_per_abstract_it_resolves() {
    let nums = 1000;
    let docx = |abstract_of: fn(u64) -> u64| {
        let nums: String = (0..nums)
            .map(|i| {
                let abs = abstract_of(i);
                format!(r#"<w:num w:numId="{i}"><w:abstractNumId w:val="a{abs:04}"/></w:num>"#)
            })
            .collect();
        let numbering = format!("<w:numbering {W}>{nums}</w:numbering>");
        let document = docx_document("<w:p/>");
        zip(&[("word/document.xml", &document), ("word/numbering.xml", &numbering)])
    };
    let work = |abstract_of| least_work_of(&docx(abstract_of), Format::Docx);
    assert_eq!(work(|i| i) - work(|_| 0), nums - 1);
}

#[test]
fn odf_style_resolution_charges_the_conversion_once_per_style() {
    let depth = 1000;
    let chain = |family: &str, prefix: char| -> String {
        (0..depth)
            .map(|i: u64| {
                let parent = i.checked_sub(1).map_or(String::new(), |parent| {
                    format!(r#" style:parent-style-name="{prefix}{parent:04}""#)
                });
                format!(
                    r#"<style:style style:name="{prefix}{i:04}" style:family="{family}"{parent}/>"#
                )
            })
            .collect()
    };
    let styles = [chain("paragraph", 'p'), chain("text", 't'), chain("table", 'b')].concat();
    let document = |body: String| {
        let content = format!(
            r#"<office:document-content {ODF} xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:automatic-styles>{styles}</office:automatic-styles><office:body>{body}</office:body></office:document-content>"#
        );
        zip(&[("content.xml", &content)])
    };
    let text = |paragraph: &str, span: &str| {
        let odt = document(format!(
            r#"<office:text><text:p text:style-name="{paragraph}"><text:span text:style-name="{span}">x</text:span></text:p></office:text>"#
        ));
        least_work_of(&odt, Format::Odt)
    };
    let sheet = |table: &str| {
        let ods = document(format!(
            r#"<office:spreadsheet><table:table table:name="s" table:style-name="{table}"><table:table-row><table:table-cell><text:p>x</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet>"#
        ));
        least_work_of(&ods, Format::Ods)
    };
    let least = text("p0000", "t0000");
    assert_eq!(text("p0999", "t0000") - least, 3 * (depth - 1));
    assert_eq!(text("p0000", "t0999") - least, depth - 1);
    assert_eq!(sheet("b0999") - sheet("b0000"), depth - 1);
}
