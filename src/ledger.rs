//! The work ledger of one conversion: deterministic fuel against
//! [`Limits::max_work`](crate::Limits::max_work), and the caller's
//! [`CancelToken`], checked at every charge.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
        Ledger { spent: Cell::new(0), max, cancel }
    }

    /// This ledger with `spent` already charged, as rendering continues the
    /// parse's work.
    pub(crate) fn starting_at(self, spent: u64) -> Self {
        Ledger { spent: Cell::new(spent), ..self }
    }

    pub(crate) fn charge(&self, units: u64) -> Result<(), ConvertError> {
        let spent = self.spent.get().saturating_add(units);
        self.spent.set(spent);
        if self.cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(ConvertError::Cancelled);
        }
        if spent > self.max {
            return Err(ConvertError::limit(Limit::Work, self.max));
        }
        Ok(())
    }

    pub(crate) fn spent(&self) -> u64 {
        self.spent.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_add_up_to_the_maximum_and_fail_past_it() {
        let ledger = Ledger::new(10, None);
        assert!(ledger.charge(4).is_ok());
        assert!(ledger.charge(6).is_ok());
        assert_eq!(ledger.spent(), 10);
        let err = ledger.charge(1).unwrap_err();
        assert!(matches!(err, ConvertError::ResourceLimit { limit: Limit::Work, max: 10 }));
        assert_eq!(ledger.spent(), 11);
    }

    #[test]
    fn fuel_saturates_instead_of_wrapping() {
        let ledger = Ledger::new(u64::MAX, None);
        assert!(ledger.charge(u64::MAX - 1).is_ok());
        assert!(ledger.charge(5).is_ok());
        assert_eq!(ledger.spent(), u64::MAX);
        assert!(ledger.charge(u64::MAX).is_ok());
        assert_eq!(ledger.spent(), u64::MAX);
    }

    #[test]
    fn every_charge_checks_the_token() {
        let token = CancelToken::new();
        let ledger = Ledger::new(u64::MAX, Some(&token));
        assert!(ledger.charge(0).is_ok());
        token.clone().cancel();
        assert!(token.is_cancelled());
        assert!(matches!(ledger.charge(0), Err(ConvertError::Cancelled)));
        assert!(matches!(ledger.charge(1), Err(ConvertError::Cancelled)));
    }

    #[test]
    fn cancellation_wins_over_an_exhausted_budget() {
        let token = CancelToken::new();
        token.cancel();
        let ledger = Ledger::new(1, Some(&token));
        assert!(matches!(ledger.charge(2), Err(ConvertError::Cancelled)));
    }

    #[test]
    fn a_ledger_started_at_spent_fuel_fails_past_the_same_maximum() {
        let ledger = Ledger::new(10, None).starting_at(9);
        assert!(ledger.charge(1).is_ok());
        assert!(matches!(
            ledger.charge(1),
            Err(ConvertError::ResourceLimit { limit: Limit::Work, max: 10 })
        ));
        assert_eq!(ledger.spent(), 11);
    }

    #[test]
    fn a_new_token_is_not_cancelled() {
        assert!(!CancelToken::new().is_cancelled());
        assert!(!CancelToken::default().is_cancelled());
    }
}
