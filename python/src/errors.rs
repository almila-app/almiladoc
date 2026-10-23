//! The exceptions the binding raises, and the one mapping from each failure
//! onto them.

use almiladoc::{ConvertError, Limit, UnsupportedKind};
use pyo3::prelude::*;

use crate::diagnostics::diagnostic_tuple;

use exceptions::{
    CancelledError, ConfigError, EncryptedError, IncompleteError, InternalError, MalformedError,
    MissingPartError, NeedsOcrError, ResourceLimitError, UnsupportedError,
};

#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "create_exception! keeps each exception type in a PyOnceLock, which only this module holds"
)]
pub(crate) mod exceptions {
    use pyo3::create_exception;
    use pyo3::exceptions::{PyException, PyValueError};

    create_exception!(
        almiladoc,
        ConfigError,
        PyValueError,
        "A `Limits` or `Config` field is out of range. `limit` names the field \
         (`threads` for the thread count); `ceiling` is the highest value a \
         limit with one accepts and `max` the most threads, each `None` where \
         it does not apply."
    );

    create_exception!(
        almiladoc,
        ConvertError,
        PyException,
        "A complete conversion was impossible. `code` names the failure, one \
         snake_case name per subclass. Catch this to handle every kind of \
         failure, or one of the subclasses below to single one out. A file \
         `Converter.convert_path` cannot read or refuses unopened raises `OSError` \
         instead."
    );

    create_exception!(
        almiladoc,
        UnsupportedError,
        ConvertError,
        "The format is unknown, or cannot be converted at all. `kind` says which: \
         `unknown_format`, `feature_disabled`, `format_mismatch` or \
         `pdf_document_model`. `detected` names the format the content was \
         recognized as when it is not the one named, to retry with, and is `None` \
         otherwise."
    );

    create_exception!(
        almiladoc,
        NeedsOcrError,
        ConvertError,
        "Pages of a PDF are scanned or image-only and need OCR, which almiladoc does \
         not do. `pages` lists them (1-indexed) and `page_count` is the length of \
         the document."
    );

    create_exception!(
        almiladoc,
        MalformedError,
        ConvertError,
        "The document is structurally unusable: no meaningful content could be \
         extracted. `part` names the package part or stream at fault, and is \
         `None` when no single part is."
    );

    create_exception!(
        almiladoc,
        EncryptedError,
        ConvertError,
        "The document is encrypted or password-protected."
    );

    create_exception!(
        almiladoc,
        ResourceLimitError,
        ConvertError,
        "A safety limit was crossed - input size, decompression, nesting depth, \
         node count, repeat expansion, work or retained asset bytes - or the \
         allocator refused a reservation. `limit` names it and `max` is its value; \
         for `allocation`, the size of the reservation or the bound of the read the \
         allocator refused, or 0 where the PDF engine does not say."
    );

    create_exception!(
        almiladoc,
        MissingPartError,
        ConvertError,
        "A part required for any meaningful output is absent. `part` names it."
    );

    create_exception!(
        almiladoc,
        IncompleteError,
        ConvertError,
        "Content the source document shows could not be converted, so no output \
         is returned that would read as complete. `diagnostics` is a tuple of every \
         `Diagnostic` of the conversion."
    );

    create_exception!(
        almiladoc,
        CancelledError,
        ConvertError,
        "The call's `CancelToken` was cancelled."
    );

    create_exception!(
        almiladoc,
        InternalError,
        ConvertError,
        "almiladoc failed in a way no input should cause; recycle the process \
         that raised it."
    );
}

/// Why a call failed, gathered while detached and raised once attached.
pub(crate) enum Failure {
    Convert(ConvertError),
    /// A limit the binding enforces itself, before the core sees the input.
    Limit(Limit, u64),
    Io(std::io::Error),
}

impl From<ConvertError> for Failure {
    fn from(error: ConvertError) -> Self {
        Self::Convert(error)
    }
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl Failure {
    pub(crate) fn raise(self, py: Python<'_>) -> PyErr {
        match self {
            Self::Convert(error) => convert_error(py, &error),
            Self::Limit(limit, max) => resource_limit(py, limit, max, limit_message(limit, max)),
            Self::Io(error) => error.into(),
        }
    }
}

/// The message the core writes for `limit` at `max`, whose
/// `ConvertError::ResourceLimit` cannot be built outside it.
fn limit_message(limit: Limit, max: u64) -> String {
    match limit {
        Limit::Allocation => {
            format!("resource limit exceeded: allocation refused reading up to {max} bytes")
        }
        _ => format!("resource limit exceeded: {limit} is {max}"),
    }
}

/// The `ResourceLimitError` for `limit` at `max`, whether the core or the
/// binding refused.
fn resource_limit(py: Python<'_>, limit: Limit, max: u64, message: String) -> PyErr {
    let raised = ResourceLimitError::new_err(message);
    let value = raised.value(py);
    let detail = value
        .setattr("code", "resource_limit")
        .and_then(|()| value.setattr("limit", limit.as_str()))
        .and_then(|()| value.setattr("max", max));
    detail.err().unwrap_or(raised)
}

pub(crate) fn convert_error(py: Python<'_>, error: &ConvertError) -> PyErr {
    raise(py, error, error.to_string())
}

/// Raise the subclass that names the failure with `message`, carrying its
/// `code`, and the kind, part or limit at fault where the variant knows one;
/// a variant newer than this binding raises as `Internal`, class and code alike.
fn raise(py: Python<'_>, error: &ConvertError, message: String) -> PyErr {
    let raised = match error {
        ConvertError::ResourceLimit { limit, max, .. } => {
            return resource_limit(py, *limit, *max, message);
        }
        ConvertError::Unsupported { .. } => UnsupportedError::new_err(message),
        ConvertError::NeedsOcr { .. } => NeedsOcrError::new_err(message),
        ConvertError::Malformed { .. } => MalformedError::new_err(message),
        ConvertError::Encrypted => EncryptedError::new_err(message),
        ConvertError::MissingPart { .. } => MissingPartError::new_err(message),
        ConvertError::Incomplete { .. } => IncompleteError::new_err(message),
        ConvertError::Cancelled => CancelledError::new_err(message),
        ConvertError::Internal => InternalError::new_err(message),
        _ => return raise(py, &ConvertError::Internal, message),
    };
    let value = raised.value(py);
    let detail = value.setattr("code", error.code()).and_then(|()| match error {
        ConvertError::Unsupported { kind, .. } => value
            .setattr("kind", kind.as_str())
            .and_then(|()| value.setattr("detected", mismatched(*kind))),
        ConvertError::NeedsOcr { pages, page_count, .. } => value
            .setattr("pages", pages.clone())
            .and_then(|()| value.setattr("page_count", *page_count)),
        ConvertError::Malformed { part, .. } => {
            value.setattr("part", part.as_ref().map(almiladoc::PartName::as_str))
        }
        ConvertError::MissingPart { part, .. } => value.setattr("part", part.as_str()),
        ConvertError::Incomplete { diagnostics, .. } => {
            diagnostic_tuple(py, diagnostics).and_then(|tuple| value.setattr("diagnostics", tuple))
        }
        _ => Ok(()),
    });
    detail.err().unwrap_or(raised)
}

fn mismatched(kind: UnsupportedKind) -> Option<&'static str> {
    match kind {
        UnsupportedKind::FormatMismatch { detected, .. } => Some(detected.as_str()),
        _ => None,
    }
}

/// The `ConfigError` naming the field `error` refuses; a variant newer than
/// this binding raises as `Internal`.
pub(crate) fn config_error(py: Python<'_>, error: &almiladoc::ConfigError) -> PyErr {
    let (limit, ceiling, max) = match error {
        almiladoc::ConfigError::ZeroLimit { limit, .. } => (limit.as_str(), None, None),
        almiladoc::ConfigError::AboveCeiling { limit, ceiling, .. } => {
            (limit.as_str(), Some(*ceiling), None)
        }
        almiladoc::ConfigError::Threads { max, .. } => ("threads", None, Some(*max)),
        _ => return raise(py, &ConvertError::Internal, error.to_string()),
    };
    let raised = ConfigError::new_err(error.to_string());
    let value = raised.value(py);
    let detail = value
        .setattr("limit", limit)
        .and_then(|()| value.setattr("ceiling", ceiling))
        .and_then(|()| value.setattr("max", max));
    detail.err().unwrap_or(raised)
}
