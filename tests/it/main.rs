//! Integration tests: the public API over the fixture corpus, and the crate's own architecture.
#![expect(
    clippy::cast_possible_truncation,
    clippy::disallowed_methods,
    clippy::format_collect,
    clippy::format_push_string,
    clippy::panic,
    clippy::unwrap_used,
    reason = "test helpers read and write fixtures, build them with format!, take known small sizes and fail by panicking"
)]

mod architecture;
mod bounds;
mod build;
mod container;
mod converter;
mod diagnostics;
mod emphasis;
mod fixes;
mod image_heading;
mod image_ooxml;
mod image_ppt;
mod image_regressions;
#[cfg(feature = "serde")]
mod json;
mod limits;
#[cfg(feature = "pdf")]
mod ocr;
mod ocr_images;
#[cfg(feature = "pdf")]
mod pdf;
mod robustness;
mod sheet_bounds;
mod snapshots;
mod spreadsheets;
mod style;
mod support;
mod units;
#[path = "../common/word97.rs"]
mod word97;
