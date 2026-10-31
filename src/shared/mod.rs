//! Helpers shared by more than one format reader.

pub(crate) mod assets;
#[deny(
    clippy::indexing_slicing,
    reason = "a binary record reader reads through checked views (ADR 0001 decision 16)"
)]
pub(crate) mod binary;
pub(crate) mod blockstyle;
pub(crate) mod chain;
pub(crate) mod codepage;
pub(crate) mod css;
pub(crate) mod delta;
pub(crate) mod dingbats;
pub(crate) mod drawingml;
pub(crate) mod fields;
pub(crate) mod grid;
pub(crate) mod header;
pub(crate) mod hidden_runs;
pub(crate) mod html;
pub(crate) mod link_label;
pub(crate) mod list;
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "std has no float-to-integer or u64-to-f64 conversion, so each named conversion is an `as` cast with its rounding and saturation documented and tested"
)]
pub(crate) mod lossy;
pub(crate) mod markdown;
pub(crate) mod math;
pub(crate) mod mc;
pub(crate) mod numbering;
pub(crate) mod ocr_table;
#[deny(
    clippy::indexing_slicing,
    reason = "a binary record reader reads through checked views (ADR 0001 decision 16)"
)]
pub(crate) mod officeart;
pub(crate) mod symbol;
pub(crate) mod tag_scan;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod text;
pub(crate) mod typeface;
pub(crate) mod uri;
