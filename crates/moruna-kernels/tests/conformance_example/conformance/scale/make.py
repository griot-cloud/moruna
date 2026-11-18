"""Writes example.scale's corpus: index.json and the Arrow IPC files it names.

    python3 -I crates/moruna-kernels/tests/conformance_example/conformance/scale/make.py

The expected products are IEEE 754 double multiplications done here, and the allowed error of
each element is |expected| * 2**-50, or null where the case wants the exact value.
"""
import json
import math
import os

import pyarrow as pa

HERE = os.path.dirname(os.path.abspath(__file__))
SCHEMA = pa.schema([pa.field("x", pa.float64())])
CASES = []


def write(name, *columns, field="x"):
    """One file, one batch per list of values."""
    schema = pa.schema([pa.field(field, pa.float64())])
    with pa.OSFile(os.path.join(HERE, name), "wb") as sink:
        with pa.ipc.new_file(sink, schema) as writer:
            for values in columns:
                writer.write_batch(pa.record_batch([pa.array(values, pa.float64())], schema=schema))
    return name


def scaled(values, factor):
    return [None if v is None else v * factor for v in values]


def allowed(values, exact=()):
    """|v| * 2**-50 for each expected value; null (bit for bit) for nulls, infinities and the
    rows in `exact`."""
    return [None if v is None or math.isinf(v) or i in exact else abs(v) * 2.0**-50
            for i, v in enumerate(values)]


def case(name, tests, about, factor, inputs, expected, allowed_file=None):
    entry = {"name": name, "version": 1, "tests": tests, "about": about,
             "params": {"factor": factor}, "inputs": inputs, "expected": expected}
    if allowed_file:
        entry["allowed"] = {"x": {"file": allowed_file}}
    CASES.append(entry)


X = [1.5, None, -2.25, 1e308, 0.1, -0.0, 3.0]
write("x.in.arrow", X)
OUT = scaled(X, 3.0)
case("scales", ["params", "nulls"],
     "Each x gives x * factor within its allowed error; a null gives a null; 1e308 * 3 is +inf, "
     "which only +inf matches.",
     3.0, [{"file": "x.in.arrow"}], write("x.expected.arrow", OUT),
     write("x.allowed.arrow", allowed(OUT)))

case("exact_where_unbounded", ["params"],
     "Where an element's allowed error is null, the product must be the expected bit for bit.",
     3.0, [{"file": "x.in.arrow"}], "x.expected.arrow",
     write("x_exact.allowed.arrow", allowed(OUT, exact={0, 4})))

SLICE = X[2:6]
OUT_SLICE = scaled(SLICE, 0.5)
case("sliced", ["sliced"], "A batch whose arrays start at row 2 gives the products of its rows.",
     0.5, [{"file": "x.in.arrow", "slice": [2, 4]}], write("sliced.expected.arrow", OUT_SLICE),
     write("sliced.allowed.arrow", allowed(OUT_SLICE)))

case("empty", ["empty"], "A batch of zero rows gives a batch of zero rows.",
     2.0, [{"file": "empty.in.arrow"}], write("empty.expected.arrow", []),
     write("empty.allowed.arrow", []))
write("empty.in.arrow", [])

TWO = [[0.25, 8.0], [None, -1e-300]]
OUT_TWO = [scaled(b, -4.0) for b in TWO]
case("two_batches_exact", ["multi_batch", "nulls"],
     "With no allowed error given, each of two batches is compared bit for bit.",
     -4.0, [{"file": "two.in.arrow"}], write("two.expected.arrow", *OUT_TWO))
write("two.in.arrow", *TWO)

assert math.isinf(OUT[3])
index = {"format": "moruna-kernel-corpus/1", "kernel": "example.scale", "cases": CASES}
with open(os.path.join(HERE, "index.json"), "w") as f:
    json.dump(index, f, indent=2)
    f.write("\n")
