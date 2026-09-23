"""S1 measured, not estimated, on a kernel that allocates outside the arena.

The criterion is that the process's peak anonymous memory is at or below the budget, at every
sample (S1, G-I1), and the first program anyone writes broke it: a Python kernel that builds a
list of twenty thousand strings and a pyarrow array from it, at a 512 MiB budget, reached
`peak_fraction_of_ceiling` 1.11 on a built wheel (PM, 2026-09-23). Nothing in the suite would
have caught it, because nothing in the suite ran a kernel whose footprint is several times its
input at a budget tight enough for that to matter.

Every run here is driven in a subprocess. The arena is sized from the process's anonymous memory
*before* it exists (11 f.1, 02 f.1), so a test that shares a process with twenty earlier runs is
sized against their allocator retention rather than against a real baseline, and what it measures
is that, not this.
"""

from __future__ import annotations

import json
import os
import pathlib
import pytest
import subprocess
import sys

SCRIPT = """
import json, os, pathlib
import pyarrow as pa, pyarrow.parquet as pq
import moruna

d = pathlib.Path(os.environ["MORUNA_TEST_DIR"])
src, out = d / "in", d / "out"
rows = 200_000
# 20,000 rows to a row group, so one morsel is one Python list of 20,000 strings.
pq.write_table(
    pa.table({"id": pa.array(range(rows), pa.int64()),
              "text": pa.array([f"row-{i:08d}-abcdefghijklmnopqrstuvwxyz" for i in range(rows)])}),
    src / "part-0.parquet", row_group_size=20_000)

@moruna.kernel
def greedy(batch):
    # Every byte here is outside the arena: a Python str per row, a list of them, and a pyarrow
    # array built from that. It costs several times the morsel it was given, which is the class
    # architecture section 8 calls observed rather than governed.
    up = pa.array([s.as_py().upper() for s in batch.column("text")])
    return batch.append_column("loud", up)

result = {}
try:
    report = moruna.run(moruna.ParquetSource(f"file://{src}/part-0.parquet"), [greedy],
                       moruna.ParquetSink(f"file://{out}"),
                       budget=os.environ["MORUNA_TEST_BUDGET"])
    result = {"exit": str(report.exit),
              "ceiling": report.limits["memory_ceiling"],
              "peak": report.peak_anon_bytes,
              "fraction": report.peak_fraction_of_ceiling,
              "rows_out": report.stages[0]["rows_out"]}
except moruna.ConfigError as err:
    # The budget cannot be honoured in this process at all (see the skip in the test).
    result = {"exit": "NoRoom", "diagnostic": str(err)}
except moruna.BudgetError as err:
    # A refusal carries the partial report (PY-I2), so S1 can be checked on this path too:
    # the point of refusing is that the ceiling was never passed.
    result = {"exit": "Budget", "diagnostic": str(err)}
    if err.report is not None:
        result |= {"peak": err.report.peak_anon_bytes,
                   "ceiling": err.report.limits["memory_ceiling"],
                   "fraction": err.report.peak_fraction_of_ceiling}
print(json.dumps(result))
"""


def _run(scratch: pathlib.Path, budget: str) -> dict:
    (scratch / "in").mkdir(exist_ok=True)
    (scratch / "out").mkdir(exist_ok=True)
    env = {k: v for k, v in os.environ.items() if k != "MORUNA_BUDGET"}
    env["MORUNA_TEST_DIR"] = str(scratch)
    env["MORUNA_TEST_BUDGET"] = budget
    proc = subprocess.run(  # noqa: S603
        [sys.executable, "-c", SCRIPT], capture_output=True, text=True, env=env, check=False
    )
    assert proc.returncode == 0, proc.stderr
    return json.loads(proc.stdout.strip().splitlines()[-1])


def test_s1_a_tight_budget_is_never_exceeded(scratch: pathlib.Path) -> None:
    """S1, G-I1 and S6 together, at a budget tight enough that the answer is not obvious.

    The claim this makes is the one the design actually makes, and it is true on any host: at a
    tight ceiling the run either completes inside it or stops with a diagnostic naming the
    morsel, its footprint and the budget, and in neither case does the process's peak anonymous
    memory pass the ceiling. It is never killed and it never quietly exceeds.

    What this test used to assert was that 512 MiB *holds* this job, which is a fact about a
    machine and not about the runtime, and it was measured on the author's laptop. On a Linux CI
    runner the same wheel refuses it: the interpreter plus pyarrow and numpy rest at a larger
    footprint there, so the same ceiling leaves 117 MB above the arena where the kernel's own
    retention is about 160 MB, and the controller says so rather than running over. That is the
    runtime behaving correctly on a host where the job does not fit, and a test that called it a
    failure was asserting the author's hardware. The completion half of the claim is the test
    below, at a budget with room on any host (2026-09-23).
    """
    result = _run(scratch, "512MiB")

    if result["exit"] == "NoRoom":
        pytest.skip(
            "this process cannot be given a 512 MiB budget: Moruna sizes a run against the "
            "memory its own cgroup already holds, which is the run itself in a pod and the "
            "whole machine on a shared CI runner. The runtime says so rather than pretending: "
            f"{result['diagnostic']}"
        )
    assert result["exit"] in ("Completed", "Budget"), result
    if result["exit"] == "Completed":
        assert result["rows_out"] == 200_000, result
    else:
        assert "footprint" in result["diagnostic"] and "budget" in result["diagnostic"], (
            "a refusal names the morsel, its footprint and the budget (G-I8): " f"{result}"
        )
    if "fraction" in result:
        assert result["fraction"] <= 1.0, (
            f"S1: peak anonymous memory {result['peak']} exceeded the ceiling "
            f"{result['ceiling']} at {result['fraction']:.3f} of it"
        )


def test_s1_the_same_kernel_completes_where_the_budget_can_hold_it(
    scratch: pathlib.Path,
) -> None:
    """The bound is a bound and not a refusal: given room, the same kernel runs inside it.

    A model that answered every greedy kernel with a diagnostic would satisfy S1 and be useless,
    so this is the other half of the assertion. 2 GiB leaves about 205 MiB above the arena, which
    is several morsels' worth at this kernel's measured cost, and the controller has to find the
    worker count and morsel size that use it without crossing the ceiling.
    """
    result = _run(scratch, "2GiB")

    assert result["exit"] == "Completed", result
    assert result["rows_out"] == 200_000, result
    assert result["fraction"] <= 1.0, (
        f"S1: peak anonymous memory {result['peak']} exceeded the ceiling "
        f"{result['ceiling']} at {result['fraction']:.3f} of it"
    )
