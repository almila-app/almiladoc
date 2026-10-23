//! OOXML markup-compatibility (`mc:AlternateContent`) branch selection.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::package::xml::{Element, ns};

const CHARTEX: &str = "http://schemas.microsoft.com/office/drawing/2014/chartex";

/// Which subtrees a frontend shows, and which of those it reads in the branch it takes.
pub(crate) type Shown = (fn(&Element) -> bool, fn(&Element) -> bool);

/// Pick the branch of an `mc:AlternateContent` to process: the first
/// `mc:Choice` whose `Requires` namespaces are all supported, else the
/// `mc:Fallback`. When no choice is taken, what the declined choices hold is
/// reported in `region`: a chart, diagram or ink always, and a picture or
/// text only when the fallback shows none, where `renders` tells which
/// subtrees the frontend shows and `reads` which of them it reads in the
/// branch it takes, and `shape` is the VML shape that holds `alt`.
pub(crate) fn alternate_branch<'e>(
    parse_ctx: &ParseCtx<'_>,
    region: Region,
    alt: &'e Element,
    shape: Option<&Element>,
    supported: &[&str],
    (renders, reads): Shown,
) -> Option<&'e Element> {
    let branch = selected_branch(alt, supported);
    if branch.is_none_or(|b| b.is(ns::MC, "Fallback")) {
        let declined: Vec<&Element> = alt.find_all(ns::MC, "Choice").collect();
        let lost = |hit: Hit| {
            declined.iter().any(|c| shows(c, shape, renders, hit))
                && !branch.is_some_and(|b| shows(b, shape, reads, hit))
        };
        if declined.iter().any(|c| holds_graphic(c)) {
            parse_ctx.emit(DiagnosticCode::GraphicUnread, region);
        }
        if lost(is_picture) {
            parse_ctx.image_loss(crate::ctx::ImageLoss::Unplaced, region);
        }
        if lost(|elem, _| is_text(elem)) {
            parse_ctx.emit(DiagnosticCode::BlockUnread, region);
        }
    }
    branch
}

/// The branch [`alternate_branch`] picks, without reporting what the declined choices hold.
pub(crate) fn selected_branch<'e>(alt: &'e Element, supported: &[&str]) -> Option<&'e Element> {
    alt.find_all(ns::MC, "Choice")
        .find(|c| choice_supported(c, supported))
        .or_else(|| alt.find(ns::MC, "Fallback"))
}

fn choice_supported(choice: &Element, supported: &[&str]) -> bool {
    let Some(requires) = choice.attr(ns::MC, "Requires") else {
        return true;
    };
    // Requires prefixes were resolved to namespace URIs at parse time, in
    // the element's own lexical scope; an unresolved prefix stays literal
    // and matches nothing.
    requires.split_whitespace().all(|uri| supported.contains(&uri))
}

/// Whether `elem` holds a chart, diagram or ink, which a declined `mc:Choice` reports as unread.
pub(crate) fn holds_graphic(elem: &Element) -> bool {
    elem.descendant_elems().any(|e| is_chart_or_diagram(e) || e.local == "contentPart")
}

pub(crate) fn is_chart_or_diagram(elem: &Element) -> bool {
    elem.is(ns::CHART, "chart") || elem.is(CHARTEX, "chart") || elem.is(ns::DGM, "relIds")
}

type Hit = fn(&Element, Option<&Element>) -> bool;

fn shows(elem: &Element, shape: Option<&Element>, renders: fn(&Element) -> bool, hit: Hit) -> bool {
    let mut open: Vec<(&Element, Option<&Element>)> =
        elem.child_elems().map(|child| (child, shape)).collect();
    while let Some((e, shape)) = open.pop() {
        if renders(e) {
            if hit(e, shape) {
                return true;
            }
            let shape = if is_vml_shape(e) {
                Some(e)
            } else if e.is(ns::W, "txbxContent") {
                None
            } else {
                shape
            };
            open.extend(e.child_elems().map(|child| (child, shape)));
        }
    }
    false
}

fn is_picture(elem: &Element, shape: Option<&Element>) -> bool {
    elem.is(ns::A, "blip") || elem.is(ns::VML, "imagedata") || is_picture_fill(elem, shape)
}

const PICTURE_FILL_TYPES: [&str; 2] = ["frame", "tile"];

const VML_SHAPE_ELEMENTS: [&str; 9] =
    ["shape", "rect", "roundrect", "oval", "line", "polyline", "arc", "curve", "image"];

pub(crate) fn is_vml_shape(elem: &Element) -> bool {
    elem.ns.as_deref() == Some(ns::VML) && VML_SHAPE_ELEMENTS.contains(&elem.local.as_str())
}

/// A VML `v:fill` that is on, is a `frame` or `tile`, and names its image by `r:id` or
/// `o:relid`: a `pattern` fill is a decorative bitmap, like `a:pattFill`.
pub(crate) fn is_picture_fill(fill: &Element, holder: Option<&Element>) -> bool {
    fill.is(ns::VML, "fill")
        && fill.attr_unqualified("type").is_some_and(|kind| PICTURE_FILL_TYPES.contains(&kind))
        && !fill_switch(fill, holder).is_some_and(|on| matches!(on, "f" | "false"))
        && (fill.attr_qualified(ns::R, "id").is_some()
            || fill.attr_qualified(ns::O_VML, "relid").is_some())
}

fn fill_switch<'e>(fill: &'e Element, holder: Option<&'e Element>) -> Option<&'e str> {
    fill.attr_unqualified("on").or_else(|| holder?.attr_unqualified("filled"))
}

/// Text [`alternate_branch`] reports a declined choice holding.
pub(crate) fn is_text(elem: &Element) -> bool {
    (elem.is(ns::W, "t") || elem.is(ns::A, "t")) && !elem.text().trim().is_empty()
}
