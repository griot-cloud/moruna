"""One timed run of one engine on one workload. Prints one JSON line. Runs inside the cgroup the driver made."""
import argparse, json, os, sys, time, glob, threading
sys.path.insert(0, "/home/bench/bench")
import pyarrow as pa, pyarrow.parquet as pq
import workloads as w
import ooc_workloads as ooc

if __name__ == "__main__":

    p = argparse.ArgumentParser()
    p.add_argument("--engine", required=True); p.add_argument("--workload", required=True)
    p.add_argument("--files", required=True); p.add_argument("--out", required=True)
    p.add_argument("--threads", type=int, default=0); p.add_argument("--batch", type=int, default=0)
    p.add_argument("--staging", default="/data/no-staging"); p.add_argument("--report", default="")
    p.add_argument("--no-guard", action="store_true")
    p.add_argument("--preferred-rows", type=int, default=0)
    p.add_argument("--trace", default="")
    p.add_argument("--profiles", default="")
    p.add_argument("--cost", default="")
    p.add_argument("--engine-cpu", type=float, default=0)
    p.add_argument("--defer", action="store_true")
    a = p.parse_args()
    files = open(a.files).read().split()
    fn = getattr(w, a.workload, None) or getattr(ooc, a.workload)
    if a.defer:
        # Diagnosis: deferred reference counting on what the function reads from its module.
        import ctypes
        _defer = ctypes.pythonapi.PyUnstable_Object_EnableDeferredRefcount
        _defer.argtypes = [ctypes.py_object]; _defer.restype = ctypes.c_int
        _n = 0
        import builtins as _b
        _mods = (w, ooc, _b) if os.environ.get("DEFER_BUILTINS") else (w, ooc)
        for _mod in _mods:
            for _v in list(vars(_mod).values()):
                try: _n += _defer(_v)
                except Exception: pass
        DEFERRED = _n
    os.makedirs(a.out, exist_ok=True)
    res = {"engine": a.engine, "workload": a.workload, "python": sys.version.split()[0],
           "gil": getattr(sys, "_is_gil_enabled", lambda: True)(), "cpus_visible": len(os.sched_getaffinity(0))}
    if a.defer: res["deferred"] = DEFERRED
    t0 = time.perf_counter()

    CPUS = len(os.sched_getaffinity(0))
    def cg_limit():
        rel = open("/proc/self/cgroup").read().strip().split("::")[-1]
        v = open(f"/sys/fs/cgroup{rel}/memory.max").read().strip()
        return None if v == "max" else int(v)
    LIMIT = cg_limit() or (16 << 30)
    SPARK_SCHEMA = {"audit": "vendor_id long, pu_location_id long, fare_amount double, expected_fare double, flagged boolean",
                    "score": "vendor_id long, pu_location_id long, fare_amount double, predicted_tip double",
                    "occupancy": "zone long, minute_of_day long, car_minutes long"}

    if a.engine in ("moruna", "morunang", "moruna065"):
        import moruna
        res["version"] = moruna.__version__
        try:
            r = moruna.run(moruna.ParquetSource(files), [moruna.function(fn, memory_guard=not a.no_guard, **({"preferred_rows": a.preferred_rows} if a.preferred_rows else {}))], moruna.ParquetSink(a.out),
                           staging_dir=a.staging, **({"trace": a.trace} if a.trace else {}),
                           **({"profiles_dir": a.profiles} if a.profiles else {}),
                           **({"cost": moruna.CostSession(a.cost)} if a.cost else {}),
                           **({"cpu": a.engine_cpu} if a.engine_cpu else {}))
            d = r.to_dict()
        except Exception as e:  # a failed run is a result
            d = getattr(e, "report", None); d = d.to_dict() if hasattr(d, "to_dict") else (d or {})
            res["error"] = f"{type(e).__name__}: {e}"[:500]
        res["wall_s"] = time.perf_counter() - t0
        if a.report: json.dump(d, open(a.report, "w"), default=str)
        fns = [t for t in d.get("tasks", []) if t.get("role") == "function"]
        res["moruna"] = {k: d.get(k) for k in ("exit", "worker_busy_fraction", "cpu_throttled_fraction", "peak_fraction_of_ceiling",
                         "peak_anon_bytes", "peak_ceiling_bytes", "staging_engaged", "staging_bytes_written",
                         "bottleneck_timeline", "limits_initial", "limits_timeline", "host")}
        res["moruna"]["resources"] = d.get("resources")
        res["moruna"]["function"] = {k: fns[0].get(k) for k in ("instances", "morsels_in", "rows_in", "rows_out", "busy_ms",
                                     "input_wait_ms", "output_wait_ms", "amplification_p95_ratio")} if fns else None
        res["moruna"]["notes"] = d.get("notes")

    elif a.engine in ("loop", "loophooked"):
        if a.engine == "loophooked":
            # Diagnosis: the plain loop in a process where Moruna has run once, so its Python
            # allocator hooks are installed.
            import moruna
            _t = pa.table({"x": pa.array([1, 2, 3])}); _d = f"{a.out}-warm"; os.makedirs(_d, exist_ok=True)
            pq.write_table(_t, f"{_d}/in.parquet")
            moruna.run(moruna.ParquetSource(f"file://{_d}/in.parquet"), [moruna.function(lambda b: b)], moruna.ParquetSink(f"file://{_d}/out"))
            t0 = time.perf_counter()
        from concurrent.futures import ThreadPoolExecutor
        # A unit is a run of consecutive row groups holding at least one batch of rows, written to
        # one file: one row group per unit when a row group holds a batch (the NYC files), and
        # several when row groups are small (FineWeb's hold a thousand documents each).
        units = []
        for f in files:
            md = pq.ParquetFile(f).metadata; cur, n = [], 0
            for g in range(md.num_row_groups):
                cur.append(g); n += md.row_group(g).num_rows
                if n >= (a.batch or 1): units.append((f, cur)); cur, n = [], 0
            if cur: units.append((f, cur))
        def one(i):
            f, gs = units[i]
            t = pq.ParquetFile(f).read_row_groups(gs)
            if len(gs) > 1: t = t.combine_chunks()
            writer = None
            for b in t.to_batches(max_chunksize=a.batch or None):
                o = fn(b)
                if writer is None: writer = pq.ParquetWriter(f"{a.out}/{i:05d}.parquet", o.schema, compression="zstd")
                writer.write_batch(o)
            if writer: writer.close()
        with ThreadPoolExecutor(a.threads) as ex: list(ex.map(one, range(len(units))))
        res["wall_s"] = time.perf_counter() - t0

    elif a.engine == "polars":
        import polars as pl
        res["version"] = pl.__version__
        writer = None
        lf = pl.scan_parquet(files)
        for df in lf.collect_batches(chunk_size=a.batch or 1_000_000):
            t = df.to_arrow()
            if t.num_rows == 0: continue
            o = fn(t.combine_chunks().to_batches()[0])
            if writer is None: writer = pq.ParquetWriter(f"{a.out}/part.parquet", o.schema, compression="zstd")
            writer.write_batch(o)
        if writer: writer.close()
        res["wall_s"] = time.perf_counter() - t0

    elif a.engine == "duckdb":
        import duckdb
        res["version"] = duckdb.__version__
        con = duckdb.connect()
        con.execute(f"SET threads={len(os.sched_getaffinity(0))}")
        lim = open("/proc/self/cgroup").read().strip().split("::")[-1]
        mx = open(f"/sys/fs/cgroup{lim}/memory.max").read().strip()
        if mx != "max": con.execute(f"SET memory_limit='{int(mx) * 8 // 10 // (1 << 20)}MiB'")
        flist = ",".join(f"'{f}'" for f in files)
        reader = con.sql(f"SELECT * FROM read_parquet([{flist}])").fetch_record_batch(a.batch or 1_000_000)
        writer = None
        for b in reader:
            if b.num_rows == 0: continue
            o = fn(b)
            if writer is None: writer = pq.ParquetWriter(f"{a.out}/part.parquet", o.schema, compression="zstd")
            writer.write_batch(o)
        if writer: writer.close()
        res["wall_s"] = time.perf_counter() - t0

    elif a.engine == "spark":
        os.environ["PYSPARK_PYTHON"] = sys.executable; os.environ["PYSPARK_DRIVER_PYTHON"] = sys.executable
        from pyspark.sql import SparkSession
        import pyspark
        res["version"] = pyspark.__version__
        spark = (SparkSession.builder.master(f"local[{CPUS}]").appName("bench")
                 .config("spark.driver.memory", f"{int(LIMIT * 0.5) >> 20}m")
                 .config("spark.sql.execution.arrow.maxRecordsPerBatch", "250000")
                 .config("spark.sql.session.timeZone", "UTC").config("spark.ui.enabled", "false")
                 .config("spark.local.dir", "/data/spark-tmp")
                 .config("spark.sql.parquet.compression.codec", "zstd").getOrCreate())
        spark.sparkContext.setLogLevel("ERROR")
        res["start_s"] = time.perf_counter() - t0
        df = spark.read.parquet(*files)
        df.mapInArrow(lambda it: (fn(b) for b in it), SPARK_SCHEMA[a.workload]).write.mode("overwrite").parquet(a.out)
        res["wall_s"] = time.perf_counter() - t0
        spark.stop()

    elif a.engine == "ray":
        import ray
        res["version"] = ray.__version__
        ray.init(num_cpus=CPUS, object_store_memory=int(LIMIT * 0.3), include_dashboard=False, logging_level="ERROR",
                 log_to_driver=False, _temp_dir="/data/ray-tmp")
        ctx = ray.data.DataContext.get_current(); ctx.enable_progress_bars = False
        res["start_s"] = time.perf_counter() - t0
        def rfn(t):
            bs = t.combine_chunks().to_batches()
            b = bs[0] if bs else pa.RecordBatch.from_arrays([pa.array([], type=f.type) for f in t.schema], schema=t.schema)
            return pa.Table.from_batches([fn(b)])
        (ray.data.read_parquet(files).map_batches(rfn, batch_format="pyarrow", batch_size=a.batch or 250_000)
            .write_parquet(a.out, compression="zstd"))
        res["wall_s"] = time.perf_counter() - t0
        ray.shutdown()

    elif a.engine == "dask":
        import dask, dask.dataframe as dd
        from dask.distributed import LocalCluster, Client
        res["version"] = dask.__version__
        cluster = LocalCluster(n_workers=CPUS, threads_per_worker=1, processes=True, memory_limit=int(LIMIT * 0.8 / CPUS),
                               dashboard_address=None, local_directory="/data/dask-tmp")
        client = Client(cluster)
        res["start_s"] = time.perf_counter() - t0
        ddf = dd.read_parquet(files)
        meta = fn(pa.RecordBatch.from_pandas(ddf.head(1000), preserve_index=False)).to_pandas().iloc[:0]
        def part(pdf):
            t = pa.Table.from_pandas(pdf, preserve_index=False).combine_chunks()
            outs = [fn(b) for b in t.to_batches(max_chunksize=250_000)]
            if not outs:
                return meta
            return pa.Table.from_batches(outs).to_pandas()
        ddf.map_partitions(part, meta=meta).to_parquet(a.out, write_index=False, compression="zstd")
        res["wall_s"] = time.perf_counter() - t0
        client.close(); cluster.close()

    elif a.engine == "duckdbudf":
        import duckdb, duckdb.sqltypes as T
        res["version"] = duckdb.__version__
        con = duckdb.connect()
        con.execute(f"SET threads={CPUS}"); con.execute(f"SET memory_limit='{int(LIMIT * 0.8) >> 20}MiB'")
        cols = ["vendor_id", "pickup_at", "dropoff_at", "passenger_count", "trip_distance", "ratecode_id",
                "pu_location_id", "do_location_id", "fare_amount"]
        types = [T.BIGINT, T.TIMESTAMP, T.TIMESTAMP, T.DOUBLE, T.DOUBLE, T.DOUBLE, T.BIGINT, T.BIGINT, T.DOUBLE]
        target = {"audit": "expected_fare", "score": "predicted_tip"}[a.workload]
        def udf(c0, c1, c2, c3, c4, c5, c6, c7, c8):
            arrays = (c0, c1, c2, c3, c4, c5, c6, c7, c8)
            b = pa.record_batch([x.combine_chunks() if isinstance(x, pa.ChunkedArray) else x for x in arrays], names=cols)
            return fn(b)[target]
        con.create_function("f", udf, types, T.DOUBLE, type="arrow", null_handling="special")
        flist = ",".join(f"'{f}'" for f in files)
        flag = (", fare_amount IS NOT NULL AND abs(fare_amount - expected_fare) > greatest(5.0, 0.25 * expected_fare) AS flagged"
                if a.workload == "audit" else "")
        inner = f"SELECT vendor_id, pu_location_id, fare_amount, f({','.join(cols)}) AS {target} FROM read_parquet([{flist}])"
        con.execute(f"COPY (SELECT *{flag} FROM ({inner})) TO '{a.out}/part.parquet' (FORMAT parquet, COMPRESSION zstd)")
        res["wall_s"] = time.perf_counter() - t0

    print("RESULT " + json.dumps(res, default=str), flush=True)
