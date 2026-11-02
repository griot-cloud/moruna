"""Kinds the program serves itself, and the listing of what ``moruna.run`` can run (SDD 18 e.3,
e.4, KD-I11).

``@moruna.kind`` declares a kind in the format of every other kind's declaration, served by the
decorated callable: Moruna runs every task of it by handing each batch to the callable, with the
task's settings, and takes what it returns as the task's output for that batch. Moruna runs none
of the callable's code itself; the batch crosses as an Arrow IPC stream each way, as it would to
an embedder's port anywhere else::

    @moruna.kind(name="shout", input_schema={"text": "string"},
                 output_schema={"adds": {"loud": "string"}})
    def shout(batch, call):
        return batch.append_column("loud", pyarrow.compute.utf8_upper(batch["text"]))

    moruna.run(source, shout(), sink)
    moruna.kinds(shout)   # the listing, with `shout` among the kernels
"""

from __future__ import annotations

import json
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import Any

from moruna import _core, _declare

#: The JSON Schema dialect a kind's settings schema is written in (SDD 18 e.2).
SCHEMA_DIALECT = "https://json-schema.org/draft/2020-12/schema"

#: The settings of a kind that takes none.
NO_SETTINGS: dict[str, Any] = {
    "$schema": SCHEMA_DIALECT,
    "type": "object",
    "additionalProperties": False,
    "properties": {},
    "examples": [{}],
}

_MAGIC = b"MSK1"


@dataclass(frozen=True)
class KindCall:
    """What a served kind's callable is told with each batch: the kind, the task's settings and
    the input port the batch came in on, numbered from 0 in the order the task lists them."""

    kind: str
    settings: dict[str, Any]
    input: int


def _spelling(value: Any, where: str) -> Any:
    """``input_schema=`` or ``output_schema=`` as a package's declaration spells it (SDD 16 e.2):
    a ``pyarrow.Schema`` exact, a mapping of column to type a subset, an output mapping of only
    ``adds``, ``drops`` and ``changes`` relative to the input."""
    declared = _declare.declaration(value, where)
    if declared is None:
        return None
    if declared[0] == "exact":
        return [[name, ty, nullable] for name, ty, nullable in declared[1]]
    if declared[0] == "subset":
        return {name: ty for name, ty, _ in declared[1]}
    _, adds, drops, changes = declared
    return {
        "adds": {name: ty for name, ty, _ in adds},
        "drops": list(drops),
        "changes": {name: ty for name, ty, _ in changes},
    }


def _fact(value: bool | str, where: str) -> str:
    if value is True:
        return "yes"
    if value is False:
        return "no"
    if value in ("yes", "no", "by_settings"):
        return str(value)
    raise ValueError(f"{where} is True, False or 'by_settings', not {value!r}")


class DeclaredKind:
    """A kind the program serves itself: its declaration, and the callable that answers each of
    its batches. Calling it with settings is a task of the kind, which ``moruna.run`` takes
    where it takes a kernel."""

    def __init__(self, declaration: dict[str, Any], serve: Callable[..., Any]) -> None:
        self._declaration = declaration
        self._serve = serve

    @property
    def name(self) -> str:
        """The kind's name, which a document's ``kind`` names it by."""
        return str(self._declaration["name"])

    @property
    def declaration(self) -> dict[str, Any]:
        """The declaration, as ``moruna kinds`` lists it."""
        return json.loads(json.dumps(self._declaration))

    @property
    def serve(self) -> Callable[..., Any]:
        """The callable that answers each batch."""
        return self._serve

    def __call__(self, **settings: Any) -> Any:
        """A task of this kind with ``settings``, checked against the kind's settings schema
        when the run is built."""
        return _core.declared_task(json.dumps(self._declaration), json.dumps(settings), self._serve)

    def __repr__(self) -> str:
        return f"<moruna.DeclaredKind {self.name!r}>"


def kind(
    fn: Callable[..., Any] | None = None,
    *,
    name: str,
    summary: str | None = None,
    settings: Mapping[str, Any] | None = None,
    input_schema: Any = None,
    output_schema: Any = None,
    deadline_ms: int | None = None,
    deterministic: bool | str = False,
    preserves_columns: bool | str = False,
) -> Any:
    """Declare a kind served by the decorated callable (SDD 18 e.4).

    The callable is ``serve(batch, call) -> batch``: ``batch`` a ``pyarrow.RecordBatch``,
    ``call`` a ``moruna.KindCall`` (the kind, the task's settings, the input port); it returns a
    ``pyarrow.RecordBatch``, a ``pyarrow.Table`` or a list of record batches of one schema. An
    exception it raises fails the task with its message.

    ``settings`` is the JSON Schema (2020-12) of a task's settings: a closed object with
    ``examples``; absent, the kind takes none. ``input_schema`` and ``output_schema`` are spelled
    as ``@moruna.kernel``'s are; a reply that is not what ``output_schema`` declares fails the
    task naming the column. ``deadline_ms`` bounds one call (absent: half an hour).

    Refused with ``moruna.ConfigError`` naming why: a name a kind of ``moruna.run`` has already,
    a schema that is not one, rules that are not rules.
    """

    def declare(serve: Callable[..., Any]) -> DeclaredKind:
        if not callable(serve):
            raise TypeError(f"@moruna.kind({name!r}) decorates a callable over pyarrow batches")
        port: dict[str, Any] = {"guest": name}
        if deadline_ms is not None:
            port["deadline_ms"] = deadline_ms
        served: dict[str, Any] = {"port": port}
        for key, value in (("input_schema", input_schema), ("output_schema", output_schema)):
            spelled = _spelling(value, key)
            if spelled is not None:
                served[key] = spelled
        doc = (getattr(serve, "__doc__", None) or "").strip().splitlines()
        declaration = {
            "name": name,
            "role": "kernel",
            "summary": summary or (doc[0] if doc else f"{name}, served by the program"),
            "settings": dict(settings) if settings is not None else NO_SETTINGS,
            "inputs": "batches",
            "outputs": "batches",
            "needs": {
                "network": "no",
                "egress": [],
                "gpu": "no",
                "model_port": "yes",
                "contracts": "no",
            },
            "deterministic": _fact(deterministic, "deterministic"),
            "preserves_columns": _fact(preserves_columns, "preserves_columns"),
            "isolation": "embedder",
            "served": served,
        }
        checked = json.loads(_core.declare_kind(json.dumps(declaration)))
        return DeclaredKind(checked, serve)

    if fn is None:
        return declare
    return declare(fn)


def kinds(*declared: DeclaredKind) -> dict[str, Any]:
    """Every kind ``moruna.run`` can run, with ``declared`` among them, as ``moruna kinds`` lists
    them (SDD 18 e.3): ``{"moruna_kinds", "moruna", "sources", "kernels", "sinks"}``."""
    for each in declared:
        if not isinstance(each, DeclaredKind):
            raise TypeError(f"moruna.kinds() takes @moruna.kind kinds, not {type(each).__name__}")
    listing: dict[str, Any] = json.loads(_core.kinds([json.dumps(k.declaration) for k in declared]))
    return listing


def _answer(serve: Callable[..., Any], request: bytes) -> bytes:
    """One call of a served kind's port: the request read, ``serve`` called, its answer written
    as an Arrow IPC stream. Called by the runtime on the call's own thread."""
    import pyarrow as pa  # noqa: PLC0415, `import moruna` imports pyarrow already

    view = memoryview(request)
    if bytes(view[:4]) != _MAGIC:
        raise ValueError("a served kind's request starts with MSK1")
    length = int.from_bytes(view[4:8], "little")
    header = json.loads(bytes(view[8 : 8 + length]))
    reader = pa.ipc.open_stream(pa.py_buffer(view[8 + length :]))
    batch = reader.read_next_batch()
    call = KindCall(kind=header["kind"], settings=header["settings"], input=header["input"])
    result = serve(batch, call)
    if isinstance(result, pa.RecordBatch):
        batches = [result]
    elif isinstance(result, pa.Table):
        batches = result.to_batches() or [pa.RecordBatch.from_pylist([], schema=result.schema)]
    elif isinstance(result, list) and result and all(isinstance(b, pa.RecordBatch) for b in result):
        batches = result
    else:
        raise TypeError(
            f"`{call.kind}` returns a pyarrow.RecordBatch, a pyarrow.Table or a list of record "
            f"batches, not {type(result).__name__}"
        )
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, batches[0].schema) as writer:
        for each in batches:
            writer.write_batch(each)
    out: bytes = sink.getvalue().to_pybytes()
    return out
