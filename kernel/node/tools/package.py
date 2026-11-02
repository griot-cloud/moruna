#!/usr/bin/env python3
"""Build a standalone node and an x86_64 UEFI GPT image, without mounts.

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


def command(args, cwd, env=None):
    subprocess.run([str(a) for a in args], cwd=cwd, env=env, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--description", type=Path, required=True)
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--package", action="append", default=[])
    parser.add_argument("--layer", action="append", type=Path, default=[])
    parser.add_argument("--writable-home", action="append", type=Path, default=[])
    parser.add_argument("--user-network", action="store_true")
    parser.add_argument("--cid", type=int, default=3)
    parser.add_argument("--agent-port", type=int, required=True)
    parser.add_argument("--node-memory", type=int, required=True)
    parser.add_argument("--cpus", type=int, required=True)
    args = parser.parse_args()
    if args.cid <= 2 or args.cid > 0xffffffff or args.agent_port <= 0 or args.agent_port > 0xffffffff or len(args.layer) > 16 or len(args.writable_home) > 4:
        parser.error("invalid guest CID or layer count")
    kernel = Path(__file__).resolve().parents[2]
    description = args.description.resolve(strict=True)
    assets = args.assets.resolve(strict=True)
    out = args.out.absolute()
    if out == assets or assets in out.parents:
        parser.error("output must not be inside the input asset tree")
    if out.exists():
        parser.error("output must be a new directory; existing artifacts are never removed")
    tools = ["cargo", "rustc", "grub-mkstandalone", "sgdisk", "mformat", "mmd", "mcopy"]
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
    profile = json.loads((payload / "profile.json").read_text())
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
    packages = list(dict.fromkeys([
        "moruna-node-supervisor", "node-admission", "moruna-storage-service", "devices", "imagefs", "linux", "netstack", "engine",
        "virtio-blk", "virtio-vsock", "virtio-net", "nvme", "gve", *args.package,
    ]))
    package_args = [word for package in packages for word in ("-p", package)]
    user_common = ["--locked", "--offline", "--release", "--target", user_target, *std]
    command([*cargo, "-Zunstable-options", "-Zjson-target-spec", "build", *user_common, *package_args, "--features", "moruna-storage-service/service"], kernel, env)
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
    command([*cargo, "run", "--locked", "--offline", "--release", "--target", host,
             "-p", "moruna-node-image", "--", description, payload,
             args.node_memory, args.cpus, args.agent_port], kernel, host_env)
    resources = json.loads((payload / "node-resources.json").read_text())
    command([*cargo, "run", "--locked", "--offline", "--release", "--target", host,
             "-p", "moruna-image", "--", description, binaries, payload], kernel, host_env)
    elf = target_dir / target / "release" / "moruna-microkernel"
    # Preserve the real ELF machine type. The EM_386 patch used only by QEMU's
    # direct -kernel loader is deliberately not part of a GRUB boot image.
    if elf.read_bytes()[:4] != b"\x7fELF":
        raise RuntimeError("kernel output is not ELF")
    shutil.copyfile(elf, payload / "moruna-kernel.elf")
    config = out / "grub.cfg"
    config.write_text("set timeout=0\nset default=0\nmenuentry 'Moruna' {\n search --file --set=root /boot/moruna-kernel.elf\n multiboot2 /boot/moruna-kernel.elf\n module2 /boot/initramfs.cpio\n boot\n}\n")
    boot = out / "BOOTX64.EFI"
    command(["grub-mkstandalone", "-O", "x86_64-efi", "-o", boot,
             f"boot/grub/grub.cfg={config}"], out)
    boot_inputs = [boot, payload / "moruna-kernel.elf", payload / "initramfs.cpio"]
    if any(path.stat().st_size >= 0xffffffff for path in boot_inputs):
        parser.error("UEFI FAT32 boot files must each be smaller than 4GiB")
    mib = 1024 * 1024
    esp_mib = max(64, (sum(path.stat().st_size for path in boot_inputs) + 8 * mib + mib - 1) // mib)
    if esp_mib > 8192:
        parser.error("boot partition exceeds explicit 8GiB package limit")
    esp = out / "esp.img"
    with esp.open("xb") as file:
        file.truncate(esp_mib * mib)
    command(["mformat", "-i", esp, "-F", "::"], out)
    command(["mmd", "-i", esp, "::/EFI", "::/EFI/BOOT", "::/boot"], out)
    command(["mcopy", "-i", esp, boot, "::/EFI/BOOT/BOOTX64.EFI"], out)
    for name in ("moruna-kernel.elf", "initramfs.cpio"):
        command(["mcopy", "-i", esp, payload / name, f"::/boot/{name}"], out)
    disk = out / "moruna-node.raw"
    with disk.open("xb") as file:
        file.truncate((esp_mib + 4) * mib)
    command(["sgdisk", "--clear", f"--new=1:2048:+{esp_mib}M", "--typecode=1:ef00", disk], out)
    with disk.open("r+b") as destination, esp.open("rb") as source:
        destination.seek(2048 * 512)
        shutil.copyfileobj(source, destination)
    evidence = {}
    for path in [description, *(payload / p.relative_to(assets) for p in files), payload / "manifest", payload / "initramfs.cpio", payload / "moruna-kernel.elf", boot, disk]:
        evidence[str(path)] = {"size": path.stat().st_size, "sha256": hashlib.file_digest(path.open("rb"), "sha256").hexdigest()}
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
    (out / "node-image.json").write_text(json.dumps({
        "version": 1, "architecture": "x86_64", "boot_mode": "multiboot2",
        "kernel": artifact(payload / "moruna-kernel.elf"),
        "initramfs": artifact(payload / "initramfs.cpio"),
        "layers": layers, "writable_disks": writable_disks, "user_network": args.user_network, "cid": args.cid, "agent_port": args.agent_port, "resources": resources,
    }, indent=2) + "\n")
    (out / "inputs-and-artifacts.json").write_text(json.dumps(evidence, indent=2) + "\n")
    print(f"Packaged {disk}; boot and hardware validation remain required.")


if __name__ == "__main__":
    main()
