"""Writes the Arrow IPC files of moruna.interval_coverage_count's corpus (index.json names them).

    python3 -I crates/moruna-kernels/conformance/interval_coverage_count/make.py

Every expected output but one is written out here by hand, not computed by an implementation:
the corpus defines the right answer. The one, `occupancy.expected.arrow`, is Occupancy as written
(bench/vm/workloads.py, 0.6.6's function: expand every trip to one row per minute, then
`np.unique`) on sixty real trips of January 2024, listed below: trips across midnight, trips
over four hours, trips that end before they start and trips of no time.
"""
import os
import sys

import pyarrow as pa

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "..", "..", "bench", "vm"))

import workloads  # noqa: E402

MIN = 60_000_000  # a minute in microseconds
DAY = 1440 * MIN
D = 19_723 * DAY  # 2024-01-01T00:00:00, a whole number of days


def at(h, m, s=0):
    """2024-01-01 at h:m:s, in microseconds."""
    return D + ((h * 60 + m) * 60 + s) * 1_000_000


INPUT = pa.schema([
    pa.field("group", pa.int32(), nullable=True),
    pa.field("start", pa.int64(), nullable=True),
    pa.field("length", pa.int64(), nullable=True),
])
OUTPUT = pa.schema([
    pa.field("group", pa.int32(), nullable=False),
    pa.field("bucket", pa.int32(), nullable=False),
    pa.field("count", pa.int64(), nullable=False),
])


def write(name, schema, *batches):
    """One file, one batch per list of rows (tuples in the schema's column order)."""
    with pa.OSFile(os.path.join(HERE, name), "wb") as sink:
        with pa.ipc.new_file(sink, schema) as writer:
            for rows in batches:
                columns = [pa.array([r[i] for r in rows], f.type) for i, f in enumerate(schema)]
                writer.write_batch(pa.record_batch(columns, schema=schema))


def inputs(name, *batches):
    write(name, INPUT, *batches)


def expected(name, *batches):
    write(name, OUTPUT, *batches)


# A grid of 10 buckets 5 wide. Group 2 covers buckets 3-6 and 5-8; group 1 bucket 2. The second
# batch's output follows the first's.
inputs("basic.in.arrow", [(2, 15, 4), (2, 25, 4), (1, 14, 1)], [(0, 0, 1)])
expected("basic.expected.arrow",
         [(1, 2, 1), (2, 3, 1), (2, 4, 1), (2, 5, 2), (2, 6, 2), (2, 7, 1), (2, 8, 1),
          (0, 0, 1)])

# A day of minutes in microseconds, across midnight: 23:58:30 for five minutes is 23:58, 23:59,
# 00:00, 00:01 and 00:02.
inputs("midnight.in.arrow",
       [(7, at(23, 58, 30), 5), (3, at(23, 59, 59), 3), (3, at(0, 0, 1), 2),
        (1, at(23, 59), 2)])
expected("midnight.expected.arrow",
         [(1, 0, 1), (1, 1439, 1), (3, 0, 2), (3, 1, 2), (3, 1439, 1),
          (7, 0, 1), (7, 1, 1), (7, 2, 1), (7, 1438, 1), (7, 1439, 1)])

# Up to midnight and from midnight: 23:58 for two minutes stops at 23:59; 00:00 for one is 00:00;
# a start a day later is the same minute of the day.
inputs("midnight_edges.in.arrow",
       [(5, at(23, 58), 2), (6, at(0, 0), 1), (6, at(23, 59, 59) + DAY, 1)])
expected("midnight_edges.expected.arrow", [(5, 1438, 1), (5, 1439, 1), (6, 0, 1), (6, 1439, 1)])

# A ring of four buckets: ten from bucket 3 pass 3, 0, 1, 2, 3, 0, 1, 2, 3, 0; four from bucket 2
# pass each bucket once; nine on a ring of one count nine.
inputs("longer.in.arrow", [(0, 3, 10), (1, 2, 4)])
expected("longer.expected.arrow",
         [(0, 0, 3), (0, 1, 2), (0, 2, 2), (0, 3, 3), (1, 0, 1), (1, 1, 1), (1, 2, 1), (1, 3, 1)])
inputs("one_bucket.in.arrow", [(5, -100, 9), (5, 3, 1), (4, 0, 0)])
expected("one_bucket.expected.arrow", [(5, 0, 10)])

# Zero and negative lengths cover nothing.
inputs("lengths.in.arrow", [(1, 2, 0), (1, 3, -1), (1, 4, -(2**63)), (1, 3, 1)])
expected("lengths.expected.arrow", [(1, 3, 1)])

# Step 10, size 6: -1 is in bucket floor(-0.1) = -1, which is 5; -60 in -6, which is 0; -61 in
# -7, which is 5.
inputs("floor.in.arrow", [(0, -1, 2), (1, -60, 1), (2, -61, 1)])
expected("floor.expected.arrow", [(0, 0, 1), (0, 5, 1), (1, 0, 1), (2, 5, 1)])

# A null in any column skips the row.
inputs("nulls.in.arrow",
       [(None, 0, 1), (1, None, 1), (1, 1, None), (1, 2, 1), (None, None, None), (2, 3, 2)])
expected("nulls.expected.arrow", [(1, 2, 1), (2, 0, 1), (2, 3, 1)])

# Rows 2 to 5 of eight: the rows around them, which would cover every bucket of group 9, are not
# read.
inputs("sliced.in.arrow",
       [(9, 0, 4), (9, 0, 4), (1, 1, 1), (None, 0, 1), (2, 2, 1), (1, 1, 2), (9, 0, 4),
        (9, 0, 4)])
expected("sliced.expected.arrow", [(1, 1, 2), (1, 2, 1), (2, 2, 1)])

inputs("empty.in.arrow", [])
inputs("no_batches.in.arrow")
expected("empty.expected.arrow", [])

# Groups 1, 3, 2, 1 on a ring of eight; with the domain [0, 3] the dense table, with [1, 2] a
# miss (group 3, then -5), and the same cells either way.
inputs("domain.in.arrow", [(1, 0, 3), (3, 2, 4), (2, 7, 2), (1, 5, 1)])
expected("domain.expected.arrow",
         [(1, 0, 1), (1, 1, 1), (1, 2, 1), (1, 5, 1), (2, 0, 1), (2, 7, 1),
          (3, 2, 1), (3, 3, 1), (3, 4, 1), (3, 5, 1)])
inputs("domain_miss.in.arrow", [(-5, 0, 1), (1, 0, 1), (None, 0, 1), (3, 7, 2)])
expected("domain_miss.expected.arrow", [(-5, 0, 1), (1, 0, 1), (3, 0, 1), (3, 7, 1)])

# Group keys at Int32's ends, and the most buckets a ring can have.
inputs("extremes.in.arrow",
       [(2**31 - 1, 2**63 - 1, 2), (-(2**31), -(2**63), 1), (0, 2**31 - 2, 3)])
expected("extremes.expected.arrow",
         [(-(2**31), (-(2**63)) % (2**31 - 1), 1),
          (0, 0, 1), (0, 1, 1), (0, 2**31 - 2, 1),
          (2**31 - 1, (2**63 - 1) % (2**31 - 1), 1), (2**31 - 1, (2**63 - 1) % (2**31 - 1) + 1, 1)])

# Two lengths of Int64's largest on a ring of one: the count overflows.
inputs("overflow.in.arrow", [(0, 0, 2**63 - 1), (0, 0, 2**63 - 1)])

# Input columns of the wrong type.
write("wrong_type.in.arrow",
      pa.schema([pa.field("group", pa.int64()), pa.field("start", pa.int64()),
                 pa.field("length", pa.int64())]),
      [(1, 0, 1)])

# Sixty real trips of January 2024 (pu_location_id, pickup_at and dropoff_at in microseconds).
TRIPS = [
    (237, 1704070329000000, 1704070348000000),
    (236, 1704068069000000, 1704068069000000),
    (42, 1704068762000000, 1704068781000000),
    (236, 1704067006000000, 1704067926000000),
    (239, 1704069976000000, 1704069991000000),
    (170, 1704068062000000, 1704068513000000),
    (79, 1704070235000000, 1704071466000000),
    (113, 1704069184000000, 1704070400000000),
    (114, 1704070525000000, 1704070986000000),
    (88, 1704068431000000, 1704069314000000),
    (141, 1704069575000000, 1704070104000000),
    (107, 1704070304000000, 1704070752000000),
    (48, 1704068631000000, 1704069537000000),
    (244, 1704069928000000, 1704070121000000),
    (237, 1704067787000000, 1704067941000000),
    (238, 1704068621000000, 1704069198000000),
    (263, 1704069719000000, 1704069969000000),
    (50, 1704067290000000, 1704068428000000),
    (237, 1704069087000000, 1704070316000000),
    (238, 1704067967000000, 1704068451000000),
    (142, 1704069592000000, 1704069899000000),
    (50, 1704068433000000, 1704069233000000),
    (249, 1704070469000000, 1704071734000000),
    (262, 1704069143000000, 1704069249000000),
    (141, 1704069465000000, 1704070290000000),
    (236, 1704068249000000, 1704068894000000),
    (186, 1704068866000000, 1704070965000000),
    (138, 1704068294000000, 1704069129000000),
    (236, 1704068172000000, 1704068399000000),
    (236, 1704069109000000, 1704069455000000),
    (263, 1704069621000000, 1704069753000000),
    (263, 1704070107000000, 1704070413000000),
    (229, 1704070476000000, 1704071502000000),
    (237, 1704069121000000, 1704069410000000),
    (43, 1704070194000000, 1704072062000000),
    (43, 1704069551000000, 1704155659000000),
    (138, 1704067115000000, 1704067986000000),
    (161, 1704067117000000, 1704067717000000),
    (79, 1704070284000000, 1704156253000000),
    (79, 1704069640000000, 1704155665000000),
    (79, 1704070391000000, 1704156596000000),
    (163, 1704068483000000, 1704146536000000),
    (246, 1704072394000000, 1704158688000000),
    (229, 1704066867000000, 1704067992000000),
    (138, 1704153127000000, 1704154152000000),
    (140, 1704153221000000, 1704153665000000),
    (68, 1704153496000000, 1704153809000000),
    (138, 1704153398000000, 1704154634000000),
    (138, 1704153121000000, 1704154337000000),
    (79, 1704153029000000, 1704153413000000),
    (161, 1704153371000000, 1704154217000000),
    (236, 1704153396000000, 1704153662000000),
    (70, 1704153259000000, 1704154411000000),
    (138, 1704153575000000, 1704154688000000),
    (132, 1704153572000000, 1704154711000000),
    (186, 1704153381000000, 1704154435000000),
    (75, 1704189600000000, 1704189236000000),
    (165, 1704371400000000, 1704370703000000),
    (95, 1704376800000000, 1704376550000000),
    (133, 1704452400000000, 1704452236000000),
]
# The kernel's inputs, as Occupancy computes them (the `expr` before the kernel): the zone, the
# pickup instant, and clip(floor(dropoff) - floor(pickup), 0, 240) + 1 minutes.
inputs("occupancy.in.arrow",
       [(z, a, min(max(b // MIN - a // MIN, 0), 240) + 1) for z, a, b in TRIPS])
trips = pa.record_batch(
    [pa.array([t[1] for t in TRIPS], pa.timestamp("us")),
     pa.array([t[2] for t in TRIPS], pa.timestamp("us")),
     pa.array([t[0] for t in TRIPS], pa.int64())],
    names=["pickup_at", "dropoff_at", "pu_location_id"])
occupied = workloads.occupancy(trips)
expected("occupancy.expected.arrow",
         list(zip(occupied["zone"].to_pylist(), occupied["minute_of_day"].to_pylist(),
                  occupied["car_minutes"].to_pylist(), strict=True)))
