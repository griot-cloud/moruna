#!/usr/bin/env bash
# Amoru quality gate. Run by the pre-commit hook (tools/hooks/pre-commit) and by
# the first CI job (preamble 6.6). It fails on: an em dash in any tracked text
# file; cargo fmt drift; a clippy warning; a wildcard arm over Tier or
# StagingCodec (tools/lint/no_tier_wildcard.sh, CT-T14); a failing test; line
# coverage below AMORU_COVERAGE_MIN (default 90) in any workspace crate that has
# instrumented lines (a stub crate with no code is not measured). Python checks
# run once python/pyproject.toml exists (wave 5).
set -euo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
MIN="${AMORU_COVERAGE_MIN:-90}"

fail() { printf 'quality: FAIL: %s\n' "$*" >&2; exit 1; }
step() { printf 'quality: %s\n' "$*"; }

step "em dashes"
EM="$(printf '\xe2\x80\x94')"
if git ls-files -z | xargs -0 grep -In -e "$EM" -- 2>/dev/null; then
  fail "em dash found; use a comma, a colon or parentheses (architecture/README.md, conventions)"
fi

if [ -f Cargo.toml ]; then
  step "cargo fmt --check"
  cargo fmt --all -- --check
  step "cargo clippy -D warnings"
  cargo clippy --workspace --all-targets -- -D warnings
  if [ -x tools/lint/no_tier_wildcard.sh ]; then
    step "tools/lint/no_tier_wildcard.sh"
    tools/lint/no_tier_wildcard.sh
  else
    step "tools/lint/no_tier_wildcard.sh not present yet (wave 0 deliverable); skipped"
  fi
  step "cargo test"
  cargo test --workspace
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
