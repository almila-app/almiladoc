//! Tests of what the reader of a page's header, footer and comment content charges and writes.

use super::*;

#[test]
fn a_pages_headers_footers_and_comment_tag_charge_each_record_they_read() {
    let blob = [record(0, 0x2B00, &[0; 4]), record(0, 0x2B01, &[0; 4]), comment()];
    let walk = |content: &[u8], parse_ctx: &ParseCtx<'_>| Extractor::new(parse_ctx).walk(content);
    let page = [headers_footers(0, FOOTER, &FOOTER_TEXT), binary_tag("___PPT10", &blob)];
    let other = binary_tag("___PPT9", &blob);
    let cases = [
        ("a page's containers", page.concat(), (1 + 2) + 1 + (1 + 2 + 3)),
        ("a tag", other, 1 + 1 + 2),
    ];
    for (case, content, fuel) in cases {
        let parse_ctx = ParseCtx::default();
        walk(&content, &parse_ctx).unwrap();
        assert_eq!(parse_ctx.fuel(), fuel, "{case}");
        let limits = Limits { max_work: fuel - 1, ..Limits::default() };
        assert_matches!(
            walk(&content, &ParseCtx::new(&limits)),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max }) if max == fuel - 1,
            "{case}"
        );
    }
}

#[test]
fn a_recovered_decks_default_containers_are_read_once_and_not_descended() {
    let fuel = |doc: &[Vec<u8>]| {
        let bytes = deck(doc, &[slide(0, &[])], OptionalStream::Absent, false);
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, &bytes).unwrap();
        parse_ctx.fuel()
    };
    let without = fuel(&[pages(0, &[(2, 256)])]);
    let defaults = headers_footers(3, FOOTER, &FOOTER_TEXT);
    let with = fuel(&[pages(0, &[(2, 256)]), defaults]);
    assert_eq!(with - without, (1 + 2) + 1);
}

#[test]
fn a_notes_page_opens_no_unit_of_its_own() {
    let doc = [pages(0, &[(2, 256)]), pages(2, &[(3, 300)])];
    let objects = [slide(0, &[label("Slide\r")]), notes(256, &[label("Note\r")])];
    let bytes = listed(&doc, &objects);
    let blocks = parse(&ParseCtx::default(), &bytes).unwrap().blocks;
    let units =
        |blocks: &[Block]| blocks.iter().filter(|b| matches!(b, Block::UnitStart(_))).count();
    assert_eq!(units(&blocks), 1);
    let quote = blocks.iter().find_map(|block| match block {
        Block::BlockQuote(inner) => Some(inner),
        _ => None,
    });
    assert_eq!(units(quote.unwrap()), 0);
}

#[test]
fn a_drawn_header_or_footer_placeholder_renders_no_text() {
    let shapes = [
        text_shape(0x0A00, &[], &[], &[label("Kept\r")]),
        drawn(7, "1 May"),
        drawn(8, "Page 3"),
        drawn(9, "Confidential"),
        drawn(10, "Draft"),
        drawn(6, "Six"),
        drawn(11, "Eleven"),
    ];
    let content = [drawing(&shapes)];
    assert_eq!(markdown_of(&on_slide(&content)), "Kept\n\nSix\n\nEleven\n");
    let doc = [pages(0, &[(2, 256)]), pages(2, &[(3, 300)])];
    let objects = [slide(0, &[]), notes(256, &content)];
    assert_eq!(markdown_of(&listed(&doc, &objects)), "> Kept\n>\n> Six\n>\n> Eleven\n");
}
