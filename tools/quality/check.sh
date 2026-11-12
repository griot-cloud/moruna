#!/usr/bin/env bash
# Moruna quality gate. Run by the pre-commit hook (tools/hooks/pre-commit) and by
# the first CI job (preamble 6.6). It fails on: an em dash in any tracked text
# file; cargo fmt drift; a clippy warning; a wildcard arm over Tier or
# StagingCodec (tools/lint/no_tier_wildcard.sh, CT-T14); a broken link, a tab
# or a page in no toctree under docs/ (tools/docs/check_docs.py; CI also builds
# the site with Sphinx, warnings as errors); a commit-message rule the DCO self-test
# rejects (tools/quality/check_dco.sh, F6.5); a supply-chain rule of deny.toml
# when cargo-deny is installed (F6.5); a failing test; a failing
# examples/append_column.rs, which is the first program a user writes; line
# coverage below MORUNA_COVERAGE_MIN (default 80) in any workspace crate that has
# instrumented lines (a stub crate with no code is not measured). Python checks
# run once python/pyproject.toml exists (wave 5).
set -euo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
MIN="${MORUNA_COVERAGE_MIN:-80}"
# `--fast` is the pre-commit hook's half: every check that takes seconds on an incremental
# build (em dashes, stubs, the docs, formatting, clippy over every target, which compiles the
# tests too, the tier lint and the supply-chain bans). The pre-push hook runs the whole gate,
# tests, the 80% coverage floor and the Python suite included, so nothing leaves the machine
# unchecked and a commit does not wait ten minutes for it.
FAST=0
[ "${1:-}" = "--fast" ] && FAST=1

fail() { printf 'quality: FAIL: %s\n' "$*" >&2; exit 1; }
step() { printf 'quality: %s\n' "$*"; }
# Run a step quietly: its output goes to target/quality/<name>.log and is shown
# only when the step fails (or when MORUNA_QUALITY_VERBOSE=1), so a passing gate
# prints one line per step and a failing one prints the evidence.
quiet() {
  local name="$1"; shift
  mkdir -p target/quality
  if [ "${MORUNA_QUALITY_VERBOSE:-0}" = "1" ]; then "$@"; return; fi
  if ! "$@" >"target/quality/$name.log" 2>&1; then
    # The interesting lines, not the whole run: a workspace test log is thousands
    # of "ok" lines and the failure is the last thing anyone wants to scroll for.
    grep -E 'FAILED|panicked at|^error|^warning: unused|test result: FAILED' \
      "target/quality/$name.log" | head -40 >&2 || true
    echo "--- last 20 lines of target/quality/$name.log ---" >&2
    tail -20 "target/quality/$name.log" >&2
    fail "$name (full log: target/quality/$name.log)"
  fi
}

step "em dashes"
EM="$(printf '\xe2\x80\x94')"
if git ls-files -z | xargs -0 grep -In -e "$EM" -- 2>/dev/null; then
  fail "em dash found; use a comma, a colon or parentheses (architecture/README.md, conventions)"
fi

step "tools/quality/no_stubs.sh (nothing in a shipping crate is a stub)"
# The allowance list is empty: every crate of preamble 6.1 has code, the last of them
# `moruna-py` (component 12, 2026-09-22). The variable is gone with it, which is what the
# comment it replaces asked for.
quiet no_stubs tools/quality/no_stubs.sh

if [ -d docs ] && [ -x tools/docs/check_docs.py ]; then
  step "tools/docs/check_docs.py (docs conventions, links, every page in a toctree)"
  quiet docs tools/docs/check_docs.py
fi

step "tools/quality/check_dco.sh --self-test"
quiet dco tools/quality/check_dco.sh --self-test

if [ -f Cargo.toml ]; then
  step "cargo fmt --check"
  quiet fmt cargo fmt --all -- --check
  step "cargo clippy -D warnings"
  quiet clippy cargo clippy --workspace --all-targets -- -D warnings
  if [ -x tools/lint/no_tier_wildcard.sh ]; then
    step "tools/lint/no_tier_wildcard.sh"
    quiet lint tools/lint/no_tier_wildcard.sh
  else
    step "tools/lint/no_tier_wildcard.sh not present yet (wave 0 deliverable); skipped"
  fi
  # Supply-chain policy (deny.toml, board F6.5). cargo-deny is a CI tool, not a
  # workspace dependency, so a machine without it still passes this gate.
  # Locally only the offline, deterministic halves are fatal: `bans` (one arrow,
  # parquet, object_store and tokio) and `sources` (crates.io only). `advisories`
  # needs to fetch the RustSec database, which a pre-commit hook must not do, and
  # `licenses` is reported rather than enforced here because it currently carries
  # an open decision for the PM (see the CC0-1.0 note in deny.toml). The
  # supply-chain workflow runs all four and fails on any of them.
  if command -v cargo-deny >/dev/null 2>&1; then
    step "cargo deny check bans sources"
    quiet deny cargo deny check bans sources
    if cargo deny check licenses >/dev/null 2>&1; then
      step "cargo deny check licenses: clean"
    else
      step "cargo deny check licenses: open finding, see deny.toml; the supply-chain workflow fails on it"
    fi
  else
    step "cargo-deny is not installed (cargo install --locked cargo-deny); deny.toml not checked here, the supply-chain workflow checks it"
  fi
  if [ "$FAST" = "1" ]; then
    step "OK (fast checks passed. The full release gate runs in required Linux CI)"
    exit 0
  fi
  step "cargo test"
  quiet test cargo test --workspace
  # The feature-gated crates are invisible to a default-feature run: moruna-adapters,
  # moruna-polars and moruna-datafusion compile to nothing without `python`, `polars`
  # and `datafusion`, so the gate reported them as stubs and their coverage as "not
  # measured" while they held thousands of lines (found by the component 5 agent,
  # 2026-09-22). The engine bridges need no interpreter and are always built; the
  # Python adapter needs an interpreter with pyarrow, so it runs when one is
  # configured and says so plainly when it is not.
  # The first program a user writes, run for real: a Parquet file in, a kernel that appends a
  # column, a Parquet file out, at a 512 MiB budget and otherwise nothing but defaults. Four
  # defects reached a built wheel while the suite was green, and every one of them would have
  # failed this (PM, 2026-09-22).
  step "cargo run --example append_column (examples/append_column.rs)"
  quiet example cargo run -p moruna-runtime --example append_column
  step "cargo test --features polars,datafusion,peql (the engine bridges)"
  quiet test_bridges cargo test -p moruna-polars -p moruna-datafusion --features moruna-polars/polars,moruna-datafusion/datafusion,moruna-datafusion/peql
  # The peQL ends of a run (MH 4.5, H6): the bridge and the job document's datafusion and peql
  # kinds, which a default-feature build refuses by kind and never compiles.
  step "cargo clippy -D warnings --features peql (the governed ends of a run)"
  quiet clippy_peql cargo clippy -p moruna-datafusion -p moruna-runtime --all-targets --features moruna-datafusion/peql,moruna-runtime/peql -- -D warnings
  step "cargo test --features peql (H6: a governed plan is a source, a contract write a sink)"
  quiet test_peql cargo test -p moruna-runtime --features peql
  # A host that embeds peQL with `iceberg` unifies that feature into this build, so the governed
  # ends must compile and pass with it on (0.4.1).
  step "cargo clippy and test --features peql-iceberg (peQL's Iceberg binding compiled in)"
  quiet clippy_peql_iceberg cargo clippy -p moruna-datafusion -p moruna-runtime --all-targets \
    --features moruna-datafusion/peql-iceberg,moruna-runtime/peql -- -D warnings
  quiet test_peql_iceberg cargo test -p moruna-datafusion --features moruna-datafusion/peql-iceberg
  if [ -n "${MORUNA_PYTHON:-}" ]; then
    # Rust's embedded-interpreter tests start without Python's executable having loaded
    # libpython. A uv-managed interpreter's library directory is outside ld.so's defaults.
    # Keep the configured interpreter's library ahead of any inherited search path.
    if [ "$(uname -s)" = "Linux" ]; then
      python_library_dir="$("$MORUNA_PYTHON" -c 'import sysconfig; print(sysconfig.get_config_var("LIBDIR") or "")')" \
        || fail "cannot locate MORUNA_PYTHON's shared-library directory"
      [ -n "$python_library_dir" ] && [ -d "$python_library_dir" ] \
        || fail "MORUNA_PYTHON reports no shared-library directory"
      export LD_LIBRARY_PATH="$python_library_dir${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    fi
    # The adapter's fixtures import pyarrow and numpy inside the embedded interpreter, so
    # MORUNA_PYTHON has to be able to find both. A uv-managed interpreter is externally managed
    # and cannot have them installed into it, so the usual answer is a venv on that same
    # interpreter with PYTHONPATH naming its site-packages:
    #
    #   uv venv --python "$MORUNA_PYTHON" /tmp/moruna-py && \
    #     uv pip install --python /tmp/moruna-py numpy pyarrow && \
    #     export PYTHONPATH="$(/tmp/moruna-py/bin/python -c \
    #       'import sysconfig;print(sysconfig.get_paths()["purelib"])')"
    #
    # Checked here rather than left to the tests: without it seven crossing-rule tests fail on
    # `ModuleNotFoundError` apiece, which reads as a broken adapter and is a missing module
    # (found 2026-09-23).
    if missing="$(PYTHONPATH="${PYTHONPATH:-}" "$MORUNA_PYTHON" -c '
import importlib.util, sys
absent = [m for m in ("pyarrow", "numpy") if importlib.util.find_spec(m) is None]
sys.stdout.write(", ".join(absent))
' 2>/dev/null)" && [ -n "$missing" ]; then
      fail "MORUNA_PYTHON ($MORUNA_PYTHON) cannot import: $missing; put them on PYTHONPATH (see the comment above this check in tools/quality/check.sh)"
    fi
    step "cargo test --features python (interpreter: ${MORUNA_PYTHON})"
    PYO3_PYTHON="$MORUNA_PYTHON" quiet test_python cargo test -p moruna-adapters --features python
    # A user's `moruna.Source` and `moruna.Sink` (07 e.6, 08 f.10, 2026-09-29): their bridges
    # compile only with `python`, so a default-feature run neither tests nor measures them. The
    # SO-T17..22 and SI-T18..22 tests run here, and the two crates' coverage with the feature on
    # is judged by the same per-crate rule as the workspace's, below.
    step "cargo test --features python (moruna-sources, moruna-sinks: the user's Source and Sink)"
    PYO3_PYTHON="$MORUNA_PYTHON" quiet test_python_ends cargo test -p moruna-sources -p moruna-sinks \
      --features moruna-sources/python,moruna-sinks/python
  else
    step "python adapter not measured: set MORUNA_PYTHON to a CPython 3.14t with pyarrow"
  fi
  step "tests: $(grep -hE '^test result:' target/quality/test.log 2>/dev/null | awk '{p+=$4; f+=$6; i+=$8} END{print p" passed, "f" failed, "i" ignored"}')"

else
  step "no Cargo.toml at the repository root; Rust checks skipped (wave 0 creates the workspace)"
fi

if [ -d python/tests ]; then
  # The package's own tests. This used to be gated on python/pyproject.toml, which lives
  # at the repository root, so 32 tests including the one guarding the surface's
  # throughput were run by nothing at all: that is the structural reason a wheel whose
  # first user program hung for ever passed a green gate (found 2026-09-23).
  if [ -n "${MORUNA_PYTHON:-}" ]; then
    step "python/tests on ${MORUNA_PYTHON}"
    # A stale python/moruna/_core*.so shadows an installed wheel, so the suite must run
    # against a build made now, not whatever was left in the tree.
    quiet python_tests env MORUNA_PYTHON="$MORUNA_PYTHON" tools/quality/python_tests.sh
  else
    step "python/tests not run: set MORUNA_PYTHON to a CPython 3.14t with pyarrow"
  fi
fi

if [ -f python/pyproject.toml ]; then
  step "ruff and pytest with coverage >= ${MIN}%"
  command -v ruff >/dev/null 2>&1 || fail "ruff is not installed"
  ruff check python
  ( cd python && python3 -m pytest -q --cov=moruna --cov-fail-under="$MIN" )
fi

# Run the coverage floor after package validation so both produce release evidence.
if [ -f Cargo.toml ]; then
  (
    # The confined backend exits without atexit handlers. Keep instrumented
    # counters mapped before confinement so its coverage survives _exit too.
    if [ "$(uname -s)" = "Linux" ]; then
      coverage_rustflags="${RUSTFLAGS:-$(python3 -c 'import shlex, tomllib; print(shlex.join(tomllib.load(open(".cargo/config.toml", "rb"))["build"]["rustflags"]))')}"
      export RUSTFLAGS="$coverage_rustflags -Cllvm-args=-runtime-counter-relocation"
      export LLVM_PROFILE_FILE_NAME="moruna-%p-%m-%c.profraw"
      # The strict raw adapter needs a real block device: a loop device over a scratch
      # file, owned by this user for the run, so its tests measure actual direct IO.
      if [ -z "${MORUNA_RAW_DEVICE:-}" ] && command -v losetup >/dev/null 2>&1; then
        as_root=""
        [ "$(id -u)" = 0 ] || as_root="sudo -n"
        raw_image="$(mktemp "${TMPDIR:-/tmp}/moruna-raw.XXXXXX")"
        truncate -s 64M "$raw_image"
        if raw_dev="$($as_root losetup -f --show "$raw_image" 2>/dev/null)"; then
          [ -z "$as_root" ] || $as_root chown "$(id -u)" "$raw_dev"
          export MORUNA_RAW_DEVICE="$raw_dev"
          trap '$as_root losetup -d "$raw_dev" 2>/dev/null; rm -f "$raw_image"' EXIT
        else
          rm -f "$raw_image"
        fi
      fi
    fi
    step "line coverage >= ${MIN}% per crate (cargo-llvm-cov, test code excluded)"
    command -v cargo-llvm-cov >/dev/null 2>&1 \
      || fail "cargo-llvm-cov is not installed: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview"
    mkdir -p target/llvm-cov
    # A workspace in which no crate has an instrumented line (every member a
    # stub, wave 0 before F0.2 lands) yields no profile at all, and llvm-cov
    # reports "no coverage data found" instead of an empty summary. That is the
    # "stub, not measured" case of preamble 6.7, not a failure; any other error
    # from cargo-llvm-cov still fails the gate.
    # The confined-child paths (sandbox, the image kind) are exercised only by their
    # privileged tests. When this host gives them a delegated cgroup and a standard CPython
    # 3.14, build that interpreter's wheel and run those tests into the same profile set,
    # so the floor measures the code they actually run.
    privileged=""
    if [ "$(uname -s)" = "Linux" ] && [ -n "${MORUNA_SANDBOX_CGROUP:-}" ] \
        && [ -z "${MORUNA_SANDBOX_PYTHON:-}" ] && [ -n "${STANDARD:-}" ]; then
      step "standard CPython 3.14 with this wheel, for the sandbox's privileged tests"
      sandbox_env="$PWD/target/quality/sandbox-venv"
      rm -rf "$sandbox_env" target/quality/sandbox-wheel
      uv venv --quiet --python "$STANDARD" "$sandbox_env"
      quiet sandbox_wheel uv tool run --from "maturin>=1.15,<2.0" maturin build --release \
        -i "$sandbox_env/bin/python" -o target/quality/sandbox-wheel
      uv pip install --quiet --python "$sandbox_env/bin/python" target/quality/sandbox-wheel/*.whl pyarrow numpy
      export MORUNA_SANDBOX_PYTHON="$sandbox_env/bin/python"
    fi
    if [ -n "${MORUNA_SANDBOX_PYTHON:-}" ] && [ -n "${MORUNA_SANDBOX_CGROUP:-}" ]; then
      privileged=1
    fi
    # Run the privileged test targets through cargo-llvm-cov itself, so they are the same
    # instrumented build and their profiles join the workspace's. Only the test binary runs
    # as root (moving a child into the delegated cgroup needs it), through cargo's runner;
    # its profiles are handed back to this user before the report reads them.
    run_privileged() (
      if [ "$(id -u)" != 0 ]; then
        triple="$(rustc -vV | sed -n 's/^host: //p' | tr 'a-z-' 'A-Z_')"
        export "CARGO_TARGET_${triple}_RUNNER=sudo -n -E env PATH=$PATH"
      fi
      status=0
      cargo llvm-cov --no-report -p moruna-sandbox --test sandbox -- --ignored --test-threads=1 \
        || status=1
      cargo llvm-cov --no-report -p moruna-image --test image_kind -- --ignored --test-threads=1 \
        || status=1
      # The launcher's real QEMU boots (software TCG, no KVM needed), inside its jail.
      if [ -n "${MORUNA_QEMU:-}" ] && [ -n "${MORUNA_JAIL_CGROUP:-}" ]; then
        cargo llvm-cov --no-report -p moruna-vmm --features native-tcg-proof --test jail_tcg_boot \
          -- --ignored --test-threads=1 || status=1
      fi
      [ "$(id -u)" = 0 ] || sudo -n chown -R "$(id -u):$(id -g)" target
      return $status
    )
    cargo llvm-cov clean --workspace >/dev/null 2>&1 || true
    if cargo llvm-cov --workspace --no-report >target/llvm-cov/test.log 2>target/llvm-cov/stderr.log \
        && { [ -z "$privileged" ] || {
          step "privileged sandbox and image-kind tests, measured"
          run_privileged >>target/llvm-cov/test.log 2>>target/llvm-cov/stderr.log; }; } \
        && cargo llvm-cov report --json --summary-only --output-path target/llvm-cov/summary.json \
          >/dev/null 2>>target/llvm-cov/stderr.log; then
      coverage_ok=1
    else
      coverage_ok=""
    fi
    if [ -z "$coverage_ok" ]; then
      if grep -q "no coverage data found" target/llvm-cov/stderr.log; then
        step "no instrumented lines in any crate (all stubs); coverage not measured"
      else
        cat target/llvm-cov/stderr.log >&2
        fail "cargo llvm-cov failed"
      fi
    else
      cargo llvm-cov report --json --output-path target/llvm-cov/report.json \
        >/dev/null 2>target/llvm-cov/report-stderr.log \
        || { cat target/llvm-cov/report-stderr.log >&2; fail "cargo llvm-cov report failed"; }
      python3 tools/quality/coverage_gate.py target/llvm-cov/summary.json "$MIN"
    fi
    if [ -n "${MORUNA_PYTHON:-}" ]; then
      step "line coverage >= ${MIN}% for moruna-sources and moruna-sinks with --features python"
      PYO3_PYTHON="$MORUNA_PYTHON" cargo llvm-cov -p moruna-sources -p moruna-sinks \
        --features moruna-sources/python,moruna-sinks/python --json --summary-only \
        --output-path target/llvm-cov/summary-python.json >/dev/null 2>target/llvm-cov/stderr-python.log \
        || { cat target/llvm-cov/stderr-python.log >&2; fail "cargo llvm-cov --features python failed"; }
      python3 tools/quality/coverage_gate.py target/llvm-cov/summary-python.json "$MIN"
    fi
  )
fi

step "OK"
# What this gate cannot tell you, said out loud rather than assumed: it compiles and
# runs for the host it is on. Moruna's target is Linux (cgroup v2, O_DIRECT, io_uring),
# and a developer host here is macOS, so a Linux-only branch behind a cfg is never
# compiled locally and its errors appear only in CI. On 2026-09-23 exactly that
# happened: a test helper's Linux arm used libc without the dev-dependency, the gate
# was green for days and CI was red. Cross-checking needs a Linux linker that is not
# installed here, so after a push to main, read the CI run.
if [ "$(uname -s)" != "Linux" ]; then
  step "note: this host is $(uname -s); Linux-only code paths are compiled in CI, not here"
fi
