#!/usr/bin/env python3
"""Which release artifacts an earlier run on main already built from the same inputs.

A release builds each artifact (the wheels of one platform, the monitor and guest image
of one target) once per set of inputs. This prints `reuse=<json>` for $GITHUB_OUTPUT:
each artifact an earlier run built from the inputs this commit has, mapped to that run's
id, so the job downloads it instead of building it again. An artifact missing from the
map is built.

An artifact's inputs are the blobs of the files it is made from, at the run's commit,
less what cannot change its bytes: tests, comments in guest/build.sh, and apt's download
settings in it (how packages are fetched, never which: the snapshot is pinned, and every
guest image is built twice and must match).

    tools/release/reuse.py [--repo owner/name] [--commit sha]
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import os
import re
import subprocess
import sys

SOURCES = ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "pyproject.toml", "crates", "python"]
GUEST = SOURCES + ["guest"]
ARTIFACTS = {
    "wheels-ubuntu-24.04": SOURCES,
    "wheels-macos-15": SOURCES,
    "vmm-x86_64-unknown-linux-gnu": GUEST,
    "vmm-aarch64-unknown-linux-gnu": GUEST,
}
NOT_INPUTS = ["*/tests/*", "*/tests.rs", "python/tests/*"]
APT_SETTINGS = re.compile(r"^cat > /etc/apt/apt\.conf\.d/80moruna-retries <<RETRIES\n.*?^RETRIES\n",
                          re.MULTILINE | re.DOTALL)
RUNS_LOOKED_AT = 30


def git(*args: str) -> str:
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def gh(path: str) -> dict:
    out = subprocess.run(["gh", "api", path], check=True, capture_output=True, text=True).stdout
    return json.loads(out)


def build_script(commit: str) -> str:
    """guest/build.sh as it builds: no comment lines, no apt download settings."""
    text = APT_SETTINGS.sub("", git("show", f"{commit}:guest/build.sh"))
    return "".join(line for line in text.splitlines(keepends=True) if not line.lstrip().startswith("#"))


def fingerprint(commit: str, paths: list[str]) -> str:
    entries = []
    for line in git("ls-tree", "-r", commit, "--", *paths).splitlines():
        meta, path = line.split("\t", 1)
        if any(fnmatch.fnmatch(path, pattern) for pattern in NOT_INPUTS):
            continue
        blob = meta.split()[2]
        if path == "guest/build.sh":
            blob = hashlib.sha256(build_script(commit).encode()).hexdigest()
        entries.append(f"{meta.split()[0]} {blob} {path}")
    if not entries:
        raise SystemExit(f"no input files for {paths} at {commit}")
    return hashlib.sha256("\n".join(sorted(entries)).encode()).hexdigest()


def reusable(repo: str, commit: str) -> dict[str, int]:
    wanted = {name: fingerprint(commit, paths) for name, paths in ARTIFACTS.items()}
    found: dict[str, int] = {}
    runs = gh(f"repos/{repo}/actions/runs?branch=main&event=push&status=completed&per_page={RUNS_LOOKED_AT}")
    for run in runs["workflow_runs"]:
        if run["head_sha"] == commit or len(found) == len(wanted):
            continue
        artifacts = gh(f"repos/{repo}/actions/runs/{run['id']}/artifacts?per_page=100")["artifacts"]
        for artifact in artifacts:
            name = artifact["name"]
            if name in wanted and name not in found and not artifact["expired"]:
                if fingerprint(run["head_sha"], ARTIFACTS[name]) == wanted[name]:
                    found[name] = run["id"]
                    print(f"{name}: built from these inputs by run {run['id']} ({run['head_sha'][:7]})",
                          file=sys.stderr)
    for name in wanted.keys() - found.keys():
        print(f"{name}: no earlier run built these inputs; it is built", file=sys.stderr)
    return found


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY"))
    parser.add_argument("--commit", default="HEAD")
    args = parser.parse_args()
    if not args.repo:
        raise SystemExit("--repo is needed outside GitHub Actions")
    commit = git("rev-parse", args.commit).strip()
    print("reuse=" + json.dumps(reusable(args.repo, commit), sort_keys=True))


if __name__ == "__main__":
    main()
