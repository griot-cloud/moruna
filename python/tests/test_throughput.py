"""A Python kernel over enough rows to be a measurement, under a wall clock bound.

The suite that shipped the wheel was green while the surface was unusable: every test used a
handful of rows and every test spelled its destination as a `file://` URL, so no test ever paid
the per-run cost of a real pass and no test ever wrote to a path spelled the way a user spells
one. This test is both of those at once, and it is driven in a subprocess with a timeout because
the failure it exists to catch was a run that never returned, which no in-process assertion can
observe.

The bound is deliberately loose. The job is trivial (an uppercase over one string column), the
whole dataset is a few megabytes, and the same job with a Rust kernel is `examples/append_column`
at about a second. A Python kernel that crosses once per morsel is in the same order; a boundary
that crosses once per row, attaches per row, or copies in both directions is a hundred times over
the bound, and so is a run that hangs. A figure that drifts towards the bound is a regression
worth reading even when the assertion still passes, so the measurement is printed either way.
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
import time

import pytest

#: Rows the measurement runs over: enough that a per-row boundary cost cannot hide in the noise.
ROWS = 200_000
#: The wall clock bound, in seconds, for the whole subprocess: interpreter start, pyarrow import,
#: writing the input, the run itself and reading the output back.
BUDGET_S = 60.0
#: How long the harness waits before calling it a hang rather than a slow run.
TIMEOUT_S = 180.0

SCRIPT = """
import json, os, pathlib, time
import pyarrow as pa, pyarrow.compute as pc, pyarrow.parquet as pq
import amoru

d = pathlib.Path(os.environ["AMORU_TEST_DIR"])
src, out = d / "in", d / "out"
src.mkdir(exist_ok=True); out.mkdir(exist_ok=True)
rows = int(os.environ["AMORU_TEST_ROWS"])
pq.write_table(pa.table({"id": pa.array(range(rows), pa.int64()),
                         "text": pa.array([f"row-{i}" for i in range(rows)])}),
               src / "part-0.parquet", row_group_size=20_000)

@amoru.kernel
def append_upper(batch):
    return batch.append_column("upper", pc.utf8_upper(batch.column("text")))

# The destination is a bare filesystem path, which is how a user writes one. A `file://` URL is
# the other spelling and both must work.
started = time.perf_counter()
report = amoru.run(amoru.ParquetSource(str(src / "part-0.parquet")),
                   append_upper,
                   amoru.ParquetSink(str(out), row_group_bytes="16MiB", file_bytes="64MiB"),
                   budget="512MiB")
run_s = time.perf_counter() - started
back = pq.read_table(str(out))
print(json.dumps({
    "exit": report.exit,
    "run_s": run_s,
    "rows_out": report.stages[0]["rows_out"],
    "rows_back": back.num_rows,
    "has_upper": "upper" in back.column_names,
    "morsels": report.stages[0]["morsels"],
    "gil": report.gil,
    "gil_serialised": report.gil_serialised,
}))
"""


def test_a_python_kernel_over_200k_rows_finishes_inside_a_wall_clock_bound(
    scratch: pathlib.Path,
) -> None:
    env = dict(os.environ)
    env["AMORU_TEST_DIR"] = str(scratch)
    env["AMORU_TEST_ROWS"] = str(ROWS)
    started = time.perf_counter()
    try:
        proc = subprocess.run(  # noqa: S603
            [sys.executable, "-c", SCRIPT],
            capture_output=True,
            text=True,
            env=env,
            check=False,
            timeout=TIMEOUT_S,
        )
    except subprocess.TimeoutExpired:
        pytest.fail(
            f"a Python kernel over {ROWS} rows did not return in {TIMEOUT_S}s; "
            "the run hung rather than ran"
        )
    total_s = time.perf_counter() - started
    assert proc.returncode == 0, proc.stderr
    result = json.loads(proc.stdout.strip().splitlines()[-1])

    # Printed whether or not the assertion fires: a human reading CI output sees the figure.
    print(
        f"\npython kernel throughput: {ROWS} rows, run {result['run_s']:.3f}s, "
        f"process {total_s:.3f}s, bound {BUDGET_S:.0f}s, "
        f"morsels {result['morsels']}, gil {result['gil']} "
        f"serialised={result['gil_serialised']}"
    )

    assert result["exit"] == "Completed"
    assert result["rows_out"] == ROWS
    assert result["rows_back"] == ROWS
    assert result["has_upper"] is True
    assert total_s < BUDGET_S, (
        f"{ROWS} rows through a Python kernel took {total_s:.1f}s, over the {BUDGET_S:.0f}s "
        f"bound (the run itself was {result['run_s']:.3f}s)"
    )
