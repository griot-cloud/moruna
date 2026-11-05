# Functions

## `moruna.function`

```python
moruna.function(
    fn=None, *, stateful=False, instances=1, device_memory=False,
    accepts="table", tier="host", releases_gil=None,
    expected_amplification=None, preferred_rows=None, resume="reinit",
    state_bytes=None, input_schema=None, output_schema=None,
    lockfile=None, memory_guard=True,
) -> moruna.Function | decorator
```

Use `@moruna.function`, `@moruna.function(...)`, or `moruna.function(obj, ...)`. When `fn` is omitted, the call returns a decorator; otherwise it returns a `moruna.Function`, which a job takes as a task. A stateless function receives and returns a `pyarrow.RecordBatch`. An annotated Polars `DataFrame` or `LazyFrame` function is converted through Polars.

| Parameter | Type and default | Meaning |
| --- | --- | --- |
| `fn` | callable or object, optional | Function to wrap, or stateful object when `stateful=True`. |
| `stateful` | `bool = False` | Call `setup(ctx)` and `__call__(state, batch)` on the object. |
| `instances` | `int = 1` | Independent copies of state; at least 1. |
| `device_memory` | `bool = False` | Declare device memory use. |
| `accepts` | `str = "table"` | `"table"`, `"tensor"`, or `"either"`. |
| `tier` | `str = "host"` | `"host"`, `"device"`, or `"any"`. |
| `releases_gil` | `bool \| None = None` | Whether the callable releases Python's GIL; `None` leaves it unspecified. |
| `expected_amplification` | `float \| None = None` | Expected working memory divided by input size. |
| `preferred_rows` | `int \| None = None` | Preferred batch row count. |
| `resume` | `str = "reinit"` | `"reinit"` rebuilds state; `"checkpoint"` saves and restores it. |
| `state_bytes` | `int \| None = None` | Estimated bytes per state instance. |
| `input_schema` | schema or mapping, optional | `pyarrow.Schema` is exact; `{column: type}` requires a subset. |
| `output_schema` | schema or mapping, optional | Exact schema, required-column mapping, or relative `{"adds": {...}, "drops": [...], "changes": {...}}`. |
| `lockfile` | path or bytes, optional | Contents are included in the fingerprint; paths are read at decoration time. |
| `memory_guard` | `bool = True` | Refuse an allocation that crosses the process memory ceiling. |

Schema mappings accept Arrow types, their string spellings, Python `int`, `float`, `str`, `bool`, and supported Polars types. A stateful object supplies `setup(self, ctx) -> state` and `__call__(self, state, batch) -> batch`; `ctx` has `instance` and `device`. It may supply `footprint(self, state)`, and `finish(self, state) -> batch | None`, which is called once per instance when the function's input has ended, after its last `__call__`. A batch `finish` returns goes downstream like any other; `None` gives nothing; an exception ends the run with a `FunctionError` naming the function. A function that must see the whole run before giving its result (one that collects every batch) gives it from `finish` and takes `instances=1`: with more instances, each instance's `finish` gives what that instance saw. On resume, a `resume="reinit"` function's `finish` sees only the batches after the checkpoint, so a whole-run function that must survive a resume uses `resume="checkpoint"`. With `resume="checkpoint"`, it must also implement `checkpoint(self, state) -> bytes` and `restore(self, ctx, data) -> state`. Invalid declarations or options raise `TypeError` or `ValueError` at decoration time.

## A decorated `moruna.Function`

What `moruna.function` returns is a `moruna.Function` (see [Tasks](python-run.md#tasks) for a function written as a class) with no public constructor.

| Member | Type | Meaning |
| --- | --- | --- |
| `__call__(*args, **kwargs)` | delegated call | Calls the wrapped Python object and returns its result. |
| `wrapped` | object | Original callable. |
| `stateful` | `bool` | Whether this function keeps state. |
| `memory_guard` | `bool` | Whether allocation refusal is on. |
| `fingerprint` | `str` | 64-character lowercase hexadecimal fingerprint. |
| `build(...)` | method | Packages the function as an OCI image; see below. |

## `moruna.polars`

```python
moruna.polars(fn, *, accepts="table", **declarations) -> moruna.Function
```

Wraps an unannotated Polars function taking and returning a `DataFrame` or `LazyFrame`. `accepts` has the same choices as `moruna.function`. Supported declaration names are `input_schema`, `output_schema`, and `lockfile`, with the same meanings above. Returns a stateless `moruna.Function`.

## Building functions into images

A function is built into an OCI image with the job it is a task of: `job.build()` or `moruna build`, one image per task (see [Jobs and `moruna.run`](python-run.md#building-images) and [Images and ports](images.md)). A decorated function's image runs it as the shim calls it: its `wrapped` callable for each batch, or, stateful, `setup` once and `finish` at the end.
