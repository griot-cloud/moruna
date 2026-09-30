//! The `moruna._core` module itself (l).

use pyo3::prelude::*;

use crate::VERSION;

/// `moruna._core`, built with `gil_used = false` (PY-I7): nothing in this module needs the
/// interpreter serialised, and declaring so keeps a free threaded interpreter free threaded.
#[pymodule(gil_used = false)]
pub fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", VERSION)?;
    crate::errors::register(m)?;
    crate::sources::register(m)?;
    crate::sinks::register(m)?;
    // The classes a user subclasses for a source or a sink of their own (07 e.6, 08 f.10).
    crate::extend::register(m)?;
    crate::kernel::register(m)?;
    crate::check::register(m)?;
    crate::report::register(m)?;
    m.add_function(wrap_pyfunction!(crate::inspect::inspect_host, m)?)?;
    m.add_function(wrap_pyfunction!(crate::run::run, m)?)?;
    crate::cli::register(m)?;
    Ok(())
}
