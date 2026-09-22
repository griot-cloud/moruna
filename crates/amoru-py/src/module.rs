//! The `amoru._core` module itself (l).

use pyo3::prelude::*;

use crate::VERSION;

/// `amoru._core`, built with `gil_used = false` (PY-I7): nothing in this module needs the
/// interpreter serialised, and declaring so keeps a free threaded interpreter free threaded.
#[pymodule(gil_used = false)]
pub fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", VERSION)?;
    crate::errors::register(m)?;
    crate::sources::register(m)?;
    crate::sinks::register(m)?;
    crate::kernel::register(m)?;
    crate::report::register(m)?;
    m.add_function(wrap_pyfunction!(crate::inspect::inspect_host, m)?)?;
    m.add_function(wrap_pyfunction!(crate::run::run, m)?)?;
    Ok(())
}
