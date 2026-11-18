"""Enrich's filter as `moruna.filter@1` on a month, against 0.6.6's rows (work item 4.3).

Enrich (bench/vm/workloads.py) appends four columns to a batch and keeps the rows of its
predicate `ok`. Here pyarrow computes the four columns and `ok` exactly as `enrich` does, and
`moruna.filter@1` keeps the rows (the Rust test `enrich_month` in tests/enrich_month.rs). The
output's canonical hash (bench/vm/canon.py, gate-canon/1) must be 0.6.6's.

    python3 -I enrich_month.py prepare MONTH.parquet DIR      # DIR/enrich_in.arrow
    MORUNA_ENRICH_DIR=DIR cargo test -p moruna-kernels --test enrich_month -- --ignored
    python3 -I enrich_month.py verify DIR REFERENCE.json      # DIR/enrich_out.arrow

`prepare` also hashes pyarrow's own `filter` of the same batches, which must equal the
reference too: it shows the hashing here is canon's. The hash is a sum of row hashes, so it is
taken batch by batch with canon's own encoding, never holding the month as Python values.
"""
import hashlib
import json
import os
import struct
import sys

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.ipc as ipc
import pyarrow.parquet as pq

ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))
sys.path.insert(0, os.path.join(ROOT, "bench", "vm"))
import canon  # noqa: E402


def enrich_unfiltered(batch: pa.RecordBatch) -> pa.RecordBatch:
    """workloads.enrich, line for line, but returning the batch with `ok` instead of filtering."""
    dur = pc.divide(pc.cast(pc.subtract(batch["dropoff_at"], batch["pickup_at"]), pa.int64()), 1_000_000.0)
    dist, fare, tip = batch["trip_distance"], batch["fare_amount"], batch["tip_amount"]
    ok = pc.and_(pc.and_(pc.greater_equal(dur, 60.0), pc.less_equal(dur, 4 * 3600.0)),
                 pc.and_(pc.and_(pc.greater(dist, 0.0), pc.less(dist, 100.0)), pc.greater(fare, 0.0)))
    out = batch.append_column("duration_s", dur)
    out = out.append_column("mph", pc.divide(dist, pc.divide(dur, 3600.0)))
    out = out.append_column("fare_per_mile", pc.divide(fare, dist))
    out = out.append_column("tip_share", pc.divide(tip, fare))
    return out.append_column("ok", ok)


class Hash:
    """gate-canon/1's content hash of every column (Enrich has no tolerant column), by batch."""

    def __init__(self):
        self.rows = 0
        self.sum = 0
        self.schema = None

    def add(self, batch: pa.RecordBatch):
        self.schema = batch.schema
        names = batch.schema.names
        types = [batch.schema.field(c).type for c in names]
        values = [canon._values(pa.chunked_array([batch.column(i)])) for i in range(len(names))]
        for i in range(batch.num_rows):
            enc = bytearray()
            for t, v in zip(types, values):
                canon.encode_value(t, v[i], enc)
            self.sum = (self.sum + int.from_bytes(hashlib.sha256(enc).digest()[:16], "little")) & canon.MASK
        self.rows += batch.num_rows

    def digest(self) -> str:
        h = hashlib.sha256(canon.TAG + canon.signature(self.schema, self.schema.names))
        h.update(struct.pack("<Q", self.rows))
        h.update(self.sum.to_bytes(16, "little"))
        return h.hexdigest()


def prepare(month: str, out: str):
    os.makedirs(out, exist_ok=True)
    mine = Hash()
    writer = None
    with pa.OSFile(os.path.join(out, "enrich_in.arrow"), "wb") as sink:
        for batch in pq.ParquetFile(month).iter_batches(batch_size=1 << 16):
            full = enrich_unfiltered(batch)
            if writer is None:
                writer = ipc.new_file(sink, full.schema)
            writer.write_batch(full)
            mine.add(full.filter(full["ok"]).drop_columns(["ok"]))
        writer.close()
    result = {"rows": mine.rows, "hash_exact": mine.digest()}
    with open(os.path.join(out, "pyarrow_filter.json"), "w") as f:
        json.dump(result, f)
    print("pyarrow filter:", json.dumps(result))


def verify(out: str, reference: str) -> int:
    ref = json.load(open(reference))
    mine = Hash()
    with pa.memory_map(os.path.join(out, "enrich_out.arrow")) as source:
        reader = ipc.open_file(source)
        for i in range(reader.num_record_batches):
            mine.add(reader.get_batch(i))
    got = {"rows": mine.rows, "hash_exact": mine.digest()}
    want = {"rows": ref["rows"], "hash_exact": ref["hash_exact"]}
    same = got == want
    print(json.dumps({"moruna.filter@1": got, "moruna 0.6.6": want, "same": same}))
    return 0 if same else 1


if __name__ == "__main__":
    if sys.argv[1:2] == ["prepare"] and len(sys.argv) == 4:
        prepare(sys.argv[2], sys.argv[3])
    elif sys.argv[1:2] == ["verify"] and len(sys.argv) == 4:
        sys.exit(verify(sys.argv[2], sys.argv[3]))
    else:
        sys.exit(__doc__)
