"""``python -m moruna`` and the ``moruna`` console script (MH 4.2, MH 4.9).

``run`` and ``serve`` are the Rust command's; this module hands it the arguments and returns its
exit code. Before ``run`` starts, the needs of the job's image tasks are resolved through the
resolver ``--resolver MODULE:ATTRIBUTE`` names (each a port the run serves) and the registry
images the job names are pulled, so a need nobody supplies and an image that is not there are
refused by name before anything runs. ``check`` loads Python modules, so it is answered here, by ``moruna._check``, which
calls the Rust harness for each function it finds, or runs a package through its runtime (SDD 16
f.9). ``package`` drives pip, so it is answered here too, by ``moruna._package`` (16 f.4), and so
is ``build``, which drives uv to make an OCI image of a function (``moruna._build``, SDD 30).
"""

from __future__ import annotations

import sys

from moruna import _core


def main(argv: list[str] | None = None) -> int:
    """Run ``moruna <command> ...`` and return its exit code."""
    args = sys.argv[1:] if argv is None else argv
    if args and args[0] == "check":
        from moruna._check import main as check  # noqa: PLC0415, only the command run is imported

        return check(args[1:])
    if args and args[0] == "aot":
        return _aot(args[1:])
    if args and args[0] == "build":
        from moruna._build import main as build  # noqa: PLC0415

        return build(args[1:])
    if args and args[0] == "run":
        return _run(args)
    if args and args[0] == "package":
        from moruna._package import main as package  # noqa: PLC0415

        return package(args[1:])
    return _core.main(args)


#: The options of `moruna run` that take a value.
_VALUED = {
    "--plan", "--image", "--launch-witness", "--arg", "--sandbox-python", "--sandbox-cgroup",
    "--kinds", "--cost-store", "--cost-model", "--resolver",
}


def _run(args: list[str]) -> int:
    """``moruna run JOB ...``: the job's image tasks' needs resolved and its registry images
    pulled, then the command."""
    rest: list[str] = []
    resolver_ref = None
    spec = None
    it = iter(args)
    for arg in it:
        if arg == "--resolver":
            value = next(it, None)
            if value is None or value.startswith("--") or resolver_ref is not None:
                print("moruna: --resolver is given once, with MODULE:ATTRIBUTE", file=sys.stderr)
                return 2
            resolver_ref = value
            continue
        rest.append(arg)
        if arg in _VALUED:
            following = next(it, None)
            if following is not None:
                rest.append(following)
        elif not arg.startswith("--") and arg != "run" and spec is None:
            spec = arg
    needs: dict[str, object] = {}
    if spec is not None and "--vm" not in rest:
        from moruna import _job, _ports  # noqa: PLC0415

        try:
            job = _job.Job.read(spec)
        except Exception:  # noqa: BLE001, the command reads it again and says what is wrong
            job = None
        if job is not None:
            drafts = [row for row in job.status() if row["digest"] is None]
            if drafts:
                print(
                    "moruna run: the job's spec is a draft: images not built ("
                    + "; ".join(f"`{r['task']}` (build key {r['build_key']})" for r in drafts)
                    + "); build the job (moruna build), and run the spec the build wrote",
                    file=sys.stderr,
                )
                return 2
            try:
                resolver = _ports.load_resolver(resolver_ref) if resolver_ref else None
                missing: list[str] = []
                for key, _role, task, _inputs in job.tasks:
                    for need in getattr(task, "needs", ()):
                        if need in needs:
                            continue
                        if resolver is None:
                            missing.append(f"job[{key!r}] needs {need!r}, and no --resolver was given")
                            continue
                        try:
                            needs[need] = _job._Served(need, resolver.resolve(need))
                        except LookupError:
                            missing.append(
                                f"job[{key!r}] needs {need!r}, which the resolver cannot supply"
                            )
                if missing:
                    print("moruna run: " + "; ".join(missing), file=sys.stderr)
                    return 2
                _job._pulled(job, resolver)
            except (_core.ConfigError, ImportError, AttributeError, TypeError, ValueError) as exc:
                print(f"moruna run: {exc}", file=sys.stderr)
                return 2
    return _core.main(rest, needs=needs)


def _aot(args: list[str]) -> int:
    """``moruna aot <package>`` (SDD 16 f.10): a ``.wasm``'s ``.cwasm`` or a ``.pex``'s tree,
    written beside the package."""
    if len(args) != 1 or args[0].startswith("-"):
        print("usage: moruna aot <package.wasm | package.pex>", file=sys.stderr)
        return 2
    try:
        print(_core.aot(args[0]))
    except _core.MorunaError as exc:
        print(f"moruna aot: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
