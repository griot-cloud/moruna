#!/usr/bin/env bash
# Builds a Rust program with std for Moruna (item 5.0, system design 5A0): a static native
# program for kernel/targets/x86_64-moruna.json, its std built from source with Moruna's
# platform layer, kernel/vendor/rust-std/library.
#
#   kernel/tools/build-std.sh MANIFEST [CARGO_BUILD_ARGUMENTS...]
#
# MANIFEST is the program's Cargo.toml, for example kernel/examples/std-arrow/Cargo.toml; the
# arguments after it go to `cargo build` (`--release` is always passed). The program is
# written to TARGET_DIR/x86_64-moruna/release/, TARGET_DIR being CARGO_TARGET_DIR or the
# program's own target directory.
#
# Cargo builds std with -Zbuild-std. `__CARGO_TESTS_ONLY_SRC_ROOT` is the only way it offers to
# point build-std at a source tree other than the toolchain's own rust-src; it takes the
# library/ directory. The program is linked as build.sh links every native program: static,
# at the addresses and in the segments of kernel/sys/user.ld, which the kernel's loader takes.
set -euo pipefail

if [ "$#" -lt 1 ]; then
  echo "usage: kernel/tools/build-std.sh MANIFEST [CARGO_BUILD_ARGUMENTS...]" >&2
  exit 2
fi
kernel="$(cd "$(dirname "$0")/.." && pwd)"
manifest="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
shift
toolchain=nightly-2026-07-21
library="$kernel/vendor/rust-std/library"
target="$kernel/targets/x86_64-moruna.json"
if [ ! -f "$library/std/src/sys/pal/moruna/mod.rs" ]; then
  echo "build-std.sh: no Moruna platform layer under $library" >&2
  exit 1
fi

# build.sh's flags for a native program, less the kernel's own (`polyval_force_soft`, `ktest`).
flags="-C relocation-model=static -C relro-level=off -C force-unwind-tables=yes -C no-redzone=y"
flags="$flags -Zunstable-options -C link-arg=-T$kernel/sys/user.ld -C link-arg=-static"
# The getrandom crate (arrow-array's hasher, ahash, needs it) knows no `target_os = "moruna"` and
# must be told a source: its RDRAND backend reads the CPU's random source, the one the core's
# `random_read` reads. Pending the PM's decision (item 5.0's report): a custom backend over the
# core's call is the other way.
flags="$flags --cfg getrandom_backend=\"rdrand\""

# This entry owns the flags: an inherited encoded set must not replace them.
unset CARGO_ENCODED_RUSTFLAGS
rustup run "$toolchain" cargo --version
__CARGO_TESTS_ONLY_SRC_ROOT="$library" RUSTFLAGS="$flags" \
  rustup run "$toolchain" cargo build --release --locked --manifest-path "$manifest" \
  --target "$target" -Zunstable-options -Zjson-target-spec \
  -Zbuild-std=std,panic_abort -Zbuild-std-features=compiler-builtins-mem "$@"
