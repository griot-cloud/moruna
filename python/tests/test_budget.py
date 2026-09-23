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


def test_s1_a_greedy_python_kernel_fits_inside_512_mib(scratch: pathlib.Path) -> None:
    """S1, G-I1: 512 MiB holds this job, so it completes inside 512 MiB.

    This is the measurement of 2026-09-23, to the row group: 200,000 rows at 20,000 to a group,
    a 512 MiB budget, `peak_fraction_of_ceiling` 1.11 and a completed run. The arena takes a
    share of `ceiling - baseline - reserve - declared kernel state` and touches every page of it
    at `new` (11 f.3, 02 f.1), so what this kernel has to allocate in is the rest of the
    allowance, which at this ceiling is about 328 MiB with a 160 MiB arena, and the runtime's own
    reader and writer buffers are in there with it.

    It is asserted as a completion and not as "fits or refuses", because both halves have been
    measured and 512 MiB holds the job: the run reaches 0.645 to 0.674 of the ceiling over eight
    runs. Two things stopped it, neither of them the budget. `ParquetSink` asked the arena for
    `row_group_bytes + 1 MiB of footer`, 129 MiB at the default row group, which 02 e.2 serves out
    of the 256 MiB class and which has to be free all at once: the sink reserved twice what it
    wanted and failed with `alloc 135266304 bytes in Host: budget 167772160 in use 15597568`. It
    now asks for one whole class with the footer inside it (08 f.1). And the controller charged
    the 86 MiB of allocator and interpreter retention this kernel holds regardless of the morsel
    to every morsel byte, so it refused the job with `footprint 186002119 exceeds budget
    152665344` where the real cost is about 85 MB; the fit now has a term for it (11 f.3). A
    refusal here is therefore a regression in one of those two and not a legitimate S6
    termination, so it fails.
    """
    result = _run(scratch, "512MiB")

    assert result["exit"] == "Completed", (
        "512 MiB holds this job (measured 2026-09-23 at 0.50 to 0.64 of the ceiling); a refusal "
        f"means the sink is over-reserving again (08 f.1): {result}"
    )
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
