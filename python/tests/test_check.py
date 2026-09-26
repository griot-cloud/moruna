"""MH 4.9 through the package: ``python -m moruna check`` (CK-T5, CK-T8, CK-T9), the schema
declarations on ``@moruna.kernel`` (MH 4.9), the Python fingerprint (MH 4.9), and the Polars
signature (MH 4.9)."""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import textwrap
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import moruna
from moruna import _check

HAS_POLARS = importlib.util.find_spec("polars") is not None
needs_polars = pytest.mark.skipif(
    not HAS_POLARS, reason="polars has no wheel for this interpreter (CK-T8 runs where it has)"
)

AGREEING = """
import pyarrow as pa
import pyarrow.compute as pc
import moruna

@moruna.kernel(input_schema={"text": pa.string()}, output_schema={"adds": {"upper": "string"}})
def shout(batch):
    return batch.append_column("upper", pc.utf8_upper(batch.column("text")))
"""

REFUSED = """
import pyarrow as pa
import pyarrow.compute as pc
import moruna

@moruna.kernel(input_schema={"n": "int64"}, output_schema={"adds": {"half": "int64"}})
def halve(batch):
    return batch.append_column("half", pc.divide(pc.cast(batch.column("n"), pa.float64()), 2.0))
"""

MIXED = (
    AGREEING
    + """
cleaned = moruna.std.fill_null(values={"text": ""})

@moruna.kernel
def undeclared(batch):
    return batch
"""
)

POLARS = """
import polars as pl
import moruna

def doubled(frame: pl.DataFrame) -> pl.DataFrame:
    return frame.with_columns((pl.col("n") * 2).alias("twice"))

@moruna.kernel(input_schema={"n": "int64", "s": "string"},
               output_schema={"adds": {}})
def lazily(frame: pl.LazyFrame) -> pl.LazyFrame:
    return frame.filter(pl.col("n") > 0).with_columns(pl.col("s").str.to_uppercase())

doubled = moruna.kernel(doubled, input_schema={"n": "int64"},
                        output_schema={"adds": {"twice": "int64"}})
"""


def write(tmp_path: Path, name: str, text: str) -> Path:
    path = tmp_path / f"{name}.py"
    path.write_text(textwrap.dedent(text))
    return path


def run_check(*args: str) -> tuple[int, str, str]:
    done = subprocess.run(
        [sys.executable, "-m", "moruna", "check", *args],
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )
    return done.returncode, done.stdout, done.stderr


def test_ck_t9_an_agreeing_module_exits_0_and_writes_a_profile(tmp_path: Path) -> None:
    module = write(tmp_path, "agreeing", AGREEING)
    profiles = tmp_path / "profiles"
    code, out, _ = run_check(str(module), "--json", "--profiles-dir", str(profiles))
    assert code == 0, out
    report = json.loads(out)
    assert report["exit"] == 0
    [kernel] = report["kernels"]
    assert kernel["kernel"] == "shout"
    assert kernel["verdict"] == "agreed"
    assert kernel["fingerprint"].startswith("sha256:")
    assert kernel["profile"]["written"]
    row = json.loads(Path(kernel["profile"]["path"]).read_text())
    assert row["source"] == "moruna check"
    assert row["gil"] in ("free_threaded", "serialised")
    assert [b["name"] for b in kernel["batches"]] == [
        "empty",
        "one_row",
        "preferred",
        "all_null",
        "edges",
    ]


def test_ck_t9_h13_a_refused_kernel_names_the_column_and_exits_2(tmp_path: Path) -> None:
    module = write(tmp_path, "refused", REFUSED)
    code, out, _ = run_check(str(module), "--no-profile")
    assert code == 2
    assert "halve (python): refused" in out
    assert "column `half`: declared int64, produced double" in out


def test_ck_t9_kernel_selection_and_no_kernels(tmp_path: Path) -> None:
    module = write(tmp_path, "mixed", MIXED)
    code, out, _ = run_check(str(module), "--json", "--no-profile")
    report = json.loads(out)
    verdicts = {k["kernel"]: k["verdict"] for k in report["kernels"]}
    assert verdicts == {"shout": "agreed", "cleaned": "agreed", "undeclared": "not_checkable"}
    assert code == 2
    code, out, _ = run_check(str(module), "--kernel", "cleaned", "--no-profile")
    assert code == 0, out
    assert "cleaned (std): agreed" in out
    code, _, err = run_check(str(module), "--kernel", "missing")
    assert code == 2
    assert "no kernel named 'missing'" in err
    empty = write(tmp_path, "empty", "x = 1\n")
    code, _, err = run_check(str(empty))
    assert code == 2
    assert "no kernels" in err
    broken = write(tmp_path, "broken", "raise RuntimeError('nope')\n")
    code, out, _ = run_check(str(broken), "--json")
    assert code == 2
    assert "RuntimeError: nope" in json.loads(out)["error"]


def test_main_usage() -> None:
    from moruna.__main__ import main  # noqa: PLC0415, importing it is what is tested

    assert main([]) == 2
    assert main(["--help"]) == 0
    assert main(["frobnicate"]) == 2


def test_the_check_is_seeded(tmp_path: Path) -> None:
    module = write(tmp_path, "seeded", AGREEING)
    loaded = _check._load(str(module))
    [(name, value)] = _check.kernels_of(loaded)
    a = _check.check_one(name, value, 3, None)
    b = _check.check_one(name, value, 3, None)
    assert [x["bytes_in"] for x in a["batches"]] == [x["bytes_in"] for x in b["batches"]]
    assert a["seed"] == 3


def test_declarations_translate_and_bad_ones_are_refused() -> None:
    schema = pa.schema([pa.field("a", pa.int64(), nullable=False)])
    k = moruna.kernel(lambda b: b, input_schema=schema, output_schema=schema)
    assert k.fingerprint
    with pytest.raises(TypeError):
        moruna.kernel(lambda b: b, input_schema=["a"])
    with pytest.raises(TypeError):
        moruna.kernel(lambda b: b, input_schema={"a": object()})
    with pytest.raises(moruna.PlanError):
        moruna.kernel(lambda b: b, input_schema={"a": "tensor"})
    with pytest.raises(TypeError):
        moruna.kernel(lambda b: b, output_schema={"drops": "a"})
    relative = moruna.kernel(
        lambda b: b,
        input_schema={"a": int, "b": float, "c": str, "d": bool},
        output_schema={"drops": ["a"], "changes": {"b": "float32"}, "adds": {"e": "any"}},
    )
    assert relative.fingerprint


def test_the_fingerprint_follows_source_schema_and_lockfile(tmp_path: Path) -> None:
    def f(batch):
        return batch

    base = moruna.kernel(f).fingerprint
    assert moruna.kernel(f).fingerprint == base
    assert moruna.kernel(f, input_schema={"a": "int64"}).fingerprint != base
    lock = tmp_path / "uv.lock"
    lock.write_bytes(b"one")
    with_lock = moruna.kernel(f, lockfile=lock).fingerprint
    assert with_lock != base
    assert moruna.kernel(f, lockfile=b"one").fingerprint == with_lock
    assert moruna.kernel(f, lockfile=b"two").fingerprint != with_lock


def test_ck_t5_every_standard_kernel_checks_through_the_binding() -> None:
    kernels = {
        "cast": moruna.std.cast(columns={"a": pa.float64()}),
        "rename": moruna.std.rename(columns={"a": "b"}),
        "select": moruna.std.select(columns=["a"]),
        "drop": moruna.std.drop(columns="a"),
        "filter": moruna.std.filter(expr="a > 1"),
        "fill_null": moruna.std.fill_null(values={"a": 0}),
        "dedupe": moruna.std.dedupe(keys=["a"]),
        "hash": moruna.std.hash(columns=["a"], algo="blake3"),
        "mask": moruna.std.mask(columns=["s"], mode="partial", keep=1),
        "explode": moruna.std.explode(column="l"),
        "concat_str": moruna.std.concat_str(columns=["a", "b"], separator="-"),
        "date_trunc": moruna.std.date_trunc(column="t", unit="day", output="d"),
    }
    for name, kernel in kernels.items():
        assert kernel.name == name
        assert len(kernel.fingerprint) == 64
        assert name in repr(kernel)
        report = json.loads(moruna._core.check_kernel(kernel, seed=1))
        assert report["verdict"] == "agreed", report["summary"]
        assert report["kind"] == "std"
    with pytest.raises(moruna.PlanError):
        moruna.std.cast(columns={"a": "nope"})
    with pytest.raises(TypeError):
        moruna.std.cast(columns={"a": object()})
    with pytest.raises(TypeError):
        moruna._core.check_kernel(object())
    assert moruna.std.hash(columns="a").args == '{"columns":["a"]}'


def test_std_kernels_run_and_fuse_in_moruna_run(tmp_path: Path) -> None:
    table = pa.table(
        {
            "id": pa.array([1, 2, 2, None], pa.int64()),
            "name": ["alice", None, "bob", "carol"],
            "score": pa.array([1.5, None, 20.0, 7.25]),
        }
    )
    src = tmp_path / "in.parquet"
    pq.write_table(table, src)
    out = tmp_path / "out"
    report = moruna.run(
        moruna.ParquetSource(str(src)),
        [
            moruna.std.fill_null(values={"score": 0.0}),
            moruna.std.filter(expr="score > 1"),
            moruna.std.cast(columns={"score": "int64"}),
            moruna.std.select(columns=["id", "score"]),
            moruna.std.dedupe(keys="id"),
        ],
        moruna.ParquetSink(str(out)),
        budget="512MiB",
        profiles_dir=str(tmp_path / "profiles"),
        staging_dir=str(tmp_path / "staging"),
    )
    assert len(report.stages) == 3, "fill_null+filter and cast+select each ran as one stage"
    got = pq.read_table(out).sort_by("score").to_pylist()
    assert got == [{"id": 1, "score": 1}, {"id": None, "score": 7}, {"id": 2, "score": 20}]


@needs_polars
def test_ck_t8_a_polars_signature_checks_and_runs(tmp_path: Path) -> None:
    module = write(tmp_path, "frames", POLARS)
    code, out, _ = run_check(str(module), "--json", "--no-profile")
    report = json.loads(out)
    assert code == 0, out
    assert {k["kernel"] for k in report["kernels"]} == {"doubled", "lazily"}
    loaded = _check._load(str(module))
    doubled = loaded.doubled
    src = tmp_path / "in.parquet"
    pq.write_table(pa.table({"n": pa.array([1, 2, 3], pa.int64())}), src)
    out_dir = tmp_path / "out"
    moruna.run(
        moruna.ParquetSource(str(src)),
        doubled,
        moruna.ParquetSink(str(out_dir)),
        budget="512MiB",
        allow_gil=True,
        profiles_dir=str(tmp_path / "profiles"),
        staging_dir=str(tmp_path / "staging"),
    )
    got = pq.read_table(out_dir).sort_by("n").to_pylist()
    assert got == [{"n": 1, "twice": 2}, {"n": 2, "twice": 4}, {"n": 3, "twice": 6}]


@needs_polars
def test_an_undecorated_polars_function_is_found_and_wrapped(tmp_path: Path) -> None:
    import polars as pl  # noqa: PLC0415, polars is optional

    module = write(
        tmp_path,
        "bare",
        """
        import polars as pl

        def plain(frame: pl.DataFrame) -> pl.DataFrame:
            return frame
        """,
    )
    loaded = _check._load(str(module))
    [(name, kernel)] = _check.kernels_of(loaded)
    assert name == "plain"
    report = _check.check_one(name, kernel, 0, None)
    assert report["verdict"] == "not_checkable"

    def not_frames(frame: pl.DataFrame) -> int:
        return 1

    assert moruna._declare.polars_signature(not_frames) is None
    wrapped = moruna.polars(lambda f: f.with_columns(pl.lit(1).alias("one")))
    batch = pa.RecordBatch.from_pydict({"s": ["a"]})
    assert wrapped(batch).num_columns == 2
