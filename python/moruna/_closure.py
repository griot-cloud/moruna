"""What a task Moruna builds is made of, known before it is built (SDD 30): its code's import
closure inside its project, the distributions that closure imports, where the code came from,
and the build key that names the image.

The closure is found statically. From the module that defines the task, every ``import`` and
``from ... import`` is followed, transitively, while it resolves to a file under the project's
import root (the directory the module is imported from); the ``__init__.py`` of every package on
the way is part of it. An import outside the root is the standard library's, Moruna's own (the
image's stand-in) or a distribution's, found through installed metadata. An import that cannot
be resolved statically, ``importlib.import_module(name)`` with a name computed at run time, is
refused by name; files the code reads are named explicitly (``files=``). An import inside
``try: ... except ImportError`` or ``if TYPE_CHECKING:`` is optional: it is followed when it
resolves, and left out when it does not.

The build key hashes each Python file of the closure as its syntax tree with docstrings
removed, not its bytes: comments, formatting and docstrings are not in the tree, so editing them
keeps the key and needs no rebuild. A data file is hashed by its bytes. Nothing here imports the
engine.
"""

from __future__ import annotations

import ast
import dataclasses
import functools
import hashlib
import importlib.metadata
import json
import re
import subprocess
import sys
from collections.abc import Iterable, Sequence
from pathlib import Path
from typing import Any

#: The version of the build key's document: a change to what is hashed is a new version.
KEY_VERSION = 1

#: The names an image's ``import moruna`` finds in its shim, not a distribution.
PROVIDED = frozenset({"moruna", "__future__"})

#: The name of a call that imports a module given at run time.
_DYNAMIC = {("importlib", "import_module"), ("", "__import__"), ("importlib", "__import__")}


class Unresolved(Exception):
    """The closure cannot be found statically, with the sentence that says where and why."""


def normal(name: str) -> str:
    """A distribution name as PEP 503 normalises it."""
    return re.sub(r"[-_.]+", "-", name).lower()


@dataclasses.dataclass(frozen=True)
class Code:
    """A task's code, found: the module it is in, the import root, the files of its closure
    (each by its path under the root) and the top-level names of what it imports from
    distributions."""

    entry: str
    module: str
    source: Path
    root: Path
    files: dict[str, Path]
    data: frozenset[str]
    imports: tuple[str, ...]

    def digests(self) -> dict[str, str]:
        """Each file's digest: a Python file's stripped syntax tree, a data file's bytes."""
        return {
            path: (
                tree_digest(file)
                if path not in self.data and file.suffix == ".py"
                else bytes_digest(file)
            )
            for path, file in sorted(self.files.items())
        }

    def file_digests(self) -> dict[str, str]:
        """Each file's bytes' digest, as the image holds them (for its provenance)."""
        return {path: bytes_digest(file) for path, file in sorted(self.files.items())}


def bytes_digest(path: Path) -> str:
    return "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()


class _Docstrings(ast.NodeTransformer):
    """A tree with the docstring of every module, class and function taken out."""

    def _strip(self, node: Any) -> Any:
        self.generic_visit(node)
        body = node.body
        if (
            body
            and isinstance(body[0], ast.Expr)
            and isinstance(body[0].value, ast.Constant)
            and isinstance(body[0].value.value, str)
        ):
            node.body = body[1:] or [ast.Pass()]
        return node

    visit_Module = _strip
    visit_ClassDef = _strip
    visit_FunctionDef = _strip
    visit_AsyncFunctionDef = _strip


def stripped_tree(text: str, where: str) -> str:
    """The syntax tree of Python ``text`` with its docstrings removed, as text with no
    positions: what the build key hashes."""
    try:
        tree = ast.parse(text, filename=where)
    except SyntaxError as exc:
        raise Unresolved(f"{where}: does not parse: {exc}") from exc
    return ast.dump(_Docstrings().visit(tree), include_attributes=False)


def tree_digest(path: Path) -> str:
    return "sha256:" + hashlib.sha256(
        stripped_tree(path.read_text(encoding="utf-8"), str(path)).encode()
    ).hexdigest()


def import_root(module: str, source: Path) -> Path:
    """The directory ``module`` (whose file is ``source``) is imported from."""
    depth = len(module.split("."))
    package = source.name == "__init__.py"
    return source.resolve().parents[depth if package else depth - 1]


@functools.cache
def _stdlib() -> frozenset[str]:
    return frozenset(sys.stdlib_module_names) | frozenset(sys.builtin_module_names)


@functools.cache
def _distributions_of() -> dict[str, tuple[str, ...]]:
    """Top-level import name to the distributions that provide it, from installed metadata."""
    return {
        name: tuple(sorted({normal(d) for d in dists}))
        for name, dists in importlib.metadata.packages_distributions().items()
    }


def _module_file(root: Path, dotted: str) -> tuple[Path | None, bool]:
    """The file of module ``dotted`` under ``root``: ``(file, is_package)``; a namespace
    package (a directory with no ``__init__.py``) is ``(None, True)``; ``(None, False)`` when
    there is no such module under the root."""
    base = root.joinpath(*dotted.split("."))
    if (base / "__init__.py").is_file():
        return base / "__init__.py", True
    if base.with_suffix(".py").is_file():
        return base.with_suffix(".py"), False
    native = sorted(base.parent.glob(f"{base.name}.*.so")) + sorted(base.parent.glob(f"{base.name}.*.pyd"))
    if native:
        return native[0], False
    if base.is_dir():
        return None, True
    return None, False


@dataclasses.dataclass
class _Import:
    names: list[str]  # absolute dotted names this statement may import, most specific last
    line: int
    optional: bool


class _Imports(ast.NodeVisitor):
    """Every import of a module, absolute, each marked optional when a missing module is
    handled; and every import whose module is computed at run time."""

    def __init__(self, package: str) -> None:
        self.package = package
        self.found: list[_Import] = []
        self.dynamic: list[tuple[int, str]] = []
        self.optional = 0

    def _absolute(self, module: str | None, level: int) -> str:
        if level == 0:
            return module or ""
        parts = self.package.split(".") if self.package else []
        if level - 1 > len(parts):
            raise Unresolved(f"a relative import goes above the top-level package {self.package!r}")
        base = parts[: len(parts) - (level - 1)]
        return ".".join([*base, module] if module else base)

    def visit_Try(self, node: ast.Try) -> None:
        handles = any(_catches_import_error(h.type) for h in node.handlers)
        if handles:
            self.optional += 1
        for child in node.body:
            self.visit(child)
        if handles:
            self.optional -= 1
        for child in [*node.handlers, *node.orelse, *node.finalbody]:
            self.visit(child)

    visit_TryStar = visit_Try  # type: ignore[assignment]

    def visit_If(self, node: ast.If) -> None:
        test = node.test
        typing = (isinstance(test, ast.Name) and test.id == "TYPE_CHECKING") or (
            isinstance(test, ast.Attribute) and test.attr == "TYPE_CHECKING"
        )
        if typing:
            self.optional += 1
        for child in node.body:
            self.visit(child)
        if typing:
            self.optional -= 1
        for child in node.orelse:
            self.visit(child)

    def visit_Import(self, node: ast.Import) -> None:
        for alias in node.names:
            parts = alias.name.split(".")
            names = [".".join(parts[: i + 1]) for i in range(len(parts))]
            self.found.append(_Import(names, node.lineno, self.optional > 0))

    def visit_ImportFrom(self, node: ast.ImportFrom) -> None:
        base = self._absolute(node.module, node.level)
        parts = base.split(".") if base else []
        names = [".".join(parts[: i + 1]) for i in range(len(parts))]
        # `from x import y` may import the submodule x.y.
        subs = [f"{base}.{a.name}" if base else a.name for a in node.names if a.name != "*"]
        self.found.append(_Import(names, node.lineno, self.optional > 0))
        for sub in subs:
            self.found.append(_Import([*names, sub], node.lineno, True))

    def visit_Call(self, node: ast.Call) -> None:
        func = node.func
        name = (
            ("", func.id)
            if isinstance(func, ast.Name)
            else (func.value.id, func.attr)
            if isinstance(func, ast.Attribute) and isinstance(func.value, ast.Name)
            else None
        )
        if name in _DYNAMIC:
            first = node.args[0] if node.args else None
            if isinstance(first, ast.Constant) and isinstance(first.value, str):
                if first.value.startswith("."):
                    self.dynamic.append((node.lineno, f"{name[1]}({first.value!r}) is relative"))
                else:
                    parts = first.value.split(".")
                    names = [".".join(parts[: i + 1]) for i in range(len(parts))]
                    self.found.append(_Import(names, node.lineno, self.optional > 0))
            elif self.optional == 0:
                self.dynamic.append(
                    (node.lineno, f"{name[1]}(...) names its module at run time")
                )
        self.generic_visit(node)


def _catches_import_error(node: ast.expr | None) -> bool:
    if node is None:
        return True
    if isinstance(node, ast.Tuple):
        return any(_catches_import_error(e) for e in node.elts)
    name = node.id if isinstance(node, ast.Name) else getattr(node, "attr", "")
    return name in ("ImportError", "ModuleNotFoundError", "Exception", "BaseException")


def closure(module: str, source: Path, files: Sequence[Path] = ()) -> Code:
    """The closure of ``module`` (whose file is ``source``) and the data ``files`` it names.

    Raises ``Unresolved`` naming the module, the line and the import when an import cannot be
    resolved: computed at run time, relative past the top, or found neither in the project, the
    standard library, Moruna nor any installed distribution.
    """
    source = source.resolve()
    root = import_root(module, source)
    found: dict[str, Path] = {}
    imports: set[str] = set()
    refusals: list[str] = []
    pending = [(module, source)]
    seen: set[str] = set()

    def add(path: Path) -> None:
        found[path.relative_to(root).as_posix()] = path

    def packages(dotted: str) -> None:
        parts = dotted.split(".")
        for i in range(1, len(parts)):
            init, _ = _module_file(root, ".".join(parts[:i]))
            if init is not None and init.name == "__init__.py" and init not in found.values():
                pending.append((".".join(parts[:i]), init))

    packages(module)
    while pending:
        name, path = pending.pop()
        if name in seen:
            continue
        seen.add(name)
        add(path)
        if path.suffix != ".py":
            continue
        package = name if path.name == "__init__.py" else name.rpartition(".")[0]
        visitor = _Imports(package)
        try:
            visitor.visit(ast.parse(path.read_text(encoding="utf-8"), filename=str(path)))
        except (SyntaxError, Unresolved) as exc:
            refusals.append(f"{path}: {exc}")
            continue
        for line, why in visitor.dynamic:
            refusals.append(
                f"{name} line {line}: {why}, which a build cannot follow; import it by name, "
                "or name the file with files="
            )
        for statement in visitor.found:
            top = statement.names[0].split(".")[0] if statement.names else ""
            if not top:
                continue
            local, is_package = _module_file(root, top)
            if local is not None or is_package:
                for dotted in statement.names:
                    file, is_package = _module_file(root, dotted)
                    if file is not None:
                        packages(dotted)
                        pending.append((dotted, file))
                    elif not is_package and not statement.optional:
                        refusals.append(
                            f"{name} line {statement.line}: `{dotted}` is not a module under "
                            f"{root}"
                        )
                continue
            if top in _stdlib() or top in PROVIDED:
                continue
            if top in _distributions_of():
                imports.add(top)
            elif not statement.optional:
                refusals.append(
                    f"{name} line {statement.line}: `{top}` is neither in the project ({root}), "
                    "the standard library, nor an installed distribution"
                )
    if refusals:
        raise Unresolved("; ".join(refusals))
    data: set[str] = set()
    for extra in files:
        path = Path(extra).resolve()
        if not path.is_file():
            raise Unresolved(f"{extra}: no such file")
        try:
            relative = path.relative_to(root).as_posix()
        except ValueError:
            raise Unresolved(f"{extra}: not under {root}, where {module} is imported from") from None
        found[relative] = path
        data.add(relative)
    return Code(
        entry="",
        module=module,
        source=source,
        root=root,
        files=dict(sorted(found.items())),
        data=frozenset(data),
        imports=tuple(sorted(imports)),
    )


def distributions(imports: Iterable[str]) -> tuple[str, ...]:
    """The distributions that provide the top-level import names ``imports``, normalised and
    sorted; every distribution that provides one of them (a namespace package may have
    several)."""
    table = _distributions_of()
    return tuple(sorted({d for name in imports for d in table.get(name, ())}))


@dataclasses.dataclass(frozen=True)
class Provenance:
    """Where code came from: the repository, the revision, and whether any of its files differ
    from it."""

    source: str | None
    revision: str | None
    dirty: bool

    def as_dict(self) -> dict[str, Any]:
        return {"source": self.source, "revision": self.revision, "dirty": self.dirty}


def _git(root: Path, *args: str) -> str | None:
    try:
        done = subprocess.run(
            ["git", "-C", str(root), *args], capture_output=True, text=True, check=False,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return done.stdout.strip() if done.returncode == 0 else None


def _public(url: str) -> str:
    """A remote URL with no credential in it."""
    return re.sub(r"^(\w+://)[^/@]*@", r"\1", url)


def provenance(code: Code) -> Provenance:
    """Where ``code`` came from, from git when its root is in a repository: the remote
    ``origin`` (credentials taken out), the revision checked out, and whether any file of the
    closure differs from it. Outside a repository, nothing is known."""
    revision = _git(code.root, "rev-parse", "HEAD")
    if revision is None:
        return Provenance(None, None, False)
    remote = _git(code.root, "remote", "get-url", "origin")
    status = _git(
        code.root, "status", "--porcelain", "--untracked-files=all", "--",
        *[str(p) for p in code.files.values()],
    )  # fmt: skip
    return Provenance(_public(remote) if remote else None, revision, bool(status))


def build_key(
    code: Code, entry: str, *, lock: str | None, runtime: str, shim: str, platform: str
) -> str:
    """``sha256:`` over what an image is made of: each file of the closure (its stripped syntax
    tree, or a data file's bytes) by its path, the lock's digest, the runtime image, the shim,
    the platform and the entry. Docstrings, comments and formatting are not part of it."""
    document = {
        "moruna_build_key": KEY_VERSION,
        "entry": entry,
        "files": code.digests(),
        "lock": lock,
        "runtime": runtime,
        "shim": shim,
        "platform": platform,
    }
    data = json.dumps(document, sort_keys=True, separators=(",", ":")).encode()
    return "sha256:" + hashlib.sha256(data).hexdigest()
