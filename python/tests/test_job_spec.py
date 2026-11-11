"""A job's spec, the tasks a user writes, what they need, and the report (0.6.2, 0.6.3).

The tasks a spec names live in two small packages written to a temporary directory on the
interpreter's path (``tasks_a`` and ``tasks_b``), as a user's code would; a test reads a spec
back and runs it in this process and through ``python -m moruna run``. Every component is real.
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

import moruna

TASKS_A = '''
import pyarrow as pa
import pyarrow.compute as pc

import moruna

ROWS = pa.schema([("id", pa.int64()), ("text", pa.string())])


class Numbers(moruna.Source):
    """`rows` rows of `id` and `text`, in splits of 1,000."""

    gives = ROWS

    def __init__(self, rows: int, split: int = 1_000) -> None:
        self.rows, self.split = rows, split

    def plan(self):
        count = -(-self.rows // self.split)
        return [moruna.Split(i, min(self.split, self.rows - i * self.split)) for i in range(count)]

    def read(self, split_id, start, end):
        first = split_id * self.split + start
        ids = list(range(first, first + end - start))
        return pa.record_batch({"id": pa.array(ids, pa.int64()),
                                "text": pa.array([f"row {i}" for i in ids])})


class Shout(moruna.Function):
    takes = ROWS
    gives = pa.schema([("id", pa.int64()), ("text", pa.string()), ("loud", pa.string())])

    def __call__(self, batch):
        return batch.append_column("loud", pc.utf8_upper(batch.column("text")))


class Enrich(moruna.Function):
    """Adds the warehouse's figure.

    The column `table`: the warehouse's figure for it, times `factor`.
    """

    needs = {"warehouse.read": "the warehouse, read only"}

    def __init__(self, table: str, factor: int = 2) -> None:
        self.table, self.factor = table, factor

    def __call__(self, batch):
        figure = self.ports["warehouse.read"][self.table] * self.factor
        return batch.append_column(self.table, pa.array([figure] * batch.num_rows, pa.int64()))


class Total(moruna.Function):
    """Gives one row at the end: how many rows it saw."""

    def __init__(self) -> None:
        self.rows = 0

    def __call__(self, batch):
        self.rows += batch.num_rows
        return pa.record_batch({"rows": pa.array([], pa.int64())})

    def finish(self):
        return pa.record_batch({"rows": pa.array([self.rows], pa.int64())})


class Collect(moruna.Sink):
    """Keeps every batch in the list the runner supplies as `store`; commits a snapshot."""

    needs = ("store",)
    target = "memory://collect"

    def write(self, batch):
        self.ports["store"].append(batch)

    def finish(self):
        return {"snapshot_id": 41 + len(self.ports["store"]), "version": "v1"}


class Host(moruna.Resolver):
    """A host's resolver: it answers needs its own way."""

    def resolve(self, need):
        if need == "warehouse.read":
            return {"orders": 21}
        raise LookupError(need)


class Window(moruna.Function):
    """Stamps each row with the run's window."""

    def __call__(self, batch):
        start = self.args["start"]
        return batch.append_column("start", pa.array([start] * batch.num_rows))


@moruna.function
def tagged(batch):
    return batch.append_column("decorated", pa.array([True] * batch.num_rows))
'''

TASKS_B = '''
import pyarrow as pa

import moruna


class Tag(moruna.Function):
    def __init__(self, label: str) -> None:
        self.label = label

    def __call__(self, batch):
        return batch.append_column("tag", pa.array([self.label] * batch.num_rows))


class Secretive(moruna.Function):
    def __init__(self, api_token: str) -> None:
        self.api_token = api_token

    def __call__(self, batch):
        return batch
'''

ROWS = 5_000


@pytest.fixture(scope="module")
def tasks(tmp_path_factory: pytest.TempPathFactory):
    """The two packages, imported from a directory on the path."""
    root = tmp_path_factory.mktemp("packages")
    for name, text in (("tasks_a", TASKS_A), ("tasks_b", TASKS_B)):
        (root / name).mkdir()
        (root / name / "__init__.py").write_text(textwrap.dedent(text))
    sys.path.insert(0, str(root))
    import tasks_a  # noqa: PLC0415
    import tasks_b  # noqa: PLC0415

    yield root, tasks_a, tasks_b
    sys.path.remove(str(root))


def _job(tasks_a, tasks_b, out: Path) -> moruna.Job:
    job = moruna.Job("orders")
    rows = job.add(tasks_a.Numbers(rows=ROWS), name="rows")
    loud = job.add(tasks_a.Shout(), inputs=rows)
    enriched = job.add(tasks_a.Enrich(table="orders"), inputs=loud)
    tagged = job.add(tasks_b.Tag(label="b"), inputs=enriched)
    job.add(moruna.ParquetSink(str(out)), inputs=tagged, name="out")
    return job


def test_names_are_tasks_and_functions(tasks) -> None:
    """Source, Function and Sink are Tasks; Moruna's own functions are Functions; the old
    names are gone."""
    _, tasks_a, _ = tasks
    assert issubclass(moruna.Source, moruna.Task)
    assert issubclass(moruna.Function, moruna.Task)
    assert issubclass(moruna.Sink, moruna.Task)
    assert isinstance(moruna.std.select(["id"]), moruna.Function)
    assert isinstance(tasks_a.tagged, moruna.Function)
    assert isinstance(moruna.function(lambda b: b), moruna.Function)
    for gone in ("Graph", "GraphTask", "kernel", "explain", "build", "KernelError", "KernelSpec",
                 "StdKernel", "PackageKernel"):
        assert not hasattr(moruna, gone), gone


def test_a_job_prints_its_tasks(tasks, tmp_path: Path) -> None:
    _, tasks_a, tasks_b = tasks
    job = _job(tasks_a, tasks_b, tmp_path / "out")
    lines = repr(job).splitlines()
    assert lines[0] == "moruna.Job('orders', 5 tasks)"
    assert len(lines) == 6
    enrich = lines[3]
    assert enrich.startswith("  enrich  function  image tasks_a:Enrich")
    assert 'params={"factor": 2, "table": "orders"}' in enrich
    assert "inputs=shout" in enrich and "needs=warehouse.read" in enrich
    assert "image=not built key=sha256:" in enrich
    assert "takes={id: int64, text: string}" in lines[2]
    assert "image=" not in lines[5], "Moruna's own kinds have no image"
    assert str(job) == repr(job)


def test_the_spec_is_the_document_and_reads_back(tasks, tmp_path: Path) -> None:
    """spec() at any step; bytes are MJB2 FlatBuffers; the digest is over exactly them; a spec
    read back writes the same bytes; every task is defined inline; to_json is for people."""
    _, tasks_a, tasks_b = tasks
    job = moruna.Job("orders")
    job.add(tasks_a.Numbers(rows=10), name="rows")
    partial = job.spec()
    assert partial.bytes()[4:8] == b"MJB2"
    job = _job(tasks_a, tasks_b, tmp_path / "out")
    spec = job.spec()
    import hashlib  # noqa: PLC0415

    assert spec.digest() == "sha256:" + hashlib.sha256(spec.bytes()).hexdigest()
    assert spec != partial
    again = moruna.Job.read(spec.bytes())
    assert again.name == "orders"
    assert again.spec() == spec, "a spec read back writes the same bytes"
    rendered = json.loads(spec.to_json())
    enrich = [t for t in rendered["tasks"] if t["id"] == "enrich"][0]
    assert enrich["role"] == "function" and enrich["kind"] == "image"
    settings = enrich["settings"]
    assert settings["entry"] == "tasks_a:Enrich"
    assert settings["params"] == {"table": "orders", "factor": 2}
    assert settings["needs"] == ["warehouse.read"]
    assert settings["image"]["build_key"].startswith("sha256:")
    assert settings["image"]["digest"] is None and settings["image"]["location"] is None
    assert "ref" not in settings and "fingerprint" not in settings, "no python kind is left"
    definition = enrich["definition"]
    assert definition["title"] == "Adds the warehouse's figure."
    assert definition["description"].startswith("The column `table`")
    assert definition["requires"] == [
        {"name": "warehouse.read", "description": "the warehouse, read only"}
    ]
    assert definition["settings"]["properties"] == {
        "table": {"type": "string"}, "factor": {"type": "integer", "default": 2},
    }  # fmt: skip
    assert definition["settings"]["required"] == ["table"]
    assert definition["provenance"]["build_key"] == settings["image"]["build_key"]
    shout = [t for t in rendered["tasks"] if t["id"] == "shout"][0]["definition"]
    assert shout["takes"]["fields"] == [
        {"name": "id", "type": "int64", "nullable": True},
        {"name": "text", "type": "string", "nullable": True},
    ]
    out = [t for t in rendered["tasks"] if t["id"] == "out"][0]
    assert out["kind"] == "parquet" and out["definition"]["name"] == "parquet", (
        "a task of Moruna's own kind carries its kind's declaration"
    )
    assert "inputs=shout" in repr(again).splitlines()[3]
    keys = {t["id"]: t["settings"]["image"]["build_key"] for t in rendered["tasks"]
            if t["kind"] == "image"}  # fmt: skip
    assert len(set(keys.values())) == len(keys), "one key per task of other code"


def test_the_writer_port(tasks, tmp_path: Path) -> None:
    """A path, a callable taking bytes and a writer of the caller's own; a URL is refused."""
    _, tasks_a, tasks_b = tasks
    job = _job(tasks_a, tasks_b, tmp_path / "out")
    spec = job.write(tmp_path / "orders.mjob")
    assert (tmp_path / "orders.mjob").read_bytes() == spec.bytes()
    got: list[bytes] = []
    job.write(got.append)
    assert got == [spec.bytes()]

    class Kept:
        def __init__(self) -> None:
            self.spec = self.report = b""

        def write_spec(self, data: bytes) -> None:
            self.spec = data

        def write_report(self, data: bytes) -> None:
            self.report = data

    kept = Kept()
    job.write(kept)
    assert kept.spec == spec.bytes()
    with pytest.raises(ValueError, match="object-store URL"):
        job.write("s3://bucket/orders.mjob")
    with pytest.raises(TypeError, match="a file path, a callable"):
        job.write(3)
    assert moruna.Job.read(tmp_path / "orders.mjob").spec() == spec


def test_a_spec_refuses_what_it_cannot_hold(tasks, tmp_path: Path) -> None:
    """An IteratorSource, a closure, a param that is not data and a param naming a secret are
    each refused naming the task; the job still runs in this process."""
    _, tasks_a, tasks_b = tasks
    batch = pa.record_batch({"id": pa.array([1, 2], pa.int64())})
    streamed = moruna.Job("streamed")
    rows = streamed.add(moruna.IteratorSource([batch], schema=batch.schema), name="rows")
    streamed.add(moruna.ParquetSink(str(tmp_path / "streamed")), inputs=rows, name="out")
    with pytest.raises(moruna.ConfigError, match=r"job\['rows'\].*IteratorSource"):
        streamed.spec()
    report = moruna.run(streamed)
    assert report.exit["status"] == "completed"
    assert report.job_digest is None, "a job no spec can hold has no digest"

    def local(batch):
        return batch

    closure = moruna.Job("closure")
    rows = closure.add(tasks_a.Numbers(rows=10), name="rows")
    closure.add(moruna.ParquetSink(str(tmp_path / "c")), inputs=closure.add(local, inputs=rows))
    with pytest.raises(moruna.ConfigError, match=r"job\['local'\].*inside a function"):
        closure.spec()

    data = moruna.Job("data")
    rows = data.add(tasks_a.Numbers(rows=pa.scalar(3)), name="rows")  # type: ignore[arg-type]
    data.add(moruna.ParquetSink(str(tmp_path / "d")), inputs=rows, name="out")
    with pytest.raises(moruna.ConfigError, match=r"param 'rows' is a .*Scalar.*, not data"):
        data.spec()

    secret = moruna.Job("secret")
    rows = secret.add(tasks_a.Numbers(rows=3), name="rows")
    hush = secret.add(tasks_b.Secretive(api_token="x"), inputs=rows)
    secret.add(moruna.ParquetSink(str(tmp_path / "s")), inputs=hush, name="out")
    with pytest.raises(moruna.ConfigError, match="'api_token' names a secret"):
        secret.spec()


def test_needs_are_supplied_by_a_resolver(tasks, tmp_path: Path) -> None:
    """ports= as a mapping (the in-process adapter) or a host's resolver; a need nobody can
    supply is refused by name before anything is read."""
    _, tasks_a, tasks_b = tasks
    job = _job(tasks_a, tasks_b, tmp_path / "out")
    no_resolver = r"job\['enrich'\] needs 'warehouse.read', and no resolver"
    with pytest.raises(moruna.ConfigError, match=no_resolver):
        moruna.run(job)
    with pytest.raises(moruna.ConfigError, match="the resolver cannot supply"):
        moruna.run(job, ports={"warehouse": {}})
    assert not (tmp_path / "out").exists(), "refused before the run started"
    report = moruna.run(job, ports={"warehouse.read": {"orders": 5}})
    assert report.exit["status"] == "completed"
    back = pq.read_table(str(tmp_path / "out"))
    assert back.num_rows == ROWS
    assert set(back.column("orders").to_pylist()) == {10}
    assert set(back.column("tag").to_pylist()) == {"b"}
    assert set(back.column("loud").to_pylist()) >= {"ROW 0"}

    other = _job(tasks_a, tasks_b, tmp_path / "host")
    moruna.run(other, ports=tasks_a.Host())
    assert set(pq.read_table(str(tmp_path / "host")).column("orders").to_pylist()) == {42}


def test_declared_schemas_are_checked(tasks, tmp_path: Path) -> None:
    """job.check() compares what one task gives with what the next takes; a batch of another
    schema fails the run naming the task."""
    _, tasks_a, _ = tasks

    job = moruna.Job("mismatch")
    rows = job.add(tasks_a.Numbers(rows=10), name="rows")
    loud = job.add(tasks_a.Shout(), inputs=rows)
    again = job.add(tasks_a.Shout(), inputs=loud, name="again")
    job.add(moruna.ParquetSink(str(tmp_path / "m")), inputs=again, name="out")
    with pytest.raises(moruna.ConfigError, match=r"job\['again'\].inputs.in: `again` takes"):
        job.check()
    with pytest.raises(moruna.ConfigError, match="`again` takes .* and `shout` gives"):
        moruna.run(job)  # refused before anything starts

    fine = moruna.Job("fine")
    rows = fine.add(tasks_a.Numbers(rows=10), name="rows")
    fine.add(moruna.ParquetSink(str(tmp_path / "f")), inputs=fine.add(tasks_a.Shout(), inputs=rows))
    assert fine.check() is None
    broken = moruna.Job("broken")
    broken.add(moruna.ParquetSink(str(tmp_path / "b")), inputs="nowhere", name="out")
    with pytest.raises(moruna.ConfigError, match="nowhere"):
        broken.check()


def test_explain_lives_on_the_job(tasks, tmp_path: Path) -> None:
    """explain() plans an in-process job and a job read from a spec alike, without running."""
    _, tasks_a, tasks_b = tasks
    job = _job(tasks_a, tasks_b, tmp_path / "out")
    plan = job.explain()
    assert plan["steps"] and plan["totals"]
    read = moruna.Job.read(job.spec())
    assert read.explain()["steps"] == plan["steps"]
    with pytest.raises(moruna.PlanError):
        job.explain(planner="no-such-planner")


def test_the_report(tasks, tmp_path: Path) -> None:
    """The report names the spec's digest, the exit, every task, the resources and every sink
    with what it committed; its canonical bytes read back unchanged, and spec_to writes both
    documents."""
    _, tasks_a, tasks_b = tasks
    store: list[pa.RecordBatch] = []
    job = moruna.Job("two-sinks")
    rows = job.add(tasks_a.Numbers(rows=ROWS), name="rows")
    loud = job.add(tasks_a.Shout(), inputs=rows)
    job.add(tasks_a.Collect(), inputs=loud, name="kept")
    job.add(moruna.ParquetSink(str(tmp_path / "copy")), inputs=rows, name="copy")
    written: list[bytes] = []
    report = moruna.run(job, ports={"store": store}, spec_to=written.append, explain=True)

    spec = job.spec()
    assert written[0] == spec.bytes()
    assert written[1] == report.bytes()
    assert report.job_digest == spec.digest()
    # 0.6.6: the normalized execution fingerprint is reported beside the public digest.
    assert report.execution_fingerprint.startswith("sha256:")
    assert report.execution_fingerprint != report.job_digest
    assert report.exit == {"code": 0, "status": "completed", "diagnostic": None}
    assert len(report.run_id) == 32
    by_key = {t["key"]: t for t in report.tasks}
    assert set(by_key) == {"rows", "shout", "kept", "copy"}
    assert by_key["shout"]["rows_in"] == ROWS and by_key["shout"]["rows_out"] == ROWS
    assert by_key["shout"]["wall_ms"] >= 0 and by_key["shout"]["status"] == "ok"
    assert by_key["rows"]["rows_out"] == ROWS
    assert by_key["kept"]["rows_in"] == ROWS
    assert by_key["kept"]["needs"]["store"]["calls"] > 0, "measured through the resolver's object"
    assert by_key["kept"]["needs"]["store"]["bytes_in"] > 0
    assert report.resources["cpu_core_s"] >= 0 and report.resources["peak_memory_bytes"] > 0
    assert report.schema_version == "2.1" and report.moruna == moruna.__version__
    assert "stages" not in report.to_dict()
    sinks = {s["task"]: s for s in report.sinks}
    assert sinks["kept"]["snapshot_id"] == str(41 + len(store))
    assert sinks["kept"]["version"] == "v1"
    assert sinks["kept"]["target"] == "memory://collect"
    assert sinks["copy"]["snapshot_id"] is None
    assert sinks["copy"]["target"].endswith("/copy")
    assert sinks["copy"]["rows"] == ROWS and sinks["copy"]["files"]
    assert report.plan is not None and report.plan["steps"]
    assert sum(b.num_rows for b in store) == ROWS

    data = report.bytes()
    assert data == json.dumps(
        json.loads(data), sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode()
    again = moruna.RunReport.from_bytes(data)
    assert again.bytes() == data and again == report
    import hashlib  # noqa: PLC0415

    assert report.digest() == "sha256:" + hashlib.sha256(data).hexdigest()
    assert report.to_json() == data.decode()
    with pytest.raises(ValueError, match="canonical"):
        moruna.RunReport.from_bytes(json.dumps(json.loads(data), indent=1).encode())
    with pytest.raises(ValueError, match="not a run report"):
        moruna.RunReport.from_bytes(b'{"run_id": "x"}')
    with pytest.raises(AttributeError):
        report.run_id = "y"  # type: ignore[misc]
    assert "moruna run" in str(report) and len(str(report).splitlines()) <= 40

    path = tmp_path / "kept.mjob"
    moruna.run(job, ports={"store": []}, spec_to=path)
    assert path.read_bytes() == spec.bytes()
    assert moruna.RunReport.from_bytes(tmp_path / "kept.report.json").job_digest == spec.digest()


def test_a_committing_sink_names_its_snapshot(tasks, tmp_path: Path) -> None:
    """A sink's finish() that returns a commit without a snapshot is refused, naming it."""
    _, tasks_a, _ = tasks

    class Careless(moruna.Sink):
        def write(self, batch):
            pass

        def finish(self):
            return {"version": "v2"}

    with pytest.raises(moruna.MorunaError, match="without `snapshot_id`"):
        moruna.run(tasks_a.Numbers(rows=10), [], Careless())


def test_condition_a_several_sinks(tasks, tmp_path: Path) -> None:
    """(a) One job writes several sinks, each its own rows, each named in the report; the
    spec read back is a draft, refused for running, naming every unbuilt task."""
    _, tasks_a, _ = tasks
    job = moruna.Job("fan")
    rows = job.add(tasks_a.Numbers(rows=ROWS), name="rows")
    loud = job.add(tasks_a.Shout(), inputs=rows)
    total = job.add(tasks_a.Total(), inputs=rows)
    for name, upstream in (("raw", rows), ("loud-out", loud), ("total-out", total)):
        job.add(moruna.ParquetSink(str(tmp_path / name)), inputs=upstream, name=name)
    report = moruna.run(job)
    assert [s["task"] for s in report.sinks] == ["raw", "loud-out", "total-out"]
    assert pq.read_table(str(tmp_path / "raw")).num_rows == ROWS
    assert "loud" in pq.read_table(str(tmp_path / "loud-out")).column_names
    # (c) a stateful function's finish gives downstream.
    assert pq.read_table(str(tmp_path / "total-out")).column("rows").to_pylist() == [ROWS]
    with pytest.raises(moruna.ConfigError, match="draft") as refused:
        moruna.run(moruna.Job.read(job.spec()))
    for name in ("rows", "shout", "total"):
        assert f"job['{name}']" in str(refused.value)


def test_condition_b_a_docstring_edit_keeps_the_image_a_code_edit_does_not(
    tasks, tmp_path: Path
) -> None:
    """(b) Functions of two packages in one job, each its own key; an edit of a docstring
    keeps every key (and the description follows the code), an edit of code changes its own."""
    root, tasks_a, tasks_b = tasks
    before = json.loads(_job(tasks_a, tasks_b, tmp_path / "out").spec().to_json())
    source = root / "tasks_b" / "__init__.py"
    original = source.read_text()
    try:
        source.write_text(original.replace(
            "class Tag(moruna.Function):", 'class Tag(moruna.Function):\n    """Tags rows."""'
        ))  # fmt: skip
        import importlib  # noqa: PLC0415

        importlib.reload(tasks_b)
        after = json.loads(_job(tasks_a, tasks_b, tmp_path / "out").spec().to_json())
        key = lambda doc, name: [t for t in doc["tasks"] if t["id"] == name][0]  # noqa: E731
        assert key(after, "tag")["settings"]["image"] == key(before, "tag")["settings"]["image"]
        assert key(after, "tag")["definition"]["title"] == "Tags rows."
        source.write_text(original.replace("self.label = label", "self.label = label + ''"))
        importlib.reload(tasks_b)
        changed = json.loads(_job(tasks_a, tasks_b, tmp_path / "out").spec().to_json())
        assert (key(changed, "tag")["settings"]["image"]["build_key"]
                != key(before, "tag")["settings"]["image"]["build_key"])  # fmt: skip
        assert key(changed, "enrich")["settings"] == key(before, "enrich")["settings"]
    finally:
        source.write_text(original)
        importlib.reload(tasks_b)


def test_the_command_refuses_a_draft_and_explains_it(tasks, tmp_path: Path) -> None:
    """`moruna run JOB` of a spec whose images are not built is refused naming every unbuilt
    task, before anything runs; `moruna explain` plans it."""
    root, tasks_a, tasks_b = tasks
    spec = tmp_path / "orders.mjob"
    _job(tasks_a, tasks_b, tmp_path / "cli").write(spec)
    env = dict(os.environ, PYTHONPATH=os.pathsep.join([str(root), *sys.path]))
    refused = subprocess.run(
        [sys.executable, "-m", "moruna", "run", str(spec), "--resolver", "tasks_a:Host"],
        env=env, capture_output=True, text=True, timeout=300, check=False, cwd=tmp_path,
    )
    assert refused.returncode == 2, refused.stderr
    assert "draft" in refused.stderr
    for name in ("rows", "shout", "enrich", "tag"):
        assert f"`{name}`" in refused.stderr, refused.stderr
    assert not (tmp_path / "cli").exists()
    explained = subprocess.run(
        [sys.executable, "-m", "moruna", "explain", str(spec)],
        env=env, capture_output=True, text=True, timeout=300, check=False, cwd=tmp_path,
    )
    assert explained.returncode == 0, explained.stderr
    assert json.loads(explained.stdout)["steps"] == moruna.Job.read(spec).explain()["steps"]


def test_run_arguments_are_declared_by_the_spec_and_given_by_each_run(
    tasks, tmp_path: Path
) -> None:
    """The spec holds the declaration, never a value; a missing, ill-typed or undeclared
    argument is refused by name before the run; tasks read self.args; the report records them."""
    _, tasks_a, _ = tasks

    job = moruna.Job("windowed")
    job.arg("start", "timestamp")
    job.arg("limit", "int")
    rows = job.add(tasks_a.Numbers(rows=10), name="rows")
    window = job.add(tasks_a.Window(), inputs=rows)
    job.add(moruna.ParquetSink(str(tmp_path / "w")), inputs=window, name="out")
    with pytest.raises(ValueError, match="one of string, int"):
        job.arg("when", "datetime")
    declared = json.loads(job.spec().to_json())["args"]
    assert declared == [{"name": "limit", "type": "int"}, {"name": "start", "type": "timestamp"}]
    with pytest.raises(moruna.ConfigError) as refused:
        moruna.run(job, args={"start": "yesterday", "colour": "red"})
    said = str(refused.value)
    assert "args.limit: the job declares it (int)" in said
    assert "args.start: \"yesterday\" is not a timestamp" in said
    assert "args.colour: the job declares no such argument" in said
    assert not (tmp_path / "w").exists()
    report = moruna.run(job, args={"start": "2026-11-01T00:00:00Z", "limit": 5})
    assert report.args == {"start": "2026-11-01T00:00:00Z", "limit": 5}
    back = pq.read_table(str(tmp_path / "w"))
    assert set(back.column("start").to_pylist()) == {"2026-11-01T00:00:00Z"}
    assert moruna.Job.read(job.spec()).args == {"limit": "int", "start": "timestamp"}


def test_a_timestamp_argument_may_be_a_datetime_and_every_task_reads_its_text(
    tasks, tmp_path: Path
) -> None:
    """A ``datetime`` with its offset is a timestamp argument and a ``date`` a date one, written
    as RFC 3339; every task reads an argument as its JSON value, as it would inside an image.
    0.6.3 wrote ``str(datetime)``, with a space, and refused it as not a timestamp."""
    import datetime

    _, tasks_a, _ = tasks
    job = moruna.Job("dated")
    job.arg("start", "timestamp")
    job.arg("day", "date")
    rows = job.add(tasks_a.Numbers(rows=3), name="rows")
    window = job.add(tasks_a.Window(), inputs=rows)
    job.add(moruna.ParquetSink(str(tmp_path / "d")), inputs=window, name="out")
    start = datetime.datetime(2026, 11, 1, 9, 30, tzinfo=datetime.timezone.utc)
    report = moruna.run(job, args={"start": start, "day": datetime.date(2026, 11, 1)})
    assert report.args == {"start": "2026-11-01T09:30:00+00:00", "day": "2026-11-01"}
    back = pq.read_table(str(tmp_path / "d"))
    assert set(back.column("start").to_pylist()) == {"2026-11-01T09:30:00+00:00"}
    with pytest.raises(TypeError, match="a run argument is a string, number, bool, date or datetime"):
        moruna.run(job, args={"start": object(), "day": "2026-11-01"})


RECORD = pa.struct([("id", pa.string()), ("seen", pa.timestamp("us", tz="UTC"))])
RECORDS = pa.schema([("records", pa.list_(RECORD))])


class Records(moruna.Function):
    """Each row's records."""

    gives = RECORDS

    def __call__(self, batch: pa.RecordBatch) -> pa.RecordBatch:
        return pa.RecordBatch.from_pylist(
            [{"records": [{"id": "a", "seen": None}]}] * batch.num_rows, schema=RECORDS
        )


def test_a_declaration_names_a_list_of_structs() -> None:
    """``takes`` and ``gives`` may hold ``list<struct<...>>`` (0.6.3 refused it as an unknown
    type), and every batch the task gives is checked against it."""
    kept: list[pa.RecordBatch] = []

    class Keep(moruna.Sink):
        """Keeps the batches."""

        takes = RECORDS

        def write(self, batch: pa.RecordBatch) -> None:
            kept.append(batch)

    job = moruna.Job("records")
    numbers = pa.schema([("n", pa.int64())])
    batches = iter([pa.record_batch([pa.array([1, 2])], schema=numbers)])
    rows = job.add(moruna.IteratorSource(batches, schema=numbers), name="rows")
    job.add(Keep(), inputs=job.add(Records(), inputs=rows, name="records"), name="out")
    moruna.run(job, checkpoint=False)
    assert pa.Table.from_batches(kept).schema.field("records").type == pa.list_(RECORD)


def test_an_image_moruna_did_not_build_is_external(tasks, tmp_path: Path) -> None:
    _, tasks_a, _ = tasks
    digest = "sha256:" + "ab" * 32
    job = moruna.Job("byo")
    rows = job.add(tasks_a.Numbers(rows=10), name="rows")
    scored = job.add(
        moruna.image(f"registry.example.com/team/scorer@{digest}", entry="score",
                     params={"column": "text"}, needs={"model": "the scoring model"}),
        inputs=rows, name="score",
    )  # fmt: skip
    job.add(moruna.ParquetSink(str(tmp_path / "b")), inputs=scored, name="out")
    status = {row["task"]: row for row in job.status()}
    assert status["score"]["state"] == "external"
    assert status["score"]["location"] == f"registry.example.com/team/scorer@{digest}"
    assert status["rows"]["state"] == "not built"
    assert job.explain()["images"] == job.status()
    with pytest.raises(ValueError, match="HOST/REPOSITORY@sha256"):
        moruna.image("scorer:latest")
    spec = json.loads(job.spec().to_json())
    score = [t for t in spec["tasks"] if t["id"] == "score"][0]
    assert score["settings"]["image"] == {
        "build_key": None, "digest": digest,
        "location": f"registry.example.com/team/scorer@{digest}",
    }  # fmt: skip
    assert score["definition"]["requires"][0]["description"] == "the scoring model"


def test_a_document_with_run_settings(tmp_path: Path) -> None:
    """A document that also sets run settings is read with them: its spec and its plan keep
    them, and moruna.run refuses it, naming them, for they are moruna.run's arguments."""
    doc = {
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": "file:///in.parquet"},
        "kernels": [{"kind": "std", "name": "select", "args": {"columns": ["a"]}}],
        "sink": {"kind": "parquet", "url": "file:///out/"},
        "budget": {"memory_bytes": 1 << 30},
    }
    path = tmp_path / "job.json"
    path.write_text(json.dumps(doc))
    job = moruna.Job.read(path)
    assert job.settings == ("budget",)
    assert "settings: budget" in repr(job)
    plan = job.explain()
    assert plan["steps"][0]["resources"]["memory_start_bytes"] == 1 << 30
    again = moruna.Job.read(job.spec())
    assert again.settings == ("budget",) and again.spec() == job.spec()
    with pytest.raises(moruna.ConfigError, match="sets budget"):
        moruna.run(job)
