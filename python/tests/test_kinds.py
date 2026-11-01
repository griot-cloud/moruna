"""Kinds the program serves itself, from Python: ``@moruna.kind`` and ``moruna.kinds`` (SDD 18
e.3, e.4, KD-I11; PY-T39 to PY-T43).

The kind is declared in the declaration format every kind has, served by a Python callable over
pyarrow batches, and run by the real scheduler between a real Parquet source and sink: each batch
crosses to the callable as an Arrow IPC stream and its answer crosses back the same way, checked
before the sink sees it.
"""

from __future__ import annotations

import pathlib

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq
import pytest

import moruna

SETTINGS = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "additionalProperties": False,
    "properties": {"suffix": {"type": "string"}},
    "examples": [{"suffix": "!"}],
}


@moruna.kind(
    name="shout",
    settings=SETTINGS,
    input_schema={"text": "string"},
    output_schema={"adds": {"loud": "string"}},
)
def shout(batch: pa.RecordBatch, call: moruna.KindCall) -> pa.RecordBatch:
    """Upper-cases `text` into `loud`, with a suffix."""
    suffix = call.settings.get("suffix", "")
    loud = pc.binary_join_element_wise(pc.utf8_upper(batch["text"]), suffix, "")
    return batch.append_column("loud", loud)


def _out(out_url: str) -> pa.Table:
    return pq.read_table(out_url.removeprefix("file://")).sort_by("id")


def test_py_t39_declare_and_list() -> None:
    """A declared kind is listed with the kinds ``moruna.run`` has, in the format every kind's
    declaration has, its port the callable's (KD-I11, e.3)."""
    assert isinstance(shout, moruna.DeclaredKind)
    assert shout.name == "shout"
    declaration = shout.declaration
    assert declaration["isolation"] == "embedder"
    assert declaration["served"]["port"] == {"guest": "shout"}
    assert declaration["served"]["output_schema"] == {
        "adds": {"loud": "string"},
        "changes": {},
        "drops": [],
    }
    assert declaration["summary"] == "Upper-cases `text` into `loud`, with a suffix."
    listing = moruna.kinds(shout)
    kernels = [k["name"] for k in listing["kernels"]]
    assert "shout" in kernels and "std" in kernels and "python" in kernels
    assert [k for k in listing["kernels"] if k["name"] == "shout"][0] == declaration
    assert "shout" not in [k["name"] for k in moruna.kinds()["kernels"]]


def test_py_t40_refusals() -> None:
    """A name a kind of ``moruna.run`` has already, a settings schema that is not a closed one
    with examples, and rules that are not rules are refused when the kind is declared."""
    with pytest.raises(moruna.ConfigError, match="a kernel kind `std` is registered already"):
        moruna.kind(name="std")(lambda batch, call: batch)
    with pytest.raises(moruna.ConfigError, match="additionalProperties"):
        moruna.kind(name="open", settings={**SETTINGS, "additionalProperties": True})(
            lambda batch, call: batch
        )
    with pytest.raises(moruna.ConfigError, match="unknown type `integer`"):
        moruna.kind(name="typo", input_schema={"a": "integer"})(lambda batch, call: batch)
    with pytest.raises(TypeError):
        moruna.kind(name="nothing")(42)


def test_py_t41_serve_and_run(dataset: tuple[str, str, int], small_sink, staging) -> None:  # noqa: ANN001
    """Every batch goes to the callable and its answer is the task's output: the rows out are the
    callable's transform of the rows in."""
    src_url, out_url, rows = dataset
    report = moruna.run(
        moruna.ParquetSource(src_url), shout(suffix="!"), small_sink(out_url), **staging
    )
    assert report.exit == "Completed", report.notes
    back = _out(out_url)
    assert back.num_rows == rows
    assert back.column("loud")[0].as_py() == "ROW 0!"
    assert back.column("loud")[rows - 1].as_py() == f"ROW {rows - 1}!"


def test_py_t42_a_task_fails_loud(dataset: tuple[str, str, int], small_sink, staging) -> None:  # noqa: ANN001
    """Settings the kind's schema refuses are refused before the run; an exception the callable
    raises, and an answer that is not what the kind declares, fail the task naming the kind."""
    src_url, out_url, _ = dataset
    with pytest.raises(moruna.ConfigError, match="`tone` is not a setting of `shout`"):
        moruna.run(moruna.ParquetSource(src_url), shout(tone="x"), small_sink(out_url), **staging)

    @moruna.kind(name="broken")
    def broken(batch: pa.RecordBatch, call: moruna.KindCall) -> pa.RecordBatch:
        raise ValueError("the service is down")

    with pytest.raises(moruna.KernelError, match="the service is down"):
        moruna.run(moruna.ParquetSource(src_url), broken(), small_sink(out_url), **staging)

    @moruna.kind(name="liar", output_schema={"adds": {"score": "double"}})
    def liar(batch: pa.RecordBatch, call: moruna.KindCall) -> pa.RecordBatch:
        return batch

    with pytest.raises(moruna.KernelError, match="column `score`: declared double, not produced"):
        moruna.run(moruna.ParquetSource(src_url), liar(), small_sink(out_url), **staging)


def test_py_t43_kinds_in_a_row(dataset: tuple[str, str, int], small_sink, staging) -> None:  # noqa: ANN001
    """Two declared kinds and a Python kernel in one run: each sees what the one before gave, and
    a table or a list of batches is an answer as a batch is."""

    @moruna.kind(name="halve", output_schema={"adds": {"half": "int64"}})
    def halve(batch: pa.RecordBatch, call: moruna.KindCall) -> pa.Table:
        table = pa.Table.from_batches([batch])
        return table.append_column("half", pc.divide(batch["id"], 2))

    @moruna.kind(name="split")
    def split(batch: pa.RecordBatch, call: moruna.KindCall) -> list[pa.RecordBatch]:
        middle = batch.num_rows // 2
        return [batch.slice(0, middle), batch.slice(middle)]

    src_url, out_url, rows = dataset
    report = moruna.run(
        moruna.ParquetSource(src_url),
        [halve(), lambda batch: batch, split(), shout(suffix="?")],
        small_sink(out_url),
        **staging,
    )
    assert report.exit == "Completed", report.notes
    back = _out(out_url)
    assert back.num_rows == rows
    assert back.column("half")[9].as_py() == 4
    assert back.column("loud")[3].as_py() == "ROW 3?"
    assert pathlib.Path(out_url.removeprefix("file://")).exists()
