# The benchmark log

Every run of the node benchmark and every fix it leads to (system design 6C). Written by bench/node/run.py; root causes are filled in by item 6.3.

## 202611181311: plan nyc-2024

- Commit: 7f97070d4a4857e9a977dc85233198ff7c90c262
- Node image: /Users/brackly/moruna-data/images/2f0b5ad0-cloud/moruna-node.vhd (123732480 bytes, sha256 ca5c051a0ad262318c3feb649854ae6b4180ce70552e13655d384933f1250acc)
- Machines: Standard_D4s_v4, zone 3, centralindia, subscription griot-dev
- Linux image: 24.04.202610250
- Data: read and written in Blob container `data`, made in Azure from https://d37ci6vzurychx.cloudfront.net/trip-data/{file}, each file the plan's SHA-256
- Results: bench/node/results/202611181311

0.6.6's wheel: built on the Linux machine, in 1959 s outside every timing: `moruna-0.6.6-cp314-cp314t-linux_x86_64.whl`, sha256 13f1d21d39eb019312399fb6350c3ad06374d2a85648101da5a7009d1d769346.

### score

| Engine | Function | Operational s | Start-up s | CPU s | Peak GiB | Memory given GiB | Data | Read s | Decode s | Function s | Encode s | Write s | Waits s | Output against 0.6.6 | Runs ok |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Moruna 0.7, the node | rust | 93.2 | 1.00 (±1 s) | 0.0 | 1.68 | 12.98 | blob | 63.8 | 16.7 | 81.8 | 10.1 | 23.0 | 8.3 | bit for bit | 5/5 |
| 0.6.6, Python function | python | 66.2 | 0.27 | 253.8 | 8.55 | 14.00 | blob |  |  |  |  |  |  | bit for bit | 5/5 |
| 0.6.6, Rust function | rust | 33.9 | 0.30 | 125.6 | 1.23 | 14.00 | blob |  |  |  |  |  |  | bit for bit | 5/5 |
| the plain Rust pool | rust | 30.0 | 0.05 | 117.9 | 0.97 | 14.00 | local copy, 36.8 s apart |  |  |  |  |  |  | bit for bit | 5/5 |
| DuckDB | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| Polars | python | 153.1 | 0.40 | 168.2 | 6.92 | 14.00 | blob |  |  |  |  |  |  | bit for bit | 5/5 |
| Ray | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| Spark | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| Dask | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |

- **MISS** no regression: Moruna's operational 93.2 s against 0.6.6, Python function 66.2 s (0.71x), 0.6.6, Rust function 33.9 s (0.36x)
- **MISS** the bar: 0.32x the best engine's operational time (the plain Rust pool, 30.0 s, against 93.2 s; the bar is 1.5x), CPU 0.0 s against the least of any row (the plain Rust pool, 117.9 s)
- the stage that loses most: the function, 81.8 s, of the split: read 63.8 s, decode 16.7 s, the function 81.8 s, encode 10.1 s, write 23.0 s, waits 8.3 s
- noise: 0.6.6 against itself, 4 runs, spread 3.7% of the median

### occupancy

| Engine | Function | Operational s | Start-up s | CPU s | Peak GiB | Memory given GiB | Data | Read s | Decode s | Function s | Encode s | Write s | Waits s | Output against 0.6.6 | Runs ok |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Moruna 0.7, the node | rust | 19.2 | 1.00 (±1 s) | 0.0 | 1.60 | 12.98 | blob | 87.2 | 14.7 | 7.2 | 8.1 | 0.0 | 9.8 | bit for bit | 5/5 |
| 0.6.6, Python function | python | 18.0 | 0.25 | 64.2 | 1.77 | 14.00 | blob |  |  |  |  |  |  | bit for bit | 5/5 |
| 0.6.6, Rust function | rust | 11.0 | 0.25 | 15.3 | 1.27 | 14.00 | blob |  |  |  |  |  |  | bit for bit | 5/5 |
| the plain Rust pool | rust | 2.9 | 0.05 | 11.2 | 0.68 | 14.00 | local copy, 28.7 s apart |  |  |  |  |  |  | bit for bit | 5/5 |
| DuckDB | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| DuckDB SQL | sql |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| Polars | python | 42.8 | 0.31 | 53.8 | 4.64 | 14.00 | blob |  |  |  |  |  |  | bit for bit | 5/5 |
| Ray | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| Spark | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| Dask | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |

- **MISS** no regression: Moruna's operational 19.2 s against 0.6.6, Python function 18.0 s (0.94x), 0.6.6, Rust function 11.0 s (0.57x)
- **MISS** the bar: 0.15x the best engine's operational time (the plain Rust pool, 2.9 s, against 19.2 s; the bar is 1.5x), CPU 0.0 s against the least of any row (the plain Rust pool, 11.2 s)
- the stage that loses most: read, 87.2 s, of the split: read 87.2 s, decode 14.7 s, the function 7.2 s, encode 8.1 s, write 0.0 s, waits 9.8 s
- noise: 0.6.6 against itself, 4 runs, spread 1.2% of the median

### enrich

| Engine | Function | Operational s | Start-up s | CPU s | Peak GiB | Memory given GiB | Data | Read s | Decode s | Function s | Encode s | Write s | Waits s | Output against 0.6.6 | Runs ok |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Moruna 0.7, the node | rust | 62.3 | 1.00 (±1 s) | 0.0 | 1.56 | 12.98 | blob | 38.1 | 19.4 | 8.5 | 58.5 | 138.0 | 47.0 | bit for bit | 5/5 |
| 0.6.6, Python function | python | 18.1 | 0.26 | 63.5 | 2.51 | 14.00 | blob |  |  |  |  |  |  | bit for bit | 5/5 |
| 0.6.6, Rust function | rust | 18.5 | 0.33 | 64.5 | 2.51 | 14.00 | blob |  |  |  |  |  |  | bit for bit | 5/5 |
| the plain Rust pool | rust | 13.5 | 0.07 | 52.3 | 2.42 | 14.00 | local copy, 39.8 s apart |  |  |  |  |  |  | bit for bit | 5/5 |
| DuckDB | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| DuckDB SQL | sql |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| Polars | python | 31.4 | 0.42 | 42.7 | 4.14 | 14.00 | blob |  |  |  |  |  |  | bit for bit | 5/5 |
| Polars expressions | expressions | 17.4 | 0.43 | 66.2 | 2.24 | 14.00 | blob |  |  |  |  |  |  | bit for bit | 5/5 |
| Ray | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| Spark | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| Dask | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |

- **MISS** no regression: Moruna's operational 62.3 s against 0.6.6, Python function 18.1 s (0.29x), 0.6.6, Rust function 18.5 s (0.30x)
- **MISS** the bar: 0.22x the best engine's operational time (the plain Rust pool, 13.5 s, against 62.3 s; the bar is 1.5x), CPU 0.0 s against the least of any row (Polars, 42.7 s)
- the stage that loses most: write, 138.0 s, of the split: read 38.1 s, decode 19.4 s, the function 8.5 s, encode 58.5 s, write 138.0 s, waits 47.0 s
- noise: 0.6.6 against itself, 4 runs, spread 2.8% of the median

### audit

| Engine | Function | Operational s | Start-up s | CPU s | Peak GiB | Memory given GiB | Data | Read s | Decode s | Function s | Encode s | Write s | Waits s | Output against 0.6.6 | Runs ok |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Moruna 0.7, the node | rust | 20.2 | 1.00 (±1 s) | 0.0 | 1.57 | 12.98 | blob | 52.8 | 14.2 | 4.8 | 9.9 | 4.2 | 11.4 | bit for bit | 5/5 |
| 0.6.6, Python function | python | 34.1 | 0.25 | 129.3 | 1.18 | 14.00 | blob |  |  |  |  |  |  | bit for bit | 5/5 |
| 0.6.6, Rust function | rust | 11.8 | 0.28 | 19.1 | 1.48 | 14.00 | blob |  |  |  |  |  |  | bit for bit | 5/5 |
| the plain Rust pool | rust | 4.1 | 0.06 | 16.0 | 0.80 | 14.00 | local copy, 30.8 s apart |  |  |  |  |  |  | bit for bit | 5/5 |
| DuckDB | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| Polars | python | 56.6 | 0.40 | 68.0 | 4.16 | 14.00 | blob |  |  |  |  |  |  | bit for bit | 5/5 |
| Ray | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| Spark | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |
| Dask | python |  |  |  |  | 14.00 | blob |  |  |  |  |  |  | no run passed | 0/5 |

- **MISS** no regression: Moruna's operational 20.2 s against 0.6.6, Python function 34.1 s (1.68x), 0.6.6, Rust function 11.8 s (0.58x)
- **MISS** the bar: 0.20x the best engine's operational time (the plain Rust pool, 4.1 s, against 20.2 s; the bar is 1.5x), CPU 0.0 s against the least of any row (the plain Rust pool, 16.0 s)
- the stage that loses most: read, 52.8 s, of the split: read 52.8 s, decode 14.2 s, the function 4.8 s, encode 9.9 s, write 4.2 s, waits 11.4 s
- noise: 0.6.6 against itself, 4 runs, spread 2.1% of the median

Operational time is each engine's own wall on the machine that ran it, from the job handed to the ready engine to the output durably written in Blob: Moruna's report's `wall_ms`; the others' own timer on the Linux machine, from the call that starts the job, after the session or cluster is up. Start-up is Moruna's job record from `submitted` to `started`, to the millisecond (from a node without item 2.4, whole seconds, marked ±1 s), and the others' process start, imports and session or cluster. Every engine reads and writes the run's one Blob container with its own reader inside operational time; a row marked `local copy` has no Blob reader, and its copies are timed apart. Memory given is what each platform leaves a job on the same machine: the node's job bound (`job_memory_bytes`, 3A) for Moruna, the plan's figure on Linux. Medians of the measured repeats; the learning run and the noise pairs are left out.

### Misses (6C)

#### Miss: score, no regression
- Run: 202611181311, plan nyc-2024
- Gap: MISS no regression: Moruna's operational 93.2 s against 0.6.6, Python function 66.2 s (0.71x), 0.6.6, Rust function 33.9 s (0.36x)
- Stage: the function, 81.8 s, of the split: read 63.8 s, decode 16.7 s, the function 81.8 s, encode 10.1 s, write 23.0 s, waits 8.3 s
- Profile: TODO
- Defect: TODO
- Beyond the benchmark: TODO
- Test: TODO
- Work item: TODO
- Not refused: TODO

#### Miss: score, bar
- Run: 202611181311, plan nyc-2024
- Gap: MISS the bar: 0.32x the best engine's operational time (the plain Rust pool, 30.0 s, against 93.2 s; the bar is 1.5x), CPU 0.0 s against the least of any row (the plain Rust pool, 117.9 s)
- Stage: the function, 81.8 s, of the split: read 63.8 s, decode 16.7 s, the function 81.8 s, encode 10.1 s, write 23.0 s, waits 8.3 s
- Profile: TODO
- Defect: TODO
- Beyond the benchmark: TODO
- Test: TODO
- Work item: TODO
- Not refused: TODO

#### Miss: occupancy, no regression
- Run: 202611181311, plan nyc-2024
- Gap: MISS no regression: Moruna's operational 19.2 s against 0.6.6, Python function 18.0 s (0.94x), 0.6.6, Rust function 11.0 s (0.57x)
- Stage: read, 87.2 s, of the split: read 87.2 s, decode 14.7 s, the function 7.2 s, encode 8.1 s, write 0.0 s, waits 9.8 s
- Profile: TODO
- Defect: TODO
- Beyond the benchmark: TODO
- Test: TODO
- Work item: TODO
- Not refused: TODO

#### Miss: occupancy, bar
- Run: 202611181311, plan nyc-2024
- Gap: MISS the bar: 0.15x the best engine's operational time (the plain Rust pool, 2.9 s, against 19.2 s; the bar is 1.5x), CPU 0.0 s against the least of any row (the plain Rust pool, 11.2 s)
- Stage: read, 87.2 s, of the split: read 87.2 s, decode 14.7 s, the function 7.2 s, encode 8.1 s, write 0.0 s, waits 9.8 s
- Profile: TODO
- Defect: TODO
- Beyond the benchmark: TODO
- Test: TODO
- Work item: TODO
- Not refused: TODO

#### Miss: enrich, no regression
- Run: 202611181311, plan nyc-2024
- Gap: MISS no regression: Moruna's operational 62.3 s against 0.6.6, Python function 18.1 s (0.29x), 0.6.6, Rust function 18.5 s (0.30x)
- Stage: write, 138.0 s, of the split: read 38.1 s, decode 19.4 s, the function 8.5 s, encode 58.5 s, write 138.0 s, waits 47.0 s
- Profile: TODO
- Defect: TODO
- Beyond the benchmark: TODO
- Test: TODO
- Work item: TODO
- Not refused: TODO

#### Miss: enrich, bar
- Run: 202611181311, plan nyc-2024
- Gap: MISS the bar: 0.22x the best engine's operational time (the plain Rust pool, 13.5 s, against 62.3 s; the bar is 1.5x), CPU 0.0 s against the least of any row (Polars, 42.7 s)
- Stage: write, 138.0 s, of the split: read 38.1 s, decode 19.4 s, the function 8.5 s, encode 58.5 s, write 138.0 s, waits 47.0 s
- Profile: TODO
- Defect: TODO
- Beyond the benchmark: TODO
- Test: TODO
- Work item: TODO
- Not refused: TODO

#### Miss: audit, no regression
- Run: 202611181311, plan nyc-2024
- Gap: MISS no regression: Moruna's operational 20.2 s against 0.6.6, Python function 34.1 s (1.68x), 0.6.6, Rust function 11.8 s (0.58x)
- Stage: read, 52.8 s, of the split: read 52.8 s, decode 14.2 s, the function 4.8 s, encode 9.9 s, write 4.2 s, waits 11.4 s
- Profile: TODO
- Defect: TODO
- Beyond the benchmark: TODO
- Test: TODO
- Work item: TODO
- Not refused: TODO

#### Miss: audit, bar
- Run: 202611181311, plan nyc-2024
- Gap: MISS the bar: 0.20x the best engine's operational time (the plain Rust pool, 4.1 s, against 20.2 s; the bar is 1.5x), CPU 0.0 s against the least of any row (the plain Rust pool, 16.0 s)
- Stage: read, 52.8 s, of the split: read 52.8 s, decode 14.2 s, the function 4.8 s, encode 9.9 s, write 4.2 s, waits 11.4 s
- Profile: TODO
- Defect: TODO
- Beyond the benchmark: TODO
- Test: TODO
- Work item: TODO
- Not refused: TODO


### Root causes and items (PM, 2026-11-18)

Every output in this run is bit for bit with 0.6.6, on all four jobs and all six repeats. Every
speed comparison is a miss.

- **Score (0.36× 0.6.6's Rust function):** the function ran as one process on a 4-CPU node: 82 of
  the 93 s were the workers waiting for its answers. Item 6.3e (merged, cdb5a7a1): a function runs
  one instance per CPU of the job. Under the emulator a month went from 260–290 s to 106 s.
- **Enrich (0.30×):** writing bound (write 138 s and encode 58.5 s summed over workers). Item 6.3d
  (the Parquet writers' memory and pages) is in progress. Item 6.3h (bytes out counted once per
  buffer, so Enrich's controller believed it amplified 15×) is in progress.
- **Occupancy (0.57×) and Audit (0.58×):** reading is the stage that loses most. Item 6.3l (decode
  CPU grows when reads are cut smaller) is in progress. The read path from Blob on the node is
  otherwise not yet diagnosed.
- **Ray, Dask, DuckDB and Spark never ran:** an environment fault in the harness, not Moruna. Fixed
  by item 6.2c (merged). They are rerun in run 4 with the node.
- **The node's CPU column is 0.0:** the node does not report a job's CPU. Item 6.3i is in progress.
- **The node ran without its cost model** (policy `admission`, nothing predicted). Item 6.3o is in
  progress.


## Run 202611181759 (6.2 run 4, PM, 2026-11-18)

- **Image:** cdb5a7a1, which is 6.3e merged (a function runs one instance per CPU of the job). The
  node was an Azure D4s_v4 in griot-dev with a job budget of 4 CPUs and 12.98 GiB, reading from and
  writing to Blob. The run's resource group is deleted.
- **The other engines did not run.** The Linux machine's setup timed out downloading wheels from
  PyPI (pyarrow, msgpack, pyyaml for Ray). This is a network fault in the harness's setup, not
  Moruna. Their figures stay those of run 3, which also did not run them.
- **Node, medians of the passing runs:**

  | Job | Run 3 | Run 4 | Runs ok |
  |---|---:|---:|---|
  | Score | 93.2 s | 53.0 s | 3 of 5 |
  | Occupancy | 19.2 s | 18.6 s | 5 of 5 |
  | Enrich | 62.3 s | none | 0 of 5 |
  | Audit | 20.2 s | 20.1 s | 5 of 5 |

- **6.3e's gain is real.** Score is 1.76 times faster with 4 instances.
- **6.3e brought three failures.** Item 6.3r (architecture/work/6.3r-instances-fail-on-the-node.md):
  - **Score, 2 of 5 runs:** "a record of 14884864 bytes does not fit its input region, which has
    13586432 bytes free and nothing to release".
  - **Enrich, every run:** "it said a batch was done without a record for it".
  - **Enrich, once:** "previous task account has no certified retirement".

  A fixed count of instances is refused as the fix. The outputs of the passing runs were not
  checked against 0.6.6 in this run: there was no 0.6.6 row to compare with.
- **Occupancy and Audit are unchanged.** They are bound by reading, and 6.3e does not touch that.
- **Next:** rerun the node and the four engines once 6.3r, 6.3g, 6.3d and 6.3h are merged.


## Host measurement: 6.3l, a row group's parts share its dictionary pages (PM, 2026-11-19)

The NYC 2024 year, 4 workers, on the Mac (load about 10). Feature at 0698ea2a^1 against 6.3l on it,
interleaved run by run, medians of 3. Every run completed. The raw table is
`~/moruna-data/measurements/6.3l-year-table.txt`.

| Sink, budget | Decode ms | Wall ms | CPU s |
|---|---:|---:|---:|
| Parquet, 384 MiB | 5504 -> 2800 | 5511 -> 4631 | 18.4 -> 15.6 |
| Parquet, 448 MiB | 3498 -> 2574 | 4622 -> 4578 | 15.7 -> 15.1 |
| Parquet, none | 2438 -> 1983 | 4700 -> 4299 | 14.8 -> 14.2 |
| count, 384 MiB | 1932 -> 1752 | 1303 -> 1272 | 1.96 -> 1.78 |
| count, 448 MiB | 2028 -> 1866 | 1381 -> 1272 | 1.98 -> 1.83 |
| count, none | 2000 -> 1799 | 1270 -> 1165 | 1.85 -> 1.79 |

The gain is largest where the arena cuts the most parts. On the node, 6.3l's own analysis found
that Occupancy and Audit were bound by the drive's one-at-a-time fetch (6.3t), not by decode.


## Run 202611182200 (6.2 run 5, PM, 2026-11-19): the node's rows

- **Image:** b11c1342. It has 6.3e, 6.3r (wait for room), 6.3s, 6.3n, 6.3t (requests in flight
  together), 6.3l, the 6.3h stack and 6.3g phases 1–2. It does not have the asynchronous fetch,
  6.3g phase 3 or 6.3r's gap fix: those are in run 6.
- **The machine:** an Azure D4s_v4 node in griot-dev, 4 CPUs, 12.98 GiB, Blob.
- **The node, medians of the passing runs:**

  | Job | Run 3 | Run 4 | Run 5 | Runs ok | 0.6.6, Rust (run 3) |
  |---|---:|---:|---:|---|---:|
  | Score | 93.2 s | 53.0 s | 55.8 s | 6 of 6 | 33.9 s |
  | Occupancy | 19.2 s | 18.6 s | 16.3 s | 6 of 6 | 11.0 s |
  | Enrich | 62.3 s | none | 69.5 s | 6 of 6 | 18.5 s |
  | Audit | 20.2 s | 20.1 s | 17.6 s | 4 of 6 | 11.8 s |

- **What changed:**
  - Score and Enrich complete in every run at 4 instances (6.3r).
  - Occupancy and Audit read faster (6.3t, 6.3l).
  - Audit failed twice with "it said a batch was done without a record". The cause was a gap in
    6.3r's wait: the batch being taken was not counted as outstanding. It is fixed and booted
    (75347b85) and in run 6.
- **What bounds each job now:**
  - **Score:** the engine workers are idle about 27 s each of 57 s, and the bottleneck is Idle
    for 33 s. The four task processes and the four workers share 4 CPUs, waiting on the ring's
    wakeups (1.11e and 6.3p, in progress).
  - **Enrich:** waits on memory for 46 of 63 s. The sink awaits each ~42 MB file's upload at
    ~8 MB/s an upload with 2 parts in flight (6.3u). The node's report now times uploads, from
    run 6 on.
  - **Occupancy and Audit:** the drive fetched one row group at a time. The asynchronous fetch
    (a906df5d) is in run 6.
- **The Linux engines installed this time** (uv's timeout fix). Ray failed every job on a missing
  aiohttp; fixed in the harness (a5c212c5). Dask's Enrich r0 lost its workers. The full table
  follows when run 5 ends.

### Run 202611182200, the full table (PM, 2026-11-19)

Every Moruna output is bit for bit with 0.6.6.

Medians, in seconds. These rows are the table's own (5 measured runs), so the node's figures differ
slightly from the 6-run medians above.

| Job | Moruna node | 0.6.6 Python | 0.6.6 Rust | Plain Rust pool | DuckDB | Polars | Spark | Dask |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Score | 55.3 | 59.9 | 30.5 | 27.7 | 139.1 | 142.3 | 120.9 | 78.0 |
| Occupancy | 16.5 | 17.8 | 11.4 | 2.8 | 40.1 (SQL 19.0) | 41.3 | 78.4 | 33.9 |
| Enrich | 71.0 | 17.7 | 18.1 | 13.3 | 28.7 | 29.8 (expressions 16.7) | 123.2 | failed |
| Audit | 17.7 (3 of 5) | 33.8 | 12.7 | 4.0 | 54.8 | 56.0 | 92.8 | 37.2 |

Ray failed every job on a missing aiohttp (fixed in a5c212c5). Dask's Enrich lost its workers in every
run.

- **Against the Python engines,** Moruna is faster on every job, Enrich excepted.
- **Against 0.6.6's Rust function:**
  - Score 0.55×, Occupancy 0.69×, Audit 0.72×, Enrich 0.25×.
  - The plain Rust pool, which reads from Blob into one process, reads the year in about 2.8 s, so
    reading from Blob on Linux runs at about 300 MB/s. The node reads at about 52 MB/s, with one row
    group fetched at a time.
  - The asynchronous fetch (a906df5d) is in run 6. Enrich is bound by its uploads (6.3u).

### Run 202611190418, the node alone at cc3086ad (PM, 2026-11-19)

Image cc3086ad carries:
- 6.3t's asynchronous fetch;
- 6.3d's writers' claims;
- 6.3g's measured memory;
- 6.3f;
- 6.3r's gap fix;
- 6.3u's object timing;
- the kernels.

It does not carry 1.11e (a wake preempts a busy CPU after 500 µs) or 6.3p, both merged after it.
Gate tasks are built at 1fceede8. The outputs' bit-for-bit check was still running when this was
written.

| Job | Runs | Run 6 (s) | Run 5 median (s) |
|---|---|---|---:|
| Score | 6 of 6 | 57.0, 60.4, 63.2, 65.1, 65.1, 67.3: slower every run | 55.3 |
| Occupancy | 4 of 6 | 16.8, 17.8, 18.1, 19.2 | 16.5 |
| Enrich | 6 of 6 | 71.5, 81.5, 78.6, 73.7, 77.2, 73.2 | 71.0 |
| Audit | 4 of 6 | 19.3, 19.8, 19.7, 19.3 | 17.7 |

- **The failures, 2 of Occupancy's and 2 of Audit's, are one defect (6.3x).** At the drain a task
  stalls for about 44 s, then a batch is released with no record.
- **The object stores' figures (6.3u's note) name the disease.** Every job reads the same 843 MB
  in 139 requests.

| Job | A read | A write | Requests in flight on average |
|---|---:|---:|---:|
| Occupancy | 15–22 MB/s | ≈10 MB/s | reads ≈2.4 |
| Audit | 23–30 MB/s | ≈10 MB/s | |
| Enrich | 11–17 MB/s | 5.5–6.3 MB/s | writes ≈2.9 |
| Score, all 4 CPUs computing | 4.6, falling to 3.1 MB/s | 3.2–3.7 MB/s | |

- **One stream is bound by the node's round trip.** A 256 KiB TCP window (the netstack's
  default) over the 11–13 ms that 1.11d measured on the node gives about 21 MB/s. That round trip
  is the scheduler's 10 ms slice: a woken network or driver thread waits behind a computing one,
  which is why Score's streams fall to 3–5 MB/s. 1.11e is the fix, and run 7 measures it.
- **Too few requests are in flight.** The controller starts the read-ahead at the worker count (4)
  and raises it only when the workers idle (`moruna-controller` `read_ahead`, `classify.rs`).
  Q0 peaked at 87 MB of a 3.16 GB tier, and the job's input would fit in its 7.4 GB arena many
  times over. Each Enrich upload has 2 parts in flight.
- **What follows run 7:**
  - reads in flight sized by the store's rate and the arena's room, not by the worker count;
  - more parts in flight per upload;
  - TCP buffers at the bandwidth-delay product, if the round trip stays long.

### Run 202611190506, the node alone at 15a67df7 (PM, 2026-11-19)

Run 6's image plus 1.11e, a wake that preempts a busy CPU after 500 µs, and 6.3p's rule d. Gate
tasks are built at 1fceede8, as in run 6.

| Job | Runs | Run 7 (s) | Run 6 (s) | 0.6.6 Rust (s) |
|---|---|---|---|---:|
| Score | 6 of 6 | 54.4, 54.7, 54.6, 55.1, 54.3, 54.9 | 57.0–67.3, slower every run | 30.5 |
| Occupancy | 6 of 6 | 16.2, 16.2, 16.2, 16.3, 16.4, 16.4 | 16.8–19.2, 4 of 6 | 11.4 |
| Enrich | 6 of 6 | 60.0, 72.9, 75.2, 69.5, 64.7, 65.6 | 71.5–81.5 | 18.1 |
| Audit | 6 of 6 | 17.6, 17.8, 17.9, 18.2, 17.8, 18.4 | 19.3–19.8, 4 of 6 | 12.7 |

- **Every job completed: 6.3x's stall did not happen in 12 Occupancy and Audit runs.** 6.3p moved
  the engine's flush before it reads the slots, and a task still marked waiting there had its port
  read. That may be what closes it. The item stays open until run 8 shows the same.
- **1.11e reached the network path.** With every CPU computing, Score's reads went from 3.1–4.6 to
  12–18 MB/s a request, and its run no longer slows from one run to the next. Occupancy's and
  Audit's went from 15–30 to 23–28 MB/s.
- **One request still runs at about 25 MB/s with the CPUs free, so the in-flight count is the
  lever (6.3y, in run 8).**
  - Score is now mostly compute: 39 s Compute, 15 s Idle and 0.3 s IoRead of 54 s. It decodes
    for 30 s and encodes for 23 s, summed over the workers, beside its function's tasks on the
    same 4 CPUs.
  - Its gap to 0.6.6 is decode, encode and the function, where the MLP kernel (4.6) applies.

### Run 202611190617, run 8b: the node alone at bench/run8b (PM, 2026-11-19)

Run 7's image plus 6.3y (the request budget, 5 MiB parts, read-ahead by the room), and be2ad8ac,
which lets the network server hold a TLS socket for every socket its grants allow. Gate tasks are
built at 1fceede8.

Run 8, at 6.3y alone, was stopped. Every Score run's upload was refused: "the network server
answered Remote(TooMany)". The server held 16 TLS sockets in all while granting a job 64, and the
request budget reached the seventeenth.

| Job | Runs | Run 8b (s) | Run 7 (s) | 0.6.6 Rust (s) |
|---|---|---|---|---:|
| Score | 6 of 6 | 49.5, 48.6, 49.7, 49.2, 50.0, 49.5 | 54.3–55.1 | 30.5 |
| Occupancy | 6 of 6 | 16.1, 16.8, 16.8, 16.8, 16.5, 16.0 | 16.2–16.4 | 11.4 |
| Enrich | 6 of 6 | 53.7, 60.4, 61.8, 59.8, 58.1, 57.5 | 60.0–75.2 | 18.1 |
| Audit | 6 of 6 | 17.1, 17.7, 17.3, 17.0, 16.8, 16.9 | 17.6–18.4 | 12.7 |

- **The jobs that write faster gained.**
  - Score is about 10% faster.
  - Enrich's median is about 9% faster: 59 s against 67.5 s.
  - Each file goes up in parts in parallel: Enrich's 23 files went in 240 parts at 8.6 MB/s a part,
    against 38–67 parts at about 6 MB/s.
  - Audit gained 4%.
- **Occupancy did not move, because the node reads at about 55 MB/s in all.**
  - About 11 reads are in flight, against about 2.4 in run 7, but each now runs at 4.7–6.2 MB/s
    against 23–28.
  - Linux on the same size reads at about 300 MB/s.
  - The cap is the node's network path, wherever it is, and nothing on the node reports it yet.
    That is item 6.3ab.
- **6.3x's stall did not occur in 12 Occupancy and Audit runs**, the second run in a row.

Every output is bit for bit (24 of 24).

### Run 202611190704, run 9: the node alone at feature's tip (PM, 2026-11-19)

Run 8b plus three changes: 6.3m (an answer belongs to its morsel), 6.3v (Linux only) and 6.3z (a
ring task stamps each piece's time). The gate tasks are rebuilt at the same commit, and attach at
version 5.

| Job | Runs | Run 9 (s) | Run 8b (s) |
|---|---|---|---|
| Score | 6 of 6 | 51.3, 52.6, 54.1, 53.2, 52.4, 53.4 | 48.6–50.0 |
| Occupancy | 6 of 6 | 16.9, 17.3, 17.7, 16.7, 16.9, 17.9 | 16.0–16.8 |
| Enrich | 6 of 6 | 62.7, 62.1, 64.2, 61.1, 57.8, 60.1 | 53.7–61.8 |
| Audit | 6 of 6 | 18.1, 18.5, 18.2, 17.7, 17.9, 18.5 | 16.8–17.7 |

- **Every output is bit for bit (24 of 24). 6.3x's stall did not occur for the third run.**
- **Every job is about 5% slower: the morsel search settled smaller.**
  - Score: 16.8 → 8.4 → 4.2 MB, against 15.9 → 8.0 → 5.7.
  - Audit: 8.4 MB, against 16.8.
  - Score read in 863 pieces against 465, and decoded for 34.9 s against 25.7 s, summed.
- **Why (corrected the same day):**
  - The search reads the records' CPU, which is still the engine worker's for each unit.
  - Since 6.3m a unit that completes no morsel leaves no record, so its CPU is lost, and one that
    completes several puts its whole CPU on each. That bookkeeping, not the function's time,
    moved the search.
  - The search also counts none of the decode and encode a smaller morsel costs.
  - Item 6.3ac makes its cost the stage's whole CPU per byte, from counters that miss no unit.

### Run 202611190756, run 10: the node alone at 80ce24c6 (PM, 2026-11-19)

Run 9 plus 6.3w (the anon fit takes the measured line) and 6.3ab's network meters, with the
harness keeping each job's meter lines. It does not have 6.3ac (merged later).

| Job | Runs | Run 10 (s) | Run 9 (s) |
|---|---|---|---|
| Score | 6 of 6 | 54.2, 54.5, 54.1, 54.0, 53.0, 52.9 | 51.3–54.1 |
| Occupancy | 6 of 6 | 18.2, 18.8, 17.8, 17.7, 18.2, 17.4 | 16.7–17.9 |
| Enrich | 6 of 6 | 63.5, 65.4, 62.8, 65.5, 64.9, 63.9 | 57.8–64.2 |
| Audit | 5 of 6 | 18.6, 18.4, 17.8, 18.1, 18.4; r5 failed at 74.6 s | 17.7–18.5 |

- **Every completed output is bit for bit (23).**
- **Audit r5 failed with 6.3x's error**, "it said a batch was done without a record". It had not
  occurred in runs 7 to 9, and it is not closed.
  - Two tasks each had two more batches than records (123/121, 165/163).
  - The stage was Idle 58.6 s of 74.6 s, and its workers spent 120 s summed taking answers.
- **The meters name the network ceiling.** The network server's totals at the node's 405th second:
  - it handed 5.06 GB to its clients in 87.3 s of copying, 58 MB/s, which is the node's ceiling;
  - its TLS decrypt ran 5.06 GB in 10.9 s (464 MB/s; ring's AES-NI);
  - it tried every pending receive at every poll: 149,618 answered of 5,205,680 tried;
  - netvsc dropped 16,716 frames and left frames queued in 82,805 of 141,067 answers.

  The copies and the retries are item 6.3ad.
- **About 3–6% slower than run 9 on every job but Score**, cause not yet named. Candidates:
  - the meter lines, each holding the serial console tens of ms at most every 10 s;
  - 6.3w's workers.

  Run 11 has 6.3ac and should show where it stands.
