"""``moruna build``: a Python function as an OCI image of four layers (SDD 30).

The image is what the ``image`` kind runs on a Linux node (SDD 19) and the Moruna kernel runs
too (SDD 27, 28)::

    1. runtime       python:3.14-slim's own layers, by digest, copied and never rebuilt
    2. dependencies  the project's locked dependencies, installed for the image's platform
    3. shim          ``_shim.py``, the stand-in ``moruna`` module and ``_declare.py``
    4. function      the file that defines the function, and the files it names

Layers 2 to 4 are uncompressed tars written the same way every time (sorted entries, time 0,
owner 0:0, modes 0644 and 0755), so a layer's digest, the sha256 of its tar, depends only on what
is in it: two functions of one project share layers 1 to 3, and one function built twice is one
image, bit for bit. Nothing here imports the engine (``moruna._core``), so the builder runs, and
is tested, wherever a Python 3.14, ``pyarrow`` and ``uv`` are.
"""

from __future__ import annotations

import argparse
import base64
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

from . import _image_metadata

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


Transport = Callable[..., tuple[int, dict[str, str], IO[bytes]]]


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args: Any, **kwargs: Any) -> None:
        return None


def urllib_transport(
    url: str, headers: dict[str, str], method: str = "GET", body: bytes | Path | None = None
) -> tuple[int, dict[str, str], IO[bytes]]:
    """``method url`` with ``body`` (bytes, or a file's): the status, the headers (lower case
    names) and the body, redirects not followed (the registry follows them, dropping
    credentials that must not leave its host)."""
    file = body.open("rb") if isinstance(body, Path) else None
    data = file if file is not None else body
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    opener = urllib.request.build_opener(_NoRedirect)
    try:
        response = opener.open(request, timeout=600 if file is not None else 60)
    except urllib.error.HTTPError as exc:
        return exc.code, {k.lower(): v for k, v in exc.headers.items()}, exc
    except urllib.error.URLError as exc:
        raise Refused(f"{url}: {exc.reason}") from exc
    finally:
        if file is not None:
            file.close()
    return response.status, {k.lower(): v for k, v in response.headers.items()}, response


Credential = Callable[[str], "dict[str, str] | None"]


def http_scheme(host: str) -> str:
    """``https``, except for a registry on this machine (``localhost``, ``127.0.0.1``,
    ``[::1]``), which is spoken to in plain HTTP as registries for development are."""
    name = host.rsplit(":", 1)[0] if not host.startswith("[") else host.split("]")[0] + "]"
    return "http" if name in ("localhost", "127.0.0.1", "[::1]") else "https"


class Registry:
    """A client of one repository of an OCI registry (the distribution API) that verifies every
    byte against the digest it names. Anonymous unless the registry asks for credentials: then
    ``credential(host)`` is asked, once, for ``{"token": ...}`` (sent as a bearer token) or
    ``{"username": ..., "password": ...}`` (sent to the registry's token service, or as basic
    credentials when the registry asks for those); ``None`` keeps it anonymous."""

    def __init__(
        self,
        host: str,
        repository: str,
        transport: Transport | None = None,
        credential: Credential | None = None,
    ) -> None:
        self.host = host
        self.repository = repository
        self.transport = transport or urllib_transport
        self.credential = credential
        self.token: str | None = None
        self.basic: str | None = None
        self.base = f"{http_scheme(host)}://{host}/v2/{repository}"

    def _credential(self) -> dict[str, str] | None:
        if self.credential is None:
            return None
        found = self.credential(self.host)
        if found is not None and not (
            "token" in found or ("username" in found and "password" in found)
        ):
            raise Refused(
                f"{self.host}: the credential the resolver gave holds neither `token` nor "
                "`username` and `password`"
            )
        return found

    def _headers(self, url: str, headers: dict[str, str]) -> dict[str, str]:
        out = dict(headers)
        if urllib.parse.urlsplit(url).netloc == self.host:
            if self.token:
                out["Authorization"] = f"Bearer {self.token}"
            elif self.basic:
                out["Authorization"] = f"Basic {self.basic}"
        return out

    def request(
        self, method: str, url: str, headers: dict[str, str], body: bytes | Path | None = None
    ) -> tuple[int, dict[str, str], IO[bytes]]:
        """``method url`` with the registry's credentials once it asked for them, following
        its redirects (and dropping credentials for another host); the status, the headers and
        the body of the last answer."""
        challenges: set[str] = set()
        for _ in range(8):
            status, got, answer = self.transport(
                url, self._headers(url, headers), method=method, body=body
            )  # type: ignore[call-arg]
            if status in (301, 302, 303, 307, 308) and "location" in got:
                url = urllib.parse.urljoin(url, got["location"])
                continue
            if (
                status == http.HTTPStatus.UNAUTHORIZED
                and "www-authenticate" in got
                and got["www-authenticate"] not in challenges
            ):
                # HEAD can grant pull-only access; uploads may then need a
                # fresh token with push scope. Retry each challenge once.
                challenge = got["www-authenticate"]
                challenges.add(challenge)
                answer.close()
                self.token = self.basic = None
                self._authenticate(challenge)
                continue
            return status, got, answer
        raise Refused(f"{url}: too many redirects")

    def _open(self, url: str, accept: str) -> IO[bytes]:
        status, _, body = self.request("GET", url, {"Accept": accept})
        if status != http.HTTPStatus.OK:
            raise Refused(f"{url}: the registry answered {status}")
        return body

    def _authenticate(self, challenge: str) -> None:
        scheme = challenge.split(" ", 1)[0].lower()
        credential = self._credential()
        if scheme == "basic":
            if credential is None or "username" not in credential:
                raise Refused(
                    f"{self.host} asks for a username and a password, and the resolver gave "
                    f"none for the need `registry:{self.host}`"
                )
            pair = f"{credential['username']}:{credential['password']}".encode()
            self.basic = base64.b64encode(pair).decode()
            return
        if credential is not None and "token" in credential:
            self.token = credential["token"]
            return
        fields = dict(re.findall(r'(\w+)="([^"]*)"', challenge))
        if "realm" not in fields:
            raise Refused(f"{self.host}: cannot read the challenge {challenge!r}")
        query = {k: fields[k] for k in ("service", "scope") if k in fields}
        url = fields["realm"] + "?" + urllib.parse.urlencode(query)
        headers = {"Accept": "application/json"}
        if credential is not None:
            pair = f"{credential['username']}:{credential['password']}".encode()
            headers["Authorization"] = "Basic " + base64.b64encode(pair).decode()
        status, _, body = self.transport(url, headers)
        if status != http.HTTPStatus.OK:
            raise Refused(f"{url}: the token service answered {status}")
        document = json.loads(body.read())
        token = document.get("token") or document.get("access_token")
        if not token:
            raise Refused(f"{url}: the token service gave no token")
        self.token = str(token)

    def fetch(self, kind: str, digest: str, accept: str, dest: Path) -> Path:
        """``digest`` of ``kind`` (``manifests`` or ``blobs``) into ``dest``, whole and hashed
        as it comes; an existing ``dest`` that hashes to the digest is the answer."""
        wanted = _digest_hex(digest, self.repository)
        if dest.is_file() and sha256_file(dest) == wanted:
            return dest
        url = f"{self.base}/{kind}/{digest}"
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

    def has_blob(self, digest: str) -> bool:
        status, _, body = self.request("HEAD", f"{self.base}/blobs/{digest}", {})
        body.close()
        if status == http.HTTPStatus.OK:
            return True
        if status == http.HTTPStatus.NOT_FOUND:
            return False
        raise Refused(f"{self.base}/blobs/{digest}: the registry answered {status}")

    def push_blob(self, digest: str, source: Path) -> None:
        """The blob ``digest`` from ``source``, uploaded whole unless the registry has it."""
        if self.has_blob(digest):
            return
        status, got, body = self.request("POST", f"{self.base}/blobs/uploads/", {})
        body.close()
        if status != http.HTTPStatus.ACCEPTED or "location" not in got:
            raise Refused(f"{self.base}: starting an upload, the registry answered {status}")
        location = urllib.parse.urljoin(self.base + "/", got["location"])
        joiner = "&" if "?" in location else "?"
        url = f"{location}{joiner}digest={urllib.parse.quote(digest)}"
        headers = {
            "Content-Type": "application/octet-stream",
            "Content-Length": str(source.stat().st_size),
        }
        status, _, body = self.request("PUT", url, headers, source)
        body.close()
        if status != http.HTTPStatus.CREATED:
            raise Refused(f"{self.base}: uploading {digest}, the registry answered {status}")

    def push_manifest(self, reference: str, data: bytes, media_type: str) -> None:
        url = f"{self.base}/manifests/{reference}"
        headers = {"Content-Type": media_type, "Content-Length": str(len(data))}
        status, _, body = self.request("PUT", url, headers, data)
        body.close()
        if status not in (http.HTTPStatus.CREATED, http.HTTPStatus.OK):
            raise Refused(f"{url}: the registry answered {status}")


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
# The shim


SHIM_PARTS = {
    "shim.py": "_shim.py",
    "moruna/__init__.py": "_standin.py",
    "moruna/_declare.py": "_declare.py",
}


# What every image's process is given beside the runtime's environment: a home on the one place it
# may write (the sandbox's /tmp), and no user site-packages. Without a home, CPython's start looks
# the user up, glibc asks nscd over a Unix socket, and the image filter kills the process for the
# socket before it has written a byte (0.6.3: every Python image died at its start, SIGSYS).
IMAGE_ENV = {"PYTHONDONTWRITEBYTECODE": "1", "PYTHONNOUSERSITE": "1", "HOME": "/tmp"}


def shim_digest() -> str:
    """The shim's version: ``sha256:`` over its files by path and the environment every image is
    given (``IMAGE_ENV``), which every image of this Moruna holds the same and every build key
    names: a change to either is a new key, so no image built before it is reused."""
    here = Path(__file__).parent
    digest = hashlib.sha256()
    for target, source in sorted(SHIM_PARTS.items()):
        data = (here / source).read_bytes()
        digest.update(f"{target}\0{len(data)}\0".encode())
        digest.update(data)
    digest.update(json.dumps(IMAGE_ENV, sort_keys=True).encode())
    return "sha256:" + digest.hexdigest()


def stage_shim(dest: Path) -> None:
    """The shim layer's files written under ``dest`` as they are at ``/opt/moruna``: the shim,
    the stand-in ``moruna`` and the module it reads."""
    here = Path(__file__).parent
    for target, source in SHIM_PARTS.items():
        path = dest / target
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes((here / source).read_bytes())
        path.chmod(FILE_MODE)


# ---------------------------------------------------------------------------------------------
# The dependencies, one layer per distribution


_REQUIRES = re.compile(r"^Requires-Dist:\s*([A-Za-z0-9][A-Za-z0-9._-]*)\s*(.*)$", re.M)


def lock_digest(lock: Path) -> str:
    return "sha256:" + sha256_file(lock)


@dataclasses.dataclass(frozen=True)
class Site:
    """Every pin of a lock installed for a platform, compiled for the runtime, and which files
    each distribution owns; kept in the build cache by the lock's digest and the platform."""

    root: Path
    owners: dict[str, tuple[str, ...]]  # distribution -> its files, relative to the root
    requires: dict[str, tuple[str, ...]]  # distribution -> the distributions it requires


def _dist_of(dist_info: str) -> str:
    return normal(dist_info.removesuffix(".dist-info").rsplit("-", 1)[0])


def install_site(
    lock: Path, pins: list[Pin], platform: Platform, tools: Toolchain, options: Options
) -> Site:
    """The lock installed into the build cache once per lock and platform: uv installs every
    pin (wheels only, hashes required), the tree is compiled for the runtime, and each
    distribution's files are read from its ``RECORD``. A file no distribution owns is refused."""
    name = f"{sha256_file(lock)[:24]}-{platform.arch}"
    root = options.cache / "sites" / name
    index = root / ".moruna-site.json"
    if index.is_file():
        known = json.loads(index.read_bytes())
        return Site(
            root / "site",
            {k: tuple(v) for k, v in known["owners"].items()},
            {k: tuple(v) for k, v in known["requires"].items()},
        )
    work = root.with_name(f".{name}.part")
    shutil.rmtree(work, ignore_errors=True)
    site = work / "site"
    site.mkdir(parents=True)
    requirements = work / "requirements.txt"
    requirements.write_text(requirements_text(pins))
    install(requirements, site, platform, options.uv_options, tools)
    compile_tree(site, f"/{SITE_PACKAGES}", tools)
    owners: dict[str, list[str]] = {}
    requires: dict[str, tuple[str, ...]] = {}
    for record in sorted(site.glob("*.dist-info/RECORD")):
        dist = _dist_of(record.parent.name)
        files: list[str] = []
        for line in record.read_text(encoding="utf-8").splitlines():
            path = line.rsplit(",", 2)[0] if line.count(",") >= 2 else line.split(",")[0]
            if not path or path.startswith(".."):
                continue
            if (site / path).is_file():
                files.append(path)
                if path.endswith(".py"):
                    source = Path(path)
                    for pyc in sorted((site / source.parent / "__pycache__").glob(f"{source.stem}.*.pyc")):
                        files.append(pyc.relative_to(site).as_posix())
        owners[dist] = sorted(set(files))
        metadata = (record.parent / "METADATA").read_text(encoding="utf-8", errors="replace")
        requires[dist] = tuple(
            sorted(
                {
                    normal(m.group(1))
                    for m in _REQUIRES.finditer(metadata)
                    if "extra" not in m.group(2)
                }
            )
        )
    owned = {f for files in owners.values() for f in files}
    stray = sorted(
        p.relative_to(site).as_posix()
        for p in site.rglob("*")
        if p.is_file() and p.relative_to(site).as_posix() not in owned
    )
    if stray:
        raise Refused(
            f"{lock}: installed files no distribution's RECORD names, which no layer would "
            f"hold: {', '.join(stray[:5])}{' ...' if len(stray) > 5 else ''}"
        )
    _write_atomic(
        work / ".moruna-site.json",
        canonical({"owners": owners, "requires": {k: list(v) for k, v in requires.items()}}),
    )
    shutil.rmtree(root, ignore_errors=True)
    os.replace(work, root)
    return Site(root / "site", {k: tuple(v) for k, v in owners.items()}, requires)


def needed(site: Site, direct: Iterable[str]) -> list[str]:
    """``direct`` and every distribution it requires, transitively, as the lock installed them
    (a requirement whose marker excluded it here is not installed, and is not followed)."""
    wanted: set[str] = set()
    pending = [normal(d) for d in direct]
    while pending:
        dist = pending.pop()
        if dist in wanted or dist not in site.owners:
            continue
        wanted.add(dist)
        pending.extend(site.requires.get(dist, ()))
    return sorted(wanted)


def dist_layer(site: Site, dist: str, options: Options) -> Blob:
    """The layer of one distribution's files at ``/opt/moruna/site-packages``: the same bytes
    for the same files, written once into the build cache and shared by every image."""
    index = site.root.parent / ".moruna-layers.json"
    known = json.loads(index.read_bytes()) if index.is_file() else {}
    blobs = options.cache / "blobs" / "sha256"
    if dist in known:
        path = blobs / _digest_hex(known[dist]["digest"], dist)
        if path.is_file():
            return Blob(known[dist]["digest"], known[dist]["size"], path)
    entries = []
    for relative in site.owners[dist]:
        source = site.root / relative
        mode = EXEC_MODE if source.stat().st_mode & 0o111 else FILE_MODE
        entries.append(Entry(f"{SITE_PACKAGES}/{relative}", "file", mode, source=source))
    blob = write_layer(entries, blobs)
    known[dist] = {"digest": blob.digest, "size": blob.size}
    _write_atomic(index, canonical(known))
    return blob


# ---------------------------------------------------------------------------------------------
# A task's image


@dataclasses.dataclass(frozen=True)
class Task:
    """What a build needs to know of one task: its name in the job, its entry, its code (the
    closure) and its build key, and the distributions its closure imports."""

    name: str
    entry: str
    code: Any  # _closure.Code
    key: str
    distributions: tuple[str, ...]
    provenance: dict[str, Any]
    definition: dict[str, Any] | None = None


@dataclasses.dataclass(frozen=True)
class LayerInfo:
    role: str
    blob: Blob
    media_type: str
    diff_id: str


@dataclasses.dataclass(frozen=True)
class Built:
    """One task's image, built: where it is and what is in it."""

    task: str
    key: str
    digest: str
    layout: Path
    layers: tuple[LayerInfo, ...]
    reused: bool
    reference: str | None = None  # the registry reference, when it was pushed
    lock: Path | None = None  # the lock its dependencies were installed from

    @property
    def location(self) -> str:
        return self.reference or str(self.layout)

    def report(self) -> dict[str, Any]:
        return {
            "task": self.task,
            "build_key": self.key,
            "digest": self.digest,
            "location": self.location,
            "reused": self.reused,
            "layers": [
                {"role": l.role, "digest": l.blob.digest, "size_bytes": l.blob.size}  # noqa: E741
                for l in self.layers  # noqa: E741
            ],
        }


@dataclasses.dataclass(frozen=True)
class Options:
    """How a build runs: where the images go and where their inputs come from."""

    out: Path
    platform: Platform
    cache: Path
    lock: Path | None = None
    runtime: Runtime | None = None  # absent: python:3.14-slim from its registry
    uv_options: tuple[str, ...] = ()
    version: str | None = None  # the Moruna that built it, for the config's labels
    registry: str | None = None  # HOST/REPOSITORY to push every image to
    credential: Credential | None = None
    transport: Transport | None = None


def runtime_identity(runtime: Runtime | None) -> str:
    """What a build key names the runtime by: the pinned index, or a layout's manifest."""
    return RUNTIME_INDEX if runtime is None else runtime.digest


def image_config(
    runtime: Runtime, task: Task, layers: Sequence[LayerInfo], options: Options
) -> dict[str, Any]:
    """The image config: the runtime's environment with this image's added (its entry among
    it, so the image runs standalone too), the shim as the entrypoint with an explicit
    workdir, its initial directory ``/``, and its origin."""
    ours = {
        "PYTHONPATH": f"/{APP_DIR}:/{SITE_PACKAGES}",
        **IMAGE_ENV,
        "MORUNA_ENTRY": task.entry,
    }
    env = [
        e for e in runtime.config.get("config", {}).get("Env", []) if e.split("=", 1)[0] not in ours
    ]
    env += [f"{k}={v}" for k, v in ours.items()]
    labels = {
        "org.opencontainers.image.base.name": runtime.name,
        "org.opencontainers.image.base.digest": runtime.digest,
        BUILD_KEY: task.key,
    }
    labels.update(callable_labels(task, options))
    if options.version:
        labels["org.moruna.version"] = options.version
    return {
        "architecture": options.platform.arch,
        "os": "linux",
        "config": {
            "Env": env,
            "Entrypoint": [PYTHON, f"/{SHIM_DIR}/shim.py", "--workdir", f"/{APP_DIR}"],
            "WorkingDir": "/",
            "Labels": labels,
        },
        "rootfs": {"type": "layers", "diff_ids": [layer.diff_id for layer in layers]},
        "history": [
            {"created_by": f"moruna build: {layer.role}", "comment": layer.blob.digest}
            for layer in layers
        ],
    }


#: The manifest annotation that names an image's build key (`moruna_image::BUILD_KEY`).
BUILD_KEY = "org.moruna.build.key"


def callable_labels(task: Task, options: Options) -> dict[str, str]:
    try:
        return _image_metadata.labels(
            entry=task.entry,
            build_key=task.key,
            definition=task.definition,
            architecture=options.platform.arch,
        )
    except (TypeError, ValueError) as exc:
        raise Refused(f"{task.name}: invalid callable metadata: {exc}") from exc


def annotations(task: Task, lock: str) -> dict[str, str]:
    """The manifest's provenance, as standard OCI annotations and Moruna's own: the source and
    revision (when the code is in a repository) and whether a bundled file differed from it,
    the entry, the build key, the lock's digest, and each bundled file with its sha256."""
    found = {
        BUILD_KEY: task.key,
        "org.moruna.entry": task.entry,
        "org.moruna.lock.digest": lock,
        "org.moruna.files": json.dumps(
            task.code.file_digests(), sort_keys=True, separators=(",", ":")
        ),
    }
    if task.definition is not None:
        found[_image_metadata.VERSION_LABEL] = _image_metadata.VERSION
    if task.provenance.get("source"):
        found["org.opencontainers.image.source"] = task.provenance["source"]
    if task.provenance.get("revision"):
        found["org.opencontainers.image.revision"] = task.provenance["revision"]
        found["org.moruna.source.dirty"] = "true" if task.provenance.get("dirty") else "false"
    return found


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
        raise Refused(f"out {layout}: holds files and is not an OCI layout ({exc})") from exc
    index.setdefault("manifests", [])
    return index


def _write_atomic(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_name(f".{path.name}.part")
    temp.write_bytes(data)
    os.replace(temp, path)


def _metadata_matches(layout: Path, manifest: dict[str, Any], metadata: dict[str, str]) -> bool:
    """A cached config is verified before its passive descriptor is reused."""
    digest = manifest["config"]["digest"]
    config_path = layout / "blobs" / "sha256" / _digest_hex(digest, "callable config")
    try:
        if sha256_file(config_path) != _digest_hex(digest, "callable config"):
            return False
        config = json.loads(config_path.read_bytes())
        labels = config.get("config", {}).get("Labels", {})
    except (OSError, ValueError, AttributeError):
        return False
    keys = (_image_metadata.LABEL, _image_metadata.VERSION_LABEL)
    return all(labels.get(key) == metadata.get(key) for key in keys) and (
        manifest.get("annotations", {}).get(_image_metadata.VERSION_LABEL)
        == metadata.get(_image_metadata.VERSION_LABEL)
    )


def _existing(layout: Path, index: dict[str, Any], key: str,
              metadata: dict[str, str] | None = None) -> str | None:
    """The image of build key ``key`` the layout already holds, whole: its manifest's digest.
    Reuse also requires the current callable metadata, so older cached images
    without a descriptor cannot masquerade as newly described builds."""
    for entry in index.get("manifests", []):
        if entry.get("annotations", {}).get(REF_NAME) != key:
            continue
        digest = entry["digest"]
        manifest_path = layout / "blobs" / "sha256" / _digest_hex(digest, key)
        try:
            manifest = json.loads(manifest_path.read_bytes())
        except (OSError, ValueError):
            return None
        if sha256_file(manifest_path) != _digest_hex(digest, key):
            return None
        blobs = [manifest["config"], *manifest["layers"]]

        def whole(descriptor: dict[str, Any]) -> bool:
            path = layout / "blobs" / "sha256" / _digest_hex(descriptor["digest"], key)
            return path.is_file() and path.stat().st_size == descriptor["size"]

        if manifest.get("annotations", {}).get(BUILD_KEY) == key and all(map(whole, blobs)):
            if metadata is not None and not _metadata_matches(layout, manifest, metadata):
                return None
            return str(digest)
    return None


def _function_layer(task: Task, work: Path, tools: Toolchain) -> Blob:
    """The task's own layer at ``/app``: its closure and its data files, and the bytecode of
    every module beside its source."""
    app = work / f"app-{task.key.split(':')[1][:16]}"
    shutil.rmtree(app, ignore_errors=True)
    app.mkdir(parents=True)
    for relative, source in task.code.files.items():
        path = app / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(source.read_bytes())
        path.chmod(EXEC_MODE if source.stat().st_mode & 0o111 else FILE_MODE)
    compile_tree(app, f"/{APP_DIR}", tools)
    return write_layer(tree_entries(app, APP_DIR), work / "layers")


def build_images(tasks: Sequence[Task], options: Options) -> list[Built]:
    """One image per task into the OCI layout ``options.out``, each named there by its build
    key: the runtime's layers, one layer per distribution the task's code needs, the shim, and
    the task's code. Blobs are content addressed, so tasks that share a distribution share its
    layer and every blob is stored once; an image the layout already holds for a key is
    reused. With ``options.registry``, every image is then pushed there."""
    if not tasks:
        return []
    root = tasks[0].code.root
    lock = find_lock(root, options.lock)
    uv = uv_command()
    pins = with_pyarrow(parse_lock(lock_text(lock, uv), str(lock)), str(lock))
    runtime = options.runtime or fetch_runtime(options.platform, options.cache)
    tools = tag_interpreter(runtime_bytecode(runtime))
    index = _prepare_layout(options.out)
    built: list[Built] = []
    site: Site | None = None
    with tempfile.TemporaryDirectory(prefix="moruna-build-") as scratch:
        work = Path(scratch)
        shim_dir = work / "shim"
        stage_shim(shim_dir)
        compile_tree(shim_dir / "moruna", f"/{SHIM_DIR}/moruna", tools)
        shim = write_layer(tree_entries(shim_dir, SHIM_DIR), work / "layers")
        runtime_layers = [_runtime_layer(runtime, layer, i) for i, layer in enumerate(runtime.layers)]
        for task in tasks:
            if task.code.root != root:
                raise Refused(
                    f"{task.name}: its code is imported from {task.code.root}, and the job's "
                    f"other tasks from {root}; a job's tasks build from one project and its lock"
                )
            existing = _existing(options.out, index, task.key, callable_labels(task, options))
            if existing is not None:
                manifest = json.loads(
                    (options.out / "blobs" / "sha256" / _digest_hex(existing, task.key)).read_bytes()
                )
                layers = tuple(
                    LayerInfo("reused", Blob(l["digest"], l["size"], options.out), l["mediaType"], "")  # noqa: E741
                    for l in manifest["layers"]  # noqa: E741
                )
                built.append(
                    Built(task.name, task.key, existing, options.out, layers, True, lock=lock)
                )
                continue
            if site is None:
                site = install_site(lock, pins, options.platform, tools, options)
            dists = needed(site, [*task.distributions, "pyarrow"])
            missing = sorted({normal(d) for d in task.distributions} - set(site.owners))
            if missing:
                raise Refused(
                    f"{task.name}: its code imports {', '.join(missing)}, which {lock} does not "
                    "pin"
                )
            layers = list(runtime_layers)
            for dist in dists:
                blob = dist_layer(site, dist, options)
                layers.append(LayerInfo(f"dependency {dist}", blob, MEDIA_LAYER, blob.digest))
            layers.append(LayerInfo("shim", shim, MEDIA_LAYER, shim.digest))
            own = _function_layer(task, work, tools)
            layers.append(LayerInfo("function", own, MEDIA_LAYER, own.digest))
            config = canonical(image_config(runtime, task, layers, options))
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
                    "annotations": annotations(task, lock_digest(lock)),
                }
            )
            manifest_digest = f"sha256:{hashlib.sha256(manifest).hexdigest()}"
            for layer in layers:
                source = (
                    runtime.files[layer.blob.digest] if layer.role == "runtime" else layer.blob.path
                )
                _put(options.out, layer.blob.digest, source, layer.blob.size)
            _put(options.out, config_digest, config)
            _put(options.out, manifest_digest, manifest)
            index["manifests"] = sorted(
                [m for m in index["manifests"] if m.get("annotations", {}).get(REF_NAME) != task.key]
                + [
                    {
                        "mediaType": MEDIA_MANIFEST,
                        "digest": manifest_digest,
                        "size": len(manifest),
                        "annotations": {REF_NAME: task.key, "org.moruna.task": task.name},
                    }
                ],
                key=lambda m: (m.get("annotations", {}).get(REF_NAME, ""), m["digest"]),
            )
            _write_atomic(options.out / "index.json", canonical(index))
            built.append(
                Built(
                    task.name, task.key, manifest_digest, options.out, tuple(layers), False,
                    lock=lock,
                )
            )
    if options.registry:
        built = [push(b, options) for b in built]
    return built


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
# Registries: push and pull


def split_registry(target: str) -> tuple[str, str]:
    """``HOST/REPOSITORY`` as its two parts; a target with no host is refused."""
    host, _, repository = target.partition("/")
    if not repository or not ("." in host or ":" in host or host == "localhost"):
        raise Refused(
            f"registry {target!r}: name it as HOST/REPOSITORY, the host with a `.` or a port, "
            "or `localhost`"
        )
    return host, repository


def push(built: Built, options: Options) -> Built:
    """``built``'s image pushed to ``options.registry``: every blob the registry lacks, then
    the manifest, by its digest and tagged with its build key (``sha256-<hex>``), so a registry
    that collects untagged manifests keeps it."""
    assert options.registry is not None
    host, repository = split_registry(options.registry)
    registry = Registry(host, repository, options.transport, options.credential)
    blobs = built.layout / "blobs" / "sha256"
    manifest = (blobs / _digest_hex(built.digest, built.task)).read_bytes()
    document = json.loads(manifest)
    for descriptor in [document["config"], *document["layers"]]:
        registry.push_blob(descriptor["digest"], blobs / _digest_hex(descriptor["digest"], built.task))
    registry.push_manifest(built.key.replace(":", "-"), manifest, MEDIA_MANIFEST)
    registry.push_manifest(built.digest, manifest, MEDIA_MANIFEST)
    return dataclasses.replace(built, reference=f"{host}/{repository}@{built.digest}")


def pull(reference: str, cache: Path, credential: Credential | None = None,
         transport: Transport | None = None) -> Path:
    """The image ``HOST/REPOSITORY@sha256:...`` pulled into the OCI layout ``cache`` (every
    blob verified against its digest, kept by digest, fetched once), named there by the
    reference; the layout."""
    path, _, digest = reference.partition("@")
    host, repository = split_registry(path)
    _digest_hex(digest, reference)
    registry = Registry(host, repository, transport, credential)
    index = _prepare_layout(cache)
    blobs = cache / "blobs" / "sha256"
    manifest_file = registry.fetch(
        "manifests", digest, f"{MEDIA_MANIFEST}, application/vnd.docker.distribution.manifest.v2+json",
        blobs / _digest_hex(digest, reference),
    )  # fmt: skip
    manifest = json.loads(manifest_file.read_bytes())
    if "layers" not in manifest:
        raise Refused(f"{reference}: names an index of images, not one image's manifest")
    for descriptor in [manifest["config"], *manifest["layers"]]:
        registry.fetch(
            "blobs", descriptor["digest"], "application/octet-stream",
            blobs / _digest_hex(descriptor["digest"], reference),
        )  # fmt: skip
    entry = {
        "mediaType": manifest.get("mediaType", MEDIA_MANIFEST),
        "digest": digest,
        "size": manifest_file.stat().st_size,
        "annotations": {REF_NAME: reference},
    }
    index["manifests"] = sorted(
        [m for m in index["manifests"] if m["digest"] != digest] + [entry],
        key=lambda m: m["digest"],
    )
    _write_atomic(cache / "index.json", canonical(index))
    return cache


def default_cache() -> Path:
    base = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    return Path(base) / "moruna" / "build"


def default_pulled() -> Path:
    base = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    return Path(base) / "moruna" / "images"


def options(  # noqa: PLR0913, the keywords are the command's options
    *,
    out: str | os.PathLike[str] = "moruna-images",
    platform: str | None = None,
    lock: str | os.PathLike[str] | None = None,
    runtime_layout: str | os.PathLike[str] | None = None,
    runtime_reference: str | None = None,
    cache: str | os.PathLike[str] | None = None,
    index_url: str | None = None,
    find_links: Iterable[str] = (),
    no_index: bool = False,
    version: str | None = None,
    registry: str | None = None,
    credential: Credential | None = None,
) -> Options:
    """A build's options from its keywords (``job.build`` and ``moruna build``)."""
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
    if registry is not None:
        split_registry(registry)
    return Options(
        out=Path(out).absolute(),
        platform=chosen,
        cache=Path(cache) if cache else default_cache(),
        lock=Path(lock) if lock else None,
        runtime=runtime,
        uv_options=tuple(uv_options),
        version=version,
        registry=registry,
        credential=credential,
    )


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="moruna build",
        description="Build one OCI image per task of a job, and write the job's runnable spec.",
    )
    add = parser.add_argument
    add("job", help="a job's spec (JOB.mjob), or <module>:<attribute> naming a moruna.Job")
    add("--out", default="moruna-images", help="the OCI layout (default: moruna-images)")
    add("--registry", help="HOST/REPOSITORY to push every image to")
    add("--platform", help="linux/aarch64 or linux/x86_64 (default: this Linux's, or aarch64)")
    add("--spec", help="where the runnable spec goes (default: JOB.mjob, or <attribute>.mjob)")
    add("--lock", help="uv.lock or requirements.lock (default: the nearest above the code)")
    add("--file", action="append", default=[], help="a data file every task reads; repeatable")
    add("--resolver", help="MODULE:ATTRIBUTE of the resolver that supplies registry credentials")
    add("--runtime-layout", help="an OCI layout holding the runtime, in place of python:3.14-slim")
    add("--runtime-reference", help="the image of --runtime-layout: a digest or a name")
    add("--cache", help="where the build keeps what it reuses (default: ~/.cache/moruna/build)")
    add("--index-url", help="passed to uv pip install")
    add("--find-links", action="append", default=[], help="passed to uv pip install")
    add("--no-index", action="store_true", help="passed to uv pip install")
    add("--json", action="store_true", help="print what was built as JSON")
    return parser


def main(argv: list[str] | None = None) -> int:
    """``moruna build JOB``: the exit status, 2 for a refusal."""
    args = _parser().parse_args(argv)
    from moruna import _job  # noqa: PLC0415, the command needs the engine to read a job

    try:
        status = _job.build_command(args)
    except Refused as exc:
        print(f"moruna build: {exc}", file=sys.stderr)
        return EXIT_REFUSED
    if args.json:
        print(json.dumps(status, indent=2))
    else:
        for row in status["tasks"]:
            print(
                f"{row['task']:<16} {row['state']:<9} {row['build_key']} "
                f"{row.get('digest') or '-'} {row.get('location') or '-'}"
            )
        print(f"spec      {status['spec']} {status['job_digest']}")
    return EXIT_DONE


if __name__ == "__main__":
    sys.exit(main())
