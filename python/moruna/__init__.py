"""Moruna: a memory-aware runtime for data and tensor pipelines (component 12, SDD 12).

Constructors for each source and sink, one decorator and one function (and two base classes,
``moruna.Source`` and ``moruna.Sink``, for a source or a sink of your own)::

    import moruna, pyarrow as pa

    @moruna.kernel
    def normalise(batch: pa.RecordBatch) -> pa.RecordBatch:
        ...

    report = moruna.run(moruna.ParquetSource("s3://bucket/in/", columns=["id", "text"]),
                       normalise,
                       moruna.ParquetSink("s3://bucket/out/"))
    print(report)

No parameter beyond the budget is required: inside a cgroup, ``moruna.run(source, kernels, sink)``
is a complete call, and outside one ``budget=`` is the only argument a user may need (PY-I3).

This module holds no orchestration: the runtime facade wires the components in Rust, and what is
written here is argument translation and nothing else (a, the anti-patterns of l).
"""

from __future__ import annotations

import importlib.util as _importlib_util
import json
import os
from collections.abc import Sequence
from pathlib import Path
from typing import Any

if _importlib_util.find_spec("pyarrow") is None:  # h, failures
    raise ImportError(
        "moruna needs pyarrow >= 17: pip install 'pyarrow>=17'. It is the format the runtime "
        "hands Python kernels and the one it takes back, with no copy in between."
    )

from moruna import _core, _declare

# The classes a user subclasses for a source or a sink of their own (SDD 07 e.6, 08 f.10).
# A function package under its own runtime on this machine (SDD 16 d.3, 12 f.8).
from moruna._core import (
    ArrowIpcSink,
    ArrowIpcSource,
    BudgetError,
    Cancelled,
    ConfigError,
    CostSession,
    EnginePorts,
    IoError,
    IteratorSource,
    KernelError,
    KernelSpec,
    MorunaError,
    PackageKernel,
    ParquetSink,
    ParquetSource,
    PlanError,
    ResumeError,
    RunReport,
    Sink,
    Source,
    Split,
    StdKernel,
    TensorSink,
    TensorSource,
    VortexSink,
    VortexSource,
    inspect_host,
    load,
)
from moruna._graph import Graph, GraphTask
from moruna._kinds import DeclaredKind, KindCall, kind, kinds

__version__: str = _core.__version__

__all__ = [
    "MorunaError",
    "ArrowIpcSink",
    "ArrowIpcSource",
    "BudgetError",
    "Cancelled",
    "ConfigError",
    "CostSession",
    "EnginePorts",
    "IoError",
    "IteratorSource",
    "KernelError",
    "KernelSpec",
    "ParquetSink",
    "ParquetSource",
    "PlanError",
    "ResumeError",
    "RunReport",
    "StdKernel",
    "TensorSink",
    "TensorSource",
    "VortexSink",
    "VortexSource",
    "__version__",
    "build",
    "explain",
    "inspect_host",
    "kernel",
    "polars",
    "read_events",
    "run",
    "std",
]

# A source or a sink of the user's own (SDD 07 e.6, 08 f.10).
__all__ += ["Sink", "Source", "Split"]

# A run as a DAG of named tasks (SDD 12 d.2, SDD 22 d.4).
__all__ += ["Graph", "GraphTask"]

# A function package (SDD 16 d.3).
__all__ += ["PackageKernel", "load"]

# Checking a function and warming it up: what it costs (MH 4.9, AR section 6).
__all__ += ["check"]
# A kind the program serves itself, and the listing of every kind (SDD 18 e.3, e.4).
__all__ += ["DeclaredKind", "KindCall", "kind", "kinds"]


def run(
    source: Any = None,
    kernels: Any = None,
    sink: Any = None,
    *,
    graph: Graph | None = None,
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
    """Run one pipeline from ``source`` through ``kernels`` to ``sink``, or the DAG ``graph``
    (a ``moruna.Graph``), and return its report.

    The chain is the graph with one path; the two are built into the same job. A graph's
    unknown task names, cycles and outputs no task reads are refused with ``ConfigError``
    naming every one of them, before anything starts.

    ``kernels`` may be one kernel or a list of them, in stage order; a plain callable is wrapped
    as if it had been decorated with ``@moruna.kernel`` and its defaults, and a task of a
    ``@moruna.kind`` (the kind called with its settings) is run by handing each batch to the
    callable that serves it. Every keyword argument is
    optional, and each is range checked once, here, against the configuration table of the design
    (PY-I10): a value outside its range is clamped to the nearest bound and the clamp is named in
    ``report.notes``.

    ``resume`` takes ``"auto"``, a run id (32 hexadecimal characters) or a path to a
    ``manifest.json``, and is the only parameter a user meets after a failure (f.7).

    ``cost`` shares the same configured observation store/model as registration warm-up.
    On the Moruna kernel, omitted ``engine_ports`` consumes the canonical startup
    grants and assigned run identity; missing native authority refuses the run.
    ``engine_ports`` is supplied by a native host with actual scoped retention/checkpoint
    authority. Python cannot create storage grants; missing capabilities refuse explicitly.

    Raises ``moruna.ConfigError`` when a Python kernel would run under a GIL interpreter without
    ``allow_gil=True`` (PY-I4), ``moruna.PlanError`` when the sink would write where the source
    reads (f.3), and ``moruna.Cancelled``, with the partial report attached, on Ctrl-C (PY-I6).
    """
    return _core.run(
        source,
        kernels,
        sink,
        graph=graph,
        budget=budget,
        cpu=cpu,
        trace=trace,
        staging_dir=staging_dir,
        staging_limit=staging_limit,
        on_error=on_error,
        ordered=ordered,
        sizer=sizer,
        profiles_dir=profiles_dir,
        storage=storage,
        host_profile=host_profile,
        allow_gil=allow_gil,
        checkpoint=checkpoint,
        checkpoint_interval=checkpoint_interval,
        keep_checkpoint=keep_checkpoint,
        resume=resume,
        cost=cost,
        engine_ports=engine_ports,
    )


def build(
    fn: Any,
    *,
    out: str | os.PathLike[str] = "moruna-image",
    tag: str | None = None,
    platform: str | None = None,
    lock: str | os.PathLike[str] | None = None,
    files: Sequence[str | os.PathLike[str]] = (),
    runtime_layout: str | os.PathLike[str] | None = None,
    runtime_reference: str | None = None,
    cache: str | os.PathLike[str] | None = None,
    index_url: str | None = None,
    find_links: Sequence[str] = (),
    no_index: bool = False,
) -> Any:
    """Build the kernel ``fn`` as an OCI image of four layers and return what was built (SDD 30).

    The same as ``moruna build <module>:<function>``. ``fn`` is a ``@moruna.kernel`` (or a plain
    function) bound to a name at the top level of its module. The image is written to the OCI
    layout directory ``out`` under the name ``tag`` (default ``<function>:latest``): the
    runtime (``python:3.14-slim``'s own layers), the dependencies the project's ``uv.lock`` or
    ``requirements.lock`` pins (``lock``, or the nearest above the module), the shim, and the
    module with the ``files`` it names. ``platform`` is ``linux/aarch64`` or ``linux/x86_64``
    (default: this machine's on Linux, aarch64 on a Mac). The result has ``digest`` (the image's),
    ``reference``, ``layout``, and ``layers``, each with its ``role``, ``blob.digest`` and
    ``blob.size``. Raises ``moruna.ConfigError`` naming the cause when the build is refused: no
    lock, an unpinned or unhashed requirement, a wheel the platform has none of.
    """
    from moruna import _build  # noqa: PLC0415, the builder is needed by `build` alone

    try:
        return _build.build(
            fn,
            out=out,
            tag=tag,
            platform=platform,
            lock=lock,
            files=files,
            runtime_layout=runtime_layout,
            runtime_reference=runtime_reference,
            cache=cache,
            index_url=index_url,
            find_links=find_links,
            no_index=no_index,
            version=__version__,
        )
    except _build.Refused as exc:
        raise ConfigError(f"build refused: {exc}") from exc


def explain(
    job: str | os.PathLike[str] | bytes,
    *,
    planner: str | None = None,
) -> dict[str, Any]:
    """The plan Moruna would run the job document ``job`` by, as a dict, without running it.

    ``job`` is a path to a job document or its bytes: version 2's FlatBuffers or version 1's
    JSON. The plan lists its steps in the order they start, each with the tasks it runs, the steps
    it waits for, its group (the steps that may run beside it), the resources it is given and its
    estimated cost; ``groups`` lists the groups in order, and ``totals`` gives the job's finish,
    critical path, peak memory and the resources it holds. The same document planned twice has
    the same step ids, which the events of its run name (``step``).

    ``planner`` names the planner; the default is ``"levels"``. Raises ``moruna.ConfigError``
    naming the field when the document or its kinds are refused, and ``moruna.PlanError`` for an
    unknown planner.
    """
    document = bytes(job) if isinstance(job, (bytes, bytearray)) else Path(job).read_bytes()
    plan: dict[str, Any] = json.loads(_core.explain(document, planner=planner))
    return plan


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


def kernel(
    fn: Any = None,
    *,
    stateful: bool = False,
    instances: int = 1,
    device_memory: bool = False,
    accepts: str = "table",
    tier: str = "host",
    releases_gil: bool | None = None,
    expected_amplification: float | None = None,
    preferred_rows: int | None = None,
    resume: str = "reinit",
    state_bytes: int | None = None,
    input_schema: Any = None,
    output_schema: Any = None,
    lockfile: Any = None,
    memory_guard: bool = True,
) -> Any:
    """Turn a callable or a class instance into a kernel.

    ``stateful=False`` (the default) takes a plain callable ``f(batch) -> batch``.
    ``stateful=True`` takes an object with ``setup(self, ctx) -> state`` and
    ``__call__(self, state, batch) -> batch``, and optionally ``footprint(self, state)`` and
    ``finish(self, state)``; ``ctx`` exposes ``.instance`` and ``.device``. ``finish`` is called
    once per instance when the kernel's input has ended, after its last ``__call__``; a batch it
    returns goes downstream like any other, ``None`` gives nothing, and an exception ends the run
    naming the kernel. A kernel that must see the whole run before it gives anything (one that
    collects every batch) gives its result there, and takes ``instances=1``: with more, each
    instance sees only the batches it was given and each instance's ``finish`` is called.
    ``resume="checkpoint"`` also requires ``checkpoint(self, state) -> bytes`` and
    ``restore(self, ctx, data) -> state``, and is refused at decoration when either is missing.

    ``input_schema`` and ``output_schema`` declare what the kernel takes and gives, which makes
    it checkable (``python -m moruna check``): a ``pyarrow.Schema`` is exact, a mapping of
    column to type is a subset, and an output mapping whose keys are only ``adds``, ``drops`` and
    ``changes`` is relative to the input. ``lockfile`` (a path or bytes) is folded into the
    fingerprint, so a kernel whose dependencies change is a different kernel.

    ``memory_guard`` (default on): while the kernel runs, a single request for memory (a NumPy
    array, a Python object) that would take the process past its memory ceiling is refused
    before the memory exists; the kernel sees ``MemoryError`` and the run ends with a budget
    error naming the kernel and the request, rather than being killed. ``memory_guard=False``
    turns refusal off for this kernel; what it asks for is still counted and reported per
    kernel in the run report's ``stages[..]["alloc"]``.

    A function annotated ``pl.DataFrame -> pl.DataFrame`` (or ``pl.LazyFrame``) is a Polars
    kernel: the batch reaches it as a Polars frame through the Arrow C data interface, and the
    frame it returns comes back the same way.

    Usable bare (``@moruna.kernel``) or with arguments (``@moruna.kernel(stateful=True)``).
    """

    def decorate(obj: Any) -> KernelSpec:
        origin = None
        frame = None if stateful else _declare.polars_signature(obj)
        if frame is not None:
            origin, obj = obj, _declare.polars_callable(obj, frame)
        return _core.build_kernel(
            obj,
            stateful=stateful,
            instances=instances,
            device_memory=device_memory,
            accepts=accepts,
            tier=tier,
            releases_gil=releases_gil,
            expected_amplification=expected_amplification,
            preferred_rows=preferred_rows,
            resume=resume,
            state_bytes=state_bytes,
            input_schema=_declare.declaration(input_schema, "input_schema"),
            output_schema=_declare.declaration(output_schema, "output_schema"),
            lockfile=_declare.lockfile_bytes(lockfile),
            origin=origin,
            memory_guard=memory_guard,
        )

    if fn is None:
        return decorate
    return decorate(fn)


def polars(fn: Any, *, accepts: str = "table", **declarations: Any) -> KernelSpec:
    """Wrap a function from a Polars frame to a Polars frame as a stateless kernel (MH 4.9).

    The same path the decorator takes for a function annotated ``pl.DataFrame ->
    pl.DataFrame``, for a function that carries no annotations: the batch enters Polars through
    the Arrow C data interface and the result leaves the same way. ``declarations`` are the
    decorator's ``input_schema``, ``output_schema`` and ``lockfile``.
    """
    kind = _declare.polars_signature(fn) or "eager"
    wrapped = _declare.polars_callable(fn, kind)
    return _core.build_kernel(
        wrapped,
        accepts=accepts,
        input_schema=_declare.declaration(declarations.get("input_schema"), "input_schema"),
        output_schema=_declare.declaration(declarations.get("output_schema"), "output_schema"),
        lockfile=_declare.lockfile_bytes(declarations.get("lockfile")),
        origin=fn,
    )


from moruna import std  # noqa: E402, the standard kernels need the names above
from moruna._check import check  # noqa: E402, it finds kernels with the decorator above

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
