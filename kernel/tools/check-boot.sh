#!/usr/bin/env bash
# Judges a boot's serial log (SDD 20 k, SDD 23 k): every line the image's description
# expects (moruna-image's OUT/expect) present, every CPU online and every CPU having
# run a process thread, nothing failed, no panic, and QEMU's status the kernel's
# success. Prints each verdict.
#
#   kernel/tools/check-boot.sh LOG QEMU_STATUS EXPECT CPUS
set -euo pipefail

log="$1"
status="$2"
expect_file="$3"
cpus="$4"
fail=0

expect() {
  local line="$1"
  if grep -qF -- "$line" "$log"; then
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
  if grep -qF -- "$bad" "$log"; then
    echo "FOUND \"$bad\":"
    grep -F -- "$bad" "$log"
    fail=1
  fi
done
if grep -qi -- "panic" "$log"; then
  echo "FOUND a panic:"
  grep -i -- "panic" "$log"
  fail=1
fi

if [ "$status" != 33 ]; then
  echo "qemu exited with $status, not 33 (the kernel's success)"
  fail=1
fi
exit "$fail"
