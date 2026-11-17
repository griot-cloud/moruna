"""Adds environment entries to every image of an OCI layout, in place: the config's Env, then the
config's and the manifest's digests and sizes, then the index. image.sh uses it to declare
MORUNA_PROCESSES, which a job's entry cannot set (the runner keeps MORUNA_ names).

    python3 -I image_env.py LAYOUT NAME=VALUE...
"""
import hashlib
import json
import os
import sys


def blob(layout, digest):
    return os.path.join(layout, "blobs", "sha256", digest.split(":", 1)[1])


def put(layout, data: bytes) -> str:
    d = hashlib.sha256(data).hexdigest()
    with open(os.path.join(layout, "blobs", "sha256", d), "wb") as f:
        f.write(data)
    return f"sha256:{d}"


def main(layout, entries):
    names = {e.split("=", 1)[0] for e in entries}
    with open(os.path.join(layout, "index.json")) as f:
        index = json.load(f)
    for m in index["manifests"]:
        with open(blob(layout, m["digest"])) as f:
            manifest = json.load(f)
        with open(blob(layout, manifest["config"]["digest"])) as f:
            config = json.load(f)
        env = [e for e in config.setdefault("config", {}).get("Env", []) if e.split("=", 1)[0] not in names]
        config["config"]["Env"] = env + list(entries)
        data = json.dumps(config, separators=(",", ":")).encode()
        manifest["config"]["digest"], manifest["config"]["size"] = put(layout, data), len(data)
        mdata = json.dumps(manifest, separators=(",", ":")).encode()
        m["digest"], m["size"] = put(layout, mdata), len(mdata)
    with open(os.path.join(layout, "index.json"), "w") as f:
        json.dump(index, f, separators=(",", ":"))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2:])
