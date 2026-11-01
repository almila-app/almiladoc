//! `encoding_rs` follows WHATWG mappings, which differ from some Microsoft and Apple code pages.

use encoding_rs::{
    BIG5, EUC_KR, Encoding, GB18030, GBK, IBM866, ISO_8859_2, ISO_8859_3, ISO_8859_4, ISO_8859_5,
    ISO_8859_6, ISO_8859_7, ISO_8859_8, ISO_8859_13, ISO_8859_15, KOI8_R, KOI8_U, MACINTOSH,
    SHIFT_JIS, UTF_8, WINDOWS_874, WINDOWS_1250, WINDOWS_1251, WINDOWS_1252, WINDOWS_1253,
    WINDOWS_1254, WINDOWS_1255, WINDOWS_1256, WINDOWS_1257, WINDOWS_1258, X_MAC_CYRILLIC,
};
use std::ops::BitOrAssign;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Codepage {
    table: Table,
    pub assumed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    Whatwg(&'static Encoding),
    Upper(&'static [char; 128]),
}

impl Codepage {
    pub(crate) const fn declared(encoding: &'static Encoding) -> Self {
        Self { table: Table::Whatwg(encoding), assumed: false }
    }

    pub(crate) const fn marked_assumed(self) -> Self {
        Self { assumed: true, ..self }
    }

    pub(crate) fn decode(self, bytes: &[u8]) -> (String, TextLoss) {
        let assumed = self.assumed && !bytes.is_ascii();
        let (text, invalid) = match self.table {
            Table::Whatwg(encoding) => {
                let (text, invalid) = encoding.decode_without_bom_handling(bytes);
                (text.into_owned(), invalid)
            }
            Table::Upper(upper) => {
                let text = bytes
                    .iter()
                    .map(|&b| match b.checked_sub(0x80) {
                        Some(high) => upper[usize::from(high)],
                        None => char::from(b),
                    })
                    .collect();
                (text, false)
            }
        };
        (text, TextLoss { invalid, assumed })
    }
}

impl Codepage {
    pub(crate) fn decode_marked(self, bytes: &[u8]) -> (String, Vec<std::ops::Range<usize>>) {
        let Table::Whatwg(encoding) = self.table else {
            return (self.decode(bytes).0, Vec::new());
        };
        let mut decoder = encoding.new_decoder_without_bom_handling();
        let capacity = decoder.max_utf8_buffer_length_without_replacement(bytes.len());
        let mut text = String::with_capacity(capacity.unwrap_or(bytes.len()));
        let mut invalid = Vec::new();
        let mut rest = bytes;
        loop {
            let (result, read) =
                decoder.decode_to_string_without_replacement(rest, &mut text, true);
            rest = rest.get(read..).unwrap_or_default();
            match result {
                encoding_rs::DecoderResult::InputEmpty => break,
                encoding_rs::DecoderResult::OutputFull => text.reserve(rest.len().max(1) * 3),
                encoding_rs::DecoderResult::Malformed(..) => {
                    let at = text.len();
                    text.push(char::REPLACEMENT_CHARACTER);
                    invalid.push(at..text.len());
                }
            }
        }
        (text, invalid)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TextLoss {
    pub invalid: bool,
    pub assumed: bool,
}

impl BitOrAssign for TextLoss {
    fn bitor_assign(&mut self, other: Self) {
        self.invalid |= other.invalid;
        self.assumed |= other.assumed;
    }
}

pub(crate) fn windows(cp: u16) -> Codepage {
    let encoding = match cp {
        437 => return Codepage { table: Table::Upper(&CP437), assumed: false },
        850 => return Codepage { table: Table::Upper(&CP850), assumed: false },
        866 => IBM866,
        874 => WINDOWS_874,
        932 => SHIFT_JIS,
        936 => GBK,
        949 => EUC_KR,
        950 => BIG5,
        1250 => WINDOWS_1250,
        1251 => WINDOWS_1251,
        1252 | 28591 => WINDOWS_1252,
        1253 => WINDOWS_1253,
        1254 | 28599 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        1257 => WINDOWS_1257,
        1258 => WINDOWS_1258,
        10000 => MACINTOSH,
        10007 => X_MAC_CYRILLIC,
        20866 => KOI8_R,
        21866 => KOI8_U,
        28592 => ISO_8859_2,
        28593 => ISO_8859_3,
        28594 => ISO_8859_4,
        28595 => ISO_8859_5,
        28596 => ISO_8859_6,
        28597 => ISO_8859_7,
        28598 | 38598 => ISO_8859_8,
        28603 => ISO_8859_13,
        28605 => ISO_8859_15,
        54936 => GB18030,
        65001 => UTF_8,
        _ => return Codepage::declared(WINDOWS_1252).marked_assumed(),
    };
    Codepage::declared(encoding)
}

pub(crate) fn charset(fcharset: i32, default: Codepage) -> Codepage {
    let encoding = match fcharset {
        0..=2 => return default,
        77 => MACINTOSH,
        89 => X_MAC_CYRILLIC,
        128 => SHIFT_JIS,
        129 => EUC_KR,
        134 => GBK,
        136 => BIG5,
        161 => WINDOWS_1253,
        162 => WINDOWS_1254,
        163 => WINDOWS_1258,
        177 => WINDOWS_1255,
        178..=180 => WINDOWS_1256,
        186 => WINDOWS_1257,
        204 => WINDOWS_1251,
        222 => WINDOWS_874,
        238 => WINDOWS_1250,
        _ => return default.marked_assumed(),
    };
    Codepage::declared(encoding)
}

// Unicode's CP437.TXT and CP850.TXT define these tables, outside encoding_rs.
const CP437: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

const CP850: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', 'ø', '£', 'Ø', '×', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '®', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', 'Á', 'Â', 'À', '©', '╣', '║', '╗', '╝', '¢', '¥', '┐', //
    '└', '┴', '┬', '├', '─', '┼', 'ã', 'Ã', '╚', '╔', '╩', '╦', '╠', '═', '╬', '¤', //
    'ð', 'Ð', 'Ê', 'Ë', 'È', 'ı', 'Í', 'Î', 'Ï', '┘', '┌', '█', '▄', '¦', 'Ì', '▀', //
    'Ó', 'ß', 'Ô', 'Ò', 'õ', 'Õ', 'µ', 'þ', 'Þ', 'Ú', 'Û', 'Ù', 'ý', 'Ý', '¯', '´', //
    '\u{ad}', '±', '‗', '¾', '¶', '§', '÷', '¸', '°', '¨', '·', '¹', '³', '²', '■', '\u{a0}',
];

#[cfg(test)]
mod tests {
    use super::*;

    const CP437_TXT: &str = "\
        00C7 00FC 00E9 00E2 00E4 00E0 00E5 00E7 00EA 00EB 00E8 00EF 00EE 00EC 00C4 00C5 \
        00C9 00E6 00C6 00F4 00F6 00F2 00FB 00F9 00FF 00D6 00DC 00A2 00A3 00A5 20A7 0192 \
        00E1 00ED 00F3 00FA 00F1 00D1 00AA 00BA 00BF 2310 00AC 00BD 00BC 00A1 00AB 00BB \
        2591 2592 2593 2502 2524 2561 2562 2556 2555 2563 2551 2557 255D 255C 255B 2510 \
        2514 2534 252C 251C 2500 253C 255E 255F 255A 2554 2569 2566 2560 2550 256C 2567 \
        2568 2564 2565 2559 2558 2552 2553 256B 256A 2518 250C 2588 2584 258C 2590 2580 \
        03B1 00DF 0393 03C0 03A3 03C3 00B5 03C4 03A6 0398 03A9 03B4 221E 03C6 03B5 2229 \
        2261 00B1 2265 2264 2320 2321 00F7 2248 00B0 2219 00B7 221A 207F 00B2 25A0 00A0";

    const CP850_TXT: &str = "\
        00C7 00FC 00E9 00E2 00E4 00E0 00E5 00E7 00EA 00EB 00E8 00EF 00EE 00EC 00C4 00C5 \
        00C9 00E6 00C6 00F4 00F6 00F2 00FB 00F9 00FF 00D6 00DC 00F8 00A3 00D8 00D7 0192 \
        00E1 00ED 00F3 00FA 00F1 00D1 00AA 00BA 00BF 00AE 00AC 00BD 00BC 00A1 00AB 00BB \
        2591 2592 2593 2502 2524 00C1 00C2 00C0 00A9 2563 2551 2557 255D 00A2 00A5 2510 \
        2514 2534 252C 251C 2500 253C 00E3 00C3 255A 2554 2569 2566 2560 2550 256C 00A4 \
        00F0 00D0 00CA 00CB 00C8 0131 00CD 00CE 00CF 2518 250C 2588 2584 00A6 00CC 2580 \
        00D3 00DF 00D4 00D2 00F5 00D5 00B5 00FE 00DE 00DA 00DB 00D9 00FD 00DD 00AF 00B4 \
        00AD 00B1 2017 00BE 00B6 00A7 00F7 00B8 00B0 00A8 00B7 00B9 00B3 00B2 25A0 00A0";

    fn upper_half() -> Vec<u8> {
        (0x80..=0xFF).collect()
    }

    fn mapping(txt: &str) -> String {
        txt.split_whitespace()
            .map(|cp| char::from_u32(u32::from_str_radix(cp, 16).unwrap()).unwrap())
            .collect()
    }

    #[test]
    fn the_dos_tables_decode_their_upper_half_as_unicode_publishes_it() {
        for (code_page, published) in [(437, CP437_TXT), (850, CP850_TXT)] {
            assert_eq!(
                windows(code_page).decode(&upper_half()),
                (mapping(published), TextLoss::default()),
                "cp{code_page}"
            );
        }
    }

    #[test]
    fn the_dos_tables_keep_ascii_and_report_a_guess_only_when_assumed() {
        let ascii: Vec<u8> = (0..0x80).collect();
        let text: String = ascii.iter().copied().map(char::from).collect();
        assert_eq!(windows(437).decode(&ascii), (text, TextLoss::default()));
        let assumed = windows(850).marked_assumed();
        assert_eq!(assumed.decode(b"a"), ("a".into(), TextLoss::default()));
        assert_eq!(
            assumed.decode(b"\x82"),
            ("\u{e9}".into(), TextLoss { invalid: false, assumed: true })
        );
    }

    #[test]
    fn the_documented_departures_from_microsoft_and_apple_tables_hold() {
        let decoded = |cp: u16, byte: u8| windows(cp).decode(&[byte]).0;
        assert_eq!(decoded(10000, 0xBD), "\u{3a9}");
        assert_eq!(decoded(10000, 0xDB), "\u{20ac}");
        assert_eq!(decoded(10007, 0xA2), "\u{490}");
        assert_eq!(decoded(10007, 0xB6), "\u{491}");
        assert_eq!(decoded(10007, 0xFF), "\u{20ac}");
        assert_eq!(decoded(28597, 0xA1), "\u{2018}");
        assert_eq!(decoded(28597, 0xA2), "\u{2019}");
        assert_eq!(decoded(28597, 0xA4), "\u{20ac}");
        assert_eq!(decoded(28597, 0xA5), "\u{20af}");
        assert_eq!(decoded(28597, 0xAA), "\u{37a}");
        assert_eq!(decoded(28598, 0xAF), "\u{af}");
        assert_eq!(decoded(28598, 0xFD), "\u{200e}");
        assert_eq!(decoded(28598, 0xFE), "\u{200f}");
        assert_eq!(decoded(1252, 0x81), "\u{81}");
    }

    #[test]
    fn latin_3_latin_4_and_arabic_decode_their_own_letters() {
        let exact = TextLoss::default();
        let invalid = TextLoss { invalid: true, assumed: false };
        assert_eq!(windows(28593).decode(b"\xa1\xfe"), ("\u{126}\u{15d}".into(), exact));
        assert_eq!(windows(28593).decode(b"\xa5"), ("\u{fffd}".into(), invalid));
        assert_eq!(windows(28594).decode(b"\xa2\xbd"), ("\u{138}\u{14a}".into(), exact));
        assert_eq!(windows(28596).decode(b"\xc7\xac"), ("\u{627}\u{60c}".into(), exact));
        assert_eq!(windows(28596).decode(b"\xa1"), ("\u{fffd}".into(), invalid));
    }

    #[test]
    fn a_byte_order_mark_decodes_as_text() {
        let latin = windows(1252).decode(b"\xff\xfeab");
        assert_eq!(latin, ("\u{ff}\u{fe}ab".into(), TextLoss::default()));
        let utf8 = windows(65001).decode(b"\xef\xbb\xbfa");
        assert_eq!(utf8, ("\u{feff}a".into(), TextLoss::default()));
    }

    #[test]
    fn an_unlisted_code_page_is_windows_1252_assumed() {
        for cp in [0, 1200, 1361, 20127, 28600, 65000, u16::MAX] {
            assert_eq!(windows(cp), windows(1252).marked_assumed(), "{cp}");
        }
        assert!(!windows(1252).assumed);
        assert_eq!(windows(28591), windows(1252));
        assert_eq!(windows(28599), windows(1254));
    }

    #[test]
    fn logical_order_hebrew_reads_as_iso_8859_8() {
        assert_eq!(windows(38598), windows(28598));
        assert_eq!(windows(38598).decode(b"\xe0"), ("\u{5d0}".into(), TextLoss::default()));
    }

    #[test]
    fn a_charset_names_its_code_page_or_keeps_the_default() {
        let default = windows(1251);
        for cs in [0, 1, 2] {
            assert_eq!(charset(cs, default), default, "{cs}");
        }
        assert_eq!(charset(77, default), windows(10000));
        assert_eq!(charset(89, default), windows(10007));
        assert_eq!(charset(204, windows(1252)), windows(1251));
        for cs in [-1, 3, 78, 254, 255, i32::MAX] {
            assert_eq!(charset(cs, default), default.marked_assumed(), "{cs}");
        }
    }
}
