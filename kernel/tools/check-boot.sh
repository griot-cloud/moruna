#!/usr/bin/env bash
# Judges a boot's serial log (SDD 20 k, SDD 23 k, SDD 25 k): every line the image's
# description expects (moruna-image's OUT/expect) present in the serial log or in
# console0's file, every CPU online and every CPU having run a process thread,
# nothing failed, no panic, every disk the description names holding the pattern it
# says (what the block and NVMe tests wrote, or what the boot wrote, untouched), the
# vsock test's and the engine's runs' lines, and QEMU's status the kernel's success.
# Prints each verdict.
#
#   kernel/tools/check-boot.sh LOG QEMU_STATUS EXPECT CPUS [generic|deadline]
#
# The boot's other files are read from EXPECT's directory (boot.sh's OUT).
set -euo pipefail

log="$1"
status="$2"
expect_file="$3"
cpus="$4"
kind="${5:-generic}"
case "$kind" in generic|deadline) ;; *) echo "unknown boot check kind: $kind" >&2; exit 2 ;; esac
out="$(dirname "$expect_file")"
tools="$(cd "$(dirname "$0")" && pwd)"
fail=0

# The serial line and console0's file, together.
seen="$(mktemp)"
trap 'rm -f "$seen"' EXIT
cat "$log" "$out/console0-$cpus.log" > "$seen"

expect() {
  local line="$1" in="${2:-$seen}"
  if grep -qF -- "$line" "$in"; then
    echo "pass: $line"
  else
    echo "MISSING: $line"
    fail=1
  fi
}

while IFS= read -r line; do
  [ -n "$line" ] && expect "$line"
done < "$expect_file"
expect "kk: $cpus CPUs online"
expect "kk: process threads ran on $cpus of $cpus CPUs"

# A test's failure line and a refused boot, exactly; a panic in any case (OSTD prints
# "Non-resettable panic!", a program's handler "panicked").
for bad in "FAIL:" "kk: boot refused" "kk: accounting broken"; do
  if grep -qF -- "$bad" "$seen"; then
    echo "FOUND \"$bad\":"
    grep -F -- "$bad" "$seen"
    fail=1
  fi
done
if grep -qi -- "panic" "$seen"; then
  echo "FOUND a panic:"
  grep -i -- "panic" "$seen"
  fail=1
fi

if [ "$kind" = generic ]; then
# DR-T9, DR-T18: what the block and NVMe tests wrote and flushed is on the host's disk
# images, and a disk no test wrote is as the boot wrote it.
while read -r name sectors after; do
  python3 "$tools/disk-image.py" check "$out/$name-$cpus.img" "$sectors" "$after" || fail=1
done < "$out/disks"

# DR-T13: both exchanges, on each side, through the vhost-user back end; SDD 30 k: the
# engine's five runs, each judged by the embedder on the host's side.
expect "KB3 vsock: ok:"
expect "KB3 conformance moruna.driver.vsock: ok:"
expect "KB3 vsock0: virtio-vsock up as cid 3"
expect "kk: process vsock0 exited with code 0 ("
peer="$out/vsock-peer-$cpus.log"
expect "vsock-peer: ok: the guest connected to port 5000" "$peer"
expect "vsock-peer: ok: connected to the guest's port 5001 and it answered 'hello back from the guest'" "$peer"
# Source-derived generic baseline: linux/test's seven check() runs, granted(),
# hog(), and its post-hog hello; plus plane/test shimless()'s two run_plane requests. The removed bare
# engine contributed5engine processes+6image tasks; all5strict assertions now
# remain mandatory on each CPU profile in check-node-fixtures.py (15boots).
# linux-test starts ten programs; the plane regression additionally starts
# arrow_echo and hello through the same actual personality.
expect "linux: served: 12 programs run"
if grep -qF -- "FAIL:" "$peer"; then
  echo "FOUND FAILURE in actual vsock peer log:"
  grep -F -- "FAIL:" "$peer"
  fail=1
fi

fi

if [ "$status" != 33 ]; then
  echo "qemu exited with $status, not 33 (the kernel's success)"
  fail=1
fi
exit "$fail"
