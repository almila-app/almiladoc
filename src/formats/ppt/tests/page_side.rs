//! Tests of the header, footer and comment content a page shows and no output carries.

use super::*;
use DiagnosticCode::*;
use OptionalStream::Absent;
use Region::{Body, Comment, Footer, Header};

pub(super) const TODAY: u16 = 0x1 | 0x2;
pub(super) const USER_DATE: u16 = 0x1 | 0x4;
pub(super) const SLIDE_NUMBER: u16 = 0x8;
pub(super) const HEADER: u16 = 0x10;
pub(super) const FOOTER: u16 = 0x20;

/// A `HeadersFooters` container of `instance` whose flags are `flags` and whose `strings`
/// are string records of the given instances (0 user date, 1 header, 2 footer).
pub(super) fn headers_footers(instance: u16, flags: u16, strings: &[(u16, &str)]) -> Vec<u8> {
    let atom = record(0, 0x0FDA, &[&[0, 0][..], &flags.to_le_bytes()].concat());
    let strings = strings.iter().map(|&(i, text)| record(i << 4, 0x0FBA, &utf16(text)));
    container(instance, 0x0FD9, &[&[atom][..], &strings.collect::<Vec<_>>()].concat())
}

pub(super) fn unpaired_footer() -> Vec<u8> {
    let atom = record(0, 0x0FDA, &[&[0, 0][..], &FOOTER.to_le_bytes()].concat());
    container(0, 0x0FD9, &[atom, record(2 << 4, 0x0FBA, &[0x00, 0xD8])])
}

pub(super) const FOOTER_TEXT: [(u16, &str); 1] = [(2, "Confidential")];

pub(super) fn comment() -> Vec<u8> {
    container(0, 0x2EE0, &[record(0, 0x2EE1, &[0; 36])])
}

/// A `ProgTags` container holding one binary tag named `name` whose blob holds `records`.
pub(super) fn binary_tag(name: &str, records: &[Vec<u8>]) -> Vec<u8> {
    let tag = [record(0, 0x0FBA, &utf16(name)), record(0, 0x138B, &records.concat())];
    container(0, 0x1388, &[container(0, 0x138A, &tag)])
}

/// A shape drawn as the placeholder `placement` showing `label`.
pub(super) fn drawn(placement: u32, label: &str) -> Vec<u8> {
    text_shape(0x0A00, &[], &[atom32(0x0BC3, &[0, placement])], &[typed(4, label)])
}

/// A deck of one slide holding `content`.
pub(super) fn on_slide(content: &[Vec<u8>]) -> Vec<u8> {
    listed(&[pages(0, &[(2, 256)])], &[slide(0, content)])
}

/// A deck of one slide holding a `HeadersFooters` container of its own.
pub(super) fn flagged(flags: u16, strings: &[(u16, &str)]) -> Vec<u8> {
    on_slide(&[headers_footers(0, flags, strings)])
}

/// A deck of one slide holding one binary tag.
pub(super) fn tagged(name: &str, records: &[Vec<u8>]) -> Vec<u8> {
    on_slide(&[binary_tag(name, records)])
}

/// A deck of one slide holding `content`, and `default` in its document.
pub(super) fn by_default(default: Vec<u8>, content: &[Vec<u8>]) -> Vec<u8> {
    listed(&[pages(0, &[(2, 256)]), default], &[slide(0, content)])
}

/// A deck of one slide and the notes page `content`, with `defaults` in its document.
pub(super) fn on_notes(defaults: &[Vec<u8>], content: &[Vec<u8>]) -> Vec<u8> {
    let doc = [&[pages(0, &[(2, 256)]), pages(2, &[(3, 300)])][..], defaults].concat();
    listed(&doc, &[slide(0, &[]), notes(256, content)])
}

/// A deck whose persist directory is unusable, with a footer default for slides and a header
/// default for notes, holding `objects`.
pub(super) fn recovered(objects: &[Vec<u8>]) -> Vec<u8> {
    let notes_header = headers_footers(4, HEADER, &[(1, "Draft")]);
    let doc = [pages(0, &[(2, 256)]), headers_footers(3, FOOTER, &FOOTER_TEXT), notes_header];
    deck(&doc, objects, Absent, false)
}

/// A deck linking to `https://example.org/a` whose slide draws a footer placeholder with
/// `client` beside its placeholder atom and `textbox` as its text.
fn linked_footer(client: &[Vec<u8>], textbox: &[Vec<u8>]) -> Vec<u8> {
    let links = container(0, 0x0409, &[hyperlink(1, "https://example.org/a", "")]);
    let client = [&[atom32(0x0BC3, &[0, 9])][..], client].concat();
    let shape = text_shape(0x0A00, &[], &client, textbox);
    listed(&[pages(0, &[(2, 256)]), links], &[slide(0, &[drawing(&[shape])])])
}

pub(super) fn footer_of(count: u64, unit: u32) -> Reported {
    (FooterOmitted, Footer, Some(unit), count)
}

#[test]
#[expect(clippy::too_many_lines, reason = "the table of header, footer and comment edges")]
fn each_edge_of_the_header_footer_and_comment_reading_is_reported_or_not() {
    let two_slides = pages(0, &[(2, 256), (3, 257)]);
    let cases: Vec<(&str, Vec<u8>, Vec<Reported>)> = vec![
        ("a footer flag with no string", flagged(FOOTER, &[]), vec![]),
        ("a footer flag with a blank string", flagged(FOOTER, &[(2, " \0")]), vec![]),
        ("a footer string its flag hides", flagged(0, &FOOTER_TEXT), vec![]),
        ("a header string on a slide", flagged(HEADER, &[(1, "Draft")]), vec![]),
        ("today's date with the date flag clear", flagged(0x2, &[]), vec![]),
        ("a date flag naming neither today's date nor a user date", flagged(0x1, &[]), vec![]),
        ("a user date with no text", flagged(USER_DATE, &[]), vec![]),
        ("a user date its flag hides", flagged(0x1, &[(0, "1 May")]), vec![]),
        (
            "a footer, a date and a slide number",
            flagged(FOOTER | TODAY | SLIDE_NUMBER, &FOOTER_TEXT),
            vec![footer_of(3, 1)],
        ),
        (
            "a notes page's footer, date and slide number",
            on_notes(&[], &[headers_footers(0, FOOTER | TODAY | SLIDE_NUMBER, &FOOTER_TEXT)]),
            vec![footer_of(3, 1)],
        ),
        (
            "a slide's default",
            by_default(headers_footers(3, FOOTER, &FOOTER_TEXT), &[]),
            vec![footer_of(1, 1)],
        ),
        (
            "a notes page's default does not apply to a slide",
            by_default(headers_footers(4, FOOTER, &FOOTER_TEXT), &[]),
            vec![],
        ),
        (
            "a slide's default its own container replaces with other kinds",
            by_default(
                headers_footers(3, FOOTER, &FOOTER_TEXT),
                &[headers_footers(0, SLIDE_NUMBER, &[])],
            ),
            vec![footer_of(1, 1)],
        ),
        (
            "a second slide the first slide's container does not cover",
            listed(
                &[two_slides.clone(), headers_footers(3, FOOTER, &FOOTER_TEXT)],
                &[slide(0, &[headers_footers(0, 0, &[])]), slide(0, &[])],
            ),
            vec![footer_of(1, 2)],
        ),
        (
            "a notes page's default beside a different slide default",
            on_notes(
                &[
                    headers_footers(3, FOOTER, &FOOTER_TEXT),
                    headers_footers(4, HEADER, &[(1, "Draft")]),
                ],
                &[],
            ),
            vec![footer_of(1, 1), (HeaderOmitted, Header, Some(1), 1)],
        ),
        (
            "a notes page's default its own container replaces",
            on_notes(
                &[headers_footers(4, FOOTER | HEADER, &[(1, "Draft"), (2, "Confidential")])],
                &[headers_footers(0, SLIDE_NUMBER, &[])],
            ),
            vec![footer_of(1, 1)],
        ),
        (
            "two comments under ___PPT10",
            tagged("___PPT10", &[comment(), comment()]),
            vec![(CommentOmitted, Comment, Some(1), 1)],
        ),
        (
            "a comment after the other records of its blob",
            tagged("___PPT10", &[record(0, 0x2B00, &[0; 4]), comment()]),
            vec![(CommentOmitted, Comment, Some(1), 1)],
        ),
        ("a comment under a longer tag", tagged("___PPT100", &[comment()]), vec![]),
        ("a comment under a shorter tag", tagged("___PPT1", &[comment()]), vec![]),
        (
            "a tag named by a string of another instance",
            on_slide(&[container(
                0,
                0x1388,
                &[container(
                    0,
                    0x138A,
                    &[record(1 << 4, 0x0FBA, &utf16("___PPT10")), record(0, 0x138B, &comment())],
                )],
            )]),
            vec![],
        ),
        (
            "a ___PPT10 blob without a comment",
            tagged("___PPT10", &[record(0, 0x2B00, &[0; 4])]),
            vec![],
        ),
        (
            "a comment nested in another record of its blob",
            tagged("___PPT10", &[container(0, 0x2B02, &[comment()])]),
            vec![],
        ),
        (
            "a comment of a notes page",
            on_notes(&[], &[binary_tag("___PPT10", &[comment()])]),
            vec![(CommentOmitted, Comment, Some(1), 1)],
        ),
        (
            "a comment on the second slide only",
            listed(
                std::slice::from_ref(&two_slides),
                &[slide(0, &[]), slide(0, &[binary_tag("___PPT10", &[comment()])])],
            ),
            vec![(CommentOmitted, Comment, Some(2), 1)],
        ),
        (
            "a date placeholder drawn on the slide",
            on_slide(&[drawing(&[drawn(7, "1 May")])]),
            vec![footer_of(1, 1)],
        ),
        (
            "a slide number placeholder drawn on the slide",
            on_slide(&[drawing(&[drawn(8, "Page 3")])]),
            vec![footer_of(1, 1)],
        ),
        (
            "a header placeholder drawn on the slide",
            on_slide(&[drawing(&[drawn(10, "Draft")])]),
            vec![(HeaderOmitted, Header, Some(1), 1)],
        ),
        (
            "placeholders showing only the field mark or blanks",
            on_slide(&[drawing(&[drawn(7, "*"), drawn(8, " * \r"), drawn(9, " \r")])]),
            vec![],
        ),
        (
            "placeholders showing a field mark beside text",
            on_slide(&[drawing(&[drawn(9, "* Confidential")])]),
            vec![footer_of(1, 1)],
        ),
        (
            "two footer placeholders drawn on the slide",
            on_slide(&[drawing(&[drawn(9, "One"), drawn(9, "Two")])]),
            vec![footer_of(1, 1)],
        ),
        (
            "a footer placeholder drawn beside its flag",
            on_slide(&[
                headers_footers(0, FOOTER, &FOOTER_TEXT),
                drawing(&[drawn(9, "Confidential")]),
            ]),
            vec![footer_of(1, 1)],
        ),
        (
            "a footer placeholder drawn beside another kind's flag",
            on_slide(&[
                headers_footers(0, SLIDE_NUMBER, &[]),
                drawing(&[drawn(9, "Confidential")]),
            ]),
            vec![footer_of(2, 1)],
        ),
        (
            "a footer placeholder whose text has no header atom",
            on_slide(&[drawing(&[text_shape(
                0x0A00,
                &[],
                &[atom32(0x0BC3, &[0, 9])],
                &[record(0, 0x0FA0, &utf16("Confidential"))],
            )])]),
            vec![footer_of(1, 1)],
        ),
        (
            "a footer string holding an unpaired surrogate",
            on_slide(&[unpaired_footer()]),
            vec![footer_of(1, 1)],
        ),
        (
            "a footer placeholder whose text does not decode",
            linked_footer(&[], &[atom32(0x0F9F, &[4]), record(0, 0x0FA0, &[0x41, 0, 0, 0xD8])]),
            vec![footer_of(1, 1)],
        ),
        (
            "a footer placeholder whose formatting does not decode",
            linked_footer(
                &[],
                &[
                    typed(4, "Confidential"),
                    record(0, 0x0FA1, &[&words(&[4])[..], &[0, 0], &words(&[1])].concat()),
                ],
            ),
            vec![footer_of(1, 1)],
        ),
        (
            "a footer placeholder whose text range is a hyperlink",
            linked_footer(&[], &[typed(4, "Confidential"), link(4, 1, Some((0, 12)))]),
            vec![footer_of(1, 1)],
        ),
        (
            "a footer placeholder holding a hyperlink no text range follows",
            linked_footer(&[], &[typed(4, "Confidential"), link(4, 1, None)]),
            vec![footer_of(1, 1)],
        ),
        (
            "a footer placeholder that is a click hyperlink",
            linked_footer(&[link(4, 1, None)], &[typed(4, "Confidential")]),
            vec![footer_of(1, 1)],
        ),
        (
            "a footer placeholder whose outline text hides its click link",
            listed(
                &[
                    outline(&[record(0, 0x0FA0, &utf16("see here\r"))]),
                    container(0, 0x0409, &[hyperlink(1, "https://example.org/a", "")]),
                ],
                &[slide(
                    0,
                    &[drawing(&[text_shape(
                        0x0A00,
                        &[],
                        &[atom32(0x0BC3, &[0, 9]), link(4, 1, None)],
                        &[atom32(0x0F9E, &[0])],
                    )])],
                )],
            ),
            vec![(LinkTargetUnread, Body, Some(1), 1)],
        ),
        (
            "a slide that does not resolve takes no default",
            listed(&[pages(0, &[(9, 256)]), headers_footers(3, FOOTER, &FOOTER_TEXT)], &[]),
            vec![(UnitUnreadable, Body, Some(1), 1)],
        ),
        (
            "a date, a slide number and a footer drawn on the slide",
            on_slide(&[drawing(&[drawn(7, "1 May"), drawn(8, "Page 3"), drawn(9, "Draft")])]),
            vec![footer_of(3, 1)],
        ),
        (
            "a date placeholder drawn beside the slide number flag",
            on_slide(&[headers_footers(0, SLIDE_NUMBER, &[]), drawing(&[drawn(7, "1 May")])]),
            vec![footer_of(2, 1)],
        ),
        (
            "a slide number placeholder drawn beside the footer flag",
            on_slide(&[headers_footers(0, FOOTER, &FOOTER_TEXT), drawing(&[drawn(8, "Page 3")])]),
            vec![footer_of(2, 1)],
        ),
        (
            "a header flag with no string on a notes page",
            on_notes(&[], &[headers_footers(0, HEADER, &[])]),
            vec![],
        ),
        (
            "a header string its flag hides on a notes page",
            on_notes(&[], &[headers_footers(0, 0, &[(1, "Draft")])]),
            vec![],
        ),
        (
            "a header flag beside a footer string only",
            on_notes(&[], &[headers_footers(0, HEADER, &FOOTER_TEXT)]),
            vec![],
        ),
        ("a footer flag beside a header string only", flagged(FOOTER, &[(1, "Draft")]), vec![]),
        ("a user date flag beside a footer string only", flagged(USER_DATE, &FOOTER_TEXT), vec![]),
        (
            "a container with no atom",
            on_slide(&[container(0, 0x0FD9, &[record(2 << 4, 0x0FBA, &utf16("Confidential"))])]),
            vec![],
        ),
        (
            "a second container of one page replaces its first",
            on_slide(&[headers_footers(0, FOOTER, &FOOTER_TEXT), headers_footers(0, 0, &[])]),
            vec![],
        ),
        (
            "a footer placeholder drawn on a notes page",
            on_notes(&[], &[drawing(&[drawn(9, "Confidential")])]),
            vec![footer_of(1, 1)],
        ),
        (
            "a header placeholder drawn beside a notes page's header flag",
            on_notes(
                &[],
                &[headers_footers(0, HEADER, &[(1, "Draft")]), drawing(&[drawn(10, "Draft")])],
            ),
            vec![(HeaderOmitted, Header, Some(1), 1)],
        ),
        (
            "a placeholder next to the four header and footer kinds",
            on_slide(&[drawing(&[drawn(6, "Six"), drawn(11, "Eleven")])]),
            vec![],
        ),
        (
            "footers drawn on a recovered slide and notes page",
            recovered(&[
                slide(0, &[headers_footers(0, 0, &[]), drawing(&[drawn(9, "Confidential")])]),
                notes(256, &[headers_footers(0, 0, &[]), drawing(&[drawn(9, "Draft")])]),
            ]),
            vec![
                (RecoveredStreamOrder, Body, None, 1),
                footer_of(1, 1),
                (FooterOmitted, Footer, None, 1),
            ],
        ),
    ];
    for (case, bytes, mut expected) in cases {
        expected.sort();
        assert_eq!(reported(&bytes), expected, "{case}");
    }
}
