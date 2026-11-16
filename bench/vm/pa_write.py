"""pyarrow's Parquet writer on the same table pqbench writes, for an x86 comparison."""
import io, resource, sys, time
import pyarrow as pa, pyarrow.parquet as pq
t = pa.concat_tables([pq.read_table(f) for f in sys.argv[1:]])
def cpu():
    r = resource.getrusage(resource.RUSAGE_SELF); return r.ru_utime + r.ru_stime
for r in range(2):
    c0 = cpu(); t0 = time.perf_counter(); b = io.BytesIO()
    pq.write_table(t, b, compression="zstd", row_group_size=1 << 20)
    if r: print(f"pyarrow zstd           cpu {cpu()-c0:.3f}s wall {time.perf_counter()-t0:.3f}s bytes {len(b.getvalue())/1e6:.1f}MB rows {t.num_rows}")
