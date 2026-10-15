"""SDD 16 through the package: ``moruna package`` for a project (PK-T7 to PK-T9) and a component
(PK-T10), ``moruna check``, ``moruna aot`` and ``moruna load`` over a package (PK-T13, PK-T15),
and a loaded package in ``moruna.run`` (PY-T22).

Wheels are built here, for named platforms, and resolved from a local ``--find-links`` directory
with ``--no-index``: no test reaches a network, and none could pass by resolving for the machine
it runs on (PK-I5).
"""

from __future__ import annotations

import hashlib
import json
import os
import sys
import textwrap
import zipfile
from pathlib import Path

import pytest

from moruna import _package

X86 = "cp314-manylinux_2_28_x86_64"
ARM = "cp314-manylinux_2_28_aarch64"
HERE = f"cp3{sys.version_info.minor}"


def make_wheel(where: Path, name: str, version: str, tag: str, files: dict[str, str]) -> Path:
    """A wheel ``name-version-<tag>.whl`` holding ``files`` and its dist-info."""
    path = where / f"{name}-{version}-{tag}.whl"
    info = f"{name}-{version}.dist-info"
    with zipfile.ZipFile(path, "w") as archive:
        for member, text in files.items():
            archive.writestr(member, text)
        archive.writestr(
            f"{info}/METADATA", f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n"
        )
        archive.writestr(
            f"{info}/WHEEL",
            f"Wheel-Version: 1.0\nGenerator: test\nRoot-Is-Purelib: false\nTag: {tag}\n",
        )
        archive.writestr(f"{info}/RECORD", "")
    return path


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def project(where: Path, *, lockfile: str | None, yaml_extra: str = "") -> Path:
    """A project: ``function.py``, a helper module, ``function.yaml`` and the lockfile."""
    root = where / "project"
    root.mkdir()
    (root / "function.py").write_text(
        textwrap.dedent(
            """
            import helper

            def shout(batch):
                return helper.same(batch)
            """
        )
    )
    (root / "helper.py").write_text("def same(batch):\n    return batch\n")
    (root / ".hidden").write_text("not packaged\n")
    yaml = textwrap.dedent(
        """
        name: shout
        entry: function:shout
        input_schema: {text: string}
        output_schema: {adds: {}}
        hints: {preferred_rows: 1024}
        """
    )
    if lockfile is not None:
        (root / "function.lock").write_text(lockfile)
    (root / "function.yaml").write_text(yaml + yaml_extra)
    return root


@pytest.fixture
def wheels(scratch: Path) -> tuple[Path, str]:
    """Two requirements: ``alpha`` with a wheel per platform, ``beta`` pure Python with a
    ``.data`` directory; and the lockfile pinning both with their hashes."""
    links = scratch / "links"
    links.mkdir()
    alpha = [
        make_wheel(
            links,
            "alpha",
            "1.0",
            f"cp314-cp314-manylinux_2_28_{arch}",
            {"alpha/__init__.py": f"ARCH = {arch!r}\n"},
        )
        for arch in ("x86_64", "aarch64")
    ]
    beta = make_wheel(
        links,
        "beta",
        "2.0",
        "py3-none-any",
        {
            "beta.py": "B = 2\n",
            "beta-2.0.data/purelib/beta_extra.py": "E = 1\n",
            "beta-2.0.data/scripts/beta": "#!/bin/sh\n",
        },
    )
    lock = (
        "# a lockfile\n"
        "alpha==1.0 \\\n"
        + "".join(f"    --hash=sha256:{sha(w)} \\\n" for w in alpha[:-1])
        + f"    --hash=sha256:{sha(alpha[-1])}\n"
        f"beta==2.0 --hash=sha256:{sha(beta)}  # pure\n"
    )
    return links, lock


@pytest.mark.skipif(HERE != "cp314", reason="the default targets are cp314 (16 i)")
def test_pk_t7_package_a_project(scratch: Path, wheels: tuple[Path, str], capsys) -> None:
    links, lock = wheels
    root = project(scratch, lockfile=lock)
    out = scratch / "out"
    args = [str(root), "--out", str(out), "--no-index", "--find-links", str(links)]
    assert _package.main(args) == 0
    printed = capsys.readouterr()
    pex = out / "shout.pex"
    digest = hashlib.sha256(pex.read_bytes()).hexdigest()
    assert printed.out.strip() == f"{pex} sha256:{digest}"
    assert "beta-2.0.data/scripts/beta left out" in printed.err
    with zipfile.ZipFile(pex) as archive:
        names = set(archive.namelist())
        manifest = json.loads(archive.read("moruna.json"))
        infos = archive.infolist()
    assert manifest["name"] == "shout"
    assert manifest["entry"] == "function:shout"
    assert manifest["format"] == "pex"
    assert manifest["platforms"] == [ARM, X86]
    assert manifest["declaration"]["input_schema"] == {"text": "string"}
    assert manifest["declaration"]["hints"] == {"preferred_rows": 1024}
    assert "__main__.py" in names
    assert "src/function.py" in names and "src/helper.py" in names
    assert "src/.hidden" not in names and "src/function.yaml" not in names
    assert "src/function.lock" not in names
    tag = sys.implementation.cache_tag
    assert f"src/__pycache__/function.{tag}.pyc" in names
    for target, arch in ((X86, "x86_64"), (ARM, "aarch64")):
        assert f"deps/{target}/alpha/__init__.py" in names
        assert f"deps/{target}/alpha/__pycache__/__init__.{tag}.pyc" in names
        assert f"deps/{target}/beta.py" in names
        assert f"deps/{target}/beta_extra.py" in names
        assert f"deps/{target}/alpha-1.0.dist-info/METADATA" in names
        with zipfile.ZipFile(pex) as archive:
            assert arch in archive.read(f"deps/{target}/alpha/__init__.py").decode()
    assert not any("scripts" in n for n in names)
    assert all(i.date_time == (1980, 1, 1, 0, 0, 0) for i in infos)
    assert [i.filename for i in infos] == sorted(i.filename for i in infos)
    # PK-I6: the same project gives the same bytes.
    first = pex.read_bytes()
    assert _package.main(args) == 0
    assert pex.read_bytes() == first


def test_pk_t8_missing_wheel_refused(scratch: Path, capsys) -> None:
    links = scratch / "links"
    links.mkdir()
    only_x86 = make_wheel(
        links, "alpha", "1.0", f"{HERE}-{HERE}-manylinux_2_28_x86_64", {"alpha/__init__.py": ""}
    )
    root = project(scratch, lockfile=f"alpha==1.0 --hash=sha256:{sha(only_x86)}\n")
    code = _package.main(
        [
            str(root),
            "--platform",
            f"{HERE}-manylinux_2_28_x86_64",
            "--platform",
            f"{HERE}-manylinux_2_28_aarch64",
            "--no-index",
            "--find-links",
            str(links),
        ]
    )
    assert code == 2
    err = capsys.readouterr().err
    assert f"no wheel of alpha==1.0 for {HERE}-manylinux_2_28_aarch64" in err
    assert not (root / "shout.pex").exists()


@pytest.mark.parametrize(
    ("lockfile", "said"),
    [
        ("alpha\n", "is not name==version"),
        ("alpha>=1.0 --hash=sha256:" + "0" * 64 + "\n", "is not name==version"),
        ("https://example.com/alpha.whl\n", "is not name==version"),
        ("alpha==1.0\n", "has no --hash"),
        ("alpha==1.0 --hash=md5:00\n", "is not --hash=sha256"),
        ("-e ./alpha\n", "is not name==version"),
    ],
)
def test_pk_t9_lockfile_rules(scratch: Path, lockfile: str, said: str, capsys) -> None:
    root = project(scratch, lockfile=lockfile)
    assert _package.main([str(root), "--platform", f"{HERE}-manylinux_2_28_x86_64"]) == 2
    err = capsys.readouterr().err
    assert said in err
    assert "function.lock:1" in err


@pytest.mark.parametrize(
    ("target", "said"),
    [
        (f"{HERE}t-manylinux_2_28_x86_64", "a .pex target is cp3NN-manylinux"),
        (f"{HERE}-macosx_14_0_arm64", "a .pex target is cp3NN-manylinux"),
        (
            "cp313-manylinux_2_28_x86_64" if HERE != "cp313" else "cp315-manylinux_2_28_x86_64",
            "its bytecode needs Python",
        ),
    ],
)
def test_pk_t9_targets_refused(scratch: Path, target: str, said: str, capsys) -> None:
    root = project(scratch, lockfile=None)
    assert _package.main([str(root), "--platform", target]) == 2
    assert said in capsys.readouterr().err


def test_pk_t9_function_yaml_rules(scratch: Path, capsys) -> None:
    root = project(scratch, lockfile=None, yaml_extra="colour: blue\n")
    assert _package.main([str(root), "--platform", f"{HERE}-manylinux_2_28_x86_64"]) == 2
    assert "colour: not a function.yaml key" in capsys.readouterr().err
    (root / "function.yaml").write_text("name: shout\n")
    assert _package.main([str(root), "--platform", f"{HERE}-manylinux_2_28_x86_64"]) == 2
    assert "entry: required for a .pex" in capsys.readouterr().err
    (root / "function.yaml").write_text("name: 9shout\nentry: function:shout\n")
    assert _package.main([str(root), "--platform", f"{HERE}-manylinux_2_28_x86_64"]) == 2
    assert "package: name:" in capsys.readouterr().err
    (root / "function.yaml").write_text("- a list\n")
    assert _package.main([str(root)]) == 2
    assert "must be a mapping" in capsys.readouterr().err
    (root / "function.yaml").write_text("name: [unclosed\n")
    assert _package.main([str(root)]) == 2
    assert "not YAML" in capsys.readouterr().err
    (root / "function.yaml").unlink()
    assert _package.main([str(root)]) == 2
    assert "a project has a function.yaml" in capsys.readouterr().err


def test_pk_t9_no_lockfile_needs_no_pip(scratch: Path, capsys) -> None:
    root = project(scratch, lockfile=None)
    target = f"{HERE}-manylinux_2_28_x86_64"
    assert _package.main([str(root), "--platform", target, "--out", str(root)]) == 0
    with zipfile.ZipFile(root / "shout.pex") as archive:
        names = archive.namelist()
        manifest = json.loads(archive.read("moruna.json"))
    assert manifest["platforms"] == [target]
    assert not any(n.startswith("deps/") for n in names)
    capsys.readouterr()


FIXTURES = Path(__file__).parent / "fixtures"

ECHO_YAML = """
name: echo
input_schema: {n: int64, s: string}
output_schema: {adds: {}}
"""


def wasm_project(where: Path, yaml: str = ECHO_YAML) -> Path:
    root = where / "wasm-project"
    root.mkdir()
    (root / "function.yaml").write_text(yaml)
    return root


def test_pk_t10_package_a_component(scratch: Path, capsys) -> None:
    root = wasm_project(scratch)
    component = FIXTURES / "echo.component.wasm"
    assert _package.main([str(root), "--wasm", str(component), "--out", str(scratch)]) == 0
    out = scratch / "echo.wasm"
    digest = hashlib.sha256(out.read_bytes()).hexdigest()
    assert capsys.readouterr().out.strip() == f"{out} sha256:{digest}"
    # The same component and function.yaml give the same bytes (PK-I6).
    first = out.read_bytes()
    assert _package.main([str(root), "--wasm", str(component), "--out", str(scratch)]) == 0
    assert out.read_bytes() == first
    capsys.readouterr()


def test_pk_t10_disagreement_refused(scratch: Path, capsys) -> None:
    component = str(FIXTURES / "echo.component.wasm")
    root = wasm_project(scratch, ECHO_YAML.replace("n: int64", "n: int32"))
    assert _package.main([str(root), "--wasm", component]) == 2
    assert "declaration" in capsys.readouterr().err
    (root / "function.lock").write_text("")
    assert _package.main([str(root), "--wasm", component]) == 2
    assert "a component links its dependencies" in capsys.readouterr().err
    (root / "function.lock").unlink()
    (root / "function.yaml").write_text(ECHO_YAML + "lockfile: x.lock\n")
    assert _package.main([str(root), "--wasm", component]) == 2
    assert "lockfile: not a function.yaml key" in capsys.readouterr().err
    (root / "function.yaml").write_text(ECHO_YAML + "entry: function:f\n")
    assert _package.main([str(root), "--wasm", component]) == 2
    assert "entry is moruna:kernel" in capsys.readouterr().err
    (root / "function.yaml").write_text(ECHO_YAML)
    assert _package.main([str(root), "--wasm", str(scratch / "missing.wasm")]) == 2
    assert "no such component" in capsys.readouterr().err
    (scratch / "junk.wasm").write_bytes(b"junk")
    assert _package.main([str(root), "--wasm", str(scratch / "junk.wasm")]) == 2
    assert "not a valid component" in capsys.readouterr().err


def test_pk_t13_check_a_package(scratch: Path, capsys) -> None:
    from moruna import _check  # noqa: PLC0415

    root = wasm_project(scratch)
    assert (
        _package.main(
            [str(root), "--wasm", str(FIXTURES / "echo.component.wasm"), "--out", str(root)]
        )
        == 0
    )
    package = root / "echo.wasm"
    capsys.readouterr()
    code = _check.main([str(package), "--json", "--profiles-dir", str(scratch / "profiles")])
    report = json.loads(capsys.readouterr().out)
    assert code == 0, report
    (kernel,) = report["kernels"]
    assert kernel["kind"] == "wasm"
    assert kernel["verdict"] == "agreed"
    assert kernel["fingerprint"] == "sha256:" + hashlib.sha256(package.read_bytes()).hexdigest()
    assert kernel["alloc"]["linear"]["requests"] >= 0
    # A package that loops runs out of fuel and is refused, not hung.
    (scratch / "l").mkdir()
    loops = wasm_project(scratch / "l", ECHO_YAML.replace("name: echo", "name: loops"))
    loops_component = str(FIXTURES / "loops.component.wasm")
    assert _package.main([str(loops), "--wasm", loops_component, "--out", str(loops)]) == 0
    capsys.readouterr()
    assert _check.main([str(loops / "loops.wasm"), "--no-profile"]) == 2
    assert "fuel exhausted" in capsys.readouterr().out
    # A .pex on this host: refused when the sandbox cannot run it, named.
    assert _check.main([str(scratch / "nothing.pex"), "--no-profile"]) == 2
    capsys.readouterr()


def test_pk_t15_aot(scratch: Path, capsys) -> None:
    from moruna import __main__ as command  # noqa: PLC0415

    root = wasm_project(scratch)
    component = str(FIXTURES / "echo.component.wasm")
    assert _package.main([str(root), "--wasm", component, "--out", str(root)]) == 0
    capsys.readouterr()
    assert command.main(["aot", str(root / "echo.wasm")]) == 0
    assert capsys.readouterr().out.strip() == str(root / "echo.cwasm")
    assert (root / "echo.cwasm").read_bytes()[:12] == b"MORUNA-CWASM"
    assert command.main(["aot"]) == 2
    assert command.main(["aot", str(scratch / "missing.wasm")]) == 2
    capsys.readouterr()


def _echo_package(scratch: Path) -> Path:
    root = wasm_project(scratch)
    component = str(FIXTURES / "echo.component.wasm")
    assert _package.main([str(root), "--wasm", component, "--out", str(root)]) == 0
    return root / "echo.wasm"


def test_pk_t15_load_and_call(scratch: Path, capsys) -> None:
    import pyarrow as pa  # noqa: PLC0415

    import moruna  # noqa: PLC0415

    package = _echo_package(scratch)
    capsys.readouterr()
    kernel = moruna.load(str(package))
    assert kernel.format == "wasm"
    assert kernel.path == str(package)
    assert kernel.fingerprint == "sha256:" + hashlib.sha256(package.read_bytes()).hexdigest()
    assert kernel.manifest["name"] == "echo"
    assert "echo.wasm" in repr(kernel)
    batch = pa.record_batch({"n": [1, 2, 3], "s": ["a", "b", "c"]})
    assert kernel(batch).equals(batch)
    with pytest.raises(TypeError):
        kernel("not a batch")


def test_py_t22_run_a_loaded_package(scratch: Path, dataset, capsys) -> None:
    import pyarrow.parquet as pq  # noqa: PLC0415

    import moruna  # noqa: PLC0415

    src, out, rows = dataset
    package = _echo_package(scratch)
    capsys.readouterr()
    loaded = moruna.load(str(package))
    report = moruna.run(moruna.ParquetSource(src), [loaded], moruna.ParquetSink(out))
    written = pq.read_table(out.removeprefix("file://"))
    assert written.num_rows == rows
    stages = json.loads(report.to_json())["stages"]
    (stage,) = [s for s in stages if s["stage"] == 1]
    assert stage["rows_in"] == rows
    assert stage["alloc"]["linear"] is not None


@pytest.mark.skipif(sys.platform == "linux", reason="off Linux the sandbox does not exist")
def test_py_t22_a_pex_off_linux_is_refused(scratch: Path) -> None:
    import moruna  # noqa: PLC0415

    target = sys.platform
    pex = scratch / "x.pex"
    with zipfile.ZipFile(pex, "w") as archive:
        archive.writestr(
            "moruna.json",
            json.dumps(
                {
                    "name": "x",
                    "entry": "function:x",
                    "declaration": {"hints": {}},
                    "abi": 1,
                    "moruna": "0",
                    "format": "pex",
                    "platforms": ["cp314-manylinux_2_28_x86_64"],
                }
            ),
        )
    with pytest.raises(moruna.MorunaError, match="needs Linux"):
        moruna.load(str(pex), python=sys.executable, cgroup="/sys/fs/cgroup/x")
    assert target != "linux"


SANDBOX_PYTHON = os.environ.get("MORUNA_SANDBOX_PYTHON")
SANDBOX_CGROUP = os.environ.get("MORUNA_SANDBOX_CGROUP")
needs_sandbox = pytest.mark.skipif(
    sys.platform != "linux" or not SANDBOX_PYTHON or not SANDBOX_CGROUP,
    reason="Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP",
)


@needs_sandbox
def test_pk_t15_load_a_pex(scratch: Path, dataset, capsys) -> None:
    import pyarrow as pa  # noqa: PLC0415
    import pyarrow.parquet as pq  # noqa: PLC0415

    import moruna  # noqa: PLC0415

    root = project(scratch, lockfile=None)
    (root / "function.py").write_text(
        "import pyarrow.compute as pc\n\n"
        "def shout(batch):\n"
        "    return batch.append_column('loud', pc.utf8_upper(batch.column('text')))\n"
    )
    (root / "function.yaml").write_text(
        "name: shout\nentry: function:shout\ninput_schema: {text: string}\n"
        "output_schema: {adds: {loud: string}}\n"
    )
    machine = os.uname().machine
    target = f"{HERE}-manylinux_2_28_{machine}"
    assert _package.main([str(root), "--platform", target, "--out", str(root)]) == 0
    capsys.readouterr()
    pex = root / "shout.pex"
    kernel = moruna.load(str(pex), python=SANDBOX_PYTHON, cgroup=SANDBOX_CGROUP)
    assert kernel.format == "pex"
    out = kernel(pa.record_batch({"text": ["a", "b"]}))
    assert out.column("loud").to_pylist() == ["A", "B"]
    # The same file in a run: the run opens it again under its own child.
    src, sink, rows = dataset
    moruna.run(moruna.ParquetSource(src), [kernel], moruna.ParquetSink(sink))
    written = pq.read_table(sink.removeprefix("file://"))
    assert written.num_rows == rows
    assert "loud" in written.column_names
    # `moruna aot` writes the tree beside the package; `check` agrees through the child.
    from moruna import __main__ as command  # noqa: PLC0415
    from moruna import _check  # noqa: PLC0415

    assert command.main(["aot", str(pex)]) == 0
    assert (root / "shout.pex.tree" / "MORUNA-TREE").is_file()
    capsys.readouterr()
    args = [str(pex), "--json", "--no-profile", "--sandbox-python", SANDBOX_PYTHON]
    assert _check.main([*args, "--sandbox-cgroup", SANDBOX_CGROUP]) == 0
    (report,) = json.loads(capsys.readouterr().out)["kernels"]
    assert report["kind"] == "pex" and report["verdict"] == "agreed"


def test_pk_t18_package_report(scratch: Path, capsys, monkeypatch) -> None:
    """16 d.2: the package goes to the working directory by default, and --json reports it."""
    from moruna import _check  # noqa: PLC0415

    root = wasm_project(scratch)
    here = scratch / "here"
    here.mkdir()
    monkeypatch.chdir(here)
    component = str(FIXTURES / "echo.component.wasm")
    assert _package.main([str(root), "--wasm", component, "--json"]) == 0
    report = json.loads(capsys.readouterr().out)
    out = here / "echo.wasm"
    assert Path(report["path"]).is_absolute()
    assert Path(report["path"]).resolve() == out.resolve()
    assert report["name"] == "echo" and report["format"] == "wasm"
    assert report["fingerprint"] == "sha256:" + hashlib.sha256(out.read_bytes()).hexdigest()
    assert report["manifest"]["declaration"]["input_schema"] == {"n": "int64", "s": "string"}
    assert _check.main([str(out), "--json", "--no-profile"]) == 0
    (checked,) = json.loads(capsys.readouterr().out)["kernels"]
    assert checked["manifest"] == report["manifest"]
