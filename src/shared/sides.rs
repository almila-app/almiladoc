//! Side stories, the headers and footers a source reader resolves: each is read once with its
//! losses held back, then published where it takes effect, every change reporting those losses
//! in the unit that shows it.

use crate::ctx::ParseCtx;
use crate::diagnostics::Diagnostics;
use crate::error::ConvertError;
use crate::model::{
    Block, HeaderFooter, HeaderFooterCertainty, HeaderFooterRegion, HeaderFooterState,
    HeaderFooterVariant, push_header_footer,
};
use std::collections::HashMap;

/// One region and variant's value, with the losses reading it reported.
pub(crate) struct SideStory {
    state: HeaderFooterState,
    losses: Diagnostics,
}

impl SideStory {
    /// A value that uses the default variant, or shows nothing when `content` is empty.
    pub(crate) fn known(
        region: HeaderFooterRegion,
        variant: HeaderFooterVariant,
        content: Option<Vec<Block>>,
    ) -> Self {
        let value = HeaderFooter { page_style: None, region, variant, content };
        Self {
            state: HeaderFooterState { value, certainty: HeaderFooterCertainty::Known },
            losses: Diagnostics::default(),
        }
    }

    /// The story `read` returns, its reports held back; `None`, or content that lost what it
    /// declares, is shown, empty for `None`, and never taken for an equal value.
    pub(crate) fn read(
        ctx: &ParseCtx<'_>,
        region: HeaderFooterRegion,
        variant: HeaderFooterVariant,
        read: impl FnOnce() -> Result<Option<Vec<Block>>, ConvertError>,
    ) -> Result<Self, ConvertError> {
        let (content, losses) = ctx.holding_reports(read)?;
        Ok(Self::with_losses(region, variant, content, losses))
    }

    fn with_losses(
        region: HeaderFooterRegion,
        variant: HeaderFooterVariant,
        content: Option<Vec<Block>>,
        losses: Diagnostics,
    ) -> Self {
        let certainty = if content.is_some() && losses.lost_codes().next().is_none() {
            HeaderFooterCertainty::Known
        } else {
            HeaderFooterCertainty::Uncertain
        };
        let value = HeaderFooter {
            page_style: None,
            region,
            variant,
            content: Some(content.unwrap_or_default()),
        };
        Self { state: HeaderFooterState { value, certainty }, losses }
    }

    /// The story as the value of the page style `name`, kept apart from other page styles'.
    pub(crate) fn of_page_style(
        mut self,
        ctx: &ParseCtx<'_>,
        name: &str,
    ) -> Result<Self, ConvertError> {
        ctx.charge((name.len() as u64).max(1))?;
        self.state.value.page_style = Some(name.to_owned());
        Ok(self)
    }
}

type InForce = HashMap<(HeaderFooterRegion, HeaderFooterVariant), HeaderFooterState>;

/// The values in force for each page style, region and variant as stories publish in reading
/// order.
#[derive(Default)]
pub(crate) struct SideTimeline {
    previous: HashMap<Option<String>, InForce>,
}

impl SideTimeline {
    /// Push `story` onto `blocks` where it changes the value in force, its images placed and its
    /// losses reported in the current unit; a first value that shows nothing still reports them.
    pub(crate) fn publish(
        &mut self,
        ctx: &ParseCtx<'_>,
        story: &SideStory,
        blocks: &mut Vec<Block>,
    ) -> Result<(), ConvertError> {
        let charge = |work| ctx.charge(work);
        let value = &story.state.value;
        let key = (value.region, value.variant);
        let key_work = value.page_style.as_ref().map_or(1, |name| (name.len() as u64).max(1));
        ctx.charge(key_work)?;
        if !self.previous.contains_key(&value.page_style) {
            ctx.charge(key_work)?;
            self.previous.insert(value.page_style.clone(), InForce::new());
        }
        let previous = self.previous.get_mut(&value.page_style).ok_or(ConvertError::Internal)?;
        let first = !previous.contains_key(&key);
        value.charge_copy(charge)?;
        let pushed = push_header_footer(blocks, story.state.clone(), previous, charge)?;
        if pushed || first {
            ctx.report_held(&story.losses)?;
        }
        if !pushed {
            if first {
                value.charge_copy(charge)?;
                previous.insert(key, story.state.clone());
            }
            return Ok(());
        }
        if let Some(Block::HeaderFooter(value)) = blocks.last_mut() {
            value.place_images(ctx.current_unit(), charge)?;
        }
        Ok(())
    }
}

/// What one slide or notes page shows of its running header and footer, with what reading each
/// lost, published as its default values.
#[derive(Default)]
pub(crate) struct PageSides {
    header: (Vec<Block>, Diagnostics),
    footer: (Vec<Block>, Diagnostics),
}

impl PageSides {
    /// Add what `read` returns to the side it shows on, its reports held back for that side.
    pub(crate) fn read(
        &mut self,
        ctx: &ParseCtx<'_>,
        side: HeaderFooterRegion,
        read: impl FnOnce() -> Result<Vec<Block>, ConvertError>,
    ) -> Result<(), ConvertError> {
        let (blocks, held) = ctx.holding_reports(read)?;
        let (content, losses) = match side {
            HeaderFooterRegion::Header => &mut self.header,
            HeaderFooterRegion::Footer => &mut self.footer,
        };
        content.extend(blocks);
        for diagnostic in held.iter() {
            ctx.charge(1)?;
            let unit = diagnostic.unit;
            losses.add(diagnostic.code, diagnostic.region, unit, diagnostic.count);
        }
        Ok(())
    }

    /// Push the page's header and footer onto `blocks` where they change.
    pub(crate) fn publish(
        self,
        ctx: &ParseCtx<'_>,
        timeline: &mut SideTimeline,
        blocks: &mut Vec<Block>,
    ) -> Result<(), ConvertError> {
        for (side, (content, losses)) in
            [(HeaderFooterRegion::Header, self.header), (HeaderFooterRegion::Footer, self.footer)]
        {
            let story =
                SideStory::with_losses(side, HeaderFooterVariant::Default, Some(content), losses);
            timeline.publish(ctx, &story, blocks)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{DiagnosticCode, Region};
    use crate::limits::Limits;
    use crate::model::{AssetId, ImageOrigin, ImageSource, Inline, UnitKind};

    fn image_story(ctx: &ParseCtx<'_>) -> SideStory {
        SideStory::read(ctx, HeaderFooterRegion::Footer, HeaderFooterVariant::Default, || {
            ctx.emit(DiagnosticCode::FieldToken, Region::Footer);
            Ok(Some(vec![Block::Paragraph(vec![Inline::Image {
                alt: String::new(),
                source: ImageSource::Asset(AssetId(0)),
                origin: ImageOrigin::new(Region::Footer, None),
            }])]))
        })
        .unwrap()
    }

    fn origin_unit(block: &Block) -> Option<u32> {
        let Block::HeaderFooter(HeaderFooter { content: Some(content), .. }) = block else {
            panic!("{block:?}")
        };
        let [Block::Paragraph(inlines)] = content.as_slice() else { panic!("{content:?}") };
        let [Inline::Image { origin, .. }] = inlines.as_slice() else { panic!("{inlines:?}") };
        origin.unit.map(|unit| unit.ordinal.get())
    }

    #[test]
    fn a_story_reports_its_held_losses_and_places_its_images_in_each_unit_it_changes() {
        let limits = Limits::default();
        let ctx = ParseCtx::new(&limits);
        let story = image_story(&ctx);
        assert!(ctx.diagnostics().is_empty());
        let blank = SideStory::known(
            HeaderFooterRegion::Footer,
            HeaderFooterVariant::Default,
            Some(Vec::new()),
        );
        let mut timeline = SideTimeline::default();
        let mut blocks = Vec::new();
        for (position, published) in [&story, &story, &blank, &story].into_iter().enumerate() {
            let _unit = ctx.begin_unit(UnitKind::Slide, position).unwrap();
            timeline.publish(&ctx, published, &mut blocks).unwrap();
        }
        assert_eq!(blocks.len(), 3);
        assert_eq!([&blocks[0], &blocks[2]].map(origin_unit), [Some(1), Some(4)]);
        let rows: Vec<_> = ctx
            .diagnostics()
            .iter()
            .map(|d| (d.code, d.region, d.unit.map(std::num::NonZeroU32::get), d.count))
            .collect();
        assert_eq!(
            rows,
            [
                (DiagnosticCode::FieldToken, Region::Footer, Some(1), 1),
                (DiagnosticCode::FieldToken, Region::Footer, Some(4), 1),
            ]
        );
    }

    #[test]
    fn a_first_blank_reports_what_it_omitted_once_and_one_that_lost_content_always_shows() {
        let limits = Limits::default();
        let ctx = ParseCtx::new(&limits);
        let blank = |code| {
            SideStory::read(&ctx, HeaderFooterRegion::Header, HeaderFooterVariant::Default, || {
                ctx.emit(code, Region::Header);
                Ok(Some(Vec::new()))
            })
            .unwrap()
        };
        let (hidden, lost) =
            (blank(DiagnosticCode::HiddenOmitted), blank(DiagnosticCode::GraphicUnread));
        let mut blocks = Vec::new();
        let mut timeline = SideTimeline::default();
        for story in [&hidden, &hidden] {
            timeline.publish(&ctx, story, &mut blocks).unwrap();
        }
        assert_eq!(blocks, []);
        let mut timeline = SideTimeline::default();
        for story in [&lost, &lost] {
            timeline.publish(&ctx, story, &mut blocks).unwrap();
        }
        assert_eq!(blocks.len(), 2);
        let rows: Vec<_> = ctx.diagnostics().iter().map(|d| (d.code, d.count)).collect();
        assert_eq!(rows, [(DiagnosticCode::GraphicUnread, 2), (DiagnosticCode::HiddenOmitted, 1)]);
    }

    #[test]
    fn a_story_that_does_not_read_shows_empty_and_repeats() {
        let limits = Limits::default();
        let ctx = ParseCtx::new(&limits);
        let unread =
            SideStory::read(&ctx, HeaderFooterRegion::Header, HeaderFooterVariant::Even, || {
                Ok(None)
            })
            .unwrap();
        let mut timeline = SideTimeline::default();
        let mut blocks = Vec::new();
        for _ in 0..2 {
            timeline.publish(&ctx, &unread, &mut blocks).unwrap();
        }
        let expected = Block::HeaderFooter(HeaderFooter {
            page_style: None,
            region: HeaderFooterRegion::Header,
            variant: HeaderFooterVariant::Even,
            content: Some(Vec::new()),
        });
        assert_eq!(blocks, [expected.clone(), expected]);
    }
}
