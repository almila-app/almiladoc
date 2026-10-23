/// Fully resolved character style. Tri-state deltas exist only during
/// frontend resolution (`shared::delta`); by the time content reaches the
/// model every toggle has a definite value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct Style {
    /// Bold weight.
    pub bold: bool,
    /// Italic or oblique.
    pub italic: bool,
    /// Struck through.
    pub strike: bool,
    /// Monospace, from a code or teletype character style.
    pub code: bool,
    /// Raised or lowered off the baseline of the text around it.
    pub position: VerticalPosition,
}

impl Style {
    /// No toggle set, on the baseline.
    pub const PLAIN: Self = Self {
        bold: false,
        italic: false,
        strike: false,
        code: false,
        position: VerticalPosition::Baseline,
    };
}

/// Where a run sits relative to the baseline of the text around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum VerticalPosition {
    /// On the baseline.
    #[default]
    Baseline,
    /// Raised, as an exponent or a note mark.
    Superscript,
    /// Lowered, as a chemical or index subscript.
    Subscript,
}
