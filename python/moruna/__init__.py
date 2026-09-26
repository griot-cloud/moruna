"""Moruna: a memory-aware runtime for data and tensor pipelines (component 12, SDD 12).

Four constructors, one decorator and one function::

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
from typing import Any

if _importlib_util.find_spec("pyarrow") is None:  # h, failures
    raise ImportError(
        "moruna needs pyarrow >= 17: pip install 'pyarrow>=17'. It is the format the runtime "
        "hands Python kernels and the one it takes back, with no copy in between."
    )

from moruna import _core, _declare
from moruna._core import (
    MorunaError,
    ArrowIpcSink,
    BudgetError,
    Cancelled,
    ConfigError,
    IoError,
    IteratorSource,
    KernelError,
    KernelSpec,
    ParquetSink,
    ParquetSource,
    PlanError,
    ResumeError,
    RunReport,
    StdKernel,
    TensorSink,
    TensorSource,
    inspect_host,
)

__version__: str = _core.__version__

__all__ = [
    "MorunaError",
    "ArrowIpcSink",
    "BudgetError",
    "Cancelled",
    "ConfigError",
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
    "__version__",
    "inspect_host",
    "kernel",
    "polars",
    "run",
    "std",
]


def run(
    source: Any,
    kernels: Any,
    sink: Any,
    *,
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
) -> RunReport:
    """Run one pipeline from ``source`` through ``kernels`` to ``sink`` and return its report.

    ``kernels`` may be one kernel or a list of them, in stage order; a plain callable is wrapped
    as if it had been decorated with ``@moruna.kernel`` and its defaults. Every keyword argument is
    optional, and each is range checked once, here, against the configuration table of the design
    (PY-I10): a value outside its range is clamped to the nearest bound and the clamp is named in
    ``report.notes``.

    ``resume`` takes ``"auto"``, a run id (32 hexadecimal characters) or a path to a
    ``manifest.json``, and is the only parameter a user meets after a failure (f.7).

    Raises ``moruna.ConfigError`` when a Python kernel would run under a GIL interpreter without
    ``allow_gil=True`` (PY-I4), ``moruna.PlanError`` when the sink would write where the source
    reads (f.3), and ``moruna.Cancelled``, with the partial report attached, on Ctrl-C (PY-I6).
    """
    return _core.run(
        source,
        kernels,
        sink,
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
    )


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
) -> Any:
    """Turn a callable or a class instance into a kernel.

    ``stateful=False`` (the default) takes a plain callable ``f(batch) -> batch``.
    ``stateful=True`` takes an object with ``setup(self, ctx) -> state`` and
    ``__call__(self, state, batch) -> batch``, and optionally ``footprint(self, state)``; ``ctx``
    exposes ``.instance`` and ``.device``. ``resume="checkpoint"`` also requires
    ``checkpoint(self, state) -> bytes`` and ``restore(self, ctx, data) -> state``, and is refused
    at decoration when either is missing.

    ``input_schema`` and ``output_schema`` declare what the kernel takes and gives, which makes
    it checkable (``python -m moruna check``): a ``pyarrow.Schema`` is exact, a mapping of
    column to type is a subset, and an output mapping whose keys are only ``adds``, ``drops`` and
    ``changes`` is relative to the input. ``lockfile`` (a path or bytes) is folded into the
    fingerprint, so a kernel whose dependencies change is a different kernel.

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
        )

    if fn is None:
        return decorate
    return decorate(fn)


def polars(fn: Any, *, accepts: str = "table", **declarations: Any) -> KernelSpec:
    """Wrap a function from a Polars frame to a Polars frame as a stateless kernel (05 f.9).

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
