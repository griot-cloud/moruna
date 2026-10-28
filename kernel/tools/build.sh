#!/usr/bin/env bash
# Builds the Moruna microkernel and the KB1 test programs, and packs the initramfs.
#
#   kernel/tools/build.sh [OUT]       OUT defaults to kernel/target/boot
#
# Writes OUT/moruna-microkernel.elf (the kernel), OUT/moruna-microkernel.qemu.elf (the
# same image with e_machine set to EM_386, which QEMU's multiboot loader requires of
# `-kernel`, as `cargo osdk run` does), and OUT/initramfs.cpio (newc: the manifest and
# bin/). Plain cargo: core and alloc are built from source for x86_64-unknown-none.
set -euo pipefail

kernel="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$kernel/target/boot}"
target=x86_64-unknown-none

# The flags `cargo osdk build` passes for an OSTD kernel (OSDK 0.19.0,
# osdk/src/commands/build/mod.rs), less `+ermsb`, which is a speed workaround that
# current toolchains warn is unstable.
export RUSTFLAGS="-C relocation-model=static -C relro-level=off -C force-unwind-tables=yes --check-cfg cfg(ktest) -C no-redzone=y"

cd "$kernel"
# Two invocations, so the kernel's features (the manifest parser, which allocates) are
# not unified into the programs, which have no allocator.
for package in moruna-microkernel-boot moruna-kb1-programs; do
  cargo build --release --locked --target "$target" \
    -Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem \
    -p "$package"
done

bin="$kernel/target/$target/release"
rm -rf "$out"
mkdir -p "$out/initramfs/bin"
cp initramfs/manifest "$out/initramfs/manifest"
for program in supervisor child fault ping pong xfer-a xfer-b iso-holder isolation deadline; do
  cp "$bin/$program" "$out/initramfs/bin/$program"
done
(cd "$out/initramfs" && find . -mindepth 1 | LC_ALL=C sort | cpio -o -H newc --quiet) > "$out/initramfs.cpio"

cp "$bin/moruna-microkernel" "$out/moruna-microkernel.elf"
cp "$bin/moruna-microkernel" "$out/moruna-microkernel.qemu.elf"
python3 - "$out/moruna-microkernel.qemu.elf" <<'PY'
import sys
path = sys.argv[1]
with open(path, "r+b") as f:
    head = f.read(20)
    if head[:4] != b"\x7fELF" or head[18:20] != b"\x3e\x00":
        sys.exit(f"{path}: not an x86_64 ELF")
    f.seek(18)
    f.write(b"\x03\x00")
PY
ls -l "$out/moruna-microkernel.elf" "$out/initramfs.cpio"
