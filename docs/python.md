# Running jobs from Python

A job in Python is one call to `moruna.run`. It takes a source, a list of kernels and a sink, runs until the source is exhausted, and returns a report:

```python
report = moruna.run(source, [kernel_a, kernel_b], sink, budget="8GiB")
```

Every keyword argument is optional. This page describes the sources, the sinks and the options you are most likely to set. The full list of arguments is in the [Python API reference](python-api.md).

## Sources

A source reads the input in pieces that Moruna can size. There are four built in, and you can [write your own](sources-and-sinks.md).

**Parquet.** `moruna.ParquetSource` reads one or more Parquet files, or every file under a directory or prefix:

```python
source = moruna.ParquetSource(
    "reviews/",
    columns=["review_id", "text", "created"],
    filters=[("created", ">", 1_700_000_000)],
)
```

`columns` limits the read to the columns you name. Columns you leave out are never decoded and never use memory, so naming them is the simplest way to make a wide dataset cheaper to process.

`filters` skips whole row groups that cannot contain a match. Each filter is a column, an operator (`>`, `<` or `==`) and a value, checked against the statistics Parquet stores for each row group. Rows that do not match can still arrive in groups that were not skipped, so apply the exact condition in a kernel as well. Filters help most when the data is sorted or clustered on the filtered column.

**Vortex.** `moruna.VortexSource` reads one or more [Vortex](https://github.com/vortex-data/vortex) files, or every `.vortex` file under a directory or prefix:

```python
source = moruna.VortexSource("s3://acme-data/events/", columns=["event_id", "text"])
```

`columns` works as it does for Parquet. The columns come back in the order the file stores them, and text columns come back as ordinary `pyarrow.string()` columns.

Moruna plans the work from the statistics a Vortex file keeps for every 8,192 rows, so it knows each piece's row count and null count before it reads anything. `split_bytes` sets how large each piece is, about 128 MiB by default.

When Vortex has stored a column uncompressed, because no encoding made it smaller, that column reaches your kernel without being copied: Moruna reads it from disk into its own memory and hands the kernel those same bytes. A compressed column is decompressed and then copied into Moruna's memory once, as a Parquet column is. Either way the memory counts against the budget.

**Tensor files.** `moruna.TensorSource` reads safetensors files, or files in Moruna's aligned binary format, and passes each tensor to kernels that declare `accepts="tensor"`. Name the files, not a directory:

```python
source = moruna.TensorSource(["embeddings/part-0.safetensors", "embeddings/part-1.safetensors"], tensors=["vectors"])
```

**Your own iterator.** `moruna.IteratorSource` reads from any Python iterable that yields `pyarrow.RecordBatch` objects, for data that is not in files Moruna can read:

```python
source = moruna.IteratorSource(fetch_batches(), schema=my_schema)
```

`schema` is the `pyarrow.Schema` every batch has. Each batch the iterator yields becomes one morsel, so yield batches of a sensible size. An iterator can be read only once, so a job with an iterator source cannot be resumed after it is stopped.

**Your own source.** When you can read part of your data by position, with an offset, a page number or a key range, subclass `moruna.Source` instead. Moruna can then split the work, read ahead and resume. See [Writing your own source or sink](sources-and-sinks.md).

## Sinks

A sink writes what the last kernel returns.

| Sink | Writes | Options |
| --- | --- | --- |
| `moruna.ParquetSink(url)` | Parquet files under a directory or prefix | `row_group_bytes` (default 128 MiB), `file_bytes` (default 1 GiB), `compression` (`zstd`, `snappy`, `gzip`, `lz4` or `none`; default `zstd`) |
| `moruna.VortexSink(url)` | Vortex files under a directory or prefix | `file_bytes` (default 1 GiB) |
| `moruna.TensorSink(path)` | Tensor files | `format` (`mrb1`, the default, or `safetensors`), `one_file_per_morsel`, `name` |
| `moruna.ArrowIpcSink(path)` | Arrow IPC files | `file_bytes` |

`moruna.VortexSink` writes files named `part-00000.vortex`, `part-00001.vortex` and so on, then an empty `_SUCCESS` file when the job completes. A file is closed when the data written into it reaches `file_bytes`, so no file is larger than that, and a file of data that compresses well is smaller. A job writing to a `VortexSink` can be resumed like one writing to a `ParquetSink`: every row is written exactly once.

To write anywhere else, subclass `moruna.Sink`; see [Writing your own source or sink](sources-and-sinks.md).

Moruna refuses to start a job whose sink would write into the files its source reads.

## The budget

`budget` is the most memory the whole process may use. Give it as a number of bytes or as a string such as `"512MiB"` or `"8GiB"`:

```python
moruna.run(source, kernels, sink, budget="8GiB")
```

Without `budget`, Moruna uses the memory limit of the container it runs in, or most of the machine's memory if there is none. `cpu` works the same way for the number of cores, for example `cpu=4`.

The budget covers everything the process holds, including memory your program was already using before the job started. You can see that amount with `moruna.inspect_host()["anon_bytes"]`. Libraries that keep freed memory for reuse count too: `pyarrow`, for example, can hold on to memory after building a large table. If you create data in the same process just before a job, call `pyarrow.default_memory_pool().release_unused()` first so that memory is returned.

## Object storage

Sources and sinks accept `s3://`, `gs://` and `az://` URLs as well as local paths. Pass the connection details with `storage`:

```python
moruna.run(
    moruna.ParquetSource("s3://acme-data/reviews/"),
    [score],
    moruna.ParquetSink("s3://acme-data/reviews-scored/"),
    storage={"region": "eu-west-1"},
)
```

Settings you do not give are read from the provider's usual environment variables, such as `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`. For S3, `storage` must name at least one setting. The accepted keys are `region`, `endpoint`, `access_key_id`, `secret_access_key`, `session_token` and `bucket` for S3 and S3-compatible stores; `service_account_path` and `service_account_json` for Google Cloud Storage; `account` and `container` for Azure; and `allow_http` for endpoints without TLS.

## When a kernel fails

By default, an exception in a kernel stops the job with `moruna.KernelError`. `on_error` changes that:

- `on_error="skip"` skips the morsel whose kernel failed and continues. The report counts skipped morsels for each stage.
- `on_error=("budget", 10)` skips up to 10 failed morsels, then stops.

Every Moruna exception is a `moruna.MorunaError`. When a job had started, the exception carries the partial report in `error.report`, and `error.manifest` names the checkpoint you can resume from. The other exceptions are `BudgetError`, when the job cannot fit its budget; `ConfigError`, for an argument Moruna cannot use; `PlanError`, when the source, kernels and sink do not fit together; `IoError`; `ResumeError`; and `Cancelled`, when you press Ctrl-C.

## Order of results

Moruna runs morsels in parallel, so by default results reach the sink in the order they finish. With `ordered=True`, the sink receives them in the order the source produced them, at some cost in memory and throughput.

Next: [Writing your own source or sink](sources-and-sinks.md).
