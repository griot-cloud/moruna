"""The plan's data, made on the benchmark's Linux machine instead of uploaded from the Mac (work
item 6.2b): each source file fetched from its public address (the plan's `source_url`, NYC TLC's
CloudFront for the taxi months), normalised by bench/vm/normalize.py as the Mac's copies were,
each output's SHA-256 checked against the plan's, and only then uploaded to the run's container
(store.py: `BENCH_ACCOUNT`, `BENCH_KEY`, `BENCH_CONTAINER`) under `prefix`, within Azure.

    dataprep.py --spec JSON --work DIR [--python PY]

SPEC is `{"source_url": ".../{file}", "sha256": {file: digest}, "prefix": "nyc/", "normalize":
"bench/vm/normalize.py"}`. Nothing is uploaded unless every file is fetched and matches: a
mismatch is reported (the run then stops, and `run.py --data-from-mac` uploads the Mac's copies).
The last line is `DATA {json}`; the exit code is 0 when the data is in the container.

Run by data-linux.sh in an environment with the plan's pyarrow, the version that wrote the Mac's
copies (a Parquet file's bytes name the writer's version).
"""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
MiB = 1 << 20


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(MiB), b""):
            h.update(chunk)
    return h.hexdigest()


def fetch(url, dest, tries=3, wait_s=5):
    """`url` into `dest`, tried `tries` times; its bytes."""
    for attempt in range(tries):
        try:
            with urllib.request.urlopen(url, timeout=120) as r, open(dest, "wb") as f:
                shutil.copyfileobj(r, f, MiB)
            return os.path.getsize(dest)
        except OSError:
            if attempt == tries - 1:
                raise
            time.sleep(wait_s * (attempt + 1))
    return 0


def prepare(spec, work, python=sys.executable, upload=None, fetcher=fetch):
    """The record of making `spec`'s files in `work`; `upload(local, name)` puts each in the
    container when all of them match."""
    raw, norm = os.path.join(work, "raw"), os.path.join(work, "norm")
    for d in (raw, norm):
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d)
    expected = spec["sha256"]
    files = sorted(expected)
    rec = {"source": spec["source_url"], "files": {}, "ok": False}
    t = time.time()
    for f in files:
        url = spec["source_url"].format(file=f)
        try:
            rec["files"][f] = {"raw_bytes": fetcher(url, os.path.join(raw, f))}
        except OSError as e:
            rec["files"][f] = {"error": f"not fetched from {url}: {str(e)[-200:]}"}
    rec["fetch_s"] = round(time.time() - t, 1)
    fetched = [f for f in files if "error" not in rec["files"][f]]
    if len(fetched) < len(files):
        rec["error"] = f"{len(files) - len(fetched)} of {len(files)} files not fetched"
        return rec
    t = time.time()
    p = subprocess.run([python, spec["normalize"], raw, norm] + [os.path.join(raw, f) for f in files],
                       capture_output=True, text=True)
    rec["normalize_s"] = round(time.time() - t, 1)
    if p.returncode != 0:
        rec["error"] = f"normalize.py exited {p.returncode}: {p.stderr[-500:]}"
        return rec
    for f in files:
        out = os.path.join(norm, f)
        got = sha256_of(out) if os.path.exists(out) else None
        rec["files"][f].update(sha256=got, bytes=os.path.getsize(out) if got else None, ok=got == expected[f])
    bad = [f for f in files if not rec["files"][f]["ok"]]
    if bad:
        rec["error"] = f"{len(bad)} of {len(files)} files differ from the plan's SHA-256: {', '.join(bad)}"
        return rec
    if upload:
        t = time.time()
        for f in files:
            upload(os.path.join(norm, f), spec.get("prefix", "") + f)
        rec["upload_s"] = round(time.time() - t, 1)
    rec["ok"] = True
    for d in (raw, norm):
        shutil.rmtree(d, ignore_errors=True)
    return rec


def main(argv=None, upload=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--spec", required=True, help="the data's spec, JSON text or a file")
    p.add_argument("--work", required=True)
    p.add_argument("--python", default=sys.executable, help="the interpreter normalize.py runs in")
    a = p.parse_args(argv)
    spec = json.loads(a.spec) if a.spec.lstrip().startswith("{") else json.load(open(a.spec))
    if upload is None:
        import store as stores
        store = stores.Store.from_env()
        if store is None:
            raise SystemExit("dataprep.py: no container: BENCH_ACCOUNT, BENCH_KEY and BENCH_CONTAINER")
        upload = store.upload
    rec = prepare(spec, os.path.abspath(a.work), a.python, upload)
    print("DATA " + json.dumps(rec), flush=True)
    return 0 if rec["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
