use crate::model::{AssetId, Region, Unit};

/// Normalized, document-scoped anchor id. Frontends that concatenate multiple
/// source parts (EPUB chapters) scope ids during normalization so they stay
/// unique across the whole document.
pub type AnchorId = String;

/// Where a link points.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize),
    serde(tag = "kind", content = "value", rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum LinkTarget {
    /// A source unit identified independently of rendered anchor names.
    Unit(Unit),
    /// Absolute URL with a scheme.
    External(String),
    /// Scheme-less relative reference, preserved as written.
    Relative(String),
    /// Internal target: a heading anchor or an `Inline::Anchor` node.
    Anchor {
        /// Normalized, document-scoped destination id.
        id: AnchorId,
        /// Source unit owning the destination, when known.
        unit: Option<Unit>,
    },
}

impl LinkTarget {
    /// Create an internal target without a known destination unit.
    ///
    /// ```
    /// use almiladoc::model::LinkTarget;
    /// assert!(!LinkTarget::anchor("section".into()).is_empty());
    /// ```
    #[must_use]
    pub fn anchor(id: AnchorId) -> Self {
        Self::Anchor { id, unit: None }
    }

    /// True for an empty textual destination; unit targets are never empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Unit(_) => false,
            Self::External(s) | Self::Relative(s) | Self::Anchor { id: s, .. } => s.is_empty(),
        }
    }
}

/// Where an image's bytes live.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize),
    serde(tag = "kind", content = "value", rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum ImageSource {
    /// Absolute URL with a scheme.
    External(String),
    /// Embedded image stored in `Document::assets`.
    Asset(AssetId),
    /// An intentional stand-in with no image source to read.
    Unavailable,
    /// An actual image whose source could not be read.
    Unread,
}

/// The source location of an image, independent of its rendered container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct ImageOrigin {
    /// The source region containing the image.
    pub region: Region,
    /// Its source unit, when the format has units.
    pub unit: Option<Unit>,
}

impl ImageOrigin {
    /// An image location in the given source region and unit.
    ///
    /// ```
    /// use almiladoc::{model::ImageOrigin, Region};
    /// let origin = ImageOrigin::new(Region::Note, None);
    /// assert_eq!(origin.region, Region::Note);
    /// ```
    #[must_use]
    pub const fn new(region: Region, unit: Option<Unit>) -> Self {
        Self { region, unit }
    }
}
