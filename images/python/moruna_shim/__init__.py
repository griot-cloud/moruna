"""Moruna's ring shim for a Python image task (architecture/data-os.md section 5).

`serve(fn)` runs a batch function as a stage's task: each batch the engine posts is read in
place from the task's input region as a `pyarrow.RecordBatch` (`pyarrow.foreign_buffer` over
the mapping), and the batch `fn` returns is written as a record into a window and posted.
One interpreter per process; a stage runs as many processes as it asks for. An image whose
layer sets `MORUNA_PLANE=rings` is served this way; any other is served on its descriptors
0 and 1.
"""

import sys

from . import record
from .plane import Plane

__all__ = ["serve"]


def serve(fn, plane=None):
    """Runs `fn` on every batch until the engine ends the input; returns how many batches."""
    plane = plane or Plane()
    n = 0
    while True:
        got = plane.next()
        if got is None:
            break
        entry, batch = got
        _, _, cookie, _ = entry
        out = fn(batch)
        del batch
        w, window = plane.window()
        length = record.write(out, window)
        plane.post(w, length, cookie)
        plane.release()
        n += 1
    plane.finish()
    copied = plane.calls.copied()
    print(
        f"moruna_shim: {n} batches, {plane.calls.made} native calls, "
        f"{plane.waits} waits, {plane.wakes} wakes, {plane.handed} hand-backs, "
        f"kernel copied {copied}",
        file=sys.stderr,
    )
    return n
