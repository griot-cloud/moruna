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
    (tmp_path / "requirements.lock").write_text(
        subprocess.run(
            ["uv", "pip", "compile", "--generate-hashes", "--no-header", "-"],
            input="pyarrow==25.0.1\n", capture_output=True, text=True, check=True,
        ).stdout  # fmt: skip
    )
    built = moruna("build", "pipeline:job", "--json", cwd=tmp_path)
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
