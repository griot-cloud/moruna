# Amoru SDD 05: Kernel adapters (`amoru-adapters`)

**Document type:** software design document, component 5 of 12
**Status:** DRAFT · 2026-09-15
**Parent:** `architecture/amoru-runtime-design.md` section 5.3 (Python kernels, portability), 6 (inside another engine); decisions D4, D9; criteria S7, S8, S13; global invariants G-I2, G-I9
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.4, d.7, e.2, e.6
**Component location:** `crates/amoru-adapters`, Rust, features `python`, `polars`, `datafusion`
**Consumes:** contracts (1). **Consumed by:** python surface (12), scheduler (10, as `Kernel` objects), external Polars and DataFusion users

**Decisions worth your eye:** (1) a Python kernel's output that was allocated by pyarrow or Torch on the host is copied once into the arena at the boundary, counted as a boundary copy, because downstream DMA needs arena buffers; device tensors are wrapped, not copied; (2) under a GIL build the adapter serialises Python kernels with its own mutex rather than relying on the interpreter, so the scheduler's accounting sees the serialisation; (3) a Python kernel's fingerprint is the qualified name plus a hash of its source text, so editing the function invalidates its profile.

---

## a. Purpose and boundary

Adapters make things that are not Rust `Kernel`s into `Kernel`s, and make Rust `Kernel`s usable inside other engines. Three adapters: the Python adapter (a callable or an object with `setup` and `__call__` becomes a `Kernel`), the Polars bridge (a `Kernel` becomes a Polars expression plugin), and the DataFusion bridge (a `Kernel` becomes a `ScalarUDF`). The Python adapter is the one that carries design weight: it is where the zero-copy claim meets the interpreter, and where the GIL is detected and contained.

It owns: crossing payloads into and out of Python without copying; the boundary copy into the arena when a kernel returns non-arena host memory; GIL detection and serialisation; Python kernel fingerprints; the two engine bridges.

It refuses to know: scheduling (which worker runs what); sizing; anything about the source or sink; the user's API (the surface's).

## b. Vocabulary

**Callable kernel.** A Python function `f(batch) -> batch`; stateless.

**Class kernel.** A Python object with optional `setup(self, ctx)` and required `__call__(self, batch)`; stateful if the decorator says so.

**Crossing.** Handing a payload to Python (export) or taking one back (import) by pointer: Arrow through the C Data Interface via `pyarrow`, tensors through DLPack via `__dlpack__` and `from_dlpack`.

**Boundary copy.** The single copy of a returned payload into arena memory when its buffers are not arena-owned and are on the host.

**Attach.** Making the current thread able to call into the interpreter: PyO3's `Python::attach` (free-threaded) or GIL acquisition (GIL build).

## c. Invariants

**AD-I1. Crossings do not copy.** Exporting a payload to Python and importing the returned object allocate nothing of payload size; measured by `AllocStats.payload_copies_total` staying constant across a crossing. Upholds G-I2, S13.

**AD-I2. At most one boundary copy per morsel, and only host-side.** A returned host payload whose buffers are not arena-owned is copied exactly once into the arena; a returned device tensor is never copied by the adapter; the copy is counted in `AllocStats.boundary_copies_total` (a new counter this document adds to contracts d.3, see m).

**AD-I3. GIL state is a fact, not a hope.** At adapter construction, `sys._is_gil_enabled()` is read; if true, the adapter reports `gil_serialised = true` and serialises `apply` with a mutex; if false, `apply` runs concurrently. The state is re-checked after the first `apply` (an import may have re-enabled the GIL) and a change is reported. Upholds G-I9, S8.

**AD-I4. Interpreter access is scoped.** No thread holds an attachment across a call into the arena, the placement engine or the reactor; the attachment covers export, call and import only.

**AD-I5. Deleters attach.** A `ManagedTensor` or Arrow buffer whose bytes are owned by a Python object is dropped by first attaching to the interpreter, from whatever thread drops it.

**AD-I6. Bridges are pure wrappers.** `amoru-polars` and `amoru-datafusion` contain no kernel logic; each converts its host's batch representation to `Payload` and back, calls `Kernel::apply` with `NoState`, and returns. Upholds S7.

**AD-I7. Exceptions become errors with context.** A Python exception in `apply` becomes `AmoruError::Kernel { stage, seq, msg: <type>: <message>\n<traceback> }`; the worker never sees a panic.

## d. Interfaces

### d.1 Exposed

```rust
// feature "python"
pub struct PyKernelSpec {
    pub target: pyo3::Py<pyo3::PyAny>,      // callable or class instance
    pub stateful: bool,
    pub instances: core::num::NonZeroUsize,
    pub device_memory: bool,
    pub accepts: PayloadSpec,               // from decorator args; default Table/Host
    pub releases_gil: Option<bool>,
    pub expected_amplification: Option<f64>,
    pub preferred_rows: Option<u64>,
}
pub struct PyKernel { /* private */ }
impl PyKernel {
    pub fn new(spec: PyKernelSpec, alloc: std::sync::Arc<dyn Allocator>) -> Result<PyKernel>;
    pub fn gil_serialised(&self) -> bool;
}
impl Kernel for PyKernel { /* contracts d.7 */ }

pub fn python_gil_enabled() -> bool;        // sys._is_gil_enabled(), false on GIL-less builds
pub fn python_build_info() -> String;       // version, free-threaded flag, for the report

// feature "polars"
pub fn polars_plugin<K: Kernel>(kernel: K) -> impl Fn(&[polars::prelude::Series]) -> polars::prelude::PolarsResult<polars::prelude::Series>;

// feature "datafusion"
pub fn datafusion_udf<K: Kernel>(kernel: K, name: &str) -> datafusion::logical_expr::ScalarUDF;
```

### d.2 Consumed

`amoru_kernel::{Kernel, KernelKind, KernelHints, KernelState, NoState, InitCtx, Payload, PayloadSpec, ManagedTensor, Fingerprint, Allocator, AllocStats, AmoruError}`; `pyo3` (0.28+, free-threaded default), `pyo3-arrow` (RecordBatch ↔ `pyarrow.RecordBatch` via C Data Interface), `dlpark` (DLPack capsules); `polars` and `datafusion` behind features.

## e. Data model, formats and state machines

### e.1 Python kernel state

`PyState { instance: Py<PyAny>, device: Option<DeviceId> }` implements `KernelState`. For a callable kernel, `instance` is the callable itself and `setup` is never called. For a class kernel with `stateful = true`, `init` deep-copies the prototype object per instance (`copy.deepcopy`) unless the class defines `__amoru_clone__`, in which case that is called; then `setup(ctx)` is called if defined, with `ctx` a dict `{ "instance": i, "device": "cuda:N" | None }`.

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

`Fingerprint::compute(identity, config)` with `identity = f"py:{module}.{qualname}"` and `config = blake3(inspect.getsource(target) or code object co_code) || decorator args as canonical JSON`. If the source is unavailable (a lambda in a REPL), `co_code` plus `co_consts` repr is used and a note is recorded.

## f. Algorithms and policies

**f.1 `apply` (free-threaded).** Attach; export input (e.2); call `instance(obj)`; import result (e.3); detach; perform the boundary copy if required (outside the attachment: the returned object is kept alive by a `Py<PyAny>` held until the copy completes, then released under a brief attachment). Record `releases_gil` observation: if the call returned in less wall time than CPU time consumed by other Python threads would allow, no inference is made; the hint is informational only.

**f.2 `apply` (GIL build).** Same, wrapped in the adapter's `Mutex<()>` so that the scheduler's `kernel_busy` for the stage reflects one-at-a-time execution and the report can say `gil_serialised`.

**f.3 Boundary copy.** `alloc(bytes, PinnedHost or Host)` from the adapter's allocator; `memcpy` each Arrow buffer (or the tensor's bytes) into the new buffer(s); rebuild the batch or tensor over arena buffers; drop the Python-owned originals under attachment. Count in `boundary_copies_total`. Skipped when the returned pointers are inside the arena (checked by the arena's `contains(ptr)`, a method this document adds to `Arena`, see m).

**f.4 GIL detection.** `python_gil_enabled` calls `sys._is_gil_enabled` if present, else returns `true` (any interpreter without the attribute has a GIL). Re-check after the first `apply` of each stage; if it flipped to true, set `gil_serialised = true` for the rest of the run and note it.

**f.5 Polars bridge.** Input `&[Series]` → one `RecordBatch` via Arrow FFI (Polars exposes `to_arrow` per series without copy for its native types); tier `Host`; `Kernel::apply(NoState)`; output batch → the first column as a `Series` (the plugin contract is one output series; multi-column kernels are not exposed through Polars, stated in docs).

**f.6 DataFusion bridge.** `ScalarUDFImpl::invoke_with_args` receives `ColumnarValue`s; build a `RecordBatch`; `apply`; return the first output column as `ColumnarValue::Array`.

**f.7 Resume policy for Python kernels.** The decorator's `resume=` maps to `KernelHints.resume`. For `"checkpoint"`, the Python kernel object must define `checkpoint(self, state) -> bytes` and `restore(self, ctx, data: bytes) -> state`; the adapter checks both attributes at wrap time (a `Plan` error otherwise, so the omission is found before any morsel is read) and implements `KernelState::checkpoint` and `Kernel::restore` by calling them under attachment, with the bytes copied out of the Python object before detaching. For `"reinit"` (the default) and `"forbid"` nothing is called. A stateful Python kernel that accumulates across morsels and leaves the default is a user error the documentation names in one sentence, and the resume path cannot detect; the `stateful=True` docstring says "if your state depends on the morsels seen, declare resume='checkpoint' or 'forbid'".


## g. Concurrency within the component

`PyKernel` is `Send + Sync`; each `apply` attaches on the calling worker thread. Under a GIL build, one adapter-level mutex per `PyKernel`. Deleters (AD-I5) attach on whatever thread drops; PyO3 0.28's `Py<T>` drop is safe from any thread when attached, which the deleter hook ensures.

## h. Behaviour

**Normal path.** Decorator builds `PyKernelSpec`; surface constructs `PyKernel`; scheduler calls `init` per instance (class kernels) then `apply` per morsel; adapter crosses, calls, imports, copies at the boundary if needed.

**Edge cases.** Kernel returns its input unchanged: no copy (arena-owned). Kernel returns a batch with a different row count: allowed. Kernel returns a device tensor while `accepts.tier == Host`: allowed; the placement engine will demote it; the report notes the mismatch once. Kernel mutates the input batch in place (pyarrow forbids; Torch permits for tensors): documented as undefined for tables and permitted for tensors when the kernel returns the same tensor.

**Failures.** Python exception: AD-I7. `setup` raises: `Kernel` error at `init`, run does not start. Interpreter finalising (process exit during cancellation): `apply` returns `Cancelled`. Import of a returned object fails (unsupported dtype): `Convert` error with the dtype named.

## i. Configuration

`python.allow_gil` (surface; the adapter only reports).

## j. Observability

`AllocStats.boundary_copies_total`; per-kernel `PyKernelStats { calls, exceptions, boundary_copies, gil_serialised, source_available }`. `tracing`: `adapter.gil` (info at construction and warn on flip), `adapter.boundary_copy` (debug, bytes).

## k. Tests

Python tests run under both a GIL and a free-threaded interpreter in CI (matrix).

**AD-T1 crossing_zero_copy.** Identity Python kernel on a 256 MiB batch; `payload_copies_total` unchanged; `boundary_copies_total` unchanged (returned input is arena-owned). AD-I1.

**AD-T2 boundary_copy_once.** Kernel returns a new pyarrow batch; exactly one boundary copy; bytes equal. AD-I2.

**AD-T3 device_no_copy.** (cuda, skippable) Kernel returns a Torch CUDA tensor; no copies; tier is `Device`. AD-I2.

**AD-T4 gil_detected.** Under the GIL interpreter, `gil_serialised` is true and two concurrent `apply` calls do not overlap (timestamps); under free-threaded, they overlap. AD-I3.

**AD-T5 gil_flip.** Kernel imports a test extension that declares `gil_used = true`; the flip is detected after the first apply. AD-I3.

**AD-T6 deleter_attach.** Drop a Python-owned tensor from a non-Python thread; no crash; refcount reaches zero. AD-I5.

**AD-T7 exception_context.** Kernel raises `ValueError("x")`; error message contains type, message and traceback; worker thread survives. AD-I7.

**AD-T8 fingerprint_source.** Editing one character of the kernel's source changes the fingerprint; same source, different decorator arg, different fingerprint. e.4.

**AD-T9 polars_bridge.** The `normalise` Rust kernel runs as a Polars plugin and inside Amoru with identical output on the same input. AD-I6, S7.

**AD-T10 datafusion_bridge.** Same for DataFusion. AD-I6, S7.

**AD-T11 speedup.** (free-threaded only) A NumPy kernel that releases the GIL on 8 workers reaches ≥ 5.6× the single-worker throughput. S8.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/python/{mod.rs, kernel.rs (f.1, f.2), cross.rs (e.2, e.3), copy.rs (f.3), gil.rs (f.4), fingerprint.rs (e.4), tensor_obj.rs (the `__dlpack__` class)}`, `src/polars.rs`, `src/datafusion.rs`. `unsafe` permitted in `cross.rs` (C Data Interface and DLPack capsule handling) with `// SAFETY:` citing the Arrow and DLPack ownership rules.

PyO3: modules declare `gil_used = false` (0.28 default); use `Python::attach` and `Python::detach`; never hold `Python<'py>` across the boundary copy.

Anti-patterns: no `to_pandas`, no `to_numpy(copy=True)`, no `combine_chunks`; no silent copy when the C Data Interface import fails (error instead).

Contracts this document relies on: `AllocStats.boundary_copies_total` and `Allocator::contains` (`01-contracts.md` d.3).

## m. Open items

(`AllocStats.boundary_copies_total` and `Allocator::contains` are in `01-contracts.md` d.3.) Two post-v1 items recorded here because this is the crate they land in:

**AD-M1. Allocator interposition (E13).** Today a Python kernel's own allocations (NumPy arrays, Torch tensors made inside `apply`) are outside the arena: the sampler sees them, the controller sizes around them, the reserve absorbs mistakes, and the cgroup is the containment (architecture section 8, "observed, not governed"). NumPy (`PyDataMem_SetHandler`) and PyTorch (`CUDAPluggableAllocator`, and the host allocator hooks) both allow the allocator to be replaced. Pointing them at the arena would make kernel-internal allocations count against the budget and fail cleanly at the line instead of being observed after the fact. Deferred because it changes what the kernel author's libraries do underneath them, which needs its own design and its own opt-in; the seam is `Allocator` (contracts d.3), and nothing in v1 precludes it.

**AD-M2. `AmoruMemoryPool` for the DataFusion bridge.** DataFusion operators reserve memory from a `MemoryPool`; a DataFusion-bridged kernel running inside Amoru currently reserves from DataFusion's own pool, invisible to the budget. An implementation of DataFusion's `MemoryPool` trait over the arena (`try_grow` becomes an arena reservation against the host budget; a refusal makes the operator spill, which is DataFusion's existing behaviour) closes that gap for the one engine whose accounting contract makes it possible. Small; Phase 7.

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| S13, G-I2 | AD-I1, AD-I2 | AD-T1, AD-T2, AD-T3 |
| S8, G-I9, D4 | AD-I3 | AD-T4, AD-T5, AD-T11 |
| S7 | AD-I6 | AD-T9, AD-T10 |
| G-I8 | AD-I7 | AD-T7 |
