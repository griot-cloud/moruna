"""Type stubs for the extension module (e.3).

Hand written, and checked against the module by PY-T5: every public symbol of ``moruna._core``
is named here, and nothing that is not.
"""

from collections.abc import Callable, Iterable, Mapping, Sequence
from typing import Any

__version__: str

class ParquetSource:
    def __init__(
        self,
        urls: str | Sequence[str],
        *,
        columns: Sequence[str] | None = ...,
        filters: Sequence[tuple[str, str, Any]] | None = ...,
    ) -> None: ...

class VortexSource:
    def __init__(
        self,
        urls: str | Sequence[str],
        *,
        columns: Sequence[str] | None = ...,
        split_bytes: int | None = ...,
    ) -> None: ...

class ArrowIpcSource:
    def __init__(
        self,
        urls: str | Sequence[str],
        *,
        columns: Sequence[str] | None = ...,
        split_rows: int | None = ...,
    ) -> None: ...

class TensorSource:
    def __init__(
        self, paths: str | Sequence[str], *, tensors: Sequence[str] | None = ...
    ) -> None: ...

class IteratorSource:
    def __init__(self, iterable: Iterable[Any], *, schema: Any) -> None: ...

class ParquetSink:
    def __init__(
        self,
        url: str,
        *,
        row_group_bytes: int | str | None = ...,
        file_bytes: int | str | None = ...,
        compression: str = ...,
    ) -> None: ...

class VortexSink:
    def __init__(self, url: str, *, file_bytes: int | str | None = ...) -> None: ...

class TensorSink:
    def __init__(
        self,
        path: str,
        *,
        format: str = ...,
        one_file_per_morsel: bool = ...,
        name: str = ...,
    ) -> None: ...

class ArrowIpcSink:
    def __init__(
        self,
        path: str,
        *,
        file_bytes: int | str | None = ...,
        compression: str = ...,
        level: int | None = ...,
    ) -> None: ...

# The classes a user extends (07 e.6, 08 f.10, 2026-09-29).

class Split:
    def __init__(self, id: int, rows: int, bytes: int | None = ...) -> None: ...
    @property
    def id(self) -> int: ...
    @property
    def rows(self) -> int: ...
    @property
    def bytes(self) -> int | None: ...

class Source:
    repeatable: bool
    def __init__(self, *args: Any, **kwargs: Any) -> None: ...
    def schema(self) -> Any: ...

class Sink:
    def __init__(self, *args: Any, **kwargs: Any) -> None: ...
    def finish(self) -> None: ...
    def checkpoint(self) -> bytes | None: ...
    def restore(self, state: bytes) -> None: ...

class KernelSpec:
    @property
    def wrapped(self) -> Any: ...
    @property
    def stateful(self) -> bool: ...
    @property
    def fingerprint(self) -> str: ...
    @property
    def memory_guard(self) -> bool: ...
    def __call__(self, *args: Any, **kwargs: Any) -> Any: ...

class StdKernel:
    @property
    def name(self) -> str: ...
    @property
    def args(self) -> str: ...
    @property
    def fingerprint(self) -> str: ...

class DeclaredTask:
    @property
    def kind(self) -> str: ...
    @property
    def settings(self) -> str: ...

class MorunaError(Exception):
    kind: str
    message: str
    diagnostic: dict[str, Any]
    run_id: str | None
    manifest: str | None
    report: Any

class PlanError(MorunaError): ...
class FunctionError(MorunaError): ...
class BudgetError(MorunaError): ...
class IoError(MorunaError): ...
class ConfigError(MorunaError): ...
class ResumeError(MorunaError): ...
class Cancelled(MorunaError): ...

def inspect_host() -> dict[str, Any]: ...
def declare_kind(declaration: str) -> str: ...
def declared_task(declaration: str, settings: str, serve: Any) -> DeclaredTask: ...
def kinds(declarations: list[str]) -> str: ...
def job_document(
    name: str | None, tasks: list[tuple[Any, ...]], base: bytes | None = ...,
    args: list[tuple[str, str]] = ...,
) -> bytes: ...
def check_job(
    name: str | None, tasks: list[tuple[Any, ...]], base: bytes | None = ...,
    args: list[tuple[str, str]] = ...,
) -> list[str]: ...
def explain_job(
    name: str | None, tasks: list[tuple[Any, ...]], base: bytes | None = ...,
    args: list[tuple[str, str]] = ..., *, planner: str | None = ...,
) -> str: ...
def read_job(document: bytes) -> str: ...
def check_args(declared: list[tuple[str, str]], given: str) -> list[str]: ...
def read_events(records: bytes) -> str: ...
def build_kernel(
    obj: Any,
    *,
    stateful: bool = ...,
    instances: int = ...,
    device_memory: bool = ...,
    accepts: str = ...,
    tier: str = ...,
    releases_gil: bool | None = ...,
    expected_amplification: float | None = ...,
    preferred_rows: int | None = ...,
    resume: str = ...,
    state_bytes: int | None = ...,
    input_schema: tuple[Any, ...] | None = ...,
    output_schema: tuple[Any, ...] | None = ...,
    lockfile: bytes | None = ...,
    origin: Any = ...,
    memory_guard: bool = ...,
) -> KernelSpec: ...
def std_kernel(name: str, args_json: str) -> StdKernel: ...
def check_kernel(
    kernel: KernelSpec | StdKernel,
    *,
    name: str | None = ...,
    seed: int = ...,
    warm_up: bool = ...,
    store: str | None = ...,
    budget: float = ...,
    statistics: str | None = ...,
    model: str | None = ...,
    cost: CostSession | None = ...,
) -> str: ...
def default_store_dir() -> str | None: ...

# The function package (SDD 16).
KERNEL_ABI: int
def manifest_json(text: str) -> str: ...
def package_manifest(path: str) -> str: ...
def package_wasm(module: str, manifest: str, out: str) -> None: ...
def check_package(
    path: str,
    *,
    seed: int = 0,
    warm_up: bool = True,
    store: str | None = None,
    budget: float = 60.0,
    statistics: str | None = None,
    model: str | None = None,
    sandbox_python: str | None = None,
    sandbox_cgroup: str | None = None,
    args: str | None = None,
    assets: str | None = None,
    cost: CostSession | None = None,
) -> str: ...
def aot(path: str) -> str: ...

class PackageKernel:
    @property
    def path(self) -> str: ...
    @property
    def fingerprint(self) -> str: ...
    @property
    def format(self) -> str: ...
    @property
    def manifest(self) -> dict[str, Any]: ...
    def __call__(self, batch: Any) -> Any: ...

def load(
    path: str,
    *,
    python: str | None = None,
    cgroup: str | None = None,
    args: Mapping[str, Any] | None = None,
    assets: str | None = None,
) -> PackageKernel: ...
def run(
    name: str,
    tasks: list[tuple[Any, ...]],
    *,
    job_digest: str | None = ...,
    explain: bool = ...,
    args: str | None = ...,
    arg_types: list[tuple[str, str]] = ...,
    held: list[str] = ...,
    served: Mapping[str, Any] | None = ...,
    pulled_images: str | None = ...,
    budget: int | str | None = ...,
    cpu: float | None = ...,
    trace: str | None = ...,
    staging_dir: str | None = ...,
    staging_limit: int | str | None = ...,
    on_error: str | tuple[str, int] | None = ...,
    ordered: bool = ...,
    sizer: str = ...,
    profiles_dir: str | None = ...,
    storage: Mapping[str, Any] | None = ...,
    host_profile: Any = ...,
    allow_gil: bool = ...,
    checkpoint: bool = ...,
    checkpoint_interval: float = ...,
    keep_checkpoint: bool = ...,
    resume: str | None = ...,
    cost: CostSession | None = ...,
    engine_ports: EnginePorts | None = ...,
) -> str: ...
def main(argv: list[str], needs: Mapping[str, Any] | None = ...) -> int: ...

class _OutputStream:
    def __init__(self, fd: int, original: Any) -> None: ...
    def write(self, text: str) -> int: ...
    def flush(self) -> None: ...
    def isatty(self) -> bool: ...
    def writable(self) -> bool: ...
    def __getattr__(self, name: str) -> Any: ...

def _record(levelno: int, text: str) -> bool: ...

_Callable = Callable[..., Any]

class CostSession:
    def __init__(self, store: str, *, model: str | None = ...) -> None: ...
    @property
    def store(self) -> str | None: ...
    @property
    def model(self) -> str: ...
    def refresh(self) -> None: ...

class EnginePorts:
    """Created only by a native host carrying existing live scoped authorities."""
    @staticmethod
    def from_guest() -> EnginePorts: ...
    def cost_session(self) -> CostSession: ...
    def retained_outputs_json(self) -> str: ...
    def artifact_manifest_json(self, pinned_json: str) -> str: ...
    def artifact_chunk(self, pinned_json: str, index: int, byte_budget: int) -> bytes: ...
    def checkpoint_receipt_json(self) -> str | None: ...
    def checkpoint_pending_json(self) -> str | None: ...
    def restore_checkpoint(self, pinned_json: str, staging_root: str) -> str: ...
