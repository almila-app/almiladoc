//! Position-explicit RTF lexer. `\binN` consumes exactly N raw bytes, so
//! binary payloads can never corrupt group state; unbalanced groups are the
//! caller's to recover (deliberately, with a log).

use crate::ctx::ParseCtx;
use crate::error::ConvertError;

pub enum Token<'a> {
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

pub struct Lexer<'a> {
    parse_ctx: &'a ParseCtx<'a>,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(parse_ctx: &'a ParseCtx<'a>, bytes: &'a [u8]) -> Self {
        Lexer { parse_ctx, bytes, pos: 0 }
    }

    /// The next token, charging one unit of work per token and per skipped
    /// line break or NUL.
    pub fn next_token(&mut self) -> Result<Option<Token<'a>>, ConvertError> {
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
                let hi = (pair[0] as char).to_digit(16);
                let lo = (pair[1] as char).to_digit(16);
                return match (hi, lo) {
                    (Some(hi), Some(lo)) => {
                        self.pos += 2;
                        Some(Token::Hex((hi * 16 + lo) as u8))
                    }
                    // Truncated escape: recover by treating it as literal.
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
        let mut param: Option<i32> = None;
        let mut negative = false;
        if self.bytes.get(self.pos) == Some(&b'-') {
            negative = true;
            self.pos += 1;
        }
        let num_start = self.pos;
        while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        if self.pos > num_start {
            if let Ok(n) =
                std::str::from_utf8(&self.bytes[num_start..self.pos]).unwrap_or("0").parse::<i64>()
            {
                let n = if negative { -n } else { n };
                param = Some(n.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
            }
        } else if negative {
            self.pos -= 1;
        }
        // One space after a control word is part of the control.
        if self.bytes.get(self.pos) == Some(&b' ') {
            self.pos += 1;
        }
        if name == "bin" {
            let n = param.unwrap_or(0).max(0) as usize;
            let end = self.pos.saturating_add(n).min(self.bytes.len());
            let payload = &self.bytes[self.pos..end];
            self.pos = end;
            return Some(Token::Bin(payload));
        }
        Some(Token::Word { name, param })
    }
}

/// Extract the balanced content of destination groups named `name`
/// (`{\name ...}` or `{\*\name ...}`), bin-aware. Returns the byte ranges of
/// the group bodies including the destination word.
pub fn destination_groups<'a>(
    parse_ctx: &'a ParseCtx<'a>,
    bytes: &'a [u8],
    name: &str,
) -> Result<Vec<&'a [u8]>, ConvertError> {
    let mut out = Vec::new();
    let mut lexer = Lexer::new(parse_ctx, bytes);
    // Track group starts; when a group's first control word (skipping `\*`)
    // matches, remember its start depth and capture until it closes.
    let mut depth = 0usize;
    let mut expecting_word_at: Option<usize> = None;
    let mut capture: Option<(usize, usize)> = None; // (depth, start_pos)
    loop {
        let before = lexer.pos;
        let Some(token) = lexer.next_token()? else { break };
        match token {
            Token::Open => {
                depth += 1;
                expecting_word_at = Some(depth);
            }
            Token::Close => {
                if let Some((d, start)) = capture
                    && depth == d
                {
                    out.push(&bytes[start..before]);
                    capture = None;
                }
                depth = depth.saturating_sub(1);
                expecting_word_at = None;
            }
            Token::Symbol(b'*') if expecting_word_at == Some(depth) => {}
            Token::Word { name: w, .. } => {
                if expecting_word_at == Some(depth) && w == name && capture.is_none() {
                    capture = Some((depth, lexer.pos));
                }
                expecting_word_at = None;
            }
            _ => expecting_word_at = None,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn destination_extraction() {
        let src = br"{\rtf1{\*\listtable{\list\listid5}}{\fonttbl{\f0 Arial;}} body}";
        let parse_ctx = ParseCtx::default();
        let lists = destination_groups(&parse_ctx, src, "listtable").unwrap();
        assert_eq!(lists.len(), 1);
        assert!(lists[0].starts_with(br"{\list"));
        let fonts = destination_groups(&parse_ctx, src, "fonttbl").unwrap();
        assert_eq!(fonts.len(), 1);
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
        assert!(matches!(
            err,
            ConvertError::ResourceLimit { limit: crate::limits::Limit::Work, max: 2 }
        ));
    }
}
