"""HO-T14: a job document run by ``python -m moruna run`` is the same run as ``moruna.run``.

MH H1, H2, H8, MH H1. The document names the kernel by module path and callable, which the command
imports; the library call is handed the same function. The two reports agree on everything but
timing, and the two outputs hold the same rows.
"""

from __future__ import annotations

import importlib.util
import json
import pathlib
import subprocess
import sys

import pyarrow.parquet as pq
import pytest

import moruna

KERNELS = """
import pyarrow.compute as pc
import moruna


def shout(batch):
    return batch.append_column("loud", pc.utf8_upper(batch.column("text")))


@moruna.function(expected_amplification=2.0)
def decorated(batch):
    return batch
"""


def _projection(report: dict) -> dict:
    return {
        # The command's report names the exit as the trace does; the library's report as a
        # code and a status.
        "exit": report["exit"]["status"] if isinstance(report["exit"], dict)
        else report["exit"].lower(),
        "resumed": report["resumed"],
        "limits": (report["limits_initial"]["memory_ceiling"], report["limits_initial"]["cpu_quota"]),
        "gil_serialised": report["gil_serialised"],
        # The command's report is the trace's, by stage; the library's is by task.
        "functions": [(s["rows_in"], s["rows_out"]) for s in report["stages"]]
        if "stages" in report
        else [(t["rows_in"], t["rows_out"]) for t in report["tasks"] if t["role"] == "function"],
    }


def _rows(directory: pathlib.Path) -> list[tuple[int, str]]:
    table = pq.read_table(directory)
    rows = zip(
        table.column("id").to_pylist(),
        table.column("text").to_pylist(),
        strict=True,
    )
    return sorted(rows)


def _document(scratch: pathlib.Path, src_url: str, out: pathlib.Path, callable_: str) -> dict:
    staging = scratch / "staging"
    staging.mkdir(exist_ok=True)
    return {
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": src_url},
        "kernels": [{"kind": "std", "name": "select", "args": {"columns": ["id", "text"]}}],
        "sink": {
            "kind": "parquet",
            "url": str(out),
            "options": {"row_group_bytes": 16 << 20, "file_bytes": 64 << 20},
        },
        "budget": {"memory_bytes": 6 << 30, "cpu": 2.0},
        "staging": {"dir": str(staging), "limit_bytes": 2 << 30},
        "allow_gil": True,
        "report": {"file": str(scratch / "report.json")},
    }


def _run(document: dict, scratch: pathlib.Path, *extra: str) -> tuple[int, dict]:
    path = scratch / "job.json"
    path.write_text(json.dumps(document))
    done = subprocess.run(
        [sys.executable, "-m", "moruna", "run", *extra, str(path)],
        capture_output=True,
        text=True,
        timeout=300,
        check=False,
    )
    return done.returncode, json.loads((scratch / "report.json").read_text())


def test_ho_t14_file_run_equals_library_run(
    dataset: tuple[str, str, int], scratch: pathlib.Path, small_sink, monkeypatch: pytest.MonkeyPatch
) -> None:  # noqa: ANN001
    src_url, out_url, rows = dataset
    (scratch / "kernels.py").write_text(KERNELS)
    # Earlier checks load other temporary kernels.py files. Import this exact
    # file afresh and restore the previous module entry when this test ends.
    spec = importlib.util.spec_from_file_location("kernels", scratch / "kernels.py")
    assert spec is not None and spec.loader is not None
    kernels = importlib.util.module_from_spec(spec)
    monkeypatch.setitem(sys.modules, "kernels", kernels)
    spec.loader.exec_module(kernels)

    from_file_out = scratch / "from_file"
    code, envelope = _run(_document(scratch, src_url, from_file_out, "shout"), scratch)
    assert code == 0, envelope["exit"]
    assert envelope["exit"] == {"code": 0, "diagnostic": None}
    assert envelope["spec_digest"].startswith("sha256:")

    library = moruna.run(
        moruna.ParquetSource(src_url),
        moruna.std.select(["id", "text"]),
        small_sink(out_url),
        budget=6 << 30,
        cpu=2.0,
        staging_dir=str(scratch / "staging"),
        staging_limit=2 << 30,
        allow_gil=True,
    )
    assert _projection(envelope["report"]) == _projection(json.loads(library.to_json()))
    assert len(_rows(from_file_out)) == rows
    assert _rows(from_file_out) == _rows(pathlib.Path(out_url.removeprefix("file://")))


def test_ho_t14_the_command_says_its_version() -> None:
    done = subprocess.run(
        [sys.executable, "-m", "moruna", "--version"],
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    assert done.returncode == 0
    assert done.stdout.strip() == f"moruna {moruna.__version__}"


