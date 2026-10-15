//! Textual extraction of DrawingML rich objects shared by DOCX and PPTX:
//! charts (title, axis titles, cached series data) and SmartArt diagram
//! data (text points).

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::model::{Block, Cell, Inline, List, ListItem, Style, Table, TableKind};
use crate::package::xml::{Element, ns};
use crate::shared::text::clean_text;

/// A chart part as blocks: bold title paragraph plus a categories x series
/// table built from the cached display strings. A chart whose data or axis
/// titles these blocks cannot carry is reported in `region`.
pub fn chart_blocks(parse_ctx: &ParseCtx, root: &Element, region: Region) -> Vec<Block> {
    let mut blocks = Vec::new();
    let title_elem = root.first_descendant(ns::CHART, "title");
    let title = title_elem.map(|t| clean_text(&drawing_text(t))).filter(|t| !t.trim().is_empty());
    if let Some(title) = title {
        blocks.push(Block::Paragraph(vec![Inline::Text {
            text: title,
            style: Style { bold: true, ..Style::PLAIN },
        }]));
    }
    struct Series {
        name: String,
        values: Vec<String>,
    }
    let mut categories: Vec<String> = Vec::new();
    let mut series: Vec<Series> = Vec::new();
    for ser in root.descendants(ns::CHART, "ser") {
        // Cached display strings only; `c:f` formula references are not text.
        let name = ser
            .find(ns::CHART, "tx")
            .and_then(|t| t.first_descendant(ns::CHART, "v"))
            .map(|v| clean_text(&v.text()))
            .unwrap_or_default();
        let cats: Vec<String> = ser
            .find(ns::CHART, "cat")
            .map(|c| c.descendants(ns::CHART, "v").map(|v| clean_text(&v.text())).collect())
            .unwrap_or_default();
        if categories.is_empty() {
            categories = cats;
        }
        let values: Vec<String> = ser
            .find(ns::CHART, "val")
            .map(|v| v.descendants(ns::CHART, "v").map(|p| clean_text(&p.text())).collect())
            .unwrap_or_default();
        series.push(Series { name, values });
    }
    let tabled = !series.is_empty() && !categories.is_empty();
    let cat_title_elem = root
        .first_descendant(ns::CHART, "catAx")
        .and_then(|ax| ax.find(ns::CHART, "title"))
        .filter(|_| tabled);
    let rendered = [title_elem, cat_title_elem];
    let untitled = root
        .descendant_elems()
        .filter(|e| ["catAx", "valAx", "dateAx", "serAx"].iter().any(|axis| e.is(ns::CHART, axis)))
        .filter_map(|axis| axis.find(ns::CHART, "title"))
        .any(|t| {
            !rendered.iter().flatten().any(|r| std::ptr::eq(*r, t))
                && !drawing_text(t).trim().is_empty()
        });
    let untabled = series.iter().any(|s| s.values.len() > categories.len());
    if untabled || untitled || chart_drops_data(root) {
        parse_ctx.emit(DiagnosticCode::GraphicUnread, region);
    }
    if tabled {
        let cat_title = cat_title_elem.map(|t| clean_text(&drawing_text(t))).unwrap_or_default();
        let mut header: Vec<Cell> = vec![Cell::from_inlines(vec![Inline::plain(cat_title)])];
        header
            .extend(series.iter().map(|s| Cell::from_inlines(vec![Inline::plain(s.name.clone())])));
        let mut rows = vec![header];
        for (i, cat) in categories.iter().enumerate() {
            let mut row = vec![Cell::from_inlines(vec![Inline::plain(cat.clone())])];
            for s in &series {
                let v = s.values.get(i).cloned().unwrap_or_default();
                row.push(Cell::from_inlines(vec![Inline::plain(v)]));
            }
            rows.push(row);
        }
        blocks.push(Block::Table(Table::from_rows(rows, 1, TableKind::Data)));
    }
    blocks
}

fn chart_drops_data(root: &Element) -> bool {
    let unaligned = |data: Option<&Element>| {
        data.is_some_and(|d| {
            d.descendants(ns::CHART, "pt").enumerate().any(|(i, pt)| {
                pt.attr_unqualified("idx").and_then(|idx| idx.parse().ok()) != Some(i)
            })
        })
    };
    root.descendants(ns::CHART, "ser").any(|ser| {
        ["xVal", "yVal", "bubbleSize"].iter().any(|name| ser.find(ns::CHART, name).is_some())
            || unaligned(ser.find(ns::CHART, "cat"))
            || unaligned(ser.find(ns::CHART, "val"))
    })
}

/// A SmartArt data part as a bullet list of its text points in order.
pub fn diagram_blocks(root: &Element) -> Vec<Block> {
    let items: Vec<ListItem> = root
        .descendants(ns::DGM, "pt")
        .filter_map(|pt| {
            let text = clean_text(&pt.find(ns::DGM, "t")?.text());
            if text.trim().is_empty() {
                return None;
            }
            Some(ListItem {
                blocks: vec![Block::Paragraph(vec![Inline::plain(text)])],
                marker_label: None,
            })
        })
        .collect();
    if items.is_empty() {
        return Vec::new();
    }
    vec![Block::List(List { marker: crate::model::MarkerKind::Bullet, start: 1, items })]
}

/// Text runs inside DrawingML rich text (`a:p`/`a:r`/`a:t`), joined.
pub fn drawing_text(elem: &Element) -> String {
    let mut parts: Vec<String> = Vec::new();
    for p in elem.descendants(ns::A, "p") {
        let text = p.text();
        if !text.trim().is_empty() {
            parts.push(text);
        }
    }
    if parts.is_empty() { elem.text() } else { parts.join(" ") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::archive::Role;
    use crate::package::xml::parse_xml;

    fn series(content: &str) -> String {
        format!("<c:ser><c:tx><c:v>Sold</c:v></c:tx>{content}</c:ser>")
    }

    fn points(name: &str, values: &[(usize, &str)]) -> String {
        let points: String = values
            .iter()
            .map(|(i, v)| format!(r#"<c:pt idx="{i}"><c:v>{v}</c:v></c:pt>"#))
            .collect();
        format!("<c:{name}><c:strCache>{points}</c:strCache></c:{name}>")
    }

    fn titled(text: &str) -> String {
        format!(
            "<c:title><c:tx><c:rich><a:p><a:r><a:t>{text}</a:t></a:r></a:p></c:rich></c:tx></c:title>"
        )
    }

    #[test]
    fn a_chart_is_reported_when_its_table_drops_data() {
        let cat = points("cat", &[(0, "Q1"), (1, "Q2")]);
        let val = points("val", &[(0, "10"), (1, "14")]);
        let tabled = series(&format!("{cat}{val}"));
        let axis = |name: &str, title: &str| format!("<c:{name}>{title}</c:{name}>");
        let chart_title = titled("Sales");
        let cases = [
            ("categories and values", series(&format!("{cat}{val}")), false),
            ("scatter values", series(&points("xVal", &[(0, "1")])), true),
            ("bubble sizes", series(&format!("{cat}{val}<c:bubbleSize/>")), true),
            ("values with no categories", series(&val), true),
            (
                "more values than categories",
                series(&format!("{}{val}", points("cat", &[(0, "Q1")]))),
                true,
            ),
            (
                "a point out of position",
                series(&format!("{}{val}", points("cat", &[(0, "Q1"), (2, "Q2")]))),
                true,
            ),
            ("scatter y values alone", series(&points("yVal", &[(0, "1")])), true),
            (
                "a value out of position",
                series(&format!("{cat}{}", points("val", &[(0, "10"), (2, "14")]))),
                true,
            ),
            (
                "a value axis title",
                format!("{chart_title}{tabled}{}", axis("valAx", &titled("Units"))),
                true,
            ),
            (
                "a date axis title",
                format!("{chart_title}{tabled}{}", axis("dateAx", &titled("Month"))),
                true,
            ),
            (
                "a series axis title",
                format!("{chart_title}{tabled}{}", axis("serAx", &titled("Depth"))),
                true,
            ),
            (
                "a second category axis title",
                format!(
                    "{chart_title}{tabled}{}{}",
                    axis("catAx", &titled("Quarter")),
                    axis("catAx", &titled("Half"))
                ),
                true,
            ),
            (
                "a category axis title in the header",
                format!("{chart_title}{tabled}{}", axis("catAx", &titled("Quarter"))),
                false,
            ),
            (
                "a value axis title shown as the chart title",
                format!("{tabled}{}{}", axis("catAx", ""), axis("valAx", &titled("Units"))),
                false,
            ),
            (
                "an empty value axis title",
                format!("{chart_title}{tabled}{}", axis("valAx", "<c:title/>")),
                false,
            ),
        ];
        for (case, content, expected) in cases {
            let xml = format!(
                r#"<c:chartSpace xmlns:c="{}" xmlns:a="{}">{content}</c:chartSpace>"#,
                ns::CHART,
                ns::A
            );
            let parse_ctx = ParseCtx::default();
            let root = parse_xml(&parse_ctx, xml.as_bytes(), Role::Reported(Region::Note)).unwrap();
            chart_blocks(&parse_ctx, &root, Region::Note);
            let reported: Vec<_> =
                parse_ctx.diagnostics().iter().map(|d| (d.code, d.region, d.count)).collect();
            let expected: Vec<_> = expected
                .then_some((DiagnosticCode::GraphicUnread, Region::Note, 1))
                .into_iter()
                .collect();
            assert_eq!(reported, expected, "{case}");
        }
    }
}
