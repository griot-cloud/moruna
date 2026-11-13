#!/usr/bin/env bash
# Point this clone's git hooks at tools/hooks. Run once per clone, before the
# first commit. Idempotent.
#   pre-commit  tools/quality/check.sh --fast: formatting, clippy, lints (seconds)
#   pre-push    refuses a push while CI runs for the branch, then check.sh --fast
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
chmod +x tools/hooks/pre-commit tools/hooks/pre-push tools/hooks/no-push-during-ci tools/quality/check.sh tools/quality/coverage_gate.py
git config core.hooksPath tools/hooks
echo "hooks: core.hooksPath = $(git config core.hooksPath)"
echo "hooks: pre-commit runs the fast checks; pre-push refuses while CI runs, then the fast checks"
