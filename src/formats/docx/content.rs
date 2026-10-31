//! Block and inline walking for WordprocessingML parts.

use super::numbering::{Counters, Numbering};
use super::styles::{FontRoute, RunFonts, Styles, Toggles, on_off, outline, rpr_delta, val};
use crate::ctx::{ImageLoss, ParseCtx};
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::{
    Block, Cell, GridBuilder, ImageSource, Inline, LinkTarget, Style, inlines_are_empty,
    inlines_to_plain_text,
};
use crate::package::Package;
use crate::package::relationships::{Relationships, TargetMode};
use crate::package::xml::{Attr, Element, Node, ns};
use crate::shared::assets::{AssetSink, rel_image_source, retain_ole_payload};
use crate::shared::blockstyle::{BlockStyle, Outline, StyledRun};
use crate::shared::delta::rebase_emphasis;
use crate::shared::drawingml::{self, GraphicPart};
use crate::shared::fields::{FieldFrame, classify_rel_target, field_result};
use crate::shared::header::resolve_header_rows;
use crate::shared::link_label::label_is_absolute_url;
use crate::shared::list::{ListEntry, ListKey, ListMark, flush_list};
use crate::shared::math::{omath_para_to_tex, omath_to_tex};
use crate::shared::mc::{self, is_picture_fill, is_text, is_vml_shape};
use crate::shared::symbol::{self, SymbolFont, Token};
use crate::shared::text::{clean_text, is_layout_invisible};
use std::cell::{Cell as Counter, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::iter::Peekable;
use std::ops::Bound;
use std::rc::Rc;

/// Namespaces whose markup this frontend understands; `mc:Choice` branches
/// requiring anything else fall back to `mc:Fallback`.
const SUPPORTED_NS: &[&str] = &[
    ns::W,
    ns::A,
    ns::PIC,
    ns::WP,
    ns::MC,
    ns::CHART,
    ns::DGM,
    ns::VML,
    ns::O_VML,
    ns::WPS,
    ns::WPG,
];

pub(super) struct Ctx<'a, 'b> {
    pub parse_ctx: &'b ParseCtx<'b>,
    pub pkg: &'b Package<'a>,
    pub rels: Relationships,
    pub base_part: String,
    pub region: Region,
    pub styles: &'b Styles<'b>,
    pub numbering: &'b Numbering,
    /// The numbering part's loss was reported, so a paragraph naming an
    /// instance it would define is not reported again.
    pub numbering_reported: bool,
    pub counters: &'b RefCell<Counters>,
    pub assets: &'b RefCell<AssetSink>,
    pub references: &'b References,
    pub moves: RefCell<Moves>,
}

/// What the walk of every part saw rendered: comment references, and the notes references
/// name, each by whether one of its references renders.
#[derive(Default)]
pub(super) struct References {
    notes: RefCell<BTreeMap<String, bool>>,
    comments: Counter<u64>,
}

impl References {
    /// Whether the note `id` names is read: unless every reference to it dropped.
    pub(super) fn reads_note(
        &self,
        parse_ctx: &ParseCtx<'_>,
        id: &str,
    ) -> Result<bool, ConvertError> {
        parse_ctx.charge(id.len() as u64)?;
        Ok(self.notes.borrow().get(id) != Some(&false))
    }

    /// Whether a reference to a note whose id starts with `prefix` dropped, and whether one
    /// rendered.
    pub(super) fn note_references(
        &self,
        parse_ctx: &ParseCtx<'_>,
        prefix: &str,
    ) -> Result<(bool, bool), ConvertError> {
        let (mut dropped, mut rendered) = (false, false);
        let from = (Bound::Included(prefix), Bound::Unbounded);
        for (id, renders) in self.notes.borrow().range::<str, _>(from) {
            if !id.starts_with(prefix) {
                break;
            }
            parse_ctx.charge(id.len() as u64)?;
            dropped |= !renders;
            rendered |= renders;
        }
        Ok((dropped, rendered))
    }

    pub(super) fn comments(&self) -> u64 {
        self.comments.get()
    }

    fn comment(&self) {
        self.comments.set(self.comments.get().saturating_add(1));
    }
}

/// A part's tracked moves: the move-source ranges open where the walk is, the destination
/// names it walked where they render, and the names of the source ranges that held content.
#[derive(Default)]
pub(super) struct Moves {
    open: Vec<(String, Rc<str>)>,
    destinations: BTreeSet<Rc<str>>,
    sources: Vec<Rc<str>>,
}

/// What this reader shows and reads of a branch: everything.
const READ_ALL: mc::Shown = (|_| true, |_| true);

impl Ctx<'_, '_> {
    /// The same document-wide dependencies scoped to another package part
    /// (notes parts): only the relationships, base path and region change.
    pub(super) fn for_part(&self, rels: Relationships, base_part: String, region: Region) -> Self {
        Self { rels, base_part, region, moves: RefCell::default(), ..*self }
    }

    pub(super) fn settle_moves(&self) -> Result<(), ConvertError> {
        let moves = self.moves.borrow();
        let mut orphans = 0u64;
        for name in &moves.sources {
            self.parse_ctx.charge(name.len() as u64)?;
            if !moves.destinations.contains(name) {
                orphans += 1;
            }
        }
        self.parse_ctx.emit_count(DiagnosticCode::RevisionOmitted, self.region, orphans);
        Ok(())
    }

    /// Tracks a move range's start or end; a destination counts only where it renders.
    fn move_range(&self, marker: &Element, renders: bool) -> Result<(), ConvertError> {
        let mut moves = self.moves.borrow_mut();
        let id = marker.attr(ns::W, "id").unwrap_or_default();
        match marker.local.as_str() {
            "moveFromRangeStart" => {
                if let Some(name) = marker.attr(ns::W, "name") {
                    self.parse_ctx.charge(id.len().saturating_add(name.len()) as u64)?;
                    moves.open.push((id.to_owned(), Rc::from(name)));
                }
            }
            "moveFromRangeEnd" => {
                for at in (0..moves.open.len()).rev() {
                    self.parse_ctx.charge(id.len() as u64 + 1)?;
                    if moves.open.get(at).is_some_and(|(open, _)| open == id) {
                        moves.open.remove(at);
                        break;
                    }
                }
            }
            _ => {
                if renders && let Some(name) = marker.attr(ns::W, "name") {
                    self.parse_ctx.charge(name.len() as u64)?;
                    moves.destinations.insert(Rc::from(name));
                }
            }
        }
        Ok(())
    }

    /// What a `w:moveFrom` here drops as: a move inside the innermost open named range,
    /// else a deletion.
    fn move_source(&self) -> Dropped {
        self.moves
            .borrow()
            .open
            .last()
            .map_or(Dropped::Revision, |(_, name)| Dropped::Moved(Rc::clone(name)))
    }

    fn note_reference(&self, id: &str, renders: bool) -> Result<(), ConvertError> {
        self.parse_ctx.charge(id.len() as u64)?;
        let mut notes = self.references.notes.borrow_mut();
        if let Some(rendered) = notes.get_mut(id) {
            *rendered |= renders;
        } else {
            notes.insert(id.to_owned(), renders);
        }
        Ok(())
    }

    fn graphic_part(&self) -> GraphicPart<'_, '_> {
        GraphicPart {
            pkg: self.pkg,
            rels: &self.rels,
            base_part: &self.base_part,
            region: self.region,
        }
    }

    fn emit(&self, code: DiagnosticCode) {
        self.parse_ctx.emit(code, self.region);
    }

    /// Report a loss whose cause is a relationship, unless the relationships
    /// part itself was reported.
    fn emit_related(&self, code: DiagnosticCode) {
        if !self.rels.reported() {
            self.emit(code);
        }
    }

    /// The branch of alternate content to walk, reporting what the declined choices hold as it
    /// would show where `holder` stands: without the runs hidden where they stand, `hides`
    /// telling whether runs outside a paragraph of their own hide, and a VML fill judged by
    /// the VML `shape` that holds `alt`.
    fn alternate_branch<'e>(
        &self,
        alt: &'e Element,
        shape: Option<&Element>,
        hides: bool,
        holder: BranchHolder,
    ) -> Result<Option<&'e Element>, ConvertError> {
        self.reported_branch(alt, shape, hides, holder)
    }

    /// As [`Self::alternate_branch`], inside an equation, where math text a declined choice
    /// would show is an unread block too unless the fallback shows text.
    fn math_branch<'e>(
        &self,
        alt: &'e Element,
        hides: bool,
    ) -> Result<Option<&'e Element>, ConvertError> {
        self.reported_branch(alt, None, hides, BranchHolder::Math)
    }

    fn reported_branch<'e>(
        &self,
        alt: &'e Element,
        shape: Option<&Element>,
        hides: bool,
        holder: BranchHolder,
    ) -> Result<Option<&'e Element>, ConvertError> {
        let branch = omitted_branch(alt);
        if branch.is_some_and(|b| b.is(ns::MC, "Choice")) {
            return Ok(branch);
        }
        let shown = without_hidden_text(self, alt, hides)?;
        if holder == BranchHolder::Instruction {
            let rendered = instruction_rendered(self.parse_ctx, shown)?;
            mc::alternate_branch(
                self.parse_ctx,
                self.region,
                &rendered,
                shape,
                SUPPORTED_NS,
                READ_ALL,
            );
            return Ok(branch);
        }
        mc::alternate_branch(self.parse_ctx, self.region, &shown, shape, SUPPORTED_NS, READ_ALL);
        let lost = |hit: fn(&Element) -> bool| {
            shown.find_all(ns::MC, "Choice").any(|c| c.descendant_elems().any(hit))
                && !shown.find(ns::MC, "Fallback").is_some_and(|f| f.descendant_elems().any(hit))
        };
        if holder == BranchHolder::Math && lost(|e| is_text(e) || is_math_text(e)) && !lost(is_text)
        {
            self.emit(DiagnosticCode::BlockUnread);
        }
        Ok(branch)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BranchHolder {
    Text,
    Instruction,
    Math,
}

/// `alt` with each branch holding only what renders from a field's instruction: its text
/// boxes, charts and diagrams.
fn instruction_rendered(
    parse_ctx: &ParseCtx<'_>,
    mut alt: Element,
) -> Result<Element, ConvertError> {
    for node in &mut alt.children {
        let Node::Elem(branch) = node else { continue };
        let mut open = std::mem::take(&mut branch.children);
        while let Some(node) = open.pop() {
            parse_ctx.charge(1)?;
            let Node::Elem(mut elem) = node else { continue };
            if elem.is(ns::W, "txbxContent") || mc::is_chart_or_diagram(&elem) {
                branch.children.push(Node::Elem(elem));
            } else {
                open.append(&mut elem.children);
            }
        }
    }
    Ok(alt)
}

fn is_math_text(elem: &Element) -> bool {
    elem.is(ns::M, "t") && !elem.text().trim().is_empty()
}

/// The branch of alternate content that drops whole, chosen as where it renders but reporting
/// nothing it declines.
fn omitted_branch(alt: &Element) -> Option<&Element> {
    mc::selected_branch(alt, SUPPORTED_NS)
}

/// `elem` without the runs hidden where they stand and without its deletions and moves away:
/// each paragraph judges its runs by its own style, and a style that cannot resolve leaves
/// them to what is around it.
fn without_hidden_text(
    ctx: &Ctx<'_, '_>,
    elem: &Element,
    hides: bool,
) -> Result<Element, ConvertError> {
    copy_with(ctx.parse_ctx, elem, |child| {
        if child.is(ns::W, "del") || child.is(ns::W, "moveFrom") {
            return Ok(None);
        }
        if (child.is(ns::W, "r") || child.is(ns::M, "r"))
            && hidden_beside_runs(ctx, child.find(ns::W, "rPr"), Style::PLAIN, hides)?
        {
            return Ok(None);
        }
        let hides = if child.is(ns::W, "p") {
            let pstyle_id = pstyle_id(child.find(ns::W, "pPr"));
            or_when_malformed(ctx.styles.paragraph_hidden(ctx.parse_ctx, pstyle_id), || hides)?
        } else {
            hides
        };
        without_hidden_text(ctx, child, hides).map(Some)
    })
}

/// `elem` copied under the charges of its text, attributes and name, its element children each
/// replaced by what `child` makes of it or dropped.
fn copy_with(
    parse_ctx: &ParseCtx<'_>,
    elem: &Element,
    mut child: impl FnMut(&Element) -> Result<Option<Element>, ConvertError>,
) -> Result<Element, ConvertError> {
    let mut children = Vec::with_capacity(elem.children.len());
    for node in &elem.children {
        match node {
            Node::Text(text) => children.push(Node::Text(copied(parse_ctx, text)?)),
            Node::Elem(inner) => children.extend(child(inner)?.map(Node::Elem)),
        }
    }
    let mut attrs = Vec::with_capacity(elem.attrs.len());
    for a in &elem.attrs {
        let local = copied(parse_ctx, &a.local)?;
        attrs.push(Attr { ns: a.ns.clone(), local, value: copied(parse_ctx, &a.value)? });
    }
    let local = copied(parse_ctx, &elem.local)?;
    Ok(Element {
        ns: elem.ns.clone(),
        local,
        attrs,
        children,
        unresolved_text: elem.unresolved_text,
    })
}

fn copied(parse_ctx: &ParseCtx<'_>, text: &str) -> Result<String, ConvertError> {
    parse_ctx.charge(text.len() as u64)?;
    Ok(text.to_string())
}

/// A style that cannot resolve leaves the value to what is around it.
fn or_when_malformed<T>(
    result: Result<T, ConvertError>,
    fallback: impl FnOnce() -> T,
) -> Result<T, ConvertError> {
    match result {
        Err(ConvertError::Malformed { .. }) => Ok(fallback()),
        other => other,
    }
}

enum ParaKind {
    Heading {
        level: u8,
        /// The visible number of a numbered heading (with its trailing
        /// separator), prepended to the content: headings have no native
        /// numbering in the output.
        label: Option<String>,
        /// The heading style's own emphasis, subtracted from its runs.
        base: Style,
    },
    ListItem(ListMark),
    /// A paragraph whose style names a block container.
    Styled(BlockStyle),
    Plain,
}

/// A paragraph's kind with its numbering not yet resolved: a paragraph whose mark drops
/// takes a number only if it is emitted as it stands, never when it joins the next one.
struct Pending<'e> {
    ppr: Option<&'e Element>,
    pstyle_id: Option<&'e str>,
    heading: Option<u8>,
    style_block: Option<BlockStyle>,
    base: Style,
}

impl Pending<'_> {
    fn kind(self, ctx: &Ctx<'_, '_>) -> Result<ParaKind, ConvertError> {
        // Numbering resolves independently of heading semantics: a numbered
        // heading advances its sequence and keeps its number visible.
        let numbering = resolve_numbering(self.ppr, self.pstyle_id, ctx)?;
        Ok(match self.heading {
            Some(level) => {
                let label = numbering.filter(|mark| mark.key.marker.ordered()).map(|mark| {
                    format!("{} ", mark.label.unwrap_or_else(|| mark.key.marker.label(mark.number)))
                });
                ParaKind::Heading { level, label, base: self.base }
            }
            None => numbering
                .map(ParaKind::ListItem)
                .or_else(|| self.style_block.map(ParaKind::Styled))
                .unwrap_or(ParaKind::Plain),
        })
    }
}

enum Mark<'e> {
    Kept(ParaKind),
    Dropped(Pending<'e>),
}

/// Block runs a following paragraph may extend: a list being built, and a
/// styled container. Only one is ever open, so starting either closes the
/// other. A paragraph whose mark drops waits to join the next one.
#[derive(Default)]
struct Runs<'e> {
    list: Vec<ListEntry>,
    styled: StyledRun,
    joining: Option<(Pending<'e>, Vec<Piece>)>,
}

impl Runs<'_> {
    fn flush(&mut self, blocks: &mut Vec<Block>) {
        self.styled.flush(blocks);
        flush_list(blocks, &mut self.list);
    }

    /// Emits a paragraph waiting to join as it stands, then closes the open runs: one that
    /// shows nothing renders nothing and takes no number.
    fn release(&mut self, ctx: &Ctx<'_, '_>, blocks: &mut Vec<Block>) -> Result<(), ConvertError> {
        if let Some((pending, pieces)) = self.joining.take()
            && !shows_nothing(&pieces)
        {
            emit_paragraph(pending.kind(ctx)?, pieces, blocks, self);
        }
        self.flush(blocks);
        Ok(())
    }
}

/// A paragraph's content in source order: inline runs interleaved with
/// block attachments (text boxes, charts) at their anchor positions.
enum Piece {
    Inlines(Vec<Inline>),
    Blocks(Vec<Block>),
}

pub(super) fn parse_blocks(
    parent: &Element,
    ctx: &Ctx<'_, '_>,
) -> Result<Vec<Block>, ConvertError> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut runs = Runs::default();
    collect_blocks(parent, ctx, &mut blocks, &mut runs)?;
    runs.release(ctx, &mut blocks)?;
    Ok(blocks)
}

fn collect_blocks<'e>(
    parent: &'e Element,
    ctx: &Ctx<'_, '_>,
    blocks: &mut Vec<Block>,
    runs: &mut Runs<'e>,
) -> Result<(), ConvertError> {
    for child in parent.child_elems() {
        if child.is(ns::MC, "AlternateContent") {
            if let Some(branch) = ctx.alternate_branch(child, None, false, BranchHolder::Text)? {
                collect_blocks(branch, ctx, blocks, runs)?;
            }
            continue;
        }
        if child.is(ns::M, "oMathPara") || child.is(ns::M, "oMath") {
            runs.release(ctx, blocks)?;
            let mut walker = InlineWalker::outside_paragraphs(ctx)?;
            walker.walk_math(child, true)?;
            walker.close_range();
            blocks.extend(split_pieces(walker.finish()).1);
            continue;
        }
        if child.ns.as_deref() != Some(ns::W) {
            continue;
        }
        match child.local.as_str() {
            "p" => {
                let (mark, mut pieces) = parse_paragraph(child, ctx)?;
                if let Some((_, held)) = runs.joining.take() {
                    pieces = joined(held, pieces);
                }
                match mark {
                    Mark::Dropped(pending) => runs.joining = Some((pending, pieces)),
                    Mark::Kept(kind) => emit_paragraph(kind, pieces, blocks, runs),
                }
            }
            "tbl" => {
                runs.release(ctx, blocks)?;
                blocks.extend(parse_table(child, ctx)?);
            }
            "sdt" => {
                if let Some(content) = child.find(ns::W, "sdtContent") {
                    collect_blocks(content, ctx, blocks, runs)?;
                }
            }
            "customXml" | "ins" | "moveTo" => collect_blocks(child, ctx, blocks, runs)?,
            "altChunk" => ctx.emit(DiagnosticCode::ImportedPartUnread),
            "del" | "moveFrom" => {
                let mut walker = InlineWalker::outside_paragraphs(ctx)?;
                walker.in_revision(child, |w| w.walk(child))?;
                walker.close_range();
            }
            "moveFromRangeStart" | "moveFromRangeEnd" | "moveToRangeStart" => {
                ctx.move_range(child, true)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Whether a paragraph's mark drops once revisions are accepted and hidden text omitted:
/// a deleted, moved-out, hidden or specially hidden mark, which joins its paragraph to the
/// next. A mark carrying its section's properties is a section break, which never joins.
fn mark_drops(
    ppr: Option<&Element>,
    ctx: &Ctx<'_, '_>,
    base: Style,
    hides: bool,
) -> Result<bool, ConvertError> {
    let rpr = ppr.and_then(|pr| pr.find(ns::W, "rPr"));
    if ppr.and_then(|pr| pr.find(ns::W, "sectPr")).is_some() {
        return Ok(false);
    }
    if rpr.is_some_and(|rpr| {
        rpr.find(ns::W, "del").is_some()
            || rpr.find(ns::W, "moveFrom").is_some()
            || on_off(rpr, "specVanish") == Some(true)
    }) {
        return Ok(true);
    }
    hidden_beside_runs(ctx, rpr, base, hides)
}

/// Whether a mark, a math run or a run in a declined choice is hidden: a character style that
/// cannot resolve leaves them to their direct properties rather than failing the conversion.
fn hidden_beside_runs(
    ctx: &Ctx<'_, '_>,
    rpr: Option<&Element>,
    base: Style,
    hides: bool,
) -> Result<bool, ConvertError> {
    or_when_malformed(run_properties(ctx, rpr, base, hides).map(|(_, hidden)| hidden), || {
        rpr.and_then(|rpr| on_off(rpr, "vanish")).unwrap_or(hides)
    })
}

fn shows_nothing(pieces: &[Piece]) -> bool {
    pieces.iter().all(|piece| match piece {
        Piece::Inlines(inlines) => {
            inlines_are_empty(inlines) && !inlines.iter().any(|i| matches!(i, Inline::Anchor(_)))
        }
        Piece::Blocks(_) => false,
    })
}

/// A run's style and whether it is hidden: its character style's toggles flip the
/// paragraph's values and its shift replaces the paragraph's, and its direct formatting is
/// absolute.
fn run_properties(
    ctx: &Ctx<'_, '_>,
    rpr: Option<&Element>,
    base: Style,
    hides: bool,
) -> Result<(Style, bool), ConvertError> {
    let Some(rpr) = rpr else {
        return Ok((base, hides));
    };
    let (parity, position) = match val(rpr, "rStyle") {
        Some(id) => (
            ctx.styles.run_toggles(ctx.parse_ctx, id)?,
            ctx.styles.run_position(ctx.parse_ctx, id)?,
        ),
        None => (Toggles::default(), None),
    };
    let delta = rpr_delta(rpr);
    let hidden = delta.hidden.unwrap_or_else(|| parity.hidden_over(hides));
    let styled = parity.apply_over(base);
    let styled = Style { position: position.unwrap_or(styled.position), ..styled };
    Ok((delta.apply(styled), hidden))
}

/// A paragraph whose mark drops followed by the next, which keeps its
/// own properties: the inline runs where they meet become one.
fn joined(mut first: Vec<Piece>, second: Vec<Piece>) -> Vec<Piece> {
    for piece in second {
        match (piece, first.last_mut()) {
            (Piece::Inlines(mut head), Some(Piece::Inlines(tail))) => tail.append(&mut head),
            (piece, _) => first.push(piece),
        }
    }
    first
}

fn emit_paragraph(
    kind: ParaKind,
    pieces: Vec<Piece>,
    blocks: &mut Vec<Block>,
    runs: &mut Runs<'_>,
) {
    match kind {
        ParaKind::ListItem(mark) => {
            runs.styled.flush(blocks);
            let item = pieces_into_blocks(pieces);
            runs.list.push(ListEntry { mark, blocks: item });
        }
        ParaKind::Styled(style) => {
            flush_list(blocks, &mut runs.list);
            for piece in pieces {
                match piece {
                    Piece::Inlines(inlines) => runs.styled.push(style, inlines, blocks),
                    Piece::Blocks(attachments) => {
                        runs.styled.flush(blocks);
                        blocks.extend(attachments);
                    }
                }
            }
        }
        ParaKind::Heading { level, label, base } => {
            runs.flush(blocks);
            let mut label = label;
            let mut emitted_heading = false;
            for piece in pieces {
                match piece {
                    Piece::Inlines(mut content) if !inlines_are_empty(&content) => {
                        rebase_emphasis(&mut content, base);
                        if emitted_heading {
                            // Attachments cannot be nested in a heading, so
                            // subsequent text becomes a paragraph.
                            blocks.push(Block::Paragraph(content));
                        } else {
                            if let Some(label) = label.take() {
                                content
                                    .insert(0, Inline::Text { text: label, style: Style::PLAIN });
                            }
                            blocks.push(Block::Heading { level, anchor: None, content });
                            emitted_heading = true;
                        }
                    }
                    Piece::Inlines(_) => {}
                    Piece::Blocks(attachments) => blocks.extend(attachments),
                }
            }
        }
        ParaKind::Plain => {
            runs.flush(blocks);
            blocks.extend(pieces_into_blocks(pieces));
        }
    }
}

fn pstyle_id(ppr: Option<&Element>) -> Option<&str> {
    val(ppr?, "pStyle")
}

fn parse_paragraph<'e>(
    p: &'e Element,
    ctx: &Ctx<'_, '_>,
) -> Result<(Mark<'e>, Vec<Piece>), ConvertError> {
    let ppr = p.find(ns::W, "pPr");
    let pstyle_id = pstyle_id(ppr);

    let (style_heading, style_block, parity) = match pstyle_id {
        Some(id) => (
            ctx.styles.heading_level(ctx.parse_ctx, id)?,
            ctx.styles.block_style(ctx.parse_ctx, id)?,
            ctx.styles.run_toggles(ctx.parse_ctx, id)?,
        ),
        None => (Outline::Inherit, None, Toggles::default()),
    };
    // Direct paragraph properties overlay the style chain: an explicit
    // outlineLvl of 9 ("no outline level") turns a style heading off.
    let heading = outline(ppr).or(style_heading).level();
    let paragraph_level = Style {
        position: ctx.styles.paragraph_position(ctx.parse_ctx, pstyle_id)?,
        ..parity.apply_over(ctx.styles.doc_defaults)
    };
    let hides = ctx.styles.paragraph_hidden(ctx.parse_ctx, pstyle_id)?;

    let pending = Pending { ppr, pstyle_id, heading, style_block, base: paragraph_level };
    let mark = if mark_drops(ppr, ctx, paragraph_level, hides)? {
        Mark::Dropped(pending)
    } else {
        Mark::Kept(pending.kind(ctx)?)
    };

    let fonts = ctx.styles.paragraph_fonts(ctx.parse_ctx, pstyle_id)?;
    let mut walker = InlineWalker::new(ctx, paragraph_level, hides, fonts);
    walker.walk(p)?;
    walker.close_range();
    Ok((mark, walker.finish()))
}

/// Resolve a paragraph's effective numbering per ECMA-376: the direct
/// `numPr` children are tri-state and merge property-by-property with the
/// style-inherited `numPr` (a missing `numId`/`ilvl` inherits; an explicit
/// `numId` of 0 suppresses), advancing the instance counters.
fn resolve_numbering(
    ppr: Option<&Element>,
    pstyle_id: Option<&str>,
    ctx: &Ctx<'_, '_>,
) -> Result<Option<ListMark>, ConvertError> {
    let direct = |name| val(ppr?.find(ns::W, "numPr")?, name);
    let direct_num_id = direct("numId").and_then(|v| v.parse::<u64>().ok());
    let direct_ilvl = direct("ilvl").and_then(|v| v.parse::<usize>().ok());

    let num_id = match (direct_num_id, pstyle_id) {
        (Some(id), _) => Some(id),
        (None, Some(id)) => ctx.styles.style_num_pr(ctx.parse_ctx, id)?,
        (None, None) => None,
    };
    let Some(num_id) = num_id.filter(|&id| id != 0) else {
        return Ok(None);
    };
    let Some(instance) = ctx.numbering.instance(num_id) else {
        if !ctx.numbering_reported {
            ctx.emit(DiagnosticCode::NumberingUnresolved);
        }
        return Ok(None);
    };
    let ilvl = match (direct_ilvl, pstyle_id) {
        (Some(l), _) => l,
        // Style-referenced numbering carries no usable ilvl (§17.3.1.19);
        // the level comes from the abstract levels' pStyle bindings.
        (None, Some(id)) => {
            ctx.styles.style_numbering_level(ctx.parse_ctx, id, instance)?.unwrap_or(0)
        }
        (None, None) => 0,
    };
    let def = instance.level(ilvl);
    // A numFmt of "none" is explicitly suppressed numbering.
    let Some(marker) = def.marker else {
        return Ok(None);
    };
    let (number, label) = if marker.ordered() {
        ctx.counters.borrow_mut().next(num_id, ilvl, instance)
    } else {
        (0, None)
    };
    Ok(Some(ListMark { key: ListKey { instance: num_id, marker }, level: ilvl, number, label }))
}

/// Content that never renders, by how it counts: deleted text, hidden text, or text moved
/// out of the named range, which counts as deleted only when no destination names it.
#[derive(Clone, PartialEq)]
enum Dropped {
    Revision,
    Hidden,
    Moved(Rc<str>),
}

#[derive(Default)]
struct MathRuns {
    any: bool,
    shown: bool,
    rendered: bool,
    hidden: bool,
    alternate: bool,
}

fn renders(c: char) -> bool {
    !c.is_whitespace() && !c.is_control() && !is_layout_invisible(c)
}

/// Whether text would show anything once the symbol fonts `route` gives it decode it: a code no
/// table maps still draws a glyph.
fn text_renders(text: &str, route: Option<FontRoute>) -> bool {
    let Some(route) = route else {
        return text.chars().any(renders);
    };
    let decoded = symbol::decode_run(text, |c| route.font_of(c));
    decoded.unmapped > 0
        || decoded.tokens.iter().any(|token| match token {
            Token::Text(text) => text.chars().any(renders),
            Token::Checkbox(_) => true,
        })
}

fn shows_text(elem: &Element, route: Option<FontRoute>) -> bool {
    elem.children.iter().any(|node| matches!(node, Node::Text(text) if text_renders(text, route)))
}

fn math_run_renders(run: &Element) -> bool {
    run.find_all(ns::M, "t").any(|t| shows_text(t, None))
}

struct InlineWalker<'a, 'b, 'e> {
    ctx: &'e Ctx<'a, 'b>,
    base: Style,
    hides: bool,
    fonts: RunFonts<'b>,
    pieces: Vec<Piece>,
    current: Vec<Inline>,
    fields: Vec<FieldFrame>,
    /// Hyperlinks, simple fields and complex fields enclosing this walker's
    /// output, which nest against `max_nesting_depth` with its own fields.
    enclosing: u64,
    /// Whether an enclosing walker discards this one's output (a field's instruction).
    discarded: bool,
    revision: Option<Dropped>,
    /// The kind of the dropped range still open in the paragraph, which holds rendered content;
    /// an inner walker takes it and hands it back.
    range: Option<Dropped>,
    dropped: bool,
}

impl<'a, 'b, 'e> InlineWalker<'a, 'b, 'e> {
    fn new(ctx: &'e Ctx<'a, 'b>, base: Style, hides: bool, fonts: RunFonts<'b>) -> Self {
        Self {
            ctx,
            base,
            hides,
            fonts,
            pieces: Vec::new(),
            current: Vec::new(),
            fields: Vec::new(),
            enclosing: 0,
            discarded: false,
            revision: None,
            range: None,
            dropped: false,
        }
    }

    /// A walker for content outside any paragraph, which the default paragraph style formats.
    fn outside_paragraphs(ctx: &'e Ctx<'a, 'b>) -> Result<Self, ConvertError> {
        let styles = ctx.styles;
        let hides = styles.paragraph_hidden(ctx.parse_ctx, None)?;
        let fonts = styles.paragraph_fonts(ctx.parse_ctx, None)?;
        Ok(Self::new(ctx, styles.doc_defaults, hides, fonts))
    }

    /// The nesting level one more wrapper here would open, refused past
    /// `max_nesting_depth`.
    fn deeper(&self) -> Result<u64, ConvertError> {
        let max = self.ctx.parse_ctx.limits().max_nesting_depth;
        let open = u64::try_from(self.fields.len()).unwrap_or(u64::MAX);
        let level = self.enclosing.saturating_add(open).saturating_add(1);
        if level > max {
            return Err(ConvertError::limit(Limit::NestingDepth, max));
        }
        Ok(level)
    }

    fn inner(&mut self) -> Result<Self, ConvertError> {
        Ok(Self {
            enclosing: self.deeper()?,
            discarded: !self.shown(),
            revision: self.revision.clone(),
            range: self.range.take(),
            ..Self::new(self.ctx, self.base, self.hides, self.fonts)
        })
    }

    /// An inner walker's content and attachments, and whether it dropped any run, taking back
    /// the range it holds open.
    fn rejoin(&mut self, mut inner: Self) -> (Vec<Inline>, Vec<Block>, bool) {
        self.range = inner.range.take();
        self.dropped |= inner.dropped;
        let dropped = inner.dropped;
        let (content, attachments) = split_pieces(inner.finish());
        (content, attachments, dropped)
    }

    fn close_range(&mut self) {
        match self.range.take() {
            Some(Dropped::Revision) => self.ctx.emit(DiagnosticCode::RevisionOmitted),
            Some(Dropped::Hidden) => self.ctx.emit(DiagnosticCode::HiddenOmitted),
            Some(Dropped::Moved(name)) => self.ctx.moves.borrow_mut().sources.push(name),
            None => {}
        }
    }

    fn drop_content(&mut self, kind: &Dropped) {
        if self.range.as_ref() != Some(kind) {
            self.close_range();
            self.range = Some(kind.clone());
        }
    }

    /// Whether what is pushed now reaches the output: inside no field's instruction, here or in
    /// an enclosing walker.
    fn shown(&self) -> bool {
        !self.discarded && self.fields.iter().all(|f| f.in_result)
    }

    /// Reports `count` symbol-font codes no table maps, where their text would have shown.
    fn unmapped(&self, count: u64) {
        if self.shown() {
            self.ctx.parse_ctx.emit_count(DiagnosticCode::UnmappedSymbol, self.ctx.region, count);
        }
    }

    /// How declined alternate content here is judged: as it would show, or from a field's
    /// instruction, where only text boxes, charts and diagrams render.
    fn branch_holder(&self) -> BranchHolder {
        if self.shown() { BranchHolder::Text } else { BranchHolder::Instruction }
    }

    fn report_shown(&self, code: DiagnosticCode) {
        if self.shown() {
            self.ctx.emit(code);
        }
    }

    /// Content a run or math shows here, which ends the dropped range open before it.
    fn push(&mut self, inline: Inline) {
        if self.range.is_some() && self.shown() {
            self.close_range();
        }
        self.place(inline);
    }

    fn place(&mut self, inline: Inline) {
        match self.fields.last_mut() {
            Some(f) if f.in_result => f.inlines.push(inline),
            Some(_) => {}
            None => self.current.push(inline),
        }
    }

    /// Attach block content a run or math shows at the current position in run order. Blocks
    /// render even from a field's instruction, so they always end the dropped range open
    /// before them.
    fn push_blocks(&mut self, blocks: Vec<Block>) {
        if blocks.is_empty() {
            return;
        }
        self.close_range();
        self.place_blocks(blocks);
    }

    fn place_all(&mut self, inlines: Vec<Inline>) {
        for inline in inlines {
            self.place(inline);
        }
    }

    fn place_blocks(&mut self, blocks: Vec<Block>) {
        if blocks.is_empty() {
            return;
        }
        if !self.current.is_empty() {
            self.pieces.push(Piece::Inlines(std::mem::take(&mut self.current)));
        }
        self.pieces.push(Piece::Blocks(blocks));
    }

    /// Content a tracked change drops whole, unwalked so nothing inside it reports.
    fn drop_whole(&mut self, kind: &Dropped) {
        self.dropped = true;
        if self.shown() {
            self.drop_content(kind);
        }
    }

    fn walk(&mut self, elem: &Element) -> Result<(), ConvertError> {
        for child in elem.child_elems() {
            let dropping = self.revision.clone();
            if child.is(ns::MC, "AlternateContent") {
                let branch = match dropping {
                    Some(_) => omitted_branch(child),
                    None => {
                        self.ctx.alternate_branch(child, None, self.hides, self.branch_holder())?
                    }
                };
                if let Some(branch) = branch {
                    self.walk(branch)?;
                }
                continue;
            }
            if child.is(ns::M, "oMathPara") || child.is(ns::M, "oMath") {
                self.walk_math(child, child.is(ns::M, "oMathPara"))?;
                continue;
            }
            if child.ns.as_deref() != Some(ns::W) {
                continue;
            }
            match child.local.as_str() {
                "r" => self.walk_run(child)?,
                "hyperlink" => self.walk_hyperlink(child, dropping.is_some())?,
                "fldSimple" => {
                    let instr = child.attr(ns::W, "instr").unwrap_or("");
                    let mut inner = self.inner()?;
                    inner.walk(child)?;
                    let (content, attachments, _) = self.rejoin(inner);
                    self.place_all(field_result(instr, content));
                    self.place_blocks(attachments);
                }
                "bookmarkStart" => {
                    if let Some(name) = child.attr(ns::W, "name")
                        && name != "_GoBack"
                    {
                        self.place(Inline::Anchor(name.to_string()));
                    }
                }
                "sdt" => {
                    if let Some(content) = child.find(ns::W, "sdtContent") {
                        self.walk(content)?;
                    }
                }
                "smartTag" | "ins" | "bdo" | "dir" | "moveTo" | "customXml" => self.walk(child)?,
                "del" | "moveFrom" => self.in_revision(child, |w| w.walk(child))?,
                "moveFromRangeStart" | "moveFromRangeEnd" | "moveToRangeStart" => {
                    self.ctx.move_range(child, dropping.is_none() && self.shown())?;
                }
                "subDoc" => match &dropping {
                    Some(kind) => self.drop_whole(kind),
                    None => self.report_shown(DiagnosticCode::ImportedPartUnread),
                },
                _ => {}
            }
        }
        Ok(())
    }

    fn walk_hyperlink(&mut self, link: &Element, dropping: bool) -> Result<(), ConvertError> {
        let mut inner = self.inner()?;
        inner.walk(link)?;
        let (content, attachments, dropped) = self.rejoin(inner);
        let target = self.hyperlink_link_target(link);
        if (dropped || dropping) && inlines_are_empty(&content) {
            self.place_all(content);
        } else {
            let unknown = target.is_none()
                && link
                    .attr_qualified(ns::R, "id")
                    .is_some_and(|id| self.ctx.rels.get(id).is_none());
            if unknown && self.shown() && !label_is_absolute_url(&inlines_to_plain_text(&content)) {
                self.ctx.emit_related(DiagnosticCode::LinkTargetUnread);
            }
            match target {
                // An empty label still keeps a resolved target: the
                // renderer shows the URL as the link text.
                Some(target) if content.is_empty() => self.push(Inline::Link { content, target }),
                Some(target) => self.place(Inline::Link { content, target }),
                None => self.place_all(content),
            }
        }
        self.place_blocks(attachments);
        Ok(())
    }

    /// An equation without the runs that drop, through the selected branch of its alternate
    /// content. A hidden, deleted or moved-away run holding text joins the dropped range open
    /// where it sits and a shown run holding text ends it, so a range still open at the
    /// equation's end runs on past it unless the equation renders something. Math with no run
    /// drops whole where the paragraph hides it or a revision drops it.
    fn walk_math(&mut self, math: &Element, display: bool) -> Result<(), ConvertError> {
        let mut runs = MathRuns::default();
        self.walk_math_runs(math, display, &mut runs)?;
        let dropping = if runs.any {
            None
        } else {
            self.revision.clone().or_else(|| self.hides.then_some(Dropped::Hidden))
        };
        if runs.any && !runs.shown {
            return Ok(());
        }
        let filtered;
        let shown_math = if runs.hidden || runs.alternate {
            filtered = self.without_hidden_runs(math)?;
            &filtered
        } else {
            math
        };
        let (blocks, tex) = if display {
            (omath_para_to_tex(shown_math), String::new())
        } else {
            (Vec::new(), omath_to_tex(shown_math))
        };
        let renders = !blocks.is_empty() || !tex.is_empty();
        if let Some(kind) = dropping {
            self.dropped = true;
            if renders && self.math_rendered(display) {
                self.drop_content(&kind);
            }
            return Ok(());
        }
        if runs.alternate && self.math_rendered(display) {
            self.report_declined(math)?;
        }
        if renders && !runs.rendered {
            self.math_shows(display);
        }
        if display {
            self.place_blocks(blocks.into_iter().map(Block::Math).collect());
        } else if !tex.is_empty() {
            self.place(Inline::Math(tex));
        }
        Ok(())
    }

    fn walk_math_runs(
        &mut self,
        elem: &Element,
        display: bool,
        runs: &mut MathRuns,
    ) -> Result<(), ConvertError> {
        for child in elem.child_elems() {
            if child.is(ns::M, "r") {
                runs.any = true;
                let mut kind = self.revision.clone();
                if kind.is_none() {
                    let rpr = child.find(ns::W, "rPr");
                    let hidden = hidden_beside_runs(self.ctx, rpr, self.base, self.hides)?;
                    runs.hidden |= hidden;
                    kind = hidden.then_some(Dropped::Hidden);
                }
                if let Some(kind) = kind {
                    self.dropped = true;
                    if self.math_rendered(display) && math_run_renders(child) {
                        self.drop_content(&kind);
                    }
                } else {
                    runs.shown = true;
                    if math_run_renders(child) {
                        runs.rendered = true;
                        self.math_shows(display);
                    }
                }
            } else if child.is(ns::MC, "AlternateContent") {
                runs.alternate = true;
                if let Some(branch) = omitted_branch(child) {
                    self.walk_math_runs(branch, display, runs)?;
                }
            } else if child.ns.as_deref() != Some(ns::W) {
                self.walk_math_runs(child, display, runs)?;
            } else {
                match child.local.as_str() {
                    "del" | "moveFrom" => {
                        self.in_revision(child, |w| w.walk_math_runs(child, display, runs))?;
                    }
                    "moveFromRangeStart" | "moveFromRangeEnd" | "moveToRangeStart" => {
                        let renders = self.revision.is_none() && self.math_rendered(display);
                        self.ctx.move_range(child, renders)?;
                    }
                    "rPr" | "pPr" => {}
                    _ => self.walk_math_runs(child, display, runs)?,
                }
            }
        }
        Ok(())
    }

    /// Reports what the declined choices of an equation that renders hold as it would show,
    /// outside its deletions and moves away.
    fn report_declined(&self, elem: &Element) -> Result<(), ConvertError> {
        for child in elem.child_elems() {
            if child.is(ns::MC, "AlternateContent") {
                if let Some(branch) = self.ctx.math_branch(child, self.hides)? {
                    self.report_declined(branch)?;
                }
            } else if !child.is(ns::W, "del") && !child.is(ns::W, "moveFrom") {
                self.report_declined(child)?;
            }
        }
        Ok(())
    }

    /// Math that shows, which ends the dropped range open before it where the equation renders.
    fn math_shows(&mut self, display: bool) {
        if self.range.is_some() && self.math_rendered(display) {
            self.close_range();
        }
    }

    /// Whether an equation here reaches the output: display math renders even from a field's
    /// instruction.
    fn math_rendered(&self, display: bool) -> bool {
        display || self.shown()
    }

    /// The equation as it renders: without its hidden runs, and alternate content replaced by
    /// its selected branch.
    fn without_hidden_runs(&self, elem: &Element) -> Result<Element, ConvertError> {
        copy_with(self.ctx.parse_ctx, elem, |child| {
            if child.is(ns::MC, "AlternateContent") {
                let branch = omitted_branch(child);
                return branch.map(|branch| self.without_hidden_runs(branch)).transpose();
            }
            let rpr = child.find(ns::W, "rPr");
            if child.is(ns::M, "r") && hidden_beside_runs(self.ctx, rpr, self.base, self.hides)? {
                return Ok(None);
            }
            self.without_hidden_runs(child).map(Some)
        })
    }

    /// The content of a deleted row or cell: each paragraph under its own style's fonts, the
    /// selected branch of alternate content, display math, and the move ranges it opens or
    /// closes, where no destination renders.
    fn walk_paragraphs(&mut self, elem: &Element) -> Result<(), ConvertError> {
        for child in elem.child_elems() {
            if child.is(ns::W, "p") {
                let pstyle_id = pstyle_id(child.find(ns::W, "pPr"));
                let outer = self.fonts;
                let fonts = self.ctx.styles.paragraph_fonts(self.ctx.parse_ctx, pstyle_id);
                self.fonts = or_when_malformed(fonts, || outer)?;
                let walked = self.walk(child);
                self.fonts = outer;
                walked?;
            } else if child.is(ns::MC, "AlternateContent") {
                if let Some(branch) = omitted_branch(child) {
                    self.walk_paragraphs(branch)?;
                }
            } else if child.is(ns::M, "oMathPara") || child.is(ns::M, "oMath") {
                self.walk_math(child, true)?;
            } else if is_move_marker(child) {
                self.ctx.move_range(child, false)?;
            } else {
                self.walk_paragraphs(child)?;
            }
        }
        Ok(())
    }

    /// Runs `walk` within a deletion or move out, whose runs all drop: a deletion wins over any
    /// move it holds.
    fn in_revision(
        &mut self,
        revision: &Element,
        walk: impl FnOnce(&mut Self) -> Result<(), ConvertError>,
    ) -> Result<(), ConvertError> {
        let kind = if revision.local == "del" || self.revision == Some(Dropped::Revision) {
            Dropped::Revision
        } else {
            self.ctx.move_source()
        };
        let outer = self.revision.replace(kind);
        let walked = walk(self);
        self.revision = outer;
        walked
    }

    fn hyperlink_link_target(&self, link: &Element) -> Option<LinkTarget> {
        if let Some(id) = link.attr_qualified(ns::R, "id")
            && let Some(rel) = self.ctx.rels.get(id)
        {
            return Some(classify_rel_target(rel.mode == TargetMode::External, &rel.target));
        }
        link.attr(ns::W, "anchor").map(|a| LinkTarget::anchor(a.to_string()))
    }

    fn walk_run(&mut self, run: &Element) -> Result<(), ConvertError> {
        if let Some(kind) = self.revision.clone() {
            return self.drop_run(run, &kind);
        }
        let rpr = run.find(ns::W, "rPr");
        let (style, hidden) = run_properties(self.ctx, rpr, self.base, self.hides)?;
        if hidden {
            return self.drop_run(run, &Dropped::Hidden);
        }
        let fonts = self.run_fonts(rpr)?;
        self.walk_run_content(run, style, fonts)
    }

    fn run_fonts<'r>(&self, rpr: Option<&'r Element>) -> Result<RunFonts<'r>, ConvertError>
    where
        'b: 'r,
    {
        let char_fonts = match rpr.and_then(|rpr| val(rpr, "rStyle")) {
            Some(id) => self.ctx.styles.run_fonts(self.ctx.parse_ctx, id)?.over(self.fonts),
            None => self.fonts,
        };
        Ok(rpr.map_or(char_fonts, |rpr| RunFonts::of(rpr).over(char_fonts)))
    }

    /// A run that never renders: its field delimiters still open and close their fields, and
    /// what it would have shown joins the dropped range, unwalked so nothing in it reports. Its
    /// text is judged by the glyphs its symbol fonts draw; a character style that cannot
    /// resolve leaves that to its direct fonts.
    fn drop_run(&mut self, run: &Element, kind: &Dropped) -> Result<(), ConvertError> {
        self.dropped = true;
        let rpr = run.find(ns::W, "rPr");
        let fonts = or_when_malformed(self.run_fonts(rpr), || {
            rpr.map_or(self.fonts, |rpr| RunFonts::of(rpr).over(self.fonts))
        })?;
        self.drop_run_content(run, kind, fonts.route())
    }

    /// The content of a dropped run, and of the branch of its alternate content that would
    /// have rendered.
    fn drop_run_content(
        &mut self,
        parent: &Element,
        kind: &Dropped,
        route: Option<FontRoute>,
    ) -> Result<(), ConvertError> {
        for child in parent.child_elems() {
            let shown = self.shown();
            let mut blocks = false;
            let content = if child.is(ns::MC, "AlternateContent") {
                if let Some(branch) = omitted_branch(child) {
                    self.drop_run_content(branch, kind, route)?;
                }
                false
            } else if child.ns.as_deref() != Some(ns::W) {
                false
            } else {
                match child.local.as_str() {
                    "t" | "delText" => shows_text(child, route),
                    "footnoteReference" | "endnoteReference" => match note_id(child) {
                        Some(id) => {
                            self.ctx.note_reference(&id, false)?;
                            true
                        }
                        None => false,
                    },
                    "drawing" | "pict" | "object" => {
                        blocks = self.drop_note_references(child)?;
                        true
                    }
                    "ruby" => {
                        self.drop_note_references(child)?;
                        true
                    }
                    "sym" | "noBreakHyphen" | "contentPart" | "pgNum" | "dayShort" | "dayLong"
                    | "monthShort" | "monthLong" | "yearShort" | "yearLong" => true,
                    "fldChar" => self.field_char(child)?,
                    "instrText" => {
                        self.instr_text(child);
                        false
                    }
                    _ => false,
                }
            };
            if content && (shown || blocks) {
                self.drop_content(kind);
            }
        }
        Ok(())
    }

    /// Records each note reference a dropped drawing or ruby holds as dropped, through the
    /// branch of any alternate content that would have rendered; true where it holds a text
    /// box, chart or diagram, whose blocks render even from a field's instruction.
    fn drop_note_references(&self, drawing: &Element) -> Result<bool, ConvertError> {
        let mut blocks = false;
        let mut stack: Vec<&Element> = drawing.child_elems().collect();
        while let Some(elem) = stack.pop() {
            self.ctx.parse_ctx.charge(1)?;
            if elem.is(ns::MC, "AlternateContent") {
                stack.extend(omitted_branch(elem));
            } else if elem.is(ns::W, "footnoteReference") || elem.is(ns::W, "endnoteReference") {
                if let Some(id) = note_id(elem) {
                    self.ctx.note_reference(&id, false)?;
                }
            } else {
                blocks |= elem.is(ns::W, "txbxContent")
                    || matches!(
                        graphic(elem, None),
                        Some(Graphic::Chart(Some(_)) | Graphic::Diagram(Some(_)))
                    );
                stack.extend(elem.child_elems());
            }
        }
        Ok(blocks)
    }

    /// Reports a ruby, which is never walked: where it shows, and in a field's instruction, which
    /// drops its note references, only when it holds a text box, chart or diagram.
    fn report_ruby(&self, ruby: &Element) -> Result<(), ConvertError> {
        if self.shown() || self.drop_note_references(ruby)? {
            self.ctx.emit(DiagnosticCode::RubyUnread);
        }
        Ok(())
    }

    fn walk_run_content(
        &mut self,
        run: &Element,
        style: Style,
        fonts: RunFonts<'_>,
    ) -> Result<(), ConvertError> {
        let route = fonts.route();
        for child in run.child_elems() {
            if child.is(ns::MC, "AlternateContent") {
                if let Some(branch) =
                    self.ctx.alternate_branch(child, None, false, self.branch_holder())?
                {
                    self.walk_run_content(branch, style, fonts)?;
                }
                continue;
            }
            if child.ns.as_deref() != Some(ns::W) {
                continue;
            }
            match child.local.as_str() {
                "t" => {
                    // Run edges carry the spacing between words in documents
                    // that never mark xml:space, and XML leaves unmarked
                    // whitespace to the application, so it is kept.
                    let text = child.text();
                    match route {
                        Some(route) => self.push_symbol_text(&text, style, |c| route.font_of(c)),
                        None => self.push_text(&text, style),
                    }
                }
                "tab" | "ptab" => self.push(Inline::Text { text: " ".into(), style: Style::PLAIN }),
                "noBreakHyphen" => self.push(Inline::Text { text: "-".into(), style }),
                "sym" => self.push_sym(child, style),
                "ruby" => self.report_ruby(child)?,
                "contentPart" => self.report_shown(DiagnosticCode::GraphicUnread),
                "pgNum" | "dayShort" | "dayLong" | "monthShort" | "monthLong" | "yearShort"
                | "yearLong" => self.report_shown(DiagnosticCode::FieldValueUnread),
                // Markdown has no pages or columns, but every w:br still
                // separates the runs around it: dropping a page break
                // outright would join the words on either side. One left at
                // the end of a paragraph is trimmed when the block renders.
                "br" | "cr" => self.push(Inline::LineBreak),
                "footnoteReference" | "endnoteReference" => {
                    if let Some(id) = note_id(child) {
                        self.ctx.note_reference(&id, self.shown())?;
                        self.push(Inline::NoteRef(id));
                    }
                }
                "commentReference" => {
                    if self.shown() {
                        self.ctx.references.comment();
                    }
                }
                "drawing" | "pict" | "object" => self.walk_drawing(child)?,
                "fldChar" => {
                    let shown = self.shown();
                    if self.field_char(child)? && shown {
                        self.ctx.emit(DiagnosticCode::FormControlUnread);
                    }
                }
                "instrText" => self.instr_text(child),
                _ => {}
            }
        }
        Ok(())
    }

    /// Opens, separates or closes a complex field at its delimiter; true where it begins a
    /// checkbox or drop-down form field.
    fn field_char(&mut self, delimiter: &Element) -> Result<bool, ConvertError> {
        match delimiter.attr(ns::W, "fldCharType") {
            Some("begin") => {
                self.deeper()?;
                self.fields.push(FieldFrame::default());
                return Ok(delimiter.find(ns::W, "ffData").is_some_and(|data| {
                    data.find(ns::W, "checkBox").is_some() || data.find(ns::W, "ddList").is_some()
                }));
            }
            Some("separate") => {
                if let Some(f) = self.fields.last_mut() {
                    f.in_result = true;
                }
            }
            Some("end") => {
                if let Some(FieldFrame { instr, inlines, .. }) = self.fields.pop() {
                    self.place_all(field_result(&instr, inlines));
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn instr_text(&mut self, instr: &Element) {
        if let Some(f) = self.fields.last_mut() {
            f.instr.push_str(&instr.text());
        }
    }

    /// Inside a field's instruction only text boxes, charts and diagrams are read or
    /// reported, since nothing else there reaches the output.
    fn walk_drawing(&mut self, elem: &Element) -> Result<(), ConvertError> {
        let mut contents = Contents::default();
        contents.collect(elem, self.ctx, self.branch_holder(), None)?;
        let Contents { boxes, graphics, shapes, shape_text } = contents;
        let boxed = !boxes.is_empty();
        let mut blocks = Vec::new();
        for tb in boxes {
            blocks.extend(parse_blocks(tb, self.ctx)?);
        }
        self.push_blocks(blocks);
        if shape_text {
            self.report_shown(DiagnosticCode::GraphicUnread);
        }
        let read = graphics
            .iter()
            .enumerate()
            .filter_map(|(index, graphic)| Some((graphic.rank()?, index)))
            .min()
            .map(|(_, index)| index)
            .filter(|_| !boxed);
        let previews = pair_previews(self.ctx.parse_ctx, &graphics, &shapes)?;
        self.report_unread(&graphics, &previews, read);
        if boxed {
            return Ok(());
        }

        let descr = elem
            .first_descendant(ns::WP, "docPr")
            .and_then(|d| d.attr(ns::WP, "descr"))
            .map(clean_text)
            .unwrap_or_default();
        let preview = read.and_then(|index| previews.get(index)).copied().unwrap_or_default();
        self.read_graphic(&graphics, preview, read, descr)
    }

    fn read_graphic(
        &mut self,
        graphics: &[Graphic<'_>],
        preview: Preview<'_>,
        read: Option<usize>,
        descr: String,
    ) -> Result<(), ConvertError> {
        match read.and_then(|index| graphics.get(index)) {
            Some(Graphic::Chart(Some(rel_id))) => {
                let blocks = drawingml::graphic_blocks(
                    self.ctx.parse_ctx,
                    &self.ctx.graphic_part(),
                    Some(rel_id),
                    ns::CHART,
                    "chartSpace",
                    |root| drawingml::chart_blocks(self.ctx.parse_ctx, root, self.ctx.region),
                )?;
                self.push_blocks(blocks);
            }
            Some(Graphic::Diagram(Some(rel_id))) => {
                let blocks = drawingml::graphic_blocks(
                    self.ctx.parse_ctx,
                    &self.ctx.graphic_part(),
                    Some(rel_id),
                    ns::DGM,
                    "dataModel",
                    |root| Ok(drawingml::diagram_blocks(root)),
                )?;
                self.push_blocks(blocks);
            }
            Some(Graphic::Object { .. } | Graphic::Control { .. } | Graphic::Picture { .. })
                if !self.shown() => {}
            Some(held @ (Graphic::Object { .. } | Graphic::Control { .. })) => {
                self.held_preview(held, graphics, preview, descr)?;
            }
            Some(Graphic::Picture { rel_id, shape, .. }) => {
                self.picture(*rel_id, labelled(*shape, descr))?;
            }
            _ => self.stand_in(descr),
        }
        Ok(())
    }

    fn held_preview(
        &mut self,
        held: &Graphic<'_>,
        graphics: &[Graphic<'_>],
        preview: Preview<'_>,
        descr: String,
    ) -> Result<(), ConvertError> {
        match held {
            Graphic::Object { rel_id, .. } => self.embedded_object(*rel_id)?,
            _ => self.ctx.emit(DiagnosticCode::FormControlUnread),
        }
        if let Some(Graphic::Picture { rel_id, shape, .. }) =
            preview.picture.and_then(|index| graphics.get(index))
        {
            return self.picture(*rel_id, labelled(*shape, descr));
        }
        self.stand_in(labelled(preview.shape, descr));
        Ok(())
    }

    fn stand_in(&mut self, descr: String) {
        if !descr.trim().is_empty() {
            self.push_image(descr, ImageSource::Unavailable);
        }
    }

    fn report_unread(
        &self,
        graphics: &[Graphic<'_>],
        previews: &[Preview<'_>],
        read: Option<usize>,
    ) {
        let previewed: BTreeSet<usize> = previews.iter().filter_map(|p| p.picture).collect();
        let shown = self.shown();
        for (index, graphic) in graphics.iter().enumerate() {
            if Some(index) == read {
                continue;
            }
            match graphic {
                Graphic::Chart(_) | Graphic::Diagram(_) => {
                    self.ctx.emit(DiagnosticCode::GraphicUnread);
                }
                _ if !shown => {}
                Graphic::WordArt | Graphic::Movie => self.ctx.emit(DiagnosticCode::GraphicUnread),
                Graphic::Object { .. } => self.ctx.emit(DiagnosticCode::EmbeddedObject),
                Graphic::Control { .. } => self.ctx.emit(DiagnosticCode::FormControlUnread),
                Graphic::Picture { .. } if previewed.contains(&index) => {}
                Graphic::Picture { .. } => {
                    self.ctx.parse_ctx.image_loss(ImageLoss::Unplaced, self.ctx.region);
                }
            }
        }
    }

    /// An embedded OLE object's payload, retained as an asset nothing renders; its content is
    /// not converted.
    fn embedded_object(&self, rel_id: Option<&str>) -> Result<(), ConvertError> {
        self.ctx.emit(DiagnosticCode::EmbeddedObject);
        let ctx = self.ctx;
        retain_ole_payload(ctx.pkg, &ctx.rels, &ctx.base_part, ctx.assets, rel_id)
    }

    /// A picture: an external-mode target (`r:link`) becomes an external
    /// source, and an embedded one is retained as an asset.
    fn picture(&mut self, rel_id: Option<&str>, descr: String) -> Result<(), ConvertError> {
        let ctx = self.ctx;
        let source = rel_id
            .map(|id| rel_image_source(ctx.pkg, &ctx.rels, &ctx.base_part, ctx.assets, id))
            .transpose()?
            .flatten()
            .unwrap_or(ImageSource::Unread);
        self.push_image(descr, source);
        Ok(())
    }

    fn push_image(&mut self, alt: String, source: ImageSource) {
        let origin = self.ctx.parse_ctx.image_origin(self.ctx.region);
        self.push(Inline::Image { origin, alt, source });
    }

    fn push_text(&mut self, text: &str, style: Style) {
        let text = clean_text(text);
        if !text.is_empty() {
            self.push(Inline::Text { text, style });
        }
    }

    fn push_symbol_text(
        &mut self,
        text: &str,
        style: Style,
        font_of: impl Fn(char) -> Option<SymbolFont>,
    ) {
        let decoded = symbol::decode_run(text, font_of);
        self.unmapped(decoded.unmapped);
        for token in decoded.tokens {
            match token {
                Token::Text(text) => self.push_text(&text, style),
                Token::Checkbox(checked) => self.push(Inline::Checkbox(checked)),
            }
        }
    }

    /// A `w:sym`: the character its font draws at its code, which is read even where it is a
    /// control or a space, since the element names a glyph and never text.
    fn push_sym(&mut self, sym: &Element, style: Style) {
        let font = sym.attr(ns::W, "font").and_then(SymbolFont::from_name);
        let code =
            sym.attr(ns::W, "char").and_then(symbol::parse_char_code).and_then(symbol::font_code);
        match font.zip(code).map(|(font, code)| font.glyph(code)) {
            Some(symbol::Glyph::Char(c)) => self.push_text(&c.to_string(), style),
            Some(symbol::Glyph::Checkbox(checked)) => self.push(Inline::Checkbox(checked)),
            Some(symbol::Glyph::Unmapped) | None => self.unmapped(1),
        }
    }

    fn finish(mut self) -> Vec<Piece> {
        while let Some(frame) = self.fields.pop() {
            for inline in frame.inlines {
                self.push(inline);
            }
        }
        if !self.current.is_empty() {
            self.pieces.push(Piece::Inlines(self.current));
        }
        self.pieces
    }
}

fn note_id(reference: &Element) -> Option<String> {
    let prefix = if reference.local == "footnoteReference" { "fn" } else { "en" };
    reference.attr(ns::W, "id").map(|id| format!("{prefix}{id}"))
}

fn split_pieces(pieces: Vec<Piece>) -> (Vec<Inline>, Vec<Block>) {
    let mut inlines = Vec::new();
    let mut blocks = Vec::new();
    for piece in pieces {
        match piece {
            Piece::Inlines(mut i) => inlines.append(&mut i),
            Piece::Blocks(b) => blocks.extend(b),
        }
    }
    (inlines, blocks)
}

fn pieces_into_blocks(pieces: Vec<Piece>) -> Vec<Block> {
    let mut blocks = Vec::new();
    for piece in pieces {
        match piece {
            // Visually empty inlines still become a paragraph: a
            // bookmark-only one carries the anchor link resolution binds to.
            Piece::Inlines(inlines) => blocks.push(Block::Paragraph(inlines)),
            Piece::Blocks(attachments) => blocks.extend(attachments),
        }
    }
    blocks
}

/// A graphic a drawing shows outside its text boxes.
enum Graphic<'e> {
    Chart(Option<&'e str>),
    Diagram(Option<&'e str>),
    /// An OLE object, with the relationship of its payload and the id of the VML shape it names
    /// as its preview.
    Object {
        rel_id: Option<&'e str>,
        shape: Option<&'e str>,
    },
    /// A form control, with the id of the VML shape it names as its preview.
    Control {
        shape: Option<&'e str>,
    },
    /// A picture, with the VML shape that holds it; a VML picture fill is read after every other
    /// picture.
    Picture {
        rel_id: Option<&'e str>,
        shape: Option<&'e Element>,
        fill: bool,
    },
    Movie,
    WordArt,
}

impl Graphic<'_> {
    /// Which graphic a drawing reads: the lowest rank, the first of equals; `None` is never read.
    fn rank(&self) -> Option<u8> {
        match self {
            Self::Chart(Some(_)) => Some(0),
            Self::Diagram(Some(_)) => Some(1),
            Self::Object { .. } | Self::Control { .. } => Some(2),
            Self::Picture { fill: false, .. } => Some(3),
            Self::Picture { fill: true, .. } => Some(4),
            Self::Chart(None) | Self::Diagram(None) | Self::Movie | Self::WordArt => None,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Preview<'e> {
    /// An index into the drawing's graphics.
    picture: Option<usize>,
    shape: Option<&'e Element>,
}

/// Each object's or control's preview: the first picture and shape holding the id it names, else
/// the first picture no shape holds, a picture fill counting only where no other picture does;
/// every id is charged by its length.
fn pair_previews<'e>(
    parse_ctx: &ParseCtx<'_>,
    graphics: &[Graphic<'e>],
    shapes: &[&'e Element],
) -> Result<Vec<Preview<'e>>, ConvertError> {
    let id_cost = |id: Option<&str>| id.map_or(0, str::len) as u64 + 1;
    let mut pictures: HashMap<&str, (bool, usize)> = HashMap::new();
    let mut unheld: Option<(bool, usize)> = None;
    for (index, graphic) in graphics.iter().enumerate() {
        let Graphic::Picture { shape, fill, .. } = graphic else { continue };
        let id = shape.and_then(|shape| shape.attr_unqualified("id"));
        parse_ctx.charge(id_cost(id))?;
        let key = (*fill, index);
        match (shape, id) {
            (None, _) => unheld = Some(unheld.map_or(key, |held| held.min(key))),
            (Some(_), Some(id)) => {
                _ = pictures.entry(id).and_modify(|held| *held = (*held).min(key)).or_insert(key);
            }
            (Some(_), None) => {}
        }
    }
    let mut by_id: HashMap<&str, &Element> = HashMap::new();
    for &shape in shapes {
        let id = shape.attr_unqualified("id");
        parse_ctx.charge(id_cost(id))?;
        if let Some(id) = id {
            by_id.entry(id).or_insert(shape);
        }
    }
    graphics
        .iter()
        .map(|graphic| {
            let (Graphic::Object { shape: id, .. } | Graphic::Control { shape: id }) = graphic
            else {
                return Ok(Preview::default());
            };
            parse_ctx.charge(id_cost(*id))?;
            let held = id.and_then(|id| pictures.get(id).copied()).or(unheld);
            let picture = held.map(|(_, index)| index);
            Ok(Preview { picture, shape: id.and_then(|id| by_id.get(id).copied()) })
        })
        .collect()
}

fn shape_alt(shape: &Element) -> Option<String> {
    let alt = clean_text(shape.attr_unqualified("alt")?);
    (!alt.trim().is_empty()).then_some(alt)
}

fn labelled(shape: Option<&Element>, descr: String) -> String {
    shape.and_then(shape_alt).unwrap_or(descr)
}

/// What a drawing or VML pict holds, in document order: text-box content
/// (`w:txbxContent`), whose own drawings its blocks walk, and the graphics,
/// VML shapes and shape text outside it, through the branch each
/// `mc:AlternateContent` takes.
#[derive(Default)]
struct Contents<'e> {
    boxes: Vec<&'e Element>,
    graphics: Vec<Graphic<'e>>,
    shapes: Vec<&'e Element>,
    shape_text: bool,
}

impl<'e> Contents<'e> {
    fn collect(
        &mut self,
        elem: &'e Element,
        ctx: &Ctx<'_, '_>,
        holder: BranchHolder,
        shape: Option<&'e Element>,
    ) -> Result<(), ConvertError> {
        for child in elem.child_elems() {
            if child.is(ns::MC, "AlternateContent") {
                if let Some(branch) = ctx.alternate_branch(child, shape, false, holder)? {
                    self.collect(branch, ctx, holder, shape)?;
                }
                continue;
            }
            if child.is(ns::W, "txbxContent") {
                self.boxes.push(child);
                continue;
            }
            self.shape_text |= child.is(ns::A, "t") && !child.text().trim().is_empty();
            if let Some(graphic) = graphic(child, shape) {
                self.graphics.push(graphic);
            }
            let shape = if is_vml_shape(child) {
                self.shapes.push(child);
                Some(child)
            } else {
                shape
            };
            self.collect(child, ctx, holder, shape)?;
        }
        Ok(())
    }
}

fn graphic<'e>(elem: &'e Element, shape: Option<&'e Element>) -> Option<Graphic<'e>> {
    let rel_id = |name| elem.attr_qualified(ns::R, name);
    Some(match (elem.ns.as_deref()?, elem.local.as_str()) {
        (ns::CHART, "chart") => Graphic::Chart(rel_id("id")),
        (ns::DGM, "relIds") => Graphic::Diagram(rel_id("dm")),
        (ns::O_VML, "OLEObject") => {
            Graphic::Object { rel_id: rel_id("id"), shape: elem.attr_unqualified("ShapeID") }
        }
        (ns::W, "objectEmbed" | "objectLink") => {
            Graphic::Object { rel_id: rel_id("id"), shape: elem.attr(ns::W, "shapeId") }
        }
        (ns::W, "control") => Graphic::Control { shape: elem.attr(ns::W, "shapeid") },
        (ns::W, "movie") => Graphic::Movie,
        (ns::A, "blip") => Graphic::Picture {
            rel_id: rel_id("embed").or_else(|| rel_id("link")),
            shape: None,
            fill: false,
        },
        (ns::VML, "imagedata") => Graphic::Picture { rel_id: rel_id("id"), shape, fill: false },
        (ns::VML, "fill") if is_picture_fill(elem, shape) => {
            Graphic::Picture { rel_id: rel_id("id"), shape, fill: true }
        }
        (ns::VML, "textpath") => {
            elem.attr_unqualified("string").filter(|text| !text.trim().is_empty())?;
            Graphic::WordArt
        }
        _ => return None,
    })
}

struct TcInfo<'e> {
    elem: &'e Element,
    /// Legacy `hMerge` continuation cells folded into this origin.
    merged: Vec<&'e Element>,
    col_span: usize,
    row_span: usize,
    /// vMerge continuation: this position belongs to the origin above.
    covered: bool,
}

/// A table row: its cells between the empty positions `gridBefore` and
/// `gridAfter` declare.
struct RowInfo<'e> {
    before: usize,
    cells: Vec<TcInfo<'e>>,
    after: usize,
    /// Its deleted cells and move markers, each by the number of its cells before it and of
    /// the continuations the last of those folded in before it.
    deleted: Vec<((usize, usize), &'e Element)>,
}

fn grid_filler(trpr: Option<&Element>, name: &str) -> usize {
    trpr.and_then(|p| val(p, name)).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Clamp a row's fillers and spans to the `w:tblGrid` width, so no
/// position a row declares lies past the grid, and return how many values
/// it clamped. A cell keeps at least its own position.
fn clamp_to_grid(row: &mut RowInfo<'_>, width: usize) -> u64 {
    let mut clamped = 0u64;
    let mut clamp = |value: &mut usize, room: usize| {
        if *value > room {
            *value = room;
            clamped += 1;
        }
    };
    clamp(&mut row.before, width);
    let mut col = row.before;
    for tc in &mut row.cells {
        clamp(&mut tc.col_span, width.saturating_sub(col).max(1));
        col = col.saturating_add(tc.col_span);
    }
    clamp(&mut row.after, width.saturating_sub(col));
    clamped
}

fn tc_at<'m, 'e>(
    matrix: &'m mut [RowInfo<'e>],
    row: usize,
    i: usize,
) -> Option<&'m mut TcInfo<'e>> {
    matrix.get_mut(row).and_then(|row| row.cells.get_mut(i))
}

fn merge_vertically(matrix: &mut [RowInfo<'_>]) {
    // Active vertical-merge chains: the column range each covers, by its
    // first column, and the origin it extends.
    let mut active: BTreeMap<usize, (usize, (usize, usize))> = BTreeMap::new();
    for r in 0..matrix.len() {
        let Some(row) = matrix.get(r) else { break };
        let mut col = row.before;
        let mut next_active = BTreeMap::new();
        let mut extended = Vec::new();
        let mut stray = Vec::new();
        for (i, tc) in row.cells.iter().enumerate() {
            let end = col.saturating_add(tc.col_span);
            if tc.covered {
                match active.range(..=col).next_back().filter(|(_, (last, _))| col < *last) {
                    Some((_, &(_, origin))) => {
                        extended.push(origin);
                        next_active.insert(col, (end, origin));
                    }
                    None => stray.push(i),
                }
            } else {
                next_active.insert(col, (end, (r, i)));
            }
            col = end;
        }
        for (orow, oidx) in extended {
            if let Some(origin) = tc_at(matrix, orow, oidx) {
                origin.row_span += 1;
            }
        }
        for i in stray {
            if let Some(continuation) = tc_at(matrix, r, i) {
                continuation.covered = false;
            }
        }
        active = next_active;
    }
}

fn parse_table(tbl: &Element, ctx: &Ctx<'_, '_>) -> Result<Vec<Block>, ConvertError> {
    let (rows, deleted_rows) = collect_rows(tbl, ctx);
    let width = tbl
        .find(ns::W, "tblGrid")
        .map(|grid| grid.find_all(ns::W, "gridCol").count())
        .filter(|&width| width > 0);
    let mut matrix: Vec<RowInfo<'_>> = Vec::new();
    let mut clamped = 0u64;
    for &tr in &rows {
        let trpr = tr.find(ns::W, "trPr");
        let mut row = RowInfo {
            before: grid_filler(trpr, "gridBefore"),
            cells: Vec::new(),
            after: grid_filler(trpr, "gridAfter"),
            deleted: Vec::new(),
        };
        collect_row_cells(tr, ctx, &mut row);
        if let Some(width) = width {
            clamped = clamped.saturating_add(clamp_to_grid(&mut row, width));
        }
        matrix.push(row);
    }
    if clamped > 0 {
        ctx.parse_ctx.emit_count(DiagnosticCode::TableSpanRepaired, ctx.region, clamped);
    }
    merge_vertically(&mut matrix);

    // tblHeader is ST_OnOff: an explicit false value is not a header row.
    let header_rows = rows
        .iter()
        .take_while(|tr| tr.find(ns::W, "trPr").and_then(|p| on_off(p, "tblHeader")) == Some(true))
        .count();

    // Deleted rows and cells and the move markers between rows and cells are walked in
    // document order between the retained ones, which the move ranges they open or close reach.
    let mut deleted_rows = deleted_rows.into_iter().peekable();
    let mut builder = GridBuilder::new(ctx.parse_ctx);
    for (r, row) in matrix.iter().enumerate() {
        drop_deleted_before(&mut deleted_rows, r, ctx)?;
        place_row(&mut builder, row, ctx)?;
    }
    drop_deleted_before(&mut deleted_rows, usize::MAX, ctx)?;
    let mut table = builder.finish()?;
    if table.grid.is_empty() {
        return Ok(Vec::new());
    }
    table.header_rows = resolve_header_rows(&table, header_rows);
    Ok(vec![Block::Table(table)])
}

fn place_row(
    builder: &mut GridBuilder<&ParseCtx<'_>>,
    row: &RowInfo<'_>,
    ctx: &Ctx<'_, '_>,
) -> Result<(), ConvertError> {
    builder.next_row();
    for _ in 0..row.before {
        builder.place(Cell::default())?;
    }
    let mut deleted_cells = row.deleted.iter().copied().peekable();
    for (i, tc) in row.cells.iter().enumerate() {
        drop_deleted_before(&mut deleted_cells, (i, usize::MAX), ctx)?;
        if tc.covered {
            for _ in 0..tc.col_span {
                builder.covered()?;
            }
        } else {
            let mut blocks = parse_blocks(tc.elem, ctx)?;
            for (folded, merged) in tc.merged.iter().enumerate() {
                drop_deleted_before(&mut deleted_cells, (i + 1, folded), ctx)?;
                blocks.extend(parse_blocks(merged, ctx)?);
            }
            let col_span = u32::try_from(tc.col_span).unwrap_or(u32::MAX);
            let row_span = u32::try_from(tc.row_span).unwrap_or(u32::MAX);
            builder.place(Cell::spanning(blocks, col_span, row_span))?;
        }
    }
    drop_deleted_before(&mut deleted_cells, (usize::MAX, usize::MAX), ctx)?;
    for _ in 0..row.after {
        builder.place(Cell::default())?;
    }
    Ok(())
}

/// A table's rows, and its deleted rows and the move markers between rows, each by the number
/// of rows before it: accepting the revisions removes a deleted row. Rows behind a content
/// control, custom XML or alternate content are reported unread.
fn collect_rows<'e>(
    tbl: &'e Element,
    ctx: &Ctx<'_, '_>,
) -> (Vec<&'e Element>, Vec<(usize, &'e Element)>) {
    let (mut rows, mut deleted) = (Vec::new(), Vec::new());
    for child in tbl.child_elems() {
        if is_move_marker(child) {
            deleted.push((rows.len(), child));
        } else if !child.is(ns::W, "tr") {
            if child.first_descendant(ns::W, "tr").is_some() {
                ctx.emit(DiagnosticCode::BlockUnread);
            }
        } else if child.find(ns::W, "trPr").and_then(|p| p.find(ns::W, "del")).is_some() {
            deleted.push((rows.len(), child));
        } else {
            rows.push(child);
        }
    }
    (rows, deleted)
}

fn drop_deleted_before<'e, K: Ord + Copy>(
    deleted: &mut Peekable<impl Iterator<Item = (K, &'e Element)>>,
    at: K,
    ctx: &Ctx<'_, '_>,
) -> Result<(), ConvertError> {
    while let Some((_, elem)) = deleted.next_if(|(before, _)| *before <= at) {
        drop_deleted(elem, ctx)?;
    }
    Ok(())
}

/// Tracks a move marker between rows or cells, or counts a deleted row or cell as one
/// revision when what it holds would render.
fn drop_deleted(elem: &Element, ctx: &Ctx<'_, '_>) -> Result<(), ConvertError> {
    if is_move_marker(elem) {
        return ctx.move_range(elem, true);
    }
    let mut walker = InlineWalker::outside_paragraphs(ctx)?;
    walker.revision = Some(Dropped::Revision);
    walker.walk_paragraphs(elem)?;
    walker.close_range();
    Ok(())
}

/// A row's cells, and its deleted cells and the move markers between cells by where each sits
/// among them.
fn collect_row_cells<'e>(parent: &'e Element, ctx: &Ctx<'_, '_>, row: &mut RowInfo<'e>) {
    for child in parent.child_elems() {
        if child.is(ns::MC, "AlternateContent") {
            if child.first_descendant(ns::W, "tc").is_some() {
                ctx.emit(DiagnosticCode::BlockUnread);
            }
            continue;
        }
        if child.ns.as_deref() != Some(ns::W) {
            continue;
        }
        match child.local.as_str() {
            local
                if is_move_marker(child)
                    || (local == "tc"
                        && child
                            .find(ns::W, "tcPr")
                            .and_then(|p| p.find(ns::W, "cellDel"))
                            .is_some()) =>
            {
                let folded = row.cells.last().map_or(0, |last| last.merged.len());
                row.deleted.push(((row.cells.len(), folded), child));
            }
            "tc" => {
                let tcpr = child.find(ns::W, "tcPr");
                let covered = continues(tcpr, "vMerge");
                let col_span: usize = tcpr
                    .and_then(|p| val(p, "gridSpan"))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1)
                    .max(1);
                // Legacy hMerge: a non-restart hMerge cell folds into the
                // preceding origin, widening its span.
                if continues(tcpr, "hMerge")
                    && let Some(prev) = row.cells.last_mut()
                    && !prev.covered
                {
                    prev.col_span = prev.col_span.saturating_add(col_span);
                    prev.merged.push(child);
                    continue;
                }
                row.cells.push(TcInfo {
                    elem: child,
                    merged: Vec::new(),
                    col_span,
                    row_span: 1,
                    covered,
                });
            }
            "sdt" => {
                if let Some(content) = child.find(ns::W, "sdtContent") {
                    collect_row_cells(content, ctx, row);
                }
            }
            "customXml" => collect_row_cells(child, ctx, row),
            _ => {}
        }
    }
}

fn continues(tcpr: Option<&Element>, name: &str) -> bool {
    tcpr.and_then(|p| p.find(ns::W, name)).is_some_and(|m| m.attr(ns::W, "val") != Some("restart"))
}

fn is_move_marker(elem: &Element) -> bool {
    elem.ns.as_deref() == Some(ns::W)
        && matches!(
            elem.local.as_str(),
            "moveFromRangeStart" | "moveFromRangeEnd" | "moveToRangeStart"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::Limits;
    use crate::model::MarkerKind;
    use crate::package::archive::{PackageKind, Role};
    use crate::package::xml::parse_xml;
    use crate::shared::test_support::zip_package;
    use std::assert_matches;

    fn text(value: &str) -> Piece {
        Piece::Inlines(vec![Inline::plain(value)])
    }

    fn assert_text(block: &Block, expected: &str) {
        let Block::Paragraph(inlines) = block else { panic!("expected paragraph: {block:?}") };
        assert_eq!(inlines_to_plain_text(inlines), expected);
    }

    #[test]
    fn a_character_renders_where_clean_text_keeps_more_than_whitespace() {
        let disagree: Vec<char> = (0..=char::MAX as u32)
            .filter_map(char::from_u32)
            .filter(|&c| {
                renders(c) != clean_text(&c.to_string()).chars().any(|kept| !kept.is_whitespace())
            })
            .collect();
        assert!(disagree.is_empty(), "{disagree:?}");
    }

    #[test]
    fn a_notes_parts_scan_of_references_is_charged_by_their_ids() {
        let references = References::default();
        let long = format!("fn{}", "1".repeat(5000));
        for id in [long.as_str(), "fn2", "en1"] {
            references.notes.borrow_mut().insert(id.to_owned(), false);
        }
        let parse_ctx = ParseCtx::default();
        assert_eq!(references.note_references(&parse_ctx, "fn").unwrap(), (true, false));
        assert!(parse_ctx.fuel() >= 5002 + 3, "{}", parse_ctx.fuel());
    }

    #[test]
    fn a_preview_is_paired_by_id_and_each_id_is_charged_by_its_length() {
        let long = "s".repeat(5000);
        let root = tree(&format!(
            r#"<w:p xmlns:w="{}" xmlns:v="{}"><v:shape id="decoy"/><v:shape/><v:shape id="{long}"/><v:shape id="{long}"/></w:p>"#,
            ns::W,
            ns::VML
        ));
        let shapes: Vec<&Element> = root.find(ns::W, "p").unwrap().child_elems().collect();
        let held = |shape: usize| Graphic::Picture {
            rel_id: None,
            shape: shapes.get(shape).copied(),
            fill: false,
        };
        let graphics = [
            held(0),
            held(1),
            Graphic::Picture { rel_id: None, shape: None, fill: false },
            held(2),
            held(3),
            Graphic::Object { rel_id: None, shape: Some(long.as_str()) },
            Graphic::Control { shape: None },
            Graphic::Object { rel_id: None, shape: Some("none") },
            Graphic::Movie,
        ];
        let parse_ctx = ParseCtx::default();
        let previews = pair_previews(&parse_ctx, &graphics, &shapes).unwrap();
        let paired: Vec<_> = previews
            .iter()
            .map(|p| {
                (p.picture, p.shape.and_then(|s| shapes.iter().position(|&x| std::ptr::eq(x, s))))
            })
            .collect();
        let none = (None, None);
        let unheld = (Some(2), None);
        let expected = [none, none, none, none, none, (Some(3), Some(2)), unheld, unheld, none];
        assert_eq!(paired, expected);
        let pictures = 6 + 1 + 1 + 5001 + 5001;
        let indexed = 6 + 1 + 5001 + 5001;
        let looked_up = 5001 + 1 + 5;
        assert_eq!(parse_ctx.fuel(), pictures + indexed + looked_up);
    }

    fn tree(xml: &str) -> Element {
        parse_xml(&ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap()
    }

    fn with_ctx<T>(styles: &str, limits: &Limits, read: impl FnOnce(&Ctx<'_, '_>) -> T) -> T {
        let probe = ParseCtx::default();
        let bytes = zip_package(&[("word/document.xml", "")]);
        let pkg = Package::open(&probe, &bytes, PackageKind::Opc).unwrap();
        let styles = tree(styles);
        let styles = Styles::parse(styles.find(ns::W, "styles"));
        let (counters, assets) = (RefCell::default(), RefCell::new(AssetSink::new(&probe)));
        let (numbering, references) = (Numbering::default(), References::default());
        let parse_ctx = ParseCtx::new(limits);
        read(&Ctx {
            parse_ctx: &parse_ctx,
            pkg: &pkg,
            rels: Relationships::default(),
            base_part: String::new(),
            region: Region::Body,
            styles: &styles,
            numbering: &numbering,
            numbering_reported: false,
            counters: &counters,
            assets: &assets,
            references: &references,
            moves: RefCell::default(),
        })
    }

    #[test]
    fn a_style_lookup_that_overruns_max_work_fails_a_dropped_run_and_a_deleted_paragraph() {
        let styles = format!(
            r#"<w:styles xmlns:w="{}"><w:style w:type="character" w:styleId="C"/><w:style w:type="paragraph" w:styleId="P"/></w:styles>"#,
            ns::W
        );
        let run = tree(&format!(
            r#"<w:r xmlns:w="{}"><w:rPr><w:rStyle w:val="C"/></w:rPr></w:r>"#,
            ns::W
        ));
        let paragraph = tree(&format!(
            r#"<w:p xmlns:w="{}"><w:pPr><w:pStyle w:val="P"/></w:pPr></w:p>"#,
            ns::W
        ));
        let limits = Limits { max_work: 0, ..Limits::default() };
        let work = |result: Result<(), ConvertError>| {
            assert_matches!(
                result,
                Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 0 })
            );
        };
        with_ctx(&styles, &limits, |ctx| {
            let run = run.find(ns::W, "r").unwrap();
            work(InlineWalker::outside_paragraphs(ctx).unwrap().drop_run(run, &Dropped::Hidden));
            work(InlineWalker::outside_paragraphs(ctx).unwrap().walk_paragraphs(&paragraph));
        });
    }

    fn bullet() -> ParaKind {
        ParaKind::ListItem(ListMark {
            key: ListKey { instance: 1, marker: MarkerKind::Bullet },
            level: 0,
            number: 0,
            label: None,
        })
    }

    fn rule_between_texts() -> Vec<Piece> {
        vec![text("before"), Piece::Blocks(vec![Block::Rule]), text("after")]
    }

    fn emitted(kind: ParaKind, pieces: Vec<Piece>) -> Vec<Block> {
        let (mut blocks, mut runs) = (Vec::new(), Runs::default());
        emit_paragraph(kind, pieces, &mut blocks, &mut runs);
        runs.flush(&mut blocks);
        blocks
    }

    #[test]
    fn heading_attachments_keep_source_order() {
        let heading = ParaKind::Heading { level: 2, label: None, base: Style::PLAIN };
        let blocks = emitted(heading, rule_between_texts());
        let [Block::Heading { content, .. }, Block::Rule, after] = &blocks[..] else {
            panic!("unexpected blocks: {blocks:?}")
        };
        assert_eq!(inlines_to_plain_text(content), "before");
        assert_text(after, "after");
    }

    #[test]
    fn list_attachments_keep_source_order_inside_the_item() {
        let blocks = emitted(bullet(), rule_between_texts());
        let [Block::List(list)] = &blocks[..] else { panic!("unexpected blocks: {blocks:?}") };
        let [before, Block::Rule, after] = &list.items[0].blocks[..] else {
            panic!("unexpected item: {:?}", list.items[0].blocks)
        };
        assert_text(before, "before");
        assert_text(after, "after");
    }

    #[test]
    fn a_bookmark_only_paragraph_keeps_its_anchor() {
        // `inlines_are_empty` is true of a lone anchor, but dropping the
        // paragraph would strip the target a link resolves against.
        let anchor = Piece::Inlines(vec![Inline::Anchor("mark".into())]);
        let blocks = emitted(ParaKind::Plain, vec![anchor]);
        let [Block::Paragraph(inlines)] = &blocks[..] else { panic!("{blocks:?}") };
        assert_matches!(inlines[..], [Inline::Anchor(_)]);
    }

    #[test]
    fn an_empty_list_item_carries_no_blocks() {
        let blocks = emitted(bullet(), Vec::new());
        let [Block::List(list)] = &blocks[..] else { panic!("{blocks:?}") };
        assert!(list.items[0].blocks.is_empty(), "{:?}", list.items[0].blocks);
    }

    #[test]
    fn attachments_split_styled_runs_in_place() {
        let blocks = emitted(ParaKind::Styled(BlockStyle::Code), rule_between_texts());
        let [
            Block::CodeBlock { text: before, .. },
            Block::Rule,
            Block::CodeBlock { text: after, .. },
        ] = &blocks[..]
        else {
            panic!("unexpected blocks: {blocks:?}")
        };
        assert_eq!(before, "before");
        assert_eq!(after, "after");
    }

    #[test]
    fn only_a_malformed_style_takes_the_fallback() {
        let work = || ConvertError::limit(Limit::Work, 7);
        let cases = [
            ("a value", Ok(1), Ok(1)),
            ("a malformed style", Err(ConvertError::malformed("style inheritance cycle")), Ok(0)),
            ("an encrypted package", Err(ConvertError::Encrypted), Err(ConvertError::Encrypted)),
            ("the work limit", Err(work()), Err(work())),
            ("a cancelled parse", Err(ConvertError::Cancelled), Err(ConvertError::Cancelled)),
            ("an internal fault", Err(ConvertError::Internal), Err(ConvertError::Internal)),
        ];
        for (label, result, expected) in cases {
            assert_eq!(or_when_malformed(result, || 0), expected, "{label}");
        }
    }

    #[test]
    fn a_copy_charges_its_text_before_each_child_and_its_attributes_and_name_after() {
        let parse_ctx = ParseCtx::default();
        let xml = r#"<w:a xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" w:x="vv">tt<w:b/>u</w:a>"#;
        let root = parse_xml(&parse_ctx, xml.as_bytes(), Role::Probe).unwrap();
        let source = root.child_elems().next().unwrap();
        let start = parse_ctx.fuel();
        let mut at_child = Vec::new();
        let copy = copy_with(&parse_ctx, source, |child| {
            at_child.push((child.local.clone(), parse_ctx.fuel() - start));
            Ok(None)
        })
        .unwrap();
        assert_eq!(at_child, [("b".to_owned(), 2)], "the text before the child is charged first");
        assert_eq!(
            parse_ctx.fuel() - start,
            2 + 1 + (1 + 2) + 1,
            "text, then attribute, then name"
        );
        assert_eq!(copy.children, [Node::Text("tt".into()), Node::Text("u".into())]);
    }
}
