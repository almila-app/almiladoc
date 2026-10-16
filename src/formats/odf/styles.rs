//! ODF style resolution: text/paragraph style chains as tri-state deltas
//! (so `font-weight: normal` can clear inherited bold), plus list styles
//! with per-level marker kinds and start values.
//!
//! Chains are keyed by owned `family\0name` strings because definitions come
//! from two separately parsed trees (`styles.xml` and `content.xml`).

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::package::xml::{Element, ns};
use crate::shared::blockstyle::{self, BlockStyle};
use crate::shared::delta::StyleDelta;
use crate::shared::list::MarkerKind;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

pub const LIST_LEVELS: usize = 10;

#[derive(Debug, Clone)]
pub struct ListLevel {
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
        ListLevel {
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
    pub fn pattern(&self, depth: usize) -> crate::shared::numbering::NumberPattern {
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
            text.push(NumberText::Level(level as u8));
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
pub struct OdfStyles<'a> {
    raw: HashMap<String, (&'a Element, Option<String>)>,
    memo: RefCell<HashMap<String, StyleDelta>>,
    blocks: RefCell<HashMap<String, Option<BlockStyle>>>,
    master_names: RefCell<HashMap<String, Option<&'a str>>>,
    list_styles: HashMap<String, [ListLevel; LIST_LEVELS]>,
    /// `text:outline-style`: heading numbering per outline level.
    outline: Option<[ListLevel; LIST_LEVELS]>,
    /// `style:default-style` per family: the base beneath every named chain
    /// (and the delta for unstyled content of that family).
    defaults: HashMap<String, StyleDelta>,
    /// `style:master-page` by name, and the document's first, which a page
    /// naming none uses.
    masters: HashMap<&'a str, &'a Element>,
    first_master: Option<&'a Element>,
}

fn key(family: &str, name: &str) -> String {
    format!("{family}\u{0}{name}")
}

impl<'a> OdfStyles<'a> {
    /// Collect styles from one document tree (`styles.xml` or `content.xml`).
    /// Call for styles.xml first so content.xml definitions chain onto it.
    pub fn collect(&mut self, tree: &'a Element) {
        for root in tree.child_elems() {
            for section in root.child_elems() {
                if section.is(ns::OFFICE, "master-styles") {
                    for master in section.find_all(ns::STYLE, "master-page") {
                        self.first_master.get_or_insert(master);
                        if let Some(name) = master.attr(ns::STYLE, "name") {
                            self.masters.entry(name).or_insert(master);
                        }
                    }
                    continue;
                }
                if !(section.is(ns::OFFICE, "automatic-styles") || section.is(ns::OFFICE, "styles"))
                {
                    continue;
                }
                for style in section.child_elems() {
                    if style.is(ns::STYLE, "default-style")
                        && let Some(family) = style.attr(ns::STYLE, "family")
                    {
                        self.defaults.insert(family.to_string(), text_properties_delta(style));
                    }
                    if style.is(ns::STYLE, "style")
                        && let Some(name) = style.attr(ns::STYLE, "name")
                    {
                        let family = style.attr(ns::STYLE, "family").unwrap_or("");
                        let parent =
                            style.attr(ns::STYLE, "parent-style-name").map(|p| key(family, p));
                        self.raw.insert(key(family, name), (style, parent));
                    }
                    if style.is(ns::TEXT, "list-style")
                        && let Some(name) = style.attr(ns::STYLE, "name")
                    {
                        self.list_styles.insert(name.to_string(), parse_list_style(style));
                    }
                    if style.is(ns::TEXT, "outline-style") {
                        self.outline = Some(parse_list_style(style));
                    }
                }
            }
        }
    }

    /// Cumulative delta of a style through its `parent-style-name` chain,
    /// over the family's `style:default-style` base; each style is resolved
    /// once and charged one unit when it is.
    pub fn delta(
        &self,
        parse_ctx: &ParseCtx,
        family: &str,
        name: &str,
    ) -> Result<StyleDelta, ConvertError> {
        let mut delta = self.defaults.get(family).copied().unwrap_or_default();
        let mut chain: Vec<(&str, &Element)> = Vec::new();
        let mut visited: HashSet<&str> = HashSet::new();
        let mut cursor = self.link(&key(family, name));
        while let Some((current, def, parent)) = cursor {
            if let Some(hit) = self.memo.borrow().get(current) {
                delta = *hit;
                break;
            }
            if !visited.insert(current) {
                return Err(ConvertError::malformed("style inheritance cycle"));
            }
            parse_ctx.charge(1)?;
            chain.push((current, def));
            cursor = parent.and_then(|parent| self.link(parent));
        }
        for (current, def) in chain.into_iter().rev() {
            delta = delta.merge(text_properties_delta(def));
            self.memo.borrow_mut().insert(current.to_string(), delta);
        }
        Ok(delta)
    }

    fn link(&self, id: &str) -> Option<(&str, &'a Element, Option<&str>)> {
        self.raw.get_key_value(id).map(|(id, (def, parent))| (id.as_str(), *def, parent.as_deref()))
    }

    /// The nearest value `own` finds along the `parent-style-name` chain
    /// from the style keyed `id`, child first; each style is resolved once
    /// into `memo` and charged one unit when it is, and a cycle ends the
    /// walk.
    fn nearest<T: Copy>(
        &self,
        parse_ctx: &ParseCtx,
        memo: &RefCell<HashMap<String, Option<T>>>,
        id: &str,
        own: impl Fn(&'a Element) -> Option<T>,
    ) -> Result<Option<T>, ConvertError> {
        let mut chain: Vec<&str> = Vec::new();
        let mut visited: HashSet<&str> = HashSet::new();
        let mut found = None;
        let mut cursor = self.link(id);
        while let Some((current, def, parent)) = cursor {
            if let Some(&hit) = memo.borrow().get(current) {
                found = hit;
                break;
            }
            if !visited.insert(current) {
                break;
            }
            parse_ctx.charge(1)?;
            chain.push(current);
            found = own(def);
            if found.is_some() {
                break;
            }
            cursor = parent.and_then(|parent| self.link(parent));
        }
        memo.borrow_mut().extend(chain.into_iter().map(|current| (current.to_string(), found)));
        Ok(found)
    }

    /// The block container a paragraph style names, walking
    /// `parent-style-name`: an automatic style carries the name on the
    /// named style it derives from.
    pub fn block_style(
        &self,
        parse_ctx: &ParseCtx,
        name: &str,
    ) -> Result<Option<BlockStyle>, ConvertError> {
        self.nearest(parse_ctx, &self.blocks, &key("paragraph", name), |def| {
            def.attr(ns::STYLE, "name").and_then(blockstyle::from_style_name)
        })
    }

    pub fn list_level(&self, style_name: &str, depth: usize) -> ListLevel {
        self.list_styles
            .get(style_name)
            .and_then(|levels| levels.get(depth).cloned())
            .unwrap_or_default()
    }

    /// The full level array of a list style, for composite-label rendering.
    pub fn list_levels(&self, style_name: &str) -> Option<&[ListLevel; LIST_LEVELS]> {
        self.list_styles.get(style_name)
    }

    /// The document's heading numbering (`text:outline-style`) levels.
    pub fn outline_levels(&self) -> Option<&[ListLevel; LIST_LEVELS]> {
        self.outline.as_ref()
    }

    /// A named style's own definition.
    pub fn style(&self, family: &str, name: &str) -> Option<&'a Element> {
        self.raw.get(&key(family, name)).map(|(def, _)| *def)
    }

    /// The master page a style starts, by name, through its
    /// `parent-style-name` chain; an empty name starts none.
    pub fn style_master_page(
        &self,
        parse_ctx: &ParseCtx,
        family: &str,
        name: &str,
    ) -> Result<Option<&'a str>, ConvertError> {
        let master = self.nearest(parse_ctx, &self.master_names, &key(family, name), |def| {
            def.attr(ns::STYLE, "master-page-name")
        })?;
        Ok(master.filter(|name| !name.is_empty()))
    }

    /// A master page by its `style:name`.
    pub fn master_page(&self, name: &str) -> Option<&'a Element> {
        self.masters.get(name).copied()
    }

    /// The document's first master page, which a page naming none uses.
    pub fn first_master_page(&self) -> Option<&'a Element> {
        self.first_master
    }
}

/// Clamp an untrusted start value so counters can never overflow.
pub fn parse_start(v: &str) -> Option<u64> {
    v.parse::<u64>().ok().map(|n| n.min(u32::MAX as u64))
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
pub fn text_properties_delta(elem: &Element) -> StyleDelta {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::CancelToken;
    use crate::limits::{Limit, Limits};
    use crate::package::archive::Role;
    use crate::package::xml::parse_xml;

    fn tree(styles: &str) -> Element {
        let xml = format!(
            r#"<office:document-styles xmlns:office="{}" xmlns:style="{}"><office:styles>{styles}</office:styles></office:document-styles>"#,
            ns::OFFICE,
            ns::STYLE
        );
        parse_xml(&ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap()
    }

    fn chain(depth: usize) -> String {
        (0..depth)
            .map(|i| match i.checked_sub(1) {
                Some(parent) => format!(
                    r#"<style:style style:name="s{i}" style:family="paragraph" style:parent-style-name="s{parent}"/>"#
                ),
                None => r#"<style:style style:name="s0" style:family="paragraph"/>"#.to_string(),
            })
            .collect()
    }

    #[test]
    fn a_resolved_ancestor_ends_the_walk() {
        let tree = tree(&chain(4));
        let mut styles = OdfStyles::default();
        styles.collect(&tree);
        let bold = StyleDelta { bold: Some(true), ..StyleDelta::default() };
        styles.memo.borrow_mut().insert(key("paragraph", "s1"), bold);
        styles.blocks.borrow_mut().insert(key("paragraph", "s1"), Some(BlockStyle::Quote));
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
        let memos = [&styles.memo.borrow().len(), &styles.blocks.borrow().len()];
        assert_eq!(memos, [&4000, &4000]);
        assert_eq!(styles.master_names.borrow().len(), 4000);
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
        token.cancel();
        let cancelled = ParseCtx::default().with_cancel(Some(&token));
        let resolve = |parse_ctx: &ParseCtx, name: &str| {
            [
                styles.delta(parse_ctx, "paragraph", name).map(drop),
                styles.block_style(parse_ctx, name).map(drop),
                styles.style_master_page(parse_ctx, "paragraph", name).map(drop),
            ]
        };
        for refused in resolve(&lowered, "s3999") {
            assert!(matches!(
                refused,
                Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 3999 })
            ));
        }
        for refused in resolve(&cancelled, "s0") {
            assert!(matches!(refused, Err(ConvertError::Cancelled)));
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
        assert!(styles.delta(&parse_ctx, "paragraph", "c").is_ok());
        let cyclic = styles.delta(&parse_ctx, "paragraph", "a");
        assert!(matches!(cyclic, Err(ConvertError::Malformed { .. })));
        assert_eq!(styles.block_style(&parse_ctx, "a").unwrap(), None);
    }
}
