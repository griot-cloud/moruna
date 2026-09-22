#!/usr/bin/env bash
# Amoru quality gate. Run by the pre-commit hook (tools/hooks/pre-commit) and by
# the first CI job (preamble 6.6). It fails on: an em dash in any tracked text
# file; cargo fmt drift; a clippy warning; a wildcard arm over Tier or
# StagingCodec (tools/lint/no_tier_wildcard.sh, CT-T14); a broken link, a tab,
# an unlisted page or a placeholder with no citation under docs/
# (tools/docs/check_docs.py, F7.1); a commit-message rule the DCO self-test
# rejects (tools/quality/check_dco.sh, F6.5); a supply-chain rule of deny.toml
# when cargo-deny is installed (F6.5); a failing test; line
# coverage below AMORU_COVERAGE_MIN (default 90) in any workspace crate that has
# instrumented lines (a stub crate with no code is not measured). Python checks
# run once python/pyproject.toml exists (wave 5).
set -euo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
MIN="${AMORU_COVERAGE_MIN:-90}"

fail() { printf 'quality: FAIL: %s\n' "$*" >&2; exit 1; }
step() { printf 'quality: %s\n' "$*"; }
# Run a step quietly: its output goes to target/quality/<name>.log and is shown
# only when the step fails (or when AMORU_QUALITY_VERBOSE=1), so a passing gate
# prints one line per step and a failing one prints the evidence.
quiet() {
  local name="$1"; shift
  mkdir -p target/quality
  if [ "${AMORU_QUALITY_VERBOSE:-0}" = "1" ]; then "$@"; return; fi
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
# The two crates still to be written are declared here by name, so that "done" cannot
# be claimed while either is empty and the list is visible to anyone reading the gate.
# Delete a name the day its crate has code; when the list is empty, delete the variable.
AMORU_STUB_CRATES_OK="amoru-py" quiet no_stubs tools/quality/no_stubs.sh

if [ -d docs ] && [ -x tools/docs/check_docs.py ]; then
  step "tools/docs/check_docs.py (docs conventions, links, SUMMARY, citations)"
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
  step "cargo test"
  quiet test cargo test --workspace
  # The feature-gated crates are invisible to a default-feature run: amoru-adapters,
  # amoru-polars and amoru-datafusion compile to nothing without `python`, `polars`
  # and `datafusion`, so the gate reported them as stubs and their coverage as "not
  # measured" while they held thousands of lines (found by the component 5 agent,
  # 2026-09-22). The engine bridges need no interpreter and are always built; the
  # Python adapter needs an interpreter with pyarrow, so it runs when one is
  # configured and says so plainly when it is not.
  step "cargo test --features polars,datafusion (the engine bridges)"
  quiet test_bridges cargo test -p amoru-polars -p amoru-datafusion --features amoru-polars/polars,amoru-datafusion/datafusion
  if [ -n "${AMORU_PYTHON:-}" ]; then
    step "cargo test --features python (interpreter: ${AMORU_PYTHON})"
    PYO3_PYTHON="$AMORU_PYTHON" quiet test_python cargo test -p amoru-adapters --features python
  else
    step "python adapter not measured: set AMORU_PYTHON to a CPython 3.14t with pyarrow"
  fi
  step "tests: $(grep -hE '^test result:' target/quality/test.log 2>/dev/null | awk '{p+=$4; f+=$6; i+=$8} END{print p" passed, "f" failed, "i" ignored"}')"
  step "line coverage >= ${MIN}% per crate (cargo-llvm-cov, test code excluded)"
  command -v cargo-llvm-cov >/dev/null 2>&1 \
    || fail "cargo-llvm-cov is not installed: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview"
  mkdir -p target/llvm-cov
  # A workspace in which no crate has an instrumented line (every member a
  # stub, wave 0 before F0.2 lands) yields no profile at all, and llvm-cov
  # reports "no coverage data found" instead of an empty summary. That is the
  # "stub, not measured" case of preamble 6.7, not a failure; any other error
  # from cargo-llvm-cov still fails the gate.
  if ! cargo llvm-cov --workspace --json --summary-only --output-path target/llvm-cov/summary.json \
      >/dev/null 2>target/llvm-cov/stderr.log; then
    if grep -q "no coverage data found" target/llvm-cov/stderr.log; then
      step "no instrumented lines in any crate (all stubs); coverage not measured"
    else
      cat target/llvm-cov/stderr.log >&2
      fail "cargo llvm-cov failed"
    fi
  else
    python3 tools/quality/coverage_gate.py target/llvm-cov/summary.json "$MIN"
  fi
else
  step "no Cargo.toml at the repository root; Rust checks skipped (wave 0 creates the workspace)"
fi

if [ -f python/pyproject.toml ]; then
  step "ruff and pytest with coverage >= ${MIN}%"
  command -v ruff >/dev/null 2>&1 || fail "ruff is not installed"
  ruff check python
  ( cd python && python3 -m pytest -q --cov=amoru --cov-fail-under="$MIN" )
fi

step "OK"
