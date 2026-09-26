//! `@moruna.kernel`: the decorator's product (b, d.2, l).
//!
//! The decorator returns a frozen `#[pyclass] KernelSpec` carrying a `PyKernel` built from a
//! `PyKernelSpec` (05 d.1). The kernel is built here, in `Configuring` (e.1), so a class kernel
//! that declares `resume="checkpoint"` without `checkpoint` and `restore`, or a stateful object
//! without `setup` and `__call__`, fails at decoration and not at the first morsel.

use core::num::NonZeroUsize;
use std::sync::Arc;

use moruna_adapters::{PyKernel, PyKernelSpec};
use moruna_kernel::declare::{ColumnDecl, Declared, SchemaDecl, parse_type};
use moruna_kernel::{PayloadKind, PayloadSpec, ResumePolicy, TierPref};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyModule};

use crate::errors::{Attachments, to_py_err};
use crate::sources::type_name;

/// What `@moruna.kernel` produces: a frozen handle over one `PyKernel` (b).
#[pyclass(frozen, module = "moruna._core", name = "KernelSpec")]
pub struct PyKernelHandle {
    pub(crate) kernel: Arc<PyKernel>,
    /// The decorated object, so `KernelSpec.__call__` still calls what the user wrote and a
    /// decorated function stays usable as a plain function.
    pub(crate) callable: Py<PyAny>,
    stateful: bool,
}

#[pymethods]
impl PyKernelHandle {
    /// Call the decorated object, so `@moruna.kernel` does not take a function away from its module.
    #[pyo3(signature = (*args, **kwargs))]
    fn __call__(
        &self,
        py: Python<'_>,
        args: &Bound<'_, pyo3::types::PyTuple>,
        kwargs: Option<&Bound<'_, pyo3::types::PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        Ok(self.callable.bind(py).call(args, kwargs)?.unbind())
    }

    /// The wrapped object.
    #[getter]
    fn wrapped(&self, py: Python<'_>) -> Py<PyAny> {
        self.callable.clone_ref(py)
    }

    /// Whether the kernel keeps per-instance state.
    #[getter]
    fn stateful(&self) -> bool {
        self.stateful
    }

    /// The kernel's fingerprint as 64 lowercase hexadecimal characters (05 e.4).
    #[getter]
    fn fingerprint(&self) -> String {
        use moruna_kernel::Kernel;
        self.kernel
            .fingerprint()
            .0
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        let name = self
            .callable
            .bind(py)
            .getattr("__name__")
            .and_then(|n| n.extract::<String>())
            .unwrap_or_else(|_| type_name(self.callable.bind(py)));
        format!("KernelSpec({name}, stateful={})", self.stateful)
    }
}

/// Build the kernel object `@moruna.kernel` returns (d.2). The Python half applies the decorator;
/// this is the whole of the work it does.
#[allow(clippy::too_many_arguments)]
pub fn build(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    stateful: bool,
    instances: usize,
    device_memory: bool,
    accepts: &str,
    tier: &str,
    releases_gil: Option<bool>,
    expected_amplification: Option<f64>,
    preferred_rows: Option<u64>,
    resume: &str,
    state_bytes: Option<u64>,
    declared: Declared,
    lockfile: Option<Vec<u8>>,
    origin: Option<&Bound<'_, PyAny>>,
) -> PyResult<PyKernelHandle> {
    if !obj.is_callable() && !stateful {
        return Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "a stateless kernel must be callable, and {} is not",
            type_name(obj)
        )));
    }
    let instances = NonZeroUsize::new(instances)
        .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("instances must be at least 1"))?;
    let spec = PyKernelSpec {
        callable: obj.clone().unbind(),
        stateful,
        instances,
        device_memory,
        accepts: payload_spec(accepts, tier)?,
        releases_gil,
        expected_amplification,
        preferred_rows,
        resume: resume_policy(resume)?,
        state_bytes,
        declared,
        lockfile,
        origin: origin.map(|o| o.clone().unbind()),
    };
    let kernel = PyKernel::new(spec).map_err(|e| to_py_err(py, &e, Attachments::default()))?;
    Ok(PyKernelHandle {
        kernel: Arc::new(kernel),
        callable: obj.clone().unbind(),
        stateful,
    })
}

fn payload_spec(accepts: &str, tier: &str) -> PyResult<PayloadSpec> {
    let kind = match accepts {
        "table" => PayloadKind::Table,
        "tensor" => PayloadKind::Tensor,
        "either" => PayloadKind::Either,
        other => {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unknown accepts `{other}` (use \"table\", \"tensor\" or \"either\")"
            )));
        }
    };
    let tier = match tier {
        "host" => TierPref::Host,
        "device" => TierPref::Device,
        "any" => TierPref::Any,
        other => {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unknown tier `{other}` (use \"host\", \"device\" or \"any\")"
            )));
        }
    };
    Ok(PayloadSpec { kind, tier })
}

fn resume_policy(resume: &str) -> PyResult<ResumePolicy> {
    Ok(match resume {
        "reinit" => ResumePolicy::Reinit,
        "checkpoint" => ResumePolicy::Checkpoint,
        "forbid" => ResumePolicy::Forbid,
        other => {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unknown resume `{other}` (use \"reinit\", \"checkpoint\" or \"forbid\")"
            )));
        }
    })
}

/// One stage of a run as the surface was given it: a Python kernel, or a standard one.
pub enum Stage {
    /// A Python kernel (`@moruna.kernel`, or a plain callable).
    Py(Arc<PyKernel>),
    /// A standard kernel (`moruna.std.<name>`), which may fuse with its neighbours.
    Std(Box<moruna_kernels::StdKernel>),
}

/// The kernels of one run, in stage order, from whatever `moruna.run` was given: one kernel, a
/// list, a decorated object, a standard kernel or a plain callable (f.3). Adjacent standard
/// kernels are fused where their combination is one stage (15 f.6).
pub fn kernels_of(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Vec<Stage>> {
    let items: Vec<Bound<'_, PyAny>> = if value.is_instance_of::<pyo3::types::PyList>()
        || value.is_instance_of::<pyo3::types::PyTuple>()
    {
        value.try_iter()?.collect::<PyResult<Vec<_>>>()?
    } else {
        vec![value.clone()]
    };
    let mut out: Vec<Stage> = Vec::with_capacity(items.len());
    for item in items {
        if let Ok(handle) = item.cast::<PyKernelHandle>() {
            out.push(Stage::Py(Arc::clone(&handle.get().kernel)));
            continue;
        }
        if let Ok(std) = item.cast::<crate::check::PyStdKernel>() {
            let kernel = std.get().kernel.clone();
            if let Some(Stage::Std(last)) = out.last()
                && let Some(fused) = moruna_kernels::fuse(last, &kernel)
            {
                let len = out.len();
                out[len - 1] = Stage::Std(Box::new(fused));
            } else {
                out.push(Stage::Std(Box::new(kernel)));
            }
            continue;
        }
        if item.is_callable() {
            // "a plain function passed as a kernel is wrapped as if decorated with defaults" (f.3).
            let handle = build(
                py,
                &item,
                false,
                1,
                false,
                "table",
                "host",
                None,
                None,
                None,
                "reinit",
                None,
                Declared::default(),
                None,
                None,
            )?;
            out.push(Stage::Py(handle.kernel));
            continue;
        }
        return Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "a kernel must be produced by @moruna.kernel or moruna.std, or be callable, and {} is \
             none of these",
            type_name(&item)
        )));
    }
    Ok(out)
}

/// A declaration from the tuple `moruna._declare.declaration` builds (05 e.5): `("exact",
/// cols)`, `("subset", cols)` or `("relative", adds, drops, changes)`, a column being
/// `(name, type, nullable)`.
fn declaration(py: Python<'_>, value: Option<&Bound<'_, PyAny>>) -> PyResult<Option<SchemaDecl>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_none() {
        return Ok(None);
    }
    let columns = |v: &Bound<'_, PyAny>| -> PyResult<Vec<ColumnDecl>> {
        let raw: Vec<(String, String, bool)> = v.extract()?;
        raw.into_iter()
            .map(|(name, ty, nullable)| {
                let ty = parse_type(&ty).map_err(|e| to_py_err(py, &e, Attachments::default()))?;
                Ok(ColumnDecl { name, ty, nullable })
            })
            .collect()
    };
    let kind: String = value.get_item(0)?.extract()?;
    Ok(Some(match kind.as_str() {
        "exact" => SchemaDecl::Exact(columns(&value.get_item(1)?)?),
        "subset" => SchemaDecl::Subset(columns(&value.get_item(1)?)?),
        "relative" => SchemaDecl::Relative {
            adds: columns(&value.get_item(1)?)?,
            drops: value.get_item(2)?.extract()?,
            changes: columns(&value.get_item(3)?)?,
        },
        other => {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unknown declaration kind `{other}`"
            )));
        }
    }))
}

/// The decorator's Rust half, exposed as `_core.build_kernel`; `moruna.kernel` applies it.
#[pyfunction]
#[pyo3(signature = (obj, *, stateful = false, instances = 1, device_memory = false,
                    accepts = "table", tier = "host", releases_gil = None,
                    expected_amplification = None, preferred_rows = None,
                    resume = "reinit", state_bytes = None, input_schema = None,
                    output_schema = None, lockfile = None, origin = None))]
#[allow(clippy::too_many_arguments)]
pub fn build_kernel(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    stateful: bool,
    instances: usize,
    device_memory: bool,
    accepts: &str,
    tier: &str,
    releases_gil: Option<bool>,
    expected_amplification: Option<f64>,
    preferred_rows: Option<u64>,
    resume: &str,
    state_bytes: Option<u64>,
    input_schema: Option<&Bound<'_, PyAny>>,
    output_schema: Option<&Bound<'_, PyAny>>,
    lockfile: Option<&Bound<'_, pyo3::types::PyBytes>>,
    origin: Option<&Bound<'_, PyAny>>,
) -> PyResult<PyKernelHandle> {
    let lockfile = lockfile.map(|b| b.as_bytes().to_vec());
    let declared = Declared {
        input: declaration(py, input_schema)?,
        output: declaration(py, output_schema)?,
    };
    build(
        py,
        obj,
        stateful,
        instances,
        device_memory,
        accepts,
        tier,
        releases_gil,
        expected_amplification,
        preferred_rows,
        resume,
        state_bytes,
        declared,
        lockfile,
        origin,
    )
}

/// Add the kernel class and the decorator's builder to the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyKernelHandle>()?;
    m.add_function(wrap_pyfunction!(build_kernel, m)?)?;
    Ok(())
}
