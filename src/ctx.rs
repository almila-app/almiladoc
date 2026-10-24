//! The context every frontend parses under: the one path from a call's
//! [`Config`](crate::Config) to the code that reads the input, and from it
//! to the call's [`Diagnostics`].

use std::cell::Cell;
use std::collections::BTreeSet;
use std::fmt;
use std::num::NonZeroU32;

use crate::config::{ConfigError, ImagePolicy, PageSelection};
use crate::diagnostics::{DiagnosticCode, Diagnostics, Region};
use crate::error::ConvertError;
use crate::ledger::{CancelToken, Ledger};
use crate::limits::{Limit, Limits};
use crate::model::{GridLedger, Unit, UnitKind};

/// What one parse reads its configuration from, charges its work to and
/// reports its losses to.
pub(crate) struct ParseCtx<'a> {
    limits: &'a Limits,
    images: ImagePolicy,
    ocr_pages: PageSelection,
    ledger: Ledger<'a>,
    diagnostics: Cell<Diagnostics>,
    unit: Cell<Option<Unit>>,
    unread_units: Cell<BTreeSet<Unit>>,
    archive_bytes: Cell<u64>,
    grid_slots: Cell<u64>,
    expansion_text_bytes: Cell<u64>,
    #[cfg(test)]
    detection_counter: Option<&'a std::sync::atomic::AtomicUsize>,
}

#[derive(Clone, Copy)]
pub(crate) enum ImageLoss {
    Unavailable,
    Unplaced,
}

/// While alive, reports go to one unit or across the document; dropping it
/// restores the unit it replaced.
pub(crate) struct ReportScope<'c> {
    unit: &'c Cell<Option<Unit>>,
    replaced: Option<Unit>,
}

impl Drop for ReportScope<'_> {
    fn drop(&mut self) {
        self.unit.set(self.replaced);
    }
}

/// A unit [`ParseCtx::begin_unit`] numbered within [`Limits::max_pages`],
/// which [`ParseCtx::enter_admitted`] enters again at no cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AdmittedUnit(Unit);

impl AdmittedUnit {
    pub(crate) fn model(self) -> Unit {
        self.0
    }
}

/// While alive, reports go to one admitted unit; dropping it restores the
/// unit it replaced.
pub(crate) struct UnitScope<'c> {
    unit: AdmittedUnit,
    _reports: ReportScope<'c>,
}

impl UnitScope<'_> {
    pub(crate) fn unit(&self) -> AdmittedUnit {
        self.unit
    }
}

impl<'a> ParseCtx<'a> {
    pub(crate) fn new(limits: &'a Limits) -> Self {
        Self {
            limits,
            images: ImagePolicy::Placeholder,
            ocr_pages: PageSelection::None,
            ledger: Ledger::new(limits.max_work, None),
            diagnostics: Cell::default(),
            unit: Cell::default(),
            unread_units: Cell::default(),
            archive_bytes: Cell::default(),
            grid_slots: Cell::default(),
            expansion_text_bytes: Cell::default(),
            #[cfg(test)]
            detection_counter: None,
        }
    }

    pub(crate) fn with_cancel(self, cancel: Option<&'a CancelToken>) -> Self {
        Self { ledger: Ledger::new(self.limits.max_work, cancel), ..self }
    }

    pub(crate) fn starting_at(self, spent: u64) -> Self {
        Self { ledger: self.ledger.starting_at(spent), ..self }
    }

    pub(crate) fn with_images(self, images: ImagePolicy) -> Self {
        Self { images, ..self }
    }

    pub(crate) fn with_ocr_pages(self, ocr_pages: PageSelection) -> Self {
        Self { ocr_pages, ..self }
    }

    #[cfg_attr(
        not(feature = "pdf"),
        expect(dead_code, reason = "only the PDF reader selects pages")
    )]
    pub(crate) fn ocr_requested(&self, page: NonZeroU32) -> bool {
        match &self.ocr_pages {
            PageSelection::None => false,
            PageSelection::All => true,
            PageSelection::Pages(pages) => pages.contains(&page),
        }
    }

    /// Refuses a selection naming a page past `page_count`.
    #[cfg_attr(
        not(feature = "pdf"),
        expect(dead_code, reason = "only the PDF reader selects pages")
    )]
    pub(crate) fn admit_ocr_pages(&self, page_count: u32) -> Result<(), ConvertError> {
        let PageSelection::Pages(pages) = &self.ocr_pages else { return Ok(()) };
        match pages.last() {
            Some(page) if page.get() > page_count => {
                Err(ConvertError::InvalidConfig(ConfigError::PageOutOfRange {
                    page: page.get(),
                    page_count,
                }))
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn image_loss(&self, loss: ImageLoss, region: Region) {
        let code = match (self.images, loss) {
            (ImagePolicy::Placeholder, ImageLoss::Unavailable) => DiagnosticCode::ImageUnavailable,
            (_, ImageLoss::Unavailable) => DiagnosticCode::ImageOmitted,
            (_, ImageLoss::Unplaced) => self.unplaced_image_code(),
        };
        self.emit(code, region);
    }

    pub(crate) fn image_origin(&self, region: Region) -> crate::model::ImageOrigin {
        crate::model::ImageOrigin::new(region, self.unit.get())
    }

    pub(crate) fn unplaced_image_code(&self) -> DiagnosticCode {
        if self.images == ImagePolicy::Omit {
            DiagnosticCode::ImageOmitted
        } else {
            DiagnosticCode::ImageUnplaced
        }
    }

    pub(crate) fn limits(&self) -> &'a Limits {
        self.limits
    }

    /// The ledger this parse charges, which rendering its document charges
    /// on.
    pub(crate) fn ledger(&self) -> &Ledger<'a> {
        &self.ledger
    }

    /// Charge `units` of work, failing once the parse is cancelled or past
    /// [`Limits::max_work`].
    pub(crate) fn charge(&self, units: u64) -> Result<(), ConvertError> {
        self.ledger.charge(units)
    }

    /// Decompressed bytes this parse has read from its archive, by every
    /// package opened over it, against [`Limits::max_total_bytes`].
    pub(crate) fn archive_bytes(&self) -> u64 {
        self.archive_bytes.get()
    }

    pub(crate) fn count_archive_bytes(&self, bytes: u64) {
        self.archive_bytes.set(self.archive_bytes.get().saturating_add(bytes));
    }

    /// Work this parse may still charge, of [`Limits::max_work`].
    pub(crate) fn work_left(&self) -> u64 {
        self.limits.max_work.saturating_sub(self.ledger.spent())
    }

    /// Grid positions this parse may still materialize, of
    /// [`Limits::max_grid_slots`] for all its tables together.
    pub(crate) fn grid_slots_left(&self) -> u64 {
        self.limits.max_grid_slots.saturating_sub(self.grid_slots.get())
    }

    /// Count `positions` more grid positions against
    /// [`Limits::max_grid_slots`], charging each one unit of work.
    pub(crate) fn admit_grid_positions(&self, positions: u64) -> Result<(), ConvertError> {
        self.admit(&self.grid_slots, positions, Limit::GridSlots, self.limits.max_grid_slots)
    }

    /// Count `bytes` more text bytes that repeat expansion duplicates
    /// against [`Limits::max_expansion_text_bytes`] for all the tables of
    /// this parse together, charging each one unit of work.
    pub(crate) fn admit_expansion_text_bytes(&self, bytes: u64) -> Result<(), ConvertError> {
        self.admit(
            &self.expansion_text_bytes,
            bytes,
            Limit::ExpansionTextBytes,
            self.limits.max_expansion_text_bytes,
        )
    }

    fn admit(
        &self,
        used: &Cell<u64>,
        amount: u64,
        limit: Limit,
        max: u64,
    ) -> Result<(), ConvertError> {
        let total = used.get().saturating_add(amount);
        used.set(total);
        if total > max {
            return Err(ConvertError::limit(limit, max));
        }
        self.charge(amount)
    }

    /// Report one occurrence of `code` in `region`, in the current unit.
    pub(crate) fn emit(&self, code: DiagnosticCode, region: Region) {
        self.emit_count(code, region, 1);
    }

    /// Report `count` occurrences of `code` in `region`, in the current unit.
    pub(crate) fn emit_count(&self, code: DiagnosticCode, region: Region, count: u64) {
        let mut diagnostics = self.diagnostics.take();
        let unit = self.unit.get();
        if code == DiagnosticCode::UnitUnreadable
            && count > 0
            && let Some(unit) = unit
        {
            let mut unread = self.unread_units.take();
            unread.insert(unit);
            self.unread_units.set(unread);
        }
        diagnostics.add(code, region, unit.map(|u| u.ordinal), count);
        self.diagnostics.set(diagnostics);
    }

    /// Admit the unit at 0-based `position` in its document's list (a slide,
    /// sheet or spine item) and report in it until the scope drops, refusing
    /// a unit numbered past [`Limits::max_pages`].
    pub(crate) fn begin_unit(
        &self,
        kind: UnitKind,
        position: usize,
    ) -> Result<UnitScope<'_>, ConvertError> {
        let max = self.limits.max_pages;
        let ordinal = u32::try_from(position)
            .ok()
            .and_then(|position| NonZeroU32::MIN.checked_add(position))
            .filter(|ordinal| u64::from(ordinal.get()) <= max)
            .ok_or_else(|| ConvertError::limit(Limit::Pages, max))?;
        Ok(self.enter_admitted(AdmittedUnit(Unit::new(kind, ordinal))))
    }

    /// Report in a unit [`ParseCtx::begin_unit`] admitted until the scope
    /// drops.
    pub(crate) fn enter_admitted(&self, unit: AdmittedUnit) -> UnitScope<'_> {
        UnitScope { unit, _reports: self.scope(Some(unit.0)) }
    }

    /// Report across the document until the scope drops: for a part every
    /// unit shares.
    pub(crate) fn outside_units(&self) -> ReportScope<'_> {
        self.scope(None)
    }

    fn scope(&self, unit: Option<Unit>) -> ReportScope<'_> {
        ReportScope { unit: &self.unit, replaced: self.unit.replace(unit) }
    }

    /// What this parse reported, the units it could not read, and the ledger its work goes on in.
    pub(crate) fn into_report(self) -> (Diagnostics, BTreeSet<Unit>, Ledger<'a>) {
        (self.diagnostics.into_inner(), self.unread_units.into_inner(), self.ledger)
    }

    #[cfg(test)]
    pub(crate) fn into_diagnostics(self) -> Diagnostics {
        self.into_report().0
    }

    #[cfg(test)]
    pub(crate) fn note_detection(&self) {
        if let Some(counter) = self.detection_counter {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    pub(crate) fn with_detection_counter(
        mut self,
        counter: &'a std::sync::atomic::AtomicUsize,
    ) -> Self {
        self.detection_counter = Some(counter);
        self
    }

    #[cfg(test)]
    pub(crate) fn fuel(&self) -> u64 {
        self.ledger.spent()
    }

    #[cfg(test)]
    pub(crate) fn diagnostics(&self) -> Diagnostics {
        let diagnostics = self.diagnostics.take();
        self.diagnostics.set(diagnostics.clone());
        diagnostics
    }
}

impl GridLedger for &ParseCtx<'_> {
    type Error = ConvertError;

    fn max_expansion(&self) -> u64 {
        self.limits.max_expansion
    }

    fn admit_positions(&mut self, count: u64) -> Result<(), ConvertError> {
        self.admit_grid_positions(count)
    }
}

impl fmt::Debug for ParseCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParseCtx")
            .field("limits", &self.limits)
            .field("ledger", &self.ledger)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
impl Default for ParseCtx<'static> {
    fn default() -> Self {
        Self::new(&Limits::DEFAULT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cell as TableCell, GridBuilder};
    use std::assert_matches;

    #[test]
    fn charges_run_against_the_contexts_max_work() {
        let limits = Limits { max_work: 3, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        parse_ctx.charge(3).unwrap();
        let err = parse_ctx.charge(1).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Work, max: 3 });
        assert_eq!(parse_ctx.fuel(), 4);
    }

    #[test]
    fn reports_aggregate_until_the_context_is_spent() {
        let parse_ctx = ParseCtx::default();
        parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Note);
        parse_ctx.emit(DiagnosticCode::PartUnreadable, Region::Note);
        parse_ctx.emit(DiagnosticCode::StylePartUnreadable, Region::Container);
        assert_eq!(parse_ctx.diagnostics().iter().count(), 2);
        let diagnostics = parse_ctx.into_diagnostics();
        let counts: Vec<(DiagnosticCode, u64)> =
            diagnostics.iter().map(|d| (d.code, d.count)).collect();
        assert_eq!(
            counts,
            [(DiagnosticCode::PartUnreadable, 2), (DiagnosticCode::StylePartUnreadable, 1)]
        );
    }

    #[test]
    fn reports_go_to_the_innermost_unit_until_its_scope_drops() {
        let parse_ctx = ParseCtx::default();
        let code = DiagnosticCode::PartUnreadable;
        {
            let _slide = parse_ctx.begin_unit(UnitKind::Slide, 1).unwrap();
            parse_ctx.emit(code, Region::Body);
            {
                let _shared = parse_ctx.outside_units();
                parse_ctx.emit(code, Region::Body);
            }
            parse_ctx.emit(code, Region::Note);
        }
        parse_ctx.emit(code, Region::Note);
        let reported: Vec<_> =
            parse_ctx.into_diagnostics().iter().map(|d| (d.region, d.unit, d.count)).collect();
        let second = NonZeroU32::new(2);
        assert_eq!(
            reported,
            [
                (Region::Body, None, 1),
                (Region::Body, second, 1),
                (Region::Note, None, 1),
                (Region::Note, second, 1)
            ]
        );
    }

    #[test]
    fn a_unit_numbered_past_max_pages_is_refused_and_reentry_costs_nothing() {
        let limits = Limits { max_pages: 2, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let code = DiagnosticCode::PartUnreadable;
        let units = [0, 1].map(|position| {
            let scope = parse_ctx.begin_unit(UnitKind::Slide, position).unwrap();
            parse_ctx.emit(code, Region::Body);
            scope.unit()
        });
        for unit in units.into_iter().rev() {
            let _again = parse_ctx.enter_admitted(unit);
            parse_ctx.emit(code, Region::Body);
        }
        {
            let _second = parse_ctx.enter_admitted(units[1]);
            let refused = parse_ctx.begin_unit(UnitKind::Slide, 2).map(drop).unwrap_err();
            assert_matches!(refused, ConvertError::ResourceLimit { limit: Limit::Pages, max: 2 });
            parse_ctx.emit(code, Region::Note);
        }
        assert_eq!(parse_ctx.fuel(), 0);
        let reported: Vec<_> =
            parse_ctx.into_diagnostics().iter().map(|d| (d.region, d.unit, d.count)).collect();
        let (first, second) = (NonZeroU32::new(1), NonZeroU32::new(2));
        assert_eq!(
            reported,
            [(Region::Body, first, 2), (Region::Body, second, 2), (Region::Note, second, 1)]
        );
    }

    #[test]
    fn a_unit_numbered_past_the_ceiling_is_refused_with_the_configured_max() {
        let max = Limits::ceiling(Limit::Pages);
        let limits = Limits { max_pages: max, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let last = usize::try_from(u32::MAX).unwrap();
        parse_ctx.begin_unit(UnitKind::Slide, last - 1).unwrap();
        let refused = parse_ctx.begin_unit(UnitKind::Slide, last).map(drop).unwrap_err();
        assert_matches!(refused, ConvertError::ResourceLimit { limit: Limit::Pages, max: m } if m == max);
    }

    #[test]
    fn a_cancelled_token_stops_the_next_charge() {
        let token = CancelToken::new();
        let parse_ctx = ParseCtx::default().with_cancel(Some(&token));
        parse_ctx.charge(1).unwrap();
        token.cancel();
        assert_matches!(parse_ctx.charge(1), Err(ConvertError::Cancelled));
        ParseCtx::default().with_cancel(None).charge(1).unwrap();
    }

    #[test]
    fn the_tables_of_a_parse_share_its_grid_slots_and_charge_each_position() {
        let limits = Limits { max_grid_slots: 5, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let table = |cells: usize| {
            let mut builder = GridBuilder::new(&parse_ctx);
            builder.next_row();
            for _ in 0..cells {
                builder.place(TableCell::default())?;
            }
            builder.finish()
        };
        table(1).unwrap();
        parse_ctx.admit_grid_positions(2).unwrap();
        assert_eq!((parse_ctx.grid_slots_left(), parse_ctx.fuel()), (2, 3));
        let refused = table(3).unwrap_err();
        assert_matches!(refused, ConvertError::ResourceLimit { limit: Limit::GridSlots, max: 5 });
    }

    #[test]
    fn the_tables_of_a_parse_share_their_expansion_text_bytes_and_charge_each_byte() {
        let limits = Limits { max_expansion_text_bytes: 5, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        parse_ctx.admit_expansion_text_bytes(2).unwrap();
        parse_ctx.admit_expansion_text_bytes(0).unwrap();
        parse_ctx.admit_expansion_text_bytes(3).unwrap();
        assert_eq!(parse_ctx.fuel(), 5);
        let refused = parse_ctx.admit_expansion_text_bytes(1).unwrap_err();
        assert_matches!(
            refused,
            ConvertError::ResourceLimit { limit: Limit::ExpansionTextBytes, max: 5 }
        );
        assert_eq!(parse_ctx.fuel(), 5);
    }

    #[test]
    fn expansion_text_bytes_saturate_instead_of_wrapping() {
        let limits = Limits { max_expansion_text_bytes: 5, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        for bytes in [u64::MAX, 2, u64::MAX] {
            assert_matches!(
                parse_ctx.admit_expansion_text_bytes(bytes),
                Err(ConvertError::ResourceLimit { limit: Limit::ExpansionTextBytes, max: 5 })
            );
        }
    }

    #[test]
    fn an_expansion_text_admission_stops_at_cancellation() {
        let token = CancelToken::new();
        let parse_ctx = ParseCtx::default().with_cancel(Some(&token));
        parse_ctx.admit_expansion_text_bytes(0).unwrap();
        token.cancel();
        assert_matches!(parse_ctx.admit_expansion_text_bytes(0), Err(ConvertError::Cancelled));
        assert_matches!(parse_ctx.admit_expansion_text_bytes(1), Err(ConvertError::Cancelled));
    }

    #[test]
    fn a_grid_position_stops_at_cancellation() {
        let token = CancelToken::new();
        let parse_ctx = ParseCtx::default().with_cancel(Some(&token));
        token.cancel();
        let mut builder = GridBuilder::new(&parse_ctx);
        builder.next_row();
        assert_matches!(builder.place(TableCell::default()), Err(ConvertError::Cancelled));
    }
    #[test]
    fn image_loss_policy_does_not_hide_unrelated_content_damage() {
        for images in [ImagePolicy::Placeholder, ImagePolicy::AltText, ImagePolicy::Omit] {
            let ctx = ParseCtx::default().with_images(images);
            assert_eq!(
                ctx.image_origin(Region::Note),
                crate::model::ImageOrigin::new(Region::Note, None)
            );
            ctx.image_loss(ImageLoss::Unavailable, Region::Body);
            ctx.image_loss(ImageLoss::Unavailable, Region::Body);
            ctx.image_loss(ImageLoss::Unplaced, Region::Body);
            ctx.emit(DiagnosticCode::PartUnreadable, Region::Body);
            let diagnostics = ctx.diagnostics();
            let count =
                |code| diagnostics.iter().filter(|d| d.code == code).map(|d| d.count).sum::<u64>();
            assert_eq!(count(DiagnosticCode::PartUnreadable), 1);
            assert_eq!(
                count(DiagnosticCode::ImageUnavailable),
                if images == ImagePolicy::Placeholder { 2 } else { 0 }
            );
            assert_eq!(
                count(DiagnosticCode::ImageUnplaced),
                u64::from(images != ImagePolicy::Omit)
            );
            assert_eq!(
                count(DiagnosticCode::ImageOmitted),
                match images {
                    ImagePolicy::Placeholder => 0,
                    ImagePolicy::AltText => 2,
                    ImagePolicy::Omit => 3,
                }
            );
        }
    }
}
