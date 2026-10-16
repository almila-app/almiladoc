//! The context every frontend parses under: the one path from a call's
//! [`Config`](crate::Config) to the code that reads the input, and from it
//! to the call's [`Diagnostics`].

use std::cell::Cell;
use std::fmt;
use std::num::NonZeroU32;

use crate::diagnostics::{DiagnosticCode, Diagnostics, Region};
use crate::error::ConvertError;
use crate::ledger::{CancelToken, Ledger};
use crate::limits::Limits;

/// What one parse reads its configuration from, charges its work to and
/// reports its losses to.
pub(crate) struct ParseCtx<'a> {
    limits: &'a Limits,
    ledger: Ledger<'a>,
    diagnostics: Cell<Diagnostics>,
    unit: Cell<Option<NonZeroU32>>,
}

/// While alive, reports go to one unit or across the document; dropping it
/// restores the unit it replaced.
pub(crate) struct UnitScope<'c> {
    unit: &'c Cell<Option<NonZeroU32>>,
    replaced: Option<NonZeroU32>,
}

impl Drop for UnitScope<'_> {
    fn drop(&mut self) {
        self.unit.set(self.replaced);
    }
}

impl<'a> ParseCtx<'a> {
    pub(crate) fn new(limits: &'a Limits) -> Self {
        ParseCtx {
            limits,
            ledger: Ledger::new(limits.max_work, None),
            diagnostics: Cell::default(),
            unit: Cell::default(),
        }
    }

    pub(crate) fn with_cancel(self, cancel: Option<&'a CancelToken>) -> Self {
        ParseCtx { ledger: Ledger::new(self.limits.max_work, cancel), ..self }
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

    /// Report one occurrence of `code` in `region`, in the current unit.
    pub(crate) fn emit(&self, code: DiagnosticCode, region: Region) {
        self.emit_count(code, region, 1);
    }

    /// Report `count` occurrences of `code` in `region`, in the current unit.
    pub(crate) fn emit_count(&self, code: DiagnosticCode, region: Region, count: u64) {
        let mut diagnostics = self.diagnostics.take();
        diagnostics.add(code, region, self.unit.get(), count);
        self.diagnostics.set(diagnostics);
    }

    /// Report in the unit at 0-based `position` (a slide, sheet or chapter)
    /// until the scope drops.
    pub(crate) fn enter_unit(&self, position: usize) -> UnitScope<'_> {
        let unit = position.checked_add(1).and_then(|n| u32::try_from(n).ok());
        self.scope(unit.and_then(NonZeroU32::new))
    }

    /// Report across the document until the scope drops: for a part every
    /// unit shares.
    pub(crate) fn outside_units(&self) -> UnitScope<'_> {
        self.scope(None)
    }

    fn scope(&self, unit: Option<NonZeroU32>) -> UnitScope<'_> {
        UnitScope { unit: &self.unit, replaced: self.unit.replace(unit) }
    }

    pub(crate) fn into_diagnostics(self) -> Diagnostics {
        self.diagnostics.into_inner()
    }

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
        ParseCtx::new(&Limits::DEFAULT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::Limit;

    #[test]
    fn charges_run_against_the_contexts_max_work() {
        let limits = Limits { max_work: 3, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        assert!(parse_ctx.charge(3).is_ok());
        let err = parse_ctx.charge(1).unwrap_err();
        assert!(matches!(err, ConvertError::ResourceLimit { limit: Limit::Work, max: 3 }));
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
        let code = DiagnosticCode::RecoveredXml;
        {
            let _slide = parse_ctx.enter_unit(1);
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
    fn a_cancelled_token_stops_the_next_charge() {
        let token = CancelToken::new();
        let parse_ctx = ParseCtx::default().with_cancel(Some(&token));
        assert!(parse_ctx.charge(1).is_ok());
        token.cancel();
        assert!(matches!(parse_ctx.charge(1), Err(ConvertError::Cancelled)));
        assert!(ParseCtx::default().with_cancel(None).charge(1).is_ok());
    }
}
