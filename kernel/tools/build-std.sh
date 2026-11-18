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
#
# The build is offline. std's crates are library/'s own: library/Cargo.toml patches each of std's
# crates.io dependencies to its copy in library/vendor, so std's lock names no registry, which
# this script checks before it builds. The program's own crates are fetched first, by
# `cargo fetch`; the build then runs --offline, and fetches nothing for std or the program.
#
# A program that reaches the getrandom crate (0.3) links kernel/getrandom (`moruna-getrandom`),
# its custom backend over the core's random_read, which this script selects.
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
# Nothing of std from a registry: every package of its lock is a path of library/.
if grep -n '^source = ' "$library/Cargo.lock" >&2; then
  echo "build-std.sh: $library/Cargo.lock names a registry package above: std would be fetched" >&2
  exit 1
fi

# build.sh's flags for a native program, less the kernel's own (`polyval_force_soft`, `ktest`).
flags="-C relocation-model=static -C relro-level=off -C force-unwind-tables=yes -C no-redzone=y"
flags="$flags -Zunstable-options -C link-arg=-T$kernel/sys/user.ld -C link-arg=-static"
# getrandom knows no `target_os = "moruna"`: its custom backend is kernel/getrandom's, the
# core's random_read, the one source of entropy (PM, 2026-11-18).
flags="$flags --cfg getrandom_backend=\"custom\""
# Cargo's build-std fingerprints std and the crates std depends on as it does a registry's: an
# edit to library/ or to the Moruna crates std builds on (moruna-sys, moruna-abi,
# moruna-record) would leave std stale in a target directory that is kept, as moruna-arm's
# shared one is. A digest of those sources goes into every unit's metadata, so an edit rebuilds
# std and what links it, and an unchanged tree rebuilds nothing.
inputs="$(python3 - "$kernel/.." <<'PY'
import hashlib, os, sys
root = sys.argv[1]
digest = hashlib.sha256()
for top in ("kernel/vendor/rust-std/library", "kernel/sys", "kernel/abi", "crates/moruna-record"):
    for here, dirs, files in os.walk(os.path.join(root, top)):
        dirs[:] = sorted(d for d in dirs if d != "target")
        for name in sorted(files):
            path = os.path.join(here, name)
            digest.update(os.path.relpath(path, root).encode() + b"\0")
            with open(path, "rb") as f:
                digest.update(hashlib.sha256(f.read()).digest())
print(digest.hexdigest()[:16])
PY
)"
flags="$flags -C metadata=moruna-std-$inputs"

# This entry owns the flags: an inherited encoded set must not replace them.
unset CARGO_ENCODED_RUSTFLAGS
rustup run "$toolchain" cargo --version
rustup run "$toolchain" cargo fetch --locked --manifest-path "$manifest"
__CARGO_TESTS_ONLY_SRC_ROOT="$library" RUSTFLAGS="$flags" \
  rustup run "$toolchain" cargo build --release --locked --offline --manifest-path "$manifest" \
  --target "$target" -Zunstable-options -Zjson-target-spec \
  -Zbuild-std=std,panic_abort -Zbuild-std-features=compiler-builtins-mem "$@"
