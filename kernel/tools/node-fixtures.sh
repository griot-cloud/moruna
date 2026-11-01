#!/usr/bin/env bash
# Mandatory successor to bare engine.conf:15 REAL production-node fixture boots.
# Caller owns execution reservation. No builds/downloads/container/defaultmachine.
# Usage: node-fixtures.sh BUILD_OUT NEW_EVIDENCE_ROOT QEMU TLS_ROOTS [SECONDS] [--sdk-proof EXECUTABLE]
set -euo pipefail
kernel="$(cd "$(dirname "$0")/.." && pwd)"
build="${1:?explicit build output required}"
evidence="${2:?new explicit absolute evidence root required}"
qemu="${3:?explicit QEMU binary required}"
roots="${4:?actual TLS roots file required}"
shift 4
seconds=180
if [[ $# -gt 0 && "$1" != --sdk-proof ]]; then seconds="$1"; shift; fi
sdk=()
if [[ $# -gt 0 ]]; then
  [[ $# -eq 2 && "$1" == --sdk-proof && "$2" = /* && -f "$2" && -x "$2" ]] || { echo 'explicit --sdk-proof ABSOLUTE_EXECUTABLE required' >&2;exit 2; }
  sdk=(--sdk-proof "$2")
fi
case "$build:$evidence:$qemu:$roots" in *$'\n'*|*$'\r'*) echo 'unsafe input path' >&2;exit 2;; esac
for path in "$build" "$evidence" "$qemu" "$roots"; do
  [[ "$path" = /* ]] || { echo 'every input/output path must be absolute' >&2;exit 2; }
done
[[ ! -e "$evidence" && ! -L "$evidence" ]] || { echo 'evidence root must be NEW and owned by this invocation' >&2;exit 2; }
mkdir -m700 "$evidence"
for cpus in 1 2 4; do
  mkdir -m700 "$evidence/cpu$cpus"
  for case in 1 2 3 4 5; do
    python3 "$kernel/node/tools/proof.py" \
      "${sdk[@]}" --cpus "$cpus" --case "$case" --out "$evidence/cpu$cpus/case$case" --seconds "$seconds" \
      --qemu "$qemu" --backend "$build/moruna-vhost-vsock" --peer "$build/moruna-kd3-peer" \
      --fixture-emitter "$build/moruna-node-ci-assets" --validator "$build/moruna-node-image" \
      --image-builder "$build/moruna-image-host" --provision "$build/moruna-storage-provision" \
      --kernel-elf "$build/moruna-microkernel.elf" --user-bins "$build/node-user-bins" \
      --image-record "$build/node-linux/image.record" --linux-layers "$build/node-linux/stack.layers" \
      --linux-digest "$build/node-linux/image.digest" --tls-roots "$roots" \
      --layer-fixture-1 "$build/layers/layer-1.img" --layer-fixture-2 "$build/layers/layer-2.img" \
      --engine-layer-dir "$build/node-linux"
  done
done
python3 "$kernel/tools/check-node-fixtures.py" "$evidence"
