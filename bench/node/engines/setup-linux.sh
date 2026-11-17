#!/bin/bash
# The benchmark's Linux machine (Ubuntu 24.04), made ready to run every engine: run as root by
# `az vm run-command invoke`, once, after the machine is created (bench/node/run.py).
#
# Its settings arrive as environment variables (run-command's named parameters):
#   BASE      the storage account's blob endpoint, https://<account>.blob.core.windows.net
#   SAS_CODE  a read SAS for container `code`: harness.tar.gz (the repository at the run's commit)
#             and moruna-066.tar.gz (0.6.6's source tree)
#   SAS_DATA  a read SAS for container `data`: the plan's Parquet files
#   FILES     the plan's file names, space separated
#
# It installs the system's packages (Spark's JVM, a C toolchain), uv, and Rust 1.98.1 (the
# repository's pin), then the engines with install.py, then copies the data to the local disk,
# timed apart (system design 6A): its last line is `SETUP {json}` with the copy's bytes and seconds.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive HOME=/root
CODE=/opt/bench; SRC066=/opt/src066; ENG=/opt/engines; DATA=/data
mkdir -p "$CODE" "$SRC066" "$ENG" "$DATA/nyc" "$DATA/results" "$DATA/work" "$DATA/ref"

apt-get update -q >/dev/null
apt-get install -y -q openjdk-17-jre-headless build-essential pkg-config python3-venv curl >/dev/null
export JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64

# uv from PyPI, pinned to the version the harness was checked with on the Mac.
python3 -m venv /opt/uv && /opt/uv/bin/pip install -q uv==0.10.9 && ln -sf /opt/uv/bin/uv /usr/local/bin/uv
# Rust, for the Rust pool, 0.6.6's wheel and its Rust function: the repository's pinned toolchain.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain 1.98.1
export PATH="/root/.cargo/bin:$PATH"

curl -sSf "$BASE/code/harness.tar.gz?$SAS_CODE" | tar -xz -C "$CODE"
curl -sSf "$BASE/code/moruna-066.tar.gz?$SAS_CODE" | tar -xz -C "$SRC066"
uv python install 3.14 3.14t >/dev/null

python3 "$CODE/bench/node/engines/install.py" --root "$ENG" --gate-src "$CODE/images/gate" \
  --moruna066 "$SRC066" --jobs "$(nproc)" > "$ENG/install.out" 2>&1 || true

start=$(date +%s.%N); bytes=0
for f in $FILES; do
  curl -sSf -o "$DATA/nyc/$f" "$BASE/data/$f?$SAS_DATA"
  bytes=$((bytes + $(stat -c %s "$DATA/nyc/$f")))
done
end=$(date +%s.%N)
python3 - "$ENG/installed.json" "$bytes" "$start" "$end" "$(java -version 2>&1 | head -1)" <<'EOF'
import json, sys
installed = {e: r.get("ok") for e, r in json.load(open(sys.argv[1])).items()}
print("SETUP " + json.dumps({"copy_bytes": int(sys.argv[2]), "copy_s": round(float(sys.argv[4]) - float(sys.argv[3]), 2),
                             "installed": installed, "java": sys.argv[5]}))
EOF
