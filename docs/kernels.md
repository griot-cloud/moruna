# Writing kernels

A kernel is the part of a job you write: a function that receives a batch of rows and returns a batch of rows. Moruna calls it once for every morsel, on several threads at once, and never tells it how large the dataset is. Anything a kernel can do to one batch, Moruna can do to a dataset of any size.

## A Python kernel

Decorate a function that takes a `pyarrow.RecordBatch` and returns one:

```python
import pyarrow as pa
import pyarrow.compute as pc
import moruna

@moruna.kernel
def loud(batch):
    return batch.append_column("loud", pc.utf8_upper(batch.column("text")))
```

A kernel may add, remove or change columns, and may return fewer rows than it received.

`moruna.run` also accepts an undecorated function and wraps it with the default settings. Decorating it lets you give Moruna information it would otherwise measure. For example, `expected_amplification=8` says the kernel uses about eight times the memory of its input, and `preferred_rows=2048` asks for batches of about that many rows, which suits a model that works best at a fixed batch size. Moruna still measures the kernel and adjusts; these values are where it starts.

## Checking a kernel

A kernel that declares its schemas can be checked before it runs on real data. `input_schema` names the columns and types the kernel needs; `output_schema` describes what it returns, either in full or as the columns it `adds`, `drops` or `changes`:

```python
import pyarrow as pa
import moruna

POSITIVE = {"great", "love", "perfect", "fine"}

@moruna.kernel(
    input_schema={"text": pa.string()},
    output_schema={"adds": {"score": pa.float64()}},
)
def score(batch):
    texts = batch.column("text").to_pylist()
    scores = [sum(w in POSITIVE for w in (t or "").split()) / 12 for t in texts]
    return batch.append_column("score", pa.array(scores, pa.float64()))
```

Save it as `kernels.py` and run `moruna check`:

```text
moruna check kernels.py
score (python): agreed
  fingerprint sha256:cadf1a2ed66f5962ab6043268a9f5ddfad6430d3170ed053d8bfa4685dd6b9a4
  profile: amplification p50 0.000 p95 0.000, state 0 bytes, 282.3 ns per row, gil free_threaded
```

`moruna check` builds batches from the input schema, including an empty batch, a single row, all-null columns and edge values such as empty strings and the largest integers, and runs the kernel on each. It then compares what the kernel returned with the declared output. A kernel that disagrees is refused, with the column and the types named:

```text
score (python): refused
  batch one_row: column `score`: declared int64, produced double
```

The command exits with 0 when every kernel agrees and 2 when any is refused, so you can run it in continuous integration. It also records a first measurement of the kernel's memory use and speed, which the kernel's first real job uses as its starting point. The **fingerprint** identifies this exact kernel; a job document can require it, so that a changed kernel is not run by mistake. Pass `--json` for a machine-readable report and `--kernel NAME` to check one kernel in a module.

## Polars kernels

A function annotated as taking and returning a Polars `DataFrame` is a Polars kernel. The batch reaches it as a Polars frame without being copied, and the frame it returns goes back the same way:

```python
import polars as pl
import moruna

@moruna.kernel
def shout(df: pl.DataFrame) -> pl.DataFrame:
    return df.with_columns(pl.col("text").str.to_uppercase().alias("loud"))
```

`pl.LazyFrame` works the same way. For a function without annotations, wrap it with `moruna.polars(fn)`.

## Kernels that keep state

Some kernels need something expensive before their first batch, such as a model loaded from disk. A **stateful** kernel is an object with a `setup` method, which builds the state once, and a `__call__` method, which receives that state with every batch:

```python
import pyarrow as pa
import moruna

class Scorer:
    def setup(self, ctx):
        return load_model("model.bin")

    def __call__(self, model, batch):
        scores = model.predict(batch.column("text").to_pylist())
        return batch.append_column("score", pa.array(scores, pa.float64()))

score = moruna.kernel(Scorer(), stateful=True, instances=4)
```

`instances` is the number of copies of the state Moruna may create; each worker uses one at a time, so a stateful kernel never receives two batches at once for the same state. `ctx.instance` tells `setup` which copy it is building. If the state is large, pass its size in bytes as `state_bytes` so Moruna can plan for it before `setup` runs.

When a stateful job is resumed after being stopped, Moruna calls `setup` again by default. A kernel whose state depends on the rows it has already seen, such as a running total, should instead pass `resume="checkpoint"` and define `checkpoint(self, state) -> bytes` and `restore(self, ctx, data) -> state`. Moruna saves the state with each checkpoint and restores it on resume.

## Standard kernels

Common transformations are available as standard kernels, which you configure with arguments instead of writing code. They are written in Rust, never take Python's global lock, and adjacent ones are combined into a single step where possible.

```python
kernels = [
    moruna.std.filter(expr="review_id < 1000"),
    moruna.std.select(columns=["review_id", "text"]),
    loud,
]
```

| Kernel | Does |
| --- | --- |
| `select(columns)`, `drop(columns)` | Keep or remove columns |
| `rename(columns)` | Rename columns, given a mapping from old name to new |
| `cast(columns)` | Convert columns to other types |
| `filter(expr)` | Keep rows where an expression is true; `==`, `!=`, `<`, `<=`, `>`, `>=`, `is_null(c)`, `and`, `or`, `not` |
| `fill_null(values)` | Replace nulls with a value for each column |
| `dedupe(keys)` | Keep the first row for each key, across the whole job |
| `hash(columns, algo, output)` | Add a SHA-256 or BLAKE3 digest of columns |
| `mask(columns, mode, keep)` | Hide string values: `redact`, `partial` (keeping the last `keep` characters), `hash` or `null` |
| `explode(column)` | Produce one row for each element of a list column |
| `concat_str(columns, separator, output)` | Join columns into one text column |
| `date_trunc(column, unit, output)` | Truncate timestamps to a year, month, day, hour, minute or second |

Every standard kernel declares its schemas, so `moruna check` checks them too.

Next: [Running jobs from the command line](command-line.md).
