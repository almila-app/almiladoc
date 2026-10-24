//! Symbol-font characters as Unicode: the Symbol encoding (ISO 32000-1 Annex D.5) and the rule
//! a document reader follows to read a symbol-font code in run text or in a symbol element.
//!
//! Each D.5 code maps to the character whose Unicode identity is the glyph's: a Greek letter to
//! the Greek-block letter, never a letterlike or operator twin; a delimiter, integral or arrow
//! piece to its piece from U+2320-2321, U+239B-23AF or U+23D0; the serif and sans forms of the
//! registered, copyright and trademark signs to one sign; never a Private Use, control,
//! Deprecated or NFC-changed character. A code D.5 leaves undefined, and 0x60 (radical
//! extension, which Unicode does not encode), has none.
//!
//! Compared against Adobe's `symbol.txt`, Apple's `SYMBOL.TXT`, the Adobe Glyph List and lopdf:
//! this table departs from them only where they give a Private Use value, a
//! compatibility or Deprecated twin, U+00A0 for the space, or a fact error (lopdf's missing Euro
//! and white card suits).

use super::dingbats;

#[rustfmt::skip]
const SYMBOL: [char; 256] = [
    '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0',
    '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0',
    '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0',
    '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0',
    '\u{20}', '\u{21}', '\u{2200}', '\u{23}', '\u{2203}', '\u{25}', '\u{26}', '\u{220b}',
    '\u{28}', '\u{29}', '\u{2217}', '\u{2b}', '\u{2c}', '\u{2212}', '\u{2e}', '\u{2f}',
    '\u{30}', '\u{31}', '\u{32}', '\u{33}', '\u{34}', '\u{35}', '\u{36}', '\u{37}',
    '\u{38}', '\u{39}', '\u{3a}', '\u{3b}', '\u{3c}', '\u{3d}', '\u{3e}', '\u{3f}',
    '\u{2245}', '\u{391}', '\u{392}', '\u{3a7}', '\u{394}', '\u{395}', '\u{3a6}', '\u{393}',
    '\u{397}', '\u{399}', '\u{3d1}', '\u{39a}', '\u{39b}', '\u{39c}', '\u{39d}', '\u{39f}',
    '\u{3a0}', '\u{398}', '\u{3a1}', '\u{3a3}', '\u{3a4}', '\u{3a5}', '\u{3c2}', '\u{3a9}',
    '\u{39e}', '\u{3a8}', '\u{396}', '\u{5b}', '\u{2234}', '\u{5d}', '\u{22a5}', '\u{5f}',
    '\0', '\u{3b1}', '\u{3b2}', '\u{3c7}', '\u{3b4}', '\u{3b5}', '\u{3c6}', '\u{3b3}',
    '\u{3b7}', '\u{3b9}', '\u{3d5}', '\u{3ba}', '\u{3bb}', '\u{3bc}', '\u{3bd}', '\u{3bf}',
    '\u{3c0}', '\u{3b8}', '\u{3c1}', '\u{3c3}', '\u{3c4}', '\u{3c5}', '\u{3d6}', '\u{3c9}',
    '\u{3be}', '\u{3c8}', '\u{3b6}', '\u{7b}', '\u{7c}', '\u{7d}', '\u{223c}', '\0',
    '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0',
    '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0',
    '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0',
    '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0',
    '\u{20ac}', '\u{3d2}', '\u{2032}', '\u{2264}', '\u{2044}', '\u{221e}', '\u{192}', '\u{2663}',
    '\u{2666}', '\u{2665}', '\u{2660}', '\u{2194}', '\u{2190}', '\u{2191}', '\u{2192}', '\u{2193}',
    '\u{b0}', '\u{b1}', '\u{2033}', '\u{2265}', '\u{d7}', '\u{221d}', '\u{2202}', '\u{2022}',
    '\u{f7}', '\u{2260}', '\u{2261}', '\u{2248}', '\u{2026}', '\u{23d0}', '\u{23af}', '\u{21b5}',
    '\u{2135}', '\u{2111}', '\u{211c}', '\u{2118}', '\u{2297}', '\u{2295}', '\u{2205}', '\u{2229}',
    '\u{222a}', '\u{2283}', '\u{2287}', '\u{2284}', '\u{2282}', '\u{2286}', '\u{2208}', '\u{2209}',
    '\u{2220}', '\u{2207}', '\u{ae}', '\u{a9}', '\u{2122}', '\u{220f}', '\u{221a}', '\u{22c5}',
    '\u{ac}', '\u{2227}', '\u{2228}', '\u{21d4}', '\u{21d0}', '\u{21d1}', '\u{21d2}', '\u{21d3}',
    '\u{25ca}', '\u{27e8}', '\u{ae}', '\u{a9}', '\u{2122}', '\u{2211}', '\u{239b}', '\u{239c}',
    '\u{239d}', '\u{23a1}', '\u{23a2}', '\u{23a3}', '\u{23a7}', '\u{23a8}', '\u{23a9}', '\u{23aa}',
    '\0', '\u{27e9}', '\u{222b}', '\u{2320}', '\u{23ae}', '\u{2321}', '\u{239e}', '\u{239f}',
    '\u{23a0}', '\u{23a4}', '\u{23a5}', '\u{23a6}', '\u{23ab}', '\u{23ac}', '\u{23ad}', '\0',
];

/// The font code a symbol-font character stands for: U+F000-F0FF folded to its low byte, and
/// U+0000-00FF as itself; `None` for any other character, which no symbol font recodes.
pub(crate) fn font_code(c: char) -> Option<u8> {
    let value = u32::from(c);
    let folded = if (0xF000..=0xF0FF).contains(&value) { value - 0xF000 } else { value };
    u8::try_from(folded).ok()
}

/// Whether an East Asian font hint draws `c` in the East Asian font slot: the Latin-1 characters
/// MS-OI29500 2.1.88 b lists, and the Private Use Area.
pub(crate) fn east_asian_ambiguous(c: char) -> bool {
    matches!(
        u32::from(c),
        0xA1 | 0xA4
            | 0xA7
            | 0xA8
            | 0xAA
            | 0xAD
            | 0xAF
            | 0xB0..=0xB4
            | 0xB6..=0xBA
            | 0xBC..=0xBF
            | 0xD7
            | 0xF7
            | 0xE000..=0xF8FF
    )
}

/// The code a symbol element's `char` attribute names: one to four hex digits, `None` otherwise.
pub(crate) fn parse_char_code(hex: &str) -> Option<char> {
    if hex.is_empty() || hex.len() > 4 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
}

/// A font that draws its own glyphs at the codes text passes through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SymbolFont {
    Symbol,
    Wingdings,
    Wingdings2,
    Wingdings3,
    Webdings,
    ZapfDingbats,
}

const NAMES: [(&str, SymbolFont); 7] = [
    ("Symbol", SymbolFont::Symbol),
    ("Wingdings", SymbolFont::Wingdings),
    ("Wingdings 2", SymbolFont::Wingdings2),
    ("Wingdings 3", SymbolFont::Wingdings3),
    ("Webdings", SymbolFont::Webdings),
    ("ZapfDingbats", SymbolFont::ZapfDingbats),
    ("Zapf Dingbats", SymbolFont::ZapfDingbats),
];

impl SymbolFont {
    /// The length in bytes of the longest name [`SymbolFont::from_name`] knows.
    pub(crate) const LONGEST_NAME: usize = "Zapf Dingbats".len();

    /// The symbol font a font name names, matched without regard to ASCII case.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        NAMES.iter().find(|(known, _)| known.eq_ignore_ascii_case(name)).map(|&(_, font)| font)
    }

    /// The checkbox state the ballot-box glyph at `code` draws, if it is one.
    fn checkbox(self, code: u8) -> Option<bool> {
        match (self, code) {
            (Self::Wingdings, 0xFE) | (Self::Wingdings2, 0x52) => Some(true),
            (Self::Wingdings, 0x6F) | (Self::Wingdings2, 0xA3) => Some(false),
            _ => None,
        }
    }

    /// The character this font draws at `code`, or `None` where it draws none Unicode encodes.
    fn char_at(self, code: u8) -> Option<char> {
        let table = match self {
            Self::Symbol => &SYMBOL,
            Self::Wingdings => &dingbats::WINGDINGS,
            Self::Wingdings2 => &dingbats::WINGDINGS_2,
            Self::Wingdings3 => &dingbats::WINGDINGS_3,
            Self::Webdings => &dingbats::WEBDINGS,
            Self::ZapfDingbats => &dingbats::ZAPF_DINGBATS,
        };
        Some(table[usize::from(code)]).filter(|&c| c != '\0')
    }

    /// What this font draws at `code`.
    pub(crate) fn glyph(self, code: u8) -> Glyph {
        if let Some(checked) = self.checkbox(code) {
            return Glyph::Checkbox(checked);
        }
        self.char_at(code).map_or(Glyph::Unmapped, Glyph::Char)
    }
}

/// What a symbol font draws at one code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Glyph {
    Char(char),
    Checkbox(bool),
    Unmapped,
}

/// A run's text with each symbol-font code read, in order: text, and the checkboxes drawn
/// between it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    Text(String),
    Checkbox(bool),
}

/// A run read through its fonts: its tokens and how many codes no table maps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Decoded {
    pub tokens: Vec<Token>,
    pub unmapped: u64,
}

impl Decoded {
    fn push_char(&mut self, c: char) {
        if let Some(Token::Text(text)) = self.tokens.last_mut() {
            text.push(c);
        } else {
            self.tokens.push(Token::Text(c.to_string()));
        }
    }
}

/// What `font` draws for the character `c` of text set in it: an ASCII control, or a character no
/// symbol font recodes, stays as it is; any other character is read as its font code.
pub(crate) fn glyph_of(font: SymbolFont, c: char) -> Glyph {
    match font_code(c) {
        Some(code) if !c.is_ascii_control() => font.glyph(code),
        _ => Glyph::Char(c),
    }
}

/// Reads `text` one character at a time through `glyph`, before any text cleaning: text, the
/// checkboxes drawn between it, and how many characters drew nothing.
pub(crate) fn decode_with(text: &str, glyph: impl Fn(char) -> Glyph) -> Decoded {
    let mut decoded = Decoded::default();
    for c in text.chars() {
        match glyph(c) {
            Glyph::Char(mapped) => decoded.push_char(mapped),
            Glyph::Checkbox(checked) => decoded.tokens.push(Token::Checkbox(checked)),
            Glyph::Unmapped => decoded.unmapped += 1,
        }
    }
    decoded
}

/// Reads `text` through the symbol font `font_of` gives each character, by [`glyph_of`].
pub(crate) fn decode_run(text: &str, font_of: impl Fn(char) -> Option<SymbolFont>) -> Decoded {
    decode_with(text, |c| font_of(c).map_or(Glyph::Char(c), |font| glyph_of(font, c)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLEANED: [char; 4] = ['\u{a0}', '\u{ad}', '\u{200b}', '\u{feff}'];
    const TWINS: [char; 5] = ['\u{b5}', '\u{2126}', '\u{2206}', '\u{2329}', '\u{232a}'];

    const FONTS: [SymbolFont; 6] = [
        SymbolFont::Symbol,
        SymbolFont::Wingdings,
        SymbolFont::Wingdings2,
        SymbolFont::Wingdings3,
        SymbolFont::Webdings,
        SymbolFont::ZapfDingbats,
    ];

    fn symbol(code: u8) -> Option<char> {
        SymbolFont::Symbol.char_at(code)
    }

    #[test]
    fn each_table_maps_exactly_its_defined_codes_to_allowed_characters() {
        let below_space = || 0x00..=0x1F;
        let expected: [Vec<u8>; 6] = [
            below_space().chain([0x60]).chain(0x7F..=0x9F).chain([0xF0, 0xFF]).collect(),
            below_space().chain([0x7F, 0xFF]).collect(),
            below_space().chain([0x7F]).chain(0xFA..=0xFF).collect(),
            below_space().chain([0x7F]).chain(0xF1..=0xFF).collect(),
            below_space().chain([0x7F]).collect(),
            below_space().chain([0x7F]).chain(0x8E..=0xA0).chain([0xF0, 0xFF]).collect(),
        ];
        for (font, expected) in FONTS.into_iter().zip(expected) {
            let unmapped: Vec<u8> =
                (0..=u8::MAX).filter(|&code| font.char_at(code).is_none()).collect();
            assert_eq!(unmapped, expected, "{font:?}");
            for c in (0..=u8::MAX).filter_map(|code| font.char_at(code)) {
                assert!(!('\u{e000}'..='\u{f8ff}').contains(&c), "{font:?} {c:?} is Private Use");
                assert!(!c.is_control(), "{font:?} {c:?} is a control");
                assert!(!CLEANED.contains(&c) && !TWINS.contains(&c), "{font:?} {c:?}");
                assert!(!('\u{fe00}'..='\u{fe0f}').contains(&c), "{font:?} {c:?} is a selector");
                let nfc = unicode_normalization::is_nfc(&c.to_string());
                assert!(nfc, "{font:?} {c:?} changes under NFC");
            }
        }
    }

    #[test]
    fn dingbats_read_as_their_sources_map_them() {
        for (font, code, expected) in [
            (SymbolFont::Wingdings, 0xFC, '\u{2713}'),
            (SymbolFont::Wingdings, 0xFB, '\u{1f5f6}'),
            (SymbolFont::Wingdings, 0x26, '\u{1f56e}'),
            (SymbolFont::Wingdings2, 0x43, '\u{1f449}'),
            (SymbolFont::Wingdings3, 0x21, '\u{2b60}'),
            (SymbolFont::Webdings, 0x21, '\u{1f577}'),
            (SymbolFont::Webdings, 0xFF, '\u{1f54a}'),
            (SymbolFont::ZapfDingbats, 0x33, '\u{2713}'),
            (SymbolFont::ZapfDingbats, 0x20, ' '),
            (SymbolFont::ZapfDingbats, 0x80, '\u{2768}'),
            (SymbolFont::ZapfDingbats, 0x8D, '\u{2775}'),
        ] {
            assert_eq!(font.glyph(code), Glyph::Char(expected), "{font:?} {code:#04x}");
        }
    }

    #[test]
    fn a_ballot_box_reads_as_a_checkbox_before_its_table_entry() {
        for (font, code, checked) in [
            (SymbolFont::Wingdings, 0xFE, true),
            (SymbolFont::Wingdings, 0x6F, false),
            (SymbolFont::Wingdings2, 0x52, true),
            (SymbolFont::Wingdings2, 0xA3, false),
        ] {
            assert!(font.char_at(code).is_some(), "{font:?} {code:#04x}");
            assert_eq!(font.glyph(code), Glyph::Checkbox(checked), "{font:?} {code:#04x}");
        }
    }

    #[test]
    fn each_departure_from_the_compared_tables_holds() {
        let rows: [(u8, Option<char>); 24] = [
            (0x20, Some(' ')),
            (0x27, Some('\u{220b}')),
            (0x44, Some('\u{394}')),
            (0x57, Some('\u{3a9}')),
            (0x60, None),
            (0x6D, Some('\u{3bc}')),
            (0xA0, Some('\u{20ac}')),
            (0xA4, Some('\u{2044}')),
            (0xA8, Some('\u{2666}')),
            (0xA9, Some('\u{2665}')),
            (0xBD, Some('\u{23d0}')),
            (0xBE, Some('\u{23af}')),
            (0xD2, Some('\u{ae}')),
            (0xD3, Some('\u{a9}')),
            (0xD4, Some('\u{2122}')),
            (0xE1, Some('\u{27e8}')),
            (0xE2, Some('\u{ae}')),
            (0xE3, Some('\u{a9}')),
            (0xE4, Some('\u{2122}')),
            (0xF0, None),
            (0xF1, Some('\u{27e9}')),
            (0xF3, Some('\u{2320}')),
            (0xF4, Some('\u{23ae}')),
            (0xF5, Some('\u{2321}')),
        ];
        for (code, expected) in rows {
            assert_eq!(symbol(code), expected, "{code:#04x}");
        }
        let pieces =
            "\u{239b}\u{239c}\u{239d}\u{23a1}\u{23a2}\u{23a3}\u{23a7}\u{23a8}\u{23a9}\u{23aa}";
        assert!((0xE6..=0xEF).filter_map(symbol).eq(pieces.chars()));
        let pieces = "\u{239e}\u{239f}\u{23a0}\u{23a4}\u{23a5}\u{23a6}\u{23ab}\u{23ac}\u{23ad}";
        assert!((0xF6..=0xFE).filter_map(symbol).eq(pieces.chars()));
    }

    #[test]
    fn a_font_code_is_a_low_byte_or_its_private_use_form() {
        for (c, expected) in [
            ('m', Some(0x6D)),
            ('\u{f06d}', Some(0x6D)),
            ('\u{0}', Some(0)),
            ('\u{ff}', Some(0xFF)),
            ('\u{f000}', Some(0)),
            ('\u{f0ff}', Some(0xFF)),
            ('\u{100}', None),
            ('\u{efff}', None),
            ('\u{f100}', None),
            ('\u{3bc}', None),
            ('\u{1f600}', None),
        ] {
            assert_eq!(font_code(c), expected, "{c:?}");
        }
    }

    #[test]
    fn a_hint_decides_the_listed_latin_1_characters_and_the_private_use_area() {
        let ambiguous: Vec<u32> =
            (0..=0xFFFF).filter(|&v| char::from_u32(v).is_some_and(east_asian_ambiguous)).collect();
        let listed = [0xA1, 0xA4, 0xA7, 0xA8, 0xAA, 0xAD, 0xAF]
            .into_iter()
            .chain(0xB0..=0xB4)
            .chain(0xB6..=0xBA)
            .chain(0xBC..=0xBF)
            .chain([0xD7, 0xF7])
            .chain(0xE000..=0xF8FF);
        assert!(ambiguous.into_iter().eq(listed));
        assert!(!east_asian_ambiguous('\u{1f600}'));
    }

    #[test]
    fn a_char_code_is_one_to_four_hex_digits() {
        for (hex, expected) in [
            ("6D", Some('m')),
            ("6d", Some('m')),
            ("06D", Some('m')),
            ("F06D", Some('\u{f06d}')),
            ("9", Some('\t')),
            ("", None),
            ("zz", None),
            ("1F06D", None),
            ("+6D", None),
            ("-1", None),
            (" 6D", None),
            ("D800", None),
        ] {
            assert_eq!(parse_char_code(hex), expected, "{hex:?}");
        }
    }

    #[test]
    fn symbol_fonts_are_named_without_regard_to_ascii_case() {
        for (name, expected) in [
            ("Symbol", Some(SymbolFont::Symbol)),
            ("SYMBOL", Some(SymbolFont::Symbol)),
            ("wingdings", Some(SymbolFont::Wingdings)),
            ("Wingdings 2", Some(SymbolFont::Wingdings2)),
            ("Wingdings 3", Some(SymbolFont::Wingdings3)),
            ("Webdings", Some(SymbolFont::Webdings)),
            ("ZapfDingbats", Some(SymbolFont::ZapfDingbats)),
            ("Zapf Dingbats", Some(SymbolFont::ZapfDingbats)),
            ("Symbol ", None),
            ("Wingdings 4", None),
            ("Arial", None),
            ("", None),
        ] {
            assert_eq!(SymbolFont::from_name(name), expected, "{name:?}");
        }
    }

    #[test]
    fn only_the_ballot_boxes_are_checkboxes() {
        let boxes: Vec<(SymbolFont, u8, bool)> = FONTS
            .into_iter()
            .flat_map(|font| (0..=u8::MAX).map(move |code| (font, code)))
            .filter_map(|(font, code)| Some((font, code, font.checkbox(code)?)))
            .collect();
        assert_eq!(
            boxes,
            [
                (SymbolFont::Wingdings, 0x6F, false),
                (SymbolFont::Wingdings, 0xFE, true),
                (SymbolFont::Wingdings2, 0x52, true),
                (SymbolFont::Wingdings2, 0xA3, false),
            ]
        );
    }

    fn text(decoded: &Decoded) -> String {
        decoded
            .tokens
            .iter()
            .map(|token| match token {
                Token::Text(text) => text.as_str(),
                Token::Checkbox(true) => "[x]",
                Token::Checkbox(false) => "[ ]",
            })
            .collect()
    }

    #[test]
    fn a_symbol_run_reads_its_codes_and_keeps_what_no_font_recodes() {
        let decoded = decode_run("10 m\t\u{f06d}\u{a0}\u{ad}\u{3bc}\u{301}\u{1f600}", |_| {
            Some(SymbolFont::Symbol)
        });
        assert_eq!(text(&decoded), "10 \u{3bc}\t\u{3bc}\u{20ac}\u{2191}\u{3bc}\u{301}\u{1f600}");
        assert_eq!(decoded.unmapped, 0);
        let lost =
            decode_run("a`b\u{f009}\u{f0}\u{ff}\u{f0f0}\n\r\u{7f}", |_| Some(SymbolFont::Symbol));
        assert_eq!(text(&lost), "\u{3b1}\u{3b2}\n\r\u{7f}");
        assert_eq!(lost.unmapped, 5);
    }

    #[test]
    fn a_dingbat_run_draws_its_boxes_and_characters_and_counts_the_codes_it_leaves_empty() {
        let decoded = decode_run("\u{fe}a \u{fc}\u{2713}\u{f06f}\u{f0ff}\u{ff}", |_| {
            Some(SymbolFont::Wingdings)
        });
        assert_eq!(
            decoded.tokens,
            [
                Token::Checkbox(true),
                Token::Text("\u{264b} \u{2713}\u{2713}".to_owned()),
                Token::Checkbox(false),
            ]
        );
        assert_eq!(decoded.unmapped, 2);
        let decoded = decode_run("R\u{a3}J\u{fe}", |_| Some(SymbolFont::Wingdings2));
        assert_eq!(
            decoded.tokens,
            [Token::Checkbox(true), Token::Checkbox(false), Token::Text("\u{1f446}".to_owned())]
        );
        assert_eq!(decoded.unmapped, 1);
        assert_eq!(decode_run("", |_| Some(SymbolFont::Webdings)), Decoded::default());
    }

    #[test]
    fn only_an_ascii_control_passes_through_a_symbol_font_unread() {
        assert_eq!(glyph_of(SymbolFont::Wingdings, '\u{81}'), Glyph::Char('\u{2780}'));
        assert_eq!(glyph_of(SymbolFont::Symbol, '\u{9f}'), Glyph::Unmapped);
        for font in FONTS {
            for c in ['\0', '\t', '\u{1f}', '\u{7f}'] {
                assert_eq!(glyph_of(font, c), Glyph::Char(c), "{font:?} {c:?}");
            }
            assert_eq!(glyph_of(font, ' '), Glyph::Char(' '), "{font:?}");
            assert_eq!(glyph_of(font, '\u{f020}'), Glyph::Char(' '), "{font:?}");
        }
    }

    #[test]
    fn the_longest_name_is_the_longest_known() {
        assert_eq!(Some(SymbolFont::LONGEST_NAME), NAMES.iter().map(|(name, _)| name.len()).max());
    }

    #[test]
    fn each_character_is_read_through_its_own_font() {
        let font_of = |c: char| (c == 'm').then_some(SymbolFont::Symbol);
        let decoded = decode_run("mam", font_of);
        assert_eq!(text(&decoded), "\u{3bc}a\u{3bc}");
    }

    proptest::proptest! {
        #[test]
        fn decoding_accounts_for_every_character(
            text in ".{0,40}",
            font in proptest::option::of(proptest::sample::select(FONTS.to_vec())),
        ) {
            let decoded = decode_run(&text, |_| font);
            let kept: usize = decoded
                .tokens
                .iter()
                .map(|token| match token {
                    Token::Text(text) => text.chars().count(),
                    Token::Checkbox(_) => 1,
                })
                .sum();
            let unmapped = usize::try_from(decoded.unmapped).unwrap();
            proptest::prop_assert_eq!(kept + unmapped, text.chars().count());
            if font.is_none() {
                proptest::prop_assert_eq!(unmapped, 0);
                proptest::prop_assert_eq!(self::text(&decoded), text);
            }
        }
    }
}
