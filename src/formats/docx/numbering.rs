//! WordprocessingML numbering: `numId -> w:num` (with level overrides) `->
//! abstractNum` (with `numStyleLink` indirection) `-> level`, plus the
//! document-order counters that produce each paragraph's effective number.

use crate::error::ConvertError;
use crate::package::xml::{Element, ns};
use crate::shared::list::MarkerKind;
use crate::shared::numbering::{NumberPattern, composite_label, parse_percent_pattern};
use std::collections::{HashMap, HashSet};

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

#[derive(Debug, Clone)]
struct AbstractNum {
    id: AbstractId,
    levels: [LevelDef; LEVELS],
    /// Paragraph style each level binds to via `w:lvl > w:pStyle`.
    pstyles: [Option<String>; LEVELS],
    num_style_link: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Instance {
    pub levels: [LevelDef; LEVELS],
    pstyles: [Option<String>; LEVELS],
    abstract_id: AbstractId,
    start_overrides: [bool; LEVELS],
}

impl Instance {
    /// The level bound to a paragraph style through the abstract levels'
    /// `w:pStyle` references.
    pub fn style_level(&self, style_id: &str) -> Option<usize> {
        self.pstyles.iter().position(|p| p.as_deref() == Some(style_id))
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
/// references (the `numStyleLink`/`styleLink` indirection contract).
pub fn parse(
    root: &Element,
    style_num_id: &impl Fn(&str) -> Option<u64>,
) -> Result<Numbering, ConvertError> {
    let mut abstracts: HashMap<&str, AbstractNum> = HashMap::new();
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
        abstracts
            .insert(id, AbstractNum { id: AbstractId(ordinal), levels, pstyles, num_style_link });
    }

    // First pass: direct abstract references per numId.
    let mut direct: HashMap<u64, (&str, &Element)> = HashMap::new();
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
    for (&num_id, &(abs_id, num_elem)) in &direct {
        let Some(abs) = resolve_abstract(abs_id, &abstracts, &direct, style_num_id)? else {
            log::warn!("numbering instance {num_id} references unknown abstract {abs_id:?}");
            continue;
        };
        let mut levels = abs.levels.clone();
        let mut pstyles = abs.pstyles.clone();
        let mut start_overrides = [false; LEVELS];
        for over in num_elem.find_all(ns::W, "lvlOverride") {
            let ilvl: usize = over.attr(ns::W, "ilvl").and_then(|v| v.parse().ok()).unwrap_or(0);
            let (Some(level), Some(pstyle), Some(start_override)) =
                (levels.get_mut(ilvl), pstyles.get_mut(ilvl), start_overrides.get_mut(ilvl))
            else {
                continue;
            };
            // A nested w:lvl replaces the level wholesale; startOverride is
            // applied last so it survives the replacement.
            if let Some(lvl) = over.find(ns::W, "lvl") {
                *level = parse_level(lvl);
                *pstyle = level_pstyle(lvl);
            }
            if let Some(start) = over
                .find(ns::W, "startOverride")
                .and_then(|e| e.attr(ns::W, "val"))
                .and_then(parse_start)
            {
                level.start = start;
                *start_override = true;
            }
        }
        numbering
            .instances
            .insert(num_id, Instance { levels, pstyles, abstract_id: abs.id, start_overrides });
    }
    Ok(numbering)
}

/// Resolve an abstract definition, following `numStyleLink` indirection:
/// abstract -> numbering style -> that style's `numId` -> its abstract.
fn resolve_abstract<'n>(
    abs_id: &str,
    abstracts: &'n HashMap<&str, AbstractNum>,
    direct: &HashMap<u64, (&str, &Element)>,
    style_num_id: &impl Fn(&str) -> Option<u64>,
) -> Result<Option<&'n AbstractNum>, ConvertError> {
    let mut seen: Vec<String> = Vec::new();
    let mut current = abs_id.to_string();
    loop {
        if seen.contains(&current) {
            return Err(ConvertError::malformed("numbering indirection cycle"));
        }
        seen.push(current.clone());
        let Some(abs) = abstracts.get(current.as_str()) else {
            return Ok(None);
        };
        let Some(style_id) = &abs.num_style_link else {
            return Ok(Some(abs));
        };
        let linked = style_num_id(style_id)
            .and_then(|num_id| direct.get(&num_id))
            .map(|(abs_id, _)| abs_id.to_string());
        match linked {
            Some(next) => current = next,
            None => return Ok(Some(abs)),
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
        let state = self.state.entry(instance.abstract_id).or_default();
        if instance.start_overrides.get(ilvl) == Some(&true)
            && self.overrides_applied.insert((num_id, ilvl))
            && let Some(pending) = state.restart_pending.get_mut(ilvl)
        {
            *pending = true;
        }
        let def = &instance.levels[ilvl];
        if !state.initialized[ilvl] || state.restart_pending[ilvl] {
            state.value[ilvl] = def.start;
            state.initialized[ilvl] = true;
            state.restart_pending[ilvl] = false;
        } else {
            state.value[ilvl] = state.value[ilvl].saturating_add(1);
        }
        // Using level `ilvl` schedules restarts for deeper levels according
        // to their lvlRestart settings.
        for (deeper, level_def) in instance.levels.iter().enumerate().skip(ilvl + 1) {
            let triggered = match level_def.restart {
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
                |l| instance.levels[l.min(LEVELS - 1)].marker.unwrap_or(MarkerKind::Decimal),
                |l| {
                    let l = l.min(LEVELS - 1);
                    if state.initialized[l] && !state.restart_pending[l] {
                        state.value[l]
                    } else {
                        instance.levels[l].start
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
    use crate::package::xml::parse_xml;

    const ABSTRACT: &str = r#"<w:abstractNum w:abstractNumId="7">
        <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl>
        <w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlRestart w:val="0"/></w:lvl>
    </w:abstractNum>"#;

    fn numbers(
        nums: &str,
        style_num_id: &impl Fn(&str) -> Option<u64>,
        paragraphs: &[(u64, usize)],
    ) -> Result<Vec<u64>, ConvertError> {
        let xml = format!(
            r#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">{ABSTRACT}{nums}</w:numbering>"#
        );
        let tree = parse_xml(xml.as_bytes())?;
        let root = tree
            .find(ns::W, "numbering")
            .ok_or_else(|| ConvertError::malformed("no w:numbering root"))?;
        let numbering = parse(root, style_num_id)?;
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
}
