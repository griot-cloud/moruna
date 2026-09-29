<p align="center">
  <img src="docs/assets/moruna-light.png" alt="Moruna" width="360">
</p>

Moruna is a fast, lightweight runtime that runs your Python functions over datasets larger than memory, inside a memory limit you set, and sizes the work itself so the job neither runs out of memory nor leaves the machine idle.

## Use cases

**Run a model over a large dataset.** Apply a tokenizer, a scoring model or any Python function to every row of a Parquet dataset that does not fit in memory, and write the results as Parquet.

**Stay inside a memory limit.** Give a job a budget, or let Moruna read its container's limit. Moruna measures what your function uses and adjusts batch sizes and workers to stay under it.

**Resume a job that was stopped.** Restart a long job from its last checkpoint instead of from the beginning. The output is the same as a run that was never interrupted.

**Run a job described in a file.** Describe the input, functions, output and budget in a job document, then run it with the `moruna` command from a scheduler, a container or a virtual machine.

## Quickstart

```bash
pip install moruna
```

```python
import moruna
import pyarrow as pa

@moruna.kernel
def shout(batch):
    loud = pa.array([s.as_py().upper() for s in batch.column("text")])
    return batch.append_column("loud", loud)

report = moruna.run(
    moruna.ParquetSource("events/"),
    [shout],
    moruna.ParquetSink("events-loud/"),
    budget="8GiB",
)
print(report)
```

Moruna requires Python 3.14. On the standard build of Python, add `allow_gil=True`; on the free-threaded build, Python kernels run on every core. The [quickstart](https://griot-cloud.github.io/moruna/quickstart.html) builds a dataset and runs this end to end.

## Documentation

| | |
| --- | --- |
| **[Getting started](https://griot-cloud.github.io/moruna/getting-started.html)**<br>Learn the concepts, then run your first job. | **[Using Moruna](https://griot-cloud.github.io/moruna/using.html)**<br>Write kernels, run jobs from Python or the command line, and choose where they run. |
| **[How it works](https://griot-cloud.github.io/moruna/execution.html)**<br>Understand how Moruna sizes work, keeps memory under the limit and resumes. | **[Reference](https://griot-cloud.github.io/moruna/reference.html)**<br>Look up functions, the job document, commands and the run report. |

Read the [documentation website](https://griot-cloud.github.io/moruna/).

---

[Contributing](CONTRIBUTING.md) · [Changelog](CHANGELOG.md) · [Apache-2.0 license](LICENSE)
