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
# The machine is what the boot description says (moruna-image's OUT/machine): each
# `machine` line's QEMU arguments, split at spaces, with {out}, {cpus}, {memory} replaced by
# OUT, CPUS and this script's single fixed CI RAM setting. That is where a test's devices are added (SDD 25, e.4): the consoles,
# the disks, the edu device, the NVMe controllers and the network cards of the CI
# boot. Before the boot every `disk` line's image (OUT/disks) is written afresh as
# OUT/NAME-CPUS.img with pattern A (disk-image.py), and console1's input line. Two
# devices this script adds itself, because they depend on what the build has: the Linux
# personality's image, one read-only virtio-blk disk a layer from 00:0a on
# (OUT/linux/stack-N.img, lowest first; block3 onward, SDD 28 k).
#
# The vsock (00:06, guest cid 3) is the description's `vhost-user-vsock-pci` over the
# socket OUT/vhost-vsock-CPUS.sock, whose back end this script starts first: build.sh's
# OUT/moruna-vhost-vsock (moruna-vmm's vsock device, SDD 30 f.11), which needs no host
# kernel module, its log OUT/vhost-vsock-CPUS.log. Its host side is the Unix socket
# OUT/vsock-CPUS.uds (`CONNECT <port>` reaches a guest port; a guest's connection to host
# port P reaches OUT/vsock-CPUS.uds_P). The host's peers use it: vsock-peer.py (DR-T13,
# OUT/vsock-peer-CPUS.log). The mandatory production-node successors run the canonical
# moruna-kd3-peer separately for every SDD 30 case/profile. The guest's
# memory is the description's shared memfd, which the back end maps; its size is the -m
# below.
set -euo pipefail

kernel="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$kernel/target/boot}"
seconds="${2:-300}"
cpus="${3:-1}"

# 3 GiB: what the boot's tests use at once, and the engine's account, which since KB5 is
# held whole out of the machine's for each run (1 GiB for it and 256 MiB for its scratch
# directory, initramfs/engine.conf; SDD 20 f.18), where before it was a figure the engine
# was told.
memory=3G

if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
  accel=(-accel kvm -cpu host -machine q35,kernel-irqchip=split)
  echo "boot: KVM" >&2
else
  accel=(-accel tcg -cpu max -machine q35)
  echo "boot: TCG (no usable /dev/kvm)" >&2
fi

console0="$out/console0-$cpus.log"
: > "$console0"
printf 'KB3 console input: hello from the host\n' > "$out/console1-input.txt"
while read -r name sectors _; do
  python3 "$kernel/tools/disk-image.py" write "$out/$name-$cpus.img" "$sectors"
done < "$out/disks"
devices=()
while IFS= read -r line; do
  line="${line//\{out\}/$out}"
  line="${line//\{cpus\}/$cpus}"
  line="${line//\{memory\}/$memory}"
  read -r -a words <<< "$line"
  devices+=("${words[@]}")
done < "$out/machine"
n=1
while [ -r "$out/linux/stack-$n.img" ]; do
  devices+=(
    -drive "file=$out/linux/stack-$n.img,format=raw,if=none,id=stack$n,readonly=on"
    -device "virtio-blk-pci,drive=stack$n,addr=$(printf '0x%x' $((9 + n))),disable-legacy=on"
  )
  n=$((n + 1))
done
backend="$out/moruna-vhost-vsock"
socket="$out/vhost-vsock-$cpus.sock"
uds="$out/vsock-$cpus.uds"
if [ ! -x "$backend" ]; then
  echo "boot.sh: no vsock back end at $backend (build.sh builds it)" >&2
  exit 2
fi
# Install cleanup before any helper starts. Only PIDs started by this invocation are
# signalled; logs remain available even if the backend never listens or the boot is stopped.
helpers=""
qemu=""
# Bash may already have reaped a completed helper while QEMU ran. Numeric PID history
# alone is not ownership: signal only a running or stopped job still tracked by this shell.
owned_job() {
  for current in $(jobs -pr) $(jobs -ps); do
    [ "$current" = "$1" ] && return 0
  done
  return 1
}
cleanup() {
  status=$?
  trap - EXIT HUP INT TERM
  set +e
  owned="$helpers${qemu:+ $qemu}"
  for pid in $owned; do
    # An owned stopped job must resume to receive TERM; no unrelated PID is resumed.
    owned_job "$pid" && kill -CONT "$pid" 2>/dev/null
    owned_job "$pid" && kill -TERM "$pid" 2>/dev/null
  done
  for _ in $(seq 50); do
    live=false
    for pid in $owned; do owned_job "$pid" && live=true; done
    "$live" || break
    sleep 0.1
  done
  for pid in $owned; do owned_job "$pid" && kill -KILL "$pid" 2>/dev/null; done
  for pid in $owned; do wait "$pid" 2>/dev/null; done
  rm -f "$socket" "$uds" "$uds"_*
  exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
rm -f "$socket" "$uds" "$uds"_*
"$backend" --socket "$socket" --cid 3 --uds "$uds" 2> "$out/vhost-vsock-$cpus.log" &
helpers=$!
for _ in $(seq 100); do
  [ -S "$socket" ] && break
  sleep 0.1
done
if [ ! -S "$socket" ]; then
  echo "boot.sh: the vsock back end did not listen at $socket; its log:" >&2
  cat "$out/vhost-vsock-$cpus.log" >&2
  exit 2
fi
python3 "$kernel/tools/vsock-peer.py" "$out/vsock-peer-$cpus.log" "$uds" &
helpers="$helpers $!"
# The five engine jobs run in REQUIRED production-node successors via
# node-fixtures.sh (all15 case/profile combinations), never bare engine.conf.
echo "boot: vsock through $backend (guest cid 3), its host side $uds" >&2

set +e
timeout --kill-after=5 "$seconds" qemu-system-x86_64 \
  "${accel[@]}" \
  -m "$memory" -smp "$cpus" \
  -nographic -display none -monitor none -serial stdio -no-reboot \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
  "${devices[@]}" \
  -kernel "$out/moruna-microkernel.qemu.elf" \
  -initrd "$out/initramfs.cpio" &
qemu=$!
wait "$qemu"
status=$?
qemu=""
echo "boot: qemu exited with status $status" >&2
exit "$status"
