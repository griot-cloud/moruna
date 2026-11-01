"""PY-T37 (SDD 12 PY-I13): a run's events are records, and ``moruna.read_events`` reads them as
the dicts the events had before they were records.

A run with no peer writes its events to ``_events.mrec`` in its output store; the same records
are what a peer is sent as frames. ``read_events`` decodes records written by the Rust runtime,
so what is checked here is the FlatBuffers and record layout from the Python side, against a
real run.
"""

from __future__ import annotations

import json
import pathlib
import subprocess
import sys

import pytest

import moruna

KERNELS = """
def keep(batch):
    return batch
"""


def _run(scratch: pathlib.Path, src_url: str, *extra: str) -> tuple[subprocess.CompletedProcess[str], pathlib.Path]:
    (scratch / "kernels.py").write_text(KERNELS)
    staging = scratch / "staging"
    staging.mkdir(exist_ok=True)
    logs = scratch / "logs"
    document = {
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": src_url},
        "kernels": [{"kind": "python", "module": str(scratch / "kernels.py"), "callable": "keep"}],
        "sink": {
            "kind": "parquet",
            "url": str(scratch / "out"),
            "options": {"row_group_bytes": 16 << 20, "file_bytes": 64 << 20},
        },
        "budget": {"memory_bytes": 6 << 30, "cpu": 2.0},
        "staging": {"dir": str(staging), "limit_bytes": 2 << 30},
        "allow_gil": True,
        "report": {"file": str(scratch / "report.json"), "logs": str(logs)},
    }
    path = scratch / "job.json"
    path.write_text(json.dumps(document))
    done = subprocess.run(
        [sys.executable, "-m", "moruna", "run", *extra, str(path)],
        capture_output=True,
        text=True,
        timeout=300,
        check=False,
    )
    return done, logs


def test_py_t37_a_run_with_no_peer_writes_records_that_read_as_dicts(
    dataset: tuple[str, str, int], scratch: pathlib.Path
) -> None:
    src_url, _, _ = dataset
    done, logs = _run(scratch, src_url)
    assert done.returncode == 0, done.stderr
    [attempt] = [p for p in logs.iterdir() if p.name.startswith("a1-")]
    events = moruna.read_events(attempt / "_events.mrec")
    assert events, "the run told its events"
    assert [e["seq"] for e in events] == list(range(1, len(events) + 1))
    assert all(e["type"] == "event" and e["v"] == 7 and e["attempt"] == 1 for e in events)
    assert len({e["start"] for e in events}) == 1
    assert events[0]["kind"] == "attempt_started"
    assert events[0]["resumed"] is False
    assert isinstance(events[0]["plan"], str)
    registered = [e for e in events if e["kind"] == "task_registered"]
    assert {e["task"] for e in registered} == {"source", "kernel-1", "sink"}
    assert all(e["step"] is not None for e in registered)
    for task in ("source", "kernel-1", "sink"):
        ends = [
            e
            for e in events
            if e.get("task") == task and e["kind"] in ("task_completed", "task_failed", "task_skipped")
        ]
        assert len(ends) == 1, (task, ends)
        assert ends[0]["kind"] == "task_completed" and ends[0]["outcome"] == "succeeded"
    assert events[-1]["kind"] == "logs"
    assert all(p["drained"] for p in events[-1]["producers"])
    # The bytes read as bytes give the same dicts.
    assert moruna.read_events((attempt / "_events.mrec").read_bytes()) == events


def test_py_t37_events_json_prints_the_same_events(
    dataset: tuple[str, str, int], scratch: pathlib.Path
) -> None:
    src_url, _, _ = dataset
    done, logs = _run(scratch, src_url, "--events-json")
    assert done.returncode == 0, done.stderr
    printed = [json.loads(line) for line in done.stdout.splitlines()]
    [attempt] = [p for p in logs.iterdir() if p.name.startswith("a1-")]
    # With a stream to print to, the run still had no peer: the file has the same events.
    stored = moruna.read_events(attempt / "_events.mrec")
    assert [(e["seq"], e["kind"]) for e in printed] == [(e["seq"], e["kind"]) for e in stored]
    assert all(e["v"] == 7 for e in printed)


def test_py_t37_bytes_that_are_not_events_are_refused(tmp_path: pathlib.Path) -> None:
    assert moruna.read_events(b"") == []
    with pytest.raises(ValueError, match="not a record|cut short"):
        moruna.read_events(b"\x10\x00\x00\x00 not a record at all")
    with pytest.raises(ValueError):
        moruna.read_events(b"x")
