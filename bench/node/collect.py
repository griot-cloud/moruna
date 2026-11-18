"""The result collector (work item 6.1): a run's records into 6B's figures, job by job.

Records are one JSON file a run, as measure.py writes them for the Linux engines and run.py for
the node: `spec` (its job, engine, repeat, noise pair), `ok`, `startup_s`, `operational_s`,
`cpu_s`, `peak_bytes`, `oom_kills`, `output_check` (its comparison with 0.6.6's output) and, where
the engine reports one, `split`: the node's from its report (`report_split`: read, decode, the
function, encode, write and waits, 6C), or bench/vm/runner_split.py's, which has read and decode
as one and encode and write as one.

`summarise` takes, per job and engine, the median over the measured repeats (`repeat` 1 or more;
the learning run and the noise pairs left out) of operational time, start-up, CPU-seconds and peak
memory; whether every output matched; and the session's noise, 0.6.6 against itself.
"""
import glob
import json
import os
import statistics

import plan as plans

# The split's stages (6C): the report's six, then runner_split.py's two joined ones.
SPLIT = ("read_s", "decode_s", "function_s", "encode_s", "write_s", "wait_s", "read_decode_s", "encode_write_s")


def report_split(report: dict) -> dict:
    """The time split of a node's report (schema 3.x, docs/run-report.md), in seconds: `read_s`
    and `decode_s` the source tasks' `read_ms` and `decode_ms`, `function_s` the function tasks'
    `busy_ms`, `encode_s` and `write_s` the sink tasks' `encode_ms` and `write_ms`, and `wait_s`
    the function tasks' `input_wait_ms` and `output_wait_ms`, each summed over its tasks. A stage
    the report does not give is absent: a decode done in the read's call has no `decode_ms`."""
    tasks = (report or {}).get("tasks") or []

    def total(role, *fields):
        vals = [t[f] for t in tasks if t.get("role") == role for f in fields if t.get(f) is not None]
        return sum(vals) / 1e3 if vals else None

    out = {"read_s": total("source", "read_ms"), "decode_s": total("source", "decode_ms"),
           "function_s": total("function", "busy_ms"), "encode_s": total("sink", "encode_ms"),
           "write_s": total("sink", "write_ms"), "wait_s": total("function", "input_wait_ms", "output_wait_ms")}
    return {k: v for k, v in out.items() if v is not None}


def load(results_dir: str) -> list:
    """Every run record under `results_dir` (a file with a `spec` holding an `id` and an `engine`),
    one a run: a run's full record replaces its `live` one (run.py's, written as the run finished)."""
    by_id = {}
    for p in sorted(glob.glob(os.path.join(results_dir, "**", "*.json"), recursive=True)):
        try:
            rec = json.load(open(p))
        except ValueError:
            continue
        spec = rec.get("spec") if isinstance(rec, dict) else None
        if not (isinstance(spec, dict) and spec.get("id") and spec.get("engine")):
            continue
        kept = by_id.get(spec["id"])
        if kept is None or (kept.get("live") and not rec.get("live")):
            by_id[spec["id"]] = rec
    return list(by_id.values())


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def output_verdict(runs):
    """One verdict for an engine's outputs: every ok run's comparison with the reference."""
    checks = [(r.get("output_check") or {}) for r in runs if r.get("ok")]
    if not checks:
        return "no run passed"
    bad = [c for c in checks if c.get("ok") is False]
    if bad:
        return f"DIFFERS ({len(bad)} of {len(checks)})"
    if any(c.get("ok") is None for c in checks):
        return "unchecked"
    if all(c.get("hash_all") is True for c in checks):
        return "bit for bit"
    worst = max((c.get("max_abs") or 0.0) for c in checks)
    return f"within tolerance (max {worst:.1e})"


def summarise(records, plan: dict) -> dict:
    jobs = {}
    for job in plan["jobs"]:
        mine = [r for r in records if r["spec"].get("job", r["spec"].get("workload")) == job]
        table = {}
        for engine in plans.engines_for(plan, job):
            runs = [r for r in mine if r["spec"]["engine"] == engine and not r["spec"].get("noise")
                    and (r["spec"].get("repeat") or 0) >= 1]
            ok = [r for r in runs if r.get("ok")]
            splits = [r.get("split") or {} for r in ok]
            data = sorted({r.get("data") for r in runs if r.get("data")})
            table[engine] = {
                "label": plans.ENGINES[engine]["label"], "function": plans.ENGINES[engine]["function"],
                "runs": len(runs), "ok": len(ok),
                # What the engine was given (the PM: each platform's own on the same machine), and
                # how it reached the data: its own Blob reader, or a local copy timed apart.
                "memory_gib": med([r.get("memory_gib") for r in runs]),
                "data": ", ".join(data) or None,
                "copy_s": med([(r.get("copy_in_s") or 0) + (r.get("copy_out_s") or 0) or None for r in ok]),
                "operational_s": med([r.get("operational_s") for r in ok]),
                "startup_s": med([r.get("startup_s") for r in ok]),
                # Moruna's start-up is read from whole seconds: within this many.
                "startup_within_s": max((r.get("startup_within_s") or 0 for r in ok), default=0) or None,
                "cpu_s": med([r.get("cpu_s") for r in ok]),
                "peak_gib": med([(r.get("peak_bytes") or 0) / (1 << 30) or None for r in ok]),
                "oom": sum((r.get("oom_kills") or 0) for r in runs),
                "split": {k: med([s.get(k) for s in splits]) for k in SPLIT},
                "output": output_verdict(runs),
                "errors": sorted({str((r.get("result") or {}).get("error") or r.get("error") or "")[:120]
                                  for r in runs if not r.get("ok")} - {""}),
            }
        noise = [r.get("operational_s") for r in mine if r["spec"].get("noise") and r.get("ok")]
        noise = [x for x in noise if x is not None]
        jobs[job] = {"engines": table,
                     "noise": {"runs": len(noise),
                               "spread": (max(noise) - min(noise)) / statistics.median(noise) if len(noise) >= 2 else None}}
    return {"plan": plan["name"], "jobs": jobs}


def write(results_dir: str, summary: dict) -> str:
    path = os.path.join(results_dir, "summary.json")
    json.dump(summary, open(path, "w"), indent=1)
    return path
