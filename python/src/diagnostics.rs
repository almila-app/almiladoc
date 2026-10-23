//! `Diagnostic`: one entry of what a conversion could not carry into its
//! output.

use std::num::NonZeroU32;

use pyo3::prelude::*;
use pyo3::types::PyTuple;

/// What a conversion could not carry into its output, holding no input
/// text: equal diagnostics compare and hash equal.
#[pyclass(frozen, eq, hash, module = "almiladoc")]
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct Diagnostic(almiladoc::Diagnostic);

#[pymethods]
impl Diagnostic {
    /// What was reported, a stable `snake_case` name such as `part_unreadable`.
    #[getter]
    fn code(&self) -> &'static str {
        self.0.code.as_str()
    }

    /// The code's fixed class: `omitted` by a policy, `deferred` behind a
    /// placeholder, `lost`, or `info`; only `lost` makes a conversion
    /// incomplete.
    #[getter]
    fn loss_class(&self) -> &'static str {
        self.0.class.as_str()
    }

    /// Where the content sat, such as `body` or `note`.
    #[getter]
    fn region(&self) -> &'static str {
        self.0.region.as_str()
    }

    /// The 1-based page, slide, sheet or chapter it sat in, or `None` across
    /// the document.
    #[getter]
    fn unit(&self) -> Option<u32> {
        self.0.unit.map(NonZeroU32::get)
    }

    /// Occurrences.
    #[getter]
    fn count(&self) -> u64 {
        self.0.count
    }

    fn __repr__(&self) -> String {
        let unit = self.0.unit.map_or_else(|| "None".to_string(), |unit| unit.to_string());
        format!(
            "Diagnostic(code='{}', loss_class='{}', region='{}', unit={unit}, count={})",
            self.code(),
            self.loss_class(),
            self.region(),
            self.0.count
        )
    }
}

/// Every diagnostic of `diagnostics`, in their fixed order.
pub(crate) fn diagnostic_tuple<'py>(
    py: Python<'py>,
    diagnostics: &almiladoc::Diagnostics,
) -> PyResult<Bound<'py, PyTuple>> {
    PyTuple::new(py, diagnostics.iter().map(Diagnostic).collect::<Vec<_>>())
}
