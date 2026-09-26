"""Schema declarations and the Polars signature, translated for ``moruna._core`` (05 e.5, e.6).

Nothing here touches a payload. A declaration becomes a small tuple the Rust half reads; a
function whose annotated signature is ``pl.DataFrame -> pl.DataFrame`` (or ``pl.LazyFrame``)
becomes a callable over ``pyarrow.RecordBatch`` that hands the batch to Polars through the Arrow
C data interface and takes the result back the same way.
"""

from __future__ import annotations

import inspect
import os
import re
import sys
import typing
from collections.abc import Mapping, Sequence
from typing import Any

_RELATIVE_KEYS = frozenset({"adds", "drops", "changes"})
_FRAME = re.compile(r"(?:^|\.)(DataFrame|LazyFrame)$")

# Python and Polars types a declaration may name, as the type grammar of 15 e.1 spells them.
_PYTHON_TYPES: dict[Any, str] = {int: "int64", float: "double", str: "string", bool: "bool"}
_POLARS_NAMES = {
    "Int8": "int8",
    "Int16": "int16",
    "Int32": "int32",
    "Int64": "int64",
    "UInt8": "uint8",
    "UInt16": "uint16",
    "UInt32": "uint32",
    "UInt64": "uint64",
    "Float32": "float",
    "Float64": "double",
    "Boolean": "bool",
    "String": "string",
    "Utf8": "string",
    "Binary": "binary",
    "Date": "date32[day]",
}


def type_spelling(value: Any, where: str) -> str:
    """One declared type as the grammar spells it: a ``pyarrow.DataType``, a string, a Python
    type or a Polars type."""
    if isinstance(value, str):
        return value
    if value in _PYTHON_TYPES:
        return _PYTHON_TYPES[value]
    module = getattr(type(value), "__module__", "") or getattr(value, "__module__", "")
    if module.startswith("pyarrow"):
        return str(value)
    if module.startswith("polars"):
        name = getattr(value, "__name__", None) or type(value).__name__
        if name in _POLARS_NAMES:
            return _POLARS_NAMES[name]
        if name == "Datetime":
            unit = getattr(value, "time_unit", None) or "us"
            zone = getattr(value, "time_zone", None)
            return f"timestamp[{unit}, tz={zone}]" if zone else f"timestamp[{unit}]"
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


def _frame_kind(annotation: Any) -> str | None:
    """``"eager"`` or ``"lazy"`` for a Polars frame annotation, ``None`` for anything else."""
    if isinstance(annotation, str):
        match = _FRAME.search(annotation.strip())
        if match and "polars" in sys.modules:
            return "lazy" if match.group(1) == "LazyFrame" else "eager"
        return None
    polars = sys.modules.get("polars")
    if polars is None:
        return None
    if annotation is polars.DataFrame:
        return "eager"
    if annotation is polars.LazyFrame:
        return "lazy"
    return None


def polars_signature(fn: Any) -> str | None:
    """The input frame kind when ``fn`` is annotated ``frame -> frame``, else ``None``."""
    if isinstance(fn, type) or not callable(fn):
        return None
    try:
        signature = inspect.signature(fn)
    except TypeError, ValueError:
        return None
    params = [
        p
        for p in signature.parameters.values()
        if p.kind in (p.POSITIONAL_ONLY, p.POSITIONAL_OR_KEYWORD)
    ]
    if len(params) != 1:
        return None
    try:
        hints = typing.get_type_hints(fn)
    except Exception:  # noqa: BLE001, a hint that does not evaluate is read as its text
        hints = {}
    given = hints.get(params[0].name, params[0].annotation)
    returned = hints.get("return", signature.return_annotation)
    kind = _frame_kind(given)
    if kind is None or _frame_kind(returned) is None:
        return None
    return kind


def _plain(dtype: Any, pa: Any) -> Any:
    """The Arrow type a Polars result column is handed back as: ``large_string`` and
    ``string_view`` as ``string``, the binary equivalents as ``binary``, large lists as lists."""
    if pa.types.is_large_string(dtype) or pa.types.is_string_view(dtype):
        return pa.string()
    if pa.types.is_large_binary(dtype) or pa.types.is_binary_view(dtype):
        return pa.binary()
    if pa.types.is_large_list(dtype) or pa.types.is_list_view(dtype):
        return pa.list_(_plain(dtype.value_type, pa))
    if pa.types.is_list(dtype):
        return pa.list_(_plain(dtype.value_type, pa))
    return dtype


def polars_callable(fn: Any, kind: str) -> Any:
    """``fn`` over Polars frames as a callable over ``pyarrow.RecordBatch`` (05 f.9).

    The batch enters Polars through the Arrow PyCapsule stream, which is the C data interface:
    no copy for fixed-width columns. The result leaves through ``to_arrow``, the same interface,
    rechunked to one chunk only when Polars left it in several, and its large and view types are
    cast to the plain ones a declaration names.
    """
    import polars as pl  # noqa: PLC0415, `import moruna` does not need polars
    import pyarrow as pa  # noqa: PLC0415

    def polars_kernel(batch: Any) -> Any:
        frame = pl.DataFrame(batch)
        result = fn(frame.lazy() if kind == "lazy" else frame)
        if isinstance(result, pl.LazyFrame):
            result = result.collect()
        if not isinstance(result, pl.DataFrame):
            raise TypeError(
                f"a Polars kernel returns a polars.DataFrame or LazyFrame, not "
                f"{type(result).__name__}"
            )
        if result.n_chunks() > 1:
            result = result.rechunk()
        table = result.to_arrow()
        target = pa.schema([pa.field(f.name, _plain(f.type, pa), f.nullable) for f in table.schema])
        batches = table.to_batches()
        out = batches[0] if batches else pa.RecordBatch.from_pylist([], schema=table.schema)
        return out.cast(target) if target != out.schema else out

    polars_kernel.__name__ = getattr(fn, "__name__", "polars_kernel")
    polars_kernel.__qualname__ = getattr(fn, "__qualname__", polars_kernel.__name__)
    polars_kernel.__wrapped__ = fn
    return polars_kernel
