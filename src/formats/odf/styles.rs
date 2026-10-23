//! ODF style resolution: text/paragraph style chains as tri-state deltas
//! (so `font-weight: normal` can clear inherited bold), plus list styles
//! with per-level marker kinds and start values, and the number style a
//! cell style names.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::formats::odf::datastyle::{Costs, FormatCost, Shown};
use crate::model::VerticalPosition;
use crate::package::xml::{Element, ns};
use crate::shared::blockstyle::{self, BlockStyle};
use crate::shared::chain::{Memo, StyleChains};
use crate::shared::delta::StyleDelta;
use crate::shared::list::MarkerKind;
use crate::shared::tag_scan::{self, Attributes};
use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::HashMap;

pub(super) const LIST_LEVELS: usize = 10;
const CELL_FAMILY: &str = "table-cell";

#[derive(Debug, Clone)]
pub(super) struct ListLevel {
    pub marker: MarkerKind,
    pub start: u64,
    /// `style:num-prefix` literal text before the number.
    pub prefix: String,
    /// `style:num-suffix` literal text after the number; absent means the
    /// bare number is displayed.
    pub suffix: Option<String>,
    /// `text:display-levels`: how many levels (ending at this one) display,
    /// joined by `.`.
    pub display_levels: usize,
}

impl Default for ListLevel {
    fn default() -> Self {
        Self {
            marker: MarkerKind::Bullet,
            start: 1,
            prefix: String::new(),
            suffix: None,
            display_levels: 1,
        }
    }
}

impl ListLevel {
    /// The level's number text as the shared pattern IR, referencing the
    /// trailing `display_levels` levels.
    pub(super) fn pattern(&self, depth: usize) -> crate::shared::numbering::NumberPattern {
        use crate::shared::numbering::{NumberPattern, NumberText};
        let mut text = Vec::new();
        if !self.prefix.is_empty() {
            text.push(NumberText::Literal(self.prefix.clone()));
        }
        let shown = self.display_levels.clamp(1, depth + 1);
        for (i, level) in (depth + 1 - shown..=depth).enumerate() {
            if i > 0 {
                text.push(NumberText::Literal(".".into()));
            }
            text.push(NumberText::Level(u8::try_from(level).unwrap_or(u8::MAX)));
        }
        if let Some(suffix) = &self.suffix
            && !suffix.is_empty()
        {
            text.push(NumberText::Literal(suffix.clone()));
        }
        NumberPattern { text, legal: false }
    }
}

#[derive(Default)]
pub(super) struct OdfStyles<'a> {
    /// The `style:style` chains of each `style:family`.
    families: HashMap<&'a str, Family<'a>>,
    list_styles: HashMap<&'a str, [ListLevel; LIST_LEVELS]>,
    /// `text:outline-style`: heading numbering per outline level.
    outline: Option<[ListLevel; LIST_LEVELS]>,
    /// `style:default-style` per family: the base beneath every named chain
    /// (and the delta for unstyled content of that family).
    defaults: HashMap<&'a str, StyleDelta>,
    /// `table:display` of the `table` family's `style:default-style`.
    default_table_display: Option<bool>,
    /// `style:master-page` by name, and the document's first, which a page
    /// naming none uses.
    masters: HashMap<&'a str, &'a Element>,
    first_master: Option<&'a Element>,
    /// The `number:*-style` data styles by name, from either part, as indices into `data`.
    data_names: HashMap<&'a str, usize>,
    data: Vec<DataStyle<'a>>,
    /// Whether the styles part did not read where it held formatting alone: a cell style or
    /// data style that no collected part holds may have been in it.
    styles_unread: bool,
}

/// A data style and its cost, worked out the first time a cell needs it.
struct DataStyle<'a> {
    style: &'a Element,
    costs: Cell<Option<Costs>>,
}

#[derive(Default)]
struct Family<'a> {
    chains: StyleChains<'a, Element>,
    deltas: Memo<'a, StyleDelta>,
    blocks: Memo<'a, Option<BlockStyle>>,
    master_names: Memo<'a, Option<&'a str>>,
    table_displays: Memo<'a, Option<bool>>,
    data_styles: Memo<'a, Option<DataRef>>,
}

/// The nearest data style a cell style's chain names, or the first parent before one that no
/// collected part holds.
#[derive(Clone, Copy)]
enum DataRef {
    Collected(usize),
    Nameless,
    Uncollected,
}

impl<'a> OdfStyles<'a> {
    pub(super) fn mark_unread(&mut self) {
        self.styles_unread = true;
    }

    /// Collect styles from one document tree (`styles.xml` or `content.xml`).
    /// Call for styles.xml first so content.xml definitions chain onto it.
    pub(super) fn collect(&mut self, tree: &'a Element) {
        for section in tree.child_elems().flat_map(Element::child_elems) {
            if section.is(ns::OFFICE, "master-styles") {
                for master in section.find_all(ns::STYLE, "master-page") {
                    self.first_master.get_or_insert(master);
                    if let Some(name) = master.attr(ns::STYLE, "name") {
                        self.masters.entry(name).or_insert(master);
                    }
                }
            } else if section.is(ns::OFFICE, "automatic-styles") || section.is(ns::OFFICE, "styles")
            {
                for style in section.child_elems() {
                    self.collect_style(style);
                }
            }
        }
    }

    fn collect_style(&mut self, style: &'a Element) {
        match (style.ns.as_deref(), style.local.as_str()) {
            (Some(ns::STYLE), "default-style") => {
                if let Some(family) = style.attr(ns::STYLE, "family") {
                    self.defaults.insert(family, text_properties_delta(style));
                    if family == "table" {
                        self.default_table_display = table_display(style);
                    }
                }
            }
            (Some(ns::STYLE), "style") => {
                if let Some(name) = style.attr(ns::STYLE, "name") {
                    let family = style.attr(ns::STYLE, "family").unwrap_or("");
                    let parent = style.attr(ns::STYLE, "parent-style-name");
                    self.families.entry(family).or_default().chains.insert(name, style, parent);
                }
            }
            (Some(ns::TEXT), "list-style") => {
                if let Some(name) = style.attr(ns::STYLE, "name") {
                    self.list_styles.insert(name, parse_list_style(style));
                }
            }
            (Some(ns::TEXT), "outline-style") => self.outline = Some(parse_list_style(style)),
            (Some(ns::NUMBER), local) if local.ends_with("-style") => {
                if let Some(name) = style.attr(ns::STYLE, "name") {
                    self.data_names.insert(name, self.data.len());
                    self.data.push(DataStyle { style, costs: Cell::new(None) });
                }
            }
            _ => {}
        }
    }

    /// Cumulative delta of a style through its `parent-style-name` chain,
    /// over the family's `style:default-style` base; each style is resolved
    /// once and charged one unit when it is.
    pub(super) fn delta(
        &self,
        parse_ctx: &ParseCtx<'_>,
        family: &str,
        name: &str,
    ) -> Result<StyleDelta, ConvertError> {
        let base = self.defaults.get(family).copied().unwrap_or_default();
        let Some(styles) = self.families.get(family) else {
            return Ok(base);
        };
        styles.chains.resolve(
            parse_ctx,
            &styles.deltas,
            name,
            base,
            |_| false,
            |def, inherited| inherited.merge(text_properties_delta(def)),
        )
    }

    /// The nearest value `own` finds up `name`'s chain in `family`, child first: each style is
    /// resolved into the memo `memo` picks and charged one unit once; a cycle fails `Malformed`.
    fn nearest<T: Clone>(
        &self,
        parse_ctx: &ParseCtx<'_>,
        family: &str,
        name: &str,
        memo: impl for<'f> Fn(&'f Family<'a>) -> &'f Memo<'a, Option<T>>,
        own: impl Fn(&'a Element) -> Option<T>,
    ) -> Result<Option<T>, ConvertError> {
        let Some(styles) = self.families.get(family) else {
            return Ok(None);
        };
        styles.chains.nearest(parse_ctx, memo(styles), name, own)
    }

    /// The block container a paragraph style names, walking
    /// `parent-style-name`: an automatic style carries the name on the
    /// named style it derives from.
    pub(super) fn block_style(
        &self,
        parse_ctx: &ParseCtx<'_>,
        name: &str,
    ) -> Result<Option<BlockStyle>, ConvertError> {
        self.nearest(
            parse_ctx,
            "paragraph",
            name,
            |styles| &styles.blocks,
            |def| def.attr(ns::STYLE, "name").and_then(blockstyle::from_style_name),
        )
    }

    pub(super) fn list_level(&self, style_name: &str, depth: usize) -> (MarkerKind, u64) {
        self.list_styles
            .get(style_name)
            .and_then(|levels| levels.get(depth))
            .map_or((MarkerKind::Bullet, 1), |level| (level.marker, level.start))
    }

    pub(super) fn list_levels(&self, style_name: &str) -> Option<&[ListLevel; LIST_LEVELS]> {
        self.list_styles.get(style_name)
    }

    pub(super) fn outline_levels(&self) -> Option<&[ListLevel; LIST_LEVELS]> {
        self.outline.as_ref()
    }

    pub(super) fn style(&self, family: &str, name: &str) -> Option<&'a Element> {
        self.families.get(family)?.chains.definition(name)
    }

    /// The master page a style starts, by name, through its
    /// `parent-style-name` chain; an empty name starts none.
    pub(super) fn style_master_page(
        &self,
        parse_ctx: &ParseCtx<'_>,
        family: &str,
        name: &str,
    ) -> Result<Option<&'a str>, ConvertError> {
        let master = self.nearest(
            parse_ctx,
            family,
            name,
            |styles| &styles.master_names,
            |def| def.attr(ns::STYLE, "master-page-name"),
        )?;
        Ok(master.filter(|name| !name.is_empty()))
    }

    /// Whether a sheet shows: the `table:display` of its table style or of the
    /// nearest parent that sets one, else the family default, else shown.
    pub(super) fn table_shown(
        &self,
        parse_ctx: &ParseCtx<'_>,
        style: Option<&str>,
    ) -> Result<bool, ConvertError> {
        let display = match style {
            Some(name) => self.nearest(
                parse_ctx,
                "table",
                name,
                |styles| &styles.table_displays,
                table_display,
            )?,
            None => None,
        };
        Ok(display.or(self.default_table_display).unwrap_or(true))
    }

    pub(super) fn master_page(&self, name: &str) -> Option<&'a Element> {
        self.masters.get(name).copied()
    }

    /// What showing a value without the number style `style` names costs: the nearest
    /// `style:data-style-name` up its chain, each style and data style resolved and charged once,
    /// and each data style name looked up once per style that names it. A style, parent or data
    /// style that no collected part holds costs `Unread` once the styles part did not read.
    pub(super) fn format_cost(
        &self,
        parse_ctx: &ParseCtx<'_>,
        style: &str,
        shown: Shown,
    ) -> Result<FormatCost, ConvertError> {
        if self.style(CELL_FAMILY, style).is_none() {
            return Ok(self.uncollected_cost());
        }
        let found = self.nearest(
            parse_ctx,
            CELL_FAMILY,
            style,
            |styles| &styles.data_styles,
            |def| self.data_ref(def),
        )?;
        let data = match found {
            Some(DataRef::Collected(at)) => self.data.get(at),
            Some(DataRef::Uncollected) => return Ok(self.uncollected_cost()),
            Some(DataRef::Nameless) | None => None,
        };
        let Some(data) = data else {
            return Ok(FormatCost::None);
        };
        let costs = if let Some(costs) = data.costs.get() {
            costs
        } else {
            parse_ctx.charge(1)?;
            let costs = Costs::of(data.style);
            data.costs.set(Some(costs));
            costs
        };
        Ok(costs.under(shown))
    }

    fn data_ref(&self, def: &Element) -> Option<DataRef> {
        if let Some(name) = def.attr(ns::STYLE, "data-style-name") {
            return Some(match self.data_names.get(name) {
                Some(&at) => DataRef::Collected(at),
                None if name.is_empty() => DataRef::Nameless,
                None => DataRef::Uncollected,
            });
        }
        let parent = named(def.attr(ns::STYLE, "parent-style-name"))?;
        self.style(CELL_FAMILY, parent).is_none().then_some(DataRef::Uncollected)
    }

    fn uncollected_cost(&self) -> FormatCost {
        if self.styles_unread { FormatCost::Unread } else { FormatCost::None }
    }

    pub(super) fn first_master_page(&self) -> Option<&'a Element> {
        self.first_master
    }

    /// The named master page, else the document's first for an absent or
    /// empty name.
    pub(super) fn master_or_first(&self, name: Option<&str>) -> Option<&'a Element> {
        match name {
            Some(name) if !name.is_empty() => self.master_page(name),
            _ => self.first_master,
        }
    }
}

/// A token of an enumerated or boolean attribute: its value without the XML
/// white space around it.
pub(super) fn token(value: &str) -> &str {
    value.trim_matches([' ', '\t', '\r', '\n'])
}

/// A style name that names one: `styleNameRef` allows the empty string for none.
pub(super) fn named(name: Option<&str>) -> Option<&str> {
    name.filter(|name| !name.is_empty())
}

/// An ODF boolean: `true` or `false` and nothing else.
pub(super) fn odf_boolean(value: &str) -> Option<bool> {
    token(value).parse().ok()
}

fn table_display(style: &Element) -> Option<bool> {
    odf_boolean(style.find(ns::STYLE, "table-properties")?.attr(ns::TABLE, "display")?)
}

/// Clamp an untrusted start value so counters can never overflow.
pub(super) fn parse_start(v: &str) -> Option<u64> {
    v.parse::<u64>().ok().map(|n| n.min(u64::from(u32::MAX)))
}

/// Levels of a `text:list-style` or `text:outline-style` (the level
/// elements differ in name, not in shape).
fn parse_list_style(style: &Element) -> [ListLevel; LIST_LEVELS] {
    let mut levels: [ListLevel; LIST_LEVELS] = std::array::from_fn(|_| ListLevel::default());
    for lvl in style.child_elems() {
        let Some(n) = lvl.attr(ns::TEXT, "level").and_then(|v| v.parse::<usize>().ok()) else {
            continue;
        };
        if !(1..=LIST_LEVELS).contains(&n) {
            continue;
        }
        let slot = &mut levels[n - 1];
        if lvl.is(ns::TEXT, "list-level-style-number") || lvl.is(ns::TEXT, "outline-level-style") {
            slot.marker = match lvl.attr(ns::STYLE, "num-format") {
                Some("a") => MarkerKind::LowerAlpha,
                Some("A") => MarkerKind::UpperAlpha,
                Some("i") => MarkerKind::LowerRoman,
                Some("I") => MarkerKind::UpperRoman,
                Some("") => MarkerKind::Bullet,
                _ => MarkerKind::Decimal,
            };
            slot.start = lvl.attr(ns::TEXT, "start-value").and_then(parse_start).unwrap_or(1);
            slot.prefix = lvl.attr(ns::STYLE, "num-prefix").unwrap_or_default().to_string();
            slot.suffix = lvl.attr(ns::STYLE, "num-suffix").map(str::to_string);
            slot.display_levels =
                lvl.attr(ns::TEXT, "display-levels").and_then(|v| v.parse().ok()).unwrap_or(1);
        }
    }
    levels
}

/// Delta carried by a style's `style:text-properties`.
pub(super) fn text_properties_delta(elem: &Element) -> StyleDelta {
    let Some(props) = elem.find(ns::STYLE, "text-properties") else {
        return StyleDelta::default();
    };
    StyleDelta {
        bold: props
            .attr(ns::FO, "font-weight")
            .map(|w| w == "bold" || w.parse::<u32>().is_ok_and(|n| n >= 600)),
        italic: props.attr(ns::FO, "font-style").map(|s| s == "italic" || s == "oblique"),
        strike: props.attr(ns::STYLE, "text-line-through-style").map(|lt| lt != "none"),
        code: None,
        position: props.attr(ns::STYLE, "text-position").and_then(text_position),
        hidden: props.attr(ns::TEXT, "display").and_then(text_hidden),
    }
}

fn text_hidden(display: &str) -> Option<bool> {
    match display {
        "none" => Some(true),
        // ODF 1.3 Part 3 20.427: `condition` defers to `text:condition`,
        // which is never evaluated.
        "true" | "condition" => Some(false),
        _ => None,
    }
}

/// Whether styles text that does not parse sets a `display` that hides a sheet or text, or one
/// the scan cannot judge: an entity, an attribute list that stops reading.
pub(super) fn sets_hiding(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<bool, ConvertError> {
    tag_scan::any_tag(parse_ctx, bytes, enables_hiding)
}

fn enables_hiding(name: &[u8], mut attributes: Attributes<'_>) -> bool {
    let hides: fn(&str) -> bool = match tag_scan::local_name(name) {
        b"table-properties" => |display| odf_boolean(display) == Some(false),
        b"text-properties" => |display| text_hidden(display) == Some(true),
        _ => return false,
    };
    attributes.by_ref().any(|(attribute, value)| {
        tag_scan::local_name(attribute) == b"display" && display_hides(value, hides)
    }) || attributes.stopped()
}

fn display_hides(value: &[u8], hides: fn(&str) -> bool) -> bool {
    let Ok(display) = std::str::from_utf8(value) else { return true };
    value.contains(&b'&') || hides(display)
}

/// The shift a `style:text-position` names by its first token: `super`, `sub`, or a
/// percentage of the font height whose sign raises or lowers.
fn text_position(value: &str) -> Option<VerticalPosition> {
    match value.split_whitespace().next()? {
        "super" => Some(VerticalPosition::Superscript),
        "sub" => Some(VerticalPosition::Subscript),
        token => {
            let percent = token.strip_suffix('%')?.parse::<f64>().ok().filter(|p| p.is_finite())?;
            Some(match percent.partial_cmp(&0.0)? {
                Ordering::Greater => VerticalPosition::Superscript,
                Ordering::Less => VerticalPosition::Subscript,
                Ordering::Equal => VerticalPosition::Baseline,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::CancelToken;
    use crate::limits::{Limit, Limits};
    use crate::package::archive::Role;
    use crate::package::xml::parse_xml;
    use crate::shared::test_support::cancelled;
    use std::assert_matches;

    fn tree(styles: &str) -> Element {
        let xml = format!(
            r#"<office:document-styles xmlns:office="{}" xmlns:style="{}"><office:styles>{styles}</office:styles></office:document-styles>"#,
            ns::OFFICE,
            ns::STYLE
        );
        parse_xml(&ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap()
    }

    fn chain(depth: usize) -> String {
        family_chain("paragraph", depth)
    }

    fn family_chain(family: &str, depth: usize) -> String {
        (0..depth)
            .map(|i| match i.checked_sub(1) {
                Some(parent) => format!(
                    r#"<style:style style:name="s{i}" style:family="{family}" style:parent-style-name="s{parent}"/>"#
                ),
                None => format!(r#"<style:style style:name="s0" style:family="{family}"/>"#),
            })
            .collect()
    }

    #[test]
    fn a_resolved_ancestor_ends_the_walk() {
        let tree = tree(&chain(4));
        let mut styles = OdfStyles::default();
        styles.collect(&tree);
        let bold = StyleDelta { bold: Some(true), ..StyleDelta::default() };
        let paragraph = &styles.families["paragraph"];
        paragraph.deltas.borrow_mut().insert("s1", bold);
        paragraph.blocks.borrow_mut().insert("s1", Some(BlockStyle::Quote));
        let parse_ctx = ParseCtx::default();
        assert_eq!(styles.delta(&parse_ctx, "paragraph", "s3").unwrap(), bold);
        assert_eq!(styles.block_style(&parse_ctx, "s3").unwrap(), Some(BlockStyle::Quote));
        assert_eq!(parse_ctx.fuel(), 4);
        assert_eq!(styles.delta(&parse_ctx, "paragraph", "s0").unwrap(), StyleDelta::default());
        assert_eq!(styles.block_style(&parse_ctx, "s0").unwrap(), None);
    }

    #[test]
    fn every_style_of_a_chain_is_resolved_once() {
        let tree = tree(&chain(4000));
        let mut styles = OdfStyles::default();
        styles.collect(&tree);
        let parse_ctx = ParseCtx::default();
        for i in (0..4000).rev() {
            let name = format!("s{i}");
            assert_eq!(
                styles.delta(&parse_ctx, "paragraph", &name).unwrap(),
                StyleDelta::default()
            );
            assert_eq!(styles.block_style(&parse_ctx, &name).unwrap(), None);
            assert_eq!(styles.style_master_page(&parse_ctx, "paragraph", &name).unwrap(), None);
        }
        let paragraph = &styles.families["paragraph"];
        let memos = [&paragraph.deltas.borrow().len(), &paragraph.blocks.borrow().len()];
        assert_eq!(memos, [&4000, &4000]);
        assert_eq!(paragraph.master_names.borrow().len(), 4000);
        assert_eq!(parse_ctx.fuel(), 12000);
    }

    #[test]
    fn resolving_a_chain_stops_at_max_work_and_at_cancellation() {
        let tree = tree(&chain(4000));
        let mut styles = OdfStyles::default();
        styles.collect(&tree);
        let limits = Limits { max_work: 3999, ..Limits::default() };
        let lowered = ParseCtx::new(&limits);
        let token = CancelToken::new();
        let cancelled = cancelled(&token);
        let resolve = |parse_ctx: &ParseCtx<'_>, name: &str| {
            [
                styles.delta(parse_ctx, "paragraph", name).map(drop),
                styles.block_style(parse_ctx, name).map(drop),
                styles.style_master_page(parse_ctx, "paragraph", name).map(drop),
            ]
        };
        for refused in resolve(&lowered, "s3999") {
            assert_matches!(
                refused,
                Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 3999 })
            );
        }
        for refused in resolve(&cancelled, "s0") {
            assert_matches!(refused, Err(ConvertError::Cancelled));
        }
    }

    #[test]
    fn text_display_hides_on_none_shows_on_true_or_condition_and_else_inherits() {
        let cases = [
            (r#"text:display="none""#, Some(true)),
            (r#"text:display="true""#, Some(false)),
            (r#"text:display="condition" text:condition="ooow:x""#, Some(false)),
            (r#"text:display="None""#, None),
            (r#"text:display="""#, None),
            ("", None),
        ];
        for (attrs, hidden) in cases {
            let tree = tree(&format!(
                r#"<style:style style:name="s" style:family="text"><style:text-properties xmlns:text="{}" {attrs}/></style:style>"#,
                ns::TEXT
            ));
            let mut styles = OdfStyles::default();
            styles.collect(&tree);
            let delta = styles.delta(&ParseCtx::default(), "text", "s").unwrap();
            assert_eq!(delta, StyleDelta { hidden, ..StyleDelta::default() }, "{attrs}");
        }
    }

    #[test]
    fn a_text_position_shifts_by_its_first_token_and_its_percentage_sign() {
        use crate::model::VerticalPosition::{Baseline, Subscript, Superscript};
        let cases = [
            ("super 58%", Some(Superscript)),
            ("sub 58%", Some(Subscript)),
            ("33% 58%", Some(Superscript)),
            ("-33% 58%", Some(Subscript)),
            ("-0.5%", Some(Subscript)),
            ("0%", Some(Baseline)),
            ("0% 100%", Some(Baseline)),
            ("58% super", Some(Superscript)),
            ("abc", None),
            ("Super", None),
            ("33", None),
            ("inf%", None),
            ("NaN%", None),
            ("", None),
        ];
        for (value, position) in cases {
            let tree = tree(&format!(
                r#"<style:style style:name="s" style:family="text"><style:text-properties style:text-position="{value}"/></style:style>"#
            ));
            let mut styles = OdfStyles::default();
            styles.collect(&tree);
            let delta = styles.delta(&ParseCtx::default(), "text", "s").unwrap();
            assert_eq!(delta, StyleDelta { position, ..StyleDelta::default() }, "{value:?}");
        }
    }

    #[test]
    fn only_a_referenced_cycle_is_malformed() {
        let tree = tree(
            r#"<style:style style:name="a" style:family="paragraph" style:parent-style-name="b"/>
            <style:style style:name="b" style:family="paragraph" style:parent-style-name="a"/>
            <style:style style:name="c" style:family="paragraph"/>"#,
        );
        let mut styles = OdfStyles::default();
        styles.collect(&tree);
        let parse_ctx = ParseCtx::default();
        styles.delta(&parse_ctx, "paragraph", "c").unwrap();
        let cyclic = styles.delta(&parse_ctx, "paragraph", "a");
        assert_matches!(cyclic, Err(ConvertError::Malformed { .. }));
        assert_matches!(styles.block_style(&parse_ctx, "a"), Err(ConvertError::Malformed { .. }));
        assert_matches!(
            styles.style_master_page(&parse_ctx, "paragraph", "b"),
            Err(ConvertError::Malformed { .. })
        );
        assert_eq!(styles.block_style(&parse_ctx, "c").unwrap(), None);
    }

    #[test]
    fn a_style_and_a_default_style_serve_only_their_own_family() {
        let tree = tree(&format!(
            r#"<style:default-style style:family="paragraph"><style:text-properties xmlns:fo="{fo}" fo:font-weight="bold"/></style:default-style>
            <style:style style:name="p" style:family="paragraph" style:master-page-name="Body"/>
            <style:style style:name="t" style:family="text" style:parent-style-name="p"/>
            <style:style style:name="t" style:family="paragraph" style:master-page-name="Other"><style:text-properties xmlns:fo="{fo}" fo:font-style="italic"/></style:style>
            <style:style style:name="q"/>"#,
            fo = ns::FO
        ));
        let mut styles = OdfStyles::default();
        styles.collect(&tree);
        let bold = StyleDelta { bold: Some(true), ..StyleDelta::default() };
        let plain = StyleDelta::default();
        for (family, name, expected) in [
            ("paragraph", "p", bold),
            ("paragraph", "", bold),
            ("paragraph", "missing", bold),
            ("paragraph", "t", StyleDelta { italic: Some(true), ..bold }),
            ("text", "t", plain),
            ("text", "p", plain),
            ("table", "p", plain),
        ] {
            let delta = styles.delta(&ParseCtx::default(), family, name).unwrap();
            assert_eq!(delta, expected, "{family} {name:?}");
        }
        let named =
            |family, name| styles.style(family, name).and_then(|def| def.attr(ns::STYLE, "name"));
        assert_eq!(named("paragraph", "p"), Some("p"));
        assert_eq!(named("text", "p"), None);
        assert_eq!(named("character", "t"), None);
        assert_eq!(named("", "q"), Some("q"));
        assert_eq!(named("paragraph", "q"), None);
        let parse_ctx = ParseCtx::default();
        let master = |family, name| styles.style_master_page(&parse_ctx, family, name).unwrap();
        assert_eq!(master("paragraph", "p"), Some("Body"));
        assert_eq!(master("paragraph", "t"), Some("Other"));
        assert_eq!(master("text", "t"), None);
        assert_eq!(master("table", "p"), None);
    }

    #[test]
    fn a_nul_in_a_family_or_a_name_keeps_two_styles_apart() {
        let tree = tree(
            &format!(
                r#"<style:style style:name="x|y" style:family="paragraph"/>
                <style:style style:name="y" style:family="paragraph|x"><style:text-properties xmlns:fo="{}" fo:font-weight="bold"/></style:style>"#,
                ns::FO
            )
            .replace('|', "\0"),
        );
        let mut styles = OdfStyles::default();
        styles.collect(&tree);
        let bold = StyleDelta { bold: Some(true), ..StyleDelta::default() };
        for (family, name, expected) in
            [("paragraph", "x\0y", StyleDelta::default()), ("paragraph\0x", "y", bold)]
        {
            let delta = styles.delta(&ParseCtx::default(), family, name).unwrap();
            assert_eq!(delta, expected, "{family:?} {name:?}");
            let named = styles.style(family, name).and_then(|def| def.attr(ns::STYLE, "name"));
            assert_eq!(named, Some(name), "{family:?} {name:?}");
        }
    }

    #[test]
    fn a_master_page_is_the_named_one_else_the_first_for_an_absent_or_empty_name() {
        let xml = format!(
            r#"<office:document-styles xmlns:office="{}" xmlns:style="{}"><office:master-styles><style:master-page style:name="first"/><style:master-page style:name="second"/></office:master-styles></office:document-styles>"#,
            ns::OFFICE,
            ns::STYLE
        );
        let masters = parse_xml(&ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap();
        let mut styles = OdfStyles::default();
        styles.collect(&masters);
        for (requested, expected) in [
            (None, Some("first")),
            (Some(""), Some("first")),
            (Some("second"), Some("second")),
            (Some("missing"), None),
        ] {
            let master = styles.master_or_first(requested);
            let name = master.and_then(|master| master.attr(ns::STYLE, "name"));
            assert_eq!(name, expected, "{requested:?}");
        }
        let none = tree("");
        let mut unmastered = OdfStyles::default();
        unmastered.collect(&none);
        assert_eq!(unmastered.master_or_first(None), None);
    }

    #[test]
    fn a_list_level_is_its_marker_and_start_else_a_bullet_from_one() {
        let tree = tree(&format!(
            r#"<text:list-style xmlns:text="{}" style:name="L"><text:list-level-style-number text:level="2" style:num-format="a" text:start-value="5"/></text:list-style>"#,
            ns::TEXT
        ));
        let mut styles = OdfStyles::default();
        styles.collect(&tree);
        for (name, depth, expected) in [
            ("L", 1, (MarkerKind::LowerAlpha, 5)),
            ("L", 0, (MarkerKind::Bullet, 1)),
            ("L", LIST_LEVELS, (MarkerKind::Bullet, 1)),
            ("missing", 1, (MarkerKind::Bullet, 1)),
        ] {
            assert_eq!(styles.list_level(name, depth), expected, "{name} {depth}");
        }
    }

    fn table_style(name: &str, parent: Option<&str>, display: Option<&str>) -> String {
        let parent = parent.map_or(String::new(), |p| format!(r#" style:parent-style-name="{p}""#));
        let properties = display.map_or(String::new(), |display| {
            format!(
                r#"<style:table-properties xmlns:table="{}" table:display="{display}"/>"#,
                ns::TABLE
            )
        });
        format!(
            r#"<style:style style:name="{name}" style:family="table"{parent}>{properties}</style:style>"#
        )
    }

    fn shown(styles: &str, style: Option<&str>) -> Result<bool, ConvertError> {
        let tree = tree(styles);
        let mut resolved = OdfStyles::default();
        resolved.collect(&tree);
        resolved.table_shown(&ParseCtx::default(), style)
    }

    #[test]
    fn a_table_display_hides_on_false_around_xml_white_space_and_on_nothing_else() {
        for (value, expected) in [
            ("false", false),
            (" false\n", false),
            ("\tfalse\r", false),
            ("true", true),
            (" true ", true),
            ("False", true),
            ("FALSE", true),
            ("0", true),
            ("1", true),
            ("no", true),
            ("", true),
            ("\u{a0}false", true),
            ("false\u{a0}", true),
        ] {
            let styles = table_style("t", None, Some(value));
            assert_eq!(shown(&styles, Some("t")).unwrap(), expected, "{value:?}");
        }
    }

    #[test]
    fn a_table_style_inherits_its_display_from_the_nearest_parent_that_sets_one() {
        for (own, expected) in [
            (Some("true"), true),
            (Some("false"), false),
            (Some("TRUE"), false),
            (Some("1"), false),
            (Some(""), false),
            (None, false),
        ] {
            let styles =
                [table_style("p", None, Some("false")), table_style("c", Some("p"), own)].concat();
            assert_eq!(shown(&styles, Some("c")).unwrap(), expected, "{own:?}");
            let styles = [
                table_style("g", None, Some("true")),
                table_style("p", Some("g"), None),
                table_style("c", Some("p"), own),
            ]
            .concat();
            let grand = own != Some("false");
            assert_eq!(shown(&styles, Some("c")).unwrap(), grand, "{own:?} over a grandparent");
        }
    }

    #[test]
    fn the_table_default_style_sits_beneath_every_chain_and_beneath_an_unstyled_sheet() {
        let default = |family: &str, display: &str| {
            format!(
                r#"<style:default-style style:family="{family}"><style:table-properties xmlns:table="{}" table:display="{display}"/></style:default-style>"#,
                ns::TABLE
            )
        };
        let styles = [
            default("table", "false"),
            default("paragraph", "true"),
            table_style("plain", None, None),
            table_style("odd", None, Some("1")),
            table_style("open", None, Some("true")),
            table_style("child", Some("open"), None),
            format!(
                r#"<style:style style:name="para" style:family="paragraph"><style:table-properties xmlns:table="{}" table:display="true"/></style:style>"#,
                ns::TABLE
            ),
        ]
        .concat();
        for (style, expected) in [
            (None, false),
            (Some("missing"), false),
            (Some("plain"), false),
            (Some("odd"), false),
            (Some("open"), true),
            (Some("child"), true),
            (Some("para"), false),
        ] {
            assert_eq!(shown(&styles, style).unwrap(), expected, "{style:?}");
        }
        assert!(shown(&table_style("t", None, None), None).unwrap());
    }

    #[test]
    fn a_table_style_cycle_fails_malformed_only_where_a_lookup_walks_it() {
        let styles = [
            table_style("a", Some("b"), None),
            table_style("b", Some("a"), None),
            table_style("c", None, None),
            table_style("d", Some("a"), Some("false")),
        ]
        .concat();
        assert_matches!(shown(&styles, Some("a")), Err(ConvertError::Malformed { .. }));
        assert_matches!(shown(&styles, Some("b")), Err(ConvertError::Malformed { .. }));
        assert!(shown(&styles, Some("c")).unwrap());
        assert!(!shown(&styles, Some("d")).unwrap());
    }

    #[test]
    fn a_table_style_chain_is_charged_once_per_style_and_stops_at_max_work_and_cancellation() {
        let tree = tree(&family_chain("table", 4000));
        let resolved = || {
            let mut styles = OdfStyles::default();
            styles.collect(&tree);
            styles
        };
        let styles = resolved();
        let parse_ctx = ParseCtx::default();
        for _ in 0..2 {
            assert!(styles.table_shown(&parse_ctx, Some("s3999")).unwrap());
        }
        assert_eq!(parse_ctx.fuel(), 4000);
        let limits = Limits { max_work: 3999, ..Limits::default() };
        assert_matches!(
            resolved().table_shown(&ParseCtx::new(&limits), Some("s3999")),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 3999 })
        );
        let token = CancelToken::new();
        let cancelled = cancelled(&token);
        assert_matches!(
            resolved().table_shown(&cancelled, Some("s0")),
            Err(ConvertError::Cancelled)
        );
    }
}
