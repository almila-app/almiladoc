use std::num::NonZeroU32;

use crate::model::{AssetId, Unit};

/// A place in the document whose content awaits OCR.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct Gap {
    /// Names this gap among the document's gaps.
    pub id: GapId,
    /// What the OCR reads to fill it.
    pub origin: OcrOrigin,
    /// Why the source's own text does not serve, in a fixed order.
    pub reasons: Vec<GapReason>,
}

/// An opaque name for one [`Gap`]: the unit it stands in, if the document has units, and its
/// place among that unit's gaps, or the document's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct GapId {
    unit: Option<Unit>,
    ordinal: NonZeroU32,
}

impl GapId {
    pub(crate) const fn new(unit: Option<Unit>, ordinal: NonZeroU32) -> Self {
        Self { unit, ordinal }
    }

    /// The unit the gap stands in, `None` in a document without units.
    #[must_use]
    pub const fn unit(self) -> Option<Unit> {
        self.unit
    }

    /// Its one-based place among the unit's gaps, or the document's.
    #[must_use]
    pub const fn ordinal(self) -> NonZeroU32 {
        self.ordinal
    }
}

/// What OCR reads to fill a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize),
    serde(tag = "kind", content = "value", rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum OcrOrigin {
    /// The whole of this one-based PDF page.
    Page(NonZeroU32),
    /// This embedded PNG or JPEG image, whose text follows the block that shows it.
    Image(AssetId),
}

/// Why a page's own text does not stand for what it shows, or that the caller asked for its
/// OCR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum GapReason {
    /// A content stream of the page does not decode, or the page does not parse.
    UnreadableContent,
    /// Its text is drawn invisible or clipped away: an OCR layer, or no text at all.
    InvisibleText,
    /// Every text run it draws is in a font whose codes give no characters.
    GarbledText,
    /// Its text is an image: no text over an image, or only furniture or margin text over a
    /// page-sized one.
    ImageBacked,
    /// Its glyphs are drawn as filled paths, with no text and no image.
    VectorDrawn,
    /// It shows text that yields no characters for another reason.
    UnreadableText,
    /// The caller selected it for OCR, whatever its text.
    CallerRequested,
}

impl GapReason {
    /// The reason's stable `snake_case` name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnreadableContent => "unreadable_content",
            Self::InvisibleText => "invisible_text",
            Self::GarbledText => "garbled_text",
            Self::ImageBacked => "image_backed",
            Self::VectorDrawn => "vector_drawn",
            Self::UnreadableText => "unreadable_text",
            Self::CallerRequested => "caller_requested",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UnitKind;

    #[test]
    fn a_gap_id_names_its_unit_and_its_place_in_it() {
        let unit = Unit::new(UnitKind::Page, NonZeroU32::new(3).unwrap());
        let id = GapId::new(Some(unit), NonZeroU32::new(2).unwrap());
        assert_eq!((id.unit(), id.ordinal().get()), (Some(unit), 2));
    }
}
