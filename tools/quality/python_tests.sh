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
uv run --python "$MORUNA_PYTHON" --with maturin maturin build --release --out "$work/dist" >/dev/null
uv venv --python "$MORUNA_PYTHON" "$work/venv" >/dev/null
uv pip install --python "$work/venv" --quiet "$work"/dist/*.whl pyarrow pytest numpy pyyaml pip
# Run from a copy of the tests so no stale extension module in python/ is importable. One
# file per process: the suite's files keep the data they made, and one process holding
# them all reaches about 6 GB, which the runs' budgets then refuse (not a leak: a run moving
# 1.9 GB leaves 71 MiB held). Every file runs, and every failing file is named.
cp -R python/tests "$work/tests"
failed=()
for file in "$work"/tests/test_*.py; do
  if ! ( cd "$work" && "$work/venv/bin/python" -m pytest -q "tests/$(basename "$file")" ); then
    failed+=("$(basename "$file")")
  fi
done
if [ "${#failed[@]}" -gt 0 ]; then
  echo "python tests failed: ${failed[*]}" >&2
  exit 1
fi
