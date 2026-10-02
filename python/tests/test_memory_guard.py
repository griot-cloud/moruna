"""PY-T20 and PY-T21 (E13): the allocator guard through the package.

A function whose one call asks NumPy for more than the budget allows is refused at the request,
and the run ends with a budget error naming the function and the request instead of being killed
(PY-T20); with ``memory_guard=False`` the same request is not refused (the array is never
touched, so the run completes) and is still counted. Every Python stage of a run report, and
``moruna check``'s report, say what each function asked for, per source (PY-T21).

The runs are driven in a subprocess, as ``test_budget.py``'s are: the arena is sized from what
the process holds, and a refusal must leave the process alive, which only a process of its own
can show.
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys

import moruna

SCRIPT = """
import json, os, pathlib, sys
import numpy as np
import pyarrow as pa, pyarrow.parquet as pq
import moruna

d = pathlib.Path(os.environ["MORUNA_TEST_DIR"])
guard = os.environ["MORUNA_TEST_GUARD"] == "1"
policy = os.environ["MORUNA_TEST_POLICY"]
src, out = d / "in", d / "out"
pq.write_table(pa.table({"id": pa.array(range(20_000), pa.int64())}), src / "part-0.parquet",
               row_group_size=5_000)

@moruna.kernel(memory_guard=guard)
def greedy(batch):
    # Twice the budget in one request, never touched: the guard refuses it, nothing else would.
    np.empty(1024 * 1024 * 1024, dtype=np.uint8)
    return batch

result = {}
try:
    report = moruna.run(moruna.ParquetSource(f"file://{src}/part-0.parquet"), [greedy],
                        moruna.ParquetSink(f"file://{out}"), budget="512MiB", on_error=policy)
    result = {"exit": str(report.exit), "report": json.loads(report.to_json())}
except moruna.BudgetError as err:
    result = {"exit": "Budget", "kind": err.kind, "message": str(err),
              "diagnostic": {k: v for k, v in err.diagnostic.items() if k != "features"}}
    if err.report is not None:
        result["report"] = json.loads(err.report.to_json())
print(json.dumps(result))
"""


def _run(scratch: pathlib.Path, guard: bool, policy: str = "terminate") -> tuple[int, dict]:
    (scratch / "in").mkdir(exist_ok=True)
    (scratch / "out").mkdir(exist_ok=True)
    env = {k: v for k, v in os.environ.items() if k != "MORUNA_BUDGET"}
    env |= {
        "MORUNA_TEST_DIR": str(scratch),
        "MORUNA_TEST_GUARD": "1" if guard else "0",
        "MORUNA_TEST_POLICY": policy,
    }
    proc = subprocess.run(  # noqa: S603
        [sys.executable, "-c", SCRIPT], capture_output=True, text=True, env=env, check=False
    )
    assert proc.returncode >= 0, f"killed by signal {-proc.returncode}: {proc.stderr}"
    assert proc.returncode == 0, proc.stderr
    return proc.returncode, json.loads(proc.stdout.strip().splitlines()[-1])


def _alloc(result: dict) -> dict:
    stages = result["report"]["stages"]
    return stages[0]["alloc"]


def test_py_t20_refusal_end_to_end(scratch: pathlib.Path) -> None:
    (scratch / "on").mkdir()
    _, result = _run(scratch / "on", guard=True)
    assert result["exit"] == "Budget", result
    assert result["kind"] == "Refused"
    assert "kernel __main__.greedy" in result["message"], result["message"]
    assert "requested 1073741824 bytes" in result["message"], result["message"]
    diagnostic = result["diagnostic"]
    assert diagnostic["kernel"] == "__main__.greedy"
    assert diagnostic["requested"] == 1024**3
    assert diagnostic["in_use"] + diagnostic["requested"] > diagnostic["ceiling"]
    alloc = _alloc(result)
    assert alloc["refusal_on"] is True
    # At least the call that ended the run; workers already inside a call are refused too.
    assert alloc["numpy"]["refused"] >= 1


def test_py_t20_switch_off_is_not_refused_and_still_counted(scratch: pathlib.Path) -> None:
    (scratch / "off").mkdir()
    _, result = _run(scratch / "off", guard=False)
    assert result["exit"] == "Completed", result
    alloc = _alloc(result)
    assert alloc["refusal_on"] is False
    assert alloc["numpy"]["requested_bytes"] >= 1024**3
    assert alloc["numpy"]["refused"] == 0


def test_py_t20_refusal_skipped_under_skip(scratch: pathlib.Path) -> None:
    (scratch / "skip").mkdir()
    _, result = _run(scratch / "skip", guard=True, policy="skip")
    assert result["exit"] == "Completed", result
    stage = result["report"]["stages"][0]
    assert stage["skipped"] >= 1
    assert stage["alloc"]["numpy"]["refused"] == stage["skipped"] + stage["errors"]


def test_py_t21_alloc_reported(dataset: tuple[str, str, int]) -> None:
    import numpy as np
    import pyarrow as pa

    src, out, rows = dataset

    @moruna.kernel
    def mixed(batch):
        np.ones(1024 * 1024, dtype=np.uint8)
        return batch.append_column("twice", pa.array([v * 2 for v in batch.column("id").to_pylist()]))

    @moruna.kernel(memory_guard=False)
    def mixed_off(batch):
        return mixed.wrapped(batch)

    assert mixed.memory_guard is True
    assert mixed_off.memory_guard is False
    report = moruna.run(moruna.ParquetSource(src), [mixed], moruna.ParquetSink(out))
    alloc = report.stages[0]["alloc"]
    assert alloc == json.loads(report.to_json())["stages"][0]["alloc"]
    assert set(alloc) == {
        "refusal_on", "python", "numpy", "arrow",
        "peak_bytes", "outside_arrow_peak_bytes", "outside_arrow_fraction",
    }
    for source in ("python", "numpy", "arrow"):
        assert set(alloc[source]) == {
            "requested_bytes", "requests", "largest_request_bytes", "peak_bytes", "refused",
        }
    assert alloc["refusal_on"] is True
    assert alloc["numpy"]["requested_bytes"] >= 1024 * 1024
    assert alloc["python"]["requests"] > 0
    assert alloc["python"]["peak_bytes"] is None
    assert alloc["arrow"]["largest_request_bytes"] is None
    assert alloc["arrow"]["requested_bytes"] > 0
    assert 0.0 <= alloc["outside_arrow_fraction"] <= 1.0


def test_py_t21_memory_guard_is_not_in_the_fingerprint_and_check_reports_it() -> None:
    import numpy as np
    import pyarrow as pa

    def body(batch):
        np.ones(1024 * 1024, dtype=np.uint8)
        return batch

    on = moruna.kernel(input_schema={"n": pa.int64()}, output_schema={"n": pa.int64()})(body)
    off = moruna.kernel(
        input_schema={"n": pa.int64()}, output_schema={"n": pa.int64()}, memory_guard=False
    )(body)
    assert on.fingerprint == off.fingerprint
    for kernel, guarded in ((on, True), (off, False)):
        report = json.loads(moruna._core.check_kernel(kernel))
        assert report["memory_guard"] is guarded
        alloc = report["alloc"]
        assert alloc["refusal_on"] is False, "a check binds no gate, so nothing is refused"
        assert alloc["numpy"]["requested_bytes"] >= 1024 * 1024
        assert "allocations: refusal" in report["summary"]
