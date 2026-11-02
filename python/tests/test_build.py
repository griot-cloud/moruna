"""BD-T7 to BD-T24 (SDD 30 k): the builder, with no engine built and no network.

``uv`` installs hand-made wheels from a directory for a platform other than this machine's, a
tiny OCI layout stands in for ``python:3.14-slim`` and a fake registry serves the real code path
that fetches it, so what runs here is what a build runs, over inputs a test can change one at a
time.
"""

from __future__ import annotations

import gzip
import hashlib
import http.server
import importlib.util
import io
import json
import marshal
import os
import subprocess
import sys
import tarfile
import threading
import types
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import builder_support as support
import pytest

build = support.load("_build.py", "moruna_build")

AARCH64 = "linux/aarch64"
X86_64 = "linux/x86_64"


@dataclass
class Project:
    """A project directory with a lock, two kernels, and the wheels and runtime to build them."""

    root: Path
    wheels: Path
    runtimes: dict[str, Path]
    scratch: Path

    def module(self, name: str) -> Any:
        """``<name>.py`` of the project, loaded by path."""
        return support.load_path(self.root / f"{name}.py", name)

    def kernel(self, name: str) -> Any:
        """The function ``add`` of ``<name>.py`` in the project."""
        return self.module(name).add

    def build(self, target: Any, out: Path | None = None, **options: Any) -> Any:
        platform = options.pop("platform", AARCH64)
        options.setdefault("runtime_layout", self.runtimes[platform])
        options.setdefault("runtime_reference", "python-test")
        return build.build(
            target,
            out=out or self.scratch / "image",
            platform=platform,
            no_index=True,
            find_links=[str(self.wheels)],
            cache=self.scratch / "cache",
            version="test",
            **options,
        )

    def write_lock(self, *lines: str, name: str = "requirements.lock") -> Path:
        path = self.root / name
        path.write_text("".join(lines))
        return path


@pytest.fixture
def project(tmp_path: Path) -> Project:
    root = tmp_path / "project"
    root.mkdir()
    wheels = tmp_path / "wheels"
    pyarrow = support.platform_wheels(wheels, "pyarrow", "25.0.1")
    leaf = support.make_wheel(wheels, "leafdep", "1.0", {"data.txt": b"leaf"})
    win = support.make_wheel(wheels, "wincolor", "0.4")
    (root / "requirements.lock").write_text(
        "# a lock\n"
        + support.lock_line("leafdep", "1.0", [leaf])
        + support.lock_line("pyarrow", "25.0.1", pyarrow)
        + support.lock_line("wincolor", "0.4", [win], "sys_platform == 'win32'")
    )
    (root / "kernel_a.py").write_text("def add(batch):\n    return batch\n")
    (root / "kernel_b.py").write_text("def add(batch):\n    return batch.slice(1)\n")
    runtimes = {
        platform: support.make_runtime(tmp_path / f"runtime-{arch}", platform)
        for platform, arch in support.ARCH.items()
    }
    return Project(root, wheels, runtimes, tmp_path / "scratch")


def blob(layout: Path, digest: str) -> bytes:
    return (layout / "blobs" / "sha256" / digest.split(":")[1]).read_bytes()


def manifest_of(built: Any) -> dict[str, Any]:
    return json.loads(blob(built.layout, built.digest))  # type: ignore[no-any-return]


def tar_bytes(data: bytes) -> bytes:
    return gzip.decompress(data) if data[:2] == b"\x1f\x8b" else data


def unpack(built: Any, dest: Path) -> None:
    """The image's layers laid over one another, as an unpacking does (no whiteouts here)."""
    for layer in manifest_of(built)["layers"]:
        data = tar_bytes(blob(built.layout, layer["digest"]))
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            archive.extractall(dest, filter="tar")


def digests(built: Any) -> list[str]:
    return [layer.blob.digest for layer in built.layers]


def test_bd_t7_a_project_with_no_lock_is_refused(project: Project) -> None:
    """BD-T7 no_lock_no_build: nothing is resolved without a lock, and the refusal says a lock
    is needed, before the runtime is fetched or anything is installed."""
    (project.root / "requirements.lock").unlink()
    kernel = project.kernel("kernel_a")
    with pytest.raises(build.Refused, match=r"no lock: .*needs a uv\.lock or a requirements\.lock"):
        project.build(kernel)
    assert not (project.scratch / "image").exists(), "nothing was written"
    assert not (project.scratch / "cache").exists(), "nothing was fetched"


def test_bd_t7_two_locks_are_refused_and_one_can_be_named(project: Project) -> None:
    (project.root / "uv.lock").write_text("version = 1\n")
    with pytest.raises(build.Refused, match="holds both uv.lock and requirements.lock"):
        project.build(project.kernel("kernel_a"))
    built = project.build(project.kernel("kernel_a"), lock=project.root / "requirements.lock")
    assert built.lock == (project.root / "requirements.lock").resolve()
    with pytest.raises(build.Refused, match="no such file"):
        project.build(project.kernel("kernel_a"), lock=project.root / "absent.lock")


def test_bd_t7_the_nearest_lock_above_the_module_is_the_one(project: Project) -> None:
    sub = project.root / "deep" / "er"
    sub.mkdir(parents=True)
    (sub / "kernel_c.py").write_text("def add(batch):\n    return batch\n")
    module = support.load_path(sub / "kernel_c.py", "kernel_c")
    built = project.build(module.add)
    assert built.lock == (project.root / "requirements.lock").resolve()


@pytest.mark.parametrize(
    ("line", "said"),
    [
        ("requests>=2\n", "is not name==version"),
        ("requests==2.0\n", "needs --hash=sha256"),
        ("requests==2.0 --hash=sha256:abc\n", "needs --hash=sha256"),
        ("--index-url https://example.test/simple\n", "options are not read from a lock"),
        ("-e .\n", "is not name==version"),
    ],
)
def test_bd_t8_a_lock_that_pins_loosely_is_refused_naming_the_line(line: str, said: str) -> None:
    """BD-T8 a_lock_pins_every_requirement_with_a_hash."""
    with pytest.raises(build.Refused, match=said) as caught:
        build.parse_lock("a==1 \\\n    --hash=sha256:" + "a" * 64 + "\n" + line, "x.lock")
    assert "x.lock:3" in str(caught.value), "the pin before it spans lines 1 and 2"


def test_bd_t8_the_pins_are_read_with_markers_and_continuations() -> None:
    text = (
        "# generated\n"
        "Foo_Bar==1.2.3 ; python_version >= '3.14' \\\n"
        f"    --hash=sha256:{'b' * 64} \\\n"
        f"    --hash=sha256:{'a' * 64}  # a comment\n"
        f"baz==0.1 --hash=sha256:{'c' * 64}\n"
    )
    pins = build.parse_lock(text, "x.lock")
    assert [(p.name, p.version, p.marker) for p in pins] == [
        ("baz", "0.1", None),
        ("foo-bar", "1.2.3", "python_version >= '3.14'"),
    ]
    assert pins[1].hashes == ("a" * 64, "b" * 64), "sorted"
    assert build.requirements_text(pins) == (
        f"baz==0.1 \\\n    --hash=sha256:{'c' * 64}\n"
        f"foo-bar==1.2.3 ; python_version >= '3.14' \\\n"
        f"    --hash=sha256:{'a' * 64} \\\n    --hash=sha256:{'b' * 64}\n"
    )


def test_bd_t9_pyarrow_is_in_every_image(project: Project) -> None:
    """BD-T9 the_shim_has_pyarrow: the lock's own when it pins one (and new enough), this
    Moruna's when it does not."""
    pins = [build.Pin("leafdep", "1.0", ("a" * 64,))]
    added = build.with_pyarrow(pins, "x.lock")
    assert [p.name for p in added] == ["leafdep", "pyarrow"]
    assert (added[1].version, added[1].hashes) == (build.SHIM_PYARROW[1], build.SHIM_PYARROW[2])
    ours = [build.Pin("pyarrow", "19.0.0", ("d" * 64,))]
    assert build.with_pyarrow(ours, "x.lock") == ours, "the project's pin is kept"
    with pytest.raises(build.Refused, match=r"pyarrow==16\.0\.0; the shim needs pyarrow >= 17"):
        build.with_pyarrow([build.Pin("pyarrow", "16.0.0", ("e" * 64,))], "x.lock")


def test_bd_t9_pyarrow_is_installed_in_layer_two(project: Project, tmp_path: Path) -> None:
    built = project.build(project.kernel("kernel_a"))
    tree = tmp_path / "root"
    unpack(built, tree)
    site = tree / "opt" / "moruna" / "site-packages"
    assert (site / "pyarrow" / "native.so").read_bytes() == b"pyarrow for aarch64"
    assert (site / "leafdep" / "data.txt").read_bytes() == b"leaf"


def test_bd_t10_the_image_is_what_the_kind_reads(project: Project, tmp_path: Path) -> None:
    """BD-T10 the_layout_is_an_image: the OCI layout KA1 reads, every blob under its digest, a
    config whose command is the shim, layers 2 to 4 where the shim and the kernel expect them."""
    built = project.build(project.kernel("kernel_a"), tag="adder:1")
    layout = built.layout
    assert json.loads((layout / "oci-layout").read_text()) == {"imageLayoutVersion": "1.0.0"}
    index = json.loads((layout / "index.json").read_text())
    [entry] = index["manifests"]
    assert (
        entry["digest"]
        == built.digest
        == f"sha256:{hashlib.sha256(blob(layout, built.digest)).hexdigest()}"
    )
    assert entry["annotations"] == {"org.opencontainers.image.ref.name": "adder:1"}
    for path in (layout / "blobs" / "sha256").iterdir():
        assert hashlib.sha256(path.read_bytes()).hexdigest() == path.name, "named by its bytes"

    manifest = manifest_of(built)
    assert manifest["mediaType"] == "application/vnd.oci.image.manifest.v1+json"
    assert [layer.role for layer in built.layers] == ["runtime", "dependencies", "shim", "function"]
    assert [m["mediaType"] for m in manifest["layers"]] == [
        "application/vnd.oci.image.layer.v1.tar+gzip",
        "application/vnd.oci.image.layer.v1.tar",
        "application/vnd.oci.image.layer.v1.tar",
        "application/vnd.oci.image.layer.v1.tar",
    ]
    for described, layer in zip(manifest["layers"], built.layers, strict=True):
        assert described["digest"] == layer.blob.digest and described["size"] == layer.blob.size
        assert len(blob(layout, layer.blob.digest)) == layer.blob.size
    # A layer 2 to 4 digest is its tar's sha256, and is its diff id.
    for layer in built.layers[1:]:
        assert (
            layer.blob.digest
            == f"sha256:{hashlib.sha256(blob(layout, layer.blob.digest)).hexdigest()}"
        )

    config = json.loads(blob(layout, manifest["config"]["digest"]))
    assert (config["os"], config["architecture"]) == ("linux", "arm64")
    assert config["config"]["Cmd"] == [
        "/usr/local/bin/python3",
        "/opt/moruna/shim.py",
        "--workdir",
        "/app",
        "kernel_a:add",
    ]
    assert "Entrypoint" not in config["config"]
    assert config["config"]["WorkingDir"] == "/"
    env = dict(e.split("=", 1) for e in config["config"]["Env"])
    assert env["PATH"] == "/usr/local/bin:/usr/bin" and env["LANG"] == "C.UTF-8", "the runtime's"
    assert env["PYTHONPATH"] == "/app:/opt/moruna/site-packages"
    labels = config["config"]["Labels"]
    assert labels["org.moruna.entry"] == "kernel_a:add"
    assert labels["org.moruna.version"] == "test"
    assert labels["org.opencontainers.image.base.digest"].startswith("sha256:")
    diff_ids = config["rootfs"]["diff_ids"]
    assert diff_ids[1:] == digests(built)[1:]
    assert (
        diff_ids[0]
        == f"sha256:{hashlib.sha256(tar_bytes(blob(layout, digests(built)[0]))).hexdigest()}"
    )

    root = tmp_path / "root"
    unpack(built, root)
    assert (root / "usr/local/bin/python3").is_file(), "layer 1"
    assert (root / "opt/moruna/shim.py").read_text() == (support.PACKAGE / "_shim.py").read_text()
    assert (root / "opt/moruna/moruna/__init__.py").is_file()
    assert (root / "opt/moruna/moruna/_declare.py").read_text() == (
        support.PACKAGE / "_declare.py"
    ).read_text()
    assert (root / "app/kernel_a.py").read_text() == "def add(batch):\n    return batch\n"
    leftovers = [
        p
        for p in (root / "opt/moruna/site-packages").rglob("*")
        if p.name in ("uv_cache.json", ".lock")
    ]
    assert leftovers == []
    record = (root / "opt/moruna/site-packages/leafdep-1.0.dist-info/RECORD").read_text()
    assert "uv_cache" not in record


def test_bd_t11_the_same_inputs_are_the_same_image(project: Project, tmp_path: Path) -> None:
    """BD-T11 reproducible: one function built twice, into two directories, from two working
    directories, with the file's time and mode changed between, is one image: every layer's
    digest, the image's and the layout's own files, bit for bit."""
    first = project.build(project.kernel("kernel_a"), out=tmp_path / "one")
    source = project.root / "kernel_a.py"
    os.utime(source, (1_000_000, 1_000_000))
    source.chmod(0o600)
    (project.wheels / "touched").write_text("x")
    cwd = Path.cwd()
    os.chdir(tmp_path)
    try:
        second = project.build(project.kernel("kernel_a"), out=tmp_path / "two")
    finally:
        os.chdir(cwd)
    assert digests(first) == digests(second)
    assert first.digest == second.digest
    for name in ("oci-layout", "index.json"):
        assert (tmp_path / "one" / name).read_bytes() == (tmp_path / "two" / name).read_bytes()
    assert {p.name for p in (tmp_path / "one/blobs/sha256").iterdir()} == {
        p.name for p in (tmp_path / "two/blobs/sha256").iterdir()
    }
    for layer in first.layers[1:]:
        assert blob(first.layout, layer.blob.digest) == blob(second.layout, layer.blob.digest)


def test_bd_t12_functions_of_one_lock_differ_in_layer_four_only(project: Project) -> None:
    """BD-T12 shared_layers: two functions of one project share layers 1 to 3 and the layout
    holds them once; each has its own layer 4."""
    out = project.scratch / "shared"
    one = project.build(project.kernel("kernel_a"), out=out, tag="a:1")
    two = project.build(project.kernel("kernel_b"), out=out, tag="b:1")
    assert digests(one)[:3] == digests(two)[:3]
    assert digests(one)[3] != digests(two)[3]
    assert one.digest != two.digest
    blobs = {p.name for p in (out / "blobs" / "sha256").iterdir()}
    # Five layers (the runtime's, dependencies, shim, two functions), two configs, two manifests.
    assert len(blobs) == 9
    refs = [
        m["annotations"]["org.opencontainers.image.ref.name"]
        for m in json.loads((out / "index.json").read_text())["manifests"]
    ]
    assert refs == ["a:1", "b:1"]
    again = project.build(project.kernel("kernel_a"), out=out, tag="a:1")
    assert again.digest == one.digest, "a name rebuilt is the same image"
    assert {p.name for p in (out / "blobs" / "sha256").iterdir()} == blobs, "and writes nothing new"
    refs = [
        m["annotations"]["org.opencontainers.image.ref.name"]
        for m in json.loads((out / "index.json").read_text())["manifests"]
    ]
    assert refs == ["a:1", "b:1"], "one entry for each name"


def test_bd_t13_a_new_lock_is_a_new_layer_two_and_nothing_else(project: Project) -> None:
    """BD-T13 a_dependency_change_rebuilds_two: layers 1, 3 and 4 are unchanged."""
    before = project.build(project.kernel("kernel_a"), out=project.scratch / "before")
    newer = support.make_wheel(project.wheels, "leafdep", "1.1", {"data.txt": b"newer"})
    old = project.root / "requirements.lock"
    text = old.read_text().replace(
        support.lock_line("leafdep", "1.0", [project.wheels / "leafdep-1.0-py3-none-any.whl"]),
        support.lock_line("leafdep", "1.1", [newer]),
    )
    assert text != old.read_text()
    old.write_text(text)
    after = project.build(project.kernel("kernel_a"), out=project.scratch / "after")
    one, two = digests(before), digests(after)
    assert (one[0], one[2], one[3]) == (two[0], two[2], two[3])
    assert one[1] != two[1]


def test_bd_t14_each_platform_gets_its_own_wheels(project: Project, tmp_path: Path) -> None:
    """BD-T14 the_target_is_the_image's_platform: wheels are chosen for the image's platform and
    its markers, never this machine's."""
    arm = project.build(project.kernel("kernel_a"), out=tmp_path / "arm", platform=AARCH64)
    amd = project.build(project.kernel("kernel_a"), out=tmp_path / "amd", platform=X86_64)
    assert digests(arm)[1] != digests(amd)[1]
    assert digests(arm)[2:] == digests(amd)[2:], "shim and function do not depend on it"
    for built, arch in ((arm, "aarch64"), (amd, "x86_64")):
        root = tmp_path / f"root-{arch}"
        unpack(built, root)
        site = root / "opt/moruna/site-packages"
        assert (site / "pyarrow/native.so").read_bytes() == f"pyarrow for {arch}".encode()
        assert not (site / "wincolor").exists(), "a win32 marker is false for linux"
    config = json.loads(blob(arm.layout, manifest_of(arm)["config"]["digest"]))
    assert config["architecture"] == "arm64"
    config = json.loads(blob(amd.layout, manifest_of(amd)["config"]["digest"]))
    assert config["architecture"] == "amd64"


def test_bd_t14_a_runtime_of_another_architecture_is_refused(project: Project) -> None:
    with pytest.raises(build.Refused, match=r"linux/arm64, and the image is for linux/x86_64"):
        project.build(
            project.kernel("kernel_a"),
            platform=X86_64,
            runtime_layout=project.runtimes[AARCH64],
        )


@pytest.mark.parametrize(
    ("system", "machine", "named"),
    [
        ("Darwin", "arm64", "linux/aarch64"),
        ("Darwin", "x86_64", "linux/aarch64"),
        ("Linux", "x86_64", "linux/x86_64"),
        ("Linux", "AMD64", "linux/x86_64"),
        ("Linux", "aarch64", "linux/aarch64"),
    ],
)
def test_bd_t14_the_default_platform(
    monkeypatch: pytest.MonkeyPatch, system: str, machine: str, named: str
) -> None:
    """BD-T14: absent, the host's architecture on Linux and linux/aarch64 on a Mac."""
    monkeypatch.setattr(
        build,
        "host_platform",
        types.SimpleNamespace(system=lambda: system, machine=lambda: machine),
    )
    assert build.parse_platform(None).name == named


@pytest.mark.parametrize(("system", "machine"), [("Windows", "AMD64"), ("Linux", "riscv64")])
def test_bd_t14_a_machine_with_no_default_must_name_one(
    monkeypatch: pytest.MonkeyPatch, system: str, machine: str
) -> None:
    monkeypatch.setattr(
        build,
        "host_platform",
        types.SimpleNamespace(system=lambda: system, machine=lambda: machine),
    )
    with pytest.raises(build.Refused, match="has no default; name linux/aarch64"):
        build.parse_platform(None)


def test_bd_t14_the_platform_names() -> None:
    assert build.parse_platform("linux/arm64").name == "linux/aarch64"
    assert build.parse_platform("linux/amd64").name == "linux/x86_64"
    assert build.parse_platform(None).name in ("linux/aarch64", "linux/x86_64")
    for bad in ("windows/amd64", "linux/riscv64", "aarch64"):
        with pytest.raises(build.Refused, match="linux/aarch64 or linux/x86_64"):
            build.parse_platform(bad)


def test_bd_t14_a_wheel_the_platform_lacks_is_a_refusal_naming_it(project: Project) -> None:
    leaf = support.make_wheel(
        project.wheels, "nativeonly", "1.0", {}, "cp314-cp314-manylinux_2_28_x86_64"
    )
    project.write_lock(
        support.lock_line("pyarrow", "25.0.1", sorted(project.wheels.glob("pyarrow-*"))),
        support.lock_line("nativeonly", "1.0", [leaf]),
    )
    with pytest.raises(build.Refused, match="uv could not install the lock for linux/aarch64"):
        project.build(project.kernel("kernel_a"))
    assert project.build(project.kernel("kernel_a"), platform=X86_64).layers


def test_bd_t14_a_hash_that_does_not_match_is_refused(project: Project) -> None:
    project.write_lock(
        support.lock_line("pyarrow", "25.0.1", sorted(project.wheels.glob("pyarrow-*"))),
        f"leafdep==1.0 --hash=sha256:{'0' * 64}\n",
    )
    with pytest.raises(build.Refused, match="uv could not install"):
        project.build(project.kernel("kernel_a"))


def test_bd_t15_a_uv_lock_is_exported_without_its_project_or_development_group(
    tmp_path: Path,
) -> None:
    """BD-T15 a_uv_lock_is_read: the runtime dependencies uv pins, with hashes and markers."""
    (tmp_path / "pyproject.toml").write_text(
        '[project]\nname = "demo"\nversion = "0.1"\nrequires-python = ">=3.14"\n'
        'dependencies = ["leafdep==1.0"]\n[dependency-groups]\ndev = ["devtool==2.0"]\n'
    )
    h = {
        name: hashlib.sha256(name.encode()).hexdigest()
        for name in ("leafdep", "devtool", "colorama")
    }

    def wheel(name: str, version: str, deps: str = "") -> str:
        return (
            f'[[package]]\nname = "{name}"\nversion = "{version}"\n'
            'source = { registry = "https://pypi.org/simple" }\n'
            + deps
            + f'wheels = [{{ url = "https://f.example/{name}-{version}-py3-none-any.whl", '
            f'hash = "sha256:{h[name]}", size = 1 }}]\n\n'
        )

    (tmp_path / "uv.lock").write_text(
        'version = 1\nrevision = 3\nrequires-python = ">=3.14"\n\n'
        '[[package]]\nname = "demo"\nversion = "0.1"\nsource = { virtual = "." }\n'
        'dependencies = [{ name = "leafdep" }]\n\n'
        '[package.dev-dependencies]\ndev = [{ name = "devtool" }]\n\n'
        '[package.metadata]\nrequires-dist = [{ name = "leafdep", specifier = "==1.0" }]\n\n'
        '[package.metadata.requires-dev]\ndev = [{ name = "devtool", specifier = "==2.0" }]\n\n'
        + wheel("devtool", "2.0")
        + wheel(
            "leafdep",
            "1.0",
            'dependencies = [{ name = "colorama", marker = "sys_platform == \'win32\'" }]\n',
        )
        + wheel("colorama", "0.4.6")
    )
    lock = build.find_lock(tmp_path, None)
    assert lock.name == "uv.lock"
    pins = build.parse_lock(build.lock_text(lock, build.uv_command()), str(lock))
    assert [(p.name, p.version, p.hashes, p.marker) for p in pins] == [
        ("colorama", "0.4.6", (h["colorama"],), "sys_platform == 'win32'"),
        ("leafdep", "1.0", (h["leafdep"],), None),
    ]


def test_bd_t15_a_broken_uv_lock_is_a_refusal_with_uvs_words(tmp_path: Path) -> None:
    (tmp_path / "uv.lock").write_text("this is not toml = = =\n")
    with pytest.raises(build.Refused, match="uv export failed"):
        build.lock_text(tmp_path / "uv.lock", build.uv_command())


def test_bd_t16_a_layer_is_the_same_tar_whatever_its_files_say(tmp_path: Path) -> None:
    """BD-T16 the_tar_is_reproducible: sorted entries, time 0, owner 0:0, fixed modes."""
    src = tmp_path / "src"
    (src / "pkg" / "__pycache__").mkdir(parents=True)
    (src / "pkg" / "a.py").write_text("a = 1\n")
    (src / "pkg" / "run.sh").write_text("#!/bin/sh\n")
    (src / "pkg" / "run.sh").chmod(0o700)
    (src / "pkg" / "b.txt").write_text("b")
    (src / "pkg" / "__pycache__" / "a.cpython-314.pyc").write_bytes(b"bytecode")
    (src / "pkg" / "latest").symlink_to("a.py")
    (src / "pkg" / "a.py").chmod(0o600)
    first = build.write_layer(build.tree_entries(src, "opt/x"), tmp_path / "w1")
    os.utime(src / "pkg" / "a.py", (5, 5))
    (src / "pkg" / "a.py").chmod(0o666)
    second = build.write_layer(reversed(list(build.tree_entries(src, "opt/x"))), tmp_path / "w2")
    assert first.digest == second.digest and first.size == second.size
    assert first.digest == f"sha256:{hashlib.sha256(first.path.read_bytes()).hexdigest()}"
    with tarfile.open(first.path) as archive:
        members = archive.getmembers()
    names = [m.name for m in members]
    assert names == sorted(names)
    assert names == [
        "opt",
        "opt/x",
        "opt/x/pkg",
        "opt/x/pkg/__pycache__",
        "opt/x/pkg/__pycache__/a.cpython-314.pyc",
        "opt/x/pkg/a.py",
        "opt/x/pkg/b.txt",
        "opt/x/pkg/latest",
        "opt/x/pkg/run.sh",
    ], "parents are entries, and bytecode is a file like another"
    assert {(m.uid, m.gid, m.uname, m.gname, m.mtime) for m in members} == {(0, 0, "", "", 0)}
    modes = {m.name: m.mode for m in members}
    assert modes["opt/x/pkg/a.py"] == 0o644 and modes["opt/x/pkg/run.sh"] == 0o755
    assert modes["opt/x/pkg"] == 0o755 and modes["opt/x/pkg/latest"] == 0o777
    assert next(m for m in members if m.name.endswith("latest")).linkname == "a.py"
    assert len(first.path.read_bytes()) % 512 == 0


def test_bd_t16_a_path_made_twice_and_a_file_that_is_no_file_are_refused(tmp_path: Path) -> None:
    twice = [build.Entry("a", "file", 0o644, data=b"1"), build.Entry("a", "file", 0o644, data=b"2")]
    with pytest.raises(build.Refused, match="a is made twice"):
        build.write_layer(twice, tmp_path / "w")
    os.mkfifo(tmp_path / "fifo")
    with pytest.raises(build.Refused, match="neither a file, a directory nor a link"):
        list(build.tree_entries(tmp_path, "x"))


def test_bd_t17_the_function_layer_holds_its_file_its_packages_and_the_files_it_names(
    tmp_path: Path,
) -> None:
    """BD-T17 layer_four: the defining module at its import path under /app, the __init__.py
    of every package above it, the files named below the import root; /app always."""
    root = (tmp_path / "src").resolve()
    (root / "pkg" / "sub").mkdir(parents=True)
    (root / "pkg" / "__init__.py").write_text("")
    (root / "pkg" / "sub" / "__init__.py").write_text("")
    (root / "pkg" / "sub" / "mod.py").write_text("def f(b):\n    return b\n")
    (root / "pkg" / "other.py").write_text("")
    (root / "model.bin").write_bytes(b"weights")
    function = build.Function(
        "pkg.sub.mod", "f", root / "pkg" / "sub" / "mod.py", (root / "model.bin",)
    )
    assert function.root == root.resolve()
    assert sorted(build.function_files(function)) == [
        "app/model.bin",
        "app/pkg/__init__.py",
        "app/pkg/sub/__init__.py",
        "app/pkg/sub/mod.py",
    ]
    (root / "kernel.py").write_text("")
    top = build.Function("kernel", "f", root / "kernel.py")
    assert top.root == root.resolve()
    assert sorted(build.function_files(top)) == ["app/kernel.py"]
    package = build.Function("pkg.sub", "f", root / "pkg" / "sub" / "__init__.py")
    assert package.root == root.resolve()
    assert sorted(build.function_files(package)) == [
        "app/pkg/__init__.py",
        "app/pkg/sub/__init__.py",
    ]
    staged = tmp_path / "staged"
    build.stage_function(top, staged)
    assert [p.name for p in staged.iterdir()] == ["kernel.py"]
    empty = tmp_path / "empty"
    build.stage_function(build.Function("kernel", "f", root / "kernel.py"), empty)
    assert empty.is_dir(), "/app exists"


def test_bd_t17_a_named_file_outside_the_import_root_or_missing_is_refused(tmp_path: Path) -> None:
    (tmp_path / "src").mkdir()
    (tmp_path / "src" / "k.py").write_text("")
    (tmp_path / "elsewhere.txt").write_text("x")
    outside = build.Function("k", "f", tmp_path / "src" / "k.py", (tmp_path / "elsewhere.txt",))
    with pytest.raises(build.Refused, match="not under"):
        build.function_files(outside)
    missing = build.Function("k", "f", tmp_path / "src" / "k.py", (tmp_path / "src" / "gone",))
    with pytest.raises(build.Refused, match="no such file"):
        build.function_files(missing)


def test_bd_t17_files_the_kernel_names_travel_with_it(project: Project, tmp_path: Path) -> None:
    (project.root / "weights.bin").write_bytes(b"w")
    built = project.build(project.kernel("kernel_a"), files=[project.root / "weights.bin"])
    unpack(built, tmp_path / "root")
    assert (tmp_path / "root/app/weights.bin").read_bytes() == b"w"
    bare = project.build(project.kernel("kernel_a"), out=project.scratch / "bare")
    assert digests(bare)[3] != digests(built)[3] and digests(bare)[:3] == digests(built)[:3]


def test_bd_t18_a_directory_that_is_not_a_layout_is_not_written_into(project: Project) -> None:
    out = project.scratch / "occupied"
    out.mkdir(parents=True)
    (out / "precious.txt").write_text("keep")
    with pytest.raises(build.Refused, match="holds files and is not an OCI layout"):
        project.build(project.kernel("kernel_a"), out=out)
    assert sorted(p.name for p in out.iterdir()) == ["precious.txt"]


def test_bd_t18_a_corrupt_blob_already_in_the_layout_is_replaced(project: Project) -> None:
    first = project.build(project.kernel("kernel_a"))
    victim = first.layout / "blobs" / "sha256" / first.layers[2].blob.digest.split(":")[1]
    victim.write_bytes(b"corrupt")
    project.build(project.kernel("kernel_a"))
    assert hashlib.sha256(victim.read_bytes()).hexdigest() == victim.name


def test_bd_t19_the_runtime_layout_is_checked_against_its_names(project: Project) -> None:
    layout = project.runtimes[AARCH64]
    layer = next(
        p for p in (layout / "blobs" / "sha256").iterdir() if p.read_bytes()[:2] == b"\x1f\x8b"
    )
    layer.write_bytes(layer.read_bytes() + b"x")
    with pytest.raises(build.Refused, match="the bytes to write hash to"):
        project.build(project.kernel("kernel_a"))
    with pytest.raises(build.Refused, match="not an OCI layout"):
        project.build(project.kernel("kernel_a"), runtime_layout=project.scratch)
    with pytest.raises(build.Refused, match="has 0 images of that name"):
        project.build(project.kernel("kernel_a"), runtime_reference="nope")
    with pytest.raises(build.Refused, match="needs runtime_reference"):
        build.build(project.kernel("kernel_a"), runtime_layout=layout)


def test_bd_t19_the_kernel_a_build_is_asked_for(project: Project) -> None:
    """BD-T19 function_of: a kernel is found in the module that defines it, by the name the
    module binds it to."""
    module = project.module("kernel_a")
    function = build.function_of(module.add)
    assert (function.module, function.name, function.source) == (
        "kernel_a",
        "add",
        project.root / "kernel_a.py",
    )
    assert function.entry == "kernel_a:add"

    class Spec:
        """What a decorated kernel is to the builder: a wrapped callable, a fingerprint."""

        wrapped = module.add
        stateful = True
        fingerprint = "sha256:abc"

    module.decorated = Spec()
    found = build.function_of(module.decorated)
    assert (found.name, found.fingerprint, found.stateful) == ("decorated", "sha256:abc", True)
    with pytest.raises(build.Refused, match="does not bind the kernel to a name"):
        build.function_of(Spec())
    built = project.build(module.decorated)
    labels = json.loads(blob(built.layout, manifest_of(built)["config"]["digest"]))["config"][
        "Labels"
    ]
    assert (labels["org.moruna.fingerprint"], labels["org.moruna.stateful"]) == (
        "sha256:abc",
        "true",
    )


def test_bd_t19_a_function_in_a_package_is_imported_as_one(
    project: Project, tmp_path: Path
) -> None:
    pkg = project.root / "pkg"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "mod.py").write_text("def add(b):\n    return b\n")
    module = support.load_path(pkg / "mod.py", "pkg.mod")
    built = project.build(module.add)
    config = json.loads(blob(built.layout, manifest_of(built)["config"]["digest"]))
    assert config["config"]["Cmd"][-1] == "pkg.mod:add"
    unpack(built, tmp_path / "root")
    assert (tmp_path / "root/app/pkg/mod.py").is_file() and (
        tmp_path / "root/app/pkg/__init__.py"
    ).is_file()


def test_bd_t20_the_report_prints_the_image_and_each_layer(
    project: Project, capsys: pytest.CaptureFixture[str]
) -> None:
    built = project.build(project.kernel("kernel_a"))
    build.print_report(built)
    text = capsys.readouterr().out
    assert f"image     {built.digest}" in text
    for position, layer in enumerate(built.layers, 1):
        assert f"layer {position}" in text and f"{layer.blob.digest} {layer.blob.size}" in text
    report = built.report()
    assert report["image"] == built.digest and len(report["layers"]) == 4
    assert [layer["role"] for layer in report["layers"]] == [
        "runtime",
        "dependencies",
        "shim",
        "function",
    ]


def test_bd_t20_the_command_refuses_a_kernel_with_no_function(
    capsys: pytest.CaptureFixture[str],
) -> None:
    assert build.main(["not_a_kernel_name"]) == build.EXIT_REFUSED
    assert "name the kernel as <module>:<function>" in capsys.readouterr().err


def test_bd_t21_the_wheels_and_bytecode_are_made_by_the_runtimes_cpython(
    project: Project, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """BD-T21 the_interpreter: a free threaded builder would be given the free threaded wheels
    and write bytecode the runtime refuses, so the tags are read and the bytecode written by a
    standard build of the runtime's minor, a final release, with its magic number: this
    interpreter, else one on the path; and nothing is built when none is."""
    runtime = build.read_runtime_layout(
        project.runtimes[AARCH64], "python-test", build.PLATFORMS["aarch64"]
    )
    wanted = build.runtime_bytecode(runtime)
    assert (wanted.minor, wanted.version, wanted.magic) == (14, "3.14.99", None)
    here = build.tag_interpreter(wanted)
    assert here.minor == 14 and build._is_runtime_build(here.interpreter, wanted)
    assert not build._is_runtime_build(here.interpreter, build.Bytecode(13, "3.13.1", None))
    assert not build._is_runtime_build(here.interpreter, build.Bytecode(14, "3.14.9", 1)), "magic"
    assert build._is_runtime_build(
        here.interpreter, build.Bytecode(14, "3.14.9", support.this_magic())
    )
    # Neither this interpreter nor one on the path: refused, saying what it looked for.
    fake = tmp_path / "bin" / "python3.14"
    fake.parent.mkdir()
    fake.write_text("#!/bin/sh\necho False\n")
    fake.chmod(0o755)
    monkeypatch.setattr(build.sys, "executable", str(fake))
    monkeypatch.setattr(build.shutil, "which", lambda name: None)
    with pytest.raises(build.Refused, match=r"final 3\.14 release .* and none was found"):
        project.build(project.kernel("kernel_a"))
    # A standard interpreter on the path stands in for the builder's own.
    fake.write_text("#!/bin/sh\necho True\n")
    monkeypatch.setattr(
        build.shutil, "which", lambda name: str(fake) if name == "python3.14" else None
    )
    assert build.tag_interpreter(wanted).interpreter == str(fake)


def test_bd_t21_the_runtimes_bytecode_is_read_from_the_image(
    project: Project, tmp_path: Path
) -> None:
    """BD-T21: the magic number of a .pyc the runtime holds is the one the bytecode needs; a
    runtime that is not a final release, or that says no version, is refused."""
    platform = build.PLATFORMS["aarch64"]
    layout = support.make_runtime(tmp_path / "with-pyc", pyc_magic=1234)
    runtime = build.read_runtime_layout(layout, "python-test", platform)
    assert build.runtime_bytecode(runtime).magic == 1234
    other = support.make_runtime(tmp_path / "other-magic", pyc_magic=support.this_magic() + 1)
    monkey_runtime = build.read_runtime_layout(other, "python-test", platform)
    with pytest.raises(build.Refused, match="magic number"):
        project.build(
            project.kernel("kernel_a"), runtime_layout=other, runtime_reference="python-test"
        )
    assert build.runtime_bytecode(monkey_runtime).magic == support.this_magic() + 1
    for version, said in [("3.15.0rc1", "not a final release"), (None, "has no PYTHON_VERSION")]:
        layout = support.make_runtime(tmp_path / f"v-{version}", version=version)
        runtime = build.read_runtime_layout(layout, "python-test", platform)
        with pytest.raises(build.Refused, match=said):
            build.runtime_bytecode(runtime)


def test_bd_t30_layers_two_to_four_carry_bytecode_the_runtime_uses(
    project: Project, tmp_path: Path
) -> None:
    """BD-T30 bytecode_is_shipped: every importable source of layers 2 to 4 has its .pyc beside
    it for the runtime's interpreter, hash based and unchecked (used whatever the source's time),
    valid for the source it sits by, naming the path the image has; a .pyc a wheel shipped is
    not in the layer; the shim script, which Python never caches, has none."""
    support.make_wheel(
        project.wheels,
        "leafdep",
        "1.2",
        {"data.txt": b"leaf", "__pycache__/old.cpython-312.pyc": b"another python's"},
    )
    leaf = project.wheels / "leafdep-1.2-py3-none-any.whl"
    old = project.root / "requirements.lock"
    text = old.read_text().replace(
        support.lock_line("leafdep", "1.0", [project.wheels / "leafdep-1.0-py3-none-any.whl"]),
        support.lock_line("leafdep", "1.2", [leaf]),
    )
    old.write_text(text)
    built = project.build(project.kernel("kernel_a"))
    root = tmp_path / "root"
    unpack(built, root)
    tag = sys.implementation.cache_tag
    checked = 0
    for source in sorted(root.rglob("*.py")):
        relative = source.relative_to(root)
        pyc = source.parent / "__pycache__" / f"{source.stem}.{tag}.pyc"
        if relative.as_posix() == "opt/moruna/shim.py":
            assert not pyc.exists(), "a script is compiled at every run and never cached"
            continue
        if relative.parts[:2] == ("opt", "moruna") and relative.parts[2] == "site-packages":
            pass
        elif relative.parts[0] not in ("opt", "app"):
            continue
        data = pyc.read_bytes()
        assert int.from_bytes(data[:2], "little") == support.this_magic()
        assert int.from_bytes(data[4:8], "little") == 0b01, "hash based, not checked"
        assert data[8:16] == importlib.util.source_hash(source.read_bytes())
        assert marshal.loads(data[16:]).co_filename == "/" + relative.as_posix()
        checked += 1
    assert checked >= 5  # leafdep, pyarrow, the stand-in's two, the kernel
    site = root / "opt/moruna/site-packages"
    assert sorted(p.name for p in (site / "leafdep/__pycache__").iterdir()) == [
        f"__init__.{tag}.pyc"
    ], "the wheel's own bytecode is replaced by this one's"
    for path in (root / "opt/moruna/moruna", root / "app"):
        assert (path / "__pycache__").is_dir()


@pytest.mark.skipif(importlib.util.find_spec("pyarrow") is None, reason="pyarrow")
def test_bd_t31_bytecode_of_a_real_tree_is_the_same_in_every_process(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """BD-T31 bytecode_is_reproducible: pyarrow's own sources, a few hundred files, compiled in
    two directories by two interpreter processes under different hash seeds and working
    directories, give the same bytes for every file."""
    spec = importlib.util.find_spec("pyarrow")
    assert spec is not None and spec.submodule_search_locations
    home = Path(next(iter(spec.submodule_search_locations)))
    trees = [tmp_path / "one" / "site", tmp_path / "two" / "elsewhere" / "site"]
    for tree in trees:
        for source in home.rglob("*.py"):
            target = tree / "pyarrow" / source.relative_to(home)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(source.read_bytes())
    tools = build.tag_interpreter(build.Bytecode(sys.version_info.minor, "3.14.0", None))
    monkeypatch.setenv("PYTHONHASHSEED", "12345")
    build.compile_tree(trees[0], "/opt/moruna/site-packages", tools)
    monkeypatch.setenv("PYTHONHASHSEED", "random")
    monkeypatch.chdir(tmp_path)
    build.compile_tree(trees[1], "/opt/moruna/site-packages", tools)
    first = {p.relative_to(trees[0]): p.read_bytes() for p in trees[0].rglob("*.pyc")}
    second = {p.relative_to(trees[1]): p.read_bytes() for p in trees[1].rglob("*.pyc")}
    assert len(first) > 50  # noqa: PLR2004
    assert first == second


def test_bd_t32_a_source_distribution_is_refused_and_no_package_code_runs(
    project: Project, tmp_path: Path
) -> None:
    """BD-T32 wheels_only: a pin that has only a source distribution is refused, and the
    package's setup script, which writes a marker if it is ever run, never is."""
    marker = tmp_path / "the-package-ran"
    sdist = support.make_sdist(project.wheels, "sdistonly", "1.0", marker)
    project.write_lock(
        support.lock_line("pyarrow", "25.0.1", sorted(project.wheels.glob("pyarrow-*"))),
        support.lock_line("sdistonly", "1.0", [sdist]),
    )
    with pytest.raises(build.Refused, match="uv could not install the lock"):
        project.build(project.kernel("kernel_a"))
    assert not marker.exists(), "nothing of the package ran"
    assert not any(project.wheels.glob("sdistonly*.whl")), "and no wheel was built from it"
    assert not list(tmp_path.rglob("sdistonly*egg-info"))


# --- the registry ---------------------------------------------------------------------------


class FakeRegistry:
    """A registry of one repository that asks for a token, serves a blob from a CDN it
    redirects to, and records what it was asked."""

    HOST = "registry.test"
    CDN = "cdn.test"

    def __init__(self, platforms: tuple[str, ...] = ("arm64", "amd64")) -> None:
        self.requests: list[tuple[str, dict[str, str]]] = []
        self.objects: dict[str, bytes] = {}
        manifests = []
        for arch in platforms:
            tar = support.tar_of({f"usr/{arch}": arch.encode()})
            layer = gzip.compress(tar, mtime=0)
            config = json.dumps(
                {
                    "architecture": arch,
                    "os": "linux",
                    "config": {"Env": ["PATH=/usr/local/bin"]},
                    "rootfs": {"type": "layers", "diff_ids": [f"sha256:{support.sha256(tar)}"]},
                }
            ).encode()
            manifest = json.dumps(
                {
                    "schemaVersion": 2,
                    "mediaType": build.MEDIA_MANIFEST,
                    "config": {
                        "mediaType": build.MEDIA_CONFIG,
                        "digest": self.add(config),
                        "size": len(config),
                    },
                    "layers": [
                        {
                            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                            "digest": self.add(layer),
                            "size": len(layer),
                        }
                    ],
                }
            ).encode()
            manifests.append(
                {
                    "mediaType": build.MEDIA_MANIFEST,
                    "digest": self.add(manifest),
                    "size": len(manifest),
                    "platform": {"os": "linux", "architecture": arch},
                }
            )
        manifests.append(
            {
                "mediaType": build.MEDIA_MANIFEST,
                "digest": "sha256:" + "9" * 64,
                "size": 1,
                "platform": {"os": "unknown", "architecture": "unknown"},
            }
        )
        index = json.dumps(
            {"schemaVersion": 2, "mediaType": build.MEDIA_INDEX, "manifests": manifests}
        ).encode()
        self.index = self.add(index)

    def add(self, data: bytes) -> str:
        digest = f"sha256:{support.sha256(data)}"
        self.objects[digest] = data
        return digest

    def __call__(self, url: str, headers: dict[str, str]) -> tuple[int, dict[str, str], io.BytesIO]:
        self.requests.append((url, dict(headers)))
        scheme, _, rest = url.partition("://")
        host, _, path = rest.partition("/")
        if host == "auth.test":
            assert (
                "service=registry.test" in path
                and "scope=repository%3Alibrary%2Fpython%3Apull" in path
            )
            return 200, {}, io.BytesIO(json.dumps({"token": "secret-token"}).encode())
        if host == self.CDN:
            assert "authorization" not in {k.lower() for k in headers}, (
                "a credential left the registry's host"
            )
            return 200, {}, io.BytesIO(self.objects["sha256:" + path.rsplit("/", 1)[1]])
        assert host == self.HOST and scheme == "https"
        if headers.get("Authorization") != "Bearer secret-token":
            challenge = 'Bearer realm="https://auth.test/token",service="registry.test",scope="repository:library/python:pull"'
            return 401, {"www-authenticate": challenge}, io.BytesIO(b"")
        kind, digest = path.split("/")[-2:]
        if kind == "blobs":
            return (
                307,
                {"location": f"https://{self.CDN}/blob/{digest.split(':')[1]}"},
                io.BytesIO(b""),
            )
        return 200, {}, io.BytesIO(self.objects[digest])


def test_bd_t22_the_runtime_comes_from_its_index_by_digest(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """BD-T22 the_runtime_is_fetched_by_digest: anonymous token, the pinned index, the
    manifest for the platform, the config and layers through a redirect that drops the
    credential, each verified and kept by digest; a second build reads the cache alone."""
    fake = FakeRegistry()
    monkeypatch.setattr(build, "RUNTIME_INDEX", fake.index)
    registry = build.Registry(fake.HOST, "library/python", fake)
    runtime = build.fetch_runtime(build.PLATFORMS["aarch64"], tmp_path / "cache", registry)
    assert runtime.config["architecture"] == "arm64"
    assert len(runtime.layers) == 1 and runtime.files[runtime.layers[0]["digest"]].is_file()
    assert runtime.name == build.RUNTIME_NAME
    asked = [url for url, _ in fake.requests]
    assert asked[0].endswith(f"/manifests/{fake.index}"), "by the pinned digest, not a tag"
    assert len({url for url in asked if "/manifests/" in url}) == 2, "the index and one manifest"
    store = tmp_path / "cache" / "blobs" / "sha256"
    for path in store.iterdir():
        assert hashlib.sha256(path.read_bytes()).hexdigest() == path.name
    before = len(fake.requests)
    again = build.fetch_runtime(
        build.PLATFORMS["aarch64"],
        tmp_path / "cache",
        build.Registry(fake.HOST, "library/python", fake),
    )
    assert again.digest == runtime.digest
    assert len(fake.requests) == before, "the second build asks the registry for nothing"


def test_bd_t22_a_blob_whose_bytes_are_not_its_name_is_refused(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = FakeRegistry()
    monkeypatch.setattr(build, "RUNTIME_INDEX", fake.index)
    layers = [d for d, data in fake.objects.items() if data[:2] == b"\x1f\x8b"]
    for layer in layers:
        fake.objects[layer] = fake.objects[layer] + b"tampered"
    registry = build.Registry(fake.HOST, "library/python", fake)
    with pytest.raises(build.Refused, match="its bytes hash to"):
        build.fetch_runtime(build.PLATFORMS["x86_64"], tmp_path / "cache", registry)
    kept = {p.name for p in (tmp_path / "cache" / "blobs" / "sha256").iterdir()}
    assert not kept & {layer.split(":")[1] for layer in layers}, "nothing tampered is kept"


def test_bd_t22_a_platform_the_index_lacks_and_a_registry_error_are_refusals(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = FakeRegistry(platforms=("amd64",))
    monkeypatch.setattr(build, "RUNTIME_INDEX", fake.index)
    registry = build.Registry(fake.HOST, "library/python", fake)
    with pytest.raises(build.Refused, match="0 manifests for linux/aarch64"):
        build.fetch_runtime(build.PLATFORMS["aarch64"], tmp_path / "cache", registry)

    def refusing(url: str, headers: dict[str, str]) -> tuple[int, dict[str, str], io.BytesIO]:
        return 404, {}, io.BytesIO(b"")

    with pytest.raises(build.Refused, match="the registry answered 404"):
        build.fetch_runtime(
            build.PLATFORMS["x86_64"],
            tmp_path / "other",
            build.Registry(fake.HOST, "library/python", refusing),
        )


def test_bd_t22_the_pinned_runtime_is_a_digest_of_the_official_image() -> None:
    assert build.RUNTIME_INDEX.startswith("sha256:") and len(build.RUNTIME_INDEX) == 71
    assert build.RUNTIME_NAME == "docker.io/library/python:3.14-slim"


# --- KC2's converter --------------------------------------------------------------------------


@pytest.mark.skipif(
    not os.environ.get("MORUNA_LAYER_BIN"), reason="MORUNA_LAYER_BIN: the built moruna-layer"
)
def test_bd_t23_the_image_converts_to_layer_files(project: Project, tmp_path: Path) -> None:
    """BD-T23 the_image_is_ready_for_the_kernel: KC2's converter (SDD 27) reads the layout and
    writes one layer file for each layer, each named by its own id."""
    built = project.build(project.kernel("kernel_a"), tag="adder:1")
    out = tmp_path / "layers"
    done = subprocess.run(
        [os.environ["MORUNA_LAYER_BIN"], "convert", str(built.layout), "adder:1", str(out)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert done.returncode == 0, done.stderr
    lines = done.stdout.strip().splitlines()
    assert len(lines) == len(built.layers)
    for line in lines:
        layer_id, _, pages, _, path = line.split(" ", 4)
        assert int(pages) > 0 and Path(path).stat().st_size > 0
        assert Path(path).name == f"{layer_id.split(':')[-1]}.layer"


# --- the command ------------------------------------------------------------------------------


@pytest.fixture
def engine(project: Project, monkeypatch: pytest.MonkeyPatch) -> Project:
    """What ``main`` imports from the package, as a module that loads a file by path and gives
    a version: the command's own code runs, and no engine is needed to run it."""
    stub = types.ModuleType("moruna")
    stub.__path__ = []
    check = types.ModuleType("moruna._check")
    check._load = lambda target: support.load_path(Path(target), Path(target).stem)  # type: ignore[attr-defined]
    core = types.ModuleType("moruna._core")
    core.__version__ = "9.9.9"  # type: ignore[attr-defined]
    stub._check, stub._core = check, core  # type: ignore[attr-defined]
    for name, module in (("moruna", stub), ("moruna._check", check), ("moruna._core", core)):
        monkeypatch.setitem(sys.modules, name, module)
    return project


def command(project: Project, *extra: str, kernel: str = "kernel_a.py:add") -> list[str]:
    return [
        f"{project.root / kernel.split(':', 1)[0]}:{kernel.split(':', 1)[1]}",
        "--out", str(project.scratch / "image"),
        "--platform", AARCH64,
        "--runtime-layout", str(project.runtimes[AARCH64]),
        "--runtime-reference", "python-test",
        "--cache", str(project.scratch / "cache"),
        "--no-index",
        "--find-links", str(project.wheels),
        *extra,
    ]  # fmt: skip


def test_bd_t20_the_command_builds_and_prints_the_image(
    engine: Project, capsys: pytest.CaptureFixture[str]
) -> None:
    """BD-T20 moruna_build_prints_the_image: the digest of the image and each layer's digest
    and size, in text and as JSON, and the version of the Moruna that built it in the config."""
    assert build.main(command(engine, "--tag", "adder:1")) == build.EXIT_DONE
    text = capsys.readouterr().out
    layout = engine.scratch / "image"
    [entry] = json.loads((layout / "index.json").read_text())["manifests"]
    assert f"image     {entry['digest']}" in text and "reference adder:1" in text
    assert text.count("layer ") == 4
    assert build.main([*command(engine), "--json"]) == build.EXIT_DONE
    report = json.loads(capsys.readouterr().out)
    assert report["moruna_build"] == 1 and report["platform"] == AARCH64
    assert [layer["role"] for layer in report["layers"]] == [
        "runtime",
        "dependencies",
        "shim",
        "function",
    ]
    assert report["reference"] == "add:latest"
    manifest = json.loads(blob(layout, report["image"]))
    config = json.loads(blob(layout, manifest["config"]["digest"]))
    assert config["config"]["Labels"]["org.moruna.version"] == "9.9.9"


def test_bd_t20_the_command_refuses_in_a_sentence(
    engine: Project, capsys: pytest.CaptureFixture[str]
) -> None:
    (engine.root / "requirements.lock").unlink()
    assert build.main(command(engine)) == build.EXIT_REFUSED
    assert "moruna build: no lock:" in capsys.readouterr().err
    assert build.main(command(engine, kernel="kernel_a.py:missing")) == build.EXIT_REFUSED
    assert "kernel_a.py has no missing" in capsys.readouterr().err
    (engine.root / "broken.py").write_text("raise RuntimeError('does not load')\n")
    assert build.main(command(engine, kernel="broken.py:f")) == build.EXIT_REFUSED
    assert "RuntimeError: does not load" in capsys.readouterr().err
    assert build.main(command(engine, "--platform", "linux/s390x")) == build.EXIT_REFUSED
    assert "linux/aarch64 or linux/x86_64" in capsys.readouterr().err


# --- the transport ----------------------------------------------------------------------------


class _Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self) -> None:  # noqa: N802, the name http.server calls
        if self.path == "/ok":
            body = b"payload"
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif self.path == "/challenge":
            self.send_response(401)
            self.send_header("WWW-Authenticate", 'Bearer realm="https://auth.test/token"')
            self.send_header("Content-Length", "0")
            self.end_headers()
        else:
            self.send_response(307)
            self.send_header("Location", "/ok")
            self.send_header("Content-Length", "0")
            self.end_headers()

    def log_message(self, *args: Any) -> None:
        pass


def test_bd_t22_the_transport_returns_what_the_server_said_and_follows_nothing() -> None:
    """BD-T22: a status, lower case header names and the body; a redirect is returned, not
    followed (the registry follows it, dropping the credential)."""
    server = http.server.HTTPServer(("127.0.0.1", 0), _Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base = f"http://127.0.0.1:{server.server_port}"
    try:
        status, headers, body = build.urllib_transport(f"{base}/ok", {})
        assert (status, body.read()) == (200, b"payload")
        status, headers, _ = build.urllib_transport(f"{base}/challenge", {})
        assert status == 401 and headers["www-authenticate"].startswith("Bearer realm=")
        status, headers, _ = build.urllib_transport(f"{base}/elsewhere", {})
        assert (status, headers["location"]) == (307, "/ok")
        server.shutdown()
        server.server_close()
        with pytest.raises(build.Refused, match="Connection refused"):
            build.urllib_transport(f"{base}/ok", {})
    finally:
        server.server_close()
