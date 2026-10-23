//! Input admission: a cancelled token and `max_input_bytes` refuse an input
//! before any byte of it is copied or read.

use std::io::{ErrorKind, Read};
use std::path::Path;

use almiladoc::{CancelToken, ConvertError, Limit};
use pyo3::buffer::PyUntypedBuffer;
use pyo3::exceptions::{PyMemoryError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyMemoryView};

use crate::errors::Failure;

/// What an input must meet before the binding copies or reads it, in the
/// core's order: the token first, then the size.
pub(crate) struct Admission<'t> {
    pub(crate) max_input_bytes: u64,
    pub(crate) cancel: Option<&'t CancelToken>,
}

impl Admission<'_> {
    pub(crate) fn check(&self, len: u64) -> Result<(), Failure> {
        if self.cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(ConvertError::Cancelled.into());
        }
        if len > self.max_input_bytes {
            return Err(Failure::Limit(Limit::InputBytes, self.max_input_bytes));
        }
        Ok(())
    }
}

/// `data` itself when it is `bytes`, which its caller admits before reading
/// it in place; else one copy of the contiguous byte buffer it exports,
/// taken through a `memoryview` that pins the export whose length was
/// admitted, so a `bytearray` cannot grow before the copy.
pub(crate) fn buffer<'py>(
    py: Python<'py>,
    data: &Bound<'py, PyAny>,
    admission: &Admission<'_>,
) -> PyResult<Bound<'py, PyBytes>> {
    if let Ok(bytes) = data.cast::<PyBytes>() {
        return Ok(bytes.clone());
    }
    let view = PyMemoryView::from(data)?;
    let buffer = PyUntypedBuffer::get(view.as_any())?
        .into_typed::<u8>()
        .map_err(|_| PyTypeError::new_err("expected a buffer of bytes"))?;
    if !buffer.is_c_contiguous() {
        return Err(PyTypeError::new_err("expected a contiguous buffer"));
    }
    let len = buffer.item_count() as u64;
    admission.check(len).map_err(|f| f.raise(py))?;
    let copied = view.call_method0("tobytes").map_err(|e| {
        if e.is_instance_of::<PyMemoryError>(py) {
            Failure::Limit(Limit::Allocation, len).raise(py)
        } else {
            e
        }
    })?;
    Ok(copied.cast_into::<PyBytes>()?)
}

/// The bytes of the regular file at `path`: its type and size are checked
/// before it is opened, since opening a FIFO blocks, and again on the open
/// file; the read stops one byte past `max_input_bytes`, so the core's own
/// admission refuses a file that grew.
#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "the binding's path entry point reads the file its caller names"
)]
pub(crate) fn read_file(path: &Path, admission: &Admission<'_>) -> Result<Vec<u8>, Failure> {
    regular_size(&std::fs::metadata(path)?, admission)?;
    let file = std::fs::File::open(path)?;
    let size = regular_size(&file.metadata()?, admission)?;
    let mut bytes = Vec::new();
    usize::try_from(size)
        .ok()
        .and_then(|n| bytes.try_reserve_exact(n).ok())
        .ok_or(Failure::Limit(Limit::Allocation, size))?;
    let bound = admission.max_input_bytes.saturating_add(1);
    file.take(bound).read_to_end(&mut bytes).map_err(|e| match e.kind() {
        ErrorKind::OutOfMemory => Failure::Limit(Limit::Allocation, bound),
        _ => Failure::Io(e),
    })?;
    Ok(bytes)
}

#[expect(clippy::disallowed_types, reason = "reads the metadata of the file read_file reads")]
fn regular_size(metadata: &std::fs::Metadata, admission: &Admission<'_>) -> Result<u64, Failure> {
    if !metadata.is_file() {
        return Err(std::io::Error::new(ErrorKind::InvalidInput, "not a regular file").into());
    }
    admission.check(metadata.len())?;
    Ok(metadata.len())
}
