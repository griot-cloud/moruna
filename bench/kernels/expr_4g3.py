"""The 4G.3 measurement (work item 4.2): one fused `moruna.expr` against separate Arrow calls,
and against a Python loop, on a real month, on this Mac.

    expr_4g3.py prepare MONTH.parquet WORK            # the month as Arrow IPC, the two trees
    expr_4g3.py check WORK BIN REFERENCES GATE_CANON  # Audit and Enrich as one expr: 0.6.6's rows?
    expr_4g3.py measure WORK BIN [--rounds N] [--reps N]
    expr_4g3.py one CONFIG WORK [--reps N]            # one Python configuration (used by measure)

The month is decoded once, to `WORK/month.arrow` in batches of 65,536 rows, so every
configuration reads the same batches from memory and only the computing is timed. The
configurations, run in turn round after round (interleaved), each round in its own processes,
the Mac's load (`uptime`) recorded before each:

- `enrich.expr`: Enrich's derived columns and filter mask as one `expr` (BIN, Rust).
- `enrich.arrow-rs`: the same with one arrow-rs call per node (BIN, Rust): workloads.enrich's
  sixteen calls and the int64-to-double cast pyarrow's `divide` makes implicitly.
- `enrich.pyarrow`: workloads.enrich's sixteen pyarrow calls, as the benchmark runs them,
  without the final filter (the `expr` side computes the mask, not the filter, either).
- `audit.expr`: Audit's rule as one `expr` (BIN, Rust).
- `audit.python`: workloads.audit, the Python loop, as the benchmark runs it.

BIN is the `expr_4g3` example of moruna-kernels, built with `--release`.
"""
import json
import os
import subprocess
import sys
import time

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "vm"))
sys.path.insert(0, HERE)
import expr_trees  # noqa: E402
import workloads  # noqa: E402

BATCH_ROWS = 65_536


def read_ipc(path):
    with pa.OSFile(path) as f:
        return pa.ipc.open_file(f).read_all().to_batches()


def prepare(month, work):
    os.makedirs(work, exist_ok=True)
    table = pq.read_table(month)
    batches = table.to_batches(max_chunksize=BATCH_ROWS)
    with pa.OSFile(os.path.join(work, "month.arrow"), "wb") as sink:
        with pa.ipc.new_file(sink, table.schema) as w:
            for b in batches:
                w.write_batch(b)
    expr_trees.write(work)
    print(json.dumps({"rows": table.num_rows, "batches": len(batches), "work": work}))


def enrich_pyarrow(batch):
    """workloads.enrich's sixteen calls, without its final filter."""
    dur = pc.divide(pc.cast(pc.subtract(batch["dropoff_at"], batch["pickup_at"]), pa.int64()), 1_000_000.0)
    dist, fare, tip = batch["trip_distance"], batch["fare_amount"], batch["tip_amount"]
    ok = pc.and_(pc.and_(pc.greater_equal(dur, 60.0), pc.less_equal(dur, 4 * 3600.0)),
                 pc.and_(pc.and_(pc.greater(dist, 0.0), pc.less(dist, 100.0)), pc.greater(fare, 0.0)))
    out = batch.append_column("duration_s", dur)
    out = out.append_column("mph", pc.divide(dist, pc.divide(dur, 3600.0)))
    out = out.append_column("fare_per_mile", pc.divide(fare, dist))
    out = out.append_column("tip_share", pc.divide(tip, fare))
    return out.append_column("keep", ok)


def one(config, work, reps):
    batches = read_ipc(os.path.join(work, "month.arrow"))
    f = {"enrich.pyarrow": enrich_pyarrow, "audit.python": workloads.audit}[config]
    for b in batches:  # warm-up
        f(b)
    times = []
    for _ in range(reps):
        t = time.perf_counter()
        for b in batches:
            f(b)
        times.append(time.perf_counter() - t)
    print(json.dumps({"mode": config, "rows": sum(b.num_rows for b in batches),
                      "batches": len(batches), "times_s": times}))


def rust(bin_path, config, work, reps, out=None):
    job = config.split(".")[0]
    if config.endswith(".expr"):
        cmd = [bin_path, "expr", os.path.join(work, f"{job}.json"), os.path.join(work, "month.arrow")]
    else:
        cmd = [bin_path, "arrow-enrich", os.path.join(work, "month.arrow")]
    cmd += ["--reps", str(reps)] + (["--out", out] if out else [])
    p = subprocess.run(cmd, capture_output=True, text=True, check=True)
    return json.loads(p.stdout.strip().splitlines()[-1])


def python(config, work, reps):
    p = subprocess.run([sys.executable, "-I", os.path.abspath(__file__), "one", config, work,
                        "--reps", str(reps)], capture_output=True, text=True, check=True)
    return json.loads(p.stdout.strip().splitlines()[-1])


def uptime():
    return subprocess.run(["uptime"], capture_output=True, text=True).stdout.strip()


def measure(work, bin_path, rounds, reps):
    configs = ["enrich.expr", "enrich.arrow-rs", "enrich.pyarrow", "audit.expr", "audit.python"]
    records = []
    for r in range(rounds):
        for config in configs:
            load = uptime()
            if config.endswith(".pyarrow") or config.endswith(".python"):
                rec = python(config, work, reps)
            else:
                rec = rust(bin_path, config, work, reps)
            rec.update(config=config, round=r, uptime_before=load)
            records.append(rec)
            print(json.dumps(rec), flush=True)
    with open(os.path.join(work, "measure.json"), "w") as f:
        json.dump(records, f, indent=1)
    summary = {}
    for config in configs:
        ts = sorted(t for rec in records if rec["config"] == config for t in rec["times_s"])
        summary[config] = {"runs": len(ts), "min_s": ts[0], "median_s": ts[len(ts) // 2], "max_s": ts[-1]}
    print(json.dumps({"summary": summary}, indent=1))


def check(work, bin_path, references, gate_canon):
    """Audit's and Enrich's trees on the month, to Parquet, through gate-canon, against 0.6.6."""
    results = {}
    for job in ("enrich", "audit"):
        out_ipc = os.path.join(work, f"{job}.out.arrow")
        rust(bin_path, f"{job}.expr", work, 1, out=out_ipc)
        out_dir = os.path.join(work, f"{job}-out")
        os.makedirs(out_dir, exist_ok=True)
        for name in os.listdir(out_dir):
            os.remove(os.path.join(out_dir, name))
        pq.write_table(pa.Table.from_batches(read_ipc(out_ipc)), os.path.join(out_dir, "part-00000.parquet"))
        p = subprocess.run([gate_canon, "digest", job, out_dir], capture_output=True, text=True, check=True)
        mine = json.loads(p.stdout)
        with open(os.path.join(references, f"reference-{job}-2024-01.json")) as f:
            ref = json.load(f)
        results[job] = {"rows": mine["rows"], "reference_rows": ref["rows"],
                        "hash_exact": mine["hash_exact"], "reference_hash_exact": ref["hash_exact"],
                        "ok": mine["rows"] == ref["rows"] and mine["hash_exact"] == ref["hash_exact"]}
    print(json.dumps(results, indent=1))
    return all(r["ok"] for r in results.values())


def opt(name, default):
    return int(sys.argv[sys.argv.index(name) + 1]) if name in sys.argv else default


if __name__ == "__main__":
    cmd = sys.argv[1]
    if cmd == "prepare":
        prepare(sys.argv[2], sys.argv[3])
    elif cmd == "one":
        one(sys.argv[2], sys.argv[3], opt("--reps", 3))
    elif cmd == "measure":
        measure(sys.argv[2], sys.argv[3], opt("--rounds", 5), opt("--reps", 3))
    elif cmd == "check":
        sys.exit(0 if check(sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]) else 1)
    else:
        sys.exit(__doc__)
