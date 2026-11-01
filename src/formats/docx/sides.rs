//! Section headers and footers (ECMA-376 Part 1, 17.10): each section shows the parts it names
//! and inherits from the section before it those it leaves unnamed.

use super::content::{self, Ctx, References, SectionBreaks};
use super::numbering::Counters;
use super::styles::on_off;
use super::{TypedPart, typed_part_path};
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::model::{Block, HeaderFooterRegion as Side, HeaderFooterVariant as Variant};
use crate::package::archive::{Role, report};
use crate::package::relationships::{read_rels, rel_target_path, rels_part_for};
use crate::package::xml::{Element, ns};
use crate::shared::sides::{SideStory, SideTimeline};
use std::cell::RefCell;
use std::collections::HashMap;

const SETTINGS: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings";
const SIDES: [(Side, &str, &str); 2] =
    [(Side::Header, "headerReference", "hdr"), (Side::Footer, "footerReference", "ftr")];
const VARIANTS: [(Variant, &str); 3] =
    [(Variant::Default, "default"), (Variant::First, "first"), (Variant::Even, "even")];

/// What a section's reference names: a part, or a relationship that does not resolve.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Named {
    Part(String),
    Broken,
}

/// The scopes every header and footer part is read in, apart from the body's.
pub(super) struct SideScopes {
    counters: RefCell<Counters>,
    references: References,
}

impl SideScopes {
    pub(super) fn comment_ids(&self) -> Vec<String> {
        self.references.comment_ids()
    }

    pub(super) fn new() -> Self {
        Self { counters: RefCell::default(), references: References::default() }
    }
}

/// `blocks` with each section's headers and footers before its content, where they change.
pub(super) fn interleave<'b>(
    ctx: &Ctx<'_, 'b>,
    scopes: &'b SideScopes,
    body: &Element,
    blocks: Vec<Block>,
    breaks: &SectionBreaks<'_>,
) -> Result<Vec<Block>, ConvertError> {
    let starts = std::iter::once(0).chain(breaks.iter().map(|&(start, _)| start));
    let properties = breaks.iter().map(|&(_, section)| Some(section));
    let sections: Vec<(usize, Option<&Element>)> =
        starts.zip(properties.chain([body.find(ns::W, "sectPr")])).collect();
    ctx.parse_ctx.charge(sections.len() as u64)?;
    let even_pages = even_pages(ctx, &sections)?;
    let mut stories = Stories { ctx, scopes, read: HashMap::new() };
    let mut named: HashMap<(Side, Variant), Named> = HashMap::new();
    let mut timeline = SideTimeline::default();
    let mut out = Vec::new();
    let mut rest = blocks.into_iter();
    let mut at = 0;
    for (start, section) in sections {
        out.extend(rest.by_ref().take(start.saturating_sub(at)));
        at = start;
        if let Some(section) = section {
            name_parts(ctx, section, &mut named)?;
        }
        let first_page = section.and_then(|section| on_off(section, "titlePg")).unwrap_or(false);
        for (side, _, _) in SIDES {
            for (variant, _) in VARIANTS {
                let shown = match variant {
                    Variant::First => first_page,
                    Variant::Even => even_pages,
                    _ => true,
                };
                let known;
                let story = match named.get(&(side, variant)) {
                    _ if !shown => {
                        known = SideStory::known(side, variant, None);
                        &known
                    }
                    None if variant != Variant::Default
                        && !named.contains_key(&(side, Variant::Default)) =>
                    {
                        known = SideStory::known(side, variant, None);
                        &known
                    }
                    None => {
                        known = SideStory::known(side, variant, Some(Vec::new()));
                        &known
                    }
                    Some(part) => stories.get(side, variant, part)?,
                };
                timeline.publish(ctx.parse_ctx, story, &mut out)?;
            }
        }
    }
    out.extend(rest);
    Ok(out)
}

/// Record the parts `section` names, replacing what earlier sections named.
fn name_parts(
    ctx: &Ctx<'_, '_>,
    section: &Element,
    named: &mut HashMap<(Side, Variant), Named>,
) -> Result<(), ConvertError> {
    for (side, reference, _) in SIDES {
        for elem in section.find_all(ns::W, reference) {
            let kind = elem.attr(ns::W, "type").unwrap_or("default");
            let Some(&(variant, _)) = VARIANTS.iter().find(|(_, name)| *name == kind) else {
                continue;
            };
            let id = elem.attr_qualified(ns::R, "id").unwrap_or_default();
            ctx.parse_ctx.charge(id.len() as u64)?;
            let part =
                rel_target_path(&ctx.rels, &ctx.base_part, id).map_or(Named::Broken, Named::Part);
            named.insert((side, variant), part);
        }
    }
    Ok(())
}

/// Whether even pages show their own headers and footers: the settings part, read only when a
/// section names one, says so.
fn even_pages(
    ctx: &Ctx<'_, '_>,
    sections: &[(usize, Option<&Element>)],
) -> Result<bool, ConvertError> {
    let names_any = sections.iter().filter_map(|(_, section)| *section).any(|section| {
        SIDES.iter().any(|(_, reference, _)| section.find(ns::W, reference).is_some())
    });
    if !names_any {
        return Ok(false);
    }
    let part: TypedPart =
        typed_part_path(ctx.parse_ctx, &ctx.rels, &ctx.base_part, SETTINGS, "settings.xml");
    let (tree, _) = part.read(ctx.parse_ctx, ctx.pkg, Role::Content(Region::Header), "settings")?;
    Ok(tree
        .as_ref()
        .and_then(|tree| tree.find(ns::W, "settings"))
        .and_then(|settings| on_off(settings, "evenAndOddHeaders"))
        .unwrap_or(false))
}

/// Each part read once per side and variant it shows as.
struct Stories<'c, 'a, 'b> {
    ctx: &'c Ctx<'a, 'b>,
    scopes: &'b SideScopes,
    read: HashMap<(Named, Side, Variant), SideStory>,
}

impl Stories<'_, '_, '_> {
    fn get(
        &mut self,
        side: Side,
        variant: Variant,
        part: &Named,
    ) -> Result<&SideStory, ConvertError> {
        if let Named::Part(path) = part {
            self.ctx.parse_ctx.charge(path.len() as u64)?;
        }
        let key = (part.clone(), side, variant);
        if !self.read.contains_key(&key) {
            let region = side.region();
            let story = SideStory::read(self.ctx.parse_ctx, side, variant, || match part {
                Named::Part(path) => self.read_part(path, side),
                Named::Broken => {
                    self.ctx.parse_ctx.emit(DiagnosticCode::PartUnreadable, region);
                    Ok(None)
                }
            })?;
            self.read.insert(key.clone(), story);
        }
        self.read.get(&key).ok_or(ConvertError::Internal)
    }

    fn read_part(&self, path: &str, side: Side) -> Result<Option<Vec<Block>>, ConvertError> {
        let (parse_ctx, pkg) = (self.ctx.parse_ctx, self.ctx.pkg);
        let region = side.region();
        let role = Role::DeferredContent(region);
        let root_name =
            SIDES.iter().find(|(s, _, _)| *s == side).map_or("hdr", |(_, _, root)| root);
        let Some(tree) = pkg.related_xml_part(path, role)? else { return Ok(None) };
        let Some(root) = tree.find(ns::W, root_name) else {
            report(parse_ctx, role.unreadable());
            return Ok(None);
        };
        let rels = read_rels(pkg, &rels_part_for(path), role)?;
        self.scopes.counters.replace(Counters::default());
        let comments = self.scopes.references.comments();
        let side_ctx = self.ctx.for_side(
            (rels, path.to_owned(), region),
            &self.scopes.counters,
            &self.scopes.references,
        );
        let blocks = content::parse_blocks(root, &side_ctx)?;
        side_ctx.settle_moves()?;
        let comments = self.scopes.references.comments().saturating_sub(comments);
        if !parse_ctx.comments_requested() {
            parse_ctx.emit_count(DiagnosticCode::CommentOmitted, Region::Comment, comments);
        }
        Ok(Some(blocks))
    }
}
