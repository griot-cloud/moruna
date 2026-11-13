# Workers do the CPU half of I/O

Decision RB1 (Brackly, 2026-11-13): Moruna's workers have no roles. Decoding a source's input, applying a function and encoding a sink's output are all work any worker takes; only waiting on storage stays with the drives and the reactor. There is no parallelism setting.

## What was wrong

The benchmark of 2026-11-13 (NYC taxi trips on an 8-vCPU host) found the two drives doing CPU work on one thread each:

- A Parquet source's `read` fetched its byte ranges and then decoded them inside the future the source drive polls, so every split of a run was decoded on the drive's thread.
- A Parquet sink's `write` encoded each morsel, with Zstd, into the one open file under the sink's lock, on the sink drive's thread.

On a job that writes more than it reads (Enrich), the workers were busy 2% of the run while the sink drive encoded: 21.8 s against 5.2 s for a 16-thread loop. Read-ahead was also capped at 2 splits (`READ_AHEAD_INITIAL`) and writes at 2 in flight (`sink.concurrency`), so even an idle pool could not have decoded on more than two cores or encoded on more than one.

## The shape

A read is two halves and so is a write:

| | I/O half (the drive, the reactor) | CPU half (any worker) |
| --- | --- | --- |
| Source | `Source::fetch`: the byte ranges of a split, into arena buffers | `Source::decode`: those bytes into a payload |
| Sink | `Sink::put`: hand a finished file to the store and record its commit | `Sink::encode`: a morsel into an open file, rolling it when full |

A source or sink that does not split keeps `read` and `write` as they are: `decodes_on_workers` and `encodes_on_workers` default to false, and the drive calls the whole operation as before.

The scheduler holds one offload queue. A drive that has fetched a split pushes its decode; a sink drive that has popped a morsel pushes its encode. The drive keeps a future for the job, which resolves when a worker has run it. A worker takes an offload job at the head of its loop, ahead of a function morsel, under the same slot claim that bounds every unit by the CPU limit in force. Offloaded work is what unblocks the pipeline (a decode feeds the functions, an encode frees the memory behind the sink), so it goes first; how much of it exists is still bounded by the read-ahead and the sink's writes in flight.

A job no worker has taken is the drive's to run: when the run is not live (before `run`, during a probe, while stopping) the drive runs the CPU half inline, so no job can be stranded in the queue.

The caps follow the pool: the read-ahead the solve allows is bounded by the workers rather than by 2, and a sink that encodes on workers may have as many writes in flight as there are workers. The Parquet sink encodes into one open file per write in flight (its lanes), each rolled on its own, so concurrent encodes never contend for one writer.

## What does not change

- SC-I1: a worker still never waits on a `Completion`. Decoding and encoding are CPU only; the waits (the fetch, the put) stay with the drives.
- G-I2: a Parquet decode and a Parquet encode are still the one CPU copy each side is allowed, made in the same place.
- Commits: each lane's file records the sequence numbers it holds, and the ledger's watermark is computed over every committed file as before, so a resume is exact whatever the number of lanes.
- An ordered sink encodes in sequence order on one lane, as before.
