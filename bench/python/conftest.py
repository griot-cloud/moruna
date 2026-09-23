"""Make ``moruna_bench_kernels`` importable when pytest is run from ``bench/python``.

There is no ``python/pyproject.toml`` in the repository yet (it arrives in wave 5
with the Python package), and this branch does not add a repository wide pytest
configuration. One line here is enough, and it keeps the tests runnable with the
single command ``bench/README.md`` documents.
"""

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
