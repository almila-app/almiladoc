//! RTF frontend: a position-explicit lexer feeding a state machine in one
//! pass, with the font/style/list tables parsed into typed definitions as
//! their groups close. Numbering comes from the list tables - never guessed
//! from label text. Page headers/footers are excluded (fixed policy).

mod lexer;
mod table;
mod tables;

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{
    Block, Document, ImageSource, Inline, LinkTarget, Note, NoteKind, Style, VerticalPosition,
    inlines_are_empty,
};
use crate::package::xml::{Element, Node, ns};
use crate::shared::blockstyle::{BlockStyle, StyledRun};
use crate::shared::codepage::{self, Codepage, TextLoss};
use crate::shared::delta::rebase_emphasis;
use crate::shared::fields::{field_result, hyperlink_target_spans, is_choice_form_field};
use crate::shared::list::{ListEntry, ListKey, ListMark, MarkerKind, flush_list};
use crate::shared::math::{DROPPED, GUESSED, math_lines, omath_para_to_tex};
use crate::shared::numbering::{LevelCounters, composite_label, every_deeper};
use crate::shared::symbol::{self, Decoded, Glyph};
use crate::shared::text::clean_text;
use lexer::{Lexer, Token, hex_digit};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use table::TableState;
use tables::{Face, Font, LIST_LEVELS, Prelude, TableScan, outline_level};

pub(super) fn parse(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Document, ConvertError> {
    if !bytes.starts_with(b"{\\rtf") {
        return Err(ConvertError::malformed("not an RTF file"));
    }
    let mut parser = Parser::new(parse_ctx, bytes);
    parser.run()?;
    parser.finish()
}

#[derive(Clone, Copy, PartialEq)]
enum Capture {
    None,
    ListText,
    FieldInstr,
    Bookmark,
    /// Inside a `\pict` destination: bytes route to the picture collector.
    Pict,
    PicturePropertyName,
    PicturePropertyValue,
    /// Inside a math zone: text routes to the math element under
    /// construction.
    Math,
}

#[derive(Clone, Copy)]
struct CharState {
    style: Style,
    font: Option<i32>,
    uc_skip: u32,
    in_table: bool,
    itap: usize,
    ilvl: usize,
    ls: Option<i32>,
    legacy_list: Option<MarkerKind>,
    outline: Option<u8>,
    /// The block container this paragraph's style names.
    block: Option<BlockStyle>,
    /// Emphasis the paragraph style itself carries, subtracted from headings.
    style_base: Style,
    suppress: bool,
    /// Inside content left out of the output, which no inner destination
    /// brings back.
    omit: bool,
    /// Text marked deleted, which accepted revisions drop.
    deleted: bool,
    /// Hidden text, which the output leaves out.
    hidden: bool,
    capture: Capture,
    in_note: bool,
}

impl CharState {
    fn row_depth(&self) -> usize {
        self.itap.max(1)
    }

    /// The table depth of the paragraph, 0 outside tables.
    fn container(&self) -> usize {
        if self.in_table { self.row_depth() } else { 0 }
    }
}

impl Default for CharState {
    fn default() -> Self {
        Self {
            style: Style::PLAIN,
            font: None,
            uc_skip: 1,
            in_table: false,
            itap: 1,
            ilvl: 0,
            ls: None,
            legacy_list: None,
            outline: None,
            block: None,
            style_base: Style::PLAIN,
            suppress: false,
            omit: false,
            deleted: false,
            hidden: false,
            capture: Capture::None,
            in_note: false,
        }
    }
}

struct FieldFrame {
    depth: usize,
    instr: String,
    /// What each damaged character of the instruction lost.
    losses: Vec<InstrLoss>,
    start: usize,
    /// Whether the field opened in hidden text.
    opened_hidden: bool,
}

impl FieldFrame {
    fn push_text(&mut self, raw: &str, loss: TextLoss) {
        let mut prev = None;
        for c in raw.chars() {
            let after_cr = prev.replace(c) == Some('\r');
            if c == '\n' && after_cr {
                continue;
            }
            let piece = clean_text(c.encode_utf8(&mut [0; 4]));
            let lost = char_loss(c, loss);
            if lost.invalid || lost.assumed {
                self.losses.push(InstrLoss {
                    at: self.instr.len(),
                    removed: piece.is_empty(),
                    loss: lost,
                });
            }
            self.instr.push_str(&piece);
        }
    }
}

/// A character of a field instruction that decoding damaged.
struct InstrLoss {
    /// Its byte offset in the cleaned instruction; one that cleaning
    /// removed takes the offset where it would have sat.
    at: usize,
    removed: bool,
    loss: TextLoss,
}

impl InstrLoss {
    /// Whether the character sits in `span`, or was removed at its end.
    fn within(&self, span: &std::ops::Range<usize>) -> bool {
        span.contains(&self.at) || (self.removed && self.at == span.end)
    }
}

/// What decoding with `loss` did to the character `c`.
fn char_loss(c: char, loss: TextLoss) -> TextLoss {
    TextLoss {
        invalid: loss.invalid && c == char::REPLACEMENT_CHARACTER,
        assumed: loss.assumed && !c.is_ascii(),
    }
}

/// `text` with what it lost in decoding marked before each character it
/// touched, for a capture that finds out later whether the text shows; the
/// source's own marker characters are left out, so they mark nothing. It
/// runs on the decoded text before `clean_text`, which keeps the markers, so
/// a character that cleaning removes or makes ASCII is still marked.
fn mark_loss(text: &str, loss: TextLoss) -> String {
    let mut marked = String::with_capacity(text.len());
    for c in text.chars().filter(|&c| c != GUESSED && c != DROPPED) {
        let lost = char_loss(c, loss);
        if lost.invalid {
            marked.push(DROPPED);
        }
        if lost.assumed {
            marked.push(GUESSED);
        }
        marked.push(c);
    }
    marked
}

struct NoteFrame {
    depth: usize,
    start: usize,
    kind: NoteKind,
}

/// The numbering sequence a list paragraph continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Sequence {
    /// A list-table list, by its `\lsN`.
    Table(i32),
    /// Legacy `\pn` numbering, one sequence for the document.
    Legacy,
}

/// The list identity of legacy `\pn` paragraphs, above every `\lsN`'s.
const LEGACY_LIST: u64 = u64::MAX;
/// The list identity of a bare `\listtext`, above every `\lsN`'s.
const LISTTEXT_LIST: u64 = u64::MAX - 1;

/// The list identity of `\lsN`: every `i32` keeps its own.
fn table_list(ls: i32) -> u64 {
    u64::from(ls.cast_unsigned())
}

type Levels = LevelCounters<LIST_LEVELS>;

/// Per-sequence numbering counters with restart-on-shallower semantics.
#[derive(Default)]
struct Counters {
    state: HashMap<Sequence, Levels>,
}

impl Counters {
    fn next(&mut self, sequence: Sequence, level: usize, start: u64) -> u64 {
        self.state.entry(sequence).or_default().next(level, start, every_deeper)
    }

    /// Advance a table-defined list level and render its composite label
    /// against the live counter values, with what the pattern's literal
    /// text lost; a lossy literal never matches the default label, so the
    /// label it is in always shows.
    fn next_labeled(
        &mut self,
        ls: i32,
        level: usize,
        levels: &[tables::ListLevelDef; LIST_LEVELS],
    ) -> (u64, Option<String>, TextLoss) {
        let level = Levels::clamp(level);
        let def = &levels[level];
        let counters = self.state.entry(Sequence::Table(ls)).or_default();
        let value = counters.next(level, def.start, every_deeper);
        let label = def.marker.and_then(|marker| {
            composite_label(
                &def.pattern,
                marker,
                value,
                |l| levels[Levels::clamp(l)].marker.unwrap_or(MarkerKind::Decimal),
                |l| counters.value_or(l, levels[Levels::clamp(l)].start),
            )
        });
        (value, label, def.loss)
    }

    /// Pin a counter to a number taken from the source (legacy `\pn` labels).
    fn seed(&mut self, sequence: Sequence, level: usize, value: u64) {
        self.state.entry(sequence).or_default().seed(level, value, every_deeper);
    }
}

/// Byte-level text decoding: pending code-page bytes, `\uN` fallback skips,
/// and surrogate pairing.
struct TextDecoder {
    default_encoding: Codepage,
    pending: Vec<u8>,
    skip: u32,
    surrogate: Option<u16>,
}

impl TextDecoder {
    fn new(default_encoding: Codepage) -> Self {
        Self { default_encoding, pending: Vec::new(), skip: 0, surrogate: None }
    }

    /// Buffer one text byte, honoring an active `\uN` fallback skip, and
    /// whether it was buffered.
    fn byte(&mut self, b: u8) -> bool {
        let buffered = !self.skip_char();
        if buffered {
            self.pending.push(b);
        }
        buffered
    }

    /// Discard a held high surrogate that something other than its pair
    /// followed, and whether one was held.
    fn interrupt(&mut self) -> bool {
        self.surrogate.take().is_some()
    }

    /// True when an active fallback skip consumed the character.
    fn skip_char(&mut self) -> bool {
        let skipped = self.skip > 0;
        self.skip = self.skip.saturating_sub(1);
        skipped
    }

    fn take_bytes(&mut self) -> Option<Vec<u8>> {
        Some(std::mem::take(&mut self.pending)).filter(|bytes| !bytes.is_empty())
    }

    /// Decode and take the buffered bytes with the current font's encoding,
    /// with what the text lost.
    fn take_pending(&mut self, encoding: Option<Codepage>) -> Option<(String, TextLoss)> {
        let bytes = self.take_bytes()?;
        Some(encoding.unwrap_or(self.default_encoding).decode(&bytes))
    }

    /// `\uN`: the completed scalar (surrogate pairs are held and combined),
    /// arming the `uc_skip`-length fallback skip, and whether a character
    /// was dropped: an unpaired surrogate or a value that is no scalar.
    fn unicode(&mut self, param: Option<i32>, uc_skip: u32) -> (Option<char>, bool) {
        // A new \u ends the previous one's fallback range.
        self.skip = 0;
        let Some(n) = param else {
            return (None, false);
        };
        let code = u32::try_from(if n < 0 { n + 65536 } else { n }).unwrap_or(u32::MAX);
        let out = match (self.surrogate.take(), u16::try_from(code).ok()) {
            (None, Some(unit @ 0xD800..=0xDBFF)) => {
                self.surrogate = Some(unit);
                (None, false)
            }
            (Some(high), Some(low @ 0xDC00..=0xDFFF)) => {
                (char::decode_utf16([high, low]).next().and_then(Result::ok), false)
            }
            (held, _) => {
                let c = char::from_u32(code);
                (c, held.is_some() || c.is_none())
            }
        };
        self.skip = uc_skip;
        out
    }
}

/// An accumulating `\pict` destination: the payload arrives as ASCII hex
/// characters (or one `\binN` run), typed by a format control word.
#[derive(Default)]
struct PictState {
    /// Group depth of the `\pict` destination itself: payload bytes live at
    /// this depth only (subgroups like `\*\picprop` carry properties).
    depth: usize,
    hex: Vec<u8>,
    alt: String,
    properties_depth: Option<usize>,
    property_depth: Option<usize>,
    property_name: String,
    property_value: String,
    binary: Option<Vec<u8>>,
    /// (media type, extension) from `\pngblip`/`\jpegblip`/`\emfblip`/
    /// `\wmetafile`; `None` = unsupported format.
    format: Option<(&'static str, &'static str)>,
}

impl PictState {
    /// Close the property groups the stack has unwound past `depth`.
    fn close_properties(&mut self, depth: usize) {
        if self.property_depth.is_some_and(|opened| depth < opened) {
            if self.property_name.trim() == "wzDescription" {
                self.alt = std::mem::take(&mut self.property_value);
            }
            self.property_depth = None;
        }
        if self.properties_depth.is_some_and(|opened| depth < opened) {
            self.properties_depth = None;
        }
    }

    fn payload(self) -> Vec<u8> {
        if let Some(binary) = self.binary {
            return binary;
        }
        let mut out = Vec::with_capacity(self.hex.len() / 2);
        let mut high: Option<u8> = None;
        for digit in self.hex.iter().copied().filter_map(hex_digit) {
            match high.take() {
                Some(h) => out.push((h << 4) | digit),
                None => high = Some(digit),
            }
        }
        out
    }
}

/// An accumulating `\mmath` zone. Its groups mirror OMML elements, each
/// named by its first control word (`\mf` is `m:f`), so they are built into
/// the same element tree the OOXML readers hand the converter: a property
/// group's text is its value, a run's text is its content, and a property
/// written as a parameter word on the run (`\msty2`) is a child element.
struct MathState {
    /// Group depth of the zone's own group.
    depth: usize,
    /// Open groups, innermost last, each with the depth it opened at. The
    /// first is the zone itself. Its text is marked by [`mark_loss`].
    open: Vec<(usize, Element)>,
}

impl MathState {
    fn new(depth: usize) -> Self {
        Self { depth, open: vec![(depth, math_elem("zone"))] }
    }

    fn open_group(&mut self, depth: usize) {
        self.open.push((depth, math_elem("")));
    }

    /// Attach the groups opened deeper than `depth` to their parents. A
    /// group that named no element is transparent.
    fn close_groups(&mut self, depth: usize) {
        while self.open.len() > 1
            && let Some((_, elem)) = self.open.pop_if(|(d, _)| *d > depth)
            && let Some((_, parent)) = self.open.last_mut()
        {
            if elem.local.is_empty() {
                parent.children.extend(elem.children);
            } else {
                parent.children.push(Node::Elem(elem));
            }
        }
    }

    /// A math control word inside a group: the first names the group, a
    /// later one is a property of it, its parameter the value.
    fn word(&mut self, name: &str, param: Option<i32>) {
        let Some((_, elem)) = self.open.last_mut() else { return };
        if elem.local.is_empty() {
            elem.local = name.to_string();
            return;
        }
        let mut prop = math_elem(name);
        if let Some(param) = param {
            prop.children.push(Node::Text(param.to_string()));
        }
        elem.children.push(Node::Elem(prop));
    }

    fn push_text(&mut self, text: String) {
        if let Some((_, elem)) = self.open.last_mut() {
            elem.children.push(Node::Text(text));
        }
    }

    fn mark_dropped(&mut self) {
        self.push_text(DROPPED.to_string());
    }

    /// The zone's equations, whether they are a math paragraph
    /// (`\moMathPara`), which displays on its own line, and what the
    /// displayed equations lost. The zone is converted once with the
    /// [`mark_loss`] markers in its text, and a loss counts when its marker
    /// is in that output, which keeps a marker on run text and on a
    /// delimiter, accent, operator or group character it shows; the
    /// equations returned are converted again without the markers.
    fn finish(mut self) -> (Vec<String>, bool, TextLoss) {
        self.close_groups(0);
        let (_, mut zone) = self.open.remove(0);
        let convert = |zone: &Element| match zone.find(ns::M, "oMathPara") {
            Some(para) => (omath_para_to_tex(para), true),
            None => (omath_para_to_tex(zone), false),
        };
        let (marked, _) = convert(&zone);
        let shows = |c: char| marked.iter().any(|line| line.contains(c));
        let loss = TextLoss { invalid: shows(DROPPED), assumed: shows(GUESSED) };
        strip_markers(&mut zone);
        let (lines, display) = convert(&zone);
        (lines, display, loss)
    }
}

fn strip_markers(root: &mut Element) {
    let mut stack = vec![root];
    while let Some(elem) = stack.pop() {
        for node in &mut elem.children {
            match node {
                Node::Text(text) => text.retain(|c| c != GUESSED && c != DROPPED),
                Node::Elem(child) => stack.push(child),
            }
        }
    }
}

fn math_elem(local: &str) -> Element {
    Element {
        unresolved_text: false,
        text_damage: None,
        ns: Some(Rc::from(ns::M)),
        local: local.to_string(),
        attrs: Vec::new(),
        children: Vec::new(),
    }
}

/// Every OMML element name, as its rtf control word without the `m` prefix.
#[rustfmt::skip]
const OMML_NAMES: &[&str] = &[
    "acc", "accPr", "aln", "alnScr", "argPr", "argSz", "bar", "barPr", "baseJc", "begChr",
    "borderBox", "borderBoxPr", "box", "boxPr", "brk", "brkBin", "brkBinSub", "cGp", "cGpRule",
    "chr", "count", "cSp", "ctrlPr", "d", "defJc", "deg", "degHide", "den", "diff", "dispDef",
    "dPr", "e", "endChr", "eqArr", "eqArrPr", "f", "fName", "fPr", "func", "funcPr", "groupChr",
    "groupChrPr", "grow", "hideBot", "hideLeft", "hideRight", "hideTop", "interSp", "intLim",
    "intraSp", "jc", "lim", "limLoc", "limLow", "limLowPr", "limUpp", "limUppPr", "lit", "lMargin",
    "m", "mathFont", "mathPr", "maxDist", "mc", "mcJc", "mcPr", "mcs", "mPr", "mr", "nary",
    "naryLim", "naryPr", "noBreak", "nor", "num", "objDist", "oMath", "oMathPara", "oMathParaPr",
    "opEmu", "phant", "phantPr", "plcHide", "pos", "postSp", "preSp", "r", "rad", "radPr",
    "rMargin", "rPr", "rSp", "rSpRule", "scr", "sepChr", "show", "shp", "smallFrac", "sPre",
    "sPrePr", "sSub", "sSubPr", "sSubSup", "sSubSupPr", "sSup", "sSupPr", "strikeBLTR", "strikeH",
    "strikeTLBR", "strikeV", "sty", "sub", "subHide", "sup", "supHide", "t", "transp", "type",
    "vertJc", "wrapIndent", "wrapRight", "zeroAsc", "zeroDesc", "zeroWid",
];

/// Field, note, bookmark, and list-label destination state: frames open at a
/// group depth and close when the group stack unwinds past it.
#[derive(Default)]
struct Destinations {
    fields: Vec<FieldFrame>,
    note_frames: Vec<NoteFrame>,
    notes: Vec<Note>,
    bookmark: String,
    listtext: Option<String>,
    /// What the captured label lost in decoding.
    listtext_loss: TextLoss,
    pict: Option<PictState>,
    math: Option<MathState>,
    math_display: bool,
}

impl Destinations {
    fn clear_paragraph_state(&mut self) {
        self.listtext = None;
        self.listtext_loss = TextLoss::default();
        self.math_display = false;
    }

    /// Close note frames opened deeper than `depth`, replacing their content
    /// with a reference to the collected note.
    fn close_notes(&mut self, depth: usize, inlines: &mut Vec<Inline>) {
        while let Some(frame) = self.note_frames.pop_if(|f| f.depth > depth) {
            let start = frame.start.min(inlines.len());
            let content = inlines.split_off(start);
            if !inlines_are_empty(&content) {
                let id = format!("rtf{}", self.notes.len());
                self.notes.push(Note {
                    id: id.clone(),
                    kind: frame.kind,
                    blocks: vec![Block::Paragraph(content)],
                });
                inlines.push(Inline::NoteRef(id));
            }
        }
    }

    /// Emit a completed bookmark capture as an anchor.
    fn close_bookmark(&mut self, still_capturing: bool, inlines: &mut Vec<Inline>) {
        if still_capturing {
            return;
        }
        let name = std::mem::take(&mut self.bookmark);
        let name = name.trim();
        if !name.is_empty() {
            inlines.push(Inline::Anchor(name.to_string()));
        }
    }
}

/// Destinations whose content is excluded: headers/footers by fixed policy,
/// metadata and binary destinations because they carry no document text.
#[rustfmt::skip]
const SUPPRESSED_DESTINATIONS: &[&str] = &[
    "fonttbl", "colortbl", "stylesheet", "info", "object", "header", "footer", "headerl", "headerr",
    "headerf", "footerl", "footerr", "footerf", "ftnsep", "ftnsepc", "aftnsep", "aftnsepc",
    "xmlnstbl", "themedata", "colorschememapping", "datastore", "latentstyles", "listtable",
    "listoverridetable", "rsidtbl", "generator", "filetbl", "revtbl", "datafield", "formfield",
    "bkmkend", "annotation", "atnid", "atnauthor", "template", "defchp", "defpap", "panose", "falt",
    "objdata", "blipuid", "nonshppict", "wgrffmtfilter", "pgdsctbl", "docvar", "sp", "sn", "sv",
    "shpinst", "background", "userprops", "operator", "author", "title", "subject", "keywords",
    "doccomm", "creatim", "revtim", "printim",
];

struct Parser<'a> {
    parse_ctx: &'a ParseCtx<'a>,
    bytes: &'a [u8],
    lexer: Lexer<'a>,
    stack: Vec<CharState>,
    state: CharState,
    prelude: Prelude,
    tables: TableScan,
    decoder: TextDecoder,
    /// Whether `\ansicpg` has declared the code page; the first one holds.
    codepage_declared: bool,
    /// Whether `encoding_assumed` has been reported; it is reported once.
    assumed_reported: bool,
    /// The font `\deffN` names, for text no `\fN` sets.
    deff: Option<i32>,
    /// The face of the last symbol `\fN` (a font a glyph table names, or
    /// `\fcharset2`) text could be set in, which
    /// U+F020-F0FF in a text font is drawn through, as RTF 1.9.1 says.
    last_symbol: Option<Face>,
    /// The `\fN` the font table lacks that have been reported.
    undefined_fonts: HashSet<i32>,
    /// The list levels (`\ls`, level) whose damaged label text has been
    /// reported.
    labels_reported: HashSet<(i32, usize)>,
    /// The region of the deleted range the last text dropped or kept was
    /// in, so a deleted range is reported once in its region.
    deleted_range: Option<Region>,
    /// The region of the hidden range that last dropped something other
    /// than whitespace, so such a range is reported once in its region.
    hidden_range: Option<Region>,
    /// The state at the dropped mark the open paragraph continues from,
    /// whose container and properties it keeps if it ends before joining.
    held: Option<CharState>,

    inlines: Vec<Inline>,
    blocks: Vec<Block>,
    list_run: Vec<ListEntry>,
    styled: StyledRun,
    counters: Counters,
    table: TableState<'a>,
    dest: Destinations,
    assets: crate::shared::assets::AssetSink,
}

impl<'a> Parser<'a> {
    fn new(parse_ctx: &'a ParseCtx<'a>, bytes: &'a [u8]) -> Self {
        Self {
            parse_ctx,
            bytes,
            lexer: Lexer::new(parse_ctx, bytes),
            stack: Vec::new(),
            state: CharState::default(),
            prelude: Prelude::default(),
            tables: TableScan::default(),
            decoder: TextDecoder::new(codepage::windows(1252)),
            codepage_declared: false,
            assumed_reported: false,
            deff: None,
            last_symbol: None,
            undefined_fonts: HashSet::new(),
            labels_reported: HashSet::new(),
            deleted_range: None,
            hidden_range: None,
            held: None,
            inlines: Vec::new(),
            blocks: Vec::new(),
            list_run: Vec::new(),
            styled: StyledRun::default(),
            counters: Counters::default(),
            table: TableState::new(parse_ctx),
            dest: Destinations::default(),
            assets: crate::shared::assets::AssetSink::new(parse_ctx),
        }
    }

    /// The one lexing pass: the prelude's tables are read as their groups
    /// close, before the body that uses them.
    fn run(&mut self) -> Result<(), ConvertError> {
        while let Some(token) = self.lexer.next_token()? {
            let depth = self.stack.len() + usize::from(matches!(token, Token::Open));
            if let Some((table, body)) = self.tables.token(&token, depth, self.lexer.pos()) {
                let group = self.bytes.get(body).unwrap_or_default();
                let codepage = self.decoder.default_encoding;
                self.prelude.read(self.parse_ctx, table, group, codepage)?;
            }
            match token {
                Token::Open => {
                    let max = self.parse_ctx.limits().max_nesting_depth;
                    if self.stack.len() as u64 >= max {
                        return Err(ConvertError::limit(Limit::NestingDepth, max));
                    }
                    self.flush_pending();
                    self.stack.push(self.state);
                    if self.state.capture == Capture::Math
                        && let Some(math) = &mut self.dest.math
                    {
                        math.open_group(self.stack.len());
                    }
                }
                Token::Close => self.close_group()?,
                Token::Word { name, param } => self.control_word(name, param)?,
                Token::Symbol(b) => self.control_symbol(b),
                Token::Hex(b) | Token::Byte(b) => {
                    if let Some(pict) = self.pict_payload() {
                        pict.hex.push(b);
                    } else if self.state.capture != Capture::Pict
                        && self.accepts_text()
                        && self.decoder.byte(b)
                    {
                        self.settle_surrogate();
                    }
                }
                Token::Bin(payload) => {
                    if let Some(pict) = self.pict_payload() {
                        pict.binary = Some(payload.to_vec());
                    }
                }
            }
        }
        if !self.stack.is_empty() {
            self.parse_ctx.emit(DiagnosticCode::UnclosedStructure, Region::Body);
        }
        self.flush_pending();
        self.finish_math();
        self.end_paragraph()
    }

    fn close_group(&mut self) -> Result<(), ConvertError> {
        self.flush_pending();
        match self.stack.pop() {
            Some(prev) => self.state = prev,
            None => self.parse_ctx.emit(DiagnosticCode::UnmatchedClose, Region::Body),
        }
        let depth = self.stack.len();
        self.close_fields(depth);
        self.dest.close_notes(depth, &mut self.inlines);
        self.dest.close_bookmark(self.state.capture == Capture::Bookmark, &mut self.inlines);
        // Inner property groups close before the picture finishes.
        if let Some(pict) = &mut self.dest.pict {
            pict.close_properties(depth);
        }
        if self.dest.pict.as_ref().is_some_and(|pict| depth < pict.depth) {
            self.finish_pict()?;
        }
        if let Some(math) = &mut self.dest.math {
            if depth < math.depth {
                self.finish_math();
            } else {
                math.close_groups(depth);
            }
        }
        Ok(())
    }

    /// The picture collector while its own group takes payload bytes.
    fn pict_payload(&mut self) -> Option<&mut PictState> {
        let depth = self.stack.len();
        let capturing = self.state.capture == Capture::Pict;
        self.dest.pict.as_mut().filter(|pict| capturing && pict.depth == depth)
    }

    fn accepts_text(&self) -> bool {
        !self.state.omit && (self.state.capture != Capture::None || !self.state.suppress)
    }

    /// Whether body text in the current state reaches the output.
    fn renders(&self) -> bool {
        self.state.capture == Capture::None
            && !self.state.suppress
            && !self.state.deleted
            && !self.state.hidden
    }

    fn region(&self) -> Region {
        if self.state.in_note { Region::Note } else { Region::Body }
    }

    /// Report `loss` in text that reaches the output in `region`.
    fn report(&mut self, loss: TextLoss, region: Region) {
        if loss.invalid {
            self.parse_ctx.emit(DiagnosticCode::InvalidEncoding, region);
        }
        if loss.assumed && !self.assumed_reported {
            self.assumed_reported = true;
            self.parse_ctx.emit(DiagnosticCode::EncodingAssumed, region);
        }
    }

    /// Route `loss` with the text the current capture takes: reported for
    /// rendered body text, held with a list label until it is shown, and
    /// dropped with a bookmark name or text that does not render. A field
    /// instruction records its own by offset, and an equation marks its own
    /// in its text.
    fn text_loss(&mut self, loss: TextLoss) {
        match self.state.capture {
            Capture::ListText => self.dest.listtext_loss |= loss,
            Capture::None if self.renders() => self.report(loss, self.region()),
            Capture::None
            | Capture::FieldInstr
            | Capture::Bookmark
            | Capture::Pict
            | Capture::PicturePropertyName
            | Capture::PicturePropertyValue
            | Capture::Math => {}
        }
    }

    /// Whether the current capture takes deleted text, which accepted
    /// revisions drop from a field instruction and an equation.
    fn captures_deleted(&self) -> bool {
        self.state.deleted && matches!(self.state.capture, Capture::FieldInstr | Capture::Math)
    }

    /// Whether the current capture takes hidden text, which an equation
    /// drops and a field instruction keeps.
    fn captures_hidden(&self) -> bool {
        self.state.hidden && self.state.capture == Capture::Math
    }

    /// Account for a character dropped because it did not decode.
    fn drop_char(&mut self) {
        if self.captures_deleted() || self.captures_hidden() {
            return;
        }
        match self.state.capture {
            Capture::FieldInstr => {
                if let Some(field) = self.dest.fields.last_mut() {
                    field.losses.push(InstrLoss {
                        at: field.instr.len(),
                        removed: true,
                        loss: TextLoss { invalid: true, assumed: false },
                    });
                }
            }
            Capture::Math => {
                if let Some(math) = &mut self.dest.math {
                    math.mark_dropped();
                }
            }
            _ => self.text_loss(TextLoss { invalid: true, assumed: false }),
        }
    }

    /// Drop a held high surrogate that its pair did not follow.
    fn settle_surrogate(&mut self) {
        if self.decoder.interrupt() {
            self.drop_char();
        }
    }

    /// Drop deleted content, reporting each deleted range once.
    fn drop_deleted(&mut self) {
        self.hidden_range = None;
        let region = self.region();
        if self.deleted_range != Some(region) {
            self.deleted_range = Some(region);
            self.parse_ctx.emit(DiagnosticCode::RevisionOmitted, region);
        }
    }

    /// Leave out a structure that opens in deleted text, reporting its
    /// range, and whether it did.
    fn omit_deleted(&mut self) -> bool {
        self.state.deleted && self.omit(Self::drop_deleted)
    }

    fn omit(&mut self, drop_range: fn(&mut Self)) -> bool {
        if !self.state.suppress {
            drop_range(self);
        }
        self.state.suppress = true;
        self.state.omit = true;
        true
    }

    fn drop_hidden(&mut self) {
        self.deleted_range = None;
        let region = self.region();
        if self.hidden_range != Some(region) {
            self.hidden_range = Some(region);
            self.parse_ctx.emit(DiagnosticCode::HiddenOmitted, region);
        }
    }

    /// Drop hidden text read as `tokens` and `unmapped` codes; whitespace
    /// alone reports nothing.
    fn drop_hidden_text(&mut self, tokens: &[symbol::Token], unmapped: u64) {
        let any = unmapped > 0
            || tokens.iter().any(|token| !matches!(token, symbol::Token::Text(t) if t.is_empty()));
        if any {
            self.deleted_range = None;
        }
        if shows(tokens, unmapped) {
            self.drop_hidden();
        }
    }

    /// Leave out a structure that opens in hidden text, reporting its
    /// range, and whether it did; deleted text is left to `omit_deleted`.
    fn omit_hidden(&mut self) -> bool {
        self.state.hidden && !self.state.deleted && self.omit(Self::drop_hidden)
    }

    fn close_ranges(&mut self) {
        self.deleted_range = None;
        self.hidden_range = None;
    }

    /// Report what a suppressed destination takes from the output, judged
    /// by the group that holds it (a `\*` has already suppressed its own),
    /// and whether policy leaves its whole content out.
    fn report_destination(&self, word: &str) -> bool {
        let (code, region) = match word {
            "header" | "headerl" | "headerr" | "headerf" => {
                (DiagnosticCode::HeaderOmitted, Region::Header)
            }
            "footer" | "footerl" | "footerr" | "footerf" => {
                (DiagnosticCode::FooterOmitted, Region::Footer)
            }
            "annotation" => (DiagnosticCode::CommentOmitted, Region::Comment),
            "object" => (DiagnosticCode::EmbeddedObject, self.region()),
            _ => return false,
        };
        // A comment's group is not suppressed by its hidden anchor, so the
        // holder test alone would report a hidden comment.
        let anchored_hidden = code == DiagnosticCode::CommentOmitted && self.state.hidden;
        if self.stack.last().is_some_and(|holder| !holder.suppress) && !anchored_hidden {
            self.parse_ctx.emit(code, region);
        }
        code != DiagnosticCode::EmbeddedObject
    }

    /// Close field frames opened deeper than `depth`, folding their results
    /// back into the inline stream.
    fn close_fields(&mut self, depth: usize) {
        while let Some(frame) = self.dest.fields.pop_if(|f| f.depth > depth) {
            let start = frame.start.min(self.inlines.len());
            let content = self.inlines.split_off(start);
            let region = self.region();
            if is_choice_form_field(&frame.instr) {
                if frame.opened_hidden {
                    self.drop_hidden();
                } else {
                    self.parse_ctx.emit(DiagnosticCode::FormControlUnread, region);
                }
            }
            if !inlines_are_empty(&content)
                && let Some((target, spans)) = hyperlink_target_spans(&frame.instr)
            {
                // An anchor shows only as a slug or not at all: a damaged
                // character it keeps never reaches the output.
                let anchor = matches!(target, LinkTarget::Anchor { .. });
                let mut loss = TextLoss::default();
                for lost in &frame.losses {
                    if (lost.removed || !anchor) && spans.iter().any(|span| lost.within(span)) {
                        loss |= lost.loss;
                    }
                }
                self.report(loss, region);
            }
            self.inlines.extend(field_result(&frame.instr, content));
        }
    }

    fn control_symbol(&mut self, b: u8) {
        match b {
            b'~' => self.push_generated('\u{a0}'),
            b'_' => self.push_generated('-'),
            b'*' => self.state.suppress = true,
            b'\\' | b'{' | b'}' => self.push_char(b as char),
            _ => {}
        }
    }

    /// Dispatch a control word to its subsystem handler; unhandled words
    /// that name a suppressed destination silence their group.
    fn control_word(&mut self, word: &str, param: Option<i32>) -> Result<(), ConvertError> {
        let handled = self.codepage_control(word, param)
            || self.picture_property_control(word)
            || self.math_control(word, param)
            || self.character_control(word, param)
            || self.paragraph_control(word, param)?
            || self.table_control(word, param)?
            || self.list_control(word, param)
            || self.object_control(word)?;
        if handled || !SUPPRESSED_DESTINATIONS.contains(&word) {
            return Ok(());
        }
        self.flush_pending();
        if !(word == "object" && (self.omit_deleted() || self.omit_hidden())) {
            self.state.omit |= self.report_destination(word);
        }
        self.state.suppress = true;
        self.state.capture = Capture::None;
        Ok(())
    }

    /// The document's code page: its first `\ansicpg`, else the last of
    /// `\mac` (10000), `\pc` (437) and `\pca` (850); a document naming no
    /// character set reads as declared windows-1252, RTF's `\ansi` default.
    fn codepage_control(&mut self, word: &str, param: Option<i32>) -> bool {
        let codepage = match (word, param) {
            ("ansicpg", Some(cp)) => codepage::windows(u16::try_from(cp).unwrap_or(0)),
            ("mac", _) => codepage::windows(10000),
            ("pc", _) => codepage::windows(437),
            ("pca", _) => codepage::windows(850),
            _ => return false,
        };
        if !self.codepage_declared {
            self.flush_pending();
            self.codepage_declared = word == "ansicpg";
            self.decoder.default_encoding = codepage;
        }
        true
    }

    fn character_control(&mut self, word: &str, param: Option<i32>) -> bool {
        let on = param != Some(0);
        match word {
            "u" => {
                if self.accepts_text() {
                    self.flush_text();
                    let (c, dropped) = self.decoder.unicode(param, self.state.uc_skip);
                    if dropped {
                        self.drop_char();
                    }
                    if let Some(c) = c {
                        self.push_text(c.encode_utf8(&mut [0; 4]), TextLoss::default());
                    }
                }
            }
            "uc" => self.state.uc_skip = u32::try_from(param.unwrap_or(1)).unwrap_or(0),
            "f" => {
                self.flush_pending();
                self.state.font = param;
                let face = param.and_then(|n| self.prelude.fonts.get(&n)).map(|font| font.face);
                if self.accepts_text() && matches!(face, Some(Face::Glyphs(_) | Face::Charset2)) {
                    self.last_symbol = face;
                }
            }
            "deff" => {
                self.flush_pending();
                self.deff = param;
            }
            "b" => self.set_style(|s| s.bold = on),
            "i" => self.set_style(|s| s.italic = on),
            "strike" | "striked" => self.set_style(|s| s.strike = on),
            "super" => self.set_style(|s| s.position = VerticalPosition::Superscript),
            "sub" => self.set_style(|s| s.position = VerticalPosition::Subscript),
            "nosupersub" => self.set_style(|s| s.position = VerticalPosition::Baseline),
            "plain" => {
                self.flush_pending();
                self.state.style = Style::PLAIN;
                self.state.font = None;
                self.state.deleted = false;
                self.state.hidden = false;
            }
            "deleted" => {
                self.flush_pending();
                self.state.deleted = on;
            }
            "v" => {
                self.flush_pending();
                self.state.hidden = on;
            }
            _ => return false,
        }
        true
    }

    fn paragraph_control(&mut self, word: &str, param: Option<i32>) -> Result<bool, ConvertError> {
        match word {
            "s" => {
                let def = param.and_then(|id| self.prelude.styles.get(&id).copied());
                if let Some(def) = def {
                    self.flush_pending();
                    self.state.outline = def.outline;
                    self.state.block = def.block;
                    self.state.style = def.delta.apply(self.state.style);
                    self.state.style_base = self.state.style;
                    self.state.hidden = def.delta.hidden.unwrap_or(self.state.hidden);
                }
            }
            "par" | "sect" => self.paragraph_mark(word == "sect")?,
            "pard" => {
                self.flush_pending();
                self.state.in_table = false;
                self.state.itap = 1;
                self.state.ilvl = 0;
                self.state.ls = None;
                self.state.legacy_list = None;
                self.state.outline = None;
                self.state.block = None;
                self.state.style_base = Style::PLAIN;
            }
            // \page and \column break the flow without ending the
            // paragraph; the page they start is unrepresentable, the word
            // boundary they carry is not.
            "line" | "lbr" | "page" | "column" => {
                self.flush_pending();
                if !self.state.suppress {
                    if self.state.deleted {
                        self.drop_deleted();
                    } else if !self.state.hidden {
                        if matches!(word, "page" | "column") && !self.state.in_note {
                            self.release_held(None)?;
                        }
                        self.inlines.push(Inline::LineBreak);
                    }
                }
            }
            "tab" | "enspace" | "emspace" | "qmspace" => self.push_generated(' '),
            "emdash" => self.push_generated('\u{2014}'),
            "endash" => self.push_generated('\u{2013}'),
            "lquote" => self.push_generated('\u{2018}'),
            "rquote" => self.push_generated('\u{2019}'),
            "ldblquote" => self.push_generated('\u{201c}'),
            "rdblquote" => self.push_generated('\u{201d}'),
            "bullet" => self.push_generated('\u{2022}'),
            "chpgn" | "chdate" | "chtime" | "chdpl" | "chdpa" => {
                if self.renders() {
                    self.parse_ctx.emit(DiagnosticCode::FieldValueUnread, self.region());
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// `\par` or `\sect`: end the paragraph, or drop a deleted or hidden
    /// mark other than a section break, joining the next paragraph.
    fn paragraph_mark(&mut self, section: bool) -> Result<(), ConvertError> {
        self.flush_pending();
        let dropped = self.state.deleted || self.state.hidden;
        if dropped && self.state.suppress {
            return Ok(());
        }
        if self.state.deleted {
            self.drop_deleted();
        }
        if !self.state.suppress {
            self.close_ranges();
        }
        if dropped && !section {
            if !self.state.in_note {
                self.held = Some(self.state);
            }
        } else if self.state.in_note {
            self.inlines.push(Inline::LineBreak);
        } else if !self.state.suppress {
            self.end_paragraph()?;
        }
        Ok(())
    }

    /// End a held paragraph as it stands, in its own container and with its
    /// own properties, unless it continues in `container`.
    fn release_held(&mut self, container: Option<usize>) -> Result<(), ConvertError> {
        let Some(held) = self.held else {
            return Ok(());
        };
        if container == Some(held.container()) {
            return Ok(());
        }
        self.held = None;
        self.flush_pending();
        if inlines_are_empty(&self.inlines) {
            return Ok(());
        }
        let current = std::mem::replace(&mut self.state, held);
        let ended = self.end_paragraph();
        self.state = current;
        ended
    }

    /// Release a held paragraph that the container now in force does not
    /// continue.
    fn enter_container(&mut self) -> Result<(), ConvertError> {
        if self.table_active() {
            self.release_held(Some(self.state.container()))?;
        }
        Ok(())
    }

    fn table_control(&mut self, word: &str, param: Option<i32>) -> Result<bool, ConvertError> {
        match word {
            "intbl" => {
                self.state.in_table = true;
                self.enter_container()?;
            }
            "itap" => {
                self.state.itap = usize::try_from(param.unwrap_or(1).clamp(0, 8)).unwrap_or(0);
                if self.state.itap > 1 {
                    self.state.in_table = true;
                }
                self.enter_container()?;
            }
            "trowd" => {
                if self.table_active() {
                    self.release_held(None)?;
                    self.table.begin_row(self.state.row_depth());
                }
            }
            "trhdr" => self.in_row(TableState::mark_header_row),
            "clmgf" => self.in_row(|table, depth| table.pending_prop(depth).merge_first = true),
            "clmrg" => self.in_row(|table, depth| table.pending_prop(depth).merge_cont = true),
            "clvmgf" => self.in_row(|table, depth| table.pending_prop(depth).vmerge_first = true),
            "clvmrg" => self.in_row(|table, depth| table.pending_prop(depth).vmerge_cont = true),
            "cellx" => {
                let right = i64::from(param.unwrap_or(0));
                self.in_row(|table, depth| table.declare_cell(depth, right));
            }
            "cell" | "nestcell" => {
                self.flush_pending();
                if self.table_active() {
                    let depth = if word == "cell" { 1 } else { self.state.itap.max(2) };
                    self.release_held(Some(depth))?;
                    self.end_cell(depth)?;
                }
            }
            "row" | "nestrow" => {
                self.flush_pending();
                if self.table_active() {
                    self.end_row(if word == "row" { 1 } else { self.state.itap.max(2) })?;
                }
            }
            // Nested row properties arrive in a `{\*\nesttableprops ...}`
            // destination; its \trowd/\cellx/\nestrow must still act.
            "nesttableprops" => self.state.suppress = self.state.omit,
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn list_control(&mut self, word: &str, param: Option<i32>) -> bool {
        match word {
            "outlinelevel" => {
                if let Some(level) = param.and_then(outline_level) {
                    self.state.outline = Some(level);
                }
            }
            "ilvl" => {
                self.state.ilvl = usize::try_from(param.unwrap_or(0).clamp(0, 8)).unwrap_or(0);
            }
            "ls" => self.state.ls = param,
            "listtext" | "pntext" => {
                self.flush_pending();
                self.state.capture = Capture::ListText;
                self.dest.listtext.get_or_insert_default();
            }
            "pnlvlblt" => self.state.legacy_list = Some(MarkerKind::Bullet),
            "pnlvlbody" | "pndec" => self.state.legacy_list = Some(MarkerKind::Decimal),
            _ => return false,
        }
        true
    }

    fn open_field(&mut self) -> Result<(), ConvertError> {
        self.flush_pending();
        self.omit_deleted();
        if self.state.suppress {
            return Ok(());
        }
        let max = self.parse_ctx.limits().max_nesting_depth;
        if self.dest.fields.len() as u64 >= max {
            return Err(ConvertError::limit(Limit::NestingDepth, max));
        }
        self.dest.fields.push(FieldFrame {
            depth: self.stack.len(),
            instr: String::new(),
            losses: Vec::new(),
            start: self.inlines.len(),
            opened_hidden: self.state.hidden,
        });
        Ok(())
    }

    fn object_control(&mut self, word: &str) -> Result<bool, ConvertError> {
        match word {
            "field" => self.open_field()?,
            "fldinst" => {
                self.flush_pending();
                if !self.dest.fields.is_empty() {
                    self.state.capture = Capture::FieldInstr;
                }
                self.state.suppress = true;
            }
            "fldrslt" => {
                self.flush_pending();
                self.state.capture = Capture::None;
            }
            "footnote" => {
                self.flush_pending();
                self.omit_hidden();
                self.state.in_note = true;
                self.state.suppress = self.state.omit;
                self.state.capture = Capture::None;
                self.dest.note_frames.push(NoteFrame {
                    depth: self.stack.len(),
                    start: self.inlines.len(),
                    kind: NoteKind::Footnote,
                });
            }
            "ftnalt" => {
                // Marks the enclosing \footnote as an endnote.
                if let Some(frame) = self.dest.note_frames.last_mut() {
                    frame.kind = NoteKind::Endnote;
                }
            }
            "chftn" => {}
            "bkmkstart" => {
                self.flush_pending();
                self.state.capture = Capture::Bookmark;
                self.state.suppress = self.state.omit;
            }
            // Destinations inside a suppressed group that still hold content:
            // the preferred picture of `\shppict` (its `\nonshppict` fallback
            // duplicate stays suppressed), the text of a `\shpinst` shape and the
            // rendering of an `\object`.
            "shppict" | "shptxt" | "result" => self.state.suppress = self.state.omit,
            "pict" => {
                self.flush_pending();
                self.omit_deleted();
                self.omit_hidden();
                if !self.state.suppress {
                    self.state.capture = Capture::Pict;
                    self.dest.pict =
                        Some(PictState { depth: self.stack.len(), ..PictState::default() });
                }
            }
            "pngblip" => self.set_pict_format(Some(("image/png", "png"))),
            "jpegblip" => self.set_pict_format(Some(("image/jpeg", "jpg"))),
            "emfblip" => self.set_pict_format(Some(("image/emf", "emf"))),
            "wmetafile" => self.set_pict_format(Some(("image/wmf", "wmf"))),
            "macpict" | "dibitmap" | "wbitmap" => self.set_pict_format(None),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Math zone controls: `\mmath` opens a zone, and inside one every math
    /// control word names the group it starts or sets a property on it.
    /// The `\mmathPict` fallback picture is skipped.
    fn math_control(&mut self, word: &str, param: Option<i32>) -> bool {
        if self.dest.math.is_none() {
            if word != "mmath" {
                return false;
            }
            self.flush_pending();
            self.omit_deleted();
            if !self.state.suppress {
                self.state.capture = Capture::Math;
                self.dest.math = Some(MathState::new(self.stack.len()));
            }
            return true;
        }
        if self.state.capture != Capture::Math {
            return false;
        }
        if word == "mmathPict" {
            self.flush_pending();
            self.state.capture = Capture::None;
            self.state.suppress = true;
            return true;
        }
        match word.strip_prefix('m') {
            Some(name) if OMML_NAMES.contains(&name) => {
                self.flush_pending();
                if self.state.deleted && !self.state.omit {
                    self.drop_deleted();
                    self.state.omit = true;
                } else if self.state.hidden && !self.state.omit {
                    self.drop_hidden();
                    self.state.omit = true;
                }
                if self.state.omit {
                    return true;
                }
                if let Some(math) = &mut self.dest.math {
                    math.word(name, param);
                }
                true
            }
            _ => false,
        }
    }

    fn finish_math(&mut self) {
        let Some(math) = self.dest.math.take() else {
            return;
        };
        let (lines, display, loss) = math.finish();
        self.report(loss, self.region());
        for (i, tex) in lines.into_iter().enumerate() {
            if i > 0 {
                self.inlines.push(Inline::LineBreak);
            }
            self.inlines.push(Inline::Math(tex));
        }
        self.dest.math_display |= display;
    }

    fn set_pict_format(&mut self, format: Option<(&'static str, &'static str)>) {
        if self.state.capture == Capture::Pict
            && let Some(pict) = &mut self.dest.pict
        {
            pict.format = format;
        }
    }

    fn picture_property_control(&mut self, word: &str) -> bool {
        let Some(pict) = &self.dest.pict else { return false };
        let depth = self.stack.len();
        let recognized = match word {
            "picprop" => depth == pict.depth + 1,
            "sp" => pict.properties_depth.is_some_and(|opened| depth == opened + 1),
            "sn" | "sv" => pict.property_depth.is_some_and(|opened| depth == opened + 1),
            _ => false,
        };
        if !recognized {
            return false;
        }
        self.flush_pending();
        let Some(pict) = &mut self.dest.pict else { return false };
        match word {
            "picprop" => pict.properties_depth = Some(depth),
            "sp" => {
                pict.property_depth = Some(depth);
                pict.property_name.clear();
                pict.property_value.clear();
            }
            "sn" => self.state.capture = Capture::PicturePropertyName,
            "sv" => self.state.capture = Capture::PicturePropertyValue,
            _ => {}
        }
        true
    }

    fn finish_pict(&mut self) -> Result<(), ConvertError> {
        let Some(mut pict) = self.dest.pict.take() else {
            return Ok(());
        };
        let format = pict.format;
        let alt = std::mem::take(&mut pict.alt);
        let bytes = pict.payload();
        let source = match format.filter(|_| !bytes.is_empty()) {
            Some((media_type, extension)) => {
                let part = format!("pict/{}.{extension}", self.assets.assets.len());
                ImageSource::Asset(self.assets.add(media_type.to_string(), part, &bytes)?)
            }
            None => ImageSource::Unread,
        };
        let origin = self.parse_ctx.image_origin(self.region());
        self.inlines.push(Inline::Image { origin, alt, source });
        Ok(())
    }

    /// Table controls act outside suppressed groups and note bodies.
    fn table_active(&self) -> bool {
        !self.state.suppress && !self.state.in_note
    }

    fn in_row(&mut self, f: impl FnOnce(&mut TableState<'a>, usize)) {
        if self.table_active() {
            f(&mut self.table, self.state.row_depth());
        }
    }

    fn set_style(&mut self, f: impl FnOnce(&mut Style)) {
        self.flush_pending();
        f(&mut self.state.style);
    }

    /// Whether a character a control writes is taken, flushing the text
    /// before it; a `\uN` fallback skip consumes it.
    fn take_char(&mut self) -> bool {
        if !self.accepts_text() || self.decoder.skip_char() {
            return false;
        }
        self.flush_pending();
        true
    }

    /// A character the source escapes, read through its font as text is.
    fn push_char(&mut self, c: char) {
        if self.take_char() {
            self.push_text(c.encode_utf8(&mut [0; 4]), TextLoss::default());
        }
    }

    /// A character a control word stands for, which no font recodes.
    fn push_generated(&mut self, c: char) {
        if !self.take_char() {
            return;
        }
        let mut buffer = [0; 4];
        let text = c.encode_utf8(&mut buffer);
        if self.state.capture == Capture::None {
            self.push_body(symbol::decode_with(text, Glyph::Char), TextLoss::default());
        } else {
            self.push_text(text, TextLoss::default());
        }
    }

    /// The font text is set in: its group's `\fN`, else the document's
    /// `\deffN`; `None` when the font table lacks the one chosen.
    fn font(&self) -> Option<Font> {
        self.state.font.or(self.deff).and_then(|n| self.prelude.fonts.get(&n).copied())
    }

    /// Body text read through the face of its font: a symbol font draws each
    /// code, and a text font draws U+F020-F0FF through the last symbol font.
    fn decode(&self, text: &str) -> Decoded {
        match self.font().map(|font| font.face) {
            Some(Face::Glyphs(font)) => symbol::decode_run(text, |_| Some(font)),
            Some(Face::Charset2) => symbol::decode_with(text, Glyph::Char),
            Some(Face::Text) | None => {
                let last = self.last_symbol;
                symbol::decode_with(text, |c| match last {
                    _ if !('\u{f020}'..='\u{f0ff}').contains(&c) => Glyph::Char(c),
                    Some(Face::Glyphs(font)) => symbol::glyph_of(font, c),
                    Some(Face::Text | Face::Charset2) => Glyph::Char(c),
                    None => Glyph::Unmapped,
                })
            }
        }
    }

    fn flush_pending(&mut self) {
        self.settle_surrogate();
        self.flush_text();
    }

    /// Take the buffered bytes as text: a symbol font's body text as its
    /// font codes, any other through the font's code page.
    fn flush_text(&mut self) {
        let font = self.font();
        if self.state.capture == Capture::None
            && let Some(Font { face: Face::Glyphs(_), .. }) = font
        {
            if let Some(bytes) = self.decoder.take_bytes() {
                let codes: String = bytes.into_iter().map(char::from).collect();
                self.push_text(&codes, TextLoss::default());
            }
            return;
        }
        if let Some((text, loss)) = self.decoder.take_pending(font.and_then(|f| f.codepage)) {
            self.push_text(&text, loss);
        }
    }

    fn push_text(&mut self, raw: &str, loss: TextLoss) {
        if self.state.capture == Capture::None {
            self.push_body(self.decode(raw), loss);
            return;
        }
        let text = clean_text(raw);
        if self.captures_deleted() {
            if !text.is_empty() {
                self.drop_deleted();
            }
            return;
        }
        if self.captures_hidden() {
            self.drop_hidden_text(&[symbol::Token::Text(text)], 0);
            return;
        }
        if matches!(self.state.capture, Capture::FieldInstr | Capture::Math) {
            if !text.is_empty() {
                self.deleted_range = None;
                if !self.state.hidden {
                    self.hidden_range = None;
                }
            }
            if self.state.capture == Capture::Math {
                if let Some(math) = &mut self.dest.math {
                    math.push_text(clean_text(&mark_loss(raw, loss)));
                }
            } else if let Some(field) = self.dest.fields.last_mut() {
                field.push_text(raw, loss);
            }
            return;
        }
        self.text_loss(loss);
        if text.is_empty() {
            return;
        }
        match self.state.capture {
            Capture::ListText => {
                if let Some(lt) = &mut self.dest.listtext {
                    lt.push_str(&text);
                }
            }
            Capture::Bookmark => self.dest.bookmark.push_str(&text),
            Capture::PicturePropertyName | Capture::PicturePropertyValue => {
                if let Some(pict) = &mut self.dest.pict {
                    if self.state.capture == Capture::PicturePropertyName {
                        pict.property_name.push_str(&text);
                    } else {
                        pict.property_value.push_str(&text);
                    }
                }
            }
            // Picture payload bytes are collected raw in the token loop.
            Capture::Pict | Capture::FieldInstr | Capture::Math | Capture::None => {}
        }
    }

    /// Body text read through its font, with what its decoding lost: shown,
    /// or reported as the range that leaves it out.
    fn push_body(&mut self, decoded: Decoded, loss: TextLoss) {
        self.text_loss(loss);
        let Decoded { mut tokens, unmapped } = decoded;
        tokens.retain_mut(|token| match token {
            symbol::Token::Text(text) => {
                *text = clean_text(text);
                !text.is_empty()
            }
            symbol::Token::Checkbox(_) => true,
        });
        if (tokens.is_empty() && unmapped == 0) || self.state.suppress {
            return;
        }
        if self.state.deleted {
            self.drop_deleted();
            return;
        }
        if self.state.hidden {
            self.drop_hidden_text(&tokens, unmapped);
            return;
        }
        self.close_ranges();
        let region = self.region();
        self.parse_ctx.emit_count(DiagnosticCode::UnmappedSymbol, region, unmapped);
        if let Some(n) = self.state.font
            && !self.prelude.fonts.is_empty()
            && !self.prelude.fonts.contains_key(&n)
            && shows(&tokens, unmapped)
            && self.undefined_fonts.insert(n)
        {
            self.parse_ctx.emit(DiagnosticCode::FormattingUnreadable, region);
        }
        for token in tokens {
            self.inlines.push(match token {
                symbol::Token::Text(text) => Inline::Text { text, style: self.state.style },
                symbol::Token::Checkbox(checked) => Inline::Checkbox(checked),
            });
        }
    }

    fn flush_top_table(&mut self) -> Result<(), ConvertError> {
        if let Some(block) = self.table.take_table(1)? {
            self.flush_runs();
            self.blocks.push(block);
        }
        Ok(())
    }

    fn end_paragraph(&mut self) -> Result<(), ConvertError> {
        self.close_ranges();
        self.held = None;
        let inlines = std::mem::take(&mut self.inlines);
        let listtext = self.dest.listtext.take();
        let listtext_loss = std::mem::take(&mut self.dest.listtext_loss);
        let math_display = std::mem::take(&mut self.dest.math_display);

        if self.state.in_table {
            self.table.push_cell_paragraph(self.state.row_depth(), self.state.block, inlines);
            return Ok(());
        }
        self.flush_top_table()?;

        // A styled container absorbs its blank paragraphs: they are the
        // blank lines of a code block.
        if let Some(style) = self.state.block {
            flush_list(&mut self.blocks, &mut self.list_run);
            self.styled.push(style, inlines, &mut self.blocks);
            return Ok(());
        }
        if inlines_are_empty(&inlines) {
            self.flush_runs();
            return Ok(());
        }
        // Numbering identity comes from the list tables; the captured label
        // text only seeds legacy Word-95 numbering. Numbered headings
        // advance the sequence and keep their number visible.
        let entry = self.list_entry(listtext.as_deref(), listtext_loss);
        if let Some(level) = self.state.outline {
            self.flush_runs();
            let mut content = inlines;
            rebase_emphasis(&mut content, self.state.style_base);
            if let Some((ListMark { key, number, label, .. }, _)) = entry
                && key.marker.ordered()
            {
                let text = format!("{} ", label.unwrap_or_else(|| key.marker.label(number)));
                content.insert(0, Inline::Text { text, style: Style::PLAIN });
            }
            self.blocks.push(Block::Heading { level, anchor: None, content });
            return Ok(());
        }
        if let Some((mark, label_loss)) = entry {
            self.report(label_loss, self.region());
            self.list_run.push(ListEntry { mark, blocks: vec![Block::Paragraph(inlines)] });
            return Ok(());
        }
        self.flush_runs();
        match math_lines(&inlines).filter(|_| math_display) {
            Some(lines) => self.blocks.extend(lines.into_iter().map(Block::Math)),
            None => self.blocks.push(Block::Paragraph(inlines)),
        }
        Ok(())
    }

    /// A captured `\listtext` as a literal marker label, trimmed and
    /// non-empty, with what it lost, to report once it is shown.
    fn listtext_label(listtext: Option<&str>, loss: TextLoss) -> (Option<String>, TextLoss) {
        let label = listtext.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
        let loss = if label.is_some() { loss } else { TextLoss::default() };
        (label, loss)
    }

    fn list_entry(
        &mut self,
        listtext: Option<&str>,
        listtext_loss: TextLoss,
    ) -> Option<(ListMark, TextLoss)> {
        if let Some(ls) = self.state.ls {
            let level = self.state.ilvl;
            if let Some(list) = self.prelude.list(ls) {
                let def = &list.levels[Levels::clamp(level)];
                let marker = def.marker?;
                let (number, label, loss) = if marker.ordered() {
                    self.counters.next_labeled(ls, level, &list.levels)
                } else {
                    (0, None, TextLoss::default())
                };
                if loss != TextLoss::default()
                    && self.labels_reported.insert((ls, Levels::clamp(level)))
                {
                    self.report(loss, self.region());
                }
                let key = ListKey { instance: table_list(ls), marker };
                return Some((ListMark { key, level, number, label }, TextLoss::default()));
            }
            // No table definition behind the \ls: the captured \listtext is
            // the only surviving evidence of the real marker; carry it as
            // the item's literal label over a bullet base.
            let (label, loss) = Self::listtext_label(listtext, listtext_loss);
            let key = ListKey { instance: table_list(ls), marker: MarkerKind::Bullet };
            return Some((ListMark { key, level, number: 0, label }, loss));
        }
        if let Some(marker) = self.state.legacy_list {
            let number = if marker.ordered() {
                // Legacy \pn numbering: the label text carries the number,
                // clamped so a crafted label cannot overflow the counter.
                let parsed = listtext.and_then(|t| {
                    let digits = t.split(|c: char| !c.is_ascii_digit()).next().unwrap_or_default();
                    digits.parse::<u64>().ok().map(|n| n.min(u64::from(u32::MAX)))
                });
                match parsed {
                    Some(n) => {
                        self.counters.seed(Sequence::Legacy, self.state.ilvl, n);
                        n
                    }
                    None => self.counters.next(Sequence::Legacy, self.state.ilvl, 1),
                }
            } else {
                0
            };
            let key = ListKey { instance: LEGACY_LIST, marker };
            let mark = ListMark { key, level: self.state.ilvl, number, label: None };
            return Some((mark, TextLoss::default()));
        }
        // A bare \listtext with no list state still marks a list paragraph
        // (some producers omit \ls); its text is the literal marker.
        if listtext.is_some_and(|t| !t.trim().is_empty()) {
            let (label, loss) = Self::listtext_label(listtext, listtext_loss);
            let key = ListKey { instance: LISTTEXT_LIST, marker: MarkerKind::Bullet };
            return Some((ListMark { key, level: self.state.ilvl, number: 0, label }, loss));
        }
        None
    }

    fn end_cell(&mut self, depth: usize) -> Result<(), ConvertError> {
        self.close_ranges();
        self.held = None;
        let inlines = std::mem::take(&mut self.inlines);
        self.dest.clear_paragraph_state();
        self.table.end_cell(depth, self.state.block, inlines)
    }

    fn end_row(&mut self, depth: usize) -> Result<(), ConvertError> {
        if self.table.has_pending_cell(depth) || !inlines_are_empty(&self.inlines) {
            self.end_cell(depth)?;
        }
        self.dest.clear_paragraph_state();
        self.table.end_row(depth);
        Ok(())
    }

    fn flush_runs(&mut self) {
        self.styled.flush(&mut self.blocks);
        flush_list(&mut self.blocks, &mut self.list_run);
    }

    fn finish(mut self) -> Result<Document, ConvertError> {
        for depth in (1..=self.table.depth()).rev() {
            if self.table.has_partial_row(depth) {
                self.end_row(depth)?;
            }
        }
        self.table.collapse_nested()?;
        self.flush_top_table()?;
        self.flush_runs();
        Ok(Document {
            blocks: self.blocks,
            notes: self.dest.notes,
            assets: self.assets.assets,
            ..Document::default()
        })
    }
}

/// Whether text read as `tokens` and `unmapped` codes shows anything: a
/// checkbox, a code no table maps, or text other than whitespace.
fn shows(tokens: &[symbol::Token], unmapped: u64) -> bool {
    unmapped > 0
        || tokens.iter().any(|token| match token {
            symbol::Token::Text(text) => !text.trim().is_empty(),
            symbol::Token::Checkbox(_) => true,
        })
}

#[cfg(test)]
mod tests {
    use super::lexer::fuel;
    use super::*;
    use crate::shared::test_support::diagnostic_rows;

    #[test]
    fn vertical_position_controls_follow_group_scope_and_resets() {
        for (source, expected) in [
            (r"{\rtf1 x\super 2\nosupersub y}", "x<sup>2</sup>y\n"),
            (r"{\rtf1 H\sub 2\nosupersub O}", "H<sub>2</sub>O\n"),
            (r"{\rtf1 \super a\sub b\nosupersub c}", "<sup>a</sup><sub>b</sub>c\n"),
            (r"{\rtf1 \super a\plain b}", "<sup>a</sup>b\n"),
            (r"{\rtf1 x{\super 2}y}", "x<sup>2</sup>y\n"),
            (r"{\rtf1 \super a{\sub b}c}", "<sup>a</sup><sub>b</sub><sup>c</sup>\n"),
            (r"{\rtf1 \super0 a\sub0 b\nosupersub1 c}", "<sup>a</sup><sub>b</sub>c\n"),
            (r"{\rtf1 \super \'e9\sub \u946?\nosupersub z}", "<sup>é</sup><sub>β</sub>z\n"),
            (r"{\rtf1 \super a\up6 b\dn6 c}", "<sup>abc</sup>\n"),
        ] {
            assert_eq!(converted(source), (expected.to_owned(), vec![]), "{source}");
        }
    }

    #[test]
    fn vertical_position_styles_inherit_and_allow_explicit_resets() {
        let styles = r"{\stylesheet{\s1\super Raised;}{\s2\sbasedon1 Inherited;}{\s3\sbasedon2\sub Lowered;}{\s4\sbasedon3\nosupersub Baseline;}}";
        for (body, expected) in [
            (r"\s1 a", "<sup>a</sup>\n"),
            (r"\s2 b", "<sup>b</sup>\n"),
            (r"\s3 c", "<sub>c</sub>\n"),
            (r"\s4 d", "d\n"),
            (r"\s2 a\sub b\nosupersub c", "<sup>a</sup><sub>b</sub>c\n"),
            (r"\s3 a\plain b", "<sub>a</sub>b\n"),
            (r"\s1 a\s4 b", "<sup>a</sup>b\n"),
        ] {
            let source = format!(r"{{\rtf1{styles}{body}}}");
            assert_eq!(converted(&source), (expected.to_owned(), vec![]), "{body}");
        }
    }

    #[test]
    fn vertical_position_stylesheet_plain_resets_local_and_inherited_formatting() {
        for (styles, expected) in [
            (r"{\s1\super\plain S;}", "x\n"),
            (r"{\s1\sub\plain S;}", "x\n"),
            (r"{\s0\super Base;}{\s1\sbasedon0\plain S;}", "x\n"),
            (r"{\s0\sub Base;}{\s1\sbasedon0\plain\super S;}", "<sup>x</sup>\n"),
            (r"{\s0\super\b\i\v Base;}{\s1\sbasedon0\plain S;}", "x\n"),
            (r"{\s1\outlinelevel0\super\plain S;}", "# x\n"),
        ] {
            let source = format!(r"{{\rtf1{{\stylesheet{styles}}}\s1 x\par}}");
            assert_eq!(converted(&source), (expected.to_owned(), vec![]), "{styles}");
        }
    }

    #[test]
    fn vertical_position_keeps_note_references_and_geometric_offsets_separate() {
        assert_eq!(markdown(r"{\rtf1 \up6 a\dn6 b}"), "ab\n");
        let source = r"{\rtf1 a{\super\chftn}{\footnote n}\par}";
        assert_eq!(converted(source), ("a[^1]\n\n[^1]: n\n".to_owned(), vec![]));
    }

    #[test]
    fn missing_list_table_keeps_listtext_marker() {
        let doc = parse(
            &ParseCtx::default(),
            br"{\rtf1 \pard{\listtext 1.\tab}\ls5 one\par \pard{\listtext 2.\tab}\ls5 two\par}",
        )
        .unwrap();
        let Block::List(list) = &doc.blocks[0] else { panic!("{:?}", doc.blocks) };
        assert_eq!(list.items[0].marker_label.as_deref(), Some("1."));
        assert_eq!(list.items[1].marker_label.as_deref(), Some("2."));
    }

    #[test]
    fn mid_paragraph_page_and_column_breaks_keep_the_word_boundary() {
        for src in [r"{\rtf1 Alfa\page Beta\par}", r"{\rtf1 Alfa\column Beta\par}"] {
            assert_eq!(markdown(src), "Alfa\\\nBeta\n", "source: {src}");
        }
    }

    #[test]
    fn backslash_before_a_line_break_breaks_the_paragraph() {
        // The paragraph mark Cocoa's RTF writer emits instead of `\par`.
        for src in
            ["{\\rtf1 Alpha\\\nBeta\\\nGamma\\\n}", "{\\rtf1 Alpha\\\r\nBeta\\\rGamma\\\r\n}"]
        {
            assert_eq!(markdown(src), "Alpha\n\nBeta\n\nGamma\n", "source: {src:?}");
        }
    }

    #[test]
    fn body_text_before_a_table_stays_out_of_the_first_cell() {
        let src = "{\\rtf1 Intro\\\n\\trowd\\cellx2000\\cellx4000 A\\cell B\\cell\\row}";
        let shown = markdown(src);
        assert!(shown.starts_with("Intro\n\n|"), "{shown}");
    }

    #[test]
    fn styled_runs_stop_before_tables_and_work_inside_cells() {
        let src = br"{\rtf1\ansi{\stylesheet{\s0 Normal;}{\s1 Source Code;}}\pard\plain\s1 before table\par\trowd\cellx2000\pard\plain\intbl\s1 first\par second\cell\row}";
        let doc = parse(&ParseCtx::default(), src).unwrap();
        let [Block::CodeBlock { text: before, .. }, Block::Table(table)] = &doc.blocks[..] else {
            panic!("unexpected block order: {:?}", doc.blocks)
        };
        assert_eq!(before, "before table");
        let crate::model::CellSlot::Origin(cell) = &table.grid[0][0] else {
            panic!("expected an origin cell")
        };
        let [Block::CodeBlock { text: inside, .. }] = &cell.blocks[..] else {
            panic!("unexpected cell blocks: {:?}", cell.blocks)
        };
        assert_eq!(inside, "first\nsecond");
    }

    #[test]
    fn row_controls_merge_declared_cells_in_the_body_and_do_nothing_in_a_note() {
        for (label, source, expected) in [
            (
                "a horizontal merge",
                r"{\rtf1 \trowd\clmgf\cellx1000\clmrg\cellx2000\intbl a\cell b\cell\row}",
                "|  |  |\n| --- | --- |\n| a<br>b |  |\n",
            ),
            (
                "a vertical merge",
                r"{\rtf1 \trowd\clvmgf\cellx1000\cellx2000 \intbl a\cell b\cell\row \trowd\clvmrg\cellx1000\cellx2000 \intbl c\cell d\cell\row}",
                "|  |  |\n| --- | --- |\n| a | b |\n|  | d |\n",
            ),
            (
                "a merge in a note",
                r"{\rtf1 \trowd{\footnote\clmgf n}\cellx1000\clmrg\cellx2000\intbl a\cell b\cell\row}",
                "|  |  |\n| --- | --- |\n| [^1]a | b |\n\n[^1]: n\n",
            ),
            (
                "a cell boundary in a note",
                r"{\rtf1 \trowd\cellx1000{\footnote n\cellx9000}\cellx2000\intbl a\cell b\cell\row \trowd\cellx1000\cellx2000\intbl c\cell d\cell\row}",
                "| [^1]a | b |\n| --- | --- |\n| c | d |\n\n[^1]: n\n",
            ),
        ] {
            assert_eq!(markdown(source), expected, "{label}");
        }
    }

    #[test]
    fn plain_cell_and_row_close_the_outer_table_and_an_itap_of_zero_reads_as_one() {
        for (label, source, expected) in [
            (
                "an itap of zero",
                r"{\rtf1 \itap0\trowd\cellx1000\intbl a\par b\cell\row}",
                "|  |\n| --- |\n| a<br>b |\n",
            ),
            (
                "a plain cell under itap 2",
                r"{\rtf1 \trowd\cellx1000\intbl x\itap2 a\cell\row\pard after}",
                "|  |\n| --- |\n| xa |\n\nafter\n",
            ),
            (
                "a plain row under itap 2",
                r"{\rtf1 \trowd\cellx1000\intbl a\cell\itap2\row\pard after}",
                "|  |\n| --- |\n| a |\n\nafter\n",
            ),
        ] {
            assert_eq!(markdown(source), expected, "{label}");
        }
    }

    #[test]
    fn pict_hex_payload_becomes_an_asset() {
        let doc =
            parse(&ParseCtx::default(), br"{\rtf1 before {\pict\pngblip 89504e470d0a1a0a} after}")
                .unwrap();
        assert_eq!(doc.assets.len(), 1, "assets: {:?}", doc.assets);
        assert_eq!(doc.assets[0].media_type, "image/png");
        assert!(doc.assets[0].bytes.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn a_pict_hex_payload_skips_non_hex_bytes_and_drops_a_dangling_digit() {
        let doc =
            parse(&ParseCtx::default(), b"{\\rtf1 {\\pict\\pngblip 89 50g4e\r\n47 0d0a1a0a f}}")
                .unwrap();
        assert_eq!(doc.assets[0].bytes, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    }

    #[test]
    fn pict_property_subgroups_preserve_alt_without_contaminating_the_payload() {
        let doc = parse(&ParseCtx::default(),
            br"{\rtf1 {\pict{\*\picprop{\sp{\sn wzDescription}{\sv abcdef}}}\pngblip 89504e470d0a1a0a}}",
        )
        .unwrap();
        assert_eq!(doc.assets.len(), 1, "assets: {:?}", doc.assets);
        assert!(
            matches!(&doc.blocks[0], Block::Paragraph(content) if matches!(&content[0], Inline::Image { alt, .. } if alt == "abcdef"))
        );
        assert!(doc.assets[0].bytes.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn a_picture_keeps_to_the_payload_and_properties_of_its_own_groups() {
        let png = vec![0x89, 0x50, 0x4e, 0x47];
        for (label, source, bytes, rendered) in [
            (
                "a subgroup",
                r"{\rtf1 {\pict\pngblip 8950{ff}4e47}}",
                png.clone(),
                "![Image](asset:0)\n",
            ),
            (
                "bytes after a word that ends the capture",
                r"{\rtf1 {\pict\pngblip 8950\fldrslt 4e47}}",
                vec![0x89, 0x50],
                "4e47![Image](asset:0)\n",
            ),
            (
                "a property group beyond the picprop",
                r"{\rtf1 {\pict{\*\picprop{\sp{\sn wzDescription}{\sv abc}}}{\x{\sp{\sn wzDescription}{\sv zzz}}}\pngblip 89504e47}}",
                png.clone(),
                "![abc](asset:0)\n",
            ),
            (
                "a second property in the picprop",
                r"{\rtf1 {\pict{\*\picprop{\sp{\sn shapeType}{\sv 75}}{\sp{\sn wzDescription}{\sv abc}}}\pngblip 89504e47}}",
                png,
                "![abc](asset:0)\n",
            ),
        ] {
            let doc = parse(&ParseCtx::default(), source.as_bytes()).unwrap();
            assert_eq!(doc.assets[0].bytes, bytes, "{label}");
            assert_eq!(markdown(source), rendered, "{label}");
        }
    }

    #[test]
    fn nonshppict_fallback_is_not_extracted_twice() {
        let doc = parse(
            &ParseCtx::default(),
            br"{\rtf1 {\*\shppict{\pict\pngblip 89504e47}}{\nonshppict{\pict\wmetafile8 0102}}}",
        )
        .unwrap();
        assert_eq!(doc.assets.len(), 1, "only the preferred picture: {:?}", doc.assets);
        assert_eq!(doc.assets[0].media_type, "image/png");
    }

    #[test]
    fn one_pass_reads_the_code_page_and_the_four_tables() {
        let fonts = r"{\f1\fcharset0 A;}";
        let styles = r"{\s1\outlinelevel0 H;}";
        let lists =
            r"{\list{\listlevel\levelnfc0{\leveltext\'02\'00.;}{\levelnumbers\'01;}}\listid7}";
        let overrides = r"{\listoverride\listid7\ls2}";
        let src = format!(
            r"{{\rtf1\ansi\ansicpg1251{{\fonttbl{fonts}}}{{\stylesheet{styles}}}{{\*\listtable{lists}}}{{\*\listoverridetable{overrides}}}\ansicpg1252\s1 \'e0\par}}"
        );
        let parse_ctx = ParseCtx::default();
        let mut parser = Parser::new(&parse_ctx, src.as_bytes());
        parser.run().unwrap();
        assert_eq!(parser.decoder.default_encoding, codepage::windows(1251));
        assert!(parser.prelude.fonts.contains_key(&1));
        assert_eq!(parser.prelude.styles.get(&1).and_then(|style| style.outline), Some(1));
        let level = parser.prelude.list(2).map(|list| list.levels[0].marker);
        assert_eq!(level, Some(Some(MarkerKind::Decimal)));
        let markdown = crate::render::markdown::document_to_markdown(&parser.finish().unwrap());
        assert_eq!(markdown, "# \u{430}\n");
        let tables = [fonts, styles, lists, overrides].map(fuel).iter().sum::<u64>();
        assert_eq!(parse_ctx.fuel(), fuel(&src) + tables);
    }

    #[test]
    fn a_table_group_inside_one_of_its_kind_is_read_once() {
        let body = r"{\fonttbl{\f1 A;}}";
        let src = format!(r"{{\rtf1{{\fonttbl{body}}}x}}");
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, src.as_bytes()).unwrap();
        assert_eq!(parse_ctx.fuel(), fuel(&src) + fuel(body));
    }

    fn list_starts(src: &str) -> Vec<(MarkerKind, u64, usize)> {
        let doc = parse(&ParseCtx::default(), src.as_bytes()).unwrap();
        let lists = doc.blocks.iter().map(|block| match block {
            Block::List(list) => (list.marker, list.start, list.items.len()),
            other => panic!("{other:?}"),
        });
        lists.collect()
    }

    #[test]
    fn a_level_override_inherits_removes_or_sets_the_marker() {
        let marker = |level: &str| {
            let src = format!(
                r"{{\rtf1{{\*\listtable{{\list{{\listlevel\levelnfc0{{\leveltext\'02\'00.;}}{{\levelnumbers\'01;}}}}\listid7}}}}{{\*\listoverridetable{{\listoverride\listid7\listoverridecount1{{\lfolevel{level}}}\ls2}}}}\pard\ls2 a\par}}"
            );
            let parse_ctx = ParseCtx::default();
            let mut parser = Parser::new(&parse_ctx, src.as_bytes());
            parser.run().unwrap();
            parser.prelude.list(2).and_then(|list| list.levels[0].marker)
        };
        assert_eq!(marker(r"\levelstartat3"), Some(MarkerKind::Decimal));
        assert_eq!(marker(r"\levelnfc255"), None);
        assert_eq!(marker(r"\levelnfc4"), Some(MarkerKind::LowerAlpha));
    }

    #[test]
    fn a_negative_ls_keeps_its_own_list_beside_legacy_bullets() {
        let src = r"{\rtf1 \pard\ls-1 a\par \pard\pnlvlblt b\par}";
        let bullet = (MarkerKind::Bullet, 1, 1);
        assert_eq!(list_starts(src), [bullet, bullet]);
    }

    #[test]
    fn the_highest_ls_numbers_apart_from_legacy_numbering() {
        let table = r"{\*\listtable{\list{\listlevel\levelnfc0{\leveltext\'02\'00.;}{\levelnumbers\'01;}}\listid7}}{\*\listoverridetable{\listoverride\listid7\ls2147483647}}";
        let src = format!(
            r"{{\rtf1{table}\pard{{\pntext 5.}}\pnlvlbody a\par \pard\ls2147483647 b\par}}"
        );
        let decimal = |start| (MarkerKind::Decimal, start, 1);
        assert_eq!(list_starts(&src), [decimal(5), decimal(1)]);
    }

    #[test]
    fn a_numbered_heading_shows_its_label_and_a_bulleted_one_none() {
        let lists = r"{\*\listtable{\list{\listlevel\levelnfc0{\leveltext\'02\'00.;}{\levelnumbers\'01;}}\listid7}{\list{\listlevel\levelnfc0{\leveltext\'03(\'00);}{\levelnumbers\'02;}}\listid8}{\list{\listlevel\levelnfc23{\leveltext\'01舦 ?;}{\levelnumbers;}}\listid9}}{\*\listoverridetable{\listoverride\listid7\ls2}{\listoverride\listid8\ls3}{\listoverride\listid9\ls4}}";
        for (label, ls, expected) in [
            ("a decimal list", 2, "# 1. Title\n"),
            ("a list with a pattern of its own", 3, "# (1) Title\n"),
            ("a bulleted list", 4, "# Title\n"),
        ] {
            let source = format!(r"{{\rtf1{lists}\pard\ls{ls}\outlinelevel0 Title\par}}");
            assert_eq!(markdown(&source), expected, "{label}");
        }
    }

    #[test]
    fn a_unicode_value_past_the_bmp_keeps_its_scalar_whatever_its_low_bits() {
        let src = r"{\rtf1 \u120832?\par}";
        assert_eq!(converted(src), ("\u{1D800}\n".to_owned(), vec![]));
    }

    #[test]
    fn an_override_table_before_its_list_table_resolves_through_it() {
        let src = r"{\rtf1{\*\listoverridetable{\listoverride\listid7\ls2}}{\*\listtable{\list{\listlevel\levelnfc0{\leveltext\'02\'00.;}{\levelnumbers\'01;}}\listid7}}\pard\ls2\ilvl0 a\par}";
        assert_eq!(markdown(src), "1. a\n");
    }

    #[test]
    fn a_declared_code_page_overrides_a_guessed_character_set() {
        let src = r"{\rtf1\mac\ansicpg1252 \'e9\par}";
        assert_eq!(converted(src), ("\u{e9}\n".to_owned(), vec![]));
    }

    #[test]
    fn a_code_page_declared_after_text_leaves_that_text_as_read() {
        assert_eq!(markdown(r"{\rtf1 \'e0\ansicpg1251 \'e0\par}"), "\u{e0}\u{430}\n");
    }

    #[test]
    fn mac_pc_and_pca_name_their_code_pages() {
        for (src, text) in [
            (r"{\rtf1\mac \'8e{\b\'8f}\par}", "\u{e9}**\u{e8}**\n"),
            (r"{\rtf1\pc \'82\'9b\'b0\par}", "\u{e9}\u{a2}\u{2591}\n"),
            (r"{\rtf1\pca \'82\'d0\'b5\par}", "\u{e9}\u{f0}\u{c1}\n"),
            (r"{\rtf1\pc a{\footnote \'82}}", "a[^1]\n\n[^1]: \u{e9}\n"),
        ] {
            assert_eq!(converted(src), (text.to_owned(), vec![]), "{src}");
        }
    }

    #[test]
    fn an_ansicpg_overrides_mac_pc_and_pca_either_side() {
        for src in [
            r"{\rtf1\mac\ansicpg1251 \'e0\par}",
            r"{\rtf1\pc\ansicpg1251 \'e0\par}",
            r"{\rtf1\pca\ansicpg1251 \'e0\par}",
            r"{\rtf1\ansicpg1251\mac \'e0\par}",
            r"{\rtf1\ansicpg1251\pc \'e0\par}",
        ] {
            assert_eq!(converted(src), ("\u{430}\n".to_owned(), vec![]), "{src}");
        }
    }

    #[test]
    fn the_last_of_mac_pc_and_pca_names_the_code_page() {
        for (src, text) in [
            (r"{\rtf1\pc\mac \'8e\par}", "\u{e9}\n"),
            (r"{\rtf1\mac\pc \'8e\par}", "\u{c4}\n"),
            (r"{\rtf1\pc\pca \'9b\par}", "\u{f8}\n"),
            (r"{\rtf1\pca\pc \'9b\par}", "\u{a2}\n"),
        ] {
            assert_eq!(converted(src), (text.to_owned(), vec![]), "{src}");
        }
    }

    #[test]
    fn newly_mapped_code_pages_and_charsets_read_exactly() {
        for (src, text) in [
            (r"{\rtf1\ansi\ansicpg10000 \'8e\'db\par}", "\u{e9}\u{20ac}\n"),
            (r"{\rtf1\ansi\ansicpg10007 \'80\'df\par}", "\u{410}\u{44f}\n"),
            (r"{\rtf1\ansi\ansicpg65001 \'c3\'a9\par}", "\u{e9}\n"),
            (r"{\rtf1\ansi\ansicpg866 \'80\'e0\par}", "\u{410}\u{440}\n"),
            (r"{\rtf1\ansi\ansicpg20866 \'c1\par}", "\u{430}\n"),
            (r"{\rtf1\ansi\ansicpg21866 \'a4\par}", "\u{454}\n"),
            (r"{\rtf1\ansi\ansicpg28591 \'80\par}", "\u{20ac}\n"),
            (r"{\rtf1\ansi\ansicpg28592 \'b1\par}", "\u{105}\n"),
            (r"{\rtf1\ansi\ansicpg28595 \'d0\par}", "\u{430}\n"),
            (r"{\rtf1\ansi\ansicpg28597 \'e1\par}", "\u{3b1}\n"),
            (r"{\rtf1\ansi\ansicpg28599 \'fd\par}", "\u{131}\n"),
            (r"{\rtf1\ansi\ansicpg28603 \'e0\par}", "\u{105}\n"),
            (r"{\rtf1\ansi\ansicpg28605 \'a4\par}", "\u{20ac}\n"),
            (r"{\rtf1\ansi\ansicpg54936 \'81\'30\'8b\'37\par}", "\u{ff}\n"),
            (r"{\rtf1\ansi\ansicpg437 \'c9\par}", "\u{2554}\n"),
            (r"{\rtf1\ansi\ansicpg850 \'d0\par}", "\u{f0}\n"),
            (r"{\rtf1\ansi\ansicpg1252{\fonttbl{\f1\fcharset77 M;}}a\f1 \'8e\par}", "a\u{e9}\n"),
            (
                r"{\rtf1\ansi\ansicpg1252{\fonttbl{\f1\fcharset89 M;}}\f1 \'84\'e0\par}",
                "\u{414}\u{430}\n",
            ),
        ] {
            assert_eq!(converted(src), (text.to_owned(), vec![]), "{src}");
        }
    }

    #[test]
    fn an_unmapped_code_page_reads_as_assumed_windows_1252() {
        use DiagnosticCode::EncodingAssumed;
        for src in [
            r"{\rtf1\ansi\ansicpg1361 \'e9\par}",
            r"{\rtf1\ansi\ansicpg-1 \'e9\par}",
            r"{\rtf1\ansi\ansicpg66536 \'e9\par}",
        ] {
            let expected = ("\u{e9}\n".to_owned(), vec![(EncodingAssumed, Region::Body, 1)]);
            assert_eq!(converted(src), expected, "{src}");
        }
    }

    #[test]
    fn an_unknown_font_charset_keeps_the_document_code_page_as_assumed() {
        let src = r"{\rtf1\ansi\ansicpg1251{\fonttbl{\f1\fcharset254 M;}}\f1 \'e0\par}";
        let expected = vec![(DiagnosticCode::EncodingAssumed, Region::Body, 1)];
        assert_eq!(converted(src), ("\u{430}\n".to_owned(), expected));
    }

    #[test]
    fn a_byte_order_mark_in_a_run_never_switches_its_encoding() {
        let src = r"{\rtf1 \'ff\'feab\par}";
        assert_eq!(converted(src), ("\u{ff}\u{fe}ab\n".to_owned(), vec![]));
        let src = r"{\rtf1\ansicpg65001 \'fe\'ff\'00a\par}";
        let expected = vec![(DiagnosticCode::InvalidEncoding, Region::Body, 1)];
        assert_eq!(converted(src), ("\u{fffd}\u{fffd}a\n".to_owned(), expected));
    }

    #[test]
    fn a_utf8_sequence_decodes_across_hex_escapes_and_reports_each_piece_a_unicode_word_splits() {
        let src = r"{\rtf1\ansicpg65001 \'e2\'82\'ac\par}";
        assert_eq!(converted(src), ("\u{20ac}\n".to_owned(), vec![]));
        let src = r"{\rtf1\ansicpg65001 \'e2\'82\u8364?\'ac\par}";
        let expected = vec![(DiagnosticCode::InvalidEncoding, Region::Body, 2)];
        assert_eq!(converted(src), ("\u{fffd}\u{20ac}\u{fffd}\n".to_owned(), expected));
    }

    type Reported = (DiagnosticCode, Region, u64);

    fn converted(src: &str) -> (String, Vec<Reported>) {
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, src.as_bytes()).unwrap();
        let diagnostics =
            crate::converter::diagnostics_of(parse_ctx, &doc, crate::ImagePolicy::Placeholder);
        (
            crate::render::markdown::document_to_markdown(&doc),
            diagnostic_rows(&diagnostics.unwrap()),
        )
    }

    fn reported(src: &str) -> Vec<Reported> {
        converted(src).1
    }

    fn markdown(src: &str) -> String {
        crate::render::markdown::document_to_markdown(
            &parse(&ParseCtx::default(), src.as_bytes()).unwrap(),
        )
    }

    type Case = (&'static str, String, Option<&'static str>, Vec<Reported>);

    fn check(cases: Vec<Case>) {
        let mut failed = Vec::new();
        for (case, src, rendered, mut expected) in cases {
            expected.sort();
            let (shown, got) = converted(&src);
            if rendered.is_some_and(|rendered| rendered != shown) {
                failed.push(format!("{case}: rendered {shown:?}"));
            }
            if got != expected {
                failed.push(format!("{case}: reported {got:?}"));
            }
        }
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }

    const SYMBOL_FONTS: &str = r"{\fonttbl{\f0\fcharset0 Times;}{\f1\fcharset2 Symbol;}{\f2\fcharset2 Wingdings;}{\f3\fcharset2 Marlett;}}";

    fn symbols(body: &str) -> String {
        format!(r"{{\rtf1\ansi\ansicpg1252{SYMBOL_FONTS}{body}\par}}")
    }

    fn check_symbols(cases: Vec<(&'static str, &str, &'static str, Vec<Reported>)>) {
        check(
            cases
                .into_iter()
                .map(|(case, body, expected, reports)| {
                    (case, symbols(body), Some(expected), reports)
                })
                .collect(),
        );
    }

    #[test]
    fn a_symbol_font_draws_its_codes_and_a_text_font_its_private_use_symbols() {
        use DiagnosticCode::UnmappedSymbol;
        use Region::Body;
        check_symbols(vec![
            (
                "Symbol text, bytes and Unicode",
                r"\f1 m\'b1\'b5\u-3987?\u109?",
                "\u{3bc}\u{b1}\u{221d}\u{3bc}\u{3bc}\n",
                vec![],
            ),
            ("an escaped backslash is text", r"\f1 \\\'5c", "\u{2234}\u{2234}\n", vec![]),
            (
                "Wingdings bytes",
                r"\f2 \'81\'82\'fe\'6f\'fc",
                "\u{2780}\u{2781}[x] [ ] \u{2713}\n",
                vec![],
            ),
            (
                "a Wingdings code its table leaves empty",
                r"\f2 a\'ffb",
                "\u{264b}\u{264c}\n",
                vec![(UnmappedSymbol, Body, 1)],
            ),
            ("a Wingdings ballot box as Unicode", r"\f2\uc0\u-3842 ", "[x]\n", vec![]),
            ("a Symbol group, then a text font", r"{\f1 x}\f0\u-3987?", "\u{3be}\u{3bc}\n", vec![]),
            (
                "a text font with no symbol font before",
                r"\f0 a\u-3987?b",
                "ab\n",
                vec![(UnmappedSymbol, Body, 1)],
            ),
            (
                "the ends of the private-use symbols with no symbol font before",
                r"\f0 a\u-4064?\u-3841?b",
                "ab\n",
                vec![(UnmappedSymbol, Body, 2)],
            ),
            (
                "a text font below the private-use symbols",
                r"{\f1 x}\f0\u-4065?\u-3840?",
                "\u{3be}\u{f01f}\u{f100}\n",
                vec![],
            ),
            (
                "a stylesheet font",
                r"{\stylesheet{\s5\f1 Symbolic;}}\s5\f0 m\u-3987?",
                "m\n",
                vec![(UnmappedSymbol, Body, 1)],
            ),
            (
                "a charset-2 font no table knows",
                r"\f3 a\'e9\f0\u-3987?",
                "a\u{e9}\u{f06d}\n",
                vec![],
            ),
            ("a Wingdings group, then a text font", r"{\f2 a}\f0\u-3842?", "\u{264b}[x]\n", vec![]),
        ]);
    }

    #[test]
    fn deff_sets_text_no_f_sets_and_plain_returns_to_it() {
        for (src, expected) in [
            (r"{\rtf1\deff1{\fonttbl{\f0 Times;}{\f1 Symbol;}}m\par}", "\u{3bc}\n"),
            (r"{\rtf1\deff0{\fonttbl{\f0 Times;}{\f1 Symbol;}}\f1 m\plain m\par}", "\u{3bc}m\n"),
            (r"{\rtf1\deff0{\fonttbl{\f0 Times;}{\f1 Symbol;}}{\f1 m}m\par}", "\u{3bc}m\n"),
            (
                r"{\rtf1\ansicpg1252\deff1{\fonttbl{\f0\fcharset0 Times;}{\f1\fcharset204 Times;}}\'e0\par}",
                "\u{430}\n",
            ),
            (
                r"{\rtf1\ansicpg1252\deff0{\fonttbl{\f0\fcharset0 Times;}{\f1\fcharset204 Times;}}\f1 \'e0\plain \'e0\par}",
                "\u{430}\u{e0}\n",
            ),
            (r"{\rtf1\deff9{\fonttbl{\f0 Times;}{\f1 Symbol;}}m\par}", "m\n"),
        ] {
            assert_eq!(converted(src), (expected.to_owned(), vec![]), "{src}");
        }
    }

    #[test]
    fn a_font_table_entry_is_named_by_its_own_text_braced_or_run_on() {
        let run_on =
            r"{\rtf1{\fonttbl\f0\fcharset0 Times;\f1\fcharset2 Symbol;}\f1 m\'b1\f0 m\par}";
        let braced =
            r"{\rtf1{\fonttbl{\f0\fcharset0 Times;}{\f1\fcharset2 Symbol;}}\f1 m\'b1\f0 m\par}";
        assert_eq!(markdown(run_on), "\u{3bc}\u{b1}m\n");
        assert_eq!(markdown(braced), markdown(run_on));
        let nested = r"{\rtf1{\fonttbl{\f0 Times;}{\f1\fcharset2{\*\falt \f7 Symbol}{\*\panose 0}Wingdings;}}\f1 \'fc\f7 x\par}";
        let expected = vec![(DiagnosticCode::FormattingUnreadable, Region::Body, 1)];
        assert_eq!(converted(nested), ("\u{2713}x\n".to_owned(), expected));
    }

    #[test]
    fn an_undefined_font_reports_once_per_number_and_hidden_codes_their_omission_only() {
        use DiagnosticCode::{FormattingUnreadable, HiddenOmitted, RevisionOmitted};
        use Region::{Body, Note};
        check_symbols(vec![
            ("an undefined font", r"\f9 x", "x\n", vec![(FormattingUnreadable, Body, 1)]),
            (
                "two undefined fonts",
                r"\f9 x{\f9 y}\f8 z",
                "xyz\n",
                vec![(FormattingUnreadable, Body, 2)],
            ),
            (
                "an undefined font in a footnote",
                r"a{\footnote\f9 x}",
                "a[^1]\n\n[^1]: x\n",
                vec![(FormattingUnreadable, Note, 1)],
            ),
            ("an undefined font over whitespace", r"a\f9\tab\f0 x", "a x\n", vec![]),
            (
                "hidden text in an undefined font",
                r"a{\v\f9 x}",
                "a\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "deleted text in an undefined font",
                r"a{\deleted\f9 x}",
                "a\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
            ("a hidden C1 code", r"a{\v\f2 \'81}", "a\n", vec![(HiddenOmitted, Body, 1)]),
            ("a hidden ballot box", r"a{\v\f2 \'fe}", "a\n", vec![(HiddenOmitted, Body, 1)]),
            ("a hidden unmapped code", r"a{\v\f2 \'ff}", "a\n", vec![(HiddenOmitted, Body, 1)]),
            (
                "a hidden private-use symbol",
                r"a{\v\f0\u-3987?}",
                "a\n",
                vec![(HiddenOmitted, Body, 1)],
            ),
            (
                "a deleted unmapped code",
                r"a{\deleted\f2 \'ff}",
                "a\n",
                vec![(RevisionOmitted, Body, 1)],
            ),
        ]);
        for src in [r"{\rtf1 \f9 x\par}", r"{\rtf1\deff9{\fonttbl{\f0 Times;}}x\par}"] {
            assert_eq!(reported(src), [], "{src}");
        }
    }

    #[test]
    fn captured_and_generated_text_in_a_symbol_font_is_never_decoded() {
        let lists = r"{\*\listtable{\list{\listlevel\levelnfc23{\leveltext\'01\'b7;}{\levelnumbers;}}\listid1}}{\*\listoverridetable{\listoverride\listid1\ls1}}";
        let src = format!(
            r"{{\rtf1\ansi\ansicpg1252{SYMBOL_FONTS}{lists}\f1 a\bullet\emdash\~\_\endash\lquote\rdblquote b\par}}"
        );
        assert_eq!(markdown(&src), "\u{3b1}\u{2022}\u{2014} -\u{2013}\u{2018}\u{201d}\u{3b2}\n");
        let link = symbols(
            r#"{\field{\*\fldinst\f1 HYPERLINK "http://www.example.org/m"}{\fldrslt\f0 www.example.org}}"#,
        );
        assert_eq!(markdown(&link), "[www.example.org](http://www.example.org/m)\n");
        let listed = format!(
            r"{{\rtf1\ansi\ansicpg1252{SYMBOL_FONTS}{lists}\pard{{\listtext\f1 \'b7\tab}}\ls1\f0 item\par}}"
        );
        assert_eq!(markdown(&listed), "- item\n");
        assert_eq!(markdown(&symbols(r"{\mmath{\*\moMath{\mr\f1 a}}}")), "$a$\n");
        let doc = parse(
            &ParseCtx::default(),
            symbols(r"{\*\bkmkstart\f1 m}x{\pict{\*\picprop{\sp{\sn wzDescription}{\sv\f1 abc}}}\pngblip 89504e470d0a1a0a}").as_bytes(),
        )
        .unwrap();
        let Some(Block::Paragraph(content)) = doc.blocks.first() else {
            panic!("{:?}", doc.blocks)
        };
        assert!(content.contains(&Inline::Anchor("m".to_owned())), "{content:?}");
        assert!(
            content
                .iter()
                .any(|inline| matches!(inline, Inline::Image { alt, .. } if alt == "abc")),
            "{content:?}"
        );
    }

    #[test]
    fn a_font_table_is_read_once_whatever_its_names_and_uses() {
        let long = "S".repeat(4096);
        let body = format!(r"{{\f1\fcharset2 Symbol;}}{{\f2\fcharset2 {long};}}");
        let uses = r"{\f1 m}".repeat(1000);
        let src = format!(r"{{\rtf1{{\fonttbl{body}}}{uses}\f2 m\par}}");
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, src.as_bytes()).unwrap();
        let markdown = crate::render::markdown::document_to_markdown(&doc);
        assert_eq!(markdown, format!("{}m\n", "\u{3bc}".repeat(1000)));
        assert_eq!(parse_ctx.fuel(), fuel(&src) + fuel(&body));
    }

    const LISTS_932: &str = r"{\rtf1\ansi\ansicpg932{\*\listtable{\list{\listlevel\levelnfc0{\leveltext \'03\'00\'82.;}{\levelnumbers \'01;}}{\listlevel\levelnfc0{\leveltext \'02\'01\'82;}{\levelnumbers \'01;}}\listid1}{\list{\listlevel\levelnfc0{\leveltext \'02\'00.;}{\levelnumbers \'01;}}\listid2}}{\*\listoverridetable{\listoverride\listid1\ls1}{\listoverride\listid2\ls3}{\listoverride\listid2\listoverridecount1{\lfolevel\listoverrideformat{\listlevel\levelnfc0{\leveltext \'02\'00\'82;}{\levelnumbers \'01;}}}\ls2}}";

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of losses by where their text sat")]
    fn each_loss_is_reported_where_its_text_sat() {
        use DiagnosticCode::*;
        use Region::{Body, Comment, Container, Footer, Header, Note};
        let lists = |body: &str| format!("{LISTS_932}{body}}}");
        let cases: Vec<(&str, String, Vec<Reported>)> = vec![
            ("groups open at the end", r"{\rtf1 a{\b b".into(), vec![(UnclosedStructure, Body, 1)]),
            ("the document group open at the end", r"{\rtf1 a".into(), vec![(UnclosedStructure, Body, 1)]),
            ("closes with no open group", r"{\rtf1 a}}}".into(), vec![(UnmatchedClose, Body, 2)]),
            ("a picture of an unread format", r"{\rtf1 {\pict\macpict 0102}}".into(), vec![(ImageUnavailable, Body, 1)]),
            ("an untyped picture", r"{\rtf1 {\pict 0102}}".into(), vec![(ImageUnavailable, Body, 1)]),
            ("an unread picture in a footnote", r"{\rtf1 a{\footnote b{\pict\wbitmap 01}}}".into(), vec![(ImageUnavailable, Note, 1)]),
            ("an unread fallback picture", r"{\rtf1 {\nonshppict{\pict\wbitmap 01}}}".into(), vec![]),
            ("an embedded object", r"{\rtf1 {\object\objemb{\*\objdata 0102}{\result x}}}".into(), vec![(EmbeddedObject, Body, 1)]),
            ("an embedded object in a footnote", r"{\rtf1 a{\footnote {\object{\*\objdata 01}}}}".into(), vec![(EmbeddedObject, Note, 1)]),
            ("headers and footers", r"{\rtf1 {\header h}{\footerl f}{\footerr {\object{\*\objdata 01}}\chpgn} b}".into(), vec![(HeaderOmitted, Header, 1), (FooterOmitted, Footer, 2)]),
            ("a comment", r"{\rtf1 a{\*\atnid x}{\*\annotation c}}".into(), vec![(CommentOmitted, Comment, 1)]),
            ("a comment in a header", r"{\rtf1 {\headerf {\*\annotation c}}}".into(), vec![(HeaderOmitted, Header, 1)]),
            ("a damaged label two paragraphs show", lists(r"\pard\ls1 one\par\pard\ls1 two\par"), vec![(InvalidEncoding, Body, 1)]),
            ("a damaged label of an override", lists(r"\pard\ls2 one\par"), vec![(InvalidEncoding, Body, 1)]),
            ("damaged labels of two levels", lists(r"\pard\ls1 one\par\pard\ls1\ilvl1 two\par"), vec![(InvalidEncoding, Body, 2)]),
            ("a clean label", lists(r"\pard\ls3 one\par"), vec![]),
            ("a damaged label no paragraph shows", lists(r"\pard one\par"), vec![]),
            ("a damaged style name", r"{\rtf1\ansi\ansicpg932{\stylesheet{\s1 \'82;}}a}".into(), vec![(InvalidStyleEncoding, Container, 1)]),
            ("a style cycle three styles reach", r"{\rtf1{\stylesheet{\s1\sbasedon2 A;}{\s2\sbasedon1 B;}{\s3\sbasedon1 C;}}a}".into(), vec![(StyleCycle, Container, 1)]),
            ("two style cycles", r"{\rtf1{\stylesheet{\s1\sbasedon2 A;}{\s2\sbasedon1 B;}{\s3\sbasedon4 C;}{\s4\sbasedon3 D;}}a}".into(), vec![(StyleCycle, Container, 2)]),
            ("a style based on itself", r"{\rtf1{\stylesheet{\s1\sbasedon1 A;}}a}".into(), vec![]),
            ("a lone lead byte ended by a paragraph", r"{\rtf1\ansi\ansicpg932 a\'82\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte ended by a group", r"{\rtf1\ansi\ansicpg932 a{\b\'82}b\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte in a footnote", r"{\rtf1\ansi\ansicpg932 a{\footnote \'82}}".into(), vec![(InvalidEncoding, Note, 1)]),
            ("a lone lead byte in math", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\mr \'82}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte in a bookmark name", r"{\rtf1\ansi\ansicpg932 {\*\bkmkstart \'82}a\par}".into(), vec![]),
            ("a lone lead byte in a header", r"{\rtf1\ansi\ansicpg932 {\header \'82}a\par}".into(), vec![(HeaderOmitted, Header, 1)]),
            ("a lone lead byte in a label a heading drops", r"{\rtf1\ansi\ansicpg932\outlinelevel0{\listtext \'82}Heading\par}".into(), vec![]),
            ("a guessed blank label", r"{\rtf1\ansicpg1361 \pard{\listtext \'a0}\ls5 x\par}".into(), vec![]),
            ("a lone lead byte in a label shown", r"{\rtf1\ansi\ansicpg932 \pard{\listtext \'82}\ls5 x\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte in a label read for its number", r"{\rtf1\ansi\ansicpg932 \pard{\pntext 1\'82}\pnlvlbody x\par}".into(), vec![]),
            ("a lone lead byte in a link target", r#"{\rtf1\ansi\ansicpg932 {\field{\*\fldinst HYPERLINK "http://x/\'82"}{\fldrslt link}}\par}"#.into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte in a link tooltip", r#"{\rtf1\ansi\ansicpg932 {\field{\*\fldinst HYPERLINK "http://x/" \\o "\'82"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a guessed code page in a link target", r#"{\rtf1\ansicpg1361 {\field{\*\fldinst HYPERLINK "http://x/\'8e"}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed space before a guessed link target", r#"{\rtf1\ansicpg1361 {\field{\*\fldinst HYPERLINK\'a0"http://x/\'8e"}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed soft hyphen in the keyword of a guessed link target", r#"{\rtf1\ansicpg1361 {\field{\*\fldinst HYPER\'adLINK "http://x/\'8e"}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed space before a link anchor", r#"{\rtf1\ansicpg1361 {\field{\*\fldinst HYPERLINK "http://x/" \\l \'a0a}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a guessed space after a bare link target", r"{\rtf1\ansicpg1361 {\field{\*\fldinst HYPERLINK http://x/\'a0\\l a}{\fldrslt link}}\par}".into(), vec![]),
            ("a guessed soft hyphen ending a bare link target", r"{\rtf1\ansicpg1361 {\field{\*\fldinst HYPERLINK http://x/\'ad \\l a}{\fldrslt link}}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a dropped character ending a bare link target", r"{\rtf1 {\field{\*\fldinst HYPERLINK http://x/\u56320? \\l a}{\fldrslt link}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a guessed code page in a link tooltip", r#"{\rtf1\ansicpg1361 {\field{\*\fldinst HYPERLINK "http://x/" \\o "\'8e"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a guessed code page in a link anchor", r#"{\rtf1\ansicpg1361 {\field{\*\fldinst HYPERLINK "http://x/" \\l \'8e}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed code page in an unresolved anchor link", r#"{\rtf1\ansicpg1361 {\field{\*\fldinst HYPERLINK \\l "\'8e"}{\fldrslt link}}\par}"#.into(), vec![(DanglingReference, Body, 1)]),
            ("a guessed code page in an anchor link to a bookmark", r#"{\rtf1\ansicpg1361 {\*\bkmkstart \'8e}x{\field{\*\fldinst HYPERLINK \\l "\'8e"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a guessed code page in an anchor link to a heading", r#"{\rtf1\ansicpg1361 \outlinelevel0{\*\bkmkstart \'8e}Head\par\pard {\field{\*\fldinst HYPERLINK \\l "\'8e"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a guessed code page in a hash link to a heading", r##"{\rtf1\ansicpg1361 \outlinelevel0{\*\bkmkstart \'8e}Head\par\pard {\field{\*\fldinst HYPERLINK "#\'8e"}{\fldrslt link}}\par}"##.into(), vec![]),
            ("a guessed byte that cleaning removes in a link anchor", r#"{\rtf1\ansicpg1361 {\field{\*\fldinst HYPERLINK \\l "a\'8fb"}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1), (DanglingReference, Body, 1)]),
            ("a lone low surrogate", r"{\rtf1 a\u56320?b\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a high surrogate before a letter", r"{\rtf1 \u55296?\u65?\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a surrogate pair", r"{\rtf1 \u55357?\u56832?\par}".into(), vec![]),
            ("a lone surrogate in a footnote", r"{\rtf1 a{\footnote \u56320?}}".into(), vec![(InvalidEncoding, Note, 1)]),
            ("a check box", r"{\rtf1 {\field{\*\fldinst FORMCHECKBOX {\*\formfield{\fftype1\ffres1{\*\ffname Check1}}}}{\fldrslt }}\par}".into(), vec![(FormControlUnread, Body, 1)]),
            ("a drop-down named first", r"{\rtf1 {\field{\*\fldinst {\*\formfield{\fftype2{\*\ffname Drop1}}} FORMDROPDOWN}{\fldrslt }}\par}".into(), vec![(FormControlUnread, Body, 1)]),
            ("a check box in a footnote", r"{\rtf1 a{\footnote {\field{\*\fldinst FORMCHECKBOX}{\fldrslt }}}}".into(), vec![(FormControlUnread, Note, 1)]),
            ("a text form field", r"{\rtf1 {\field{\*\fldinst FORMTEXT {\*\formfield{\fftype0}}}{\fldrslt typed}}\par}".into(), vec![]),
            ("page, date and time characters", r"{\rtf1 Page \chpgn, \chdate\chtime\chdpl\chdpa\par}".into(), vec![(FieldValueUnread, Body, 5)]),
            ("a page character in a footnote", r"{\rtf1 a{\footnote \chpgn}}".into(), vec![(FieldValueUnread, Note, 1)]),
            ("deleted ranges", r"{\rtf1 a{\deleted b}c{\deleted d\par e}f{\revised g}\deleted h\plain i\par}".into(), vec![(RevisionOmitted, Body, 4)]),
            ("deleted ranges in two paragraphs", r"{\rtf1 {\deleted a}\par{\deleted b}\par}".into(), vec![(RevisionOmitted, Body, 2)]),
            ("a deleted range in a footnote", r"{\rtf1 a{\footnote b{\deleted c}}}".into(), vec![(RevisionOmitted, Note, 1)]),
            ("a damaged deleted range", r"{\rtf1\ansi\ansicpg932 a{\deleted \'82\chpgn}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a Mac code page", r"{\rtf1\mac \'8e{\b\'8f}\par}".into(), vec![]),
            ("the Mac Roman code page", r"{\rtf1\ansi\ansicpg10000 \'8e\par}".into(), vec![]),
            ("the UTF-8 code page", r"{\rtf1\ansi\ansicpg65001 \'c3\'a9\par}".into(), vec![]),
            ("a Mac font charset", r"{\rtf1\ansi\ansicpg1252{\fonttbl{\f1\fcharset77 M;}}a\f1 \'8e\par}".into(), vec![]),
            ("a guessed code page in a footnote", r"{\rtf1\ansicpg1361 a{\footnote \'8e}}".into(), vec![(EncodingAssumed, Note, 1)]),
            ("a guessed code page over ASCII", r"{\rtf1\ansicpg1361 plain\par}".into(), vec![]),
            ("a declared code page", r"{\rtf1\ansi\ansicpg1252{\fonttbl{\f1\fcharset0 A;}{\f2\fcharset2 S;}}\'e9\f1\'e9\f2\'e9\par}".into(), vec![]),
            ("a guessed byte that cleaning removes", r"{\rtf1\ansicpg1361 a{\'ad}b\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a PC code page", r"{\rtf1\pca \'8e\par}".into(), vec![]),
            ("a high surrogate before text", r"{\rtf1 a\u55296? b\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a high surrogate at a paragraph end", r"{\rtf1 a\u55296?\par b\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a high surrogate at the end", r"{\rtf1 a\u55296?}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("surrogate halves split by a paragraph", r"{\rtf1 a\u55357? b\par c\u56832? d\par}".into(), vec![(InvalidEncoding, Body, 2)]),
            ("a high surrogate in a footnote before body text", r"{\rtf1 a{\footnote b\u55296?} c\par}".into(), vec![(InvalidEncoding, Note, 1)]),
            ("a lone surrogate in a link target", r#"{\rtf1 {\field{\*\fldinst HYPERLINK "http://x/\u56320?"}{\fldrslt link}}\par}"#.into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone surrogate in a link tooltip", r#"{\rtf1 {\field{\*\fldinst HYPERLINK "http://x/" \\o "\u55296?"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a damaged link target with no result", r#"{\rtf1\ansi\ansicpg932 {\field{\*\fldinst HYPERLINK "http://x/\'82"}{\fldrslt }}\par}"#.into(), vec![]),
            ("a clean Unicode link target beside a guessed tooltip", r#"{\rtf1\ansicpg1361 {\field{\*\fldinst HYPERLINK "http://x/\u233?" \\o "\'8e"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a lone lead byte in a shown degree", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\mrad{\mdeg{\mr \'82}}{\me{\mr x}}}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte in a hidden degree", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\mrad{\mradPr{\mdegHide1}}{\mdeg{\mr \'82}}{\me{\mr x}}}}}\par}".into(), vec![]),
            ("a lone lead byte in a hidden limit", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\mnary{\mnaryPr{\msubHide1}}{\msub{\mr \'82}}{\msup{\mr n}}{\me{\mr i}}}}}\par}".into(), vec![]),
            ("a lone surrogate in math", r"{\rtf1 {\mmath{\*\moMath{\mr x\u56320?}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a deleted check box", r"{\rtf1 a{\deleted {\field{\*\fldinst FORMCHECKBOX}{\fldrslt }}}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted unread picture", r"{\rtf1 a{\deleted {\pict\macpict 01}}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted picture", r"{\rtf1 a{\deleted {\pict\pngblip 89504e47}}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted object", r"{\rtf1 a{\deleted {\object{\*\objdata 01}{\result x}}}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted equation", r"{\rtf1 a{\deleted{\mmath{\*\moMath{\mr x}}}}b\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted line break", r"{\rtf1 a{\deleted b\line c}d\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted paragraph in a header", r"{\rtf1 {\header {\deleted x\par}}a\par}".into(), vec![(HeaderOmitted, Header, 1)]),
            ("an object preview in a header", r"{\rtf1{\header{\object{\result{\pict\wbitmap 01}}}}b}".into(), vec![(HeaderOmitted, Header, 1)]),
            ("a damaged label in a table cell", r"{\rtf1\ansi\ansicpg932 \trowd\cellx1000\pard\intbl{\listtext \'82}\ls5 a\cell\row\pard{\listtext 1.}\ls5 b\par}".into(), vec![]),
            ("a damaged label in a header", r"{\rtf1\ansi\ansicpg932 {\header {\listtext \'82}}\pard\ls5 a\par}".into(), vec![(HeaderOmitted, Header, 1)]),
            ("a high surrogate split from its pair by text", r"{\rtf1 \u55357? b\u56832?\par}".into(), vec![(InvalidEncoding, Body, 2)]),
            ("a guessed code page in math", r"{\rtf1\ansicpg1361 {\mmath{\*\moMath{\mr \'8e}}}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a deleted picture in a header", r"{\rtf1 {\header {\deleted {\pict\pngblip 89}}}a\par}".into(), vec![(HeaderOmitted, Header, 1)]),
            ("an empty unread picture", r"{\rtf1 {\pict\macpict}}".into(), vec![(ImageUnavailable, Body, 1)]),
            ("a guessed code page in a degree shown after a damaged flag", r"{\rtf1\ansicpg1361 {\mmath{\*\moMath{\mrad{\mradPr{\mdegHide \'ad0}}{\mdeg{\mr \'8e}}{\me{\mr x}}}}}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed code page in a hidden degree", r"{\rtf1\ansicpg1361 {\mmath{\*\moMath{\mrad{\mradPr{\mdegHide1}}{\mdeg{\mr \'8e}}{\me{\mr x}}}}}\par}".into(), vec![]),
            ("a lone surrogate in a hidden degree", r"{\rtf1 {\mmath{\*\moMath{\mrad{\mradPr{\mdegHide1}}{\mdeg{\mr \u56320?}}{\me{\mr x}}}}}\par}".into(), vec![]),
            ("a high surrogate dropped in a hidden limit", r"{\rtf1 {\mmath{\*\moMath{\mnary{\mnaryPr{\msubHide1}}{\msub{\mr i\u55296?}}{\msup{\mr n}}{\me{\mr i}}}}}\par}".into(), vec![]),
            ("a high surrogate dropped in a shown limit", r"{\rtf1 {\mmath{\*\moMath{\mnary{\msub{\mr i\u55296?}}{\msup{\mr n}}{\me{\mr i}}}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a guessed code page in a shown degree", r"{\rtf1\ansicpg1361 {\mmath{\*\moMath{\mrad{\mdeg{\mr \'8e}}{\me{\mr x}}}}}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a deleted check box instruction", r"{\rtf1 a{\field{\*\fldinst {\deleted FORMCHECKBOX}}{\fldrslt }}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted damaged link target", r#"{\rtf1\ansi\ansicpg932 {\field{\*\fldinst HYPERLINK "http://x/{\deleted \'82}"}{\fldrslt link}}\par}"#.into(), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted lone surrogate in a link target", r#"{\rtf1 {\field{\*\fldinst HYPERLINK "http://x/{\deleted \u56320?}"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a deleted math run", r"{\rtf1 a{\mmath{\*\moMath{\mr {\deleted x}y}}}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted damaged math run", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\mr {\deleted \'82}x}}}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted lone surrogate in math", r"{\rtf1 {\mmath{\*\moMath{\mr {\deleted \u56320?}x}}}\par}".into(), vec![]),
            ("noncharacters in math", r"{\rtf1 {\mmath{\*\moMath{\mr \u64976?\u64977?\u65533?x}}}\par}".into(), vec![]),
            ("a replacement character in math", r"{\rtf1 {\mmath{\*\moMath{\mr \u25955?3\u65533?}}}\par}".into(), vec![]),
            ("a replacement character in a link target", r#"{\rtf1 {\field{\*\fldinst HYPERLINK "http://x/\u25955?3\u65533?"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("noncharacters in a link target", r#"{\rtf1 {\field{\*\fldinst HYPERLINK "http://x/\u25799?6\u64976?\u64977?"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a guessed accent character", r"{\rtf1\ansicpg1361 {\mmath{\*\moMath{\macc{\maccPr{\mchr \'a8}}{\me{\mr x}}}}}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a lone lead byte as a delimiter", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\md{\mdPr{\mbegChr \'82}}{\me{\mr x}}}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte as a closing delimiter", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\md{\mdPr{\mendChr \'82}}{\me{\mr x}}}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte as a shown separator", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\md{\mdPr{\msepChr \'82}}{\me{\mr x}}{\me{\mr y}}}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte as an unused separator", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\md{\mdPr{\msepChr \'82}}{\me{\mr x}}}}}\par}".into(), vec![]),
            ("a lone lead byte as an operator", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\mnary{\mnaryPr{\mchr \'82}}{\me{\mr x}}}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte as a group character", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\mgroupChr{\mgroupChrPr{\mchr \'82}}{\me{\mr x}}}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte as a delimiter in a hidden degree", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\mrad{\mradPr{\mdegHide1}}{\mdeg{\md{\mdPr{\mbegChr \'82}}{\me{\mr y}}}}{\me{\mr x}}}}}\par}".into(), vec![]),
            ("a deleted check box with plain instruction text", r"{\rtf1 a{\deleted {\field{\*\fldinst {\plain FORMCHECKBOX}}{\fldrslt }}}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted soft hyphen in math", r"{\rtf1 {\mmath{\*\moMath{\mr {\deleted \'ad}x}}}\par}".into(), vec![]),
            ("a deleted guessed math run", r"{\rtf1\ansicpg1361 {\mmath{\*\moMath{\mr {\deleted \'8e}x}}}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a fallback picture in a header", r"{\rtf1 {\header {\*\shppict{\pict\wbitmap 01}}}a\par}".into(), vec![(HeaderOmitted, Header, 1)]),
            ("shape text in a header", r"{\rtf1 {\header {\shp{\*\shpinst{\shptxt \chpgn}}}}a\par}".into(), vec![(HeaderOmitted, Header, 1)]),
            ("a footnote in a header", r"{\rtf1 {\header a{\footnote \chpgn}}b\par}".into(), vec![(HeaderOmitted, Header, 1)]),
            ("a form field in an unknown destination", r"{\rtf1 {\*\xyz {\field{\*\fldinst FORMCHECKBOX}{\fldrslt x}}}a\par}".into(), vec![]),
            ("a guessed byte that cleaning removes in math", r"{\rtf1\ansicpg1361 {\mmath{\*\moMath{\mr \'8f}}}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed byte that cleaning removes in a link target", r#"{\rtf1\ansicpg1361 {\field{\*\fldinst HYPERLINK "http://x/\'8f"}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed space in math", r"{\rtf1\ansicpg1361 {\mmath{\*\moMath{\mr x\'a0y}}}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed space in a link target", r#"{\rtf1\ansicpg1361 {\field{\*\fldinst HYPERLINK "http://x/a\'a0b"}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
            ("a deleted range in a footnote after one in the body", r"{\rtf1 a{\deleted b}{\footnote {\deleted c}}\par}".into(), vec![(RevisionOmitted, Body, 1), (RevisionOmitted, Note, 1)]),
            ("a deleted structure in an equation", r"{\rtf1 {\mmath{\*\moMath{\deleted{\mf{\mnum{\mr b}}{\mden{\mr c}}}}{\mr x}}}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("two deleted ranges in a math run", r"{\rtf1 {\mmath{\*\moMath{\mr {\deleted a}x{\deleted b}}}}\par}".into(), vec![(RevisionOmitted, Body, 2)]),
            ("two deleted ranges in an instruction", r#"{\rtf1 {\field{\*\fldinst {\deleted a}HYPERLINK "http://x/"{\deleted b}}{\fldrslt link}}\par}"#.into(), vec![(RevisionOmitted, Body, 2)]),
        ];
        for (case, src, mut expected) in cases {
            expected.sort();
            assert_eq!(reported(&src), expected, "{case}");
        }
    }

    #[test]
    fn accepted_revisions_drop_deleted_text_and_join_at_a_deleted_paragraph_mark() {
        for (source, expected) in [
            (
                r"{\rtf1 a{\deleted b}c{\deleted d\par e}f{\revised g}\deleted h\plain i\par}",
                "acfgi\n",
            ),
            (r"{\rtf1 one\deleted\par\deleted0 two\par}", "onetwo\n"),
            (r"{\rtf1 a{\deleted b\line c}d\par}", "ad\n"),
            (r"{\rtf1 a{\deleted{\mmath{\*\moMath{\mr x}}}}b\par}", "ab\n"),
            (
                r"{\rtf1 a{\deleted{\mmath{\*\moMath{\mf{\mnum{\mr b}}{\mden{\mr c}}}}}}d\par}",
                "ad\n",
            ),
            (
                r"{\rtf1 {\mmath{\*\moMath{\deleted{\mf{\mnum{\mr b}}{\mden{\mr c}}}}{\mr x}}}\par}",
                "$x$\n",
            ),
        ] {
            assert_eq!(markdown(source), expected, "{source}");
        }
        let doc = parse(&ParseCtx::default(), br"{\rtf1 a{\deleted {\pict\pngblip 89504e47}}\par}");
        assert_eq!(doc.unwrap().assets, []);
    }

    #[test]
    fn a_line_break_in_a_link_target_is_one_space() {
        assert_eq!(
            markdown(
                r#"{\rtf1 {\field{\*\fldinst HYPERLINK "http://x/a\'0d\'0ab"}{\fldrslt link}}\par}"#
            ),
            "[link](<http://x/a b>)\n"
        );
    }

    #[test]
    fn an_omitted_header_keeps_its_object_preview_out() {
        assert_eq!(markdown(r"{\rtf1{\header{\object{\result h}}}b\par}"), "b\n");
    }

    #[test]
    fn an_omitted_header_keeps_its_destinations_out() {
        for src in [
            r"{\rtf1 a{\header {\*\shppict{\pict\pngblip 89}}}b\par}",
            r"{\rtf1 a{\header {\shp{\*\shpinst{\shptxt h}}}}b\par}",
            r"{\rtf1 a{\header {\footnote h}}b\par}",
            r"{\rtf1 a{\header {\*\bkmkstart h\par}}b\par}",
            r"{\rtf1 a{\header {\*\nesttableprops\par}}b\par}",
        ] {
            assert_eq!(markdown(src), "ab\n", "{src}");
        }
    }

    #[test]
    fn accepted_revisions_drop_deleted_text_from_equations_and_instructions() {
        assert_eq!(markdown(r"{\rtf1 a{\mmath{\*\moMath{\mr {\deleted x}y}}}\par}"), "a$y$\n");
        let link =
            r#"{\rtf1 {\field{\*\fldinst HYPERLINK "http://x/{\deleted y}z"}{\fldrslt link}}\par}"#;
        assert_eq!(markdown(link), "[link](http://x/z)\n");
    }

    #[test]
    fn a_marked_equation_renders_without_its_markers() {
        let equation = r"{\mmath{\*\moMath{\msSup{\me{\mr x}}{\msup{\mr \'e9}}}}}\par}";
        let guessed = markdown(&format!(r"{{\rtf1\ansicpg1361 {equation}"));
        assert_eq!(guessed, markdown(&format!(r"{{\rtf1\ansi\ansicpg1252 {equation}")));
        assert!(guessed.contains('é'), "{guessed}");
    }

    #[test]
    fn one_display_equation_displays_every_equation_of_its_paragraph() {
        let source =
            r"{\rtf1 {\mmath{\*\moMathPara{\moMath{\mr y}}}}{\mmath{\*\moMath{\mr x}}}\par}";
        assert_eq!(markdown(source), "$$\ny\n$$\n\n$$\nx\n$$\n");
    }

    #[test]
    fn a_display_equation_in_a_cell_leaves_the_next_paragraph_inline() {
        let after = r"\pard {\mmath{\*\moMath{\mr x}}}\par}";
        let y = r"{\mmath{\*\moMathPara{\moMath{\mr y}}}}";
        let table = |shown: &str| format!("|  |\n| --- |\n| {shown} |\n");
        let cases = [
            (format!(r"{y}\cell\row"), format!("{}\n$x$\n", table("$y$"))),
            (format!(r"{y}\row"), format!("{}\n$x$\n", table("$y$"))),
            // An empty display equation shows nothing, so its `\row` closes no cell.
            (r"a\cell {\mmath{\*\moMathPara}}\row".to_owned(), format!("{}\n$x$\n", table("a"))),
            // With no `\row`, the row closes at the end of the document, after `x`.
            (format!(r"{y}\cell"), format!("$x$\n\n{}", table("$y$"))),
        ];
        for (row, expected) in cases {
            let source = format!(r"{{\rtf1 \trowd\cellx1000\pard\intbl {row}{after}");
            assert_eq!(markdown(&source), expected, "{row}");
        }
    }

    #[test]
    fn a_label_left_in_a_row_does_not_label_the_next_paragraph() {
        check(vec![
            (
                "a label after the last cell",
                r"{\rtf1 \trowd\cellx1000\pard\intbl a\cell {\listtext 1.\tab}\row\pard b\par}".into(),
                Some("|  |\n| --- |\n| a |\n\nb\n"),
                vec![],
            ),
            (
                "a label in a row with no cell",
                r"{\rtf1 \trowd\cellx1000\pard\intbl {\listtext 1.\tab}\row\pard b\par}".into(),
                Some("b\n"),
                vec![],
            ),
            (
                "a damaged label after the last cell",
                r"{\rtf1\ansi\ansicpg932 \trowd\cellx1000\pard\intbl a\cell {\listtext \'82}\row\pard{\listtext 1.}\ls5 b\par}".into(),
                Some("|  |\n| --- |\n| a |\n\n- 1\\. b\n"),
                vec![],
            ),
        ]);
    }

    #[test]
    fn a_paragraph_mark_in_a_note_is_a_line_break() {
        let source = r"{\rtf1 x{\footnote a\par b}y\par}";
        assert_eq!(markdown(source), "x[^1]y\n\n[^1]: a\\\n    b\n");
    }

    fn anchors(source: &str) -> Vec<String> {
        let doc = parse(&ParseCtx::default(), source.as_bytes()).unwrap();
        let [Block::Paragraph(content)] = &doc.blocks[..] else {
            panic!("unexpected blocks: {:?}", doc.blocks)
        };
        content
            .iter()
            .filter_map(|inline| match inline {
                Inline::Anchor(name) => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_bookmark_anchors_once_when_its_group_ends_under_its_trimmed_name() {
        assert_eq!(anchors(r"{\rtf1 x{\*\bkmkstart ab{\b c}d}y}"), ["abcd"]);
        assert_eq!(anchors(r"{\rtf1 x{\*\bkmkstart  name }y}"), ["name"]);
    }

    #[test]
    fn a_noncharacter_no_reader_places_is_a_property_character() {
        let src =
            r"{\rtf1 {\mmath{\*\moMath{\macc{\maccPr{\mchr \u64978?\u776?}}{\me{\mr x}}}}}\par}";
        assert_eq!(markdown(src), "$\\hat{x}$\n");
    }

    #[test]
    fn a_form_field_keeps_its_data_out_of_the_instruction() {
        let src = r#"{\rtf1 {\field{\*\fldinst {\*\formfield{\*\ffname x}} HYPERLINK "http://x/"}{\fldrslt link}}\par}"#;
        assert_eq!(markdown(src), "[link](http://x/)\n");
    }

    const HIDING_STYLES: &str = r"{\stylesheet{\s1\v Hidden;}{\s2\v0 Shown;}{\s3\sbasedon1 Child;}{\s4 Plain;}{\*\cs5\v Hidden Char;}}";

    fn hidden(region: Region, count: u64) -> Reported {
        (DiagnosticCode::HiddenOmitted, region, count)
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of hiding controls and styles")]
    fn hidden_text_follows_v_plain_and_paragraph_styles() {
        use DiagnosticCode::{EncodingAssumed, InvalidEncoding};
        use Region::Body;
        let styled = |body: &str| format!(r"{{\rtf1{HIDING_STYLES}{body}}}");
        check(vec![
            ("hidden text", r"{\rtf1 a{\v b}c\par}".into(), Some("ac\n"), vec![hidden(Body, 1)]),
            (
                "hidden text up to \\v0",
                r"{\rtf1 a\v b\v0 c\par}".into(),
                Some("ac\n"),
                vec![hidden(Body, 1)],
            ),
            ("\\v twice", r"{\rtf1 a\v\v b\v0 c\par}".into(), Some("ac\n"), vec![hidden(Body, 1)]),
            (
                "\\v after \\v0",
                r"{\rtf1 \v a\v0 b\v c\par}".into(),
                Some("b\n"),
                vec![hidden(Body, 2)],
            ),
            (
                "operands other than 0",
                r"{\rtf1 {\v1 a}b{\v-1 c}d{\v99999999999 e}f{\v0 g}\par}".into(),
                Some("bdfg\n"),
                vec![hidden(Body, 3)],
            ),
            ("\\plain", r"{\rtf1 \v a\plain b\par}".into(), Some("b\n"), vec![hidden(Body, 1)]),
            (
                "a hiding style",
                styled(r"\pard\plain\s1 a\par\pard\plain\s4 b\par"),
                Some("b\n"),
                vec![hidden(Body, 1)],
            ),
            (
                "a style based on a hiding one",
                styled(r"\pard\plain\s3 a\par\pard\plain b\par"),
                Some("b\n"),
                vec![hidden(Body, 1)],
            ),
            ("\\v0 after a hiding style", styled(r"\pard\plain\s1\v0 a\par"), Some("a\n"), vec![]),
            ("a style's \\v0", styled(r"\pard\plain\v\s2 a\par"), Some("a\n"), vec![]),
            (
                "a style silent on hiding",
                styled(r"\pard\plain\v\s4 a\par\pard\plain b\par"),
                Some("b\n"),
                vec![hidden(Body, 1)],
            ),
            ("a hiding character style", styled(r"{\cs5 a}\par"), Some("a\n"), vec![]),
            ("\\webhidden", r"{\rtf1 {\webhidden a}\par}".into(), Some("a\n"), vec![]),
            (
                "a Unicode character hidden before its fallback",
                r"{\rtf1 a\v\u8364\v0 ?b\par}".into(),
                Some("ab\n"),
                vec![hidden(Body, 1)],
            ),
            (
                "a hidden lone lead byte",
                r"{\rtf1\ansi\ansicpg932 a{\v \'82}b\par}".into(),
                Some("ab\n"),
                vec![hidden(Body, 1)],
            ),
            (
                "a lead byte hidden before a shown trail byte",
                r"{\rtf1\ansi\ansicpg932 a\v\'82\v0\'a0b\par}".into(),
                Some("a\u{fffd}b\n"),
                vec![hidden(Body, 1), (InvalidEncoding, Body, 1)],
            ),
            ("a hidden lone surrogate", r"{\rtf1 a{\v\u56320?}b\par}".into(), Some("ab\n"), vec![]),
            (
                "a hidden high surrogate at a group end",
                r"{\rtf1 a{\v\u55296?}b\par}".into(),
                Some("ab\n"),
                vec![],
            ),
            (
                "a hidden Unicode space and its fallback",
                r"{\rtf1 a{\v\u32?}b\par}".into(),
                Some("ab\n"),
                vec![],
            ),
            (
                "hidden text in a guessed code page",
                r"{\rtf1\ansicpg1361 a{\v\'8e}b\par}".into(),
                Some("ab\n"),
                vec![hidden(Body, 1)],
            ),
            (
                "a hidden space in a guessed code page",
                r"{\rtf1\ansicpg1361 a{\v\'a0}b\par}".into(),
                Some("ab\n"),
                vec![],
            ),
            (
                "hidden then shown text in a guessed code page",
                r"{\rtf1\ansicpg1361 {\v\'8e}\'8e\par}".into(),
                Some("\u{17d}\n"),
                vec![hidden(Body, 1), (EncodingAssumed, Body, 1)],
            ),
        ]);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of hidden ranges and their counts")]
    fn each_hidden_range_that_shows_something_counts_once() {
        use DiagnosticCode::RevisionOmitted;
        use Region::{Body, Note};
        check(vec![
            ("hidden whitespace", r"{\rtf1 a{\v \tab\~ }b\par}".into(), Some("ab\n"), vec![]),
            ("an empty hidden group", r"{\rtf1 a{\v}b\par}".into(), Some("ab\n"), vec![]),
            (
                "two ranges around shown text",
                r"{\rtf1 {\v a}b{\v c}\par}".into(),
                Some("b\n"),
                vec![hidden(Body, 2)],
            ),
            (
                "adjacent hidden groups",
                r"{\rtf1 {\v a}{\v b}c\par}".into(),
                Some("c\n"),
                vec![hidden(Body, 1)],
            ),
            (
                "text that cleans to nothing between hidden groups",
                r"{\rtf1 {\v a}\u173?{\v b}c\par}".into(),
                Some("c\n"),
                vec![hidden(Body, 1)],
            ),
            (
                "hidden whitespace inside a range",
                r"{\rtf1 {\v a}{\v  }{\v b}c\par}".into(),
                Some("c\n"),
                vec![hidden(Body, 1)],
            ),
            (
                "a range that starts with whitespace",
                r"{\rtf1 a{\v  b}\par}".into(),
                Some("a\n"),
                vec![hidden(Body, 1)],
            ),
            (
                "hidden ranges in two paragraphs",
                r"{\rtf1 {\v a}\par{\v b}\par c\par}".into(),
                Some("c\n"),
                vec![hidden(Body, 2)],
            ),
            (
                "a paragraph mark in a skipped destination inside a range",
                r"{\rtf1 {\v a}{\*\x \par}{\v b}c\par}".into(),
                Some("c\n"),
                vec![hidden(Body, 1)],
            ),
            (
                "hidden ranges in the body and a footnote",
                r"{\rtf1 {\v a}b{\footnote {\v c}d}\par}".into(),
                Some("b[^1]\n\n[^1]: d\n"),
                vec![hidden(Body, 1), hidden(Note, 1)],
            ),
            (
                "deleted hidden text",
                r"{\rtf1 a{\v\deleted b}c\par}".into(),
                Some("ac\n"),
                vec![(RevisionOmitted, Body, 1)],
            ),
            (
                "a deleted range inside a hidden one",
                r"{\rtf1 {\v a{\deleted b}c}d\par}".into(),
                Some("d\n"),
                vec![hidden(Body, 2), (RevisionOmitted, Body, 1)],
            ),
            (
                "a hidden range between deleted ones",
                r"{\rtf1 {\deleted a}{\v b}{\deleted c}d\par}".into(),
                Some("d\n"),
                vec![hidden(Body, 1), (RevisionOmitted, Body, 2)],
            ),
            (
                "hidden whitespace between deleted ranges",
                r"{\rtf1 {\deleted a}{\v  }{\deleted b}c\par}".into(),
                Some("c\n"),
                vec![(RevisionOmitted, Body, 2)],
            ),
        ]);
    }

    #[test]
    fn hidden_breaks_drop_and_every_paragraph_or_cell_mark_closes_a_range() {
        use DiagnosticCode::RevisionOmitted;
        use Region::{Body, Note};
        check(vec![
            ("a hidden mark then \\pard", r"{\rtf1 \v a\par\pard b\par}".into(), Some(""), vec![hidden(Body, 2)]),
            ("a hidden paragraph mark inside a range", r"{\rtf1 {\v a\par b}c\par}".into(), Some("c\n"), vec![hidden(Body, 2)]),
            ("a textless hidden paragraph mark", r"{\rtf1 a{\v\par}b\par}".into(), Some("ab\n"), vec![]),
            ("hidden line, page and column breaks", r"{\rtf1 a{\v\line\lbr1\page\column}b\par}".into(), Some("ab\n"), vec![]),
            ("a shown paragraph mark in a footnote", r"{\rtf1 a{\footnote {\v b}\par{\v c}d}\par}".into(), None, vec![hidden(Note, 2)]),
            ("a deleted paragraph mark inside a range", r"{\rtf1 {\deleted a\par b}c\par}".into(), Some("c\n"), vec![(RevisionOmitted, Body, 2)]),
            ("a deleted paragraph mark ending a range", r"{\rtf1 {\deleted a\par}c\par}".into(), Some("c\n"), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted hidden paragraph mark", r"{\rtf1 a{\v\deleted\par}b\par}".into(), Some("ab\n"), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted section break", r"{\rtf1 a{\deleted\sect}b\par}".into(), Some("a\n\nb\n"), vec![(RevisionOmitted, Body, 1)]),
            ("a cell mark", r"{\rtf1 \trowd\cellx1000\cellx2000\pard\intbl\v a\cell b\cell\row}".into(), Some(""), vec![hidden(Body, 2)]),
            (
                "a nested cell mark",
                r"{\rtf1 \trowd\cellx1000\pard\intbl\itap2\v a\nestcell b\nestcell{\*\nesttableprops\trowd\cellx500\cellx1000\nestrow}\pard\intbl\itap1\v0 c\cell\row}".into(),
                Some("|  |\n| --- |\n| c |\n"),
                vec![hidden(Body, 2)],
            ),
        ]);
    }

    #[test]
    fn hidden_destinations_and_field_results_drop_and_instructions_stay_read() {
        use DiagnosticCode::{FormControlUnread, InvalidEncoding, RevisionOmitted};
        use Region::{Body, Note};
        check(vec![
            ("\\v inside an unknown destination", r"{\rtf1 a{\*\x \v t}b\par}".into(), Some("ab\n"), vec![]),
            ("\\v inside the document information", r"{\rtf1 {\info{\title \v t}}a\par}".into(), Some("a\n"), vec![]),
            ("\\v inside a list label", r"{\rtf1 \pard{\listtext \v 1.\tab}a\par}".into(), Some("- 1\\. a\n"), vec![]),
            ("a hidden comment", r"{\rtf1 a{\v{\*\atnid x}{\*\annotation c}}\par}".into(), Some("a\n"), vec![]),
            ("a hidden page number", r"{\rtf1 a{\v\chpgn}\par}".into(), Some("a\n"), vec![]),
            ("a hidden object", r"{\rtf1 a{\v{\object{\*\objdata 01}{\result x}}}\par}".into(), Some("a\n"), vec![hidden(Body, 1)]),
            ("a hidden field", r#"{\rtf1 a{\v{\field{\*\fldinst HYPERLINK "http://x/"}{\fldrslt b}}}\par}"#.into(), Some("a\n"), vec![hidden(Body, 1)]),
            ("a hidden link result", r#"{\rtf1 a{\field{\*\fldinst HYPERLINK "http://x/"}{\fldrslt {\v b}}}c\par}"#.into(), Some("ac\n"), vec![hidden(Body, 1)]),
            ("\\v toggled inside a link result", r#"{\rtf1 {\field{\*\fldinst HYPERLINK "http://x/"}{\fldrslt a\v b\v0 c}}\par}"#.into(), Some("[ac](http://x/)\n"), vec![hidden(Body, 1)]),
            ("a hidden link result with a damaged target", r#"{\rtf1\ansi\ansicpg932 {\field{\*\fldinst HYPERLINK "http://x/\'82"}{\fldrslt {\v link}}}\par}"#.into(), Some(""), vec![hidden(Body, 1)]),
            ("a hidden instruction", r#"{\rtf1 {\field{\*\fldinst {\v HYPERLINK "http://x/"}}{\fldrslt link}}\par}"#.into(), Some("[link](http://x/)\n"), vec![]),
            ("a damaged hidden link target", r#"{\rtf1\ansi\ansicpg932 {\field{\*\fldinst HYPERLINK "http://x/{\v \'82}"}{\fldrslt link}}\par}"#.into(), None, vec![(InvalidEncoding, Body, 1)]),
            ("deleted text in a hidden instruction", r#"{\rtf1 {\field{\*\fldinst {\v\deleted x}HYPERLINK "http://x/"}{\fldrslt link}}\par}"#.into(), Some("[link](http://x/)\n"), vec![(RevisionOmitted, Body, 1)]),
            ("a hidden check box", r"{\rtf1 a{\v{\field{\*\fldinst FORMCHECKBOX}{\fldrslt }}}\par}".into(), Some("a\n"), vec![hidden(Body, 1)]),
            ("a hidden check box inside a range", r"{\rtf1 {\v a{\field{\*\fldinst FORMCHECKBOX}{\fldrslt }}}b\par}".into(), Some("b\n"), vec![hidden(Body, 1)]),
            ("a check box with a hidden instruction", r"{\rtf1 {\field{\*\fldinst {\v FORMCHECKBOX}}{\fldrslt }}\par}".into(), None, vec![(FormControlUnread, Body, 1)]),
            ("a range across a hidden field", r#"{\rtf1 {\v a{\field{\*\fldinst HYPERLINK "http://x/"}{\fldrslt b}}c}d\par}"#.into(), Some("d\n"), vec![hidden(Body, 1)]),
            ("a range split by a shown instruction", r#"{\rtf1 {\v a{\field{\*\fldinst {\v0 HYPERLINK "http://x/"}}{\fldrslt b}}}c\par}"#.into(), Some("c\n"), vec![hidden(Body, 2)]),
            ("a range across a field end", r#"{\rtf1 {\field{\*\fldinst HYPERLINK "http://x/"}{\fldrslt a{\v b}}}{\v c}d\par}"#.into(), Some("[a](http://x/)d\n"), vec![hidden(Body, 1)]),
            ("a range in a link in a field result", r#"{\rtf1 {\field{\*\fldinst REF x}{\fldrslt {\field{\*\fldinst HYPERLINK "http://x/"}{\fldrslt a{\v b}}}}}\par}"#.into(), Some("[a](http://x/)\n"), vec![hidden(Body, 1)]),
            ("a range in a link in two field results", r#"{\rtf1 {\field{\*\fldinst REF x}{\fldrslt {\field{\*\fldinst REF y}{\fldrslt {\field{\*\fldinst HYPERLINK "http://x/"}{\fldrslt a{\v b}}}}}}}\par}"#.into(), Some("[a](http://x/)\n"), vec![hidden(Body, 1)]),
            ("a range in a link in a field result in a footnote", r#"{\rtf1 a{\footnote {\field{\*\fldinst REF x}{\fldrslt {\field{\*\fldinst HYPERLINK "http://x/"}{\fldrslt b{\v c}}}}}}\par}"#.into(), Some("a[^1]\n\n[^1]: [b](http://x/)\n"), vec![hidden(Note, 1)]),
        ]);
    }

    #[test]
    fn hidden_pictures_notes_and_equations_drop_with_nothing_inside_reported() {
        use DiagnosticCode::{HeaderOmitted, RevisionOmitted};
        use Region::{Body, Header, Note};
        let in_hidden_note = |inner: &str| {
            format!(r"{{\rtf1\ansi\ansicpg932 a{{\v{{\footnote \plain {inner}}}}}\par}}")
        };
        check(vec![
            ("a hidden range in a footnote", r"{\rtf1 a{\footnote b{\v c}}\par}".into(), Some("a[^1]\n\n[^1]: b\n"), vec![hidden(Note, 1)]),
            ("a hidden footnote", r"{\rtf1 a{\v{\footnote b}}c\par}".into(), Some("ac\n"), vec![hidden(Body, 1)]),
            ("a hidden footnote reference inside a range", r"{\rtf1 {\v a{\footnote b}c}d\par}".into(), Some("d\n"), vec![hidden(Body, 1)]),
            ("a hidden footnote reference after whitespace", r"{\rtf1 a{\v  {\footnote b}}\par}".into(), Some("a\n"), vec![hidden(Body, 1)]),
            ("a footnote shown inside a hidden one", r"{\rtf1 a{\v{\footnote \v0\plain b{\footnote c}}}\par}".into(), Some("a\n"), vec![hidden(Body, 1)]),
            ("a lone lead byte in a hidden footnote", in_hidden_note(r"\'82"), Some("a\n"), vec![hidden(Body, 1)]),
            ("a lone surrogate in a hidden footnote", in_hidden_note(r"\u56320?"), Some("a\n"), vec![hidden(Body, 1)]),
            ("a page number in a hidden footnote", in_hidden_note(r"\chpgn"), Some("a\n"), vec![hidden(Body, 1)]),
            ("an unread picture in a hidden footnote", in_hidden_note(r"{\pict\macpict 01}"), Some("a\n"), vec![hidden(Body, 1)]),
            ("an object in a hidden footnote", in_hidden_note(r"{\object{\*\objdata 01}}"), Some("a\n"), vec![hidden(Body, 1)]),
            ("a check box in a hidden footnote", in_hidden_note(r"{\field{\*\fldinst FORMCHECKBOX}{\fldrslt }}"), Some("a\n"), vec![hidden(Body, 1)]),
            ("a comment in a hidden footnote", in_hidden_note(r"{\*\annotation c}"), Some("a\n"), vec![hidden(Body, 1)]),
            ("deleted text in a hidden footnote", in_hidden_note(r"{\deleted d}"), Some("a\n"), vec![hidden(Body, 1)]),
            ("a deleted hidden footnote", r"{\rtf1 a{\v\deleted{\footnote b}}\par}".into(), Some("a\n"), vec![(RevisionOmitted, Note, 1)]),
            ("a hidden picture", r"{\rtf1 a{\v{\pict\pngblip 89504e47}}\par}".into(), Some("a\n"), vec![hidden(Body, 1)]),
            ("a hidden unread picture", r"{\rtf1 a{\v{\pict\macpict 01}}\par}".into(), Some("a\n"), vec![hidden(Body, 1)]),
            ("a hidden picture inside a range", r"{\rtf1 {\v a{\pict\pngblip 89504e47}}b\par}".into(), Some("b\n"), vec![hidden(Body, 1)]),
            ("a hidden shape picture", r"{\rtf1 a{\v{\*\shppict{\pict\pngblip 89504e47}}{\nonshppict{\pict\wmetafile8 01}}}\par}".into(), Some("a\n"), vec![hidden(Body, 1)]),
            ("a hidden picture between deleted ranges", r"{\rtf1 {\deleted a}{\v{\pict\pngblip 89504e47}}{\deleted b}c\par}".into(), Some("c\n"), vec![hidden(Body, 1), (RevisionOmitted, Body, 2)]),
            ("a hidden picture in a header", r"{\rtf1 {\header {\v{\pict\pngblip 89504e47}}}a\par}".into(), Some("a\n"), vec![(HeaderOmitted, Header, 1)]),
            ("a hidden equation", r"{\rtf1 a{\v{\mmath{\*\moMath{\mr x}}}}b\par}".into(), Some("ab\n"), vec![hidden(Body, 1)]),
            ("an empty hidden equation", r"{\rtf1 a{\v{\mmath}}b\par}".into(), Some("ab\n"), vec![]),
            ("a hidden equation of whitespace", r"{\rtf1 a{\v{\mmath  }}b\par}".into(), Some("ab\n"), vec![]),
            ("a hidden math run", r"{\rtf1 {\mmath{\*\moMath{\mr {\v x}y}}}\par}".into(), Some("$y$\n"), vec![hidden(Body, 1)]),
            ("a hidden damaged math run", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\mr {\v \'82}x}}}\par}".into(), Some("$x$\n"), vec![hidden(Body, 1)]),
            ("a hidden lone surrogate in math", r"{\rtf1 {\mmath{\*\moMath{\mr {\v \u56320?}x}}}\par}".into(), Some("$x$\n"), vec![]),
            ("hidden whitespace in math", r"{\rtf1 {\mmath{\*\moMath{\mr {\v  }x}}}\par}".into(), Some("$x$\n"), vec![]),
            ("a hidden math structure", r"{\rtf1 {\mmath{\*\moMath{\v{\mf{\mnum{\mr b}}{\mden{\mr c}}}}{\mr x}}}\par}".into(), Some("$x$\n"), vec![hidden(Body, 1)]),
        ]);
    }

    #[test]
    fn hidden_pictures_and_footnotes_are_not_read() {
        let doc =
            parse(&ParseCtx::default(), br"{\rtf1 a{\v{\pict\pngblip 89504e47}}\par}").unwrap();
        assert_eq!(doc.assets, []);
        let doc =
            parse(&ParseCtx::default(), br"{\rtf1 a{\v{\footnote b}}{\footnote c}\par}").unwrap();
        let ids: Vec<_> = doc.notes.iter().map(|note| note.id.as_str()).collect();
        assert_eq!(ids, ["rtf0"]);
    }

    #[test]
    fn a_hidden_lone_lead_byte_converts_under_on_loss_fail() {
        let src = br"{\rtf1\ansi\ansicpg932 a{\v \'82}b\par}";
        let converter = crate::Converter::new(crate::Config::default()).unwrap();
        let converted = converter.convert(crate::Input::new(src, crate::Format::Rtf)).unwrap();
        assert_eq!(converted.markdown(), "ab\n");
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of dropped paragraph marks by container")]
    fn a_dropped_paragraph_mark_joins_the_next_paragraph_of_its_container() {
        let heading = r"{\stylesheet{\s1\outlinelevel0 H;}}";
        let table = "|  |\n| --- |\n| b |\n";
        let nested = r"{\*\nesttableprops\trowd\cellx500\nestrow}";
        let cases: Vec<(String, String)> = vec![
            (format!(r"{{\rtf1{heading}\pard one{{\v\par}}\pard\s1 two\par}}"), "# onetwo\n".into()),
            (format!(r"{{\rtf1{heading}\pard\s1 one{{\v\par}}\pard two\par}}"), "onetwo\n".into()),
            (r"{\rtf1 \pard one{\v\par}\pard\ls5 two\par}".into(), "- onetwo\n".into()),
            (r"{\rtf1 \pard\ls5 one{\v\par}\pard two\par}".into(), "onetwo\n".into()),
            (r"{\rtf1 a{\v\par}}".into(), "a\n".into()),
            (r"{\rtf1 a{\footnote b{\v\par}c}\par}".into(), "a[^1]\n\n[^1]: bc\n".into()),
            (r"{\rtf1 a{\v\par}\sect b\par}".into(), "a\n\nb\n".into()),
            (r"{\rtf1 a{\v\par}\line b\par}".into(), "a\\\nb\n".into()),
            (r"{\rtf1 a{\v\par}\page b\par}".into(), "a\n\nb\n".into()),
            (r"{\rtf1 a{\v\par}\column b\par}".into(), "a\n\nb\n".into()),
            (r"{\rtf1 a{\v\par}{\footnote b\page c}d\par}".into(), "a[^1]d\n\n[^1]: b\\\n    c\n".into()),
            (r"{\rtf1 a{\v\sect}b\par}".into(), "a\n\nb\n".into()),
            (r"{\rtf1 a{\v\par}{\deleted\sect}b\par}".into(), "a\n\nb\n".into()),
            (r"{\rtf1 a{\v\par}\trowd\cellx1000\pard\intbl b\cell\row}".into(), format!("a\n\n{table}")),
            (r"{\rtf1 a{\deleted\par}\trowd\cellx1000\pard\intbl b\cell\row}".into(), format!("a\n\n{table}")),
            (r"{\rtf1 a{\v\par}\pard\intbl b\cell\row}".into(), format!("a\n\n{table}")),
            (r"{\rtf1 a{\v\par}\trowd\cellx1000 b\cell\row}".into(), format!("a\n\n{table}")),
            (r"{\rtf1 a{\v\par}\cell\row}".into(), "a\n".into()),
            (r"{\rtf1 \trowd\cellx1000\pard\intbl a{\v\par}\pard\intbl b\cell\row}".into(), "|  |\n| --- |\n| ab |\n".into()),
            (r"{\rtf1 \trowd\cellx1000\pard\intbl\itap0 a{\v\par}\pard\intbl b\cell\row}".into(), "|  |\n| --- |\n| ab |\n".into()),
            (
                format!(r"{{\rtf1 \trowd\cellx1000\pard\intbl a{{\v\par}}\nestcell{nested}\pard\intbl b\cell\row}}"),
                "|  |\n| --- |\n| a<br>b |\n".into(),
            ),
            (format!(r"{{\rtf1{heading}\pard\s1 one{{\v\par}}\pard\intbl b\cell\row}}"), format!("# one\n\n{table}")),
            (r"{\rtf1 a{\v\par}{\footnote \pard\intbl b}c\par}".into(), "a[^1]c\n\n[^1]: b\n".into()),
            (r"{\rtf1 a{\v\par}{\*\x \intbl}c\par}".into(), "ac\n".into()),
            (
                format!(r"{{\rtf1 \trowd\cellx1000\pard\intbl x{{\v\par}}\pard\intbl\itap2 y\nestcell{nested}\cell\row}}"),
                "|  |\n| --- |\n| x<br>y |\n".into(),
            ),
            (
                format!(r"{{\rtf1 \trowd\cellx1000\pard\intbl x{{\v\par}}\itap2 y\nestcell{nested}\cell\row}}"),
                "|  |\n| --- |\n| x<br>y |\n".into(),
            ),
            (
                format!(r"{{\rtf1 \trowd\cellx1000\pard\intbl\itap2 x{{\v\par}}\pard\intbl\itap3 y\nestcell{{\*\nesttableprops\trowd\cellx250\nestrow}}\pard\intbl\itap2 z\nestcell{nested}\pard\intbl\itap1 w\cell\row}}"),
                "|  |\n| --- |\n| w<br>x<br>z<br>y |\n".into(),
            ),
            (
                r"{\rtf1 \trowd\cellx1000\pard\intbl x\cell\row\pard a{\v\par}\trowd\cellx1000\pard\intbl b\cell\row}".into(),
                format!("|  |\n| --- |\n| x |\n\na\n\n{table}"),
            ),
            (
                r"{\rtf1 \trowd\cellx1000\pard\intbl x\cell\row\pard{\deleted a\par}\trowd\cellx1000\pard\intbl b\cell\row}".into(),
                "| x |\n| --- |\n| b |\n".into(),
            ),
            (
                r"{\rtf1 \trowd\cellx1000\pard\intbl x{\v\par}\cell\row\pard a\trowd\cellx1000\pard\intbl b\cell\row}".into(),
                "| x |\n| --- |\n| ab |\n".into(),
            ),
            (
                r"{\rtf1 a{\footnote b{\v\par}c}\trowd\cellx1000\pard\intbl d\cell\row}".into(),
                "|  |\n| --- |\n| a[^1]d |\n\n[^1]: bc\n".into(),
            ),
            (
                r"{\rtf1 a{\v\par}{\footnote b{\v\par}c}\trowd\cellx1000\pard\intbl d\cell\row}".into(),
                "a[^1]\n\n|  |\n| --- |\n| d |\n\n[^1]: bc\n".into(),
            ),
            (
                r"{\rtf1 a{\v\par}b\par x\trowd\cellx1000\pard\intbl y\cell\row}".into(),
                "ab\n\n|  |\n| --- |\n| xy |\n".into(),
            ),
            (
                r"{\rtf1 a{\v\par}b\trowd\cellx1000\pard\intbl c\cell\row}".into(),
                "ab\n\n|  |\n| --- |\n| c |\n".into(),
            ),
            (
                r"{\rtf1 a{\deleted\par}b\trowd\cellx1000\pard\intbl c\cell\row}".into(),
                "ab\n\n|  |\n| --- |\n| c |\n".into(),
            ),
            (
                r"{\rtf1 \trowd\cellx1000\pard\intbl x{\deleted\par}\cell\row\pard a\trowd\cellx1000\pard\intbl b\cell\row}".into(),
                "| x |\n| --- |\n| ab |\n".into(),
            ),
            (
                r"{\rtf1 \trowd\cellx1000\pard\intbl x{\v\par}\cell\row\trowd\cellx1000\pard\intbl y\cell\row}".into(),
                "| x |\n| --- |\n| y |\n".into(),
            ),
            (
                r"{\rtf1 \trowd\cellx1000\cellx2000\pard\intbl a{\v\par}b\cell c\cell\row}".into(),
                "|  |  |\n| --- | --- |\n| ab | c |\n".into(),
            ),
            (
                r"{\rtf1 \trowd\cellx1000\cellx2000\pard\intbl a{\v\cell}b\cell\row}".into(),
                "|  |  |\n| --- | --- |\n| a | b |\n".into(),
            ),
            (
                r"{\rtf1 \trowd\cellx1000\cellx2000\pard\intbl a{\deleted\cell}b\cell\row}".into(),
                "|  |  |\n| --- | --- |\n| a | b |\n".into(),
            ),
            (
                format!(r"{{\rtf1 \trowd\cellx1000\pard\intbl\itap2 a{{\v\nestcell}}{nested}\pard\intbl\itap1 b\cell\row}}"),
                "|  |\n| --- |\n| b<br>a |\n".into(),
            ),
            (
                r"{\rtf1 \trowd\cellx1000\pard\intbl a\cell{\v\row}\trowd\cellx1000\pard\intbl b\cell\row}".into(),
                "| a |\n| --- |\n| b |\n".into(),
            ),
        ];
        let mut failed = Vec::new();
        for (src, expected) in cases {
            let got = markdown(&src);
            if got != expected {
                failed.push(format!("{src}\n  got {got:?}\n  want {expected:?}"));
            }
        }
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }

    #[test]
    fn an_unreadable_or_empty_picture_keeps_its_place() {
        for picture in [r"{\pict\wbitmap 01}", r"{\pict}", r"{\pict\pngblip}"] {
            let source = format!(r"{{\rtf1 before{picture}after}}");
            assert_eq!(markdown(&source), "before\\[Image\\]after\n");
            assert_eq!(reported(&source), [(DiagnosticCode::ImageUnavailable, Region::Body, 1)]);
            for hidden in [r"\v", r"\deleted"] {
                let source = format!(r"{{\rtf1 before{{{hidden}{picture}}}after}}");
                assert_eq!(markdown(&source), "beforeafter\n");
                let ctx = ParseCtx::default();
                parse(&ctx, source.as_bytes()).unwrap();
                assert!(ctx.diagnostics().iter().all(|d| matches!(
                    d.code,
                    DiagnosticCode::HiddenOmitted | DiagnosticCode::RevisionOmitted
                )));
            }
        }
    }
}
