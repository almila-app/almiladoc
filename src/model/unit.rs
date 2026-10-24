use std::num::NonZeroU32;

/// The source structure a document unit represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum UnitKind {
    /// A presentation slide.
    Slide,
    /// A spreadsheet sheet.
    Sheet,
    /// A publication's spine item.
    SpineItem,
    /// A PDF page.
    Page,
}

impl UnitKind {
    /// The stable snake-case name of this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slide => "slide",
            Self::Sheet => "sheet",
            Self::SpineItem => "spine_item",
            Self::Page => "page",
        }
    }
}

/// A unit identified by its kind and original one-based position in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct Unit {
    /// The source structure this unit represents.
    pub kind: UnitKind,
    /// Its one-based listed position, including preceding skipped units.
    pub ordinal: NonZeroU32,
}

impl Unit {
    /// A unit at the given original one-based position.
    ///
    /// ```
    /// use almiladoc::model::{Unit, UnitKind};
    /// use std::num::NonZeroU32;
    /// let unit = Unit::new(UnitKind::Slide, NonZeroU32::MIN);
    /// assert_eq!(unit.ordinal.get(), 1);
    /// ```
    #[must_use]
    pub const fn new(kind: UnitKind, ordinal: NonZeroU32) -> Self {
        Self { kind, ordinal }
    }
}

/// A content location in the source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Region {
    /// The main text.
    Body,
    /// Page or slide headers.
    Header,
    /// Page or slide footers.
    Footer,
    /// Footnotes, endnotes and speaker notes.
    Note,
    /// Comments and annotations.
    Comment,
    /// Document properties.
    Metadata,
    /// Parts that hold no rendered text, such as stylesheets.
    Container,
}

impl Region {
    /// The region's stable `snake_case` name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::Header => "header",
            Self::Footer => "footer",
            Self::Note => "note",
            Self::Comment => "comment",
            Self::Metadata => "metadata",
            Self::Container => "container",
        }
    }
}
