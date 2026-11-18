"""Audit's rule and Enrich's columns as `moruna.expr` trees (work item 4.2, design 4B).

Each tree is written step for step as `bench/vm/workloads.py` computes it, in the same order of
floating-point operations, so it gives the same bits:

- Enrich: the four derived columns and the filter's mask (`keep`), after the input's 19 columns
  passed through. `expr` then `filter` (4.3) on `keep` gives Enrich's rows.
- Audit: the re-priced fare and the flag. Its four behaviours: a null distance counts as zero
  (`coalesce`); a null rate code takes the standard branch (an `if` clause whose condition is
  null counts as false); a null fare is never flagged (`and_kleene` with `is_valid`); the flag
  reads the unrounded fare, and the output is rounded half to even at two decimals on the exact
  binary value, as Python's `round` does.

Usage: expr_trees.py OUT_DIR   (writes audit.json and enrich.json)
"""
import json
import os
import sys

ENRICH_INPUT = ["vendor_id", "pickup_at", "dropoff_at", "passenger_count", "trip_distance",
                "ratecode_id", "store_and_fwd_flag", "pu_location_id", "do_location_id",
                "payment_type", "fare_amount", "extra", "mta_tax", "tip_amount", "tolls_amount",
                "improvement_surcharge", "total_amount", "congestion_surcharge", "airport_fee"]


def col(name):
    return {"column": name}


def lit(value, ty=None):
    return {"literal": value} if ty is None else {"literal": value, "type": ty}


def call(name, *args, **options):
    node = {"call": name, "args": list(args)}
    if options:
        node["options"] = options
    return node


def duration_us():
    """dropoff_at - pickup_at as its int64 microseconds: `pc.cast(pc.subtract(b, a), int64)`."""
    return {"cast": call("subtract", col("dropoff_at"), col("pickup_at")), "to": "int64"}


def enrich_tree():
    dur = call("divide", duration_us(), lit(1_000_000.0))
    dist, fare, tip = col("trip_distance"), col("fare_amount"), col("tip_amount")
    keep = call("and",
                call("and", call("greater_equal", dur, lit(60.0)), call("less_equal", dur, lit(4 * 3600.0))),
                call("and",
                     call("and", call("greater", dist, lit(0.0)), call("less", dist, lit(100.0))),
                     call("greater", fare, lit(0.0))))
    outputs = [{"name": c, "expr": col(c)} for c in ENRICH_INPUT]
    outputs += [
        {"name": "duration_s", "expr": dur},
        {"name": "mph", "expr": call("divide", dist, call("divide", dur, lit(3600.0)))},
        {"name": "fare_per_mile", "expr": call("divide", fare, dist)},
        {"name": "tip_share", "expr": call("divide", tip, fare)},
        {"name": "keep", "expr": keep},
    ]
    return {"outputs": outputs}


def audit_fare():
    """The unrounded fare `e` of workloads.audit."""
    d = call("coalesce", col("trip_distance"), lit(0.0))  # (d or 0.0)
    # A null pickup or dropoff makes the fare null (Audit's Python loop raises there), so the
    # minutes' bounds keep a null rather than skip it.
    minutes = call("max_element_wise", call("divide", duration_us(), lit(60e6)), lit(0.0),
                   skip_nulls=False)
    standard = call("add",
                    call("add", lit(3.0), call("multiply", lit(3.5), d)),
                    call("multiply", lit(0.35),
                         call("max_element_wise",
                              call("subtract", minutes, call("multiply", d, lit(2.0))),
                              lit(0.0), skip_nulls=False)))
    hour = call("hour", col("pickup_at"))  # (a // 3_600_000_000) % 24
    peak = call("and", call("greater_equal", hour, lit(16)), call("less", hour, lit(20)))
    standard = call("if_else", peak, call("add", standard, lit(2.5)), standard)
    # `r == 2`: a null rate code compares null, which an `if` clause counts as false.
    return {"if": [{"when": call("equal", col("ratecode_id"), lit(2.0)), "then": lit(70.0)}],
            "else": standard}


def audit_tree():
    e = audit_fare()
    fare = col("fare_amount")
    flagged = call("and_kleene",
                   call("is_valid", fare),
                   call("greater",
                        call("abs", call("subtract", fare, e)),
                        call("max_element_wise", lit(5.0), call("multiply", lit(0.25), e))))
    return {"outputs": [
        {"name": "vendor_id", "expr": col("vendor_id")},
        {"name": "pu_location_id", "expr": col("pu_location_id")},
        {"name": "fare_amount", "expr": fare},
        {"name": "expected_fare", "expr": call("round", e, ndigits=2, round_mode="half_to_even")},
        {"name": "flagged", "expr": flagged},
    ]}


def write(out_dir):
    for name, tree in (("audit", audit_tree()), ("enrich", enrich_tree())):
        with open(os.path.join(out_dir, f"{name}.json"), "w") as f:
            json.dump(tree, f, indent=1)
            f.write("\n")


if __name__ == "__main__":
    write(sys.argv[1])
