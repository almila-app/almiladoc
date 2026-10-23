//! Tri-state style deltas used during cascade resolution. A property is
//! either explicitly on, explicitly off, or unset (inherit); only after the
//! full cascade is a delta collapsed into the model's resolved [`Style`].

use crate::model::{Inline, Style, VerticalPosition};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StyleDelta {
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub strike: Option<bool>,
    pub code: Option<bool>,
    pub position: Option<VerticalPosition>,
    /// Merged like the others but never applied: the model `Style` has no hidden flag, so a
    /// reader drops hidden text itself.
    pub hidden: Option<bool>,
}

impl StyleDelta {
    /// Overlay `child` on `self`: an explicit child value (on **or** off)
    /// wins; unset inherits.
    pub(crate) fn merge(self, child: Self) -> Self {
        Self {
            bold: child.bold.or(self.bold),
            italic: child.italic.or(self.italic),
            strike: child.strike.or(self.strike),
            code: child.code.or(self.code),
            position: child.position.or(self.position),
            hidden: child.hidden.or(self.hidden),
        }
    }

    pub(crate) fn apply(self, base: Style) -> Style {
        Style {
            bold: self.bold.unwrap_or(base.bold),
            italic: self.italic.unwrap_or(base.italic),
            strike: self.strike.unwrap_or(base.strike),
            code: self.code.unwrap_or(base.code),
            position: self.position.unwrap_or(base.position),
        }
    }

    pub(crate) fn resolve(self) -> Style {
        self.apply(Style::PLAIN)
    }
}

/// Drop from every run the emphasis `base` already carries. A heading style
/// defines its own typography, so its runs should carry only what they add
/// beyond it - otherwise the bold in `## **Heading**` is the style's, not the
/// author's.
pub(crate) fn rebase_emphasis(inlines: &mut [Inline], base: Style) {
    if base == Style::PLAIN {
        return;
    }
    for inline in inlines {
        match inline {
            Inline::Text { style, .. } => {
                style.bold &= !base.bold;
                style.italic &= !base.italic;
                style.strike &= !base.strike;
            }
            Inline::Link { content, .. } => rebase_emphasis(content, base),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_off_beats_inherited_on() {
        let base = StyleDelta { bold: Some(true), ..Default::default() };
        let child = StyleDelta { bold: Some(false), ..Default::default() };
        assert_eq!(base.merge(child).resolve(), Style::PLAIN);
    }

    #[test]
    fn unset_inherits() {
        let base = StyleDelta { bold: Some(true), italic: Some(true), ..Default::default() };
        let child = StyleDelta { italic: Some(false), ..Default::default() };
        let resolved = base.merge(child).resolve();
        assert!(resolved.bold && !resolved.italic);
    }

    #[test]
    fn hidden_merges_but_never_reaches_the_style() {
        let hide = StyleDelta { hidden: Some(true), ..Default::default() };
        let show = StyleDelta { hidden: Some(false), ..Default::default() };
        assert_eq!(hide.merge(StyleDelta::default()).hidden, Some(true));
        assert_eq!(hide.merge(show).hidden, Some(false));
        assert_eq!(show.merge(hide).hidden, Some(true));
        assert_eq!(hide.resolve(), Style::PLAIN);
    }

    #[test]
    fn a_position_merges_and_applies_like_a_toggle_and_rebasing_keeps_it() {
        let raise =
            StyleDelta { position: Some(VerticalPosition::Superscript), ..Default::default() };
        let lower =
            StyleDelta { position: Some(VerticalPosition::Subscript), ..Default::default() };
        let level = StyleDelta { position: Some(VerticalPosition::Baseline), ..Default::default() };
        assert_eq!(raise.merge(lower).position, Some(VerticalPosition::Subscript));
        assert_eq!(
            raise.merge(StyleDelta::default()).position,
            Some(VerticalPosition::Superscript)
        );
        assert_eq!(raise.merge(level).resolve(), Style::PLAIN);
        let raised = Style { position: VerticalPosition::Superscript, ..Style::PLAIN };
        assert_eq!(StyleDelta::default().apply(raised), raised);
        assert_eq!(lower.apply(raised).position, VerticalPosition::Subscript);
        let bold = Style { bold: true, ..raised };
        let mut inlines = vec![Inline::Text { text: "x".into(), style: bold }];
        rebase_emphasis(&mut inlines, bold);
        assert_eq!(inlines, [Inline::Text { text: "x".into(), style: raised }]);
    }
}
