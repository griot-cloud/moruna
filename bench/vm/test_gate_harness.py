"""The harness's own tests for the gate's data-OS mode: the canonical form, the native-expression
rows, the 0.7 job documents, the plan, the split and the report. They run anywhere with pyarrow
(duckdb and polars for the native rows): `python3 -m pytest -q bench/vm/test_gate_harness.py`."""
import json
import os
import sys

import pyarrow as pa
import pyarrow.ipc as ipc
import pyarrow.parquet as pq
import pytest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import canon  # noqa: E402
import gate_jobs  # noqa: E402
import gate_plan  # noqa: E402
import report  # noqa: E402
import runner_split  # noqa: E402

FIX = os.path.join(HERE, "..", "..", "images", "gate", "fixtures")
MANIFEST = json.load(open(os.path.join(FIX, "manifest.json")))


def expected(job, case="main"):
    return ipc.open_file(os.path.join(FIX, job, f"{case}.expected.arrow")).read_all()


def nyc_input(case="main"):
    return ipc.open_file(os.path.join(FIX, "inputs", "nyc", f"{case}.arrow")).read_all()


@pytest.mark.parametrize("job", ["enrich", "audit", "occupancy", "score"])
def test_canon_is_the_manifest_and_split_blind(job):
    t = expected(job)
    c = canon.canonical(job, t)
    want = MANIFEST["jobs"][job]["main"]
    assert (c["rows"], c["hash_all"], c["hash_exact"]) == (want["rows"], want["hash_all"], want["hash_exact"])
    # The same rows in another order and split hash the same.
    half = t.num_rows // 2
    again = pa.concat_tables([t.slice(half), t.slice(0, half)])
    assert canon.canonical(job, again)["hash_all"] == c["hash_all"]


def test_occupancy_regroups_per_batch_counts():
    t = expected("occupancy")
    # Each count split in two rows, as two batches holding the same minute would give.
    ones = pa.array([1] * t.num_rows, pa.int64())
    rest = pa.compute.subtract(t["car_minutes"], ones)
    split = pa.concat_tables([t.set_column(2, "car_minutes", ones), t.set_column(2, "car_minutes", rest)])
    assert canon.canonical("occupancy", split)["hash_all"] == MANIFEST["jobs"]["occupancy"]["main"]["hash_all"]


def test_tolerance_on_score():
    t = expected("score")
    a = canon.canonical("score", t)
    tip = t["predicted_tip"].to_pylist()
    near = [None if v is None else v + 5e-7 for v in tip]
    far = [None if v is None else v + 5e-6 for v in tip]
    b = canon.canonical("score", t.set_column(3, "predicted_tip", pa.array(near, pa.float64())))
    c = canon.canonical("score", t.set_column(3, "predicted_tip", pa.array(far, pa.float64())))
    assert b["hash_exact"] == a["hash_exact"] and b["hash_all"] != a["hash_all"]
    ok = canon.compare("score", a, b)
    assert ok["ok"] and ok["differing"] > 0
    assert not canon.compare("score", a, c)["ok"]
    assert not canon.compare("enrich", canon.canonical("enrich", expected("enrich")),
                             canon.canonical("enrich", expected("enrich", "offset")))["ok"]
    assert canon.within(None, None, (0, 0)) and not canon.within(1.0, None, (0, 0))
    assert canon.within(float("nan"), float("nan"), (0, 0)) and canon.within(float("inf"), float("inf"), (0, 0))


def test_canon_encodes_every_type():
    t = pa.table({"b": [True, None], "u": pa.array([1, 2], pa.uint32()), "f": pa.array([1.5, None], pa.float32()),
                  "s": ["x", None], "l": pa.array([[1, None], None], pa.list_(pa.int64())),
                  "st": pa.array([{"x": 1}, None], pa.struct([("x", pa.int64())])),
                  "bin": pa.array([b"\x00", None], pa.binary())})
    c = canon.canonical("enrich", t)
    assert c["rows"] == 2
    with pytest.raises(TypeError):
        canon.canonical("enrich", pa.table({"d": pa.array([1], pa.date32())}))


def native_rows(tmp_path, engine, workload):
    native = pytest.importorskip("native_exprs")
    pytest.importorskip("duckdb" if engine == "duckdb-sql" else "polars")
    src = tmp_path / "in.parquet"
    pq.write_table(nyc_input(), src, row_group_size=1000)
    out = tmp_path / engine
    out.mkdir()
    native.run(engine, workload, [str(src)], str(out), 2, 1024, {"zstd_level": 1, "row_group_rows": 1 << 20})
    return canon.canonical(workload, canon.read_output([str(out)]))


@pytest.mark.parametrize("engine,workload", [("duckdb-sql", "enrich"), ("polars-expr", "enrich"),
                                             ("duckdb-sql", "occupancy")])
def test_native_expressions_give_python_rows(tmp_path, engine, workload):
    c = native_rows(tmp_path, engine, workload)
    assert c["hash_all"] == MANIFEST["jobs"][workload]["main"]["hash_all"]


def test_native_expressions_refuse_udf_jobs(tmp_path):
    import native_exprs
    with pytest.raises(ValueError):
        native_exprs.run("duckdb-sql", "score", [], str(tmp_path), 1, 64, {})
    with pytest.raises(ValueError):
        native_exprs.duckdb_query("audit", "t")


def test_job_documents():
    files = [f"/data/nyc/norm/yellow_tripdata_2024-{m:02d}.parquet" for m in range(1, 13)]
    d = gate_jobs.document("score", files, "/data/out/x", {"digest": "sha256:" + "a" * 64, "location": "/oci"})
    k = d["kernels"][0]
    assert (k["kind"], k["command"], k["plane"], k["abi"]) == ("image", ["/gate-score"], True, "moruna")
    assert d["source"]["url"] == files and d["sink"]["options"]["compression"] == "zstd"
    disk = gate_jobs.document("score", files, "/data/out/x", {}, disk=True)
    assert disk["source"]["url"][0] == "disk://2024-01"
    names = gate_jobs.inputs(["/a/x.parquet", "/b/x.parquet", "/c/" + "y" * 40 + ".parquet"])
    assert list(names)[:2] == ["x", "x-1"] and all(len(n) <= 24 for n in names)
    with pytest.raises(ValueError):
        gate_jobs.document("nope", files, "/o", {})


def test_job_document_written(tmp_path):
    p = tmp_path / "job.json"
    gate_jobs.write(str(p), "enrich", ["/a/f.parquet"], "/o", {}, disk=True)
    assert json.load(open(p))["kernels"][0]["command"] == ["/gate-enrich"]
    assert json.load(open(tmp_path / "job.inputs.json")) == {"f": "/a/f.parquet"}


def test_the_nyc_plan():
    plan = gate_plan.nyc(["f"], "d8")
    ids = [p["id"] for p in plan]
    assert len(ids) == len(set(ids))
    enrich = [p for p in plan if p["workload"] == "enrich"]
    engines = gate_plan.engines_for("enrich")
    assert [p["engine"] for p in enrich if p.get("repeat") == 1] == engines
    assert [p["engine"] for p in enrich if p.get("repeat") == 2] == engines[::-1]
    assert sum(1 for p in enrich if p.get("noise")) == 4
    assert "polars-expr" not in gate_plan.engines_for("occupancy")
    assert "duckdb-sql" not in gate_plan.engines_for("score")
    assert "moruna066-rust" not in gate_plan.engines_for("score", with_066_rust=False)
    vm = next(p for p in plan if p["engine"] == "rust-baseline")
    assert (vm["threads"], vm["batch"], vm["group"]) == (8, 0, 2)
    assert gate_plan.nyc(["f"], "d4")[0]["cpus"] == 4


def test_split_from_a_report():
    d = {"tasks": [{"role": "source", "busy_ms": 2000}, {"role": "function", "busy_ms": 5000,
                    "input_wait_ms": 100, "output_wait_ms": 300}, {"role": "sink", "busy_ms": 1000}],
         "exit": "completed"}
    s = runner_split.split(d)["split"]
    assert s == {"read_decode_s": 2.0, "function_s": 5.0, "encode_write_s": 1.0, "wait_s": 0.4}
    assert runner_split.split({})["split"]["function_s"] == 0


def fake(engine, repeat, wall, cpu, canon_digest, workload="enrich", noise=None):
    return {"spec": {"id": f"g-d8-{workload}-{engine}-r{repeat}{noise or ''}", "engine": engine, "workload": workload,
                     "machine": "d8", "repeat": repeat, "noise": noise},
            "ok": True, "wall_s": wall, "cpu_s": cpu, "cg_peak_bytes": 2 << 30, "oom_kills": 0,
            "canon": canon_digest, "split": {"function_s": 1.0}, "function": None, "boot_s": 0.5 if engine == "moruna-vm" else None}


def test_the_report(tmp_path):
    good = {"rows": 10, "hash_exact": "a", "hash_all": "b"}
    bad = {"rows": 10, "hash_exact": "a", "hash_all": "c"}
    recs = []
    for r in range(1, 6):
        recs += [fake("moruna", r, 7.0, 40.0, good), fake("moruna-vm", r, 2.0, 12.0, good),
                 fake("rust-baseline", r, 4.0, 14.0, good), fake("duckdb-sql", r, 3.0, 20.0, bad)]
    recs += [fake("moruna", None, 7.1, 40.0, good, noise="p1"), fake("moruna", None, 6.9, 40.0, good, noise="p2")]
    for i, rec in enumerate(recs):
        json.dump(rec, open(tmp_path / f"g-{i}.json", "w"))
    text = report.render(report.load(str(tmp_path)), ["d8"])
    assert "| 1 | moruna-vm | rust | 2.0 | 12.0 |" in text
    assert "against the plain Rust pool (best: rust-baseline, 4.0 s): 2.00x" in text and "is met" in text
    assert "DIFFERS" in text  # duckdb-sql's rows are not 0.6.6's
    assert "noise" in text
    slow = [fake("moruna-vm", 1, 3.9, 15.0, good), fake("rust-baseline", 1, 4.0, 14.0, good)]
    table, _ = report.rows_for(slow, "d8", "enrich")
    assert any("NOT met" in line and "CPU-seconds higher" in line for line in report.speedups(table))
    assert report.speedups({}) == ["0.7 (moruna-vm) has no measured run."]


def test_driver_commands(tmp_path, monkeypatch):
    import driver
    spec = {"id": "g-d8-score-rust-baseline-r1", "engine": "rust-baseline", "workload": "score", "files": ["f"],
            "cpus": 8, "mem_gib": 24, "threads": 8, "batch": 0}
    cmd, report_path = driver.command(spec, "/out", "/list")
    assert cmd[-10:] == [f"{driver.GATE}/gate-baseline", "--job", "score", "--files", "/list", "--out", "/out",
                         "--threads", "8", "--batch"] or cmd[cmd.index("--job") + 1] == "score"
    assert report_path is None and cmd[0] == "setpriv"
    monkeypatch.setattr(driver, "RES", str(tmp_path))
    vm = dict(spec, engine="moruna-vm", image={"digest": "sha256:" + "0" * 64, "location": "/oci"})
    cmd, report_path = driver.command(vm, "/out", "/list")
    assert cmd[cmd.index(driver.MORUNA07) + 1] == "run" and report_path.endswith(".report.json")
    assert json.load(open(tmp_path / f"{vm['id']}.job.json"))["kernels"][0]["command"] == ["/gate-score"]
    py = dict(spec, engine="moruna", profiles=str(tmp_path / "p"), staging="/s")
    monkeypatch.setattr(driver.os, "chown", lambda *a: None)
    c = driver.runner_command(py, py["id"], "/out", "/list")
    assert "--profiles" in c and "--staging" in c and c[c.index("--engine") + 1] == "moruna"
    assert driver.command(py, "/out", "/list") == (None, None)
    # moruna-vm's result: the boot apart, the split from its report.
    rp = tmp_path / "r.json"
    json.dump({"vm": {"boot_ms": 850}, "exit": "completed",
               "tasks": [{"role": "function", "busy_ms": 3000}]}, open(rp, "w"))
    r = driver.vm_result(vm, str(rp), 10.0)
    assert (r["boot_s"], r["function"], r["moruna"]["split"]["function_s"]) == (0.85, "rust", 3.0)
    assert "error" in driver.vm_result(vm, str(tmp_path / "missing.json"), 1.0)


# ---------------------------------------------------------------------------------------------
# The node fixtures' KVM mode (kernel/node/tools/proof.py, kernel/tools/node-fixtures.sh): the
# parts a host without /dev/kvm can check.

ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))


def load_proof():
    import importlib.util
    spec = importlib.util.spec_from_file_location("proof", os.path.join(ROOT, "kernel", "node", "tools", "proof.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def evidence(root, mode, broken=None):
    import hashlib as h
    inputs = {"kernel_elf": "a" * 64}
    census = h.sha256(json.dumps(inputs, sort_keys=True).encode()).hexdigest()
    proof = load_proof()
    for cpus in (1, 2, 4):
        for case, name in enumerate(proof.NAMES, 1):
            out = root / f"cpu{cpus}" / f"case{case}"
            out.mkdir(parents=True)
            rec = {"version": 1, "cpus": cpus, "case": case, "passed": True, "cleanup": True, "mode": mode,
                   "qemu_exit": 33, "cleanup_method": "guest-normal33", "inputs": inputs, "inputs_sha256": census}
            if broken == (cpus, case):
                rec["passed"] = False
            (out / "result.json").write_text(json.dumps(rec))
            serial = f"kk: {cpus} CPUs online\n"
            if case == 3:
                serial += "faulted: its record on slot 1 was refused by the engine's check: x\n"
            (out / "serial.log").write_text(serial)
            (out / "peer.log").write_text(f"embedder: ok: run {case} ({name}): done\n")


def test_proof_kvm_mode(tmp_path):
    proof = load_proof()
    assert proof.accel_args("kvm") == ["-accel", "kvm", "-cpu", "host"]
    assert proof.accel_args("tcg") == ["-accel", "tcg", "-cpu", "max"]
    with pytest.raises(ValueError):
        proof.accel_args("hvf")
    assert proof.kvm_ready(str(tmp_path / "no-kvm")) is False
    evidence(tmp_path / "kvm", "explicit-kvm")
    assert "all15" in proof.check(tmp_path / "kvm", "kvm")
    # TCG evidence is not KVM evidence, and a failed case fails the check.
    with pytest.raises(ValueError):
        proof.check(tmp_path / "kvm", "tcg")
    evidence(tmp_path / "bad", "explicit-kvm", broken=(2, 3))
    with pytest.raises(ValueError):
        proof.check(tmp_path / "bad", "kvm")


def test_node_fixtures_arguments(tmp_path):
    import subprocess
    script = os.path.join(ROOT, "kernel", "tools", "node-fixtures.sh")
    base = ["bash", script, str(tmp_path / "b"), str(tmp_path / "e"), "/q", "/r"]
    run = lambda *a: subprocess.run(base + list(a), capture_output=True, text=True)
    p = run("--accel", "hvf")
    assert p.returncode == 2 and "--accel tcg|kvm" in p.stderr
    p = run("60", "--accel", "kvm", "--sdk-proof", "/bin/ls")
    assert p.returncode == 2 and "SDK proof launches its own TCG VM" in p.stderr
    p = run("--bogus")
    assert p.returncode == 2 and "unknown argument" in p.stderr


def test_the_large_jobs_plan():
    plan = gate_plan.ooc({"overture": ["o"], "fineweb": ["f"]}, "d8")
    shape = [p["engine"] for p in plan if p["workload"] == "ov_shape"]
    attrs = [p["engine"] for p in plan if p["workload"] == "ov_attrs"]
    assert shape == attrs[::-1] and len(plan) == 18
    assert next(p for p in plan if p["workload"] == "fw_minhash" and p["engine"] == "loop")["batch"] == 20000
    assert all(p["files"] == ["f"] for p in plan if p["workload"] == "fw_minhash")
