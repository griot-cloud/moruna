"""Schema declarations, translated for ``moruna._core`` (MH 4.9).

Nothing here touches a payload. A declaration becomes a small tuple the Rust half reads.
"""

from __future__ import annotations

import os
from collections.abc import Mapping, Sequence
from typing import Any

_RELATIVE_KEYS = frozenset({"adds", "drops", "changes"})

# Python types a declaration may name, as the type grammar of MH 4.9 spells them.
_PYTHON_TYPES: dict[Any, str] = {int: "int64", float: "double", str: "string", bool: "bool"}


def type_spelling(value: Any, where: str) -> str:
    """One declared type as the grammar spells it: a ``pyarrow.DataType``, a string or a Python
    type."""
    if isinstance(value, str):
        return value
    if value in _PYTHON_TYPES:
        return _PYTHON_TYPES[value]
    module = getattr(type(value), "__module__", "") or getattr(value, "__module__", "")
    if module.startswith("pyarrow"):
        return str(value)
    raise TypeError(f"{where}: {value!r} is not a type moruna can declare")


def _columns(value: Any, where: str) -> list[tuple[str, str, bool]]:
    if hasattr(value, "names") and hasattr(value, "field"):  # a pyarrow.Schema
        return [(f.name, str(f.type), bool(f.nullable)) for f in value]
    if isinstance(value, Mapping):
        return [(str(k), type_spelling(v, f"{where}[{k!r}]"), True) for k, v in value.items()]
    raise TypeError(f"{where} must be a pyarrow.Schema or a mapping of column to type")


def declaration(value: Any, where: str) -> tuple[Any, ...] | None:
    """``input_schema=`` or ``output_schema=`` as the tuple ``_core.build_kernel`` reads.

    A ``pyarrow.Schema`` is exact. A mapping of column to type is a subset. For an output, a
    mapping whose keys are only ``adds``, ``drops`` and ``changes`` is relative to the input.
    """
    if value is None:
        return None
    if hasattr(value, "names") and hasattr(value, "field"):
        return ("exact", _columns(value, where))
    if isinstance(value, Mapping):
        keys = set(value)
        if where == "output_schema" and keys and keys <= _RELATIVE_KEYS:
            drops = value.get("drops") or []
            if isinstance(drops, str) or not isinstance(drops, Sequence):
                raise TypeError("output_schema['drops'] must be a list of column names")
            return (
                "relative",
                _columns(value.get("adds") or {}, "output_schema['adds']"),
                [str(d) for d in drops],
                _columns(value.get("changes") or {}, "output_schema['changes']"),
            )
        return ("subset", _columns(value, where))
    raise TypeError(f"{where} must be a pyarrow.Schema or a mapping of column to type")


def lockfile_bytes(value: Any) -> bytes | None:
    """``lockfile=``: a path read now, or bytes as they are (MH 4.9, the fingerprint)."""
    if value is None:
        return None
    if isinstance(value, bytes | bytearray):
        return bytes(value)
    with open(os.fspath(value), "rb") as handle:
        return handle.read()
