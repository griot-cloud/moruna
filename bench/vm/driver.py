"""Runs the plan: each run in its own cgroup (cpuset, cpu.max, memory.max/high), sampled every 0.5 s.
Run as root. Results: /data/results/<id>.json. Usage: driver.py PLAN.json [ONLY_PREFIX]"""
import json, os, subprocess, sys, time, threading, shutil, glob
import pyarrow.parquet as pq, pyarrow.compute as pc
CG = "/sys/fs/cgroup/bench"; RES = "/data/results"
PY = {"moruna": "/data/venv-t/bin/python", "loop": "/data/venv-t/bin/python", "loophooked": "/data/venv-t/bin/python", "moruna065": "/data/venv-065/bin/python",
      "polars": "/data/venv-g/bin/python", "duckdb": "/data/venv-g/bin/python",
      "morunang": "/data/venv-t/bin/python", "spark": "/data/venv-rt/bin/python", "ray": "/data/venv-rt/bin/python",
      "dask": "/data/venv-rt/bin/python", "duckdbudf": "/data/venv-rt/bin/python"}
GiB = 1 << 30

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

def run(spec):
    rid = spec["id"]; cg = f"{CG}/{rid}"
    if os.path.isdir(cg): os.rmdir(cg)
    os.makedirs(cg)
    limits(cg, spec["cpus"], spec["mem_gib"])
    out = f"/data/out/{rid}"; shutil.rmtree(out, ignore_errors=True); os.makedirs(out); os.chown(out, 1000, 1000)
    flist = f"/data/results/{rid}.files"; open(flist, "w").write("\n".join(spec["files"]))
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
    if spec.get("perf"): cmd = ["perf", "record", "-F", "199", "-g", "-o", f"{RES}/{rid}.perf.data", "--"] + cmd
    env = {"PATH": "/usr/bin:/bin", "HOME": "/home/bench", "OPENBLAS_NUM_THREADS": "1", "OMP_NUM_THREADS": "1",
           "MKL_NUM_THREADS": "1", "PYTHONPATH": "/home/bench/bench", "JAVA_HOME": "/usr/lib/jvm/java-17-openjdk-amd64", "TMPDIR": "/data/tmp"}
    if spec["engine"] == "duckdb": env["HOME"] = "/home/bench"
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
    rec = {"spec": {k: v for k, v in spec.items() if k != "files"}, "files": len(spec["files"]), "exit_code": proc.returncode,
           "oom_kills": int(events.get("oom_kill", 0)), "wall_s": round(wall, 2), "cpu_s": round(cpu_s, 1),
           "cpu_util": round(cpu_s / (wall * spec["cpus"]), 3) if not spec.get("schedule") else None,
           "cg_peak_bytes": peak, "result": result, "applied": applied, "samples": samples}
    rec["ok"] = proc.returncode == 0 and result is not None and not (result or {}).get("error")
    if rec["ok"]: rec["check"] = checksum(spec["workload"], out)
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
