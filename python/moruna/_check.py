"""``python -m moruna check``: is this a kernel, and what does it cost (MH 4.9).

A ``.wasm`` or ``.pex`` package is checked through its own runtime (SDD 16 f.9), never in
process, so the verdict is the one a hosted run would give; its fingerprint is the file's.

Loads a module or a file, finds its kernels (decorated functions, standard kernels, and plain
functions whose annotated signature is Polars frame to Polars frame), runs each through the
check harness of ``moruna-runtime`` on synthetic batches from its declared input schema, and
reports: the verdict, the fingerprint, every refusal naming the column and the types. A kernel
that agrees is then warmed up (AR section 6): run at sizes, workers and distinct counts on
generated data, every unit written to the observation store and the cost model fitted, so the
report carries what the function costs per 10,000 rows and what warm-up itself consumed. Exit 0
when every kernel checked agrees with its declaration and was warmed up, 2 otherwise (MH 4.9).

``moruna.check(target, ...)`` is the same as a function: it returns the report this command
prints with ``--json``.
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
CHECK_FORMAT = 2
BUDGET_SECONDS = 60.0


def _load(target: str) -> Any:
    """Import ``target``: a path to a ``.py`` file, or a dotted module name (MH 4.9)."""
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
    which is wrapped as ``@moruna.kernel`` would wrap it (MH 4.9)."""
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


def _warm_up(
    warm_up: bool,
    store: str | None,
    budget: float,
    statistics: str | Path | dict[str, Any] | None,
    model: str | None,
    cost: _core.CostSession | None = None,
) -> dict[str, Any]:
    """The warm-up keywords of the harness: the statistics as JSON text, from a mapping or a
    file."""
    if isinstance(statistics, dict):
        text: str | None = json.dumps(statistics)
    elif statistics is not None:
        text = Path(statistics).read_text()
    else:
        text = None
    return {
        "warm_up": warm_up,
        "store": store,
        "budget": budget,
        "statistics": text,
        "model": model,
        "cost": cost,
    }


def check_one(name: str, value: Any, seed: int, warm: dict[str, Any]) -> dict[str, Any]:
    """One kernel through the harness; the JSON report of MH 4.9 as a dict."""
    return json.loads(_core.check_kernel(value, name=name, seed=seed, **warm))


def check_package(
    target: str,
    seed: int,
    warm: dict[str, Any],
    *,
    sandbox_python: str | None = None,
    sandbox_cgroup: str | None = None,
    args: str | None = None,
    assets: str | None = None,
) -> dict[str, Any]:
    """A package through its own runtime (SDD 16 f.9); the JSON report of MH 4.9 as a dict."""
    return json.loads(
        _core.check_package(
            target,
            seed=seed,
            sandbox_python=sandbox_python,
            sandbox_cgroup=sandbox_cgroup,
            args=args,
            assets=assets,
            **warm,
        )
    )


def check(
    target: str,
    *,
    kernel: str | None = None,
    seed: int = 0,
    warm_up: bool = True,
    store: str | None = None,
    budget: float = BUDGET_SECONDS,
    statistics: str | Path | dict[str, Any] | None = None,
    model: str | None = None,
    cost: _core.CostSession | None = None,
    sandbox_python: str | None = None,
    sandbox_cgroup: str | None = None,
    args: str | None = None,
    assets: str | None = None,
) -> dict[str, Any]:
    """Check ``target`` (a module name, a ``.py`` file or a package) and warm up each kernel
    that agrees; the report ``python -m moruna check --json`` prints. Each kernel's
    ``warm_up.cost_per_10000_rows`` is its fitted cost, every estimate naming its basis;
    ``warm_up.consumed`` is what warm-up took, for an embedder to charge; ``warm_up.error`` says
    why a warm-up could not run.

    ``store`` is the observation store's directory (``~/.moruna/observations`` when not given),
    ``budget`` warm-up's seconds, ``statistics`` the input table's statistics (a mapping, or a
    path to a JSON file, in ``moruna_cost::Data``'s shape) and ``model`` the cost model by name.
    """
    if seed < 0 or seed >= 1 << 64:
        raise ValueError("seed takes an integer from 0 to 2**64 - 1")
    reports: list[dict[str, Any]] = []
    error: str | None = None
    try:
        warm = _warm_up(warm_up, store, budget, statistics, model, cost)
        if target.endswith((".wasm", ".pex")):
            reports.append(
                check_package(
                    target,
                    seed,
                    warm,
                    sandbox_python=sandbox_python,
                    sandbox_cgroup=sandbox_cgroup,
                    args=args,
                    assets=assets,
                )
            )
        else:
            found = kernels_of(_load(target), kernel)
            if not found:
                error = (
                    f"no kernel named {kernel!r} in {target}"
                    if kernel
                    else f"no kernels in {target}: decorate a function with @moruna.kernel"
                )
            for name, value in found:
                reports.append(check_one(name, value, seed, warm))
    except Exception as exc:  # noqa: BLE001, a target that does not load is a refusal
        error = f"{type(exc).__name__}: {exc}"
    code = EXIT_REFUSED if error or any(r["exit"] != EXIT_AGREED for r in reports) else EXIT_AGREED
    return {
        "moruna_check": CHECK_FORMAT,
        "target": target,
        "exit": code,
        "error": error,
        "kernels": reports,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python -m moruna check",
        description="Check that a module's kernels agree with their declared schemas, and warm "
        "each one up: what it costs, measured and fitted.",
    )
    parser.add_argument("target", help="a module name, a path to a .py file, or a package")
    parser.add_argument("--kernel", help="check only the kernel with this name")
    parser.add_argument("--json", action="store_true", help="print the JSON report")
    parser.add_argument("--seed", type=int, default=0, help="seed of the generated batches")
    parser.add_argument(
        "--store",
        help="the observation store's directory (default: ~/.moruna/observations)",
    )
    parser.add_argument("--no-warm-up", action="store_true", help="check without costing")
    parser.add_argument(
        "--budget",
        type=float,
        default=BUDGET_SECONDS,
        help="seconds warm-up may take (default: 60)",
    )
    parser.add_argument("--statistics", help="the input table's statistics, a JSON file")
    parser.add_argument("--cost-model", help="the cost model to fit, by name (default: loglinear)")
    parser.add_argument("--sandbox-python", help="a .pex's standard-build interpreter (SDD 17 e.6)")
    parser.add_argument("--sandbox-cgroup", help="the cgroup a .pex's child runs under")
    parser.add_argument("--args", help="a package's arguments, a JSON object (SDD 16 b)")
    parser.add_argument("--assets", help="a directory a package reads, read only (SDD 16 b)")
    args = parser.parse_args(argv)
    if args.seed < 0 or args.seed >= 1 << 64:
        parser.error("--seed takes an integer from 0 to 2**64 - 1")

    result = check(
        args.target,
        kernel=args.kernel,
        seed=args.seed,
        warm_up=not args.no_warm_up,
        store=args.store,
        budget=args.budget,
        statistics=args.statistics,
        model=args.cost_model,
        sandbox_python=args.sandbox_python,
        sandbox_cgroup=args.sandbox_cgroup,
        args=args.args,
        assets=args.assets,
    )
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        for report in result["kernels"]:
            print(report["summary"], end="")
        if result["error"]:
            print(f"moruna check: {result['error']}", file=sys.stderr)
    return int(result["exit"])
