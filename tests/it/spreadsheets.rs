//! One workbook converts alike from its OpenDocument and Excel forms: the
//! sheets, rows and columns it hides drop, and the merges they cut keep the
//! part that shows.

use crate::build::{REL, odf_styled, relationships};
use crate::support::{converter, reported, zip_package};
use almiladoc::{Format, Input};

/// A sheet's cells by row and column, the rows and columns it hides and its
/// merges as inclusive first row, first column, last row and last column.
struct Sheet<'a> {
    cells: &'a [&'a [&'a str]],
    hidden_rows: &'a [usize],
    hidden_cols: &'a [usize],
    merges: &'a [[usize; 4]],
}

fn cell_ref(row: usize, col: usize) -> String {
    format!("{}{}", char::from(b'A' + u8::try_from(col).unwrap()), row + 1)
}

fn collapse(hidden: bool) -> &'static str {
    if hidden { r#" table:visibility="collapse""# } else { "" }
}

impl Sheet<'_> {
    fn merge_at(&self, row: usize, col: usize) -> Option<[usize; 4]> {
        self.merges
            .iter()
            .copied()
            .find(|m| m[0] <= row && row <= m[2] && m[1] <= col && col <= m[3])
    }

    fn ods(&self, name: &str, style: &str) -> String {
        let width = self.cells.iter().map(|row| row.len()).max().unwrap_or(0);
        let columns: String = (0..width)
            .map(|col| {
                format!("<table:table-column{}/>", collapse(self.hidden_cols.contains(&col)))
            })
            .collect();
        let cell = |row: usize, col: usize, text: &str| {
            let merge = self.merge_at(row, col);
            if merge.is_some_and(|m| (m[0], m[1]) != (row, col)) {
                return "<table:covered-table-cell/>".to_string();
            }
            let span = merge.map_or(String::new(), |m| {
                let (cols, rows) = (m[3] - m[1] + 1, m[2] - m[0] + 1);
                format!(
                    r#" table:number-columns-spanned="{cols}" table:number-rows-spanned="{rows}""#
                )
            });
            let text =
                if text.is_empty() { String::new() } else { format!("<text:p>{text}</text:p>") };
            format!("<table:table-cell{span}>{text}</table:table-cell>")
        };
        let rows: String = self
            .cells
            .iter()
            .enumerate()
            .map(|(row, texts)| {
                let cells: String =
                    texts.iter().enumerate().map(|(col, text)| cell(row, col, text)).collect();
                let hidden = collapse(self.hidden_rows.contains(&row));
                format!("<table:table-row{hidden}>{cells}</table:table-row>")
            })
            .collect();
        format!(r#"<table:table table:name="{name}"{style}>{columns}{rows}</table:table>"#)
    }

    fn xlsx(&self) -> String {
        let cols: String = self
            .hidden_cols
            .iter()
            .map(|col| format!(r#"<col min="{0}" max="{0}" width="9" hidden="1"/>"#, col + 1))
            .collect();
        let cols = if cols.is_empty() { cols } else { format!("<cols>{cols}</cols>") };
        let rows: String = self
            .cells
            .iter()
            .enumerate()
            .map(|(row, texts)| {
                let cells: String = texts
                    .iter()
                    .enumerate()
                    .filter(|(_, text)| !text.is_empty())
                    .map(|(col, text)| {
                        let at = cell_ref(row, col);
                        format!(r#"<c r="{at}" t="inlineStr"><is><t>{text}</t></is></c>"#)
                    })
                    .collect();
                let hidden = if self.hidden_rows.contains(&row) { r#" hidden="1""# } else { "" };
                format!(r#"<row r="{}"{hidden}>{cells}</row>"#, row + 1)
            })
            .collect();
        let merges: String = self
            .merges
            .iter()
            .map(|m| {
                format!(r#"<mergeCell ref="{}:{}"/>"#, cell_ref(m[0], m[1]), cell_ref(m[2], m[3]))
            })
            .collect();
        let merges =
            if merges.is_empty() { merges } else { format!("<mergeCells>{merges}</mergeCells>") };
        format!("{cols}<sheetData>{rows}</sheetData>{merges}")
    }
}

const SML: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

/// A workbook of `sheets`, each with its name, its worksheet body and whether
/// it is hidden.
fn xlsx(sheets: &[(&str, String, bool)]) -> Vec<u8> {
    let listed: String = sheets
        .iter()
        .enumerate()
        .map(|(i, (name, _, hidden))| {
            let state = if *hidden { r#" state="hidden""# } else { "" };
            format!(r#"<sheet name="{name}" sheetId="{0}"{state} r:id="rId{0}"/>"#, i + 1)
        })
        .collect();
    let workbook =
        format!(r#"<workbook xmlns="{SML}" xmlns:r="{REL}"><sheets>{listed}</sheets></workbook>"#);
    let ids: Vec<(String, String)> = (1..=sheets.len())
        .map(|i| (format!("rId{i}"), format!("worksheets/sheet{i}.xml")))
        .collect();
    let targets: Vec<(&str, &str, &str)> =
        ids.iter().map(|(id, target)| (id.as_str(), "worksheet", target.as_str())).collect();
    let mut parts = vec![
        (
            "_rels/.rels".to_string(),
            relationships(&[("rId1", "officeDocument", "xl/workbook.xml")]),
        ),
        ("xl/workbook.xml".to_string(), workbook),
        ("xl/_rels/workbook.xml.rels".to_string(), relationships(&targets)),
    ];
    for (i, (_, body, _)) in sheets.iter().enumerate() {
        parts.push((
            format!("xl/worksheets/sheet{}.xml", i + 1),
            format!(r#"<worksheet xmlns="{SML}">{body}</worksheet>"#),
        ));
    }
    zip_package(&parts)
}

fn ods(tables: &str, styles: &str) -> Vec<u8> {
    odf_styled(styles, &format!("<office:spreadsheet>{tables}</office:spreadsheet>"))
}

type Reported = Vec<(&'static str, Option<u32>, u64)>;

fn converted(bytes: &[u8], format: Format) -> (String, Reported) {
    let conversion = converter(1).convert(Input::new(bytes, format)).unwrap();
    let diagnostics = reported(conversion.diagnostics())
        .into_iter()
        .map(|(code, _, unit, count)| (code.as_str(), unit, count))
        .collect();
    (conversion.into_markdown(), diagnostics)
}

#[test]
fn a_workbook_drops_what_it_hides_alike_from_its_ods_and_xlsx_forms() {
    const GRID: &[&[&str]] = &[
        &["r0c0", "r0c1", "r0c2", "r0c3"],
        &["r1c0", "r1c1", "r1c2", "r1c3"],
        &["r2c0", "r2c1", "r2c2", "r2c3"],
        &["r3c0", "r3c1", "r3c2", "r3c3"],
    ];
    let on_grid = |name, hidden_rows, hidden_cols, merges| {
        (name, Sheet { cells: GRID, hidden_rows, hidden_cols, merges })
    };
    let cases = [
        on_grid("hidden rows and columns", &[1], &[2], &[]),
        on_grid("a hidden run", &[1, 2], &[1, 2], &[]),
        on_grid("a merge over a hidden column", &[], &[1], &[[0, 0, 0, 2]]),
        on_grid("a merge over a hidden row", &[1], &[], &[[0, 1, 2, 1]]),
        on_grid("a merge origin in a hidden column", &[], &[0], &[[1, 0, 1, 1]]),
        on_grid("a merge origin in a hidden row", &[1], &[], &[[1, 1, 2, 2]]),
        on_grid("a merge origin hidden on both axes", &[1], &[1], &[[1, 1, 2, 2]]),
        on_grid("a merge with nothing left to show", &[1, 2], &[], &[[1, 1, 2, 1]]),
        (
            "an empty merge origin in a hidden column",
            Sheet {
                cells: &[&["a", "", "b", "c"], &["d", "e", "f", "g"]],
                hidden_rows: &[],
                hidden_cols: &[1],
                merges: &[[0, 1, 0, 2]],
            },
        ),
    ];
    for (name, sheet) in cases {
        let from_ods = converted(&ods(&sheet.ods("S", ""), ""), Format::Ods);
        let from_xlsx = converted(&xlsx(&[("S", sheet.xlsx(), false)]), Format::Excel);
        assert_eq!(from_ods, from_xlsx, "{name}");
    }
}

#[test]
fn a_workbook_counts_every_hidden_row_it_drops_alike_from_its_ods_and_xlsx_forms() {
    let sheet = Sheet {
        cells: &[&["a", "b"], &["c", "d"], &["e", "f"]],
        hidden_rows: &[1, 2],
        hidden_cols: &[],
        merges: &[],
    };
    let from_ods = converted(&ods(&sheet.ods("S", ""), ""), Format::Ods);
    let from_xlsx = converted(&xlsx(&[("S", sheet.xlsx(), false)]), Format::Excel);
    assert_eq!(from_ods, from_xlsx);
    assert_eq!(from_ods.1, [("hidden_omitted", Some(1), 2)]);
}

#[test]
fn a_workbook_drops_a_hidden_sheet_alike_from_its_ods_and_xlsx_forms() {
    let sheet = |cells| Sheet { cells, hidden_rows: &[], hidden_cols: &[], merges: &[] };
    let (first, second, third) =
        (sheet(&[&["a"], &["b"]]), sheet(&[&["gone"]]), sheet(&[&["c"], &["d"]]));
    let styles = r#"<office:automatic-styles><style:style style:name="Hid" style:family="table"><style:table-properties table:display="false"/></style:style></office:automatic-styles>"#;
    let tables =
        [first.ods("A", ""), second.ods("B", r#" table:style-name="Hid""#), third.ods("C", "")]
            .concat();
    let from_ods = converted(&ods(&tables, styles), Format::Ods);
    let book = [("A", first.xlsx(), false), ("B", second.xlsx(), true), ("C", third.xlsx(), false)];
    assert_eq!(from_ods, converted(&xlsx(&book), Format::Excel));
    assert_eq!(from_ods.1, [("hidden_omitted", Some(2), 1)]);
}
