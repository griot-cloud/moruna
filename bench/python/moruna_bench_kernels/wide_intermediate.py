"""``wide-intermediate``: the Python kernel (preamble 6.5, about 20, releases the GIL).

What it does
------------

The kernel is handed a morsel as a mapping of column name to NumPy array, takes
the numeric columns in the order they appear, and returns the degree two
expansion of each row: the upper triangle of the outer product of the row's
feature vector with itself, the diagonal included. ``k`` features become
``k (k + 1) / 2``, so the intermediate is wide as well as tall, which is what the
name means and what makes the amplification large enough to matter to a memory
budget.

On ``wide-mixed``, the dataset ``bench/README.md`` pairs with this kernel, 74
columns carry 64 numeric ones, so a row of about 830 bytes becomes 2080 float64
values, and the amplification is about 20.

Which call releases the GIL
---------------------------

:data:`GIL_RELEASING_CALL`, ``numpy.matmul``, applied to the stacked
``n x k x 1`` by ``n x 1 x k`` batch in :func:`expand`. NumPy's matmul is a
generalised ufunc whose inner loop runs between ``NPY_BEGIN_THREADS`` and
``NPY_END_THREADS`` and dispatches to the BLAS ``dgemm`` for float64, so the
interpreter lock is not held while the arithmetic runs. ``test_wide_intermediate.py``
measures it: a monitor thread keeps running throughout the call, which it could
not do if the lock were held for the call's duration.

Determinism
-----------

The output is a pure function of the input array bytes: no clock, no random
stream, no iteration over an unordered mapping (columns are taken in the order
the mapping yields them, which is insertion order for a ``dict`` and schema order
for a table read with pyarrow). BLAS is deterministic for a fixed library and
shape, so two runs on one host give identical bytes.
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

import numpy as np

__all__ = [
    "GIL_RELEASING_CALL",
    "NUMERIC_KINDS",
    "expand",
    "feature_matrix",
    "numeric_columns",
    "payload_bytes",
    "wide_intermediate",
]

#: The NumPy call inside :func:`expand` that releases the GIL.
GIL_RELEASING_CALL = "numpy.matmul"

#: The NumPy dtype kinds contracts section e.3 maps to a ``DType``: signed and
#: unsigned integers and floats. Booleans are excluded, as e.3 excludes them.
NUMERIC_KINDS = frozenset("iuf")


def numeric_columns(columns: Mapping[str, Any]) -> dict[str, np.ndarray]:
    """The numeric columns of a morsel, in the order the mapping yields them."""
    numeric: dict[str, np.ndarray] = {}
    for name, values in columns.items():
        array = np.asarray(values)
        if array.dtype.kind in NUMERIC_KINDS:
            numeric[name] = array
    return numeric


def feature_matrix(columns: Mapping[str, Any]) -> np.ndarray:
    """The ``rows x k`` float64 matrix the numeric columns make.

    Raises ``ValueError`` when there are no numeric columns, when a column is not
    one dimensional, or when the columns disagree about their length: each is a
    morsel the kernel cannot work on, and a silent reshape would hide it.
    """
    numeric = numeric_columns(columns)
    if not numeric:
        raise ValueError("wide-intermediate: the morsel holds no numeric column")
    lengths = {name: array.shape for name, array in numeric.items()}
    for name, shape in lengths.items():
        if len(shape) != 1:
            raise ValueError(
                f"wide-intermediate: column {name} has shape {shape}, expected one dimension"
            )
    rows = {shape[0] for shape in lengths.values()}
    if len(rows) != 1:
        raise ValueError(
            f"wide-intermediate: the numeric columns disagree about their length: {lengths}"
        )
    return np.stack([array.astype(np.float64, copy=False) for array in numeric.values()], axis=1)


def expand(features: np.ndarray) -> np.ndarray:
    """The degree two expansion of every row of ``features``.

    ``features`` is ``rows x k`` and the result is ``rows x k (k + 1) / 2``: the
    upper triangle, diagonal included, of each row's outer product with itself.

    The outer product is ``numpy.matmul`` of an ``n x k x 1`` by an ``n x 1 x k``
    batch, which is the call that releases the GIL.
    """
    if features.ndim != 2:
        raise ValueError(
            f"wide-intermediate: features have shape {features.shape}, expected two dimensions"
        )
    contiguous = np.ascontiguousarray(features, dtype=np.float64)
    width = contiguous.shape[1]
    # The GIL is released here, for the whole of the batched dgemm.
    outer = np.matmul(contiguous[:, :, None], contiguous[:, None, :])
    upper_rows, upper_cols = np.triu_indices(width)
    return np.ascontiguousarray(outer[:, upper_rows, upper_cols])


def wide_intermediate(columns: Mapping[str, Any]) -> np.ndarray:
    """Apply the kernel to one morsel.

    This is the function the runtime's Python adapter (component 5) will call.
    Its shape is the one contracts section d.7 gives ``Kernel::apply``, reduced to
    what Python can carry: a payload in, a payload out, an exception where the
    Rust side returns ``Err``.
    """
    return expand(feature_matrix(columns))


def payload_bytes(columns: Mapping[str, Any]) -> int:
    """The bytes a morsel occupies, the denominator of the amplification.

    Numeric and fixed width columns report ``nbytes``. A column of Python objects
    (which is how pyarrow hands back a string column) is summed element by
    element, because ``nbytes`` for an object array counts pointers rather than
    the text they point at, and the text is what a morsel carries.
    """
    total = 0
    for values in columns.values():
        array = np.asarray(values)
        if array.dtype.kind in ("O", "S", "U"):
            for item in array.ravel():
                if item is None:
                    continue
                if isinstance(item, bytes):
                    total += len(item)
                else:
                    total += len(str(item).encode("utf-8"))
        else:
            total += int(array.nbytes)
    return total
