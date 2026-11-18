"""proof.py's tests (work item 3.8): the whole proof against a fake node, through the same
calls a node answers (functions, jobs, events over SSE and MCP, reports), its outputs the gate's
fixtures written as the sink volume's only layer and read back by a stand-in for `moruna-engine
volume unpack`; a hash that differs, a job
that fails, a lost connection resumed after its last record, a job past its time, a busy node;
and a real TLS stream that drops. Nothing here reaches a node or a cloud.

    python3 -m pytest -q bench/node/test_proof.py
"""
import hashlib
import json
import os
import shutil
import subprocess
import sys
import threading

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
