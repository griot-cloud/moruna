"""4G.2: Occupancy as written (bench/vm/workloads.py, in Python), batch by batch, as
occupancy-coverage runs the kernel and the gate's Rust function.

    occupancy_py.py read|python --batch ROWS [--out FILE.parquet] FILE.parquet...

`read` reads the batches and nothing else (the baseline of memory); `python` calls
`workloads.occupancy` on each. Each file is read in batches of `--batch` rows of the three columns
Occupancy reads, and every batch's output is written, in order, to `--out`. One JSON line goes to
stdout, with the fields occupancy-coverage gives (no `prep_s`, and no `alloc_peak_bytes`: the
process's peak is measured from outside).
"""
import json
import os
import sys
import time

START = time.perf_counter()

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "vm"))

import pyarrow.parquet as pq  # noqa: E402
import workloads  # noqa: E402

COLUMNS = ["pickup_at", "dropoff_at", "pu_location_id"]


def main(argv):
    mode, rest = argv[0], argv[1:]
    if mode not in ("read", "python"):
        raise SystemExit("usage: occupancy_py.py read|python --batch ROWS [--out FILE] FILE...")
    batch_rows, out, files = 0, None, []
    it = iter(rest)
    for a in it:
        if a == "--batch":
            batch_rows = int(next(it))
        elif a == "--out":
            out = next(it)
        else:
            files.append(a)
    if not batch_rows or not files:
        raise SystemExit("give --batch ROWS and at least one file")
    writer = None
    batches = rows_in = rows_out = 0
    compute_s = 0.0
    sizes = []
    for path in files:
        for batch in pq.ParquetFile(path).iter_batches(batch_size=batch_rows, columns=COLUMNS):
            batches += 1
            rows_in += batch.num_rows
            sizes.append(batch.num_rows)
            if mode == "read":
                continue
            t = time.perf_counter()
            o = workloads.occupancy(batch)
            compute_s += time.perf_counter() - t
            rows_out += o.num_rows
            if out:
                if writer is None:
                    writer = pq.ParquetWriter(out, o.schema, compression="zstd")
                writer.write_batch(o)
    if writer is not None:
        writer.close()
    print(json.dumps({"mode": mode, "batch": batch_rows, "files": len(files), "batches": batches,
                      "batch_rows": sizes, "rows_in": rows_in, "rows_out": rows_out,
                      "compute_s": compute_s, "process_s": time.perf_counter() - START}))


if __name__ == "__main__":
    main(sys.argv[1:])
