#!/usr/bin/env bash
# Builds the Moruna microkernel, every userland program of the kernel workspace, and
# the boot image a description names.
#
#   kernel/tools/build.sh [OUT] [DESCRIPTION] [PYTHON_LAYOUT] [HOST_TARGET]
#
# OUT defaults to kernel/target/boot, DESCRIPTION to kernel/initramfs/boot.image,
# PYTHON_LAYOUT to kernel/target/oci/python, the OCI layout of the official
# python:3.14-slim image for linux/amd64 (kernel/tools/python-layout.sh writes it), and
# HOST_TARGET, the target directory the machine's own programs are built in and kept
# between builds (the vsock's back end), to the root workspace's target/host.
# The Linux personality's image (SDD 28 k, SDD 30 k) is derived from that image: its
# layers with one more on top holding the C test programs of kernel/linux/programs, built
# static with musl and dynamic with glibc, and the engine with its Python task and fixture,
# and the Python task as its command (`moruna-layer derive`). Its layers are
# OUT/linux/stack-N.img, their list OUT/linux/stack.layers, its record OUT/linux/image.record
# (what the engine checks an image entry's reference against, SDD 30 f.12) and its manifest
# digest OUT/linux/image.digest. OUT/moruna-vhost-vsock is the vsock's vhost-user back end
# (SDD 30 f.11), a program of the machine QEMU runs on, which kernel/tools/boot.sh starts.
# Writes OUT/moruna-microkernel.elf (the kernel), OUT/moruna-microkernel.qemu.elf (the
# same image with e_machine set to EM_386, which QEMU's multiboot loader requires of
# `-kernel`, as `cargo osdk run` does), OUT/layers (the image filesystem's two layer
# disks, its layer list and the check list of their merged tree, SDD 27 k, written by
# the root workspace's `moruna-layer fixture`), and, through moruna-image (SDD 23,
# f.8), OUT/initramfs.cpio, OUT/manifest (the boot manifest it carries) and OUT/expect
# (the lines a boot of it must print). Plain cargo: core and alloc are built from
# source separately for the builtin kernel target and canonical SSE2/SysV user target.
set -euo pipefail

kernel="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$kernel/target/boot}"
description="${2:-$kernel/initramfs/boot.image}"
python_layout="${3:-$kernel/target/oci/python}"
host_target="${4:-$kernel/../target/host}"
# Resolve caller-relative inputs before changing into the kernel workspace.
absolute_path() {
  case "$1" in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s/%s\n' "$PWD" "$1" ;;
  esac
}
out="$(absolute_path "$out")"
description="$(absolute_path "$description")"
python_layout="$(absolute_path "$python_layout")"
host_target="$(absolute_path "$host_target")"
if [ ! -f "$description" ]; then
  echo "build.sh: image description does not exist: $description" >&2
  exit 1
fi
target=x86_64-unknown-none
# Pinned nightly JSON target selects the user ABI; never override kernel soft-float.
user_target="$kernel/targets/x86_64-moruna-user.json"
user_target_name=x86_64-moruna-user
host="$(rustc -vV | sed -n 's/^host: //p')"
build_std=(-Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem)

# The flags `cargo osdk build` passes for an OSTD kernel (OSDK 0.19.0,
# osdk/src/commands/build/mod.rs), less `+ermsb`, which is a speed workaround that
# current toolchains warn is unstable.
flags="--cfg polyval_force_soft -C relocation-model=static -C relro-level=off -C force-unwind-tables=yes --check-cfg cfg(ktest) -C no-redzone=y"

# This canonical entry owns dependency-wide flags. An inherited encoded override
# must not silently supersede the approved backend or inject kernel flags into host tools.
unset CARGO_ENCODED_RUSTFLAGS
export RUSTFLAGS="--cfg polyval_force_soft"
cd "$kernel"
# The CI boot's kernel answers `plane_probe` (SDD 29 k): the frames and copies the data
# plane's tests measure.
RUSTFLAGS="$flags" cargo build --release --locked --target "$target" "${build_std[@]}" \
  -p moruna-microkernel-boot --features probe
# Every userland crate (the test programs, the SDK, the examples) in a second
# invocation, so the kernel's features (the manifest parser, which allocates) are not
# unified into them; each program is linked static with the user linker script, whose
# segments are what the kernel's loader takes (SDD 20, f.2). A new program needs no
# change here.
# Match the separately exercised native-link invocation and retain exact toolchain
# and argv evidence if Cargo rejects the custom target before compiling a crate.
rustup run nightly-2026-07-21 cargo --version
rustup run nightly-2026-07-21 rustc --version
# The audited freestanding Ring build uses Clang's -nostdlibinc and a real ELF
# baremetal target. Bind only this userland invocation, never host/musl tools.
user_cc="${CC_x86_64_moruna_user:-clang}"
"$user_cc" --version
set -x
CC_x86_64_moruna_user="$user_cc" \
CFLAGS_x86_64_moruna_user="--target=x86_64-unknown-none -ffreestanding -mno-red-zone -fno-stack-protector ${CFLAGS_x86_64_moruna_user:-}" \
RUSTFLAGS="$flags -Zunstable-options -C link-arg=-T$kernel/sys/user.ld -C link-arg=-static" \
  rustup run nightly-2026-07-21 cargo build --release --locked --target "$user_target" \
  -Zunstable-options -Zjson-target-spec "${build_std[@]}" --workspace \
  --features moruna-storage-service/service \
  --exclude moruna-microkernel --exclude moruna-microkernel-boot --exclude moruna-image --exclude moruna-node-image
set +x

bin="${CARGO_TARGET_DIR:-$kernel/target}/$user_target_name/release"
kernel_bin="${CARGO_TARGET_DIR:-$kernel/target}/$target/release"
rm -rf "$out"
mkdir -p "$out"
cp "$bin/deadline" "$out/deadline"
# The layer disks are the host writer's (moruna-image, the root workspace, on its own
# toolchain); the boot description gives their lists as `data:` handles.
(cd "$kernel/.." && cargo run --quiet --release --locked -p moruna-image --features testing \
  --bin moruna-layer -- fixture "$out/layers")
# SDD 29 k: the data plane's records and framings, and KA10's bombs as records.
(cd "$kernel/.." && cargo run --quiet --release --locked -p moruna-testkit \
  --bin plane-fixture -- "$out/plane")

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
for p in hello threads refuse isolate bench granted hog tap badrecord; do
  "${muslcc[@]}" -static -O2 -Wall -Werror -s -o "$programs/kc1/bin/$p" "$kernel/linux/programs/$p.c"
done
"$gnucc" -O2 -Wall -Werror -s -o "$programs/kc1/bin/dynamic" "$kernel/linux/programs/dynamic.c"
# SDD 29 k (DP-T16): KA1's arrow_echo, unmodified, as a static musl program: a Rust program
# that speaks Arrow IPC on its standard streams and knows no Moruna library.
musl=x86_64-unknown-linux-musl
if ! rustup target list --installed --toolchain "$(cd "$kernel/.." && rustup show active-toolchain | cut -d' ' -f1)" | grep -qx "$musl"; then
  echo "build.sh: the root toolchain has no $musl target (rustup target add $musl)" >&2
  exit 1
fi
(cd "$kernel/.." && \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$gnucc" \
  CC_x86_64_unknown_linux_musl="${muslcc[0]}" \
  CFLAGS_x86_64_unknown_linux_musl="${muslcc[*]:1}" \
  RUSTFLAGS="--cfg polyval_force_soft -C link-self-contained=yes -C target-feature=+crt-static" \
  cargo build --quiet --release --locked --target "$musl" -p moruna-image --bin arrow_echo)
cp "${CARGO_TARGET_DIR:-$kernel/../target}/$musl/release/arrow_echo" "$programs/kc1/bin/arrow_echo"
# SDD 30 k: Moruna's engine itself, the root workspace's moruna-engine, an unmodified static musl
# program the personality runs with granted handles; its Python task, which reads and writes Arrow
# IPC with the standard library alone, and the rows it reads.
(cd "$kernel/.." && \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$gnucc" \
  CC_x86_64_unknown_linux_musl="${muslcc[0]}" \
  CFLAGS_x86_64_unknown_linux_musl="${muslcc[*]:1}" \
  RUSTFLAGS="--cfg polyval_force_soft -C link-self-contained=yes -C target-feature=+crt-static -C strip=symbols" \
  cargo build --quiet --release --locked --target "$musl" -p moruna-runtime --bin moruna-engine)
mkdir -p "$programs/moruna/bin" "$programs/kd2"
cp "${CARGO_TARGET_DIR:-$kernel/../target}/$musl/release/moruna-engine" "$programs/moruna/bin/moruna-engine"
cp "$kernel/linux/programs/kd2/transform.py" "$kernel/linux/programs/kd2/arrowlite.py" "$programs/kd2/"
(cd "$kernel/.." && cargo run --quiet --release --locked -p moruna-testkit \
  --bin kd2-fixture -- "$programs/kd2")
cp "$kernel/linux/programs/greeting.txt" "$programs/kc1/greeting.txt"
if [ ! -r "$python_layout/index.json" ]; then
  echo "build.sh: no OCI layout of python:3.14-slim at $python_layout (kernel/tools/python-layout.sh writes it)" >&2
  exit 1
fi
# The layout's linux/amd64 manifest, through any image index above it.
python_manifest="$(python3 "$kernel/tools/oci-manifest.py" "$python_layout" linux amd64)"
# The derived image, whose command is the Python task; then its layers on block3 onward
# (block0 is the test disk, block1 and block2 the image filesystem's fixture), its record and
# its digest.
(cd "$kernel/.." && cargo run --quiet --release --locked -p moruna-image \
  --bin moruna-layer -- derive "$python_layout" "$python_manifest" "$programs" kd2 \
  "$out/linux/image" python3 /kd2/transform.py)
(cd "$kernel/.." && cargo run --quiet --release --locked -p moruna-image \
  --bin moruna-layer -- stack "$out/linux/image" kd2 "$out/linux" 3)
# Native node boot has only the two readonly fixture disks in the `block`
# service; its independently owned raw home is `nvme0`, not another blockN.
# Emit a separately sealed canonical stack/list for FIRST=2 while preserving
# generic FIRST=3 above. This duplicates one bounded image stack on disk.
(cd "$kernel/.." && cargo run --quiet --release --locked -p moruna-image \
  --bin moruna-layer -- stack "$out/linux/image" kd2 "$out/node-linux" 2)
rm -rf "$out/linux/image"
# SDD 30 f.11: the vsock's vhost-user back end, for the machine QEMU runs on, built in
# HOST_TARGET, where the next build finds it built.
(cd "$kernel/.." && cargo build --quiet --release --locked -p moruna-vmm --bin moruna-vhost-vsock \
  --target-dir "$host_target")
cp "$host_target/release/moruna-vhost-vsock" "$out/moruna-vhost-vsock"
# The actual host peer shares the runtime's events7 framing/codec; no JSON-only peer.
(cd "$kernel/.." && cargo build --quiet --release --locked -p moruna-runtime --example kd3-peer \
  --target-dir "$host_target")
cp "$host_target/release/examples/kd3-peer" "$out/moruna-kd3-peer"
# Production-node successor tools are REQUIRED inputs to the15-case gate, not optional.
(cd "$kernel/.." && cargo build --quiet --release --locked -p moruna-runtime --example node-ci-assets --target-dir "$host_target")
(cd "$kernel/.." && cargo build --quiet --release --locked -p moruna-storage --bin moruna-storage-provision --target-dir "$host_target")
cp "$host_target/release/examples/node-ci-assets" "$out/moruna-node-ci-assets"
cp "$host_target/release/moruna-storage-provision" "$out/moruna-storage-provision"
cargo build --quiet --release --locked --target "$host" -p moruna-image -p moruna-node-image --target-dir "$host_target/node-tools"
cp "$host_target/node-tools/$host/release/moruna-image" "$out/moruna-image-host"
cp "$host_target/node-tools/$host/release/moruna-node-image" "$out/moruna-node-image"
mkdir "$out/node-user-bins"
for program in node-supervisor node-admission devices imagefs linux netstack engine storage virtio-blk virtio-vsock virtio-net nvme gve; do
  cp "$bin/$program" "$out/node-user-bins/$program"
done
# SDD 30 d.3: the engine's start, which its supervisor reads from a data handle.
mkdir -p "$out/engine"
cp "$kernel/initramfs/engine.conf" "$out/engine/engine.conf"
cargo run --quiet --release --locked --target "$host" -p moruna-image -- "$description" "$bin" "$out"

cp "$kernel_bin/moruna-microkernel" "$out/moruna-microkernel.elf"
cp "$kernel_bin/moruna-microkernel" "$out/moruna-microkernel.qemu.elf"
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
