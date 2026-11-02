//! `moruna check` and the standard kernels, as the module exposes them (MH 4.9).

use std::sync::Arc;

use moruna_kernel::Kernel;
use moruna_kernels::StdKernel;
use moruna_runtime::check::{CheckOptions, WarmUp, check};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyModule};

use crate::errors::{Attachments, to_py_err};
use crate::kernel::PyKernelHandle;
use crate::sources::type_name;

/// A standard kernel (MH 4.9): what `moruna.std.<name>(...)` returns and `moruna.run` takes.
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

    /// The fingerprint as 64 lowercase hexadecimal characters (MH 4.9).
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

/// What a surface's warm-up settings make (AR section 6): `None` when warm-up is off; else the
/// observation store at `store` (the default directory when `None`), the budget in seconds, the
/// statistics as JSON and the model by name, each refused with its cause.
pub(crate) fn warm_up_of(
    warm_up: bool,
    store: Option<&str>,
    budget: f64,
    statistics: Option<&str>,
    model: Option<&str>,
    cost: Option<moruna_runtime::cost::CostSettings>,
) -> moruna_kernel::Result<Option<WarmUp>> {
    if !warm_up {
        return Ok(None);
    }
    if let Some(cost) = cost {
        if store.is_some() || model.is_some() {
            return Err(moruna_kernel::MorunaError::Config {
                name: "warm_up",
                msg: "cost session and store/model overrides name two authorities".into(),
            });
        }
        WarmUp::with_settings(cost, budget, statistics).map(Some)
    } else {
        WarmUp::configured(store.map(std::path::Path::new), budget, statistics, model).map(Some)
    }
}

/// `_core.check_kernel(kernel, *, name, seed, warm_up, store, budget, statistics, model)`: the
/// harness of 15 over one kernel object, warmed up unless `warm_up` is false; the JSON report
/// of MH 4.9 plus its `summary`.
#[pyfunction]
#[pyo3(signature = (kernel, *, name = None, seed = 0, warm_up = true, store = None, budget = 60.0, statistics = None, model = None, cost = None))]
#[allow(clippy::too_many_arguments)]
pub fn check_kernel(
    py: Python<'_>,
    kernel: &Bound<'_, PyAny>,
    name: Option<String>,
    seed: u64,
    warm_up: bool,
    store: Option<String>,
    budget: f64,
    statistics: Option<String>,
    model: Option<String>,
    cost: Option<PyRef<'_, crate::engine::PyCostSession>>,
) -> PyResult<String> {
    let _guard = crate::run::RunningGuard::acquire()
        .map_err(|e| to_py_err(py, &e, Attachments::default()))?;
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
            opts.memory_guard = Some(py_kernel.memory_guard());
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
    let cost_settings = cost.map(|cost| cost.settings());
    let report = py
        .detach(move || {
            opts.warm_up = warm_up_of(
                warm_up,
                store.as_deref(),
                budget,
                statistics.as_deref(),
                model.as_deref(),
                cost_settings,
            )?;
            if let Some(warm) = &mut opts.warm_up {
                warm.settings = moruna_runtime::cost::registration_settings(
                    opts.kind,
                    &serde_json::Value::Null,
                );
            }
            check(target, opts)
        })
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

/// `_core.default_store_dir()`: where a check keeps its observations when it is given no store.
#[pyfunction]
pub fn default_store_dir() -> Option<String> {
    moruna_runtime::cost::default_store_dir().map(|p| p.display().to_string())
}

/// Add the standard kernel class and the check functions to the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyStdKernel>()?;
    m.add_function(wrap_pyfunction!(std_kernel, m)?)?;
    m.add_function(wrap_pyfunction!(check_kernel, m)?)?;
    m.add_function(wrap_pyfunction!(default_store_dir, m)?)?;
    Ok(())
}
