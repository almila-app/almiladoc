use crate::model::Block;

/// The marker family a list level uses in the source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum MarkerKind {
    /// Unordered.
    Bullet,
    /// `1`, `2`, `3`.
    Decimal,
    /// `a`, `b`, `c`.
    LowerAlpha,
    /// `A`, `B`, `C`.
    UpperAlpha,
    /// `i`, `ii`, `iii`.
    LowerRoman,
    /// `I`, `II`, `III`.
    UpperRoman,
}

impl MarkerKind {
    /// True for every kind but [`MarkerKind::Bullet`].
    #[must_use]
    pub fn ordered(self) -> bool {
        !matches!(self, Self::Bullet)
    }

    /// The marker text for ordinal `n` (1-based), without trailing space:
    /// `3.`, `c.`, `iv.`; bullets have no ordinal text.
    #[must_use]
    pub fn label(self, n: u64) -> String {
        let mut label = self.ordinal(n);
        if self.ordered() {
            label.push('.');
        }
        label
    }

    /// The bare ordinal text for `n` without punctuation: `3`, `c`, `iv`.
    #[must_use]
    pub fn ordinal(self, n: u64) -> String {
        match self {
            Self::Bullet => "-".to_string(),
            Self::Decimal => n.to_string(),
            Self::LowerAlpha => alpha(n),
            Self::UpperAlpha => alpha(n).to_ascii_uppercase(),
            Self::LowerRoman => roman(n),
            Self::UpperRoman => roman(n).to_ascii_uppercase(),
        }
    }
}

/// 1 -> `a`, 26 -> `z`, 27 -> `aa` (bijective base 26).
fn alpha(mut n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut out = String::new();
    while n > 0 {
        n -= 1;
        out.push(char::from(b'a' + (n % 26) as u8));
        n /= 26;
    }
    out.chars().rev().collect()
}

fn roman(mut n: u64) -> String {
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    const NUMERALS: [(u64, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut out = String::new();
    for (value, numeral) in NUMERALS {
        while n >= value {
            out.push_str(numeral);
            n -= value;
        }
    }
    out
}

/// A fully resolved list: numbering identity and marker resolution happen in
/// the frontends (`shared::list`), which split runs whenever the list
/// instance or marker kind changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct List {
    /// The marker family every item in this run uses.
    pub marker: MarkerKind,
    /// Ordinal of the first item, from the source's own numbering.
    pub start: u64,
    /// The items, in order.
    pub items: Vec<ListItem>,
}

impl List {
    /// True when this list's marker is a numbered one.
    #[must_use]
    pub fn ordered(&self) -> bool {
        self.marker.ordered()
    }
}

/// One item of a [`List`], which may hold nested blocks including further
/// lists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct ListItem {
    /// The item's content.
    pub blocks: Vec<Block>,
    /// Literal marker text that overrides the level marker when the source
    /// number text cannot be reproduced from `marker` + position alone
    /// (composite number text such as `1-a)`).
    pub marker_label: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_cover_the_marker_kinds() {
        assert_eq!(MarkerKind::Bullet.label(7), "-");
        assert_eq!(MarkerKind::Decimal.label(7), "7.");
        assert_eq!(MarkerKind::LowerAlpha.label(3), "c.");
        assert_eq!(MarkerKind::LowerAlpha.label(27), "aa.");
        assert_eq!(MarkerKind::LowerAlpha.label(28), "ab.");
        assert_eq!(MarkerKind::LowerAlpha.label(53), "ba.");
        assert_eq!(MarkerKind::LowerAlpha.label(703), "aaa.");
        assert_eq!(MarkerKind::UpperAlpha.label(2), "B.");
        assert_eq!(MarkerKind::LowerRoman.label(4), "iv.");
        assert_eq!(MarkerKind::LowerRoman.label(1994), "mcmxciv.");
        assert_eq!(MarkerKind::UpperRoman.label(9), "IX.");
    }
}
