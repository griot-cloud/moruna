#!/usr/bin/env bash
# SV-T15 (SDD 23, k): the extensibility rule, as a check. A commit that adds or
# changes anything under kernel/examples/ touches no file of the kernel or its ABI
# (kernel/core, kernel/abi, kernel/sys, kernel/handles, kernel/loader, kernel/boot): a
# server is added by writing a process against moruna-svc and listing it in the boot
# description, and nothing in the kernel is rebuilt.
#
#   kernel/tools/check-examples.sh BASE HEAD
set -euo pipefail

base="$1"
head="$2"
fail=0
seen=0
for commit in $(git rev-list --no-merges "$base..$head"); do
  files="$(git diff-tree --no-commit-id --name-only -r "$commit")"
  if grep -q '^kernel/examples/' <<<"$files"; then
    seen=$((seen + 1))
    subject="$(git log -1 --format=%s "$commit")"
    if kernel_files="$(grep -E '^kernel/(core|abi|sys|handles|loader|boot)/' <<<"$files")"; then
      echo "FAIL $commit ($subject) changes an example and the kernel:"
      echo "$kernel_files"
      fail=1
    else
      echo "pass $commit ($subject): the example touches no kernel file"
    fi
  fi
done
echo "check-examples: $seen commit(s) in $base..$head touch kernel/examples"
exit "$fail"
