# The gate's functions in Rust

The 0.7 no-regression gate (`architecture/arrow-core-0.7.md` section 9) runs each of 0.6.6's seven
benchmark jobs with its function ported to Rust: the same input, the function, 0.6.6's output
rows, timed against 0.6.6. This directory is the image-side code for it. It is not Moruna's
runtime, so it is its own Cargo workspace, as `images/python` is stream P's.

| Path | What it is |
| --- | --- |
| `functions/` | The seven functions: a `no_std` core over column slices, and Arrow `RecordBatch` in, `RecordBatch` out on arrow-rs 59.3.0 (`batch.rs`), and gate-canon/1 (`canon.rs`) |
| `record/` | A function on the data plane: a page-aligned input record read in place, the output record written into a window (`apply`), `no_std`, safe. The native path until 5.1, kept as the tasks' reference |
| `tasks/` | The seven native tasks, `gate-JOB`, programs on `moruna-function` (`kernel/function/rust`, system design 5A) built for the core with std (`tasks/build.sh`, item 5.0) |
| `baseline/` | `gate-baseline`, the PM's control 2 (a rayon pool over arrow-rs and parquet), and `gate-canon`, the canonical hash of any engine's output |
| `py066/` | `moruna_gate`, the PM's control 1: the seven ports as a Python extension 0.6.6's `moruna.run` runs (`check.py` checks a build against the fixtures) |
| `fixtures/` | The oracle's inputs and Python's outputs (`bench/vm/gate_oracle.py`), and `manifest.json` |
| `functions/data/` | NumPy's draws, exported bit for bit: Score's weights and the hour tables, FineWeb's permutations |

`cargo test` here runs every port against the fixtures with no Python. `bench/vm/gate_oracle.py`
writes them again; `--check-constants` on another machine checks its NumPy draws the same
constants.

## The equivalence records (the PM's control 6)

The records hold for the gate's final build: x86-64 with AVX, AVX2 and FMA (`.cargo/config.toml`,
below), checked by running the tests as `x86_64-apple-darwin` code under Rosetta 2, which
executes AVX2 and FMA, and again as native arm64 code. Both gave the same counts and the same
largest differences.

Each port against the Python function it replaces, on the oracle's synthetic inputs in the job's
real input schema (nulls, edge values, unicode, offsets into shared buffers, empty batches, and
the cases Python raises on). Rows are counted on the canonical form (gate-canon/1, below).
`hash_all` equal means every column matched bit for bit; `hash_exact` equal means every column
but the named tolerant ones did, and those are within their tolerance row by row.

| Job | Python function (0.6.6) | Port | Fixture rows matched | Content hash | Tolerance, named before measuring |
| --- | --- | --- | --- | --- | --- |
| NYC Score | `workloads.py` `score` | `functions/src/nyc.rs` `score`, its products on `gemm.rs` | 5,118 of 5,118 (5 cases) | `hash_all` equal on every case | `predicted_tip`: absolute 1.1e-6, one step of its sixth decimal. The three products run on `matrixmultiply`'s GEMM, which orders and fuses its sums its own way, as BLAS does, and the features use musl's `log1p`. No value differed on the fixtures, on the AVX2 and FMA kernel or on NEON's. |
| NYC Occupancy | `workloads.py` `occupancy` | `nyc.rs` `occupancy` | 194,776 of 194,776 (5 cases), regrouped | `hash_all` equal | none |
| NYC Enrich | `workloads.py` `enrich` | `nyc.rs` `enrich` | 3,343 of 3,343 (5 cases) | `hash_all` equal | none |
| NYC Audit | `workloads.py` `audit` | `nyc.rs` `audit` | 4,518 of 4,518 (4 cases) | `hash_all` equal | none |
| Overture shapes | `ooc_workloads.py` `ov_shape` | `functions/src/overture.rs` `shapes`, `geom.rs` | 871 of 871 (4 cases) | `hash_exact` equal | `area_m2`, `perimeter_m`, `compactness`: relative 1e-9; `lon`, `lat`: absolute 1e-9 degrees. The scaling's `cos` is NumPy's in Python and musl's here, and `perimeter_m` subtracts nearby scaled coordinates, which magnifies its last bit. Observed: `area_m2` 29 of 871 values differ, at most 3.3e-16 relative; `perimeter_m` 73, at most 2.3e-11; `compactness` 81, at most 4.6e-11; `lon` and `lat` none. |
| Overture attributes | `ooc_workloads.py` `ov_attrs`, `_norm_name` | `overture.rs` `attrs` | 811 of 811 (3 cases) | `hash_exact` equal | `volume_m3`: relative 1e-12, for `math.cos` (the platform's libm against musl's). Observed: 17 of 811 differ, at most 3.1e-16. |
| FineWeb MinHash | `ooc_workloads.py` `fw_minhash`, `_signature` | `functions/src/fineweb.rs` | 501 of 501 (3 cases) | `hash_all` equal | none |

Each job runs one function: `runner.py` takes `getattr(workloads or ooc_workloads, WORKLOAD)`, so
FineWeb MinHash is `fw_minhash` alone. `fw_clean` is a job of its own, not on 0.6.6's table, and is
not ported.

The cases Python raises on, each port refuses with the same error: Audit and Occupancy with a null
timestamp, Enrich with a duration past 2**53 microseconds (pyarrow's cast), `ov_shape` with a WKB
GEOS refuses or an empty geometry (`get_x` on its empty centroid), `ov_attrs` with a null bound
of a box it uses. A task refuses such a batch and ends, as the stage of a raising Python function
did. Untrusted input never panics a port: the record reader goes through the engine's own record
check, every offset is checked, and a WKB count is checked against its bytes before anything is
allocated.

The same records hold for the native path: `record/tests/plane.rs` makes each fixture's input a
record as the engine lays one out, applies the job as a task does, passes the output through the
engine's record check, and finds it bit for bit the arrow-rs port's.

### What the ports take from Python and NumPy

- Python's `round(x, 2)` is the double nearest the exact value's decimal rounding, ties to even
  (`pyfloat::py_round2`, checked against 38,088 values); NumPy's `np.round(x, 6)` is
  `rint(x * 1e6) / 1e6`.
- Python's `max(a, b)` keeps a NaN `a`; NumPy's `maximum` and `clip` propagate NaN; NumPy's float
  `//` and `%` are `npy_divmod`'s.
- NumPy holds an integer column with a null as doubles (NaN for the null). Score then works in
  floats, which equals the integer path for timestamps within +/- 2**53 microseconds; Occupancy
  then raises (null times), or keys its zones as doubles (a null zone), as the port does.
- A float cast to an integer is x86-64's (`cvttsd2si`: the minimum integer for NaN and anything
  out of range), the gate's machine. The oracle runs on arm64, where NumPy gives 0, so it writes
  x86-64's value for `ov_shape`'s tiles of a null geometry, the one place a NaN is cast.
- 0.6.6's `ov_shape` writes `perimeter_m` as `int64` zeros for a batch with no two coordinates in
  one geometry (`np.bincount` of nothing is an integer array); so does the port.
- `str.split()` and `str.isspace()` count U+001C to U+001F as whitespace, which Rust does not.
  Case mapping and NFKC are Rust's and `unicode-normalization`'s Unicode 17 tables against Python
  3.14's 16.0: they differ only on characters new in 17.
- A NaN's payload is not compared: NumPy's sort writes 0x7fff_ffff_ffff_ffff on one machine and
  another on the next, and gate-canon/1 maps every NaN to one.
- Floating point is `libm`'s (musl's functions in Rust) everywhere, and Score's products are
  `matrixmultiply`'s: the native task, the baseline and 0.6.6's extension build with the same
  flags and compute the same bits. Rust never fuses a multiply and an add on its own, so FMA
  changes only the GEMM's kernel, not `libm` or the geometry: the Overture figures above are the
  same with it and without it.

The oracle ran on macOS arm64: Python 3.14.3, NumPy 2.4.2 (Accelerate's BLAS), pyarrow 24.0.0,
shapely 2.1.2 on GEOS 3.13.1. 0.6.6's benchmark ran on x86-64 Linux; the gate's first VM run
compares the ports with 0.6.6's own outputs there, on the real data, by the same hash.

## The same function on 0.6.6 (the PM's control 1)

0.6.6 cannot name a Rust function in a job. The `rust` entry of strict-guest-kernels
(`0796bb1a`, PR #44, shipped in 0.5.1) looked a kernel up in a table the embedding binary
compiled in; MC1 (`dab9f3ed`, 0.5.8) replaced it with the registry of kinds, and
`architecture/sdd/18-kinds.md` (KD-I7) records that `rust` is not registered. On 0.6.6
(`fix/adaptive-runtime`) a job naming it is refused ("no function kind `rust` is registered;
this build runs image, package, std"), and the wheel's `moruna.run` and `moruna run` start from
`Registry::standard()` with no hook for a Rust kind.

What 0.6.6 does run is any Python callable, so `py066/` builds the ports as an extension module,
`moruna_gate`, whose functions take a pyarrow `RecordBatch` and return one over the Arrow C data
interface, with the GIL released and free-threading declared. `runner.py`'s `moruna066-rust`
engine runs `moruna.run(ParquetSource, [moruna.function(moruna_gate.JOB)], ParquetSink)`, the
same as its `moruna` engine with the Python function: 0.6.6 timed with the Rust function, and the
report says how. `py066/check.py` finds every fixture case matching through it.

## gate-canon/1

The content hash every engine's output is compared by: `bench/vm/canon.py` is the reference and
documents it, `functions/src/canon.rs` implements it (and `gate-canon` runs it over Parquet
files), and the tests check the two give the same hashes. A per-batch aggregate (Occupancy) is
regrouped by its keys first, so the hash does not depend on where batches split; every other gate
function is row-wise. The hash is a multiset hash (the sum of the rows' hashes), the same for the
same rows in any order or split.

## One build for every Rust row

`.cargo/config.toml` is the gate's one setting for the CPU: `+avx,+avx2,+fma` on x86-64. The
native tasks (`tasks/build.sh` puts its own flags in the target's rustflags, which cargo joins
with the file's), `gate-baseline` and `py066` all build under this directory, so they get the
same flags and the same GEMM kernel. `matrixmultiply` is built without its `std` feature, so it
picks the kernel at compile time from those features (its `src/x86/macros.rs`), with no CPU
detection inside the task; with them it is the AVX2 and FMA kernel. `py066` links
`matrixmultiply`'s `std` through pyo3-arrow's numpy, so it detects at run time, and finds the
same kernel on a machine with the features. A `RUSTFLAGS` in the environment would replace the
file's flags: leave it unset when building the gate.

Score's function time on one core, 500,000 rows in two batches (this Mac, an Apple M5):

| Score's function | Time on one core |
| --- | --- |
| NumPy 2.4.2, Accelerate's BLAS on one thread (`VECLIB_MAXIMUM_THREADS=1`) | 0.20 s |
| The port on `matrixmultiply` (NEON, fused) | 0.41 s |
| The port before, its own 4 x 8 register tile, unfused | 0.72 s |
| The port first, sums in order row by row | 1.33 s |

Accelerate runs `dgemm` on Apple's matrix coprocessor, so this Mac flatters NumPy; on the gate's
x86-64 machines it is OpenBLAS against the AVX2 and FMA kernel, measured by the VM's first run
(`rust-baseline` against `moruna`, the function's share in each one's split). The network's
products are most of Score's time: 9.7 billion multiply-adds for these rows.

## The native tasks

Each task is a function on `moruna-function` (system design 5A, item 5.1), the SDK's first user
and its conformance suite: `main` is `moruna_gate_tasks::serve(batch::JOB)`, which calls the job's
arrow-rs port (`functions/src/batch.rs`) on each batch, read in place from its record, and writes
its output back into windows: one record when it fits a window, several in order, cut by rows, when
it does not. A refusal's reason goes to the task's log and the task ends with exit code 7, as
before; the exit codes are the ones the tasks always had (`moruna_function::code`).
`tasks/tests/serve.rs` runs each task on the data plane in memory (`moruna_function::host`) over
every fixture case and finds its rows bit for bit those of the record path the tasks ran on
before 5.1 (`record/`), and the oracle's `hash_exact`, and each record the bytes the record path's
took: arrow-ipc writes a validity bitmap of all ones for a column with no nulls, a page of its own
in a record, and `moruna-function` leaves it out as the record path did.

`tasks/build.sh` builds `gate-JOB` for the seven jobs as Rust programs with std for the core
(system design 5A0, item 5.0): the kernel's nightly, std from Moruna's copy of Rust's library, the
user linker script, and the gate's one CPU setting. It needs item 5.0's target and std, and says
so without them.

A 0.7 job names the task as an `image` function with `command: ["/gate-JOB"]`, `plane: true` and
`abi: "moruna"` (`bench/vm/gate_jobs.py`): the engine checks the program is of the kernel's user
target and starts it itself, with no Linux personality (stream P). `tasks/image.sh` builds the
gate image: the seven tasks at its root, derived as the node's image is, its environment declaring
`MORUNA_PROCESSES` (the task processes a stage runs; a job entry cannot set a `MORUNA_` name), and
writes the layer stack `moruna run --image-stack` attaches, with its digest in `image.digest`. The
image carries the engine (`/moruna/bin/moruna-engine`), since it replaces the kit's, and its
lowest layer is on `block0`, so the node attaches it alone, without the kit's two image filesystem
fixtures (`layers/layer-1.img` and `layer-2.img`, 106 MB) no job reads.
