"""``python -m moruna <command>``: the package's command line (12 d.2, MH 4.9).

One command today, ``check``; the job-document commands of MH 4.2 are the ``moruna`` binary's.
"""

from __future__ import annotations

import sys

USAGE = "usage: python -m moruna check <module-or-file> [--kernel NAME] [--json] [--seed N]"


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if not args or args[0] in ("-h", "--help"):
        print(USAGE)
        return 0 if args else 2
    if args[0] == "check":
        from moruna._check import main as check  # noqa: PLC0415, only the command run is imported

        return check(args[1:])
    print(f"moruna: unknown command {args[0]!r}\n{USAGE}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
