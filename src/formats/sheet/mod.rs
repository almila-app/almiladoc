//! Excel spreadsheets (xlsx, xlsm, xlsb, xls). Every container is read
//! in-house: SpreadsheetML as XML (xlsx, xlsm) or binary (xlsb), and
//! OLE-based BIFF (xls). All three share the number format engine and grid
//! assembly, so one workbook saved in any of them converts identically.

mod controls;
mod grid;
mod headers;
mod numfmt;
#[cfg(test)]
mod test_support;
#[deny(
    clippy::indexing_slicing,
    reason = "a binary record reader reads through checked views (ADR 0001 decision 16)"
)]
mod xls;
#[deny(
    clippy::indexing_slicing,
    reason = "a binary record reader reads through checked views (ADR 0001 decision 16)"
)]
mod xlsb;
mod xlsx;

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::model::Document;
use crate::package::archive::probe_ole;
use crate::package::relationships::read_main_part;
use crate::package::{Package, PackageKind};
use crate::shared::lossy;

const MAIN_PARTS: [&str; 2] = ["xl/workbook.xml", "xl/workbook.bin"];

pub(super) fn parse(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Document, ConvertError> {
    // An encrypted OOXML package is an OLE file without a BIFF stream.
    if let Some(probe) = probe_ole(bytes) {
        return match probe {
            ConvertError::Encrypted => Err(probe),
            _ => xls::parse(parse_ctx, bytes),
        };
    }
    let mut pkg = Package::open(parse_ctx, bytes, PackageKind::Opc).map_err(|e| match e {
        ConvertError::Malformed { .. } => not_a_workbook(),
        e => e,
    })?;
    let Some(wb_part) = read_main_part(parse_ctx, &pkg, &MAIN_PARTS)? else {
        return Err(not_a_workbook());
    };
    match classify(&mut pkg, &wb_part)? {
        Some(Container::Xml) => xlsx::parse(parse_ctx, &mut pkg, &wb_part),
        Some(Container::Bin) => xlsb::parse(parse_ctx, &mut pkg, &wb_part),
        None => Err(not_a_workbook()),
    }
}

fn not_a_workbook() -> ConvertError {
    ConvertError::malformed("not a readable workbook container")
}

enum Container {
    Xml,
    Bin,
}

/// The container a main part belongs to, from its own bytes rather than its
/// name: a SpreadsheetML workbook is XML, an xlsb one is a record stream.
fn classify(pkg: &mut Package<'_>, part: &str) -> Result<Option<Container>, ConvertError> {
    let Some(bytes) = pkg.part(part)? else {
        return Ok(None);
    };
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    // A UTF-16 part opens on its byte order mark rather than the tag.
    Ok(if matches!(body.iter().find(|b| !b.is_ascii_whitespace()), Some(b'<' | 0xFF | 0xFE)) {
        Some(Container::Xml)
    } else {
        (!body.is_empty()).then_some(Container::Bin)
    })
}

/// Float formatting at the 15 significant decimal digits a spreadsheet
/// stores and displays. Shortest round-trip formatting past that surfaces the
/// binary representation (`3554.7000000000003`); 15 digits still keeps small
/// values like 0.0000004 exact.
pub(super) fn format_float(f: f64) -> String {
    match format!("{f:.14e}").parse::<f64>() {
        Ok(rounded) => format!("{rounded}"),
        Err(_) => format!("{f}"),
    }
}

/// Whole seconds in a span of days, saturating.
fn whole_seconds(days: f64) -> u64 {
    lossy::round_u64(days.abs() * 86_400.0)
}

/// Render a time-of-day serial (a fraction of a day) as `hh:mm:ss`.
fn format_time_of_day(days: f64) -> String {
    let total_secs = whole_seconds(days) % 86_400;
    let (h, m, s) = (total_secs / 3600, (total_secs % 3600) / 60, total_secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

/// Render an Excel duration (stored in days) as `[h]:mm:ss`.
fn format_duration_days(days: f64) -> String {
    let sign = if days < 0.0 { "-" } else { "" };
    let total_secs = whole_seconds(days);
    let (h, m, s) = (total_secs / 3600, (total_secs % 3600) / 60, total_secs % 60);
    format!("{sign}{h}:{m:02}:{s:02}")
}

/// Decode an RK value, the packed number both binary containers use. Bit 0
/// asks for a hundredth of the result; bit 1 selects an integer over the
/// high 30 bits of a double.
pub(super) fn rk_number(rk: u32) -> f64 {
    let value = if rk & 0x02 != 0 {
        f64::from(rk.cast_signed() >> 2)
    } else {
        f64::from_bits(u64::from(rk & 0xFFFF_FFFC) << 32)
    };
    if rk & 0x01 != 0 { value / 100.0 } else { value }
}

/// The literal an error code displays as, shared by both binary containers.
pub(super) fn error_literal(code: u8) -> Option<&'static str> {
    Some(match code {
        0x00 => "#NULL!",
        0x07 => "#DIV/0!",
        0x0F => "#VALUE!",
        0x17 => "#REF!",
        0x1D => "#NAME?",
        0x24 => "#NUM!",
        0x2A => "#N/A",
        0x2B => "#GETTING_DATA",
        _ => return None,
    })
}

#[cfg(test)]
#[expect(
    clippy::cast_sign_loss,
    clippy::unreadable_literal,
    reason = "date serials are written as spreadsheets store them, RK words as bit patterns"
)]
mod tests {
    use super::*;
    use crate::diagnostics::{DiagnosticCode, Region};
    use crate::shared::test_support::{diagnostic_rows, zip_package};
    use std::assert_matches;
    use std::io::Cursor;

    #[test]
    fn time_of_day_serials_carry_no_date() {
        assert_eq!(format_time_of_day(32_694.184 / 86_400.0), "09:04:54");
        assert_eq!(format_time_of_day(0.0), "00:00:00");
    }

    #[test]
    fn floats_render_at_spreadsheet_precision() {
        assert_eq!(format_float(3554.7000000000003), "3554.7");
        assert_eq!(format_float(5649.5599999999995), "5649.56");
        assert_eq!(format_float(346_289_529.491_800_1), "346289529.4918");
        assert_eq!(format_float(0.0000004), "0.0000004");
        assert_eq!(format_float(12.0), "12");
        assert_eq!(format_float(1.0), "1");
        assert_eq!(format_float(1.5), "1.5");
    }

    #[test]
    fn durations_render_as_clock_time() {
        let days = (26.0 * 3600.0 + 30.0 * 60.0 + 15.0) / 86_400.0;
        assert_eq!(format_duration_days(days), "26:30:15");
        assert_eq!(format_duration_days(-0.5), "-12:00:00");
    }

    #[test]
    fn rk_values_decode_both_kinds_with_and_without_x100() {
        assert_eq!(rk_number((1 << 2) | 2), 1.0);
        assert_eq!(rk_number(((-1i32 as u32) << 2) | 2), -1.0);
        assert_eq!(rk_number((12345 << 2) | 2 | 1), 123.45);
        assert_eq!(rk_number((1.5f64.to_bits() >> 32) as u32), 1.5);
        assert_eq!(rk_number((1.5f64.to_bits() >> 32) as u32 | 1), 0.015);
    }

    #[test]
    fn a_main_part_is_xml_on_a_tag_or_a_utf16_mark_and_records_otherwise() {
        let cases: [(&str, &[u8], Option<&str>); 8] = [
            ("a tag", b"<workbook/>", Some("xml")),
            ("a tag after whitespace", b" \r\n\t<workbook/>", Some("xml")),
            ("a tag after a UTF-8 mark", b"\xEF\xBB\xBF<workbook/>", Some("xml")),
            ("a UTF-16LE mark", b"\xFF\xFE<\0", Some("xml")),
            ("a UTF-16BE mark", b"\xFE\xFF\0<", Some("xml")),
            ("a record stream", b"\x83\x00\x04", Some("bin")),
            ("an empty part", b"", None),
            ("only a UTF-8 mark", b"\xEF\xBB\xBF", None),
        ];
        let parse_ctx = ParseCtx::default();
        for (label, body, expected) in cases {
            let bytes = zip_package(&[("xl/workbook.xml", body)]);
            let mut pkg = Package::open(&parse_ctx, &bytes, PackageKind::Opc).unwrap();
            let container = classify(&mut pkg, "xl/workbook.xml").unwrap().map(|c| match c {
                Container::Xml => "xml",
                Container::Bin => "bin",
            });
            assert_eq!(container, expected, "{label}");
        }
    }

    #[test]
    fn an_encrypted_ooxml_package_is_not_read_as_biff() {
        let mut ole = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        ole.create_stream("EncryptionInfo").unwrap();
        ole.create_stream("EncryptedPackage").unwrap();
        let bytes = ole.into_inner().into_inner();

        assert_matches!(parse(&ParseCtx::default(), &bytes), Err(ConvertError::Encrypted));
    }

    #[test]
    fn a_package_that_is_not_a_workbook_does_not_convert_as_one() {
        // Resolving the root relationship reaches any OOXML main part, so
        // without a workbook check a document would convert to nothing.
        const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
        let rels = format!(
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="word/document.xml"/></Relationships>"#
        );
        let doc = r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body/></w:document>"#;

        let bytes = zip_package(&[("_rels/.rels", rels.as_str()), ("word/document.xml", doc)]);

        assert_matches!(parse(&ParseCtx::default(), &bytes), Err(ConvertError::Malformed { .. }));
    }

    const WORKBOOK: &str = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheets/></workbook>"#;

    fn root_rels(kind: &str, target: &str) -> String {
        format!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}" Target="{target}"/></Relationships>"#
        )
    }

    fn reported(bytes: &[u8]) -> Vec<(DiagnosticCode, Region, u64)> {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, bytes).unwrap();
        diagnostic_rows(&parse_ctx.diagnostics())
    }

    #[test]
    fn a_workbook_found_by_convention_is_reported_beside_the_root_relationships() {
        let used = [(DiagnosticCode::ConventionalPartUsed, Region::Container, 1)];
        for rels in [
            root_rels("officeDocument", ""),
            root_rels("officeDocument", "/"),
            root_rels("officeDocument", "a%2Fb.xml"),
            root_rels("styles", "styles.xml"),
        ] {
            let bytes =
                zip_package(&[("_rels/.rels", rels.as_str()), ("xl/workbook.xml", WORKBOOK)]);
            assert_eq!(reported(&bytes), used);
        }
        let named = root_rels("officeDocument", "xl/workbook.xml");
        let bytes = zip_package(&[("_rels/.rels", named.as_str()), ("xl/workbook.xml", WORKBOOK)]);
        assert_eq!(reported(&bytes), []);
        assert_eq!(reported(&zip_package(&[("xl/workbook.xml", WORKBOOK)])), []);
    }

    #[test]
    fn the_xml_workbook_is_the_conventional_part_ahead_of_the_binary_one() {
        let bytes = zip_package(&[("xl/workbook.bin", "\0"), ("xl/workbook.xml", WORKBOOK)]);
        assert_eq!(reported(&bytes), []);
    }

    #[test]
    fn a_workbook_the_relationship_names_is_never_replaced_by_a_leftover() {
        let missing = root_rels("officeDocument", "book/workbook.xml");
        let bytes =
            zip_package(&[("_rels/.rels", missing.as_str()), ("xl/workbook.xml", WORKBOOK)]);
        assert_matches!(parse(&ParseCtx::default(), &bytes), Err(ConvertError::Malformed { .. }));
    }

    #[test]
    fn a_package_with_no_workbook_part_is_not_a_workbook() {
        let styles = root_rels("styles", "styles.xml");
        for parts in [
            vec![("docProps/core.xml", "<x/>")],
            vec![("_rels/.rels", styles.as_str()), ("word/document.xml", "<x/>")],
        ] {
            let error = parse(&ParseCtx::default(), &zip_package(&parts)).unwrap_err();
            assert_eq!(error.to_string(), not_a_workbook().to_string());
        }
    }
}
