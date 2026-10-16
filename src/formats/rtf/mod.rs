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
use crate::model::{Block, Document, Inline, LinkTarget, Note, NoteKind, Style, inlines_are_empty};
use crate::package::xml::{Element, Node, ns};
use crate::shared::blockstyle::{BlockStyle, StyledRun};
use crate::shared::delta::rebase_emphasis;
use crate::shared::fields::{field_result, hyperlink_target_spans};
use crate::shared::list::{ListEntry, ListKey, MarkerKind, flush_list};
use crate::shared::math::{DROPPED, GUESSED, math_lines, omath_para_to_tex};
use crate::shared::text::clean_text;
use lexer::{Lexer, Token};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use table::TableState;
use tables::{Codepage, LIST_LEVELS, Prelude, TableScan, TextLoss, codepage_encoding};

pub fn parse(parse_ctx: &ParseCtx, bytes: &[u8]) -> Result<Document, ConvertError> {
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
    capture: Capture,
    note: Option<NoteKind>,
}

impl Default for CharState {
    fn default() -> Self {
        CharState {
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
            capture: Capture::None,
            note: None,
        }
    }
}

struct FieldFrame {
    depth: usize,
    instr: String,
    /// What each damaged character of the instruction lost.
    losses: Vec<InstrLoss>,
    start: usize,
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

/// Per-instance numbering counters with restart-on-shallower semantics.
#[derive(Default)]
struct Counters {
    state: HashMap<i32, ([u64; LIST_LEVELS], [bool; LIST_LEVELS])>,
}

impl Counters {
    fn next(&mut self, ls: i32, level: usize, start: u64) -> u64 {
        let level = level.min(LIST_LEVELS - 1);
        let (values, started) = self.state.entry(ls).or_default();
        let value = if started[level] { values[level].saturating_add(1) } else { start };
        values[level] = value;
        started[level] = true;
        for s in started.iter_mut().skip(level + 1) {
            *s = false;
        }
        value
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
        let level = level.min(LIST_LEVELS - 1);
        let def = &levels[level];
        let value = self.next(ls, level, def.start);
        let (values, started) = self.state.entry(ls).or_default();
        let label = def.marker.and_then(|marker| {
            crate::shared::numbering::composite_label(
                &def.pattern,
                marker,
                value,
                |l| levels[l.min(LIST_LEVELS - 1)].marker.unwrap_or(MarkerKind::Decimal),
                |l| {
                    let l = l.min(LIST_LEVELS - 1);
                    if started[l] { values[l] } else { levels[l].start }
                },
            )
        });
        (value, label, def.loss)
    }

    /// Pin a counter to a number taken from the source (legacy `\pn` labels).
    fn seed(&mut self, ls: i32, level: usize, value: u64) {
        let level = level.min(LIST_LEVELS - 1);
        let (values, started) = self.state.entry(ls).or_default();
        values[level] = value;
        started[level] = true;
        for s in started.iter_mut().skip(level + 1) {
            *s = false;
        }
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
        TextDecoder { default_encoding, pending: Vec::new(), skip: 0, surrogate: None }
    }

    /// Buffer one text byte, honoring an active `\uN` fallback skip, and
    /// whether it was buffered.
    fn byte(&mut self, b: u8) -> bool {
        if self.skip > 0 {
            self.skip -= 1;
            false
        } else {
            self.pending.push(b);
            true
        }
    }

    /// Discard a held high surrogate that something other than its pair
    /// followed, and whether one was held.
    fn interrupt(&mut self) -> bool {
        self.surrogate.take().is_some()
    }

    /// True when an active fallback skip consumed the character.
    fn skip_char(&mut self) -> bool {
        if self.skip > 0 {
            self.skip -= 1;
            true
        } else {
            false
        }
    }

    /// Decode and take the buffered bytes with the current font's encoding,
    /// with what the text lost.
    fn take_pending(&mut self, encoding: Option<Codepage>) -> Option<(String, TextLoss)> {
        if self.pending.is_empty() {
            return None;
        }
        let bytes = std::mem::take(&mut self.pending);
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
        let code = if n < 0 { (n + 65536) as u32 } else { n as u32 };
        let out = match (self.surrogate.take(), code as u16) {
            // High surrogate: hold for its pair.
            (None, unit @ 0xD800..=0xDBFF) => {
                self.surrogate = Some(unit);
                (None, false)
            }
            (Some(high), low @ 0xDC00..=0xDFFF) => {
                let combined = 0x10000 + ((high as u32 - 0xD800) << 10) + (low as u32 - 0xDC00);
                (char::from_u32(combined), false)
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
    binary: Option<Vec<u8>>,
    /// (media type, extension) from `\pngblip`/`\jpegblip`/`\emfblip`/
    /// `\wmetafile`; `None` = unsupported format.
    format: Option<(&'static str, &'static str)>,
}

impl PictState {
    /// The decoded payload bytes.
    fn payload(self) -> Vec<u8> {
        if let Some(binary) = self.binary {
            return binary;
        }
        let mut out = Vec::with_capacity(self.hex.len() / 2);
        let mut high: Option<u8> = None;
        for &b in &self.hex {
            let Some(digit) = (b as char).to_digit(16) else {
                continue;
            };
            match high.take() {
                Some(h) => out.push((h << 4) | digit as u8),
                None => high = Some(digit as u8),
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
        MathState { depth, open: vec![(depth, math_elem("zone"))] }
    }

    fn open_group(&mut self, depth: usize) {
        self.open.push((depth, math_elem("")));
    }

    /// Attach the groups opened deeper than `depth` to their parents. A
    /// group that named no element is transparent.
    fn close_groups(&mut self, depth: usize) {
        while self.open.len() > 1 && self.open.last().is_some_and(|(d, _)| *d > depth) {
            let (_, elem) = self.open.pop().unwrap();
            let (_, parent) = self.open.last_mut().unwrap();
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
        ns: Some(Rc::from(ns::M)),
        local: local.to_string(),
        attrs: Vec::new(),
        children: Vec::new(),
    }
}

/// Every OMML element name, as its rtf control word without the `m` prefix.
const OMML_NAMES: &[&str] = &[
    "acc",
    "accPr",
    "aln",
    "alnScr",
    "argPr",
    "argSz",
    "bar",
    "barPr",
    "baseJc",
    "begChr",
    "borderBox",
    "borderBoxPr",
    "box",
    "boxPr",
    "brk",
    "brkBin",
    "brkBinSub",
    "cGp",
    "cGpRule",
    "chr",
    "count",
    "cSp",
    "ctrlPr",
    "d",
    "defJc",
    "deg",
    "degHide",
    "den",
    "diff",
    "dispDef",
    "dPr",
    "e",
    "endChr",
    "eqArr",
    "eqArrPr",
    "f",
    "fName",
    "fPr",
    "func",
    "funcPr",
    "groupChr",
    "groupChrPr",
    "grow",
    "hideBot",
    "hideLeft",
    "hideRight",
    "hideTop",
    "interSp",
    "intLim",
    "intraSp",
    "jc",
    "lim",
    "limLoc",
    "limLow",
    "limLowPr",
    "limUpp",
    "limUppPr",
    "lit",
    "lMargin",
    "m",
    "mathFont",
    "mathPr",
    "maxDist",
    "mc",
    "mcJc",
    "mcPr",
    "mcs",
    "mPr",
    "mr",
    "nary",
    "naryLim",
    "naryPr",
    "noBreak",
    "nor",
    "num",
    "objDist",
    "oMath",
    "oMathPara",
    "oMathParaPr",
    "opEmu",
    "phant",
    "phantPr",
    "plcHide",
    "pos",
    "postSp",
    "preSp",
    "r",
    "rad",
    "radPr",
    "rMargin",
    "rPr",
    "rSp",
    "rSpRule",
    "scr",
    "sepChr",
    "show",
    "shp",
    "smallFrac",
    "sPre",
    "sPrePr",
    "sSub",
    "sSubPr",
    "sSubSup",
    "sSubSupPr",
    "sSup",
    "sSupPr",
    "strikeBLTR",
    "strikeH",
    "strikeTLBR",
    "strikeV",
    "sty",
    "sub",
    "subHide",
    "sup",
    "supHide",
    "t",
    "transp",
    "type",
    "vertJc",
    "wrapIndent",
    "wrapRight",
    "zeroAsc",
    "zeroDesc",
    "zeroWid",
];

/// Field, note, bookmark, and list-label destination state: frames open at a
/// group depth and close when the group stack unwinds past it.
#[derive(Default)]
struct Destinations {
    fields: Vec<FieldFrame>,
    note_frames: Vec<NoteFrame>,
    notes: Vec<Note>,
    bookmark: String,
    /// Captured `\listtext` label for the current paragraph.
    listtext: Option<String>,
    /// What the captured label lost in decoding.
    listtext_loss: TextLoss,
    /// The `\pict` destination currently accumulating, if any.
    pict: Option<PictState>,
    /// The math zone currently accumulating, if any.
    math: Option<MathState>,
    /// The current paragraph holds a finished math paragraph.
    math_display: bool,
}

impl Destinations {
    /// Close note frames opened deeper than `depth`, replacing their content
    /// with a reference to the collected note.
    fn close_notes(&mut self, depth: usize, inlines: &mut Vec<Inline>) {
        while self.note_frames.last().is_some_and(|f| f.depth > depth) {
            let frame = self.note_frames.pop().unwrap();
            let start = frame.start.min(inlines.len());
            let content: Vec<Inline> = inlines.drain(start..).collect();
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
        if !still_capturing && !self.bookmark.is_empty() {
            let name = std::mem::take(&mut self.bookmark);
            let name = name.trim().to_string();
            if !name.is_empty() {
                inlines.push(Inline::Anchor(name));
            }
        }
    }
}

/// Destinations whose content is excluded: headers/footers by fixed policy,
/// metadata and binary destinations because they carry no document text.
const SUPPRESSED_DESTINATIONS: &[&str] = &[
    "fonttbl",
    "colortbl",
    "stylesheet",
    "info",
    "object",
    "header",
    "footer",
    "headerl",
    "headerr",
    "headerf",
    "footerl",
    "footerr",
    "footerf",
    "ftnsep",
    "ftnsepc",
    "aftnsep",
    "aftnsepc",
    "xmlnstbl",
    "themedata",
    "colorschememapping",
    "datastore",
    "latentstyles",
    "listtable",
    "listoverridetable",
    "rsidtbl",
    "generator",
    "filetbl",
    "revtbl",
    "datafield",
    "formfield",
    "bkmkend",
    "annotation",
    "atnid",
    "atnauthor",
    "template",
    "defchp",
    "defpap",
    "panose",
    "falt",
    "objdata",
    "blipuid",
    "nonshppict",
    "wgrffmtfilter",
    "pgdsctbl",
    "docvar",
    "sp",
    "sn",
    "sv",
    "shpinst",
    "background",
    "userprops",
    "operator",
    "author",
    "title",
    "subject",
    "keywords",
    "doccomm",
    "creatim",
    "revtim",
    "printim",
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
    /// The list levels (`\ls`, level) whose damaged label text has been
    /// reported.
    labels_reported: HashSet<(i32, usize)>,
    /// The region of the deleted range the last text dropped or kept was
    /// in, so a deleted range is reported once in its region.
    deleted_range: Option<Region>,

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
        Parser {
            parse_ctx,
            bytes,
            lexer: Lexer::new(parse_ctx, bytes),
            stack: Vec::new(),
            state: CharState::default(),
            prelude: Prelude::default(),
            tables: TableScan::default(),
            decoder: TextDecoder::new(Codepage {
                encoding: encoding_rs::WINDOWS_1252,
                assumed: false,
            }),
            codepage_declared: false,
            assumed_reported: false,
            labels_reported: HashSet::new(),
            deleted_range: None,
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
                Token::Close => {
                    self.flush_pending();
                    match self.stack.pop() {
                        Some(prev) => self.state = prev,
                        None => self.parse_ctx.emit(DiagnosticCode::UnmatchedClose, Region::Body),
                    }
                    let depth = self.stack.len();
                    self.close_fields(depth);
                    self.dest.close_notes(depth, &mut self.inlines);
                    self.dest
                        .close_bookmark(self.state.capture == Capture::Bookmark, &mut self.inlines);
                    // The pict destination finishes when the stack unwinds
                    // past its own group (inner property groups close first).
                    if self.dest.pict.as_ref().is_some_and(|p| self.stack.len() < p.depth) {
                        self.finish_pict()?;
                    }
                    if let Some(math) = &mut self.dest.math {
                        if self.stack.len() < math.depth {
                            self.finish_math();
                        } else {
                            math.close_groups(self.stack.len());
                        }
                    }
                }
                Token::Word { name, param } => self.control_word(name, param)?,
                Token::Symbol(b) => self.control_symbol(b),
                Token::Hex(b) | Token::Byte(b) => {
                    if self.state.capture == Capture::Pict {
                        if let Some(pict) = &mut self.dest.pict
                            && self.stack.len() == pict.depth
                        {
                            pict.hex.push(b);
                        }
                    } else if self.accepts_text() && self.decoder.byte(b) {
                        self.settle_surrogate();
                    }
                }
                Token::Bin(payload) => {
                    if self.state.capture == Capture::Pict
                        && let Some(pict) = &mut self.dest.pict
                        && self.stack.len() == pict.depth
                    {
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

    fn accepts_text(&self) -> bool {
        !self.state.omit && (self.state.capture != Capture::None || !self.state.suppress)
    }

    /// Whether body text in the current state reaches the output.
    fn renders(&self) -> bool {
        self.state.capture == Capture::None && !self.state.suppress && !self.state.deleted
    }

    fn region(&self) -> Region {
        if self.state.note.is_some() { Region::Note } else { Region::Body }
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
            | Capture::Math => {}
        }
    }

    /// Whether the current capture takes deleted text, which accepted
    /// revisions drop from a field instruction and an equation.
    fn captures_deleted(&self) -> bool {
        self.state.deleted && matches!(self.state.capture, Capture::FieldInstr | Capture::Math)
    }

    /// Account for a character dropped because it did not decode.
    fn drop_char(&mut self) {
        if self.captures_deleted() {
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
        let region = self.region();
        if self.deleted_range != Some(region) {
            self.deleted_range = Some(region);
            self.parse_ctx.emit(DiagnosticCode::RevisionOmitted, region);
        }
    }

    /// Leave out a structure that opens in deleted text, reporting its
    /// range, and whether it did.
    fn omit_deleted(&mut self) -> bool {
        if !self.state.deleted {
            return false;
        }
        if !self.state.suppress {
            self.drop_deleted();
        }
        self.state.suppress = true;
        self.state.omit = true;
        true
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
        if self.stack.last().is_some_and(|holder| !holder.suppress) {
            self.parse_ctx.emit(code, region);
        }
        code != DiagnosticCode::EmbeddedObject
    }

    /// Close field frames opened deeper than `depth`, folding their results
    /// back into the inline stream.
    fn close_fields(&mut self, depth: usize) {
        while let Some(frame) = self.dest.fields.pop_if(|f| f.depth > depth) {
            let start = frame.start.min(self.inlines.len());
            let content: Vec<Inline> = self.inlines.drain(start..).collect();
            let region = self.region();
            if is_choice_form_field(&frame.instr) {
                self.parse_ctx.emit(DiagnosticCode::FormControlUnread, region);
            }
            if !inlines_are_empty(&content)
                && let Some((target, spans)) = hyperlink_target_spans(&frame.instr)
            {
                // An anchor shows only as a slug or not at all: a damaged
                // character it keeps never reaches the output.
                let anchor = matches!(target, LinkTarget::Anchor(_));
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
            b'~' => self.push_char('\u{a0}'),
            b'-' => {}
            b'_' => self.push_char('-'),
            b'*' => self.state.suppress = true,
            b'\\' | b'{' | b'}' => self.push_char(b as char),
            _ => {}
        }
    }

    /// Dispatch a control word to its subsystem handler; unhandled words
    /// that name a suppressed destination silence their group.
    fn control_word(&mut self, word: &str, param: Option<i32>) -> Result<(), ConvertError> {
        if self.codepage_control(word, param) {
            return Ok(());
        }
        if self.math_control(word, param) {
            return Ok(());
        }
        if self.text_control(word, param)? {
            return Ok(());
        }
        if self.table_control(word, param)? {
            return Ok(());
        }
        if self.list_control(word, param) {
            return Ok(());
        }
        if self.object_control(word)? {
            return Ok(());
        }
        if SUPPRESSED_DESTINATIONS.contains(&word) {
            self.flush_pending();
            if !(word == "object" && self.omit_deleted()) {
                self.state.omit |= self.report_destination(word);
            }
            self.state.suppress = true;
            self.state.capture = Capture::None;
        }
        Ok(())
    }

    /// The document's code page: its first `\ansicpg`, else windows-1252,
    /// a guess once `\mac`, `\pc` or `\pca` names another character set.
    fn codepage_control(&mut self, word: &str, param: Option<i32>) -> bool {
        let codepage = match (word, param) {
            ("ansicpg", Some(cp)) => match codepage_encoding(cp.max(0) as u32) {
                Some(encoding) => Codepage::declared(encoding),
                None => Codepage { encoding: encoding_rs::WINDOWS_1252, assumed: true },
            },
            ("mac" | "pc" | "pca", _) => {
                Codepage { assumed: true, ..self.decoder.default_encoding }
            }
            _ => return false,
        };
        if !self.codepage_declared {
            self.flush_pending();
            self.codepage_declared = word == "ansicpg";
            self.decoder.default_encoding = codepage;
        }
        true
    }

    /// Character- and paragraph-level controls: text encoding, styling,
    /// paragraph and line breaks, and special characters.
    fn text_control(&mut self, word: &str, param: Option<i32>) -> Result<bool, ConvertError> {
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
                        self.push_text(c.to_string(), TextLoss::default());
                    }
                }
            }
            "uc" => self.state.uc_skip = param.unwrap_or(1).max(0) as u32,
            "f" => {
                self.flush_pending();
                self.state.font = param;
            }
            "b" => self.set_style(|s| s.bold = on),
            "i" => self.set_style(|s| s.italic = on),
            "strike" | "striked" => self.set_style(|s| s.strike = on),
            "plain" => {
                self.flush_pending();
                self.state.style = Style::PLAIN;
                self.state.deleted = false;
            }
            "deleted" => {
                self.flush_pending();
                self.state.deleted = on;
            }
            "s" => {
                // Paragraph style: outline level for headings plus its
                // formatting delta as the new base.
                let def = param.and_then(|id| self.prelude.styles.get(&id).copied());
                if let Some(def) = def {
                    self.flush_pending();
                    self.state.outline = def.outline;
                    self.state.block = def.block;
                    self.state.style = def.delta.apply(self.state.style);
                    self.state.style_base = self.state.style;
                }
            }
            "par" | "sect" => {
                self.flush_pending();
                if self.state.deleted {
                    if !self.state.suppress {
                        self.drop_deleted();
                    }
                } else if self.state.note.is_some() {
                    self.inlines.push(Inline::LineBreak);
                } else if !self.state.suppress {
                    self.end_paragraph()?;
                }
            }
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
                    } else {
                        self.inlines.push(Inline::LineBreak);
                    }
                }
            }
            "tab" => self.push_char(' '),
            "emdash" => self.push_char('\u{2014}'),
            "endash" => self.push_char('\u{2013}'),
            "lquote" => self.push_char('\u{2018}'),
            "rquote" => self.push_char('\u{2019}'),
            "ldblquote" => self.push_char('\u{201c}'),
            "rdblquote" => self.push_char('\u{201d}'),
            "bullet" => self.push_char('\u{2022}'),
            "enspace" | "emspace" | "qmspace" => self.push_char(' '),
            "chpgn" | "chdate" | "chtime" | "chdpl" | "chdpa" => {
                if self.renders() {
                    self.parse_ctx.emit(DiagnosticCode::FieldValueUnread, self.region());
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Table controls, delegated to the per-depth [`TableState`].
    fn table_control(&mut self, word: &str, param: Option<i32>) -> Result<bool, ConvertError> {
        match word {
            "intbl" => self.state.in_table = true,
            "itap" => {
                self.state.itap = param.unwrap_or(1).clamp(0, 8) as usize;
                if self.state.itap > 1 {
                    self.state.in_table = true;
                }
            }
            "trowd" => {
                if self.table_active() {
                    let depth = self.state.itap.max(1);
                    self.table.begin_row(depth);
                }
            }
            "trhdr" => {
                if self.table_active() {
                    let depth = self.state.itap.max(1);
                    self.table.mark_header_row(depth);
                }
            }
            "clmgf" => self.pending_cell_prop(|p| p.merge_first = true),
            "clmrg" => self.pending_cell_prop(|p| p.merge_cont = true),
            "clvmgf" => self.pending_cell_prop(|p| p.vmerge_first = true),
            "clvmrg" => self.pending_cell_prop(|p| p.vmerge_cont = true),
            "cellx" => {
                if self.table_active() {
                    let depth = self.state.itap.max(1);
                    self.table.declare_cell(depth, param.unwrap_or(0) as i64);
                }
            }
            "cell" => {
                self.flush_pending();
                if self.table_active() {
                    self.end_cell(1)?;
                }
            }
            "nestcell" => {
                self.flush_pending();
                if self.table_active() {
                    self.end_cell(self.state.itap.max(2))?;
                }
            }
            "row" => {
                self.flush_pending();
                if self.table_active() {
                    self.end_row(1)?;
                }
            }
            "nestrow" => {
                self.flush_pending();
                if self.table_active() {
                    self.end_row(self.state.itap.max(2))?;
                }
            }
            // Nested row properties arrive in a `{\*\nesttableprops ...}`
            // destination; its \trowd/\cellx/\nestrow must still act.
            "nesttableprops" => self.state.suppress = self.state.omit,
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// List and outline controls.
    fn list_control(&mut self, word: &str, param: Option<i32>) -> bool {
        match word {
            "outlinelevel" => {
                if let Some(n) = param
                    && (0..9).contains(&n)
                {
                    self.state.outline = Some((n + 1) as u8);
                }
            }
            "ilvl" => self.state.ilvl = param.unwrap_or(0).clamp(0, 8) as usize,
            "ls" => self.state.ls = param,
            "listtext" | "pntext" => {
                self.flush_pending();
                self.state.capture = Capture::ListText;
                if self.dest.listtext.is_none() {
                    self.dest.listtext = Some(String::new());
                }
            }
            "pnlvlblt" => self.state.legacy_list = Some(MarkerKind::Bullet),
            "pnlvlbody" | "pndec" => self.state.legacy_list = Some(MarkerKind::Decimal),
            _ => return false,
        }
        true
    }

    /// Field, footnote, and bookmark controls.
    fn object_control(&mut self, word: &str) -> Result<bool, ConvertError> {
        match word {
            "field" => {
                self.flush_pending();
                self.omit_deleted();
                if !self.state.suppress {
                    let max = self.parse_ctx.limits().max_nesting_depth;
                    if self.dest.fields.len() as u64 >= max {
                        return Err(ConvertError::limit(Limit::NestingDepth, max));
                    }
                    self.dest.fields.push(FieldFrame {
                        depth: self.stack.len(),
                        instr: String::new(),
                        losses: Vec::new(),
                        start: self.inlines.len(),
                    });
                }
            }
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
                self.state.note = Some(NoteKind::Footnote);
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
            // `{\*\shppict {\pict ...}}` wraps the preferred picture; the
            // `\nonshppict` fallback duplicate stays suppressed.
            "shppict" => self.state.suppress = self.state.omit,
            // `\shpinst` itself is suppressed (shape properties), but its
            // `\shptxt` destination holds the shape's real text.
            "shptxt" => self.state.suppress = self.state.omit,
            // Likewise `\object` is suppressed (class names, `\objdata`
            // payload), but `\result` is the object's displayable rendering.
            "result" => self.state.suppress = self.state.omit,
            "pict" => {
                // A pict inside a suppressed destination (the nonshppict
                // fallback, excluded headers) is not extracted.
                self.flush_pending();
                self.omit_deleted();
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
        if display {
            self.dest.math_display = true;
        }
    }

    fn set_pict_format(&mut self, format: Option<(&'static str, &'static str)>) {
        if self.state.capture == Capture::Pict
            && let Some(pict) = &mut self.dest.pict
        {
            pict.format = format;
        }
    }

    /// Finalize a closed `\pict` destination: retain the payload as an
    /// asset and emit its inline reference. A picture in an unsupported
    /// format is `image_unavailable`.
    fn finish_pict(&mut self) -> Result<(), ConvertError> {
        let Some(pict) = self.dest.pict.take() else {
            return Ok(());
        };
        let format = pict.format;
        let bytes = pict.payload();
        if bytes.is_empty() {
            return Ok(());
        }
        let Some((media_type, extension)) = format else {
            self.parse_ctx.emit(DiagnosticCode::ImageUnavailable, self.region());
            return Ok(());
        };
        let part = format!("pict/{}.{extension}", self.assets.assets.len());
        let id = self.assets.add(media_type.to_string(), part, &bytes)?;
        self.inlines.push(Inline::Image {
            alt: String::new(),
            source: crate::model::ImageSource::Asset(id),
        });
        Ok(())
    }

    /// Table controls act outside suppressed groups and note bodies.
    fn table_active(&self) -> bool {
        !self.state.suppress && self.state.note.is_none()
    }

    fn pending_cell_prop(&mut self, f: impl FnOnce(&mut table::CellProp)) {
        if self.table_active() {
            let depth = self.state.itap.max(1);
            f(self.table.pending_prop(depth));
        }
    }

    fn set_style(&mut self, f: impl FnOnce(&mut Style)) {
        self.flush_pending();
        f(&mut self.state.style);
    }

    fn push_char(&mut self, c: char) {
        if !self.accepts_text() {
            return;
        }
        if self.decoder.skip_char() {
            return;
        }
        self.flush_pending();
        self.push_text(c.to_string(), TextLoss::default());
    }

    fn flush_pending(&mut self) {
        self.settle_surrogate();
        self.flush_text();
    }

    fn flush_text(&mut self) {
        let encoding = self.state.font.and_then(|f| self.prelude.fonts.get(&f).copied());
        if let Some((text, loss)) = self.decoder.take_pending(encoding) {
            self.push_text(text, loss);
        }
    }

    fn push_text(&mut self, raw: String, loss: TextLoss) {
        let text = clean_text(&raw);
        if self.captures_deleted() {
            if !text.is_empty() {
                self.drop_deleted();
            }
            return;
        }
        if matches!(self.state.capture, Capture::FieldInstr | Capture::Math) {
            if !text.is_empty() {
                self.deleted_range = None;
            }
            if self.state.capture == Capture::Math {
                if let Some(math) = &mut self.dest.math {
                    math.push_text(clean_text(&mark_loss(&raw, loss)));
                }
            } else if let Some(field) = self.dest.fields.last_mut() {
                let mut prev = None;
                for c in raw.chars() {
                    let after_cr = prev.replace(c) == Some('\r');
                    if c == '\n' && after_cr {
                        continue;
                    }
                    let piece = clean_text(c.encode_utf8(&mut [0; 4]));
                    let lost = char_loss(c, loss);
                    if lost.invalid || lost.assumed {
                        field.losses.push(InstrLoss {
                            at: field.instr.len(),
                            removed: piece.is_empty(),
                            loss: lost,
                        });
                    }
                    field.instr.push_str(&piece);
                }
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
            // Picture payload bytes are collected raw in the token loop.
            Capture::Pict | Capture::FieldInstr | Capture::Math => {}
            Capture::None if self.state.suppress => {}
            Capture::None if self.state.deleted => self.drop_deleted(),
            Capture::None => {
                self.deleted_range = None;
                self.inlines.push(Inline::Text { text, style: self.state.style });
            }
        }
    }

    /// Flush the finished top-level table, if any, into the block stream.
    fn flush_top_table(&mut self) -> Result<(), ConvertError> {
        if let Some(block) = self.table.take_table(1)? {
            self.flush_runs();
            self.blocks.push(block);
        }
        Ok(())
    }

    fn end_paragraph(&mut self) -> Result<(), ConvertError> {
        self.deleted_range = None;
        let inlines = std::mem::take(&mut self.inlines);
        let listtext = self.dest.listtext.take();
        let listtext_loss = std::mem::take(&mut self.dest.listtext_loss);
        let math_display = std::mem::take(&mut self.dest.math_display);

        if self.state.in_table {
            let depth = self.state.itap.max(1);
            self.table.push_cell_paragraph(depth, self.state.block, inlines);
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
            if let Some((key, _, number, label, _)) = &entry
                && key.marker.ordered()
            {
                let text =
                    format!("{} ", label.clone().unwrap_or_else(|| key.marker.label(*number)));
                content.insert(0, Inline::Text { text, style: Style::PLAIN });
            }
            self.blocks.push(Block::Heading { level, anchor: None, content });
            return Ok(());
        }
        if let Some((key, level, number, label, label_loss)) = entry {
            self.report(label_loss, self.region());
            self.list_run.push(ListEntry {
                level,
                key,
                number,
                label,
                blocks: vec![Block::Paragraph(inlines)],
            });
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

    #[allow(clippy::type_complexity)]
    fn list_entry(
        &mut self,
        listtext: Option<&str>,
        listtext_loss: TextLoss,
    ) -> Option<(ListKey, usize, u64, Option<String>, TextLoss)> {
        if let Some(ls) = self.state.ls {
            let level = self.state.ilvl;
            if let Some(list) = self.prelude.list(ls) {
                let def = &list.levels[level.min(LIST_LEVELS - 1)];
                let marker = def.marker?;
                let (number, label, loss) = if marker.ordered() {
                    self.counters.next_labeled(ls, level, &list.levels)
                } else {
                    (0, None, TextLoss::default())
                };
                if loss != TextLoss::default()
                    && self.labels_reported.insert((ls, level.min(LIST_LEVELS - 1)))
                {
                    self.report(loss, self.region());
                }
                return Some((
                    ListKey { instance: ls as u64, marker },
                    level,
                    number,
                    label,
                    TextLoss::default(),
                ));
            }
            // No table definition behind the \ls: the captured \listtext is
            // the only surviving evidence of the real marker; carry it as
            // the item's literal label over a bullet base.
            let (label, loss) = Self::listtext_label(listtext, listtext_loss);
            return Some((
                ListKey { instance: ls as u64, marker: MarkerKind::Bullet },
                level,
                0,
                label,
                loss,
            ));
        }
        if let Some(marker) = self.state.legacy_list {
            let number = if marker.ordered() {
                // Legacy \pn numbering: the label text carries the number,
                // clamped so a crafted label cannot overflow the counter.
                let parsed = listtext.and_then(|t| {
                    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
                    digits.parse::<u64>().ok().map(|n| n.min(u32::MAX as u64))
                });
                match parsed {
                    Some(n) => {
                        self.counters.seed(i32::MAX, self.state.ilvl, n);
                        n
                    }
                    None => self.counters.next(i32::MAX, self.state.ilvl, 1),
                }
            } else {
                0
            };
            return Some((
                ListKey { instance: u64::MAX, marker },
                self.state.ilvl,
                number,
                None,
                TextLoss::default(),
            ));
        }
        // A bare \listtext with no list state still marks a list paragraph
        // (some producers omit \ls); its text is the literal marker.
        if listtext.is_some_and(|t| !t.trim().is_empty()) {
            let (label, loss) = Self::listtext_label(listtext, listtext_loss);
            return Some((
                ListKey { instance: u64::MAX - 1, marker: MarkerKind::Bullet },
                self.state.ilvl,
                0,
                label,
                loss,
            ));
        }
        None
    }

    fn end_cell(&mut self, depth: usize) -> Result<(), ConvertError> {
        let inlines = std::mem::take(&mut self.inlines);
        self.dest.listtext = None;
        self.dest.listtext_loss = TextLoss::default();
        self.table.end_cell(depth, self.state.block, inlines)
    }

    fn end_row(&mut self, depth: usize) -> Result<(), ConvertError> {
        if self.table.has_pending_cell(depth) || !inlines_are_empty(&self.inlines) {
            self.end_cell(depth)?;
        }
        self.table.end_row(depth);
        Ok(())
    }

    /// Close every open block run before something else is emitted.
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
        // Collapse any dangling nested tables outward, then flush.
        self.table.collapse_nested()?;
        self.flush_top_table()?;
        self.flush_runs();
        Ok(Document { blocks: self.blocks, notes: self.dest.notes, assets: self.assets.assets })
    }
}

/// Whether a field instruction is a check box or drop-down form field,
/// whose state lives only in its form-field data.
fn is_choice_form_field(instr: &str) -> bool {
    let keyword = instr.split_whitespace().next().unwrap_or_default();
    keyword.eq_ignore_ascii_case("FORMCHECKBOX") || keyword.eq_ignore_ascii_case("FORMDROPDOWN")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_list_table_keeps_listtext_marker() {
        // A \ls with no list-table definition degrades to a bullet, but the
        // captured \listtext (the marker Word displays) must stay visible.
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
        // \page and \column carry no paragraph mark: without a break of
        // their own the text on either side would run together.
        for src in [r"{\rtf1 Alfa\page Beta\par}", r"{\rtf1 Alfa\column Beta\par}"] {
            let markdown = crate::render::markdown::document_to_markdown(
                &parse(&ParseCtx::default(), src.as_bytes()).unwrap(),
            );
            assert_eq!(markdown, "Alfa\\\nBeta\n", "source: {src}");
        }
    }

    #[test]
    fn backslash_before_a_line_break_breaks_the_paragraph() {
        // The paragraph mark Cocoa's RTF writer emits instead of `\par`.
        for src in
            ["{\\rtf1 Alpha\\\nBeta\\\nGamma\\\n}", "{\\rtf1 Alpha\\\r\nBeta\\\rGamma\\\r\n}"]
        {
            let markdown = crate::render::markdown::document_to_markdown(
                &parse(&ParseCtx::default(), src.as_bytes()).unwrap(),
            );
            assert_eq!(markdown, "Alpha\n\nBeta\n\nGamma\n", "source: {src:?}");
        }
    }

    #[test]
    fn body_text_before_a_table_stays_out_of_the_first_cell() {
        let src = "{\\rtf1 Intro\\\n\\trowd\\cellx2000\\cellx4000 A\\cell B\\cell\\row}";
        let markdown = crate::render::markdown::document_to_markdown(
            &parse(&ParseCtx::default(), src.as_bytes()).unwrap(),
        );
        assert!(markdown.starts_with("Intro\n\n|"), "{markdown}");
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
    fn pict_hex_payload_becomes_an_asset() {
        let doc =
            parse(&ParseCtx::default(), br"{\rtf1 before {\pict\pngblip 89504e470d0a1a0a} after}")
                .unwrap();
        assert_eq!(doc.assets.len(), 1, "assets: {:?}", doc.assets);
        assert_eq!(doc.assets[0].media_type, "image/png");
        assert!(doc.assets[0].bytes.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn pict_property_subgroups_do_not_contaminate_the_payload() {
        let doc = parse(&ParseCtx::default(),
            br"{\rtf1 {\pict{\*\picprop{\sp{\sn wzDescription}{\sv abcdef}}}\pngblip 89504e470d0a1a0a}}",
        )
        .unwrap();
        assert_eq!(doc.assets.len(), 1, "assets: {:?}", doc.assets);
        assert!(doc.assets[0].bytes.starts_with(&[0x89, b'P', b'N', b'G']));
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
        assert_eq!(parser.decoder.default_encoding.encoding, encoding_rs::WINDOWS_1251);
        assert!(parser.prelude.fonts.contains_key(&1));
        assert_eq!(parser.prelude.styles.get(&1).and_then(|style| style.outline), Some(1));
        let level = parser.prelude.list(2).map(|list| list.levels[0].marker);
        assert_eq!(level, Some(Some(MarkerKind::Decimal)));
        let markdown = crate::render::markdown::document_to_markdown(&parser.finish().unwrap());
        assert_eq!(markdown, "# \u{430}\n");
        let tokens = |bytes: &str| {
            let lexing = ParseCtx::default();
            let mut lexer = Lexer::new(&lexing, bytes.as_bytes());
            while lexer.next_token().unwrap().is_some() {}
            lexing.fuel()
        };
        let tables = [fonts, styles, lists, overrides].map(tokens).iter().sum::<u64>();
        assert_eq!(parse_ctx.fuel(), tokens(&src) + tables);
    }

    #[test]
    fn a_table_group_inside_one_of_its_kind_is_read_once() {
        let tokens = |bytes: &str| {
            let lexing = ParseCtx::default();
            let mut lexer = Lexer::new(&lexing, bytes.as_bytes());
            while lexer.next_token().unwrap().is_some() {}
            lexing.fuel()
        };
        let body = r"{\fonttbl{\f1 A;}}";
        let src = format!(r"{{\rtf1{{\fonttbl{body}}}x}}");
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, src.as_bytes()).unwrap();
        assert_eq!(parse_ctx.fuel(), tokens(&src) + tokens(body));
    }

    #[test]
    fn an_override_table_before_its_list_table_resolves_through_it() {
        let src = r"{\rtf1{\*\listoverridetable{\listoverride\listid7\ls2}}{\*\listtable{\list{\listlevel\levelnfc0{\leveltext\'02\'00.;}{\levelnumbers\'01;}}\listid7}}\pard\ls2\ilvl0 a\par}";
        assert_eq!(markdown(src), "1. a\n");
    }

    #[test]
    fn a_declared_code_page_overrides_a_guessed_character_set() {
        let src = r"{\rtf1\mac\ansicpg1252 \'e9\par}";
        assert_eq!(markdown(src), "\u{e9}\n");
        assert_eq!(reported(src), []);
    }

    #[test]
    fn a_code_page_declared_after_text_leaves_that_text_as_read() {
        let doc = parse(&ParseCtx::default(), br"{\rtf1 \'e0\ansicpg1251 \'e0\par}").unwrap();
        let markdown = crate::render::markdown::document_to_markdown(&doc);
        assert_eq!(markdown, "\u{e0}\u{430}\n");
    }

    type Reported = (DiagnosticCode, Region, u64);

    fn reported(src: &str) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, src.as_bytes()).unwrap();
        parse_ctx.diagnostics().iter().map(|d| (d.code, d.region, d.count)).collect()
    }

    fn markdown(src: &str) -> String {
        crate::render::markdown::document_to_markdown(
            &parse(&ParseCtx::default(), src.as_bytes()).unwrap(),
        )
    }

    const LISTS_932: &str = r"{\rtf1\ansi\ansicpg932{\*\listtable{\list{\listlevel\levelnfc0{\leveltext \'03\'00\'82.;}{\levelnumbers \'01;}}{\listlevel\levelnfc0{\leveltext \'02\'01\'82;}{\levelnumbers \'01;}}\listid1}{\list{\listlevel\levelnfc0{\leveltext \'02\'00.;}{\levelnumbers \'01;}}\listid2}}{\*\listoverridetable{\listoverride\listid1\ls1}{\listoverride\listid2\ls3}{\listoverride\listid2\listoverridecount1{\lfolevel\listoverrideformat{\listlevel\levelnfc0{\leveltext \'02\'00\'82;}{\levelnumbers \'01;}}}\ls2}}";

    #[test]
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
            ("a guessed blank label", r"{\rtf1\mac \pard{\listtext \'a0}\ls5 x\par}".into(), vec![]),
            ("a lone lead byte in a label shown", r"{\rtf1\ansi\ansicpg932 \pard{\listtext \'82}\ls5 x\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte in a label read for its number", r"{\rtf1\ansi\ansicpg932 \pard{\pntext 1\'82}\pnlvlbody x\par}".into(), vec![]),
            ("a lone lead byte in a link target", r#"{\rtf1\ansi\ansicpg932 {\field{\*\fldinst HYPERLINK "http://x/\'82"}{\fldrslt link}}\par}"#.into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte in a link tooltip", r#"{\rtf1\ansi\ansicpg932 {\field{\*\fldinst HYPERLINK "http://x/" \\o "\'82"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a guessed code page in a link target", r#"{\rtf1\mac {\field{\*\fldinst HYPERLINK "http://x/\'8e"}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed space before a guessed link target", r#"{\rtf1\mac {\field{\*\fldinst HYPERLINK\'a0"http://x/\'8e"}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed soft hyphen in the keyword of a guessed link target", r#"{\rtf1\mac {\field{\*\fldinst HYPER\'adLINK "http://x/\'8e"}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed space before a link anchor", r#"{\rtf1\mac {\field{\*\fldinst HYPERLINK "http://x/" \\l \'a0a}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a guessed space after a bare link target", r#"{\rtf1\mac {\field{\*\fldinst HYPERLINK http://x/\'a0\\l a}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a guessed soft hyphen ending a bare link target", r#"{\rtf1\mac {\field{\*\fldinst HYPERLINK http://x/\'ad \\l a}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
            ("a dropped character ending a bare link target", r#"{\rtf1 {\field{\*\fldinst HYPERLINK http://x/\u56320? \\l a}{\fldrslt link}}\par}"#.into(), vec![(InvalidEncoding, Body, 1)]),
            ("a guessed code page in a link tooltip", r#"{\rtf1\mac {\field{\*\fldinst HYPERLINK "http://x/" \\o "\'8e"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a guessed code page in a link anchor", r#"{\rtf1\mac {\field{\*\fldinst HYPERLINK "http://x/" \\l \'8e}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed code page in an unresolved anchor link", r#"{\rtf1\mac {\field{\*\fldinst HYPERLINK \\l "\'8e"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a guessed code page in an anchor link to a bookmark", r#"{\rtf1\mac {\*\bkmkstart \'8e}x{\field{\*\fldinst HYPERLINK \\l "\'8e"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a guessed code page in an anchor link to a heading", r#"{\rtf1\mac \outlinelevel0{\*\bkmkstart \'8e}Head\par\pard {\field{\*\fldinst HYPERLINK \\l "\'8e"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a guessed code page in a hash link to a heading", r##"{\rtf1\mac \outlinelevel0{\*\bkmkstart \'8e}Head\par\pard {\field{\*\fldinst HYPERLINK "#\'8e"}{\fldrslt link}}\par}"##.into(), vec![]),
            ("a guessed byte that cleaning removes in a link anchor", r#"{\rtf1\mac {\field{\*\fldinst HYPERLINK \\l "a\'8fb"}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
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
            ("deleted ranges", r"{\rtf1 a{\deleted b}c{\deleted d\par e}f{\revised g}\deleted h\plain i\par}".into(), vec![(RevisionOmitted, Body, 3)]),
            ("deleted ranges in two paragraphs", r"{\rtf1 {\deleted a}\par{\deleted b}\par}".into(), vec![(RevisionOmitted, Body, 2)]),
            ("a deleted range in a footnote", r"{\rtf1 a{\footnote b{\deleted c}}}".into(), vec![(RevisionOmitted, Note, 1)]),
            ("a damaged deleted range", r"{\rtf1\ansi\ansicpg932 a{\deleted \'82\chpgn}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a guessed Mac code page", r"{\rtf1\mac \'8e{\b\'8f}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("an unknown code page", r"{\rtf1\ansi\ansicpg10000 \'8e\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("the UTF-8 code page", r"{\rtf1\ansi\ansicpg65001 \'c3\'a9\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("an unknown font charset", r"{\rtf1\ansi\ansicpg1252{\fonttbl{\f1\fcharset77 M;}}a\f1 \'8e\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed code page in a footnote", r"{\rtf1\pc a{\footnote \'8e}}".into(), vec![(EncodingAssumed, Note, 1)]),
            ("a guessed code page over ASCII", r"{\rtf1\mac plain\par}".into(), vec![]),
            ("a declared code page", r"{\rtf1\ansi\ansicpg1252{\fonttbl{\f1\fcharset0 A;}{\f2\fcharset2 S;}}\'e9\f1\'e9\f2\'e9\par}".into(), vec![]),
            ("a guessed byte that cleaning removes", r"{\rtf1\mac a{\'ad}b\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed PC code page", r"{\rtf1\pca \'8e\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a high surrogate before text", r"{\rtf1 a\u55296? b\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a high surrogate at a paragraph end", r"{\rtf1 a\u55296?\par b\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a high surrogate at the end", r"{\rtf1 a\u55296?}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("surrogate halves split by a paragraph", r"{\rtf1 a\u55357? b\par c\u56832? d\par}".into(), vec![(InvalidEncoding, Body, 2)]),
            ("a high surrogate in a footnote before body text", r"{\rtf1 a{\footnote b\u55296?} c\par}".into(), vec![(InvalidEncoding, Note, 1)]),
            ("a lone surrogate in a link target", r#"{\rtf1 {\field{\*\fldinst HYPERLINK "http://x/\u56320?"}{\fldrslt link}}\par}"#.into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone surrogate in a link tooltip", r#"{\rtf1 {\field{\*\fldinst HYPERLINK "http://x/" \\o "\u55296?"}{\fldrslt link}}\par}"#.into(), vec![]),
            ("a damaged link target with no result", r#"{\rtf1\ansi\ansicpg932 {\field{\*\fldinst HYPERLINK "http://x/\'82"}{\fldrslt }}\par}"#.into(), vec![]),
            ("a clean Unicode link target beside a guessed tooltip", r#"{\rtf1\mac {\field{\*\fldinst HYPERLINK "http://x/\u233?" \\o "\'8e"}{\fldrslt link}}\par}"#.into(), vec![]),
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
            ("a guessed code page in math", r"{\rtf1\mac {\mmath{\*\moMath{\mr \'8e}}}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a deleted picture in a header", r"{\rtf1 {\header {\deleted {\pict\pngblip 89}}}a\par}".into(), vec![(HeaderOmitted, Header, 1)]),
            ("an empty unread picture", r"{\rtf1 {\pict\macpict}}".into(), vec![]),
            ("a guessed code page in a degree shown after a damaged flag", r"{\rtf1\mac {\mmath{\*\moMath{\mrad{\mradPr{\mdegHide \'ad0}}{\mdeg{\mr \'8e}}{\me{\mr x}}}}}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed code page in a hidden degree", r"{\rtf1\mac {\mmath{\*\moMath{\mrad{\mradPr{\mdegHide1}}{\mdeg{\mr \'8e}}{\me{\mr x}}}}}\par}".into(), vec![]),
            ("a lone surrogate in a hidden degree", r"{\rtf1 {\mmath{\*\moMath{\mrad{\mradPr{\mdegHide1}}{\mdeg{\mr \u56320?}}{\me{\mr x}}}}}\par}".into(), vec![]),
            ("a high surrogate dropped in a hidden limit", r"{\rtf1 {\mmath{\*\moMath{\mnary{\mnaryPr{\msubHide1}}{\msub{\mr i\u55296?}}{\msup{\mr n}}{\me{\mr i}}}}}\par}".into(), vec![]),
            ("a high surrogate dropped in a shown limit", r"{\rtf1 {\mmath{\*\moMath{\mnary{\msub{\mr i\u55296?}}{\msup{\mr n}}{\me{\mr i}}}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a guessed code page in a shown degree", r"{\rtf1\mac {\mmath{\*\moMath{\mrad{\mdeg{\mr \'8e}}{\me{\mr x}}}}}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
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
            ("a guessed accent character", r"{\rtf1\mac {\mmath{\*\moMath{\macc{\maccPr{\mchr \'a8}}{\me{\mr x}}}}}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a lone lead byte as a delimiter", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\md{\mdPr{\mbegChr \'82}}{\me{\mr x}}}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte as a closing delimiter", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\md{\mdPr{\mendChr \'82}}{\me{\mr x}}}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte as a shown separator", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\md{\mdPr{\msepChr \'82}}{\me{\mr x}}{\me{\mr y}}}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte as an unused separator", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\md{\mdPr{\msepChr \'82}}{\me{\mr x}}}}}\par}".into(), vec![]),
            ("a lone lead byte as an operator", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\mnary{\mnaryPr{\mchr \'82}}{\me{\mr x}}}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte as a group character", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\mgroupChr{\mgroupChrPr{\mchr \'82}}{\me{\mr x}}}}}\par}".into(), vec![(InvalidEncoding, Body, 1)]),
            ("a lone lead byte as a delimiter in a hidden degree", r"{\rtf1\ansi\ansicpg932 {\mmath{\*\moMath{\mrad{\mradPr{\mdegHide1}}{\mdeg{\md{\mdPr{\mbegChr \'82}}{\me{\mr y}}}}{\me{\mr x}}}}}\par}".into(), vec![]),
            ("a deleted check box with plain instruction text", r"{\rtf1 a{\deleted {\field{\*\fldinst {\plain FORMCHECKBOX}}{\fldrslt }}}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a deleted soft hyphen in math", r"{\rtf1 {\mmath{\*\moMath{\mr {\deleted \'ad}x}}}\par}".into(), vec![]),
            ("a deleted guessed math run", r"{\rtf1\mac {\mmath{\*\moMath{\mr {\deleted \'8e}x}}}\par}".into(), vec![(RevisionOmitted, Body, 1)]),
            ("a fallback picture in a header", r"{\rtf1 {\header {\*\shppict{\pict\wbitmap 01}}}a\par}".into(), vec![(HeaderOmitted, Header, 1)]),
            ("shape text in a header", r"{\rtf1 {\header {\shp{\*\shpinst{\shptxt \chpgn}}}}a\par}".into(), vec![(HeaderOmitted, Header, 1)]),
            ("a footnote in a header", r"{\rtf1 {\header a{\footnote \chpgn}}b\par}".into(), vec![(HeaderOmitted, Header, 1)]),
            ("a guessed byte that cleaning removes in math", r"{\rtf1\mac {\mmath{\*\moMath{\mr \'8f}}}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed byte that cleaning removes in a link target", r#"{\rtf1\mac {\field{\*\fldinst HYPERLINK "http://x/\'8f"}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed space in math", r"{\rtf1\mac {\mmath{\*\moMath{\mr x\'a0y}}}\par}".into(), vec![(EncodingAssumed, Body, 1)]),
            ("a guessed space in a link target", r#"{\rtf1\mac {\field{\*\fldinst HYPERLINK "http://x/a\'a0b"}{\fldrslt link}}\par}"#.into(), vec![(EncodingAssumed, Body, 1)]),
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
        assert_eq!(
            markdown(
                r"{\rtf1 a{\deleted b}c{\deleted d\par e}f{\revised g}\deleted h\plain i\par}"
            ),
            "acfgi\n"
        );
        assert_eq!(markdown(r"{\rtf1 one\deleted\par\deleted0 two\par}"), "onetwo\n");
        assert_eq!(markdown(r"{\rtf1 a{\deleted b\line c}d\par}"), "ad\n");
        assert_eq!(markdown(r"{\rtf1 a{\deleted{\mmath{\*\moMath{\mr x}}}}b\par}"), "ab\n");
        assert_eq!(
            markdown(
                r"{\rtf1 a{\deleted{\mmath{\*\moMath{\mf{\mnum{\mr b}}{\mden{\mr c}}}}}}d\par}"
            ),
            "ad\n"
        );
        assert_eq!(
            markdown(
                r"{\rtf1 {\mmath{\*\moMath{\deleted{\mf{\mnum{\mr b}}{\mden{\mr c}}}}{\mr x}}}\par}"
            ),
            "$x$\n"
        );
        let doc = parse(&ParseCtx::default(), br"{\rtf1 a{\deleted {\pict\pngblip 89504e47}}\par}");
        assert!(doc.unwrap().assets.is_empty());
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
        let guessed = markdown(&format!(r"{{\rtf1\mac {equation}"));
        assert_eq!(guessed, markdown(&format!(r"{{\rtf1\ansi\ansicpg1252 {equation}")));
        assert!(guessed.contains('é'), "{guessed}");
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
}
