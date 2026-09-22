#!/usr/bin/env bash
# Build the wheel and run python/tests against it, in a throwaway environment.
#
# Never against the tree: a gitignored python/amoru/_core*.so shadows an installed
# wheel for anything run from python/, so the suite would test whatever build was left
# behind rather than the one this commit produces.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
: "${AMORU_PYTHON:?set AMORU_PYTHON to a CPython 3.14t interpreter}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
uv run --python "$AMORU_PYTHON" --with maturin maturin build --release --out "$work/dist" >/dev/null
uv venv --python "$AMORU_PYTHON" "$work/venv" >/dev/null
uv pip install --python "$work/venv" --quiet "$work"/dist/*.whl pyarrow pytest
# Run from a copy of the tests so no stale extension module in python/ is importable.
cp -R python/tests "$work/tests"
( cd "$work" && "$work/venv/bin/python" -m pytest -q tests )
