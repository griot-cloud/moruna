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
import subprocess
import sys

SCRIPT = """
import json, os, pathlib
import pyarrow as pa, pyarrow.parquet as pq
import amoru

d = pathlib.Path(os.environ["AMORU_TEST_DIR"])
src, out = d / "in", d / "out"
rows = 200_000
# 20,000 rows to a row group, so one morsel is one Python list of 20,000 strings.
pq.write_table(
    pa.table({"id": pa.array(range(rows), pa.int64()),
              "text": pa.array([f"row-{i:08d}-abcdefghijklmnopqrstuvwxyz" for i in range(rows)])}),
    src / "part-0.parquet", row_group_size=20_000)

@amoru.kernel
def greedy(batch):
    # Every byte here is outside the arena: a Python str per row, a list of them, and a pyarrow
    # array built from that. It costs several times the morsel it was given, which is the class
    # architecture section 8 calls observed rather than governed.
    up = pa.array([s.as_py().upper() for s in batch.column("text")])
    return batch.append_column("loud", up)

result = {}
try:
    report = amoru.run(amoru.ParquetSource(f"file://{src}/part-0.parquet"), [greedy],
                       amoru.ParquetSink(f"file://{out}"),
                       budget=os.environ["AMORU_TEST_BUDGET"])
    result = {"exit": str(report.exit),
              "ceiling": report.limits["memory_ceiling"],
              "peak": report.peak_anon_bytes,
              "fraction": report.peak_fraction_of_ceiling,
              "rows_out": report.stages[0]["rows_out"]}
except amoru.BudgetError as err:
    result = {"exit": "Budget", "diagnostic": str(err)}
print(json.dumps(result))
"""


def _run(scratch: pathlib.Path, budget: str) -> dict:
    (scratch / "in").mkdir(exist_ok=True)
    (scratch / "out").mkdir(exist_ok=True)
    env = {k: v for k, v in os.environ.items() if k != "AMORU_BUDGET"}
    env["AMORU_TEST_DIR"] = str(scratch)
    env["AMORU_TEST_BUDGET"] = budget
    proc = subprocess.run(  # noqa: S603
        [sys.executable, "-c", SCRIPT], capture_output=True, text=True, env=env, check=False
    )
    assert proc.returncode == 0, proc.stderr
    return json.loads(proc.stdout.strip().splitlines()[-1])


def test_s1_a_greedy_python_kernel_never_exceeds_its_budget(scratch: pathlib.Path) -> None:
    """S1, G-I1, G-I8: at the budget that broke it, the run fits or it fails.

    This is the measurement of 2026-09-23, to the row group: 200,000 rows at 20,000 to a group,
    a 512 MiB budget, `peak_fraction_of_ceiling` 1.11 and a completed run. The arena takes
    `ceiling - baseline - reserve` and touches every page of it at `new`, so what this kernel has
    to allocate in is the reserve, about 51 MiB, and the runtime's own reader and writer buffers
    are in there with it. There may be no worker count and no morsel size that fits, and if there
    is not, that is a refusal with a diagnostic and not a run that finishes over the ceiling.
    """
    result = _run(scratch, "512MiB")

    if result["exit"] == "Budget":
        # A legitimate termination under S6 and G-I8: shrinking to `morsel.min_bytes` on one
        # worker still would not fit. It has to name the morsel, its stage, its footprint and the
        # budget, and the two figures it compares have to be the same kind of number.
        diagnostic = result["diagnostic"]
        assert diagnostic.startswith("budget: morsel "), diagnostic
        assert "stage 1" in diagnostic, diagnostic
        footprint = int(diagnostic.split("footprint ")[1].split(" ")[0])
        budget = int(diagnostic.split("exceeds budget ")[1].split(" ")[0])
        assert footprint > budget, diagnostic
        return

    assert result["exit"] == "Completed", result
    assert result["rows_out"] == 200_000, result
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
