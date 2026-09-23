//! GIL detection (f.4).
//!
//! The adapter reads the interpreter's GIL state rather than assuming it: a free threaded build
//! can re enable the GIL when a module that declares `gil_used = true` is imported, so the state
//! is a fact to be read, before the run and again after the first `apply` (AD-I3).

use pyo3::Python;
use pyo3::prelude::*;

/// True when the interpreter has a GIL right now.
///
/// Reads `sys._is_gil_enabled()`. An interpreter without the attribute predates free threading
/// and always has a GIL, so the absence of the attribute is itself the answer (f.4). A failure to
/// read it is treated the same way: the conservative answer is the one that serialises.
pub fn python_gil_enabled() -> bool {
    Python::attach(gil_enabled_in)
}

/// `python_gil_enabled` for a thread that is already attached.
pub(crate) fn gil_enabled_in(py: Python<'_>) -> bool {
    let Ok(sys) = py.import("sys") else {
        return true;
    };
    let Ok(is_enabled) = sys.getattr("_is_gil_enabled") else {
        return true;
    };
    is_enabled
        .call0()
        .and_then(|v| v.extract::<bool>())
        .unwrap_or(true)
}

/// The interpreter's version and build, for the run report: the `sys.version` line with the GIL
/// state appended, for example `3.14.3 free-threading build (...) gil_enabled=false`.
pub fn python_build_info() -> String {
    Python::attach(|py| {
        let version = py
            .import("sys")
            .and_then(|sys| sys.getattr("version"))
            .and_then(|v| v.extract::<String>())
            .unwrap_or_else(|_| "unknown".to_string());
        let one_line = version.replace('\n', " ");
        format!("{one_line} gil_enabled={}", gil_enabled_in(py))
    })
}
