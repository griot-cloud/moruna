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
# For the volumes' checks (item 3.3): NODE_UP_MEMORY sets the machine's memory (QEMU's -m, 2G
# by default; at most 6G, this Mac's rule for a VM), NODE_UP_BOOT_SERIAL gives the boot disk a
# virtio serial, and NODE_UP_VOLUMES attaches each of its space-separated FILE:SERIAL[:ro] as a
# raw virtio-blk disk with that serial (modern virtio, so the node's virtio-blk driver serves
# it), read only with `:ro`.
#
# It returns once `moruna: network` is on the console (exit 0), within 120 s under KVM or 300 s
# under the emulator, and prints how long after QEMU's start it came, QEMU left running with its
# process ID in OUT/qemu.pid. When a `moruna: fatal` line comes first it prints it, waits up to
# 90 s for the machine to power itself off (the node's 60 s hold after a fatal line, system
# design 1B, and 30 s), says whether it did, and exits 1. It exits 1 too
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
memory="${NODE_UP_MEMORY:-2G}"
case "$memory" in
  [1-6]G) ;;
  *) echo "node-up.sh: NODE_UP_MEMORY must be 1G to 6G: $memory" >&2; exit 2 ;;
esac
serial_ok() {
  case "$1" in
    '' | *[!A-Za-z0-9._-]*) return 1 ;;
  esac
  [ "${#1}" -le 20 ]
}
boot_serial=""
if [ -n "${NODE_UP_BOOT_SERIAL:-}" ]; then
  serial_ok "$NODE_UP_BOOT_SERIAL" || {
    echo "node-up.sh: NODE_UP_BOOT_SERIAL must be 1 to 20 of [A-Za-z0-9._-]: $NODE_UP_BOOT_SERIAL" >&2
    exit 2
  }
  boot_serial=",serial=$NODE_UP_BOOT_SERIAL"
fi
volumes=()
n=0
for volume in ${NODE_UP_VOLUMES:-}; do
  IFS=: read -r file vserial access extra <<< "$volume"
  if [ -n "${extra:-}" ] || [ ! -f "$file" ] || ! serial_ok "$vserial" \
     || { [ -n "${access:-}" ] && [ "$access" != ro ]; }; then
    echo "node-up.sh: NODE_UP_VOLUMES takes FILE:SERIAL[:ro], an existing file and a serial of 1 to 20 of [A-Za-z0-9._-]: $volume" >&2
    exit 2
  fi
  readonly_flag=""
  [ "${access:-}" = ro ] && readonly_flag=",readonly=on"
  volumes+=(-drive "if=none,id=volume$n,format=raw$readonly_flag,file=$file"
            -device "virtio-blk-pci,drive=volume$n,serial=$vserial,disable-legacy=on")
  n=$((n + 1))
done
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
echo "node-up: ${accel[1]}, $cpus CPUs, $memory, $format disk, card $card, $n volumes, $seconds s" >&2

console="$out/console.log"
rm -f "$console" "$out/net.pcap" "$pidfile"
start="$EPOCHREALTIME"
# In the background, its own process: it stays up after this script returns.
nohup qemu-system-x86_64 -machine q35 -smp "$cpus" -m "$memory" -rtc base=utc -nodefaults -no-reboot -display none \
  "${accel[@]}" \
  -drive "if=pflash,format=raw,readonly=on,file=$code" \
  -drive "if=pflash,format=raw,file=$vars" \
  -drive "if=none,id=boot,format=$format,readonly=on,file=$disk" \
  -device "virtio-blk-pci,drive=boot,disable-legacy=on$boot_serial" \
  ${volumes[@]+"${volumes[@]}"} \
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
      for _ in $(seq 450); do
        running || break
        sleep 0.2
      done
      if running; then
        echo "node-up: the machine did not power off within 90 s of the fatal line" >&2
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
