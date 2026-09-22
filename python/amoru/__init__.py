"""Amoru: a memory-aware runtime for data and tensor pipelines (component 12, SDD 12).

Four constructors, one decorator and one function::

    import amoru, pyarrow as pa

    @amoru.kernel
    def normalise(batch: pa.RecordBatch) -> pa.RecordBatch:
        ...

    report = amoru.run(amoru.ParquetSource("s3://bucket/in/", columns=["id", "text"]),
                       normalise,
                       amoru.ParquetSink("s3://bucket/out/"))
    print(report)

No parameter beyond the budget is required: inside a cgroup, ``amoru.run(source, kernels, sink)``
is a complete call, and outside one ``budget=`` is the only argument a user may need (PY-I3).

This module holds no orchestration: the runtime facade wires the components in Rust, and what is
written here is argument translation and nothing else (a, the anti-patterns of l).
"""

from __future__ import annotations

import importlib.util as _importlib_util
from typing import Any

if _importlib_util.find_spec("pyarrow") is None:  # h, failures
    raise ImportError(
        "amoru needs pyarrow >= 17: pip install 'pyarrow>=17'. It is the format the runtime "
        "hands Python kernels and the one it takes back, with no copy in between."
    )

from amoru import _core
from amoru._core import (
    AmoruError,
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
    TensorSink,
    TensorSource,
    inspect_host,
)

__version__: str = _core.__version__

__all__ = [
    "AmoruError",
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
    "TensorSink",
    "TensorSource",
    "__version__",
    "inspect_host",
    "kernel",
    "polars",
    "run",
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
    as if it had been decorated with ``@amoru.kernel`` and its defaults. Every keyword argument is
    optional, and each is range checked once, here, against the configuration table of the design
    (PY-I10): a value outside its range is clamped to the nearest bound and the clamp is named in
    ``report.notes``.

    ``resume`` takes ``"auto"``, a run id (32 hexadecimal characters) or a path to a
    ``manifest.json``, and is the only parameter a user meets after a failure (f.7).

    Raises ``amoru.ConfigError`` when a Python kernel would run under a GIL interpreter without
    ``allow_gil=True`` (PY-I4), ``amoru.PlanError`` when the sink would write where the source
    reads (f.3), and ``amoru.Cancelled``, with the partial report attached, on Ctrl-C (PY-I6).
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
) -> Any:
    """Turn a callable or a class instance into a kernel.

    ``stateful=False`` (the default) takes a plain callable ``f(batch) -> batch``.
    ``stateful=True`` takes an object with ``setup(self, ctx) -> state`` and
    ``__call__(self, state, batch) -> batch``, and optionally ``footprint(self, state)``; ``ctx``
    exposes ``.instance`` and ``.device``. ``resume="checkpoint"`` also requires
    ``checkpoint(self, state) -> bytes`` and ``restore(self, ctx, data) -> state``, and is refused
    at decoration when either is missing.

    Usable bare (``@amoru.kernel``) or with arguments (``@amoru.kernel(stateful=True)``).
    """

    def decorate(obj: Any) -> KernelSpec:
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
        )

    if fn is None:
        return decorate
    return decorate(fn)


def polars(fn: Any, *, accepts: str = "table") -> KernelSpec:
    """Wrap a Polars function as a stateless Python kernel (f.6).

    ``fn`` takes a ``polars.DataFrame`` and returns one. In v1 the only path is through Python:
    the batch is handed to ``polars.from_arrow`` and the result is taken back with ``to_arrow``,
    which copies inside Polars for some types. The Rust path through ``pyo3-polars`` is deferred
    and will not change this signature.
    """
    # Imported here so that `import amoru` does not need polars installed.
    import polars as pl

    def call(batch: Any) -> Any:
        frame = pl.from_arrow(batch)
        result = fn(frame)
        table = result.to_arrow()
        # A Polars frame converts to a pyarrow.Table; a kernel returns one record batch.
        return table.combine_chunks().to_batches()[0] if table.num_rows else table

    call.__name__ = getattr(fn, "__name__", "polars_kernel")
    return kernel(call, accepts=accepts)
