use crate::model::{AnchorId, Gap, Inline, List, Table, inlines_to_plain_text};

/// One block-level piece of a document body.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize),
    serde(tag = "kind", content = "value", rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum Block {
    /// The start of a readable source unit, occurring only in the document body.
    UnitStart(crate::model::Unit),
    /// A section heading.
    Heading {
        /// Outline depth as the source assigns it, 1-based. Word outline
        /// levels reach past 6, so renderers clamp to what their target
        /// supports rather than the model doing it here.
        level: u8,
        /// Stable anchor id when the source document targets this heading
        /// (bookmark, chapter fragment, ...). Renderers map it to the
        /// heading's own anchor rather than emitting a separate one.
        anchor: Option<AnchorId>,
        /// The heading text.
        content: Vec<Inline>,
    },
    /// A run of body text.
    Paragraph(Vec<Inline>),
    /// A list, with its numbering already resolved.
    List(List),
    /// A table grid.
    Table(Table),
    /// Quoted content, which nests.
    BlockQuote(Vec<Self>),
    /// Preformatted text.
    CodeBlock {
        /// Language hint when the source names one.
        lang: Option<String>,
        /// The literal text, newlines intact.
        text: String,
    },
    /// A horizontal rule.
    Rule,
    /// A displayed formula, as LaTeX math without delimiters.
    Math(String),
    /// Content that awaits OCR.
    Gap(Gap),
}

impl Block {
    /// A heading with no anchor, for the common case where nothing in the
    /// document links to it.
    #[must_use]
    pub fn heading(level: u8, content: Vec<Inline>) -> Self {
        Self::Heading { level, anchor: None, content }
    }

    /// The text of this block and every block inside it, list items and table cells joined by
    /// spaces.
    pub(crate) fn plain_text(&self) -> String {
        let join = |texts: Vec<String>| texts.join(" ");
        match self {
            Self::Paragraph(inlines) | Self::Heading { content: inlines, .. } => {
                inlines_to_plain_text(inlines)
            }
            Self::List(list) => join(
                list.items.iter().flat_map(|item| &item.blocks).map(Self::plain_text).collect(),
            ),
            Self::BlockQuote(blocks) => join(blocks.iter().map(Self::plain_text).collect()),
            Self::CodeBlock { text, .. } | Self::Math(text) => text.clone(),
            Self::Table(table) => join(
                table
                    .origins()
                    .map(|cell| join(cell.blocks.iter().map(Self::plain_text).collect()))
                    .collect(),
            ),
            Self::Rule | Self::UnitStart(_) | Self::Gap(_) => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cell, ListItem, MarkerKind};

    #[test]
    fn plain_text_joins_items_blocks_and_cells_by_spaces() {
        let para = |text: &str| Block::Paragraph(vec![Inline::plain(text)]);
        let item = |blocks| ListItem { blocks, marker_label: None };
        let items = vec![item(vec![para("a"), para("b")]), item(vec![para("c")])];
        let list = Block::List(List { marker: MarkerKind::Bullet, start: 1, items });
        let cells = vec![Cell::new(vec![para("d"), para("e")]), Cell::new(vec![para("f")])];
        let table = Block::Table(Table::from_rows(vec![cells], 0));
        let code = Block::CodeBlock { lang: None, text: "g\nh".into() };
        let quote = Block::BlockQuote(vec![list, table, Block::Rule, code]);
        assert_eq!(quote.plain_text(), "a b c d e f  g\nh");
    }
}
