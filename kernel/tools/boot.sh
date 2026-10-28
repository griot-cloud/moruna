#!/usr/bin/env bash
# Boots the kernel built by build.sh under QEMU, the serial line on stdout, and exits
# with QEMU's status: 33 when the kernel powers off with success (isa-debug-exit,
# (0x10 << 1) | 1), 65 on failure, 124 on the timeout. KVM when /dev/kvm is usable,
# TCG otherwise.
#
#   kernel/tools/boot.sh [OUT] [TIMEOUT_SECONDS]
set -euo pipefail

kernel="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$kernel/target/boot}"
seconds="${2:-300}"

if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
  accel=(-accel kvm -cpu host -machine q35,kernel-irqchip=split)
  echo "boot: KVM" >&2
else
  accel=(-accel tcg -cpu max -machine q35)
  echo "boot: TCG (no usable /dev/kvm)" >&2
fi

set +e
timeout "$seconds" qemu-system-x86_64 \
  "${accel[@]}" \
  -m 1G -smp 1 \
  -nographic -display none -monitor none -serial stdio -no-reboot \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
  -kernel "$out/moruna-microkernel.qemu.elf" \
  -initrd "$out/initramfs.cpio"
status=$?
set -e
echo "boot: qemu exited with status $status" >&2
exit "$status"
