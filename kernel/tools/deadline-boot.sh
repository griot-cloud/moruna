#!/usr/bin/env bash
# Reuse built ELF/image/boot/check tools; no compiler, source rewrite or disk copy.
# Usage: deadline-boot.sh BUILT_OUT FRESH_OUT [DEADLINE_ELF] [IMAGE_WRITER]
set -euo pipefail
kernel="$(cd "$(dirname "$0")/.." && pwd)"
built="$(cd "$1" && pwd)"
out="$2"
deadline="${3:-$built/deadline}"
writer="${4:-$built/moruna-image-host}"
# Canonical build.sh retains both host tools and the existing compiled timer ELF.
test -f "$deadline"
test -x "$writer"
test -f "$built/moruna-microkernel.qemu.elf" || test -f "$built/moruna-microkernel.elf"
test -x "$built/moruna-vhost-vsock"
test ! -e "$out"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
mkdir "$out/bins"
deadline="$(cd "$(dirname "$deadline")" && pwd)/$(basename "$deadline")"
ln -s "$deadline" "$out/bins/deadline"
"$writer" "$kernel/initramfs/deadline.image" "$out/bins" "$out"
if [ -f "$built/moruna-microkernel.qemu.elf" ]; then
  ln -s "$built/moruna-microkernel.qemu.elf" "$out/moruna-microkernel.qemu.elf"
else
  # Same private EM_386 Multiboot copy as build.sh; the original ELF is untouched.
  python3 - "$built/moruna-microkernel.elf" "$out/moruna-microkernel.qemu.elf" <<'PYELF'
import pathlib, sys
source, target = map(pathlib.Path, sys.argv[1:])
if not 20 <= source.stat().st_size <= 64 * 1024 * 1024:
    raise SystemExit("kernel ELF exceeds bounded fixture copy")
data = bytearray(source.read_bytes())
if data[:4] != b"\x7fELF" or data[18:20] != b"\x3e\x00":
    raise SystemExit("not an x86_64 kernel ELF")
data[18:20] = b"\x03\x00"
with target.open("xb") as stream:
    stream.write(data)
PYELF
fi
ln -s "$built/moruna-vhost-vsock" "$out/moruna-vhost-vsock"
fail=0
for cpus in 1 2 4; do
  set +e
  "$kernel/tools/boot.sh" "$out" 60 "$cpus" | tee "$out/serial-$cpus.log"
  status="${PIPESTATUS[0]}"
  set -e
  printf '%s\n' "$status" > "$out/status-$cpus"
  "$kernel/tools/check-boot.sh" "$out/serial-$cpus.log" "$status" "$out/expect" "$cpus" deadline || fail=1
done
exit "$fail"
