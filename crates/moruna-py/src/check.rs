//! `moruna check` and the standard kernels, as the module exposes them (15 d.1).

use std::path::PathBuf;
use std::sync::Arc;

use moruna_kernel::Kernel;
use moruna_kernels::StdKernel;
use moruna_runtime::check::{CheckOptions, check};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyModule};

use crate::errors::{Attachments, to_py_err};
use crate::kernel::PyKernelHandle;
use crate::sources::type_name;

/// A standard kernel (15 e.6): what `moruna.std.<name>(...)` returns and `moruna.run` takes.
#[pyclass(frozen, module = "moruna._core", name = "StdKernel")]
pub struct PyStdKernel {
    pub(crate) kernel: StdKernel,
}

#[pymethods]
impl PyStdKernel {
    /// The kernel's name.
    #[getter]
    fn name(&self) -> String {
        self.kernel.name().to_string()
    }

    /// The canonical arguments, as JSON.
    #[getter]
    fn args(&self) -> String {
        moruna_kernels::args::canonical(self.kernel.args())
    }

    /// The fingerprint as 64 lowercase hexadecimal characters (15 e.6).
    #[getter]
    fn fingerprint(&self) -> String {
        self.kernel.fingerprint().to_hex()
    }

    fn __repr__(&self) -> String {
        format!("moruna.std.{}({})", self.kernel.name(), self.args())
    }
}

/// `_core.std_kernel(name, args_json)`: build a standard kernel; a bad name or argument is a
/// `PlanError` naming it.
#[pyfunction]
pub fn std_kernel(py: Python<'_>, name: &str, args_json: &str) -> PyResult<PyStdKernel> {
    let args: serde_json::Value = serde_json::from_str(args_json).map_err(|e| {
        pyo3::exceptions::PyValueError::new_err(format!("moruna.std.{name}: arguments: {e}"))
    })?;
    let kernel =
        StdKernel::new(name, &args).map_err(|e| to_py_err(py, &e, Attachments::default()))?;
    Ok(PyStdKernel { kernel })
}

/// `_core.check_kernel(kernel, *, name, seed, profiles_dir)`: the harness of 15 over one
/// kernel object; the JSON report of 15 e.3 plus its `summary`.
#[pyfunction]
#[pyo3(signature = (kernel, *, name = None, seed = 0, profiles_dir = None))]
pub fn check_kernel(
    py: Python<'_>,
    kernel: &Bound<'_, PyAny>,
    name: Option<String>,
    seed: u64,
    profiles_dir: Option<String>,
) -> PyResult<String> {
    let (target, mut opts): (Arc<dyn Kernel>, CheckOptions) =
        if let Ok(handle) = kernel.cast::<PyKernelHandle>() {
            let py_kernel = Arc::clone(&handle.get().kernel);
            let label = name.unwrap_or_else(|| {
                handle
                    .get()
                    .callable
                    .bind(py)
                    .getattr("__name__")
                    .and_then(|n| n.extract::<String>())
                    .unwrap_or_else(|_| "kernel".into())
            });
            let mut opts = CheckOptions::new(label);
            opts.kind = "python";
            opts.fingerprint_scheme = "sha256";
            opts.gil = Some(py_kernel.gil_state());
            let bound = Arc::clone(&py_kernel);
            opts.bind = Some(Box::new(move |alloc| bound.bind_allocator(alloc)));
            (py_kernel as Arc<dyn Kernel>, opts)
        } else if let Ok(std) = kernel.cast::<PyStdKernel>() {
            let std = std.get().kernel.clone();
            let mut opts = CheckOptions::new(name.unwrap_or_else(|| std.name().to_string()));
            opts.kind = "std";
            opts.fingerprint_scheme = "sha256";
            (Arc::new(std) as Arc<dyn Kernel>, opts)
        } else {
            return Err(pyo3::exceptions::PyTypeError::new_err(format!(
                "moruna check takes a kernel made by @moruna.kernel or moruna.std, not {}",
                type_name(kernel)
            )));
        };
    opts.seed = seed;
    opts.profiles_dir = profiles_dir.map(PathBuf::from);
    let report = py
        .detach(move || check(target, opts))
        .map_err(|e| to_py_err(py, &e, Attachments::default()))?;
    let mut json = report.to_json();
    if let serde_json::Value::Object(map) = &mut json {
        map.insert(
            "summary".into(),
            serde_json::Value::String(report.summary()),
        );
    }
    Ok(json.to_string())
}

/// `_core.default_profiles_dir()`: where a run reads profiles when it is given none (12 f.1).
#[pyfunction]
pub fn default_profiles_dir() -> Option<String> {
    moruna_runtime::spec::default_profiles_dir().map(|p| p.display().to_string())
}

/// Add the standard kernel class and the check functions to the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyStdKernel>()?;
    m.add_function(wrap_pyfunction!(std_kernel, m)?)?;
    m.add_function(wrap_pyfunction!(check_kernel, m)?)?;
    m.add_function(wrap_pyfunction!(default_profiles_dir, m)?)?;
    Ok(())
}
