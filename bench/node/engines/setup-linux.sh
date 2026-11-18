#!/bin/bash
# The benchmark's Linux machine (Ubuntu 24.04), made ready to run every engine: run as root, once,
# after the machine is created and before any run, as a step of the setup's batch (batch.py, which
# bench/node/run.py starts with one `az vm run-command`), so nothing here is inside a timing.
#
# Its settings arrive as environment variables:
#   BASE      the storage account's blob endpoint, https://<account>.blob.core.windows.net
#   SAS_CODE  a SAS for container `code` (read, add, create, write): harness.tar.gz (the
#             repository at the run's commit) and moruna-066.tar.gz (0.6.6's source tree) are
#             there; 0.6.6's wheel is kept there once built
#   V066      0.6.6's version on PyPI (run.py --version066)
#   WHEEL066  the name in `code` of the wheel the operator gave (run.py --wheel066), or of the one a
#             build keeps there
#
# It installs the system's packages (Spark's JVM, a C toolchain), uv, and Rust 1.98.1 (the
# repository's pin); takes 0.6.6's wheel from the first place that has it (the run's container,
# PyPI, else a build from 0.6.6's tree, kept in the container); then installs every engine with
# install.py, each with its Blob reader. The engines read the data from Blob themselves (store.py),
# so nothing is copied here. Its last line is `SETUP {json}`.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive HOME=/root
CODE=/opt/bench; SRC066=/opt/src066; ENG=/opt/engines; DATA=/data
mkdir -p "$CODE" "$SRC066" "$ENG/wheels" "$DATA/results" "$DATA/work" "$DATA/ref"

# A first boot's apt runs may hold dpkg's lock for a while: wait for it rather than fail.
apt-get -o DPkg::Lock::Timeout=600 update -q >/dev/null
apt-get -o DPkg::Lock::Timeout=600 install -y -q openjdk-17-jre-headless build-essential pkg-config python3-venv \
  curl >/dev/null
export JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64

# uv from PyPI, pinned to the version the harness was checked with on the Mac.
python3 -m venv /opt/uv && /opt/uv/bin/pip install -q uv==0.10.9 && ln -sf /opt/uv/bin/uv /usr/local/bin/uv
# Rust, for the Rust pool, 0.6.6's wheel and its Rust function: the repository's pinned toolchain.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain 1.98.1
export PATH="/root/.cargo/bin:$PATH"

curl -sSf "$BASE/code/harness.tar.gz?$SAS_CODE" | tar -xz -C "$CODE"
uv python install 3.14 3.14t >/dev/null

# 0.6.6's wheel, outside every timing, from the first place that has it:
# 1. the run's container: the wheel the operator gave (run.py --wheel066);
# 2. PyPI, once 0.6.6 is released there: its free-threaded wheel for CPython 3.14t on x86_64,
#    as the release built it (manylinux_2_28);
# 3. built here from 0.6.6's tree (45-60 min) and kept in the container.
V066=${V066:-0.6.6}
WHEEL066=${WHEEL066:-moruna-$V066-cp314-cp314t-linux_x86_64.whl}
start=$(date +%s)
if curl -sSf -o "$ENG/wheels/$WHEEL066" "$BASE/code/$WHEEL066?$SAS_CODE"; then
  wheel_from=container
  wheel="$ENG/wheels/$WHEEL066"
elif rm -f "$ENG/wheels/$WHEEL066" && /opt/uv/bin/pip download -q --no-deps --only-binary=:all: \
    --python-version 3.14 --implementation cp --abi cp314t --platform manylinux_2_28_x86_64 \
    --dest "$ENG/wheels/pypi" "moruna==$V066" >"$ENG/pypi-066.log" 2>&1; then
  wheel_from=pypi
  wheel=$(ls "$ENG"/wheels/pypi/moruna-"$V066"-cp314-cp314t-*.whl | head -1)
else
  curl -sSf "$BASE/code/moruna-066.tar.gz?$SAS_CODE" | tar -xz -C "$SRC066"
  (cd "$SRC066" && CARGO_BUILD_JOBS="$(nproc)" CARGO_INCREMENTAL=0 \
    uv build --wheel --python 3.14t --out-dir "$ENG/wheels/built" . >"$ENG/build-066.log" 2>&1)
  cp "$(ls "$ENG"/wheels/built/moruna-"$V066"-*.whl | head -1)" "$ENG/wheels/$WHEEL066"
  curl -sSf -X PUT -H 'x-ms-blob-type: BlockBlob' -T "$ENG/wheels/$WHEEL066" "$BASE/code/$WHEEL066?$SAS_CODE"
  wheel_from=built
  wheel="$ENG/wheels/$WHEEL066"
fi
wheel_s=$(( $(date +%s) - start ))

python3 "$CODE/bench/node/engines/install.py" --root "$ENG" --gate-src "$CODE/images/gate" \
  --moruna066 "$wheel" --jobs "$(nproc)" > "$ENG/install.out" 2>&1 || true

python3 - "$ENG/installed.json" "$wheel_from" "$wheel_s" "$(java -version 2>&1 | head -1)" "$wheel" <<'EOF'
import hashlib, json, os, sys
installed = {e: r.get("ok") for e, r in json.load(open(sys.argv[1])).items()}
wheel = sys.argv[5]
print("SETUP " + json.dumps({"installed": installed, "wheel_066": sys.argv[2], "wheel_066_s": int(sys.argv[3]),
                             "wheel_066_name": os.path.basename(wheel),
                             "wheel_066_sha256": hashlib.sha256(open(wheel, "rb").read()).hexdigest(),
                             "java": sys.argv[4]}))
EOF
