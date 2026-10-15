//! Python bindings for almiladoc.

use std::path::PathBuf;

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;

mod document;

create_exception!(
    almiladoc,
    ConvertError,
    PyException,
    "A complete conversion was impossible. Catch this to handle every kind of \
     failure, or one of the subclasses below to single one out. An unreadable \
     file raises `OSError` instead."
);

create_exception!(
    almiladoc,
    UnsupportedError,
    ConvertError,
    "The format is unknown, or cannot be converted at all."
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
    "A fixed safety limit was crossed: decompression, nesting depth, node \
     count, repeat expansion, or retained asset bytes. `limit` names it."
);

create_exception!(
    almiladoc,
    MissingPartError,
    ConvertError,
    "A part required for any meaningful output is absent. `part` names it."
);

/// Format names, as the extension that identifies each format. Container
/// variants that share a parser (`.docm`, `.xlsm`, `.ppsx`, ...) map onto
/// these via `format_from_bytes` or `format_from_extension`.
const FORMATS: [(&str, almiladoc::Format); 12] = [
    ("doc", almiladoc::Format::Doc),
    ("docx", almiladoc::Format::Docx),
    ("odt", almiladoc::Format::Odt),
    ("pdf", almiladoc::Format::Pdf),
    ("ppt", almiladoc::Format::Ppt),
    ("pptx", almiladoc::Format::Pptx),
    ("rtf", almiladoc::Format::Rtf),
    ("epub", almiladoc::Format::Epub),
    ("xlsx", almiladoc::Format::Excel),
    ("ods", almiladoc::Format::Ods),
    ("odp", almiladoc::Format::Odp),
    ("csv", almiladoc::Format::Csv),
];

fn parse_format(name: &str) -> PyResult<almiladoc::Format> {
    FORMATS.iter().find(|(n, _)| *n == name).map(|(_, format)| *format).ok_or_else(|| {
        let names: Vec<&str> = FORMATS.iter().map(|(n, _)| *n).collect();
        PyValueError::new_err(format!(
            "unknown format {name:?}; expected one of {}",
            names.join(", ")
        ))
    })
}

fn format_name(format: almiladoc::Format) -> &'static str {
    FORMATS
        .iter()
        .find(|(_, f)| *f == format)
        .map(|(name, _)| *name)
        .expect("every format is named")
}

/// Raise the subclass that names the failure, carrying the part or limit at
/// fault where the variant knows one. An unreadable file raises the `OSError`
/// subclass any other read of it would.
fn convert_error(py: Python<'_>, error: almiladoc::ConvertError) -> PyErr {
    let error = match error {
        almiladoc::ConvertError::Io(e) => return e.into(),
        other => other,
    };
    let message = error.to_string();
    // A variant added later raises the base class until it is named here.
    let raised = match &error {
        almiladoc::ConvertError::Unsupported(_) => UnsupportedError::new_err(message),
        almiladoc::ConvertError::NeedsOcr { .. } => NeedsOcrError::new_err(message),
        almiladoc::ConvertError::Malformed { .. } => MalformedError::new_err(message),
        almiladoc::ConvertError::Encrypted => EncryptedError::new_err(message),
        almiladoc::ConvertError::ResourceLimit { .. } => ResourceLimitError::new_err(message),
        almiladoc::ConvertError::MissingPart { .. } => MissingPartError::new_err(message),
        _ => ConvertError::new_err(message),
    };
    let detail = match &error {
        almiladoc::ConvertError::NeedsOcr { pages, page_count } => raised
            .value(py)
            .setattr("pages", pages.clone())
            .and_then(|()| raised.value(py).setattr("page_count", *page_count)),
        almiladoc::ConvertError::Malformed { part, .. } => {
            raised.value(py).setattr("part", part.as_deref())
        }
        almiladoc::ConvertError::ResourceLimit { limit, .. } => {
            raised.value(py).setattr("limit", *limit)
        }
        almiladoc::ConvertError::MissingPart { part } => {
            raised.value(py).setattr("part", part.as_str())
        }
        _ => Ok(()),
    };
    detail.err().unwrap_or(raised)
}

/// Detect the format from the content itself: the signature and identity each
/// container specification designates (PDF header, RTF open group, OLE stream
/// names, ZIP package mimetype/content types). Plain-text formats (CSV) carry
/// no signature and return `None`; so does anything unrecognized.
#[pyfunction]
fn format_from_bytes(data: Vec<u8>) -> Option<&'static str> {
    almiladoc::Format::from_bytes(&data).map(format_name)
}

/// The format an extension names, with or without a leading dot.
#[pyfunction]
fn format_from_extension(extension: &str) -> Option<&'static str> {
    almiladoc::Format::from_extension(extension.trim_start_matches('.')).map(format_name)
}

/// The format a path's extension names.
#[pyfunction]
fn format_from_path(path: PathBuf) -> Option<&'static str> {
    almiladoc::Format::from_path(&path).map(format_name)
}

/// Convert a document file to Markdown. The format is detected from the file
/// content; the extension is the fallback for signature-less formats (CSV)
/// and unrecognizable containers.
#[pyfunction]
fn to_markdown(py: Python<'_>, path: PathBuf) -> PyResult<String> {
    py.detach(|| almiladoc::to_markdown(&path)).map_err(|e| convert_error(py, e))
}

/// Convert an in-memory document to Markdown. Without a format, it is
/// detected from the content, which signature-less formats (CSV) have to name
/// explicitly.
#[pyfunction]
#[pyo3(signature = (data, format=None))]
fn to_markdown_bytes(py: Python<'_>, data: Vec<u8>, format: Option<&str>) -> PyResult<String> {
    let format = format.map(parse_format).transpose()?;
    py.detach(|| almiladoc::to_markdown_bytes(&data, format)).map_err(|e| convert_error(py, e))
}

/// Parse an in-memory document into the document model, which also carries
/// the embedded assets. Without a format, it is detected from the content.
///
/// Unsupported for `pdf`: PDF conversion produces Markdown directly and has
/// no document-model form; use `to_markdown_bytes`.
#[pyfunction]
#[pyo3(signature = (data, format=None))]
fn to_document(
    py: Python<'_>,
    data: Vec<u8>,
    format: Option<&str>,
) -> PyResult<document::Document> {
    let format = format.map(parse_format).transpose()?;
    let parsed =
        py.detach(|| almiladoc::to_document(&data, format)).map_err(|e| convert_error(py, e))?;
    document::document(py, parsed)
}

/// Convert documents to GitHub-Flavored Markdown.
#[pymodule]
fn _almiladoc(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(format_from_bytes, m)?)?;
    m.add_function(wrap_pyfunction!(format_from_extension, m)?)?;
    m.add_function(wrap_pyfunction!(format_from_path, m)?)?;
    m.add_function(wrap_pyfunction!(to_markdown, m)?)?;
    m.add_function(wrap_pyfunction!(to_markdown_bytes, m)?)?;
    m.add_function(wrap_pyfunction!(to_document, m)?)?;
    m.add_class::<document::Asset>()?;
    m.add_class::<document::Block>()?;
    m.add_class::<document::Cell>()?;
    m.add_class::<document::CellSlot>()?;
    m.add_class::<document::Document>()?;
    m.add_class::<document::ImageSource>()?;
    m.add_class::<document::Inline>()?;
    m.add_class::<document::LinkTarget>()?;
    m.add_class::<document::List>()?;
    m.add_class::<document::ListItem>()?;
    m.add_class::<document::Note>()?;
    m.add_class::<document::Style>()?;
    m.add_class::<document::Table>()?;
    m.add("ConvertError", m.py().get_type::<ConvertError>())?;
    m.add("EncryptedError", m.py().get_type::<EncryptedError>())?;
    m.add("MalformedError", m.py().get_type::<MalformedError>())?;
    m.add("MissingPartError", m.py().get_type::<MissingPartError>())?;
    m.add("NeedsOcrError", m.py().get_type::<NeedsOcrError>())?;
    m.add("ResourceLimitError", m.py().get_type::<ResourceLimitError>())?;
    m.add("UnsupportedError", m.py().get_type::<UnsupportedError>())?;
    Ok(())
}
