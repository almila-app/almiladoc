//! The MTEF equation format of Equation Editor 3.x (v3) and of `MathType` 4 and later (v5),
//! read into one version-neutral tree that the shared TeX builder writes.

use super::tex::{Fraction, Tex, big_operator, function_name};
use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::shared::binary::ByteReader;
use crate::shared::symbol::{Glyph as FontGlyph, SymbolFont, glyph_of};

#[derive(Clone, Debug)]
pub(crate) struct Equation {
    pub tex: String,
    /// Its header marked it inline; otherwise the paragraph around it decides.
    pub inline: bool,
    pub unmapped: u64,
}

const MTEF_V3: u8 = 3;
const MTEF_V5: u8 = 5;
const PRODUCT_BYTES: usize = 4;

const END: u8 = 0;
const LINE: u8 = 1;
const CHAR: u8 = 2;
const TMPL: u8 = 3;
const PILE: u8 = 4;
const MATRIX: u8 = 5;
const EMBELL: u8 = 6;
const RULER: u8 = 7;
const FONT: u8 = 8;
const SIZE: u8 = 9;
const FULL: u8 = 10;
const SUBSYM: u8 = 14;
const COLOR: u8 = 15;
const COLOR_DEF: u8 = 16;
const FONT_DEF: u8 = 17;
const EQN_PREFS: u8 = 18;
const ENCODING_DEF: u8 = 19;
const FUTURE: u8 = 100;

const NUDGE: u8 = 0x08;
const LINE_NULL: u8 = 0x01;
const LINE_SPACE: u8 = 0x04;
const RULER_FOLLOWS: u8 = 0x02;
const V3_CHAR_EMBELL: u8 = 0x02;
const V5_CHAR_EMBELL: u8 = 0x01;
const V5_FUNCTION_START: u8 = 0x02;
const CHAR_8: u8 = 0x04;
const CHAR_16: u8 = 0x10;
const NO_MTCODE: u8 = 0x20;
const COLOR_CMYK: u8 = 0x01;
const COLOR_NAME: u8 = 0x04;
const EQUATION_INLINE: u8 = 0x01;
const VARIATION_WIDE: u8 = 0x80;

const WIDE: u8 = 255;
const NUDGE_WIDE: u8 = 128;
const SIZE_POINTS: u8 = 101;
const SIZE_WIDE: u8 = 100;
const TYPEFACE_BIAS: u8 = 128;
const FN_TEXT: u8 = 1;
const FN_FUNCTION: u8 = 2;
const FN_VECTOR: u8 = 7;
const FN_TEXT_FE: u8 = 12;
const FN_MARKER: u8 = 23;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Version {
    Three,
    Five,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Face {
    Math,
    Text,
    Function,
    Vector,
    Marker,
}

#[derive(Clone, Copy)]
enum Mark {
    Accent(&'static str),
    Prime(&'static str),
    Not,
}

#[derive(Clone, Copy)]
enum Template {
    Fence(&'static str, &'static str),
    Root,
    Fraction(Fraction),
    Wrap(&'static str, u16),
    Operator(Option<&'static str>),
    Integral { sign: Integral, glyphs: usize },
    Limit,
    Scripts { leading: bool },
    Brace(&'static str, char),
}

#[derive(Clone, Copy)]
enum Integral {
    Named(&'static str),
    /// A directed contour integral, which no `KaTeX` or `MathJax` macro names.
    Drawn(char),
}

impl Integral {
    const fn operator(self) -> (Option<&'static str>, Option<char>) {
        match self {
            Self::Named(name) => (Some(name), None),
            Self::Drawn(sign) => (None, Some(sign)),
        }
    }
}

#[derive(Clone, Copy)]
enum Contour {
    Open,
    Closed,
    Clockwise,
    Counterclockwise,
}

impl Template {
    const fn slots(self) -> usize {
        match self {
            Self::Fence(..) | Self::Wrap(..) => 1,
            Self::Root | Self::Fraction(_) | Self::Scripts { .. } | Self::Brace(..) => 2,
            Self::Operator(_) | Self::Integral { .. } | Self::Limit => 3,
        }
    }

    /// The characters its writers draw it with: its fences, its brace, its operator, or an
    /// integral's signs and loop.
    fn glyphs(self) -> usize {
        match self {
            Self::Fence(left, right) => usize::from(left != ".") + usize::from(right != "."),
            Self::Operator(_) | Self::Brace(..) => 1,
            Self::Integral { glyphs, .. } => glyphs,
            Self::Root
            | Self::Fraction(_)
            | Self::Wrap(..)
            | Self::Limit
            | Self::Scripts { .. } => 0,
        }
    }
}

struct Glyph {
    c: char,
    face: Face,
    marks: Vec<Mark>,
    starts_function: bool,
}

enum Node {
    Line(Vec<Self>),
    Glyph(Glyph),
    Unmapped,
    Template(Template, Vec<Self>),
    Pile(Vec<Self>),
    Matrix { cols: usize, cells: Vec<Self> },
}

impl Node {
    const fn is_slot(&self) -> bool {
        matches!(self, Self::Line(_) | Self::Pile(_))
    }
}

enum Stop {
    Undecodable,
    Fatal(ConvertError),
}

impl From<ConvertError> for Stop {
    fn from(e: ConvertError) -> Self {
        Self::Fatal(e)
    }
}

type Step<T> = Result<T, Stop>;

fn need<T>(value: Option<T>) -> Step<T> {
    value.ok_or(Stop::Undecodable)
}

/// `None` when the bytes are not an equation this reader knows, so the caller keeps its
/// fallback; limits and cancellation still fail.
pub(crate) fn mtef_to_tex(
    parse_ctx: &ParseCtx<'_>,
    mtef: &[u8],
) -> Result<Option<Equation>, ConvertError> {
    let mut bytes = ByteReader::new(mtef);
    let version = match bytes.u8() {
        Some(MTEF_V3) => Version::Three,
        Some(MTEF_V5) => Version::Five,
        _ => return Ok(None),
    };
    let mut decoder =
        Decoder { ctx: parse_ctx, bytes, version, unmapped: 0, records: 0, fonts: [None; 256] };
    let (nodes, inline) = match decoder.equation() {
        Ok(read) => read,
        Err(Stop::Undecodable) => return Ok(None),
        Err(Stop::Fatal(e)) => return Err(e),
    };
    let tex = line_tex(parse_ctx, &nodes)?.finish();
    Ok((!tex.is_empty()).then_some(Equation { tex, inline, unmapped: decoder.unmapped }))
}

struct Decoder<'a, 'c> {
    ctx: &'c ParseCtx<'c>,
    bytes: ByteReader<'a>,
    version: Version,
    unmapped: u64,
    records: u64,
    /// The symbol fonts v3 FONT records number; a CHAR names one as 128 minus its number.
    fonts: [Option<SymbolFont>; 256],
}

impl Decoder<'_, '_> {
    fn equation(&mut self) -> Step<(Vec<Node>, bool)> {
        self.skip(PRODUCT_BYTES)?;
        let inline = match self.version {
            Version::Three => false,
            Version::Five => {
                self.string()?;
                self.u8()? & EQUATION_INLINE != 0
            }
        };
        let nodes = self.content(0)?;
        let tail = self.bytes.rest();
        self.skip(tail.len())?;
        need(tail.iter().all(|&byte| byte == 0).then_some(()))?;
        Ok((nodes, inline))
    }

    fn u8(&mut self) -> Step<u8> {
        need(self.bytes.u8())
    }

    fn u16(&mut self) -> Step<u16> {
        need(self.bytes.u16())
    }

    fn skip(&mut self, n: usize) -> Step<()> {
        self.ctx.charge(n as u64)?;
        need(self.bytes.skip(n))
    }

    fn string(&mut self) -> Step<()> {
        let len = need(self.bytes.rest().iter().position(|&byte| byte == 0))?;
        self.skip(len + 1)
    }

    fn unsigned(&mut self) -> Step<u16> {
        match self.u8()? {
            WIDE => self.u16(),
            value => Ok(u16::from(value)),
        }
    }

    fn tag(&mut self) -> Step<(u8, u8)> {
        self.ctx.charge(1)?;
        self.records += 1;
        let max = self.ctx.limits().max_records;
        if self.records > max {
            return Err(ConvertError::limit(Limit::Records, max).into());
        }
        let byte = self.u8()?;
        Ok(match self.version {
            Version::Three => (byte & 0x0F, byte >> 4),
            Version::Five if (LINE..=EMBELL).contains(&byte) => (byte, self.u8()?),
            Version::Five => (byte, 0),
        })
    }

    /// A list whose characters are its content, so one no table maps is lost.
    fn content(&mut self, depth: u64) -> Step<Vec<Node>> {
        let mut nodes = self.list(depth)?;
        let read = nodes.len();
        nodes.retain(|node| !matches!(node, Node::Unmapped));
        self.unmapped += (read - nodes.len()) as u64;
        Ok(nodes)
    }

    /// A record list `depth` lists inside the equation's own: the reader, the TeX writer and the
    /// tree's drop recurse once per list.
    fn list(&mut self, depth: u64) -> Step<Vec<Node>> {
        let max = self.ctx.limits().max_nesting_depth;
        if depth >= max {
            return Err(ConvertError::limit(Limit::NestingDepth, max).into());
        }
        let mut nodes = Vec::new();
        loop {
            let (kind, options) = self.tag()?;
            if kind == END {
                return Ok(nodes);
            }
            if (LINE..=EMBELL).contains(&kind) && options & NUDGE != 0 {
                self.nudge()?;
            }
            let deeper = depth + 1;
            let node = match kind {
                LINE => self.line(options, deeper)?,
                CHAR => match self.glyph(options)? {
                    Some(node) => node,
                    None => continue,
                },
                TMPL => self.template(deeper)?,
                PILE => {
                    self.skip(2)?;
                    self.ruler_if(options)?;
                    Node::Pile(self.content(deeper)?)
                }
                MATRIX => self.matrix(deeper)?,
                _ => {
                    self.definition(kind)?;
                    continue;
                }
            };
            nodes.push(node);
        }
    }

    fn nudge(&mut self) -> Step<()> {
        let [dx, dy] = need(self.bytes.array())?;
        if dx == NUDGE_WIDE && dy == NUDGE_WIDE {
            self.skip(4)?;
        }
        Ok(())
    }

    fn line(&mut self, options: u8, depth: u64) -> Step<Node> {
        if options & LINE_SPACE != 0 {
            self.skip(2)?;
        }
        self.ruler_if(options)?;
        if options & LINE_NULL != 0 {
            return Ok(Node::Line(Vec::new()));
        }
        Ok(Node::Line(self.content(depth)?))
    }

    fn ruler_if(&mut self, options: u8) -> Step<()> {
        if options & RULER_FOLLOWS == 0 {
            return Ok(());
        }
        match self.tag()? {
            (RULER, _) => self.ruler(),
            _ => Err(Stop::Undecodable),
        }
    }

    fn ruler(&mut self) -> Step<()> {
        let stops = self.u8()?;
        self.skip(usize::from(stops) * 3)
    }

    fn glyph(&mut self, options: u8) -> Step<Option<Node>> {
        let (face, font) = self.typeface()?;
        let code = match self.version {
            Version::Three => Some(self.u16()?),
            Version::Five => {
                let code = if options & NO_MTCODE == 0 { Some(self.u16()?) } else { None };
                if options & CHAR_8 != 0 {
                    self.skip(1)?;
                }
                if options & CHAR_16 != 0 {
                    self.skip(2)?;
                }
                code
            }
        };
        let embellished = match self.version {
            Version::Three => V3_CHAR_EMBELL,
            Version::Five => V5_CHAR_EMBELL,
        };
        let marks = if options & embellished != 0 { self.marks()? } else { Vec::new() };
        if face == Face::Marker {
            return Ok(None);
        }
        let starts_function = self.version == Version::Five && options & V5_FUNCTION_START != 0;
        let decoded =
            code.and_then(|code| font.map_or_else(|| character(code), |font| in_font(font, code)));
        Ok(Some(match decoded {
            Some(c) => Node::Glyph(Glyph { c, face, marks, starts_function }),
            None => Node::Unmapped,
        }))
    }

    fn typeface(&mut self) -> Step<(Face, Option<SymbolFont>)> {
        let byte = self.u8()?;
        if self.version == Version::Five && byte == WIDE {
            self.skip(2)?;
            return Ok((Face::Math, None));
        }
        let face = match byte.checked_sub(TYPEFACE_BIAS) {
            Some(FN_TEXT | FN_TEXT_FE) => Face::Text,
            Some(FN_FUNCTION) => Face::Function,
            Some(FN_VECTOR) => Face::Vector,
            Some(FN_MARKER) => Face::Marker,
            _ => Face::Math,
        };
        let explicit = (self.version == Version::Three && byte < TYPEFACE_BIAS)
            .then(|| self.fonts.get(usize::from(TYPEFACE_BIAS - byte)).copied().flatten());
        Ok((face, explicit.flatten()))
    }

    fn marks(&mut self) -> Step<Vec<Mark>> {
        let mut marks = Vec::new();
        loop {
            match self.tag()? {
                (END, _) => return Ok(marks),
                (EMBELL, options) => {
                    if options & NUDGE != 0 {
                        self.nudge()?;
                    }
                    self.embellishments(&mut marks)?;
                }
                _ => return Err(Stop::Undecodable),
            }
        }
    }

    /// One code in v5; v3, as Equation Editor writes it, lists codes up to a zero byte.
    fn embellishments(&mut self, marks: &mut Vec<Mark>) -> Step<()> {
        loop {
            self.ctx.charge(1)?;
            let code = self.u8()?;
            if self.version == Version::Three && code == 0 {
                return Ok(());
            }
            marks.push(need(mark(self.version, code))?);
            if self.version == Version::Five {
                return Ok(());
            }
        }
    }

    fn template(&mut self, depth: u64) -> Step<Node> {
        let selector = self.u8()?;
        let template = match self.version {
            Version::Three => {
                let variation = self.u8()?;
                v3_template(selector, variation)
            }
            Version::Five => {
                let first = self.u8()?;
                let variation = if first & VARIATION_WIDE == 0 {
                    u16::from(first)
                } else {
                    u16::from(first & !VARIATION_WIDE) | (u16::from(self.u8()?) << 8)
                };
                v5_template(selector, variation)
            }
        };
        let template = need(template)?;
        self.skip(1)?;
        let mut parts = self.list(depth)?;
        let slots = parts.iter().filter(|part| part.is_slot()).count();
        let glyphs = parts.iter().filter(|part| matches!(part, Node::Glyph(_) | Node::Unmapped));
        let glyphs = glyphs.count();
        let drawn = slots + glyphs == parts.len() && glyphs <= template.glyphs();
        need((drawn && slots <= template.slots()).then_some(()))?;
        let read = parts.len();
        parts.retain(|part| !matches!(part, Node::Unmapped));
        if matches!(template, Template::Operator(None)) {
            self.unmapped += (read - parts.len()) as u64;
        }
        Ok(Node::Template(template, parts))
    }

    fn matrix(&mut self, depth: u64) -> Step<Node> {
        self.skip(3)?;
        let rows = usize::from(self.u8()?);
        let cols = usize::from(self.u8()?);
        let ruled = [self.ruled(rows)?, self.ruled(cols)?];
        let cells = self.content(depth)?;
        need((ruled == [false; 2] && cols > 0 && cells.len() <= rows * cols).then_some(()))?;
        self.ctx.charge((cells.len().next_multiple_of(cols) - cells.len()) as u64)?;
        Ok(Node::Matrix { cols, cells })
    }

    /// Whether a partition line around `count` rows or columns is drawn: two bits per line, low
    /// bits first, padded to whole bytes.
    fn ruled(&mut self, count: usize) -> Step<bool> {
        let bytes = need(self.bytes.take((count + 1).div_ceil(4)))?;
        self.ctx.charge(bytes.len() as u64)?;
        let style =
            |line: usize| bytes.get(line / 4).map_or(0, |byte| (byte >> (line % 4 * 2)) & 3);
        Ok((0..=count).any(|line| style(line) != 0))
    }

    fn definition(&mut self, kind: u8) -> Step<()> {
        match (self.version, kind) {
            (_, RULER) => self.ruler(),
            (Version::Three, FONT) => self.font(),
            (Version::Five, FONT) => {
                self.unsigned()?;
                self.skip(1)
            }
            (_, SIZE) => self.size(),
            (_, FULL..=SUBSYM) => Ok(()),
            (Version::Five, COLOR) => self.unsigned().map(drop),
            (Version::Five, COLOR_DEF) => self.color(),
            (Version::Five, FONT_DEF) => {
                self.unsigned()?;
                self.string()
            }
            (Version::Five, EQN_PREFS) => self.preferences(),
            (Version::Five, ENCODING_DEF) => self.string(),
            (Version::Five, FUTURE..) => {
                let len = self.unsigned()?;
                self.skip(usize::from(len))
            }
            _ => Err(Stop::Undecodable),
        }
    }

    fn font(&mut self) -> Step<()> {
        self.ctx.charge(1)?;
        let number = self.u8()?;
        self.skip(1)?;
        let len = need(self.bytes.rest().iter().position(|&byte| byte == 0))?;
        self.ctx.charge(len as u64)?;
        let name = need(self.bytes.take(len))?;
        self.skip(1)?;
        let font = std::str::from_utf8(name).ok().and_then(SymbolFont::from_name);
        if let Some(slot) = self.fonts.get_mut(usize::from(number)) {
            *slot = font;
        }
        Ok(())
    }

    fn size(&mut self) -> Step<()> {
        match self.u8()? {
            SIZE_POINTS => self.skip(2),
            SIZE_WIDE => self.skip(3),
            _ => self.skip(1),
        }
    }

    fn color(&mut self) -> Step<()> {
        let options = self.u8()?;
        self.skip(if options & COLOR_CMYK != 0 { 8 } else { 6 })?;
        if options & COLOR_NAME != 0 {
            self.string()?;
        }
        Ok(())
    }

    fn preferences(&mut self) -> Step<()> {
        self.skip(1)?;
        self.dimensions()?;
        self.dimensions()?;
        let styles = self.u8()?;
        for _ in 0..styles {
            if self.unsigned()? != 0 {
                self.skip(1)?;
            }
        }
        Ok(())
    }

    /// A count, then per dimension a unit nibble and value nibbles ended by 0xF, high
    /// nibble first and padded to a whole byte.
    fn dimensions(&mut self) -> Step<()> {
        const VALUE_END: u8 = 0xF;
        let mut left = self.u8()?;
        let mut in_value = false;
        while left > 0 {
            let byte = self.u8()?;
            self.ctx.charge(1)?;
            for nibble in [byte >> 4, byte & 0x0F] {
                if left == 0 {
                    break;
                }
                if !in_value {
                    in_value = true;
                } else if nibble == VALUE_END {
                    in_value = false;
                    left -= 1;
                }
            }
        }
        Ok(())
    }
}

/// A font index the symbol font draws; a code past its table is the character itself.
fn in_font(font: SymbolFont, code: u16) -> Option<char> {
    let c = char::from_u32(u32::from(code))?;
    match glyph_of(font, c) {
        FontGlyph::Char(drawn) if drawn != c => Some(drawn),
        FontGlyph::Char(_) => character(code),
        FontGlyph::Checkbox(checked) => Some(if checked { '\u{2611}' } else { '\u{2610}' }),
        FontGlyph::Unmapped => None,
    }
}

fn character(code: u16) -> Option<char> {
    match code {
        0x09 => Some(' '),
        0xE000..=0xF8FF => mt_extra(code),
        code => char::from_u32(u32::from(code)).filter(|c| !c.is_control()),
    }
}

/// MT Extra private-use codes as the Unicode characters they draw.
#[rustfmt::skip]
fn mt_extra(code: u16) -> Option<char> {
    Some(match code {
        0xEB01 => '\u{200b}', 0xEB02 => '\u{2009}', 0xEB04 => '\u{2005}', 0xEB05 => '\u{2003}',
        0xEB08 => '\u{a0}', 0xEF04 | 0xEF05 => '\u{200a}', 0xE421 => '⩾', 0xE425 => '⩽',
        0xEC09 => '∥', 0xE083 => '+',
        _ => return None,
    })
}

#[rustfmt::skip]
fn mark(version: Version, code: u8) -> Option<Mark> {
    use Mark::{Accent, Not, Prime};
    Some(match (version, code) {
        (_, 2) => Accent("\\dot"), (_, 3) => Accent("\\ddot"), (_, 4) => Accent("\\dddot"),
        (_, 5) => Prime("'"), (_, 6) => Prime("''"), (_, 8) => Accent("\\tilde"),
        (_, 9) => Accent("\\hat"), (_, 10) => Not, (_, 11) => Accent("\\vec"),
        (_, 12) => Accent("\\overleftarrow"), (_, 13) => Accent("\\overleftrightarrow"),
        (_, 17) => Accent("\\bar"), (_, 18) => Prime("'''"), (_, 20) => Accent("\\breve"),
        (Version::Five, 24) => Accent("\\ddddot"), (Version::Five, 29) => Accent("\\underline"),
        (Version::Five, 30) => Accent("\\utilde"), (Version::Five, 33) => Accent("\\underrightarrow"),
        (Version::Five, 34) => Accent("\\underleftarrow"),
        (Version::Five, 35) => Accent("\\underleftrightarrow"),
        _ => return None,
    })
}

const FENCES: [(&str, &str); 9] = [
    ("\\langle", "\\rangle"),
    ("(", ")"),
    ("\\{", "\\}"),
    ("[", "]"),
    ("|", "|"),
    ("\\Vert", "\\Vert"),
    ("\\lfloor", "\\rfloor"),
    ("\\lceil", "\\rceil"),
    ("\\llbracket", "\\rrbracket"),
];
const INTERVAL: [&str; 4] = ["(", ")", "[", "]"];
const OVER_ARROWS: [&str; 3] = ["\\overleftarrow", "\\overrightarrow", "\\overleftrightarrow"];
const UNDER_ARROWS: [&str; 3] = ["\\underleftarrow", "\\underrightarrow", "\\underleftrightarrow"];
const HARPOONS: [&str; 2] = ["\\overleftharpoon", "\\overrightharpoon"];

/// `None` for a directed contour around more than one sign, which no character draws.
fn integral(signs: u16, contour: Contour) -> Option<Template> {
    use Integral::{Drawn, Named};
    let sign = match (signs, contour) {
        (2, Contour::Open) => Named("\\iint"),
        (3, Contour::Open) => Named("\\iiint"),
        (2, Contour::Closed) => Named("\\oiint"),
        (3, Contour::Closed) => Named("\\oiiint"),
        (_, Contour::Open) => Named("\\int"),
        (_, Contour::Closed) => Named("\\oint"),
        (0 | 1, Contour::Clockwise) => Drawn('\u{2232}'),
        (0 | 1, Contour::Counterclockwise) => Drawn('\u{2233}'),
        _ => return None,
    };
    let glyphs = usize::from(signs.max(1)) + usize::from(!matches!(contour, Contour::Open));
    Some(Template::Integral { sign, glyphs })
}

const fn closed(contour: bool) -> Contour {
    if contour { Contour::Closed } else { Contour::Open }
}

/// MTEF v5 "Template selectors and variations".
fn v5_template(selector: u8, variation: u16) -> Option<Template> {
    const LEFT: u16 = 0x1;
    const RIGHT: u16 = 0x2;
    const CONTOUR: u16 = 0xC;
    const LOOP: u16 = 0x4;
    const CLOCKWISE: u16 = 0x8;
    const UNDER: u16 = 0x4;
    const HARPOON: u16 = 0x8;
    const BOX_SIDES: u16 = 0x1E;
    let side = |bit: u16, fence: &'static str| if variation & bit != 0 { fence } else { "." };
    let interval = |shift: u16| INTERVAL.get(usize::from((variation >> shift) & 0x3)).copied();
    let flag = variation & 0x1 != 0;
    Some(match selector {
        0..=8 => {
            let &(left, right) = FENCES.get(usize::from(selector))?;
            Template::Fence(side(LEFT, left), side(RIGHT, right))
        }
        9 => Template::Fence(interval(0)?, interval(4)?),
        10 => Template::Root,
        11 if variation & 0x2 != 0 => Template::Fraction(Fraction::Linear),
        11 => Template::Fraction(Fraction::Bar),
        12 => Template::Wrap("\\underline", 1 + u16::from(flag)),
        13 => Template::Wrap("\\overline", 1 + u16::from(flag)),
        15 => {
            let contour = match variation & CONTOUR {
                0 => Contour::Open,
                LOOP => Contour::Closed,
                CLOCKWISE => Contour::Clockwise,
                _ => Contour::Counterclockwise,
            };
            integral(variation & 0x3, contour)?
        }
        16 => Template::Operator(Some("\\sum")),
        17 => Template::Operator(Some("\\prod")),
        18 => Template::Operator(Some("\\coprod")),
        19 => Template::Operator(Some("\\bigcup")),
        20 => Template::Operator(Some("\\bigcap")),
        21 | 22 => Template::Operator(None),
        23 => Template::Limit,
        24 if flag => Template::Brace("\\overbrace", '^'),
        24 => Template::Brace("\\underbrace", '_'),
        27..=29 => Template::Scripts { leading: flag },
        31 => {
            let arrow = usize::from(variation & (LEFT | RIGHT)).checked_sub(1)?;
            let arrows = match (variation & UNDER != 0, variation & HARPOON != 0) {
                (false, false) => OVER_ARROWS.as_slice(),
                (true, false) => UNDER_ARROWS.as_slice(),
                (false, true) => HARPOONS.as_slice(),
                (true, true) => return None,
            };
            Template::Wrap(arrows.get(arrow)?, 1)
        }
        32 => Template::Wrap("\\widetilde", 1),
        33 => Template::Wrap("\\widehat", 1),
        37 if variation & BOX_SIDES == BOX_SIDES => Template::Wrap("\\boxed", 1),
        _ => return None,
    })
}

/// MTEF v3 "Template Selectors and Variations": its own numbering, with one-sided fences
/// as variations and half-open intervals as selectors.
fn v3_template(selector: u8, variation: u8) -> Option<Template> {
    const HALF_OPEN: [(&str, &str); 5] =
        [("[", "["), ("]", "]"), ("]", "["), ("[", ")"), ("(", "]")];
    let doubled = 1 + u16::from(variation == 1);
    Some(match selector {
        0..=5 => {
            let &(left, right) = FENCES.get(usize::from(selector))?;
            match variation {
                0 => Template::Fence(left, right),
                1 => Template::Fence(left, "."),
                2 => Template::Fence(".", right),
                _ => return None,
            }
        }
        6 | 7 => {
            let &(left, right) = FENCES.get(usize::from(selector))?;
            Template::Fence(left, right)
        }
        8..=12 => {
            let &(left, right) = HALF_OPEN.get(usize::from(selector - 8))?;
            Template::Fence(left, right)
        }
        13 => Template::Root,
        14 => Template::Fraction(Fraction::Bar),
        15 => Template::Scripts { leading: false },
        16 => Template::Wrap("\\underline", doubled),
        17 => Template::Wrap("\\overline", doubled),
        21 => integral(1, closed(matches!(variation, 3 | 4)))?,
        22 | 23 => integral(u16::from(selector - 20), closed(matches!(variation, 2 | 3)))?,
        24 => integral(1, closed(variation == 2))?,
        25 | 26 => integral(u16::from(selector - 23), closed(variation == 0))?,
        27 => Template::Brace("\\overbrace", '^'),
        28 => Template::Brace("\\underbrace", '_'),
        29 | 30 => Template::Operator(Some("\\sum")),
        31 | 32 => Template::Operator(Some("\\prod")),
        33 | 34 => Template::Operator(Some("\\coprod")),
        35 | 36 => Template::Operator(Some("\\bigcup")),
        37 | 38 => Template::Operator(Some("\\bigcap")),
        39 => Template::Limit,
        41 => Template::Fraction(Fraction::Linear),
        42 | 43 => Template::Operator(None),
        44 => Template::Scripts { leading: true },
        46 => Template::Wrap(UNDER_ARROWS.get(usize::from(variation))?, 1),
        47 => Template::Wrap(OVER_ARROWS.get(usize::from(variation))?, 1),
        _ => return None,
    })
}

fn charged(ctx: &ParseCtx<'_>, tex: Tex) -> Result<Tex, ConvertError> {
    ctx.charge(tex.len() as u64)?;
    Ok(tex)
}

fn line_tex(ctx: &ParseCtx<'_>, nodes: &[Node]) -> Result<Tex, ConvertError> {
    let mut line = Line::default();
    let mut run: Option<(Face, String)> = None;
    for node in nodes {
        if let Node::Glyph(Glyph { c, face, marks, starts_function }) = node
            && marks.is_empty()
            && *face != Face::Math
        {
            match &mut run {
                Some((open, text)) if open == face && !starts_function => text.push(*c),
                _ => {
                    line.run(ctx, run.take())?;
                    run = Some((*face, c.to_string()));
                }
            }
            continue;
        }
        line.run(ctx, run.take())?;
        let written = alone(ctx, node)?;
        if matches!(node, Node::Template(Template::Scripts { leading: false }, _)) {
            line.script(ctx, &written)?;
        } else {
            line.place(ctx, &written)?;
        }
    }
    line.run(ctx, run.take())?;
    charged(ctx, line.tex)
}

/// A line written atom by atom. A script template or a raised character attaches to the last
/// atom; one that would double its script groups it once, and gets an empty base after that, so
/// no text is shifted twice.
#[derive(Default)]
struct Line {
    tex: Tex,
    base: usize,
    grouped: bool,
    scripted: bool,
}

impl Line {
    fn separate(&mut self, ctx: &ParseCtx<'_>) -> Result<(), ConvertError> {
        if self.grouped {
            self.tex.push_str("{}");
        } else {
            ctx.charge((self.tex.len() - self.base) as u64)?;
            self.tex.group_from(self.base);
            self.grouped = true;
        }
        Ok(())
    }

    fn place(&mut self, ctx: &ParseCtx<'_>, written: &Tex) -> Result<(), ConvertError> {
        if written.is_empty() {
            return Ok(());
        }
        let start = self.tex.len();
        let raised = written.raises();
        if raised && self.tex.carries_superscript() {
            self.separate(ctx)?;
        }
        self.tex.push_tex(written);
        if !raised && !written.is_blank() {
            (self.base, self.grouped) = (start, false);
        }
        self.scripted = false;
        Ok(())
    }

    fn script(&mut self, ctx: &ParseCtx<'_>, scripts: &Tex) -> Result<(), ConvertError> {
        if scripts.is_empty() {
            return Ok(());
        }
        if self.tex.is_empty() || self.scripted {
            self.tex.push_str("{}");
        } else if self.tex.ends_with_script() {
            self.separate(ctx)?;
        }
        self.tex.push_tex(scripts);
        self.scripted = true;
        Ok(())
    }

    fn run(&mut self, ctx: &ParseCtx<'_>, run: Option<(Face, String)>) -> Result<(), ConvertError> {
        if run.is_none() {
            return Ok(());
        }
        let mut written = Tex::default();
        flush(&mut written, run);
        self.place(ctx, &charged(ctx, written)?)
    }
}

fn flush(tex: &mut Tex, run: Option<(Face, String)>) {
    let Some((face, text)) = run else { return };
    match face {
        Face::Text => tex.push_text_mode(&text),
        Face::Vector => {
            let mut inner = Tex::default();
            text.chars().for_each(|c| inner.push_math_char(c));
            tex.push_command("\\mathbf", &inner);
        }
        Face::Function => {
            // MathType gives the function face to punctuation after a name too, as in `min,`.
            let named = text.trim_start();
            let (name, rest) = named
                .split_at(named.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(named.len()));
            let rest = match function_name(name) {
                Some(name) => {
                    tex.push_macro(&name);
                    rest
                }
                None => text.as_str(),
            };
            rest.chars().for_each(|c| tex.push_math_char(c));
        }
        Face::Math | Face::Marker => text.chars().for_each(|c| tex.push_math_char(c)),
    }
}

fn alone(ctx: &ParseCtx<'_>, node: &Node) -> Result<Tex, ConvertError> {
    let mut tex = Tex::default();
    node_tex(ctx, node, &mut tex)?;
    charged(ctx, tex)
}

fn node_tex(ctx: &ParseCtx<'_>, node: &Node, tex: &mut Tex) -> Result<(), ConvertError> {
    match node {
        Node::Line(nodes) => tex.push_tex(&line_tex(ctx, nodes)?),
        Node::Glyph(glyph) => tex.push_tex(&glyph_tex(ctx, glyph)?),
        Node::Unmapped => {}
        Node::Template(template, parts) => template_tex(ctx, *template, parts, tex)?,
        Node::Pile(lines) => {
            if let [line] = lines.as_slice() {
                node_tex(ctx, line, tex)?;
            } else {
                let rows: Vec<Vec<Tex>> = lines
                    .iter()
                    .map(|line| alone(ctx, line).map(|tex| vec![tex]))
                    .collect::<Result<_, _>>()?;
                tex.push_environment("gathered", &rows);
            }
        }
        Node::Matrix { cols, cells } => {
            let rows: Vec<Vec<Tex>> = cells
                .chunks(*cols)
                .map(|row| {
                    let mut cells: Vec<Tex> =
                        row.iter().map(|cell| alone(ctx, cell)).collect::<Result<_, _>>()?;
                    cells.resize_with(*cols, Tex::default);
                    Ok(cells)
                })
                .collect::<Result<_, ConvertError>>()?;
            tex.push_environment("matrix", &rows);
        }
    }
    Ok(())
}

fn glyph_tex(ctx: &ParseCtx<'_>, glyph: &Glyph) -> Result<Tex, ConvertError> {
    let mut tex = Tex::default();
    flush(&mut tex, Some((glyph.face, glyph.c.to_string())));
    for mark in &glyph.marks {
        let inner = charged(ctx, tex)?;
        tex = Tex::default();
        match mark {
            Mark::Accent(command) => tex.push_command(command, &inner),
            Mark::Prime(primes) => {
                if inner.carries_superscript() {
                    tex.push_group(&inner);
                } else {
                    tex.push_tex(&inner);
                }
                tex.push_str(primes);
            }
            Mark::Not => {
                tex.push_macro("\\not");
                tex.push_tex(&inner);
            }
        }
    }
    Ok(tex)
}

fn first_glyph(parts: &[Node]) -> Option<char> {
    parts.iter().find_map(|part| match part {
        Node::Glyph(glyph) => Some(glyph.c),
        _ => None,
    })
}

fn operator(tex: &mut Tex, named: Option<&'static str>, drawn: Option<char>) {
    if let Some(op) = named {
        tex.push_macro(op);
        return;
    }
    let mut op = Tex::default();
    if let Some(c) = drawn {
        op.push_math_char(c);
    }
    tex.push_command("\\operatorname*", &op);
}

/// MTEF lists main, upper, lower; Equation Editor and `MathType` write main, lower, upper.
fn limits(tex: &mut Tex, main: &Tex, lower: &Tex, upper: &Tex) {
    script(tex, '_', lower);
    script(tex, '^', upper);
    tex.push_group(main);
}

fn script(tex: &mut Tex, mark: char, slot: &Tex) {
    if !slot.is_empty() {
        tex.push_script(mark, slot);
    }
}

fn template_tex(
    ctx: &ParseCtx<'_>,
    template: Template,
    parts: &[Node],
    tex: &mut Tex,
) -> Result<(), ConvertError> {
    let mut slots = parts.iter().filter(|part| part.is_slot()).map(|part| alone(ctx, part));
    let mut slot = || slots.next().transpose().map(Option::unwrap_or_default);
    let (first, second, third) = (slot()?, slot()?, slot()?);
    match template {
        Template::Fence(left, right) => {
            tex.push_macro("\\left");
            tex.push_str(left);
            tex.push_tex(&first);
            tex.push_macro("\\right");
            tex.push_str(right);
        }
        Template::Root => {
            tex.push_sqrt((!second.is_empty()).then_some(&second));
            tex.push_group(&first);
        }
        Template::Fraction(kind) => tex.push_fraction(kind, &first, &second),
        Template::Wrap(command, times) => {
            let mut wrapped = first;
            for _ in 0..times {
                let mut outer = Tex::default();
                outer.push_command(command, &wrapped);
                wrapped = outer;
            }
            tex.push_tex(&wrapped);
        }
        Template::Operator(named) => {
            let glyph = first_glyph(parts);
            operator(tex, named.or_else(|| glyph.and_then(big_operator)), glyph);
            limits(tex, &first, &second, &third);
        }
        Template::Integral { sign, .. } => {
            let (named, drawn) = sign.operator();
            operator(tex, named, drawn);
            limits(tex, &first, &second, &third);
        }
        Template::Limit => {
            let start = tex.len();
            tex.push_tex(&first);
            if first.ends_with_script() {
                ctx.charge((tex.len() - start) as u64)?;
                tex.group_from(start);
            }
            script(tex, '_', &second);
            script(tex, '^', &third);
        }
        Template::Scripts { leading } => {
            if leading {
                tex.push_str("{}");
            }
            script(tex, '_', &first);
            script(tex, '^', &second);
        }
        Template::Brace(command, mark) => {
            tex.push_command(command, &first);
            script(tex, mark, &second);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Decoder, Stop, Version, mtef_to_tex};
    use crate::ctx::ParseCtx;
    use crate::error::ConvertError;
    use crate::limits::{Limit, Limits};
    use crate::shared::binary::ByteReader;
    use std::assert_matches;

    struct W(Version);

    impl W {
        fn tag(&self, kind: u8, options: u8) -> Vec<u8> {
            match self.0 {
                Version::Three => vec![kind | (options << 4)],
                Version::Five => vec![kind, options],
            }
        }

        fn line(&self, items: &[Vec<u8>]) -> Vec<u8> {
            [self.tag(1, 0), items.concat(), vec![0]].concat()
        }

        fn null(&self) -> Vec<u8> {
            self.tag(1, 1)
        }

        fn glyph(&self, options: u8, face: u8, c: char) -> Vec<u8> {
            let code = u16::try_from(u32::from(c)).unwrap();
            [self.tag(2, options), vec![face + 128], code.to_le_bytes().to_vec()].concat()
        }

        fn ch(&self, face: u8, text: &str) -> Vec<u8> {
            text.chars().flat_map(|c| self.glyph(0, face, c)).collect()
        }

        fn var(&self, text: &str) -> Vec<u8> {
            self.ch(3, text)
        }

        /// A character, then one EMBELL record per code and the END of their list.
        fn embellished(&self, face: u8, c: char, codes: &[u8]) -> Vec<u8> {
            let flag = if self.0 == Version::Three { 0x2 } else { 0x1 };
            let records: Vec<u8> = codes.iter().flat_map(|&code| self.embell(&[code])).collect();
            [self.glyph(flag, face, c), records, vec![0]].concat()
        }

        fn embell(&self, codes: &[u8]) -> Vec<u8> {
            match self.0 {
                Version::Three => [self.tag(6, 0), codes.to_vec(), vec![0]].concat(),
                Version::Five => [self.tag(6, 0), codes.to_vec()].concat(),
            }
        }

        fn tmpl(&self, selector: u8, variation: u16, parts: &[Vec<u8>]) -> Vec<u8> {
            let [low, high] = variation.to_le_bytes();
            let variation = match self.0 {
                Version::Three => vec![low],
                Version::Five if variation < 0x80 => vec![low],
                Version::Five => vec![(low & 0x7F) | 0x80, high],
            };
            [self.tag(3, 0), vec![selector], variation, vec![0], parts.concat(), vec![0]].concat()
        }

        fn pile(&self, lines: &[Vec<u8>]) -> Vec<u8> {
            [self.tag(4, 0), vec![1, 0], lines.concat(), vec![0]].concat()
        }

        fn matrix(&self, rows: u8, cols: u8, cells: &[Vec<u8>]) -> Vec<u8> {
            let partitions = |lines: u8| vec![0; ((usize::from(lines) + 1) * 2).div_ceil(8)];
            self.parted(rows, cols, &[partitions(rows), partitions(cols)].concat(), cells)
        }

        /// A matrix whose row then column partition lines take two bits each, low bits first.
        fn parted(&self, rows: u8, cols: u8, lines: &[u8], cells: &[Vec<u8>]) -> Vec<u8> {
            let layout = [&[0, 1, 0, rows, cols][..], lines].concat();
            [self.tag(5, 0), layout, cells.concat(), vec![0]].concat()
        }

        fn font(&self, number: u8, name: &str) -> Vec<u8> {
            [self.tag(8, 0), vec![number, 0], name.as_bytes().to_vec(), vec![0]].concat()
        }

        fn in_font(&self, number: u8, code: u16) -> Vec<u8> {
            [self.tag(2, 0), vec![128 - number], code.to_le_bytes().to_vec()].concat()
        }

        fn nudged(&self, nudge: &[u8]) -> Vec<u8> {
            [self.tag(2, 0x8), nudge.to_vec(), vec![131, b'x', 0]].concat()
        }

        fn header(&self, inline: bool) -> Vec<u8> {
            match self.0 {
                Version::Three => vec![3, 1, 1, 3, 0],
                Version::Five => [&[5, 1, 0, 6, 0][..], b"DSMT4\0", &[u8::from(inline)]].concat(),
            }
        }

        fn equation(&self, items: &[Vec<u8>]) -> Vec<u8> {
            [self.header(false), vec![10], self.line(items), vec![0]].concat()
        }
    }

    const BOTH: [Version; 2] = [Version::Three, Version::Five];

    fn tex(bytes: &[u8]) -> Option<String> {
        mtef_to_tex(&ParseCtx::default(), bytes).unwrap().map(|equation| equation.tex)
    }

    type Parts = fn(&W) -> Vec<Vec<u8>>;

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of templates in both versions")]
    fn each_template_reads_the_same_in_its_own_version_numbering() {
        let x: Parts = |w| vec![w.line(&[w.var("x")])];
        let fence =
            |v3: u8, v5: u8, tex: &'static str| ("a fence", (v3, 0), (v5, 3), "", "", x, tex);
        let rows: Vec<Row> = vec![
            (
                "a fraction",
                (14, 0),
                (11, 0),
                "",
                "",
                |w| vec![w.line(&[w.var("a")]), w.line(&[w.var("b")])],
                r"\frac{a}{b}",
            ),
            (
                "a slash fraction",
                (41, 0),
                (11, 2),
                "",
                "",
                |w| vec![w.line(&[w.var("a")]), w.line(&[w.var("b")])],
                "{a}/{b}",
            ),
            (
                "a square root",
                (13, 0),
                (10, 0),
                "",
                "",
                |w| vec![w.line(&[w.var("x")]), w.null()],
                r"\sqrt{x}",
            ),
            (
                "a root with an index",
                (13, 1),
                (10, 1),
                "",
                "",
                |w| vec![w.line(&[w.var("x")]), w.line(&[w.ch(8, "3")])],
                r"\sqrt[3]{x}",
            ),
            (
                "a superscript",
                (15, 0),
                (28, 0),
                "x",
                "",
                |w| vec![w.null(), w.line(&[w.ch(8, "2")])],
                "x^{2}",
            ),
            (
                "a subscript",
                (15, 1),
                (27, 0),
                "x",
                "",
                |w| vec![w.line(&[w.var("i")]), w.null()],
                "x_{i}",
            ),
            (
                "both scripts",
                (15, 2),
                (29, 0),
                "x",
                "",
                |w| vec![w.line(&[w.var("i")]), w.line(&[w.ch(8, "2")])],
                "x_{i}^{2}",
            ),
            (
                "leading scripts",
                (44, 2),
                (29, 1),
                "",
                "x",
                |w| vec![w.line(&[w.var("i")]), w.line(&[w.ch(8, "2")])],
                "{}_{i}^{2}x",
            ),
            fence(0, 0, r"\left\langle x\right\rangle"),
            fence(1, 1, r"\left(x\right)"),
            fence(2, 2, r"\left\{x\right\}"),
            fence(3, 3, r"\left[x\right]"),
            fence(4, 4, r"\left|x\right|"),
            fence(5, 5, r"\left\Vert x\right\Vert"),
            fence(6, 6, r"\left\lfloor x\right\rfloor"),
            fence(7, 7, r"\left\lceil x\right\rceil"),
            ("a left fence", (1, 1), (1, 1), "", "", x, r"\left(x\right."),
            ("a right fence", (1, 2), (1, 2), "", "", x, r"\left.x\right)"),
            ("two left brackets", (8, 0), (9, 0x22), "", "", x, r"\left[x\right["),
            ("two right brackets", (9, 0), (9, 0x33), "", "", x, r"\left]x\right]"),
            ("brackets turned out", (10, 0), (9, 0x23), "", "", x, r"\left]x\right["),
            ("a bracket then a parenthesis", (11, 0), (9, 0x12), "", "", x, r"\left[x\right)"),
            ("a parenthesis then a bracket", (12, 0), (9, 0x30), "", "", x, r"\left(x\right]"),
            (
                "a sum over a stacked lower limit",
                (29, 1),
                (16, 0x30),
                "",
                "",
                |w| {
                    vec![
                        w.line(&[w.var("x")]),
                        w.pile(&[
                            w.line(&[w.var("i"), w.ch(6, "="), w.ch(8, "1")]),
                            w.line(&[w.var("j")]),
                        ]),
                        w.line(&[w.var("n")]),
                        w.glyph(0, 6, '\u{2211}'),
                    ]
                },
                r"\sum_{\begin{gathered} i=1 \\ j \end{gathered}}^{n}{x}",
            ),
            (
                "a limit",
                (39, 1),
                (23, 0x10),
                "",
                "",
                |w| {
                    vec![
                        w.line(&[w.ch(2, "lim")]),
                        w.line(&[w.var("x"), w.ch(6, "\u{2192}"), w.ch(8, "0")]),
                    ]
                },
                r"\lim_{x\to0}",
            ),
            ("an overbar", (17, 0), (13, 0), "", "", x, r"\overline{x}"),
            ("an underbar", (16, 0), (12, 0), "", "", x, r"\underline{x}"),
            ("a double underbar", (16, 1), (12, 1), "", "", x, r"\underline{\underline{x}}"),
            ("an under arrow", (46, 1), (31, 6), "", "", x, r"\underrightarrow{x}"),
            (
                "an overbrace",
                (27, 0),
                (24, 1),
                "",
                "",
                |w| vec![w.line(&[w.var("a"), w.ch(6, "+"), w.var("b")]), w.line(&[w.var("n")])],
                r"\overbrace{a+b}^{n}",
            ),
            (
                "an over arrow",
                (47, 1),
                (31, 2),
                "",
                "",
                |w| vec![w.line(&[w.var("AB")])],
                r"\overrightarrow{AB}",
            ),
        ];
        for (name, v3, v5, before, after, parts, expected) in rows {
            for (version, (selector, variation)) in [(Version::Three, v3), (Version::Five, v5)] {
                let w = W(version);
                let template = w.tmpl(selector, variation, &parts(&w));
                let items = [w.var(before), template, w.var(after)];
                assert_eq!(
                    tex(&w.equation(&items)).as_deref(),
                    Some(expected),
                    "{name}: {version:?}"
                );
            }
        }
        let w = W(Version::Five);
        let only_v5 = [
            (8, 3, r"\left\llbracket x\right\rrbracket"),
            (31, 0x9, r"\overleftharpoon{x}"),
            (31, 0xA, r"\overrightharpoon{x}"),
            (32, 0, r"\widetilde{x}"),
            (33, 0, r"\widehat{x}"),
            (37, 0x1E, r"\boxed{x}"),
            (37, 0x1F, r"\boxed{x}"),
        ];
        for (selector, variation, expected) in only_v5 {
            let equation = w.equation(&[w.tmpl(selector, variation, &x(&w))]);
            assert_eq!(tex(&equation).as_deref(), Some(expected), "{selector}/{variation}");
        }
    }

    #[test]
    fn each_big_operator_reads_the_same_in_both_versions() {
        type Operator = ((u8, u8), (u8, u16), char, &'static [&'static str], &'static str);
        let rows: [Operator; 28] = [
            ((21, 0), (15, 0x01), '\u{222B}', &[], r"\int{f}"),
            ((42, 1), (15, 0x19), '\u{2232}', &["C"], r"\operatorname*{∲}_{C}{f}"),
            ((42, 1), (15, 0x1D), '\u{2233}', &["C"], r"\operatorname*{∳}_{C}{f}"),
            ((21, 2), (15, 0x31), '\u{222B}', &["a", "b"], r"\int_{a}^{b}{f}"),
            ((21, 2), (15, 0x131), '\u{222B}', &["a", "b"], r"\int_{a}^{b}{f}"),
            ((21, 4), (15, 0x15), '\u{222E}', &["C"], r"\oint_{C}{f}"),
            ((22, 1), (15, 0x12), '\u{222C}', &["D"], r"\iint_{D}{f}"),
            ((22, 3), (15, 0x16), '\u{222F}', &["S"], r"\oiint_{S}{f}"),
            ((23, 1), (15, 0x13), '\u{222D}', &["V"], r"\iiint_{V}{f}"),
            ((23, 3), (15, 0x17), '\u{2230}', &["V"], r"\oiiint_{V}{f}"),
            ((24, 0), (15, 0x71), '\u{222B}', &["a", "b"], r"\int_{a}^{b}{f}"),
            ((24, 2), (15, 0x55), '\u{222E}', &["C"], r"\oint_{C}{f}"),
            ((25, 1), (15, 0x52), '\u{222C}', &["D"], r"\iint_{D}{f}"),
            ((25, 0), (15, 0x56), '\u{222F}', &["S"], r"\oiint_{S}{f}"),
            ((26, 1), (15, 0x53), '\u{222D}', &["V"], r"\iiint_{V}{f}"),
            ((26, 0), (15, 0x57), '\u{2230}', &["V"], r"\oiiint_{V}{f}"),
            ((29, 1), (16, 0x70), '\u{2211}', &["i", "n"], r"\sum_{i}^{n}{f}"),
            ((30, 1), (16, 0x30), '\u{2211}', &["i", "n"], r"\sum_{i}^{n}{f}"),
            ((31, 1), (17, 0x70), '\u{220F}', &["i", "n"], r"\prod_{i}^{n}{f}"),
            ((32, 1), (17, 0x30), '\u{220F}', &["i", "n"], r"\prod_{i}^{n}{f}"),
            ((33, 1), (18, 0x70), '\u{2210}', &["i", "n"], r"\coprod_{i}^{n}{f}"),
            ((34, 1), (18, 0x30), '\u{2210}', &["i", "n"], r"\coprod_{i}^{n}{f}"),
            ((35, 1), (19, 0x70), '\u{22C3}', &["i", "n"], r"\bigcup_{i}^{n}{f}"),
            ((36, 1), (19, 0x30), '\u{22C3}', &["i", "n"], r"\bigcup_{i}^{n}{f}"),
            ((37, 1), (20, 0x70), '\u{22C2}', &["i", "n"], r"\bigcap_{i}^{n}{f}"),
            ((38, 1), (20, 0x30), '\u{22C2}', &["i", "n"], r"\bigcap_{i}^{n}{f}"),
            ((42, 2), (21, 0x30), '\u{2A0C}', &["a", "b"], r"\iiiint_{a}^{b}{f}"),
            ((43, 2), (22, 0x70), '\u{2A01}', &["a", "b"], r"\bigoplus_{a}^{b}{f}"),
        ];
        for (v3, v5, operator, limits, expected) in rows {
            for (version, (selector, variation)) in
                [(Version::Three, (v3.0, u16::from(v3.1))), (Version::Five, v5)]
            {
                let w = W(version);
                let mut slots: Vec<Vec<u8>> =
                    limits.iter().map(|limit| w.line(&[w.var(limit)])).collect();
                slots.resize_with(2, || w.null());
                let parts = [vec![w.line(&[w.var("f")])], slots, vec![w.glyph(0, 6, operator)]];
                assert_eq!(
                    tex(&w.equation(&[w.tmpl(selector, variation, &parts.concat())])).as_deref(),
                    Some(expected),
                    "{selector}/{variation}: {version:?}"
                );
            }
        }
    }

    type Record = Box<dyn Fn(&W) -> Vec<u8>>;
    type Row =
        (&'static str, (u8, u16), (u8, u16), &'static str, &'static str, Parts, &'static str);

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of records in both versions")]
    fn records_read_alike_in_both_versions() {
        let pile = |w: &W| w.pile(&[w.line(&[w.var("a")]), w.line(&[w.var("b")])]);
        let matrix = |w: &W| {
            let cells: Vec<Vec<u8>> =
                "abcd".chars().map(|c| w.line(&[w.var(&c.to_string())])).collect();
            w.matrix(2, 2, &cells)
        };
        let marked = |code: u8| move |w: &W| w.embellished(3, 'x', &[code]);
        let rows: Vec<(&str, Record, &str)> = vec![
            ("a pile", Box::new(pile), r"\begin{gathered} a \\ b \end{gathered}"),
            ("a matrix", Box::new(matrix), r"\begin{matrix} a & b \\ c & d \end{matrix}"),
            ("a hat", Box::new(marked(9)), r"\hat{x}"),
            ("a bar", Box::new(marked(17)), r"\bar{x}"),
            ("a dot", Box::new(marked(2)), r"\dot{x}"),
            ("a prime", Box::new(marked(5)), "x'"),
            ("text", Box::new(|w: &W| w.ch(1, "if x")), r"\text{if x}"),
            ("a function", Box::new(|w: &W| [w.ch(2, "sin"), w.var("x")].concat()), r"\sin x"),
            ("a vector", Box::new(|w: &W| w.ch(7, "v")), r"\mathbf{v}"),
            (
                "Greek letters",
                Box::new(|w: &W| [w.ch(4, "\u{3B1}"), w.ch(5, "\u{3A9}")].concat()),
                r"\alpha\Omega",
            ),
            ("numbers", Box::new(|w: &W| w.ch(8, "12")), "12"),
            (
                "MT Extra characters",
                Box::new(|w: &W| [w.var("a"), w.ch(11, "\u{E421}\u{EB04}"), w.var("b")].concat()),
                r"a\ge\:b",
            ),
            (
                "nudges",
                Box::new(|w: &W| {
                    [w.nudged(&[130, 126]), w.nudged(&[128, 128, 1, 0, 2, 0])].concat()
                }),
                "xx",
            ),
            (
                "sizes",
                Box::new(|w: &W| {
                    [
                        w.var("a"),
                        vec![9, 101, 1, 0, 9, 100, 1, 2, 0, 9, 1, 130, 10, 11, 13],
                        w.var("b"),
                    ]
                    .concat()
                }),
                "ab",
            ),
        ];
        for (name, record, expected) in rows {
            for version in BOTH {
                let w = W(version);
                assert_eq!(
                    tex(&w.equation(&[record(&w)])).as_deref(),
                    Some(expected),
                    "{name}: {version:?}"
                );
            }
        }
        let five = W(Version::Five);
        let only_v5 = [
            (24, r"\ddddot{x}"),
            (29, r"\underline{x}"),
            (30, r"\utilde{x}"),
            (33, r"\underrightarrow{x}"),
            (34, r"\underleftarrow{x}"),
            (35, r"\underleftrightarrow{x}"),
        ];
        for (code, expected) in only_v5 {
            let equation = five.equation(&[marked(code)(&five)]);
            assert_eq!(tex(&equation).as_deref(), Some(expected), "{code}");
        }
        let three = W(Version::Three);
        assert_eq!(tex(&three.equation(&[marked(24)(&three)])), None);
    }

    #[test]
    fn definitions_are_skipped_by_their_layout() {
        let five = W(Version::Five);
        let definitions = [
            vec![19],
            b"WinAllBasicCodePages\0".to_vec(),
            vec![17, 5],
            b"Times New Roman\0".to_vec(),
            vec![8, 1, 2, 15, 1, 16, 0, 0, 0, 0, 0, 0, 0],
            vec![16, 0x5, 0, 0, 0, 0, 0, 0, 0, 0],
            b"Ink\0".to_vec(),
            vec![18, 0, 2, 0x21, 0x2F, 0x4F, 1, 0x0F, 2, 1, 0, 0],
            vec![7, 1, 0, 32, 0],
            vec![100, 3, 9, 9, 9],
        ]
        .concat();
        let equation =
            [five.header(false), definitions, vec![10], five.line(&[five.var("x")]), vec![0]]
                .concat();
        assert_eq!(tex(&equation).as_deref(), Some("x"));
        let three = W(Version::Three);
        let font = [vec![8, 3, 1], b"Symbol\0".to_vec()].concat();
        let equation = [three.header(false), font, three.line(&[three.var("x")]), vec![0]].concat();
        assert_eq!(tex(&equation).as_deref(), Some("x"));
    }

    #[test]
    fn only_a_v5_equation_can_say_it_is_inline() {
        let inline =
            |bytes: &[u8]| mtef_to_tex(&ParseCtx::default(), bytes).unwrap().unwrap().inline;
        let five = W(Version::Five);
        let body = [vec![10], five.line(&[five.var("x")]), vec![0]].concat();
        assert!(inline(&[five.header(true), body.clone()].concat()));
        assert!(!inline(&[five.header(false), body].concat()));
        let three = W(Version::Three);
        assert!(!inline(&three.equation(&[three.var("x")])));
    }

    #[test]
    fn what_this_reader_cannot_read_falls_back() {
        for version in BOTH {
            let w = W(version);
            let good = w.equation(&[w.var("x")]);
            let unknown_selector = if version == Version::Three { 45 } else { 30 };
            let fraction = if version == Version::Three { 14 } else { 11 };
            let slot = |text: &str| w.line(&[w.var(text)]);
            let cases = [
                (
                    "a template outside a slot",
                    w.equation(&[w.tmpl(
                        fraction,
                        0,
                        &[w.tmpl(fraction, 0, &[slot("a")]), slot("b")],
                    )]),
                ),
                (
                    "a slot too many",
                    w.equation(&[w.tmpl(fraction, 0, &[slot("a"), slot("b"), slot("c")])]),
                ),
                ("a truncated record", good[..good.len() - 2].to_vec()),
                ("a missing END", good[..good.len() - 1].to_vec()),
                ("bytes past the last END", [good.clone(), vec![0, 7]].concat()),
                ("an unknown selector", w.equation(&[w.tmpl(unknown_selector, 0, &[w.line(&[])])])),
                (
                    "an embellishment outside a character",
                    w.equation(&[[w.tag(6, 0), vec![9]].concat()]),
                ),
                ("an unknown embellishment", w.equation(&[w.embellished(3, 'x', &[7])])),
                ("an empty equation", w.equation(&[])),
            ];
            for (name, bytes) in cases {
                assert_eq!(tex(&bytes), None, "{name}: {version:?}");
            }
            assert_eq!(tex(&[good, vec![0, 0]].concat()).as_deref(), Some("x"), "{version:?}");
        }
        let five = W(Version::Five);
        let mut four = five.equation(&[five.var("x")]);
        four[0] = 4;
        assert_eq!(tex(&four), None, "an unknown version");
        assert_eq!(tex(&five.equation(&[vec![20]])), None, "an unknown record");
    }

    #[test]
    fn a_glyph_its_template_does_not_draw_falls_back() {
        for version in BOTH {
            let w = W(version);
            let (fraction, paren, left, sum, int, iint, brace) = match version {
                Version::Three => (14, (1, 0), (1, 1), (29, 1), (21, 1), (22, 1), (27, 0)),
                Version::Five => (11, (1, 3), (1, 1), (16, 0x10), (15, 0x11), (15, 0x12), (24, 1)),
            };
            let slot = |text: &str| w.line(&[w.var(text)]);
            let drawn = |(selector, variation): (u8, u16), slots: &[Vec<u8>], glyphs: &str| {
                w.equation(&[w.tmpl(selector, variation, &[slots.concat(), w.ch(6, glyphs)])])
            };
            let operator = [slot("f"), slot("i"), w.null()];
            let cases = [
                ("beside a fraction", drawn((fraction, 0), &[slot("a"), slot("b")], "z")),
                ("a third beside a fence", drawn(paren, &[slot("x")], "()z")),
                ("a second beside a one-sided fence", drawn(left, &[slot("x")], "(z")),
                ("a second beside a sum", drawn(sum, &operator, "\u{2211}z")),
                ("a second beside a single integral", drawn(int, &operator, "\u{222B}z")),
                ("a third beside a double integral", drawn(iint, &operator, "\u{222B}\u{222B}z")),
                ("a second beside a brace", drawn(brace, &[slot("a"), slot("n")], "\u{23DE}z")),
            ];
            for (name, bytes) in cases {
                assert_eq!(tex(&bytes), None, "{name}: {version:?}");
            }
        }
    }

    #[test]
    fn an_optional_argument_holds_its_brackets_and_a_row_its_first_bracket() {
        for version in BOTH {
            let w = W(version);
            let root = if version == Version::Three { 13 } else { 10 };
            let index = w.line(&[w.var("n"), w.ch(6, "["), w.var("m"), w.ch(6, "]")]);
            let indexed = w.tmpl(root, 1, &[w.line(&[w.var("x")]), index]);
            let rows = [w.line(&[w.var("a")]), w.line(&[w.ch(6, "["), w.var("b"), w.ch(6, "]")])];
            let starred = [w.line(&[w.var("a")]), w.line(&[w.ch(6, "*"), w.var("b")])];
            let bracketed = || w.line(&[w.ch(6, "["), w.var("b"), w.ch(6, "]")]);
            let first = [bracketed(), w.line(&[w.var("c")])];
            let first_starred = [w.line(&[w.ch(6, "*"), w.var("b")]), w.line(&[w.var("c")])];
            let spaced =
                [w.line(&[w.ch(6, " ["), w.var("b"), w.ch(6, "]")]), w.line(&[w.var("c")])];
            let cases = [
                (indexed, r"\sqrt[{n[m]}]{x}"),
                (w.pile(&rows), r"\begin{gathered} a \\ {}[b] \end{gathered}"),
                (w.pile(&starred), r"\begin{gathered} a \\ {}*b \end{gathered}"),
                (w.pile(&first), r"\begin{gathered} {}[b] \\ c \end{gathered}"),
                (w.pile(&first_starred), r"\begin{gathered} {}*b \\ c \end{gathered}"),
                (w.pile(&spaced), r"\begin{gathered} {} [b] \\ c \end{gathered}"),
                (w.matrix(1, 2, &first), r"\begin{matrix} {}[b] & c \end{matrix}"),
                (w.matrix(2, 1, &first), r"\begin{matrix} {}[b] \\ c \end{matrix}"),
            ];
            for (items, expected) in cases {
                assert_eq!(tex(&w.equation(&[items])).as_deref(), Some(expected), "{version:?}");
            }
        }
    }

    #[test]
    fn empty_nodes_between_scripts_cost_linear_work_and_change_nothing() {
        for version in BOTH {
            let w = W(version);
            let (fraction, (sub, sup)) = match version {
                Version::Three => (14, ((15, 1), (15, 0))),
                Version::Five => (11, ((27, 0), (28, 0))),
            };
            let empty = || w.tmpl(sub.0, sub.1, &[w.null(), w.null()]);
            let alternating = |n: usize| {
                let over =
                    w.tmpl(fraction, 0, &[w.line(&[w.var("a")]), w.line(&[w.var(&"b".repeat(n))])]);
                let pairs = (0..n).flat_map(|_| [w.null(), empty()]);
                w.equation(&std::iter::once(over).chain(pairs).collect::<Vec<_>>())
            };
            let spent = |n: usize| {
                let ctx = ParseCtx::default();
                assert!(mtef_to_tex(&ctx, &alternating(n)).unwrap().is_some(), "{n}: {version:?}");
                ctx.fuel()
            };
            let (one, two, four) = (spent(1000), spent(2000), spent(4000));
            assert_eq!(four - two, 2 * (two - one), "{version:?}");
            let i = || w.line(&[w.var("i")]);
            let squared_two = || w.line(&[w.ch(8, "2")]);
            let squared = w.tmpl(sup.0, sup.1, &[w.null(), squared_two()]);
            let indexed = w.tmpl(sub.0, sub.1, &[i(), w.null()]);
            let rows = [
                (vec![w.var("x"), squared.clone(), empty()], "x^{2}"),
                (vec![w.var("x"), indexed, w.null(), squared], "x_{i}{}^{2}"),
            ];
            for (items, expected) in rows {
                assert_eq!(tex(&w.equation(&items)).as_deref(), Some(expected), "{version:?}");
            }
        }
    }

    #[test]
    fn raised_characters_in_a_row_group_their_base_once_and_cost_linear_work() {
        for version in BOTH {
            let w = W(version);
            let degrees = |n: usize| w.equation(&[w.var("x"), w.ch(6, &"\u{B0}".repeat(n))]);
            let spent = |n: usize| {
                let ctx = ParseCtx::default();
                assert!(mtef_to_tex(&ctx, &degrees(n)).unwrap().is_some(), "{n}: {version:?}");
                ctx.fuel()
            };
            let step = |n: usize| spent(n + 1) - spent(n);
            assert_eq!(step(1), step(2) + "x^{\\circ}".len() as u64, "{version:?}");
            let (one, two, four) = (spent(1000), spent(2000), spent(4000));
            assert_eq!(four - two, 2 * (two - one), "{version:?}");
            let limits = Limits { max_work: 200_000, ..Limits::default() };
            let read = mtef_to_tex(&ParseCtx::new(&limits), &degrees(1000));
            assert_matches!(read, Ok(Some(_)), "{version:?}");
            let three = r"{x^{\circ}}^{\circ}{}^{\circ}";
            assert_eq!(tex(&degrees(3)).as_deref(), Some(three), "{version:?}");
        }
    }

    #[test]
    fn a_raised_character_after_a_superscript_groups_its_base() {
        for version in BOTH {
            let w = W(version);
            let sup = if version == Version::Three { (15, 0) } else { (28, 0) };
            let squared =
                || [w.var("x"), w.tmpl(sup.0, sup.1, &[w.null(), w.line(&[w.ch(8, "2")])])];
            let after = |items: &[Vec<u8>]| [squared().as_slice(), items].concat();
            let rows = [
                (after(&[w.ch(6, "\u{2032}")]), "{x^{2}}'"),
                (after(&[w.ch(6, "\u{2033}")]), "{x^{2}}''"),
                (after(&[w.ch(6, "\u{2034}")]), "{x^{2}}'''"),
                (after(&[w.ch(6, "\u{2057}")]), "{x^{2}}''''"),
                (after(&[w.ch(6, "'")]), "{x^{2}}'"),
                (after(&[w.ch(6, "\u{B0}")]), r"{x^{2}}^{\circ}"),
                (after(&[w.ch(2, "\u{2032}")]), "{x^{2}}'"),
                (after(&[w.ch(6, " "), w.ch(6, "\u{2032}")]), "{x^{2} }'"),
                (after(&[w.ch(2, " \u{B0}")]), r"{x^{2}} ^{\circ}"),
                (after(&[w.ch(2, " \u{2032}")]), "{x^{2}} '"),
                (after(&[w.ch(2, "\u{A0}\u{2032}")]), r"x^{2}\ '"),
                (vec![w.embellished(6, '\u{B0}', &[5])], r"{^{\circ}}'"),
                (vec![w.embellished(3, 'x', &[5]), w.ch(6, "\u{2032}")], "x''"),
                (vec![w.var("x"), w.ch(6, "\u{2032}\u{B0}")], r"x'^{\circ}"),
            ];
            for (items, expected) in rows {
                assert_eq!(tex(&w.equation(&items)).as_deref(), Some(expected), "{version:?}");
            }
        }
    }

    #[test]
    fn a_base_that_already_carries_a_script_is_grouped_before_another() {
        for version in BOTH {
            let w = W(version);
            let ((both, sub, sup), limit) = match version {
                Version::Three => (((15, 2), (15, 1), (15, 0)), (39, 1)),
                Version::Five => (((29, 0), (27, 0), (28, 0)), (23, 0x10)),
            };
            let scripts = |(selector, variation): (u8, u16), lower: Vec<u8>, upper: Vec<u8>| {
                w.tmpl(selector, variation, &[lower, upper])
            };
            let i = || w.line(&[w.var("i")]);
            let two = || w.line(&[w.ch(8, "2")]);
            let primed = || w.embellished(3, 'x', &[5]);
            let rows = [
                (vec![primed(), scripts(both, i(), two())], "{x'}_{i}^{2}"),
                (vec![primed(), scripts(sub, i(), w.null())], "{x'}_{i}"),
                (
                    vec![w.var("x"), w.ch(6, "\u{B0}"), scripts(sup, w.null(), two())],
                    r"{x^{\circ}}^{2}",
                ),
                (
                    vec![w.tmpl(
                        limit.0,
                        limit.1,
                        &[
                            w.line(&[w.var("a"), scripts(sub, i(), w.null())]),
                            w.line(&[w.var("n")]),
                        ],
                    )],
                    "{a_{i}}_{n}",
                ),
                (
                    vec![w.tmpl(
                        limit.0,
                        limit.1,
                        &[
                            w.line(&[w.var("a"), scripts(sub, i(), w.null()), w.ch(6, " ")]),
                            w.line(&[w.var("n")]),
                        ],
                    )],
                    "{a_{i} }_{n}",
                ),
                (
                    vec![w.tmpl(
                        limit.0,
                        limit.1,
                        &[
                            w.line(&[w.var("a"), scripts(sub, i(), w.null()), w.ch(6, "\u{A0}")]),
                            w.line(&[w.var("n")]),
                        ],
                    )],
                    r"a_{i}\ _{n}",
                ),
                (vec![primed(), w.ch(6, " "), scripts(both, i(), two())], "{x' }_{i}^{2}"),
                (vec![primed(), w.ch(6, "\u{A0}"), scripts(sup, w.null(), two())], r"x'\ ^{2}"),
                (vec![w.var("x"), scripts(both, i(), two())], "x_{i}^{2}"),
            ];
            for (items, expected) in rows {
                assert_eq!(tex(&w.equation(&items)).as_deref(), Some(expected), "{version:?}");
            }
        }
    }

    #[test]
    fn a_v3_character_in_a_symbol_font_reads_through_that_font() {
        let w = W(Version::Three);
        let read = |fonts: &[Vec<u8>], glyph: Vec<u8>| {
            let line = w.line(&[glyph, w.var("y")]);
            let equation = [w.header(false), fonts.concat(), vec![10], line, vec![0]].concat();
            let read = mtef_to_tex(&ParseCtx::default(), &equation).unwrap().unwrap();
            (read.tex, read.unmapped)
        };
        let symbol = || w.font(3, "Symbol");
        let rows = [
            (vec![symbol()], w.in_font(3, 0x61), r"\alpha y", 0),
            (vec![w.font(3, "symbol")], w.in_font(3, 0x61), r"\alpha y", 0),
            (vec![symbol()], w.in_font(3, 0x3C0), r"\pi y", 0),
            (vec![symbol()], w.in_font(3, 0x60), "y", 1),
            (vec![w.font(2, "Arial")], w.in_font(2, 0x61), "ay", 0),
            (vec![symbol(), w.font(3, "Arial")], w.in_font(3, 0x61), "ay", 0),
            (vec![symbol()], w.ch(6, "a"), "ay", 0),
        ];
        for (fonts, glyph, expected, unmapped) in rows {
            assert_eq!(read(&fonts, glyph), (expected.to_owned(), unmapped), "{expected}");
        }
    }

    #[test]
    fn a_trailing_control_space_keeps_its_space() {
        for version in BOTH {
            let w = W(version);
            for (face, space) in [(6, "\u{A0}"), (11, "\u{EB08}")] {
                let line = w.line(&[w.var("x"), w.ch(face, space)]);
                for inline in [false, true] {
                    let bytes = [w.header(inline), vec![10], line.clone(), vec![0]].concat();
                    let read = mtef_to_tex(&ParseCtx::default(), &bytes).unwrap().unwrap();
                    let expected = ("x\\ ", inline && version == Version::Five);
                    assert_eq!(
                        (read.tex.as_str(), read.inline),
                        expected,
                        "{space:?}: {version:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_function_name_ends_at_its_first_non_letter() {
        for version in BOTH {
            let w = W(version);
            let sub = if version == Version::Three { (15, 1) } else { (27, 0) };
            let subscript = w.tmpl(sub.0, sub.1, &[w.line(&[w.var("i")]), w.null()]);
            let rows = [
                (vec![w.ch(2, "min,"), w.var("x")], r"\min,x"),
                (vec![w.ch(2, "sin("), w.var("x"), w.ch(2, ")")], r"\sin(x)"),
                (vec![w.ch(2, "abc,"), w.var("x")], r"\operatorname{abc},x"),
                (vec![w.ch(2, " cos"), w.var("x")], r"\cos x"),
                (vec![w.ch(2, "min"), subscript], r"\min_{i}"),
                (vec![w.var("a"), w.ch(2, ","), w.var("b")], "a,b"),
            ];
            for (items, expected) in rows {
                assert_eq!(tex(&w.equation(&items)).as_deref(), Some(expected), "{version:?}");
            }
        }
    }

    #[test]
    fn a_v5_function_start_begins_another_function() {
        let w = W(Version::Five);
        let function = |name: &str| -> Vec<u8> {
            let first = |i: usize| if i == 0 { 0x2 } else { 0 };
            name.chars().enumerate().flat_map(|(i, c)| w.glyph(first(i), 2, c)).collect()
        };
        let bytes = w.equation(&[function("sin"), function("cos"), w.var("x")]);
        assert_eq!(tex(&bytes).as_deref(), Some(r"\sin\cos x"));
    }

    #[test]
    fn a_matrix_with_a_drawn_partition_line_falls_back() {
        for version in BOTH {
            let w = W(version);
            let cells: Vec<Vec<u8>> =
                "abc".chars().map(|c| w.line(&[w.var(&c.to_string())])).collect();
            let read = |row: u8, col: u8| tex(&w.equation(&[w.parted(1, 3, &[row, col], &cells)]));
            let plain = Some(r"\begin{matrix} a & b & c \end{matrix}".to_owned());
            assert_eq!((read(0, 0), read(0xF0, 0)), (plain.clone(), plain), "{version:?}");
            for (name, row, col) in
                [("dividers", 0, 0x14), ("a dashed rule", 0x8, 0), ("a frame", 0x5, 0x41)]
            {
                assert_eq!(read(row, col), None, "{name}: {version:?}");
            }
        }
    }

    #[test]
    fn a_box_missing_a_side_falls_back() {
        let w = W(Version::Five);
        for (variation, name) in
            [(0, "no side"), (0x08, "top"), (0x10, "bottom"), (0x1C, "no left")]
        {
            let x = [w.line(&[w.var("x")])];
            assert_eq!(tex(&w.equation(&[w.tmpl(37, variation, &x)])), None, "{name}");
        }
    }

    #[test]
    fn a_directed_contour_around_several_signs_falls_back() {
        let five = W(Version::Five);
        for (variation, name) in [(0x0A, "clockwise double"), (0x0F, "counter-clockwise triple")] {
            let parts = [five.line(&[five.var("f")]), five.null(), five.null()];
            assert_eq!(tex(&five.equation(&[five.tmpl(15, variation, &parts)])), None, "{name}");
        }
    }

    #[test]
    fn an_embellished_character_leaves_the_records_after_it_in_place() {
        for version in BOTH {
            let w = W(version);
            let fraction = if version == Version::Three { 14 } else { 11 };
            let numerator = w.line(&[w.embellished(3, 'x', &[9]), w.ch(6, "+"), w.var("y")]);
            let inner = w.tmpl(fraction, 0, &[numerator, w.line(&[w.var("b")])]);
            let outer = w.tmpl(fraction, 0, &[w.line(&[inner]), w.line(&[w.var("c")])]);
            assert_eq!(
                tex(&w.equation(&[outer, w.var("z")])).as_deref(),
                Some(r"\frac{\frac{\hat{x}+y}{b}}{c}z"),
                "{version:?}"
            );
        }
    }

    #[test]
    fn one_v3_embellishment_record_lists_codes_up_to_a_zero() {
        let w = W(Version::Three);
        let marked = [w.glyph(0x2, 3, 'x'), w.embell(&[9, 5]), vec![0]].concat();
        assert_eq!(tex(&w.equation(&[marked, w.var("y")])).as_deref(), Some(r"\hat{x}'y"));
    }

    #[test]
    fn a_template_draws_its_own_glyphs_unless_one_names_its_operator() {
        for version in BOTH {
            let w = W(version);
            let (bars, left, oiint, named_by_glyph) = match version {
                Version::Three => ((4, 0), (1, 1), (22, 3), (42, 1)),
                Version::Five => ((4, 3), (1, 1), (15, 0x16), (21, 0x10)),
            };
            let drawn = |(selector, variation): (u8, u16), slots: &[Vec<u8>], glyphs: &str| {
                w.equation(&[w.tmpl(selector, variation, &[slots.concat(), w.ch(11, glyphs)])])
            };
            let x = [w.line(&[w.var("x")])];
            let integrand = [w.line(&[w.var("f")]), w.line(&[w.var("S")]), w.null()];
            let cases = [
                (drawn(bars, &x, "\u{EC07}\u{EC08}"), r"\left|x\right|", 0),
                (drawn(left, &x, "\u{E999}"), r"\left(x\right.", 0),
                (drawn(oiint, &integrand, "\u{EE12}\u{222B}\u{222B}"), r"\oiint_{S}{f}", 0),
                (drawn(named_by_glyph, &integrand, "\u{E999}"), r"\operatorname*{}_{S}{f}", 1),
            ];
            for (bytes, expected, unmapped) in cases {
                let read = mtef_to_tex(&ParseCtx::default(), &bytes).unwrap();
                let read = read.map(|equation| (equation.tex, equation.unmapped));
                assert_eq!(read, Some((expected.to_owned(), unmapped)), "{expected}: {version:?}");
            }
        }
    }

    #[test]
    fn every_record_counts_against_max_records() {
        for version in BOTH {
            let w = W(version);
            let fraction = if version == Version::Three { 14 } else { 11 };
            let slots = [w.line(&[w.embellished(3, 'x', &[9])]), w.line(&[w.var("b")])];
            let cases = [
                ("a line", w.equation(&[w.var("xyz")]), 7),
                ("nested records", w.equation(&[w.tmpl(fraction, 0, &slots)]), 14),
            ];
            for (name, bytes, records) in cases {
                let read = |max_records| {
                    let limits = Limits { max_records, ..Limits::default() };
                    mtef_to_tex(&ParseCtx::new(&limits), &bytes).map(|read| read.is_some())
                };
                let past =
                    Err(ConvertError::ResourceLimit { limit: Limit::Records, max: records - 1 });
                assert_eq!(
                    (read(records), read(records - 1)),
                    (Ok(true), past),
                    "{name}: {version:?}"
                );
            }
        }
    }

    #[test]
    fn an_unmapped_character_is_counted_and_the_rest_reads() {
        for version in BOTH {
            let w = W(version);
            let bytes = w.equation(&[w.var("a"), w.ch(11, "\u{E999}\u{9F}"), w.var("b")]);
            let equation = mtef_to_tex(&ParseCtx::default(), &bytes).unwrap().unwrap();
            assert_eq!((equation.tex.as_str(), equation.unmapped), ("ab", 2), "{version:?}");
        }
    }

    fn nested(w: &W, fractions: usize, inner: Vec<u8>) -> Vec<u8> {
        (0..fractions).fold(inner, |inner, _| w.tmpl(11, 0, &[w.line(&[inner]), w.null()]))
    }

    #[test]
    fn nesting_past_the_nesting_depth_fails() {
        let limits = Limits { max_nesting_depth: 8, ..Limits::default() };
        let w = W(Version::Five);
        let read = |fractions| {
            mtef_to_tex(&ParseCtx::new(&limits), &w.equation(&[nested(&w, fractions, w.var("x"))]))
        };
        assert_matches!(read(3), Ok(Some(_)));
        assert_matches!(
            read(4),
            Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 8 })
        );
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the test reads on a small stack of its own, where unbounded recursion would overflow"
    )]
    fn records_nested_far_past_the_nesting_depth_are_refused_on_a_small_stack() {
        let w = W(Version::Five);
        let lines = 50_000;
        let nested = [w.tag(1, 0).repeat(lines), w.var("x"), vec![0; lines]].concat();
        let bytes = w.equation(&[nested]);
        let limits = Limits { max_record_depth: 100_000, ..Limits::default() };
        let read = std::thread::scope(|scope| {
            let small = std::thread::Builder::new().stack_size(2 * 1024 * 1024);
            let read = small.spawn_scoped(scope, || {
                mtef_to_tex(&ParseCtx::new(&limits), &bytes).map(|read| read.is_some())
            });
            read.unwrap().join().unwrap()
        });
        let max = limits.max_nesting_depth;
        assert_eq!(read, Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max }));
    }

    #[test]
    fn records_and_skipped_bytes_are_charged() {
        let limits = Limits { max_work: 1000, ..Limits::default() };
        let w = W(Version::Five);
        let sizes = w.equation(&[vec![10; 1 << 20]]);
        let future: Vec<u8> =
            (0..16).flat_map(|_| [vec![100, 255, 0xE8, 0xFD], vec![0; 65000]].concat()).collect();
        let tail = [w.equation(&[w.var("x")]), vec![0; 1 << 20]].concat();
        let cases =
            [("records", sizes), ("skipped bytes", w.equation(&[future])), ("a tail", tail)];
        for (name, bytes) in cases {
            for _ in 0..2 {
                let read = mtef_to_tex(&ParseCtx::new(&limits), &bytes);
                assert_matches!(
                    read,
                    Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 1000 }),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn written_tex_is_charged_as_it_grows() {
        let limits = Limits { max_work: 200_000, ..Limits::default() };
        let w = W(Version::Five);
        let padded: Vec<u8> = (0..1000).flat_map(|_| w.matrix(1, 255, &[w.null()])).collect();
        let marked = w.embellished(3, 'x', &[2; 4000]);
        let deep = nested(&w, 25, w.var(&"x".repeat(10_000)));
        let work = Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 200_000 });
        for (name, items) in [("padded matrices", padded), ("marks", marked), ("copies", deep)] {
            let bytes = w.equation(&[items]);
            let read = mtef_to_tex(&ParseCtx::new(&limits), &bytes).map(|_| ());
            assert_eq!(read, work, "{name}");
        }
    }

    #[test]
    fn a_padded_matrix_is_charged_before_its_tex_is_written() {
        let limits = Limits { max_work: 200_000, ..Limits::default() };
        let w = W(Version::Five);
        let padded: Vec<u8> = (0..1000).flat_map(|_| w.matrix(1, 255, &[w.null()])).collect();
        let bytes = w.equation(&[padded]);
        let ctx = ParseCtx::new(&limits);
        let mut decoder = Decoder {
            ctx: &ctx,
            bytes: ByteReader::new(&bytes[1..]),
            version: Version::Five,
            unmapped: 0,
            records: 0,
            fonts: [None; 256],
        };
        assert!(matches!(
            decoder.equation(),
            Err(Stop::Fatal(ConvertError::ResourceLimit { limit: Limit::Work, max: 200_000 }))
        ));
    }

    #[test]
    fn a_cancelled_parse_stops_inside_an_equation() {
        let token = crate::CancelToken::new();
        token.cancel();
        let w = W(Version::Three);
        let ctx = ParseCtx::default().with_cancel(Some(&token));
        assert_matches!(
            mtef_to_tex(&ctx, &w.equation(&[w.var("x")])),
            Err(ConvertError::Cancelled)
        );
    }
}
