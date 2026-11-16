"""One-time prep: the 72 monthly NYC TLC yellow-taxi files into one schema, 1M-row row groups."""
import pyarrow as pa, pyarrow.parquet as pq, pyarrow.compute as pc, glob, os, sys
from concurrent.futures import ProcessPoolExecutor
SCHEMA = pa.schema([
    ("vendor_id", pa.int64()), ("pickup_at", pa.timestamp("us")), ("dropoff_at", pa.timestamp("us")),
    ("passenger_count", pa.float64()), ("trip_distance", pa.float64()), ("ratecode_id", pa.float64()),
    ("store_and_fwd_flag", pa.string()), ("pu_location_id", pa.int64()), ("do_location_id", pa.int64()),
    ("payment_type", pa.int64()), ("fare_amount", pa.float64()), ("extra", pa.float64()),
    ("mta_tax", pa.float64()), ("tip_amount", pa.float64()), ("tolls_amount", pa.float64()),
    ("improvement_surcharge", pa.float64()), ("total_amount", pa.float64()),
    ("congestion_surcharge", pa.float64()), ("airport_fee", pa.float64()),
])
SRC = {"vendorid": "vendor_id", "tpep_pickup_datetime": "pickup_at", "tpep_dropoff_datetime": "dropoff_at",
       "ratecodeid": "ratecode_id", "pulocationid": "pu_location_id", "dolocationid": "do_location_id"}
def one(path):
    t = pq.read_table(path)
    cols = {SRC.get(n.lower(), n.lower()): c for n, c in zip(t.column_names, t.columns)}
    arrays = [cols[f.name].cast(f.type) if f.name in cols else pa.nulls(t.num_rows, f.type) for f in SCHEMA]
    out = pa.Table.from_arrays(arrays, schema=SCHEMA)
    dst = "/data/nyc/norm/" + os.path.basename(path)
    pq.write_table(out, dst, row_group_size=1_000_000)
    return out.num_rows
if __name__ == "__main__":
    os.makedirs("/data/nyc/norm", exist_ok=True)
    files = sorted(glob.glob("/data/nyc/yellow/*.parquet"))
    with ProcessPoolExecutor(4) as ex:
        rows = sum(ex.map(one, files))
    print("normalized rows", rows)
