"""The PM's group 5 (arrow-core-0.7.md section 9): a job as native expressions where it needs no
UDF. Enrich as DuckDB SQL and as Polars expressions, Occupancy as DuckDB SQL; Score and Audit have
none. Each must produce the Python function's rows: it passes the same canonical-hash check
(canon.py, gate-canon) as every other engine.

NaN is the one place the engines' comparisons differ from pyarrow's: DuckDB and Polars order NaN
above every number, where IEEE comparisons are false. A row with a NaN the Python function drops,
these drop explicitly.
"""

JOBS = {"duckdb-sql": ("enrich", "occupancy"), "polars-expr": ("enrich",)}


def duckdb_query(workload: str, src: str) -> str:
    if workload == "enrich":
        return f"""SELECT * EXCLUDE (dur_), dur_ AS duration_s, trip_distance / (dur_ / 3600.0) AS mph,
                   fare_amount / trip_distance AS fare_per_mile, tip_amount / fare_amount AS tip_share
            FROM (SELECT *, (epoch_us(dropoff_at) - epoch_us(pickup_at)) / 1000000.0 AS dur_ FROM {src})
            WHERE dur_ >= 60.0 AND dur_ <= 14400.0 AND trip_distance > 0.0 AND trip_distance < 100.0
              AND NOT isnan(trip_distance) AND fare_amount > 0.0 AND NOT isnan(fare_amount)"""
    if workload == "occupancy":
        # Python's minutes are floor divisions, its minute of the day a floor modulo.
        return f"""WITH t AS (
              SELECT pu_location_id AS zone, floor(epoch_us(pickup_at) / 60000000.0)::BIGINT AS a,
                     least(greatest(floor(epoch_us(dropoff_at) / 60000000.0)::BIGINT
                                    - floor(epoch_us(pickup_at) / 60000000.0)::BIGINT, 0), 240) + 1 AS n
              FROM {src})
            SELECT zone, (((a + g) % 1440) + 1440) % 1440 AS minute_of_day, count(*)::BIGINT AS car_minutes
            FROM t, generate_series(0, n - 1) AS s(g) GROUP BY ALL"""
    raise ValueError(f"duckdb-sql has no SQL for {workload}: it needs a UDF")


def run(engine: str, workload: str, files, out: str, threads: int, memory_mib: int, output: dict) -> str:
    """Runs `workload` on `engine` from `files` into `out/part.parquet`; the engine's version."""
    if workload not in JOBS.get(engine, ()):
        raise ValueError(f"{engine} has no native form of {workload}")
    if engine == "duckdb-sql":
        import duckdb
        con = duckdb.connect()
        con.execute(f"SET threads={threads}")
        con.execute(f"SET memory_limit='{memory_mib}MiB'")
        src = "read_parquet([" + ",".join(f"'{f}'" for f in files) + "])"
        con.execute(f"COPY ({duckdb_query(workload, src)}) TO '{out}/part.parquet' (FORMAT parquet, "
                    f"COMPRESSION zstd, COMPRESSION_LEVEL {output['zstd_level']}, "
                    f"ROW_GROUP_SIZE {output['row_group_rows']})")
        return duckdb.__version__
    import polars as pl
    dist, fare = pl.col("trip_distance"), pl.col("fare_amount")
    # Polars divides by a scalar as a multiplication by its reciprocal (3501695176 / 1e6 comes
    # out 3501.6951759999997), which is not the IEEE division pyarrow and NumPy do. Dividing by
    # a column divides, so each divisor is a column of the constant, which costs one column.
    one = pl.col("pickup_at").is_null().cast(pl.Float64) * 0.0 + 1.0
    dur = (pl.col("dropoff_at").cast(pl.Int64) - pl.col("pickup_at").cast(pl.Int64)) / (one * 1_000_000.0)
    (pl.scan_parquet(list(files))
       .with_columns(duration_s=dur)
       .filter((pl.col("duration_s") >= 60.0) & (pl.col("duration_s") <= 14400.0) & (dist > 0.0)
               & (dist < 100.0) & dist.is_nan().not_() & (fare > 0.0) & fare.is_nan().not_())
       .with_columns(mph=dist / (pl.col("duration_s") / (one * 3600.0)), fare_per_mile=fare / dist,
                     tip_share=pl.col("tip_amount") / fare)
       .sink_parquet(f"{out}/part.parquet", compression="zstd", compression_level=output["zstd_level"],
                     row_group_size=output["row_group_rows"], engine="streaming"))
    return pl.__version__
