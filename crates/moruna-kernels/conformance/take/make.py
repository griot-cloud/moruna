"""Writes moruna.take's conformance corpus: index.json and the Arrow IPC files it names.

    python3 -I crates/moruna-kernels/conformance/take/make.py

The expected outputs are pyarrow's (`pyarrow.compute.take`), or written out by hand; never the
CPU reference's. Rerunning it writes the same files.
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


def column(name, array, nullable=True):
    return pa.record_batch([array], schema=pa.schema([pa.field(name, array.type, nullable)]))


def source(file, sliced=None):
    out = {"file": file}
    if sliced is not None:
        out["slice"] = list(sliced)
    return out


def case(name, tests, about, inputs, values=None, expected=None, error=None, params=None):
    entry = {"name": name, "version": 1, "tests": tests, "about": about}
    if params:
        entry["params"] = params
    if values is not None:
        entry["arrays"] = {"values": values}
    entry["inputs"] = inputs
    if error is not None:
        entry["error"] = error
    else:
        entry["expected"] = expected
    CASES.append(entry)


def taken(name, index, values, index_nullable=True, values_nullable=True):
    """The expected output of `values` (an array, named `name`) taken at `index` (an array)."""
    out = pc.take(values, index)
    return column(name, out, index_nullable or values_nullable)


INTS = pa.array([10, None, 30, 40, 50, 60, -70, 80], pa.int64())
write("ints.values.arrow", [column("v", INTS)])

IDX = pa.array([2, 0, 7, 3, 3, 6], pa.int32())
write("idx.in.arrow", [column("i", IDX)])
case("fixed_values", ["params", "ordering"],
     "With values fixed at configure, row i of the output is the value at index row i, an index "
     "may repeat, and the output is named and typed as the values.",
     [source("idx.in.arrow")], source("ints.values.arrow"),
     write("idx.expected.arrow", [taken("v", IDX, INTS)]))

IDX_NULLS = pa.array([1, None, 0, None, 5], pa.uint32())
write("idx_nulls.in.arrow", [column("i", IDX_NULLS)])
case("nulls", ["nulls"],
     "A null index entry gives a null, and an entry that selects a null value gives a null.",
     [source("idx_nulls.in.arrow")], source("ints.values.arrow"),
     write("idx_nulls.expected.arrow", [taken("v", IDX_NULLS, INTS)]))

case("sliced_index", ["sliced", "nulls"],
     "An index whose array starts at row 1 of its buffers is read from its own first row.",
     [source("idx_nulls.in.arrow", (1, 3))], source("ints.values.arrow"),
     write("sliced_index.expected.arrow", [taken("v", IDX_NULLS.slice(1, 3), INTS)]))

case("sliced_values", ["sliced"],
     "Values whose array starts at row 3 of their buffers are numbered from their own first row.",
     [source("idx_small.in.arrow")], source("ints.values.arrow", (3, 4)),
     write("sliced_values.expected.arrow",
           [taken("v", pa.array([3, 0, 1], pa.int64()), INTS.slice(3, 4))]))
write("idx_small.in.arrow", [column("i", pa.array([3, 0, 1], pa.int64()))])

case("empty_index", ["empty"], "An index of zero rows gives zero rows.",
     [source("idx.in.arrow", (0, 0))], source("ints.values.arrow"),
     write("empty.expected.arrow", [taken("v", IDX.slice(0, 0), INTS)]))

case("no_batches", ["empty"], "No index batches give no output.",
     [source("none.in.arrow")], source("ints.values.arrow"),
     write("none.expected.arrow", [], pa.schema([pa.field("v", pa.int64())])))
write("none.in.arrow", [], pa.schema([pa.field("i", pa.int32())]))

NO_VALUES = pa.array([], pa.string())
write("no_values.values.arrow", [column("s", NO_VALUES)])
ALL_NULL = pa.array([None, None, None], pa.int64())
write("all_null.in.arrow", [column("i", ALL_NULL)])
case("nulls_over_no_values", ["nulls", "empty"],
     "An index of nulls alone over no values gives nulls: a null entry is never out of range.",
     [source("all_null.in.arrow")], source("no_values.values.arrow"),
     write("all_null.expected.arrow", [column("s", pa.array([None] * 3, pa.string()))]))

# Every integer type is an index.
for ty in [pa.int8(), pa.int16(), pa.int64(), pa.uint8(), pa.uint16(), pa.uint64()]:
    idx = pa.array([7, 0, None, 4], ty)
    name = f"index_{ty}"
    write(f"{name}.in.arrow", [column("i", idx)])
    case(name, ["params", "nulls"], f"An index of type {ty} gathers as any other.",
         [source(f"{name}.in.arrow")], source("ints.values.arrow"),
         write(f"{name}.expected.arrow", [taken("v", idx, INTS)]))

# Dictionary values: the output is a dictionary of the same type.
DICT = pa.array(["n", "s", None, "e", "s", "w"]).dictionary_encode()
write("dict.values.arrow", [column("zone", DICT)])
DIDX = pa.array([5, 2, 1, 1, None, 0], pa.int32())
write("didx.in.arrow", [column("i", DIDX)])
case("dictionary_values", ["dictionary", "nulls"],
     "Dictionary values give a dictionary column of the same type with the selected values.",
     [source("didx.in.arrow")], source("dict.values.arrow"),
     write("dict.expected.arrow", [taken("zone", DIDX, DICT)]))
case("dictionary_values_sliced", ["dictionary", "sliced"],
     "Sliced dictionary values are numbered from their own first row.",
     [source("idx_small.in.arrow")], source("dict.values.arrow", (2, 4)),
     write("dict_sliced.expected.arrow",
           [taken("zone", pa.array([3, 0, 1], pa.int64()), DICT.slice(2, 4))]))

# Nested values: lists and structs.
LISTS = pa.array([[1, 2], [], None, [None, 4], [5, 6, 7]], pa.list_(pa.int64()))
write("lists.values.arrow", [column("tags", LISTS)])
LIDX = pa.array([4, 2, 1, 3, 0, 4], pa.int32())
write("lidx.in.arrow", [column("i", LIDX)])
case("list_values", ["nested", "nulls"],
     "List values are gathered whole: empty lists, null lists and null items as given.",
     [source("lidx.in.arrow")], source("lists.values.arrow"),
     write("lists.expected.arrow", [taken("tags", LIDX, LISTS)]))
case("list_values_sliced", ["nested", "sliced"],
     "Sliced list values are numbered from their own first row, offsets included.",
     [source("idx_small.in.arrow", (1, 2))], source("lists.values.arrow", (1, 4)),
     write("lists_sliced.expected.arrow",
           [taken("tags", pa.array([0, 1], pa.int64()), LISTS.slice(1, 4))]))

STRUCTS = pa.array([{"x": 1.5, "y": "a"}, None, {"x": None, "y": "c"}, {"x": -0.0, "y": None}],
                   pa.struct([("x", pa.float64()), ("y", pa.string())]))
write("structs.values.arrow", [column("point", STRUCTS)])
SIDX = pa.array([3, 1, 2, 0, 0], pa.uint8())
write("sidx.in.arrow", [column("i", SIDX)])
case("struct_values", ["nested", "nulls"],
     "Struct values are gathered whole, null structs and null fields as given.",
     [source("sidx.in.arrow")], source("structs.values.arrow"),
     write("structs.expected.arrow", [taken("point", SIDX, STRUCTS)]))

# Not-null fields give a not-null output.
NN_VALUES = column("v", pa.array([1.0, 2.0, 3.0], pa.float32()), nullable=False)
NN_INDEX = column("i", pa.array([2, 2, 0], pa.uint32()), nullable=False)
write("nn.values.arrow", [NN_VALUES])
write("nn.in.arrow", [NN_INDEX])
case("not_nullable", ["params"],
     "When neither the values nor the index is nullable, the output is not nullable.",
     [source("nn.in.arrow")], source("nn.values.arrow"),
     write("nn.expected.arrow", [taken("v", NN_INDEX["i"], NN_VALUES["v"], False, False)]))

# Values on a second input, joined by morsel.
V1 = pa.array(["a", "b", "c"])
V2 = pa.array(["d", None, "f", "g", "h"])
I1 = pa.array([2, 2, 0], pa.int64())
I2 = pa.array([4, None, 1, 0], pa.int64())
write("pair.index.arrow", [column("i", I1), column("i", I2)])
write("pair.values.arrow", [column("s", V1), column("s", V2)])
case("values_by_morsel", ["multi_batch", "nulls"],
     "With no values fixed, each index batch is taken from the values batch of its morsel.",
     [source("pair.index.arrow"), source("pair.values.arrow")], None,
     write("pair.expected.arrow", [taken("s", I1, V1), taken("s", I2, V2)]))
case("values_by_morsel_sliced", ["sliced"],
     "Both inputs sliced: each is numbered from its own first row.",
     [source("pair.index.arrow", (2, 1)), source("pair.values.arrow", (1, 2))], None,
     write("pair_sliced.expected.arrow",
           [taken("s", I1.slice(2, 1), V1.slice(1, 2)), taken("s", I2.slice(2, 1), V2.slice(1, 2))]))

# Refusals.
case("out_of_range", ["error"], "An index past the values refuses the batch.",
     [source("oob.in.arrow")], source("ints.values.arrow"),
     error={"at": "apply", "contains": "out of range"})
write("oob.in.arrow", [column("i", pa.array([0, 8], pa.int64()))])
case("negative_index", ["error"], "A negative index refuses the batch.",
     [source("neg.in.arrow")], source("ints.values.arrow"),
     error={"at": "apply", "contains": "out of range"})
write("neg.in.arrow", [column("i", pa.array([0, -1], pa.int32()))])
case("out_of_range_by_morsel", ["error"],
     "An index past its morsel's values refuses the batch.",
     [source("pair.index.arrow"), source("pair.values.arrow", (0, 2))], None,
     error={"at": "apply", "contains": "out of range"})
case("index_not_integer", ["error"], "An index that is not an integer is refused at plan.",
     [source("float_idx.in.arrow")], source("ints.values.arrow"), error={"at": "schema"})
write("float_idx.in.arrow", [column("i", pa.array([0.0, 1.0]))])
case("index_of_two_columns", ["error"], "An index batch of two columns is refused at plan.",
     [source("two_cols.in.arrow")], source("ints.values.arrow"), error={"at": "schema"})
write("two_cols.in.arrow", [pa.record_batch({"i": pa.array([0], pa.int32()),
                                             "j": pa.array([1], pa.int32())})])
case("values_of_two_columns", ["params", "error"],
     "Values fixed at configure that are not one column are refused.",
     [source("idx.in.arrow")], source("two_cols.in.arrow"), error={"at": "configure"})
case("json_parameter", ["params", "error"], "The kernel takes no JSON parameter.",
     [source("idx.in.arrow")], source("ints.values.arrow"), error={"at": "configure"},
     params={"check_bounds": False})
case("one_input_without_values", ["error"],
     "With no values fixed, an index alone is refused at plan.",
     [source("idx.in.arrow")], None, error={"at": "schema"})

index = {"format": "moruna-kernel-corpus/1", "kernel": "moruna.take", "cases": CASES}
with open(os.path.join(HERE, "index.json"), "w") as f:
    json.dump(index, f, indent=2)
    f.write("\n")
