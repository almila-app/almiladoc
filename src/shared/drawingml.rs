//! Textual extraction of DrawingML rich objects shared by DOCX and PPTX:
//! charts (title, axis titles, cached series data) and SmartArt diagram
//! data (text points).

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Block, Cell, Inline, List, ListItem, Style, Table, TableKind};
use crate::package::xml::{Element, ns};
use crate::shared::text::clean_text;

const AXES: [&str; 4] = ["catAx", "valAx", "dateAx", "serAx"];

/// A chart part as blocks: bold title paragraph plus a categories x series
/// table built from the cached display strings, its positions admitted to
/// the parse's grid slots before it is built. A chart whose data or axis
/// titles these blocks cannot carry is reported in `region`.
pub fn chart_blocks(
    parse_ctx: &ParseCtx,
    root: &Element,
    region: Region,
) -> Result<Vec<Block>, ConvertError> {
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
    let mut nested = false;
    for ser in root.outermost(ns::CHART, "ser") {
        nested |= ser.outermost(ns::CHART, "ser").next().is_some();
        // Cached display strings only; `c:f` formula references are not text.
        let name = ser
            .find(ns::CHART, "tx")
            .and_then(|t| t.first_descendant(ns::CHART, "v"))
            .map(|v| clean_text(&v.text()))
            .unwrap_or_default();
        let cats = ser.find(ns::CHART, "cat").map(cached).unwrap_or_default();
        if categories.is_empty() {
            categories = cats;
        }
        let values = ser.find(ns::CHART, "val").map(cached).unwrap_or_default();
        series.push(Series { name, values });
    }
    let tabled = !series.is_empty() && !categories.is_empty();
    let cat_title_elem = root
        .first_descendant(ns::CHART, "catAx")
        .and_then(|ax| ax.find(ns::CHART, "title"))
        .filter(|_| tabled);
    let untitled = axis_title_unshown(root, [title_elem, cat_title_elem]);
    let untabled = series.iter().any(|s| s.values.len() > categories.len());
    if nested || untabled || untitled || chart_drops_data(root) {
        parse_ctx.emit(DiagnosticCode::GraphicUnread, region);
    }
    if tabled {
        let count = |len: usize| u64::try_from(len).unwrap_or(u64::MAX).saturating_add(1);
        let positions = count(categories.len()).saturating_mul(count(series.len()));
        parse_ctx.admit_grid_positions(positions)?;
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
    Ok(blocks)
}

fn cached(data: &Element) -> Vec<String> {
    data.outermost(ns::CHART, "v").map(|v| clean_text(&v.text())).collect()
}

/// Whether an axis title other than the `rendered` ones shows text. A title
/// inside such a title shows only text the outer one holds, so it is never
/// walked again.
fn axis_title_unshown(root: &Element, rendered: [Option<&Element>; 2]) -> bool {
    let mut stack = vec![root];
    while let Some(elem) = stack.pop() {
        let title = AXES
            .iter()
            .any(|axis| elem.is(ns::CHART, axis))
            .then(|| elem.find(ns::CHART, "title"))
            .flatten()
            .filter(|t| !rendered.iter().flatten().any(|r| std::ptr::eq(*r, *t)));
        if title.is_some_and(|t| !drawing_text(t).trim().is_empty()) {
            return true;
        }
        stack.extend(elem.child_elems().filter(|c| !title.is_some_and(|t| std::ptr::eq(*c, t))));
    }
    false
}

fn chart_drops_data(root: &Element) -> bool {
    let unaligned = |data: Option<&Element>| {
        data.is_some_and(|d| {
            d.outermost(ns::CHART, "pt").enumerate().any(|(i, pt)| {
                pt.attr_unqualified("idx").and_then(|idx| idx.parse().ok()) != Some(i)
                    || pt.outermost(ns::CHART, "pt").next().is_some()
            })
        })
    };
    root.outermost(ns::CHART, "ser").any(|ser| {
        ["xVal", "yVal", "bubbleSize"].iter().any(|name| ser.find(ns::CHART, name).is_some())
            || unaligned(ser.find(ns::CHART, "cat"))
            || unaligned(ser.find(ns::CHART, "val"))
    })
}

/// A SmartArt data part as a bullet list of its text points in order; a
/// point inside another's text shows there, not again as an item.
pub fn diagram_blocks(root: &Element) -> Vec<Block> {
    let mut items: Vec<ListItem> = Vec::new();
    let mut stack: Vec<&Element> = root.child_elems().collect();
    stack.reverse();
    while let Some(elem) = stack.pop() {
        let text = elem.is(ns::DGM, "pt").then(|| elem.find(ns::DGM, "t")).flatten();
        if let Some(t) = text.map(|t| clean_text(&t.text())).filter(|t| !t.trim().is_empty()) {
            items.push(ListItem {
                blocks: vec![Block::Paragraph(vec![Inline::plain(t)])],
                marker_label: None,
            });
        }
        let start = stack.len();
        stack.extend(elem.child_elems().filter(|c| !text.is_some_and(|t| std::ptr::eq(*c, t))));
        if let Some(children) = stack.get_mut(start..) {
            children.reverse();
        }
    }
    if items.is_empty() {
        return Vec::new();
    }
    vec![Block::List(List { marker: crate::model::MarkerKind::Bullet, start: 1, items })]
}

/// Text runs inside DrawingML rich text (`a:p`/`a:r`/`a:t`), joined; a
/// paragraph inside another shows with it.
pub fn drawing_text(elem: &Element) -> String {
    let mut parts: Vec<String> = Vec::new();
    for p in elem.outermost(ns::A, "p") {
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
    use crate::limits::Limit;
    use crate::package::archive::Role;
    use crate::package::xml::parse_xml;

    fn chart(parse_ctx: &ParseCtx, content: &str) -> Element {
        let xml = format!(
            r#"<c:chartSpace xmlns:c="{}" xmlns:a="{}">{content}</c:chartSpace>"#,
            ns::CHART,
            ns::A
        );
        parse_xml(parse_ctx, xml.as_bytes(), Role::Reported(Region::Note)).unwrap()
    }

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
            ("a series nested in a series", series(&format!("{cat}{val}{tabled}")), true),
            (
                "a point nested in a point",
                series(&format!(
                    r#"{cat}<c:val><c:numCache><c:pt idx="0"><c:v>10</c:v><c:pt idx="1"><c:v>14</c:v></c:pt></c:pt></c:numCache></c:val>"#
                )),
                true,
            ),
            (
                "a value axis title whose text is only a formula",
                format!(
                    "{chart_title}{tabled}{}",
                    axis(
                        "valAx",
                        "<c:title><c:tx><c:strRef><c:f>Sheet1!B1</c:f></c:strRef></c:tx></c:title>"
                    )
                ),
                true,
            ),
            (
                "an empty value axis title",
                format!("{chart_title}{tabled}{}", axis("valAx", "<c:title/>")),
                false,
            ),
        ];
        for (case, content, expected) in cases {
            let parse_ctx = ParseCtx::default();
            let root = chart(&parse_ctx, &content);
            chart_blocks(&parse_ctx, &root, Region::Note).unwrap();
            let reported: Vec<_> =
                parse_ctx.diagnostics().iter().map(|d| (d.code, d.region, d.count)).collect();
            let expected: Vec<_> = expected
                .then_some((DiagnosticCode::GraphicUnread, Region::Note, 1))
                .into_iter()
                .collect();
            assert_eq!(reported, expected, "{case}");
        }
    }

    #[test]
    fn a_title_whose_paragraphs_nest_shows_their_text_once() {
        let nested = "<a:p><a:p><a:p><a:r><a:t>Sales</a:t></a:r></a:p></a:p></a:p>";
        let title = format!("<c:title><c:tx><c:rich>{nested}</c:rich></c:tx></c:title>");
        let parse_ctx = ParseCtx::default();
        let blocks = chart_blocks(&parse_ctx, &chart(&parse_ctx, &title), Region::Body).unwrap();
        let [Block::Paragraph(title)] = blocks.as_slice() else {
            panic!("{blocks:?}");
        };
        assert_eq!(crate::model::inlines_to_plain_text(title), "Sales");
    }

    #[test]
    fn a_chart_past_the_grid_slots_is_refused_before_its_table_is_built() {
        let points = |name: &str, count: usize| {
            let points: String =
                (0..count).map(|i| format!(r#"<c:pt idx="{i}"><c:v>{i}</c:v></c:pt>"#)).collect();
            format!("<c:{name}><c:strCache>{points}</c:strCache></c:{name}>")
        };
        let size = 20_000;
        let first = format!("<c:ser>{}{}</c:ser>", points("cat", size), points("val", 1));
        let rest = format!("<c:ser>{}</c:ser>", points("val", 1)).repeat(size - 1);
        let parse_ctx = ParseCtx::default();
        let root = chart(&parse_ctx, &format!("{first}{rest}"));
        let refused = chart_blocks(&parse_ctx, &root, Region::Body);
        assert!(
            matches!(
                refused,
                Err(ConvertError::ResourceLimit { limit: Limit::GridSlots, max: 4_000_000 })
            ),
            "{refused:?}"
        );
        let positions = u64::try_from((size + 1) * (size + 1)).unwrap();
        assert!(parse_ctx.fuel() < positions, "{}", parse_ctx.fuel());
    }

    #[test]
    fn a_point_or_value_inside_another_shows_once() {
        let xml = format!(
            r#"<dgm:dataModel xmlns:dgm="{}"><dgm:ptLst><dgm:pt><dgm:t>a<dgm:pt><dgm:t>b</dgm:t></dgm:pt></dgm:t></dgm:pt><dgm:pt><dgm:t>c</dgm:t></dgm:pt></dgm:ptLst></dgm:dataModel>"#,
            ns::DGM
        );
        let parse_ctx = ParseCtx::default();
        let root = parse_xml(&parse_ctx, xml.as_bytes(), Role::Reported(Region::Body)).unwrap();
        let blocks = diagram_blocks(&root);
        let [Block::List(list)] = blocks.as_slice() else {
            panic!("{blocks:?}");
        };
        let items: Vec<_> = list.items.iter().map(|item| format!("{:?}", item.blocks)).collect();
        assert_eq!(items.len(), 2, "{items:?}");
        assert!(items[0].contains("\"ab\"") && items[1].contains("\"c\""), "{items:?}");
        let value = r#"<c:v>1<c:v>2</c:v></c:v>"#;
        let cat =
            format!(r#"<c:cat><c:strCache><c:pt idx="0">{value}</c:pt></c:strCache></c:cat>"#);
        let val =
            format!(r#"<c:val><c:numCache><c:pt idx="0">{value}</c:pt></c:numCache></c:val>"#);
        let blocks = chart_blocks(
            &parse_ctx,
            &chart(&parse_ctx, &series(&format!("{cat}{val}"))),
            Region::Body,
        )
        .unwrap();
        let [Block::Table(table)] = blocks.as_slice() else {
            panic!("{blocks:?}");
        };
        assert_eq!(table.grid.len(), 2);
        assert!(parse_ctx.diagnostics().is_empty());
    }
}
