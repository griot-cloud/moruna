"""BD-T28 (SDD 30 k): ``Function.build()`` and ``moruna build`` over the engine's own objects.

The builder's tests (``test_build.py``) run without the engine; this is what needs it: a real
``moruna.Function`` is read as a function, and the package's two doors refuse a project with no lock the
same way, before anything is fetched. It needs a built ``moruna`` and no network.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import builder_support as support
import pytest

try:
    import moruna
    from moruna import _build
except ImportError as exc:  # no wheel built here: the builder's own tests still run
    pytest.skip(f"moruna is not built: {exc}", allow_module_level=True)

KERNEL = """
import moruna
import pyarrow as pa


@moruna.function(input_schema={"id": "int64"}, output_schema={"adds": {"c": "int64"}})
def stateless(batch: pa.RecordBatch) -> pa.RecordBatch:
    return batch


class Counter:
    def setup(self, ctx):
        return {"n": 0}

    def __call__(self, state, batch):
        return batch


counter = moruna.function(Counter(), stateful=True)
"""


def test_bd_t28_a_real_function_is_read_as_a_function(tmp_path: Path) -> None:
    (tmp_path / "kernels.py").write_text(KERNEL)
    module = support.load_path(tmp_path / "kernels.py", "kernels")
    stateless = _build.function_of(module.stateless)
    assert (stateless.module, stateless.name, stateless.stateful) == ("kernels", "stateless", False)
    assert stateless.source == tmp_path / "kernels.py"
    assert stateless.fingerprint == module.stateless.fingerprint
    assert len(stateless.fingerprint) == 64  # noqa: PLR2004, the engine's hex digest
    counter = _build.function_of(module.counter)
    assert (counter.name, counter.stateful) == ("counter", True)
    assert counter.fingerprint != stateless.fingerprint


def test_bd_t28_no_lock_is_refused_by_both_doors_before_anything_is_fetched(tmp_path: Path) -> None:
    (tmp_path / "kernels.py").write_text(KERNEL)
    module = support.load_path(tmp_path / "kernels.py", "kernels")
    out = tmp_path / "image"
    with pytest.raises(moruna.ConfigError, match=r"build refused: no lock: .*needs a uv\.lock"):
        module.stateless.build(out=out, cache=tmp_path / "cache")
    done = subprocess.run(
        [
            sys.executable,
            "-m",
            "moruna",
            "build",
            f"{tmp_path / 'kernels.py'}:stateless",
            "--out",
            str(out),
        ],
        capture_output=True,
        text=True,
        check=False,
        cwd=tmp_path,
    )
    assert done.returncode == 2  # noqa: PLR2004
    assert "moruna build: no lock:" in done.stderr
    assert not out.exists() and not (tmp_path / "cache").exists()
