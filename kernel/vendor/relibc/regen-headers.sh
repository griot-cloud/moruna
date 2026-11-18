#!/usr/bin/env bash
# relibc's C headers for Moruna (item 5.2b), generated as relibc's own build generates them, by
# cbindgen 0.29.4 from each of its header modules (src/header/NAME, with its cbindgen.toml and
# cbindgen.globdefs.toml; not its math, which openlibm's replaces), and committed in
# include-generated/ so that a build takes them as they are and needs no tool of its own
# (kernel/function/c/build-core.sh). include-generated/provenance.json records the tool, the
# relibc commit the vendored tree is (with Moruna's changes, NOTICE.md) and each header's hash.
#
#   kernel/vendor/relibc/regen-headers.sh           regenerates them and fails if they differ
#                                                   from include-generated/
#   kernel/vendor/relibc/regen-headers.sh --write   regenerates them into include-generated/
#
# Run it when relibc's pin moves, or Moruna's changes reach a header. CBINDGEN names cbindgen,
# else it is the one on PATH; it must be 0.29.4.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
version=0.29.4
cbindgen="${CBINDGEN:-cbindgen}"
write=false
case "${1:-}" in
  "") ;;
  --write) write=true ;;
  *)
    echo "usage: regen-headers.sh [--write]" >&2
    exit 2
    ;;
esac
command -v "$cbindgen" >/dev/null || {
  echo "regen-headers.sh: no cbindgen: set CBINDGEN to cbindgen $version" >&2
  exit 1
}
if [ "$("$cbindgen" --version)" != "cbindgen $version" ]; then
  echo "regen-headers.sh: $("$cbindgen" --version) is not cbindgen $version" >&2
  exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
generated="$tmp/include-generated"
mkdir -p "$generated"
for dir in "$here/src/header/"*/; do
  name="$(basename "$dir")"
  case "$name" in _* | math) continue ;; esac
  [ -f "$dir/cbindgen.toml" ] || continue
  header="$generated/$(echo "$name" | tr _ /).h"
  mkdir -p "$(dirname "$header")"
  (cd "$here" && cat "$dir/cbindgen.toml" cbindgen.globdefs.toml |
    "$cbindgen" "src/header/$name/mod.rs" --config=/dev/stdin --output "$header" --quiet)
done

python3 -I - "$here" "$generated" "$version" <<'PY'
import hashlib, json, pathlib, sys
here, generated, version = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]), sys.argv[3]
relibc = next(u for u in json.loads((here / "provenance.json").read_text())["upstreams"]
              if u["name"] == "relibc")
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
record = {
    "tool": f"cbindgen {version}",
    "relibc": {"url": relibc["url"], "commit": relibc["commit"],
               "tree": "kernel/vendor/relibc, with Moruna's changes (NOTICE.md)"},
    "inputs": "src/header/NAME/mod.rs with src/header/NAME/cbindgen.toml and "
              "cbindgen.globdefs.toml, for each header module but _* and math",
    "globdefs_sha256": sha(here / "cbindgen.globdefs.toml"),
    "headers": {p.relative_to(generated).as_posix(): sha(p)
                for p in sorted(generated.rglob("*.h"))},
}
(generated / "provenance.json").write_text(json.dumps(record, indent=2) + "\n")
print(f"{len(record['headers'])} headers")
PY

if "$write"; then
  rm -rf "$here/include-generated"
  cp -R "$generated" "$here/include-generated"
  echo "regen-headers.sh: wrote $here/include-generated"
elif ! diff -r "$generated" "$here/include-generated" >"$tmp/diff"; then
  head -40 "$tmp/diff" >&2
  echo "regen-headers.sh: the headers differ from include-generated/: run it with --write and commit them" >&2
  exit 1
else
  echo "regen-headers.sh: include-generated/ is what cbindgen $version makes"
fi
