//! The kernel contract (contracts d.7).

use std::sync::Arc;

use crate::buffer::Allocator;
use crate::error::MorunaError;
use crate::fingerprint::Fingerprint;
use crate::ids::DeviceId;
use crate::payload::{Payload, PayloadSpec, SourceSchema};

/// Whether a kernel keeps per-instance state between `apply` calls.
pub enum KernelKind {
    /// No state; any worker may run any morsel.
    Stateless,
    /// Per-instance state, at most `max_instances` instances.
    Stateful {
        /// The instance pool size; the scheduler creates every instance eagerly (preamble 4.4).
        max_instances: core::num::NonZeroUsize,
    },
}

/// How a kernel's instances come back when a run is resumed from its manifest.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum ResumePolicy {
    /// A fresh `init` is enough: the state does not depend on which morsels were
    /// seen (a loaded model, a compiled expression). The default.
    #[default]
    Reinit,
    /// The state depends on morsels seen; `KernelState::checkpoint` returns it and
    /// `Kernel::restore` rebuilds it. The scheduler checkpoints every instance at
    /// each manifest write.
    Checkpoint,
    /// The kernel cannot be resumed; a resume attempt fails with `Resume` naming the stage.
    Forbid,
}

/// How a Python kernel's invocations run; reported per stage in the run report.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum GilState {
    /// Invocations run concurrently on a free-threaded interpreter.
    FreeThreaded,
    /// Invocations are serialised by the GIL.
    Serialised,
}

/// What a kernel tells the controller about itself before it has been probed.
#[derive(Clone, Debug, Default)]
pub struct KernelHints {
    /// Expected peak working set over input bytes.
    pub expected_amplification: Option<f64>,
    /// True when `apply` allocates device memory.
    pub uses_device_memory: bool,
    /// For a Python kernel: whether `apply` releases the GIL.
    pub releases_gil: Option<bool>,
    /// A row count the kernel would rather receive.
    pub preferred_rows: Option<u64>,
    /// How instances come back on resume.
    pub resume: ResumePolicy,
    /// Bytes one instance's state is expected to hold (a model's weights); seeds the
    /// controller's state term before the first `footprint` is observed (RC f.3).
    pub state_bytes: Option<u64>,
}

/// Per-instance state a stateful kernel keeps between `apply` calls.
pub trait KernelState: Send {
    /// The concrete state, for the kernel's own use inside `apply`.
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any;
    /// Serialise the state for the run manifest. Called only when the kernel's
    /// `ResumePolicy` is `Checkpoint`; the default returns `Ok(None)`, which the
    /// scheduler treats as "nothing to save" for `Reinit` kernels and as an error
    /// for `Checkpoint` kernels (a `Checkpoint` kernel must return `Some`).
    fn checkpoint(&mut self) -> crate::Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Bytes this instance's state holds right now (a loaded model, an accumulator).
    /// `None` means unknown, and the default is `None`. Read by the scheduler after
    /// every `apply` into `TraceRecord::state_bytes`, so the controller can see
    /// state that grows with morsels seen and budget for it (RC f.3); a kernel
    /// whose state accumulates should implement it, because the linear model
    /// `a_k * bytes_in` does not describe it and the alternative is a breach.
    fn footprint(&self) -> Option<u64> {
        None
    }
}

/// Unit state for stateless kernels.
pub struct NoState;

impl KernelState for NoState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

/// What a kernel instance is built with.
pub struct InitCtx {
    /// Instance index, `0..max_instances`.
    pub instance: usize,
    /// Assigned device for `uses_device_memory` kernels.
    pub device: Option<DeviceId>,
    /// The arena, for a kernel that allocates payload-sized buffers.
    pub alloc: Arc<dyn Allocator>,
}

/// The user's transformation (component 5 adapts foreign ones).
pub trait Kernel: Send + Sync + 'static {
    /// Stable identity plus configuration hash (e.6).
    fn fingerprint(&self) -> Fingerprint;
    /// Stateless or stateful.
    fn kind(&self) -> KernelKind;
    /// What the controller should assume before the probe.
    fn hints(&self) -> KernelHints {
        KernelHints::default()
    }
    /// What this kernel wants delivered.
    fn accepts(&self) -> PayloadSpec;
    /// Output schema for the given input schema; errors are plan-time errors.
    fn output_schema(&self, input: &SourceSchema) -> crate::Result<SourceSchema>;
    /// Once per instance, on the worker that will own the instance.
    fn init(&self, ctx: &InitCtx) -> crate::Result<Box<dyn KernelState>>;
    /// Rebuild an instance from bytes `KernelState::checkpoint` produced. Called
    /// instead of `init` on resume, only for `ResumePolicy::Checkpoint` kernels.
    /// The default refuses; a kernel that declares `Checkpoint` must override it.
    fn restore(&self, ctx: &InitCtx, state: &[u8]) -> crate::Result<Box<dyn KernelState>> {
        let _ = (ctx, state);
        Err(MorunaError::Resume(
            "kernel declares Checkpoint but does not implement restore".into(),
        ))
    }
    /// Synchronous; may take seconds; must not spawn threads that outlive the call;
    /// safe to call concurrently on different `state`s. Returns a resident payload.
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> crate::Result<Payload>;
}
