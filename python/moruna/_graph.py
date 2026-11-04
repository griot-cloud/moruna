"""``moruna.Graph``: a run as a DAG of named tasks (SDD 12 d.2, SDD 22 d.4).

A graph holds what it was given and checks only the arguments of each call. It is built into the
same version 2 job a job document is, the in-process objects bound to their tasks, when it is
passed to ``moruna.run(graph=...)``; unknown names, cycles and an output no task reads are
refused there, by name, before anything starts.
"""

from __future__ import annotations

from collections.abc import Sequence
from typing import Any, NamedTuple


class GraphTask(NamedTuple):
    """One task of a graph: its role, its name, its object and the tasks that feed it."""

    role: str
    name: str
    object: Any
    inputs: tuple[str, ...]


class Graph:
    """A run as a DAG of named tasks: sources, kernels and sinks joined by name.

    Each task takes its inputs from the tasks it names, which may be added before or after it.
    A task's output may feed many tasks (fan-out); a kernel may take several inputs (fan-in),
    such as ``moruna.std.join()``, which puts the columns of branches of one upstream side by
    side, or ``moruna.std.union()``, which passes on every batch of every input::

        g = moruna.Graph()
        g.source("rows", moruna.ParquetSource("in/"))
        g.kernel("words", count_words, inputs="rows")
        g.kernel("lang", detect_language, inputs="rows")
        g.kernel("both", moruna.std.join(), inputs=["words", "lang"])
        g.sink("out", moruna.ParquetSink("out/"), input="both")
        report = moruna.run(graph=g, budget="2GiB")

    A kernel of one input takes it on its port ``in``; one of several takes each on a port named
    after the task that feeds it. Branches that are ready at once run at once on the worker
    pool.
    """

    def __init__(self) -> None:
        self._tasks: list[GraphTask] = []

    def source(self, name: str, source: Any) -> str:
        """Add the source ``source`` (a ``moruna.*Source`` or a ``moruna.Source`` subclass
        instance) as the task ``name``; returns ``name``."""
        return self._add("source", name, source, ())

    def kernel(self, name: str, kernel: Any, inputs: str | Sequence[str]) -> str:
        """Add ``kernel`` (anything ``moruna.run`` takes as a kernel) as the task ``name``, fed
        by the task or tasks ``inputs`` names, in port order; returns ``name``."""
        if isinstance(inputs, str):
            named: tuple[str, ...] = (inputs,)
        else:
            named = tuple(inputs)
        for upstream in named:
            if not isinstance(upstream, str):
                raise TypeError(f"the inputs of {name!r} are task names, not {upstream!r}")
        return self._add("kernel", name, kernel, named)

    def sink(self, name: str, sink: Any, input: str) -> str:  # noqa: A002, the input it takes
        """Add the sink ``sink`` (a ``moruna.*Sink`` or a ``moruna.Sink`` subclass instance) as
        the task ``name``, fed by the task ``input``; returns ``name``."""
        if not isinstance(input, str):
            raise TypeError(f"the input of {name!r} is a task name, not {input!r}")
        return self._add("sink", name, sink, (input,))

    @property
    def tasks(self) -> tuple[GraphTask, ...]:
        """The tasks, in the order they were added."""
        return tuple(self._tasks)

    def __repr__(self) -> str:
        shown = ", ".join(
            f"{t.role} {t.name!r}" + (f" <- {', '.join(t.inputs)}" if t.inputs else "")
            for t in self._tasks
        )
        return f"moruna.Graph({shown})"

    def _add(self, role: str, name: str, obj: Any, inputs: tuple[str, ...]) -> str:
        if not isinstance(name, str):
            raise TypeError(f"a task's name is a string, not {name!r}")
        if any(t.name == name for t in self._tasks):
            raise ValueError(f"the graph has a task named {name!r} already")
        self._tasks.append(GraphTask(role, name, obj, inputs))
        return name
