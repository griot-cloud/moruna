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
