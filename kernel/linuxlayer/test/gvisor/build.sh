#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Builds the gVisor system-call tests that apply (`applies`) from gVisor's source at the tag
# Asterinas a0de2258 runs, with Bazel, into OUT, stripped. gVisor's checkout and Bazel's
# cache stay under CACHE, so a second build only builds what changed. Run it on Linux x86-64
# (moruna-bench), not on a Mac.
#
#   kernel/linuxlayer/test/gvisor/build.sh [OUT] [CACHE]
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
tag=release-20260622.0
cache="${2:-$HOME/b2}"
out="${1:-$cache/gvisor-bins}"
mkdir -p "$cache" "$out" "$HOME/.local/bin"
export PATH="$HOME/.local/bin:$PATH"

# Bazel, through bazelisk at a pinned release, checked against the digest GitHub publishes
# for the asset; bazelisk then fetches the Bazel version gVisor's .bazelversion names.
if ! command -v bazel >/dev/null; then
  version=v1.25.0
  asset=bazelisk-linux-amd64
  digest="$(curl -fsSL "https://api.github.com/repos/bazelbuild/bazelisk/releases/tags/$version" |
    python3 -c "import json,sys; print(next(a['digest'] for a in json.load(sys.stdin)['assets'] if a['name'] == '$asset'))")"
  curl -fsSL -o "$cache/$asset" "https://github.com/bazelbuild/bazelisk/releases/download/$version/$asset"
  echo "${digest#sha256:}  $cache/$asset" | sha256sum -c -
  install -m 0755 "$cache/$asset" "$HOME/.local/bin/bazel"
fi

if [ ! -d "$cache/gvisor/.git" ]; then
  git clone -q -b "$tag" --single-branch --depth 1 https://github.com/google/gvisor.git "$cache/gvisor"
fi
cd "$cache/gvisor"
test "$(git describe --tags --exact-match 2>/dev/null || git rev-parse --abbrev-ref HEAD)" = "$tag"

mapfile -t names < <(grep -v '^#' "$here/applies" | sed 's/#.*//; s/[[:space:]]//g' | grep .)
targets=()
for name in "${names[@]}"; do
  targets+=("//test/syscalls/linux:$name")
done
bazel --output_user_root="$cache/bazel" build -c opt --test_tag_filters=native "${targets[@]}"
for name in "${names[@]}"; do
  install -m 0755 "bazel-bin/test/syscalls/linux/$name" "$out/$name"
  strip -s "$out/$name" 2>/dev/null || true
done
echo "build.sh: ${#names[@]} test binaries in $out"
