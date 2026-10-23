//! Peak live heap of whole conversions, measured by a counting allocator: the
//! input-controlled work a reader admits does not grow memory it never needs.
#![expect(clippy::unwrap_used, reason = "test helpers fail by panicking")]

use almiladoc::{Config, Converter, Format, Input, OnLoss};
use std::sync::{Mutex, MutexGuard, PoisonError};

#[path = "common/word97.rs"]
mod word97;
use word97::word97;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Held by each test for its whole body: a running profiler counts every thread's
/// allocations, so no test allocates while another test's profiler runs.
static PROFILER: Mutex<()> = Mutex::new(());

fn exclusive() -> MutexGuard<'static, ()> {
    PROFILER.lock().unwrap_or_else(PoisonError::into_inner)
}

fn converter() -> Converter {
    let mut config = Config::default();
    config.on_loss = OnLoss::Report;
    Converter::new(config).unwrap()
}

/// The most heap live at once while `converter` converts `bytes`, and the Markdown it returns.
fn converted(converter: &Converter, bytes: &[u8], format: Format) -> (usize, String) {
    let _profiler = dhat::Profiler::builder().testing().build();
    let conversion = converter.convert(Input::new(bytes, format)).unwrap();
    (dhat::HeapStats::get().max_bytes, conversion.into_markdown())
}

fn peak(converter: &Converter, bytes: &[u8]) -> usize {
    converted(converter, bytes, Format::Doc).0
}

/// The heap `converter` leaves live while it holds the parsed `bytes`.
fn parsed_heap(converter: &Converter, bytes: &[u8], format: Format) -> usize {
    let _profiler = dhat::Profiler::builder().testing().build();
    let parsed = converter.parse(Input::new(bytes, format)).unwrap();
    let live = dhat::HeapStats::get().curr_bytes;
    drop(parsed);
    live
}

/// Peak heap at `aliases` and four times as many, after a warm-up outside any profiler.
fn peaks(doc: impl Fn(u32) -> Vec<u8>, aliases: u32) -> (usize, usize) {
    let (few, many) = (doc(aliases), doc(4 * aliases));
    let converter = converter();
    converter.convert(Input::new(&few, Format::Doc)).unwrap();
    (peak(&converter, &few), peak(&converter, &many))
}

/// Compressed bytes every aliasing piece reads.
const STRETCH: u32 = 1 << 14;
const TEXT_FC: u32 = 0x800;

fn stretch_text() -> Vec<u8> {
    [&b"a\r"[..], &vec![b'x'; STRETCH as usize]].concat()
}

fn doc(ccps: &[(usize, u32)], text: &[u8], pieces: &[(u32, u32, u16)]) -> Vec<u8> {
    use std::io::Write;
    let (word_doc, table) = word97(TEXT_FC, ccps, text, pieces, None);
    let mut ole = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
    ole.create_stream("WordDocument").unwrap().write_all(&word_doc).unwrap();
    ole.create_stream("0Table").unwrap().write_all(&table).unwrap();
    ole.into_inner().into_inner()
}

/// `a` as the main document, then a header story of `aliases` pieces that
/// all read one stretch of the stream.
fn aliased_headers(aliases: u32) -> Vec<u8> {
    let mut pieces = vec![(2, TEXT_FC, 0)];
    pieces.extend((1..=aliases).map(|i| (2 + i * STRETCH, TEXT_FC + 2, 0)));
    pieces.push((3 + aliases * STRETCH, TEXT_FC, 0));
    doc(&[(0x4C, 2), (0x54, aliases * STRETCH)], &stretch_text(), &pieces)
}

/// A main document of `aliases` deleted pieces that all read one stretch of
/// the stream, then `a`.
fn aliased_deletions(aliases: u32) -> Vec<u8> {
    let deleted = (0x41 << 1) | (1 << 8);
    let mut pieces: Vec<_> = (1..=aliases).map(|i| (i * STRETCH, TEXT_FC + 2, deleted)).collect();
    pieces.push((aliases * STRETCH + 2, TEXT_FC, 0));
    doc(&[(0x4C, aliases * STRETCH + 2)], &stretch_text(), &pieces)
}

/// Heap the four-fold document may hold beyond the first: its longer piece table and
/// what reading it costs, not its expanded text.
const SLACK: usize = 64 << 10;

fn assert_flat_heap(aliased: fn(u32) -> Vec<u8>) {
    let _held = exclusive();
    let converted = converter().convert(Input::new(&aliased(4), Format::Doc)).unwrap();
    assert_eq!(converted.markdown(), "a\n");
    let (few, many) = peaks(aliased, 4);
    assert!(many < few + SLACK, "{few} bytes for 64 Ki CPs, {many} for 256 Ki");
}

#[test]
fn header_stories_never_rendered_take_no_heap_by_the_text_their_aliases_expand_to() {
    assert_flat_heap(aliased_headers);
}

#[test]
fn deleted_text_read_through_aliases_takes_no_heap_by_its_length() {
    assert_flat_heap(aliased_deletions);
}

const SML: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

/// Columns and rows between a sheet's two cells, so its table holds `SIDE * SIDE` positions.
const SIDE: usize = 1 << 10;
/// The letters of column `SIDE`, counted from one.
const LAST_COLUMN: &str = "AMJ";

fn relationship(kind: &str, target: &str) -> String {
    format!(
        r#"<Relationships xmlns="{PKG_RELS}"><Relationship Id="{kind}" Type="{REL}/{kind}" Target="{target}"/></Relationships>"#
    )
}

/// A one-sheet xlsx holding `x` in `A1` and in the far corner of a table of `SIDE * SIDE`
/// positions, with `merges` after its cells.
fn corner_cells_xlsx(merges: &str) -> Vec<u8> {
    use std::io::Write;
    let workbook = format!(
        r#"<workbook xmlns="{SML}" xmlns:r="{REL}"><sheets><sheet name="S" sheetId="1" r:id="worksheet"/></sheets></workbook>"#
    );
    let cell = |at: String| format!(r#"<c r="{at}" t="inlineStr"><is><t>x</t></is></c>"#);
    let rows = format!(
        r#"<sheetData><row r="1">{}</row><row r="{SIDE}">{}</row></sheetData>"#,
        cell("A1".to_owned()),
        cell(format!("{LAST_COLUMN}{SIDE}")),
    );
    let sheet = format!(r#"<worksheet xmlns="{SML}">{rows}{merges}</worksheet>"#);
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, body) in [
        ("_rels/.rels", relationship("officeDocument", "xl/workbook.xml")),
        ("xl/workbook.xml", workbook),
        ("xl/_rels/workbook.xml.rels", relationship("worksheet", "worksheets/sheet1.xml")),
        ("xl/worksheets/sheet1.xml", sheet),
    ] {
        writer.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// Two-row merges across the full width, stacked over every row between the two cells.
fn stacked_merges() -> String {
    use std::fmt::Write;
    let mut merges = String::from("<mergeCells>");
    for top in (2..SIDE).step_by(2) {
        write!(merges, r#"<mergeCell ref="A{top}:{LAST_COLUMN}{}"/>"#, top + 1).unwrap();
    }
    merges + "</mergeCells>"
}

/// One merge over every position between the two cells.
fn inner_merge() -> String {
    format!(r#"<mergeCells><mergeCell ref="B2:AMI{}"/></mergeCells>"#, SIDE - 1)
}

/// Heap the conversion may hold beyond the document it parses and the Markdown buffer it
/// grows: the bookkeeping of each of the table's rows.
const TABLE_SLACK: usize = 64 << 10;

#[test]
fn a_table_of_empty_positions_takes_no_heap_beyond_its_document_and_its_markdown() {
    assert_table_heap(&corner_cells_xlsx(""));
}

#[test]
fn a_table_of_merged_positions_takes_no_heap_beyond_its_document_and_its_markdown() {
    assert_table_heap(&corner_cells_xlsx(&stacked_merges()));
}

#[test]
fn a_table_of_one_merge_takes_no_heap_beyond_its_document_and_its_markdown() {
    assert_table_heap(&corner_cells_xlsx(&inner_merge()));
}

fn assert_table_heap(xlsx: &[u8]) {
    let _held = exclusive();
    let converter = converter();
    converter.convert(Input::new(xlsx, Format::Excel)).unwrap();
    let document = parsed_heap(&converter, xlsx, Format::Excel);
    let (peak, markdown) = converted(&converter, xlsx, Format::Excel);
    assert!(markdown.len() >= SIDE * SIDE, "{} bytes of Markdown", markdown.len());
    let buffer = markdown.capacity();
    assert!(
        peak <= document + buffer + TABLE_SLACK,
        "{peak} bytes for a document of {document} and a Markdown buffer of {buffer}"
    );
}
