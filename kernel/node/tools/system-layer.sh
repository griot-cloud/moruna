#!/usr/bin/env bash
# The cloud image's system layer (system design 1A, items 3.2 and 3.3): Moruna's engine,
# `moruna-engine` built as kernel/tools/build.sh builds it (x86_64, static, musl), at
# /moruna/bin/moruna beside an empty /tmp, packed by that engine's own `volume pack` into the
# new layer file OUT. `package.py --system-layer OUT` writes it whole into the boot disk's
# moruna-system partition. Every image and every boot of one take their engine this way.
#
#   kernel/node/tools/system-layer.sh OUT
#
# OUT must not exist yet. The engine is built in CARGO_TARGET_DIR (the root workspace's
# `target` when unset), and run once, for `volume pack`, so the machine must run x86_64 Linux
# programs. It needs, as build.sh does, a C compiler for x86_64 Linux (x86_64-linux-gnu-gcc, or
# gcc on x86_64), musl (musl-gcc on x86_64, or musl-dev:amd64's specs), and the root toolchain's
# x86_64-unknown-linux-musl target. Nothing is fetched.
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: system-layer.sh OUT" >&2
  exit 2
fi
out="$1"
case "$out" in
  /*) ;;
  *) out="$PWD/$out" ;;
esac
if [ -e "$out" ] || [ -L "$out" ]; then
  echo "system-layer.sh: $out exists; the layer is written to a new file" >&2
  exit 2
fi
root="$(cd "$(dirname "$0")/../../.." && pwd)"

if command -v x86_64-linux-gnu-gcc >/dev/null; then
  gnucc=x86_64-linux-gnu-gcc
elif [ "$(uname -m)" = x86_64 ] && command -v gcc >/dev/null; then
  gnucc=gcc
else
  echo "system-layer.sh: no C compiler for x86_64 Linux (gcc-x86-64-linux-gnu, or gcc on x86_64)" >&2
  exit 1
fi
specs=/usr/lib/x86_64-linux-musl/musl-gcc.specs
if [ "$(uname -m)" = x86_64 ] && command -v musl-gcc >/dev/null; then
  muslcc=(musl-gcc)
elif [ -r "$specs" ]; then
  muslcc=("$gnucc" -specs="$specs")
else
  echo "system-layer.sh: no musl for x86_64 (musl-tools, or musl-dev:amd64 with $specs)" >&2
  exit 1
fi

# build.sh's flags for the engine, and no inherited encoded override of them.
unset CARGO_ENCODED_RUSTFLAGS
musl=x86_64-unknown-linux-musl
(cd "$root" && \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$gnucc" \
  CC_x86_64_unknown_linux_musl="${muslcc[0]}" \
  CFLAGS_x86_64_unknown_linux_musl="${muslcc[*]:1}" \
  RUSTFLAGS="--cfg polyval_force_soft -C link-self-contained=yes -C target-feature=+crt-static -C strip=symbols" \
  cargo build --quiet --release --locked --target "$musl" -p moruna-runtime --bin moruna-engine)
target="${CARGO_TARGET_DIR:-$root/target}"
case "$target" in
  /*) ;;
  *) target="$root/$target" ;;
esac
engine="$target/$musl/release/moruna-engine"
if [ ! -x "$engine" ]; then
  echo "system-layer.sh: the build left no engine at $engine" >&2
  exit 1
fi

# The layer's tree: the engine where node-admission's engine.conf runs it, and /tmp, its HOME.
tree="$(mktemp -d)"
trap 'rm -rf "$tree"' EXIT
mkdir -p "$tree/moruna/bin" "$tree/tmp"
chmod 0755 "$tree" "$tree/moruna" "$tree/moruna/bin" "$tree/tmp"
cp "$engine" "$tree/moruna/bin/moruna"
chmod 0755 "$tree/moruna/bin/moruna"
"$engine" volume pack "$tree" "$out"
