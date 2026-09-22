#!/usr/bin/env bash
# Nothing in a shipping crate's src/ may be a stub.
#
# A stub is code that compiles and does not do the thing: todo!(), unimplemented!(),
# a panic whose message admits it, a function that returns a default with a comment
# saying it will be written later. They are how a project looks finished and is not,
# and a coverage gate does not catch them, because an untested stub is simply
# uncovered and an unreachable one is covered by the test that asserts it panics.
#
# Test code is exempt: a test tagged "(reference host, E1)" or "(integration, closes
# in wave N)" is required by the preamble to exist and not run, and unimplemented!()
# with the tag's reason is how that is written.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

fail=0
scan() {
  local pattern="$1" what="$2"
  # src/ of every crate, plus the Python package; never tests/, benches/ or bench/.
  local hits
  hits="$(git grep -n -E "$pattern" -- 'crates/*/src/**' 'python/**' 2>/dev/null || true)"
  if [ -n "$hits" ]; then
    printf 'no_stubs: %s\n' "$what" >&2
    printf '%s\n' "$hits" >&2
    fail=1
  fi
}

scan '\btodo!\(' 'todo!() in shipping code'
scan '\bunimplemented!\(' 'unimplemented!() in shipping code'
scan 'panic!\("[^"]*(not implemented|unimplemented|not yet|TODO|stub)' 'a panic that admits it is a stub'
scan 'raise NotImplementedError' 'NotImplementedError in the Python package'
scan '(FIXME|XXX)' 'a FIXME or XXX marker'

# A crate that is a member of the workspace and has no code is a stub crate. Wave 0
# created them deliberately; by the time a crate is claimed done it must have some.
while IFS= read -r toml; do
  crate="$(dirname "$toml")"
  name="$(basename "$crate")"
  lines="$(find "$crate/src" -name '*.rs' -exec cat {} + 2>/dev/null | grep -vcE '^\s*(//|$)' || true)"
  if [ "${lines:-0}" -lt 50 ]; then
    if [ -n "${AMORU_STUB_CRATES_OK:-}" ] && printf '%s' "$AMORU_STUB_CRATES_OK" | tr ',' '\n' | grep -qx "$name"; then
      printf 'no_stubs: %s is an empty crate, allowed for now by AMORU_STUB_CRATES_OK\n' "$name"
    else
      printf 'no_stubs: %s has %s lines of code, which is a stub crate\n' "$name" "${lines:-0}" >&2
      fail=1
    fi
  fi
done < <(find crates -maxdepth 3 -name Cargo.toml | sort)

if [ "$fail" = "1" ]; then
  echo "no_stubs: FAIL" >&2
  exit 1
fi
echo "no_stubs: ok (no stub in any crate's src/ or the Python package)"
