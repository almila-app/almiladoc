//! Every recursion over input nesting converts at the deepest
//! `max_nesting_depth` a converter accepts, on the pool's stack, and refuses
//! one level more as `ResourceLimit`; the legacy binary readers' record
//! nesting and optional streams stay within their limits; rendering stays
//! within the output and work limits.

use crate::build::{
    W, docx, docx_document, docx_with, epub, odf, odf_styled, ods, ole, pptx, relationships,
};
use crate::support::{count, limit_mut, limited, trips, zip_package};
use crate::word97::word97;
use almiladoc::{
    Conversion, ConvertError, Converter, DiagnosticCode, Format, Input, Limit, Limits,
};
use std::assert_matches;

const CEILING: u64 = Limits::MAX_NESTING_CEILING;

type Build = fn(usize) -> Vec<u8>;

/// One recursion's input, `(name, format, wrap, per, build)`: `build(units)` nests `units`
/// levels inside `wrap` levels of fixed structure, each level costing `per` of the limit.
const CASES: &[(&str, Format, u64, u64, Build)] = &[
    ("docx tables", Format::Docx, 5, 3, docx_tables),
    ("docx hyperlinks", Format::Docx, 5, 1, docx_links),
    ("docx empty links", Format::Docx, 5, 1, docx_empty_links),
    ("docx smart tags", Format::Docx, 5, 1, docx_smart_tags),
    ("docx run branches", Format::Docx, 5, 2, docx_branches),
    ("docx row wrappers", Format::Docx, 8, 1, docx_rows),
    ("docx drawing", Format::Docx, 6, 1, docx_drawing),
    ("docx math", Format::Docx, 6, 2, docx_math),
    ("docx fields", Format::Docx, 0, 1, docx_link_fields),
    ("docx links and fields", Format::Docx, 0, 1, docx_links_and_fields),
    ("docx unread links and fields", Format::Docx, 0, 1, docx_unread_links_and_fields),
    ("odt lists", Format::Odt, 4, 2, odt_lists),
    ("odt spans", Format::Odt, 4, 1, odt_spans),
    ("odt tables", Format::Odt, 4, 3, odt_tables),
    ("odt links", Format::Odt, 4, 1, odt_links),
    ("odt row groups", Format::Odt, 7, 1, odt_row_groups),
    ("odt cell links", Format::Odt, 7, 1, odt_cell_links),
    ("odp groups", Format::Odp, 7, 1, odp_groups),
    ("pptx groups", Format::Pptx, 8, 1, pptx_groups),
    ("epub lists", Format::Epub, 2, 2, epub_lists),
    ("epub divs", Format::Epub, 3, 1, epub_divs),
    ("epub quotes", Format::Epub, 3, 1, epub_quotes),
    ("epub links", Format::Epub, 3, 1, epub_links),
    ("epub item links", Format::Epub, 5, 1, epub_item_links),
    ("epub item anchor links", Format::Epub, 5, 1, epub_item_anchor_links),
    ("epub item quotes", Format::Epub, 7, 1, epub_item_quotes),
    ("epub bold links", Format::Epub, 3, 2, epub_bold_links),
    ("epub heading links", Format::Epub, 3, 1, epub_heading_links),
    ("epub math", Format::Epub, 5, 1, epub_math),
    ("rtf groups", Format::Rtf, 1, 1, rtf_groups),
    ("rtf fields", Format::Rtf, 1, 2, rtf_fields),
    ("rtf math", Format::Rtf, 3, 2, rtf_math),
    ("doc links", Format::Doc, 0, 1, doc_links),
];

fn converter(max_nesting_depth: u64) -> Converter {
    limited(|limits| limits.max_nesting_depth = max_nesting_depth)
}

#[track_caller]
fn assert_least(bytes: &[u8], format: Format, limit: Limit, least: u64) -> Conversion {
    let at =
        |max| limited(|limits| *limit_mut(limits, limit) = max).convert(Input::new(bytes, format));
    let converted = match at(least) {
        Ok(converted) => converted,
        Err(error) => panic!("{format:?} {limit:?} at {least}: {error:?}"),
    };
    let refusal = at(least - 1);
    assert!(
        trips(&refusal, limit, least - 1),
        "{format:?} {limit:?} at {}: {refusal:?}",
        least - 1
    );
    converted
}

#[test]
fn every_recursion_converts_at_the_ceiling_and_refuses_one_level_more() {
    let converter = converter(CEILING);
    for &(name, format, wrap, per, build) in CASES {
        let units = usize::try_from((CEILING - wrap) / per).unwrap();
        let deepest = build(units);
        let converted = converter.convert(Input::new(&deepest, format));
        assert!(converted.is_ok(), "{name} at {units}: {converted:?}");
        #[cfg(feature = "serde")]
        {
            let parsed = converter.parse(Input::new(&deepest, format)).unwrap();
            let json = on_a_small_stack(|| parsed.to_json());
            assert!(json.is_ok(), "{name} at {units}: {json:?}");
        }
        let deeper = build(units + 1);
        let refusal = converter.convert(Input::new(&deeper, format));
        assert!(trips(&refusal, Limit::NestingDepth, CEILING), "{name} at {}", units + 1);
    }
}

#[cfg(feature = "serde")]
fn on_a_small_stack<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        let caller = std::thread::Builder::new().stack_size(64 * 1024);
        caller.spawn_scoped(scope, work).unwrap().join().unwrap()
    })
}

#[test]
fn fields_and_groups_read_the_configured_nesting_depth() {
    let (begin, end) = (field_begin("PAGE"), field_char("end"));
    let simple = r#"<w:fldSimple w:instr="PAGE">"#;
    let simple_fields = docx(&format!(
        "<w:p>{}{}{}{DOCX_TEXT}{}{}{}</w:p>",
        begin.repeat(4),
        simple.repeat(2),
        begin.repeat(5),
        end.repeat(5),
        "</w:fldSimple>".repeat(2),
        end.repeat(4)
    ));
    let cases = [
        (word_document(&doc_fields("PAGE", 9)), Format::Doc, 9),
        (docx_fields("PAGE", 9), Format::Docx, 9),
        (simple_fields, Format::Docx, 11),
        (rtf_flat_fields("PAGE", 9), Format::Rtf, 9),
        (rtf_groups(9), Format::Rtf, 10),
    ];
    for (bytes, format, least) in cases {
        assert_least(&bytes, format, Limit::NestingDepth, least);
    }
}

#[test]
fn fields_nested_far_past_the_limit_are_refused_while_parsing() {
    let fields = 300_000;
    let refused_at_default = |bytes: &[u8], format| {
        let max = Limits::default().max_nesting_depth;
        trips(&converter(max).convert(Input::new(bytes, format)), Limit::NestingDepth, max)
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
    converter(9).unwrap();
    let refusal = converter(8);
    assert_matches!(
        refusal,
        Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 8, .. })
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
    let rels = relationships(&[("rId1", "footnotes", "footnotes.xml")]);
    let bytes = docx_with(
        body,
        &[
            ("word/_rels/document.xml.rels", rels.as_bytes()),
            ("word/footnotes.xml", footnotes.as_bytes()),
        ],
    );
    let markdown = converter(Limits::default().max_nesting_depth)
        .convert(Input::new(&bytes, Format::Docx))
        .unwrap()
        .into_markdown();
    assert!(markdown.starts_with("body[^1]\n\n[^1]: n0[^2]\n\n[^2]: n1[^3]\n"), "{markdown:.80}");
    assert!(markdown.ends_with(&format!("[^{notes}]: n{}\n", notes - 1)));
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
    ole(&[("PowerPoint Document", &stream)])
}

#[test]
fn notes_nested_in_a_stream_order_read_stop_at_max_record_depth() {
    let max = Limits::default().max_record_depth;
    let deep = ppt_nested_notes(200_000);
    assert!(trips(
        &limited(|_| {}).convert(Input::new(&deep, Format::Ppt)),
        Limit::RecordDepth,
        max
    ));
    assert_least(&ppt_nested_notes(8), Format::Ppt, Limit::RecordDepth, 9);
}

#[test]
fn a_doc_data_stream_past_max_entry_bytes_is_a_limit() {
    let mut file = cfb::CompoundFile::open(std::io::Cursor::new(word_document(b"x\r"))).unwrap();
    std::io::Write::write_all(&mut file.create_stream("Data").unwrap(), &[0u8; 4096]).unwrap();
    file.flush().unwrap();
    assert_least(&file.into_inner().into_inner(), Format::Doc, Limit::EntryBytes, 4096);
}

pub(crate) fn csv_records() -> Vec<u8> {
    (0..50).map(|i| format!("cell {i},value {i}\n")).collect::<String>().into_bytes()
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
    assert!(trips(&at(len - 1).convert(input()), Limit::OutputBytes, len - 1));
    let parsed = at(1).parse(input()).unwrap();
    assert!(trips(&parsed.render(), Limit::OutputBytes, 1));
}

#[cfg(feature = "pdf")]
#[test]
fn a_pdf_stops_at_max_output_bytes() {
    let pdf = crate::support::fixture("pdf/text.pdf");
    let len =
        limited(|_| {}).convert(Input::new(&pdf, Format::Pdf)).unwrap().markdown().len() as u64;
    assert_least(&pdf, Format::Pdf, Limit::OutputBytes, len);
}

/// The least `max_work` under which `passes` holds, which holds for every
/// larger one.
pub(crate) fn least_work(passes: impl Fn(Converter) -> bool) -> u64 {
    let (mut low, mut high) = (0, 1 << 20);
    while low < high {
        let mid = low + (high - low) / 2;
        if passes(limited(|limits| limits.max_work = mid)) { high = mid } else { low = mid + 1 }
    }
    low
}

#[test]
fn rendering_charges_each_run_thrice_and_a_unit_per_64_bytes_on_top_of_the_parse() {
    let csv = csv_records();
    let input = || Input::new(&csv, Format::Csv);
    let len = limited(|_| {}).convert(input()).unwrap().markdown().len() as u64;
    let parse = least_work(|converter| converter.parse(input()).is_ok());
    let convert = least_work(|converter| converter.convert(input()).is_ok());
    assert!(parse > 0 && len >= 64, "{parse} {len}");
    let (cells, sheet_shows) = (100, 1);
    assert_eq!(convert, parse + sheet_shows + 3 * cells + len / 64);
    let parsed = limited(|limits| limits.max_work = convert - 1).parse(input()).unwrap();
    assert!(trips(&parsed.render(), Limit::Work, convert - 1));
    let parsed = limited(|limits| limits.max_work = convert).parse(input()).unwrap();
    parsed.render().unwrap();
}

#[test]
fn rendering_a_parse_charges_on_from_its_loss_walk_as_a_conversion_does() {
    let rtf = rtf(&format!(r"a{{\footnote {}}}", r"{\b b}{\i c}".repeat(40)));
    let input = || Input::new(&rtf, Format::Rtf);
    let convert = least_work(|converter| converter.convert(input()).is_ok());
    let split = least_work(|converter| converter.parse(input()).is_ok_and(|p| p.render().is_ok()));
    assert_eq!((split, convert), (660, 660));
}

#[test]
fn a_ragged_csv_writes_the_blank_cells_of_its_short_rows_against_max_output_bytes() {
    let fields = 100_000;
    let mut csv = "a,".repeat(fields - 1);
    csv.push_str("a\n");
    csv.push_str(&"a\n".repeat(fields));
    let refusal = limited(|_| {}).convert(Input::new(csv.as_bytes(), Format::Csv));
    let max = Limits::default().max_output_bytes;
    assert!(trips(&refusal, Limit::OutputBytes, max));
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
    assert!(trips(&at(len), Limit::OutputBytes, len));
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
        r#"<a:graphic><a:graphicData><c:chart xmlns:c="{CHART}" r:id="rId1"/></a:graphicData></a:graphic>"#
    );
    let rels = |target: &str| relationships(&[("rId1", "chart", target)]);
    let body = format!("<w:p><w:r><w:drawing>{graphic}</w:drawing></w:r></w:p>");
    let docx = docx_with(
        &body,
        &[
            ("word/_rels/document.xml.rels", rels("charts/chart1.xml").as_bytes()),
            ("word/charts/chart1.xml", part.as_bytes()),
        ],
    );
    let pptx = pptx(
        &format!("<p:graphicFrame>{graphic}</p:graphicFrame>"),
        &[
            ("ppt/slides/_rels/slide1.xml.rels", rels("../charts/chart1.xml").as_bytes()),
            ("ppt/charts/chart1.xml", part.as_bytes()),
        ],
    );
    [(docx, Format::Docx), (pptx, Format::Pptx)]
}

#[test]
fn a_chart_table_is_admitted_against_max_grid_slots_before_it_is_built() {
    for (data, format) in charted(&chart(3, 3)) {
        let markdown = assert_least(&data, format, Limit::GridSlots, 16).into_markdown();
        assert!(markdown.contains("| 0 | 0 | 0 | 0 |"), "{format:?}: {markdown}");
    }
}

fn nest(open: &str, close: &str, units: usize, inner: &str) -> String {
    format!("{}{inner}{}", open.repeat(units), close.repeat(units))
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
    let links = nest(r#"<w:hyperlink w:anchor="nowhere">"#, "</w:hyperlink>", units, DOCX_TEXT);
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
    let pairs = [
        (format!("<w:hyperlink {link}>"), "</w:hyperlink>".to_owned()),
        (field_begin("HYPERLINK x"), field_char("end")),
    ];
    let opens: String = (0..levels).map(|l| pairs[l % 2].0.as_str()).collect();
    let closes: String = (0..levels).rev().map(|l| pairs[l % 2].1.as_str()).collect();
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
    pptx(&nest("<p:grpSp>", "</p:grpSp>", units, shape), &[])
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

fn word_document(text: &[u8]) -> Vec<u8> {
    let len = u32::try_from(text.len()).unwrap();
    let (stream, table) = word97(0x400, &[(0x4C, len)], text, &[(len, 0x400, 0)], Some(0x0409));
    ole(&[("0Table", &table), ("WordDocument", &stream)])
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
        format!("<a:tc {attrs}><a:txBody><a:p><a:r><a:t>x</a:t></a:r></a:p></a:txBody></a:tc>")
    };
    let frame = format!(
        "<p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tr>{}</a:tr><a:tr>{}{}</a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>",
        a_tc(r#"gridSpan="2""#),
        a_tc(""),
        a_tc("")
    );
    let pptx = pptx(&frame, &[]);
    let cell = "<table:table-cell><text:p>x</text:p></table:table-cell>";
    let odt = odf(&format!(
        r#"<office:text><table:table><table:table-row><table:table-cell table:number-columns-spanned="2"><text:p>x</text:p></table:table-cell><table:covered-table-cell/></table:table-row><table:table-row>{cell}{cell}</table:table-row></table:table></office:text>"#
    ));
    let epub =
        epub(r#"<table><tr><td colspan="2">x</td></tr><tr><td>x</td><td>x</td></tr></table>"#);
    let row = r"\trowd\cellx1000\cellx2000\intbl x\cell x\cell\row ";
    let rtf = rtf(&row.repeat(2));
    let cases = [
        (docx, Format::Docx),
        (pptx, Format::Pptx),
        (odt, Format::Odt),
        (epub, Format::Epub),
        (rtf, Format::Rtf),
    ];
    for (bytes, format) in cases {
        assert_least(&bytes, format, Limit::GridSlots, 4);
    }
}

/// A DOCX of one table of `rows`, and the bytes of its document part.
fn docx_declared_positions(rows: &str) -> (Vec<u8>, u64) {
    let part = docx_document(&format!("<w:tbl>{rows}</w:tbl>"));
    (zip_package(&[("word/document.xml", &part)]), part.len() as u64)
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
        assert!(trips(&refusal, Limit::GridSlots, max), "{name}");
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
    let continued = "<w:tc><w:tcPr><w:vMerge/></w:tcPr><w:p/></w:tc>";
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
    assert_least(&odt(1000), Format::Odt, Limit::GridSlots, 1000);
}

#[test]
fn odf_repeated_nested_tables_count_every_copy_against_max_grid_slots() {
    let cell = "<table:table-cell><text:p>x</text:p></table:table-cell>";
    let row = format!("<table:table-row>{cell}{cell}</table:table-row>");
    let odt = odf(&format!(
        r#"<office:text><table:table><table:table-row table:number-rows-repeated="2"><table:table-cell table:number-columns-repeated="3"><table:table>{row}{row}</table:table></table:table-cell></table:table-row></table:table></office:text>"#
    ));
    assert_least(&odt, Format::Odt, Limit::GridSlots, 30);
}

#[test]
fn odf_repeated_cells_copy_the_tables_their_lists_hold_and_not_their_notes() {
    let pair = "<table:table><table:table-row><table:table-cell><text:p>x</text:p></table:table-cell><table:table-cell><text:p>x</text:p></table:table-cell></table:table-row></table:table>";
    let odt = odf(&format!(
        r#"<office:text><table:table><table:table-row table:number-rows-repeated="2"><table:table-cell table:number-columns-repeated="3"><text:list><text:list-item>{pair}</text:list-item></text:list><text:p>y<text:note text:id="n1"><text:note-body>{pair}</text:note-body></text:note></text:p></table:table-cell></table:table-row></table:table></office:text>"#
    ));
    assert_least(&odt, Format::Odt, Limit::GridSlots, 20);
    assert_least(&odt, Format::Odt, Limit::ExpansionTextBytes, 55);
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
        assert!(trips(&at(10), Limit::Expansion, 10), "{name}: {:?}", at(10).err());
    }
}

#[test]
fn odf_hidden_runs_cost_the_same_at_any_length() {
    let sheet = |hidden: u64| {
        ods(&format!(
            r#"<table:table-column table:number-columns-repeated="{hidden}" table:visibility="collapse"/><table:table-column/>
            <table:table-row table:number-rows-repeated="{hidden}" table:visibility="collapse"><table:table-cell table:number-columns-repeated="{hidden}"><text:p>x</text:p></table:table-cell><table:table-cell><text:p>k</text:p></table:table-cell></table:table-row>
            <table:table-row><table:table-cell table:number-columns-repeated="{hidden}"><text:p>y</text:p></table:table-cell><table:table-cell table:number-columns-spanned="3"><text:p>m</text:p></table:table-cell><table:covered-table-cell table:number-columns-repeated="2"/></table:table-row>"#
        ))
    };
    let work = |hidden| least_work_of(&sheet(hidden), Format::Ods);
    assert_eq!(work(10), work(4_000_000_000_000_000_000));
}

#[test]
fn odf_a_span_over_hidden_lines_charges_the_expansion_budget_and_grid_slots_by_what_shows() {
    let wide = ods(
        r#"<table:table-column/><table:table-column table:number-columns-repeated="990" table:visibility="collapse"/><table:table-column table:number-columns-repeated="9"/>
        <table:table-row><table:table-cell table:number-columns-spanned="1000"><text:p>m</text:p></table:table-cell><table:covered-table-cell table:number-columns-repeated="999"/></table:table-row>"#,
    );
    let tall = ods(
        r#"<table:table-row><table:table-cell table:number-rows-spanned="1000"><text:p>m</text:p></table:table-cell></table:table-row>
        <table:table-row table:number-rows-repeated="990" table:visibility="collapse"><table:covered-table-cell/></table:table-row>
        <table:table-row table:number-rows-repeated="9"><table:covered-table-cell/></table:table-row>"#,
    );
    let moved = ods(
        r#"<table:table-column table:number-columns-repeated="991" table:visibility="collapse"/><table:table-column table:number-columns-repeated="9"/>
        <table:table-row><table:table-cell table:number-columns-spanned="1000"><text:p>m</text:p></table:table-cell><table:covered-table-cell table:number-columns-repeated="999"/></table:table-row>"#,
    );
    for (ods, least, slots_least) in [(wide, 19, 10), (tall, 18, 10), (moved, 18, 9)] {
        assert_least(&ods, Format::Ods, Limit::GridSlots, slots_least);
        assert_least(&ods, Format::Ods, Limit::Expansion, least);
    }
}

#[test]
fn odf_a_cell_repeated_across_hidden_columns_admits_the_tables_of_the_copies_that_show() {
    let sheet = ods(
        r#"<table:table-column/><table:table-column table:number-columns-repeated="2" table:visibility="collapse"/><table:table-column/>
        <table:table-row><table:table-cell table:number-columns-repeated="4"><table:table><table:table-row><table:table-cell><text:p>x</text:p></table:table-cell></table:table-row></table:table></table:table-cell></table:table-row>"#,
    );
    assert_least(&sheet, Format::Ods, Limit::GridSlots, 4);
}

#[test]
fn odf_positions_declared_past_what_a_coordinate_counts_are_refused_as_an_expansion() {
    let most = u64::MAX;
    let max = Limits::default().max_expansion;
    let cell =
        |repeat: u64| format!(r#"<table:table-cell table:number-columns-repeated="{repeat}"/>"#);
    for (name, body) in [
        (
            "rows",
            format!(r#"<table:table-row table:number-rows-repeated="{most}"/><table:table-row/>"#),
        ),
        (
            "columns",
            format!(
                r#"<table:table-column table:number-columns-repeated="{most}"/><table:table-column/>"#
            ),
        ),
        (
            "cells",
            format!(
                "<table:table-row><table:table-cell><text:p>x</text:p></table:table-cell>{}{}</table:table-row>",
                cell(most),
                cell(2)
            ),
        ),
    ] {
        let refusal = limited(|_| {}).convert(Input::new(&ods(&body), Format::Ods));
        assert!(trips(&refusal, Limit::Expansion, max), "{name}");
    }
}

#[test]
fn odf_a_hidden_sheet_holds_its_number_against_max_pages() {
    let sheets = |hidden: usize| {
        let styles = r#"<office:automatic-styles><style:style style:name="Hid" style:family="table"><style:table-properties table:display="false"/></style:style></office:automatic-styles>"#;
        let tables: String = (0..3)
            .map(|i| {
                let style = if i == hidden { r#" table:style-name="Hid""# } else { "" };
                format!(
                    r#"<table:table table:name="T{i}"{style}><table:table-row><table:table-cell><text:p>x</text:p></table:table-cell></table:table-row></table:table>"#
                )
            })
            .collect();
        odf_styled(styles, &format!("<office:spreadsheet>{tables}</office:spreadsheet>"))
    };
    for hidden in [0, 1, 2] {
        assert_least(&sheets(hidden), Format::Ods, Limit::Pages, 3);
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
        docx_with("<w:p/>", &[("word/numbering.xml", numbering.as_bytes())])
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
    let styles = format!("<office:automatic-styles>{styles}</office:automatic-styles>");
    let document = |body: String| odf_styled(&styles, &body);
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
    assert_eq!(sheet("b0999") - sheet("b0000"), 2 * (depth - 1));
}

fn docx_numbered(body: &str, styles: &str, numbering: &str) -> Vec<u8> {
    let styles = format!("<w:styles {W}>{styles}</w:styles>");
    let numbering = format!("<w:numbering {W}>{numbering}</w:numbering>");
    docx_with(
        body,
        &[("word/styles.xml", styles.as_bytes()), ("word/numbering.xml", numbering.as_bytes())],
    )
}

fn docx_style_chain(styles: usize, paragraphs: usize, id: impl Fn(usize) -> String) -> Vec<u8> {
    let chain: String = (0..styles)
        .map(|i| {
            let parent = i
                .checked_sub(1)
                .map_or(String::new(), |parent| format!(r#"<w:basedOn w:val="{}"/>"#, id(parent)));
            format!(r#"<w:style w:type="paragraph" w:styleId="{}">{parent}</w:style>"#, id(i))
        })
        .collect();
    let numbering = r#"<w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#;
    let paragraph = format!(
        r#"<w:p><w:pPr><w:pStyle w:val="{}"/><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr>{DOCX_TEXT}</w:p>"#,
        id(styles - 1)
    );
    docx_numbered(&paragraph.repeat(paragraphs), &chain, numbering)
}

#[test]
fn docx_numbering_levels_by_style_charge_each_step_by_the_length_of_its_id() {
    let at = |width: usize| {
        let docx = docx_style_chain(400, 400, |i| format!("{i:0>width$}"));
        limited(|_| {}).convert(Input::new(&docx, Format::Docx))
    };
    assert!(at(8).is_ok(), "{:?}", at(8).err());
    let refusal = at(4096);
    assert!(trips(&refusal, Limit::Work, Limits::default().max_work), "{:?}", at(4096).err());
}

#[test]
fn docx_numbering_levels_by_style_charge_a_missing_parent_its_length_per_paragraph() {
    let docx = |width: usize, paragraphs: usize| {
        let style = format!(
            r#"<w:style w:type="paragraph" w:styleId="a"><w:basedOn w:val="{}"/></w:style>"#,
            "m".repeat(width)
        );
        let numbering = r#"<w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#;
        let paragraph = format!(
            r#"<w:p><w:pPr><w:pStyle w:val="a"/><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr>{DOCX_TEXT}</w:p>"#
        );
        docx_numbered(&paragraph.repeat(paragraphs), &style, numbering)
    };
    let least = |width, paragraphs| least_work_of(&docx(width, paragraphs), Format::Docx);
    let per_paragraph = |width| least(width, 101) - least(width, 100);
    assert_eq!(per_paragraph(4096) - per_paragraph(1), 4095);
    let long = docx(4096, 100);
    let least = least_work_of(&long, Format::Docx);
    assert_least(&long, Format::Docx, Limit::Work, least);
}

#[test]
fn odf_repeated_cells_copy_the_urls_anchors_and_headings_they_hold() {
    let odt = |content: &str| {
        odf(&format!(
            r#"<office:text><table:table><table:table-row><table:table-cell table:number-columns-repeated="5">{content}</table:table-cell></table:table-row></table:table></office:text>"#
        ))
    };
    let image =
        r#"<text:p><draw:frame><draw:image xlink:href="http://e/u"/></draw:frame></text:p>"#;
    let heading = r#"<text:h text:outline-level="1">abc</text:h>"#;
    for (content, bytes) in [(image, 4 * 10), (heading, 4 * (3 + 3))] {
        assert_least(&odt(content), Format::Odt, Limit::ExpansionTextBytes, bytes);
    }
}

#[test]
fn an_odf_cell_repeated_four_million_times_over_a_long_url_is_refused() {
    let url = format!("https://example.org/{}", "a".repeat(250));
    let odt = odf(&format!(
        r#"<office:text><table:table><table:table-row><table:table-cell table:number-columns-repeated="4000000"><text:p><draw:frame><draw:image xlink:href="{url}"/></draw:frame></text:p></table:table-cell></table:table-row></table:table></office:text>"#
    ));
    let refusal = limited(|_| {}).convert(Input::new(&odt, Format::Odt)).err();
    let max = Limits::default().max_expansion_text_bytes;
    assert_matches!(
        refusal,
        Some(ConvertError::ResourceLimit { limit: Limit::ExpansionTextBytes, max: m, .. })
            if m == max
    );
}

#[test]
fn odf_tables_nested_two_deep_carry_their_copies_up_to_every_repeat() {
    let cell = "<table:table-cell><text:p>x</text:p></table:table-cell>";
    let row = format!("<table:table-row>{cell}{cell}</table:table-row>");
    let odt = odf(&format!(
        r#"<office:text><table:table><table:table-row table:number-rows-repeated="2"><table:table-cell table:number-columns-repeated="3"><table:table><table:table-row><table:table-cell><table:table>{row}{row}</table:table></table:table-cell></table:table-row></table:table></table:table-cell></table:table-row></table:table></office:text>"#
    ));
    assert_least(&odt, Format::Odt, Limit::GridSlots, 36);
    assert_least(&odt, Format::Odt, Limit::ExpansionTextBytes, 20);
}

fn odf_repeated_cell_table(text: &str, copies: u32) -> String {
    format!(
        r#"<table:table><table:table-row><table:table-cell table:number-columns-repeated="{copies}"><text:p>{text}</text:p></table:table-cell></table:table-row></table:table>"#
    )
}

#[test]
fn odf_sibling_tables_share_one_max_expansion_text_bytes() {
    let odt = odf(&format!(
        "<office:text>{}{}</office:text>",
        odf_repeated_cell_table("xx", 3),
        odf_repeated_cell_table("yyy", 2)
    ));
    assert_least(&odt, Format::Odt, Limit::ExpansionTextBytes, 7);
}

#[test]
fn odf_inner_tables_in_separate_cells_and_rows_share_one_max_expansion_text_bytes() {
    let cell = |inner: String| format!("<table:table-cell>{inner}</table:table-cell>");
    let odt = odf(&format!(
        "<office:text><table:table><table:table-row>{}{}</table:table-row><table:table-row>{}</table:table-row></table:table></office:text>",
        cell(odf_repeated_cell_table("xx", 3)),
        cell(odf_repeated_cell_table("yyy", 2)),
        cell(odf_repeated_cell_table("z", 4)),
    ));
    assert_least(&odt, Format::Odt, Limit::ExpansionTextBytes, 10);
}

#[test]
fn an_odf_row_repeated_over_an_inner_repeat_charges_both_against_one_max_expansion_text_bytes() {
    let odt = odf(&format!(
        r#"<office:text><table:table><table:table-row table:number-rows-repeated="2"><table:table-cell>{}</table:table-cell></table:table-row></table:table></office:text>"#,
        odf_repeated_cell_table("xx", 3)
    ));
    assert_least(&odt, Format::Odt, Limit::ExpansionTextBytes, 10);
}

#[test]
fn rtf_styles_under_a_basedon_cycle_resolve_in_work_linear_in_their_count() {
    let (leaves, chain, head) = (2000, 2000, 5000);
    let below: String =
        (1001..1001 + leaves).map(|id| format!(r"{{\s{id}\sbasedon{head} L{id};}}")).collect();
    let links: String =
        (head..head + chain).map(|id| format!(r"{{\s{id}\sbasedon{} C{id};}}", id + 1)).collect();
    let (a, b) = (head + chain, head + chain + 1);
    let cycle = format!(r"{{\s{a}\sbasedon{b} A;}}{{\s{b}\sbasedon{a} B;}}");
    let rtf = rtf(&format!(r"{{\stylesheet{below}{links}{cycle}}}\pard\s1001 x"));
    let converted =
        limited(|limits| limits.max_work = 100_000).convert(Input::new(&rtf, Format::Rtf));
    let converted = converted.unwrap_or_else(|error| panic!("{error:?}"));
    let cycles = count(converted.diagnostics(), |d| d.code == DiagnosticCode::StyleCycle);
    assert_eq!(cycles, 1);
}

fn docx_numbering(numbering: &str, styles: &str, paragraphs: &[usize]) -> Vec<u8> {
    let body: String = paragraphs
        .iter()
        .map(|num| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:numId w:val="{num}"/></w:numPr></w:pPr>{DOCX_TEXT}</w:p>"#
            )
        })
        .collect();
    docx_numbered(&body, styles, numbering)
}

#[test]
fn a_numbering_style_linked_from_many_abstracts_converts_in_work_linear_in_its_children() {
    let (abstracts, children) = (10_000, 150_000);
    let style = format!(
        r#"<w:style w:type="numbering" w:styleId="S">{}<w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr></w:style>"#,
        "<w:q/>".repeat(children)
    );
    let linking: String = (2..abstracts + 2)
        .map(|num| {
            format!(
                r#"<w:abstractNum w:abstractNumId="a{num}"><w:numStyleLink w:val="S"/></w:abstractNum><w:num w:numId="{num}"><w:abstractNumId w:val="a{num}"/></w:num>"#
            )
        })
        .collect();
    let numbering = format!(
        r#"<w:abstractNum w:abstractNumId="t"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="t"/></w:num>{linking}"#
    );
    let docx = docx_numbering(&numbering, &style, &[2, abstracts + 1]);
    let events = children as u64 + 6 * abstracts as u64;
    let walk_to_num_id = children as u64 + 3;
    let bound = events + walk_to_num_id + abstracts as u64 + 4096;
    let converted =
        limited(|limits| limits.max_work = bound).convert(Input::new(&docx, Format::Docx));
    let converted = converted.unwrap_or_else(|error| panic!("{error:?}"));
    assert_eq!(converted.markdown(), "1. x\n\n2) x\n");
}
