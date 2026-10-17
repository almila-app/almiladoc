//! RTF prelude tables parsed into typed definitions: the font table (with
//! per-font charsets), the style sheet, and the list/list-override tables.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::formats::rtf::lexer::{Lexer, Token};
use crate::shared::blockstyle::{self, BlockStyle};
use crate::shared::delta::StyleDelta;
use crate::shared::list::MarkerKind;
use crate::shared::numbering::{NumberPattern, NumberText};
use std::collections::HashMap;
use std::ops::{BitOrAssign, Range};

pub const LIST_LEVELS: usize = 9;

/// A text encoding, and whether the document left it to be guessed.
#[derive(Debug, Clone, Copy)]
pub struct Codepage {
    pub encoding: &'static encoding_rs::Encoding,
    pub assumed: bool,
}

impl Codepage {
    /// An encoding the document declares.
    pub const fn declared(encoding: &'static encoding_rs::Encoding) -> Self {
        Codepage { encoding, assumed: false }
    }

    /// Decode `bytes`, and what a reader loses in the result.
    pub fn decode(self, bytes: &[u8]) -> (String, TextLoss) {
        let (text, _, invalid) = self.encoding.decode(bytes);
        let assumed = self.assumed && !bytes.is_ascii();
        (text.into_owned(), TextLoss { invalid, assumed })
    }
}

/// What decoded text loses: bytes that did not decode, and non-ASCII bytes
/// read in a guessed code page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextLoss {
    pub invalid: bool,
    pub assumed: bool,
}

impl BitOrAssign for TextLoss {
    fn bitor_assign(&mut self, other: TextLoss) {
        self.invalid |= other.invalid;
        self.assumed |= other.assumed;
    }
}

#[derive(Debug, Clone)]
pub struct ListLevelDef {
    pub marker: Option<MarkerKind>,
    pub start: u64,
    /// `\leveltext` with `\levelnumbers` placeholders and `\levellegal`.
    pub pattern: NumberPattern,
    /// What the pattern's literal text lost in decoding.
    pub loss: TextLoss,
}

impl Default for ListLevelDef {
    fn default() -> Self {
        ListLevelDef {
            marker: Some(MarkerKind::Bullet),
            start: 1,
            pattern: Default::default(),
            loss: TextLoss::default(),
        }
    }
}

/// Build a number pattern from a `\leveltext` payload (first byte = length,
/// then the characters) and the 1-based placeholder positions listed in
/// `\levelnumbers` (each placeholder byte is a zero-based level index),
/// with what its literal text lost in decoding.
fn build_pattern(text: &[u8], positions: &[u8], enc: Codepage) -> (Vec<NumberText>, TextLoss) {
    let mut loss = TextLoss::default();
    let Some((&count, rest)) = text.split_first() else {
        return (Vec::new(), loss);
    };
    let chars = &rest[..rest.len().min(count as usize)];
    let mut out: Vec<NumberText> = Vec::new();
    let mut literal: Vec<u8> = Vec::new();
    let mut flush = |literal: &mut Vec<u8>, out: &mut Vec<NumberText>| {
        if !literal.is_empty() {
            let (s, lost) = enc.decode(literal);
            loss |= lost;
            out.push(NumberText::Literal(s));
            literal.clear();
        }
    };
    for (i, &b) in chars.iter().enumerate() {
        if b <= 8 && positions.contains(&((i + 1) as u8)) {
            flush(&mut literal, &mut out);
            out.push(NumberText::Level(b));
        } else {
            literal.push(b);
        }
    }
    flush(&mut literal, &mut out);
    (out, loss)
}

#[derive(Debug, Clone)]
pub struct ListDef {
    pub levels: [ListLevelDef; LIST_LEVELS],
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StyleDef {
    pub outline: Option<u8>,
    pub delta: StyleDelta,
    /// The block container the style's name designates.
    pub block: Option<BlockStyle>,
}

impl StyleDef {
    fn over(self, base: StyleDef) -> StyleDef {
        StyleDef {
            outline: self.outline.or(base.outline),
            delta: base.delta.merge(self.delta),
            block: self.block.or(base.block),
        }
    }
}

/// Resolve the styles of a `sbasedon` cycle, given in walk order, each as
/// found going once around from itself, and return the first's.
fn resolve_cycle(ring: &[(i32, StyleDef)], resolved: &mut HashMap<i32, StyleDef>) -> StyleDef {
    let mut base = ring.iter().rev().fold(StyleDef::default(), |base, &(_, def)| def.over(base));
    for &(id, def) in ring.iter().rev() {
        base = def.over(base);
        resolved.insert(id, base);
    }
    base
}

/// A prelude table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Fonts,
    Styles,
    Lists,
    Overrides,
}

impl Table {
    /// The table whose destination word is `word`.
    fn named(word: &str) -> Option<Table> {
        match word {
            "fonttbl" => Some(Table::Fonts),
            "stylesheet" => Some(Table::Styles),
            "listtable" => Some(Table::Lists),
            "listoverridetable" => Some(Table::Overrides),
            _ => None,
        }
    }
}

/// Finds the table groups (`{\name ...}` or `{\*\name ...}`) in the one
/// lexing pass over the input, bin-aware since it follows the lexer.
#[derive(Debug, Default)]
pub struct TableScan {
    /// The depth of the group whose first control word, after any `\*`, is
    /// still to come.
    opening: Option<usize>,
    /// Each open table group: its table, its depth and where its body starts.
    open: Vec<(Table, usize, usize)>,
}

impl TableScan {
    /// Follow `token`, lexed at group depth `depth` (after an `Open` pushes,
    /// before a `Close` pops) and ending at byte `end`: the table and body
    /// range of the table group it closes.
    pub fn token(
        &mut self,
        token: &Token,
        depth: usize,
        end: usize,
    ) -> Option<(Table, Range<usize>)> {
        let opening = self.opening.take();
        match *token {
            Token::Open => self.opening = Some(depth),
            Token::Symbol(b'*') => self.opening = opening,
            Token::Word { name, .. } if opening == Some(depth) => {
                if let Some(table) = Table::named(name)
                    && !self.open.iter().any(|&(open, ..)| open == table)
                {
                    self.open.push((table, depth, end));
                }
            }
            Token::Close if self.open.last().is_some_and(|&(_, open, _)| open == depth) => {
                let (table, _, start) = self.open.pop()?;
                return Some((table, start..end.saturating_sub(1)));
            }
            _ => {}
        }
        None
    }
}

#[derive(Debug, Default)]
pub struct Prelude {
    /// Font number -> encoding, from `\fcharsetN`.
    pub fonts: HashMap<i32, Codepage>,
    /// Paragraph style id (`\sN`) -> definition.
    pub styles: HashMap<i32, StyleDef>,
    /// `\lsN` -> its override, resolved through `by_list_id` at first use.
    overrides: HashMap<i32, Override>,
    /// `\lsN` -> list definition, resolved once.
    lists: HashMap<i32, ListDef>,
    /// `\listidN` -> list definition, which overrides resolve through.
    by_list_id: HashMap<i32, ListDef>,
}

impl Prelude {
    /// Read the body of one `table` group, decoding its text in `enc`.
    pub fn read(
        &mut self,
        parse_ctx: &ParseCtx,
        table: Table,
        group: &[u8],
        enc: Codepage,
    ) -> Result<(), ConvertError> {
        match table {
            Table::Fonts => parse_fonttbl(parse_ctx, group, &mut self.fonts, enc),
            Table::Styles => parse_stylesheet(parse_ctx, group, &mut self.styles, enc),
            Table::Lists => parse_listtable(parse_ctx, group, &mut self.by_list_id, enc),
            Table::Overrides => parse_overrides(parse_ctx, group, &mut self.overrides, enc),
        }
    }

    /// The list definition `\lsN` names, resolved through the list table
    /// read by its first use, wherever the two tables sit.
    pub fn list(&mut self, ls: i32) -> Option<&ListDef> {
        let over = self.overrides.get(&ls)?;
        Some(self.lists.entry(ls).or_insert_with(|| over.resolve(&self.by_list_id)))
    }
}

/// One `\listoverride`: the list it names and what it overrides per level.
#[derive(Debug)]
struct Override {
    list_id: i32,
    levels: [LevelOverride; LIST_LEVELS],
}

/// One `\lfolevel`: an overridden start, marker or number text.
#[derive(Debug, Default)]
struct LevelOverride {
    start: Option<u64>,
    marker: Option<Option<MarkerKind>>,
    pattern: Option<(NumberPattern, TextLoss)>,
}

impl Override {
    fn resolve(&self, by_list_id: &HashMap<i32, ListDef>) -> ListDef {
        let mut def = by_list_id.get(&self.list_id).cloned().unwrap_or_else(|| ListDef {
            levels: std::array::from_fn(|_| ListLevelDef::default()),
        });
        for (level, over) in def.levels.iter_mut().zip(&self.levels) {
            if let Some(marker) = over.marker {
                level.marker = marker;
            }
            if let Some(start) = over.start {
                level.start = start;
            }
            if let Some((pattern, loss)) = &over.pattern
                && level.marker.is_some_and(MarkerKind::ordered)
            {
                level.pattern = pattern.clone();
                level.loss = *loss;
            }
        }
        def
    }
}

/// Collector for the byte payloads of `\leveltext` / `\levelnumbers`
/// destinations inside one level group.
#[derive(Default)]
struct LevelTextCollector {
    /// Which destination is currently receiving bytes.
    active: Option<bool>, // true = leveltext, false = levelnumbers
    text: Vec<u8>,
    numbers: Vec<u8>,
    legal: bool,
}

impl LevelTextCollector {
    fn byte(&mut self, b: u8) {
        match self.active {
            Some(true) => self.text.push(b),
            Some(false) => self.numbers.push(b),
            None => {}
        }
    }

    /// The finished pattern for an ordered level, with what its literal
    /// text lost; bullets carry glyph text, not a pattern.
    fn finish(&mut self, marker: Option<MarkerKind>, enc: Codepage) -> (NumberPattern, TextLoss) {
        let finished = if marker.is_some_and(MarkerKind::ordered) {
            let (text, loss) = build_pattern(&self.text, &self.numbers, enc);
            (NumberPattern { text, legal: self.legal }, loss)
        } else {
            (NumberPattern::default(), TextLoss::default())
        };
        *self = LevelTextCollector::default();
        finished
    }
}

fn parse_fonttbl(
    parse_ctx: &ParseCtx,
    group: &[u8],
    fonts: &mut HashMap<i32, Codepage>,
    default_encoding: Codepage,
) -> Result<(), ConvertError> {
    let mut lexer = Lexer::new(parse_ctx, group);
    let mut current: Option<i32> = None;
    while let Some(token) = lexer.next_token()? {
        if let Token::Word { name, param } = token {
            match name {
                "f" => current = param,
                "fcharset" => {
                    if let (Some(f), Some(cs)) = (current, param) {
                        fonts.insert(f, charset_encoding(cs, default_encoding));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// The null style id: `\sbasedon222` means "based on nothing".
const NULL_STYLE: i32 = 222;

fn parse_stylesheet(
    parse_ctx: &ParseCtx,
    group: &[u8],
    styles: &mut HashMap<i32, StyleDef>,
    enc: Codepage,
) -> Result<(), ConvertError> {
    let mut lexer = Lexer::new(parse_ctx, group);
    let mut depth = 0usize;
    let mut current: Option<(i32, StyleDef, Option<i32>)> = None;
    let mut raw: HashMap<i32, (StyleDef, Option<i32>)> = HashMap::new();
    // A style's name is the text at the end of its group, before the `;`.
    let mut name: Vec<u8> = Vec::new();
    while let Some(token) = lexer.next_token()? {
        match token {
            Token::Open => depth += 1,
            Token::Close => {
                if depth == 1
                    && let Some((id, mut def, base)) = current.take()
                {
                    let (text, loss) = enc.decode(&name);
                    if loss.invalid {
                        parse_ctx.emit(DiagnosticCode::InvalidStyleEncoding, Region::Container);
                    }
                    def.block = blockstyle::from_style_name(text.trim_end_matches(';'));
                    raw.insert(id, (def, base));
                }
                name.clear();
                depth = depth.saturating_sub(1);
            }
            Token::Hex(b) | Token::Byte(b) if depth == 1 && current.is_some() => name.push(b),
            Token::Word { name: word, param } => match word {
                "s" => {
                    current = Some((param.unwrap_or(0), StyleDef::default(), None));
                    name.clear();
                }
                "sbasedon" => {
                    if let Some((id, _, base)) = current.as_mut() {
                        *base = param.filter(|&b| b != NULL_STYLE && b != *id);
                    }
                }
                "outlinelevel" => {
                    if let (Some((_, def, _)), Some(n)) = (current.as_mut(), param)
                        && (0..9).contains(&n)
                    {
                        def.outline = Some((n + 1) as u8);
                    }
                }
                "b" => {
                    if let Some((_, def, _)) = current.as_mut() {
                        def.delta.bold = Some(param != Some(0));
                    }
                }
                "i" => {
                    if let Some((_, def, _)) = current.as_mut() {
                        def.delta.italic = Some(param != Some(0));
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    let mut resolved: HashMap<i32, StyleDef> = HashMap::with_capacity(raw.len());
    let mut ids: Vec<i32> = raw.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        if resolved.contains_key(&id) {
            continue;
        }
        let mut walked: Vec<(i32, StyleDef)> = Vec::new();
        let mut positions: HashMap<i32, usize> = HashMap::new();
        let mut base = StyleDef::default();
        let mut cycle_at = None;
        let mut cursor = Some(id);
        while let Some(cur) = cursor {
            if let Some(&known) = resolved.get(&cur) {
                base = known;
                break;
            }
            if let Some(&at) = positions.get(&cur) {
                cycle_at = Some(at);
                break;
            }
            let Some(&(def, next)) = raw.get(&cur) else { break };
            positions.insert(cur, walked.len());
            walked.push((cur, def));
            if next.is_some() {
                parse_ctx.charge(1)?;
            }
            cursor = next;
        }
        let tail = match cycle_at {
            Some(at) => {
                parse_ctx.emit(DiagnosticCode::StyleCycle, Region::Container);
                let (tail, ring) = walked.split_at(at);
                base = resolve_cycle(ring, &mut resolved);
                tail
            }
            None => &walked,
        };
        for &(link, def) in tail.iter().rev() {
            base = def.over(base);
            resolved.insert(link, base);
        }
    }
    styles.extend(resolved);
    Ok(())
}

fn parse_listtable(
    parse_ctx: &ParseCtx,
    group: &[u8],
    by_list_id: &mut HashMap<i32, ListDef>,
    enc: Codepage,
) -> Result<(), ConvertError> {
    let mut lexer = Lexer::new(parse_ctx, group);
    let mut depth = 0usize;
    let mut list_depth: Option<usize> = None;
    let mut levels: [ListLevelDef; LIST_LEVELS] = std::array::from_fn(|_| ListLevelDef::default());
    let mut level_index = 0usize;
    let mut in_level = false;
    let mut list_id: Option<i32> = None;
    let mut collector = LevelTextCollector::default();
    while let Some(token) = lexer.next_token()? {
        match token {
            Token::Open => depth += 1,
            Token::Close => {
                // A leveltext/levelnumbers destination ends at its group.
                collector.active = None;
                if in_level && Some(depth) == list_depth.map(|d| d + 1) {
                    in_level = false;
                    if level_index < LIST_LEVELS {
                        let level = &mut levels[level_index];
                        (level.pattern, level.loss) = collector.finish(level.marker, enc);
                    }
                    level_index += 1;
                }
                if list_depth == Some(depth) {
                    if let Some(id) = list_id.take() {
                        by_list_id.insert(id, ListDef { levels: levels.clone() });
                    }
                    levels = std::array::from_fn(|_| ListLevelDef::default());
                    level_index = 0;
                    list_depth = None;
                }
                depth = depth.saturating_sub(1);
            }
            Token::Word { name, param } => match name {
                "list" => {
                    list_depth = Some(depth);
                    list_id = None;
                    levels = std::array::from_fn(|_| ListLevelDef::default());
                    level_index = 0;
                }
                "listid" => {
                    if list_depth.is_some() {
                        list_id = param;
                    }
                }
                "listlevel" => {
                    in_level = true;
                    collector = LevelTextCollector::default();
                }
                "levelnfc" | "levelnfcn" => {
                    if in_level && level_index < LIST_LEVELS {
                        levels[level_index].marker = marker_for_nfc(param.unwrap_or(0));
                    }
                }
                "levelstartat" => {
                    if in_level
                        && level_index < LIST_LEVELS
                        && let Some(n) = param
                    {
                        levels[level_index].start = n.max(0) as u64;
                    }
                }
                "leveltext" if in_level => collector.active = Some(true),
                "levelnumbers" if in_level => collector.active = Some(false),
                "levellegal" if in_level => collector.legal = param != Some(0),
                _ => {}
            },
            Token::Hex(b) | Token::Byte(b) => collector.byte(b),
            _ => {}
        }
    }
    Ok(())
}

fn parse_overrides(
    parse_ctx: &ParseCtx,
    group: &[u8],
    overrides: &mut HashMap<i32, Override>,
    enc: Codepage,
) -> Result<(), ConvertError> {
    let mut lexer = Lexer::new(parse_ctx, group);
    let mut depth = 0usize;
    let mut over_depth: Option<usize> = None;
    let mut lfo_depth: Option<usize> = None;
    let mut list_id: Option<i32> = None;
    let mut ls: Option<i32> = None;
    // Per-level override records (`\lfolevel` groups in level order): an
    // overridden start and/or an overriding format (embedded `\listlevel`
    // with its own number text).
    let mut level_index = 0usize;
    let mut levels: [LevelOverride; LIST_LEVELS] = Default::default();
    let mut collector = LevelTextCollector::default();
    let mut flush = |ls: &mut Option<i32>,
                     list_id: &mut Option<i32>,
                     levels: &mut [LevelOverride; LIST_LEVELS]| {
        let levels = std::mem::take(levels);
        if let (Some(ls), Some(list_id)) = (ls.take(), list_id.take()) {
            overrides.insert(ls, Override { list_id, levels });
        }
    };
    while let Some(token) = lexer.next_token()? {
        match token {
            Token::Open => depth += 1,
            Token::Close => {
                collector.active = None;
                if lfo_depth == Some(depth) {
                    lfo_depth = None;
                    if level_index < LIST_LEVELS && !collector.text.is_empty() {
                        let (text, loss) = build_pattern(&collector.text, &collector.numbers, enc);
                        let pattern = NumberPattern { text, legal: collector.legal };
                        levels[level_index].pattern = Some((pattern, loss));
                    }
                    collector = LevelTextCollector::default();
                    level_index += 1;
                }
                if over_depth == Some(depth) {
                    flush(&mut ls, &mut list_id, &mut levels);
                    level_index = 0;
                    over_depth = None;
                }
                depth = depth.saturating_sub(1);
            }
            Token::Word { name, param } => match name {
                "listoverride" => {
                    // A previous override group left unclosed still counts.
                    flush(&mut ls, &mut list_id, &mut levels);
                    over_depth = Some(depth);
                    level_index = 0;
                }
                "listid" if over_depth.is_some() => list_id = param,
                "ls" if over_depth.is_some() => ls = param,
                "lfolevel" => {
                    lfo_depth = Some(depth);
                    collector = LevelTextCollector::default();
                }
                "levelstartat" if lfo_depth.is_some() => {
                    if level_index < LIST_LEVELS
                        && let Some(n) = param
                    {
                        levels[level_index].start = Some(n.max(0) as u64);
                    }
                }
                "levelnfc" | "levelnfcn" if lfo_depth.is_some() => {
                    if level_index < LIST_LEVELS {
                        levels[level_index].marker = Some(marker_for_nfc(param.unwrap_or(0)));
                    }
                }
                "leveltext" if lfo_depth.is_some() => collector.active = Some(true),
                "levelnumbers" if lfo_depth.is_some() => collector.active = Some(false),
                "levellegal" if lfo_depth.is_some() => collector.legal = param != Some(0),
                _ => {}
            },
            Token::Hex(b) | Token::Byte(b) => collector.byte(b),
            _ => {}
        }
    }
    flush(&mut ls, &mut list_id, &mut levels);
    Ok(())
}

/// MS-OSHARED numbering formats -> marker kinds. `None` = no number.
fn marker_for_nfc(nfc: i32) -> Option<MarkerKind> {
    match nfc {
        0 => Some(MarkerKind::Decimal),
        1 => Some(MarkerKind::UpperRoman),
        2 => Some(MarkerKind::LowerRoman),
        3 => Some(MarkerKind::UpperAlpha),
        4 => Some(MarkerKind::LowerAlpha),
        23 => Some(MarkerKind::Bullet),
        255 => None,
        _ => Some(MarkerKind::Decimal),
    }
}

/// `\fcharsetN` -> encoding; a charset with no code page here is guessed
/// to be the document's.
fn charset_encoding(charset: i32, default_encoding: Codepage) -> Codepage {
    let encoding = match charset {
        // The symbol charset maps glyphs, not a code page's letters.
        0..=2 => return default_encoding,
        128 => encoding_rs::SHIFT_JIS,
        129 => encoding_rs::EUC_KR,
        134 => encoding_rs::GBK,
        136 => encoding_rs::BIG5,
        161 => encoding_rs::WINDOWS_1253,
        162 => encoding_rs::WINDOWS_1254,
        163 => encoding_rs::WINDOWS_1258,
        177 => encoding_rs::WINDOWS_1255,
        178..=180 => encoding_rs::WINDOWS_1256,
        186 => encoding_rs::WINDOWS_1257,
        204 => encoding_rs::WINDOWS_1251,
        222 => encoding_rs::WINDOWS_874,
        238 => encoding_rs::WINDOWS_1250,
        _ => return Codepage { assumed: true, ..default_encoding },
    };
    Codepage::declared(encoding)
}

/// `\ansicpgN` -> encoding, or `None` for a code page this reader lacks.
pub fn codepage_encoding(cp: u32) -> Option<&'static encoding_rs::Encoding> {
    Some(match cp {
        932 => encoding_rs::SHIFT_JIS,
        936 => encoding_rs::GBK,
        949 => encoding_rs::EUC_KR,
        950 => encoding_rs::BIG5,
        1250 => encoding_rs::WINDOWS_1250,
        1251 => encoding_rs::WINDOWS_1251,
        1253 => encoding_rs::WINDOWS_1253,
        1254 => encoding_rs::WINDOWS_1254,
        1255 => encoding_rs::WINDOWS_1255,
        1256 => encoding_rs::WINDOWS_1256,
        1257 => encoding_rs::WINDOWS_1257,
        1258 => encoding_rs::WINDOWS_1258,
        874 => encoding_rs::WINDOWS_874,
        1252 => encoding_rs::WINDOWS_1252,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{Limit, Limits};
    use std::assert_matches;

    fn stylesheet(
        parse_ctx: &ParseCtx,
        group: &str,
    ) -> Result<HashMap<i32, StyleDef>, ConvertError> {
        let mut styles = HashMap::new();
        parse_stylesheet(
            parse_ctx,
            group.as_bytes(),
            &mut styles,
            Codepage::declared(encoding_rs::WINDOWS_1252),
        )?;
        Ok(styles)
    }

    fn tokens(group: &str) -> u64 {
        let lexing = ParseCtx::default();
        let mut lexer = Lexer::new(&lexing, group.as_bytes());
        while lexer.next_token().unwrap().is_some() {}
        lexing.fuel()
    }

    fn chain(depth: i32) -> String {
        let styles: String = (1001..=1000 + depth)
            .map(|i| format!(r"{{\s{i}\sbasedon{} \b S{i};}}", i - 1))
            .collect();
        format!(r"{{\s1000\i Normal;}}{styles}")
    }

    #[test]
    fn a_chain_resolves_each_style_once_against_max_work() {
        let group = chain(4000);
        let parse_ctx = ParseCtx::default();
        let styles = stylesheet(&parse_ctx, &group).unwrap();
        assert_eq!(styles.len(), 4001);
        assert!(styles.values().all(|s| s.delta.italic == Some(true)));
        assert_eq!(parse_ctx.fuel(), tokens(&group) + 4000);
        let max_work = parse_ctx.fuel() - 1;
        let limits = Limits { max_work, ..Limits::default() };
        let err = stylesheet(&ParseCtx::new(&limits), &group).unwrap_err();
        assert_matches!(
            err,
            ConvertError::ResourceLimit { limit: Limit::Work, max } if max == max_work
        );
    }

    #[test]
    fn a_style_below_a_cycle_keeps_the_prefix_up_to_it() {
        let group = r"{\s1\sbasedon2\b A;}{\s2\sbasedon1 B;}{\s3\sbasedon1\i C;}";
        let parse_ctx = ParseCtx::default();
        let styles = stylesheet(&parse_ctx, group).unwrap();
        let c = styles[&3].delta;
        assert_eq!((c.bold, c.italic), (Some(true), Some(true)));
        assert_eq!(styles[&2].delta.bold, Some(true));
        assert_eq!(parse_ctx.fuel(), tokens(group) + 3);
    }

    fn cycles(parse_ctx: ParseCtx) -> u64 {
        parse_ctx
            .into_diagnostics()
            .iter()
            .filter(|d| d.code == DiagnosticCode::StyleCycle)
            .map(|d| d.count)
            .sum()
    }

    #[test]
    fn each_style_of_a_cycle_resolves_as_found_going_once_around_from_itself() {
        let group = r"{\s1\sbasedon2\b A;}{\s2\sbasedon3\b0 B;}{\s3\sbasedon1 C;}{\s4\sbasedon2 D;}{\s5\sbasedon3 E;}";
        let parse_ctx = ParseCtx::default();
        let styles = stylesheet(&parse_ctx, group).unwrap();
        let bold: Vec<_> = (1..=5).map(|id| styles[&id].delta.bold).collect();
        assert_eq!(bold, [Some(true), Some(false), Some(true), Some(false), Some(true)]);
        assert_eq!(cycles(parse_ctx), 1);
    }

    #[test]
    fn styles_under_a_cycle_resolve_in_work_linear_in_their_count() {
        let (leaves, chain, head) = (2000, 2000, 5000);
        let below: String =
            (1001..1001 + leaves).map(|i| format!(r"{{\s{i}\sbasedon{head} L{i};}}")).collect();
        let links: String =
            (head..head + chain).map(|i| format!(r"{{\s{i}\sbasedon{} C{i};}}", i + 1)).collect();
        let (a, b) = (head + chain, head + chain + 1);
        let group = format!(r"{below}{links}{{\s{a}\sbasedon{b} A;}}{{\s{b}\sbasedon{a} B;}}");
        let parse_ctx = ParseCtx::default();
        let styles = stylesheet(&parse_ctx, &group).unwrap();
        let count = u64::try_from(leaves + chain).unwrap() + 2;
        assert_eq!(u64::try_from(styles.len()).unwrap(), count);
        assert_eq!(parse_ctx.fuel(), tokens(&group) + count);
        assert_eq!(cycles(parse_ctx), 1);
    }
}
