# How it works

Moruna runs a job as a stream of morsels passing through a pipeline: the source reads them, the kernels transform them, and the sink writes them. Between each step is a queue. Moruna decides how large the morsels are, how many are in flight and when to move queued data to disk, and revisits those decisions while the job runs.

## From a job to its report

1. **Read the limits.** Moruna finds the memory budget, the number of cores and a staging directory for spilling, from your arguments, the container's limits or the machine.
2. **Reserve memory.** It measures how much memory the process already holds and sets aside a region for the data it manages, leaving room for the memory your kernels allocate themselves. Learn more in [Memory and the budget](memory.md).
3. **Measure the kernels.** It runs each kernel on one morsel and records how much memory the kernel used relative to its input, and how long it took.
4. **Plan the morsels.** From those measurements it chooses a morsel size and a number of workers that fit the budget, and plans the source's reads.
5. **Run.** The source reads ahead into the first queue. Workers take morsels from a queue, run a kernel on them and put the result in the next queue. The sink takes finished morsels from the last queue and writes them.
6. **Adjust.** Four times a second, Moruna checks memory use and where the pipeline is waiting, then changes morsel sizes, the number of active workers or how far the source reads ahead.
7. **Checkpoint and report.** Every few seconds it records how far the job has got. When the source is exhausted and the sink has written everything, it returns the run report.

## The queues and the disk

Each queue has a size in bytes. When a queue is full, the step before it waits, which stops the source from reading more than the kernels can process and the kernels from producing more than the sink can write.

When waiting is not enough, for example when the sink is slow for a long time, Moruna moves morsels from the queues to the staging directory and reads them back when there is room. A job that spills runs more slowly but continues. The run report says how much was written to disk.

## Choosing sizes

A morsel that is too small wastes time on overhead; one that is too large uses too much memory. The right size depends on how much memory the kernel uses per byte of input, which Moruna calls the kernel's **amplification**. A kernel that turns a text column into Python strings might use ten times the memory of its input; one that adds a number to each row uses almost none.

Moruna starts from the measurement it took before the job and updates it with every morsel. If a morsel uses more memory than expected, Moruna reduces the morsel size and the number of workers at once, and increases them again gradually as measurements agree. Where the kernel's memory does not depend on the morsel size, for example a model loaded once, Moruna treats it as a fixed cost and does not shrink morsels to pay for it.

Kernels you run often have a **profile**: Moruna saves what it measured about a kernel after each job and uses it to start the next job closer to the right sizes.

## Engines inside a job

A kernel can hand its batch to another engine without copying it: Polars kernels receive their batch as a Polars frame. A job can also read from a peQL query, so that the rows and columns it reads are the ones peQL's policies allow for the caller, and it can write its output under a peQL contract. Moruna streams the query's results through its queues rather than waiting for the whole result, so the query can be larger than the budget. See the [job document reference](job-document.md) for how to name them, and the [peQL documentation](https://griot-cloud.github.io/peQL/) for contracts and callers.

```{toctree}
:hidden:

memory
resume
```

Next: [Memory and the budget](memory.md).
