#!/usr/bin/env python3
"""Writes pyarrow as the repository's lock pins it (uv.lock: its wheel for CPython 3.14 on
glibc x86_64 Linux), unpacked into OUT, for CI's Linux test image (build.sh): KC1's pyarrow
probe imports it through the Linux personality, as a Python function's image does (item 5.3).

    kernel/tools/pyarrow-layer.py OUT CACHE

The wheel is fetched from the URL the lock names into the directory CACHE, once, and checked
against the lock's SHA-256 every time it is used.
"""

import hashlib
import pathlib
import shutil
import sys
import tomllib
import urllib.request
import zipfile

SUFFIX = "-cp314-cp314-manylinux_2_28_x86_64.whl"


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: kernel/tools/pyarrow-layer.py OUT CACHE", file=sys.stderr)
        return 2
    out, cache = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
    root = pathlib.Path(__file__).resolve().parents[2]
    lock = tomllib.loads((root / "uv.lock").read_text())
    (package,) = [p for p in lock["package"] if p["name"] == "pyarrow"]
    (wheel,) = [w for w in package["wheels"] if w["url"].endswith(SUFFIX)]
    url, digest = wheel["url"], wheel["hash"].removeprefix("sha256:")
    cache.mkdir(parents=True, exist_ok=True)
    kept = cache / url.rsplit("/", 1)[1]
    if not kept.is_file() or hashlib.sha256(kept.read_bytes()).hexdigest() != digest:
        with urllib.request.urlopen(url, timeout=300) as response:
            data = response.read()
        if hashlib.sha256(data).hexdigest() != digest:
            print(f"pyarrow-layer.py: {url} is not the lock's sha256:{digest}", file=sys.stderr)
            return 1
        kept.write_bytes(data)
    shutil.rmtree(out, ignore_errors=True)
    with zipfile.ZipFile(kept) as archive:
        archive.extractall(out)
    print(f"pyarrow-layer.py: pyarrow {package['version']} ({kept.name}) is at {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
