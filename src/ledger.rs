//! The work ledger of one conversion: deterministic fuel against
//! [`Limits::max_work`](crate::Limits::max_work), and the caller's
//! [`CancelToken`], checked at every charge.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::error::ConvertError;
use crate::limits::Limit;

/// A flag a caller sets from any thread to stop the conversions it was given
/// to; clones share it.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token not yet cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel every conversion holding this token: each fails
    /// [`ConvertError::Cancelled`] at its next unit of work.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// True once [`CancelToken::cancel`] was called on this token or a clone.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Fuel spent by one conversion, saturating, and the token that may cancel it.
#[derive(Debug)]
pub(crate) struct Ledger<'a> {
    spent: Cell<u64>,
    max: u64,
    cancel: Option<&'a CancelToken>,
}

impl<'a> Ledger<'a> {
    pub(crate) fn new(max: u64, cancel: Option<&'a CancelToken>) -> Self {
        Self { spent: Cell::new(0), max, cancel }
    }

    /// This ledger with `spent` already charged, as rendering continues the
    /// parse's work.
    pub(crate) fn starting_at(self, spent: u64) -> Self {
        Self { spent: Cell::new(spent), ..self }
    }

    pub(crate) fn charge(&self, units: u64) -> Result<(), ConvertError> {
        let spent = self.spent.get().saturating_add(units);
        self.spent.set(spent);
        judge(spent, self.max, self.cancel)
    }

    pub(crate) fn share(&self) -> SharedLedger<'a> {
        SharedLedger { spent: AtomicU64::new(self.spent.get()), max: self.max, cancel: self.cancel }
    }

    pub(crate) fn settle(&self, shared: SharedLedger<'_>) {
        self.spent.set(shared.spent.into_inner());
    }

    pub(crate) fn spent(&self) -> u64 {
        self.spent.get()
    }
}

#[derive(Debug)]
pub(crate) struct SharedLedger<'a> {
    spent: AtomicU64,
    max: u64,
    cancel: Option<&'a CancelToken>,
}

impl SharedLedger<'_> {
    #[cfg_attr(
        not(any(feature = "raster", test)),
        expect(dead_code, reason = "only painting charges a shared ledger")
    )]
    pub(crate) fn charge(&self, units: u64) -> Result<(), ConvertError> {
        let add = |spent: u64| spent.saturating_add(units);
        let before = self.spent.update(Ordering::Relaxed, Ordering::Relaxed, add);
        judge(before.saturating_add(units), self.max, self.cancel)
    }
}

fn judge(spent: u64, max: u64, cancel: Option<&CancelToken>) -> Result<(), ConvertError> {
    if cancel.is_some_and(CancelToken::is_cancelled) {
        return Err(ConvertError::Cancelled);
    }
    if spent > max {
        return Err(ConvertError::limit(Limit::Work, max));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn charges_add_up_to_the_maximum_and_fail_past_it() {
        let ledger = Ledger::new(10, None);
        ledger.charge(4).unwrap();
        ledger.charge(6).unwrap();
        assert_eq!(ledger.spent(), 10);
        let err = ledger.charge(1).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Work, max: 10 });
        assert_eq!(ledger.spent(), 11);
    }

    #[test]
    fn fuel_saturates_instead_of_wrapping() {
        let ledger = Ledger::new(u64::MAX, None);
        ledger.charge(u64::MAX - 1).unwrap();
        ledger.charge(5).unwrap();
        assert_eq!(ledger.spent(), u64::MAX);
        ledger.charge(u64::MAX).unwrap();
        assert_eq!(ledger.spent(), u64::MAX);
    }

    #[test]
    fn every_charge_checks_the_token() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        assert!(!CancelToken::default().is_cancelled());
        let ledger = Ledger::new(u64::MAX, Some(&token));
        ledger.charge(0).unwrap();
        token.clone().cancel();
        assert!(token.is_cancelled());
        assert_matches!(ledger.charge(0), Err(ConvertError::Cancelled));
        assert_matches!(ledger.charge(1), Err(ConvertError::Cancelled));
    }

    #[test]
    fn cancellation_wins_over_an_exhausted_budget() {
        let token = CancelToken::new();
        token.cancel();
        let ledger = Ledger::new(1, Some(&token));
        assert_matches!(ledger.charge(2), Err(ConvertError::Cancelled));
    }

    #[test]
    fn a_shared_ledger_charges_on_from_its_ledger_and_settles_back() {
        let token = CancelToken::new();
        let ledger = Ledger::new(100, Some(&token)).starting_at(40);
        let shared = ledger.share();
        let config = crate::Config { threads: 4, ..crate::Config::default() };
        crate::pool::run_on_pool(&config, || {
            crate::pool::map_ordered(&[(); 4], |()| (0..5).try_for_each(|_| shared.charge(2)));
            Ok(())
        })
        .unwrap();
        assert_matches!(
            shared.charge(21),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 100 })
        );
        token.cancel();
        assert_matches!(shared.charge(0), Err(ConvertError::Cancelled));
        ledger.settle(shared);
        assert_eq!(ledger.spent(), 40 + 4 * 5 * 2 + 21);
        let full = Ledger::new(u64::MAX, None).starting_at(u64::MAX - 1).share();
        full.charge(5).unwrap();
        assert_eq!(full.spent.into_inner(), u64::MAX);
    }

    #[test]
    fn a_ledger_started_at_spent_fuel_fails_past_the_same_maximum() {
        let ledger = Ledger::new(10, None).starting_at(9);
        ledger.charge(1).unwrap();
        assert_matches!(
            ledger.charge(1),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 10 })
        );
        assert_eq!(ledger.spent(), 11);
    }
}
