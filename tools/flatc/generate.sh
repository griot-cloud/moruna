#!/usr/bin/env bash
# Generate the Rust of the job document's FlatBuffers schema (SDD 22 e.4), or, with --check,
# fail when what is committed differs from what the schema generates.
#
#   tools/flatc/generate.sh          writes crates/moruna-job/src/generated/job_generated.rs
#   tools/flatc/generate.sh --check  regenerates into a scratch directory and diffs (CI)
#
# flatc runs inside a container, so nothing is installed on the machine: it is built from the
# source of FLATC_VERSION, verified by its SHA-256, in an image pinned by digest, and kept under
# target/flatc for the next run. FLATC_VERSION is the version of the `flatbuffers` crate in
# Cargo.lock, so the generated code is what that crate reads; a bump of one is a bump of the
# other. What flatc generates does not depend on the compiler that built it or the machine it
# runs on, so a build on an arm64 laptop and one on an amd64 runner generate the same file.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

FLATC_VERSION="25.12.19"
SOURCE_URL="https://codeload.github.com/google/flatbuffers/tar.gz/refs/tags/v$FLATC_VERSION"
SOURCE_SHA256="f81c3162b1046fe8b84b9a0dbdd383e24fdbcf88583b9cb6028f90d04d90696a"
# gcc:14-bookworm, the multi-platform index, by digest: a C++ compiler and nothing to install.
IMAGE="docker.io/library/gcc@sha256:5e927c284bf55a7dc796262e311a0703344f62f41f5621eb56843111b1d37e15"
# flatc's sources, as the release's CMakeLists.txt lists them (FlatBuffers_Compiler_SRCS).
SOURCES="src/file_manager.cpp src/file_name_manager.cpp src/idl_parser.cpp src/idl_gen_text.cpp
  src/reflection.cpp src/util.cpp src/idl_gen_binary.cpp src/idl_gen_cpp.cpp src/idl_gen_csharp.cpp
  src/idl_gen_dart.cpp src/idl_gen_kotlin.cpp src/idl_gen_kotlin_kmp.cpp src/idl_gen_go.cpp
  src/idl_gen_java.cpp src/idl_gen_ts.cpp src/idl_gen_php.cpp src/idl_gen_python.cpp
  src/idl_gen_lobster.cpp src/idl_gen_rust.cpp src/idl_gen_fbs.cpp src/idl_gen_grpc.cpp
  src/idl_gen_json_schema.cpp src/idl_gen_swift.cpp src/flatc.cpp src/flatc_main.cpp
  include/codegen/python.cc src/binary_annotator.cpp src/annotated_binary_text_gen.cpp
  src/bfbs_gen_lua.cpp src/bfbs_gen_nim.cpp src/code_generators.cpp
  grpc/src/compiler/cpp_generator.cc grpc/src/compiler/go_generator.cc
  grpc/src/compiler/java_generator.cc grpc/src/compiler/python_generator.cc
  grpc/src/compiler/swift_generator.cc grpc/src/compiler/ts_generator.cc"
SCHEMA="crates/moruna-job/schema/job.fbs"
OUT="crates/moruna-job/src/generated"

check=0
[ "${1:-}" = "--check" ] && check=1

locked="$(awk '/^name = "flatbuffers"$/{getline; gsub(/"/, "", $3); print $3; exit}' Cargo.lock)"
if [ "$locked" != "$FLATC_VERSION" ]; then
  echo "flatc: Cargo.lock has flatbuffers $locked and this script builds flatc $FLATC_VERSION; move them together" >&2
  exit 1
fi

if command -v podman >/dev/null 2>&1; then
  engine=podman
elif command -v docker >/dev/null 2>&1; then
  engine=docker
else
  echo "flatc: neither podman nor docker is installed; flatc runs in a container" >&2
  exit 1
fi

cache="target/flatc/$FLATC_VERSION/$(uname -m)"
mkdir -p "$cache"
if [ ! -x "$cache/flatc" ]; then
  curl -fsSL --retry 5 --retry-all-errors --connect-timeout 20 -o "$cache/source.tar.gz" "$SOURCE_URL"
  got="$( (shasum -a 256 "$cache/source.tar.gz" 2>/dev/null || sha256sum "$cache/source.tar.gz") | cut -d' ' -f1)"
  if [ "$got" != "$SOURCE_SHA256" ]; then
    echo "flatc: the flatbuffers $FLATC_VERSION source has SHA-256 $got, expected $SOURCE_SHA256" >&2
    rm -f "$cache/source.tar.gz"
    exit 1
  fi
  # One line, so the list reaches the container's shell as one command's arguments.
  sources="$(echo $SOURCES)"
  "$engine" run --rm -v "$PWD:/w" -w /w "$IMAGE" sh -euc "
    rm -rf /tmp/fb && mkdir -p /tmp/fb/obj
    tar -xzf '$cache/source.tar.gz' -C /tmp/fb --strip-components=1
    cd /tmp/fb
    printf '%s\\n' $sources | xargs -P \"\$(nproc)\" -I{} sh -c \\
      'g++ -std=c++17 -O1 -DNDEBUG -Iinclude -Igrpc -Isrc -c {} -o obj/\$(echo {} | tr / _).o'
    g++ -o flatc obj/*.o
    cp flatc '/w/$cache/flatc'
  "
  rm -f "$cache/source.tar.gz"
fi

scratch="target/flatc/out"
rm -rf "$scratch"
mkdir -p "$scratch"
"$engine" run --rm -v "$PWD:/w" -w /w "$IMAGE" \
  "/w/$cache/flatc" --rust -o "/w/$scratch" "/w/$SCHEMA"

if [ "$check" = "1" ]; then
  if ! diff -u "$OUT/job_generated.rs" "$scratch/job_generated.rs"; then
    echo "flatc: $OUT/job_generated.rs is not what $SCHEMA generates; run tools/flatc/generate.sh and commit the result" >&2
    exit 1
  fi
  echo "flatc: $OUT/job_generated.rs is what $SCHEMA generates"
else
  mkdir -p "$OUT"
  cp "$scratch/job_generated.rs" "$OUT/job_generated.rs"
  echo "flatc: wrote $OUT/job_generated.rs"
fi
