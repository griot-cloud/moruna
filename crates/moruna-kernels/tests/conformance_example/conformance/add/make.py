"""Writes the Arrow IPC files of example.add's corpus (index.json names them), by hand.

    python3 -I crates/moruna-kernels/tests/conformance_example/conformance/add/make.py

Every expected output is written out here, not computed by an implementation: the corpus
defines the right answer.
"""
import os

import pyarrow as pa

HERE = os.path.dirname(os.path.abspath(__file__))
SCHEMA = pa.schema([pa.field("x", pa.int64(), nullable=True)])


def write(name, *columns):
    """One file, one batch per list of values of x."""
    with pa.OSFile(os.path.join(HERE, name), "wb") as sink:
        with pa.ipc.new_file(sink, SCHEMA) as writer:
            for values in columns:
                writer.write_batch(pa.record_batch([pa.array(values, pa.int64())], schema=SCHEMA))


write("adds.in.arrow", [1, 2, 3], [-4])
write("adds.expected.arrow", [11, 12, 13, 6])
write("nulls.in.arrow", [5, None, 7, 8, None, 10, 11, 12])
write("nulls.expected.arrow", [4, None, 6, 7, None, 9, 10, 11])
write("sliced.expected.arrow", [108, None, 110, 111])
write("empty.in.arrow", [])
write("empty.expected.arrow", [])
write("max.in.arrow", [0, 2**63 - 1])
