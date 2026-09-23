"""Scratch directories and a small Parquet dataset for the surface tests.

Every test writes only under a directory unique to its own process (preamble 6.7): several
executors run the gate on one machine at the same time.
"""

from __future__ import annotations

import os
import pathlib
import tempfile

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import moruna

# Outside a cgroup the discovered ceiling is most of the host's RAM, and every run reserves an
# arena against it. A suite that starts thirty runs in one process on a developer host is then
# reserving that much thirty times over, and the host kills the process long before the last
# test. `MORUNA_BUDGET` is the platform-owned spelling of the ceiling (preamble section 5), so
# the calls in the tests stay parameter-free, which is what PY-I3 is about, and the suite runs
# inside a ceiling the host can carry. A caller that sets it already keeps their own value.
os.environ.setdefault("MORUNA_BUDGET", "6GiB")

# `profiles.dir` defaults to `~/.moruna/profiles`, which is one fixed path shared by every run on
# the machine, and a test that writes a profile there changes what a later run of anything else
# probes. Preamble 6.7 says a test writes only to a directory unique to its own process, so this
# process gets its own home, which the facade reads when a run starts, and every subprocess a
# test starts inherits it.
_HOME = pathlib.Path(tempfile.gettempdir()) / f"moruna-home-{os.getpid()}"
(_HOME / ".moruna").mkdir(parents=True, exist_ok=True)
os.environ["HOME"] = str(_HOME)


@pytest.fixture
def scratch(request: pytest.FixtureRequest) -> pathlib.Path:
    name = request.node.name.replace("/", "_")[:40]
    return pathlib.Path(tempfile.mkdtemp(prefix=f"moruna-{name}-{os.getpid()}-"))


@pytest.fixture
def dataset(scratch: pathlib.Path) -> tuple[str, str, int]:
    """A real Parquet file, a real output directory, and the row count. Returns URLs."""
    rows = 10_000
    src = scratch / "in"
    out = scratch / "out"
    src.mkdir()
    out.mkdir()
    table = pa.table(
        {
            "id": pa.array(range(rows), pa.int64()),
            "text": pa.array([f"row {i}" for i in range(rows)]),
        }
    )
    pq.write_table(table, src / "part-0.parquet", row_group_size=2_000)
    return f"file://{src}/part-0.parquet", f"file://{out}", rows


@pytest.fixture
def staging(scratch: pathlib.Path) -> dict[str, object]:
    """`staging_dir=` and `staging_limit=` for a run that may have to stage.

    Under the suite's ceiling a run can reach the disk tier, and a run that reaches it with no
    staging directory resolved has a disk bound of zero and fails. Passing both is what a user
    with a small ceiling does, and it keeps the suite's failures about the surface.
    """
    directory = scratch / "staging"
    directory.mkdir(exist_ok=True)
    return {"staging_dir": str(directory), "staging_limit": "2GiB"}


@pytest.fixture
def small_sink():
    """A `ParquetSink` whose buffers fit a test host.

    The default `sink.file_bytes` is 1 GiB and the sink reserves it from the arena in one
    buffer, which is most of a test ceiling on its own. Every test but the one about defaults
    (PY-T3) asks for smaller files, because what it is testing is not the default.
    """

    def build(url: str, **kwargs: object):
        kwargs.setdefault("row_group_bytes", "16MiB")
        kwargs.setdefault("file_bytes", "64MiB")
        return moruna.ParquetSink(url, **kwargs)

    return build
