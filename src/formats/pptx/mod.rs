//! OOXML PresentationML (.pptx / .pptm / .ppsx): slides in `sldIdLst` order,
//! with the full text cascade - slide -> layout -> master placeholder /
//! `txStyles` -> presentation defaults. Speaker notes are included (fixed
//! policy), rendered as a quote after each slide's content.

mod cascade;

use crate::ctx::{AdmittedUnit, ImageLoss, ParseCtx};
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{
    Block, Cell, Document, GridBuilder, ImageSource, Inline, LinkTarget, Unit, UnitKind,
    inlines_are_empty, inlines_to_plain_text,
};
use crate::package::archive::{Role, open_ooxml, report};
use crate::package::relationships::{
    Relationship, Relationships, TargetMode, read_main_part, read_rels, rels_part_for,
};
use crate::package::xml::{Element, ns};
use crate::package::{Package, path};
use crate::shared::assets::{AssetSink, rel_image_source, retain_ole_payload};
use crate::shared::delta::rebase_emphasis;
use crate::shared::drawingml::{self, GraphicPart};
use crate::shared::fields::classify_rel_target;
use crate::shared::header::resolve_header_rows;
use crate::shared::link_label::label_is_absolute_url;
use crate::shared::list::{ListEntry, ListKey, ListMark, MarkerKind, flush_list};
use crate::shared::math::{math_lines, omath_para_to_tex};
use crate::shared::mc::{alternate_branch, holds_graphic};
use crate::shared::numbering::{LevelCounters, every_deeper};
use crate::shared::text::clean_text;
use crate::shared::typeface::run_inlines;
use cascade::{Bullet, LevelStyle, Ph, Placeholder, TextProps, TitleClass};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

const MAIN_PART: &str = "ppt/presentation.xml";
const LAYOUT_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout";
const MASTER_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster";
const NOTES_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide";
const THEME_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme";
const COMMENT_RELS: &[&str] = &[
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments",
    "http://schemas.microsoft.com/office/2018/10/relationships/comments",
];
const P14: &str = "http://schemas.microsoft.com/office/powerpoint/2010/main";
const TEXT_BODY_LIST: u64 = 0;
const MEDIA: &[&str] = &["videoFile", "audioFile", "wavAudioFile", "quickTimeFile", "audioCd"];
const FOOTERS: &[&str] = &["sldNum", "dt", "ftr"];
/// `hlinkClick` actions that carry their destination themselves; any other
/// destination is named by relationship.
const NAVIGATION: &[&str] = &["ppaction://hlinkshowjump", "ppaction://customshow"];

/// Namespaces whose markup this frontend understands; `mc:Choice` branches
/// requiring anything else fall back to `mc:Fallback`.
const SUPPORTED_NS: &[&str] = &[ns::P, ns::A, ns::R, ns::MC, ns::A14];

struct LayoutInfo {
    placeholders: Vec<Placeholder>,
    master_path: Option<String>,
    backdrop: Backdrop,
}

struct MasterInfo {
    title: LevelStyle,
    body: LevelStyle,
    other: LevelStyle,
    placeholders: Vec<Placeholder>,
    backdrop: Backdrop,
    theme: ThemeFills,
}

/// What a slide, layout or master shows besides its placeholders.
struct Backdrop {
    shapes: bool,
    shows_master: bool,
    background: Option<Background>,
}

#[derive(Clone, Copy)]
enum Background {
    Picture,
    Theme(u32),
    Fill,
}

impl Backdrop {
    fn of(root: Option<&Element>) -> Self {
        let c_sld = root.and_then(|r| r.find(ns::P, "cSld"));
        let shapes = c_sld.and_then(|c| c.find(ns::P, "spTree")).is_some_and(|tree| {
            tree.child_elems().any(|shape| {
                !matches!(shape.local.as_str(), "nvGrpSpPr" | "grpSpPr" | "extLst")
                    && shape.first_descendant(ns::P, "ph").is_none()
            })
        });
        let background =
            c_sld.and_then(|c| c.find(ns::P, "bg")).map(|bg| match bg.find(ns::P, "bgRef") {
                Some(r) => Background::Theme(
                    r.attr(ns::P, "idx").and_then(|v| v.parse().ok()).unwrap_or(0),
                ),
                None if bg
                    .find(ns::P, "bgPr")
                    .is_some_and(|pr| pr.find(ns::A, "blipFill").is_some()) =>
                {
                    Background::Picture
                }
                None => Background::Fill,
            });
        let shows_master = !root
            .and_then(|r| r.attr(ns::P, "showMasterSp"))
            .is_some_and(|v| matches!(v, "0" | "false"));
        Self { shapes, shows_master, background }
    }
}

/// Which theme fill styles are pictures, by the index `p:bgRef` gives them:
/// `1..=999` into the fill styles, `1001..` into the background fills.
#[derive(Default)]
struct ThemeFills {
    fills: Vec<bool>,
    backgrounds: Vec<bool>,
}

impl ThemeFills {
    fn read(theme: &Element) -> Self {
        let pictures = |list: &str| -> Vec<bool> {
            theme
                .first_descendant(ns::A, list)
                .map(|l| l.child_elems().map(|fill| fill.is(ns::A, "blipFill")).collect())
                .unwrap_or_default()
        };
        Self { fills: pictures("fillStyleLst"), backgrounds: pictures("bgFillStyleLst") }
    }

    fn picture(&self, idx: u32) -> bool {
        let (list, first) = if idx >= 1001 { (&self.backgrounds, 1001) } else { (&self.fills, 1) };
        idx.checked_sub(first)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| list.get(i))
            .copied()
            .unwrap_or(false)
    }
}

pub(super) fn parse(parse_ctx: &ParseCtx<'_>, bytes: &[u8]) -> Result<Document, ConvertError> {
    let pkg = open_ooxml(parse_ctx, bytes)?;

    let pres_part = read_main_part(parse_ctx, &pkg, &[MAIN_PART])?
        .ok_or_else(|| ConvertError::missing_part(MAIN_PART))?;
    let pres = pkg.required_xml_part(&pres_part, Role::Content(Region::Body))?;
    let pres_rels = read_rels(&pkg, &rels_part_for(&pres_part), Role::Content(Region::Body))?;

    let default_text =
        cascade::parse_level_styles(pres.first_descendant(ns::P, "defaultTextStyle"));

    let slides = listed_slides(parse_ctx, &pres, &pres_part, &pres_rels)?;

    let assets = RefCell::new(AssetSink::new(parse_ctx));
    let mut templates = Templates::default();
    let mut blocks: Vec<Block> = Vec::new();
    let mut failed = 0usize;
    let slide_units: HashMap<&str, Unit> =
        slides.iter().map(|(unit, path)| (path.as_str(), unit.model())).collect();
    let mut all_rels: Vec<Relationships> = Vec::with_capacity(slides.len());
    for (unit, path) in &slides {
        let _unit = parse_ctx.enter_admitted(*unit);
        all_rels.push(read_rels(&pkg, &rels_part_for(path), Role::Content(Region::Body))?);
    }

    for ((unit, slide_path), slide_rels) in slides.iter().zip(&all_rels) {
        let _unit = parse_ctx.enter_admitted(*unit);
        let tree = pkg.related_xml_part(slide_path, Role::Reported(Region::Body))?;
        let slide = tree.as_ref().and_then(|t| t.find(ns::P, "sld"));
        let c_sld = slide.and_then(|s| s.find(ns::P, "cSld"));
        let Some(sp_tree) = c_sld.and_then(|c| c.find(ns::P, "spTree")) else {
            parse_ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body);
            failed += 1;
            continue;
        };

        let layout_path = rel_target_of_type(slide_rels, slide_path, LAYOUT_REL);
        let (layout, master) =
            templates.of_slide(parse_ctx, &pkg, slide, layout_path.as_deref())?;
        report_comments_and_controls(parse_ctx, slide_rels, c_sld);

        let ctx = SlideCtx {
            parse_ctx,
            pkg: &pkg,
            rels: slide_rels,
            base_part: slide_path,
            region: Region::Body,
            assets: &assets,
            default_text: &default_text,
            layout,
            master,
            slide_units: &slide_units,
        };
        blocks.push(Block::UnitStart(unit.model()));
        parse_shapes(sp_tree, &ctx, &mut blocks)?;
        blocks.extend(ctx.speaker_notes()?);
    }
    if failed == slides.len() {
        return Err(ConvertError::malformed("no slide in the presentation could be read"));
    }

    let assets = std::mem::take(&mut assets.borrow_mut().assets);
    Ok(Document { blocks, notes: Vec::new(), assets })
}

fn listed_slides(
    parse_ctx: &ParseCtx<'_>,
    pres: &Element,
    pres_part: &str,
    pres_rels: &Relationships,
) -> Result<Vec<(AdmittedUnit, String)>, ConvertError> {
    let mut slides = Vec::new();
    let mut listed_parts: HashSet<String> = HashSet::new();
    let listed = pres
        .first_descendant(ns::P, "sldIdLst")
        .into_iter()
        .flat_map(|l| l.find_all(ns::P, "sldId"));
    for (position, sld_id) in listed.enumerate() {
        let target = sld_id
            .attr_qualified(ns::R, "id")
            .and_then(|rid| pres_rels.internal_target(rid))
            .and_then(|target| path::resolve(pres_part, target).ok());
        match target {
            Some(t) if !listed_parts.insert(t.path.to_ascii_lowercase()) => {}
            Some(t) => {
                slides.push((parse_ctx.begin_unit(UnitKind::Slide, position)?.unit(), t.path));
            }
            None if pres_rels.reported() => {}
            None => {
                let _unit = parse_ctx.begin_unit(UnitKind::Slide, position)?;
                parse_ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body);
            }
        }
    }
    if slides.is_empty() {
        return Err(ConvertError::malformed_part(pres_part, "presentation has no slide list"));
    }
    Ok(slides)
}

#[derive(Default)]
struct Templates {
    layouts: HashMap<String, LayoutInfo>,
    masters: HashMap<String, MasterInfo>,
    repeated: HashSet<String>,
}

impl Templates {
    fn of_slide(
        &mut self,
        parse_ctx: &ParseCtx<'_>,
        pkg: &Package<'_>,
        slide: Option<&Element>,
        layout_path: Option<&str>,
    ) -> Result<(Option<&LayoutInfo>, Option<&MasterInfo>), ConvertError> {
        if let Some(lp) = layout_path
            && !self.layouts.contains_key(lp)
        {
            let _shared = parse_ctx.outside_units();
            let info = load_layout(parse_ctx, pkg, lp)?;
            if let Some(mp) = info.master_path.clone()
                && !self.masters.contains_key(&mp)
            {
                let m = load_master(parse_ctx, pkg, &mp)?;
                self.masters.insert(mp, m);
            }
            self.layouts.insert(lp.to_owned(), info);
        }
        let layout = layout_path.and_then(|lp| self.layouts.get_key_value(lp));
        let master = layout
            .and_then(|(_, l)| l.master_path.as_deref())
            .and_then(|mp| self.masters.get_key_value(mp));
        report_repeated(parse_ctx, slide, layout, master, &mut self.repeated);
        Ok((layout.map(|(_, l)| l), master.map(|(_, m)| m)))
    }
}

fn report_comments_and_controls(
    parse_ctx: &ParseCtx<'_>,
    slide_rels: &Relationships,
    c_sld: Option<&Element>,
) {
    if slide_rels.iter().any(|(_, r)| COMMENT_RELS.contains(&r.rel_type.as_str())) {
        parse_ctx.emit(DiagnosticCode::CommentOmitted, Region::Comment);
    }
    if let Some(controls) = c_sld.and_then(|c| c.find(ns::P, "controls")) {
        let count = selected_children(parse_ctx, Region::Body, controls).len();
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        parse_ctx.emit_count(DiagnosticCode::FormControlUnread, Region::Body, count);
    }
}

fn rel_target_of_type(rels: &Relationships, base: &str, rel_type: &str) -> Option<String> {
    rels.first_of_type(rel_type)
        .and_then(|rel| path::resolve(base, &rel.target).ok())
        .map(|t| t.path)
}

/// A related part's tree when it parses and its root is `p:{root}`; one
/// rooted elsewhere is reported by `role`.
fn rooted_part(
    parse_ctx: &ParseCtx<'_>,
    pkg: &Package<'_>,
    part: &str,
    role: Role,
    root: &str,
) -> Result<Option<Element>, ConvertError> {
    match pkg.related_xml_part(part, role)? {
        Some(tree) if tree.find(ns::P, root).is_none() => {
            report(parse_ctx, role.unreadable());
            Ok(None)
        }
        tree => Ok(tree),
    }
}

fn load_layout(
    parse_ctx: &ParseCtx<'_>,
    pkg: &Package<'_>,
    layout_path: &str,
) -> Result<LayoutInfo, ConvertError> {
    let tree = rooted_part(parse_ctx, pkg, layout_path, Role::Content(Region::Body), "sldLayout")?;
    let backdrop = Backdrop::of(tree.as_ref().and_then(|t| t.find(ns::P, "sldLayout")));
    let rels = read_rels(pkg, &rels_part_for(layout_path), Role::Content(Region::Body))?;
    let master_path = rel_target_of_type(&rels, layout_path, MASTER_REL);
    Ok(LayoutInfo { placeholders: placeholders(tree.as_ref()), master_path, backdrop })
}

fn placeholders(tree: Option<&Element>) -> Vec<Placeholder> {
    tree.and_then(|t| t.first_descendant(ns::P, "spTree"))
        .map(cascade::collect_placeholders)
        .unwrap_or_default()
}

fn load_master(
    parse_ctx: &ParseCtx<'_>,
    pkg: &Package<'_>,
    master_path: &str,
) -> Result<MasterInfo, ConvertError> {
    let tree = rooted_part(parse_ctx, pkg, master_path, Role::Content(Region::Body), "sldMaster")?;
    let rels = read_rels(pkg, &rels_part_for(master_path), Role::Probe)?;
    let theme = match rel_target_of_type(&rels, master_path, THEME_REL) {
        Some(theme_path) => pkg.related_xml_part(&theme_path, Role::Probe)?,
        None => None,
    };
    let styles = |name: &str| {
        let tx_styles = tree.as_ref().and_then(|t| t.first_descendant(ns::P, "txStyles"));
        cascade::parse_level_styles(tx_styles.and_then(|tx| tx.find(ns::P, name)))
    };
    Ok(MasterInfo {
        title: styles("titleStyle"),
        body: styles("bodyStyle"),
        other: styles("otherStyle"),
        placeholders: placeholders(tree.as_ref()),
        backdrop: Backdrop::of(tree.as_ref().and_then(|t| t.find(ns::P, "sldMaster"))),
        theme: theme.as_ref().map(ThemeFills::read).unwrap_or_default(),
    })
}

/// Report what a slide shows from its layout and master, each part once
/// (`repeated` holds those reported), and its effective background picture:
/// its own is lost, a layout's, master's or theme's repeats on every slide.
fn report_repeated(
    parse_ctx: &ParseCtx<'_>,
    slide: Option<&Element>,
    layout: Option<(&String, &LayoutInfo)>,
    master: Option<(&String, &MasterInfo)>,
    repeated: &mut HashSet<String>,
) {
    let mut omit = |part: &str| {
        if repeated.insert(part.to_string()) {
            let _shared = parse_ctx.outside_units();
            parse_ctx.emit(DiagnosticCode::HeaderOmitted, Region::Header);
        }
    };
    let own = Backdrop::of(slide);
    if own.shows_master
        && let Some((path, layout)) = layout
    {
        if layout.backdrop.shapes {
            omit(path);
        }
        if layout.backdrop.shows_master
            && let Some((path, master)) = master
            && master.backdrop.shapes
        {
            omit(path);
        }
    }
    let layout_background = layout.and_then(|(p, l)| Some((l.backdrop.background?, Some(p))));
    let master_background = master.and_then(|(p, m)| Some((m.backdrop.background?, Some(p))));
    let background = own.background.map(|b| (b, None)).or(layout_background).or(master_background);
    match background {
        Some((Background::Picture, None)) => {
            parse_ctx.image_loss(ImageLoss::Unplaced, Region::Body);
        }
        Some((Background::Picture, Some(part))) => omit(part.as_str()),
        Some((Background::Theme(idx), holder)) => {
            if let Some((path, master)) = master
                && master.theme.picture(idx)
            {
                omit(holder.unwrap_or(path));
            }
        }
        Some((Background::Fill, _)) | None => {}
    }
}

/// Report a slide-number, date, footer or header placeholder, none of which
/// is carried, when it shows text: `true` when `ph_type` names one.
fn side_placeholder(parse_ctx: &ParseCtx<'_>, sp: &Element, ph_type: &str) -> bool {
    let (code, region) = match ph_type {
        kind if FOOTERS.contains(&kind) => (DiagnosticCode::FooterOmitted, Region::Footer),
        "hdr" => (DiagnosticCode::HeaderOmitted, Region::Header),
        _ => return false,
    };
    if sp.outermost(ns::A, "t").any(|t| !t.text().trim().is_empty()) {
        parse_ctx.emit(code, region);
    }
    true
}

struct SlideCtx<'a, 'b> {
    parse_ctx: &'b ParseCtx<'b>,
    pkg: &'b Package<'a>,
    rels: &'b Relationships,
    base_part: &'b str,
    region: Region,
    assets: &'b RefCell<AssetSink>,
    default_text: &'b LevelStyle,
    layout: Option<&'b LayoutInfo>,
    master: Option<&'b MasterInfo>,
    /// Slide part path -> the slide's unit, for internal slide-to-slide links.
    slide_units: &'b HashMap<&'b str, Unit>,
}

impl SlideCtx<'_, '_> {
    /// Slide-to-slide relationships become links to the target slide's unit;
    /// everything else classifies as usual.
    fn link_target(&self, rel: &Relationship) -> LinkTarget {
        if rel.mode == TargetMode::Internal
            && let Ok(t) = path::resolve(self.base_part, &rel.target)
            && let Some(unit) = self.slide_units.get(t.path.as_str())
        {
            return LinkTarget::Unit(*unit);
        }
        classify_rel_target(rel.mode == TargetMode::External, &rel.target)
    }

    /// Fold the cascade for a paragraph, outermost first.
    fn base_props(&self, ph: Option<Ph<'_>>, shape_styles: &LevelStyle, lvl: usize) -> TextProps {
        let mut props = self.default_text.level(lvl);
        if let Some(master) = self.master {
            let class_style = match ph.map(|p| cascade::title_class(p.kind)) {
                Some(TitleClass::Title) => &master.title,
                Some(TitleClass::Body) => &master.body,
                None => &master.other,
            };
            props = props.merge(class_style.level(lvl));
            if let Some(ph) = ph
                && let Some(hit) = cascade::match_placeholder(&master.placeholders, ph)
            {
                props = props.merge(hit.styles.level(lvl));
            }
        }
        if let Some(layout) = self.layout
            && let Some(ph) = ph
            && let Some(hit) = cascade::match_placeholder(&layout.placeholders, ph)
        {
            props = props.merge(hit.styles.level(lvl));
        }
        props.merge(shape_styles.level(lvl))
    }

    fn paragraph(
        &self,
        p: &Element,
        ph: Option<Ph<'_>>,
        shape_styles: &LevelStyle,
    ) -> (usize, TextProps) {
        let ppr = p.find(ns::A, "pPr");
        let lvl =
            ppr.and_then(|pr| pr.attr(ns::A, "lvl")).and_then(|v| v.parse().ok()).unwrap_or(0);
        let props = self.base_props(ph, shape_styles, lvl);
        (lvl, ppr.map_or(props, |ppr| props.merge(cascade::paragraph_props(ppr))))
    }

    fn speaker_notes(&self) -> Result<Option<Block>, ConvertError> {
        if let Some(notes_path) = rel_target_of_type(self.rels, self.base_part, NOTES_REL)
            && let Some(tree) = rooted_part(
                self.parse_ctx,
                self.pkg,
                &notes_path,
                Role::Content(Region::Note),
                "notes",
            )?
        {
            let rels =
                read_rels(self.pkg, &rels_part_for(&notes_path), Role::Content(Region::Note))?;
            let notes_ctx = SlideCtx {
                rels: &rels,
                base_part: &notes_path,
                region: Region::Note,
                layout: None,
                master: None,
                ..*self
            };
            let mut blocks = Vec::new();
            if let Some(sp_tree) = tree.first_descendant(ns::P, "spTree") {
                parse_shapes(sp_tree, &notes_ctx, &mut blocks)?;
            }
            return Ok((!blocks.is_empty()).then_some(Block::BlockQuote(blocks)));
        }
        Ok(None)
    }

    fn graphic_part(&self) -> GraphicPart<'_, '_> {
        GraphicPart {
            pkg: self.pkg,
            rels: self.rels,
            base_part: self.base_part,
            region: self.region,
        }
    }

    fn emit(&self, code: DiagnosticCode) {
        self.parse_ctx.emit(code, self.region);
    }

    fn emit_related(&self, code: DiagnosticCode) {
        if !self.rels.reported() {
            self.emit(code);
        }
    }
}

/// Whether a slide shows `elem`: its slide-number, date and footer
/// placeholders it does not.
fn renders(elem: &Element) -> bool {
    !hidden_shape(elem)
        && (!(elem.is(ns::P, "sp") || elem.is(ns::P, "cxnSp"))
            || !Ph::of(elem).is_some_and(|ph| FOOTERS.contains(&ph.kind)))
}

/// Whether this reader reads `elem` where a slide shows it: never VML.
fn reads(elem: &Element) -> bool {
    elem.ns.as_deref() != Some(ns::VML) && renders(elem)
}

/// `parent`'s children in order, each `mc:AlternateContent` replaced by the
/// children of its selected branch, whose declined choices report in `region`.
fn selected_children<'e>(
    parse_ctx: &ParseCtx<'_>,
    region: Region,
    parent: &'e Element,
) -> Vec<&'e Element> {
    let mut selected = Vec::new();
    let mut open: Vec<&Element> = parent.child_elems().collect();
    open.reverse();
    while let Some(elem) = open.pop() {
        if elem.is(ns::MC, "AlternateContent") {
            let branch =
                alternate_branch(parse_ctx, region, elem, None, SUPPORTED_NS, (renders, reads));
            if let Some(branch) = branch {
                let at = open.len();
                open.extend(branch.child_elems());
                open[at..].reverse();
            }
        } else {
            selected.push(elem);
        }
    }
    selected
}

/// The first of `content`, or of their descendants, that is `namespace:local`.
fn first_in<'e>(content: &[&'e Element], namespace: &str, local: &str) -> Option<&'e Element> {
    content.iter().find_map(|elem| {
        if elem.is(namespace, local) {
            Some(*elem)
        } else {
            elem.first_descendant(namespace, local)
        }
    })
}

fn parse_shapes(
    parent: &Element,
    ctx: &SlideCtx<'_, '_>,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    for child in selected_children(ctx.parse_ctx, ctx.region, parent) {
        if child.is(ns::P, "contentPart") || child.is(P14, "contentPart") {
            ctx.emit(DiagnosticCode::GraphicUnread);
            continue;
        }
        if child.ns.as_deref().is_none_or(|n| n != ns::P) {
            continue;
        }
        if hidden_shape(child) {
            ctx.emit(DiagnosticCode::HiddenOmitted);
            continue;
        }
        match child.local.as_str() {
            // Connector shapes carry the same `p:txBody` as plain shapes.
            "sp" | "cxnSp" => parse_shape(child, ctx, blocks),
            "grpSp" => parse_shapes(child, ctx, blocks)?,
            "graphicFrame" => parse_graphic_frame(child, ctx, blocks)?,
            "pic" => parse_picture(child, ctx, blocks)?,
            _ => {}
        }
    }
    Ok(())
}

/// A picture: an external-mode target (`r:link`) becomes an external
/// source, and an embedded one is retained as an asset. The movie or sound
/// it plays is not read.
fn parse_picture(
    pic: &Element,
    ctx: &SlideCtx<'_, '_>,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    let plays =
        pic.find(ns::P, "nvPicPr").and_then(|nv| nv.find(ns::P, "nvPr")).is_some_and(|nv| {
            nv.descendant_elems().any(|e| {
                (e.ns.as_deref() == Some(ns::A) && MEDIA.contains(&e.local.as_str()))
                    || e.is(P14, "media")
            })
        });
    if plays {
        ctx.emit(DiagnosticCode::GraphicUnread);
    }
    let source = blip_source(pic, ctx)?;
    push_image(blocks, ctx, description(pic), source);
    Ok(())
}

/// The source of `pic`'s blip, `Unread` when it names none or its part does not resolve.
fn blip_source(pic: &Element, ctx: &SlideCtx<'_, '_>) -> Result<ImageSource, ConvertError> {
    let rid = pic
        .first_descendant(ns::A, "blip")
        .and_then(|b| b.attr_qualified(ns::R, "embed").or_else(|| b.attr_qualified(ns::R, "link")));
    Ok(match rid {
        Some(rid) => rel_image_source(ctx.pkg, ctx.rels, ctx.base_part, ctx.assets, rid)?,
        None => None,
    }
    .unwrap_or(ImageSource::Unread))
}

fn description(elem: &Element) -> String {
    elem.first_descendant(ns::P, "cNvPr")
        .and_then(|p| p.attr(ns::P, "descr"))
        .map(clean_text)
        .unwrap_or_default()
}

fn push_image(blocks: &mut Vec<Block>, ctx: &SlideCtx<'_, '_>, alt: String, source: ImageSource) {
    let origin = ctx.parse_ctx.image_origin(ctx.region);
    blocks.push(Block::Paragraph(vec![Inline::Image { origin, alt, source }]));
}

/// An embedded OLE object: its payload is retained as an asset nothing renders, and its
/// preview picture (`p:pic`) is shown in its place under the frame's description.
fn embedded_object(
    ole: &Element,
    frame: &Element,
    ctx: &SlideCtx<'_, '_>,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    ctx.emit(DiagnosticCode::EmbeddedObject);
    retain_ole_payload(
        ctx.pkg,
        ctx.rels,
        ctx.base_part,
        ctx.assets,
        ole.attr_qualified(ns::R, "id"),
    )?;
    let descr = frame.find(ns::P, "nvGraphicFramePr").map(description).unwrap_or_default();
    let pic =
        selected_children(ctx.parse_ctx, ctx.region, ole).into_iter().find(|c| c.is(ns::P, "pic"));
    match pic {
        Some(pic) => {
            let source = blip_source(pic, ctx)?;
            push_image(blocks, ctx, descr, source);
        }
        None if !descr.trim().is_empty() => {
            push_image(blocks, ctx, descr, ImageSource::Unavailable);
        }
        None => {}
    }
    Ok(())
}

fn hidden_shape(shape: &Element) -> bool {
    shape
        .child_elems()
        .filter(|e| {
            e.ns.as_deref() == Some(ns::P)
                && matches!(
                    e.local.as_str(),
                    "nvPicPr" | "nvGrpSpPr" | "nvSpPr" | "nvCxnSpPr" | "nvGraphicFramePr"
                )
        })
        .filter_map(|e| e.find(ns::P, "cNvPr"))
        .any(|e| is_true(e.attr(ns::P, "hidden")))
}

fn is_true(value: Option<&str>) -> bool {
    matches!(value, Some("1" | "true"))
}

fn parse_shape(sp: &Element, ctx: &SlideCtx<'_, '_>, blocks: &mut Vec<Block>) {
    let ph = Ph::of(sp);
    if let Some(ph) = ph
        && (ctx.region == Region::Note && ph.kind == "sldImg"
            || (ctx.region == Region::Note || ph.kind != "hdr")
                && side_placeholder(ctx.parse_ctx, sp, ph.kind))
    {
        return;
    }
    if sp
        .outermost(ns::P, "sp")
        .any(|inner| !hidden_shape(inner) && !inner.text().trim().is_empty())
    {
        ctx.emit(DiagnosticCode::GraphicUnread);
    }
    if sp.find(ns::P, "spPr").is_some_and(|pr| pr.find(ns::A, "blipFill").is_some()) {
        ctx.parse_ctx.image_loss(ImageLoss::Unplaced, ctx.region);
    }
    let Some(tx) = sp.find(ns::P, "txBody") else {
        return;
    };
    if ph.is_some_and(|p| cascade::title_class(p.kind) == TitleClass::Title) {
        push_title_heading(tx, ctx, ph, blocks);
    } else {
        parse_text_body(tx, ctx, ph, blocks);
    }
}

/// Collapse a title placeholder's paragraphs into one slide heading. Runs
/// resolve through the full cascade like body text.
fn push_title_heading(
    tx: &Element,
    ctx: &SlideCtx<'_, '_>,
    ph: Option<Ph<'_>>,
    blocks: &mut Vec<Block>,
) {
    let shape_styles = cascade::parse_level_styles(tx.find(ns::A, "lstStyle"));
    let mut inlines: Vec<Inline> = Vec::new();
    for p in tx.find_all(ns::A, "p") {
        let (_, props) = ctx.paragraph(p, ph, &shape_styles);
        let mut para = parse_para_inlines(p, ctx, props);
        if inlines_are_empty(&para) {
            continue;
        }
        rebase_emphasis(&mut para, props.delta.resolve());
        if !inlines.is_empty() {
            inlines.push(Inline::LineBreak);
        }
        inlines.extend(para);
    }
    if !inlines_are_empty(&inlines) {
        let anchor = Some(inlines_to_plain_text(&inlines));
        blocks.push(Block::Heading { level: 2, anchor, content: inlines });
    }
}

fn parse_text_body(
    tx: &Element,
    ctx: &SlideCtx<'_, '_>,
    ph: Option<Ph<'_>>,
    blocks: &mut Vec<Block>,
) {
    let shape_styles = cascade::parse_level_styles(tx.find(ns::A, "lstStyle"));
    let mut counters = LevelCounters::<{ cascade::LEVELS }>::default();
    let mut list_run: Vec<ListEntry> = Vec::new();

    for p in tx.find_all(ns::A, "p") {
        let (lvl, props) = ctx.paragraph(p, ph, &shape_styles);
        let inlines = parse_para_inlines(p, ctx, props);
        if inlines_are_empty(&inlines) {
            flush_list(blocks, &mut list_run);
            continue;
        }
        let (marker, number, label) = match props.bullet {
            Bullet::AutoNum { marker, start, wrap } => {
                let number = counters.next(lvl, start, every_deeper);
                (marker, number, wrap.label(marker, number))
            }
            Bullet::Char => (MarkerKind::Bullet, 0, None),
            Bullet::None | Bullet::Inherit => {
                flush_list(blocks, &mut list_run);
                match math_lines(&inlines) {
                    Some(lines) => blocks.extend(lines.into_iter().map(Block::Math)),
                    None => blocks.push(Block::Paragraph(inlines)),
                }
                continue;
            }
        };
        list_run.push(ListEntry {
            mark: ListMark {
                key: ListKey { instance: TEXT_BODY_LIST, marker },
                level: lvl,
                number,
                label,
            },
            blocks: vec![Block::Paragraph(inlines)],
        });
    }
    flush_list(blocks, &mut list_run);
}

fn parse_para_inlines(p: &Element, ctx: &SlideCtx<'_, '_>, props: TextProps) -> Vec<Inline> {
    let base = props.delta.resolve();
    let mut out: Vec<Inline> = Vec::new();
    for child in p.child_elems() {
        if child.is(ns::A14, "m") {
            let para = child.find(ns::M, "oMathPara").unwrap_or(child);
            for (i, tex) in omath_para_to_tex(para).into_iter().enumerate() {
                if i > 0 {
                    out.push(Inline::LineBreak);
                }
                out.push(Inline::Math(tex));
            }
            continue;
        }
        if child.ns.as_deref().is_none_or(|n| n != ns::A) {
            continue;
        }
        match child.local.as_str() {
            "r" | "fld" => {
                let rpr = child.find(ns::A, "rPr");
                let style = rpr.map_or(base, |rpr| cascade::rpr_delta(rpr).apply(base));
                let faces =
                    rpr.map_or(props.faces, |rpr| props.faces.merge(cascade::rpr_faces(rpr)));
                let text = child.find(ns::A, "t").map(Element::text).unwrap_or_default();
                let (content, unmapped) = run_inlines(&text, faces.route(), style);
                ctx.parse_ctx.emit_count(DiagnosticCode::UnmappedSymbol, ctx.region, unmapped);
                if content.is_empty() {
                    continue;
                }
                let click = rpr.and_then(|r| r.find(ns::A, "hlinkClick"));
                let rid =
                    click.and_then(|h| h.attr_qualified(ns::R, "id")).filter(|r| !r.is_empty());
                let target = rid.and_then(|rid| ctx.rels.get(rid)).map(|rel| ctx.link_target(rel));
                let jump = click
                    .and_then(|h| h.attr(ns::A, "action"))
                    .is_some_and(|action| NAVIGATION.iter().any(|n| action.starts_with(n)));
                if rid.is_some()
                    && target.is_none()
                    && !jump
                    && !label_is_absolute_url(&inlines_to_plain_text(&content))
                {
                    ctx.emit_related(DiagnosticCode::LinkTargetUnread);
                }
                match target {
                    Some(target) => out.push(Inline::Link { content, target }),
                    None => out.extend(content),
                }
            }
            "br" => out.push(Inline::LineBreak),
            _ => {}
        }
    }
    out
}

fn parse_graphic_frame(
    frame: &Element,
    ctx: &SlideCtx<'_, '_>,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    let data = frame.first_descendant(ns::A, "graphicData");
    let content =
        data.map(|data| selected_children(ctx.parse_ctx, ctx.region, data)).unwrap_or_default();
    if let Some(tbl) = first_in(&content, ns::A, "tbl") {
        parse_table(tbl, ctx, blocks)?;
        return Ok(());
    }
    if let Some(ole) = first_in(&content, ns::P, "oleObj") {
        return embedded_object(ole, frame, ctx, blocks);
    }
    if let Some(chart) = first_in(&content, ns::CHART, "chart") {
        blocks.extend(drawingml::graphic_blocks(
            ctx.parse_ctx,
            &ctx.graphic_part(),
            chart.attr_qualified(ns::R, "id"),
            ns::CHART,
            "chartSpace",
            |root| drawingml::chart_blocks(ctx.parse_ctx, root, ctx.region),
        )?);
        return Ok(());
    }
    if let Some(rel_ids) = first_in(&content, ns::DGM, "relIds") {
        blocks.extend(drawingml::graphic_blocks(
            ctx.parse_ctx,
            &ctx.graphic_part(),
            rel_ids.attr_qualified(ns::R, "dm"),
            ns::DGM,
            "dataModel",
            |root| Ok(drawingml::diagram_blocks(root)),
        )?);
        return Ok(());
    }
    let dropped = data.is_some_and(|data| {
        data.child_elems().next().is_some()
            && !data.descendant_elems().any(|e| e.is(ns::MC, "Choice") && holds_graphic(e))
    });
    if !content.is_empty() || dropped {
        ctx.emit(DiagnosticCode::GraphicUnread);
    }
    Ok(())
}

/// DrawingML slide table: origins carry `gridSpan`/`rowSpan`; merged
/// continuation cells (`hMerge`/`vMerge`) consume covered positions.
fn parse_table(
    tbl: &Element,
    ctx: &SlideCtx<'_, '_>,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    let header_rows =
        usize::from(tbl.find(ns::A, "tblPr").is_some_and(|p| is_true(p.attr(ns::A, "firstRow"))));
    let mut builder = GridBuilder::new(ctx.parse_ctx);
    for tr in tbl.find_all(ns::A, "tr") {
        builder.next_row();
        for tc in tr.find_all(ns::A, "tc") {
            if is_true(tc.attr(ns::A, "hMerge")) || is_true(tc.attr(ns::A, "vMerge")) {
                builder.covered()?;
                continue;
            }
            let span = |name: &str| {
                tc.attr(ns::A, name).and_then(|v| v.parse::<u32>().ok()).unwrap_or(1).max(1)
            };
            let mut cell_blocks = Vec::new();
            if let Some(tx) = tc.find(ns::A, "txBody") {
                parse_text_body(tx, ctx, None, &mut cell_blocks);
            }
            builder.place(Cell::spanning(cell_blocks, span("gridSpan"), span("rowSpan")))?;
        }
    }
    let mut table = builder.finish()?;
    if table.grid.is_empty() {
        return Ok(());
    }
    table.header_rows = resolve_header_rows(&table, header_rows);
    blocks.push(Block::Table(table));
    Ok(())
}

#[cfg(test)]
#[expect(clippy::format_collect, reason = "fixture XML is built with format!")]
mod tests {
    use super::*;
    use crate::shared::test_support::{diagnostic_rows, unit_ordinals, zip_package};
    use std::assert_matches;

    const NS: &str = concat!(
        r#"xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" "#,
        r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" "#,
        r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" "#,
        r#"xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" "#,
        r#"xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" "#,
        r#"xmlns:cx="http://schemas.microsoft.com/office/drawing/2014/chartex" "#,
        r#"xmlns:p14="http://schemas.microsoft.com/office/powerpoint/2010/main" "#,
        r#"xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#,
    );

    type Reported = (DiagnosticCode, Region, Option<u32>, u64);
    type Parts = Vec<(String, String)>;

    fn reported(parts: &Parts) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        let doc = parse(&parse_ctx, &zip_package(parts)).unwrap();
        let diagnostics =
            crate::converter::diagnostics_of(parse_ctx, &doc, crate::ImagePolicy::Placeholder);
        let mut reported: Vec<Reported> = diagnostic_rows(&diagnostics.unwrap());
        reported.sort();
        reported
    }

    #[test]
    fn a_package_without_its_main_part_is_missing_it() {
        let styles = relationships(&[("rId1", "styles", "styles.xml")]);
        let absent = relationships(&[("rId1", "officeDocument", "missing.xml")]);
        let cases: [(Parts, &str); 3] = [
            (vec![("docProps/core.xml".into(), "<x/>".into())], "ppt/presentation.xml"),
            (
                vec![("_rels/.rels".into(), styles), ("word/document.xml".into(), "<x/>".into())],
                "ppt/presentation.xml",
            ),
            (
                vec![
                    ("_rels/.rels".into(), absent),
                    ("ppt/presentation.xml".into(), "<x/>".into()),
                ],
                "missing.xml",
            ),
        ];
        for (parts, missing) in cases {
            let error = parse(&ParseCtx::default(), &zip_package(&parts)).unwrap_err();
            assert_matches!(error, ConvertError::MissingPart { part } if part.to_string() == missing);
        }
    }

    fn relationships(targets: &[(&str, &str, &str)]) -> String {
        let rels: String = targets
            .iter()
            .map(|(id, kind, target)| {
                let kind = if kind.contains("://") {
                    kind.to_string()
                } else {
                    format!(
                        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}"
                    )
                };
                format!(r#"<Relationship Id="{id}" Type="{kind}" Target="{target}"/>"#)
            })
            .collect();
        format!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
        )
    }

    /// A deck of the given slide parts, each listed with its own
    /// relationship, plus `extra` parts that replace any of the same name.
    fn deck(slides: &[String], extra: &[(&str, String)]) -> Parts {
        let ids: String = (1..=slides.len())
            .map(|i| format!(r#"<p:sldId id="{}" r:id="rId{i}"/>"#, 255 + i))
            .collect();
        let targets: Vec<(String, String)> = (1..=slides.len())
            .map(|i| (format!("rId{i}"), format!("slides/slide{i}.xml")))
            .collect();
        let pres_rels: Vec<(&str, &str, &str)> =
            targets.iter().map(|(id, target)| (id.as_str(), "slide", target.as_str())).collect();
        let mut parts: Parts = vec![
            (
                "ppt/presentation.xml".into(),
                format!("<p:presentation {NS}><p:sldIdLst>{ids}</p:sldIdLst></p:presentation>"),
            ),
            ("ppt/_rels/presentation.xml.rels".into(), relationships(&pres_rels)),
        ];
        parts.extend(
            slides
                .iter()
                .enumerate()
                .map(|(i, xml)| (format!("ppt/slides/slide{}.xml", i + 1), xml.clone())),
        );
        for (name, body) in extra {
            parts.retain(|(n, _)| n != name);
            parts.push(((*name).into(), body.clone()));
        }
        parts
    }

    fn one(tree: &str, slide_rels: &[(&str, &str, &str)], extra: &[(&str, String)]) -> Parts {
        let mut all = vec![("ppt/slides/_rels/slide1.xml.rels", relationships(slide_rels))];
        all.extend(extra.iter().cloned());
        deck(&[part("sld", "", tree, "")], &all)
    }

    fn notes_in(root: &str, tree: &str) -> Parts {
        one(
            "",
            &[("rId7", "notesSlide", "../notesSlides/notesSlide1.xml")],
            &[("ppt/notesSlides/notesSlide1.xml", part(root, "", tree, ""))],
        )
    }

    fn notes(tree: &str) -> Parts {
        notes_in("notes", tree)
    }

    fn text(body: &str) -> String {
        format!("<p:txBody><a:p><a:r><a:t>{body}</a:t></a:r></a:p></p:txBody>")
    }

    fn shape(ph: &str, body: &str) -> String {
        format!("<p:sp><p:nvSpPr><p:nvPr>{ph}</p:nvPr></p:nvSpPr><p:spPr/>{body}</p:sp>")
    }

    fn frame(data: &str) -> String {
        format!(
            "<p:graphicFrame><a:graphic><a:graphicData>{data}</a:graphicData></a:graphic></p:graphicFrame>"
        )
    }

    fn picture(nv_pr: &str, rid: &str) -> String {
        format!(
            r#"<p:pic><p:nvPicPr><p:cNvPr id="2" name="pic"/><p:nvPr>{nv_pr}</p:nvPr></p:nvPicPr><p:blipFill><a:blip r:embed="{rid}"/></p:blipFill></p:pic>"#
        )
    }

    fn alternate(choice: &str, fallback: &str) -> String {
        format!(
            r#"<mc:AlternateContent><mc:Choice Requires="cx">{choice}</mc:Choice><mc:Fallback>{fallback}</mc:Fallback></mc:AlternateContent>"#
        )
    }

    fn linked(rid: &str, action: &str, label: &str) -> String {
        format!(
            r#"<p:sp><p:txBody><a:p><a:r><a:rPr><a:hlinkClick r:id="{rid}" {action}/></a:rPr><a:t>{label}</a:t></a:r></a:p></p:txBody></p:sp>"#
        )
    }

    const CHART: &str = r#"<c:chart r:id="rId3"/>"#;
    const DIAGRAM: &str = r#"<dgm:relIds r:dm="rId4"/>"#;
    const UNPARSEABLE: &str = "</x>";
    const PNG: (&str, &str, &str) = ("rId2", "image", "../media/one.png");
    const GRAPHICS: &[(&str, &str, &str)] = &[
        PNG,
        ("rId3", "chart", "../charts/chart1.xml"),
        ("rId4", "diagramData", "../diagrams/data1.xml"),
    ];

    fn media() -> (&'static str, String) {
        ("ppt/media/one.png", "png".into())
    }

    fn with_master(slide: String, layout: String, master: String, theme: &str) -> Parts {
        deck(
            &[slide.clone(), slide],
            &[
                (
                    "ppt/slides/_rels/slide1.xml.rels",
                    relationships(&[("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml")]),
                ),
                (
                    "ppt/slides/_rels/slide2.xml.rels",
                    relationships(&[("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml")]),
                ),
                ("ppt/slideLayouts/slideLayout1.xml", layout),
                (
                    "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
                    relationships(&[("rId1", "slideMaster", "../slideMasters/slideMaster1.xml")]),
                ),
                ("ppt/slideMasters/slideMaster1.xml", master),
                (
                    "ppt/slideMasters/_rels/slideMaster1.xml.rels",
                    relationships(&[("rId1", "theme", "../theme/theme1.xml")]),
                ),
                (
                    "ppt/theme/theme1.xml",
                    format!(
                        "<a:theme {NS}><a:themeElements><a:fmtScheme><a:fillStyleLst><a:solidFill/><a:blipFill/></a:fillStyleLst>\
                         <a:bgFillStyleLst>{theme}</a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>"
                    ),
                ),
            ],
        )
    }

    fn part(root: &str, attrs: &str, tree: &str, bg: &str) -> String {
        format!(
            "<p:{root} {NS} {attrs}><p:cSld>{bg}<p:spTree>{tree}</p:spTree></p:cSld></p:{root}>"
        )
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the table of losses by region")]
    fn each_loss_is_reported_in_its_region() {
        use DiagnosticCode::*;
        use Region::{Body, Comment, Container, Footer, Header, Note};
        let plain = part("sld", "", &shape("", &text("kept")), "");
        let logo = shape("", "");
        let placeholder = shape(r#"<p:ph type="body"/>"#, "");
        let bg_picture = "<p:bg><p:bgPr><a:blipFill/></p:bgPr></p:bg>";
        let bg_theme = r#"<p:bg><p:bgRef idx="1002"/></p:bg>"#;
        let controls = |inner: &str| {
            let controls = format!("</p:spTree><p:controls>{inner}</p:controls>");
            deck(&[part("sld", "", "", "").replace("</p:spTree>", &controls)], &[])
        };
        let unreadable_rels = [("ppt/slides/_rels/slide1.xml.rels", UNPARSEABLE.into())];
        let footer = shape(r#"<p:ph type="sldNum"/>"#, &text("3"));
        let chart =
            |xml: &str| one(&frame(CHART), GRAPHICS, &[("ppt/charts/chart1.xml", xml.into())]);
        let numbered = part("sld", "", &shape(r#"<p:ph type="sldNum"/>"#, &text("2")), "");
        let mut unresolved = deck(&[plain.clone(), numbered], &[]);
        unresolved[1].1 = relationships(&[("rId2", "slide", "slides/slide2.xml")]);
        let mut unread = deck(&[plain.clone(), plain.clone()], &[]);
        unread.retain(|(name, _)| name != "ppt/slides/slide1.xml");
        let ole = r#"<p:oleObj progId="Equation.3" r:id="rId5"/>"#;
        let symbol_rpr =
            r#"<a:rPr><a:latin typeface="Symbol"/><a:hlinkClick r:id="rId9"/></a:rPr>"#;
        let symbol_link = shape("", &paragraphs(&[&[(symbol_rpr, "https://example.org/")]]));
        let players = [
            ("a movie with its poster", r#"<a:videoFile r:link="rId8"/>"#),
            ("an audio file", r#"<a:audioFile r:link="rId8"/>"#),
            ("a wave sound", r#"<a:wavAudioFile r:embed="rId8"/>"#),
            ("a QuickTime movie", r#"<a:quickTimeFile r:link="rId8"/>"#),
            (
                "a movie named only by its 2010 extension",
                r#"<p:extLst><p:ext><p14:media r:embed="rId9"/></p:ext></p:extLst>"#,
            ),
            ("a sound", "<a:audioCd/>"),
        ];
        let mut cases: Vec<(&str, Parts, Vec<Reported>)> = vec![
            (
                "a conventional presentation part",
                {
                    let mut parts = one("", &[], &[]);
                    parts.push(("_rels/.rels".into(), relationships(&[])));
                    parts
                },
                vec![(ConventionalPartUsed, Container, None, 1)],
            ),
            (
                "a main part target that names no part",
                {
                    let mut parts = one("", &[], &[]);
                    parts.push((
                        "_rels/.rels".into(),
                        relationships(&[("rId1", "officeDocument", "/")]),
                    ));
                    parts
                },
                vec![(ConventionalPartUsed, Container, None, 1)],
            ),
            ("an unreadable slide", unread, vec![(UnitUnreadable, Body, Some(1), 1)]),
            (
                "a slide with no shape tree",
                deck(&[format!("<p:sld {NS}/>"), plain.clone()], &[]),
                vec![(UnitUnreadable, Body, Some(1), 1)],
            ),
            (
                "a slide with no shape tree after one with",
                deck(&[plain.clone(), format!("<p:sld {NS}/>")], &[]),
                vec![(UnitUnreadable, Body, Some(2), 1)],
            ),
            (
                "a listed slide that resolves nowhere",
                unresolved,
                vec![(UnitUnreadable, Body, Some(1), 1), (FooterOmitted, Footer, Some(2), 1)],
            ),
            ("a corrupt chart", chart(UNPARSEABLE), vec![(GraphicUnread, Body, Some(1), 1)]),
            (
                "a chart part rooted elsewhere",
                chart("<x/>"),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "a scatter chart",
                chart(&format!(
                    "<c:chartSpace {NS}><c:chart><c:plotArea><c:scatterChart><c:ser>\
                     <c:xVal/><c:yVal/></c:ser></c:scatterChart></c:plotArea></c:chart></c:chartSpace>"
                )),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "a chart whose part is absent",
                one(&frame(CHART), GRAPHICS, &[]),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "a chart with no relationship",
                one(&frame("<c:chart/>"), GRAPHICS, &[]),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "a corrupt diagram",
                one(&frame(DIAGRAM), GRAPHICS, &[("ppt/diagrams/data1.xml", UNPARSEABLE.into())]),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "a diagram whose part is absent",
                one(&frame(DIAGRAM), GRAPHICS, &[]),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "other graphic data",
                one(&frame(r#"<x:addin xmlns:x="urn:example"/>"#), &[], &[]),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            ("an empty graphic frame", one(&frame(""), &[], &[]), vec![]),
            (
                "an object in both branches",
                one(
                    &frame(&format!(
                        r#"<mc:AlternateContent><mc:Choice Requires="p14">{ole}</mc:Choice><mc:Fallback>{ole}</mc:Fallback></mc:AlternateContent>"#
                    )),
                    &[("rId5", "oleObject", "../embeddings/one.bin")],
                    &[("ppt/embeddings/one.bin", "ole".into())],
                ),
                vec![(EmbeddedObject, Body, Some(1), 1)],
            ),
            (
                "a declined chart",
                one(
                    &alternate(
                        &frame(r#"<cx:chart r:id="rId3"/>"#),
                        &shape("", &text("This chart isn't available")),
                    ),
                    &[],
                    &[],
                ),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "a declined picture and text",
                one(
                    &alternate(
                        &format!("{}{}", picture("", "rId2"), shape("", &text("caption"))),
                        "",
                    ),
                    &[],
                    &[],
                ),
                vec![(ImageUnplaced, Body, Some(1), 1), (BlockUnread, Body, Some(1), 1)],
            ),
            (
                "a declined picture and text the fallback shows",
                one(
                    &alternate(
                        &format!("{}{}", picture("", "rId2"), shape("", &text("caption"))),
                        &format!("{}{}", picture("", "rId2"), shape("", &text("caption"))),
                    ),
                    &[PNG],
                    &[media()],
                ),
                vec![(ImagePlaceholder, Body, Some(1), 1)],
            ),
            (
                "a declined picture over a VML picture the fallback holds",
                one(
                    &alternate(
                        &picture("", "rId2"),
                        r#"<v:shape xmlns:v="urn:schemas-microsoft-com:vml"><v:imagedata r:id="rId2"/></v:shape>"#,
                    ),
                    &[],
                    &[],
                ),
                vec![(ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "a declined picture over a VML picture fill the fallback holds",
                one(
                    &alternate(
                        &picture("", "rId2"),
                        r#"<v:rect xmlns:v="urn:schemas-microsoft-com:vml"><v:fill r:id="rId2" type="frame"/></v:rect>"#,
                    ),
                    &[],
                    &[],
                ),
                vec![(ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "a declined VML picture",
                one(
                    &alternate(
                        r#"<v:shape xmlns:v="urn:schemas-microsoft-com:vml"><v:imagedata r:id="rId2"/></v:shape>"#,
                        "",
                    ),
                    &[],
                    &[],
                ),
                vec![(ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "a declined VML picture fill",
                one(
                    &alternate(
                        r#"<v:rect xmlns:v="urn:schemas-microsoft-com:vml"><v:fill r:id="rId2" type="tile"/></v:rect>"#,
                        "",
                    ),
                    &[],
                    &[],
                ),
                vec![(ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "declined text over a footer the fallback shows",
                one(&alternate(&shape("", &text("body")), &footer), &[], &[]),
                vec![(BlockUnread, Body, Some(1), 1), (FooterOmitted, Footer, Some(1), 1)],
            ),
            ("a declined footer", one(&alternate(&footer, ""), &[], &[]), vec![]),
            (
                "a declined add-in with an empty Fallback",
                one(&frame(&alternate(r#"<x:addin xmlns:x="urn:example"/>"#, "")), &[], &[]),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "a declined chart with an empty Fallback",
                one(&frame(&alternate(r#"<cx:chart r:id="rId3"/>"#, "")), &[], &[]),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "a declined chart over a chart",
                one(
                    &frame(&alternate(r#"<cx:chart r:id="rId3"/>"#, CHART)),
                    GRAPHICS,
                    &[(
                        "ppt/charts/chart1.xml",
                        format!("<c:chartSpace {NS}><c:chart/></c:chartSpace>"),
                    )],
                ),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "ink",
                one(r#"<p:contentPart r:id="rId6"/>"#, &[], &[]),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "ink of 2010",
                one(r#"<p14:contentPart r:id="rId6"/>"#, &[], &[]),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "controls, each in the selected branch",
                controls(&format!(
                    r#"<p:control r:id="rId1"/>{}"#,
                    alternate("<p:control/>", "<p:control/><p:control/>")
                )),
                vec![(FormControlUnread, Body, Some(1), 3)],
            ),
            ("controls in an empty branch", controls(&alternate("<p:control/>", "")), vec![]),
            (
                "a picture with no bytes named",
                one("<p:pic/>", &[], &[]),
                vec![(ImageUnavailable, Body, Some(1), 1)],
            ),
            (
                "a picture of an unknown relationship",
                one(&picture("", "rId2"), &[], &[]),
                vec![(ImageUnavailable, Body, Some(1), 1)],
            ),
            (
                "a picture behind unreadable relationships",
                one(&picture("", "rId2"), &[], &unreadable_rels),
                vec![(PartUnreadable, Body, Some(1), 1), (ImageUnavailable, Body, Some(1), 1)],
            ),
            (
                "a chart behind unreadable relationships",
                one(&frame(CHART), &[], &unreadable_rels),
                vec![(PartUnreadable, Body, Some(1), 1)],
            ),
            (
                "a link of an unknown relationship",
                one(&linked("rId9", "", "see here"), &[], &[]),
                vec![(LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "file, presentation and slide actions of an unknown relationship",
                one(
                    &[
                        linked("rId9", r#"action="ppaction://hlinkfile""#, "notes"),
                        linked(
                            "rId9",
                            r#"action="ppaction://hlinkpres?slideindex=1&amp;slidetitle=""#,
                            "deck",
                        ),
                        linked("rId9", r#"action="ppaction://hlinksldjump""#, "summary"),
                    ]
                    .concat(),
                    &[],
                    &[],
                ),
                vec![(LinkTargetUnread, Body, Some(1), 3)],
            ),
            (
                "a custom show",
                one(
                    &linked(
                        "rId9",
                        r#"action="ppaction://customshow?id=0&amp;return=true""#,
                        "appendix",
                    ),
                    &[],
                    &[],
                ),
                vec![],
            ),
            (
                "a link labelled by its address",
                one(&linked("rId9", "", "https://example.org/"), &[], &[]),
                vec![],
            ),
            (
                "a link labelled by its address in a symbol font",
                one(&symbol_link, &[], &[]),
                vec![(LinkTargetUnread, Body, Some(1), 1)],
            ),
            (
                "a link with an empty relationship",
                one(&linked("", "", "see here"), &[], &[]),
                vec![],
            ),
            (
                "a next-slide jump",
                one(
                    &linked("rId9", r#"action="ppaction://hlinkshowjump?jump=nextslide""#, "next"),
                    &[],
                    &[],
                ),
                vec![],
            ),
            (
                "a link behind unreadable relationships",
                one(&linked("rId9", "", "see here"), &[], &unreadable_rels),
                vec![(PartUnreadable, Body, Some(1), 1)],
            ),
            (
                "a link in the notes",
                notes(&linked("rId9", "", "see here")),
                vec![(LinkTargetUnread, Note, Some(1), 1)],
            ),
            (
                "a code dropped from a symbol face in the notes",
                notes(&shape("", &paragraphs(&[&[(&face("Wingdings", ""), "&#xFF;")]]))),
                vec![(UnmappedSymbol, Note, Some(1), 1)],
            ),
            (
                "a picture filling a shape",
                one(
                    &shape("", "").replace("<p:spPr/>", "<p:spPr><a:blipFill/></p:spPr>"),
                    &[],
                    &[],
                ),
                vec![(ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "a picture filling a slide number",
                one(
                    &shape(r#"<p:ph type="sldNum"/>"#, &text("3"))
                        .replace("<p:spPr/>", "<p:spPr><a:blipFill/></p:spPr>"),
                    &[],
                    &[],
                ),
                vec![(FooterOmitted, Footer, Some(1), 1)],
            ),
            (
                "the slide's own background picture",
                deck(&[part("sld", "", "", bg_picture)], &[]),
                vec![(ImageUnplaced, Body, Some(1), 1)],
            ),
            (
                "slide footers",
                one(
                    &format!(
                        "{}{}{}",
                        shape(r#"<p:ph type="sldNum"/>"#, &text("3")),
                        shape(r#"<p:ph type="dt"/>"#, &text("today")),
                        shape(r#"<p:ph type="ftr"/>"#, &text(" ")),
                    ),
                    &[],
                    &[],
                ),
                vec![(FooterOmitted, Footer, Some(1), 2)],
            ),
            (
                "notes header and footer",
                notes(&format!(
                    "{}{}{}",
                    shape(r#"<p:ph type="hdr"/>"#, &text("Course")),
                    shape(r#"<p:ph type="ftr"/>"#, &text("Draft")),
                    shape(r#"<p:ph type="sldImg"/>"#, ""),
                )),
                vec![(HeaderOmitted, Header, Some(1), 1), (FooterOmitted, Footer, Some(1), 1)],
            ),
            (
                "modern comments",
                one(
                    "",
                    &[(
                        "rId5",
                        "http://schemas.microsoft.com/office/2018/10/relationships/comments",
                        "../comments/c1.xml",
                    )],
                    &[],
                ),
                vec![(CommentOmitted, Comment, Some(1), 1)],
            ),
            (
                "legacy comments",
                one("", &[("rId5", "comments", "../comments/comment1.xml")], &[]),
                vec![(CommentOmitted, Comment, Some(1), 1)],
            ),
            (
                "a header on a slide",
                one(&shape(r#"<p:ph type="hdr"/>"#, &text("Course")), &[], &[]),
                vec![],
            ),
            (
                "a notes shape nested in a notes shape",
                notes(&shape("", &format!("{}{}", text("kept"), shape("", &text("nested"))))),
                vec![(GraphicUnread, Note, Some(1), 1)],
            ),
            (
                "an empty shape nested in a notes shape",
                notes(&shape("", &format!("{}{}", text("kept"), shape("", "")))),
                vec![],
            ),
            (
                "notes rooted elsewhere",
                notes_in("sld", &shape("", &text("kept"))),
                vec![(PartUnreadable, Note, Some(1), 1)],
            ),
            (
                "a layout rooted elsewhere",
                with_master(
                    plain.clone(),
                    part("sld", "", &logo, ""),
                    part("sldMaster", "", "", ""),
                    "<a:solidFill/>",
                ),
                vec![(PartUnreadable, Body, None, 1)],
            ),
            (
                "a master rooted elsewhere",
                with_master(
                    plain.clone(),
                    part("sldLayout", "", "", ""),
                    part("sldLayout", "", &logo, ""),
                    "<a:solidFill/>",
                ),
                vec![(PartUnreadable, Body, None, 1)],
            ),
            (
                "master and layout shapes, once each",
                with_master(
                    plain.clone(),
                    part("sldLayout", "", &format!("{logo}{placeholder}"), ""),
                    part("sldMaster", "", &logo, ""),
                    "<a:solidFill/>",
                ),
                vec![(HeaderOmitted, Header, None, 2)],
            ),
            (
                "master shapes a layout hides",
                with_master(
                    plain.clone(),
                    part("sldLayout", r#"showMasterSp="0""#, &placeholder, ""),
                    part("sldMaster", "", &logo, ""),
                    "<a:solidFill/>",
                ),
                vec![],
            ),
            (
                "layout shapes a slide hides",
                with_master(
                    part("sld", r#"showMasterSp="false""#, "", ""),
                    part("sldLayout", "", &logo, ""),
                    part("sldMaster", "", &logo, ""),
                    "<a:solidFill/>",
                ),
                vec![],
            ),
            (
                "a layout background picture",
                with_master(
                    plain.clone(),
                    part("sldLayout", "", "", bg_picture),
                    part("sldMaster", "", "", bg_theme),
                    "<a:solidFill/><a:solidFill/>",
                ),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "a theme background picture",
                with_master(
                    plain.clone(),
                    part("sldLayout", "", "", ""),
                    part("sldMaster", "", "", bg_theme),
                    "<a:solidFill/><a:blipFill/>",
                ),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "a theme fill picture",
                with_master(
                    plain.clone(),
                    part("sldLayout", "", "", ""),
                    part("sldMaster", "", "", r#"<p:bg><p:bgRef idx="2"/></p:bg>"#),
                    "<a:solidFill/><a:solidFill/>",
                ),
                vec![(HeaderOmitted, Header, None, 1)],
            ),
            (
                "a layout's theme background picture beside master shapes",
                with_master(
                    plain.clone(),
                    part("sldLayout", "", "", bg_theme),
                    part("sldMaster", "", &logo, ""),
                    "<a:solidFill/><a:blipFill/>",
                ),
                vec![(HeaderOmitted, Header, None, 2)],
            ),
            (
                "a theme background fill",
                with_master(
                    plain,
                    part("sldLayout", "", "", ""),
                    part("sldMaster", "", "", bg_theme),
                    "<a:blipFill/><a:solidFill/>",
                ),
                vec![],
            ),
            (
                "a slide fill over a master picture",
                with_master(
                    part("sld", "", "", "<p:bg><p:bgPr><a:solidFill/></p:bgPr></p:bg>"),
                    part("sldLayout", "", "", ""),
                    part("sldMaster", "", "", bg_picture),
                    "<a:solidFill/>",
                ),
                vec![],
            ),
        ];
        cases.extend(players.map(|(case, nv_pr)| {
            let shown = [(ImagePlaceholder, Body, Some(1), 1), (GraphicUnread, Body, Some(1), 1)];
            (case, one(&picture(nv_pr, "rId2"), &[PNG], &[media()]), shown.to_vec())
        }));
        for (case, parts, mut expected) in cases {
            expected.sort();
            assert_eq!(reported(&parts), expected, "{case}");
        }
    }

    #[test]
    fn a_notes_shape_nested_in_a_notes_shape_is_not_shown() {
        let tree = shape("", &format!("{}{}", text("kept"), shape("", &text("nested"))));
        let markdown = markdown_of(&notes(&tree));
        assert!(markdown.contains("kept") && !markdown.contains("nested"), "{markdown}");
    }

    #[test]
    fn unreadable_presentation_relationships_report_only_themselves() {
        let mut parts = deck(&[part("sld", "", "", "")], &[]);
        parts[1].1 = UNPARSEABLE.into();
        let parse_ctx = ParseCtx::default();
        assert_matches!(
            parse(&parse_ctx, &zip_package(&parts)),
            Err(ConvertError::Malformed { reason: "presentation has no slide list", .. })
        );
        let reported: Vec<(DiagnosticCode, Region)> =
            parse_ctx.diagnostics().iter().map(|d| (d.code, d.region)).collect();
        assert_eq!(reported, [(DiagnosticCode::PartUnreadable, Region::Body)]);
    }

    #[test]
    fn an_unresolved_slide_keeps_the_anchors_of_the_rest() {
        let link = linked("rId2", "", "back");
        let slide = part("sld", "", "", "");
        let mut parts = deck(&[slide.clone(), slide, part("sld", "", &link, "")], &[]);
        parts[1].1 = relationships(&[
            ("rId2", "slide", "slides/slide2.xml"),
            ("rId3", "slide", "slides/slide3.xml"),
        ]);
        parts.push((
            "ppt/slides/_rels/slide3.xml.rels".into(),
            relationships(&[("rId2", "slide", "slide2.xml")]),
        ));
        let doc = parse(&ParseCtx::default(), &zip_package(&parts)).unwrap();
        assert_eq!(unit_ordinals(&doc), [2, 3]);
        let markdown = crate::render::markdown::document_to_markdown(&doc);
        assert!(markdown.contains("[back](#slide-2)"), "{markdown}");
        assert!(markdown.contains("<a id=\"slide-2\"></a>"), "{markdown}");
    }

    #[test]
    fn a_skipped_or_unresolvable_entry_holds_its_number_against_max_pages() {
        use crate::limits::{Limit, Limits};
        let listed = |ids: &str| {
            format!("<p:presentation {NS}><p:sldIdLst>{ids}</p:sldIdLst></p:presentation>")
        };
        let plain = part("sld", "", "", "");
        let unread_rels = [("ppt/slides/_rels/slide2.xml.rels", UNPARSEABLE.into())];
        let mut duplicate = deck(&[plain.clone(), plain.clone()], &unread_rels);
        duplicate[0].1 = listed(
            r#"<p:sldId id="256" r:id="rId1"/><p:sldId id="257" r:id="rId1"/><p:sldId id="258" r:id="rId2"/>"#,
        );
        let mut unresolvable = deck(&[plain], &[]);
        unresolvable[0].1 =
            listed(r#"<p:sldId id="256" r:id="rId1"/><p:sldId id="257" r:id="rId9"/>"#);
        for (case, parts, listed) in
            [("duplicate", duplicate, 3), ("unresolvable", unresolvable, 2)]
        {
            let bytes = zip_package(&parts);
            let at = |max_pages| {
                let limits = Limits { max_pages, ..Limits::DEFAULT };
                let parse_ctx = ParseCtx::new(&limits);
                let parsed = parse(&parse_ctx, &bytes).map(drop);
                (parsed, parse_ctx.into_diagnostics().is_empty())
            };
            assert_matches!(at(listed), (Ok(()), false), "{case}");
            let max = listed - 1;
            assert_matches!(
                at(max),
                (Err(ConvertError::ResourceLimit { limit: Limit::Pages, max: m }), true) if m == max,
                "{case}"
            );
        }
    }

    #[test]
    fn an_unreadable_picture_without_alt_keeps_its_slide_position() {
        let tree = format!(
            "{}{}{}",
            shape("", &text("before")),
            picture("", "missing"),
            shape("", &text("after"))
        );
        let ctx = ParseCtx::default();
        let document = parse(&ctx, &zip_package(&one(&tree, &[], &[]))).unwrap();
        assert_eq!(
            crate::render::markdown::document_to_markdown(&document),
            "before\n\n\\[Image\\]\n\nafter\n"
        );
        for hidden in ["true", "1"] {
            for group in [false, true] {
                let picture = picture("", "missing");
                let tree = if group {
                    format!(
                        r#"<p:grpSp><p:nvGrpSpPr><p:cNvPr hidden="{hidden}"/></p:nvGrpSpPr>{picture}</p:grpSp>"#
                    )
                } else {
                    picture.replace("name=\"pic\"", &format!("name=\"pic\" hidden=\"{hidden}\""))
                };
                let ctx = ParseCtx::default();
                let document = parse(&ctx, &zip_package(&one(&tree, &[], &[]))).unwrap();
                assert_eq!(crate::render::markdown::document_to_markdown(&document), "");
                let diagnostics = ctx.diagnostics();
                assert_eq!(
                    diagnostics.iter().map(|d| (d.code, d.count)).collect::<Vec<_>>(),
                    [(DiagnosticCode::HiddenOmitted, 1)]
                );
            }
        }
    }

    fn markdown_of(parts: &Parts) -> String {
        let document = parse(&ParseCtx::default(), &zip_package(parts)).unwrap();
        crate::render::markdown::document_to_markdown(&document)
    }

    fn runs(runs: &[(&str, &str)]) -> String {
        let run = |(rpr, text): &(&str, &str)| format!("<a:r>{rpr}<a:t>{text}</a:t></a:r>");
        runs.iter().map(run).collect()
    }

    fn paragraphs(paragraphs: &[&[(&str, &str)]]) -> String {
        let paragraph = |para: &&[(&str, &str)]| format!("<a:p>{}</a:p>", runs(para));
        format!(
            "<p:txBody><a:bodyPr/>{}</p:txBody>",
            paragraphs.iter().map(paragraph).collect::<String>()
        )
    }

    fn face(latin: &str, sym: &str) -> String {
        let font = |tag: &str, typeface: &str| {
            if typeface.is_empty() {
                String::new()
            } else {
                format!(r#"<a:{tag} typeface="{typeface}"/>"#)
            }
        };
        format!("<a:rPr>{}{}</a:rPr>", font("latin", latin), font("sym", sym))
    }

    fn with_text_styles(slide: String, tx_styles: &str) -> Parts {
        let master = format!(
            "<p:sldMaster {NS}><p:cSld><p:spTree/></p:cSld><p:txStyles>{tx_styles}</p:txStyles></p:sldMaster>"
        );
        deck(
            &[slide],
            &[
                (
                    "ppt/slides/_rels/slide1.xml.rels",
                    relationships(&[("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml")]),
                ),
                ("ppt/slideLayouts/slideLayout1.xml", part("sldLayout", "", "", "")),
                (
                    "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
                    relationships(&[("rId1", "slideMaster", "../slideMasters/slideMaster1.xml")]),
                ),
                ("ppt/slideMasters/slideMaster1.xml", master),
            ],
        )
    }

    #[test]
    fn a_master_shift_is_inherited_by_a_run_and_cancelled_by_baseline_zero() {
        let body =
            r#"<p:bodyStyle><a:lvl1pPr><a:defRPr baseline="30000"/></a:lvl1pPr></p:bodyStyle>"#;
        let pair = [
            ("", "x"),
            (r#"<a:rPr baseline="0"/>"#, "y"),
            (r#"<a:rPr baseline="-25%"/>"#, "z"),
            (r#"<a:rPr baseline="bogus"/>"#, "w"),
        ];
        let tree = shape(r#"<p:ph type="body"/>"#, &paragraphs(&[&pair]));
        let parts = with_text_styles(part("sld", "", &tree, ""), body);
        assert_eq!(markdown_of(&parts), "<sup>x</sup>y<sub>z</sub><sup>w</sup>\n");
        let loose = shape("", &paragraphs(&[&pair]));
        assert_eq!(
            markdown_of(&with_text_styles(part("sld", "", &loose, ""), body)),
            "xy<sub>z</sub>w\n"
        );
    }

    #[test]
    fn a_title_and_a_table_cell_keep_their_shifts() {
        let title = shape(
            r#"<p:ph type="title"/>"#,
            &paragraphs(&[&[("", "H"), (r#"<a:rPr baseline="-25000"/>"#, "2"), ("", "O")]]),
        );
        let cell = format!(
            "<a:tbl><a:tr><a:tc>{}</a:tc></a:tr></a:tbl>",
            paragraphs(&[&[("", "m"), (r#"<a:rPr baseline="30%"/>"#, "2")]])
                .replace("p:txBody", "a:txBody")
        );
        let table = frame(&cell).replace("<a:graphicData>", r#"<a:graphicData uri="x">"#);
        let parts = one(&format!("{title}{table}"), &[], &[]);
        let markdown = markdown_of(&parts);
        assert!(markdown.starts_with("## H<sub>2</sub>O\n"), "{markdown}");
        assert!(markdown.contains("m<sup>2</sup>"), "{markdown}");
    }

    #[test]
    fn a_run_reads_its_latin_face_and_its_private_use_codes_through_its_symbol_face() {
        let latin = face("Symbol", "");
        let theme = face("+mn-lt", "");
        let symbol = face("", "Symbol");
        let both = face("Arial", "Symbol");
        let unrouted = face("Symbol", "Arial");
        let cases: [(&[(&str, &str)], &str); 6] = [
            (&[("", "10 "), (&latin, "m"), (&theme, "m")], "10 \u{3bc}m"),
            (&[(&symbol, "&#xF06D;m")], "\u{3bc}m"),
            (&[(&both, "&#xF06D;m")], "\u{3bc}m"),
            (&[(&latin, "&#xF06D;")], "\u{3bc}"),
            (&[(&unrouted, "&#xF06D;m")], "\u{f06d}\u{3bc}"),
            (&[("", "&#xF06D;m")], "\u{f06d}m"),
        ];
        for (para, expected) in cases {
            let parts = one(&shape("", &paragraphs(&[para])), &[], &[]);
            assert_eq!(markdown_of(&parts), format!("{expected}\n"), "{para:?}");
        }
    }

    #[test]
    fn a_face_is_inherited_through_the_cascade_and_replaced_by_a_nearer_one() {
        let body = r#"<p:bodyStyle><a:lvl1pPr><a:defRPr><a:latin typeface="Symbol"/></a:defRPr></a:lvl1pPr></p:bodyStyle>"#;
        let run = [
            ("", "m"),
            (&face("Arial", ""), "m"),
            (&face("+mn-lt", ""), "m"),
            (&face("", "Arial"), "m"),
            (&face("", ""), "m"),
        ];
        let tree = shape(r#"<p:ph type="body"/>"#, &paragraphs(&[&run]));
        let parts = with_text_styles(part("sld", "", &tree, ""), body);
        assert_eq!(markdown_of(&parts), "\u{3bc}mm\u{3bc}\u{3bc}\n");
    }

    #[test]
    fn a_dingbat_run_draws_its_characters_and_reports_the_codes_it_leaves_empty() {
        let wingdings = face("Wingdings", "");
        let hyperlink =
            r#"<a:rPr><a:latin typeface="Wingdings"/><a:hlinkClick r:id="rId9"/></a:rPr>"#;
        let tree = format!(
            "{}{}",
            shape(
                "",
                &paragraphs(&[&[
                    (&wingdings, "&#xFC;&#xFF;"),
                    ("", " ok "),
                    (&wingdings, "&#xFE;")
                ]])
            ),
            shape("", &paragraphs(&[&[(hyperlink, "&#xFF;")]])),
        );
        let parts = one(&tree, &[], &[]);
        assert_eq!(markdown_of(&parts), "\u{2713} ok [x]\n");
        assert_eq!(reported(&parts), [(DiagnosticCode::UnmappedSymbol, Region::Body, Some(1), 2)]);
    }

    #[test]
    fn a_linked_symbol_run_keeps_its_decoded_label() {
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.org/" TargetMode="External"/></Relationships>"#;
        let linked = r#"<a:rPr><a:latin typeface="Symbol"/><a:hlinkClick r:id="rId5"/></a:rPr>"#;
        let tree = shape("", &paragraphs(&[&[(linked, "m")]]));
        let rels = [("ppt/slides/_rels/slide1.xml.rels", rels.to_string())];
        let parts = deck(&[part("sld", "", &tree, "")], &rels);
        assert_eq!(markdown_of(&parts), "[\u{3bc}](https://example.org/)\n");
    }

    #[test]
    fn a_cell_spans_the_columns_and_rows_its_grid_span_and_row_span_name() {
        use crate::model::CellSlot;
        let cell = |attrs: &str| {
            format!("<a:tc{attrs}>{}</a:tc>", text("x").replace("p:txBody", "a:txBody"))
        };
        let spans = |attrs: &str| {
            let rows: String =
                [attrs, "", ""].iter().map(|a| format!("<a:tr>{}</a:tr>", cell(a))).collect();
            let table = frame(&format!("<a:tbl>{rows}</a:tbl>"));
            let document = parse(&ParseCtx::default(), &zip_package(&one(&table, &[], &[])));
            match document.unwrap().blocks.as_slice() {
                [_, Block::Table(table)] => match &table.grid[0][0] {
                    CellSlot::Origin(cell) => (cell.col_span, cell.row_span),
                    CellSlot::Covered { .. } => panic!("the first cell is covered"),
                },
                blocks => panic!("{blocks:?}"),
            }
        };
        for (attrs, expected) in [
            ("", (1, 1)),
            (r#" gridSpan="2""#, (2, 1)),
            (r#" rowSpan="3""#, (1, 3)),
            (r#" gridSpan="2" rowSpan="3""#, (2, 3)),
            (r#" gridSpan="0" rowSpan="x""#, (1, 1)),
        ] {
            assert_eq!(spans(attrs), expected, "{attrs:?}");
        }
    }

    #[test]
    fn a_shape_is_hidden_only_by_a_hidden_flag_on_its_own_non_visual_properties() {
        for (case, decoy) in [
            ("a flag on other shape properties", r#"<p:spPr><p:cNvPr hidden="1"/></p:spPr>"#),
            ("a flag under a foreign element", r#"<a:nvSpPr><p:cNvPr hidden="1"/></a:nvSpPr>"#),
        ] {
            let tree = format!("<p:sp>{decoy}{}</p:sp>", text("shown"));
            assert_eq!(markdown_of(&one(&tree, &[], &[])), "shown\n", "{case}");
        }
    }

    #[test]
    fn a_placeholder_inherits_the_layout_placeholder_of_its_idx_before_one_of_its_type() {
        let styled = |idx: &str, style: &str| {
            let lst =
                format!("<a:lstStyle><a:lvl1pPr><a:defRPr {style}/></a:lvl1pPr></a:lstStyle>");
            shape(
                &format!(r#"<p:ph type="body" idx="{idx}"/>"#),
                &format!("<p:txBody>{lst}</p:txBody>"),
            )
        };
        let layout = part(
            "sldLayout",
            "",
            &format!("{}{}", styled("1", r#"b="1""#), styled("2", r#"i="1""#)),
            "",
        );
        let slide = part("sld", "", &shape(r#"<p:ph type="body" idx="2"/>"#, &text("x")), "");
        let parts = with_master(slide, layout, part("sldMaster", "", "", ""), "");
        assert_eq!(markdown_of(&parts), "_x_\n\n---\n\n_x_\n");
    }

    #[test]
    fn a_placeholder_without_an_idx_inherits_the_layout_placeholder_of_its_own_type() {
        let styled = |kind: &str, style: &str| {
            let lst =
                format!("<a:lstStyle><a:lvl1pPr><a:defRPr {style}/></a:lvl1pPr></a:lstStyle>");
            shape(&format!(r#"<p:ph type="{kind}"/>"#), &format!("<p:txBody>{lst}</p:txBody>"))
        };
        let layout = part(
            "sldLayout",
            "",
            &format!("{}{}", styled("obj", r#"b="1""#), styled("body", r#"i="1""#)),
            "",
        );
        let slide = part("sld", "", &shape(r#"<p:ph type="body"/>"#, &text("x")), "");
        let parts = with_master(slide, layout, part("sldMaster", "", "", ""), "");
        assert_eq!(markdown_of(&parts), "_x_\n\n---\n\n_x_\n");
    }

    #[test]
    fn a_merged_continuation_cell_and_a_header_flag_read_both_spellings_of_true() {
        let cell = |attrs: &str| {
            format!("<a:tc{attrs}>{}</a:tc>", text("x").replace("p:txBody", "a:txBody"))
        };
        let table = |first_row: &str, rows: &[&str]| {
            let rows: String = rows.iter().map(|row| format!("<a:tr>{row}</a:tr>")).collect();
            let tbl = frame(&format!("<a:tbl><a:tblPr{first_row}/>{rows}</a:tbl>"));
            let document = parse(&ParseCtx::default(), &zip_package(&one(&tbl, &[], &[])));
            match document.unwrap().blocks.as_slice() {
                [_, Block::Table(table)] => {
                    (table.grid.iter().map(Vec::len).max(), table.header_rows)
                }
                blocks => panic!("{blocks:?}"),
            }
        };
        for value in ["1", "true"] {
            let spanning = [cell(r#" rowSpan="2""#), cell(&format!(r#" vMerge="{value}""#))];
            assert_eq!(table("", &[&spanning[0], &spanning[1]]), (Some(1), 0), "{value}");
            let header = format!(r#" firstRow="{value}""#);
            assert_eq!(table(&header, &[&cell("")]), (Some(1), 1), "{value}");
        }
        assert_eq!(table(r#" firstRow="0""#, &[&cell("")]), (Some(1), 0));
    }

    #[test]
    fn notes_that_show_nothing_add_no_quote() {
        let notes = part("notes", "", &shape(r#"<p:ph type="sldImg"/>"#, ""), "");
        let parts = one(
            &shape("", &text("slide")),
            &[("rId7", "notesSlide", "../notesSlides/notesSlide1.xml")],
            &[("ppt/notesSlides/notesSlide1.xml", notes)],
        );
        let document = parse(&ParseCtx::default(), &zip_package(&parts)).unwrap();
        assert!(!document.blocks.iter().any(|b| matches!(b, Block::BlockQuote(_))));
    }
}
