"""Author real KD3 inputs and all canonical document/library fixture cells.

Run in the prepared Linux Python environment with Moruna/PyArrow already installed.
Packages must come from the canonical packager; contracts are actual compiled
ContractEntry JSON from the existing peQL compiler. No assets are downloaded or
installed. Preserve this exact absolute root in the native image: paths and refusal
sentences must agree across adapters. Sink readback runs in the original admitted process over its actual ACK files.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import uuid

HERE = Path(__file__).resolve().parent
MATRIX = HERE.parents[2] / "crates/moruna-py/tests/fixtures/kd3-matrix.json"


def file_sha(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def write(path, value):
    with path.open("x") as out:
        json.dump(value, out, indent=2)
        out.write("\n")


def job(source, kernels, sink, root, name, contracts):
    run = uuid.uuid4().hex
    writable = Path("/tmp/kd3") / run
    source, sink = dict(source), dict(sink)
    if source["kind"] == "datafusion":
        source["root"] = str(writable / "governed")
    if sink["kind"] == "peql":
        sink["root"] = str(writable / "governed")
    else:
        sink["url"] = str(writable / "output")
    document = {"moruna_spec": 1, "run_id": run, "source": source,
                "kernels": kernels, "sink": sink,
                "budget": {"memory_bytes": 768 << 20}, "ordered": True,
                "staging": {"dir": str(writable / "staging")},
                "report": {"file": str(writable / "report.json")}}
    # Only these actual matrix kinds consume governed contracts (KD-I8).
    # Image/package/std and ordinary file/tensor IO must not carry unused authority.
    if source["kind"] == "datafusion" or sink["kind"] == "peql":
        document["contracts"] = contracts
    return document


def assets(args):
    import moruna
    import pyarrow as pa
    import pyarrow.parquet as pq
    matrix = json.loads(args.matrix.read_text())
    contracts = json.loads(args.contracts.read_text())
    if not isinstance(contracts, list) or len(contracts) != 2:
        raise ValueError("supply actual compiled kd3/input and kd3/output ContractEntry objects")
    packages = {}
    for name, path in (("wasm", args.wasm), ("pex", args.pex), ("pex.assets", args.pex_assets)):
        if not path.is_absolute() or not path.is_file():
            raise ValueError(f"missing actual canonical package: {name}")
        packages[name] = {"kind": "package", "path": str(path),
                          "fingerprint": "sha256:" + file_sha(path)}
    root = args.root
    if not root.is_absolute() or root.exists():
        raise ValueError("explicit new absolute root required")
    root.mkdir(mode=0o700)
    for name in ("data", "governed", "cases", "outputs", "reports", "staging", "asset-source", "packages"):
        (root / name).mkdir()
    shutil.copyfile(args.matrix, root / "matrix.json")
    shutil.copyfile(HERE / "kd3_library.py", root / "kd3_library.py")
    for name, package in packages.items():
        original = Path(package["path"])
        target = root / "packages" / original.name
        if target.exists():
            raise ValueError("package filenames collide")
        shutil.copyfile(original, target)
        if "sha256:" + file_sha(target) != package["fingerprint"]:
            raise ValueError("copied actual package bytes differ")
        package["path"] = str(target)
    (root / "asset-source/nested").mkdir()
    (root / "asset-source/weights.txt").write_bytes(b"kd3-real-assets-v1")
    (root / "imported.py").write_text("def identity(batch):\n    return batch\n")
    schema = pa.schema([pa.field("x", pa.int64(), nullable=False)])
    batch = pa.RecordBatch.from_arrays([pa.array(matrix["rows"], type=pa.int64())], schema=schema)
    with (root / "data/input.arrow").open("xb") as out:
        with pa.ipc.new_file(out, schema) as writer:
            writer.write_batch(batch)
    echo_schema = pa.schema([pa.field("x", pa.int64(), nullable=False),
                             pa.field("n", pa.int64(), nullable=False),
                             pa.field("s", pa.string(), nullable=False)])
    echo_batch = pa.RecordBatch.from_arrays(
        [batch.column(0), batch.column(0), pa.array([""] * len(matrix["rows"]))], schema=echo_schema)
    with (root / "data/echo.arrow").open("xb") as out:
        with pa.ipc.new_file(out, echo_schema) as writer:
            writer.write_batch(echo_batch)
    pq.write_table(pa.Table.from_batches([batch]), root / "data/input.parquet")
    shutil.copyfile(root / "data/input.parquet", root / "governed/input.parquet")
    # Same tiny standards-compliant writer used by test_tensor, with actual int64
    # row bytes. Runtime's canonical tensor source is the only decoder.
    raw = struct.pack("<" + "q" * len(matrix["rows"]), *matrix["rows"])
    header = json.dumps({"x": {"dtype": "I64", "shape": [len(matrix["rows"]), 1],
                               "data_offsets": [0, len(raw)]}}).encode()
    # Native typed tensor mappings require an aligned int64 data offset.
    header += b" " * (-len(header) % 8)
    with (root / "data/input.safetensors").open("xb") as out:
        out.write(struct.pack("<Q", len(header)))
        out.write(header)
        out.write(raw)
    # Produce genuine Vortex via the public adapter, not a renamed Arrow file.
    result = moruna.run(moruna.ArrowIpcSource(str(root / "data/input.arrow")), [],
                        moruna.VortexSink(str(root / "data/vortex")),
                        budget=768 << 20, staging_dir=str(root / "staging/prepare-vortex"),
                        staging_limit=128 << 20, checkpoint=False)
    if result.exit["status"] != "completed":
        raise RuntimeError("actual Vortex preparation did not complete")
    write(root / "contracts.json", contracts)
    write(root / "packages.json", packages)
    print(json.dumps({"prepared_assets": str(root), "executed_matrix_cells": 0}))


def cases(args):
    root = args.root
    matrix = json.loads((root / "matrix.json").read_text())
    contracts = json.loads((root / "contracts.json").read_text())
    packages = json.loads((root / "packages.json").read_text())
    for digest, layout in ((args.image, args.image_layout),
                           (args.linux_image, args.linux_image_layout)):
        if not digest.startswith("sha256:") or len(digest) != 71:
            raise ValueError("actual architecture-specific derived image digest required")
        if not layout.is_absolute() or not (layout / "index.json").is_file():
            raise ValueError("actual external architecture-specific OCI layout required")
    out = args.output.absolute()
    if out.exists() or out == root or out.is_relative_to(root):
        raise ValueError("private cases must be NEW and outside immutable OCI inputs")
    out.mkdir(mode=0o700)
    for directory in ("cases", "jobs", "commands"):
        (out / directory).mkdir()
    caller = {"id": "kd3", "tenant": "kd3", "purpose": "conformance"}
    sources = {kind: {"kind": kind, "url": str(root / "data" / file)}
               for kind, file in (("arrow_ipc", "input.arrow"), ("parquet", "input.parquet"),
                                  ("vortex", "vortex"), ("tensor", "input.safetensors"))}
    sources["datafusion"] = {"kind": "datafusion", "root": str(root / "governed"),
                             "contract": "kd3/input", "caller": caller}
    kernels = {"std": {"kind": "std", "name": "select", "args": {"columns": ["x"]}},
               "image": {"kind": "image",
                         "image": {"build_key": None, "digest": args.image,
                                   "location": str(args.image_layout)},
                         "command": ["/kc1/bin/arrow_echo"]},
               "fixture.echo": {"kind": "fixture.echo"}}
    cells = []
    for case in matrix["cases"]:
        for adapter in ("linux", "native"):
            if case["runner"] == "python-library":
                for mode in ("rows", "adapter-refusal"):
                    run = uuid.uuid4().hex
                    name = case["id"] + "-" + adapter + "-" + mode
                    staging = str(Path("/tmp/kd3") / run / "staging")
                    identity = {"moruna_spec": 1, "run_id": run,
                                "source": {"kind": "iterator" if case["id"] == "library.iterator" else "image"},
                                "kernels": ([{"kind": "image", "entry": "__main__:identity"}]
                                            if case["id"] == "library.image.kernel" else []),
                                "sink": {"kind": "image"}, "budget": {"memory_bytes": 384 << 20, "cpu": 1.0},
                                "staging": {"dir": staging, "limit_bytes": 128 << 20}, "ordered": True,
                                "checkpoint": {"enabled": False}, "allow_gil": True}
                    job_file = "jobs/" + name + ".json"
                    write(out / job_file, identity)
                    cell = {"case": case["id"], "adapter": adapter, "phase": "execute", "mode": mode,
                            "runner": "python-library", "script": str(root / "kd3_library.py"),
                            "matrix": str(root / "matrix.json"), "run_id": run, "staging": staging,
                            "job": job_file, "admission": "actual caller-role/run declaration; opaque Python objects supplied in process"}
                    if adapter == "native":
                        command_file = "commands/" + name + ".json"
                        write(out / command_file, {"program": "/opt/python-std/bin/python3",
                              "args": [str(root / "kd3_library.py"), str(root / "matrix.json"), case["id"], adapter, staging, mode],
                              "nonce": uuid.uuid4().hex})
                        cell["command"] = command_file
                    cells.append(cell)
                continue
            modes = ("rows", "unknown-setting") if case["expect"] == "rows" else ("rows",)
            for mode in modes:
                name = case["id"] + "-" + adapter + "-" + mode
                source = dict(sources[case["kind"]] if case["role"] == "source" else sources["arrow_ipc"])
                sink = {"kind": "arrow_ipc", "url": str(root / "outputs" / name)}
                if case["id"] == "source.tensor":
                    sink["kind"] = "tensor"
                stages = []
                if case["role"] == "sink":
                    sink["kind"] = case["kind"]
                    if case["kind"] == "tensor":
                        source = dict(sources["tensor"])
                    if case["kind"] == "peql":
                        sink = {"kind": "peql", "root": str(root / "governed"), "contract": "kd3/output",
                                "caller": caller, "mode": "overwrite"}
                if case["role"] == "function":
                    if case["kind"] == "package":
                        stage = dict(packages[case["id"].removeprefix("kernel.package.")])
                        if case["id"].endswith(".assets"):
                            stage.update(args={"fixture": "kd3-assets-v1"}, assets=str(root / "asset-source"))
                    else:
                        stage = dict(kernels[case["kind"]])
                    if case["kind"] == "image" and adapter == "linux":
                        stage.update(reference=args.linux_image, layout=str(args.linux_image_layout))
                    stages = [stage]
                    if case["id"] == "kernel.package.wasm":
                        # The checked-in real echo component declares n/s. Carry
                        # those real columns and retain x, then project the one
                        # canonical observable column after its genuine output.
                        source = {"kind": "arrow_ipc", "url": str(root / "data/echo.arrow")}
                        stages.append({"kind": "std", "name": "select", "args": {"columns": ["x"]}})
                    if case["id"] == "kernel.image":
                        # The canonical arrow_echo program adds its real n column.
                        stages.append({"kind": "std", "name": "select", "args": {"columns": ["x"]}})
                document = job(source, stages, sink, root, name, contracts)
                fixture = {"case": case["id"], "phase": "execute", "mode": mode, "job": document}
                if case["kind"] in ("datafusion", "peql"):
                    fixture["governed_input"] = str(root / "governed/input.parquet")
                if adapter == "linux":
                    fixture.update(sandbox_python=args.sandbox_python, sandbox_cgroup=args.sandbox_cgroup)
                elif case["id"].startswith("kernel.package.pex"):
                    # NativePex requires an explicit real interpreter from the
                    # attached image; the preparation overlay installs this
                    # pinned standard CPython here. Never pass Linux cgroups.
                    fixture["sandbox_python"] = "/opt/python-std/bin/python3"
                path = out / "cases" / (name + ".json")
                write(path, fixture)
                job_file = "jobs/" + name + ".json"
                write(out / job_file, document)
                cell = {"case": case["id"], "adapter": adapter, "phase": "execute", "mode": mode,
                        "fixture": "cases/" + name + ".json", "run_id": document["run_id"], "job": job_file}
                if adapter == "native":
                    command_file = "commands/" + name + ".json"
                    write(out / command_file, {"program": "/moruna/bin/moruna-kd3-fixture",
                          "args": ["run-grant", "kd3.case", "native"], "nonce": uuid.uuid4().hex})
                    cell["command"] = command_file
                cells.append(cell)
            if case["role"] == "sink":
                cells.append({"case": case["id"], "adapter": adapter, "phase": "readback", "mode": "rows",
                              "emitted_with": "cases/" + case["id"] + "-" + adapter + "-rows.json",
                              "requires": "actual sink ACK and canonical source readback within original admitted scope"})
    write(out / "cells.json", cells)
    print(json.dumps({"prepared": str(out), "cases": len(matrix["cases"]), "required_cells": len(cells),
                      "executed_matrix_cells": 0}))


def linux(args):
    """Run the authored real Linux cells, retaining raw process output separately."""
    prepared = args.prepared.resolve(strict=True)
    cells = json.loads((prepared / "cells.json").read_text())
    selected = [cell for cell in cells if cell["adapter"] == "linux" and cell["phase"] == "execute"]
    if len(selected) != 41 or args.output.exists():
        raise ValueError("exact 41 authored Linux executions and NEW output required")
    args.output.mkdir(mode=0o700)
    results, logs = args.output / "results", args.output / "logs"
    results.mkdir()
    logs.mkdir()
    for index, cell in enumerate(selected):
        if cell.get("runner") == "python-library":
            command = [sys.executable, cell["script"], str(args.assets / "matrix.json"),
                       cell["case"], "linux", cell["staging"], cell["mode"]]
        else:
            command = [str(args.fixture), "run", str(prepared / cell["fixture"]), "linux"]
        name = f"{index:02d}-{cell['case']}-{cell['mode']}"
        with (logs / (name + ".stdout")).open("xb") as stdout, (logs / (name + ".stderr")).open("xb") as stderr:
            subprocess.run(command, stdout=stdout, stderr=stderr, timeout=args.seconds, check=True)
        value = json.loads((logs / (name + ".stdout")).read_text())
        receipts = value["receipts"] if "receipts" in value else [value]
        expected = {(cell["case"], "linux", "execute", cell["mode"])}
        if any(other.get("emitted_with") == cell.get("fixture") and other["adapter"] == "linux"
               for other in cells if "fixture" in cell):
            expected.add((cell["case"], "linux", "readback", "rows"))
        actual = [tuple(record[key] for key in ("case", "adapter", "phase", "mode")) for record in receipts]
        if len(actual) != len(expected) or set(actual) != expected:
            raise ValueError(f"actual Linux receipt identities differ: {name}: {actual}")
        for record in receipts:
            write(results / ("-".join(record[key] for key in ("case", "adapter", "phase", "mode")) + ".json"), record)
    if len(list(results.glob("*.json"))) != 46:
        raise ValueError("actual Linux execution did not produce all 46 required receipts")
    print(json.dumps({"executed_processes": 41, "actual_phase_receipts": 46, "results": str(results)}))


def projects(args):
    if not args.root.is_absolute() or args.root.exists():
        raise ValueError("new absolute package-project root required")
    args.root.mkdir(mode=0o700)
    for name in ("wasm", "pex", "pex-assets"):
        (args.root / name).mkdir()
    (args.root / "wasm/function.yaml").write_text(
        "name: echo\ninput_schema: {n: int64, s: string}\noutput_schema: {adds: {}}\n")
    (args.root / "pex/function.py").write_text("def identity(batch):\n    return batch\n")
    (args.root / "pex/function.yaml").write_text(
        "name: identity\nentry: function:identity\ninput_schema: {x: int64}\noutput_schema: {adds: {}}\n")
    shutil.copyfile(HERE.parents[2] / "crates/moruna-py/tests/fixtures/kd3_pex_assets.py",
                    args.root / "pex-assets/function.py")
    (args.root / "pex-assets/function.yaml").write_text(
        "name: assets_identity\nentry: function:readonly_identity\ninput_schema: {x: int64}\n"
        "output_schema: {adds: {}}\nhints: {args: true}\n")
    print(json.dumps({"projects": str(args.root), "packaging": "use the canonical moruna package command; no package build performed"}))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    commands = p.add_subparsers(dest="command", required=True)
    a = commands.add_parser("projects")
    a.add_argument("--root", type=Path, required=True)
    a = commands.add_parser("assets")
    a.add_argument("--matrix", type=Path, default=MATRIX)
    for name in ("root", "wasm", "pex", "pex-assets", "contracts"):
        a.add_argument("--" + name, type=Path, required=True)
    a = commands.add_parser("cases")
    for name in ("root", "output", "image-layout", "linux-image-layout"):
        a.add_argument("--" + name, type=Path, required=True)
    for name in ("image", "linux-image", "sandbox-python", "sandbox-cgroup"):
        a.add_argument("--" + name, required=True)
    a = commands.add_parser("linux")
    for name in ("prepared", "assets", "fixture", "output"):
        a.add_argument("--" + name, type=Path, required=True)
    a.add_argument("--seconds", type=int, default=180)
    args = p.parse_args()
    {"assets": assets, "cases": cases, "projects": projects, "linux": linux}[args.command](args)


if __name__ == "__main__":
    main()
