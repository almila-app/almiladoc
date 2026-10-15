//! The context every frontend parses under: the one path from a call's
//! [`Config`](crate::Config) to the code that reads the input.

use crate::error::ConvertError;
use crate::ledger::{CancelToken, Ledger};
use crate::limits::Limits;

/// What one parse reads its configuration from, and charges its work to.
#[derive(Debug)]
pub(crate) struct ParseCtx<'a> {
    limits: &'a Limits,
    ledger: Ledger<'a>,
}

impl<'a> ParseCtx<'a> {
    pub(crate) fn new(limits: &'a Limits) -> Self {
        ParseCtx { limits, ledger: Ledger::new(limits.max_work, None) }
    }

    pub(crate) fn with_cancel(self, cancel: Option<&'a CancelToken>) -> Self {
        ParseCtx { ledger: Ledger::new(self.limits.max_work, cancel), ..self }
    }

    pub(crate) fn limits(&self) -> &'a Limits {
        self.limits
    }

    /// Charge `units` of work, failing once the parse is cancelled or past
    /// [`Limits::max_work`].
    pub(crate) fn charge(&self, units: u64) -> Result<(), ConvertError> {
        self.ledger.charge(units)
    }

    #[cfg(test)]
    pub(crate) fn fuel(&self) -> u64 {
        self.ledger.spent()
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
    fn a_cancelled_token_stops_the_next_charge() {
        let token = CancelToken::new();
        let parse_ctx = ParseCtx::default().with_cancel(Some(&token));
        assert!(parse_ctx.charge(1).is_ok());
        token.cancel();
        assert!(matches!(parse_ctx.charge(1), Err(ConvertError::Cancelled)));
        assert!(ParseCtx::default().with_cancel(None).charge(1).is_ok());
    }
}
