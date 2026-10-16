//! The grid every Excel container fills: one worksheet's cells, visibility,
//! merges, hyperlinks and losses, the number formats that render its values,
//! and the table built from it. The xlsx, xlsb and xls readers each decode
//! their records into a [`SheetContent`] and share everything after that.

use super::controls::{Checkboxes, cell_inlines};
use super::numfmt::{DateParts, NumberFormat, Rendered, builtin_code, text_section};
use super::{format_duration_days, format_float, format_time_of_day};
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::limits::{Limit, Limits};
use crate::model::{Cell, GridBuilder, Inline, Table, TableKind, inlines_to_plain_text};
use crate::shared::header::resolve_header_rows;
use crate::shared::link_label::{label_is_absolute_url, label_shows_target};
use crate::shared::text::clean_text;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

/// The grid bounds the format defines; a reference outside them is not a
/// real cell.
pub(super) const MAX_ROWS: u32 = 1_048_576;
pub(super) const MAX_COLS: u32 = 16_384;

/// A zero-based cell position, ordered row first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct CellRef {
    pub(super) row: u32,
    pub(super) col: u32,
}

impl CellRef {
    pub(super) const fn new(row: u32, col: u32) -> CellRef {
        CellRef { row, col }
    }

    fn min_each(self, other: CellRef) -> CellRef {
        CellRef::new(self.row.min(other.row), self.col.min(other.col))
    }

    fn max_each(self, other: CellRef) -> CellRef {
        CellRef::new(self.row.max(other.row), self.col.max(other.col))
    }
}

/// An inclusive zero-based cell range from its top-left to its bottom-right
/// cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CellRange {
    pub(super) first: CellRef,
    pub(super) last: CellRef,
}

impl CellRange {
    /// The range two opposite corners span, given in either order.
    pub(super) fn spanning(a: CellRef, b: CellRef) -> CellRange {
        CellRange { first: a.min_each(b), last: a.max_each(b) }
    }

    fn single(at: CellRef) -> CellRange {
        CellRange { first: at, last: at }
    }

    fn is_single(self) -> bool {
        self.first == self.last
    }

    fn contains(self, other: CellRange) -> bool {
        self.first.row <= other.first.row
            && other.last.row <= self.last.row
            && self.first.col <= other.first.col
            && other.last.col <= self.last.col
    }

    fn intersects(self, other: CellRange) -> bool {
        self.first.row <= other.last.row
            && self.last.row >= other.first.row
            && self.first.col <= other.last.col
            && self.last.col >= other.first.col
    }

    fn union(self, other: CellRange) -> CellRange {
        CellRange { first: self.first.min_each(other.first), last: self.last.max_each(other.last) }
    }
}

/// The day a workbook's date serials count from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum DateSystem {
    /// Serial 1 is 1900-01-01, with the fictitious 1900-02-29 at serial 60.
    #[default]
    Since1900,
    /// Serial 0 is 1904-01-01.
    Since1904,
}

impl DateSystem {
    /// The system a workbook's `date1904` flag selects.
    pub(super) fn from_date1904(date1904: bool) -> DateSystem {
        if date1904 { DateSystem::Since1904 } else { DateSystem::Since1900 }
    }
}

/// The rows and columns a container's grid holds; a cell at or past either
/// count is not a real cell.
#[derive(Clone, Copy, Debug)]
pub(super) struct GridBounds {
    pub(super) rows: u32,
    pub(super) cols: u32,
}

/// The grid of xlsx and xlsb: [MS-XLSB] Rw and Col bound a zero-based row
/// index at 1,048,575 and a column index at 16,383. ECMA-376 Part 1
/// §18.17.5.1 sets the same grid for SpreadsheetML, A1 to XFD1048576, and
/// lets an implementation extend it; xlsx keeps it unextended, so the two
/// readers agree.
pub(super) const OOXML_GRID: GridBounds = GridBounds { rows: MAX_ROWS, cols: MAX_COLS };

/// One worksheet, parsed but not yet filtered or gridded.
#[derive(Default)]
pub(super) struct SheetContent {
    /// Rendered text by cell; empty results are absent.
    cells: HashMap<CellRef, String>,
    /// Form control checkboxes by the cell they are anchored in.
    pub(super) checkboxes: Checkboxes,
    hidden_rows: HashSet<u32>,
    /// Inclusive zero-based column ranges.
    hidden_cols: Vec<(u32, u32)>,
    /// Merge ranges of more than one cell.
    merges: Vec<CellRange>,
    /// External hyperlinks by the cells whose text labels them.
    pub(super) links: Vec<SheetLink>,
    /// What each cell lost, reported only where the cell survives hidden
    /// filtering and no merge covers it.
    losses: Vec<(CellRef, DiagnosticCode)>,
}

impl SheetContent {
    /// Record a cell's text; a text that is empty or only whitespace shows
    /// nothing and leaves no cell, so it never widens the grid.
    pub(super) fn put(&mut self, at: CellRef, text: String) {
        if !text.trim().is_empty() {
            self.cells.insert(at, text);
        }
    }

    /// Record what a cell lost, reported once the grid shows the cell.
    pub(super) fn lose(&mut self, at: CellRef, code: DiagnosticCode) {
        self.losses.push((at, code));
    }

    pub(super) fn hide_row(&mut self, row: u32) {
        self.hidden_rows.insert(row);
    }

    /// Hide the inclusive column range `first..=last`, as far as the grid
    /// reaches.
    pub(super) fn hide_cols(&mut self, first: u32, last: u32) {
        let last = last.min(MAX_COLS - 1);
        if first <= last {
            self.hidden_cols.push((first, last));
        }
    }

    /// Record a merge range within `grid`: one running past it is clamped to
    /// the part the grid shows and one wholly outside it is dropped, each
    /// reported; a single cell merges nothing.
    pub(super) fn merge(&mut self, parse_ctx: &ParseCtx, range: CellRange, grid: GridBounds) {
        if range.first.row >= grid.rows || range.first.col >= grid.cols {
            parse_ctx.emit(DiagnosticCode::TableSpanRepaired, Region::Body);
            return;
        }
        let last = CellRef::new(
            range.last.row.min(grid.rows.saturating_sub(1)),
            range.last.col.min(grid.cols.saturating_sub(1)),
        );
        if last != range.last {
            parse_ctx.emit(DiagnosticCode::TableSpanRepaired, Region::Body);
        }
        let range = CellRange { last, ..range };
        if !range.is_single() {
            self.merges.push(range);
        }
    }
}

/// A hyperlink to a target outside the workbook over a cell range.
pub(super) struct SheetLink {
    pub(super) range: CellRange,
    pub(super) target: LinkTarget,
}

pub(super) enum LinkTarget {
    /// A relationship id not yet looked up in the worksheet's relationships.
    Related(String),
    /// A target held once per relationship, however many links name it.
    Url(Rc<str>),
    /// The target is gone and nothing has reported it.
    Unknown,
    /// The target is gone with a relationships part already reported.
    Reported,
}

/// A cell's resolved number format: General, a parsed format code, or
/// General standing in for a format that could not be applied, which a
/// numeric cell reports as `number` and a text cell as `number_format_unread`
/// when the format's text section shows more than the text.
#[derive(Clone)]
pub(super) enum CellFormat {
    General,
    Fmt(Rc<NumberFormat>),
    Fallback { number: DiagnosticCode, text_unread: bool },
}

/// The ordered cell formats of a workbook, which a cell names by index,
/// each with whether its format code needed a replacement character.
pub(super) struct CellXfs {
    xfs: Vec<(CellFormat, bool)>,
    /// The styles part did not read and reported that, so a cell naming an
    /// index it would have held reports nothing more.
    reported: bool,
}

impl CellXfs {
    pub(super) fn new(xfs: Vec<(CellFormat, bool)>) -> CellXfs {
        CellXfs { xfs, reported: false }
    }

    /// No styles part was read; `reported` when its read reported the loss.
    pub(super) fn unread(reported: bool) -> CellXfs {
        CellXfs { xfs: Vec::new(), reported }
    }

    /// The format at `index`; one outside the table renders General, as the
    /// application repairs it, and the default index 0 of a workbook with no
    /// formats is General.
    pub(super) fn get(&self, index: usize) -> CellFormat {
        match self.xfs.get(index) {
            Some((format, _)) => format.clone(),
            None if self.reported || (index == 0 && self.xfs.is_empty()) => CellFormat::General,
            None => CellFormat::Fallback {
                number: DiagnosticCode::NumberFormatFallback,
                text_unread: false,
            },
        }
    }

    /// Whether the format at `index` put a replacement character into the
    /// cell text it rendered.
    pub(super) fn replaced_in(&self, index: usize, text: &str) -> bool {
        self.xfs.get(index).is_some_and(|&(_, replaced)| replaced)
            && text.contains(char::REPLACEMENT_CHARACTER)
    }
}

/// A numFmtId's format: the file's own `numFmt` entries first, then the
/// built-in table. Unknown ids and unsupported codes fall back to General -
/// never to a guess - and carry what that fallback costs: a number what its
/// numeric sections show, a text what its text section adds.
pub(super) fn resolve_format(id: u32, custom: &HashMap<u32, &str>) -> CellFormat {
    let code = custom.get(&id).copied().or_else(|| builtin_code(id));
    let (unread, text_unread) = match code {
        Some(code) => match NumberFormat::parse(code) {
            Some(f) => return CellFormat::Fmt(Rc::new(f)),
            None => {
                let text = text_section(code);
                (shows_more_than_its_number(text.numeric), text.decorates)
            }
        },
        None if id == 0 => return CellFormat::General,
        None => (matches!(id, 5..=8 | 27..=36 | 42 | 44 | 50..=58 | 71..=81), false),
    };
    let number = if unread {
        DiagnosticCode::NumberFormatUnread
    } else {
        DiagnosticCode::NumberFormatFallback
    };
    CellFormat::Fallback { number, text_unread }
}

/// Whether a format code shows more than the number General shows: a date
/// or time token, an elapsed-time bracket, literal text, or a currency or
/// unit symbol. A `[$-…]` bracket names only a locale, and the `General`
/// keyword shows the number.
fn shows_more_than_its_number(code: &str) -> bool {
    const CURRENCY: &[char] = &['$', '¢', '£', '¥', '₩', '₪', '₫', '€', '₱', '₹', '₺', '₽', '฿'];
    let mut rest = code;
    loop {
        let mut chars = rest.chars();
        let Some(c) = chars.next() else {
            return false;
        };
        let after = chars.as_str();
        rest = match c {
            '[' => {
                let (bracket, tail) = after.split_once(']').unwrap_or((after, ""));
                let currency =
                    bracket.strip_prefix('$').is_some_and(|s| !s.is_empty() && !s.starts_with('-'));
                let elapsed = !bracket.is_empty()
                    && bracket.chars().all(|c| matches!(c.to_ascii_lowercase(), 'h' | 'm' | 's'));
                if currency || elapsed {
                    return true;
                }
                tail
            }
            '"' | '\\' => return true,
            '_' | '*' => {
                chars.next();
                chars.as_str()
            }
            'E' | 'e' if after.starts_with(['+', '-']) => after.get(1..).unwrap_or_default(),
            'G' | 'g' if rest.get(..7).is_some_and(|w| w.eq_ignore_ascii_case("general")) => {
                rest.get(7..).unwrap_or_default()
            }
            c if c.is_alphabetic() || CURRENCY.contains(&c) => return true,
            _ => after,
        };
    }
}

/// Render a numeric cell through its resolved format, with the code a
/// fallback to General costs, so every container formats identically.
pub(super) fn render_number(
    fmt: &CellFormat,
    n: f64,
    dates: DateSystem,
) -> (String, Option<DiagnosticCode>) {
    let (text, loss) = match fmt {
        CellFormat::General => (format_float(n), None),
        CellFormat::Fallback { number, .. } => (format_float(n), Some(*number)),
        CellFormat::Fmt(f) => match f.format_number(n) {
            Rendered::General { value, prefix, suffix } => {
                (format!("{prefix}{}{suffix}", format_float(value)), None)
            }
            Rendered::Text(s) => (s, None),
            Rendered::DateTime(parts) => (render_serial(n, parts, dates), None),
        },
    };
    (clean_text(&text), loss)
}

/// Render a text cell through its resolved format's text section, with the
/// code a fallback to General costs when that section would have shown more.
pub(super) fn format_as_text(fmt: &CellFormat, text: &str) -> (String, Option<DiagnosticCode>) {
    match fmt {
        CellFormat::Fmt(f) => match f.format_text(text) {
            Some(s) => (clean_text(&s), None),
            None => (text.to_string(), None),
        },
        CellFormat::Fallback { text_unread: true, .. } => {
            (text.to_string(), Some(DiagnosticCode::NumberFormatUnread))
        }
        CellFormat::General | CellFormat::Fallback { .. } => (text.to_string(), None),
    }
}

/// Render a date/time serial the way the crate always has: elapsed formats
/// as a duration, sub-day serials as a time of day, everything else as an
/// ISO-like date with the midnight time omitted.
fn render_serial(serial: f64, parts: DateParts, dates: DateSystem) -> String {
    if !serial.is_finite() {
        return format_float(serial);
    }
    if parts.elapsed {
        return format_duration_days(serial);
    }
    if !parts.date {
        return format_time_of_day(serial.fract());
    }
    // A serial carrying no whole day has no date for a date format to show;
    // the clock a combined format also names still renders.
    if serial.abs() < 1.0 {
        return if parts.time { format_time_of_day(serial) } else { format_float(serial) };
    }
    // Out of the representable date range (through 9999-12-31): the serial
    // is not a date, show the number.
    if !(0.0..2_958_466.0).contains(&serial) {
        return format_float(serial);
    }
    let mut days = serial.trunc() as i64;
    // The fictitious 1900-02-29 has no date to render and would otherwise
    // collapse onto serial 59. Tested before the seconds carry, so a value
    // late on serial 59 still resolves as the date it belongs to.
    if dates == DateSystem::Since1900 && days == 60 {
        return format_float(serial);
    }
    let mut secs = (serial.fract() * 86_400.0).round() as i64;
    if secs >= 86_400 {
        secs = 0;
        days += 1;
    }
    let civil_days = match dates {
        DateSystem::Since1904 => days + CivilDate { year: 1904, month: 1, day: 1 }.days(),
        DateSystem::Since1900 => {
            days - i64::from(days >= 60) + CivilDate { year: 1899, month: 12, day: 31 }.days()
        }
    };
    let CivilDate { year, month, day } = CivilDate::from_days(civil_days);
    if !(1..=9999).contains(&year) {
        return format_float(serial);
    }
    let mut out = format!("{year:04}-{month:02}-{day:02}");
    if parts.time && secs != 0 {
        out.push_str(&format!(" {:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60));
    }
    out
}

/// A proleptic Gregorian calendar date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CivilDate {
    year: i64,
    month: u32,
    day: u32,
}

impl CivilDate {
    /// Days from 1970-01-01 (Howard Hinnant's algorithm).
    fn days(self) -> i64 {
        let y = self.year - i64::from(self.month <= 2);
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let mp = i64::from((self.month + 9) % 12);
        let doy = (153 * mp + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// The date a count of days since 1970-01-01 falls on (Howard Hinnant's
    /// algorithm).
    fn from_days(z: i64) -> CivilDate {
        let z = z + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        CivilDate { year: y + i64::from(month <= 2), month, day }
    }
}

/// Materialize a sheet's grid: visibility filtering, the populated extent
/// widened to cover intersecting merge regions (a merge anchored on the
/// only populated cell must survive at full size), and merges remapped onto
/// the surviving rows and columns.
pub(super) fn build_table(
    parse_ctx: &ParseCtx,
    mut sheet: SheetContent,
    slots: &mut u64,
) -> Result<Option<Table>, ConvertError> {
    let hidden = Hidden::of(&mut sheet);
    let mut cells = cell_contents(&mut sheet);
    let moved = move_hidden_origins(&mut sheet.merges, &hidden, &mut cells);
    report_losses_and_links(parse_ctx, &sheet, &hidden, &moved, &cells)?;
    let Some(extent) = extent(&cells, &hidden, &mut sheet.merges) else {
        return Ok(None);
    };
    let row_map: Vec<u32> =
        (extent.first.row..=extent.last.row).filter(|&r| !hidden.row(r)).collect();
    let col_map: Vec<u32> =
        (extent.first.col..=extent.last.col).filter(|&c| !hidden.col(c)).collect();
    if row_map.is_empty() || col_map.is_empty() {
        return Ok(None);
    }
    // Charged before materializing, and across the workbook rather than per
    // sheet: the extent comes from cell coordinates, so a handful of cells
    // describes a whole sheet and a handful of sheets multiplies it.
    let limits = parse_ctx.limits();
    let positions = row_map.len() as u64 * col_map.len() as u64;
    *slots = slots.saturating_add(positions);
    if *slots > limits.max_grid_slots {
        return Err(ConvertError::limit(Limit::GridSlots, limits.max_grid_slots));
    }
    parse_ctx.charge(positions)?;
    let layout = MergeLayout::on(limits, &sheet.merges, &row_map, &col_map)?;
    assemble(limits, &row_map, &col_map, &layout, cells)
}

/// The rows and columns a sheet hides, as sorted lists: lookups and
/// first-visible scans stay logarithmic, so an adversarial pile of hidden
/// rows or column ranges cannot force quadratic work.
struct Hidden {
    rows: Vec<u32>,
    cols: Vec<u32>,
}

impl Hidden {
    fn of(sheet: &mut SheetContent) -> Hidden {
        let mut rows: Vec<u32> = sheet.hidden_rows.iter().copied().collect();
        rows.sort_unstable();
        Hidden { rows, cols: expand_ranges(&mut sheet.hidden_cols) }
    }

    fn row(&self, r: u32) -> bool {
        self.rows.binary_search(&r).is_ok()
    }

    fn col(&self, c: u32) -> bool {
        self.cols.binary_search(&c).is_ok()
    }

    fn shows(&self, at: CellRef) -> bool {
        !self.row(at.row) && !self.col(at.col)
    }

    /// The first cell of `range` in a shown row and a shown column.
    fn first_shown(&self, range: CellRange) -> Option<CellRef> {
        let row = first_visible(&self.rows, range.first.row, range.last.row)?;
        let col = first_visible(&self.cols, range.first.col, range.last.col)?;
        Some(CellRef::new(row, col))
    }
}

/// Cell text and anchored checkboxes as one inline map; the rest of the
/// assembly no longer cares which was which.
fn cell_contents(sheet: &mut SheetContent) -> HashMap<CellRef, Vec<Inline>> {
    let mut cells = HashMap::new();
    for (at, boxes) in sheet.checkboxes.drain() {
        if at.row < MAX_ROWS && at.col < MAX_COLS {
            cells.insert(at, cell_inlines(sheet.cells.remove(&at), &boxes));
        }
    }
    for (at, text) in sheet.cells.drain() {
        cells.insert(at, vec![Inline::plain(text)]);
    }
    cells
}

/// Where each merge whose origin is hidden moved its origin. A merge with
/// no surviving row or column disappears with its content; one whose origin
/// is hidden keeps its content at the first surviving position it covers,
/// so the value is not lost.
fn move_hidden_origins(
    merges: &mut Vec<CellRange>,
    hidden: &Hidden,
    cells: &mut HashMap<CellRef, Vec<Inline>>,
) -> HashMap<CellRef, CellRef> {
    let mut moved = HashMap::new();
    merges.retain(|&merge| {
        let Some(at) = hidden.first_shown(merge) else {
            return false;
        };
        if at != merge.first {
            moved.insert(merge.first, at);
            if let Some(text) = cells.remove(&merge.first) {
                cells.insert(at, text);
            }
        }
        true
    });
    moved
}

/// Report the hidden rows and columns that hold content, the losses of
/// shown cells, and the hyperlinks whose shown label does not carry their
/// target.
fn report_losses_and_links(
    parse_ctx: &ParseCtx,
    sheet: &SheetContent,
    hidden: &Hidden,
    moved: &HashMap<CellRef, CellRef>,
    cells: &HashMap<CellRef, Vec<Inline>>,
) -> Result<(), ConvertError> {
    let place = |at: CellRef| moved.get(&at).copied().unwrap_or(at);
    // A link shows in the first visible cell of its range, or where its
    // range's origin moved with a merge.
    let candidates: Vec<(CellRef, bool, &SheetLink)> = sheet
        .links
        .iter()
        .filter_map(|link| match moved.get(&link.range.first) {
            Some(&at) => Some((at, true, link)),
            None => hidden.first_shown(link.range).map(|at| (at, false, link)),
        })
        .collect();
    let covered = merge_covered(
        parse_ctx,
        &sheet.merges,
        sheet
            .losses
            .iter()
            .map(|&(at, _)| at)
            .chain(candidates.iter().filter(|&&(_, moved, _)| !moved).map(|&(at, _, _)| at)),
    )?;
    // An uncovered cell is judged once, by its own label. A link a single
    // merge hides whole is not shown; one that leaves the merge is judged
    // at its covered first cell, which shows no label.
    let mut judged: HashSet<CellRef> = HashSet::new();
    let mut links: Vec<(CellRef, bool, &LinkTarget)> = Vec::new();
    for (at, moved, link) in candidates {
        match covered.get(&at).filter(|_| !moved) {
            None => {
                if judged.insert(at) {
                    links.push((at, true, &link.target));
                }
            }
            Some(&merge) if !merge.contains(link.range) => {
                links.push((at, false, &link.target));
            }
            Some(_) => {}
        }
    }
    let losses: Vec<(CellRef, DiagnosticCode)> = sheet
        .losses
        .iter()
        .filter(|&&(at, _)| !covered.contains_key(&at))
        .map(|&(at, code)| (place(at), code))
        .collect();
    let held: HashSet<CellRef> =
        cells.keys().copied().chain(losses.iter().map(|&(at, _)| at)).collect();
    let hidden_rows_held: HashSet<u32> =
        held.iter().map(|at| at.row).filter(|&r| hidden.row(r)).collect();
    let hidden_cols_held: HashSet<u32> =
        held.iter().map(|at| at.col).filter(|&c| hidden.col(c)).collect();
    let hidden_lines = hidden_rows_held.len().saturating_add(hidden_cols_held.len());
    let hidden_lines = u64::try_from(hidden_lines).unwrap_or(u64::MAX);
    parse_ctx.emit_count(DiagnosticCode::HiddenOmitted, Region::Body, hidden_lines);
    for (at, code) in losses {
        if hidden.shows(at) {
            parse_ctx.emit(code, Region::Body);
        }
    }
    for (at, labelled, target) in links {
        let label =
            cells.get(&at).filter(|_| labelled).map(|inlines| inlines_to_plain_text(inlines));
        let label = label.as_deref().unwrap_or_default();
        let lost = match target {
            LinkTarget::Url(url) => {
                parse_ctx.charge(label.len().saturating_add(url.len()) as u64)?;
                !label_shows_target(label, url)
            }
            LinkTarget::Related(_) | LinkTarget::Unknown => {
                parse_ctx.charge(label.len() as u64)?;
                !label_is_absolute_url(label)
            }
            LinkTarget::Reported => false,
        };
        if lost {
            parse_ctx.emit(DiagnosticCode::LinkTargetUnread, Region::Body);
        }
    }
    Ok(())
}

/// The populated extent over shown cells, widened to the full size of the
/// merges touching it; the rest are dropped, so a crafted merge list can
/// neither force unbounded materialization nor saturate onto (0,0).
fn extent(
    cells: &HashMap<CellRef, Vec<Inline>>,
    hidden: &Hidden,
    merges: &mut Vec<CellRange>,
) -> Option<CellRange> {
    let populated = cells
        .keys()
        .filter(|&&at| hidden.shows(at))
        .map(|&at| CellRange::single(at))
        .reduce(CellRange::union)?;
    merges.retain(|&merge| merge.intersects(populated));
    Some(merges.iter().fold(populated, |extent, &merge| extent.union(merge)))
}

/// The rows and columns a merge origin spans on the shown grid.
#[derive(Clone, Copy)]
struct MergeSpan {
    rows: usize,
    cols: usize,
}

/// Merges remapped onto the shown grid, by grid index: each origin's span
/// and every position an origin covers.
struct MergeLayout {
    origins: HashMap<(usize, usize), MergeSpan>,
    covered: HashSet<(usize, usize)>,
}

impl MergeLayout {
    /// The covered-position set is charged against the expansion budget up
    /// front, before any insertion work, mirroring what placement would
    /// charge.
    fn on(
        limits: &Limits,
        merges: &[CellRange],
        row_map: &[u32],
        col_map: &[u32],
    ) -> Result<MergeLayout, ConvertError> {
        let visible_span = |map: &[u32], lo: u32, hi: u32| {
            let a = map.partition_point(|&x| x < lo);
            let b = map.partition_point(|&x| x <= hi);
            (a, b - a)
        };
        let mut layout = MergeLayout { origins: HashMap::new(), covered: HashSet::new() };
        let mut expansion = 0u64;
        for merge in merges {
            let (r0, rows) = visible_span(row_map, merge.first.row, merge.last.row);
            let (c0, cols) = visible_span(col_map, merge.first.col, merge.last.col);
            if rows * cols <= 1 {
                continue;
            }
            expansion = expansion.saturating_add((rows as u64) * (cols as u64) - 1);
            if expansion > limits.max_expansion {
                return Err(ConvertError::limit(Limit::Expansion, limits.max_expansion));
            }
            layout.origins.insert((r0, c0), MergeSpan { rows, cols });
            for r in r0..r0 + rows {
                for c in c0..c0 + cols {
                    if (r, c) != (r0, c0) {
                        layout.covered.insert((r, c));
                    }
                }
            }
        }
        Ok(layout)
    }
}

/// Place every shown position into the table, merge origins with their
/// spans.
fn assemble(
    limits: &Limits,
    row_map: &[u32],
    col_map: &[u32],
    layout: &MergeLayout,
    mut cells: HashMap<CellRef, Vec<Inline>>,
) -> Result<Option<Table>, ConvertError> {
    let mut builder = GridBuilder::new(limits.max_expansion);
    // A merge is real extent: trailing rows it covers stay in the grid.
    builder.keep_covered_tail();
    for (ri, &row) in row_map.iter().enumerate() {
        builder.next_row();
        for (ci, &col) in col_map.iter().enumerate() {
            if layout.covered.contains(&(ri, ci)) {
                builder.covered();
                continue;
            }
            let cell = match cells.remove(&CellRef::new(row, col)) {
                Some(inlines) => Cell::from_inlines(inlines),
                None => Cell::default(),
            };
            match layout.origins.get(&(ri, ci)) {
                Some(span) => builder.place(Cell::spanning(
                    cell.blocks,
                    span.cols as u32,
                    span.rows as u32,
                ))?,
                None => builder.place(cell)?,
            }
        }
    }
    // A spreadsheet marks no header row, so the shape of the data decides.
    let mut table = builder.finish(TableKind::Data);
    if table.grid.is_empty() {
        return Ok(None);
    }
    table.header_rows = resolve_header_rows(&table, 0);
    Ok(Some(table))
}

/// The positions among `at` that a merge covers without being its origin,
/// which the application does not show, each with a merge that covers it.
/// Each position a merge's range visits is charged.
fn merge_covered(
    parse_ctx: &ParseCtx,
    merges: &[CellRange],
    at: impl Iterator<Item = CellRef>,
) -> Result<HashMap<CellRef, CellRange>, ConvertError> {
    let at: BTreeSet<CellRef> = at.collect();
    let mut covered = HashMap::new();
    for &merge in merges {
        for &cell in at.range(merge.first..=merge.last) {
            parse_ctx.charge(1)?;
            if merge.contains(CellRange::single(cell)) && cell != merge.first {
                covered.insert(cell, merge);
            }
        }
    }
    Ok(covered)
}

/// Flatten inclusive ranges into a sorted, deduplicated coordinate list.
/// Coalescing before expansion bounds the output by the coordinate space,
/// not by the range count.
fn expand_ranges(ranges: &mut [(u32, u32)]) -> Vec<u32> {
    ranges.sort_unstable();
    let mut out = Vec::new();
    let mut next = 0u32;
    for &(a, b) in ranges.iter() {
        out.extend(a.max(next)..=b);
        next = next.max(b.saturating_add(1));
    }
    out
}

/// First coordinate in `lo..=hi` absent from the sorted hidden list. The
/// consecutive hidden run starting at `lo` is measured by binary search, so
/// a long run cannot force a linear scan per query.
fn first_visible(hidden: &[u32], lo: u32, hi: u32) -> Option<u32> {
    let tail = &hidden[hidden.partition_point(|&h| h < lo)..];
    // `tail[j] - j` never decreases, so "the run still holds at j" is a
    // prefix property.
    let (mut a, mut b) = (0usize, tail.len());
    while a < b {
        let mid = (a + b) / 2;
        if tail[mid] == lo + mid as u32 {
            a = mid + 1;
        } else {
            b = mid;
        }
    }
    let first = lo.checked_add(u32::try_from(a).ok()?)?;
    (first <= hi).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATE_ONLY: DateParts = DateParts { date: true, time: false, elapsed: false };

    #[test]
    fn the_fictitious_leap_day_keeps_its_own_value() {
        // Serial 60 is 1900-02-29, a day that never existed, and mapping it
        // onto a real date would make it indistinguishable from serial 59.
        let since1900 = DateSystem::Since1900;
        assert_eq!(render_serial(59.0, DATE_ONLY, since1900), "1900-02-28");
        assert_eq!(render_serial(60.0, DATE_ONLY, since1900), "60");
        assert_eq!(render_serial(61.0, DATE_ONLY, since1900), "1900-03-01");
        // A value late on serial 59 still belongs to its own day.
        assert_eq!(render_serial(59.9999999, DATE_ONLY, since1900), "1900-02-28");
        // The 1904 system has no such day.
        assert_eq!(render_serial(60.0, DATE_ONLY, DateSystem::Since1904), "1904-03-01");
    }

    #[test]
    fn a_sub_day_serial_keeps_the_clock_a_combined_format_names() {
        let both = DateParts { date: true, time: true, elapsed: false };
        assert_eq!(render_serial(0.5, both, DateSystem::Since1900), "12:00:00");
        // A date-only format still has nothing to show but the number.
        assert_eq!(render_serial(0.5, DATE_ONLY, DateSystem::Since1900), "0.5");
    }

    #[test]
    fn literal_text_dates_and_currency_are_more_than_the_number() {
        for code in [
            "[DBNum1]yyyy",
            "abc0",
            "€0.00",
            "0\" kg\"",
            "\\$0",
            "[$€-407]0.0.0",
            "[DBNum1][h]",
            "[MM]0.0.0",
        ] {
            assert!(shows_more_than_its_number(code), "{code}");
        }
        for code in [
            "0.0.0",
            "0;0;0;0;0",
            "[DBNum1]0",
            "0.0E+0",
            "_(0_)",
            "[Red]0",
            "[$-409]0",
            "[$-409]0.0.0",
            "[DBNum1][$-804]General",
        ] {
            assert!(!shows_more_than_its_number(code), "{code}");
        }
    }

    #[test]
    fn every_grid_position_is_charged_once() {
        let sheet = SheetContent {
            cells: HashMap::from([
                (CellRef::new(0, 0), "a".to_string()),
                (CellRef::new(1, 2), "b".to_string()),
            ]),
            ..SheetContent::default()
        };
        let parse_ctx = ParseCtx::default();
        assert!(build_table(&parse_ctx, sheet, &mut 0).unwrap().is_some());
        assert_eq!(parse_ctx.fuel(), 6);
    }

    fn cell(col: u32) -> CellRange {
        CellRange::single(CellRef::new(0, col))
    }

    #[test]
    fn a_link_target_is_charged_per_judged_cell() {
        let target: Rc<str> = format!("https://example.org/{}", "a".repeat(10_000)).into();
        let sheet = || SheetContent {
            cells: (0..100).map(|c| (CellRef::new(0, c), "a".to_string())).collect(),
            links: (0..100)
                .map(|c| SheetLink { range: cell(c), target: LinkTarget::Url(target.clone()) })
                .collect(),
            ..SheetContent::default()
        };
        let limits = Limits { max_work: 2_000_000, ..Default::default() };
        assert!(build_table(&ParseCtx::new(&limits), sheet(), &mut 0).is_ok());
        let limits = Limits { max_work: 500_000, ..Default::default() };
        let err = build_table(&ParseCtx::new(&limits), sheet(), &mut 0).unwrap_err();
        assert!(matches!(err, ConvertError::ResourceLimit { limit: Limit::Work, .. }), "{err:?}");
    }

    #[test]
    fn a_link_label_is_charged_once_per_cell() {
        let sheet = || SheetContent {
            cells: HashMap::from([(CellRef::new(0, 0), "a".repeat(10_000))]),
            links: (0..1_000)
                .map(|_| SheetLink {
                    range: cell(0),
                    target: LinkTarget::Url("https://example.org/".into()),
                })
                .collect(),
            ..SheetContent::default()
        };
        let limits = Limits { max_work: 20_000, ..Default::default() };
        assert!(build_table(&ParseCtx::new(&limits), sheet(), &mut 0).is_ok());
        let limits = Limits { max_work: 5_000, ..Default::default() };
        let err = build_table(&ParseCtx::new(&limits), sheet(), &mut 0).unwrap_err();
        assert!(matches!(err, ConvertError::ResourceLimit { limit: Limit::Work, .. }), "{err:?}");
    }

    fn range(r1: u32, c1: u32, r2: u32, c2: u32) -> CellRange {
        CellRange::spanning(CellRef::new(r1, c1), CellRef::new(r2, c2))
    }

    fn merged(ranges: &[CellRange], grid: GridBounds) -> (Vec<CellRange>, u64) {
        let parse_ctx = ParseCtx::default();
        let mut sheet = SheetContent::default();
        for &r in ranges {
            sheet.merge(&parse_ctx, r, grid);
        }
        let diagnostics = parse_ctx.diagnostics();
        assert!(diagnostics.iter().all(|d| d.code == DiagnosticCode::TableSpanRepaired));
        (sheet.merges, diagnostics.iter().map(|d| d.count).sum())
    }

    #[test]
    fn a_merge_past_the_grid_is_clamped_to_it_and_reported() {
        let past = [range(0, 0, 0, MAX_COLS), range(0, 0, MAX_ROWS, 0), range(2, 2, u32::MAX, 3)];
        let clamped = vec![
            range(0, 0, 0, MAX_COLS - 1),
            range(0, 0, MAX_ROWS - 1, 0),
            range(2, 2, MAX_ROWS - 1, 3),
        ];
        assert_eq!(merged(&past, OOXML_GRID), (clamped, 3));
        let biff8 = super::super::xls::BIFF8_GRID;
        assert_eq!(merged(&[range(0, 0, 0, 300)], biff8), (vec![range(0, 0, 0, 255)], 1));
        let inside = [range(0, 0, 1, MAX_COLS - 1), range(3, 3, 3, 3)];
        assert_eq!(merged(&inside, OOXML_GRID), (vec![inside[0]], 0));
    }

    #[test]
    fn a_merge_outside_the_grid_is_dropped_and_reported() {
        let outside = [range(0, MAX_COLS, 2, MAX_COLS + 3), range(MAX_ROWS, 0, MAX_ROWS + 1, 2)];
        assert_eq!(merged(&outside, OOXML_GRID), (vec![], 2));
        let biff8 = super::super::xls::BIFF8_GRID;
        assert_eq!(merged(&[range(0, 256, 2, 300)], biff8), (vec![], 1));
        assert_eq!(merged(&[range(0, MAX_COLS - 1, 0, MAX_COLS)], OOXML_GRID), (vec![], 1));
    }

    #[test]
    fn a_whitespace_cell_leaves_no_cell() {
        let mut sheet = SheetContent::default();
        sheet.put(CellRef::new(0, 0), "a".to_string());
        sheet.put(CellRef::new(0, 1), " \t\u{a0}".to_string());
        sheet.put(CellRef::new(0, 2), String::new());
        sheet.put(CellRef::new(0, 3), " b ".to_string());
        let kept: HashMap<CellRef, String> =
            HashMap::from([(CellRef::new(0, 0), "a".into()), (CellRef::new(0, 3), " b ".into())]);
        assert_eq!(sheet.cells, kept);
    }

    #[test]
    fn a_shown_cell_below_a_merge_keeps_its_loss() {
        let mut sheet = SheetContent::default();
        let parse_ctx = ParseCtx::default();
        sheet.put(CellRef::new(0, 0), "a".to_string());
        sheet.put(CellRef::new(5, 0), "b".to_string());
        sheet.merge(&parse_ctx, range(0, 0, 1, 1), OOXML_GRID);
        sheet.lose(CellRef::new(5, 0), DiagnosticCode::CellValueUnreadable);
        assert!(build_table(&parse_ctx, sheet, &mut 0).unwrap().is_some());
        let reported: Vec<_> = parse_ctx.diagnostics().iter().map(|d| d.code).collect();
        assert_eq!(reported, [DiagnosticCode::CellValueUnreadable]);
    }

    #[test]
    fn a_shown_cell_beside_a_merge_keeps_its_loss() {
        let mut sheet = SheetContent::default();
        let parse_ctx = ParseCtx::default();
        sheet.put(CellRef::new(0, 0), "a".to_string());
        sheet.put(CellRef::new(0, 5), "b".to_string());
        sheet.merge(&parse_ctx, range(0, 0, 1, 1), OOXML_GRID);
        sheet.lose(CellRef::new(0, 5), DiagnosticCode::CellValueUnreadable);
        assert!(build_table(&parse_ctx, sheet, &mut 0).unwrap().is_some());
        let reported: Vec<_> = parse_ctx.diagnostics().iter().map(|d| d.code).collect();
        assert_eq!(reported, [DiagnosticCode::CellValueUnreadable]);
    }
}
