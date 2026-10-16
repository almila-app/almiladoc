//! OOXML markup-compatibility (`mc:AlternateContent`) branch selection.

use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::package::xml::{Element, ns};

const CHARTEX: &str = "http://schemas.microsoft.com/office/drawing/2014/chartex";

/// Pick the branch of an `mc:AlternateContent` to process: the first
/// `mc:Choice` whose `Requires` namespaces are all supported, else the
/// `mc:Fallback`. When no choice is taken, what the declined choices hold is
/// reported in `region`: a chart, diagram or ink always, and a picture or
/// text only when the fallback shows none, where `renders` tells which
/// subtrees the frontend shows.
pub fn alternate_branch<'e>(
    parse_ctx: &ParseCtx,
    region: Region,
    alt: &'e Element,
    supported: &[&str],
    renders: fn(&Element) -> bool,
) -> Option<&'e Element> {
    let branch = alt
        .find_all(ns::MC, "Choice")
        .find(|c| choice_supported(c, supported))
        .or_else(|| alt.find(ns::MC, "Fallback"));
    if branch.is_none_or(|b| b.is(ns::MC, "Fallback")) {
        let declined: Vec<&Element> = alt.find_all(ns::MC, "Choice").collect();
        let lost = |hit: fn(&Element) -> bool| {
            declined.iter().any(|c| shows(c, renders, hit))
                && !branch.is_some_and(|b| shows(b, renders, hit))
        };
        if declined.iter().any(|c| holds_graphic(c)) {
            parse_ctx.emit(DiagnosticCode::GraphicUnread, region);
        }
        if lost(is_picture) {
            parse_ctx.emit(DiagnosticCode::ImageUnplaced, region);
        }
        if lost(is_text) {
            parse_ctx.emit(DiagnosticCode::BlockUnread, region);
        }
    }
    branch
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
pub fn holds_graphic(elem: &Element) -> bool {
    elem.descendant_elems().any(|e| {
        e.is(ns::CHART, "chart")
            || e.is(CHARTEX, "chart")
            || e.is(ns::DGM, "relIds")
            || e.local == "contentPart"
    })
}

fn shows(elem: &Element, renders: fn(&Element) -> bool, hit: fn(&Element) -> bool) -> bool {
    let mut open: Vec<&Element> = elem.child_elems().collect();
    while let Some(e) = open.pop() {
        if renders(e) {
            if hit(e) {
                return true;
            }
            open.extend(e.child_elems());
        }
    }
    false
}

fn is_picture(elem: &Element) -> bool {
    elem.is(ns::A, "blip") || elem.is(ns::VML, "imagedata")
}

fn is_text(elem: &Element) -> bool {
    (elem.is(ns::W, "t") || elem.is(ns::A, "t")) && !elem.text().trim().is_empty()
}
