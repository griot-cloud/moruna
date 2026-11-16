#!/usr/bin/env python3
"""The node image one job boots with `moruna run --vm` (stream D): a descriptor beside a build.

From a build output (kernel/tools/build.sh), a canonical job, and the job's data disks, it renders
the node's assets with configure.py (the data disks granted to the engine), checks them with the
node image validator, builds the initramfs, provisions the node's home disk, and writes
`node-image.json` naming the kernel, the initramfs, the layers, the home and the data disks.

Every artifact path in a descriptor is relative to its directory, so the descriptor is written in
the closest directory holding the build and OUT (`DESCRIPTOR` printed last), as proof.py does.

    node-for-job.py --build BUILD --job JOB --out NEW_DIR --cpus N --tls-roots ROOTS
                    [--input NAME]... [--output-bytes N] [--node-memory BYTES]

No shell runs, nothing is downloaded, nothing boots here.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

MIB = 1024 * 1024
PORT = 5100


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(MIB), b""):
            h.update(block)
    return h.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--build", type=Path, required=True)
    parser.add_argument("--job", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--cpus", type=int, required=True)
    parser.add_argument("--tls-roots", type=Path, required=True)
    parser.add_argument("--input", action="append", default=[])
    parser.add_argument("--output-bytes", type=int, default=0)
    parser.add_argument("--node-memory", type=int, default=3072 * MIB)
    args = parser.parse_args()
    build, out = args.build.resolve(strict=True), args.out.absolute()
    if out.exists():
        parser.error("OUT must be a new directory")
    if not 1 <= args.cpus <= 64:
        parser.error("1 to 64 CPUs")
    out.mkdir(mode=0o700)
    tools = Path(__file__).resolve().parent
    description = tools.parent / "standalone.image"

    def command(argv):
        with (out / "prepare.log").open("ab") as log:
            subprocess.run([str(a) for a in argv], check=True, stdout=log, stderr=subprocess.STDOUT)

    job = json.loads(args.job.read_text())
    run = job.get("run_id")
    if not isinstance(run, str) or len(run) != 32:
        parser.error("the job needs its canonical run_id")
    (out / "job.json").write_bytes(args.job.read_bytes())
    command([build / "moruna-node-ci-assets", out / "job.json", out / "canonical"])
    profile = {"version": 1, "node_memory_bytes": args.node_memory, "cpus": args.cpus, "agent_port": PORT,
               "job_memory_bytes": 1024 * MIB, "job_account_bytes": 1408 * MIB, "scratch_bytes": 256 * MIB,
               "engine_program": "/moruna/bin/moruna-engine", "storage_provider": "single-node-home",
               "service_accounts": {"devices": 128 * MIB, "imagefs": 768 * MIB, "linux": 256 * MIB,
                                    "network": 64 * MIB, "admission": 1536 * MIB, "storage": 128 * MIB},
               "data_disks": {"inputs": args.input, "output_bytes": args.output_bytes}}
    (out / "profile.json").write_text(json.dumps(profile) + "\n")
    (out / "storage.conf").write_text("moruna-storage 1\ntimeout-ns 5000000000\nbody-limit 1048576\nsession-limit 4\n"
                                      "provider single-node-home\nblock-service nvme0\nalignment 4096\n")
    (out / "network.conf").write_text(f"moruna-netstack 1\nsetting tls-unix-seconds {int(time.time())}\n"
                                      "setting tls-time-valid-ms 3600000\n"
                                      "grant listen-engine rights=connect remote=127.0.0.0/8 local=127.0.0.0/8 sockets=8\n")
    linux = build / "node-linux"
    command(["python3", "-I", tools / "configure.py", "--profile", out / "profile.json", "--out", out / "assets",
             "--image-record", linux / "image.record", "--layers", linux / "stack.layers", "--network", out / "network.conf",
             "--tls-roots", args.tls_roots, "--engine-bindings", out / "canonical/engine.bindings",
             "--storage-config", out / "storage.conf", "--storage-grants", out / "canonical/storage.grants",
             "--storage-admission", out / "canonical/storage.admission"])
    command([build / "moruna-node-image", description, out / "assets", args.node_memory, args.cpus, PORT])
    resources = json.loads((out / "assets/node-resources.json").read_text())
    command([build / "moruna-image-host", description, build / "node-user-bins", out / "assets"])
    home = out / "home.img"
    with home.open("xb") as file:
        file.truncate(64 * MIB)
    command([build / "moruna-storage-provision", "plan", "--output", out / "home.root", "--capacity", 64 * MIB,
             "--alignment", 4096, "--log-bytes", MIB, "--role", "single-node-home", "--workspace", "moruna-ci", "--bucket", "jobs"])
    command([build / "moruna-storage-provision", "format", "--image", home, "--root", out / "home.root"])
    layers = [build / "layers/layer-1.img", build / "layers/layer-2.img"]
    index = 1
    while (linux / f"stack-{index}.img").is_file():
        layers.append(linux / f"stack-{index}.img")
        index += 1
    kernel = build / "moruna-microkernel.elf"
    initramfs = out / "assets/initramfs.cpio"
    parent = Path(os.path.commonpath([str(p.parent) for p in [kernel, initramfs, home, *layers]]))
    if parent == Path(parent.anchor):
        parser.error("the build and OUT need a common directory below the root")

    def artifact(path):
        return {"path": path.relative_to(parent).as_posix(), "bytes": path.stat().st_size, "sha256": digest(path)}

    descriptor = {"version": 1, "architecture": "x86_64", "boot_mode": "multiboot2",
                  "kernel": artifact(kernel), "initramfs": artifact(initramfs),
                  "layers": [{"artifact": artifact(p)} for p in layers],
                  "writable_disks": [{"path": home.relative_to(parent).as_posix(), "bytes": home.stat().st_size,
                                      "serial": "node-ci-home"}],
                  "user_network": False, "cid": 3, "agent_port": PORT, "resources": resources,
                  "data_disks": profile["data_disks"]}
    path = parent / f".moruna-node-{run}.json"
    with path.open("x") as file:
        json.dump(descriptor, file, indent=2)
        file.write("\n")
    (out / "node-image.json").write_bytes(path.read_bytes())
    print(path)


if __name__ == "__main__":
    main()
