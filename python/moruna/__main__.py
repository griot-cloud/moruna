"""``python -m moruna`` and the ``moruna`` console script (MH 4.2, MH 4.9).

``run`` and ``serve`` are the Rust command's; this module hands it the arguments and returns its
exit code. ``check`` loads Python modules, so it is answered here, by ``moruna._check``, which
calls the Rust harness for each kernel it finds, or runs a package through its runtime (SDD 16
f.9). ``package`` drives pip, so it is answered here too, by ``moruna._package`` (16 f.4).
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
    if args and args[0] == "package":
        from moruna._package import main as package  # noqa: PLC0415

        return package(args[1:])
    return _core.main(args)


if __name__ == "__main__":
    sys.exit(main())
