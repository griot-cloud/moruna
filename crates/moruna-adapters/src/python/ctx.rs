//! The `ctx` object a class kernel's `setup` and `restore` receive (e.1).

use moruna_kernel::InitCtx;
use pyo3::prelude::*;

/// What `setup(self, ctx)` and `restore(self, ctx, data)` are handed: a small frozen object with
/// `instance` (int) and `device` (`"cuda:N"` or `None`), and nothing else. The arena is
/// deliberately absent: a Python kernel allocates through its own libraries (AD-O1).
#[pyclass(frozen, name = "KernelContext", module = "moruna")]
pub struct PyInitCtx {
    /// The instance index, `0..max_instances`.
    #[pyo3(get)]
    pub instance: usize,
    /// The device assigned to this instance, as `"cuda:N"`, or `None` on a host only run.
    #[pyo3(get)]
    pub device: Option<String>,
}

impl PyInitCtx {
    /// The `ctx` object for one `InitCtx` (e.1).
    pub fn of(ctx: &InitCtx) -> PyInitCtx {
        PyInitCtx {
            instance: ctx.instance,
            device: ctx.device.map(|d| format!("cuda:{}", d.0)),
        }
    }
}

#[pymethods]
impl PyInitCtx {
    fn __repr__(&self) -> String {
        match &self.device {
            Some(device) => format!(
                "KernelContext(instance={}, device='{device}')",
                self.instance
            ),
            None => format!("KernelContext(instance={}, device=None)", self.instance),
        }
    }
}
