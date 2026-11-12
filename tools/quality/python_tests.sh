#!/usr/bin/env bash
# Build the wheel and run python/tests against it, in a throwaway environment.
#
# Never against the tree: a gitignored python/moruna/_core*.so shadows an installed
# wheel for anything run from python/, so the suite would test whatever build was left
# behind rather than the one this commit produces.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
: "${MORUNA_PYTHON:?set MORUNA_PYTHON to a CPython 3.14t interpreter}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
# `uv tool run`, not `uv run`: inside the project `uv run` first builds and installs the
# project into .venv, a whole release build thrown away before maturin builds the wheel again.
uv tool run --from "maturin>=1.15,<2.0" maturin build --release -i "$MORUNA_PYTHON" --out "$work/dist" >/dev/null
uv venv --python "$MORUNA_PYTHON" "$work/venv" >/dev/null
uv pip install --python "$work/venv" --quiet "$work"/dist/*.whl pyarrow pytest numpy pyyaml pip uv
# Run from a copy of the tests so no stale extension module in python/ is importable.
cp -R python/tests "$work/tests"
# One process per test file: a run's budget counts what its process already holds, and the
# files keep their own data alive, so a single process for the whole suite refuses later runs.
#
# A hung test must fail, not wait for the CI job's own limit: each file gets
# MORUNA_PYTEST_FILE_TIMEOUT seconds (default 900; the whole suite took 31 minutes in CI),
# and a test still running after 300 seconds has every thread's stack dumped, so the log
# says where it is stuck. `timeout` signals the file's whole process group.
limit="${MORUNA_PYTEST_FILE_TIMEOUT:-900}"
bounded=()
command -v timeout >/dev/null 2>&1 && bounded=(timeout --kill-after=30 "$limit")
failed=""
for file in "$work"/tests/test_*.py; do
  name="$(basename "$file")"
  started=$SECONDS
  status=0
  ( cd "$work" && ${bounded[@]+"${bounded[@]}"} "$work/venv/bin/python" -m pytest -q \
      -o faulthandler_timeout=300 "tests/$name" ) || status=$?
  if [ "$status" = 124 ] || [ "$status" = 137 ]; then
    echo "python/tests: $name timed out after ${limit}s" >&2
    failed="$failed $name(timed out)"
  elif [ "$status" != 0 ]; then
    failed="$failed $name"
  fi
  echo "python/tests: $name $((SECONDS - started))s, exit $status"
done
if [ -n "$failed" ]; then echo "python/tests failed in:$failed" >&2; exit 1; fi
