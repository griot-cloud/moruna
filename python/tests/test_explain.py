"""`moruna.explain(...)` (SDD 26 d.5, PN-T26): a job document's plan as a dict, before it runs."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

import moruna


def _document(tmp_path: Path) -> Path:
    doc = {
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": str(tmp_path / "in.parquet")},
        "kernels": [
            {"kind": "std", "name": "cast", "args": {"columns": {"a": "double"}}},
            {"kind": "std", "name": "select", "args": {"columns": ["a"]}},
        ],
        "sink": {"kind": "parquet", "url": str(tmp_path / "out")},
        "budget": {"memory_bytes": 512 << 20, "cpu": 2},
    }
    path = tmp_path / "job.json"
    path.write_text(json.dumps(doc))
    return path


def test_pn_t26_explain_returns_the_plan(tmp_path: Path) -> None:
    path = _document(tmp_path)
    plan = moruna.explain(path)
    assert plan["moruna_plan"] == 1
    assert plan["planner"] == {"name": "levels", "version": 1}
    steps = plan["steps"]
    assert [s["tasks"] for s in steps] == [["source"], ["kernel-1"], ["kernel-2"], ["sink"]]
    assert plan["groups"] == [[s["id"]] for s in steps]
    assert steps[1]["depends_on"] == [steps[0]["id"]]
    assert steps[0]["resources"]["memory_start_bytes"] == 512 << 20
    assert steps[0]["cost"]["duration"]["basis"] == "default"
    assert plan["totals"]["critical_path"] == [s["id"] for s in steps]

    again = moruna.explain(path.read_bytes())
    assert [s["id"] for s in again["steps"]] == [s["id"] for s in steps]
    assert again["id"] == plan["id"]


def test_pn_t26_explain_refuses_by_name(tmp_path: Path) -> None:
    path = _document(tmp_path)
    with pytest.raises(moruna.PlanError, match="no planner `simulated`"):
        moruna.explain(path, planner="simulated")
    doc = json.loads(path.read_text())
    doc["kernels"][0]["kind"] = "no.such"
    path.write_text(json.dumps(doc))
    with pytest.raises(moruna.ConfigError, match="no.such"):
        moruna.explain(path)
