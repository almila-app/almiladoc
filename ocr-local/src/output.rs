use std::fmt;

use oar_ocr_core::domain::TextRegion;

use crate::{Error, Limits};

/// A finite, nondegenerate axis-aligned rectangle in input image pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rectangle {
    /// The rectangle's left edge.
    pub left: f32,
    /// The rectangle's top edge.
    pub top: f32,
    /// The rectangle's right edge.
    pub right: f32,
    /// The rectangle's bottom edge.
    pub bottom: f32,
}

/// One recognized text region; text is literal and has no Markdown interpretation.
#[derive(Clone, PartialEq)]
pub struct Line {
    text: String,
    rectangle: Rectangle,
}

impl Line {
    /// The exact recognized UTF-8 text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The region's bounds in the input image coordinate system.
    #[must_use]
    pub fn rectangle(&self) -> Rectangle {
        self.rectangle
    }
}

impl fmt::Debug for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Line")
            .field("text_bytes", &self.text.len())
            .field("rectangle", &self.rectangle)
            .finish()
    }
}

/// Ungrouped text regions in detector order; reading order and paragraphs are the caller's concern.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// Input width in pixels.
    pub width: u32,
    /// Input height in pixels.
    pub height: u32,
    /// Recognized regions with no column ordering or paragraph inference applied.
    pub lines: Vec<Line>,
}

pub(crate) fn page(
    regions: impl IntoIterator<Item = Result<TextRegion, Error>>,
    width: u32,
    height: u32,
    limits: &Limits,
) -> Result<Page, Error> {
    let mut lines = Vec::new();
    let mut bytes = 0usize;
    for region in regions {
        let region = region?;
        if lines.len() >= limits.max_lines {
            return Err(Error::OutputLimit);
        }
        let text = region.text.ok_or(Error::InvalidOutput)?;
        if text.trim().is_empty() {
            return Err(Error::EmptyRecognition);
        }
        bytes = bytes.checked_add(text.len()).ok_or(Error::OutputLimit)?;
        if bytes > limits.max_text_bytes {
            return Err(Error::OutputLimit);
        }
        let rectangle = rectangle(&region.bounding_box.points, width, height)?;
        lines.push(Line { text: text.to_string(), rectangle });
    }
    Ok(Page { width, height, lines })
}

pub(crate) fn rectangle(
    points: &[oar_ocr_core::processors::Point],
    width: u32,
    height: u32,
) -> Result<Rectangle, Error> {
    if points.len() != 4 {
        return Err(Error::InvalidOutput);
    }
    let (mut left, mut top, mut right, mut bottom) = (f32::INFINITY, f32::INFINITY, 0.0f32, 0.0f32);
    for point in points {
        if !point.x.is_finite()
            || !point.y.is_finite()
            || point.x < 0.0
            || point.y < 0.0
            || f64::from(point.x) > f64::from(width)
            || f64::from(point.y) > f64::from(height)
        {
            return Err(Error::InvalidOutput);
        }
        left = left.min(point.x);
        right = right.max(point.x);
        top = top.min(point.y);
        bottom = bottom.max(point.y);
    }
    if left >= right || top >= bottom {
        return Err(Error::InvalidOutput);
    }
    Ok(Rectangle { left, top, right, bottom })
}
