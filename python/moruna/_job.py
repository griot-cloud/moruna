"""``moruna.Job``: a run as a DAG of named tasks, and the tasks a user writes (SDD 12 d.2, 22 d.4).

A job holds tasks, each added with the tasks that feed it::

    job = moruna.Job("orders")
    rows = job.add(moruna.ParquetSource("in/"), name="rows")
    enriched = job.add(Enrich(table="orders"), inputs=rows)
    job.add(moruna.ParquetSink("out/"), inputs=enriched, name="out")
    print(job)                       # one line per task
    job.write("orders.mjob")         # the spec: the job document, version 2
    report = moruna.run(job, ports={"warehouse": warehouse})

A task is a ``moruna.Source``, a ``moruna.Function`` or a ``moruna.Sink``. One of the library's
own (``moruna.ParquetSource``, ``moruna.std.select(...)``, a ``@moruna.function``) is written to
the spec by its kind and settings. One a user writes subclasses ``Source``, ``Function`` or
``Sink``, declares what it ``takes`` and ``gives`` (Arrow schemas) and what it ``needs`` (names
the runner supplies, each passed to it in ``self.ports``), and takes data only in its
constructor; the spec names it as a ``python`` task, ``{"ref": "module:Class", "params": {...},
"needs": [...], "fingerprint": ...}``, and a runner that reads the spec constructs it again,
checking the fingerprint first.
"""

from __future__ import annotations

import functools
import hashlib
import importlib
import importlib.util
import inspect
import json
import math
import os
import re
import sys
import types
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import Any

from moruna import _core, _declare
from moruna._ports import load_resolver, resolver_of, writer_of

__all__ = ["Function", "Job", "JobSpec", "Sink", "Source", "Task", "function"]

#: The roles of a task, as a spec names them.
ROLES = ("source", "function", "sink")

#: Names of a parameter that would carry a secret; a spec holds none.
_SECRET = re.compile(
    r"secret|password|passwd|token|credential|private_key|api_key|access_key", re.IGNORECASE
)

_TASK_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.:-]{0,127}$")


def _config_error(message: str) -> Exception:
    return _core.ConfigError(message)


# ---------------------------------------------------------------------------------------------
# Tasks


def _capture(task: Any, args: tuple[Any, ...], kwargs: dict[str, Any]) -> None:
    """Keep the arguments a task was constructed with: its params, when they are data."""
    task.__dict__["_moruna_args"] = (args, dict(kwargs))


def _schema_check(task: Any, which: str, batch: Any) -> Any:
    """``batch`` checked against the schema ``task`` declares as ``which`` (``takes`` or
    ``gives``), at run time: a batch of another schema fails the task, naming both."""
    declared = getattr(task, which, None)
    if declared is None or batch is None:
        return batch
    schema = getattr(batch, "schema", None)
    if schema is None or not schema.equals(declared, check_metadata=False):
        found = (
            schema.to_string(show_schema_metadata=False)
            if schema is not None
            else type(batch).__name__
        )
        raise TypeError(
            f"{type(task).__name__} {which} {declared.to_string(show_schema_metadata=False)!r}, "
            f"and a batch has {found!r}"
        )
    return batch


def _checked(method: Callable[..., Any], which: str, argument: bool) -> Callable[..., Any]:
    """``method`` with its result (``argument=False``) or its batch argument (``True``) checked
    against the task's declared ``which``."""
    if getattr(method, "_moruna_checked", False):
        return method

    @functools.wraps(method)
    def call(self: Any, *args: Any, **kwargs: Any) -> Any:
        if argument and args:
            _schema_check(self, which, args[0])
        result = method(self, *args, **kwargs)
        if not argument:
            _schema_check(self, which, result)
        return result

    call._moruna_checked = True  # type: ignore[attr-defined]
    return call


class Task:
    """What every task of a job is: a ``Source``, a ``Function`` or a ``Sink``.

    A subclass declares ``takes`` and ``gives`` (``pyarrow.Schema``; ``None`` declares nothing),
    ``needs`` (the names of what the runner must supply; each is given to the task in
    ``self.ports`` before the run starts) and optionally ``lockfile`` (a path, folded into the
    fingerprint). Its constructor's arguments are its params: data only (JSON values) for a
    task a spec can hold.
    """

    takes: Any = None
    gives: Any = None
    needs: Sequence[str] = ()
    lockfile: str | os.PathLike[str] | None = None

    def __new__(cls, *args: Any, **kwargs: Any) -> Any:
        task = super().__new__(cls)
        _capture(task, args, kwargs)
        return task

    @property
    def ports(self) -> Mapping[str, Any]:
        """What the runner supplied for ``needs``, by name; empty before a run supplies it."""
        return self.__dict__.get("_moruna_ports", types.MappingProxyType({}))


def _task_init_subclass(cls: type, role: str) -> None:
    for which in ("takes", "gives"):
        value = cls.__dict__.get(which)
        if value is not None and not (hasattr(value, "names") and hasattr(value, "field")):
            raise TypeError(f"{cls.__name__}.{which} is a pyarrow.Schema, not {value!r}")
    needs = cls.__dict__.get("needs")
    if needs is not None:
        if isinstance(needs, str) or not all(isinstance(n, str) and n for n in needs):
            raise TypeError(
                f"{cls.__name__}.needs is a list of names (strings), not {needs!r}"
            )
        cls.needs = tuple(needs)
    if role == "source" and "read" in cls.__dict__:
        cls.read = _checked(cls.__dict__["read"], "gives", argument=False)
    if role == "sink" and "write" in cls.__dict__:
        cls.write = _checked(cls.__dict__["write"], "takes", argument=True)


class Source(_core.Source, Task):
    """The base of a source of the user's own (07 e.6): ``plan(self) -> list[moruna.Split]`` and
    ``read(self, split_id, start, end) -> pyarrow.RecordBatch`` returning exactly rows
    ``[start, end)`` of the split. ``gives``, when declared, is its schema, and every batch
    ``read`` returns is checked against it."""

    def __new__(cls, *args: Any, **kwargs: Any) -> Any:
        task = _core.Source.__new__(cls, *args, **kwargs)
        _capture(task, args, kwargs)
        return task

    def __init_subclass__(cls, **kwargs: Any) -> None:
        super().__init_subclass__(**kwargs)
        _task_init_subclass(cls, "source")

    def schema(self) -> Any:
        """``gives``: the schema, which saves a read at plan time; ``None`` when undeclared."""
        return self.gives


class Sink(_core.Sink, Task):
    """The base of a sink of the user's own (08 f.10): ``write(self, batch)``, and optionally
    ``finish(self)``, which returns ``None`` or the version it committed, ``{"snapshot_id":
    ..., "version": ...}``. Every batch is checked against ``takes`` when it is declared, and the
    sink's ``target`` attribute, when it has one, is the target the report names."""

    def __new__(cls, *args: Any, **kwargs: Any) -> Any:
        task = _core.Sink.__new__(cls, *args, **kwargs)
        _capture(task, args, kwargs)
        return task

    def __init_subclass__(cls, **kwargs: Any) -> None:
        super().__init_subclass__(**kwargs)
        _task_init_subclass(cls, "sink")


class Function(Task):
    """A function of a job: a batch in, a batch out.

    A subclass defines ``__call__(self, batch) -> batch`` and optionally ``finish(self)``, called
    once when its input has ended, whose batch (or ``None``) goes downstream. Moruna calls one
    instance, the one added to the job, one batch at a time; every batch it takes is checked
    against ``takes`` and every batch it gives against ``gives`` when they are declared.

    ``@moruna.function`` makes a function from a callable, and ``moruna.std`` builds Moruna's own;
    each is a ``moruna.Function``. ``build()`` packages one as an OCI image.
    """

    def __init_subclass__(cls, **kwargs: Any) -> None:
        super().__init_subclass__(**kwargs)
        if cls.__module__ != __name__:
            _task_init_subclass(cls, "function")

    def build(
        self,
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
        """Build this function as an OCI image of four layers and return what was built (SDD 30).

        The same as ``moruna build <module>:<name>``: the function is bound to a name at the top
        level of its module. The image is written to the OCI layout directory ``out`` under
        ``tag`` (default ``<name>:latest``): the runtime, the dependencies the project's
        ``uv.lock`` or ``requirements.lock`` pins (``lock``, or the nearest above the module),
        the shim, and the module with the ``files`` it names. Raises ``moruna.ConfigError``
        naming the cause when the build is refused.
        """
        from moruna import _build  # noqa: PLC0415, the builder is needed by `build` alone

        if isinstance(self, _Handle) and not isinstance(self._handle, _core.KernelSpec):
            raise _config_error(
                f"build refused: {self!r} is Moruna's own; only a Python function is built"
            )
        try:
            return _build.build(
                self,
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
                version=_core.__version__,
            )
        except _build.Refused as exc:
            raise _config_error(f"build refused: {exc}") from exc


class _Handle(Function):
    """A function the library made: a decorated callable, a standard function, a loaded
    package or a task of a declared kind, around the extension's object for it. Frozen, as the
    object it holds is (PY-I7)."""

    def __init__(self, handle: Any) -> None:
        self.__dict__["_handle"] = handle

    def __setattr__(self, name: str, value: Any) -> None:
        raise AttributeError(f"{self!r} is frozen")

    def __call__(self, *args: Any, **kwargs: Any) -> Any:
        return self._handle(*args, **kwargs)

    def __getattr__(self, name: str) -> Any:
        if name.startswith("__"):
            raise AttributeError(name)
        return getattr(self.__dict__["_handle"], name)

    def __repr__(self) -> str:
        return f"moruna.Function({self._handle!r})"


def _wrap(handle: Any) -> Function:
    return _Handle(handle)


def function(
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
    """Turn a callable or a class instance into a ``moruna.Function``.

    ``stateful=False`` (the default) takes a plain callable ``f(batch) -> batch``.
    ``stateful=True`` takes an object with ``setup(self, ctx) -> state`` and
    ``__call__(self, state, batch) -> batch``, and optionally ``footprint(self, state)`` and
    ``finish(self, state)``; ``ctx`` exposes ``.instance`` and ``.device``. ``finish`` is called
    once per instance when the function's input has ended, after its last ``__call__``; a batch
    it returns goes downstream like any other, ``None`` gives nothing, and an exception ends the
    run naming the function. A function that must see the whole run before it gives anything
    gives its result there, and takes ``instances=1``. ``resume="checkpoint"`` also requires
    ``checkpoint(self, state) -> bytes`` and ``restore(self, ctx, data) -> state``.

    ``input_schema`` and ``output_schema`` declare what the function takes and gives, which
    makes it checkable (``python -m moruna check``): a ``pyarrow.Schema`` is exact, a mapping of
    column to type is a subset, and an output mapping whose keys are only ``adds``, ``drops``
    and ``changes`` is relative to the input. ``lockfile`` (a path or bytes) is folded into the
    fingerprint.

    ``memory_guard`` (default on): a single request for memory that would take the process past
    its ceiling is refused before the memory exists, and the run ends with a budget error naming
    the function and the request.

    A function annotated ``pl.DataFrame -> pl.DataFrame`` (or ``pl.LazyFrame``) is a Polars
    function: the batch reaches it as a Polars frame through the Arrow C data interface.

    Usable bare (``@moruna.function``) or with arguments (``@moruna.function(stateful=True)``).
    """

    def decorate(obj: Any) -> Function:
        origin = None
        frame = None if stateful else _declare.polars_signature(obj)
        if frame is not None:
            origin, obj = obj, _declare.polars_callable(obj, frame)
        return _wrap(
            _core.build_kernel(
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
        )

    if fn is None:
        return decorate
    return decorate(fn)


class _Instance:
    """What the runtime calls for a ``Function`` subclass: one stateful instance over the task
    object, checking each batch against what the task declares."""

    def __init__(self, task: Function) -> None:
        self.task = task

    def setup(self, ctx: Any) -> None:
        return None

    def __call__(self, state: Any, batch: Any) -> Any:
        _schema_check(self.task, "takes", batch)
        return _schema_check(self.task, "gives", self.task(batch))

    def finish(self, state: Any) -> Any:
        finish = getattr(self.task, "finish", None)
        if finish is None:
            return None
        return _schema_check(self.task, "gives", finish())


def _lockfile(task: Any) -> bytes | None:
    return _declare.lockfile_bytes(getattr(task, "lockfile", None))


def _declared(task: Any) -> tuple[Any, Any]:
    return (
        _declare.declaration(getattr(task, "takes", None), "input_schema"),
        _declare.declaration(getattr(task, "gives", None), "output_schema"),
    )


def _function_handle(task: Function) -> Any:
    """The extension's function for a ``Function`` subclass instance: one instance, whose
    fingerprint is taken of the task's class."""
    handle = task.__dict__.get("_moruna_handle")
    if handle is None:
        takes, gives = _declared(task)
        handle = _core.build_kernel(
            _Instance(task),
            stateful=True,
            instances=1,
            input_schema=takes,
            output_schema=gives,
            lockfile=_lockfile(task),
            origin=type(task),
        )
        task.__dict__["_moruna_handle"] = handle
    return handle


def _fingerprint(task: Any) -> str:
    """What pins a task's code: the fingerprint ``moruna check`` computes for a function (its
    qualified name, its source, its declaration and its lock file), taken of the class of a
    ``Source`` or ``Sink`` the same way."""
    if isinstance(task, _Handle):
        return f"sha256:{task._handle.fingerprint}"
    if isinstance(task, Function):
        return f"sha256:{_function_handle(task).fingerprint}"
    takes, gives = _declared(task)
    probe = _core.build_kernel(
        _Instance(task),  # never called: the class is what is fingerprinted
        stateful=True,
        instances=1,
        input_schema=takes,
        output_schema=gives,
        lockfile=_lockfile(task),
        origin=type(task),
    )
    return f"sha256:{probe.fingerprint}"


# ---------------------------------------------------------------------------------------------
# What a task is to a job


_BUILTIN_SOURCES = (
    _core.ParquetSource,
    _core.VortexSource,
    _core.ArrowIpcSource,
    _core.TensorSource,
    _core.IteratorSource,
)
_BUILTIN_SINKS = (_core.ParquetSink, _core.VortexSink, _core.TensorSink, _core.ArrowIpcSink)
_LIBRARY_FUNCTIONS = (_core.StdKernel, _core.PackageKernel, _core.DeclaredTask)


class _Read:
    """A task read from a spec: its role, kind, settings and input ports, written back as read.
    A ``python`` task is constructed from its reference when the job runs."""

    def __init__(self, role: str, kind: str, settings: dict[str, Any], ports: list[str]) -> None:
        self.role = role
        self.kind = kind
        self.settings = settings
        self.port_names = ports

    @property
    def needs(self) -> tuple[str, ...]:
        return tuple(self.settings.get("needs", ())) if self.kind == "python" else ()

    def __repr__(self) -> str:
        return f"<{self.role} {self.kind} {json.dumps(self.settings, sort_keys=True)}>"


def _role_of(task: Any) -> str:
    if isinstance(task, _Read):
        return task.role
    if isinstance(task, (Source, *_BUILTIN_SOURCES)):
        return "source"
    if isinstance(task, (Sink, *_BUILTIN_SINKS)):
        return "sink"
    if isinstance(task, Function):
        return "function"
    if isinstance(task, (_core.Source, _core.Sink)):
        raise TypeError(f"{type(task).__name__} subclasses moruna._core; subclass moruna.Source "
                        "or moruna.Sink")
    raise TypeError(
        f"a task is a moruna.Source, a moruna.Function or a moruna.Sink (or a callable, made a "
        f"function), not {type(task).__name__}"
    )


def _snake(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def _default_name(task: Any) -> str:
    if isinstance(task, _Handle):
        handle = task._handle
        if isinstance(handle, _core.KernelSpec):
            return str(getattr(handle.wrapped, "__name__", "function"))
        if isinstance(handle, _core.StdKernel):
            return str(handle.name)
        if isinstance(handle, _core.DeclaredTask):
            return str(handle.kind)
        return "package"
    if isinstance(task, _Read):
        return task.kind
    return _snake(type(task).__name__)


def _reference(obj: Any) -> str:
    """``module:qualname`` of a class or a function, with ``__main__`` named by its file."""
    module = getattr(obj, "__module__", None) or ""
    qualname = getattr(obj, "__qualname__", None) or getattr(obj, "__name__", "")
    if module == "__main__":
        main = sys.modules.get("__main__")
        path = getattr(main, "__file__", None)
        if path:
            module = Path(path).stem
    return f"{module}:{qualname}"


def _data(value: Any, where: str) -> None:
    """Refuse a value that is not data (a JSON value), naming where it is."""
    if value is None or isinstance(value, (str, bool, int)):
        return
    if isinstance(value, float):
        if not math.isfinite(value):
            raise ValueError(f"{where} is {value}, which JSON does not hold")
        return
    if isinstance(value, list):
        for i, item in enumerate(value):
            _data(item, f"{where}[{i}]")
        return
    if isinstance(value, dict):
        for key, item in value.items():
            if not isinstance(key, str):
                raise ValueError(f"{where} has the key {key!r}; a key of data is a string")
            _data(item, f"{where}[{key!r}]")
        return
    raise ValueError(
        f"{where} is a {type(value).__name__}, not data; a task's params are JSON values "
        "(None, bool, int, float, str, list, dict)"
    )


def _params(task: Any, key: str) -> dict[str, Any]:
    """The arguments ``task`` was constructed with, by name, defaults applied: its params."""
    args, kwargs = task.__dict__.get("_moruna_args", ((), {}))
    init = type(task).__init__
    if init in (object.__init__, Task.__init__, _core.Source.__init__, _core.Sink.__init__):
        if args or kwargs:
            raise ValueError(f"task {key!r}: {type(task).__name__} takes no params")
        return {}
    signature = inspect.signature(init)
    bound = signature.bind(task, *args, **kwargs)
    bound.apply_defaults()
    params: dict[str, Any] = {}
    for name, value in list(bound.arguments.items())[1:]:
        kind = signature.parameters[name].kind
        if kind is inspect.Parameter.VAR_POSITIONAL:
            if value:
                raise ValueError(
                    f"task {key!r}: {type(task).__name__} was given positional arguments by "
                    "*args, which a spec cannot name; take them by name"
                )
        elif kind is inspect.Parameter.VAR_KEYWORD:
            params.update(value)
        else:
            params[name] = value
    return params


def _writable_reference(obj: Any, key: str) -> str:
    """The reference of a class or function a spec can name: one at the top level of an
    importable module, found there under its name."""
    reference = _reference(obj)
    module, _, qualname = reference.partition(":")
    if "<locals>" in qualname or "<lambda>" in qualname:
        raise ValueError(
            f"task {key!r}: {qualname} is defined inside a function; a spec names code at the "
            "top level of a module"
        )
    found: Any = sys.modules.get(module) or sys.modules.get(getattr(obj, "__module__", ""))
    for part in qualname.split("."):
        found = getattr(found, part, None)
    if found is not obj and getattr(found, "_handle", None) is not obj:
        raise ValueError(
            f"task {key!r}: {reference} does not name it; a spec names code by the module and "
            "the name it is bound to"
        )
    return reference


def _python_settings(task: Any, key: str, *, strict: bool) -> dict[str, Any]:
    """The ``python`` settings of a task the user wrote or decorated: its reference, params,
    needs and fingerprint. ``strict`` (for a spec) refuses what a spec cannot hold: a closure, a
    param that is not data, a param that names a secret."""
    if isinstance(task, _Handle):
        target = task._handle.wrapped
        target = getattr(target, "__wrapped__", target)
        params: dict[str, Any] = {}
        needs: tuple[str, ...] = ()
    else:
        target = type(task)
        needs = tuple(task.needs)
        if strict:
            params = _params(task, key)
        else:
            # A run or a plan of this process names the params it can, so its digest and its
            # step ids are the spec's whenever the job has one.
            try:
                params = _params(task, key)
                for name, value in params.items():
                    _data(value, name)
            except (TypeError, ValueError):
                params = {}
    if strict:
        if isinstance(task, _Handle):
            reference = _decorated_reference(task, key)
        else:
            reference = _writable_reference(target, key)
        for name, value in params.items():
            if _SECRET.search(name):
                raise ValueError(
                    f"task {key!r}: the param {name!r} names a secret, and a spec holds none; "
                    "declare it in `needs` and supply it when the job runs"
                )
            _data(value, f"task {key!r}: param {name!r}")
    else:
        reference = _reference(target)
    settings: dict[str, Any] = {"ref": reference}
    if params:
        settings["params"] = params
    if needs:
        settings["needs"] = list(needs)
    settings["fingerprint"] = _fingerprint(task)
    return settings


def _decorated_reference(task: _Handle, key: str) -> str:
    """The reference of a decorated function: the name its module binds the function to."""
    wrapped = task._handle.wrapped
    reference = _reference(getattr(wrapped, "__wrapped__", wrapped))
    module, _, qualname = reference.partition(":")
    if "<locals>" in qualname or "<lambda>" in qualname:
        raise ValueError(
            f"task {key!r}: {qualname} is defined inside a function; a spec names code at the "
            "top level of a module"
        )
    found = sys.modules.get(module) or sys.modules.get(getattr(wrapped, "__module__", ""))
    for name, value in vars(found).items() if found is not None else ():
        if value is task or value is task._handle:
            return f"{module}:{name}"
    raise ValueError(
        f"task {key!r}: {reference} is not bound to a name of its module; a spec names a "
        "function by the module and the name it is bound to"
    )


# ---------------------------------------------------------------------------------------------
# Making a task from its reference


def _import(reference: str) -> Any:
    """What ``module:attribute`` names: the module a dotted name on the interpreter's path, or a
    ``.py`` file by its path, loaded once per path."""
    module, sep, attribute = reference.rpartition(":")
    if not sep or not module or not attribute:
        raise ValueError(f"`{reference}`: a reference is module:attribute")
    target: Any
    if module.endswith(".py") or "/" in module:
        name = "_moruna_job_" + re.sub(r"[^A-Za-z0-9]", "_", module)
        target = sys.modules.get(name)
        if target is None:
            spec = importlib.util.spec_from_file_location(name, module)
            if spec is None or spec.loader is None:
                raise ImportError(f"`{module}` is not a Python file")
            target = importlib.util.module_from_spec(spec)
            sys.modules[name] = target
            try:
                spec.loader.exec_module(target)
            except BaseException:
                del sys.modules[name]
                raise
    else:
        target = importlib.import_module(module)
    for part in attribute.split("."):
        target = getattr(target, part)
    return target


def _construct(role: str, settings: Mapping[str, Any], at: str) -> Any:
    """The task the ``python`` settings name, constructed and its fingerprint checked."""
    reference = settings["ref"]
    params = dict(settings.get("params") or {})
    needs = list(settings.get("needs") or [])
    try:
        target = _import(reference)
    except (ImportError, AttributeError, ValueError) as exc:
        raise _config_error(f"{at}.ref: `{reference}` does not import: {exc}") from None
    if isinstance(target, type) and issubclass(target, Task):
        try:
            task = target(**params)
        except Exception as exc:
            raise _config_error(
                f"{at}.params: `{reference}` refused its params: {type(exc).__name__}: {exc}"
            ) from exc
    elif params:
        raise _config_error(f"{at}.params: `{reference}` is not a class and takes no params")
    elif isinstance(target, Function):
        task = target
    elif isinstance(target, _core.KernelSpec):
        task = _wrap(target)
    elif callable(target):
        task = function(target)
    else:
        raise _config_error(f"{at}.ref: `{reference}` is a {type(target).__name__}, not a task")
    found = _role_of(task)
    if found != role:
        raise _config_error(f"{at}.ref: `{reference}` is a {found}, and the task is a {role}")
    declared = list(getattr(task, "needs", ()) or ())
    if declared != needs:
        raise _config_error(
            f"{at}.needs: the spec names {needs} and `{reference}` needs {declared}"
        )
    pinned = settings.get("fingerprint")
    if pinned is not None:
        have = _fingerprint(task)
        if pinned.rsplit(":", 1)[-1].lower() != have.rsplit(":", 1)[-1]:
            raise _config_error(
                f"{at}.fingerprint: the spec pins {pinned} and `{reference}` here is {have}"
            )
    return task


def _supply(tasks: Sequence[tuple[str, Any]], resolver: Any) -> None:
    """Supply every need of every task (each with the label a refusal names it by) through
    ``resolver`` before the run starts; a need it cannot supply is refused, every one of them
    named."""
    missing: list[str] = []
    supplied: list[tuple[Any, dict[str, Any]]] = []
    for key, task in tasks:
        needs = tuple(getattr(task, "needs", ()) or ())
        if not needs:
            continue
        got: dict[str, Any] = {}
        for need in needs:
            if resolver is None:
                missing.append(f"{key} needs {need!r}, and no resolver was given")
                continue
            try:
                got[need] = resolver.resolve(need)
            except LookupError:
                missing.append(f"{key} needs {need!r}, which the resolver cannot supply")
            except Exception as exc:  # the resolver's own failure, named
                missing.append(
                    f"{key} needs {need!r}, and the resolver failed: "
                    f"{type(exc).__name__}: {exc}"
                )
        supplied.append((task, got))
    if missing:
        raise _config_error("; ".join(missing))
    for task, got in supplied:
        task.__dict__["_moruna_ports"] = types.MappingProxyType(got)


def _materialize(role: str, at: str, settings: str, resolver: str | None) -> Any:
    """The command's ``python`` task (``moruna run JOB``): constructed from its settings, its
    needs supplied by the resolver ``--resolver`` names, lowered to what the runtime runs."""
    task = _construct(role, json.loads(settings), at)
    _supply([(at, task)], load_resolver(resolver) if resolver else None)
    if role == "function":
        return _runtime_object(task)
    return task


def _runtime_object(task: Any) -> Any:
    """What the runtime is handed for a task: the extension's function, or the object."""
    if isinstance(task, _Handle):
        return task._handle
    if isinstance(task, Function):
        return _function_handle(task)
    return task


# ---------------------------------------------------------------------------------------------
# The job


class JobSpec:
    """A job's spec: the job document, version 2 (FlatBuffers, file identifier ``MJB2``).
    ``bytes()`` are the document; ``digest()`` is ``"sha256:"`` and the hex of SHA-256 over
    exactly those bytes; ``to_json()`` renders it for a person and is never read back."""

    __slots__ = ("_bytes",)

    def __init__(self, data: bytes) -> None:
        self._bytes = bytes(data)

    def bytes(self) -> bytes:
        return self._bytes

    def digest(self) -> str:
        return "sha256:" + hashlib.sha256(self._bytes).hexdigest()

    def to_json(self) -> str:
        read = json.loads(_core.read_job(self._bytes))
        return json.dumps(
            {"version": 2, "name": read["name"], "tasks": read["tasks"],
             "settings": read["settings"]},
            indent=2,
            sort_keys=True,
        )

    def __eq__(self, other: object) -> bool:
        return isinstance(other, JobSpec) and other._bytes == self._bytes

    def __hash__(self) -> int:
        return hash(self._bytes)

    def __repr__(self) -> str:
        return f"moruna.JobSpec({self.digest()}, {len(self._bytes)} bytes)"


class _Entry:
    __slots__ = ("inputs", "key", "role", "task")

    def __init__(self, key: str, role: str, task: Any, inputs: tuple[Any, ...]) -> None:
        self.key = key
        self.role = role
        self.task = task
        self.inputs = inputs


class Job:
    """A run as a DAG of named tasks (SDD 12 d.2, 22 d.4).

    ``add(task, inputs=..., name=None)`` adds a task fed by ``inputs`` (tasks of the job or
    their names, in port order) and returns it. A function of one input takes it on its port
    ``in``; one of several takes each on a port named after the task that feeds it. A task's
    output may feed many tasks, and branches ready at once run at once.
    """

    def __init__(self, name: str) -> None:
        if not isinstance(name, str) or not name:
            raise TypeError(f"a job is named by a non-empty string, not {name!r}")
        self.name: str | None = name
        self._entries: dict[str, _Entry] = {}
        # A job read from a document that also sets run settings: the document, whose settings
        # its spec, its check and its plan keep, and the fields it sets.
        self._base: bytes | None = None
        self._settings: tuple[str, ...] = ()

    @property
    def settings(self) -> tuple[str, ...]:
        """The run settings the document a job was read from sets (``budget``, ``staging``,
        ...), which its spec and its plan keep; empty for a job built here."""
        return self._settings

    # Building ------------------------------------------------------------------------------

    def add(self, task: Any, inputs: Any = (), name: str | None = None) -> Any:
        """Add ``task`` fed by ``inputs`` and return it (a callable is made a
        ``moruna.Function`` first, and that is returned)."""
        if not isinstance(task, (Task, _Read, *_BUILTIN_SOURCES, *_BUILTIN_SINKS)) and callable(
            task
        ):
            task = function(task)
        if isinstance(task, _LIBRARY_FUNCTIONS) or isinstance(task, _core.KernelSpec):
            task = _wrap(task)
        role = _role_of(task)
        for entry in self._entries.values():
            if entry.task is task:
                raise ValueError(f"this task is in the job already, as {entry.key!r}")
        if name is None:
            base = _default_name(task)
            name, n = base, 2
            while name in self._entries:
                name, n = f"{base}-{n}", n + 1
        if not isinstance(name, str) or not _TASK_ID.match(name):
            raise ValueError(
                f"a task's name is 1 to 128 of letters, digits, `_`, `-`, `.` and `:`, starting "
                f"with a letter or a digit, not {name!r}"
            )
        if name in self._entries:
            raise ValueError(f"the job has a task named {name!r} already")
        listed = (inputs,) if isinstance(inputs, (str, Task, _Read)) or not isinstance(
            inputs, Sequence
        ) else tuple(inputs)
        if role == "source" and listed:
            raise ValueError(f"{name!r} is a source, and a source takes no input")
        if role != "source" and not listed:
            raise ValueError(f"{name!r} is a {role}, and takes at least one input")
        resolved: list[str] = []
        for upstream in listed:
            if isinstance(upstream, str):
                resolved.append(upstream)
                continue
            found = [e.key for e in self._entries.values() if e.task is upstream]
            if not found:
                raise ValueError(
                    f"the inputs of {name!r} are tasks of this job or their names, and "
                    f"{upstream!r} is not in the job; add it first"
                )
            resolved.append(found[0])
        self._entries[name] = _Entry(name, role, task, tuple(resolved))
        return task

    @property
    def tasks(self) -> tuple[tuple[str, str, Any, tuple[str, ...]], ...]:
        """``(name, role, task, inputs)`` per task, in the order they were added."""
        return tuple((e.key, e.role, e.task, e.inputs) for e in self._entries.values())

    def __getitem__(self, name: str) -> Any:
        return self._entries[name].task

    def __len__(self) -> int:
        return len(self._entries)

    # Reading and writing -------------------------------------------------------------------

    def spec(self) -> JobSpec:
        """The job's spec, at any step of building it. Refused, naming the task, when a task
        cannot be written: an ``IteratorSource``, code defined inside a function, a param that
        is not data or that names a secret."""
        return JobSpec(_core.job_document(self.name, self._lowered(spec=True), self._base))

    def write(self, target: Any) -> JobSpec:
        """Write the spec's bytes through the writer ``target`` is: a file path, a callable
        taking bytes, or a ``moruna.Writer``. Returns the spec."""
        spec = self.spec()
        writer_of(target).write_spec(spec.bytes())
        return spec

    @classmethod
    def read(cls, source: Any) -> Job:
        """The job a spec holds: a ``JobSpec``, its bytes, or a path to them (a version 2
        document, or a version 1 JSON document). A ``python`` task is constructed from its
        reference when the job runs. A document that also sets run settings keeps them, in its
        spec and its plan (``job.settings`` names them); such a job is run by ``moruna run``,
        and ``moruna.run`` refuses it."""
        if isinstance(source, JobSpec):
            data = source.bytes()
        elif isinstance(source, (bytes, bytearray)):
            data = bytes(source)
        else:
            data = Path(source).read_bytes()
        read = json.loads(_core.read_job(data))
        job = cls(read["name"] or "job")
        job.name = read["name"]
        if read["settings"]:
            job._base, job._settings = data, tuple(read["settings"])
        for task in read["tasks"]:
            role = "function" if task["role"] == "function" else task["role"]
            read = _Read(role, task["kind"], task["settings"], list(task["ports"]))
            job._entries[task["id"]] = _Entry(task["id"], role, read, tuple(task["inputs"]))
        return job

    # Checking and explaining ---------------------------------------------------------------

    def check(self) -> None:
        """Check the job without running it: its names, ports and edges, each task's kind and
        settings, and along every edge whose two ends declare one, that the schema one task
        gives is the schema the next takes. Raises ``moruna.ConfigError`` naming every
        refusal."""
        refusals = list(
            _core.check_job(self.name, self._lowered(spec=False, make=False), self._base)
        )
        for entry in self._entries.values():
            takes = getattr(entry.task, "takes", None)
            for port, upstream in enumerate(entry.inputs):
                feeder = self._entries.get(upstream)
                gives = getattr(feeder.task, "gives", None) if feeder else None
                if takes is None or gives is None:
                    continue
                if not gives.equals(takes, check_metadata=False):
                    refusals.append(
                        f"job[{entry.key!r}].inputs[{port}]: {entry.key!r} takes "
                        f"{takes.to_string(show_schema_metadata=False)!r} and {upstream!r} "
                        f"gives {gives.to_string(show_schema_metadata=False)!r}"
                    )
        if refusals:
            raise _config_error("; ".join(refusals))

    def explain(self, planner: str | None = None) -> dict[str, Any]:
        """The plan Moruna would run the job by, as a dict, without running it: its steps in the
        order they start, each with its tasks, the steps it waits for, its group, its resources
        and its estimated cost; ``groups`` and ``totals``. The same function plans ``moruna
        explain JOB``. Raises ``moruna.ConfigError`` naming the field when the job is refused,
        and ``moruna.PlanError`` for an unknown planner."""
        plan: dict[str, Any] = json.loads(
            _core.explain_job(
                self.name, self._lowered(spec=False, make=False), self._base, planner=planner
            )
        )
        return plan

    # Lowering ------------------------------------------------------------------------------

    def _lowered(
        self, *, spec: bool, make: bool = False, materialized: dict[str, Any] | None = None
    ) -> list[tuple[Any, ...]]:
        """The tasks as the extension takes them: ``(role, id, at, object, inputs, ports, kind,
        settings)``. ``spec``: for a spec, refusing what it cannot hold. ``make``: for a run,
        with ``materialized`` holding the task each read ``python`` task was constructed as."""
        lowered = []
        for e in self._entries.values():
            at = f"job[{e.key!r}]"
            task = e.task
            obj: Any = None
            kind: str | None = None
            settings: str | None = None
            ports: list[str] | None = None
            try:
                if isinstance(task, _Read):
                    kind, settings, ports = task.kind, json.dumps(task.settings), task.port_names
                    if make and task.kind == "python":
                        obj = _runtime_object((materialized or {})[e.key])
                elif isinstance(task, _Handle) and not isinstance(task._handle, _core.KernelSpec):
                    obj = task._handle
                elif isinstance(task, (Task,)):
                    kind = "python"
                    settings = json.dumps(_python_settings(task, e.key, strict=spec))
                    obj = _runtime_object(task) if make else None
                elif isinstance(task, _core.IteratorSource):
                    if spec:
                        raise ValueError(
                            f"task {e.key!r}: an IteratorSource is a stream this process holds, "
                            "which a spec cannot; subclass moruna.Source to read it by position"
                        )
                    obj = task
                else:
                    obj = task
            except ValueError as exc:
                raise _config_error(f"{at}: {exc}") from None
            lowered.append((e.role, e.key, at, obj, list(e.inputs), ports, kind, settings))
        return lowered

    # Showing ---------------------------------------------------------------------------------

    def __repr__(self) -> str:
        lines = [f"moruna.Job({self.name!r}, {len(self._entries)} tasks)"]
        if self._settings:
            lines.append(f"  settings: {', '.join(self._settings)}")
        for e in self._entries.values():
            lines.append("  " + _describe(e))
        return "\n".join(lines)

    __str__ = __repr__


def _schema_text(schema: Any) -> str:
    if schema is None:
        return "-"
    return "{" + ", ".join(f"{f.name}: {f.type}" for f in schema) + "}"


def _describe(e: _Entry) -> str:
    """One line of metadata per task: key, role, kind or code reference, params, inputs, needs
    and schemas."""
    task = e.task
    params: Any = {}
    if isinstance(task, _Read):
        what = task.kind
        if task.kind == "python":
            what = f"python {task.settings.get('ref')}"
            params = task.settings.get("params", {})
        else:
            params = task.settings
    elif isinstance(task, _Handle):
        handle = task._handle
        if isinstance(handle, _core.KernelSpec):
            what = f"python {_reference(getattr(handle.wrapped, '__wrapped__', handle.wrapped))}"
        else:
            what = repr(handle)
    elif isinstance(task, Task):
        what = f"python {_reference(type(task))}"
        try:
            params = _params(task, e.key)
        except (TypeError, ValueError) as exc:
            params = f"<{exc}>"
    else:
        what = repr(task)
    needs = ",".join(getattr(task, "needs", ()) or ()) or "-"
    inputs = ",".join(e.inputs) or "-"
    shown = json.dumps(params, sort_keys=True, default=lambda v: f"<{type(v).__name__}>")
    return (
        f"{e.key}  {e.role}  {what}  params={shown}  inputs={inputs}  needs={needs}  "
        f"takes={_schema_text(getattr(task, 'takes', None))}  "
        f"gives={_schema_text(getattr(task, 'gives', None))}"
    )


def _materialize_read(job: Job) -> dict[str, Any]:
    """Construct every ``python`` task a job read from a spec, its fingerprint checked."""
    made: dict[str, Any] = {}
    for e in job._entries.values():
        if isinstance(e.task, _Read) and e.task.kind == "python":
            made[e.key] = _construct(e.role, e.task.settings, f"job[{e.key!r}]")
    return made


def run_job(job: Job, *, ports: Any, spec_to: Any, explain: bool, options: dict[str, Any]) -> Any:
    """``moruna.run(job, ...)``: the needs supplied, the spec written when asked, the job run,
    its report written beside the spec."""
    from moruna._report import RunReport  # noqa: PLC0415

    if not isinstance(job, Job):
        raise TypeError(f"moruna.run takes a moruna.Job, not {type(job).__name__}")
    if job.settings:
        raise _config_error(
            f"this job was read from a document that sets {', '.join(job.settings)}; those are "
            "moruna.run's arguments, so run the document as it is with `moruna run`, or run a "
            "spec of tasks and edges"
        )
    resolver = resolver_of(ports)
    made = _materialize_read(job)
    live = [
        (f"job[{e.key!r}]", made.get(e.key, e.task))
        for e in job._entries.values()
        if not isinstance(made.get(e.key, e.task), _Read)
    ]
    _supply(live, resolver)
    try:
        spec: JobSpec | None = job.spec()
    except _core.ConfigError:
        if spec_to is not None:
            raise
        spec = None
    writer = writer_of(spec_to) if spec_to is not None else None
    if writer is not None and spec is not None:
        writer.write_spec(spec.bytes())
    lowered = job._lowered(spec=False, make=True, materialized=made)
    try:
        text = _core.run(
            job.name or "job",
            lowered,
            job_digest=spec.digest() if spec is not None else None,
            explain=explain,
            **options,
        )
    except _core.MorunaError as exc:
        report = getattr(exc, "report", None)
        if writer is not None and report is not None:
            writer.write_report(report.bytes())
        raise
    report = RunReport._from_json(text)
    if writer is not None:
        writer.write_report(report.bytes())
    return report


def chain(source: Any, functions: Any, sink: Any) -> Job:
    """``moruna.run(source, functions, sink)``: the job of one path, its tasks ``source``,
    ``function-1`` .. ``function-n`` and ``sink``."""
    items = list(functions) if isinstance(functions, (list, tuple)) else [functions]
    job = Job("run")
    job.add(source, name="source")
    before = "source"
    for i, item in enumerate(items, start=1):
        job.add(item, inputs=before, name=f"function-{i}")
        before = f"function-{i}"
    job.add(sink, inputs=before, name="sink")
    return job
