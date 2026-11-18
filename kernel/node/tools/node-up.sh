#!/usr/bin/env bash
# Brings the node's disk image up in QEMU for the booted checks that talk to it (work items 1.3,
# 1.4 and 2.2b), and leaves it running. The machine is uefi-boot.sh's (system design 1A: q35,
# 2 GiB, UEFI firmware, the boot disk read-only on virtio-blk, one virtio network card, COM1 to
# OUT/console.log), with its user network set for those checks:
#
# - `net=169.254.169.0/24,host=169.254.169.254`: the node's connections to 169.254.169.254
#   port 80 reach this machine's own 127.0.0.1 port 80, where fake-instance-data.py plays a
#   cloud's instance data (item 1.4). The node's lease is then 169.254.169.15/24, gateway
#   169.254.169.254.
# - `hostfwd=tcp::8443-:443` and `hostfwd=tcp::8444-:444`: this machine's 8443 and 8444 reach
#   the node's 443 and 444.
# - `filter-dump` writes the card's every frame to OUT/net.pcap, which tells whether a
#   connection reached the node and how it answered.
#
#   kernel/node/tools/node-up.sh OUT CPUS [FORMAT]
#   kernel/node/tools/node-down.sh OUT
#
# FORMAT is the boot disk's, as uefi-boot.sh takes it: raw (the default) or qcow2. With
# NODE_UP_CARD=none the machine has no network card (the check that the node says it has no
# lease, and powers off).
#
# It returns once `moruna: network` is on the console (exit 0), within 120 s under KVM or 300 s
# under the emulator, and prints how long after QEMU's start it came, QEMU left running with its
# process ID in OUT/qemu.pid. When a `moruna: fatal` line comes first it prints it, waits up to
# 30 s for the machine to power itself off, says whether it did, and exits 1. It exits 1 too
# when QEMU ends first, and 124 when the time runs out. On each of those QEMU is stopped.
set -euo pipefail

if [ "$#" -ne 2 ] && [ "$#" -ne 3 ]; then
  echo "usage: node-up.sh OUT CPUS [FORMAT]" >&2
  exit 2
fi
out="$(cd "$1" && pwd)"
cpus="$2"
case "$cpus" in
  '' | *[!0-9]* | 0*) echo "node-up.sh: CPUS must be a positive number: $cpus" >&2; exit 2 ;;
esac
format="${3:-raw}"
case "$format" in
  raw | qcow2) ;;
  *) echo "node-up.sh: FORMAT must be raw or qcow2: $format" >&2; exit 2 ;;
esac
card="${NODE_UP_CARD:-virtio}"
case "$card" in
  virtio | none) ;;
  *) echo "node-up.sh: NODE_UP_CARD must be virtio or none: $card" >&2; exit 2 ;;
esac
disk="$out/moruna-node.$format"
if [ ! -f "$disk" ]; then
  echo "node-up.sh: no disk image at $disk" >&2
  exit 2
fi
pidfile="$out/qemu.pid"
if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
  echo "node-up.sh: a node is already up from $out (process $(cat "$pidfile")); node-down.sh $out stops it" >&2
  exit 2
fi
qemu="$(command -v qemu-system-x86_64)" || { echo "node-up.sh: qemu-system-x86_64 is not on PATH" >&2; exit 2; }
firmware="$(cd "$(dirname "$qemu")/../share/qemu" && pwd)"
code="$firmware/edk2-x86_64-code.fd"
vars_template="$firmware/edk2-i386-vars.fd"
for file in "$code" "$vars_template"; do
  if [ ! -r "$file" ]; then
    echo "node-up.sh: QEMU's firmware file is missing: $file" >&2
    exit 2
  fi
done
vars="$out/edk2-vars.fd"
cp "$vars_template" "$vars"

if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
  accel=(-accel kvm -cpu host)
  seconds=120
else
  accel=(-accel tcg -cpu max)
  seconds=300
fi
network=()
if [ "$card" = virtio ]; then
  network=(
    -netdev "user,id=net0,net=169.254.169.0/24,host=169.254.169.254,hostfwd=tcp::8443-:443,hostfwd=tcp::8444-:444"
    -object "filter-dump,id=dump0,netdev=net0,file=$out/net.pcap"
    -device virtio-net-pci,netdev=net0,disable-legacy=on
  )
fi
echo "node-up: ${accel[1]}, $cpus CPUs, $format disk, card $card, $seconds s" >&2

console="$out/console.log"
rm -f "$console" "$out/net.pcap" "$pidfile"
start="$EPOCHREALTIME"
# In the background, its own process: it stays up after this script returns.
nohup qemu-system-x86_64 -machine q35 -smp "$cpus" -m 2G -rtc base=utc -nodefaults -no-reboot -display none \
  "${accel[@]}" \
  -drive "if=pflash,format=raw,readonly=on,file=$code" \
  -drive "if=pflash,format=raw,file=$vars" \
  -drive "if=none,id=boot,format=$format,readonly=on,file=$disk" \
  -device virtio-blk-pci,drive=boot,disable-legacy=on \
  "${network[@]}" \
  -serial "file:$console" > "$out/qemu.log" 2>&1 < /dev/null &
qemu_pid=$!
echo "$qemu_pid" > "$pidfile"

elapsed() {
  awk -v a="$start" -v b="$EPOCHREALTIME" 'BEGIN { printf "%.1f", b - a }'
}
running() {
  kill -0 "$qemu_pid" 2>/dev/null
}
stop() {
  "$(dirname "$0")/node-down.sh" "$out" >&2 || true
}
deadline=$((${start%.*} + seconds))
while :; do
  if [ -f "$console" ]; then
    if line="$(grep -m 1 -F -- "moruna: network" "$console")"; then
      echo "node-up: \"$line\" $(elapsed) s after QEMU's start; QEMU is process $qemu_pid" >&2
      exit 0
    fi
    if line="$(grep -m 1 -F -- "moruna: fatal" "$console")"; then
      echo "node-up: \"$line\" $(elapsed) s after QEMU's start" >&2
      for _ in $(seq 150); do
        running || break
        sleep 0.2
      done
      if running; then
        echo "node-up: the machine did not power off within 30 s of the fatal line" >&2
        stop
      else
        echo "node-up: the machine powered off $(elapsed) s after QEMU's start" >&2
        rm -f "$pidfile"
      fi
      exit 1
    fi
  fi
  if ! running; then
    status=0
    wait "$qemu_pid" || status=$?
    rm -f "$pidfile"
    echo "node-up: QEMU ended with status $status after $(elapsed) s, before \"moruna: network\"; see $out/qemu.log" >&2
    exit 1
  fi
  if [ "${EPOCHREALTIME%.*}" -ge "$deadline" ]; then
    echo "node-up: no \"moruna: network\" within $seconds s" >&2
    stop
    exit 124
  fi
  sleep 0.2
done
