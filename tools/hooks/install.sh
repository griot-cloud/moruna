#!/usr/bin/env bash
# Point this clone's git hooks at tools/hooks. Run once per clone, before the
# first commit (preamble 6.7). Idempotent.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
chmod +x tools/hooks/pre-commit tools/quality/check.sh tools/quality/coverage_gate.py
git config core.hooksPath tools/hooks
echo "hooks: core.hooksPath = $(git config core.hooksPath); pre-commit runs tools/quality/check.sh"
