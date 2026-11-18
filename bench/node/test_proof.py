"""proof.py's tests (work item 3.8, phase 1): the whole proof against a fake node, through the
same calls a node answers (functions, jobs, events over SSE and MCP, reports), its outputs the
gate's fixtures written where the sink volume's files are read back; a hash that differs, a job
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
    where the sink volume's files are read back (`out_root/proof/RUN/`), and its stream holds five
    records, its report and its exit."""

    def __init__(self, out_root, outputs, fail=(), drop_after=None, busy=0, hang=(), listed=None, page=2,
                 mcp_errors=0):
        self.out_root, self.outputs, self.fail, self.drop_after = out_root, outputs, set(fail), drop_after
        self.busy, self.hang, self.listed, self.page, self.mcp_errors = busy, set(hang), listed or {}, page, mcp_errors
        self.calls, self.streams, self.jobs, self.registered, self.cancelled = [], [], {}, {}, []

    # The transport: (method, path, body, token) -> (status, bytes).
    def __call__(self, method, path, body=None, token=None):
        self.calls.append((method, path, body, token))
        ok = lambda obj, status=200: (status, json.dumps(obj).encode())  # noqa: E731
        if path == "/v1/node":
            return ok({"memory_bytes": 4 * GiB, "instance": "i-1", "cloud": "none", "version": "0.7.0"})
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
        return ok({"id": jid, "state": job["state"], "submitted": 100, "started": 101, "ended": 104,
                   "exit_code": job["exit"]["code"]})

    def start(self, body):
        doc = body["v1"]
        run = doc["sink"]["url"].rstrip("/").split("/")[-1]
        job = run.split("-")[0]
        jid = f"j{len(self.jobs) + 1}"
        failed = run in self.fail
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


def args(tmp_path, *extra, jobs="score,enrich"):
    return ["--url", "https://node:443", "--pin", "ab" * 32, "--token", "T", "--function-key", "/k.pem",
            "--functions", str(tmp_path / "fn"), "--function-base", "https://code.example/fn",
            "--data", "volume", "--source-serial", "nyc-in", "--sink-serial", "nyc-out",
            "--volume-out", str(tmp_path / "vol"), "--jobs", jobs, "--periods", "2024-01",
            "--references", str(tmp_path / "ref"), "--results", str(tmp_path / "res"),
            "--work", str(tmp_path / "work")] + list(extra)


@pytest.fixture
def world(tmp_path):
    for d in ("fn", "ref", "vol"):
        (tmp_path / d).mkdir()
    functions(tmp_path / "fn")
    references(tmp_path / "ref")
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
    doc, secrets = proof.document(run, proof.Volumes("in-1", "out-1", "/x"), "https://f", 4, 3 * GiB)
    assert doc["source"]["url"][0] == "disk://in-1/yellow_tripdata_2024-01.parquet" and len(doc["source"]["url"]) == 12
    assert doc["sink"]["url"] == "disk://out-1/proof/occupancy-2024/" and secrets == {}
    assert doc["kernels"][0]["image"]["location"] == "https://f/gate-occupancy"
    assert doc["budget"] == {"memory_bytes": 3 * GiB, "cpu": 4} and "object_store" not in doc
    blob = proof.Blob(store.Store("acct", "KEY", "data", "http://127.0.0.1:10000/acct"), "nyc/", "proof/x/")
    doc, secrets = proof.document(run, blob, "https://f", 4, 3 * GiB)
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
        assert r["records"] == 5 and r["first_rows_s"] == 1.5 and r["times"]["ended"] == 104
        assert r["output_check"]["rows_mine"] == expected(job).num_rows
    s = json.load(open(world / "res" / "proof.json"))
    assert s["ok"] and [x["id"] for x in s["runs"]] == ["score-2024-01", "enrich-2024-01"]
    # Each function registered once, by its image's SHA-256 and its signature, with the admin token.
    digest = hashlib.sha256(b"a static program for score").hexdigest()
    assert node.registered["gate-score"] == {"name": "gate-score", "address": "https://code.example/fn/gate-score",
                                             "sha256": digest, "signature": f"SIG-{digest[:8]}"}
    # The budget is the most the node takes: its memory less 768 MiB.
    doc = node.jobs["j1"]["doc"]["v1"]
    assert doc["budget"]["memory_bytes"] == 4 * GiB - 768 * (1 << 20)
    assert doc["source"]["url"] == ["disk://nyc-in/yellow_tripdata_2024-01.parquet"]
    assert node.streams == ["/v1/jobs/j1/events?from=0", "/v1/jobs/j2/events?from=0"]
    assert "bit for bit" in said[-1] and not os.listdir(world / "work")


def test_a_function_the_node_lists_already_is_not_registered_again(world):
    digest = hashlib.sha256(b"a static program for score").hexdigest()
    node = FakeNode(str(world / "vol"), {"score": expected("score")}, listed={"gate-score": digest})
    rc, said = proof_run(world, node, jobs="score")
    assert rc == 0 and node.registered == {} and any("registered already" in s for s in said)


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


def test_comparisons_that_cannot_be_made(tmp_path):
    ref = {"rows": 3, "hash_exact": "a", "hash_all": "b"}
    assert proof.compare("score", {"error": "x"}, ref, "/r")["verdict"] == "no canonical form: x"
    c = proof.compare("score", {"rows": 3, "hash_exact": "a", "hash_all": "c", "by": "gate-canon"}, ref, "/r")
    assert not c["ok"] and c["verdict"].startswith("not compared: no tolerant rows")
    assert proof.compare("audit", {"rows": 2, "hash_exact": "a", "hash_all": "b"}, ref, "/r")["verdict"] == \
        "DIFFERS: rows"
    assert proof.canonical("score", str(tmp_path))["error"] == "no output files"
    bad = proof.canonical("score", str(tmp_path), gate_canon=sys.executable)
    assert bad["error"]


def test_the_volume_fetch_and_the_blob_fetch(tmp_path):
    v = proof.Volumes("in", "out", str(tmp_path))
    with pytest.raises(FileNotFoundError):
        v.fetch({"id": "score-2024-01"}, str(tmp_path / "d"))

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
