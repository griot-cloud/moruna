# Results and errors

## `moruna.RunReport`

`moruna.run(...) -> RunReport`. The runtime computes it; it is not edited. `str(report)` returns a page of at most forty lines; [The run report](run-report.md) details the fields Moruna's trace adds.

A report is one JSON document with one canonical form. `report.bytes()` is that document as UTF-8 JSON with its keys sorted at every level and no whitespace between tokens, so the same report always gives the same bytes; `report.digest()` is `"sha256:"` and the hex of SHA-256 over exactly those bytes; `report.to_json()` is the same text, and `moruna.RunReport.from_bytes(data)` reads the bytes (or a file of them) back, refusing bytes that are not a report in that form with `ValueError`. A host that forwards the bytes unchanged can certify them by that digest, beside the spec's. `moruna.run(..., spec_to=target)` writes them through its writer after the run.

| Attribute | Type | Meaning |
| --- | --- | --- |
| `schema_version`, `moruna` | `str`, `str` | The document's schema, `"MAJOR.MINOR"` (`"2.1"`; a reader refuses an unknown major), and the Moruna that wrote it. |
| `host`, `args` | `dict`, `dict` | Where it ran (platform, CPUs, memory limit, CPU quota, devices) and the run's argument values. |
| `run_id` | `str` | The run's id, 32 hex characters. |
| `job_digest` | `str \| None` | The spec's public digest, SHA-256 of its exact bytes (`JobSpec.digest()`), the same from `moruna.run` and `moruna run`; `None` for a job that holds a task a spec cannot hold. |
| `execution_fingerprint` | `str \| None` | The normalized execution fingerprint the planner and recovery use. It differs from `job_digest` and is not a document identity; `None` for a run refused before its job was built. |
| `exit` | `dict` | `code` (0 completed, 130 cancelled, otherwise the command's exit code for the failure), `status` (`"completed"`, `"terminated"`, `"cancelled"`) and `diagnostic`. |
| `tasks` | list of dicts | Per task, in job order, keyed by its name in the spec: its status, times, CPU, memory, rows, bytes and morsels, waits, needs, what the plan predicted, its observation and a recommendation (see [The run report](run-report.md#task-fields)). A measurement not made is absent. |
| `resources` | `dict` | `cpu_core_s`, `peak_memory_bytes`, `mem_byte_seconds`, `usage_measured` and `memory_basis`. |
| `sinks` | list of dicts | Per sink: `task`, `target` (as its settings name it), `rows`, `bytes`, `files`, `excluded_rows`, and `snapshot_id` and `version` when it committed a version. |
| `plan`, `plan_steps` | `dict \| None`, list \| None | The plan the run started from, when `moruna.run(..., explain=True)` asked for it, and each step's actual start and end. |
| `trace_path` | `str \| None` | Written trace file, if requested. |
| `resumed`, `manifest`, `wall_ms` | `bool`, `str \| None`, `float` | Resume state, latest checkpoint path, elapsed milliseconds. |
| `limits_initial`, `limits_timeline` | `dict`, list | Initial host limits and limit changes. |
| `peak_anon_bytes`, `peak_ceiling_bytes`, `peak_fraction_of_ceiling` | `int`, `int`, `float` | Peak memory, denominator and fraction. |
| `worker_busy_fraction`, `cpu_throttled_fraction` | `float` | Worker use and CPU throttling. |
| `source_bytes_per_s`, `source_bandwidth`, `staging_bandwidth` | `float` | Source and staging rates. |
| `staging_bytes_written`, `staging_engaged` | `int`, `bool` | Staging volume and whether used. |
| `io_paths` | `dict` | Direct I/O, io_uring, GDS, pinned and RDMA flags. |
| `gil`, `gil_serialised` | list of pairs, `bool` | Per-stage GIL state and serialization. |
| `sizer_used`, `sizer_fallback_at` | `str`, `int \| None` | Sizer and optional fallback morsel. |
| `drains`, `bottleneck_timeline` | lists | Arena drains and bottlenecks. |
| `notes`, `overflow_failed`, `late_records` | `list[str]`, `bool`, `int` | Decisions and trace completeness. |
| `write`, `read` | `dict \| None` | What the engine behind a governed sink or source reported. |

## `moruna.inspect_host`

```python
moruna.inspect_host() -> dict
```

Runs host discovery without a job. The dictionary contains `limits` (`memory_ceiling`, `memory_kill`, `cpu_quota`, `page_bytes`, `source`, `devices`), `anon_bytes` (memory already held), `host_profile` (feature guarantees and staging location), `host_tier`, `cgroup_path`, and `notes`. Discovery failures raise a `MorunaError` subclass.

## `moruna.check`

```python
moruna.check(target, *, function=None, seed=0, warm_up=True, store=None, budget=60.0,
             statistics=None, model=None, sandbox_python=None, sandbox_cgroup=None,
             args=None, assets=None) -> dict
```

Checks every function in `target` (a module name, a `.py` file or a package) against its declared schemas and warms up each one that agrees, as `moruna check --json` does, and returns the same report: `exit`, `error` and `functions`, one report per function, each naming its `function`. A function's `warm_up` holds `cost_per_10000_rows` (each field a `p50`, `p95`, `unit`, `basis` and `observations`), `kernel` (start-up, speed-up at 2 and 4 workers, state and checkpoint), `skew` and `selectivity` (marked `value_dependent`, since generated data has no real distribution), `progress` (how far the warm-up got and why it stopped), and `consumed` (wall and worker seconds, memory and the bytes written to the store, for charging). When a warm-up cannot run, `warm_up` holds `error` instead and `exit` is 2. `store` is the observation store's directory, `budget` the warm-up's seconds, `statistics` the input table's statistics (a dictionary, or a path to a JSON file) and `model` the cost model by name.

## Exceptions

Runtime exceptions inherit from `moruna.MorunaError`, which inherits from `Exception`. Runtime-created instances expose `kind: str`, `message: str`, `diagnostic: dict`, `run_id: str | None`, `manifest: str | None`, and `report: RunReport | None`. Constructor validation can instead raise ordinary `TypeError` or `ValueError`.

| Class | Typical cause |
| --- | --- |
| `moruna.ConfigError` | Bad run configuration or unsupported host setting. |
| `moruna.PlanError` | Incompatible source, function or sink. |
| `moruna.BudgetError` | Memory budget or guarded allocation exceeded. |
| `moruna.FunctionError` | A function failed and the error policy stopped the run. |
| `moruna.IoError` | Source, sink, object store or staging I/O failed. |
| `moruna.ResumeError` | Checkpoint cannot be used. |
| `moruna.Cancelled` | Interrupted run; `error.report` may contain partial progress. |

```python
try:
    report = moruna.run(job)
except moruna.MorunaError as error:
    print(error.kind, error.message, error.diagnostic)
    if error.report is not None:
        print(error.report.run_id)
```
