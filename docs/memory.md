# Memory and the budget

The budget is a limit on the memory of the whole process, measured the way the operating system measures it. Moruna keeps the process under it for the entire job, or stops the job with a message that says why it cannot. This page explains what counts against the budget, how Moruna divides it, and what to do when a job does not fit.

## What counts

Everything the process holds counts: the Python interpreter, the libraries it has imported, data your program created before the job, the data Moruna holds in its queues, and the memory your kernels allocate while they run. On Linux, Moruna measures the process as the container or the kernel accounts for it; on macOS, it uses the same measure the system uses to enforce its own limits.

The report's `peak_anon_bytes` is the highest value it saw during the job, and `peak_fraction_of_ceiling` is that value as a fraction of the budget. A value close to 1 means the job used most of its budget; that is normal for a job that spilled to disk.

## How the budget is divided

Suppose a job has a 1 GiB budget and the process already holds 100 MB when it starts. Moruna divides the budget into four parts:

- **What the process already holds.** The 100 MB is unavailable to the job. Memory a library holds for reuse counts here too, so release it before starting a job if you can.
- **A reserve.** Moruna keeps a tenth of the budget free as a margin.
- **Moruna's own region.** Queues and morsels in flight live in a region Moruna sets aside when the job starts. It is smaller when your kernels are expected to use a lot of memory of their own.
- **Room for your kernels.** The rest is for the memory kernels allocate themselves, such as Python objects built from a batch. Moruna plans morsels to use part of it and keeps the remainder for memory that no plan can predict, such as the buffers a Parquet writer or an Arrow conversion uses.

Because the process's starting memory comes off the top, the same budget leaves less room in a process that has already loaded large libraries or data. `moruna.inspect_host()["anon_bytes"]` shows what your process holds before the job starts.

## When a job does not fit

Moruna refuses a job it can tell in advance will not fit. If the process already holds almost the whole budget, it stops before doing anything, with a `ConfigError` naming what the process held and what the budget was. If the smallest morsel it can form would not fit, it stops before processing any data, with the amounts involved.

During a job, a morsel can cost more than Moruna expected, for example when later data has longer text than the first morsel. Moruna then reduces the morsel size and the number of workers at once. If even one small morsel on one worker cannot fit, it stops with a `BudgetError` that names the morsel, what it cost and the budget, rather than letting the process exceed its limit.

Moruna learns what a kernel costs by running it, so the budget must leave room to run it on one morsel. A budget smaller than that cannot be kept for any job.

## Getting more from a budget

- **Read fewer columns.** `columns=` on a Parquet source keeps unused columns out of memory entirely.
- **Avoid building Python objects.** A kernel that uses `pyarrow.compute`, Polars or NumPy on whole columns uses much less memory than one that converts each value to a Python object, so it gets larger morsels and more workers.
- **Tell Moruna what to expect.** If the first morsel is not typical of the data, set `expected_amplification` on the kernel. For a stateful kernel with a large state, such as a model, set `state_bytes`.
- **Start from a clean process.** Create input data in a separate process, or release library memory with `pyarrow.default_memory_pool().release_unused()` before the job.

Next: [Checkpoints and resume](resume.md).
