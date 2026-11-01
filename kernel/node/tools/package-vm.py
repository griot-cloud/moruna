#!/usr/bin/env python3
"""Copy an explicit direct-VM descriptor into a new PRIVATE artifact bundle.

This is not a release uploader, descriptor validator, provisioner or launcher.
Credential-bearing initramfs/descriptors MUST remain private. Public releases
must instead select safe immutable binaries/layers and setup templates separately.
The actual Rust SDK inspect/snapshot validation and fresh authority setup remain
mandatory; this command never claims KVM or execution acceptance.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat

MAX_BYTES = 16 * 1024**3


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def confined(root, relative):
    path = Path(relative)
    if path.is_absolute() or not path.parts or any(p in (".", "..") for p in path.parts):
        raise ValueError("artifact path must be confined and relative")
    cursor = root
    for part in path.parts:
        cursor = cursor / part
        if cursor.is_symlink():
            raise ValueError("symlink input refused")
    info = cursor.stat()
    if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= MAX_BYTES:
        raise ValueError("artifact must be a bounded nonempty regular file")
    return cursor


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--descriptor", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--source-bundle", type=Path, required=True)
    parser.add_argument("--receipt", type=Path, action="append", required=True)
    parser.add_argument("--binary", type=Path, action="append", default=[],
                        help="explicit safe host binary, copied to bin/ preserving executable mode")
    parser.add_argument("--private-proof-assets", action="store_true", required=True,
                        help="acknowledge this credential-bearing bundle MUST NOT be uploaded")
    args = parser.parse_args()
    os.umask(0o077)
    descriptor = args.descriptor.absolute()
    root = descriptor.parent.resolve(strict=True)
    source = confined(root, descriptor.name)
    if source.stat().st_size > 64 * 1024:
        parser.error("descriptor exceeds canonical bounded control size")
    raw = source.read_bytes()
    doc = json.loads(raw)
    # Only select canonical named file references; Rust remains the schema authority.
    immutable = [doc["kernel"], doc["initramfs"]]
    layers = doc["layers"]
    homes = doc["writable_disks"]
    if len(layers) > 16 or len(homes) > 4:
        parser.error("descriptor file census exceeds canonical bounds")
    immutable += [layer["artifact"] for layer in layers]
    rows = []
    names = set()
    for item in immutable:
        path = confined(root, item["path"])
        if path.stat().st_size != item["bytes"] or digest(path) != item["sha256"]:
            parser.error("immutable descriptor artifact hash/length mismatch")
        rows.append((item["path"], path, item["sha256"], False))
    for item in homes:
        path = confined(root, item["path"])
        if path.stat().st_size != item["bytes"] or item["bytes"] % 4096:
            parser.error("mutable home size differs; no formatting/truncation is allowed")
        rows.append((item["path"], path, digest(path), True))
    for relative, _, _, _ in rows:
        if relative in names:
            parser.error("duplicate artifact path")
        names.add(relative)
    out = args.out.absolute()
    if out.exists() or out.is_symlink() or out == root or root in out.parents:
        parser.error("output must be NEW and outside the input artifact tree")
    out.mkdir(mode=0o700)
    manifest = []

    def copy(source, relative, expected, mutable=False):
        target = out / relative
        target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        with source.open("rb") as reader, target.open("xb") as writer:
            shutil.copyfileobj(reader, writer, 1024 * 1024)
            writer.flush()
            os.fsync(writer.fileno())
        if digest(target) != expected or digest(source) != expected:
            raise ValueError("input changed or copied artifact hash differs; bundle incomplete")
        manifest.append({"path": relative, "bytes": target.stat().st_size,
                         "sha256": expected, "mutable_initial_snapshot": mutable})

    for relative, path, expected, mutable in rows:
        copy(path, relative, expected, mutable)
    copy(source, "node-image.json", hashlib.sha256(raw).hexdigest())
    for item in args.binary:
        path = confined(item.absolute().parent.resolve(strict=True), item.name)
        if not path.stat().st_mode & 0o111:
            parser.error("explicit host binary is not executable")
        copy(path, f"bin/{path.name}", digest(path))
        (out / "bin" / path.name).chmod(stat.S_IMODE(path.stat().st_mode) & 0o755)
    evidence = [args.source_bundle, *args.receipt]
    for index, item in enumerate(evidence):
        path = confined(item.absolute().parent.resolve(strict=True), item.name)
        copy(path, f"provenance/{index:03d}-{path.name}", digest(path))
    record = {"format": "moruna-private-vm-bundle-1", "visibility": "PRIVATE_NEVER_UPLOAD",
              "rust_sdk_validation": "REQUIRED_NOT_PERFORMED_BY_PACKAGER",
              "kvm": "NOT_RUN", "execution": "CONSULT_SOURCE_BOUND_RECEIPTS_NOT_INFERRED",
              "files": sorted(manifest, key=lambda row: row["path"])}
    with (out / "bundle.json").open("x") as file:
        json.dump(record, file, sort_keys=True, indent=2)
        file.write("\n")
        file.flush()
        os.fsync(file.fileno())
    print(f"Private VM bundle copied to {out}; Rust validation and execution remain required.")


if __name__ == "__main__":
    main()
