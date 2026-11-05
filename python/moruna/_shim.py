"""The shim of an image Moruna builds (SDD 30 f.3): its entry point, layer "shim" of every image.

This file is copied into every image as ``/opt/moruna/shim.py`` and is the image's entrypoint,
``python3 /opt/moruna/shim.py --workdir /app``. It speaks the contract every image task speaks
(see ``docs/images.md``), whatever runs it, and nothing else:

- ``MORUNA_ENTRY`` names the code, ``<module>:<attribute>`` (the image sets it; the runner gives
  the task's); ``MORUNA_ROLE`` is ``source``, ``function`` or ``sink``; ``MORUNA_PARAMS`` is the
  task's params and ``MORUNA_ARGS`` the run's arguments, each a JSON object; ``MORUNA_PORTS``
  lists the task's ports and ``MORUNA_PORT_SOCKET`` names the socket they are called on.
- A function reads one Arrow IPC stream on standard input and writes one on standard output,
  one record batch out for each record batch in, in order, and, when its input ends, the batch
  its ``finish`` gives, if any; then the end-of-stream marker, and exit 0. An input with no byte
  at all is no batch.
- A source reads nothing and writes one Arrow IPC stream: every row of every split its ``plan``
  gives, read by position.
- A sink reads one Arrow IPC stream, writes each batch, and at its end writes on standard output
  the commit its ``finish`` returns, one JSON object, or nothing.

Any failure is a message on standard error and exit status 1, with no end-of-stream marker, so
the runner sees the process end and says why. The shim imports only the standard library and
``pyarrow``; ``import moruna`` inside an image finds the stand-in (``_standin.py``). The code is
found by what the engine's tasks and the stand-in's both have: a class is constructed with the
params; a decorated function has a ``wrapped`` callable and a ``stateful`` flag.
"""

from __future__ import annotations

import importlib
import inspect
import json
import os
import socket
import struct
import sys
import threading
import traceback
import types
from collections.abc import Callable, Iterator, Mapping
from typing import Any, BinaryIO

import pyarrow as pa

EXIT_DONE = 0
EXIT_FAILED = 1
EXIT_USAGE = 2

ROLES = ("source", "function", "sink")

#: The rows a source is read in at once, at most.
READ_ROWS = 65_536


class ShimError(Exception):
    """The task or its environment does not do what the contract needs; the message says how."""


class PortError(Exception):
    """A port refused a call, in its words."""


# ---------------------------------------------------------------------------------------------
# Ports (the port protocol, version 1; `moruna_sandbox::ports`)

_MAGIC = b"MRP1"


class _Socket:
    """The one connection every port of the task is called on, one call at a time."""

    def __init__(self, address: str) -> None:
        self.address = address
        self.lock = threading.Lock()
        self.sock: socket.socket | None = None

    def _connect(self) -> socket.socket:
        if self.sock is None:
            kind, _, where = self.address.partition(":")
            if kind == "fd" and where.isdigit():
                self.sock = socket.socket(fileno=int(where))
            elif kind == "unix" and where:
                self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                self.sock.connect(where)
            else:
                raise ShimError(
                    f"MORUNA_PORT_SOCKET is {self.address!r}; it is fd:<descriptor> or unix:<path>"
                )
        return self.sock

    def _read(self, sock: socket.socket, n: int) -> bytes:
        out = bytearray()
        while len(out) < n:
            chunk = sock.recv(n - len(out))
            if not chunk:
                raise PortError("the runner closed the port socket")
            out += chunk
        return bytes(out)

    def call(self, name: str, request: bytes) -> bytes:
        encoded = name.encode()
        with self.lock:
            sock = self._connect()
            sock.sendall(
                _MAGIC + struct.pack("<H", len(encoded)) + encoded
                + struct.pack("<Q", len(request)) + request
            )  # fmt: skip
            status, length = struct.unpack("<BQ", self._read(sock, 9))
            body = self._read(sock, length)
        if status != 0:
            raise PortError(f"port {name!r}: {body.decode(errors='replace')}")
        return body


class Port:
    """A client of one of the task's ports: a named request and response the runner serves."""

    def __init__(self, name: str, connection: _Socket) -> None:
        self.name = name
        self._connection = connection

    def call(self, request: bytes = b"") -> bytes:
        """``request`` to the port, and its answer; raises ``PortError`` when it refuses."""
        return self._connection.call(self.name, bytes(request))

    def batches(self, request: bytes = b"") -> list[pa.RecordBatch]:
        """The answer of a port that answers with an Arrow IPC stream, as its batches."""
        return list(pa.ipc.open_stream(self.call(request)))

    def table(self, request: bytes = b"") -> pa.Table:
        """The answer of a port that answers with an Arrow IPC stream, as one table."""
        return pa.ipc.open_stream(self.call(request)).read_all()

    def __repr__(self) -> str:
        return f"moruna port {self.name!r}"


def ports_from(env: Mapping[str, str]) -> dict[str, Port]:
    """The task's ports, from ``MORUNA_PORTS`` and ``MORUNA_PORT_SOCKET``."""
    listed = json.loads(env.get("MORUNA_PORTS") or "[]")
    if not isinstance(listed, list) or not all(isinstance(n, str) for n in listed):
        raise ShimError("MORUNA_PORTS is a JSON array of port names")
    if not listed:
        return {}
    address = env.get("MORUNA_PORT_SOCKET")
    if not address:
        raise ShimError(
            f"the task has ports ({', '.join(listed)}) and MORUNA_PORT_SOCKET names no socket"
        )
    connection = _Socket(address)
    return {name: Port(name, connection) for name in listed}


# ---------------------------------------------------------------------------------------------
# The task


def _object(env: Mapping[str, str], key: str) -> dict[str, Any]:
    text = env.get(key) or "{}"
    try:
        value = json.loads(text)
    except ValueError as exc:
        raise ShimError(f"{key} is not JSON: {exc}") from exc
    if not isinstance(value, dict):
        raise ShimError(f"{key} is a JSON object, not {type(value).__name__}")
    return value


def _target(entry: str) -> Any:
    module_name, _, name = entry.partition(":")
    if not module_name or not name:
        raise ShimError(f"entry {entry!r} is not <module>:<name>")
    try:
        module = importlib.import_module(module_name)
    except ImportError as exc:
        raise ShimError(f"entry {entry!r}: the module does not import: {exc}") from exc
    target: Any = module
    for part in name.split("."):
        try:
            target = getattr(target, part)
        except AttributeError as exc:
            raise ShimError(f"entry {entry!r}: {module_name} has no {name}") from exc
    return target


def make(entry: str, params: dict[str, Any], args: dict[str, Any], ports: dict[str, Port]) -> Any:
    """The task ``entry`` names: a class constructed with ``params``, given its ports and the
    run's arguments; or a decorated (or plain) function, which takes no params."""
    target = _target(entry)
    if inspect.isclass(target):
        try:
            task = target(**params)
        except Exception as exc:  # noqa: BLE001, a constructor's refusal is the task's
            raise ShimError(
                f"entry {entry!r} refused its params {sorted(params)}: {type(exc).__name__}: {exc}"
            ) from exc
        task.__dict__["_moruna_ports"] = types.MappingProxyType(dict(ports))
        task.__dict__["_moruna_run_args"] = types.MappingProxyType(dict(args))
        return task
    if params:
        raise ShimError(f"entry {entry!r} is a function, and takes no params ({sorted(params)})")
    if not callable(getattr(target, "wrapped", target)):
        raise ShimError(f"entry {entry!r} is a {type(target).__name__}, which is not callable")
    return target


def _checked(task: Any, which: str, batch: Any) -> Any:
    declared = getattr(task, which, None)
    if declared is None or batch is None:
        return batch
    if not batch.schema.equals(declared, check_metadata=False):
        raise ShimError(
            f"{type(task).__name__} {which} {declared.to_string(show_schema_metadata=False)!r}, "
            f"and a batch has {batch.schema.to_string(show_schema_metadata=False)!r}"
        )
    return batch


def function_calls(task: Any, entry: str) -> tuple[Callable[[pa.RecordBatch], Any], Callable[[], Any]]:
    """``(call(batch), finish())`` of a function: an instance of a class is called with each
    batch and its ``finish()`` at the end; a decorated function is its ``wrapped`` callable, and
    a stateful one has its ``setup`` called once (instance 0, no device) and is called with the
    state it returned, its ``finish(state)`` at the end."""
    if not inspect.isroutine(task) and not hasattr(task, "wrapped"):
        finish = getattr(task, "finish", None)

        def call(batch: pa.RecordBatch) -> Any:
            return _checked(task, "gives", task(_checked(task, "takes", batch)))

        return call, (lambda: _checked(task, "gives", finish())) if finish else (lambda: None)
    wrapped = getattr(task, "wrapped", task)
    if not getattr(task, "stateful", False):
        return wrapped, lambda: None
    if not hasattr(wrapped, "setup"):
        raise ShimError(f"entry {entry!r} is stateful and has no setup(ctx)")
    state = wrapped.setup(types.SimpleNamespace(instance=0, device=None))
    finish = getattr(wrapped, "finish", None)
    return (lambda batch: wrapped(state, batch)), (lambda: finish(state) if finish else None)


def resolve(entry: str) -> Callable[[pa.RecordBatch], Any]:
    """The function ``entry`` names, as ``call(batch)``: with no params, no ports."""
    return function_calls(make(entry, {}, {}, {}), entry)[0]


def as_batch(result: Any, index: int | str) -> pa.RecordBatch:
    """What a function returned for input ``index`` as one record batch: a batch as it is, a
    table as one batch."""
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


def serve(
    call: Callable[[pa.RecordBatch], Any],
    source: BinaryIO,
    sink: BinaryIO,
    finish: Callable[[], Any] = lambda: None,
) -> int:
    """Answer every batch of the stream ``source`` holds on ``sink``, then the batch ``finish``
    gives, if any; the number of batches written.

    ``source`` is a buffered binary reader (it is peeked, so that an input with no byte at all
    is told from one with a schema) and ``sink`` is flushed after each batch, because the runner
    writes the next batch only when it has read this answer. The end-of-stream marker is
    written only when every batch was answered; an exception leaves the stream where it was.
    """
    writer = None
    answered = 0

    def put(out: pa.RecordBatch) -> None:
        nonlocal writer
        if writer is None:
            writer = pa.ipc.new_stream(sink, out.schema)
        writer.write_batch(out)
        sink.flush()

    if source.peek(1):
        for batch in pa.ipc.open_stream(source):
            put(as_batch(call(batch), answered))
            answered += 1
    last = finish()
    if last is not None:
        put(as_batch(last, "finish"))
        answered += 1
    if writer is not None:
        writer.close()
        sink.flush()
    return answered


def read_source(task: Any) -> Iterator[pa.RecordBatch]:
    """Every row of a source, split by split as its ``plan`` gives them, read by position in
    pieces of at most ``READ_ROWS`` rows."""
    for split in task.plan():
        rows = int(split.rows)
        start = 0
        while start < rows:
            end = min(rows, start + READ_ROWS)
            yield _checked(task, "gives", as_batch(task.read(split.id, start, end), split.id))
            start = end


def run_source(task: Any, sink: BinaryIO) -> int:
    writer = None
    written = 0
    for batch in read_source(task):
        if writer is None:
            writer = pa.ipc.new_stream(sink, batch.schema)
        writer.write_batch(batch)
        written += 1
    if writer is None:
        gives = getattr(task, "gives", None)
        if gives is None:
            raise ShimError("the source gave no batch and declares no schema (gives) to end with")
        writer = pa.ipc.new_stream(sink, gives)
    writer.close()
    sink.flush()
    return written


def run_sink(task: Any, source: BinaryIO, sink: BinaryIO) -> int:
    written = 0
    if source.peek(1):
        for batch in pa.ipc.open_stream(source):
            task.write(_checked(task, "takes", batch))
            written += 1
    finish = getattr(task, "finish", None)
    commit = finish() if finish is not None else None
    if commit is not None:
        if not isinstance(commit, dict):
            raise ShimError(f"the sink's finish() returned {type(commit).__name__}, not a dict")
        sink.write(json.dumps(commit, sort_keys=True).encode() + b"\n")
        sink.flush()
    return written


def take_streams() -> tuple[BinaryIO, BinaryIO]:
    """The two descriptors the contract is on, for the shim alone.

    Standard input and output are duplicated and descriptors 0 and 1 are pointed elsewhere
    (``/dev/null`` and standard error), so a ``print`` in the task, or in a library it calls,
    can neither read the stream nor corrupt it.
    """
    source = os.fdopen(os.dup(0), "rb")
    sink = os.fdopen(os.dup(1), "wb")
    null = os.open(os.devnull, os.O_RDONLY)
    os.dup2(null, 0)
    os.close(null)
    os.dup2(2, 1)
    sys.stdout = sys.stderr
    return source, sink


def run(env: Mapping[str, str], source: BinaryIO, sink: BinaryIO) -> int:
    """The task the environment names, run over ``source`` and ``sink``."""
    entry = env.get("MORUNA_ENTRY")
    if not entry:
        raise ShimError("MORUNA_ENTRY names no entry (<module>:<name>)")
    role = env.get("MORUNA_ROLE") or "function"
    if role not in ROLES:
        raise ShimError(f"MORUNA_ROLE is {role!r}; a task is a source, a function or a sink")
    task = make(entry, _object(env, "MORUNA_PARAMS"), _object(env, "MORUNA_ARGS"), ports_from(env))
    if role == "source":
        return run_source(task, sink)
    if role == "sink":
        return run_sink(task, source, sink)
    call, finish = function_calls(task, entry)
    return serve(call, source, sink, finish)


def main(argv: list[str]) -> int:
    """``shim.py [--workdir ABSOLUTE_PATH]``: the process exit status."""
    workdir = None
    if argv and argv[0] == "--workdir":
        if len(argv) != 2 or not os.path.isabs(argv[1]) or "\x00" in argv[1]:
            print("usage: shim.py [--workdir ABSOLUTE_PATH]", file=sys.stderr)
            return EXIT_USAGE
        workdir = argv[1]
        argv = argv[2:]
    if argv:
        print("usage: shim.py [--workdir ABSOLUTE_PATH]", file=sys.stderr)
        return EXIT_USAGE
    source, sink = take_streams()
    try:
        if workdir is not None:
            os.chdir(workdir)
        run(os.environ, source, sink)
    except (ShimError, PortError) as exc:
        print(f"moruna-shim: {exc}", file=sys.stderr)
        return EXIT_FAILED
    except Exception:  # noqa: BLE001, whatever the task raised ends the run with its traceback
        role = os.environ.get("MORUNA_ROLE") or "function"
        print(f"moruna-shim: the {role} failed:", file=sys.stderr)
        traceback.print_exc()
        return EXIT_FAILED
    return EXIT_DONE


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
