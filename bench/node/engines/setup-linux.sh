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
#   ENGINES   install.py's engines the run needs (run.py: those its runs use, measure.py's own and
#             gate-canon's); every engine when unset
#
# It installs the system's packages (Spark's JVM, a C toolchain), uv, and Rust 1.98.1 (the
# repository's pin); takes 0.6.6's wheel from the first place that has it (the run's container,
# PyPI, else a build from 0.6.6's tree, kept in the container); then installs the engines with
# install.py, each on the newest CPython it installs on, with its Blob reader. The engines read the
# data from Blob themselves (store.py), so nothing is copied here. Its last line is `SETUP {json}`:
# per engine whether it installed, its interpreter and, for one that did not, why. It exits 1 when
# an engine did not install, and the run stops there (work item 6.2c).
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive HOME=/root
CODE=/opt/bench; SRC066=/opt/src066; ENG=/opt/engines; DATA=/data
mkdir -p "$CODE" "$SRC066" "$ENG/wheels" "$DATA/results" "$DATA/work" "$DATA/ref"

# A first boot's apt runs may hold dpkg's lock for a while: wait for it rather than fail.
apt-get -o DPkg::Lock::Timeout=600 update -q >/dev/null
apt-get -o DPkg::Lock::Timeout=600 install -y -q openjdk-17-jre-headless build-essential pkg-config python3-venv \
  curl >/dev/null
export JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64

# uv's downloads wait up to 5 minutes and are tried 5 times: at uv's 30 s a large wheel from
# PyPI (pyarrow for Ray, Dask, DuckDB and Spark) timed out from Central India, and none of the
# four engines ran (6.2 run 4, 2026-11-18). This is the harness's network, not an engine's time.
export UV_HTTP_TIMEOUT=300 UV_HTTP_RETRIES=5
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
ENGINES=${ENGINES:-harness,moruna066,moruna066-rust,rust-pool,duckdb,polars,ray,spark,dask}
start=$(date +%s)
wheel=""
if [[ ",$ENGINES," != *,moruna066,* ]]; then
  wheel_from=none
elif curl -sSf -o "$ENG/wheels/$WHEEL066" "$BASE/code/$WHEEL066?$SAS_CODE"; then
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

status=0
python3 "$CODE/bench/node/engines/install.py" --root "$ENG" --gate-src "$CODE/images/gate" \
  ${wheel:+--moruna066 "$wheel"} --engines "$ENGINES" --jobs "$(nproc)" > "$ENG/install.out" 2>&1 || status=$?

# Each engine's interpreter (its version, its GIL, those tried before it) and, for one that did not
# install, why: the end of its install's error, and for one tried on several interpreters each
# one's own last lines (install.py's `tried`). The run's setup.json keeps them, and run.py stops on
# the first, so a setup that fails explains itself (run 3's Ray and Dask did not).
python3 - "$ENG/installed.json" "$wheel_from" "$wheel_s" "$(java -version 2>&1 | head -1)" "$wheel" <<'EOF'
import hashlib, json, os, sys
done = json.load(open(sys.argv[1])) if os.path.exists(sys.argv[1]) else {}
wheel = sys.argv[5]
facts = {"installed": {e: r.get("ok") for e, r in done.items()},
         "interpreters": {e: {"python": r.get("python"), "gil": r.get("gil"),
                              "tried": [t["python"] for t in r.get("tried", [])]}
                          for e, r in done.items() if r.get("python")},
         "wheel_066": sys.argv[2] if wheel else None, "wheel_066_s": int(sys.argv[3]),
         "wheel_066_name": os.path.basename(wheel) if wheel else None,
         "wheel_066_sha256": hashlib.sha256(open(wheel, "rb").read()).hexdigest() if wheel else None,
         "java": sys.argv[4]}
failed = {e: r for e, r in done.items() if not r.get("ok")}


def line(limit):
    errors = {e: ("installs and imports on none of CPython " + ", ".join(t["python"] for t in r["tried"])
                  if r.get("tried") else str(r.get("error"))[-limit:]) for e, r in failed.items()}
    tried = {e: [{"python": t["python"], "error": str(t["error"])[-limit:]} for t in r["tried"]]
             for e, r in failed.items() if r.get("tried")}
    return "SETUP " + json.dumps(dict(facts, errors=errors, tried=tried), ensure_ascii=False)


# The batch keeps the last 8 KiB of this script's output: each error is cut until the line is well
# inside that, so the line always reaches setup.json whole.
for limit in (500, 300, 200, 120, 60, 20):
    text = line(limit)
    if len(text.encode()) <= 6000:
        break
print(text)
EOF
exit "$status"
