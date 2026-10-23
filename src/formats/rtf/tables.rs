//! RTF prelude tables parsed into typed definitions: the font table (with
//! per-font charsets and faces), the style sheet, and the list/list-override
//! tables.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::formats::rtf::lexer::{Lexer, Token};
use crate::model::VerticalPosition;
use crate::shared::blockstyle::{self, BlockStyle};
use crate::shared::codepage::{self, Codepage, TextLoss};
use crate::shared::delta::StyleDelta;
use crate::shared::list::MarkerKind;
use crate::shared::numbering::{NumberPattern, NumberText, marker_for_nfc};
use crate::shared::symbol::SymbolFont;
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

pub(super) const LIST_LEVELS: usize = crate::shared::numbering::OFFICE_LEVELS;

/// The heading level an `\outlinelevelN` parameter names: 0 to 8 are levels
/// 1 to 9, and anything else names none.
pub(super) fn outline_level(n: i32) -> Option<u8> {
    u8::try_from(n).ok().filter(|&n| n < 9).map(|n| n + 1)
}

#[derive(Debug, Clone)]
pub(super) struct ListLevelDef {
    pub marker: Option<MarkerKind>,
    pub start: u64,
    /// `\leveltext` with `\levelnumbers` placeholders and `\levellegal`.
    pub pattern: NumberPattern,
    /// What the pattern's literal text lost in decoding.
    pub loss: TextLoss,
}

impl Default for ListLevelDef {
    fn default() -> Self {
        Self {
            marker: Some(MarkerKind::Bullet),
            start: 1,
            pattern: NumberPattern::default(),
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
    let mut literal = |run: &[u8], out: &mut Vec<NumberText>| {
        if !run.is_empty() {
            let (s, lost) = enc.decode(run);
            loss |= lost;
            out.push(NumberText::Literal(s));
        }
    };
    let mut start = 0;
    for (i, &b) in chars.iter().enumerate() {
        if b <= 8 && u8::try_from(i + 1).is_ok_and(|at| positions.contains(&at)) {
            literal(&chars[start..i], &mut out);
            out.push(NumberText::Level(b));
            start = i + 1;
        }
    }
    literal(&chars[start..], &mut out);
    (out, loss)
}

#[derive(Debug, Clone, Default)]
pub(super) struct ListDef {
    pub levels: [ListLevelDef; LIST_LEVELS],
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct StyleDef {
    pub outline: Option<u8>,
    pub delta: StyleDelta,
    /// The block container the style's name designates.
    pub block: Option<BlockStyle>,
}

impl StyleDef {
    fn apply_control(&mut self, word: &str, param: Option<i32>) {
        let on = Some(param != Some(0));
        match word {
            "b" => self.delta.bold = on,
            "i" => self.delta.italic = on,
            "v" => self.delta.hidden = on,
            "super" => self.delta.position = Some(VerticalPosition::Superscript),
            "sub" => self.delta.position = Some(VerticalPosition::Subscript),
            "nosupersub" => self.delta.position = Some(VerticalPosition::Baseline),
            "plain" => {
                self.delta = StyleDelta {
                    bold: Some(false),
                    italic: Some(false),
                    strike: Some(false),
                    code: Some(false),
                    position: Some(VerticalPosition::Baseline),
                    hidden: Some(false),
                };
            }
            "outlinelevel" => {
                if let Some(level) = param.and_then(outline_level) {
                    self.outline = Some(level);
                }
            }
            _ => {}
        }
    }

    fn over(self, base: Self) -> Self {
        Self {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Table {
    Fonts,
    Styles,
    Lists,
    Overrides,
}

impl Table {
    fn named(word: &str) -> Option<Self> {
        match word {
            "fonttbl" => Some(Self::Fonts),
            "stylesheet" => Some(Self::Styles),
            "listtable" => Some(Self::Lists),
            "listoverridetable" => Some(Self::Overrides),
            _ => None,
        }
    }
}

/// Finds the table groups (`{\name ...}` or `{\*\name ...}`) in the one
/// lexing pass over the input, bin-aware since it follows the lexer.
#[derive(Debug, Default)]
pub(super) struct TableScan {
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
    pub(super) fn token(
        &mut self,
        token: &Token<'_>,
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

/// What a font draws for the text set in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Face {
    Text,
    Glyphs(SymbolFont),
    /// A symbol-charset font no glyph table knows, whose text stays as stored.
    Charset2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Font {
    /// The code page its `\fcharsetN` names; `None` reads in the document's.
    pub codepage: Option<Codepage>,
    pub face: Face,
}

#[derive(Debug, Default)]
pub(super) struct Prelude {
    pub fonts: HashMap<i32, Font>,
    pub styles: HashMap<i32, StyleDef>,
    overrides: HashMap<i32, Override>,
    lists: HashMap<i32, ListDef>,
    by_list_id: HashMap<i32, ListDef>,
}

impl Prelude {
    /// Read the body of one `table` group, decoding its text in `enc`.
    pub(super) fn read(
        &mut self,
        parse_ctx: &ParseCtx<'_>,
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
    pub(super) fn list(&mut self, ls: i32) -> Option<&ListDef> {
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
    marker: MarkerOverride,
    pattern: Option<(NumberPattern, TextLoss)>,
}

#[derive(Debug, Default, Clone, Copy)]
enum MarkerOverride {
    #[default]
    Inherit,
    Unnumbered,
    Kind(MarkerKind),
}

impl MarkerOverride {
    fn of_nfc(nfc: i32) -> Self {
        marker_for_nfc(nfc).map_or(Self::Unnumbered, Self::Kind)
    }

    fn over(self, level: Option<MarkerKind>) -> Option<MarkerKind> {
        match self {
            Self::Inherit => level,
            Self::Unnumbered => None,
            Self::Kind(kind) => Some(kind),
        }
    }
}

impl Override {
    fn resolve(&self, by_list_id: &HashMap<i32, ListDef>) -> ListDef {
        let mut def = by_list_id.get(&self.list_id).cloned().unwrap_or_default();
        for (level, over) in def.levels.iter_mut().zip(&self.levels) {
            level.marker = over.marker.over(level.marker);
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

enum LevelDest {
    Text,
    Numbers,
}

/// Collector for the byte payloads of `\leveltext` / `\levelnumbers`
/// destinations inside one level group.
#[derive(Default)]
struct LevelTextCollector {
    active: Option<LevelDest>,
    text: Vec<u8>,
    numbers: Vec<u8>,
    legal: bool,
}

impl LevelTextCollector {
    fn byte(&mut self, b: u8) {
        match self.active {
            Some(LevelDest::Text) => self.text.push(b),
            Some(LevelDest::Numbers) => self.numbers.push(b),
            None => {}
        }
    }

    fn control(&mut self, word: &str, param: Option<i32>) {
        match word {
            "leveltext" => self.active = Some(LevelDest::Text),
            "levelnumbers" => self.active = Some(LevelDest::Numbers),
            "levellegal" => self.legal = param != Some(0),
            _ => {}
        }
    }

    fn pattern(&self, enc: Codepage) -> (NumberPattern, TextLoss) {
        let (text, loss) = build_pattern(&self.text, &self.numbers, enc);
        (NumberPattern { text, legal: self.legal }, loss)
    }

    /// The finished pattern for an ordered level, with what its literal
    /// text lost; bullets carry glyph text, not a pattern.
    fn finish(&mut self, marker: Option<MarkerKind>, enc: Codepage) -> (NumberPattern, TextLoss) {
        let finished = if marker.is_some_and(MarkerKind::ordered) {
            self.pattern(enc)
        } else {
            (NumberPattern::default(), TextLoss::default())
        };
        *self = Self::default();
        finished
    }
}

#[derive(Default)]
struct FontEntry {
    id: i32,
    depth: usize,
    charset: Option<i32>,
    /// The name with its leading whitespace dropped, capped at the longest
    /// name a glyph table knows; `overlong` once text runs past the cap.
    name: Vec<u8>,
    overlong: bool,
    named: bool,
}

impl FontEntry {
    fn byte(&mut self, b: u8) {
        let space = b.is_ascii_whitespace();
        match b {
            b';' => self.named = true,
            _ if self.named || (space && self.name.is_empty()) => {}
            _ if self.name.len() < SymbolFont::LONGEST_NAME => self.name.push(b),
            _ => self.overlong |= !space,
        }
    }

    fn finish(self, default_encoding: Codepage) -> (i32, Font) {
        let glyphs = std::str::from_utf8(self.name.trim_ascii())
            .ok()
            .filter(|_| !self.overlong)
            .and_then(SymbolFont::from_name);
        let face = match (glyphs, self.charset) {
            (Some(font), _) => Face::Glyphs(font),
            (None, Some(2)) => Face::Charset2,
            (None, _) => Face::Text,
        };
        let codepage = self.charset.map(|cs| codepage::charset(cs, default_encoding));
        (self.id, Font { codepage, face })
    }
}

/// Read the font table: each entry braced (`{\f1 ...;}`) or run on
/// (`\f1 ...;\f2 ...;`), its name the text before `;` at its own depth, so
/// a nested `\falt` or `\panose` group neither names nor opens one.
fn parse_fonttbl(
    parse_ctx: &ParseCtx<'_>,
    group: &[u8],
    fonts: &mut HashMap<i32, Font>,
    default_encoding: Codepage,
) -> Result<(), ConvertError> {
    let mut lexer = Lexer::new(parse_ctx, group);
    let mut depth = 0usize;
    let mut entry: Option<FontEntry> = None;
    while let Some(token) = lexer.next_token()? {
        let at_entry = entry.as_ref().is_some_and(|entry| entry.depth == depth);
        match token {
            Token::Open => depth += 1,
            Token::Close => {
                depth = depth.saturating_sub(1);
                if entry.as_ref().is_some_and(|entry| depth < entry.depth) {
                    fonts.extend(entry.take().map(|e| e.finish(default_encoding)));
                }
            }
            Token::Word { name: "f", param } if entry.as_ref().is_none_or(|e| depth <= e.depth) => {
                fonts.extend(entry.take().map(|e| e.finish(default_encoding)));
                entry = param.map(|id| FontEntry { id, depth, ..FontEntry::default() });
            }
            Token::Word { name: "fcharset", param: Some(charset) } if at_entry => {
                if let Some(entry) = entry.as_mut() {
                    entry.charset = Some(charset);
                }
            }
            Token::Hex(b) | Token::Byte(b) | Token::Symbol(b @ (b'\\' | b'{' | b'}'))
                if at_entry =>
            {
                if let Some(entry) = entry.as_mut() {
                    entry.byte(b);
                }
            }
            _ => {}
        }
    }
    fonts.extend(entry.take().map(|e| e.finish(default_encoding)));
    Ok(())
}

/// The null style id: `\sbasedon222` means "based on nothing".
const NULL_STYLE: i32 = 222;

fn parse_stylesheet(
    parse_ctx: &ParseCtx<'_>,
    group: &[u8],
    styles: &mut HashMap<i32, StyleDef>,
    enc: Codepage,
) -> Result<(), ConvertError> {
    let mut lexer = Lexer::new(parse_ctx, group);
    let mut depth = 0usize;
    let mut current: Option<(i32, StyleDef, Option<i32>)> = None;
    let mut raw: BTreeMap<i32, (StyleDef, Option<i32>)> = BTreeMap::new();
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
                _ => {
                    if let Some((_, def, _)) = current.as_mut() {
                        def.apply_control(word, param);
                    }
                }
            },
            _ => {}
        }
    }
    styles.extend(resolve_styles(parse_ctx, &raw)?);
    Ok(())
}

fn resolve_styles(
    parse_ctx: &ParseCtx<'_>,
    raw: &BTreeMap<i32, (StyleDef, Option<i32>)>,
) -> Result<HashMap<i32, StyleDef>, ConvertError> {
    let mut resolved: HashMap<i32, StyleDef> = HashMap::with_capacity(raw.len());
    for &id in raw.keys() {
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
    Ok(resolved)
}

#[derive(Default)]
struct ListReader {
    depth: usize,
    list_depth: Option<usize>,
    levels: [ListLevelDef; LIST_LEVELS],
    level_index: usize,
    in_level: bool,
    list_id: Option<i32>,
    collector: LevelTextCollector,
}

impl ListReader {
    fn close(&mut self, by_list_id: &mut HashMap<i32, ListDef>, enc: Codepage) {
        // A leveltext/levelnumbers destination ends at its group.
        self.collector.active = None;
        if self.in_level && Some(self.depth) == self.list_depth.map(|d| d + 1) {
            self.in_level = false;
            if let Some(level) = self.levels.get_mut(self.level_index) {
                (level.pattern, level.loss) = self.collector.finish(level.marker, enc);
            }
            self.level_index += 1;
        }
        if self.list_depth == Some(self.depth) {
            let levels = std::mem::take(&mut self.levels);
            if let Some(id) = self.list_id.take() {
                by_list_id.insert(id, ListDef { levels });
            }
            self.level_index = 0;
            self.list_depth = None;
        }
        self.depth = self.depth.saturating_sub(1);
    }

    fn control(&mut self, word: &str, param: Option<i32>) {
        match word {
            "list" => {
                self.list_depth = Some(self.depth);
                self.list_id = None;
                self.levels = Default::default();
                self.level_index = 0;
            }
            "listid" if self.list_depth.is_some() => self.list_id = param,
            "listlevel" => {
                self.in_level = true;
                self.collector = LevelTextCollector::default();
            }
            "levelnfc" | "levelnfcn" if self.in_level => {
                if let Some(level) = self.levels.get_mut(self.level_index) {
                    level.marker = marker_for_nfc(param.unwrap_or(0));
                }
            }
            "levelstartat" if self.in_level => {
                if let (Some(level), Some(n)) = (self.levels.get_mut(self.level_index), param) {
                    level.start = u64::try_from(n).unwrap_or(0);
                }
            }
            _ if self.in_level => self.collector.control(word, param),
            _ => {}
        }
    }
}

fn parse_listtable(
    parse_ctx: &ParseCtx<'_>,
    group: &[u8],
    by_list_id: &mut HashMap<i32, ListDef>,
    enc: Codepage,
) -> Result<(), ConvertError> {
    let mut lexer = Lexer::new(parse_ctx, group);
    let mut reader = ListReader::default();
    while let Some(token) = lexer.next_token()? {
        match token {
            Token::Open => reader.depth += 1,
            Token::Close => reader.close(by_list_id, enc),
            Token::Word { name, param } => reader.control(name, param),
            Token::Hex(b) | Token::Byte(b) => reader.collector.byte(b),
            _ => {}
        }
    }
    Ok(())
}

#[derive(Default)]
struct OverrideReader {
    depth: usize,
    over_depth: Option<usize>,
    lfo_depth: Option<usize>,
    list_id: Option<i32>,
    ls: Option<i32>,
    /// One per `\lfolevel` group, in level order.
    levels: [LevelOverride; LIST_LEVELS],
    level_index: usize,
    collector: LevelTextCollector,
}

impl OverrideReader {
    fn flush(&mut self, overrides: &mut HashMap<i32, Override>) {
        let levels = std::mem::take(&mut self.levels);
        if let (Some(ls), Some(list_id)) = (self.ls.take(), self.list_id.take()) {
            overrides.insert(ls, Override { list_id, levels });
        }
    }

    fn close(&mut self, overrides: &mut HashMap<i32, Override>, enc: Codepage) {
        self.collector.active = None;
        if self.lfo_depth == Some(self.depth) {
            self.lfo_depth = None;
            if let Some(level) = self.levels.get_mut(self.level_index)
                && !self.collector.text.is_empty()
            {
                level.pattern = Some(self.collector.pattern(enc));
            }
            self.collector = LevelTextCollector::default();
            self.level_index += 1;
        }
        if self.over_depth == Some(self.depth) {
            self.flush(overrides);
            self.level_index = 0;
            self.over_depth = None;
        }
        self.depth = self.depth.saturating_sub(1);
    }

    fn control(&mut self, overrides: &mut HashMap<i32, Override>, word: &str, param: Option<i32>) {
        match word {
            "listoverride" => {
                // A previous override group left unclosed still counts.
                self.flush(overrides);
                self.over_depth = Some(self.depth);
                self.level_index = 0;
            }
            "listid" if self.over_depth.is_some() => self.list_id = param,
            "ls" if self.over_depth.is_some() => self.ls = param,
            "lfolevel" => {
                self.lfo_depth = Some(self.depth);
                self.collector = LevelTextCollector::default();
            }
            "levelstartat" if self.lfo_depth.is_some() => {
                if let (Some(level), Some(n)) = (self.levels.get_mut(self.level_index), param) {
                    level.start = Some(u64::try_from(n).unwrap_or(0));
                }
            }
            "levelnfc" | "levelnfcn" if self.lfo_depth.is_some() => {
                if let Some(level) = self.levels.get_mut(self.level_index) {
                    level.marker = MarkerOverride::of_nfc(param.unwrap_or(0));
                }
            }
            _ if self.lfo_depth.is_some() => self.collector.control(word, param),
            _ => {}
        }
    }
}

fn parse_overrides(
    parse_ctx: &ParseCtx<'_>,
    group: &[u8],
    overrides: &mut HashMap<i32, Override>,
    enc: Codepage,
) -> Result<(), ConvertError> {
    let mut lexer = Lexer::new(parse_ctx, group);
    let mut reader = OverrideReader::default();
    while let Some(token) = lexer.next_token()? {
        match token {
            Token::Open => reader.depth += 1,
            Token::Close => reader.close(overrides, enc),
            Token::Word { name, param } => reader.control(overrides, name, param),
            Token::Hex(b) | Token::Byte(b) => reader.collector.byte(b),
            _ => {}
        }
    }
    reader.flush(overrides);
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    clippy::format_collect,
    reason = "fixture RTF is built with format!, and an assertion holds for every map value"
)]
mod tests {
    use super::*;
    use crate::formats::rtf::lexer::fuel;
    use crate::limits::{Limit, Limits};
    use std::assert_matches;

    #[test]
    fn an_outline_level_names_a_heading_only_from_0_to_8() {
        assert_eq!(outline_level(0), Some(1));
        assert_eq!(outline_level(8), Some(9));
        for n in [i32::MIN, -1, 9, 256, i32::MAX] {
            assert_eq!(outline_level(n), None, "{n}");
        }
    }

    fn stylesheet(
        parse_ctx: &ParseCtx<'_>,
        group: &str,
    ) -> Result<HashMap<i32, StyleDef>, ConvertError> {
        let mut styles = HashMap::new();
        parse_stylesheet(parse_ctx, group.as_bytes(), &mut styles, codepage::windows(1252))?;
        Ok(styles)
    }

    #[test]
    fn a_font_entry_is_a_symbol_face_only_by_a_name_a_glyph_table_knows() {
        let group = r"{\f1 Zapf Dingbats;}{\f2 Zapf Dingbatss;}{\f3\fcharset2 Zapf Dingbatss;}{\f4\fcharset204 symbol ;}{\f5\fcharset0 Arial;}{\f6 Arial}{\f8 Zapf Dingbats X;}{\f9 Symbol        Pro;}{\f10 Zapf Dingbats BT;}{\f11 Sym\{bol;}{\f12\fcharset2 Sym}{bol;}{\f13               Symbol ;}{\f14 Zapf Dingbats   ;}\f7\fcharset2{\*\falt Symbol}Webdings;\f";
        let default = codepage::windows(1251).marked_assumed();
        let mut fonts = HashMap::new();
        parse_fonttbl(&ParseCtx::default(), group.as_bytes(), &mut fonts, default).unwrap();
        let font = |codepage, face| Font { codepage, face };
        let cyrillic = Some(codepage::windows(1251));
        let expected = HashMap::from([
            (1, font(None, Face::Glyphs(SymbolFont::ZapfDingbats))),
            (2, font(None, Face::Text)),
            (3, font(Some(default), Face::Charset2)),
            (4, font(cyrillic, Face::Glyphs(SymbolFont::Symbol))),
            (5, font(Some(default), Face::Text)),
            (6, font(None, Face::Text)),
            (7, font(Some(default), Face::Glyphs(SymbolFont::Webdings))),
            (8, font(None, Face::Text)),
            (9, font(None, Face::Text)),
            (10, font(None, Face::Text)),
            (11, font(None, Face::Text)),
            (12, font(Some(default), Face::Charset2)),
            (13, font(None, Face::Glyphs(SymbolFont::Symbol))),
            (14, font(None, Face::Glyphs(SymbolFont::ZapfDingbats))),
        ]);
        assert_eq!(fonts, expected);
    }

    fn read_tables(lists: &str, overrides: &str) -> Prelude {
        let parse_ctx = ParseCtx::default();
        let mut prelude = Prelude::default();
        for (table, group) in [(Table::Lists, lists), (Table::Overrides, overrides)] {
            prelude.read(&parse_ctx, table, group.as_bytes(), codepage::windows(1252)).unwrap();
        }
        prelude
    }

    #[test]
    fn a_pattern_splits_its_literal_runs_at_the_listed_placeholders() {
        use NumberText::{Level, Literal};
        let lit = |text: &str| Literal(text.to_owned());
        let none = TextLoss::default();
        let invalid = TextLoss { invalid: true, ..none };
        let cases = [
            ("no payload", (vec![], vec![]), (vec![], none)),
            ("a zero count", (vec![0, b'x'], vec![]), (vec![], none)),
            ("literal text only", (vec![2, b'a', b'.'], vec![]), (vec![lit("a.")], none)),
            (
                "a placeholder between literals",
                (vec![3, b'(', 0, b')'], vec![2]),
                (vec![lit("("), Level(0), lit(")")], none),
            ),
            (
                "adjacent placeholders leave no empty literal",
                (vec![3, 0, 1, b'.'], vec![1, 2]),
                (vec![Level(0), Level(1), lit(".")], none),
            ),
            (
                "a trailing placeholder",
                (vec![2, b'1', 0], vec![2]),
                (vec![lit("1"), Level(0)], none),
            ),
            (
                "a low byte not listed stays text",
                (vec![2, 0, b'.'], vec![]),
                (vec![lit("\0.")], none),
            ),
            ("a listed byte past 8 stays text", (vec![1, 9], vec![1]), (vec![lit("\t")], none)),
            ("the count cuts the bytes", (vec![1, b'a', b'b'], vec![]), (vec![lit("a")], none)),
            (
                "a count past the bytes reads them all",
                (vec![9, b'a'], vec![]),
                (vec![lit("a")], none),
            ),
            (
                "an undecodable run is lost where it sits",
                (vec![3, 0x82, 0, b'.'], vec![2]),
                (vec![lit("\u{fffd}"), Level(0), lit(".")], invalid),
            ),
        ];
        for (label, (text, positions), expected) in cases {
            let got = build_pattern(&text, &positions, codepage::windows(932));
            assert_eq!(got, expected, "{label}");
        }
    }

    #[test]
    fn a_legal_level_reads_from_the_list_table_and_from_an_override_level() {
        let text = r"{\leveltext\'02\'00.;}{\levelnumbers\'01;}";
        for (label, flag, legal) in [
            ("no flag", "", false),
            ("a bare flag", r"\levellegal", true),
            ("a flag of one", r"\levellegal1", true),
            ("a flag of zero", r"\levellegal0", false),
        ] {
            let lists = format!(r"{{\list{{\listlevel\levelnfc0{text}{flag}}}\listid7}}");
            let overrides = format!(
                r"{{\listoverride\listid7\ls2}}{{\listoverride\listid7\ls3{{\lfolevel\levelnfc0{text}{flag}}}}}"
            );
            let mut prelude = read_tables(&lists, &overrides);
            let read = [2, 3].map(|ls| prelude.list(ls).unwrap().levels[0].pattern.legal);
            assert_eq!(read, [legal, legal], "{label}");
        }
    }

    #[test]
    fn a_level_start_reads_from_the_list_table_and_from_an_override_level() {
        for (label, flag, start) in [
            ("no start", "", 1),
            ("a start", r"\levelstartat3", 3),
            ("a negative start", r"\levelstartat-1", 0),
        ] {
            let lists = format!(r"{{\list{{\listlevel\levelnfc0{flag}}}\listid7}}");
            let overrides = format!(
                r"{{\listoverride\listid7\ls2}}{{\listoverride\listid7\ls3{{\lfolevel{flag}}}}}"
            );
            let mut prelude = read_tables(&lists, &overrides);
            let read = [2, 3].map(|ls| prelude.list(ls).unwrap().levels[0].start);
            assert_eq!(read, [start, start], "{label}");
        }
    }

    #[test]
    fn an_override_of_an_unknown_list_starts_from_bullet_levels() {
        let mut prelude = read_tables("", r"{\listoverride\listid99\ls2}");
        let level = &prelude.list(2).unwrap().levels[0];
        assert_eq!((level.marker, level.start), (Some(MarkerKind::Bullet), 1));
    }

    #[test]
    fn a_group_between_two_list_levels_is_no_level() {
        let lists = r"{\list\listid7{\listlevel\levelnfc0}{\listname X;}{\listlevel\levelnfc4}}";
        let mut prelude = read_tables(lists, r"{\listoverride\listid7\ls2}");
        let levels = &prelude.list(2).unwrap().levels;
        let markers: Vec<_> = levels.iter().take(3).map(|level| level.marker).collect();
        let (decimal, alpha, bullet) =
            (MarkerKind::Decimal, MarkerKind::LowerAlpha, MarkerKind::Bullet);
        assert_eq!(markers, [Some(decimal), Some(alpha), Some(bullet)]);
    }

    #[test]
    fn level_controls_outside_a_level_group_change_no_level() {
        let lists = r"{\list\listid7{\listlevel\levelnfc0}\levelnfc4\levelstartat9{\listlevel}}";
        let overrides = r"{\listoverride\listid7\ls2}{\listoverride\listid8\ls3{\lfolevel\levelstartat2}\levelnfc4\levelstartat9{\lfolevel}}";
        let mut prelude = read_tables(lists, overrides);
        let second = [2, 3].map(|ls| {
            let level = &prelude.list(ls).unwrap().levels[1];
            (level.marker, level.start)
        });
        assert_eq!(second, [(Some(MarkerKind::Bullet), 1); 2]);
    }

    #[test]
    fn a_list_opened_inside_an_unclosed_list_starts_afresh() {
        let bullet = MarkerKind::Bullet;
        for (label, lists, list_id, expected) in [
            (
                "an inner list takes no id from the outer",
                r"{\list\listid1{\listlevel\levelnfc4}{\list{\listlevel\levelstartat3}}}",
                1,
                (Some(bullet), 1),
            ),
            (
                "an inner list takes no levels from the outer",
                r"{\list\listid1{\listlevel\levelnfc4}{\list\listid2{\listlevel\levelstartat3}}}",
                2,
                (Some(bullet), 3),
            ),
        ] {
            let overrides = format!(r"{{\listoverride\listid{list_id}\ls2}}");
            let mut prelude = read_tables(lists, &overrides);
            let level = &prelude.list(2).unwrap().levels[0];
            assert_eq!((level.marker, level.start), expected, "{label}");
        }
    }

    #[test]
    fn override_levels_apply_to_successive_levels_whatever_groups_sit_between() {
        let overrides = r"{\listoverride\listid7\ls2{\lfolevel\levelstartat2}{\unknown x}{\lfolevel\levelstartat3}}";
        let mut prelude = read_tables("", overrides);
        let starts: Vec<_> =
            prelude.list(2).unwrap().levels.iter().take(3).map(|l| l.start).collect();
        assert_eq!(starts, [2, 3, 1]);
    }

    #[test]
    fn an_override_level_without_number_text_keeps_the_lists_pattern_and_takes_its_start() {
        let lists =
            r"{\list{\listlevel\levelnfc0{\leveltext\'03(\'00);}{\levelnumbers\'02;}}\listid7}";
        let overrides = r"{\listoverride\listid7\ls2{\lfolevel\levelstartat5}}";
        let mut prelude = read_tables(lists, overrides);
        let level = &prelude.list(2).unwrap().levels[0];
        let open = NumberText::Literal("(".to_owned());
        let close = NumberText::Literal(")".to_owned());
        assert_eq!(level.pattern.text, [open, NumberText::Level(0), close]);
        assert_eq!(level.start, 5);
    }

    #[test]
    fn an_override_without_an_ls_leaves_the_previous_ls_on_its_list() {
        let lists = r"{\list\listid7{\listlevel\levelnfc0}}{\list\listid8{\listlevel\levelnfc4}}";
        let mut prelude = read_tables(lists, r"{\listoverride\listid7\ls2}{\listoverride\listid8}");
        let marker = prelude.list(2).unwrap().levels[0].marker;
        assert_eq!(marker, Some(MarkerKind::Decimal));
    }

    #[test]
    fn an_override_group_left_unclosed_still_counts() {
        for (label, overrides, defined) in [
            (
                "before the next override opens",
                r"{\listoverride\listid7\ls2{\listoverride\listid7\ls3}}",
                [true, true],
            ),
            ("at the end of the table", r"{\listoverride\listid7\ls2", [true, false]),
        ] {
            let mut prelude = read_tables("", overrides);
            let read = [2, 3].map(|ls| prelude.list(ls).is_some());
            assert_eq!(read, defined, "{label}");
        }
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
        assert_eq!(parse_ctx.fuel(), fuel(&group) + 4000);
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
        assert_eq!(parse_ctx.fuel(), fuel(group) + 3);
    }

    #[test]
    fn a_paragraph_style_reads_its_hiding_and_a_character_style_stays_unread() {
        let group = r"{\s1\v A;}{\*\cs5\v B;}{\s2\v0 C;}{\s3\sbasedon1 D;}{\s4 E;}{\s6\v1\v0 F;}";
        let styles = stylesheet(&ParseCtx::default(), group).unwrap();
        let hidden: Vec<_> = [1, 2, 3, 4, 6].iter().map(|id| styles[id].delta.hidden).collect();
        assert_eq!(hidden, [Some(true), Some(false), Some(true), None, Some(false)]);
        assert!(!styles.contains_key(&5));
    }

    fn cycles(parse_ctx: ParseCtx<'_>) -> u64 {
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
        assert_eq!(parse_ctx.fuel(), fuel(&group) + count);
        assert_eq!(cycles(parse_ctx), 1);
    }
}
