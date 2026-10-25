//! `OcrRequest`: a gap's image exported for an OCR run elsewhere.

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyTuple, PyType};

use crate::converter::GapRow;

/// One gap's image for an OCR run elsewhere, which pickles: `gap`, the row of
/// `Parsed.gaps` that `Parsed.fill_gap` and `Parsed.fail_gap` take; `image`,
/// its PNG or JPEG bytes; `media_type`, `image/png` or `image/jpeg`; and
/// `max_text_bytes`, the most bytes of Markdown a fill of it takes.
#[pyclass(frozen, module = "almiladoc")]
pub(crate) struct OcrRequest {
    gap: Py<PyTuple>,
    image: Py<PyBytes>,
    media_type: String,
    max_text_bytes: u64,
}

impl OcrRequest {
    pub(crate) fn of(
        py: Python<'_>,
        gap: GapRow,
        request: &almiladoc::OcrRequest,
    ) -> PyResult<Self> {
        Ok(Self {
            gap: gap.into_pyobject(py)?.unbind(),
            image: PyBytes::new(py, &request.image.bytes).unbind(),
            media_type: request.image.media_type.to_owned(),
            max_text_bytes: request.max_text_bytes,
        })
    }
}

#[pymethods]
impl OcrRequest {
    #[new]
    fn new(
        gap: Bound<'_, PyTuple>,
        image: Bound<'_, PyBytes>,
        media_type: String,
        max_text_bytes: u64,
    ) -> Self {
        Self { gap: gap.unbind(), image: image.unbind(), media_type, max_text_bytes }
    }

    /// The row of `Parsed.gaps` this request reads the image of.
    #[getter]
    fn gap<'py>(&self, py: Python<'py>) -> Bound<'py, PyTuple> {
        self.gap.bind(py).clone()
    }

    /// The image's PNG or JPEG bytes.
    #[getter]
    fn image<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        self.image.bind(py).clone()
    }

    /// `image/png` or `image/jpeg`.
    #[getter]
    fn media_type(&self) -> &str {
        &self.media_type
    }

    /// The most bytes of Markdown a fill of this gap takes.
    #[getter]
    fn max_text_bytes(&self) -> u64 {
        self.max_text_bytes
    }

    #[expect(
        clippy::type_complexity,
        reason = "pickle's reduce tuple of a class and its arguments"
    )]
    fn __reduce__<'py>(
        slf: &Bound<'py, Self>,
    ) -> (Bound<'py, PyType>, (Bound<'py, PyTuple>, Bound<'py, PyBytes>, String, u64)) {
        let py = slf.py();
        let this = slf.get();
        let arguments =
            (this.gap(py), this.image(py), this.media_type.clone(), this.max_text_bytes);
        (slf.get_type(), arguments)
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        Ok(format!(
            "OcrRequest(gap={}, media_type={:?}, image=<{} bytes>, max_text_bytes={})",
            self.gap.bind(py).repr()?,
            self.media_type,
            self.image.bind(py).as_bytes().len(),
            self.max_text_bytes
        ))
    }
}
