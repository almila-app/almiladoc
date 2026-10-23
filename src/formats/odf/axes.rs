//! The rows and columns of an ODF table in source order, and which of them a
//! sheet hides: `table:visibility` on a row or column and `table:display` on
//! the groups around them, and the default cell style its columns name, in a
//! table of any kind. All are read before any cell is, so a hidden cell is
//! decoded only where a merge it opens shows.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::formats::odf::styles::{named, odf_boolean, token};
use crate::limits::Limit;
use crate::package::xml::{Element, ns};
use crate::shared::hidden_runs::HiddenRuns;
use std::ops::Range;

/// Whether a table drops the rows and columns it hides.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Visibility {
    /// Any table but a sheet, which keeps every line.
    Ignored,
    /// A sheet.
    Applied,
}

impl Visibility {
    fn hides_group(self, group: &Element) -> bool {
        self == Self::Applied
            && group.attr(ns::TABLE, "display").and_then(odf_boolean) == Some(false)
    }

    fn hides_line(self, line: &Element) -> bool {
        self == Self::Applied
            && line
                .attr(ns::TABLE, "visibility")
                .is_some_and(|value| matches!(token(value), "collapse" | "filter"))
    }
}

/// One step through a table's row containers, in document order.
pub(super) enum Step<'e> {
    Shapes(&'e Element),
    /// The start and end of the `table:table-header-rows` a table holds
    /// directly.
    HeaderOpen,
    HeaderClose,
    Row {
        row: &'e Element,
        repeat: u64,
        hidden: bool,
    },
}

/// The repeat count of a row, column or cell, at least 1.
pub(super) fn repeats(elem: &Element, attr: &str) -> u64 {
    elem.attr(ns::TABLE, attr).and_then(|v| v.parse().ok()).unwrap_or(1).max(1)
}

pub(super) fn row_steps(table: &Element, visibility: Visibility) -> Vec<Step<'_>> {
    let mut steps = Vec::new();
    collect_steps(table, visibility, false, Level::Table, &mut steps);
    steps
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Table,
    Group,
}

fn collect_steps<'e>(
    container: &'e Element,
    visibility: Visibility,
    hidden: bool,
    level: Level,
    steps: &mut Vec<Step<'e>>,
) {
    for child in container.child_elems() {
        if child.ns.as_deref() != Some(ns::TABLE) {
            continue;
        }
        match child.local.as_str() {
            "table-header-rows" => {
                if level == Level::Table {
                    steps.push(Step::HeaderOpen);
                }
                collect_steps(child, visibility, hidden, Level::Group, steps);
                if level == Level::Table {
                    steps.push(Step::HeaderClose);
                }
            }
            "table-rows" => collect_steps(child, visibility, hidden, Level::Group, steps),
            "table-row-group" => {
                let hidden = hidden || visibility.hides_group(child);
                collect_steps(child, visibility, hidden, Level::Group, steps);
            }
            "shapes" => steps.extend(child.child_elems().map(Step::Shapes)),
            "table-row" => steps.push(Step::Row {
                row: child,
                repeat: repeats(child, "number-rows-repeated"),
                hidden: hidden || visibility.hides_line(child),
            }),
            _ => {}
        }
    }
}

/// The source rows and columns a sheet hides, and the default cell style its columns name.
#[derive(Debug, Default)]
pub(super) struct Axes<'e> {
    pub(super) rows: HiddenRuns,
    pub(super) cols: HiddenRuns,
    /// The `table:default-cell-style-name` of each run of columns that names one, in column order.
    pub(super) col_styles: Vec<(Range<u64>, &'e str)>,
}

impl<'e> Axes<'e> {
    /// Survey a table's rows and columns, charging each one declared; a
    /// table that ignores visibility surveys only its columns' default styles.
    pub(super) fn of(
        table: &'e Element,
        steps: &[Step<'_>],
        visibility: Visibility,
        parse_ctx: &ParseCtx<'_>,
    ) -> Result<Self, ConvertError> {
        let mut hidden_rows = Vec::new();
        let mut at = 0u64;
        let surveyed = if visibility == Visibility::Applied { steps } else { &[] };
        for step in surveyed {
            if let Step::Row { repeat, hidden, .. } = step {
                parse_ctx.charge(1)?;
                let end = declared_end(parse_ctx, at, *repeat)?;
                if *hidden {
                    hidden_rows.push(at..end);
                }
                at = end;
            }
        }
        let mut columns = Columns { visibility, at: 0, hidden: Vec::new(), styled: Vec::new() };
        survey_columns(table, false, &mut columns, parse_ctx)?;
        Ok(Self {
            rows: HiddenRuns::of(hidden_rows),
            cols: HiddenRuns::of(columns.hidden),
            col_styles: columns.styled,
        })
    }

    /// The runs of columns that name a default cell style and overlap `cols`, in column order.
    pub(super) fn column_styles(&self, cols: &Range<u64>) -> &[(Range<u64>, &'e str)] {
        let from = self.col_styles.partition_point(|(run, _)| run.end <= cols.start);
        let to = self.col_styles.partition_point(|(run, _)| run.start < cols.end);
        self.col_styles.get(from..to).unwrap_or_default()
    }
}

struct Columns<'e> {
    visibility: Visibility,
    at: u64,
    hidden: Vec<Range<u64>>,
    styled: Vec<(Range<u64>, &'e str)>,
}

fn survey_columns<'e>(
    container: &'e Element,
    hidden: bool,
    columns: &mut Columns<'e>,
    parse_ctx: &ParseCtx<'_>,
) -> Result<(), ConvertError> {
    for child in container.child_elems() {
        if child.ns.as_deref() != Some(ns::TABLE) {
            continue;
        }
        match child.local.as_str() {
            "table-column" => {
                parse_ctx.charge(1)?;
                let end =
                    declared_end(parse_ctx, columns.at, repeats(child, "number-columns-repeated"))?;
                let run = columns.at..end;
                columns.at = end;
                if let Some(style) = named(child.attr(ns::TABLE, "default-cell-style-name")) {
                    columns.styled.push((run.clone(), style));
                }
                if hidden || columns.visibility.hides_line(child) {
                    columns.hidden.push(run);
                }
            }
            "table-columns" | "table-header-columns" => {
                survey_columns(child, hidden, columns, parse_ctx)?;
            }
            "table-column-group" => {
                let hidden = hidden || columns.visibility.hides_group(child);
                survey_columns(child, hidden, columns, parse_ctx)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// The coordinate after `count` more lines from `at`, refused as an
/// expansion once it passes what a coordinate can count.
pub(super) fn declared_end(
    parse_ctx: &ParseCtx<'_>,
    at: u64,
    count: u64,
) -> Result<u64, ConvertError> {
    at.checked_add(count)
        .ok_or_else(|| ConvertError::limit(Limit::Expansion, parse_ctx.limits().max_expansion))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::CancelToken;
    use crate::limits::Limits;
    use crate::package::archive::Role;
    use crate::package::xml::parse_xml;
    use crate::shared::test_support::cancelled;
    use std::assert_matches;

    fn with_table<R>(body: &str, read: impl FnOnce(&Element) -> R) -> R {
        let xml = format!(
            r#"<table:table xmlns:table="{}" xmlns:draw="{}">{body}</table:table>"#,
            ns::TABLE,
            ns::DRAW
        );
        let doc = parse_xml(&ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap();
        read(doc.find(ns::TABLE, "table").unwrap())
    }

    #[derive(Debug, PartialEq)]
    struct Surveyed {
        rows: Vec<(u64, u64)>,
        cols: Vec<(u64, u64)>,
        styles: Vec<(Range<u64>, String)>,
    }

    fn survey(body: &str, parse_ctx: &ParseCtx<'_>) -> Result<Surveyed, ConvertError> {
        with_table(body, |table| {
            let steps = row_steps(table, Visibility::Applied);
            let axes = Axes::of(table, &steps, Visibility::Applied, parse_ctx)?;
            Ok(Surveyed {
                rows: hidden(&axes.rows),
                cols: hidden(&axes.cols),
                styles: axes
                    .col_styles
                    .iter()
                    .map(|(run, name)| (run.clone(), name.to_string()))
                    .collect(),
            })
        })
    }

    fn hidden(runs: &HiddenRuns) -> Vec<(u64, u64)> {
        runs.hidden_in(0..u64::MAX).map(|run| (run.start, run.end)).collect()
    }

    const COLLAPSED: &str = r#"table:visibility="collapse""#;

    #[test]
    fn a_line_hides_by_its_own_visibility_or_a_group_around_it_and_a_group_never_reopens_one() {
        let columns = format!(
            r#"<table:table-column/><table:table-column {COLLAPSED} table:number-columns-repeated="2"/>
            <table:table-column-group table:display="false"><table:table-column/><table:table-column-group table:display="true"><table:table-column table:number-columns-repeated="3"/></table:table-column-group></table:table-column-group>
            <table:table-columns><table:table-column table:visibility="filter"/></table:table-columns>
            <table:table-header-columns><table:table-column table:visibility="visible"/></table:table-header-columns>"#
        );
        let rows = format!(
            r#"<table:table-row table:number-rows-repeated="3"/>
            <table:table-row {COLLAPSED}/>
            <table:table-row-group table:display="false"><table:table-row-group><table:table-row table:number-rows-repeated="2"/></table:table-row-group><table:table-rows><table:table-row/></table:table-rows></table:table-row-group>
            <table:table-header-rows><table:table-row table:visibility=" filter "/></table:table-header-rows>
            <table:table-row table:visibility="Collapse"/>"#
        );
        let axes = survey(&format!("{columns}{rows}"), &ParseCtx::default()).unwrap();
        assert_eq!(axes.cols, [(1, 8)]);
        assert_eq!(axes.rows, [(3, 8)]);
    }

    #[test]
    fn a_table_that_ignores_visibility_hides_nothing_and_surveys_only_its_columns_styles() {
        let body = format!(
            r#"<table:table-column {COLLAPSED} table:default-cell-style-name="a"/><table:table-column-group table:display="false"><table:table-column table:default-cell-style-name="b"/></table:table-column-group><table:table-row {COLLAPSED}/><table:table-row-group table:display="false"><table:table-row/></table:table-row-group>"#
        );
        with_table(&body, |table| {
            let steps = row_steps(table, Visibility::Ignored);
            assert_eq!(steps.len(), 2);
            assert!(steps.iter().all(|step| !matches!(step, Step::Row { hidden: true, .. })));
            let parse_ctx = ParseCtx::default();
            let axes = Axes::of(table, &steps, Visibility::Ignored, &parse_ctx).unwrap();
            let styles: Vec<_> =
                axes.col_styles.iter().map(|(run, name)| (run.clone(), *name)).collect();
            assert_eq!(
                (hidden(&axes.rows), hidden(&axes.cols), styles, parse_ctx.fuel()),
                (vec![], vec![], vec![(0..1, "a"), (1..2, "b")], 2)
            );
        });
    }

    #[test]
    fn the_survey_reads_the_default_cell_style_each_run_of_columns_names() {
        let columns = r#"<table:table-column/>
            <table:table-column table:default-cell-style-name="a" table:number-columns-repeated="2"/>
            <table:table-column table:default-cell-style-name=""/>
            <table:table-columns><table:table-column table:default-cell-style-name="b"/></table:table-columns>
            <table:table-header-columns><table:table-column table:default-cell-style-name="c"/></table:table-header-columns>
            <table:table-column-group table:display="false"><table:table-column table:default-cell-style-name="d" table:visibility="collapse"/></table:table-column-group>"#;
        let surveyed = survey(columns, &ParseCtx::default()).unwrap();
        let owned = |runs: &[(Range<u64>, &str)]| -> Vec<(Range<u64>, String)> {
            runs.iter().map(|(run, name)| (run.clone(), name.to_string())).collect()
        };
        assert_eq!(surveyed.styles, owned(&[(1..3, "a"), (4..5, "b"), (5..6, "c"), (6..7, "d")]));
        assert_eq!(surveyed.cols, [(6, 7)]);
    }

    #[test]
    fn the_column_runs_a_cell_covers_are_those_that_overlap_it() {
        let columns = r#"<table:table-column table:default-cell-style-name="a"/>
            <table:table-column/>
            <table:table-column table:default-cell-style-name="b" table:number-columns-repeated="3"/>
            <table:table-column table:default-cell-style-name="c"/>"#;
        with_table(columns, |table| {
            let axes = Axes::of(table, &[], Visibility::Applied, &ParseCtx::default()).unwrap();
            let names = |cols: Range<u64>| -> Vec<&str> {
                axes.column_styles(&cols).iter().map(|(_, name)| *name).collect()
            };
            for (label, cols, expected) in [
                ("before every run", 1..2, vec![]),
                ("the first run alone", 0..1, vec!["a"]),
                ("across a gap", 0..3, vec!["a", "b"]),
                ("inside a run", 3..4, vec!["b"]),
                ("a run's end and the run after", 4..6, vec!["b", "c"]),
                ("one column past the last", 6..7, vec![]),
                ("every column", 0..10, vec!["a", "b", "c"]),
                ("reversed", Range { start: 5, end: 0 }, vec![]),
            ] {
                assert_eq!(names(cols), expected, "{label}");
            }
        });
    }

    #[test]
    fn the_steps_of_a_table_keep_document_order_and_mark_only_its_own_header_rows() {
        let body = r#"<table:shapes><draw:frame/></table:shapes>
            <table:table-header-rows><table:table-row table:number-rows-repeated="2"/></table:table-header-rows>
            <table:table-row-group><table:table-header-rows><table:table-row/></table:table-header-rows></table:table-row-group>
            <table:table-row table:number-rows-repeated="0"/>"#;
        let steps: Vec<String> = with_table(body, |table| {
            row_steps(table, Visibility::Applied)
                .iter()
                .map(|step| match step {
                    Step::Shapes(_) => "shape".to_string(),
                    Step::HeaderOpen => "[".to_string(),
                    Step::HeaderClose => "]".to_string(),
                    Step::Row { repeat, hidden, .. } => {
                        format!("{repeat}{}", if *hidden { "h" } else { "" })
                    }
                })
                .collect()
        });
        assert_eq!(steps, ["shape", "[", "2", "]", "1", "1"]);
    }

    #[test]
    fn the_survey_charges_each_declared_row_and_column_and_stops_at_max_work_and_cancellation() {
        let rows = "<table:table-row/>".repeat(40);
        let columns = "<table:table-columns><table:table-column/></table:table-columns>".repeat(25);
        let both = format!("{columns}{rows}");
        let parse_ctx = ParseCtx::default();
        survey(&both, &parse_ctx).unwrap();
        assert_eq!(parse_ctx.fuel(), 65);
        let exactly = |body: &str, max_work| {
            let limits = Limits { max_work, ..Limits::default() };
            survey(body, &ParseCtx::new(&limits)).map(drop)
        };
        for (body, work) in [(&both, 65), (&rows, 40), (&columns, 25)] {
            exactly(body, work).unwrap();
            assert_matches!(
                exactly(body, work - 1),
                Err(ConvertError::ResourceLimit { limit: Limit::Work, max }) if max == work - 1,
                "{work}"
            );
        }
        let token = CancelToken::new();
        let cancelled = cancelled(&token);
        assert_matches!(survey(&rows, &cancelled).map(drop), Err(ConvertError::Cancelled));
        assert_matches!(survey(&columns, &cancelled).map(drop), Err(ConvertError::Cancelled));
    }

    #[test]
    fn positions_declared_past_what_a_coordinate_counts_are_refused_as_an_expansion() {
        let max = Limits::default().max_expansion;
        let parse_ctx = ParseCtx::default();
        assert_eq!(declared_end(&parse_ctx, 0, u64::MAX).unwrap(), u64::MAX);
        assert_eq!(declared_end(&parse_ctx, u64::MAX - 3, 3).unwrap(), u64::MAX);
        assert_matches!(
            declared_end(&parse_ctx, u64::MAX, 1),
            Err(ConvertError::ResourceLimit { limit: Limit::Expansion, max: m }) if m == max
        );
        let most = u64::MAX;
        for (name, body) in [
            (
                "rows",
                format!(
                    r#"<table:table-row table:number-rows-repeated="{most}"/><table:table-row/>"#
                ),
            ),
            (
                "columns",
                format!(
                    r#"<table:table-column table:number-columns-repeated="{most}"/><table:table-column/>"#
                ),
            ),
        ] {
            assert_matches!(
                survey(&body, &parse_ctx).map(drop),
                Err(ConvertError::ResourceLimit { limit: Limit::Expansion, .. }),
                "{name}"
            );
        }
    }
}
