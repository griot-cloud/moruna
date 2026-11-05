"""``moruna.Job`` and ``moruna.run(job)``: a run as a DAG of tasks (SDD 12 d.2, 22 d.4).

PY-T44 to PY-T47. Every component is real: in-process sources, functions and sinks are bound to
the tasks of the version 2 job the job is built into, and the real scheduler, placement engine
and controller run it. The suite runs on CPython 3.14t (``tools/quality/python_tests.sh``), so
the branches run with no GIL.
"""

from __future__ import annotations

import sys
import threading
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
    """An in-memory table, one split per thousand rows."""

    def __init__(self, table: pa.Table, split_rows: int = SPLIT_ROWS) -> None:
        self.batch = table.combine_chunks().to_batches()[0]
        self.split_rows = split_rows

    def plan(self) -> list[moruna.Split]:
        count = -(-self.batch.num_rows // self.split_rows)
        return [
            moruna.Split(i, min(self.split_rows, self.batch.num_rows - i * self.split_rows))
            for i in range(count)
        ]

    def schema(self) -> pa.Schema:
        return self.batch.schema

    def read(self, split_id: int, start: int, end: int) -> pa.RecordBatch:
        return self.batch.slice(split_id * self.split_rows + start, end - start)


class Rows(moruna.Sink):
    """Keeps every batch it is given, in the order it is given them."""

    def __init__(self) -> None:
        self.batches: list[pa.RecordBatch] = []

    def write(self, batch: pa.RecordBatch) -> None:
        self.batches.append(batch)

    def finish(self) -> None:
        pass

    def table(self) -> pa.Table:
        return pa.Table.from_batches(self.batches)


@moruna.function
def loud(batch: pa.RecordBatch) -> pa.RecordBatch:
    return batch.append_column("loud", pc.utf8_upper(batch.column("text")))


@moruna.function
def length(batch: pa.RecordBatch) -> pa.RecordBatch:
    return batch.append_column("length", pc.utf8_length(batch.column("text")))


@moruna.function
def length_only(batch: pa.RecordBatch) -> pa.RecordBatch:
    return pa.record_batch({"length": pc.utf8_length(batch.column("text"))})


def test_py_t44_a_fan_out_and_a_join_equal_the_chain(staging: dict[str, object]) -> None:
    """PY-T44: a source fanned out to two kernels and joined gives, row for row and in order,
    what the same kernels give in a chain; the chain is the graph with one path."""
    g = moruna.Job("graph")
    g.add(name="rows", task=TableSource(_table()))
    g.add(name="loud", task=loud, inputs="rows")
    g.add(name="length", task=length_only, inputs="rows")
    g.add(name="both", task=moruna.std.join(), inputs=["loud", "length"])
    joined = Rows()
    g.add(name="out", task=joined, inputs="both")
    report = moruna.run(g, ordered=True, **staging)
    assert report.exit["status"] == "completed", report

    chained = Rows()
    moruna.run(TableSource(_table()), [loud, length], chained, ordered=True, **staging)
    assert joined.table().schema.names == ["id", "text", "loud", "length"]
    assert joined.table().equals(chained.table())
    assert joined.table().num_rows == ROWS
    assert [stage["rows_in"] for stage in [t for t in report.tasks if t["role"] == "function"]] == [ROWS, ROWS, ROWS]


def test_py_t44_a_graph_of_several_sources_and_sinks(staging: dict[str, object]) -> None:
    """PY-T44: two sources unioned and fanned out to two sinks; each object is bound to its
    own task."""
    first, second = _table().slice(0, 4_000), _table().slice(4_000)
    g = moruna.Job("graph")
    g.add(name="first", task=TableSource(first))
    g.add(name="second", task=TableSource(second))
    g.add(name="all", task=moruna.std.union(), inputs=["first", "second"])
    g.add(name="loud", task=loud, inputs="all")
    every, upper = Rows(), Rows()
    g.add(name="every", task=every, inputs="all")
    g.add(name="upper", task=upper, inputs="loud")
    moruna.run(g, **staging)
    assert sorted(every.table().column("id").to_pylist()) == list(range(ROWS))
    assert sorted(upper.table().column("loud").to_pylist()) == sorted(
        f"ROW {i}" for i in range(ROWS)
    )


def test_py_t45_a_graph_is_refused_by_name_before_it_starts(staging: dict[str, object]) -> None:
    """PY-T45: an unknown name, a cycle and an output no task reads are each refused with
    `ConfigError` naming the task, and nothing is read."""
    source = TableSource(_table())
    unknown = moruna.Job("graph")
    unknown.add(name="rows", task=source)
    unknown.add(name="loud", task=loud, inputs="rwos")
    unknown.add(name="out", task=Rows(), inputs="loud")
    with pytest.raises(moruna.ConfigError) as caught:
        moruna.run(unknown, **staging)
    said = str(caught.value)
    assert "no task `rwos`" in said, said
    assert "`rows`'s output `out`" in said, "every refusal is named, not the first alone"

    cycle = moruna.Job("graph")
    cycle.add(name="rows", task=source)
    cycle.add(name="a", task=moruna.std.union(), inputs=["rows", "b"])
    cycle.add(name="b", task=loud, inputs="a")
    cycle.add(name="out", task=Rows(), inputs="b")
    with pytest.raises(moruna.ConfigError, match="cycle through `a`, `b`"):
        moruna.run(cycle, **staging)

    unread = moruna.Job("graph")
    unread.add(name="rows", task=source)
    unread.add(name="loud", task=loud, inputs="rows")
    unread.add(name="idle", task=length, inputs="rows")
    unread.add(name="out", task=Rows(), inputs="loud")
    with pytest.raises(moruna.ConfigError, match="no edge reads `idle`'s output `out`"):
        moruna.run(unread, **staging)

    misjoined = moruna.Job("graph")
    misjoined.add(name="a", task=TableSource(_table()))
    misjoined.add(name="b", task=TableSource(_table()))
    misjoined.add(name="both", task=moruna.std.join(), inputs=["a", "b"])
    misjoined.add(name="out", task=Rows(), inputs="both")
    with pytest.raises(moruna.ConfigError, match="`both` joins its inputs by morsel"):
        moruna.run(misjoined, **staging)


def test_py_t45_the_join_refuses_what_is_not_one_morsel(staging: dict[str, object]) -> None:
    """PY-T45: a column two branches give, and a branch that does not keep its rows, are
    refused naming the ports."""
    g = moruna.Job("graph")
    g.add(name="rows", task=TableSource(_table()))
    g.add(name="loud", task=loud, inputs="rows")
    g.add(name="length", task=length, inputs="rows")
    g.add(name="both", task=moruna.std.join(), inputs=["loud", "length"])
    g.add(name="out", task=Rows(), inputs="both")
    with pytest.raises(moruna.MorunaError, match="the column `id` on its port 0 and again"):
        moruna.run(g, **staging)

    @moruna.function
    def half(batch: pa.RecordBatch) -> pa.RecordBatch:
        return pa.record_batch({"n": pa.array(range(batch.num_rows // 2), pa.int64())})

    g = moruna.Job("graph")
    g.add(name="rows", task=TableSource(_table()))
    g.add(name="half", task=half, inputs="rows")
    g.add(name="both", task=moruna.std.join(), inputs=["rows", "half"])
    g.add(name="out", task=Rows(), inputs="both")
    with pytest.raises(moruna.FunctionError, match="rows on its port 1 and 1000 on its port 0"):
        moruna.run(g, **staging)


def test_py_t45_the_job_checks_its_arguments() -> None:
    """PY-T45: a name twice, a name that is not one, a source given an input, a function given
    none and an input that is not in the job are refused when added; a run is a job or a
    chain, not both."""
    g = moruna.Job("graph")
    rows = TableSource(_table())
    assert g.add(rows, name="rows") is rows
    with pytest.raises(ValueError, match="a task named 'rows' already"):
        g.add(loud, inputs="rows", name="rows")
    with pytest.raises(ValueError, match="a task's name"):
        g.add(loud, inputs="rows", name="no spaces")
    with pytest.raises(ValueError, match="is in the job already"):
        g.add(rows, name="again")
    with pytest.raises(ValueError, match="takes no input"):
        g.add(TableSource(_table()), inputs=rows, name="second")
    with pytest.raises(ValueError, match="takes at least one input"):
        g.add(loud, name="alone")
    with pytest.raises(ValueError, match="add it first"):
        g.add(loud, inputs=Rows(), name="loud")
    with pytest.raises(TypeError, match="not int"):
        g.add(3)
    with pytest.raises(TypeError, match="a moruna.Job, or a source, functions and a sink"):
        moruna.run(g, [], Rows())
    with pytest.raises(TypeError, match="a moruna.Job, or a source, functions and a sink"):
        moruna.run(TableSource(_table()))
    assert [name for name, _, _, _ in g.tasks] == ["rows"]
    assert "rows  source" in repr(g)


class Sleeper:
    """Blocks for `pause` seconds on every batch, and records when each call ran."""

    def __init__(self, pause: float, calls: list[tuple[str, float, float]], name: str) -> None:
        self.pause, self.calls, self.name = pause, calls, name
        self.lock = threading.Lock()

    def __call__(self, batch: pa.RecordBatch) -> pa.RecordBatch:
        started = time.monotonic()
        time.sleep(self.pause)
        with self.lock:
            self.calls.append((self.name, started, time.monotonic()))
        return pa.record_batch({self.name: pa.array([1] * batch.num_rows, pa.int64())})


def test_py_t46_ready_branches_run_at_once() -> None:
    """PY-T46: a fan-out to four kernels that each block for T on the one batch takes about T
    for the batch, not 4T: the four calls overlap, every one running while every other is.

    The bound is generous so load cannot flake it: the four calls must share an instant (the
    latest start before the earliest end), and the span from the first start to the last end
    must be under 2.5 T, where a serial run is at least 4 T."""
    branches, pause = 4, 1.0
    calls: list[tuple[str, float, float]] = []
    batch = pa.record_batch({"id": pa.array(range(100), pa.int64())})
    g = moruna.Job("graph")
    g.add(name="rows", task=moruna.IteratorSource([batch], schema=batch.schema))
    names = [f"b{i}" for i in range(branches)]
    for name in names:
        g.add(name=name, task=moruna.function(Sleeper(pause, calls, name)), inputs="rows")
    g.add(name="all", task=moruna.std.join(), inputs=names)
    out = Rows()
    g.add(name="out", task=out, inputs="all")
    moruna.run(g, cpu=float(branches + 2), checkpoint=False)

    assert sorted(name for name, _, _ in calls) == names, calls
    starts = [s for _, s, _ in calls]
    ends = [e for _, _, e in calls]
    span = max(ends) - min(starts)
    assert max(starts) < min(ends), f"the branches did not overlap: {calls}"
    assert span < 2.5 * pause, f"{branches} branches of {pause}s took {span:.2f}s"
    assert out.table().column_names == names
    print(f"PY-T46: {branches} branches of {pause}s each, the batch took {span:.3f}s; "
          f"GIL enabled: {sys._is_gil_enabled()}")  # noqa: SLF001


class Count:
    """A whole-run kernel: counts every row it is given, gives nothing per batch, and gives
    the count once its input has ended."""

    def __init__(self) -> None:
        self.finished: list[int] = []
        self.lock = threading.Lock()

    def setup(self, ctx: object) -> dict[str, int]:
        return {"instance": ctx.instance, "rows": 0}  # type: ignore[attr-defined]

    def __call__(self, state: dict[str, int], batch: pa.RecordBatch) -> pa.RecordBatch:
        state["rows"] += batch.num_rows
        return pa.record_batch(
            {"instance": pa.array([], pa.int64()), "rows": pa.array([], pa.int64())}
        )

    def finish(self, state: dict[str, int]) -> pa.RecordBatch:
        with self.lock:
            self.finished.append(state["instance"])
        return pa.record_batch(
            {
                "instance": pa.array([state["instance"]], pa.int64()),
                "rows": pa.array([state["rows"]], pa.int64()),
            }
        )


def test_py_t47_finish_gives_the_whole_run(staging: dict[str, object]) -> None:
    """PY-T47: a stateful kernel's `finish` is called once at the end of its input, and the
    batch it returns reaches the sink: with `instances=1` it saw every row."""
    count = Count()
    sink = Rows()
    report = moruna.run(
        TableSource(_table()), moruna.function(count, stateful=True), sink, **staging
    )
    assert report.exit["status"] == "completed", report
    assert count.finished == [0], "finish once, for the one instance"
    table = sink.table()
    assert table.num_rows == 1
    assert table.column("rows").to_pylist() == [ROWS]


def test_py_t47_every_instance_finishes(staging: dict[str, object]) -> None:
    """PY-T47: with `instances=2` each instance's `finish` is called, each on what that
    instance was given, and between them they saw every row."""
    count = Count()
    sink = Rows()
    moruna.run(
        TableSource(_table()),
        moruna.function(count, stateful=True, instances=2),
        sink,
        **staging,
    )
    assert sorted(count.finished) == [0, 1], "each instance's finish, once"
    table = sink.table()
    assert sorted(table.column("instance").to_pylist()) == [0, 1]
    assert sum(table.column("rows").to_pylist()) == ROWS


def test_py_t47_finish_feeds_the_next_task(staging: dict[str, object]) -> None:
    """PY-T47: what `finish` gives goes downstream like any batch, through a later kernel of
    a graph, and a `finish` that returns None gives nothing."""

    class Nothing:
        def setup(self, ctx: object) -> None:
            return None

        def __call__(self, state: None, batch: pa.RecordBatch) -> pa.RecordBatch:
            return batch

        def finish(self, state: None) -> None:
            return None

    @moruna.function
    def doubled(batch: pa.RecordBatch) -> pa.RecordBatch:
        return batch.append_column("twice", pc.multiply(batch.column("rows"), 2))

    g = moruna.Job("graph")
    g.add(name="rows", task=TableSource(_table()))
    g.add(name="same", task=moruna.function(Nothing(), stateful=True), inputs="rows")
    g.add(name="count", task=moruna.function(Count(), stateful=True), inputs="same")
    g.add(name="doubled", task=doubled, inputs="count")
    sink = Rows()
    g.add(name="out", task=sink, inputs="doubled")
    moruna.run(g, **staging)
    assert sink.table().column("twice").to_pylist() == [2 * ROWS]


def test_py_t47_an_error_in_finish_ends_the_run_naming_the_kernel(
    staging: dict[str, object],
) -> None:
    """PY-T47: an exception raised by `finish` ends the run with `FunctionError`, naming the
    kernel and `finish`, and the sink is not finished."""

    class Broken(Count):
        def finish(self, state: dict[str, int]) -> pa.RecordBatch:
            raise ValueError("the total did not add up")

    class Watched(Rows):
        finished = 0

        def finish(self) -> None:
            Watched.finished += 1

    with pytest.raises(moruna.FunctionError) as caught:
        moruna.run(
            TableSource(_table()), moruna.function(Broken(), stateful=True), Watched(), **staging
        )
    said = str(caught.value)
    assert "Broken.finish" in said and "ValueError: the total did not add up" in said, said
    assert Watched.finished == 0
