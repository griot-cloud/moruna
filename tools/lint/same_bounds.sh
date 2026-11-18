#!/usr/bin/env bash
# The bounds that the engine (root workspace) and the node (kernel/ workspace, which the root
# excludes) each hold a copy of, because neither can link the other: each pair must be equal.
#
# - A job's socket budget (system design 3D): the node mints a job's channel with
#   `egress::SOCKETS`, and the engine's connector keeps its pool within `moruna_storage::net::SOCKETS`.
#
# Usage: tools/lint/same_bounds.sh. Run by tools/quality/check.sh.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

value() { # FILE NAME: the integer a `pub const NAME: ... = N;` line in FILE gives
  sed -n "s/^pub const $2: [a-z0-9]* = \([0-9_]*\);.*/\1/p" "$root/$1" | tr -d _
}

fail=0
same() { # WHAT FILE_A FILE_B NAME
  local a b
  a="$(value "$2" "$4")"; b="$(value "$3" "$4")"
  if [ -z "$a" ] || [ -z "$b" ]; then
    echo "same_bounds: $1: $4 not found in $2 or $3" >&2; fail=1
  elif [ "$a" != "$b" ]; then
    echo "same_bounds: $1: $2 says $a, $3 says $b" >&2; fail=1
  fi
}

same "a job's socket budget (3D)" crates/moruna-storage/src/net.rs kernel/node/core/src/egress.rs SOCKETS
exit "$fail"
