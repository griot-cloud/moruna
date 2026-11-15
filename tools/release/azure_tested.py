#!/usr/bin/env python3
"""Whether a commit on main is the merge of a pull request whose checks passed (Azure Repos).

The release path's rule, as on GitHub (.github/workflows/ci.yml, `tested`): a commit on main is
published only when it is the merge of a completed pull request into main whose validation
build of this pipeline succeeded. Main's branch policy runs that build and requires it, so what
was tested is what merged; a commit that reached main any other way publishes nothing.

    tools/release/azure_tested.py COMMIT

Reads SYSTEM_ACCESSTOKEN, SYSTEM_COLLECTIONURI, SYSTEM_TEAMPROJECT, BUILD_REPOSITORY_ID and
SYSTEM_DEFINITIONID, which Azure Pipelines sets (the token mapped into the step's env). Exits 0
when the commit qualifies, 1 with the reason when it does not.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.parse
import urllib.request


def get(url: str) -> dict:
    request = urllib.request.Request(
        url, headers={"Authorization": f"Bearer {os.environ['SYSTEM_ACCESSTOKEN']}"}
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def main(commit: str) -> int:
    base = os.environ["SYSTEM_COLLECTIONURI"].rstrip("/")
    project = urllib.parse.quote(os.environ["SYSTEM_TEAMPROJECT"])
    repo = os.environ["BUILD_REPOSITORY_ID"]
    definition = os.environ["SYSTEM_DEFINITIONID"]
    api = f"{base}/{project}/_apis"
    prs = get(
        f"{api}/git/repositories/{repo}/pullrequests?searchCriteria.status=completed"
        "&searchCriteria.targetRefName=refs/heads/main&$top=200&api-version=7.1"
    )["value"]
    merged = [p for p in prs if (p.get("lastMergeCommit") or {}).get("commitId") == commit]
    if not merged:
        print(f"{commit} is not the merge of a completed pull request into main, so nothing "
              "checked it; it is not published")
        return 1
    pr = merged[0]["pullRequestId"]
    builds = get(
        f"{api}/build/builds?definitions={definition}&branchName=refs/pull/{pr}/merge"
        "&statusFilter=completed&resultFilter=succeeded&$top=20&api-version=7.1"
    )["value"]
    # What was tested is what merged: a squash merge is a new commit, so the check is that a
    # passing build ran on the same tree.
    tree = get(f"{api}/git/repositories/{repo}/commits/{commit}?api-version=7.1")["treeId"]
    for build in builds:
        tested = get(
            f"{api}/git/repositories/{repo}/commits/{build['sourceVersion']}?api-version=7.1"
        )["treeId"]
        if tested == tree:
            print(f"{commit} is the merge of pull request {pr}, and validation build "
                  f"{build['buildNumber']} passed on the same tree ({tree})")
            return 0
    print(f"no passing validation build of pull request {pr} ran on {commit}'s tree ({tree}), "
          "so it is not published")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
