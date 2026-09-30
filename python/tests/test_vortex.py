"""The Vortex format end to end from Python: `moruna.VortexSink` writes it, `moruna.VortexSource`
reads it back through a kernel, and the rows are the rows that went in (07 e.7, e.8; 08 f.11;
SDD 12 d.2 and PY-T19, 2026-09-29).

The suite has no Vortex package of its own, so the Vortex files are written by Moruna from a
Parquet file pyarrow wrote, and what comes out is written as Parquet for pyarrow to read back: the
Vortex files are made and read only by the code under test, and the comparison is made against
data neither end of it produced.
"""

from __future__ import annotations

import pathlib

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq
import pytest

import moruna

ROWS = 50_000


def budget() -> int:
    """A budget with room above what this process already holds. The claim here is the format,
    not the budget, and the budget counts the whole process: earlier in-process runs leave the
    test process holding a host-dependent amount, so a fixed figure would be refused or not
    depending on test order. The floor a run needs is what the process holds, plus the arena's
    floor, plus a reserve of a tenth of the budget itself, so the room is added before dividing
    by 0.9, not after."""
    held = moruna.inspect_host()["anon_bytes"]
    return int((held + (512 << 20)) / 0.9)


@moruna.kernel
def shout(batch: pa.RecordBatch) -> pa.RecordBatch:
    """Upper case the text and keep the rest, which is real work without its cost."""
    return pa.RecordBatch.from_arrays(
        [batch.column("id"), pc.utf8_upper(batch.column("text")), batch.column("score")],
        names=["id", "text", "score"],
    )


@pytest.fixture
def vortex_dataset(scratch: pathlib.Path) -> tuple[pathlib.Path, pa.Table]:
    """A Parquet file pyarrow wrote, copied into Vortex files by `moruna.VortexSink`."""
    table = pa.table(
        {
            "id": pa.array(range(ROWS), pa.int64()),
            "text": pa.array([f"row {i}" for i in range(ROWS)]),
            "score": pa.array([None if i % 7 == 0 else i * 0.5 for i in range(ROWS)], pa.float64()),
        }
    )
    src = scratch / "in"
    src.mkdir()
    pq.write_table(table, src / "part-0.parquet", row_group_size=10_000)
    vx = scratch / "vx"
    report = moruna.run(
        moruna.ParquetSource(f"file://{src}/part-0.parquet"),
        [],
        moruna.VortexSink(f"file://{vx}", file_bytes="64MiB"),
        budget=budget(),
        ordered=True,
    )
    assert report.exit == "Completed", report.notes
    return vx, table


def test_py_t19_vortex_sink_writes_rolling_part_files(
    vortex_dataset: tuple[pathlib.Path, pa.Table],
) -> None:
    vx, _ = vortex_dataset
    names = sorted(p.name for p in vx.iterdir())
    assert "_SUCCESS" in names, names
    parts = [n for n in names if n.endswith(".vortex")]
    assert parts and parts[0] == "part-00000.vortex", names
    assert not [n for n in names if n.endswith(".tmp")], names


def test_py_t19_vortex_source_through_a_kernel(
    scratch: pathlib.Path, vortex_dataset: tuple[pathlib.Path, pa.Table]
) -> None:
    vx, table = vortex_dataset
    out = scratch / "out"
    report = moruna.run(
        moruna.VortexSource(str(vx), split_bytes=256 << 10),
        [shout],
        moruna.ParquetSink(f"file://{out}", row_group_bytes="16MiB", file_bytes="64MiB"),
        budget=budget(),
    )
    assert report.exit == "Completed", report.notes
    assert report.stages[0]["rows_in"] == ROWS, report.stages
    back = pq.read_table(str(out)).sort_by("id")
    assert back.num_rows == ROWS
    assert back.column("id").to_pylist() == table.column("id").to_pylist()
    assert back.column("text").to_pylist() == [t.upper() for t in table.column("text").to_pylist()]
    assert back.column("score").to_pylist() == table.column("score").to_pylist()


def test_py_t19_vortex_round_trip_and_projection(
    scratch: pathlib.Path, vortex_dataset: tuple[pathlib.Path, pa.Table]
) -> None:
    """Vortex to Vortex, then a projection by name, which keeps the file's column order."""
    vx, table = vortex_dataset
    again = scratch / "again"
    report = moruna.run(
        moruna.VortexSource(f"file://{vx}"),
        [],
        moruna.VortexSink(str(again), file_bytes="64MiB"),
        budget=budget(),
        ordered=True,
    )
    assert report.exit == "Completed", report.notes
    out = scratch / "projected"
    report = moruna.run(
        moruna.VortexSource(str(again), columns=["score", "id"]),
        [],
        moruna.ParquetSink(f"file://{out}", row_group_bytes="16MiB", file_bytes="64MiB"),
        budget=budget(),
    )
    assert report.exit == "Completed", report.notes
    back = pq.read_table(str(out)).sort_by("id")
    assert back.column_names == ["id", "score"]
    assert back.column("score").to_pylist() == table.column("score").to_pylist()


def test_py_t19_vortex_handles_validate_what_they_are_given(
    scratch: pathlib.Path, vortex_dataset: tuple[pathlib.Path, pa.Table]
) -> None:
    vx, _ = vortex_dataset
    assert repr(moruna.VortexSource([str(vx), str(vx)])) == "VortexSource(2 url(s))"
    assert repr(moruna.VortexSink("s3://bucket/out")) == "VortexSink(s3://bucket/out)"
    assert "VortexSource" in moruna.__all__ and "VortexSink" in moruna.__all__
    with pytest.raises(ValueError, match="split_bytes"):
        moruna.VortexSource(str(vx), split_bytes=0)
    with pytest.raises(ValueError):
        moruna.VortexSource([])
    with pytest.raises(TypeError):
        moruna.VortexSink(str(scratch / "x"), file_bytes=[1])
    # A projection naming a column the files lack is refused before anything runs.
    with pytest.raises(moruna.PlanError) as refused:
        moruna.run(
            moruna.VortexSource(str(vx), columns=["nope"]),
            [],
            moruna.VortexSink(str(scratch / "never"), file_bytes="64MiB"),
            budget=budget(),
        )
    assert "nope" in refused.value.message
    # A small file size is clamped, and the report says so.
    report = moruna.run(
        moruna.VortexSource(str(vx)),
        [],
        moruna.VortexSink(str(scratch / "clamped"), file_bytes=1024),
        budget=budget(),
    )
    assert report.exit == "Completed", report.notes
    assert any("sink.file_bytes" in note for note in report.notes), report.notes
