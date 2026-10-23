//! SpreadsheetML number format codes (ISO/IEC 29500-1 §18.8.30/31).
//!
//! A code parses into up to four `;`-separated sections. Numeric sections
//! render the value here; date/time sections are only classified, because
//! the sheet reader deliberately keeps its ISO-like date output (`mm/dd`
//! versus `dd/mm` is ambiguous for a downstream reader, ISO is not). Any
//! construct outside the implemented grammar makes [`NumberFormat::parse`]
//! return `None` and the caller falls back to General: approximating a
//! format silently would be worse than not applying it.

use crate::shared::lossy;
use std::cmp::Ordering;

/// Implied format codes for built-in numFmtIds. Ids 5-8 are absent
/// deliberately: the standard leaves them to the file's own formatCode, so
/// an unresolved reference falls back to General rather than a guessed
/// currency format. Ids 27-36 and 50-81 are locale-specific (zh, ja, ko, th)
/// and unresolvable without a locale.
pub(super) fn builtin_code(id: u32) -> Option<&'static str> {
    Some(match id {
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        9 => "0%",
        10 => "0.00%",
        11 => "0.00E+00",
        12 => "# ?/?",
        13 => "# ??/??",
        14 => "mm-dd-yy",
        15 => "d-mmm-yy",
        16 => "d-mmm",
        17 => "mmm-yy",
        18 => "h:mm AM/PM",
        19 => "h:mm:ss AM/PM",
        20 => "h:mm",
        21 => "h:mm:ss",
        22 => "m/d/yy h:mm",
        37 => "#,##0 ;(#,##0)",
        38 => "#,##0 ;[Red](#,##0)",
        39 => "#,##0.00;(#,##0.00)",
        40 => "#,##0.00;[Red](#,##0.00)",
        45 => "mm:ss",
        46 => "[h]:mm:ss",
        47 => "mmss.0",
        48 => "##0.0E+0",
        49 => "@",
        _ => return None,
    })
}

/// What a resolved format asks the caller to do with a numeric value.
#[derive(Debug, PartialEq)]
pub(super) enum Rendered<'a> {
    /// Render this value the way an unformatted cell renders.
    General { value: f64, prefix: &'a str, suffix: &'a str },
    /// The section is a date/time format: render the serial as the parts it
    /// asks for.
    DateTime(DateParts),
    /// The formatted text, ready to emit.
    Text(String),
}

/// Which of a date/time format's parts it asks to see. A code naming only
/// months and days must not gain a time, and one naming only hours must not
/// gain a date.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct DateParts {
    pub(super) date: bool,
    pub(super) time: bool,
    /// `[h]`, `[m]` or `[s]`: a span rather than a point in time.
    pub(super) elapsed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Region {
    Int,
    Frac,
    Exp,
    Num,
    Den,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Digit {
        place: char,
        region: Region,
    },
    Decimal,
    Percent,
    Literal(String),
    /// Unquoted digits, valid only as a fixed fraction denominator.
    BareDigits(String),
    /// A `,` pending resolution into grouping, scaling, or a literal.
    Comma,
    /// `E+` / `E-`; `plus` keeps the sign on non-negative exponents.
    Exp {
        plus: bool,
    },
    /// Fraction bar between numerator and denominator placeholders.
    Slash,
    /// `@`, the text placeholder.
    At,
    /// `_x`: skip the width of one character (one space here).
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

#[derive(Debug, Clone, Copy)]
struct Cond {
    op: Op,
    operand: f64,
}

impl Cond {
    fn matches(&self, v: f64) -> bool {
        match self.op {
            Op::Lt => v < self.operand,
            Op::Le => v <= self.operand,
            Op::Gt => v > self.operand,
            Op::Ge => v >= self.operand,
            Op::Eq => v.partial_cmp(&self.operand) == Some(Ordering::Equal),
            Op::Ne => v.partial_cmp(&self.operand) != Some(Ordering::Equal),
        }
    }
}

#[derive(Debug, PartialEq, Default)]
struct NumSpec {
    toks: Vec<Tok>,
    grouping: bool,
    /// Commas that follow a digit placeholder rather than a fixed denominator: each divides the
    /// value by 1000.
    scale: u32,
    /// Each `%` multiplies the value by 100.
    percents: u32,
    int_places: usize,
    frac_places: usize,
    exp: bool,
    num_places: usize,
    den_places: usize,
    /// Fraction denominator written as literal digits.
    fixed_den: Option<u64>,
}

#[derive(Debug, PartialEq)]
enum Body {
    /// `General` with the literals that decorate it, which render around the
    /// value the way an unformatted cell would show it.
    General {
        prefix: String,
        suffix: String,
    },
    DateTime(DateParts),
    Number(NumSpec),
    Text(Vec<Tok>),
}

#[derive(Debug)]
struct Section {
    condition: Option<Cond>,
    body: Body,
    shows_non_ascii: bool,
}

impl Section {
    fn is_text(&self) -> bool {
        matches!(self.body, Body::Text(_))
    }
}

/// A parsed format code.
#[derive(Debug)]
pub(super) struct NumberFormat {
    sections: Vec<Section>,
}

/// The longest format code, in UTF-16 code units: [MS-XLS] Format and
/// [MS-XLSB] BrtFmt cap the code at 255, and xlsx takes the same cap so the
/// three containers agree.
const MAX_CODE_UNITS: usize = 255;

impl NumberFormat {
    pub(super) fn parse(code: &str) -> Option<Self> {
        // An empty code is not the `;;;` idiom for hiding a value, it is a
        // format that says nothing, so the caller falls back to General.
        if code.is_empty() || code.encode_utf16().nth(MAX_CODE_UNITS).is_some() {
            return None;
        }
        let parts = split_sections(code)?;
        if parts.len() > 4 {
            return None;
        }
        let sections: Vec<Section> = parts.into_iter().map(parse_section).collect::<Option<_>>()?;
        if sections.iter().filter(|s| s.condition.is_some()).count() > 2 {
            return None;
        }
        let (last, init) = sections.split_last()?;
        // A text section is only valid in last position; the fourth section
        // is the text position, so nothing else may sit there.
        let misplaced = init.iter().any(Section::is_text)
            || last.is_text() && last.condition.is_some()
            || sections.len() == 4
                && !last.is_text()
                && !matches!(&last.body, Body::Number(spec) if spec.toks.is_empty());
        (!misplaced).then_some(Self { sections })
    }

    /// The section text renders through, which may be an empty fourth
    /// section that is not a text section.
    fn text_part(&self) -> Option<&Section> {
        self.sections.last().filter(|s| self.sections.len() == 4 || s.is_text())
    }

    fn numeric_sections(&self) -> &[Section] {
        &self.sections[..self.sections.len() - usize::from(self.text_part().is_some())]
    }

    /// The section a finite value takes, the value to render (the magnitude
    /// for the positional negative section) and whether to emit a minus.
    fn section_for(&self, v: f64) -> Option<(&Section, f64, bool)> {
        if !v.is_finite() {
            return None;
        }
        let sections = self.numeric_sections();
        if sections.iter().any(|s| s.condition.is_some()) {
            let s = sections.iter().find(|s| s.condition.is_none_or(|c| c.matches(v)))?;
            return Some((s, v, v < 0.0));
        }
        let idx = match sections.len() {
            1 => 0,
            2 if v >= 0.0 => 0,
            2 => 1,
            _ if v > 0.0 => 0,
            _ if v < 0.0 => 1,
            _ => 2,
        };
        // The positional negative section renders the magnitude; its code
        // supplies the sign (parens, a literal minus).
        let negative_section = idx == 1;
        let value = if negative_section { v.abs() } else { v };
        Some((sections.get(idx)?, value, !negative_section && v < 0.0))
    }

    pub(super) fn format_number(&self, v: f64) -> Rendered<'_> {
        let general = Rendered::General { value: v, prefix: "", suffix: "" };
        let Some((section, value, minus)) = self.section_for(v) else {
            return general;
        };
        match &section.body {
            Body::General { prefix, suffix } => Rendered::General { value, prefix, suffix },
            Body::DateTime(parts) => Rendered::DateTime(*parts),
            Body::Number(spec) => {
                render_number(spec, value.abs(), minus).map_or(general, Rendered::Text)
            }
            Body::Text(_) => general,
        }
    }

    /// Whether `format_number` shows a literal outside ASCII for `v`, as the
    /// code spells it, before the rendered text is cleaned: never for a
    /// date/time section, whose literals the serial's rendering drops, nor
    /// for a value that falls back to General.
    pub(super) fn number_shows_non_ascii(&self, v: f64) -> bool {
        let Some((section, value, minus)) = self.section_for(v) else {
            return false;
        };
        section.shows_non_ascii
            && match &section.body {
                Body::General { .. } => true,
                Body::Number(spec) => render_number(spec, value.abs(), minus).is_some(),
                Body::DateTime(_) | Body::Text(_) => false,
            }
    }

    /// Whether the section `format_text` renders text with shows a literal
    /// outside ASCII.
    pub(super) fn text_shows_non_ascii(&self) -> bool {
        self.text_part().is_some_and(|s| s.shows_non_ascii)
    }

    pub(super) fn text_shows_value(&self) -> bool {
        self.text_part().is_none_or(Section::is_text)
    }

    /// Apply the text section to a text value; `None` means the format has
    /// no section for text, so the value stays as it is.
    pub(super) fn format_text(&self, text: &str) -> Option<String> {
        // An empty fourth section hides text.
        let Body::Text(toks) = &self.text_part()?.body else {
            return Some(String::new());
        };
        Some(
            toks.iter()
                .map(|tok| match tok {
                    Tok::Literal(s) => s.as_str(),
                    Tok::Skip => " ",
                    Tok::At => text,
                    _ => "",
                })
                .collect(),
        )
    }
}

/// A code that does not parse, split at its text section: the numeric
/// sections before it, which a number renders through, and whether the text
/// section adds to a text cell what its fallback to General drops.
pub(super) struct TextSection<'a> {
    pub(super) numeric: &'a str,
    pub(super) decorates: bool,
}

/// Split a code at its text section, at any length. The text section is the
/// fourth section, else a last one holding an unquoted `@`, and it decorates
/// with a quoted or escaped literal, a skip, a `[$…]` currency symbol, or any
/// character but `@`, a fill, a digit placeholder or `General`. A code with
/// no text section, or whose sections do not split, is numeric whole and
/// decorates nothing. One pass over the bytes decides it with nothing
/// copied, so a code past [`MAX_CODE_UNITS`] costs no parse.
pub(super) fn text_section(code: &str) -> TextSection<'_> {
    scan(code).0
}

/// [`text_section`] with the bytes its scan steps over, a byte stepped over twice counted twice;
/// a peek (the `General` lookahead, a bracket's `$` and the byte after it) is not counted.
fn scan(code: &str) -> (TextSection<'_>, usize) {
    let whole = TextSection { numeric: code, decorates: false };
    let split_at = |start: usize, decorates: bool| TextSection {
        numeric: code.get(..start).unwrap_or(code),
        decorates,
    };
    let (mut index, mut at, mut adds, mut start, mut read) = (0, false, false, 0, 0);
    let mut rest = code.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        (rest, read) = (tail, read + 1);
        match b {
            b';' if index == 3 => return (split_at(start, adds), read),
            b';' => (index, at, adds, start) = (index + 1, false, false, code.len() - tail.len()),
            b'"' | b'[' => {
                let close = if b == b'"' { b'"' } else { b']' };
                let mut run = tail.splitn(2, |&c| {
                    read += 1;
                    c == close
                });
                let (Some(inner), Some(after)) = (run.next(), run.next()) else {
                    return (whole, read);
                };
                adds |= match b {
                    b'"' => !inner.is_empty(),
                    _ => inner.first() == Some(&b'$') && inner.get(1).is_some_and(|&c| c != b'-'),
                };
                rest = after;
            }
            b'\\' | b'_' | b'*' => {
                let Some((_, after)) = tail.split_first() else {
                    return (whole, read);
                };
                adds |= b != b'*';
                (rest, read) = (after, read + 1);
            }
            b'@' => at = true,
            b'g' | b'G' => match tail.split_at_checked(6) {
                Some((word, after)) if word.eq_ignore_ascii_case(b"eneral") => {
                    (rest, read) = (after, read + word.len());
                }
                _ => adds = true,
            },
            // A UTF-8 continuation byte belongs to a character already read.
            b'0' | b'#' | b'?' | 0x80..=0xBF => {}
            _ => adds = true,
        }
    }
    (if index == 3 || at { split_at(start, adds) } else { whole }, read)
}

/// Split a code on `;` outside quotes, brackets, and escapes.
fn split_sections(code: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut chars = code.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            ';' => {
                parts.push(code.get(start..i)?);
                start = i + 1;
            }
            '"' | '[' => {
                let close = if c == '"' { '"' } else { ']' };
                chars.find(|&(_, c)| c == close)?;
            }
            '\\' | '_' | '*' => {
                chars.next()?;
            }
            _ => {}
        }
    }
    parts.push(code.get(start..)?);
    Some(parts)
}

const COLORS: &[&str] = &["black", "blue", "cyan", "green", "magenta", "red", "white", "yellow"];

/// The text literal tokens render as, used to carry the decoration around a
/// `General` body.
fn decoration(toks: &[Tok]) -> String {
    toks.iter()
        .map(|t| match t {
            Tok::Literal(s) => s.as_str(),
            _ => " ",
        })
        .collect()
}

/// Which parts a date/time section asks for. `m` is minutes when an hour run
/// precedes it or a seconds run follows it, and months otherwise.
fn date_parts(runs: &[char], elapsed: bool) -> DateParts {
    let is_time = |(i, &run): (usize, &char)| match run {
        'h' | 's' | 'a' => true,
        'm' => runs[..i].last() == Some(&'h') || runs.get(i + 1) == Some(&'s'),
        _ => false,
    };
    let any = |time: bool| runs.iter().enumerate().any(|run| is_time(run) == time);
    // An elapsed span is a duration whatever else the section names (`[m]`
    // is elapsed minutes, never months).
    DateParts { date: !elapsed && any(false), time: elapsed || any(true), elapsed }
}

fn push_literal(raw: &mut Vec<Tok>, c: char) {
    match raw.last_mut() {
        Some(Tok::Literal(s)) => s.push(c),
        _ => raw.push(Tok::Literal(c.to_string())),
    }
}

/// What one section's tokenizer read.
#[derive(Default)]
struct Tokens {
    raw: Vec<Tok>,
    condition: Option<Cond>,
    elapsed: bool,
    /// The date/time runs in order, one letter each, so `m` can be read as
    /// months or minutes from what sits beside it.
    runs: Vec<char>,
    /// Where `General` sits in the rendered decoration, when the section has it.
    general_at: Option<usize>,
}

fn parse_section(s: &str) -> Option<Section> {
    let tokens = tokenize(s)?;
    let condition = tokens.condition;
    let shows_non_ascii =
        tokens.raw.iter().any(|t| matches!(t, Tok::Literal(text) if !text.is_ascii()));
    Some(Section { condition, body: classify_body(tokens)?, shows_non_ascii })
}

/// The token one placeholder or operator character makes.
fn single_token(c: char) -> Tok {
    match c {
        '.' => Tok::Decimal,
        ',' => Tok::Comma,
        '%' => Tok::Percent,
        '@' => Tok::At,
        '/' => Tok::Slash,
        place => Tok::Digit { place, region: Region::Int },
    }
}

/// Matched over chars: the tokens are ASCII, but a multibyte char can follow
/// them, and byte slicing would split it.
fn starts_with_ignore_case(chars: &[char], word: &str) -> bool {
    chars.len() >= word.len()
        && chars.iter().zip(word.chars()).all(|(c, w)| c.eq_ignore_ascii_case(&w))
}

fn tokenize(s: &str) -> Option<Tokens> {
    let chars: Vec<char> = s.chars().collect();
    let mut t = Tokens::default();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '[' => {
                let rest = &chars[i + 1..];
                let len = rest.iter().position(|&c| c == ']')?;
                t.bracket(&rest[..len].iter().collect::<String>())?;
                i += len + 2;
            }
            '"' => {
                let rest = &chars[i + 1..];
                let len = rest.iter().position(|&c| c == '"')?;
                for &c in &rest[..len] {
                    push_literal(&mut t.raw, c);
                }
                i += len + 2;
            }
            '\\' => {
                push_literal(&mut t.raw, *chars.get(i + 1)?);
                i += 2;
            }
            '_' => {
                chars.get(i + 1)?;
                t.raw.push(Tok::Skip);
                i += 2;
            }
            '*' => {
                // Repeat-to-fill: no column width exists here, emit nothing.
                chars.get(i + 1)?;
                i += 2;
            }
            '0' | '#' | '?' | '.' | ',' | '%' | '@' | '/' => {
                t.raw.push(single_token(c));
                i += 1;
            }
            '1'..='9' => {
                let digits: String = chars[i..].iter().take_while(|c| c.is_ascii_digit()).collect();
                i += digits.len();
                t.raw.push(Tok::BareDigits(digits));
            }
            '$' | '-' | '+' | '(' | ')' | ':' | ' ' => {
                push_literal(&mut t.raw, c);
                i += 1;
            }
            _ if c.is_ascii_alphabetic() => i = t.word(&chars, i)?,
            _ => return None,
        }
    }
    Some(t)
}

/// The body a section's tokens make: `General` with its decoration, a date or time, text
/// around `@`, or a number; `None` for a mix the grammar has no reading of.
fn classify_body(t: Tokens) -> Option<Body> {
    let Tokens { raw, elapsed, runs, general_at, .. } = t;
    Some(if let Some(general_at) = general_at {
        if raw.iter().any(|t| !matches!(t, Tok::Literal(_) | Tok::Skip)) {
            return None;
        }
        let text = decoration(&raw);
        let mut chars = text.chars();
        let prefix = chars.by_ref().take(general_at).collect();
        Body::General { prefix, suffix: chars.collect() }
    } else if !runs.is_empty() {
        if raw.iter().any(|t| matches!(t, Tok::At | Tok::Exp { .. } | Tok::BareDigits(_))) {
            return None;
        }
        Body::DateTime(date_parts(&runs, elapsed))
    } else if raw.iter().any(|t| matches!(t, Tok::At)) {
        if raw.iter().any(|t| {
            matches!(
                t,
                Tok::Digit { .. }
                    | Tok::Decimal
                    | Tok::Exp { .. }
                    | Tok::Slash
                    | Tok::BareDigits(_)
            )
        }) {
            return None;
        }
        let toks = raw
            .into_iter()
            .map(|t| match t {
                Tok::Percent => Tok::Literal("%".to_string()),
                Tok::Comma => Tok::Literal(",".to_string()),
                t => t,
            })
            .collect();
        Body::Text(toks)
    } else {
        Body::Number(resolve_number(raw)?)
    })
}

impl Tokens {
    /// Reads the letter-spelled token at `chars[i]`: an exponent, a date or
    /// time run, AM/PM or `General`. Returns the index after it.
    fn word(&mut self, chars: &[char], i: usize) -> Option<usize> {
        let c = chars[i];
        Some(match c {
            'E' | 'e' if matches!(chars.get(i + 1), Some('+' | '-')) => {
                self.raw.push(Tok::Exp { plus: chars[i + 1] == '+' });
                i + 2
            }
            'y' | 'Y' | 'd' | 'D' | 'h' | 'H' | 's' | 'S' | 'm' | 'M' => {
                self.runs.push(c.to_ascii_lowercase());
                i + chars[i..].iter().take_while(|x| x.eq_ignore_ascii_case(&c)).count()
            }
            'g' | 'G' => {
                if !starts_with_ignore_case(&chars[i..], "general") {
                    return None;
                }
                // Adjacent literals merge, so the split has to be recorded
                // against the rendered text rather than a token index.
                self.general_at = Some(decoration(&self.raw).chars().count());
                i + 7
            }
            'a' | 'A' => {
                let word = ["AM/PM", "A/P"]
                    .into_iter()
                    .find(|word| starts_with_ignore_case(&chars[i..], word))?;
                self.runs.push('a');
                i + word.len()
            }
            _ => return None,
        })
    }

    fn bracket(&mut self, inner: &str) -> Option<()> {
        match inner.chars().next()? {
            '<' | '>' | '=' => {
                if self.condition.is_some() {
                    return None;
                }
                let (op, rest) = [
                    (">=", Op::Ge),
                    ("<=", Op::Le),
                    ("<>", Op::Ne),
                    (">", Op::Gt),
                    ("<", Op::Lt),
                    ("=", Op::Eq),
                ]
                .into_iter()
                .find_map(|(prefix, op)| inner.strip_prefix(prefix).map(|rest| (op, rest)))?;
                self.condition = Some(Cond { op, operand: rest.trim().parse().ok()? });
            }
            '$' => {
                // `[$sym-lcid]`: the currency string emits literally, the
                // locale id affects nothing rendered here.
                let sym = inner.get(1..).and_then(|rest| rest.split('-').next()).unwrap_or("");
                if !sym.is_empty() {
                    self.raw.push(Tok::Literal(sym.to_string()));
                }
            }
            c @ ('h' | 'H' | 'm' | 'M' | 's' | 'S')
                if inner.chars().all(|x| x.eq_ignore_ascii_case(&c)) =>
            {
                self.elapsed = true;
                self.runs.push(c.to_ascii_lowercase());
            }
            _ => {
                let lower = inner.to_ascii_lowercase();
                let is_color = COLORS.contains(&lower.as_str())
                    || lower
                        .strip_prefix("color")
                        .and_then(|n| n.trim().parse::<u32>().ok())
                        .is_some_and(|n| (1..=56).contains(&n));
                if !is_color {
                    return None;
                }
            }
        }
        Some(())
    }
}

/// Second pass over a numeric section: assign digit regions, resolve commas
/// into grouping or scaling, and recognize the fraction form.
fn resolve_number(raw: Vec<Tok>) -> Option<NumSpec> {
    let (toks, mut spec) = assign_regions(raw)?;
    spec.toks = classify_commas(toks, &mut spec);
    count_places(&mut spec);
    let valid = (!spec.exp
        || spec.toks.iter().any(|t| matches!(t, Tok::Digit { region: Region::Exp, .. })))
        && (spec.num_places == 0 || spec.den_places > 0 || spec.fixed_den.is_some());
    valid.then_some(spec)
}

/// Places each digit placeholder in its region (integer, fraction, exponent, numerator,
/// denominator) and reads the decimal point, exponent, fraction bar and percent signs.
fn assign_regions(raw: Vec<Tok>) -> Option<(Vec<Tok>, NumSpec)> {
    let mut toks: Vec<Tok> = Vec::new();
    let mut region = Region::Int;
    let mut spec = NumSpec::default();
    let mut iter = raw.into_iter().peekable();
    while let Some(tok) = iter.next() {
        match tok {
            Tok::Digit { place, .. } => {
                if spec.fixed_den.is_some() && region == Region::Den {
                    return None;
                }
                toks.push(Tok::Digit { place, region });
            }
            Tok::Decimal => {
                if region != Region::Int {
                    return None;
                }
                region = Region::Frac;
                toks.push(Tok::Decimal);
            }
            Tok::Exp { plus } => {
                if spec.exp || matches!(region, Region::Num | Region::Den) {
                    return None;
                }
                spec.exp = true;
                region = Region::Exp;
                toks.push(Tok::Exp { plus });
            }
            Tok::Slash => {
                // A fraction bar needs a numerator run directly before it;
                // otherwise the slash is a literal.
                let run = toks
                    .iter()
                    .rev()
                    .take_while(|t| matches!(t, Tok::Digit { region: Region::Int, .. }))
                    .count();
                if run == 0 || region != Region::Int || spec.fixed_den.is_some() {
                    push_literal(&mut toks, '/');
                    continue;
                }
                let at = toks.len() - run;
                for t in &mut toks[at..] {
                    if let Tok::Digit { region, .. } = t {
                        *region = Region::Num;
                    }
                }
                toks.push(Tok::Slash);
                region = Region::Den;
                if let Some(Tok::BareDigits(d)) = iter.next_if(|t| matches!(t, Tok::BareDigits(_)))
                {
                    spec.fixed_den = Some(d.parse().ok()?);
                    toks.push(Tok::BareDigits(d));
                }
            }
            Tok::Percent => {
                spec.percents += 1;
                toks.push(Tok::Percent);
            }
            // Unquoted digits anywhere but a fixed denominator are outside
            // the grammar.
            Tok::BareDigits(_) => return None,
            other => toks.push(other),
        }
    }
    Some((toks, spec))
}

fn classify_commas(toks: Vec<Tok>, spec: &mut NumSpec) -> Vec<Tok> {
    let is_digit = |t: &Tok| matches!(t, Tok::Digit { .. });
    let first_digit = toks.iter().position(is_digit);
    let last_digit = toks.iter().rposition(is_digit);
    let fixed_den = toks.iter().position(|t| matches!(t, Tok::BareDigits(_)));
    let mut out: Vec<Tok> = Vec::new();
    for (i, tok) in toks.into_iter().enumerate() {
        if tok != Tok::Comma {
            out.push(tok);
            continue;
        }
        match (first_digit, last_digit) {
            _ if fixed_den.is_some_and(|d| i > d) => {}
            (Some(_), Some(l)) if i > l => spec.scale += 1,
            (Some(f), Some(l)) if i > f && i < l => spec.grouping = true,
            _ => push_literal(&mut out, ','),
        }
    }
    out
}

fn count_places(spec: &mut NumSpec) {
    for t in &spec.toks {
        match t {
            Tok::Digit { region: Region::Int, .. } => spec.int_places += 1,
            Tok::Digit { region: Region::Frac, .. } => spec.frac_places += 1,
            Tok::Digit { region: Region::Num, .. } => spec.num_places += 1,
            Tok::Digit { region: Region::Den, .. } => spec.den_places += 1,
            _ => {}
        }
    }
}

fn render_number(spec: &NumSpec, v_abs: f64, minus: bool) -> Option<String> {
    let mut v = v_abs;
    for _ in 0..spec.percents {
        v *= 100.0;
    }
    for _ in 0..spec.scale {
        v /= 1000.0;
    }
    if !v.is_finite() {
        return None;
    }
    let body = if spec.exp {
        render_scientific(spec, v)?
    } else if spec.num_places > 0 {
        render_fraction(spec, v)?
    } else {
        let (int_digits, frac_digits) = split_digits(v, spec.frac_places, 0)?;
        emit(spec, &int_digits, &frac_digits, "", false)
    };
    Some(if minus { format!("-{body}") } else { body })
}

/// The digits of `v / 10^exp10`, rounded: the integer part (empty when
/// zero, so `#` can drop it) and exactly `dp` fractional digits. Rounding
/// happens on the 15-significant-digit decimal form, half away from zero,
/// the way a spreadsheet displays - binary arithmetic would round 5.255 at
/// two decimals to 5.25 - and the power of ten only moves the decimal
/// point.
fn split_digits(v: f64, dp: usize, exp10: i64) -> Option<(String, String)> {
    if !v.is_finite() || v < 0.0 || dp > 512 {
        return None;
    }
    if v == 0.0 {
        return Some((String::new(), "0".repeat(dp)));
    }
    // `{:.14e}` is the value at 15 significant decimal digits: an integer D
    // of up to 15 digits and an exponent, v = D * 10^(e-14).
    let repr = format!("{v:.14e}");
    let (mantissa, e) = repr.split_once('e')?;
    let e: i64 = e.parse().ok()?;
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    // Digits of round(v * 10^(dp - exp10)), by shifting or by decimal rounding.
    let shift = e - 14 + i64::try_from(dp).ok()? - exp10;
    let scaled = if shift >= 0 {
        format!("{digits}{}", "0".repeat(usize::try_from(shift).ok()?))
    } else {
        let drop = usize::try_from(-shift).ok()?;
        if drop > digits.len() {
            String::new()
        } else {
            let (kept, rest) = digits.split_at(digits.len() - drop);
            let mut kept: Vec<u8> = kept.bytes().collect();
            if rest.as_bytes().first().is_some_and(|&b| b >= b'5') {
                if let Some(i) = kept.iter().rposition(|&b| b != b'9') {
                    kept[i] += 1;
                    kept[i + 1..].fill(b'0');
                } else {
                    kept.fill(b'0');
                    kept.insert(0, b'1');
                }
            }
            String::from_utf8(kept).ok()?
        }
    };
    let s = format!("{:0>dp$}", scaled.trim_start_matches('0'));
    let (int, frac) = s.split_at(s.len() - dp);
    Some((int.to_string(), frac.to_string()))
}

fn places(toks: &[Tok], r: Region) -> Vec<char> {
    toks.iter()
        .filter_map(|t| match t {
            Tok::Digit { place, region } if *region == r => Some(*place),
            _ => None,
        })
        .collect()
}

/// Distribute a digit string over placeholders: the first takes any excess
/// digits, and placeholders past the available digits pad per their kind
/// (`0` a zero, `?` a space, `#` nothing).
fn assign(digits: &str, places: &[char]) -> Vec<String> {
    let k = places.len();
    if k == 0 {
        return Vec::new();
    }
    let n = digits.chars().count();
    if n > k {
        let mut rest = digits.chars();
        let first = rest.by_ref().take(n - k + 1).collect();
        std::iter::once(first).chain(rest.map(String::from)).collect()
    } else {
        places[..k - n]
            .iter()
            .map(|&p| match p {
                '0' => "0".to_string(),
                '?' => " ".to_string(),
                _ => String::new(),
            })
            .chain(digits.chars().map(String::from))
            .collect()
    }
}

/// Walk the token list emitting digits into placeholders.
fn emit(
    spec: &NumSpec,
    int_digits: &str,
    frac_digits: &str,
    exp_digits: &str,
    exp_negative: bool,
) -> String {
    let int_assigned = assign(int_digits, &places(&spec.toks, Region::Int));
    let exp_assigned = assign(exp_digits, &places(&spec.toks, Region::Exp));
    let is_digit = |c: &char| c.is_ascii_digit();
    let total_int: usize = int_assigned.iter().map(|s| s.chars().filter(is_digit).count()).sum();
    // Fractional digits kept: everything up to the rightmost `0` placeholder
    // always shows; beyond that, trailing zeros drop from `#` and pad `?`.
    let frac_places = places(&spec.toks, Region::Frac);
    let min_frac = frac_places.iter().rposition(|&p| p == '0').map_or(0, |i| i + 1);
    let keep_frac = frac_digits.trim_end_matches('0').len().max(min_frac);

    // The point hides only when its placeholders all produced nothing; a
    // point with no placeholder at all (`0.`) is written as it stands.
    let show_decimal = keep_frac > 0 || frac_places.contains(&'?') || frac_places.is_empty();

    let mut out = String::new();
    let (mut int_i, mut frac_i, mut exp_i) = (0usize, 0usize, 0usize);
    let mut int_remaining = total_int;
    for tok in &spec.toks {
        match tok {
            Tok::Digit { region: Region::Int, .. } => {
                for c in int_assigned[int_i].chars() {
                    out.push(c);
                    if c.is_ascii_digit() {
                        int_remaining -= 1;
                        if spec.grouping && int_remaining > 0 && int_remaining.is_multiple_of(3) {
                            out.push(',');
                        }
                    }
                }
                int_i += 1;
            }
            Tok::Digit { place, region: Region::Frac } => {
                if frac_i < keep_frac {
                    out.push(frac_digits.as_bytes()[frac_i] as char);
                } else if *place == '?' {
                    out.push(' ');
                }
                frac_i += 1;
            }
            Tok::Digit { region: Region::Exp, .. } => {
                out.push_str(&exp_assigned[exp_i]);
                exp_i += 1;
            }
            Tok::Decimal => {
                if show_decimal {
                    out.push('.');
                }
            }
            Tok::Percent => out.push('%'),
            Tok::Literal(s) => out.push_str(s),
            Tok::Exp { plus } => {
                out.push('E');
                if exp_negative {
                    out.push('-');
                } else if *plus {
                    out.push('+');
                }
            }
            Tok::Skip => out.push(' '),
            Tok::Digit { .. } | Tok::Slash | Tok::At | Tok::Comma | Tok::BareDigits(_) => {}
        }
    }
    out
}

fn render_scientific(spec: &NumSpec, v: f64) -> Option<String> {
    let n_int = i64::try_from(spec.int_places.max(1)).ok()?;
    let (int_digits, frac_digits, exp10) = if v == 0.0 {
        let (i, f) = split_digits(0.0, spec.frac_places, 0)?;
        (i, f, 0i64)
    } else {
        // The decimal exponent comes from the 15-digit form split_digits
        // rounds; the float log10 misplaces boundaries like 1000.
        let repr = format!("{v:.14e}");
        let mut e: i64 = repr.split('e').nth(1)?.parse().ok()?;
        // The exponent stays a multiple of the integer placeholder count
        // (engineering notation for `##0.0E+0`).
        e = e.div_euclid(n_int) * n_int;
        let (mut i, mut f) = split_digits(v, spec.frac_places, e)?;
        // Rounding at the display precision can carry into a new digit
        // (9.99 -> 10.0): renormalize.
        if i.len() > usize::try_from(n_int).ok()? {
            e += n_int;
            (i, f) = split_digits(v, spec.frac_places, e)?;
        }
        (i, f, e)
    };
    let width = places(&spec.toks, Region::Exp).len();
    let exp_digits = format!("{:0>width$}", exp10.unsigned_abs());
    Some(emit(spec, &int_digits, &frac_digits, &exp_digits, exp10 < 0))
}

fn render_fraction(spec: &NumSpec, v: f64) -> Option<String> {
    if v >= 1e15 {
        return None;
    }
    let has_int = spec.int_places > 0;
    let (mut whole, target) = if has_int { (v.trunc(), v.fract()) } else { (0.0, v) };
    let (mut num, den) = best_fraction(target, spec.fixed_den, spec.den_places)?;
    if has_int && num == den && den > 0 {
        whole += 1.0;
        num = 0;
    }
    let int_digits = if whole == 0.0 {
        // A zero integer part still shows when the whole value is zero.
        if num == 0 { "0".to_string() } else { String::new() }
    } else {
        split_digits(whole, 0, 0)?.0
    };
    // A zero numerator blanks the fraction: "5", not "5 0/1".
    let hide = has_int && num == 0;
    let mut int_assigned = assign(&int_digits, &places(&spec.toks, Region::Int)).into_iter();
    let mut num_assigned = assign(&num.to_string(), &places(&spec.toks, Region::Num)).into_iter();
    let mut den_assigned = assign(&den.to_string(), &places(&spec.toks, Region::Den)).into_iter();
    let mut out = String::new();
    for tok in &spec.toks {
        match tok {
            Tok::Digit { region: Region::Int, .. } => {
                out.push_str(&int_assigned.next().unwrap_or_default());
            }
            Tok::Digit { region: Region::Num, .. } => {
                if !hide {
                    out.push_str(&num_assigned.next().unwrap_or_default());
                }
            }
            Tok::Digit { region: Region::Den, .. } => {
                if !hide {
                    out.push_str(&den_assigned.next().unwrap_or_default());
                }
            }
            Tok::Slash => {
                if !hide {
                    out.push('/');
                }
            }
            Tok::BareDigits(s) => {
                if !hide {
                    out.push_str(s);
                }
            }
            Tok::Literal(s) => out.push_str(s),
            Tok::Percent => out.push('%'),
            Tok::Skip => out.push(' '),
            _ => {}
        }
    }
    Some(out.trim_end().to_string())
}

fn best_fraction(x: f64, fixed: Option<u64>, den_places: usize) -> Option<(u64, u64)> {
    if x < 0.0 || !x.is_finite() {
        return None;
    }
    if let Some(d) = fixed {
        let n = (x * lossy::to_f64(d)).round();
        return (n < 1e18).then_some((lossy::round_u64(n), d));
    }
    // Past 19 places the power of ten overflows; every u64 denominator is
    // within such a bound, so it saturates rather than dropping the format.
    let max_den = u32::try_from(den_places)
        .ok()
        .and_then(|p| 10u64.checked_pow(p))
        .map_or(u64::MAX, |p| p.saturating_sub(1))
        .max(1);
    Some(closest_rational(x, max_den))
}

/// The closest rational to `x` with a denominator no larger than `max_den`.
///
/// Walks the continued fraction of the exact ratio the float stores, in a
/// number of steps proportional to the digits of `max_den`. Repeatedly
/// inverting the float remainder instead would compound rounding until the
/// walk picks a non-closest fraction under large bounds. When the next term
/// would overshoot the bound, the only other candidate is the semiconvergent
/// with the largest term that fits, and the exact tail decides between them.
fn closest_rational(x: f64, max_den: u64) -> (u64, u64) {
    let Some((mut p, mut q)) = dyadic(x) else {
        return (0, 1);
    };
    let (mut pn, mut pd) = (0u64, 1u64);
    let (mut n, mut d) = (1u64, 0u64);
    while q > 0 {
        let a = p / q;
        let rem = p % q;
        let next = u64::try_from(a).ok().and_then(|a| {
            Some((a.checked_mul(n)?.checked_add(pn)?, a.checked_mul(d)?.checked_add(pd)?))
        });
        let (nn, nd) = match next {
            Some((nn, nd)) if nd <= max_den => (nn, nd),
            // The first term alone is past every u64 numerator.
            _ if d == 0 => return (0, 1),
            _ => {
                let k = (max_den - pd) / d;
                let semi =
                    k.checked_mul(n).and_then(|v| v.checked_add(pn)).map(|sn| (sn, k * d + pd));
                // With the exact tail r = p/q, the semiconvergent is closer
                // iff (r - k)d < kd + pd, i.e. rem*d < q*((2k - a)d + pd):
                // certain for 2k > a, impossible for 2k < a (pd < d).
                let better = match (u128::from(k) * 2).cmp(&a) {
                    Ordering::Greater => true,
                    Ordering::Less => false,
                    Ordering::Equal => mul_cmp(rem, d, q, pd) == Ordering::Less,
                };
                return match (semi, better) {
                    (Some(semi), true) => semi,
                    _ => (n, d),
                };
            }
        };
        (pn, pd, n, d) = (n, d, nn, nd);
        (p, q) = (q, rem);
    }
    if d == 0 { (0, 1) } else { (n, d) }
}

/// A finite non-negative float as the ratio it stores exactly. `None` when
/// no exact ratio fits (under 2^-74 every u64-bounded fraction rounds to
/// zero; past u128 no bounded denominator distinguishes it from an
/// integer), which the walk treats as zero.
fn dyadic(x: f64) -> Option<(u128, u128)> {
    let bits = x.to_bits();
    let exp = (bits >> 52) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    let (m, e) = if exp == 0 { (frac, -1074) } else { (frac | (1 << 52), exp - 1075) };
    if m == 0 {
        return Some((0, 1));
    }
    if e >= 0 {
        return (e <= 75).then(|| (u128::from(m) << e, 1));
    }
    let shift = m.trailing_zeros().min(e.unsigned_abs());
    let k = e.unsigned_abs() - shift;
    (k <= 127).then(|| (u128::from(m >> shift), 1u128 << k))
}

/// `a * b` against `c * e`, exact in 192 bits.
fn mul_cmp(a: u128, b: u64, c: u128, e: u64) -> Ordering {
    let wide = |x: u128, y: u64| {
        let lo = (x & u128::from(u64::MAX)) * u128::from(y);
        ((x >> 64) * u128::from(y) + (lo >> 64), lo & u128::from(u64::MAX))
    };
    wide(a, b).cmp(&wide(c, e))
}

#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::unreadable_literal,
    reason = "the tests sweep known value ranges and write serials as spreadsheets store them"
)]
mod tests {
    use super::*;

    #[test]
    fn a_tokenizer_reads_an_elapsed_span_and_its_runs() {
        let t = tokenize("[h]:mm").unwrap();
        assert!(t.elapsed);
        assert_eq!(t.runs, ['h', 'm']);
        assert_eq!(t.raw, [Tok::Literal(":".into())]);
        assert_eq!(
            classify_body(t).unwrap(),
            Body::DateTime(DateParts { date: false, time: true, elapsed: true })
        );
        let general = tokenize("\"x\"General\"y\"").unwrap();
        assert_eq!(general.general_at, Some(1));
        assert_eq!(
            classify_body(general).unwrap(),
            Body::General { prefix: "x".into(), suffix: "y".into() }
        );
        assert!(tokenize("Gen").is_none());
    }

    #[test]
    fn regions_place_each_digit_and_a_fixed_denominator() {
        let (toks, spec) = assign_regions(tokenize("# ?/8").unwrap().raw).unwrap();
        assert_eq!(spec.fixed_den, Some(8));
        assert!(toks.contains(&Tok::BareDigits("8".into())));
        assert!(toks.contains(&Tok::Digit { place: '?', region: Region::Num }));
        assert!(toks.contains(&Tok::Digit { place: '#', region: Region::Int }));
        let (toks, spec) = assign_regions(tokenize("0.00E+0").unwrap().raw).unwrap();
        assert!(spec.exp);
        assert_eq!(
            toks.iter().filter(|t| matches!(t, Tok::Digit { region: Region::Frac, .. })).count(),
            2
        );
        assert!(toks.contains(&Tok::Digit { place: '0', region: Region::Exp }));
        assert!(assign_regions(tokenize("0.0.0").unwrap().raw).is_none());
    }

    #[test]
    fn commas_group_between_digits_scale_after_them_and_stay_literal_before() {
        let spec = resolve_number(tokenize(",#,##0.0,,").unwrap().raw).unwrap();
        assert!(spec.grouping);
        assert_eq!(spec.scale, 2);
        assert_eq!(spec.toks.first(), Some(&Tok::Literal(",".into())));
        assert_eq!((spec.int_places, spec.frac_places), (4, 1));
        assert!(resolve_number(tokenize("0E+").unwrap().raw).is_none());
        assert!(resolve_number(tokenize("#/").unwrap().raw).is_none());
    }

    #[test]
    fn a_comma_after_a_fixed_denominator_neither_scales_nor_shows() {
        let spec = resolve_number(tokenize("# ?/8\\8,,").unwrap().raw).unwrap();
        assert_eq!(spec.scale, 0);
        assert!(spec.toks.contains(&Tok::Literal("8".into())));
        assert!(!spec.toks.iter().any(|t| matches!(t, Tok::Literal(s) if s.contains(','))));
        let spec = resolve_number(tokenize("# ?/?,").unwrap().raw).unwrap();
        assert_eq!(spec.scale, 1);
    }

    #[test]
    fn digits_are_distributed_by_character_not_byte() {
        let places = ['0', '0'];
        assert_eq!(assign("1\u{e9}", &places), ["1", "\u{e9}"]);
        assert_eq!(assign("1\u{e9}3", &places), ["1\u{e9}", "3"]);
    }

    fn fmt(code: &str, v: f64) -> String {
        match NumberFormat::parse(code).expect("code must parse").format_number(v) {
            Rendered::Text(s) => s,
            other => panic!("expected text for {code:?} on {v}, got {other:?}"),
        }
    }

    #[test]
    fn a_non_ascii_literal_shows_only_where_the_text_section_renders_it() {
        assert!(!NumberFormat::parse("0\" \u{e9}\"").unwrap().text_shows_non_ascii());
        assert!(!NumberFormat::parse("\"x\"@").unwrap().text_shows_non_ascii());
        assert!(NumberFormat::parse("\"\u{e9}\"@").unwrap().text_shows_non_ascii());
    }

    #[test]
    fn a_non_ascii_literal_shows_only_where_the_number_section_renders_it() {
        let shows =
            |code: &str, v: f64| NumberFormat::parse(code).unwrap().number_shows_non_ascii(v);
        assert!(shows("0\"\u{a0}\";0\"\u{ad}\"", 2.0));
        assert!(shows("0\"\u{a0}\";0\"\u{ad}\"", -2.0));
        assert!(!shows("0\"\u{a0}\";0", -2.0));
        assert!(shows("\"\u{e9}\"General", 2.0));
        assert!(!shows("yyyy\" \u{e9}\"", 2.0));
        assert!(!shows("0%\" \u{e9}\"", f64::MAX));
        assert!(!shows("\"\u{e9}\"General", f64::INFINITY));
        assert!(!shows("0;0;0;\"\u{e9}\"@", 0.0));
        assert!(shows("0\\\u{e9}", 2.0));
        assert!(!shows("0_\u{e9}", 2.0));
        assert!(!shows("0*\u{e9}", 2.0));
        assert!(shows("[$\u{e9}-407]0", 2.0));
        assert!(shows("[<0]0\"\u{e9}\";0", -2.0));
        assert!(!shows("[<0]0\"\u{e9}\";0", 2.0));
        assert!(!shows("[<0]0;[>5]0\"\u{e9}\"", 2.0));
        assert!(shows("0;0;0\"\u{e9}\"", 0.0));
        assert!(!shows("0;0;0\"\u{e9}\"", 2.0));
    }

    #[test]
    fn a_text_value_shows_unless_its_section_leaves_it_out() {
        let shows = |code: &str| NumberFormat::parse(code).unwrap().text_shows_value();
        assert!(shows("0"));
        assert!(shows("0;0;0;\"x\"@"));
        assert!(shows("0;@"));
        assert!(shows("0;0;@"));
        assert!(!shows(";;;"));
        assert!(!shows("0;0;0;"));
    }

    #[test]
    fn a_code_past_the_binary_cap_does_not_parse() {
        assert!(NumberFormat::parse(&"0".repeat(MAX_CODE_UNITS)).is_some());
        assert!(NumberFormat::parse(&"0".repeat(MAX_CODE_UNITS + 1)).is_none());
        let quoted = |n| format!("0\"{}\"", "\u{1F600}".repeat(n));
        assert!(NumberFormat::parse(&quoted((MAX_CODE_UNITS - 3) / 2)).is_some());
        assert!(NumberFormat::parse(&quoted((MAX_CODE_UNITS - 3) / 2 + 1)).is_none());
    }

    const DECORATING: [&str; 20] = [
        "0;0;0;\"a\"@",
        "0;0;0;[Red]\"a\"@",
        "0;0;0;@_)",
        "0;0;0;[$USD-409]@",
        "0;0;0;[$€-407]@",
        "\\>@",
        "0;\"n: \"@",
        "0;0;0;x;@",
        "0;0;0;\"a\"0",
        "0;0;0;\\a",
        "0;0;0;\"a\"General",
        "0;0;0;Gen@",
        "€@",
        "0;€@",
        "x@",
        "0;x@",
        "0;1@",
        "0;.@",
        "0;!@",
        "0;yy@",
    ];

    const PLAIN: [&str; 18] = [
        "0;0;0;@",
        "0;0;0;",
        "0;0;0;[Red]@",
        "0;0;0;*-@",
        "0;0;0;*€@",
        "0;0;0;0",
        "\"@\"0",
        "\"a\"0",
        "0;\"a\"",
        "0;\"open",
        "0;0;0;0@",
        "0;0;0;#@",
        "0;0;0;[h]@",
        "0;0;0;General",
        "0;0;0;general@",
        "0;0;0;[$-409]@",
        "0;0;0;\"\"@",
        "0;0;0;@_",
    ];

    fn classified() -> impl Iterator<Item = (&'static str, bool)> {
        DECORATING.map(|code| (code, true)).into_iter().chain(PLAIN.map(|code| (code, false)))
    }

    #[test]
    fn a_text_section_decorates_when_it_adds_to_the_text() {
        for (code, decorates) in classified() {
            assert_eq!(text_section(code).decorates, decorates, "{code}");
        }
    }

    #[test]
    fn a_text_section_past_the_cap_decorates_by_the_same_rule() {
        for (code, decorates) in classified() {
            let padded = format!("{}{code}", "\"\"".repeat(MAX_CODE_UNITS));
            assert!(NumberFormat::parse(&padded).is_none(), "{code}");
            assert_eq!(text_section(&padded).decorates, text_section(code).decorates, "{code}");
            assert_eq!(text_section(&padded).decorates, decorates, "{code}");
        }
    }

    #[test]
    fn a_text_section_past_the_cap_steps_once_over_the_bytes_it_needs() {
        let section = "@".repeat(2 * MAX_CODE_UNITS);
        let cases = [
            (format!("0;0;0;{section}"), "", false),
            (format!("0;0;0;{section}\"a\""), "", true),
            (format!("0;0;0;[{section}"), "", false),
            (format!("0;0;0;{section};"), section.as_str(), false),
            (format!("0;0;0;\\a{section}"), "", true),
            (format!("0;0;0;General{section}"), "", false),
            (format!("0;0;0;Genera{section}"), "", true),
            (format!("0;0;0;[$€-409]{section}"), "", true),
        ];
        for (i, (read, unread, decorates)) in cases.iter().enumerate() {
            let code = format!("{read}{unread}");
            let (text, stepped) = scan(&code);
            assert_eq!(stepped, read.len(), "case {i}");
            assert_eq!(text.decorates, *decorates, "case {i}");
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the table of codes, values and the text each renders"
    )]
    fn a_code_renders_a_value_as_text() {
        let long_code = format!("?/{}", "?".repeat(20));
        let long_want = format!("1/{}2", " ".repeat(19));
        let cases = [
            ("0.0%", 0.075, "7.5%"),
            ("0%", 0.155, "16%"),
            ("0.00%", -0.5, "-50.00%"),
            ("#,##0", 9876543.0, "9,876,543"),
            ("#,##0.00", 1234.5, "1,234.50"),
            ("#,##0", 0.0, "0"),
            ("#,##0", 999.0, "999"),
            ("\"$\"#,##0.00", 1234.5, "$1,234.50"),
            ("[$$-409]#,##0.00", 1234.5, "$1,234.50"),
            ("#,##0.00\\ \"kr\"", 1234.5, "1,234.50 kr"),
            ("00000", 42.0, "00042"),
            ("#", 0.0, ""),
            ("0.##", 5.0, "5"),
            ("0.??", 5.0, "5.  "),
            ("0.0#", 5.25, "5.25"),
            ("0.00", 5.255, "5.26"),
            ("???", 42.0, " 42"),
            ("0.00", 0.001, "0.00"),
            ("0.00", 0.005, "0.01"),
            // 0.075 is stored just under 0.075; the percent path must still show 7.5, not 7.4.
            ("0.0%", 0.075, "7.5%"),
            ("0", 2.5, "3"),
            ("0.", 5.0, "5."),
            ("0.\"kg\"", 5.0, "5.kg"),
            ("0.00;(0.00)", -3.5, "(3.50)"),
            ("0.00;(0.00)", 3.5, "3.50"),
            ("0;-0;\"zero\"", 0.0, "zero"),
            ("0", -3.0, "-3"),
            ("0.00;(0.00);@", 0.0, "0.00"),
            ("#,##0;[Red](#,##0)", -1234.0, "(1,234)"),
            ("[>=100]0.0;0.00", 250.0, "250.0"),
            ("[>=100]0.0;0.00", 3.0, "3.00"),
            (r#"[=5]"five";0"#, 5.0, "five"),
            (r#"[=5]"five";0"#, 6.0, "6"),
            (r#"[<>5]"other";0"#, 6.0, "other"),
            (r#"[<>5]"other";0"#, 5.0, "5"),
            ("[<0]0.0;0", -2.0, "-2.0"),
            ("0.0,,", 12_345_678.0, "12.3"),
            ("#,##0,", 12_345_678.0, "12,346"),
            ("0.00E+00", 12345.0, "1.23E+04"),
            ("0.00E+00", 0.0001234, "1.23E-04"),
            ("##0.0E+0", 0.0000123, "12.3E-6"),
            ("0.00E+00", 0.0, "0.00E+00"),
            ("0.0E+##", 12345.0, "1.2E+04"),
            ("0.00E+00", 1000.0, "1.00E+03"),
            ("0.00E+00", 999.9995, "1.00E+03"),
            ("0.00E+00", 1e15, "1.00E+15"),
            ("0.00E+00", f64::MAX, "1.80E+308"),
            ("##0.0E+0", 999_950.0, "1.0E+6"),
            ("0.00E+00", 5e-324, "4.94E-324"),
            ("0.00E+00", 9.9999e-310, "1.00E-309"),
            ("0.00E+00", f64::MIN_POSITIVE, "2.23E-308"),
            ("##0.0E+0", 5e-324, "4.9E-324"),
            ("##0.0E+0", 1e-320, "10.0E-321"),
            ("##0.0E+0", 9.9999e-322, "998.0E-324"),
            ("0.00E+00", 1e-322, "9.88E-323"),
            ("# ?/?", 5.25, "5 1/4"),
            ("# ??/??", 2.675, "2 27/40"),
            ("# ?/?", 5.0, "5"),
            ("?/?", 0.5, "1/2"),
            ("# ?/8", 5.25, "5 2/8"),
            ("#\\8?/8", 5.0, "58"),
            ("# ?/8\\8", 5.0, "5 8"),
            ("# ?/8/", 5.0, "5 /"),
            ("# ?/8\\8", 5.5, "5 4/88"),
            ("#\\8?/8", 5.5, "584/8"),
            ("# ?/8,", 5.5, "5 4/8"),
            ("# ?/8,", 5500.0, "5500"),
            ("?/8,", 0.5, "4/8"),
            ("# ?/8\\8,", 5.5, "5 4/88"),
            ("# ?/?,", 5500.0, "5 1/2"),
            ("# ?/16", 5.25, "5 4/16"),
            (&long_code, 0.5, &long_want),
            ("0.00_);(0.00)", 3.5, "3.50 "),
            ("$* 0.00", 3.5, "$3.50"),
            ("0.0\\ \"m/s\"", 3.51, "3.5 m/s"),
            // Quoted date letters must not turn the section into a date.
            ("0\"d\"", 3.0, "3d"),
            ("\"yes\";\"yes\";\"no\"", 1.0, "yes"),
            ("\"yes\";\"yes\";\"no\"", 0.0, "no"),
        ];
        for (code, value, want) in cases {
            assert_eq!(fmt(code, value), want, "{code} {value:?}");
        }
    }

    #[test]
    fn general_keeps_the_literals_around_it() {
        // `General"kg"` shows the unformatted value with its unit, so the
        // literals cannot be dropped on the way through.
        let f = NumberFormat::parse(r#""~"General" kg""#).unwrap();
        assert_eq!(
            f.format_number(1234.5),
            Rendered::General { value: 1234.5, prefix: "~", suffix: " kg" }
        );
    }

    #[test]
    fn general_renders_generally() {
        let f = NumberFormat::parse("General").unwrap();
        assert_eq!(f.format_number(3.5), Rendered::General { value: 3.5, prefix: "", suffix: "" });
        // The positional negative section receives the magnitude.
        let f = NumberFormat::parse("General;General").unwrap();
        assert_eq!(f.format_number(-3.5), Rendered::General { value: 3.5, prefix: "", suffix: "" });
    }

    #[test]
    fn unsupported_constructs_refuse_to_parse() {
        assert!(NumberFormat::parse("[DBNum1]0").is_none());
        assert!(NumberFormat::parse("0.0.0").is_none());
        assert!(NumberFormat::parse("abc0").is_none());
        assert!(NumberFormat::parse("0;0;0;0;0").is_none());
        assert!(NumberFormat::parse("0;0;0;0").is_none());
        assert!(NumberFormat::parse("@;0").is_none());
        assert!(NumberFormat::parse("0;[>5]@").is_none());
        assert!(NumberFormat::parse("[>5][<3]0").is_none());
        // Unquoted currency letters are outside the implemented grammar.
        assert!(NumberFormat::parse("€0.00").is_none());
    }

    #[test]
    fn text_section_applies_to_text_only() {
        let f = NumberFormat::parse("0.00;(0.00);\"-\";\"* \"@\" *\"").unwrap();
        assert_eq!(f.format_text("hi"), Some("* hi *".to_string()));
        let f = NumberFormat::parse("@").unwrap();
        assert_eq!(f.format_text("hi"), Some("hi".to_string()));
        assert_eq!(f.format_number(3.5), Rendered::General { value: 3.5, prefix: "", suffix: "" });
        let f = NumberFormat::parse("0;@_)").unwrap();
        assert_eq!(f.format_text("hi"), Some("hi ".to_string()));
        let f = NumberFormat::parse("0.00").unwrap();
        assert_eq!(f.format_text("hi"), None);
    }

    #[test]
    fn a_date_section_names_only_the_parts_it_asks_for() {
        let parts =
            |code: &str, value| match NumberFormat::parse(code).unwrap().format_number(value) {
                Rendered::DateTime(p) => (p.date, p.time, p.elapsed),
                other => panic!("expected a date/time section, got {other:?}"),
            };
        let cases = [
            ("yyyy-mm-dd", 45000.5, (true, false, false)),
            (r"yyyy\-mm\-dd", 45000.0, (true, false, false)),
            ("d mmm yyyy", 45000.5, (true, false, false)),
            ("h:mm", 45000.5, (false, true, false)),
            ("h:mm AM/PM", 0.5, (false, true, false)),
            ("yyyy-mm-dd hh:mm:ss", 45000.5, (true, true, false)),
            ("[h]:mm:ss", 45000.5, (false, true, true)),
            ("[hh]:mm:ss", 1.5, (false, true, true)),
            // A lone elapsed `m` is minutes with no neighbour to say so.
            ("[m]", 45000.5, (false, true, true)),
            // `m` is minutes beside an hour or a second, and months otherwise.
            ("mm:ss", 45000.5, (false, true, false)),
            ("mm/dd/yyyy", 45000.5, (true, false, false)),
        ];
        for (code, value, want) in cases {
            assert_eq!(parts(code, value), want, "{code} {value:?}");
        }
    }

    #[test]
    fn the_closest_rational_matches_an_exhaustive_search() {
        // The continued fraction walk has to reach the same answer a scan of
        // every denominator would, semiconvergents included.
        let brute = |x: f64, max_den: u64| {
            let mut best = (0u64, 1u64);
            let mut err = f64::INFINITY;
            for d in 1..=max_den {
                let n = (x * d as f64).round();
                let e = (x - n / d as f64).abs();
                if e < err - 1e-15 {
                    err = e;
                    best = (n as u64, d);
                }
            }
            best
        };
        let mut seed = 12_345u64;
        for _ in 0..2_000 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let x = (seed >> 11) as f64 / (1u64 << 53) as f64 * 10.0;
            for places in 1..=4 {
                let max_den = 10u64.pow(places) - 1;
                let (n, d) = closest_rational(x, max_den);
                let (bn, bd) = brute(x, max_den);
                let got = (x - n as f64 / d as f64).abs();
                let want = (x - bn as f64 / bd as f64).abs();
                assert!(got <= want + 1e-12, "x={x} max_den={max_den}: {n}/{d} vs {bn}/{bd}");
            }
        }
    }

    #[test]
    fn the_walk_stays_exact_past_float_precision() {
        // 1/3 as a float is the dyadic 6004799503160661/2^54, which a large
        // bound represents exactly; a float walk stops at 1/3 instead.
        assert_eq!(closest_rational(1.0 / 3.0, u64::MAX), (6004799503160661, 18014398509481984));
    }
}
