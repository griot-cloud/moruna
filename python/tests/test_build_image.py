"""A job built for real and run through the ``image`` kind (0.6.3).

``moruna build`` fetches ``python:3.14-slim`` by digest and installs ``pyarrow`` from the lock,
so it needs the network and a standard CPython 3.14 on the path; the run needs a sandbox host
(Linux, and a cgroup v2 directory with ``memory`` and ``cpu`` delegated), as the image kind's
own Linux tests do. Each test names what it needs and is skipped, saying so, when it is not
there:

``MORUNA_BUILD_REAL``      set to 1: build for real (network)
``MORUNA_SANDBOX_CGROUP``  the cgroup directory, for the run
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import textwrap
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

REAL = os.environ.get("MORUNA_BUILD_REAL") == "1"
CGROUP = os.environ.get("MORUNA_SANDBOX_CGROUP")

PIPELINE = '''
import pyarrow as pa
import pyarrow.compute as pc

import moruna

ROWS = pa.schema([("id", pa.int64()), ("level", pa.int64())])


class Events(moruna.Source):
    """Events, `rows` of them."""

    gives = ROWS

    def __init__(self, rows: int) -> None:
        self.rows = rows

    def plan(self):
        return [moruna.Split(0, self.rows)]

    def read(self, split_id, start, end):
        ids = pa.array(range(start, end), pa.int64())
        return pa.record_batch({"id": ids, "level": pc.bit_wise_and(ids, 7)})


class High(moruna.Function):
    """Keeps the events of level 4 and over."""

    takes = gives = ROWS

    def __call__(self, batch):
        return batch.filter(pc.greater_equal(batch.column("level"), 4))

'''

# The job, in a module of its own: an image imports its task's module, and holds only the task
# classes (not the engine), so a job built at a task module's import is refused inside it.
JOBS = '''
import moruna

from pipeline import Events, High

job = moruna.Job("events")
events = job.add(Events(rows=1000), name="events")
high = job.add(High(), inputs=events, name="high")
job.add(moruna.ParquetSink("out"), inputs=high, name="out")
'''


def moruna(*args: str, cwd: Path) -> subprocess.CompletedProcess[str]:
    env = dict(os.environ, PYTHONPATH=os.pathsep.join([str(cwd), *sys.path]))
    return subprocess.run(
        [sys.executable, "-m", "moruna", *args], capture_output=True, text=True, check=False,
        cwd=cwd, timeout=3600, env=env,
    )  # fmt: skip


@pytest.mark.skipif(not REAL, reason="MORUNA_BUILD_REAL=1 (network)")
def test_a_job_built_for_real_runs_through_the_image_kind(tmp_path: Path) -> None:
    (tmp_path / "pipeline.py").write_text(textwrap.dedent(PIPELINE))
    (tmp_path / "jobs.py").write_text(textwrap.dedent(JOBS))
    (tmp_path / "requirements.lock").write_text(
        subprocess.run(
            ["uv", "pip", "compile", "--generate-hashes", "--no-header", "-"],
            input="pyarrow==25.0.1\n", capture_output=True, text=True, check=True,
        ).stdout  # fmt: skip
    )
    built = moruna("build", "jobs:job", "--json", cwd=tmp_path)
    assert built.returncode == 0, built.stderr
    status = json.loads(built.stdout)
    assert {row["state"] for row in status["tasks"]} == {"built"}
    if sys.platform != "linux" or not CGROUP:
        pytest.skip("the run needs Linux and MORUNA_SANDBOX_CGROUP")
    ran = moruna("run", "job.mjob", "--sandbox-cgroup", CGROUP, cwd=tmp_path)
    assert ran.returncode == 0, ran.stderr
    back = pq.read_table(str(tmp_path / "out"))
    assert back.num_rows == 500
    assert set(back.column("level").to_pylist()) == {4, 5, 6, 7}
    assert back.schema.equals(pa.schema([("id", pa.int64()), ("level", pa.int64())]))


@pytest.mark.skipif(not REAL, reason="MORUNA_BUILD_REAL=1 (network)")
def test_a_built_job_runs_its_images_from_python_given_the_sandbox(tmp_path: Path) -> None:
    """``moruna.run(job, sandbox_cgroup=...)`` runs a built job's images as ``moruna run
    --sandbox-cgroup`` does (0.6.4); 0.6.3's ``moruna.run`` had no sandbox to give them."""
    (tmp_path / "pipeline.py").write_text(textwrap.dedent(PIPELINE))
    (tmp_path / "jobs.py").write_text(textwrap.dedent(JOBS))
    (tmp_path / "requirements.lock").write_text(
        subprocess.run(
            ["uv", "pip", "compile", "--generate-hashes", "--no-header", "-"],
            input="pyarrow==25.0.1\n", capture_output=True, text=True, check=True,
        ).stdout  # fmt: skip
    )
    built = moruna("build", "jobs:job", "--json", cwd=tmp_path)
    assert built.returncode == 0, built.stderr
    if sys.platform != "linux" or not CGROUP:
        pytest.skip("the run needs Linux and MORUNA_SANDBOX_CGROUP")
    import moruna as mo

    job = mo.Job.read((tmp_path / "job.mjob").read_bytes())
    previous = os.getcwd()
    os.chdir(tmp_path)
    try:
        report = mo.run(job, checkpoint=False, sandbox_cgroup=CGROUP)
    finally:
        os.chdir(previous)
    assert report.to_dict()["exit"]["status"] == "completed"
    assert pq.read_table(str(tmp_path / "out")).num_rows == 500


TALLY = '''
import pyarrow as pa
import moruna

# `note` is a string column of nulls, so a batch's last buffer has no bytes (0.6.4).
ROWS = pa.schema([("id", pa.int64()), ("level", pa.int64()), ("note", pa.string())])
LEVELS = pa.schema([("level", pa.int64()), ("count", pa.int64())])


class Events(moruna.Source):
    """Events in splits of a hundred."""

    gives = ROWS

    def __init__(self, splits: int) -> None:
        self.splits = splits

    def plan(self):
        return [moruna.Split(i, 100) for i in range(self.splits)]

    def read(self, split_id, start, end):
        ids = pa.array(range(split_id * 100 + start, split_id * 100 + end), pa.int64())
        levels = pa.array([i % 8 for i in ids.to_pylist()], pa.int64())
        return pa.record_batch({"id": ids, "level": levels, "note": pa.nulls(len(ids), pa.string())})


class Tally(moruna.Function):
    """Counts the events of each level, given when its input ends."""

    takes = ROWS
    gives = LEVELS

    def __init__(self) -> None:
        self.counts: dict[int, int] = {}

    def __call__(self, batch):
        for level in batch.column("level").to_pylist():
            self.counts[level] = self.counts.get(level, 0) + 1
        return pa.RecordBatch.from_pylist([], schema=LEVELS)

    def finish(self):
        levels = sorted(self.counts)
        return pa.record_batch(
            {"level": pa.array(levels, pa.int64()), "count": pa.array([self.counts[k] for k in levels], pa.int64())}
        )


class Totals(moruna.Sink):
    """Commits the levels and events it was given, as its snapshot."""

    takes = LEVELS

    def __init__(self) -> None:
        self.levels = 0
        self.events = 0

    def write(self, batch):
        self.levels += batch.num_rows
        self.events += sum(batch.column("count").to_pylist())

    def finish(self):
        return {"snapshot_id": f"{self.levels}-{self.events}"}
'''

TALLY_JOB = '''
import moruna
from tally import Events, Tally, Totals

job = moruna.Job("tally")
events = job.add(Events(splits=10), name="events")
tally = job.add(Tally(), inputs=events, name="tally")
job.add(Totals(), inputs=tally, name="totals")
'''


@pytest.mark.skipif(not REAL, reason="MORUNA_BUILD_REAL=1 (network)")
def test_a_function_image_gives_another_schema_at_its_end_to_a_sink_image(tmp_path: Path) -> None:
    """Every task an image (0.6.4): a function that gives another schema than it takes, one
    empty batch for each it reads and its tally at its end, read by a sink of its own whose
    commit says what it was given. Its input's last buffer has no bytes, which ended the
    function's input after its first batch."""
    (tmp_path / "tally.py").write_text(textwrap.dedent(TALLY))
    (tmp_path / "jobs.py").write_text(textwrap.dedent(TALLY_JOB))
    (tmp_path / "requirements.lock").write_text(
        subprocess.run(
            ["uv", "pip", "compile", "--generate-hashes", "--no-header", "-"],
            input="pyarrow==25.0.1\n", capture_output=True, text=True, check=True,
        ).stdout  # fmt: skip
    )
    built = moruna("build", "jobs:job", "--json", cwd=tmp_path)
    assert built.returncode == 0, built.stderr
    if sys.platform != "linux" or not CGROUP:
        pytest.skip("the run needs Linux and MORUNA_SANDBOX_CGROUP")
    import moruna as mo

    job = mo.Job.read((tmp_path / "job.mjob").read_bytes())
    previous = os.getcwd()
    os.chdir(tmp_path)
    try:
        report = mo.run(job, checkpoint=False, sandbox_cgroup=CGROUP)
    finally:
        os.chdir(previous)
    done = report.to_dict()
    assert done["exit"]["status"] == "completed", done
    assert [s["snapshot_id"] for s in done["sinks"]] == ["8-1000"]
