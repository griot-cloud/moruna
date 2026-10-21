"""A Python kernel's output in a run, by stage (MH 4.3 ``log``).

``install`` replaces ``sys.stdout`` and ``sys.stderr`` with streams that hand what a worker
thread writes to the stage the worker works for, and gives ``logging``'s root logger a handler
that does the same with each record, its level and its whole text (an exception's traceback
included). What any other thread writes goes where it went before.
"""

from __future__ import annotations

import logging
import sys

from moruna import _core


class _Records(logging.Handler):
    """A record of a stage's, kept with its level; anyone else's, on the original stream."""

    def emit(self, record: logging.LogRecord) -> None:
        try:
            text = self.format(record)
            if not _core._record(record.levelno, text):
                sys.__stderr__.write(text + "\n")
        except Exception:  # noqa: BLE001, logging's own way to report a broken handler
            self.handleError(record)


def install() -> None:
    """Route this interpreter's kernel output by stage, once."""
    if isinstance(sys.stdout, _core._OutputStream):
        return
    sys.stdout = _core._OutputStream(1, sys.stdout)
    sys.stderr = _core._OutputStream(2, sys.stderr)
    logging.getLogger().addHandler(_Records())
