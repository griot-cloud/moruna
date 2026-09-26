"""``python -m moruna check``: is this a kernel, and what does it cost (SDD 15).

Loads a module or a file, finds its kernels (decorated functions, standard kernels, and plain
functions whose annotated signature is Polars frame to Polars frame), runs each through the
check harness of ``moruna-runtime`` on synthetic batches from its declared input schema, and
reports: the verdict, the fingerprint, every refusal naming the column and the types, and the
profile row it wrote for the first run to size from. Exit 0 when every kernel checked agrees
with its declaration, 2 otherwise (15 e.5).
"""

from __future__ import annotations

import argparse
import importlib
import importlib.util
import json
import sys
from pathlib import Path
from typing import Any

from moruna import _core
from moruna._declare import polars_signature

EXIT_AGREED = 0
EXIT_REFUSED = 2


def _load(target: str) -> Any:
    """Import ``target``: a path to a ``.py`` file, or a dotted module name (15 f.2)."""
    path = Path(target)
    if target.endswith(".py") or path.is_file():
        name = path.stem
        spec = importlib.util.spec_from_file_location(name, path)
        if spec is None or spec.loader is None:
            raise ImportError(f"cannot load {target}")
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module
        parent = str(path.resolve().parent)
        if parent not in sys.path:
            sys.path.insert(0, parent)
        spec.loader.exec_module(module)
        return module
    return importlib.import_module(target)


def kernels_of(module: Any, only: str | None = None) -> list[tuple[str, Any]]:
    """The module's kernels, in definition order: every ``KernelSpec``, every standard kernel,
    and every function defined in the module whose signature is a Polars frame to a frame,
    which is wrapped as ``@moruna.kernel`` would wrap it (15 f.2)."""
    from moruna import kernel  # noqa: PLC0415, the package imports this module

    found: list[tuple[str, Any]] = []
    for name, value in vars(module).items():
        if name.startswith("_") or (only is not None and name != only):
            continue
        if isinstance(value, _core.KernelSpec | _core.StdKernel):
            found.append((name, value))
        elif (
            callable(value)
            and getattr(value, "__module__", None) == module.__name__
            and polars_signature(value) is not None
        ):
            found.append((name, kernel(value)))
    return found


def check_one(name: str, value: Any, seed: int, profiles_dir: str | None) -> dict[str, Any]:
    """One kernel through the harness; the JSON report of 15 e.3 as a dict."""
    return json.loads(_core.check_kernel(value, name=name, seed=seed, profiles_dir=profiles_dir))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python -m moruna check",
        description="Check that a module's kernels agree with their declared schemas, and "
        "write each one's first profile.",
    )
    parser.add_argument("target", help="a module name or a path to a .py file")
    parser.add_argument("--kernel", help="check only the kernel with this name")
    parser.add_argument("--json", action="store_true", help="print the JSON report")
    parser.add_argument("--seed", type=int, default=0, help="seed of the synthetic batches")
    parser.add_argument(
        "--profiles-dir",
        help="where the profile rows go (default: ~/.moruna/profiles, as a run reads them)",
    )
    parser.add_argument("--no-profile", action="store_true", help="write no profile row")
    args = parser.parse_args(argv)
    if args.seed < 0 or args.seed >= 1 << 64:
        parser.error("--seed takes an integer from 0 to 2**64 - 1")

    profiles_dir = None if args.no_profile else (args.profiles_dir or _core.default_profiles_dir())
    reports: list[dict[str, Any]] = []
    error: str | None = None
    try:
        module = _load(args.target)
        found = kernels_of(module, args.kernel)
        if not found:
            error = (
                f"no kernel named {args.kernel!r} in {args.target}"
                if args.kernel
                else f"no kernels in {args.target}: decorate a function with @moruna.kernel"
            )
        for name, value in found:
            reports.append(check_one(name, value, args.seed, profiles_dir))
    except Exception as exc:  # noqa: BLE001, a module that does not load is a refusal
        error = f"{type(exc).__name__}: {exc}"

    code = EXIT_REFUSED if error or any(r["exit"] != EXIT_AGREED for r in reports) else EXIT_AGREED
    if args.json:
        print(
            json.dumps(
                {
                    "moruna_check": 1,
                    "target": args.target,
                    "exit": code,
                    "error": error,
                    "kernels": reports,
                },
                indent=2,
            )
        )
    else:
        for report in reports:
            print(report["summary"], end="")
        if error:
            print(f"moruna check: {error}", file=sys.stderr)
    return code
