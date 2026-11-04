"""Moruna's standard functions, constructed by arguments rather than written (MH 4.9).

Each returns a ``moruna.Function``, which a job takes wherever it takes a decorated one::

    import moruna

    report = moruna.run(
        source,
        [moruna.std.fill_null(values={"score": 0}),
         moruna.std.filter(expr="score > 10 and country == 'KE'"),
         moruna.std.cast(columns={"score": "double"}),
         moruna.std.select(columns=["id", "score"])],
        sink,
    )

The functions are written in Rust. None of them enters the interpreter, so a chain of them never
takes the GIL, and two adjacent ones are fused into one stage where their combination is one
operation (``select`` after ``cast``, and any run of ``cast``, ``rename``, ``select`` and
``drop``, is one projection; ``filter`` after ``fill_null`` is one pass). Each declares its
schemas as a function of its arguments, so ``python -m moruna check`` checks every one of them.
"""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from typing import Any

from moruna import _core
from moruna._declare import type_spelling

__all__ = [
    "cast",
    "concat_str",
    "date_trunc",
    "dedupe",
    "drop",
    "explode",
    "fill_null",
    "filter",
    "hash",
    "join",
    "mask",
    "rename",
    "select",
    "union",
]


def _build(name: str, args: dict[str, Any]) -> Any:
    from moruna._job import _wrap  # noqa: PLC0415, the package imports this module

    present = {k: v for k, v in args.items() if v is not None}
    return _wrap(_core.std_kernel(name, json.dumps(present, sort_keys=True)))


def _names(value: str | Sequence[str]) -> list[str]:
    return [value] if isinstance(value, str) else [str(v) for v in value]


def cast(columns: Mapping[str, Any], strict: bool | None = None) -> Any:
    """Cast each named column to its type (a ``pyarrow.DataType`` or its spelling).

    A value that does not fit becomes null; with ``strict=True`` it fails the morsel instead.
    """
    types = {str(k): type_spelling(v, f"cast columns[{k!r}]") for k, v in columns.items()}
    return _build("cast", {"columns": types, "strict": strict})


def rename(columns: Mapping[str, str]) -> Any:
    """Rename columns, old name to new name."""
    return _build("rename", {"columns": {str(k): str(v) for k, v in columns.items()}})


def select(columns: str | Sequence[str]) -> Any:
    """Keep these columns, in this order."""
    return _build("select", {"columns": _names(columns)})


def drop(columns: str | Sequence[str]) -> Any:
    """Remove these columns."""
    return _build("drop", {"columns": _names(columns)})


def filter(expr: str) -> Any:  # noqa: A001, the name is the function's
    """Keep the rows where ``expr`` is true.

    ``expr`` compares columns with literals or with each other (``==``, ``!=``, ``<``, ``<=``,
    ``>``, ``>=``), tests ``is_null(c)`` and ``is_not_null(c)``, and combines them with ``and``,
    ``or``, ``not`` and parentheses. A column name with spaces is written in backquotes.
    """
    return _build("filter", {"expr": expr})


def fill_null(values: Mapping[str, Any]) -> Any:
    """Replace each named column's nulls with a number, a string or a boolean."""
    return _build("fill_null", {"values": {str(k): v for k, v in values.items()}})


def dedupe(keys: str | Sequence[str]) -> Any:
    """Keep the first row seen for each key, across the whole run (stateful, one instance)."""
    return _build("dedupe", {"keys": _names(keys)})


def hash(  # noqa: A001, the name is the function's
    columns: str | Sequence[str], algo: str | None = None, output: str | None = None
) -> Any:
    """Append ``output`` (default ``"hash"``): a hex digest, ``sha256`` (the default) or
    ``blake3``, of the named columns. Arguments left out are left out of the fingerprint too."""
    return _build("hash", {"columns": _names(columns), "algo": algo, "output": output})


def mask(columns: str | Sequence[str], mode: str | None = None, keep: int | None = None) -> Any:
    """Hide string columns in place: ``redact`` (every character ``*``), ``partial`` (all but
    the last ``keep``, default 4), ``hash`` (SHA-256 hex) or ``null`` (any column type); the
    default mode is ``redact``."""
    return _build("mask", {"columns": _names(columns), "mode": mode, "keep": keep})


def explode(column: str) -> Any:
    """One row per element of a list column; a null or empty list gives one null element."""
    return _build("explode", {"column": column})


def concat_str(
    columns: Sequence[str], separator: str | None = None, output: str | None = None
) -> Any:
    """Append ``output`` (default ``"concat"``): the named columns as text joined by
    ``separator`` (default empty); null if any is."""
    return _build(
        "concat_str", {"columns": _names(columns), "separator": separator, "output": output}
    )


def date_trunc(column: str, unit: str, output: str | None = None) -> Any:
    """Truncate a timestamp or date32 column to ``year``, ``month``, ``day``, ``hour``,
    ``minute`` or ``second`` (in UTC), in place or into a new column ``output``."""
    return _build("date_trunc", {"column": column, "unit": unit, "output": output})


def join() -> Any:
    """Join branches of one upstream side by side: for each morsel, the columns every input
    gives for it, port order, in one batch with that morsel's rows. A function of a ``moruna.Job``
    with several inputs (``inputs=[a, b]``), each a branch of the same upstream through
    functions that keep its rows and their order. A row count that differs between inputs, or a
    column two inputs both give, is refused naming the ports; drop or rename such a column in a
    branch before the join."""
    return _build("join", {})


def union() -> Any:
    """Pass on the batches of every input, each as it came; every input has one schema. A
    function of a ``moruna.Job`` with several inputs."""
    return _build("union", {})
