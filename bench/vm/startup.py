"""Setup time against the other engines (Brackly, 2026-11-17), beside the NYC tables: for every
engine of the matrix, 5 runs cold (the page cache dropped first) and 5 warm, of

- **start**: the command to ready for the first batch. 0.7: its report's `vm.launch_s + boot_s +
  stage_s`; Ray: `ray.init`; Spark: the session; Dask: its local cluster; 0.6.6, DuckDB and
  Polars: the process's start and the import or open; the Rust pool: the process's start and its
  open (a run of no files);
- **first result**: one NYC file (January 2024) through Occupancy, the command to its output on
  disk and the process gone;
- **shutdown**: the last batch written (the engine's own mark; 0.7: its `exit` update) to every
  process of the run gone, the VM's QEMU included, so its memory is returned.

Each run is in a cgroup of its own, as driver.py's are, sampled every 20 ms. Run as root after
the NYC matrix's groups 1, 2, 3 and 5; install.py (clean installs) runs last of all.

    sudo /data/venv-t/bin/python startup.py [RUNS]     # writes /data/results/startup/*.json
"""
import json
import os
import shutil
import statistics
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
FILE = "/data/nyc/norm/yellow_tripdata_2024-01.parquet"
CG = "/sys/fs/cgroup/bench"
OUT = "/data/results/startup"
BIN = os.environ.get("GATE_BIN", "/home/bench/gate/bin")
MORUNA07 = os.environ.get("MORUNA07_BIN", f"{BIN}/moruna")
STACK = os.environ.get("GATE_STACK", "/data/gate/stack8s")
PY = {"moruna": "/data/venv-t/bin/python", "duckdb": "/data/venv-g/bin/python", "polars": "/data/venv-g/bin/python",
      "ray": "/data/venv-rt/bin/python", "spark": "/data/venv-rt/bin/python", "dask": "/data/venv-rt/bin/python"}
MARK = "import time\ndef mark(what): print('MARK', what, time.time(), flush=True)\n"

# Each engine's job: Occupancy on one file into sys.argv[1], marking `ready` and `last`.
PROGRAMS = {
    "moruna": MARK + """
import sys, moruna, workloads
mark('ready')
moruna.run(moruna.ParquetSource([%(file)r]), [moruna.function(workloads.occupancy)], moruna.ParquetSink(sys.argv[1]))
mark('last')
""",
    "ray": MARK + """
import sys, ray, pyarrow as pa, workloads
ray.init(num_cpus=8, include_dashboard=False, logging_level='ERROR', log_to_driver=False, _temp_dir='/data/ray-tmp')
mark('ready')
ray.data.DataContext.get_current().enable_progress_bars = False
def f(t):
    bs = t.combine_chunks().to_batches()
    return pa.Table.from_batches([workloads.occupancy(bs[0])]) if bs else t
ray.data.read_parquet([%(file)r]).map_batches(f, batch_format='pyarrow', batch_size=250000).write_parquet(sys.argv[1])
mark('last')
ray.shutdown()
""",
    "spark": MARK + """
import os, sys
os.environ['PYSPARK_PYTHON'] = sys.executable
from pyspark.sql import SparkSession
import workloads
spark = (SparkSession.builder.master('local[8]').appName('startup').config('spark.ui.enabled', 'false')
         .config('spark.sql.execution.arrow.maxRecordsPerBatch', '250000').config('spark.local.dir', '/data/spark-tmp')
         .getOrCreate())
spark.sparkContext.setLogLevel('ERROR')
mark('ready')
(spark.read.parquet(%(file)r).mapInArrow(lambda it: (workloads.occupancy(b) for b in it),
    'zone long, minute_of_day long, car_minutes long').write.mode('overwrite').parquet(sys.argv[1]))
mark('last')
spark.stop()
""",
    "dask": MARK + """
import sys
from dask.distributed import LocalCluster, Client
import dask.dataframe as dd, pyarrow as pa, workloads
cluster = LocalCluster(n_workers=8, threads_per_worker=1, processes=True, dashboard_address=None,
                       local_directory='/data/dask-tmp')
client = Client(cluster)
mark('ready')
ddf = dd.read_parquet(%(file)r)
meta = workloads.occupancy(pa.RecordBatch.from_pandas(ddf.head(1000), preserve_index=False)).to_pandas().iloc[:0]
ddf.map_partitions(lambda p: workloads.occupancy(pa.RecordBatch.from_pandas(p, preserve_index=False)).to_pandas(),
                   meta=meta).to_parquet(sys.argv[1], write_index=False)
mark('last')
client.close(); cluster.close()
""",
    "duckdb": MARK + """
import sys, duckdb, native_exprs
con = duckdb.connect()
mark('ready')
con.execute("COPY (" + native_exprs.duckdb_query('occupancy', "read_parquet([%(file)r])") + ") TO '" + sys.argv[1] + "/part.parquet' (FORMAT parquet, COMPRESSION zstd)")
mark('last')
""",
    "polars": MARK + """
import sys, polars as pl, pyarrow.parquet as pq, workloads
mark('ready')
w = None
for df in pl.scan_parquet(%(file)r).collect_batches(chunk_size=250000):
    o = workloads.occupancy(df.to_arrow().combine_chunks().to_batches()[0])
    w = w or pq.ParquetWriter(sys.argv[1] + '/part.parquet', o.schema, compression='zstd')
    w.write_batch(o)
w.close()
mark('last')
""",
}
ENGINES = ("moruna-vm", "moruna", "rust-pool", "duckdb", "polars", "ray", "spark", "dask")


def wr(path, value):
    with open(path, "w") as f:
        f.write(str(value))


def rd(path):
    try:
        return open(path).read()
    except OSError:
        return ""


def command(engine, out, tag):
    """The run's command, and where 0.7's report goes."""
    as_bench = ["setpriv", "--reuid=bench", "--regid=bench", "--init-groups", "--"]
    if engine == "moruna-vm":
        sys.path.insert(0, HERE)
        import gate_jobs
        job = f"/data/tmp/{tag}.job.json"
        digest = open(f"{STACK}/image.digest").read().strip()
        gate_jobs.write(job, "occupancy", [FILE], out, {"digest": digest, "location": "/images/gate"},
                        cpus=8, mem_gib=20.5)
        os.chmod(job, 0o644)
        report = f"/data/tmp/{tag}.report.json"
        return as_bench + [MORUNA07, "run", job, "--output", out, "--report", report, "--memory-gib", "24",
                           "--image-stack", STACK], report
    if engine == "rust-pool":
        files = f"/data/tmp/{tag}.files"
        wr(files, FILE)
        os.chmod(files, 0o644)
        return as_bench + [f"{BIN}/gate-baseline", "--job", "occupancy", "--files", files, "--out", out,
                           "--threads", "8"], None
    code = PROGRAMS[engine] % {"file": FILE}
    return as_bench + [PY[engine], "-I", "-c", "import sys; sys.path.insert(0, %r)\n" % HERE + code, out], None


def sample(engine, cold, i):
    tag = f"startup-{engine}-{'cold' if cold else 'warm'}-{i}"
    out = f"/data/out/{tag}"
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    os.chown(out, 1000, 1000)
    cg = f"{CG}/{tag}"
    if os.path.isdir(cg):
        os.rmdir(cg)
    os.makedirs(cg)
    wr(f"{cg}/cpuset.cpus", "0-7")
    wr(f"{cg}/memory.max", 27 << 30)
    cmd, report = command(engine, out, tag)
    env = {"PATH": "/opt/qemu-kvm/bin:/usr/bin:/bin", "HOME": "/home/bench", "TMPDIR": "/data/tmp",
           "MORUNA_NODE_KIT": os.environ.get("GATE_NODE_KIT", "/data/boot-gate"), "JAVA_HOME": "/usr/lib/jvm/java-17-openjdk-amd64",
           "OPENBLAS_NUM_THREADS": "1", "PYTHONPATH": HERE}
    if cold:
        subprocess.run(["sync"])
        wr("/proc/sys/vm/drop_caches", 3)
    marks, stop = {}, threading.Event()
    t0 = time.time()
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env, text=True, cwd="/tmp",
                            preexec_fn=lambda: wr(f"{cg}/cgroup.procs", os.getpid()))

    def read():
        for line in proc.stdout:
            now = time.time()
            if line.startswith("MARK "):
                _, what, at = line.split()
                marks[what] = float(at)
            elif '"exit"' in line and "last" not in marks:
                marks["last"] = now  # 0.7: the job's exit update
    th = threading.Thread(target=read)
    th.start()
    exited = None
    while True:
        if exited is None and proc.poll() is not None:
            exited = time.time()
        if exited is not None and not rd(f"{cg}/cgroup.procs").strip():
            gone = time.time()
            break
        if time.time() - t0 > 900:
            wr(f"{cg}/cgroup.kill", 1)
        time.sleep(0.02)
    th.join()
    rec = {"engine": engine, "cold": cold, "run": i, "exit_code": proc.returncode,
           "first_result_s": exited - t0, "gone_s": gone - t0}
    if engine == "moruna-vm":
        try:
            vm = json.load(open(report)).get("vm") or {}
        except (OSError, ValueError):
            vm = {}
        rec["vm"] = vm
        rec["start_s"] = sum(vm.get(k) or 0 for k in ("launch_s", "boot_s", "stage_s")) or None
        rec["start_parts"] = {k: vm.get(k) for k in ("launch_s", "boot_s", "stage_s")}
    elif engine == "rust-pool":
        rec["start_s"] = None  # measured by a run of no files, below
    else:
        rec["start_s"] = marks["ready"] - t0 if "ready" in marks else None
    rec["shutdown_s"] = gone - marks["last"] if "last" in marks else None
    rec["rows_ok"] = bool(os.listdir(out))
    try:
        os.rmdir(cg)
    except OSError:
        pass
    shutil.rmtree(out, ignore_errors=True)
    return rec


def pool_start(cold):
    """The Rust pool's start: its process and its open, a run of no files."""
    empty = "/data/tmp/startup-empty.files"
    wr(empty, "")
    os.chmod(empty, 0o644)
    if cold:
        subprocess.run(["sync"])
        wr("/proc/sys/vm/drop_caches", 3)
    t0 = time.time()
    subprocess.run(["setpriv", "--reuid=bench", "--regid=bench", "--init-groups", "--", f"{BIN}/gate-baseline",
                    "--job", "occupancy", "--files", empty, "--out", "/data/tmp/startup-empty-out"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return time.time() - t0


def summary(recs):
    def stat(xs):
        xs = [x for x in xs if x is not None]
        return {"median": statistics.median(xs), "min": min(xs), "max": max(xs)} if xs else None
    out = {}
    for e in ENGINES:
        for cold in (True, False):
            rs = [r for r in recs if r["engine"] == e and r["cold"] == cold]
            if rs:
                out[f"{e} {'cold' if cold else 'warm'}"] = {
                    k: stat([r.get(k) for r in rs]) for k in ("start_s", "first_result_s", "shutdown_s")}
    return out


def main(runs):
    os.makedirs(OUT, exist_ok=True)
    os.makedirs(CG, exist_ok=True)
    recs = []
    for i in range(runs):
        for cold in (True, False):
            for e in ENGINES:
                r = sample(e, cold, i)
                if e == "rust-pool":
                    r["start_s"] = pool_start(cold)
                recs.append(r)
                json.dump(r, open(f"{OUT}/{e}-{'cold' if cold else 'warm'}-{i}.json", "w"))
                print(time.strftime("%H:%M:%S"), json.dumps({k: r.get(k) for k in
                      ("engine", "cold", "run", "exit_code", "start_s", "first_result_s", "shutdown_s")}), flush=True)
    s = summary(recs)
    json.dump(s, open(f"{OUT}/summary.json", "w"), indent=1)
    print(json.dumps(s, indent=1))


if __name__ == "__main__":
    main(int(sys.argv[1]) if len(sys.argv) > 1 else 5)
