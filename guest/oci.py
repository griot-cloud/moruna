#!/usr/bin/env python3
"""Assemble the guest image as an OCI image layout, byte-for-byte reproducibly.

    oci.py <out-dir> <arch: amd64|arm64> <kernel> <initramfs> <rootfs>

The layout is what `moruna-vmm boot --image <out-dir>` reads (crates/moruna-vmm/src/image.rs):
one manifest with artifactType application/vnd.moruna.guest.v1 whose three layers are the
kernel, the gzip initramfs and the EROFS root filesystem, each by its own media type. JSON is
written with sorted keys and no whitespace, and nothing time-dependent is recorded, so the
same inputs give the same manifest digest. Prints that digest.
"""

import hashlib
import json
import os
import shutil
import sys

ARTIFACT_TYPE = "application/vnd.moruna.guest.v1"
MEDIA = {
    "kernel": "application/vnd.moruna.guest.kernel.v1",
    "initramfs": "application/vnd.moruna.guest.initramfs.v1+gzip",
    "rootfs": "application/vnd.moruna.guest.rootfs.v1.erofs",
}
MANIFEST = "application/vnd.oci.image.manifest.v1+json"
INDEX = "application/vnd.oci.image.index.v1+json"
EMPTY = "application/vnd.oci.empty.v1+json"


def canonical(obj) -> bytes:
    return json.dumps(obj, sort_keys=True, separators=(",", ":")).encode()


def put(out: str, data: bytes) -> dict:
    digest = hashlib.sha256(data).hexdigest()
    path = os.path.join(out, "blobs", "sha256", digest)
    with open(path, "wb") as f:
        f.write(data)
    return {"digest": f"sha256:{digest}", "size": len(data)}


def put_file(out: str, src: str) -> dict:
    h = hashlib.sha256()
    with open(src, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    digest = h.hexdigest()
    shutil.copyfile(src, os.path.join(out, "blobs", "sha256", digest))
    return {"digest": f"sha256:{digest}", "size": os.path.getsize(src)}


def main(argv: list[str]) -> int:
    if len(argv) != 6 or argv[2] not in ("amd64", "arm64"):
        print(__doc__, file=sys.stderr)
        return 2
    out, arch, kernel, initramfs, rootfs = argv[1:]
    if os.path.exists(out):
        shutil.rmtree(out)
    os.makedirs(os.path.join(out, "blobs", "sha256"))
    with open(os.path.join(out, "oci-layout"), "wb") as f:
        f.write(canonical({"imageLayoutVersion": "1.0.0"}))
    config = {"mediaType": EMPTY, **put(out, b"{}")}
    layers = [
        {"mediaType": MEDIA["kernel"], **put_file(out, kernel)},
        {"mediaType": MEDIA["initramfs"], **put_file(out, initramfs)},
        {"mediaType": MEDIA["rootfs"], **put_file(out, rootfs)},
    ]
    manifest = {
        "schemaVersion": 2,
        "mediaType": MANIFEST,
        "artifactType": ARTIFACT_TYPE,
        "config": config,
        "layers": layers,
    }
    m = put(out, canonical(manifest))
    index = {
        "schemaVersion": 2,
        "mediaType": INDEX,
        "manifests": [
            {
                "mediaType": MANIFEST,
                **m,
                "platform": {"architecture": arch, "os": "linux"},
            }
        ],
    }
    with open(os.path.join(out, "index.json"), "wb") as f:
        f.write(canonical(index))
    print(m["digest"])
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
