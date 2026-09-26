# Moruna SDD 05: Kernel adapters (`moruna-adapters`)

**Document type:** software design document, component 5 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/moruna-runtime-design.md` section 5.3 (Python kernels, portability), 6 (inside another engine); decisions D4, D9; criteria S7, S8, S13; global invariants G-I2, G-I9
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.3 (`Allocator::contains`, `AllocStats`), d.4, d.7 (`Kernel`, `KernelState`, `KernelHints`, `ResumePolicy`), e.2, e.6
**Component location:** `crates/moruna-adapters` (the Python adapter, feature `python`); `crates/moruna-polars` and `crates/moruna-datafusion` (the two engine bridges, thin crates, preamble 6.1)
**Consumes:** contracts (1) only. **Consumed by:** python surface (12), scheduler (10, as `Kernel` objects), external Polars and DataFusion users

**Decisions worth your eye:** (1) a Python kernel's output that was allocated by pyarrow or Torch on the host is copied once into the arena at the boundary, counted as a boundary copy, because downstream DMA needs arena buffers; device tensors are wrapped, not copied; (2) under a GIL build the adapter serialises Python kernels with its own mutex rather than relying on the interpreter, so the scheduler's accounting sees the serialisation; (3) a Python kernel's fingerprint is the qualified name plus a hash of its source text, so editing the function invalidates its profile; (4) a stateful Python kernel's state is whatever `setup` returned, passed back to `__call__` explicitly, so the adapter never copies or clones Python objects to make instances.

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

## c. Invariants

**AD-I1. Crossings do not copy.** Exporting a payload to Python and importing the returned object allocate nothing of payload size; measured by `AllocStats.payload_copies_total` staying constant across a crossing. Upholds G-I2, S13.

**AD-I2. At most one boundary copy per morsel, and only host-side.** A returned host payload whose buffers are not arena-owned is copied exactly once into the arena; a returned device tensor is never copied by the adapter; the copy is counted in `AllocStats.boundary_copies_total` (contracts d.3).

**AD-I3. GIL state is a fact, not a hope.** At adapter construction, `sys._is_gil_enabled()` is read; if true, `gil_state()` reports `Serialised` and the adapter serialises `apply` with a mutex; if false, it reports `FreeThreaded` and `apply` runs concurrently. The state is re-checked after the first `apply` (an import may have re-enabled the GIL) and a change is reported. Upholds G-I9, S8.

**AD-I4. Interpreter access is scoped.** No thread holds an attachment across a call into the arena, the placement engine or the reactor; the attachment covers export, call and import only.

**AD-I5. Deleters attach.** A `ManagedTensor` or Arrow buffer whose bytes are owned by a Python object is dropped by first attaching to the interpreter, from whatever thread drops it.

**AD-I6. Bridges are pure wrappers.** `moruna-polars` and `moruna-datafusion` contain no kernel logic; each converts its host's batch representation to `Payload` and back, calls `Kernel::apply` with `NoState`, and returns. Upholds S7.

**AD-I7. Exceptions become errors with context.** A Python exception in `apply` becomes `MorunaError::Kernel { stage, seq, msg, features }` with `msg` exactly `"<type>: <message>\n<traceback>"` (the exception's qualified type name, `str(exc)`, a newline, then `traceback.format_exception` joined); the worker never sees a panic. The same format is used for an exception in `setup`, `checkpoint`, `restore` or `footprint`.

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

`Fingerprint::compute(identity, config)` with `identity = f"py:{module}.{qualname}"` (of the callable, or of the class for a class kernel) and `config = blake3(inspect.getsource(callable) or code object co_code) || decorator args as canonical JSON` (every `PyKernelSpec` field except `callable`). If the source is unavailable (a lambda in a REPL), `co_code` plus `co_consts` repr is used and a note is recorded.

## f. Algorithms and policies

**f.1 `apply` (free-threaded).** Attach; export input (e.2); call `callable(obj)` for a callable kernel or `callable(state, obj)` for a class kernel; import result (e.3); detach; perform the boundary copy if required (outside the attachment: the returned object is kept alive by a `Py<PyAny>` held until the copy completes, then released under a brief attachment). Record `releases_gil` observation: if the call returned in less wall time than CPU time consumed by other Python threads would allow, no inference is made; the hint is informational only.

**f.2 `apply` (GIL build).** Same, wrapped in the adapter's `Mutex<()>` so that the scheduler's `kernel_busy` for the stage reflects one-at-a-time execution and the report can say the stage is `Serialised`.

**f.3 Boundary copy.** `alloc(bytes, PinnedHost or Host)` from the allocator bound by `bind_allocator` (d.1; the host tier is the allocator's, `is_pinned()`); `memcpy` each Arrow buffer (or the tensor's bytes) into the new buffer(s); rebuild the batch or tensor over arena buffers with `Payload::table_with(batch, alloc)` so the tier is inferred through `tier_of`; drop the Python-owned originals under attachment. Count in `boundary_copies_total`. Skipped when the returned pointers are inside the arena (`Allocator::contains(ptr)`, contracts d.3).

**f.4 GIL detection.** `python_gil_enabled` calls `sys._is_gil_enabled` if present, else returns `true` (any interpreter without the attribute has a GIL). `PyKernel::new` records the result as the kernel's `gil_state`. Re-check after the first `apply` of each stage; if it flipped to true, set `gil_state` to `Serialised` for the rest of the run and note it. The facade reads `gil_state()` per Python stage into `RunMeta.gil` (04 d.1) after the run.

**f.5 Polars bridge.** Input `&[Series]` → one `RecordBatch` via Arrow FFI (Polars exposes `to_arrow` per series without copy for its native types); tier `Host`; `Kernel::apply(NoState)`; output batch → the first column as a `Series` (the plugin contract is one output series; multi-column kernels are not exposed through Polars, stated in docs).

**f.6 DataFusion bridge.** `ScalarUDFImpl::invoke_with_args` receives `ColumnarValue`s; build a `RecordBatch`; `apply`; return the first output column as `ColumnarValue::Array`.

**f.7 Resume policy for Python kernels.** `PyKernelSpec.resume` maps to `KernelHints.resume`. For `Checkpoint`, the class kernel must define `checkpoint(self, state) -> bytes` and `restore(self, ctx, data: bytes) -> state`; `PyKernel::new` checks both attributes (a `Plan` error naming the missing one otherwise, so the omission is found before any morsel is read) and the adapter implements `KernelState::checkpoint` (calls `checkpoint(state)` under attachment, copies the bytes out of the Python object before detaching, returns `Some`) and `Kernel::restore` (calls `restore(ctx_obj, data)` under attachment and stores the returned value as the new `PyState.state`). For `Reinit` (the default) and `Forbid` nothing is called. A stateful Python kernel that accumulates across morsels and leaves the default is a user error the documentation names in one sentence, and the resume path cannot detect; the `stateful=True` docstring says "if your state depends on the morsels seen, declare resume='checkpoint' or 'forbid'".

**f.8 Startup placement.** The surface constructs `PyKernel::new(spec)` while translating arguments, before discovery, so `Plan` errors from `new` surface before any component starts; the facade binds the allocator in the "kernels built" step of the facade's startup order (12 PY-I1) and the scheduler's `init_instances` (SC f.4) then calls `init` for every instance of every class kernel on the worker that owns it, so `setup` failures terminate before any morsel is read (architecture 7, "stateful init fails"; SC-T18).

## g. Concurrency within the component

`PyKernel` is `Send + Sync`; each `apply` attaches on the calling worker thread. Under a GIL build, one adapter-level mutex per `PyKernel`. Deleters (AD-I5) attach on whatever thread drops; PyO3 0.28's `Py<T>` drop is safe from any thread when attached, which the deleter hook ensures.

## h. Behaviour

**Normal path.** Decorator builds `PyKernelSpec`; surface constructs `PyKernel::new(spec)`; facade binds the allocator; scheduler's `init_instances` calls `init` per instance (class kernels only, eagerly, f.8) then `apply` per morsel; adapter crosses, calls, imports, copies at the boundary if needed.

**Edge cases.** Kernel returns its input unchanged: no copy (arena-owned). Kernel returns a batch with a different row count: allowed. Kernel returns a device tensor while `accepts.tier == Host`: allowed; the placement engine will demote it; the report notes the mismatch once. Kernel mutates the input batch in place (pyarrow forbids; Torch permits for tensors): documented as undefined for tables and permitted for tensors when the kernel returns the same tensor. `setup` returns `None`: allowed, `state` is `None` and is passed back as such. `footprint` absent: `KernelState::footprint` returns `None` and the controller falls back to `state_bytes` from the spec (RC f.3).

**Failures.** Python exception: AD-I7. `setup` raises: `Kernel` error at `init`, which `init_instances` returns before the run starts (SC f.4); nothing has been read or written. Interpreter finalising (process exit during cancellation): `apply` returns `Cancelled`. Import of a returned object fails (unsupported dtype): `Convert` error with the dtype named. `apply` before `bind_allocator`: `Config { name: "adapter" }` (a facade bug, not a runtime condition).

## i. Configuration

`python.allow_gil` (surface; the adapter only reports).

## j. Observability

`AllocStats.boundary_copies_total`; per-kernel `PyKernelStats { calls, exceptions, boundary_copies, gil_state, source_available }`. `tracing`: `adapter.gil` (info at construction and warn on flip), `adapter.boundary_copy` (debug, bytes).

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

## l. Implementation notes for the agent

Files: `crates/moruna-adapters/src/lib.rs`, `src/python/{mod.rs, kernel.rs (f.1, f.2, d.1 `PyKernel`), state.rs (e.1 `PyState`, f.7), cross.rs (e.2, e.3), copy.rs (f.3), gil.rs (f.4), fingerprint.rs (e.4), tensor_obj.rs (the `__dlpack__` class), ctx.rs (the `ctx` object)}`; `crates/moruna-polars/src/lib.rs` (f.5); `crates/moruna-datafusion/src/lib.rs` (f.6). `unsafe` permitted in `cross.rs` (C Data Interface and DLPack capsule handling) with `// SAFETY:` citing the Arrow and DLPack ownership rules; nowhere else in these three crates outside tests (E9).

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

## o. Deferred (post-v1)

Two items recorded here because this is the crate they land in.

**AD-O1. Allocator interposition (E13).** Today a Python kernel's own allocations (NumPy arrays, Torch tensors made inside `apply`) are outside the arena: the sampler sees them, the controller sizes around them, the reserve absorbs mistakes, and the cgroup is the containment (architecture section 8, "observed, not governed"). NumPy (`PyDataMem_SetHandler`) and PyTorch (`CUDAPluggableAllocator`, and the host allocator hooks) both allow the allocator to be replaced. Pointing them at the arena would make kernel-internal allocations count against the budget and fail cleanly at the line instead of being observed after the fact. Deferred because it changes what the kernel author's libraries do underneath them, which needs its own design and its own opt-in; the seam is `Allocator` (contracts d.3), and nothing in v1 precludes it.

**AD-O2. `MorunaMemoryPool` for the DataFusion bridge.** DataFusion operators reserve memory from a `MemoryPool`; a DataFusion-bridged kernel running inside Moruna currently reserves from DataFusion's own pool, invisible to the budget. An implementation of DataFusion's `MemoryPool` trait over the arena (`try_grow` becomes an arena reservation against the host budget; a refusal makes the operator spill, which is DataFusion's existing behaviour) closes that gap for the one engine whose accounting contract makes it possible. Small; Phase 7.
