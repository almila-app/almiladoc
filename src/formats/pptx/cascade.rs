//! PresentationML text-property cascade: run properties resolve through
//! paragraph `pPr` -> shape `lstStyle` -> layout placeholder -> master
//! placeholder / `txStyles` -> presentation `defaultTextStyle`, with
//! explicit-off states honored at every layer.

use std::cmp::Ordering;

use crate::model::VerticalPosition;
use crate::package::xml::{Element, ns};
use crate::shared::delta::StyleDelta;
use crate::shared::list::MarkerKind;
use crate::shared::numbering::{LevelCounters, OFFICE_LEVELS};
use crate::shared::typeface::{Slot, Slots};

pub(super) const LEVELS: usize = OFFICE_LEVELS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Bullet {
    /// Not specified at this layer; inherit from the next one.
    #[default]
    Inherit,
    /// Explicitly no bullet (`a:buNone`).
    None,
    /// Character bullet (`a:buChar`).
    Char,
    /// Auto-numbered (`a:buAutoNum`).
    AutoNum { marker: MarkerKind, start: u64, wrap: NumWrap },
}

/// The punctuation an `ST_TextAutoNumberScheme` wraps around the ordinal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum NumWrap {
    /// `1.` — matches the renderer's default label.
    #[default]
    Period,
    /// `1)`
    ParenR,
    /// `(1)`
    ParenBoth,
    /// Bare `1`.
    Plain,
}

impl NumWrap {
    /// The literal marker label for ordinal `n`; `None` when the default
    /// label (`n.`) is already faithful.
    pub(super) fn label(self, marker: MarkerKind, n: u64) -> Option<String> {
        match self {
            Self::Period => None,
            Self::ParenR => Some(format!("{})", marker.ordinal(n))),
            Self::ParenBoth => Some(format!("({})", marker.ordinal(n))),
            Self::Plain => Some(marker.ordinal(n)),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct TextProps {
    pub delta: StyleDelta,
    pub bullet: Bullet,
    pub faces: Slots,
}

impl TextProps {
    /// Overlay `over` on `self`: explicit values win, `Inherit` falls through.
    pub(super) fn merge(self, over: Self) -> Self {
        Self {
            delta: self.delta.merge(over.delta),
            bullet: if over.bullet == Bullet::Inherit { self.bullet } else { over.bullet },
            faces: self.faces.merge(over.faces),
        }
    }
}

/// Per-level text properties from a `lstStyle`-shaped element
/// (`a:lvl1pPr`..`a:lvl9pPr` children).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct LevelStyle {
    levels: [TextProps; LEVELS],
}

impl LevelStyle {
    pub(super) fn level(&self, lvl: usize) -> TextProps {
        self.levels[LevelCounters::<LEVELS>::clamp(lvl)]
    }
}

const LEVEL_TAGS: [&str; LEVELS] = [
    "lvl1pPr", "lvl2pPr", "lvl3pPr", "lvl4pPr", "lvl5pPr", "lvl6pPr", "lvl7pPr", "lvl8pPr",
    "lvl9pPr",
];

pub(super) fn parse_level_styles(elem: Option<&Element>) -> LevelStyle {
    let mut out = LevelStyle::default();
    let Some(elem) = elem else {
        return out;
    };
    for (slot, tag) in out.levels.iter_mut().zip(LEVEL_TAGS) {
        if let Some(ppr) = elem.find(ns::A, tag) {
            *slot = paragraph_props(ppr);
        }
    }
    out
}

/// Properties carried by one `pPr`-shaped element (direct paragraph
/// properties or one `lvlNpPr` level).
pub(super) fn paragraph_props(ppr: &Element) -> TextProps {
    let bullet = if ppr.find(ns::A, "buNone").is_some() {
        Bullet::None
    } else if let Some(auto) = ppr.find(ns::A, "buAutoNum") {
        let scheme = auto.attr(ns::A, "type").unwrap_or("");
        let marker = match scheme {
            t if t.starts_with("alphaLc") => MarkerKind::LowerAlpha,
            t if t.starts_with("alphaUc") => MarkerKind::UpperAlpha,
            t if t.starts_with("romanLc") => MarkerKind::LowerRoman,
            t if t.starts_with("romanUc") => MarkerKind::UpperRoman,
            _ => MarkerKind::Decimal,
        };
        let wrap = match scheme {
            t if t.ends_with("ParenBoth") => NumWrap::ParenBoth,
            t if t.ends_with("ParenR") => NumWrap::ParenR,
            t if t.ends_with("Plain") => NumWrap::Plain,
            _ => NumWrap::Period,
        };
        // ST_TextBulletStartAtNum: 1..=32767; untrusted values are clamped
        // so counters can never overflow.
        let start = auto
            .attr(ns::A, "startAt")
            .and_then(|v| v.parse::<i64>().ok())
            .map_or(1, |v| v.clamp(1, 32767).cast_unsigned());
        Bullet::AutoNum { marker, start, wrap }
    } else if ppr.find(ns::A, "buChar").is_some() {
        Bullet::Char
    } else {
        Bullet::Inherit
    };
    let def_rpr = ppr.find(ns::A, "defRPr");
    let delta = def_rpr.map(rpr_delta).unwrap_or_default();
    let faces = def_rpr.map(rpr_faces).unwrap_or_default();
    TextProps { delta, bullet, faces }
}

/// Delta from an `a:rPr`/`a:defRPr` element's attributes.
pub(super) fn rpr_delta(rpr: &Element) -> StyleDelta {
    let on_off = |name: &str| rpr.attr(ns::A, name).map(|v| matches!(v, "1" | "true" | "on"));
    StyleDelta {
        bold: on_off("b"),
        italic: on_off("i"),
        strike: rpr.attr(ns::A, "strike").map(|v| matches!(v, "sngStrike" | "dblStrike")),
        code: None,
        position: rpr.attr(ns::A, "baseline").and_then(baseline_position),
        hidden: None,
    }
}

/// The shift an `a:rPr@baseline` names: a percentage of the font height, written as a number
/// or with a trailing `%`, whose sign raises or lowers; anything else names none.
fn baseline_position(value: &str) -> Option<VerticalPosition> {
    let number = value.strip_suffix('%').unwrap_or(value);
    if !number.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.')) {
        return None;
    }
    Some(match number.parse::<f64>().ok()?.partial_cmp(&0.0)? {
        Ordering::Greater => VerticalPosition::Superscript,
        Ordering::Less => VerticalPosition::Subscript,
        Ordering::Equal => VerticalPosition::Baseline,
    })
}

/// The Latin and symbol faces an `a:rPr`/`a:defRPr` element names; a theme reference
/// (`+mn-lt`) is a plain face and an empty `typeface` names none.
pub(super) fn rpr_faces(rpr: &Element) -> Slots {
    let face = |name: &str| {
        rpr.find(ns::A, name)
            .and_then(|font| font.attr(ns::A, "typeface"))
            .filter(|typeface| !typeface.is_empty())
            .map_or(Slot::Inherit, Slot::named)
    };
    Slots { latin: face("latin"), symbol: face("sym") }
}

#[derive(Clone, Copy)]
pub(super) struct Ph<'e> {
    pub kind: &'e str,
    pub idx: Option<&'e str>,
}

impl<'e> Ph<'e> {
    pub(super) fn of(shape: &'e Element) -> Option<Self> {
        let ph = shape.first_descendant(ns::P, "ph")?;
        Some(Self { kind: ph.attr(ns::P, "type").unwrap_or("body"), idx: ph.attr(ns::P, "idx") })
    }
}

/// A placeholder shape's identity and level styles inside a layout/master.
#[derive(Debug)]
pub(super) struct Placeholder {
    pub ph_type: String,
    pub idx: Option<String>,
    pub styles: LevelStyle,
}

pub(super) fn collect_placeholders(sp_tree: &Element) -> Vec<Placeholder> {
    sp_tree
        .outermost(ns::P, "sp")
        .filter_map(|sp| {
            let ph = Ph::of(sp)?;
            let lst_style = sp.find(ns::P, "txBody").and_then(|tx| tx.find(ns::A, "lstStyle"));
            Some(Placeholder {
                ph_type: ph.kind.to_string(),
                idx: ph.idx.map(str::to_string),
                styles: parse_level_styles(lst_style),
            })
        })
        .collect()
}

/// Find the placeholder a shape inherits from: match by `idx` first, then by
/// type (title variants unify).
pub(super) fn match_placeholder<'p>(
    placeholders: &'p [Placeholder],
    ph: Ph<'_>,
) -> Option<&'p Placeholder> {
    if let Some(idx) = ph.idx
        && let Some(hit) = placeholders.iter().find(|p| p.idx.as_deref() == Some(idx))
    {
        return Some(hit);
    }
    let class = title_class(ph.kind);
    placeholders
        .iter()
        .find(|p| {
            title_class(&p.ph_type) == class && (class == TitleClass::Title || p.ph_type == ph.kind)
        })
        .or_else(|| placeholders.iter().find(|p| title_class(&p.ph_type) == class))
}

#[derive(PartialEq, Clone, Copy)]
pub(super) enum TitleClass {
    Title,
    Body,
}

pub(super) fn title_class(ph_type: &str) -> TitleClass {
    match ph_type {
        "title" | "ctrTitle" => TitleClass::Title,
        _ => TitleClass::Body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::ParseCtx;
    use crate::package::archive::Role;
    use crate::package::xml::parse_xml;
    use crate::shared::symbol::SymbolFont;
    use VerticalPosition::{Baseline, Subscript, Superscript};
    use std::fmt::Write as _;

    const SYMBOL: Slot = Slot::Symbol(SymbolFont::Symbol);

    fn read<T>(tag: &str, attrs: &str, children: &str, read: impl FnOnce(&Element) -> T) -> T {
        let xml = format!(r#"<a:{tag} xmlns:a="{}" {attrs}>{children}</a:{tag}>"#, ns::A);
        let root = parse_xml(&ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap();
        read(root.find(ns::A, tag).unwrap())
    }

    fn level(def_rpr: &str) -> TextProps {
        read("lvl1pPr", "", def_rpr, paragraph_props)
    }

    #[test]
    fn a_level_style_reads_each_of_its_nine_level_elements() {
        let mut levels = String::new();
        for n in 1..=LEVELS {
            let item = format!(r#"<a:buAutoNum type="arabicPeriod" startAt="{n}"/>"#);
            write!(levels, "<a:lvl{n}pPr>{item}</a:lvl{n}pPr>").unwrap();
        }
        let styles = read("lstStyle", "", &levels, |elem| parse_level_styles(Some(elem)));
        for (lvl, start) in (0..LEVELS).zip(1..) {
            let bullet =
                Bullet::AutoNum { marker: MarkerKind::Decimal, start, wrap: NumWrap::Period };
            assert_eq!(styles.level(lvl).bullet, bullet, "level {}", lvl + 1);
        }
    }

    #[test]
    fn a_baseline_shift_is_a_number_or_a_percentage_whose_sign_raises_or_lowers() {
        for (value, expected) in [
            ("30000", Some(Superscript)),
            ("+5", Some(Superscript)),
            ("-25000", Some(Subscript)),
            ("0", Some(Baseline)),
            ("30%", Some(Superscript)),
            ("0.5%", Some(Superscript)),
            ("-25%", Some(Subscript)),
            ("-0.5%", Some(Subscript)),
            ("0%", Some(Baseline)),
            ("-0%", Some(Baseline)),
            ("", None),
            ("%", None),
            ("abc", None),
            ("1e3", None),
            ("inf", None),
            ("NaN", None),
            ("30 %", None),
            ("3%%", None),
            ("--5", None),
            ("1.2.3", None),
        ] {
            assert_eq!(baseline_position(value), expected, "{value:?}");
        }
    }

    #[test]
    fn a_run_reads_its_baseline_into_the_position_and_an_unreadable_one_leaves_it_unset() {
        for (baseline, expected) in
            [("30000", Some(Superscript)), ("-25%", Some(Subscript)), ("0", Some(Baseline))]
        {
            let delta = read("rPr", &format!(r#"baseline="{baseline}""#), "", rpr_delta);
            assert_eq!(delta.position, expected, "{baseline}");
        }
        for attrs in [r#"baseline="up""#, r#"baseline="""#, ""] {
            assert_eq!(read("rPr", attrs, "", rpr_delta).position, None, "{attrs}");
        }
    }

    #[test]
    fn a_baseline_cascades_through_the_default_run_properties_like_bold() {
        let resolved = |near: TextProps| {
            level(r#"<a:defRPr baseline="30000"/>"#).merge(near).delta.resolve().position
        };
        assert_eq!(resolved(TextProps::default()), Superscript);
        assert_eq!(resolved(level(r#"<a:defRPr baseline="0"/>"#)), Baseline);
        assert_eq!(resolved(level(r#"<a:defRPr baseline="-25000"/>"#)), Subscript);
        assert_eq!(resolved(level(r#"<a:defRPr baseline="bogus"/>"#)), Superscript);
        assert_eq!(resolved(level(r#"<a:defRPr b="1"/>"#)), Superscript);
    }

    #[test]
    fn the_latin_and_symbol_faces_are_classified_when_read() {
        let faces = |children: &str| read("rPr", "", children, rpr_faces);
        assert_eq!(faces(r#"<a:latin typeface="Symbol"/>"#).latin, SYMBOL);
        assert_eq!(faces(r#"<a:sym typeface="Symbol"/>"#).symbol, SYMBOL);
        assert_eq!(faces(r#"<a:latin typeface="Arial"/>"#).latin, Slot::Plain);
        assert_eq!(faces(r#"<a:latin typeface="+mn-lt"/>"#).latin, Slot::Plain);
        assert_eq!(faces(r#"<a:sym typeface="+mj-lt"/>"#).symbol, Slot::Plain);
        for children in [
            "",
            r#"<a:latin typeface=""/>"#,
            "<a:latin/>",
            r#"<a:ea typeface="Symbol"/><a:cs typeface="Symbol"/>"#,
        ] {
            assert_eq!(faces(children), Slots::default(), "{children}");
        }
        let both = faces(r#"<a:latin typeface="Wingdings"/><a:sym typeface="Symbol"/>"#);
        assert_eq!(both.latin, Slot::Symbol(SymbolFont::Wingdings));
        assert_eq!(both.symbol, SYMBOL);
    }

    #[test]
    fn a_face_cascades_through_the_default_run_properties() {
        let resolved = |near: TextProps| {
            level(r#"<a:defRPr><a:latin typeface="Symbol"/></a:defRPr>"#).merge(near).faces
        };
        assert_eq!(resolved(TextProps::default()).latin, SYMBOL);
        assert_eq!(resolved(level(r#"<a:defRPr b="1"/>"#)).latin, SYMBOL);
        assert_eq!(
            resolved(level(r#"<a:defRPr><a:latin typeface="Arial"/></a:defRPr>"#)).latin,
            Slot::Plain
        );
        assert_eq!(
            resolved(level(r#"<a:defRPr><a:latin typeface="+mn-lt"/></a:defRPr>"#)).latin,
            Slot::Plain
        );
        let symbol = resolved(level(r#"<a:defRPr><a:sym typeface="Symbol"/></a:defRPr>"#));
        assert_eq!((symbol.latin, symbol.symbol), (SYMBOL, SYMBOL));
    }
}
