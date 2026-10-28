#!/usr/bin/env bash
# Judges a boot's serial log (SDD 20 k, SDD 23 k, SDD 25 k): every line the image's
# description expects (moruna-image's OUT/expect) present in the serial log or in
# console0's file, every CPU online and every CPU having run a process thread,
# nothing failed, no panic, the disk image holding what the block test wrote, the
# vsock test's lines (or, when the boot had no vsock, the line saying so), and QEMU's
# status the kernel's success. Prints each verdict.
#
#   kernel/tools/check-boot.sh LOG QEMU_STATUS EXPECT CPUS
#
# The boot's other files are read from EXPECT's directory (boot.sh's OUT).
set -euo pipefail

log="$1"
status="$2"
expect_file="$3"
cpus="$4"
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
for bad in "FAIL:" "kk: boot refused"; do
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

# DR-T9: what the block test wrote and flushed is on the host's disk image.
python3 "$tools/disk-image.py" check "$out/disk-$cpus.img" || fail=1

# DR-T13: with a vsock, both exchanges, on each side; without one, the line saying so.
vsock="$(cat "$out/vsock-$cpus")"
if [ "$vsock" = attached ]; then
  expect "KB3 vsock: ok:"
  expect "KB3 conformance moruna.driver.vsock: ok:"
  expect "KB3 vsock0: virtio-vsock up as cid 3"
  expect "kk: process vsock0 exited with code 0 ("
  peer="$out/vsock-peer-$cpus.log"
  expect "vsock-peer: ok: the guest (cid 3" "$peer"
  expect "vsock-peer: ok: connected to the guest's port 5001 and it answered 'hello back from the guest'" "$peer"
else
  echo "note: the boot had no vsock ($vsock)"
  expect "KB3 vsock: no device:"
fi

if [ "$status" != 33 ]; then
  echo "qemu exited with $status, not 33 (the kernel's success)"
  fail=1
fi
exit "$fail"
