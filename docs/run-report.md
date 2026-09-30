# The run report

Every job produces a run report, computed from a record Moruna keeps of every morsel. `moruna.run` returns it, a failed job attaches it to its exception as `error.report`, and the `moruna` command writes it to a file. Printing a report gives a summary; `report.to_json()` gives every field.

## Job fields

These are attributes of the report object and keys of `to_json()`.

| Field | Meaning |
| --- | --- |
| `run_id` | The job's id. |
| `exit` | `"Completed"`, or the reason the job stopped. |
| `resumed` | Whether the job continued from a checkpoint. |
| `manifest` | The path of the latest checkpoint, if one was kept. |
| `wall_s` | Seconds from start to finish. |
| `limits` | The memory limit, CPU quota and devices the job started with, and where they came from. |
| `peak_anon_bytes` | The most memory the whole process held during the job. |
| `peak_fraction_of_ceiling` | `peak_anon_bytes` as a fraction of the memory limit in force at that moment. |
| `worker_busy_fraction` | The share of worker time spent running kernels. |
| `cpu_throttled_fraction` | The share of time the operating system held the process back for exceeding its CPU quota. |
| `source_bytes_per_s` | How fast the source delivered data. |
| `staging_engaged` | Whether the job spilled to disk. |
| `staging_bytes_written` | How much it spilled. |
| `gil` | For each stage, whether Python kernels ran free-threaded or under Python's global lock. |
| `sizer_used` | How morsels were sized. |
| `bottleneck_timeline` | What the pipeline was waiting for, and for how long: reading, a kernel or writing. |
| `stages` | One entry per kernel; see below. |
| `notes` | Decisions Moruna made and facts it found, in sentences. |
| `trace_path` | The trace file, if `trace` was set. |

`to_json()` and the report file also include the following. `limits_timeline` lists each change to the machine's limits during the job. `cpu_ns` is the CPU time the job used. `mem_byte_seconds` is its memory use over time, the sum of memory held multiplied by time. `peak_ceiling_bytes` is the limit that `peak_fraction_of_ceiling` was measured against.

## Stage fields

`stages` has one entry for each kernel, in order.

| Field | Meaning |
| --- | --- |
| `morsels` | Morsels the kernel processed. |
| `rows_in`, `rows_out` | Rows received and returned. |
| `bytes_in`, `bytes_out` | Bytes received and returned. |
| `wall_s`, `kernel_busy_s` | Time the stage was active, and time spent inside the kernel across all workers. |
| `rows_per_s`, `bytes_per_s` | Throughput. |
| `amplification_p50`, `amplification_p95` | The kernel's memory use as a multiple of its input: typical and near the highest. |
| `errors`, `skipped` | Morsels that failed, and morsels skipped under the error policy. |
| `state_bytes_max`, `state_growth` | For a stateful kernel, the largest state and how much it grew. |

## Reading a report

A few patterns come up often:

- **`peak_fraction_of_ceiling` is low and `worker_busy_fraction` is high.** The job was limited by CPU, not memory. More cores would make it faster; more memory would not.
- **`worker_busy_fraction` is low.** The kernels were waiting, usually for the source or the sink. `bottleneck_timeline` says which.
- **`staging_engaged` is true.** Data waited long enough to be written to disk, usually because the sink was slower than the kernels. The job was correct but slower than it could have been.
- **`amplification_p95` is much higher than `amplification_p50`.** Some morsels cost far more memory than most, which makes Moruna size conservatively. Look for rows much larger than the rest.

The notes explain each decision Moruna made in a sentence, and are the first place to look when a job behaves unexpectedly.
