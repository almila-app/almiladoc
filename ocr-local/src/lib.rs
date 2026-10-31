//! Offline PP-OCR inference with caller-verified files and ungrouped image-space text regions.

mod artifact;
mod backend;
mod config;
mod decode;
mod error;
mod inference;
mod output;
mod runtime;
mod stages;

pub use artifact::{FileRole, FileSpec, ModelFiles};
pub use backend::OfflineOcr;
pub use config::{Config, Limits};
pub use error::Error;
pub use output::{Line, Page, Rectangle};
pub use runtime::Runtime;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod stage_tests;

pub(crate) fn checkpoint(cancelled: &(impl Fn() -> bool + Sync)) -> Result<(), Error> {
    if cancelled() { Err(Error::Cancelled) } else { Ok(()) }
}

#[cfg(test)]
#[path = "../tests/support/logging.rs"]
mod test_logging;
