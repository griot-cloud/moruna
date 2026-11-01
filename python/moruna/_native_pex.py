"""An actual native PEX child. The engine starts this ONLY through its real
Account/record-plane task path; package bytes and commands arrive on native slots.
No socket template, fork, cgroup, guessed package path or host fallback is used.
"""
import os
from moruna import _core


def main():
    # Data travels exclusively through the native checked plane, not stdout IPC.
    # Raw writes by imported extensions remain real stderr in the task's log ring.
    os.dup2(2, 1)
    child = getattr(_core, "_native_pex_child", None)
    if child is None:
        raise RuntimeError("this SDK has no real native PEX child transport")
    child()


if __name__ == "__main__":
    main()
