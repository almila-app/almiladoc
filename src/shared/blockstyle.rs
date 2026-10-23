//! Paragraph styles that name a block container. Word, Pandoc and
//! LibreOffice all mark quotations and preformatted text with a built-in
//! paragraph style name rather than dedicated markup, so the name is the
//! only signal a frontend has. [`Outline`] is the paragraph outline level
//! that marks a heading.

use crate::model::{Block, Inline, Style, inlines_are_empty, inlines_to_plain_text};

/// The block container a paragraph style designates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BlockStyle {
    Quote,
    Code,
}

/// A paragraph's outline level. `Body` is the explicit off value, which
/// stops inheritance; `Inherit` leaves the decision to the style chain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Outline {
    #[default]
    Inherit,
    Body,
    Level(u8),
}

impl Outline {
    /// An `outlineLvl` operand: 0 to 8 are heading levels 1 to 9, and 9 is
    /// body text.
    pub(crate) fn from_operand(operand: u8) -> Self {
        if operand < 9 { Self::Level(operand + 1) } else { Self::Body }
    }

    pub(crate) fn or(self, base: Self) -> Self {
        if self == Self::Inherit { base } else { self }
    }

    pub(crate) fn level(self) -> Option<u8> {
        match self {
            Self::Level(level) => Some(level),
            Self::Inherit | Self::Body => None,
        }
    }
}

/// The container a paragraph style name designates. ODF encodes spaces in
/// internal style names as `_20_`.
pub(crate) fn from_style_name(name: &str) -> Option<BlockStyle> {
    let name = name.replace("_20_", " ");
    match name.trim().to_ascii_lowercase().as_str() {
        // Word, Pandoc, LibreOffice.
        "quote" | "intense quote" | "block text" | "quotations" => Some(BlockStyle::Quote),
        "html preformatted" | "source code" | "preformatted text" => Some(BlockStyle::Code),
        _ => None,
    }
}

/// Consecutive paragraphs sharing one styled container, folded into a single
/// block: producers write a multi-paragraph quote, and a code block one line
/// per paragraph, as a run of separately styled paragraphs.
#[derive(Default)]
pub(crate) enum StyledRun {
    #[default]
    Empty,
    Quote(Vec<Block>),
    Code(Vec<String>),
}

impl StyledRun {
    /// The container currently open.
    fn style(&self) -> Option<BlockStyle> {
        match self {
            Self::Empty => None,
            Self::Quote(_) => Some(BlockStyle::Quote),
            Self::Code(_) => Some(BlockStyle::Code),
        }
    }

    /// Add one styled paragraph, closing the open container first when the
    /// style changes.
    pub(crate) fn push(
        &mut self,
        style: BlockStyle,
        mut inlines: Vec<Inline>,
        out: &mut Vec<Block>,
    ) {
        if style == BlockStyle::Code
            && inlines
                .iter()
                .any(|inline| !matches!(inline, Inline::Text { .. } | Inline::LineBreak))
        {
            self.flush(out);
            code_text(&mut inlines);
            out.push(Block::Paragraph(inlines));
            return;
        }
        if self.style() != Some(style) {
            self.flush(out);
            *self = match style {
                BlockStyle::Quote => Self::Quote(Vec::new()),
                BlockStyle::Code => Self::Code(Vec::new()),
            };
        }
        match self {
            // Code is literal text; character styling is presentation the
            // source applied to its syntax, never content.
            Self::Code(lines) => lines.push(inlines_to_plain_text(&inlines)),
            Self::Quote(blocks) => {
                if !inlines_are_empty(&inlines) {
                    blocks.push(Block::Paragraph(inlines));
                }
            }
            Self::Empty => {}
        }
    }

    /// Close the open container, if any.
    pub(crate) fn flush(&mut self, out: &mut Vec<Block>) {
        match std::mem::take(self) {
            Self::Quote(inner) => {
                if !inner.is_empty() {
                    out.push(Block::BlockQuote(inner));
                }
            }
            Self::Code(lines) => {
                let first = lines.iter().position(|l| !l.trim().is_empty());
                let last = lines.iter().rposition(|l| !l.trim().is_empty());
                if let (Some(first), Some(last)) = (first, last) {
                    out.push(Block::CodeBlock { lang: None, text: lines[first..=last].join("\n") });
                }
            }
            Self::Empty => {}
        }
    }
}

fn code_text(inlines: &mut [Inline]) {
    for inline in inlines {
        match inline {
            Inline::Text { style, .. } => *style = Style { code: true, ..Style::PLAIN },
            Inline::Link { content, .. } => code_text(content),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_style_names_map_to_containers() {
        for name in ["Quote", "Intense Quote", "Block Text", "Quotations", "QUOTATIONS"] {
            assert_eq!(from_style_name(name), Some(BlockStyle::Quote), "{name}");
        }
        for name in ["HTML Preformatted", "Source Code", "Preformatted Text"] {
            assert_eq!(from_style_name(name), Some(BlockStyle::Code), "{name}");
        }
        // ODF internal names encode the spaces.
        assert_eq!(from_style_name("Preformatted_20_Text"), Some(BlockStyle::Code));
        for name in ["Normal", "Body Text", "heading 1", ""] {
            assert_eq!(from_style_name(name), None, "{name}");
        }
    }

    #[test]
    fn an_outline_operand_is_a_level_or_body_text() {
        assert_eq!(Outline::from_operand(0), Outline::Level(1));
        assert_eq!(Outline::from_operand(8), Outline::Level(9));
        assert_eq!(Outline::from_operand(9), Outline::Body);
        assert_eq!(Outline::from_operand(u8::MAX), Outline::Body);
    }

    #[test]
    fn a_specified_outline_wins_and_body_text_stops_inheritance() {
        let heading = Outline::Level(2);
        assert_eq!(Outline::Inherit.or(heading), heading);
        assert_eq!(Outline::Body.or(heading), Outline::Body);
        assert_eq!(Outline::Level(1).or(heading), Outline::Level(1));
        assert_eq!(Outline::Body.or(heading).level(), None);
        assert_eq!(Outline::Inherit.or(Outline::Inherit).level(), None);
        assert_eq!(heading.level(), Some(2));
    }

    #[test]
    fn code_style_preserves_semantic_inlines_and_literal_text_between_code_blocks() {
        use crate::model::{LinkTarget, Style};
        let semantic = vec![
            Inline::Link {
                content: vec![Inline::plain("link")],
                target: LinkTarget::External("https://example.org".into()),
            },
            Inline::NoteRef("n".into()),
            Inline::Math("x".into()),
            Inline::Checkbox(true),
            Inline::Anchor("a".into()),
        ];
        let mut run = StyledRun::default();
        let mut out = Vec::new();
        run.push(BlockStyle::Code, vec![Inline::plain("before")], &mut out);
        run.push(BlockStyle::Code, semantic, &mut out);
        run.push(BlockStyle::Code, vec![Inline::plain("after")], &mut out);
        run.flush(&mut out);
        let [
            Block::CodeBlock { text: before, .. },
            Block::Paragraph(content),
            Block::CodeBlock { text: after, .. },
        ] = out.as_slice()
        else {
            panic!("{out:?}")
        };
        assert_eq!((before.as_str(), after.as_str()), ("before", "after"));
        let [
            Inline::Link { content: label, .. },
            Inline::NoteRef(id),
            Inline::Math(math),
            Inline::Checkbox(true),
            Inline::Anchor(anchor),
        ] = content.as_slice()
        else {
            panic!("{content:?}")
        };
        assert_eq!((id.as_str(), math.as_str(), anchor.as_str()), ("n", "x", "a"));
        assert_eq!(
            label,
            &[Inline::Text { text: "link".into(), style: Style { code: true, ..Style::PLAIN } }]
        );
    }

    #[test]
    fn consecutive_paragraphs_fold_into_one_container() {
        let mut out = Vec::new();
        let mut run = StyledRun::default();
        run.push(BlockStyle::Code, vec![Inline::plain("fn main() {")], &mut out);
        run.push(BlockStyle::Code, vec![Inline::plain("}")], &mut out);
        run.push(BlockStyle::Quote, vec![Inline::plain("first")], &mut out);
        run.push(BlockStyle::Quote, vec![Inline::plain("second")], &mut out);
        run.flush(&mut out);
        assert_eq!(out.len(), 2, "{out:?}");
        let Block::CodeBlock { text, .. } = &out[0] else { panic!("{out:?}") };
        assert_eq!(text, "fn main() {\n}");
        let Block::BlockQuote(inner) = &out[1] else { panic!("{out:?}") };
        assert_eq!(inner.len(), 2);
    }

    #[test]
    fn blank_paragraphs_around_a_code_run_are_dropped() {
        let mut out = Vec::new();
        let mut run = StyledRun::default();
        run.push(BlockStyle::Code, vec![Inline::plain("")], &mut out);
        run.push(BlockStyle::Code, vec![Inline::plain("a")], &mut out);
        run.push(BlockStyle::Code, vec![Inline::plain("")], &mut out);
        run.push(BlockStyle::Code, vec![Inline::plain("b")], &mut out);
        run.push(BlockStyle::Code, vec![Inline::plain("")], &mut out);
        run.flush(&mut out);
        let [Block::CodeBlock { text, .. }] = &out[..] else { panic!("{out:?}") };
        assert_eq!(text, "a\n\nb");
    }
}
