"""Stateful tasks on separate branches run at the same time (SC f.4, amended 2026-11-06).

A ``moruna.Function`` subclass is a stateful stage with one instance, and so is a stateful
object with ``instances=1``. Every such stage's instance had worker 0 as its home, so stages on
separate branches of one job ran one at a time: six branches of a 0.5 s function took 3.5 s. The
run's instances are now dealt to the workers in turn across stages.
"""

from __future__ import annotations

import time

import pyarrow as pa

import moruna

SCHEMA = pa.schema([("n", pa.int64())])
NAP = 0.5
BRANCHES = 6


class One(moruna.Source):
    """One row."""

    gives = SCHEMA

    def plan(self) -> list[moruna.Split]:
        return [moruna.Split(0, 1)]

    def read(self, split_id: int, start: int, end: int) -> pa.RecordBatch:
        return pa.record_batch([pa.array([1], pa.int64())], schema=SCHEMA)


class Nap(moruna.Function):
    """Waits, then gives the row a column of its own."""

    takes = SCHEMA

    def __init__(self, column: str):
        self.column = column

    def __call__(self, batch: pa.RecordBatch) -> pa.RecordBatch:
        time.sleep(NAP)
        return pa.RecordBatch.from_pydict({self.column: [self.column] * batch.num_rows})


class Drop(moruna.Sink):
    """Keeps nothing."""

    def write(self, batch: pa.RecordBatch) -> None:
        pass


def test_function_subclasses_on_separate_branches_run_at_the_same_time() -> None:
    job = moruna.Job("branches")
    one = job.add(One(), name="one")
    naps = [job.add(Nap(f"c{i}"), inputs=one, name=f"c{i}") for i in range(BRANCHES)]
    joined = job.add(moruna.std.join(), inputs=[one, *naps], name="join")
    job.add(Drop(), inputs=joined, name="out")
    started = time.monotonic()
    moruna.run(job, checkpoint=False, cpu=BRANCHES)
    took = time.monotonic() - started
    assert took < NAP * BRANCHES / 2, (
        f"{BRANCHES} branches of {NAP} s took {took:.2f} s: they ran one after another"
    )
