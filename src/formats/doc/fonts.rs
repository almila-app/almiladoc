//! The font table (`SttbfFfn`) and the rule that picks the font each character is drawn in.

use super::Fib;
use super::sprm::{Chp, Direct, HINT_COMPLEX, HINT_EAST_ASIA, Symbol};
use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::shared::binary::{ByteReader, utf16le_units};
use crate::shared::symbol::{self, Glyph, SymbolFont};

/// [MS-DOC] STTB `fExtend`, which `SttbfFfn` never has.
const F_EXTEND: u16 = 0xFFFF;
/// [MS-DOC] FFN: the offset of `xszFfn`, past `prq` through `fs`.
pub(super) const FFN_NAME: usize = 39;

/// How a font draws the characters set in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Face {
    Text,
    Glyphs(SymbolFont),
    /// A font the file names but whose entry does not read.
    Unresolved,
}

/// Each font table entry's face, and the face of an index past them.
#[derive(Debug)]
pub(super) struct FontTable {
    faces: Vec<Face>,
    past: Face,
}

impl Default for FontTable {
    fn default() -> Self {
        Self { faces: Vec::new(), past: Face::Text }
    }
}

impl FontTable {
    fn unreadable() -> Self {
        Self { faces: Vec::new(), past: Face::Unresolved }
    }

    fn face(&self, ftc: u16) -> Face {
        self.faces.get(usize::from(ftc)).copied().unwrap_or(self.past)
    }

    /// What `c`, set with the properties `chp`, draws, and the index of its font when that is
    /// unresolved: a `sprmCSymbol` draws its own character in its own font in place of `c`.
    pub(super) fn draw(&self, chp: &Chp, c: char) -> (Glyph, Option<u16>) {
        let (ftc, c) = match chp.direct.symbol {
            Some(Symbol { ftc, xchar }) => match char::from_u32(u32::from(xchar)) {
                Some(xchar) => (ftc, xchar),
                None => return (Glyph::Unmapped, None),
            },
            None => (slot(chp, c), c),
        };
        match self.face(ftc) {
            Face::Glyphs(font) => (symbol::glyph_of(font, c), None),
            Face::Text => (text_glyph(c), None),
            Face::Unresolved => (text_glyph(c), Some(ftc)),
        }
    }
}

/// A text font draws a character as itself, and a symbol-font code as nothing.
fn text_glyph(c: char) -> Glyph {
    if ('\u{f000}'..='\u{f0ff}').contains(&c) { Glyph::Unmapped } else { Glyph::Char(c) }
}

/// The font index the slot rule of [MS-DOC] `sprmCRgFtc0`, `sprmCRgFtc1`, `sprmCRgFtc2` and
/// `sprmCFtcBi` gives `c`.
fn slot(chp: &Chp, c: char) -> u16 {
    let Direct { hint, bidi, complex, .. } = chp.direct;
    if bidi || complex || hint == HINT_COMPLEX {
        chp.fonts.bidi
    } else if hint == HINT_EAST_ASIA && symbol::east_asian_ambiguous(c) {
        chp.fonts.east_asia
    } else if (' '..='\u{7f}').contains(&c) {
        chp.fonts.ascii
    } else {
        chp.fonts.other
    }
}

/// The font table the FIB names, each entry charged and read once: an absent or empty table makes
/// every font a text font, and an entry that does not read, or an index past the table, is
/// unresolved.
pub(super) fn parse(
    parse_ctx: &ParseCtx<'_>,
    fib: Fib<'_>,
    table: &[u8],
) -> Result<FontTable, ConvertError> {
    let Some(sttb) = fib.table_range(table, Fib::FC_STTBF_FFN) else {
        return Ok(FontTable::unreadable());
    };
    if sttb.is_empty() {
        return Ok(FontTable::default());
    }
    let mut reader = ByteReader::new(sttb);
    let (Some(c_data), Some(cb_extra)) = (reader.u16(), reader.u16()) else {
        return Ok(FontTable::unreadable());
    };
    if c_data == F_EXTEND {
        return Ok(FontTable::unreadable());
    }
    if c_data == 0 {
        return Ok(FontTable::default());
    }
    let mut faces = Vec::with_capacity(usize::from(c_data).min(sttb.len()));
    for _ in 0..c_data {
        parse_ctx.charge(1)?;
        let Some(ffn) = reader.u8().and_then(|cch| reader.take(usize::from(cch))) else {
            break;
        };
        faces.push(face_of(ffn));
        if reader.skip(usize::from(cb_extra)).is_none() {
            break;
        }
    }
    Ok(FontTable { faces, past: Face::Unresolved })
}

/// The face of the font an FFN names: a symbol font its name names, else a text font; an entry
/// too short to hold one UTF-16 unit of name is unresolved.
fn face_of(ffn: &[u8]) -> Face {
    let Some(name) = ffn.get(FFN_NAME..).filter(|name| name.len() >= size_of::<u16>()) else {
        return Face::Unresolved;
    };
    let mut units = utf16le_units(name).take_while(|&unit| unit != 0);
    let name: Option<String> = units
        .by_ref()
        .take(SymbolFont::LONGEST_NAME)
        .map(|unit| u8::try_from(unit).ok().filter(u8::is_ascii).map(char::from))
        .collect();
    match name {
        Some(name) if units.next().is_none() => {
            SymbolFont::from_name(&name).map_or(Face::Text, Face::Glyphs)
        }
        _ => Face::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{SYMBOL_CHARSET, ffn, ffn_in, sttbf_ffn};
    use super::*;
    use crate::formats::doc::sprm::Fonts;
    use crate::ledger::CancelToken;
    use crate::limits::{Limit, Limits};
    use crate::shared::test_support::cancelled;
    use std::assert_matches;

    fn table_of(parse_ctx: &ParseCtx<'_>, sttb: &[u8]) -> Result<FontTable, ConvertError> {
        let mut fib = [0u8; super::super::FIB_MIN];
        let lcb = u32::try_from(sttb.len()).unwrap().to_le_bytes();
        fib[Fib::FC_STTBF_FFN + 4..][..4].copy_from_slice(&lcb);
        parse(parse_ctx, Fib(&fib), sttb)
    }

    fn faces(sttb: &[u8]) -> (Vec<Face>, Face) {
        let table = table_of(&ParseCtx::default(), sttb).unwrap();
        (table.faces, table.past)
    }

    #[test]
    fn each_entry_reads_as_a_text_symbol_or_unresolved_face() {
        use Face::{Glyphs, Text, Unresolved};
        let symbol_set = |name| ffn_in(name, SYMBOL_CHARSET);
        let named =
            sttbf_ffn(&[ffn("Times New Roman"), symbol_set("Symbol"), symbol_set("Marlett")]);
        assert_eq!(faces(&named), (vec![Text, Glyphs(SymbolFont::Symbol), Text], Unresolved));
        let mut one_unit = ffn("S");
        one_unit.truncate(FFN_NAME + 2);
        let mut half_past = one_unit.clone();
        half_past.push(0);
        let short = [FFN_NAME - 1, FFN_NAME, FFN_NAME + 1].map(|len| vec![0; len]);
        let short = sttbf_ffn(&[&short[..], &[one_unit, half_past, ffn("wingdings")]].concat());
        let expected = vec![Unresolved, Unresolved, Unresolved, Text, Text];
        let wingdings = Glyphs(SymbolFont::Wingdings);
        assert_eq!(faces(&short), ([expected, vec![wingdings]].concat(), Unresolved));
        let mut unterminated = ffn("Symbol");
        unterminated.truncate(unterminated.len() - 2);
        let odd = [ffn("Symbol X"), ffn("Symbo\u{e9}"), ffn(""), unterminated];
        let expected = vec![Text, Text, Text, Glyphs(SymbolFont::Symbol)];
        assert_eq!(faces(&sttbf_ffn(&odd)), (expected, Unresolved));
        let mut cut = sttbf_ffn(&[ffn("Symbol"), ffn("Webdings")]);
        cut.pop();
        assert_eq!(faces(&cut), (vec![Glyphs(SymbolFont::Symbol)], Unresolved));
        let mut extra = sttbf_ffn(&[]);
        extra.splice(0..4, [2, 0, 1, 0]);
        for entry in [ffn("Symbol"), ffn("Wingdings")] {
            extra.push(u8::try_from(entry.len()).unwrap());
            extra.extend(entry);
            extra.push(9);
        }
        let both = vec![Glyphs(SymbolFont::Symbol), Glyphs(SymbolFont::Wingdings)];
        assert_eq!(faces(&extra), (both, Unresolved));
    }

    #[test]
    fn a_table_absent_or_empty_is_all_text_and_one_that_does_not_read_is_all_unresolved() {
        assert_eq!(faces(&[]), (vec![], Face::Text));
        assert_eq!(faces(&sttbf_ffn(&[])), (vec![], Face::Text));
        for unreadable in [&[1, 0][..], &[0xFF, 0xFF, 0, 0, 0]] {
            assert_eq!(faces(unreadable), (vec![], Face::Unresolved), "{unreadable:?}");
        }
        let mut fib = [0u8; super::super::FIB_MIN];
        fib[Fib::FC_STTBF_FFN + 4] = 4;
        let outside = parse(&ParseCtx::default(), Fib(&fib), &[0, 0]).unwrap();
        assert_eq!((outside.faces, outside.past), (vec![], Face::Unresolved));
    }

    #[test]
    fn each_entry_is_charged_one_unit_before_it_is_read() {
        let sttb = sttbf_ffn(&[ffn("Symbol"), ffn("Arial"), ffn("Wingdings")]);
        let parse_ctx = ParseCtx::default();
        table_of(&parse_ctx, &sttb).unwrap();
        assert_eq!(parse_ctx.fuel(), 3);
        let limits = Limits { max_work: 2, ..Limits::DEFAULT };
        let refused = table_of(&ParseCtx::new(&limits), &sttb);
        assert_matches!(refused, Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 2 }));
        let token = CancelToken::new();
        assert_matches!(table_of(&cancelled(&token), &sttb), Err(ConvertError::Cancelled));
    }

    fn chp(direct: Direct) -> Chp {
        let fonts = Fonts { ascii: 0, east_asia: 1, other: 2, bidi: 3 };
        Chp { fonts, direct, ..Chp::default() }
    }

    #[test]
    fn the_slot_rule_picks_one_font_by_script_hint_and_character() {
        let hinted = |hint| Direct { hint, ..Direct::default() };
        let rows = [
            ("bidi", Direct { bidi: true, ..Direct::default() }, 'a', 3),
            ("complex scripts", Direct { complex: true, ..Direct::default() }, '\u{b1}', 3),
            ("complex hint", hinted(HINT_COMPLEX), 'a', 3),
            (
                "bidi over an east asian hint",
                Direct { bidi: true, hint: 1, ..Direct::default() },
                '\u{b1}',
                3,
            ),
            ("east asian hint, ambiguous", hinted(HINT_EAST_ASIA), '\u{b1}', 1),
            ("east asian hint, private use", hinted(HINT_EAST_ASIA), '\u{f06d}', 1),
            ("east asian hint, ascii", hinted(HINT_EAST_ASIA), 'a', 0),
            ("east asian hint, other", hinted(HINT_EAST_ASIA), '\u{e9}', 2),
            ("no hint, ambiguous", Direct::default(), '\u{b1}', 2),
            ("space", Direct::default(), ' ', 0),
            ("delete", Direct::default(), '\u{7f}', 0),
            ("below space", Direct::default(), '\u{1f}', 2),
            ("past ascii", Direct::default(), '\u{80}', 2),
            ("private use", Direct::default(), '\u{f06d}', 2),
            ("unknown hint", hinted(3), '\u{b1}', 2),
        ];
        for (case, direct, c, expected) in rows {
            assert_eq!(slot(&chp(direct), c), expected, "{case}");
        }
    }

    #[test]
    fn a_character_draws_through_its_slot_or_its_symbol_operand() {
        let table = FontTable {
            faces: vec![Face::Glyphs(SymbolFont::Symbol), Face::Text, Face::Unresolved],
            past: Face::Unresolved,
        };
        let symbol =
            |ftc, xchar| Direct { symbol: Some(Symbol { ftc, xchar }), ..Direct::default() };
        let rows = [
            ("ascii in Symbol", Direct::default(), 'm', (Glyph::Char('\u{3bc}'), None)),
            (
                "private use through a text slot",
                Direct::default(),
                '\u{f06d}',
                (Glyph::Unmapped, Some(2)),
            ),
            (
                "text through an unresolved slot",
                Direct::default(),
                '\u{b1}',
                (Glyph::Char('\u{b1}'), Some(2)),
            ),
            (
                "past the table",
                Direct { bidi: true, ..Direct::default() },
                'a',
                (Glyph::Char('a'), Some(3)),
            ),
            ("symbol in Symbol", symbol(0, 0xF06D), '(', (Glyph::Char('\u{3bc}'), None)),
            ("symbol low code in Symbol", symbol(0, 0x6D), 'x', (Glyph::Char('\u{3bc}'), None)),
            ("symbol undefined in Symbol", symbol(0, 0xF0FF), '(', (Glyph::Unmapped, None)),
            ("symbol in a text font", symbol(1, 0xB1), '(', (Glyph::Char('\u{b1}'), None)),
            ("private symbol in a text font", symbol(1, 0xF06D), '(', (Glyph::Unmapped, None)),
            ("symbol in an unresolved font", symbol(2, 0x61), '(', (Glyph::Char('a'), Some(2))),
            ("surrogate symbol", symbol(0, 0xD800), '(', (Glyph::Unmapped, None)),
        ];
        for (case, direct, c, expected) in rows {
            assert_eq!(table.draw(&chp(direct), c), expected, "{case}");
        }
    }
}
