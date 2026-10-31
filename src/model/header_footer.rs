use super::{Block, Document, ImageSource, Inline, LinkTarget, Node, children};
use std::collections::HashMap;

/// A header or footer change with content isolated from body anchors and notes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct HeaderFooter {
    /// The side of the page this value describes.
    pub region: HeaderFooterRegion,
    /// The printed-page variant this value changes.
    pub variant: HeaderFooterVariant,
    /// `None` uses the default variant; an empty block list clears the content.
    pub content: Option<Vec<Block>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeaderFooterCertainty {
    Known,
    Uncertain,
}

#[derive(Clone)]
pub(crate) struct HeaderFooterState {
    pub(crate) value: HeaderFooter,
    pub(crate) certainty: HeaderFooterCertainty,
}

impl HeaderFooterState {
    pub(crate) fn same_content<E>(
        &self,
        other: &Self,
        charge: impl FnMut(u64) -> Result<(), E>,
    ) -> Result<bool, E> {
        self.value.charge_copy(charge)?;
        Ok(self.certainty == HeaderFooterCertainty::Known
            && other.certainty == HeaderFooterCertainty::Known
            && self.value.content == other.value.content)
    }
}

/// The page side a print value belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum HeaderFooterRegion {
    /// Text printed above the page body.
    Header,
    /// Text printed below the page body.
    Footer,
}

/// The printed pages a header or footer value applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum HeaderFooterVariant {
    /// Pages without a first-page or even-page override.
    Default,
    /// The first printed page.
    First,
    /// Even printed pages.
    Even,
}

impl HeaderFooterRegion {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Footer => "footer",
        }
    }
}

impl HeaderFooterVariant {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::First => "first",
            Self::Even => "even",
        }
    }
}

impl HeaderFooter {
    pub(crate) fn charge_copy<E>(
        &self,
        mut charge: impl FnMut(u64) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut pending = Vec::new();
        charge(1)?;
        if let Some(content) = &self.content {
            pending.push(Node::Blocks(content));
        }
        while let Some(node) = pending.pop() {
            match node {
                Node::Blocks(blocks) => {
                    charge((blocks.len() as u64).saturating_mul(2))?;
                    for block in blocks {
                        charge_block_fields(block, &mut charge)?;
                        pending.extend(children(block, &|_| None));
                    }
                }
                Node::Inlines(inlines) => {
                    charge((inlines.len() as u64).saturating_mul(2))?;
                    pending.extend(inlines.iter().map(Node::Inline));
                }
                Node::Inline(inline) => {
                    charge_inline_fields(inline, &mut charge)?;
                    if let Inline::Link { content, .. } = inline {
                        pending.push(Node::Inlines(content));
                    }
                }
                Node::Cells(mut cells) => {
                    if let Some(cell) = cells.next() {
                        pending.push(Node::Cells(cells));
                        pending.push(Node::Blocks(&cell.blocks));
                    }
                }
            }
        }
        Ok(())
    }
}

fn text_work(value: &str) -> u64 {
    (value.len() as u64).max(1)
}

fn charge_block_fields<E>(
    block: &Block,
    charge: &mut impl FnMut(u64) -> Result<(), E>,
) -> Result<(), E> {
    match block {
        Block::Heading { anchor: Some(value), .. } | Block::Math(value) => {
            charge(text_work(value))?;
        }
        Block::CodeBlock { lang, text: value } => {
            if let Some(lang) = lang {
                charge(text_work(lang))?;
            }
            charge(text_work(value))?;
        }
        Block::List(list) => {
            charge((list.items.len() as u64).saturating_mul(3))?;
            for item in &list.items {
                if let Some(label) = &item.marker_label {
                    charge(text_work(label))?;
                }
            }
        }
        Block::Table(table) => {
            charge(table.grid.len() as u64)?;
            for row in &table.grid {
                charge((row.len() as u64).saturating_mul(3))?;
            }
        }
        Block::Gap(gap) => charge(gap.reasons.len() as u64)?,
        Block::Paragraph(_)
        | Block::Heading { anchor: None, .. }
        | Block::BlockQuote(_)
        | Block::HeaderFooter(_)
        | Block::Rule
        | Block::UnitStart(_) => {}
    }
    Ok(())
}

fn charge_inline_fields<E>(
    inline: &Inline,
    charge: &mut impl FnMut(u64) -> Result<(), E>,
) -> Result<(), E> {
    match inline {
        Inline::Text { text: value, .. }
        | Inline::Math(value)
        | Inline::Anchor(value)
        | Inline::NoteRef(value) => charge(text_work(value))?,
        Inline::Link { target, .. } => match target {
            LinkTarget::External(value)
            | LinkTarget::Relative(value)
            | LinkTarget::Anchor { id: value, .. } => charge(text_work(value))?,
            LinkTarget::Unit(_) => {}
        },
        Inline::Image { alt, source, .. } => {
            charge(text_work(alt))?;
            match source {
                ImageSource::External(value) => charge(text_work(value))?,
                ImageSource::Asset(_) | ImageSource::Unavailable | ImageSource::Unread => {}
            }
        }
        Inline::LineBreak | Inline::Checkbox(_) => {}
    }
    Ok(())
}

impl Document {
    pub(crate) fn push_header_footer<E>(
        &mut self,
        state: HeaderFooterState,
        previous: &mut HashMap<(HeaderFooterRegion, HeaderFooterVariant), HeaderFooterState>,
        mut charge: impl FnMut(u64) -> Result<(), E>,
    ) -> Result<bool, E> {
        let key = (state.value.region, state.value.variant);
        let unchanged = if let Some(old) = previous.get(&key) {
            state.same_content(old, &mut charge)?
        } else {
            state.value.charge_copy(&mut charge)?;
            state.certainty == HeaderFooterCertainty::Known
                && (state.value.content.is_none()
                    || (state.value.variant == HeaderFooterVariant::Default
                        && state.value.content.as_ref().is_some_and(Vec::is_empty)))
        };
        if !unchanged {
            state.value.charge_copy(charge)?;
            previous.insert(key, state.clone());
            self.blocks.push(Block::HeaderFooter(state.value));
        }
        Ok(!unchanged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConvertError;
    use crate::ledger::Ledger;
    use crate::limits::Limit;

    fn linked_side(target: LinkTarget) -> HeaderFooter {
        HeaderFooter {
            region: HeaderFooterRegion::Header,
            variant: HeaderFooterVariant::Default,
            content: Some(vec![Block::BlockQuote(vec![Block::Paragraph(vec![Inline::Link {
                content: vec![Inline::plain("")],
                target,
            }])])]),
        }
    }

    #[test]
    fn side_copy_charges_owned_keys_and_empty_text_before_cloning() {
        let value = linked_side(LinkTarget::External("é".into()));
        let ledger = Ledger::new(12, None);
        value.charge_copy(|work| ledger.charge(work)).unwrap();
        assert_eq!(ledger.spent(), 12);
        let small = Ledger::new(11, None);
        assert_eq!(
            value.charge_copy(|work| small.charge(work)),
            Err(ConvertError::limit(Limit::Work, 11))
        );
        let token = crate::CancelToken::default();
        token.cancel();
        let cancelled = Ledger::new(12, Some(&token));
        assert_eq!(value.charge_copy(|work| cancelled.charge(work)), Err(ConvertError::Cancelled));
    }
    #[test]
    fn side_copy_prices_every_owned_link_target_by_utf8_bytes() {
        let wide = "é".repeat(40);
        for (target, expected) in [
            (LinkTarget::External(format!("https://e/{wide}")), 100),
            (LinkTarget::Relative(format!("{wide}/target")), 97),
            (LinkTarget::anchor(wide), 90),
        ] {
            let value = linked_side(target);
            let ledger = Ledger::new(expected, None);
            value.charge_copy(|work| ledger.charge(work)).unwrap();
            assert_eq!(ledger.spent(), expected);
            let small = Ledger::new(expected - 1, None);
            assert_eq!(
                value.charge_copy(|work| small.charge(work)),
                Err(ConvertError::limit(Limit::Work, expected - 1))
            );
        }
    }
}
