"""The exception hierarchy (e.2, PY-I2).

The classes are defined in the extension module, because that is what raises them; this module
re-exports them so ``moruna._errors`` is the import path e.3 names and ``except moruna.KernelError``
and ``except moruna._errors.KernelError`` are the same class.

Every instance carries ``.kind`` (the ``MorunaError`` variant name), ``.message``, ``.diagnostic``
(a dict), ``.run_id``, ``.manifest`` and ``.report``; the last three are ``None`` when the run had
not produced them.
"""

from moruna._core import (
    MorunaError,
    BudgetError,
    Cancelled,
    ConfigError,
    IoError,
    KernelError,
    PlanError,
    ResumeError,
)

__all__ = [
    "MorunaError",
    "BudgetError",
    "Cancelled",
    "ConfigError",
    "IoError",
    "KernelError",
    "PlanError",
    "ResumeError",
]
