#!/usr/bin/env bash
# Builds the gate's native tasks for the core: programs on moruna-function, so Rust programs with
# std, built for the target std supports on Moruna (architecture/moruna-os-system.md 5A0, item 5.0)
# with the kernel's pinned nightly and std built from Moruna's patched copy of Rust's library/,
# static. Writes gate-JOB for each of the seven jobs.
#
#   images/gate/tasks/build.sh [CARGO_ARGS...]
#
# The binaries land in target/x86_64-moruna/release (CARGO_TARGET_DIR to build elsewhere).
#
# It needs item 5.0's target (kernel/targets/x86_64-moruna.json), its link script
# (kernel/targets/x86_64-moruna.ld) and its std (kernel/vendor/rust-std/library, Rust's library/
# with Moruna's platform layer); without them it stops and says so. The flags are item 5.0's
# kernel/tools/build-std.sh's, which this script follows: build-std takes the library/
# directory, and a program with std links by x86_64-moruna.ld, kernel/sys/user.ld's layout with
# the ELF TLS template std's runtime makes each thread's thread-locals from (rust-lld refuses
# std's objects without one). getrandom, which the tasks reach through ahash, knows no
# `target_os = "moruna"`: its backend is the custom one, kernel/getrandom's, as build-std.sh
# selects.
#
# The CPU features are the gate's one setting, images/gate/.cargo/config.toml (AVX, AVX2 and
# FMA past the user target's SSE2): the flags here go in the target's own rustflags, which cargo
# joins with that file's, so the tasks build exactly as gate-baseline and py066 do.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
kernel="$(cd "$here/../../../kernel" && pwd)"
toolchain=nightly-2026-07-21
target="$kernel/targets/x86_64-moruna.json"
library="$kernel/vendor/rust-std/library"
link="$kernel/targets/x86_64-moruna.ld"
for need in "$target" "$link" "$library/std/src/sys/pal/moruna/mod.rs"; do
  if [ ! -e "$need" ]; then
    echo "build.sh: $need is missing: the tasks are Rust programs with std, built for item 5.0's target" >&2
    exit 1
  fi
done
flags="--cfg polyval_force_soft -C relocation-model=static -C relro-level=off -C force-unwind-tables=yes -C no-redzone=y"
flags="$flags --cfg getrandom_backend=\"custom\""
unset CARGO_ENCODED_RUSTFLAGS RUSTFLAGS
cd "$here"
CARGO_TARGET_X86_64_MORUNA_RUSTFLAGS="$flags -Zunstable-options -C link-arg=-T$link -C link-arg=-static" \
  __CARGO_TESTS_ONLY_SRC_ROOT="$library" \
  rustup run "$toolchain" cargo build --release --target "$target" \
  -Zunstable-options -Zjson-target-spec \
  -Zbuild-std=std,panic_abort -Zbuild-std-features=compiler-builtins-mem "$@"
bin="${CARGO_TARGET_DIR:-$here/target}/x86_64-moruna/release"
for t in enrich audit occupancy score fw_minhash ov_shape ov_attrs; do
  test -f "$bin/gate-$t" || { echo "build.sh: gate-$t was not built" >&2; exit 1; }
done
echo "$bin"
