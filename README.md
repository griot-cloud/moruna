# Amoru

Amoru runs a Python function over a dataset bigger than your memory, without you choosing a batch size.

You have a few hundred gigabytes in Parquet and a function that a SQL engine cannot express: a model, a tokenizer, a rule set. The usual answer is a hand written loop with a guessed batch size and a guessed worker count, which either gets killed by the out of memory killer or runs at a fraction of the machine you are paying for. Amoru replaces that loop. It reads the limits of the process it is in, looks ahead at what the source is about to deliver, measures how much your function expands its input, and adjusts batch size, worker count, read ahead and spill continuously, so the pass finishes inside the budget.

## Install

```bash
pip install amoru
```

Requires CPython 3.14, standard or free threaded. The free threaded build is the one to use if your function releases the GIL, because Amoru then runs it on every core.

## Your first job

```python
import amoru, pyarrow as pa

@amoru.kernel
def shout(batch):                       # a pyarrow.RecordBatch in, one out
    loud = pa.array([s.as_py().upper() for s in batch.column("text")])
    return batch.append_column("loud", loud)

report = amoru.run(
    amoru.ParquetSource("s3://bucket/events/"),
    [shout],
    amoru.ParquetSink("s3://bucket/events-loud/"),
    budget="8GiB",
)
print(report)
```

There is no batch size, no worker count, no read ahead depth and no spill threshold, because Amoru decides all four and keeps deciding them as the data changes. Inside a container or a pod you can drop `budget=` too: Amoru reads the cgroup limit itself.

If the budget is too small for what your function costs, the run does not quietly exceed it and does not get killed: it stops with a diagnostic naming the morsel, its measured footprint and the budget. A Python kernel that builds new arrays costs several times its input in memory the runtime cannot place in its own arena, so give a Python job a couple of gigabytes to work in; a Rust kernel runs in a fraction of that.

## What you get back

`report` carries what the run actually did, computed from a per morsel trace rather than from estimates:

```python
report.peak_fraction_of_ceiling   # how close the run came to its budget
report.worker_busy_fraction       # how much of the CPU quota was used
report.rows_out, report.wall_s
report.stages[0].amplification_p95  # how much your function expanded its input
report.to_json()
```

## Sources and sinks

```python
amoru.ParquetSource(urls, columns=None, filters=None)   # local paths, s3://, gs://, az://
amoru.TensorSource(paths, tensors=None)                 # safetensors, .npy, aligned binary
amoru.IteratorSource(iterable, schema=...)              # anything you can yield

amoru.ParquetSink(url, row_group_bytes=None, file_bytes=None, compression="zstd")
amoru.TensorSink(path, format="amb1")
amoru.ArrowIpcSink(path)
```

## Kernels

A kernel is a function from a batch to a batch. Stateless is the default:

```python
@amoru.kernel
def clean(batch): ...
```

A kernel that loads something expensive declares itself stateful, and Amoru gives each worker its own instance and keeps that instance on that worker:

```python
@amoru.kernel(stateful=True, instances=4, state_bytes=2 << 30)
class Score:
    def setup(self, ctx):               # once per instance
        return load_model(device=ctx.device)
    def __call__(self, model, batch):   # once per morsel
        return batch.append_column("score", model.predict(batch))
```

Tensors work the same way. A numeric column becomes a tensor by pointer, not by copy, and a tensor comes back as a column the same way, so a Torch or NumPy kernel costs nothing at the boundary.

## When a run does not fit

Amoru spills to local disk on purpose rather than as an emergency, and moves bytes between memory and disk without passing them through the CPU. If the budget is genuinely too small for one row, the run stops with a diagnostic that names the morsel, its size and the budget. It does not get killed by a signal, and it does not silently thrash.

Long runs can be resumed. Pass `checkpoint=True` (the default when a staging directory exists) and a killed run picks up from its own manifest:

```python
amoru.run(source, kernels, sink, resume="auto")
```

## Running your kernel somewhere else

A kernel written for Amoru is a plain function of a batch, so the same code runs as a Polars expression or a DataFusion function through thin wrappers, with no Amoru in the process. You are not buying a runtime lock in.

## Status

Alpha, and honest about it. The runtime is complete and runs end to end; the published performance claims are not yet measured, the reference host figures are not yet recorded, and the wheel is not yet on PyPI. See [BOARD.md](BOARD.md) for exactly what is done and what is not.

## How it works

If you want the design rather than the usage, [`architecture/`](architecture/) is the whole of it: [the architecture document](architecture/amoru-runtime-design.md) for the system, and [`architecture/sdd/`](architecture/sdd/) for each component in enough detail to rebuild it. Amoru was designed before it was written, and the documents are still the specification.

## Licence

Apache 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
