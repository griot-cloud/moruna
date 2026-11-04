"""The shim of an image built from Python code (SDD 30 f.3): layer 3 of ``moruna build``.

This file is copied into every such image as ``/opt/moruna/shim.py`` and run as the image's
command, ``python3 /opt/moruna/shim.py --workdir /app <module>:<name>``. The explicit
workdir is applied before importing the function; plain-entry invocations retain their caller's
working directory. It speaks the contract of SDD 19 f.6 and
nothing else: one Arrow IPC stream on standard input, one on standard output, one record batch
out for each record batch in, in order; when the input ends it writes the end-of-stream marker
and exits 0. A run that applied no batch ends the input without a byte, and the shim then writes
nothing and exits 0. Any failure is a message on standard error and exit status 1, with no
end-of-stream marker, so the engine sees the process end and says why.

It imports only the standard library and ``pyarrow``, and it never imports ``moruna``: inside an
image ``import moruna`` finds ``/opt/moruna/moruna/__init__.py`` (``_standin.py``), which holds
the names a function's module uses at import time. A function is found by what a
``moruna.Function`` and the stand-in's both have, a ``wrapped`` callable and a ``stateful``
flag, so this file runs unchanged against either.
"""

from __future__ import annotations

import importlib
import os
import sys
import traceback
import types
from collections.abc import Callable
from typing import Any, BinaryIO

import pyarrow as pa

EXIT_DONE = 0
EXIT_FAILED = 1
EXIT_USAGE = 2


class ShimError(Exception):
    """The function or its entry does not do what the contract needs; the message says how."""


def resolve(entry: str) -> Callable[[pa.RecordBatch], Any]:
    """The callable a ``<module>:<name>`` entry names, as ``call(batch)``.

    A decorated function is its ``wrapped`` callable; a stateful one has its ``setup`` called once
    (the instance is 0 and there is no device) and is called with the state it returned.
    """
    module_name, _, name = entry.partition(":")
    if not module_name or not name:
        raise ShimError(f"entry {entry!r} is not <module>:<name>")
    try:
        module = importlib.import_module(module_name)
    except ImportError as exc:
        raise ShimError(f"entry {entry!r}: the module does not import: {exc}") from exc
    try:
        target = getattr(module, name)
    except AttributeError as exc:
        raise ShimError(f"entry {entry!r}: {module_name} has no {name}") from exc
    wrapped = getattr(target, "wrapped", target)
    if not callable(wrapped):
        raise ShimError(f"entry {entry!r} is a {type(wrapped).__name__}, which is not callable")
    if not getattr(target, "stateful", False):
        return wrapped
    if not hasattr(wrapped, "setup"):
        raise ShimError(f"entry {entry!r} is stateful and has no setup(ctx)")
    state = wrapped.setup(types.SimpleNamespace(instance=0, device=None))

    def call(batch: pa.RecordBatch) -> Any:
        return wrapped(state, batch)

    return call


def as_batch(result: Any, index: int) -> pa.RecordBatch:
    """What the function returned for input ``index`` as the one record batch the contract
    answers with: a batch as it is, a table as one batch."""
    if isinstance(result, pa.RecordBatch):
        return result
    if isinstance(result, pa.Table):
        batches = result.combine_chunks().to_batches()
        if len(batches) == 1:
            return batches[0]
        if not batches:
            arrays = [pa.array([], type=field.type) for field in result.schema]
            return pa.RecordBatch.from_arrays(arrays, schema=result.schema)
    raise ShimError(
        f"the function returned {type(result).__name__} for batch {index}; "
        "it returns a pyarrow.RecordBatch or Table"
    )


def serve(call: Callable[[pa.RecordBatch], Any], source: BinaryIO, sink: BinaryIO) -> int:
    """Answer every batch of the stream ``source`` holds on ``sink``; the number answered.

    ``source`` is a buffered binary reader (it is peeked, so that an input with no byte at all
    is told from one with a schema) and ``sink`` is flushed after each batch, because the engine
    writes the next batch only when it has read this answer. The end-of-stream marker is written
    only when every batch was answered; an exception leaves the stream where it was.
    """
    if not source.peek(1):
        return 0
    reader = pa.ipc.open_stream(source)
    writer = None
    answered = 0
    for batch in reader:
        out = as_batch(call(batch), answered)
        if writer is None:
            writer = pa.ipc.new_stream(sink, out.schema)
        writer.write_batch(out)
        sink.flush()
        answered += 1
    if writer is not None:
        writer.close()
        sink.flush()
    return answered


def take_streams() -> tuple[BinaryIO, BinaryIO]:
    """The two descriptors the contract is on, for the shim alone.

    Standard input and output are duplicated and descriptors 0 and 1 are pointed elsewhere
    (``/dev/null`` and standard error), so a ``print`` in the function, or in a library it
    calls, can neither read the stream nor corrupt it.
    """
    source = os.fdopen(os.dup(0), "rb")
    sink = os.fdopen(os.dup(1), "wb")
    null = os.open(os.devnull, os.O_RDONLY)
    os.dup2(null, 0)
    os.close(null)
    os.dup2(2, 1)
    sys.stdout = sys.stderr
    return source, sink


def main(argv: list[str]) -> int:
    """``shim.py [--workdir ABSOLUTE_PATH] <module>:<name>``: the process exit status."""
    workdir = None
    if argv and argv[0] == "--workdir":
        if len(argv) != 3 or not os.path.isabs(argv[1]) or "\x00" in argv[1]:
            print("usage: shim.py [--workdir ABSOLUTE_PATH] <module>:<name>", file=sys.stderr)
            return EXIT_USAGE
        workdir = argv[1]
        argv = argv[2:]
    if len(argv) != 1:
        print("usage: shim.py [--workdir ABSOLUTE_PATH] <module>:<name>", file=sys.stderr)
        return EXIT_USAGE
    source, sink = take_streams()
    try:
        if workdir is not None:
            os.chdir(workdir)
        serve(resolve(argv[0]), source, sink)
    except ShimError as exc:
        print(f"moruna-shim: {exc}", file=sys.stderr)
        return EXIT_FAILED
    except Exception:  # noqa: BLE001, whatever the function raised ends the run with its traceback
        print("moruna-shim: the function failed:", file=sys.stderr)
        traceback.print_exc()
        return EXIT_FAILED
    return EXIT_DONE


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
