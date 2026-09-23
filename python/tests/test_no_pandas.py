"""PY-T8 no_pandas: `import moruna` does not import pandas (PY-I8)."""

from __future__ import annotations

import subprocess
import sys


def test_py_t8_no_pandas() -> None:
    code = "import sys, moruna; print('pandas' in sys.modules)"
    out = subprocess.run(  # noqa: S603
        [sys.executable, "-c", code], capture_output=True, text=True, check=True
    )
    assert out.stdout.strip() == "False", out.stdout
