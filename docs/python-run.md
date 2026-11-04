# `moruna.run`

```python
moruna.run(
    source=None, kernels=None, sink=None, *, graph=None,
    budget=None, cpu=None, trace=None,
    staging_dir=None, staging_limit=None, on_error="terminate",
    ordered=False, sizer="rule", profiles_dir=None, storage=None,
    host_profile=None, allow_gil=False, checkpoint=True,
    checkpoint_interval=5.0, keep_checkpoint=False, resume=None,
) -> moruna.RunReport
```

Runs the source through each kernel and writes results to the sink, or runs the DAG `graph` (a [`moruna.Graph`](#morunagraph)). Pass a source, kernels and a sink, or `graph=`, not both. `kernels` may be one kernel or a list in execution order. A plain callable is wrapped with the defaults of `moruna.kernel`; an empty list copies the source.

| Parameter | Type and default | Meaning |
| --- | --- | --- |
| `source` | source handle | Built-in source or `Source` subclass instance. |
| `kernels` | kernel or list | `KernelSpec`, `StdKernel`, plain callable, or a list of these. |
| `sink` | sink handle | Built-in sink or `Sink` subclass instance. |
| `graph` | `Graph \| None = None` | A DAG of named tasks, in place of `source`, `kernels` and `sink`. |
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

## `moruna.Graph`

```python
g = moruna.Graph()
g.source(name, source) -> str
g.kernel(name, kernel, inputs) -> str      # inputs: a task name or a list of them
g.sink(name, sink, input) -> str
g.tasks -> tuple[moruna.GraphTask, ...]    # (role, name, object, inputs), in the order added
```

A run as a DAG of named tasks. A task takes its inputs from the tasks it names, which may be added before or after it. One task's output may feed several tasks (fan-out), and a kernel may take several inputs (fan-in), each on a port named after the task that feeds it. Branches that are ready at the same time run at the same time on the worker pool. `moruna.std.join()` puts the columns of branches of one upstream side by side, morsel by morsel; `moruna.std.union()` passes on every batch of every input.

Adding a name twice raises `ValueError`; a name or an input that is not a string raises `TypeError`. `moruna.run(graph=g)` refuses unknown names, cycles and outputs no task reads with one `ConfigError` that names every one of them, before anything starts.

```python
import moruna
import pyarrow as pa
import pyarrow.compute as pc

@moruna.kernel
def words(batch: pa.RecordBatch) -> pa.RecordBatch:
    return pa.record_batch({"words": pc.count_substring(batch["text"], " ")})

@moruna.kernel
def length(batch: pa.RecordBatch) -> pa.RecordBatch:
    return pa.record_batch({"length": pc.utf8_length(batch["text"])})

g = moruna.Graph()
g.source("rows", moruna.ParquetSource("reviews.parquet", columns=["id", "text"]))
g.kernel("words", words, inputs="rows")
g.kernel("length", length, inputs="rows")
g.kernel("all", moruna.std.join(), inputs=["rows", "words", "length"])
g.sink("out", moruna.ParquetSink("reviews-scored"), input="all")
report = moruna.run(graph=g, budget="2GiB")
```

Each row of `reviews-scored` holds `id`, `text`, `words` and `length`, the same rows the chain `[words, length]` would give if each kernel appended its column.
