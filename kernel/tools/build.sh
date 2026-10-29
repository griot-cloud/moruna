#!/usr/bin/env bash
# Builds the Moruna microkernel, every userland program of the kernel workspace, and
# the boot image a description names.
#
#   kernel/tools/build.sh [OUT] [DESCRIPTION] [PYTHON_LAYOUT]
#
# OUT defaults to kernel/target/boot, DESCRIPTION to kernel/initramfs/boot.image and
# PYTHON_LAYOUT to kernel/target/oci/python, the OCI layout of the official
# python:3.14-slim image for linux/amd64 (kernel/tools/python-layout.sh writes it).
# The Linux personality's image (SDD 28 k) is that image's layers with one more on top
# holding the C test programs of kernel/linux/programs, built static with musl and
# dynamic with glibc: OUT/linux/stack-N.img, and their list OUT/linux/stack.layers.
# Writes OUT/moruna-microkernel.elf (the kernel), OUT/moruna-microkernel.qemu.elf (the
# same image with e_machine set to EM_386, which QEMU's multiboot loader requires of
# `-kernel`, as `cargo osdk run` does), OUT/layers (the image filesystem's two layer
# disks, its layer list and the check list of their merged tree, SDD 27 k, written by
# the root workspace's `moruna-layer fixture`), and, through moruna-image (SDD 23,
# f.8), OUT/initramfs.cpio, OUT/manifest (the boot manifest it carries) and OUT/expect
# (the lines a boot of it must print). Plain cargo: core and alloc are built from
# source for x86_64-unknown-none.
set -euo pipefail

kernel="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$kernel/target/boot}"
description="${2:-$kernel/initramfs/boot.image}"
python_layout="${3:-$kernel/target/oci/python}"
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

bin="${CARGO_TARGET_DIR:-$kernel/target}/$target/release"
rm -rf "$out"
mkdir -p "$out"
# The layer disks are the host writer's (moruna-image, the root workspace, on its own
# toolchain); the boot description gives their lists as `data:` handles.
(cd "$kernel/.." && cargo run --quiet --release --locked -p moruna-image --features testing \
  --bin moruna-layer -- fixture "$out/layers")

# SDD 28: the Linux test programs, for x86_64: static with musl, and one dynamic with
# glibc, which the image's own Debian layer gives its interpreter and libc.
if command -v x86_64-linux-gnu-gcc >/dev/null; then
  gnucc=x86_64-linux-gnu-gcc
elif [ "$(uname -m)" = x86_64 ] && command -v gcc >/dev/null; then
  gnucc=gcc
else
  echo "build.sh: no C compiler for x86_64 Linux (gcc-x86-64-linux-gnu, or gcc on x86_64)" >&2
  exit 1
fi
specs=/usr/lib/x86_64-linux-musl/musl-gcc.specs
if [ "$(uname -m)" = x86_64 ] && command -v musl-gcc >/dev/null; then
  muslcc=(musl-gcc)
elif [ -r "$specs" ]; then
  muslcc=("$gnucc" -specs="$specs")
else
  echo "build.sh: no musl for x86_64 (musl-tools, or musl-dev:amd64 with $specs)" >&2
  exit 1
fi
programs="$out/linux/programs"
mkdir -p "$programs/kc1/bin"
for p in hello threads refuse isolate bench; do
  "${muslcc[@]}" -static -O2 -Wall -Werror -s -o "$programs/kc1/bin/$p" "$kernel/linux/programs/$p.c"
done
"$gnucc" -O2 -Wall -Werror -s -o "$programs/kc1/bin/dynamic" "$kernel/linux/programs/dynamic.c"
cp "$kernel/linux/programs/greeting.txt" "$programs/kc1/greeting.txt"
if [ ! -r "$python_layout/index.json" ]; then
  echo "build.sh: no OCI layout of python:3.14-slim at $python_layout (kernel/tools/python-layout.sh writes it)" >&2
  exit 1
fi
# The layout's linux/amd64 manifest, through any image index above it.
python_manifest="$(python3 "$kernel/tools/oci-manifest.py" "$python_layout" linux amd64)"
# The image's layers on block3 onward: block0 is the test disk, block1 and block2 the
# image filesystem's fixture.
(cd "$kernel/.." && cargo run --quiet --release --locked -p moruna-image \
  --bin moruna-layer -- stack "$python_layout" "$python_manifest" "$programs" "$out/linux" 3)
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
