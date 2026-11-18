"""proof.py's tests (work item 3.8): the whole proof against a fake node, through the same
calls a node answers (functions, jobs, events over SSE and MCP, reports), its outputs the gate's
fixtures written as the sink volume's only layer and read back by a stand-in for `moruna-engine
volume unpack`; a hash that differs, a job
that fails, a lost connection resumed after its last record, a job past its time, a busy node;
and a real TLS stream that drops. Nothing here reaches a node or a cloud.

    python3 -m pytest -q bench/node/test_proof.py
"""
import hashlib
import io
import json
import os
import shutil
import ssl
import subprocess
import sys
import threading
import types

import pyarrow as pa
import pyarrow.parquet as pq
import pytest
from pyarrow import ipc

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(1, os.path.join(HERE, "engines"))
sys.path.insert(2, os.path.join(HERE, "..", "vm"))
import canon  # noqa: E402
import node_api  # noqa: E402
import proof  # noqa: E402
import store  # noqa: E402

FIX = os.path.join(HERE, "..", "..", "images", "gate", "fixtures")
OPENSSL = node_api.OPENSSL if os.path.exists(node_api.OPENSSL) else shutil.which("openssl")
GiB = 1 << 30


def expected(job):
    return ipc.open_file(os.path.join(FIX, job, "main.expected.arrow")).read_all()


def record_lines(rid, data):
    return [f"id: {rid}", "event: event", "data: " + json.dumps(data), ""]


class FakeNode:
    """A node's API as proof.py calls it. A job "runs" at once: its output, `outputs[job]`, is written
    to the sink volume, `out_root` (a directory standing for its image), as its only layer: the
    last run's output is gone (`out_root/proof/RUN/`). Its stream holds five records, its report
    and its exit. `job_memory` is what GET /v1/node answers as job_memory_bytes, when given."""

    def __init__(self, out_root, outputs, fail=(), drop_after=None, busy=0, hang=(), listed=None, page=2,
                 mcp_errors=0, job_memory=None):
        self.out_root, self.outputs, self.fail, self.drop_after = out_root, outputs, set(fail), drop_after
        self.busy, self.hang, self.listed, self.page, self.mcp_errors = busy, set(hang), listed or {}, page, mcp_errors
        self.job_memory = job_memory
        self.calls, self.streams, self.jobs, self.registered, self.cancelled = [], [], {}, {}, []

    # The transport: (method, path, body, token) -> (status, bytes).
    def __call__(self, method, path, body=None, token=None):
        self.calls.append((method, path, body, token))
        ok = lambda obj, status=200: (status, json.dumps(obj).encode())  # noqa: E731
        if path == "/v1/node":
            facts = {"memory_bytes": 4 * GiB, "instance": "i-1", "cloud": "none", "version": "0.7.0"}
            if self.job_memory:
                facts["job_memory_bytes"] = self.job_memory
            return ok(facts)
        if path == "/v1/functions" and method == "GET":
            return ok({"functions": [{"name": n, "sha256": s} for n, s in self.listed.items()]})
        if path == "/v1/functions":
            self.registered[body["name"]] = body
            return ok({}, 201)
        if path == "/v1/jobs" and method == "POST":
            if self.busy:
                self.busy -= 1
                return 503, b'{"error":{"code":"busy","message":"a job runs"}}'
            return ok({"id": self.start(body)}, 201)
        if path == "/mcp":
            return self.mcp(body)
        jid = path.split("/")[3]
        job = self.jobs[jid]
        if path.endswith("/cancel"):
            self.cancelled.append(jid)
            return ok({"id": jid, "state": "cancelled"})
        if path.endswith("/report"):
            return ok(job["report"]["report"])
        return ok({"id": jid, "state": job["state"], "submitted": 100, "submitted_ms": 100_125,
                   "started": 100, "started_ms": 100_874, "ended": 104, "ended_ms": 104_002,
                   "exit_code": job["exit"]["code"]})

    def start(self, body):
        doc = body["v1"]
        run = doc["sink"]["url"].rstrip("/").split("/")[-1]
        job = run.split("-")[0]
        jid = f"j{len(self.jobs) + 1}"
        failed = run in self.fail
        shutil.rmtree(self.out_root, ignore_errors=True)
        os.makedirs(self.out_root)
        if not failed:
            out = os.path.join(self.out_root, "proof", run)
            os.makedirs(out, exist_ok=True)
            pq.write_table(self.outputs[job], os.path.join(out, "part-0.parquet"))
        records = [{"type": "event", "kind": "task_started", "task": "sink", "t_us": 1_500_000}] + \
                  [{"type": "event", "kind": "progress", "n": k} for k in range(2, 6)]
        self.jobs[jid] = {
            "doc": body, "run": run, "records": records, "state": "failed" if failed else "completed",
            "report": {"type": "report", "report": {"rows_out": self.outputs[job].num_rows, "cpu_s": 1.5},
                       "plan": {}},
            "exit": {"type": "exit", "code": 1 if failed else 0,
                     "diagnostic": "the function gate-score failed" if failed else None},
        }
        return jid

    def tail(self, jid):
        job = self.jobs[jid]
        if job["run"] in self.hang:
            return []
        return [("report", job["report"]), ("exit", job["exit"])]

    # Server-Sent Events, as GET /v1/jobs/{id}/events?from=N sends them.
    def stream(self, method, path, token=None):
        self.streams.append(path)
        jid, after = path.split("/")[3], int(path.split("from=")[1])
        first = sum(1 for p in self.streams if p.split("/")[3] == jid) == 1
        sent = 0
        for rid, data in enumerate(self.jobs[jid]["records"], start=1):
            if rid <= after:
                continue
            if first and self.drop_after is not None and sent == self.drop_after:
                raise ConnectionResetError("the connection was reset")
            yield from record_lines(rid, data)
            sent += 1
        for name, data in self.tail(jid):
            yield from [f"event: {name}", "data: " + json.dumps(data), ""]

    # MCP's moruna_jobs_events, a page of `page` records at a time.
    def mcp(self, call):
        if self.mcp_errors:
            self.mcp_errors -= 1
            raise ConnectionResetError("the connection was reset")
        args = call["params"]["arguments"]
        jid, after = args["id"], args.get("from", 0)
        records = [{"event": "event", "id": rid, "data": d}
                   for rid, d in enumerate(self.jobs[jid]["records"], start=1) if rid > after][:self.page]
        last = records[-1]["id"] if records else after
        if last == len(self.jobs[jid]["records"]):
            records += [{"event": n, "data": d} for n, d in self.tail(jid)]
        page = {"records": records, "next": last}
        result = {"content": [{"type": "text", "text": json.dumps(page)}], "structuredContent": page}
        return 200, json.dumps({"jsonrpc": "2.0", "id": call["id"], "result": result}).encode()


def references(root, jobs=("score", "enrich")):
    for job in jobs:
        json.dump(canon.canonical(job, expected(job)), open(root / f"reference-{job}-2024-01.json", "w"))


def functions(root, jobs=("score", "enrich")):
    for job in jobs:
        (root / f"gate-{job}").write_bytes(f"a static program for {job}".encode())


UNPACK = f"""#!{sys.executable}
# A stand-in for `moruna-engine volume unpack IMAGE DIR`: IMAGE is a directory standing for the
# volume's layer; DIR must not be there yet.
import os, shutil, sys
verb, sub, image, dest = sys.argv[1:5]
assert (verb, sub) == ("volume", "unpack")
if os.path.exists(dest):
    sys.exit(f"{{dest}} exists")
if not os.path.isdir(image):
    sys.exit(f"no layer at the start of {{image}}")
shutil.copytree(image, dest)
"""


def unpacker(root):
    path = root / "unpack"
    path.write_text(UNPACK)
    path.chmod(0o755)
    return str(path)


def args(tmp_path, *extra, jobs="score,enrich"):
    return ["--url", "https://node:443", "--pin", "ab" * 32, "--token", "T", "--function-key", "/k.pem",
            "--functions", str(tmp_path / "fn"), "--function-base", "https://code.example/fn",
            "--data", "volume", "--source-serial", "nyc-in", "--sink-serial", "nyc-out",
            "--volume-image", str(tmp_path / "vol"), "--unpacker", str(tmp_path / "unpack"),
            "--jobs", jobs, "--periods", "2024-01",
            "--references", str(tmp_path / "ref"), "--results", str(tmp_path / "res"),
            "--work", str(tmp_path / "work")] + list(extra)


@pytest.fixture
def world(tmp_path):
    for d in ("fn", "ref", "vol"):
        (tmp_path / d).mkdir()
    functions(tmp_path / "fn")
    references(tmp_path / "ref")
    unpacker(tmp_path)
    return tmp_path


def proof_run(tmp_path, node, *extra, jobs="score,enrich", clock=None):
    said = []
    rc = proof.main(args(tmp_path, *extra, jobs=jobs), transport=node, sign=lambda d: f"SIG-{d[:8]}",
                    log=said.append, sleep=lambda s: None, clock=clock or (lambda: 0.0))
    return rc, said


def result(tmp_path, run):
    return json.load(open(tmp_path / "res" / f"{run}.json"))


# --- The plan and the documents ----------------------------------------------------------------

def test_the_runs_and_their_documents():
    assert [r["id"] for r in proof.plan(["score", "audit"], ["2024-01", "2024"])] == \
        ["score-2024-01", "audit-2024-01", "score-2024", "audit-2024"]
    assert proof.files("2024-01") == ["yellow_tripdata_2024-01.parquet"]
    assert len(proof.files("2024")) == 12 and proof.files("2024")[-1] == "yellow_tripdata_2024-12.parquet"
    run = {"job": "occupancy", "period": "2024", "id": "occupancy-2024"}
    doc, secrets = proof.document(run, proof.Volumes("in-1", "out-1", "/x"), 4, 3 * GiB)
    assert doc["source"]["url"][0] == "disk://in-1/yellow_tripdata_2024-01.parquet" and len(doc["source"]["url"]) == 12
    assert doc["sink"]["url"] == "disk://out-1/proof/occupancy-2024/" and secrets == {}
    assert doc["kernels"] == [{"kind": "function", "function": "gate-occupancy"}]
    assert doc["budget"] == {"memory_bytes": 3 * GiB, "cpu": 4} and "object_store" not in doc
    blob = proof.Blob(store.Store("acct", "KEY", "data", "http://127.0.0.1:10000/acct"), "nyc/", "proof/x/")
    doc, secrets = proof.document(run, blob, 4, 3 * GiB)
    assert doc["source"]["url"][1] == "az://data/nyc/yellow_tripdata_2024-02.parquet"
    assert doc["sink"]["url"] == "az://data/proof/x/occupancy-2024/"
    assert doc["object_store"] == {"azure": {"account": "acct", "container": "data", "allow_http": True,
                                             "access_key": {"secret": "blob-key"}}}
    assert secrets == {"blob-key": "KEY"} and "KEY" not in json.dumps(doc)


def test_the_arguments_that_cannot_make_a_proof(tmp_path):
    for change, why in ((["--jobs", "nope"], "no job"), (["--periods", "2023"], "no period"),
                        (["--periods", "2024"], "needs --gate-canon")):
        with pytest.raises(SystemExit, match=why):
            proof.parse(args(tmp_path) + change)
    a = proof.parse(args(tmp_path))
    a.source_serial = None
    with pytest.raises(SystemExit, match="--source-serial"):
        proof.data_of(a)
    a.data, a.token = "blob", None
    with pytest.raises(SystemExit, match="BENCH_KEY"):
        proof.data_of(a)
    with pytest.raises(SystemExit, match="--token-key"):
        proof.tokens_of(a)
    a.token_key, a.instance = "/k", "i-1"
    assert isinstance(proof.tokens_of(a), node_api.Tokens)


# --- Whole proofs against the fake node ----------------------------------------------------------

def test_a_whole_proof_passes(world):
    node = FakeNode(str(world / "vol"), {"score": expected("score"), "enrich": expected("enrich")})
    rc, said = proof_run(world, node)
    assert rc == 0, said
    for job in ("score", "enrich"):
        r = result(world, f"{job}-2024-01")
        assert r["ok"] and r["state"] == "completed" and r["output_check"]["verdict"] == "bit for bit"
        assert r["records"] == 5 and r["first_rows_s"] == 1.5
        # The node's times, to its millisecond (item 2.4).
        assert r["times"] == {"submitted": 100.125, "started": 100.874, "ended": 104.002}
        assert r["output_check"]["rows_mine"] == expected(job).num_rows
    s = json.load(open(world / "res" / "proof.json"))
    assert s["ok"] and [x["id"] for x in s["runs"]] == ["score-2024-01", "enrich-2024-01"]
    # Each function registered once, by its image's SHA-256 and its signature, with the admin token.
    digest = hashlib.sha256(b"a static program for score").hexdigest()
    assert node.registered["gate-score"] == {"name": "gate-score", "address": "https://code.example/fn/gate-score",
                                             "sha256": digest, "signature": f"SIG-{digest[:8]}"}
    # The budget is the most the node takes: with no job_memory_bytes, its memory less 768 MiB.
    doc = node.jobs["j1"]["doc"]["v1"]
    assert doc["budget"]["memory_bytes"] == 4 * GiB - 768 * (1 << 20)
    assert doc["kernels"] == [{"kind": "function", "function": "gate-score"}]
    assert doc["source"]["url"] == ["disk://nyc-in/yellow_tripdata_2024-01.parquet"]
    assert node.streams == ["/v1/jobs/j1/events?from=0", "/v1/jobs/j2/events?from=0"]
    assert "bit for bit" in said[-1] and not os.listdir(world / "work")


# A container SAS as Azure writes one, every value fake; its `sig` is the secret part.
SIG = "FAKEfakeFAKEfake%2Bfake%2Ffake%3D"
SAS = f"sv=2022-11-02&sr=c&sp=r&se=2026-11-18T12%3A00Z&spr=https&sig={SIG}"
BLOB_BASE = "https://acct.blob.core.windows.net/functions"


def sas_run(tmp_path, node, monkeypatch, sas=SAS, jobs="score,enrich"):
    """proof.py with the functions in a private Blob container, its SAS in the environment."""
    monkeypatch.setenv("FUNCTIONS_SAS", sas)
    argv = args(tmp_path, jobs=jobs)
    argv[argv.index("--function-base") + 1] = BLOB_BASE
    said = []
    rc = proof.main(argv, transport=node, sign=lambda d: f"SIG-{d[:8]}", log=said.append, sleep=lambda s: None,
                    clock=lambda: 0.0)
    return rc, said


def written(tmp_path):
    """Everything proof.py wrote: its results and what is left of its work."""
    return "".join(open(os.path.join(d, f), errors="replace").read()
                   for root in ("res", "work") for d, _, fs in os.walk(tmp_path / root) for f in fs)


def secret_parts(sas=SAS):
    return [sas, SIG, "FAKEfakeFAKEfake+fake/fake="]


def test_each_function_is_registered_at_its_blob_address_with_the_sas(world, monkeypatch):
    node = FakeNode(str(world / "vol"), {"score": expected("score"), "enrich": expected("enrich")})
    rc, said = sas_run(world, node, monkeypatch)
    assert rc == 0, said
    for job in ("score", "enrich"):
        assert node.registered[f"gate-{job}"]["address"] == f"{BLOB_BASE}/gate-{job}?{SAS}"
    # The address is shown with the SAS named, never with it; nothing written holds it.
    assert "gate-score: registered" in "\n".join(said)
    assert any(s.endswith(f"fetched from {BLOB_BASE}/gate-score?<FUNCTIONS_SAS>") for s in said), said
    for part in secret_parts():
        assert part not in "\n".join(said) and part not in written(world)
    # Azure's query string may be given with its `?`: the address is the same.
    again = FakeNode(str(world / "vol"), {"score": expected("score")})
    rc, _ = sas_run(world, again, monkeypatch, sas=f"?{SAS}\n", jobs="score")
    assert rc == 0 and again.registered["gate-score"]["address"] == f"{BLOB_BASE}/gate-score?{SAS}"


def test_without_a_sas_the_address_is_the_base_and_the_name(monkeypatch):
    monkeypatch.delenv("FUNCTIONS_SAS", raising=False)
    assert proof.sas_of() == ""
    assert proof.address("https://code.example/fn", "gate-audit", proof.sas_of()) == "https://code.example/fn/gate-audit"
    assert proof.sas_of({"FUNCTIONS_SAS": f" ?{SAS} "}) == SAS
    assert proof.address(BLOB_BASE, "gate-audit", SAS) == f"{BLOB_BASE}/gate-audit?{SAS}"


def test_a_node_diagnostic_that_quotes_the_sas_is_redacted_in_the_logs_and_the_results(world, monkeypatch):
    """The node's fetch failure names the address it fetched (kernel/node/functions' https.rs:
    `{address}: {why}`), the SAS with it; proof.py keeps the failure and not the SAS."""
    node = FakeNode(str(world / "vol"), {"score": expected("score"), "enrich": expected("enrich")},
                    fail={"score-2024-01"})
    real = node.start

    def start(body):
        jid = real(body)
        if node.jobs[jid]["run"] == "score-2024-01":
            node.jobs[jid]["exit"]["diagnostic"] = (
                f"function gate-score: {BLOB_BASE}/gate-score?{SAS}: 403 (signature "
                f"{secret_parts()[2]} did not match)")
            node.jobs[jid]["report"]["report"]["error"] = f"fetching {BLOB_BASE}/gate-score?{SAS}"
        return jid
    node.start = start
    monkeypatch.setenv("BENCH_KEY", "fake-account-key-0123456789")
    rc, said = sas_run(world, node, monkeypatch)
    assert rc == 1
    r = result(world, "score-2024-01")
    assert r["diagnostic"] == (f"function gate-score: {BLOB_BASE}/gate-score?<FUNCTIONS_SAS>: 403 "
                               "(signature <FUNCTIONS_SAS> did not match)")
    assert r["exit"]["diagnostic"] == r["diagnostic"]
    assert any("FAIL: function gate-score: " in s and "?<FUNCTIONS_SAS>: 403" in s for s in said), said
    for part in secret_parts():
        assert part not in "\n".join(said) and part not in written(world)
    assert result(world, "enrich-2024-01")["ok"]


def test_a_refused_registration_says_why_without_the_sas(world, monkeypatch):
    node = FakeNode(str(world / "vol"), {"score": expected("score")})
    real = node.__call__

    def t(method, path, body=None, token=None):
        if path == "/v1/functions" and method == "POST":
            return 400, json.dumps({"error": {"code": "invalid", "message": f"{body['address']}: too long"}}).encode()
        return real(method, path, body, token)
    with pytest.raises(SystemExit) as stop:
        sas_run(world, t, monkeypatch, jobs="score")
    why = str(stop.value)
    assert why.startswith("proof.py: a function was not registered: POST /v1/functions: 400")
    assert "gate-score?<FUNCTIONS_SAS>: too long" in why
    for part in secret_parts():
        assert part not in why


def test_redact_names_each_secret_and_a_sas_s_signature_alone():
    r = proof.Redact({"FUNCTIONS_SAS": SAS, "BENCH_KEY": "fake-key==", "EMPTY": ""})
    assert r(f"a {SAS} b") == "a <FUNCTIONS_SAS> b"
    assert r(f"sig={SIG}&x") == "sig=<FUNCTIONS_SAS>&x"
    assert r("decoded FAKEfakeFAKEfake+fake/fake= here") == "decoded <FUNCTIONS_SAS> here"
    assert r("key fake-key==") == "key <BENCH_KEY>"
    assert r(42) == "42" and r("") == ""
    # A signature too short to be one is not looked for alone: it would hide ordinary text.
    short = proof.Redact({"FUNCTIONS_SAS": "sp=r&sig=ab"})
    assert short("ab sp=r&sig=ab") == "ab <FUNCTIONS_SAS>"
    assert proof.Redact({})("nothing") == "nothing"


def test_a_function_the_node_lists_already_is_not_registered_again(world):
    digest = hashlib.sha256(b"a static program for score").hexdigest()
    node = FakeNode(str(world / "vol"), {"score": expected("score")}, listed={"gate-score": digest},
                    job_memory=3 * GiB)
    rc, said = proof_run(world, node, jobs="score")
    assert rc == 0 and node.registered == {} and any("registered already" in s for s in said)
    # The budget is the node's job_memory_bytes when it answers one.
    assert node.jobs["j1"]["doc"]["v1"]["budget"]["memory_bytes"] == 3 * GiB


def test_a_hash_that_differs_fails_the_proof(world):
    t = expected("enrich")
    changed = t.set_column(t.schema.get_field_index("tip_share"), "tip_share",
                           pa.array([None if v is None else v + 1.0 for v in t["tip_share"].to_pylist()]))
    node = FakeNode(str(world / "vol"), {"score": expected("score"), "enrich": changed})
    rc, _ = proof_run(world, node, "--keep-output")
    assert rc == 1
    assert result(world, "score-2024-01")["ok"]
    r = result(world, "enrich-2024-01")
    assert not r["ok"] and r["output_check"]["verdict"] == "DIFFERS" and r["output_check"]["rows"]
    assert os.listdir(world / "work" / "enrich-2024-01") == ["part-0.parquet"]


def test_score_within_its_tolerance_passes_and_beyond_it_fails(world):
    t = expected("score")
    tip = t["predicted_tip"].to_pylist()
    for delta, ok, verdict in ((5e-7, True, "within tolerance"), (5e-6, False, "DIFFERS beyond")):
        moved = t.set_column(3, "predicted_tip",
                             pa.array([None if v is None else v + delta for v in tip], pa.float64()))
        node = FakeNode(str(world / "vol"), {"score": moved})
        rc, _ = proof_run(world, node, jobs="score")
        r = result(world, "score-2024-01")
        assert (rc == 0) is ok and r["output_check"]["verdict"].startswith(verdict), r["output_check"]


def test_a_failed_job_is_recorded_and_the_next_still_runs(world):
    node = FakeNode(str(world / "vol"), {"score": expected("score"), "enrich": expected("enrich")},
                    fail={"score-2024-01"})
    rc, said = proof_run(world, node)
    assert rc == 1
    r = result(world, "score-2024-01")
    assert not r["ok"] and r["state"] == "failed" and r["exit"]["code"] == 1
    assert r["diagnostic"] == "the function gate-score failed" and "output_check" not in r
    assert result(world, "enrich-2024-01")["ok"]
    assert "FAIL: the function gate-score failed" in "\n".join(said)


def test_a_lost_connection_resumes_after_the_last_record(world):
    node = FakeNode(str(world / "vol"), {"score": expected("score")}, drop_after=2)
    rc, said = proof_run(world, node, jobs="score")
    assert rc == 0
    # The second stream asks for what follows record 2, and every record came once.
    assert node.streams == ["/v1/jobs/j1/events?from=0", "/v1/jobs/j1/events?from=2"]
    r = result(world, "score-2024-01")
    assert r["records"] == 5 and r["resumed"] == 1 and r["ok"]
    assert any("resuming after record 2" in s for s in said)


def test_events_over_mcp_a_page_at_a_time_and_after_a_lost_call(world):
    node = FakeNode(str(world / "vol"), {"score": expected("score")}, mcp_errors=1)
    rc, said = proof_run(world, node, "--events", "mcp", jobs="score")
    assert rc == 0
    froms = [c[2]["params"]["arguments"]["from"] for c in node.calls if c[1] == "/mcp"]
    # The lost call, asked again from 0; then pages of two: 2, 4, then 5 with the report and exit.
    assert froms == [0, 0, 2, 4]
    r = result(world, "score-2024-01")
    assert r["ok"] and r["records"] == 5 and r["resumed"] == 1 and r["events_by"] == "mcp"


def test_an_mcp_error_is_the_run_s_failure(world):
    node = FakeNode(str(world / "vol"), {"score": expected("score")})
    node.mcp = lambda call: (200, json.dumps({"jsonrpc": "2.0", "id": 1, "result": {
        "content": [{"type": "text", "text": '{"error":{"code":"not_found"}}'}], "isError": True}}).encode())
    rc, _ = proof_run(world, node, "--events", "mcp", jobs="score")
    r = result(world, "score-2024-01")
    assert rc == 1 and r["state"] == "not followed" and "not_found" in r["error"]


def test_a_job_past_its_time_is_cancelled(world):
    node = FakeNode(str(world / "vol"), {"score": expected("score")}, hang={"score-2024-01"})
    now = [0.0]

    def clock():
        now[0] += 400.0
        return now[0]
    rc, _ = proof_run(world, node, "--timeout-s", "1000", jobs="score", clock=clock)
    r = result(world, "score-2024-01")
    assert rc == 1 and r["state"] == "timed out" and node.cancelled == ["j1"]


def test_a_busy_node_is_waited_for_and_a_refusal_is_recorded(world):
    node = FakeNode(str(world / "vol"), {"score": expected("score")}, busy=2)
    rc, said = proof_run(world, node, jobs="score")
    assert rc == 0 and sum("busy" in s for s in said) == 2
    refuse = FakeNode(str(world / "vol"), {"score": expected("score")})
    real = refuse.__call__

    def t(method, path, body=None, token=None):
        if path == "/v1/jobs" and method == "POST":
            return 400, b'{"error":{"code":"invalid","message":"no such function"}}'
        return real(method, path, body, token)
    rc, _ = proof_run(world, t, jobs="score")
    r = result(world, "score-2024-01")
    assert rc == 1 and r["state"] == "refused" and "no such function" in r["error"]


def test_an_output_or_a_reference_that_is_not_there(world):
    node = FakeNode(str(world / "vol"), {"score": expected("score"), "enrich": expected("enrich")})
    os.remove(world / "ref" / "reference-enrich-2024-01.json")
    real = node.start

    def start(body):
        jid = real(body)
        if node.jobs[jid]["run"] == "score-2024-01":
            shutil.rmtree(world / "vol" / "proof" / "score-2024-01")
        return jid
    node.start = start
    rc, _ = proof_run(world, node)
    assert rc == 1
    assert result(world, "score-2024-01")["output_check"]["verdict"].startswith("output not fetched")
    assert result(world, "enrich-2024-01")["output_check"]["verdict"].startswith("no reference")


# --- The pieces --------------------------------------------------------------------------------

def test_server_sent_events_are_read_with_their_numbers():
    lines = [": a comment", "id: 7", "event: event", 'data: {"a":', "data: 1}", "", "event: heartbeat",
             'data: {"type":"heartbeat"}', "", "id: x", "data: not json", "", "", "event: exit", 'data: {"code":0}']
    assert list(proof.sse_events(lines + [""])) == [
        ("event", 7, {"a": 1}), ("heartbeat", None, {"type": "heartbeat"}), ("exit", None, {"code": 0})]
    f = proof.Followed()
    for e, rid, d in [("event", 2, {"n": 2}), ("event", 1, {"n": 1}), ("event", 2, {"n": "again"}),
                      ("report", None, {"r": 1}), ("heartbeat", None, {})]:
        f.take(e, rid, d)
    assert f.events() == [{"n": 1}, {"n": 2}] and f.last == 2 and f.report == {"r": 1} and f.exit is None


def test_a_job_s_times_are_its_milliseconds_where_the_node_gives_them():
    # A node with item 2.4: each time to the millisecond, beside its whole seconds.
    record = {"state": "completed", "submitted": 1794931200, "submitted_ms": 1794931200250,
              "started": 1794931200, "started_ms": 1794931200999, "ended": 1794931221, "ended_ms": 1794931221251}
    times = node_api.job_times(record)
    assert times == {"submitted": 1794931200.25, "started": 1794931200.999, "ended": 1794931221.251}
    assert round(times["started"] - times["submitted"], 3) == 0.749
    # A node without them: its whole seconds; a time the job has not reached yet is None.
    assert node_api.job_times({"submitted": 100, "started": 101}) == {"submitted": 100, "started": 101, "ended": None}
    assert node_api.job_times({"submitted": 100, "submitted_ms": 100_001, "state": "starting"}) == {
        "submitted": 100.001, "started": None, "ended": None}


def test_comparisons_that_cannot_be_made(tmp_path, monkeypatch):
    ref = {"rows": 3, "hash_exact": "a", "hash_all": "b", "tolerant": ["predicted_tip"]}
    assert proof.compare("score", {"error": "x"}, ref, "/r")["verdict"] == "no canonical form: x"
    c = proof.compare("score", {"rows": 3, "hash_exact": "a", "hash_all": "c", "by": "gate-canon"}, ref, "/r")
    assert not c["ok"] and c["verdict"].startswith("not compared: no tolerant rows")
    assert proof.compare("audit", {"rows": 2, "hash_exact": "a", "hash_all": "b"}, ref, "/r")["verdict"] == \
        "DIFFERS: rows"
    assert proof.canonical("score", str(tmp_path))["error"] == "no output files"
    bad = proof.canonical("score", str(tmp_path), gate_canon=sys.executable)
    assert bad["error"]
    # A reference with no tolerant columns: hashes that differ differ.
    c = proof.compare("audit", {"rows": 3, "hash_exact": "a", "hash_all": "c"}, dict(ref, tolerant=[]), "/r")
    assert not c["ok"] and c["verdict"] == "DIFFERS"
    # Where there is no pyarrow, canon.py hashes nothing and compares no tolerant rows.
    monkeypatch.setattr(proof, "canon", None)
    assert proof.canonical("score", str(tmp_path))["error"].startswith(proof.NO_CANON)
    c = proof.compare("score", {"rows": 3, "hash_exact": "a", "hash_all": "c", "tolerant_rows": []},
                      dict(ref, tolerant_rows=[]), "/r")
    assert not c["ok"] and c["verdict"] == f"not compared: the tolerant values differ and {proof.NO_CANON}"


def test_proof_loads_where_there_is_no_pyarrow(monkeypatch):
    """Where proof.py runs beside a node under QEMU there may be no pyarrow: it loads, and
    gate-canon hashes."""
    import importlib
    monkeypatch.setitem(sys.modules, "canon", None)
    try:
        importlib.reload(proof)
        assert proof.canon is None
    finally:
        monkeypatch.undo()
        importlib.reload(proof)
    assert proof.canon is canon


def test_the_volume_fetch_and_the_blob_fetch(tmp_path):
    image = tmp_path / "image"
    v = proof.Volumes("in", "out", str(image), unpacker(tmp_path))
    run = {"id": "score-2024-01"}
    # No layer on the volume: the unpacker's refusal, said.
    with pytest.raises(RuntimeError, match="volume unpack: no layer"):
        v.fetch(run, str(tmp_path / "d"))
    # A layer without the run's output: what it holds, said.
    (image / "proof" / "audit-2024-01").mkdir(parents=True)
    (image / "proof" / "audit-2024-01" / "p.parquet").write_bytes(b"x")
    with pytest.raises(FileNotFoundError, match="audit-2024-01/p.parquet"):
        v.fetch(run, str(tmp_path / "d"))
    # The run's output: its files, and nothing else of the volume's left behind.
    (image / "proof" / "score-2024-01").mkdir()
    (image / "proof" / "score-2024-01" / "part-0.parquet").write_bytes(b"rows")
    (tmp_path / "d").mkdir()
    v.fetch(run, str(tmp_path / "d"))
    assert os.listdir(tmp_path / "d") == ["part-0.parquet"] and not (tmp_path / "d.volume").exists()

    class Store:
        def download(self, prefix, dest):
            self.got = (prefix, dest)
    s = Store()
    proof.Blob(s, "nyc/", "proof/p/").fetch({"id": "score-2024"}, "/d")
    assert s.got == ("proof/p/score-2024/", "/d")


GATE = os.environ.get("GATE_CANON")


@pytest.mark.skipif(not GATE, reason="GATE_CANON names a built gate-canon")
def test_a_month_checked_by_gate_canon(world):
    """gate-canon's digest of the node's output against canon.py's references; Score within its
    tolerance is compared on canon.py's tolerant rows, which gate-canon's digest does not carry."""
    t = expected("score")
    moved = t.set_column(3, "predicted_tip", pa.array([None if v is None else v + 5e-7
                                                       for v in t["predicted_tip"].to_pylist()], pa.float64()))
    node = FakeNode(str(world / "vol"), {"score": moved, "enrich": expected("enrich")})
    rc, said = proof_run(world, node, "--gate-canon", GATE)
    assert rc == 0, said
    s, e = result(world, "score-2024-01"), result(world, "enrich-2024-01")
    assert s["canon"]["by"] == "gate-canon" and s["output_check"]["verdict"].startswith("within tolerance")
    assert e["canon"]["by"] == "gate-canon" and e["output_check"]["verdict"] == "bit for bit"


@pytest.mark.skipif(not GATE, reason="GATE_CANON names a built gate-canon")
def test_the_year_is_checked_with_gate_canon_and_its_dump(world):
    """A year's reference keeps Score's tolerant rows in a gate-canon dump: the node's output is
    compared with it by `gate-canon compare`."""
    t = expected("score")
    out = world / "year"
    out.mkdir()
    pq.write_table(t, out / "p.parquet")
    dump = str(world / "ref" / "reference-score-2024.dump")
    ref = proof.canonical("score", str(out), GATE, dump)
    ref["dump"] = os.path.basename(dump)
    moved = t.set_column(3, "predicted_tip", pa.array([None if v is None else v + 5e-7
                                                       for v in t["predicted_tip"].to_pylist()], pa.float64()))
    mine_dir = world / "mine"
    mine_dir.mkdir()
    pq.write_table(moved, mine_dir / "p.parquet")
    mine = proof.canonical("score", str(mine_dir), GATE, str(world / "mine.dump"))
    c = proof.compare("score", mine, ref, str(world / "ref"), GATE)
    assert c["ok"] and c["verdict"] == "within tolerance (gate-canon)", c


NYC = os.path.expanduser(os.environ.get("BENCH_NYC", "~/moruna-data/nyc"))


@pytest.mark.skipif(not (GATE and os.path.isdir(os.path.join(NYC, "out066"))),
                    reason="GATE_CANON and 0.6.6's January outputs (references.py --keep-output) under BENCH_NYC")
@pytest.mark.parametrize("job", proof.JOBS)
def test_0_6_6_s_own_january_output_passes_the_check(job, tmp_path):
    """The check passes on what the references were made from: 0.6.6's outputs of January."""
    ref = json.load(open(os.path.join(NYC, f"reference-{job}-2024-01.json")))
    mine = proof.canonical(job, os.path.join(NYC, "out066", f"{job}-2024-01"), GATE, str(tmp_path / "m.dump"))
    c = proof.compare(job, mine, ref, NYC, GATE)
    assert c["ok"] and c["verdict"] == "bit for bit", c


# --- A real TLS stream that drops ----------------------------------------------------------------

@pytest.mark.skipif(not OPENSSL, reason="needs openssl to make a certificate")
def test_a_pinned_tls_stream_that_drops_is_resumed(tmp_path):
    import ssl
    from http.server import BaseHTTPRequestHandler, HTTPServer
    key, cert = tmp_path / "k.pem", tmp_path / "c.pem"
    subprocess.run([OPENSSL, "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
                    "-keyout", str(key), "-out", str(cert), "-days", "1", "-subj", "/CN=node"], check=True,
                   capture_output=True)
    paths = []
    records = [{"type": "event", "n": k} for k in range(1, 5)]

    class H(BaseHTTPRequestHandler):
        def do_GET(self):
            paths.append((self.path, self.headers["Authorization"]))
            if self.path.startswith("/v1/jobs/none/"):
                self.send_response(404)
                self.end_headers()
                self.wfile.write(b'{"error":{"code":"not_found"}}')
                return
            after = int(self.path.split("from=")[1])
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for rid, r in enumerate(records, start=1):
                if rid <= after:
                    continue
                self.wfile.write("\n".join(record_lines(rid, r)).encode() + b"\n")
                self.wfile.flush()
                if len(paths) == 1 and rid == 2:
                    return  # the first stream ends here, before the job's exit
            self.wfile.write(b'event: report\ndata: {"type":"report","report":{}}\n\n'
                             b'event: exit\ndata: {"type":"exit","code":0}\n\n')

        def log_message(self, *a):
            pass

    srv = HTTPServer(("127.0.0.1", 0), H)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(str(cert), str(key))
    srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
    th = threading.Thread(target=lambda: [srv.handle_request() for _ in range(3)])
    th.start()
    pin = hashlib.sha256(ssl.PEM_cert_to_DER_cert(cert.read_text())).hexdigest()
    node = node_api.Node(node_api.Https(f"https://127.0.0.1:{srv.server_address[1]}", pin, timeout=10),
                         {"observe": "O"})
    said = []
    got = proof.follow_sse(node, "j9", deadline=float("inf"), sleep=lambda s: None, log=said.append)
    assert paths[:2] == [("/v1/jobs/j9/events?from=0", "Bearer O"), ("/v1/jobs/j9/events?from=2", "Bearer O")]
    assert got.events() == records and got.exit == {"type": "exit", "code": 0} and got.resumed == 1
    with pytest.raises(node_api.ApiError, match="404"):
        list(node.t.stream("GET", "/v1/jobs/none/events?from=0", "O"))
    th.join()
    srv.server_close()


# --- Blob with a container SAS (work item 3.8b: the proof on Servercore) -------------------------

# The data container's SAS, as nyc-blob.sh makes it: read, add, create, write and list.
DATA_SIG = "DATAdataDATAdata%2Bdata%2Fdata%3D"
DATA_SAS = f"se=2026-11-18T20%3A00Z&sp=racwl&spr=https&sv=2022-11-02&sr=c&sig={DATA_SIG}"


def data_secret_parts():
    return [DATA_SAS, DATA_SIG, "DATAdataDATAdata+data/data="]


class FakeBlob:
    """Blob's REST API as a container SAS reaches it, over plain HTTP on the loopback (as an
    emulator's endpoint is): List Blobs a page of `page` names at a time, and Get Blob, each only
    with the SAS as its query, else 403. The container's blobs are the files under `root`, by
    their paths from it. `fail_once` names a blob whose first Get is cut short. With `tls`, a
    certificate and its key, it is served over HTTPS, as Azure serves it."""

    def __init__(self, root, container="data", page=1, fail_once=None, tls=None):
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        import urllib.parse as up
        self.root, self.container, self.page, self.fail_once = str(root), container, page, fail_once
        self.requests = []
        fake = self

        class H(BaseHTTPRequestHandler):
            def do_GET(self):
                path, _, query = self.path.partition("?")
                fake.requests.append(self.path)
                q = dict(up.parse_qsl(query, keep_blank_values=True))
                if q.get("sig") != up.unquote(DATA_SIG) or q.get("sp") != "racwl":
                    return self.send(403, b"<Error><Code>AuthenticationFailed</Code></Error>")
                head = f"/acct/{fake.container}"
                if path == head and q.get("comp") == "list":
                    return self.send(200, fake.listing(q.get("prefix", ""), q.get("marker", "")))
                name = up.unquote(path[len(head) + 1:])
                file = os.path.join(fake.root, name)
                if not path.startswith(head + "/") or not os.path.isfile(file):
                    return self.send(404, b"<Error><Code>BlobNotFound</Code></Error>")
                if name == fake.fail_once:
                    fake.fail_once = None
                    self.send_response(200)
                    self.send_header("Content-Length", "1000")
                    self.end_headers()
                    self.wfile.write(b"cut")
                    return
                with open(file, "rb") as f:
                    self.send(200, f.read())

            def send(self, status, body):
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *a):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        if tls:
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.load_cert_chain(*tls)
            self.server.socket = ctx.wrap_socket(self.server.socket, server_side=True)
        self.endpoint = f"http{'s' if tls else ''}://127.0.0.1:{self.server.server_address[1]}/acct"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def listing(self, prefix, marker):
        names = sorted(os.path.relpath(os.path.join(d, f), self.root)
                       for d, _, fs in os.walk(self.root) for f in fs)
        names = [n for n in names if n.startswith(prefix) and n > marker]
        page, rest = names[:self.page], names[self.page:]
        blobs = "".join(f"<Blob><Name>{n}</Name><Properties><Content-Length>1</Content-Length></Properties></Blob>"
                        for n in page)
        nxt = f"<NextMarker>{page[-1]}</NextMarker>" if rest else "<NextMarker />"
        return ("﻿<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults ContainerName=\"data\">"
                f"<Prefix>{prefix}</Prefix><Blobs>{blobs}</Blobs>{nxt}</EnumerationResults>").encode()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


def test_a_blob_store_with_a_sas_names_it_by_secret_in_the_document():
    run = {"job": "score", "period": "2024-01", "id": "score-2024-01"}
    blob = proof.Blob(store.Store("acct", "", "data", "", DATA_SAS), "nyc/", "proof/x/")
    doc, secrets = proof.document(run, blob, 4, 3 * GiB)
    assert doc["object_store"] == {"azure": {"account": "acct", "container": "data",
                                             "sas_token": {"secret": "blob-sas"}}}
    assert secrets == {"blob-sas": DATA_SAS}
    for part in data_secret_parts():
        assert part not in json.dumps(doc)


def test_blob_takes_the_key_or_the_sas_from_the_environment_not_both(tmp_path, monkeypatch):
    a = proof.parse(args(tmp_path))
    a.data, a.blob_account, a.blob_container = "blob", "acct", "data"
    monkeypatch.delenv("BENCH_KEY", raising=False)
    monkeypatch.delenv("BENCH_SAS", raising=False)
    with pytest.raises(SystemExit, match="BENCH_KEY or BENCH_SAS in the environment"):
        proof.data_of(a)
    monkeypatch.setenv("BENCH_SAS", f" ?{DATA_SAS}\n")
    data = proof.data_of(a)
    assert (data.store.key, data.store.sas, data.store.container) == ("", DATA_SAS, "data")
    monkeypatch.setenv("BENCH_KEY", "fake-key==")
    with pytest.raises(SystemExit, match="not both"):
        proof.data_of(a)
    monkeypatch.delenv("BENCH_SAS")
    data = proof.data_of(a)
    assert (data.store.key, data.store.sas) == ("fake-key==", "")


def test_the_data_sas_and_its_signature_alone_are_redacted():
    r = proof.Redact({"BENCH_SAS": DATA_SAS, "FUNCTIONS_SAS": SAS})
    assert r(f"GET /acct/data?{DATA_SAS}") == "GET /acct/data?<BENCH_SAS>"
    assert r(f"sig={DATA_SIG}") == "sig=<BENCH_SAS>"
    assert r("decoded DATAdataDATAdata+data/data= here") == "decoded <BENCH_SAS> here"
    assert r(f"{SAS} {DATA_SAS}") == "<FUNCTIONS_SAS> <BENCH_SAS>"


def test_a_sas_store_downloads_by_list_and_get_page_by_page_and_tries_again(tmp_path):
    root = tmp_path / "container"
    for name, data in (("proof/p/score-2024/part-0.parquet", b"zero"),
                       ("proof/p/score-2024/sub/part-1.parquet", b"one"),
                       ("proof/p/score-2024-01/part-0.parquet", b"other"), ("nyc/a.parquet", b"input")):
        (root / name).parent.mkdir(parents=True, exist_ok=True)
        (root / name).write_bytes(data)
    blob = FakeBlob(root, fail_once="proof/p/score-2024/sub/part-1.parquet")
    try:
        s = store.Store("acct", "", "data", blob.endpoint, f"?{DATA_SAS}")
        assert s.sas_url("proof/a b.parquet", "x=1") == f"{blob.endpoint}/data/proof/a%20b.parquet?x=1&{DATA_SAS}"
        dest = tmp_path / "out"
        got = s.sas_download("proof/p/score-2024/", str(dest), wait_s=0)
        assert got == len(b"zero") + len(b"one")
        assert sorted(os.path.relpath(os.path.join(d, f), dest) for d, _, fs in os.walk(dest) for f in fs) == \
            ["part-0.parquet", "sub/part-1.parquet"]
        assert (dest / "sub/part-1.parquet").read_bytes() == b"one"
        lists = [r for r in blob.requests if "comp=list" in r]
        assert len(lists) == 2, "a page of one name, then the next from its marker"
        assert all(r.endswith(DATA_SAS) for r in blob.requests), "the SAS is every request's query"
        assert sum("part-1.parquet" in r for r in blob.requests) == 2, "the cut-short Get is tried again"
        # A blob alone, by its own name.
        single = tmp_path / "single"
        assert store.Store("acct", "", "data", blob.endpoint, DATA_SAS).download("nyc/a.parquet", str(single)) == 5
        assert (single / "a.parquet").read_bytes() == b"input"
        # A SAS the store refuses: the download fails with the store's answer.
        with pytest.raises(OSError, match="403"):
            store.Store("acct", "", "data", blob.endpoint, "sp=racwl&sig=wrong").download("nyc/", str(tmp_path / "x"))
    finally:
        blob.close()


# --- The CA roots the SAS path verifies Blob's certificate against (work item 3.8c) -------------

def certificate(root, name):
    """A self-signed certificate for 127.0.0.1 and its key, as PEM files."""
    key, cert = root / f"{name}.key.pem", root / f"{name}.pem"
    subprocess.run([OPENSSL, "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
                    "-keyout", str(key), "-out", str(cert), "-days", "1", "-subj", f"/CN={name}",
                    "-addext", "subjectAltName=IP:127.0.0.1"], check=True, capture_output=True)
    return str(cert), str(key)


def no_roots():
    """A context as Python's default is where OpenSSL's default paths hold nothing (python.org's
    build for macOS): it verifies the certificate and the name, and has no root to verify against."""
    return ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)


@pytest.fixture
def own_context(monkeypatch):
    """The store's context is made once for a run; a test makes its own, from a context with no
    roots and the bundles it names: `own_context(bundles)`."""
    real = store.ca_context

    def use(bundles):
        store.tls_context.cache_clear()
        monkeypatch.setattr(store, "ca_context", lambda: real(no_roots(), bundles))
    store.tls_context.cache_clear()
    yield use
    store.tls_context.cache_clear()


@pytest.mark.skipif(not OPENSSL, reason="needs openssl to make a certificate")
def test_a_context_with_no_default_roots_falls_back_to_the_first_bundle_there_is(tmp_path, own_context):
    cert, key = certificate(tmp_path, "blob")
    other, _ = certificate(tmp_path, "other")
    (tmp_path / "empty.pem").write_text("not a certificate\n")
    bundles = [("SSL_CERT_FILE", ""), ("a file that is not there", str(tmp_path / "none.pem")),
               ("a file with no certificate", str(tmp_path / "empty.pem")), ("the system's bundle", cert),
               ("certifi", other)]
    ctx = store.ca_context(no_roots(), bundles)
    # The first bundle that has roots, and no other; the certificate and the name still verified.
    assert ctx.cert_store_stats()["x509"] == 1
    assert ctx.verify_mode == ssl.CERT_REQUIRED and ctx.check_hostname
    root = tmp_path / "container"
    (root / "proof/p/score-2024").mkdir(parents=True)
    (root / "proof/p/score-2024/part-0.parquet").write_bytes(b"rows")
    blob = FakeBlob(root, page=5, tls=(cert, key))
    try:
        assert blob.endpoint.startswith("https://")
        s = store.Store("acct", "", "data", blob.endpoint, DATA_SAS)
        # The store's own opener, no opener given: over HTTPS it verifies against that context.
        own_context(bundles)
        assert s.sas_names("proof/p/") == ["proof/p/score-2024/part-0.parquet"]
        assert s.download("proof/p/score-2024/", str(tmp_path / "out")) == len(b"rows")
        assert (tmp_path / "out/part-0.parquet").read_bytes() == b"rows"
        # Roots that did not sign the store's certificate: the store is refused, not trusted.
        own_context(bundles[4:])
        with pytest.raises(OSError, match="CERTIFICATE_VERIFY_FAILED"):
            s.sas_names("proof/p/")
    finally:
        blob.close()


def test_no_roots_anywhere_is_an_error_that_names_what_was_tried(tmp_path, own_context):
    (tmp_path / "empty.pem").write_text("not a certificate\n")
    bundles = [("SSL_CERT_FILE", ""), ("the system's bundle", str(tmp_path / "none.pem")),
               ("certifi", str(tmp_path / "empty.pem"))]
    with pytest.raises(store.NoCaRoots) as stop:
        store.ca_context(no_roots(), bundles)
    why = str(stop.value)
    assert why.startswith("no CA roots to verify the store's certificate against: Python's default paths (")
    assert "SSL_CERT_FILE (there is none)" in why
    assert f"the system's bundle, {tmp_path / 'none.pem'} (No such file or directory)" in why
    assert f"certifi, {tmp_path / 'empty.pem'} (" in why and why.endswith("name a CA bundle in SSL_CERT_FILE")
    # An https:// address is not opened without roots: the error is the download's.
    own_context(bundles)
    with pytest.raises(store.NoCaRoots, match="nor did SSL_CERT_FILE"):
        store.Store("acct", "", "data", "", DATA_SAS).download("proof/p/", str(tmp_path / "x"))
    assert not (tmp_path / "x").exists()


def test_where_the_roots_are_looked_for_and_a_default_that_has_them(tmp_path, monkeypatch):
    found = store.ca_bundles({"SSL_CERT_FILE": "/my/roots.pem"})
    assert [what for what, _ in found] == ["SSL_CERT_FILE", "the system's bundle", "certifi"]
    assert found[0][1] == "/my/roots.pem" and found[1][1] == "/etc/ssl/cert.pem"
    assert store.ca_bundles({})[0] == ("SSL_CERT_FILE", "")
    # certifi's bundle when certifi is installed, and none when it is not.
    monkeypatch.setitem(sys.modules, "certifi", types.SimpleNamespace(where=lambda: "/site/cacert.pem"))
    assert store.ca_bundles({})[2] == ("certifi", "/site/cacert.pem")
    monkeypatch.setitem(sys.modules, "certifi", None)
    assert store.ca_bundles({})[2] == ("certifi", "")

    # A context that has roots is taken as it is: no bundle is read.
    class Loaded:
        def cert_store_stats(self):
            return {"x509": 3, "crl": 0, "x509_ca": 3}

        def load_verify_locations(self, cafile):
            raise AssertionError(f"{cafile} was read")
    ctx = Loaded()
    assert store.ca_context(ctx, [("the system's bundle", "/etc/ssl/cert.pem")]) is ctx
    # Python's default context: with a directory of roots at OpenSSL's default path it is taken
    # as it is (the roots are read as they are needed); with none anywhere, the error.
    made = []
    monkeypatch.setattr(store.ssl, "create_default_context", lambda: made.append(no_roots()) or made[-1])
    (tmp_path / "certs").mkdir()
    (tmp_path / "certs" / "4042bcee.0").write_text("a root, by its hash\n")
    paths = types.SimpleNamespace(capath=str(tmp_path / "certs"), openssl_cafile="/openssl/cert.pem")
    monkeypatch.setattr(store.ssl, "get_default_verify_paths", lambda: paths)
    assert store.ca_context(bundles=[]) is made[-1]
    paths.capath = None
    with pytest.raises(store.NoCaRoots, match=r"default paths \(/openssl/cert.pem\) gave none"):
        store.ca_context(bundles=[])


def test_an_opener_that_is_given_is_the_one_used_and_plain_http_needs_no_roots(tmp_path, monkeypatch):
    """A fake opener works as it did: the store's own context is not made for it, nor for a
    local emulator's plain HTTP."""
    def never():
        raise AssertionError("the roots were looked for")
    store.tls_context.cache_clear()
    monkeypatch.setattr(store, "ca_context", never)
    listing = (b"<EnumerationResults><Blobs><Blob><Name>proof/p/a.parquet</Name></Blob>"
               b"<Blob><Name>proof/p/dir/</Name></Blob></Blobs><NextMarker /></EnumerationResults>")
    opened = []

    class Answer(io.BytesIO):
        headers = {}

    def opener(url, timeout):
        opened.append((url, timeout))
        return Answer(listing if "comp=list" in url else b"rows")
    s = store.Store("acct", "", "data", "", DATA_SAS)
    assert s.sas_names("proof/p/", opener) == ["proof/p/a.parquet", "proof/p/dir/"]
    assert s.sas_download("proof/p/", str(tmp_path / "out"), opener=opener) == len(b"rows")
    assert (tmp_path / "out/a.parquet").read_bytes() == b"rows"
    assert opened[0] == ("https://acct.blob.core.windows.net/data?restype=container&comp=list"
                         f"&prefix=proof%2Fp%2F&{DATA_SAS}", 120)
    assert opened[-1] == (f"https://acct.blob.core.windows.net/data/proof/p/a.parquet?{DATA_SAS}", 120)
    # The store's own opener over plain HTTP.
    root = tmp_path / "container"
    (root / "nyc").mkdir(parents=True)
    (root / "nyc/a.parquet").write_bytes(b"input")
    blob = FakeBlob(root)
    try:
        assert store.Store("acct", "", "data", blob.endpoint, DATA_SAS).sas_names("nyc/") == ["nyc/a.parquet"]
    finally:
        blob.close()


def test_a_whole_proof_from_and_to_blob_with_the_data_sas(world, monkeypatch):
    """As openstack-up.sh --proof runs it: the jobs read and write the data container with its
    SAS by secret name, the outputs are fetched with it, and it is never said or written."""
    container = world / "container"
    node = FakeNode(str(container), {"score": expected("score"), "enrich": expected("enrich")})
    blob = FakeBlob(container, page=5)
    try:
        monkeypatch.delenv("BENCH_KEY", raising=False)
        monkeypatch.setenv("BENCH_SAS", DATA_SAS)
        monkeypatch.setenv("FUNCTIONS_SAS", SAS)
        argv = args(world)
        at = argv.index("--data")
        argv[at:at + 10] = ["--data", "blob", "--blob-account", "acct", "--blob-container", "data",
                            "--blob-endpoint", blob.endpoint, "--blob-outputs", "proof/"]
        argv[argv.index("--function-base") + 1] = BLOB_BASE
        said = []
        rc = proof.main(argv, transport=node, sign=lambda d: f"SIG-{d[:8]}", log=said.append,
                        sleep=lambda s: None, clock=lambda: 0.0)
    finally:
        blob.close()
    assert rc == 0, said
    for job in node.jobs.values():
        body = job["doc"]
        assert body["v1"]["object_store"]["azure"]["sas_token"] == {"secret": "blob-sas"}
        assert "access_key" not in body["v1"]["object_store"]["azure"]
        assert body["secrets"] == {"blob-sas": DATA_SAS}
        assert body["v1"]["source"]["url"][0].startswith("az://data/nyc/")
        assert body["v1"]["sink"]["url"] == f"az://data/proof/{job['run']}/"
    for job in ("score", "enrich"):
        r = result(world, f"{job}-2024-01")
        assert r["output_check"]["verdict"] == "bit for bit"
        # A run's record names where its output is: what --check-only is given to check it again.
        assert r["output"] == f"az://data/proof/{job}-2024-01/" and "check_only" not in r
    assert json.load(open(world / "res" / "proof.json"))["check_only"] is False
    assert any("comp=list&prefix=proof%2Fenrich-2024-01%2F" in r for r in blob.requests)
    for part in data_secret_parts() + secret_parts():
        assert part not in "\n".join(said) and part not in written(world)


# --- --check-only: the outputs a proof's jobs wrote already (work item 3.8c) --------------------

def outputs(container, tables, prefix="proof/p/"):
    """Each run's output in the container, where its job wrote it: PREFIX and the run's name."""
    for run, table in tables.items():
        out = container / prefix / run
        out.mkdir(parents=True)
        pq.write_table(table, out / "part-0.parquet")


def check_only(world, blob, monkeypatch, *extra, jobs="score,enrich", sas=DATA_SAS):
    """proof.py --check-only over the fake Blob, with the data's SAS in the environment. There is
    no node: one asked, a signature made or a wait fails the test."""
    def no_node(*a, **k):
        raise AssertionError("--check-only asked a node")
    monkeypatch.delenv("BENCH_KEY", raising=False)
    monkeypatch.delenv("FUNCTIONS_SAS", raising=False)
    monkeypatch.setenv("BENCH_SAS", sas)
    argv = ["--check-only", "--data", "blob", "--blob-account", "acct", "--blob-container", "data",
            "--blob-endpoint", blob.endpoint, "--blob-outputs", "proof/p/", "--jobs", jobs, "--periods", "2024-01",
            "--references", str(world / "ref"), "--results", str(world / "res"),
            "--work", str(world / "work")] + list(extra)
    said = []
    rc = proof.main(argv, transport=no_node, sign=no_node, log=said.append, sleep=no_node, clock=lambda: 0.0)
    return rc, said


def test_check_only_passes_on_the_outputs_a_proof_wrote(world, monkeypatch):
    container = world / "container"
    outputs(container, {"score-2024-01": expected("score"), "enrich-2024-01": expected("enrich")})
    # Another proof's outputs, under another prefix, are not these.
    outputs(container, {"score-2024-01": expected("enrich")}, prefix="proof/q/")
    blob = FakeBlob(container, page=5)
    try:
        rc, said = check_only(world, blob, monkeypatch)
    finally:
        blob.close()
    assert rc == 0, said
    for job in ("score", "enrich"):
        r = result(world, f"{job}-2024-01")
        assert r["ok"] and r["output_check"]["verdict"] == "bit for bit"
        assert r["output_check"]["rows_mine"] == expected(job).num_rows and r["canon"]["by"] == "canon.py"
        # Where a run of the proof named its output, and nothing of a job: none ran.
        assert r["output"] == f"az://data/proof/p/{job}-2024-01/" and r["check_only"] and r["data"] == "blob"
        assert not {"job_id", "state", "exit", "times", "events_by"} & set(r)
    s = json.load(open(world / "res" / "proof.json"))
    assert s["ok"] and s["check_only"] and s["node"] is None and s["events"] is None
    assert [(x["id"], x["ok"], x["verdict"]) for x in s["runs"]] == [
        ("score-2024-01", True, "bit for bit"), ("enrich-2024-01", True, "bit for bit")]
    # Each output is fetched as a run fetches it: List Blobs under PREFIX and RUN/, then its blob.
    lists = [r for r in blob.requests if "comp=list" in r]
    assert [r.split("prefix=")[1].split("&")[0] for r in lists] == ["proof%2Fp%2Fscore-2024-01%2F",
                                                                   "proof%2Fp%2Fenrich-2024-01%2F"]
    assert len(blob.requests) == 4 and not any("proof/q/" in r for r in blob.requests)
    assert said[0].startswith("proof: check only: the outputs under az://data/proof/p/ against the references in ")
    assert "score-2024-01: pass: bit for bit" in said[1] and "bit for bit" in said[-1]
    assert not os.listdir(world / "work")
    for part in data_secret_parts():
        assert part not in "\n".join(said) and part not in written(world)


def test_check_only_fails_when_an_output_is_missing(world, monkeypatch):
    container = world / "container"
    outputs(container, {"score-2024-01": expected("score")})
    blob = FakeBlob(container, page=5)
    try:
        rc, said = check_only(world, blob, monkeypatch)
    finally:
        blob.close()
    assert rc == 1
    assert result(world, "score-2024-01")["ok"]
    r = result(world, "enrich-2024-01")
    assert not r["ok"] and r["output_check"] == {"ok": False, "verdict": "no canonical form: no output files"}
    s = json.load(open(world / "res" / "proof.json"))
    assert not s["ok"] and [x["ok"] for x in s["runs"]] == [True, False]
    assert any("enrich-2024-01: FAIL: no canonical form: no output files" in line for line in said)


def test_check_only_fails_when_an_output_is_wrong(world, monkeypatch):
    t = expected("enrich")
    changed = t.set_column(t.schema.get_field_index("tip_share"), "tip_share",
                           pa.array([None if v is None else v + 1.0 for v in t["tip_share"].to_pylist()]))
    container = world / "container"
    outputs(container, {"score-2024-01": expected("score"), "enrich-2024-01": changed})
    blob = FakeBlob(container, page=5)
    try:
        rc, said = check_only(world, blob, monkeypatch, "--keep-output")
    finally:
        blob.close()
    assert rc == 1
    assert result(world, "score-2024-01")["ok"]
    r = result(world, "enrich-2024-01")
    assert not r["ok"] and r["output_check"]["verdict"] == "DIFFERS" and r["output_check"]["rows"]
    assert not json.load(open(world / "res" / "proof.json"))["ok"]
    assert any("enrich-2024-01: FAIL: DIFFERS" in line for line in said)
    # The output that differs is kept where it was fetched to, as a run keeps it.
    assert os.listdir(world / "work" / "enrich-2024-01") == ["part-0.parquet"]


def test_check_only_with_an_output_that_cannot_be_fetched_fails_and_says_why(world, monkeypatch):
    """A store that refuses the SAS (one past its time, say) fails the run, without the SAS."""
    container = world / "container"
    outputs(container, {"score-2024-01": expected("score")})
    blob = FakeBlob(container, page=5)
    refused = DATA_SAS.replace("sp=racwl", "sp=r")
    try:
        rc, said = check_only(world, blob, monkeypatch, jobs="score", sas=refused)
    finally:
        blob.close()
    r = result(world, "score-2024-01")
    assert rc == 1 and not r["ok"] and r["output_check"]["verdict"].startswith("output not fetched: ")
    assert "403" in r["output_check"]["verdict"]
    for part in (refused, DATA_SIG, "DATAdataDATAdata+data/data="):
        assert part not in "\n".join(said) and part not in written(world)


@pytest.mark.skipif(not (GATE and os.path.isdir(os.path.join(NYC, "out066"))),
                    reason="GATE_CANON and 0.6.6's January outputs (references.py --keep-output) under BENCH_NYC")
def test_check_only_with_gate_canon_passes_on_0_6_6_s_own_january_outputs(tmp_path, monkeypatch):
    """As the PM runs it after a proof: the four jobs' outputs fetched from Blob with the SAS and
    checked by gate-canon against 0.6.6's references. The container here is the references'
    directory, its `out066/` standing for the proof's prefix."""
    blob = FakeBlob(NYC, page=3)
    monkeypatch.delenv("BENCH_KEY", raising=False)
    monkeypatch.setenv("BENCH_SAS", DATA_SAS)
    said = []
    try:
        rc = proof.main(["--check-only", "--data", "blob", "--blob-account", "acct", "--blob-container", "data",
                         "--blob-endpoint", blob.endpoint, "--blob-outputs", "out066/", "--periods", "2024-01",
                         "--references", NYC, "--gate-canon", GATE, "--results", str(tmp_path / "res"),
                         "--work", str(tmp_path / "work")], log=said.append)
    finally:
        blob.close()
    assert rc == 0, said
    s = json.load(open(tmp_path / "res" / "proof.json"))
    assert [(x["id"], x["verdict"]) for x in s["runs"]] == [(f"{job}-2024-01", "bit for bit") for job in proof.JOBS]
    assert result(tmp_path, "score-2024-01")["canon"]["by"] == "gate-canon"


def test_what_check_only_needs_and_what_a_proof_against_a_node_needs(tmp_path):
    only = ["--check-only", "--data", "blob", "--blob-account", "acct", "--blob-container", "data",
            "--jobs", "score", "--periods", "2024-01"]
    # The prefix the outputs are under: a time of now names none.
    with pytest.raises(SystemExit, match="--check-only needs --data blob and --blob-outputs"):
        proof.parse(only)
    # A sink volume holds its last run's output alone.
    with pytest.raises(SystemExit, match="--check-only needs --data blob and --blob-outputs"):
        proof.parse(["--check-only", "--data", "volume", "--blob-outputs", "proof/p/"])
    a = proof.parse(only + ["--blob-outputs", "proof/p/"])
    assert a.check_only and a.blob_outputs == "proof/p/" and (a.url, a.pin, a.function_key) == (None, None, None)
    # The year's check needs gate-canon, as in a proof.
    with pytest.raises(SystemExit, match="needs --gate-canon"):
        proof.parse(only + ["--blob-outputs", "proof/p/", "--periods", "2024"])
    # A proof against a node still needs the node and the functions, each named when it is missing.
    argv = args(tmp_path)
    for name in ("--url", "--function-base"):
        at = argv.index(name)
        with pytest.raises(SystemExit, match=f"proof.py: {name} must be given"):
            proof.parse(argv[:at] + argv[at + 2:])
    with pytest.raises(SystemExit, match="--url, --pin, --function-key, --functions, --function-base must be given"):
        proof.parse(["--data", "blob"])
    # Its outputs go under proof/ and the time it started, when no prefix is given.
    a = proof.parse(argv)
    assert not a.check_only and a.blob_outputs.startswith("proof/2") and a.blob_outputs.endswith("/")
    assert proof.parse(argv + ["--blob-outputs", "proof/mine/"]).blob_outputs == "proof/mine/"
