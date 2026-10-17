//! WordprocessingML numbering: `numId -> w:num` (with level overrides) `->
//! abstractNum` (with `numStyleLink` indirection) `-> level`, plus the
//! document-order counters that produce each paragraph's effective number.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::package::xml::{Element, ns};
use crate::shared::list::MarkerKind;
use crate::shared::numbering::{NumberPattern, composite_label, parse_percent_pattern};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

pub const LEVELS: usize = 9;

#[derive(Debug, Clone)]
pub struct LevelDef {
    /// `None` = suppressed numbering (`numFmt` of `none`).
    pub marker: Option<MarkerKind>,
    pub start: u64,
    /// `w:lvlRestart`: `None` = restart when any shallower level appears,
    /// `Some(0)` = never restart, `Some(n)` = restart when a level with
    /// `ilvl < n` appears.
    pub restart: Option<u32>,
    /// `w:lvlText` (with `w:isLgl`) as the shared pattern IR.
    pub pattern: NumberPattern,
}

impl Default for LevelDef {
    fn default() -> Self {
        LevelDef {
            marker: Some(MarkerKind::Bullet),
            start: 1,
            restart: None,
            pattern: NumberPattern::default(),
        }
    }
}

/// Identity of one `w:abstractNum`; the instances that resolve to it share one counter sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct AbstractId(usize);

#[derive(Debug)]
struct AbstractNum {
    id: AbstractId,
    levels: [LevelDef; LEVELS],
    /// Paragraph style each level binds to via `w:lvl > w:pStyle`.
    pstyles: [Option<String>; LEVELS],
    num_style_link: Option<String>,
}

/// One `w:num`: the levels of its abstract, shared with every other `w:num`
/// of it, and only what its own `w:lvlOverride`s replace or restart.
#[derive(Debug)]
pub struct Instance {
    base: Rc<AbstractNum>,
    overrides: Option<Box<Overrides>>,
}

#[derive(Debug, Default)]
struct Overrides {
    replaced: [Option<Box<Replacement>>; LEVELS],
    starts: [Option<u64>; LEVELS],
}

#[derive(Debug)]
struct Replacement {
    level: LevelDef,
    pstyle: Option<String>,
}

/// The entry for `ilvl`, the deepest one for a level past the last.
fn at_level<T>(levels: &[T; LEVELS], ilvl: usize) -> &T {
    levels.get(ilvl).unwrap_or(&levels[LEVELS - 1])
}

impl Instance {
    /// The level `ilvl` as this instance defines it, before its start override.
    pub fn level(&self, ilvl: usize) -> &LevelDef {
        match self.replacement(ilvl) {
            Some(replacement) => &replacement.level,
            None => at_level(&self.base.levels, ilvl),
        }
    }

    /// The number the level `ilvl` starts at: [`LevelDef::start`] alone omits the
    /// instance's `w:startOverride`.
    fn start(&self, ilvl: usize) -> u64 {
        self.start_override(ilvl).unwrap_or(self.level(ilvl).start)
    }

    fn start_override(&self, ilvl: usize) -> Option<u64> {
        *at_level(&self.overrides.as_ref()?.starts, ilvl)
    }

    fn replacement(&self, ilvl: usize) -> Option<&Replacement> {
        at_level(&self.overrides.as_ref()?.replaced, ilvl).as_deref()
    }

    fn pstyle(&self, ilvl: usize) -> Option<&str> {
        match self.replacement(ilvl) {
            Some(replacement) => replacement.pstyle.as_deref(),
            None => at_level(&self.base.pstyles, ilvl).as_deref(),
        }
    }

    /// The level bound to a paragraph style through the abstract levels'
    /// `w:pStyle` references.
    pub fn style_level(&self, style_id: &str) -> Option<usize> {
        (0..LEVELS).find(|&ilvl| self.pstyle(ilvl) == Some(style_id))
    }
}

#[derive(Debug, Default)]
pub struct Numbering {
    instances: HashMap<u64, Instance>,
}

impl Numbering {
    pub fn instance(&self, num_id: u64) -> Option<&Instance> {
        self.instances.get(&num_id)
    }
}

/// `style_num_id` maps a numbering style id to the `numId` its own `numPr`
/// references (the `numStyleLink`/`styleLink` indirection contract); its
/// error fails the parse.
pub fn parse(
    parse_ctx: &ParseCtx,
    root: &Element,
    style_num_id: &impl Fn(&str) -> Result<Option<u64>, ConvertError>,
) -> Result<Numbering, ConvertError> {
    let mut abstracts: HashMap<&str, Rc<AbstractNum>> = HashMap::new();
    for (ordinal, abs) in root.find_all(ns::W, "abstractNum").enumerate() {
        let Some(id) = abs.attr(ns::W, "abstractNumId") else {
            continue;
        };
        let mut levels: [LevelDef; LEVELS] = std::array::from_fn(|_| LevelDef::default());
        let mut pstyles: [Option<String>; LEVELS] = Default::default();
        for lvl in abs.find_all(ns::W, "lvl") {
            let ilvl: usize = lvl.attr(ns::W, "ilvl").and_then(|v| v.parse().ok()).unwrap_or(0);
            if ilvl < LEVELS {
                levels[ilvl] = parse_level(lvl);
                pstyles[ilvl] = level_pstyle(lvl);
            }
        }
        let num_style_link =
            abs.find(ns::W, "numStyleLink").and_then(|e| e.attr(ns::W, "val")).map(str::to_string);
        abstracts.insert(
            id,
            Rc::new(AbstractNum { id: AbstractId(ordinal), levels, pstyles, num_style_link }),
        );
    }

    // First pass: direct abstract references per numId.
    let mut direct: BTreeMap<u64, (&str, &Element)> = BTreeMap::new();
    for num in root.find_all(ns::W, "num") {
        let (Some(num_id), Some(abs_id)) = (
            num.attr(ns::W, "numId").and_then(|v| v.parse::<u64>().ok()),
            num.find(ns::W, "abstractNumId").and_then(|e| e.attr(ns::W, "val")),
        ) else {
            continue;
        };
        direct.insert(num_id, (abs_id, num));
    }

    let mut numbering = Numbering::default();
    let mut links = Links {
        abstracts: &abstracts,
        direct: &direct,
        style_num_id,
        resolved: HashMap::new(),
        style_numbers: HashMap::new(),
    };
    for (&num_id, &(abs_id, num_elem)) in &direct {
        let Some(abs) = links.resolve(parse_ctx, abs_id)? else {
            continue;
        };
        let mut overrides: Option<Box<Overrides>> = None;
        for over in num_elem.find_all(ns::W, "lvlOverride") {
            let ilvl: usize = over.attr(ns::W, "ilvl").and_then(|v| v.parse().ok()).unwrap_or(0);
            if ilvl >= LEVELS {
                continue;
            }
            let held = overrides.get_or_insert_default();
            let (replaced, start) = (&mut held.replaced[ilvl], &mut held.starts[ilvl]);
            if let Some(lvl) = over.find(ns::W, "lvl") {
                *replaced = Some(Box::new(Replacement {
                    level: parse_level(lvl),
                    pstyle: level_pstyle(lvl),
                }));
            }
            if let Some(value) = over
                .find(ns::W, "startOverride")
                .and_then(|e| e.attr(ns::W, "val"))
                .and_then(parse_start)
            {
                *start = Some(value);
            }
        }
        numbering.instances.insert(num_id, Instance { base: Rc::clone(abs), overrides });
    }
    Ok(numbering)
}

/// The `numStyleLink` indirection: abstract -> numbering style -> that
/// style's `numId` -> its abstract.
struct Links<'n, 'e, F> {
    abstracts: &'n HashMap<&'e str, Rc<AbstractNum>>,
    direct: &'n BTreeMap<u64, (&'e str, &'e Element)>,
    style_num_id: &'n F,
    resolved: HashMap<&'e str, Option<&'n Rc<AbstractNum>>>,
    style_numbers: HashMap<&'n str, Option<u64>>,
}

impl<'n, 'e, F: Fn(&str) -> Result<Option<u64>, ConvertError>> Links<'n, 'e, F> {
    /// The abstract definition `abs_id` resolves to, each id on the way
    /// resolved once and charged one unit when it is, and each numbering
    /// style looked up once.
    fn resolve(
        &mut self,
        parse_ctx: &ParseCtx,
        abs_id: &'e str,
    ) -> Result<Option<&'n Rc<AbstractNum>>, ConvertError> {
        let mut path: Vec<&'e str> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut current = abs_id;
        let found = loop {
            if let Some(&hit) = self.resolved.get(current) {
                break hit;
            }
            if !seen.insert(current) {
                return Err(ConvertError::malformed("numbering indirection cycle"));
            }
            parse_ctx.charge(1)?;
            path.push(current);
            let Some(abs) = self.abstracts.get(current) else {
                break None;
            };
            let linked = match abs.num_style_link.as_deref() {
                Some(style) => {
                    self.style_number(style)?.and_then(|num_id| self.direct.get(&num_id))
                }
                None => None,
            };
            match linked {
                Some(&(next, _)) => current = next,
                None => break Some(abs),
            }
        };
        self.resolved.extend(path.into_iter().map(|id| (id, found)));
        Ok(found)
    }

    fn style_number(&mut self, style: &'n str) -> Result<Option<u64>, ConvertError> {
        match self.style_numbers.entry(style) {
            Entry::Occupied(known) => Ok(*known.get()),
            Entry::Vacant(slot) => Ok(*slot.insert((self.style_num_id)(style)?)),
        }
    }
}

/// The paragraph style a `w:lvl` binds to (`w:pStyle`).
fn level_pstyle(lvl: &Element) -> Option<String> {
    lvl.find(ns::W, "pStyle").and_then(|e| e.attr(ns::W, "val")).map(str::to_string)
}

/// `ST_DecimalNumber` is `xsd:int`; values are clamped to the non-negative
/// range so untrusted starting values can never overflow counters.
fn parse_start(v: &str) -> Option<u64> {
    v.parse::<i64>().ok().map(|n| n.clamp(0, i32::MAX as i64) as u64)
}

fn parse_level(lvl: &Element) -> LevelDef {
    let fmt = lvl.find(ns::W, "numFmt").and_then(|e| e.attr(ns::W, "val")).unwrap_or("bullet");
    let marker = match fmt {
        "none" => None,
        "bullet" => Some(MarkerKind::Bullet),
        "lowerLetter" => Some(MarkerKind::LowerAlpha),
        "upperLetter" => Some(MarkerKind::UpperAlpha),
        "lowerRoman" => Some(MarkerKind::LowerRoman),
        "upperRoman" => Some(MarkerKind::UpperRoman),
        _ => Some(MarkerKind::Decimal),
    };
    let start = lvl
        .find(ns::W, "start")
        .and_then(|e| e.attr(ns::W, "val"))
        .and_then(parse_start)
        .unwrap_or(1);
    let restart = lvl
        .find(ns::W, "lvlRestart")
        .and_then(|e| e.attr(ns::W, "val"))
        .and_then(|v| v.parse().ok());
    // The number text pattern applies to ordered levels; bullet lvlText is
    // the glyph itself, not a pattern.
    let text = match marker {
        Some(m) if m.ordered() => lvl
            .find(ns::W, "lvlText")
            .and_then(|e| e.attr(ns::W, "val"))
            .map(parse_percent_pattern)
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let legal = crate::formats::docx::styles::on_off(lvl, "isLgl") == Some(true);
    LevelDef { marker, start, restart, pattern: NumberPattern { text, legal } }
}

/// Document-order numbering state: one counter array per abstract definition,
/// shared by its instances and across interruptions so continuation works,
/// with `lvlRestart` semantics and each `startOverride` applied once.
#[derive(Default)]
pub struct Counters {
    state: HashMap<AbstractId, ListState>,
    overrides_applied: HashSet<(u64, usize)>,
}

#[derive(Default)]
struct ListState {
    value: [u64; LEVELS],
    initialized: [bool; LEVELS],
    restart_pending: [bool; LEVELS],
}

impl Counters {
    /// Advance the counter for a paragraph at (`num_id`, `ilvl`) and return
    /// its effective number plus, when the level's number text is not
    /// reproducible from the marker kind alone, the composite label.
    pub fn next(&mut self, num_id: u64, ilvl: usize, instance: &Instance) -> (u64, Option<String>) {
        let ilvl = ilvl.min(LEVELS - 1);
        let state = self.state.entry(instance.base.id).or_default();
        if instance.start_override(ilvl).is_some()
            && self.overrides_applied.insert((num_id, ilvl))
            && let Some(pending) = state.restart_pending.get_mut(ilvl)
        {
            *pending = true;
        }
        let def = instance.level(ilvl);
        if !state.initialized[ilvl] || state.restart_pending[ilvl] {
            state.value[ilvl] = instance.start(ilvl);
            state.initialized[ilvl] = true;
            state.restart_pending[ilvl] = false;
        } else {
            state.value[ilvl] = state.value[ilvl].saturating_add(1);
        }
        // Using level `ilvl` schedules restarts for deeper levels according
        // to their lvlRestart settings.
        for deeper in ilvl + 1..LEVELS {
            let triggered = match instance.level(deeper).restart {
                None => true,
                Some(0) => false,
                Some(n) => (ilvl as u32) < n,
            };
            if triggered {
                state.restart_pending[deeper] = true;
            }
        }
        let value = state.value[ilvl];
        let label = def.marker.and_then(|marker| {
            composite_label(
                &def.pattern,
                marker,
                value,
                |l| instance.level(l).marker.unwrap_or(MarkerKind::Decimal),
                |l| {
                    let l = l.min(LEVELS - 1);
                    if state.initialized[l] && !state.restart_pending[l] {
                        state.value[l]
                    } else {
                        instance.start(l)
                    }
                },
            )
        });
        (value, label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::CancelToken;
    use crate::limits::{Limit, Limits};
    use crate::package::archive::Role;
    use crate::package::xml::parse_xml;
    use std::assert_matches;

    const ABSTRACT: &str = r#"<w:abstractNum w:abstractNumId="7">
        <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl>
        <w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlRestart w:val="0"/></w:lvl>
    </w:abstractNum>"#;

    fn numbers(
        nums: &str,
        style_num_id: &impl Fn(&str) -> Option<u64>,
        paragraphs: &[(u64, usize)],
    ) -> Result<Vec<u64>, ConvertError> {
        numbers_in(&ParseCtx::default(), nums, style_num_id, paragraphs)
    }

    fn numbering_in(
        parse_ctx: &ParseCtx,
        nums: &str,
        style_num_id: &impl Fn(&str) -> Option<u64>,
    ) -> Result<Numbering, ConvertError> {
        let xml = format!(
            r#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">{ABSTRACT}{nums}</w:numbering>"#
        );
        let tree = parse_xml(&ParseCtx::default(), xml.as_bytes(), Role::Probe)?;
        let root = tree
            .find(ns::W, "numbering")
            .ok_or_else(|| ConvertError::malformed("no w:numbering root"))?;
        parse(parse_ctx, root, &|style| Ok(style_num_id(style)))
    }

    fn numbers_in(
        parse_ctx: &ParseCtx,
        nums: &str,
        style_num_id: &impl Fn(&str) -> Option<u64>,
        paragraphs: &[(u64, usize)],
    ) -> Result<Vec<u64>, ConvertError> {
        let numbering = numbering_in(parse_ctx, nums, style_num_id)?;
        let mut counters = Counters::default();
        paragraphs
            .iter()
            .map(|&(num_id, ilvl)| {
                let instance = numbering
                    .instance(num_id)
                    .ok_or_else(|| ConvertError::malformed("no such numbering instance"))?;
                Ok(counters.next(num_id, ilvl, instance).0)
            })
            .collect()
    }

    fn no_styles(_: &str) -> Option<u64> {
        None
    }

    const SHARED: &str = r#"
        <w:num w:numId="1"><w:abstractNumId w:val="7"/></w:num>
        <w:num w:numId="2"><w:abstractNumId w:val="7"/></w:num>
        <w:num w:numId="3"><w:abstractNumId w:val="7"/>
            <w:lvlOverride w:ilvl="0"><w:startOverride w:val="10"/></w:lvlOverride>
        </w:num>"#;

    #[test]
    fn instances_of_one_abstract_continue_one_sequence() -> Result<(), ConvertError> {
        let got = numbers(SHARED, &no_styles, &[(1, 0), (1, 0), (2, 0), (3, 0)])?;
        assert_eq!(got, [1, 2, 3, 10]);
        Ok(())
    }

    #[test]
    fn a_start_override_restarts_only_on_its_instances_first_use() -> Result<(), ConvertError> {
        let got = numbers(SHARED, &no_styles, &[(3, 0), (1, 0), (3, 0)])?;
        assert_eq!(got, [10, 11, 12]);
        Ok(())
    }

    #[test]
    fn an_override_on_an_unused_level_restarts_nothing() -> Result<(), ConvertError> {
        let nums = r#"
            <w:num w:numId="18446744073709551615"><w:abstractNumId w:val="7"/></w:num>
            <w:num w:numId="2"><w:abstractNumId w:val="7"/>
                <w:lvlOverride w:ilvl="0"><w:startOverride w:val="10"/></w:lvlOverride>
            </w:num>
            <w:num w:numId="3"><w:abstractNumId w:val="7"/></w:num>"#;
        let got = numbers(nums, &no_styles, &[(u64::MAX, 0), (2, 1), (3, 0)])?;
        assert_eq!(got, [1, 1, 2]);
        Ok(())
    }

    #[test]
    fn a_start_override_restarts_its_level_alone() -> Result<(), ConvertError> {
        let nums = r#"
            <w:num w:numId="1"><w:abstractNumId w:val="7"/></w:num>
            <w:num w:numId="2"><w:abstractNumId w:val="7"/>
                <w:lvlOverride w:ilvl="1"><w:startOverride w:val="4"/></w:lvlOverride>
            </w:num>"#;
        let got = numbers(nums, &no_styles, &[(1, 0), (1, 1), (1, 1), (2, 1), (2, 0)])?;
        assert_eq!(got, [1, 1, 2, 4, 2]);
        Ok(())
    }

    #[test]
    fn each_overridden_level_restarts_on_its_own_first_use() -> Result<(), ConvertError> {
        let nums = r#"
            <w:num w:numId="1"><w:abstractNumId w:val="7"/></w:num>
            <w:num w:numId="2"><w:abstractNumId w:val="7"/>
                <w:lvlOverride w:ilvl="0"><w:startOverride w:val="10"/></w:lvlOverride>
                <w:lvlOverride w:ilvl="1"><w:startOverride w:val="4"/></w:lvlOverride>
            </w:num>"#;
        let got = numbers(nums, &no_styles, &[(1, 0), (1, 1), (2, 1), (2, 0)])?;
        assert_eq!(got, [1, 1, 4, 10]);
        Ok(())
    }

    #[test]
    fn each_overriding_instance_restarts_its_level() -> Result<(), ConvertError> {
        let nums = r#"
            <w:num w:numId="2"><w:abstractNumId w:val="7"/>
                <w:lvlOverride w:ilvl="0"><w:startOverride w:val="10"/></w:lvlOverride>
            </w:num>
            <w:num w:numId="3"><w:abstractNumId w:val="7"/>
                <w:lvlOverride w:ilvl="0"><w:startOverride w:val="10"/></w:lvlOverride>
            </w:num>"#;
        let got = numbers(nums, &no_styles, &[(2, 0), (2, 0), (3, 0)])?;
        assert_eq!(got, [10, 11, 10]);
        Ok(())
    }

    #[test]
    fn a_level_override_without_start_override_continues() -> Result<(), ConvertError> {
        let nums = r#"
            <w:num w:numId="1"><w:abstractNumId w:val="7"/></w:num>
            <w:num w:numId="2"><w:abstractNumId w:val="7"/>
                <w:lvlOverride w:ilvl="0">
                    <w:lvl w:ilvl="0"><w:start w:val="5"/><w:numFmt w:val="upperLetter"/></w:lvl>
                </w:lvlOverride>
            </w:num>"#;
        assert_eq!(numbers(nums, &no_styles, &[(1, 0), (1, 0), (2, 0), (1, 0)])?, [1, 2, 3, 4]);
        assert_eq!(numbers(nums, &no_styles, &[(2, 0), (1, 0), (2, 0)])?, [5, 6, 7]);
        Ok(())
    }

    #[test]
    fn instances_share_the_abstract_a_numbering_style_links_to() -> Result<(), ConvertError> {
        let nums = r#"
            <w:abstractNum w:abstractNumId="8"><w:numStyleLink w:val="Outline"/></w:abstractNum>
            <w:abstractNum w:abstractNumId="9">
                <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl>
            </w:abstractNum>
            <w:num w:numId="1"><w:abstractNumId w:val="7"/></w:num>
            <w:num w:numId="4"><w:abstractNumId w:val="9"/></w:num>
            <w:num w:numId="5"><w:abstractNumId w:val="8"/></w:num>"#;
        let outline = |style: &str| (style == "Outline").then_some(4);
        let got = numbers(nums, &outline, &[(4, 0), (5, 0), (1, 0), (4, 0)])?;
        assert_eq!(got, [1, 2, 1, 3]);
        Ok(())
    }

    #[test]
    fn a_start_override_survives_the_replacement_of_its_level() -> Result<(), ConvertError> {
        let nums = r#"
            <w:num w:numId="2"><w:abstractNumId w:val="7"/>
                <w:lvlOverride w:ilvl="0">
                    <w:lvl w:ilvl="0"><w:start w:val="5"/><w:numFmt w:val="upperLetter"/></w:lvl>
                    <w:startOverride w:val="9"/>
                </w:lvlOverride>
            </w:num>"#;
        assert_eq!(numbers(nums, &no_styles, &[(2, 0), (2, 0)])?, [9, 10]);
        Ok(())
    }

    const TWO_LEVELS: &str = r#"<w:abstractNum w:abstractNumId="20">
        <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
        <w:lvl w:ilvl="1"><w:numFmt w:val="decimal"/><w:lvlText w:val="%1.%2"/></w:lvl>
    </w:abstractNum>"#;

    #[test]
    fn a_label_reads_an_unstarted_ancestor_from_its_start_override() -> Result<(), ConvertError> {
        let nums = format!(
            r#"{TWO_LEVELS}<w:num w:numId="1"><w:abstractNumId w:val="20"/>
                <w:lvlOverride w:ilvl="0"><w:startOverride w:val="5"/></w:lvlOverride>
            </w:num>"#
        );
        let numbering = numbering_in(&ParseCtx::default(), &nums, &no_styles)?;
        let instance = numbering.instance(1).ok_or_else(|| ConvertError::malformed("no num"))?;
        let (value, label) = Counters::default().next(1, 1, instance);
        assert_eq!((value, label.as_deref()), (1, Some("5.1")));
        Ok(())
    }

    #[test]
    fn a_replaced_level_restarts_as_its_replacement_says() -> Result<(), ConvertError> {
        let nums = format!(
            r#"{TWO_LEVELS}<w:num w:numId="1"><w:abstractNumId w:val="20"/>
                <w:lvlOverride w:ilvl="1">
                    <w:lvl w:ilvl="1"><w:numFmt w:val="decimal"/><w:lvlRestart w:val="0"/></w:lvl>
                </w:lvlOverride>
            </w:num>"#
        );
        assert_eq!(numbers(&nums, &no_styles, &[(1, 0), (1, 1), (1, 0), (1, 1)])?, [1, 1, 2, 2]);
        Ok(())
    }

    #[test]
    fn instances_share_their_abstracts_levels_and_own_only_what_they_override()
    -> Result<(), ConvertError> {
        let nums = r#"
            <w:num w:numId="1"><w:abstractNumId w:val="7"/></w:num>
            <w:num w:numId="2"><w:abstractNumId w:val="7"/>
                <w:lvlOverride w:ilvl="0"><w:startOverride w:val="10"/></w:lvlOverride>
            </w:num>
            <w:num w:numId="3"><w:abstractNumId w:val="7"/>
                <w:lvlOverride w:ilvl="1">
                    <w:lvl w:ilvl="1"><w:start w:val="5"/><w:numFmt w:val="upperLetter"/><w:pStyle w:val="Own"/></w:lvl>
                </w:lvlOverride>
                <w:lvlOverride w:ilvl="12"><w:startOverride w:val="3"/></w:lvlOverride>
            </w:num>"#;
        let numbering = numbering_in(&ParseCtx::default(), nums, &no_styles)?;
        let [Some(plain), Some(restarted), Some(replaced)] =
            [1, 2, 3].map(|n| numbering.instance(n))
        else {
            panic!("an instance is missing");
        };
        assert!(Rc::ptr_eq(&plain.base, &restarted.base));
        assert!(Rc::ptr_eq(&plain.base, &replaced.base));
        assert!(plain.overrides.is_none());
        assert!(std::ptr::eq(plain.level(0), restarted.level(0)));
        assert_eq!((plain.start(0), restarted.start(0)), (1, 10));
        assert!(std::ptr::eq(plain.level(0), replaced.level(0)));
        assert!(!std::ptr::eq(plain.level(1), replaced.level(1)));
        assert_eq!(replaced.level(1).marker, Some(MarkerKind::UpperAlpha));
        assert_eq!((plain.start(1), replaced.start(1)), (1, 5));
        assert_eq!(replaced.style_level("Own"), Some(1));
        assert_eq!(plain.style_level("Own"), None);
        assert_eq!(replaced.start_override(8), None);
        Ok(())
    }

    #[test]
    fn a_numbering_style_is_looked_up_once_however_many_abstracts_link_it()
    -> Result<(), ConvertError> {
        let linking = |style: &str, first: u64| -> String {
            (first..first + 50)
                .map(|n| {
                    format!(
                        r#"<w:abstractNum w:abstractNumId="l{n}"><w:numStyleLink w:val="{style}"/></w:abstractNum><w:num w:numId="{n}"><w:abstractNumId w:val="l{n}"/></w:num>"#
                    )
                })
                .collect()
        };
        let nums = format!(
            r#"{}{}<w:num w:numId="1"><w:abstractNumId w:val="7"/></w:num>"#,
            linking("Shared", 100),
            linking("Unknown", 200)
        );
        let lookups = std::cell::RefCell::new(Vec::new());
        let link = |style: &str| {
            lookups.borrow_mut().push(style.to_string());
            (style == "Shared").then_some(1)
        };
        let got = numbers(&nums, &link, &[(100, 0), (149, 0), (1, 0), (200, 0)])?;
        assert_eq!(got, [1, 2, 3, 1]);
        let mut looked_up = lookups.into_inner();
        looked_up.sort();
        assert_eq!(looked_up, ["Shared", "Unknown"]);
        Ok(())
    }

    #[test]
    fn a_chain_of_linked_abstracts_resolves_each_link_once() -> Result<(), ConvertError> {
        let depth: u64 = 4000;
        let nums: String = (0..depth)
            .map(|i| {
                let link = format!(r#"<w:numStyleLink w:val="L{i}"/>"#);
                let link = if i + 1 < depth { link.as_str() } else { "" };
                format!(
                    r#"<w:abstractNum w:abstractNumId="a{i}">{link}</w:abstractNum><w:num w:numId="{i}"><w:abstractNumId w:val="a{i}"/></w:num>"#
                )
            })
            .collect();
        let lookups = std::cell::Cell::new(0u64);
        let link = |style: &str| {
            lookups.set(lookups.get() + 1);
            style.strip_prefix('L')?.parse::<u64>().ok().map(|i| i + 1)
        };
        let parse_ctx = ParseCtx::default();
        let got = numbers_in(&parse_ctx, &nums, &link, &[(0, 0), (depth - 1, 0)])?;
        assert_eq!(got, [1, 2]);
        assert_eq!((lookups.get(), parse_ctx.fuel()), (depth - 1, depth));
        let limits = Limits { max_work: depth - 1, ..Limits::default() };
        let refused = numbers_in(&ParseCtx::new(&limits), &nums, &link, &[]);
        assert_matches!(refused, Err(ConvertError::ResourceLimit { limit: Limit::Work, .. }));
        let token = CancelToken::new();
        token.cancel();
        let cancelled =
            numbers_in(&ParseCtx::default().with_cancel(Some(&token)), &nums, &link, &[]);
        assert_matches!(cancelled, Err(ConvertError::Cancelled));
        Ok(())
    }

    #[test]
    fn instances_resolve_in_num_id_order_whatever_the_work_left() {
        let terminals: u64 = 30;
        let cyclic = terminals + 1;
        let nums: String = (1..=terminals)
            .map(|i| {
                format!(
                    r#"<w:abstractNum w:abstractNumId="t{i}"/><w:num w:numId="{i}"><w:abstractNumId w:val="t{i}"/></w:num>"#
                )
            })
            .chain([format!(
                r#"<w:abstractNum w:abstractNumId="c"><w:numStyleLink w:val="C"/></w:abstractNum><w:num w:numId="{cyclic}"><w:abstractNumId w:val="c"/></w:num>"#
            )])
            .collect();
        let link = |style: &str| (style == "C").then_some(cyclic);
        let verdict = |max_work| {
            let limits = Limits { max_work, ..Limits::default() };
            let parse_ctx = ParseCtx::new(&limits);
            let refused = numbers_in(&parse_ctx, &nums, &link, &[]).unwrap_err();
            (refused, parse_ctx.fuel())
        };
        let (short, fuel) = verdict(terminals);
        assert_matches!(short, ConvertError::ResourceLimit { limit: Limit::Work, .. });
        assert_eq!(fuel, cyclic);
        let (enough, fuel) = verdict(cyclic);
        assert_matches!(enough, ConvertError::Malformed { .. });
        assert_eq!(fuel, cyclic);
    }

    #[test]
    fn a_cycle_of_numbering_links_is_malformed() {
        let nums = r#"
            <w:abstractNum w:abstractNumId="a"><w:numStyleLink w:val="B"/></w:abstractNum>
            <w:abstractNum w:abstractNumId="b"><w:numStyleLink w:val="A"/></w:abstractNum>
            <w:num w:numId="1"><w:abstractNumId w:val="a"/></w:num>
            <w:num w:numId="2"><w:abstractNumId w:val="b"/></w:num>"#;
        let link = |style: &str| match style {
            "A" => Some(1),
            "B" => Some(2),
            _ => None,
        };
        assert_matches!(numbers(nums, &link, &[]), Err(ConvertError::Malformed { .. }));
    }
}
