# Results and errors

## `moruna.RunReport`

`moruna.run(...) -> RunReport`. The runtime constructs it; attributes are read-only. `str(report)` returns a compact summary; `report.to_json() -> str` returns the full serialized report. [The run report](run-report.md) details its nested fields.

| Attribute | Type | Meaning |
| --- | --- | --- |
| `run_id`, `resumed`, `manifest` | `str`, `bool`, `str \| None` | Run id, resume state and latest checkpoint path. |
| `exit` | `str \| dict` | `"Completed"`, `"Cancelled"`, or `{"Terminated": {"diagnostic": ...}}`. |
| `wall_s` | `float` | Elapsed seconds. |
| `limits_initial`, `limits`, `limits_timeline` | `dict`, `dict`, list | Initial host limits, compatibility alias, and limit changes. |
| `peak_anon_bytes`, `peak_ceiling_bytes`, `peak_fraction_of_ceiling` | `int`, `int`, `float` | Peak memory, denominator and fraction. |
| `worker_busy_fraction`, `cpu_throttled_fraction` | `float` | Worker use and CPU throttling. |
| `cpu_ns`, `mem_byte_seconds`, `usage_measured` | `int`, `int`, `bool` | Measured CPU and memory usage. |
| `source_bytes_per_s`, `source_bandwidth`, `staging_bandwidth` | `float` | Source and staging rates. |
| `staging_bytes_written`, `staging_engaged` | `int`, `bool` | Staging volume and whether used. |
| `io_paths` | `dict` | Direct I/O, io_uring, GDS, pinned and RDMA flags. |
| `gil`, `gil_serialised` | list of pairs, `bool` | Per-stage GIL state and serialization. |
| `sizer_used`, `sizer_fallback_at` | `str`, `int \| None` | Sizer and optional fallback morsel. |
| `stages`, `drains`, `bottleneck_timeline` | lists | Per-stage statistics, arena drains and bottlenecks. |
| `notes`, `overflow_failed`, `late_records` | `list[str]`, `bool`, `int` | Decisions and trace completeness. |
| `snapshot` | `dict \| None` | Committed snapshot ids, when available. |
| `trace_path` | `str \| None` | Written trace file, if requested. |

## `moruna.inspect_host`

```python
moruna.inspect_host() -> dict
```

Runs host discovery without a job. The dictionary contains `limits` (`memory_ceiling`, `memory_kill`, `cpu_quota`, `page_bytes`, `source`, `devices`), `anon_bytes` (memory already held), `host_profile` (feature guarantees and staging location), `host_tier`, `cgroup_path`, and `notes`. Discovery failures raise a `MorunaError` subclass.

## Exceptions

Runtime exceptions inherit from `moruna.MorunaError`, which inherits from `Exception`. Runtime-created instances expose `kind: str`, `message: str`, `diagnostic: dict`, `run_id: str | None`, `manifest: str | None`, and `report: RunReport | None`. Constructor validation can instead raise ordinary `TypeError` or `ValueError`.

| Class | Typical cause |
| --- | --- |
| `moruna.ConfigError` | Bad run configuration or unsupported host setting. |
| `moruna.PlanError` | Incompatible source, kernel or sink. |
| `moruna.BudgetError` | Memory budget or guarded allocation exceeded. |
| `moruna.KernelError` | Kernel failed and the error policy stopped the run. |
| `moruna.IoError` | Source, sink, object store or staging I/O failed. |
| `moruna.ResumeError` | Checkpoint cannot be used. |
| `moruna.Cancelled` | Interrupted run; `error.report` may contain partial progress. |

```python
try:
    report = moruna.run(source, kernels, sink)
except moruna.MorunaError as error:
    print(error.kind, error.message, error.diagnostic)
    if error.report is not None:
        print(error.report.run_id)
```
