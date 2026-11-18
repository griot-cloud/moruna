"""Measures one run on the benchmark's Linux machine (system design 6B), and checks its output.

    measure.py run --spec SPEC.json --root ENGINES --results DIR --work DIR [REFERENCE]
    measure.py check --workload W --out DIR --id ID --root ENGINES --results DIR [--blob-prefix P] [REFERENCE]

REFERENCE is `--make-reference F` (this run's output is the reference: its gate-canon dump is
written to F), `--reference-dump F` (compared with that dump by `gate-canon compare`), or
`--reference-json F` (compared with a canon.py JSON that holds its tolerant rows, as 0.6.6's
Score reference for the Mac's check does).

`run` starts the engine (engine.py in its environment, or `gate-baseline` for the Rust pool) and
measures it as bench/vm/driver.py does:

- **Linux, as root:** in a cgroup of its own under /sys/fs/cgroup/bench, with `cpuset.cpus` and
  `cpu.max` of the spec's CPUs, `memory.max` of its memory and no swap. CPU-seconds are the
  cgroup's `usage_usec`, peak memory its `memory.peak`; every process of the run is killed with
  the cgroup when it ends, so a runtime's daemons don't outlive their run.
- **Elsewhere (the Mac's check):** CPU-seconds are the children's rusage, user and system; peak
  memory is the largest one process reached (`ru_maxrss`), not the run's total, and is marked so.

Operational time runs from the job's being handed to the ready engine (engine.py's `start`) to
the output durably written (`done`); start-up is everything before `start`: the process, the
imports, the session or cluster. Every mark is kept. For the Rust pool, operational time is its
own `wall_s` and start-up the rest of the process's life before it.

**Data.** With a store in the environment (store.py), the spec's `inputs` are blob names and its
`output` a blob prefix, and the engine reads and writes them itself. An engine with no Blob
reader (`LOCAL_COPY`: the Rust pool) gets a local copy, timed apart: its inputs are downloaded
before it starts (`copy_in_s`) and its output uploaded after it ends (`copy_out_s`), and its
record says `data: local copy`. Without a store, everything is local. Before a run starts, the empty
directory markers an earlier run's driver left above its output (Spark's ABFS) are deleted
(store.drop_markers), outside every timing.

**A failed run's own error** (work item 6.2c): the last lines of the engine's stderr (`error_lines`,
else the error its RESULT gave) are in its record, in its summary line, which the batch keeps in
its state (runs.json), and on this process's own stderr, which the batch keeps too.

The output is reduced to its canonical form (gate-canon/1: `gate-canon` when it is built, else
bench/vm/canon.py), after it is fetched from Blob outside every timing, and compared with the
reference's: a dump `gate-canon` wrote, or a canon.py JSON with its tolerant rows. `check` does
the same for an output another machine wrote (the Moruna node's) under `--blob-prefix`. Each
writes RESULTS/ID.json and prints it on one line after `MEASURED `.
"""
import argparse
import json
import os
import resource
import shutil
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
VM = os.environ.get("BENCH_VM", os.path.abspath(os.path.join(HERE, "..", "..", "vm")))
sys.path.insert(0, VM)
import store as stores  # noqa: E402, store.py beside this file

CG = "/sys/fs/cgroup/bench"
GiB = 1 << 30
VENV = {"moruna066": "moruna066", "moruna066-rust": "moruna066", "ray": "ray", "spark": "spark",
        "dask": "dask", "duckdb": "duckdb", "duckdb-sql": "duckdb", "polars": "polars", "polars-expr": "polars"}
PYTHON_ENGINES = tuple(VENV)
# Engines with no Blob reader of their own: a local copy, timed apart (the PM, 2026-11-18).
LOCAL_COPY = ("rust-pool",)
# A failed run's own error, as the summary line carries it to the batch's state and the table:
# the last lines of the engine's stderr, at most this many and this many characters.
ERROR_LINES, ERROR_BYTES = 12, 1500


def wr(path, value):
    with open(path, "w") as f:
        f.write(str(value))


def rd(path):
    try:
        return open(path).read()
    except OSError:
        return ""


def command(spec, root, out, flist, tmp, report):
    """The run's command line: engine.py in the engine's environment, or the Rust pool's binary.
    `out` is a local directory, or a blob prefix when the engine writes Blob itself."""
    e = spec["engine"]
    if e == "rust-pool":
        return [os.path.join(root, "bin", "gate-baseline"), "--job", spec["workload"], "--files", flist,
                "--out", out, "--threads", str(spec["cpus"]), "--batch", str(spec.get("batch", 0))]
    if e not in PYTHON_ENGINES:
        raise ValueError(f"measure.py: no engine {e!r}")
    cmd = [os.path.join(root, f"venv-{VENV[e]}", "bin", "python"), "-I", os.path.join(HERE, "engine.py"),
           "--engine", e, "--workload", spec["workload"], "--inputs", flist, "--out", out,
           "--cpus", str(spec["cpus"]), "--mem-gib", str(spec["mem_gib"]), "--tmp", tmp, "--root", root,
           "--batch", str(spec.get("batch", 0)), "--report", report]
    if spec.get("staging"):
        cmd += ["--staging", spec["staging"]]
    if spec.get("profiles"):
        cmd += ["--profiles", spec["profiles"]]
    return cmd


def environment(spec, tmp):
    """The run's environment, bench/vm/driver.py's: one BLAS thread per worker, as every engine
    there ran; the engine's threads are the spec's CPUs."""
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": os.environ.get("HOME", "/root"),
           "OPENBLAS_NUM_THREADS": "1", "OMP_NUM_THREADS": "1", "MKL_NUM_THREADS": "1",
           "VECLIB_MAXIMUM_THREADS": "1", "POLARS_MAX_THREADS": str(spec["cpus"]),
           "PYTHONPATH": VM, "TMPDIR": tmp}
    for k in ("JAVA_HOME", "BENCH_ACCOUNT", "BENCH_KEY", "BENCH_CONTAINER", "BENCH_ENDPOINT"):
        if os.environ.get(k):
            env[k] = os.environ[k]
    env.update(spec.get("env", {}))
    return env


class Cgroup:
    """A run's cgroup (Linux, root): limits, samples every 0.5 s, and its totals."""

    def __init__(self, rid, cpus, mem_gib):
        self.path = f"{CG}/{rid}"
        wr("/sys/fs/cgroup/cgroup.subtree_control", "+cpuset +cpu +memory")
        os.makedirs(CG, exist_ok=True)
        wr(f"{CG}/cgroup.subtree_control", "+cpuset +cpu +memory")
        if os.path.isdir(self.path):
            os.rmdir(self.path)
        os.makedirs(self.path)
        wr(f"{self.path}/cpuset.cpus", f"0-{cpus - 1}")
        wr(f"{self.path}/cpu.max", f"{cpus * 100000} 100000")
        wr(f"{self.path}/memory.max", int(mem_gib * GiB))
        wr(f"{self.path}/memory.swap.max", 0)
        self.samples, self._stop = [], threading.Event()

    def enter(self):
        wr(f"{self.path}/cgroup.procs", os.getpid())

    def usage_s(self):
        return next((int(line.split()[1]) for line in rd(f"{self.path}/cpu.stat").splitlines()
                     if line.startswith("usage_usec")), 0) / 1e6

    def sample(self, t0):
        last_u, last_t = None, None
        while not self._stop.is_set():
            now, u = time.time() - t0, self.usage_s()
            if last_u is not None and now > last_t:
                self.samples.append({"t": round(now, 2), "cores": round((u - last_u) / (now - last_t), 2),
                                     "mem": int(rd(f"{self.path}/memory.current").strip() or 0)})
            last_u, last_t = u, now
            self._stop.wait(0.5)

    def finish(self):
        self._stop.set()
        wr(f"{self.path}/cgroup.kill", 1)
        time.sleep(1)
        events = dict(line.split() for line in rd(f"{self.path}/memory.events").splitlines())
        totals = {"cpu_s": round(self.usage_s(), 2), "peak_bytes": int(rd(f"{self.path}/memory.peak").strip() or 0),
                  "peak_is": "the cgroup's memory.peak", "oom_kills": int(events.get("oom_kill", 0))}
        try:
            os.rmdir(self.path)
        except OSError:
            pass
        return totals


def execute(spec, cmd, env, timeout):
    """Runs `cmd`, measured; returns (exit code, stdout, stderr's tail, spawn time, end time, totals)."""
    linux = sys.platform.startswith("linux") and os.geteuid() == 0
    cg = Cgroup(spec["id"], spec["cpus"], spec["mem_gib"]) if linux else None
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    t0 = time.time()
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, text=True,
                            preexec_fn=cg.enter if cg else None)
    sampler = threading.Thread(target=cg.sample, args=(t0,)) if cg else None
    if sampler:
        sampler.start()
    try:
        stdout, stderr = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        if cg:
            wr(f"{cg.path}/cgroup.kill", 1)
        proc.kill()
        stdout, stderr = proc.communicate()
        stderr += f"\nmeasure.py: killed at the timeout, {timeout} s"
    t1 = time.time()
    if cg:
        totals = cg.finish()
        sampler.join()
        totals["samples"] = cg.samples
    else:
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        cpu = (after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime)
        peak = after.ru_maxrss * (1 if sys.platform == "darwin" else 1024)
        totals = {"cpu_s": round(cpu, 2), "peak_bytes": peak, "oom_kills": None,
                  "peak_is": "the largest one process's resident set (rusage), not the run's total"}
    return proc.returncode, stdout, stderr[-2000:], t0, t1, totals


def phases(result, t_spawn, t_end):
    """Start-up and operational time from the engine's marks (engine.py): the job handed to the
    ready engine (`start`) to the output durably written (`done`), and everything before `start`;
    or the Rust pool's own wall time."""
    marks = (result or {}).get("marks") or {}
    start = marks.get("start") or marks.get("ready")
    if start and marks.get("done"):
        r = lambda x: round(x, 3) if x is not None else None  # noqa: E731
        gap = lambda a, b: r(marks[b] - marks[a]) if marks.get(a) and marks.get(b) else None  # noqa: E731
        return {"startup_s": r(start - t_spawn), "operational_s": r(marks["done"] - start),
                "interpreter_s": r(marks["main"] - t_spawn) if marks.get("main") else None,
                "imports_s": gap("main", "imported"), "session_s": gap("imported", "ready"),
                "handover_s": gap("ready", "start"), "shutdown_s": r(t_end - marks["done"])}
    if (result or {}).get("engine") in ("rust-pool", "rust-baseline") and result.get("wall_s") is not None:
        # gate-baseline's own RESULT names its engine `rust-baseline`, as bench/vm's runs did.
        return {"startup_s": round(max(t_end - t_spawn - result["wall_s"], 0.0), 3),
                "operational_s": round(result["wall_s"], 3)}
    return {"startup_s": None, "operational_s": None}


def canonical(workload, out, root, keep_dump=None):
    """The output's canonical form: gate-canon's digest when it is built (with its dump kept at
    `keep_dump`), else canon.py's (with its tolerant rows)."""
    import canon
    tool = os.path.join(root, "bin", "gate-canon")
    if os.path.exists(tool):
        cmd = [tool, "digest", workload] + (["--dump", keep_dump] if keep_dump else []) + [out]
        p = subprocess.run(cmd, capture_output=True, text=True)
        if p.returncode != 0:
            return {"error": p.stderr.strip()[-300:]}
        d = json.loads(p.stdout)
        d["by"], d["dump"] = "gate-canon", keep_dump
        return d
    table = canon.read_output([out])
    if table is None:
        return {"error": "no output files"}
    d = canon.canonical(workload, table)
    d["by"] = "canon.py"
    return d


def compare(workload, mine, root, out, reference_dump=None, reference_json=None):
    """Whether `mine` matches the reference: bit for bit, within the job's tolerance, or not."""
    import canon
    if "error" in mine:
        return {"ok": False, "verdict": "no canonical form", "error": mine["error"]}
    ref = None
    if reference_json:
        ref = json.load(open(reference_json))
    if reference_dump and mine.get("dump"):
        p = subprocess.run([os.path.join(root, "bin", "gate-canon"), "compare", workload, reference_dump, mine["dump"]],
                           capture_output=True, text=True)
        c = json.loads(p.stdout) if p.stdout.strip().startswith("{") else {"error": p.stderr.strip()[-300:]}
        c["ok"] = p.returncode == 0
        c["verdict"] = verdict(c)
        c["by"] = "gate-canon"
        return c
    if ref is None:
        return {"ok": None, "verdict": "unchecked: no reference"}
    out_c = {"rows": mine["rows"] == ref["rows"], "hash_exact": mine["hash_exact"] == ref["hash_exact"],
             "hash_all": mine["hash_all"] == ref["hash_all"], "rows_mine": mine["rows"], "rows_ref": ref["rows"]}
    if out_c["rows"] and out_c["hash_exact"] and not out_c["hash_all"] and canon.SPECS[workload].get("tolerant"):
        if "tolerant_rows" not in mine:  # gate-canon gave no tolerant rows: canon.py's, for the comparison
            mine = canonical_py(workload, out)
        c = canon.compare(workload, ref, mine)
        out_c.update({"ok": c["ok"], "differing": c["differing"], "max_abs": c["max_abs"]})
        out_c["tolerance"] = canon.SPECS[workload]["tolerant"]
    else:
        out_c["ok"] = out_c["rows"] and out_c["hash_exact"] and out_c["hash_all"]
    out_c["verdict"] = verdict(out_c)
    out_c["by"] = "canon.py"
    return out_c


def canonical_py(workload, out):
    import canon
    return canon.canonical(workload, canon.read_output([out]))


def verdict(c):
    if c.get("error"):
        return f"not compared: {c['error']}"
    if c.get("hash_all"):
        return "bit for bit"
    if c.get("ok"):
        return f"within tolerance ({c.get('differing', 0)} values differ, max {c.get('max_abs', 0):.1e})"
    if c.get("rows") is False:
        return "DIFFERS: rows"
    return "DIFFERS"


def dump_path(a, tmp):
    """Where this run's gate-canon dump goes: the reference's file, or a temporary one to compare
    with the reference's (deleted with the run's temporary files)."""
    if a.make_reference:
        os.makedirs(os.path.dirname(os.path.abspath(a.make_reference)), exist_ok=True)
        return a.make_reference
    return os.path.join(tmp, "canon.dump") if a.reference_dump else None


def error_lines(stderr, result=None, code=None, lines=ERROR_LINES, limit=ERROR_BYTES):
    """A failed run's own error: the last `lines` lines of the engine's stderr (each line at most
    300 characters, at most `limit` in all, the last kept), else the error its RESULT line gave,
    else its exit code."""
    kept = [line.rstrip()[:300] for line in (stderr or "").splitlines() if line.strip()][-lines:]
    text = "\n".join(kept)[-limit:]
    if text:
        return text
    if (result or {}).get("error"):
        return str(result["error"])[-limit:]
    return f"no output on stderr; exit {code}"


def record(spec, rid, results, rec):
    os.makedirs(results, exist_ok=True)
    rec["id"] = rid
    rec["host"] = {"platform": sys.platform, "cpus": os.cpu_count()}
    json.dump(rec, open(os.path.join(results, f"{rid}.json"), "w"), default=str, indent=1)
    # What the table needs (run.py keeps it as the run's live record): with a failed run's last
    # lines at most ERROR_BYTES, about 2 KiB, inside the 8 KiB of output the batch keeps.
    brief = {k: rec.get(k) for k in ("id", "ok", "exit_code", "startup_s", "operational_s", "cpu_s", "peak_bytes",
                                     "oom_kills", "memory_gib", "data", "copy_in_s", "copy_out_s")}
    if rec.get("error_lines"):
        brief["error_lines"] = rec["error_lines"]
    check = rec.get("output_check") or {}
    brief["output"] = check.get("verdict")
    brief["check"] = {k: check[k] for k in ("ok", "hash_all", "max_abs", "verdict") if k in check}
    print("MEASURED " + json.dumps(brief, default=str), flush=True)
    return rec


def run(a):
    spec = json.load(open(a.spec))
    if isinstance(spec, list):  # the plan's runs (run.py's runs.json): the one named by --id
        spec = next(s for s in spec if s["id"] == a.id)
    rid = spec["id"]
    work = os.path.abspath(a.work)
    out, tmp = os.path.join(work, "out", rid), os.path.join(work, "tmp", rid)
    for d in (out, tmp):
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d)
    if spec.get("fresh_profiles") and spec.get("profiles"):
        shutil.rmtree(spec["profiles"], ignore_errors=True)
    if spec.get("profiles"):
        os.makedirs(spec["profiles"], exist_ok=True)
    if spec.get("staging"):
        os.makedirs(spec["staging"], exist_ok=True)
    store = stores.Store.from_env()
    data = {"data": "local"}
    # Outside every timing: the directory markers an earlier run's driver left above this run's
    # output (Spark's ABFS), which would fail a directory made under them.
    dropped = store.drop_markers(spec["output"]) if store else []
    inputs, target = spec.get("files", []), out
    if store and spec["engine"] in LOCAL_COPY:
        # No Blob reader: the inputs come to the local disk before the engine starts, timed apart.
        local_in = os.path.join(work, "in", rid)
        shutil.rmtree(local_in, ignore_errors=True)
        t = time.time()
        copied = sum(store.download(n, local_in) for n in spec["inputs"])
        inputs = [os.path.join(local_in, os.path.basename(n)) for n in spec["inputs"]]
        data = {"data": "local copy", "copy_in_s": round(time.time() - t, 3), "copy_in_bytes": copied}
    elif store:
        inputs, target = spec["inputs"], spec["output"]
        data = {"data": "blob"}
    flist = os.path.join(tmp, "files")
    open(flist, "w").write("\n".join(inputs))
    os.makedirs(a.results, exist_ok=True)
    report = os.path.join(os.path.abspath(a.results), f"{rid}.report.json")
    cmd = command(spec, a.root, target, flist, tmp, report)
    code, stdout, stderr, t0, t1, totals = execute(spec, cmd, environment(spec, tmp), spec.get("timeout_s", 1800))
    result = next((json.loads(line[7:]) for line in stdout.splitlines() if line.startswith("RESULT ")), None)
    rec = {"spec": spec, "command": cmd, "exit_code": code, "wall_s": round(t1 - t0, 3), "result": result,
           "memory_gib": spec.get("mem_gib"), **data, **totals, **phases(result, t0, t1)}
    if dropped:
        rec["markers_dropped"] = dropped
    rec["ok"] = code == 0 and result is not None and not result.get("error")
    if not rec["ok"]:
        rec["stderr"] = stderr
        rec["error_lines"] = error_lines(stderr, result, code)
        # The batch keeps this process's stderr in the run's state (runs.json): the engine's own
        # error reaches it, not only the record left on the machine.
        sys.stderr.write(f"{rid}: exit {code}: the engine's last lines:\n{rec['error_lines']}\n")
        sys.stderr.flush()
    if rec["ok"] and store and data["data"] == "local copy":
        t = time.time()
        for d, _, fs in os.walk(out):
            for f in fs:
                store.upload(os.path.join(d, f), f"{spec['output']}/{os.path.relpath(os.path.join(d, f), out)}")
        rec["copy_out_s"] = round(time.time() - t, 3)
    elif rec["ok"] and store:
        # The engine wrote Blob: its output comes back for the check, outside every timing.
        t = time.time()
        store.download(spec["output"], out)
        rec["fetch_s"] = round(time.time() - t, 3)
    rec["out_bytes"] = sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(out) for f in fs)
    if rec["ok"]:
        mine = canonical(spec["workload"], out, a.root, dump_path(a, tmp))
        rec["canon"] = {k: v for k, v in mine.items() if k != "tolerant_rows"}
        rec["output_check"] = compare(spec["workload"], mine, a.root, out, a.reference_dump, a.reference_json)
    if not a.keep_output:
        shutil.rmtree(out, ignore_errors=True)
    shutil.rmtree(tmp, ignore_errors=True)
    return record(spec, rid, a.results, rec)


def check(a):
    """The canonical form of an output written elsewhere (the node's), against the reference."""
    if a.blob_prefix:
        shutil.rmtree(a.out, ignore_errors=True)
        os.makedirs(a.out)
        stores.Store.from_env().download(a.blob_prefix, a.out)
    tmp = os.path.join(os.path.abspath(a.results), f".{a.id}.tmp")
    os.makedirs(tmp, exist_ok=True)
    mine = canonical(a.workload, a.out, a.root, dump_path(a, tmp))
    rec = {"spec": {"id": a.id, "workload": a.workload}, "ok": "error" not in mine,
           "canon": {k: v for k, v in mine.items() if k != "tolerant_rows"},
           "output_check": compare(a.workload, mine, a.root, a.out, a.reference_dump, a.reference_json)}
    shutil.rmtree(tmp, ignore_errors=True)
    return record(rec["spec"], f"{a.id}.check", a.results, rec)


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("run", "check"):
        s = sub.add_parser(name)
        s.add_argument("--root", required=True, help="install.py's root: the environments and bin/")
        s.add_argument("--results", required=True)
        s.add_argument("--reference-dump", default=None)
        s.add_argument("--reference-json", default=None)
        s.add_argument("--make-reference", default=None)
    r = sub.choices["run"]
    r.add_argument("--spec", required=True, help="one run's spec, or a list of them with --id")
    r.add_argument("--id", default=None)
    r.add_argument("--work", required=True, help="where the run's output and temporary files go")
    r.add_argument("--keep-output", action="store_true")
    c = sub.choices["check"]
    c.add_argument("--workload", required=True)
    c.add_argument("--out", required=True)
    c.add_argument("--id", required=True)
    c.add_argument("--blob-prefix", default="", help="fetch the output from the store's container first "
                   "(store.py: BENCH_ACCOUNT and the rest in the environment)")
    a = p.parse_args(argv)
    rec = run(a) if a.cmd == "run" else check(a)
    return 0 if rec.get("ok") and (rec.get("output_check") or {}).get("ok") is not False else 1


if __name__ == "__main__":
    sys.exit(main())
