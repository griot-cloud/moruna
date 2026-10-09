# Sources

These constructors return frozen handles for `moruna.run(source=...)`. They have no public data methods.

## `moruna.ParquetSource`

```python
moruna.ParquetSource(urls, *, columns=None, filters=None)
```

| Parameter | Type and default | Meaning |
| --- | --- | --- |
| `urls` | `str \| sequence[str]`, required | Parquet path, prefix, URL, or a nonempty sequence. |
| `columns` | `list[str] \| None = None` | Columns to read. |
| `filters` | `list[tuple[str, str, scalar]] \| None = None` | Row-group predicates `(column, op, value)`. Operators: `>`, `<`, `==` (also `gt`, `lt`, `=`, `eq`). Scalars: Boolean, integer, float, or string. |

## `moruna.VortexSource`

```python
moruna.VortexSource(urls, *, columns=None, split_bytes=None)
```

`urls` is a path, prefix, URL, or nonempty sequence of them. `columns: list[str] | None` projects columns. `split_bytes: int | None` sets target split size; it must be positive and defaults to about 128 MiB.

## `moruna.TensorSource`

```python
moruna.TensorSource(paths, *, tensors=None)
```

`paths` is one safetensors/aligned-binary path or a nonempty sequence. `tensors: list[str] | None` selects tensor names; `None` reads all.

## `moruna.IteratorSource`

```python
moruna.IteratorSource(iterable, *, schema)
```

`iterable` yields `pyarrow.RecordBatch` objects and is converted to an iterator at construction. Required `schema` is a `pyarrow.Schema` for tables or `(dtype: str, shape: sequence[int])` for tensors. An iterator source cannot be resumed or read by row position; use a [custom source](#morunasource) for that.

## `moruna.Split`

```python
moruna.Split(id: int, rows: int, bytes: int | None = None)
```

Frozen descriptor returned by `Source.plan()`. Read-only `id` is a unique unsigned 32-bit integer; `rows` is the exact unsigned 64-bit count; `bytes` is an optional estimated size in Arrow form. Invalid or negative values raise `ValueError`.

## `moruna.Source`

```python
class MySource(moruna.Source):
    repeatable = True
    def plan(self) -> list[moruna.Split]: ...
    def read(self, split_id: int, start: int, end: int) -> pyarrow.RecordBatch: ...
    def schema(self) -> pyarrow.Schema | None: ...
```

| Member | Required | Contract |
| --- | --- | --- |
| `plan(self)` | Yes | Called once. Return splits with unique ids and exact row counts. |
| `read(self, split_id, start, end)` | Yes | Return exactly `end - start` rows from the half-open range `[start, end)`. A range may be read again. |
| `schema(self)` | No | Base returns `None`; Moruna infers the schema from the first batch. Override when the plan can be empty. |
| `repeatable` | No | Class attribute, default `True`. Set `False` when repeating a read can return different rows; such a run cannot resume. |
