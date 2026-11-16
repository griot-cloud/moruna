#!/usr/bin/env python3
"""What an earlier run of this pipeline already did from the same inputs (Azure Pipelines).

The Azure port of reuse.py. A run pays for work once per set of inputs: the gate (the Rust,
Python and kernel jobs) and each wheel. When a job of an earlier run passed on inputs this
run has, this run reuses it: the gate is not run again, and a wheel is downloaded from that run
instead of being built. A wheel-only fix rebuilds that wheel and nothing else, and the merge to
main publishes the wheels its pull request built.

A passed job records what it covered as a tag on its run, `<name>-<fingerprint>`, so a later
run finds it by its inputs without fetching the earlier run's commit. The fingerprint is the
blobs of the job's input files at the run's commit:

  gate    every file but the ones nothing in it reads: the pipeline's own files, the release
          scripts, and CHANGELOG.md (the lint job, which always runs, checks those). Queue the
          run with `gate: always` to run it anyway, after changing what the gate runs.
  wheels  reuse.py's sources, less tests, plus the wheels' build recipe.

    tools/release/azure_reuse.py              # in the version job: sets the outputs below
    tools/release/azure_reuse.py --dry-run    # here: what a run of HEAD would reuse

Outputs, one per name (`gate`, `wheels_linux_x86_64`, ...): `reused_<name>` ('true' or
'false'), `from_<name>` (the earlier run's id) and `fp_<name>` (this run's fingerprint, for the
tag a passing job adds). A missing or failed lookup reuses nothing, so the work is done again.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request

import reuse

GATE_NOT_INPUTS = (".azure-pipelines/", ".github/", "tools/release/", "CHANGELOG.md")
WHEELS_RECIPE = ".azure-pipelines/templates/wheels.yml"
WHEELS = {
    "wheels-linux-x86_64": reuse.LINUX_WHEELS + [WHEELS_RECIPE],
    "wheels-linux-aarch64": reuse.LINUX_WHEELS + [WHEELS_RECIPE],
    "wheels-macos-cp314": reuse.SOURCES + [WHEELS_RECIPE],
    "wheels-macos-cp314t": reuse.SOURCES + [WHEELS_RECIPE],
}
RUNS_LOOKED_AT = 20


def gate_fingerprint(commit: str) -> str:
    entries = [
        line
        for line in reuse.git("ls-tree", "-r", commit).splitlines()
        if not line.split("\t", 1)[1].startswith(GATE_NOT_INPUTS)
    ]
    return hashlib.sha256("\n".join(sorted(entries)).encode()).hexdigest()


def fingerprints(commit: str) -> dict[str, str]:
    found = {"gate": gate_fingerprint(commit)}
    found.update({name: reuse.fingerprint(commit, paths) for name, paths in WHEELS.items()})
    return found


def tag(name: str, fingerprint: str) -> str:
    return f"{name}-{fingerprint[:40]}"


class Azure:
    def __init__(self, token: str) -> None:
        base = os.environ["SYSTEM_COLLECTIONURI"].rstrip("/")
        project = urllib.parse.quote(os.environ["SYSTEM_TEAMPROJECT"])
        self.api = f"{base}/{project}/_apis"
        self.definition = os.environ["SYSTEM_DEFINITIONID"]
        self.token = token

    def get(self, path: str) -> dict:
        request = urllib.request.Request(
            f"{self.api}/{path}", headers={"Authorization": f"Bearer {self.token}"}
        )
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.load(response)

    def runs_tagged(self, name: str) -> list[dict]:
        query = urllib.parse.urlencode({
            "definitions": self.definition,
            "tagFilters": name,
            "queryOrder": "finishTimeDescending",
            "$top": RUNS_LOOKED_AT,
            "api-version": "7.1",
        })
        return self.get(f"build/builds?{query}")["value"]

    def has_artifact(self, run: int, artifact: str) -> bool:
        query = urllib.parse.urlencode({"artifactName": artifact, "api-version": "7.1"})
        try:
            self.get(f"build/builds/{run}/artifacts?{query}")
        except urllib.error.HTTPError as error:
            if error.code == 404:
                return False
            raise
        return True


def reusable(azure: Azure, wanted: dict[str, str], current: int | None) -> dict[str, int]:
    found: dict[str, int] = {}
    for name, fingerprint in wanted.items():
        for run in azure.runs_tagged(tag(name, fingerprint)):
            if run["id"] == current:
                continue
            if name != "gate" and not azure.has_artifact(run["id"], name):
                continue
            found[name] = run["id"]
            print(f"{name}: run {run['buildNumber']} (id {run['id']}) did it from these inputs",
                  file=sys.stderr)
            break
        else:
            print(f"{name}: no earlier run did it from these inputs", file=sys.stderr)
    return found


def variable(name: str) -> str:
    return name.replace("-", "_")


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--commit", default="HEAD")
    parser.add_argument("--gate", choices=["auto", "always"], default="auto")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    commit = reuse.git("rev-parse", args.commit).strip()
    wanted = fingerprints(commit)
    token = os.environ.get("SYSTEM_ACCESSTOKEN", "")
    if args.dry_run and not token:
        token = subprocess.run(
            ["az", "account", "get-access-token", "--resource",
             "499b84ac-1321-427f-aa17-267ca6975798", "--query", "accessToken", "-o", "tsv"],
            check=True, capture_output=True, text=True,
        ).stdout.strip()
    try:
        found = reusable(Azure(token), wanted, int(os.environ.get("BUILD_BUILDID", "0")) or None)
    except (KeyError, OSError, ValueError) as error:
        print(f"the lookup failed ({error!r}), so nothing is reused", file=sys.stderr)
        found = {}
    if args.gate == "always":
        found.pop("gate", None)
        print("gate: run anyway (gate: always)", file=sys.stderr)
    for name, fingerprint in wanted.items():
        values = {
            f"reused_{variable(name)}": "true" if name in found else "false",
            f"from_{variable(name)}": str(found.get(name, "")),
            f"fp_{variable(name)}": tag(name, fingerprint),
        }
        for key, value in values.items():
            if args.dry_run:
                print(f"{key}={value}")
            else:
                print(f"##vso[task.setvariable variable={key};isOutput=true]{value}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
