use super::{
    Block, CellSlot, Document, ImageSource, Inline, LinkTarget, Node, Region, Unit, children,
};
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
    /// The named page style whose pages show this value, where values are kept per page style.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub page_style: Option<String>,
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

    /// The diagnostic region content on this side reports in.
    pub(crate) fn region(self) -> Region {
        match self {
            Self::Header => Region::Header,
            Self::Footer => Region::Footer,
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
        charge(1)?;
        if let Some(name) = &self.page_style {
            charge(text_work(name))?;
        }
        match &self.content {
            Some(content) => charge_blocks_copy(content, charge),
            None => Ok(()),
        }
    }
}

/// Charge a copy of `blocks`: each container and every owned string they hold.
pub(crate) fn charge_blocks_copy<E>(
    blocks: &[Block],
    mut charge: impl FnMut(u64) -> Result<(), E>,
) -> Result<(), E> {
    let mut pending = vec![Node::Blocks(blocks)];
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
        Block::HeaderFooter(HeaderFooter { page_style: Some(name), .. }) => {
            charge(text_work(name))?;
        }
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

impl HeaderFooter {
    /// Place every image the content shows in `unit`, charging each container walked.
    pub(crate) fn place_images<E>(
        &mut self,
        unit: Option<Unit>,
        mut charge: impl FnMut(u64) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut pending: Vec<Place<'_>> =
            self.content.as_deref_mut().map(Place::Blocks).into_iter().collect();
        while let Some(place) = pending.pop() {
            match place {
                Place::Blocks(blocks) => {
                    charge(blocks.len() as u64)?;
                    for block in blocks {
                        match block {
                            Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
                                pending.push(Place::Inlines(inlines));
                            }
                            Block::List(list) => pending.extend(
                                list.items.iter_mut().map(|item| Place::Blocks(&mut item.blocks)),
                            ),
                            Block::Table(table) => pending.extend(origins(&mut table.grid)),
                            Block::BlockQuote(inner)
                            | Block::HeaderFooter(Self { content: Some(inner), .. }) => {
                                pending.push(Place::Blocks(inner));
                            }
                            Block::HeaderFooter(_)
                            | Block::CodeBlock { .. }
                            | Block::Rule
                            | Block::Math(_)
                            | Block::Gap(_)
                            | Block::UnitStart(_) => {}
                        }
                    }
                }
                Place::Inlines(inlines) => {
                    charge(inlines.len() as u64)?;
                    for inline in inlines {
                        match inline {
                            Inline::Image { origin, .. } => origin.unit = unit,
                            Inline::Link { content, .. } => pending.push(Place::Inlines(content)),
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// What [`HeaderFooter::place_images`] has yet to walk.
enum Place<'d> {
    Blocks(&'d mut [Block]),
    Inlines(&'d mut [Inline]),
}

/// The blocks of each origin cell of `grid`, to walk.
fn origins(grid: &mut [Vec<CellSlot>]) -> impl Iterator<Item = Place<'_>> {
    grid.iter_mut().flatten().filter_map(|slot| match slot {
        CellSlot::Origin(cell) => Some(Place::Blocks(&mut cell.blocks)),
        CellSlot::Covered { .. } => None,
    })
}

impl Document {
    /// Charge a copy of every side story the document holds, wherever it stands, each with what
    /// it nests.
    #[cfg_attr(
        not(feature = "serde"),
        expect(dead_code, reason = "only the JSON copies a whole document")
    )]
    pub(crate) fn charge_side_copies<E>(
        &self,
        mut charge: impl FnMut(u64) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut pending: Vec<Node<'_>> = vec![Node::Blocks(&self.blocks)];
        pending.extend(self.notes.iter().map(|note| Node::Blocks(&note.blocks)));
        while let Some(node) = pending.pop() {
            match node {
                Node::Blocks(blocks) => {
                    charge(blocks.len() as u64)?;
                    for block in blocks {
                        match block {
                            Block::HeaderFooter(value) => value.charge_copy(&mut charge)?,
                            _ => pending.extend(children(block, &|_| None)),
                        }
                    }
                }
                Node::Cells(mut cells) => {
                    if let Some(cell) = cells.next() {
                        pending.push(Node::Cells(cells));
                        pending.push(Node::Blocks(&cell.blocks));
                    }
                }
                Node::Inlines(_) | Node::Inline(_) => {}
            }
        }
        Ok(())
    }

    pub(crate) fn push_header_footer<E>(
        &mut self,
        state: HeaderFooterState,
        previous: &mut HashMap<(HeaderFooterRegion, HeaderFooterVariant), HeaderFooterState>,
        charge: impl FnMut(u64) -> Result<(), E>,
    ) -> Result<bool, E> {
        push_header_footer(&mut self.blocks, state, previous, charge)
    }
}

/// Push `state` onto `blocks` unless it repeats the value in force for its region and variant,
/// or, first of its key, changes nothing a page shows; whether it was pushed.
pub(crate) fn push_header_footer<E>(
    blocks: &mut Vec<Block>,
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
        blocks.push(Block::HeaderFooter(state.value));
    }
    Ok(!unchanged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConvertError;
    use crate::ledger::Ledger;
    use crate::limits::Limit;

    fn linked_side(target: LinkTarget) -> HeaderFooter {
        HeaderFooter {
            page_style: None,
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
