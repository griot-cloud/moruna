#!/usr/bin/env bash
# The Linux wheels of a release, for this machine's architecture: built in the manylinux_2_28
# image, so a task image (which installs its lock for manylinux_2_28, python/moruna/_build.py
# PLATFORMS) can install Moruna. 0.6.3's were built on ubuntu-24.04 and tagged manylinux_2_39,
# and only for x86_64, so no task image could install Moruna at all.
#
#     tools/release/linux-wheels.sh RUNTIME OUT [RUNTIME OPTION...]
#     MORUNA_WHEEL_TARGET=aarch64 tools/release/linux-wheels.sh docker OUT   (from x86_64)
#
# RUNTIME runs a Linux container: `docker` on a release runner, `container` (Apple's) on a Mac,
# which gives a container too little memory to compile Moruna unless told (`--memory 16g`).
# Both builds (PY-I7): the free-threaded wheel and the standard one. The toolchain, the registry
# and the build stay under target/, which the release caches, so a release compiles only what
# changed.
set -euo pipefail

runtime=${1:?RUNTIME: docker, or container on a Mac}
out=${2:?OUT: the directory the wheels go to}
shift 2
root=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) echo "linux-wheels: no manylinux image for $(uname -m)" >&2; exit 1 ;;
esac
toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$root/rust-toolchain.toml")
maturin=$(sed -n 's/^requires = \["\(maturin[^"]*\)"\].*/\1/p' "$root/pyproject.toml")
: "${toolchain:?no channel in rust-toolchain.toml}" "${maturin:?no maturin requirement in pyproject.toml}"

# MORUNA_WHEEL_TARGET=aarch64 on an x86_64 machine cross-builds the aarch64 wheels: Azure
# Pipelines has no arm64 build machines (2026-11-15), where GitHub's release ran on native ones.
# The rust-cross manylinux_2_28 image carries a GCC for aarch64 over a manylinux_2_28 sysroot,
# maturin writes the target's interpreter configuration from its own tables, and bindgen is
# pointed at that sysroot.
target=${MORUNA_WHEEL_TARGET:-$arch}
if [ "$target" != "$arch" ]; then
  if [ "$arch" != x86_64 ] || [ "$target" != aarch64 ]; then
    echo "linux-wheels: cross-building $target on $arch is not a recipe this script has" >&2
    exit 1
  fi
  cross=ghcr.io/rust-cross/manylinux_2_28-cross:aarch64@sha256:ca53fa07ecf1c3e6408c51fbca64c036d9d29af832d3f8bb954910e89097f275
  "$runtime" run --rm "$@" -u "$(id -u):$(id -g)" -v "$root:/src" -v "$out:/out" -w /src \
    -e HOME=/src/target/manylinux/home \
    -e CARGO_HOME=/src/target/manylinux/cargo -e RUSTUP_HOME=/src/target/manylinux/rustup \
    -e CARGO_TARGET_DIR=/src/target/manylinux/cross-aarch64 \
    -e UV_INSTALL_DIR=/src/target/manylinux/uv -e UV_CACHE_DIR=/src/target/manylinux/uv-cache \
    -e UV_PYTHON_INSTALL_DIR=/src/target/manylinux/uv-python \
    "$cross" bash -euo pipefail -c "
      mkdir -p /src/target/manylinux/home
      export PATH=/src/target/manylinux/cargo/bin:/src/target/manylinux/uv:\$PATH
      [ -x /src/target/manylinux/cargo/bin/rustup ] \
        || curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain none --profile minimal
      rustup run $toolchain rustc --version >/dev/null 2>&1 || rustup toolchain install $toolchain --profile minimal
      rustup target add --toolchain $toolchain aarch64-unknown-linux-gnu
      [ -x /src/target/manylinux/uv/uv ] || curl -LsSf https://astral.sh/uv/install.sh | sh
      venv=/src/target/manylinux/maturin-cross-aarch64
      [ -x \$venv/bin/maturin ] || uv venv -q -p 3.13 \$venv
      uv pip install -q -p \$venv/bin/python '$maturin' 'libclang==18.1.1'
      export LIBCLANG_PATH=\$(\$venv/bin/python -c 'import clang, os; print(os.path.join(os.path.dirname(clang.__file__), \"native\"))')
      cc=\${TARGET_CC:-aarch64-unknown-linux-gnu-gcc}
      export BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu=\"--sysroot=\$(\$cc -print-sysroot)\"
      \$venv/bin/maturin build --release --target aarch64-unknown-linux-gnu \
        --compatibility manylinux_2_28 --out /out -i python3.14t -i python3.14
    "
  ls "$out"/*-cp314-cp314-manylinux_2_28_aarch64.whl "$out"/*-cp314-cp314t-manylinux_2_28_aarch64.whl
  exit 0
fi

# As the caller, so the release runner can cache and prune what the build leaves under target/.
"$runtime" run --rm "$@" -u "$(id -u):$(id -g)" -v "$root:/src" -v "$out:/out" -w /src \
  -e HOME=/src/target/manylinux/home \
  -e CARGO_HOME=/src/target/manylinux/cargo -e RUSTUP_HOME=/src/target/manylinux/rustup \
  -e CARGO_TARGET_DIR="/src/target/manylinux/$arch" \
  "quay.io/pypa/manylinux_2_28_$arch" bash -euo pipefail -c "
    mkdir -p /src/target/manylinux/home
    export PATH=/src/target/manylinux/cargo/bin:\$PATH
    [ -x /src/target/manylinux/cargo/bin/rustup ] \
      || curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain none --profile minimal
    rustup run $toolchain rustc --version >/dev/null 2>&1 || rustup toolchain install $toolchain --profile minimal
    venv=/src/target/manylinux/maturin-$arch
    [ -x \$venv/bin/maturin ] || /opt/python/cp314-cp314t/bin/python -m venv \$venv
    # bindgen (vortex-io's custom-labels) needs libclang, which the image does not carry; the
    # libclang wheel does, and needs no root to install.
    \$venv/bin/python -m pip install --quiet '$maturin' 'libclang==18.1.1'
    export LIBCLANG_PATH=\$(\$venv/bin/python -c 'import clang, os; print(os.path.join(os.path.dirname(clang.__file__), \"native\"))')
    \$venv/bin/maturin build --release --compatibility manylinux_2_28 --out /out \
      --interpreter /opt/python/cp314-cp314t/bin/python /opt/python/cp314-cp314/bin/python
  "
ls "$out"/*-cp314-cp314-manylinux_2_28_"$arch".whl "$out"/*-cp314-cp314t-manylinux_2_28_"$arch".whl
