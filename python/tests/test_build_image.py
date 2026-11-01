"""BD-T24 to BD-T27 (SDD 30 k): a built image through the ``image`` kind, on Linux, for real.

``moruna build`` fetches ``python:3.14-slim`` by digest and installs ``pyarrow`` from the lock,
so this needs the network; the run needs a sandbox host (root, and a cgroup v2 directory with
``memory`` and ``cpu`` delegated), as the image kind's own Linux tests do (SDD 19 k). Each test
names what it needs and is skipped, saying so, when it is not there:

``MORUNA_SANDBOX_CGROUP``  the cgroup directory
``MORUNA_IMAGE_JOB``       the built ``image_job`` example (``cargo build -p moruna-job --example
                           image_job``), which writes the version 2 job document
``MORUNA_IMAGE_EXAMPLE``   the repository's ``examples/image_kernel`` directory
``MORUNA_LAYER_BIN``       the built ``moruna-layer`` (``cargo build -p moruna-image --bin
                           moruna-layer``), for the conversion test
"""

from __future__ import annotations

import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

CGROUP = os.environ.get("MORUNA_SANDBOX_CGROUP")
JOB = os.environ.get("MORUNA_IMAGE_JOB")
EXAMPLE = os.environ.get("MORUNA_IMAGE_EXAMPLE")
LAYER_BIN = os.environ.get("MORUNA_LAYER_BIN")
ROWS = 25_000

needs_linux_run = pytest.mark.skipif(
    sys.platform != "linux" or not (CGROUP and JOB and EXAMPLE),
    reason="Linux, MORUNA_SANDBOX_CGROUP, MORUNA_IMAGE_JOB and MORUNA_IMAGE_EXAMPLE",
)


def moruna(*args: str, cwd: Path | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-m", "moruna", *args],
        capture_output=True,
        text=True,
        check=False,
        cwd=cwd,
        timeout=1800,
    )


def build_image(source: Path, kernel: str, out: Path, tag: str) -> dict:
    """``moruna build`` as a user runs it; its JSON report."""
    done = moruna("build", f"{source}:{kernel}", "--out", str(out), "--tag", tag, "--json")
    assert done.returncode == 0, done.stderr
    return json.loads(done.stdout)


def run_image(work: Path, layout: Path, tag: str) -> subprocess.CompletedProcess[str]:
    """Parquet in, the image as the one kernel, Parquet out: the version 2 job and its run."""
    source = work / "in.parquet"
    if not source.exists():
        ids = pa.array(range(ROWS), pa.int64())
        pq.write_table(pa.table({"id": ids}), source, row_group_size=ROWS // 5)
    out = work / "out"
    shutil.rmtree(out, ignore_errors=True)
    out.mkdir()
    made = subprocess.run(
        [
            JOB,
            "--layout", str(layout),
            "--reference", tag,
            "--input", str(source),
            "--output", str(out),
            "--work", str(work / "run"),
            "--job", str(work / "job.mjb"),
        ],
        capture_output=True,
        text=True,
        check=False,
    )  # fmt: skip
    assert made.returncode == 0, made.stderr
    return moruna("run", str(work / "job.mjb"), "--sandbox-cgroup", str(CGROUP))


@needs_linux_run
def test_bd_t24_a_built_image_is_a_stage(tmp_path: Path) -> None:
    """BD-T24 a_built_image_runs_as_a_kernel: Parquet in, the function adds a column, Parquet
    out, every row checked, through the sandbox's namespaces, filter and read-only root."""
    layout = tmp_path / "image"
    report = build_image(Path(EXAMPLE) / "kernel.py", "add_doubled", layout, "add_doubled:1")
    assert [layer["role"] for layer in report["layers"][-3:]] == [
        "dependencies",
        "shim",
        "function",
    ]
    done = run_image(tmp_path, layout, "add_doubled:1")
    assert done.returncode == 0, done.stdout + done.stderr
    table = pa.concat_tables(pq.read_table(p) for p in sorted((tmp_path / "out").glob("*.parquet")))
    assert table.num_rows == ROWS
    ids = table.column("id").to_pylist()
    assert sorted(ids) == list(range(ROWS)), "every row, once"
    assert table.column("doubled").to_pylist() == [2 * i for i in ids], "each row's column"


@needs_linux_run
def test_bd_t25_a_function_that_raises_ends_the_run_with_its_traceback(tmp_path: Path) -> None:
    """BD-T25 a_failing_function_is_named: the shim's status 1 and its standard error reach
    the run's error (SDD 19 f.7)."""
    project = tmp_path / "project"
    project.mkdir()
    shutil.copy(Path(EXAMPLE) / "requirements.lock", project / "requirements.lock")
    (project / "fails.py").write_text(
        "def explode(batch):\n    raise ValueError('a row this function refuses')\n"
    )
    layout = tmp_path / "image"
    build_image(project / "fails.py", "explode", layout, "explode:1")
    done = run_image(tmp_path, layout, "explode:1")
    assert done.returncode != 0
    text = done.stdout + done.stderr
    assert "exited with status 1" in text, text
    assert "ValueError: a row this function refuses" in text, text


@needs_linux_run
def test_bd_t26_the_same_function_built_twice_is_one_image(tmp_path: Path) -> None:
    """BD-T26 reproducible_for_real: real wheels, the real runtime, two builds into two
    layouts, one by the command and one by ``moruna.build``: one image."""
    import builder_support as support  # noqa: PLC0415, the Linux proof alone needs it

    import moruna  # noqa: PLC0415

    one = build_image(Path(EXAMPLE) / "kernel.py", "add_doubled", tmp_path / "one", "a:1")
    module = support.load_path(Path(EXAMPLE) / "kernel.py", "kernel")
    built = moruna.build(module.add_doubled, out=tmp_path / "two", tag="a:1")
    assert one["image"] == built.digest
    assert [layer["digest"] for layer in one["layers"]] == [
        layer.blob.digest for layer in built.layers
    ]
    for name in ("oci-layout", "index.json"):
        assert (tmp_path / "one" / name).read_bytes() == (tmp_path / "two" / name).read_bytes()


@needs_linux_run
def test_bd_t26_two_functions_of_one_lock_share_layers_one_to_three(tmp_path: Path) -> None:
    project = tmp_path / "project"
    project.mkdir()
    shutil.copy(Path(EXAMPLE) / "requirements.lock", project / "requirements.lock")
    shutil.copy(Path(EXAMPLE) / "kernel.py", project / "first.py")
    (project / "second.py").write_text(
        "import pyarrow.compute as pc\n\n"
        "def triple(batch):\n"
        "    return batch.append_column('tripled', pc.multiply(batch.column('id'), 3))\n"
    )
    out = tmp_path / "shared"
    first = build_image(project / "first.py", "add_doubled", out, "first:1")
    second = build_image(project / "second.py", "triple", out, "second:1")
    digests = [[layer["digest"] for layer in r["layers"]] for r in (first, second)]
    assert digests[0][:-1] == digests[1][:-1], "every layer but the function's is one"
    assert digests[0][-1] != digests[1][-1]
    assert first["image"] != second["image"]


@pytest.mark.skipif(
    sys.platform != "linux" or not (EXAMPLE and LAYER_BIN),
    reason="Linux, MORUNA_IMAGE_EXAMPLE and MORUNA_LAYER_BIN",
)
def test_bd_t27_the_real_image_converts_to_layer_files(tmp_path: Path) -> None:
    """BD-T27 the_image_is_ready_for_the_kernel: KC2's converter (SDD 27) reads the built layout
    and writes a layer file for each of its layers."""
    layout = tmp_path / "image"
    report = build_image(Path(EXAMPLE) / "kernel.py", "add_doubled", layout, "add_doubled:1")
    done = subprocess.run(
        [str(LAYER_BIN), "convert", str(layout), "add_doubled:1", str(tmp_path / "layers")],
        capture_output=True,
        text=True,
        check=False,
    )
    assert done.returncode == 0, done.stderr
    lines = done.stdout.strip().splitlines()
    assert len(lines) == len(report["layers"]), done.stdout
    for line in lines:
        path = Path(line.split(" ", 4)[4])
        assert path.stat().st_size > 0 and path.suffix == ".layer"


@pytest.mark.skipif(
    sys.platform != "linux" or not EXAMPLE or os.geteuid() != 0 or shutil.which("chroot") is None,
    reason="Linux, root, chroot and MORUNA_IMAGE_EXAMPLE",
)
def test_bd_t29_the_images_python_uses_the_bytecode_it_ships(tmp_path: Path) -> None:
    """BD-T29 the_shipped_bytecode_is_the_bytecode_used: the image's own CPython imports pyarrow,
    the stand-in `moruna` and the kernel, every one from the .pyc beside its source, and
    writes nothing: a .pyc the interpreter did not accept (another magic number, a source it
    would compile) would be compiled and written, in a root that is writable here, and the
    tree under /opt and /app would change."""
    layout = tmp_path / "image"
    report = build_image(Path(EXAMPLE) / "kernel.py", "add_doubled", layout, "add_doubled:1")
    root = tmp_path / "root"
    root.mkdir()
    manifest = json.loads((layout / "blobs/sha256" / report["image"].split(":")[1]).read_bytes())
    for layer in manifest["layers"]:
        data = (layout / "blobs/sha256" / layer["digest"].split(":")[1]).read_bytes()
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:*") as archive:
            archive.extractall(root, filter="tar")

    def snapshot() -> dict[str, tuple[int, int]]:
        found = {}
        for top in ("opt", "app"):
            for path in sorted((root / top).rglob("*")):
                if path.is_file():
                    info = path.stat()
                    found[str(path.relative_to(root))] = (info.st_size, info.st_mtime_ns)
        return found

    before = snapshot()
    probe = (
        "import json, sys, pyarrow, kernel\n"
        "ours = ('/opt/moruna', '/app')\n"
        "cached = {m.__spec__.cached for m in list(sys.modules.values())\n"
        "          if getattr(m, '__spec__', None) and str(m.__spec__.origin).startswith(ours)\n"
        "          and m.__spec__.cached}\n"
        "print(json.dumps(sorted(cached)))\n"
    )
    env = {
        "PATH": "/usr/local/bin:/usr/bin:/bin",
        "LANG": "C.UTF-8",
        "PYTHONPATH": "/opt/moruna:/app:/opt/moruna/site-packages",
    }  # the image's, without PYTHONDONTWRITEBYTECODE: this interpreter may write if it wants to
    done = subprocess.run(
        ["chroot", str(root), "/usr/local/bin/python3", "-v", "-c", probe],
        capture_output=True,
        text=True,
        env=env,
        check=False,
        timeout=600,
    )
    assert done.returncode == 0, done.stderr[-3000:]
    cached = json.loads(done.stdout.strip().splitlines()[-1])
    used = set(re.findall(r"# code object from '([^']+\.pyc)'", done.stderr))
    used |= set(re.findall(r"# (\S+\.pyc) matches ", done.stderr))
    assert any("/pyarrow/__pycache__/__init__." in p for p in cached), cached
    assert any(p.startswith("/app/__pycache__/kernel.") for p in cached), cached
    assert any(p.startswith("/opt/moruna/moruna/__pycache__/") for p in cached), cached
    assert set(cached) <= used, f"not used from bytecode: {sorted(set(cached) - used)[:10]}"
    assert snapshot() == before, "nothing under /opt or /app was written"
