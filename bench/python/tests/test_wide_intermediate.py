"""Tests for the ``wide-intermediate`` kernel (preamble section 6.5).

Run them from ``bench/python``::

    uv run --python 3.14 --with numpy --with pyarrow --with pytest pytest
    uv run --python 3.14t --with numpy --with pyarrow --with pytest pytest

The amplification test over the generated dataset needs the dataset. Write it
first::

    cargo run --release -p amoru-bench -- suite --scale small --local-only --out bench/data

With the file absent the test is skipped with that reason printed, never failed,
which is the same rule the generator's S3 test follows.
"""

from __future__ import annotations

import os
import sys
import threading
import time
from pathlib import Path

import numpy as np
import pytest

from amoru_bench_kernels import wide_intermediate as kernel

#: The band preamble 6.5's "about 20" is taken to mean, the same band the Rust
#: side declares in ``WideIntermediate::hints``.
BAND = (15.0, 25.0)

#: Where ``amoru-bench suite`` writes by default: ``bench/data``, two levels up.
DATA_DIR = Path(__file__).resolve().parents[2] / "data"

#: The dataset ``bench/README.md`` pairs with this kernel.
DATASET = "wide-mixed.parquet"


def deterministic_columns(rows: int, ints: int, floats: int, strings: int, texts: int) -> dict:
    """A morsel with the column mix of ``wide-mixed``, built from a fixed stream.

    No random seed and no clock: every value is a function of its row and column
    index, so the test measures the same thing on every host and every run.
    """
    columns: dict[str, np.ndarray] = {}
    index = np.arange(rows, dtype=np.int64)
    for column in range(ints):
        columns[f"i64_{column}"] = (index * 7 + column) % 1_000_003
    for column in range(floats):
        columns[f"f64_{column}"] = ((index * 13 + column) % 997).astype(np.float64) / 997.0
    words = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"]
    for column in range(strings):
        columns[f"str_{column}"] = np.array(
            [f"{words[(row + column) % len(words)]}{(row + column) % 100:02d}" for row in range(rows)],
            dtype=object,
        )
    for column in range(texts):
        columns[f"text_{column}"] = np.array(
            [("Morsel arena; SPILL staging, reactor " * 4)[: 96 + (row + column) % 64] for row in range(rows)],
            dtype=object,
        )
    return columns


def test_the_expansion_is_the_upper_triangle_of_the_outer_product():
    features = np.array([[1.0, 2.0, 3.0]])
    out = kernel.expand(features)
    # [1*1, 1*2, 1*3, 2*2, 2*3, 3*3]
    assert out.shape == (1, 6)
    assert np.array_equal(out[0], np.array([1.0, 2.0, 3.0, 4.0, 6.0, 9.0]))


def test_k_features_become_k_times_k_plus_one_over_two():
    for width in (1, 2, 5, 16, 64):
        out = kernel.expand(np.ones((3, width)))
        assert out.shape == (3, width * (width + 1) // 2)


def test_only_numeric_columns_take_part_and_the_order_is_the_mapping_s():
    columns = {
        "i64_0": np.array([1, 2], dtype=np.int64),
        "str_0": np.array(["a", "b"], dtype=object),
        "flag": np.array([True, False]),
        "f64_0": np.array([10.0, 20.0]),
    }
    assert list(kernel.numeric_columns(columns)) == ["i64_0", "f64_0"]
    features = kernel.feature_matrix(columns)
    assert features.dtype == np.float64
    assert np.array_equal(features, np.array([[1.0, 10.0], [2.0, 20.0]]))


def test_a_morsel_the_kernel_cannot_work_on_raises_rather_than_reshaping():
    with pytest.raises(ValueError, match="no numeric column"):
        kernel.wide_intermediate({"str_0": np.array(["a"], dtype=object)})
    with pytest.raises(ValueError, match="expected one dimension"):
        kernel.wide_intermediate({"f64_0": np.ones((2, 2))})
    with pytest.raises(ValueError, match="disagree about their length"):
        kernel.wide_intermediate({"a": np.ones(2), "b": np.ones(3)})
    with pytest.raises(ValueError, match="expected two dimensions"):
        kernel.expand(np.ones(4))


def test_the_same_input_twice_gives_the_same_bytes():
    columns = deterministic_columns(64, 4, 4, 1, 1)
    first = kernel.wide_intermediate(columns)
    second = kernel.wide_intermediate(columns)
    assert first.tobytes() == second.tobytes()


def test_payload_bytes_counts_text_and_not_pointers():
    columns = {
        "f64_0": np.zeros(4, dtype=np.float64),
        "str_0": np.array(["abc", "de", None, b"fghi"], dtype=object),
    }
    assert kernel.payload_bytes(columns) == 4 * 8 + 3 + 2 + 4


def test_the_amplification_on_a_wide_mixed_shaped_morsel_is_about_twenty():
    """The band of preamble 6.5, measured on the column mix of ``wide-mixed``.

    This runs everywhere, with no generated file, because the mix is what sets
    the ratio: 32 i64 and 32 f64 make 64 features and 2080 expanded values, and
    the 8 short string and 2 text columns are carried in the denominator.
    """
    columns = deterministic_columns(256, 32, 32, 8, 2)
    out = kernel.wide_intermediate(columns)
    measured = out.nbytes / kernel.payload_bytes(columns)
    assert BAND[0] <= measured <= BAND[1], f"amplification {measured:.3f} outside {BAND}"


def test_the_amplification_on_the_generated_wide_mixed_is_about_twenty():
    """The same band, on the file the generator writes."""
    pyarrow_parquet = pytest.importorskip(
        "pyarrow.parquet", reason="pyarrow is not installed; install it to read the generated file"
    )
    path = DATA_DIR / DATASET
    if not path.exists():
        pytest.skip(
            f"{path} does not exist; write it with "
            "`cargo run --release -p amoru-bench -- suite --scale small --local-only --out bench/data`"
        )
    table = pyarrow_parquet.read_table(path)
    columns = {
        name: table.column(name).to_numpy(zero_copy_only=False) for name in table.column_names
    }
    out = kernel.wide_intermediate(columns)
    measured = out.nbytes / kernel.payload_bytes(columns)
    assert BAND[0] <= measured <= BAND[1], f"amplification {measured:.3f} outside {BAND}"


def test_the_gil_releasing_call_is_named():
    assert kernel.GIL_RELEASING_CALL == "numpy.matmul"


def test_the_expansion_releases_the_gil():
    """A monitor thread keeps running while the expansion runs.

    If ``numpy.matmul`` held the interpreter lock for the duration of the call,
    the monitor thread could not be scheduled at all during it, and the longest
    gap between two of its observations would be the length of the call. The
    assertion is that the longest gap is well under that, which is only possible
    if the lock is released. On a free threaded build there is no lock to release
    and the same assertion holds for the same reason.

    The test is a timing observation, so it is generous: the threshold is half
    the call's own duration, not a few milliseconds, and the test is skipped
    when the call turns out too short to observe on this host.
    """
    if os.cpu_count() is None or (os.cpu_count() or 1) < 2:
        pytest.skip("a single processor cannot run the monitor thread beside the call")

    features = np.ascontiguousarray(
        np.stack(
            [((np.arange(4096, dtype=np.int64) * (c + 3)) % 251).astype(np.float64) for c in range(96)],
            axis=1,
        )
    )
    gaps: list[float] = []
    stop = threading.Event()

    def monitor() -> None:
        last = time.perf_counter()
        while not stop.is_set():
            now = time.perf_counter()
            gaps.append(now - last)
            last = now

    watcher = threading.Thread(target=monitor, name="gil-monitor")
    watcher.start()
    try:
        time.sleep(0.05)
        before = len(gaps)
        start = time.perf_counter()
        kernel.expand(features)
        duration = time.perf_counter() - start
        during = gaps[before:]
    finally:
        stop.set()
        watcher.join()

    if duration < 0.05:
        pytest.skip(f"the expansion took {duration:.4f}s, too short to observe on this host")
    assert during, "the monitor thread observed nothing while the expansion ran"
    longest = max(during)
    assert longest < duration / 2, (
        f"the longest gap was {longest:.4f}s of a {duration:.4f}s call, "
        "which is what holding the lock would look like"
    )


def test_the_interpreter_reports_its_gil_state():
    """Both interpreters of the documented command run these tests.

    Free threaded or not, the kernel behaves the same; this records which build
    ran, so a report can say the suite passed on both.
    """
    reported = getattr(sys, "_is_gil_enabled", None)
    state = "free-threaded" if reported is not None and not reported() else "gil"
    assert state in ("free-threaded", "gil")
    print(f"interpreter {sys.version.split()[0]} runs {state}")
