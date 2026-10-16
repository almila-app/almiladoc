//! Python bindings for almiladoc.

use std::path::{Path, PathBuf};

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
    "The format is unknown, or cannot be converted at all. `detected` names the \
     format the content was recognized as when it is not the one named, to \
     retry with, and is `None` otherwise."
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
    "A safety limit was crossed - decompression, nesting depth, node count, \
     repeat expansion or retained asset bytes - or the allocator refused a \
     reservation. `limit` names it and `max` is its value; for `allocation`, \
     the size of the reservation or the bound of the read the allocator \
     refused, or 0 where the PDF engine does not say."
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
     is returned that would read as complete. `diagnostics` lists every \
     diagnostic as a `(code, class, region, unit, count)` tuple, `unit` being \
     a 1-based page, slide, sheet or chapter or `None`."
);

create_exception!(almiladoc, CancelledError, ConvertError, "The conversion was cancelled.");

create_exception!(
    almiladoc,
    InternalError,
    ConvertError,
    "almiladoc failed in a way no input should cause; recycle the process \
     that raised it."
);

/// Every format, named in Python by [`almiladoc::Format::as_str`]. Container
/// variants that share a parser (`.docm`, `.xlsm`, `.ppsx`, ...) map onto
/// these via `format_from_bytes` or `format_from_extension`.
const FORMATS: [almiladoc::Format; 12] = [
    almiladoc::Format::Doc,
    almiladoc::Format::Docx,
    almiladoc::Format::Odt,
    almiladoc::Format::Pdf,
    almiladoc::Format::Ppt,
    almiladoc::Format::Pptx,
    almiladoc::Format::Rtf,
    almiladoc::Format::Epub,
    almiladoc::Format::Excel,
    almiladoc::Format::Ods,
    almiladoc::Format::Odp,
    almiladoc::Format::Csv,
];

fn parse_format(name: &str) -> PyResult<almiladoc::Format> {
    FORMATS.into_iter().find(|format| format.as_str() == name).ok_or_else(|| {
        let names: Vec<&str> = FORMATS.map(almiladoc::Format::as_str).to_vec();
        PyValueError::new_err(format!(
            "unknown format {name:?}; expected one of {}",
            names.join(", ")
        ))
    })
}

/// Raise the subclass that names the failure, carrying the part or limit at
/// fault where the variant knows one.
fn convert_error(py: Python<'_>, error: almiladoc::ConvertError) -> PyErr {
    let message = error.to_string();
    let raised = match &error {
        almiladoc::ConvertError::Unsupported { .. } => UnsupportedError::new_err(message),
        almiladoc::ConvertError::NeedsOcr { .. } => NeedsOcrError::new_err(message),
        almiladoc::ConvertError::Malformed { .. } => MalformedError::new_err(message),
        almiladoc::ConvertError::Encrypted => EncryptedError::new_err(message),
        almiladoc::ConvertError::ResourceLimit { .. } => ResourceLimitError::new_err(message),
        almiladoc::ConvertError::MissingPart { .. } => MissingPartError::new_err(message),
        almiladoc::ConvertError::Incomplete { .. } => IncompleteError::new_err(message),
        almiladoc::ConvertError::Cancelled => CancelledError::new_err(message),
        _ => InternalError::new_err(message),
    };
    let value = raised.value(py);
    let detail = match &error {
        almiladoc::ConvertError::Unsupported { kind, .. } => {
            value.setattr("detected", mismatched(*kind))
        }
        almiladoc::ConvertError::NeedsOcr { pages, page_count, .. } => value
            .setattr("pages", pages.clone())
            .and_then(|()| value.setattr("page_count", *page_count)),
        almiladoc::ConvertError::Malformed { part, .. } => {
            value.setattr("part", part.as_ref().map(almiladoc::PartName::as_str))
        }
        almiladoc::ConvertError::ResourceLimit { limit, max, .. } => {
            value.setattr("limit", limit.as_str()).and_then(|()| value.setattr("max", *max))
        }
        almiladoc::ConvertError::MissingPart { part, .. } => value.setattr("part", part.as_str()),
        almiladoc::ConvertError::Incomplete { diagnostics, .. } => {
            value.setattr("diagnostics", diagnostic_tuples(diagnostics))
        }
        _ => Ok(()),
    };
    detail.err().unwrap_or(raised)
}

fn mismatched(kind: almiladoc::UnsupportedKind) -> Option<&'static str> {
    match kind {
        almiladoc::UnsupportedKind::FormatMismatch { detected, .. } => Some(detected.as_str()),
        _ => None,
    }
}

type DiagnosticTuple = (&'static str, &'static str, &'static str, Option<u32>, u64);

fn diagnostic_tuples(diagnostics: &almiladoc::Diagnostics) -> Vec<DiagnosticTuple> {
    diagnostics
        .iter()
        .map(|d| {
            let unit = d.unit.map(std::num::NonZeroU32::get);
            (d.code.as_str(), d.class.as_str(), d.region.as_str(), unit, d.count)
        })
        .collect()
}

fn extension_format(path: &Path) -> Option<almiladoc::Format> {
    path.extension().and_then(|ext| ext.to_str()).and_then(almiladoc::Format::from_extension)
}

fn unknown_format(py: Python<'_>) -> PyErr {
    let kind = almiladoc::UnsupportedKind::UnknownFormat;
    let raised = UnsupportedError::new_err(format!("unsupported input: {kind}"));
    raised.value(py).setattr("detected", mismatched(kind)).err().unwrap_or(raised)
}

fn converter() -> PyResult<almiladoc::Converter> {
    almiladoc::Converter::new(almiladoc::Config::default())
        .map_err(|e| InternalError::new_err(e.to_string()))
}

/// Detect the format from the content itself: the signature and identity each
/// container specification designates (PDF header, RTF open group, OLE stream
/// names, ZIP package mimetype/content types). Plain-text formats (CSV) carry
/// no signature and return `None`; so does anything unrecognized.
#[pyfunction]
fn format_from_bytes(data: Vec<u8>) -> Option<&'static str> {
    almiladoc::Format::sniff(&data).map(almiladoc::Format::as_str)
}

/// The format an extension names, with or without a leading dot.
#[pyfunction]
fn format_from_extension(extension: &str) -> Option<&'static str> {
    almiladoc::Format::from_extension(extension.trim_start_matches('.'))
        .map(almiladoc::Format::as_str)
}

/// The format a path's extension names.
#[pyfunction]
fn format_from_path(path: PathBuf) -> Option<&'static str> {
    extension_format(&path).map(almiladoc::Format::as_str)
}

/// Convert a document file to Markdown. The format is detected from the file
/// content; the extension is the fallback for signature-less formats (CSV)
/// and unrecognizable containers.
#[pyfunction]
fn to_markdown(py: Python<'_>, path: PathBuf) -> PyResult<String> {
    let converter = converter()?;
    let converted = py.detach(|| {
        let data = std::fs::read(&path)?;
        let format = almiladoc::Format::sniff(&data).or_else(|| extension_format(&path));
        Ok::<_, std::io::Error>(format.map(|format| {
            converter.convert(almiladoc::Input::new(&data, format)).map(|c| c.into_markdown())
        }))
    })?;
    converted.ok_or_else(|| unknown_format(py))?.map_err(|e| convert_error(py, e))
}

/// Convert an in-memory document to Markdown. Without a format, it is
/// detected from the content, which signature-less formats (CSV) have to name
/// explicitly.
#[pyfunction]
#[pyo3(signature = (data, format=None))]
fn to_markdown_bytes(py: Python<'_>, data: Vec<u8>, format: Option<&str>) -> PyResult<String> {
    let converter = converter()?;
    let format = resolve_format(py, &data, format)?;
    py.detach(|| converter.convert(almiladoc::Input::new(&data, format)))
        .map(almiladoc::Conversion::into_markdown)
        .map_err(|e| convert_error(py, e))
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
    let converter = converter()?;
    let format = resolve_format(py, &data, format)?;
    let parsed = py
        .detach(|| converter.parse(almiladoc::Input::new(&data, format)))
        .map_err(|e| convert_error(py, e))?;
    document::document(py, parsed.into_document())
}

fn resolve_format(
    py: Python<'_>,
    data: &[u8],
    format: Option<&str>,
) -> PyResult<almiladoc::Format> {
    match format {
        Some(name) => parse_format(name),
        None => almiladoc::Format::sniff(data).ok_or_else(|| unknown_format(py)),
    }
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
    m.add("CancelledError", m.py().get_type::<CancelledError>())?;
    m.add("ConvertError", m.py().get_type::<ConvertError>())?;
    m.add("EncryptedError", m.py().get_type::<EncryptedError>())?;
    m.add("IncompleteError", m.py().get_type::<IncompleteError>())?;
    m.add("InternalError", m.py().get_type::<InternalError>())?;
    m.add("MalformedError", m.py().get_type::<MalformedError>())?;
    m.add("MissingPartError", m.py().get_type::<MissingPartError>())?;
    m.add("NeedsOcrError", m.py().get_type::<NeedsOcrError>())?;
    m.add("ResourceLimitError", m.py().get_type::<ResourceLimitError>())?;
    m.add("UnsupportedError", m.py().get_type::<UnsupportedError>())?;
    Ok(())
}
