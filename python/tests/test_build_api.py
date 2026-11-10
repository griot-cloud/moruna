"""``job.build()`` and ``moruna build JOB`` over the engine's own objects (0.6.3).

The builder's tests (``test_build.py``) run without the engine; this is what needs it: a job of
a source, two functions sharing a package and a sink, built into one image per task with a tiny
runtime layout and hand-made wheels (no network), its spec a draft before and runnable after;
the command builds a job named by its module, and rebuilds a spec only from the code that made
its keys.
"""

from __future__ import annotations

import json
import subprocess
import sys
import textwrap
from pathlib import Path

import builder_support as support
import pytest

import moruna

PIPELINE = '''
import pyarrow as pa

import moruna

ROWS = pa.schema([("id", pa.int64())])


class Events(moruna.Source):
    """Raw events."""

    gives = ROWS

    def __init__(self, rows: int) -> None:
        self.rows = rows

    def plan(self):
        return [moruna.Split(0, self.rows)]

    def read(self, split_id, start, end):
        return pa.record_batch({"id": pa.array(range(start, end), pa.int64())})


class Parse(moruna.Function):
    """Parses events."""

    takes = gives = ROWS

    def __call__(self, batch):
        return batch


class Score(moruna.Function):
    """Scores events against the warehouse."""

    takes = gives = ROWS
    needs = {"warehouse.read": "the warehouse, read only"}

    def __call__(self, batch):
        return batch


class Store(moruna.Sink):
    """Stores alerts."""

    takes = ROWS
    needs = ("warehouse.write",)

    def write(self, batch):
        pass


job = moruna.Job("alerts")
events = job.add(Events(rows=10), name="events")
parsed = job.add(Parse(), inputs=events, name="parse")
scored = job.add(Score(), inputs=parsed, name="score")
job.add(Store(), inputs=scored, name="store")
'''


@pytest.fixture
def project(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    root = tmp_path / "project"
    (root / "alerts").mkdir(parents=True)
    (root / "alerts" / "__init__.py").write_text(textwrap.dedent(PIPELINE))
    wheels = tmp_path / "wheels"
    pyarrow = support.platform_wheels(wheels, "pyarrow", "25.0.1")
    (root / "requirements.lock").write_text(support.lock_line("pyarrow", "25.0.1", pyarrow))
    runtime = support.make_runtime(tmp_path / "runtime", "linux/aarch64")
    monkeypatch.syspath_prepend(str(root))
    monkeypatch.delitem(sys.modules, "alerts", raising=False)
    import alerts  # noqa: PLC0415

    return root, wheels, runtime, alerts


def build_options(wheels: Path, tmp_path: Path) -> dict:
    return {"no_index": True, "find_links": [str(wheels)], "cache": str(tmp_path / "cache")}


def test_a_job_builds_one_image_per_task_and_its_spec_becomes_runnable(
    project, tmp_path: Path
) -> None:
    root, wheels, runtime, alerts = project
    job = alerts.job
    # The platform the fixture runtime is for, not this machine's: CI's runners are x86_64.
    job.target(platform="linux/aarch64", runtime=(str(runtime), "python-test"))
    draft = job.spec()
    assert {row["state"] for row in job.status()} == {"not built"}
    with pytest.raises(moruna.ConfigError, match="draft"):
        moruna.run(moruna.Job.read(draft), ports={"warehouse.read": lambda r: b"", "warehouse.write": lambda r: b""})
    status = job.build(tmp_path / "images", **build_options(wheels, tmp_path))
    assert [row["task"] for row in status] == ["events", "parse", "score", "store"]
    assert {row["state"] for row in status} == {"built"}
    assert all(row["location"] == str(tmp_path / "images") for row in status)
    runnable = job.spec()
    assert runnable != draft, "the spec names its images' digests once built"
    rendered = json.loads(runnable.to_json())
    keys = [t["settings"]["image"]["build_key"] for t in rendered["tasks"]]
    assert keys == [row["build_key"] for row in status], "the keys did not change"
    from moruna import _image_metadata  # noqa: PLC0415

    layout = tmp_path / "images" / "blobs" / "sha256"
    for node in rendered["tasks"]:
        image = node["settings"]["image"]
        manifest = json.loads((layout / image["digest"].split(":")[1]).read_bytes())
        config = json.loads((layout / manifest["config"]["digest"].split(":")[1]).read_bytes())
        descriptor = _image_metadata.read(config["config"]["Labels"])
        assert descriptor["definition"] == node["definition"]
        assert descriptor["entry"] == node["settings"]["entry"]
        assert descriptor["build_key"] == image["build_key"]
        assert descriptor["platform"] == {"os": "linux", "architecture": "arm64"}
    assert "image=built" in repr(job)
    assert job.explain()["images"] == status
    again = job.build(tmp_path / "images", **build_options(wheels, tmp_path))
    assert again == status and job.spec() == runnable, "built again, the same images"
    read = moruna.Job.read(runnable)
    assert {row["state"] for row in read.status()} == {"built"}


def test_the_command_builds_a_job_and_rebuilds_only_the_code_of_its_keys(
    project, tmp_path: Path
) -> None:
    root, wheels, runtime, alerts = project
    command = [
        sys.executable, "-m", "moruna", "build", "alerts:job", "--out", str(tmp_path / "images"),
        "--runtime-layout", str(runtime), "--runtime-reference", "python-test",
        "--platform", "linux/aarch64",
        "--no-index", "--find-links", str(wheels), "--cache", str(tmp_path / "cache"), "--json",
    ]  # fmt: skip
    env = {"PYTHONPATH": f"{root}:{':'.join(sys.path)}", "PATH": "/usr/bin:/bin"}
    import os  # noqa: PLC0415

    env["PATH"] = os.environ["PATH"]
    done = subprocess.run(command, capture_output=True, text=True, cwd=tmp_path, env=env, check=False)
    assert done.returncode == 0, done.stderr
    printed = json.loads(done.stdout)
    spec = tmp_path / "job.mjob"
    assert printed["spec"] == "job.mjob" and spec.is_file()
    assert {row["state"] for row in printed["tasks"]} == {"built"}
    assert moruna.JobSpec(spec.read_bytes()).digest() == printed["job_digest"]
    unserved = subprocess.run(
        [sys.executable, "-m", "moruna", "run", str(spec)], capture_output=True, text=True,
        cwd=tmp_path, env=env, check=False,
    )  # fmt: skip
    assert unserved.returncode == 2
    assert "job['score'] needs 'warehouse.read', and no --resolver was given" in unserved.stderr
    assert "job['store'] needs 'warehouse.write'" in unserved.stderr
    rebuilt = subprocess.run(
        [*command[:4], str(spec), *command[5:]], capture_output=True, text=True, cwd=tmp_path,
        env=env, check=False,
    )  # fmt: skip
    assert rebuilt.returncode == 0, rebuilt.stderr
    source = root / "alerts" / "__init__.py"
    source.write_text(source.read_text().replace("return batch\n", "return batch.slice(0)\n", 1))
    refused = subprocess.run(
        [*command[:4], str(spec), *command[5:]], capture_output=True, text=True, cwd=tmp_path,
        env=env, check=False,
    )  # fmt: skip
    assert refused.returncode == 2
    assert "the spec's build keys are not the code's" in refused.stderr
    assert "job[parse]" in refused.stderr and "job[score]" in refused.stderr


def test_no_lock_is_refused_before_anything_is_fetched(project, tmp_path: Path) -> None:
    root, wheels, runtime, alerts = project
    (root / "requirements.lock").unlink()
    with pytest.raises(moruna.ConfigError, match="build refused: no lock"):
        alerts.job.build(tmp_path / "images", **build_options(wheels, tmp_path))
    assert not (tmp_path / "cache").exists()
