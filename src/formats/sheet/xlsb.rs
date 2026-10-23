//! In-house binary SpreadsheetML reader (.xlsb): MS-XLSB record streams in the xlsx package,
//! the grid shared with `grid.rs`; the sheet driver and the package parts beside the cells
//! shared with `xlsx.rs`.

use super::grid::{
    CellFormat, CellRange, CellRef, CellXfs, DateSystem, LinkTarget, OOXML_GRID, SheetContent,
    SheetLink, UnreadFormats, format_as_text, render_number, resolve_format,
};
use super::xlsx::{SHARED_STRINGS_REL, Sibling, convert_sheets, list_sheets};
use super::{error_literal, rk_number};
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::Document;
use crate::package::Package;
use crate::package::archive::{Role, skip_unreadable};
use crate::package::relationships::{read_rels, rel_type, rels_part_for};
use crate::shared::binary::{ByteReader, decode_utf16, utf16le_units};
use crate::shared::text::clean_text;
use std::collections::HashMap;

// MS-XLSB record type values (section 2.3, "By Number").
const BRT_ROW_HDR: u16 = 0;
const BRT_CELL_RK: u16 = 2;
const BRT_CELL_ERROR: u16 = 3;
const BRT_CELL_BOOL: u16 = 4;
const BRT_CELL_REAL: u16 = 5;
const BRT_CELL_ST: u16 = 6;
const BRT_CELL_ISST: u16 = 7;
const BRT_FMLA_STRING: u16 = 8;
const BRT_FMLA_NUM: u16 = 9;
const BRT_FMLA_BOOL: u16 = 10;
const BRT_FMLA_ERROR: u16 = 11;
const BRT_SST_ITEM: u16 = 19;
const BRT_FMT: u16 = 44;
const BRT_XF: u16 = 47;
const BRT_COL_INFO: u16 = 60;
const BRT_CELL_RSTRING: u16 = 62;
const BRT_WB_PROP: u16 = 153;
const BRT_BUNDLE_SH: u16 = 156;
const BRT_MERGE_CELL: u16 = 176;
const BRT_BEGIN_HEADER_FOOTER: u16 = 479;
const BRT_HLINK: u16 = 494;
const BRT_DRAWING: u16 = 550;
const BRT_BEGIN_CELL_XFS: u16 = 617;
const BRT_END_CELL_XFS: u16 = 618;

pub(super) fn parse(
    parse_ctx: &ParseCtx<'_>,
    pkg: &mut Package<'_>,
    wb_part: &str,
) -> Result<Document, ConvertError> {
    let workbook = pkg.required_part(wb_part)?;
    let (dates, bundles) = read_workbook(parse_ctx, &workbook)?;
    let wb_rels = read_rels(pkg, &rels_part_for(wb_part), Role::Content(Region::Body))?;

    let shared = read_optional(
        parse_ctx,
        pkg,
        &Sibling::of(&wb_rels, wb_part, SHARED_STRINGS_REL, "sharedStrings.bin"),
        Role::Reported(Region::Body),
        read_shared_strings,
    )?
    .unwrap_or_default();
    let styles_part = Sibling::of(&wb_rels, wb_part, rel_type::STYLES, "styles.bin");
    let xfs =
        read_optional(parse_ctx, pkg, &styles_part, Role::Content(Region::Body), read_styles)?
            .map_or_else(|| CellXfs::unread(styles_part.reported_when_unread(pkg)), CellXfs::new);

    let listed = bundles.iter().map(|bundle| (bundle.hidden, bundle.rid.as_deref(), bundle));
    let sheets = list_sheets(parse_ctx, &wb_rels, wb_part, listed)?;
    convert_sheets(
        parse_ctx,
        pkg,
        &sheets,
        |pkg, part| match pkg.related_part(part, Role::Reported(Region::Body))? {
            Some(bytes) => unless_fatal(read_sheet(parse_ctx, &bytes, &shared, &xfs, dates)),
            None => Ok(None),
        },
        |bundle| {
            if bundle.name_replaced {
                parse_ctx.emit(DiagnosticCode::InvalidEncoding, Region::Body);
            }
            bundle.name.clone()
        },
    )
}

fn unless_fatal<T>(read: Result<T, ConvertError>) -> Result<Option<T>, ConvertError> {
    match read {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.is_fatal() => Err(e),
        Err(_) => Ok(None),
    }
}

/// Read an optional binary part under the unified recovery policy: absent
/// or corrupt is `None`, a corrupt part reported by `role`; fatal
/// resource-limit errors always propagate.
fn read_optional<T>(
    parse_ctx: &ParseCtx<'_>,
    pkg: &mut Package<'_>,
    part: &Sibling,
    role: Role,
    read: fn(&ParseCtx<'_>, &[u8]) -> Result<T, ConvertError>,
) -> Result<Option<T>, ConvertError> {
    let Some(bytes) = part.read(pkg, role)? else {
        return Ok(None);
    };
    skip_unreadable(parse_ctx, role, read(parse_ctx, &bytes).map(Some))
}

/// A sheet the workbook lists, in source order.
struct Bundle {
    name: String,
    /// The name needed a replacement character.
    name_replaced: bool,
    rid: Option<String>,
    hidden: bool,
}

/// `xl/workbook.bin`: the 1904 date flag from BrtWbProp, and each sheet's
/// name, relationship id and visibility from its BrtBundleSh.
fn read_workbook(
    parse_ctx: &ParseCtx<'_>,
    data: &[u8],
) -> Result<(DateSystem, Vec<Bundle>), ConvertError> {
    let mut dates = DateSystem::Since1900;
    let mut sheets = Vec::new();
    let mut records = Records::new(parse_ctx, data);
    while let Some((id, payload)) = records.next()? {
        match id {
            BRT_WB_PROP => {
                dates = DateSystem::from_date1904(Fields::new(payload).u32()? & 1 != 0);
            }
            BRT_BUNDLE_SH => {
                let mut f = Fields::new(payload);
                let state = f.u32()?;
                f.u32()?; // iTabID
                let rid = f.nullable_wide_string()?.map(|(rid, _)| rid);
                let (name, name_replaced) = f.wide_string()?;
                // hsState 1 is hidden, 2 is veryHidden: omitted entirely,
                // heading included, exactly as in xlsx.
                let hidden = matches!(state, 1 | 2);
                sheets.push(Bundle { name: clean_text(&name), name_replaced, rid, hidden });
            }
            _ => {}
        }
    }
    Ok((dates, sheets))
}

/// `styles.bin` reduced to the ordered cellXfs list, each entry's format id
/// resolved to a parsed format and whether its code needed a replacement
/// character. Only BrtXF records between BrtBeginCellXFs
/// and BrtEndCellXFs are cell XFs; the cell-style XF collection also holds
/// BrtXF records and must not shift the indices.
fn read_styles(
    parse_ctx: &ParseCtx<'_>,
    data: &[u8],
) -> Result<Vec<(CellFormat, bool)>, ConvertError> {
    let mut codes: HashMap<u32, (String, bool)> = HashMap::new();
    let mut fmt_ids: Vec<u32> = Vec::new();
    let mut in_cell_xfs = false;
    let mut records = Records::new(parse_ctx, data);
    while let Some((id, payload)) = records.next()? {
        match id {
            BRT_BEGIN_CELL_XFS => in_cell_xfs = true,
            BRT_END_CELL_XFS => in_cell_xfs = false,
            BRT_FMT => {
                let mut f = Fields::new(payload);
                let ifmt = f.u16()?;
                codes.insert(u32::from(ifmt), f.wide_string()?);
            }
            BRT_XF if in_cell_xfs => {
                let mut f = Fields::new(payload);
                f.u16()?; // ixfeParent
                fmt_ids.push(u32::from(f.u16()?));
            }
            _ => {}
        }
    }
    let unread = UnreadFormats::default();
    let mut cache: HashMap<u32, CellFormat> = HashMap::new();
    Ok(fmt_ids
        .iter()
        .map(|id| {
            let entry = codes.get(id);
            let format = cache.entry(*id).or_insert_with(|| {
                resolve_format(*id, entry.map(|(code, _)| code.as_str()), &unread)
            });
            (format.clone(), entry.is_some_and(|&(_, replaced)| replaced))
        })
        .collect())
}

/// `sharedStrings.bin`: one cleaned entry per BrtSSTItem in order, with
/// whether it needed a replacement character. The item is a RichStr;
/// formatting runs and phonetic data trail the string and are not content.
fn read_shared_strings(
    parse_ctx: &ParseCtx<'_>,
    data: &[u8],
) -> Result<Vec<(String, bool)>, ConvertError> {
    let mut out = Vec::new();
    let mut records = Records::new(parse_ctx, data);
    while let Some((id, payload)) = records.next()? {
        if id == BRT_SST_ITEM {
            let mut f = Fields::new(payload);
            f.u8()?; // fRichStr / fExtStr flags
            let (text, replaced) = f.wide_string()?;
            out.push((clean_text(&text), replaced));
        }
    }
    Ok(out)
}

/// One cell record, placed at the row of the preceding BrtRowHdr.
fn read_cell(
    id: u16,
    f: &mut Fields<'_>,
    row: Option<u32>,
    shared: &[(String, bool)],
    xfs: &CellXfs,
    dates: DateSystem,
    out: &mut SheetContent,
) -> Result<(), ConvertError> {
    let col = f.u32()?;
    let style = (f.u32()? & 0x00FF_FFFF) as usize;
    let (text, loss, replaced) = cell_text(id, f, &xfs.get(style), shared, dates)?;
    let Some(r) = row else {
        if loss.is_some() || !text.trim().is_empty() {
            out.report(DiagnosticCode::CellValueUnreadable, Region::Body);
        }
        return Ok(());
    };
    let at = CellRef::new(r, col);
    if let Some(code) = loss {
        out.lose(at, code)?;
    }
    if replaced || xfs.replaced_in(style, &text) {
        out.lose(at, DiagnosticCode::InvalidEncoding)?;
    }
    out.put(at, text)
}

/// One worksheet part, with the relationship id of the drawing it places.
/// Rows come from the preceding BrtRowHdr; every cell record carries only
/// its column and cellXfs index in the common cell header (MS-XLSB 2.5.10).
fn read_sheet(
    parse_ctx: &ParseCtx<'_>,
    data: &[u8],
    shared: &[(String, bool)],
    xfs: &CellXfs,
    dates: DateSystem,
) -> Result<(SheetContent, Option<String>), ConvertError> {
    let mut out = SheetContent::on(
        OOXML_GRID,
        parse_ctx.grid_slots_left(),
        parse_ctx.limits().max_grid_slots,
    );
    let mut drawing = None;
    let mut row: Option<u32> = None;
    let mut records = Records::new(parse_ctx, data);
    while let Some((id, payload)) = records.next()? {
        let mut f = Fields::new(payload);
        match id {
            BRT_ROW_HDR => {
                let rw = f.u32()?;
                row = Some(rw);
                f.skip(7)?; // ixfe, miyRw, fExtraAsc/fExtraDsc byte
                if f.u8()? & 0x10 != 0 {
                    // fDyZero: the row is hidden.
                    out.hide_row(rw);
                }
            }
            BRT_COL_INFO => {
                let first = f.u32()?;
                let last = f.u32()?;
                f.skip(8)?; // coldx, ixfe
                if f.u16()? & 1 != 0 {
                    out.hide_cols(first, last);
                }
            }
            BRT_MERGE_CELL => out.merge(range(&mut f)?),
            BRT_CELL_RK | BRT_CELL_ERROR | BRT_CELL_BOOL | BRT_CELL_REAL | BRT_CELL_ST
            | BRT_CELL_ISST | BRT_CELL_RSTRING | BRT_FMLA_STRING | BRT_FMLA_NUM | BRT_FMLA_BOOL
            | BRT_FMLA_ERROR => {
                read_cell(id, &mut f, row, shared, xfs, dates, &mut out)?;
            }
            BRT_HLINK => match unless_fatal(hyperlink(&mut f))? {
                Some(link) => out.links.extend(link),
                None => out.report(DiagnosticCode::LinkTargetUnread, Region::Body),
            },
            BRT_BEGIN_HEADER_FOOTER => {
                unless_fatal(omit_headers_and_footers(&mut out, &mut f))?;
            }
            BRT_DRAWING => match unless_fatal(f.nullable_wide_string())? {
                Some(id) => drawing = id.map(|(id, _)| id),
                None => out.report(DiagnosticCode::PartUnreadable, Region::Body),
            },
            _ => {}
        }
    }
    Ok((out, drawing))
}

/// A BrtHLink's range and relationship id; `None` for a link within the
/// workbook, which names no relationship.
fn hyperlink(f: &mut Fields<'_>) -> Result<Option<SheetLink>, ConvertError> {
    let range = range(f)?;
    let rel = f.nullable_wide_string()?.map(|(rel, _)| rel).filter(|rel| !rel.is_empty());
    Ok(rel.map(|rel| SheetLink { range, target: LinkTarget::Related(rel) }))
}

/// An UncheckedRfX: first and last row, then first and last column.
fn range(f: &mut Fields<'_>) -> Result<CellRange, ConvertError> {
    let (r1, r2, c1, c2) = (f.u32()?, f.u32()?, f.u32()?, f.u32()?);
    Ok(CellRange::spanning(CellRef::new(r1, c1), CellRef::new(r2, c2)))
}

/// Hold a report of each non-blank header and footer of a
/// BrtBeginHeaderFooter, in its order: odd, even and first-page header and
/// footer pairs.
fn omit_headers_and_footers(
    out: &mut SheetContent,
    f: &mut Fields<'_>,
) -> Result<(), ConvertError> {
    f.u16()?; // fHFDiffOddEven, fHFDiffFirst, fHFScaleWithDoc, fHFAlignMargins
    let (header, footer) = (Region::Header, Region::Footer);
    for region in [header, footer, header, footer, header, footer] {
        if f.nullable_wide_string()?.is_some_and(|(s, _)| !s.trim().is_empty()) {
            out.omit_header_footer(region);
        }
    }
    Ok(())
}

/// A cell's rendered text from the record body after the common header,
/// what rendering it lost, and whether its string decoded with a
/// replacement. Formula records carry their cached result first; the parsed
/// formula trails it and is never read.
fn cell_text(
    id: u16,
    f: &mut Fields<'_>,
    fmt: &CellFormat,
    shared: &[(String, bool)],
    dates: DateSystem,
) -> Result<(String, Option<DiagnosticCode>, bool), ConvertError> {
    let unread = (String::new(), Some(DiagnosticCode::CellValueUnreadable), false);
    let string = |text: &str, replaced: bool| {
        let (text, loss) = format_as_text(fmt, text);
        (text, loss, replaced)
    };
    let number = |(text, loss): (String, Option<DiagnosticCode>)| (text, loss, false);
    Ok(match id {
        BRT_CELL_RK => number(render_number(fmt, rk_number(f.u32()?), dates)),
        BRT_CELL_REAL | BRT_FMLA_NUM => number(render_number(fmt, f.f64()?, dates)),
        BRT_CELL_ST | BRT_FMLA_STRING | BRT_CELL_RSTRING => {
            if id == BRT_CELL_RSTRING {
                f.u8()?; // RichStr flags; runs and phonetic data trail the string
            }
            let (text, replaced) = f.wide_string()?;
            string(&clean_text(&text), replaced)
        }
        BRT_CELL_ISST => match usize::try_from(f.u32()?).ok().and_then(|i| shared.get(i)) {
            Some((text, replaced)) => string(text, *replaced),
            None => unread,
        },
        BRT_CELL_BOOL | BRT_FMLA_BOOL => match f.u8()? {
            1 => ("TRUE".to_string(), None, false),
            0 => ("FALSE".to_string(), None, false),
            _ => unread,
        },
        BRT_CELL_ERROR | BRT_FMLA_ERROR => match error_literal(f.u8()?) {
            Some(text) => (text.to_string(), None, false),
            None => unread,
        },
        _ => (String::new(), None, false),
    })
}

/// Iterator over one part's record stream: a 1-2 byte record type and a 1-4
/// byte payload size, both 7 bits per byte with the high bit as the
/// continuation flag (MS-XLSB 2.1.4).
struct Records<'a> {
    reader: ByteReader<'a>,
    seen: u64,
    max: u64,
}

impl<'a> Records<'a> {
    fn new(parse_ctx: &ParseCtx<'_>, data: &'a [u8]) -> Self {
        Self { reader: ByteReader::new(data), seen: 0, max: parse_ctx.limits().max_records }
    }

    fn next(&mut self) -> Result<Option<(u16, &'a [u8])>, ConvertError> {
        if self.reader.rest().is_empty() {
            return Ok(None);
        }
        self.seen += 1;
        if self.seen > self.max {
            return Err(ConvertError::limit(Limit::Records, self.max));
        }
        let b0 = self.byte()?;
        let id = if b0 & 0x80 != 0 {
            u16::from(b0 & 0x7F) | (u16::from(self.byte()? & 0x7F) << 7)
        } else {
            u16::from(b0)
        };
        let mut size: usize = 0;
        for i in 0..4 {
            let b = self.byte()?;
            size |= usize::from(b & 0x7F) << (7 * i);
            if b & 0x80 == 0 {
                break;
            }
            if i == 3 {
                return Err(ConvertError::malformed("record size runs past four bytes"));
            }
        }
        let payload = self
            .reader
            .take(size)
            .ok_or_else(|| ConvertError::malformed("record payload runs past the part"))?;
        Ok(Some((id, payload)))
    }

    fn byte(&mut self) -> Result<u8, ConvertError> {
        self.reader.u8().ok_or_else(|| ConvertError::malformed("truncated record header"))
    }
}

/// Field reads within one record payload, each failing `Malformed` when the
/// payload is too short.
struct Fields<'a>(ByteReader<'a>);

impl<'a> Fields<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self(ByteReader::new(data))
    }

    fn need<T>(read: Option<T>) -> Result<T, ConvertError> {
        read.ok_or_else(|| ConvertError::malformed("truncated record payload"))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ConvertError> {
        Self::need(self.0.take(n))
    }

    fn skip(&mut self, n: usize) -> Result<(), ConvertError> {
        Self::need(self.0.skip(n))
    }

    fn u8(&mut self) -> Result<u8, ConvertError> {
        Self::need(self.0.u8())
    }

    fn u16(&mut self) -> Result<u16, ConvertError> {
        Self::need(self.0.u16())
    }

    fn u32(&mut self) -> Result<u32, ConvertError> {
        Self::need(self.0.u32())
    }

    fn f64(&mut self) -> Result<f64, ConvertError> {
        Self::need(self.0.f64())
    }

    /// XLWideString (MS-XLSB 2.5.169): a u32 character count, then that many
    /// UTF-16LE code units. Unpaired surrogates decode to U+FFFD, and the
    /// text comes with whether one did.
    fn wide_string(&mut self) -> Result<(String, bool), ConvertError> {
        let cch = self.u32()? as usize;
        let len =
            cch.checked_mul(2).ok_or_else(|| ConvertError::malformed("oversized string length"))?;
        let bytes = self.take(len)?;
        Ok(decode_utf16(utf16le_units(bytes)))
    }

    /// XLNullableWideString: the reserved count 0xFFFFFFFF is the null
    /// string.
    fn nullable_wide_string(&mut self) -> Result<Option<(String, bool)>, ConvertError> {
        if self.0.rest().starts_with(&[0xFF; 4]) {
            self.skip(4)?;
            return Ok(None);
        }
        self.wide_string().map(Some)
    }
}

#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    clippy::format_push_string,
    reason = "fixture records take known small sizes in fixed-width fields, and fixture XML is built with format!"
)]
mod tests {
    use super::super::grid::{MAX_COLS, MAX_ROWS};
    use super::super::test_support::{Reported, first_table, in_unit, origin, reported, texts};
    use super::*;
    use crate::model::{Block, UnitKind};
    use crate::package::xml::ns::PKG_RELS;
    use crate::shared::test_support::{unit_starts, zip_package};
    use std::assert_matches;

    const BRT_WS_DIM: u16 = 148;
    const WS_REL: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet";

    /// Route through the shared container dispatch, the only entry a caller
    /// has.
    fn parse(bytes: &[u8]) -> Result<Document, ConvertError> {
        super::super::parse(&ParseCtx::default(), bytes)
    }

    /// One record in canonical (shortest) framing.
    fn rec(id: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        if id < 0x80 {
            out.push(id as u8);
        } else {
            out.push((id & 0x7F) as u8 | 0x80);
            out.push((id >> 7) as u8);
        }
        let mut size = payload.len() as u32;
        loop {
            let low = (size & 0x7F) as u8;
            size >>= 7;
            if size == 0 {
                out.push(low);
                break;
            }
            out.push(low | 0x80);
        }
        out.extend_from_slice(payload);
        out
    }

    fn wunits(units: &[u16]) -> Vec<u8> {
        let mut out = (units.len() as u32).to_le_bytes().to_vec();
        out.extend(units.iter().flat_map(|u| u.to_le_bytes()));
        out
    }

    fn wstr(s: &str) -> Vec<u8> {
        wunits(&s.encode_utf16().collect::<Vec<_>>())
    }

    fn cell_rec(id: u16, col: u32, style: u32, payload: &[u8]) -> Vec<u8> {
        rec(id, &[&col.to_le_bytes()[..], &style.to_le_bytes(), payload].concat())
    }

    fn text_cell(col: u32, s: &str) -> Vec<u8> {
        cell_rec(BRT_CELL_ST, col, 0, &wstr(s))
    }

    fn row_hdr(r: u32, hidden: bool) -> Vec<u8> {
        let mut p = r.to_le_bytes().to_vec();
        p.extend_from_slice(&[0; 4]); // ixfe
        p.extend_from_slice(&[0; 2]); // miyRw
        p.push(0); // fExtraAsc / fExtraDsc
        p.push(if hidden { 0x10 } else { 0 }); // iOutLevel..fDyZero..
        p.push(0); // fPhShow
        p.extend_from_slice(&0u32.to_le_bytes()); // ccolspan
        rec(BRT_ROW_HDR, &p)
    }

    fn real_cell(col: u32, style: u32, v: f64) -> Vec<u8> {
        cell_rec(BRT_CELL_REAL, col, style, &v.to_le_bytes())
    }

    fn col_info(first: u32, last: u32, hidden: bool) -> Vec<u8> {
        let mut p = first.to_le_bytes().to_vec();
        p.extend_from_slice(&last.to_le_bytes());
        p.extend_from_slice(&[0; 8]); // coldx, ixfe
        p.extend_from_slice(&u16::from(hidden).to_le_bytes());
        rec(BRT_COL_INFO, &p)
    }

    fn merge(r1: u32, r2: u32, c1: u32, c2: u32) -> Vec<u8> {
        let p = [r1.to_le_bytes(), r2.to_le_bytes(), c1.to_le_bytes(), c2.to_le_bytes()].concat();
        rec(BRT_MERGE_CELL, &p)
    }

    /// styles.bin: custom BrtFmt entries and the cellXfs list by numFmtId. A
    /// cell-style BrtXF precedes BrtBeginCellXFs to prove it shifts nothing.
    fn styles(fmts: &[(u16, &str)], xf_fmt_ids: &[u16]) -> Vec<u8> {
        let xf = |parent: u16, ifmt: u16| {
            let mut p = parent.to_le_bytes().to_vec();
            p.extend_from_slice(&ifmt.to_le_bytes());
            p.extend_from_slice(&[0; 12]);
            rec(BRT_XF, &p)
        };
        let mut out = Vec::new();
        for (id, code) in fmts {
            let mut p = id.to_le_bytes().to_vec();
            p.extend(wstr(code));
            out.extend(rec(BRT_FMT, &p));
        }
        out.extend(xf(0xFFFF, 9)); // cell-style XF outside cellXfs
        out.extend(rec(BRT_BEGIN_CELL_XFS, &(xf_fmt_ids.len() as u32).to_le_bytes()));
        for id in xf_fmt_ids {
            out.extend(xf(0, *id));
        }
        out.extend(rec(BRT_END_CELL_XFS, &[]));
        out
    }

    fn shared_strings(items: &[&str]) -> Vec<u8> {
        let units: Vec<Vec<u16>> = items.iter().map(|s| s.encode_utf16().collect()).collect();
        shared_strings_units(&units.iter().map(Vec::as_slice).collect::<Vec<_>>())
    }

    /// Assemble a workbook: (name, hsState, sheet records) per sheet, plus
    /// optional styles and shared-string parts.
    #[derive(Default)]
    struct Wb<'a> {
        sheets: Vec<(&'a str, u32, Vec<u8>)>,
        styles: Option<Vec<u8>>,
        shared: Option<Vec<u8>>,
        date1904: bool,
        /// Further parts, verbatim: (name, body).
        extra: Vec<(&'a str, Vec<u8>)>,
        /// Further workbook records and relationships, verbatim.
        bundles: Vec<u8>,
        workbook_rels: &'a str,
    }

    impl Wb<'_> {
        fn build(&self) -> Vec<u8> {
            let mut workbook = Vec::new();
            let mut prop = u32::from(self.date1904).to_le_bytes().to_vec();
            prop.extend_from_slice(&0u32.to_le_bytes()); // dwThemeVersion
            prop.extend(wstr("")); // strName
            workbook.extend(rec(BRT_WB_PROP, &prop));
            let mut rels = String::new();
            for (i, (name, state, _)) in self.sheets.iter().enumerate() {
                let id = i + 1;
                let mut p = state.to_le_bytes().to_vec();
                p.extend_from_slice(&(id as u32).to_le_bytes()); // iTabID
                p.extend(wstr(&format!("rId{id}")));
                p.extend(wstr(name));
                workbook.extend(rec(BRT_BUNDLE_SH, &p));
                rels.push_str(&format!(
                    r#"<Relationship Id="rId{id}" Type="{WS_REL}" Target="worksheets/sheet{id}.bin"/>"#
                ));
            }
            workbook.extend_from_slice(&self.bundles);
            rels.push_str(self.workbook_rels);
            if self.styles.is_some() {
                rels.push_str(&format!(
                    r#"<Relationship Id="rId90" Type="{}" Target="styles.bin"/>"#,
                    rel_type::STYLES
                ));
            }
            if self.shared.is_some() {
                rels.push_str(&format!(
                    r#"<Relationship Id="rId91" Type="{SHARED_STRINGS_REL}" Target="sharedStrings.bin"/>"#
                ));
            }
            let rels = format!(
                r#"<?xml version="1.0"?><Relationships xmlns="{PKG_RELS}">{rels}</Relationships>"#
            );
            let mut parts = Vec::new();
            let mut add = |name: &str, body: &[u8]| parts.push((name.to_string(), body.to_vec()));
            add("xl/workbook.bin", &workbook);
            add("xl/_rels/workbook.bin.rels", rels.as_bytes());
            for (i, (_, _, body)) in self.sheets.iter().enumerate() {
                add(&format!("xl/worksheets/sheet{}.bin", i + 1), body);
            }
            if let Some(styles) = &self.styles {
                add("xl/styles.bin", styles);
            }
            if let Some(shared) = &self.shared {
                add("xl/sharedStrings.bin", shared);
            }
            for (name, body) in &self.extra {
                add(name, body);
            }
            zip_package(&parts)
        }
    }

    fn one_sheet(body: Vec<u8>) -> Wb<'static> {
        Wb { sheets: vec![("S", 0, body)], ..Wb::default() }
    }

    fn table(wb: &Wb<'_>) -> Vec<Vec<String>> {
        texts(first_table(&parse(&wb.build()).unwrap()))
    }

    #[test]
    fn record_framing_uses_seven_bit_groups() {
        let mut stream = rec(BRT_BUNDLE_SH, &[7u8; 300]);
        assert_eq!(&stream[..4], &[0x9C, 0x01, 0xAC, 0x02]);
        stream.extend(rec(BRT_CELL_RK, &[1, 2, 3]));
        let mut records = Records::new(&ParseCtx::default(), &stream);
        let (id, payload) = records.next().unwrap().unwrap();
        assert_eq!((id, payload.len()), (BRT_BUNDLE_SH, 300));
        let (id, payload) = records.next().unwrap().unwrap();
        assert_eq!((id, payload), (BRT_CELL_RK, &[1u8, 2, 3][..]));
        assert!(records.next().unwrap().is_none());
    }

    #[test]
    fn truncated_record_stream_is_malformed() {
        let mut stream = rec(BRT_CELL_RK, &[0u8; 10]);
        stream.truncate(stream.len() - 1);
        let mut records = Records::new(&ParseCtx::default(), &stream);
        assert_matches!(
            records.next(),
            Err(ConvertError::Malformed { reason: "record payload runs past the part", .. })
        );
    }

    #[test]
    fn a_record_header_cut_short_or_overlong_is_malformed() {
        for (stream, reason) in [
            (&[0x80][..], "truncated record header"),
            (&[0x01][..], "truncated record header"),
            (&[0x01, 0x80, 0x80, 0x80][..], "truncated record header"),
            (&[0x01, 0x80, 0x80, 0x80, 0x80][..], "record size runs past four bytes"),
        ] {
            let mut records = Records::new(&ParseCtx::default(), stream);
            assert_matches!(
                records.next(),
                Err(ConvertError::Malformed { reason: r, .. }) if r == reason,
                "{stream:?}"
            );
        }
    }

    #[test]
    fn a_field_past_its_payload_is_malformed() {
        fn truncated<T>(result: &Result<T, ConvertError>) -> bool {
            matches!(
                result,
                Err(ConvertError::Malformed { reason: "truncated record payload", .. })
            )
        }
        assert!(truncated(&Fields::new(&[1, 2, 3]).u32()));
        assert!(truncated(&Fields::new(&[1]).u16()));
        assert!(truncated(&Fields::new(&[0; 7]).f64()));
        assert!(truncated(&Fields::new(&[]).u8()));
        assert!(truncated(&Fields::new(&[0; 6]).skip(7)));
        assert!(truncated(&Fields::new(&[2, 0, 0, 0, b'a', 0, b'b']).wide_string()));
        let mut f = Fields::new(&[1, 0, 0, 0, b'a', 0, 9]);
        assert_eq!(f.wide_string().unwrap(), ("a".to_string(), false));
        assert_eq!(f.u8().unwrap(), 9);
    }

    #[test]
    fn a_nullable_string_is_null_only_on_four_bytes_of_ff() {
        let mut f = Fields::new(&[0xFF, 0xFF, 0xFF, 0xFF, 7]);
        assert_eq!(f.nullable_wide_string().unwrap(), None);
        assert_eq!(f.u8().unwrap(), 7);
        assert_matches!(
            Fields::new(&[0xFF, 0xFF, 0xFF]).nullable_wide_string(),
            Err(ConvertError::Malformed { reason: "truncated record payload", .. })
        );
        assert_matches!(
            Fields::new(&[0xFF, 0xFF, 0xFF, 0x00]).nullable_wide_string(),
            Err(ConvertError::Malformed { reason: "truncated record payload", .. })
        );
        let mut f = Fields::new(&[1, 0, 0, 0, b'a', 0]);
        assert_eq!(f.nullable_wide_string().unwrap(), Some(("a".to_string(), false)));
    }

    #[test]
    fn number_formats_apply_to_stored_values() {
        let mut body = row_hdr(0, false);
        let rk_three_quarters = (75u32 << 2) | 2 | 1;
        body.extend(cell_rec(BRT_CELL_RK, 0, 1, &rk_three_quarters.to_le_bytes()));
        body.extend(real_cell(1, 2, 1234.5));
        body.extend(real_cell(2, 3, 46096.0));
        body.extend(real_cell(3, 0, 1234.5)); // General
        let wb = Wb {
            styles: Some(styles(
                &[(164, "0.0%"), (165, "\"$\"#,##0.00"), (166, "mm/dd/yyyy")],
                &[0, 164, 165, 166],
            )),
            ..one_sheet(body)
        };
        assert_eq!(table(&wb), vec![vec!["75.0%", "$1,234.50", "2026-03-15", "1234.5"]]);
    }

    #[test]
    fn strings_resolve_from_the_shared_table_and_inline() {
        let mut body = row_hdr(0, false);
        body.extend(cell_rec(BRT_CELL_ISST, 0, 0, &1u32.to_le_bytes()));
        body.extend(text_cell(1, "  inline  "));
        let wb = Wb { shared: Some(shared_strings(&["zeroth", "shared"])), ..one_sheet(body) };
        assert_eq!(table(&wb), vec![vec!["shared", "  inline  "]]);
    }

    #[test]
    fn every_part_counts_records_against_the_contexts_limit() {
        let parse_under = |max_records, wb: &Wb<'_>| {
            let limits = crate::limits::Limits { max_records, ..crate::limits::Limits::default() };
            super::super::parse(&ParseCtx::new(&limits), &wb.build())
        };
        let three_cells = || {
            let mut body = row_hdr(0, false);
            for col in 0..3 {
                body.extend(real_cell(col, 0, 1.0));
            }
            body
        };
        let shared = Wb { shared: Some(shared_strings(&["a", "b", "c"])), ..one_sheet(Vec::new()) };
        parse_under(3, &shared).unwrap();
        let err = parse_under(2, &shared).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Records, max: 2 });
        let sheet = one_sheet(three_cells());
        parse_under(4, &sheet).unwrap();
        let err = parse_under(3, &sheet).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Records, max: 3 });
        let styled = Wb { styles: Some(styles(&[], &[0; 5])), ..one_sheet(Vec::new()) };
        parse_under(8, &styled).unwrap();
        let err = parse_under(7, &styled).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Records, max: 7 });
    }

    #[test]
    fn bool_and_error_cells_render_literally() {
        let mut body = row_hdr(0, false);
        body.extend(cell_rec(BRT_CELL_BOOL, 0, 0, &[1]));
        body.extend(cell_rec(BRT_CELL_BOOL, 1, 0, &[0]));
        body.extend(cell_rec(BRT_CELL_ERROR, 2, 0, &[0x07]));
        body.extend(cell_rec(BRT_CELL_ERROR, 3, 0, &[0x2A]));
        assert_eq!(table(&one_sheet(body)), vec![vec!["TRUE", "FALSE", "#DIV/0!", "#N/A"]]);
    }

    #[test]
    fn formula_records_render_their_cached_result() {
        let num =
            [&2.5f64.to_le_bytes()[..], &[0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x1E, 0x05, 0x00]];
        let string = [&wstr("computed")[..], &[0; 6]];
        let mut body = row_hdr(0, false);
        body.extend(cell_rec(BRT_FMLA_NUM, 0, 0, &num.concat()));
        body.extend(cell_rec(BRT_FMLA_STRING, 1, 0, &string.concat()));
        assert_eq!(table(&one_sheet(body)), vec![vec!["2.5", "computed"]]);
    }

    #[test]
    fn hidden_rows_columns_and_sheets_are_omitted() {
        let mut visible = col_info(1, 1, true);
        visible.extend(row_hdr(0, false));
        visible.extend(real_cell(0, 0, 1.0));
        visible.extend(real_cell(1, 0, 2.0)); // hidden column
        visible.extend(real_cell(2, 0, 3.0));
        visible.extend(row_hdr(1, true));
        visible.extend(real_cell(0, 0, 4.0)); // hidden row
        visible.extend(row_hdr(2, false));
        visible.extend(real_cell(0, 0, 5.0));
        let mut secret = row_hdr(0, false);
        secret.extend(real_cell(0, 0, 9.0));
        let wb = Wb {
            sheets: vec![
                ("Shown", 0, visible),
                ("Hidden", 1, secret.clone()),
                ("VeryHidden", 2, secret),
            ],
            ..Wb::default()
        };
        let doc = parse(&wb.build()).unwrap();
        assert_matches!(doc.blocks.as_slice(), [Block::UnitStart(unit), Block::Table(_)] if unit.ordinal.get() == 1, "hidden sheets must add no unit, heading or table");
        assert_eq!(texts(first_table(&doc)), vec![vec!["1", "3"], vec!["5", ""]]);
    }

    #[test]
    fn a_merge_past_the_content_is_clamped_to_it() {
        let mut body = row_hdr(0, false);
        body.extend(text_cell(5, "wide"));
        body.extend(merge(0, 2, 5, 14));
        let wb = one_sheet(body);
        assert_eq!(table(&wb), vec![vec!["wide"]]);
        assert_eq!(diagnose(&wb), []);
    }

    #[test]
    fn a_merge_past_the_grid_is_clamped_and_one_outside_it_dropped() {
        let mut body = row_hdr(0, false);
        body.extend(string_cell(MAX_COLS - 2, &[0x61]));
        body.extend(merge(0, 1, MAX_COLS - 2, MAX_COLS + 5));
        body.extend(merge(2, 3, MAX_COLS, MAX_COLS + 1));
        body.extend(merge(MAX_ROWS, u32::MAX, 0, 1));
        let wb = one_sheet(body);
        let doc = parse(&wb.build()).unwrap();
        let table = first_table(&doc);
        let cell = origin(table);
        assert_eq!((table.grid.len(), cell.col_span, cell.row_span), (1, 1, 1));
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::TableSpanRepaired, 1, 3)]);
    }

    #[test]
    fn date1904_serials_shift_epoch() {
        let mut body = row_hdr(0, false);
        body.extend(real_cell(0, 0, 100.0));
        let wb = Wb {
            styles: Some(styles(&[(164, "yyyy-mm-dd")], &[164])),
            date1904: true,
            ..one_sheet(body)
        };
        assert_eq!(table(&wb), vec![vec!["1904-04-10"]]);
    }

    #[test]
    fn unknown_records_are_skipped_without_desync() {
        let mut body = rec(BRT_WS_DIM, &[0u8; 16]);
        body.extend(row_hdr(0, false));
        body.extend(real_cell(0, 0, 7.0));
        assert_eq!(table(&one_sheet(body)), vec![vec!["7"]]);
    }

    #[test]
    fn form_control_checkboxes_come_from_the_vml_part() {
        const VML_REL: &str =
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/vmlDrawing";
        let rels = format!(
            r#"<?xml version="1.0"?><Relationships xmlns="{PKG_RELS}"><Relationship Id="rId1" Type="{VML_REL}" Target="../drawings/vmlDrawing1.vml"/></Relationships>"#
        );
        let vml = r##"<xml xmlns:v="urn:schemas-microsoft-com:vml" xmlns:x="urn:schemas-microsoft-com:office:excel">
            <v:shape id="_x0000_s1025" type="#_x0000_t201" style="position:absolute">
              <v:textbox><div><font>Roof</font></div></v:textbox>
              <x:ClientData ObjectType="Checkbox"><x:Anchor>1, 5, 0, 2, 2, 10, 1, 1</x:Anchor><x:Checked>1</x:Checked></x:ClientData>
            </v:shape></xml>"##;
        let mut body = row_hdr(0, false);
        body.extend(cell_rec(BRT_CELL_BOOL, 0, 0, &[1]));
        let wb = Wb {
            sheets: vec![("S", 0, body)],
            extra: vec![
                ("xl/worksheets/_rels/sheet1.bin.rels", rels.into_bytes()),
                ("xl/drawings/vmlDrawing1.vml", vml.as_bytes().to_vec()),
            ],
            ..Wb::default()
        };
        assert_eq!(table(&wb), vec![vec!["TRUE", "[x] Roof"]]);
    }

    fn diagnose(wb: &Wb<'_>) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        super::super::parse(&parse_ctx, &wb.build()).unwrap();
        reported(parse_ctx)
    }

    /// A BrtBundleSh naming its relationship (`None` for the null string)
    /// and its name as UTF-16 code units.
    fn bundle(name: &[u16], rid: Option<&str>) -> Vec<u8> {
        let mut p = 0u32.to_le_bytes().to_vec();
        p.extend_from_slice(&9u32.to_le_bytes());
        match rid {
            Some(rid) => p.extend(wstr(rid)),
            None => p.extend_from_slice(&[0xFF; 4]),
        }
        p.extend(wunits(name));
        rec(BRT_BUNDLE_SH, &p)
    }

    /// A RichStr with no formatting runs and no phonetic data.
    fn rich_str(string: &[u8]) -> Vec<u8> {
        [&[0][..], string].concat()
    }

    fn shared_strings_units(items: &[&[u16]]) -> Vec<u8> {
        items.iter().flat_map(|units| rec(BRT_SST_ITEM, &rich_str(&wunits(units)))).collect()
    }

    fn one_value() -> Vec<u8> {
        [row_hdr(0, false), real_cell(0, 0, 1.0)].concat()
    }

    fn string_cell(col: u32, units: &[u16]) -> Vec<u8> {
        cell_rec(BRT_CELL_ST, col, 0, &wunits(units))
    }

    #[test]
    fn each_sheet_site_reports_its_loss_in_its_sheets_unit() {
        let rels = format!(
            r#"<Relationship Id="rId6" Type="{WS_REL}" Target="worksheets/gone.bin"/><Relationship Id="rId7" Type="{WS_REL}" Target="worksheets/a%2Fb.bin"/>"#
        );
        let name = |s: &str| s.encode_utf16().collect::<Vec<_>>();
        let wb = Wb {
            sheets: vec![("A", 0, one_value()), ("H", 1, one_value()), ("C", 0, vec![0x80])],
            bundles: [
                bundle(&name("X"), Some("rId77")),
                bundle(&name("N"), None),
                bundle(&name("G"), Some("rId6")),
                bundle(&name("E"), Some("rId7")),
            ]
            .concat(),
            workbook_rels: &rels,
            ..Wb::default()
        };
        let doc = parse(&wb.build()).unwrap();
        assert_matches!(doc.blocks.as_slice(), [Block::UnitStart(unit), Block::Heading { .. }, Block::Table(_)] if unit.ordinal.get() == 1, "unreadable sheets add no unit");
        let lost = |unit| in_unit(DiagnosticCode::UnitUnreadable, unit, 1);
        assert_eq!(
            diagnose(&wb),
            [
                lost(3),
                lost(4),
                lost(5),
                lost(6),
                lost(7),
                in_unit(DiagnosticCode::HiddenOmitted, 2, 1)
            ]
        );
    }

    #[test]
    fn a_shown_or_hidden_sheet_holds_its_number_against_max_pages() {
        use crate::limits::Limits;
        for hidden in [[0, 1, 0], [0, 0, 1]] {
            let sheet = |(name, flag)| (name, flag, one_value());
            let sheets = ["A", "B", "C"].into_iter().zip(hidden).map(sheet).collect();
            let bytes = Wb { sheets, ..Wb::default() }.build();
            let at = |max_pages| {
                let limits = Limits { max_pages, ..Limits::DEFAULT };
                super::super::parse(&ParseCtx::new(&limits), &bytes)
            };
            assert!(at(3).is_ok(), "{hidden:?}");
            assert_matches!(
                at(2),
                Err(ConvertError::ResourceLimit { limit: Limit::Pages, max: 2 }),
                "{hidden:?}"
            );
        }
    }

    #[test]
    fn a_shown_sheet_name_that_does_not_decode_reports_with_its_heading() {
        let rels =
            format!(r#"<Relationship Id="rId8" Type="{WS_REL}" Target="worksheets/sheet1.bin"/>"#);
        let broken = |rid| bundle(&[0x42, 0xD800], Some(rid));
        let wb = Wb { bundles: broken("rId8"), workbook_rels: &rels, ..one_sheet(one_value()) };
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::InvalidEncoding, 2, 1)]);
        let hidden = |rid| {
            let mut record = broken(rid);
            record[3] = 1;
            record
        };
        let wb = Wb { bundles: hidden("rId8"), workbook_rels: &rels, ..one_sheet(one_value()) };
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::HiddenOmitted, 2, 1)]);
    }

    #[test]
    fn several_shown_sheets_are_headed_by_their_names() {
        let sheets = vec![("First", 0, one_value()), ("Second", 0, one_value())];
        let doc = parse(&Wb { sheets, ..Wb::default() }.build()).unwrap();
        let headings = doc.blocks.iter().filter(|b| matches!(b, Block::Heading { .. }));
        assert_eq!(headings.map(Block::plain_text).collect::<Vec<_>>(), ["First", "Second"]);
    }

    #[test]
    fn a_cell_record_that_does_not_read_reports_it() {
        let mut body = real_cell(0, 0, 1.0);
        body.extend(row_hdr(0, false));
        body.extend(cell_rec(BRT_CELL_ISST, 1, 0, &7u32.to_le_bytes()));
        body.extend(cell_rec(BRT_CELL_ERROR, 2, 0, &[0x99]));
        body.extend(cell_rec(BRT_FMLA_BOOL, 3, 0, &[2]));
        body.extend(string_cell(4, &[0xD800, 0x61]));
        body.extend(cell_rec(BRT_CELL_ISST, 5, 0, &1u32.to_le_bytes()));
        body.extend(real_cell(6, 0, 2.0));
        body.extend(cell_rec(BRT_CELL_RK, 7, 5, &((7u32 << 2) | 2).to_le_bytes()));
        body.extend(cell_rec(BRT_CELL_RSTRING, 8, 0, &rich_str(&wunits(&[0xDC00, 0x61]))));
        body.extend(merge(0, 0, MAX_COLS, MAX_COLS));
        let shared = shared_strings_units(&[&[0x61], &[0xDC00], &[0xD800]]);
        let wb = Wb { shared: Some(shared), ..one_sheet(body) };
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::CellValueUnreadable, 1, 4),
                in_unit(DiagnosticCode::InvalidEncoding, 1, 3),
                in_unit(DiagnosticCode::NumberFormatFallback, 1, 1),
                in_unit(DiagnosticCode::TableSpanRepaired, 1, 1),
            ]
        );
    }

    #[test]
    fn a_hidden_cell_reports_only_that_it_is_hidden() {
        let mut body = col_info(2, 2, true);
        body.extend(row_hdr(0, false));
        body.extend(real_cell(0, 0, 1.0));
        body.extend(string_cell(2, &[0xD800]));
        body.extend(row_hdr(1, true));
        body.extend(cell_rec(BRT_CELL_ISST, 0, 0, &7u32.to_le_bytes()));
        body.extend(real_cell(1, 5, 1.0));
        assert_eq!(diagnose(&one_sheet(body)), [in_unit(DiagnosticCode::HiddenOmitted, 1, 2)]);
    }

    #[test]
    fn a_hidden_row_past_the_grid_reports_its_content_as_hidden_only() {
        let body = [one_value(), row_hdr(MAX_ROWS, true), real_cell(0, 0, 2.0)].concat();
        let wb = one_sheet(body);
        assert_eq!(table(&wb), vec![vec!["1"]]);
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::HiddenOmitted, 1, 1)]);
    }

    #[test]
    fn a_hidden_column_past_the_grid_reports_its_content_as_hidden_only() {
        let body =
            [col_info(MAX_COLS, MAX_COLS + 5, true), one_value(), real_cell(MAX_COLS, 0, 2.0)]
                .concat();
        let wb = one_sheet(body);
        assert_eq!(table(&wb), vec![vec!["1"]]);
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::HiddenOmitted, 1, 1)]);
    }

    #[test]
    fn a_merge_moved_off_a_hidden_origin_replaces_the_covered_text_it_lands_on() {
        let run = |body: Vec<u8>| table(&one_sheet(body));
        let below = [
            row_hdr(0, true),
            row_hdr(1, false),
            text_cell(0, "covered"),
            text_cell(1, "y"),
            merge(0, 2, 0, 0),
        ];
        assert_eq!(run(below.concat()), vec![vec!["y"]]);
        let beside = [
            col_info(0, 0, true),
            row_hdr(0, false),
            text_cell(1, "covered"),
            text_cell(2, "y"),
            merge(0, 0, 0, 1),
        ];
        assert_eq!(run(beside.concat()), vec![vec!["y"]]);
        let kept = [
            row_hdr(0, true),
            text_cell(0, "kept"),
            row_hdr(1, false),
            text_cell(0, "covered"),
            text_cell(1, "y"),
            merge(0, 2, 0, 0),
        ];
        assert_eq!(run(kept.concat()), vec![vec!["kept", "y"]]);
    }

    #[test]
    fn a_cell_a_merge_clamped_to_the_content_covers_shows_nothing() {
        let body = [
            row_hdr(1, false),
            text_cell(0, "a"),
            text_cell(1, "hidden"),
            text_cell(2, "c"),
            merge(0, 1, 1, 1),
        ]
        .concat();
        let wb = one_sheet(body);
        assert_eq!(table(&wb), vec![vec!["a", "", "c"]]);
        assert_eq!(diagnose(&wb), []);
        let body = [
            row_hdr(2, false),
            text_cell(0, "a"),
            text_cell(1, "hidden"),
            text_cell(3, "d"),
            merge(1, 2, 1, 2),
        ]
        .concat();
        assert_eq!(table(&one_sheet(body)), vec![vec!["a", "", "<covered>", "d"]]);
    }

    #[test]
    fn a_cell_before_any_row_header_is_unreadable_only_where_its_text_shows() {
        let body = [
            string_cell(0, &[0x20]),
            string_cell(1, &[]),
            real_cell(2, 0, 1.0),
            cell_rec(BRT_CELL_ERROR, 3, 0, &[0x99]),
            one_value(),
        ]
        .concat();
        let wb = one_sheet(body);
        assert_eq!(table(&wb), vec![vec!["1"]]);
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::CellValueUnreadable, 1, 2)]);
    }

    #[test]
    fn a_cell_past_the_grid_is_unreadable_only_where_its_text_shows() {
        let body = [
            row_hdr(MAX_ROWS, false),
            real_cell(0, 0, 1.0),
            cell_rec(BRT_CELL_ERROR, 1, 0, &[0x99]),
            string_cell(2, &[0x20]),
            row_hdr(0, false),
            real_cell(0, 0, 2.0),
            real_cell(MAX_COLS, 0, 3.0),
            string_cell(MAX_COLS + 1, &[]),
            row_hdr(1, true),
            real_cell(MAX_COLS, 0, 4.0),
        ]
        .concat();
        let wb = one_sheet(body);
        assert_eq!(table(&wb), vec![vec!["2"]]);
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::CellValueUnreadable, 1, 3),
                in_unit(DiagnosticCode::HiddenOmitted, 1, 1)
            ]
        );
    }

    #[test]
    fn cells_and_losses_held_as_the_sheet_streams_count_against_the_grid_slots_left() {
        use crate::limits::{Limit, Limits};
        let limits = Limits { max_grid_slots: 5, ..Limits::default() };
        let placed = [row_hdr(0, false), real_cell(0, 0, 1.0), real_cell(1, 0, 1.0)].concat();
        let run = |body: Vec<u8>| {
            let sheets = vec![("A", 0, placed.clone()), ("B", 0, body)];
            super::super::parse(&ParseCtx::new(&limits), &Wb { sheets, ..Wb::default() }.build())
                .map(drop)
        };
        let unread: fn(u32) -> Vec<u8> = |col| cell_rec(BRT_CELL_ISST, col, 0, &7u32.to_le_bytes());
        let hidden = |col: u32| real_cell(col, 0, 1.0);
        let past = |col: u32| real_cell(MAX_COLS + col, 0, 1.0);
        let cases = [("lose", false, unread), ("put", true, hidden), ("past", false, past)];
        for (case, row, cell) in cases {
            let body = |cells: u32| [row_hdr(0, row), (0..cells).flat_map(cell).collect()].concat();
            assert!(run(body(3)).is_ok(), "{case}");
            let refused = run(body(4));
            assert_matches!(
                refused,
                Err(ConvertError::ResourceLimit { limit: Limit::GridSlots, max: 5 }),
                "{case}"
            );
        }
    }

    fn short_real(col: u32) -> Vec<u8> {
        cell_rec(BRT_CELL_REAL, col, 0, &[0; 4])
    }

    #[test]
    fn a_truncated_cell_record_or_row_header_fails_its_sheet_wherever_it_lies() {
        let short_row = rec(BRT_ROW_HDR, &[MAX_ROWS.to_le_bytes().to_vec(), vec![0; 4]].concat());
        for body in [
            short_real(0),
            [row_hdr(MAX_ROWS, false), short_real(0)].concat(),
            [row_hdr(0, false), short_real(MAX_COLS)].concat(),
            [row_hdr(0, false), short_real(0)].concat(),
            [one_value(), short_row].concat(),
        ] {
            let alone = parse(&one_sheet(body.clone()).build());
            assert_matches!(alone, Err(ConvertError::Malformed { .. }));
            let beside =
                Wb { sheets: vec![("A", 0, one_value()), ("B", 0, body)], ..Wb::default() };
            assert_eq!(diagnose(&beside), [in_unit(DiagnosticCode::UnitUnreadable, 2, 1)]);
        }
    }

    #[test]
    fn a_sheet_a_later_record_fails_reports_only_that_it_is_unreadable() {
        let header_footer = |at: usize| {
            let mut p = 0u16.to_le_bytes().to_vec();
            for field in 0..6 {
                if field == at {
                    p.extend(wstr("&CTitle"));
                } else {
                    p.extend_from_slice(&[0xFF; 4]);
                }
            }
            rec(BRT_BEGIN_HEADER_FOOTER, &p)
        };
        let after = |record: Vec<u8>| [one_value(), record].concat();
        for (read, code) in [
            (after(merge(0, 0, 0, MAX_COLS)), DiagnosticCode::TableSpanRepaired),
            (after(merge(MAX_ROWS, MAX_ROWS, 0, 0)), DiagnosticCode::TableSpanRepaired),
            ([text_cell(0, "x"), one_value()].concat(), DiagnosticCode::CellValueUnreadable),
            (after(rec(BRT_HLINK, &[0; 6])), DiagnosticCode::LinkTargetUnread),
            (after(header_footer(0)), DiagnosticCode::HeaderOmitted),
            (after(header_footer(1)), DiagnosticCode::FooterOmitted),
            (after(rec(BRT_DRAWING, &[9, 0, 0, 0])), DiagnosticCode::PartUnreadable),
        ] {
            assert_eq!(diagnose(&one_sheet(read.clone())), [in_unit(code, 1, 1)], "{code:?}");
            let failed = [read, short_real(1)].concat();
            let sheets = vec![("A", 0, one_value()), ("B", 0, failed)];
            let beside = Wb { sheets, ..Wb::default() };
            let unreadable = in_unit(DiagnosticCode::UnitUnreadable, 2, 1);
            assert_eq!(diagnose(&beside), [unreadable], "{code:?}");
        }
    }

    #[test]
    fn a_format_code_that_does_not_decode_reports_where_it_shows() {
        let code: Vec<u16> =
            "0\" ".encode_utf16().chain([0xD800]).chain("\"".encode_utf16()).collect();
        let format = [164u16.to_le_bytes().to_vec(), wunits(&code)].concat();
        let styles = [rec(BRT_FMT, &format), styles(&[], &[0, 164])].concat();
        let body = [
            row_hdr(0, false),
            real_cell(0, 1, 2.0),
            real_cell(1, 0, 3.0),
            string_cell(2, &[0x74]),
            cell_rec(BRT_CELL_ST, 3, 1, &wunits(&[0xDC00])),
            row_hdr(1, true),
            real_cell(0, 1, 4.0),
        ]
        .concat();
        let wb = Wb { styles: Some(styles), ..one_sheet(body) };
        assert_eq!(table(&wb), vec![vec!["2 \u{FFFD}", "3", "t", "\u{FFFD}"]]);
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::InvalidEncoding, 1, 2),
                in_unit(DiagnosticCode::HiddenOmitted, 1, 1),
            ]
        );
    }

    #[test]
    fn a_format_code_past_the_binary_cap_falls_back_by_what_it_shows() {
        let digits = "0".repeat(256);
        let text = format!("0\"{}\"", "x".repeat(253));
        let styles = styles(&[(164, &digits), (165, &text)], &[0, 164, 165]);
        let body = [row_hdr(0, false), real_cell(0, 1, 2.0), real_cell(1, 2, 2.0)].concat();
        let wb = Wb { styles: Some(styles), ..one_sheet(body) };
        assert_eq!(table(&wb), vec![vec!["2", "2"]]);
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::NumberFormatUnread, 1, 1),
                in_unit(DiagnosticCode::NumberFormatFallback, 1, 1),
            ]
        );
    }

    #[test]
    fn a_number_under_a_code_past_the_binary_cap_is_judged_by_its_numeric_sections() {
        let x = "x".repeat(260);
        let unread = format!("0\"{x}\";0;0;@");
        let text_only = format!("0;0;0;\"{x}\"@");
        let last_at = format!("{};\"a\"@", "0".repeat(300));
        let styles =
            styles(&[(164, &unread), (165, &text_only), (166, &last_at)], &[0, 164, 165, 166]);
        let body =
            [row_hdr(0, false), real_cell(0, 1, 2.0), real_cell(1, 2, 2.0), real_cell(2, 3, 2.0)]
                .concat();
        let wb = Wb { styles: Some(styles), ..one_sheet(body) };
        assert_eq!(table(&wb), vec![vec!["2", "2", "2"]]);
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::NumberFormatUnread, 1, 1),
                in_unit(DiagnosticCode::NumberFormatFallback, 1, 2),
            ]
        );
    }

    #[test]
    fn a_text_section_past_the_binary_cap_reports_only_what_it_decorates() {
        let decorated = format!("0;0;0;\"{}\"@", "x".repeat(247));
        let plain = format!("{};@", "0".repeat(254));
        let styles = styles(&[(164, &decorated), (165, &plain)], &[0, 164, 165]);
        let text = |col: u32, style: u32| cell_rec(BRT_CELL_ST, col, style, &wstr("t"));
        let isst = cell_rec(BRT_CELL_ISST, 2, 1, &0u32.to_le_bytes());
        let rich = cell_rec(BRT_CELL_RSTRING, 3, 1, &rich_str(&wstr("t")));
        let body = [row_hdr(0, false), text(0, 1), text(1, 2), isst, rich].concat();
        let wb =
            Wb { styles: Some(styles), shared: Some(shared_strings(&["t"])), ..one_sheet(body) };
        assert_eq!(table(&wb), vec![vec!["t", "t", "t", "t"]]);
        assert_eq!(diagnose(&wb), [in_unit(DiagnosticCode::NumberFormatUnread, 1, 3)]);
    }

    #[test]
    fn a_replaced_string_under_an_unread_text_section_reports_both() {
        let decorated = format!("0;0;0;\"{}\"@", "x".repeat(247));
        let styles = styles(&[(164, &decorated)], &[0, 164]);
        let units = [0x74u16, 0xD800];
        let string = wunits(&units);
        let st = cell_rec(BRT_CELL_ST, 0, 1, &string);
        let isst = cell_rec(BRT_CELL_ISST, 1, 1, &0u32.to_le_bytes());
        let rich = cell_rec(BRT_CELL_RSTRING, 2, 1, &rich_str(&string));
        let body = [row_hdr(0, false), st, isst, rich].concat();
        let shared = shared_strings_units(&[&units]);
        let wb = Wb { styles: Some(styles), shared: Some(shared), ..one_sheet(body) };
        assert_eq!(table(&wb), vec![vec!["t\u{FFFD}"; 3]]);
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::InvalidEncoding, 1, 3),
                in_unit(DiagnosticCode::NumberFormatUnread, 1, 3),
            ]
        );
    }

    #[test]
    fn a_styles_part_that_does_not_read_reports_once_for_its_cells() {
        let body = [row_hdr(0, false), real_cell(0, 3, 1.0)].concat();
        let wb = Wb { styles: Some(vec![0x80]), ..one_sheet(body.clone()) };
        assert_eq!(diagnose(&wb), [(DiagnosticCode::PartUnreadable, Region::Body, None, 1)]);
        assert_eq!(
            diagnose(&one_sheet(body)),
            [in_unit(DiagnosticCode::NumberFormatFallback, 1, 1)]
        );
    }

    fn hlink(r1: u32, r2: u32, c1: u32, c2: u32, fields: [&str; 4]) -> Vec<u8> {
        let mut link = [r1, r2, c1, c2].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>();
        for field in fields {
            link.extend(wstr(field));
        }
        rec(BRT_HLINK, &link)
    }

    #[test]
    fn hyperlinks_and_print_headers_are_reported() {
        let mut body = row_hdr(0, false);
        body.extend(text_cell(1, "see here"));
        body.extend(row_hdr(2, false));
        body.extend(text_cell(0, "www.example.org"));
        body.extend(hlink(0, 0, 1, 1, ["rId1", "", "", ""]));
        body.extend(hlink(2, 2, 0, 0, ["rId2", "", "", ""]));
        body.extend(hlink(0, 0, 0, 0, ["", "S!A1", "", ""]));
        let mut header_footer = 0u16.to_le_bytes().to_vec();
        header_footer.extend(wunits(&[0x58, 0xD800]));
        header_footer.extend_from_slice(&[0xFF; 4]);
        for text in ["", "", "", "&CPage &P"] {
            header_footer.extend(wstr(text));
        }
        body.extend(rec(BRT_BEGIN_HEADER_FOOTER, &header_footer));
        const LINK: &str =
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";
        let rels = format!(
            r#"<?xml version="1.0"?><Relationships xmlns="{PKG_RELS}"><Relationship Id="rId1" Type="{LINK}" Target="https://example.org/" TargetMode="External"/><Relationship Id="rId2" Type="{LINK}" Target="http://www.example.org/" TargetMode="External"/></Relationships>"#
        );
        let wb = Wb {
            extra: vec![("xl/worksheets/_rels/sheet1.bin.rels", rels.into_bytes())],
            ..one_sheet(body)
        };
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::LinkTargetUnread, 1, 1),
                in_unit(DiagnosticCode::HeaderOmitted, 1, 1),
                in_unit(DiagnosticCode::FooterOmitted, 1, 1),
            ]
        );
    }

    #[test]
    fn a_record_read_only_to_report_never_drops_its_sheet() {
        let mut body = one_value();
        body.extend(rec(BRT_HLINK, &[0u8; 6]));
        body.extend(rec(BRT_BEGIN_HEADER_FOOTER, &[0, 0, 1, 0, 0, 0, b'a', 0, 5, 0, 0, 0]));
        body.extend(rec(BRT_DRAWING, &[9, 0, 0, 0]));
        body.extend(real_cell(1, 0, 2.0));
        let parse_ctx = ParseCtx::default();
        let doc = super::super::parse(&parse_ctx, &one_sheet(body).build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["1", "2"]]);
        assert_eq!(
            reported(parse_ctx),
            [
                in_unit(DiagnosticCode::PartUnreadable, 1, 1),
                in_unit(DiagnosticCode::LinkTargetUnread, 1, 1),
                in_unit(DiagnosticCode::HeaderOmitted, 1, 1),
            ]
        );
    }

    #[test]
    fn the_drawing_and_comments_a_sheet_names_are_reported() {
        const DRAWING_REL: &str =
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing";
        const COMMENTS_REL: &str =
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";
        let rels = format!(
            r#"<?xml version="1.0"?><Relationships xmlns="{PKG_RELS}"><Relationship Id="rId1" Type="{DRAWING_REL}" Target="../drawings/drawing1.xml"/><Relationship Id="rId2" Type="{DRAWING_REL}" Target="../drawings/drawing2.xml"/><Relationship Id="rId3" Type="{COMMENTS_REL}" Target="../comments1.bin"/></Relationships>"#
        );
        let drawing = |kind: &str| {
            format!(
                r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"><xdr:twoCellAnchor><xdr:{kind}><xdr:nvPicPr><xdr:cNvPr id="1" name="x"/></xdr:nvPicPr></xdr:{kind}></xdr:twoCellAnchor></xdr:wsDr>"#
            )
            .into_bytes()
        };
        let mut body = one_value();
        body.extend(rec(BRT_DRAWING, &wstr("rId2")));
        let wb = Wb {
            extra: vec![
                ("xl/worksheets/_rels/sheet1.bin.rels", rels.into_bytes()),
                ("xl/drawings/drawing1.xml", drawing("graphicFrame")),
                ("xl/drawings/drawing2.xml", drawing("pic")),
            ],
            ..one_sheet(body)
        };
        assert_eq!(
            diagnose(&wb),
            [
                in_unit(DiagnosticCode::ImageUnplaced, 1, 1),
                in_unit(DiagnosticCode::CommentOmitted, 1, 1),
            ]
        );
    }
    #[test]
    fn empty_readable_sheets_keep_their_original_positions_after_hidden_sheets() {
        let body = [row_hdr(0, false), real_cell(0, 0, 7.0)].concat();
        let wb = Wb {
            sheets: vec![("Hidden", 1, vec![]), ("Empty", 0, vec![]), ("Shown", 0, body)],
            ..Wb::default()
        };
        let document = parse(&wb.build()).unwrap();
        assert_eq!(unit_starts(&document), [(UnitKind::Sheet, 2), (UnitKind::Sheet, 3)]);
    }
}
