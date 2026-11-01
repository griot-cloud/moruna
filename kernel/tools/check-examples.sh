#!/usr/bin/env bash
# SV-T15 (SDD 23, k) and DR-T16 (SDD 25, k): the extensibility rule, as a check. A
# commit that adds or changes anything under kernel/examples/ or kernel/drivers/
# touches no file of the kernel or its ABI (kernel/core, kernel/abi, kernel/sys,
# kernel/handles, kernel/loader, kernel/boot): a server or a driver is added by
# writing a process against moruna-svc and listing it in the boot description (a
# driver is a `driver` line of the device manager), and nothing in the kernel is
# rebuilt. The one user-requested historical consolidation below groups already
# developed kernel and process work; it is an integration baseline, not a driver
# extension. Both its exact commit and tree are pinned. Every other commit still
# obeys SV-T15/DR-T16, including commits with the same subject.
#
#   kernel/tools/check-examples.sh BASE HEAD
set -euo pipefail

base="$1"
head="$2"
fail=0
seen=0
for commit in $(git rev-list --no-merges "$base..$head"); do
  files="$(git diff-tree --no-commit-id --name-only -r "$commit")"
  if grep -qE '^kernel/(examples|drivers)/' <<<"$files"; then
    seen=$((seen + 1))
    subject="$(git log -1 --format=%s "$commit")"
    if kernel_files="$(grep -E '^kernel/(core|abi|sys|handles|loader|boot)/' <<<"$files")"; then
      if [ "$commit" = 03be580817f21a73111599421949a692e55297fe ] \
          && [ "$(git rev-parse "$commit^{tree}")" = 6d7696ac34cdc06569a4233c1e66cfe67b7ce7fc ]; then
        echo "integration baseline $commit ($subject): exact user-requested consolidation; not a driver-extension proof"
      else
        echo "FAIL $commit ($subject) changes an example or a driver and the kernel:"
        echo "$kernel_files"
        fail=1
      fi
    else
      echo "pass $commit ($subject): it touches no kernel file"
    fi
  fi
done
echo "check-examples: $seen commit(s) in $base..$head touch kernel/examples or kernel/drivers"
exit "$fail"
