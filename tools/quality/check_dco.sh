#!/usr/bin/env bash
# Developer Certificate of Origin check (board E6, feature F6.5; CONTRIBUTING.md
# "Licence" and "Agent identity and sign-off"; preamble 6.7).
#
# Two rules, applied to every non-merge commit a pull request adds:
#
#   1. The commit message carries a `Signed-off-by: Name <email>` line. This is
#      what `git commit -s` writes and what the DCO (https://developercertificate.org/)
#      is asserted with.
#   2. A commit authored by `Amoru Agent <agents@griotdata.com>` is signed off by
#      that same identity. An agent commit signed off by somebody else, or by an
#      agent identity with a different address, is the case CONTRIBUTING.md's
#      sign-off paragraph is about: the maintainer takes DCO responsibility for
#      what the agents commit, and that only holds if the identity on the commit
#      and the identity on the sign-off are the same one.
#
# Merge commits are skipped: a merge adds no authored content, and `git merge`
# writes no sign-off.
#
# Usage:
#   tools/quality/check_dco.sh BASE HEAD    check every commit in BASE..HEAD
#   tools/quality/check_dco.sh --self-test  run the rules over tools/quality/fixtures/
#
# CI runs it as `tools/quality/check_dco.sh "$BASE_SHA" "$HEAD_SHA"` on a pull
# request, and a human runs the same line locally, for example
#   tools/quality/check_dco.sh origin/main HEAD
set -euo pipefail

AGENT_NAME="Amoru Agent"
AGENT_EMAIL="agents@griotdata.com"
AGENT_IDENT="$AGENT_NAME <$AGENT_EMAIL>"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fixtures="$here/fixtures"

# trim leading and trailing whitespace from stdin
trim() { sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'; }

# check_one AUTHOR_IDENT MESSAGE_FILE
# Prints nothing and returns 0 when the message satisfies both rules; prints the
# reason on stderr and returns 1 when it does not.
check_one() {
  local author="$1" file="$2"
  local signoffs ident found_any=0 found_agent=0

  # A sign-off line is `Signed-off-by: Some Name <address>`, at the start of a
  # line, with a non-empty name and an address in angle brackets.
  signoffs="$(grep -E '^[[:space:]]*Signed-off-by:[[:space:]]*[^<>]+<[^<> ]+@[^<> ]+>[[:space:]]*$' "$file" || true)"

  if [ -n "$signoffs" ]; then
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      found_any=1
      ident="$(printf '%s' "${line#*Signed-off-by:}" | trim)"
      if [ "$ident" = "$AGENT_IDENT" ]; then found_agent=1; fi
    done <<EOF
$signoffs
EOF
  fi

  if [ "$found_any" -eq 0 ]; then
    echo "no valid 'Signed-off-by: Name <email>' line (commit with 'git commit -s'; CONTRIBUTING.md, Licence)" >&2
    return 1
  fi
  if [ "$author" = "$AGENT_IDENT" ] && [ "$found_agent" -eq 0 ]; then
    echo "authored by $AGENT_IDENT but not signed off by that identity (CONTRIBUTING.md, Agent identity and sign-off)" >&2
    return 1
  fi
  return 0
}

# A fixture is a commit message with one extra first line, `Author: Name <email>`,
# naming who the commit would have been authored by. `good_*` fixtures must be
# accepted and `bad_*` fixtures must be rejected, as in tools/lint/no_tier_wildcard.sh.
self_test() {
  local f author body good=0 bad=0
  body="$(mktemp)"
  trap 'rm -f "$body"' RETURN

  for f in "$fixtures"/bad_*.txt; do
    author="$(sed -n '1s/^Author:[[:space:]]*//p' "$f")"
    tail -n +2 "$f" > "$body"
    if check_one "$author" "$body" 2>/dev/null; then
      echo "check_dco: self-test FAILED: $(basename "$f") was accepted" >&2
      return 1
    fi
    bad=$((bad + 1))
  done

  for f in "$fixtures"/good_*.txt; do
    author="$(sed -n '1s/^Author:[[:space:]]*//p' "$f")"
    tail -n +2 "$f" > "$body"
    if ! check_one "$author" "$body"; then
      echo "check_dco: self-test FAILED: $(basename "$f") was rejected" >&2
      return 1
    fi
    good=$((good + 1))
  done

  echo "check_dco: self-test ok ($bad rejected, $good accepted)"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

if [ $# -ne 2 ]; then
  echo "usage: tools/quality/check_dco.sh BASE HEAD | --self-test" >&2
  exit 2
fi

base="$1"
head="$2"
body="$(mktemp)"
trap 'rm -f "$body"' EXIT

# A plain list and a `while read` loop rather than `mapfile`, so the script runs
# on the bash 3.2 that macOS ships as well as on the bash CI has.
commits="$(git rev-list --no-merges "$base..$head")"
if [ -z "$commits" ]; then
  echo "check_dco: no non-merge commits in $base..$head; nothing to check"
  exit 0
fi

total=0
failed=0
while IFS= read -r sha; do
  [ -n "$sha" ] || continue
  total=$((total + 1))
  author="$(git log -1 --format='%an <%ae>' "$sha")"
  git log -1 --format='%B' "$sha" > "$body"
  if check_one "$author" "$body"; then
    echo "check_dco: ok      $(git log -1 --format='%h %s' "$sha")  [$author]"
  else
    echo "check_dco: FAIL    $(git log -1 --format='%h %s' "$sha")  [$author]" >&2
    failed=$((failed + 1))
  fi
done <<EOF
$commits
EOF

if [ "$failed" -gt 0 ]; then
  echo "check_dco: $failed of $total commit(s) in $base..$head are not signed off correctly" >&2
  exit 1
fi
echo "check_dco: ok ($total commit(s) in $base..$head, every one signed off)"
