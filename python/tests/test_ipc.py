"""Arrow IPC files end to end from Python: the files `moruna.ArrowIpcSink` writes are files
pyarrow opens (KA11, 08 e.3, SI-T27), and `moruna.ArrowIpcSource` reads files pyarrow wrote
(KA9, 07 SO-T34 onward).

pyarrow is Arrow C++, which holds an IPC file to the format where arrow-rs is lenient: every
block of the footer has an offset, a metadata length and a body length that are multiples of 8,
and the footer's body length is the one the message declares. A file that arrow-rs reads in
full and that Arrow C++ refuses was the report behind KA11, and `pyarrow.ipc.open_file` is the
reader that proves it fixed.
"""

from __future__ import annotations

import pathlib

import pyarrow as pa
import pyarrow.ipc
import pyarrow.parquet as pq
import pytest

import moruna


def budget() -> int:
    """A budget with room above what this process already holds (see test_vortex.budget)."""
    held = moruna.inspect_host()["anon_bytes"]
    return int((held + (512 << 20)) / 0.9)


def make_table(rows: int) -> pa.Table:
    """An `id`, a string with an empty one in it, a nullable float and a list of ints: buffers
    that are not a multiple of 8 bytes for most row counts, which is what the framing padded."""
    return pa.table(
        {
            "id": pa.array(range(rows), pa.int64()),
            "text": pa.array([f"row {i}" if i % 5 else "" for i in range(rows)]),
            "score": pa.array([None if i % 7 == 0 else i * 0.5 for i in range(rows)], pa.float32()),
            "tags": pa.array([list(range(i % 4)) for i in range(rows)], pa.list_(pa.int32())),
        }
    )


def write_through_the_sink(
    scratch: pathlib.Path, table: pa.Table, *, ordered: bool, row_group_size: int
) -> pathlib.Path:
    src = scratch / "in"
    src.mkdir()
    pq.write_table(table, src / "part-0.parquet", row_group_size=row_group_size)
    out = scratch / "ipc"
    report = moruna.run(
        moruna.ParquetSource(f"file://{src}/part-0.parquet"),
        [],
        moruna.ArrowIpcSink(str(out)),
        budget=budget(),
        ordered=ordered,
    )
    assert report.exit == "Completed", report.notes
    return out


def part_files(out: pathlib.Path) -> list[pathlib.Path]:
    parts = sorted(out.glob("part-*.arrow"))
    assert parts, sorted(p.name for p in out.iterdir())
    return parts


def read_with_pyarrow(out: pathlib.Path) -> tuple[pa.Table, int]:
    """Every part file opened the way a user opens one, batch by batch, and the batch count."""
    batches: list[pa.RecordBatch] = []
    for part in part_files(out):
        reader = pa.ipc.open_file(part)
        for i in range(reader.num_record_batches):
            batches.append(reader.get_batch(i))
        mapped = pa.ipc.open_file(pa.memory_map(str(part)))
        assert mapped.read_all().num_rows == sum(
            reader.get_batch(i).num_rows for i in range(reader.num_record_batches)
        )
    table = pa.Table.from_batches(batches)
    table.validate(full=True)
    return table, len(batches)


@pytest.mark.parametrize("ordered", [True, False])
@pytest.mark.parametrize(
    ("rows", "row_group_size", "at_least"),
    [(1_001, 1_001, 1), (1_000_000, 100_000, 5)],
    ids=["one-batch", "many-batches"],
)
def test_si_t27_pyarrow_opens_the_sinks_files(
    scratch: pathlib.Path, rows: int, row_group_size: int, at_least: int, ordered: bool
) -> None:
    table = make_table(rows)
    out = write_through_the_sink(scratch, table, ordered=ordered, row_group_size=row_group_size)
    back, batches = read_with_pyarrow(out)
    assert batches >= at_least, batches
    assert back.schema.equals(table.schema), back.schema
    assert back.num_rows == rows
    if not ordered:
        back = back.sort_by("id")
    assert back.equals(table)


def write_with_pyarrow(path: pathlib.Path, table: pa.Table, *, batch_rows: int) -> None:
    """A file written by pyarrow itself: the schema message at the head, 8-aligned buffers."""
    with pa.OSFile(str(path), "wb") as sink, pa.ipc.new_file(sink, table.schema) as writer:
        for batch in table.to_batches(max_chunksize=batch_rows):
            writer.write_batch(batch)


def run_to_parquet(source: moruna.ArrowIpcSource, out: pathlib.Path) -> pa.Table:
    report = moruna.run(
        source,
        [],
        moruna.ParquetSink(f"file://{out}", row_group_bytes="16MiB", file_bytes="64MiB"),
        budget=budget(),
        ordered=True,
    )
    assert report.exit == "Completed", report.notes
    return pq.read_table(str(out)).sort_by("id")


def test_so_t38_a_file_pyarrow_wrote_is_read_in_full(scratch: pathlib.Path) -> None:
    table = make_table(25_000)
    src = scratch / "in"
    src.mkdir()
    write_with_pyarrow(src / "a.arrow", table, batch_rows=4_000)

    back = run_to_parquet(moruna.ArrowIpcSource(str(src / "a.arrow")), scratch / "out")
    assert back.equals(table)

    # Splits cut across the file's batches, and a URL spelled `file://`.
    again = run_to_parquet(
        moruna.ArrowIpcSource(f"file://{src}/a.arrow", split_rows=9_000), scratch / "grouped"
    )
    assert again.equals(table)


def test_so_t38_a_directory_and_a_projection(scratch: pathlib.Path) -> None:
    table = make_table(10_000)
    src = scratch / "in"
    src.mkdir()
    write_with_pyarrow(src / "part-0.arrow", table.slice(0, 6_000), batch_rows=1_500)
    write_with_pyarrow(src / "part-1.arrow", table.slice(6_000), batch_rows=1_000)
    (src / "notes.txt").write_text("not an arrow file")

    back = run_to_parquet(moruna.ArrowIpcSource(str(src)), scratch / "out")
    assert back.equals(table)

    projected = run_to_parquet(
        moruna.ArrowIpcSource([str(src)], columns=["score", "id"]), scratch / "projected"
    )
    assert projected.column_names == ["id", "score"]
    assert projected.equals(table.select(["id", "score"]))


def test_so_t38_the_sinks_own_files_read_back(scratch: pathlib.Path) -> None:
    """The sink's page-aligned files go through the source and come out equal: the two ends of
    the same format, one of them checked by pyarrow above."""
    table = make_table(30_000)
    out = write_through_the_sink(scratch, table, ordered=True, row_group_size=7_500)
    back = run_to_parquet(moruna.ArrowIpcSource(str(out)), scratch / "back")
    assert back.equals(table)


def test_so_t38_a_stream_is_refused(scratch: pathlib.Path) -> None:
    table = make_table(100)
    path = scratch / "stream.arrow"
    with pa.OSFile(str(path), "wb") as sink, pa.ipc.new_stream(sink, table.schema) as writer:
        writer.write_table(table)
    with pytest.raises(moruna.PlanError) as raised:
        moruna.run(
            moruna.ArrowIpcSource(str(path)),
            [],
            moruna.ParquetSink(f"file://{scratch / 'never'}"),
            budget=budget(),
        )
    assert "stream" in str(raised.value)
    assert "file format" in str(raised.value)
    assert str(path) in str(raised.value)
    with pytest.raises(ValueError, match="split_rows"):
        moruna.ArrowIpcSource(str(path), split_rows=0)
