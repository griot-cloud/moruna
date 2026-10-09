# Sinks

These constructors return frozen handles for `moruna.run(sink=...)`. A bare local `url` for Parquet or Vortex is resolved as a local file URL.

## `moruna.ParquetSink`

```python
moruna.ParquetSink(url, *, row_group_bytes=None, file_bytes=None, compression="zstd")
```

`url: str` is the output directory or prefix. `row_group_bytes: int | str | None` defaults to 128 MiB; `file_bytes: int | str | None` defaults to 1 GiB. `compression: str` accepts `"zstd"`, `"snappy"`, `"gzip"`, `"lz4"`, `"none"`, or `"uncompressed"` (case-insensitive).

## `moruna.VortexSink`

```python
moruna.VortexSink(url, *, file_bytes=None)
```

`url: str` is an output directory or prefix. `file_bytes: int | str | None` defaults to 1 GiB.

## `moruna.TensorSink`

```python
moruna.TensorSink(path, *, format="mrb1", one_file_per_morsel=False, name="tensor")
```

`path: str` is the output directory. `format: str` accepts `"mrb1"` or `"safetensors"` (case-insensitive). `one_file_per_morsel: bool` selects whether each morsel gets its own file. `name: str` is the output tensor name.

## `moruna.ArrowIpcSink`

```python
moruna.ArrowIpcSink(path, *, file_bytes=None)
```

`path: str` is the output directory. `file_bytes: int | str | None` defaults to 1 GiB. Size settings may be clamped to supported bounds, with a note in the run report. Unknown formats and compression names raise `ValueError` at construction.

## `moruna.Sink`

```python
class MySink(moruna.Sink):
    def write(self, batch: pyarrow.RecordBatch) -> None: ...
    def finish(self) -> None: ...
    def checkpoint(self) -> bytes | None: ...
    def restore(self, state: bytes) -> None: ...
```

| Method | Required | Contract |
| --- | --- | --- |
| `write(self, batch)` | Yes | Receive each output batch, counted against Moruna's memory budget while retained. |
| `finish(self)` | No | Called once on successful completion; base implementation does nothing. |
| `checkpoint(self)` | No | Return bytes describing all output written; base returns `None`, disabling sink resume. |
| `restore(self, state)` | If checkpointing | Restore from checkpoint bytes; base raises `TypeError`. |
