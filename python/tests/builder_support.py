"""What the builder's tests are made of (SDD 30 k): modules loaded by path, so that they run with
no engine built; wheels, locks and a runtime layout small enough to build a hundred times.

Nothing here touches the network. ``uv`` installs the hand-made wheels from a directory
(``--no-index --find-links``), for a platform other than the machine's, which is the same code
path a build for ``linux/aarch64`` from a Mac takes.
"""

from __future__ import annotations

import base64
import gzip
import hashlib
import importlib.util
import io
import json
import sys
import tarfile
import zipfile
from pathlib import Path
from types import ModuleType


def _package() -> Path:
    """The directory of the ``moruna`` package whose builder is under test: the tree this file
    is in, or, when the tests are run from a copy (against an installed wheel), the installed
    package's, found without importing it."""
    beside = Path(__file__).resolve().parents[1] / "moruna"
    if (beside / "_build.py").is_file():
        return beside
    spec = importlib.util.find_spec("moruna")
    assert spec is not None and spec.origin is not None, (
        "moruna is neither beside the tests nor installed"
    )
    return Path(spec.origin).parent


PACKAGE = _package()

ARCH = {"linux/aarch64": "aarch64", "linux/x86_64": "x86_64"}
OCI_ARCH = {"linux/aarch64": "arm64", "linux/x86_64": "amd64"}


#: The package the builder's modules are loaded in, so that they import one another as they do
#: in ``moruna`` (``from . import _closure``) without ``moruna`` itself, and its engine, loading.
BUILDER_PACKAGE = "moruna_builder"


def load(filename: str, name: str) -> ModuleType:
    """``python/moruna/<filename>`` as a module of the package ``moruna_builder`` (also known as
    ``name``), without importing ``moruna``."""
    if BUILDER_PACKAGE not in sys.modules:
        package = ModuleType(BUILDER_PACKAGE)
        package.__path__ = [str(PACKAGE)]  # type: ignore[attr-defined]
        sys.modules[BUILDER_PACKAGE] = package
    module = importlib.import_module(f"{BUILDER_PACKAGE}.{filename.removesuffix('.py')}")
    sys.modules[name] = module
    return module


def load_path(path: Path, name: str) -> ModuleType:
    """A user's module from its file, registered so that the builder can find its file."""
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def make_wheel(
    directory: Path,
    name: str,
    version: str,
    files: dict[str, bytes] | None = None,
    tag: str = "py3-none-any",
) -> Path:
    """A wheel of one importable package ``name`` holding ``files`` (path to bytes, under the
    package); the file's path."""
    directory.mkdir(parents=True, exist_ok=True)
    pure = tag.endswith("none-any")
    members = {f"{name}/__init__.py": f'VERSION = "{version}"\n'.encode()}
    for path, data in (files or {}).items():
        members[f"{name}/{path}"] = data
    dist = f"{name}-{version}.dist-info"
    members[f"{dist}/METADATA"] = (
        f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n".encode()
    )
    members[f"{dist}/WHEEL"] = (
        f"Wheel-Version: 1.0\nGenerator: moruna-tests\nRoot-Is-Purelib: {str(pure).lower()}\n"
        f"Tag: {tag}\n"
    ).encode()
    record = []
    for path, data in members.items():
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
        record.append(f"{path},sha256={digest},{len(data)}")
    record.append(f"{dist}/RECORD,,")
    members[f"{dist}/RECORD"] = ("\n".join(record) + "\n").encode()
    wheel = directory / f"{name}-{version}-{tag}.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        for path, data in members.items():
            archive.writestr(path, data)
    return wheel


def make_sdist(directory: Path, name: str, version: str, marker: Path) -> Path:
    """A source distribution whose ``setup.py`` writes ``marker`` if anything ever runs it."""
    directory.mkdir(parents=True, exist_ok=True)
    root = f"{name}-{version}"
    files = {
        f"{root}/PKG-INFO": f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n",
        f"{root}/setup.py": (
            "from setuptools import setup\n"
            f"open({str(marker)!r}, 'w').write('the package ran')\n"
            f"setup(name={name!r}, version={version!r}, py_modules=[])\n"
        ),
        f"{root}/pyproject.toml": (
            '[build-system]\nrequires = ["setuptools"]\nbuild-backend = "setuptools.build_meta"\n'
        ),
    }
    sdist = directory / f"{root}.tar.gz"
    with tarfile.open(sdist, "w:gz") as archive:
        for path, text in files.items():
            data = text.encode()
            info = tarfile.TarInfo(path)
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
    return sdist


def hashed(wheel: Path) -> str:
    return f"--hash=sha256:{sha256(wheel.read_bytes())}"


def lock_line(name: str, version: str, wheels: list[Path], marker: str | None = None) -> str:
    head = f"{name}=={version}" + (f" ; {marker}" if marker else "")
    return head + " \\\n" + " \\\n".join(f"    {hashed(w)}" for w in wheels) + "\n"


def platform_wheels(directory: Path, name: str, version: str) -> list[Path]:
    """The wheel of ``name`` for each platform an image has, with content that differs by
    platform (as a compiled extension's does), so a layer built for one is not the other's."""
    return [
        make_wheel(
            directory,
            name,
            version,
            {"native.so": f"{name} for {arch}".encode()},
            f"cp314-cp314-manylinux_2_28_{arch}",
        )
        for arch in ("aarch64", "x86_64")
    ]


def tar_of(files: dict[str, bytes]) -> bytes:
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as archive:
        for path, data in files.items():
            info = tarfile.TarInfo(path)
            info.size = len(data)
            info.mode = 0o755
            archive.addfile(info, io.BytesIO(data))
    return buffer.getvalue()


def this_magic() -> int:
    """The magic number of the bytecode this interpreter writes."""
    return int.from_bytes(importlib.util.MAGIC_NUMBER[:2], "little")


def make_runtime(  # noqa: PLR0913, a runtime is described by what the builder asks of it
    directory: Path,
    platform: str = "linux/aarch64",
    ref: str = "python-test",
    version: str | None = "3.14.99",
    pyc_magic: int | None = None,
) -> Path:
    """An OCI layout of a one-layer image standing for ``python:3.14-slim``: its layer is a
    gzip tar holding ``usr/local/bin/python3``, its config names an environment and the
    architecture. The layout's directory."""
    blobs = directory / "blobs" / "sha256"
    blobs.mkdir(parents=True)
    files = {"usr/local/bin/python3": b"#!test python\n", "etc/os-release": b"test\n"}
    if pyc_magic is not None:
        pyc = pyc_magic.to_bytes(2, "little") + b"\r\n" + bytes(12)
        files["usr/local/lib/python3.14/__pycache__/os.cpython-314.pyc"] = pyc
    tar = tar_of(files)
    layer = gzip.compress(tar, mtime=0)
    config = json.dumps(
        {
            "architecture": OCI_ARCH[platform],
            "os": "linux",
            "config": {
                "Env": [
                    "PATH=/usr/local/bin:/usr/bin",
                    "LANG=C.UTF-8",
                    *([f"PYTHON_VERSION={version}"] if version else []),
                ],
                "Cmd": ["python3"],
            },
            "rootfs": {"type": "layers", "diff_ids": [f"sha256:{sha256(tar)}"]},
        }
    ).encode()
    manifest = json.dumps(
        {
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": f"sha256:{sha256(config)}",
                "size": len(config),
            },
            "layers": [
                {
                    "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": f"sha256:{sha256(layer)}",
                    "size": len(layer),
                }
            ],
        }
    ).encode()
    for data in (layer, config, manifest):
        (blobs / sha256(data)).write_bytes(data)
    (directory / "oci-layout").write_text('{"imageLayoutVersion":"1.0.0"}')
    (directory / "index.json").write_text(
        json.dumps(
            {
                "schemaVersion": 2,
                "manifests": [
                    {
                        "mediaType": "application/vnd.oci.image.manifest.v1+json",
                        "digest": f"sha256:{sha256(manifest)}",
                        "size": len(manifest),
                        "annotations": {"org.opencontainers.image.ref.name": ref},
                    }
                ],
            }
        )
    )
    return directory
