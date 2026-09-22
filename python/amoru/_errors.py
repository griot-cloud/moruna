"""The exception hierarchy (e.2, PY-I2).

The classes are defined in the extension module, because that is what raises them; this module
re-exports them so ``amoru._errors`` is the import path e.3 names and ``except amoru.KernelError``
and ``except amoru._errors.KernelError`` are the same class.

Every instance carries ``.kind`` (the ``AmoruError`` variant name), ``.message``, ``.diagnostic``
(a dict), ``.run_id``, ``.manifest`` and ``.report``; the last three are ``None`` when the run had
not produced them.
"""

from amoru._core import (
    AmoruError,
    BudgetError,
    Cancelled,
    ConfigError,
    IoError,
    KernelError,
    PlanError,
    ResumeError,
)

__all__ = [
    "AmoruError",
    "BudgetError",
    "Cancelled",
    "ConfigError",
    "IoError",
    "KernelError",
    "PlanError",
    "ResumeError",
]
