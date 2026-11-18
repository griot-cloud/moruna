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

Start-up is from the process's start to the engine's `ready` mark (engine.py); operational time
from `ready` to `done`, the output written. For the Rust pool, operational time is its own
`wall_s` and start-up the rest of the process's life before it.

The output is reduced to its canonical form (gate-canon/1: `gate-canon` when it is built, else
bench/vm/canon.py) and compared with the reference's: a dump `gate-canon` wrote, or a canon.py
JSON with its tolerant rows. `check` does the same for an output another machine wrote (the
Moruna node's), fetched from Blob first with `--blob-prefix`. Each writes RESULTS/ID.json and prints it on one line after `MEASURED `.
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
CG = "/sys/fs/cgroup/bench"
GiB = 1 << 30
PYTHON_ENGINES = ("moruna066", "moruna066-rust", "duckdb", "polars", "ray", "spark", "dask")
VENV = {"moruna066": "moruna066", "moruna066-rust": "moruna066", "ray": "ray", "spark": "spark",
        "dask": "dask", "duckdb": "duckdb", "polars": "polars"}


def wr(path, value):
    with open(path, "w") as f:
        f.write(str(value))


def rd(path):
    try:
        return open(path).read()
    except OSError:
        return ""


def command(spec, root, out, flist, tmp, report):
    """The run's command line: engine.py in the engine's environment, or the Rust pool's binary."""
    e = spec["engine"]
    if e == "rust-pool":
        return [os.path.join(root, "bin", "gate-baseline"), "--job", spec["workload"], "--files", flist,
                "--out", out, "--threads", str(spec["cpus"]), "--batch", str(spec.get("batch", 0))]
    if e not in PYTHON_ENGINES:
        raise ValueError(f"measure.py: no engine {e!r}")
    cmd = [os.path.join(root, f"venv-{VENV[e]}", "bin", "python"), "-I", os.path.join(HERE, "engine.py"),
           "--engine", e, "--workload", spec["workload"], "--files", flist, "--out", out,
           "--cpus", str(spec["cpus"]), "--mem-gib", str(spec["mem_gib"]), "--tmp", tmp,
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
    if os.environ.get("JAVA_HOME"):
        env["JAVA_HOME"] = os.environ["JAVA_HOME"]
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
    """Start-up and operational time from the engine's marks (engine.py), or from the Rust pool's
    own wall time."""
    marks = (result or {}).get("marks") or {}
    if marks.get("ready") and marks.get("done"):
        return {"startup_s": round(marks["ready"] - t_spawn, 3), "operational_s": round(marks["done"] - marks["ready"], 3),
                "interpreter_s": round(marks["main"] - t_spawn, 3) if marks.get("main") else None,
                "imports_s": round(marks["imported"] - marks["main"], 3) if marks.get("imported") and marks.get("main") else None,
                "session_s": round(marks["ready"] - marks["imported"], 3) if marks.get("imported") else None,
                "shutdown_s": round(t_end - marks["done"], 3)}
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


def record(spec, rid, results, rec):
    os.makedirs(results, exist_ok=True)
    rec["id"] = rid
    rec["host"] = {"platform": sys.platform, "cpus": os.cpu_count()}
    json.dump(rec, open(os.path.join(results, f"{rid}.json"), "w"), default=str, indent=1)
    brief = {k: rec.get(k) for k in ("id", "ok", "exit_code", "startup_s", "operational_s", "cpu_s", "peak_bytes")}
    brief["output"] = (rec.get("output_check") or {}).get("verdict")
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
    flist = os.path.join(tmp, "files")
    open(flist, "w").write("\n".join(spec["files"]))
    os.makedirs(a.results, exist_ok=True)
    report = os.path.join(os.path.abspath(a.results), f"{rid}.report.json")
    cmd = command(spec, a.root, out, flist, tmp, report)
    code, stdout, stderr, t0, t1, totals = execute(spec, cmd, environment(spec, tmp), spec.get("timeout_s", 1800))
    result = next((json.loads(line[7:]) for line in stdout.splitlines() if line.startswith("RESULT ")), None)
    rec = {"spec": spec, "command": cmd, "exit_code": code, "wall_s": round(t1 - t0, 3), "result": result,
           **totals, **phases(result, t0, t1)}
    rec["ok"] = code == 0 and result is not None and not result.get("error")
    if not rec["ok"]:
        rec["stderr"] = stderr
    rec["out_bytes"] = sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(out) for f in fs)
    if rec["ok"]:
        mine = canonical(spec["workload"], out, a.root, dump_path(a, tmp))
        rec["canon"] = {k: v for k, v in mine.items() if k != "tolerant_rows"}
        rec["output_check"] = compare(spec["workload"], mine, a.root, out, a.reference_dump, a.reference_json)
    if not a.keep_output:
        shutil.rmtree(out, ignore_errors=True)
    shutil.rmtree(tmp, ignore_errors=True)
    return record(spec, rid, a.results, rec)


def fetch(base, container, prefix, sas, dest):
    """Every blob under `prefix` in `container`, into `dest`: the node's output, from Blob."""
    import urllib.request
    import xml.etree.ElementTree as ET
    names, marker = [], ""
    while True:
        url = f"{base}/{container}?restype=container&comp=list&prefix={prefix}&{sas}" + (f"&marker={marker}" if marker else "")
        root = ET.fromstring(urllib.request.urlopen(url, timeout=60).read())
        names += [b.findtext("Name") for b in root.iter("Blob")]
        marker = root.findtext("NextMarker") or ""
        if not marker:
            break
    for name in names:
        path = os.path.join(dest, name[len(prefix):].lstrip("/"))
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with urllib.request.urlopen(f"{base}/{container}/{name}?{sas}", timeout=600) as r, open(path, "wb") as f:
            shutil.copyfileobj(r, f)
    return names


def check(a):
    """The canonical form of an output written elsewhere (the node's), against the reference."""
    if a.blob_prefix:
        shutil.rmtree(a.out, ignore_errors=True)
        os.makedirs(a.out)
        fetch(os.environ["BASE"], "results", a.blob_prefix, os.environ["SAS_RESULTS"], a.out)
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
    c.add_argument("--blob-prefix", default="", help="fetch the output from container `results` first "
                   "(BASE and SAS_RESULTS in the environment)")
    a = p.parse_args(argv)
    rec = run(a) if a.cmd == "run" else check(a)
    return 0 if rec.get("ok") and (rec.get("output_check") or {}).get("ok") is not False else 1


if __name__ == "__main__":
    sys.exit(main())
