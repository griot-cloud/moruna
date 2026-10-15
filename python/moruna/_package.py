"""``moruna package``: a Python project or a wasm component as one file (SDD 16 f.4, f.5).

A project is ``function.py``, ``function.yaml`` (16 e.3), any sibling files and, optionally,
``function.lock``, a lockfile in pip's requirements format with a hash on every pin.
``moruna package <project>`` writes ``<name>.pex`` into the working directory (or ``--out``):
the project's files, the wheels of the lockfile installed once per target, bytecode beside every
source file, and ``moruna.json``; it prints ``<path> sha256:<hex>``, or with ``--json`` the
report of 16 d.2. Wheels are resolved by ``pip download``
for each named target and never for the machine packaging them (PK-I5), and the archive is
written the same way every time (PK-I6). ``moruna package --wasm <module.wasm> <project>``
verifies a component the author built against the ``moruna:kernel`` world and embeds the
manifest (16 f.5).
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import io
import json
import py_compile
import re
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path
from typing import Any

from moruna import _core

EXIT_DONE = 0
EXIT_REFUSED = 2

# The guest image's interpreter and the wheel platforms the release builds for (16 i; MH 4.8).
DEFAULT_TARGETS = ("cp314-manylinux_2_28_x86_64", "cp314-manylinux_2_28_aarch64")

YAML_KEYS = ("name", "entry", "input_schema", "output_schema", "hints")

# The lockfile's one name (16 e.3).
LOCKFILE = "function.lock"

# 16 e.4: every entry of a .pex has this time, a file this mode, and deflate at this level.
ZIP_TIME = (1980, 1, 1, 0, 0, 0)
ZIP_FILE_MODE = 0o644
ZIP_LEVEL = 6

MAIN = '''"""A Moruna function package (SDD 16 e.4): `python <name>.pex` prints its manifest."""
import pkgutil
import sys

sys.stdout.write(pkgutil.get_data(__name__, "moruna.json").decode())
'''

_TARGET = re.compile(r"^cp3(\d+)-manylinux_(\d+)_(\d+)_(x86_64|aarch64)$")
_PIN = re.compile(r"^([A-Za-z0-9][A-Za-z0-9._-]*)==([A-Za-z0-9.!+_-]+)$")
_HASH = re.compile(r"^--hash=sha256:[0-9a-fA-F]{64}$")
_COMMENT = re.compile(r"(^|\s)#.*$")


class Refused(Exception):
    """A package refused, with the line that names why (16 d.2)."""


def _normal(name: str) -> str:
    """A distribution name as PEP 503 normalises it."""
    return re.sub(r"[-_.]+", "-", name).lower()


def read_function_yaml(project: Path) -> dict[str, Any]:
    """``function.yaml`` (16 e.3), with only its own keys."""
    path = project / "function.yaml"
    if not path.is_file():
        raise Refused(f"{path}: a project has a function.yaml")
    if importlib.util.find_spec("yaml") is None:
        raise Refused(
            "moruna package reads function.yaml with PyYAML: pip install 'moruna[package]'"
        )
    import yaml  # noqa: PLC0415, only `moruna package` needs it

    try:
        spec = yaml.safe_load(path.read_text(encoding="utf-8"))
    except yaml.YAMLError as exc:
        raise Refused(f"{path}: not YAML: {exc}") from exc
    if not isinstance(spec, dict):
        raise Refused(f"{path}: must be a mapping")
    for key in spec:
        if key not in YAML_KEYS:
            raise Refused(
                f"{path}: {key}: not a function.yaml key; the keys are {', '.join(YAML_KEYS)}"
            )
    if "name" not in spec:
        raise Refused(f"{path}: name: required")
    return spec


def manifest_text(spec: dict[str, Any], fmt: str, entry: str, platforms: list[str]) -> str:
    """The manifest of 16 e.1, validated and written by the same Rust code that reads it."""
    declaration: dict[str, Any] = {"hints": spec.get("hints") or {}}
    for key in ("input_schema", "output_schema"):
        if spec.get(key) is not None:
            declaration[key] = spec[key]
    manifest = {
        "name": spec["name"],
        "entry": entry,
        "declaration": declaration,
        "abi": _core.KERNEL_ABI,
        "moruna": _core.__version__,
        "format": fmt,
        "platforms": platforms,
    }
    try:
        return _core.manifest_json(json.dumps(manifest))
    except ValueError as exc:
        raise Refused(str(exc)) from exc


def targets_of(given: list[str] | None) -> list[str]:
    """The sorted targets, each one a .pex can run on with bytecode this interpreter writes."""
    targets = sorted(set(given or DEFAULT_TARGETS))
    here = sys.version_info.minor
    for target in targets:
        match = _TARGET.match(target)
        if match is None:
            raise Refused(
                f"--platform {target}: a .pex target is cp3NN-manylinux_<major>_<minor>_<x86_64|"
                "aarch64> (the sandbox is Linux only; a packaged kernel runs on the standard "
                "build, so cp314t is not a target)"
            )
        if int(match.group(1)) != here:
            raise Refused(
                f"--platform {target}: its bytecode needs Python 3.{match.group(1)} and this is "
                f"3.{here}; package with that interpreter"
            )
    return targets


def parse_lockfile(path: Path) -> list[tuple[str, str]]:
    """The pins of a lockfile in pip's requirements format (16 e.3): ``name==version`` and at
    least one ``--hash=sha256:<hex>`` each; anything else is refused naming the line."""
    if not path.is_file():
        raise Refused(f"lockfile: {path} does not exist")
    logical: list[tuple[int, str]] = []
    pending = ""
    start = 0
    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = _COMMENT.sub("", raw).rstrip()
        if not pending:
            start = number
        if line.endswith("\\"):
            pending += line[:-1] + " "
            continue
        whole = (pending + line).strip()
        pending = ""
        if whole:
            logical.append((start, whole))
    if pending.strip():
        logical.append((start, pending.strip()))
    pins: list[tuple[str, str]] = []
    for number, text in logical:
        words = text.split()
        match = _PIN.match(words[0])
        if match is None:
            raise Refused(
                f"{path}:{number}: `{words[0]}` is not name==version; a lockfile pins every "
                "requirement exactly"
            )
        hashes = words[1:]
        if not hashes:
            raise Refused(f"{path}:{number}: {words[0]} has no --hash=sha256:<hex>")
        for word in hashes:
            if _HASH.match(word) is None:
                raise Refused(f"{path}:{number}: `{word}` is not --hash=sha256:<64 hex>")
        pins.append((match.group(1), match.group(2)))
    return pins


def _pip_options(args: argparse.Namespace) -> list[str]:
    options: list[str] = []
    if args.index_url:
        options += ["--index-url", args.index_url]
    for link in args.find_links or []:
        options += ["--find-links", link]
    if args.no_index:
        options.append("--no-index")
    return options


def resolve(lockfile: Path, target: str, dest: Path, options: list[str]) -> list[Path]:
    """``pip download`` of every pin for one target and only that target (PK-I5)."""
    match = _TARGET.match(target)
    if match is None:
        raise Refused(f"{target}: not a .pex target")
    minor = match.group(1)
    platform = target.split("-", 1)[1]
    command = [
        sys.executable,
        "-m",
        "pip",
        "download",
        "--quiet",
        "--disable-pip-version-check",
        "--dest",
        str(dest),
        "--no-deps",
        "--only-binary=:all:",
        "--require-hashes",
        "--implementation",
        "cp",
        "--python-version",
        f"3.{minor}",
        "--abi",
        f"cp3{minor}",
        "--platform",
        platform,
        "-r",
        str(lockfile),
        *options,
    ]
    if importlib.util.find_spec("pip") is None:
        raise Refused(
            f"moruna package resolves wheels with pip, which {sys.executable} does not have"
        )
    done = subprocess.run(command, capture_output=True, text=True, check=False)
    if done.returncode != 0:
        missing = re.search(r"No matching distribution found for (\S+)", done.stderr)
        if missing:
            raise Refused(f"no wheel of {missing.group(1)} for {target}")
        last = [line for line in done.stderr.splitlines() if line.strip()]
        said = last[-1] if last else f"exit {done.returncode}"
        raise Refused(f"pip could not resolve the lockfile for {target}: {said}")
    return sorted(dest.glob("*.whl"))


def install_wheel(wheel: Path, dest: Path, log: list[str]) -> None:
    """Unpack ``wheel`` into ``dest`` as an installer would for an importable tree (16 e.4):
    ``<dist>.data/purelib`` and ``platlib`` at the root, ``scripts``, ``headers`` and ``data``
    left out, each one named."""
    with zipfile.ZipFile(wheel) as archive:
        for info in archive.infolist():
            if info.is_dir():
                continue
            parts = Path(info.filename).parts
            if ".." in parts or info.filename.startswith("/"):
                raise Refused(f"{wheel.name}: {info.filename} would be written outside the tree")
            if parts and parts[0].endswith(".data"):
                if parts[1:2] in (("purelib",), ("platlib",)) and parts[2:]:
                    relative = Path(*parts[2:])
                else:
                    log.append(
                        f"{wheel.name}: {info.filename} left out, not part of an importable tree"
                    )
                    continue
            else:
                relative = Path(*parts)
            target = dest / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(archive.read(info))


def compile_tree(root: Path) -> None:
    """Bytecode beside every source file, ``unchecked-hash`` so extraction's times never matter."""
    for source in sorted(root.rglob("*.py")):
        try:
            py_compile.compile(
                str(source),
                cfile=importlib.util.cache_from_source(str(source)),
                dfile=str(source.relative_to(root)),
                doraise=True,
                invalidation_mode=py_compile.PycInvalidationMode.UNCHECKED_HASH,
            )
        except py_compile.PyCompileError as exc:
            raise Refused(f"{source}: does not compile: {exc.msg}") from exc


def write_zip(files: dict[str, bytes], out: Path) -> None:
    """The archive of 16 e.4, entry for entry the same every time (PK-I6)."""
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        for name in sorted(files):
            info = zipfile.ZipInfo(name, date_time=ZIP_TIME)
            info.external_attr = ZIP_FILE_MODE << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3
            archive.writestr(info, files[name], compresslevel=ZIP_LEVEL)
    out.write_bytes(buffer.getvalue())


def _tree_files(root: Path, prefix: str) -> dict[str, bytes]:
    return {
        f"{prefix}{path.relative_to(root).as_posix()}": path.read_bytes()
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def _project_files(project: Path) -> list[Path]:
    """The project's files that go into ``src/``: everything but ``function.yaml``,
    ``function.lock``, dot-files, bytecode caches and packages written there before."""
    left_out = {"function.yaml", LOCKFILE}
    chosen = []
    for path in sorted(project.rglob("*")):
        relative = path.relative_to(project)
        if any(part.startswith(".") or part == "__pycache__" for part in relative.parts):
            continue
        if not path.is_file() or relative.as_posix() in left_out:
            continue
        if path.suffix in (".pex", ".wasm", ".cwasm") or relative.parts[0].endswith(".tree"):
            continue
        chosen.append(path)
    return chosen


def package_project(project: Path, args: argparse.Namespace, log: list[str]) -> Path:
    """16 f.4: ``<out>/<name>.pex``."""
    spec = read_function_yaml(project)
    if not (project / "function.py").is_file():
        raise Refused(f"{project / 'function.py'}: a project has a function.py")
    entry = spec.get("entry")
    if not isinstance(entry, str):
        raise Refused(
            f"{project / 'function.yaml'}: entry: required for a .pex, <module>:<attribute>"
        )
    targets = targets_of(args.platform)
    text = manifest_text(spec, "pex", entry, targets)
    lockfile = project / LOCKFILE
    pins = parse_lockfile(lockfile) if lockfile.is_file() else []
    files: dict[str, bytes] = {"__main__.py": MAIN.encode(), "moruna.json": text.encode()}
    with tempfile.TemporaryDirectory(prefix="moruna-package-") as work:
        work_dir = Path(work)
        src = work_dir / "src"
        for path in _project_files(project):
            target = src / path.relative_to(project)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(path.read_bytes())
        compile_tree(src)
        files.update(_tree_files(src, "src/"))
        options = _pip_options(args)
        for target in targets:
            deps = work_dir / "deps" / target
            deps.mkdir(parents=True)
            if pins:
                wheels = resolve(lockfile, target, work_dir / "wheels" / target, options)
                names = {_normal(w.name.split("-", 1)[0]) for w in wheels}
                for name, version in pins:
                    if _normal(name) not in names:
                        raise Refused(f"no wheel of {name}=={version} for {target}")
                for wheel in wheels:
                    install_wheel(wheel, deps, log)
                    log.append(f"{target}: {wheel.name}")
            compile_tree(deps)
            files.update(_tree_files(deps, f"deps/{target}/"))
            log.append(f"{target}: resolved {len(pins)} pins")
    out_dir = Path(args.out) if args.out else Path.cwd()
    out_dir.mkdir(parents=True, exist_ok=True)
    out = (out_dir / f"{spec['name']}.pex").absolute()
    write_zip(files, out)
    return out


def package_wasm(module: Path, project: Path, args: argparse.Namespace) -> Path:
    """16 f.5: ``<out>/<name>.wasm``, the component verified and its manifest embedded."""
    spec = read_function_yaml(project)
    if (project / LOCKFILE).is_file():
        raise Refused(
            f"{project / LOCKFILE}: a component links its dependencies; a lockfile is for a .pex"
        )
    if "entry" in spec:
        raise Refused(f"{project / 'function.yaml'}: entry: a .wasm's entry is moruna:kernel")
    text = manifest_text(spec, "wasm", "moruna:kernel", ["wasm32"])
    if not module.is_file():
        raise Refused(f"{module}: no such component")
    out_dir = Path(args.out) if args.out else Path.cwd()
    out_dir.mkdir(parents=True, exist_ok=True)
    out = (out_dir / f"{spec['name']}.wasm").absolute()
    try:
        _core.package_wasm(str(module), text, str(out))
    except (ValueError, OSError) as exc:
        raise Refused(str(exc)) from exc
    return out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="moruna package",
        description="Package a Python project as a .pex, or a wasm component as a .wasm.",
    )
    parser.add_argument("project", help="the directory with function.yaml")
    parser.add_argument("--wasm", help="a component built with the author's own toolchain")
    parser.add_argument(
        "--platform",
        action="append",
        help="a target, cp314-manylinux_2_28_x86_64; repeatable (default: the guest image's)",
    )
    parser.add_argument(
        "--out", help="where the package is written (default: the working directory)"
    )
    parser.add_argument("--json", action="store_true", help="print the report as JSON")
    parser.add_argument("--index-url", help="passed to pip download")
    parser.add_argument("--find-links", action="append", help="passed to pip download")
    parser.add_argument("--no-index", action="store_true", help="passed to pip download")
    args = parser.parse_args(argv)
    project = Path(args.project)
    log: list[str] = []
    try:
        if args.wasm:
            out = package_wasm(Path(args.wasm), project, args)
        else:
            out = package_project(project, args, log)
    except Refused as exc:
        for line in log:
            print(line, file=sys.stderr)
        print(f"moruna package: {exc}", file=sys.stderr)
        return EXIT_REFUSED
    for line in log:
        print(line, file=sys.stderr)
    data = out.read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    if args.json:
        manifest = json.loads(_core.package_manifest(str(out)))
        print(
            json.dumps(
                {
                    "moruna_package": 1,
                    "path": str(out),
                    "name": manifest["name"],
                    "format": manifest["format"],
                    "fingerprint": f"sha256:{digest}",
                    "manifest": manifest,
                }
            )
        )
    else:
        print(f"{out} sha256:{digest}")
    return EXIT_DONE


if __name__ == "__main__":
    sys.exit(main())
