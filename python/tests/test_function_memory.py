"""A run finds what ``moruna check`` measured of a Python function (system design 3A, item 6.3g).

``moruna check`` warms a function up and keeps what it measured against the function's own
fingerprint. A run of that function through ``moruna.run`` holds the same function, and costs it
under the same identity, so on the store the check wrote to it finds the measurement and sizes
the room above its arena from it. Before 6.3g it did not: the run keyed the function by its
image task's settings (a build key, an entry), which a check knows nothing of, so every Python
function was sized from a fixed 4.0 whatever its check had measured.
"""

from __future__ import annotations

import importlib.util
import textwrap
from pathlib import Path
from typing import Any

import pyarrow as pa
import pyarrow.parquet as pq

import moruna

CODE = '''
import numpy as np
import pyarrow as pa

import moruna

SCHEMA = pa.schema([pa.field("value", pa.int64(), nullable=False)])


@moruna.function(input_schema=SCHEMA, output_schema={"adds": {}})
def hungry(batch):
    """Holds twelve bytes of its own for every byte of its batch while it works."""
    held = np.ones(batch.nbytes * 12, dtype=np.uint8)
    assert int(held.sum()) == held.size
    return batch


@moruna.function(input_schema=SCHEMA, output_schema={"adds": {}})
def lean(batch):
    """Holds nothing of its own."""
    return batch
'''


def _module(path: Path, name: str) -> Any:
    """The module at ``path``, loaded under ``name``: the name is no part of a function's
    fingerprint, so a check of the file and a program that imports it agree."""
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _room(notes: list[str]) -> float:
    """The share of its allowance the run left above the arena, from its own note of how it
    sized the arena (``arena sized at A bytes: S of the B byte allowance``)."""
    note = next(n for n in notes if n.startswith("arena sized at "))
    arena = int(note.split("arena sized at ", 1)[1].split(" ", 1)[0])
    allowance = int(note.split(" of the ", 1)[1].split(" ", 1)[0])
    return 1.0 - arena / allowance


def test_a_run_finds_what_check_measured_of_a_python_function(
    scratch: Path, small_sink, staging
) -> None:  # noqa: ANN001
    path = scratch / "jobs.py"
    path.write_text(textwrap.dedent(CODE))
    store = str(scratch / "observations")

    # `moruna check` measures both, and keeps what it measured in the store.
    checked = moruna.check(str(path), store=store, budget=20.0)
    assert checked["exit"] == 0, checked
    by_name = {f["function"]: f for f in checked["functions"]}
    hungry_memory = by_name["hungry"]["warm_up"]["kernel"]["memory"]
    lean_memory = by_name["lean"]["warm_up"]["kernel"]["memory"]
    assert hungry_memory["per_input_byte"] > 8.0, hungry_memory
    assert lean_memory["per_input_byte"] < 1.0, lean_memory
    assert hungry_memory["units"] >= 3 and lean_memory["units"] >= 3

    # A program imports the same file, under another name, and runs each function over rows of
    # the schema it declares, on the store the check wrote to.
    jobs = _module(path, "the_program")
    src = scratch / "in.parquet"
    rows = 400_000
    pq.write_table(
        pa.table({"value": pa.array(range(rows), pa.int64())}, schema=jobs.SCHEMA),
        src,
        row_group_size=50_000,
    )

    def ran(function: Any, name: str, cost: Any) -> tuple[dict[str, Any], list[str]]:
        report = moruna.run(
            moruna.ParquetSource(f"file://{src}"),
            function,
            small_sink(f"file://{scratch}/out-{name}"),
            budget="2GiB",
            **({} if cost is None else {"cost": cost}),
            **staging,
        ).to_dict()
        task = next(t for t in report["tasks"] if t["role"] == "function")
        assert task["rows_out"] == rows
        return task, report["notes"]

    cost = moruna.CostSession(store)
    for name, memory in (("hungry", hungry_memory), ("lean", lean_memory)):
        task, notes = ran(getattr(jobs, name), name, cost)
        # The run costs the function under the key its check kept it under, to the last member.
        assert task["observation"]["key"] == by_name[name]["warm_up"]["key"], name
        measured = [n for n in notes if "its function's memory was measured, not assumed" in n]
        assert len(measured) == 1 and measured[0].startswith("stage 1:"), notes
        assert f"{memory['units']} units" in measured[0], measured[0]
        # And divides its budget from the measurement: a third of the allowance was the arena's
        # whatever the function, when nothing measured of it was found.
        if name == "hungry":
            assert _room(notes) > 0.8, notes
        else:
            assert _room(notes) < 0.5, notes

    # With no store the run has nothing measured of the function, and says nothing of it; the
    # function's key is the same, so a store given later holds one history of it.
    task, notes = ran(jobs.lean, "lean-alone", None)
    assert task["observation"]["key"] == by_name["lean"]["warm_up"]["key"]
    assert not any("was measured, not assumed" in n for n in notes)
    assert abs(_room(notes) - 2 / 3) < 0.02, notes
