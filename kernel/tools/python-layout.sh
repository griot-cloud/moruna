#!/usr/bin/env bash
# Writes the OCI layout of the official python:3.14-slim image for linux/amd64, which
# the Linux personality's boot image is built from (build.sh, SDD 28 k), with docker
# or Apple's container.
#
#   kernel/tools/python-layout.sh [OUT]
#
# OUT defaults to kernel/target/oci/python.
set -euo pipefail

kernel="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$kernel/target/oci/python}"
image=docker.io/library/python:3.14-slim
archive="$out.tar"
mkdir -p "$(dirname "$out")"
if command -v docker >/dev/null; then
  docker pull --platform linux/amd64 "$image"
  docker save --platform linux/amd64 -o "$archive" "$image"
elif command -v container >/dev/null; then
  container image pull --platform linux/amd64 "$image"
  container image save --platform linux/amd64 -o "$archive" "$image"
else
  echo "python-layout.sh: neither docker nor container is here to pull $image" >&2
  exit 1
fi
rm -rf "$out"
mkdir -p "$out"
tar -xf "$archive" -C "$out"
rm -f "$archive"
echo "python-layout.sh: $image for linux/amd64 is at $out"
