#!/usr/bin/env bash
# Point this clone's git hooks at tools/hooks. Run once per clone, before the
# first commit. Idempotent.
#   pre-commit  tools/quality/check.sh --fast: formatting, clippy, lints (seconds)
#   pre-push    tools/quality/check.sh: tests, 90% coverage, Python suite (minutes)
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
chmod +x tools/hooks/pre-commit tools/hooks/pre-push tools/quality/check.sh tools/quality/coverage_gate.py
git config core.hooksPath tools/hooks
echo "hooks: core.hooksPath = $(git config core.hooksPath)"
echo "hooks: pre-commit runs the fast checks, pre-push runs the whole gate"
