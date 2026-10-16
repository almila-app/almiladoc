//! Block and inline walking for WordprocessingML parts.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::formats::docx::numbering::{Counters, Numbering};
use crate::formats::docx::styles::{Styles, on_off, rpr_delta};
use crate::model::{
    Block, Cell, GridBuilder, ImageSource, Inline, LinkTarget, Style, TableKind, inlines_are_empty,
};
use crate::package::Package;
use crate::package::archive::Role;
use crate::package::relationships::{
    RelTarget, Relationships, TargetMode, rel_target_bytes, rel_target_path,
};
use crate::package::xml::{Element, ns};
use crate::shared::blockstyle::{BlockStyle, StyledRun};
use crate::shared::delta::rebase_emphasis;
use crate::shared::fields::{FieldFrame, field_result};
use crate::shared::header::resolve_header_rows;
use crate::shared::link_label::label_is_absolute_url;
use crate::shared::list::{ListEntry, ListKey, flush_list};
use crate::shared::math::{omath_para_to_tex, omath_to_tex};
use crate::shared::text::clean_text;
use std::cell::RefCell;
use std::collections::HashMap;

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

use crate::shared::assets::AssetSink;

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
}

impl<'a, 'b> Ctx<'a, 'b> {
    /// The same document-wide dependencies scoped to another package part
    /// (notes parts): only the relationships, base path and region change.
    pub fn for_part(&self, rels: Relationships, base_part: String, region: Region) -> Ctx<'a, 'b> {
        Ctx {
            parse_ctx: self.parse_ctx,
            pkg: self.pkg,
            rels,
            base_part,
            region,
            styles: self.styles,
            numbering: self.numbering,
            numbering_reported: self.numbering_reported,
            counters: self.counters,
            assets: self.assets,
        }
    }

    /// Load an internal relationship target's bytes, resolved against this
    /// part: `None` when it cannot be loaded, whose loss the caller reports.
    fn rel_part(&self, rel_id: &str) -> Result<Option<RelTarget>, ConvertError> {
        rel_target_bytes(self.pkg, &self.rels, &self.base_part, rel_id)
    }

    fn add_asset(
        &self,
        media_type: String,
        part: String,
        bytes: &[u8],
    ) -> Result<crate::model::AssetId, ConvertError> {
        self.assets.borrow_mut().add(media_type, part, bytes)
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

    fn alternate_branch<'e>(&self, alt: &'e Element) -> Option<&'e Element> {
        crate::shared::mc::alternate_branch(self.parse_ctx, self.region, alt, SUPPORTED_NS, |_| {
            true
        })
    }
}

pub(super) enum ParaKind {
    Heading {
        level: u8,
        /// The visible number of a numbered heading (with its trailing
        /// separator), prepended to the content: headings have no native
        /// numbering in the output.
        label: Option<String>,
        /// The heading style's own emphasis, subtracted from its runs.
        base: Style,
    },
    ListItem {
        ilvl: usize,
        key: ListKey,
        number: u64,
        label: Option<String>,
    },
    /// A paragraph whose style names a block container.
    Styled(BlockStyle),
    Plain,
}

/// Block runs a following paragraph may extend: a list being built, and a
/// styled container. Only one is ever open, so starting either closes the
/// other. A paragraph whose mark is deleted waits to join the next one.
#[derive(Default)]
pub(super) struct Runs {
    list: Vec<ListEntry>,
    styled: StyledRun,
    joining: Option<(ParaKind, Vec<Piece>)>,
}

impl Runs {
    fn flush(&mut self, blocks: &mut Vec<Block>) {
        if let Some((kind, pieces)) = self.joining.take() {
            emit_paragraph(kind, pieces, blocks, self);
        }
        self.styled.flush(blocks);
        flush_list(blocks, &mut self.list);
    }
}

/// A paragraph's content in source order: inline runs interleaved with
/// block attachments (text boxes, charts) at their anchor positions.
pub(super) enum Piece {
    Inlines(Vec<Inline>),
    Blocks(Vec<Block>),
}

pub(super) fn parse_blocks(parent: &Element, ctx: &Ctx) -> Result<Vec<Block>, ConvertError> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut runs = Runs::default();
    collect_blocks(parent, ctx, &mut blocks, &mut runs)?;
    runs.flush(&mut blocks);
    Ok(blocks)
}

fn collect_blocks(
    parent: &Element,
    ctx: &Ctx,
    blocks: &mut Vec<Block>,
    runs: &mut Runs,
) -> Result<(), ConvertError> {
    for child in parent.child_elems() {
        if child.is(ns::MC, "AlternateContent") {
            if let Some(branch) = ctx.alternate_branch(child) {
                collect_blocks(branch, ctx, blocks, runs)?;
            }
            continue;
        }
        if child.is(ns::M, "oMathPara") || child.is(ns::M, "oMath") {
            runs.flush(blocks);
            blocks.extend(omath_para_to_tex(child).into_iter().map(Block::Math));
            continue;
        }
        if child.ns.as_deref().is_none_or(|n| n != ns::W) {
            continue;
        }
        match child.local.as_str() {
            "p" => {
                let (kind, mut pieces) = parse_paragraph(child, ctx)?;
                if let Some((_, held)) = runs.joining.take() {
                    pieces = joined(held, pieces);
                }
                if mark_deleted(child) {
                    runs.joining = Some((kind, pieces));
                } else {
                    emit_paragraph(kind, pieces, blocks, runs);
                }
            }
            "tbl" => {
                runs.flush(blocks);
                blocks.extend(parse_table(child, ctx)?);
            }
            "sdt" => {
                if let Some(content) = child.find(ns::W, "sdtContent") {
                    collect_blocks(content, ctx, blocks, runs)?;
                }
            }
            "customXml" => collect_blocks(child, ctx, blocks, runs)?,
            "altChunk" => ctx.emit(DiagnosticCode::ImportedPartUnread),
            _ => {}
        }
    }
    Ok(())
}

/// Whether a paragraph's mark is a tracked deletion, which joins it to the
/// next paragraph once the revisions are accepted.
fn mark_deleted(p: &Element) -> bool {
    p.find(ns::W, "pPr")
        .and_then(|pr| pr.find(ns::W, "rPr"))
        .and_then(|r| r.find(ns::W, "del"))
        .is_some()
}

/// A paragraph whose mark is deleted followed by the next, which keeps its
/// own properties: the inline runs where they meet become one.
fn joined(mut first: Vec<Piece>, second: Vec<Piece>) -> Vec<Piece> {
    for piece in second {
        if let Piece::Inlines(mut head) = piece {
            if let Some(Piece::Inlines(tail)) = first.last_mut() {
                tail.append(&mut head);
            } else {
                first.push(Piece::Inlines(head));
            }
        } else {
            first.push(piece);
        }
    }
    first
}

fn emit_paragraph(kind: ParaKind, pieces: Vec<Piece>, blocks: &mut Vec<Block>, runs: &mut Runs) {
    match kind {
        ParaKind::ListItem { ilvl, key, number, label } => {
            runs.styled.flush(blocks);
            let item = pieces_into_blocks(pieces);
            runs.list.push(ListEntry { level: ilvl, key, number, label, blocks: item });
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
                        if !emitted_heading {
                            if let Some(label) = label.take() {
                                content
                                    .insert(0, Inline::Text { text: label, style: Style::PLAIN });
                            }
                            blocks.push(Block::Heading { level, anchor: None, content });
                            emitted_heading = true;
                        } else {
                            // Attachments cannot be nested in a heading, so
                            // subsequent text becomes a paragraph.
                            blocks.push(Block::Paragraph(content));
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

fn parse_paragraph(p: &Element, ctx: &Ctx) -> Result<(ParaKind, Vec<Piece>), ConvertError> {
    let ppr = p.find(ns::W, "pPr");
    let pstyle_id = ppr.and_then(|pr| pr.find(ns::W, "pStyle")).and_then(|e| e.attr(ns::W, "val"));

    // Direct paragraph properties overlay the style chain: an explicit
    // outlineLvl of 9 ("no outline level") turns a style heading off.
    let direct_outline: Option<Option<u8>> = ppr
        .and_then(|pr| pr.find(ns::W, "outlineLvl"))
        .and_then(|e| e.attr(ns::W, "val"))
        .and_then(|v| v.parse::<u8>().ok())
        .map(|l| if l < 9 { Some(l + 1) } else { None });
    let style_heading = match pstyle_id {
        Some(id) => ctx.styles.heading_level(id)?,
        None => None,
    };
    let heading = direct_outline.or(style_heading).flatten();
    let style_block = match pstyle_id {
        Some(id) => ctx.styles.block_style(id)?,
        None => None,
    };

    // Numbering resolves independently of heading semantics: a numbered
    // heading advances its sequence and keeps its number visible.
    let numbering = if mark_deleted(p) { None } else { resolve_numbering(ppr, pstyle_id, ctx)? };

    // Toggle properties: the paragraph style chain's true-parity flips the
    // docDefaults base. Headings use the same resolution as body text.
    let parity = match pstyle_id {
        Some(id) => ctx.styles.run_toggles(id)?,
        None => Default::default(),
    };
    let paragraph_level = parity.apply_over(ctx.styles.doc_defaults);

    let kind = match heading {
        Some(level) => {
            let label = match &numbering {
                Some((_, key, number, label)) if key.marker.ordered() => {
                    Some(format!("{} ", label.clone().unwrap_or_else(|| key.marker.label(*number))))
                }
                _ => None,
            };
            ParaKind::Heading { level, label, base: paragraph_level }
        }
        None => match numbering {
            Some((ilvl, key, number, label)) => ParaKind::ListItem { ilvl, key, number, label },
            None => match style_block {
                Some(style) => ParaKind::Styled(style),
                None => ParaKind::Plain,
            },
        },
    };

    let mut walker = InlineWalker::new(ctx, paragraph_level);
    walker.walk(p)?;
    Ok((kind, walker.finish()))
}

/// Resolve a paragraph's effective numbering per ECMA-376: the direct
/// `numPr` children are tri-state and merge property-by-property with the
/// style-inherited `numPr` (a missing `numId`/`ilvl` inherits; an explicit
/// `numId` of 0 suppresses). Returns the level, list identity, effective
/// number, and composite label, advancing the instance counters.
#[expect(clippy::type_complexity)]
fn resolve_numbering(
    ppr: Option<&Element>,
    pstyle_id: Option<&str>,
    ctx: &Ctx,
) -> Result<Option<(usize, ListKey, u64, Option<String>)>, ConvertError> {
    let direct = ppr.and_then(|pr| pr.find(ns::W, "numPr"));
    let direct_num_id: Option<u64> = direct
        .and_then(|numpr| numpr.find(ns::W, "numId"))
        .and_then(|e| e.attr(ns::W, "val"))
        .and_then(|v| v.parse().ok());
    let direct_ilvl: Option<usize> = direct
        .and_then(|numpr| numpr.find(ns::W, "ilvl"))
        .and_then(|e| e.attr(ns::W, "val"))
        .and_then(|v| v.parse().ok());

    let num_id = match direct_num_id {
        Some(id) => Some(id),
        None => match pstyle_id {
            Some(id) => ctx.styles.style_num_pr(id)?,
            None => None,
        },
    };
    let Some(num_id) = num_id else {
        return Ok(None);
    };
    if num_id == 0 {
        // numId 0 is explicitly suppressed numbering.
        return Ok(None);
    }
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
        (None, Some(id)) => ctx.styles.style_numbering_level(id, instance)?.unwrap_or(0),
        (None, None) => 0,
    };
    let def = &instance.levels[ilvl.min(crate::formats::docx::numbering::LEVELS - 1)];
    // A numFmt of "none" is explicitly suppressed numbering.
    let Some(marker) = def.marker else {
        return Ok(None);
    };
    let (number, label) = if marker.ordered() {
        ctx.counters.borrow_mut().next(num_id, ilvl, instance)
    } else {
        (0, None)
    };
    Ok(Some((ilvl, ListKey { instance: num_id, marker }, number, label)))
}

struct InlineWalker<'a, 'b, 'e> {
    ctx: &'e Ctx<'a, 'b>,
    base: Style,
    pieces: Vec<Piece>,
    current: Vec<Inline>,
    fields: Vec<FieldFrame>,
}

impl<'a, 'b, 'e> InlineWalker<'a, 'b, 'e> {
    fn new(ctx: &'e Ctx<'a, 'b>, base: Style) -> Self {
        InlineWalker { ctx, base, pieces: Vec::new(), current: Vec::new(), fields: Vec::new() }
    }

    fn push(&mut self, inline: Inline) {
        match self.fields.last_mut() {
            Some(f) if f.in_result => f.inlines.push(inline),
            Some(_) => {}
            None => self.current.push(inline),
        }
    }

    /// Attach block content at the current position in run order.
    fn push_blocks(&mut self, blocks: Vec<Block>) {
        if blocks.is_empty() {
            return;
        }
        if !self.current.is_empty() {
            self.pieces.push(Piece::Inlines(std::mem::take(&mut self.current)));
        }
        self.pieces.push(Piece::Blocks(blocks));
    }

    fn walk(&mut self, elem: &Element) -> Result<(), ConvertError> {
        for child in elem.child_elems() {
            if child.is(ns::MC, "AlternateContent") {
                if let Some(branch) = self.ctx.alternate_branch(child) {
                    self.walk(branch)?;
                }
                continue;
            }
            if child.is(ns::M, "oMathPara") {
                // A math paragraph is displayed on its own line.
                self.push_blocks(omath_para_to_tex(child).into_iter().map(Block::Math).collect());
                continue;
            }
            if child.is(ns::M, "oMath") {
                let tex = omath_to_tex(child);
                if !tex.is_empty() {
                    self.push(Inline::Math(tex));
                }
                continue;
            }
            if child.ns.as_deref().is_none_or(|n| n != ns::W) {
                continue;
            }
            match child.local.as_str() {
                "pPr" => {}
                "r" => self.walk_run(child)?,
                "hyperlink" => {
                    let target = self.hyperlink_link_target(child);
                    let mut inner = InlineWalker::new(self.ctx, self.base);
                    inner.walk(child)?;
                    let (content, attachments) = split_pieces(inner.finish());
                    let unknown = target.is_none()
                        && child
                            .attr_qualified(ns::R, "id")
                            .is_some_and(|id| self.ctx.rels.get(id).is_none());
                    if unknown
                        && !label_is_absolute_url(&crate::model::inlines_to_plain_text(&content))
                    {
                        self.ctx.emit_related(DiagnosticCode::LinkTargetUnread);
                    }
                    if let Some(target) = target {
                        // An empty label still keeps a resolved target: the
                        // renderer shows the URL as the link text.
                        self.push(Inline::Link { content, target });
                    } else {
                        for inline in content {
                            self.push(inline);
                        }
                    }
                    self.push_blocks(attachments);
                }
                "fldSimple" => {
                    let instr = child.attr(ns::W, "instr").unwrap_or("").to_string();
                    let mut inner = InlineWalker::new(self.ctx, self.base);
                    inner.walk(child)?;
                    let (content, attachments) = split_pieces(inner.finish());
                    self.push_field_result(&instr, content);
                    self.push_blocks(attachments);
                }
                "bookmarkStart" => {
                    if let Some(name) = child.attr(ns::W, "name")
                        && name != "_GoBack"
                    {
                        self.push(Inline::Anchor(name.to_string()));
                    }
                }
                "sdt" => {
                    if let Some(content) = child.find(ns::W, "sdtContent") {
                        self.walk(content)?;
                    }
                }
                // `moveTo` is moved-in text — part of the final document;
                // `customXml` wraps ordinary run content.
                "smartTag" | "ins" | "bdo" | "dir" | "moveTo" | "customXml" => self.walk(child)?,
                "del" | "moveFrom" => self.ctx.emit(DiagnosticCode::RevisionOmitted),
                "subDoc" => self.ctx.emit(DiagnosticCode::ImportedPartUnread),
                _ => {}
            }
        }
        Ok(())
    }

    fn hyperlink_link_target(&self, link: &Element) -> Option<LinkTarget> {
        if let Some(id) = link.attr_qualified(ns::R, "id")
            && let Some(rel) = self.ctx.rels.get(id)
        {
            return Some(crate::shared::fields::classify_rel_target(
                rel.mode == TargetMode::External,
                &rel.target,
            ));
        }
        link.attr(ns::W, "anchor").map(|a| LinkTarget::Anchor(a.to_string()))
    }

    fn walk_run(&mut self, run: &Element) -> Result<(), ConvertError> {
        let style = match run.find(ns::W, "rPr") {
            Some(rpr) => {
                // Character-style chain: another toggle layer over the
                // paragraph-level value. Direct formatting is absolute.
                let char_parity = match rpr.find(ns::W, "rStyle").and_then(|e| e.attr(ns::W, "val"))
                {
                    Some(id) => self.ctx.styles.run_toggles(id)?,
                    None => Default::default(),
                };
                let with_char = char_parity.apply_over(self.base);
                rpr_delta(rpr).apply(with_char)
            }
            None => self.base,
        };
        self.walk_run_content(run, style)
    }

    fn walk_run_content(&mut self, run: &Element, style: Style) -> Result<(), ConvertError> {
        for child in run.child_elems() {
            if child.is(ns::MC, "AlternateContent") {
                if let Some(branch) = self.ctx.alternate_branch(child) {
                    self.walk_run_content(branch, style)?;
                }
                continue;
            }
            let in_w = child.ns.as_deref().is_some_and(|n| n == ns::W);
            if !in_w {
                continue;
            }
            match child.local.as_str() {
                "t" => {
                    // Run edges carry the spacing between words in documents
                    // that never mark xml:space, and XML leaves unmarked
                    // whitespace to the application, so it is kept.
                    let text = clean_text(child.text().as_ref());
                    if !text.is_empty() {
                        self.push(Inline::Text { text, style });
                    }
                }
                "tab" | "ptab" => self.push(Inline::Text { text: " ".into(), style: Style::PLAIN }),
                "noBreakHyphen" => self.push(Inline::Text { text: "-".into(), style }),
                "sym" => match symbol_checkbox(child) {
                    Some(checked) => self.push(Inline::Checkbox(checked)),
                    None => self.ctx.emit(DiagnosticCode::UnmappedSymbol),
                },
                "ruby" => self.ctx.emit(DiagnosticCode::RubyUnread),
                "contentPart" => self.ctx.emit(DiagnosticCode::GraphicUnread),
                "pgNum" | "dayShort" | "dayLong" | "monthShort" | "monthLong" | "yearShort"
                | "yearLong" => self.ctx.emit(DiagnosticCode::FieldValueUnread),
                // Markdown has no pages or columns, but every w:br still
                // separates the runs around it: dropping a page break
                // outright would join the words on either side. One left at
                // the end of a paragraph is trimmed when the block renders.
                "br" => self.push(Inline::LineBreak),
                "cr" => self.push(Inline::LineBreak),
                "footnoteReference" => {
                    if let Some(id) = child.attr(ns::W, "id") {
                        self.push(Inline::NoteRef(format!("fn{id}")));
                    }
                }
                "endnoteReference" => {
                    if let Some(id) = child.attr(ns::W, "id") {
                        self.push(Inline::NoteRef(format!("en{id}")));
                    }
                }
                "drawing" | "pict" | "object" => self.walk_drawing(child)?,
                "fldChar" => match child.attr(ns::W, "fldCharType") {
                    Some("begin") => {
                        if child.find(ns::W, "ffData").is_some_and(|data| {
                            data.find(ns::W, "checkBox").is_some()
                                || data.find(ns::W, "ddList").is_some()
                        }) {
                            self.ctx.emit(DiagnosticCode::FormControlUnread);
                        }
                        self.fields.push(FieldFrame::default());
                    }
                    Some("separate") => {
                        if let Some(f) = self.fields.last_mut() {
                            f.in_result = true;
                        }
                    }
                    Some("end") => {
                        if let Some(FieldFrame { instr, inlines, .. }) = self.fields.pop() {
                            self.push_field_result(&instr, inlines);
                        }
                    }
                    _ => {}
                },
                "instrText" => {
                    if let Some(f) = self.fields.last_mut() {
                        f.instr.push_str(&child.text());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Drawings, VML picts, and embedded objects: text boxes become block
    /// attachments at this position, and otherwise the first chart, else
    /// diagram, else object, else picture is read. Every other graphic and
    /// any shape text is reported, an object's own preview picture excepted.
    fn walk_drawing(&mut self, elem: &Element) -> Result<(), ConvertError> {
        let mut contents = Contents::default();
        contents.collect(elem, self.ctx, None);
        let Contents { boxes, graphics, shape_text } = contents;
        let boxed = !boxes.is_empty();
        let mut blocks = Vec::new();
        for tb in boxes {
            blocks.extend(parse_blocks(tb, self.ctx)?);
        }
        self.push_blocks(blocks);
        if shape_text {
            self.ctx.emit(DiagnosticCode::GraphicUnread);
        }
        let read =
            [Graphic::is_chart, Graphic::is_diagram, Graphic::is_object, Graphic::is_picture]
                .iter()
                .find_map(|kind| graphics.iter().position(kind))
                .filter(|_| !boxed);
        let previews: Vec<&str> = graphics
            .iter()
            .filter_map(|graphic| match graphic {
                Graphic::Object(ole) => ole.attr_unqualified("ShapeID"),
                _ => None,
            })
            .collect();
        for (index, graphic) in graphics.iter().enumerate() {
            if Some(index) == read {
                continue;
            }
            match graphic {
                Graphic::Chart(_) | Graphic::Diagram(_) | Graphic::WordArt => {
                    self.ctx.emit(DiagnosticCode::GraphicUnread)
                }
                Graphic::Object(_) => self.ctx.emit(DiagnosticCode::EmbeddedObject),
                Graphic::Picture { shape: Some(shape), .. } if previews.contains(shape) => {}
                Graphic::Picture { .. } => self.ctx.emit(DiagnosticCode::ImageUnplaced),
            }
        }
        if boxed {
            return Ok(());
        }

        let descr = elem
            .first_descendant(ns::WP, "docPr")
            .and_then(|d| d.attr(ns::WP, "descr"))
            .map(clean_text)
            .unwrap_or_default();
        match read.and_then(|index| graphics.get(index)) {
            Some(Graphic::Chart(Some(rel_id))) => {
                let blocks = self.chart_blocks(rel_id)?;
                self.push_blocks(blocks);
            }
            Some(Graphic::Diagram(Some(rel_id))) => {
                let blocks = self.diagram_blocks(rel_id)?;
                self.push_blocks(blocks);
            }
            Some(Graphic::Object(ole)) => self.embedded_object(ole, descr)?,
            Some(Graphic::Picture { rel_id, .. }) => self.picture(*rel_id, descr)?,
            _ => {
                if !descr.trim().is_empty() {
                    self.push(Inline::Image { alt: descr, source: ImageSource::Unavailable });
                }
            }
        }
        Ok(())
    }

    /// An embedded OLE object as its payload asset; its content is not
    /// converted.
    fn embedded_object(&mut self, ole: &Element, descr: String) -> Result<(), ConvertError> {
        self.ctx.emit(DiagnosticCode::EmbeddedObject);
        let prog_id = ole.attr(ns::O_VML, "ProgID").unwrap_or("object").to_string();
        let alt =
            if descr.trim().is_empty() { format!("Embedded object: {prog_id}") } else { descr };
        let source = match ole.attr_qualified(ns::R, "id") {
            Some(rel_id) => match self.ctx.rel_part(rel_id)? {
                Some((part, bytes)) => Some(ImageSource::Asset(self.ctx.add_asset(
                    "application/vnd.ms-ole-object".into(),
                    part,
                    &bytes,
                )?)),
                None => None,
            },
            None => None,
        };
        self.push(Inline::Image { alt, source: source.unwrap_or(ImageSource::Unavailable) });
        Ok(())
    }

    /// A picture: an external-mode target (`r:link`) becomes an external
    /// source, and an embedded one is retained as an asset.
    fn picture(&mut self, rel_id: Option<&str>, descr: String) -> Result<(), ConvertError> {
        let source = match rel_id {
            Some(rel_id) => crate::shared::assets::rel_image_source(
                self.ctx.pkg,
                &self.ctx.rels,
                &self.ctx.base_part,
                self.ctx.assets,
                rel_id,
            )?,
            None => None,
        };
        match source {
            Some(source) => self.push(Inline::Image { alt: descr, source }),
            None => {
                if rel_id.is_some() {
                    self.ctx.emit_related(DiagnosticCode::ImageUnavailable);
                } else {
                    self.ctx.emit(DiagnosticCode::ImageUnavailable);
                }
                if !descr.trim().is_empty() {
                    self.push(Inline::Image { alt: descr, source: ImageSource::Unavailable });
                }
            }
        }
        Ok(())
    }

    /// Textual extraction of a chart part via `shared::drawingml`.
    fn chart_blocks(&self, rel_id: &str) -> Result<Vec<Block>, ConvertError> {
        let parse_ctx = self.ctx.parse_ctx;
        let region = self.ctx.region;
        self.graphic_blocks(rel_id, ns::CHART, "chartSpace", |root| {
            crate::shared::drawingml::chart_blocks(parse_ctx, root, region)
        })
    }

    /// Textual extraction of a SmartArt data part via `shared::drawingml`.
    fn diagram_blocks(&self, rel_id: &str) -> Result<Vec<Block>, ConvertError> {
        self.graphic_blocks(rel_id, ns::DGM, "dataModel", crate::shared::drawingml::diagram_blocks)
    }

    /// The blocks `extract` makes of a chart or diagram part rooted at
    /// `namespace:root`; one that is absent, does not parse or is rooted
    /// elsewhere is reported.
    fn graphic_blocks(
        &self,
        rel_id: &str,
        namespace: &str,
        root: &str,
        extract: impl FnOnce(&Element) -> Vec<Block>,
    ) -> Result<Vec<Block>, ConvertError> {
        let Some(path) = rel_target_path(&self.ctx.rels, &self.ctx.base_part, rel_id) else {
            self.ctx.emit_related(DiagnosticCode::GraphicUnread);
            return Ok(Vec::new());
        };
        match self.ctx.pkg.related_xml_tree(&path, Role::Reported(self.ctx.region))? {
            Some(tree) if tree.find(namespace, root).is_some() => Ok(extract(&tree)),
            Some(_) | None => {
                self.ctx.emit(DiagnosticCode::GraphicUnread);
                Ok(Vec::new())
            }
        }
    }

    fn push_field_result(&mut self, instr: &str, content: Vec<Inline>) {
        for inline in field_result(instr, content) {
            self.push(inline);
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

/// The checkbox state a `w:sym` draws, for the Wingdings fonts' ballot-box glyphs only.
fn symbol_checkbox(sym: &Element) -> Option<bool> {
    let font = sym.attr(ns::W, "font")?;
    let hex = sym.attr(ns::W, "char")?;
    if hex.len() > 4 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let code = match u16::from_str_radix(hex, 16).ok()? {
        // Word writes a symbol-font code either as the byte or in the U+F000 private-use block.
        private @ 0xF000..=0xF0FF => private - 0xF000,
        code => code,
    };
    if font.eq_ignore_ascii_case("Wingdings 2") {
        match code {
            0x52 => Some(true),
            0xA3 => Some(false),
            _ => None,
        }
    } else if font.eq_ignore_ascii_case("Wingdings") {
        match code {
            0xFE => Some(true),
            0x6F => Some(false),
            _ => None,
        }
    } else {
        None
    }
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
    Object(&'e Element),
    /// A picture, with the id of the VML shape that holds it.
    Picture {
        rel_id: Option<&'e str>,
        shape: Option<&'e str>,
    },
    WordArt,
}

impl Graphic<'_> {
    fn is_chart(&self) -> bool {
        matches!(self, Graphic::Chart(Some(_)))
    }

    fn is_diagram(&self) -> bool {
        matches!(self, Graphic::Diagram(Some(_)))
    }

    fn is_object(&self) -> bool {
        matches!(self, Graphic::Object(_))
    }

    fn is_picture(&self) -> bool {
        matches!(self, Graphic::Picture { .. })
    }
}

/// What a drawing or VML pict holds, in document order: text-box content
/// (`w:txbxContent`), whose own drawings its blocks walk, and the graphics
/// and shape text outside it, through the branch each
/// `mc:AlternateContent` takes.
#[derive(Default)]
struct Contents<'e> {
    boxes: Vec<&'e Element>,
    graphics: Vec<Graphic<'e>>,
    shape_text: bool,
}

impl<'e> Contents<'e> {
    fn collect(&mut self, elem: &'e Element, ctx: &Ctx, shape: Option<&'e str>) {
        for child in elem.child_elems() {
            if child.is(ns::MC, "AlternateContent") {
                if let Some(branch) = ctx.alternate_branch(child) {
                    self.collect(branch, ctx, shape);
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
            let shape =
                if child.is(ns::VML, "shape") { child.attr_unqualified("id") } else { shape };
            self.collect(child, ctx, shape);
        }
    }
}

fn graphic<'e>(elem: &'e Element, shape: Option<&'e str>) -> Option<Graphic<'e>> {
    let rel_id = |name| elem.attr_qualified(ns::R, name);
    Some(match elem.ns.as_deref()? {
        ns::CHART if elem.local == "chart" => Graphic::Chart(rel_id("id")),
        ns::DGM if elem.local == "relIds" => Graphic::Diagram(rel_id("dm")),
        ns::O_VML if elem.local == "OLEObject" => Graphic::Object(elem),
        ns::A if elem.local == "blip" => {
            Graphic::Picture { rel_id: rel_id("embed").or_else(|| rel_id("link")), shape: None }
        }
        ns::VML if elem.local == "imagedata" => Graphic::Picture { rel_id: rel_id("id"), shape },
        ns::VML if elem.local == "textpath" => {
            elem.attr_unqualified("string").filter(|text| !text.trim().is_empty())?;
            Graphic::WordArt
        }
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Tables

struct TcInfo<'e> {
    elem: Option<&'e Element>,
    /// Legacy `hMerge` continuation cells folded into this origin.
    merged: Vec<&'e Element>,
    col_span: usize,
    row_span: usize,
    /// vMerge continuation: this position belongs to the origin above.
    covered: bool,
}

impl TcInfo<'_> {
    /// Empty single-column filler for `gridBefore`/`gridAfter` positions.
    fn filler() -> Self {
        TcInfo { elem: None, merged: Vec::new(), col_span: 1, row_span: 1, covered: false }
    }
}

/// A `w:trPr` grid filler count (`gridBefore`/`gridAfter`).
fn grid_filler(trpr: Option<&Element>, name: &str) -> usize {
    trpr.and_then(|p| p.find(ns::W, name))
        .and_then(|e| e.attr(ns::W, "val"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        .min(1000)
}

pub(super) fn parse_table(tbl: &Element, ctx: &Ctx) -> Result<Vec<Block>, ConvertError> {
    // Collect the raw cell matrix first so vertical merges can be resolved
    // into row spans before the grid is built. gridBefore/gridAfter filler
    // materializes as empty cells so every cell keeps its grid column.
    let rows = collect_rows(tbl, ctx);
    let mut matrix: Vec<Vec<TcInfo>> = Vec::new();
    for &tr in &rows {
        let trpr = tr.find(ns::W, "trPr");
        let mut row = Vec::new();
        row.extend((0..grid_filler(trpr, "gridBefore")).map(|_| TcInfo::filler()));
        collect_row_cells(tr, ctx, &mut row);
        row.extend((0..grid_filler(trpr, "gridAfter")).map(|_| TcInfo::filler()));
        matrix.push(row);
    }
    // Active vertical-merge chains by grid column.
    let mut active: HashMap<usize, (usize, usize)> = HashMap::new();
    let rows_len = matrix.len();
    for r in 0..rows_len {
        let mut col = 0usize;
        let mut next_active: Vec<(usize, (usize, usize))> = Vec::new();
        for i in 0..matrix[r].len() {
            let (covered, span) = (matrix[r][i].covered, matrix[r][i].col_span);
            if covered {
                if let Some(&(orow, oidx)) = active.get(&col) {
                    matrix[orow][oidx].row_span += 1;
                    for c in col..col + span {
                        next_active.push((c, (orow, oidx)));
                    }
                } else {
                    matrix[r][i].covered = false; // stray continuation
                }
            } else {
                for c in col..col + span {
                    next_active.push((c, (r, i)));
                }
            }
            col += span;
        }
        active = next_active.into_iter().collect();
    }

    // tblHeader is ST_OnOff: an explicit false value is not a header row.
    let header_rows = rows
        .iter()
        .take_while(|tr| tr.find(ns::W, "trPr").and_then(|p| on_off(p, "tblHeader")) == Some(true))
        .count();

    let mut builder = GridBuilder::new(ctx.parse_ctx.limits().max_expansion);
    for row in &matrix {
        builder.next_row();
        for tc in row {
            if tc.covered {
                for _ in 0..tc.col_span {
                    builder.covered();
                }
            } else {
                let mut blocks = match tc.elem {
                    Some(elem) => parse_blocks(elem, ctx)?,
                    None => Vec::new(),
                };
                for merged in &tc.merged {
                    blocks.extend(parse_blocks(merged, ctx)?);
                }
                builder.place(Cell::spanning(blocks, tc.col_span as u32, tc.row_span as u32))?;
            }
        }
    }
    let mut table = builder.finish(TableKind::Data);
    if table.grid.is_empty() {
        return Ok(Vec::new());
    }
    table.header_rows = resolve_header_rows(&table, header_rows);
    Ok(vec![Block::Table(table)])
}

/// A table's rows: a deleted row is reported and left out, as accepting the
/// revisions removes it, and rows behind a content control, custom XML or
/// alternate content are reported unread.
fn collect_rows<'e>(tbl: &'e Element, ctx: &Ctx) -> Vec<&'e Element> {
    let mut rows = Vec::new();
    for child in tbl.child_elems() {
        if !child.is(ns::W, "tr") {
            if child.first_descendant(ns::W, "tr").is_some() {
                ctx.emit(DiagnosticCode::BlockUnread);
            }
        } else if child.find(ns::W, "trPr").and_then(|p| p.find(ns::W, "del")).is_some() {
            ctx.emit(DiagnosticCode::RevisionOmitted);
        } else {
            rows.push(child);
        }
    }
    rows
}

fn collect_row_cells<'e>(parent: &'e Element, ctx: &Ctx, cells: &mut Vec<TcInfo<'e>>) {
    for child in parent.child_elems() {
        if child.is(ns::MC, "AlternateContent") {
            if child.first_descendant(ns::W, "tc").is_some() {
                ctx.emit(DiagnosticCode::BlockUnread);
            }
            continue;
        }
        if child.ns.as_deref().is_none_or(|n| n != ns::W) {
            continue;
        }
        match child.local.as_str() {
            "tc" if child.find(ns::W, "tcPr").and_then(|p| p.find(ns::W, "cellDel")).is_some() => {
                ctx.emit(DiagnosticCode::RevisionOmitted)
            }
            "tc" => {
                let tcpr = child.find(ns::W, "tcPr");
                let covered = tcpr
                    .and_then(|p| p.find(ns::W, "vMerge"))
                    .is_some_and(|v| !matches!(v.attr(ns::W, "val"), Some("restart")));
                let col_span: usize = tcpr
                    .and_then(|p| p.find(ns::W, "gridSpan"))
                    .and_then(|e| e.attr(ns::W, "val"))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1)
                    .clamp(1, 1000);
                // Legacy hMerge: a non-restart hMerge cell folds into the
                // preceding origin, widening its span.
                let hmerge_cont = tcpr
                    .and_then(|p| p.find(ns::W, "hMerge"))
                    .is_some_and(|h| !matches!(h.attr(ns::W, "val"), Some("restart")));
                if hmerge_cont
                    && let Some(prev) = cells.last_mut()
                    && !prev.covered
                {
                    prev.col_span += col_span;
                    prev.merged.push(child);
                    continue;
                }
                cells.push(TcInfo {
                    elem: Some(child),
                    merged: Vec::new(),
                    col_span,
                    row_span: 1,
                    covered,
                });
            }
            "sdt" => {
                if let Some(content) = child.find(ns::W, "sdtContent") {
                    collect_row_cells(content, ctx, cells);
                }
            }
            "customXml" => collect_row_cells(child, ctx, cells),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MarkerKind;

    fn text(value: &str) -> Piece {
        Piece::Inlines(vec![Inline::plain(value)])
    }

    fn assert_text(block: &Block, expected: &str) {
        let Block::Paragraph(inlines) = block else { panic!("expected paragraph: {block:?}") };
        assert_eq!(crate::model::inlines_to_plain_text(inlines), expected);
    }

    #[test]
    fn heading_attachments_keep_source_order() {
        let mut blocks = Vec::new();
        emit_paragraph(
            ParaKind::Heading { level: 2, label: None, base: Style::PLAIN },
            vec![text("before"), Piece::Blocks(vec![Block::Rule]), text("after")],
            &mut blocks,
            &mut Runs::default(),
        );
        let [Block::Heading { content, .. }, Block::Rule, after] = &blocks[..] else {
            panic!("unexpected blocks: {blocks:?}")
        };
        assert_eq!(crate::model::inlines_to_plain_text(content), "before");
        assert_text(after, "after");
    }

    #[test]
    fn list_attachments_keep_source_order_inside_the_item() {
        let mut blocks = Vec::new();
        let mut runs = Runs::default();
        emit_paragraph(
            ParaKind::ListItem {
                ilvl: 0,
                key: ListKey { instance: 1, marker: MarkerKind::Bullet },
                number: 0,
                label: None,
            },
            vec![text("before"), Piece::Blocks(vec![Block::Rule]), text("after")],
            &mut blocks,
            &mut runs,
        );
        runs.flush(&mut blocks);
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
        let mut blocks = Vec::new();
        emit_paragraph(
            ParaKind::Plain,
            vec![Piece::Inlines(vec![Inline::Anchor("mark".into())])],
            &mut blocks,
            &mut Runs::default(),
        );
        let [Block::Paragraph(inlines)] = &blocks[..] else { panic!("{blocks:?}") };
        assert!(matches!(inlines[..], [Inline::Anchor(_)]), "{inlines:?}");
    }

    #[test]
    fn an_empty_list_item_carries_no_blocks() {
        let mut blocks = Vec::new();
        let mut runs = Runs::default();
        emit_paragraph(
            ParaKind::ListItem {
                ilvl: 0,
                key: ListKey { instance: 1, marker: MarkerKind::Bullet },
                number: 0,
                label: None,
            },
            Vec::new(),
            &mut blocks,
            &mut runs,
        );
        runs.flush(&mut blocks);
        let [Block::List(list)] = &blocks[..] else { panic!("{blocks:?}") };
        assert!(list.items[0].blocks.is_empty(), "{:?}", list.items[0].blocks);
    }

    #[test]
    fn attachments_split_styled_runs_in_place() {
        let mut blocks = Vec::new();
        let mut runs = Runs::default();
        emit_paragraph(
            ParaKind::Styled(BlockStyle::Code),
            vec![text("before"), Piece::Blocks(vec![Block::Rule]), text("after")],
            &mut blocks,
            &mut runs,
        );
        runs.flush(&mut blocks);
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
}
