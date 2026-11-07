#!/usr/bin/env bash
# The Linux wheels of a release, for this machine's architecture: built in the manylinux_2_28
# image, so a task image (which installs its lock for manylinux_2_28, python/moruna/_build.py
# PLATFORMS) can install Moruna. 0.6.3's were built on ubuntu-24.04 and tagged manylinux_2_39,
# and only for x86_64, so no task image could install Moruna at all.
#
#     tools/release/linux-wheels.sh RUNTIME OUT [RUNTIME OPTION...]
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
