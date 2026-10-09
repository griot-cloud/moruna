# Kernels

## `moruna.kernel`

```python
moruna.kernel(
    fn=None, *, stateful=False, instances=1, device_memory=False,
    accepts="table", tier="host", releases_gil=None,
    expected_amplification=None, preferred_rows=None, resume="reinit",
    state_bytes=None, input_schema=None, output_schema=None,
    lockfile=None, memory_guard=True,
) -> moruna.KernelSpec | decorator
```

Use `@moruna.kernel`, `@moruna.kernel(...)`, or `moruna.kernel(obj, ...)`. When `fn` is omitted, the call returns a decorator; otherwise it returns a `KernelSpec`. A stateless function receives and returns a `pyarrow.RecordBatch`. An annotated Polars `DataFrame` or `LazyFrame` function is converted through Polars.

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

Schema mappings accept Arrow types, their string spellings, Python `int`, `float`, `str`, `bool`, and supported Polars types. A stateful object supplies `setup(self, ctx) -> state` and `__call__(self, state, batch) -> batch`; `ctx` has `instance` and `device`. It may supply `footprint(self, state)`. With `resume="checkpoint"`, it must also implement `checkpoint(self, state) -> bytes` and `restore(self, ctx, data) -> state`. Invalid declarations or options raise `TypeError` or `ValueError` at decoration time.

## `moruna.KernelSpec`

The frozen object returned by `moruna.kernel` has no public constructor.

| Member | Type | Meaning |
| --- | --- | --- |
| `__call__(*args, **kwargs)` | delegated call | Calls the wrapped Python object and returns its result. |
| `wrapped` | object | Original callable. |
| `stateful` | `bool` | Whether this kernel keeps state. |
| `memory_guard` | `bool` | Whether allocation refusal is on. |
| `fingerprint` | `str` | 64-character lowercase hexadecimal fingerprint. |

## `moruna.polars`

```python
moruna.polars(fn, *, accepts="table", **declarations) -> moruna.KernelSpec
```

Wraps an unannotated Polars function taking and returning a `DataFrame` or `LazyFrame`. `accepts` has the same choices as `moruna.kernel`. Supported declaration names are `input_schema`, `output_schema`, and `lockfile`, with the same meanings above. Returns a stateless `KernelSpec`.
