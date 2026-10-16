//! Form control checkboxes: drawing objects floating over the grid, each
//! anchored to a cell. The OOXML containers (xlsx, xlsm, xlsb) keep them in
//! the worksheet's legacy VML drawing part; BIFF keeps them in OBJ records.
//! Either way a checkbox lands in the cell its anchor starts in.

use super::grid::CellRef;
use crate::ctx::ParseCtx;
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::Inline;
use crate::package::Package;
use crate::package::archive::Role;
use crate::package::path;
use crate::package::relationships::{Relationships, TargetMode};
use crate::package::xml::{Element, ns};
use crate::shared::text::{clean_text, collapse_ws};
use std::collections::HashMap;

const VML_DRAWING_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/vmlDrawing";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Checkbox {
    pub(super) checked: bool,
    pub(super) caption: String,
}

/// Checkboxes anchored in each cell, in drawing order.
pub(super) type Checkboxes = HashMap<CellRef, Vec<Checkbox>>;

/// A cell's inlines: its own text, then each checkbox with its caption.
pub(super) fn cell_inlines(text: Option<String>, boxes: &[Checkbox]) -> Vec<Inline> {
    let mut out: Vec<Inline> = text.into_iter().map(Inline::plain).collect();
    for b in boxes {
        if !out.is_empty() {
            out.push(Inline::plain(" "));
        }
        out.push(Inline::Checkbox(b.checked));
        if !b.caption.is_empty() {
            out.push(Inline::plain(format!(" {}", b.caption)));
        }
    }
    out
}

/// The checkboxes in the VML drawings a worksheet part's relationships
/// name; hidden form controls, and those the grid cannot show, are
/// reported.
pub(super) fn read_vml_checkboxes(
    parse_ctx: &ParseCtx,
    pkg: &mut Package,
    sheet_part: &str,
    rels: &Relationships,
) -> Result<Checkboxes, ConvertError> {
    let mut out = Checkboxes::new();
    let mut targets: Vec<String> = rels
        .iter()
        .filter(|(_, r)| r.rel_type == VML_DRAWING_REL && r.mode == TargetMode::Internal)
        .filter_map(|(_, r)| path::resolve(sheet_part, &r.target).ok())
        .map(|t| t.path)
        .collect();
    targets.sort();
    targets.dedup();
    for target in targets {
        if let Some(root) = pkg.related_xml_part(&target, Role::Content(Region::Body))? {
            vml_checkboxes(parse_ctx, &root, &mut out);
        }
    }
    Ok(out)
}

/// The form control kinds a legacy drawing holds beside checkboxes, each of
/// which shows a caption, state or selection the grid does not hold.
const OTHER_CONTROLS: [&str; 9] =
    ["Radio", "Label", "Button", "GBox", "Drop", "List", "Spin", "Scroll", "Edit"];

fn vml_checkboxes(parse_ctx: &ParseCtx, root: &Element, out: &mut Checkboxes) {
    for shape in root.descendants(ns::VML, "shape") {
        let Some(data) = shape.find(ns::X_VML, "ClientData") else {
            continue;
        };
        let kind = data.attr_unqualified("ObjectType").unwrap_or_default();
        let checkbox = kind == "Checkbox";
        if !checkbox && !OTHER_CONTROLS.contains(&kind) {
            continue;
        }
        if shape
            .attr_unqualified("style")
            .is_some_and(|s| s.replace(' ', "").contains("visibility:hidden"))
        {
            parse_ctx.emit(DiagnosticCode::HiddenOmitted, Region::Body);
            continue;
        }
        let at = data.find(ns::X_VML, "Anchor").and_then(|a| anchor_cell(&a.text()));
        // Absent means unchecked; 2 is the mixed state, which has no token.
        let checked = match data.find(ns::X_VML, "Checked").map(|c| c.text()) {
            None => Some(false),
            Some(v) => match v.trim() {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            },
        };
        let (Some(at), Some(checked), true) = (at, checked, checkbox) else {
            parse_ctx.emit(DiagnosticCode::FormControlUnread, Region::Body);
            continue;
        };
        let caption = shape
            .find(ns::VML, "textbox")
            .map(|t| collapse_ws(&clean_text(&t.text())).trim().to_string())
            .unwrap_or_default();
        out.entry(at).or_default().push(Checkbox { checked, caption });
    }
}

/// The cell an `x:Anchor` starts in: `LeftColumn, LeftOffset, TopRow, TopOffset, ...`.
fn anchor_cell(anchor: &str) -> Option<CellRef> {
    let mut parts = anchor.split(',').map(|p| p.trim().parse::<u32>().ok());
    let col = parts.next()??;
    parts.next()??;
    let row = parts.next()??;
    Some(CellRef::new(row, col))
}
