#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Stream B2-vm's measurement boot: what a page costs the personality, under KVM. Builds the
# microkernel and b2vm-cost, renders cost.image with moruna-image, boots it, and prints the
# figures (`B2VM cost: ...`). No shell runs in the guest: the one process is b2vm-cost.
#
#   kernel/linuxlayer/vmcost/cost.sh [OUT] [TIMEOUT_SECONDS]
#
# Needs what kernel/linuxlayer/boot/boot.sh needs (the pinned nightly, QEMU, KVM for figures
# that mean anything). CARGO_TARGET_DIR, when set, is where the builds go.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
kernel="$(cd "$here/../.." && pwd)"
out="${1:-$kernel/target/linuxlayer-vmcost}"
seconds="${2:-300}"
targets="${CARGO_TARGET_DIR:-$kernel/target}"
target=x86_64-unknown-none
user_target="$kernel/targets/x86_64-moruna-user.json"
host="$(rustc -vV | sed -n 's/^host: //p')"
build_std=(-Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem)
common="-C relro-level=off -C force-unwind-tables=yes --check-cfg cfg(ktest) -C no-redzone=y"
unset CARGO_ENCODED_RUSTFLAGS
export CARGO_INCREMENTAL=0

(cd "$kernel" && CARGO_TARGET_DIR="$targets" \
  RUSTFLAGS="--cfg polyval_force_soft -C relocation-model=static $common" \
  cargo build --release --locked --target "$target" "${build_std[@]}" -p moruna-microkernel-boot)
(cd "$here" && CARGO_TARGET_DIR="$targets/linuxlayer-vmcost" \
  RUSTFLAGS="-C relocation-model=static $common" \
  cargo build --release --locked --target "$user_target" -Zunstable-options -Zjson-target-spec \
  "${build_std[@]}")

rm -rf "$out"
mkdir -p "$out/bins"
cp "$targets/linuxlayer-vmcost/x86_64-moruna-user/release/b2vm-cost" "$out/bins/"
(cd "$kernel" && RUSTFLAGS="--cfg polyval_force_soft" CARGO_TARGET_DIR="$targets" \
  cargo run --quiet --release --locked --target "$host" -p moruna-image -- \
  "$here/cost.image" "$out/bins" "$out")
# QEMU boots a Multiboot kernel only as an EM_386 ELF: the copy boot.sh makes.
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
  echo "cost: no usable /dev/kvm, so TCG: the figures mean nothing" >&2
fi
set +e
timeout --kill-after=5 "$seconds" qemu-system-x86_64 "${accel[@]}" -m 2G -smp 2 \
  -nographic -display none -monitor none -serial stdio -no-reboot \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
  -kernel "$out/moruna-microkernel.qemu.elf" -initrd "$out/initramfs.cpio" \
  > "$out/serial.log"
status=$?
set -e
echo "cost: qemu exited with status $status"
grep -E "B2VM cost|FAIL|panic" "$out/serial.log" || true
grep -q "B2VM cost: tester: done" "$out/serial.log" && [ "$status" = 33 ]
