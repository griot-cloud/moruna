"""Bindings to native engine facts. JSON conversion adds no storage or execution rules."""
from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal, TypedDict

from moruna import _core
from moruna._check import BUDGET_SECONDS, check

class ObjectIdentity(TypedDict):
    workspace: str
    bucket: str
    key: str

class ObjectVersion(TypedDict):
    etag: str | None
    generation: str | None

class PinnedObject(TypedDict):
    object: ObjectIdentity
    version: ObjectVersion
    bytes: int

class LogicalOutputIdentity(TypedDict):
    workspace: str
    run: str
    attempt: str
    task: str
    port: str

class RefusedCapture(TypedDict):
    Refused: str

class ArtifactChunk(TypedDict):
    encoding: Literal["ArrowIpcStream"]
    metadata: PinnedObject
    content_hash: str
    schema_identity: str
    rows: int

class ArtifactManifest(TypedDict):
    version: int
    identity: LogicalOutputIdentity
    policy: str
    state: Literal["Capturing", "Complete"] | RefusedCapture
    chunks: list[ArtifactChunk]
    rows: int
    bytes: int

class RetainedOutput(TypedDict):
    """Exact native manifest plus genuine partial/complete immutable pointers."""
    manifest: ArtifactManifest
    latest: PinnedObject | None
    complete: PinnedObject | None

@dataclass(frozen=True)
class Registration:
    """Successful real check/warm-up handoff; it does not publish an external registry entry."""
    report: dict[str, Any]
    cost: _core.CostSession

class RegistrationRefused(RuntimeError):
    """Actual check/warm-up refusal with its complete original report attached."""
    def __init__(self, report: dict[str, Any]) -> None:
        self.report = report
        super().__init__("registration preparation refused; inspect .report for the actual cause")

def prepare_registration(
    target: str,
    *,
    cost: _core.CostSession,
    kernel: str | None = None,
    seed: int = 0,
    budget: float = BUDGET_SECONDS,
    statistics: str | Path | dict[str, Any] | None = None,
    sandbox_python: str | None = None,
    sandbox_cgroup: str | None = None,
    args: str | None = None,
    assets: str | None = None,
) -> Registration:
    """Check and warm up before registration, then load the same fitted store/model for runs.

    The owning registry must call this before publishing its function entry and charge the
    returned native per-kernel ``warm_up.consumed`` facts. No registration is published here.
    Required warm-up cannot be disabled, missing measurements are not fabricated, and failed
    reports remain attached to RegistrationRefused. Lifetime/account peaks are not per-unit
    peaks; the returned native report keeps those distinctions.
    """
    report = check(target, kernel=kernel, seed=seed, warm_up=True, cost=cost,
                   budget=budget, statistics=statistics,
                   sandbox_python=sandbox_python, sandbox_cgroup=sandbox_cgroup,
                   args=args, assets=assets)
    kernels = report.get("kernels", [])
    if report.get("exit") != 0 or not kernels or any(
        not isinstance(result.get("warm_up"), dict)
        or result["warm_up"].get("error") is not None
        or result["warm_up"].get("observations", 0) <= 0
        for result in kernels
    ):
        raise RegistrationRefused(report)
    cost.refresh()
    return Registration(report=report, cost=cost)

def retained_outputs(ports: _core.EnginePorts) -> list[RetainedOutput]:
    """Actual publisher states; partial/refused output remains partial/refused."""
    return json.loads(ports.retained_outputs_json())

def artifact_manifest(ports: _core.EnginePorts, pinned: PinnedObject) -> ArtifactManifest:
    """Read exact pinned historical metadata through the existing live-authorized reader."""
    return json.loads(ports.artifact_manifest_json(json.dumps(pinned)))

def artifact_chunk(ports: _core.EnginePorts, pinned: PinnedObject, index: int, *, byte_budget: int) -> bytes:
    """One complete self-contained Arrow IPC stream, subject to actual native admission."""
    return ports.artifact_chunk(json.dumps(pinned), index, byte_budget)

def checkpoint_receipt(ports: _core.EnginePorts) -> PinnedObject | None:
    """Latest complete bucket acknowledgement; never a local-only pending receipt."""
    value = ports.checkpoint_receipt_json()
    return None if value is None else json.loads(value)

def checkpoint_pending(ports: _core.EnginePorts) -> PinnedObject | None:
    """Actual local acknowledgement when bucket publication refused; not portable."""
    value = ports.checkpoint_pending_json()
    return None if value is None else json.loads(value)

def restore_checkpoint(ports: _core.EnginePorts, pinned: PinnedObject, staging_root: str | Path) -> Path:
    """Verify and materialize every dependency before returning the real native resume path."""
    return Path(ports.restore_checkpoint(json.dumps(pinned), str(staging_root)))
