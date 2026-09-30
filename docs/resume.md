# Checkpoints and resume

While a job runs, Moruna regularly records how far it has got. If the process is stopped, whether by Ctrl-C, a crash, the out-of-memory killer or a machine being shut down, you can run the same job again and it continues from the last checkpoint. The output is the same as a job that was never interrupted.

## Checkpoints

A **checkpoint** is a small file, `manifest.json`, that records which output the sink has already committed, where the source was reading, and which data was spilled to disk. Moruna writes it to a directory for the job, `moruna-<run id>`, inside the staging directory, every 5 seconds by default. It also writes one when a job stops with an error or is cancelled.

Writing a checkpoint does not copy data. Morsels that were in memory when the process stopped are read from the source again on resume, which is why the source must be able to read the same data twice: Parquet, Vortex and tensor files can, an `IteratorSource` cannot.

`checkpoint_interval` changes how often checkpoints are written, in seconds. `checkpoint=False` turns them off. Moruna deletes a job's checkpoint directory when the job completes, unless you pass `keep_checkpoint=True`.

## Resuming a job

To resume, a job needs a staging directory that it can find again. Create one and pass it to every run of the job:

```python
from pathlib import Path

Path("staging").mkdir(exist_ok=True)

report = moruna.run(
    moruna.ParquetSource("reviews/"),
    [score],
    moruna.ParquetSink("reviews-scored/"),
    staging_dir="staging",
    resume="auto",
)
print(report.resumed)
```

With `resume="auto"`, Moruna looks in the staging directory for the newest checkpoint of the same job and continues from it; if there is none, it starts from the beginning. `report.resumed` says which happened. You can also name a checkpoint directly, with the run's 32-character id or the path to its `manifest.json`. The id is in the report, and in `error.run_id` when a job stops with an error.

The staging directory must already exist. If it does not, Moruna runs without a disk to spill to or checkpoint in, and adds a note to the report.

In a job document, the same settings are `"staging": {"dir": "staging"}` and `"resume": "auto"`.

## What must stay the same

A checkpoint belongs to one job: the same source, kernels, sink and options. With `resume="auto"`, Moruna ignores checkpoints that belong to a different job and starts from the beginning. If you name a checkpoint that belongs to a different job, it refuses with a `ResumeError` that says what differs, for example `kernel fingerprints differ`.

Moruna identifies a kernel by its code. A kernel whose behaviour depends on something outside its code, such as a file it reads or a command-line argument, looks unchanged to Moruna when that input changes, so start such a job again without `resume` after changing its inputs.

## Kernels with state

A stateful kernel builds its state again with `setup` when a job resumes, which suits a kernel whose state is a loaded model. A kernel whose state depends on the rows it has seen, such as a running count, would lose that state. Such a kernel should declare `resume="checkpoint"` and implement `checkpoint` and `restore`; Moruna then saves its state with every checkpoint and restores it on resume. See [Kernels that keep state](kernels.md#kernels-that-keep-state).

## Resuming on another machine

If the machine running a job may be replaced, as with spot instances or a scheduler that moves work between hosts, put the staging directory on a disk that survives the machine and mark it durable in the job document:

```json
"staging": {"dir": "/staging", "durable": true}
```

The next machine attaches the same disk and runs the job with `"resume": "auto"`. A host that expects to stop a job can also ask for a checkpoint immediately before stopping it; see [Letting a host control the job](command-line.md#letting-a-host-control-the-job).

Next: [Reference](reference.md).
