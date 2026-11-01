use super::symbol::{self, SymbolFont, Token};
use super::text::clean_text;
use crate::model::{Inline, Style};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Slot {
    #[default]
    Inherit,
    Plain,
    Symbol(SymbolFont),
}

impl Slot {
    pub(crate) fn named(face: &str) -> Self {
        SymbolFont::from_name(face).map_or(Self::Plain, Self::Symbol)
    }

    fn over(self, base: Self) -> Self {
        if self == Self::Inherit { base } else { self }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Slots {
    pub latin: Slot,
    pub symbol: Slot,
}

impl Slots {
    pub(crate) fn merge(self, over: Self) -> Self {
        Self { latin: over.latin.over(self.latin), symbol: over.symbol.over(self.symbol) }
    }

    pub(crate) fn route(self) -> Route {
        let latin = match self.latin {
            Slot::Symbol(font) => Some(font),
            Slot::Inherit | Slot::Plain => None,
        };
        let symbol = match self.symbol {
            Slot::Inherit => latin,
            Slot::Symbol(font) => Some(font),
            Slot::Plain => None,
        };
        Route { latin, symbol }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Route {
    latin: Option<SymbolFont>,
    symbol: Option<SymbolFont>,
}

impl Route {
    pub(crate) fn font_of(self, c: char) -> Option<SymbolFont> {
        if ('\u{f000}'..='\u{f0ff}').contains(&c) { self.symbol } else { self.latin }
    }
}

pub(crate) fn run_inlines(text: &str, route: Route, style: Style) -> (Vec<Inline>, u64) {
    let decoded = symbol::decode_run(text, |c| route.font_of(c));
    let inlines = decoded
        .tokens
        .into_iter()
        .filter_map(|token| match token {
            Token::Text(text) => Some(clean_text(&text))
                .filter(|text| !text.is_empty())
                .map(|text| Inline::Text { text, style }),
            Token::Checkbox(checked) => Some(Inline::Checkbox(checked)),
        })
        .collect();
    (inlines, decoded.unmapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYMBOL: Slot = Slot::Symbol(SymbolFont::Symbol);
    const WINGDINGS: Slot = Slot::Symbol(SymbolFont::Wingdings);

    fn slots(latin: Slot, symbol: Slot) -> Slots {
        Slots { latin, symbol }
    }

    #[test]
    fn a_face_name_sets_a_symbol_or_a_plain_slot() {
        for (face, expected) in [
            ("Symbol", SYMBOL),
            ("wingdings", WINGDINGS),
            ("Arial", Slot::Plain),
            ("+mn-lt", Slot::Plain),
            ("+mj-lt", Slot::Plain),
            ("", Slot::Plain),
        ] {
            assert_eq!(Slot::named(face), expected, "{face:?}");
        }
    }

    #[test]
    fn the_nearer_layer_wins_a_slot_it_specifies_and_inherits_the_rest() {
        let base = slots(SYMBOL, WINGDINGS);
        assert_eq!(base.merge(Slots::default()), base);
        assert_eq!(Slots::default().merge(base), base);
        assert_eq!(base.merge(slots(Slot::Plain, Slot::Inherit)), slots(Slot::Plain, WINGDINGS));
        assert_eq!(base.merge(slots(Slot::Inherit, Slot::Plain)), slots(SYMBOL, Slot::Plain));
    }

    #[test]
    fn symbol_range_characters_read_through_the_symbol_slot_else_the_latin_one() {
        let symbol = Some(SymbolFont::Symbol);
        let wingdings = Some(SymbolFont::Wingdings);
        for (case, slots, latin, private) in [
            ("nothing set", Slots::default(), None, None),
            ("latin only", slots(SYMBOL, Slot::Inherit), symbol, symbol),
            ("symbol only", slots(Slot::Inherit, WINGDINGS), None, wingdings),
            ("both", slots(SYMBOL, WINGDINGS), symbol, wingdings),
            ("a plain symbol slot", slots(SYMBOL, Slot::Plain), symbol, None),
            ("a plain latin slot", slots(Slot::Plain, WINGDINGS), None, wingdings),
        ] {
            let route = slots.route();
            assert_eq!(route.font_of('m'), latin, "{case}: m");
            assert_eq!(route.font_of('\u{f06d}'), private, "{case}: U+F06D");
        }
    }

    #[test]
    fn the_symbol_range_is_exactly_u_f000_to_u_f0ff() {
        let route = slots(Slot::Plain, SYMBOL).route();
        for (c, expected) in [
            ('\u{efff}', None),
            ('\u{f000}', Some(SymbolFont::Symbol)),
            ('\u{f0ff}', Some(SymbolFont::Symbol)),
            ('\u{f100}', None),
        ] {
            assert_eq!(route.font_of(c), expected, "{c:?}");
        }
    }

    #[test]
    fn runs_that_read_alike_have_equal_routes() {
        assert_eq!(slots(Slot::Plain, Slot::Inherit).route(), Slots::default().route());
        assert_eq!(slots(SYMBOL, Slot::Inherit).route(), slots(SYMBOL, SYMBOL).route());
        assert_ne!(slots(SYMBOL, Slot::Plain).route(), slots(SYMBOL, Slot::Inherit).route());
    }

    #[test]
    fn a_run_reads_its_codes_cleans_its_text_and_counts_the_codes_no_table_maps() {
        let style = Style { bold: true, ..Style::PLAIN };
        let text = |text: &str| Inline::Text { text: text.to_owned(), style };
        let wingdings = slots(WINGDINGS, Slot::Inherit).route();
        let (inlines, unmapped) = run_inlines("\u{fe}\u{fc}\u{ff}", wingdings, style);
        assert_eq!((inlines, unmapped), (vec![Inline::Checkbox(true), text("\u{2713}")], 1));
        let plain = Slots::default().route();
        assert_eq!(run_inlines("a\u{a0}b\u{200b}", plain, style), (vec![text("a b")], 0));
        assert_eq!(run_inlines("\u{200b}", plain, style), (Vec::new(), 0));
    }
}
