"""What ``import moruna`` is inside an image built from Python code (SDD 30 f.3).

``moruna build`` copies this file into layer 3 as ``/opt/moruna/moruna/__init__.py``, beside
``_declare.py``, so a function's module, which does ``import moruna`` and decorates a function
with ``@moruna.function`` (or subclasses ``moruna.Function``), imports in an image that has no
Moruna engine. It holds the names such a module uses at import time, ``function``, ``polars``,
``Task`` and ``Function``, and they decorate the way the engine's do:
a function annotated Polars frame to Polars frame is wrapped the same way (``_declare``), and the
result carries the ``wrapped`` callable and ``stateful`` flag that the shim reads. The declarations
(schemas, memory, tier and the rest) are how the engine plans and checks a function, and an image is
planned by its job document, so they are accepted and not read; ``moruna build`` imports the
module with the real decorator and refuses a declaration that is wrong. Every other name of
``moruna`` is refused when it is used, saying so.
"""

from __future__ import annotations

from typing import Any

from moruna import _declare


class Task:
    """A task's base, as a module subclasses it: its declarations are accepted and not read."""

    takes: Any = None
    gives: Any = None
    needs: Any = ()


class Function(Task):
    """A function's base: an instance is called with each batch, as the shim calls it."""

    stateful = False

    @property
    def wrapped(self) -> Any:
        return self


class _Decorated(Function):
    """A decorated function as the shim reads it: what it wraps and whether it is stateful."""

    def __init__(self, wrapped: Any, stateful: bool) -> None:
        self._wrapped = wrapped
        self.stateful = stateful

    @property
    def wrapped(self) -> Any:
        return self._wrapped

    def __call__(self, *args: Any, **kwargs: Any) -> Any:
        return self._wrapped(*args, **kwargs)


def function(fn: Any = None, *, stateful: bool = False, **declarations: Any) -> Any:
    """``@moruna.function`` and ``@moruna.function(stateful=True, ...)``."""

    def decorate(obj: Any) -> _Decorated:
        frame = None if stateful else _declare.polars_signature(obj)
        if frame is not None:
            obj = _declare.polars_callable(obj, frame)
        return _Decorated(obj, stateful)

    if fn is None:
        return decorate
    return decorate(fn)


def polars(fn: Any, **declarations: Any) -> _Decorated:
    """A function from a Polars frame to a Polars frame as a stateless function."""
    kind = _declare.polars_signature(fn) or "eager"
    return _Decorated(_declare.polars_callable(fn, kind), False)


def __getattr__(name: str) -> Any:
    raise AttributeError(
        f"moruna.{name}: inside an image built by `moruna build` the moruna module holds "
        "only function, polars, Task and Function; the engine is not in the image"
    )
