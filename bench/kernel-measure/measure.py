"""4G.2: Occupancy through `moruna.interval_coverage_count` against expand-and-count, on real
months.

    measure.py --bin target/release/occupancy-coverage --work DIR
               [--data ~/moruna-data/nyc/norm] [--references ~/moruna-data/nyc]
               [--gate-canon ~/moruna-data/bin/gate-canon]
               [--plan 2024-01:1000000,2024-01:125000,2024:1000000] [--rounds 3]
               [--ways python,rust-expand,kernel,kernel-dense,read-rust,read-python] [--no-check]

For each period and batch size of the plan, five ways of computing Occupancy on the same batches,
each in its own process:

- `python`: Occupancy as written (bench/vm/workloads.py: expand each trip to one row per minute,
  then `np.unique`), by occupancy_py.py;
- `rust-expand`: the gate's Rust Occupancy (expand and count, `moruna_gate_functions::batch::
  occupancy`, the function the benchmark registers);
- `kernel`: the kernel's inputs (Occupancy's minutes, the `expr` of 4.2) and then the kernel with no
  group domain, its sparse table;
- `kernel-dense`: the same with the domain [0, 299], its dense table;
- `read` (Rust and Python): reading the batches alone, the baseline of each language's memory.

First a check round writes every way's output and compares them: each must equal Occupancy as
written, batch by batch in the same order, and its gate-canon digest must be 0.6.6's reference
(`reference-occupancy-PERIOD.json`: rows and both hashes, bit for bit). Then `--rounds` timed
rounds run every way once each, in a rotated order (interleaved, so the Mac's load falls on all of
them alike), with no output written. Each run records the seconds in the function, the process's
seconds, its maximum resident set and peak memory footprint (`/usr/bin/time -l`), and `uptime`'s
load averages before it. The results go to WORK/results.json and a table is printed: the medians
of the timed rounds. `--ways` and `--no-check` give more timed rounds of some ways alone.
"""
import argparse
import json
import os
import re
import statistics
import subprocess
import sys

import pyarrow.parquet as pq

HERE = os.path.dirname(os.path.abspath(__file__))
WAYS = ["python", "rust-expand", "kernel", "kernel-dense"]
BASELINES = ["read-rust", "read-python"]


def load():
    out = subprocess.run(["uptime"], capture_output=True, text=True, check=True).stdout
    m = re.search(r"load averages?: ([\d.]+),? ([\d.]+),? ([\d.]+)", out)
    return [float(x) for x in m.groups()] if m else None


def command(a, way, batch, files, out):
    if way in ("python", "read-python"):
        cmd = [sys.executable, "-I", os.path.join(HERE, "occupancy_py.py"),
               "python" if way == "python" else "read"]
    else:
        cmd = [a.bin, "read" if way == "read-rust" else way]
    cmd += ["--batch", str(batch)]
    if out:
        cmd += ["--out", out]
    return cmd + files


def run(a, way, batch, files, out=None):
    before = load()
    p = subprocess.run(["/usr/bin/time", "-l"] + command(a, way, batch, files, out),
                       capture_output=True, text=True, check=False)
    if p.returncode != 0:
        raise SystemExit(f"{way} failed ({p.returncode}): {p.stderr[-2000:]}")
    rec = json.loads(p.stdout.strip().splitlines()[-1])
    for key, label in (("max_rss_bytes", "maximum resident set size"),
                       ("peak_footprint_bytes", "peak memory footprint")):
        m = re.search(r"(\d+)\s+" + label, p.stderr)
        rec[key] = int(m.group(1)) if m else None
    rec["way"] = way
    rec["load_before"] = before
    rec["notes"] = [ln for ln in p.stderr.splitlines() if "moruna." in ln]
    return rec


def digest(a, path):
    p = subprocess.run([a.gate_canon, "digest", "occupancy", path], capture_output=True, text=True,
                       check=False)
    if p.returncode != 0:
        return {"error": p.stderr.strip()[-500:]}
    return json.loads(p.stdout)


def check(a, period, batch, files):
    """The check round: every way's output, equal to Occupancy as written and to 0.6.6's rows."""
    ref = json.load(open(os.path.join(a.references, f"reference-occupancy-{period}.json")))
    outs, recs = {}, {}
    for way in WAYS:
        outs[way] = os.path.join(a.work, f"occupancy-{period}-{batch}-{way}.parquet")
        recs[way] = run(a, way, batch, files, outs[way])
    want = pq.read_table(outs["python"])
    result = {}
    for way in WAYS:
        got = pq.read_table(outs[way])
        d = digest(a, outs[way])
        result[way] = {
            "rows_out": recs[way]["rows_out"],
            "same_batches": recs[way]["batch_rows"] == recs["python"]["batch_rows"],
            "equal_in_order": got.equals(want),
            "canon": d,
            "reference": {k: d.get(k) == ref[k] for k in ("rows", "hash_exact", "hash_all")},
            "notes": recs[way]["notes"],
        }
        result[way]["bit_for_bit"] = all(result[way]["reference"].values())
    for way in WAYS:
        os.remove(outs[way])
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--bin", required=True)
    p.add_argument("--work", required=True)
    p.add_argument("--data", default=os.path.expanduser("~/moruna-data/nyc/norm"))
    p.add_argument("--references", default=os.path.expanduser("~/moruna-data/nyc"))
    p.add_argument("--gate-canon", default=os.path.expanduser("~/moruna-data/bin/gate-canon"))
    p.add_argument("--plan", default="2024-01:1000000,2024-01:125000,2024:1000000")
    p.add_argument("--rounds", type=int, default=3)
    p.add_argument("--ways", default=",".join(WAYS + BASELINES),
                   help="the ways the timed rounds run, of " + ", ".join(WAYS + BASELINES))
    p.add_argument("--no-check", action="store_true",
                   help="no check round: timed rounds alone, for more samples")
    a = p.parse_args()
    os.makedirs(a.work, exist_ok=True)
    ways = a.ways.split(",")
    unknown = sorted(set(ways) - set(WAYS + BASELINES))
    if unknown:
        raise SystemExit(f"unknown ways {unknown}")
    results = {"plan": a.plan, "rounds": a.rounds, "ways": ways, "load_start": load(),
               "runs": []}
    for item in a.plan.split(","):
        period, batch = item.split(":")
        batch = int(batch)
        months = [period] if "-" in period else [f"{period}-{m:02d}" for m in range(1, 13)]
        files = [os.path.join(a.data, f"yellow_tripdata_{m}.parquet") for m in months]
        print(f"{period} at {batch} rows" + ("" if a.no_check else ": check round"), flush=True)
        entry = {"period": period, "batch": batch,
                 "check": {} if a.no_check else check(a, period, batch, files), "timed": []}
        for r in range(a.rounds):
            order = ways[r % len(ways):] + ways[:r % len(ways)]
            for way in order:
                rec = run(a, way, batch, files)
                rec["round"] = r
                rec.pop("batch_rows", None)
                entry["timed"].append(rec)
                print(f"  round {r} {way:13s} compute {rec['compute_s']:8.3f} s  "
                      f"rss {rec['max_rss_bytes'] / 2**20:8.1f} MiB  load {rec['load_before']}",
                      flush=True)
        results["runs"].append(entry)
        with open(os.path.join(a.work, "results.json"), "w") as f:
            json.dump(results, f, indent=1)
    results["load_end"] = load()
    with open(os.path.join(a.work, "results.json"), "w") as f:
        json.dump(results, f, indent=1)
    table(results)


def table(results):
    print()
    print(f"{'period':8s} {'batch':>8s} {'way':13s} {'compute s':>10s} {'process s':>10s} "
          f"{'max RSS MiB':>12s} {'alloc peak MiB':>15s} {'canon':>6s} {'= as written':>12s}")
    for e in results["runs"]:
        for way in WAYS + BASELINES:
            recs = [r for r in e["timed"] if r["way"] == way]
            if not recs:
                continue

            def med(k, recs=recs):
                return statistics.median(r[k] for r in recs if r.get(k) is not None)

            alloc = [r["alloc_peak_bytes"] for r in recs if "alloc_peak_bytes" in r]
            c = e["check"].get(way, {})
            print(f"{e['period']:8s} {e['batch']:8d} {way:13s} {med('compute_s'):10.3f} "
                  f"{med('process_s'):10.3f} {med('max_rss_bytes') / 2**20:12.1f} "
                  f"{(max(alloc) / 2**20 if alloc else float('nan')):15.1f} "
                  f"{str(c.get('bit_for_bit', '')):>6s} {str(c.get('equal_in_order', '')):>12s}")


if __name__ == "__main__":
    main()
