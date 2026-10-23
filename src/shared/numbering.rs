//! Shared list-number pattern IR.
//!
//! A level's number text is a sequence of literal pieces and level
//! references, rendered against the live counter values. Every frontend
//! resolves its native syntax onto this one form: WordprocessingML
//! `w:lvlText` (`%1.%2)`), the DOC `xst` placeholder string, RTF
//! `\leveltext`/`\levelnumbers`, and ODF prefix/suffix/display-levels.
//! Rendering returns `None` when the pattern reproduces the default label
//! the renderer builds from marker + value alone, so native Markdown list
//! numbering is used whenever it is faithful.

use crate::shared::list::MarkerKind;

/// One piece of a level's number text: literal characters, or the current
/// number of a (zero-based) level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NumberText {
    Literal(String),
    Level(u8),
}

/// A level's resolved number pattern.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NumberPattern {
    /// Empty = no pattern known; the marker kind's default label applies.
    pub text: Vec<NumberText>,
    /// Legal numbering (`w:isLgl`, LVLF `fLegal`): referenced levels render
    /// as decimal regardless of their own format.
    pub legal: bool,
}

/// Parse WordprocessingML-style percent patterns (`%1`–`%9` level
/// references, everything else literal) into pattern tokens.
pub(crate) fn parse_percent_pattern(text: &str) -> Vec<NumberText> {
    let mut out: Vec<NumberText> = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%'
            && let Some(level) = chars
                .peek()
                .and_then(|c| c.to_digit(10))
                .and_then(|d| u8::try_from(d.checked_sub(1)?).ok())
        {
            chars.next();
            out.push(NumberText::Level(level));
            continue;
        }
        match out.last_mut() {
            Some(NumberText::Literal(s)) => s.push(c),
            _ => out.push(NumberText::Literal(c.to_string())),
        }
    }
    out
}

/// Render a pattern against the current sequence values. `level_marker` and
/// `level_value` supply each referenced level's marker kind and current
/// ordinal (the level's start value when it has not begun counting).
/// `None` when the result matches the default label produced from the own
/// level's marker and value.
pub(crate) fn composite_label(
    pattern: &NumberPattern,
    own_marker: MarkerKind,
    own_value: u64,
    level_marker: impl Fn(usize) -> MarkerKind,
    level_value: impl Fn(usize) -> u64,
) -> Option<String> {
    if pattern.text.is_empty() {
        return None;
    }
    let mut out = String::new();
    for piece in &pattern.text {
        match piece {
            NumberText::Literal(s) => out.push_str(s),
            NumberText::Level(l) => {
                let l = *l as usize;
                let kind = if pattern.legal { MarkerKind::Decimal } else { level_marker(l) };
                out.push_str(&kind.ordinal(level_value(l)));
            }
        }
    }
    if out == own_marker.label(own_value) { None } else { Some(out) }
}

/// MS-OSHARED numbering formats -> marker kinds. `None` = no number.
pub(crate) fn marker_for_nfc(nfc: i32) -> Option<MarkerKind> {
    match nfc {
        1 => Some(MarkerKind::UpperRoman),
        2 => Some(MarkerKind::LowerRoman),
        3 => Some(MarkerKind::UpperAlpha),
        4 => Some(MarkerKind::LowerAlpha),
        23 => Some(MarkerKind::Bullet),
        255 => None,
        _ => Some(MarkerKind::Decimal),
    }
}

/// The list levels Word, PowerPoint and RTF define.
pub(crate) const OFFICE_LEVELS: usize = 9;

/// One list's counters, a value per level: a level past the last counts as the last, and a
/// level that has not started takes its start value on first use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LevelCounters<const N: usize> {
    values: [u64; N],
    started: [bool; N],
}

impl<const N: usize> Default for LevelCounters<N> {
    fn default() -> Self {
        const { assert!(N > 0, "a list has at least one level") };
        Self { values: [0; N], started: [false; N] }
    }
}

impl<const N: usize> LevelCounters<N> {
    /// `level` within this list's levels.
    pub(crate) const fn clamp(level: usize) -> usize {
        if level < N { level } else { N - 1 }
    }

    /// Numbers the next item at `level`: `start` if the level has not started, else one more
    /// than its last; then restarts each deeper level `restarts` names.
    pub(crate) fn next(
        &mut self,
        level: usize,
        start: u64,
        restarts: impl Fn(usize) -> bool,
    ) -> u64 {
        let level = Self::clamp(level);
        let value = if self.started[level] { self.values[level].saturating_add(1) } else { start };
        self.seed(level, value, restarts);
        value
    }

    /// Numbers the next item at `level` with `value`, as a source that states its number does;
    /// then restarts each deeper level `restarts` names.
    pub(crate) fn seed(&mut self, level: usize, value: u64, restarts: impl Fn(usize) -> bool) {
        let level = Self::clamp(level);
        self.values[level] = value;
        self.started[level] = true;
        for deeper in level + 1..N {
            if restarts(deeper) {
                self.started[deeper] = false;
            }
        }
    }

    /// Makes the next item at `level` take its start value.
    pub(crate) fn restart(&mut self, level: usize) {
        self.started[Self::clamp(level)] = false;
    }

    /// The number `level` shows now: its last value, or `start` if it has not started.
    pub(crate) fn value_or(&self, level: usize, start: u64) -> u64 {
        let level = Self::clamp(level);
        if self.started[level] { self.values[level] } else { start }
    }
}

/// Restarts every deeper level, as a list without per-level restart rules does.
pub(crate) fn every_deeper(_: usize) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_starts_at_its_start_then_counts() {
        let mut c = LevelCounters::<3>::default();
        assert_eq!(c.value_or(0, 4), 4);
        assert_eq!(c.next(0, 4, every_deeper), 4);
        assert_eq!(c.next(0, 1, every_deeper), 5);
        assert_eq!(c.value_or(0, 1), 5);
        c.seed(0, u64::MAX, every_deeper);
        assert_eq!(c.next(0, 1, every_deeper), u64::MAX);
    }

    #[test]
    fn a_level_past_the_last_counts_as_the_last() {
        let mut c = LevelCounters::<9>::default();
        assert_eq!(LevelCounters::<9>::clamp(8), 8);
        assert_eq!(LevelCounters::<9>::clamp(9), 8);
        assert_eq!(LevelCounters::<10>::clamp(usize::MAX), 9);
        assert_eq!(c.next(usize::MAX, 1, every_deeper), 1);
        assert_eq!(c.next(8, 1, every_deeper), 2);
        c.restart(100);
        assert_eq!(c.value_or(8, 7), 7);
    }

    #[test]
    fn using_a_level_restarts_only_the_deeper_levels_its_rule_names() {
        let mut c = LevelCounters::<4>::default();
        for level in 0..4 {
            c.next(level, 1, every_deeper);
            c.next(level, 1, every_deeper);
        }
        let limits = [None, Some(0), Some(2), Some(1)];
        let rule = |used: usize| {
            move |deeper: usize| match limits[deeper] {
                None => true,
                Some(limit) => used < limit,
            }
        };
        c.next(1, 1, rule(1));
        assert_eq!(
            [c.value_or(0, 0), c.value_or(1, 0), c.value_or(2, 0), c.value_or(3, 0)],
            [2, 3, 0, 2]
        );
        c.next(0, 1, rule(0));
        assert_eq!([c.value_or(1, 0), c.value_or(2, 0), c.value_or(3, 0)], [3, 0, 0]);
    }

    #[test]
    fn a_seed_states_its_value_and_restarts_by_the_rule() {
        let mut c = LevelCounters::<3>::default();
        c.next(1, 1, every_deeper);
        c.next(2, 1, every_deeper);
        c.seed(1, 7, |_| false);
        assert_eq!([c.value_or(1, 0), c.value_or(2, 0)], [7, 1]);
        c.seed(1, 3, every_deeper);
        assert_eq!([c.value_or(1, 0), c.value_or(2, 0)], [3, 0]);
        assert_eq!(c.next(1, 1, every_deeper), 4);
    }

    #[test]
    fn a_restart_takes_the_start_value_next() {
        let mut c = LevelCounters::<2>::default();
        c.next(0, 1, every_deeper);
        c.next(1, 1, every_deeper);
        c.restart(0);
        assert_eq!(c.value_or(0, 9), 9);
        assert_eq!(c.value_or(1, 9), 1);
        assert_eq!(c.next(0, 5, |_| false), 5);
        assert_eq!(c.next(1, 5, every_deeper), 2);
    }

    #[test]
    fn percent_patterns_tokenize() {
        assert_eq!(
            parse_percent_pattern("%1.%2)"),
            vec![
                NumberText::Level(0),
                NumberText::Literal(".".into()),
                NumberText::Level(1),
                NumberText::Literal(")".into()),
            ]
        );
        assert_eq!(
            parse_percent_pattern("Ch. %3:"),
            vec![
                NumberText::Literal("Ch. ".into()),
                NumberText::Level(2),
                NumberText::Literal(":".into()),
            ]
        );
        assert_eq!(parse_percent_pattern("%9"), vec![NumberText::Level(8)]);
        // `%0` and a trailing `%` are literal.
        assert_eq!(parse_percent_pattern("%0%"), vec![NumberText::Literal("%0%".into())]);
    }

    #[test]
    fn default_pattern_yields_no_label() {
        let pattern = NumberPattern { text: parse_percent_pattern("%1."), legal: false };
        let label =
            composite_label(&pattern, MarkerKind::Decimal, 3, |_| MarkerKind::Decimal, |_| 3);
        assert_eq!(label, None);
    }

    #[test]
    fn composite_pattern_renders_parent_values() {
        let pattern = NumberPattern { text: parse_percent_pattern("%1-%2)"), legal: false };
        let values = [2u64, 5];
        let label = composite_label(
            &pattern,
            MarkerKind::LowerAlpha,
            5,
            |l| if l == 0 { MarkerKind::Decimal } else { MarkerKind::LowerAlpha },
            |l| values[l.min(1)],
        );
        assert_eq!(label.as_deref(), Some("2-e)"));
    }

    #[test]
    fn legal_numbering_forces_decimal_references() {
        let pattern = NumberPattern { text: parse_percent_pattern("%1.%2"), legal: true };
        let label = composite_label(
            &pattern,
            MarkerKind::LowerRoman,
            4,
            |_| MarkerKind::LowerRoman,
            |l| if l == 0 { 2 } else { 4 },
        );
        assert_eq!(label.as_deref(), Some("2.4"));
    }
}
