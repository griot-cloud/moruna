# Moruna SDD 05: Kernel adapters (`moruna-adapters`)

**Document type:** software design document, component 5 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/moruna-runtime-design.md` section 5.3 (Python kernels, portability), 6 (inside another engine); decisions D4, D9; criteria S7, S8, S13; global invariants G-I2, G-I9
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.3 (`Allocator::contains`, `AllocStats`), d.4, d.7 (`Kernel`, `KernelState`, `KernelHints`, `ResumePolicy`, `KernelAlloc`, `set_kernel_alloc`), d.12 (`Sampler`, `Sample`), d.14 (`MorunaError::Refused`), e.2, e.6
**Component location:** `crates/moruna-adapters` (the Python adapter, feature `python`); `crates/moruna-polars` and `crates/moruna-datafusion` (the two engine bridges, thin crates, preamble 6.1)
**Consumes:** contracts (1) only. **Consumed by:** python surface (12), scheduler (10, as `Kernel` objects), external Polars and DataFusion users

**Decisions worth your eye:** (1) a Python kernel's output that was allocated by pyarrow or Torch on the host is copied once into the arena at the boundary, counted as a boundary copy, because downstream DMA needs arena buffers; device tensors are wrapped, not copied; (2) under a GIL build the adapter serialises Python kernels with its own mutex rather than relying on the interpreter, so the scheduler's accounting sees the serialisation; (3) a Python kernel's fingerprint is the qualified name plus a hash of its source text, so editing the function invalidates its profile; (4) a stateful Python kernel's state is whatever `setup` returned, passed back to `__call__` explicitly, so the adapter never copies or clones Python objects to make instances. (5) (E13, 2026-10-02) a Python kernel's own requests through CPython's allocator domains and NumPy's data allocator are counted per call while its `apply` runs, with Arrow's from its pool's statistics, into the trace and the run report, and a request that would pass the process's ceiling is refused before the memory exists, as `MemoryError` in Python and `MorunaError::Refused` from `apply`; `memory_guard=False` in the decorator turns refusal off for that kernel and leaves counting on (AD-I8 to AD-I15, f.9 to f.14).

---

## a. Purpose and boundary

Adapters make things that are not Rust `Kernel`s into `Kernel`s, and make Rust `Kernel`s usable inside other engines. Three adapters: the Python adapter (a callable or an object with `setup` and `__call__` becomes a `Kernel`), the Polars bridge (a `Kernel` becomes a Polars expression plugin), and the DataFusion bridge (a `Kernel` becomes a `ScalarUDF`). The Python adapter is the one that carries design weight: it is where the zero-copy claim meets the interpreter, and where the GIL is detected and contained.

It owns: crossing payloads into and out of Python without copying; the boundary copy into the arena when a kernel returns non-arena host memory; GIL detection and serialisation; Python kernel fingerprints; the two engine bridges.

It refuses to know: scheduling (which worker runs what); sizing; anything about the source or sink; the user's API (the surface's).

## b. Vocabulary

**Callable kernel.** A plain Python callable `f(batch) -> batch`; stateless. The decorator's `stateful=False` (the default) selects this shape.

**Class kernel.** A Python object with `setup(self, ctx) -> state` and `__call__(self, state, batch) -> batch`, and optionally `checkpoint(self, state) -> bytes`, `restore(self, ctx, data) -> state` and `footprint(self, state) -> int | None`; selected by `stateful=True`. `ctx` is an object exposing `instance` (int) and `device` (`"cuda:N"` or `None`). The object itself is shared by every instance and is never copied; the per-instance state is the value `setup` returned.

**State.** The Python value `setup` (or `restore`) returned for one instance; held in a `PyState` (e.1) and passed as the first argument of every `__call__`.

**Crossing.** Handing a payload to Python (export) or taking one back (import) by pointer: Arrow through the C Data Interface via `pyarrow`, tensors through DLPack via `__dlpack__` and `from_dlpack`.

**Boundary copy.** The single copy of a returned payload into arena memory when its buffers are not arena-owned and are on the host.

**Attach.** Making the current thread able to call into the interpreter: PyO3's `Python::attach` (free-threaded) or GIL acquisition (GIL build).

**Allocator guard (E13).** The thin layer Moruna puts in front of the allocators a Python kernel's own code uses, through each library's official hook: CPython's allocator domains (PEP 445, `PyMem_SetAllocator`) and NumPy's data allocator (NEP 49, `PyDataMem_SetHandler`). It counts what every Python kernel asks for while its `apply` runs, per call and per source, for the trace and the run report, and, for a guarded kernel, refuses a request that would take the process past its ceiling before the memory exists. Decided by the owner on 2026-10-02 (`DECISIONS.md`, E13).

**Guarded kernel.** A Python kernel whose decorator left `memory_guard=True` (the default): its requests can be refused. A kernel with `memory_guard=False` is unguarded: it is counted exactly like a guarded one and never refused (AD-I10).

**Gate.** One per run (`MemoryGate`): the run's sampler, the ceiling in force, the process's memory at the last measurement (`base`) and the guarded bytes admitted since that measurement (`since`). It decides every guarded request (f.9).

**Frame.** The thread-local record the adapter opens on the worker thread for the length of one Python kernel's attached half of `apply` (f.12), guarded or not: the kernel being applied, its gate when it is guarded and bound, the call's counts, the first refusal inside the call, and a flag set while the guard itself is calling the allocator underneath it. A request is attributed to a kernel only through a frame.

**Call counts.** What one call of a Python kernel asked for, per source: Python objects (CPython's allocator domains), NumPy data, and Arrow (pyarrow's default memory pool, measured from the pool's own statistics around the call, f.14). For each: bytes requested in total, the number of requests, the largest single request, the most held at once during the call, and the number refused. `KernelAlloc` (contracts d.7) carries them to the call's trace record (d.13), and the run report aggregates them per stage (04 f.2), so the report stays a function of the trace (Brackly, 2026-10-02: the figures are a basis for judging a function's design, whose ideal asks for little or nothing outside Arrow).

**Guarded request.** A request of at least `guard.min_bytes` (i) through a hooked allocator, made inside the frame of a guarded kernel bound to a gate: the only kind the gate examines. Every request inside a frame is counted, whatever its size.

**Refusal.** A guarded request the gate declined: the hook returns `NULL`, which the library raises as `MemoryError`; the frame keeps the request, the memory in use and the ceiling, which become the run's diagnostic (f.12).

## c. Invariants

**AD-I1. Crossings do not copy.** Exporting a payload to Python and importing the returned object allocate nothing of payload size; measured by `AllocStats.payload_copies_total` staying constant across a crossing. Upholds G-I2, S13.

**AD-I2. At most one boundary copy per morsel, and only host-side.** A returned host payload whose buffers are not arena-owned is copied exactly once into the arena; a returned device tensor is never copied by the adapter; the copy is counted in `AllocStats.boundary_copies_total` (contracts d.3).

**AD-I3. GIL state is a fact, not a hope.** At adapter construction, `sys._is_gil_enabled()` is read; if true, `gil_state()` reports `Serialised` and the adapter serialises `apply` with a mutex; if false, it reports `FreeThreaded` and `apply` runs concurrently. The state is re-checked after the first `apply` (an import may have re-enabled the GIL) and a change is reported. Upholds G-I9, S8.

**AD-I4. Interpreter access is scoped.** No thread holds an attachment across a call into the arena, the placement engine or the reactor; the attachment covers export, call and import only.

**AD-I5. Deleters attach.** A `ManagedTensor` or Arrow buffer whose bytes are owned by a Python object is dropped by first attaching to the interpreter, from whatever thread drops it.

**AD-I6. Bridges are pure wrappers.** `moruna-polars` and `moruna-datafusion` contain no kernel logic; each converts its host's batch representation to `Payload` and back, calls `Kernel::apply` with `NoState`, and returns. Upholds S7.

**AD-I7. Exceptions become errors with context.** A Python exception in `apply` becomes `MorunaError::Kernel { stage, seq, msg, features }` with `msg` exactly `"<type>: <message>\n<traceback>"` (the exception's qualified type name, `str(exc)`, a newline, then `traceback.format_exception` joined); the worker never sees a panic. The same format is used for an exception in `setup`, `checkpoint`, `restore` or `footprint`.

**AD-I8. A guarded request past the ceiling is refused before the memory exists.** A guarded request of `n` bytes is admitted only when `in_use + n <= ceiling`, where `ceiling` is the ceiling in force (`Sample::ceiling_bytes`, the run's `Limits::memory_ceiling` until a sample names another) and `in_use` is the gate's estimate of the process's memory (f.9); when the estimate says no, the gate measures the process before it refuses, so a refusal is always judged on a fresh measurement. A refused request returns `NULL` from the hook and nothing is allocated. The line is the ceiling and not a tighter one: the controller's working-set target, its breach handling and the arena's budget are unchanged (architecture section 8), so a kernel that today passes the controller's target without passing the ceiling (a soft overrun) behaves exactly as it does without the guard; and the ceiling is at least 5% below the kill line wherever one exists (DS-I2), so the refusal comes before the host's signal. Upholds G-I1, G-I8.

**AD-I9. A request is charged to the kernel whose `apply` is running on its thread, and to that call.** Attribution is by the frame (b): a call's counts include exactly the requests made on the worker's thread while that call's frame was open, whichever worker it was and however many workers ran the kernel at once, and they reach that call's trace record and no other. A NumPy block is credited back, at free, to the kernel whose handler allocated it, from whatever thread frees it (f.11): a free on the call's own thread while its frame is open lowers what the call holds, so its peak is what it held at once.

**AD-I10. `memory_guard=False` turns refusal off and nothing else.** An unguarded kernel opens its frame and installs its NumPy handler like a guarded one and is counted the same, so its figures are in the report beside every other function's; the gate never examines its requests, so they reach the allocator underneath unrefused, and the run behaves for it as it did before E13. Counting costs a thread-local read and a few non-atomic additions per request (AD-T21 measures it), which is not what the keyword exists to avoid; refusing is. Each record says whether refusal was on for its call (`KernelAlloc::refusal_on`). The switch is honoured at the hook: the Python domain hooks are process-wide (f.10) and decide per request from the frame.

**AD-I11. The guard is inert outside a frame.** On a thread with no open frame (the runtime's own threads, a kernel's own threads, a kernel's thread between calls, any thread after the run) a request through a Python domain hook costs a thread-local read and goes to the allocator underneath; it is never counted and never refused. A NumPy block allocated through a kernel's handler is still credited to the kernel's live count when it is freed outside a frame.

**AD-I12. The hook keeps an allocator's constraints.** The code that runs on an allocation takes no lock on its fast path (atomics only; the gate's measurement on the slow path takes the sampler's lock, f.9), allocates nothing from the allocators it hooks, calls no Python API, needs no attachment and no GIL (CPython calls `PYMEM_DOMAIN_RAW` without one), is safe on the free-threaded build, and is reentrant-safe (a hooked allocator that calls another hooked allocator underneath is not charged twice, f.10). Every block is freed by the allocator it came from: the Python domain hooks never wrap `free` or `realloc` (they install the allocator underneath's own functions for both), and a NumPy block is freed through the handler that allocated it, which NumPy records on the array (NEP 49).

**AD-I13. A refusal ends as a diagnostic, never as a signal.** When the call into Python fails after a refusal in its frame, `apply` returns `MorunaError::Refused` (contracts d.14) naming the kernel, the bytes requested, the bytes in use and the ceiling of the first refusal of that call; the scheduler fills in the stage, the morsel and its features (SC f.8) and the error policy decides what follows (f.13). A call that catches `MemoryError` and returns normally has recovered: its output is used and the refusal is counted (the record's refused counts, `LiveCounts::refusals`). Upholds G-I8.

**AD-I14. The guard costs the runtime nothing.** No allocation the runtime makes passes through the guard: the hooks are CPython's and NumPy's allocators only, and Moruna installs no process-wide allocator of its own (architecture section 8, F8.9). The hooks are installed the first time a Python kernel is bound, and a run with no Python kernel installs nothing. The fast path of a hook is counters: thread-local additions for the call's counts, and atomics only for the gate and a NumPy block of at least `guard.min_bytes`.

**AD-I15. Arrow's share is measured, not hooked.** pyarrow's memory pool cannot be hooked from Moruna's wheel (o, AD-O6), so a call's Arrow figures come from the default pool's own statistics read before and after the call (f.14): bytes requested and requests exactly, the most held at once as a lower bound, the largest request and refusals unmeasured (reported as absent, never as zero). The pool is the process's, so with several workers running at once a call's Arrow figures include what the others asked for in the same interval; the report says so (04 f.2).

## d. Interfaces

### d.1 Exposed

```rust
// crate moruna-adapters, feature "python"
pub struct PyKernelSpec {
    pub callable: pyo3::Py<pyo3::PyAny>,    // the plain callable (stateless) or the class instance (stateful), b
    pub stateful: bool,
    pub instances: core::num::NonZeroUsize, // KernelKind::Stateful { max_instances }; ignored when !stateful
    pub device_memory: bool,                // KernelHints.uses_device_memory
    pub accepts: PayloadSpec,               // from decorator args; default Table/Host
    pub releases_gil: Option<bool>,
    pub expected_amplification: Option<f64>,
    pub preferred_rows: Option<u64>,
    pub resume: ResumePolicy,               // decorator resume="reinit" | "checkpoint" | "forbid"
    pub state_bytes: Option<u64>,           // KernelHints.state_bytes, the declared state size (RC f.3)
}
pub struct PyKernel { /* private */ }
impl PyKernel {
    /// Builds the kernel from the spec alone, so the surface can construct it before any runtime
    /// component exists (12 e.1 `Configuring`). Reads the interpreter's GIL state (f.4), computes the
    /// fingerprint (e.4) and, when `spec.resume == ResumePolicy::Checkpoint`, checks that `callable`
    /// has `checkpoint` and `restore` attributes (`Plan` error naming the missing one otherwise, f.7).
    /// `Plan` also when `stateful` and `callable` lacks `setup` or `__call__`.
    pub fn new(spec: PyKernelSpec) -> Result<PyKernel>;
    /// The arena the boundary copy (f.3) allocates from. Called once by the facade in its "kernels
    /// built" step (12 f.1), after the arena exists and before the scheduler is constructed; `apply`
    /// before it is bound is a `Config { name: "adapter" }` error, never a copy to the global allocator.
    pub fn bind_allocator(&self, alloc: std::sync::Arc<dyn Allocator>);
    /// AD-I3. `Serialised` under a GIL build (or after a flip, f.4), `FreeThreaded` otherwise.
    pub fn gil_state(&self) -> GilState;
}
impl Kernel for PyKernel { /* contracts d.7; kind = Stateless or Stateful { max_instances: spec.instances } */ }

/// `GilState { FreeThreaded, Serialised }` is `moruna_kernel::GilState` (contracts d.7), the type the report carries.
pub use moruna_kernel::GilState;

pub fn python_gil_enabled() -> bool;        // sys._is_gil_enabled(); true on any interpreter without the attribute (f.4)
pub fn python_build_info() -> String;       // version, free-threaded flag, for the report

// crate moruna-polars
pub fn polars_plugin<K: Kernel>(kernel: K) -> impl Fn(&[polars::prelude::Series]) -> polars::prelude::PolarsResult<polars::prelude::Series>;

// crate moruna-datafusion
pub fn datafusion_udf<K: Kernel>(kernel: K, name: &str) -> datafusion::logical_expr::ScalarUDF;
```

`KernelHints` for a `PyKernel` are built from the spec field by field (`expected_amplification`, `uses_device_memory = device_memory`, `releases_gil`, `preferred_rows`, `resume`, `state_bytes`).

The allocator guard (E13; b, f.9 to f.14). `PyKernelSpec` gains one field and `PyKernel` four methods; the gate, the frame and the counting are in a module that needs no interpreter, so they are built and tested without the `python` feature.

```rust
// crate moruna-adapters, feature "python"
pub struct PyKernelSpec {
    /* ...the fields above... */
    pub memory_guard: bool,                 // decorator memory_guard=, default true (12 d.2); refusal only; not in the fingerprint (e.4)
}
impl PyKernel {
    /// The run's gate (f.9). Called by the facade in its "kernels built" step, beside
    /// `bind_allocator`, for every Python kernel; the first call in the process installs the
    /// Python domain hooks (f.10), under attachment. A second call is ignored, like
    /// `bind_allocator`'s. A kernel never bound (a check, 12 f.8) is counted and never refused.
    pub fn bind_gate(&self, gate: std::sync::Arc<moruna_adapters::guard::MemoryGate>);
    /// Whether the decorator left refusal on.
    pub fn memory_guard(&self) -> bool;
    /// What the guard holds for this kernel now: NumPy bytes live and their peak, refusals so
    /// far; readable at any moment from any thread (a consumer such as the controller, AD-O7).
    pub fn memory(&self) -> moruna_adapters::guard::LiveCounts;
    /// `<module>.<qualname>` of the function the author wrote (e.4's target), which the
    /// refusal diagnostic names.
    pub fn name(&self) -> &str;
}

// crate moruna-adapters, module guard (no feature)
pub const GUARD_MIN_BYTES: u64 = 64 * 1024;      // guard.min_bytes (i)
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Refusal { pub requested: u64, pub in_use: u64, pub ceiling: u64 }
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveCounts { pub live_bytes: u64, pub peak_bytes: u64, pub refusals: u64 }
pub struct MemoryGate { /* private: sampler, base, since, ceiling, counters */ }
impl MemoryGate {
    /// One gate per run, over the run's sampler, starting from `ceiling` (the run's
    /// `Limits::memory_ceiling`) and one measurement taken now.
    pub fn new(sampler: std::sync::Arc<dyn moruna_kernel::Sampler>, ceiling: u64) -> std::sync::Arc<MemoryGate>;
    /// f.9. `Ok` admits `bytes` (and counts them in `since`); `Err` refuses them.
    pub fn admit(&self, bytes: u64) -> Result<(), Refusal>;
    /// Bytes a counted block gave back (a NumPy free, f.11, or an admitted request whose
    /// allocator underneath failed).
    pub fn credit(&self, bytes: u64);
    /// How many times the slow path measured the process, and how many requests were refused.
    pub fn measurements(&self) -> u64;
    pub fn refusals(&self) -> u64;
}
pub struct KernelMemory { /* private: guarded, gate, live, peak, refusals */ }
impl KernelMemory {
    pub fn new(guarded: bool) -> std::sync::Arc<KernelMemory>;
    pub fn bind(&self, gate: std::sync::Arc<MemoryGate>);  // first call wins
    pub fn guarded(&self) -> bool;
    pub fn live(&self) -> LiveCounts;
    /// f.12: run `f` inside a frame for this kernel on the calling thread; returns `f`'s value,
    /// the call's counts (Python objects and NumPy data; Arrow is the adapter's, f.14) and the
    /// first refusal recorded in the frame. Frames nest: the previous frame is restored on exit.
    pub fn frame<R>(self: &std::sync::Arc<Self>, f: impl FnOnce() -> R) -> (R, FrameEnd);
}
pub struct FrameEnd { pub counts: moruna_kernel::KernelAlloc, pub refusal: Option<Refusal> }
/// f.10: what a Python domain hook does with a request for `bytes`; `underlying` calls the
/// allocator underneath. Returns `underlying()`'s pointer, or null for a refusal.
pub fn allocate<T>(bytes: u64, underlying: impl FnOnce() -> *mut T) -> *mut T;
/// f.11: the NumPy handler's three operations for a block owned by `owner`.
pub fn allocate_owned<T>(owner: &KernelMemory, bytes: u64, underlying: impl FnOnce() -> *mut T) -> *mut T;
pub fn resize_owned<T>(owner: &KernelMemory, old: u64, new: u64, underlying: impl FnOnce() -> *mut T) -> *mut T;
pub fn release_owned(owner: &KernelMemory, bytes: u64);
```

### d.2 Consumed

`moruna_kernel::{Kernel, KernelKind, KernelHints, KernelState, NoState, InitCtx, ResumePolicy, GilState, Payload, PayloadSpec, ManagedTensor, Fingerprint, Allocator, AllocStats, MorunaError}`; `pyo3` (0.28+, free-threaded default), `pyo3-arrow` (RecordBatch ↔ `pyarrow.RecordBatch` via C Data Interface), `dlpark` (DLPack capsules); `polars` in `moruna-polars` and `datafusion` in `moruna-datafusion`.

## e. Data model, formats and state machines

### e.1 Python kernel state

`PyState { state: Py<PyAny>, instance: usize, device: Option<DeviceId> }` implements `KernelState`. A callable kernel is `KernelKind::Stateless`: the scheduler never calls `init` for it (SC f.4: stateless stages have no instances), `apply` receives `NoState` and calls `callable(batch)`. A class kernel is `KernelKind::Stateful`: `init(ctx)` calls `callable.setup(ctx_obj)` under attachment and stores the returned value as `state`; `apply` calls `callable(state, batch)`. `ctx_obj` is a small frozen PyO3 object with attributes `instance` (`ctx.instance`) and `device` (`"cuda:N"` from `ctx.device`, else `None`). `KernelState::footprint` calls `callable.footprint(state)` when the attribute exists (an `int` becomes `Some`, `None` stays `None`, anything else is a `Kernel` error once and then `None`); `KernelState::checkpoint` and `Kernel::restore` are f.7. Nothing is deep-copied: `instances` independent states come from `instances` calls of `setup`.

### e.2 Export rules

| Payload | Python object handed to the kernel |
|---|---|
| `Table` on `Host`/`PinnedHost` | `pyarrow.RecordBatch` via C Data Interface (no copy) |
| `Table` on `Device` | `pyarrow` has no device batches; if the kernel declared `accepts.kind == Table` and `tier == Device`, the object is a capsule pair per the Arrow C Device Interface, exposed as an object with `__arrow_c_device_array__`; cuDF consumes this |
| `Tensor` on any tier | an object implementing `__dlpack__` and `__dlpack_device__` (a small PyO3 class wrapping `ManagedTensor`); `torch.from_dlpack`, `jax.dlpack.from_dlpack`, `cupy.from_dlpack`, `numpy.from_dlpack` all accept it |

### e.3 Import rules

| Returned object | Handling |
|---|---|
| `pyarrow.RecordBatch` | import via C Data Interface; tier inferred: if buffers are arena-owned (the kernel returned a slice of its input) no copy; else boundary copy into arena (AD-I2) |
| `pyarrow.Table` with one chunk | as RecordBatch; more than one chunk → `Kernel` error "return a RecordBatch or a single-chunk Table" (combining chunks would be a hidden copy) |
| object with `__dlpack__` | import via DLPack; device tensors wrapped with tier `Device(id)`; host tensors: boundary copy unless the data pointer lies in the arena |
| `None` | `Kernel` error "kernel returned None" |
| anything else | `Kernel` error naming the type |

### e.4 Fingerprint

`Fingerprint::compute(identity, config)` with `identity = f"py:{module}.{qualname}"` (of the callable, or of the class for a class kernel) and `config = blake3(inspect.getsource(callable) or code object co_code) || decorator args as canonical JSON` (every `PyKernelSpec` field except `callable`). If the source is unavailable (a lambda in a REPL), `co_code` plus `co_consts` repr is used and a note is recorded. `memory_guard` is the one spec field the fingerprint leaves out (2026-10-02): it changes neither what the kernel computes nor what it costs when it is not refused, so it does not invalidate a profile, and including it would have changed the fingerprint of every existing Python kernel, which hosts pin (MH 4.9).

### e.5 The guard's NumPy block

A block NumPy obtains through a kernel's guard handler (f.11) is `16 + n` bytes from NumPy's default handler, of which NumPy receives the address 16 bytes in: the first eight bytes hold `n` as a little-endian `u64` and the next eight are zero. Sixteen keeps the alignment the default handler gives (that of `malloc`, sixteen bytes on every supported platform). The header is how `realloc`, which NumPy calls with the new size only, learns the old one; `free` reads it too rather than trusting the size argument. A block is only ever handed back to the handler that made it (NEP 49 stores the handler on the array), so the header is never read in front of a pointer the guard did not produce.

## f. Algorithms and policies

**f.1 `apply` (free-threaded).** Attach; export input (e.2); call `callable(obj)` for a callable kernel or `callable(state, obj)` for a class kernel; import result (e.3); detach; perform the boundary copy if required (outside the attachment: the returned object is kept alive by a `Py<PyAny>` held until the copy completes, then released under a brief attachment). Record `releases_gil` observation: if the call returned in less wall time than CPU time consumed by other Python threads would allow, no inference is made; the hint is informational only.

**f.2 `apply` (GIL build).** Same, wrapped in the adapter's `Mutex<()>` so that the scheduler's `kernel_busy` for the stage reflects one-at-a-time execution and the report can say the stage is `Serialised`.

**f.3 Boundary copy.** `alloc(bytes, PinnedHost or Host)` from the allocator bound by `bind_allocator` (d.1; the host tier is the allocator's, `is_pinned()`); `memcpy` each Arrow buffer (or the tensor's bytes) into the new buffer(s); rebuild the batch or tensor over arena buffers with `Payload::table_with(batch, alloc)` so the tier is inferred through `tier_of`; drop the Python-owned originals under attachment. Count in `boundary_copies_total`. Skipped when the returned pointers are inside the arena (`Allocator::contains(ptr)`, contracts d.3).

**f.4 GIL detection.** `python_gil_enabled` calls `sys._is_gil_enabled` if present, else returns `true` (any interpreter without the attribute has a GIL). `PyKernel::new` records the result as the kernel's `gil_state`. Re-check after the first `apply` of each stage; if it flipped to true, set `gil_state` to `Serialised` for the rest of the run and note it. The facade reads `gil_state()` per Python stage into `RunMeta.gil` (04 d.1) after the run.

**f.5 Polars bridge.** Input `&[Series]` → one `RecordBatch` via Arrow FFI (Polars exposes `to_arrow` per series without copy for its native types); tier `Host`; `Kernel::apply(NoState)`; output batch → the first column as a `Series` (the plugin contract is one output series; multi-column kernels are not exposed through Polars, stated in docs).

**f.6 DataFusion bridge.** `ScalarUDFImpl::invoke_with_args` receives `ColumnarValue`s; build a `RecordBatch`; `apply`; return the first output column as `ColumnarValue::Array`.

**f.7 Resume policy for Python kernels.** `PyKernelSpec.resume` maps to `KernelHints.resume`. For `Checkpoint`, the class kernel must define `checkpoint(self, state) -> bytes` and `restore(self, ctx, data: bytes) -> state`; `PyKernel::new` checks both attributes (a `Plan` error naming the missing one otherwise, so the omission is found before any morsel is read) and the adapter implements `KernelState::checkpoint` (calls `checkpoint(state)` under attachment, copies the bytes out of the Python object before detaching, returns `Some`) and `Kernel::restore` (calls `restore(ctx_obj, data)` under attachment and stores the returned value as the new `PyState.state`). For `Reinit` (the default) and `Forbid` nothing is called. A stateful Python kernel that accumulates across morsels and leaves the default is a user error the documentation names in one sentence, and the resume path cannot detect; the `stateful=True` docstring says "if your state depends on the morsels seen, declare resume='checkpoint' or 'forbid'".

**f.8 Startup placement.** The surface constructs `PyKernel::new(spec)` while translating arguments, before discovery, so `Plan` errors from `new` surface before any component starts; the facade binds the allocator in the "kernels built" step of the facade's startup order (12 PY-I1) and the scheduler's `init_instances` (SC f.4) then calls `init` for every instance of every class kernel on the worker that owns it, so `setup` failures terminate before any morsel is read (architecture 7, "stateful init fails"; SC-T18).

**f.9 The gate.** State: `ceiling` (u64), `base` (u64, the process's memory at the last measurement, `Sample::anon_bytes`, the quantity the ceiling is judged on, DS-I4), `since` (i64, guarded bytes admitted since that measurement, less any that were admitted and then not allocated). `new` takes one sample for `base`. `admit(n)`: (1) `s = since.fetch_add(n) + n`; when `ceiling == 0` (no ceiling known) or `base + max(s, 0) <= ceiling`, admit; this is the fast path, two atomic operations and no lock. (2) Otherwise undo the add, measure: `t = since.load()`, `sample = sampler.sample()`, `since.fetch_sub(t)`, `base = sample.anon_bytes`, and `ceiling = sample.ceiling_bytes` unless it is zero; `in_use = base + max(since, 0)`. When `in_use + n > ceiling`, refuse with `Refusal { requested: n, in_use, ceiling }`; otherwise `since.fetch_add(n)` and admit. `credit(n)` is `since.fetch_sub(n)`, used only for an admitted request whose allocator underneath returned null. The estimate errs one way only: a block admitted but not yet touched is not resident, so a measurement that replaces `since` with what is resident can admit a later request that, once both are touched, passes the ceiling; it can never refuse a request a fresh measurement would admit. Freed blocks are never credited, from either source (a Python domain block's size is unknown at `free`, f.10, and crediting a NumPy block would need to know whether it was charged), so `since` overstates what is held until the next measurement, which only makes the slow path run sooner and never admits what a measurement would refuse. Two threads on the slow path at once each measure; the later measurement wins, which is the fresher.

**f.10 Python domain hooks.** Installed once per process, the first time a Python kernel is bound to a gate (d.1), under attachment, and never removed (the hook is inert outside a frame, AD-I11, so there is nothing to remove, and removing it would race with threads already inside it). For each of `PYMEM_DOMAIN_RAW`, `PYMEM_DOMAIN_MEM` and `PYMEM_DOMAIN_OBJ`: `PyMem_GetAllocator` saves the allocator underneath; `PyMem_SetAllocator` installs a struct whose `ctx`, `realloc` and `free` are the saved ones and whose `malloc` and `calloc` are the guard's. PEP 445 permits wrapping after initialisation (a hook must call through, which this does). Keeping `ctx` equal means a thread that reads the struct while it is being replaced calls either the old function or the new one with the right context. The guard's `malloc(ctx, n)` and `calloc(ctx, k, size)` (with `n = k * size`, and an overflowing product passed through for the allocator underneath to refuse) call `allocate(n, underlying)`: when no frame is open on the thread, or the frame's reentrancy flag is set, it calls `underlying()` and returns. Otherwise it counts the request in the frame's Python object counts (bytes, requests, largest), and when the request is guarded (b: at least `guard.min_bytes`, a guarded kernel, a gate bound) asks the gate; on a refusal it records it in the frame (the first one of the call is kept for the diagnostic, every one is counted) and returns null. Then it sets the reentrancy flag, calls `underlying()`, clears the flag, credits the gate back if a guarded request's allocator underneath returned null, and returns its pointer. The flag is what stops a `PyObject_Malloc` that pymalloc forwards to `PyMem_RawMalloc` (the GIL build's path for requests over 512 bytes) from being counted twice. The peak a call held in Python objects is not measured: PEP 445 does not give the size at `free`, so what is held cannot be followed, and the report says it is absent rather than estimating it (04 f.2). `realloc` is neither counted nor guarded, for the same reason: the growth a call adds is unknown, and checking the whole new size would refuse a list that grows by one element near the ceiling, a run that fits; growth by `realloc` stays with the sampler and the controller, as before E13 (o, AD-O5).

**f.11 NumPy's data allocator.** When the frame opens on a call (f.12) the adapter looks for NumPy among the loaded modules (`numpy._core._multiarray_umath`, else `numpy.core._multiarray_umath`; never importing it) and, once found, reads its C API table from `_ARRAY_API` once per process: `PyArray_GetNDArrayCFeatureVersion` (slot 211) must be at least `0x0f` (NumPy 1.22, NEP 49), `PyDataMem_SetHandler` is slot 304 and `PyDataMem_DefaultHandler` slot 306, whose `PyDataMem_Handler` (version 1) is the allocator underneath. A NumPy too old for NEP 49 leaves the kernel's NumPy requests uncounted and unguarded and a warning says so once (j). Each `PyKernel` makes one handler capsule (named `"mem_handler"`, handler name `"moruna_guard"`, version 1) whose context owns an `Arc` of the kernel's `KernelMemory` and a copy of the default handler; the capsule's destructor frees both, so the kernel's live count lives as long as the last array that can still free through it. Around the call the adapter makes it the thread context's handler (`PyDataMem_SetHandler(capsule)`, which returns the previous one) and restores the previous one afterwards, whatever the call did; NumPy contexts are per thread, so this touches no other worker. The handler's `malloc(n)` and `calloc(k, size)` call `allocate_owned(owner, n, ...)` over the default handler with `16 + n` bytes and write the header (e.5). `allocate_owned`, when a frame of the owner is open on the thread, counts the request in the frame's NumPy counts (bytes, requests, largest, and what the call holds, whose maximum is its peak) and, for a guarded request, asks the gate exactly as `allocate` does; a block of at least `guard.min_bytes` is also added to the owner's atomic `live` and `peak`. `realloc(p, n)` reads the old size from the header and calls `resize_owned(owner, old, n, ...)`, which counts and asks the gate for the growth only (the size after less the size before) and refuses by returning null, which leaves the old block valid, as `realloc` requires. `free(p)` reads the header and calls `release_owned(owner, old)`, which takes the block out of the owner's `live`, from any thread, frame or none, and lowers what the call holds when it happens inside the owner's frame on its thread.

**f.12 The frame around `apply`.** `PyKernel::apply` runs its attached half (export, the Arrow pool read, the NumPy handler in, the call, the NumPy handler back out, the Arrow pool read again, import) inside `KernelMemory::frame`, for every Python kernel. The frame is a value on the worker's stack whose address is placed in a thread-local slot for the length of the closure and replaced by the previous slot's value on exit, unwind included. After the closure the adapter completes the call's counts with Arrow's (f.14), marks them `measured` with `refusal_on = memory_guard() && bound`, and hands them to the scheduler through `moruna_kernel::set_kernel_alloc` on the worker thread, whatever the call's outcome; the scheduler takes them into the call's trace record (SC f.8, contracts d.7). Then: if the call failed and the frame holds a refusal, `apply` returns `MorunaError::Refused { stage: UNKNOWN_STAGE, seq: UNKNOWN_SEQ, kernel: name(), requested, in_use, ceiling, features: None }` in place of the Python exception, whatever the exception was (the refusal is its cause, and a kernel that catches `MemoryError` and raises something else is still the kernel that asked for too much); if the call succeeded, the refusal stays counted and a warning (`adapter.guard`) names it. The boundary copy (f.3) runs outside the frame: it allocates from the arena, which is the budget's own.

**f.13 What the error policy does with a refusal.** `Refused` is a failure of one morsel's `apply`, and SC f.8 treats it as it treats a kernel error: under `terminate` the run ends with the refusal as its diagnostic (so the run report's exit and the Python `BudgetError` name the kernel, the request, the memory in use and the ceiling, and the host's exit code is the budget's, MH 4.2); under `skip` the morsel is skipped, its trace record's `error` is the diagnostic and the run continues; under `budget(n)` it is one of the `n`. Nothing about a refusal is special to the policy, deliberately: the memory never existed, the process is intact, and whether losing that morsel is acceptable is what the user's policy already says. The controller is not told (AD-O7).

**f.14 Arrow from its pool's statistics.** pyarrow is imported for every crossing (e.2), so its default pool is always there to read. Under the same attachment as the call, the adapter reads `bytes_allocated()`, `max_memory()`, `total_bytes_allocated()` and `num_allocations()` of `pyarrow.default_memory_pool()` before the call and again after it. The call's Arrow counts are: bytes requested `total_after - total_before`, requests `num_after - num_before`, peak held `max(bytes_after - bytes_before, max_after - bytes_before when max_after > max_before, 0)` (a lower bound: `max_memory` is the pool's high-water mark for the life of the process, so a call's own peak shows only when it set a new mark), largest request and refusals unmeasured (zero in the record, absent in the report). A pool read that fails (a pyarrow without these methods) leaves the Arrow counts zero and a warning says so once; the rest of the call is unaffected. The pool is the process's: concurrent calls on other workers land in the same interval, so a call's Arrow figures are exact with one worker and an upper bound on its requests otherwise (AD-I15).

## g. Concurrency within the component

`PyKernel` is `Send + Sync`; each `apply` attaches on the calling worker thread. Under a GIL build, one adapter-level mutex per `PyKernel`. Deleters (AD-I5) attach on whatever thread drops; PyO3 0.28's `Py<T>` drop is safe from any thread when attached, which the deleter hook ensures.

The guard (f.9 to f.12): the gate's state and every kernel's counts are atomics shared by every worker; the frame is thread-local and read only by its own thread; the Python domain hooks' saved allocators are written once, before the hooks are installed, and read through a `OnceLock`; the NumPy table is a `OnceLock`. `peak` is raised with a compare-and-swap loop that only ever increases it. The gate's slow path is the only place a lock is taken (the sampler's, 03 g), and it is never held across a call into an allocator or into Python.

## h. Behaviour

**Normal path.** Decorator builds `PyKernelSpec`; surface constructs `PyKernel::new(spec)`; facade binds the allocator; scheduler's `init_instances` calls `init` per instance (class kernels only, eagerly, f.8) then `apply` per morsel; adapter crosses, calls, imports, copies at the boundary if needed.

**Edge cases.** Kernel returns its input unchanged: no copy (arena-owned). Kernel returns a batch with a different row count: allowed. Kernel returns a device tensor while `accepts.tier == Host`: allowed; the placement engine will demote it; the report notes the mismatch once. Kernel mutates the input batch in place (pyarrow forbids; Torch permits for tensors): documented as undefined for tables and permitted for tensors when the kernel returns the same tensor. `setup` returns `None`: allowed, `state` is `None` and is passed back as such. `footprint` absent: `KernelState::footprint` returns `None` and the controller falls back to `state_bytes` from the spec (RC f.3).

**Failures.** Python exception: AD-I7. `setup` raises: `Kernel` error at `init`, which `init_instances` returns before the run starts (SC f.4); nothing has been read or written. Interpreter finalising (process exit during cancellation): `apply` returns `Cancelled`. Import of a returned object fails (unsupported dtype): `Convert` error with the dtype named. `apply` before `bind_allocator`: `Config { name: "adapter" }` (a facade bug, not a runtime condition).

**The guard's failure modes.** A single request larger than the room left: refused, `MemoryError` in Python, `Refused` from `apply` (f.12). Many requests each smaller than the room left but larger together, made between two measurements by several workers while untouched: each is admitted against the estimate, `since` holds them all, so the next one to reach the slow path is judged with them; the window is one measurement wide, and memory touched after admission is the sampler's and the controller's, as before E13. Small requests (below `guard.min_bytes`) that add up inside one call: never refused by the guard, which is what keeps it off every small allocation; the sampler and the controller bound them as before E13 (architecture 8). A request through `realloc` in a Python domain: never refused (f.10). An allocation a kernel makes on a thread of its own: no frame, so not guarded; a NumPy array the thread allocates through a handler it inherited is still counted and credited (AD-I11). Memory from an allocator the guard does not hook (pyarrow's pool, PyTorch's CPU allocator, a C extension's `malloc`, a Rust kernel's global allocator): not guarded (o). The sampler fails to read on the slow path: the sampler repeats its last sample (DS-I3), so the decision is taken on that, which is the controller's position too. NumPy without NEP 49 (before 1.22): its requests are unguarded and a note says so once. A Python exception raised while restoring the NumPy handler after the call: the call's own result stands and the restore error is logged (`adapter.guard`, warn), because the context it was restoring dies with the attachment.

## i. Configuration

`python.allow_gil` (surface; the adapter only reports). `guard.min_bytes` (preamble section 5): 64 KiB, `compile`; the smallest request the guard examines.

## j. Observability

`AllocStats.boundary_copies_total`; per-kernel `PyKernelStats { calls, exceptions, boundary_copies, gil_state, source_available }`. `tracing`: `adapter.gil` (info at construction and warn on flip), `adapter.boundary_copy` (debug, bytes), `adapter.guard` (info when the Python domain hooks and a kernel's NumPy handler are installed, warn on a refusal a kernel recovered from and on a NumPy without NEP 49). Every Python call's counts reach its trace record as `KernelAlloc` (contracts d.7, d.13) and the run report aggregates them per stage (04 d.1 `StageReport::alloc`, f.2), so a host and an author can see per function whether refusal was on, what it asked for from each source, and the share of its peak outside Arrow; `moruna check` reports the same per kernel over its synthetic batches (MH 4.9). `PyKernel::memory()` is the live view between records.

## k. Tests

Python tests run under both a GIL and a free-threaded interpreter in CI (matrix). Tests that need an allocator use `FakeAllocator` (contracts d.15) with `with_limit(tier, bytes)` and `pinned(bool)` only, bound through `bind_allocator`; a batch "in the arena" is one built over `FakeAllocator` buffers.

**AD-T1 crossing_zero_copy.** Identity Python kernel on a 256 MiB batch built over `FakeAllocator` buffers; `payload_copies_total` unchanged; `boundary_copies_total` unchanged (returned input is arena-owned). AD-I1.

**AD-T2 boundary_copy_once.** Kernel returns a new pyarrow batch; exactly one boundary copy; bytes equal; the copied payload's tier is `PinnedHost` when the fake is `pinned(true)` and `Host` otherwise. AD-I2.

**AD-T3 device_no_copy.** (reference host, E1) Kernel returns a Torch CUDA tensor; no copies; tier is `Device`. AD-I2.

**AD-T4 gil_detected.** Under the GIL interpreter, `gil_state() == Serialised` and two concurrent `apply` calls do not overlap (timestamps); under free-threaded, `FreeThreaded` and they overlap. AD-I3.

**AD-T5 gil_flip.** Kernel imports a test extension that declares `gil_used = true`; the flip is detected after the first apply. AD-I3.

**AD-T6 deleter_attach.** Drop a Python-owned tensor from a non-Python thread; no crash; refcount reaches zero. AD-I5.

**AD-T7 exception_context.** Kernel raises `ValueError("x")`; `msg` starts with `"ValueError: x\n"` and the rest is the formatted traceback naming the kernel's source line; worker thread survives. AD-I7.

**AD-T8 fingerprint_source.** Editing one character of the kernel's source changes the fingerprint; same source, different decorator arg, different fingerprint. e.4.

**AD-T9 polars_bridge.** (integration, closes in wave 1) The `normalise` Rust kernel from the bench agent runs as a Polars plugin and inside Moruna with identical output on the same input. AD-I6, S7.

**AD-T10 datafusion_bridge.** (integration, closes in wave 1) Same for DataFusion. AD-I6, S7.

**AD-T11 speedup.** (reference host, E1; free-threaded only) A NumPy kernel that releases the GIL on 8 workers reaches ≥ 5.6× the single-worker throughput. S8.

**AD-T12 class_kernel_shape.** A class kernel whose `setup` returns a counter object: `init` is called `instances` times with `ctx.instance` 0..instances and distinct returned states; `apply` passes the matching state as the first argument; `footprint` returns the int the class reports and `None` when the method is absent; a class without `__call__` or `setup` is `Plan` at `new`; `resume=Checkpoint` without `restore` is `Plan` at `new` naming `restore`; with both, `KernelState::checkpoint` returns the bytes `checkpoint(state)` produced and `Kernel::restore` yields a state whose `__call__` output equals the original's. b, e.1, f.7.

The guard's tests (E13, 2026-10-02). AD-T13 needs no interpreter; the rest embed one with NumPy, under the `python` feature. Budgets and requests are relative to what the process holds when the test runs (a gate's ceiling is the measured memory plus a margin, a refused request is larger than the margin), never a fixed figure, so each holds on any host.

**AD-T13 gate_decides_on_a_fresh_measurement.** Over `FakeSampler::scripted`: a request that fits the estimate is admitted without a sample; one that does not is measured first, and is admitted when the fresh measurement leaves room (a scripted fall in `anon_bytes`) and refused when it does not, with `in_use` the fresh measurement (which replaces what was admitted before it) and `ceiling` the sample's; a sample with `ceiling_bytes == 0` keeps the gate's ceiling; a gate with ceiling zero admits everything; `credit` makes room again; a frame records only the first refusal, restores the previous frame on exit (nested frames), and an unbound or unguarded `KernelMemory` opens none; `allocate` below `guard.min_bytes`, outside a frame, and with the reentrancy flag set calls through uncounted; an admitted request whose allocator underneath returns null is credited back; `allocate_owned`, `resize_owned` and `release_owned` keep the owner's `live` and `peak` and the call's NumPy counts and peak exact, and a refused growth leaves `live` unchanged. f.9, f.10, f.11, AD-I8, AD-I11.

**AD-T14 numpy_blocks_counted.** A kernel that builds `numpy.ones` arrays of 1 MiB and 8 MiB and keeps one in its state while dropping the other: the call's NumPy counts have requests at least two, bytes at least both, largest at least 8 MiB and peak at least 9 MiB and below their sum plus what NumPy needs for the result; the kernel's `live_bytes` after the call is exactly the kept array's bytes and `peak_bytes` at least both; an array grown with `ndarray.resize` is counted for its growth; after the state is dropped the kept array's bytes leave `live`, and a kept array freed on another Python thread after the call is credited to the kernel. f.11, AD-I9, AD-I11.

**AD-T15 numpy_request_refused.** A guarded kernel asking for `numpy.empty` of more than the gate's room: the call raises `MemoryError` inside Python, `apply` returns `Refused` naming the kernel, the request (the array's bytes), the memory in use and the ceiling, and the process holds no new block of that size; the NumPy handler in the worker's context after the call is the one before it. f.11, f.12, AD-I8, AD-I13.

**AD-T16 python_domain_request_refused.** The same through `bytearray(n)` (the `OBJ` domain) and `[None] * k` (the `MEM` domain): refused, `Refused` from `apply`; a call that catches `MemoryError` and returns its input completes with `refusals == 1`; requests below `guard.min_bytes` and a list grown by `append` past the room are not refused. f.10, f.12, AD-I8, AD-I13.

**AD-T17 attribution_across_threads.** Two guarded kernels applied at once from two worker threads, each allocating a different known size many times: each kernel's `requested_bytes` and `peak_bytes` are its own and never include the other's; a refusal on one thread is recorded in that thread's frame only and the other call completes. AD-I9, AD-I12.

**AD-T18 inert_outside_a_frame.** Before any frame, between two calls and on a Python thread started by the test, a request larger than the gate's room is neither refused nor counted, with the hooks installed and a gate bound. AD-I11.

**AD-T19 switch_off.** The request AD-T15 refuses, from a kernel built with `memory_guard=False` and bound like the guarded one: not refused, the call completes, and it is counted all the same (NumPy bytes requested at least the request, `refusal_on == false`, no refusal). This is AD-T15 broken on purpose. AD-I10.

**AD-T20 refusal_diagnostic.** `MorunaError::Refused`'s message is exactly `budget: kernel <name> stage <stage> morsel <seq> requested <n> bytes with <in_use> in use, which would pass the ceiling of <ceiling> bytes; refused before the memory existed`; a call that catches `MemoryError` and raises `ValueError` still returns `Refused`. f.12, AD-I13.

**AD-T21 guard_overhead.** (reference host, E1; reports, never asserts) An allocation-heavy kernel (one million NumPy arrays of 64 KiB, and the same with 1 KiB arrays below `guard.min_bytes`) timed guarded and unguarded on one worker: prints the per-allocation overhead in nanoseconds and the ratio, with the host name. AD-I14.

**AD-T22 arrow_from_the_pool.** A kernel that builds a `pyarrow` array of a known size and returns its input: the call's Arrow counts have bytes requested at least the array's buffers, requests at least one, peak at least the array's buffers when it is the pool's new high-water mark, and largest and refused zero; a kernel that allocates only in NumPy has Arrow bytes requested zero (one worker). f.14, AD-I15.

**AD-T23 counts_reach_the_record.** One call of a Python kernel leaves its counts in `moruna_kernel::take_kernel_alloc()` on the calling thread, `measured` and with `refusal_on` the kernel's; a second `take` returns the default; a failed call and a refused call leave theirs too. f.12, AD-I9.

## l. Implementation notes for the agent

Files: `crates/moruna-adapters/src/lib.rs`, `src/python/{mod.rs, kernel.rs (f.1, f.2, d.1 `PyKernel`), state.rs (e.1 `PyState`, f.7), cross.rs (e.2, e.3), copy.rs (f.3), gil.rs (f.4), fingerprint.rs (e.4), tensor_obj.rs (the `__dlpack__` class), ctx.rs (the `ctx` object)}`; `crates/moruna-polars/src/lib.rs` (f.5); `crates/moruna-datafusion/src/lib.rs` (f.6). `unsafe` permitted in `cross.rs` (C Data Interface and DLPack capsule handling) with `// SAFETY:` citing the Arrow and DLPack ownership rules, in `guard.rs` (reading the frame through its thread-local address, f.12) and in `python/hooks.rs` (the `extern "C"` hook functions, `PyMem_GetAllocator` and `PyMem_SetAllocator`, NumPy's C API table, the handler capsule and the block header, f.10, f.11, e.5) with `// SAFETY:` citing PEP 445, NEP 49 or the frame's lifetime; nowhere else in these three crates outside tests (E9). The guard adds `src/guard.rs` (f.9 the gate, f.12 the frame, the `allocate` family of d.1; no feature) and `src/python/hooks.rs` (f.10, f.11; feature `python`), and `libc` to the crate's dependencies (preamble 6.2 lists it) for `size_t`.

Environment facts to verify before starting the guard: the interpreter's `PyMem_SetAllocator` accepts a wrapper after initialisation (PEP 445; `tracemalloc` does the same); the installed NumPy's `numpy/_core/include/numpy/__multiarray_api.h` names slot 211 `PyArray_GetNDArrayCFeatureVersion`, 304 `PyDataMem_SetHandler` and 306 `PyDataMem_DefaultHandler`, and `ndarraytypes.h` the version 1 `PyDataMem_Handler` layout (`char name[127]; uint8_t version; PyDataMemAllocator allocator`).

PyO3: modules declare `gil_used = false` (0.28 default); use `Python::attach` and `Python::detach`; never hold `Python<'py>` across the boundary copy.

Anti-patterns: no `to_pandas`, no `to_numpy(copy=True)`, no `combine_chunks`; no silent copy when the C Data Interface import fails (error instead).

Contracts this document relies on: `AllocStats.boundary_copies_total`, `Allocator::contains`, `Allocator::tier_of` and `Payload::table_with` (`01-contracts.md` d.3, d.4); `KernelHints.state_bytes` and `KernelState::footprint` (d.7).

## m. Open items

None. (`AllocStats.boundary_copies_total` and `Allocator::contains` are in `01-contracts.md` d.3; the post-v1 items that used to sit here are AD-O1 and AD-O2 in section o.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| S13, G-I2 | AD-I1, AD-I2 | AD-T1, AD-T2, AD-T3 |
| S8, G-I9, D4 | AD-I3 | AD-T4, AD-T5, AD-T11 |
| S7 | AD-I6 | AD-T9, AD-T10 |
| G-I8 | AD-I7 | AD-T7 |
| S17, D13 (Python kernels resume) | e.1, f.7, f.8 | AD-T12 |
| S1, G-I1, G-I8, E13 (allocator guard) | AD-I8, AD-I13 | AD-T13, AD-T15, AD-T16, AD-T20, PY-T20 |
| E13 (attribution, inertness, switch) | AD-I9, AD-I10, AD-I11 | AD-T14, AD-T17, AD-T18, AD-T19, AD-T23, PY-T20 |
| E13 (per-function figures, Arrow's share) | AD-I9, AD-I15 | AD-T14, AD-T22, AD-T23, TR-T14, PY-T21 |
| E13 (the hook's constraints, cost) | AD-I12, AD-I14 | AD-T17, AD-T21 |

## o. Deferred (post-v1)

Recorded here because this is the crate they land in.

**AD-O1. Allocator interposition (E13): built in part, 2026-10-02.** The owner decided to build it with four constraints: per-library hooks first, refuse and never wait, a hard stop before the kill line and not a tighter budget, and switchable off per kernel in the decorator. What was built is the guard of b, AD-I8 to AD-I14 and f.9 to f.13: CPython's allocator domains and NumPy's data allocator. It counts and refuses rather than routing into the arena: the arena's pages are all resident from `new` (02 f.1), so an arena allocation for a kernel's array would be a smaller arena for everything else, which is AD-O8's question and not this one. What was not built is AD-O3 to AD-O8.

**AD-O3. Waiting instead of refusing.** A request the gate cannot admit now might be admissible once placement has demoted a queue to disk. Waiting inside an allocator, with the GIL or a NumPy context held, while the runtime spills is a deadlock risk the first version declines; it needs the gate to ask placement for a demotion and a bound on how long a request may wait.

**AD-O4. A replaced process-wide `malloc`.** Interposing the C allocator itself (`LD_PRELOAD`, a `#[global_allocator]` in the binary) would guard every library at once, Arrow, PyTorch and C extensions included, but it puts a cost on every allocation the runtime makes (AD-I14), and the last process-wide allocator (mimalloc, removed in F8.9) held freed memory and doubled peaks. Not to be revisited without measuring that cost against F8.9's figures.

**AD-O5. Guarding Python domain `realloc`.** Needs the old size, which PEP 445 does not pass; a lock-free table of the large blocks the domains handed out, keyed by address and sized from the ceiling, would give it, at a probe on every `free`.

**AD-O6. pyarrow's memory pool and PyTorch's CPU allocator.** Neither hook is reachable from Moruna's wheel without building against the library. pyarrow: `pyarrow.set_memory_pool` takes a `pyarrow.MemoryPool` whose only implementations are C++ `arrow::MemoryPool` subclasses (`Allocate(size, alignment, out)`, `Reallocate`, `Free(buffer, size, alignment)`, sizes known, so `live` would be exact); a guard pool needs a C++ shim compiled against the installed pyarrow's headers and `libarrow` (`pyarrow.get_include()`, `pyarrow.get_libraries()`), built per pyarrow release because the C++ ABI is not stable, and shipped as a separate wheel or compiled at first use. PyTorch: the CPU allocator is replaced through C++ `c10::SetAllocator(c10::DeviceType::CPU, allocator, priority)` (the Python `CUDAPluggableAllocator` covers CUDA only); a guard needs a `c10::Allocator` whose `allocate(n)` returns a `DataPtr` whose deleter context carries `n`, compiled against the installed torch's headers and C++ ABI (`torch.utils.cpp_extension`), per torch release, and a refusal surfaces as a `c10::Error`, a `RuntimeError` in Python rather than `MemoryError`. Both are worth building as optional companion packages once a kernel that needs them is in front of us.

**AD-O7. Telling the controller.** A refusal under `skip` or `budget(n)` drops a morsel the controller never sees grow, so the next morsel is sized the same and may be refused the same way. The controller could treat a refusal as a breach of the stage (shrink the morsel target, RC f.6) using the trace record's refused counts, or `PyKernel::memory` at any moment.

**AD-O8. An arena split that depends on the kernels.** A smaller arena share where a guarded Python kernel is in the chain, so the guard's room is larger than the reserve (architecture section 8, "A kernel's out-of-arena allowance"); a change to the facade's sizing (12 f.1), not to the guard.

**AD-O9. Rust kernels.** A counting wrapper scoped to a Rust kernel's `apply` needs a `#[global_allocator]`: a library cannot install one for the binary that links it, and in the binary every allocation, the runtime's own included, would pay a thread-local check to find out whether it is inside an `apply`. That is AD-O4's cost in a smaller form, so it was assessed and not built. A Rust kernel that wants the guard can allocate its large buffers from the arena through `Allocator` (contracts d.3), which is the budget's own.

**AD-O2. `MorunaMemoryPool` for the DataFusion bridge.** DataFusion operators reserve memory from a `MemoryPool`; a DataFusion-bridged kernel running inside Moruna currently reserves from DataFusion's own pool, invisible to the budget. An implementation of DataFusion's `MemoryPool` trait over the arena (`try_grow` becomes an arena reservation against the host budget; a refusal makes the operator spill, which is DataFusion's existing behaviour) closes that gap for the one engine whose accounting contract makes it possible. Small; Phase 7.
