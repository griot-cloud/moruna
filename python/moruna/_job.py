"""``moruna.Job``: a run as a DAG of named tasks, and the tasks a user writes (SDD 12 d.2, 22 d.4).

A job holds tasks, each added with the tasks that feed it::

    job = moruna.Job("orders")
    rows = job.add(moruna.ParquetSource("in/"), name="rows")
    enriched = job.add(Enrich(table="orders"), inputs=rows)
    job.add(moruna.ParquetSink("out/"), inputs=enriched, name="out")
    print(job)                       # one line per task, with its image's state
    job.build(out="images")          # one image per task of the user's code
    job.write("orders.mjob")         # the spec: the job document, version 2
    report = moruna.run(job, ports={"warehouse": warehouse})

A task is a ``moruna.Source``, a ``moruna.Function`` or a ``moruna.Sink``. One of the library's
own (``moruna.ParquetSource``, ``moruna.std.select(...)``) is written to the spec by its kind and
settings. One a user writes (a subclass, or a ``@moruna.function``) is an ``image`` task: the spec
names its entry, its params, its needs and its image, ``{build_key, digest, location}``, and
carries its definition inline, made from its code (its docstring, its constructor's params,
what it takes, gives and needs, and where its code came from). The build key is known before
anything is built; ``job.build()`` fills the digest and the location. In this process a job's
tasks run as the objects they are, built or not.
"""

from __future__ import annotations

import dataclasses
import datetime
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

#: The types a run argument may have.
ARG_TYPES = ("string", "int", "float", "bool", "date", "timestamp")

__all__ = ["Function", "Job", "JobSpec", "Sink", "Source", "Task", "function", "image"]

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
    files: Sequence[str | os.PathLike[str]] = ()
    lockfile: str | os.PathLike[str] | None = None

    def __new__(cls, *args: Any, **kwargs: Any) -> Any:
        task = super().__new__(cls)
        _capture(task, args, kwargs)
        return task

    @property
    def ports(self) -> Mapping[str, Any]:
        """What the runner supplied for ``needs``, by name; empty before a run supplies it."""
        return self.__dict__.get("_moruna_ports", types.MappingProxyType({}))

    @property
    def args(self) -> Mapping[str, Any]:
        """The run's arguments, by name; empty before a run supplies them."""
        return self.__dict__.get("_moruna_run_args", types.MappingProxyType({}))


def _task_init_subclass(cls: type, role: str) -> None:
    for which in ("takes", "gives"):
        value = cls.__dict__.get(which)
        if value is not None and not (hasattr(value, "names") and hasattr(value, "field")):
            raise TypeError(f"{cls.__name__}.{which} is a pyarrow.Schema, not {value!r}")
    needs = cls.__dict__.get("needs")
    if needs is not None:
        described: dict[str, str | None]
        if isinstance(needs, Mapping):
            described = dict(needs)
        elif isinstance(needs, str):
            raise TypeError(
                f"{cls.__name__}.needs is a list of names, or a mapping of name to what it is, "
                f"not {needs!r}"
            )
        else:
            described = {n: None for n in needs}
        if not all(isinstance(n, str) and n for n in described) or not all(
            d is None or isinstance(d, str) for d in described.values()
        ):
            raise TypeError(
                f"{cls.__name__}.needs is a list of names, or a mapping of name to what it is, "
                f"not {needs!r}"
            )
        cls.needs = tuple(described)
        cls._moruna_need_text = described
    files = cls.__dict__.get("files")
    if files is not None:
        if isinstance(files, (str, os.PathLike)):
            files = (files,)
        cls.files = tuple(files)
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
    each is a ``moruna.Function``. ``job.build()`` builds a job's functions into images.
    """

    def __init_subclass__(cls, **kwargs: Any) -> None:
        super().__init_subclass__(**kwargs)
        if cls.__module__ != __name__:
            _task_init_subclass(cls, "function")


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

    @property
    def args(self) -> Any:  # type: ignore[override]
        """A standard function's arguments, as its handle holds them; otherwise the run's."""
        handle = self.__dict__["_handle"]
        if hasattr(handle, "args"):
            return handle.args
        return self.__dict__.get("_moruna_run_args", types.MappingProxyType({}))


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
        made = _wrap(
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
        # What its definition says it takes and gives, when it declares exact schemas.
        made.__dict__["_moruna_declared"] = (input_schema, output_schema)
        return made

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
    """A task read from a spec: its role, kind, settings, definition and input ports, written
    back as read. An ``image`` task runs its image."""

    def __init__(
        self,
        role: str,
        kind: str,
        settings: dict[str, Any],
        ports: list[str],
        definition: dict[str, Any] | None,
    ) -> None:
        self.role = role
        self.kind = kind
        self.settings = settings
        self.port_names = ports
        self.definition = definition
        self.takes = self.gives = None

    @property
    def needs(self) -> tuple[str, ...]:
        return tuple(self.settings.get("needs", ())) if self.kind == "image" else ()

    @property
    def image(self) -> dict[str, Any] | None:
        return self.settings.get("image") if self.kind == "image" else None

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
# A task of the user's code as an image task: its entry, its params, its needs, its image and
# its definition


@dataclasses.dataclass(frozen=True)
class Target:
    """What a job's images are built for: the platform, the runtime image (its name in a build
    key), the lock (when given, else the nearest above the code) and the data files every task
    reads."""

    platform: str
    runtime: str
    runtime_layout: str | None = None
    runtime_reference: str | None = None
    lock: str | None = None
    files: tuple[str, ...] = ()


def _default_target() -> Target:
    from moruna import _build  # noqa: PLC0415

    return Target(platform=_build.parse_platform(None).name, runtime=_build.RUNTIME_INDEX)


def _entry(task: Any, key: str) -> tuple[str, Any]:
    """``module:attribute`` of the code of a task the user wrote or decorated, and that code: the
    name its module binds it to."""
    if isinstance(task, _Handle):
        return _decorated_reference(task, key), task._handle.wrapped
    return _writable_reference(type(task), key), type(task)


def _arrow(schema: Any) -> dict[str, Any] | None:
    """A ``pyarrow.Schema`` as a definition spells it."""
    if schema is None or not (hasattr(schema, "names") and hasattr(schema, "field")):
        return None
    return {
        "fields": [
            {"name": f.name, "type": str(f.type), "nullable": bool(f.nullable)} for f in schema
        ]
    }


_SCHEMA_DIALECT = "https://json-schema.org/draft/2020-12/schema"
_JSON_TYPES = {int: "integer", float: "number", str: "string", bool: "boolean", type(None): "null"}


def _param_schema(annotation: Any, where: str) -> dict[str, Any]:
    """The JSON Schema of one param from its annotation; an unannotated param takes any data."""
    import typing  # noqa: PLC0415

    if annotation is inspect.Parameter.empty or annotation is Any:
        return {}
    if annotation in _JSON_TYPES:
        return {"type": _JSON_TYPES[annotation]}
    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)
    if origin in (list, tuple, Sequence) or annotation in (list, tuple):
        items = args[0] if args and args[-1] is not Ellipsis or len(args) == 2 else None
        schema: dict[str, Any] = {"type": "array"}
        if items is not None:
            schema["items"] = _param_schema(items, where)
        return schema
    if origin in (dict, Mapping) or annotation is dict:
        schema = {"type": "object"}
        if len(args) == 2:
            schema["additionalProperties"] = _param_schema(args[1], where)
        return schema
    if origin is typing.Literal:
        return {"enum": list(args)}
    if origin in (typing.Union, types.UnionType):
        options = [_param_schema(a, where) for a in args]
        simple = [o.get("type") for o in options]
        if all(isinstance(t, str) and len(o) == 1 for t, o in zip(simple, options, strict=True)):
            return {"type": simple}
        return {"anyOf": options}
    raise ValueError(
        f"{where} is annotated {annotation!r}, which a JSON Schema of data cannot describe: "
        "annotate a param with bool, int, float, str, None, list, dict, Literal or a union of them"
    )


def _params_schema(cls: type, key: str, strict: bool = True) -> dict[str, Any]:
    """The JSON Schema of the params a task's constructor takes: each named param with its type
    (from its annotation) and its default; closed."""
    import typing  # noqa: PLC0415

    init = cls.__init__
    properties: dict[str, Any] = {}
    required: list[str] = []
    if init not in (object.__init__, Task.__init__, _core.Source.__init__, _core.Sink.__init__):
        signature = inspect.signature(init)
        try:
            hints = typing.get_type_hints(init)
        except Exception:  # noqa: BLE001, an annotation that does not evaluate is taken as written
            hints = {}
        for name, param in list(signature.parameters.items())[1:]:
            if param.kind is inspect.Parameter.VAR_POSITIONAL:
                continue
            if param.kind is inspect.Parameter.VAR_KEYWORD:
                if not strict:
                    continue
                raise ValueError(
                    f"task {key!r}: {cls.__name__} takes **{name}, which a closed schema of its "
                    "params cannot describe; name each param"
                )
            try:
                schema = _param_schema(
                    hints.get(name, param.annotation), f"task {key!r}: param {name!r}"
                )
            except ValueError:
                if strict:
                    raise
                schema = {}
            if param.default is inspect.Parameter.empty:
                required.append(name)
            else:
                try:
                    _data(param.default, name)
                    schema = {**schema, "default": param.default}
                except ValueError:
                    pass
            properties[name] = schema
    document: dict[str, Any] = {
        "$schema": _SCHEMA_DIALECT,
        "title": cls.__name__,
        "type": "object",
        "properties": properties,
        "additionalProperties": False,
    }
    if required:
        document["required"] = sorted(required)
    return document


def _kind_name(name: str) -> str:
    snake = _snake(name).replace("__", "_").strip("_") or "task"
    snake = re.sub(r"[^a-z0-9_.-]", "_", snake)
    return snake if snake[0].isalpha() else f"task_{snake}"[:64]


def _doc(code: Any) -> tuple[str | None, str | None]:
    """The first line of an object's docstring and the rest."""
    text = inspect.getdoc(code)
    if not text:
        return None, None
    first, _, rest = text.strip().partition("\n")
    return first.strip() or None, rest.strip() or None


def _definition(
    task: Any, role: str, key: str, code: Any, build_key: str | None, provenance: Any,
    strict: bool = True,
) -> dict[str, Any]:
    """The task's definition, the kind declaration its code makes (SDD 18 e.1): its name and
    title, its docstring's first line as its summary and the rest as its description, the JSON
    Schema of its params, the schemas it takes and gives, what it needs, and its provenance."""
    first, rest = _doc(code)
    title = first or getattr(code, "__name__", key)
    if isinstance(task, _Handle):
        settings = {
            "$schema": _SCHEMA_DIALECT,
            "title": title,
            "type": "object",
            "properties": {},
            "additionalProperties": False,
        }
        takes, gives = task.__dict__.get("_moruna_declared", (None, None))
        text: dict[str, str | None] = {}
    else:
        settings = _params_schema(type(task), key, strict)
        takes, gives = task.takes, task.gives
        text = getattr(type(task), "_moruna_need_text", {})
    needs = tuple(getattr(task, "needs", ()) or ())
    flows = {
        "source": ("nothing", "batches"),
        "function": ("batches", "batches"),
        "sink": ("batches", "nothing"),
    }[role]
    definition: dict[str, Any] = {
        "name": _kind_name(getattr(code, "__name__", key)),
        "role": role,
        "summary": first or title,
        "settings": settings,
        "inputs": flows[0],
        "outputs": flows[1],
        "needs": {
            "network": "no",
            "egress": [],
            "gpu": "no",
            "model_port": "yes" if needs else "no",
            "contracts": "no",
        },
        "deterministic": "no",
        "preserves_columns": "no",
        "isolation": "confined",
        "title": title,
    }
    if rest:
        definition["description"] = rest
    for which, schema in (("takes", _arrow(takes)), ("gives", _arrow(gives))):
        if schema is not None:
            definition[which] = schema
    if needs:
        definition["requires"] = [
            {"name": n, **({"description": text[n]} if text.get(n) else {})} for n in needs
        ]
    if build_key is not None:
        definition["provenance"] = {"build_key": build_key, **{
            k: v for k, v in provenance.as_dict().items() if v is not None
        }}
    return definition


class _Lowering:
    """What lowering a job's image tasks shares: the closures found, the provenance read and
    the lock's digest, once each."""

    def __init__(self, target: Target) -> None:
        self.target = target
        self.codes: dict[tuple[str, tuple[str, ...]], Any] = {}
        self.provenances: dict[tuple[Path, tuple[str, ...]], Any] = {}
        self.locks: dict[Path, str | None] = {}

    def code(self, module: str, source: Path, files: tuple[str, ...]) -> Any:
        from moruna import _closure  # noqa: PLC0415

        found = self.codes.get((module, files))
        if found is None:
            found = _closure.closure(module, source, [Path(f) for f in files])
            self.codes[(module, files)] = found
        return found

    def provenance(self, code: Any) -> Any:
        from moruna import _closure  # noqa: PLC0415

        key = (code.root, tuple(sorted(code.files)))
        found = self.provenances.get(key)
        if found is None:
            found = _closure.provenance(code)
            self.provenances[key] = found
        return found

    def lock(self, root: Path) -> str | None:
        from moruna import _build  # noqa: PLC0415

        if root not in self.locks:
            try:
                found = _build.find_lock(root, Path(self.target.lock) if self.target.lock else None)
                self.locks[root] = _build.lock_digest(found)
            except _build.Refused:
                self.locks[root] = None
        return self.locks[root]


def _code_of(task: Any, key: str, target: Target, lowering: _Lowering) -> tuple[str, Any, Any]:
    """A task's entry, its code object and its closure."""
    entry, code = _entry(task, key)
    module_name = entry.partition(":")[0]
    module = sys.modules.get(module_name) or sys.modules.get(getattr(code, "__module__", ""))
    path = getattr(module, "__file__", None)
    if path is None:
        raise ValueError(f"task {key!r}: {module_name} is not a file, which a build bundles")
    own = tuple(str(Path(path).resolve().parent / f) for f in getattr(task, "files", ()) or ())
    files = tuple(sorted({*own, *target.files}))
    return entry, code, lowering.code(module_name, Path(path), files)


def _image_task(
    task: Any, role: str, key: str, target: Target, built: Mapping[str, tuple[str, str]],
    lowering: _Lowering, *, strict: bool,
) -> tuple[dict[str, Any], dict[str, Any]]:
    """The settings and the definition of an image task: its image ``{build_key, digest,
    location}`` (the digest and location of an image built for its key, else null), its entry,
    its params and its needs. ``strict`` (for a spec) refuses what a spec cannot hold: code
    defined inside a function, a param that is not data or names a secret, an import the build
    cannot follow."""
    from moruna import _build, _closure  # noqa: PLC0415

    try:
        params: dict[str, Any] = {} if isinstance(task, _Handle) else _params(task, key)
        for name, value in params.items():
            if _SECRET.search(name):
                raise ValueError(
                    f"task {key!r}: the param {name!r} names a secret, and a spec holds none; "
                    "declare it in `needs` and supply it when the job runs"
                )
            _data(value, f"task {key!r}: param {name!r}")
    except (TypeError, ValueError):
        if strict:
            raise
        # A run or a plan of this process names the params it can: none, when one is not data.
        params = {}
    try:
        entry, code, closure = _code_of(task, key, target, lowering)
    except (_closure.Unresolved, ValueError) as exc:
        if strict:
            if isinstance(exc, ValueError):
                raise
            raise ValueError(f"task {key!r}: {exc}") from None
        # A run of this process holds the object; its image is unknown, and nothing builds it.
        image = {"build_key": None, "digest": None, "location": None}
        settings = {"image": image, "params": params, "needs": list(getattr(task, "needs", ()) or ())}
        code = task._handle.wrapped if isinstance(task, _Handle) else type(task)
        return settings, _definition(task, role, key, code, None, None, strict=False)
    build_key = _closure.build_key(
        closure, entry, lock=lowering.lock(closure.root), runtime=target.runtime,
        shim=_build.shim_digest(), platform=target.platform,
    )  # fmt: skip
    digest, location = built.get(build_key, (None, None))
    settings = {
        "image": {"build_key": build_key, "digest": digest, "location": location},
        "entry": entry,
        "params": params,
        "needs": list(getattr(task, "needs", ()) or ()),
    }
    if not settings["params"]:
        del settings["params"]
    if not settings["needs"]:
        del settings["needs"]
    provenance = lowering.provenance(closure)
    return settings, _definition(task, role, key, code, build_key, provenance, strict=strict)


# ---------------------------------------------------------------------------------------------
# Needs: supplied by the resolver, measured per task


class _Measure:
    """What a task's calls through one need cost: calls, bytes in and out, time."""

    __slots__ = ("bytes_in", "bytes_out", "calls", "ns")

    def __init__(self) -> None:
        self.calls = 0
        self.bytes_in = 0
        self.bytes_out = 0
        self.ns = 0

    def as_dict(self) -> dict[str, Any]:
        return {
            "calls": self.calls,
            "bytes_in": self.bytes_in,
            "bytes_out": self.bytes_out,
            "time_ms": round(self.ns / 1e6, 3),
        }


def _size(value: Any) -> int:
    """The bytes a value carries across a need: a buffer's length, a batch's or a table's
    Arrow bytes, the sum of a list's or a tuple's; 0 for anything else (a name, a number)."""
    if isinstance(value, (bytes, bytearray, memoryview)):
        return len(value)
    nbytes = getattr(value, "nbytes", None)
    if isinstance(nbytes, int):
        return nbytes
    if isinstance(value, (list, tuple)):
        return sum(_size(v) for v in value)
    return 0


class _Measured:
    """The object a resolver supplied, as the task receives it: every call through it, of a
    method, of the object itself or of its items, is counted in the task's measure of the need,
    and the object answers as it would."""

    def __init__(self, target: Any, measure: _Measure) -> None:
        object.__setattr__(self, "_target", target)
        object.__setattr__(self, "_measure", measure)

    def _timed(self, call: Callable[..., Any], *args: Any, **kwargs: Any) -> Any:
        import time  # noqa: PLC0415

        measure = self._measure
        measure.calls += 1
        measure.bytes_in += sum(_size(a) for a in args) + sum(_size(v) for v in kwargs.values())
        start = time.perf_counter_ns()
        try:
            result = call(*args, **kwargs)
        finally:
            measure.ns += time.perf_counter_ns() - start
        measure.bytes_out += _size(result)
        return result

    def __getattr__(self, name: str) -> Any:
        attribute = getattr(self._target, name)
        if callable(attribute):
            return functools.partial(self._timed, attribute)
        return attribute

    def __setattr__(self, name: str, value: Any) -> None:
        setattr(self._target, name, value)

    def __call__(self, *args: Any, **kwargs: Any) -> Any:
        return self._timed(self._target, *args, **kwargs)

    def __getitem__(self, key: Any) -> Any:
        return self._timed(self._target.__getitem__, key)

    def __contains__(self, key: Any) -> bool:
        return key in self._target

    def __len__(self) -> int:
        return len(self._target)

    def __iter__(self) -> Any:
        return iter(self._target)

    def __repr__(self) -> str:
        return repr(self._target)


def _supply(
    tasks: Sequence[tuple[str, str, Any]], resolver: Any, args: Mapping[str, Any]
) -> dict[str, dict[str, _Measure]]:
    """Supply every need of every task (each with its key and the label a refusal names it by)
    through ``resolver`` before the run starts, each object measured, and the run's arguments;
    a need it cannot supply is refused, every one of them named. Returns the measures, by task
    and need."""
    missing: list[str] = []
    supplied: list[tuple[Any, dict[str, Any]]] = []
    measures: dict[str, dict[str, _Measure]] = {}
    for key, at, task in tasks:
        needs = tuple(getattr(task, "needs", ()) or ())
        got: dict[str, Any] = {}
        for need in needs:
            if resolver is None:
                missing.append(f"{at} needs {need!r}, and no resolver was given")
                continue
            try:
                found = resolver.resolve(need)
            except LookupError:
                missing.append(f"{at} needs {need!r}, which the resolver cannot supply")
                continue
            except Exception as exc:  # the resolver's own failure, named
                missing.append(
                    f"{at} needs {need!r}, and the resolver failed: {type(exc).__name__}: {exc}"
                )
                continue
            measure = measures.setdefault(key, {}).setdefault(need, _Measure())
            got[need] = _Measured(found, measure)
        supplied.append((task, got))
    if missing:
        raise _config_error("; ".join(missing))
    for task, got in supplied:
        if isinstance(task, Task):
            task.__dict__["_moruna_ports"] = types.MappingProxyType(got)
            task.__dict__["_moruna_run_args"] = types.MappingProxyType(dict(args))
    return measures


class _Served:
    """A need of an image task, served from what the resolver supplied: a callable taking the
    request's bytes, or an object with ``call(request)``; its answer is bytes."""

    def __init__(self, need: str, handler: Any) -> None:
        call = getattr(handler, "call", None)
        self.call = call if callable(call) else handler
        if not callable(self.call):
            raise TypeError(
                f"the need {need!r} of an image task is a port: the resolver supplies a "
                "callable taking bytes, or an object with call(request), not "
                f"{type(handler).__name__}"
            )
        self.need = need

    def __call__(self, request: bytes) -> bytes:
        answer = self.call(bytes(request))
        if not isinstance(answer, (bytes, bytearray, memoryview)):
            raise TypeError(f"port {self.need!r} answered {type(answer).__name__}, not bytes")
        return bytes(answer)


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
            {"version": 2, "name": read["name"], "args": read["args"], "tasks": read["tasks"],
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

    ``platform`` and ``runtime`` say what the job's images are built for (by default this
    machine's platform, or ``linux/aarch64`` on a Mac, and Python's official
    ``python:3.14-slim``); ``files`` are data files every task of the user's code reads. Each is
    part of every build key.
    """

    def __init__(
        self,
        name: str,
        *,
        platform: str | None = None,
        runtime: tuple[str, str] | None = None,
        lock: str | os.PathLike[str] | None = None,
        files: Sequence[str | os.PathLike[str]] = (),
    ) -> None:
        if not isinstance(name, str) or not name:
            raise TypeError(f"a job is named by a non-empty string, not {name!r}")
        self.name: str | None = name
        self._entries: dict[str, _Entry] = {}
        # A job read from a document that also sets run settings: the document, whose settings
        # its spec, its check and its plan keep, and the fields it sets.
        self._base: bytes | None = None
        self._settings: tuple[str, ...] = ()
        self._args: dict[str, str] = {}
        # The images built for this job's keys: build key -> (digest, location).
        self._built: dict[str, tuple[str, str]] = {}
        self._target = _default_target()
        self.target(platform=platform, runtime=runtime, lock=lock, files=files)

    def target(
        self,
        *,
        platform: str | None = None,
        runtime: tuple[str, str] | None = None,
        lock: str | os.PathLike[str] | None = None,
        files: Sequence[str | os.PathLike[str]] | None = None,
    ) -> None:
        """What the job's images are built for, each given value replacing the job's: the
        ``platform`` (``linux/aarch64`` or ``linux/x86_64``), the ``runtime`` as ``(layout,
        reference)`` of an OCI layout holding it, the ``lock`` and the data ``files`` every task
        reads. The spec's build keys follow."""
        from moruna import _build  # noqa: PLC0415

        target = self._target
        try:
            if platform is not None:
                target = dataclasses.replace(target, platform=_build.parse_platform(platform).name)
            if runtime is not None:
                layout, reference = runtime
                found = _build.read_runtime_layout(
                    Path(layout), reference, _build.parse_platform(target.platform)
                )
                target = dataclasses.replace(
                    target, runtime=found.digest, runtime_layout=str(Path(layout).absolute()),
                    runtime_reference=reference,
                )  # fmt: skip
        except _build.Refused as exc:
            raise _config_error(f"job target refused: {exc}") from exc
        if lock is not None:
            target = dataclasses.replace(target, lock=str(Path(lock).absolute()))
        if files is not None:
            target = dataclasses.replace(
                target, files=tuple(sorted(str(Path(f).absolute()) for f in files))
            )
        self._target = target

    @property
    def platform(self) -> str:
        """The platform the job's images are built for."""
        return self._target.platform

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

    def arg(self, name: str, type: str) -> None:  # noqa: A002, the declaration's own word
        """Declare a run argument: ``name`` (1 to 64 letters, digits and ``_``, starting with a
        letter) of ``type`` (``string``, ``int``, ``float``, ``bool``, ``date`` or
        ``timestamp``). The spec carries the declaration; every run supplies the value
        (``moruna.run(job, args=...)``, ``moruna run JOB --arg NAME=VALUE``) and its tasks read
        it as ``self.args[name]``."""
        if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z][A-Za-z0-9_]{0,63}", name):
            raise ValueError(
                f"an argument's name is 1 to 64 of ASCII letters, digits and `_`, starting with "
                f"a letter, not {name!r}"
            )
        if type not in ARG_TYPES:
            raise ValueError(f"an argument's type is one of {', '.join(ARG_TYPES)}, not {type!r}")
        if name in self._args:
            raise ValueError(f"the job declares the argument {name!r} already")
        self._args[name] = type

    @property
    def args(self) -> Mapping[str, str]:
        """The arguments the job declares, name to type, in name order."""
        return types.MappingProxyType(dict(sorted(self._args.items())))

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
        """The job's spec, at any step of building it: every task defined inline, each image
        task with its build key and, once built, its image's digest and location. Refused,
        naming the task, when a task cannot be written: an ``IteratorSource``, code defined
        inside a function, a param that is not data or that names a secret, an import a build
        cannot follow."""
        return JobSpec(
            _core.job_document(
                self.name, self._lowered(spec=True), self._base, self._arg_list()
            )
        )

    def _arg_list(self) -> list[tuple[str, str]]:
        return sorted(self._args.items())

    def write(self, target: Any) -> JobSpec:
        """Write the spec's bytes through the writer ``target`` is: a file path, a callable
        taking bytes, or a ``moruna.Writer``. Returns the spec."""
        spec = self.spec()
        writer_of(target).write_spec(spec.bytes())
        return spec

    @classmethod
    def read(cls, source: Any) -> Job:
        """The job a spec holds: a ``JobSpec``, its bytes, or a path to them (a version 2
        document, or a version 1 JSON document). Its image tasks run their images. A document
        that also sets run settings keeps them, in its spec and its plan (``job.settings``
        names them); such a job is run by ``moruna run``, and ``moruna.run`` refuses it."""
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
        job._args = {a["name"]: a["type"] for a in read["args"]}
        for task in read["tasks"]:
            role = task["role"]
            made = _Read(
                role, task["kind"], task["settings"], list(task["ports"]), task["definition"]
            )
            job._entries[task["id"]] = _Entry(task["id"], role, made, tuple(task["inputs"]))
        return job

    # Images ----------------------------------------------------------------------------------

    def status(self) -> list[dict[str, Any]]:
        """Each image task's image, read from the spec: its key, its role, its state (``not
        built``, ``built`` into a local layout, ``pushed`` to a registry, or ``external``, an
        image Moruna did not build), its build key, its digest and its location."""
        rows = []
        for e, settings in self._images():
            image = settings.get("image") or {}
            key, digest, location = image.get("build_key"), image.get("digest"), image.get("location")
            if digest is None:
                state = "not built"
            elif key is None:
                state = "external"
            elif str(location).startswith("/"):
                state = "built"
            else:
                state = "pushed"
            rows.append({
                "task": e.key, "role": e.role, "state": state, "build_key": key,
                "digest": digest, "location": location,
            })  # fmt: skip
        return rows

    def _images(self) -> list[tuple[_Entry, dict[str, Any]]]:
        """Each image task and its settings, as the spec writes them (unspeccable code: its
        image unknown)."""
        lowered = {t[1]: t for t in self._lowered(spec=False)}
        found = []
        for e in self._entries.values():
            kind, settings = lowered[e.key][6], lowered[e.key][7]
            if kind == "image" and settings is not None:
                found.append((e, json.loads(settings)))
        return found

    def build(
        self,
        out: str | os.PathLike[str] = "moruna-images",
        *,
        registry: str | None = None,
        platform: str | None = None,
        ports: Any = None,
        files: Sequence[str | os.PathLike[str]] | None = None,
        lock: str | os.PathLike[str] | None = None,
        runtime: tuple[str, str] | None = None,
        cache: str | os.PathLike[str] | None = None,
        index_url: str | None = None,
        find_links: Sequence[str] = (),
        no_index: bool = False,
    ) -> list[dict[str, Any]]:
        """Build one image per task of the user's code into the OCI layout ``out`` and return
        the job's status (``job.status()``): each image named by its build key, layers shared by
        content, an image already built for a key reused. ``registry`` (``HOST/REPOSITORY``)
        also pushes every image there, with credentials the resolver ``ports`` supplies for the
        need ``registry:<HOST>`` (never in the spec). ``platform``, ``files``, ``lock`` and
        ``runtime`` retarget the job first, as ``job.target`` does. The spec then names each
        image's digest and location: the runnable spec. Raises ``moruna.ConfigError`` naming
        the cause when the build is refused."""
        from moruna import _build  # noqa: PLC0415

        self.target(platform=platform, runtime=runtime, lock=lock, files=files)
        resolver = resolver_of(ports)
        try:
            options = _build.options(
                out=out, platform=self._target.platform, lock=self._target.lock,
                runtime_layout=self._target.runtime_layout,
                runtime_reference=self._target.runtime_reference, cache=cache,
                index_url=index_url, find_links=find_links, no_index=no_index,
                version=_core.__version__, registry=registry,
                credential=_credential(resolver) if registry else None,
            )  # fmt: skip
            tasks = self._build_tasks()
            built = _build.build_images(tasks, options)
        except _build.Refused as exc:
            raise _config_error(f"build refused: {exc}") from exc
        for image in built:
            self._built[image.key] = (image.digest, image.location)
        return self.status()

    def _build_tasks(self) -> list[Any]:
        """What the build makes of each image task of the user's code: its entry, its closure,
        its key and the distributions it imports."""
        from moruna import _build, _closure  # noqa: PLC0415

        lowering = _Lowering(self._target)
        tasks: dict[str, Any] = {}
        refusals: list[str] = []
        for e in self._entries.values():
            task = e.task
            if isinstance(task, _Read):
                if task.kind == "image" and (task.image or {}).get("digest") is None:
                    refusals.append(
                        f"job[{e.key!r}]: read from a spec, it has no code here to build; build "
                        "the job its code makes (moruna build MODULE:JOB)"
                    )
                continue
            if not isinstance(task, Task) or (
                isinstance(task, _Handle) and not isinstance(task._handle, _core.KernelSpec)
            ):
                continue
            try:
                settings, definition = _image_task(
                    task, e.role, e.key, self._target, {}, lowering, strict=True
                )
                entry, _, code = _code_of(task, e.key, self._target, lowering)
            except (ValueError, _closure.Unresolved) as exc:
                refusals.append(f"job[{e.key!r}]: {exc}")
                continue
            key = settings["image"]["build_key"]
            if key in tasks:
                continue
            tasks[key] = _build.Task(
                name=e.key, entry=entry, code=code, key=key,
                distributions=_closure.distributions(code.imports),
                provenance=definition.get("provenance", {}),
            )  # fmt: skip
        if refusals:
            raise _build.Refused("; ".join(refusals))
        return list(tasks.values())

    # Checking and explaining ---------------------------------------------------------------

    def check(self) -> None:
        """Check the job without running it: its names, ports and edges, each task's kind and
        settings against its definition, and along every edge whose two ends declare one, that
        the schema one task gives is the schema the next takes. Raises ``moruna.ConfigError``
        naming every refusal."""
        refusals = list(
            _core.check_job(
                self.name, self._lowered(spec=False), self._base, self._arg_list()
            )
        )
        if refusals:
            raise _config_error("; ".join(refusals))

    def explain(self, planner: str | None = None) -> dict[str, Any]:
        """The plan Moruna would run the job by, as a dict, without running it: its steps in the
        order they start, each with its tasks, the steps it waits for, its group, its resources
        and its estimated cost; ``groups`` and ``totals``; and ``images``, each image task's
        image as ``status()`` gives it. The same function plans ``moruna explain JOB``. Raises
        ``moruna.ConfigError`` naming the field when the job is refused, and
        ``moruna.PlanError`` for an unknown planner."""
        plan: dict[str, Any] = json.loads(
            _core.explain_job(
                self.name, self._lowered(spec=False), self._base, self._arg_list(),
                planner=planner,
            )  # fmt: skip
        )
        plan["images"] = self.status()
        return plan

    # Lowering ------------------------------------------------------------------------------

    def _lowered(
        self, *, spec: bool, make: bool = False
    ) -> list[tuple[Any, ...]]:
        """The tasks as the extension takes them: ``(role, id, at, object, inputs, ports, kind,
        settings, definition)``. ``spec``: for a spec, refusing what it cannot hold. ``make``:
        for a run, with each object the run holds."""
        lowering = _Lowering(self._target)
        lowered = []
        for e in self._entries.values():
            at = f"job[{e.key!r}]"
            task = e.task
            obj: Any = None
            kind: str | None = None
            settings: str | None = None
            definition: str | None = None
            ports: list[str] | None = None
            try:
                if isinstance(task, _Read):
                    kind, settings, ports = task.kind, json.dumps(task.settings), task.port_names
                    definition = json.dumps(task.definition) if task.definition else None
                elif isinstance(task, _Handle) and not isinstance(task._handle, _core.KernelSpec):
                    obj = task._handle
                elif isinstance(task, Task):
                    made, defined = _image_task(
                        task, e.role, e.key, self._target, self._built, lowering, strict=spec
                    )
                    kind, settings, definition = "image", json.dumps(made), json.dumps(defined)
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
            lowered.append(
                (e.role, e.key, at, obj, list(e.inputs), ports, kind, settings, definition)
            )
        return lowered

    # Showing ---------------------------------------------------------------------------------

    def __repr__(self) -> str:
        lines = [f"moruna.Job({self.name!r}, {len(self._entries)} tasks)"]
        if self._settings:
            lines.append(f"  settings: {', '.join(self._settings)}")
        if self._args:
            lines.append(
                "  args: " + ", ".join(f"{n}: {t}" for n, t in sorted(self._args.items()))
            )
        images = {row["task"]: row for row in self.status()}
        for e in self._entries.values():
            lines.append("  " + _describe(e, images.get(e.key)))
        return "\n".join(lines)

    __str__ = __repr__


def _credential(resolver: Any) -> Callable[[str], Any]:
    """Registry credentials through the resolver: the need ``registry:<HOST>``, or none when
    the resolver cannot supply it."""

    def credential(host: str) -> Any:
        if resolver is None:
            return None
        try:
            found = resolver.resolve(f"registry:{host}")
        except LookupError:
            return None
        if isinstance(found, str):
            return {"token": found}
        return dict(found)

    return credential


def _schema_text(schema: Any) -> str:
    if schema is None:
        return "-"
    return "{" + ", ".join(f"{f.name}: {f.type}" for f in schema) + "}"


def _definition_schema_text(schema: Any) -> str:
    if not schema:
        return "-"
    return "{" + ", ".join(f"{f['name']}: {f['type']}" for f in schema["fields"]) + "}"


def _describe(e: _Entry, image: Mapping[str, Any] | None) -> str:
    """One line of metadata per task: key, role, kind or entry, params, inputs, needs, schemas
    and, for an image task, its image's state."""
    task = e.task
    params: Any = {}
    takes = gives = "-"
    if isinstance(task, _Read):
        what = task.kind
        if task.kind == "image":
            what = f"image {task.settings.get('entry') or '-'}"
            params = task.settings.get("params", {})
        else:
            params = task.settings
        if task.definition:
            takes = _definition_schema_text(task.definition.get("takes"))
            gives = _definition_schema_text(task.definition.get("gives"))
    elif isinstance(task, _Handle):
        handle = task._handle
        if isinstance(handle, _core.KernelSpec):
            what = f"image {_reference(getattr(handle.wrapped, '__wrapped__', handle.wrapped))}"
        else:
            what = repr(handle)
    elif isinstance(task, Task):
        what = f"image {_reference(type(task))}"
        try:
            params = _params(task, e.key)
        except (TypeError, ValueError) as exc:
            params = f"<{exc}>"
    else:
        what = repr(task)
    if not isinstance(task, _Read):
        takes = _schema_text(getattr(task, "takes", None))
        gives = _schema_text(getattr(task, "gives", None))
    needs = ",".join(getattr(task, "needs", ()) or ()) or "-"
    inputs = ",".join(e.inputs) or "-"
    shown = json.dumps(params, sort_keys=True, default=lambda v: f"<{type(v).__name__}>")
    line = (
        f"{e.key}  {e.role}  {what}  params={shown}  inputs={inputs}  needs={needs}  "
        f"takes={takes}  gives={gives}"
    )
    if image is not None:
        line += f"  image={image['state']}"
        if image["build_key"]:
            line += f" key={image['build_key'][:19]}"
        if image["digest"]:
            line += f" digest={image['digest'][:19]} at={image['location']}"
    return line


def _pulled(job: Job, resolver: Any) -> str | None:
    """Every registry image the job's image tasks run, pulled into this machine's image cache
    before anything starts; the cache, or ``None`` when no task names one."""
    from moruna import _build  # noqa: PLC0415

    locations = sorted({
        row["location"] for row in job.status()
        if row["location"] and not str(row["location"]).startswith("/")
    })  # fmt: skip
    if not locations:
        return None
    cache = _build.default_pulled()
    refused: list[str] = []
    for location in locations:
        try:
            _build.pull(location, cache, _credential(resolver))
        except _build.Refused as exc:
            refused.append(f"{location}: {exc}")
    if refused:
        raise _config_error("images not available: " + "; ".join(refused))
    return str(cache)


def _args_json(given: dict[str, Any]) -> str:
    """A run's arguments as the JSON its checks, its tasks and its report read: a
    ``datetime.datetime`` as RFC 3339 (``YYYY-MM-DDTHH:MM:SS``, its fraction, its offset) and a
    ``datetime.date`` as ``YYYY-MM-DD``; every other value as JSON writes it. A value JSON cannot
    write is refused by name, never written as its ``str``."""

    def encode(value: Any) -> Any:
        if isinstance(value, datetime.datetime):
            return value.isoformat()
        if isinstance(value, datetime.date):
            return value.isoformat()
        raise TypeError(f"a run argument is a string, number, bool, date or datetime, not {value!r}")

    return json.dumps(given, default=encode)


def run_job(
    job: Job, *, ports: Any, spec_to: Any, explain: bool, args: Any, options: dict[str, Any]
) -> Any:
    """``moruna.run(job, ...)``: the arguments checked, the needs supplied, the registry
    images pulled, the spec written when asked, the job run, its report written beside the
    spec."""
    from moruna._report import RunReport  # noqa: PLC0415

    if not isinstance(job, Job):
        raise TypeError(f"moruna.run takes a moruna.Job, not {type(job).__name__}")
    if job.settings:
        raise _config_error(
            f"this job was read from a document that sets {', '.join(job.settings)}; those are "
            "moruna.run's arguments, so run the document as it is with `moruna run`, or run a "
            "spec of tasks and edges"
        )
    encoded = _args_json(dict(args or {}))
    refused = _core.check_args(job._arg_list(), encoded)
    if refused:
        raise _config_error("; ".join(refused))
    # Every task reads an argument as its JSON value, as a task inside an image does from
    # MORUNA_ARGS: a timestamp or a date as its text, whatever object the caller gave.
    given = json.loads(encoded)
    resolver = resolver_of(ports)
    live = [
        (e.key, f"job[{e.key!r}]", e.task)
        for e in job._entries.values()
        if not isinstance(e.task, _Read)
    ]
    measures = _supply(live, resolver, given)
    # The needs of the image tasks are ports: each served by what the resolver supplied.
    served: dict[str, Any] = {}
    missing: list[str] = []
    for e in job._entries.values():
        if not isinstance(e.task, _Read):
            continue
        for need in e.task.needs:
            if need in served:
                continue
            if resolver is None:
                missing.append(f"job[{e.key!r}] needs {need!r}, and no resolver was given")
                continue
            try:
                served[need] = _Served(need, resolver.resolve(need))
            except LookupError:
                missing.append(f"job[{e.key!r}] needs {need!r}, which the resolver cannot supply")
            except TypeError as exc:
                missing.append(f"job[{e.key!r}]: {exc}")
    if missing:
        raise _config_error("; ".join(missing))
    try:
        spec: JobSpec | None = job.spec()
    except _core.ConfigError:
        if spec_to is not None:
            raise
        spec = None
    held = [k for k, _, _ in live]
    drafts = [
        row for row in job.status() if row["digest"] is None and row["task"] not in held
    ]
    if drafts:
        raise _config_error(
            "the job's spec is a draft: images not built ("
            + "; ".join(f"job[{r['task']!r}] (build key {r['build_key']})" for r in drafts)
            + "); build the job, and run the spec the build wrote"
        )
    pulled = _pulled(job, resolver)
    writer = writer_of(spec_to) if spec_to is not None else None
    if writer is not None and spec is not None:
        writer.write_spec(spec.bytes())
    lowered = job._lowered(spec=False, make=True)
    try:
        text = _core.run(
            job.name or "job",
            lowered,
            job_digest=spec.digest() if spec is not None else None,
            explain=explain,
            args=encoded,
            arg_types=job._arg_list(),
            held=held,
            served=served,
            pulled_images=pulled,
            **options,
        )
    except _core.MorunaError as exc:
        report = getattr(exc, "report", None)
        if writer is not None and report is not None:
            writer.write_report(report.bytes())
        raise
    report = RunReport._from_json(_with_needs(text, measures))
    if writer is not None:
        writer.write_report(report.bytes())
    return report


def _with_needs(text: str, measures: Mapping[str, Mapping[str, _Measure]]) -> str:
    """The report with each in-process task's needs as the resolver's objects measured them,
    beside what the ports measured for image tasks."""
    if not measures:
        return text
    document = json.loads(text)
    for task in document.get("tasks", []):
        found = measures.get(task.get("key"))
        if found:
            task["needs"] = {name: m.as_dict() for name, m in sorted(found.items())}
    return json.dumps(document)


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


# ---------------------------------------------------------------------------------------------
# `moruna build JOB`


def _job_of(target: str, retarget: Mapping[str, Any]) -> tuple[Job, Path]:
    """The job ``moruna build`` is given, and where its runnable spec goes by default: a spec
    file (rebuilt from the code its entries name, imported from the current directory and the
    interpreter's path), or ``<module>:<attribute>`` naming a ``moruna.Job``."""
    from moruna import _build, _check  # noqa: PLC0415

    path = Path(target)
    if path.suffix in (".mjob", ".json") and path.is_file():
        read = Job.read(path)
        return _rebuilt(read, retarget), path
    module_name, sep, attribute = target.rpartition(":")
    if not sep or not module_name or not attribute:
        raise _build.Refused(
            f"{target}: name a job's spec (JOB.mjob) or a job as <module>:<attribute>"
        )
    try:
        module = _check._load(module_name)
    except Exception as exc:  # noqa: BLE001, a module that does not load is a refusal
        raise _build.Refused(f"{module_name}: {type(exc).__name__}: {exc}") from exc
    job = getattr(module, attribute, None)
    if not isinstance(job, Job):
        raise _build.Refused(f"{target} is not a moruna.Job")
    if retarget:
        job.target(**retarget)
    return job, Path(f"{attribute}.mjob")


def _rebuilt(read: Job, retarget: Mapping[str, Any]) -> Job:
    """A job read from a spec, its image tasks made again from the code their entries name: the
    class constructed with its params, the decorated function found. Each must make the build key
    the spec names; a key that differs is refused, naming both, since the spec was written from
    other code or for another platform."""
    from moruna import _build  # noqa: PLC0415

    sys.path.insert(0, os.getcwd())
    job = Job(read.name or "job")
    job._args = dict(read._args)
    if retarget:
        job.target(**retarget)
    expected: dict[str, str | None] = {}
    for e in read._entries.values():
        task = e.task
        if isinstance(task, _Read) and task.kind == "image" and task.settings.get("entry"):
            entry = task.settings["entry"]
            module_name, _, attribute = entry.partition(":")
            try:
                target: Any = importlib.import_module(module_name)
                for part in attribute.split("."):
                    target = getattr(target, part)
                made = target(**task.settings.get("params", {})) if isinstance(target, type) else target
            except Exception as exc:  # noqa: BLE001, the entry's failure is the refusal
                raise _build.Refused(
                    f"job[{e.key!r}]: the entry {entry} does not make the task here: "
                    f"{type(exc).__name__}: {exc}"
                ) from exc
            expected[e.key] = (task.image or {}).get("build_key")
            job._entries[e.key] = _Entry(e.key, e.role, made, e.inputs)
        else:
            job._entries[e.key] = e
    keys = {row["task"]: row["build_key"] for row in job.status()}
    differ = [
        f"job[{k}] names {v} and its code here makes {keys.get(k)}"
        for k, v in expected.items()
        if v is not None and keys.get(k) != v
    ]
    if differ:
        raise _build.Refused(
            "the spec's build keys are not the code's: " + "; ".join(differ)
            + " (the code changed, or the spec is for another platform, lock or runtime)"
        )
    return job


def build_command(args: Any) -> dict[str, Any]:
    """``moruna build JOB``: the job built, its runnable spec written; what the command prints."""
    from moruna import _build  # noqa: PLC0415

    retarget = {
        "platform": args.platform,
        "runtime": (args.runtime_layout, args.runtime_reference) if args.runtime_layout else None,
        "lock": args.lock,
        "files": args.file or None,
    }
    retarget = {k: v for k, v in retarget.items() if v is not None}
    try:
        job, default_spec = _job_of(args.job, retarget)
    except _core.ConfigError as exc:
        raise _build.Refused(str(exc)) from exc
    resolver = load_resolver(args.resolver) if args.resolver else None
    try:
        job.build(
            args.out, registry=args.registry, ports=resolver, cache=args.cache,
            index_url=args.index_url, find_links=args.find_links, no_index=args.no_index,
        )  # fmt: skip
    except _core.ConfigError as exc:
        raise _build.Refused(str(exc).removeprefix("build refused: ")) from exc
    spec_path = Path(args.spec) if args.spec else default_spec
    spec = job.write(spec_path)
    return {
        "spec": str(spec_path),
        "job_digest": spec.digest(),
        "tasks": job.status(),
    }


# ---------------------------------------------------------------------------------------------
# An image Moruna did not build


def image(
    reference: str,
    *,
    role: str = "function",
    entry: str | None = None,
    params: Mapping[str, Any] | None = None,
    needs: Sequence[str] | Mapping[str, str] = (),
    takes: Any = None,
    gives: Any = None,
    title: str | None = None,
    description: str | None = None,
) -> Any:
    """A task of an image Moruna did not build: ``reference`` is ``HOST/REPOSITORY@sha256:...``,
    ``role`` its role, ``entry`` what its entry point reads as ``MORUNA_ENTRY``, ``params`` its
    data, ``needs`` the ports it calls (a mapping gives each its description), ``takes`` and
    ``gives`` the Arrow schemas it declares. Its status is ``external``; the runner pulls it by
    its digest before the run starts. It follows the contract of ``docs/images.md``."""
    if role not in ROLES:
        raise ValueError(f"a task is a source, a function or a sink, not {role!r}")
    path, at, digest = reference.partition("@")
    host, _, repository = path.partition("/")
    if not at or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest) or not repository or not (
        "." in host or ":" in host or host == "localhost"
    ):
        raise ValueError(
            f"an image is named HOST/REPOSITORY@sha256:<64 hex>, its digest exact, not "
            f"{reference!r}"
        )
    given = dict(params or {})
    for name, value in given.items():
        _data(value, f"param {name!r}")
    described = dict(needs) if isinstance(needs, Mapping) else {n: None for n in needs}
    properties = {name: {} for name in sorted(given)}
    name = _kind_name(repository.rsplit("/", 1)[-1].replace("-", "_"))
    flows = {
        "source": ("nothing", "batches"),
        "function": ("batches", "batches"),
        "sink": ("batches", "nothing"),
    }[role]
    definition: dict[str, Any] = {
        "name": name,
        "role": role,
        "summary": title or f"The image {path}",
        "settings": {
            "$schema": _SCHEMA_DIALECT, "title": name, "type": "object",
            "properties": properties, "additionalProperties": False,
        },
        "inputs": flows[0],
        "outputs": flows[1],
        "needs": {"network": "no", "egress": [], "gpu": "no",
                  "model_port": "yes" if described else "no", "contracts": "no"},
        "deterministic": "no",
        "preserves_columns": "no",
        "isolation": "confined",
        "title": title or name,
    }  # fmt: skip
    if description:
        definition["description"] = description
    for which, schema in (("takes", _arrow(takes)), ("gives", _arrow(gives))):
        if schema is not None:
            definition[which] = schema
    if described:
        definition["requires"] = [
            {"name": n, **({"description": d} if d else {})} for n, d in described.items()
        ]
    settings: dict[str, Any] = {
        "image": {"build_key": None, "digest": digest, "location": reference},
    }
    if entry is not None:
        settings["entry"] = entry
    if given:
        settings["params"] = given
    if described:
        settings["needs"] = list(described)
    return _Read(role, "image", settings, None, definition)
