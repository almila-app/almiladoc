//! WordprocessingML style table.
//!
//! Bold/italic/strike are *toggle properties* (ECMA-376 §17.7.3): within the
//! style hierarchy a `true` specification toggles the inherited value and a
//! `false` specification leaves it unchanged, so the style layers contribute
//! a true-count *parity* XORed over the `docDefaults` base. Direct run
//! formatting is absolute on/off.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::model::Style;
use crate::package::xml::{Element, Node, ns};
use crate::shared::blockstyle::{self, BlockStyle};
use crate::shared::chain::{Memo, StyleChains};
use crate::shared::delta::StyleDelta;

/// Per-property parity of `true` toggle specifications in a style chain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Toggles {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
}

impl Toggles {
    pub fn xor(self, other: Toggles) -> Toggles {
        Toggles {
            bold: self.bold ^ other.bold,
            italic: self.italic ^ other.italic,
            strike: self.strike ^ other.strike,
        }
    }

    pub fn apply_over(self, base: Style) -> Style {
        Style {
            bold: base.bold ^ self.bold,
            italic: base.italic ^ self.italic,
            strike: base.strike ^ self.strike,
            code: base.code,
        }
    }
}

pub struct Styles<'a> {
    chains: StyleChains<'a, Element>,
    /// docDefaults as absolute values (the base the toggles flip over).
    pub doc_defaults: Style,
    toggles: Memo<'a, Toggles>,
    headings: Memo<'a, Option<Option<u8>>>,
    blocks: Memo<'a, Option<BlockStyle>>,
    num_ids: Memo<'a, Option<u64>>,
}

impl<'a> Styles<'a> {
    pub fn parse_opt(root: Option<&'a Element>) -> Styles<'a> {
        match root {
            Some(root) => Styles::parse(root),
            None => Styles::new(StyleChains::default(), Style::PLAIN),
        }
    }

    pub fn parse(root: &'a Element) -> Styles<'a> {
        let mut chains = StyleChains::default();
        for style in root.find_all(ns::W, "style") {
            if let Some(id) = style.attr(ns::W, "styleId") {
                let parent = style.find(ns::W, "basedOn").and_then(|e| e.attr(ns::W, "val"));
                chains.insert(id, style, parent);
            }
        }
        let doc_defaults = root
            .find(ns::W, "docDefaults")
            .and_then(|d| d.find(ns::W, "rPrDefault"))
            .and_then(|d| d.find(ns::W, "rPr"))
            .map(|rpr| rpr_delta(rpr).resolve())
            .unwrap_or(Style::PLAIN);
        Styles::new(chains, doc_defaults)
    }

    fn new(chains: StyleChains<'a, Element>, doc_defaults: Style) -> Styles<'a> {
        Styles {
            chains,
            doc_defaults,
            toggles: Memo::default(),
            headings: Memo::default(),
            blocks: Memo::default(),
            num_ids: Memo::default(),
        }
    }

    /// The nearest specification `own` finds along a style's `basedOn`
    /// chain, child first.
    fn nearest<T: Clone>(
        &self,
        parse_ctx: &ParseCtx,
        memo: &Memo<'a, Option<T>>,
        id: &str,
        own: impl Fn(&'a Element) -> Option<T>,
    ) -> Result<Option<T>, ConvertError> {
        self.chains.resolve(
            parse_ctx,
            memo,
            id,
            None,
            |style| own(style).is_some(),
            |style, inherited| own(style).or(inherited),
        )
    }

    /// The parity of `true` toggle specifications along a style's `basedOn`
    /// chain. A `false` in a style leaves the inherited value unchanged.
    pub fn run_toggles(&self, parse_ctx: &ParseCtx, id: &str) -> Result<Toggles, ConvertError> {
        self.chains.resolve(
            parse_ctx,
            &self.toggles,
            id,
            Toggles::default(),
            |_| false,
            |style, inherited| match style.find(ns::W, "rPr") {
                Some(rpr) => inherited.xor(Toggles {
                    bold: on_off(rpr, "b") == Some(true),
                    italic: on_off(rpr, "i") == Some(true),
                    strike: on_off(rpr, "strike") == Some(true)
                        || on_off(rpr, "dstrike") == Some(true),
                }),
                None => inherited,
            },
        )
    }

    /// Heading level a paragraph style resolves to, from its name
    /// (`heading N`, `Title`) or an `outlineLvl`, inherited through
    /// `basedOn`. Tri-state: `Some(None)` when the nearest specification is
    /// the explicit off value (`outlineLvl` 9), which stops inheritance.
    pub fn heading_level(
        &self,
        parse_ctx: &ParseCtx,
        id: &str,
    ) -> Result<Option<Option<u8>>, ConvertError> {
        self.nearest(parse_ctx, &self.headings, id, |style| {
            let name = style
                .find(ns::W, "name")
                .and_then(|e| e.attr(ns::W, "val"))
                .unwrap_or("")
                .to_ascii_lowercase();
            if let Some(rest) = name.strip_prefix("heading ")
                && let Ok(level) = rest.trim().parse::<u8>()
            {
                return Some(Some(level));
            }
            if name == "title" {
                return Some(Some(1));
            }
            let level = style
                .find(ns::W, "pPr")?
                .find(ns::W, "outlineLvl")?
                .attr(ns::W, "val")?
                .parse::<u8>()
                .ok()?;
            Some(if level < 9 { Some(level + 1) } else { None })
        })
    }

    /// The block container a paragraph style names, inherited through
    /// `basedOn` (Word's `Quote`, Pandoc's `Source Code`, ...).
    pub fn block_style(
        &self,
        parse_ctx: &ParseCtx,
        id: &str,
    ) -> Result<Option<BlockStyle>, ConvertError> {
        self.nearest(parse_ctx, &self.blocks, id, |style| {
            let name = style.find(ns::W, "name")?.attr(ns::W, "val")?;
            blockstyle::from_style_name(name)
        })
    }

    /// The `numId` a paragraph style contributes, inherited through
    /// `basedOn`. An `ilvl` inside a style's `numPr` is ignored per ECMA-376
    /// §17.3.1.19 - the effective level comes from the abstract levels'
    /// `w:pStyle` bindings ([`Styles::style_numbering_level`]).
    pub fn style_num_pr(
        &self,
        parse_ctx: &ParseCtx,
        id: &str,
    ) -> Result<Option<u64>, ConvertError> {
        self.nearest(parse_ctx, &self.num_ids, id, |style| {
            style
                .find(ns::W, "pPr")?
                .find(ns::W, "numPr")?
                .find(ns::W, "numId")
                .and_then(|e| e.attr(ns::W, "val"))?
                .parse()
                .ok()
        })
    }

    /// The numbering level a paragraph style binds to: the first style along
    /// the `basedOn` chain (child first) that one of the abstract levels of
    /// `instance` references via `w:pStyle`. Walked on every call: a memo
    /// per instance would grow with instances times styles.
    pub fn style_numbering_level(
        &self,
        parse_ctx: &ParseCtx,
        id: &str,
        instance: &crate::formats::docx::numbering::Instance,
    ) -> Result<Option<usize>, ConvertError> {
        self.chains.find(parse_ctx, id, |style| instance.style_level(style))
    }

    /// The `numId` referenced by a numbering style's own `numPr`
    /// (`numStyleLink` contract), without inheritance, each child the walk
    /// passes charged one unit.
    pub fn direct_num_id(
        &self,
        parse_ctx: &ParseCtx,
        id: &str,
    ) -> Result<Option<u64>, ConvertError> {
        let Some(mut element) = self.chains.definition(id) else {
            return Ok(None);
        };
        for local in ["pPr", "numPr", "numId"] {
            match charged_child(parse_ctx, element, local)? {
                Some(child) => element = child,
                None => return Ok(None),
            }
        }
        Ok(element.attr(ns::W, "val").and_then(|val| val.parse().ok()))
    }
}

fn charged_child<'e>(
    parse_ctx: &ParseCtx,
    parent: &'e Element,
    local: &str,
) -> Result<Option<&'e Element>, ConvertError> {
    for node in &parent.children {
        parse_ctx.charge(1)?;
        if let Node::Elem(child) = node
            && child.is(ns::W, local)
        {
            return Ok(Some(child));
        }
    }
    Ok(None)
}

/// A `w:rPr` element as a tri-state delta - used only for *direct* run
/// formatting, where specifications are absolute on/off.
pub fn rpr_delta(rpr: &Element) -> StyleDelta {
    let (s, d) = (on_off(rpr, "strike"), on_off(rpr, "dstrike"));
    StyleDelta {
        bold: on_off(rpr, "b"),
        italic: on_off(rpr, "i"),
        strike: if s.is_some() || d.is_some() {
            Some(s.unwrap_or(false) || d.unwrap_or(false))
        } else {
            None
        },
        code: None,
    }
}

/// ST_OnOff: `1`/`true`/`on` (or no value) are true; `0`/`false`/`off` are
/// false; an absent element is unspecified.
pub fn on_off(parent: &Element, name: &str) -> Option<bool> {
    let elem = parent.find(ns::W, name)?;
    Some(!matches!(elem.attr(ns::W, "val"), Some("0" | "false" | "off" | "none")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{Limit, Limits};
    use crate::package::archive::Role;
    use crate::package::xml::parse_xml;

    // parse_xml returns a synthetic root wrapping the document element.
    fn parse(doc: &str) -> Element {
        parse_xml(&crate::ctx::ParseCtx::default(), doc.as_bytes(), Role::Probe).unwrap()
    }

    #[test]
    fn on_off_covers_the_full_value_space() {
        for (xml, expect) in [
            (r#"<w:b/>"#, Some(true)),
            (r#"<w:b w:val="1"/>"#, Some(true)),
            (r#"<w:b w:val="true"/>"#, Some(true)),
            (r#"<w:b w:val="on"/>"#, Some(true)),
            (r#"<w:b w:val="0"/>"#, Some(false)),
            (r#"<w:b w:val="false"/>"#, Some(false)),
            (r#"<w:b w:val="off"/>"#, Some(false)),
            ("", None),
        ] {
            let root = parse(&format!(
                r#"<w:rPr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">{xml}</w:rPr>"#
            ));
            let rpr = root.find(ns::W, "rPr").unwrap();
            assert_eq!(on_off(rpr, "b"), expect, "for {xml:?}");
        }
    }

    #[test]
    fn toggles_flip_the_base_and_double_flips_cancel() {
        let base = Style { bold: true, ..Style::PLAIN };
        let flip = Toggles { bold: true, ..Default::default() };
        assert!(!flip.apply_over(base).bold);
        assert!(flip.xor(flip).apply_over(base).bold);
    }

    #[test]
    fn style_false_contributes_nothing_to_parity() {
        let styles_xml = r#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:style w:type="character" w:styleId="NotBold"><w:rPr><w:b w:val="0"/></w:rPr></w:style>
            <w:style w:type="character" w:styleId="Flip"><w:basedOn w:val="NotBold"/><w:rPr><w:b/></w:rPr></w:style>
        </w:styles>"#;
        let root = parse(styles_xml);
        let styles = Styles::parse(root.find(ns::W, "styles").unwrap());
        let parse_ctx = crate::ctx::ParseCtx::default();
        assert_eq!(styles.run_toggles(&parse_ctx, "NotBold").unwrap(), Toggles::default());
        assert!(styles.run_toggles(&parse_ctx, "Flip").unwrap().bold);
    }

    #[test]
    fn only_a_referenced_cycle_is_malformed() {
        let styles_xml = r#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:style w:styleId="A"><w:basedOn w:val="B"/></w:style>
            <w:style w:styleId="B"><w:basedOn w:val="A"/></w:style>
            <w:style w:styleId="C"><w:name w:val="heading 2"/><w:basedOn w:val="A"/></w:style>
            <w:style w:styleId="D"/>
        </w:styles>"#;
        let root = parse(styles_xml);
        let styles = Styles::parse(root.find(ns::W, "styles").unwrap());
        let parse_ctx = crate::ctx::ParseCtx::default();
        assert_eq!(styles.heading_level(&parse_ctx, "C").unwrap(), Some(Some(2)));
        assert_eq!(styles.run_toggles(&parse_ctx, "D").unwrap(), Toggles::default());
        assert!(matches!(
            styles.heading_level(&parse_ctx, "A"),
            Err(ConvertError::Malformed { .. })
        ));
        assert!(matches!(styles.run_toggles(&parse_ctx, "C"), Err(ConvertError::Malformed { .. })));
    }

    #[test]
    fn every_resolver_charges_each_style_of_a_chain_once() {
        let chain: String = (0..4000)
            .map(|i| match i {
                0 => r#"<w:style w:styleId="s0"><w:name w:val="Quote"/><w:pPr><w:outlineLvl w:val="0"/><w:numPr><w:numId w:val="7"/></w:numPr></w:pPr></w:style>"#.to_string(),
                2000 => r#"<w:style w:styleId="s2000"><w:basedOn w:val="s1999"/><w:rPr><w:b/></w:rPr></w:style>"#.to_string(),
                _ => format!(
                    r#"<w:style w:styleId="s{i}"><w:basedOn w:val="s{}"/></w:style>"#,
                    i - 1
                ),
            })
            .collect();
        let root = parse(&format!(
            r#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">{chain}</w:styles>"#
        ));
        let ids: Vec<String> = (0..4000).map(|i| format!("s{i}")).collect();
        let lowered = Limits { max_work: 3999, ..Limits::default() };
        let refused = |result: Result<String, ConvertError>| {
            matches!(result, Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 3999 }))
        };
        type Resolver = fn(&Styles, &ParseCtx, &str) -> Result<String, ConvertError>;
        let resolvers: [(Resolver, &str); 4] = [
            (|s, c, id| s.run_toggles(c, id).map(|t| format!("{}", t.bold)), "true"),
            (|s, c, id| s.heading_level(c, id).map(|l| format!("{l:?}")), "Some(Some(1))"),
            (|s, c, id| s.block_style(c, id).map(|b| format!("{b:?}")), "Some(Quote)"),
            (|s, c, id| s.style_num_pr(c, id).map(|n| format!("{n:?}")), "Some(7)"),
        ];
        for (resolve, expected) in resolvers {
            let styles = Styles::parse(root.find(ns::W, "styles").unwrap());
            let parse_ctx = ParseCtx::default();
            for id in &ids {
                resolve(&styles, &parse_ctx, id).unwrap();
            }
            assert_eq!(resolve(&styles, &parse_ctx, "s3999").unwrap(), expected);
            assert_eq!(parse_ctx.fuel(), 4000, "{expected}");
            let styles = Styles::parse(root.find(ns::W, "styles").unwrap());
            assert!(refused(resolve(&styles, &ParseCtx::new(&lowered), "s3999")), "{expected}");
        }
    }

    fn memo_entries(styles: &Styles) -> usize {
        styles.toggles.borrow().len()
            + styles.headings.borrow().len()
            + styles.blocks.borrow().len()
            + styles.num_ids.borrow().len()
    }

    #[test]
    fn a_numbering_level_by_style_walks_the_chain_and_keeps_nothing_per_instance() {
        let depth: u64 = 200;
        let chain: String = (0..depth)
            .map(|i| match i.checked_sub(1) {
                Some(parent) => {
                    format!(r#"<w:style w:styleId="s{i}"><w:basedOn w:val="s{parent}"/></w:style>"#)
                }
                None => r#"<w:style w:styleId="s0"/>"#.to_string(),
            })
            .collect();
        let root = parse(&format!(
            r#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">{chain}</w:styles>"#
        ));
        let nums: String = (1..=depth)
            .map(|n| format!(r#"<w:num w:numId="{n}"><w:abstractNumId w:val="0"/></w:num>"#))
            .collect();
        let numbering = parse(&format!(
            r#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"/></w:abstractNum><w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="3"><w:pStyle w:val="s150"/></w:lvl></w:abstractNum><w:num w:numId="999"><w:abstractNumId w:val="1"/></w:num>{nums}</w:numbering>"#
        ));
        let numbering = crate::formats::docx::numbering::parse(
            &ParseCtx::default(),
            numbering.find(ns::W, "numbering").unwrap(),
            &|_| Ok(None),
        )
        .unwrap();
        let styles = || Styles::parse(root.find(ns::W, "styles").unwrap());
        let level = |styles: &Styles, parse_ctx: &ParseCtx, num_id| {
            let instance = numbering.instance(num_id).unwrap();
            styles.style_numbering_level(parse_ctx, "s199", instance)
        };
        let resolved = styles();
        let parse_ctx = ParseCtx::default();
        assert_eq!(level(&resolved, &parse_ctx, 999).unwrap(), Some(3));
        assert_eq!(parse_ctx.fuel(), 50 * 4);
        for num_id in 1..=depth {
            assert_eq!(level(&resolved, &parse_ctx, num_id).unwrap(), None);
        }
        let whole_chain: u64 = (0..depth).map(|i| format!("s{i}").len() as u64).sum();
        assert_eq!(parse_ctx.fuel(), 50 * 4 + depth * whole_chain);
        assert_eq!(memo_entries(&resolved), 0);
        let lowered = Limits { max_work: depth - 1, ..Limits::default() };
        let refused = level(&styles(), &ParseCtx::new(&lowered), 1);
        assert!(matches!(refused, Err(ConvertError::ResourceLimit { limit: Limit::Work, max })
            if max == depth - 1));
    }

    #[test]
    fn a_numbering_styles_num_id_charges_every_child_its_walk_passes() {
        let root = parse(
            r#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="numbering" w:styleId="S"><w:q/><w:q/><w:q/><w:q/><w:q/><w:pPr><w:ind/><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr></w:style></w:styles>"#,
        );
        let styles = Styles::parse(root.find(ns::W, "styles").unwrap());
        let walk = 6 + 2 + 2;
        let parse_ctx = ParseCtx::default();
        assert_eq!(styles.direct_num_id(&parse_ctx, "S").unwrap(), Some(7));
        assert_eq!(parse_ctx.fuel(), walk);
        assert_eq!(styles.direct_num_id(&parse_ctx, "Missing").unwrap(), None);
        assert_eq!(parse_ctx.fuel(), walk);
        let lowered = Limits { max_work: walk - 1, ..Limits::default() };
        let refused = styles.direct_num_id(&ParseCtx::new(&lowered), "S");
        assert!(matches!(refused, Err(ConvertError::ResourceLimit { limit: Limit::Work, max })
            if max == walk - 1));
        let token = crate::ledger::CancelToken::new();
        token.cancel();
        let cancelled = styles.direct_num_id(&ParseCtx::default().with_cancel(Some(&token)), "S");
        assert!(matches!(cancelled, Err(ConvertError::Cancelled)));
    }
}
