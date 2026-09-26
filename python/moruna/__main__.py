"""``python -m moruna`` and the ``moruna`` console script: ``run`` and ``serve`` (MH 4.2, MH).

The command is the Rust one; this module hands it the arguments and returns its exit code.
"""

from __future__ import annotations

import sys

from moruna import _core


def main() -> int:
    """Run ``moruna <command> ...`` and return its exit code (MH 4.2)."""
    return _core.main(sys.argv[1:])


if __name__ == "__main__":
    sys.exit(main())
