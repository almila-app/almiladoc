//! Position-explicit RTF lexer. `\binN` consumes exactly N raw bytes, so
//! binary payloads can never corrupt group state; unbalanced groups are the
//! caller's to recover and report.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;

pub(super) enum Token<'a> {
    Open,
    Close,
    /// Control word with optional numeric parameter.
    Word {
        name: &'a str,
        param: Option<i32>,
    },
    /// Control symbol (`\~`, `\*`, `\{`, ...).
    Symbol(u8),
    /// `\'xx` hex-escaped byte in the current code page.
    Hex(u8),
    /// One plain text byte.
    Byte(u8),
    /// The raw payload of a `\binN` control.
    Bin(&'a [u8]),
}

pub(super) struct Lexer<'a> {
    parse_ctx: &'a ParseCtx<'a>,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub(super) fn new(parse_ctx: &'a ParseCtx<'a>, bytes: &'a [u8]) -> Self {
        Self { parse_ctx, bytes, pos: 0 }
    }

    pub(super) fn pos(&self) -> usize {
        self.pos
    }

    /// The next token, charging one unit of work per token and per skipped
    /// line break or NUL.
    pub(super) fn next_token(&mut self) -> Result<Option<Token<'a>>, ConvertError> {
        loop {
            let Some(&b) = self.bytes.get(self.pos) else {
                return Ok(None);
            };
            self.parse_ctx.charge(1)?;
            self.pos += 1;
            return Ok(match b {
                b'{' => Some(Token::Open),
                b'}' => Some(Token::Close),
                b'\\' => self.control(),
                b'\r' | b'\n' | b'\0' => continue,
                b => Some(Token::Byte(b)),
            });
        }
    }

    fn control(&mut self) -> Option<Token<'a>> {
        let b = *self.bytes.get(self.pos)?;
        if !b.is_ascii_alphabetic() {
            self.pos += 1;
            // A reader treats `\` before CR or LF as a paragraph mark; the
            // trailing LF of a CRLF pair is skipped as plain-text whitespace.
            if b == b'\r' || b == b'\n' {
                return Some(Token::Word { name: "par", param: None });
            }
            if b == b'\'' {
                let pair = self.bytes.get(self.pos..)?.get(..2)?;
                return match (hex_digit(pair[0]), hex_digit(pair[1])) {
                    (Some(hi), Some(lo)) => {
                        self.pos += 2;
                        Some(Token::Hex((hi << 4) | lo))
                    }
                    _ => Some(Token::Byte(b'\'')),
                };
            }
            return Some(Token::Symbol(b));
        }
        let start = self.pos;
        while self.bytes.get(self.pos).is_some_and(u8::is_ascii_alphabetic) {
            self.pos += 1;
        }
        let name = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap_or("");
        let param = self.parameter();
        // One space after a control word is part of the control.
        if self.bytes.get(self.pos) == Some(&b' ') {
            self.pos += 1;
        }
        if name == "bin" {
            let n = usize::try_from(param.unwrap_or(0)).unwrap_or(0);
            let end = self.pos.saturating_add(n).min(self.bytes.len());
            let payload = &self.bytes[self.pos..end];
            self.pos = end;
            return Some(Token::Bin(payload));
        }
        Some(Token::Word { name, param })
    }

    fn parameter(&mut self) -> Option<i32> {
        let negative = self.bytes.get(self.pos) == Some(&b'-');
        let start = self.pos + usize::from(negative);
        let rest = self.bytes.get(start..)?;
        let len = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        if len == 0 {
            return None;
        }
        self.pos = start + len;
        let n: i64 = std::str::from_utf8(&rest[..len]).ok()?.parse().ok()?;
        let n = if negative { -n } else { n };
        Some(i32::try_from(n).unwrap_or(if n < 0 { i32::MIN } else { i32::MAX }))
    }
}

pub(super) fn hex_digit(b: u8) -> Option<u8> {
    char::from(b).to_digit(16).and_then(|d| u8::try_from(d).ok())
}

#[cfg(test)]
pub(super) fn fuel(src: &str) -> u64 {
    let parse_ctx = ParseCtx::default();
    let mut lexer = Lexer::new(&parse_ctx, src.as_bytes());
    while lexer.next_token().unwrap().is_some() {}
    parse_ctx.fuel()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn bin_payload_consumed_raw() {
        // Payload bytes are braces and backslashes; they must not lex.
        let src = br"{\rtf1 a\bin5 }}{\\x b}";
        let parse_ctx = ParseCtx::default();
        let mut lexer = Lexer::new(&parse_ctx, src);
        let mut bin: Vec<u8> = Vec::new();
        let mut opens = 0;
        let mut closes = 0;
        while let Some(t) = lexer.next_token().unwrap() {
            match t {
                Token::Bin(p) => bin.extend_from_slice(p),
                Token::Open => opens += 1,
                Token::Close => closes += 1,
                _ => {}
            }
        }
        assert_eq!(bin, br"}}{\\");
        assert_eq!((opens, closes), (1, 1));
    }

    #[test]
    fn a_control_word_parameter_is_signed_clamped_and_absent_without_digits() {
        let cases: [(&str, &[u8], Option<i32>, usize); 10] = [
            ("no parameter", br"\a x", None, 3),
            ("digits", br"\a12x", Some(12), 4),
            ("a minus sign", br"\a-12x", Some(-12), 5),
            ("a lone minus is left to the text", br"\a-x", None, 2),
            ("a minus before a space is left to the text", br"\a- x", None, 2),
            ("the largest i32", br"\a2147483647 x", Some(i32::MAX), 13),
            ("past the largest i32", br"\a2147483648x", Some(i32::MAX), 12),
            ("the smallest i32", br"\a-2147483648x", Some(i32::MIN), 13),
            ("past the smallest i32", br"\a-99999999999x", Some(i32::MIN), 14),
            (
                "past i64 reads no parameter but consumes the digits",
                br"\a9223372036854775808x",
                None,
                21,
            ),
        ];
        for (label, src, expected, consumed) in cases {
            let parse_ctx = ParseCtx::default();
            let mut lexer = Lexer::new(&parse_ctx, src);
            let Some(Token::Word { param, .. }) = lexer.next_token().unwrap() else {
                panic!("{label}: not a control word");
            };
            assert_eq!((param, lexer.pos()), (expected, consumed), "{label}");
        }
    }

    #[test]
    fn backslash_before_a_line_break_is_one_paragraph_mark() {
        for src in [b"{\\rtf1 a\\\nb}".as_slice(), b"{\\rtf1 a\\\r\nb}", b"{\\rtf1 a\\\rb}"] {
            let parse_ctx = ParseCtx::default();
            let mut lexer = Lexer::new(&parse_ctx, src);
            let mut pars = 0;
            let mut symbols = 0;
            while let Some(t) = lexer.next_token().unwrap() {
                match t {
                    Token::Word { name: "par", param: None } => pars += 1,
                    Token::Symbol(_) => symbols += 1,
                    _ => {}
                }
            }
            assert_eq!((pars, symbols), (1, 0), "source: {:?}", String::from_utf8_lossy(src));
        }
    }

    #[test]
    fn every_token_and_skipped_line_break_is_charged() {
        let parse_ctx = ParseCtx::default();
        let mut lexer = Lexer::new(&parse_ctx, b"{\\b a}\r\n\0\\bin2 xy");
        let mut tokens = 0;
        while lexer.next_token().unwrap().is_some() {
            tokens += 1;
        }
        assert_eq!((tokens, parse_ctx.fuel()), (5, 8));
    }

    #[test]
    fn lexing_stops_at_the_contexts_max_work() {
        let limits = crate::limits::Limits { max_work: 2, ..crate::limits::Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let mut lexer = Lexer::new(&parse_ctx, b"{}{}");
        assert!(lexer.next_token().unwrap().is_some());
        assert!(lexer.next_token().unwrap().is_some());
        let err = lexer.next_token().err().unwrap();
        assert_matches!(
            err,
            ConvertError::ResourceLimit { limit: crate::limits::Limit::Work, max: 2 }
        );
    }
}
