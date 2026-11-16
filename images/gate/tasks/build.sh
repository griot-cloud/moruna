#!/usr/bin/env bash
# Builds the gate's native tasks for the Moruna kernel's user target, as kernel/tools/build.sh
# builds the kernel's own programs: the kernel's pinned nightly, core and alloc from source, the
# user linker script, static. Writes gate-JOB for each of the seven jobs.
#
#   images/gate/tasks/build.sh [OUT]
#
# The binaries land in target/x86_64-moruna-user/release (CARGO_TARGET_DIR to build elsewhere).
#
# The CPU features are the gate's one setting, images/gate/.cargo/config.toml (AVX, AVX2 and
# FMA past the user target's SSE2): the flags here go in the target's own rustflags, which cargo
# joins with that file's, so the tasks build exactly as gate-baseline and py066 do.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
kernel="$(cd "$here/../../../kernel" && pwd)"
toolchain=nightly-2026-07-21
target="$kernel/targets/x86_64-moruna-user.json"
flags="--cfg polyval_force_soft -C relocation-model=static -C relro-level=off -C force-unwind-tables=yes -C no-redzone=y"
unset CARGO_ENCODED_RUSTFLAGS RUSTFLAGS
cd "$here"
CARGO_TARGET_X86_64_MORUNA_USER_RUSTFLAGS="$flags -Zunstable-options -C link-arg=-T$kernel/sys/user.ld -C link-arg=-static" \
  rustup run "$toolchain" cargo build --release --target "$target" \
  -Zunstable-options -Zjson-target-spec \
  -Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem "$@"
bin="${CARGO_TARGET_DIR:-$here/target}/x86_64-moruna-user/release"
for t in enrich audit occupancy score fw_minhash ov_shape ov_attrs; do
  test -f "$bin/gate-$t" || { echo "build.sh: gate-$t was not built" >&2; exit 1; }
done
echo "$bin"
