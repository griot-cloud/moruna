#!/usr/bin/env bash
# Builds the C SDK for the core's user target (item 5.2b, system design 5B), and the C functions
# that run on it as static native programs:
#
#   kernel/function/c/build-core.sh
#
# Into TARGET/core (TARGET being CARGO_TARGET_DIR or kernel/function/c/target):
#   lib/libmoruna_function.a  std, the SDK (moruna-function) and relibc on Moruna's platform:
#                             one crate graph, one runtime, std's _start (5B)
#   lib/libopenlibm.a         the C math library (kernel/vendor/relibc/openlibm)
#   include/                  relibc's headers (cbindgen's, from relibc's sources), openlibm's,
#                             and moruna_function.h
#   bin/double                examples/double.c
#   bin/sum                   the aggregate test (tests/c/scenarios.c, moruna_test_sum)
#   bin/runtime               tests/c/runtime.c: malloc, pthread_create, a thread_local, stderr
#
# The archive is built as kernel/tools/build-std.sh builds a program with std (the kernel's
# nightly, std from kernel/vendor/rust-std/library, offline); the C is compiled to ELF by the
# host's clang (CC) for x86_64-unknown-none-elf, with __moruna__ defined, against those headers
# only; the programs are linked by rust-lld with kernel/targets/x86_64-moruna.ld, static.
# cbindgen (CBINDGEN, else on PATH) writes relibc's headers, as relibc's own build does.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
kernel="$(cd "$here/../.." && pwd)"
relibc="$kernel/vendor/relibc"
library="$kernel/vendor/rust-std/library"
target="$kernel/targets/x86_64-moruna.json"
script="$kernel/targets/x86_64-moruna.ld"
toolchain=nightly-2026-07-21
out="${CARGO_TARGET_DIR:-$here/target}/core"
cc="${CC:-cc}"
cbindgen="${CBINDGEN:-cbindgen}"

command -v "$cbindgen" >/dev/null || {
  echo "build-core.sh: cbindgen not found (relibc's headers are its): set CBINDGEN" >&2
  exit 1
}
sysroot="$(rustup run "$toolchain" rustc --print sysroot)"
host="$(rustup run "$toolchain" rustc -vV | sed -n 's/^host: //p')"
tools="$sysroot/lib/rustlib/$host/bin"
ar="$tools/llvm-ar"
lld="$tools/rust-lld"
resource="$("$cc" -print-resource-dir)/include"

# The C side of the user target: x86_64 ELF, SysV ABI, no red zone, static and not PIC, TLS by
# the local-exec model, as the Rust side is built (x86_64-moruna.json, build-std.sh's flags).
cflags=(--target=x86_64-unknown-none-elf -D__moruna__ -mno-red-zone -fno-stack-protector
  -fno-pic -fno-pie -ftls-model=local-exec -O2 -g)
# Against the C library's headers, then the compiler's own (float.h, stdarg.h and the like).
includes=(-nostdinc -isystem "$out/include" -isystem "$resource")

mkdir -p "$out/lib" "$out/include" "$out/bin" "$out/obj"

# 1. The archive: std, the SDK and relibc, with std's _start.
flags="-C relocation-model=static -C relro-level=off -C force-unwind-tables=yes -C no-redzone=y"
flags="$flags -Zunstable-options --cfg getrandom_backend=\"custom\""
unset CARGO_ENCODED_RUSTFLAGS
CC_x86_64_moruna="$cc" AR_x86_64_moruna="$ar" CFLAGS_x86_64_moruna="${cflags[*]}" \
__CARGO_TESTS_ONLY_SRC_ROOT="$library" RUSTFLAGS="$flags" \
  rustup run "$toolchain" cargo rustc --release --locked --offline \
  --manifest-path "$here/Cargo.toml" --lib --crate-type staticlib \
  --target "$target" -Zunstable-options -Zjson-target-spec \
  -Zbuild-std=std,panic_abort -Zbuild-std-features=compiler-builtins-mem
cp "${CARGO_TARGET_DIR:-$here/target}/x86_64-moruna/release/libmoruna_function.a" "$out/lib/"

# 2. The headers: relibc's own, openlibm's, cbindgen's from each of relibc's header modules (not
# its Rust math, which openlibm's replaces), and the SDK's.
rm -rf "$out/include"
mkdir -p "$out/include"
cp -R "$relibc/include/." "$out/include/"
cp "$relibc/openlibm/include/"*.h "$relibc/openlibm/src/"*.h "$out/include/"
for dir in "$relibc/src/header/"*/; do
  name="$(basename "$dir")"
  case "$name" in _* | math) continue ;; esac
  [ -f "$dir/cbindgen.toml" ] || continue
  header="$out/include/$(echo "$name" | tr _ /).h"
  mkdir -p "$(dirname "$header")"
  (cd "$relibc" && cat "$dir/cbindgen.toml" cbindgen.globdefs.toml |
    "$cbindgen" "src/header/$name/mod.rs" --config=/dev/stdin --output "$header" --quiet)
done
cp "$here/moruna_function.h" "$out/include/"

# 3. openlibm, built from a copy so the vendored tree stays as it is.
rm -rf "$out/openlibm"
cp -R "$relibc/openlibm" "$out/openlibm"
make -s -C "$out/openlibm" libopenlibm.a OS=Linux ARCH=amd64 AR="$ar" \
  CC="$cc ${cflags[*]} ${includes[*]} -fno-builtin -Wno-everything" >/dev/null
cp "$out/openlibm/libopenlibm.a" "$out/lib/"

# 4. The C functions: each a static native program, its entry std's _start.
compile() {
  local object="$out/obj/$(basename "$1" .c).o"
  "$cc" "${cflags[@]}" "${includes[@]}" -std=c11 -Wall -Wextra -Werror -I"$here" \
    -c "$here/$1" -o "$object"
  echo "$object"
}
link() {
  local name="$1"
  shift
  "$lld" -flavor gnu -static -no-pie -z norelro --gc-sections -T "$script" \
    -o "$out/bin/$name" "$@" "$out/lib/libmoruna_function.a" "$out/lib/libopenlibm.a"
}
link double "$(compile examples/double.c)"
link sum "$(compile tests/c/sum.c)" "$(compile tests/c/scenarios.c)" "$(compile tests/c/support.c)"
link runtime "$(compile tests/c/runtime.c)"
ls -l "$out/bin"
