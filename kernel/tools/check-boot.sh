#!/usr/bin/env bash
# Judges a boot's serial log (SDD 20, k): every KB1 line present, nothing failed, no
# kernel panic, and QEMU's status the kernel's success. Prints each verdict.
#
#   kernel/tools/check-boot.sh LOG QEMU_STATUS
set -euo pipefail

log="$1"
status="$2"
fail=0

expect() {
  local id="$1" line="$2"
  if grep -qF -- "$line" "$log"; then
    echo "pass $id: $line"
  else
    echo "MISSING $id: $line"
    fail=1
  fi
}

expect KK-T6  "KB1 ping-pong: ok: 10000 round trips over one channel"
expect KK-T7  "KB1 handle-transfer: ok: 2 pages arrived through a read-only handle with the pattern intact; mapping them writable was refused with access denied"
expect KK-T8  "KB1 isolation: ok: "
expect KK-T8  "kk: process isolation exited with code 0"
expect KK-T9  "KB1 wait-deadline: ok: the timer woke the waiting thread"
expect KK-T10 "KB1 fault: about to read address 0"
expect KK-T10 "kk: process fault faulted: page fault at 0x0 on read"
expect KK-T11 "KB1 spawn: ok: child runs, started by the supervisor with 1 handle"
expect KK-T11 "KB1 supervisor: ok: "
expect KK-T12 "kk: moruna microkernel, abi 1.0: started 9 processes from the manifest"
for p in supervisor child ping pong xfer-a xfer-b iso-holder deadline; do
  expect KK-T12 "kk: process $p exited with code 0"
done
expect KK-T12 "kk: every process has ended; the supervisor succeeded; powering off"

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
