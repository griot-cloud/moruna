//! Python entry into the real native PEX record process, never a host sandbox fallback.
use pyo3::prelude::*;
/// Execute the actual child's checked native record loop, releasing the interpreter
/// while it waits. Package callable/factory/state handling remains in the sole loader.
#[pyfunction]
fn _native_pex_child(py: Python<'_>) -> PyResult<()> {
    crate::output::install(py);
    py.detach(moruna_runtime::native_pex::child)
        .map_err(|error| {
            crate::errors::to_py_err(py, &error, crate::errors::Attachments::default())
        })
}
/// Register only in the actual Linux x86_64 SDK build.
pub fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(_native_pex_child, module)?)
}
