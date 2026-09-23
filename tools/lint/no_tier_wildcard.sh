#!/usr/bin/env bash
# Repository lint for contracts CT-I11 (test CT-T14): every `match` on a `Tier`
# or a `StagingCodec` names every variant; a `_ =>` arm is where the multi-node
# rework would hide. Run by tools/quality/check.sh and by CI.
#
# Usage:
#   tools/lint/no_tier_wildcard.sh             self-test on the fixtures, then scan crates/ and bench/
#   tools/lint/no_tier_wildcard.sh --self-test self-test only
#   tools/lint/no_tier_wildcard.sh FILE...     scan the given Rust files
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
scanner="$here/no_tier_wildcard.py"
fixtures="$here/fixtures"

self_test() {
  local bad ok
  # each bad fixture must be rejected, and the report must name it
  for bad in "$fixtures"/bad_*.rs; do
    if python3 "$scanner" "$bad" >/dev/null 2>&1; then
      echo "no_tier_wildcard: self-test FAILED: $bad was accepted" >&2
      return 1
    fi
  done
  # every good fixture must pass
  for ok in "$fixtures"/good_*.rs; do
    if ! python3 "$scanner" "$ok"; then
      echo "no_tier_wildcard: self-test FAILED: $ok was rejected" >&2
      return 1
    fi
  done
  echo "no_tier_wildcard: self-test ok ($(ls "$fixtures"/bad_*.rs | wc -l | tr -d ' ') rejected, $(ls "$fixtures"/good_*.rs | wc -l | tr -d ' ') accepted)"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

if [ $# -gt 0 ]; then
  python3 "$scanner" "$@"
  exit $?
fi

self_test
cd "$root"
files=()
while IFS= read -r -d '' f; do files+=("$f"); done < <(
  find crates bench -name '*.rs' -not -path '*/target/*' -print0 2>/dev/null
)
if [ ${#files[@]} -eq 0 ]; then
  echo "no_tier_wildcard: no Rust files to scan"
  exit 0
fi
if python3 "$scanner" "${files[@]}"; then
  echo "no_tier_wildcard: ok (${#files[@]} files, no wildcard arm over Tier or StagingCodec)"
else
  echo "no_tier_wildcard: FAIL (CT-I11: name every variant; Remote returns MorunaError::Unsupported(\"rdma\"))" >&2
  exit 1
fi
