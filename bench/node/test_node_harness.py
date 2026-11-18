"""The node benchmark harness's tests (work item 6.1, phase 1): the plans, the `az` commands, the
dry run, the whole run against fakes (teardown included), the collector, the table, the
root-cause log, the node's API calls, and the engines' scripts on the gate's fixtures. Nothing
here reaches a cloud.

    python3 -m pytest -q bench/node/test_node_harness.py
"""
import base64
import io
import json
import os
import shutil
import subprocess
import sys

import pyarrow as pa
import pyarrow.ipc as ipc
import pyarrow.parquet as pq
import pytest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(1, os.path.join(HERE, "engines"))
sys.path.insert(2, os.path.join(HERE, "..", "vm"))
import azure  # noqa: E402
import collect  # noqa: E402
import install  # noqa: E402
import measure  # noqa: E402
import node_api  # noqa: E402
import plan as plans  # noqa: E402
import rootcause  # noqa: E402
import run  # noqa: E402
import table  # noqa: E402

FIX = os.path.join(HERE, "..", "..", "images", "gate", "fixtures")
OPENSSL = node_api.OPENSSL if os.path.exists(node_api.OPENSSL) else shutil.which("openssl")


def nyc_plan():
    return plans.load(plans.default_plan_path("nyc-2024"))


# --- Plans -------------------------------------------------------------------------------------

def test_the_nyc_plan_expands_in_interleaved_order():
    p = nyc_plan()
    runs = plans.runs(p)
    ids = [r["id"] for r in runs]
    # 9 engines on Score and Audit, 10 on Occupancy (DuckDB SQL), 11 on Enrich (and Polars
    # expressions): a learning run and 5 repeats each, and 4 noise runs a job.
    assert len(ids) == len(set(ids)) == (9 + 10 + 11 + 9) * 6 + 4 * 4
    score = [r for r in runs if r["job"] == "score"]
    plain = [e for e in p["engines"] if e not in ("duckdb-sql", "polars-expr")]
    assert [r["engine"] for r in score if r["repeat"] == 0][0] == plans.REFERENCE
    assert [r["engine"] for r in score if r["repeat"] == 1] == plain
    assert [r["engine"] for r in score if r["repeat"] == 2] == plain[::-1]
    enrich = [r["engine"] for r in runs if r["job"] == "enrich" and r["repeat"] == 1]
    occupancy = [r["engine"] for r in runs if r["job"] == "occupancy" and r["repeat"] == 1]
    assert enrich == p["engines"] and "duckdb-sql" in occupancy and "polars-expr" not in occupancy
    assert sum(1 for r in score if r.get("noise")) == 4
    first = next(r for r in score if r["engine"] == "moruna066" and r["repeat"] == 0)
    assert first["fresh_profiles"] and first["profiles"] == "/data/profiles/score-moruna066"
    assert not next(r for r in score if r["engine"] == "moruna066" and r["repeat"] == 1).get("fresh_profiles")
    assert all(r["cpus"] == 4 and r["mem_gib"] == 14 and r["data"] == "blob" for r in runs)
    assert runs[0]["inputs"][0] == "nyc/yellow_tripdata_2024-01.parquet" and len(runs[0]["inputs"]) == 12
    assert runs[0]["output"] == f"out/{runs[0]['id']}"
    assert {r["where"] for r in runs if r["engine"] == "moruna-node"} == {"node"}


def test_the_month_plan():
    p = plans.load(plans.default_plan_path("nyc-month-mac"))
    runs = plans.runs(p)
    assert all(r["repeat"] == 1 for r in runs)
    assert [r["engine"] for r in runs if r["job"] == "enrich"] == p["engines"]


@pytest.mark.parametrize("change,message", [
    (lambda p: p.update(extra=1), "unknown field extra"),
    (lambda p: p.pop("jobs"), "missing jobs"),
    (lambda p: p.update(plan=2), "version 2"),
    (lambda p: p["dataset"].update(files=[]), "dataset.files"),
    (lambda p: p["dataset"].update(files=["a/b.parquet"]), "dataset.files"),
    (lambda p: p["dataset"].update(files=["a.parquet", "a.parquet"]), "named twice"),
    (lambda p: p["dataset"].update(url="x"), "unknown field url"),
    (lambda p: p.update(jobs=["score", "nope"]), "unknown nope"),
    (lambda p: p.update(jobs=["score", "score"]), "given twice"),
    (lambda p: p.update(engines=[]), "non-empty list"),
    (lambda p: p.update(engines=["duckdb"]), "reference"),
    (lambda p: p.update(repeats=0), "repeats"),
    (lambda p: p.update(noise_pairs=-1), "noise_pairs"),
    (lambda p: p["machine"].update(cpus=0), "machine"),
    (lambda p: p.update(machine=[]), "an object"),
    (lambda p: p.update(linux_data="nfs"), "linux_data"),
])
def test_a_bad_plan_is_refused(change, message):
    p = nyc_plan()
    change(p)
    with pytest.raises(plans.PlanError, match=message):
        plans.check(p)


def test_a_plan_that_is_not_json(tmp_path):
    f = tmp_path / "p.json"
    f.write_text("{")
    with pytest.raises(plans.PlanError, match="not JSON"):
        plans.load(str(f))


# --- az commands -------------------------------------------------------------------------------

def cloud():
    return azure.Cloud("202611180900")


def test_names_and_the_guard():
    c = cloud()
    assert c.group == "moruna-bench-20261118"
    assert c.account.isalnum() and c.account.islower() and 3 <= len(c.account) <= 24
    every = [azure.group_create(c), *azure.storage(c), azure.storage_key(c), *azure.containers(c, "K"),
             azure.upload(c, "K", "data", "f", "/f"), azure.sas(c, "K", "data", "r", "2026-11-18T00:00Z"),
             azure.blob_url(c, "K", "vhds", "node.vhd"), azure.node_disk(c, "u", "i"), azure.account_id(c),
             *azure.nsgs(c, "203.0.113.9"), azure.node_ip(c), azure.node_vm(c, "/cfg"), azure.boot_log(c),
             azure.node_address(c), azure.linux_vm(c, "/k.pub"), azure.image_version(c),
             azure.run_command(c, "true", ["A=1"]), azure.group_delete(c), azure.group_exists(c)]
    for cmd in every:
        azure.guard(c, cmd)
        assert cmd[0] == "az"
    azure.guard(c, ["git", "status"])


@pytest.mark.parametrize("cmd", [
    ["az", "group", "delete", "-n", "griot-stg-in", "--subscription", "griot-dev", "--yes"],
    ["az", "vm", "create", "-n", "x", "-g", "moruna-bench-20261118", "--subscription", "griot-stg"],
    ["az", "vm", "create", "-n", "x", "-g", "another", "--subscription", "griot-dev"],
    ["az", "vm", "list"],
    ["az", "storage", "blob", "upload", "--account-name", "someoneelse", "--account-key", "K"],
])
def test_the_guard_refuses_what_is_not_the_run(cmd):
    with pytest.raises(ValueError, match="refused"):
        azure.guard(cloud(), cmd)


def test_commands_are_well_formed():
    c = cloud()
    page = azure.upload(c, "K", "vhds", "node.vhd", "/v", page=True)
    assert page[page.index("--type") + 1] == "page"
    rc = azure.run_command(c, "echo hi", ["SAS=a=b&c"])
    assert rc.index("--parameters") < rc.index("--query") and rc[rc.index("--parameters") + 1] == "SAS=a=b&c"
    assert "--parameters" not in azure.run_command(c, "true")
    node = azure.node_vm(c, "/cfg")
    # AZURE.md, step 6: boot diagnostics into the run's account from power-on, never restarted.
    for flag, value in (("--security-type", "Standard"), ("--accelerated-networking", "false"), ("--zone", "3"),
                        ("--size", "Standard_D4s_v4"), ("--user-data", "/cfg"), ("--public-ip-address", "moruna-node-ip"),
                        ("--boot-diagnostics-storage", c.account)):
        assert node[node.index(flag) + 1] == value
    assert "--no-wait" in node and "--public-ip-sku" not in node
    # Step 5: TCP 443 from the operator's address alone.
    rule = azure.nsgs(c, "203.0.113.9")[1]
    assert rule[rule.index("--destination-port-ranges") + 1] == "443"
    assert rule[rule.index("--source-address-prefixes") + 1] == "203.0.113.9/32"
    ip = azure.node_ip(c)
    assert ip[ip.index("--allocation-method") + 1] == "Static" and ip[ip.index("--zone") + 1] == "3"
    assert "functions" in azure.CONTAINERS
    assert azure.redact(["az", "x", "--account-key", "secret"], {"storage-key": "secret"}) == \
        "az x --account-key <storage-key>"


# --- The dry run -------------------------------------------------------------------------------

def dry(tmp_path, *extra):
    out = io.StringIO()
    assert run.main(["--plan", "nyc-2024", "--dry-run", "--stamp", "202611180900",
                     "--results", str(tmp_path / "results"), *extra], out=out) == 0
    return out.getvalue().splitlines()


def test_the_dry_run_lists_every_command_and_creates_nothing(tmp_path):
    lines = dry(tmp_path)
    steps = [line for line in lines if line.startswith("[")]
    azs = [line.split(" ", 2)[2].strip() for line in steps if line[6:11].strip() == "az"]
    assert azs[0].startswith("az group create -n moruna-bench-20261118")
    assert azs[-2].startswith("az group delete -n moruna-bench-20261118") and azs[-1].startswith("az group exists")
    for line in azs:
        assert "--subscription griot-dev" in line or "--account-name mrnb202611180900" in line, line
    runs = plans.runs(nyc_plan())
    linux = [r for r in runs if r["where"] == "linux"]
    node = [r for r in runs if r["where"] == "node"]
    measured = [a for a in azs if "measure.py run" in a]
    assert [a.split("--id ")[1].split()[0] for a in measured] == [r["id"] for r in linux]
    assert sum("measure.py check" in a for a in azs) == len(node)
    assert sum(" POST https://<node-address>/v1/jobs " in line for line in steps) == len(node)
    # Each job followed over SSE to its exit, as proof.py follows one.
    assert sum("/events?from=0 (Server-Sent Events" in line for line in steps) == len(node)
    # The proof's registration: each task at its SAS address.
    assert sum(" POST https://<node-address>/v1/functions " in line for line in steps) == 4
    assert any('"address":"https://mrnb202611180900.blob.core.windows.net/functions/gate-score?<FUNCTIONS_SAS>"'
               in line for line in steps)
    assert sum("blob upload -c functions -n gate-" in a for a in azs) == 4
    assert any("generate-sas -n functions --permissions r " in a for a in azs)
    assert any("setup-linux.sh" in a for a in azs) and any("--make-reference /data/ref/score.dump" in a for a in azs)
    # Every engine reads and writes the one container; the key reaches the machine redacted here.
    assert all("'BENCH_KEY=<storage-key>'" in a and "BENCH_CONTAINER=data" in a for a in measured)
    assert sum("blob upload -c data -n nyc/yellow_tripdata_2024-" in a for a in azs) == 12
    # The proof's job document: the function by name, Blob by `az://`, the key by secret name.
    job = next(line for line in steps if " POST https://<node-address>/v1/jobs " in line and "score-moruna-node-r1" in line)
    assert '"kernels":[{"kind":"function","function":"gate-score"}]' in job
    assert '"url":"az://data/out/score-moruna-node-r1/"' in job and '"az://data/nyc/yellow_tripdata_2024-12.parquet"' in job
    assert '"access_key":{"secret":"blob-key"}' in job and '"secrets":{"blob-key":"<storage-key>"}' in job
    assert '"budget":{"memory_bytes":"<job-memory-bytes>","cpu":4}' in job
    # AZURE.md's node: never restarted, 443 from the operator alone.
    assert not any("vm restart" in a or "boot-diagnostics enable" in a for a in azs)
    assert any("--source-address-prefixes '<operator-address>/32'" in a for a in azs)
    assert not (tmp_path / "results").exists()


def fake_inputs(tmp_path, disk_mib=2, disk_type=2, cookie=b"conectix", machine=62):
    """A fixed VHD of `disk_mib` MiB and the four gate tasks, as x86_64 ELF headers."""
    vhd = tmp_path / "node.vhd"
    footer = bytearray(512)
    footer[:8] = cookie
    footer[48:56] = (disk_mib << 20).to_bytes(8, "big")
    footer[60:64] = disk_type.to_bytes(4, "big")
    vhd.write_bytes(b"\0" * (disk_mib << 20) + bytes(footer))
    tasks = tmp_path / "tasks"
    tasks.mkdir(exist_ok=True)
    for job in plans.JOBS:
        (tasks / f"gate-{job}").write_bytes(b"\x7fELF" + b"\0" * 14 + machine.to_bytes(2, "little") + b"rest")
    return str(vhd), str(tasks)


def test_the_dry_run_checks_the_image_the_tasks_and_the_address(tmp_path):
    vhd, tasks = fake_inputs(tmp_path)
    lines = dry(tmp_path, "--vhd", vhd, "--tasks", tasks, "--from", "203.0.113.9")
    notes = "\n".join(line for line in lines if line.startswith("      # "))
    assert f"node image: {vhd}, {(2 << 20) + 512} bytes, a fixed VHD of 2 MiB, sha256 " in notes
    assert f"gate tasks: {tasks}: gate-score " in notes and "TCP 443 from 203.0.113.9/32 alone" in notes
    assert "2 machines of Standard_D4s_v4 at once, 8 vCPUs" in notes and "12 files from" in notes
    steps = [line for line in lines if line.startswith("[")]
    # With the tasks given, proof.py registers them: the node's list first, then each by its SHA-256.
    assert any(" GET https://<node-address>/v1/functions" in line for line in steps)
    digest = run.sha256_of(os.path.join(tasks, "gate-score"))
    assert any('"name":"gate-score"' in line and f'"sha256":"{digest}"' in line for line in steps)
    assert any(f"blob upload -c vhds -n node.vhd -f {vhd} --type page" in line for line in steps)
    assert any("--source-address-prefixes 203.0.113.9/32" in line for line in steps)


@pytest.mark.parametrize("kw,extra,message", [
    ({"cookie": b"notavhd!"}, [], "not a VHD"),
    ({"disk_type": 3}, [], "not a fixed VHD"),
    ({"machine": 183}, [], "not an x86_64 ELF"),
    ({}, ["--from", "203.0.113"], "not an IPv4 address"),
])
def test_the_dry_run_refuses_wrong_inputs(tmp_path, kw, extra, message):
    vhd, tasks = fake_inputs(tmp_path, **kw)
    with pytest.raises(SystemExit, match=message):
        dry(tmp_path, "--vhd", vhd, "--tasks", tasks, *extra)


def test_wrong_inputs_are_refused_before_anything_runs(tmp_path):
    vhd, tasks = fake_inputs(tmp_path)
    os.remove(os.path.join(tasks, "gate-audit"))
    with pytest.raises(SystemExit, match="no gate task at .*gate-audit"):
        dry(tmp_path, "--tasks", tasks)
    with pytest.raises(SystemExit, match="no VHD at"):
        dry(tmp_path, "--vhd", str(tmp_path / "none.vhd"))
    odd = tmp_path / "odd.vhd"
    footer = bytearray(open(vhd, "rb").read()[-512:])
    odd.write_bytes(b"\0" * 1000 + bytes(footer))
    with pytest.raises(SystemExit, match="not the file less its footer"):
        dry(tmp_path, "--vhd", str(odd))
    plan = nyc_plan()
    plan["dataset"]["source_dir"] = str(tmp_path / "nodata")
    path = tmp_path / "p.json"
    path.write_text(json.dumps(plan))
    with pytest.raises(SystemExit, match="the plan's data is not all in"):
        run.main(["--plan", str(path), "--dry-run", "--results", str(tmp_path / "r")], out=io.StringIO())


def test_real_mode_needs_its_inputs(tmp_path):
    with pytest.raises(SystemExit):
        run.main(["--plan", "nyc-2024", "--go", "--results", str(tmp_path)])
    vhd, tasks = fake_inputs(tmp_path)
    with pytest.raises(SystemExit, match="needs --vhd, --tasks and --from"):
        run.main(["--plan", "nyc-2024", "--go", "--vhd", vhd, "--tasks", tasks, "--results", str(tmp_path)])
    with pytest.raises(SystemExit):
        run.main(["--plan", "nyc-2024", "--local"])


# --- The whole run, against fakes --------------------------------------------------------------

PIN = "ab" * 32


# A schema 3.1 report, as the node's engine writes it (docs/run-report.md): reads and decodes on
# the source's task, encodes and writes on the sink's, the function's busy time and waits.
REPORT = {"schema_version": "3.1", "wall_ms": 3000.0, "peak_anon_bytes": 3 << 30,
          "resources": {"cpu_core_s": 20.0, "peak_memory_bytes": 3 << 30},
          "budget": {"memory_bytes": 13 << 30}, "object_io": {"room_bytes": 1 << 30, "requests": 64},
          "exit": {"code": 0, "status": "completed", "diagnostic": None},
          "tasks": [{"key": "src", "role": "source", "read_ms": 700.0, "decode_ms": 300.0},
                    {"key": "fn", "role": "function", "busy_ms": 4000.0, "input_wait_ms": 10.0, "output_wait_ms": 20.0},
                    {"key": "out", "role": "sink", "encode_ms": 200.0, "write_ms": 300.0}]}


class FakeNode:
    """The node's API: the calls kept; each job completes, its events streamed to the exit."""

    def __init__(self, refuse=(), fail=()):
        self.calls, self.jobs, self.bodies, self.refuse, self.fail = [], 0, [], refuse, fail

    def __call__(self, method, path, body=None, token=None):
        self.calls.append((method, path, token))
        if path == "/v1/node":
            return 200, json.dumps({"memory_bytes": 16 << 30, "job_memory_bytes": 13 << 30, "instance": "i-1",
                                    "version": "0.1.0"}).encode()
        if path == "/v1/functions":
            return (200, b'{"functions": []}') if method == "GET" else (201, b"{}")
        if path == "/v1/jobs":
            self.jobs += 1
            self.bodies.append(body)
            if self.jobs in self.refuse:
                return 422, b'{"error":{"code":"invalid","message":"no"}}'
            return 201, json.dumps({"id": f"j{self.jobs}"}).encode()
        if path.endswith("/cancel"):
            return 200, b"{}"
        return 200, json.dumps({"id": path.split("/")[3], "state": "completed", "submitted": 101,
                                "submitted_ms": 101_250, "started": 101, "started_ms": 101_999, "ended": 105,
                                "ended_ms": 105_004, "exit_code": 0}).encode()

    def stream(self, method, path, token=None):
        self.calls.append((method, path, token))
        n = int(path.split("/")[3][1:])
        if n in self.fail:
            raise node_api.ApiError(f"{method} {path}: 404 b'no'")
        yield from ["id: 1", "event: event", 'data: {"type":"heartbeat","t_ms":500,"rows_out":0}', "",
                    "id: 2", "event: event", 'data: {"type":"event","kind":"task_started","task":"sink","t_us":1000000}',
                    "", "event: report", "data: " + json.dumps({"type": "report", "report": REPORT, "plan": None}), "",
                    "event: exit", 'data: {"type":"exit","code":0,"diagnostic":null}', ""]


class Fake(run.DryRun):
    """A run's executor whose cloud is a dictionary: what a real run would learn, made up."""

    def __init__(self, results, fail_on=None, node=None, console_after=0):
        super().__init__(io.StringIO())
        self.results, self.fail_on, self.azs, self.node = results, fail_on, [], node or FakeNode()
        self.console_after = console_after

    def az(self, cmd, out=None, secrets=None):
        azure.guard(azure.Cloud("202611180900"), cmd)
        text = " ".join(cmd)
        self.azs.append(text)
        if self.fail_on and self.fail_on in text:
            raise RuntimeError(f"failed: {self.fail_on}")
        if "keys list" in text:
            return "KEY"
        if "generate-sas" in text:
            return "sv=1&sig=x"
        if "get-boot-log" in text:
            # Made with --no-wait: the first looks may find no machine yet.
            if sum("get-boot-log" in a for a in self.azs) <= self.console_after:
                raise RuntimeError("az: (ResourceNotFound) the machine is not there yet")
            return (f"moruna: cloud azure instance i-1\nmoruna: tls sha256={PIN}\n"
                    "moruna: ready api=10.0.0.4:443 boot_ms=2140\n")
        if "public-ip show" in text:
            return "203.0.113.9"
        if "group exists" in text:
            return "false"
        if "setup-linux.sh" in text:
            return 'SETUP {"installed": {"duckdb": true}, "wheel_066": "built", "wheel_066_s": 1800}'
        if "measure.py run" in text:
            assert "BENCH_KEY=KEY" in text and "BENCH_ACCOUNT=mrnb202611180900" in text
            rid = text.split("--id ")[1].split()[0]
            spec = next(r for r in plans.runs(nyc_plan(), run.DATA) if r["id"] == rid)
            fast = {"moruna066": 9.0, "moruna066-rust": 6.0, "rust-pool": 4.5}.get(spec["engine"], 12.0)
            copy = {"data": "local copy", "copy_in_s": 2.0, "copy_out_s": 1.0} if spec["engine"] == "rust-pool" else \
                {"data": "blob"}
            rec = {"spec": spec, "ok": True, "startup_s": 0.5, "operational_s": fast, "cpu_s": 30.0,
                   "peak_bytes": 2 << 30, "memory_gib": spec["mem_gib"], **copy,
                   "output_check": {"ok": True, "hash_all": True, "verdict": "bit for bit"}}
            os.makedirs(os.path.join(self.results, "results"), exist_ok=True)
            json.dump(rec, open(os.path.join(self.results, "results", f"{rid}.json"), "w"))
            return "MEASURED " + json.dumps({"id": rid, "ok": True, "output": "bit for bit"})
        if "measure.py check" in text:
            return 'MEASURED {"ok": true, "output": "bit for bit"}'
        return f"<{out}>" if out else ""

    def local(self, cmd, out=None):
        return "f" * 64 + "  file" if cmd[0] == "shasum" else ""

    def keygen(self, path):
        return "PUBKEY"

    def sign(self, key, digest):
        return "SIG"

    def tokens(self, key, aud):
        assert aud == "i-1"
        return {s: f"t-{s}" for s in ("observe", "submit", "admin")}

    def connect(self, url, pin):
        assert (url, pin) == ("https://203.0.113.9", PIN)
        self.transport = self.node

    def http(self, method, path, body=None, token=None):
        return self.node(method, path, body, token)

    def poll(self, what, every_s, at_most_s, fn):
        for _ in range(5):
            got = fn()
            if got is not None:
                return got
        return None

    def save(self, path, obj):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        json.dump(obj, open(path, "w"), default=str)

    def finish(self, plan, results, meta, log=True):
        self.logged = log
        self.summary = run.finish(plan, results, meta, log=os.path.join(results, "benchmark-log.md") if log else None)


class Args:
    vhd, tasks, ref066, operator = "/node.vhd", "/tasks", "fix/adaptive-runtime", "203.0.113.9"


def test_a_whole_run_against_fakes(tmp_path):
    p = nyc_plan()
    results = str(tmp_path)
    _, tasks = fake_inputs(tmp_path)
    args = Args()
    args.tasks = tasks
    x = Fake(results, console_after=2)
    run.Bench(p, x, azure.Cloud("202611180900"), args, results, "/secrets").go()
    assert x.azs[-2].startswith("az group delete") and x.azs[-1].startswith("az group exists")
    assert ("POST", "/v1/jobs", "t-submit") in x.node.calls and ("GET", "/v1/node", "t-observe") in x.node.calls
    assert ("POST", "/v1/functions", "t-admin") in x.node.calls and ("GET", "/v1/functions", "t-observe") in x.node.calls
    # The console was looked at again while the machine was not there yet.
    assert sum("get-boot-log" in a for a in x.azs) == 3
    node_rec = json.load(open(tmp_path / "node" / "score-moruna-node-r1.json"))
    # Operational time is the engine's own wall, its report's 3,000 ms; start-up is the job record's
    # submitted_ms to started_ms, 749 ms, though both are the same whole second (item 2.4); the
    # node's job bound (13 GiB) is each job's budget. The end-to-end time by this machine's clock is
    # kept, and the boot.
    assert (node_rec["operational_s"], node_rec["startup_s"], node_rec["ok"]) == (3.0, 0.749, True)
    assert node_rec["operational_by"] == "the engine's report, wall_ms" and node_rec["startup_within_s"] is None
    assert node_rec["startup_by"] == "the node's job record, submitted_ms to started_ms"
    assert node_rec["marks"]["node_times"] == {"submitted": 101.25, "started": 101.999, "ended": 105.004}
    assert node_rec["marks"]["first_rows_s"] == 1.0 and node_rec["marks"]["wall_ms"] == 3000.0
    assert node_rec["marks"]["end_to_end_s"] >= 0 and node_rec["boot_ms"] == 2140
    assert node_rec["memory_gib"] == 13.0 and node_rec["data"] == "blob" and node_rec["cpu_s"] == 20.0
    assert node_rec["split"] == {"read_s": 0.7, "decode_s": 0.3, "function_s": 4.0, "encode_s": 0.2, "write_s": 0.3,
                                 "wait_s": 0.03}
    assert node_rec["output_check"]["ok"] and node_rec["object_io"]["requests"] == 64
    doc = x.node.bodies[0]["v1"]
    assert doc["budget"]["memory_bytes"] == 13 << 30 and doc["kernels"] == [{"kind": "function", "function": "gate-score"}]
    assert x.node.bodies[0]["secrets"] == {"blob-key": "KEY"}
    s = x.summary["jobs"]["score"]["engines"]
    assert s["moruna-node"]["operational_s"] == 3.0 and s["moruna-node"]["runs"] == 5
    assert s["duckdb"]["operational_s"] == 12.0 and s["duckdb"]["output"] == "bit for bit"
    assert "duckdb-sql" not in s and "duckdb-sql" in x.summary["jobs"]["enrich"]["engines"]
    table_text = open(tmp_path / "table.md").read()
    assert ("| Moruna 0.7, the node | rust | 3.0 | 0.75 | 20.0 | 3.00 | 13.00 | blob | 0.7 | 0.3 | 4.0 | 0.2 | 0.3 | "
            "0.0 | bit for bit | 5/5 |") in table_text
    assert "| the plain Rust pool | rust | 4.5 | 0.50 | 30.0 | 2.00 | 14.00 | local copy, 3.0 s apart |" in table_text
    log = open(tmp_path / "benchmark-log.md").read()
    assert log.startswith("# The benchmark log") and "## 202611180900: plan nyc-2024" in log
    # 3.0 s against the Rust pool's 4.5 s is 1.5x: the bar's time holds; its CPU (20 s) is the least.
    assert "**PASS** no regression" in log and "**PASS** the bar: 1.50x" in log
    assert "#### Miss" not in log
    assert json.load(open(tmp_path / "setup.json"))["wheel_066"] == "built"


def test_each_run_lands_as_it_finishes_and_the_table_reads_them_so_far(tmp_path):
    """While the plan runs: Moruna's records and the Linux machine's live ones are on this machine
    run by run, and `--table` makes the table of those so far; the full records replace them."""
    _, tasks = fake_inputs(tmp_path)
    args = Args()
    args.tasks = tasks
    results = tmp_path / "results"
    x = Fake(str(results), fail_on="--id occupancy-moruna066-r0 ")
    with pytest.raises(RuntimeError):
        run.Bench(nyc_plan(), x, azure.Cloud("202611180900"), args, str(results), "/s").go()
    live = sorted(os.listdir(results / "live"))
    assert "score-duckdb-r1.json" in live and "score-noise-p2b.json" in live and len(live) == 9 * 6 - 6 + 4
    rec = json.load(open(results / "live" / "score-duckdb-r1.json"))
    assert rec["live"] and rec["spec"]["id"] == "score-duckdb-r1" and rec["ok"] and rec["memory_gib"] == 14
    # The full records (here written by the fake machine) replace the live ones: one record a run.
    assert len([r for r in collect.load(str(results)) if r["spec"]["job"] == "score"]) == 9 * 6 + 4
    out = io.StringIO()
    assert run.main(["--plan", "nyc-2024", "--table", str(results)], out=out) == 0
    text = out.getvalue()
    assert text.startswith(f"# {9 * 6 + 4} of 250 runs recorded in {results}")
    assert "| DuckDB | python | 12.0 |" in text and "### occupancy" in text
    assert (results / "summary.json").exists() and (results / "table.md").exists()
    with pytest.raises(SystemExit):
        run.main(["--plan", "nyc-2024", "--table", str(tmp_path / "none")], out=io.StringIO())


def test_a_live_record_from_the_summary_line(tmp_path):
    spec = {"id": "score-duckdb-r1", "job": "score", "engine": "duckdb", "repeat": 1, "mem_gib": 14}
    got = {"id": "score-duckdb-r1", "ok": True, "operational_s": 12.5, "startup_s": 0.4, "cpu_s": 40.0,
           "peak_bytes": 1 << 30, "data": "blob", "copy_in_s": None,
           "check": {"ok": True, "hash_all": True, "verdict": "bit for bit"}}
    rec = run.live_record(spec, got)
    assert rec["live"] and rec["ok"] and rec["operational_s"] == 12.5 and "copy_in_s" not in rec
    assert rec["output_check"] == got["check"] and rec["memory_gib"] == 14
    lost = run.live_record(spec, None)
    assert not lost["ok"] and lost["error"] == "no summary line in the run's output"
    # An older summary line, with the verdict alone.
    assert run.live_record(spec, {"ok": True, "output": "DIFFERS"})["output_check"] == {"verdict": "DIFFERS"}
    # measure.py's summary line carries what the live record needs.
    rec = {"spec": spec, "ok": True, "operational_s": 1.0, "memory_gib": 14, "data": "local copy", "copy_in_s": 2.0,
           "output_check": {"ok": True, "hash_all": False, "max_abs": 1e-7, "verdict": "within tolerance", "rows": True}}
    import contextlib
    printed = io.StringIO()
    with contextlib.redirect_stdout(printed):
        measure.record(spec, "score-duckdb-r1", str(tmp_path), rec)
    line = run.measured(printed.getvalue())
    assert line["check"] == {"ok": True, "hash_all": False, "max_abs": 1e-7, "verdict": "within tolerance"}
    assert (line["data"], line["copy_in_s"], line["memory_gib"], line["output"]) == ("local copy", 2.0, 14,
                                                                                     "within tolerance")
    assert len(printed.getvalue()) < 1024


def test_a_job_the_node_refuses_or_loses_is_a_failed_run(tmp_path):
    """The run goes on: a refused job, and one whose events are not followed, are failed runs."""
    _, tasks = fake_inputs(tmp_path)
    args = Args()
    args.tasks = tasks
    x = Fake(str(tmp_path), node=FakeNode(refuse=(1,), fail=(2,)))
    run.Bench(nyc_plan(), x, azure.Cloud("202611180900"), args, str(tmp_path), "/s").go()
    r0 = json.load(open(tmp_path / "node" / "score-moruna-node-r0.json"))
    r1 = json.load(open(tmp_path / "node" / "score-moruna-node-r1.json"))
    assert (r0["ok"], r0["state"]) == (False, "refused") and "422" in r0["error"]
    assert (r1["ok"], r1["state"]) == (False, "not followed") and "404" in r1["error"]
    assert x.summary["jobs"]["score"]["engines"]["moruna-node"]["runs"] == 5
    assert x.summary["jobs"]["score"]["engines"]["moruna-node"]["ok"] == 4


def test_a_job_that_fails_or_times_out(tmp_path, monkeypatch):
    class Failing(FakeNode):
        def stream(self, method, path, token=None):
            self.calls.append((method, path, token))
            yield from ["event: exit", 'data: {"type":"exit","code":3,"diagnostic":"the function failed"}', ""]

        def __call__(self, method, path, body=None, token=None):
            if path.startswith("/v1/jobs/j") and method == "GET":
                return 200, b'{"state": "failed", "submitted": 1, "ended": 2}'
            return super().__call__(method, path, body, token)

    b = run.Bench(nyc_plan(), Fake(str(tmp_path), node=Failing()), azure.Cloud("202611180900"), Args(), str(tmp_path), "/s")
    b.prepare()
    b.provision()
    b.node_ready()
    spec = next(r for r in b.runs if r["where"] == "node")
    rec = b.moruna(spec)
    assert (rec["ok"], rec["state"], rec["diagnostic"]) == (False, "failed", "the function failed")
    assert "output_check" not in rec and not any("measure.py check" in a for a in b.x.azs)

    class Silent(FakeNode):
        def stream(self, method, path, token=None):
            self.calls.append((method, path, token))
            yield from ["id: 1", "event: event", 'data: {"type":"heartbeat"}', ""]

    # The run's deadline by a clock stopped at 0: proof.py's follower, on the real one, finds it passed.
    monkeypatch.setattr(run.time, "time", lambda: 0.0)
    b.x.node = b.node.t = Silent()
    rec = b.moruna(spec)
    assert (rec["ok"], rec["state"]) == (False, "timed out")
    assert ("POST", "/v1/jobs/j1/cancel", "t-submit") in b.node.t.calls


def test_the_group_is_deleted_when_a_step_fails(tmp_path):
    x = Fake(str(tmp_path), fail_on="vm create -n moruna-node")
    with pytest.raises(RuntimeError, match="failed"):
        run.Bench(nyc_plan(), x, azure.Cloud("202611180900"), Args(), str(tmp_path), "/s").go()
    assert x.azs[-2].startswith("az group delete") and x.azs[-1].startswith("az group exists")
    assert not any("run-command" in a for a in x.azs)


def test_what_the_linux_machine_holds_is_collected_when_a_later_step_fails(tmp_path):
    """Hours of runs are not lost with the group: a failure after the setup still collects."""
    x = Fake(str(tmp_path), fail_on="--id score-duckdb-r1 ")
    with pytest.raises(RuntimeError, match="failed"):
        run.Bench(nyc_plan(), x, azure.Cloud("202611180900"), Args(), str(tmp_path), "/s").go()
    assert any("results/linux.tgz" in a for a in x.azs) and x.logged is False
    assert x.azs[-2].startswith("az group delete") and x.azs[-1].startswith("az group exists")
    assert (tmp_path / "summary.json").exists() and not (tmp_path / "benchmark-log.md").exists()
    assert x.summary["jobs"]["score"]["engines"]["moruna066"]["runs"] == 1


def test_a_failed_collection_does_not_hide_the_failure(tmp_path):
    x = Fake(str(tmp_path), fail_on="--id score-duckdb-r1 ")
    real_az = x.az

    def az(cmd, out=None, secrets=None):
        if "linux.tgz" in " ".join(cmd):
            raise RuntimeError("the collection failed too")
        return real_az(cmd, out, secrets)
    x.az = az
    with pytest.raises(RuntimeError, match="failed: --id score-duckdb-r1"):
        run.Bench(nyc_plan(), x, azure.Cloud("202611180900"), Args(), str(tmp_path), "/s").go()
    assert "the partial results were not collected: the collection failed too" in x.out.getvalue()
    assert x.azs[-1].startswith("az group exists")


def test_real_checks_every_az_command_before_it_runs(tmp_path, monkeypatch):
    ran = []
    monkeypatch.setattr(run.subprocess, "run", lambda cmd, **kw: ran.append(cmd) or
                        subprocess.CompletedProcess(cmd, 0, "out\n", ""))
    x = run.Real(cloud(), str(tmp_path / "commands.log"), io.StringIO())
    with pytest.raises(ValueError, match="refused"):
        x.az(["az", "group", "delete", "-n", "griot-stg-in", "--subscription", "griot-dev", "--yes"])
    assert ran == []
    assert x.az(azure.storage_key(cloud())) == "out"
    assert x.az(azure.containers(cloud(), "SECRET")[0], secrets={"storage-key": "SECRET"}) == "out"
    log = (tmp_path / "commands.log").read_text()
    assert "SECRET" not in log and "<storage-key>" in log
    monkeypatch.setattr(run.subprocess, "run", lambda cmd, **kw: subprocess.CompletedProcess(cmd, 1, "", "boom"))
    with pytest.raises(RuntimeError, match="boom"):
        x.local(["false"])
    assert x.poll("x", 0, 0, lambda: None) is None and x.value("v", "a") == "a"
    with pytest.raises(RuntimeError):
        x.value("node-pin", None)
    x.save(str(tmp_path / "d" / "r.json"), {"a": 1})
    x.write(str(tmp_path / "w.txt"), "w")
    assert json.load(open(tmp_path / "d" / "r.json")) == {"a": 1} and (tmp_path / "w.txt").read_text() == "w"


def test_teardown_says_when_the_group_is_still_there(tmp_path, monkeypatch):
    class Stuck(run.Real):
        def az(self, cmd, out=None, secrets=None):
            return "true" if "exists" in cmd else ""
    b = run.Bench(nyc_plan(), Stuck(cloud(), str(tmp_path / "l"), io.StringIO()), cloud(), Args(), str(tmp_path), "/s")
    with pytest.raises(RuntimeError, match="still there"):
        b.teardown()


def test_measured_reads_the_last_summary_line():
    assert run.measured("noise\nMEASURED {\"id\": \"a\"}\n") == {"id": "a"}
    assert run.measured(None) is None and run.measured("nothing") is None


# --- Collector, table, root causes -------------------------------------------------------------

def rec(engine, repeat, ops, cpu=30.0, job="score", ok=True, check=None, noise=None, split=None):
    rid = f"{job}-noise-{noise}" if noise else f"{job}-{engine}-r{repeat}"
    spec = {"id": rid, "job": job, "engine": engine, "repeat": repeat}
    if noise:
        spec["noise"] = noise
    return {"spec": spec, "ok": ok, "operational_s": ops, "startup_s": 0.4, "cpu_s": cpu, "peak_bytes": 1 << 30,
            "oom_kills": 0, "split": split, "output_check": check or {"ok": True, "hash_all": True}}


def small_plan():
    p = nyc_plan()
    p.update(jobs=["score"], engines=["moruna-node", "moruna066", "moruna066-rust", "duckdb"])
    return p


def test_the_collector_takes_medians_of_the_measured_runs(tmp_path):
    recs = [rec("duckdb", 0, 99.0)] + [rec("duckdb", r, v) for r, v in ((1, 10.0), (2, 12.0), (3, 11.0))]
    recs += [rec("moruna066", None, 9.0, noise="p1a"), rec("moruna066", None, 10.0, noise="p1b")]
    recs += [rec("moruna066-rust", 1, None, ok=False, check={"ok": False})]
    recs += [rec("moruna066", 1, 8.0, check={"ok": False}), rec("moruna066", 2, 8.0)]
    for i, r in enumerate(recs):
        json.dump(r, open(tmp_path / f"r{i}.json", "w"))
    (tmp_path / "bad.json").write_text("{")
    (tmp_path / "x.report.json").write_text("{}")
    s = collect.summarise(collect.load(str(tmp_path)), small_plan())
    d = s["jobs"]["score"]["engines"]["duckdb"]
    assert (d["operational_s"], d["runs"], d["ok"], d["output"]) == (11.0, 3, 3, "bit for bit")
    assert s["jobs"]["score"]["engines"]["moruna066"]["output"] == "DIFFERS (1 of 2)"
    assert s["jobs"]["score"]["engines"]["moruna066-rust"]["output"] == "no run passed"
    assert s["jobs"]["score"]["engines"]["moruna-node"]["operational_s"] is None
    assert round(s["jobs"]["score"]["noise"]["spread"], 3) == round(1 / 9.5, 3)
    assert collect.write(str(tmp_path), s).endswith("summary.json")
    assert collect.output_verdict([rec("a", 1, 1.0, check={"ok": True, "hash_all": False, "max_abs": 5e-7})]) == \
        "within tolerance (max 5.0e-07)"
    assert collect.output_verdict([rec("a", 1, 1.0, check={"ok": None})]) == "unchecked"


def summary_of(recs):
    return collect.summarise(recs, small_plan())


def test_the_table_and_its_verdicts():
    split = {"read_decode_s": 2.0, "function_s": 5.0, "encode_write_s": 1.0, "wait_s": 0.5}
    recs = [rec("moruna-node", 1, 2.0, cpu=10.0, split=split), rec("moruna066", 1, 9.0),
            rec("moruna066-rust", 1, 6.0), rec("duckdb", 1, 3.5, cpu=12.0)]
    s = summary_of(recs)
    text = table.render(s)
    # runner_split.py's joined stages stand in the read and write columns, named.
    assert ("| Moruna 0.7, the node | rust | 2.0 | 0.40 | 10.0 | 1.00 |  |  | 2.0 (read and decode) |  | 5.0 |  | "
            "1.0 (encode and write) | 0.5 | bit for bit | 1/1 |") in text
    six = {"read_s": 0.7, "decode_s": 0.3, "function_s": 4.0, "encode_s": 0.2, "write_s": 0.3, "wait_s": 0.5}
    text6 = table.render(summary_of([rec("moruna-node", 1, 2.0, split=six), rec("moruna066", 1, 9.0)]))
    assert "| blob |" not in text6 and "|  | 0.7 | 0.3 | 4.0 | 0.2 | 0.3 | 0.5 | bit for bit | 1/1 |" in text6
    assert text6.splitlines()[2].count("|") == text6.splitlines()[3].count("|") == 17
    assert "**PASS** no regression" in text
    # DuckDB's 3.5 s is 1.75x Moruna's 2.0 s, and Moruna's CPU is the least: the bar holds.
    assert "**PASS** the bar: 1.75x" in text
    slow = summary_of([rec("moruna-node", 1, 7.0, cpu=40.0, split=split), rec("moruna066", 1, 9.0),
                       rec("moruna066-rust", 1, 6.0), rec("duckdb", 1, 3.5)])
    vs = table.verdicts(slow["jobs"]["score"]["engines"])
    assert [v["passed"] for v in vs] == [False, False, None]
    assert vs[2]["text"] == ("the stage that loses most: the function, 5.0 s, of the split: read and decode 2.0 s, "
                             "the function 5.0 s, encode and write 1.0 s, waits 0.5 s")
    slow6 = summary_of([rec("moruna-node", 1, 7.0, cpu=40.0, split=six), rec("moruna066", 1, 9.0),
                        rec("moruna066-rust", 1, 6.0), rec("duckdb", 1, 3.5)])
    assert table.verdicts(slow6["jobs"]["score"]["engines"])[2]["text"] == (
        "the stage that loses most: the function, 4.0 s, of the split: read 0.7 s, decode 0.3 s, the function 4.0 s, "
        "encode 0.2 s, write 0.3 s, waits 0.5 s")
    assert table.verdicts({})[0]["passed"] is None
    lonely = summary_of([rec("moruna-node", 1, 2.0)])["jobs"]["score"]["engines"]
    vs = table.verdicts(lonely)
    assert "unchecked, no row for 0.6.6, Python function or 0.6.6, Rust function" in vs[0]["text"]
    assert vs[1]["passed"] is None
    nosplit = summary_of([rec("moruna-node", 1, 9.5), rec("moruna066", 1, 9.0), rec("moruna066-rust", 1, 6.0)])
    assert "no split" in table.verdicts(nosplit["jobs"]["score"]["engines"])[-1]["text"]


def test_the_log_entry_and_the_root_cause_format(tmp_path):
    split = {"read_decode_s": 6.0, "function_s": 1.0}
    s = summary_of([rec("moruna-node", 1, 7.0, split=split), rec("moruna066", 1, 9.0),
                    rec("moruna066-rust", 1, 6.0), rec("duckdb", 1, 3.5)])
    ms = rootcause.misses(s)
    assert [(m["job"], m["step"]) for m in ms] == [("score", "no regression"), ("score", "bar")]
    stage = "read and decode, 6.0 s, of the split: read and decode 6.0 s, the function 1.0 s"
    assert ms[0]["stage"] == stage
    text = "\n".join(rootcause.entry(m, "202611180900, plan nyc-2024") for m in ms)
    entries = rootcause.parse(text)
    assert len(entries) == 2 and entries[0]["fields"]["Stage"] == stage
    assert "Profile: TODO" in rootcause.problems(entries[0]) and "Defect: TODO" in rootcause.problems(entries[0])
    done = rootcause.entry(ms[1], "r", profile="perf of the source: arrow decode in moruna-sources/src/parquet.rs",
                           defect="pages decoded twice", beyond_the_benchmark="every Parquet read",
                           test="crates/moruna-sources/tests/decode_once.rs", work_item="architecture/work/6.4-decode-once.md",
                           not_refused="a general fix, no constant or job path")
    assert rootcause.problems(rootcause.parse(done)[0]) == []
    bad = done.replace("architecture/work/6.4-decode-once.md", "somewhere")
    assert rootcause.problems(rootcause.parse(bad)[0]) == ["Work item: not a path under architecture/work/"]
    short = rootcause.parse("#### Miss: score, bar\n- Run: r\n## next\n- Gap: ignored\n")[0]
    assert "Gap: missing" in rootcause.problems(short) and "Gap" not in short["fields"]
    log = tmp_path / "log.md"
    meta = {"date": "202611180900", "commit": "abc", "machines": "Standard_D4s_v4"}
    table.append(str(log), table.entry(s, meta, text))
    table.append(str(log), table.entry(s, meta))
    body = log.read_text()
    assert body.count("# The benchmark log") == 1 and body.count("## 202611180900: plan nyc-2024") == 2
    assert "- Commit: abc" in body and "### Misses (6C)" in body and len(rootcause.parse(body)) == 2


# --- The node's API ----------------------------------------------------------------------------

def test_the_console_and_the_events():
    text = ("\x1b[0mBdsDxe: loading\nmoruna: core 0.7.0 cpus=4 memory=17179869184\nmoruna: services started\n"
            "moruna: cloud azure instance 9f1c\nmoruna: tls sha256=" + PIN + "\nmoruna: \n"
            "moruna: ready api=10.0.0.4:443 boot_ms=2140\n")
    assert node_api.console(text) == {"cloud": "azure", "instance": "9f1c", "pin": PIN, "api": "10.0.0.4:443",
                                      "boot_ms": 2140}
    assert node_api.console("moruna: fatal platform: no instance data")["fatal"] == "platform: no instance data"
    ev = node_api.sse('data: {"type":"heartbeat","t_ms":2044,"rows_out":5}\n\ndata: not json\n\n: comment\n'
                      'data: {"type":"exit"}\n')
    assert ev == [{"type": "heartbeat", "t_ms": 2044, "rows_out": 5}, {"type": "exit"}]
    assert node_api.first_rows_s(ev) == 2.044
    assert node_api.first_rows_s([{"type": "event", "kind": "task_started", "task": "sink", "t_us": 1650000}]) == 1.65
    assert node_api.first_rows_s([]) is None
    cfg = node_api.node_config("K", ["F"])
    assert cfg == {"moruna": 1, "api": {"token_keys": ["K"]}, "functions": {"keys": ["F"]}}
    assert "functions" not in node_api.node_config("K")


def test_the_node_calls_and_a_job_that_times_out():
    calls = []

    def t(method, path, body, token):
        calls.append((method, path, body, token))
        if path == "/v1/jobs/j/cancel":
            return 200, b"{}"
        if path == "/v1/nope":
            return 404, b'{"error":{"code":"not_found","message":"no"}}'
        return 200, b'{"state": "running", "id": "j"}'

    n = node_api.Node(t, {"observe": "o", "submit": "s", "admin": "a"})
    assert n.submit({"moruna_spec": 1}) == "j" and calls[-1] == ("POST", "/v1/jobs", {"v1": {"moruna_spec": 1}}, "s")
    clock = iter([0.0, 1.0, 5.0])
    j = n.wait("j", 3.0, poll_s=0, sleep=lambda s: None, clock=lambda: next(clock))
    assert j["state"] == "timed out" and calls[-1][:2] == ("POST", "/v1/jobs/j/cancel")
    with pytest.raises(node_api.ApiError, match="404"):
        n.json("GET", "/v1/nope")
    n.register("gate-score", "https://x/code/gate-score", "f" * 64, "SIG")
    assert calls[-1][3] == "a" and calls[-1][2]["name"] == "gate-score"

    def done(method, path, body, token):
        return 200, b'{"state": "completed"}'
    assert node_api.Node(done, {}).wait("j", 1, sleep=lambda s: None)["state"] == "completed"


def test_tokens_are_made_again_before_they_expire():
    now = [0.0]
    made = []
    t = node_api.Tokens("/k", "i-1", refresh_s=2400, mint=lambda s: made.append(s) or f"{s}{len(made)}",
                        clock=lambda: now[0])
    assert t.get("submit") == "submit1" and t.get("submit") == "submit1"
    now[0] = 2401.0
    assert t.get("submit") == "submit2" and t.get("observe") == "observe3" and made == ["submit", "submit", "observe"]


def test_the_report_split():
    assert collect.report_split(REPORT) == {"read_s": 0.7, "decode_s": 0.3, "function_s": 4.0, "encode_s": 0.2,
                                            "write_s": 0.3, "wait_s": 0.03}
    # A decode done in the read's call has no decode_ms; a measured zero is kept.
    fused = {"tasks": [{"role": "source", "read_ms": 900.0}, {"role": "function", "busy_ms": 0.0},
                       {"role": "sink", "write_ms": 100.0}, {"role": "source", "read_ms": 100.0}]}
    assert collect.report_split(fused) == {"read_s": 1.0, "function_s": 0.0, "write_s": 0.1}
    assert collect.report_split({}) == {} and collect.report_split(None) == {}


def test_real_redacts_every_secret_it_holds(tmp_path, monkeypatch):
    """The functions' SAS, by its whole value and by its signature alone, and the storage key."""
    monkeypatch.setattr(run.subprocess, "run", lambda cmd, **kw: subprocess.CompletedProcess(
        cmd, 1, "", "AuthenticationFailed: sig=AbCdEfGhIjKlMnOp%2B and KEYKEYKEY"))
    out = io.StringIO()
    x = run.Real(cloud(), str(tmp_path / "commands.log"), out)
    sas = "sv=2024&sr=c&sp=r&sig=AbCdEfGhIjKlMnOp%2B"
    x.secrets.update({"storage-key": "KEYKEYKEY", proof_sas(): sas})
    x.say(f"fetched from https://a/functions/gate-score?{sas}")
    x.save(str(tmp_path / "r.json"), {"address": f"https://a?{sas}", "said": "AbCdEfGhIjKlMnOp+", "key": "KEYKEYKEY"})
    with pytest.raises(RuntimeError) as e:
        x.az(azure.containers(cloud(), "KEYKEYKEY")[0])
    every = out.getvalue() + (tmp_path / "r.json").read_text() + (tmp_path / "commands.log").read_text() + str(e.value)
    for secret in ("KEYKEYKEY", "AbCdEfGhIjKlMnOp", sas):
        assert secret not in every
    assert "<FUNCTIONS_SAS>" in out.getvalue() and "<storage-key>" in (tmp_path / "commands.log").read_text()


def proof_sas():
    return run.proof.SAS_ENV


@pytest.mark.skipif(not OPENSSL or "LibreSSL" in subprocess.run([OPENSSL or "true", "version"], capture_output=True,
                                                               text=True).stdout, reason="needs OpenSSL 3")
def test_tokens_and_signatures_verify(tmp_path):
    key = str(tmp_path / "k.pem")
    pub = node_api.keygen(key, OPENSSL)
    assert len(base64.b64decode(pub)) == 32
    tok = node_api.token(key, "i-1", ["observe"], ttl_s=600, now=1_000_000, openssl=OPENSSL)
    head, claims, sig = tok.split(".")
    pad = lambda s: s + "=" * (-len(s) % 4)  # noqa: E731
    assert json.loads(base64.urlsafe_b64decode(pad(head))) == {"alg": "EdDSA", "typ": "JWT"}
    assert json.loads(base64.urlsafe_b64decode(pad(claims))) == {"aud": "i-1", "exp": 1_000_600, "scope": ["observe"]}
    pubpem = tmp_path / "pub.pem"
    subprocess.run([OPENSSL, "pkey", "-in", key, "-pubout", "-out", str(pubpem)], check=True)
    (tmp_path / "msg").write_bytes(f"{head}.{claims}".encode())
    (tmp_path / "sig").write_bytes(base64.urlsafe_b64decode(pad(sig)))
    ok = subprocess.run([OPENSSL, "pkeyutl", "-verify", "-pubin", "-inkey", str(pubpem), "-rawin", "-in",
                         str(tmp_path / "msg"), "-sigfile", str(tmp_path / "sig")], capture_output=True)
    assert ok.returncode == 0
    s = node_api.sign_digest(key, "ab" * 32, OPENSSL)
    assert len(base64.b64decode(s)) == 64


def test_https_pins_the_certificate(tmp_path):
    """A local TLS server with a self-signed certificate: the pinned one is accepted, any other refused."""
    if not OPENSSL:
        pytest.skip("needs openssl")
    import ssl
    import threading
    from http.server import BaseHTTPRequestHandler, HTTPServer
    key, cert = tmp_path / "k.pem", tmp_path / "c.pem"
    subprocess.run([OPENSSL, "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
                    "-keyout", str(key), "-out", str(cert), "-days", "1", "-subj", "/CN=node"], check=True,
                   capture_output=True)

    class H(BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers["Content-Length"]))
            self.send_response(201)
            self.end_headers()
            self.wfile.write(json.dumps({"auth": self.headers["Authorization"], "body": json.loads(body)}).encode())

        def log_message(self, *a):
            pass

    srv = HTTPServer(("127.0.0.1", 0), H)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(str(cert), str(key))
    srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
    th = threading.Thread(target=srv.handle_request)
    th.start()
    der = ssl.PEM_cert_to_DER_cert(cert.read_text())
    import hashlib
    url = f"https://127.0.0.1:{srv.server_address[1]}"
    status, data = node_api.Https(url, hashlib.sha256(der).hexdigest())("POST", "/v1/jobs", {"a": 1}, "tok")
    th.join()
    assert status == 201 and json.loads(data) == {"auth": "Bearer tok", "body": {"a": 1}}
    th = threading.Thread(target=srv.handle_request)
    th.start()
    with pytest.raises(ConnectionError, match="pinned"):
        node_api.Https(url, "00" * 32)("GET", "/v1/node")
    th.join()
    srv.server_close()


# --- The engines' scripts ----------------------------------------------------------------------

def fixture_parquet(tmp_path):
    t = ipc.open_file(os.path.join(FIX, "inputs", "nyc", "main.arrow")).read_all()
    src = tmp_path / "in.parquet"
    pq.write_table(t, src, row_group_size=1000)
    return src


def reference_json(tmp_path):
    import canon
    t = ipc.open_file(os.path.join(FIX, "score", "main.expected.arrow")).read_all()
    ref = tmp_path / "ref.json"
    json.dump(canon.canonical("score", t), open(ref, "w"))
    return ref


@pytest.mark.parametrize("engine,job", [("duckdb", "score"), ("polars", "score"), ("duckdb-sql", "enrich"),
                                        ("polars-expr", "enrich"), ("duckdb-sql", "occupancy")])
def test_engine_py_gives_the_python_functions_rows(tmp_path, monkeypatch, engine, job):
    pytest.importorskip(engine.split("-")[0])
    for k in ("BENCH_ACCOUNT", "BENCH_KEY", "BENCH_CONTAINER", "BENCH_ENDPOINT"):
        monkeypatch.delenv(k, raising=False)
    import canon
    import engine as eng
    src = fixture_parquet(tmp_path)
    flist = tmp_path / "files"
    flist.write_text(str(src))
    out = tmp_path / "out"
    assert eng.main(["--engine", engine, "--workload", job, "--inputs", str(flist), "--out", str(out), "--cpus", "2",
                     "--mem-gib", "1", "--tmp", str(tmp_path / "tmp")]) == 0
    c = canon.canonical(job, canon.read_output([str(out)]))
    want = json.load(open(os.path.join(FIX, "manifest.json")))["jobs"][job]["main"]
    assert (c["rows"], c["hash_exact"]) == (want["rows"], want["hash_exact"])
    # predicted_tip is NumPy's on this machine: the oracle's to within Score's tolerance.
    ref = canon.canonical(job, ipc.open_file(os.path.join(FIX, job, "main.expected.arrow")).read_all())
    assert canon.compare(job, ref, c)["ok"]


def test_engine_py_reports_a_failed_run(tmp_path, capsys, monkeypatch):
    pytest.importorskip("duckdb")
    monkeypatch.delenv("BENCH_ACCOUNT", raising=False)
    import engine as eng
    flist = tmp_path / "files"
    flist.write_text(str(tmp_path / "missing.parquet"))
    assert eng.main(["--engine", "duckdb", "--workload", "score", "--inputs", str(flist), "--out", str(tmp_path / "o"),
                     "--cpus", "1", "--mem-gib", "1", "--tmp", str(tmp_path / "t")]) == 1
    res = json.loads(capsys.readouterr().out.split("RESULT ", 1)[1])
    assert res["error"] and res["marks"]["main"] and res["settings"]["threads"] == 1 and res["data"] == "local"
    assert eng.main(["--engine", "polars-expr", "--workload", "score", "--inputs", str(flist), "--out",
                     str(tmp_path / "o2"), "--cpus", "1", "--mem-gib", "1", "--tmp", str(tmp_path / "t")]) == 1
    assert "no native form of score" in capsys.readouterr().out


@pytest.mark.parametrize("job", ["score", "occupancy", "enrich", "audit"])
def test_sparks_output_schemas_are_the_python_functions(job):
    """Spark is told each job's output schema; it must be the Python function's own (the gate's
    fixtures are its output), Enrich's timestamps included."""
    import engine as eng
    schema = ipc.open_file(os.path.join(FIX, job, "main.expected.arrow")).schema
    assert eng.spark_ddl(schema) == eng.SPARK_SCHEMA[job]
    with pytest.raises(TypeError):
        eng.spark_ddl(pa.schema([("d", pa.date32())]))
    assert eng.spark_ddl(pa.schema([("t", pa.timestamp("us", tz="UTC")), ("i", pa.int32())])) == "t timestamp, i int"


def test_the_store_names_each_engines_blobs():
    import store as stores
    azure_store = stores.Store("acct", "KEY", "data")
    assert azure_store.blob_endpoint == "https://acct.blob.core.windows.net"
    assert azure_store.az("nyc/f.parquet") == "az://data/nyc/f.parquet"
    assert azure_store.abfs("out/x") == "abfs://data/out/x" and azure_store.path("out/x") == "data/out/x"
    assert azure_store.wasbs("nyc/f") == "wasbs://data@acct.blob.core.windows.net/nyc/f"
    assert azure_store.moruna_storage() == {"account": "acct", "access_key": "KEY"} and azure_store.moruna_env() == {}
    assert azure_store.polars_options() == {"account_name": "acct", "account_key": "KEY"}
    assert azure_store.adlfs_options() == {"account_name": "acct", "account_key": "KEY"}
    assert azure_store.spark_conf() == {"spark.hadoop.fs.azure.account.key.acct.blob.core.windows.net": "KEY"}
    assert "AccountKey=KEY;BlobEndpoint=https://acct.blob.core.windows.net;" in azure_store.duckdb_secret()
    local = stores.Store(stores.AZURITE_ACCOUNT, stores.AZURITE_KEY, "data", "http://127.0.0.1:10000/devstoreaccount1")
    assert local.moruna_storage()["allow_http"] and local.moruna_env() == {"AZURE_STORAGE_USE_EMULATOR": "true"}
    assert local.polars_options()["use_emulator"] == "true" and "connection_string" in local.adlfs_options()
    assert local.connection_string().startswith("DefaultEndpointsProtocol=http;")
    assert stores.Store.from_env(local.env()) == local and stores.Store.from_env({}) is None
    assert "BENCH_ENDPOINT" not in azure_store.env()


AZURITE = os.environ.get("BENCH_AZURITE", "")


@pytest.mark.skipif(not AZURITE, reason="BENCH_AZURITE names a running Azurite's blob endpoint")
@pytest.mark.parametrize("engine,job", [("duckdb", "score"), ("polars", "score"), ("duckdb-sql", "enrich"),
                                        ("polars-expr", "enrich"), ("rust-pool", "score"), ("dask", "score"),
                                        ("moruna066", "score"), ("moruna066-rust", "score")])
def test_engines_read_and_write_blob(tmp_path, monkeypatch, engine, job):
    """Against Azurite: the engine reads its input from the container and writes its output there,
    and its rows are the Python function's. BENCH_ENGINES names install.py's root."""
    import canon
    import store as stores
    root = os.environ["BENCH_ENGINES"]
    store = stores.Store(stores.AZURITE_ACCOUNT, stores.AZURITE_KEY, "test", AZURITE)
    store.create()
    store.upload(str(fixture_parquet(tmp_path)), "nyc/in.parquet")
    for k, v in store.env().items():
        monkeypatch.setenv(k, v)
    spec = {"id": f"{job}-{engine}-azurite", "job": job, "workload": job, "engine": engine, "repeat": 1, "cpus": 2,
            "mem_gib": 2, "files": [], "inputs": ["nyc/in.parquet"], "output": f"out/{job}-{engine}-{os.getpid()}"}
    (tmp_path / "spec.json").write_text(json.dumps(spec))
    ref = tmp_path / "ref.json"
    json.dump(canon.canonical(job, ipc.open_file(os.path.join(FIX, job, "main.expected.arrow")).read_all()), open(ref, "w"))
    rc = measure.main(["run", "--spec", str(tmp_path / "spec.json"), "--root", root, "--results", str(tmp_path / "r"),
                       "--work", str(tmp_path / "w"), "--reference-json", str(ref)])
    r = json.load(open(tmp_path / "r" / f"{spec['id']}.json"))
    assert r["data"] == ("local copy" if engine == "rust-pool" else "blob")
    if engine == "dask":
        # Dask's pandas round trip makes an integer column with a null a float column: the
        # fixtures have such nulls (2024's integer columns have none), so only the rows compare.
        assert r["ok"] and r["output_check"]["rows"], r
    else:
        assert rc == 0 and r["output_check"]["ok"], r


def test_measure_runs_and_checks_against_a_reference(tmp_path, monkeypatch):
    """measure.py's own path, with a stand-in engine that writes the fixture's expected output."""
    src = os.path.join(FIX, "score", "main.expected.arrow")
    code = ("import sys, time, json, pyarrow.ipc as i, pyarrow.parquet as pq\n"
            "t0 = time.time(); out = sys.argv[1]\n"
            f"pq.write_table(i.open_file({src!r}).read_all(), out + '/part.parquet')\n"
            "time.sleep(0.2)\n"
            "print('RESULT ' + json.dumps({'engine': 'duckdb', 'marks': {'main': t0, 'imported': t0, "
            "'ready': t0 + 0.1, 'done': time.time()}}))\n")
    monkeypatch.setattr(measure, "command", lambda spec, root, out, flist, tmp, report: [sys.executable, "-c", code, out])
    spec = {"id": "score-duckdb-r1", "engine": "duckdb", "workload": "score", "files": ["/x"], "cpus": 2, "mem_gib": 1,
            "profiles": str(tmp_path / "prof"), "fresh_profiles": True, "staging": str(tmp_path / "st")}
    (tmp_path / "spec.json").write_text(json.dumps([spec]))
    ref = reference_json(tmp_path)
    rc = measure.main(["run", "--spec", str(tmp_path / "spec.json"), "--id", spec["id"], "--root", str(tmp_path / "eng"),
                       "--results", str(tmp_path / "res"), "--work", str(tmp_path / "work"), "--reference-json", str(ref)])
    assert rc == 0
    r = json.load(open(tmp_path / "res" / "score-duckdb-r1.json"))
    assert r["ok"] and r["output_check"]["verdict"] == "bit for bit" and r["canon"]["by"] == "canon.py"
    assert r["operational_s"] >= 0 and r["startup_s"] >= 0.1 and r["cpu_s"] >= 0 and "rusage" in r["peak_is"]
    # The same output checked as the node's would be, from a directory.
    out = tmp_path / "nodeout"
    out.mkdir()
    pq.write_table(ipc.open_file(src).read_all(), out / "a.parquet")
    assert measure.main(["check", "--workload", "score", "--out", str(out), "--id", "n1", "--root", str(tmp_path / "eng"),
                         "--results", str(tmp_path / "res"), "--reference-json", str(ref)]) == 0
    assert json.load(open(tmp_path / "res" / "n1.check.json"))["output_check"]["ok"]


def test_measure_compares_within_the_tolerance_and_finds_a_difference(tmp_path):
    import canon
    t = ipc.open_file(os.path.join(FIX, "score", "main.expected.arrow")).read_all()
    ref = reference_json(tmp_path)
    tip = t["predicted_tip"].to_pylist()
    for name, delta, ok in (("near", 5e-7, True), ("far", 5e-6, False)):
        d = tmp_path / name
        d.mkdir()
        moved = t.set_column(3, "predicted_tip", pa.array([None if v is None else v + delta for v in tip], pa.float64()))
        pq.write_table(moved, d / "p.parquet")
        mine = measure.canonical("score", str(d), str(tmp_path / "noeng"))
        c = measure.compare("score", mine, str(tmp_path / "noeng"), str(d), reference_json=str(ref))
        assert c["ok"] is ok and c["hash_exact"] and not c["hash_all"], c
        assert c["verdict"].startswith("within tolerance" if ok else "DIFFERS")
    short = tmp_path / "short"
    short.mkdir()
    pq.write_table(t.slice(1), short / "p.parquet")
    c = measure.compare("score", measure.canonical("score", str(short), "/none"), "/none", str(short), reference_json=str(ref))
    assert c["verdict"] == "DIFFERS: rows"
    assert measure.compare("score", {"error": "x"}, "/none", "/none")["verdict"] == "no canonical form"
    assert measure.compare("score", {"rows": 1}, "/none", "/none")["verdict"] == "unchecked: no reference"
    empty = tmp_path / "empty"
    empty.mkdir()
    assert measure.canonical("score", str(empty), "/none") == {"error": "no output files"}
    assert canon.SPECS["score"]["tolerant"]["predicted_tip"] == (1.1e-6, 0.0)  # gate-canon's (canon.rs)


@pytest.mark.skipif(not os.environ.get("GATE_CANON"), reason="GATE_CANON names a built gate-canon")
def test_measure_with_gate_canon_dumps(tmp_path):
    eng = tmp_path / "eng" / "bin"
    eng.mkdir(parents=True)
    shutil.copy(os.environ["GATE_CANON"], eng / "gate-canon")
    t = ipc.open_file(os.path.join(FIX, "score", "main.expected.arrow")).read_all()
    a, b = tmp_path / "a", tmp_path / "b"
    a.mkdir()
    b.mkdir()
    pq.write_table(t, a / "p.parquet")
    pq.write_table(t.slice(0, 2000), b / "p1.parquet")
    pq.write_table(t.slice(2000), b / "p2.parquet")
    ref = measure.canonical("score", str(a), str(tmp_path / "eng"), str(tmp_path / "ref.dump"))
    assert ref["by"] == "gate-canon"
    mine = measure.canonical("score", str(b), str(tmp_path / "eng"), str(tmp_path / "mine.dump"))
    c = measure.compare("score", mine, str(tmp_path / "eng"), str(b), reference_dump=str(tmp_path / "ref.dump"))
    assert c["ok"] and c["verdict"] == "bit for bit"


NYC = os.path.expanduser(os.environ.get("BENCH_NYC", "~/moruna-data/nyc"))


@pytest.mark.skipif(not (os.environ.get("GATE_CANON") and os.path.isdir(os.path.join(NYC, "out066"))),
                    reason="GATE_CANON and 0.6.6's January outputs (references.py --keep-output) under BENCH_NYC")
@pytest.mark.parametrize("job", ["score", "occupancy", "enrich", "audit"])
def test_canon_py_and_gate_canon_agree_on_the_january_references(job):
    """gate-canon governs; canon.py must give the same canonical form of 0.6.6's January outputs,
    and both must be the references written from them."""
    import canon
    out = os.path.join(NYC, "out066", f"{job}-2024-01")
    ref = json.load(open(os.path.join(NYC, f"reference-{job}-2024-01.json")))
    p = subprocess.run([os.environ["GATE_CANON"], "digest", job, out], capture_output=True, text=True, check=True)
    g = json.loads(p.stdout)
    c = canon.canonical(job, canon.read_output([out]))
    for k in ("rows", "hash_exact", "hash_all"):
        assert c[k] == g[k] == ref[k], k
    assert canon.compare(job, ref, c)["ok"]


@pytest.mark.skipif(not os.environ.get("GATE_CANON"), reason="GATE_CANON names a built gate-canon")
def test_references_from_a_stand_in_066(tmp_path, monkeypatch, capsys):
    """references.py's path with a stand-in for 0.6.6 that writes the gate's expected output: a
    month by canon.py and gate-canon together, an existing reference compared and kept, the year
    with its tolerant rows in a dump."""
    import types
    import references
    fake = types.ModuleType("moruna")
    fake.__version__ = "0.6.6"
    fake.ParquetSource = fake.ParquetSink = lambda x: x
    fake.function = lambda fn: fn

    def run_(source, fns, sink, **kw):
        job = fns[0].__name__
        os.makedirs(sink)
        pq.write_table(ipc.open_file(os.path.join(FIX, job, "main.expected.arrow")).read_all(), f"{sink}/p.parquet")
        return types.SimpleNamespace(to_dict=lambda: {"exit": "completed"})
    fake.run = run_
    monkeypatch.setitem(sys.modules, "moruna", fake)
    data = tmp_path / "data"
    (data / "norm").mkdir(parents=True)
    for m in ("01", "02"):
        (data / "norm" / f"yellow_tripdata_2099-{m}.parquet").write_text("x")
    args = ["--data", str(data), "--gate-canon", os.environ["GATE_CANON"], "--work", str(tmp_path / "w"),
            "--jobs", "score,enrich"]
    assert references.main(args + ["--period", "2099-01", "--keep-output", str(tmp_path / "keep")]) == 0
    ref = json.load(open(data / "reference-score-2099-01.json"))
    assert ref["canon"] == "canon.py and gate-canon agree" and len(ref["tolerant_rows"]) == ref["rows"] == 4000
    assert ref["schema"][-1] == "predicted_tip:double" and (tmp_path / "keep" / "enrich-2099-01").is_dir()
    capsys.readouterr()
    assert references.main(args + ["--period", "2099-01"]) == 0
    again = [json.loads(line) for line in capsys.readouterr().out.splitlines()]
    assert all(r["written"] is None and r["same_as_existing"] for r in again) and again[0]["tolerant_within"]
    assert references.main(args + ["--period", "2099"]) == 0
    year = json.load(open(data / "reference-score-2099.json"))
    assert year["canon"] == "gate-canon" and (data / year["dump"]).exists() and "tolerant_rows" not in year
    with pytest.raises(SystemExit):
        references.main(args + ["--period", "1999"])


def test_measure_commands_and_phases():
    spec = {"id": "x", "engine": "rust-pool", "workload": "score", "cpus": 4, "mem_gib": 14}
    cmd = measure.command(spec, "/eng", "/out", "/list", "/tmp", "/r")
    assert cmd[0] == "/eng/bin/gate-baseline" and cmd[cmd.index("--threads") + 1] == "4"
    py = measure.command(dict(spec, engine="moruna066-rust", staging="/s", profiles="/p"), "/eng", "/o", "/l", "/t", "/r")
    assert py[0] == "/eng/venv-moruna066/bin/python" and py[py.index("--staging") + 1] == "/s" and "--profiles" in py
    with pytest.raises(ValueError):
        measure.command(dict(spec, engine="nope"), "/e", "/o", "/l", "/t", "/r")
    env = measure.environment(dict(spec, env={"X": "1"}), "/tmp")
    assert env["OPENBLAS_NUM_THREADS"] == "1" and env["POLARS_MAX_THREADS"] == "4" and env["X"] == "1"
    marks = {"main": 10.2, "imported": 11.0, "ready": 12.0, "start": 12.5, "done": 20.0}
    ph = measure.phases({"marks": marks}, 10.0, 21.0)
    # The session is start-up; operational time starts when the job is handed over.
    assert (ph["startup_s"], ph["operational_s"], ph["imports_s"], ph["session_s"], ph["handover_s"],
            ph["shutdown_s"]) == (2.5, 7.5, 0.8, 1.0, 0.5, 1.0)
    assert py[py.index("--inputs") + 1] == "/l" and py[py.index("--root") + 1] == "/eng"
    assert measure.phases({"engine": "rust-pool", "wall_s": 3.0}, 0.0, 3.25) == {"startup_s": 0.25, "operational_s": 3.0}
    assert measure.phases(None, 0, 1) == {"startup_s": None, "operational_s": None}
    assert measure.verdict({"error": "e"}).startswith("not compared")


class LocalStore:
    """A store whose container is a local directory: store.py's own transfers, on pyarrow's local
    filesystem instead of Azure's."""

    def __new__(cls, root):
        import pyarrow.fs as pafs
        import store as stores

        class S(stores.Store):
            def pyarrow_fs(self):
                return pafs.SubTreeFileSystem(str(root), pafs.LocalFileSystem())
        s = S("acct", "KEY", "data")
        os.makedirs(root / "data", exist_ok=True)
        return s


def stand_in(src, to_blob):
    """An engine that writes the fixture's expected output, to the store's container (as an
    engine with a Blob reader does) or to its local output directory."""
    target = "os.environ['BLOBROOT'] + '/data/' + out" if to_blob else "out"
    return ("import os, sys, time, json, pyarrow.ipc as i, pyarrow.parquet as pq\n"
            "t0 = time.time(); out = sys.argv[1]; d = " + target + "\nos.makedirs(d, exist_ok=True)\n"
            f"pq.write_table(i.open_file({src!r}).read_all(), d + '/part.parquet')\n"
            "print('RESULT ' + json.dumps({'engine': 'x', 'marks': {'main': t0, 'ready': t0, 'start': t0, "
            "'done': time.time()}}))\n")


@pytest.mark.parametrize("engine", ["duckdb", "rust-pool"])
def test_measure_with_a_store(tmp_path, monkeypatch, engine):
    """With a store, an engine with a Blob reader is given blob names and fetched from Blob for the
    check; the Rust pool gets a local copy each way, timed apart."""
    import store as stores
    blob = tmp_path / "blob"
    st = LocalStore(blob)
    src = os.path.join(FIX, "score", "main.expected.arrow")
    st.upload(str(fixture_parquet(tmp_path)), "nyc/in.parquet")
    monkeypatch.setattr(stores.Store, "from_env", classmethod(lambda cls, env=None: st))
    monkeypatch.setenv("BLOBROOT", str(blob))
    seen = {}

    def command(spec, root, out, flist, tmp, report):
        seen["out"], seen["inputs"] = out, open(flist).read().split()
        return [sys.executable, "-c", stand_in(src, engine != "rust-pool"), out]
    monkeypatch.setattr(measure, "command", command)
    spec = {"id": f"score-{engine}-r1", "engine": engine, "workload": "score", "files": [], "cpus": 1, "mem_gib": 1,
            "inputs": ["nyc/in.parquet"], "output": f"out/score-{engine}-r1", "env": {"BLOBROOT": str(blob)}}
    (tmp_path / "spec.json").write_text(json.dumps(spec))
    assert measure.main(["run", "--spec", str(tmp_path / "spec.json"), "--root", str(tmp_path / "eng"), "--results",
                         str(tmp_path / "r"), "--work", str(tmp_path / "w"),
                         "--reference-json", str(reference_json(tmp_path))]) == 0
    r = json.load(open(tmp_path / "r" / f"score-{engine}-r1.json"))
    assert r["ok"] and r["output_check"]["verdict"] == "bit for bit" and r["memory_gib"] == 1
    assert (blob / "data" / "out" / f"score-{engine}-r1" / "part.parquet").exists()
    if engine == "rust-pool":
        assert r["data"] == "local copy" and r["copy_in_s"] >= 0 and r["copy_out_s"] >= 0
        assert seen["inputs"][0].endswith("in/score-rust-pool-r1/in.parquet") and seen["out"].startswith(str(tmp_path))
    else:
        assert r["data"] == "blob" and "fetch_s" in r
        assert (seen["inputs"], seen["out"]) == (["nyc/in.parquet"], "out/score-duckdb-r1")
    # The node's output, checked from the container.
    assert measure.main(["check", "--workload", "score", "--out", str(tmp_path / "nodeout"), "--id", "n1", "--root",
                         str(tmp_path / "eng"), "--results", str(tmp_path / "r"), "--blob-prefix",
                         f"out/score-{engine}-r1", "--reference-json", str(reference_json(tmp_path))]) == 0
    assert st.download("nyc/in.parquet", str(tmp_path / "again")) > 0


def test_measure_kills_a_run_at_its_timeout(tmp_path):
    spec = {"id": "slow", "cpus": 1, "mem_gib": 1}
    code, out, err, t0, t1, totals = measure.execute(spec, [sys.executable, "-c", "import time; time.sleep(30)"],
                                                     dict(os.environ), timeout=1)
    assert code != 0 and "killed at the timeout" in err and t1 - t0 < 10


def test_install_records_each_engine(tmp_path, monkeypatch):
    cmds = []

    def sh(cmd, log, env=None, cwd=None):
        cmds.append((cmd, cwd, (env or {}).get("CARGO_BUILD_JOBS")))
        if cmd[:2] == ["cargo", "build"]:
            rel = tmp_path / "root" / "target-gate" / "release"
            rel.mkdir(parents=True, exist_ok=True)
            for b in ("gate-baseline", "gate-canon"):
                (rel / b).write_text("bin")
        if cmd[:2] == ["uv", "build"]:
            out = cmd[cmd.index("--out-dir") + 1]
            os.makedirs(out, exist_ok=True)
            open(os.path.join(out, "moruna-0.6.6-cp314-cp314t-x.whl"), "w").close()

    monkeypatch.setattr(install, "sh", sh)
    monkeypatch.setattr(install, "versions", lambda python, modules: {"python": "3.14", "modules": {m: "1" for m in modules}})
    monkeypatch.setattr(install, "java", lambda: None)
    root = str(tmp_path / "root")
    src = tmp_path / "src066"
    src.mkdir()
    assert install.main(["--root", root, "--gate-src", str(tmp_path), "--engines", "rust-pool,spark,moruna066",
                         "--moruna066", str(src), "--jobs", "3"]) == 0
    done = json.load(open(os.path.join(root, "installed.json")))
    assert done["spark"]["note"].startswith("no JVM") and done["rust-pool"]["ok"]
    assert ["cargo", "build", "--release", "-j", "3", "-p", "moruna-gate-baseline", "--bins"] in [c[0] for c in cmds]
    assert any(c[0][:2] == ["uv", "build"] and c[2] == "3" for c in cmds)
    assert any(c[0][-3:] == [os.path.join(root, "wheels", "moruna-0.6.6-cp314-cp314t-x.whl"), "pyarrow", "numpy"]
               for c in cmds)
    assert any(c[0][:2] == ["uv", "venv"] and "3.14+gil" in c[0] for c in cmds)
    # 0.6.6's Rust function needs 0.6.6's environment, and 0.6.6 needs its wheel or tree.
    assert install.main(["--root", str(tmp_path / "r2"), "--gate-src", str(tmp_path), "--engines", "moruna066-rust"]) == 1
    assert "install moruna066 first" in json.load(open(tmp_path / "r2" / "installed.json"))["moruna066-rust"]["error"]
    assert install.main(["--root", str(tmp_path / "r3"), "--gate-src", str(tmp_path), "--engines", "moruna066"]) == 1
    with pytest.raises(SystemExit):
        install.main(["--root", str(tmp_path / "r4"), "--gate-src", str(tmp_path), "--engines", "nope"])
    n = len(cmds)
    assert install.main(["--root", root, "--gate-src", str(tmp_path), "--engines", "rust-pool"]) == 0 and len(cmds) == n


def test_install_runs_commands_and_reads_versions(tmp_path):
    log = str(tmp_path / "log")
    install.sh([sys.executable, "-c", "print('hi')"], log)
    assert "hi" in open(log).read()
    with pytest.raises(RuntimeError, match="exited 3"):
        install.sh([sys.executable, "-c", "import sys; sys.exit(3)"], log)
    v = install.versions(sys.executable, ["json", "no_such_module"])
    assert v["modules"]["no_such_module"].startswith("missing") and v["python"]
    assert install.wheel066(str(tmp_path), log, log, 1) == log


def test_local_runs_the_linux_runs_here(tmp_path, monkeypatch):
    p = small_plan()
    p["linux_data"] = "local"
    seen, refs = [], []
    (tmp_path / "ref-score.json").write_text("{}")

    def fake_run(cmd, **kw):
        spec = json.load(open(cmd[cmd.index("--spec") + 1]))
        seen.append(spec)
        refs.append(cmd[cmd.index("--reference-json") + 1] if "--reference-json" in cmd else None)
        return subprocess.CompletedProcess(cmd, 0, 'MEASURED {"id": "%s"}\n' % spec["id"], "")

    monkeypatch.setattr(run.subprocess, "run", fake_run)

    class A:
        results, engines_root, store_endpoint = str(tmp_path / "r"), str(tmp_path / "e"), ""
        reference_json = str(tmp_path / "ref-{job}.json")
    out = io.StringIO()
    assert run.local(p, A, out) == 0
    assert [s["engine"] for s in seen if s["repeat"] == 1] == ["moruna066", "moruna066-rust", "duckdb"]
    assert "not run here" in out.getvalue() and seen[0]["files"][0].endswith("yellow_tripdata_2024-01.parquet")
    assert seen[0]["profiles"].startswith(str(tmp_path / "r" / "work"))
    assert set(refs) == {str(tmp_path / "ref-score.json")}
    p["linux_data"] = "blob"
    with pytest.raises(SystemExit, match="store-endpoint"):
        run.local(p, A, out)
