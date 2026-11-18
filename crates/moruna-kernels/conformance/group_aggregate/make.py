"""Writes moruna.group_aggregate's corpus: its Arrow IPC files and index.json.

    python3 -I crates/moruna-kernels/conformance/group_aggregate/make.py [MONTH.parquet]

Every expected output is written out here by hand, or computed independently of the CPU
reference: the exact float sums with Python's Fraction (float(Fraction) rounds correctly, ties
to even) and their partial encodings from the same Fractions; Occupancy's counting step with
bench/vm/workloads.py's own np.unique. MONTH.parquet (default
~/moruna-data/nyc/norm/yellow_tripdata_2024-01.parquet) gives Occupancy's trips.
"""
import json
import os
import struct
import sys
from fractions import Fraction

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

HERE = os.path.dirname(os.path.abspath(__file__))
MONTH = os.path.expanduser("~/moruna-data/nyc/norm/yellow_tripdata_2024-01.parquet")
I64 = pa.int64()
F64 = pa.float64()
NAN = float("nan")
INF = float("inf")
SUM_META = {"moruna.group_aggregate.sum": "int64"}
FSUM_META = {"moruna.group_aggregate.sum": "float64"}


def schema(*fields):
    """(name, type, nullable[, metadata]) fields."""
    return pa.schema([pa.field(f[0], f[1], f[2], metadata=f[3] if len(f) > 3 else None)
                      for f in fields])


def batch(sch, *columns):
    return pa.record_batch([pa.array(c, type=f.type) for c, f in zip(columns, sch)], schema=sch)


def write(name, sch, *batches):
    with pa.OSFile(os.path.join(HERE, name), "wb") as sink:
        with pa.ipc.new_file(sink, sch) as w:
            for b in batches:
                w.write_batch(b)


def exact(values):
    return sum((Fraction(v) for v in values), Fraction(0))


def exact_bytes(values):
    """The canonical encoding of the exact sum of `values` (tag 0, the window)."""
    total = exact(values)
    if total == 0:
        s, e = 0, 0
    else:
        n, d = total.numerator, total.denominator
        k = d.bit_length() - 1
        assert d == 1 << k
        tz = (abs(n) & -abs(n)).bit_length() - 1
        s = n >> tz if n > 0 else -((-n) >> tz)
        e = 1074 - k + tz
        assert abs(s) < 1 << 126
    return bytes([0, 0]) + struct.pack("<i", e) + s.to_bytes(16, "little", signed=True)


CASES = []


def case(name, tests, about, params, inputs, expected=None, error=None):
    c = {"name": name, "version": 1, "tests": tests, "about": about, "params": params,
         "inputs": inputs}
    if error is not None:
        c["error"] = error
    else:
        c["expected"] = expected
    CASES.append(c)


def src(file, slice_=None):
    return {"file": file} if slice_ is None else {"file": file, "slice": list(slice_)}


# The trips: two keys, null keys, null values, a NaN; two batches.
TRIPS = schema(("zone", I64, True), ("minute", I64, True), ("fare", F64, True), ("n", I64, True))
write("trips.in.arrow", TRIPS,
      batch(TRIPS, [2, 1, 2, None, 1], [5, 5, 5, 0, 7], [1.5, 2.0, None, 4.0, 0.25],
            [10, -3, 7, None, 1]),
      batch(TRIPS, [1, None, 3], [5, 0, None], [-1.0, None, NAN], [None, 6, 2]))
EVERY = [
    {"op": "count", "as": "rows"},
    {"op": "count", "column": "fare", "as": "fares"},
    {"op": "sum", "column": "n", "as": "n_sum"},
    {"op": "min", "column": "fare", "as": "fare_min"},
    {"op": "max", "column": "fare", "as": "fare_max"},
    {"op": "mean", "column": "n", "as": "n_mean"},
    {"op": "sum", "column": "fare", "as": "fare_sum"},
    {"op": "mean", "column": "fare", "as": "fare_mean"},
]
EVERY_OUT = schema(("zone", I64, True), ("minute", I64, True), ("rows", I64, False),
                   ("fares", I64, False), ("n_sum", I64, True), ("fare_min", F64, True),
                   ("fare_max", F64, True), ("n_mean", F64, True), ("fare_sum", F64, True),
                   ("fare_mean", F64, True))
# By hand, in key order with nulls last: (1,5) (1,7) (2,5) (3,null) (null,0).
write("trips.expected.arrow", EVERY_OUT,
      batch(EVERY_OUT, [1, 1, 2, 3, None], [5, 7, 5, None, 0], [2, 1, 2, 1, 2], [2, 1, 1, 1, 1],
            [-3, 1, 17, 2, 6], [-1.0, 0.25, 1.5, NAN, 4.0], [2.0, 0.25, 1.5, NAN, 4.0],
            [-3.0, 1.0, 8.5, 2.0, 6.0], [1.0, 0.25, 1.5, NAN, 4.0], [0.5, 0.25, 1.5, NAN, 4.0]))
TWO_KEYS = {"keys": ["zone", "minute"], "aggregates": EVERY}
case("every_aggregate", ["nulls", "multi_batch", "ordering", "params"],
     "count, sum, min, max and mean over two batches on the hash path: null keys are groups and "
     "sort last, null values are skipped, NaN is a float's min and max.",
     TWO_KEYS, [src("trips.in.arrow")], "trips.expected.arrow")
case("every_aggregate_dense", ["nulls", "multi_batch", "params"],
     "The same on the dense path of a declared domain gives the same rows.",
     dict(TWO_KEYS, domain={"zone": [0, 9], "minute": [0, 1439]}), [src("trips.in.arrow")],
     "trips.expected.arrow")
case("dense_declined", ["nulls", "multi_batch", "params"],
     "Zone 3, outside its declared [0, 2], arrives in the second batch: the instance continues "
     "on the hash path and gives the same rows.",
     dict(TWO_KEYS, domain={"zone": [0, 2], "minute": [0, 1439]}), [src("trips.in.arrow")],
     "trips.expected.arrow")
# Rows 1 and 2 of each batch: (1,5,2.0,-3) (2,5,null,7) and (null,0,null,6) (3,null,NaN,2).
write("sliced.expected.arrow", EVERY_OUT,
      batch(EVERY_OUT, [1, 2, 3, None], [5, 5, None, 0], [1, 1, 1, 1], [1, 0, 1, 0],
            [-3, 7, 2, 6], [2.0, None, NAN, None], [2.0, None, NAN, None],
            [-3.0, 7.0, 2.0, 6.0], [2.0, None, NAN, None], [2.0, None, NAN, None]))
case("sliced", ["sliced", "nulls", "multi_batch"],
     "Batches whose arrays start at offset 1 are read where they lie: only their rows count.",
     TWO_KEYS, [src("trips.in.arrow", (1, 2))], "sliced.expected.arrow")
case("sliced_dense", ["sliced", "nulls", "params"],
     "The same slices on the dense path.",
     dict(TWO_KEYS, domain={"zone": [0, 9], "minute": [0, 1439]}),
     [src("trips.in.arrow", (1, 2))], "sliced.expected.arrow")

# Empty input.
KV = schema(("k", I64, True), ("v", F64, True))
COUNT_OUT = schema(("k", I64, True), ("n", I64, False))
write("empty.in.arrow", KV, batch(KV, [], []))
write("none.in.arrow", KV)
write("empty.expected.arrow", COUNT_OUT)
COUNT_K = {"keys": ["k"], "aggregates": [{"op": "count", "as": "n"}]}
case("empty_batch", ["empty"], "A batch of no rows adds no group: nothing at finish.",
     COUNT_K, [src("empty.in.arrow")], "empty.expected.arrow")
case("no_batch", ["empty"], "No batch at all: nothing at finish.",
     COUNT_K, [src("none.in.arrow")], "empty.expected.arrow")
case("empty_partial", ["empty", "params"], "No rows in partial mode: no partial result.",
     dict(COUNT_K, mode="partial", instances=4), [src("empty.in.arrow")], "empty.expected.arrow")

# A group of only nulls.
write("all_null.in.arrow", KV, batch(KV, [1, 1], [None, None]))
ALL_OUT = schema(("k", I64, True), ("rows", I64, False), ("values", I64, False),
                 ("sum", F64, True), ("min", F64, True), ("max", F64, True), ("mean", F64, True))
write("all_null.expected.arrow", ALL_OUT,
      batch(ALL_OUT, [1], [2], [0], [None], [None], [None], [None]))
case("all_null_values", ["nulls"],
     "A group whose values are all null counts 2 rows and 0 values; its sum, min, max and mean "
     "are null.",
     {"keys": ["k"], "aggregates": [
         {"op": "count", "as": "rows"}, {"op": "count", "column": "v", "as": "values"},
         {"op": "sum", "column": "v", "as": "sum"}, {"op": "min", "column": "v", "as": "min"},
         {"op": "max", "column": "v", "as": "max"}, {"op": "mean", "column": "v", "as": "mean"}]},
     [src("all_null.in.arrow")], "all_null.expected.arrow")

# Float keys.
X = schema(("x", F64, True))
NEG_NAN = struct.unpack("<d", struct.pack("<Q", 0xFFF8000000000001))[0]
write("float_keys.in.arrow", X, batch(X, [0.0, -0.0, NAN, NEG_NAN, None, -INF, INF]))
XN = schema(("x", F64, True), ("n", I64, False))
write("float_keys.expected.arrow", XN, batch(XN, [-INF, 0.0, INF, NAN, None], [1, 2, 1, 2, 1]))
case("float_keys", ["ordering", "nulls"],
     "-0.0 groups with 0.0 and every NaN with every other, after +infinity; null last.",
     {"keys": ["x"], "aggregates": [{"op": "count", "as": "n"}]}, [src("float_keys.in.arrow")],
     "float_keys.expected.arrow")

# String keys.
S = schema(("s", pa.string(), True))
write("strings.in.arrow", S, batch(S, ["b", None, "a", "é", "b", "B"]))
SN = schema(("s", pa.string(), True), ("n", I64, False))
write("strings.expected.arrow", SN, batch(SN, ["B", "a", "b", "é", None], [1, 1, 2, 1, 1]))
case("string_keys", ["ordering", "nulls"],
     "Strings group by value and sort by their bytes (B, a, b, é); null last.",
     {"keys": ["s"], "aggregates": [{"op": "count", "as": "n"}]}, [src("strings.in.arrow")],
     "strings.expected.arrow")

# Keys of several kinds on the row-format path: a string and a decimal.
SD = schema(("s", pa.string(), True), ("d", pa.decimal128(10, 2), True), ("v", pa.int32(), True))
sd = batch(SD, ["x", "x", "x"], [5, 5, -1], [1, 2, 4])
write("string_decimal.in.arrow", SD, sd, sd)
SDO = schema(("s", pa.string(), True), ("d", pa.decimal128(10, 2), True), ("t", I64, True))
write("string_decimal.expected.arrow", SDO, batch(SDO, ["x", "x"], [-1, 5], [8, 6]))
case("string_and_decimal_keys", ["ordering", "multi_batch"],
     "A string key and a decimal key together; an Int32 sum is Int64.",
     {"keys": ["s", "d"], "aggregates": [{"op": "sum", "column": "v", "as": "t"}]},
     [src("string_decimal.in.arrow")], "string_decimal.expected.arrow")

# Exact float sums.
VALS = [1e16, 1.0, -1e16, 0.1, 0.2, 1e-300]
KV1 = schema(("k", I64, True), ("v", F64, True))
write("float_sum.in.arrow", KV1, batch(KV1, [1] * 6, VALS))
write("float_sum_reversed.in.arrow", KV1, batch(KV1, [1] * 2, VALS[::-1][:2]),
      batch(KV1, [1] * 4, VALS[::-1][2:]))
SM = schema(("k", I64, True), ("s", F64, True), ("m", F64, True))
total = float(exact(VALS))
write("float_sum.expected.arrow", SM, batch(SM, [1], [total], [total / 6]))
SUM_MEAN = {"keys": ["k"], "aggregates": [{"op": "sum", "column": "v", "as": "s"},
                                          {"op": "mean", "column": "v", "as": "m"}]}
case("float_sum_exact", ["rounding"],
     "A float sum is the exact sum rounded once: 1e16 + 1 - 1e16 keeps its 1, and 1e-300 is not "
     "lost to it.", SUM_MEAN, [src("float_sum.in.arrow")], "float_sum.expected.arrow")
case("float_sum_any_order", ["rounding", "multi_batch"],
     "The same values reversed and in two batches give the same bits.",
     SUM_MEAN, [src("float_sum_reversed.in.arrow")], "float_sum.expected.arrow")
MAX = 1.7976931348623157e308
write("float_specials.in.arrow", KV1,
      batch(KV1, [1, 1, 2, 2, 3, 3, 4, 4], [INF, 1.0, INF, -INF, MAX, MAX, -0.0, -0.0]))
KS = schema(("k", I64, True), ("s", F64, True))
write("float_specials.expected.arrow", KS, batch(KS, [1, 2, 3, 4], [INF, NAN, INF, 0.0]))
case("float_sum_specials", ["rounding", "overflow"],
     "An infinity wins; both infinities are NaN; an exact total past the largest double is "
     "infinity; -0.0 + -0.0 is +0.0.",
     {"keys": ["k"], "aggregates": [{"op": "sum", "column": "v", "as": "s"}]},
     [src("float_specials.in.arrow")], "float_specials.expected.arrow")

# Min and max in total order.
write("min_max.in.arrow", KV1, batch(KV1, [1, 1, 2, 2], [0.0, -0.0, -INF, -1.0]))
MM = schema(("k", I64, True), ("lo", F64, True), ("hi", F64, True))
write("min_max.expected.arrow", MM, batch(MM, [1, 2], [-0.0, -INF], [0.0, -1.0]))
case("float_min_max", ["ordering"], "-0.0 is less than +0.0, and -infinity is a minimum.",
     {"keys": ["k"], "aggregates": [{"op": "min", "column": "v", "as": "lo"},
                                    {"op": "max", "column": "v", "as": "hi"}]},
     [src("min_max.in.arrow")], "min_max.expected.arrow")

# Unsigned and narrow values, temporal min.
UV = schema(("k", I64, True), ("v", pa.uint8(), True), ("d", pa.date32(), True))
write("unsigned.in.arrow", UV, batch(UV, [1, 1], [200, 100], [19000, 18000]))
UO = schema(("k", I64, True), ("s", pa.uint64(), True), ("lo", pa.uint8(), True),
            ("hi", pa.uint8(), True), ("m", F64, True), ("first", pa.date32(), True))
write("unsigned.expected.arrow", UO, batch(UO, [1], [300], [100], [200], [150.0], [18000]))
case("unsigned_and_temporal", ["params"],
     "A sum of UInt8 is UInt64, min and max keep UInt8 and Date32, a mean of integers is "
     "Float64.",
     {"keys": ["k"], "aggregates": [
         {"op": "sum", "column": "v", "as": "s"}, {"op": "min", "column": "v", "as": "lo"},
         {"op": "max", "column": "v", "as": "hi"}, {"op": "mean", "column": "v", "as": "m"},
         {"op": "min", "column": "d", "as": "first"}]},
     [src("unsigned.in.arrow")], "unsigned.expected.arrow")

# Integer overflow.
KI = schema(("k", I64, True), ("v", I64, True))
write("overflow.in.arrow", KI, batch(KI, [1, 1], [2**63 - 1, 1]))
case("sum_overflow", ["overflow", "error"],
     "A final Int64 sum past Int64 refuses the run at finish, naming the group.",
     {"keys": ["k"], "aggregates": [{"op": "sum", "column": "v", "as": "s"}]},
     [src("overflow.in.arrow")], error={"at": "apply", "contains": "the sum `s` of the group (1)"})
write("back_in_range.in.arrow", KI, batch(KI, [1, 1, 1], [2**63 - 1, 1, -5]))
KIS = schema(("k", I64, True), ("s", I64, True))
write("back_in_range.expected.arrow", KIS, batch(KIS, [1], [2**63 - 5]))
case("sum_back_in_range", ["overflow"],
     "A total that passes Int64 in between and ends in range is no overflow.",
     {"keys": ["k"], "aggregates": [{"op": "sum", "column": "v", "as": "s"}]},
     [src("back_in_range.in.arrow")], "back_in_range.expected.arrow")

# Keys alone.
A = schema(("a", I64, True))
write("keys_only.in.arrow", A, batch(A, [3, 1, 3, None]))
write("keys_only.expected.arrow", A, batch(A, [1, 3, None]))
case("keys_only", ["ordering", "nulls"], "No aggregates: the distinct keys, in order.",
     {"keys": ["a"]}, [src("keys_only.in.arrow")], "keys_only.expected.arrow")

# The partial form and the merge.
P_IN = schema(("k", I64, True), ("n", I64, True), ("v", F64, True))
write("partial.in.arrow", P_IN, batch(P_IN, [1, 1, 2], [2**63 - 1, 2**63 - 1, None],
                                       [0.5, 0.25, None]))
DEC = pa.decimal128(38, 0)
P_OUT = schema(("k", I64, True), ("rows", I64, False), ("n_sum", DEC, True, SUM_META),
               ("v_sum", pa.binary(), True, FSUM_META), ("n_mean.sum", DEC, False),
               ("n_mean.count", I64, False), ("v_mean.sum", pa.binary(), False),
               ("v_mean.count", I64, False))
first = batch(P_OUT, [1, 2], [2, 1], [2 * (2**63 - 1), None], [exact_bytes([0.5, 0.25]), None],
              [2 * (2**63 - 1), 0], [2, 0], [exact_bytes([0.5, 0.25]), exact_bytes([])], [2, 0])
write("partial.expected.arrow", P_OUT, first)
P_PARAMS = {"keys": ["k"], "aggregates": [
    {"op": "count", "as": "rows"}, {"op": "sum", "column": "n", "as": "n_sum"},
    {"op": "sum", "column": "v", "as": "v_sum"}, {"op": "mean", "column": "n", "as": "n_mean"},
    {"op": "mean", "column": "v", "as": "v_mean"}]}
case("partial_form", ["params", "nulls", "overflow"],
     "Partial mode gives each aggregate's state: an Int64 total past Int64 kept exactly as "
     "Decimal128(38, 0), float sums in their one encoding, a mean's sum and count.",
     dict(P_PARAMS, mode="partial"), [src("partial.in.arrow")],
     "partial.expected.arrow")
second = batch(P_OUT, [1, 2], [1, 1], [-(2**63 - 1), 4], [exact_bytes([0.125]),
                                                          exact_bytes([3.0])],
               [-(2**63 - 1), 4], [1, 1], [exact_bytes([0.125]), exact_bytes([3.0])], [1, 1])
write("merge.in.arrow", P_OUT, first, second)
M_OUT = schema(("k", I64, True), ("rows", I64, False), ("n_sum", I64, True),
               ("v_sum", F64, True), ("n_mean", F64, True), ("v_mean", F64, True))
write("merge.expected.arrow", M_OUT,
      batch(M_OUT, [1, 2], [3, 2], [2**63 - 1, 4], [0.875, 3.0], [float(2**63 - 1) / 3, 4.0],
            [0.875 / 3, 3.0]))
case("final_merge", ["multi_batch", "overflow", "nulls"],
     "Final mode merges partial results: counts and totals add (a partial past Int64 comes back "
     "in range), a null partial adds nothing, a mean divides the merged sum by the merged count.",
     dict(P_PARAMS, mode="final"), [src("merge.in.arrow")], "merge.expected.arrow")

# Refusals.
case("no_keys", ["params", "error"], "No keys is refused at configure.",
     {"keys": [], "aggregates": [{"op": "count", "as": "n"}]}, [src("empty.in.arrow")],
     error={"at": "configure", "contains": "names no column"})
case("unknown_op", ["params", "error"], "An unknown aggregate is refused at configure.",
     {"keys": ["k"], "aggregates": [{"op": "median", "column": "v", "as": "m"}]},
     [src("empty.in.arrow")], error={"at": "configure", "contains": "`median`"})
case("sum_of_strings", ["error"], "A sum of a string column is refused at plan.",
     {"keys": ["s"], "aggregates": [{"op": "sum", "column": "s", "as": "t"}]},
     [src("strings.in.arrow")], error={"at": "schema", "contains": "does not take"})
case("domain_of_strings", ["params", "error"],
     "A declared domain of a string key is refused at plan.",
     {"keys": ["s"], "domain": {"s": [0, 1]}}, [src("strings.in.arrow")],
     error={"at": "schema", "contains": "integer keys"})


def occupancy(path, trips=1000):
    """Occupancy's counting step after its expansion (bench/vm/workloads.py): the first `trips`
    trips of the month, expanded as workloads.py expands them, in three batches; the expected
    counts are workloads.py's own np.unique of zone * 1440 + minute % 1440."""
    t = pq.read_table(path, columns=["pickup_at", "dropoff_at", "pu_location_id"]).slice(0, trips)
    b = t.combine_chunks().to_batches()[0]
    a = b["pickup_at"].cast(pa.int64()).to_numpy(zero_copy_only=False) // 60_000_000
    d = b["dropoff_at"].cast(pa.int64()).to_numpy(zero_copy_only=False) // 60_000_000
    zone = b["pu_location_id"].to_numpy(zero_copy_only=False)
    n = np.clip(d - a, 0, 240) + 1
    idx = np.repeat(np.arange(len(n)), n)
    minute = a[idx] + (np.arange(len(idx)) - np.repeat(np.cumsum(n) - n, n))
    key = zone[idx] * 1440 + minute % 1440
    keys, counts = np.unique(key, return_counts=True)
    ins = schema(("zone", I64, False), ("minute_of_day", I64, False))
    rows = batch(ins, zone[idx].tolist(), (minute % 1440).tolist())
    third = rows.num_rows // 3
    write("occupancy.in.arrow", ins, rows.slice(0, third), rows.slice(third, third),
          rows.slice(2 * third))
    out = schema(("zone", I64, False), ("minute_of_day", I64, False), ("car_minutes", I64, False))
    write("occupancy.expected.arrow", out,
          batch(out, (keys // 1440).tolist(), (keys % 1440).tolist(), counts.tolist()))
    return rows.num_rows


expanded = occupancy(sys.argv[1] if len(sys.argv) > 1 else MONTH)
OCC = {"keys": ["zone", "minute_of_day"], "aggregates": [{"op": "count", "as": "car_minutes"}]}
about = (f"Occupancy's counting step after its expansion (bench/vm/workloads.py): the first 1000 "
         f"trips of January 2024, {expanded} rows in three batches, against workloads.py's "
         f"np.unique")
case("occupancy", ["multi_batch"], about + ", on the hash path.", OCC,
     [src("occupancy.in.arrow")], "occupancy.expected.arrow")
case("occupancy_dense", ["multi_batch", "params"],
     about + ", on the dense path of 4G.4's domain (zones 0 to 299, minutes 0 to 1439).",
     dict(OCC, domain={"zone": [0, 299], "minute_of_day": [0, 1439]}),
     [src("occupancy.in.arrow")], "occupancy.expected.arrow")
case("occupancy_partial", ["multi_batch", "params"],
     about + ", in partial mode: for count alone the partial form is the final form.",
     dict(OCC, mode="partial"), [src("occupancy.in.arrow")],
     "occupancy.expected.arrow")

with open(os.path.join(HERE, "index.json"), "w") as f:
    json.dump({"format": "moruna-kernel-corpus/1", "kernel": "moruna.group_aggregate",
               "cases": CASES}, f, indent=2, ensure_ascii=False)
    f.write("\n")
print(f"{len(CASES)} cases")
