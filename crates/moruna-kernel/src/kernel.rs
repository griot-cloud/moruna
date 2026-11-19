//! The kernel contract (contracts d.7).

use std::sync::Arc;

use crate::Result;
use crate::buffer::Allocator;
use crate::declare::Declared;
use crate::error::MorunaError;
use crate::fingerprint::Fingerprint;
use crate::ids::{DeviceId, Seq};
use crate::payload::{Payload, PayloadSpec, SourceSchema};

/// How long a morsel a kernel gave took, as the cost model is told it (item 6.3m, decision 4).
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum MorselTime {
    /// The call that gave it, as the scheduler measures it around the call: its wall time and its
    /// worker's CPU time. What a kernel that gives each morsel in the call that brought it says
    /// ([`Kernel::apply_morsels`]'s default), exactly as before.
    Unit,
    /// The wall and CPU nanoseconds the kernel measured for the morsel itself: a pipelined
    /// stage's, its pieces' function times summed as its task reported them.
    Given {
        /// Wall nanoseconds.
        wall_ns: u64,
        /// CPU nanoseconds.
        cpu_ns: u64,
    },
    /// No time: the kernel has none for it (a ring task that stamps no time on its pieces). The
    /// cost model is given no observation of the morsel rather than a wrong one; its trace record
    /// and the stage's learned growth stand.
    Untimed,
}

/// A morsel a call of a kernel completed (item 6.3m): its number, the output that answers it,
/// and how long it took. A stage gives a morsel's output once, under that morsel's number,
/// when the morsel's last answer has come.
#[derive(Debug)]
pub struct Completed {
    /// The morsel's number: the one the stage was given it under.
    pub seq: Seq,
    /// Its whole output.
    pub output: Payload,
    /// How long it took, for the cost model.
    pub time: MorselTime,
}

impl Completed {
    /// Morsel `seq`'s `output`, given in the call that brought it and timed as that call.
    pub fn unit(seq: Seq, output: Payload) -> Completed {
        Completed {
            seq,
            output,
            time: MorselTime::Unit,
        }
    }
}

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

/// What one call of a kernel asked for from one source (contracts d.7; 05 b "call counts";
/// E13). A field a source cannot measure is zero here and absent in the run report.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub struct AllocCounts {
    /// Bytes requested in total.
    pub bytes: u64,
    /// Number of requests.
    pub requests: u64,
    /// The largest single request.
    pub largest: u64,
    /// The most held at once during the call.
    pub peak: u64,
    /// Requests the allocator guard refused.
    pub refused: u64,
}

impl AllocCounts {
    const ZERO: AllocCounts = AllocCounts {
        bytes: 0,
        requests: 0,
        largest: 0,
        peak: 0,
        refused: 0,
    };
}

/// One call's counts per source, carried on its trace record (contracts d.7, d.13).
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub struct KernelAlloc {
    /// False for a call nothing counted (a Rust kernel, no kernel).
    pub measured: bool,
    /// Whether the allocator guard could refuse this call's requests.
    pub refusal_on: bool,
    /// CPython's allocator domains (Python objects); `peak` is unmeasured.
    pub python: AllocCounts,
    /// NumPy's data allocator.
    pub numpy: AllocCounts,
    /// pyarrow's default pool, from its statistics; `largest` and `refused` are unmeasured.
    pub arrow: AllocCounts,
    /// A wasm instance's linear memory (17 e.2): each growth a request, `peak` the memory's size
    /// at the call's end.
    pub linear: AllocCounts,
    /// Microseconds the call waited for a region of its runtime's shared mapping to be given
    /// back by its kernel (17 SB-I13): the backpressure of a kernel that keeps its inputs.
    pub lease_wait_us: u64,
}

impl KernelAlloc {
    /// The default, as a constant.
    pub const NONE: KernelAlloc = KernelAlloc {
        measured: false,
        refusal_on: false,
        python: AllocCounts::ZERO,
        numpy: AllocCounts::ZERO,
        arrow: AllocCounts::ZERO,
        linear: AllocCounts::ZERO,
        lease_wait_us: 0,
    };
}

thread_local! {
    static LAST_ALLOC: core::cell::Cell<KernelAlloc> = const { core::cell::Cell::new(KernelAlloc::NONE) };
}

/// The kernel's half of the hand-off (contracts d.7): the counts of the call that is about to
/// return, on the worker thread that made it.
pub fn set_kernel_alloc(counts: KernelAlloc) {
    LAST_ALLOC.with(|slot| slot.set(counts));
}

/// The scheduler's half: what the last `set_kernel_alloc` on this thread left, once; the
/// default when nothing did.
pub fn take_kernel_alloc() -> KernelAlloc {
    LAST_ALLOC.with(|slot| slot.replace(KernelAlloc::NONE))
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
    /// The stage's measured line, where the run has one (system design 3A; item 6.3w): what the
    /// job, the registry or the run's store measured of the stage's function. Set by the run,
    /// not by a kernel. The controller's anon fit takes its slope where its window cannot
    /// resolve one (RC f.3).
    pub measured_line: Option<MeasuredLine>,
}

/// A function's memory as a measured line over its batch (system design 3A), in the units the
/// run sizes the function's instances with: bytes held whatever the batch, and bytes held per
/// byte of input.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeasuredLine {
    /// Bytes held whatever the batch: the line's value at no input.
    pub fixed_bytes: f64,
    /// Bytes held beside each byte of the input batch: the line's slope.
    pub per_input_byte: f64,
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
    fn footprint(&self) -> Result<Option<u64>> {
        Ok(None)
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

/// A logical task's requested row boundaries inside one executable stage.
/// The ordered task list survives fusion; an empty port list requests no retained rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalTaskRows {
    /// Logical task identity from the checked document, never an executable stage number.
    pub task: String,
    /// Logical output ports whose exact rows must be delivered.
    pub ports: Vec<String>,
}

/// An engine-installed logical row tap, independent from any storage implementation.
/// A required capture's refusal is a real kernel error and must stop the run.
pub trait LogicalRows: Send + Sync {
    /// One exact logical task/output-port batch, before a later fused operation changes it.
    fn append(
        &self,
        task: &str,
        port: &str,
        batch: &arrow::record_batch::RecordBatch,
    ) -> crate::Result<()>;
}

/// The user's transformation (component 5 adapts foreign ones).
pub trait Kernel: Send + Sync + 'static {
    /// Install exact logical-row taps before execution. Opaque kinds refuse required capture;
    /// they must not pretend final executable output is every fused task's intermediate output.
    /// Supported kinds return a run-local bound executable after validating their actual fused
    /// representation; the shared original must remain untouched for other concurrent runs.
    fn with_logical_rows(
        &self,
        _tasks: &[LogicalTaskRows],
        _rows: Arc<dyn LogicalRows>,
    ) -> crate::Result<Arc<dyn Kernel>> {
        Err(MorunaError::Unsupported(
            "kernel has no logical row boundary capability",
        ))
    }

    /// Stable identity plus configuration hash (e.6).
    fn fingerprint(&self) -> Fingerprint;
    /// Stateless or stateful.
    fn kind(&self) -> KernelKind;
    /// What the controller should assume before the probe.
    fn hints(&self) -> KernelHints {
        KernelHints::default()
    }
    /// Whether the reported instance state may retain bytes in the run's arena.
    /// False is reserved for state held entirely outside that arena, such as an
    /// image process. Its footprint still counts against the run's memory ceiling.
    fn state_uses_arena(&self) -> bool {
        true
    }
    /// The kernel's declared input and output schemas (MH 4.9). Optional: the default
    /// declares nothing, which leaves the kernel runnable and not checkable. `moruna check`
    /// (MH 4.9) checks the kernel against it; a run never does, and reads it only to find what
    /// that check measured of the kernel, which is kept against the declared input (system
    /// design 3A).
    fn declared(&self) -> Declared {
        Declared::default()
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
    /// safe to call concurrently on different `state`s. Returns a resident payload: the output
    /// of `input`'s own morsel. The scheduler reaches it through [`Kernel::apply_morsels`], whose
    /// default calls it, on a stage of one input.
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> crate::Result<Payload>;
    /// One unit of a stage of one input (contracts d.7, amended 2026-11-19 by item 6.3m): `input`
    /// is morsel `seq`'s, and the call returns the morsels it completed, each under its own
    /// number with its whole output: none, one or several. A morsel is given once, when its last
    /// answer has come, and never under another morsel's number. A pipelined kernel, which keeps
    /// a morsel's pieces in flight past the call that brought them (a ring stage's task,
    /// `moruna_micro::task`), overrides it: a unit then gives the morsels whose last pieces were
    /// answered while it ran, and the ones still open come with later units or from
    /// [`Kernel::finish_morsels`]. The scheduler pushes each morsel downstream under its number
    /// and records it then: its trace record (its rows and bytes in, kept from when it was taken,
    /// and its rows and bytes out), the stage's learned growth, and the cost model's observation
    /// by its [`MorselTime`]. A unit that completes nothing pushes and records nothing; its
    /// worker's time still counts as the stage's busy time. A number the stage was not given, or
    /// one given twice, fails the unit as a kernel error under the unit's own number.
    ///
    /// The default calls [`Kernel::apply`] and gives the unit's own morsel, timed as the call
    /// ([`MorselTime::Unit`]): a kernel that is not pipelined behaves exactly as before. A stage
    /// of several inputs, or one that joins its inputs by morsel, is not called here
    /// ([`Kernel::apply_port`], [`Kernel::apply_aligned`]): each of its units gives its own
    /// morsel.
    fn apply_morsels(
        &self,
        state: &mut dyn KernelState,
        input: Payload,
        seq: Seq,
    ) -> crate::Result<Vec<Completed>> {
        Ok(vec![Completed::unit(seq, self.apply(state, input)?)])
    }
    /// The end of the input (contracts d.7, amended 2026-10-28 and 2026-11-04): called once per
    /// live instance of a stateful stage, in instance order, once every input of the stage has
    /// ended and its last `apply` has returned, and before the stage's output queues close. A
    /// kernel that runs something apart (an image's process) ends its input here and answers for
    /// how it ended; a kernel that must see the whole run before it gives anything (one that
    /// collects every batch) gives it here. A payload it returns goes downstream like the output
    /// of an `apply`, numbered after every number the stage's input could carry (d.16); `None`
    /// gives nothing. An `Err` ends the run. The default gives nothing, which is every kernel that
    /// holds nothing apart. The scheduler reaches it through [`Kernel::finish_morsels`], whose
    /// default calls it.
    fn finish(&self, state: &mut dyn KernelState) -> crate::Result<Option<Payload>> {
        let _ = state;
        Ok(None)
    }
    /// The end of the input, as [`Kernel::finish`], for a kernel that gives morsels (item 6.3m):
    /// the morsels still open in this instance at the stage's end, each under its own number with
    /// its whole output, and anything the kernel gives of its own under `after`, the number after
    /// every one the stage's input could carry (d.16; the next instance's `after` is one more for
    /// each such number used). The scheduler records a morsel the stage was given as
    /// [`Kernel::apply_morsels`] does. Any other number at or past the stage's first `after` is
    /// the stage's own output, as a `finish`'s payload is; one below it that the stage was not
    /// given, or gave already, ends the run as a kernel error.
    ///
    /// The default calls [`Kernel::finish`] and gives its payload under `after`, as before.
    fn finish_morsels(
        &self,
        state: &mut dyn KernelState,
        after: Seq,
    ) -> crate::Result<Vec<Completed>> {
        Ok(self
            .finish(state)?
            .map(|output| Completed::unit(after, output))
            .into_iter()
            .collect())
    }
    /// How many input ports the kernel takes at most (d.16): a stage that takes several inputs
    /// numbers them from 0 and is refused at build when it has more than this. One for a
    /// kernel of one input, which is every kernel that does not say otherwise.
    fn max_inputs(&self) -> u16 {
        1
    }
    /// `apply` for a batch that came in on input port `port` (d.16). A kernel that takes one
    /// input leaves it alone: port 0 is `apply`, and a stage of one input never sees another
    /// port. A kernel that takes several overrides it and is told, on every call, which input
    /// the batch is; the batches of different ports arrive in no particular order, each once.
    fn apply_port(
        &self,
        state: &mut dyn KernelState,
        port: u16,
        input: Payload,
    ) -> crate::Result<Payload> {
        match port {
            0 => self.apply(state, input),
            other => Err(MorunaError::Plan(format!(
                "the kernel takes one input and was handed a batch on its port {other}"
            ))),
        }
    }
    /// Whether the kernel joins its inputs by morsel (d.16, 2026-11-04): instead of one
    /// `apply_port` per batch on each port, the stage waits until the morsel with one number has
    /// arrived on every port and hands them over together to [`Kernel::apply_aligned`], whose one
    /// output keeps that number. Every input of such a stage must carry the morsels of one
    /// upstream, numbered alike, which the topology checks ([`crate::Topology::align`]). `false`
    /// for every kernel that does not say otherwise.
    fn aligns_inputs(&self) -> bool {
        false
    }
    /// `apply` for the morsel of one number from every input port, port 0 first (d.16), for a
    /// kernel whose [`Kernel::aligns_inputs`] is `true`. The default refuses, for every other.
    fn apply_aligned(
        &self,
        state: &mut dyn KernelState,
        inputs: Vec<Payload>,
    ) -> crate::Result<Payload> {
        let _ = (state, inputs);
        Err(MorunaError::Plan(
            "the kernel does not join its inputs by morsel and was handed them joined".into(),
        ))
    }
    /// The output schema for the given input schemas, port 0 first (d.16); errors are plan-time
    /// errors. A kernel of one input leaves it alone: one schema is `output_schema`.
    fn output_schema_of(&self, inputs: &[SourceSchema]) -> crate::Result<SourceSchema> {
        match inputs {
            [one] => self.output_schema(one),
            many => Err(MorunaError::Plan(format!(
                "the kernel takes one input and is given {}",
                many.len()
            ))),
        }
    }
    /// The checkpoint of `instance` while it is inside `apply` and waiting on a port (MH 4.10):
    /// its state as it was when that `apply` began, which is what a resume restores before the
    /// morsel is applied again. `None` when the instance is not waiting on a port, and for every
    /// kernel that calls no port (the default); the checkpoint then acquires the instance as
    /// for any `Checkpoint` kernel (10 f.12).
    fn waiting_state(&self, instance: usize) -> Option<Vec<u8>> {
        let _ = instance;
        None
    }
    // stream P (architecture/data-os.md section 5): a native stage's instances fed in turn.
    /// Whether the stage's free instances take its batches in turn, whichever worker brings
    /// them (f.4). For a kernel whose instances run apart from the engine and keep batches in
    /// flight of their own, an image's task processes: such an instance has no cache on a
    /// worker to keep warm, and one worker can keep every instance busy. `false` for every
    /// kernel that does not say otherwise: a worker keeps the instance it last held.
    fn deals_instances(&self) -> bool {
        false
    }
    /// How many of a stateful kernel's instances may be given work now, the first ones; the
    /// rest are started, and given work, once it rises (system design 3A: a function measured by
    /// nothing yet is warmed up on its first instance, and the others are sized from what that
    /// finds). A worker that finds none of a stage's ready instances free takes other work, so
    /// only that stage waits. Every instance, for a kernel that does not say otherwise.
    fn instances_ready(&self) -> usize {
        usize::MAX
    }
    /// The largest morsel, in input bytes, one of this kernel's instances can take inside its own
    /// room (system design 3A: a function's measured memory bounds its morsel, piece <= (room -
    /// fixed) / per-byte): `None` when nothing bounds it but the run's memory, which the
    /// controller's inequalities already hold it to. The controller holds the stage's target to
    /// it, below `morsel_min` if it must, since a smaller morsel is only slower and a larger one
    /// fails the instance; and the kernel cuts a larger batch into slices of it, since the target
    /// bounds only what the source reads for its first consumer (PM, 2026-11-18). Read on every
    /// tick: a warm-up's measurement may lower it.
    fn morsel_bytes_max(&self) -> Option<u64> {
        None
    }
}
