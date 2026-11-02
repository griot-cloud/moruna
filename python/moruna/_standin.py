"""What ``import moruna`` is inside an image built from Python code (SDD 30 f.3).

``moruna build`` copies this file into layer 3 as ``/opt/moruna/moruna/__init__.py``, beside
``_declare.py``, so a kernel's module, which does ``import moruna`` and decorates a function with
``@moruna.kernel``, imports in an image that has no Moruna engine. It holds the two names such a
module uses at import time, ``kernel`` and ``polars``, and they decorate the way the engine's do:
a function annotated Polars frame to Polars frame is wrapped the same way (``_declare``), and the
result carries the ``wrapped`` callable and ``stateful`` flag that the shim reads. The declarations
(schemas, memory, tier and the rest) are how the engine plans and checks a kernel, and an image is
planned by its job document, so they are accepted and not read; ``moruna build`` imports the
module with the real decorator and refuses a declaration that is wrong. Every other name of
``moruna`` is refused when it is used, saying so.
"""

from __future__ import annotations

from typing import Any

from moruna import _declare


class KernelSpec:
    """A decorated kernel as the shim reads it: what it wraps and whether it is stateful."""

    __slots__ = ("stateful", "wrapped")

    def __init__(self, wrapped: Any, stateful: bool) -> None:
        self.wrapped = wrapped
        self.stateful = stateful

    def __call__(self, *args: Any, **kwargs: Any) -> Any:
        return self.wrapped(*args, **kwargs)


def kernel(fn: Any = None, *, stateful: bool = False, **declarations: Any) -> Any:
    """``@moruna.kernel`` and ``@moruna.kernel(stateful=True, ...)``."""

    def decorate(obj: Any) -> KernelSpec:
        frame = None if stateful else _declare.polars_signature(obj)
        if frame is not None:
            obj = _declare.polars_callable(obj, frame)
        return KernelSpec(obj, stateful)

    if fn is None:
        return decorate
    return decorate(fn)


def polars(fn: Any, **declarations: Any) -> KernelSpec:
    """A function from a Polars frame to a Polars frame as a stateless kernel."""
    kind = _declare.polars_signature(fn) or "eager"
    return KernelSpec(_declare.polars_callable(fn, kind), False)


def __getattr__(name: str) -> Any:
    raise AttributeError(
        f"moruna.{name}: inside an image built by `moruna build` the moruna module holds "
        "only kernel and polars; the engine is not in the image"
    )
