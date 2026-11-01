use super::styles::odf_boolean;
use super::text::{self, Ctx, FrameContent, parse_children, parse_container};
use crate::diagnostics::DiagnosticCode;
use crate::error::ConvertError;
use crate::model::{Block, HeaderFooterRegion as Side, HeaderFooterVariant as Variant, Inline};
use crate::package::xml::{Element, ns};
use crate::shared::sides::{PageSides, SideStory, SideTimeline};
use std::collections::{HashMap, HashSet};
use std::ptr;

const RUNNING: [(Side, [&str; 3]); 2] = [
    (Side::Header, ["header", "header-left", "header-first"]),
    (Side::Footer, ["footer", "footer-left", "footer-first"]),
];
const REGIONS: [&str; 3] = ["region-left", "region-center", "region-right"];

enum Shown<'e> {
    UsesDefault,
    Nothing,
    Content(&'e Element),
}

pub(super) struct MasterSides<'c, 'a, 'b> {
    ctx: &'c Ctx<'a, 'b>,
    read: HashMap<(*const Element, Side, Variant), SideStory>,
    timeline: SideTimeline,
}

impl<'c, 'a, 'b> MasterSides<'c, 'a, 'b> {
    pub(super) fn new(ctx: &'c Ctx<'a, 'b>) -> Self {
        Self { ctx, read: HashMap::new(), timeline: SideTimeline::default() }
    }

    /// Push the sides of `master` and of every master its pages lead to through
    /// `style:next-style-name`, the next page's master (ODF 1.3 Part 3, 19.503.2), onto `blocks`
    /// where they change: each once, so a cycle ends where it repeats.
    pub(super) fn publish_run(
        &mut self,
        master: &'b Element,
        blocks: &mut Vec<Block>,
    ) -> Result<(), ConvertError> {
        let mut seen = HashSet::new();
        let mut page = Some(master);
        while let Some(current) = page {
            self.ctx.parse_ctx.charge(1)?;
            if !seen.insert(ptr::from_ref(current)) {
                break;
            }
            self.publish(current, blocks)?;
            let name = current.attr(ns::STYLE, "next-style-name").unwrap_or_default();
            self.ctx.parse_ctx.charge((name.len() as u64).max(1))?;
            page = self.ctx.styles.master_page(name);
        }
        Ok(())
    }

    fn publish(
        &mut self,
        master: &'b Element,
        blocks: &mut Vec<Block>,
    ) -> Result<(), ConvertError> {
        let name = master.attr(ns::STYLE, "name").unwrap_or_default();
        for (side, [default, left, first]) in RUNNING {
            let main = shown(master.find(ns::STYLE, default), Shown::Nothing);
            let variant = |name| match main {
                Shown::Content(_) => variant_shown(master.find(ns::STYLE, name)),
                _ => Shown::UsesDefault,
            };
            let (opening, even) = (variant(first), variant(left));
            for (variant, shown) in
                [(Variant::Default, main), (Variant::First, opening), (Variant::Even, even)]
            {
                self.publish_one((side, variant), &shown, name, blocks)?;
            }
        }
        Ok(())
    }

    fn publish_one(
        &mut self,
        (side, variant): (Side, Variant),
        shown: &Shown<'b>,
        name: &str,
        blocks: &mut Vec<Block>,
    ) -> Result<(), ConvertError> {
        let parse_ctx = self.ctx.parse_ctx;
        let known;
        let story = match *shown {
            Shown::UsesDefault => {
                known = SideStory::known(side, variant, None).of_page_style(parse_ctx, name)?;
                &known
            }
            Shown::Nothing => {
                known = SideStory::known(side, variant, Some(Vec::new()))
                    .of_page_style(parse_ctx, name)?;
                &known
            }
            Shown::Content(elem) => {
                let key = (ptr::from_ref(elem), side, variant);
                if !self.read.contains_key(&key) {
                    let ctx = self.ctx;
                    let story = SideStory::read(parse_ctx, side, variant, || {
                        ctx.in_side(side.region(), || side_blocks(elem, ctx)).map(Some)
                    })?;
                    self.read.insert(key, story.of_page_style(parse_ctx, name)?);
                }
                self.read.get(&key).ok_or(ConvertError::Internal)?
            }
        };
        self.timeline.publish(parse_ctx, story, blocks)
    }
}

fn shown<'e>(side: Option<&'e Element>, absent: Shown<'e>) -> Shown<'e> {
    match side {
        None => absent,
        Some(side) if side.attr(ns::STYLE, "display").and_then(odf_boolean) == Some(false) => {
            Shown::Nothing
        }
        Some(side) => Shown::Content(side),
    }
}

fn variant_shown(side: Option<&Element>) -> Shown<'_> {
    match side {
        Some(side) if side.attr(ns::STYLE, "display").and_then(odf_boolean) != Some(false) => {
            Shown::Content(side)
        }
        _ => Shown::UsesDefault,
    }
}

fn side_blocks<'b>(side: &'b Element, ctx: &Ctx<'_, 'b>) -> Result<Vec<Block>, ConvertError> {
    let selected = ctx.selected_story(side)?;
    let side = selected.as_ref().unwrap_or(side);
    let regions: Vec<&Element> =
        REGIONS.iter().filter_map(|name| side.find(ns::STYLE, name)).collect();
    if regions.is_empty() {
        return parse_container(side, ctx);
    }
    let mut blocks = Vec::new();
    for region in regions {
        blocks.extend(parse_container(region, ctx)?);
    }
    Ok(blocks)
}

pub(super) fn text_with_masters(
    text: &Element,
    ctx: &Ctx<'_, '_>,
) -> Result<Vec<Block>, ConvertError> {
    let mut sides = MasterSides::new(ctx);
    let mut blocks = Vec::new();
    let mut opening = None;
    for child in text.child_elems() {
        in_flow(child, ctx, |elem| {
            opening = Some(elem);
            Ok(false)
        })?;
        if opening.is_some() {
            break;
        }
    }
    let opens_own = match opening {
        Some(elem) => applied(elem, ctx)?.is_some(),
        None => false,
    };
    if !opens_own && let Some(first) = ctx.styles.first_master_page() {
        sides.publish_run(first, &mut blocks)?;
    }
    walk_masters(text, ctx, &mut sides, &mut blocks)?;
    Ok(blocks)
}

fn walk_masters<'b>(
    container: &Element,
    ctx: &Ctx<'_, 'b>,
    sides: &mut MasterSides<'_, '_, 'b>,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    let children: Vec<&Element> = container.child_elems().collect();
    ctx.parse_ctx.charge(children.len() as u64)?;
    let mut start = 0;
    for (position, child) in children.iter().enumerate() {
        let mut masters = Vec::new();
        in_flow(child, ctx, |elem| {
            masters.extend(applied(elem, ctx)?);
            Ok(true)
        })?;
        if masters.is_empty() {
            continue;
        }
        let segment = children.get(start..position).unwrap_or_default();
        blocks.extend(parse_children(segment.iter().copied(), ctx)?);
        if flattened(child, ctx) {
            walk_masters(child, ctx, sides, blocks)?;
            start = position + 1;
        } else {
            for master in masters {
                sides.publish_run(master, blocks)?;
            }
            start = position;
        }
    }
    let segment = children.get(start..).unwrap_or_default();
    blocks.extend(parse_children(segment.iter().copied(), ctx)?);
    Ok(())
}

fn in_flow<'e>(
    root: &'e Element,
    ctx: &Ctx<'_, '_>,
    mut visit: impl FnMut(&'e Element) -> Result<bool, ConvertError>,
) -> Result<(), ConvertError> {
    let mut pending = vec![root];
    while let Some(elem) = pending.pop() {
        ctx.parse_ctx.charge(1)?;
        if let Some((family, _)) = styled_block(elem) {
            let hidden = family == "paragraph" && text::block_hidden(elem, ctx)?;
            if !hidden && !visit(elem)? {
                return Ok(());
            }
        } else if flattened(elem, ctx) || is_list_part(elem) {
            let start = pending.len();
            pending.extend(elem.child_elems());
            pending.get_mut(start..).unwrap_or_default().reverse();
        }
    }
    Ok(())
}

fn is_list_part(elem: &Element) -> bool {
    elem.ns.as_deref() == Some(ns::TEXT)
        && matches!(elem.local.as_str(), "list" | "list-item" | "list-header")
}

fn styled_block(elem: &Element) -> Option<(&'static str, Option<&str>)> {
    if elem.is(ns::TEXT, "p") || elem.is(ns::TEXT, "h") {
        Some(("paragraph", elem.attr(ns::TEXT, "style-name")))
    } else if elem.is(ns::TABLE, "table") {
        Some(("table", elem.attr(ns::TABLE, "style-name")))
    } else {
        None
    }
}

fn applied<'b>(elem: &Element, ctx: &Ctx<'_, 'b>) -> Result<Option<&'b Element>, ConvertError> {
    let Some((family, Some(style))) = styled_block(elem) else { return Ok(None) };
    let styles = ctx.styles;
    Ok(styles
        .style_master_page(ctx.parse_ctx, family, style)?
        .and_then(|name| styles.master_page(name)))
}

fn flattened(elem: &Element, ctx: &Ctx<'_, '_>) -> bool {
    elem.ns.as_deref() == Some(ns::TEXT)
        && match elem.local.as_str() {
            "section" => {
                ctx.parse_ctx.hidden_requested() || elem.attr(ns::TEXT, "display") != Some("none")
            }
            "index-body" | "index-title" | "table-of-content" | "alphabetical-index"
            | "bibliography" | "illustration-index" | "table-index" | "object-index"
            | "user-index" => true,
            _ => false,
        }
}

pub(super) fn read_frame(
    sides: &mut PageSides,
    frame: &Element,
    side: Side,
    ctx: &Ctx<'_, '_>,
) -> Result<(), ConvertError> {
    sides.read(ctx.parse_ctx, side, || {
        let content = ctx.in_side(side.region(), || text::frame_content(frame, ctx))?;
        Ok(match content {
            Some(FrameContent::Blocks(blocks)) => blocks,
            Some(FrameContent::Inline(inline)) => vec![Block::Paragraph(vec![inline])],
            None => Vec::new(),
        })
    })
}

pub(super) fn read_declaration(
    sides: &mut PageSides,
    decl: &Element,
    side: Side,
    ctx: &Ctx<'_, '_>,
) -> Result<(), ConvertError> {
    sides.read(ctx.parse_ctx, side, || {
        let content = if decl.attr(ns::PRESENTATION, "source") == Some("current-date") {
            ctx.parse_ctx.emit(DiagnosticCode::FieldToken, side.region());
            Inline::plain("{date}")
        } else {
            Inline::plain(crate::shared::text::clean_text(&decl.text()))
        };
        Ok(vec![Block::Paragraph(vec![content])])
    })
}
