#!/usr/bin/env python3
"""Cancel this pipeline's older unfinished runs of the same pull request or branch (Azure).

A newer run of a pull request or branch answers the same question for a newer commit, so the
older one only holds the agents (the Managed DevOps Pool runs one job at a time). As GitHub's
`concurrency` did (.github/workflows/ci.yml), runs of main are never cancelled: a superseded
release is a half published one.

    tools/release/azure_supersede.py

Reads SYSTEM_ACCESSTOKEN, SYSTEM_COLLECTIONURI, SYSTEM_TEAMPROJECT, SYSTEM_DEFINITIONID,
BUILD_BUILDID and BUILD_SOURCEBRANCH, which Azure Pipelines sets (the token mapped into the
step's env). Prints what it cancelled; a refusal to cancel is reported and does not fail the run.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request


def call(method: str, url: str, body: dict | None = None) -> dict:
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(
        url,
        data=data,
        method=method,
        headers={
            "Authorization": f"Bearer {os.environ['SYSTEM_ACCESSTOKEN']}",
            "Content-Type": "application/json",
        },
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def main() -> int:
    branch = os.environ["BUILD_SOURCEBRANCH"]
    if branch == "refs/heads/main":
        print("a run of main is never cancelled by a newer one")
        return 0
    base = os.environ["SYSTEM_COLLECTIONURI"].rstrip("/")
    project = urllib.parse.quote(os.environ["SYSTEM_TEAMPROJECT"])
    api = f"{base}/{project}/_apis/build/builds"
    me = int(os.environ["BUILD_BUILDID"])
    query = urllib.parse.urlencode(
        {
            "definitions": os.environ["SYSTEM_DEFINITIONID"],
            "branchName": branch,
            "statusFilter": "inProgress,notStarted",
            "api-version": "7.1",
        }
    )
    older = [b for b in call("GET", f"{api}?{query}")["value"] if b["id"] < me]
    if not older:
        print(f"no older unfinished run of {branch}")
    for build in older:
        try:
            call("PATCH", f"{api}/{build['id']}?api-version=7.1", {"status": "cancelling"})
            print(f"cancelled {build['buildNumber']}: {branch} has a newer run, {me}")
        except urllib.error.HTTPError as error:
            print(f"could not cancel {build['buildNumber']} ({error.code}): the pipeline's "
                  "identity needs 'Stop builds' on this pipeline")
    return 0


if __name__ == "__main__":
    sys.exit(main())
