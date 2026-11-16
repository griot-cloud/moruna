#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# B2's booted test: builds the microkernel, the Linux personality and linuxlayer-boot, an
# initramfs whose /init is a static musl program, renders linuxlayer.image with
# moruna-image, and boots it under QEMU (KVM when usable). Prints the serial log, then
# judges it: every line the description expects, no failure, no panic.
#
#   kernel/linuxlayer/boot/boot.sh [OUT] [TIMEOUT_SECONDS] [INIT_C] [CMDLINE]
#
# Needs what kernel/tools/build.sh needs for the kernel and the Linux programs (the
# pinned nightly, musl for x86_64, QEMU). CARGO_TARGET_DIR, when set, is where the three
# builds go.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
layer="$(dirname "$here")"
kernel="$(dirname "$layer")"
out="${1:-$kernel/target/linuxlayer-boot}"
seconds="${2:-120}"
init_c="${3:-$kernel/linux/programs/hello.c}"
cmdline="${4:-}"
targets="${CARGO_TARGET_DIR:-$kernel/target}"
target=x86_64-unknown-none
user_target="$kernel/targets/x86_64-moruna-user.json"
host="$(rustc -vV | sed -n 's/^host: //p')"
build_std=(-Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem)
common="-C relro-level=off -C force-unwind-tables=yes --check-cfg cfg(ktest) -C no-redzone=y"
unset CARGO_ENCODED_RUSTFLAGS
export CARGO_INCREMENTAL=0

# The microkernel, as build.sh builds it.
(cd "$kernel" && CARGO_TARGET_DIR="$targets" \
  RUSTFLAGS="--cfg polyval_force_soft -C relocation-model=static $common" \
  cargo build --release --locked --target "$target" "${build_std[@]}" -p moruna-microkernel-boot)
# The personality links inside its region, far above 4 GiB: position-independent code,
# linked at its address (personality/build.rs).
(cd "$layer" && CARGO_TARGET_DIR="$targets/linuxlayer-image" \
  RUSTFLAGS="-C relocation-model=pie -C code-model=small $common" \
  cargo build --release --locked --target "$user_target" -Zunstable-options -Zjson-target-spec \
  "${build_std[@]}" -p linuxlayer-personality)
# linuxlayer-boot links as every Moruna program does.
(cd "$layer" && CARGO_TARGET_DIR="$targets/linuxlayer-boot" \
  RUSTFLAGS="-C relocation-model=static $common" \
  cargo build --release --locked --target "$user_target" -Zunstable-options -Zjson-target-spec \
  "${build_std[@]}" -p linuxlayer-launch --features launcher --bin linuxlayer-boot)

rm -rf "$out"
mkdir -p "$out/bins" "$out/linuxlayer" "$out/root/kc1" "$out/root/tmp"
cp "$targets/linuxlayer-boot/x86_64-moruna-user/release/linuxlayer-boot" "$out/bins/"
cp "$targets/linuxlayer-image/x86_64-moruna-user/release/linuxlayer-personality" \
  "$out/linuxlayer/personality"

# The initramfs: /init, and the file hello.c reads.
specs=/usr/lib/x86_64-linux-musl/musl-gcc.specs
if [ "$(uname -m)" = x86_64 ] && command -v musl-gcc >/dev/null; then
  muslcc=(musl-gcc)
elif [ -r "$specs" ]; then
  muslcc=("${CC:-x86_64-linux-gnu-gcc}" -specs="$specs")
else
  echo "boot.sh: no musl for x86_64 (musl-tools)" >&2
  exit 1
fi
"${muslcc[@]}" -static -O2 -Wall -Werror -s -o "$out/root/init" "$init_c"
cp "$kernel/linux/programs/greeting.txt" "$out/root/kc1/greeting.txt"
python3 "$here/cpio.py" "$out/root" "$out/linuxlayer/initramfs.cpio"
printf '%s\0' "$cmdline" > "$out/linuxlayer/cmdline"

(cd "$kernel" && RUSTFLAGS="--cfg polyval_force_soft" CARGO_TARGET_DIR="$targets" \
  cargo run --quiet --release --locked --target "$host" -p moruna-image -- \
  "$here/linuxlayer.image" "$out/bins" "$out")
# QEMU boots a Multiboot kernel only as an EM_386 ELF: the same private copy build.sh
# makes.
python3 - "$targets/$target/release/moruna-microkernel" "$out/moruna-microkernel.qemu.elf" <<'PY'
import sys
data = bytearray(open(sys.argv[1], "rb").read())
if data[:4] != b"\x7fELF" or data[18:20] != b"\x3e\x00":
    sys.exit("not an x86_64 kernel ELF")
data[18:20] = b"\x03\x00"
open(sys.argv[2], "wb").write(data)
PY

if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
  accel=(-accel kvm -cpu host -machine q35,kernel-irqchip=split)
else
  accel=(-accel tcg -cpu max -machine q35)
fi
set +e
timeout --kill-after=5 "$seconds" qemu-system-x86_64 "${accel[@]}" -m 2G -smp 2 \
  -nographic -display none -monitor none -serial stdio -no-reboot \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
  -kernel "$out/moruna-microkernel.qemu.elf" -initrd "$out/initramfs.cpio" \
  | tee "$out/serial.log"
status="${PIPESTATUS[0]}"
set -e
echo "boot: qemu exited with status $status"

fail=0
while IFS= read -r line; do
  [ -z "$line" ] && continue
  if grep -qF -- "$line" "$out/serial.log"; then
    echo "ok: $line"
  else
    echo "MISSING: $line"
    fail=1
  fi
done < "$out/expect"
if grep -qE -- "FAIL:|panic" "$out/serial.log"; then
  echo "FOUND a failure or a panic:"
  grep -E -- "FAIL:|panic" "$out/serial.log"
  fail=1
fi
# 33: the kernel powered off with success.
[ "$status" = 33 ] || { echo "QEMU's status $status is not the kernel's success (33)"; fail=1; }
exit "$fail"
