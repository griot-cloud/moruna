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
    def __init__(self, path: str, *, file_bytes: int | str | None = ...) -> None: ...

class KernelSpec:
    @property
    def wrapped(self) -> Any: ...
    @property
    def stateful(self) -> bool: ...
    @property
    def fingerprint(self) -> str: ...
    def __call__(self, *args: Any, **kwargs: Any) -> Any: ...

class StdKernel:
    @property
    def name(self) -> str: ...
    @property
    def args(self) -> str: ...
    @property
    def fingerprint(self) -> str: ...

class RunReport:
    @property
    def run_id(self) -> str: ...
    @property
    def exit(self) -> str | dict[str, Any]: ...
    @property
    def resumed(self) -> bool: ...
    @property
    def manifest(self) -> str | None: ...
    @property
    def wall_s(self) -> float: ...
    @property
    def limits(self) -> dict[str, Any]: ...
    @property
    def io_paths(self) -> dict[str, bool]: ...
    @property
    def peak_anon_bytes(self) -> int: ...
    @property
    def peak_fraction_of_ceiling(self) -> float: ...
    @property
    def worker_busy_fraction(self) -> float: ...
    @property
    def cpu_throttled_fraction(self) -> float: ...
    @property
    def source_bytes_per_s(self) -> float: ...
    @property
    def source_bandwidth(self) -> float: ...
    @property
    def staging_bandwidth(self) -> float: ...
    @property
    def staging_bytes_written(self) -> int: ...
    @property
    def staging_engaged(self) -> bool: ...
    @property
    def gil(self) -> list[tuple[int, str]]: ...
    @property
    def gil_serialised(self) -> bool: ...
    @property
    def sizer_used(self) -> str: ...
    @property
    def sizer_fallback_at(self) -> int | None: ...
    @property
    def bottleneck_timeline(self) -> list[tuple[float, str]]: ...
    @property
    def stages(self) -> list[dict[str, Any]]: ...
    @property
    def notes(self) -> list[str]: ...
    @property
    def overflow_failed(self) -> bool: ...
    @property
    def late_records(self) -> int: ...
    @property
    def trace_path(self) -> str | None: ...
    def to_json(self) -> str: ...

class MorunaError(Exception):
    kind: str
    message: str
    diagnostic: dict[str, Any]
    run_id: str | None
    manifest: str | None
    report: RunReport | None

class PlanError(MorunaError): ...
class KernelError(MorunaError): ...
class BudgetError(MorunaError): ...
class IoError(MorunaError): ...
class ConfigError(MorunaError): ...
class ResumeError(MorunaError): ...
class Cancelled(MorunaError): ...

def inspect_host() -> dict[str, Any]: ...
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
) -> KernelSpec: ...
def std_kernel(name: str, args_json: str) -> StdKernel: ...
def check_kernel(
    kernel: KernelSpec | StdKernel,
    *,
    name: str | None = ...,
    seed: int = ...,
    profiles_dir: str | None = ...,
) -> str: ...
def default_profiles_dir() -> str | None: ...
def run(
    source: Any,
    kernels: Any,
    sink: Any,
    *,
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
) -> RunReport: ...
def main(argv: list[str]) -> int: ...

_Callable = Callable[..., Any]
