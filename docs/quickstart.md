# Quickstart

In this example, you will create a Parquet dataset of 500,000 product reviews, write a kernel that gives each review a score, and run it inside a 1 GiB memory budget. Moruna reads the reviews in pieces, runs your kernel on each, and writes the scored reviews to a new dataset.

You need Python 3.14 on Linux (x86-64) or macOS (Apple silicon). The example takes about a minute.

## 1. Install Moruna

```bash
pip install moruna
```

This also installs `pyarrow`, which Moruna uses to pass data to your kernels. Check that it works:

```bash
python -c "import moruna; print(moruna.__version__)"
```

Moruna runs Python kernels in parallel on the free-threaded build of Python 3.14, often installed as `python3.14t`. On the standard build, Python runs one thread at a time, and Moruna asks you to confirm by passing `allow_gil=True` to `moruna.run`. The example works on either build.

## 2. Create some data

Create an empty directory and move into it. Save the following as `make_data.py`:

```python
import random
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

random.seed(1)
Path("reviews").mkdir(exist_ok=True)
words = ["great", "slow", "broken", "fine", "love", "refund", "late", "perfect"]
rows = 500_000
table = pa.table({
    "review_id": pa.array(range(rows), pa.int64()),
    "text": pa.array([" ".join(random.choices(words, k=12)) for _ in range(rows)]),
})
pq.write_table(table, "reviews/part-0.parquet", row_group_size=50_000)
print(f"wrote {rows:,} reviews")
```

Run it:

```text
python make_data.py
wrote 500,000 reviews
```

## 3. Write a kernel and run it

A kernel receives a batch of rows as a `pyarrow.RecordBatch` and returns a batch. This one counts the positive words in each review and adds the result as a new `score` column. Save the following as `score.py`:

```python
import pyarrow as pa
import moruna

POSITIVE = {"great", "love", "perfect", "fine"}

@moruna.kernel
def score(batch):
    texts = batch.column("text").to_pylist()
    scores = [sum(w in POSITIVE for w in t.split()) / 12 for t in texts]
    return batch.append_column("score", pa.array(scores, pa.float64()))

report = moruna.run(
    moruna.ParquetSource("reviews/"),
    [score],
    moruna.ParquetSink("reviews-scored/"),
    budget="1GiB",
)
print(report)
```

If you are using the standard build of Python, add `allow_gil=True` after `budget="1GiB",`. Then run the job:

```text
python score.py
```

## 4. Read the report

`moruna.run` returns when the job is finished, and printing the report gives a summary:

```text
moruna run f9eba5ceb9c08a064c0b3465aa36549f completed
wall 0.84s   peak 528.2 MiB (52% of ceiling 1.0 GiB)   workers busy 53%
...
stage    morsels       rows in      rows out    rows/s  errors  skipped
    1         20        500000        500000   6.921e5       0        0
```

The job completed in under a second. At its highest, the whole Python process used 52% of its 1 GiB budget. Moruna divided the data into 20 morsels and chose their size by measuring how much memory `score` used on the first one. Your figures will differ with your machine.

The same values are available as attributes, for example `report.peak_fraction_of_ceiling` and `report.stages[0]["rows_out"]`. See [The run report](run-report.md) for every field.

## 5. Check the output

The scored reviews are Parquet files in `reviews-scored/`:

```python
import pyarrow.parquet as pq

table = pq.read_table("reviews-scored/")
print(table.num_rows, table.column_names)
```

```text
500000 ['review_id', 'text', 'score']
```

To run this job over your own data, change the source and sink paths. Moruna also reads and writes `s3://`, `gs://` and `az://` URLs; see [Object storage](python.md#object-storage) for the `storage` argument they need. The budget can stay the same whatever the size of the input: a larger dataset takes longer, not more memory.

Next: [Using Moruna](using.md).
