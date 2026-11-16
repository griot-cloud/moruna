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
| `canon.py` | The gate's canonical form and content hash (gate-canon/1), the reference; `images/gate`'s `gate-canon` is the fast one |
| `gate_oracle.py` | Runs the Python functions on synthetic inputs and writes the Rust ports' fixtures (`images/gate/fixtures`) |
| `gate_jobs.py` | The gate's jobs as 0.7 job documents: Parquet source, the native task, Parquet sink |
| `gate_plan.py` | The PM's NYC matrix as a plan (`make_plan.py gate-nyc d8\|d4`) |
| `native_exprs.py` | The PM's group 5: Enrich as DuckDB SQL and Polars expressions, Occupancy as DuckDB SQL |
| `runner_split.py` | The time split (control 5) from a Moruna report |
| `report.py` | The NYC matrix from a plan's results, with 0.7's speedups and the PM's target |
| `test_gate_harness.py` | The harness's own tests (`python3 -m pytest -q bench/vm/test_gate_harness.py`) |

## The 0.7 gate (architecture/arrow-core-0.7.md section 9)

The gate runs each job's function ported to Rust (`images/gate`) and compares every engine's
output by its canonical form, not by `checksum`'s column sums:

- `rust-baseline` and `moruna-vm` run without Python. `rust-baseline` is `gate-baseline` from
  `images/gate` (`cargo build --release` there, then `GATE_BIN`), built with the gate's one CPU
  setting (`images/gate/.cargo/config.toml`, AVX2 and FMA) as the tasks and `py066` are; `moruna-vm` is 0.7's
  `moruna run` (stream D, `MORUNA07_BIN`) on the job `gate_jobs.py` writes.
- `moruna066-rust` is 0.6.6 running the Rust port through the `moruna_gate` extension
  (`images/gate/py066`, installed with `/data/venv-t/bin/pip install images/gate/py066`);
  `duckdb-sql` and `polars-expr` are the native-expression rows.
- Every engine writes zstd level 1, row groups of at most 1 Mi rows, files of at most 1 GiB
  (control 3); `runner.py`'s `OUTPUT` and `gate-baseline`'s `Output`.
- `driver.py` records `canon` (rows, `hash_exact`, `hash_all`) for every run, keeps Score's
  tolerant rows in `/data/results/canon/`, and the engine's time split and the VM's boot.
- `report.py /data/results d8` prints the matrix.

## Where it runs

It runs on the benchmark VM. The paths are hard-coded for it:

- The harness lives in `/home/bench/bench` (`driver.py:55, 75-76`, `runner.py:3`).
- The data is on the local NVMe `/data`.
- `/home/bench/setup8.sh` on the VM's OS disk recreates `/data`, downloads the datasets and mirrors
  the results.
- The VM shuts down at 02:00 UTC, which wipes `/data`.

The VM's SSH key and connection details are deliberately not in the repository.
