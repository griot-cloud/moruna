//! `PyKernel`: a Python callable or class as an Moruna [`Kernel`] (d.1, f.1, f.2, f.4).

use core::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use moruna_kernel::{
    Allocator, MorunaError, Fingerprint, GilState, InitCtx, Kernel, KernelHints, KernelKind,
    KernelState, NoState, Payload, PayloadKind, PayloadSpec, Result, ResumePolicy, Seq,
    SourceSchema, StageId, TierPref,
};
use pyo3::prelude::*;

use super::cross::{self, Imported};
use super::ctx::PyInitCtx;
use super::fingerprint;
use super::gil::{gil_enabled_in, python_gil_enabled};
use super::state::{PyState, missing_restore};

/// The stage an `MorunaError::Kernel` from this crate carries.
///
/// `Kernel::apply` receives a `Payload`, not a morsel, so the adapter never learns which stage it
/// is or which morsel it is holding: a kernel that knew its position could not also be a Polars
/// expression or a DataFusion function, which S7 requires. AD-I7 names both fields all the same,
/// so the adapter fills them with a sentinel the scheduler can recognise and replace rather than
/// with a plausible lie. Reported.
pub const UNKNOWN_STAGE: StageId = StageId::MAX;
/// The morsel an `MorunaError::Kernel` from this crate carries; see [`UNKNOWN_STAGE`].
pub const UNKNOWN_SEQ: Seq = Seq::MAX;

const GIL_FREE_THREADED: u8 = 0;
const GIL_SERIALISED: u8 = 1;

/// An `MorunaError::Kernel` from the adapter, with the sentinel stage and morsel.
pub(crate) fn kernel_error(msg: String) -> MorunaError {
    MorunaError::Kernel {
        stage: UNKNOWN_STAGE,
        seq: UNKNOWN_SEQ,
        msg,
        features: None,
    }
}

/// AD-I7: a Python exception as a message, exactly `"<type>: <message>\n<traceback>"`.
pub(crate) fn py_err_message(py: Python<'_>, err: &PyErr) -> String {
    let type_name = err
        .get_type(py)
        .qualname()
        .map(|q| q.to_string())
        .unwrap_or_else(|_| "Exception".to_string());
    let value = err
        .value(py)
        .str()
        .map(|s| s.to_string())
        .unwrap_or_default();
    format!("{type_name}: {value}\n{}", formatted_traceback(py, err))
}

fn formatted_traceback(py: Python<'_>, err: &PyErr) -> String {
    let Ok(traceback) = py.import("traceback") else {
        return String::new();
    };
    let exception = err.value(py);
    let Ok(lines) = traceback.call_method1("format_exception", (exception,)) else {
        return String::new();
    };
    lines
        .extract::<Vec<String>>()
        .map(|parts| parts.concat())
        .unwrap_or_default()
}

/// What the Python surface builds a kernel from (d.1). Every field but `callable` is a decorator
/// argument and every one of them is part of the fingerprint (e.4).
pub struct PyKernelSpec {
    /// The plain callable (stateless) or the class instance (stateful), section b.
    pub callable: Py<PyAny>,
    /// True for a class kernel with `setup` and `__call__`.
    pub stateful: bool,
    /// `KernelKind::Stateful { max_instances }`; ignored when `!stateful`.
    pub instances: NonZeroUsize,
    /// `KernelHints::uses_device_memory`.
    pub device_memory: bool,
    /// What the kernel wants delivered; the default is a host table.
    pub accepts: PayloadSpec,
    /// Whether `apply` releases the GIL, when the author declared it.
    pub releases_gil: Option<bool>,
    /// Expected peak working set over input bytes, when the author declared it.
    pub expected_amplification: Option<f64>,
    /// A row count the kernel would rather receive.
    pub preferred_rows: Option<u64>,
    /// `resume="reinit" | "checkpoint" | "forbid"`.
    pub resume: ResumePolicy,
    /// Bytes one instance's state is expected to hold (RC f.3).
    pub state_bytes: Option<u64>,
}

impl PyKernelSpec {
    /// A stateless spec over `callable` with every decorator argument at its default: one
    /// instance, no device memory, a host table, no declared hints, `ResumePolicy::Reinit`.
    pub fn new(callable: Py<PyAny>) -> PyKernelSpec {
        PyKernelSpec {
            callable,
            stateful: false,
            instances: NonZeroUsize::MIN,
            device_memory: false,
            accepts: PayloadSpec {
                kind: PayloadKind::Table,
                tier: TierPref::Host,
            },
            releases_gil: None,
            expected_amplification: None,
            preferred_rows: None,
            resume: ResumePolicy::Reinit,
            state_bytes: None,
        }
    }
}

/// What the run report says about one Python kernel (section j).
#[derive(Copy, Clone, Debug)]
pub struct PyKernelStats {
    /// `apply` calls made.
    pub calls: u64,
    /// Calls that ended in a Python exception.
    pub exceptions: u64,
    /// Morsels copied into the arena at the boundary (AD-I2).
    pub boundary_copies: u64,
    /// Whether this kernel's invocations are serialised.
    pub gil_state: GilState,
    /// Whether the fingerprint saw the kernel's source text (e.4).
    pub source_available: bool,
}

/// A Python callable or class as an Moruna kernel.
pub struct PyKernel {
    callable: Py<PyAny>,
    stateful: bool,
    instances: NonZeroUsize,
    accepts: PayloadSpec,
    hints: KernelHints,
    resume: ResumePolicy,
    has_footprint: bool,
    fingerprint: Fingerprint,
    source_available: bool,
    gil: AtomicU8,
    flip_checked: AtomicBool,
    serialise: Mutex<()>,
    alloc: OnceLock<Arc<dyn Allocator>>,
    calls: AtomicU64,
    exceptions: AtomicU64,
    boundary_copies: AtomicU64,
}

impl PyKernel {
    /// Build the kernel from the spec alone, so the surface can construct it before any runtime
    /// component exists (12 e.1). Reads the interpreter's GIL state (f.4), computes the
    /// fingerprint (e.4) and checks the shape the spec claims: a stateful kernel must have
    /// `setup` and `__call__`, and a `Checkpoint` kernel must have `checkpoint` and `restore`.
    /// A missing one is a `Plan` error naming it, raised here rather than on morsel 40,000.
    pub fn new(spec: PyKernelSpec) -> Result<PyKernel> {
        Python::attach(|py| {
            let callable = spec.callable.bind(py);
            if spec.stateful {
                for method in ["setup", "__call__"] {
                    if !callable.hasattr(method).unwrap_or(false) {
                        return Err(MorunaError::Plan(format!(
                            "a stateful Python kernel needs {method}; this one has no {method}"
                        )));
                    }
                }
            } else if !callable.is_callable() {
                return Err(MorunaError::Plan(
                    "a stateless Python kernel must be callable".into(),
                ));
            }
            if spec.resume == ResumePolicy::Checkpoint {
                for method in ["checkpoint", "restore"] {
                    if !callable.hasattr(method).unwrap_or(false) {
                        return Err(MorunaError::Plan(format!(
                            "resume='checkpoint' needs {method}; this kernel has no {method}"
                        )));
                    }
                }
            }
            let has_footprint = callable.hasattr("footprint").unwrap_or(false);
            let printed = fingerprint::compute(py, &spec);
            let gil = if gil_enabled_in(py) {
                GIL_SERIALISED
            } else {
                GIL_FREE_THREADED
            };
            tracing::info!(
                target: "adapter.gil",
                serialised = gil == GIL_SERIALISED,
                "Python kernel built"
            );
            Ok(PyKernel {
                callable: spec.callable.clone_ref(py),
                stateful: spec.stateful,
                instances: spec.instances,
                accepts: spec.accepts,
                hints: KernelHints {
                    expected_amplification: spec.expected_amplification,
                    uses_device_memory: spec.device_memory,
                    releases_gil: spec.releases_gil,
                    preferred_rows: spec.preferred_rows,
                    resume: spec.resume,
                    state_bytes: spec.state_bytes,
                },
                resume: spec.resume,
                has_footprint,
                fingerprint: printed.fingerprint,
                source_available: printed.source_available,
                gil: AtomicU8::new(gil),
                flip_checked: AtomicBool::new(false),
                serialise: Mutex::new(()),
                alloc: OnceLock::new(),
                calls: AtomicU64::new(0),
                exceptions: AtomicU64::new(0),
                boundary_copies: AtomicU64::new(0),
            })
        })
    }

    /// The arena the boundary copy allocates from (d.1, f.3). Called once by the facade in its
    /// "kernels built" step, after the arena exists and before the scheduler is constructed; a
    /// second call is ignored, because the arena a run allocates from does not change.
    pub fn bind_allocator(&self, alloc: Arc<dyn Allocator>) {
        let _ = self.alloc.set(alloc);
    }

    /// AD-I3: `Serialised` under a GIL build or after a flip, `FreeThreaded` otherwise.
    pub fn gil_state(&self) -> GilState {
        if self.gil.load(Ordering::SeqCst) == GIL_SERIALISED {
            GilState::Serialised
        } else {
            GilState::FreeThreaded
        }
    }

    /// What this kernel has done so far (section j).
    pub fn stats(&self) -> PyKernelStats {
        PyKernelStats {
            calls: self.calls.load(Ordering::SeqCst),
            exceptions: self.exceptions.load(Ordering::SeqCst),
            boundary_copies: self.boundary_copies.load(Ordering::SeqCst),
            gil_state: self.gil_state(),
            source_available: self.source_available,
        }
    }

    /// f.2: under a GIL build the adapter serialises `apply` itself, so the scheduler's
    /// accounting sees the serialisation instead of inferring it from timings.
    fn serialise_guard(&self) -> Option<MutexGuard<'_, ()>> {
        match self.gil_state() {
            GilState::Serialised => Some(
                self.serialise
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            ),
            GilState::FreeThreaded => None,
        }
    }

    /// f.4: an import inside the first `apply` may have re-enabled the GIL. Checked once, after
    /// the first call of this kernel; a flip is permanent for the rest of the run.
    fn recheck_gil(&self) {
        if self.flip_checked.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.gil_state() == GilState::Serialised {
            return;
        }
        if python_gil_enabled() {
            self.gil.store(GIL_SERIALISED, Ordering::SeqCst);
            tracing::warn!(
                target: "adapter.gil",
                "the GIL was re-enabled during the first apply; this kernel is now serialised"
            );
        }
    }

    /// The attached half of `apply` (f.1): export, call, import. Nothing here touches the arena.
    fn call_python(
        &self,
        state: &mut dyn KernelState,
        input: Payload,
    ) -> Result<(Imported, Py<PyAny>)> {
        Python::attach(|py| {
            let argument = cross::export(py, input)?;
            let callable = self.callable.bind(py);
            let returned = if self.stateful {
                let py_state = state
                    .as_any_mut()
                    .downcast_mut::<PyState>()
                    .ok_or_else(|| {
                        kernel_error(
                            "a stateful Python kernel was given a state it did not make".into(),
                        )
                    })?;
                let instance_state = py_state.state.bind(py).clone();
                callable.call1((instance_state, argument))
            } else {
                callable.call1((argument,))
            };
            let returned = returned.map_err(|e| kernel_error(py_err_message(py, &e)))?;
            let imported = cross::import(py, &returned)?;
            Ok((imported, returned.unbind()))
        })
    }

    fn ctx_object<'py>(&self, py: Python<'py>, ctx: &InitCtx) -> Result<Bound<'py, PyAny>> {
        Bound::new(py, PyInitCtx::of(ctx))
            .map(Bound::into_any)
            .map_err(|e| kernel_error(format!("building ctx: {}", py_err_message(py, &e))))
    }
}

impl std::fmt::Debug for PyKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PyKernel")
            .field("fingerprint", &self.fingerprint)
            .field("stateful", &self.stateful)
            .field("gil_state", &self.gil_state())
            .finish()
    }
}

impl Kernel for PyKernel {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn kind(&self) -> KernelKind {
        if self.stateful {
            KernelKind::Stateful {
                max_instances: self.instances,
            }
        } else {
            KernelKind::Stateless
        }
    }

    fn hints(&self) -> KernelHints {
        self.hints.clone()
    }

    fn accepts(&self) -> PayloadSpec {
        self.accepts
    }

    /// A Python kernel is opaque: nothing it exposes says what its output schema will be, and
    /// asking it would mean running it. The adapter therefore answers with the input schema,
    /// which is right for the identity shaped majority and is corrected by the first morsel for
    /// the rest. The SDD does not say what this should return; reported.
    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        self.accepts.check(input)?;
        Ok(input.clone())
    }

    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        if !self.stateful {
            return Ok(Box::new(NoState));
        }
        Python::attach(|py| {
            let ctx_obj = self.ctx_object(py, ctx)?;
            let state = self
                .callable
                .bind(py)
                .call_method1("setup", (ctx_obj,))
                .map_err(|e| kernel_error(py_err_message(py, &e)))?;
            Ok(Box::new(PyState::new(
                state.unbind(),
                self.callable.clone_ref(py),
                ctx.instance,
                ctx.device,
                self.resume,
                self.has_footprint,
            )) as Box<dyn KernelState>)
        })
    }

    fn restore(&self, ctx: &InitCtx, state: &[u8]) -> Result<Box<dyn KernelState>> {
        if self.resume != ResumePolicy::Checkpoint {
            return Err(missing_restore());
        }
        Python::attach(|py| {
            let ctx_obj = self.ctx_object(py, ctx)?;
            let restored = PyState::restored(
                py,
                &self.callable,
                ctx_obj,
                state,
                ctx.instance,
                ctx.device,
                self.resume,
                self.has_footprint,
            )?;
            Ok(Box::new(restored) as Box<dyn KernelState>)
        })
    }

    /// f.1 and f.2. The interpreter is attached for the export, the call and the import, and for
    /// nothing else: the boundary copy runs detached, because it allocates from the arena and
    /// AD-I4 forbids holding an attachment across such a call. The returned Python object is kept
    /// alive by a handle until the copy has finished with it and is then released under a brief
    /// attachment.
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let Some(alloc) = self.alloc.get().cloned() else {
            return Err(MorunaError::Config {
                name: "adapter",
                msg: "apply before bind_allocator; the facade binds the arena in its kernels built step".into(),
            });
        };
        let _serialised = self.serialise_guard();
        self.calls.fetch_add(1, Ordering::SeqCst);
        let called = self.call_python(state, input);
        self.recheck_gil();
        let (imported, returned) = match called {
            Ok(pair) => pair,
            Err(e) => {
                self.exceptions.fetch_add(1, Ordering::SeqCst);
                return Err(e);
            }
        };
        let landed = super::copy::land(imported, &alloc);
        Python::attach(move |_| drop(returned));
        let landed = landed?;
        if landed.copied_bytes > 0 {
            self.boundary_copies.fetch_add(1, Ordering::SeqCst);
        }
        Ok(landed.payload)
    }
}
