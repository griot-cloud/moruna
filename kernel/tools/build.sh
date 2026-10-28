#!/usr/bin/env bash
# Builds the Moruna microkernel, every userland program of the kernel workspace, and
# the boot image a description names.
#
#   kernel/tools/build.sh [OUT] [DESCRIPTION]
#
# OUT defaults to kernel/target/boot and DESCRIPTION to kernel/initramfs/boot.image.
# Writes OUT/moruna-microkernel.elf (the kernel), OUT/moruna-microkernel.qemu.elf (the
# same image with e_machine set to EM_386, which QEMU's multiboot loader requires of
# `-kernel`, as `cargo osdk run` does), and, through moruna-image (SDD 23, f.8),
# OUT/initramfs.cpio, OUT/manifest (the boot manifest it carries) and OUT/expect (the
# lines a boot of it must print). Plain cargo: core and alloc are built from source
# for x86_64-unknown-none.
set -euo pipefail

kernel="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$kernel/target/boot}"
description="${2:-$kernel/initramfs/boot.image}"
target=x86_64-unknown-none
host="$(rustc -vV | sed -n 's/^host: //p')"
build_std=(-Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem)

# The flags `cargo osdk build` passes for an OSTD kernel (OSDK 0.19.0,
# osdk/src/commands/build/mod.rs), less `+ermsb`, which is a speed workaround that
# current toolchains warn is unstable.
flags="-C relocation-model=static -C relro-level=off -C force-unwind-tables=yes --check-cfg cfg(ktest) -C no-redzone=y"

cd "$kernel"
RUSTFLAGS="$flags" cargo build --release --locked --target "$target" "${build_std[@]}" \
  -p moruna-microkernel-boot
# Every userland crate (the test programs, the SDK, the examples) in a second
# invocation, so the kernel's features (the manifest parser, which allocates) are not
# unified into them; each program is linked static with the user linker script, whose
# segments are what the kernel's loader takes (SDD 20, f.2). A new program needs no
# change here.
RUSTFLAGS="$flags -C link-arg=-T$kernel/sys/user.ld -C link-arg=-static" \
  cargo build --release --locked --target "$target" "${build_std[@]}" --workspace \
  --exclude moruna-microkernel --exclude moruna-microkernel-boot --exclude moruna-image

bin="$kernel/target/$target/release"
rm -rf "$out"
mkdir -p "$out"
cargo run --quiet --release --locked --target "$host" -p moruna-image -- "$description" "$bin" "$out"

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
