"""Install from a clean machine to able to run a job (Brackly, 2026-11-17): every engine of the
matrix, 5 runs each, on moruna-bench. Run as root, last of all (after the NYC matrix and
startup.py): it downloads.

- **The pip engines** (0.6.6, Ray, Spark, Dask, DuckDB, Polars): a new virtual environment and
  `pip install --no-cache-dir` of what the matrix's row imports, as bench. Measured: wall time,
  bytes received (/proc/net/dev, every interface but `lo`), disk (the environment's bytes) and
  the commands (2). The interpreter (uv's CPython 3.14) is there before and is not counted.
  0.6.6 is not on PyPI (it stops at 0.6.5): its wheel is installed from the file built here, and
  its bytes count as downloaded.
- **Spark's JVM**: `openjdk-17-jre-headless` from the distro, its .debs fetched again into an
  empty apt cache; its bytes and its installed tree count, and its command.
- **0.7** has no published release: what a release would carry is counted, its node kit, `moruna`
  and `moruna-vhost-vsock`, the gate image, and its QEMU (our build with KVM and seccomp, in
  /opt/qemu-kvm; QEMU counts as Spark's JVM does). Bytes and disk are the files'; its download
  time is derived from those bytes at the median throughput the pip installs got, and says so.
- **The Rust pool**: the `gate-baseline` binary, derived the same way.

    sudo /data/venv-t/bin/python install.py [RUNS]     # writes /data/results/install/*.json
"""
import glob
import json
import os
import shutil
import statistics
import subprocess
import sys
import time

OUT = "/data/results/install"
WORK = "/data/install"
KIT = os.environ.get("GATE_NODE_KIT", "/data/boot-gate")
BIN = os.environ.get("GATE_BIN", "/home/bench/gate/bin")
STACK = os.environ.get("GATE_STACK", "/data/gate/stack8s")
UV = "/home/bench/.local/bin/uv"
PIP = {
    "moruna": ("3.14t", ["{wheel}", "pyarrow", "numpy"]),
    "ray": ("3.14", ["ray[data]", "pyarrow", "pandas", "numpy"]),
    "spark": ("3.14", ["pyspark", "pyarrow", "pandas", "numpy"]),
    "dask": ("3.14", ["dask[distributed,dataframe]", "pyarrow", "pandas", "numpy"]),
    "duckdb": ("3.14", ["duckdb", "pyarrow", "numpy"]),
    "polars": ("3.14", ["polars", "pyarrow", "numpy"]),
}


def rx():
    total = 0
    for line in open("/proc/net/dev").read().splitlines()[2:]:
        name, data = line.split(":", 1)
        if name.strip() != "lo":
            total += int(data.split()[0])
    return total


def du(*paths):
    total = 0
    for p in paths:
        if os.path.isfile(p):
            total += os.path.getsize(p)
            continue
        for d, _, fs in os.walk(p):
            for f in fs:
                fp = os.path.join(d, f)
                if not os.path.islink(fp):
                    total += os.path.getsize(fp)
    return total


def as_bench(cmd):
    return subprocess.run(["setpriv", "--reuid=bench", "--regid=bench", "--init-groups", "--"] + cmd,
                          env={"PATH": "/home/bench/.local/bin:/usr/bin:/bin", "HOME": "/home/bench",
                               "PIP_NO_CACHE_DIR": "1", "UV_NO_CACHE": "1"},
                          stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)


def pip_install(engine, i):
    py, pkgs = PIP[engine]
    wheel = (glob.glob("/data/dist066/moruna-0.6.6-*.whl") or [""])[0]
    pkgs = [p.format(wheel=wheel) for p in pkgs]
    venv = f"{WORK}/{engine}-{i}"
    shutil.rmtree(venv, ignore_errors=True)
    os.makedirs(WORK, exist_ok=True)
    os.chown(WORK, 1000, 1000)
    interp = subprocess.run([UV, "python", "find", py], capture_output=True, text=True,
                            env={"HOME": "/home/bench", "PATH": "/usr/bin:/bin"}).stdout.strip()
    r0, t0 = rx(), time.time()
    a = as_bench([UV, "venv", "-q", "--seed", "-p", interp, venv])
    b = as_bench([f"{venv}/bin/pip", "install", "-q", "--no-cache-dir"] + pkgs)
    wall, got = time.time() - t0, rx() - r0
    rec = {"engine": engine, "run": i, "wall_s": wall, "bytes_down": got + (os.path.getsize(wheel) if engine == "moruna" else 0),
           "disk_bytes": du(venv), "commands": 2, "ok": a.returncode == 0 and b.returncode == 0,
           "error": (a.stderr + b.stderr)[-300:] if a.returncode or b.returncode else None}
    if engine == "moruna":
        rec["note"] = "0.6.6 is not on PyPI: its wheel installed from the file built here, its bytes counted as downloaded"
    shutil.rmtree(venv, ignore_errors=True)
    return rec


def jvm(i):
    """Spark's JVM from the distro: its .debs fetched again into an empty apt cache."""
    subprocess.run(["apt-get", "clean"])
    r0, t0 = rx(), time.time()
    p = subprocess.run(["apt-get", "install", "-y", "-q", "--reinstall", "--download-only", "openjdk-17-jre-headless",
                        "openjdk-17-jre-headless:amd64"], capture_output=True, text=True,
                       env={"PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "DEBIAN_FRONTEND": "noninteractive"})
    debs = du("/var/cache/apt/archives")
    return {"engine": "spark-jvm", "run": i, "wall_s": time.time() - t0, "bytes_down": max(rx() - r0, debs),
            "disk_bytes": du("/usr/lib/jvm/java-17-openjdk-amd64"), "commands": 1, "ok": p.returncode == 0}


def moruna07_files():
    """What a 0.7 release would carry: node-for-job.py's kit, the CLI, the gate image, QEMU."""
    kit = [f"{KIT}/{p}" for p in ("moruna-microkernel.elf", "node-user-bins", "moruna-node-ci-assets",
                                   "moruna-node-image", "moruna-image-host", "moruna-storage-provision",
                                   "layers/layer-1.img", "layers/layer-2.img", "ke2/roots", "node")]
    return {"node kit": kit, "moruna and its vsock back end": [f"{BIN}/moruna", f"{BIN}/moruna-vhost-vsock"],
            "gate image": [STACK], "QEMU (our build, KVM and seccomp)": ["/opt/qemu-kvm"]}


def main(runs):
    os.makedirs(OUT, exist_ok=True)
    recs = []
    for i in range(runs):
        for e in PIP:
            r = pip_install(e, i)
            recs.append(r)
            print(time.strftime("%H:%M:%S"), json.dumps(r), flush=True)
        r = jvm(i)
        recs.append(r)
        print(time.strftime("%H:%M:%S"), json.dumps(r), flush=True)
    json.dump(recs, open(f"{OUT}/runs.json", "w"), indent=1)
    ok = [r for r in recs if r["ok"] and r["engine"] in PIP and r["wall_s"] > 0]
    throughput = statistics.median(r["bytes_down"] / r["wall_s"] for r in ok) if ok else None
    summary = {}
    for e in list(PIP) + ["spark-jvm"]:
        rs = [r for r in recs if r["engine"] == e and r["ok"]]
        if rs:
            ws = [r["wall_s"] for r in rs]
            summary[e] = {"wall_s": {"median": statistics.median(ws), "min": min(ws), "max": max(ws)},
                          "bytes_down": statistics.median(r["bytes_down"] for r in rs),
                          "disk_bytes": statistics.median(r["disk_bytes"] for r in rs), "commands": rs[0]["commands"]}
    files = moruna07_files()
    parts = {k: du(*v) for k, v in files.items()}
    total = sum(parts.values())
    summary["moruna-vm (0.7)"] = {"bytes": total, "parts": parts, "disk_bytes": total,
                                  "derived_download_s": total / throughput if throughput else None,
                                  "how": "the files a release would carry, at the median pip throughput; no release exists"}
    pool = du(f"{BIN}/gate-baseline")
    summary["rust-pool"] = {"bytes": pool, "disk_bytes": pool,
                            "derived_download_s": pool / throughput if throughput else None,
                            "how": "the binary, at the median pip throughput"}
    summary["pip_throughput_bytes_s"] = throughput
    json.dump(summary, open(f"{OUT}/summary.json", "w"), indent=1)
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main(int(sys.argv[1]) if len(sys.argv) > 1 else 5)
