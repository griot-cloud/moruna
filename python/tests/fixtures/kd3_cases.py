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
    return {"moruna_spec": 1, "run_id": uuid.uuid4().hex, "source": source,
            "kernels": kernels, "sink": sink, "contracts": contracts,
            "budget": {"memory_bytes": 768 << 20}, "ordered": True,
            "staging": {"dir": str(root / "staging" / name)},
            "report": {"file": str(root / "reports" / (name + ".json"))}}


def prepare(args):
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
    if not args.image.startswith("sha256:") or len(args.image) != 71:
        raise ValueError("actual attached image manifest digest required")
    if not args.image_layout.is_absolute() or not (args.image_layout / "index.json").is_file():
        raise ValueError("actual explicit OCI image layout required")
    root = args.root
    if not root.is_absolute() or root.exists():
        raise ValueError("explicit new absolute root required")
    root.mkdir(mode=0o700)
    for name in ("data", "governed", "cases", "outputs", "reports", "staging", "asset-source", "packages"):
        (root / name).mkdir()
    # Retain the real complete layout, at the SAME path on both adapters. The
    # canonical image loader, not this fixture, verifies reference association.
    layout_total = 0
    for source in sorted(args.image_layout.rglob("*")):
        if source.is_symlink():
            raise ValueError("OCI fixture layout must not contain symlinks")
        target = root / "image-layout" / source.relative_to(args.image_layout)
        if source.is_dir():
            target.mkdir(parents=True, exist_ok=True)
        elif source.is_file():
            layout_total += source.stat().st_size
            if layout_total > 512 << 20:
                raise ValueError("actual OCI fixture layout exceeds 512MiB")
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
            if file_sha(source) != file_sha(target):
                raise ValueError("copied actual OCI bytes differ")
        else:
            raise ValueError("OCI fixture contains nonregular data")
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
    if result.exit != "Completed":
        raise RuntimeError("actual Vortex preparation did not complete")
    caller = {"id": "kd3", "tenant": "kd3", "purpose": "conformance"}
    sources = {kind: {"kind": kind, "url": str(root / "data" / file)}
               for kind, file in (("arrow_ipc", "input.arrow"), ("parquet", "input.parquet"),
                                  ("vortex", "vortex"), ("tensor", "input.safetensors"))}
    sources["datafusion"] = {"kind": "datafusion", "root": str(root / "governed"),
                             "contract": "kd3/input", "caller": caller}
    kernels = {"std": {"kind": "std", "name": "select", "args": {"columns": ["x"]}},
               "image": {"kind": "image", "reference": args.image, "layout": str(root / "image-layout"),
                         "command": ["/kc1/bin/arrow_echo"]},
               "fixture.echo": {"kind": "fixture.echo"},
               "python": {"kind": "python", "module": str(root / "imported.py"), "callable": "identity"}}
    cells = []
    for case in matrix["cases"]:
        for adapter in ("linux", "native"):
            if case["runner"] == "python-library":
                for mode in ("rows", "adapter-refusal"):
                    cells.append({"case": case["id"], "adapter": adapter, "phase": "execute", "mode": mode,
                                  "runner": "python-library", "script": str(root / "kd3_library.py"),
                                  "matrix": str(root / "matrix.json"),
                                  "staging": str(root / "staging" / (case["id"] + adapter + mode))})
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
                if case["role"] == "kernel":
                    if case["kind"] == "package":
                        stage = dict(packages[case["id"].removeprefix("kernel.package.")])
                        if case["id"].endswith(".assets"):
                            stage.update(args={"fixture": "kd3-assets-v1"}, assets=str(root / "asset-source"))
                    else:
                        stage = dict(kernels[case["kind"]])
                    stages = [stage]
                    if case["id"] == "kernel.package.wasm":
                        # The checked-in real echo component declares n/s. Carry
                        # those real columns and retain x, then project the one
                        # canonical observable column after its genuine output.
                        source = {"kind": "arrow_ipc", "url": str(root / "data/echo.arrow")}
                        stages.append({"kind": "std", "name": "select", "args": {"columns": ["x"]}})
                document = job(source, stages, sink, root, name, contracts)
                fixture = {"case": case["id"], "phase": "execute", "mode": mode, "job": document}
                if adapter == "linux":
                    fixture.update(sandbox_python=args.sandbox_python, sandbox_cgroup=args.sandbox_cgroup)
                path = root / "cases" / (name + ".json")
                write(path, fixture)
                cells.append({"case": case["id"], "adapter": adapter, "phase": "execute", "mode": mode,
                              "fixture": str(path), "run_id": document["run_id"]})
            if case["role"] == "sink":
                cells.append({"case": case["id"], "adapter": adapter, "phase": "readback", "mode": "rows",
                              "emitted_with": str(root / "cases" / (case["id"] + "-" + adapter + "-rows.json")),
                              "requires": "actual sink ACK and canonical source readback within original admitted scope"})
    write(root / "cells.json", cells)
    print(json.dumps({"prepared": str(root), "cases": len(matrix["cases"]), "required_cells": len(cells),
                      "executed_matrix_cells": 0}))


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
    shutil.copyfile(HERE / "kd3_pex_assets.py", args.root / "pex-assets/function.py")
    (args.root / "pex-assets/function.yaml").write_text(
        "name: assets-identity\nentry: function:readonly_identity\ninput_schema: {x: int64}\n"
        "output_schema: {adds: {}}\nhints: {args: true}\n")
    print(json.dumps({"projects": str(args.root), "packaging": "use the canonical moruna package command; no package build performed"}))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    commands = p.add_subparsers(dest="command", required=True)
    a = commands.add_parser("projects")
    a.add_argument("--root", type=Path, required=True)
    a = commands.add_parser("prepare")
    a.add_argument("--matrix", type=Path, default=MATRIX)
    for name in ("root", "wasm", "pex", "pex-assets", "contracts", "image-layout"):
        a.add_argument("--" + name, type=Path, required=True)
    for name in ("image", "sandbox-python", "sandbox-cgroup"):
        a.add_argument("--" + name, required=True)
    args = p.parse_args()
    {"prepare": prepare, "projects": projects}[args.command](args)


if __name__ == "__main__":
    main()
