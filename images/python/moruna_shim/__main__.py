"""`python -m moruna_shim MODULE:FUNCTION`: serves the batch function as the stage's task."""

import importlib
import sys

from . import serve


def main(argv):
    if len(argv) != 2 or ":" not in argv[1]:
        print("usage: python -m moruna_shim MODULE:FUNCTION", file=sys.stderr)
        return 2
    module, _, name = argv[1].partition(":")
    fn = getattr(importlib.import_module(module), name)
    serve(fn)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
