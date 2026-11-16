# The benchmark VM harness

This is the harness behind the 0.6.5 and 0.6.6 benchmark tables (the 0.6.6 table is in
`CHANGELOG.md` on `fix/adaptive-runtime`). It was copied unchanged on 2026-11-16 from the 0.6.6
release session's scratchpad, which a reboot would clear. The 0.7 no-regression gate runs it
(`architecture/arrow-core-0.7.md`, §7).

| File | What it is |
| --- | --- |
| `make_plan.py` | Writes a run plan: `make_plan.py preflight\|grid\|main` |
| `driver.py` | Runs a plan as root. Each run gets its own cgroup (cpuset, `cpu.max`, `memory.max`/`high`), sampled every 0.5 s. Results go to `/data/results/<id>.json`: `wall_s`, `cpu_s`, `cg_peak_bytes`, `oom_kills` (`driver.py:112-120`). |
| `runner.py` | One timed run of one engine on one workload, inside the driver's cgroup. Engines: Moruna, the loop, Polars, DuckDB, Spark, Ray, Dask. |
| `workloads.py` | NYC workloads: Score, Occupancy, Enrich, Audit |
| `ooc_workloads.py` | Larger-than-memory workloads: Overture shapes and attributes, FineWeb MinHash |
| `normalize.py` | One-time prep: the monthly NYC yellow-taxi files into one schema, with 1M-row row groups |
| `pa_write.py` | pyarrow's Parquet writer on the same table, for a writer comparison |
| `nomallopt.c` | Diagnosis only: ignores the runtime's `mallopt` settings when `NOMALLOPT` is set |
| `ov_urls.txt`, `fw_urls.txt`, `fw_urls_rest.txt` | Public dataset URLs: Overture buildings and FineWeb samples |

## Where it runs

It runs on the benchmark VM. The paths are hard-coded for it:

- The harness lives in `/home/bench/bench` (`driver.py:55, 75-76`, `runner.py:3`).
- The data is on the local NVMe `/data`.
- `/home/bench/setup8.sh` on the VM's OS disk recreates `/data`, downloads the datasets and mirrors
  the results.
- The VM shuts down at 02:00 UTC, which wipes `/data`.

The VM's SSH key and connection details are deliberately not in the repository.
