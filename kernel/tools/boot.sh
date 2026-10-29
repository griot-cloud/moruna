#!/usr/bin/env bash
# Boots the kernel built by build.sh under QEMU, the serial line on stdout, and exits
# with QEMU's status: 33 when the kernel powers off with success (isa-debug-exit,
# (0x10 << 1) | 1), 65 on failure, 124 on the timeout. KVM when /dev/kvm is usable,
# TCG otherwise.
#
#   kernel/tools/boot.sh [OUT] [TIMEOUT_SECONDS] [CPUS]
#
# CPUS (default 1) is the number of virtual CPUs; SV-T12 boots with 2 and 4.
#
# The machine carries the devices SDD 25's tests drive, each at a fixed PCI slot:
# two virtio consoles (00:03, whose output goes to OUT/console0-CPUS.log, and 00:04,
# whose input is OUT/console1-input.txt), a virtio-blk disk (00:05,
# OUT/disk-CPUS.img, written with pattern A by disk-image.py before the boot, reads
# throttled to 8 MiB/s so DR-T11's 32 MiB of reads are still in flight when the
# driver is killed, on any number of CPUs), a
# vhost vsock with guest cid 3 (00:06) when /dev/vhost-vsock is usable, with
# vsock-peer.py as the host's side (OUT/vsock-peer-CPUS.log), and QEMU's edu test
# device (00:07). OUT/vsock-CPUS says whether the vsock was attached, and why not.
set -euo pipefail

kernel="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$kernel/target/boot}"
seconds="${2:-300}"
cpus="${3:-1}"

if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
  accel=(-accel kvm -cpu host -machine q35,kernel-irqchip=split)
  echo "boot: KVM" >&2
else
  accel=(-accel tcg -cpu max -machine q35)
  echo "boot: TCG (no usable /dev/kvm)" >&2
fi

disk="$out/disk-$cpus.img"
console0="$out/console0-$cpus.log"
console1="$out/console1-$cpus.log"
input="$out/console1-input.txt"
python3 "$kernel/tools/disk-image.py" write "$disk"
: > "$console0"
printf 'KB3 console input: hello from the host\n' > "$input"
devices=(
  -device virtio-serial-pci,id=serial0,addr=0x3,disable-legacy=on
  -chardev "file,id=console0,path=$console0"
  -device virtconsole,bus=serial0.0,chardev=console0
  -device virtio-serial-pci,id=serial1,addr=0x4,disable-legacy=on
  -chardev "file,id=console1,path=$console1,input-path=$input"
  -device virtconsole,bus=serial1.0,chardev=console1
  -drive "file=$disk,format=raw,if=none,id=disk0,throttling.bps-read=8388608"
  -device virtio-blk-pci,drive=disk0,addr=0x5,disable-legacy=on
  -device edu,addr=0x7
)
peer=""
if [ -r /dev/vhost-vsock ] && [ -w /dev/vhost-vsock ]; then
  devices+=(-device vhost-vsock-pci,guest-cid=3,addr=0x6)
  python3 "$kernel/tools/vsock-peer.py" "$out/vsock-peer-$cpus.log" 3 &
  peer=$!
  echo attached > "$out/vsock-$cpus"
  echo "boot: vsock attached (guest cid 3), the host's peer listening on port 5000" >&2
else
  echo "absent: no usable /dev/vhost-vsock on this machine, so QEMU cannot give it a vsock" > "$out/vsock-$cpus"
  echo "boot: no vsock: no usable /dev/vhost-vsock" >&2
fi

set +e
timeout "$seconds" qemu-system-x86_64 \
  "${accel[@]}" \
  -m 1G -smp "$cpus" \
  -nographic -display none -monitor none -serial stdio -no-reboot \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
  "${devices[@]}" \
  -kernel "$out/moruna-microkernel.qemu.elf" \
  -initrd "$out/initramfs.cpio"
status=$?
if [ -n "$peer" ]; then
  # The peer ends by itself once both exchanges are done; after QEMU has gone it
  # never will, and its log already says what it saw.
  kill "$peer" 2>/dev/null
  wait "$peer" 2>/dev/null
fi
set -e
echo "boot: qemu exited with status $status" >&2
exit "$status"
