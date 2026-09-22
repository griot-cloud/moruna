#!/usr/bin/env bash
# Remove the worktree an executor finished with, and its build tree.
#
# Several executors build at once, each with its own target directory, and a
# dozen crates linking arrow and parquet fill a disk faster than anyone expects:
# on 2026-09-22 ten trees reached 90 GB, filled the machine, and made three
# components' tests fail in ways that looked like code defects and were not.
# So an agent's tree goes as soon as its branch is merged or abandoned.
#
# Usage:
#   tools/worktree-prune.sh <path>...     remove these worktrees
#   tools/worktree-prune.sh --merged      remove every worktree whose branch is
#                                         merged into main (the usual case)
#   tools/worktree-prune.sh --list        show what exists and what it costs
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

list() {
  printf '%-58s %-28s %8s\n' WORKTREE BRANCH SIZE
  git worktree list --porcelain | awk '/^worktree /{w=$2} /^branch /{b=$2; print w, b}' | while read -r w b; do
    [ "$w" = "$(git rev-parse --show-toplevel)" ] && continue
    printf '%-58s %-28s %8s\n' "${w##*/}" "${b#refs/heads/}" "$(du -sh "$w" 2>/dev/null | cut -f1)"
  done
}

remove() {
  local w="$1"
  [ -d "$w" ] || { echo "not a directory: $w" >&2; return 0; }
  if [ -n "$(git -C "$w" status --porcelain 2>/dev/null)" ]; then
    echo "REFUSED, uncommitted changes: $w" >&2
    return 1
  fi
  local size; size="$(du -sh "$w" 2>/dev/null | cut -f1)"
  git worktree remove --force "$w"
  echo "removed $w ($size)"
}

case "${1:---list}" in
  --list) list ;;
  --merged)
    git fetch -q origin main 2>/dev/null || true
    git worktree list --porcelain | awk '/^worktree /{w=$2} /^branch /{print w, $2}' | while read -r w b; do
      [ "$w" = "$(git rev-parse --show-toplevel)" ] && continue
      br="${b#refs/heads/}"
      own="$(git rev-list --count "$(git merge-base main "$br")".."$br" 2>/dev/null || echo 0)"
      if [ "$own" = "0" ]; then
        # No commits of its own: the branch was cut and nothing has landed on it,
        # which is "not started", not "merged", however far main has moved since.
        # An executor is probably working in it right now.
        echo "kept $w ($br has no commits of its own yet)"
      elif git merge-base --is-ancestor "$br" main 2>/dev/null; then
        remove "$w" || true
      else
        echo "kept $w ($br is not merged into main)"
      fi
    done
    git worktree prune
    df -h . | tail -1
    ;;
  *)
    for w in "$@"; do remove "$w" || true; done
    git worktree prune
    df -h . | tail -1
    ;;
esac
