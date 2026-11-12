#!/usr/bin/env python3
"""Per-crate line coverage gate over a cargo-llvm-cov JSON summary.

Usage: coverage_gate.py <summary.json> <min_percent>

Every workspace crate under crates/<name>/ that has at least one instrumented
line must reach <min_percent> line coverage. Crates with no instrumented lines
(compiling stubs) are listed and not judged. Files outside crates/ (the bench
runner, tools) are reported under their top-level directory and judged too.

A crate in EXEMPT is still measured and printed, but not judged: its code runs only
where no host test can reach it, and another required check proves it instead.
"""
import json
import sys
from collections import defaultdict

# Decided 2026-11-12 (release 0.6.5+0.6.6): moruna-micro is the engine's binding to the
# Moruna microkernel. Every call it makes is a kernel call that exists only inside the
# native guest, so host tests reach only its "not on the kernel" answers. The required
# native boot job runs all production cases through it on the real kernel instead. Remove
# this entry once guest-side coverage profiles are collected and merged.
EXEMPT = {
    "moruna-micro": "guest-only kernel calls; proven by the native boot job, not host lines",
}


def crate_of(path: str) -> str:
    parts = path.replace("\\", "/").split("/")
    if "crates" in parts:
        i = parts.index("crates")
        if i + 1 < len(parts):
            return parts[i + 1]
    # a file outside crates/: use the first path component under the repo root
    for marker in ("bench", "tools", "python"):
        if marker in parts:
            return marker
    return "(other)"


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    summary_path, minimum = sys.argv[1], float(sys.argv[2])
    with open(summary_path, encoding="utf-8") as fh:
        data = json.load(fh)
    counts = defaultdict(lambda: [0, 0])  # crate -> [covered, total]
    for export in data.get("data", []):
        for entry in export.get("files", []):
            lines = entry.get("summary", {}).get("lines", {})
            crate = crate_of(entry.get("filename", ""))
            counts[crate][0] += int(lines.get("covered", 0))
            counts[crate][1] += int(lines.get("count", 0))
    failed = []
    print(f"{'crate':<22}{'lines':>8}{'covered':>9}{'percent':>9}  verdict")
    for crate in sorted(counts):
        covered, total = counts[crate]
        if total == 0:
            print(f"{crate:<22}{total:>8}{covered:>9}{'n/a':>9}  stub, not measured")
            continue
        pct = 100.0 * covered / total
        if crate in EXEMPT:
            print(f"{crate:<22}{total:>8}{covered:>9}{pct:>8.1f}%  exempt: {EXEMPT[crate]}")
            continue
        ok = pct + 1e-9 >= minimum
        print(f"{crate:<22}{total:>8}{covered:>9}{pct:>8.1f}%  {'ok' if ok else 'BELOW ' + str(minimum) + '%'}")
        if not ok:
            failed.append(crate)
    if failed:
        print(f"coverage gate: {', '.join(failed)} below {minimum:g}% line coverage", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
