"""The two ports a job runs through: the resolver that supplies what its tasks need, and the
writer that keeps its spec and its report.

A **resolver** answers a need by name with the object the task receives. Moruna does not read a
need's name: ``"warehouse"``, ``"warehouse.read"`` and ``"model"`` are strings the task and the
runner agree on. ``moruna.run(job, ports={...})`` wraps a mapping in ``moruna.Ports``, the
in-process adapter; a host that runs Moruna supplies its own ``moruna.Resolver`` (to
``moruna.run``'s ``ports=``, or to ``moruna run --resolver module:attribute``). A need the
resolver cannot supply is refused by name before the run starts.

A **writer** keeps the bytes of a job's spec and of its run's report. ``moruna.FileWriter``
writes a file, ``moruna.BytesWriter`` hands the bytes to a callable, and any object with
``write_spec(data)`` and ``write_report(data)`` is a writer too, which is how a writer of
another package plugs in.
"""

from __future__ import annotations

import importlib
import os
from collections.abc import Callable, Mapping
from pathlib import Path
from typing import Any

__all__ = ["BytesWriter", "FileWriter", "Ports", "Resolver", "Writer"]


class Resolver:
    """The port that supplies a task's needs: ``resolve(need)`` returns the object the task
    receives under that name, or raises ``LookupError`` when it cannot supply it."""

    def resolve(self, need: str) -> Any:
        raise LookupError(need)


class Ports(Resolver):
    """The in-process resolver: the objects of a mapping, by name."""

    def __init__(self, objects: Mapping[str, Any]) -> None:
        for name in objects:
            if not isinstance(name, str) or not name:
                raise TypeError(f"a need is named by a non-empty string, not {name!r}")
        self._objects = dict(objects)

    def resolve(self, need: str) -> Any:
        try:
            return self._objects[need]
        except KeyError:
            raise LookupError(need) from None

    def __repr__(self) -> str:
        return f"moruna.Ports({sorted(self._objects)!r})"


def resolver_of(ports: Any) -> Resolver | None:
    """``ports=`` as a resolver: ``None``, a mapping (the in-process adapter), or a resolver."""
    if ports is None:
        return None
    if isinstance(ports, Resolver) or callable(getattr(ports, "resolve", None)):
        return ports  # type: ignore[no-any-return]
    if isinstance(ports, Mapping):
        return Ports(ports)
    raise TypeError(
        f"ports= takes a mapping of need to object or a moruna.Resolver, not {type(ports).__name__}"
    )


def load_resolver(reference: str) -> Resolver:
    """The resolver ``module:attribute`` names (``moruna run --resolver``): a resolver, or a
    callable taking nothing that returns one."""
    module, _, attribute = reference.partition(":")
    if not module or not attribute:
        raise ValueError(f"--resolver {reference!r}: name it as module:attribute")
    target: Any = importlib.import_module(module)
    for part in attribute.split("."):
        target = getattr(target, part)
    if isinstance(target, type) or (
        not callable(getattr(target, "resolve", None)) and callable(target)
    ):
        target = target()
    if not callable(getattr(target, "resolve", None)):
        raise TypeError(f"--resolver {reference!r} names no moruna.Resolver")
    return target  # type: ignore[no-any-return]


class Writer:
    """The port that keeps a job's spec and its run's report: each method takes the exact bytes
    and keeps them, raising when it cannot."""

    def write_spec(self, data: bytes) -> None:
        raise NotImplementedError

    def write_report(self, data: bytes) -> None:
        raise NotImplementedError


class FileWriter(Writer):
    """A file: the spec at ``path``, the report beside it at ``<stem>.report.json``."""

    def __init__(self, path: str | os.PathLike[str]) -> None:
        self.path = Path(path)

    @property
    def report_path(self) -> Path:
        return self.path.with_name(f"{self.path.stem}.report.json")

    def write_spec(self, data: bytes) -> None:
        _write(self.path, data)

    def write_report(self, data: bytes) -> None:
        _write(self.report_path, data)

    def __repr__(self) -> str:
        return f"moruna.FileWriter({str(self.path)!r})"


class BytesWriter(Writer):
    """A caller's byte sink: the callable is given the spec's bytes, then the report's."""

    def __init__(self, sink: Callable[[bytes], Any]) -> None:
        if not callable(sink):
            raise TypeError(f"a byte sink is a callable taking bytes, not {type(sink).__name__}")
        self.sink = sink

    def write_spec(self, data: bytes) -> None:
        self.sink(data)

    def write_report(self, data: bytes) -> None:
        self.sink(data)


def writer_of(target: Any) -> Writer:
    """A writer for ``target``: a writer (anything with ``write_spec`` and ``write_report``), a
    file path, or a callable taking bytes. A URL is refused: this release writes no object
    store."""
    if isinstance(target, Writer) or (
        callable(getattr(target, "write_spec", None))
        and callable(getattr(target, "write_report", None))
    ):
        return target  # type: ignore[no-any-return]
    if isinstance(target, (str, os.PathLike)):
        text = os.fspath(target)
        if "://" in str(text):
            raise ValueError(
                f"{text}: a spec is written to a file path, a callable or a moruna.Writer; "
                "an object-store URL is not a target in this release"
            )
        return FileWriter(text)
    if callable(target):
        return BytesWriter(target)
    raise TypeError(
        "a spec is written to a file path, a callable taking bytes or a moruna.Writer, "
        f"not {type(target).__name__}"
    )


def _write(path: Path, data: bytes) -> None:
    """``data`` at ``path``, whole or not at all: written beside it, then renamed over it."""
    path.parent.mkdir(parents=True, exist_ok=True)
    partial = path.with_name(f".{path.name}.partial")
    partial.write_bytes(data)
    partial.replace(path)
