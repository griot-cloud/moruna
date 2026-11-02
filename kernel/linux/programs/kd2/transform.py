"""KD2's Python task (SDD 30 k): an unmodified CPython 3.14 program, from the python:3.14-slim
image's layers, that knows no Moruna library. It reads an Arrow IPC stream on its descriptor 0
(the stage's input slot, through the Linux personality) and writes one on its descriptor 1 (its
output slot): the same schema, and for each record batch read, one written in answer, with
`y` replaced by `3 * x + y` and `x` as read. At the end of its input it ends its stream and exits
0, which is the image stage's contract (SDD 19 f.6, f.7).

    transform.py [--fail-after N]

With `--fail-after N` it raises once it has answered N batches, so the stage ends as a task that
failed: the run's terminal event carries its exit status and its traceback's last lines.
"""

import os
import struct
import sys
from array import array

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import arrowlite  # noqa: E402


def read_exact(n):
    parts = []
    while n:
        piece = os.read(0, n)
        if not piece:
            break
        parts.append(piece)
        n -= len(piece)
    return b"".join(parts)


def write_all(data):
    view = memoryview(data)
    while view:
        n = os.write(1, view)
        view = view[n:]


def main():
    fail_after = None
    if len(sys.argv) == 3 and sys.argv[1] == "--fail-after":
        fail_after = int(sys.argv[2])
    elif len(sys.argv) != 1:
        sys.exit(f"usage: {sys.argv[0]} [--fail-after N]")
    reader = arrowlite.Reader(read_exact)
    first = reader.next()
    if first is None:
        # No batch at all: the input ended before a schema, and so does the output.
        write_all(arrowlite.END)
        return
    meta, msg, _ = first
    if msg.scalar(1, "B") != arrowlite.SCHEMA:
        sys.exit("transform: the stream does not start with a schema")
    names = arrowlite.schema_fields(msg)
    if names != ["x", "y"]:
        sys.exit(f"transform: columns {names}, not x and y")
    write_all(arrowlite.frame(meta, b""))
    answered = 0
    while True:
        item = reader.next()
        if item is None:
            break
        meta, msg, body = item
        if msg.scalar(1, "B") != arrowlite.RECORD_BATCH:
            sys.exit(f"transform: a message of type {msg.scalar(1, 'B')} after the schema")
        if fail_after is not None and answered >= fail_after:
            raise RuntimeError(f"kd2 transform fails on purpose after {answered} batches")
        rows, layout = arrowlite.batch_layout(msg, names)
        (x_at, _), (y_at, _) = layout
        x = array("q")
        x.frombytes(body[x_at : x_at + 8 * rows])
        y = array("q")
        y.frombytes(body[y_at : y_at + 8 * rows])
        out = array("q", [3 * a + b for a, b in zip(x, y)])
        if sys.byteorder != "little":
            out.byteswap()
        changed = bytearray(body)
        changed[y_at : y_at + 8 * rows] = out.tobytes()
        write_all(arrowlite.frame(meta, bytes(changed)))
        answered += 1
    write_all(arrowlite.END)
    print(f"transform: answered {answered} batches", file=sys.stderr)


if __name__ == "__main__":
    main()
