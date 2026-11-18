"""The node benchmark's plans (system design 6B): what runs, where, and in what order.

A plan is a JSON file (`plans/nyc-2024.json`). `load` checks it and refuses an unknown field;
`runs` expands it into the ordered list of runs the harness makes, one at a time:

- for each job, a learning run of every engine (`r0`, left out of the medians), 0.6.6 with the
  Python function first, since its output is the reference every other run is checked against;
- then `repeats` measured runs of every engine, the order reversed on every other repeat, so no
  engine always runs after the same one (bench/vm/gate_plan.py's interleaving);
- then `noise_pairs` pairs of 0.6.6 against itself, for the session's noise.

0.6.6 keeps a profile store across a job's runs, emptied before its learning run, and stages to
disk, as gate_plan.py's runs did. Every engine has the machine's CPUs. Memory is what each
platform leaves on the same machine (the PM, 2026-11-18): the plan's `machine.mem_gib` on Linux,
the node's job bound on Moruna (`job_memory_bytes`, 3A: the most a node takes; run.py reads it
from the node).

**Data** (`linux_data`): `blob`, every engine reads its inputs (`inputs`, blob names under
`nyc/`) from and writes its output (`output`, under `out/<run id>`) to the run's one container,
with its own Blob reader (store.py); `local` (the Mac's check with no emulator) reads local files.
The dataset's `source_url`, `sha256` and `pyarrow` let the Linux machine make the container's
copies itself (work item 6.2b): each file fetched from `source_url` (`{file}` its name),
normalised by bench/vm/normalize.py with that pyarrow, and checked against its SHA-256, the Mac's
copy's in `source_dir`.

The native-expression rows run only the jobs that have a native form (bench/vm/native_exprs.py):
DuckDB SQL for Enrich and Occupancy, Polars expressions for Enrich.
"""
import json
import os

JOBS = ("score", "occupancy", "enrich", "audit")
# Every engine of 6A and the native-expression rows: where it runs, the language of its
# function, its row's name, and the jobs it has a form of (every job when not named).
ENGINES = {
    "moruna-node": {"where": "node", "function": "rust", "label": "Moruna 0.7, the node"},
    "moruna066": {"where": "linux", "function": "python", "label": "0.6.6, Python function"},
    "moruna066-rust": {"where": "linux", "function": "rust", "label": "0.6.6, Rust function"},
    "rust-pool": {"where": "linux", "function": "rust", "label": "the plain Rust pool"},
    "duckdb": {"where": "linux", "function": "python", "label": "DuckDB"},
    "duckdb-sql": {"where": "linux", "function": "sql", "label": "DuckDB SQL", "jobs": ("enrich", "occupancy")},
    "polars": {"where": "linux", "function": "python", "label": "Polars"},
    "polars-expr": {"where": "linux", "function": "expressions", "label": "Polars expressions", "jobs": ("enrich",)},
    "ray": {"where": "linux", "function": "python", "label": "Ray"},
    "spark": {"where": "linux", "function": "python", "label": "Spark"},
    "dask": {"where": "linux", "function": "python", "label": "Dask"},
}
REFERENCE = "moruna066"
FIELDS = {"plan", "name", "dataset", "jobs", "engines", "repeats", "learning_run", "noise_pairs", "machine",
          "linux_data", "timeout_s", "notes"}
DATASET = {"name", "source_dir", "files", "rows", "source_url", "sha256", "pyarrow"}
# The data made in Azure (run.py, engines/dataprep.py) needs all three: where each file is fetched
# from, the pyarrow that normalises it as the Mac's copies were, and each copy's SHA-256.
MADE_IN_AZURE = ("source_url", "sha256", "pyarrow")
MACHINE = {"size", "zone", "cpus", "mem_gib", "image", "os_disk_gib"}
LINUX_DATA = ("blob", "local")


def engines_for(plan: dict, job: str) -> list:
    """The plan's engines that have a form of `job`, in the plan's order."""
    return [e for e in plan["engines"] if job in ENGINES[e].get("jobs", JOBS)]


class PlanError(ValueError):
    pass


def _check_keys(where, value, allowed, required=()):
    if not isinstance(value, dict):
        raise PlanError(f"{where}: an object, not {type(value).__name__}")
    unknown = sorted(set(value) - allowed)
    if unknown:
        raise PlanError(f"{where}: unknown field {', '.join(unknown)}")
    missing = [k for k in required if k not in value]
    if missing:
        raise PlanError(f"{where}: missing {', '.join(missing)}")


def check(plan: dict) -> dict:
    """`plan` when it is a valid plan, version 1; otherwise a PlanError naming what is wrong."""
    _check_keys("plan", plan, FIELDS, ("plan", "name", "dataset", "jobs", "engines", "repeats", "machine",
                                       "linux_data"))
    if plan["plan"] != 1:
        raise PlanError(f"plan: version {plan['plan']!r}; this harness reads version 1")
    _check_keys("dataset", plan["dataset"], DATASET, ("name", "source_dir", "files"))
    files = plan["dataset"]["files"]
    if not files or not all(isinstance(f, str) and f.endswith(".parquet") and "/" not in f for f in files):
        raise PlanError("dataset.files: one or more Parquet file names, without a directory")
    if len(set(files)) != len(files):
        raise PlanError("dataset.files: a file is named twice")
    d = plan["dataset"]
    if any(k in d for k in MADE_IN_AZURE):
        if not all(k in d for k in MADE_IN_AZURE):
            raise PlanError(f"dataset: {', '.join(MADE_IN_AZURE)} go together")
        if not isinstance(d["source_url"], str) or "{file}" not in d["source_url"] \
                or not d["source_url"].startswith("https://"):
            raise PlanError("dataset.source_url: an https address with `{file}` where each file's name goes")
        digests = d["sha256"]
        if not isinstance(digests, dict) or set(digests) != set(files) or not all(
                isinstance(v, str) and len(v) == 64 and all(ch in "0123456789abcdef" for ch in v)
                for v in digests.values()):
            raise PlanError("dataset.sha256: each file's SHA-256, 64 lowercase hex digits, for every file and no other")
        if not isinstance(d["pyarrow"], str) or not d["pyarrow"]:
            raise PlanError("dataset.pyarrow: the version that wrote the copies, such as 24.0.0")
    for key, known in (("jobs", JOBS), ("engines", tuple(ENGINES))):
        vals = plan[key]
        if not vals or not isinstance(vals, list):
            raise PlanError(f"{key}: a non-empty list")
        bad = [v for v in vals if v not in known]
        if bad:
            raise PlanError(f"{key}: unknown {', '.join(map(str, bad))}; known: {', '.join(known)}")
        if len(set(vals)) != len(vals):
            raise PlanError(f"{key}: a name is given twice")
    if REFERENCE not in plan["engines"]:
        raise PlanError(f"engines: {REFERENCE} is the reference every output is checked against (6B)")
    if not isinstance(plan["repeats"], int) or plan["repeats"] < 1:
        raise PlanError("repeats: a whole number, 1 or more")
    if not isinstance(plan.get("noise_pairs", 0), int) or plan.get("noise_pairs", 0) < 0:
        raise PlanError("noise_pairs: a whole number, 0 or more")
    _check_keys("machine", plan["machine"], MACHINE, ("size", "zone", "cpus", "mem_gib"))
    m = plan["machine"]
    if not isinstance(m["cpus"], int) or m["cpus"] < 1 or not isinstance(m["mem_gib"], (int, float)) or m["mem_gib"] <= 0:
        raise PlanError("machine: cpus a whole number and mem_gib a positive number")
    if plan["linux_data"] not in LINUX_DATA:
        raise PlanError(f"linux_data: one of {', '.join(LINUX_DATA)}")
    return plan


def load(path: str) -> dict:
    try:
        plan = json.load(open(path))
    except ValueError as e:
        raise PlanError(f"{path}: not JSON: {e}") from None
    return check(plan)


def _order(engines):
    """The learning run's order: the reference first."""
    return [REFERENCE] + [e for e in engines if e != REFERENCE]


def runs(plan: dict, root: str = "/data") -> list:
    """Every run of `plan`, in the order they are made. Linux paths are under `root`; blob names
    are under the run's container (store.py)."""
    m = plan["machine"]
    files = [f"{root}/nyc/{f}" for f in plan["dataset"]["files"]]
    inputs = [f"nyc/{f}" for f in plan["dataset"]["files"]]
    out = []

    def add(job, engine, repeat, noise=None):
        rid = f"{job}-{engine}-r{repeat}" if noise is None else f"{job}-noise-{noise}"
        spec = {"id": rid, "job": job, "workload": job, "engine": engine, "where": ENGINES[engine]["where"],
                "repeat": repeat, "cpus": m["cpus"], "mem_gib": m["mem_gib"], "files": files,
                "inputs": inputs, "output": f"out/{rid}", "data": plan["linux_data"],
                "timeout_s": plan.get("timeout_s", 1800)}
        if noise is not None:
            spec["noise"] = noise
        if engine in ("moruna066", "moruna066-rust"):
            spec["profiles"] = f"{root}/profiles/{job}-{engine}"
            spec["staging"] = f"{root}/staging"
            if repeat == 0:
                spec["fresh_profiles"] = True
        out.append(spec)

    for job in plan["jobs"]:
        engines = engines_for(plan, job)
        if plan.get("learning_run", True):
            for e in _order(engines):
                add(job, e, 0)
        for r in range(1, plan["repeats"] + 1):
            for e in (engines if r % 2 else engines[::-1]):
                add(job, e, r)
        for p in range(1, plan.get("noise_pairs", 0) + 1):
            for side in ("a", "b"):
                add(job, REFERENCE, None, noise=f"p{p}{side}")
    return out


def default_plan_path(name: str) -> str:
    return os.path.join(os.path.dirname(os.path.abspath(__file__)), "plans", f"{name}.json")
