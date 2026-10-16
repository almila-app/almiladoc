//! OOXML PresentationML (.pptx / .pptm / .ppsx): slides in `sldIdLst` order,
//! with the full text cascade - slide -> layout -> master placeholder /
//! `txStyles` -> presentation defaults. Speaker notes are included (fixed
//! policy), rendered as a quote after each slide's content.

mod cascade;

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{
    Block, Cell, Document, GridBuilder, ImageSource, Inline, LinkTarget, Style, TableKind,
    inlines_are_empty,
};
use crate::package::archive::{Role, probe_ole, report};
use crate::package::relationships::{
    RelTarget, Relationships, TargetMode, read_rels, rel_target_bytes, rel_type, rels_part_for,
};
use crate::package::xml::{Element, ns, parse_xml};
use crate::package::{Package, PackageKind, path};
use crate::shared::assets::AssetSink;
use crate::shared::delta::rebase_emphasis;
use crate::shared::fields::classify_rel_target;
use crate::shared::header::resolve_header_rows;
use crate::shared::link_label::label_is_absolute_url;
use crate::shared::list::{ListEntry, ListKey, MarkerKind, flush_list};
use crate::shared::math::{math_lines, omath_para_to_tex};
use crate::shared::mc::{alternate_branch, holds_graphic};
use crate::shared::text::clean_text;
use cascade::{Bullet, LevelStyle, Placeholder, TextProps, TitleClass};
use std::cell::{Cell as StdCell, RefCell};
use std::collections::{HashMap, HashSet};

const LAYOUT_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout";
const MASTER_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster";
const NOTES_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide";
const SLIDE_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";
const THEME_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme";
const COMMENT_RELS: &[&str] = &[
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments",
    "http://schemas.microsoft.com/office/2018/10/relationships/comments",
];
const P14: &str = "http://schemas.microsoft.com/office/powerpoint/2010/main";
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
    fn of(root: Option<&Element>) -> Backdrop {
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
        Backdrop { shapes, shows_master, background }
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
    fn read(theme: &Element) -> ThemeFills {
        let pictures = |list: &str| -> Vec<bool> {
            theme
                .first_descendant(ns::A, list)
                .map(|l| l.child_elems().map(|fill| fill.is(ns::A, "blipFill")).collect())
                .unwrap_or_default()
        };
        ThemeFills { fills: pictures("fillStyleLst"), backgrounds: pictures("bgFillStyleLst") }
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

pub fn parse(parse_ctx: &ParseCtx, bytes: &[u8]) -> Result<Document, ConvertError> {
    let pkg = match Package::open(parse_ctx, bytes, PackageKind::Opc) {
        Ok(p) => p,
        Err(e @ ConvertError::Malformed { .. }) => return Err(probe_ole(bytes).unwrap_or(e)),
        Err(e) => return Err(e),
    };
    let pkg = RefCell::new(pkg);

    let root_rels = read_rels(&mut pkg.borrow_mut(), "_rels/.rels", Role::Reported(Region::Body))?;
    let pres_part = match root_rels
        .first_of_type(rel_type::OFFICE_DOCUMENT)
        .map(|rel| path::resolve("", &rel.target))
    {
        Some(Ok(target)) => target.path,
        _ => {
            if pkg.borrow().has_part("_rels/.rels") {
                parse_ctx.emit(DiagnosticCode::ConventionalPartUsed, Region::Container);
            }
            "ppt/presentation.xml".to_string()
        }
    };
    let pres = pkg.borrow_mut().required_xml_part(&pres_part, Role::Content(Region::Body))?;
    let pres_rels =
        read_rels(&mut pkg.borrow_mut(), &rels_part_for(&pres_part), Role::Content(Region::Body))?;

    let default_text =
        cascade::parse_level_styles(pres.first_descendant(ns::P, "defaultTextStyle"));

    let mut slide_paths: Vec<(usize, String)> = Vec::new();
    let listed = pres
        .first_descendant(ns::P, "sldIdLst")
        .into_iter()
        .flat_map(|l| l.find_all(ns::P, "sldId"));
    for (position, sld_id) in listed.enumerate() {
        let target = sld_id
            .attr_qualified(ns::R, "id")
            .and_then(|rid| pres_rels.internal_target(rid))
            .and_then(|target| path::resolve(&pres_part, target).ok());
        match target {
            Some(t) => slide_paths.push((position, t.path)),
            None if pres_rels.reported() => {}
            None => {
                let _unit = parse_ctx.enter_unit(position);
                parse_ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body);
            }
        }
    }
    if slide_paths.is_empty() {
        return Err(ConvertError::malformed_part(&pres_part, "presentation has no slide list"));
    }

    let assets = RefCell::new(AssetSink::new(parse_ctx));
    let mut layouts: HashMap<String, LayoutInfo> = HashMap::new();
    let mut masters: HashMap<String, MasterInfo> = HashMap::new();
    let mut repeated: HashSet<String> = HashSet::new();
    let mut blocks: Vec<Block> = Vec::new();
    let mut failed = 0usize;
    let instance_counter = StdCell::new(0u64);
    // Every slide has a start anchor id so internal slide-to-slide links
    // resolve after concatenation; the anchor node is emitted only on
    // slides some link actually targets.
    let slide_anchors: HashMap<String, String> = slide_paths
        .iter()
        .enumerate()
        .map(|(i, (_, p))| (p.clone(), format!("slide-{}", i + 1)))
        .collect();
    let mut all_rels: Vec<Relationships> = Vec::with_capacity(slide_paths.len());
    for (position, p) in &slide_paths {
        let _unit = parse_ctx.enter_unit(*position);
        all_rels.push(read_rels(
            &mut pkg.borrow_mut(),
            &rels_part_for(p),
            Role::Content(Region::Body),
        )?);
    }
    let targeted: HashSet<String> = slide_paths
        .iter()
        .map(|(_, p)| p)
        .zip(&all_rels)
        .flat_map(|(p, rels)| {
            rels.iter()
                .filter(|(_, r)| r.rel_type == SLIDE_REL && r.mode == TargetMode::Internal)
                .filter_map(move |(_, r)| path::resolve(p, &r.target).ok().map(|t| t.path))
        })
        .filter(|t| slide_anchors.contains_key(t))
        .collect();

    for ((position, slide_path), slide_rels) in slide_paths.iter().zip(&all_rels) {
        let _unit = parse_ctx.enter_unit(*position);
        let tree = pkg.borrow_mut().related_xml_part(slide_path, Role::Reported(Region::Body))?;
        let slide = tree.as_ref().and_then(|t| t.find(ns::P, "sld"));
        let c_sld = slide.and_then(|s| s.find(ns::P, "cSld"));
        let Some(sp_tree) = c_sld.and_then(|c| c.find(ns::P, "spTree")) else {
            parse_ctx.emit(DiagnosticCode::UnitUnreadable, Region::Body);
            failed += 1;
            continue;
        };

        let layout_path = rel_target_of_type(slide_rels, slide_path, LAYOUT_REL);
        if let Some(lp) = &layout_path
            && !layouts.contains_key(lp)
        {
            let _shared = parse_ctx.outside_units();
            let info = load_layout(parse_ctx, &pkg, lp)?;
            if let Some(mp) = info.master_path.clone()
                && !masters.contains_key(&mp)
            {
                let m = load_master(parse_ctx, &pkg, &mp)?;
                masters.insert(mp, m);
            }
            layouts.insert(lp.clone(), info);
        }
        let layout = layout_path.as_deref().and_then(|lp| layouts.get_key_value(lp));
        let master = layout
            .and_then(|(_, l)| l.master_path.as_deref())
            .and_then(|mp| masters.get_key_value(mp));
        report_repeated(parse_ctx, slide, layout, master, &mut repeated);
        if slide_rels.iter().any(|(_, r)| COMMENT_RELS.contains(&r.rel_type.as_str())) {
            parse_ctx.emit(DiagnosticCode::CommentOmitted, Region::Comment);
        }
        if let Some(controls) = c_sld.and_then(|c| c.find(ns::P, "controls")) {
            let count = selected_children(parse_ctx, Region::Body, controls).len();
            let count = u64::try_from(count).unwrap_or(u64::MAX);
            parse_ctx.emit_count(DiagnosticCode::FormControlUnread, Region::Body, count);
        }
        let layout = layout.map(|(_, l)| l);
        let master = master.map(|(_, m)| m);

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
            instance_counter: &instance_counter,
            slide_anchors: &slide_anchors,
        };
        if targeted.contains(slide_path)
            && let Some(anchor) = slide_anchors.get(slide_path)
        {
            blocks.push(Block::Paragraph(vec![Inline::Anchor(anchor.clone())]));
        }
        parse_shapes(sp_tree, &ctx, &mut blocks)?;

        // Speaker notes, set off as a quote (fixed policy: included). The
        // tree is loaded before the `if let` so the package borrow is not
        // held across the body.
        let notes = match rel_target_of_type(slide_rels, slide_path, NOTES_REL) {
            Some(notes_path) => {
                let tree = rooted_part(
                    parse_ctx,
                    &pkg,
                    &notes_path,
                    Role::Content(Region::Note),
                    "notes",
                )?;
                tree.map(|t| (notes_path, t))
            }
            None => None,
        };
        if let Some((notes_path, notes_tree)) = notes {
            let notes_rels = read_rels(
                &mut pkg.borrow_mut(),
                &rels_part_for(&notes_path),
                Role::Content(Region::Note),
            )?;
            let notes_ctx = SlideCtx {
                rels: &notes_rels,
                base_part: &notes_path,
                region: Region::Note,
                layout: None,
                master: None,
                ..ctx
            };
            let mut notes_blocks = Vec::new();
            for sp in notes_tree.descendants(ns::P, "sp") {
                // Keep note text bodies (real producers use a body
                // placeholder; LibreOffice writes plain text boxes) but skip
                // the slide-image and chrome placeholders.
                match placeholder_type(sp) {
                    Some("sldImg") => continue,
                    Some(kind) if side_placeholder(parse_ctx, sp, kind) => continue,
                    _ => {}
                }
                if let Some(tx) = sp.find(ns::P, "txBody") {
                    parse_text_body(tx, &notes_ctx, None, &mut notes_blocks)?;
                }
            }
            if !notes_blocks.is_empty() {
                blocks.push(Block::BlockQuote(notes_blocks));
            }
        }
    }
    if failed == slide_paths.len() {
        return Err(ConvertError::malformed("no slide in the presentation could be read"));
    }

    let assets = std::mem::take(&mut assets.borrow_mut().assets);
    Ok(Document { blocks, notes: Vec::new(), assets })
}

fn rel_target_of_type(rels: &Relationships, base: &str, rel_type: &str) -> Option<String> {
    // first_of_type picks the lowest id, so duplicate relationships resolve
    // deterministically.
    rels.first_of_type(rel_type)
        .and_then(|rel| path::resolve(base, &rel.target).ok())
        .map(|t| t.path)
}

/// A related part's tree when it parses and its root is `p:{root}`; one
/// rooted elsewhere is reported by `role`.
fn rooted_part(
    parse_ctx: &ParseCtx,
    pkg: &RefCell<Package>,
    part: &str,
    role: Role,
    root: &str,
) -> Result<Option<Element>, ConvertError> {
    match pkg.borrow_mut().related_xml_part(part, role)? {
        Some(tree) if tree.find(ns::P, root).is_none() => {
            report(parse_ctx, role.unreadable());
            Ok(None)
        }
        tree => Ok(tree),
    }
}

fn load_layout(
    parse_ctx: &ParseCtx,
    pkg: &RefCell<Package>,
    layout_path: &str,
) -> Result<LayoutInfo, ConvertError> {
    let tree = rooted_part(parse_ctx, pkg, layout_path, Role::Content(Region::Body), "sldLayout")?;
    let placeholders = tree
        .as_ref()
        .and_then(|t| t.first_descendant(ns::P, "spTree"))
        .map(cascade::collect_placeholders)
        .unwrap_or_default();
    let backdrop = Backdrop::of(tree.as_ref().and_then(|t| t.find(ns::P, "sldLayout")));
    let rels =
        read_rels(&mut pkg.borrow_mut(), &rels_part_for(layout_path), Role::Content(Region::Body))?;
    let master_path = rel_target_of_type(&rels, layout_path, MASTER_REL);
    Ok(LayoutInfo { placeholders, master_path, backdrop })
}

fn load_master(
    parse_ctx: &ParseCtx,
    pkg: &RefCell<Package>,
    master_path: &str,
) -> Result<MasterInfo, ConvertError> {
    let mut info = MasterInfo {
        title: LevelStyle::default(),
        body: LevelStyle::default(),
        other: LevelStyle::default(),
        placeholders: Vec::new(),
        backdrop: Backdrop::of(None),
        theme: ThemeFills::default(),
    };
    if let Some(tree) =
        rooted_part(parse_ctx, pkg, master_path, Role::Content(Region::Body), "sldMaster")?
    {
        info.backdrop = Backdrop::of(tree.find(ns::P, "sldMaster"));
        if let Some(tx_styles) = tree.first_descendant(ns::P, "txStyles") {
            info.title = cascade::parse_level_styles(tx_styles.find(ns::P, "titleStyle"));
            info.body = cascade::parse_level_styles(tx_styles.find(ns::P, "bodyStyle"));
            info.other = cascade::parse_level_styles(tx_styles.find(ns::P, "otherStyle"));
        }
        if let Some(sp_tree) = tree.first_descendant(ns::P, "spTree") {
            info.placeholders = cascade::collect_placeholders(sp_tree);
        }
    }
    let rels = read_rels(&mut pkg.borrow_mut(), &rels_part_for(master_path), Role::Probe)?;
    if let Some(theme_path) = rel_target_of_type(&rels, master_path, THEME_REL)
        && let Some(theme) = pkg.borrow_mut().related_xml_part(&theme_path, Role::Probe)?
    {
        info.theme = ThemeFills::read(&theme);
    }
    Ok(info)
}

/// Report what a slide shows from its layout and master, each part once
/// (`repeated` holds those reported), and its effective background picture:
/// its own is lost, a layout's, master's or theme's repeats on every slide.
fn report_repeated(
    parse_ctx: &ParseCtx,
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
            parse_ctx.emit(DiagnosticCode::ImageUnplaced, Region::Body);
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
fn side_placeholder(parse_ctx: &ParseCtx, sp: &Element, ph_type: &str) -> bool {
    let (code, region) = match ph_type {
        kind if FOOTERS.contains(&kind) => (DiagnosticCode::FooterOmitted, Region::Footer),
        "hdr" => (DiagnosticCode::HeaderOmitted, Region::Header),
        _ => return false,
    };
    if sp.descendants(ns::A, "t").any(|t| !t.text().trim().is_empty()) {
        parse_ctx.emit(code, region);
    }
    true
}

struct SlideCtx<'a, 'b> {
    parse_ctx: &'b ParseCtx<'b>,
    pkg: &'b RefCell<Package<'a>>,
    rels: &'b Relationships,
    base_part: &'b str,
    region: Region,
    assets: &'b RefCell<AssetSink>,
    default_text: &'b LevelStyle,
    layout: Option<&'b LayoutInfo>,
    master: Option<&'b MasterInfo>,
    /// Per-text-body list instance ids, unique document-wide.
    instance_counter: &'b StdCell<u64>,
    /// Slide part path -> the slide's start anchor id, for internal
    /// slide-to-slide links.
    slide_anchors: &'b HashMap<String, String>,
}

impl SlideCtx<'_, '_> {
    /// Slide-to-slide relationships become anchor links to the target
    /// slide's start anchor; everything else classifies as usual.
    fn link_target(&self, rel: &crate::package::relationships::Relationship) -> LinkTarget {
        if rel.mode == TargetMode::Internal
            && let Ok(t) = path::resolve(self.base_part, &rel.target)
            && let Some(anchor) = self.slide_anchors.get(&t.path)
        {
            return LinkTarget::Anchor(anchor.clone());
        }
        classify_rel_target(rel.mode == TargetMode::External, &rel.target)
    }

    /// Fold the cascade for a paragraph, outermost first.
    fn base_props(&self, ph: Option<&PhInfo>, shape_styles: &LevelStyle, lvl: usize) -> TextProps {
        let mut props = self.default_text.level(lvl);
        if let Some(master) = self.master {
            let class_style = match ph.map(|p| cascade::title_class(&p.ph_type)) {
                Some(TitleClass::Title) => &master.title,
                Some(TitleClass::Body) => &master.body,
                None => &master.other,
            };
            props = props.merge(class_style.level(lvl));
            if let Some(ph) = ph
                && let Some(hit) =
                    cascade::match_placeholder(&master.placeholders, &ph.ph_type, ph.idx.as_deref())
            {
                props = props.merge(hit.styles.level(lvl));
            }
        }
        if let Some(layout) = self.layout
            && let Some(ph) = ph
            && let Some(hit) =
                cascade::match_placeholder(&layout.placeholders, &ph.ph_type, ph.idx.as_deref())
        {
            props = props.merge(hit.styles.level(lvl));
        }
        props.merge(shape_styles.level(lvl))
    }

    /// Load an internal relationship target's bytes, resolved against this
    /// part: `None` when it cannot be loaded, whose loss the caller reports.
    fn rel_part(&self, rel_id: &str) -> Result<Option<RelTarget>, ConvertError> {
        rel_target_bytes(self.pkg, self.rels, self.base_part, rel_id)
    }

    fn emit(&self, code: DiagnosticCode) {
        self.parse_ctx.emit(code, self.region);
    }

    fn emit_related(&self, code: DiagnosticCode) {
        if !self.rels.reported() {
            self.emit(code);
        }
    }

    /// A chart or diagram part's tree, rooted at `namespace:root`; one that
    /// is unnamed, absent, does not parse or is rooted elsewhere is reported.
    fn graphic_part(
        &self,
        rel_id: Option<&str>,
        namespace: &str,
        root: &str,
    ) -> Result<Option<Element>, ConvertError> {
        let loaded = match rel_id {
            Some(rel_id) => self.rel_part(rel_id)?,
            None => None,
        };
        let Some((_, bytes)) = loaded else {
            match rel_id {
                Some(_) => self.emit_related(DiagnosticCode::GraphicUnread),
                None => self.emit(DiagnosticCode::GraphicUnread),
            }
            return Ok(None);
        };
        match parse_xml(self.parse_ctx, &bytes, Role::Reported(self.region)) {
            Ok(tree) if tree.find(namespace, root).is_some() => Ok(Some(tree)),
            Err(e) if e.is_fatal() => Err(e),
            Ok(_) | Err(_) => {
                self.emit(DiagnosticCode::GraphicUnread);
                Ok(None)
            }
        }
    }
}

struct PhInfo {
    ph_type: String,
    idx: Option<String>,
}

fn placeholder_type(sp: &Element) -> Option<&str> {
    sp.first_descendant(ns::P, "ph").map(|ph| ph.attr(ns::P, "type").unwrap_or("body"))
}

/// Whether a slide shows `elem`: its slide-number, date and footer
/// placeholders it does not.
fn renders(elem: &Element) -> bool {
    !(elem.is(ns::P, "sp") || elem.is(ns::P, "cxnSp"))
        || !placeholder_type(elem).is_some_and(|kind| FOOTERS.contains(&kind))
}

/// `parent`'s children in order, each `mc:AlternateContent` replaced by the
/// children of its selected branch, whose declined choices report in `region`.
fn selected_children<'e>(
    parse_ctx: &ParseCtx,
    region: Region,
    parent: &'e Element,
) -> Vec<&'e Element> {
    let mut selected = Vec::new();
    let mut open: Vec<&Element> = parent.child_elems().collect();
    open.reverse();
    while let Some(elem) = open.pop() {
        if elem.is(ns::MC, "AlternateContent") {
            let branch = alternate_branch(parse_ctx, region, elem, SUPPORTED_NS, renders);
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
    ctx: &SlideCtx,
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
        match child.local.as_str() {
            // Connector shapes carry the same `p:txBody` as plain shapes.
            "sp" | "cxnSp" => parse_shape(child, ctx, blocks)?,
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
    ctx: &SlideCtx,
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
    let descr = pic
        .first_descendant(ns::P, "cNvPr")
        .and_then(|p| p.attr(ns::P, "descr"))
        .map(clean_text)
        .unwrap_or_default();
    let rid = pic
        .first_descendant(ns::A, "blip")
        .and_then(|b| b.attr_qualified(ns::R, "embed").or_else(|| b.attr_qualified(ns::R, "link")));
    let source = match rid {
        Some(rid) => crate::shared::assets::rel_image_source(
            ctx.pkg,
            ctx.rels,
            ctx.base_part,
            ctx.assets,
            rid,
        )?,
        None => None,
    };
    match (&source, rid) {
        (Some(_), _) => {}
        (None, Some(_)) => ctx.emit_related(DiagnosticCode::ImageUnavailable),
        (None, None) => ctx.emit(DiagnosticCode::ImageUnavailable),
    }
    if source.is_some() || !descr.trim().is_empty() {
        blocks.push(Block::Paragraph(vec![Inline::Image {
            alt: descr,
            source: source.unwrap_or(ImageSource::Unavailable),
        }]));
    }
    Ok(())
}

fn parse_shape(sp: &Element, ctx: &SlideCtx, blocks: &mut Vec<Block>) -> Result<(), ConvertError> {
    let ph = sp.first_descendant(ns::P, "ph").map(|p| PhInfo {
        ph_type: p.attr(ns::P, "type").unwrap_or("body").to_string(),
        idx: p.attr(ns::P, "idx").map(str::to_string),
    });
    if sp.find(ns::P, "spPr").is_some_and(|pr| pr.find(ns::A, "blipFill").is_some()) {
        ctx.emit(DiagnosticCode::ImageUnplaced);
    }
    if let Some(ph) = &ph
        && ph.ph_type != "hdr"
        && side_placeholder(ctx.parse_ctx, sp, &ph.ph_type)
    {
        return Ok(());
    }
    let Some(tx) = sp.find(ns::P, "txBody") else {
        return Ok(());
    };
    // Titles get heading semantics but keep their shape-order position.
    if ph.as_ref().is_some_and(|p| cascade::title_class(&p.ph_type) == TitleClass::Title) {
        push_title_heading(tx, ctx, ph.as_ref(), blocks)?;
    } else {
        parse_text_body(tx, ctx, ph.as_ref(), blocks)?;
    }
    Ok(())
}

/// Collapse a title placeholder's paragraphs into one slide heading. Runs
/// resolve through the full cascade like body text.
fn push_title_heading(
    tx: &Element,
    ctx: &SlideCtx,
    ph: Option<&PhInfo>,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    let shape_styles = cascade::parse_level_styles(tx.find(ns::A, "lstStyle"));
    let mut inlines: Vec<Inline> = Vec::new();
    for p in tx.find_all(ns::A, "p") {
        let ppr = p.find(ns::A, "pPr");
        let lvl: usize =
            ppr.and_then(|pr| pr.attr(ns::A, "lvl")).and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut props = ctx.base_props(ph, &shape_styles, lvl);
        if let Some(ppr) = ppr {
            props = props.merge(cascade::paragraph_props(ppr));
        }
        let base = props.delta.resolve();
        let mut para = parse_para_inlines(p, ctx, base);
        if inlines_are_empty(&para) {
            continue;
        }
        rebase_emphasis(&mut para, base);
        if !inlines.is_empty() {
            inlines.push(Inline::LineBreak);
        }
        inlines.extend(para);
    }
    if !inlines_are_empty(&inlines) {
        let anchor = Some(crate::model::inlines_to_plain_text(&inlines));
        blocks.push(Block::Heading { level: 2, anchor, content: inlines });
    }
    Ok(())
}

fn parse_text_body(
    tx: &Element,
    ctx: &SlideCtx,
    ph: Option<&PhInfo>,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    let shape_styles = cascade::parse_level_styles(tx.find(ns::A, "lstStyle"));
    let instance = {
        let id = ctx.instance_counter.get() + 1;
        ctx.instance_counter.set(id);
        id
    };
    let mut counters = [0u64; cascade::LEVELS];
    let mut started = [false; cascade::LEVELS];
    let mut list_run: Vec<ListEntry> = Vec::new();

    for p in tx.find_all(ns::A, "p") {
        let ppr = p.find(ns::A, "pPr");
        let lvl: usize =
            ppr.and_then(|pr| pr.attr(ns::A, "lvl")).and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut props = ctx.base_props(ph, &shape_styles, lvl);
        if let Some(ppr) = ppr {
            props = props.merge(cascade::paragraph_props(ppr));
        }
        let base = props.delta.resolve();
        let inlines = parse_para_inlines(p, ctx, base);
        if inlines_are_empty(&inlines) {
            flush_list(blocks, &mut list_run);
            continue;
        }
        match props.bullet {
            Bullet::AutoNum { marker, start, wrap } => {
                let lvl_idx = lvl.min(cascade::LEVELS - 1);
                let number = if started[lvl_idx] {
                    counters[lvl_idx].saturating_add(1)
                } else {
                    started[lvl_idx] = true;
                    start
                };
                counters[lvl_idx] = number;
                for deeper in started.iter_mut().skip(lvl_idx + 1) {
                    *deeper = false;
                }
                list_run.push(ListEntry {
                    level: lvl,
                    key: ListKey { instance, marker },
                    number,
                    label: wrap.label(marker, number),
                    blocks: vec![Block::Paragraph(inlines)],
                });
            }
            Bullet::Char => list_run.push(ListEntry {
                level: lvl,
                key: ListKey { instance, marker: MarkerKind::Bullet },
                number: 0,
                label: None,
                blocks: vec![Block::Paragraph(inlines)],
            }),
            Bullet::None | Bullet::Inherit => {
                flush_list(blocks, &mut list_run);
                match math_lines(&inlines) {
                    Some(lines) => blocks.extend(lines.into_iter().map(Block::Math)),
                    None => blocks.push(Block::Paragraph(inlines)),
                }
            }
        }
    }
    flush_list(blocks, &mut list_run);
    Ok(())
}

fn parse_para_inlines(p: &Element, ctx: &SlideCtx, base: Style) -> Vec<Inline> {
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
                let text =
                    clean_text(&child.find(ns::A, "t").map(|t| t.text()).unwrap_or_default());
                if text.is_empty() {
                    continue;
                }
                let style = match rpr {
                    Some(rpr) => cascade::rpr_delta(rpr).apply(base),
                    None => base,
                };
                let click = rpr.and_then(|r| r.find(ns::A, "hlinkClick"));
                let rid =
                    click.and_then(|h| h.attr_qualified(ns::R, "id")).filter(|r| !r.is_empty());
                let target = rid.and_then(|rid| ctx.rels.get(rid)).map(|rel| ctx.link_target(rel));
                let jump = click
                    .and_then(|h| h.attr(ns::A, "action"))
                    .is_some_and(|action| NAVIGATION.iter().any(|n| action.starts_with(n)));
                if rid.is_some() && target.is_none() && !jump && !label_is_absolute_url(&text) {
                    ctx.emit_related(DiagnosticCode::LinkTargetUnread);
                }
                let inline = Inline::Text { text, style };
                match target {
                    Some(target) => out.push(Inline::Link { content: vec![inline], target }),
                    None => out.push(inline),
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
    ctx: &SlideCtx,
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
        ctx.emit(DiagnosticCode::EmbeddedObject);
        let prog_id = ole.attr(ns::P, "progId").unwrap_or("object");
        let name = ole.attr(ns::P, "name").unwrap_or("").trim();
        let alt =
            if name.is_empty() { format!("Embedded object: {prog_id}") } else { name.to_string() };
        let source = match ole.attr_qualified(ns::R, "id") {
            Some(rid) => match ctx.rel_part(rid)? {
                Some((part, bytes)) => Some(ImageSource::Asset(ctx.assets.borrow_mut().add(
                    "application/vnd.ms-ole-object".into(),
                    part,
                    &bytes,
                )?)),
                None => None,
            },
            None => None,
        };
        blocks.push(Block::Paragraph(vec![Inline::Image {
            alt,
            source: source.unwrap_or(ImageSource::Unavailable),
        }]));
        return Ok(());
    }
    if let Some(chart) = first_in(&content, ns::CHART, "chart") {
        let tree = ctx.graphic_part(chart.attr_qualified(ns::R, "id"), ns::CHART, "chartSpace")?;
        if let Some(tree) = tree {
            blocks.extend(crate::shared::drawingml::chart_blocks(ctx.parse_ctx, &tree, ctx.region));
        }
        return Ok(());
    }
    if let Some(rel_ids) = first_in(&content, ns::DGM, "relIds") {
        let tree = ctx.graphic_part(rel_ids.attr_qualified(ns::R, "dm"), ns::DGM, "dataModel")?;
        if let Some(tree) = tree {
            blocks.extend(crate::shared::drawingml::diagram_blocks(&tree));
        }
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
fn parse_table(tbl: &Element, ctx: &SlideCtx, blocks: &mut Vec<Block>) -> Result<(), ConvertError> {
    let header_rows = usize::from(
        tbl.find(ns::A, "tblPr")
            .is_some_and(|p| matches!(p.attr(ns::A, "firstRow"), Some("1") | Some("true"))),
    );
    let mut builder = GridBuilder::new(ctx.parse_ctx.limits().max_expansion);
    for tr in tbl.find_all(ns::A, "tr") {
        builder.next_row();
        for tc in tr.find_all(ns::A, "tc") {
            let merged = matches!(tc.attr(ns::A, "hMerge"), Some("1") | Some("true"))
                || matches!(tc.attr(ns::A, "vMerge"), Some("1") | Some("true"));
            if merged {
                builder.covered();
                continue;
            }
            let col_span: u32 =
                tc.attr(ns::A, "gridSpan").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
            let row_span: u32 =
                tc.attr(ns::A, "rowSpan").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
            let mut cell_blocks = Vec::new();
            if let Some(tx) = tc.find(ns::A, "txBody") {
                parse_text_body(tx, ctx, None, &mut cell_blocks)?;
            }
            builder.place(Cell::spanning(cell_blocks, col_span, row_span))?;
        }
    }
    let mut table = builder.finish(TableKind::Data);
    if table.grid.is_empty() {
        return Ok(());
    }
    table.header_rows = resolve_header_rows(&table, header_rows);
    blocks.push(Block::Table(table));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

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

    fn zipped(parts: &Parts) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in parts {
            w.start_file(name.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn reported(parts: &Parts) -> Vec<Reported> {
        let parse_ctx = ParseCtx::default();
        parse(&parse_ctx, &zipped(parts)).unwrap();
        let mut reported: Vec<Reported> = parse_ctx
            .diagnostics()
            .iter()
            .map(|d| (d.code, d.region, d.unit.map(std::num::NonZeroU32::get), d.count))
            .collect();
        reported.sort();
        reported
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

    fn slide_xml(attrs: &str, tree: &str, c_sld_tail: &str) -> String {
        format!(
            "<p:sld {NS} {attrs}><p:cSld>{c_sld_tail}<p:spTree>{tree}</p:spTree></p:cSld></p:sld>"
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
        deck(&[slide_xml("", tree, "")], &all)
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
    fn each_loss_is_reported_in_its_region() {
        use DiagnosticCode::*;
        use Region::{Body, Comment, Container, Footer, Header, Note};
        let plain = slide_xml("", &shape("", &text("kept")), "");
        let logo = shape("", "");
        let placeholder = shape(r#"<p:ph type="body"/>"#, "");
        let bg_picture = r#"<p:bg><p:bgPr><a:blipFill/></p:bgPr></p:bg>"#;
        let bg_theme = r#"<p:bg><p:bgRef idx="1002"/></p:bg>"#;
        let notes_in = |root: &str, tree: &str| {
            one(
                "",
                &[("rId7", "notesSlide", "../notesSlides/notesSlide1.xml")],
                &[("ppt/notesSlides/notesSlide1.xml", part(root, "", tree, ""))],
            )
        };
        let notes = |tree: &str| notes_in("notes", tree);
        let controls = |inner: &str| {
            let controls = format!("</p:spTree><p:controls>{inner}</p:controls>");
            deck(&[slide_xml("", "", "").replace("</p:spTree>", &controls)], &[])
        };
        let unreadable_rels = [("ppt/slides/_rels/slide1.xml.rels", UNPARSEABLE.into())];
        let footer = shape(r#"<p:ph type="sldNum"/>"#, &text("3"));
        let chart =
            |xml: &str| one(&frame(CHART), GRAPHICS, &[("ppt/charts/chart1.xml", xml.into())]);
        let numbered = slide_xml("", &shape(r#"<p:ph type="sldNum"/>"#, &text("2")), "");
        let mut unresolved = deck(&[plain.clone(), numbered], &[]);
        unresolved[1].1 = relationships(&[("rId2", "slide", "slides/slide2.xml")]);
        let mut unread = deck(&[plain.clone(), plain.clone()], &[]);
        unread.retain(|(name, _)| name != "ppt/slides/slide1.xml");
        let ole = r#"<p:oleObj progId="Equation.3" r:id="rId5"/>"#;
        let cases: Vec<(&str, Parts, Vec<Reported>)> = vec![
            (
                "a conventional presentation part",
                {
                    let mut parts = one("", &[], &[]);
                    parts.push(("_rels/.rels".into(), relationships(&[])));
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
                vec![],
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
                "a movie with its poster",
                one(&picture(r#"<a:videoFile r:link="rId8"/>"#, "rId2"), &[PNG], &[media()]),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "an audio file",
                one(&picture(r#"<a:audioFile r:link="rId8"/>"#, "rId2"), &[PNG], &[media()]),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "a wave sound",
                one(&picture(r#"<a:wavAudioFile r:embed="rId8"/>"#, "rId2"), &[PNG], &[media()]),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "a QuickTime movie",
                one(&picture(r#"<a:quickTimeFile r:link="rId8"/>"#, "rId2"), &[PNG], &[media()]),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "a movie named only by its 2010 extension",
                one(
                    &picture(
                        r#"<p:extLst><p:ext><p14:media r:embed="rId9"/></p:ext></p:extLst>"#,
                        "rId2",
                    ),
                    &[PNG],
                    &[media()],
                ),
                vec![(GraphicUnread, Body, Some(1), 1)],
            ),
            (
                "a sound",
                one(&picture(r#"<a:audioCd/>"#, "rId2"), &[PNG], &[media()]),
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
                vec![(PartUnreadable, Body, Some(1), 1)],
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
                vec![(ImageUnplaced, Body, Some(1), 1), (FooterOmitted, Footer, Some(1), 1)],
            ),
            (
                "the slide's own background picture",
                deck(&[slide_xml("", "", bg_picture)], &[]),
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
                    slide_xml(r#"showMasterSp="false""#, "", ""),
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
                    plain.clone(),
                    part("sldLayout", "", "", ""),
                    part("sldMaster", "", "", bg_theme),
                    "<a:blipFill/><a:solidFill/>",
                ),
                vec![],
            ),
            (
                "a slide fill over a master picture",
                with_master(
                    slide_xml("", "", r#"<p:bg><p:bgPr><a:solidFill/></p:bgPr></p:bg>"#),
                    part("sldLayout", "", "", ""),
                    part("sldMaster", "", "", bg_picture),
                    "<a:solidFill/>",
                ),
                vec![],
            ),
        ];
        for (case, parts, mut expected) in cases {
            expected.sort();
            assert_eq!(reported(&parts), expected, "{case}");
        }
    }

    #[test]
    fn unreadable_presentation_relationships_report_only_themselves() {
        let mut parts = deck(&[slide_xml("", "", "")], &[]);
        parts[1].1 = UNPARSEABLE.into();
        let parse_ctx = ParseCtx::default();
        assert!(parse(&parse_ctx, &zipped(&parts)).is_err());
        let reported: Vec<(DiagnosticCode, Region)> =
            parse_ctx.diagnostics().iter().map(|d| (d.code, d.region)).collect();
        assert_eq!(reported, [(DiagnosticCode::PartUnreadable, Region::Body)]);
    }

    #[test]
    fn an_unresolved_slide_keeps_the_anchors_of_the_rest() {
        let link = r#"<p:sp><p:txBody><a:p><a:r><a:rPr><a:hlinkClick r:id="rId2"/></a:rPr><a:t>back</a:t></a:r></a:p></p:txBody></p:sp>"#;
        let mut parts =
            deck(&[slide_xml("", "", ""), slide_xml("", "", ""), slide_xml("", link, "")], &[]);
        parts[1].1 = relationships(&[
            ("rId2", "slide", "slides/slide2.xml"),
            ("rId3", "slide", "slides/slide3.xml"),
        ]);
        parts.push((
            "ppt/slides/_rels/slide3.xml.rels".into(),
            relationships(&[("rId2", "slide", "slide2.xml")]),
        ));
        let doc = parse(&ParseCtx::default(), &zipped(&parts)).unwrap();
        let anchors: Vec<&str> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph(inlines) => match inlines.as_slice() {
                    [Inline::Anchor(anchor)] => Some(anchor.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(anchors, ["slide-1"]);
    }
}
