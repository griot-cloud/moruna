"""Checks a built `moruna_gate` against the oracle's fixtures: every function, every case, the
same canonical hash as the Python function's (exact columns; tolerant ones within tolerance).

    python3 -I images/gate/py066/check.py NATIVE_LIBRARY   # the cdylib cargo built

The library is copied as `moruna_gate/_native` into a scratch package, so nothing is installed.
On the benchmark VM, `pip install images/gate/py066` builds and installs it with maturin into the
interpreter 0.6.6 runs on (3.14t), and runner.py's `moruna066-rust` engine imports it.
"""
import importlib
import json
import os
import shutil
import sys
import sysconfig
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
FIX = os.path.join(ROOT, "images", "gate", "fixtures")
sys.path.insert(0, os.path.join(ROOT, "bench", "vm"))


def main(lib):
    import pyarrow as pa
    import pyarrow.ipc as ipc
    import canon
    scratch = tempfile.mkdtemp()
    try:
        pkg = os.path.join(scratch, "moruna_gate")
        shutil.copytree(os.path.join(HERE, "python", "moruna_gate"), pkg)
        shutil.copy(lib, os.path.join(pkg, "_native" + sysconfig.get_config_var("EXT_SUFFIX")))
        sys.path.insert(0, scratch)
        gate = importlib.import_module("moruna_gate")
        m = json.load(open(os.path.join(FIX, "manifest.json")))
        bad = 0
        for job, cases in sorted(m["jobs"].items()):
            for case, rec in sorted(cases.items()):
                batch = ipc.open_file(os.path.join(FIX, rec["input"])).read_all()
                batch = batch.combine_chunks().to_batches()[0] if batch.num_rows else pa.RecordBatch.from_pylist([], schema=batch.schema)
                try:
                    out = getattr(gate, job)(batch)
                except ValueError as e:
                    ok = "error" in rec
                    print(f"{job}/{case}: refused ({str(e)[:60]}){'' if ok else ' UNEXPECTED'}")
                    bad += not ok
                    continue
                c = canon.canonical(job, pa.Table.from_batches([out]))
                ok = "error" not in rec and c["rows"] == rec["rows"] and c["hash_exact"] == rec["hash_exact"]
                print(f"{job}/{case}: {c['rows']} rows, hash_exact {c['hash_exact'] == rec.get('hash_exact')}, "
                      f"hash_all {c['hash_all'] == rec.get('hash_all')}")
                bad += not ok
        print("moruna_gate: all cases match" if not bad else f"moruna_gate: {bad} cases differ")
        return 1 if bad else 0
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
