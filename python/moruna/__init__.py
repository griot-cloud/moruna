"""Moruna: a memory-aware runtime for data and tensor pipelines (component 12, SDD 12).

A run is a job: sources, functions and sinks joined by name::

    import moruna, pyarrow as pa

    @moruna.function
    def normalise(batch: pa.RecordBatch) -> pa.RecordBatch:
        ...

    job = moruna.Job("normalise")
    rows = job.add(moruna.ParquetSource("s3://bucket/in/", columns=["id", "text"]), name="rows")
    out = job.add(normalise, inputs=rows)
    job.add(moruna.ParquetSink("s3://bucket/out/"), inputs=out, name="out")
    report = moruna.run(job)
    print(report)

``moruna.run(source, functions, sink)`` is the job of one path. No parameter beyond the budget
is required: inside a cgroup a job is a complete call, and outside one ``budget=`` is the only
argument a user may need (PY-I3).

This module holds no orchestration: the runtime facade wires the components in Rust, and what is
written here is argument translation and nothing else (a, the anti-patterns of l).
"""

from __future__ import annotations

import importlib.util as _importlib_util
import json
import os
from pathlib import Path
from typing import Any

if _importlib_util.find_spec("pyarrow") is None:  # h, failures
    raise ImportError(
        "moruna needs pyarrow >= 17: pip install 'pyarrow>=17'. It is the format the runtime "
        "hands Python functions and the one it takes back, with no copy in between."
    )

from moruna import _core, _declare
from moruna._core import (
    ArrowIpcSink,
    ArrowIpcSource,
    BudgetError,
    Cancelled,
    ConfigError,
    CostSession,
    EnginePorts,
    FunctionError,
    IoError,
    IteratorSource,
    MorunaError,
    ParquetSink,
    ParquetSource,
    PlanError,
    ResumeError,
    Split,
    TensorSink,
    TensorSource,
    VortexSink,
    VortexSource,
    inspect_host,
)
from moruna._job import Function, Job, JobSpec, Sink, Source, Task, function
from moruna._kinds import DeclaredKind, KindCall, kind, kinds
from moruna._ports import BytesWriter, FileWriter, Ports, Resolver, Writer
from moruna._report import RunReport

__version__: str = _core.__version__

__all__ = [
    "ArrowIpcSink",
    "ArrowIpcSource",
    "BudgetError",
    "BytesWriter",
    "Cancelled",
    "ConfigError",
    "CostSession",
    "DeclaredKind",
    "EnginePorts",
    "FileWriter",
    "Function",
    "FunctionError",
    "IoError",
    "IteratorSource",
    "Job",
    "JobSpec",
    "KindCall",
    "MorunaError",
    "ParquetSink",
    "ParquetSource",
    "PlanError",
    "Ports",
    "Resolver",
    "ResumeError",
    "RunReport",
    "Sink",
    "Source",
    "Split",
    "Task",
    "TensorSink",
    "TensorSource",
    "VortexSink",
    "VortexSource",
    "Writer",
    "__version__",
    "check",
    "function",
    "inspect_host",
    "kind",
    "kinds",
    "load",
    "polars",
    "read_events",
    "run",
    "std",
]


def run(
    job: Any,
    functions: Any = None,
    sink: Any = None,
    *,
    ports: Any = None,
    spec_to: Any = None,
    explain: bool = False,
    budget: int | str | None = None,
    cpu: float | None = None,
    trace: str | None = None,
    staging_dir: str | None = None,
    staging_limit: int | str | None = None,
    on_error: str | tuple[str, int] = "terminate",
    ordered: bool = False,
    sizer: str = "rule",
    profiles_dir: str | None = None,
    storage: dict[str, Any] | None = None,
    host_profile: Any = None,
    allow_gil: bool = False,
    checkpoint: bool = True,
    checkpoint_interval: float = 5.0,
    keep_checkpoint: bool = False,
    resume: str | None = None,
    cost: CostSession | None = None,
    engine_ports: EnginePorts | None = None,
) -> RunReport:
    """Run the ``moruna.Job`` ``job``, or the job of one path ``moruna.run(source, functions,
    sink)``, and return its ``moruna.RunReport``.

    ``ports`` supplies what the job's tasks need: a mapping of need to object (the in-process
    adapter, ``moruna.Ports``) or a ``moruna.Resolver``. Every need is resolved before the run
    starts, and one the resolver cannot supply is refused by name. ``spec_to`` (a file path, a
    callable taking bytes, or a ``moruna.Writer``) gets the job's spec before the run starts and
    the report's bytes after it ends. ``explain=True`` puts the plan the run started from in the
    report's ``plan``.

    A job's unknown task names, cycles and outputs no task reads are refused with
    ``ConfigError`` naming every one of them, before anything starts. Every other keyword is
    optional, and each is range checked once, here, against the configuration table of the
    design (PY-I10): a value outside its range is clamped to the nearest bound and the clamp is
    named in ``report.notes``.

    ``storage`` names the object store (``endpoint``, ``region``, ``bucket``, ``account``,
    ``container``, ``local_root``, ``allow_http``) and supplies its credentials
    (``access_key_id``, ``secret_access_key``, ``session_token``, ``service_account_path``,
    ``service_account_json``, ``access_key``), which reach the run and never its spec; settings
    not given are read where the run runs, from the provider's usual environment variables.

    ``resume`` takes ``"auto"``, a run id (32 hexadecimal characters) or a path to a
    ``manifest.json``, and is the only parameter a user meets after a failure (f.7).

    ``cost`` shares the same configured observation store/model as registration warm-up.
    ``engine_ports`` is supplied by a native host with actual scoped retention/checkpoint
    authority; missing capabilities refuse explicitly.

    Raises ``moruna.ConfigError`` when a Python function would run under a GIL interpreter
    without ``allow_gil=True`` (PY-I4), ``moruna.PlanError`` when a sink would write where a
    source reads (f.3), and ``moruna.Cancelled``, with the partial report attached, on Ctrl-C
    (PY-I6).
    """
    from moruna import _job  # noqa: PLC0415

    if isinstance(job, Job):
        if functions is not None or sink is not None:
            raise TypeError("moruna.run takes a moruna.Job, or a source, functions and a sink")
    elif functions is None or sink is None:
        raise TypeError("moruna.run takes a moruna.Job, or a source, functions and a sink")
    else:
        job = _job.chain(job, functions, sink)
    return _job.run_job(
        job,
        ports=ports,
        spec_to=spec_to,
        explain=explain,
        options={
            "budget": budget,
            "cpu": cpu,
            "trace": trace,
            "staging_dir": staging_dir,
            "staging_limit": staging_limit,
            "on_error": on_error,
            "ordered": ordered,
            "sizer": sizer,
            "profiles_dir": profiles_dir,
            "storage": storage,
            "host_profile": host_profile,
            "allow_gil": allow_gil,
            "checkpoint": checkpoint,
            "checkpoint_interval": checkpoint_interval,
            "keep_checkpoint": keep_checkpoint,
            "resume": resume,
            "cost": cost,
            "engine_ports": engine_ports,
        },
    )


def load(path: str | os.PathLike[str], **options: Any) -> Function:
    """A function package (SDD 16 d.3) opened under its own runtime on this machine, as a
    ``moruna.Function`` a job runs (12 f.8); ``options`` are ``_core.load``'s."""
    from moruna._job import _wrap  # noqa: PLC0415

    return _wrap(_core.load(os.fspath(path), **options))


def read_events(source: str | os.PathLike[str] | bytes) -> list[dict[str, Any]]:
    """The events a run wrote as records, as dicts.

    ``source`` is a path to a file of records or its bytes: a run's events file in its output
    store (``_events.mrec`` in the attempt's directory, written when the run had no peer), or a
    segment of one producer's output (``<task>/<first seq>.mrec``). Every event is a dict with
    the fields version 6 of the host protocol's ``event`` had (``type``, ``v``, ``attempt``,
    ``start``, ``seq``, ``t_us``, ``kind`` and the fact's own), in the order they were written;
    a record of output (``kind`` ``log``) also carries its ``id``.

    Raises ``ValueError`` when the bytes are not whole events, naming where they stop being one.
    """
    data = bytes(source) if isinstance(source, (bytes, bytearray)) else Path(source).read_bytes()
    events: list[dict[str, Any]] = json.loads(_core.read_events(data))
    return events


def polars(fn: Any, *, accepts: str = "table", **declarations: Any) -> Function:
    """Wrap a function from a Polars frame to a Polars frame as a stateless function (MH 4.9).

    The same path the decorator takes for a function annotated ``pl.DataFrame ->
    pl.DataFrame``, for a function that carries no annotations: the batch enters Polars through
    the Arrow C data interface and the result leaves the same way. ``declarations`` are the
    decorator's ``input_schema``, ``output_schema`` and ``lockfile``.
    """
    from moruna._job import _wrap  # noqa: PLC0415

    kind = _declare.polars_signature(fn) or "eager"
    wrapped = _declare.polars_callable(fn, kind)
    return _wrap(_core.build_kernel(
        wrapped,
        accepts=accepts,
        input_schema=_declare.declaration(declarations.get("input_schema"), "input_schema"),
        output_schema=_declare.declaration(declarations.get("output_schema"), "output_schema"),
        lockfile=_declare.lockfile_bytes(declarations.get("lockfile")),
        origin=fn,
    ))


from moruna import std  # noqa: E402, the standard functions need the names above
from moruna._check import check  # noqa: E402, it finds functions with the decorator above

from moruna._engine import (  # noqa: E402, native handles are defined above
    ArtifactChunk, ArtifactManifest, LogicalOutputIdentity, ObjectIdentity, ObjectVersion,
    PinnedObject, RetainedOutput, Registration, RegistrationRefused, artifact_chunk, artifact_manifest,
    checkpoint_receipt, checkpoint_pending, prepare_registration, retained_outputs,
    restore_checkpoint,
)
__all__ += ["Registration", "RegistrationRefused", "prepare_registration", "retained_outputs",
            "artifact_manifest", "artifact_chunk", "checkpoint_receipt", "checkpoint_pending",
            "restore_checkpoint"]

__all__ += ["ArtifactChunk", "ArtifactManifest", "LogicalOutputIdentity", "ObjectIdentity",
            "ObjectVersion", "PinnedObject", "RetainedOutput"]
