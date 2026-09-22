# Amoru SDD 12: Python surface and runtime facade (`amoru-py`, `amoru-runtime`, `python/amoru`)

**Document type:** software design document, component 12 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/amoru-runtime-design.md` section 5.10, 6 (hosting), 4.3 (D4); criteria S2, S8, S12; global invariants G-I9, G-I10
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` (all)
**Component location:** `crates/amoru-runtime` (facade, Rust; built in wave 4 because RC-T12 needs it), `crates/amoru-py` (PyO3 module) and `python/amoru` (package), both wave 5; Python 3.13 and 3.14
**Consumes:** every component. **Consumed by:** users.

**Decisions worth your eye:** (1) the runtime facade `Runtime::run` is in Rust and is the only place components are wired, so the Python package has no orchestration logic; (2) the Python API is four constructors, one decorator and one function, with keyword-only arguments and no configuration object; (3) a Python kernel under a GIL interpreter is refused by default with a message that names the fix, rather than silently serialised; (4) the Rust runtime runs on a helper thread while the Python main thread does nothing but poll for signals, so `KeyboardInterrupt` is honoured without the runtime ever touching the interpreter.

---

## a. Purpose and boundary

Two pieces. The runtime facade wires discovery, arena, reactor, trace, sources, sinks, kernels, placement, scheduler and controller into `Runtime::run`, in a fixed order, and produces the run report; it is the only component that knows the order of startup and shutdown. The Python surface exposes the facade and the sources, sinks and kernel adapter to Python users with as little of its own logic as possible, packages the wheel, and holds the documentation users read.

It owns: startup and shutdown sequencing; the run id; error-to-exception mapping; the public Python API; argument translation and its range clamping; the wheel build; GIL refusal; the report's Python rendering; the end-to-end benchmark harness.

It refuses to know: anything a component owns. If logic appears here that is not sequencing or translation, it belongs in a component.

## b. Vocabulary

**Facade.** `amoru_runtime::Runtime`, the Rust entry point; `amoru.run` calls it.

**Session.** One `Runtime::run` invocation from start to report.

**Kernel object.** What `@amoru.kernel` produces: a frozen `#[pyclass]` carrying a `PyKernel` built from the `PyKernelSpec` (05 d.1), passed to `run`.

**Handle.** A Python object wrapping a Rust source or sink (`ParquetSource`, `TensorSource`, `IteratorSource`, `ParquetSink`, `TensorSink`, `ArrowIpcSink`), constructed eagerly (Parquet footers are read at construction so errors surface early).

**Runtime thread.** The helper thread `amoru.run` spawns to execute `Runtime::run`; the Python main thread waits on it while polling for signals (f.5).

**Components.** The struct of optional pre-built components `Runtime::run_with` accepts (d.1); every `None` is built normally. The test seam for PY-T1 and PY-T2.

## c. Invariants

**PY-I1. Startup order is fixed.** `Runtime::run` performs, in this order and no other: discover → arena → reactor → trace → sources and sinks built → kernels built → placement → scheduler `new` (validates the chain, opens the sink on a fresh run, spawns workers parked; drives and the checkpoint thread are not started) → `scheduler.init_instances()` (eagerly runs `Kernel::init` for every instance up to `max_instances` of every stateful stage, on the worker threads that will own them; stateless stages have no instances) → controller `prepare` (baseline sampled after init) → controller `probe_all` (through `Prober`) → controller `start` → scheduler `run` (starts the drives and the checkpoint thread, enters `Running`) → controller `stop` → trace `finish` → report. Shutdown is the reverse of what was started, always executed, including on error. A resumed run (f.7) differs in exactly these steps: the manifest header is read before placement is built; `placement.restore` runs right after placement is built; scheduler `new` is given `resuming = true` and does not open the sink; `scheduler.apply_resume_point` replaces `init_instances`; `probe_missing` replaces `probe_all`; `scheduler.run_resumed` replaces `run`.

**PY-I2. Every error reaches Python as one exception type with a structured payload.** `amoru.AmoruError` with `.kind` (the `AmoruError` variant name), `.message`, and `.diagnostic` (a dict with seq, stage, features, footprint, budget where present); subclasses `PlanError`, `KernelError`, `BudgetError`, `IoError`, `ConfigError`, `ResumeError`, `Cancelled` for `except` ergonomics.

**PY-I3. No parameters beyond the budget are required.** `amoru.run(source, kernels, sink)` with no other arguments completes every benchmark inside a cgroup; outside one, `budget=` is the only argument a user may need. Upholds G-I10, S2.

**PY-I4. GIL refusal is the default.** If any kernel is Python and `python_gil_enabled()` is true and `allow_gil` is false, `run` raises `ConfigError` before starting anything, with the message "Python kernels require a free-threaded interpreter (python3.13t or python3.14t); pass allow_gil=True to run serialised". Upholds G-I9, S8.

**PY-I5. The report is returned, not only printed.** `run` returns a `RunReport` object whose attributes are exactly the fields of `amoru_trace::RunReport` (04 d.1: including `run_id`, `manifest`, `resumed`, `notes`, `gil_serialised`, `io_paths`), with `__str__` giving the fixed layout, `.to_json()`, and `.trace_path` when a trace was written.

**PY-I6. Cancellation is honoured.** `KeyboardInterrupt` during `run` sets the cancel token, waits for the runtime's cancellation sequence (preamble 4.3) on the runtime thread, and re-raises as `amoru.Cancelled` with the partial report attached. The Python main thread is the only thread that ever sees the signal (f.5).

**PY-I7. Wheels are build-specific and thread-safe.** The module is built with `gil_used = false`, for CPython 3.14 in both builds, standard and free-threaded, on `manylinux_2_28` x86_64 and aarch64 and `macosx` arm64 (six wheels: two builds by three platforms); no abi3, which is why the two builds cannot share one wheel. The 3.13 interpreters are not targets (preamble 6.6): 3.13's free-threaded build was experimental, pyarrow ships no wheel for it, and a user could not run this surface there. Adding an interpreter is a release decision at F5.2, taken when someone asks for one. Every `#[pyclass]` is `frozen`, so no Python-visible object has interior mutability the free-threaded interpreter would have to guard.

**PY-I8. Nothing on the Python side allocates or touches payload bytes.** The package never imports `pandas`; it accepts and returns `pyarrow` and DLPack objects through the adapters only.

**PY-I9. A run that can be resumed says so, and a resume never starts from the wrong run.** Every `Terminated` or `Cancelled` outcome with a manifest carries `.manifest` (path) and `.run_id` on the exception and the partial report; `amoru.run(..., resume=<run_id | path | "auto">)` refuses, before any write, a manifest whose run id, plan or kernels differ from what was passed (`Placement::restore`), so a user cannot resume yesterday's job over today's inputs by accident. Upholds S17.

**PY-I10. Every user argument is range-checked once, here.** Values outside the preamble's configuration table are clamped at argument translation (f.3) and each clamp is reported in `report.notes`; the scheduler clamps its own knobs (SC f.15) and discovery clamps the budget; nothing else clamps anything (preamble section 5).

## d. Interfaces

### d.1 Runtime facade (Rust)

```rust
pub struct RunSpec {
    pub source: Arc<dyn Source>,
    pub kernels: Vec<Arc<dyn Kernel>>,                 // may be empty (h)
    pub py_kernels: Vec<(StageId, Arc<amoru_adapters::PyKernel>)>,   // the Python ones, for bind_allocator and gil_state
    pub sink: Box<dyn Sink>,                           // wrapped into SinkHandle in f.1
    pub budget: Option<u64>, pub cpu: Option<f64>,
    pub trace_path: Option<PathBuf>, pub staging_dir: Option<PathBuf>, pub staging_limit: Option<u64>,
    pub error_policy: ErrorPolicy, pub ordered: bool,  // contracts d.11 ErrorPolicy
    pub sizer: SizerKind, pub profiles_dir: Option<PathBuf>,   // None: the preamble's `~/.amoru/profiles` default, resolved in f.1
    pub object_store: ObjectStoreConfig,               // 06 d.1
    pub host_profile: Option<HostProfile>,
    pub allow_gil: bool,
    pub checkpoint: bool, pub checkpoint_interval_ms: u64, pub checkpoint_keep: bool,
    /// None: fresh run. Some: resume from this manifest (found by run id or path; "auto" resolved by the surface to the newest under `staging_dir`).
    pub resume: Option<PathBuf>,
    pub notes: Vec<String>,                            // clamps and translations the surface reports (f.3)
}

/// Pre-built components for tests (PY-T1, PY-T2). Every `None` is built as in f.1; a `Some` is
/// used in its place at the same step, so the order is observable with instrumented fakes.
#[derive(Default)]
pub struct Components {
    pub discovered: Option<Discovered>,
    pub alloc: Option<Arc<dyn Allocator>>,
    pub reactor: Option<Arc<dyn Reactor>>,
    pub trace: Option<(Arc<dyn TraceSink>, Arc<dyn TraceTail>)>,   // two handles to one object (FakeTrace implements both)
    pub sampler: Option<Arc<dyn Sampler>>,
    pub placement: Option<Arc<dyn Placement>>,
    pub run_id: Option<RunId>,
}

pub struct Runtime;
impl Runtime {
    /// PY-I1. Blocks until the run ends. `cancel` is polled by the scheduler. Equals
    /// `run_with(spec, cancel, Components::default())`.
    pub fn run(spec: RunSpec, cancel: CancelToken) -> Result<RunReport>;
    pub fn run_with(spec: RunSpec, cancel: CancelToken, components: Components) -> Result<RunReport>;
    /// Discovery only, for `amoru.inspect_host()`.
    pub fn inspect(input: &DiscoveryInput) -> Result<Discovered>;
}
```

`Runtime::run` returns `Err` for a failure at any step and for a run the scheduler reports as `Terminated { diagnostic, manifest }` (the diagnostic is the error, the partial report and the manifest path are attached); `Cancelled { manifest }` is `Err(Cancelled)` with the same attachments; `Completed` is `Ok(report)`.

### d.2 Python API (`python/amoru/__init__.py`)

```python
def run(source, kernels, sink, *, budget=None, cpu=None, trace=None, staging_dir=None,
        staging_limit=None, on_error="terminate", ordered=False, sizer="rule",
        profiles_dir=None, storage=None, host_profile=None, allow_gil=False,
        checkpoint=True, checkpoint_interval=5.0, keep_checkpoint=False,
        resume=None) -> RunReport
    # resume: None | "auto" | run id (str, 32 hex characters) | path to a manifest.json

def kernel(fn=None, *, stateful=False, instances=1, device_memory=False,
           accepts="table", tier="host", releases_gil=None,
           expected_amplification=None, preferred_rows=None,
           resume="reinit",     # "reinit" | "checkpoint" | "forbid"; "checkpoint" requires the
                                # object to define checkpoint(self, state) -> bytes and restore(self, ctx, data) -> state
           state_bytes=None)    # declared per-instance state size in bytes (05 d.1)
    # stateful=False: fn is a plain callable f(batch) -> batch
    # stateful=True: fn is a class instance with setup(self, ctx) -> state and __call__(self, state, batch) -> batch,
    #                optionally footprint(self, state) -> int | None; ctx exposes .instance and .device (05 b)

class ParquetSource:   def __init__(self, urls: str | list[str], *, columns=None, filters=None)
class TensorSource:    def __init__(self, paths: str | list[str], *, tensors=None)
class IteratorSource:  def __init__(self, iterable, *, schema: pyarrow.Schema | tuple[str, tuple[int, ...]])
class ParquetSink:     def __init__(self, url: str, *, row_group_bytes=None, file_bytes=None, compression="zstd")
class TensorSink:      def __init__(self, path: str, *, format="amb1", one_file_per_morsel=False, name="tensor")
class ArrowIpcSink:    def __init__(self, path: str, *, file_bytes=None)

class RunReport:  # attributes exactly the fields of amoru_trace::RunReport (04 d.1); __str__; to_json(); trace_path;
                  # manifest is None when the run completed and the checkpoint was not kept
class AmoruError(Exception): kind: str; message: str; diagnostic: dict; run_id: str | None; manifest: str | None; report: RunReport | None
class ResumeError(AmoruError) ...   # a manifest that cannot be used; message names the first mismatch
class PlanError(AmoruError) ...; KernelError; BudgetError; IoError; ConfigError; Cancelled

def inspect_host() -> dict    # {"limits": {...}, "host_profile": {...}, "notes": [...]}: Limits and HostProfile field by field, discovery notes
def polars(fn, *, accepts="table")  # wraps a function from a Polars DataFrame to a Polars DataFrame as a stateless Python kernel (f.6)
__version__: str
```

`storage=` takes a dict of object-store options (`endpoint`, `region`, `access_key`, `secret_key`, `session_token`, `allow_http`) or a `pyarrow.fs`-style URL scheme mapping; credentials also come from the environment as the `object_store` crate reads them.

### d.3 Consumed

Every crate's public constructor: `amoru_discovery::{discover, Sampler}`, `amoru_arena::Arena`, `amoru_reactor::Reactor`, `amoru_trace::{TraceWriter, RunReport, RunMeta}` (with `amoru_kernel::GilState`, contracts d.7), `amoru_sources`, `amoru_sinks::{SinkHandle, ReorderBuffer}`, `amoru_adapters::{PyKernel, PyKernelSpec, python_gil_enabled}`, `amoru_placement::PlacementEngine` (`new`, `find_manifest`, `read_manifest_header`, the trait), `amoru_scheduler::{Scheduler, SchedulerConfig, Pipeline, RunOutcome}`, `amoru_controller::{Controller, ControllerConfig, PlanSummary, KernelInfo}`; `getrandom` (the run id); `pyo3` 0.28+ with `gil_used = false`; `pyo3-arrow`; `maturin` for the build; `pyarrow` ≥ 17 at runtime (import check with a clear error); `tracing-subscriber`; `mimalloc`.

## e. Data model, formats and state machines

### e.1 Session state machine

`Configuring` (Python constructs handles and kernel objects, including `PyKernel::new`) → `Starting` (PY-I1 steps up to controller `start`) → `Running` → `Finishing` | `Terminating` | `Cancelling` → `Done`. Errors in `Starting` unwind what was started in reverse order before raising.

### e.2 Exception mapping

| `AmoruError` variant | Python class |
|---|---|
| Plan, Config, Convert | `PlanError`, `ConfigError`, `PlanError` |
| Kernel | `KernelError` (diagnostic: seq, stage, features, original traceback string) |
| Budget | `BudgetError` (diagnostic: seq, stage, footprint, budget, features) |
| Source, Sink, Io, Staging | `IoError` (diagnostic: op, target, split or file) |
| Alloc | `BudgetError` |
| Cancelled | `Cancelled` (with partial report, `.manifest` when one was written) |
| Resume | `ResumeError` |
| Unsupported | `ConfigError` (message names the feature: "built without rdma") |

### e.3 Package layout

```
python/amoru/__init__.py        the API above; imports amoru._core (the PyO3 module)
python/amoru/_report.py         RunReport wrapper and __str__
python/amoru/_errors.py         exception classes
python/amoru/py.typed, _core.pyi   type stubs; _core.pyi is hand-written and checked against the module in PY-T5
pyproject.toml                  maturin backend; wheel matrix in CI
```

## f. Algorithms and policies

**f.1 `Runtime::run` sequence.** Exactly PY-I1, with these details between steps. Mint the run id: `RunId(getrandom 16 bytes)` on a fresh run (or `components.run_id`); on a resumed run it comes from the manifest header (f.7). Discover: if `spec.host_profile` is given it overrides the environment. Arena: the facade samples the process's anonymous memory **before** `Arena::new`, because 02 f.1 touches every page of the region and a sample taken after it contains the arena. The *allowance* is `ceiling - baseline - reserve - expected kernel state`, where `expected kernel state` is `sum over stateful kernels of hints().state_bytes.unwrap_or(0) x max_instances` and `reserve` is `budget.reserve_fraction x ceiling`. The region is a share of that allowance and not all of it: `max(allowance x 2 / (2 + a_anon), min(allowance, 256 MiB + 4 x morsel.min_bytes))`, rounded down to a multiple of `max(page.bytes, 64 KiB)` as 02 e.1 requires, where `a_anon` is the largest `hints().expected_amplification` any stage declares, counting a stage that declares none as 4.0 (the figure the controller itself seeds `a_anon` with, 11 f.3) and a chain with no kernels at all as zero. The share is what equalises the slack in 11 b's two inequalities: per byte in flight the arena needs two bytes, one for the worker and one for the queue behind it (11 f.3 divides the arena in half), while the process needs `a_anon` bytes outside it, so the arena takes `2 / (2 + a_anon)` and the rest of the allowance stays as anonymous headroom for what the chain allocates outside it. A chain declaring `expected_amplification = 0.0` throughout says it allocates nothing outside the arena and takes the whole allowance, which is what every chain took until 2026-09-23: that left `budget.reserve_fraction`, ten percent, as the entire governed headroom for out-of-arena bytes at every budget, shared with the runtime's own reader and writer buffers, and it is why a Python kernel appending an uppercased column over 200,000 rows was refused below about 2 GiB, for want of the 51 MiB that rule left at a 512 MiB ceiling rather than because the budget could not hold the job. The 256 MiB floor is the largest single allocation a default pipeline makes: the Parquet sink asks for `sink.row_group_bytes` plus a megabyte of footer headroom and will not go below `row_group_bytes` (08 f.1), so at the default 128 MiB row group it asks for 129 MiB, and 02 e.1's classes are powers of two, so it costs 256 MiB of region, and a class that size has to be free all at once, so the floor is that class plus the morsels that can be in flight when the sink opens: 272 MiB, measured as the smallest arena `examples/append_column.rs` runs in at a 512 MiB ceiling. An arena below it cannot open a sink at defaults whatever the budget, and lowering it is a change in 08 rather than here. It is capped by the allowance, so a budget with no room still reaches the controller's own refusal rather than being handed a region that is not there. It is the floor and not the rule: above about a 1 GiB ceiling the share is what governs. The note the facade records names the share, the allowance and the headroom left above the arena, so a run report says what the region was sized from. That same number and that same baseline go to the controller as `ControllerConfig::arena_bytes` and `baseline_bytes` (11 f.1), so the bytes the controller believes it may hold are the bytes the arena can actually give it; nothing is halved and nothing is subtracted twice. A rule that left the arena no bytes is a `Config { name: "budget.host" }` from the controller's own check, which names the ceiling, the baseline, the reserve and the kernel state it was sized from. The reactor then receives the arena as `Arc<dyn Allocator>`. Trace: `TraceWriter::start` with the run id. Sources: built with the reactor (and, for `ParquetSource`, the same reactor as `Arc<dyn ObjectMetadata>`; 07 d.1); sinks: built with the reactor and allocator (08 d.1); `plan()` is called once (the source caches it, so the scheduler's own call in SC f.1 returns the same list) to build `PlanSummary { total_bytes, total_rows, splits, max_split_bytes, sub_splittable_all }` for the controller and to fail early on plan errors; the sink is wrapped with `amoru_sinks::SinkHandle::wrap(sink, spec.ordered, ordering.buffer_bytes)` (08 d.1: `Ordered` when `spec.ordered || sink.requires_order()`, else `Plain`). Kernels: `bind_allocator(alloc)` on every `PyKernel` (05 d.1); the chain's input schemas are computed here (`source.schema()`, then each `output_schema`) and `KernelInfo { schema_hash: input_schema.hash(), .. }` (contracts d.4) is built per stage. Placement: `PlacementEngine::new(cfg, alloc, reactor)` with the run id and `node`; the facade computes the manifest identity fields of `PlacementConfig` (09 d.1, e.5): `plan_digest` by the 09 e.5 rule (BLAKE3 over each split's `id` and `rows` as little-endian u64, in plan order), `fingerprints` (each kernel's `fingerprint()`, stages 1..n), `resume_policy` (each kernel's `hints().resume`), `config` (the resolved configuration table of preamble section 5 as a JSON object), `durable_staging` (`host_profile.durable_staging.is_guaranteed()`), `gds` (`host_profile.gds.is_available()` and the `gds` feature and `reactor.paths().gds`) and `checkpoint_enabled`. When the source is a `PyIteratorSource` the facade calls `placement.set_staging(0, true)` before the first push (07 SO-I8, PL-I6) and notes "iterator source: no resume, Q0 staged". Scheduler `new` with `Arc<dyn Placement>`, the allocator, the trace as `Arc<dyn TraceSink>`, the sampler (one `Arc<dyn Sampler>` shared with the controller); `init_instances`. The chain's schemas are the best the facade can compute, and for an opaque kernel (a Python one, 05 d.1) the last one is only the source's; the sink is therefore opened with a schema it treats as provisional and takes its output schema from the first payload instead (08 f.1). A kernel that appends a column is the canonical job and must not need a declaration. Controller `new` with `Arc<dyn Knobs>`, `Arc<dyn StatsSource>`, `Arc<dyn Prober>` (all three the scheduler), the sampler, the trace as `Arc<dyn TraceTail>`, the placement engine for `set_budgets`, and `ControllerConfig { plan, workers_max, pinned: alloc.is_pinned(), checkpoint_enabled, checkpoint_interval_ms, .. }`; then `scheduler.set_record_hook(Arc::new(move |r| controller.on_record(r)))`, so the hook exists before any probe record. `prepare`, `probe_all`, `start`, `run(cancel)` (blocks on the runtime thread), `stop`, then `trace.finish()` (a `flush` and an empty view when `components.trace` injected a fake, which has no `finish`), then the report (f.2). `Terminated { diagnostic, manifest }` and `Cancelled { manifest }` are returned as `Err` with the partial report and the manifest attached (d.1). On any `Err` from a step, every started component is shut down in reverse (`controller.stop`, `scheduler.shutdown` which joins workers and drives, `placement.shutdown`, `reactor.shutdown`, `trace.finish`, arena drop) and the error is returned with the partial report attached where one exists. `profiles_dir`: `None` from the surface means the preamble's default, `~/.amoru/profiles`, which the facade resolves from `$HOME` and creates here; a home directory that is not set or a directory that cannot be created leaves the store off with a note, which is the same outcome 11 f.9 already has for an unwritable store. Without it a resumed run has no memory of the interrupted run and `probe_missing` has nothing to seed from (f.7, 11 f.14). The facade never calls a checkpoint method: the scheduler writes the final manifest itself (SC f.12), including the one a completed run keeps when `checkpoint.keep` is set, which is what makes `keep_checkpoint` mean anything on a run shorter than `checkpoint.interval_ms`; the facade passes `checkpoint_keep` to `SchedulerConfig` for that reason and reads the path back from the engine (f.2).

**f.2 Report assembly.** `RunMeta` (04 d.1) from: `run_id`; `exit` from the `RunOutcome`; start and end instants; `resumed`; `manifest` (the outcome's, or the engine's `manifest_path()` when `checkpoint_keep` kept it); `notes` = `Discovered.notes` followed by `spec.notes` (f.3); `gil` = `[(stage, py_kernel.gil_state())]` for every Python stage; `io_paths = reactor.paths()`; `sizer`, `sizer_fallback_at`, `bottleneck_timeline` and `controller_notes` from `ControllerSummary`; then `RunReport::compute(&trace.finish()?, &limits, &meta)`.

**f.3 `amoru.run` argument translation.** Strings for sizes (`"6GiB"`) go through discovery's parser (component 3 f.3); `on_error` maps `"terminate" | "skip" | ("budget", n)` to `ErrorPolicy` (contracts d.11); `kernels` may be a single kernel or a list; a plain function passed as a kernel is wrapped as if decorated with defaults; `ordered=True` requests `Ordered` (f.1 also wraps when the sink requires order). Range clamping (PY-I10): every argument with a row in the preamble's configuration table whose owner is `user` and whose range is not `fixed` is clamped to the nearest bound here and a note "clamped <name> from <given> to <bound>" is appended to `spec.notes`: `staging_limit` (`budget.disk`), `checkpoint_interval` (`checkpoint.interval_ms`, 500 to 60,000 ms), `sink.row_group_bytes`, `sink.file_bytes`, `trace` path validity; `budget` and `cpu` are passed to discovery unchanged, which owns their clamping against the discovered ceiling and reports it in `Discovered.notes`; an unknown `on_error` or `sizer` string is `ConfigError`, not clamped. Sink URL scheme: a `ParquetSink` destination with no `://` is a local directory, so it is rewritten here to an absolute `file://` URL before it reaches component 8. The source side already reads a bare path as local (07 util), while the reactor reads a string with no scheme as a bare key in the default S3 bucket (06 d.1), so a bare path that was passed through unchanged reached the reactor as an object no bucket was configured for and every write failed. A string that carries a scheme is passed through untouched. Sink-equals-source check: the sink URL and every source URL are normalised (scheme lower-cased, trailing slash removed, `file://` resolved to an absolute path) and the run is refused with `PlanError` when the sink string equals a source string or the sink path is a prefix of any source path (it would overwrite or write into the input).

**f.4 GIL check (PY-I4).** Before any Rust component starts: if any kernel is a Python kernel, call `amoru_adapters::python_gil_enabled()`; refuse unless `allow_gil`. After the run, each Python stage's `gil_state()` goes into `RunMeta.gil` (f.2), so the report says which stages ran serialised.

**f.5 `KeyboardInterrupt` and thread roles.** `amoru.run` spawns the runtime thread, which runs `Runtime::run` and never touches the interpreter (the adapters attach on worker threads for kernel calls, 05 f.1; the runtime thread itself only wires and waits). The Python main thread stays in `amoru.run` and loops: attach, `PyErr::check_signals`, detach, sleep 100 ms, until the runtime thread finishes. When `check_signals` raises `KeyboardInterrupt`, the main thread sets the cancel token, keeps looping (a second interrupt is swallowed with a message) until the runtime thread returns, then raises `Cancelled` with the partial report. Signals are delivered to the main thread by CPython's design, which is why the roles are not the other way round.

**f.6 `polars()` helper.** In v1 the only path is a stateless Python kernel `lambda batch: pl.from_arrow(batch).pipe(fn).to_arrow()` wrapped as if decorated with the given `accepts` (this copies inside Polars on some types; the docstring says so). The Rust path through `pyo3-polars` and `amoru-polars` is deferred (preamble 6.2 lists `pyo3-polars` as deferred); the signature does not change when it lands.

**f.7 Resume path.** With `resume` set, in this order: resolve it to a manifest path (`"auto"`: `PlacementEngine::find_manifest(staging_dir, None)`; a run id: `find_manifest(staging_dir, Some(id))`; a path: as given; nothing found is `ResumeError`); refuse with `ResumeError` if `source.repeatable()` is false; read the manifest header (`PlacementEngine::read_manifest_header`) and take its `run_id` and `node` for `PlacementConfig` and `TraceConfig`, so the new process continues the old run's identity rather than minting a new one; then PY-I1's steps discover → arena → reactor → trace → sources and sinks → kernels → placement; `placement.restore(&manifest, &plan, &fingerprints)` (its refusals surface as `ResumeError` before the sink is touched); scheduler `new` with `resuming = true` (the sink is not opened); `scheduler.apply_resume_point(point)` (SC f.13: refuses when checkpointing cannot be on, resumes the sink, sets the cursor, restores or re-inits instances) in place of `init_instances`; controller `prepare`; controller `probe_missing` in place of `probe_all`; controller `start`; `scheduler.run_resumed(cancel)` in place of `run` (re-reads `to_recompute`, then continues as `run`); controller `stop`; trace `finish`; report with `resumed = true`. `checkpoint=True` is the default whenever a staging directory exists and the source is repeatable (discovery finds or creates one; `staging_dir=None` with no discoverable directory disables both staging and checkpointing with a note in the report); a non-resumable sink forces it off with a note naming the sink (SC f.1); on `Completed`, the run directory is removed unless `keep_checkpoint`, and when `keep_checkpoint` is set the scheduler has written a final manifest at the end of the run whether or not the checkpoint interval ever elapsed, so a run shorter than `checkpoint.interval_ms` keeps something rather than an empty directory; on `Terminated` or `Cancelled`, the final manifest the scheduler wrote is put on the exception and the partial report, and the message ends with "resumable: pass resume=\"<run_id>\"" when it is. The resume argument is the only new parameter a user meets, and only after a failure; PY-I3 holds.

## g. Concurrency within the component

`run` is not re-entrant: a second `run` in the same process while one is active raises `ConfigError` (the arena is a process-wide reservation). A module-level mutex enforces it. Python-side threads: the interpreter's main thread (the signal loop, f.5) and the runtime thread (the facade); every other thread belongs to a component (preamble 4.1). The main thread holds no attachment while sleeping, so worker threads attaching for kernel calls are never blocked by it.

## h. Behaviour

**Normal path.**

```python
import amoru, pyarrow as pa

@amoru.kernel
def normalise(batch: pa.RecordBatch) -> pa.RecordBatch: ...

report = amoru.run(amoru.ParquetSource("s3://bucket/in/", columns=["id", "text"]),
                   normalise,
                   amoru.ParquetSink("s3://bucket/out/"))
print(report)
```

**Edge cases.** `kernels=[]`: allowed; the pipeline is source → sink (a copy or format conversion); the facade passes an empty kernel list and an empty `KernelInfo` list, the chain check is the sink's `accepts` against the source schema, the scheduler's sink drive pops Q0 (SC h), the controller skips probes and sizes the source reads through `MorselTarget { stage: 0 }` with one active worker (RC f.3), and the report has no stage rows (04 h). A sink URL equal to a source URL, or a sink path that is a prefix of a source path: `PlanError` (f.3). `budget` larger than the cgroup ceiling: clamped by discovery with a note in `report.notes`. `trace=` to a directory: file named `amoru-<run_id>.arrow` inside it. `checkpoint_interval=0.1`: clamped to 0.5 s with a note.

**Failures.** `pyarrow` missing: `ImportError` at `import amoru` with the install hint. A kernel object not produced by `@amoru.kernel` and not callable: `TypeError` in `run` before anything starts. A stateful kernel whose `setup` raises: `KernelError` from `init_instances`, before any read or write (SC f.4). Any component failure: PY-I2 mapping with the partial report on the exception as `.report` when available. `Terminated { diagnostic }`: the mapped exception for the diagnostic, with `.report` and `.manifest`.

## i. Configuration

`python.allow_gil`, `trace.path`, `errors.policy`, `ordering.required`, `sizer`, `profiles.dir`, `budget.*`, `staging.*`, `checkpoint.*`, `sink.row_group_bytes`, `sink.file_bytes` (as `run` and constructor arguments; the surface owns the mapping and the user-argument clamping, PY-I10; the components own the semantics).

## j. Observability

`RunReport` (returned and printable); `amoru.inspect_host()`; `tracing` subscriber installed by the facade with level from `AMORU_LOG` (default warn) to stderr; `AMORU_LOG_JSON=1` for JSON lines.

## k. Tests

Python tests run under the four interpreter builds in CI; Rust facade tests use the contracts d.15 fakes through `Runtime::run_with` and name only their knobs.

**PY-T1 startup_order.** `run_with` with `FakeAllocator`, `FakeReactor`, `FakeTrace`, `FakeSampler::scripted`, `FakePlacement::with_manifest_store()`, a `FakeSource::splits(4, 1000, 1 MiB)`, a `FakeSink::resumable(true)` and two `FakeKernel`s (one `stateful(2, 0)`): the order of first calls observed on the fakes (`FakeKernel.init_calls`, `FakeSink.open_calls` then `written()`, `FakePlacement.pushed(0)`, `FakeTrace.records()`) equals PY-I1, in success and with an injected failure at each step (`FakeSink::fail_at(seq)`, `FakeSource::fail_split(id)`, `FakeReactor::fail_next(op, n)`, a `FakeKernel::fail_on([0])` and a test-local kernel whose `init` fails); after a failure every started fake has `shutdown_calls == 1` (contracts d.15: `FakeReactor`, `FakePlacement`, `FakeSink`) and nothing after it. PY-I1.

**PY-T2 exception_mapping.** Through `run_with`, each `AmoruError` variant raised from a fake (`FakeSource::fail_split`, `FakeSink::fail_at`, `FakeKernel::fail_on` (by apply index, contracts d.15), `FakeAllocator::fail_next`, `FakeReactor::fail_next`, a cancelled token) arrives as the mapped class with the diagnostic dict populated; a `Terminated` outcome arrives with `.report` and `.manifest` set. PY-I2.

**PY-T3 no_parameters.** (integration, closes in wave 5) In a container with limits, `amoru.run(src, [k], sink)` completes for the identity and normalise kernels. PY-I3, S2.

**PY-T4 gil_refused.** Under a GIL interpreter, a Python kernel raises `ConfigError` with the exact message; `allow_gil=True` runs and the report says `gil_serialised` with the stage listed in `gil`. PY-I4.

**PY-T5 report_object.** Attributes equal the field list of `amoru_trace::RunReport`, `__str__` line count ≤ 40, `to_json` round-trips; `_core.pyi` names every public symbol of `amoru._core` and nothing else (checked by introspection). PY-I5, e.3.

**PY-T6 keyboard_interrupt.** Send SIGINT during a 10 s run; `Cancelled` raised within 2 s of the longest kernel; partial report attached; the signal was observed on the main thread and the cancel token set from it (thread-id assertion); a second SIGINT during cancellation is swallowed. PY-I6, f.5.

**PY-T7 wheel_matrix.** CI builds the six wheels (two builds by three platforms) and imports each; `sys._is_gil_enabled()` is false on the free-threaded build after import and true on the standard one, and true on the free-threaded build under `PYTHON_GIL=1`; every `amoru._core` class is frozen (setting an attribute raises). PY-I7.

**PY-T8 no_pandas.** `import amoru` does not import pandas (checked via `sys.modules`). PY-I8.

**PY-T9 reentrancy.** Second concurrent `run` raises `ConfigError`. g.

**PY-T10 end_to_end_benchmarks.** (reference host, E1) The full suite: S1, S3, S4, S12 numbers in the report meet the criteria; recorded with the host name. S12 in particular: identity kernel with 256 MiB morsels within 5% of the plain-loop baseline.

**PY-T11 speedup.** (reference host, E1; free-threaded, 8 cores) NumPy GIL-releasing kernel: ≥ 5.6× one-worker throughput. S8.

**PY-T12 resume_end_to_end.** (integration, closes in wave 5; real components, temp staging directory) A three-kernel run to a `ParquetSink` is killed by `SIGKILL` from a subprocess harness at five points; `amoru.run(..., resume="auto")` completes each time; the output directory read back equals an uninterrupted run; the report says `resumed` and its `run_id` equals the first run's; a resume with a different kernel list raises `ResumeError` naming the stage before any file is written; a resume with a `resume="forbid"` kernel raises `ResumeError`; `keep_checkpoint=True` leaves the manifest after completion. PY-I9, S17.

**PY-T15 defaults_that_make_resume_work.** `profiles_dir: None` resolves to `<home>/.amoru/profiles` and the directory is created (the resolver takes the home directory as an argument, so the test needs no environment mutation); and, through `run_with`, a completed run with `checkpoint_keep` and a `checkpoint_interval_ms` longer than the run leaves a manifest, while the same run without `checkpoint_keep` leaves none and the report names none. The report's own `manifest` field comes from `PlacementEngine::manifest_path` (f.2), which an injected `FakePlacement` does not have, so that half is the end-to-end suite's. 12 f.1, f.7, SC f.12.

**PY-T13 configuration_clamping.** For every row of the preamble's configuration table: a value below the range and one above it, passed through `amoru.run` arguments where the owner is `user` (through `run_with` and a `RunSpec` otherwise), arrives at the component clamped to the bound, exactly one note names the clamp, and rows whose owner is `controller` reach the scheduler unclamped by the surface and are clamped there (`SchedulerStats.knob_clamps` counts them) while `budget.*` rows are clamped by discovery; a `fixed` row rejects a foreign value with `ConfigError`. PY-I10 (preamble section 5).

**PY-T14 sink_equals_source.** `ParquetSink("s3://b/in/")` with `ParquetSource("s3://b/in/")`, `ParquetSink("s3://b/")` with the same source, and `ParquetSink("file:///data/out")` with `ParquetSource("/data/out/part.parquet")` each raise `PlanError` before anything starts; `ParquetSink("s3://b/in2/")` does not. f.3.

**PY-T17 arena_share_from_the_chain.** The arena's share of the allowance falls as the chain's declared out-of-arena cost rises and is exactly `2 / (2 + a_anon)` of it at the measured 4.5; the bytes the arena gives up are headroom above it, more than four times the reserve at a 512 MiB ceiling where the rule it replaces left exactly the reserve; a stage that declares nothing counts as 4.0 and the largest declaration in a chain is the one that governs; a budget too small for a share to be a working region gets the 256 MiB floor, and one smaller than the floor gets its whole allowance and one with nothing left still gets nothing; a non-finite or negative declaration is treated as no declaration. And end to end, the greedy Python kernel of `python/tests/test_budget.py` completes inside 512 MiB. f.1, S1.

**PY-T16 python_kernel_throughput.** A `@amoru.kernel` that appends an uppercased column over 200,000 rows, to a `ParquetSink` given a bare filesystem path, driven in a subprocess with a timeout: the run completes, the output has the appended column and all 200,000 rows, and the whole process finishes inside a wall clock bound, which the test prints whether or not it fires. The bound is loose by design (the Rust half of the same job is `examples/append_column`); what it catches is a boundary that crosses per row rather than per morsel, and a run that never returns at all. Every other test in this section uses ten thousand rows or fewer and a `file://` destination, which is why both of those reached a built wheel. f.1, S8.

## l. Implementation notes for the agent

Rust files: `crates/amoru-runtime/src/{lib.rs, spec.rs (RunSpec, Components), run.rs (f.1, f.7), report.rs (f.2), cancel.rs}` (wave 4, so RC-T12 can drive real components through it); `crates/amoru-py/src/{lib.rs (module, gil_used = false), sources.rs, sinks.rs, kernel.rs (decorator support: a frozen `#[pyclass] KernelSpec` holding the `PyKernel`), run.rs (f.3, f.4, f.5), report.rs, errors.rs (e.2), inspect.rs}` and the Python files per e.3 (wave 5). Every `#[pyclass]` is `frozen`; state that must change lives in Rust behind a `Mutex` or is returned as a new object. `_core.pyi` is written by hand and kept in step by PY-T5. `unsafe` not permitted outside what PyO3 generates.

Set the global allocator to `mimalloc` in `amoru-runtime` (`#[global_allocator]`), which also covers the Python module since it links the runtime.

`pyproject.toml`: `[build-system] requires = ["maturin>=1.7"]`, `[tool.maturin] features = ["python"]`, `python-source = "python"`, `module-name = "amoru._core"`; CI matrix over `cp313`, `cp313t`, `cp314`, `cp314t` × `manylinux_2_28_x86_64`, `manylinux_2_28_aarch64`, `macosx_11_0_arm64` (twelve wheels); features `uring` on Linux, `cuda` and `gds` off in published wheels (a separate `amoru-cuda` wheel is a later decision; record in o).

Anti-patterns: no logic in `__init__.py` beyond argument translation; no retry loops in the facade (components own retries); no swallowing of component errors during shutdown (collect and attach as notes, raise the first); no checkpoint call from the facade; no `Python<'py>` held on the runtime thread.

## m. Open items

None. (The GPU-wheel question is PY-O1 in section o; it does not block hand-off.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| S2, G-I10 | PY-I3 | PY-T3 |
| S8, G-I9, D4 | PY-I4, PY-I7 | PY-T4, PY-T7, PY-T11 |
| S12 | f.1 (no overhead in the facade) | PY-T10 |
| S9 | PY-I5 | PY-T5 |
| G-I8 | PY-I2 | PY-T2 |
| preamble 4.3 | PY-I1, PY-I6 | PY-T1, PY-T6 |
| S17, D13 | PY-I9, f.7 | PY-T12 |
| preamble section 5 (clamping ownership) | PY-I10, f.3 | PY-T13 |
| architecture 7 (configuration hazards) | f.3 sink-equals-source rule | PY-T14 |
| S1 (out-of-arena allocation; architecture 8) | f.1 arena share | PY-T17 |

## o. Deferred (post-v1)

**PY-O1. GPU wheel packaging.** Whether GPU support ships as a feature of the main wheel (larger, needs CUDA at import time behind a lazy load) or as a separate `amoru-cuda` wheel. Assumption until decided: separate wheel, built by the same CI from the same source with `--features cuda,gds`.
