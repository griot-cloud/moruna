"""The run report (PY-I5).

``RunReport`` is the extension module's class: its attributes are exactly the fields of
``moruna_trace::RunReport`` (04 d.1), ``str(report)`` is the fixed layout, ``report.to_json()`` is
the trace crate's own serialisation, and ``report.trace_path`` is the file the trace went to when
``trace=`` asked for one. This module re-exports it under the import path e.3 names and adds
nothing: a report the runtime computed is not a report the surface may edit.
"""

from moruna._core import RunReport

__all__ = ["RunReport"]
