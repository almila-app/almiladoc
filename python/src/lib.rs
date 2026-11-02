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
    use crate::config::{
        CommentPolicy, Config, HeaderFooterPolicy, HiddenPolicy, ImageOcr, ImagePolicy, Limits,
        OnLoss, RevisionPolicy,
    };
    #[pymodule_export]
    use crate::converter::{
        CancelToken, Conversion, Converter, Identity, Inspection, Parsed, sniff,
    };
    #[pymodule_export]
    use crate::diagnostics::Diagnostic;
    #[pymodule_export]
    use crate::errors::exceptions::{
        CancelledError, ConfigError, ConvertError, EncryptedError, GapRefused, IncompleteError,
        InternalError, MalformedError, MissingPartError, NeedsOcrError, PageOutOfRangeError,
        ResourceLimitError, UnsupportedError,
    };
    #[pymodule_export]
    use crate::ocr::{
        OcrAttempt, OcrFailure, OcrHttp, OcrLine, OcrRequest, OcrResult, OcrSummary, TokenUsage,
    };
}
