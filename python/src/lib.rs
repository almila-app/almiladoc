//! Python bindings for almiladoc.

mod config;
mod converter;
mod diagnostics;
mod errors;
mod input;
mod ocr;

use pyo3::prelude::*;

/// Convert documents to GitHub-Flavored Markdown.
#[pymodule]
mod _almiladoc {
    #[pymodule_export]
    use crate::config::{Config, HeaderFooterPolicy, ImageOcr, ImagePolicy, Limits, OnLoss};
    #[pymodule_export]
    use crate::converter::{
        CancelToken, Conversion, Converter, Identity, Inspection, Parsed, sniff,
    };
    #[pymodule_export]
    use crate::diagnostics::Diagnostic;
    #[pymodule_export]
    use crate::errors::exceptions::{
        CancelledError, ConfigError, ConvertError, EncryptedError, GapRefused, IncompleteError,
        InternalError, InvalidConfigError, MalformedError, MissingPartError, NeedsOcrError,
        ResourceLimitError, UnsupportedError,
    };
    #[pymodule_export]
    use crate::ocr::{OcrFailure, OcrHttp, OcrLine, OcrRequest, OcrSummary};
}
