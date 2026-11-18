"""One timed run of one Python engine on one job, for the node benchmark's Linux machine.

bench/vm/runner.py's engines, with its settings, plus the native-expression rows of
bench/vm/native_exprs.py (DuckDB SQL for Enrich and Occupancy, Polars expressions for Enrich).
On Linux or macOS: the CPUs and memory come from the arguments (the run's cgroup on Linux has the
same), not from /proc or /sys.

**Data.** With a store in the environment (store.py: `BENCH_ACCOUNT` and the rest), `--inputs`
lists blob names and `--out` is a blob prefix, and the engine reads and writes the run's Blob
container with its own reader, inside operational time. Without one, they are local paths (the
tests, and a check with no emulator).

**Marks,** in seconds of the wall clock (time.time()), from which measure.py takes start-up and
operational time (the PM's answers, 2026-11-18):

- `main`: the interpreter runs this file's first line;
- `imported`: the engine's modules are imported;
- `ready`: its session or cluster is up (Ray's init, Spark's session, Dask's cluster, DuckDB's
  connection with its Azure secret; for the others the same as `imported`);
- `start`: the job is handed to the ready engine: the call that starts it is made next;
- `done`: the output is durably written: the call or the writer's close has returned.

Shutting the engine down comes after `done`. Prints one line, `RESULT {json}`, with the engine's
version, every setting it was given and the marks.

    engine.py --engine E --workload W --inputs LIST --out DIR_OR_PREFIX --cpus N --mem-gib G --tmp DIR [--root ENGINES]
"""
import time

T_MAIN = time.time()

import argparse  # noqa: E402
import json  # noqa: E402
import os  # noqa: E402
import sys  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
# bench/vm for the jobs' functions. Never bench/node itself: its azure.py would stand in for the
# Azure SDK's `azure` package, which adlfs imports.
sys.path.insert(0, os.environ.get("BENCH_VM", os.path.join(HERE, "..", "..", "vm")))
sys.path.insert(1, HERE)  # run with -I, which leaves the script's own directory out

import pyarrow as pa  # noqa: E402
import pyarrow.parquet as pq  # noqa: E402

import store as stores  # noqa: E402, store.py beside this file: the run's Blob container
import workloads as w  # noqa: E402, bench/vm/workloads.py: the jobs' functions

# Control 3 (bench/vm/runner.py): zstd level 1, row groups of at most 1 Mi rows (128 MiB where the
# writer takes bytes).
OUTPUT = {"zstd_level": 1, "row_group_rows": 1 << 20, "row_group_bytes": 128 << 20, "file_bytes": 1 << 30}
# Spark's output schemas: bench/vm/runner.py's for the UDF jobs; Enrich's is 0.6.6's Enrich output
# schema (its January and 2024 references, `schema`), in Spark's names: a timestamp without a time
# zone is `timestamp_ntz`, which is how Spark reads the inputs' (spark_ddl).
SPARK_SCHEMA = {"audit": "vendor_id long, pu_location_id long, fare_amount double, expected_fare double, flagged boolean",
                "score": "vendor_id long, pu_location_id long, fare_amount double, predicted_tip double",
                "occupancy": "zone long, minute_of_day long, car_minutes long",
                "enrich": ("vendor_id long, pickup_at timestamp_ntz, dropoff_at timestamp_ntz, passenger_count double, "
                           "trip_distance double, ratecode_id double, store_and_fwd_flag string, pu_location_id long, "
                           "do_location_id long, payment_type long, fare_amount double, extra double, mta_tax double, "
                           "tip_amount double, tolls_amount double, improvement_surcharge double, total_amount double, "
                           "congestion_surcharge double, airport_fee double, duration_s double, mph double, "
                           "fare_per_mile double, tip_share double")}
ENGINES = ("moruna066", "moruna066-rust", "duckdb", "duckdb-sql", "polars", "polars-expr", "ray", "spark", "dask")


def spark_ddl(schema: pa.Schema) -> str:
    """An Arrow schema in Spark's DDL, as mapInArrow must be told the function's output."""
    def one(t):
        if pa.types.is_timestamp(t):
            return "timestamp" if t.tz else "timestamp_ntz"
        for test, name in ((pa.types.is_int64, "long"), (pa.types.is_int32, "int"), (pa.types.is_float64, "double"),
                           (pa.types.is_boolean, "boolean"), (pa.types.is_string, "string"),
                           (pa.types.is_large_string, "string")):
            if test(t):
                return name
        raise TypeError(f"no Spark type for {t}")
    return ", ".join(f"{f.name} {one(f.type)}" for f in schema)


class Writer:
    """pyarrow's Parquet writer, as bench/vm's runner wrote, onto a local file or a blob."""

    def __init__(self, a, store):
        self.a, self.store, self.w = a, store, None

    def write(self, batch):
        if self.w is None:
            kw = {}
            path = f"{self.a.out}/part.parquet"
            if self.store:
                kw["filesystem"] = self.store.pyarrow_fs()
                path = self.store.path(path)
            self.w = pq.ParquetWriter(path, batch.schema, compression="zstd",
                                      compression_level=OUTPUT["zstd_level"], **kw)
        self.w.write_batch(batch, row_group_size=OUTPUT["row_group_rows"])

    def close(self):
        if self.w:
            self.w.close()


def moruna(a, fn, marks, res, store):
    if store:
        os.environ.update(store.moruna_env())
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
    if store:
        source, sink = m.ParquetSource([store.az(n) for n in a.inputs]), m.ParquetSink(store.az(a.out) + "/")
        kw["storage"] = store.moruna_storage()
    else:
        source, sink = m.ParquetSource(a.inputs), m.ParquetSink(a.out)
    try:
        marks["start"] = time.time()
        r = m.run(source, [m.function(fn, memory_guard=True)], sink, **kw)
        marks["done"] = time.time()
        d = r.to_dict()
    except Exception as e:  # a failed run is a result
        d = getattr(e, "report", None)
        d = d.to_dict() if hasattr(d, "to_dict") else (d or {})
        res["error"] = f"{type(e).__name__}: {e}"[:500]
    if a.report:
        json.dump(d, open(a.report, "w"), default=str)
    from runner_split import split
    res["moruna"] = split(d)


def duckdb_session(a, res, store):
    import duckdb as d
    con = d.connect()
    limit_mib = int(a.mem_gib * (1 << 30)) * 8 // 10 // (1 << 20)
    con.execute(f"SET threads={a.cpus}")
    con.execute(f"SET memory_limit='{limit_mib}MiB'")
    res["version"] = d.__version__
    res["settings"] = {"threads": a.cpus, "memory_limit_mib": limit_mib}
    if store:
        # The azure extension installed by install.py, outside every timing.
        ext = os.path.join(a.root, "duckdb-ext") if a.root else None
        if ext:
            con.execute(f"SET extension_directory='{ext}'")
        con.execute("LOAD azure")
        con.execute(store.duckdb_secret())
        res["settings"]["azure"] = "the azure extension, a secret from the connection string"
    src = [store.az(n) for n in a.inputs] if store else a.inputs
    return con, "read_parquet([" + ",".join(f"'{f}'" for f in src) + "])"


def duckdb(a, fn, marks, res, store):
    import duckdb  # noqa: F401
    marks["imported"] = time.time()
    con, src = duckdb_session(a, res, store)
    marks["ready"] = time.time()
    rows = a.batch or 1_000_000
    res["settings"]["fetch_record_batch"] = rows
    out = Writer(a, store)
    marks["start"] = time.time()
    for b in con.sql(f"SELECT * FROM {src}").fetch_record_batch(rows):
        if b.num_rows:
            out.write(fn(b))
    out.close()
    marks["done"] = time.time()


def duckdb_sql(a, fn, marks, res, store):
    import native_exprs
    marks["imported"] = time.time()
    con, src = duckdb_session(a, res, store)
    marks["ready"] = time.time()
    dest = store.az(f"{a.out}/part.parquet") if store else f"{a.out}/part.parquet"
    marks["start"] = time.time()
    con.execute(f"COPY ({native_exprs.duckdb_query(a.workload, src)}) TO '{dest}' (FORMAT parquet, COMPRESSION zstd, "
                f"COMPRESSION_LEVEL {OUTPUT['zstd_level']}, ROW_GROUP_SIZE {OUTPUT['row_group_rows']})")
    marks["done"] = time.time()


def polars_scan(a, res, store):
    import polars as pl
    res["version"] = pl.__version__
    res["settings"] = {"POLARS_MAX_THREADS": os.environ.get("POLARS_MAX_THREADS")}
    if store:
        res["settings"]["storage_options"] = "object_store: account name and key"
        return pl.scan_parquet([store.az(n) for n in a.inputs], storage_options=store.polars_options())
    return pl.scan_parquet(a.inputs)


def polars(a, fn, marks, res, store):
    import polars  # noqa: F401
    marks["imported"] = marks["ready"] = time.time()
    chunk = a.batch or 1_000_000
    marks["start"] = time.time()
    lf = polars_scan(a, res, store)
    res["settings"]["collect_batches.chunk_size"] = chunk
    out = Writer(a, store)
    for df in lf.collect_batches(chunk_size=chunk):
        t = df.to_arrow()
        if t.num_rows:
            out.write(fn(t.combine_chunks().to_batches()[0]))
    out.close()
    marks["done"] = time.time()


def polars_expr(a, fn, marks, res, store):
    import native_exprs
    import polars  # noqa: F401
    marks["imported"] = marks["ready"] = time.time()
    if a.workload not in native_exprs.JOBS["polars-expr"]:
        raise ValueError(f"polars-expr has no native form of {a.workload}")
    marks["start"] = time.time()
    lf = native_exprs.polars_enrich(polars_scan(a, res, store))
    kw = {"storage_options": store.polars_options()} if store else {}
    dest = store.az(f"{a.out}/part.parquet") if store else f"{a.out}/part.parquet"
    lf.sink_parquet(dest, compression="zstd", compression_level=OUTPUT["zstd_level"],
                    row_group_size=OUTPUT["row_group_rows"], engine="streaming", **kw)
    marks["done"] = time.time()


def ray(a, fn, marks, res, store):
    import ray as r
    marks["imported"] = time.time()
    res["version"] = r.__version__
    store_bytes = int(a.mem_gib * (1 << 30) * 0.3)
    r.init(num_cpus=a.cpus, object_store_memory=store_bytes, include_dashboard=False, logging_level="ERROR",
           log_to_driver=False, _temp_dir=os.path.join(a.tmp, "ray"))
    ctx = r.data.DataContext.get_current()
    ctx.enable_progress_bars = False
    fs = store.pyarrow_fs() if store else None
    marks["ready"] = time.time()
    rows = a.batch or 250_000
    res["settings"] = {"num_cpus": a.cpus, "object_store_memory": store_bytes, "map_batches.batch_size": rows,
                       "batch_format": "pyarrow", "filesystem": "pyarrow AzureFileSystem" if fs else "local"}

    def rfn(t):
        bs = t.combine_chunks().to_batches()
        b = bs[0] if bs else pa.RecordBatch.from_arrays([pa.array([], type=f.type) for f in t.schema], schema=t.schema)
        return pa.Table.from_batches([fn(b)])

    paths = [store.path(n) for n in a.inputs] if store else a.inputs
    out = store.path(a.out) if store else a.out
    marks["start"] = time.time()
    (r.data.read_parquet(paths, filesystem=fs).map_batches(rfn, batch_format="pyarrow", batch_size=rows)
        .write_parquet(out, filesystem=fs, compression="zstd", compression_level=OUTPUT["zstd_level"],
                       row_group_size=OUTPUT["row_group_rows"]))
    marks["done"] = time.time()
    r.shutdown()


def spark(a, fn, marks, res, store):
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
            "spark.hadoop.parquet.block.size": str(OUTPUT["row_group_bytes"]),
            # The PM (2026-11-18): microsecond timestamps, so Enrich's output is 0.6.6's; it makes
            # the output the same, not faster.
            "spark.sql.parquet.outputTimestampType": "TIMESTAMP_MICROS"}
    if store:
        spark_install = installed(a).get("spark", {})
        conf.update({"spark.jars.packages": spark_install.get("packages", ""),
                     "spark.jars.ivy": spark_install.get("ivy", os.path.join(a.tmp, "ivy"))})
    b = SparkSession.builder.master(f"local[{a.cpus}]").appName("bench")
    for k, v in {**conf, **(store.spark_conf() if store else {})}.items():
        b = b.config(k, v)
    session = b.getOrCreate()
    session.sparkContext.setLogLevel("ERROR")
    marks["ready"] = time.time()
    res["settings"] = {"master": f"local[{a.cpus}]", **conf}
    res["output_note"] = "Spark rolls row groups by bytes (parquet.block.size) and writes one file a task"
    paths = [store.wasbs(n) for n in a.inputs] if store else a.inputs
    out = store.wasbs(a.out) if store else a.out
    marks["start"] = time.time()
    df = session.read.parquet(*paths)
    df.mapInArrow(lambda it: (fn(b) for b in it), SPARK_SCHEMA[a.workload]).write.mode("overwrite").parquet(out)
    marks["done"] = time.time()
    session.stop()


def dask(a, fn, marks, res, store):
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
                       "memory_limit_per_worker": per_worker, "to_batches.max_chunksize": 250_000,
                       "storage": "adlfs" if store else "local"}
    opts = {"storage_options": store.adlfs_options()} if store else {}
    paths = [store.abfs(n) for n in a.inputs] if store else a.inputs
    out = store.abfs(a.out) if store else a.out
    marks["start"] = time.time()
    ddf = dd.read_parquet(paths, **opts)
    meta = fn(pa.RecordBatch.from_pandas(ddf.head(1000), preserve_index=False)).to_pandas().iloc[:0]

    def part(pdf):
        t = pa.Table.from_pandas(pdf, preserve_index=False).combine_chunks()
        outs = [fn(b) for b in t.to_batches(max_chunksize=250_000)]
        return pa.Table.from_batches(outs).to_pandas() if outs else meta

    ddf.map_partitions(part, meta=meta).to_parquet(out, write_index=False, compression="zstd",
                                                   compression_level=OUTPUT["zstd_level"],
                                                   row_group_size=OUTPUT["row_group_rows"], **opts)
    marks["done"] = time.time()
    client.close()
    cluster.close()


def installed(a):
    try:
        return json.load(open(os.path.join(a.root, "installed.json")))
    except (OSError, TypeError, ValueError):
        return {}


RUN = {"moruna066": moruna, "moruna066-rust": moruna, "duckdb": duckdb, "duckdb-sql": duckdb_sql,
       "polars": polars, "polars-expr": polars_expr, "ray": ray, "spark": spark, "dask": dask}


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--engine", required=True, choices=ENGINES)
    p.add_argument("--workload", required=True)
    p.add_argument("--inputs", required=True, help="a file listing the inputs, one a line")
    p.add_argument("--out", required=True)
    p.add_argument("--cpus", type=int, required=True)
    p.add_argument("--mem-gib", type=float, required=True)
    p.add_argument("--tmp", required=True)
    p.add_argument("--root", default="", help="install.py's root, for what it installed beside the engines")
    p.add_argument("--batch", type=int, default=0)
    p.add_argument("--staging", default="")
    p.add_argument("--profiles", default="")
    p.add_argument("--report", default="")
    a = p.parse_args(argv)
    a.inputs = open(a.inputs).read().split()
    store = stores.Store.from_env()
    if not store:
        os.makedirs(a.out, exist_ok=True)
    os.makedirs(a.tmp, exist_ok=True)
    if a.engine == "moruna066-rust":
        import moruna_gate
        fn = getattr(moruna_gate, a.workload)
    else:
        fn = getattr(w, a.workload)
    marks = {"main": T_MAIN}
    res = {"engine": a.engine, "workload": a.workload, "python": sys.version.split()[0],
           "function": {"moruna066-rust": "rust", "duckdb-sql": "sql", "polars-expr": "expressions"}.get(a.engine, "python"),
           "output": dict(OUTPUT), "gil": getattr(sys, "_is_gil_enabled", lambda: True)(), "cpus": a.cpus,
           "mem_gib": a.mem_gib, "data": "blob" if store else "local", "marks": marks}
    try:
        RUN[a.engine](a, fn, marks, res, store)
    except Exception as e:  # a failed run is a result
        res["error"] = f"{type(e).__name__}: {e}"[:500]
    print("RESULT " + json.dumps(res, default=str), flush=True)
    return 1 if res.get("error") else 0


if __name__ == "__main__":
    sys.exit(main())
