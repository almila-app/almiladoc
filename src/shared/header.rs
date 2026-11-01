//! Pagination and table-style flags do not identify column labels, so header detection uses the data shape.

use crate::model::{Block, CellSlot, Table, inlines_to_plain_text};
use std::collections::{HashMap, HashSet};

/// A header shows in the first screenful of data, so a full pass over a million-row export buys nothing.
const SAMPLE_ROWS: usize = 50;

/// Stray `N/A` and footnote markers are common enough that exact agreement would silence otherwise obvious columns.
const DOMINANCE_NUM: usize = 9;
const DOMINANCE_DEN: usize = 10;

const MAX_LABEL: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    Bool,
    Date,
    Text,
}

/// A grid's [`Table::header_rows`]: what the format `declared`, or - when it declared none - 1 if the first row labels the columns and 0 otherwise.
pub(crate) fn resolve_header_rows(table: &Table, declared: usize) -> usize {
    if declared > 0 {
        return declared.min(table.grid.len());
    }
    let grid = &table.grid;
    if grid.len() < 2 {
        return 0;
    }
    if grid[0]
        .iter()
        .any(|s| !matches!(s, CellSlot::Origin(c) if c.col_span == 1 && c.row_span == 1))
    {
        return 0;
    }
    let sample = &grid[1..grid.len().min(SAMPLE_ROWS + 1)];
    if grid[0].len() != modal_width(sample) {
        return 0;
    }

    let head: Vec<String> = grid[0].iter().map(slot_text).collect();
    let body: Vec<Vec<String>> =
        sample.iter().map(|row| row.iter().map(slot_text).collect()).collect();
    let content_width =
        |row: &Vec<String>| row.iter().rposition(|v| !v.trim().is_empty()).map_or(0, |i| i + 1);
    let width = std::iter::once(&head).chain(body.iter()).map(content_width).max().unwrap_or(0);
    if width == 0 || width > head.len() || !labels_fit(&head, width) {
        return 0;
    }

    match column_votes(&head, &body, width).verdict() {
        Some(is_header) => usize::from(is_header),
        None => usize::from(head.iter().take(width).all(|v| v.chars().count() <= MAX_LABEL)),
    }
}

fn labels_fit(head: &[String], width: usize) -> bool {
    let mut seen: HashSet<String> = HashSet::new();
    head.iter().enumerate().take(width).all(|(c, value)| {
        if value.trim().is_empty() {
            return c == 0;
        }
        !value.contains('\n') && seen.insert(fold(value))
    })
}

#[derive(Default)]
struct Votes {
    header: usize,
    data: usize,
    repeated: usize,
}

impl Votes {
    fn verdict(&self) -> Option<bool> {
        (self.header + self.data + self.repeated > 0)
            .then(|| self.header > self.data && self.header >= self.data + self.repeated)
    }
}

fn column_votes(head: &[String], body: &[Vec<String>], width: usize) -> Votes {
    let mut votes = Votes::default();
    for (c, label) in head.iter().enumerate().take(width) {
        let values: Vec<&str> = body
            .iter()
            .filter_map(|row| row.get(c))
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .collect();
        if values.is_empty() {
            continue;
        }
        let label = label.trim();
        match dominant_kind(&values) {
            Some(kind) if kind != Kind::Text => {
                if classify(label) == Some(Kind::Text) {
                    votes.header += 1;
                } else {
                    votes.data += 1;
                }
            }
            _ => {
                if values.iter().any(|v| fold(v) == fold(label)) {
                    votes.repeated += 1;
                }
            }
        }
    }
    votes
}

/// Frequency ties break toward the wider shape so the result never depends on hash-map iteration order.
fn modal_width(rows: &[Vec<CellSlot>]) -> usize {
    let mut tally: HashMap<usize, usize> = HashMap::new();
    for row in rows {
        *tally.entry(row.len()).or_default() += 1;
    }
    tally.into_iter().max_by_key(|&(width, n)| (n, width)).map_or(0, |(width, _)| width)
}

fn slot_text(slot: &CellSlot) -> String {
    let CellSlot::Origin(cell) = slot else { return String::new() };
    let mut out = String::new();
    for block in &cell.blocks {
        let Block::Paragraph(inlines) = block else { return String::new() };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&inlines_to_plain_text(inlines));
    }
    out
}

fn fold(value: &str) -> String {
    value.trim().to_lowercase()
}

/// Values are non-empty, so they all classify.
fn dominant_kind(values: &[&str]) -> Option<Kind> {
    [Kind::Number, Kind::Bool, Kind::Date, Kind::Text].into_iter().find(|&kind| {
        let n = values.iter().filter(|v| classify(v) == Some(kind)).count();
        n * DOMINANCE_DEN >= values.len() * DOMINANCE_NUM
    })
}

fn classify(value: &str) -> Option<Kind> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Some(if is_number(value) {
        Kind::Number
    } else if matches!(fold(value).as_str(), "true" | "false" | "yes" | "no") {
        Kind::Bool
    } else if is_temporal(value) {
        Kind::Date
    } else {
        Kind::Text
    })
}

fn is_number(value: &str) -> bool {
    let value = value.strip_suffix('%').unwrap_or(value);
    let cleaned: String =
        value.chars().filter(|c| !matches!(c, ',' | ' ' | '_' | '\u{a0}')).collect();
    cleaned.chars().any(|c| c.is_ascii_digit()) && cleaned.parse::<f64>().is_ok()
}

/// Month names stay `Text`: a column of them is a column of labels, and it would cast the same vote either way.
fn is_temporal(value: &str) -> bool {
    let (date, time) = value.split_once(['T', ' ']).unwrap_or((value, ""));
    let time = time.trim_end_matches('Z').split('.').next().unwrap_or_default();
    let is_clock = |t: &str| matches!(digit_groups(t, &[':']), 2 | 3);
    if digit_groups(date, &['-', '/', '.']) == 3 {
        return time.is_empty() || is_clock(time);
    }
    time.is_empty() && is_clock(date)
}

fn digit_groups(value: &str, seps: &[char]) -> usize {
    let Some(sep) = seps.iter().copied().find(|&s| value.contains(s)) else { return 0 };
    let short_digits =
        |p: &str| (1..=4).contains(&p.len()) && p.chars().all(|c| c.is_ascii_digit());
    value.split(sep).try_fold(0, |n, p| short_digits(p).then_some(n + 1)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cell, Inline};

    fn detect(rows: &[&[&str]]) -> usize {
        let rows: Vec<Vec<Cell>> = rows
            .iter()
            .map(|r| r.iter().map(|t| Cell::from_inlines(vec![Inline::plain(*t)])).collect())
            .collect();
        resolve_header_rows(&Table::from_rows(rows, 0), 0)
    }

    #[test]
    fn labels_over_typed_columns_are_a_header() {
        assert_eq!(detect(&[&["name", "qty"], &["a", "1"], &["b", "2"]]), 1);
        assert_eq!(
            detect(&[&["when", "ok"], &["2026-03-15", "TRUE"], &["2026-03-16", "FALSE"]]),
            1
        );
    }

    #[test]
    fn a_first_data_row_is_not_a_header() {
        assert_eq!(
            detect(&[&["36", "12", "aka"], &["173", "57", "aka"], &["306", "220", "aka"]]),
            0
        );
    }

    #[test]
    fn a_label_repeated_below_is_data() {
        assert_eq!(detect(&[&["x", "aka"], &["y", "aka"], &["z", "aka"]]), 0);
        assert_eq!(
            detect(&[
                &["Male", "Group A", "—"],
                &["Male", "Group A", "34"],
                &["Female", "Group B", "29"]
            ]),
            0
        );
        assert_eq!(
            detect(&[
                &["TR", "Ankara", "f", "n/a"],
                &["TR", "Izmir", "f", "3"],
                &["DE", "Berlin", "m", "4"],
                &["TR", "Bursa", "f", "5"]
            ]),
            0
        );
    }

    #[test]
    fn all_text_falls_back_to_row_shape() {
        assert_eq!(detect(&[&["col1", "col2"], &["naïve", "café"], &["Αθήνα", "数据"]]), 1);
        let long = "l".repeat(MAX_LABEL + 1);
        assert_eq!(detect(&[&[&long, "col2"], &["a", "b"], &["c", "d"]]), 0);
    }

    #[test]
    fn structural_vetoes() {
        assert_eq!(detect(&[&["Q1 report"], &["a", "1"], &["b", "2"]]), 0);
        assert_eq!(detect(&[&["name", ""], &["a", "1"], &["b", "2"]]), 0);
        assert_eq!(detect(&[&["name", "name"], &["a", "1"], &["b", "2"]]), 0);
        assert_eq!(detect(&[&["name", "qty"]]), 0);
        assert_eq!(detect(&[&["name", ""], &["a", "x"], &["b", "y"]]), 0);
        assert_eq!(detect(&[&["na\nme", "qty"], &["a", "1"], &["b", "2"]]), 0);
        assert_eq!(detect(&[&["name", "qty"], &["a", "1"], &["b", "2"], &["c", "3", "more"]]), 0);
    }

    #[test]
    fn a_tied_vote_is_not_a_header() {
        assert_eq!(detect(&[&["name", "7"], &["1", "2"], &["2", "3"]]), 0);
    }

    #[test]
    fn a_body_cell_repeating_its_label_does_not_outvote_typed_columns() {
        let head = ["city", "region", "pop"];
        let cases = [
            [head, ["city", "north", "100"], ["Lyon", "south", "200"]],
            [head, ["Paris", "north", "100"], ["Lyon", "region", "200"]],
            [head, ["Paris", "north", "100"], ["Lyon", "REGION", "200"]],
            [head, ["Paris", "north", "100"], ["Lyon", "ReGiOn", "200"]],
            [head, ["Paris", "north", "100"], ["Lyon", "  region  ", "200"]],
            [head, ["Paris", "region", "100"], ["Lyon", "south", "200"]],
            [["city", "Status", "pop"], ["Paris", "active", "100"], ["Lyon", "status", "200"]],
            [head, ["Paris", "north", "100"], ["Lyon", "south", "200"]],
        ];
        for [labels, first, second] in cases {
            assert_eq!(detect(&[&labels, &first, &second]), 1, "{labels:?} {first:?} {second:?}");
        }
    }

    #[test]
    fn a_leading_index_column_may_be_unlabelled() {
        assert_eq!(detect(&[&["", "qty"], &["a", "1"], &["b", "2"]]), 1);
    }

    #[test]
    fn a_mixed_column_abstains_rather_than_voting_against() {
        assert_eq!(
            detect(&[&["kind", "value"], &["percent", "15.5%"], &["date", "2026-03-15"]]),
            1
        );
    }

    #[test]
    fn value_kinds() {
        assert_eq!(classify("1,234.56"), Some(Kind::Number));
        assert_eq!(classify("-1.5e3"), Some(Kind::Number));
        assert_eq!(classify("15.5%"), Some(Kind::Number));
        assert_eq!(classify("-0-"), Some(Kind::Text));
        assert_eq!(classify("NaN"), Some(Kind::Text));
        assert_eq!(classify("2026-03-15"), Some(Kind::Date));
        assert_eq!(classify("15/03/2026 09:04"), Some(Kind::Date));
        assert_eq!(classify("2026-03-15T09:04:54.123Z"), Some(Kind::Date));
        assert_eq!(classify("26:30:15"), Some(Kind::Date));
        assert_eq!(classify("20260-03-15"), Some(Kind::Text));
        assert_eq!(classify("2026--15"), Some(Kind::Text));
        assert_eq!(classify("yes"), Some(Kind::Bool));
        assert_eq!(classify("  "), None);
    }
}
