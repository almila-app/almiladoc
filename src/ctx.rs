//! The context every frontend parses under: the one path from a call's
//! [`Config`](crate::Config) to the code that reads the input.

use crate::limits::Limits;
use std::cell::Cell;
use std::marker::PhantomData;

/// What one parse reads its configuration from.
#[derive(Debug)]
pub(crate) struct ParseCtx<'a> {
    limits: &'a Limits,
    single_thread: PhantomData<Cell<()>>,
}

impl<'a> ParseCtx<'a> {
    pub(crate) fn new(limits: &'a Limits) -> Self {
        ParseCtx { limits, single_thread: PhantomData }
    }

    pub(crate) fn limits(&self) -> &'a Limits {
        self.limits
    }
}

#[cfg(test)]
impl Default for ParseCtx<'static> {
    fn default() -> Self {
        ParseCtx::new(&Limits::DEFAULT)
    }
}
