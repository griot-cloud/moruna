"""PY-T4 gil_refused: a Python kernel under a GIL interpreter is refused by default (PY-I4).

The free-threaded interpreter under `PYTHON_GIL=1` is the GIL build (preamble 6.6), so the
refusal and its escape hatch are driven in a subprocess with that variable set.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys

import pytest

import moruna

REFUSAL = (
    "Python kernels require a free-threaded interpreter (python3.13t or python3.14t); "
    "pass allow_gil=True to run serialised"
)

SCRIPT = """
import os, pathlib, sys, json
import pyarrow as pa, pyarrow.parquet as pq
import moruna

assert sys._is_gil_enabled(), "this subprocess is meant to have a GIL"
d = pathlib.Path(os.environ["MORUNA_TEST_DIR"])
src, out = d / "in", d / "out"
src.mkdir(exist_ok=True); out.mkdir(exist_ok=True)
pq.write_table(pa.table({"id": pa.array(range(1000), pa.int64())}), src / "p.parquet")

@moruna.kernel
def identity(batch):
    return batch

source = lambda: moruna.ParquetSource(f"file://{src}/p.parquet")
sink = lambda: moruna.ParquetSink(f"file://{out}", row_group_bytes="16MiB", file_bytes="64MiB")

try:
    moruna.run(source(), identity, sink())
except moruna.ConfigError as e:
    print(json.dumps({"refused": True, "message": e.message, "kind": e.kind}))
else:
    print(json.dumps({"refused": False}))

report = moruna.run(source(), identity, sink(), allow_gil=True)
print(json.dumps({"gil_serialised": report.gil_serialised, "gil": report.gil}))
"""


def _under_a_gil(tmp: str) -> list[dict]:
    env = dict(os.environ, PYTHON_GIL="1", MORUNA_TEST_DIR=tmp)
    out = subprocess.run(  # noqa: S603
        [sys.executable, "-c", SCRIPT], capture_output=True, text=True, env=env, check=False
    )
    assert out.returncode == 0, out.stderr
    return [json.loads(line) for line in out.stdout.strip().splitlines() if line.startswith("{")]


def test_py_t4_gil_refused(scratch) -> None:  # noqa: ANN001
    refusal, allowed = _under_a_gil(str(scratch))
    assert refusal["refused"] is True
    assert refusal["kind"] == "Config"
    assert REFUSAL in refusal["message"]
    assert allowed["gil_serialised"] is True
    assert allowed["gil"], "the report names the stage that ran serialised"
    assert allowed["gil"][0][1] == "Serialised"


def test_a_free_threaded_interpreter_is_not_refused(
    dataset: tuple[str, str, int], staging
) -> None:  # noqa: ANN001
    """The same run on this interpreter, which is free threaded, needs no `allow_gil`."""
    if sys._is_gil_enabled():  # noqa: SLF001
        pytest.skip("this interpreter has a GIL; the refusal path is the subprocess test")
    src_url, out_url, _ = dataset

    @moruna.kernel
    def identity(batch):  # noqa: ANN001, ANN202
        return batch

    report = moruna.run(
        moruna.ParquetSource(src_url),
        identity,
        moruna.ParquetSink(out_url, row_group_bytes="16MiB", file_bytes="64MiB"),
        **staging,
    )
    assert report.gil_serialised is False
    assert report.gil == [(1, "FreeThreaded")]
