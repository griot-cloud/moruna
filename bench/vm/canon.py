"""The gate's canonical form of a job's output, and its content hash (gate-canon/1).

Every engine's output of a job is reduced to the same canonical form before it is compared, so
the comparison does not depend on how an engine split its batches or files:

- A per-batch aggregate is aggregated again by its group keys (Occupancy: `zone` and
  `minute_of_day`, summing `car_minutes`). Every other gate function is row-wise.
- Each row is encoded column by column (`encode_value`) and hashed: the first 16 bytes of its
  SHA-256, read as a little-endian integer.
- The content hash is SHA-256 over the spec's tag, the columns' signature, the canonical row
  count and the sum of the row hashes modulo 2**128. The sum is a multiset hash: it is the same
  for the same rows in any order, as sorting the rows and hashing them would be, with no sort
  and in one pass.

Two hashes are taken: `hash_exact` over the columns that must match bit for bit, and `hash_all`
over every column. A job with a column named with a float tolerance (Score's `predicted_tip`)
passes when `hash_exact` matches and that column is within its tolerance, row by row, after the
rows are sorted by their exact-column hash and then by the tolerant values (`tolerant_rows`).

This file is the reference: pure Python, for the fixtures and small outputs. The harness runs
the Rust implementation of the same spec (`gate-canon`, `images/gate/functions/src/canon.rs`),
whose tests check it against this one. Usage: canon.py WORKLOAD DIR_OR_FILE...
"""
import hashlib
import json
import math
import struct
import sys

import pyarrow as pa
import pyarrow.parquet as pq

TAG = b"gate-canon/1\0"
MASK = (1 << 128) - 1
CANON_NAN = 0x7FF8000000000000

# Per workload: the columns compared within a tolerance (abs, rel), and the regrouping of a
# per-batch aggregate (keys, summed columns). Every other column must match bit for bit.
# gate-canon's table (images/gate/functions/src/canon.rs, `spec`) governs, and this one is the
# same (test_gate_harness.py checks it): Score's is one step of the sixth decimal, 1.1e-6, so a
# value NumPy rounds one way and another engine the other is within it.
SPECS = {
    "score": {"tolerant": {"predicted_tip": (1.1e-6, 0.0)}},
    "enrich": {},
    "audit": {},
    "occupancy": {"regroup": (["zone", "minute_of_day"], ["car_minutes"])},
    "ov_shape": {"tolerant": {"area_m2": (0.0, 1e-9), "perimeter_m": (0.0, 1e-9),
                              "lon": (1e-9, 0.0), "lat": (1e-9, 0.0),
                              "compactness": (0.0, 1e-9)}},
    "ov_attrs": {"tolerant": {"volume_m3": (0.0, 1e-12)}},
    "fw_minhash": {},
    "fw_clean": {},
}


def type_class(t: pa.DataType) -> str:
    """The coarse type a column is hashed under: engines differ in integer widths and in large
    or small offsets for the same values."""
    if pa.types.is_boolean(t):
        return "bool"
    if pa.types.is_signed_integer(t):
        return "int"
    if pa.types.is_unsigned_integer(t):
        return "uint"
    if pa.types.is_floating(t):
        return "float"
    if pa.types.is_string(t) or pa.types.is_large_string(t) or pa.types.is_string_view(t):
        return "str"
    if pa.types.is_binary(t) or pa.types.is_large_binary(t) or pa.types.is_binary_view(t):
        return "bin"
    if pa.types.is_timestamp(t):
        return f"ts:{t.unit}"
    if pa.types.is_list(t) or pa.types.is_large_list(t) or pa.types.is_fixed_size_list(t):
        return f"list<{type_class(t.value_type)}>"
    if pa.types.is_struct(t):
        return "struct<" + ",".join(f"{t.field(i).name}:{type_class(t.field(i).type)}"
                                    for i in range(t.num_fields)) + ">"
    raise TypeError(f"gate-canon/1 has no encoding for {t}")


def signature(schema: pa.Schema, cols) -> bytes:
    return b"".join(f"{c}\0{type_class(schema.field(c).type)}\0".encode() for c in cols)


def encode_value(t: pa.DataType, v, out: bytearray) -> None:
    """One value of type `t`, as gate-canon/1 encodes it: a null is one zero byte; anything
    else is a one byte, then integers (and timestamps) as 8 little-endian bytes, floats as
    their float64 bits with one NaN, booleans as one byte, strings and binaries as an 8-byte
    length and the bytes, lists as an 8-byte count and each item, structs as each field."""
    if v is None:
        out += b"\x00"
        return
    out += b"\x01"
    cls = type_class(t)
    if cls == "bool":
        out += b"\x01" if v else b"\x00"
    elif cls == "int" or cls.startswith("ts:"):
        out += struct.pack("<q", v)
    elif cls == "uint":
        out += struct.pack("<Q", v)
    elif cls == "float":
        bits = CANON_NAN if math.isnan(v) else struct.unpack("<Q", struct.pack("<d", v))[0]
        out += struct.pack("<Q", bits)
    elif cls in ("str", "bin"):
        b = v.encode() if isinstance(v, str) else bytes(v)
        out += struct.pack("<Q", len(b)) + b
    elif cls.startswith("list<"):
        out += struct.pack("<Q", len(v))
        for x in v:
            encode_value(t.value_type, x, out)
    elif cls.startswith("struct<"):
        for i in range(t.num_fields):
            f = t.field(i)
            encode_value(f.type, v.get(f.name), out)
    else:
        raise TypeError(f"gate-canon/1 has no encoding for {t}")


def _values(col: pa.ChunkedArray):
    """A column's values as Python values, timestamps as their integers."""
    if pa.types.is_timestamp(col.type):
        return col.cast(pa.int64()).to_pylist()
    return col.to_pylist()


def regroup(table: pa.Table, keys, sums) -> pa.Table:
    acc = {}
    cols = {c: table[c].to_pylist() for c in keys + sums}
    for i in range(table.num_rows):
        k = tuple(cols[c][i] for c in keys)
        vals = [cols[c][i] for c in sums]
        cur = acc.get(k)
        acc[k] = vals if cur is None else [a + b for a, b in zip(cur, vals)]
    arrays = {c: [k[j] for k in acc] for j, c in enumerate(keys)}
    for j, c in enumerate(sums):
        arrays[c] = [v[j] for v in acc.values()]
    return pa.table({c: pa.array(arrays[c], table.schema.field(c).type) for c in keys + sums})


def _total_key(v):
    """Floats in IEEE total order, nulls first."""
    if v is None:
        return (0, 0)
    bits = struct.unpack("<q", struct.pack("<d", v))[0]
    return (1, bits ^ 0x7FFFFFFFFFFFFFFF if bits < 0 else bits)


def canonical(workload: str, table: pa.Table) -> dict:
    """The canonical row count, the two hashes and the tolerant rows of `table`."""
    spec = SPECS[workload]
    if "regroup" in spec:
        table = regroup(table, *spec["regroup"])
    names = table.schema.names
    tolerant = [c for c in names if c in spec.get("tolerant", {})]
    exact = [c for c in names if c not in tolerant]
    types = {c: table.schema.field(c).type for c in names}
    values = {c: _values(table[c]) for c in names}
    sum_exact = sum_all = 0
    rows_tol = []
    for i in range(table.num_rows):
        enc = bytearray()
        for c in exact:
            encode_value(types[c], values[c][i], enc)
        enc_all = bytearray()
        for c in names:
            encode_value(types[c], values[c][i], enc_all)
        d_exact = int.from_bytes(hashlib.sha256(enc).digest()[:16], "little")
        d_all = int.from_bytes(hashlib.sha256(enc_all).digest()[:16], "little")
        sum_exact = (sum_exact + d_exact) & MASK
        sum_all = (sum_all + d_all) & MASK
        if tolerant:
            tol_vals = [values[c][i] for c in tolerant]
            rows_tol.append((d_exact, tuple(_total_key(v) for v in tol_vals), tol_vals))

    def digest(cols, s):
        h = hashlib.sha256(TAG + signature(table.schema, cols))
        h.update(struct.pack("<Q", table.num_rows))
        h.update(s.to_bytes(16, "little"))
        return h.hexdigest()

    rows_tol.sort(key=lambda r: (r[0], r[1]))
    return {"rows": table.num_rows, "hash_exact": digest(exact, sum_exact),
            "hash_all": digest(names, sum_all), "exact": exact, "tolerant": tolerant,
            "tolerant_rows": [r[2] for r in rows_tol]}


def within(a, b, tol) -> bool:
    abs_tol, rel_tol = tol
    if a is None or b is None:
        return a is None and b is None
    if math.isnan(a) or math.isnan(b):
        return math.isnan(a) and math.isnan(b)
    if math.isinf(a) or math.isinf(b):
        return a == b
    return abs(a - b) <= abs_tol + rel_tol * max(abs(a), abs(b))


def compare(workload: str, a: dict, b: dict) -> dict:
    """Whether two canonical forms of one workload's output match: bit for bit, or within the
    named tolerances. Also how many tolerant values differ at all, and the largest gap."""
    tol = SPECS[workload].get("tolerant", {})
    out = {"rows": a["rows"] == b["rows"], "hash_exact": a["hash_exact"] == b["hash_exact"],
           "hash_all": a["hash_all"] == b["hash_all"], "differing": 0, "max_abs": 0.0}
    ok = out["rows"] and out["hash_exact"]
    if ok and not out["hash_all"]:
        for ra, rb in zip(a["tolerant_rows"], b["tolerant_rows"]):
            for c, x, y in zip(a["tolerant"], ra, rb):
                if not within(x, y, tol[c]):
                    ok = False
                same_nan = x is not None and y is not None and math.isnan(x) and math.isnan(y)
                if x != y and not same_nan:
                    out["differing"] += 1
                    if x is not None and y is not None and math.isfinite(x) and math.isfinite(y):
                        out["max_abs"] = max(out["max_abs"], abs(x - y))
    out["ok"] = ok
    return out


def read_output(paths) -> pa.Table:
    import glob
    import os
    files = []
    for p in paths:
        files += sorted(glob.glob(f"{p}/**/*.parquet", recursive=True)) if os.path.isdir(p) else [p]
    return pa.concat_tables([pq.read_table(f) for f in files]) if files else None


if __name__ == "__main__":
    c = canonical(sys.argv[1], read_output(sys.argv[2:]))
    c.pop("tolerant_rows")
    print(json.dumps(c))
