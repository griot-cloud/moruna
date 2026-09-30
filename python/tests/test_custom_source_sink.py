"""A source and a sink of the user's own: ``moruna.Source``, ``moruna.Sink`` and ``moruna.Split``.

SDD 07 e.6 and f.7, 08 f.10 and e.6, 12 d.2. Every component between the user's two classes is
real: the source is read through a Python kernel by the real scheduler, placement engine and
controller into the sink, and the rows that reach the sink are compared with the table the
source was made from, row for row. The kill test SIGKILLs a run in a subprocess and resumes it in
a fresh one, and the sink ends up holding every row exactly once.

Budgets come from ``conftest.py``: ``MORUNA_BUDGET`` is set for the whole suite from what a test
host carries, never a figure in a test.
"""

from __future__ import annotations

import json
import os
import pathlib
import signal
import subprocess
import sys
import time

import pyarrow as pa
import pyarrow.compute as pc
import pytest

import moruna

ROWS = 10_000
SPLIT_ROWS = 1_000


def _table() -> pa.Table:
    return pa.table(
        {
            "id": pa.array(range(ROWS), pa.int64()),
            "text": pa.array([f"row {i}" for i in range(ROWS)], pa.string()),
        }
    )


class TableSource(moruna.Source):
    """An in-memory table, one split per thousand rows; ``read`` slices it."""

    def __init__(self, table: pa.Table, split_rows: int = SPLIT_ROWS) -> None:
        self.batch = table.combine_chunks().to_batches()[0]
        self.split_rows = split_rows
        self.reads: list[tuple[int, int, int]] = []

    def plan(self) -> list[moruna.Split]:
        count = -(-self.batch.num_rows // self.split_rows)
        return [
            moruna.Split(i, min(self.split_rows, self.batch.num_rows - i * self.split_rows))
            for i in range(count)
        ]

    def schema(self) -> pa.Schema:
        return self.batch.schema

    def read(self, split_id: int, start: int, end: int) -> pa.RecordBatch:
        self.reads.append((split_id, start, end))
        return self.batch.slice(split_id * self.split_rows + start, end - start)


class Stream(TableSource):
    """The same table, declared not repeatable."""

    repeatable = False


class CollectSink(moruna.Sink):
    """Keeps the ids and loud texts it is given; no checkpoint, so not resumable."""

    def __init__(self) -> None:
        self.ids: list[int] = []
        self.loud: list[str] = []
        self.batches = 0
        self.finished = 0

    def write(self, batch: pa.RecordBatch) -> None:
        self.batches += 1
        self.ids.extend(batch.column("id").to_pylist())
        if "loud" in batch.schema.names:
            self.loud.extend(batch.column("loud").to_pylist())

    def finish(self) -> None:
        self.finished += 1


@moruna.kernel
def shout(batch: pa.RecordBatch) -> pa.RecordBatch:
    return batch.append_column("loud", pc.utf8_upper(batch.column("text")))


def test_a_source_through_a_kernel_into_a_sink(staging: dict[str, object]) -> None:
    """07 e.6, 08 f.10: every row of the table reaches the sink once, through the kernel."""
    source = TableSource(_table())
    sink = CollectSink()
    report = moruna.run(source, shout, sink, **staging)

    assert report.exit == "Completed", report
    assert sorted(sink.ids) == list(range(ROWS)), "exactly the table's rows, each once"
    assert sorted(sink.loud) == sorted(f"ROW {i}" for i in range(ROWS))
    assert sink.finished == 1, "finish once, after the last write"
    assert report.stages[0]["rows_in"] == ROWS
    # Every read asked for a range inside its split, and the reads covered the table.
    assert all(0 <= s <= e <= SPLIT_ROWS for _, s, e in source.reads)
    # A sink that does not checkpoint says so in the report (12 f.7).
    assert any("the sink does not checkpoint" in n for n in report.notes), report.notes


def test_ordered_delivery_reaches_write_in_order(staging: dict[str, object]) -> None:
    """08 SI-I5 through the real scheduler: with ``ordered=True`` the batches arrive in order."""
    sink = CollectSink()
    moruna.run(TableSource(_table()), [], sink, ordered=True, **staging)
    assert sink.ids == list(range(ROWS))


def test_a_source_that_is_not_repeatable_turns_eviction_and_resume_off(
    staging: dict[str, object],
) -> None:
    """07 SO-I8: ``repeatable = False`` is treated as an iterator source is."""
    sink = CollectSink()
    report = moruna.run(Stream(_table()), [], sink, **staging)
    assert sorted(sink.ids) == list(range(ROWS))
    assert any("Q0 staged" in n for n in report.notes), report.notes


def test_split_is_checked_and_readable() -> None:
    split = moruna.Split(3, 1_000, bytes=8_000)
    assert (split.id, split.rows, split.bytes) == (3, 1_000, 8_000)
    assert moruna.Split(1, 2).bytes is None
    assert moruna.Split(1, 2) == moruna.Split(1, 2, None)
    assert repr(moruna.Split(1, 2)) == "Split(id=1, rows=2)"
    with pytest.raises(ValueError, match="rows"):
        moruna.Split(0, -1)
    with pytest.raises(ValueError, match="id"):
        moruna.Split("a", 1)
    with pytest.raises(AttributeError):
        split.rows = 5  # type: ignore[misc]


def test_a_subclass_missing_a_method_is_refused_before_anything_starts() -> None:
    class NoRead(moruna.Source):
        def plan(self) -> list[moruna.Split]:
            return []

    class CheckpointsButCannotRestore(moruna.Sink):
        def write(self, batch: pa.RecordBatch) -> None:
            pass

        def checkpoint(self) -> bytes:
            return b""

    with pytest.raises(TypeError, match=r"must define read\(\)"):
        moruna.run(NoRead(), [], CollectSink())
    with pytest.raises(TypeError, match=r"write\(batch\)"):
        moruna.run(TableSource(_table()), [], moruna.Sink())
    with pytest.raises(TypeError, match=r"not restore\(state\)"):
        moruna.run(TableSource(_table()), [], CheckpointsButCannotRestore())
    assert moruna.Source().schema() is None
    assert moruna.Sink().checkpoint() is None


def test_errors_in_user_code_carry_the_python_message(staging: dict[str, object]) -> None:
    """07 h, 08 h: an exception in ``read`` or ``write`` fails the run with its message."""

    class Broken(TableSource):
        def read(self, split_id: int, start: int, end: int) -> pa.RecordBatch:
            if split_id == 3:
                raise ConnectionError("the upstream went away")
            return super().read(split_id, start, end)

    class Short(TableSource):
        def read(self, split_id: int, start: int, end: int) -> pa.RecordBatch:
            batch = super().read(split_id, start, end)
            return batch.slice(0, max(batch.num_rows - 1, 0)) if split_id == 2 else batch

    class Full(CollectSink):
        def write(self, batch: pa.RecordBatch) -> None:
            if self.batches >= 2:
                raise OSError("disk full")
            super().write(batch)

    class BadPlan(TableSource):
        def plan(self) -> list[moruna.Split]:
            raise LookupError("no such table")

    with pytest.raises(moruna.IoError, match="ConnectionError: the upstream went away"):
        moruna.run(Broken(_table()), [], CollectSink(), **staging)
    with pytest.raises(moruna.IoError, match="it must return exactly"):
        moruna.run(Short(_table()), [], CollectSink(), **staging)
    full = Full()
    with pytest.raises(moruna.IoError, match="OSError: disk full"):
        moruna.run(TableSource(_table()), [], full, **staging)
    assert full.finished == 0, "finish is not called on a failed run"
    with pytest.raises(moruna.PlanError, match="LookupError"):
        moruna.run(BadPlan(_table()), [], CollectSink(), **staging)


CHILD = r"""
import json, os, pathlib, sys, time
import pyarrow as pa, pyarrow.ipc as ipc
import moruna

SPLITS, SPLIT_ROWS = 40, 500
out, staging, mode = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]


class Numbers(moruna.Source):
    def plan(self):
        return [moruna.Split(i, SPLIT_ROWS) for i in range(SPLITS)]

    def schema(self):
        return pa.schema([("id", pa.int64())])

    def read(self, split_id, start, end):
        base = split_id * SPLIT_ROWS
        return pa.record_batch({"id": pa.array(range(base + start, base + end), pa.int64())})


class Files(moruna.Sink):
    # One Arrow IPC file per batch. The checkpoint is the list of files written; restore
    # removes every file the list does not name, which is what a process killed after the
    # checkpoint left behind.
    def __init__(self, directory):
        self.dir = directory
        self.names = []

    def write(self, batch):
        name = f"part-{len(self.names):05d}.arrow"
        tmp = self.dir / (name + ".tmp")
        with ipc.new_file(str(tmp), batch.schema) as writer:
            writer.write_batch(batch)
        os.replace(tmp, self.dir / name)
        self.names.append(name)
        time.sleep(0.05)

    def checkpoint(self):
        return json.dumps(self.names).encode()

    def restore(self, state):
        self.names = json.loads(state)
        keep = set(self.names)
        for path in self.dir.iterdir():
            if path.name not in keep:
                path.unlink()


report = moruna.run(Numbers(), [], Files(out), staging_dir=staging, staging_limit="2GiB",
                   checkpoint_interval=0.5, resume=None if mode == "fresh" else "auto")
print(json.dumps({"exit": report.exit, "resumed": report.resumed, "run_id": report.run_id}))
"""


def test_a_killed_run_resumes_into_a_sink_that_checkpoints(scratch: pathlib.Path) -> None:
    """08 e.6, f.10, S17: SIGKILL mid-run, resume in a fresh process, every row exactly once."""
    out = scratch / "out"
    staging = scratch / "staging"
    out.mkdir()
    staging.mkdir()
    script = scratch / "child.py"
    script.write_text(CHILD)
    run = [sys.executable, str(script), str(out), str(staging)]

    child = subprocess.Popen(  # noqa: S603
        [*run, "fresh"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
    )
    try:
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline and child.poll() is None:
            written = len(list(out.glob("part-*.arrow")))
            if written >= 12 and any(staging.rglob("manifest.json")):
                break
            time.sleep(0.02)
        assert child.poll() is None, f"the run ended before it could be killed: {child.stderr}"
        child.send_signal(signal.SIGKILL)
        child.wait(timeout=30)
    finally:
        if child.poll() is None:
            child.kill()
    assert child.returncode == -signal.SIGKILL
    before = len(list(out.glob("part-*.arrow")))
    assert 0 < before < 40, before

    resumed = subprocess.run(  # noqa: S603
        [*run, "resume"], capture_output=True, text=True, check=False, timeout=300
    )
    assert resumed.returncode == 0, resumed.stderr
    result = json.loads(resumed.stdout.strip().splitlines()[-1])
    assert result["exit"] == "Completed"
    assert result["resumed"] is True

    ids: list[int] = []
    for path in sorted(out.iterdir()):
        assert path.suffix == ".arrow", f"a file restore should have removed: {path.name}"
        with pa.ipc.open_file(str(path)) as reader:
            ids.extend(reader.read_all().column("id").to_pylist())
    assert sorted(ids) == list(range(40 * 500)), "every row exactly once"
    assert os.path.isdir(staging)
