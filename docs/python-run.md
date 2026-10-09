# `moruna.run`

```python
moruna.run(
    source, kernels, sink, *, budget=None, cpu=None, trace=None,
    staging_dir=None, staging_limit=None, on_error="terminate",
    ordered=False, sizer="rule", profiles_dir=None, storage=None,
    host_profile=None, allow_gil=False, checkpoint=True,
    checkpoint_interval=5.0, keep_checkpoint=False, resume=None,
) -> moruna.RunReport
```

Runs the source through each kernel and writes results to the sink. `kernels` may be one kernel or a list in execution order. A plain callable is wrapped with the defaults of `moruna.kernel`; an empty list copies the source.

| Parameter | Type and default | Meaning |
| --- | --- | --- |
| `source` | source handle, required | Built-in source or `Source` subclass instance. |
| `kernels` | kernel or list, required | `KernelSpec`, `StdKernel`, plain callable, or a list of these. |
| `sink` | sink handle, required | Built-in sink or `Sink` subclass instance. |
| `budget` | `int \| str \| None = None` | Whole-process memory ceiling in bytes or a size string such as `"8GiB"`; `None` uses discovery. |
| `cpu` | `float \| None = None` | CPU quota; `None` uses discovery. |
| `trace` | `str \| None = None` | File or directory for a per-morsel trace. |
| `staging_dir` | `str \| None = None` | Directory for staged data and checkpoints. |
| `staging_limit` | `int \| str \| None = None` | Maximum staging bytes; default is 20% of available space. |
| `on_error` | `str \| tuple[str, int] = "terminate"` | `"terminate"`, `"skip"`, or `("budget", n)` to allow at most `n` failed morsels. |
| `ordered` | `bool = False` | Deliver results to the sink in source order. |
| `sizer` | `str = "rule"` | `"rule"` or `"learned"` morsel sizing. |
| `profiles_dir` | `str \| None = None` | Profile directory; defaults to `~/.moruna/profiles`. |
| `storage` | `dict \| None = None` | Connection settings for remote object-store URLs. |
| `host_profile` | `None` | Reserved. A non-`None` value currently raises `ConfigError`; use `MORUNA_HOST_PROFILE`. |
| `allow_gil` | `bool = False` | Permit Python kernels on a GIL-enabled interpreter. |
| `checkpoint` | `bool = True` | Write checkpoints while running. |
| `checkpoint_interval` | `float = 5.0` | Seconds between checkpoints. |
| `keep_checkpoint` | `bool = False` | Keep checkpoint files after success. |
| `resume` | `str \| None = None` | `"auto"`, a 32-character run id, or path to `manifest.json`. |

**Returns:** `RunReport`. **Raises:** `ConfigError`, `PlanError`, `BudgetError`, `KernelError`, `IoError`, `ResumeError`, or `Cancelled` as appropriate. Runtime exceptions may carry a partial report. Size strings accept binary units (`KiB` through `TiB`) and decimal units (`KB` through `TB`). Numeric values outside supported ranges may be clamped; `report.notes` records the change.

```python
import moruna

report = moruna.run(
    moruna.ParquetSource("orders.parquet", columns=["order_id", "amount"]),
    [moruna.std.filter("amount > 0")],
    moruna.ParquetSink("clean-orders"),
    budget="2GiB",
)
print(report.run_id, report.stages[0]["rows_out"])
```
