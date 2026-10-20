"""SB-I10 for the template as Moruna starts it (SDD 17 f.9): ``python -I -m moruna._sandbox`` with
an environment of only the BLAS thread counts and its socket at descriptor 3, which says
``hello`` only when importing its libraries left it one thread.

pyarrow's Linux x86_64 wheels link a jemalloc whose library constructor starts a background
purging thread when libarrow loads; a template that let it start refused every ``.pex`` (2
threads after importing its libraries).
"""

from __future__ import annotations

import os
import socket
import subprocess
import sys
import textwrap

import pytest

from moruna import _sandbox

# `crates/moruna-sandbox/src/template.rs`: `env_clear()` and these.
TEMPLATE_ENV = {"OPENBLAS_NUM_THREADS": "1", "OMP_NUM_THREADS": "1", "MKL_NUM_THREADS": "1"}


def start(argv: list[str]) -> tuple[dict | None, str]:
    """Start ``argv`` as the template, its socket at descriptor 3; its first frame and stderr."""
    ours, theirs = socket.socketpair()
    fd = theirs.fileno()

    def at_three() -> None:
        os.dup2(fd, _sandbox.TEMPLATE_FD)

    process = subprocess.Popen(  # noqa: S603, the interpreter running the tests
        argv,
        env=TEMPLATE_ENV,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        preexec_fn=at_three,  # noqa: PLW1509, nothing else runs threads here
        close_fds=False,
    )
    theirs.close()
    ours.settimeout(120)
    try:
        frame, fds = _sandbox._receive(ours)
        for extra in fds:
            os.close(extra)
    finally:
        ours.close()
    _, stderr = process.communicate(timeout=60)
    return frame, stderr.decode(errors="replace")


@pytest.mark.skipif(
    sys.platform != "linux", reason="the template counts its threads in /proc/self/task: Linux"
)
def test_template_holds_one_thread_after_importing_pyarrow() -> None:
    frame, stderr = start([sys.executable, "-I", "-m", "moruna._sandbox"])
    assert frame is not None, stderr
    assert frame["op"] == "hello", frame
    assert "pyarrow" in frame["libraries"]


def test_template_turns_off_arrow_jemalloc_thread_before_importing_pyarrow() -> None:
    # What the environment holds when `pyarrow.lib`, the extension that loads libarrow (and
    # with it jemalloc's constructor), is imported, wherever libarrow carries jemalloc or not.
    wrapper = textwrap.dedent(
        """
        import importlib.abc, os, runpy, sys

        class Witness(importlib.abc.MetaPathFinder):
            def find_spec(self, name, path=None, target=None):
                if name == "pyarrow.lib":
                    conf = os.environ.get("JE_ARROW_MALLOC_CONF")
                    sys.stderr.write(f"JE_ARROW_MALLOC_CONF={conf!r}\\n")
                    sys.meta_path.remove(self)
                return None

        sys.meta_path.insert(0, Witness())
        runpy.run_module("moruna._sandbox", run_name="__main__")
        """
    )
    _frame, stderr = start([sys.executable, "-I", "-c", wrapper])
    assert "JE_ARROW_MALLOC_CONF='background_thread:false'" in stderr, stderr
