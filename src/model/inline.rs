use crate::model::{AnchorId, ImageOrigin, ImageSource, LinkTarget, Style};

/// One span of inline content.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize),
    serde(tag = "kind", content = "value", rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum Inline {
    /// Styled text. Runs are split wherever the style changes.
    Text {
        /// The text itself.
        text: String,
        /// The character style covering all of it.
        style: Style,
    },
    /// A hyperlink wrapping its own inline content.
    Link {
        /// The link text, which may carry its own styling.
        content: Vec<Self>,
        /// Where it points.
        target: LinkTarget,
    },
    /// An image.
    Image {
        /// Verified source alt text, empty when the source gives none.
        alt: String,
        /// Where the bytes are, or that they are gone.
        source: ImageSource,
        /// Where the image appears in the source document.
        origin: ImageOrigin,
    },
    /// Zero-width anchor marking an internal link target at this position
    /// (bookmarks on paragraphs, spans, list items, table cells, ...).
    Anchor(AnchorId),
    /// A reference to the [`Note`](crate::model::Note) with this id.
    NoteRef(String),
    /// A line break inside a block, not a new block.
    LineBreak,
    /// An inline formula, as LaTeX math without delimiters.
    Math(String),
    /// A checkbox control with its state.
    Checkbox(bool),
}

impl Inline {
    /// Unstyled text.
    pub fn plain(text: impl Into<String>) -> Self {
        Self::Text { text: text.into(), style: Style::PLAIN }
    }
}

/// Flatten inlines to their text, dropping styling and links but keeping link
/// text, image alt text, and formula source. Line breaks become newlines;
/// anchors and note references contribute nothing.
#[must_use]
pub fn inlines_to_plain_text(inlines: &[Inline]) -> String {
    let mut out = String::new();
    collect_plain_text(inlines, &mut out);
    out
}

fn collect_plain_text(inlines: &[Inline], out: &mut String) {
    for inline in inlines {
        match inline {
            Inline::Text { text, .. } => out.push_str(text),
            Inline::Link { content, .. } => collect_plain_text(content, out),
            Inline::Image { alt, .. } => out.push_str(alt),
            Inline::Math(tex) => out.push_str(tex),
            Inline::Checkbox(checked) => out.push_str(if *checked { "[x]" } else { "[ ]" }),
            Inline::Anchor(_) | Inline::NoteRef(_) => {}
            Inline::LineBreak => out.push('\n'),
        }
    }
}

/// True when nothing here would render as visible content: only whitespace,
/// empty-target links, anchors, and line breaks. An image or a note reference
/// always counts as content.
#[must_use]
pub fn inlines_are_empty(inlines: &[Inline]) -> bool {
    inlines.iter().all(|i| match i {
        Inline::Text { text, .. } => text.trim().is_empty(),
        Inline::Link { content, target } => target.is_empty() && inlines_are_empty(content),
        Inline::Image { .. } | Inline::NoteRef(_) | Inline::Checkbox(_) => false,
        Inline::Math(tex) => tex.trim().is_empty(),
        Inline::Anchor(_) | Inline::LineBreak => true,
    })
}
