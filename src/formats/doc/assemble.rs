//! Assembly: the text of a story and its formatting runs into the model.

use super::fkp::{Run, Runs};
use super::fonts::FontTable;
use super::lists::{Counters, ListDef, Lists};
use super::sprm::{
    self, Chp, PapDelta, SPRM_CF_DATA, SPRM_CF_OLE2, Tap, apply_pap_sprms, direct_chp, sets_flag,
};
use super::stsh::Stylesheet;
use super::text::{Cursor, Event, Scalar, TextView};
use super::{ShapeKind, shape_kinds};
use crate::ctx::{ImageLoss, ParseCtx};
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{Block, ImageSource, Inline, LinkTarget, Style, inlines_are_empty};
use crate::shared::assets::AssetSink;
use crate::shared::binary::ByteReader;
use crate::shared::blockstyle::{BlockStyle, StyledRun};
use crate::shared::delta::rebase_emphasis;
use crate::shared::fields::{
    FieldFrame, TargetSpans, field_result, hyperlink_target_spans, is_choice_form_field,
};
use crate::shared::grid::{CellProp, GridRow, build_edge_table};
use crate::shared::list::{ListEntry, ListKey, ListMark, flush_list};
use crate::shared::officeart;
use crate::shared::symbol::Glyph;
use crate::shared::uri::is_safe_link;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

/// Text whose units or formatting do not reach the output intact: a piece's undecodable
/// characters, the number text of the list level at a table-stream offset, a piece whose bytes
/// the stream does not hold, or the font index of an unresolved font.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Damaged {
    Piece(u32),
    Level(usize),
    Missing(u32),
    Font(u16),
}

/// Pieces, each once, whose undecodable text a part of a paragraph holds.
type Pieces = BTreeSet<u32>;

/// Add `from` to `into`, charging each piece copied.
fn lift(parse_ctx: &ParseCtx<'_>, into: &mut Pieces, from: Pieces) -> Result<(), ConvertError> {
    parse_ctx.charge(from.len() as u64)?;
    into.extend(from);
    Ok(())
}

pub(super) struct Assembler<'c, 'w> {
    parse_ctx: &'c ParseCtx<'c>,
    text: TextView<'w>,
    chpx: Runs,
    papx: Runs,
    stylesheet: Stylesheet,
    fonts: FontTable,
    lists: Lists,
    /// The id of the note each reference CP names.
    note_refs: HashMap<usize, String>,
    pub(super) dropped_notes: RefCell<HashSet<String>>,
    /// The floating shape each main-document anchor CP names.
    shapes: HashMap<usize, ShapeKind>,
    counters: RefCell<Counters>,
    /// The damaged text and regions already reported.
    reported: RefCell<HashSet<(Damaged, Region)>>,
    /// The Data stream, for `sprmCPicLocation` picture payloads.
    data: Vec<u8>,
    pub(super) assets: RefCell<AssetSink>,
}

/// A paragraph's resolved properties: the style chain's contribution merged
/// with the PAPX and any piece `Prm`.
struct EffectivePap {
    istd: u16,
    effective: PapDelta,
}

#[derive(Clone, Copy, PartialEq)]
enum Holder {
    Body,
    Cell(i32),
    Row,
}

/// The scalars before the CP `end` that share one CHPX run, one PAPX run and
/// one piece, and their character properties.
#[derive(Default)]
struct Segment {
    end: usize,
    chp: Chp,
}

/// Why a character does not render; a deletion wins over hiding.
#[derive(Clone, Copy, PartialEq)]
enum Dropped {
    Revision,
    Hidden,
}

impl Dropped {
    fn of(chp: Chp) -> Option<Self> {
        chp.deleted.then_some(Self::Revision).or_else(|| chp.hidden.then_some(Self::Hidden))
    }
}

#[derive(Default)]
struct DroppedRange {
    kind: Option<Dropped>,
    renders: bool,
}

impl DroppedRange {
    /// Extend the range by a character dropped as `kind`, closing it first
    /// when the kind changes; a shown character has none.
    fn extend(
        &mut self,
        parse_ctx: &ParseCtx<'_>,
        region: Region,
        kind: Option<Dropped>,
        renders: impl FnOnce() -> bool,
    ) {
        if kind != self.kind {
            self.close(parse_ctx, region);
            self.kind = kind;
        }
        if kind.is_some() && !self.renders {
            self.renders = renders();
        }
    }

    fn close(&mut self, parse_ctx: &ParseCtx<'_>, region: Region) {
        let code = match self.kind {
            Some(Dropped::Revision) => DiagnosticCode::RevisionOmitted,
            Some(Dropped::Hidden) => DiagnosticCode::HiddenOmitted,
            None => return,
        };
        if self.renders {
            parse_ctx.emit(code, region);
        }
        *self = Self::default();
    }
}

/// Whether `c`, outside a field instruction, shows something: a picture, a
/// floating shape's anchor or a character other than whitespace.
fn renders(c: char) -> bool {
    matches!(c, '\u{1}' | '\u{8}' | '\u{1e}') || !(c.is_control() || c.is_whitespace())
}

fn push_shown(
    c: char,
    s: &Scalar,
    style: Style,
    para: &mut ParaBuilder<'_>,
) -> Result<(), ConvertError> {
    match shown_char(c) {
        Some(shown) => para.push_char(shown, style, s.replaced.then_some(s.piece)),
        None => Ok(()),
    }
}

fn is_mark(c: char) -> bool {
    matches!(c, '\r' | '\u{7}' | '\u{c}' | '\u{e}')
}

/// What a character that is no field delimiter adds to the text it stands in.
fn shown_char(c: char) -> Option<char> {
    match c {
        '\t' => Some(' '),
        '\u{1e}' => Some('-'),
        c if c.is_control() => None,
        c => Some(c),
    }
}

/// An open field: the CPs of its instruction, and the pieces whose undecodable
/// text its result shows, as label or as the targets of links completed in it.
struct OpenField {
    frame: FieldFrame,
    instr: Range<usize>,
    result_damage: Pieces,
    target_damage: Pieces,
}

struct ParaBuilder<'c> {
    parse_ctx: &'c ParseCtx<'c>,
    inlines: Vec<Inline>,
    fields: Vec<OpenField>,
    /// The open fields still in their instruction.
    instructions: usize,
    text: String,
    style: Style,
    /// The pieces whose undecodable text `text` holds.
    text_damage: Pieces,
    /// The pieces whose undecodable text `inlines` holds.
    damage: Pieces,
}

impl<'c> ParaBuilder<'c> {
    fn new(parse_ctx: &'c ParseCtx<'c>) -> Self {
        Self {
            parse_ctx,
            inlines: Vec::new(),
            fields: Vec::new(),
            instructions: 0,
            text: String::new(),
            style: Style::PLAIN,
            text_damage: Pieces::new(),
            damage: Pieces::new(),
        }
    }

    fn flush_text(&mut self) -> Result<(), ConvertError> {
        if self.text.is_empty() {
            return Ok(());
        }
        let text = std::mem::take(&mut self.text);
        let damage = std::mem::take(&mut self.text_damage);
        let style = self.style;
        match self.fields.last_mut() {
            Some(f) if !f.frame.in_result => f.frame.instr.push_str(&text),
            Some(f) => {
                f.frame.inlines.push(Inline::Text { text, style });
                lift(self.parse_ctx, &mut f.result_damage, damage)?;
            }
            None => {
                self.inlines.push(Inline::Text { text, style });
                lift(self.parse_ctx, &mut self.damage, damage)?;
            }
        }
        Ok(())
    }

    /// Push `c`, which stands in for undecodable bytes of piece `damaged`
    /// when one is given.
    fn push_char(
        &mut self,
        c: char,
        style: Style,
        damaged: Option<u32>,
    ) -> Result<(), ConvertError> {
        if style != self.style {
            self.flush_text()?;
            self.style = style;
        }
        self.text.push(c);
        if let Some(piece) = damaged {
            self.parse_ctx.charge(1)?;
            self.text_damage.insert(piece);
        }
        Ok(())
    }

    fn push_inline(&mut self, inline: Inline) -> Result<(), ConvertError> {
        self.flush_text()?;
        match self.fields.last_mut() {
            Some(f) if !f.frame.in_result => {}
            Some(f) => f.frame.inlines.push(inline),
            None => self.inlines.push(inline),
        }
        Ok(())
    }

    /// Open a field whose instruction starts at the CP `instr`.
    fn field_begin(&mut self, max_depth: u64, instr: usize) -> Result<(), ConvertError> {
        if self.fields.len() as u64 >= max_depth {
            return Err(ConvertError::limit(Limit::NestingDepth, max_depth));
        }
        self.flush_text()?;
        self.fields.push(OpenField {
            frame: FieldFrame::default(),
            instr: instr..instr,
            result_damage: Pieces::new(),
            target_damage: Pieces::new(),
        });
        self.instructions += 1;
        Ok(())
    }

    /// End the innermost instruction at the separator at CP `cp`.
    fn field_separate(&mut self, cp: usize) -> Result<(), ConvertError> {
        self.flush_text()?;
        if let Some(f) = self.fields.last_mut()
            && !f.frame.in_result
        {
            f.frame.in_result = true;
            f.instr.end = cp;
            self.instructions -= 1;
        }
        Ok(())
    }

    /// Whether text here lands in a field instruction, which never renders.
    fn in_instruction(&self) -> bool {
        self.instructions > 0
    }

    /// Whether the innermost open field is in the result of a `SHAPE`
    /// field, the inline stand-in of the floating shape its `\x08` anchors.
    fn in_shape_result(&self) -> bool {
        self.fields.last().is_some_and(|f| {
            let keyword = f.frame.instr.split_whitespace().next().unwrap_or_default();
            f.frame.in_result && keyword.eq_ignore_ascii_case("SHAPE")
        })
    }

    /// Close the innermost field at CP `cp`; `targets` gives the pieces whose
    /// undecodable text the target of the link it makes shows.
    fn field_end(
        &mut self,
        cp: usize,
        targets: &mut impl FnMut(&OpenField) -> Result<Pieces, ConvertError>,
    ) -> Result<(), ConvertError> {
        self.flush_text()?;
        let Some(mut field) = self.fields.pop() else {
            return Ok(());
        };
        if !field.frame.in_result {
            field.instr.end = cp;
            self.instructions -= 1;
        }
        let shown = !matches!(self.fields.last(), Some(f) if !f.frame.in_result);
        let own = if shown { targets(&field)? } else { Pieces::new() };
        let OpenField { frame, result_damage, mut target_damage, .. } = field;
        lift(self.parse_ctx, &mut target_damage, own)?;
        let inlines = field_result(&frame.instr, frame.inlines);
        match self.fields.last_mut() {
            Some(f) if !f.frame.in_result => {}
            Some(f) => {
                f.frame.inlines.extend(inlines);
                lift(self.parse_ctx, &mut f.result_damage, result_damage)?;
                lift(self.parse_ctx, &mut f.target_damage, target_damage)?;
            }
            None => {
                self.inlines.extend(inlines);
                lift(self.parse_ctx, &mut self.damage, result_damage)?;
                lift(self.parse_ctx, &mut self.damage, target_damage)?;
            }
        }
        Ok(())
    }

    /// Text the stream does not hold: the results known so far stay in source
    /// order, no open field makes its link, and every open instruction and
    /// the fields in it are dropped.
    fn gap(&mut self) -> Result<(), ConvertError> {
        self.flush_text()?;
        self.instructions = 0;
        for field in std::mem::take(&mut self.fields) {
            if !field.frame.in_result {
                break;
            }
            self.inlines.extend(field.frame.inlines);
            lift(self.parse_ctx, &mut self.damage, field.result_damage)?;
            lift(self.parse_ctx, &mut self.damage, field.target_damage)?;
        }
        Ok(())
    }

    /// The paragraph's inlines, every field still open closed at CP `cp`, and
    /// the pieces whose undecodable text they hold.
    fn finish(
        mut self,
        cp: usize,
        targets: &mut impl FnMut(&OpenField) -> Result<Pieces, ConvertError>,
    ) -> Result<(Vec<Inline>, Pieces), ConvertError> {
        self.flush_text()?;
        while !self.fields.is_empty() {
            self.field_end(cp, targets)?;
        }
        Ok((self.inlines, self.damage))
    }
}

impl<'c, 'w> Assembler<'c, 'w> {
    #[expect(
        clippy::too_many_arguments,
        reason = "one argument per stream and table the walk reads"
    )]
    pub(super) fn new(
        parse_ctx: &'c ParseCtx<'c>,
        text: TextView<'w>,
        chpx: Vec<Run>,
        papx: Vec<Run>,
        stylesheet: Stylesheet,
        fonts: FontTable,
        lists: Lists,
        note_refs: HashMap<usize, String>,
        shapes: HashMap<usize, ShapeKind>,
        data: Vec<u8>,
    ) -> Self {
        Self {
            parse_ctx,
            text,
            chpx: Runs::new(chpx),
            papx: Runs::new(papx),
            stylesheet,
            fonts,
            lists,
            note_refs,
            dropped_notes: RefCell::default(),
            shapes,
            counters: RefCell::default(),
            reported: RefCell::default(),
            data,
            assets: RefCell::new(AssetSink::new(parse_ctx)),
        }
    }
}

impl Assembler<'_, '_> {
    /// The blocks of the scalars of `range` in `story`, or `None` when it
    /// holds none.
    pub(super) fn build_blocks(
        &self,
        story: &Range<usize>,
        range: &Range<usize>,
        region: Region,
    ) -> Result<Option<Vec<Block>>, ConvertError> {
        let parse_ctx = self.parse_ctx;
        let mut flow = Flow::default();
        let mut para = ParaBuilder::new(parse_ctx);
        let mut segment = Segment::default();
        let mut dropped = DroppedRange::default();
        let mut read = false;
        let mut targets = |field: &OpenField| self.target_damage(story, field);

        self.text.preflight(parse_ctx, range)?;
        let mut cursor = self.text.cursor(parse_ctx, story, range)?;
        while let Some(event) = cursor.next(parse_ctx)? {
            let s = match event {
                Event::Scalar(s) => s,
                Event::Gap(gap) => {
                    self.report([Damaged::Missing(gap.piece)], region);
                    para.gap()?;
                    segment = Segment::default();
                    continue;
                }
            };
            read = true;
            if s.cp >= segment.end {
                parse_ctx.charge(1)?;
                segment = self.segment(&s, Some(region))?;
            }
            let drop = Dropped::of(segment.chp);
            if self.note_reference(&s, drop, &mut dropped, &mut para, region)? {
                continue;
            }
            let (c, mark) = (s.c, is_mark(s.c));
            let delimiter = matches!(c, '\u{13}' | '\u{14}' | '\u{15}');
            if mark {
                dropped.close(parse_ctx, region);
            } else {
                dropped.extend(parse_ctx, region, drop, || renders(c) && !para.in_instruction());
            }
            if drop.is_some() && !mark && !delimiter {
                continue;
            }
            match c {
                '\r' if drop.is_some() && self.joins_next(&cursor, &s)? => {}
                '\r' | '\u{7}' | '\u{c}' | '\u{e}' => {
                    let pap = self.effective_pap(s.fc, s.piece)?;
                    let (inlines, damage) =
                        std::mem::replace(&mut para, ParaBuilder::new(parse_ctx))
                            .finish(s.cp, &mut targets)?;
                    self.report(damage.into_iter().map(Damaged::Piece), region);
                    self.end_paragraph(c, pap, inlines, &mut flow, region)?;
                }
                _ => {
                    self.character(&s, drop, &segment.chp, &mut para, &mut targets, region)?;
                }
            }
        }
        dropped.close(parse_ctx, region);
        let (inlines, damage) = para.finish(range.end.min(story.end), &mut targets)?;
        self.report(damage.into_iter().map(Damaged::Piece), region);
        let blocks = flow.finish(parse_ctx, inlines)?;
        Ok(read.then_some(blocks))
    }

    fn note_reference(
        &self,
        s: &Scalar,
        drop: Option<Dropped>,
        dropped: &mut DroppedRange,
        para: &mut ParaBuilder<'_>,
        region: Region,
    ) -> Result<bool, ConvertError> {
        if s.c.len_utf16() == 2
            && let Some(id) = self.note_refs.get(&(s.cp + 1))
        {
            if drop.is_some() {
                self.dropped_notes.borrow_mut().insert(id.clone());
            } else if !para.in_instruction() {
                self.parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Note);
            }
        }
        let Some(id) = self.note_refs.get(&s.cp) else {
            return Ok(false);
        };
        dropped.extend(self.parse_ctx, region, drop, || true);
        if drop.is_some() {
            self.dropped_notes.borrow_mut().insert(id.clone());
        } else {
            para.push_inline(Inline::NoteRef(id.clone()))?;
        }
        Ok(true)
    }

    /// Add the character `s`, which is no paragraph mark, to `para`.
    fn character(
        &self,
        s: &Scalar,
        drop: Option<Dropped>,
        chp: &Chp,
        para: &mut ParaBuilder<'_>,
        targets: &mut impl FnMut(&OpenField) -> Result<Pieces, ConvertError>,
        region: Region,
    ) -> Result<(), ConvertError> {
        let parse_ctx = self.parse_ctx;
        match s.c {
            '\u{b}' => para.push_inline(Inline::LineBreak)?,
            '\u{13}' => para.field_begin(parse_ctx.limits().max_nesting_depth, s.cp + 1)?,
            '\u{14}' => para.field_separate(s.cp)?,
            '\u{15}' => {
                para.flush_text()?;
                let choice = drop.is_none()
                    && para.fields.last().is_some_and(|f| is_choice_form_field(&f.frame.instr));
                para.field_end(s.cp, targets)?;
                if choice && !para.in_instruction() {
                    parse_ctx.emit(DiagnosticCode::FormControlUnread, region);
                }
            }
            '\u{1}' => self.object_char(s.fc, region, para)?,
            '\u{8}' => self.anchored_shape(s.cp, region, para),
            c if para.in_instruction() => push_shown(c, s, chp.style, para)?,
            c => match self.fonts.draw(chp, c) {
                (Glyph::Char(drawn), unresolved) => {
                    if let Some(ftc) = unresolved.filter(|_| renders(drawn)) {
                        self.report([Damaged::Font(ftc)], region);
                    }
                    push_shown(drawn, s, chp.style, para)?;
                }
                (Glyph::Checkbox(checked), _) => para.push_inline(Inline::Checkbox(checked))?,
                (Glyph::Unmapped, _) => parse_ctx.emit(DiagnosticCode::UnmappedSymbol, region),
            },
        }
        Ok(())
    }

    /// A floating shape reports where its anchor shows: a field instruction never renders.
    fn anchored_shape(&self, cp: usize, region: Region, para: &ParaBuilder<'_>) {
        if para.in_instruction() {
            return;
        }
        match self.shapes.get(&cp).copied().unwrap_or(ShapeKind::Graphic) {
            ShapeKind::Picture => self.parse_ctx.image_loss(ImageLoss::Unplaced, region),
            ShapeKind::Graphic => self.parse_ctx.emit(DiagnosticCode::GraphicUnread, region),
            ShapeKind::TextBox => {}
        }
    }

    fn end_paragraph(
        &self,
        c: char,
        pap: EffectivePap,
        inlines: Vec<Inline>,
        flow: &mut Flow,
        region: Region,
    ) -> Result<(), ConvertError> {
        let cell_mark = c == '\u{7}';
        if !(pap.effective.in_table.unwrap_or(false) || cell_mark) {
            flow.flush_table(self.parse_ctx)?;
            self.emit_paragraph(&pap, inlines, flow, region);
            return Ok(());
        }
        flow.flush_runs();
        let inner = pap.effective.itap.unwrap_or(1) > 1
            || pap.effective.inner_cell.unwrap_or(false)
            || pap.effective.inner_ttp.unwrap_or(false);
        let row_end = cell_mark && !inner && pap.effective.ttp.unwrap_or(false);
        if !row_end {
            flow.cell_paragraph(self.stylesheet.get(pap.istd).block, inlines);
        }
        if cell_mark && !inner {
            flow.cell_styled.flush(&mut flow.cell);
            if row_end {
                // The row-end mark's PAPX carries the TAP: boundaries, merge
                // flags and the header row.
                if !flow.row.is_empty() {
                    let cells = std::mem::take(&mut flow.row);
                    flow.rows.push(DocRow { cells, tap: pap.effective.tap });
                }
            } else {
                flow.row.push(std::mem::take(&mut flow.cell));
            }
        }
        Ok(())
    }

    /// A field instruction never renders, an embedded object's location names its
    /// `ObjectPool` storage, and field data (a hyperlink's or form field's) holds no picture.
    fn object_char(
        &self,
        fc: u32,
        region: Region,
        para: &mut ParaBuilder<'_>,
    ) -> Result<(), ConvertError> {
        if para.in_instruction() {
            return Ok(());
        }
        let chpx = self.chpx.lookup(fc).map(|p| p.chpx.as_slice());
        let sets = |flag| chpx.is_some_and(|chpx| sets_flag(chpx, flag));
        if sets(SPRM_CF_OLE2) {
            self.parse_ctx.emit(DiagnosticCode::EmbeddedObject, region);
        } else if !sets(SPRM_CF_DATA)
            && let Some(image) = self.picture_at(fc, region, para.in_shape_result())?
        {
            para.push_inline(image)?;
        }
        Ok(())
    }

    /// The segment from the scalar `s`, with its properties in specification
    /// order: paragraph style chain -> CHPX -> piece Prm, where a `sprmCIstd`
    /// resets to the paragraph style and applies its character style at its
    /// own position. A style reference that resolves to no character style
    /// is reported in `region`, when one is given.
    fn segment(&self, s: &Scalar, region: Option<Region>) -> Result<Segment, ConvertError> {
        let (para, para_end) = self.papx.span(s.fc);
        let (chpx, chpx_end) = self.chpx.span(s.fc);
        let chpx = chpx.map_or(&[][..], |p| p.chpx.as_slice());
        let paragraph = self.stylesheet.get(para.map_or(0, |p| p.istd)).chp;
        let prm = self.text.prm(s.piece);
        let grpprls = [chpx, prm.grpprl().unwrap_or_default()];
        let (chp, unresolved) =
            direct_chp(self.parse_ctx, paragraph, grpprls, |istd| self.stylesheet.character(istd))?;
        if unresolved && let Some(region) = region {
            self.parse_ctx.emit(DiagnosticCode::FormattingUnreadable, region);
        }
        let pcd = self.text.pcd(s.piece);
        let units = u64::from(para_end.min(chpx_end))
            .saturating_sub(u64::from(s.fc))
            .div_ceil(pcd.width() as u64)
            .max(1);
        let shared = usize::try_from(units).unwrap_or(usize::MAX);
        Ok(Segment { end: pcd.cps.1.min(s.cp.saturating_add(shared)), chp })
    }

    /// Whether the dropped paragraph mark `s`, a plain one that ends no cell
    /// or row, joins its paragraph to the next one, which must end before the
    /// end of `cursor`'s range in the same container and with no text the
    /// stream does not hold between them.
    fn joins_next(&self, cursor: &Cursor<'_, '_>, s: &Scalar) -> Result<bool, ConvertError> {
        let here = self.effective_pap(s.fc, s.piece)?.effective;
        if [here.inner_cell, here.ttp, here.inner_ttp].iter().any(|flag| flag.unwrap_or(false)) {
            return Ok(false);
        }
        let mut ahead = cursor.fork();
        while let Some(event) = ahead.next(self.parse_ctx)? {
            match event {
                Event::Gap(_) => return Ok(false),
                Event::Scalar(next) if is_mark(next.c) => {
                    let there = self.effective_pap(next.fc, next.piece)?.effective;
                    return Ok(holder('\r', &here) == holder(next.c, &there));
                }
                Event::Scalar(_) => {}
            }
        }
        Ok(false)
    }

    /// Paragraph properties in specification order: style chain -> PAPX ->
    /// the `Prm` of piece `piece`, whose bytes are charged before it is
    /// applied.
    fn effective_pap(&self, fc: u32, piece: u32) -> Result<EffectivePap, ConvertError> {
        let (istd, papx_delta) =
            self.papx.lookup(fc).map_or((0, PapDelta::default()), |p| (p.istd, p.pap.clone()));
        let style = self.stylesheet.get(istd);
        let mut effective = style.pap.clone().merge(papx_delta);
        if let Some(prm) = self.text.prm(piece).grpprl() {
            self.parse_ctx.charge(prm.len() as u64)?;
            let mut prm_delta = PapDelta::default();
            apply_pap_sprms(self.parse_ctx, prm, &[], &mut prm_delta)?;
            effective = effective.merge(prm_delta);
        }
        Ok(EffectivePap { istd, effective })
    }

    /// The pieces whose undecodable text shows in the destination of the link
    /// `field` makes: an external or relative target the renderer keeps,
    /// replayed from the field's instruction CPs in `story` to find the
    /// damage inside the spans the target is read from.
    fn target_damage(
        &self,
        story: &Range<usize>,
        field: &OpenField,
    ) -> Result<Pieces, ConvertError> {
        let instr = &field.frame.instr;
        let Some((LinkTarget::External(url) | LinkTarget::Relative(url), spans)) =
            hyperlink_target_spans(instr)
        else {
            return Ok(Pieces::new());
        };
        let damaged = spans.iter().any(|span| {
            instr.get(span.clone()).is_some_and(|text| text.contains(char::REPLACEMENT_CHARACTER))
        });
        if !damaged || !is_safe_link(&url) || inlines_are_empty(&field.frame.inlines) {
            return Ok(Pieces::new());
        }
        self.instruction_damage(story, &field.instr, &spans)
    }

    /// Replay the instruction text at the CPs `instr` of `story` as the walk
    /// built it, collecting the pieces of the undecodable characters that
    /// land inside `spans`.
    fn instruction_damage(
        &self,
        story: &Range<usize>,
        instr: &Range<usize>,
        spans: &TargetSpans,
    ) -> Result<Pieces, ConvertError> {
        let mut damage = Pieces::new();
        let mut cursor = self.text.cursor(self.parse_ctx, story, instr)?;
        let mut segment = Segment::default();
        let (mut depth, mut offset) = (0usize, 0usize);
        while let Some(event) = cursor.next(self.parse_ctx)? {
            let Event::Scalar(s) = event else {
                break;
            };
            if s.cp >= segment.end {
                self.parse_ctx.charge(1)?;
                segment = self.segment(&s, None)?;
            }
            if self.note_refs.contains_key(&s.cp) {
                continue;
            }
            match s.c {
                '\u{13}' => depth += 1,
                '\u{15}' => depth = depth.saturating_sub(1),
                _ if depth > 0 || Dropped::of(segment.chp).is_some() => {}
                c => {
                    let Some(shown) = shown_char(c) else {
                        continue;
                    };
                    if s.replaced && spans.iter().any(|span| span.contains(&offset)) {
                        self.parse_ctx.charge(1)?;
                        damage.insert(s.piece);
                    }
                    offset += shown.len_utf8();
                }
            }
        }
        Ok(damage)
    }

    fn emit_paragraph(
        &self,
        pap: &EffectivePap,
        inlines: Vec<Inline>,
        flow: &mut Flow,
        region: Region,
    ) {
        let style = self.stylesheet.get(pap.istd);
        // A styled container absorbs its blank paragraphs: they are the
        // blank lines of a code block.
        if let Some(block) = style.block {
            flush_list(&mut flow.blocks, &mut flow.list);
            flow.styled.push(block, inlines, &mut flow.blocks);
            return;
        }
        if inlines_are_empty(&inlines) {
            flow.flush_runs();
            return;
        }
        let heading = style.heading.or(pap.effective.outline.level());
        if let Some(level) = heading {
            flow.flush_runs();
            let mut content = inlines;
            rebase_emphasis(&mut content, style.chp.style);
            // A numbered heading advances its sequence and keeps its number
            // visible - headings have no native numbering in the output.
            if let Some(label) = self.heading_label(pap, region) {
                content.insert(0, Inline::Text { text: label, style: Style::PLAIN });
            }
            flow.blocks.push(Block::Heading { level, anchor: None, content });
            return;
        }
        if let Some((ilfo, ilvl)) = pap.effective.numbering() {
            let list = self
                .lists
                .get(&ilfo)
                .map_or_else(|| Cow::Owned(ListDef::unknown(ilfo)), Cow::Borrowed);
            if let Some(marker) = list.level(ilvl).marker {
                let (number, label) = if marker.ordered() {
                    self.number(ilfo, &list, ilvl, region)
                } else {
                    (0, None)
                };
                flow.styled.flush(&mut flow.blocks);
                flow.list.push(ListEntry {
                    mark: ListMark {
                        key: ListKey { instance: list.id.instance(), marker },
                        level: ilvl,
                        number,
                        label,
                    },
                    blocks: vec![Block::Paragraph(inlines)],
                });
                return;
            }
        }
        flow.flush_runs();
        flow.blocks.push(Block::Paragraph(inlines));
    }

    /// Report in `region` once each damaged text whose units do not reach the
    /// output intact: `invalid_encoding` for undecodable units,
    /// `truncated_records` for a piece the stream does not hold, and
    /// `formatting_unreadable` for a font that does not resolve.
    fn report(&self, damaged: impl IntoIterator<Item = Damaged>, region: Region) {
        let mut reported = self.reported.borrow_mut();
        for damaged in damaged {
            if reported.insert((damaged, region)) {
                let code = match damaged {
                    Damaged::Piece(_) | Damaged::Level(_) => DiagnosticCode::InvalidEncoding,
                    Damaged::Missing(_) => DiagnosticCode::TruncatedRecords,
                    Damaged::Font(_) => DiagnosticCode::FormattingUnreadable,
                };
                self.parse_ctx.emit(code, region);
            }
        }
    }

    fn picture_at(
        &self,
        fc: u32,
        region: Region,
        stand_in: bool,
    ) -> Result<Option<Inline>, ConvertError> {
        let image = |source| {
            Some(Inline::Image {
                origin: self.parse_ctx.image_origin(region),
                alt: String::new(),
                source,
            })
        };
        let Some(offset) = self.chpx.lookup(fc).and_then(|p| sprm::chpx_pic_location(&p.chpx))
        else {
            return Ok(image(ImageSource::Unread));
        };
        let offset = offset as usize;
        let picf = self.data.get(offset..).and_then(|from| {
            let mut header = ByteReader::new(from);
            let lcb = usize::try_from(header.u32()?).ok()?;
            let cb_header = usize::from(header.u16()?);
            Some(from.get(..lcb)?.get(cb_header..).unwrap_or_default())
        });
        let max_bytes = self.parse_ctx.limits().max_entry_bytes;
        let blip = picf.map(|art| officeart::first_blip(art, max_bytes)).transpose()?.flatten();
        let Some(blip) = blip else {
            if stand_in && !self.names_a_picture(picf)? {
                return Ok(None);
            }
            return Ok(image(ImageSource::Unread));
        };
        let part = format!("data/pic{offset}.{}", blip.extension);
        let id = self.assets.borrow_mut().add(blip.media_type.to_string(), part, &blip.bytes)?;
        Ok(image(ImageSource::Asset(id)))
    }

    /// Whether a `SHAPE` field's stand-in that yields no picture stood for
    /// one: data that does not read, or a shape with a `pib`, or no shape at
    /// all. Any other shape is reported at the field's floating anchor.
    #[expect(
        clippy::disallowed_methods,
        reason = "`any` over the shape kinds answers the same in every order"
    )]
    fn names_a_picture(&self, art: Option<&[u8]>) -> Result<bool, ConvertError> {
        let Some(art) = art else {
            return Ok(true);
        };
        let kinds = shape_kinds(self.parse_ctx, art)?;
        Ok(kinds.is_empty() || kinds.values().any(|&kind| kind == ShapeKind::Picture))
    }

    /// Advance the sequence of an ordered level whose label a paragraph in
    /// `region` shows, reporting the level's damaged number text there.
    fn number(
        &self,
        ilfo: u16,
        list: &ListDef,
        ilvl: usize,
        region: Region,
    ) -> (u64, Option<String>) {
        self.report(list.level(ilvl).damaged_at.map(Damaged::Level), region);
        self.counters.borrow_mut().next(ilfo, list, ilvl)
    }

    /// The visible number label of a numbered heading paragraph, with its
    /// trailing separator; advances the list sequence.
    fn heading_label(&self, pap: &EffectivePap, region: Region) -> Option<String> {
        let (ilfo, ilvl) = pap.effective.numbering()?;
        let list = self.lists.get(&ilfo)?;
        let marker = list.level(ilvl).marker.filter(|marker| marker.ordered())?;
        let (number, label) = self.number(ilfo, list, ilvl, region);
        Some(format!("{} ", label.unwrap_or_else(|| marker.label(number))))
    }
}

/// The container of the paragraph the mark `c` with properties `pap` ends.
fn holder(c: char, pap: &PapDelta) -> Holder {
    if pap.ttp.unwrap_or(false) {
        Holder::Row
    } else if pap.in_table.unwrap_or(false) || c == '\u{7}' {
        Holder::Cell(pap.itap.unwrap_or(1))
    } else {
        Holder::Body
    }
}

#[derive(Default)]
struct Flow {
    blocks: Vec<Block>,
    list: Vec<ListEntry>,
    styled: StyledRun,
    cell: Vec<Block>,
    cell_styled: StyledRun,
    row: Vec<Vec<Block>>,
    rows: Vec<DocRow>,
}

impl Flow {
    fn flush_runs(&mut self) {
        self.styled.flush(&mut self.blocks);
        flush_list(&mut self.blocks, &mut self.list);
    }

    fn cell_paragraph(&mut self, block: Option<BlockStyle>, inlines: Vec<Inline>) {
        if let Some(block) = block {
            self.cell_styled.push(block, inlines, &mut self.cell);
        } else {
            self.cell_styled.flush(&mut self.cell);
            if !inlines_are_empty(&inlines) {
                self.cell.push(Block::Paragraph(inlines));
            }
        }
    }

    /// The blocks once the last paragraph, `inlines`, ends the story.
    fn finish(
        mut self,
        parse_ctx: &ParseCtx<'_>,
        inlines: Vec<Inline>,
    ) -> Result<Vec<Block>, ConvertError> {
        self.cell_styled.flush(&mut self.cell);
        self.flush_table(parse_ctx)?;
        self.flush_runs();
        if !inlines_are_empty(&inlines) {
            self.blocks.push(Block::Paragraph(inlines));
        }
        Ok(self.blocks)
    }

    fn flush_table(&mut self, parse_ctx: &ParseCtx<'_>) -> Result<(), ConvertError> {
        if !self.cell.is_empty() {
            self.row.push(std::mem::take(&mut self.cell));
        }
        if !self.row.is_empty() {
            self.rows.push(DocRow { cells: std::mem::take(&mut self.row), tap: None });
        }
        if self.rows.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.rows).into_iter().map(DocRow::into_grid_row).collect();
        if let Some(block) = build_edge_table(parse_ctx, rows)? {
            self.blocks.push(block);
        }
        Ok(())
    }
}

/// One accumulated table row: cell contents plus the TAP from the row-end
/// mark (absent for rows without `sprmTDefTable`).
struct DocRow {
    cells: Vec<Vec<Block>>,
    tap: Option<Tap>,
}

impl DocRow {
    fn into_grid_row(self) -> GridRow {
        let tap = self.tap.unwrap_or_default();
        let header = tap.header;
        let cells = self
            .cells
            .into_iter()
            .enumerate()
            .map(|(k, blocks)| {
                let tc = tap.cells.get(k).copied().unwrap_or_default();
                // Without a TAP boundary, a synthetic index-based edge keeps
                // the flat column behavior.
                let right = tap.boundaries.get(k + 1).map_or_else(
                    || i64::try_from(k + 1).unwrap_or(i64::MAX).saturating_mul(1000),
                    |&b| i64::from(b),
                );
                let prop = CellProp {
                    merge_first: tc.horz_first,
                    merge_cont: tc.horz_cont,
                    vmerge_first: tc.vert_restart,
                    vmerge_cont: tc.vert_cont,
                    right,
                };
                (blocks, prop)
            })
            .collect();
        GridRow { cells, header }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::doc::fkp::RunProps;
    use crate::formats::doc::pieces::{Coverage, PieceTable};
    use crate::formats::doc::tests::{PIC_LOCATION_CHPX, picture_data};
    use crate::ledger::CancelToken;
    use crate::limits::Limits;
    use crate::shared::test_support::cancelled;
    use std::assert_matches;

    #[test]
    fn a_row_takes_its_cell_edges_from_its_boundaries_else_from_the_cell_index() {
        let rights = |tap: Option<Tap>| -> Vec<i64> {
            let row = DocRow { cells: vec![Vec::new(); 3], tap }.into_grid_row();
            row.cells.iter().map(|(_, prop)| prop.right).collect()
        };
        assert_eq!(rights(None), [1000, 2000, 3000]);
        let tap = Tap { boundaries: vec![0, 700, -5], ..Tap::default() };
        assert_eq!(rights(Some(tap)), [700, -5, 3000]);
    }

    /// A Clx of the `prcs` and compressed pieces `(cp_end, fc, prm)`.
    fn clx(prcs: &[&[u8]], pieces: &[(u32, u32, u16)]) -> Vec<u8> {
        let cps: Vec<u32> = [0].into_iter().chain(pieces.iter().map(|p| p.0)).collect();
        let pcds: Vec<(u32, u16)> =
            pieces.iter().map(|&(_, fc, prm)| ((fc * 2) | 0x4000_0000, prm)).collect();
        crate::formats::doc::tests::clx(prcs, &cps, &pcds)
    }

    /// The text of `word_doc` through the piece table `table`, whose last CP is `terminal_cp`.
    fn view<'w>(word_doc: &'w [u8], table: &'w [u8], terminal_cp: usize) -> TextView<'w> {
        let coverage = Coverage { terminal_cp, cb_mac: word_doc.len() };
        let pieces = PieceTable::parse(&ParseCtx::default(), table, 0, table.len(), coverage);
        TextView::new(word_doc, pieces.unwrap())
    }

    const ONE_PIECE: &[(u32, u32, u16)] = &[(1, 0, 0)];

    fn assembler<'c, 'w>(
        parse_ctx: &'c ParseCtx<'c>,
        text: TextView<'w>,
        runs: Vec<Run>,
        data: Vec<u8>,
    ) -> Assembler<'c, 'w> {
        Assembler::new(
            parse_ctx,
            text,
            runs,
            Vec::new(),
            Stylesheet::default(),
            FontTable::default(),
            Lists::default(),
            HashMap::new(),
            HashMap::new(),
            data,
        )
    }

    fn inline_picture_bytes(limits: &Limits, inflated: usize) -> Result<usize, ConvertError> {
        let parse_ctx = ParseCtx::new(limits);
        let runs = vec![run(0, 2, PIC_LOCATION_CHPX)];
        let table = clx(&[], ONE_PIECE);
        let assembler = assembler(&parse_ctx, view(b"x", &table, 1), runs, picture_data(inflated));
        assert!(assembler.picture_at(0, Region::Body, false)?.is_some());
        Ok(assembler.assets.borrow().assets[0].bytes.len())
    }

    #[test]
    fn an_inline_picture_inflating_past_the_contexts_entry_bytes_is_a_limit() {
        let limits = Limits { max_entry_bytes: 4096, ..Default::default() };
        assert_eq!(inline_picture_bytes(&limits, 4096).unwrap(), 4096);
        let limits = Limits { max_entry_bytes: 4095, ..Default::default() };
        let err = inline_picture_bytes(&limits, 4096).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::EntryBytes, max: 4095 });
    }

    fn flush_spanning_table(max_expansion: u64) -> Result<Vec<Block>, ConvertError> {
        let limits = Limits { max_expansion, ..Default::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let row = |boundaries: Vec<i16>| {
            let cells =
                vec![vec![Block::Paragraph(vec![Inline::plain("x")])]; boundaries.len() - 1];
            let tap = Tap { boundaries, ..Tap::default() };
            DocRow { cells, tap: Some(tap) }
        };
        let rows = vec![row(vec![0, 1000, 2000, 3000]), row(vec![0, 3000])];
        let mut flow = Flow { rows, ..Flow::default() };
        flow.flush_table(&parse_ctx)?;
        Ok(flow.blocks)
    }

    #[test]
    fn a_table_cell_spanning_past_the_contexts_expansion_is_a_limit() {
        assert_eq!(flush_spanning_table(2).unwrap().len(), 1);
        let err = flush_spanning_table(1).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Expansion, max: 1 });
    }

    #[test]
    fn a_segment_ends_at_the_first_run_paragraph_or_piece_boundary() {
        let parse_ctx = ParseCtx::default();
        let run = |fc_start, fc_end| Run { fc_start, fc_end, props: RunProps::default() };
        let table = clx(&[], &[(10, 0, 0), (12, 4, 0)]);
        let mut text = assembler(
            &parse_ctx,
            view(b"0123456789", &table, 12),
            vec![run(0, 4), run(3, 6), run(8, 12)],
            Vec::new(),
        );
        text.papx = Runs::new(vec![run(0, 7), run(7, 12)]);
        let mut cursor = text.text.cursor(&parse_ctx, &(0..12), &(0..12)).unwrap();
        let mut ends = Vec::new();
        while let Some(Event::Scalar(s)) = cursor.next(&parse_ctx).unwrap() {
            if ends.last().is_none_or(|&end| s.cp >= end) {
                ends.push(text.segment(&s, None).unwrap().end);
            }
        }
        assert_eq!(ends, [3, 6, 7, 8, 10, 12]);
    }

    fn prm_table(len: u32) -> Vec<u8> {
        clx(&[&[0x16, 0x24, 0].repeat(10)], &[(len, 0, 1)])
    }

    fn run(fc_start: u32, fc_end: u32, chpx: &[u8]) -> Run {
        Run { fc_start, fc_end, props: RunProps { chpx: chpx.to_vec(), ..RunProps::default() } }
    }

    #[test]
    fn every_replay_of_a_piece_modifier_is_charged_before_it_applies_and_a_refusal_propagates() {
        fn text<'c, 'w>(parse_ctx: &'c ParseCtx<'c>, table: &'w [u8]) -> Assembler<'c, 'w> {
            let runs = vec![run(0, 4, &[0x35, 0x08, 1])];
            assembler(parse_ctx, view(b"a\rb\r", table, 4), runs, Vec::new())
        }
        let table = prm_table(4);
        let scalar = |cp: usize| Scalar {
            c: '\r',
            cp,
            fc: u32::try_from(cp).unwrap(),
            piece: 0,
            replaced: false,
        };
        type Replay = fn(&Assembler<'_, '_>, Scalar) -> Result<(), ConvertError>;
        let replays: [(&str, u64, Replay); 3] = [
            ("a segment", 3 + 30, |text, s| text.segment(&s, None).map(drop)),
            ("a paragraph", 30, |text, s| text.effective_pap(s.fc, 0).map(drop)),
            ("a dropped mark's join and the units it reads ahead", 60 + 2, |text, s| {
                let ctx = ParseCtx::default();
                let mut cursor = text.text.cursor(&ctx, &(0..4), &(0..4))?;
                cursor.next(&ctx)?;
                cursor.next(&ctx)?;
                text.joins_next(&cursor, &s).map(|joins| assert!(joins))
            }),
        ];
        for (case, cost, replay) in replays {
            let parse_ctx = ParseCtx::default();
            replay(&text(&parse_ctx, &table), scalar(1)).unwrap();
            assert_eq!(parse_ctx.fuel(), cost, "{case}");
            let limits = Limits { max_work: cost - 1, ..Limits::DEFAULT };
            let limited = ParseCtx::new(&limits);
            let refused = replay(&text(&limited, &table), scalar(1));
            assert_eq!(refused, Err(ConvertError::limit(Limit::Work, cost - 1)), "{case}");
            let token = CancelToken::new();
            let cancelled = cancelled(&token);
            assert_eq!(
                replay(&text(&cancelled, &table), scalar(1)),
                Err(ConvertError::Cancelled),
                "{case}"
            );
        }
    }

    #[test]
    fn a_join_charges_only_the_units_it_reads_up_to_the_next_mark() {
        let word_doc = [b"a\rb\r".to_vec(), vec![b'x'; 1 << 17]].concat();
        let len = word_doc.len();
        let table = prm_table(u32::try_from(len).unwrap());
        let parse_ctx = ParseCtx::default();
        let runs = vec![run(0, 4, &[0x35, 0x08, 1])];
        let text = assembler(&parse_ctx, view(&word_doc, &table, len), runs, Vec::new());
        let walk = ParseCtx::default();
        let mut cursor = text.text.cursor(&walk, &(0..len), &(0..len)).unwrap();
        cursor.next(&walk).unwrap();
        cursor.next(&walk).unwrap();
        let mark = Scalar { c: '\r', cp: 1, fc: 1, piece: 0, replaced: false };
        assert!(text.joins_next(&cursor, &mark).unwrap());
        assert_eq!(parse_ctx.fuel(), 60 + 2, "two Prm replays and the units up to the next mark");
    }

    #[test]
    fn a_target_replay_charges_its_seek_units_segment_and_each_damaged_piece() {
        let instr = " HYPERLINK \"http://x/~\" ";
        let units: Vec<u16> = instr
            .encode_utf16()
            .map(|unit| if unit == u16::from(b'~') { 0xDC00 } else { unit })
            .collect();
        let word_doc: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        let len = units.len();
        let plc = [[0, u32::try_from(len).unwrap()].map(u32::to_le_bytes).concat(), vec![0; 8]];
        let plc = plc.concat();
        let table = [vec![2], u32::try_from(plc.len()).unwrap().to_le_bytes().to_vec(), plc];
        let table = table.concat();
        let (_, spans) = hyperlink_target_spans(&instr.replace('~', "\u{FFFD}")).unwrap();
        let replay = |parse_ctx: &ParseCtx<'_>| {
            let text = assembler(parse_ctx, view(&word_doc, &table, len), Vec::new(), Vec::new());
            text.instruction_damage(&(0..len), &(0..len), &spans)
        };
        let parse_ctx = ParseCtx::default();
        assert_eq!(replay(&parse_ctx), Ok(Pieces::from([0])));
        let cost = 1 + len as u64 + 1 + 1;
        assert_eq!(parse_ctx.fuel(), cost, "the seek, the units, one segment and one piece");
        let limits = Limits { max_work: cost - 1, ..Limits::DEFAULT };
        assert_eq!(
            replay(&ParseCtx::new(&limits)),
            Err(ConvertError::limit(Limit::Work, cost - 1))
        );
        let token = CancelToken::new();
        let cancelled = cancelled(&token);
        assert_eq!(replay(&cancelled), Err(ConvertError::Cancelled));
    }

    #[test]
    fn a_join_refused_at_the_next_paragraph_stops_the_walk_there() {
        let (deleted, prm) = ([0x00, 0x08, 1], 30);
        let (preflight, seek, units) = (2, 1, 2);
        let segment = 1 + deleted.len() as u64 + prm;
        let spent = preflight + seek + units + segment + prm + 1 + prm;
        let limits = Limits { max_work: spent - 1, ..Limits::DEFAULT };
        let parse_ctx = ParseCtx::new(&limits);
        let runs = vec![run(0, 1, &deleted), run(1, 2, &[])];
        let table = prm_table(2);
        let text = assembler(&parse_ctx, view(b"\r\r", &table, 2), runs, Vec::new());
        let refused = text.build_blocks(&(0..2), &(0..2), Region::Body);
        assert_eq!(refused.map(drop), Err(ConvertError::limit(Limit::Work, spent - 1)));
        assert_eq!(parse_ctx.fuel(), spent);
    }

    #[test]
    fn an_unreadable_picture_keeps_its_place_with_or_without_a_location() {
        for runs in [Vec::new(), vec![run(0, 1, PIC_LOCATION_CHPX)]] {
            let ctx = ParseCtx::default();
            let table = clx(&[], ONE_PIECE);
            let assembler = assembler(&ctx, view(b"x", &table, 1), runs, Vec::new());
            let picture = assembler.picture_at(0, Region::Body, false).unwrap().unwrap();
            let document = crate::model::Document {
                blocks: vec![Block::Paragraph(vec![
                    Inline::plain("before"),
                    picture,
                    Inline::plain("after"),
                ])],
                ..crate::model::Document::default()
            };
            assert_eq!(
                crate::render::markdown::document_to_markdown(&document),
                "before\\[Image\\]after\n"
            );
            assert_eq!(
                crate::converter::diagnostics_of(ctx, &document, crate::ImagePolicy::Placeholder)
                    .unwrap()
                    .iter()
                    .map(|d| (d.code, d.count))
                    .collect::<Vec<_>>(),
                [(DiagnosticCode::ImageUnavailable, 1)]
            );
        }
    }

    fn plain(text: &str) -> Vec<Inline> {
        vec![Inline::plain(text)]
    }

    fn mark_flow(c: char, effective: PapDelta, mut flow: Flow) -> Result<Flow, ConvertError> {
        let parse_ctx = ParseCtx::default();
        let table = clx(&[], ONE_PIECE);
        let text = assembler(&parse_ctx, view(b"x", &table, 1), Vec::new(), Vec::new());
        let pap = EffectivePap { istd: 0, effective };
        text.end_paragraph(c, pap, plain("x"), &mut flow, Region::Body)?;
        Ok(flow)
    }

    fn in_table() -> PapDelta {
        PapDelta { in_table: Some(true), ..PapDelta::default() }
    }

    fn row_mark() -> PapDelta {
        PapDelta { ttp: Some(true), ..in_table() }
    }

    fn no_seed(_: &mut Flow) {}

    fn open_row(flow: &mut Flow) {
        flow.row.push(vec![Block::Paragraph(plain("r"))]);
    }

    fn open_quote(flow: &mut Flow) {
        flow.styled.push(BlockStyle::Quote, plain("q"), &mut flow.blocks);
    }

    fn open_cell_quote(flow: &mut Flow) {
        flow.row.push(Vec::new());
        flow.cell_styled.push(BlockStyle::Quote, plain("q"), &mut flow.cell);
    }

    /// The mark, its paragraph's properties, the flow it finds, and the flow's
    /// block, cell, row, table-row and header-row counts.
    type MarkCase = (&'static str, char, PapDelta, fn(&mut Flow), [usize; 5]);

    fn mark_cases() -> [MarkCase; 9] {
        let nested_cell = PapDelta { inner_cell: Some(true), ..in_table() };
        let nested_row = PapDelta { inner_ttp: Some(true), ..row_mark() };
        let header = Tap { header: true, ..Tap::default() };
        let header_row = PapDelta { tap: Some(header), ..row_mark() };
        let body = PapDelta::default();
        [
            ("a paragraph joins the cell", '\r', in_table(), no_seed, [0, 1, 0, 0, 0]),
            ("a cell mark joins the row", '\u{7}', in_table(), no_seed, [0, 0, 1, 0, 0]),
            (
                "a row mark ends the row with its TAP",
                '\u{7}',
                header_row,
                open_row,
                [0, 0, 0, 1, 1],
            ),
            ("a row mark over no cells", '\u{7}', row_mark(), no_seed, [0, 0, 0, 0, 0]),
            (
                "a nested cell mark stays in the cell",
                '\u{7}',
                nested_cell,
                no_seed,
                [0, 1, 0, 0, 0],
            ),
            ("a nested row mark stays in the cell", '\u{7}', nested_row, open_row, [0, 1, 1, 0, 0]),
            ("table content ends a styled run", '\u{7}', in_table(), open_quote, [1, 0, 1, 0, 0]),
            (
                "a row mark ends its cell's run",
                '\u{7}',
                row_mark(),
                open_cell_quote,
                [0, 1, 0, 1, 0],
            ),
            ("a body paragraph ends the table", '\r', body, open_row, [2, 0, 0, 0, 0]),
        ]
    }

    #[test]
    fn a_mark_ends_a_cell_a_row_or_a_body_paragraph_as_its_properties_say() {
        for (case, c, effective, seed, expected) in mark_cases() {
            let mut flow = Flow::default();
            seed(&mut flow);
            let flow = mark_flow(c, effective, flow).unwrap();
            let headers = flow.rows.iter().filter(|r| r.tap.as_ref().is_some_and(|t| t.header));
            let counts = [
                flow.blocks.len(),
                flow.cell.len(),
                flow.row.len(),
                flow.rows.len(),
                headers.count(),
            ];
            assert_eq!(counts, expected, "{case}");
        }
    }

    #[test]
    fn a_cell_paragraph_closes_the_styled_run_before_it_and_the_end_closes_the_cell() {
        let mut flow = Flow::default();
        flow.cell_paragraph(Some(BlockStyle::Quote), plain("a"));
        flow.cell_paragraph(None, plain("b"));
        assert_matches!(flow.cell.as_slice(), [Block::BlockQuote(_), Block::Paragraph(_)]);
        let mut flow = Flow::default();
        flow.cell_paragraph(Some(BlockStyle::Quote), plain("a"));
        let blocks = flow.finish(&ParseCtx::default(), Vec::new()).unwrap();
        assert_matches!(blocks.as_slice(), [Block::Table(_)]);
    }

    #[test]
    fn a_list_item_closes_the_open_styled_run_before_it_joins_the_list() {
        let parse_ctx = ParseCtx::default();
        let table = clx(&[], ONE_PIECE);
        let text = assembler(&parse_ctx, view(b"x", &table, 1), Vec::new(), Vec::new());
        let mut flow = Flow::default();
        flow.styled.push(BlockStyle::Quote, plain("q"), &mut flow.blocks);
        let effective = PapDelta { ilfo: Some(1), ..PapDelta::default() };
        text.emit_paragraph(
            &EffectivePap { istd: 0, effective },
            plain("x"),
            &mut flow,
            Region::Body,
        );
        assert_matches!(flow.blocks.as_slice(), [Block::BlockQuote(_)]);
        assert_eq!(flow.list.len(), 1);
    }

    #[test]
    fn a_styled_paragraph_closes_the_open_list_before_it_joins_a_run() {
        const QUOTE: u16 = 3;
        let parse_ctx = ParseCtx::default();
        let table = clx(&[], ONE_PIECE);
        let mut text = assembler(&parse_ctx, view(b"x", &table, 1), Vec::new(), Vec::new());
        text.stylesheet = Stylesheet::with_block(QUOTE, BlockStyle::Quote);
        let mut flow = Flow::default();
        let listed = PapDelta { ilfo: Some(1), ..PapDelta::default() };
        for (istd, effective) in [(0, listed), (QUOTE, PapDelta::default())] {
            let pap = EffectivePap { istd, effective };
            text.emit_paragraph(&pap, plain("x"), &mut flow, Region::Body);
        }
        assert_matches!(flow.blocks.as_slice(), [Block::List(_)]);
        assert_eq!(flow.list.len(), 0);
        flow.styled.flush(&mut flow.blocks);
        assert_matches!(flow.blocks.as_slice(), [Block::List(_), Block::BlockQuote(_)]);
    }

    #[test]
    fn a_row_is_a_header_row_when_its_tap_says_so() {
        let header = |tap: Option<Tap>| DocRow { cells: Vec::new(), tap }.into_grid_row().header;
        let flagged = Tap { header: true, ..Tap::default() };
        assert_eq!(
            [header(None), header(Some(Tap::default())), header(Some(flagged))],
            [false, false, true]
        );
    }

    #[test]
    fn a_paragraph_takes_the_style_of_its_papx_run_else_style_zero() {
        let parse_ctx = ParseCtx::default();
        let table = clx(&[], ONE_PIECE);
        let mut text = assembler(&parse_ctx, view(b"x", &table, 1), Vec::new(), Vec::new());
        let props = RunProps { istd: 7, ..RunProps::default() };
        text.papx = Runs::new(vec![Run { fc_start: 1, fc_end: 2, props }]);
        let istd = |fc| text.effective_pap(fc, 0).unwrap().istd;
        assert_eq!([istd(0), istd(1)], [0, 7]);
    }

    #[test]
    fn an_object_character_is_an_embedded_object_then_field_data_else_a_picture() {
        let flag = |sprm: u16| [&sprm.to_le_bytes()[..], &[1]].concat();
        let cases = [
            (
                "an embedded object",
                flag(SPRM_CF_OLE2),
                0,
                vec![(DiagnosticCode::EmbeddedObject, 1)],
            ),
            ("field data", flag(SPRM_CF_DATA), 0, Vec::new()),
            (
                "an object that is also field data",
                [flag(SPRM_CF_OLE2), flag(SPRM_CF_DATA)].concat(),
                0,
                vec![(DiagnosticCode::EmbeddedObject, 1)],
            ),
            ("a picture", Vec::new(), 1, Vec::new()),
        ];
        for (case, chpx, pushed, expected) in cases {
            let parse_ctx = ParseCtx::default();
            let table = clx(&[], ONE_PIECE);
            let runs = vec![run(0, 1, &chpx)];
            let text = assembler(&parse_ctx, view(b"\x01", &table, 1), runs, Vec::new());
            let mut para = ParaBuilder::new(&parse_ctx);
            text.object_char(0, Region::Body, &mut para).unwrap();
            assert_eq!(para.inlines.len(), pushed, "{case}");
            drop(para);
            drop(text);
            let document = crate::model::Document::default();
            let diagnostics = crate::converter::diagnostics_of(
                parse_ctx,
                &document,
                crate::ImagePolicy::Placeholder,
            );
            let reported: Vec<_> = diagnostics.unwrap().iter().map(|d| (d.code, d.count)).collect();
            assert_eq!(reported, expected, "{case}");
        }
    }
}
