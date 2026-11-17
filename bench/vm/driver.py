"""Runs the plan: each run in its own cgroup (cpuset, cpu.max, memory.max/high), sampled every 0.5 s.
Run as root. Results: /data/results/<id>.json. Usage: driver.py PLAN.json [ONLY_PREFIX]

Every run's output is reduced to the gate's canonical form (gate-canon, canon.py) before it is
deleted: `canon` holds its canonical row count and hashes, and a workload with a tolerant column
keeps its tolerant rows in /data/results/canon/<id>.dump for report.py to compare. `check`, the
row count and column sums of 0.6.5's harness, stays beside it.

The engines with no Python (rust-baseline, the gate's Rust functions in a rayon pool, and
moruna-vm, 0.7 through `moruna run` on the data OS) run as their own commands in the cgroup."""
import json, os, subprocess, sys, time, threading, shutil, glob
import pyarrow.parquet as pq, pyarrow.compute as pc
CG = "/sys/fs/cgroup/bench"; RES = "/data/results"
PY = {"moruna066-rust": "/data/venv-t/bin/python", "duckdb-sql": "/data/venv-g/bin/python", "polars-expr": "/data/venv-g/bin/python",
      "moruna": "/data/venv-t/bin/python", "loop": "/data/venv-t/bin/python", "loophooked": "/data/venv-t/bin/python", "moruna065": "/data/venv-065/bin/python",
      "polars": "/data/venv-g/bin/python", "duckdb": "/data/venv-g/bin/python",
      "morunang": "/data/venv-t/bin/python", "spark": "/data/venv-rt/bin/python", "ray": "/data/venv-rt/bin/python",
      "dask": "/data/venv-rt/bin/python", "duckdbudf": "/data/venv-rt/bin/python"}
GiB = 1 << 30
# The gate's binaries (images/gate): gate-baseline and gate-canon, built on the VM with
# `cargo build --release` in images/gate; 0.7's `moruna` CLI (stream D) for moruna-vm.
GATE = os.environ.get("GATE_BIN", "/home/bench/gate/bin")
MORUNA07 = os.environ.get("MORUNA07_BIN", "/home/bench/moruna07/bin/moruna")
NATIVE = ("rust-baseline", "moruna-vm", "moruna-vm-free1")
DUMPED = ("score",)

def wr(path, value):
    with open(path, "w") as f: f.write(str(value))

def rd(path):
    try: return open(path).read()
    except OSError: return ""

def setup():
    wr("/sys/fs/cgroup/cgroup.subtree_control", "+cpuset +cpu +memory")
    os.makedirs(CG, exist_ok=True)
    wr(f"{CG}/cgroup.subtree_control", "+cpuset +cpu +memory")
    os.makedirs(RES, exist_ok=True)
    for d in ("/data/out", "/data/tmp", "/data/spark-tmp", "/data/ray-tmp", "/data/dask-tmp"):
        os.makedirs(d, exist_ok=True); os.chmod(d, 0o1777)

def limits(cg, cpus, mem_gib, high_gib=None):
    wr(f"{cg}/cpuset.cpus", f"0-{cpus - 1}")
    wr(f"{cg}/cpu.max", f"{cpus * 100000} 100000")
    wr(f"{cg}/memory.high", "max" if high_gib is None else int(high_gib * GiB))
    wr(f"{cg}/memory.max", int(mem_gib * GiB))
    wr(f"{cg}/memory.swap.max", 0)

def checksum(workload, out):
    files = sorted(glob.glob(f"{out}/**/*.parquet", recursive=True))
    rows, s = 0, 0.0
    col = {"enrich": "duration_s", "audit": "expected_fare", "occupancy": "car_minutes", "score": "predicted_tip",
           "fw_clean": "quality", "fw_minhash": None, "ov_shape": "area_m2", "ov_attrs": "volume_m3"}[workload]
    for f in files:
        if col is None:
            rows += pq.ParquetFile(f).metadata.num_rows; continue
        t = pq.read_table(f, columns=[col]); rows += t.num_rows
        v = pc.sum(t[col]).as_py(); s += v or 0
    if workload == "occupancy": rows = None   # per-batch partial counts: only the total is invariant
    return {"rows_out": rows, "sum": round(s, 1), "files": len(files)}

def canon(spec, out):
    """The output's canonical form: gate-canon's digest, and its tolerant rows kept for a later
    comparison when the workload has a tolerant column (canon.py's SPECS)."""
    import canon as reference
    wl = spec["workload"]
    os.makedirs(f"{RES}/canon", exist_ok=True)
    # Tolerant rows are kept for the NYC jobs only: Overture's 1.19 billion rows would make a dump
    # of tens of GB an engine. There, hash_exact is the check, and hash_all says whether the
    # tolerant columns matched bit for bit; a bucketed comparison of them is not built yet.
    dump = f"{RES}/canon/{spec['id']}.dump" if reference.SPECS[wl].get("tolerant") and wl in DUMPED else None
    tool = f"{GATE}/gate-canon"
    if os.path.exists(tool):
        cmd = [tool, "digest", wl] + (["--dump", dump] if dump else []) + [out]
        p = subprocess.run(cmd, capture_output=True, text=True)
        if p.returncode != 0:
            return {"error": p.stderr.strip()[-300:]}
        d = json.loads(p.stdout)
    else:  # the reference implementation: fine for small outputs, slow for large ones
        d = reference.canonical(wl, reference.read_output([out]))
        d.pop("tolerant_rows")
        d["note"] = "canon.py: gate-canon was not built"
    d["dump"] = dump
    return d


def moruna_vm(spec, out, flist):
    """moruna-vm's command and where its report goes: the gate's job (gate_jobs.py) through 0.7's
    `moruna run JOB --output DIR --report FILE` (stream D), its native tasks the image of the
    stack in `image_stack`, its VM `vm_memory_gib`, its job's budget `budget_gib` (D: a 24 GiB VM
    takes a budget of 20.6 GiB or less). The node's serial console and QEMU's log are kept in
    /data/tmp/ID.node (MORUNA_KEEP_NODE_LOGS) and moved beside the results."""
    import gate_jobs
    stack = spec["image_stack"]
    digest = open(f"{stack}/image.digest").read().strip()
    job = f"{RES}/{spec['id']}.job.json"
    gate_jobs.write(job, spec["workload"], spec["files"], out, {"digest": digest, "location": "/images/gate"},
                    cpus=spec["cpus"], mem_gib=spec.get("budget_gib", spec["mem_gib"]))
    os.chmod(job, 0o644)
    report = f"/data/tmp/{spec['id']}.report.json"
    cmd = [MORUNA07, "run", job, "--output", out, "--report", report,
           "--memory-gib", str(spec.get("vm_memory_gib", 24)), "--image-stack", stack]
    return cmd, report


def vm_result(spec, report_path, wall):
    """moruna-vm's result from 0.7's report: the VM's figures (`vm`: boot_s, stage_s, job_s,
    outputs_s, total_s) apart, and the time split as far as the report has it (control 5)."""
    import runner_split
    try:
        d = json.load(open(report_path))
    except (OSError, ValueError) as e:
        return {"engine": spec["engine"], "function": "rust", "wall_s": wall, "error": f"no report: {e}"}
    vm = d.get("vm") or {}
    boot = vm.get("boot_s", (vm.get("boot_ms") or 0) / 1e3 or None)
    res = {"engine": spec["engine"], "function": "rust", "wall_s": wall, "boot_s": boot, "vm": vm,
           "version": d.get("version")}
    res["moruna"] = runner_split.split(d)
    res["split"] = res["moruna"]["split"]
    if d.get("exit") not in (None, "completed", "ok", 0):
        res["error"] = f"exit {d.get('exit')}: {str(d.get('diagnostic'))[:300]}"
    return res


def command(spec, out, flist):
    """The run's command: runner.py for every Python engine, the binary for the Rust ones."""
    as_bench = ["setpriv", "--reuid=bench", "--regid=bench", "--init-groups", "--"]
    if spec["engine"] == "rust-baseline":
        return as_bench + [f"{GATE}/gate-baseline", "--job", spec["workload"], "--files", flist, "--out", out,
                           "--threads", str(spec.get("threads", 0)), "--batch", str(spec.get("batch", 0))], None
    if spec["engine"].startswith("moruna-vm"):
        cmd, report = moruna_vm(spec, out, flist)
        return as_bench + cmd, report
    return None, None


def runner_command(spec, rid, out, flist):
    """runner.py's command for a Python engine, with the plan's options."""
    cmd = ["setpriv", "--reuid=bench", "--regid=bench", "--init-groups", "--", PY[spec["engine"]], "-I",
           "/home/bench/bench/runner.py", "--engine", spec["engine"], "--workload", spec["workload"],
           "--files", flist, "--out", out, "--report", f"{RES}/{rid}.report.json",
           "--threads", str(spec.get("threads", 0)), "--batch", str(spec.get("batch", 0))]
    if spec.get("staging"): cmd += ["--staging", spec["staging"]]
    if spec["engine"] == "morunang": cmd[cmd.index("--engine") + 1] = "morunang"; cmd += ["--no-guard"]
    if spec.get("preferred_rows"): cmd += ["--preferred-rows", str(spec["preferred_rows"])]
    if spec.get("trace"): cmd += ["--trace", f"/data/tmp/{rid}.trace.arrow"]
    if spec.get("engine_cpu"): cmd += ["--engine-cpu", str(spec["engine_cpu"])]
    if spec.get("defer"): cmd += ["--defer"]
    if spec.get("profiles"):
        # A fresh store makes the run a first run; a kept one lets it learn from the runs before.
        if spec.get("fresh_profiles"): shutil.rmtree(spec["profiles"], ignore_errors=True)
        os.makedirs(spec["profiles"], exist_ok=True); os.chown(spec["profiles"], 1000, 1000)
        cmd += ["--profiles", spec["profiles"]]
    if spec.get("cost"):
        # The cost model's observation store: emptied for a first run, kept for the repeats.
        if spec.get("fresh_cost"): shutil.rmtree(spec["cost"], ignore_errors=True)
        os.makedirs(spec["cost"], exist_ok=True); os.chown(spec["cost"], 1000, 1000)
        cmd += ["--cost", spec["cost"]]
    return cmd


def run(spec):
    rid = spec["id"]; cg = f"{CG}/{rid}"
    if os.path.isdir(cg): os.rmdir(cg)
    os.makedirs(cg)
    limits(cg, spec["cpus"], spec["mem_gib"])
    out = f"/data/out/{rid}"; shutil.rmtree(out, ignore_errors=True); os.makedirs(out); os.chown(out, 1000, 1000)
    flist = f"/data/results/{rid}.files"; open(flist, "w").write("\n".join(spec["files"]))
    native, vm_report = command(spec, out, flist)
    cmd = native or runner_command(spec, rid, out, flist)
    if spec.get("perf"): cmd = ["perf", "record", "-F", "199", "-g", "-o", f"{RES}/{rid}.perf.data", "--"] + cmd
    env = {"PATH": "/usr/bin:/bin", "HOME": "/home/bench", "OPENBLAS_NUM_THREADS": "1", "OMP_NUM_THREADS": "1",
           "MKL_NUM_THREADS": "1", "PYTHONPATH": "/home/bench/bench", "JAVA_HOME": "/usr/lib/jvm/java-17-openjdk-amd64", "TMPDIR": "/data/tmp"}
    if spec["engine"] == "duckdb": env["HOME"] = "/home/bench"
    if spec["engine"].startswith("moruna-vm"):
        # QEMU with KVM and seccomp first in PATH, the node kit, a scratch for the run's directory,
        # and where the node's logs go.
        node_logs = f"/data/tmp/{rid}.node"; shutil.rmtree(node_logs, ignore_errors=True)
        env.update({"PATH": "/opt/qemu-kvm/bin:/usr/bin:/bin", "MORUNA_NODE_KIT": "/data/boot",
                    "TMPDIR": "/data/tmp", "MORUNA_KEEP_NODE_LOGS": node_logs})
    env.update(spec.get("env", {}))
    log = open(f"{RES}/{rid}.log", "w")
    t0 = time.time()
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=log, env=env, text=True, cwd="/tmp",
                            preexec_fn=lambda: wr(f"{cg}/cgroup.procs", os.getpid()))
    samples, stop = [], threading.Event()
    def sample():
        last_u, last_t = None, None
        while not stop.is_set():
            now = time.time() - t0
            u = next((int(l.split()[1]) for l in rd(f"{cg}/cpu.stat").splitlines() if l.startswith("usage_usec")), None)
            m = rd(f"{cg}/memory.current").strip()
            st = dict(l.split() for l in rd(f"{cg}/memory.stat").splitlines() if l.split()[0] in ("anon", "file"))
            if u is not None and last_u is not None:
                samples.append({"t": round(now, 2), "cores": round((u - last_u) / 1e6 / (now - last_t), 2),
                                "mem": int(m or 0), "anon": int(st.get("anon", 0))})
            last_u, last_t = u, now
            stop.wait(0.5)
    th = threading.Thread(target=sample); th.start()
    sched = sorted(spec.get("schedule", []), key=lambda e: e["t"]); applied = []
    while proc.poll() is None:
        el = time.time() - t0
        while sched and sched[0]["t"] <= el:
            e = sched.pop(0)
            if "cpus" in e:
                wr(f"{cg}/cpu.max", f"{e['cpus'] * 100000} 100000"); wr(f"{cg}/cpuset.cpus", f"0-{e['cpus'] - 1}")
            if "high_gib" in e: wr(f"{cg}/memory.high", "max" if e["high_gib"] is None else int(e["high_gib"] * GiB))
            if "mem_gib" in e: wr(f"{cg}/memory.max", int(e["mem_gib"] * GiB))
            applied.append({**e, "at": round(el, 2)})
        if el > spec.get("timeout_s", 1800) and proc.poll() is None:
            wr(f"{cg}/cgroup.kill", 1); proc.kill()
        time.sleep(0.2)
    stdout = proc.stdout.read(); stop.set(); th.join()
    wr(f"{cg}/cgroup.kill", 1); time.sleep(1)   # runtimes' daemons (Ray, Dask, Spark) must not outlive their run
    wall = time.time() - t0
    events = dict(l.split() for l in rd(f"{cg}/memory.events").splitlines())
    peak = int(rd(f"{cg}/memory.peak").strip() or 0)
    cpu_s = next((int(l.split()[1]) for l in rd(f"{cg}/cpu.stat").splitlines() if l.startswith("usage_usec")), 0) / 1e6
    result = next((json.loads(l[7:]) for l in stdout.splitlines() if l.startswith("RESULT ")), None)
    if spec["engine"].startswith("moruna-vm"):
        result = vm_result(spec, vm_report, wall)
        # The run's console (the job updates, task logs among them) and its node's logs.
        open(f"{RES}/{rid}.stdout", "w").write(stdout)
        if os.path.isdir(f"/data/tmp/{rid}.node"):
            shutil.rmtree(f"{RES}/{rid}.node", ignore_errors=True)
            shutil.move(f"/data/tmp/{rid}.node", f"{RES}/{rid}.node")
        if vm_report and os.path.exists(vm_report):
            shutil.move(vm_report, f"{RES}/{rid}.report.json")
    rec = {"spec": {k: v for k, v in spec.items() if k != "files"}, "files": len(spec["files"]), "exit_code": proc.returncode,
           "oom_kills": int(events.get("oom_kill", 0)), "wall_s": round(wall, 2), "cpu_s": round(cpu_s, 1),
           "cpu_util": round(cpu_s / (wall * spec["cpus"]), 3) if not spec.get("schedule") else None,
           "cg_peak_bytes": peak, "result": result, "applied": applied, "samples": samples}
    rec["ok"] = proc.returncode == 0 and result is not None and not (result or {}).get("error")
    if rec["ok"]:
        rec["check"] = checksum(spec["workload"], out)
        rec["canon"] = canon(spec, out)
    # The PM's controls 4 and 5 beside wall time: CPU-seconds and peak memory are above; the
    # split and the VM's boot are the engine's own figures.
    r = result or {}
    rec["boot_s"] = r.get("boot_s")
    rec["vm"] = r.get("vm")
    rec["split"] = r.get("split") or (r.get("moruna") or {}).get("split")
    rec["function"] = r.get("function")
    shutil.rmtree(out, ignore_errors=True)
    for p in glob.glob(f"{cg}/*"): pass
    try: os.rmdir(cg)
    except OSError: pass
    if spec.get("trace") and os.path.exists(f"/data/tmp/{rid}.trace.arrow"):
        shutil.move(f"/data/tmp/{rid}.trace.arrow", f"{RES}/{rid}.trace.arrow")
    if spec.get("perf") and os.path.exists(f"{RES}/{rid}.perf.data"):
        with open(f"{RES}/{rid}.perf.txt", "w") as fh:
            subprocess.run(["perf", "report", "-i", f"{RES}/{rid}.perf.data", "--stdio", "--no-children",
                            "--sort", "dso,symbol", "--percent-limit", "0.3"], stdout=fh, stderr=subprocess.DEVNULL)
        with open(f"{RES}/{rid}.perf-dso.txt", "w") as fh:
            subprocess.run(["perf", "report", "-i", f"{RES}/{rid}.perf.data", "--stdio", "--no-children",
                            "--sort", "dso", "--percent-limit", "0.3"], stdout=fh, stderr=subprocess.DEVNULL)
        os.remove(f"{RES}/{rid}.perf.data")
    json.dump(rec, open(f"{RES}/{rid}.json", "w"), default=str)
    brief = {k: rec[k] for k in ("ok", "exit_code", "oom_kills", "wall_s", "cpu_util")}
    print(time.strftime("%H:%M:%S"), rid, json.dumps(brief), (result or {}).get("error", "")[:200],
          json.dumps(rec.get("check")), flush=True)

if __name__ == "__main__":
    setup()
    plan = json.load(open(sys.argv[1]))
    only = sys.argv[2] if len(sys.argv) > 2 else ""
    for spec in plan:
        if spec["id"].startswith(only) and not os.path.exists(f"{RES}/{spec['id']}.json"):
            run(spec)
    print("PLAN-DONE", flush=True)
