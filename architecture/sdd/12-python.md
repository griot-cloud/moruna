# Amoru SDD 12: Python surface and runtime facade (`amoru-py`, `amoru-runtime`, `python/amoru`)

**Document type:** software design document, component 12 of 12
**Status:** DRAFT · 2026-09-15
**Parent:** `architecture/amoru-runtime-design.md` section 5.10, 6 (hosting), 4.3 (D4); criteria S2, S8, S12; global invariants G-I9, G-I10
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` (all)
**Component location:** `crates/amoru-runtime` (facade, Rust), `crates/amoru-py` (PyO3 module), `python/amoru` (package), Python 3.13 and 3.14
**Consumes:** every component. **Consumed by:** users.

**Decisions worth your eye:** (1) the runtime facade `Runtime::run` is in Rust and is the only place components are wired, so the Python package has no orchestration logic; (2) the Python API is four constructors, one decorator and one function, with keyword-only arguments and no configuration object; (3) a Python kernel under a GIL interpreter is refused by default with a message that names the fix, rather than silently serialised.

---

## a. Purpose and boundary

Two pieces. The runtime facade wires discovery, arena, reactor, placement, scheduler, controller and trace into `Runtime::run`, in a fixed order, and produces the run report; it is the only component that knows the order of startup and shutdown. The Python surface exposes the facade and the sources, sinks and kernel adapter to Python users with as little of its own logic as possible, packages the wheel, and holds the documentation users read.

It owns: startup and shutdown sequencing; error-to-exception mapping; the public Python API; the wheel build; GIL refusal; the report's Python rendering; the end-to-end benchmark harness.

It refuses to know: anything a component owns. If logic appears here that is not sequencing or translation, it belongs in a component.

## b. Vocabulary

**Facade.** `amoru_runtime::Runtime`, the Rust entry point; `amoru.run` calls it.

**Session.** One `Runtime::run` invocation from start to report.

**Kernel object.** What `@amoru.kernel` produces: a Python object carrying the `PyKernelSpec` fields, passed to `run`.

**Handle.** A Python object wrapping a Rust source or sink (`ParquetSource`, `TensorSource`, `IteratorSource`, `ParquetSink`, `TensorSink`, `ArrowIpcSink`), constructed eagerly (Parquet footers are read at construction so errors surface early).

## c. Invariants

**PY-I1. Startup order is fixed.** `Runtime::run` performs, in this order and no other: discover → arena → reactor → trace → build sources and sinks (needs reactor) → build kernels (adapters) → placement → scheduler (validates the chain, spawns pools; the sink is opened inside `run`) → controller `prepare` → kernel `init` via scheduler → controller `probe_all` → controller `start` → scheduler `run` → controller `stop` → trace `finish` → report. Shutdown is the reverse of what was started, always executed, including on error. A resumed run (f.7) differs in exactly three steps: `placement.restore` right after placement is built, `scheduler.resume` in place of `scheduler.run` (which also resumes the sink in place of opening it), and controller `probe_missing` in place of `probe_all`.

**PY-I2. Every error reaches Python as one exception type with a structured payload.** `amoru.AmoruError` with `.kind` (the `AmoruError` variant name), `.message`, and `.diagnostic` (a dict with seq, stage, features, footprint, budget where present); subclasses `PlanError`, `KernelError`, `BudgetError`, `IoError`, `ConfigError`, `Cancelled` for `except` ergonomics.

**PY-I3. No parameters beyond the budget are required.** `amoru.run(source, kernels, sink)` with no other arguments completes every benchmark inside a cgroup; outside one, `budget=` is the only argument a user may need. Upholds G-I10, S2.

**PY-I4. GIL refusal is the default.** If any kernel is Python and `python_gil_enabled()` is true and `allow_gil` is false, `run` raises `ConfigError` before starting anything, with the message "Python kernels require a free-threaded interpreter (python3.13t or python3.14t); pass allow_gil=True to run serialised". Upholds G-I9, S8.

**PY-I5. The report is returned, not only printed.** `run` returns a `RunReport` object with attributes matching `amoru_trace::RunReport`, `__str__` giving the fixed layout, `.to_json()`, and `.trace_path` when a trace was written.

**PY-I6. Cancellation is honoured.** `KeyboardInterrupt` during `run` sets the cancel token, waits for the runtime's cancellation sequence (preamble 4.3), and re-raises as `amoru.Cancelled` with the partial report attached.

**PY-I7. Wheels are version-specific and thread-safe.** The module is built with `gil_used = false`, for CPython 3.13, 3.13t, 3.14, 3.14t, on `manylinux_2_28` x86_64 and aarch64 and `macosx` arm64; no abi3.

**PY-I8. Nothing on the Python side allocates or touches payload bytes.** The package never imports `pandas`; it accepts and returns `pyarrow` and DLPack objects through the adapters only.

**PY-I9. A run that can be resumed says so, and a resume never starts from the wrong run.** Every `Terminated` or `Cancelled` outcome with a manifest carries `.manifest` (path) and `.run_id` on the exception and the partial report; `amoru.run(..., resume=<run_id | path | "auto">)` refuses, before any write, a manifest whose run id, plan or kernels differ from what was passed (`Placement::restore`), so a user cannot resume yesterday's job over today's inputs by accident. Upholds S17.

## d. Interfaces

### d.1 Runtime facade (Rust)

```rust
pub struct RunSpec {
    pub source: Arc<dyn Source>,
    pub kernels: Vec<Arc<dyn Kernel>>,
    pub sink: SinkHandle,
    pub budget: Option<u64>, pub cpu: Option<f64>,
    pub trace_path: Option<PathBuf>, pub staging_dir: Option<PathBuf>, pub staging_limit: Option<u64>,
    pub error_policy: ErrorPolicy, pub ordered: bool,
    pub sizer: SizerKind, pub profiles_dir: Option<PathBuf>,
    pub object_store: ObjectStoreConfig,
    pub host_profile: Option<HostProfile>,
    pub allow_gil: bool,
    pub checkpoint: bool, pub checkpoint_interval_ms: u64, pub checkpoint_keep: bool,
    /// None: fresh run. Some: resume from this manifest (found by run id or path; "auto" resolved by the surface to the newest under `staging_dir`).
    pub resume: Option<PathBuf>,
}

pub struct Runtime;
impl Runtime {
    /// PY-I1. Blocks until the run ends. `cancel` is polled by the scheduler.
    pub fn run(spec: RunSpec, cancel: CancelToken) -> Result<RunReport>;
    /// Discovery only, for `amoru.inspect_host()`.
    pub fn inspect(input: &DiscoveryInput) -> Result<Discovered>;
}
```

### d.2 Python API (`python/amoru/__init__.py`)

```python
def run(source, kernels, sink, *, budget=None, cpu=None, trace=None, staging_dir=None,
        staging_limit=None, on_error="terminate", ordered=False, sizer="rule",
        profiles_dir=None, storage=None, host_profile=None, allow_gil=False,
        checkpoint=True, checkpoint_interval=5.0, keep_checkpoint=False,
        resume=None) -> RunReport
    # resume: None | "auto" | run id (str) | path to a manifest.json

def kernel(fn=None, *, stateful=False, instances=1, device_memory=False,
           accepts="table", tier="host", releases_gil=None,
           expected_amplification=None, preferred_rows=None,
           resume="reinit")   # "reinit" | "checkpoint" | "forbid"; "checkpoint" requires the
                              # kernel object to define checkpoint(state) -> bytes and restore(ctx, bytes) -> state

class ParquetSource:   def __init__(self, urls: str | list[str], *, columns=None, filters=None)
class TensorSource:    def __init__(self, paths: str | list[str], *, tensors=None)
class IteratorSource:  def __init__(self, iterable, *, schema: pyarrow.Schema | tuple[str, tuple[int, ...]])
class ParquetSink:     def __init__(self, url: str, *, row_group_bytes=None, file_bytes=None, compression="zstd")
class TensorSink:      def __init__(self, path: str, *, format="amb1", one_file_per_morsel=False, name="tensor")
class ArrowIpcSink:    def __init__(self, path: str, *, file_bytes=None)

class RunReport:  # attributes per amoru_trace::RunReport; __str__; to_json(); trace_path; run_id; manifest (None when the run completed and the checkpoint was not kept)
class AmoruError(Exception): kind: str; message: str; diagnostic: dict; run_id: str | None; manifest: str | None
class ResumeError(AmoruError) ...   # a manifest that cannot be used; message names the first mismatch
class PlanError(AmoruError) ...; KernelError; BudgetError; IoError; ConfigError; Cancelled

def inspect_host() -> dict            # discovered limits, resolved host profile, notes
def polars(expr_or_fn, *, accepts="table")  # wraps a Polars lazy transform as a kernel (uses the adapters' Rust path through pyo3-polars when available, else a Python kernel that calls Polars)
__version__: str
```

`storage=` takes a dict of object-store options (`endpoint`, `region`, `access_key`, `secret_key`, `session_token`, `allow_http`) or a `pyarrow.fs`-style URL scheme mapping; credentials also come from the environment as the `object_store` crate reads them.

### d.3 Consumed

Every crate's public constructor; `pyo3` 0.28+ with `gil_used = false`; `pyo3-arrow`; `maturin` for the build; `pyarrow` ≥ 17 at runtime (import check with a clear error).

## e. Data model, formats and state machines

### e.1 Session state machine

`Configuring` (Python constructs handles and kernel objects) → `Starting` (PY-I1 steps up to controller start) → `Running` → `Finishing` | `Terminating` | `Cancelling` → `Done`. Errors in `Starting` unwind what was started in reverse order before raising.

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
python/amoru/py.typed, _core.pyi   type stubs generated from the Rust module
pyproject.toml                  maturin backend; wheel matrix in CI
```

## f. Algorithms and policies

**f.1 `Runtime::run` sequence.** Exactly PY-I1. Between steps: after discovery, if `spec.host_profile` is given it overrides the environment; after the arena, the reactor receives `arena.is_pinned()`; after sources are built, `plan()` is called once to get the total planned bytes for the controller's tiny-dataset check (f.10 of component 11) and to fail early on plan errors; kernels' `init` happens inside the scheduler's startup on the workers that will own instances; `probe_all` runs with the scheduler's `probe`; `run` blocks; on any `Err` from a step, every started component is shut down in reverse (`controller.stop`, `scheduler` drop joins workers, `placement.shutdown`, `reactor.shutdown`, `trace.finish`, arena drop) and the error is returned with the partial report attached where one exists.

**f.2 Report assembly.** `RunMeta` from: exit reason, start and end instants, `Discovered.notes`, `reactor.paths()` and stats, `adapters` GIL state per Python kernel, `ControllerSummary`; `RunReport::compute(trace.finish()?, &limits, &meta)`.

**f.3 `amoru.run` argument translation.** Strings for sizes (`"6GiB"`) go through discovery's parser (component 3 f.3); `on_error` maps `"terminate" | "skip" | ("budget", n)`; `kernels` may be a single kernel or a list; a plain function passed as a kernel is wrapped as if decorated with defaults; `ordered=True` wraps the sink in `ReorderBuffer` with `ordering.buffer_bytes`.

**f.4 GIL check (PY-I4).** Before any Rust component starts: if any kernel is a Python kernel, call `amoru_adapters::python_gil_enabled()`; refuse unless `allow_gil`.

**f.5 `KeyboardInterrupt`.** `run` releases the interpreter (`Python::detach`) while the Rust runtime blocks, and installs a SIGINT handler through PyO3's `check_signals` polling every 100 ms from the facade's waiting thread; on signal, set the cancel token, wait, raise `Cancelled`.

**f.6 `polars()` helper.** If `amoru._core` was built with the `polars` feature and the argument is a Rust plugin name, use the plugin path; otherwise wrap a Python function `lambda batch: pl.from_arrow(batch).pipe(fn).to_arrow()` as a stateless Python kernel (this path copies inside Polars on some types; the docstring says so).

**f.7 Resume path.** With `resume` set: resolve it to a manifest path (`"auto"`: `PlacementEngine::find_manifest(staging_dir, None)`; a run id: `find_manifest(staging_dir, Some(id))`; a path: as given; nothing found is `ResumeError`); refuse with `ResumeError` if `source.repeatable()` is false; read the manifest header (`PlacementEngine::read_manifest_header`) and build `PlacementConfig` with its `run_id` and `node`, so the new process continues the old run's identity rather than minting a new one; run PY-I1 with the three substitutions: after placement is built, `placement.restore(&manifest, &plan, &fingerprints)` (its refusals surface as `ResumeError` before the sink is touched); `controller.probe_missing` instead of `probe_all`; `scheduler.resume(point, cancel)` instead of `run`. `checkpoint=True` is the default whenever a staging directory exists and the source is repeatable (discovery finds or creates one; `staging_dir=None` with no discoverable directory disables both staging and checkpointing with a note in the report); on `Completed`, the run directory is removed unless `keep_checkpoint`; on `Terminated` or `Cancelled`, the final manifest is written and its path is put on the exception and the partial report, and the message ends with "resumable: pass resume=\"<run_id>\"" when it is. The resume argument is the only new parameter a user meets, and only after a failure; PY-I3 holds.

## g. Concurrency within the component

`run` is not re-entrant: a second `run` in the same process while one is active raises `ConfigError` (the arena is a process-wide reservation). A module-level mutex enforces it. The facade's waiting thread and the interpreter's main thread are the only Python-side threads.

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

**Edge cases.** `kernels=[]`: allowed; the pipeline is source → sink (a copy or format conversion). A sink URL equal to a source URL: `PlanError` (would overwrite input). `budget` larger than the cgroup ceiling: clamped with a warning in `report.notes`. `trace=` to a directory: file named `amoru-<run_id>.arrow` inside it.

**Failures.** `pyarrow` missing: `ImportError` at `import amoru` with the install hint. A kernel object not produced by `@amoru.kernel` and not callable: `TypeError` in `run` before anything starts. Any component failure: PY-I2 mapping with the partial report on the exception as `.report` when available.

## i. Configuration

`python.allow_gil`, `trace.path`, `errors.policy`, `ordering.required`, `sizer`, `profiles.dir`, `budget.*`, `staging.*` (as `run` arguments; the surface owns the mapping, the components own the semantics).

## j. Observability

`RunReport` (returned and printable); `amoru.inspect_host()`; `tracing` subscriber installed by the facade with level from `AMORU_LOG` (default warn) to stderr; `AMORU_LOG_JSON=1` for JSON lines.

## k. Tests

Python tests run under the four interpreter builds in CI; Rust facade tests use fakes.

**PY-T1 startup_order.** Instrumented components record their start and stop order; equals PY-I1 in both success and each injected-failure step. PY-I1.

**PY-T2 exception_mapping.** Each `AmoruError` variant raised from a fake component arrives as the mapped class with the diagnostic dict populated. PY-I2.

**PY-T3 no_parameters.** In a container with limits, `amoru.run(src, [k], sink)` completes for the identity and normalise kernels. PY-I3, S2.

**PY-T4 gil_refused.** Under a GIL interpreter, a Python kernel raises `ConfigError` with the exact message; `allow_gil=True` runs and the report says `gil_serialised`. PY-I4.

**PY-T5 report_object.** Attributes, `__str__` line count ≤ 40, `to_json` round-trips. PY-I5.

**PY-T6 keyboard_interrupt.** Send SIGINT during a 10 s run; `Cancelled` raised within 2 s of the longest kernel; partial report attached. PY-I6.

**PY-T7 wheel_matrix.** CI builds the eight wheels and imports each; `sys._is_gil_enabled()` is false on the `t` builds after import. PY-I7.

**PY-T8 no_pandas.** `import amoru` does not import pandas (checked via `sys.modules`). PY-I8.

**PY-T9 reentrancy.** Second concurrent `run` raises `ConfigError`. g.

**PY-T10 end_to_end_benchmarks.** (reference host) The full suite: S1, S3, S4, S12 numbers in the report meet the criteria; recorded with the host name. S12 in particular: identity kernel with 256 MiB morsels within 5% of the plain-loop baseline.

**PY-T11 speedup.** (free-threaded, 8 cores) NumPy GIL-releasing kernel: ≥ 5.6× one-worker throughput. S8.

**PY-T12 resume_end_to_end.** (real components, temp staging directory) A three-kernel run to a `ParquetSink` is killed by `SIGKILL` from a subprocess harness at five points; `amoru.run(..., resume="auto")` completes each time; the output directory read back equals an uninterrupted run; the report says `resumed`; a resume with a different kernel list raises `ResumeError` naming the stage before any file is written; a resume with a `resume="forbid"` kernel raises `ResumeError`; `keep_checkpoint=True` leaves the manifest after completion. PY-I9, S17.

## l. Implementation notes for the agent

Rust files: `crates/amoru-runtime/src/{lib.rs, spec.rs, run.rs (f.1), report.rs (f.2), cancel.rs}`; `crates/amoru-py/src/{lib.rs (module, gil_used = false), sources.rs, sinks.rs, kernel.rs (decorator support: a `#[pyclass] KernelSpec`), run.rs (f.3, f.4, f.5), report.rs, errors.rs (e.2), inspect.rs}`. Python files per e.3. `unsafe` not permitted outside what PyO3 generates.

Set the global allocator to `mimalloc` in `amoru-runtime` (`#[global_allocator]`), which also covers the Python module since it links the runtime.

`pyproject.toml`: `[build-system] requires = ["maturin>=1.7"]`, `[tool.maturin] features = ["python"]`, `python-source = "python"`, `module-name = "amoru._core"`; CI matrix over `cp313`, `cp313t`, `cp314`, `cp314t` × `manylinux_2_28_x86_64`, `manylinux_2_28_aarch64`, `macosx_11_0_arm64`; features `uring` on Linux, `cuda` and `gds` off in published wheels (a separate `amoru-cuda` wheel is a later decision; record in m).

Anti-patterns: no logic in `__init__.py` beyond argument translation; no retry loops in the facade (components own retries); no swallowing of component errors during shutdown (collect and attach as notes, raise the first).

## m. Open items

Whether GPU support ships as a feature of the main wheel (larger, needs CUDA at import time behind a lazy load) or as a separate `amoru-cuda` wheel. Assumption until decided: separate wheel, built by the same CI from the same source with `--features cuda,gds`.

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
