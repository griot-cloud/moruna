//! The per instance state of a class kernel (e.1) and its checkpoint path (f.7).
//!
//! Nothing here is deep copied. A class kernel is one Python object shared by every instance; an
//! instance is the value its `setup` returned, held as a reference and handed back as the first
//! argument of every `__call__`. `instances` independent states come from `instances` calls of
//! `setup`, not from cloning anything.

use moruna_kernel::{DeviceId, KernelState, MorunaError, Result, ResumePolicy};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use super::kernel::{kernel_error, py_err_message};

/// One instance of a class kernel.
pub struct PyState {
    /// The value `setup` (or `restore`) returned.
    pub(crate) state: Py<PyAny>,
    /// The class kernel itself, so `checkpoint` and `footprint` can be called on it.
    callable: Py<PyAny>,
    instance: usize,
    device: Option<DeviceId>,
    resume: ResumePolicy,
    has_footprint: bool,
}

impl PyState {
    /// Build the state for one instance.
    pub(crate) fn new(
        state: Py<PyAny>,
        callable: Py<PyAny>,
        instance: usize,
        device: Option<DeviceId>,
        resume: ResumePolicy,
        has_footprint: bool,
    ) -> PyState {
        PyState {
            state,
            callable,
            instance,
            device,
            resume,
            has_footprint,
        }
    }

    /// The value `setup` or `restore` returned for this instance (e.1). The adapter never
    /// copies or clones it; this is a borrow of the handle it holds.
    pub fn state(&self) -> &Py<PyAny> {
        &self.state
    }

    /// The instance index this state belongs to, `0..max_instances`.
    pub fn instance(&self) -> usize {
        self.instance
    }

    /// The device this instance was built for, if any.
    pub fn device(&self) -> Option<DeviceId> {
        self.device
    }
}

impl KernelState for PyState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// f.7: for a `Checkpoint` kernel, call `checkpoint(state)` under attachment and copy the
    /// bytes out of the Python object before detaching. For `Reinit` and `Forbid` nothing is
    /// called and the answer is "nothing to save".
    fn checkpoint(&mut self) -> Result<Option<Vec<u8>>> {
        match self.resume {
            ResumePolicy::Reinit | ResumePolicy::Forbid => Ok(None),
            ResumePolicy::Checkpoint => Python::attach(|py| {
                let bytes = self
                    .callable
                    .bind(py)
                    .call_method1("checkpoint", (self.state.bind(py),))
                    .map_err(|e| kernel_error(py_err_message(py, &e)))?;
                let owned = bytes.extract::<Vec<u8>>().map_err(|e| {
                    kernel_error(format!(
                        "checkpoint must return bytes: {}",
                        py_err_message(py, &e)
                    ))
                })?;
                Ok(Some(owned))
            }),
        }
    }

    /// Python None is genuinely unknown; raised/invalid readings propagate as errors.
    fn footprint(&self) -> Result<Option<u64>> {
        if !self.has_footprint {
            return Ok(None);
        }
        Python::attach(|py| {
            let value = self
                .callable
                .bind(py)
                .call_method1("footprint", (self.state.bind(py),))
                .map_err(|e| {
                    kernel_error(format!(
                        "footprint raised for instance {}: {}",
                        self.instance,
                        py_err_message(py, &e)
                    ))
                })?;
            if value.is_none() {
                return Ok(None);
            }
            value.extract::<u64>().map(Some).map_err(|e| {
                kernel_error(format!(
                    "footprint must return an unsigned integer or None: {}",
                    py_err_message(py, &e)
                ))
            })
        })
    }
}

impl PyState {
    /// Replace the state with the value `restore(ctx, data)` returned (f.7).
    ///
    /// Every argument is one field of the state being built, so there is nothing to group.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn restored(
        py: Python<'_>,
        callable: &Py<PyAny>,
        ctx_obj: Bound<'_, PyAny>,
        data: &[u8],
        instance: usize,
        device: Option<DeviceId>,
        resume: ResumePolicy,
        has_footprint: bool,
    ) -> Result<PyState> {
        let payload = PyBytes::new(py, data);
        let state = callable
            .bind(py)
            .call_method1("restore", (ctx_obj, payload))
            .map_err(|e| kernel_error(py_err_message(py, &e)))?;
        Ok(PyState::new(
            state.unbind(),
            callable.clone_ref(py),
            instance,
            device,
            resume,
            has_footprint,
        ))
    }
}

/// `MorunaError::Resume` for a kernel that declares `Checkpoint` and whose `restore` is missing at
/// the moment it is needed; `PyKernel::new` refuses such a kernel long before this, so reaching
/// it is a bug in the caller rather than a run condition.
pub(crate) fn missing_restore() -> MorunaError {
    MorunaError::Resume("the Python kernel declares Checkpoint but has no restore".into())
}
