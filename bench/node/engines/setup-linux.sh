#!/bin/bash
# The benchmark's Linux machine (Ubuntu 24.04), made ready to run every engine: run as root by
# `az vm run-command invoke`, once, after the machine is created and before any run, so nothing
# here is inside a timing (bench/node/run.py).
#
# Its settings arrive as environment variables (run-command's named parameters):
#   BASE      the storage account's blob endpoint, https://<account>.blob.core.windows.net
#   SAS_CODE  a SAS for container `code` (read, add, create, write): harness.tar.gz (the
#             repository at the run's commit) and moruna-066.tar.gz (0.6.6's source tree) are
#             there; 0.6.6's wheel is kept there once built
#
# It installs the system's packages (Spark's JVM, a C toolchain), uv, and Rust 1.98.1 (the
# repository's pin); builds 0.6.6's wheel once, unless the container already keeps one, and keeps
# it there; then installs every engine with install.py, each with its Blob reader. The engines
# read the data from Blob themselves (store.py), so nothing is copied here. Its last line is
# `SETUP {json}`.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive HOME=/root
CODE=/opt/bench; SRC066=/opt/src066; ENG=/opt/engines; DATA=/data
mkdir -p "$CODE" "$SRC066" "$ENG/wheels" "$DATA/results" "$DATA/work" "$DATA/ref"

apt-get update -q >/dev/null
apt-get install -y -q openjdk-17-jre-headless build-essential pkg-config python3-venv curl >/dev/null
export JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64

# uv from PyPI, pinned to the version the harness was checked with on the Mac.
python3 -m venv /opt/uv && /opt/uv/bin/pip install -q uv==0.10.9 && ln -sf /opt/uv/bin/uv /usr/local/bin/uv
# Rust, for the Rust pool, 0.6.6's wheel and its Rust function: the repository's pinned toolchain.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain 1.98.1
export PATH="/root/.cargo/bin:$PATH"

curl -sSf "$BASE/code/harness.tar.gz?$SAS_CODE" | tar -xz -C "$CODE"
uv python install 3.14 3.14t >/dev/null

# 0.6.6's wheel: built once, outside every timing, and kept in the run's container.
WHEEL=moruna-0.6.6-cp314-cp314t-linux_x86_64.whl
start=$(date +%s)
if curl -sSf -o "$ENG/wheels/$WHEEL" "$BASE/code/$WHEEL?$SAS_CODE"; then
  wheel_from=container
else
  curl -sSf "$BASE/code/moruna-066.tar.gz?$SAS_CODE" | tar -xz -C "$SRC066"
  (cd "$SRC066" && CARGO_BUILD_JOBS="$(nproc)" CARGO_INCREMENTAL=0 \
    uv build --wheel --python 3.14t --out-dir "$ENG/wheels/built" . >"$ENG/build-066.log" 2>&1)
  cp "$(ls "$ENG"/wheels/built/moruna-0.6.6-*.whl | head -1)" "$ENG/wheels/$WHEEL"
  curl -sSf -X PUT -H 'x-ms-blob-type: BlockBlob' -T "$ENG/wheels/$WHEEL" "$BASE/code/$WHEEL?$SAS_CODE"
  wheel_from=built
fi
wheel_s=$(( $(date +%s) - start ))

python3 "$CODE/bench/node/engines/install.py" --root "$ENG" --gate-src "$CODE/images/gate" \
  --moruna066 "$ENG/wheels/$WHEEL" --jobs "$(nproc)" > "$ENG/install.out" 2>&1 || true

python3 - "$ENG/installed.json" "$wheel_from" "$wheel_s" "$(java -version 2>&1 | head -1)" <<'EOF'
import json, sys
installed = {e: r.get("ok") for e, r in json.load(open(sys.argv[1])).items()}
print("SETUP " + json.dumps({"installed": installed, "wheel_066": sys.argv[2], "wheel_066_s": int(sys.argv[3]),
                             "java": sys.argv[4]}))
EOF
