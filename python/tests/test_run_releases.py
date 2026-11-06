"""A run lets go of what it held when it ends (05 d.1, amended 2026-11-06).

A ``moruna.Function`` subclass's kernel kept the arena of the first run it was in, and a
reference cycle through the extension (the task holds its kernel, the kernel held the task) kept
the task and its kernel alive for good: every run's arena, a share of the budget that is 3 GB and
more on a laptop, stayed mapped for the life of the process, until a run was refused because the
process held more than its budget before it started.
"""

from __future__ import annotations

import gc
import re
import weakref

import pyarrow as pa

import moruna

SCHEMA = pa.schema([("n", pa.int64())])


class Rows(moruna.Source):
    """A few rows."""

    gives = SCHEMA

    def plan(self) -> list[moruna.Split]:
        return [moruna.Split(0, 100, 800)]

    def read(self, split_id: int, start: int, end: int) -> pa.RecordBatch:
        return pa.record_batch([pa.array(range(start, end), pa.int64())], schema=SCHEMA)


class Same(moruna.Function):
    """Gives each batch back."""

    takes = SCHEMA
    gives = SCHEMA

    def __call__(self, batch: pa.RecordBatch) -> pa.RecordBatch:
        return batch


class Drop(moruna.Sink):
    """Keeps nothing."""

    def write(self, batch: pa.RecordBatch) -> None:
        pass


def held_before(report: moruna.RunReport) -> int:
    """The bytes the process held before the run's arena existed, as the run's note says."""
    for note in report.notes:
        found = re.search(r"less the (\d+) bytes the process held before the arena existed", note)
        if found:
            return int(found.group(1))
    raise AssertionError(f"no arena note in {report.notes}")


def run_once() -> tuple[int, weakref.ref]:
    job = moruna.Job("released")
    task = Same()
    job.add(Drop(), inputs=job.add(task, inputs=job.add(Rows(), name="rows"), name="same"),
            name="out")
    return held_before(moruna.run(job, checkpoint=False)), weakref.ref(task)


def test_a_function_subclass_and_its_runs_arena_are_freed_when_the_run_ends() -> None:
    first, task = run_once()
    gc.collect()
    assert task() is None, "the task outlived its run and its job"
    held = [run_once()[0] for _ in range(3)]
    # Each arena is a share of the budget, gigabytes on a laptop: one kept is unmistakable.
    assert max(held) - first < 256 << 20, (
        f"the process held {first} bytes before the first run and {held} before the next three"
    )
