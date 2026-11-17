"""One timed run of one Python engine on one job, for the node benchmark's Linux machine.

bench/vm/runner.py's engines, with its settings, on Linux or macOS: the CPUs and memory come from
the arguments (the run's cgroup on Linux has the same), not from /proc or /sys. Prints one line,
`RESULT {json}`, with the engine's version, every setting it was given, and four marks in
seconds of the wall clock (time.time()), from which measure.py takes start-up and operational time:

- `main`: the interpreter has started and runs this file's first line;
- `imported`: the engine's modules are imported;
- `ready`: the engine can take its first batch (Ray's init, Spark's session and Dask's cluster
  are up; for the others the same as `imported`), as bench/vm/startup.py's `start` ends;
- `done`: the output is written and closed. Shutting the engine down comes after.

    engine.py --engine E --workload W --files LIST --out DIR --cpus N --mem-gib G --tmp DIR
"""
import time

T_MAIN = time.time()

import argparse  # noqa: E402
import json  # noqa: E402
import os  # noqa: E402
import sys  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.environ.get("BENCH_VM", os.path.join(HERE, "..", "..", "vm")))

import pyarrow as pa  # noqa: E402
import pyarrow.parquet as pq  # noqa: E402

import workloads as w  # noqa: E402, bench/vm/workloads.py: the jobs' functions

# Control 3 (bench/vm/runner.py): zstd level 1, row groups of at most 1 Mi rows (128 MiB where the
# writer takes bytes).
OUTPUT = {"zstd_level": 1, "row_group_rows": 1 << 20, "row_group_bytes": 128 << 20, "file_bytes": 1 << 30}
SPARK_SCHEMA = {"audit": "vendor_id long, pu_location_id long, fare_amount double, expected_fare double, flagged boolean",
                "score": "vendor_id long, pu_location_id long, fare_amount double, predicted_tip double",
                "occupancy": "zone long, minute_of_day long, car_minutes long",
                "enrich": None}
ENGINES = ("moruna066", "moruna066-rust", "duckdb", "polars", "ray", "spark", "dask")


def pq_writer(path, schema):
    return pq.ParquetWriter(path, schema, compression="zstd", compression_level=OUTPUT["zstd_level"])


def write(writer, batch):
    writer.write_batch(batch, row_group_size=OUTPUT["row_group_rows"])


def moruna(a, fn, marks, res):
    import moruna as m
    marks["imported"] = marks["ready"] = time.time()
    res["version"] = m.__version__
    kw = {"staging_dir": a.staging} if a.staging else {}
    if a.profiles:
        kw["profiles_dir"] = a.profiles
    if a.cpus < (os.cpu_count() or a.cpus):
        # Only where the machine has more CPUs than the run may use (the Mac's check): on the
        # benchmark machine the run has them all, and 0.6.6 sees them, as bench/vm's runs did.
        kw["cpu"] = a.cpus
    res["settings"] = {"memory_guard": True, **kw}
    try:
        r = m.run(m.ParquetSource(a.files), [m.function(fn, memory_guard=True)], m.ParquetSink(a.out), **kw)
        d = r.to_dict()
    except Exception as e:  # a failed run is a result
        d = getattr(e, "report", None)
        d = d.to_dict() if hasattr(d, "to_dict") else (d or {})
        res["error"] = f"{type(e).__name__}: {e}"[:500]
    marks["done"] = time.time()
    if a.report:
        json.dump(d, open(a.report, "w"), default=str)
    from runner_split import split
    res["moruna"] = split(d)


def polars(a, fn, marks, res):
    import polars as pl
    marks["imported"] = marks["ready"] = time.time()
    res["version"] = pl.__version__
    chunk = a.batch or 1_000_000
    res["settings"] = {"collect_batches.chunk_size": chunk, "POLARS_MAX_THREADS": os.environ.get("POLARS_MAX_THREADS")}
    writer = None
    for df in pl.scan_parquet(a.files).collect_batches(chunk_size=chunk):
        t = df.to_arrow()
        if t.num_rows == 0:
            continue
        o = fn(t.combine_chunks().to_batches()[0])
        if writer is None:
            writer = pq_writer(f"{a.out}/part.parquet", o.schema)
        write(writer, o)
    if writer:
        writer.close()
    marks["done"] = time.time()


def duckdb(a, fn, marks, res):
    import duckdb as d
    marks["imported"] = time.time()
    res["version"] = d.__version__
    con = d.connect()
    limit_mib = int(a.mem_gib * (1 << 30)) * 8 // 10 // (1 << 20)
    con.execute(f"SET threads={a.cpus}")
    con.execute(f"SET memory_limit='{limit_mib}MiB'")
    marks["ready"] = time.time()
    rows = a.batch or 1_000_000
    res["settings"] = {"threads": a.cpus, "memory_limit_mib": limit_mib, "fetch_record_batch": rows}
    flist = ",".join(f"'{f}'" for f in a.files)
    reader = con.sql(f"SELECT * FROM read_parquet([{flist}])").fetch_record_batch(rows)
    writer = None
    for b in reader:
        if b.num_rows == 0:
            continue
        o = fn(b)
        if writer is None:
            writer = pq_writer(f"{a.out}/part.parquet", o.schema)
        write(writer, o)
    if writer:
        writer.close()
    marks["done"] = time.time()


def ray(a, fn, marks, res):
    import ray as r
    marks["imported"] = time.time()
    res["version"] = r.__version__
    store = int(a.mem_gib * (1 << 30) * 0.3)
    r.init(num_cpus=a.cpus, object_store_memory=store, include_dashboard=False, logging_level="ERROR",
           log_to_driver=False, _temp_dir=os.path.join(a.tmp, "ray"))
    ctx = r.data.DataContext.get_current()
    ctx.enable_progress_bars = False
    marks["ready"] = time.time()
    rows = a.batch or 250_000
    res["settings"] = {"num_cpus": a.cpus, "object_store_memory": store, "map_batches.batch_size": rows,
                       "batch_format": "pyarrow"}

    def rfn(t):
        bs = t.combine_chunks().to_batches()
        b = bs[0] if bs else pa.RecordBatch.from_arrays([pa.array([], type=f.type) for f in t.schema], schema=t.schema)
        return pa.Table.from_batches([fn(b)])

    (r.data.read_parquet(a.files).map_batches(rfn, batch_format="pyarrow", batch_size=rows)
        .write_parquet(a.out, compression="zstd", compression_level=OUTPUT["zstd_level"],
                       row_group_size=OUTPUT["row_group_rows"]))
    marks["done"] = time.time()
    r.shutdown()


def spark(a, fn, marks, res):
    os.environ["PYSPARK_PYTHON"] = sys.executable
    os.environ["PYSPARK_DRIVER_PYTHON"] = sys.executable
    import pyspark
    from pyspark.sql import SparkSession
    marks["imported"] = time.time()
    res["version"] = pyspark.__version__
    conf = {"spark.driver.memory": f"{int(a.mem_gib * (1 << 30) * 0.5) >> 20}m",
            "spark.sql.execution.arrow.maxRecordsPerBatch": "250000",
            "spark.sql.session.timeZone": "UTC", "spark.ui.enabled": "false",
            "spark.local.dir": os.path.join(a.tmp, "spark"),
            "spark.sql.parquet.compression.codec": "zstd",
            "spark.hadoop.parquet.compression.codec.zstd.level": str(OUTPUT["zstd_level"]),
            "spark.hadoop.parquet.block.size": str(OUTPUT["row_group_bytes"])}
    b = SparkSession.builder.master(f"local[{a.cpus}]").appName("bench")
    for k, v in conf.items():
        b = b.config(k, v)
    session = b.getOrCreate()
    session.sparkContext.setLogLevel("ERROR")
    marks["ready"] = time.time()
    res["settings"] = {"master": f"local[{a.cpus}]", **conf}
    res["output_note"] = "Spark rolls row groups by bytes (parquet.block.size) and writes one file a task"
    schema = SPARK_SCHEMA[a.workload]
    if schema is None:
        raise ValueError(f"spark has no output schema for {a.workload}: bench/vm/runner.py ran it on the UDF jobs")
    df = session.read.parquet(*a.files)
    df.mapInArrow(lambda it: (fn(b) for b in it), schema).write.mode("overwrite").parquet(a.out)
    marks["done"] = time.time()
    session.stop()


def dask(a, fn, marks, res):
    import dask as dk
    import dask.dataframe as dd
    from dask.distributed import Client, LocalCluster
    marks["imported"] = time.time()
    res["version"] = dk.__version__
    per_worker = int(a.mem_gib * (1 << 30) * 0.8 / a.cpus)
    cluster = LocalCluster(n_workers=a.cpus, threads_per_worker=1, processes=True, memory_limit=per_worker,
                           dashboard_address=None, local_directory=os.path.join(a.tmp, "dask"))
    client = Client(cluster)
    marks["ready"] = time.time()
    res["settings"] = {"n_workers": a.cpus, "threads_per_worker": 1, "processes": True,
                       "memory_limit_per_worker": per_worker, "to_batches.max_chunksize": 250_000}
    ddf = dd.read_parquet(a.files)
    meta = fn(pa.RecordBatch.from_pandas(ddf.head(1000), preserve_index=False)).to_pandas().iloc[:0]

    def part(pdf):
        t = pa.Table.from_pandas(pdf, preserve_index=False).combine_chunks()
        outs = [fn(b) for b in t.to_batches(max_chunksize=250_000)]
        return pa.Table.from_batches(outs).to_pandas() if outs else meta

    ddf.map_partitions(part, meta=meta).to_parquet(a.out, write_index=False, compression="zstd",
                                                   compression_level=OUTPUT["zstd_level"],
                                                   row_group_size=OUTPUT["row_group_rows"])
    marks["done"] = time.time()
    client.close()
    cluster.close()


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--engine", required=True, choices=ENGINES)
    p.add_argument("--workload", required=True)
    p.add_argument("--files", required=True, help="a file listing the inputs, one a line")
    p.add_argument("--out", required=True)
    p.add_argument("--cpus", type=int, required=True)
    p.add_argument("--mem-gib", type=float, required=True)
    p.add_argument("--tmp", required=True)
    p.add_argument("--batch", type=int, default=0)
    p.add_argument("--staging", default="")
    p.add_argument("--profiles", default="")
    p.add_argument("--report", default="")
    a = p.parse_args(argv)
    a.files = open(a.files).read().split()
    os.makedirs(a.out, exist_ok=True)
    os.makedirs(a.tmp, exist_ok=True)
    if a.engine == "moruna066-rust":
        import moruna_gate
        fn = getattr(moruna_gate, a.workload)
    else:
        fn = getattr(w, a.workload)
    marks = {"main": T_MAIN}
    res = {"engine": a.engine, "workload": a.workload, "python": sys.version.split()[0],
           "function": "rust" if a.engine == "moruna066-rust" else "python", "output": dict(OUTPUT),
           "gil": getattr(sys, "_is_gil_enabled", lambda: True)(), "cpus": a.cpus, "mem_gib": a.mem_gib,
           "marks": marks}
    run = {"moruna066": moruna, "moruna066-rust": moruna}.get(a.engine) or globals()[a.engine]
    try:
        run(a, fn, marks, res)
    except Exception as e:  # a failed run is a result
        res["error"] = f"{type(e).__name__}: {e}"[:500]
    print("RESULT " + json.dumps(res, default=str), flush=True)
    return 1 if res.get("error") else 0


if __name__ == "__main__":
    sys.exit(main())
