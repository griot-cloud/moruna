"""What ``import moruna`` is inside an image Moruna builds (SDD 30 f.3).

The build copies this file into the shim's layer as ``/opt/moruna/moruna/__init__.py``, beside
``_declare.py``, so a task's module, which does ``import moruna`` and subclasses
``moruna.Source``, ``moruna.Function`` or ``moruna.Sink`` (or decorates a function with
``@moruna.function``), imports in an image that has no Moruna engine. It holds the names such a
module uses, and they behave as the engine's do for the shim: a task reads its ports as
``self.ports[name]`` (each a client of the port protocol) and the run's arguments as
``self.args``; a decorated function carries the ``wrapped`` callable and ``stateful`` flag the
shim reads; ``moruna.params()`` reads the task's params, as it does outside an image (the
package's is this one). The declarations (schemas, memory, tier and the rest) are how the engine
plans and checks a task; in an image the shim checks ``takes`` and ``gives`` on every batch and
reads nothing else. Every other name of ``moruna`` is refused when it is used, saying so.
"""

from __future__ import annotations

import json
import os
import types
from typing import Any


class Task:
    """A task's base: its declarations, its ports and the run's arguments."""

    takes: Any = None
    gives: Any = None
    needs: Any = ()
    files: Any = ()

    @property
    def ports(self) -> Any:
        """The task's ports, by need: each a client with ``call(bytes) -> bytes``."""
        return self.__dict__.get("_moruna_ports", types.MappingProxyType({}))

    @property
    def args(self) -> Any:
        """The run's arguments, by name."""
        return self.__dict__.get("_moruna_run_args", types.MappingProxyType({}))


class Source(Task):
    """A source's base: ``plan()`` gives its splits and ``read(split_id, start, end)`` a batch."""


class Function(Task):
    """A function's base: an instance is called with each batch, and ``finish()`` at the end."""


class Sink(Task):
    """A sink's base: ``write(batch)`` each batch, and ``finish()`` returns its commit."""


class Split:
    """One split of a source: its id and its rows."""

    def __init__(self, id: int, rows: int, bytes: int | None = None) -> None:  # noqa: A002
        self.id = int(id)
        self.rows = int(rows)
        self.bytes = bytes


class _Decorated:
    """A decorated function as the shim reads it: what it wraps and whether it is stateful."""

    def __init__(self, wrapped: Any, stateful: bool) -> None:
        self.wrapped = wrapped
        self.stateful = stateful

    def __call__(self, *args: Any, **kwargs: Any) -> Any:
        return self.wrapped(*args, **kwargs)


def function(fn: Any = None, *, stateful: bool = False, **declarations: Any) -> Any:
    """``@moruna.function`` and ``@moruna.function(stateful=True, ...)``."""

    def decorate(obj: Any) -> _Decorated:
        return _Decorated(obj, stateful)

    if fn is None:
        return decorate
    return decorate(fn)


def params() -> dict[str, Any]:
    """The task's params: ``MORUNA_PARAMS``, the JSON object its task was given (item 5.5).

    Every function's process is given it, native or image, so a function reads its params the
    same way wherever it runs. ``{}`` when it was given none, or when the process runs outside a
    job. A value that is not a JSON object raises ``ValueError`` naming the variable.
    """
    text = os.environ.get("MORUNA_PARAMS") or "{}"
    try:
        value = json.loads(text)
    except ValueError as exc:
        raise ValueError(f"MORUNA_PARAMS is not JSON: {exc}") from exc
    if not isinstance(value, dict):
        raise ValueError(f"MORUNA_PARAMS is a JSON object, not {type(value).__name__}")
    return value


def __getattr__(name: str) -> Any:
    raise AttributeError(
        f"moruna.{name}: inside an image Moruna builds the moruna module holds only Task, "
        "Source, Function, Sink, Split, function and params; the engine is not in the image"
    )
