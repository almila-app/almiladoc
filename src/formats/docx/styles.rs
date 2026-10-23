//! WordprocessingML style table.
//!
//! Bold/italic/strike/vanish are *toggle properties* (ECMA-376 §17.7.3): within the
//! style hierarchy a `true` specification toggles the inherited value and a
//! `false` specification leaves it unchanged, so the style layers contribute
//! a true-count *parity* XORed over the `docDefaults` base. Direct run
//! formatting is absolute on/off.

use super::numbering::Instance;
use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::model::{Style, VerticalPosition};
use crate::package::xml::{Element, Node, ns};
use crate::shared::blockstyle::{self, BlockStyle, Outline};
use crate::shared::chain::{Memo, StyleChains};
use crate::shared::delta::StyleDelta;
use crate::shared::symbol::{self, SymbolFont};

/// The font Word applies where no level of the hierarchy names one (MS-OI29500 2.1.88 c).
const DEFAULT_FONT: &str = "Times New Roman";

/// One font slot of `w:rFonts`: unset, a font name, or a theme font, which overrides the name
/// on the same element (MS-OI29500 2.1.88 e).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Face<'a> {
    #[default]
    Inherit,
    Named(&'a str),
    Theme(&'a str),
}

impl<'a> Face<'a> {
    fn of(fonts: &'a Element, name: &str, theme: &str) -> Self {
        match (fonts.attr(ns::W, theme), fonts.attr(ns::W, name)) {
            (Some(theme), _) => Self::Theme(theme),
            (None, Some(name)) => Self::Named(name),
            (None, None) => Self::Inherit,
        }
    }

    fn or(self, base: Self) -> Self {
        if self == Self::Inherit { base } else { self }
    }

    fn named(self) -> Option<&'a str> {
        match self {
            Self::Inherit => Some(DEFAULT_FONT),
            Self::Named(name) => Some(name),
            Self::Theme(_) => None,
        }
    }

    fn same(self, other: Self) -> bool {
        match (self, other) {
            (Self::Theme(a), Self::Theme(b)) => a == b,
            _ => self.named().zip(other.named()).is_some_and(|(a, b)| a.eq_ignore_ascii_case(b)),
        }
    }

    fn symbol_font(self) -> Option<SymbolFont> {
        match self {
            Self::Named(name) => SymbolFont::from_name(name),
            Self::Inherit | Self::Theme(_) => None,
        }
    }
}

/// The run fonts a level of the style hierarchy specifies, each slot nearest-wins.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RunFonts<'a> {
    ascii: Face<'a>,
    h_ansi: Face<'a>,
    east_asia: Face<'a>,
    east_asia_hint: Option<bool>,
    complex: Option<bool>,
    rtl: Option<bool>,
}

impl<'a> RunFonts<'a> {
    pub(super) fn of(rpr: &'a Element) -> Self {
        let fonts = rpr.find(ns::W, "rFonts");
        let face = |name, theme| fonts.map_or(Face::Inherit, |f| Face::of(f, name, theme));
        Self {
            ascii: face("ascii", "asciiTheme"),
            h_ansi: face("hAnsi", "hAnsiTheme"),
            east_asia: face("eastAsia", "eastAsiaTheme"),
            east_asia_hint: fonts.and_then(|f| f.attr(ns::W, "hint")).map(|h| h == "eastAsia"),
            complex: on_off(rpr, "cs"),
            rtl: on_off(rpr, "rtl"),
        }
    }

    /// These fonts with every slot they leave unset taken from `base`.
    pub(super) fn over(self, base: Self) -> Self {
        Self {
            ascii: self.ascii.or(base.ascii),
            h_ansi: self.h_ansi.or(base.h_ansi),
            east_asia: self.east_asia.or(base.east_asia),
            east_asia_hint: self.east_asia_hint.or(base.east_asia_hint),
            complex: self.complex.or(base.complex),
            rtl: self.rtl.or(base.rtl),
        }
    }

    /// Which symbol font draws each character of a run in these fonts, as Word picks the slot
    /// (MS-OI29500 2.1.88 b); `None` when no slot is a symbol font. A complex-script or
    /// right-to-left run draws in its `cs` font, which is not read, and the Chinese-language
    /// exceptions to the Latin-1 rows are not applied.
    pub(super) fn route(self) -> Option<FontRoute> {
        if self.complex == Some(true) || self.rtl == Some(true) {
            return None;
        }
        let east_asia_is_default =
            self.east_asia.named().is_some_and(|name| name.eq_ignore_ascii_case(DEFAULT_FONT))
                && self.ascii.same(self.h_ansi);
        let east_asia = if east_asia_is_default { self.ascii } else { self.east_asia };
        let route = FontRoute {
            ascii: self.ascii.symbol_font(),
            h_ansi: self.h_ansi.symbol_font(),
            east_asia: east_asia.symbol_font(),
            east_asia_hint: self.east_asia_hint == Some(true),
        };
        (route.ascii.is_some() || route.h_ansi.is_some() || route.east_asia.is_some())
            .then_some(route)
    }
}

/// The symbol font of each character class of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FontRoute {
    ascii: Option<SymbolFont>,
    h_ansi: Option<SymbolFont>,
    east_asia: Option<SymbolFont>,
    east_asia_hint: bool,
}

impl FontRoute {
    pub(super) fn font_of(self, c: char) -> Option<SymbolFont> {
        match c {
            '\0'..='\u{7f}' => self.ascii,
            _ if self.east_asia_hint && symbol::east_asian_ambiguous(c) => self.east_asia,
            _ => self.h_ansi,
        }
    }
}

/// Per-property parity of `true` toggle specifications in a style chain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Toggles {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub hidden: bool,
}

impl Toggles {
    fn of(rpr: &Element) -> Self {
        let delta = rpr_delta(rpr);
        Self {
            bold: delta.bold == Some(true),
            italic: delta.italic == Some(true),
            strike: delta.strike == Some(true),
            hidden: delta.hidden == Some(true),
        }
    }

    pub(super) fn xor(self, other: Self) -> Self {
        Self {
            bold: self.bold ^ other.bold,
            italic: self.italic ^ other.italic,
            strike: self.strike ^ other.strike,
            hidden: self.hidden ^ other.hidden,
        }
    }

    pub(super) fn hidden_over(self, base: bool) -> bool {
        base ^ self.hidden
    }

    pub(super) fn apply_over(self, base: Style) -> Style {
        Style {
            bold: base.bold ^ self.bold,
            italic: base.italic ^ self.italic,
            strike: base.strike ^ self.strike,
            code: base.code,
            position: base.position,
        }
    }
}

pub(super) struct Styles<'a> {
    chains: StyleChains<'a, Element>,
    /// docDefaults as absolute values (the base the toggles flip over).
    pub doc_defaults: Style,
    pub(super) hidden_by_default: bool,
    default_fonts: RunFonts<'a>,
    /// The paragraph style a paragraph naming none takes (`w:default`, ECMA-376 §17.7.4.17).
    default_paragraph: Option<&'a str>,
    toggles: Memo<'a, Toggles>,
    fonts: Memo<'a, RunFonts<'a>>,
    positions: Memo<'a, Option<VerticalPosition>>,
    headings: Memo<'a, Outline>,
    blocks: Memo<'a, Option<BlockStyle>>,
    num_ids: Memo<'a, Option<u64>>,
}

impl<'a> Styles<'a> {
    pub(super) fn parse(root: Option<&'a Element>) -> Self {
        let mut chains = StyleChains::default();
        let mut default_paragraph = None;
        for style in root.into_iter().flat_map(|root| root.find_all(ns::W, "style")) {
            let Some(id) = style.attr(ns::W, "styleId") else {
                continue;
            };
            chains.insert(id, style, val(style, "basedOn"));
            if default_paragraph.is_none()
                && style.attr(ns::W, "type") == Some("paragraph")
                && matches!(style.attr(ns::W, "default"), Some("1" | "true" | "on"))
            {
                default_paragraph = Some(id);
            }
        }
        let doc_defaults = root
            .and_then(|root| root.find(ns::W, "docDefaults"))
            .and_then(|defaults| defaults.find(ns::W, "rPrDefault"))
            .and_then(|defaults| defaults.find(ns::W, "rPr"));
        Self {
            chains,
            doc_defaults: doc_defaults.map_or(Style::PLAIN, |rpr| rpr_delta(rpr).resolve()),
            hidden_by_default: doc_defaults.is_some_and(|rpr| on_off(rpr, "vanish") == Some(true)),
            default_fonts: doc_defaults.map(RunFonts::of).unwrap_or_default(),
            default_paragraph,
            toggles: Memo::default(),
            fonts: Memo::default(),
            positions: Memo::default(),
            headings: Memo::default(),
            blocks: Memo::default(),
            num_ids: Memo::default(),
        }
    }

    /// The parity of `true` toggle specifications along a style's `basedOn`
    /// chain. A `false` in a style leaves the inherited value unchanged.
    pub(super) fn run_toggles(
        &self,
        parse_ctx: &ParseCtx<'_>,
        id: &str,
    ) -> Result<Toggles, ConvertError> {
        self.chains.resolve(
            parse_ctx,
            &self.toggles,
            id,
            Toggles::default(),
            |_| false,
            |style, inherited| {
                style.find(ns::W, "rPr").map_or(inherited, |rpr| inherited.xor(Toggles::of(rpr)))
            },
        )
    }

    /// The run fonts a style specifies along its `basedOn` chain, each slot nearest-wins.
    pub(super) fn run_fonts(
        &self,
        parse_ctx: &ParseCtx<'_>,
        id: &str,
    ) -> Result<RunFonts<'a>, ConvertError> {
        self.chains.resolve(
            parse_ctx,
            &self.fonts,
            id,
            RunFonts::default(),
            |_| false,
            |style, inherited| {
                style.find(ns::W, "rPr").map_or(inherited, |rpr| RunFonts::of(rpr).over(inherited))
            },
        )
    }

    /// The run fonts of a paragraph's text before its runs' own: its style's, or the default
    /// paragraph style's when it names none that exists, over `docDefaults`.
    pub(super) fn paragraph_fonts(
        &self,
        parse_ctx: &ParseCtx<'_>,
        pstyle_id: Option<&str>,
    ) -> Result<RunFonts<'a>, ConvertError> {
        match self.paragraph_style(pstyle_id) {
            Some(id) => Ok(self.run_fonts(parse_ctx, id)?.over(self.default_fonts)),
            None => Ok(self.default_fonts),
        }
    }

    /// Whether a paragraph's text is hidden before its runs' own properties: by its style, or
    /// the default paragraph style when it names none that exists, over `docDefaults`.
    pub(super) fn paragraph_hidden(
        &self,
        parse_ctx: &ParseCtx<'_>,
        pstyle_id: Option<&str>,
    ) -> Result<bool, ConvertError> {
        match self.paragraph_style(pstyle_id) {
            Some(id) => Ok(self.run_toggles(parse_ctx, id)?.hidden_over(self.hidden_by_default)),
            None => Ok(self.hidden_by_default),
        }
    }

    /// The shift a style's runs take, from the nearest style along its `basedOn` chain that
    /// names one.
    pub(super) fn run_position(
        &self,
        parse_ctx: &ParseCtx<'_>,
        id: &str,
    ) -> Result<Option<VerticalPosition>, ConvertError> {
        self.chains
            .nearest(parse_ctx, &self.positions, id, |style| vert_align(style.find(ns::W, "rPr")?))
    }

    /// The shift of a paragraph's text before its runs' own: its style's, or the default
    /// paragraph style's when it names none that exists, over `docDefaults`.
    pub(super) fn paragraph_position(
        &self,
        parse_ctx: &ParseCtx<'_>,
        pstyle_id: Option<&str>,
    ) -> Result<VerticalPosition, ConvertError> {
        let own = match self.paragraph_style(pstyle_id) {
            Some(id) => self.run_position(parse_ctx, id)?,
            None => None,
        };
        Ok(own.unwrap_or(self.doc_defaults.position))
    }

    fn paragraph_style<'s>(&'s self, pstyle_id: Option<&'s str>) -> Option<&'s str> {
        pstyle_id.filter(|id| self.chains.definition(id).is_some()).or(self.default_paragraph)
    }

    /// Heading level a paragraph style resolves to, from its name
    /// (`heading N`, `Title`) or an `outlineLvl`, inherited through
    /// `basedOn`. [`Outline::Body`] when the nearest specification is the
    /// explicit off value (`outlineLvl` 9), which stops inheritance.
    pub(super) fn heading_level(
        &self,
        parse_ctx: &ParseCtx<'_>,
        id: &str,
    ) -> Result<Outline, ConvertError> {
        let own = |style: &'a Element| {
            let name = val(style, "name").unwrap_or("").to_ascii_lowercase();
            if let Some(rest) = name.strip_prefix("heading ")
                && let Ok(level) = rest.trim().parse::<u8>()
            {
                return Outline::Level(level);
            }
            if name == "title" {
                return Outline::Level(1);
            }
            outline(style.find(ns::W, "pPr"))
        };
        self.chains.resolve(
            parse_ctx,
            &self.headings,
            id,
            Outline::Inherit,
            |style| own(style) != Outline::Inherit,
            |style, inherited| own(style).or(inherited),
        )
    }

    /// The block container a paragraph style names, inherited through
    /// `basedOn` (Word's `Quote`, Pandoc's `Source Code`, ...).
    pub(super) fn block_style(
        &self,
        parse_ctx: &ParseCtx<'_>,
        id: &str,
    ) -> Result<Option<BlockStyle>, ConvertError> {
        self.chains.nearest(parse_ctx, &self.blocks, id, |style| {
            blockstyle::from_style_name(val(style, "name")?)
        })
    }

    /// The `numId` a paragraph style contributes, inherited through
    /// `basedOn`. An `ilvl` inside a style's `numPr` is ignored per ECMA-376
    /// §17.3.1.19 - the effective level comes from the abstract levels'
    /// `w:pStyle` bindings ([`Styles::style_numbering_level`]).
    pub(super) fn style_num_pr(
        &self,
        parse_ctx: &ParseCtx<'_>,
        id: &str,
    ) -> Result<Option<u64>, ConvertError> {
        self.chains.nearest(parse_ctx, &self.num_ids, id, |style| {
            val(style.find(ns::W, "pPr")?.find(ns::W, "numPr")?, "numId")?.parse().ok()
        })
    }

    /// The numbering level a paragraph style binds to: the first style along
    /// the `basedOn` chain (child first) that one of the abstract levels of
    /// `instance` references via `w:pStyle`. Walked on every call: a memo
    /// per instance would grow with instances times styles.
    pub(super) fn style_numbering_level(
        &self,
        parse_ctx: &ParseCtx<'_>,
        id: &str,
        instance: &Instance,
    ) -> Result<Option<usize>, ConvertError> {
        self.chains.find(parse_ctx, id, |style| instance.style_level(style))
    }

    /// The `numId` referenced by a numbering style's own `numPr`
    /// (`numStyleLink` contract), without inheritance, each child the walk
    /// passes charged one unit.
    pub(super) fn direct_num_id(
        &self,
        parse_ctx: &ParseCtx<'_>,
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
        Ok(element.attr(ns::W, "val").and_then(|v| v.parse().ok()))
    }
}

fn charged_child<'e>(
    parse_ctx: &ParseCtx<'_>,
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
pub(super) fn rpr_delta(rpr: &Element) -> StyleDelta {
    let (s, d) = (on_off(rpr, "strike"), on_off(rpr, "dstrike"));
    StyleDelta {
        bold: on_off(rpr, "b"),
        italic: on_off(rpr, "i"),
        strike: match (s, d) {
            (None, None) => None,
            _ => Some(s == Some(true) || d == Some(true)),
        },
        code: None,
        position: vert_align(rpr),
        hidden: on_off(rpr, "vanish"),
    }
}

/// `w:vertAlign`, whose `baseline` cancels a shift the style hierarchy applies.
fn vert_align(rpr: &Element) -> Option<VerticalPosition> {
    match val(rpr, "vertAlign")? {
        "superscript" => Some(VerticalPosition::Superscript),
        "subscript" => Some(VerticalPosition::Subscript),
        "baseline" => Some(VerticalPosition::Baseline),
        _ => None,
    }
}

pub(super) fn val<'e>(parent: &'e Element, name: &str) -> Option<&'e str> {
    parent.find(ns::W, name)?.attr(ns::W, "val")
}

pub(super) fn outline(ppr: Option<&Element>) -> Outline {
    ppr.and_then(|ppr| val(ppr, "outlineLvl"))
        .and_then(|v| v.parse::<u8>().ok())
        .map_or(Outline::Inherit, Outline::from_operand)
}

/// ST_OnOff: `1`/`true`/`on` (or no value) are true; `0`/`false`/`off` are
/// false; an absent element is unspecified.
pub(super) fn on_off(parent: &Element, name: &str) -> Option<bool> {
    let elem = parent.find(ns::W, name)?;
    Some(!elem.attr(ns::W, "val").is_some_and(|value| turns_off(value.as_bytes())))
}

/// Whether an ST_OnOff value is one of the spellings of false.
pub(super) fn turns_off(value: &[u8]) -> bool {
    matches!(value, b"0" | b"false" | b"off" | b"none")
}

#[cfg(test)]
#[expect(clippy::format_collect, reason = "fixture XML is built with format!")]
mod tests {
    use super::*;
    use crate::limits::{Limit, Limits};
    use crate::package::archive::Role;
    use crate::package::xml::parse_xml;
    use crate::shared::test_support::cancelled;
    use std::assert_matches;

    // parse_xml returns a synthetic root wrapping the document element.
    fn parse(doc: &str) -> Element {
        parse_xml(&ParseCtx::default(), doc.as_bytes(), Role::Probe).unwrap()
    }

    fn styles_xml(body: &str) -> Element {
        parse(&format!(r#"<w:styles xmlns:w="{}">{body}</w:styles>"#, ns::W))
    }

    #[test]
    fn on_off_covers_the_full_value_space() {
        for (xml, expect) in [
            ("<w:b/>", Some(true)),
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
    fn toggles_keep_the_shift_of_the_style_they_flip() {
        let raised = Style { position: VerticalPosition::Superscript, ..Style::PLAIN };
        let flip = Toggles { bold: true, ..Default::default() };
        assert_eq!(flip.apply_over(raised), Style { bold: true, ..raised });
    }

    #[test]
    fn style_false_contributes_nothing_to_parity() {
        let root = styles_xml(
            r#"<w:style w:type="character" w:styleId="NotBold"><w:rPr><w:b w:val="0"/></w:rPr></w:style>
            <w:style w:type="character" w:styleId="Flip"><w:basedOn w:val="NotBold"/><w:rPr><w:b/></w:rPr></w:style>"#,
        );
        let styles = Styles::parse(root.find(ns::W, "styles"));
        let parse_ctx = ParseCtx::default();
        assert_eq!(styles.run_toggles(&parse_ctx, "NotBold").unwrap(), Toggles::default());
        assert!(styles.run_toggles(&parse_ctx, "Flip").unwrap().bold);
    }

    #[test]
    fn the_nearest_outline_decides_and_body_text_stops_inheritance() {
        let root = styles_xml(
            r#"<w:style w:styleId="A"><w:name w:val="heading 1"/></w:style>
            <w:style w:styleId="Off"><w:basedOn w:val="A"/><w:pPr><w:outlineLvl w:val="9"/></w:pPr></w:style>
            <w:style w:styleId="Below"><w:basedOn w:val="Off"/></w:style>
            <w:style w:styleId="Deeper"><w:basedOn w:val="A"/><w:pPr><w:outlineLvl w:val="2"/></w:pPr></w:style>
            <w:style w:styleId="Same"><w:basedOn w:val="A"/></w:style>
            <w:style w:styleId="None"/>"#,
        );
        let styles = Styles::parse(root.find(ns::W, "styles"));
        let parse_ctx = ParseCtx::default();
        let outline = |id| styles.heading_level(&parse_ctx, id).unwrap();
        assert_eq!(outline("A"), Outline::Level(1));
        assert_eq!(outline("Off"), Outline::Body);
        assert_eq!(outline("Below"), Outline::Body);
        assert_eq!(outline("Deeper"), Outline::Level(3));
        assert_eq!(outline("Same"), Outline::Level(1));
        assert_eq!(outline("None"), Outline::Inherit);
    }

    #[test]
    fn only_a_referenced_cycle_is_malformed() {
        let root = styles_xml(
            r#"<w:style w:styleId="A"><w:basedOn w:val="B"/></w:style>
            <w:style w:styleId="B"><w:basedOn w:val="A"/></w:style>
            <w:style w:styleId="C"><w:name w:val="heading 2"/><w:basedOn w:val="A"/></w:style>
            <w:style w:styleId="D"/>"#,
        );
        let styles = Styles::parse(root.find(ns::W, "styles"));
        let parse_ctx = ParseCtx::default();
        assert_eq!(styles.heading_level(&parse_ctx, "C").unwrap(), Outline::Level(2));
        assert_eq!(styles.run_toggles(&parse_ctx, "D").unwrap(), Toggles::default());
        assert_matches!(styles.heading_level(&parse_ctx, "A"), Err(ConvertError::Malformed { .. }));
        assert_matches!(styles.run_toggles(&parse_ctx, "C"), Err(ConvertError::Malformed { .. }));
    }

    #[test]
    fn every_resolver_charges_each_style_of_a_chain_once() {
        let chain: String = (0..4000)
            .map(|i| match i {
                0 => r#"<w:style w:styleId="s0"><w:name w:val="Quote"/><w:pPr><w:outlineLvl w:val="0"/><w:numPr><w:numId w:val="7"/></w:numPr></w:pPr><w:rPr><w:vertAlign w:val="superscript"/></w:rPr></w:style>"#.to_string(),
                2000 => r#"<w:style w:styleId="s2000"><w:basedOn w:val="s1999"/><w:rPr><w:b/><w:rFonts w:ascii="Symbol"/></w:rPr></w:style>"#.to_string(),
                _ => format!(
                    r#"<w:style w:styleId="s{i}"><w:basedOn w:val="s{}"/></w:style>"#,
                    i - 1
                ),
            })
            .collect();
        let root = styles_xml(&chain);
        let ids: Vec<String> = (0..4000).map(|i| format!("s{i}")).collect();
        let lowered = Limits { max_work: 3999, ..Limits::default() };
        let refused = |result: Result<String, ConvertError>| {
            matches!(result, Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 3999 }))
        };
        type Resolver = fn(&Styles<'_>, &ParseCtx<'_>, &str) -> Result<String, ConvertError>;
        let resolvers: [(Resolver, &str); 6] = [
            (|s, c, id| s.run_toggles(c, id).map(|t| format!("{}", t.bold)), "true"),
            (|s, c, id| s.heading_level(c, id).map(|l| format!("{l:?}")), "Level(1)"),
            (|s, c, id| s.block_style(c, id).map(|b| format!("{b:?}")), "Some(Quote)"),
            (|s, c, id| s.style_num_pr(c, id).map(|n| format!("{n:?}")), "Some(7)"),
            (|s, c, id| s.run_position(c, id).map(|p| format!("{p:?}")), "Some(Superscript)"),
            (
                |s, c, id| s.run_fonts(c, id).map(|f| format!("{:?}", f.route())),
                "Some(FontRoute { ascii: Some(Symbol), h_ansi: None, east_asia: None, east_asia_hint: false })",
            ),
        ];
        for (resolve, expected) in resolvers {
            let styles = Styles::parse(root.find(ns::W, "styles"));
            let parse_ctx = ParseCtx::default();
            for id in &ids {
                resolve(&styles, &parse_ctx, id).unwrap();
            }
            assert_eq!(resolve(&styles, &parse_ctx, "s3999").unwrap(), expected);
            assert_eq!(parse_ctx.fuel(), 4000, "{expected}");
            let styles = Styles::parse(root.find(ns::W, "styles"));
            assert!(refused(resolve(&styles, &ParseCtx::new(&lowered), "s3999")), "{expected}");
        }
    }

    fn memo_entries(styles: &Styles<'_>) -> usize {
        styles.toggles.borrow().len()
            + styles.headings.borrow().len()
            + styles.blocks.borrow().len()
            + styles.num_ids.borrow().len()
            + styles.fonts.borrow().len()
            + styles.positions.borrow().len()
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
        let root = styles_xml(&chain);
        let nums: String = (1..=depth)
            .map(|n| format!(r#"<w:num w:numId="{n}"><w:abstractNumId w:val="0"/></w:num>"#))
            .collect();
        let numbering = parse(&format!(
            r#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"/></w:abstractNum><w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="3"><w:pStyle w:val="s150"/></w:lvl></w:abstractNum><w:num w:numId="999"><w:abstractNumId w:val="1"/></w:num>{nums}</w:numbering>"#
        ));
        let numbering = super::super::numbering::parse(
            &ParseCtx::default(),
            numbering.find(ns::W, "numbering").unwrap(),
            &|_| Ok(None),
        )
        .unwrap();
        let styles = || Styles::parse(root.find(ns::W, "styles"));
        let level = |styles: &Styles<'_>, parse_ctx: &ParseCtx<'_>, num_id| {
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
        assert_matches!(
            refused,
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max }) if max == depth - 1
        );
    }

    #[test]
    fn a_numbering_styles_num_id_charges_every_child_its_walk_passes() {
        let root = styles_xml(
            r#"<w:style w:type="numbering" w:styleId="S"><w:q/><w:q/><w:q/><w:q/><w:q/><w:pPr><w:ind/><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr></w:style>"#,
        );
        let styles = Styles::parse(root.find(ns::W, "styles"));
        let walk = 6 + 2 + 2;
        let parse_ctx = ParseCtx::default();
        assert_eq!(styles.direct_num_id(&parse_ctx, "S").unwrap(), Some(7));
        assert_eq!(parse_ctx.fuel(), walk);
        assert_eq!(styles.direct_num_id(&parse_ctx, "Missing").unwrap(), None);
        assert_eq!(parse_ctx.fuel(), walk);
        let lowered = Limits { max_work: walk - 1, ..Limits::default() };
        let refused = styles.direct_num_id(&ParseCtx::new(&lowered), "S");
        assert_matches!(
            refused,
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max }) if max == walk - 1
        );
        let token = crate::ledger::CancelToken::new();
        let cancelled = styles.direct_num_id(&cancelled(&token), "S");
        assert_matches!(cancelled, Err(ConvertError::Cancelled));
    }
}
