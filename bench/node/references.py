"""0.6.6's reference outputs for the NYC jobs: what every engine's output is checked against.

    references.py --data DIR --period 2024-01|2024 --jobs score,occupancy,enrich,audit \
                  --gate-canon BIN --work DIR [--keep-output DIR] [--cpus 4]

Run with 0.6.6's interpreter (its wheel and pyarrow installed). For each job, 0.6.6 runs the
Python function (bench/vm/workloads.py) over DIR/norm's files of the period, as January's Score
reference was made, and its output is reduced to its canonical form (gate-canon/1):

- **A month:** by bench/vm/canon.py, with its tolerant rows, and by `gate-canon`; the two must
  give the same rows and hashes, or the script stops.
- **The year:** by `gate-canon` alone (canon.py holds a run in Python objects, which the year
  does not fit); a job with a tolerant column keeps its tolerant rows in a gate-canon dump
  beside the JSON, `reference-JOB-PERIOD.dump`.

Writes DIR/reference-JOB-PERIOD.json, with the output's schema, 0.6.6's version and the run's
settings. A reference already there is never rewritten: the new one goes to the work directory
and the two are compared. With `--keep-output`, the outputs are kept there (the January ones
are what test_node_harness.py checks canon.py and gate-canon against).
"""
import argparse
import glob
import json
import os
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "vm"))
import canon  # noqa: E402
import workloads  # noqa: E402

DIGEST = ("rows", "hash_exact", "hash_all")


def files_of(data, period):
    files = sorted(glob.glob(os.path.join(data, "norm", f"yellow_tripdata_{period}*.parquet")))
    if not files:
        raise SystemExit(f"references.py: no normalised files for {period} under {data}/norm")
    return files


def gate_digest(tool, job, out, dump=None):
    p = subprocess.run([tool, "digest", job] + (["--dump", dump] if dump else []) + [out],
                       capture_output=True, text=True)
    if p.returncode != 0:
        raise RuntimeError(f"gate-canon: {p.stderr.strip()[-300:]}")
    return json.loads(p.stdout)


def one(job, a, files):
    import moruna
    out = os.path.join(a.work, f"{job}-{a.period}")
    shutil.rmtree(out, ignore_errors=True)
    t0 = time.time()
    report = moruna.run(moruna.ParquetSource(files), [moruna.function(getattr(workloads, job))],
                        moruna.ParquetSink(out), cpu=a.cpus, staging_dir=os.path.join(a.work, "staging"))
    seconds = round(time.time() - t0, 1)
    tolerant = bool(canon.SPECS[job].get("tolerant"))
    month = len(files) == 1
    name = f"reference-{job}-{a.period}"
    target = os.path.join(a.data, f"{name}.json")
    fresh = not os.path.exists(target)
    where = a.data if fresh else a.work
    dump = os.path.join(where, f"{name}.dump") if tolerant and not month else None
    g = gate_digest(a.gate_canon, job, out, dump)
    import pyarrow.parquet as pq
    schema = pq.read_schema(sorted(glob.glob(os.path.join(out, "**", "*.parquet"), recursive=True))[0])
    ref = {"rows": g["rows"], "hash_exact": g["hash_exact"], "hash_all": g["hash_all"], "exact": g["exact"],
           "tolerant": g["tolerant"]}
    if month:
        c = canon.canonical(job, canon.read_output([out]))
        if any(c[k] != g[k] for k in DIGEST):
            raise SystemExit(f"references.py: canon.py and gate-canon disagree on {job} {a.period}: "
                             f"{ {k: c[k] for k in DIGEST} } against { {k: g[k] for k in DIGEST} }")
        ref = c
        ref["canon"] = "canon.py and gate-canon agree"
    else:
        ref["canon"] = "gate-canon"
        if dump:
            ref["dump"] = os.path.basename(dump)
    ref.update({"job": job, "period": a.period, "inputs": [os.path.basename(f) for f in files],
                "schema": [f"{f.name}:{f.type}" for f in schema],
                "made_by": {"engine": "moruna 0.6.6", "version": moruna.__version__, "function": "python",
                            "python": sys.version.split()[0], "gil": getattr(sys, "_is_gil_enabled", lambda: True)(),
                            "settings": {"cpu": a.cpus, "staging_dir": "the work directory"},
                            "exit": report.to_dict().get("exit"), "seconds": seconds}})
    path = os.path.join(where, f"{name}.json")
    json.dump(ref, open(path, "w"))
    summary = {"job": job, "period": a.period, **{k: ref[k] for k in DIGEST}, "seconds": seconds,
               "written": path if fresh else None}
    if not fresh:
        old = json.load(open(target))
        summary["existing"] = target
        summary["same_as_existing"] = all(old[k] == ref[k] for k in DIGEST)
        if tolerant and month:
            summary["tolerant_within"] = canon.compare(job, old, ref)["ok"]
    if a.keep_output:
        dest = os.path.join(a.keep_output, f"{job}-{a.period}")
        shutil.rmtree(dest, ignore_errors=True)
        shutil.move(out, dest)
    else:
        shutil.rmtree(out, ignore_errors=True)
    return summary


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--data", required=True, help="the data directory: its norm/ holds the inputs")
    p.add_argument("--period", required=True, help="2024-01, or 2024 for the year")
    p.add_argument("--jobs", default="score,occupancy,enrich,audit")
    p.add_argument("--gate-canon", required=True)
    p.add_argument("--work", required=True)
    p.add_argument("--keep-output", default="")
    p.add_argument("--cpus", type=float, default=4)
    a = p.parse_args(argv)
    a.data, a.work = os.path.expanduser(a.data), os.path.abspath(a.work)
    os.makedirs(a.work, exist_ok=True)
    files = files_of(a.data, a.period)
    for job in a.jobs.split(","):
        print(json.dumps(one(job, a, files)), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
