#!/usr/bin/env python3
"""Build a node and an x86_64 UEFI GPT image, without mounts.

The profile says which node: `standalone` (the default), the node the Linux-node launcher
starts, with its engine, layers and launcher descriptor; or `cloud` (system design 1A), the
one disk image for every cloud, which holds only what kernel/node/cloud.image lists and takes
no agent port, CPU count, layers, writable homes, user network or guest CID.

`--formats` names the disk's formats (system design 1A, work item 1.5): `raw`, the disk every
build writes, `qcow2` for OpenStack and `vhd` for Azure, each made from that one raw disk and
written beside it.

`--system-layer` names a layer file in the format the image filesystem reads (the engine's
layer, with `/moruna/bin/moruna`): the disk gains a second GPT partition, `moruna-system`,
holding it whole (system design 1A, item 3.3), which the boot disk's driver serves as the read
only disk `system`, and the payload gains `linux.layers`, the image filesystem's list naming it
by its seal (`moruna-layers 1`, then `layer system <seal>`), for the profile to grant.

All inputs are explicit local files. This command does not fetch layers, install
host tools, acquire credentials, upload an image or start a virtual machine.
The caller must hold the programme's shared build reservation before invoking it.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

PROFILES = ("standalone", "cloud")
# The user programs each profile's image starts; `--package` adds to them.
PROGRAMS = {
    "standalone": ["moruna-node-supervisor", "node-admission", "moruna-storage-service", "devices", "imagefs", "linux", "netstack", "engine",
                   "virtio-blk", "virtio-vsock", "virtio-net", "nvme", "gve"],
    "cloud": ["moruna-node-supervisor", "devices", "netstack", "moruna-node-api", "moruna-node-platform",
              "virtio-blk", "virtio-net", "nvme", "gve", "moruna-vmbus-server", "moruna-storvsc-server"],
}
# The flags only a launcher's node takes, refused for a cloud image.
LAUNCHER_FLAGS = (("agent_port", "--agent-port"), ("cpus", "--cpus"), ("layer", "--layer"),
                  ("writable_home", "--writable-home"), ("user_network", "--user-network"), ("cid", "--cid"))
# The C of a user program (`ring`, which the network server brings in) is compiled as
# kernel/tools/build.sh compiles it: by clang, freestanding, for the bare x86_64 ELF target.
USER_CC = "CC_x86_64_moruna_user"
USER_CFLAGS = "CFLAGS_x86_64_moruna_user"
USER_C_FLAGS = "--target=x86_64-unknown-none -ffreestanding -mno-red-zone -fno-stack-protector"


def user_c(environment):
    """The two C compiler variables of the user build, as kernel/tools/build.sh sets them:
    the caller's compiler, or clang; build.sh's flags, then the caller's. They name the user
    target, so no host tool and no kernel build reads them."""
    flags = " ".join(part for part in (USER_C_FLAGS, environment.get(USER_CFLAGS, "").strip()) if part)
    return {USER_CC: environment.get(USER_CC) or "clang", USER_CFLAGS: flags}

# The disk's formats. `raw` is the disk itself; each other one is a file beside it.
FORMATS = ("raw", "qcow2", "vhd")
MIB = 1024 * 1024
# The boot payload's file naming the GUID of the image's own disk (system design 3C, item
# 3.3), and what it holds while the image it is computed from is built.
DISK_GUID = "disk.guid"
PLACEHOLDER = "00000000-0000-0000-0000-000000000000"
# The boot disk's second partition, which holds the engine's layer (system design 1A), and the
# payload's file naming that layer for the image filesystem (item 3.3).
SYSTEM_PARTITION = "moruna-system"
LINUX_LAYERS = "linux.layers"
# A layer file's first bytes, and its pages (moruna_imagefs::format).
LAYER_MAGIC = b"MRNLAYR1"
PAGE = 4096
# The GPT's first partition starts at 1 MiB, its sectors are 512 bytes, and the disk keeps
# room after its partitions for the backup table.
FIRST_SECTOR = 2048
SECTOR = 512


def system_layer(path):
    """The seal of the layer file at `path`: the SHA-256 of its header page, by which the image
    filesystem names a layer. A file that is not whole pages of a layer is refused."""
    path = Path(path)
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"the system layer must be a regular file: {path}")
    size = path.stat().st_size
    if size == 0 or size % PAGE:
        raise ValueError(f"the system layer is not whole pages: {path} has {size} bytes")
    with path.open("rb") as file:
        header = file.read(PAGE)
    if header[:len(LAYER_MAGIC)] != LAYER_MAGIC:
        raise ValueError(f"the system layer is not a layer the image filesystem reads: {path}")
    return hashlib.sha256(header).hexdigest()


def linux_layers(seal):
    """The image filesystem's `layers` text naming the system layer by its seal."""
    return f"moruna-layers 1\nlayer system {seal}\n"


def layout(esp_mib, layer_bytes):
    """The disk's partitions, in MiB: the EFI system partition from 1 MiB, then, when there is a
    system layer of `layer_bytes`, `moruna-system` right after it, whole MiB; and the disk's
    size, with room after them for the backup table. The sgdisk arguments that make them."""
    system_mib = whole_mib(layer_bytes) // MIB if layer_bytes else 0
    disk_mib = esp_mib + system_mib + 4
    args = [f"--new=1:{FIRST_SECTOR}:+{esp_mib}M", "--typecode=1:ef00"]
    system_sector = None
    if system_mib:
        system_sector = FIRST_SECTOR + esp_mib * MIB // SECTOR
        args += [f"--new=2:{system_sector}:+{system_mib}M", "--typecode=2:8300",
                 f"--change-name=2:{SYSTEM_PARTITION}"]
    return {"disk_mib": disk_mib, "esp_mib": esp_mib, "system_mib": system_mib,
            "system_sector": system_sector, "sgdisk": args}


def assemble(disk, esp, layer, guid, esp_mib, cwd, run=None):
    """Writes the raw disk at `disk`: its GPT (disk GUID `guid`) by sgdisk, run by `run` (the
    command line's runner unless given), the EFI system partition's image `esp`, and, when
    `layer` names one, the system layer whole at the start of `moruna-system`. The layout
    written."""
    run = run or command
    planned = layout(esp_mib, layer.stat().st_size if layer else 0)
    with disk.open("xb") as file:
        file.truncate(planned["disk_mib"] * MIB)
    run(["sgdisk", "--clear", f"--disk-guid={guid}", *planned["sgdisk"], disk], cwd)
    with disk.open("r+b") as destination:
        with esp.open("rb") as source:
            destination.seek(FIRST_SECTOR * SECTOR)
            shutil.copyfileobj(source, destination)
        if layer:
            with layer.open("rb") as source:
                destination.seek(planned["system_sector"] * SECTOR)
                shutil.copyfileobj(source, destination)
    return planned


def disk_guid(files):
    """The GPT disk GUID of an image built from `files`, in order: the first 16 bytes of the
    SHA-256 of their lengths and bytes, as an RFC 9562 version 8 GUID in sgdisk's text form.
    The same files give the same GUID, and another build another, so a block driver tells the
    disk the node booted from (the one whose GPT header names the GUID the boot payload
    carries) from another Moruna image's disk attached as a volume."""
    digest = hashlib.sha256()
    for path in files:
        data = Path(path).read_bytes()
        digest.update(len(data).to_bytes(8, "little"))
        digest.update(data)
    raw = bytearray(digest.digest()[:16])
    raw[6] = (raw[6] & 0x0F) | 0x80
    raw[8] = (raw[8] & 0x3F) | 0x80
    text = raw.hex().upper()
    return f"{text[:8]}-{text[8:12]}-{text[12:16]}-{text[16:20]}-{text[20:]}"


def command(args, cwd, env=None):
    subprocess.run([str(a) for a in args], cwd=cwd, env=env, check=True)


def whole_mib(size):
    """`size` bytes rounded up to a whole MiB."""
    return -(-size // MIB) * MIB


def convert(disk, formats, run=command):
    """Each of `formats` made from the raw disk, beside it: the files written, in that order.

    `raw` is the disk itself, so it writes nothing. `qcow2` is for OpenStack. `vhd` is Azure's
    fixed-size VHD: the raw disk rounded up to a whole MiB (zeros after its end), then the VHD's
    512-byte footer. `force_size` keeps exactly that size, where QEMU would otherwise round it
    to a disk geometry.
    """
    written = []
    for name in dict.fromkeys(formats):
        if name not in FORMATS:
            raise ValueError(f"unknown disk format: {name}")
        if name == "raw":
            continue
        target = disk.with_suffix(f".{name}")
        if name == "qcow2":
            run(["qemu-img", "convert", "-f", "raw", "-O", "qcow2", disk, target], disk.parent)
        else:
            source, size = disk, disk.stat().st_size
            if size != whole_mib(size):
                source = disk.with_suffix(".vhd-source")
                shutil.copyfile(disk, source)
                os.truncate(source, whole_mib(size))
            try:
                run(["qemu-img", "convert", "-f", "raw", "-O", "vpc", "-o", "subformat=fixed,force_size",
                     source, target], disk.parent)
            finally:
                if source != disk:
                    source.unlink()
        written.append(target)
    return written


def recorded(paths):
    """Each file's size and SHA-256, as the package's evidence file holds them."""
    evidence = {}
    for path in paths:
        with path.open("rb") as file:
            digest = hashlib.file_digest(file, "sha256").hexdigest()
        evidence[str(path)] = {"size": path.stat().st_size, "sha256": digest}
    return evidence


def arguments(argv=None):
    """The command line, checked for its profile before anything is built."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=PROFILES, default="standalone")
    parser.add_argument("--description", type=Path, required=True)
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--package", action="append", default=[])
    parser.add_argument("--layer", action="append", type=Path, default=[])
    parser.add_argument("--writable-home", action="append", type=Path, default=[])
    parser.add_argument("--user-network", action="store_true")
    parser.add_argument("--cid", type=int)
    parser.add_argument("--agent-port", type=int)
    parser.add_argument("--node-memory", type=int, required=True)
    parser.add_argument("--cpus", type=int)
    parser.add_argument("--system-layer", type=Path,
                        help="a layer file (the engine's) for the boot disk's moruna-system partition")
    parser.add_argument("--formats", nargs="+", choices=FORMATS, default=["raw"], metavar="FORMAT",
                        help="the disk's formats, each written beside the raw disk: raw (the default), qcow2, vhd")
    args = parser.parse_args(argv)
    if args.profile == "cloud":
        given = [flag for name, flag in LAUNCHER_FLAGS if getattr(args, name) not in (None, False, [])]
        if given:
            parser.error(f"the cloud profile takes no {', '.join(given)}")
        return parser, args
    missing = [flag for name, flag in (("agent_port", "--agent-port"), ("cpus", "--cpus")) if getattr(args, name) is None]
    if missing:
        parser.error(f"the following arguments are required: {', '.join(missing)}")
    if args.cid is None:
        args.cid = 3
    if args.cid <= 2 or args.cid > 0xffffffff or args.agent_port <= 0 or args.agent_port > 0xffffffff or len(args.layer) > 16 or len(args.writable_home) > 4:
        parser.error("invalid guest CID or layer count")
    return parser, args


def main():
    parser, args = arguments()
    cloud = args.profile == "cloud"
    kernel = Path(__file__).resolve().parents[2]
    description = args.description.resolve(strict=True)
    assets = args.assets.resolve(strict=True)
    out = args.out.absolute()
    if out == assets or assets in out.parents:
        parser.error("output must not be inside the input asset tree")
    if out.exists():
        parser.error("output must be a new directory; existing artifacts are never removed")
    layer = None
    if args.system_layer:
        layer = args.system_layer.absolute()
        try:
            seal = system_layer(layer)
        except ValueError as refused:
            parser.error(str(refused))
    tools = ["cargo", "rustc", "grub-mkstandalone", "sgdisk", "mformat", "mmd", "mcopy"]
    if any(name != "raw" for name in args.formats):
        tools.append("qemu-img")
    for tool in tools:
        if shutil.which(tool) is None:
            parser.error(f"required tool unavailable: {tool}")
    # Do not follow asset symlinks into a host home or silently package sockets.
    files = []
    for item in sorted(assets.rglob("*")):
        if item.is_symlink() or not (item.is_dir() or item.is_file()):
            parser.error(f"asset must be a regular file or directory: {item}")
        if item.is_file():
            files.append(item)
    out.mkdir(mode=0o700, parents=False)
    payload = out / "payload"
    payload.mkdir()
    for item in files:
        target = payload / item.relative_to(assets)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(item, target)
    # configure.py's rendering profile is the standalone node's; a cloud image has none.
    if layer:
        (payload / LINUX_LAYERS).write_text(linux_layers(seal))
    profile = {} if cloud else json.loads((payload / "profile.json").read_text())
    additions=[]
    if profile.get("storage_provider") == "bucket": additions.append("  handle data:storage.credential storage.credential")
    if profile.get("storage_provider")=="local-single-node-home":
        additions += ["  handle data:checkpoint.admission checkpoint.admission", "  handle data:cache.admission cache.admission", "  handle data:cache.key cache.key"]
    if "local" in profile.get("service_accounts",{}):
        additions += ["  handle data:local.conf local.config", "  handle data:checkpoint.admission checkpoint.admission", "  handle data:cache.admission cache.admission", "  handle data:cache.key cache.key"]
    if "housekeeping" in profile.get("service_accounts", {}):
        additions += ["  handle program:storage-maintenance image.maintenance",
                      "  handle data:maintenance.conf maintenance.server-config",
                      "  handle data:maintenance.conf maintenance.client-config"]
    if additions:
        rendered=out / "standalone.image"
        rendered.write_text(description.read_text() + "\n".join(additions) + "\n")
        description=rendered
    target = "x86_64-unknown-none"
    # Separate real user ABI; core/alloc and binaries must not reuse kernel output.
    user_target = kernel / "targets/x86_64-moruna-user.json"
    std = ["-Zbuild-std=core,alloc,compiler_builtins", "-Zbuild-std-features=compiler-builtins-mem"]
    cargo = ["cargo", "+nightly-2026-07-21"]
    common = ["--locked", "--offline", "--release", "--target", target, *std]
    flags = "--cfg polyval_force_soft -C relocation-model=static -C relro-level=off -C force-unwind-tables=yes --check-cfg cfg(ktest) -C no-redzone=y"
    env = os.environ.copy()
    env.pop("CARGO_ENCODED_RUSTFLAGS", None)
    env["RUSTFLAGS"] = flags
    command([*cargo, "build", *common, "-p", "moruna-microkernel-boot"], kernel, env)
    env["RUSTFLAGS"] = f"{flags} -Zunstable-options -C link-arg=-T{kernel / 'sys/user.ld'} -C link-arg=-static"
    env.update(user_c(os.environ))
    packages =list(dict.fromkeys([*PROGRAMS[args.profile], *args.package]))
    package_args = [word for package in packages for word in ("-p", package)]
    user_common = ["--locked", "--offline", "--release", "--target", user_target, *std]
    # The storage service's feature names a package only the standalone node builds.
    features = [] if "moruna-storage-service" not in packages else ["--features", "moruna-storage-service/service"]
    # The JSON target's -Z flags follow `build`, as in kernel/tools/build.sh: this nightly
    # refuses them before the subcommand.
    command([*cargo, "build", "-Zunstable-options", "-Zjson-target-spec", *user_common, *package_args, *features], kernel, env)
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", kernel / "target"))
    if not target_dir.is_absolute():
        target_dir = kernel / target_dir
    target_dir = target_dir.resolve()
    binaries = target_dir / user_target.stem / "release"
    host_env = os.environ.copy()
    host_env.pop("CARGO_ENCODED_RUSTFLAGS", None)
    # The host validator now consumes engine/storage types too. Preserve the
    # dependency-wide approved crypto backend without bare-metal linker flags.
    host_env["RUSTFLAGS"] = "--cfg polyval_force_soft"
    # Explicit host target avoids the kernel workspace's default bare-metal target.
    host = subprocess.check_output(["rustc", "+nightly-2026-07-21", "-vV"], cwd=kernel, text=True)
    host = next(line.removeprefix("host: ") for line in host.splitlines() if line.startswith("host: "))
    if cloud:
        command([*cargo, "run", "--locked", "--offline", "--release", "--target", host,
                 "-p", "moruna-node-image", "--", "--profile", "cloud", description, payload,
                 args.node_memory], kernel, host_env)
    else:
        command([*cargo, "run", "--locked", "--offline", "--release", "--target", host,
                 "-p", "moruna-node-image", "--", description, payload,
                 args.node_memory, args.cpus, args.agent_port], kernel, host_env)
        resources = json.loads((payload / "node-resources.json").read_text())
    # The image is built once with a placeholder GUID, which names it by its content, then
    # again with that GUID in its payload: the disk carries the GUID the boot payload says.
    (payload / DISK_GUID).write_text(PLACEHOLDER + "\n")
    image_command = [*cargo, "run", "--locked", "--offline", "--release", "--target", host,
                     "-p", "moruna-image", "--", description, binaries, payload]
    command(image_command, kernel, host_env)
    elf = target_dir / target / "release" / "moruna-microkernel"
    guid = disk_guid([payload / "initramfs.cpio", elf, *([layer] if layer else [])])
    (payload / DISK_GUID).write_text(guid + "\n")
    command(image_command, kernel, host_env)
    # Preserve the real ELF machine type. The EM_386 patch used only by QEMU's
    # direct -kernel loader is deliberately not part of a GRUB boot image.
    if elf.read_bytes()[:4] != b"\x7fELF":
        raise RuntimeError("kernel output is not ELF")
    shutil.copyfile(elf, payload / "moruna-kernel.elf")
    config = out / "grub.cfg"
    # A node has no screen, and the core's multiboot2 header asks for no framebuffer. GRUB's EFI
    # build has no text console to hand over instead, so at `boot` it still tries a video mode
    # ("auto") and prints "error: no suitable video mode found" where the firmware offers none
    # (QEMU with no display, Hyper-V Generation 2). `gfxpayload=text` has it try none.
    config.write_text("set timeout=0\nset default=0\nmenuentry 'Moruna' {\n search --file --set=root /boot/moruna-kernel.elf\n multiboot2 /boot/moruna-kernel.elf\n module2 /boot/initramfs.cpio\n set gfxpayload=text\n boot\n}\n")
    boot = out / "BOOTX64.EFI"
    # GRUB reads the boot files from the GPT disk's FAT32 system partition: its partition and
    # filesystem modules are loaded with it, or `search` finds no device holding the kernel.
    command(["grub-mkstandalone", "-O", "x86_64-efi", "--modules=part_gpt fat", "-o", boot,
             f"boot/grub/grub.cfg={config}"], out)
    boot_inputs = [boot, payload / "moruna-kernel.elf", payload / "initramfs.cpio"]
    if any(path.stat().st_size >= 0xffffffff for path in boot_inputs):
        parser.error("UEFI FAT32 boot files must each be smaller than 4GiB")
    esp_mib = max(64, (sum(path.stat().st_size for path in boot_inputs) + 8 * MIB + MIB - 1) // MIB)
    if esp_mib > 8192:
        parser.error("boot partition exceeds explicit 8GiB package limit")
    esp = out / "esp.img"
    with esp.open("xb") as file:
        file.truncate(esp_mib * MIB)
    command(["mformat", "-i", esp, "-F", "::"], out)
    command(["mmd", "-i", esp, "::/EFI", "::/EFI/BOOT", "::/boot"], out)
    command(["mcopy", "-i", esp, boot, "::/EFI/BOOT/BOOTX64.EFI"], out)
    for name in ("moruna-kernel.elf", "initramfs.cpio"):
        command(["mcopy", "-i", esp, payload / name, f"::/boot/{name}"], out)
    disk = out / "moruna-node.raw"
    assemble(disk, esp, layer, guid, esp_mib, out)
    images = convert(disk, args.formats)
    evidence = recorded([description, *(payload / p.relative_to(assets) for p in files), payload / "manifest",
                         payload / "initramfs.cpio", payload / "moruna-kernel.elf", boot, disk, *images,
                         *([layer, payload / LINUX_LAYERS] if layer else [])])
    if cloud:
        # No launcher descriptor, resources contract or writable inputs: a cloud boots the disk.
        (out / "inputs-and-artifacts.json").write_text(json.dumps(evidence, indent=2) + "\n")
        print(f"Packaged {disk} (cloud profile); boot and hardware validation remain required.")
        return
    def artifact(path):
        with path.open("rb") as file:
            digest = hashlib.file_digest(file, "sha256").hexdigest()
        return {"path": str(path.relative_to(out)), "bytes": path.stat().st_size, "sha256": digest}
    layers = []
    for name in args.layer:
        if name.is_absolute() or ".." in name.parts:
            parser.error("layer must name an exact relative asset path")
        layer = payload / name
        if not layer.is_file():
            parser.error(f"layer asset unavailable: {name}")
        layers.append({"artifact": artifact(layer)})
    writable_disks = []
    for index, source in enumerate(args.writable_home):
        if source.is_symlink() or not source.is_file() or source.stat().st_size == 0 or source.stat().st_size % 4096:
            parser.error("writable home must be an explicit preprovisioned page-aligned regular image")
        destination = out / "homes" / f"home-{index}.raw"
        destination.parent.mkdir(exist_ok=True)
        shutil.copyfile(source, destination)
        writable_disks.append({"path": str(destination.relative_to(out)), "bytes": destination.stat().st_size, "serial": f"moruna-home-{index}"})
    (out / "mutable-inputs.json").write_text(json.dumps({str(p): artifact(p) for p in (out / "homes").glob("*.raw")}, indent=2) + "\n")
    descriptor = {
        "version": 1, "architecture": "x86_64", "boot_mode": "multiboot2",
        "kernel": artifact(payload / "moruna-kernel.elf"),
        "initramfs": artifact(payload / "initramfs.cpio"),
        "layers": layers, "writable_disks": writable_disks, "user_network": args.user_network, "cid": args.cid, "agent_port": args.agent_port, "resources": resources,
    }
    # The engine's data disks, as configure.py granted them (stream D): the launcher binds a
    # run's files to these names and attaches them in this order.
    if "data_disks" in profile:
        descriptor["data_disks"] = profile["data_disks"]
    (out / "node-image.json").write_text(json.dumps(descriptor, indent=2) + "\n")
    (out / "inputs-and-artifacts.json").write_text(json.dumps(evidence, indent=2) + "\n")
    print(f"Packaged {disk}; boot and hardware validation remain required.")


if __name__ == "__main__":
    main()
