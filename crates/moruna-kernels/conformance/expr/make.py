"""Writes moruna.expr's corpus: index.json and the Arrow IPC files it names.

    python3 -I crates/moruna-kernels/conformance/expr/make.py [--month PATH]

Every expected output is computed here, independently of the CPU reference: by pyarrow 24's
compute functions, which are the vocabulary's reference; by Python's `decimal` for `round` (its
exact definition, spec/expr.md "Rounding"), and Python's `round` for the month's values; by
IEEE 754's totalOrder written out for `"order": "total"`; and by `bench/vm/workloads.py` itself
for Audit and Enrich. `--month` re-derives the 53,698 Audit fares on which pyarrow's round and
Python's differ from a month of trips; without it the file already here is kept.
"""
import decimal
import json
import math
import os
import struct
import sys

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", "..", ".."))
sys.path.insert(0, os.path.join(REPO, "bench", "vm"))
sys.path.insert(0, os.path.join(REPO, "bench", "kernels"))
import expr_trees  # noqa: E402
import workloads  # noqa: E402

CASES = []
FILES = set()
MIN64, MAX64 = -(2**63), 2**63 - 1
NAN, INF = float("nan"), float("inf")


def write(name, *batches):
    """One IPC file, one batch per argument (a RecordBatch or a dict of arrays)."""
    batches = [b if isinstance(b, pa.RecordBatch) else pa.record_batch(b) for b in batches]
    with pa.OSFile(os.path.join(HERE, name), "wb") as sink:
        with pa.ipc.new_file(sink, batches[0].schema) as writer:
            for b in batches:
                writer.write_batch(b)
    FILES.add(name)
    return name


def col(n):
    return {"column": n}


def lit(v, ty=None):
    return {"literal": v} if ty is None else {"literal": v, "type": ty}


def call(name, *args, **options):
    node = {"call": name, "args": list(args)}
    if options:
        node["options"] = options
    return node


def tree(outputs):
    return {"outputs": [{"name": n, "expr": e} for n, e in outputs]}


def case(name, tests, about, outputs, inputs, expected=None, error=None, allowed=None,
         params=None):
    entry = {"name": name, "version": 1, "tests": tests, "about": about,
             "params": params if params is not None else tree(outputs),
             "inputs": inputs if isinstance(inputs, list) else [{"file": inputs}]}
    if error is not None:
        entry["error"] = error
    else:
        entry["expected"] = expected
    if allowed:
        entry["allowed"] = {k: {"file": v} for k, v in allowed.items()}
    CASES.append(entry)


def out(batch_or_arrays):
    """An expected batch from (name, array) pairs: every column nullable, as the kernel gives."""
    names = [n for n, _ in batch_or_arrays]
    arrays = [pa.array(a) if not isinstance(a, (pa.Array, pa.ChunkedArray)) else a
              for _, a in batch_or_arrays]
    arrays = [a.combine_chunks() if isinstance(a, pa.ChunkedArray) else a for a in arrays]
    return pa.record_batch(arrays, schema=pa.schema([pa.field(n, a.type) for n, a in zip(names, arrays)]))


def run(batch, outputs):
    """Each output node evaluated by pyarrow on `batch`: the vocabulary's reference."""
    return out([(n, ev(batch, e)) for n, e in outputs])


SIMPLE = {"bool": pa.bool_(), "int8": pa.int8(), "int16": pa.int16(), "int32": pa.int32(),
          "int64": pa.int64(), "uint8": pa.uint8(), "uint16": pa.uint16(), "uint32": pa.uint32(),
          "uint64": pa.uint64(), "float": pa.float32(), "double": pa.float64(),
          "string": pa.string(), "large_string": pa.large_string(), "date32": pa.date32(),
          "decimal128(10, 2)": pa.decimal128(10, 2)}


def pa_type(spelled):
    """A type by its spelling in a tree."""
    if spelled.startswith("timestamp["):
        unit, *zone = [p.strip() for p in spelled[len("timestamp["):-1].split(",")]
        return pa.timestamp(unit, tz=zone[0].split("=", 1)[1] if zone else None)
    if spelled.startswith("duration["):
        return pa.duration(spelled[len("duration["):-1])
    return SIMPLE[spelled]


def scalar(node):
    v, ty = node["literal"], node.get("type")
    if ty is None:
        ty = pa.bool_() if isinstance(v, bool) else pa.string() if isinstance(v, str) else \
            pa.float64() if isinstance(v, float) else pa.int64()
    else:
        ty = pa_type(ty)
    if isinstance(v, str) and pa.types.is_floating(ty):
        v = float(v.replace("Infinity", "inf"))
    if pa.types.is_decimal(ty) and v is not None:
        v = decimal.Decimal(v)
    return pa.scalar(v, ty)


def ev(batch, node):
    """A node by pyarrow: columns, literals, casts and calls (no if or switch here)."""
    if "column" in node:
        return batch[node["column"]]
    if "literal" in node:
        return scalar(node)
    if "cast" in node:
        return pc.cast(ev(batch, node["cast"]), pa_type(node["to"]), safe=node.get("safe", True))
    args = [ev(batch, a) for a in node["args"]]
    opts = dict(node.get("options", {}))
    name = node["call"]
    if name in ("and", "or"):
        name += "_"
    return getattr(pc, name)(*args, **opts)


def nulls_tags(*more):
    return ["nulls", *more]


# ------------------------------------------------------------------ arithmetic
ints = {
    "i": pa.array([1, -7, None, MAX64, MIN64], pa.int64()),
    "j": pa.array([2, 2, 3, 1, -1], pa.int64()),
    "a": pa.array([2**31 - 1, -7, 0, 5, None], pa.int32()),
    "u": pa.array([1, 2, 255, 0, None], pa.uint8()),
    "v": pa.array([1, -128, 3, None, 127], pa.int8()),
}
f_ints = write("ints.in.arrow", ints)
B_INTS = pa.record_batch(ints)
o = [("add", call("add", col("i"), col("j"))),
     ("subtract", call("subtract", col("i"), col("j"))),
     ("multiply", call("multiply", col("i"), col("j"))),
     ("divide", call("divide", col("i"), col("j"))),
     ("add_int32", call("add", col("a"), col("a"))),
     ("add_uint8_int8", call("add", col("u"), col("v"))),
     ("subtract_uint8", call("subtract", col("u"), lit(2, "uint8"))),
     ("multiply_int8", call("multiply", col("v"), lit(3, "int8"))),
     ("power", call("power", col("j"), lit(63))),
     ("negate", call("negate", col("i"))),
     ("negate_uint8", call("negate", col("u"))),
     ("abs_int8", call("abs", col("v"))),
     ("sign", call("sign", col("i"))),
     ("sign_uint8", call("sign", col("u")))]
case("integer_arithmetic_wraps", nulls_tags("overflow"),
     "Integer add, subtract, multiply, power, negate and abs wrap at the result type's width, the "
     "minimum divided by -1 is 0, mixed widths take Arrow's common type, and a null gives null.",
     o, f_ints, write("ints.expected.arrow", run(B_INTS, o)))
case("checked_add_refuses_overflow", ["overflow", "error"],
     "add_checked of int64's maximum and 1 refuses the batch.",
     [("x", call("add_checked", col("i"), col("j")))], f_ints,
     error={"at": "apply", "contains": "add_checked: overflow"})
case("checked_divide_refuses_min_over_minus_one", ["overflow", "error"],
     "divide_checked of int64's minimum by -1 refuses the batch.",
     [("x", call("divide_checked", col("i"), col("j")))], f_ints,
     error={"at": "apply", "contains": "overflow"})
case("integer_divide_by_zero_refused", ["error"],
     "An integer divided by zero refuses the batch in the unchecked form too.",
     [("x", call("divide", col("j"), call("subtract", col("j"), col("j"))))], f_ints,
     error={"at": "apply", "contains": "divide by zero"})
case("negative_integer_power_refused", ["error"],
     "An integer to a negative integer power refuses the batch.",
     [("x", call("power", col("j"), col("v")))], f_ints,
     error={"at": "apply", "contains": "negative integer powers"})
quiet = {"x": pa.array([MAX64, 2, 5], pa.int64()), "y": pa.array([None, 3, None], pa.int64()),
         "z": pa.array([None, 1, 0], pa.int64())}
f_quiet = write("quiet.in.arrow", quiet)
o = [("multiply_checked", call("multiply_checked", col("x"), col("y"))),
     ("divide", call("divide", col("y"), col("z")))]
case("errors_only_on_valid_rows", ["nulls", "overflow"],
     "An overflow or a zero divisor under a null row is no error: the row is null.",
     o, f_quiet, write("quiet.expected.arrow", run(pa.record_batch(quiet), o)))

floats = {
    "f": pa.array([1.5, NAN, -0.0, None, INF, 1e308], pa.float64()),
    "g": pa.array([0.0, 1.0, 0.0, 2.0, INF, 10.0], pa.float64()),
    "h": pa.array([1.0, 2.0, None, 0.1, -3.0, 1e30], pa.float32()),
    "j": pa.array([2, 2, 3, 1, -1, 7], pa.int64()),
}
f_floats = write("floats.in.arrow", floats)
B_FLOATS = pa.record_batch(floats)
o = [("add", call("add", col("f"), col("g"))),
     ("subtract", call("subtract", col("f"), col("g"))),
     ("multiply", call("multiply", col("f"), col("g"))),
     ("divide", call("divide", col("f"), col("g"))),
     ("add_checked", call("add_checked", col("f"), col("f"))),
     ("multiply_checked", call("multiply_checked", col("f"), lit(10.0))),
     ("float32", call("divide", col("h"), lit(3))),
     ("float32_times", call("multiply", col("h"), col("h"))),
     ("int_and_double", call("add", col("j"), col("f"))),
     ("power", call("power", col("g"), lit(2.0))),
     ("negate", call("negate", col("f"))),
     ("abs", call("abs", col("f"))),
     ("sign", call("sign", col("f"))),
     ("sign_float32", call("sign", col("h")))]
case("float_arithmetic_is_ieee", nulls_tags(),
     "Doubles and singles follow IEEE 754: x/0 is an infinity or NaN, overflow is an infinity "
     "the checked forms do not refuse, a single stays a single, an int with a double is a double.",
     o, f_floats, write("floats.expected.arrow", run(B_FLOATS, o)))
case("divide_checked_refuses_zero", ["error"],
     "divide_checked of a double by 0.0 refuses the batch.",
     [("x", call("divide_checked", col("f"), col("g")))], f_floats,
     error={"at": "apply", "contains": "divide by zero"})

# ------------------------------------------------------------------ comparisons
cmp_in = {
    "x": pa.array([NAN, -0.0, 1.0, None, NAN, INF, 0.0, -INF], pa.float64()),
    "y": pa.array([NAN, 0.0, NAN, 1.0, 5.0, INF, -0.0, NAN], pa.float64()),
    "p": pa.array([NAN, -0.0, 1.0, None, NAN, INF, 0.0, 2.0], pa.float32()),
    "q": pa.array([1.0, 0.0, NAN, 1.0, NAN, 3.0, -0.0, 2.0], pa.float32()),
}
f_cmp = write("compare.in.arrow", cmp_in)
B_CMP = pa.record_batch(cmp_in)
NAMES = ["equal", "not_equal", "less", "less_equal", "greater", "greater_equal"]
o = [(f"{n}_{c}", call(n, col(c), col(d))) for n in NAMES for c, d in (("x", "y"), ("p", "q"))]
case("comparisons_in_ieee_order", nulls_tags("ordering"),
     "Each comparison of doubles and of singles in IEEE order (the default): NaN compares false "
     "but not_equal, -0.0 equals 0.0.",
     o, f_cmp, write("compare_ieee.expected.arrow", run(B_CMP, o)))


def total_key(v, single):
    """IEEE 754 totalOrder as an integer key."""
    if single:
        bits = struct.unpack("<i", struct.pack("<f", v))[0]
        return bits ^ 0x7FFFFFFF if bits < 0 else bits
    bits = struct.unpack("<q", struct.pack("<d", v))[0]
    return bits ^ 0x7FFFFFFFFFFFFFFF if bits < 0 else bits


def total(name, xs, ys, single):
    res = []
    for x, y in zip(xs, ys):
        if x is None or y is None:
            res.append(None)
            continue
        a, b = total_key(x, single), total_key(y, single)
        res.append({"equal": a == b, "not_equal": a != b, "less": a < b, "less_equal": a <= b,
                    "greater": a > b, "greater_equal": a >= b}[name])
    return pa.array(res, pa.bool_())


o = [(f"{n}_{c}", call(n, col(c), col(d), order="total")) for n in NAMES for c, d in (("x", "y"), ("p", "q"))]
exp = out([(f"{n}_{c}", total(n, cmp_in[c].to_pylist(), cmp_in[d].to_pylist(), c == "p"))
           for n in NAMES for c, d in (("x", "y"), ("p", "q"))])
case("comparisons_in_total_order", nulls_tags("ordering", "params"),
     "Each comparison of doubles and of singles with \"order\": \"total\": NaN equals NaN and is "
     "above every number, -0.0 is below 0.0.",
     o, f_cmp, write("compare_total.expected.arrow", exp))

other = {
    "s": pa.array(["a", "B", "é", None, ""], pa.string()),
    "l": pa.array(["b", "a", "z", "x", ""], pa.large_string()),
    "b": pa.array([False, True, True, None, False]),
    "c": pa.array([True, True, False, True, False]),
    "i": pa.array([1, 2, 3, None, -1], pa.int32()),
    "u": pa.array([1, 3, 2, 0, 0], pa.uint16()),
    "ts": pa.array([5, 1, None, -1, 0], pa.timestamp("s")),
    "tu": pa.array([5_000_000, 1_000_001, 0, -1_000_000, 0], pa.timestamp("us")),
    "d": pa.array([0, 19_723, -1, None, 5], pa.date32()),
    "e": pa.array([0, 19_000, 0, 1, 6], pa.date32()),
    "k": pa.array([-5, 5, 0, None, 1], pa.duration("us")),
    "m": pa.array([None if x is None else decimal.Decimal(x) for x in ["1.25", "-0.05", None, "1.25", "0.00"]],
                  pa.decimal128(10, 2)),
}
f_other = write("compare_types.in.arrow", other)
B_OTHER = pa.record_batch(other)
o = [("strings", call("less", col("s"), col("l"))),
     ("string_literal", call("equal", col("s"), lit("B"))),
     ("bools", call("less", col("b"), col("c"))),
     ("int32_uint16", call("greater_equal", col("i"), col("u"))),
     ("int_double", call("equal", col("i"), lit(2.0))),
     ("timestamps", call("less", col("ts"), col("tu"))),
     ("dates", call("less_equal", col("d"), col("e"))),
     ("durations", call("less", col("k"), lit(0, "duration[s]"))),
     ("decimals", call("equal", col("m"), lit("1.25", "decimal128(10, 2)"))),
     ("decimals_less", call("less", col("m"), lit("1.00", "decimal128(10, 2)"))),
     ("decimal_valid", call("is_valid", col("m"))),
     ("decimal_column", col("m"))]
case("comparisons_of_other_types", nulls_tags("ordering"),
     "Strings by their bytes (string with large_string), false before true, mixed integers and "
     "an int with a double by Arrow's common type, timestamps of two units as instants, dates, "
     "durations, and a decimal (an opaque type) through Arrow's kernels.",
     o, f_other, write("compare_types.expected.arrow", run(B_OTHER, o)))

# ------------------------------------------------------------------ logic, nulls, selection
pairs = [(a, b) for a in (True, False, None) for b in (True, False, None)]
logic_in = {"l": pa.array([a for a, _ in pairs], pa.bool_()), "r": pa.array([b for _, b in pairs], pa.bool_())}
f_logic = write("logic.in.arrow", logic_in)
o = [(n, call(n, col("l"), col("r"))) for n in
     ["and", "and_kleene", "or", "or_kleene", "xor", "and_not", "and_not_kleene"]]
o.append(("invert", call("invert", col("l"))))
case("boolean_logic_with_nulls", nulls_tags(),
     "Every pair of true, false and null through each Boolean function; the Kleene forms decide a "
     "row by a known operand.",
     o, f_logic, write("logic.expected.arrow", run(pa.record_batch(logic_in), o)))

sel = {
    "b": pa.array([True, False, None, True, False]),
    "i": pa.array([1, None, 3, 4, 5], pa.int8()),
    "f": pa.array([1.5, NAN, -0.0, None, 2.5]),
    "s": pa.array(["a", None, "é", "b", None]),
    "t": pa.array([None, 1, 2, None, 4], pa.timestamp("ms")),
    "n": pa.array([1, None, None, 2, 2], pa.int64()),
}
f_sel = write("select.in.arrow", sel)
B_SEL = pa.record_batch(sel)
o = [("is_null", call("is_null", col("f"))),
     ("is_null_nan", call("is_null", col("f"), nan_is_null=True)),
     ("is_valid", call("is_valid", col("s"))),
     ("is_nan", call("is_nan", col("f"))),
     ("is_finite", call("is_finite", col("f"))),
     ("is_inf", call("is_inf", call("divide", col("f"), lit(0.0)))),
     ("is_nan_int", call("is_nan", col("i"))),
     ("is_finite_int", call("is_finite", col("i"))),
     ("if_else", call("if_else", col("b"), col("i"), col("f"))),
     ("coalesce", call("coalesce", col("f"), col("i"), lit(9.5))),
     ("coalesce_strings", call("coalesce", col("s"), lit("?", "large_string"))),
     ("fill_null", call("fill_null", col("s"), lit("-"))),
     ("fill_null_time", call("fill_null", col("t"), lit(5, "timestamp[s]")))]
exp = run(B_SEL, o)
# The if node is case_when; a switch is case_when of equalities.
cw = pc.case_when(pc.make_struct(sel["b"], pc.is_null(sel["b"]), field_names=["a", "c"]),
                  pc.cast(sel["i"], pa.int64()), pa.scalar(9, pa.int64()), pa.scalar(0, pa.int64()))
cw_no_else = pc.case_when(pc.make_struct(sel["b"], field_names=["a"]), sel["i"])
sw = pc.case_when(pc.make_struct(pc.equal(sel["n"], 1), pc.equal(sel["n"], 2), field_names=["a", "c"]),
                  pa.scalar("one"), sel["s"], pa.scalar("other"))
names = [f.name for f in exp.schema] + ["if", "if_no_else", "switch"]
arrays = exp.columns + [cw, cw_no_else, sw]
o += [("if", {"if": [{"when": col("b"), "then": col("i")},
                     {"when": call("is_null", col("b")), "then": lit(9)}], "else": lit(0)}),
      ("if_no_else", {"if": [{"when": col("b"), "then": col("i")}]}),
      ("switch", {"switch": col("n"), "cases": [{"match": lit(1), "then": lit("one")},
                                               {"match": lit(2), "then": col("s")}],
                  "else": lit("other")})]
case("null_tests_and_selection", nulls_tags(),
     "is_null (with and without nan_is_null), is_valid, is_nan, is_finite and is_inf; if_else (a "
     "null condition is null), an if (a null condition counts as false), a switch (a null argument "
     "matches nothing), coalesce and fill_null, in Arrow's common types.",
     o, f_sel, write("select.expected.arrow", out(list(zip(names, arrays)))))

ext = {
    "x": pa.array([NAN, 1.0, -0.0, 0.0, None, NAN, 3.0]),
    "y": pa.array([0.0, NAN, 0.0, -0.0, 2.0, NAN, None]),
    "i": pa.array([1, 5, None, 3, 2, 0, -4], pa.int64()),
    "t": pa.array([1, 9, 3, None, 5, 6, 7], pa.timestamp("us")),
    "s": pa.array([2, 2, 2, 2, None, 2, 2], pa.timestamp("s")),
}
f_ext = write("extremes.in.arrow", ext)
o = [("max", call("max_element_wise", col("x"), col("y"))),
     ("min", call("min_element_wise", col("x"), col("y"))),
     ("max_strict", call("max_element_wise", col("x"), col("y"), skip_nulls=False)),
     ("max_int_double", call("max_element_wise", col("i"), lit(2.5))),
     ("min_ints", call("min_element_wise", col("i"), lit(2))),
     ("max_times", call("max_element_wise", col("t"), col("s")))]
case("element_wise_extremes", nulls_tags(),
     "min and max element-wise ignore NaN unless all are NaN, put 0.0 above -0.0, skip nulls "
     "unless told not to, and compare an int with a double, and timestamps of two units.",
     o, f_ext, write("extremes.expected.arrow", run(pa.record_batch(ext), o)))

# ------------------------------------------------------------------ casts
casts = {
    "f": pa.array([2.0, -3.0, None, 1e30, 0.0], pa.float64()),
    "i": pa.array([300, -1, None, 2**40, 5], pa.int64()),
    "u": pa.array([2**64 - 1, 1, None, 2, 5], pa.uint64()),
    "b": pa.array([True, False, None, True, False]),
    "t": pa.array([1_500_000, -1_500_000, None, -1, 86_400_000_000], pa.timestamp("us")),
    "z": pa.array([0, 23 * 3_600_000_000, None, -1, 1], pa.timestamp("us", tz="America/New_York")),
    "g": pa.array([0.1, 1.0, None, 2.5, -4.0], pa.float32()),
    "d": pa.array([0, 1, None, -1, 19_723], pa.date32()),
    "s": pa.array(["a", None, "b", "", "c"]),
}
f_casts = write("casts.in.arrow", casts)
B_CASTS = pa.record_batch(casts)
o = [("double_to_int32", {"cast": col("f"), "to": "int32", "safe": False}),
     ("int_to_int8_unsafe", {"cast": col("i"), "to": "int8", "safe": False}),
     ("int_to_double", {"cast": col("i"), "to": "double"}),
     ("uint_to_int_unsafe", {"cast": col("u"), "to": "int64", "safe": False}),
     ("uint_to_double", {"cast": col("u"), "to": "double", "safe": False}),
     ("bool_to_double", {"cast": col("b"), "to": "double"}),
     ("bool_to_int8", {"cast": col("b"), "to": "int8"}),
     ("double_to_bool", {"cast": col("f"), "to": "bool"}),
     ("int_to_bool", {"cast": col("i"), "to": "bool"}),
     ("single_to_double", {"cast": col("g"), "to": "double"}),
     ("double_to_single", {"cast": col("f"), "to": "float"}),
     ("us_to_s_unsafe", {"cast": col("t"), "to": "timestamp[s]", "safe": False}),
     ("us_to_ns", {"cast": col("t"), "to": "timestamp[ns]"}),
     ("timestamp_to_int", {"cast": col("t"), "to": "int64"}),
     ("int_to_timestamp", {"cast": col("i"), "to": "timestamp[ms]"}),
     ("timestamp_to_date", {"cast": col("t"), "to": "date32"}),
     ("zoned_to_date", {"cast": col("z"), "to": "date32"}),
     ("zone_change", {"cast": col("t"), "to": "timestamp[us, tz=UTC]"}),
     ("date_to_timestamp", {"cast": col("d"), "to": "timestamp[ms]"}),
     ("date_to_int32", {"cast": col("d"), "to": "int32"}),
     ("int_to_duration", {"cast": col("i"), "to": "duration[s]"}),
     ("string_to_large", {"cast": col("s"), "to": "large_string"})]
exp = run(B_CASTS, o)
# Out of range, the spec holds a float at the target's ends (Arrow C++ leaves it undefined).
held = [None if v is None else max(-(2**31), min(2**31 - 1, int(math.trunc(v)))) for v in casts["f"].to_pylist()]
exp = exp.set_column(0, pa.field("double_to_int32", pa.int32()), pa.array(held, pa.int32()))
case("casts", nulls_tags("overflow", "params"),
     "The table of casts: numbers, booleans, timestamps by unit, zone and date (a zoned "
     "timestamp's local date), dates, durations and strings; unsafe casts wrap integers, "
     "truncate times toward zero and hold an out-of-range float at the target's ends.",
     o, f_casts, write("casts.expected.arrow", exp))
case("safe_cast_refuses_a_fraction", ["error"],
     "A safe cast of 2.5 to int64 refuses the batch.",
     [("x", {"cast": call("add", col("f"), lit(0.5)), "to": "int64"})], f_casts,
     error={"at": "apply", "contains": "was truncated"})
case("safe_cast_refuses_out_of_range", ["overflow", "error"],
     "A safe cast of 300 to int8 refuses the batch.",
     [("x", {"cast": col("i"), "to": "int8"})], f_casts,
     error={"at": "apply", "contains": "not in range"})
case("safe_cast_refuses_lost_time", ["error"],
     "A safe cast of 1.5 seconds in microseconds to seconds refuses the batch.",
     [("x", {"cast": col("t"), "to": "timestamp[s]"})], f_casts,
     error={"at": "apply", "contains": "would lose data"})
case("implicit_cast_refuses_int_past_2_53", ["overflow", "error"],
     "A uint64 of 2^64-1 added to a double does not become a double: the batch is refused.",
     [("x", call("add", col("u"), lit(1.0)))], f_casts,
     error={"at": "apply", "contains": "not in range"})

# ------------------------------------------------------------------ temporal
US = 1_000_000
times = {
    "t": pa.array([-1, 0, 1_704_067_200 * US + 17 * 3600 * US + 123_456, None,
                   951_782_400 * US, -2_208_988_800 * US + 1, 1_609_632_000 * US], pa.timestamp("us")),
    "z": pa.array([0, 1_710_054_000 * US, None, 1_730_613_600 * US, -1, 5, 1], pa.timestamp("us", tz="America/New_York")),
    "o": pa.array([0, -1, None, 3600, 5, 6, 7], pa.timestamp("s", tz="+05:30")),
    "n": pa.array([-1, 1_234_567_891, None, 0, 999, 1000, 1001], pa.timestamp("ns")),
    "d": pa.array([-1, 19_723, None, 11_016, 0, 18_628, 20_088], pa.date32()),
}
f_times = write("temporal.in.arrow", times)
PARTS = ["year", "quarter", "month", "day", "day_of_week", "day_of_year", "iso_year", "iso_week",
         "hour", "minute", "second", "millisecond", "microsecond", "nanosecond"]
o = [(f"{p}_{c}", call(p, col(c))) for c in "tzon" for p in PARTS]
o += [(f"{p}_d", call(p, col("d"))) for p in PARTS[:8]]
o += [("day_of_week_sunday1", call("day_of_week", col("t"), count_from_zero=False, week_start=7))]
case("temporal_parts", nulls_tags(),
     "Every temporal part of timestamps of each unit, before and after 1970, in New York (across "
     "both 2024 clock changes) and at +05:30, and the date parts of date32.",
     o, f_times, write("temporal.expected.arrow", run(pa.record_batch(times), o)))
case("time_parts_of_a_date_refused", ["error"],
     "hour of a date32 has no kernel: refused at plan.",
     [("x", call("hour", col("d")))], f_times, error={"at": "schema", "contains": "no kernel"})

tarith = {
    "a": pa.array([1, 5 * US, None, -US], pa.timestamp("us")),
    "s": pa.array([5, 1, 3, None], pa.timestamp("s")),
    "k": pa.array([5, -5, 7, 0], pa.duration("us")),
    "x": pa.array([5, 3, None, -1], pa.date32()),
    "y": pa.array([3, 5, 1, 1], pa.date32()),
    "m": pa.array([2**63 - 1, 0, None, 1], pa.timestamp("us")),
}
f_tarith = write("temporal_arithmetic.in.arrow", tarith)
o = [("ts_minus_ts", call("subtract", col("a"), col("s"))),
     ("ts_plus_duration", call("add", col("s"), col("k"))),
     ("duration_plus_ts", call("add", col("k"), col("a"))),
     ("ts_minus_duration", call("subtract", col("a"), col("k"))),
     ("durations", call("add", col("k"), col("k"))),
     ("days", call("subtract", col("x"), col("y"))),
     ("times_three", call("multiply", col("k"), lit(3))),
     ("as_int", {"cast": call("subtract", col("a"), col("s")), "to": "int64"})]
case("temporal_arithmetic", nulls_tags(),
     "Timestamps minus timestamps of two units, plus and minus durations, dates minus dates (in "
     "seconds) and durations times integers, as Arrow types them.",
     o, f_tarith, write("temporal_arithmetic.expected.arrow", run(pa.record_batch(tarith), o)))
case("checked_time_overflow_refused", ["overflow", "error"],
     "add_checked past the last microsecond timestamp refuses the batch.",
     [("x", call("add_checked", col("m"), col("k")))], f_tarith,
     error={"at": "apply", "contains": "overflow"})

# ------------------------------------------------------------------ rounding
MODES = ["down", "up", "towards_zero", "towards_infinity", "half_down", "half_up",
         "half_towards_zero", "half_towards_infinity", "half_to_even", "half_to_odd"]


def exact_round(x, ndigits, mode):
    """The definition: the exact binary value, rounded to a multiple of 10^-ndigits by `mode`,
    then the nearest double; a zero keeps x's sign."""
    if x is None:
        return None
    if math.isnan(x) or math.isinf(x) or x == 0.0:
        return x
    d = decimal.Decimal(x)
    unit = decimal.Decimal(1).scaleb(-ndigits)
    lo = (d / unit).to_integral_value(rounding=decimal.ROUND_FLOOR)
    rest = d / unit - lo  # exact: the context has the precision
    neg = x < 0
    half = decimal.Decimal("0.5")
    if rest == 0:
        n = lo
    elif mode in ("down", "up", "towards_zero", "towards_infinity"):
        up = {"down": False, "up": True, "towards_zero": neg, "towards_infinity": not neg}[mode]
        n = lo + 1 if up else lo
    elif rest != half:
        n = lo + 1 if rest > half else lo
    else:
        up = {"half_down": False, "half_up": True, "half_towards_zero": neg,
              "half_towards_infinity": not neg, "half_to_even": lo % 2 != 0,
              "half_to_odd": lo % 2 == 0}[mode]
        n = lo + 1 if up else lo
    r = float(n * unit)
    return math.copysign(r, x) if r == 0.0 else r


decimal.getcontext().prec = 2000
RX = [2.5, -2.5, 2.45, 7.095, -0.001, 1250.0, 0.125, -86.64999999999999, NAN, INF, None, 1e300]
rin = {"x": pa.array(RX), "i": pa.array([15, 25, -15, 250, None, 7, 0, -5, 1, 2, 3, 4], pa.int64()),
       "g": pa.array([2.675, -2.5, 0.125, 1.5, None, 7.0, 0.5, 1e30, NAN, 3.0, 4.0, 5.0], pa.float32())}
f_round = write("round.in.arrow", rin)
o, cols = [], []
for m in MODES:
    for nd in (0, 2, -2):
        name = f"{m}_{'m' if nd < 0 else ''}{abs(nd)}"
        o.append((name, call("round", col("x"), ndigits=nd, round_mode=m)))
        cols.append((name, pa.array([exact_round(v, nd, m) for v in RX], pa.float64())))
    o.append((f"{m}_int", call("round", col("i"), ndigits=-1, round_mode=m)))
    ints_r = [None if v is None else int(exact_round(float(v), -1, m)) for v in rin["i"].to_pylist()]
    cols.append((f"{m}_int", pa.array(ints_r, pa.int64())))
o.append(("single_2", call("round", col("g"), ndigits=2)))
cols.append(("single_2", pa.array([None if v is None else (v if math.isnan(v) else
              float(np.float32(exact_round(float(np.float32(v)), 2, "half_to_even")))) for v in rin["g"].to_pylist()],
              pa.float32())))
o += [("floor", call("floor", col("x"))), ("ceil", call("ceil", col("x"))),
      ("trunc", call("trunc", col("x"))), ("floor_int", call("floor", col("i")))]
B_ROUND = pa.record_batch(rin)
cols += [(n, ev(B_ROUND, e)) for n, e in o[-4:]]
case("round_every_mode", ["rounding", "nulls"],
     "round in each of the ten modes at 0, 2 and -2 digits, on the exact binary value (7.095 is "
     "7.09 at two digits), a zero keeping its sign; integers to tens; singles; floor, ceil, trunc.",
     o, f_round, write("round.expected.arrow", out(cols)))
case("round_int_overflow_refused", ["overflow", "error"],
     "Rounding int8's 127 up to tens refuses the batch.",
     [("x", call("round", lit(127, "int8"), ndigits=-1, round_mode="up"))],
     f_round, error={"at": "apply", "contains": "would overflow"})


def month_round_values(path):
    """The unrounded Audit fares of a month on which pyarrow's round(x, 2) and Python's differ."""
    import pyarrow.parquet as pq
    b = pq.read_table(path).combine_chunks().to_batches()[0]
    dist = b["trip_distance"].to_pylist()
    pu = b["pickup_at"].cast(pa.int64()).to_pylist()
    do = b["dropoff_at"].cast(pa.int64()).to_pylist()
    rate = b["ratecode_id"].to_pylist()
    es = []
    for d, a, bb, r in zip(dist, pu, do, rate):
        minutes = max((bb - a) / 60e6, 0.0)
        if r == 2:
            e = 70.0
        else:
            e = 3.0 + 3.5 * (d or 0.0) + 0.35 * max(minutes - (d or 0.0) * 2.0, 0.0)
            if 16 <= (a // 3_600_000_000) % 24 < 20:
                e += 2.5
        es.append(e)
    arrow = pc.round(pa.array(es), 2, round_mode="half_to_even").to_pylist()
    return [e for e, q in zip(es, arrow) if round(e, 2) != q]


month = sys.argv[sys.argv.index("--month") + 1] if "--month" in sys.argv else None
if month:
    write("round_python.in.arrow", {"e": pa.array(month_round_values(month), pa.float64())})
FILES.add("round_python.in.arrow")
with pa.OSFile(os.path.join(HERE, "round_python.in.arrow")) as f:
    E = pa.ipc.open_file(f).read_all()["e"].to_pylist()
assert len(E) == 53_698, len(E)
case("round_python_cases", ["rounding"],
     "The 53,698 Audit fares of 2024-01 that pyarrow 24 rounds otherwise give Python's "
     "round(e, 2): half to even on the exact binary value.",
     [("expected_fare", call("round", col("e"), ndigits=2, round_mode="half_to_even"))],
     "round_python.in.arrow",
     write("round_python.expected.arrow", out([("expected_fare", pa.array([round(e, 2) for e in E]))])))

# ------------------------------------------------------------------ maths
MX = [-1.0, 0.0, 0.5, 4.0, None, INF, NAN, 1e-300]
maths = {"x": pa.array(MX), "i": pa.array([4, 0, 1, 9, None, 2, 3, 16], pa.int32()),
         "g": pa.array([0.5, 1.0, None, 2.0, 0.0, -1.0, 3.0, 4.0], pa.float32())}
f_maths = write("maths.in.arrow", maths)
B_MATHS = pa.record_batch(maths)
TRANS = ["exp", "ln", "log10", "log2", "log1p", "sin", "cos", "tan", "asin", "acos", "atan"]
o = [(f, call(f, col("x"))) for f in TRANS]
o += [("atan2", call("atan2", col("x"), lit(2))), ("power", call("power", col("x"), lit(0.5))),
      ("sin_single", call("sin", col("g"))), ("sqrt", call("sqrt", col("x"))),
      ("sqrt_int", call("sqrt", col("i"))), ("ln_checked", call("ln_checked", col("i")))]
exp = run(B_MATHS, [(n, e) for n, e in o if n != "ln_checked"])
exp = exp.append_column(pa.field("ln_checked", pa.float64()),
                        pa.array([None if v is None or v == 0 else math.log(v) for v in maths["i"].to_pylist()]))
# ln_checked of 0 refuses; the case's column takes ln of the integers but row 1's 0.
o[-1] = ("ln_checked", call("ln_checked", call("if_else", call("equal", col("i"), lit(0)),
                                                {"literal": None, "type": "int32"}, col("i"))))


def ulps(values, single=False):
    """Twice the unit in the last place of each expected value; null (exact) for NaN, an
    infinity or a null."""
    res = []
    for v in values:
        if v is None or math.isnan(v) or math.isinf(v):
            res.append(None)
        else:
            res.append(2.0 * float(np.spacing(np.float32(abs(v)))) if single else 2.0 * math.ulp(v))
    return pa.array(res, pa.float64())


allowed = {}
for n in TRANS + ["atan2", "power", "sin_single", "ln_checked"]:
    allowed[n] = write(f"maths.{n}.allowed.arrow",
                       {n: ulps(exp[n].to_pylist(), single=(n == "sin_single"))})
case("unary_maths", nulls_tags(),
     "Each transcendental function within two units in the last place of pyarrow's; sqrt exact; "
     "an integer argument as a double, a single as a single.",
     o, f_maths, write("maths.expected.arrow", exp), allowed=allowed)
case("ln_checked_refuses_negative", ["error"],
     "ln_checked of -1.0 refuses the batch.",
     [("x", call("ln_checked", col("x")))], f_maths,
     error={"at": "apply", "contains": "logarithm of negative number"})
case("sqrt_checked_refuses_negative", ["error"],
     "sqrt_checked of -1.0 refuses the batch.",
     [("x", call("sqrt_checked", col("x")))], f_maths,
     error={"at": "apply", "contains": "square root of negative number"})

# ------------------------------------------------------------------ Audit and Enrich
TRIP = pa.schema([("vendor_id", pa.int64()), ("pickup_at", pa.timestamp("us")),
                  ("dropoff_at", pa.timestamp("us")), ("passenger_count", pa.float64()),
                  ("trip_distance", pa.float64()), ("ratecode_id", pa.float64()),
                  ("store_and_fwd_flag", pa.string()), ("pu_location_id", pa.int64()),
                  ("do_location_id", pa.int64()), ("payment_type", pa.int64()),
                  ("fare_amount", pa.float64()), ("extra", pa.float64()), ("mta_tax", pa.float64()),
                  ("tip_amount", pa.float64()), ("tolls_amount", pa.float64()),
                  ("improvement_surcharge", pa.float64()), ("total_amount", pa.float64()),
                  ("congestion_surcharge", pa.float64()), ("airport_fee", pa.float64())])
T0 = 1_704_067_200 * US  # 2024-01-01T00:00


def trips(rows):
    """Trips from (pickup, minutes, distance, rate, fare, tip) rows; the rest filled in."""
    cols = {f.name: [] for f in TRIP}
    for k, (pu, minutes, dist, rate, fare, tip) in enumerate(rows):
        cols["vendor_id"].append(1 + k % 2)
        cols["pickup_at"].append(pu)
        cols["dropoff_at"].append(None if pu is None or minutes is None else pu + int(minutes * 60 * US))
        cols["passenger_count"].append(None if k % 5 == 0 else 1.0)
        cols["trip_distance"].append(dist)
        cols["ratecode_id"].append(rate)
        cols["store_and_fwd_flag"].append(None if rate is None else "N")
        cols["pu_location_id"].append(100 + k)
        cols["do_location_id"].append(200 + k)
        cols["payment_type"].append(1)
        cols["fare_amount"].append(fare)
        for c in ["extra", "mta_tax", "improvement_surcharge", "congestion_surcharge", "airport_fee"]:
            cols[c].append(None if rate is None else 0.5)
        cols["tip_amount"].append(tip)
        cols["tolls_amount"].append(0.0)
        cols["total_amount"].append(None if fare is None else fare + 1.0)
    return pa.record_batch([pa.array(cols[f.name], f.type) for f in TRIP], schema=TRIP)


H = 3600 * US
# e = 3 + 3.5 d at zero minutes past 2d; this d gives e = 30.0039..., which rounds to 30.0, and
# the fare e * 1.25 - 1e-9 is within 0.25 e of e but not within 0.25 * 30.0 of 30.0.
D_TIE = (30.0039 - 3.0) / 3.5
E_TIE = 3.0 + 3.5 * D_TIE
AUDIT_ROWS = [
    (T0 + 10 * H, 12.0, 2.5, 1.0, 14.0, 2.0),          # standard, not peak
    (T0 + 16 * H, 30.0, 5.0, 1.0, 60.0, 0.0),          # peak (16:00), flagged
    (T0 + 19 * H + 59 * 60 * US, 10.0, 1.0, 1.0, 9.0, 1.0),  # still peak at 19:59
    (T0 + 20 * H, 10.0, 1.0, 1.0, 9.0, 1.0),           # 20:00 is not peak
    (T0 + 15 * H, 45.0, 3.0, 2.0, 70.0, 5.0),          # JFK flat rate
    (T0 + 17 * H, 45.0, 3.0, 2.0, 52.0, 5.0),          # flat rate, flagged
    (T0 + 9 * H, 20.0, None, 1.0, 12.0, 0.0),          # a null distance counts as zero
    (T0 + 9 * H, 20.0, 4.0, None, 18.0, 0.0),          # a null rate code takes the standard branch
    (T0 + 9 * H, 20.0, 4.0, 1.0, None, 0.0),           # a null fare is never flagged
    (T0 + 9 * H, -5.0, 2.0, 5.0, 1.0, 0.0),            # dropoff before pickup: zero minutes
    (T0 + 9 * H, 0.0, 0.0, 1.0, 3.0, 0.0),             # zero distance, zero time
    (T0 + 9 * H, 3.0, D_TIE, 1.0, E_TIE * 1.25 - 1e-9, 0.0),  # flag by the unrounded fare
    (T0 + 9 * H, 7.0, 1.37, 1.0, NAN, 0.0),            # a NaN fare is not flagged
    (T0 - 1, 61.0, 10.3, 99.0, 50.0, 0.0),             # pickup a microsecond before 2024
    (T0 + 12 * H, 9.0, 0.81, 1.0, 7.1, 0.5),           # 7.095-like ties at the cents
]
B_AUDIT = trips(AUDIT_ROWS)
f_audit = write("audit.in.arrow", B_AUDIT)
AUDIT = expr_trees.audit_tree()
want = workloads.audit(B_AUDIT)
assert want["flagged"].to_pylist()[11] is False and want["flagged"].to_pylist()[1] is True
case("audit_rule", ["nulls", "rounding"],
     "Audit's rule as one tree gives bench/vm/workloads.py's audit rows: a null distance counts "
     "as zero, a null rate code takes the standard branch, a null fare is never flagged, and the "
     "flag reads the unrounded fare while the output is rounded half to even at two decimals.",
     None, f_audit, write("audit.expected.arrow", out(list(zip(want.schema.names, want.columns)))),
     params=AUDIT)
case("audit_rule_sliced", ["sliced", "nulls"],
     "Audit's rule on rows 5 to 11 of a batch, given at an offset, gives those rows' audit.",
     None, [{"file": f_audit, "slice": [5, 7]}],
     write("audit_sliced.expected.arrow", out(list(zip(want.schema.names, [c.slice(5, 7) for c in want.columns])))),
     params=AUDIT)
NULL_TIMES = [(None, 10.0, 1.0, 1.0, 9.0, 0.0), (T0, None, 1.0, 1.0, None, 0.0), (T0, 5.0, 1.0, 1.0, 9.0, 0.0)]
B_NT = trips(NULL_TIMES)
ok = workloads.audit(trips(NULL_TIMES[2:]))
nt = out([("vendor_id", B_NT["vendor_id"]), ("pu_location_id", B_NT["pu_location_id"]),
          ("fare_amount", B_NT["fare_amount"]),
          ("expected_fare", pa.array([None, None] + ok["expected_fare"].to_pylist(), pa.float64())),
          ("flagged", pa.array([None, False] + ok["flagged"].to_pylist(), pa.bool_()))])
case("audit_rule_null_times", ["nulls"],
     "Where a pickup or dropoff is null (Audit's Python loop raises there) the fare is null and "
     "the flag null, or false where the fare is null too.",
     None, write("audit_null_times.in.arrow", B_NT), write("audit_null_times.expected.arrow", nt),
     params=AUDIT)

ENRICH_ROWS = AUDIT_ROWS + [(T0, 0.5, 1.0, 1.0, 5.0, 1.0), (T0, 300.0, 1.0, 1.0, 5.0, 1.0),
                            (T0, 10.0, 100.0, 1.0, 5.0, 1.0), (T0, 10.0, NAN, 1.0, 5.0, 1.0),
                            (T0, 10.0, 2.0, 1.0, 0.0, 1.0), (T0, 240.0, 2.0, 1.0, -3.0, 1.0)]
B_ENRICH = trips(ENRICH_ROWS)
f_enrich = write("enrich.in.arrow", B_ENRICH)


def enrich_columns(batch):
    """workloads.enrich's sixteen calls, its columns before the filter, and the mask."""
    dur = pc.divide(pc.cast(pc.subtract(batch["dropoff_at"], batch["pickup_at"]), pa.int64()), 1_000_000.0)
    dist, fare, tip = batch["trip_distance"], batch["fare_amount"], batch["tip_amount"]
    keep = pc.and_(pc.and_(pc.greater_equal(dur, 60.0), pc.less_equal(dur, 4 * 3600.0)),
                   pc.and_(pc.and_(pc.greater(dist, 0.0), pc.less(dist, 100.0)), pc.greater(fare, 0.0)))
    cols = list(zip(batch.schema.names, batch.columns))
    cols += [("duration_s", dur), ("mph", pc.divide(dist, pc.divide(dur, 3600.0))),
             ("fare_per_mile", pc.divide(fare, dist)), ("tip_share", pc.divide(tip, fare)), ("keep", keep)]
    return out(cols)


exp = enrich_columns(B_ENRICH)
filtered = exp.filter(exp["keep"]).drop_columns(["keep"])
want = workloads.enrich(B_ENRICH)
assert filtered.equals(want), "Enrich's columns filtered by keep are workloads.enrich's rows"
case("enrich_columns", ["nulls"],
     "Enrich's derived columns and its filter's mask as one tree give workloads.enrich's sixteen "
     "calls; the input columns pass through.",
     None, f_enrich, write("enrich.expected.arrow", exp), params=expr_trees.enrich_tree())
EMPTY = B_ENRICH.slice(0, 0)
case("enrich_empty", ["empty"],
     "A batch of zero rows gives zero rows of Enrich's output schema.",
     None, write("enrich_empty.in.arrow", EMPTY), write("enrich_empty.expected.arrow", enrich_columns(EMPTY)),
     params=expr_trees.enrich_tree())
case("enrich_two_batches", ["multi_batch"],
     "Each of two batches gives its own rows' columns, in order.",
     None, write("enrich_two.in.arrow", B_ENRICH.slice(0, 9), B_ENRICH.slice(9)),
     "enrich.expected.arrow", params=expr_trees.enrich_tree())
case("constant_error_only_with_rows", ["empty"],
     "A constant subtree that would refuse (1.0 / 0.0 checked) is not evaluated on zero rows.",
     [("x", call("divide_checked", lit(1.0), lit(0.0)))], "enrich_empty.in.arrow",
     write("constant_empty.expected.arrow", out([("x", pa.array([], pa.float64()))])))
case("constant_error_with_rows", ["error"],
     "The same constant subtree refuses a batch that has rows.",
     [("x", call("divide_checked", lit(1.0), lit(0.0)))], f_enrich,
     error={"at": "apply", "contains": "divide by zero"})

# ------------------------------------------------------------------ refusals
case("malformed_tree_refused", ["params", "error"],
     "A node with an unknown key is refused at configure.",
     None, f_enrich, error={"at": "configure", "contains": "unknown key"},
     params={"outputs": [{"name": "x", "expr": {"column": "fare_amount", "as": "y"}}]})
case("unknown_column_refused", ["error"],
     "A column the input does not have is refused at plan.",
     [("x", col("fare"))], f_enrich, error={"at": "schema", "contains": "no column `fare`"})
case("no_kernel_refused", ["error"],
     "add of a string and a number is refused at plan.",
     [("x", call("add", col("store_and_fwd_flag"), lit(1)))], f_enrich,
     error={"at": "schema", "contains": "no kernel"})
case("unknown_function_refused", ["error"],
     "A function outside the vocabulary is refused at plan.",
     [("x", call("modulo", col("fare_amount"), lit(2.0)))], f_enrich,
     error={"at": "schema", "contains": "no function `modulo`"})

for name in os.listdir(HERE):
    if name.endswith(".arrow") and name not in FILES:
        os.remove(os.path.join(HERE, name))
index = {"format": "moruna-kernel-corpus/1", "kernel": "moruna.expr", "cases": CASES}
with open(os.path.join(HERE, "index.json"), "w") as f:
    json.dump(index, f, indent=1)
    f.write("\n")
print(f"{len(CASES)} cases, {len(FILES)} files")
