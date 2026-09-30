# Writing your own source or sink

Moruna reads Parquet and tensor files, and writes Parquet, tensor and Arrow IPC files. For anything else, write a class of your own in Python. Subclass `moruna.Source` to read from somewhere Moruna does not, and `moruna.Sink` to write somewhere it does not. `moruna.run` accepts an instance of either wherever it accepts the built-in ones:

```python
report = moruna.run(MySource(...), [kernel], MySink(...), budget="8GiB")
```

Your source and sink run inside the same budget as the rest of the job. Moruna still sizes the work, reads ahead, spills to disk and resumes, as long as your classes follow the rules below.

## A source

A source says what there is to read, then reads any part of it on request. It has two methods, and a third that is optional:

```python
class moruna.Source:
    repeatable: bool = True

    def plan(self) -> list[moruna.Split]: ...                                    # required
    def read(self, split_id: int, start: int, end: int) -> pyarrow.RecordBatch: ...  # required
    def schema(self) -> pyarrow.Schema | None: ...                               # optional

class moruna.Split:
    def __init__(self, id: int, rows: int, bytes: int | None = None): ...
```

`plan` returns the splits: the independent parts of your data, such as the pages of an API, the partitions of a table or the files of a format Moruna does not read. Each `moruna.Split` has an id, its exact number of rows and, if you know it, its approximate size in bytes. Moruna calls `plan` once, before it reads anything.

`read` returns rows `start` to `end` of one split, as a `pyarrow.RecordBatch`: `start` is included and `end` is not. Moruna chooses the ranges itself, so a split is often read in several pieces, and the same range may be read more than once.

This source serves an in-memory table in splits of 100,000 rows:

```python
import pyarrow as pa
import moruna


class TableSource(moruna.Source):
    def __init__(self, table: pa.Table, split_rows: int = 100_000):
        self.batch = table.combine_chunks().to_batches()[0]
        self.split_rows = split_rows

    def plan(self):
        rows = self.batch.num_rows
        count = -(-rows // self.split_rows)
        return [
            moruna.Split(i, min(self.split_rows, rows - i * self.split_rows))
            for i in range(count)
        ]

    def schema(self):
        return self.batch.schema

    def read(self, split_id, start, end):
        return self.batch.slice(split_id * self.split_rows + start, end - start)
```

### The rules for a source

- **Row counts are exact.** `Split.rows` must be the split's true number of rows, because Moruna sizes the work from it before reading anything. `read(split_id, start, end)` must return exactly `end - start` rows, in order. A batch with the wrong number of rows stops the job with `moruna.IoError`, which names the split and both counts.
- **Every batch has the same columns.** The column names and types must match `schema()`, or the first batch read if you do not define `schema()`. A batch that differs stops the job with `moruna.IoError`.
- **Split ids are unique.** Use whole numbers from 0 to 4,294,967,295.
- **The size is an estimate.** When you leave out `bytes`, Moruna reads up to 1,024 rows of the first split that has any, once, to measure the size of a row. Giving `bytes` and defining `schema()` avoids that read.
- **Returning a slice is fine.** Moruna copies the rows you return into its own memory, not the whole batch a slice belongs to.

### When reads can change: `repeatable`

Leave `repeatable` as `True` when reading the same range twice gives the same rows for as long as the job runs. That lets Moruna drop data it has read when memory is short and read it again later, and resume a job that was stopped.

Set `repeatable = False` on the class when that is not true, for example when the data changes while the job runs:

```python
class LiveTable(TableSource):
    repeatable = False
```

Moruna then keeps what it has read on disk instead of reading it again, and the job cannot be resumed. The run report says so: `source is not repeatable (an iterator source, or repeatable = False): no resume, Q0 staged`.

### When to use `IteratorSource` instead

Use `moruna.IteratorSource` for data that arrives once, in order, and cannot be asked for again, such as a queue, a socket or a generator. It is simpler, but an iterator cannot be read by position: Moruna cannot split its batches, read ahead or drop and re-read data, and a job over it cannot be resumed.

Use `moruna.Source` whenever you can read a part of your data by position, with an offset, a page number or a key range.

## A sink

A sink receives each result as a `pyarrow.RecordBatch`. It has one required method and three optional ones:

```python
class moruna.Sink:
    def write(self, batch: pyarrow.RecordBatch) -> None: ...   # required
    def finish(self) -> None: ...                              # optional
    def checkpoint(self) -> bytes | None: ...                  # optional
    def restore(self, state: bytes) -> None: ...               # required if you define checkpoint
```

This sink writes each batch to its own Arrow IPC file, and can be resumed:

```python
import json
import os
import pathlib

import pyarrow as pa
import pyarrow.ipc as ipc
import moruna


class IpcFiles(moruna.Sink):
    def __init__(self, directory):
        self.dir = pathlib.Path(directory)
        self.dir.mkdir(parents=True, exist_ok=True)
        self.names = []

    def write(self, batch):
        name = f"part-{len(self.names):05d}.arrow"
        tmp = self.dir / (name + ".tmp")
        with ipc.new_file(str(tmp), batch.schema) as writer:
            writer.write_batch(batch)
        os.replace(tmp, self.dir / name)
        self.names.append(name)

    def finish(self):
        (self.dir / "_SUCCESS").touch()

    def checkpoint(self):
        return json.dumps(self.names).encode()

    def restore(self, state):
        self.names = json.loads(state)
        keep = set(self.names)
        for path in self.dir.iterdir():
            if path.name not in keep:
                path.unlink()
```

### The rules for a sink

- **`write` gets Moruna's own memory.** The batch is not copied. If your sink keeps a batch after `write` returns, that memory still counts against the job's budget until you release it. Write the data out, or copy what you need to keep.
- **Order.** Batches arrive in the order they finish. Pass `ordered=True` to `moruna.run` to receive them in the order the source produced them.
- **`finish`** is called once, after the last `write`, and only when the job completes. It is not called after an error.
- **Errors.** An exception in `write`, `finish` or `checkpoint` stops the job with `moruna.IoError`, carrying your message.
- **Threads.** Moruna never calls one sink, or one source, from two threads at once, on either build of Python.

### Resuming into your sink: `checkpoint` and `restore`

A sink that does not define `checkpoint`, or whose `checkpoint` returns `None`, cannot be resumed. The job still runs, and the run report says `sink: no resume, the sink does not checkpoint`.

To take part in [resume](resume.md), define both methods:

- `checkpoint()` returns bytes that describe everything written so far. Moruna never calls it while a `write` is running, so the state it describes is complete.
- `restore(state)` is called on a new sink object when a stopped job is resumed, with the bytes from the last checkpoint. It must put the sink back into that state and undo anything written after it. In the example, that means deleting the files the list does not name.

Moruna records which batches each checkpoint covers. After `restore`, it does not send your sink any of those batches again, so every row reaches `write` exactly once across a stop and a resume. `moruna.run` refuses a sink that defines `checkpoint` without `restore`.

## Errors in your code

An exception in your source or sink stops the job, and the message includes your class, the method and the original Python error. A missing required method is reported by `moruna.run` before anything starts.

| Where | Exception |
| --- | --- |
| `plan` or `schema`, or a plan Moruna cannot use | `moruna.PlanError` |
| `read`, or a batch with the wrong rows or columns | `moruna.IoError` |
| `write`, `finish` or `checkpoint` | `moruna.IoError` |
| `restore`, or a checkpoint that cannot be used | `moruna.ResumeError` |
| A class without `plan`, `read` or `write`, or with `checkpoint` but no `restore` | `TypeError` |

Next: [Writing kernels](kernels.md).
