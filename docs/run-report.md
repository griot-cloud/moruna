# The run report

Every job produces a run report, computed from a record Moruna keeps of every morsel. `moruna.run` returns it, a failed job attaches it to its exception as `error.report`, and the `moruna` command writes it to a file. Printing a report gives a summary; `report.bytes()` is its canonical form, every field, with its digest `report.digest()` (see [Results and errors](python-results.md)). The report gives a screen and any host enough to show each task, and to decide how to run it better the next time.

## The schema

`schema_version` is `"MAJOR.MINOR"`. A reader refuses a report whose major it does not know, and reads a later minor of its major, which only adds fields. This release's schema is `2.0`: `tasks` holds every figure of a task, and the `stages` of schema 1 are gone.

A measurement that was not made is absent from the report, never zero: a zero is a measured zero. A task that ran no morsel has no `wall_ms`; a sink's `snapshot_id` is `null` only when it committed none.

### Units

Every number names its unit, in its field's name or in this table.

| Suffix or field | Unit |
| --- | --- |
| `_bytes` | Bytes. |
| `_ms` | Milliseconds; `started_at_ms` and `ended_at_ms` are after the run started. |
| `cpu_core_s` | CPU time in core-seconds: one core busy for one second is 1. |
| `_cores` | Cores, a mean over a span. |
| `_per_s` | Per second of the task's wall time. |
| `_ratio` | A dimensionless ratio. |
| `mem_byte_seconds` | Bytes held, multiplied by seconds. |
| `rows`, `rows_in`, `rows_out`, `excluded_rows` | Rows. |
| `morsels_in`, `morsels_out`, `failed_morsels`, `skipped_morsels`, `observations` | Morsels. |
| `calls`, `instances`, `cpus`, `runs` | Counts. |
| `percentile` | A percentile, 0 to 100. |

The trace's own fields follow the same rule: a field it measures in seconds, nanoseconds or microseconds is converted to milliseconds and named `_ms`.

## Job fields

| Field | Meaning |
| --- | --- |
| `schema_version` | The report's schema, `"MAJOR.MINOR"`. |
| `moruna` | The version of Moruna that ran the job. |
| `run_id` | The job's id. |
| `job_digest` | The digest of the job's spec, `sha256:` and hex; `null` for a job a spec cannot hold. |
| `args` | The run's argument values, by name. |
| `host` | Where it ran: `platform` (`os/arch`), `cpus`, `memory_limit_bytes`, `cpu_quota_cores`, `limits_source` and `devices`. |
| `exit` | `code`, `status` (`"completed"`, `"terminated"` or `"cancelled"`) and `diagnostic`, the reason the job stopped. |
| `tasks` | One entry per task, in the spec's order; see below. |
| `resources` | `cpu_core_s`, `peak_memory_bytes` (the whole process's) and `memory_basis`, how it was measured. |
| `sinks` | Per sink: its `task`, its `target`, `rows`, `bytes`, `files`, `excluded_rows`, and the `snapshot_id` and `version` it committed (every committing sink names its snapshot). |
| `plan` | The plan the run started from, when asked for (`explain=True`); `null` otherwise. |
| `plan_steps` | With a plan, each step's actual start and end: `step`, `tasks`, `started_at_ms`, `ended_at_ms`, so the parallelism the run achieved shows beside the plan's groups. |
| `resumed`, `manifest` | Whether the job continued from a checkpoint, and the latest checkpoint kept. |
| `wall_ms` | From start to finish. |
| `limits_initial`, `limits_timeline` | The memory limit, CPU quota and devices the job started with, and each change. |
| `peak_anon_bytes`, `peak_fraction_of_ceiling`, `peak_ceiling_bytes` | The most memory the whole process held, as a fraction of the limit in force, and that limit. |
| `worker_busy_fraction`, `cpu_throttled_fraction` | The share of worker time spent running functions, and of time the system held the process back. |
| `staging_engaged`, `staging_bytes_written` | Whether the job spilled to disk, and how much. |
| `bottleneck_timeline` | What the pipeline was waiting for, and for how long. |
| `notes` | Decisions Moruna made and facts it found, in sentences. |
| `trace_path` | The trace file, if `trace` was set. |
| `write`, `read` | What peQL said of a `peql` sink's write and a `datafusion` source's read; `null` otherwise. |
| `buffers`, `policy` | What the buffer catalog and the scheduling policy did. |

## Task fields

Each task's `key` is exactly its name in the spec: a host joins a report to the spec's tasks by it.

| Field | Meaning |
| --- | --- |
| `key`, `role`, `kind` | The task's name, its role (`source`, `function`, `sink`) and its kind. |
| `image_digest` | For an image task, the image it ran. |
| `status` | `ok`, `failed` (a morsel of it failed) or `skipped` (the run ended before it ran). |
| `fused_with` | The tasks that ran in one stage with it; the figures are the stage's, on its first task. |
| `started_at_ms`, `ended_at_ms` | When its first morsel started and its last ended. |
| `wall_ms`, `busy_ms` | From its first morsel to its last, and the time inside it across all instances. |
| `cpu_core_s`, `peak_memory_bytes` | The CPU its morsels used, and the most memory one of its morsels added while it ran. |
| `instances` | How many instances ran it. |
| `rows_in`, `rows_out`, `bytes_in`, `bytes_out`, `morsels_in`, `morsels_out` | What it took and gave. |
| `rows_per_s`, `bytes_per_s` | Its throughput. |
| `amplification_p50_ratio`, `amplification_p95_ratio` | Its memory use as a multiple of its input, typical and near the highest. |
| `state_bytes_max`, `state_growth_bytes` | For a stateful function, its largest state and how much one instance's state grew. |
| `alloc` | For a Python function, the memory it asked for itself, by source; see below. |
| `failed_morsels`, `skipped_morsels`, `excluded_rows` | Morsels that failed, morsels skipped under the error policy, and rows a sink set aside. |
| `input_wait_ms`, `output_wait_ms` | Between two of its morsels, the time its instances waited for the next input to be ready, and the time they waited with it ready (held back by memory admission or a full queue downstream), summed over instances. |
| `needs` | Per need: `calls`, `bytes_in`, `bytes_out` and `time_ms`. A task this process ran is measured through the object the resolver supplied; an image task at its port. |
| `planned` | With a plan: its `step`, the plan's prediction per morsel (`duration_ms` and `memory_bytes`, each `p50`, `p95`, `basis` and `observations`), and what was observed per morsel beside it. |
| `observation` | Its measurement in the form the cost model stores (`moruna-cost`): `key` (`role`, `kind`, `settings`), `outcome` (`origin`, and per morsel `duration`, `worker_time`, `peak_memory`, `rows_out`, `bytes_out`, `state_growth`, in seconds and bytes as the store keeps them) and `units`, the morsels it is the mean of, so another host can aggregate it across runs. |
| `recommendation` | For the next run: `memory` (`value_bytes`), `cpu` (`value_cores`) and `instances` (`value`), each with its `basis` as data: the statistic and percentile it used, what was observed, how many observations and runs it drew on, and, when the plan predicted it, `last_prediction_error_ratio`. |

`alloc` has `refusal_on` (whether a request past the memory ceiling would be refused, the decorator's `memory_guard`), then one entry each for `python` (Python objects), `numpy` (NumPy arrays) and `arrow` (pyarrow's memory pool), each with `requested_bytes`, `requests`, `largest_request_bytes`, `peak_bytes` (the most one call held at once) and `refused`, and finally `peak_bytes`, `outside_arrow_peak_bytes` and `outside_arrow_fraction`.

## Reading a report

A few patterns come up often:

- **`peak_fraction_of_ceiling` is low and `worker_busy_fraction` is high.** The job was limited by CPU, not memory. More cores would make it faster; more memory would not.
- **A task's `input_wait_ms` is high.** It waited for what feeds it: the upstream task, or the source, is the one to speed up.
- **A task's `output_wait_ms` is high.** Its input was ready and it was held back: by memory, or by a slow task downstream.
- **`staging_engaged` is true.** Data waited long enough to be written to disk, usually because the sink was slower than the functions.
- **A task's `alloc.outside_arrow_fraction` is high.** The function builds most of its working memory as Python objects or NumPy arrays rather than Arrow, which Moruna can only observe.
- **`amplification_p95_ratio` is much higher than `amplification_p50_ratio`.** Some morsels cost far more memory than most, which makes Moruna size conservatively.
- **A need's `time_ms` is most of a task's `busy_ms`.** The task waits on what the runner serves it, not on its own work.

The notes explain each decision Moruna made in a sentence, and are the first place to look when a job behaves unexpectedly.
