use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::model::{AnchorId, Block, CellSlot, ImageSource, LinkTarget};
use crate::package::xml::{self, Dialect, Element, Node};
use crate::shared::{
    html::{self, HtmlCtx, Stylesheet},
    tag_scan,
};

fn invalid() -> ConvertError {
    ConvertError::malformed("invalid OCR table markup")
}

struct NoTargets;

impl HtmlCtx for NoTargets {
    fn exact_tables(&self) -> bool {
        true
    }
    fn link_target(&self, _: &str) -> Option<LinkTarget> {
        None
    }

    fn image_source(&self, _: &str) -> Result<ImageSource, ConvertError> {
        Err(invalid())
    }

    fn anchor_id(&self, _: &str) -> AnchorId {
        String::new()
    }
}

pub(crate) fn mentions_table(ctx: &ParseCtx<'_>, raw: &str) -> Result<bool, ConvertError> {
    tag_scan::any_tag_or_end(ctx, raw.as_bytes(), |name, _| name.eq_ignore_ascii_case(b"table"))
}

pub(crate) fn unescaped_table(
    ctx: &ParseCtx<'_>,
    raw: &str,
    prefix: &str,
) -> Result<bool, ConvertError> {
    if !raw.contains('<') {
        return Ok(false);
    }
    ctx.charge(raw.len() as u64)?;
    let mut bytes = raw.as_bytes().to_vec();
    let mut slashes = 0usize;
    for byte in prefix.bytes().rev() {
        ctx.charge(1)?;
        if byte != b'\\' {
            break;
        }
        slashes += 1;
    }
    for byte in &mut bytes {
        if *byte == b'<' && slashes % 2 == 1 {
            *byte = b' ';
        }
        slashes = if *byte == b'\\' { slashes + 1 } else { 0 };
    }
    tag_scan::any_tag_or_end(ctx, &bytes, |name, _| name.eq_ignore_ascii_case(b"table"))
}

pub(crate) fn blocks(ctx: &ParseCtx<'_>, raw: &str) -> Result<Vec<Block>, ConvertError> {
    ctx.charge(raw.len() as u64)?;
    let mut parsed = xml::parse(ctx, raw.as_bytes(), Dialect::Xhtml, &mut xml::WholeTree)?;
    if parsed.marks.unresolved {
        return Err(invalid());
    }
    let mut top = elements(ctx, &mut parsed.root)?;
    let [table] = top.as_mut_slice() else { return Err(invalid()) };
    name(ctx, table, &["table"])?;
    if !table.attrs.is_empty() {
        return Err(invalid());
    }
    let declared = validate(ctx, table)?;
    if declared.is_empty() {
        return Err(invalid());
    }
    let blocks = html::to_blocks(ctx, &parsed.root, &Stylesheet::default(), &NoTargets)?;
    let [Block::Table(table)] = blocks.as_slice() else { return Err(invalid()) };
    let mut actual = Vec::new();
    for row in &table.grid {
        ctx.charge(row.len() as u64 + 1)?;
        for slot in row {
            if let CellSlot::Origin(cell) = slot {
                actual.push((cell.col_span, cell.row_span));
            }
        }
    }
    if declared != actual {
        return Err(invalid());
    }
    Ok(blocks)
}

fn name(ctx: &ParseCtx<'_>, elem: &mut Element, allowed: &[&str]) -> Result<(), ConvertError> {
    ctx.charge(elem.local.len() as u64 + 1)?;
    elem.local.make_ascii_lowercase();
    if elem.ns.is_some() || !allowed.contains(&elem.local.as_str()) {
        return Err(invalid());
    }
    Ok(())
}

fn elements<'a>(
    ctx: &ParseCtx<'_>,
    elem: &'a mut Element,
) -> Result<Vec<&'a mut Element>, ConvertError> {
    let mut children = Vec::new();
    for child in &mut elem.children {
        ctx.charge(1)?;
        match child {
            Node::Elem(child) => children.push(child),
            Node::Text(text) => {
                ctx.charge(text.len() as u64)?;
                if !text.trim().is_empty() {
                    return Err(invalid());
                }
            }
        }
    }
    Ok(children)
}

fn validate(ctx: &ParseCtx<'_>, table: &mut Element) -> Result<Vec<(u32, u32)>, ConvertError> {
    let mut spans = Vec::new();
    for row in elements(ctx, table)? {
        name(ctx, row, &["tr"])?;
        if !row.attrs.is_empty() {
            return Err(invalid());
        }
        for cell in elements(ctx, row)? {
            name(ctx, cell, &["td"])?;
            let mut span = (1, 1);
            let mut seen = 0;
            for attr in &mut cell.attrs {
                ctx.charge((attr.local.len() + attr.value.len()) as u64 + 1)?;
                attr.local.make_ascii_lowercase();
                let (bit, target) = match attr.local.as_str() {
                    "colspan" => (1, &mut span.0),
                    "rowspan" => (2, &mut span.1),
                    _ => return Err(invalid()),
                };
                if attr.ns.is_some() || seen & bit != 0 {
                    return Err(invalid());
                }
                seen |= bit;
                *target = attr.value.parse().ok().filter(|n| *n > 0).ok_or_else(invalid)?;
            }
            for child in &cell.children {
                ctx.charge(1)?;
                let Node::Text(text) = child else { return Err(invalid()) };
                ctx.charge(text.len() as u64)?;
            }
            spans.push(span);
        }
    }
    Ok(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancelToken;
    use crate::limits::{Limit, Limits};
    use crate::model::Inline;
    use crate::shared::markdown;

    const TABLE: &str = concat!(
        "<table><tr><td rowspan=\"2\">Sample</td><td colspan=\"2\">Yield (%)</td></tr>",
        "<tr><td>Run 1</td><td>Run 2</td></tr>",
        "<tr><td>A</td><td>12.4</td><td>13.1</td></tr></table>"
    );

    fn read(text: &str) -> Result<Vec<Block>, ConvertError> {
        markdown::blocks_with_tables(&ParseCtx::default(), text, 0)
    }

    #[test]
    fn covered_rows_and_escaped_cell_text_keep_their_content() {
        for tail in ["<tr></tr>", "<tr/>", "<tr> \n </tr>"] {
            let source = format!(
                "<table><tr><td rowspan=\"2\" colspan=\"2\">1.9 &lt; 2.5 &amp; &lt;b&gt;x&lt;/b&gt;</td></tr>{tail}</table>"
            );
            let parsed = read(&source).unwrap();
            let Block::Table(table) = &parsed[0] else { panic!("not a table") };
            assert_eq!((table.grid.len(), table.grid[1].len()), (2, 2));
            assert_eq!(
                table.origins().next().unwrap().blocks,
                vec![Block::Paragraph(vec![Inline::plain("1.9 < 2.5 & <b>x</b>")])]
            );
        }
        for source in
            ["`<table>`", "```html\n<table>\n```", "\\<table>", "&lt;table&gt;", "<b>literal</b>"]
        {
            assert_eq!(
                read(source).unwrap(),
                markdown::blocks(&ParseCtx::default(), source, 0).unwrap()
            );
        }
    }

    #[test]
    fn malformed_unsupported_or_clamped_tables_never_fall_back_to_text() {
        for source in [
            "> <table><tr><td>x</td></tr></table>",
            "<table",
            "</table>",
            "<table><tr><td>x</tr></table>",
            "<table></table>",
            "<table><tr></tr></table>",
            "<div><table><tr><td>x</td></tr></table></div>",
            "<table><tr><td><img src=\"https://invalid/\"/></td></tr></table>",
            "<table style=\"display:none\"><tr><td>x</td></tr></table>",
            "<table><tr><th rowspan=\"2\">x</th><td>y</td></tr><tr><td>z</td></tr></table>",
            "<table><tr><td onclick=\"bad()\">x</td></tr></table>",
            "<table><tr><td rowspan=\"0\">x</td></tr></table>",
            "<table><tr><td rowspan=\"2\">x</td></tr></table>",
            "<table><tr><td colspan=\"1001\">x</td></tr></table>",
            "<table><tr><td colspan=\"bad\">x</td></tr></table>",
            "<table><tr><td>&unknown;</td></tr></table>",
            "<table><tr><td>x</td><td rowspan=\"2\">y</td></tr><tr><td colspan=\"2\">z</td></tr></table>",
            "<table><tr><td>x</td></tr></table>\nText without a blank line.",
        ] {
            assert!(matches!(read(source), Err(ConvertError::Malformed { .. })), "{source}");
        }
    }

    #[test]
    fn limits_and_cancellation_are_the_callers() {
        let cases = [
            (Limits { max_ocr_text_bytes: 1, ..Limits::default() }, Limit::OcrTextBytes, 0),
            (Limits { max_work: 1, ..Limits::default() }, Limit::Work, 0),
            (Limits { max_xml_nodes: 1, ..Limits::default() }, Limit::XmlNodes, 0),
            (Limits { max_grid_slots: 8, ..Limits::default() }, Limit::GridSlots, 0),
            (Limits { max_expansion: 1, ..Limits::default() }, Limit::Expansion, 0),
            (Limits { max_nesting_depth: 2, ..Limits::default() }, Limit::NestingDepth, 0),
        ];
        for (limits, wanted, depth) in cases {
            let error =
                markdown::blocks_with_tables(&ParseCtx::new(&limits), TABLE, depth).unwrap_err();
            assert!(
                matches!(error, ConvertError::ResourceLimit { limit, .. } if limit == wanted),
                "{wanted}: {error:?}"
            );
        }
        let huge = "<table><tr><td colspan=\"1000\" rowspan=\"65534\">x</td></tr></table>";
        let max = Limits::default().max_expansion;
        assert_eq!(read(huge), Err(ConvertError::limit(Limit::Expansion, max)));
        let limits = Limits { max_work: TABLE.len() as u64, ..Limits::default() };
        assert_eq!(
            blocks(&ParseCtx::new(&limits), TABLE),
            Err(ConvertError::limit(Limit::Work, limits.max_work))
        );
        let cancel = CancelToken::new();
        cancel.cancel();
        assert_eq!(
            markdown::blocks_with_tables(&ParseCtx::default().with_cancel(Some(&cancel)), TABLE, 0),
            Err(ConvertError::Cancelled)
        );
    }

    #[test]
    fn backslash_events_do_not_rescan_their_prefixes() {
        let source = "\\".repeat(100_000);
        let limits = Limits { max_work: 200_000, ..Limits::default() };
        let blocks = markdown::blocks_with_tables(&ParseCtx::new(&limits), &source, 0).unwrap();
        let [Block::Paragraph(content)] = blocks.as_slice() else { panic!("not a paragraph") };
        assert_eq!(crate::model::inlines_to_plain_text(content), "\\".repeat(50_000));
    }

    #[test]
    fn escape_prefix_scans_spend_the_callers_work_and_check_cancellation() {
        let prefix = "\\".repeat(32);
        let limits = Limits { max_work: 31, ..Limits::default() };
        assert_eq!(
            unescaped_table(&ParseCtx::new(&limits), "<table>", &prefix),
            Err(ConvertError::limit(Limit::Work, 31))
        );
        let cancel = CancelToken::new();
        cancel.cancel();
        assert_eq!(
            unescaped_table(&ParseCtx::default().with_cancel(Some(&cancel)), "<table>", &prefix),
            Err(ConvertError::Cancelled)
        );
        for (prefix, expected) in [("\\", false), ("\\\\", true), ("\\a", true), ("a\\", false)] {
            assert_eq!(unescaped_table(&ParseCtx::default(), "<table>", prefix), Ok(expected));
        }
    }
}
