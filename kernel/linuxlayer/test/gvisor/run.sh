#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Runs the gVisor tests that apply, BATCH binaries to a boot (`boot/boot.sh gvisor`), so a
# test that ends the personality loses only its batch, and writes OUT/results: a line for
# each test binary, `gvisor: ok: NAME` or `gvisor: FAIL: NAME ...`, then a count.
#
#   kernel/linuxlayer/test/gvisor/run.sh BINS OUT [BATCH] [TIMEOUT_SECONDS]
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
bins="$1"
out="$2"
batch="${3:-8}"
seconds="${4:-3600}"
mkdir -p "$out"
mapfile -t names < <(grep -v '^#' "$here/applies" | sed 's/#.*//; s/[[:space:]]//g' | grep .)
: > "$out/results"
for ((i = 0; i < ${#names[@]}; i += batch)); do
  set=("${names[@]:i:batch}")
  dir="$out/batch-$((i / batch))"
  GVISOR_BINS="$bins" GVISOR_TESTS="${set[*]}" BOOT_MEMORY=4G \
    "$here/../../boot/boot.sh" "$dir" "$seconds" gvisor > "$out/batch-$((i / batch)).log" 2>&1
  for name in "${set[@]}"; do
    line="$(grep -a -o "gvisor: \(ok\|FAIL\): $name\b.*" "$dir/serial.log" 2>/dev/null | head -1)"
    echo "${line:-gvisor: FAIL: $name had no verdict: the boot ended first}" >> "$out/results"
  done
done
passed="$(grep -c "^gvisor: ok:" "$out/results")"
echo "gvisor: $passed of ${#names[@]} test binaries passed" >> "$out/results"
cat "$out/results"
