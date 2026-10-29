"""The digest of an OCI layout's manifest for one platform, through any image index
above it (build.sh, SDD 28 k).

    python3 kernel/tools/oci-manifest.py LAYOUT OS ARCHITECTURE
"""

import json
import sys


def blob(root, digest):
    with open(f"{root}/blobs/sha256/{digest.split(':', 1)[1]}") as f:
        return json.load(f)


def find(root, entries, os_, arch):
    for e in entries:
        media = e.get("mediaType", "")
        if media.endswith("manifest.v1+json"):
            p = e.get("platform")
            if p is None or (p.get("os") == os_ and p.get("architecture") == arch):
                return e["digest"]
        elif media.endswith("index.v1+json"):
            found = find(root, blob(root, e["digest"])["manifests"], os_, arch)
            if found:
                return found
    return None


def main():
    if len(sys.argv) != 4:
        sys.exit("usage: oci-manifest.py LAYOUT OS ARCHITECTURE")
    root, os_, arch = sys.argv[1:]
    with open(f"{root}/index.json") as f:
        digest = find(root, json.load(f)["manifests"], os_, arch)
    if not digest:
        sys.exit(f"{root}: no {os_}/{arch} manifest")
    print(digest)


main()
