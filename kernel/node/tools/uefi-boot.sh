#!/usr/bin/env bash
# Boots the node's disk image, OUT/moruna-node.raw (package.py --profile cloud), from UEFI
# firmware in QEMU: the machine of system design 1A and work item 1.1. A q35 machine, one
# read-only virtio disk (the node never writes its boot disk), one virtio network card, and the
# first serial port written to OUT/console.log. No direct kernel load, no vsock, no host socket.
#
#   kernel/node/tools/uefi-boot.sh OUT CPUS [FORMAT]
#
# FORMAT is the boot disk's format (work item 1.5), one that package.py --formats wrote: raw
# (the default), qcow2 (OUT/moruna-node.qcow2) or vhd (OUT/moruna-node.vhd, which QEMU reads
# with its vpc driver). The machine is the same for each.
#
# Exits 0 once `moruna: services started` is on the console, within 120 s under KVM or 900 s
# under the emulator, and prints how long after QEMU's start it came. Exits 124 when the time
# runs out, and 1 when QEMU ends before the line. QEMU is stopped either way. The firmware is
# QEMU's own: edk2-x86_64-code.fd read-only, and a fresh copy of its edk2 vars file
# (edk2-i386-vars.fd, the vars template QEMU's descriptors give its x86_64 code) as
# OUT/edk2-vars.fd.
set -euo pipefail

if [ "$#" -ne 2 ] && [ "$#" -ne 3 ]; then
  echo "usage: uefi-boot.sh OUT CPUS [FORMAT]" >&2
  exit 2
fi
out="$(cd "$1" && pwd)"
cpus="$2"
case "$cpus" in
  '' | *[!0-9]* | 0*) echo "uefi-boot.sh: CPUS must be a positive number: $cpus" >&2; exit 2 ;;
esac
format="${3:-raw}"
# QEMU's name for the driver that reads each format.
case "$format" in
  raw | qcow2) driver="$format" ;;
  vhd) driver=vpc ;;
  *) echo "uefi-boot.sh: FORMAT must be raw, qcow2 or vhd: $format" >&2; exit 2 ;;
esac
disk="$out/moruna-node.$format"
if [ ! -f "$disk" ]; then
  echo "uefi-boot.sh: no disk image at $disk (package.py --profile cloud --formats $format writes it)" >&2
  exit 2
fi
qemu="$(command -v qemu-system-x86_64)" || { echo "uefi-boot.sh: qemu-system-x86_64 is not on PATH" >&2; exit 2; }
firmware="$(cd "$(dirname "$qemu")/../share/qemu" && pwd)"
code="$firmware/edk2-x86_64-code.fd"
vars_template="$firmware/edk2-i386-vars.fd"
for file in "$code" "$vars_template"; do
  if [ ! -r "$file" ]; then
    echo "uefi-boot.sh: QEMU's firmware file is missing: $file" >&2
    exit 2
  fi
done
vars="$out/edk2-vars.fd"
cp "$vars_template" "$vars"

if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
  accel=(-accel kvm -cpu host)
  seconds=120
  echo "uefi-boot: KVM, $cpus CPUs, $seconds s" >&2
else
  accel=(-accel tcg -cpu max)
  seconds=900
  echo "uefi-boot: TCG (no usable /dev/kvm), $cpus CPUs, $seconds s" >&2
fi
echo "uefi-boot: boot disk $disk, read as $driver" >&2

console="$out/console.log"
line="moruna: services started"
rm -f "$console"
qemu_pid=""
stop() {
  if [ -n "$qemu_pid" ] && kill -0 "$qemu_pid" 2>/dev/null; then
    kill -TERM "$qemu_pid" 2>/dev/null || true
    for _ in $(seq 50); do
      kill -0 "$qemu_pid" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL "$qemu_pid" 2>/dev/null || true
  fi
  if [ -n "$qemu_pid" ]; then
    wait "$qemu_pid" 2>/dev/null || true
  fi
}
trap stop EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

start="$EPOCHREALTIME"
qemu-system-x86_64 -machine q35 -smp "$cpus" -m 2G -nodefaults -no-reboot -display none \
  "${accel[@]}" \
  -drive "if=pflash,format=raw,readonly=on,file=$code" \
  -drive "if=pflash,format=raw,file=$vars" \
  -drive "if=none,id=boot,format=$driver,readonly=on,file=$disk" \
  -device virtio-blk-pci,drive=boot,disable-legacy=on \
  -netdev user,id=net0 -device virtio-net-pci,netdev=net0,disable-legacy=on \
  -serial "file:$console" &
qemu_pid=$!

elapsed() {
  awk -v a="$start" -v b="$EPOCHREALTIME" 'BEGIN { printf "%.1f", b - a }'
}
deadline=$((${start%.*} + seconds))
while :; do
  if [ -f "$console" ] && grep -qF -- "$line" "$console"; then
    echo "uefi-boot: \"$line\" $(elapsed) s after QEMU's start" >&2
    exit 0
  fi
  if ! kill -0 "$qemu_pid" 2>/dev/null; then
    status=0
    wait "$qemu_pid" || status=$?
    qemu_pid=""
    echo "uefi-boot: QEMU ended with status $status after $(elapsed) s, before \"$line\"" >&2
    exit 1
  fi
  if [ "${EPOCHREALTIME%.*}" -ge "$deadline" ]; then
    echo "uefi-boot: no \"$line\" within $seconds s" >&2
    exit 124
  fi
  sleep 0.2
done
