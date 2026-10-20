"""The warm template a ``.pex`` stage's child is forked from (SDD 17 f.9).

Started by Moruna as ``python -I -m moruna._sandbox`` with its socket at descriptor 3, on the
standard-build interpreter the host names. It imports the data libraries its interpreter has,
checks that it holds one thread (SB-I10), says hello, and then serves requests: ``start`` forks a
child that confines itself and runs the package (``moruna._core._sandbox_child``); ``status``
reports how a child ended (SB-I11). The frames are 17 e.3's: a little-endian ``u32`` length and
that many bytes of JSON.
"""

from __future__ import annotations

import importlib
import importlib.metadata
import importlib.util
import json
import os
import platform
import socket
import struct
import sys
import time

TEMPLATE_FD = 3
# The data libraries a template imports when its interpreter has them (17 b); pyarrow it must.
# Polars is not one of them: importing it starts threads (its out-of-core cleaner and its
# allocator's background threads), and a template that holds a thread cannot fork (SB-I10), so
# a Polars kernel imports it inside its own child.
PRELOAD = ("pyarrow", "pyarrow.compute", "pyarrow.ipc", "numpy")
# pyarrow's Linux x86_64 wheels link a jemalloc (prefix `je_arrow_`) compiled with
# `background_thread:true`, and jemalloc's library constructor starts that purging thread when
# libarrow loads, before any allocation: `import pyarrow` alone leaves the template with two
# threads. jemalloc reads `JE_ARROW_MALLOC_CONF` after the compiled-in options, so the template
# turns the thread off before it imports anything; jemalloc then purges on its own allocations.
ARROW_JEMALLOC = ("JE_ARROW_MALLOC_CONF", "background_thread:false")
# How long ``status`` waits for a child to be reapable before saying it is still running.
STATUS_WAIT = 2.0
FRAME_MAX_BYTES = 1 << 20


def _send(sock: socket.socket, message: dict) -> None:
    body = json.dumps(message).encode()
    sock.sendall(struct.pack("<I", len(body)) + body)


def _recv_exact(sock: socket.socket, n: int, first: bytes = b"") -> bytes | None:
    data = bytearray(first)
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            return None
        data += chunk
    return bytes(data)


def _receive(sock: socket.socket) -> tuple[dict | None, list[int]]:
    """One frame and the descriptors that came with its first bytes."""
    head, fds, _flags, _addr = socket.recv_fds(sock, 4, 4)
    if not head:
        return None, list(fds)
    head = _recv_exact(sock, 4, head)
    if head is None:
        return None, list(fds)
    (length,) = struct.unpack("<I", head)
    if length > FRAME_MAX_BYTES:
        raise ValueError(f"a frame of {length} bytes is over the {FRAME_MAX_BYTES} allowed")
    body = _recv_exact(sock, length)
    if body is None:
        return None, list(fds)
    return json.loads(body), list(fds)


def _threads() -> int:
    return len(os.listdir("/proc/self/task"))


def _glibc() -> list[int] | None:
    try:
        name, version = os.confstr("CS_GNU_LIBC_VERSION").split()
    except ValueError, OSError, AttributeError:
        return None
    if name != "glibc":
        return None
    major, minor = version.split(".")[:2]
    return [int(major), int(minor)]


def _describe(status: int) -> str:
    if os.WIFEXITED(status):
        return f"exit status {os.WEXITSTATUS(status)}"
    if os.WIFSIGNALED(status):
        return f"killed by signal {os.WTERMSIG(status)}"
    return f"wait status {status}"


def _reap(ended: dict[int, str], pid: int = -1) -> None:
    while True:
        try:
            got, status = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            return
        if got == 0:
            return
        ended[got] = _describe(status)
        if pid != -1:
            return


def _status(ended: dict[int, str], pid: int) -> str:
    deadline = time.monotonic() + STATUS_WAIT
    while True:
        _reap(ended, pid)
        if pid in ended:
            return ended[pid]
        if time.monotonic() >= deadline:
            return "running"
        time.sleep(0.01)


# A start request carries the child's control socket and the memfd (17 e.3).
FDS_PER_START = 2


def _preload() -> dict[str, str]:
    """Import the data libraries (17 f.9) and the extension; their versions by name."""
    libraries: dict[str, str] = {}
    os.environ[ARROW_JEMALLOC[0]] = ARROW_JEMALLOC[1]
    for name in PRELOAD:
        top = name.split(".")[0]
        if importlib.util.find_spec(top) is None:
            if top == "pyarrow":
                raise ImportError("the sandbox's interpreter has no pyarrow")
            continue
        importlib.import_module(name)
        libraries[top] = importlib.metadata.version(top)
    from moruna import _core  # noqa: PLC0415, the extension, for the child's runner

    if not hasattr(_core, "_sandbox_child"):
        raise ImportError("this moruna was built without the sandbox's child runner")
    return libraries


def _start(sock: socket.socket, message: dict, fds: list[int], ended: dict[int, str]) -> None:
    """Fork a child for a stage (17 f.9); the child never returns from here."""
    _reap(ended)
    threads = _threads()
    if threads != 1 or len(fds) != FDS_PER_START:
        for fd in fds:
            os.close(fd)
        reason = (
            f"the template holds {threads} threads; a fork needs one (SB-I10)"
            if threads != 1
            else f"a start request carries {FDS_PER_START} descriptors, not {len(fds)}"
        )
        _send(sock, {"op": "refused", "reason": reason})
        return
    pid = os.fork()
    if pid == 0:
        sock.close()
        code = 3
        try:
            from moruna import _core  # noqa: PLC0415

            code = _core._sandbox_child(json.dumps(message), fds[0], fds[1])
        finally:
            sys.stdout.flush()
            sys.stderr.flush()
            os._exit(code)
    for fd in fds:
        os.close(fd)
    _send(sock, {"op": "started", "pid": pid})


def main() -> int:
    sock = socket.socket(fileno=TEMPLATE_FD)
    try:
        libraries = _preload()
    except Exception as exc:  # noqa: BLE001, the refusal names it
        _send(sock, {"op": "refused", "reason": f"{type(exc).__name__}: {exc}"})
        return 2
    threads = _threads()
    if threads != 1:
        _send(
            sock,
            {
                "op": "refused",
                "reason": f"the template holds {threads} threads after importing its libraries; "
                "a fork needs one (SB-I10)",
            },
        )
        return 2
    _send(
        sock,
        {
            "op": "hello",
            "pid": os.getpid(),
            "python": platform.python_version(),
            "libraries": libraries,
            "glibc": _glibc(),
            "machine": platform.machine(),
        },
    )
    ended: dict[int, str] = {}
    while True:
        message, fds = _receive(sock)
        if message is None:
            return 0
        op = message.get("op")
        if op == "start":
            _start(sock, message, fds, ended)
        elif op == "status":
            pid = int(message["pid"])
            _send(sock, {"op": "exited", "pid": pid, "status": _status(ended, pid)})
        else:
            for fd in fds:
                os.close(fd)
            _send(sock, {"op": "refused", "reason": f"`{op}` is not a request"})


if __name__ == "__main__":
    sys.exit(main())
