"""Work item 4.4's measurement (system design 4G.4) and Occupancy's check, the Python half.

    group_aggregate_measure.py to-ipc MONTH.parquet OUT.arrow [--batch ROWS]
        The trips' four columns Occupancy needs, as an Arrow IPC file the Rust harness reads
        (crates/moruna-kernels/examples/occupancy_group_aggregate.rs): pickup_at, dropoff_at,
        pu_location_id and fare_amount, in batches of ROWS trips (131072).
    group_aggregate_measure.py np-unique MONTH.parquet [--batch ROWS] [--repeats N]
        Occupancy's counting step as workloads.py does it, np.unique(key, return_counts=True), on
        the same expanded keys the harness counts: over the whole month at once, and batch by
        batch as the function runs (whose per-batch results the gate regroups). The expansion is
        workloads.py's and is not timed. One JSON line per timing.
    group_aggregate_measure.py canon DIR REFERENCE.json [--gate-canon BIN]
        The harness's output under DIR (Arrow IPC files) as Parquet, hashed by gate-canon
        (gate-canon/1, which regroups Occupancy's per-batch aggregates) and compared with 0.6.6's
        reference: rows and hash_exact must match.

Every timing names the load on the Mac (`uptime`'s averages) before and after.
"""
import argparse
import glob
import json
import os
import subprocess
import sys
import tempfile
import time

import numpy as np
import pyarrow as pa
import pyarrow.ipc as ipc
import pyarrow.parquet as pq

COLUMNS = ["pickup_at", "dropoff_at", "pu_location_id", "fare_amount"]


def load():
    """The Mac's load averages, as `uptime` gives them."""
    out = subprocess.run(["uptime"], capture_output=True, text=True).stdout
    return out.split("load averages:")[-1].strip() if "load averages:" in out else out.strip()


def to_ipc(src, dst, batch):
    table = pq.read_table(src, columns=COLUMNS)
    with ipc.new_file(dst, table.schema) as w:
        for b in table.to_batches(max_chunksize=batch):
            w.write_batch(b)
    print(json.dumps({"to_ipc": dst, "rows": table.num_rows}))


def expand_keys(batch):
    """workloads.py's occupancy up to its np.unique: the expanded keys."""
    a = batch["pickup_at"].cast(pa.int64()).to_numpy(zero_copy_only=False) // 60_000_000
    b = batch["dropoff_at"].cast(pa.int64()).to_numpy(zero_copy_only=False) // 60_000_000
    zone = batch["pu_location_id"].to_numpy(zero_copy_only=False)
    n = np.clip(b - a, 0, 240) + 1
    idx = np.repeat(np.arange(len(n)), n)
    minute = a[idx] + (np.arange(len(idx)) - np.repeat(np.cumsum(n) - n, n))
    return zone[idx] * 1440 + minute % 1440


def np_unique(src, batch, repeats):
    table = pq.read_table(src, columns=COLUMNS)
    keys = [expand_keys(b) for b in table.to_batches(max_chunksize=batch)]
    whole = np.concatenate(keys)
    for r in range(repeats):
        before = load()
        t = time.perf_counter()
        u, c = np.unique(whole, return_counts=True)
        seconds = time.perf_counter() - t
        print(json.dumps({"measure": "np.unique", "over": "month", "repeat": r,
                          "rows_in": int(whole.size), "groups": int(u.size),
                          "seconds": seconds, "load_before": before, "load_after": load()}))
        before = load()
        t = time.perf_counter()
        groups = 0
        for k in keys:
            u, c = np.unique(k, return_counts=True)
            groups += u.size
        seconds = time.perf_counter() - t
        print(json.dumps({"measure": "np.unique", "over": "batches", "repeat": r,
                          "batches": len(keys), "rows_in": int(whole.size),
                          "partial_rows": int(groups), "seconds": seconds,
                          "load_before": before, "load_after": load()}))


def canon(directory, reference, gate_canon):
    files = sorted(glob.glob(os.path.join(directory, "**", "*.arrow"), recursive=True))
    if not files:
        sys.exit(f"no .arrow files under {directory}")
    with open(reference) as f:
        ref = json.load(f)
    with tempfile.TemporaryDirectory(dir=os.path.dirname(os.path.abspath(directory))) as tmp:
        for i, path in enumerate(files):
            with ipc.open_file(path) as r:
                pq.write_table(r.read_all(), os.path.join(tmp, f"part-{i:05}.parquet"))
        out = subprocess.run([gate_canon, "digest", "occupancy", tmp], capture_output=True,
                             text=True, check=True).stdout
    got = json.loads(out)
    ok = got["rows"] == ref["rows"] and got["hash_exact"] == ref["hash_exact"]
    print(json.dumps({"canon": directory, "files": len(files), "rows": got["rows"],
                      "hash_exact": got["hash_exact"], "reference_rows": ref["rows"],
                      "reference_hash_exact": ref["hash_exact"], "ok": ok}))
    return ok


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    a = sub.add_parser("to-ipc")
    a.add_argument("src")
    a.add_argument("dst")
    a.add_argument("--batch", type=int, default=131072)
    a = sub.add_parser("np-unique")
    a.add_argument("src")
    a.add_argument("--batch", type=int, default=131072)
    a.add_argument("--repeats", type=int, default=3)
    a = sub.add_parser("canon")
    a.add_argument("dir")
    a.add_argument("reference")
    a.add_argument("--gate-canon", default=os.path.expanduser("~/moruna-data/bin/gate-canon"))
    args = p.parse_args()
    if args.cmd == "to-ipc":
        to_ipc(args.src, args.dst, args.batch)
    elif args.cmd == "np-unique":
        np_unique(args.src, args.batch, args.repeats)
    else:
        sys.exit(0 if canon(args.dir, args.reference, args.gate_canon) else 1)


if __name__ == "__main__":
    main()
