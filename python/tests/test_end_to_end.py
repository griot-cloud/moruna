"""The whole promise in one test: a real pass from Python with no parameters (PY-I3, S2).

PY-T3 in the SDD's section k. The dataset is a real Parquet file this test writes, the source,
kernel, sink, arena, reactor, placement engine, scheduler and controller are all real, and the
output is read back and compared row for row.
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

import moruna


@moruna.kernel
def identity(batch: pa.RecordBatch) -> pa.RecordBatch:
    return batch


@moruna.kernel
def normalise(batch: pa.RecordBatch) -> pa.RecordBatch:
    """Lower case the text column, which is the shape of real work without its cost."""
    text = pc.utf8_lower(batch.column("text"))
    return pa.RecordBatch.from_arrays([batch.column("id"), text], names=["id", "text"])


SCRIPT = """
import json, os, pathlib, sys
import pyarrow as pa, pyarrow.parquet as pq
import moruna

d = pathlib.Path(os.environ["MORUNA_TEST_DIR"])
src, out = d / "in", d / "out"
rows = 10_000
pq.write_table(pa.table({"id": pa.array(range(rows), pa.int64()),
                         "text": pa.array([f"row {i}" for i in range(rows)])}),
               src / "part-0.parquet", row_group_size=2_000)

@moruna.kernel
def identity(batch):
    return batch

# S2, PY-I3: no parameters beyond the budget, and here not even that.
report = moruna.run(moruna.ParquetSource(f"file://{src}/part-0.parquet"),
                   identity,
                   moruna.ParquetSink(f"file://{out}"))
back = pq.read_table(str(out))
print(json.dumps({
    "exit": report.exit,
    "run_id": report.run_id,
    "rows_in": report.stages[0]["rows_in"],
    "rows_out": report.stages[0]["rows_out"],
    "rows_back": back.num_rows,
    "ids_match": sorted(back.column("id").to_pylist()) == list(range(rows)),
    "text": str(report),
}))
"""


def test_py_t3_no_parameters(scratch: pathlib.Path) -> None:
    """`moruna.run(source, kernels, sink)` and nothing else completes a real pass (PY-I3, S2).

    Driven in a subprocess with no `MORUNA_BUDGET`, so the ceiling is the one the host gives a
    caller who passed nothing: this is the call the product promises, made exactly as promised.
    """
    (scratch / "in").mkdir()
    (scratch / "out").mkdir()
    env = {k: v for k, v in os.environ.items() if k != "MORUNA_BUDGET"}
    env["MORUNA_TEST_DIR"] = str(scratch)
    proc = subprocess.run(  # noqa: S603
        [sys.executable, "-c", SCRIPT], capture_output=True, text=True, env=env, check=False
    )
    assert proc.returncode == 0, proc.stderr
    result = json.loads(proc.stdout.strip().splitlines()[-1])

    assert result["exit"] == "Completed"
    assert len(result["run_id"]) == 32
    assert result["rows_in"] == 10_000
    assert result["rows_out"] == 10_000
    assert result["rows_back"] == 10_000
    # `ordered=False` is the default, so the rows come back in whatever order the sink wrote
    # them; every row is there exactly once, which is what the run promises.
    assert result["ids_match"] is True
    assert len(result["text"].splitlines()) <= 40


def test_py_t3_normalise(
    dataset: tuple[str, str, int], small_sink, staging
) -> None:  # noqa: ANN001
    """The same pass with a kernel that changes the data, and the change is in the output."""
    src_url, out_url, rows = dataset
    report = moruna.run(
        moruna.ParquetSource(src_url), [normalise], small_sink(out_url), **staging
    )

    assert report.exit == "Completed", report.notes
    out = pathlib.Path(out_url.removeprefix("file://"))
    back = pq.read_table(str(out)).sort_by("id")
    assert back.num_rows == rows
    assert back.column("text")[0].as_py() == "row 0"


def test_a_plain_function_is_wrapped(
    dataset: tuple[str, str, int], small_sink, staging
) -> None:  # noqa: ANN001
    """f.3: a plain callable passed as a kernel is wrapped as if decorated with defaults."""
    src_url, out_url, rows = dataset
    report = moruna.run(
        moruna.ParquetSource(src_url), lambda batch: batch, small_sink(out_url), **staging
    )
    assert report.exit == "Completed", report.notes
    assert report.stages[0]["rows_out"] == rows


def test_no_kernels_is_a_copy(
    dataset: tuple[str, str, int], small_sink, staging
) -> None:  # noqa: ANN001
    """h: `kernels=[]` is allowed; the pipeline is source to sink."""
    src_url, out_url, rows = dataset
    report = moruna.run(moruna.ParquetSource(src_url), [], small_sink(out_url), **staging)
    assert report.exit == "Completed", report.notes
    out = pathlib.Path(out_url.removeprefix("file://"))
    assert pq.read_table(str(out)).num_rows == rows
