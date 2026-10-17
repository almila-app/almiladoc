//! In-house SpreadsheetML reader (.xlsx / .xlsm): the workbook's visible
//! sheets, shared strings, cell number formats from `xl/styles.xml`, and
//! merge regions. Rows, columns, and sheets the source hides are omitted,
//! and merge regions are remapped onto the surviving grid. Hyperlinks,
//! print headers and footers, comments and the referenced drawing are read
//! only to report what the grid does not carry.

use super::controls::read_vml_checkboxes;
use super::grid::{
    CellFormat, CellRange, CellRef, CellXfs, DateSystem, LinkTarget, MAX_COLS, MAX_ROWS,
    OOXML_GRID, SheetContent, SheetLink, build_table, format_as_text, render_number,
    resolve_format,
};
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Block, Document, Inline};
use crate::package::archive::Role;
use crate::package::relationships::{Relationships, read_rels, rel_type, rels_part_for};
use crate::package::xml::{Element, NodeSink, ns};
use crate::package::{Package, path};
use crate::shared::text::clean_text;
use std::collections::HashMap;
use std::rc::Rc;

pub(super) const SHARED_STRINGS_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings";

const COMMENTS_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";

pub(super) fn parse(
    parse_ctx: &ParseCtx,
    pkg: &mut Package,
    wb_part: &str,
) -> Result<Document, ConvertError> {
    let workbook = pkg.required_xml_part(wb_part, Role::Content(Region::Body))?;
    // Any OOXML main part resolves here; a document or presentation would
    // otherwise convert to an empty workbook.
    if !workbook.child_elems().next().is_some_and(|e| e.is(ns::SML, "workbook")) {
        return Err(ConvertError::malformed("main part is not a workbook"));
    }
    let wb_rels = read_rels(pkg, &rels_part_for(wb_part), Role::Content(Region::Body))?;
    let dates = DateSystem::from_date1904(
        workbook
            .first_descendant(ns::SML, "workbookPr")
            .and_then(|e| e.attr_unqualified("date1904"))
            .is_some_and(bool_attr),
    );

    let shared = match Sibling::of(&wb_rels, wb_part, SHARED_STRINGS_REL, "sharedStrings.xml")
        .read_xml(pkg, Role::Reported(Region::Body))?
    {
        Some(root) => shared_strings(&root),
        None => Vec::new(),
    };
    let styles_part = Sibling::of(&wb_rels, wb_part, rel_type::STYLES, "styles.xml");
    let styles = match styles_part.read_xml(pkg, Role::Content(Region::Body))? {
        Some(root) => match root.find(ns::SML, "styleSheet") {
            Some(style_sheet) => Styles::read(style_sheet),
            None => {
                parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Body);
                Styles { xfs: CellXfs::unread(true) }
            }
        },
        None => Styles { xfs: CellXfs::unread(styles_part.reported_when_unread(pkg)) },
    };

    let mut sheets: Vec<(usize, String, String)> = Vec::new();
    for (position, sheet) in workbook
        .first_descendant(ns::SML, "sheets")
        .into_iter()
        .flat_map(|s| s.find_all(ns::SML, "sheet"))
        .enumerate()
    {
        let _unit = parse_ctx.enter_unit(position);
        if matches!(sheet.attr_unqualified("state"), Some("hidden" | "veryHidden")) {
            parse_ctx.emit(DiagnosticCode::HiddenOmitted, Region::Body);
            continue;
        }
        let name = sheet.attr_unqualified("name").unwrap_or_default().to_string();
        let target = sheet
            .attr_qualified(ns::R, "id")
            .and_then(|rid| wb_rels.internal_target(rid))
            .and_then(|target| path::resolve(wb_part, target).ok());
        match target {
            Some(t) => sheets.push((position, name, t.path)),
            None if wb_rels.reported() => {}
            None => parse_ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body),
        }
    }

    let multi_sheet = sheets.len() > 1;
    let mut doc = Document::default();
    let mut failed = 0usize;
    for (position, name, part) in &sheets {
        let _unit = parse_ctx.enter_unit(*position);
        let mut rows = SheetRows::new(parse_ctx, &shared, &styles, dates);
        let worksheet =
            pkg.related_xml_part_streamed(part, Role::Reported(Region::Body), &mut rows)?;
        let Some(worksheet) = worksheet.as_ref().and_then(|r| r.find(ns::SML, "worksheet")) else {
            parse_ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body);
            failed += 1;
            continue;
        };
        let mut content = read_sheet(parse_ctx, worksheet, rows.content);
        let drawing =
            worksheet.find(ns::SML, "drawing").and_then(|d| d.attr_qualified(ns::R, "id"));
        read_sheet_parts(parse_ctx, pkg, part, drawing, &mut content)?;
        let Some(table) = build_table(parse_ctx, content)? else {
            continue;
        };
        if multi_sheet {
            doc.blocks.push(Block::heading(2, vec![Inline::plain(name.clone())]));
        }
        doc.blocks.push(Block::Table(table));
    }
    if !sheets.is_empty() && failed == sheets.len() {
        return Err(ConvertError::malformed("no sheet in the workbook could be read"));
    }
    Ok(doc)
}

/// What a worksheet's relationships carry beside its cells: form control
/// checkboxes, the targets of its external hyperlinks, and the comments and
/// the drawing the sheet names, which no output holds and which are
/// reported.
pub(super) fn read_sheet_parts(
    parse_ctx: &ParseCtx,
    pkg: &mut Package,
    part: &str,
    drawing: Option<&str>,
    content: &mut SheetContent,
) -> Result<(), ConvertError> {
    let rels = read_rels(pkg, &rels_part_for(part), Role::Content(Region::Body))?;
    content.checkboxes = read_vml_checkboxes(parse_ctx, pkg, part, &rels)?;
    let mut targets: HashMap<String, Rc<str>> = HashMap::new();
    for link in &mut content.links {
        if let LinkTarget::Related(id) = &link.target {
            link.target = match rels.get(id) {
                Some(rel) => LinkTarget::Url(
                    targets.entry(id.clone()).or_insert_with(|| rel.target.as_str().into()).clone(),
                ),
                None if rels.reported() => LinkTarget::Reported,
                None => LinkTarget::Unknown,
            };
        }
    }
    for (_, rel) in rels.iter() {
        if rel.rel_type == COMMENTS_REL {
            parse_ctx.emit(DiagnosticCode::CommentOmitted, Region::Comment);
        }
    }
    let Some(id) = drawing else {
        return Ok(());
    };
    match rels.internal_target(id).and_then(|target| path::resolve(part, target).ok()) {
        Some(target) => {
            if let Some(root) = pkg.related_xml_part(&target.path, Role::Content(Region::Body))? {
                let Some(drawing) = root.find(ns::XDR, "wsDr") else {
                    parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Body);
                    return Ok(());
                };
                let mut losses = Vec::new();
                drawing_losses(drawing, Branch::Other, &mut losses);
                for code in losses {
                    parse_ctx.emit(code, Region::Body);
                }
            }
        }
        None if rels.reported() => {}
        None => parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Body),
    }
    Ok(())
}

/// Where a drawing walk is: inside an alternate content Choice, or not.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Branch {
    Choice,
    Other,
}

/// The pictures, charts and text-bearing shapes a sheet drawing places,
/// none of which the grid holds. Hidden objects show nothing; alternate
/// content counts its Choice, which the application shows, there leaving
/// out the form controls a legacy drawing also carries, or its Fallback
/// when the Choice holds nothing counted.
fn drawing_losses(parent: &Element, branch: Branch, out: &mut Vec<DiagnosticCode>) {
    for child in parent.child_elems() {
        if child.is(ns::MC, "AlternateContent") {
            let mut shown = Vec::new();
            if let Some(choice) = child.find(ns::MC, "Choice") {
                drawing_losses(choice, Branch::Choice, &mut shown);
            }
            if shown.is_empty()
                && let Some(fallback) = child.find(ns::MC, "Fallback")
            {
                drawing_losses(fallback, branch, &mut shown);
            }
            out.append(&mut shown);
            continue;
        }
        if child.ns.as_deref() != Some(ns::XDR) {
            continue;
        }
        let properties = child
            .child_elems()
            .find(|e| e.ns.as_deref() == Some(ns::XDR) && e.local.starts_with("nv"))
            .and_then(|nv| nv.find(ns::XDR, "cNvPr"));
        let hidden = properties.and_then(|pr| pr.attr_unqualified("hidden")).is_some_and(bool_attr);
        let compat_control = branch == Branch::Choice
            && properties.is_some_and(|pr| pr.descendants(ns::A14, "compatExt").next().is_some());
        match child.local.as_str() {
            "twoCellAnchor" | "oneCellAnchor" | "absoluteAnchor" => {
                drawing_losses(child, branch, out)
            }
            _ if hidden || compat_control => {}
            "grpSp" => drawing_losses(child, branch, out),
            "pic" => out.push(DiagnosticCode::ImageUnplaced),
            "graphicFrame" => out.push(DiagnosticCode::GraphicUnread),
            "sp" => {
                if child.outermost(ns::A, "t").any(|t| !t.text().trim().is_empty()) {
                    out.push(DiagnosticCode::GraphicUnread);
                }
                if child
                    .find(ns::XDR, "spPr")
                    .is_some_and(|pr| pr.find(ns::A, "blipFill").is_some())
                {
                    out.push(DiagnosticCode::ImageUnplaced);
                }
            }
            _ => {}
        }
    }
}

/// A workbook-level sibling part: the relationship of the given type when
/// present, else the conventional name next to the workbook part.
pub(super) struct Sibling {
    pub(super) path: String,
    related: bool,
}

impl Sibling {
    pub(super) fn of(rels: &Relationships, base: &str, rel: &str, conventional: &str) -> Self {
        let related = rels.first_of_type(rel);
        let path = related
            .and_then(|r| path::resolve(base, &r.target).ok())
            .map(|t| t.path)
            .unwrap_or_else(|| match base.rsplit_once('/') {
                Some((dir, _)) => format!("{dir}/{conventional}"),
                None => conventional.to_string(),
            });
        Sibling { path, related: related.is_some() }
    }

    pub(super) fn read(
        &self,
        pkg: &mut Package,
        role: Role,
    ) -> Result<Option<Rc<[u8]>>, ConvertError> {
        if self.related {
            pkg.related_part(&self.path, role)
        } else {
            pkg.optional_part(&self.path, role)
        }
    }

    /// Whether a read that came back empty reported its loss: the part is
    /// related, or present but unreadable.
    pub(super) fn reported_when_unread(&self, pkg: &Package) -> bool {
        self.related || pkg.has_part(&self.path)
    }

    fn read_xml(&self, pkg: &mut Package, role: Role) -> Result<Option<Element>, ConvertError> {
        if self.related {
            pkg.related_xml_part(&self.path, role)
        } else {
            pkg.optional_xml_part(&self.path, role)
        }
    }
}

/// The shared string table, one cleaned entry per `si` in order.
fn shared_strings(root: &Element) -> Vec<String> {
    let Some(sst) = root.find(ns::SML, "sst") else {
        return Vec::new();
    };
    sst.find_all(ns::SML, "si").map(|si| clean_text(&rich_text(si))).collect()
}

/// Text of an `si` or `is`: a single `t`, or rich-text `r` runs
/// concatenated. Phonetic guides (`rPh`) are not content.
fn rich_text(item: &Element) -> String {
    let mut out = String::new();
    for child in item.child_elems() {
        if child.is(ns::SML, "t") {
            out.push_str(&child.text());
        } else if child.is(ns::SML, "r")
            && let Some(t) = child.find(ns::SML, "t")
        {
            out.push_str(&t.text());
        }
    }
    out
}

/// `xl/styles.xml` reduced to what rendering needs: the ordered `cellXfs`
/// list, each entry's numFmtId resolved to a parsed format.
struct Styles {
    xfs: CellXfs,
}

impl Styles {
    fn read(root: &Element) -> Styles {
        let mut custom: HashMap<u32, &str> = HashMap::new();
        for fmts in root.descendants(ns::SML, "numFmts") {
            for nf in fmts.find_all(ns::SML, "numFmt") {
                if let (Some(id), Some(code)) = (
                    nf.attr_unqualified("numFmtId").and_then(|v| v.parse().ok()),
                    nf.attr_unqualified("formatCode"),
                ) {
                    custom.insert(id, code);
                }
            }
        }
        let mut cache: HashMap<u32, CellFormat> = HashMap::new();
        let xfs = root
            .first_descendant(ns::SML, "cellXfs")
            .map(|xfs| {
                xfs.find_all(ns::SML, "xf")
                    .map(|xf| {
                        let id = xf
                            .attr_unqualified("numFmtId")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0u32);
                        let format = cache.entry(id).or_insert_with(|| resolve_format(id, &custom));
                        (format.clone(), false)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Styles { xfs: CellXfs::new(xfs) }
    }

    /// The format for a cell's `s` attribute, an index into `cellXfs`
    /// (default 0).
    fn for_cell(&self, s: Option<&str>) -> CellFormat {
        self.xfs.get(s.map_or(Some(0), |s| s.parse::<usize>().ok()).unwrap_or(usize::MAX))
    }
}

/// A worksheet's rows read as the parse closes each one, so the part holds
/// one row's nodes at a time beside the structure around the rows, and its
/// cells, hidden or shown, are judged against the grid slots the parse has
/// left.
struct SheetRows<'s> {
    shared: &'s [String],
    styles: &'s Styles,
    dates: DateSystem,
    content: SheetContent,
    next_row: u32,
}

impl<'s> SheetRows<'s> {
    fn new(
        parse_ctx: &ParseCtx,
        shared: &'s [String],
        styles: &'s Styles,
        dates: DateSystem,
    ) -> Self {
        SheetRows {
            shared,
            styles,
            dates,
            content: SheetContent::on(
                OOXML_GRID,
                parse_ctx.grid_slots_left(),
                parse_ctx.limits().max_grid_slots,
            ),
            next_row: 0,
        }
    }

    fn read_row(&mut self, row: &Element) -> Result<(), ConvertError> {
        let r = row
            .attr_unqualified("r")
            .and_then(|v| v.parse::<u32>().ok())
            .and_then(|v| v.checked_sub(1))
            .unwrap_or(self.next_row);
        self.next_row = r.saturating_add(1);
        if row.attr_unqualified("hidden").is_some_and(bool_attr) {
            self.content.hide_row(r);
        }
        let mut next_col: u32 = 0;
        for c in row.find_all(ns::SML, "c") {
            let (text, loss) = cell_text(c, self.shared, self.styles, self.dates);
            // Position comes from the cell's own reference: a row may skip
            // cells entirely, so iteration order says nothing.
            let at = match c.attr_unqualified("r").map(parse_coords) {
                Some(Some(at)) => at,
                Some(None) => {
                    self.content.put_unplaced(CellRef::new(r, next_col), &text, loss.is_some())?;
                    continue;
                }
                None => CellRef::new(r, next_col),
            };
            next_col = at.col.saturating_add(1);
            if let Some(code) = loss {
                self.content.lose(at, code)?;
            }
            self.content.put(at, text)?;
        }
        Ok(())
    }
}

impl NodeSink for SheetRows<'_> {
    fn takes(&self, ancestors: &[Element]) -> bool {
        matches!(ancestors, [worksheet, sheet_data]
            if worksheet.is(ns::SML, "worksheet") && sheet_data.is(ns::SML, "sheetData"))
    }

    fn take(&mut self, _ancestors: &[Element], elem: Element) -> Result<(), ConvertError> {
        if elem.is(ns::SML, "row") { self.read_row(&elem) } else { Ok(()) }
    }
}

fn read_sheet(parse_ctx: &ParseCtx, worksheet: &Element, mut out: SheetContent) -> SheetContent {
    for cols in worksheet.find_all(ns::SML, "cols") {
        for col in cols.find_all(ns::SML, "col") {
            if !col.attr_unqualified("hidden").is_some_and(bool_attr) {
                continue;
            }
            let bound = |name| {
                col.attr_unqualified(name)
                    .and_then(|v| v.parse::<u32>().ok())
                    .and_then(|v| v.checked_sub(1))
            };
            if let (Some(min), Some(max)) = (bound("min"), bound("max")) {
                out.hide_cols(min, max);
            }
        }
    }
    for merge in
        worksheet.find_all(ns::SML, "mergeCells").flat_map(|mc| mc.find_all(ns::SML, "mergeCell"))
    {
        match merge.attr_unqualified("ref").and_then(|r| parse_region(r, parse_coords)) {
            Some(range) => out.merge(parse_ctx, range),
            None => parse_ctx.emit(DiagnosticCode::TableSpanRepaired, Region::Body),
        }
    }
    for link in worksheet
        .find_all(ns::SML, "hyperlinks")
        .flat_map(|links| links.find_all(ns::SML, "hyperlink"))
    {
        let range = link.attr_unqualified("ref").and_then(|r| parse_region(r, parse_ref));
        if let (Some(range), Some(id)) = (range, link.attr_qualified(ns::R, "id")) {
            out.links.push(SheetLink { range, target: LinkTarget::Related(id.to_string()) });
        }
    }
    for header_footer in worksheet.find_all(ns::SML, "headerFooter") {
        for item in header_footer.child_elems() {
            if item.text().trim().is_empty() {
                continue;
            }
            match item.local.as_str() {
                "oddHeader" | "evenHeader" | "firstHeader" => {
                    parse_ctx.emit(DiagnosticCode::HeaderOmitted, Region::Header)
                }
                "oddFooter" | "evenFooter" | "firstFooter" => {
                    parse_ctx.emit(DiagnosticCode::FooterOmitted, Region::Footer)
                }
                _ => {}
            }
        }
    }
    out
}

/// A cell's rendered text, per its `t` type and resolved number format, and
/// what rendering it lost.
fn cell_text(
    c: &Element,
    shared: &[String],
    styles: &Styles,
    dates: DateSystem,
) -> (String, Option<DiagnosticCode>) {
    let fmt = styles.for_cell(c.attr_unqualified("s"));
    let v = c.find(ns::SML, "v").map(|v| v.text());
    let value = v.as_deref().map(str::trim).unwrap_or_default();
    // A formula whose result the writer never cached: the application
    // recalculates and shows it. Excel caches an empty string result as an
    // empty `v`, so only an absent one is lost there.
    let formula = c.find(ns::SML, "f").is_some();
    let unread = || (String::new(), Some(DiagnosticCode::CellValueUnreadable));
    let text = match c.attr_unqualified("t").unwrap_or("n") {
        "s" => match value.parse::<usize>().ok().and_then(|i| shared.get(i)) {
            Some(text) => return format_as_text(&fmt, text),
            None => return unread(),
        },
        "str" if formula && v.is_none() => return unread(),
        "str" => return format_as_text(&fmt, &clean_text(v.as_deref().unwrap_or_default())),
        "inlineStr" => {
            let text = c.find(ns::SML, "is").map(|is| clean_text(&rich_text(is)));
            return format_as_text(&fmt, &text.unwrap_or_default());
        }
        "b" => match value {
            "1" | "true" => "TRUE".to_string(),
            "0" | "false" => "FALSE".to_string(),
            "" if !formula => String::new(),
            _ => return unread(),
        },
        "e" | "d" => clean_text(v.as_deref().unwrap_or_default()),
        _ if value.is_empty() && formula => return unread(),
        _ if value.is_empty() => String::new(),
        _ => match value.parse::<f64>() {
            Ok(n) => return render_number(&fmt, n, dates),
            Err(_) => return unread(),
        },
    };
    (text, None)
}

/// A cell reference (`C3`) as a zero-based cell inside the grid.
fn parse_ref(r: &str) -> Option<CellRef> {
    parse_coords(r).filter(|at| at.row < MAX_ROWS && at.col < MAX_COLS)
}

/// A cell reference as zero-based coordinates, which may lie past the grid
/// and saturate past `u32`; the column letters are bijective base-26.
fn parse_coords(r: &str) -> Option<CellRef> {
    let digits_at = r.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = r.split_at(digits_at);
    if letters.is_empty()
        || !letters.bytes().all(|b| b.is_ascii_alphabetic())
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let col = letters.bytes().fold(0u32, |col, b| {
        col.saturating_mul(26).saturating_add(u32::from(b.to_ascii_uppercase() - b'A') + 1)
    });
    let row = digits.parse::<u32>().unwrap_or(u32::MAX);
    Some(CellRef::new(row.checked_sub(1)?, col - 1))
}

/// A reference (`F1:O3`, or a single cell) as a cell range, each corner read
/// by `cell`.
fn parse_region(r: &str, cell: fn(&str) -> Option<CellRef>) -> Option<CellRange> {
    let (a, b) = r.split_once(':').unwrap_or((r, r));
    Some(CellRange::spanning(cell(a.trim())?, cell(b.trim())?))
}

/// XML schema boolean attribute.
fn bool_attr(v: &str) -> bool {
    matches!(v.trim(), "1" | "true")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::Limit;
    use crate::model::{CellSlot, Table, inlines_to_plain_text};
    use std::io::Write;

    const SML: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PKG_RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const WS_REL: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet";
    const STYLES_REL: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";

    /// Route through the shared container dispatch, the only entry a caller
    /// has.
    fn parse(bytes: &[u8]) -> Result<Document, ConvertError> {
        super::super::parse(&ParseCtx::default(), bytes)
    }

    /// Assemble a workbook: (name, state, worksheet body) per sheet, plus
    /// optional styleSheet and sst parts.
    #[derive(Default)]
    struct Wb<'a> {
        sheets: Vec<(&'a str, &'a str, &'a str)>,
        styles: Option<&'a str>,
        shared: Option<&'a str>,
        date1904: bool,
        /// Further parts, verbatim: (name, body).
        extra: Vec<(&'a str, &'a str)>,
        /// The workbook's `sheet` list in place of one entry per sheet.
        listed: Option<&'a str>,
        /// The first sheet's relationships part.
        sheet_rels: Option<&'a str>,
        /// Further workbook relationships, verbatim.
        workbook_rels: &'a str,
    }

    impl Wb<'_> {
        fn build(&self) -> Vec<u8> {
            let mut sheets = String::new();
            let mut rels = String::new();
            for (i, (name, state, _)) in self.sheets.iter().enumerate() {
                let id = i + 1;
                let state =
                    if state.is_empty() { String::new() } else { format!(" state=\"{state}\"") };
                sheets.push_str(&format!(
                    r#"<sheet name="{name}" sheetId="{id}"{state} r:id="rId{id}"/>"#
                ));
                rels.push_str(&format!(
                    r#"<Relationship Id="rId{id}" Type="{WS_REL}" Target="worksheets/sheet{id}.xml"/>"#
                ));
            }
            if self.styles.is_some() {
                rels.push_str(&format!(
                    r#"<Relationship Id="rId90" Type="{STYLES_REL}" Target="styles.xml"/>"#
                ));
            }
            if self.shared.is_some() {
                rels.push_str(&format!(
                    r#"<Relationship Id="rId91" Type="{SHARED_STRINGS_REL}" Target="sharedStrings.xml"/>"#
                ));
            }
            let pr = if self.date1904 { r#"<workbookPr date1904="1"/>"# } else { "" };
            let sheets = self.listed.unwrap_or(&sheets);
            let workbook = format!(
                r#"<?xml version="1.0"?><workbook xmlns="{SML}" xmlns:r="{R}">{pr}<sheets>{sheets}</sheets></workbook>"#
            );
            let rels = format!(
                r#"<?xml version="1.0"?><Relationships xmlns="{PKG_RELS}">{rels}{}</Relationships>"#,
                self.workbook_rels
            );
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let opts = zip::write::SimpleFileOptions::default();
            let mut add = |name: &str, body: &str| {
                zip.start_file(name, opts).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            };
            add("xl/workbook.xml", &workbook);
            add("xl/_rels/workbook.xml.rels", &rels);
            for (i, (_, _, body)) in self.sheets.iter().enumerate() {
                add(
                    &format!("xl/worksheets/sheet{}.xml", i + 1),
                    &format!(r#"<?xml version="1.0"?><worksheet xmlns="{SML}">{body}</worksheet>"#),
                );
            }
            if let Some(styles) = self.styles {
                add(
                    "xl/styles.xml",
                    &format!(
                        r#"<?xml version="1.0"?><styleSheet xmlns="{SML}">{styles}</styleSheet>"#
                    ),
                );
            }
            if let Some(shared) = self.shared {
                add(
                    "xl/sharedStrings.xml",
                    &format!(r#"<?xml version="1.0"?><sst xmlns="{SML}">{shared}</sst>"#),
                );
            }
            if let Some(body) = self.sheet_rels {
                add(
                    "xl/worksheets/_rels/sheet1.xml.rels",
                    &format!(
                        r#"<?xml version="1.0"?><Relationships xmlns="{PKG_RELS}">{body}</Relationships>"#
                    ),
                );
            }
            for (name, body) in &self.extra {
                add(name, body);
            }
            zip.finish().unwrap().into_inner()
        }
    }

    fn one_sheet(body: &str) -> Wb<'_> {
        Wb { sheets: vec![("S", "", body)], ..Wb::default() }
    }

    fn first_table(doc: &Document) -> &Table {
        match doc.blocks.iter().find_map(|b| match b {
            Block::Table(t) => Some(t),
            _ => None,
        }) {
            Some(t) => t,
            None => panic!("expected a table, got {:?}", doc.blocks),
        }
    }

    fn texts(table: &Table) -> Vec<Vec<String>> {
        table
            .grid
            .iter()
            .map(|row| {
                row.iter()
                    .map(|slot| match slot {
                        CellSlot::Origin(cell) => cell
                            .blocks
                            .iter()
                            .filter_map(|b| match b {
                                Block::Paragraph(i) => Some(inlines_to_plain_text(i)),
                                _ => None,
                            })
                            .collect(),
                        CellSlot::Covered { .. } => "<covered>".to_string(),
                    })
                    .collect()
            })
            .collect()
    }

    fn covered_count(table: &Table) -> usize {
        table.grid.iter().flatten().filter(|s| matches!(s, CellSlot::Covered { .. })).count()
    }

    #[test]
    fn number_formats_apply_to_stored_values() {
        // The issue #27 case: a percent renders as its display value, not
        // its stored fraction; currency keeps its symbol and grouping; a
        // date format keeps the unambiguous ISO rendering.
        let wb = Wb {
            styles: Some(
                r#"<numFmts><numFmt numFmtId="164" formatCode="0.0%"/><numFmt numFmtId="165" formatCode="&quot;$&quot;#,##0.00"/><numFmt numFmtId="166" formatCode="mm/dd/yyyy"/></numFmts><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="165"/><xf numFmtId="166"/></cellXfs>"#,
            ),
            ..one_sheet(
                r#"<sheetData><row r="1"><c r="A1" s="1"><v>0.075</v></c><c r="B1" s="2"><v>1234.5</v></c><c r="C1" s="3"><v>46096</v></c></row></sheetData>"#,
            )
        };
        let doc = parse(&wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["7.5%", "$1,234.50", "2026-03-15"]]);
    }

    #[test]
    fn unresolvable_numfmt_ids_render_general() {
        // Id 5 is not an implied built-in and id 30 is locale-specific:
        // with no numFmt element the code is unknown, and guessing (a
        // currency format, a date shape) would be worse than General.
        let wb = Wb {
            styles: Some(r#"<cellXfs><xf numFmtId="5"/><xf numFmtId="30"/></cellXfs>"#),
            ..one_sheet(
                r#"<sheetData><row r="1"><c r="A1" s="0"><v>1234.5</v></c><c r="B1" s="1"><v>1234.5</v></c></row></sheetData>"#,
            )
        };
        let doc = parse(&wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["1234.5", "1234.5"]]);
    }

    #[test]
    fn value_types_render_by_their_t_attribute() {
        let wb = one_sheet(
            r#"<sheetData><row r="1"><c r="A1" t="b"><v>1</v></c><c r="B1" t="e"><v>#DIV/0!</v></c><c r="C1" t="str"><v>=sum</v></c><c r="D1" t="d"><v>2026-03-15</v></c><c r="E1" t="inlineStr"><is><r><t>in</t></r><r><t>line</t></r></is></c></row></sheetData>"#,
        );
        let doc = parse(&wb.build()).unwrap();
        assert_eq!(
            texts(first_table(&doc)),
            vec![vec!["TRUE", "#DIV/0!", "=sum", "2026-03-15", "inline"]]
        );
    }

    #[test]
    fn shared_strings_resolve_including_rich_text_runs() {
        let wb = Wb {
            shared: Some(
                r#"<si><t>plain</t></si><si><r><t>ri</t></r><r><t>ch</t></r><rPh sb="0" eb="1"><t>ignored</t></rPh></si>"#,
            ),
            ..one_sheet(
                r#"<sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row></sheetData>"#,
            )
        };
        let doc = parse(&wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["plain", "rich"]]);
    }

    #[test]
    fn date1904_serials_shift_epoch() {
        let wb = Wb {
            styles: Some(
                r#"<numFmts><numFmt numFmtId="164" formatCode="yyyy-mm-dd"/></numFmts><cellXfs><xf numFmtId="164"/></cellXfs>"#,
            ),
            date1904: true,
            ..one_sheet(r#"<sheetData><row r="1"><c r="A1" s="0"><v>100</v></c></row></sheetData>"#)
        };
        let doc = parse(&wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["1904-04-10"]]);
    }

    #[test]
    fn a_merge_past_the_content_is_clamped_to_it() {
        let wb = one_sheet(
            r#"<sheetData><row r="1"><c r="F1" t="inlineStr"><is><t>wide</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="F1:O3"/></mergeCells>"#,
        );
        let doc = parse(&wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["wide"]]);
        assert_eq!(diagnose(&wb), []);
    }

    /// Minimal sheet with a used range at D11:E12 and the given merge.
    fn sheet_with_merge(merge_ref: &str) -> Vec<u8> {
        one_sheet(&format!(
            r#"<sheetData><row r="11"><c r="D11" t="inlineStr"><is><t>x</t></is></c><c r="E11" t="inlineStr"><is><t>y</t></is></c></row><row r="12"><c r="D12" t="inlineStr"><is><t>z</t></is></c><c r="E12" t="inlineStr"><is><t>w</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="{merge_ref}"/></mergeCells>"#
        ))
        .build()
    }

    #[test]
    fn merge_inside_the_used_range_covers_cells() {
        let doc = parse(&sheet_with_merge("D11:E11")).unwrap();
        assert_eq!(covered_count(first_table(&doc)), 1);
    }

    #[test]
    fn merge_outside_the_used_columns_is_ignored() {
        // The merge overlaps the used rows but not the used columns; it
        // must neither cover cells nor drag the grid out to column A.
        let doc = parse(&sheet_with_merge("A1:B12")).unwrap();
        let table = first_table(&doc);
        assert_eq!(covered_count(table), 0, "out-of-range merge must not cover cells");
        assert_eq!(table.grid[0].len(), 2);
    }

    #[test]
    fn a_merge_past_the_grid_is_clamped_and_one_outside_it_dropped() {
        let wb = one_sheet(
            r#"<sheetData><row r="1"><c r="XFC1" t="inlineStr"><is><t>edge</t></is></c></row></sheetData><mergeCells><mergeCell ref="XFC1:XFZ2"/><mergeCell ref="XFE3:XFF4"/><mergeCell ref="A1048577:B99999999999"/></mergeCells>"#,
        );
        let doc = parse(&wb.build()).unwrap();
        let table = first_table(&doc);
        let CellSlot::Origin(cell) = &table.grid[0][0] else {
            panic!("expected the merge origin at (0,0)");
        };
        assert_eq!((table.grid.len(), cell.col_span, cell.row_span), (1, 1, 1));
        assert_eq!(diagnose(&wb), [(DiagnosticCode::TableSpanRepaired, Region::Body, Some(1), 3)]);
    }

    #[test]
    fn only_the_rows_of_the_worksheets_own_sheet_data_are_read() {
        let wb = one_sheet(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c></row><other r="3"><c r="A3"><v>3</v></c></other></sheetData><extLst><ext><sheetData><row r="2"><c r="A2"><v>2</v></c></row></sheetData></ext></extLst>"#,
        );
        assert_eq!(texts(first_table(&parse(&wb.build()).unwrap())), vec![vec!["1"]]);
    }

    #[test]
    fn rows_read_before_a_worksheet_fails_to_parse_render_nothing() {
        let wb = Wb {
            sheets: vec![
                ("Broken", "", r#"<sheetData><row r="1"><c r="A1"><v>1</v></c></row><x<y/>"#),
                ("Kept", "", r#"<sheetData><row r="1"><c r="A1"><v>2</v></c></row></sheetData>"#),
            ],
            ..Wb::default()
        };
        let doc = parse(&wb.build()).unwrap();
        let tables: Vec<_> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Table(t) => Some(texts(t)),
                _ => None,
            })
            .collect();
        assert_eq!(tables, [vec![vec!["2"]]]);
        assert_eq!(diagnose(&wb), [(DiagnosticCode::UnitUnreadable, BODY, Some(1), 1)]);
    }

    #[test]
    fn a_cell_past_the_grid_or_off_it_is_unreadable_only_where_its_text_shows() {
        let wb = one_sheet(
            r#"<sheetData><row r="1048577"><c><v>1</v></c><c s="0"/></row><row r="1"><c r="1B"><v>4</v></c><c r="2C" s="0"/><c r="A1"><v>2</v></c><c r="XFE1"><v>3</v></c><c r="XFF1" t="inlineStr"><is><t> </t></is></c><c><v>5</v></c><c r="1D"><f>A1</f></c><c r="1E" t="s"><v>99</v></c><c r="XFG1" t="s"><v>99</v></c></row><row r="2" hidden="1"><c r="XFE2"><v>6</v></c><c r="XFG2" t="s"><v>99</v></c></row></sheetData>"#,
        );
        assert_eq!(texts(first_table(&parse(&wb.build()).unwrap())), vec![vec!["2"]]);
        assert_eq!(
            diagnose(&wb),
            [(CELL, BODY, Some(1), 7), (DiagnosticCode::HiddenOmitted, BODY, Some(1), 1)]
        );
    }

    #[test]
    fn a_row_past_the_grid_places_its_cells_by_their_own_reference() {
        let wb = one_sheet(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="1048577"><c r="B1"><v>2</v></c><c><v>3</v></c></row></sheetData>"#,
        );
        assert_eq!(texts(first_table(&parse(&wb.build()).unwrap())), vec![vec!["1", "2"]]);
        assert_eq!(diagnose(&wb), [(CELL, BODY, Some(1), 1)]);
    }

    #[test]
    fn cells_hidden_or_shown_are_judged_against_the_grid_slots_left_as_the_rows_stream() {
        let limits = crate::limits::Limits { max_grid_slots: 2, ..Default::default() };
        let run = |sheets: Vec<(&str, &str, &str)>| {
            let wb = Wb { sheets, ..Wb::default() };
            super::super::parse(&ParseCtx::new(&limits), &wb.build()).map(|doc| doc.blocks.len())
        };
        let fits = r#"<sheetData><row r="1"><c r="A1"><v>1</v></c><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c><c r="C1" t="inlineStr"><is><t> </t></is></c></row></sheetData>"#;
        let broken =
            |row: &str, cells: &str| format!(r#"<sheetData><row r="1"{row}>{cells}</row><x<y/>"#);
        let past = |err: Result<usize, ConvertError>| {
            matches!(err, Err(ConvertError::ResourceLimit { limit: Limit::GridSlots, max: 2 }))
        };
        assert_eq!(run(vec![("A", "", fits)]).unwrap(), 1);
        let three = r#"<c r="A1"><v>1</v></c><c r="B1"><v>2</v></c><c r="C1"><v>3</v></c>"#;
        assert!(past(run(vec![("A", "", &broken("", three))])));
        let hidden_cols = r#"<cols><col min="1" max="16384" hidden="1"/></cols>"#;
        assert!(past(run(vec![("A", "", &format!("{hidden_cols}{}", broken("", three)))])));
        assert!(past(run(vec![("A", "", &broken(r#" hidden="1""#, three))])));
        let one = broken("", r#"<c r="A1"><v>1</v></c>"#);
        assert!(past(run(vec![("A", "", fits), ("B", "", &one)])));
        let unread = |at: &str| format!(r#"<c r="{at}" t="s"><v>9</v></c>"#);
        let unplaced = |at: &str| format!(r#"<c r="1{at}"><v>1</v></c>"#);
        for cell in [unread, unplaced] {
            let held = |cols: &[&str]| cols.iter().map(|at| cell(at)).collect::<String>();
            let two = format!("<sheetData><row r=\"1\">{}</row></sheetData>", held(&["A1", "B1"]));
            assert_eq!(run(vec![("A", "", &two)]).unwrap(), 0);
            assert!(past(run(vec![("A", "", &broken("", &held(&["A1", "B1", "C1"])))])));
        }
    }

    #[test]
    fn a_hidden_row_past_the_grid_reports_its_content_as_hidden_only() {
        let wb = one_sheet(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="1048577" hidden="1"><c><v>2</v></c></row></sheetData>"#,
        );
        assert_eq!(texts(first_table(&parse(&wb.build()).unwrap())), vec![vec!["1"]]);
        assert_eq!(diagnose(&wb), [(DiagnosticCode::HiddenOmitted, BODY, Some(1), 1)]);
    }

    #[test]
    fn a_hidden_column_past_the_grid_reports_its_content_as_hidden_only() {
        let wb = one_sheet(
            r#"<cols><col min="16385" max="16390" hidden="1"/></cols><sheetData><row r="1"><c r="A1"><v>1</v></c><c r="XFE1"><v>2</v></c></row></sheetData>"#,
        );
        assert_eq!(texts(first_table(&parse(&wb.build()).unwrap())), vec![vec!["1"]]);
        assert_eq!(diagnose(&wb), [(DiagnosticCode::HiddenOmitted, BODY, Some(1), 1)]);
    }

    #[test]
    fn a_merge_moved_off_a_hidden_origin_replaces_the_covered_text_it_lands_on() {
        let cell =
            |r: &str, t: &str| format!(r#"<c r="{r}" t="inlineStr"><is><t>{t}</t></is></c>"#);
        let run = |cols: &str, rows: &str, merge: &str| {
            let body = format!(
                r#"{cols}<sheetData>{rows}</sheetData><mergeCells count="1"><mergeCell ref="{merge}"/></mergeCells>"#
            );
            texts(first_table(&parse(&one_sheet(&body).build()).unwrap()))
        };
        let below = format!(
            r#"<row r="1" hidden="1"/><row r="2">{}{}</row>"#,
            cell("A2", "covered"),
            cell("B2", "y")
        );
        assert_eq!(run("", &below, "A1:A3"), vec![vec!["y"]]);
        let hidden_col = r#"<cols><col min="1" max="1" hidden="1"/></cols>"#;
        let beside = format!(r#"<row r="1">{}{}</row>"#, cell("B1", "covered"), cell("C1", "y"));
        assert_eq!(run(hidden_col, &beside, "A1:B1"), vec![vec!["y"]]);
        let kept = format!(
            r#"<row r="1" hidden="1">{}</row><row r="2">{}{}</row>"#,
            cell("A1", "kept"),
            cell("A2", "covered"),
            cell("B2", "y")
        );
        assert_eq!(run("", &kept, "A1:A3"), vec![vec!["kept", "y"]]);
    }

    #[test]
    fn hidden_column_ranges_that_touch_or_nest_hide_as_one() {
        let wb = one_sheet(
            r#"<cols><col min="2" max="2" hidden="1"/><col min="1" max="1" hidden="1"/><col min="5" max="7" hidden="1"/><col min="6" max="6" hidden="1"/></cols><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>origin</t></is></c><c r="C1" t="inlineStr"><is><t>covered</t></is></c><c r="D1" t="inlineStr"><is><t>d</t></is></c><c r="G1" t="inlineStr"><is><t>hidden</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:C1"/></mergeCells>"#,
        );
        assert_eq!(texts(first_table(&parse(&wb.build()).unwrap())), vec![vec!["origin", "d"]]);
    }

    #[test]
    fn a_cell_a_merge_clamped_to_the_content_covers_shows_nothing() {
        let cell =
            |r: &str, t: &str| format!(r#"<c r="{r}" t="inlineStr"><is><t>{t}</t></is></c>"#);
        let sheet = |row: u32, cells: &[(&str, &str)], merge: &str| {
            let cells: String = cells.iter().map(|&(r, t)| cell(r, t)).collect();
            format!(
                r#"<sheetData><row r="{row}">{cells}</row></sheetData><mergeCells count="1"><mergeCell ref="{merge}"/></mergeCells>"#
            )
        };
        let body = sheet(2, &[("A2", "a"), ("B2", "hidden"), ("C2", "c")], "B1:B2");
        let wb = one_sheet(&body);
        assert_eq!(texts(first_table(&parse(&wb.build()).unwrap())), vec![vec!["a", "", "c"]]);
        assert_eq!(diagnose(&wb), []);
        let body = sheet(3, &[("A3", "a"), ("B3", "hidden"), ("D3", "d")], "B2:C3");
        let doc = parse(&one_sheet(&body).build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["a", "", "<covered>", "d"]]);
    }

    #[test]
    fn a_reference_rejects_row_zero_and_stray_digits_and_saturates_past_u32() {
        assert_eq!(parse_coords("A0"), None);
        assert_eq!(parse_coords("B2x"), None);
        assert_eq!(parse_coords("XFE1"), Some(CellRef::new(0, MAX_COLS)));
        assert_eq!(parse_ref("XFE1"), None);
        assert_eq!(parse_coords("A99999999999"), Some(CellRef::new(u32::MAX - 1, 0)));
        assert_eq!(parse_coords("ZZZZZZZZ1"), Some(CellRef::new(0, u32::MAX - 1)));
    }

    #[test]
    fn a_merge_down_past_u32_rows_is_clamped_to_the_grid() {
        let wb = one_sheet(
            r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row></sheetData><mergeCells><mergeCell ref="A1:A99999999999"/></mergeCells>"#,
        );
        let doc = parse(&wb.build()).unwrap();
        let table = first_table(&doc);
        let CellSlot::Origin(cell) = &table.grid[0][0] else {
            panic!("expected the merge origin at (0,0)");
        };
        assert_eq!((table.grid.len(), cell.row_span), (1, 1));
        assert_eq!(diagnose(&wb), [(DiagnosticCode::TableSpanRepaired, Region::Body, Some(1), 1)]);
    }

    #[test]
    fn hidden_rows_columns_and_sheets_are_omitted() {
        // Hidden content is invisible to someone opening the workbook, so
        // passing it on would make it look authoritative. One visible sheet
        // remains, so no sheet heading is emitted either.
        let visible = r#"<cols><col min="2" max="2" hidden="1"/></cols><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c><c r="B1" t="inlineStr"><is><t>hidden col</t></is></c><c r="C1" t="inlineStr"><is><t>c</t></is></c></row><row r="2" hidden="1"><c r="A2" t="inlineStr"><is><t>hidden row</t></is></c></row><row r="3"><c r="A3" t="inlineStr"><is><t>d</t></is></c></row></sheetData>"#;
        let secret = r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>secret</t></is></c></row></sheetData>"#;
        let wb = Wb {
            sheets: vec![("Shown", "", visible), ("Secret", "hidden", secret)],
            ..Wb::default()
        };
        let doc = parse(&wb.build()).unwrap();
        assert_eq!(doc.blocks.len(), 1, "hidden sheet must add no heading and no table");
        assert_eq!(texts(first_table(&doc)), vec![vec!["a", "c"], vec!["d", ""]]);
    }

    #[test]
    fn merges_remap_across_hidden_columns() {
        // Dropping a hidden column renumbers the grid: a merge spanning it
        // comes out one column narrower, not applied at stale indices.
        let wb = one_sheet(
            r#"<cols><col min="2" max="2" hidden="1"/></cols><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>m</t></is></c><c r="D1" t="inlineStr"><is><t>x</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:C1"/></mergeCells>"#,
        );
        let doc = parse(&wb.build()).unwrap();
        let table = first_table(&doc);
        let CellSlot::Origin(cell) = &table.grid[0][0] else {
            panic!("expected the merge origin at (0,0)");
        };
        assert_eq!((cell.col_span, cell.row_span), (2, 1));
        assert_eq!(table.grid[0].len(), 3);
    }

    #[test]
    fn merge_origin_in_a_hidden_row_keeps_its_content() {
        // The origin row is hidden but the merge survives: its value moves
        // to the first surviving position it covers instead of being lost.
        let wb = one_sheet(
            r#"<sheetData><row r="1" hidden="1"><c r="A1" t="inlineStr"><is><t>kept</t></is></c></row><row r="2"/><row r="3"><c r="B3" t="inlineStr"><is><t>x</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:A3"/></mergeCells>"#,
        );
        let doc = parse(&wb.build()).unwrap();
        let table = first_table(&doc);
        let CellSlot::Origin(cell) = &table.grid[0][0] else {
            panic!("expected the merge origin at (0,0)");
        };
        assert_eq!(cell.row_span, 2);
        assert_eq!(texts(table)[0][0], "kept");
    }

    #[test]
    fn the_grid_budget_spans_the_whole_workbook() {
        // Two cells at opposite corners describe the whole sheet, so the
        // extent has to be charged before any position is built.
        let wb = one_sheet(
            r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row><row r="1048576"><c r="XFD1048576" t="inlineStr"><is><t>b</t></is></c></row></sheetData>"#,
        );
        let err = parse(&wb.build()).unwrap_err();
        assert!(
            matches!(err, ConvertError::ResourceLimit { limit: Limit::GridSlots, .. }),
            "got {err:?}"
        );

        // Sheets accumulate, so a pile of them cannot each sit under the cap.
        let half = r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row><row r="150000"><c r="P150000" t="inlineStr"><is><t>b</t></is></c></row></sheetData>"#;
        let wb = Wb { sheets: vec![("A", "", half), ("B", "", half)], ..Wb::default() };
        let err = parse(&wb.build()).unwrap_err();
        assert!(
            matches!(err, ConvertError::ResourceLimit { limit: Limit::GridSlots, .. }),
            "got {err:?}"
        );
    }

    const VML_REL: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/vmlDrawing";

    /// A legacy drawing with a checked captioned box over B1, an unchecked
    /// bare one over C1, a hidden one over D1, and a cell note.
    const VML: &str = r##"<xml xmlns:v="urn:schemas-microsoft-com:vml" xmlns:o="urn:schemas-microsoft-com:office:office" xmlns:x="urn:schemas-microsoft-com:office:excel">
        <v:shape id="_x0000_s1025" type="#_x0000_t201" style="position:absolute;margin-left:1pt">
          <v:textbox>
            <div style="text-align:left"><font face="Tahoma">Roof</font></div>
          </v:textbox>
          <x:ClientData ObjectType="Checkbox"><x:Anchor>1, 5, 0, 2, 2, 10, 1, 1</x:Anchor><x:Checked>1</x:Checked></x:ClientData>
        </v:shape>
        <v:shape id="_x0000_s1026" type="#_x0000_t201" style="position:absolute">
          <v:textbox><div><font></font></div></v:textbox>
          <x:ClientData ObjectType="Checkbox"><x:Anchor>2, 5, 0, 2, 3, 10, 1, 1</x:Anchor></x:ClientData>
        </v:shape>
        <v:shape id="_x0000_s1027" type="#_x0000_t201" style="position:absolute;visibility:hidden">
          <x:ClientData ObjectType="Checkbox"><x:Anchor>3, 5, 0, 2, 4, 10, 1, 1</x:Anchor><x:Checked>1</x:Checked></x:ClientData>
        </v:shape>
        <v:shape id="_x0000_s1028" type="#_x0000_t202" style="position:absolute">
          <v:textbox><div>a note</div></v:textbox>
          <x:ClientData ObjectType="Note"><x:Anchor>4, 5, 0, 2, 5, 10, 1, 1</x:Anchor></x:ClientData>
        </v:shape>
        </xml>"##;

    #[test]
    fn form_control_checkboxes_land_in_their_anchor_cell() {
        let rels = format!(
            r#"<?xml version="1.0"?><Relationships xmlns="{PKG_RELS}"><Relationship Id="rId1" Type="{VML_REL}" Target="../drawings/vmlDrawing1.vml"/></Relationships>"#
        );
        let wb = Wb {
            sheets: vec![(
                "S",
                "",
                r#"<sheetData><row r="1"><c r="A1" t="str"><v>14</v></c><c r="B1" t="str"><v>L/R</v></c></row></sheetData><legacyDrawing r:id="rId1"/>"#,
            )],
            extra: vec![
                ("xl/worksheets/_rels/sheet1.xml.rels", &rels),
                ("xl/drawings/vmlDrawing1.vml", VML),
            ],
            ..Wb::default()
        };
        let doc = parse(&wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), vec![vec!["14", "L/R [x] Roof", "[ ]"]]);
    }

    #[test]
    fn merges_are_charged_against_the_contexts_expansion_before_placement() {
        let bytes = one_sheet(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="3"><c r="C3"><v>3</v></c></row><row r="5"><c r="E5"><v>5</v></c></row></sheetData><mergeCells count="2"><mergeCell ref="A1:B2"/><mergeCell ref="C3:D4"/></mergeCells>"#,
        )
        .build();
        let limits = crate::limits::Limits { max_expansion: 6, ..Default::default() };
        assert!(super::super::parse(&ParseCtx::new(&limits), &bytes).is_ok());
        let limits = crate::limits::Limits { max_expansion: 5, ..Default::default() };
        let err = super::super::parse(&ParseCtx::new(&limits), &bytes).unwrap_err();
        assert!(matches!(err, ConvertError::ResourceLimit { limit: Limit::Expansion, max: 5 }));
    }

    type Reported = (DiagnosticCode, Region, Option<u32>, u64);

    fn diagnose(wb: &Wb) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        super::super::parse(&parse_ctx, &wb.build()).unwrap();
        let unit = |d: &crate::diagnostics::Diagnostic| d.unit.map(std::num::NonZeroU32::get);
        parse_ctx.diagnostics().iter().map(|d| (d.code, d.region, unit(&d), d.count)).collect()
    }

    const CELL: DiagnosticCode = DiagnosticCode::CellValueUnreadable;
    const UNREAD: DiagnosticCode = DiagnosticCode::NumberFormatUnread;
    const FALLBACK: DiagnosticCode = DiagnosticCode::NumberFormatFallback;
    const BODY: Region = Region::Body;

    fn number_cells(styles: &[u32]) -> String {
        let cells: String = styles
            .iter()
            .enumerate()
            .map(|(i, s)| format!(r#"<c r="{}1" s="{s}"><v>2</v></c>"#, char::from(b'A' + i as u8)))
            .collect();
        format!(
            r#"<sheetData><row r="1">{cells}<c r="Z1" s="1" t="inlineStr"><is><t>x</t></is></c></row></sheetData>"#
        )
    }

    #[test]
    fn each_sheet_site_reports_its_loss_in_its_sheets_unit() {
        let one = r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row></sheetData>"#;
        let listed = r#"<sheet name="A" sheetId="1" r:id="rId1"/><sheet name="H" sheetId="2" state="hidden" r:id="rId2"/><sheet name="N" sheetId="3"/><sheet name="X" sheetId="4" r:id="rId9"/><sheet name="M" sheetId="5" r:id="rId3"/><sheet name="E" sheetId="6" r:id="rId4"/>"#;
        let missing = format!(
            r#"<Relationship Id="rId3" Type="{WS_REL}" Target="worksheets/gone.xml"/><Relationship Id="rId4" Type="{WS_REL}" Target="worksheets/a%2Fb.xml"/>"#
        );
        let wb = Wb {
            sheets: vec![("A", "", one), ("H", "", one)],
            listed: Some(listed),
            workbook_rels: &missing,
            ..Wb::default()
        };
        let bytes = wb.build();
        let parse_ctx = ParseCtx::default();
        super::super::parse(&parse_ctx, &bytes).unwrap();
        let reported: Vec<_> = parse_ctx
            .diagnostics()
            .iter()
            .map(|d| (d.code, d.unit.map(std::num::NonZeroU32::get), d.count))
            .collect();
        assert_eq!(
            reported,
            [
                (DiagnosticCode::UnitUnreadable, Some(3), 1),
                (DiagnosticCode::UnitUnreadable, Some(4), 1),
                (DiagnosticCode::UnitUnreadable, Some(5), 1),
                (DiagnosticCode::UnitUnreadable, Some(6), 1),
                (DiagnosticCode::HiddenOmitted, Some(2), 1),
            ]
        );
    }

    #[test]
    fn a_number_format_that_does_not_apply_reports_per_numeric_cell() {
        let styles = r#"<numFmts><numFmt numFmtId="164" formatCode="[DBNum1]yyyy"/><numFmt numFmtId="165" formatCode="0.0.0"/><numFmt numFmtId="166" formatCode="€0.00"/><numFmt numFmtId="167" formatCode="[DBNum1][h]"/></numFmts><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="165"/><xf numFmtId="166"/><xf numFmtId="5"/><xf numFmtId="30"/><xf numFmtId="41"/><xf numFmtId="23"/><xf numFmtId="167"/></cellXfs>"#;
        let body = number_cells(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let wb = Wb { styles: Some(styles), ..one_sheet(&body) };
        assert_eq!(diagnose(&wb), [(UNREAD, BODY, Some(1), 5), (FALLBACK, BODY, Some(1), 4)]);
        let body = number_cells(&[0, 3]);
        let unstyled =
            Wb { styles: Some(r#"<cellXfs><xf numFmtId="0"/></cellXfs>"#), ..one_sheet(&body) };
        assert_eq!(diagnose(&unstyled), [(FALLBACK, BODY, Some(1), 1)]);
    }

    #[test]
    fn a_format_code_past_the_binary_cap_falls_back_by_what_it_shows() {
        let styles = format!(
            r#"<numFmts><numFmt numFmtId="164" formatCode="{}"/><numFmt numFmtId="165" formatCode="0&quot;{}&quot;"/></numFmts><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="165"/></cellXfs>"#,
            "0".repeat(256),
            "x".repeat(253),
        );
        let body = number_cells(&[1, 2]);
        let wb = Wb { styles: Some(&styles), ..one_sheet(&body) };
        let doc = super::super::parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc))[0][..2], ["2", "2"]);
        assert_eq!(diagnose(&wb), [(UNREAD, BODY, Some(1), 1), (FALLBACK, BODY, Some(1), 1)]);
    }

    #[test]
    fn a_number_under_a_code_past_the_binary_cap_is_judged_by_its_numeric_sections() {
        let styles = format!(
            r#"<numFmts><numFmt numFmtId="164" formatCode="0&quot;{x}&quot;;0;0;@"/><numFmt numFmtId="165" formatCode="0;0;0;&quot;{x}&quot;@"/><numFmt numFmtId="166" formatCode="{};&quot;a&quot;@"/></numFmts><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="165"/><xf numFmtId="166"/></cellXfs>"#,
            "0".repeat(300),
            x = "x".repeat(260),
        );
        let body = number_cells(&[1, 2, 3]);
        let wb = Wb { styles: Some(&styles), ..one_sheet(&body) };
        let doc = super::super::parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc))[0][..3], ["2", "2", "2"]);
        assert_eq!(diagnose(&wb), [(UNREAD, BODY, Some(1), 1), (FALLBACK, BODY, Some(1), 2)]);
    }

    #[test]
    fn a_text_section_past_the_binary_cap_reports_only_what_it_decorates() {
        let styles = format!(
            r#"<numFmts><numFmt numFmtId="164" formatCode="0;0;0;&quot;{}&quot;@"/><numFmt numFmtId="165" formatCode="{};@"/></numFmts><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="165"/></cellXfs>"#,
            "x".repeat(247),
            "0".repeat(254),
        );
        let cell =
            |r: &str, s: u8| format!(r#"<c r="{r}" s="{s}" t="inlineStr"><is><t>t</t></is></c>"#);
        let body = format!(
            r#"<sheetData><row r="1">{}{}<c r="C1" s="1" t="s"><v>0</v></c><c r="D1" s="1" t="str"><f>A2</f><v>t</v></c></row></sheetData>"#,
            cell("A1", 1),
            cell("B1", 2)
        );
        let wb =
            Wb { styles: Some(&styles), shared: Some("<si><t>t</t></si>"), ..one_sheet(&body) };
        let doc = super::super::parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), [["t", "t", "t", "t"]]);
        assert_eq!(diagnose(&wb), [(UNREAD, BODY, Some(1), 3)]);
    }

    #[test]
    fn a_mebibyte_format_code_renders_its_value_as_general() {
        let styles = format!(
            r#"<numFmts><numFmt numFmtId="164" formatCode="{}"/></numFmts><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/></cellXfs>"#,
            "0".repeat(1 << 20),
        );
        let body = r#"<sheetData><row r="1"><c r="A1" s="1"><v>2.5</v></c></row></sheetData>"#;
        let wb = Wb { styles: Some(&styles), ..one_sheet(body) };
        let doc = super::super::parse(&ParseCtx::default(), &wb.build()).unwrap();
        assert_eq!(texts(first_table(&doc)), [["2.5"]]);
        assert_eq!(diagnose(&wb), [(FALLBACK, BODY, Some(1), 1)]);
    }

    #[test]
    fn a_cell_whose_value_does_not_read_reports_it() {
        for (cell, expected) in [
            (r#"<c r="A1" t="s"><v>7</v></c>"#, &[(CELL, BODY, Some(1), 1)][..]),
            (r#"<c r="A1"><v>twelve</v></c>"#, &[(CELL, BODY, Some(1), 1)]),
            (r#"<c r="A1" t="str"><f>A2</f></c>"#, &[(CELL, BODY, Some(1), 1)]),
            (r#"<c r="A1" t="str"><f>A2</f><v></v></c>"#, &[]),
            (r#"<c r="A1"><f>A2</f><v/></c>"#, &[(CELL, BODY, Some(1), 1)]),
            (r#"<c r="A1"><v/></c>"#, &[]),
            (r#"<c r="A1" t="b"><f>A2</f></c>"#, &[(CELL, BODY, Some(1), 1)]),
            (r#"<c r="A1" t="b"><v>2</v></c>"#, &[(CELL, BODY, Some(1), 1)]),
            (r#"<c r="1A"><v>1</v></c>"#, &[(CELL, BODY, Some(1), 1)]),
        ] {
            let body = format!(
                r#"<sheetData><row r="1">{cell}<c r="B1" t="inlineStr"><is><t>x</t></is></c></row></sheetData><mergeCells><mergeCell ref="B1:B2"/></mergeCells>"#
            );
            assert_eq!(diagnose(&one_sheet(&body)), expected, "{cell}");
        }
        let bad_merge = r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>x</t></is></c></row></sheetData><mergeCells><mergeCell ref="A0:B"/></mergeCells>"#;
        assert_eq!(
            diagnose(&one_sheet(bad_merge)),
            [(DiagnosticCode::TableSpanRepaired, BODY, Some(1), 1)]
        );
    }

    #[test]
    fn hidden_rows_and_columns_count_where_they_hold_content() {
        let body = r#"<cols><col min="2" max="16384" hidden="1"/></cols><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c><c r="B1" t="inlineStr"><is><t>b</t></is></c><c r="C1" t="inlineStr"><is><t>c</t></is></c></row><row r="2" hidden="1"><c r="A2" t="inlineStr"><is><t>h</t></is></c></row><row r="3" hidden="1"/></sheetData>"#;
        assert_eq!(diagnose(&one_sheet(body)), [(DiagnosticCode::HiddenOmitted, BODY, Some(1), 3)]);
    }

    const HLINK_REL: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";

    fn link_cells(cells: &[(&str, &str)]) -> String {
        cells
            .iter()
            .map(|(at, text)| format!(r#"<c r="{at}" t="inlineStr"><is><t>{text}</t></is></c>"#))
            .collect()
    }

    #[test]
    fn a_hyperlink_whose_label_hides_its_target_loses_it() {
        let rels = format!(
            r#"<Relationship Id="rId1" Type="{HLINK_REL}" Target="https://example.org/paper" TargetMode="External"/><Relationship Id="rId2" Type="{HLINK_REL}" Target="http://www.example.org/" TargetMode="External"/><Relationship Id="rId3" Type="{HLINK_REL}" Target="https://doi.org/10.1234/abc" TargetMode="External"/>"#
        );
        let row1 = link_cells(&[
            ("A1", "see here"),
            ("B1", "www.example.org"),
            ("C1", "10.1234/abc"),
            ("D1", "next sheet"),
            ("E1", "gone"),
            ("F1", "https://example.org/x"),
            ("G1", "localhost:8888"),
            ("H1", "see here"),
        ]);
        let row2 = link_cells(&[("A2", "see here")]);
        let body = format!(
            r#"<cols><col min="8" max="8" hidden="1"/></cols><sheetData><row r="1">{row1}</row><row r="2" hidden="1">{row2}</row></sheetData><hyperlinks xmlns:r="{R}"><hyperlink ref="A1" r:id="rId1"/><hyperlink ref="B1" r:id="rId2"/><hyperlink ref="C1:C2" r:id="rId3"/><hyperlink ref="D1" location="S!A1"/><hyperlink ref="E1" r:id="rId8"/><hyperlink ref="F1" r:id="rId8"/><hyperlink ref="G1" r:id="rId8"/><hyperlink ref="H1" r:id="rId1"/><hyperlink ref="A2" r:id="rId1"/></hyperlinks>"#
        );
        let wb = Wb { sheet_rels: Some(&rels), ..one_sheet(&body) };
        assert_eq!(
            diagnose(&wb),
            [
                (DiagnosticCode::LinkTargetUnread, BODY, Some(1), 3),
                (DiagnosticCode::HiddenOmitted, BODY, Some(1), 2),
            ]
        );
    }

    #[test]
    fn a_loss_or_link_a_merge_covers_is_not_shown() {
        let rels = format!(
            r#"<Relationship Id="rId1" Type="{HLINK_REL}" Target="https://example.org/paper" TargetMode="External"/>"#
        );
        let covered = format!(
            r#"<sheetData><row r="1">{}<c r="B1"><f>1+1</f></c></row></sheetData><mergeCells><mergeCell ref="A1:B1"/></mergeCells><hyperlinks xmlns:r="{R}"><hyperlink ref="B1" r:id="rId1"/></hyperlinks>"#,
            link_cells(&[("A1", "a")])
        );
        assert_eq!(diagnose(&Wb { sheet_rels: Some(&rels), ..one_sheet(&covered) }), []);
        let past_hidden = format!(
            r#"<sheetData><row r="1" hidden="1"/><row r="2">{}</row></sheetData><hyperlinks xmlns:r="{R}"><hyperlink ref="A1:A2" r:id="rId1"/></hyperlinks>"#,
            link_cells(&[("A2", "see here")])
        );
        assert_eq!(
            diagnose(&Wb { sheet_rels: Some(&rels), ..one_sheet(&past_hidden) }),
            [(DiagnosticCode::LinkTargetUnread, BODY, Some(1), 1)]
        );
        let moved = format!(
            r#"<sheetData><row r="1" hidden="1">{}</row><row r="2">{}</row></sheetData><mergeCells><mergeCell ref="A1:A2"/></mergeCells><hyperlinks xmlns:r="{R}"><hyperlink ref="A1" r:id="rId1"/></hyperlinks>"#,
            link_cells(&[("A1", "see here")]),
            link_cells(&[("B2", "x")])
        );
        assert_eq!(
            diagnose(&Wb { sheet_rels: Some(&rels), ..one_sheet(&moved) }),
            [(DiagnosticCode::LinkTargetUnread, BODY, Some(1), 1)]
        );
        let covered_first = format!(
            r#"<sheetData><row r="1" hidden="1">{}</row><row r="2">{}</row></sheetData><mergeCells><mergeCell ref="A1:A2"/></mergeCells><hyperlinks xmlns:r="{R}"><hyperlink ref="A2" r:id="rId1"/><hyperlink ref="A1" r:id="rId1"/></hyperlinks>"#,
            link_cells(&[("A1", "see here")]),
            link_cells(&[("B2", "x")])
        );
        assert_eq!(
            diagnose(&Wb { sheet_rels: Some(&rels), ..one_sheet(&covered_first) }),
            [(DiagnosticCode::LinkTargetUnread, BODY, Some(1), 1)]
        );
        let leaves_merge = format!(
            r#"<sheetData><row r="1">{}</row><row r="2">{}</row></sheetData><mergeCells><mergeCell ref="A1:B1"/></mergeCells><hyperlinks xmlns:r="{R}"><hyperlink ref="B1:B2" r:id="rId1"/></hyperlinks>"#,
            link_cells(&[("A1", "a"), ("B1", "https://example.org/paper")]),
            link_cells(&[("B2", "see here")])
        );
        assert_eq!(
            diagnose(&Wb { sheet_rels: Some(&rels), ..one_sheet(&leaves_merge) }),
            [(DiagnosticCode::LinkTargetUnread, BODY, Some(1), 1)]
        );
    }

    #[test]
    fn a_hyperlink_under_a_reported_relationships_part_adds_nothing() {
        let body = format!(
            r#"<sheetData><row r="1">{}</row></sheetData><hyperlinks xmlns:r="{R}"><hyperlink ref="A1" r:id="rId1"/></hyperlinks>"#,
            link_cells(&[("A1", "see here")])
        );
        let wb = Wb {
            extra: vec![("xl/worksheets/_rels/sheet1.xml.rels", "<Relationships")],
            ..one_sheet(&body)
        };
        assert_eq!(diagnose(&wb), [(DiagnosticCode::PartUnreadable, BODY, Some(1), 1)]);
    }

    const DRAWING_REL: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing";

    fn drawing(objects: &str) -> String {
        format!(
            r#"<xdr:wsDr xmlns:xdr="{xdr}" xmlns:a="{a}" xmlns:a14="{a14}" xmlns:mc="{mc}">{objects}</xdr:wsDr>"#,
            xdr = ns::XDR,
            a = ns::A,
            a14 = ns::A14,
            mc = ns::MC
        )
    }

    fn sp(hidden: &str, inner: &str) -> String {
        format!(
            r#"<xdr:twoCellAnchor><xdr:sp><xdr:nvSpPr><xdr:cNvPr id="4" name="s"{hidden}/></xdr:nvSpPr>{inner}</xdr:sp></xdr:twoCellAnchor>"#
        )
    }

    fn drawing_wb<'a>(rels: &'a str, body: &'a str, parts: Vec<(&'a str, &'a str)>) -> Wb<'a> {
        Wb { sheet_rels: Some(rels), extra: parts, ..one_sheet(body) }
    }

    #[test]
    fn drawings_comments_and_print_headers_are_reported() {
        const TEXT: &str = "<xdr:txBody><a:p><a:r><a:t>Note</a:t></a:r></a:p></xdr:txBody>";
        const FILL: &str = r#"<xdr:spPr><a:blipFill><a:blip/></a:blipFill></xdr:spPr>"#;
        const PIC: &str = r#"<xdr:twoCellAnchor><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="2" name="p"/></xdr:nvPicPr></xdr:pic></xdr:twoCellAnchor>"#;
        const CHART: &str = "<xdr:oneCellAnchor><xdr:graphicFrame/></xdr:oneCellAnchor>";
        let control = r#"<xdr:twoCellAnchor><xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="c"><a:extLst><a:ext><a14:compatExt spid="_x0000_s1025"/></a:ext></a:extLst></xdr:cNvPr></xdr:nvSpPr><xdr:txBody><a:p><a:r><a:t>Check Box 1</a:t></a:r></a:p></xdr:txBody></xdr:sp></xdr:twoCellAnchor>"#;
        let alternate = |choice: &str, fallback: &str| {
            format!(
                r#"<mc:AlternateContent><mc:Choice Requires="a14">{choice}</mc:Choice><mc:Fallback>{fallback}</mc:Fallback></mc:AlternateContent>"#
            )
        };
        let objects = [
            PIC.to_string(),
            CHART.to_string(),
            format!(
                "<xdr:absoluteAnchor><xdr:grpSp>{}{}</xdr:grpSp></xdr:absoluteAnchor>",
                sp("", TEXT),
                sp("", "")
            ),
            PIC.replace(r#"name="p""#, r#"name="p" hidden="1""#),
            r#"<xdr:oneCellAnchor><xdr:graphicFrame><xdr:nvGraphicFramePr><xdr:cNvPr id="6" name="g" hidden="1"/></xdr:nvGraphicFramePr></xdr:graphicFrame></xdr:oneCellAnchor>"#.to_string(),
            sp(r#" hidden="1""#, TEXT),
            sp("", FILL),
            sp("", &format!("{FILL}{TEXT}")),
            alternate(control, ""),
            alternate(PIC, ""),
            alternate(CHART, ""),
            alternate(CHART, PIC),
            alternate(&format!("{PIC}{PIC}"), PIC),
            alternate("", PIC),
            control.to_string(),
        ]
        .concat();
        let drawing1 = drawing(&objects);
        let orphan = drawing(PIC);
        let rels = format!(
            r#"<Relationship Id="rId1" Type="{DRAWING_REL}" Target="../drawings/drawing1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="../comments1.xml"/><Relationship Id="rId3" Type="{DRAWING_REL}" Target="../drawings/drawing2.xml"/>"#
        );
        let body = format!(
            r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row></sheetData><headerFooter><oddHeader>&amp;CTitle</oddHeader><oddFooter>&amp;CPage &amp;P</oddFooter><evenFooter> </evenFooter></headerFooter><drawing xmlns:r="{R}" r:id="rId1"/>"#
        );
        let parts = vec![
            ("xl/drawings/drawing1.xml", drawing1.as_str()),
            ("xl/drawings/drawing2.xml", &orphan),
        ];
        assert_eq!(
            diagnose(&drawing_wb(&rels, &body, parts)),
            [
                (DiagnosticCode::ImageUnplaced, BODY, Some(1), 7),
                (DiagnosticCode::GraphicUnread, BODY, Some(1), 6),
                (DiagnosticCode::HeaderOmitted, Region::Header, Some(1), 1),
                (DiagnosticCode::FooterOmitted, Region::Footer, Some(1), 1),
                (DiagnosticCode::CommentOmitted, Region::Comment, Some(1), 1),
            ]
        );
        let unnamed = format!(
            r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row></sheetData><drawing xmlns:r="{R}" r:id="rId9"/>"#
        );
        assert_eq!(
            diagnose(&drawing_wb(&rels, &unnamed, vec![])),
            [
                (DiagnosticCode::PartUnreadable, BODY, Some(1), 1),
                (DiagnosticCode::CommentOmitted, Region::Comment, Some(1), 1),
            ]
        );
        let named = format!(
            r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row></sheetData><drawing xmlns:r="{R}" r:id="rId1"/>"#
        );
        let corrupt_rels = Wb {
            extra: vec![("xl/worksheets/_rels/sheet1.xml.rels", "<Relationships")],
            ..one_sheet(&named)
        };
        assert_eq!(diagnose(&corrupt_rels), [(DiagnosticCode::PartUnreadable, BODY, Some(1), 1)]);
        let only_drawing = format!(
            r#"<Relationship Id="rId1" Type="{DRAWING_REL}" Target="../drawings/drawing1.xml"/>"#
        );
        let wrong_root = format!(r#"<xdr:pic xmlns:xdr="{}"/>"#, ns::XDR);
        let parts = vec![("xl/drawings/drawing1.xml", wrong_root.as_str())];
        assert_eq!(
            diagnose(&drawing_wb(&only_drawing, &named, parts)),
            [(DiagnosticCode::PartUnreadable, BODY, Some(1), 1)]
        );
    }

    #[test]
    fn a_compat_control_is_left_out_only_inside_a_choice() {
        let control = r#"<xdr:twoCellAnchor><xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="c"><a:extLst><a:ext><a14:compatExt spid="_x0000_s1025"/></a:ext></a:extLst></xdr:cNvPr></xdr:nvSpPr><xdr:txBody><a:p><a:r><a:t>Check Box 1</a:t></a:r></a:p></xdr:txBody></xdr:sp></xdr:twoCellAnchor>"#;
        let rels = format!(
            r#"<Relationship Id="rId1" Type="{DRAWING_REL}" Target="../drawings/drawing1.xml"/>"#
        );
        let body = format!(
            r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row></sheetData><drawing xmlns:r="{R}" r:id="rId1"/>"#
        );
        let chosen = drawing(&format!(
            r#"<mc:AlternateContent><mc:Choice Requires="a14">{control}</mc:Choice></mc:AlternateContent>"#
        ));
        let parts = vec![("xl/drawings/drawing1.xml", chosen.as_str())];
        assert_eq!(diagnose(&drawing_wb(&rels, &body, parts)), []);
        let bare = drawing(control);
        let parts = vec![("xl/drawings/drawing1.xml", bare.as_str())];
        assert_eq!(
            diagnose(&drawing_wb(&rels, &body, parts)),
            [(DiagnosticCode::GraphicUnread, BODY, Some(1), 1)]
        );
    }

    #[test]
    fn a_styles_part_with_the_wrong_root_is_unreadable_once() {
        let rels = format!(r#"<Relationship Id="rId90" Type="{STYLES_REL}" Target="styles.xml"/>"#);
        let styles = format!(r#"<?xml version="1.0"?><sst xmlns="{SML}"/>"#);
        let body = number_cells(&[1, 2]);
        let wb = Wb {
            workbook_rels: &rels,
            extra: vec![("xl/styles.xml", &styles)],
            ..one_sheet(&body)
        };
        assert_eq!(diagnose(&wb), [(DiagnosticCode::PartUnreadable, BODY, None, 1)]);
    }

    #[test]
    fn a_loss_where_the_sheet_hides_it_is_only_hidden() {
        let styles = r#"<numFmts><numFmt numFmtId="164" formatCode="[DBNum1]yyyy"/></numFmts><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/></cellXfs>"#;
        let body = r#"<cols><col min="2" max="2" hidden="1"/></cols><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c><c r="B1" s="1"><v>2</v></c></row><row r="2" hidden="1"><c r="A2"><f>1+1</f><v/></c></row></sheetData>"#;
        let wb = Wb { styles: Some(styles), ..one_sheet(body) };
        assert_eq!(diagnose(&wb), [(DiagnosticCode::HiddenOmitted, BODY, Some(1), 2)]);
        let moved = r#"<sheetData><row r="1" hidden="1"><c r="A1"><f>1+1</f><v/></c></row><row r="2"><c r="B2" t="inlineStr"><is><t>x</t></is></c></row></sheetData><mergeCells><mergeCell ref="A1:A2"/></mergeCells>"#;
        assert_eq!(diagnose(&one_sheet(moved)), [(CELL, BODY, Some(1), 1)]);
    }

    #[test]
    fn form_controls_the_grid_cannot_show_are_reported() {
        let vml = r##"<xml xmlns:v="urn:schemas-microsoft-com:vml" xmlns:x="urn:schemas-microsoft-com:office:excel">
            <v:shape><x:ClientData ObjectType="Checkbox"><x:Anchor>1, 5, 0, 2, 2, 10, 1, 1</x:Anchor><x:Checked>1</x:Checked></x:ClientData></v:shape>
            <v:shape style="visibility:hidden"><x:ClientData ObjectType="Checkbox"><x:Anchor>1, 5, 0, 2, 2, 10, 1, 1</x:Anchor></x:ClientData></v:shape>
            <v:shape><x:ClientData ObjectType="Checkbox"><x:Anchor>1, 5, 0, 2, 2, 10, 1, 1</x:Anchor><x:Checked>2</x:Checked></x:ClientData></v:shape>
            <v:shape><x:ClientData ObjectType="Checkbox"><x:Anchor>bad</x:Anchor></x:ClientData></v:shape>
            <v:shape><x:ClientData ObjectType="Checkbox"><x:Anchor>1, 5, 0, 2, 2, 10, 1, 1</x:Anchor><x:Checked>0</x:Checked></x:ClientData></v:shape>
            <v:shape><x:ClientData ObjectType="Note"><x:Anchor>1, 5, 0, 2, 2, 10, 1, 1</x:Anchor></x:ClientData></v:shape>
            {others}
            </xml>"##;
        let others: String = ["Radio", "Label", "Button", "GBox", "Drop", "List", "Spin", "Scroll", "Edit"]
            .map(|kind| format!(r#"<v:shape><x:ClientData ObjectType="{kind}"><x:Anchor>1, 5, 0, 2, 2, 10, 1, 1</x:Anchor></x:ClientData></v:shape>"#))
            .concat();
        let vml = vml.replace("{others}", &others);
        let rels = format!(
            r#"<Relationship Id="rId1" Type="{VML_REL}" Target="../drawings/vmlDrawing1.vml"/>"#
        );
        let wb = Wb {
            sheet_rels: Some(&rels),
            extra: vec![("xl/drawings/vmlDrawing1.vml", &vml)],
            ..one_sheet(
                r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row></sheetData>"#,
            )
        };
        assert_eq!(
            diagnose(&wb),
            [
                (DiagnosticCode::FormControlUnread, BODY, Some(1), 11),
                (DiagnosticCode::HiddenOmitted, BODY, Some(1), 1),
            ]
        );
    }
}
