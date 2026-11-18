"""Writes moruna.filter's conformance corpus: index.json and the Arrow IPC files it names.

    python3 -I crates/moruna-kernels/conformance/filter/make.py

The expected outputs are pyarrow's (`RecordBatch.filter`, which drops a row whose predicate is
null), or written out by hand; never the CPU reference's. Rerunning it writes the same files.
"""
import json
import os

import pyarrow as pa
import pyarrow.compute as pc

HERE = os.path.dirname(os.path.abspath(__file__))
CASES = []


def write(name, batches, schema=None):
    """An Arrow IPC file of `batches` (a list), with `schema` when there are none."""
    schema = schema or batches[0].schema
    with pa.OSFile(os.path.join(HERE, name), "wb") as sink:
        with pa.ipc.new_file(sink, schema) as writer:
            for b in batches:
                writer.write_batch(b)
    return name


def source(file, sliced=None):
    out = {"file": file}
    if sliced is not None:
        out["slice"] = list(sliced)
    return out


def case(name, tests, about, params, inputs, expected=None, error=None):
    entry = {"name": name, "version": 1, "tests": tests, "about": about}
    if params:
        entry["params"] = params
    entry["inputs"] = inputs
    if error is not None:
        entry["error"] = error
    else:
        entry["expected"] = expected
    CASES.append(entry)


def without(batch, column):
    return batch.drop_columns([column])


def selection(mask):
    """The positions `mask` selects, as the selection form gives them."""
    rows = [i for i, v in enumerate(mask.to_pylist()) if v is True]
    schema = pa.schema([pa.field("selection", pa.uint32(), nullable=False)])
    return pa.record_batch([pa.array(rows, pa.uint32())], schema=schema)


def expect_batch(name, batch, predicate="p"):
    """The batch form's expected output of `batch`, by pyarrow."""
    out = without(batch.filter(batch[predicate]), predicate)
    return write(name, [out], out.schema)


# A batch of ten rows: ints with a null, strings with a null, floats with NaN and -0.0, and a
# predicate with nulls.
TEN = pa.record_batch(
    {
        "id": pa.array([0, 1, 2, 3, None, 5, 6, 7, 8, 9], pa.int64()),
        "name": pa.array(["a", None, "c", "d", "e", "f", None, "h", "i", "j"]),
        "x": pa.array([0.5, float("nan"), -0.0, 3.25, None, 5.0, 6.5, -7.0, 8.0, 1e300]),
        "p": pa.array([True, False, None, True, True, None, False, True, True, False]),
    }
)
write("ten.in.arrow", [TEN])

case("keeps_true_rows", ["nulls", "ordering", "params"],
     "Only rows whose predicate is true are kept, in order, without the predicate column; a "
     "null predicate drops its row, and nulls, NaN and -0.0 in kept rows are kept as given.",
     {"predicate": "p"}, [source("ten.in.arrow")], expect_batch("ten.expected.arrow", TEN))

case("default_form_is_batch", ["params"],
     "With `output` given as \"batch\" the kernel gives what it gives with no `output`.",
     {"predicate": "p", "output": "batch"}, [source("ten.in.arrow")], "ten.expected.arrow")

ALL_TRUE = TEN.set_column(3, "p", pa.array([True] * 10))
write("all_true.in.arrow", [ALL_TRUE])
case("all_true", ["all_true"], "A predicate of all true gives every row, without the predicate.",
     {"predicate": "p"}, [source("all_true.in.arrow")],
     expect_batch("all_true.expected.arrow", ALL_TRUE))

ALL_FALSE = TEN.set_column(3, "p", pa.array([False] * 10))
write("all_false.in.arrow", [ALL_FALSE])
case("all_false", ["all_false", "empty"], "A predicate of all false gives a batch of no rows.",
     {"predicate": "p"}, [source("all_false.in.arrow")],
     expect_batch("all_false.expected.arrow", ALL_FALSE))

ALL_NULL = TEN.set_column(3, "p", pa.array([None] * 10, pa.bool_()))
write("all_null.in.arrow", [ALL_NULL])
case("all_null", ["nulls", "all_false"], "A predicate of all nulls selects nothing.",
     {"predicate": "p"}, [source("all_null.in.arrow")], "all_false.expected.arrow")

EMPTY = TEN.slice(0, 0)
write("empty.in.arrow", [EMPTY], TEN.schema)
case("empty_batch", ["empty"], "A batch of zero rows gives a batch of zero rows.",
     {"predicate": "p"}, [source("empty.in.arrow")], expect_batch("empty.expected.arrow", EMPTY))

case("no_batches", ["empty"], "No batches give no output.",
     {"predicate": "p"}, [source("none.in.arrow")], "none.expected.arrow")
write("none.in.arrow", [], TEN.schema)
write("none.expected.arrow", [], without(TEN, "p").schema)

# A slice whose offset is not a multiple of 8, so the predicate's bits start mid-byte.
SLICED = TEN.slice(3, 6)
case("sliced", ["sliced", "nulls"],
     "A batch whose arrays start at row 3 of their buffers is filtered from its own first row.",
     {"predicate": "p"}, [source("ten.in.arrow", (3, 6))],
     expect_batch("sliced.expected.arrow", SLICED))

TWO = [TEN.slice(0, 4), TEN.slice(4, 6)]
write("two.in.arrow", TWO)
case("two_batches", ["multi_batch"],
     "Each batch is filtered on its own, and the outputs follow the batches' order.",
     {"predicate": "p"}, [source("two.in.arrow")], "ten.expected.arrow")

# Nested and dictionary columns are filtered as any other.
NESTED = pa.record_batch(
    {
        "tags": pa.array([[1, 2], [], None, [3], [None, 4], [5]], pa.list_(pa.int64())),
        "point": pa.array([{"x": 1.0, "y": "a"}, None, {"x": None, "y": "c"},
                           {"x": 4.0, "y": None}, {"x": 5.0, "y": "e"}, {"x": 6.0, "y": "f"}],
                          pa.struct([("x", pa.float64()), ("y", pa.string())])),
        "zone": pa.array(["n", "s", None, "n", "e", "s"]).dictionary_encode(),
        "p": pa.array([True, True, False, None, True, True]),
    }
)
write("nested.in.arrow", [NESTED])
case("nested_and_dictionary", ["nested", "dictionary", "nulls"],
     "List, struct and dictionary columns keep the selected rows' values, nulls included.",
     {"predicate": "p"}, [source("nested.in.arrow")], expect_batch("nested.expected.arrow", NESTED))

ONLY = pa.record_batch({"p": pa.array([True, False, True, None])})
write("only.in.arrow", [ONLY])
case("predicate_only", ["empty"],
     "A batch of the predicate alone gives a batch of no columns with the selected rows' count.",
     {"predicate": "p"}, [source("only.in.arrow")], expect_batch("only.expected.arrow", ONLY))

NOT_NULL = pa.record_batch([pa.array([1, 2, 3], pa.int32()), pa.array([False, True, True])],
                           schema=pa.schema([pa.field("v", pa.int32(), nullable=False),
                                             pa.field("keep", pa.bool_(), nullable=False)]))
write("not_null.in.arrow", [NOT_NULL])
case("named_predicate_and_nullability", ["params"],
     "The predicate is the column `predicate` names, and the other fields keep their "
     "nullability.",
     {"predicate": "keep"}, [source("not_null.in.arrow")],
     expect_batch("not_null.expected.arrow", NOT_NULL, "keep"))

# The selection form.
def expect_selection(name, batch):
    return write(name, [selection(batch["p"])])


case("selection", ["params", "nulls"],
     "The selection form gives the positions of the rows whose predicate is true, ascending.",
     {"predicate": "p", "output": "selection"}, [source("ten.in.arrow")],
     expect_selection("ten.selection.arrow", TEN))
case("selection_sliced", ["sliced"],
     "A sliced batch's positions count from its own first row.",
     {"predicate": "p", "output": "selection"}, [source("ten.in.arrow", (3, 6))],
     expect_selection("sliced.selection.arrow", SLICED))
case("selection_all_true", ["all_true"], "A predicate of all true selects every position.",
     {"predicate": "p", "output": "selection"}, [source("all_true.in.arrow")],
     expect_selection("all_true.selection.arrow", ALL_TRUE))
case("selection_all_false", ["all_false"], "A predicate of all false selects no position.",
     {"predicate": "p", "output": "selection"}, [source("all_false.in.arrow")],
     expect_selection("all_false.selection.arrow", ALL_FALSE))
case("selection_empty", ["empty"], "A batch of zero rows selects no position.",
     {"predicate": "p", "output": "selection"}, [source("empty.in.arrow")],
     "all_false.selection.arrow")
case("selection_two_batches", ["multi_batch"],
     "Each batch's positions count from its own first row.",
     {"predicate": "p", "output": "selection"}, [source("two.in.arrow")],
     write("two.selection.arrow", [selection(TWO[0]["p"]), selection(TWO[1]["p"])]))

# Refusals.
case("no_predicate_parameter", ["params", "error"], "A configure without `predicate` is refused.",
     {}, [source("ten.in.arrow")], error={"at": "configure"})
case("unknown_output", ["params", "error"], "An `output` other than batch or selection is "
     "refused.", {"predicate": "p", "output": "mask"}, [source("ten.in.arrow")],
     error={"at": "configure"})
case("unknown_parameter", ["params", "error"], "A parameter the kernel does not take is refused.",
     {"predicate": "p", "expr": "x > 1"}, [source("ten.in.arrow")], error={"at": "configure"})
case("missing_predicate_column", ["error"],
     "An input with no column of the predicate's name is refused at plan.",
     {"predicate": "keep"}, [source("ten.in.arrow")],
     error={"at": "schema", "contains": "keep"})
case("predicate_not_boolean", ["error"], "A predicate column that is not Boolean is refused at "
     "plan.", {"predicate": "id"}, [source("ten.in.arrow")],
     error={"at": "schema", "contains": "Boolean"})

index = {"format": "moruna-kernel-corpus/1", "kernel": "moruna.filter", "cases": CASES}
with open(os.path.join(HERE, "index.json"), "w") as f:
    json.dump(index, f, indent=2)
    f.write("\n")
