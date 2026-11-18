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

Use `@moruna.function`, `@moruna.function(...)`, or `moruna.function(obj, ...)`. When `fn` is omitted, the call returns a decorator; otherwise it returns a `moruna.Function`, which a job takes as a task. A stateless function receives a `pyarrow.RecordBatch` and returns Arrow.

| Parameter | Type and default | Meaning |
| --- | --- | --- |
| `fn` | callable or object, optional | Function to wrap, or stateful object when `stateful=True`. |
| `stateful` | `bool = False` | Call `setup(ctx)` and `__call__(state, batch)` on the object. |
| `instances` | `int = 1` | With `stateful=True`, the most instances a job runs, each with its own state; at least 1. |
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

Schema mappings accept Arrow types, their string spellings, and Python `int`, `float`, `str` and `bool`. A stateful object supplies `setup(self, ctx) -> state` and `__call__(self, state, batch) -> batch`; `ctx` has `instance` and `device`. It may supply `footprint(self, state)`, and `finish(self, state) -> batch | None`, which is called once per instance when the function's input has ended, after its last `__call__`. A batch `finish` returns goes downstream like any other; `None` gives nothing; an exception ends the run with a `FunctionError` naming the function. A function that must see the whole run before giving its result (one that collects every batch) gives it from `finish` and takes `instances=1`: with more instances, each instance's `finish` gives what that instance saw. On resume, a `resume="reinit"` function's `finish` sees only the batches after the checkpoint, so a whole-run function that must survive a resume uses `resume="checkpoint"`. With `resume="checkpoint"`, it must also implement `checkpoint(self, state) -> bytes` and `restore(self, ctx, data) -> state`. Invalid declarations or options raise `TypeError` or `ValueError` at decoration time.

### How many instances run

A job runs a function one instance per CPU of the job (its `budget.cpu`, or the node's CPUs), each a process of its own, and gives each a share of the batches. A plain `@moruna.function` declares nothing and runs on every CPU. A stateful one (`stateful=True`) runs at most its `instances`, 1 unless it says more, which its image declares as `MORUNA_PROCESSES` when it is built. A `moruna.Function` subclass runs one instance, as its contract says. The declaration is a ceiling, never a count: a job of fewer CPUs runs fewer.

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

## `moruna.params`

```python
moruna.params() -> dict
```

The task's params: the JSON object a job gives the function's task (`{"kind": "function", "function": "taxi.score", "params": {"threshold": 0.5}}`), which its process reads as `MORUNA_PARAMS` (see [Images and ports](images.md#the-contract)). It is `{}` when the task was given none, or when the code runs outside a job, and a value that is not a JSON object raises `ValueError` naming the variable. A function reads its params the same way in each SDK, native or image: `moruna_function::params()` in Rust and `moruna_params()` in C. Inside an image Moruna builds, `moruna.params()` is the stand-in module's, and reads the same. A decorated function given params reads them there; nothing refuses them. A class task is also constructed with its params, as before, and can read them through `moruna.params()` too.

## Building functions into images

A function is built into an OCI image with the job it is a task of: `job.build()` or `moruna build`, one image per task (see [Jobs and `moruna.run`](python-run.md#building-images) and [Images and ports](images.md)). A decorated function's image runs it as the shim calls it: its `wrapped` callable for each batch, or, stateful, `setup` once and `finish` at the end.
