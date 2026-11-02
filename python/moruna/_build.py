"""``moruna build``: a Python kernel as an OCI image of four layers (SDD 30).

The image is what the ``image`` kind runs on a Linux node (SDD 19) and the Moruna kernel runs
too (SDD 27, 28)::

    1. runtime       python:3.14-slim's own layers, by digest, copied and never rebuilt
    2. dependencies  the project's locked dependencies, installed for the image's platform
    3. shim          ``_shim.py``, the stand-in ``moruna`` module and ``_declare.py``
    4. function      the file that defines the kernel, and the files it names

Layers 2 to 4 are uncompressed tars written the same way every time (sorted entries, time 0,
owner 0:0, modes 0644 and 0755), so a layer's digest, the sha256 of its tar, depends only on what
is in it: two functions of one project share layers 1 to 3, and one function built twice is one
image, bit for bit. Nothing here imports the engine (``moruna._core``), so the builder runs, and
is tested, wherever a Python 3.14, ``pyarrow`` and ``uv`` are.
"""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import http
import importlib.util
import io
import json
import os
import platform as host_platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Callable, Iterable, Iterator, Sequence
from pathlib import Path
from typing import IO, Any

EXIT_DONE = 0
EXIT_REFUSED = 2

# Layer 1: the official image's index, by digest. A newer python:3.14-slim is a later change to
# this constant and to nothing else; the layers of the image it names are never rebuilt.
RUNTIME_NAME = "docker.io/library/python:3.14-slim"
RUNTIME_REGISTRY = "registry-1.docker.io"
RUNTIME_REPOSITORY = "library/python"
RUNTIME_INDEX = "sha256:cd89c30eda26ee911a4611b18680086e0ea42e6f5f8cbe653d07fa7958c9cec8"

# Where things are in the image.
PYTHON = "/usr/local/bin/python3"
SHIM_DIR = "opt/moruna"
SITE_PACKAGES = f"{SHIM_DIR}/site-packages"
APP_DIR = "app"

MEDIA_MANIFEST = "application/vnd.oci.image.manifest.v1+json"
MEDIA_INDEX = "application/vnd.oci.image.index.v1+json"
MEDIA_CONFIG = "application/vnd.oci.image.config.v1+json"
MEDIA_LAYER = "application/vnd.oci.image.layer.v1.tar"
REF_NAME = "org.opencontainers.image.ref.name"
LAYER_ROLES = ("runtime", "dependencies", "shim", "function")

# The reproducible tar (SDD 30 f.4).
TAR_TIME = 0
FILE_MODE = 0o644
EXEC_MODE = 0o755
DIR_MODE = 0o755
LINK_MODE = 0o777

# The pyarrow the shim needs (SDD 30 f.2), from this repository's own lock: used when the
# project's lock does not pin pyarrow itself. Both wheels of the platforms an image has.
SHIM_PYARROW = (
    "pyarrow",
    "25.0.1",
    (
        "0b726ad7e7b669be982b0c71c07fe4b037d654354130da79a7902a669e93a66b",
        "9171748cdf796972d85a4b60157c279913e242992e350c90c7450182a9838b2a",
    ),
)
PYARROW_FLOOR = 17

# A `.pyc` starts with its magic number: two bytes, then `\r\n`; four are read.
PYC_MAGIC_BYTES = 4

_PIN = re.compile(r"^([A-Za-z0-9][A-Za-z0-9._-]*)==([A-Za-z0-9.!+_-]+)\s*(?:;\s*(.+))?$")
_HASH = re.compile(r"--hash=sha256:([0-9a-f]{64})\b")
_HASH_ANY = re.compile(r"--hash=\S+")
_COMMENT = re.compile(r"(^|\s)#.*$")
_DIGEST = re.compile(r"^sha256:([0-9a-f]{64})$")


class Refused(Exception):
    """A build refused, with the sentence that names why (SDD 30 h)."""


# ---------------------------------------------------------------------------------------------
# The platform


@dataclasses.dataclass(frozen=True)
class Platform:
    """The platform an image is built for."""

    arch: str  # the OCI name: arm64 or amd64
    uv: str  # uv's name for the wheels it may install
    name: str  # what the user wrote and the output says: linux/aarch64 or linux/x86_64


PLATFORMS = {
    "aarch64": Platform("arm64", "aarch64-manylinux_2_28", "linux/aarch64"),
    "x86_64": Platform("amd64", "x86_64-manylinux_2_28", "linux/x86_64"),
}
_ARCH_NAMES = {"aarch64": "aarch64", "arm64": "aarch64", "x86_64": "x86_64", "amd64": "x86_64"}


def parse_platform(text: str | None) -> Platform:
    """``linux/aarch64`` or ``linux/x86_64`` (``arm64`` and ``amd64`` are the same two); absent,
    the host's architecture on Linux and aarch64 on a Mac (SDD 30 f.2)."""
    if text is None:
        if host_platform.system() == "Darwin":
            return PLATFORMS["aarch64"]
        machine = _ARCH_NAMES.get(host_platform.machine().lower())
        if host_platform.system() != "Linux" or machine is None:
            raise Refused(
                f"--platform: this machine is {host_platform.system()} "
                f"{host_platform.machine()}, which has no default; name linux/aarch64 or "
                "linux/x86_64"
            )
        return PLATFORMS[machine]
    operating_system, _, arch = text.partition("/")
    if operating_system != "linux" or arch not in _ARCH_NAMES:
        raise Refused(f"--platform {text}: an image is built for linux/aarch64 or linux/x86_64")
    return PLATFORMS[_ARCH_NAMES[arch]]


# ---------------------------------------------------------------------------------------------
# The lock


@dataclasses.dataclass(frozen=True)
class Pin:
    """One requirement of a lock: an exact version and the hashes its files may have."""

    name: str
    version: str
    hashes: tuple[str, ...]
    marker: str | None = None


def normal(name: str) -> str:
    """A distribution name as PEP 503 normalises it."""
    return re.sub(r"[-_.]+", "-", name).lower()


def find_lock(start: Path, given: Path | None) -> Path:
    """The lock of the project that holds ``start``: ``--lock`` when given, else the nearest
    directory at or above ``start`` that has a ``uv.lock`` or a ``requirements.lock`` (both is
    refused: a build does not choose between two locks)."""
    if given is not None:
        if not given.is_file():
            raise Refused(f"--lock {given}: no such file")
        return given.resolve()
    for directory in (start, *start.parents):
        found = [directory / name for name in ("uv.lock", "requirements.lock")]
        found = [path for path in found if path.is_file()]
        if len(found) > 1:
            raise Refused(
                f"{directory} holds both uv.lock and requirements.lock; name the one to build "
                "from with --lock"
            )
        if found:
            return found[0].resolve()
    raise Refused(
        f"no lock: a build installs exactly what a lock pins, so {start} or a directory above it "
        "needs a uv.lock or a requirements.lock (uv lock; or uv pip compile --generate-hashes). "
        "Nothing is resolved without one"
    )


def uv_command() -> list[str]:
    """How to run uv: the executable on the path, else the interpreter's own ``uv`` module."""
    found = shutil.which("uv")
    if found:
        return [found]
    if importlib.util.find_spec("uv") is not None:
        return [sys.executable, "-m", "uv"]
    raise Refused(
        "moruna build installs the lock's wheels with uv, which is neither on the path nor "
        f"importable by {sys.executable}"
    )


def _tool_env() -> dict[str, str]:
    """The environment uv runs in: the caller's, but with nothing that changes what is
    installed (bytecode, hashes) or where it is copied from."""
    env = {k: v for k, v in os.environ.items() if k not in ("UV_REQUIRE_HASHES", "UV_LINK_MODE")}
    env["UV_COMPILE_BYTECODE"] = "0"
    return env


def lock_text(lock: Path, uv: list[str]) -> str:
    """The lock as requirements with hashes: a ``requirements.lock`` as written, a ``uv.lock``
    exported by uv without its development group or its own project."""
    if lock.name != "uv.lock":
        return lock.read_text(encoding="utf-8")
    command = [
        *uv, "export", "--frozen", "--no-config", "--no-dev", "--no-emit-project",
        "--no-emit-workspace", "--no-header", "--no-annotate", "--format", "requirements.txt",
        "--project", str(lock.parent),
    ]  # fmt: skip
    done = subprocess.run(
        command, capture_output=True, text=True, check=False, cwd=lock.parent, env=_tool_env()
    )
    if done.returncode != 0:
        raise Refused(f"{lock}: uv export failed: {_last_line(done.stderr)}")
    return done.stdout


def _last_line(text: str) -> str:
    lines = [line for line in text.splitlines() if line.strip()]
    return lines[-1] if lines else "no message"


def parse_lock(text: str, where: str) -> list[Pin]:
    """The pins of requirements text, each ``name==version`` with at least one
    ``--hash=sha256:<hex>`` and, optionally, a marker; anything else is refused naming the line."""
    logical: list[tuple[int, str]] = []
    pending = ""
    start = 0
    for number, raw in enumerate(text.splitlines(), 1):
        line = _COMMENT.sub("", raw).rstrip()
        if not pending:
            start = number
        if line.endswith("\\"):
            pending += line[:-1] + " "
            continue
        whole = (pending + line).strip()
        pending = ""
        if whole:
            logical.append((start, whole))
    if pending.strip():
        logical.append((start, pending.strip()))
    pins: dict[str, Pin] = {}
    for number, line in logical:
        hashes = tuple(sorted(set(_HASH.findall(line))))
        bare = _HASH_ANY.sub("", line).strip()
        match = _PIN.match(bare)
        if match is None:
            raise Refused(
                f"{where}:{number}: `{bare}` is not name==version; a lock pins every "
                "requirement exactly (options are not read from a lock: pass --index-url to the "
                "build)"
            )
        if not hashes or len(hashes) != len(_HASH_ANY.findall(line)):
            raise Refused(
                f"{where}:{number}: {match.group(1)} needs --hash=sha256:<64 hex> on every "
                "hash; generate the lock with hashes"
            )
        name = normal(match.group(1))
        pins[name] = Pin(name, match.group(2), hashes, match.group(3))
    return [pins[name] for name in sorted(pins)]


def with_pyarrow(pins: list[Pin], where: str) -> list[Pin]:
    """The pins plus what the shim needs: the lock's own pyarrow when it has one (it must be new
    enough to read the stream), else this Moruna's pin."""
    for pin in pins:
        if pin.name == "pyarrow":
            major = re.match(r"\d+", pin.version)
            if major is None or int(major.group()) < PYARROW_FLOOR:
                raise Refused(
                    f"{where}: pyarrow=={pin.version}; the shim needs pyarrow >= {PYARROW_FLOOR}"
                )
            return pins
    name, version, hashes = SHIM_PYARROW
    return sorted([*pins, Pin(name, version, hashes)], key=lambda p: p.name)


def requirements_text(pins: Iterable[Pin]) -> str:
    """The pins as one file uv reads with ``--require-hashes``, in name order."""
    lines = []
    for pin in sorted(pins, key=lambda p: p.name):
        head = f"{pin.name}=={pin.version}" + (f" ; {pin.marker}" if pin.marker else "")
        body = " \\\n".join(f"    --hash=sha256:{h}" for h in pin.hashes)
        lines.append(f"{head} \\\n{body}\n")
    return "".join(lines)


# ---------------------------------------------------------------------------------------------
# Bytes, digests and tars


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonical(document: Any) -> bytes:
    """A JSON document as bytes, the same every time."""
    return json.dumps(document, sort_keys=True, separators=(",", ":")).encode()


@dataclasses.dataclass(frozen=True)
class Blob:
    """A file named by its digest: the sha256 of its bytes."""

    digest: str  # sha256:<hex>
    size: int
    path: Path

    @property
    def hex(self) -> str:
        return self.digest.split(":", 1)[1]


@dataclasses.dataclass(frozen=True)
class Entry:
    """One path of a layer, as the reproducible tar will write it."""

    path: str
    kind: str  # "dir", "file" or "link"
    mode: int
    source: Path | None = None  # a file's bytes, on disk
    data: bytes | None = None  # or in memory
    target: str = ""  # a symbolic link's target


class _Hashing:
    """A write-only file that hashes and counts what passes."""

    def __init__(self, handle: IO[bytes]) -> None:
        self.handle = handle
        self.digest = hashlib.sha256()
        self.size = 0

    def write(self, data: bytes) -> int:
        self.digest.update(data)
        self.size += len(data)
        return self.handle.write(data)

    def flush(self) -> None:
        self.handle.flush()


def _info(entry: Entry, size: int) -> tarfile.TarInfo:
    info = tarfile.TarInfo(entry.path)
    info.mtime = TAR_TIME
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    info.mode = entry.mode
    info.pax_headers = {}
    if entry.kind == "dir":
        info.type = tarfile.DIRTYPE
    elif entry.kind == "link":
        info.type = tarfile.SYMTYPE
        info.linkname = entry.target
    else:
        info.type = tarfile.REGTYPE
        info.size = size
    return info


def write_layer(entries: Iterable[Entry], work: Path) -> Blob:
    """The tar of ``entries`` in path order, byte for byte the same for the same entries, written
    under ``work`` and named by its sha256 (SDD 30 f.4). Parent directories of every path are
    entries too, and a path made twice is refused."""
    by_path: dict[str, Entry] = {}
    for entry in entries:
        if entry.path in by_path:
            raise Refused(f"layer: {entry.path} is made twice")
        by_path[entry.path] = entry
    for path in list(by_path):
        parent = path
        while "/" in parent:
            parent = parent.rsplit("/", 1)[0]
            by_path.setdefault(parent, Entry(parent, "dir", DIR_MODE))
    work.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(dir=work, prefix="layer-")
    temp = Path(name)
    with os.fdopen(fd, "wb") as handle:
        hashing = _Hashing(handle)
        with tarfile.open(fileobj=hashing, mode="w|", format=tarfile.PAX_FORMAT) as archive:
            for path in sorted(by_path):
                entry = by_path[path]
                if entry.kind != "file":
                    archive.addfile(_info(entry, 0))
                elif entry.data is not None:
                    archive.addfile(_info(entry, len(entry.data)), io.BytesIO(entry.data))
                else:
                    assert entry.source is not None, entry.path  # a file has bytes
                    with entry.source.open("rb") as content:
                        archive.addfile(_info(entry, entry.source.stat().st_size), content)
    digest = hashing.digest.hexdigest()
    final = work / digest
    os.replace(temp, final)
    return Blob(f"sha256:{digest}", hashing.size, final)


def tree_entries(root: Path, prefix: str) -> Iterator[Entry]:
    """Every file, directory and symbolic link under ``root`` as entries below ``prefix``, with
    the modes of SDD 30 f.4."""
    for directory, dirs, files in os.walk(root, followlinks=False):
        dirs.sort()
        base = Path(directory)
        relative = base.relative_to(root).as_posix()
        here = prefix if relative == "." else f"{prefix}/{relative}"
        yield Entry(here, "dir", DIR_MODE)
        for name in sorted(files + [d for d in dirs if (base / d).is_symlink()]):
            path = base / name
            info = path.lstat()
            if path.is_symlink():
                yield Entry(f"{here}/{name}", "link", LINK_MODE, target=os.readlink(path))
            elif path.is_file():
                mode = EXEC_MODE if info.st_mode & 0o111 else FILE_MODE
                yield Entry(f"{here}/{name}", "file", mode, source=path)
            else:
                raise Refused(f"{path}: neither a file, a directory nor a link; a layer holds none")


# ---------------------------------------------------------------------------------------------
# Layer 1: the runtime


@dataclasses.dataclass(frozen=True)
class Runtime:
    """The image layer 1 comes from: its manifest digest, config and layers (as descriptors)
    with the file each is in."""

    name: str
    digest: str
    config: dict[str, Any]
    config_blob: Blob
    layers: tuple[dict[str, Any], ...]
    files: dict[str, Path]  # layer digest -> its file


def _check_blob(path: Path, digest: str, size: int | None, what: str) -> Blob:
    if not path.is_file():
        raise Refused(f"{what}: {path} does not exist")
    actual = path.stat().st_size
    if size is not None and actual != size:
        raise Refused(f"{what} {digest}: {actual} bytes, the manifest says {size}")
    found = f"sha256:{sha256_file(path)}"
    if found != digest:
        raise Refused(f"{what} {digest}: its bytes hash to {found}")
    return Blob(digest, actual, path)


def _digest_hex(digest: str, what: str) -> str:
    match = _DIGEST.match(digest)
    if match is None:
        raise Refused(f"{what}: {digest!r} is not a sha256 digest")
    return match.group(1)


def _runtime_from_manifest(
    name: str,
    digest: str,
    manifest: dict[str, Any],
    blobs: Callable[[str], Path],
    platform: Platform,
) -> Runtime:
    if manifest.get("mediaType", MEDIA_MANIFEST) not in (
        MEDIA_MANIFEST,
        "application/vnd.docker.distribution.manifest.v2+json",
    ):
        raise Refused(f"{name}: {manifest.get('mediaType')} is not an image manifest")
    config_descriptor = manifest["config"]
    config_blob = _check_blob(
        blobs(config_descriptor["digest"]),
        config_descriptor["digest"],
        config_descriptor.get("size"),
        f"{name}: the config",
    )
    config = json.loads(config_blob.path.read_bytes())
    if config.get("os") != "linux" or config.get("architecture") != platform.arch:
        raise Refused(
            f"{name}: the runtime is {config.get('os')}/{config.get('architecture')}, and the "
            f"image is for {platform.name}"
        )
    layers = tuple(manifest["layers"])
    files = {layer["digest"]: blobs(layer["digest"]) for layer in layers}
    return Runtime(name, digest, config, config_blob, layers, files)


def read_runtime_layout(layout: Path, reference: str, platform: Platform) -> Runtime:
    """The runtime from an OCI layout on this machine (``--runtime-layout``): the image
    ``reference`` names, by digest or by ``org.opencontainers.image.ref.name``. Every blob is
    hashed against its name before it is used."""
    try:
        marker = json.loads((layout / "oci-layout").read_bytes())
        index = json.loads((layout / "index.json").read_bytes())
    except (OSError, ValueError) as exc:
        raise Refused(f"--runtime-layout {layout}: not an OCI layout: {exc}") from exc
    if marker.get("imageLayoutVersion") != "1.0.0":
        raise Refused(f"--runtime-layout {layout}: only layout version 1.0.0 is read")

    def blob_path(digest: str) -> Path:
        return layout / "blobs" / "sha256" / _digest_hex(digest, str(layout))

    if _DIGEST.match(reference):
        digest = reference
    else:
        named = [
            m
            for m in index.get("manifests", [])
            if m.get("annotations", {}).get(REF_NAME) == reference
        ]
        if len(named) != 1:
            raise Refused(
                f"--runtime-reference {reference}: the layout has {len(named)} images of that "
                "name; name one by its digest"
            )
        digest = named[0]["digest"]
    manifest_file = _check_blob(blob_path(digest), digest, None, f"{layout}: the manifest")
    manifest = json.loads(manifest_file.path.read_bytes())
    return _runtime_from_manifest(f"{layout}@{digest}", digest, manifest, blob_path, platform)


Transport = Callable[[str, dict[str, str]], tuple[int, dict[str, str], IO[bytes]]]


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args: Any, **kwargs: Any) -> None:
        return None


def urllib_transport(url: str, headers: dict[str, str]) -> tuple[int, dict[str, str], IO[bytes]]:
    """``GET url``: the status, the headers (lower case names) and the body, redirects not
    followed (the registry follows them, dropping credentials that must not leave its host)."""
    request = urllib.request.Request(url, headers=headers)
    opener = urllib.request.build_opener(_NoRedirect)
    try:
        response = opener.open(request, timeout=60)
    except urllib.error.HTTPError as exc:
        return exc.code, {k.lower(): v for k, v in exc.headers.items()}, exc
    except urllib.error.URLError as exc:
        raise Refused(f"{url}: {exc.reason}") from exc
    return response.status, {k.lower(): v for k, v in response.headers.items()}, response


class Registry:
    """A read-only client of one repository of an OCI registry, anonymous, which verifies every
    byte against the digest it asked for."""

    def __init__(self, host: str, repository: str, transport: Transport = urllib_transport) -> None:
        self.host = host
        self.repository = repository
        self.transport = transport
        self.token: str | None = None

    def _open(self, url: str, accept: str) -> IO[bytes]:
        headers = {"Accept": accept}
        for _ in range(8):
            if self.token and urllib.parse.urlsplit(url).netloc == self.host:
                headers["Authorization"] = f"Bearer {self.token}"
            else:
                headers.pop("Authorization", None)
            status, got, body = self.transport(url, headers)
            if status == http.HTTPStatus.OK:
                return body
            if status in (301, 302, 303, 307, 308) and "location" in got:
                url = urllib.parse.urljoin(url, got["location"])
                continue
            if (
                status == http.HTTPStatus.UNAUTHORIZED
                and self.token is None
                and "www-authenticate" in got
            ):
                self.token = self._authenticate(got["www-authenticate"])
                continue
            raise Refused(f"{url}: the registry answered {status}")
        raise Refused(f"{url}: too many redirects")

    def _authenticate(self, challenge: str) -> str:
        fields = dict(re.findall(r'(\w+)="([^"]*)"', challenge))
        if "realm" not in fields:
            raise Refused(f"{self.host}: cannot read the challenge {challenge!r}")
        query = {k: fields[k] for k in ("service", "scope") if k in fields}
        url = fields["realm"] + "?" + urllib.parse.urlencode(query)
        status, _, body = self.transport(url, {"Accept": "application/json"})
        if status != http.HTTPStatus.OK:
            raise Refused(f"{url}: the token service answered {status}")
        document = json.loads(body.read())
        token = document.get("token") or document.get("access_token")
        if not token:
            raise Refused(f"{url}: the token service gave no token")
        return str(token)

    def fetch(self, kind: str, digest: str, accept: str, dest: Path) -> Path:
        """``digest`` of ``kind`` (``manifests`` or ``blobs``) into ``dest``, whole and hashed
        as it comes; an existing ``dest`` that hashes to the digest is the answer."""
        wanted = _digest_hex(digest, self.repository)
        if dest.is_file() and sha256_file(dest) == wanted:
            return dest
        url = f"https://{self.host}/v2/{self.repository}/{kind}/{digest}"
        dest.parent.mkdir(parents=True, exist_ok=True)
        hasher = hashlib.sha256()
        fd, temp = tempfile.mkstemp(dir=dest.parent, prefix=".part-")
        try:
            with os.fdopen(fd, "wb") as out, self._open(url, accept) as body:
                for chunk in iter(lambda: body.read(1 << 20), b""):
                    hasher.update(chunk)
                    out.write(chunk)
            if hasher.hexdigest() != wanted:
                raise Refused(f"{url}: its bytes hash to sha256:{hasher.hexdigest()}")
            os.replace(temp, dest)
        finally:
            if os.path.exists(temp):
                os.unlink(temp)
        return dest


def fetch_runtime(platform: Platform, cache: Path, registry: Registry | None = None) -> Runtime:
    """Layer 1's image, by the pinned index digest: the index, the manifest it names for
    ``platform``, the config and the layers, each into ``cache`` (a content-addressed blob
    directory) unless already there."""
    registry = registry or Registry(RUNTIME_REGISTRY, RUNTIME_REPOSITORY)
    store = cache / "blobs" / "sha256"

    def get(kind: str, digest: str, accept: str) -> Path:
        return registry.fetch(kind, digest, accept, store / _digest_hex(digest, "runtime"))

    index_file = get("manifests", RUNTIME_INDEX, MEDIA_INDEX)
    index = json.loads(index_file.read_bytes())
    chosen = [
        m
        for m in index.get("manifests", [])
        if m.get("platform", {}).get("os") == "linux"
        and m.get("platform", {}).get("architecture") == platform.arch
    ]
    if len(chosen) != 1:
        raise Refused(f"{RUNTIME_NAME}: {len(chosen)} manifests for {platform.name}, not one")
    digest = chosen[0]["digest"]
    manifest_file = get("manifests", digest, MEDIA_MANIFEST)
    manifest = json.loads(manifest_file.read_bytes())

    def blob(blob_digest: str) -> Path:
        return get("blobs", blob_digest, "application/octet-stream")

    return _runtime_from_manifest(RUNTIME_NAME, digest, manifest, blob, platform)


# ---------------------------------------------------------------------------------------------
# Layer 2: the locked dependencies


@dataclasses.dataclass(frozen=True)
class Bytecode:
    """What the runtime's CPython needs of the bytecode written for it: its minor version,
    the version text the image says, and the magic number of the ``.pyc`` files it holds, when
    it holds any (the official image removes them)."""

    minor: int
    version: str
    magic: int | None


@dataclasses.dataclass(frozen=True)
class Toolchain:
    """The interpreter that reads the wheels' tags and compiles the bytecode, and its minor."""

    interpreter: str
    minor: int


_FINAL = re.compile(r"^3\.(\d+)\.\d+$")
_PYC = re.compile(r"^usr/local/lib/python3\.(\d+)/.*__pycache__/[^/]+\.cpython-3\d+\.pyc$")


def runtime_bytecode(runtime: Runtime) -> Bytecode:
    """The CPython of the runtime, from the image itself: ``PYTHON_VERSION`` of its config (a
    final release: the magic number of bytecode does not change within one, and may before it)
    and, when its layers hold a ``.pyc`` of the standard library, that file's magic number
    (SDD 30 f.2)."""
    entries = runtime.config.get("config", {}).get("Env", [])
    version = next((e.split("=", 1)[1] for e in entries if e.startswith("PYTHON_VERSION=")), None)
    if version is None:
        raise Refused(f"{runtime.name}: its config has no PYTHON_VERSION; the bytecode needs one")
    final = _FINAL.match(version)
    if final is None:
        raise Refused(
            f"{runtime.name}: CPython {version} is not a final release, whose bytecode "
            "format is not settled"
        )
    minor = int(final.group(1))
    magic = None
    for descriptor in runtime.layers:
        with tarfile.open(runtime.files[descriptor["digest"]], mode="r|*") as archive:
            for member in archive:
                found = _PYC.match(member.name.removeprefix("./"))
                if (
                    found
                    and int(found.group(1)) == minor
                    and member.isfile()
                    and member.size >= PYC_MAGIC_BYTES
                ):
                    reader = archive.extractfile(member)
                    assert reader is not None, member.name  # a file has a reader
                    magic = int.from_bytes(reader.read(PYC_MAGIC_BYTES)[:2], "little")
                    break
        if magic is not None:
            break
    return Bytecode(minor, version, magic)


_PROBE = (
    "import importlib.util, sys, sysconfig; "
    "print(sys.implementation.name == 'cpython' and sys.version_info[:2] == (3, {minor}) "
    "and sys.version_info.releaselevel == 'final' "
    "and not sysconfig.get_config_var('Py_GIL_DISABLED') "
    "and ({magic} is None or int.from_bytes(importlib.util.MAGIC_NUMBER[:2], 'little') == {magic}))"
)


def _is_runtime_build(interpreter: str, bytecode: Bytecode) -> bool:
    """Whether ``interpreter`` writes the bytecode the runtime reads: CPython, the standard
    build, a final release of the runtime's minor, with the runtime's magic number."""
    probe = subprocess.run(
        [interpreter, "-c", _PROBE.format(minor=bytecode.minor, magic=bytecode.magic)],
        capture_output=True,
        text=True,
        check=False,
    )
    return probe.returncode == 0 and probe.stdout.strip() == "True"


def tag_interpreter(bytecode: Bytecode) -> Toolchain:
    """The interpreter uv reads the wheels' tags from and the bytecode is compiled by: the
    standard CPython build the runtime is. A free threaded build would be given the free
    threaded wheels and write bytecode the runtime refuses, so it is not used, and neither is
    any whose magic number is not the runtime's: bytecode that does not match is never shipped.
    This interpreter when it is one, else ``python3.<minor>`` or ``python3`` on the path."""
    tried = [sys.executable]
    for name in (f"python3.{bytecode.minor}", "python3"):
        found = shutil.which(name)
        if found and found not in tried:
            tried.append(found)
    for candidate in tried:
        if _is_runtime_build(candidate, bytecode):
            return Toolchain(candidate, bytecode.minor)
    magic = f"magic number {bytecode.magic}" if bytecode.magic is not None else "its magic number"
    raise Refused(
        f"an image's wheels and bytecode are made by the runtime's CPython, the standard build of "
        f"a final {bytecode.version.rsplit('.', 1)[0]} release with {magic}, and none was "
        f"found (tried {', '.join(tried)}); run moruna build with one"
    )


def install(
    requirements: Path, target: Path, platform: Platform, options: Sequence[str], tools: Toolchain
) -> None:
    """Install the pins of ``requirements`` into ``target`` for ``platform`` with uv. Wheels
    only: a source distribution is refused (``--no-build``, which is ``--only-binary :all:``),
    so no build backend and no package code runs here; hashes are required, there is no
    dependency resolution (the lock has every one) and nothing is compiled by uv."""
    command = [
        *uv_command(), "pip", "install", "--no-config", "--python", tools.interpreter,
        "--target", str(target), "--python-platform", platform.uv,
        "--python-version", f"3.{tools.minor}", "--no-build",
        "--require-hashes", "--no-deps", "--link-mode", "copy", "-r", str(requirements),
        *options,
    ]  # fmt: skip
    done = subprocess.run(command, capture_output=True, text=True, check=False, env=_tool_env())
    if done.returncode != 0:
        raise Refused(
            f"uv could not install the lock for {platform.name}: {_last_line(done.stderr)}"
        )
    clean_install(target)


def compile_tree(root: Path, destination: str, tools: Toolchain) -> None:
    """Write the bytecode of every ``.py`` under ``root`` beside it, as the image will have
    it at ``destination`` (SDD 30 f.2): hash based and unchecked, so Python uses the file
    whatever the time of the source, with the path the image will have in tracebacks, and so
    the same bytes whatever the directory it was built in. A source that does not compile is
    left as it is: importing it fails in the image as it would anywhere."""
    env = {k: v for k, v in os.environ.items() if not k.startswith(("PYTHON", "UV_"))}
    env["PYTHONHASHSEED"] = "0"
    command = [
        tools.interpreter, "-m", "compileall", "-q", "-f", "--invalidation-mode",
        "unchecked-hash", "-d", destination, str(root),
    ]  # fmt: skip
    done = subprocess.run(command, capture_output=True, text=True, check=False, env=env)
    if done.returncode not in (0, 1):  # 1: some file did not compile
        raise Refused(f"compiling {destination}: {_last_line(done.stderr)}")


def clean_install(target: Path) -> None:
    """Take out of an install what is about this machine and this moment and not about the
    packages: the scripts (they name this machine's interpreter), uv's lock file, and each
    package's ``uv_cache.json`` (a timestamp), with its line of ``RECORD``, and any bytecode a
    wheel shipped (it is compiled again, for the runtime's interpreter). What is left is a
    function of the wheels alone (SDD 30 f.4)."""
    shutil.rmtree(target / "bin", ignore_errors=True)
    (target / ".lock").unlink(missing_ok=True)
    for cache in sorted(target.rglob("__pycache__")):
        shutil.rmtree(cache)
    for stamp in sorted(target.glob("*.dist-info/uv_cache.json")):
        stamp.unlink()
        record = stamp.with_name("RECORD")
        if record.is_file():
            kept = [
                line
                for line in record.read_text(encoding="utf-8").splitlines(keepends=True)
                if not line.startswith(f"{stamp.parent.name}/uv_cache.json,")
            ]
            record.write_text("".join(kept), encoding="utf-8")


# ---------------------------------------------------------------------------------------------
# Layer 3 and 4: the shim and the function


def stage_shim(dest: Path) -> None:
    """Layer 3's files written under ``dest`` as they are at ``/opt/moruna``: the shim, the
    stand-in ``moruna`` and the module it reads. They are this package's own files, so they
    are the same for every image of this Moruna."""
    here = Path(__file__).parent
    parts = {
        "shim.py": "_shim.py",
        "moruna/__init__.py": "_standin.py",
        "moruna/_declare.py": "_declare.py",
    }
    for target, source in parts.items():
        path = dest / target
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes((here / source).read_bytes())
        path.chmod(FILE_MODE)


@dataclasses.dataclass(frozen=True)
class Function:
    """What a build needs to know of a kernel: where it is imported from and what it names."""

    module: str  # dotted, as the shim imports it
    name: str  # the attribute of the module
    source: Path  # the file defining the module
    files: tuple[Path, ...] = ()  # files it names, under the import root
    fingerprint: str | None = None
    stateful: bool = False

    @property
    def entry(self) -> str:
        return f"{self.module}:{self.name}"

    @property
    def root(self) -> Path:
        """The directory the module is imported from: what a ``sys.path`` entry names."""
        depth = len(self.module.split("."))
        package = self.source.name == "__init__.py"
        return self.source.resolve().parents[depth if package else depth - 1]


def function_files(function: Function) -> dict[str, Path]:
    """Layer 4's files: the one that defines the function at its import path under ``/app``,
    the ``__init__.py`` of each package above it, and the files it names at their paths below
    the import root; each path of the image with the file it is made from."""
    source = function.source.resolve()
    root = function.root
    files: dict[str, Path] = {}

    def add(path: Path) -> None:
        try:
            relative = path.resolve().relative_to(root)
        except ValueError:
            raise Refused(
                f"{path}: not under {root}, where {function.module} is imported from"
            ) from None
        if not path.is_file():
            raise Refused(f"{path}: no such file")
        files[f"{APP_DIR}/{relative.as_posix()}"] = path.resolve()

    add(source)
    directory = source.parent
    while directory != root:
        init = directory / "__init__.py"
        if init.is_file() and init != source:
            add(init)
        directory = directory.parent
    for extra in function.files:
        add(extra)
    return files


def stage_function(function: Function, dest: Path) -> None:
    """Layer 4's files written under ``dest`` as they are at ``/app`` (which exists even when
    nothing is in it), each with the mode the layer gives it."""
    dest.mkdir(parents=True, exist_ok=True)
    for target, source in function_files(function).items():
        path = dest / target.removeprefix(f"{APP_DIR}/")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(source.read_bytes())
        path.chmod(EXEC_MODE if source.stat().st_mode & 0o111 else FILE_MODE)


# ---------------------------------------------------------------------------------------------
# The image


@dataclasses.dataclass(frozen=True)
class LayerInfo:
    role: str
    blob: Blob
    media_type: str
    diff_id: str


@dataclasses.dataclass(frozen=True)
class Built:
    """A finished build: the layout it is in and what is in it."""

    layout: Path
    reference: str
    digest: str
    layers: tuple[LayerInfo, ...]
    lock: Path
    platform: Platform

    def report(self) -> dict[str, Any]:
        return {
            "moruna_build": 1,
            "image": self.digest,
            "layout": str(self.layout),
            "reference": self.reference,
            "platform": self.platform.name,
            "lock": str(self.lock),
            "layers": [
                {
                    "layer": i,
                    "role": layer.role,
                    "digest": layer.blob.digest,
                    "size": layer.blob.size,
                    "diff_id": layer.diff_id,
                }
                for i, layer in enumerate(self.layers, 1)
            ],
        }


@dataclasses.dataclass(frozen=True)
class Options:
    """How a build runs: where the image goes and where its inputs come from."""

    out: Path
    platform: Platform
    cache: Path
    tag: str | None = None
    lock: Path | None = None
    runtime: Runtime | None = None  # absent: python:3.14-slim from its registry
    uv_options: tuple[str, ...] = ()
    version: str | None = None  # the Moruna that built it, for the config's labels


def image_config(
    runtime: Runtime, function: Function, layers: Sequence[LayerInfo], options: Options
) -> dict[str, Any]:
    """The image config: the runtime's environment with this image's added, the shim as the
    command with an explicit shim workdir, its initial directory ``/``, and its origin."""
    ours = {
        "PYTHONPATH": f"/{APP_DIR}:/{SITE_PACKAGES}",
        "PYTHONDONTWRITEBYTECODE": "1",
    }
    env = [
        e for e in runtime.config.get("config", {}).get("Env", []) if e.split("=", 1)[0] not in ours
    ]
    env += [f"{k}={v}" for k, v in ours.items()]
    labels = {
        "org.opencontainers.image.base.name": runtime.name,
        "org.opencontainers.image.base.digest": runtime.digest,
        "org.moruna.entry": function.entry,
        "org.moruna.stateful": "true" if function.stateful else "false",
    }
    if function.fingerprint:
        labels["org.moruna.fingerprint"] = function.fingerprint
    if options.version:
        labels["org.moruna.version"] = options.version
    return {
        "architecture": options.platform.arch,
        "os": "linux",
        "config": {
            "Env": env,
            "Cmd": [PYTHON, f"/{SHIM_DIR}/shim.py", "--workdir", f"/{APP_DIR}", function.entry],
            "WorkingDir": "/",
            "Labels": labels,
        },
        "rootfs": {"type": "layers", "diff_ids": [layer.diff_id for layer in layers]},
        "history": [
            {"created_by": f"moruna build: {layer.role}", "comment": layer.blob.digest}
            for layer in layers
        ],
    }


def _put(layout: Path, digest: str, source: Path | bytes, size: int | None = None) -> None:
    """``source`` as the blob ``digest`` of the layout, hashed as it is copied; a blob already
    there that hashes right is left alone."""
    hex_ = _digest_hex(digest, "blob")
    dest = layout / "blobs" / "sha256" / hex_
    if dest.is_file() and sha256_file(dest) == hex_:
        return
    dest.parent.mkdir(parents=True, exist_ok=True)
    fd, temp = tempfile.mkstemp(dir=dest.parent, prefix=".part-")
    hasher = hashlib.sha256()
    try:
        with os.fdopen(fd, "wb") as out:
            if isinstance(source, bytes):
                hasher.update(source)
                out.write(source)
            else:
                with source.open("rb") as handle:
                    for chunk in iter(lambda: handle.read(1 << 20), b""):
                        hasher.update(chunk)
                        out.write(chunk)
        if hasher.hexdigest() != hex_ or (size is not None and os.path.getsize(temp) != size):
            raise Refused(f"{digest}: the bytes to write hash to sha256:{hasher.hexdigest()}")
        os.replace(temp, dest)
    finally:
        if os.path.exists(temp):
            os.unlink(temp)


def _prepare_layout(layout: Path) -> dict[str, Any]:
    """The index of ``layout``, made if it is new; a directory that is not one is refused."""
    marker = layout / "oci-layout"
    if not layout.exists() or not any(layout.iterdir()):
        layout.mkdir(parents=True, exist_ok=True)
        marker.write_bytes(canonical({"imageLayoutVersion": "1.0.0"}))
        index = {"schemaVersion": 2, "mediaType": MEDIA_INDEX, "manifests": []}
        _write_atomic(layout / "index.json", canonical(index))
        return index
    try:
        if json.loads(marker.read_bytes()).get("imageLayoutVersion") != "1.0.0":
            raise Refused(f"{layout}: an OCI layout of a version other than 1.0.0")
        index = json.loads((layout / "index.json").read_bytes())
    except (OSError, ValueError) as exc:
        raise Refused(f"--out {layout}: holds files and is not an OCI layout ({exc})") from exc
    index.setdefault("manifests", [])
    return index


def _write_atomic(path: Path, data: bytes) -> None:
    temp = path.with_name(f".{path.name}.part")
    temp.write_bytes(data)
    os.replace(temp, path)


def build_image(function: Function, options: Options) -> Built:
    """Build the four layers of ``function`` into the OCI layout ``options.out`` and add the
    image to its ``index.json`` under ``options.tag`` (an image of that name already there is
    replaced). Blobs already in the layout are shared, not written again."""
    lock = find_lock(function.root, options.lock)
    uv = uv_command()
    pins = with_pyarrow(parse_lock(lock_text(lock, uv), str(lock)), str(lock))
    runtime = options.runtime or fetch_runtime(options.platform, options.cache)
    tools = tag_interpreter(runtime_bytecode(runtime))
    reference = options.tag or f"{function.name}:latest"
    with tempfile.TemporaryDirectory(prefix="moruna-build-") as scratch:
        work = Path(scratch)
        requirements = work / "requirements.txt"
        requirements.write_text(requirements_text(pins))
        site, shim, app = work / "site-packages", work / "shim", work / "app"
        site.mkdir()
        install(requirements, site, options.platform, options.uv_options, tools)
        stage_shim(shim)
        stage_function(function, app)
        compile_tree(site, f"/{SITE_PACKAGES}", tools)
        compile_tree(shim / "moruna", f"/{SHIM_DIR}/moruna", tools)
        compile_tree(app, f"/{APP_DIR}", tools)
        made = {
            "dependencies": write_layer(tree_entries(site, SITE_PACKAGES), work / "layers"),
            "shim": write_layer(tree_entries(shim, SHIM_DIR), work / "layers"),
            "function": write_layer(tree_entries(app, APP_DIR), work / "layers"),
        }
        layers = [_runtime_layer(runtime, layer, i) for i, layer in enumerate(runtime.layers)]
        layers += [LayerInfo(role, blob, MEDIA_LAYER, blob.digest) for role, blob in made.items()]
        config = canonical(image_config(runtime, function, layers, options))
        config_digest = f"sha256:{hashlib.sha256(config).hexdigest()}"
        manifest = canonical(
            {
                "schemaVersion": 2,
                "mediaType": MEDIA_MANIFEST,
                "config": {"mediaType": MEDIA_CONFIG, "digest": config_digest, "size": len(config)},
                "layers": [
                    {"mediaType": l.media_type, "digest": l.blob.digest, "size": l.blob.size}
                    for l in layers  # noqa: E741
                ],
            }
        )
        manifest_digest = f"sha256:{hashlib.sha256(manifest).hexdigest()}"
        index = _prepare_layout(options.out)
        for layer in layers:
            source = (
                runtime.files[layer.blob.digest] if layer.role == "runtime" else layer.blob.path
            )
            _put(options.out, layer.blob.digest, source, layer.blob.size)
        _put(options.out, config_digest, config)
        _put(options.out, manifest_digest, manifest)
        index["manifests"] = sorted(
            [m for m in index["manifests"] if m.get("annotations", {}).get(REF_NAME) != reference]
            + [
                {
                    "mediaType": MEDIA_MANIFEST,
                    "digest": manifest_digest,
                    "size": len(manifest),
                    "annotations": {REF_NAME: reference},
                }
            ],
            key=lambda m: (m.get("annotations", {}).get(REF_NAME, ""), m["digest"]),
        )
        _write_atomic(options.out / "index.json", canonical(index))
    return Built(options.out, reference, manifest_digest, tuple(layers), lock, options.platform)


def _runtime_layer(runtime: Runtime, descriptor: dict[str, Any], position: int) -> LayerInfo:
    diff_ids = runtime.config.get("rootfs", {}).get("diff_ids", [])
    if len(diff_ids) != len(runtime.layers):
        raise Refused(
            f"{runtime.name}: its config lists {len(diff_ids)} diff ids for "
            f"{len(runtime.layers)} layers"
        )
    blob = Blob(descriptor["digest"], descriptor["size"], runtime.files[descriptor["digest"]])
    return LayerInfo("runtime", blob, descriptor["mediaType"], diff_ids[position])


# ---------------------------------------------------------------------------------------------
# The kernel a build is asked for


def function_of(target: Any, name: str | None = None) -> Function:
    """The ``Function`` of a decorated kernel, or of a plain function or class instance, found in
    the module that defines it (the build imports it by that name, as the shim will)."""
    inner = getattr(target, "wrapped", target)
    while hasattr(inner, "__wrapped__"):
        inner = inner.__wrapped__
    module_name = getattr(inner, "__module__", None) or type(inner).__module__
    module = sys.modules.get(module_name)
    if module is None or getattr(module, "__file__", None) is None:
        raise Refused(
            f"{module_name}: the module that defines the kernel is not a file that can be imported"
        )
    if name is None:
        names = [n for n, v in vars(module).items() if v is target]
        if not names:
            raise Refused(
                f"{module_name} does not bind the kernel to a name the image could import; "
                "assign it at the top level of the module"
            )
        name = names[0]
    if module_name == "__main__":
        module_name = Path(module.__file__).stem
    return Function(
        module=module_name,
        name=name,
        source=Path(module.__file__),
        fingerprint=getattr(target, "fingerprint", None),
        stateful=bool(getattr(target, "stateful", False)),
    )


def default_cache() -> Path:
    base = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    return Path(base) / "moruna" / "build"


def build(  # noqa: PLR0913, the keywords are the command's options
    target: Any,
    *,
    name: str | None = None,
    out: str | os.PathLike[str] = "moruna-image",
    tag: str | None = None,
    platform: str | None = None,
    lock: str | os.PathLike[str] | None = None,
    files: Iterable[str | os.PathLike[str]] = (),
    runtime_layout: str | os.PathLike[str] | None = None,
    runtime_reference: str | None = None,
    cache: str | os.PathLike[str] | None = None,
    index_url: str | None = None,
    find_links: Iterable[str] = (),
    no_index: bool = False,
    version: str | None = None,
) -> Built:
    """Build the kernel ``target`` (a decorated function, a function or a stateful instance
    bound to a name of the module that defines it; ``name`` says which when the module binds it
    twice) as an image: see ``build_image``. The keywords are ``moruna build``'s options."""
    function = dataclasses.replace(
        function_of(target, name), files=tuple(Path(f).resolve() for f in files)
    )
    chosen = parse_platform(platform)
    runtime = None
    if runtime_layout is not None:
        if runtime_reference is None:
            raise Refused("runtime_layout needs runtime_reference: the image of it to use")
        runtime = read_runtime_layout(Path(runtime_layout), runtime_reference, chosen)
    uv_options: list[str] = []
    if index_url:
        uv_options += ["--index-url", index_url]
    for link in find_links:
        uv_options += ["--find-links", link]
    if no_index:
        uv_options.append("--no-index")
    return build_image(
        function,
        Options(
            out=Path(out).absolute(),
            platform=chosen,
            cache=Path(cache) if cache else default_cache(),
            tag=tag,
            lock=Path(lock) if lock else None,
            runtime=runtime,
            uv_options=tuple(uv_options),
            version=version,
        ),
    )


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="moruna build",
        description="Build a Python kernel as an OCI image of four layers (runtime, locked "
        "dependencies, shim, function) that the image kind runs.",
    )
    add = parser.add_argument
    add("kernel", help="<module>:<function>; the module is a dotted name or a .py path")
    add("--out", default="moruna-image", help="the OCI layout directory (default: moruna-image)")
    add("--tag", help="the image's name in the layout (default: <function>:latest)")
    add("--platform", help="linux/aarch64 or linux/x86_64 (default: this Linux's, or aarch64)")
    add("--lock", help="uv.lock or requirements.lock (default: the nearest above the module)")
    add("--file", action="append", default=[], help="a file the function names; repeatable")
    add("--runtime-layout", help="an OCI layout holding the runtime, in place of python:3.14-slim")
    add("--runtime-reference", help="the image of --runtime-layout: a digest or a name")
    add("--cache", help="where the runtime's blobs are kept (default: ~/.cache/moruna/build)")
    add("--index-url", help="passed to uv pip install")
    add("--find-links", action="append", default=[], help="passed to uv pip install")
    add("--no-index", action="store_true", help="passed to uv pip install")
    add("--json", action="store_true", help="print the report as JSON")
    return parser


def main(argv: list[str] | None = None) -> int:
    """``moruna build <module>:<function>``: the exit status, 2 for a refusal."""
    args = _parser().parse_args(argv)
    module_name, _, name = args.kernel.partition(":")
    if not module_name or not name:
        print(
            f"moruna build: {args.kernel}: name the kernel as <module>:<function>", file=sys.stderr
        )
        return EXIT_REFUSED
    from moruna import _check, _core  # noqa: PLC0415, only the engine's package loads a module

    try:
        try:
            module = _check._load(module_name)
        except Exception as exc:  # noqa: BLE001, a module that does not load is a refusal
            raise Refused(f"{module_name}: {type(exc).__name__}: {exc}") from exc
        if not hasattr(module, name):
            raise Refused(f"{module_name} has no {name}")
        built = build(
            getattr(module, name),
            name=name,
            out=args.out,
            tag=args.tag,
            platform=args.platform,
            lock=args.lock,
            files=args.file,
            runtime_layout=args.runtime_layout,
            runtime_reference=args.runtime_reference,
            cache=args.cache,
            index_url=args.index_url,
            find_links=args.find_links,
            no_index=args.no_index,
            version=_core.__version__,
        )
    except Refused as exc:
        print(f"moruna build: {exc}", file=sys.stderr)
        return EXIT_REFUSED
    if args.json:
        print(json.dumps(built.report(), indent=2))
    else:
        print_report(built)
    return EXIT_DONE


def print_report(built: Built) -> None:
    """The image's digest and each layer's digest and size, one line each."""
    print(f"image     {built.digest}")
    print(f"reference {built.reference}")
    print(f"layout    {built.layout}")
    print(f"platform  {built.platform.name}")
    print(f"lock      {built.lock}")
    for position, layer in enumerate(built.layers, 1):
        print(f"layer {position}   {layer.role:<12} {layer.blob.digest} {layer.blob.size}")


if __name__ == "__main__":
    sys.exit(main())
