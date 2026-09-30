# Concepts

Moruna runs a **job**: it reads data from a **source**, passes it through one or more **kernels**, and writes the result to a **sink**, all inside a **memory budget**. You write the kernels. Moruna decides how much data each kernel receives at a time, how many run at once, and when to use disk instead of memory.

## A job

Acme keeps its product reviews in Parquet files, about 80 GB in total, and wants to add a sentiment score to every review with a Python function. The machine it has for the job has 16 GB of memory.

Loading the files into memory is not possible, and a hand-written loop needs a batch size. Too large and the process is killed for using too much memory; too small and the machine sits mostly idle. The right size also depends on how much memory the scoring function uses, which Acme does not know in advance.

With Moruna, the job is three objects and a budget:

```python
import moruna

report = moruna.run(
    moruna.ParquetSource("s3://acme-data/reviews/"),
    [score],
    moruna.ParquetSink("s3://acme-data/reviews-scored/"),
    budget="12GiB",
    storage={"region": "eu-west-1"},
)
```

`score` is Acme's function, and `storage` says where the bucket is; the credentials come from the usual AWS environment variables. Moruna reads the reviews a piece at a time, runs `score` on each piece, writes the results, and returns a report of what happened.

## Kernels

A **kernel** is a function that takes a batch of rows and returns a batch of rows. Batches are Apache Arrow record batches, so a kernel can use `pyarrow`, NumPy, Polars or any library that reads Arrow data.

```python
import pyarrow as pa
import moruna

@moruna.kernel
def score(batch):
    texts = batch.column("text").to_pylist()
    return batch.append_column("score", pa.array([model(t) for t in texts]))
```

A kernel sees only its batch. It does not open files, choose batch sizes or manage threads, which is what lets Moruna run it on any amount of data. A job can chain several kernels; each receives what the previous one returned.

Some transformations do not need your own code. Moruna includes **standard kernels** for common operations such as filtering rows, selecting and renaming columns, casting types and removing duplicates. Learn more in [Writing kernels](kernels.md).

## The memory budget

The **budget** is the most memory the whole process may use, including Python, your libraries and your kernels' own data, not only the data Moruna holds. Moruna keeps the process under it for the entire run.

You can set the budget with `budget=`. If you do not, Moruna reads the limit of the container it runs in, or uses most of the machine's memory when there is no container limit. It never asks for a batch size, a worker count or a buffer size: the budget is the only setting a job needs.

If the budget is too small for the job, for example because a single piece of data costs more than the budget allows, Moruna stops with a message that names the amounts involved rather than exceeding the limit.

## Morsels

Moruna divides the input into **morsels**, pieces of data sized to fit the budget. Before the job starts, it runs your kernel on one morsel and measures how much memory the kernel used compared with the size of its input. A kernel that builds a list of Python strings from its batch may use several times the memory of the batch itself; a kernel that adds one number per row uses very little.

Moruna uses that measurement to choose the morsel size and the number of kernels that run at once. It keeps measuring during the job and adjusts both, so a kernel whose memory use grows is given smaller morsels.

## Workers and threads

Kernels run on **worker** threads, several at once. On the free-threaded build of CPython 3.14, Python kernels run in parallel on every core. On the standard build, Python holds a lock that lets only one thread run Python code at a time, so Moruna asks you to confirm with `allow_gil=True` before running Python kernels there.

## Spilling to disk

When the data waiting between steps does not fit in memory, Moruna moves it to a **staging directory** on disk and reads it back when it is needed. This lets a job continue when, for example, the sink writes more slowly than the kernels produce results. Moruna chooses the staging directory itself and limits it to part of the free disk space unless you set one.

## Reports and resuming

Every job returns a **run report**: rows read and written, how long each stage took, how close the process came to its budget, and notes about decisions Moruna made. Learn more in [The run report](run-report.md).

While a job runs, Moruna writes a **checkpoint** every few seconds. If the process is stopped, you can run the same job again with `resume="auto"` and it continues from the last checkpoint. The output is the same as a run that was never interrupted. Learn more in [Checkpoints and resume](resume.md).

Next: [Quickstart](quickstart.md).
