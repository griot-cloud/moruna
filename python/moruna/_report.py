"""The run report (PY-I5): what ``moruna.run`` returns.

A report is one JSON document. Its canonical form, ``report.bytes()``, is that document as UTF-8
JSON with its keys sorted at every level and no whitespace between tokens (``json.dumps`` with
``sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False``), so the same
report always gives the same bytes; ``report.digest()`` is ``"sha256:"`` and the hex of SHA-256
over exactly those bytes, and ``moruna.RunReport.from_bytes(data)`` reads them back, refusing bytes
that are not a report in that form. A host that forwards the bytes unchanged can certify them by
that digest, beside the job's.

Every field of the document is an attribute: ``schema_version`` (``"MAJOR.MINOR"``; a reader
refuses a major it does not know); ``moruna``, ``host`` and ``args`` (what ran it, where, and the
run's argument values); ``run_id``; ``job_digest`` (the spec's public digest, SHA-256 of its exact
bytes as ``JobSpec.digest()`` gives it, or ``None`` for a job that holds a task a spec cannot
hold); ``execution_fingerprint`` (the normalized planning and recovery identity, which differs
from ``job_digest``; ``None`` for a run refused before its job was built); ``exit`` (``code``, ``status`` and ``diagnostic``); ``tasks``
(per task, keyed by its name in the spec: everything measured of it, its needs, what the plan
predicted, its observation and a recommendation for the next run; a measurement not made is
absent, never zero); ``resources`` (``cpu_core_s``, ``peak_memory_bytes`` and how it was
measured); ``sinks``; ``plan`` and ``plan_steps``; and the trace's own fields. Every number names
its unit (see ``docs/run-report.md``).
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
from typing import Any

__all__ = ["RunReport"]

#: The report document's schema, ``MAJOR.MINOR``: a reader refuses a major it does not know.
SCHEMA_VERSION = "2.1"

_REQUIRED = ("schema_version", "run_id", "job_digest", "exit", "tasks", "resources", "sinks")


def _canonical(document: Any) -> bytes:
    return json.dumps(
        document, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False
    ).encode("utf-8")


class RunReport:
    """A run's report: its fields as attributes, ``bytes()`` its canonical form, ``digest()``
    the digest of those bytes, ``to_json()`` the same text, ``str()`` a page for a person."""

    __slots__ = ("_document",)

    def __init__(self, document: dict[str, Any]) -> None:
        if not isinstance(document, dict):
            raise ValueError("a run report is a JSON object")
        missing = [key for key in _REQUIRED if key not in document]
        if missing:
            raise ValueError(f"not a run report: it has no {', '.join(missing)}")
        version = str(document["schema_version"])
        if version.split(".")[0] != SCHEMA_VERSION.split(".")[0]:
            raise ValueError(
                f"a run report of schema {version!r}; this Moruna reads major "
                f"{SCHEMA_VERSION.split('.')[0]} ({SCHEMA_VERSION})"
            )
        object.__setattr__(self, "_document", document)

    @classmethod
    def _from_json(cls, text: str) -> RunReport:
        return cls(json.loads(text))

    @classmethod
    def from_bytes(cls, source: bytes | bytearray | str | os.PathLike[str]) -> RunReport:
        """The report whose canonical bytes are ``source`` (or the file at that path). Bytes that
        are not a report, or not in the canonical form, are refused with ``ValueError``."""
        if isinstance(source, (bytes, bytearray)):
            data = bytes(source)
        else:
            data = Path(source).read_bytes()
        try:
            document = json.loads(data.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise ValueError(f"not a run report: {exc}") from None
        report = cls(document)
        if report.bytes() != data:
            raise ValueError(
                "not a run report's canonical bytes: keys sorted, no whitespace between tokens, "
                "UTF-8 (RunReport.bytes() writes them)"
            )
        return report

    def bytes(self) -> bytes:
        """The canonical form: sorted keys, no insignificant whitespace, UTF-8."""
        return _canonical(self._document)

    def digest(self) -> str:
        """``"sha256:"`` and the hex of SHA-256 over ``bytes()``."""
        return "sha256:" + hashlib.sha256(self.bytes()).hexdigest()

    def to_json(self) -> str:
        """The canonical form as text."""
        return self.bytes().decode("utf-8")

    def to_dict(self) -> dict[str, Any]:
        """The document, as a new dict."""
        return json.loads(self.bytes())

    def __getattr__(self, name: str) -> Any:
        document = object.__getattribute__(self, "_document")
        if name in document:
            value = document[name]
            return json.loads(json.dumps(value)) if isinstance(value, (dict, list)) else value
        raise AttributeError(f"a run report has no field {name!r}")

    def __setattr__(self, name: str, value: Any) -> None:
        raise AttributeError("a run report is what the run computed; it is not edited")

    def __eq__(self, other: object) -> bool:
        return isinstance(other, RunReport) and self.bytes() == other.bytes()

    def __hash__(self) -> int:
        return hash(self.bytes())

    def __repr__(self) -> str:
        d = self._document
        return (
            f"RunReport(run_id={d['run_id']}, exit={d['exit']['status']}, "
            f"wall_ms={d.get('wall_ms', 0.0):.1f})"
        )

    def __str__(self) -> str:
        return _render(self._document)


def _size(n: float) -> str:
    units = ("B", "KiB", "MiB", "GiB", "TiB")
    value, unit = float(n), 0
    while value >= 1024.0 and unit + 1 < len(units):
        value /= 1024.0
        unit += 1
    return f"{int(n)} B" if unit == 0 else f"{value:.1f} {units[unit]}"


def _render(d: dict[str, Any]) -> str:
    """The fixed layout (PY-I5): at most forty lines, however many tasks and notes."""
    exit_ = d["exit"]
    status = exit_["status"] + (f": {exit_['diagnostic']}" if exit_.get("diagnostic") else "")
    limits = d.get("limits_initial") or {}
    lines = [
        f"moruna run {d['run_id']} {status} (exit {exit_['code']})"
        + (" (resumed)" if d.get("resumed") else ""),
        f"job {d['job_digest'] or '(no spec)'}",
        f"wall {d.get('wall_ms', 0.0) / 1e3:.2f}s   peak "
        f"{_size(d['resources']['peak_memory_bytes'])} "
        f"({d.get('peak_fraction_of_ceiling', 0.0) * 100:.0f}% of ceiling "
        f"{_size(limits.get('memory_ceiling', 0))})   cpu {d['resources']['cpu_core_s']:.2f} "
        "core-s",
    ]
    if d.get("manifest"):
        lines.append(f"manifest {d['manifest']}")
    lines.append("")
    lines.append(f"{'task':<24} {'role':<8} {'status':<8} {'rows in':>12} {'rows out':>12} {'wall ms':>10}")
    shown = 20
    for task in d["tasks"][:shown]:
        def cell(value: Any, width: int, fmt: str = "") -> str:
            return f"{'-':>{width}}" if value is None else f"{value:>{width}{fmt}}"

        lines.append(
            f"{task['key'][:24]:<24} {task['role']:<8} {task.get('status', '-'):<8} "
            f"{cell(task.get('rows_in'), 12)} {cell(task.get('rows_out'), 12)} "
            f"{cell(task.get('wall_ms'), 10, '.1f')}"
        )
    if len(d["tasks"]) > shown:
        lines.append(f"... {len(d['tasks']) - shown} more tasks (report.tasks has them all)")
    for sink in d["sinks"][:4]:
        commit = f" snapshot {sink['snapshot_id']}" if sink.get("snapshot_id") else ""
        lines.append(f"sink {sink['task']}: {sink['rows']} rows{commit}")
    notes = d.get("notes") or []
    if notes:
        lines.append("")
        lines.append(f"notes ({len(notes)}):")
        lines.extend(f"  {note}" for note in notes[:2])
        if len(notes) > 2:
            lines.append(f"  ... {len(notes) - 2} more (report.notes has them all)")
    return "\n".join(lines[:40])
