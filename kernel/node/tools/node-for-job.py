#!/usr/bin/env python3
"""The node image one job boots with `moruna run` (stream D): a descriptor in OUT.

From a node kit (kernel/tools/build.sh's output, with this directory as `node/tools` and
`standalone.image` as `node/standalone.image`), a canonical job, and the job's data disks, it
renders the node's assets with configure.py (the data disks granted to the engine), checks them
with the node image validator, builds the initramfs, provisions the node's home disk, and writes
OUT/node-image.json naming the kernel, the initramfs, the layers, the home and the data disks.
`moruna run` calls it for a run that names no prebuilt node (`--image`).

The artifacts are linked into OUT (copied where a link cannot be made), since a descriptor's
paths are relative to its own directory. The node's memory is the job's, its scratch (where the
engine's outputs are kept until they are written to the output disk) and the node's services;
`--node-memory` asks for an exact size and is refused when the job does not fit it.

    node-for-job.py --build KIT --job JOB --out NEW_DIR --cpus N [--tls-roots ROOTS]
                    [--input NAME]... [--output-bytes N] [--scratch BYTES]
                    [--node-memory BYTES] [--image-stack DIR]

No shell runs, nothing is downloaded, nothing boots here. The descriptor's path is printed last.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import time

MIB = 1024 * 1024
PORT = 5100
# The node's services' accounts, as proof.py sizes them; admission's is the engine's and more.
SERVICES = {"devices": 128 * MIB, "imagefs": 768 * MIB, "linux": 256 * MIB, "network": 64 * MIB, "storage": 128 * MIB}
# What the engine's account holds beyond its memory and scratch, and admission beyond that.
ENGINE_OVERHEAD = 128 * MIB
ADMISSION_OVERHEAD = 128 * MIB
# Room for the kernel and the boot beside the carved accounts.
BOOT_SLACK = 256 * MIB


def round_up(value, unit):
    return (value + unit - 1) // unit * unit


def sizes(job_memory, scratch, node_memory=None):
    """The node's fixed resources for a job of `job_memory` bytes: (node, job, account, scratch,
    admission), or a ValueError naming what does not fit."""
    job_memory, scratch = round_up(job_memory, 4096), round_up(scratch, 4096)
    account = job_memory + scratch + ENGINE_OVERHEAD
    admission = account + ADMISSION_OVERHEAD
    least = round_up(sum(SERVICES.values()) + admission + BOOT_SLACK, 2 * MIB)
    if node_memory is None:
        node_memory = least
    elif node_memory < least:
        raise ValueError(f"a node of {node_memory} bytes cannot hold the job's {job_memory} bytes, "
                         f"its {scratch} bytes of scratch and the node's services: it needs at least {least}")
    if node_memory % (2 * MIB):
        raise ValueError("the node's memory is whole 2 MiB pages")
    return node_memory, job_memory, account, scratch, admission


def kit_layers(build, stack_list):
    """The kit's layer disks the node attaches below a stack, from the stack's own list: the
    device of its lowest layer (`layer blockN ...`, `moruna-layer stack`'s FIRST) is how many
    disks come before it. A stack from block0 is attached alone (an image of its own, as the
    gate's, `--image-stack`); one from block2 has the kit's two image filesystem fixtures below
    it (`layers/layer-1.img` and `layer-2.img`, the kit's own `node-linux`), which no job reads."""
    fixtures = [build / "layers/layer-1.img", build / "layers/layer-2.img"]
    for line in stack_list.read_text().splitlines():
        words = line.split()
        if len(words) >= 2 and words[0] == "layer":
            first = words[1].removeprefix("block")
            if first in ("0", "2"):
                return fixtures[:int(first)]
            raise ValueError(f"{stack_list}: the lowest layer is on {words[1]}; a node puts it on block0, "
                             "or on block2 above the kit's two fixtures")
    raise ValueError(f"{stack_list}: no layer")


def place(source, destination):
    """`source` at `destination`: a hard link where the file system allows one, else a copy."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    try:
        os.link(source, destination)
    except OSError:
        shutil.copyfile(source, destination)
    return destination


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(MIB), b""):
            h.update(block)
    return h.hexdigest()


DATA_LINE = "  driver 1af4:1001 - data\n"


def data_disk_description(description, programs, out):
    """The node's description with the data disks' driver-table line (transitional virtio-blk,
    no driver started: the device manager hands each to the engine's supervisor), and the
    programs it is built from.

    The image tool asks every driver line for a program, the `-` of a line that starts none
    included (kernel/tools/image `Description::programs`; requested of the integrator). Until it
    does not, the programs are linked into OUT/bins with `-` a link to `virtio-blk`, which the
    device manager never starts for this line."""
    text = description.read_text()
    anchor = "  driver 1af4:1042 virtio-blk block\n"
    if DATA_LINE not in text:
        if anchor not in text:
            raise SystemExit("the node's description has no virtio-blk driver line to follow")
        text = text.replace(anchor, anchor + DATA_LINE, 1)
    rendered = out / "node.image"
    rendered.write_text(text)
    bins = out / "bins"
    for program in programs.iterdir():
        if program.is_file():
            place(program, bins / program.name)
    place(programs / "virtio-blk", bins / "-")
    return rendered, bins


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--build", type=Path, required=True)
    parser.add_argument("--job", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--cpus", type=int, required=True)
    parser.add_argument("--tls-roots", type=Path)
    parser.add_argument("--input", action="append", default=[])
    parser.add_argument("--output-bytes", type=int, default=0)
    parser.add_argument("--scratch", type=int, default=256 * MIB)
    parser.add_argument("--node-memory", type=int)
    parser.add_argument("--image-stack", type=Path)
    args = parser.parse_args()
    build, out = args.build.resolve(strict=True), args.out.absolute()
    if out.exists():
        parser.error("OUT must be a new directory")
    if not 1 <= args.cpus <= 64:
        parser.error("1 to 64 CPUs")
    out.mkdir(mode=0o700)
    tools = Path(__file__).resolve().parent
    description = tools.parent / "standalone.image"
    programs = build / "node-user-bins"
    if args.input or args.output_bytes:
        description, programs = data_disk_description(description, programs, out)

    def command(argv):
        with (out / "prepare.log").open("ab") as log:
            subprocess.run([str(a) for a in argv], check=True, stdout=log, stderr=subprocess.STDOUT)

    job = json.loads(args.job.read_text())
    run = job.get("run_id")
    if not isinstance(run, str) or len(run) != 32:
        parser.error("the job needs its canonical run_id")
    budget = job.get("budget") or {}
    elastic = budget.get("elastic") or {}
    job_memory = max(int(budget.get("memory_bytes") or 0), int(elastic.get("memory_max_bytes") or 0))
    if job_memory <= 0:
        parser.error("the job's budget names no memory")
    try:
        node_memory, job_memory, account, scratch, admission = sizes(job_memory, args.scratch, args.node_memory)
    except ValueError as error:
        parser.error(str(error))
    tls_roots = args.tls_roots or build / "ke2/roots"
    linux = (args.image_stack or build / "node-linux").resolve(strict=True)
    (out / "job.json").write_bytes(args.job.read_bytes())
    command([build / "moruna-node-ci-assets", out / "job.json", out / "canonical"])
    profile = {"version": 1, "node_memory_bytes": node_memory, "cpus": args.cpus, "agent_port": PORT,
               "job_memory_bytes": job_memory, "job_account_bytes": account, "scratch_bytes": scratch,
               "engine_program": "/moruna/bin/moruna-engine", "storage_provider": "single-node-home",
               "service_accounts": {**SERVICES, "admission": admission},
               "data_disks": {"inputs": args.input, "output_bytes": args.output_bytes}}
    (out / "profile.json").write_text(json.dumps(profile) + "\n")
    (out / "storage.conf").write_text("moruna-storage 1\ntimeout-ns 5000000000\nbody-limit 1048576\nsession-limit 4\n"
                                      "provider single-node-home\nblock-service nvme0\nalignment 4096\n")
    (out / "network.conf").write_text(f"moruna-netstack 1\nsetting tls-unix-seconds {int(time.time())}\n"
                                      "setting tls-time-valid-ms 3600000\n"
                                      "grant listen-engine rights=connect remote=127.0.0.0/8 local=127.0.0.0/8 sockets=8\n")
    command(["python3", "-I", tools / "configure.py", "--profile", out / "profile.json", "--out", out / "assets",
             "--image-record", linux / "image.record", "--layers", linux / "stack.layers", "--network", out / "network.conf",
             "--tls-roots", tls_roots, "--engine-bindings", out / "canonical/engine.bindings",
             "--storage-config", out / "storage.conf", "--storage-grants", out / "canonical/storage.grants",
             "--storage-admission", out / "canonical/storage.admission"])
    command([build / "moruna-node-image", description, out / "assets", node_memory, args.cpus, PORT])
    resources = json.loads((out / "assets/node-resources.json").read_text())
    command([build / "moruna-image-host", description, programs, out / "assets"])
    home = out / "home.img"
    with home.open("xb") as file:
        file.truncate(64 * MIB)
    command([build / "moruna-storage-provision", "plan", "--output", out / "home.root", "--capacity", 64 * MIB,
             "--alignment", 4096, "--log-bytes", MIB, "--role", "single-node-home", "--workspace", "moruna-ci", "--bucket", "jobs"])
    command([build / "moruna-storage-provision", "format", "--image", home, "--root", out / "home.root"])
    try:
        layers = kit_layers(build, linux / "stack.layers")
    except ValueError as error:
        parser.error(str(error))
    index = 1
    while (linux / f"stack-{index}.img").is_file():
        layers.append(linux / f"stack-{index}.img")
        index += 1
    artifacts = out / "artifacts"
    kernel = place(build / "moruna-microkernel.elf", artifacts / "kernel.elf")
    linked = [place(path, artifacts / f"layer-{n}.img") for n, path in enumerate(layers)]
    initramfs = out / "assets/initramfs.cpio"

    def artifact(path):
        return {"path": path.relative_to(out).as_posix(), "bytes": path.stat().st_size, "sha256": digest(path)}

    descriptor = {"version": 1, "architecture": "x86_64", "boot_mode": "multiboot2",
                  "kernel": artifact(kernel), "initramfs": artifact(initramfs),
                  "layers": [{"artifact": artifact(p)} for p in linked],
                  "writable_disks": [{"path": home.relative_to(out).as_posix(), "bytes": home.stat().st_size,
                                      "serial": "node-ci-home"}],
                  "user_network": False, "cid": 3, "agent_port": PORT, "resources": resources,
                  "data_disks": profile["data_disks"]}
    path = out / "node-image.json"
    with path.open("x") as file:
        json.dump(descriptor, file, indent=2)
        file.write("\n")
    print(path)


if __name__ == "__main__":
    main()
