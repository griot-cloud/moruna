# Python API

Everything below is available from `import moruna`. For explanations and examples, see [Running jobs from Python](python.md) and [Writing kernels](kernels.md).

## moruna.run

```python
moruna.run(source, kernels, sink, *, budget=None, cpu=None, ...) -> RunReport
```

Runs a job and returns its report when the source is exhausted. `kernels` is a kernel or a list of kernels, applied in order; an empty list copies the source to the sink. An undecorated function is accepted and treated as `@moruna.kernel` with its defaults.

| Argument | Default | Meaning |
| --- | --- | --- |
| `budget` | discovered | Memory limit for the whole process, in bytes or as a string such as `"8GiB"`. |
| `cpu` | discovered | Number of cores to use. |
| `staging_dir` | discovered | Existing directory for spilled data and checkpoints. |
| `staging_limit` | 20% of free space | Most disk the job may use in `staging_dir`. |
| `on_error` | `"terminate"` | `"terminate"`, `"skip"`, or `("budget", n)` to skip up to `n` failed morsels. |
| `ordered` | `False` | Deliver results to the sink in source order. |
| `storage` | `None` | Connection settings for `s3://`, `gs://` and `az://` URLs. |
| `allow_gil` | `False` | Run Python kernels on the standard, non-free-threaded build of Python. |
| `checkpoint` | `True` | Write checkpoints while the job runs. |
| `checkpoint_interval` | `5.0` | Seconds between checkpoints. |
| `keep_checkpoint` | `False` | Keep the checkpoint directory after the job completes. |
| `resume` | `None` | `"auto"`, a run id, or the path to a `manifest.json`. |
| `trace` | `None` | Path of a file to write the per-morsel trace to. |
| `profiles_dir` | `~/.moruna/profiles` | Where kernel profiles are stored. |
| `sizer` | `"rule"` | How morsels are sized: `"rule"` or `"learned"`. |

A value outside its accepted range is changed to the nearest accepted value, and the change is recorded in `report.notes`.

## moruna.kernel

```python
@moruna.kernel
@moruna.kernel(stateful=False, instances=1, ...)
moruna.kernel(obj, **options)
```

Turns a function, or for a stateful kernel an object, into a kernel.

| Option | Default | Meaning |
| --- | --- | --- |
| `stateful` | `False` | The object has `setup(self, ctx)` and `__call__(self, state, batch)`. |
| `instances` | `1` | Copies of a stateful kernel's state. |
| `resume` | `"reinit"` | For a stateful kernel, `"checkpoint"` saves its state with each checkpoint; requires `checkpoint` and `restore` methods. |
| `state_bytes` | `None` | Expected size of each copy of the state. |
| `expected_amplification` | `None` | Expected memory use as a multiple of the input size. |
| `preferred_rows` | `None` | Preferred number of rows per batch. |
| `releases_gil` | `None` | Whether the kernel releases Python's global lock while it works. |
| `accepts` | `"table"` | `"table"` for record batches, `"tensor"` for tensors. |
| `input_schema` | `None` | Columns the kernel needs: a `pyarrow.Schema`, or a mapping of column name to type. |
| `output_schema` | `None` | What the kernel returns: a schema, a mapping, or a mapping with `adds`, `drops` and `changes`. |
| `lockfile` | `None` | A lock file whose contents are included in the fingerprint. |

A kernel object has `fingerprint`, a string identifying its code, and `stateful`.

`moruna.polars(fn, **declarations)` wraps a function from a Polars `DataFrame` to a Polars `DataFrame` that has no type annotations.

## Sources

| Source | Arguments |
| --- | --- |
| `ParquetSource(urls, *, columns=None, filters=None)` | A path, URL, or list of them; the columns to read; row-group filters as `(column, op, value)` with op `>`, `<` or `==`. |
| `TensorSource(paths, *, tensors=None)` | Safetensors or aligned binary tensor files; the tensors to read. |
| `IteratorSource(iterable, *, schema)` | An iterable of `pyarrow.RecordBatch` objects and their schema. |
| A subclass of `Source` | Your own source; see below. |

## Sinks

| Sink | Arguments |
| --- | --- |
| `ParquetSink(url, *, row_group_bytes=None, file_bytes=None, compression="zstd")` | Target directory or prefix; about 128 MiB per row group and 1 GiB per file by default. |
| `TensorSink(path, *, format="mrb1", one_file_per_morsel=False, name="tensor")` | Target directory; `mrb1` or `safetensors`. |
| `ArrowIpcSink(path, *, file_bytes=None)` | Target directory. |
| A subclass of `Sink` | Your own sink; see below. |

## moruna.Source, moruna.Sink and moruna.Split

Base classes for a source or a sink of your own. For examples and the rules they follow, see [Writing your own source or sink](sources-and-sinks.md).

```python
class Split:
    def __init__(self, id: int, rows: int, bytes: int | None = None)

class Source:
    repeatable: bool = True
    def plan(self) -> list[Split]
    def read(self, split_id: int, start: int, end: int) -> pyarrow.RecordBatch
    def schema(self) -> pyarrow.Schema | None

class Sink:
    def write(self, batch: pyarrow.RecordBatch) -> None
    def finish(self) -> None
    def checkpoint(self) -> bytes | None
    def restore(self, state: bytes) -> None
```

| Method | Required | Meaning |
| --- | --- | --- |
| `Source.plan` | yes | The splits to read, each with its exact row count. Called once. |
| `Source.read` | yes | Exactly rows `start` to `end` (not included) of one split. |
| `Source.schema` | no | The columns every batch has. By default, the first batch's. |
| `Source.repeatable` | no | `False` when reading a range twice can give different rows; the job then cannot be resumed. |
| `Sink.write` | yes | Receives each result. The batch is Moruna's memory, counted against the budget while you keep it. |
| `Sink.finish` | no | Called once after the last write, when the job completes. |
| `Sink.checkpoint` | no | Bytes that describe everything written so far; `None` means the sink cannot be resumed. |
| `Sink.restore` | with `checkpoint` | Returns a new sink to the state of a checkpoint when a job resumes. |

`Split` is read-only; its `bytes` is an estimate and may be `None`.

## Standard kernels

`moruna.std` provides `cast`, `rename`, `select`, `drop`, `filter`, `fill_null`, `dedupe`, `hash`, `mask`, `explode`, `concat_str` and `date_trunc`. Each takes its settings as arguments and returns a kernel; see [Standard kernels](kernels.md#standard-kernels).

## moruna.inspect_host

```python
moruna.inspect_host() -> dict
```

Returns what Moruna would find on this machine without running a job: `limits` (memory, CPU and devices), `host_profile` (the system features it detected), `anon_bytes` (the memory this process holds now) and `notes`.

## Exceptions

Every exception Moruna raises is a `moruna.MorunaError`, with these attributes:

- `kind` and `message`: the kind of failure and a description.
- `diagnostic`: a dictionary of the values involved.
- `run_id` and `manifest`: the job's id and its latest checkpoint, when the job had started.
- `report`: the partial run report, when the job had started.

| Exception | Raised when |
| --- | --- |
| `ConfigError` | An argument cannot be used, or the budget leaves no room to run. |
| `PlanError` | The source, kernels and sink do not fit together, for example when the sink would overwrite the source. |
| `BudgetError` | The job cannot stay within its budget. |
| `KernelError` | A kernel raised an exception and the error policy stopped the job. |
| `IoError` | Data could not be read or written. |
| `ResumeError` | A checkpoint cannot be resumed. |
| `Cancelled` | The job was interrupted, for example with Ctrl-C. |
