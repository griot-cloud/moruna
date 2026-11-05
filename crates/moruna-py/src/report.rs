//! The run report as Python holds it: the document `moruna_runtime::report_doc` makes, read by
//! `moruna.RunReport` (`python/moruna/_report.py`).

use pyo3::prelude::*;

/// The report document as the Python object `moruna.RunReport` is.
pub(crate) fn to_python(py: Python<'_>, text: &str) -> PyResult<Py<PyAny>> {
    Ok(py
        .import("moruna._report")?
        .getattr("RunReport")?
        .call_method1("_from_json", (text,))?
        .unbind())
}
