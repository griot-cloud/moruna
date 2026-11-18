"""The node benchmark from one command on the Mac (system design 6, work items 6.1 and 6.2).

    bench/node/run.py --plan PLAN --dry-run [--vhd F] [--tasks DIR] [--from ADDRESS] [--ref066 REF]
                      [--stamp yyyymmddHHMM]
    bench/node/run.py --plan PLAN --go --vhd F --tasks DIR --from ADDRESS [--ref066 REF]
    bench/node/run.py --plan PLAN --local --engines-root DIR --results DIR [--store-endpoint URL]
                      [--reference-json F]
    bench/node/run.py --plan PLAN --table bench/node/results/<stamp>

Each run's record lands in `bench/node/results/<stamp>/` as the run finishes (`node/` for Moruna's,
`live/` for the Linux machine's, from measure.py's summary line), so `--table` gives the table of
the runs so far while the plan goes on; at the end the Linux machine's full records replace the
live ones.

One run, in order, every step on the run's resource group in `griot-dev` (azure.py):

1. **Provision:** the group; a storage account with containers for the node's disk, the data
   (the one container every engine reads its inputs from and writes its output to), the code,
   the functions and the results; the plan's data uploaded; the Moruna node from its VHD
   (`kernel/node/AZURE.md`'s steps, as `azure-up.sh --proof` runs them: TCP 443 from `--from`
   alone) and one Linux machine (Ubuntu 24.04) for the other engines, each the plan's size in its
   zone. The node is read from its console until `ready`; the Linux machine builds 0.6.6's wheel
   once and keeps it in the container, and installs every engine with its Blob reader
   (engines/setup-linux.sh).
2. **Register** the gate tasks as the node's functions as proof.py does (3E): each in container
   `functions`, at its SAS address, by its SHA-256 and a signature with the run's function key.
3. **Run** the plan's runs one at a time, in its order (plan.py): Moruna's through its API as
   proof.py runs a job (its document, the account key by secret name, its events followed over
   SSE to the exit), every other engine's over `az vm run-command` (engines/measure.py), the
   container's account and key given to it as environment variables. Each output is checked
   against 0.6.6's on the Linux machine.
4. **Collect** the records into `bench/node/results/<stamp>/`, the table into its `table.md`, and
   the run's entry, with a root-cause entry for each miss (rootcause.py), into
   `architecture/benchmark-log.md`. When a step fails, what the Linux machine holds is still
   collected (summary.json and table.md, no log entry) before the tear-down.
5. **Tear down:** the resource group is deleted, always, even when a step failed, and shown gone.

`--dry-run` prints every command of that run, in order, and does nothing else: no `az` command
runs, nothing is created and no file is written. Values only a real run learns (the storage key,
the node's address) are printed as `<names>`. The image, the tasks and the operator's address are
checked when given, as a real run checks them before it creates anything. Creating resources
needs the PM's go-ahead each time (1E), so a real run needs `--go`, which says it was given.

No secret is shown or kept: the storage key and every SAS are replaced by their names in what is
printed, in commands.log and in every record, the functions' SAS also by its signature alone
(proof.Redact).

`--local` runs the plan's Linux runs on this machine with measure.py, with no cloud: the engines'
check on the Mac (work item 6.1, phase 1), each engine reading and writing a local Blob emulator's
container (`--store-endpoint`, Azurite) as it would Azure's. Moruna's runs are skipped there.
"""
import argparse
import datetime
import hashlib
import http.client
import ipaddress
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
sys.path.insert(0, HERE)
sys.path.insert(1, os.path.join(HERE, "..", "vm"))
sys.path.insert(2, os.path.join(HERE, "engines"))
import azure  # noqa: E402
import collect  # noqa: E402
import node_api  # noqa: E402
import plan as plans  # noqa: E402
import proof  # noqa: E402, the proof's function registration, SAS, job document and SSE follower
import rootcause  # noqa: E402
import store as stores  # noqa: E402, engines/store.py: the run's Blob container
import table  # noqa: E402

ENG, CODE, DATA = "/opt/engines", "/opt/bench", "/data"
MEASURE = [f"{ENG}/venv-harness/bin/python", f"{CODE}/bench/node/engines/measure.py"]
JAVA = "JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64"
# The console is read every 10 s, for at most 10 minutes: AZURE.md's bound for a node to be ready.
CONSOLE_WAIT = (10, 600)
MiB, GiB = 1 << 20, 1 << 30
LOG = os.path.join(REPO, "architecture", "benchmark-log.md")


class DryRun:
    """Prints each step; answers with `<names>` where a real run would learn a value."""

    def __init__(self, out=sys.stdout):
        self.out, self.n = out, 0

    def say(self, text):
        self.out.write(text + "\n")

    def step(self, kind, text):
        self.n += 1
        self.say(f"[{self.n:03d}] {kind:5s} {text}")

    def az(self, cmd, out=None, secrets=None):
        self.step("az", azure.redact(cmd, secrets or {}))
        return f"<{out}>" if out else ""

    def local(self, cmd, out=None):
        self.step("local", shlex.join(cmd))
        return f"<{out}>" if out else ""

    def write(self, path, text):
        self.step("write", f"{path}")

    def keygen(self, path):
        self.local([node_api.OPENSSL, "genpkey", "-algorithm", "ed25519", "-out", path])
        return f"<public half of {os.path.basename(path)}>"

    def sign(self, key, digest):
        self.local([node_api.OPENSSL, "pkeyutl", "-sign", "-inkey", key, "-rawin", "-in", f"<the 32 bytes of {digest}>"])
        return f"<signature of {digest}>"

    def tokens(self, key, aud):
        self.local([node_api.OPENSSL, "pkeyutl", "-sign", "-inkey", key, "-rawin", "-in",
                    f"<header.claims: aud {aud}, exp now + 50 min, scope observe|submit|admin>"])
        return {s: f"<{s}-token>" for s in ("observe", "submit", "admin")}

    def connect(self, url, pin):
        self.transport = DryTransport(self)

    def http(self, method, path, body=None, token=None):
        shown = "" if body is None else " " + json.dumps(body, separators=(",", ":"))
        self.step("api", f"{method} https://<node-address>{path}{shown}")
        if path == "/v1/jobs" and method == "POST":
            return 201, b'{"id": "<job-id>"}'
        if path.startswith("/v1/jobs/") and path.count("/") == 3:
            return 200, b'{"state": "completed"}'
        return 200, b"{}"

    def stream(self, method, path, token=None):
        self.step("api", f"{method} https://<node-address>{path} (Server-Sent Events, to the job's report and exit)")
        yield from ("event: report", 'data: {"type":"report","report":{},"plan":null}', "",
                    "event: exit", 'data: {"type":"exit","code":0,"diagnostic":null}', "")

    def poll(self, what, every_s, at_most_s, fn):
        got = fn()
        self.say(f"      (again every {every_s} s, for at most {at_most_s} s, until {what})")
        return got

    def value(self, name, real):
        return real if real not in (None, "") else f"<{name}>"

    def save(self, path, obj):
        self.step("write", path)

    def finish(self, plan, results, meta, log=True):
        entry = f"; the run's entry into {os.path.relpath(LOG, REPO)}" if log else ""
        self.step("local", f"collect {results}: summary.json, table.md{entry}")

    def note(self, text):
        self.say(f"      # {text}")


class DryTransport:
    """The dry run's node: each call printed, a job answered as completed."""

    def __init__(self, x):
        self.x = x

    def __call__(self, method, path, body=None, token=None):
        return self.x.http(method, path, body, token)

    def stream(self, method, path, token=None):
        return self.x.stream(method, path, token)


class Real:
    """Runs each step. Every `az` command is checked by azure.guard first, and logged to the run's
    commands.log. Every secret the run holds (`secrets`, which Bench fills) is replaced by its name
    in what is said, logged and saved."""

    def __init__(self, cloud, log_path, out=sys.stdout):
        self.cloud, self.log, self.out = cloud, open(log_path, "a"), out
        self.transport, self.secrets = None, {}

    def redact(self, text, extra=None):
        return proof.Redact({**self.secrets, **(extra or {})})(text)

    def say(self, text):
        self.out.write(self.redact(text) + "\n")
        self.out.flush()

    def _run(self, cmd, text):
        self.log.write(f"{time.strftime('%H:%M:%S')} {text}\n")
        self.log.flush()
        p = subprocess.run(cmd, capture_output=True, text=True)
        if p.returncode != 0:
            raise RuntimeError(self.redact(f"{text[:200]}: exit {p.returncode}: {p.stderr.strip()[-500:]}"))
        return p.stdout.strip()

    def az(self, cmd, out=None, secrets=None):
        azure.guard(self.cloud, cmd)
        return self._run(cmd, self.redact(shlex.join(cmd), secrets))

    def local(self, cmd, out=None):
        return self._run(cmd, shlex.join(cmd))

    def write(self, path, text):
        with open(path, "w") as f:
            f.write(text)

    def keygen(self, path):
        return node_api.keygen(path)

    def sign(self, key, digest):
        return node_api.sign_digest(key, digest)

    def tokens(self, key, aud):
        return node_api.Tokens(key, aud)

    def connect(self, url, pin):
        self.transport = node_api.Https(url, pin)

    def http(self, method, path, body=None, token=None):
        return self.transport(method, path, body, token)

    def poll(self, what, every_s, at_most_s, fn):
        end = time.time() + at_most_s
        while True:
            got = fn()
            if got is not None or time.time() > end:
                return got
            time.sleep(every_s)

    def value(self, name, real):
        if real in (None, ""):
            raise RuntimeError(f"the run has no {name}")
        return real

    def save(self, path, obj):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(self.redact(json.dumps(obj, default=str, indent=1)))

    def finish(self, plan, results, meta, log=True):
        finish(plan, results, meta, log=LOG if log else None)

    def note(self, text):
        self.say(f"# {text}")


class SpecBlob(proof.Blob):
    """proof.py's Blob data for one run of a plan: the spec's inputs, its output prefix."""

    def __init__(self, store, spec):
        super().__init__(store)
        self.spec = spec

    def inputs(self, _period):
        return [self.store.az(n) for n in self.spec["inputs"]]

    def output(self, _run):
        return self.store.az(self.spec["output"].rstrip("/") + "/")


class Bench:
    def __init__(self, plan, x, cloud, args, results, secrets_dir):
        self.plan, self.x, self.c, self.a = plan, x, cloud, args
        self.results, self.sec = results, secrets_dir
        self.runs = plans.runs(plan, DATA)
        self.with_node = "moruna-node" in plan["engines"]
        # Every secret the run learns, by name: the executor redacts them wherever they would show.
        self.secrets, self.setup, self.collected = {}, None, False
        x.secrets = self.secrets
        self.node, self.boot_ms, self.budget = None, None, None

    def base(self):
        return f"https://{self.c.account}.blob.core.windows.net"

    def tasks(self):
        return getattr(self.a, "tasks", "") or "<images/gate/tasks/build.sh's output>"

    def operator(self):
        return getattr(self.a, "operator", "") or "<operator-address>"

    # 1. Provision ---------------------------------------------------------------------------------
    def prepare(self):
        x, sec = self.x, self.sec
        m = self.plan["machine"]
        machines = 2 if self.with_node else 1
        x.note(f"run {self.c.stamp}: group {self.c.group} in {self.c.subscription}, {len(self.runs)} runs; "
               f"{machines} machines of {self.c.size} at once, {machines * m['cpus']} vCPUs")
        for line in getattr(self.a, "checked", ()):
            x.note(line)
        token_key = x.keygen(f"{sec}/token-key.pem")
        function_key = x.keygen(f"{sec}/function-key.pem")
        x.write(f"{sec}/node-config.json", json.dumps(node_api.node_config(token_key, [function_key]), indent=1))
        x.local(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", f"{sec}/linux-key"])
        x.local(["git", "-C", REPO, "archive", "--format=tar.gz", "-o", f"{sec}/harness.tar.gz", "HEAD"])
        x.local(["git", "-C", REPO, "archive", "--format=tar.gz", "-o", f"{sec}/moruna-066.tar.gz", self.a.ref066])
        x.write(f"{sec}/runs.json", json.dumps(self.runs, indent=1))

    def provision(self):
        x, c, sec = self.x, self.c, self.sec
        x.az(azure.group_create(c))
        for cmd in azure.storage(c):
            x.az(cmd)
        s = self.secrets
        s["storage-key"] = key = x.az(azure.storage_key(c), out="storage-key")
        for cmd in azure.containers(c, key):
            x.az(cmd, secrets=s)
        src = os.path.expanduser(self.plan["dataset"]["source_dir"])
        for f in self.plan["dataset"]["files"]:
            # The one container every engine reads from and writes to (store.py): inputs under nyc/.
            x.az(azure.upload(c, key, "data", f"nyc/{f}", os.path.join(src, f)), secrets=s)
        for name in ("harness.tar.gz", "moruna-066.tar.gz", "runs.json"):
            x.az(azure.upload(c, key, "code", name, f"{sec}/{name}"), secrets=s)
        # Long enough for the slowest plan; the account goes with the group at the end. `code` is
        # written once, by the setup's 0.6.6 wheel; `functions` is read only, by the node (3E).
        expiry = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=48)).strftime("%Y-%m-%dT%H:%MZ")
        for container, perms in (("code", "racw"), ("results", "racwl")):
            s[f"sas-{container}"] = x.az(azure.sas(c, key, container, perms, expiry), out=f"sas-{container}", secrets=s)
        for cmd in azure.nsgs(c, self.operator()):
            x.az(cmd)
        if self.with_node:
            # The proof's functions: the gate tasks in `functions`, fetched by a read-only SAS.
            for job in self.plan["jobs"]:
                x.az(azure.upload(c, key, "functions", f"gate-{job}", f"{self.tasks()}/gate-{job}"), secrets=s)
            s[proof.SAS_ENV] = x.az(azure.sas(c, key, "functions", "r", expiry), out=proof.SAS_ENV, secrets=s)
            x.az(azure.upload(c, key, "vhds", "node.vhd", getattr(self.a, "vhd", "") or "<node.vhd>", page=True),
                 secrets=s)
            vhd = x.az(azure.blob_url(c, key, "vhds", "node.vhd"), out="vhd-url", secrets=s)
            x.az(azure.node_disk(c, vhd, x.az(azure.account_id(c), out="account-id")))
            x.az(azure.node_ip(c))
            x.az(azure.node_vm(c, f"{sec}/node-config.json"))
        x.az(azure.linux_vm(c, f"{sec}/linux-key.pub"))

    def node_ready(self):
        x = self.x

        def look():
            try:
                text = x.az(azure.boot_log(self.c), out="console")
            except RuntimeError as e:
                # Made with --no-wait: until the machine and its log exist, `az` says so; look again.
                x.note(f"no console yet: {str(e)[-160:]}")
                return None
            info = node_api.console(text or "")
            if info.get("fatal"):
                raise RuntimeError(f"the node stopped: moruna: fatal {info['fatal']}")
            return info if "boot_ms" in info else None

        info = x.poll("the console shows `moruna: ready` or `moruna: fatal`", *CONSOLE_WAIT, look) or {}
        address = x.value("node-address", x.az(azure.node_address(self.c), out="node-address"))
        x.connect(f"https://{address}", x.value("node-pin", info.get("pin")))
        self.boot_ms = info.get("boot_ms")
        tokens = x.tokens(f"{self.sec}/token-key.pem", x.value("instance-id", info.get("instance")))
        self.node = node_api.Node(x.transport, tokens)
        facts = self.node.node()
        # The most the node takes (3A: a larger budget is refused), as proof.py gives every job.
        bound = facts.get("job_memory_bytes") or (facts["memory_bytes"] - proof.NODE_KEEPS
                                                  if facts.get("memory_bytes") else None)
        self.budget = x.value("job-memory-bytes", bound)
        x.note(f"node {facts.get('instance', '<instance>')}, version {facts.get('version', '<version>')}: "
               f"each job's budget {self.budget} bytes")

    def setup_linux(self):
        x, s = self.x, self.secrets
        params = [f"BASE={self.base()}", f"SAS_CODE={s['sas-code']}"]
        out = x.az(azure.run_command(self.c, "@" + os.path.join(HERE, "engines", "setup-linux.sh"), params),
                   out="setup", secrets=s)
        x.az(azure.run_command(self.c, f'curl -sSf -o {DATA}/runs.json "$BASE/code/runs.json?$SAS_CODE"',
                               [f"BASE={self.base()}", f"SAS_CODE={s['sas-code']}"]), secrets=s)
        image = x.az(azure.image_version(self.c), out="linux-image-version")
        setup = next((json.loads(line[6:]) for line in (out or "").splitlines() if line.startswith("SETUP ")), {})
        self.setup = dict(setup, linux_image=image)
        x.save(os.path.join(self.results, "setup.json"), self.setup)

    def register(self):
        """The gate tasks as the node's functions, as proof.py registers them (3E): each at its
        SAS address in `functions`, by its SHA-256, signed with the run's function key."""
        x, sas = self.x, self.secrets[proof.SAS_ENV]
        key = f"{self.sec}/function-key.pem"
        where = f"{self.base()}/functions"
        if os.path.isdir(self.tasks()):
            proof.register(self.node, self.plan["jobs"], self.tasks(), where, lambda d: x.sign(key, d), x.note, sas)
            return
        # A dry run given no --tasks: the same calls, with the digests named.
        for job in self.plan["jobs"]:
            digest = f"<sha256-of-gate-{job}>"
            self.node.register(f"gate-{job}", proof.address(where, f"gate-{job}", sas), digest, x.sign(key, digest))

    # 2. Run ----------------------------------------------------------------------------------------
    def reference_args(self, spec):
        ref = f"{DATA}/ref/{spec['job']}.dump"
        if spec["engine"] == plans.REFERENCE and spec["repeat"] == 0:
            return ["--make-reference", ref]
        return ["--reference-dump", ref]

    def store_params(self):
        """The run's container, as measure.py and the engines read it (store.py), for run-command."""
        return [f"BENCH_ACCOUNT={self.c.account}", f"BENCH_KEY={self.secrets['storage-key']}", "BENCH_CONTAINER=data"]

    def linux(self, spec):
        cmd = MEASURE + ["run", "--spec", f"{DATA}/runs.json", "--id", spec["id"], "--root", ENG, "--results",
                         f"{DATA}/results", "--work", f"{DATA}/work"] + self.reference_args(spec)
        params = self.store_params() if self.plan["linux_data"] == "blob" else []
        out = self.x.az(azure.run_command(self.c, f"{JAVA} {shlex.join(cmd)}", params), out=f"measured {spec['id']}",
                        secrets=self.secrets)
        got = measured(out)
        if got:
            self.x.say(f"      {json.dumps(got)}")
        # The run's record lands here as it finishes, from measure.py's summary line, so a partial
        # table can be made while the plan runs (`--table`); the full one replaces it at the end.
        self.x.save(os.path.join(self.results, "live", f"{spec['id']}.json"), live_record(spec, got))
        return got

    def moruna(self, spec):
        """One job on the node, as proof.py runs one: its document (the registered function by
        name, the inputs and output in the run's container, the account key by secret name), its
        events followed over SSE to the exit. Every engine's operational time is its own wall on
        the machine that ran it (the PM, 2026-11-18): Moruna's is its report's `wall_ms`. Its
        start-up is the node's `submitted` to `started`, whole seconds, so within 1 s. Every mark
        is kept: this machine's end-to-end time (the answer to the exit seen), the boot's
        `boot_ms` and the first rows at the sink among them. A job the node refuses, or that is
        not followed to its end, is a failed run, and the next run goes on."""
        x, s = self.x, self.secrets
        store = stores.Store(self.c.account, s.get("storage-key", "<storage-key>"), "data")
        numeric = isinstance(self.budget, int)
        doc, job_secrets = proof.document({"job": spec["job"], "id": spec["id"], "period": None},
                                          SpecBlob(store, spec), spec["cpus"], self.budget if numeric else 0)
        if not numeric:
            doc["budget"]["memory_bytes"] = self.budget
        rec = {"spec": spec, "data": "blob", "memory_gib": self.budget / GiB if numeric else None,
               "boot_ms": self.boot_ms}
        deadline = time.time() + spec["timeout_s"]
        t_sent = time.time()
        try:
            jid = proof.submit(self.node, doc, job_secrets, deadline, log=x.note)
        except node_api.ApiError as e:
            return self.keep(spec, dict(rec, ok=False, state="refused", error=str(e)))
        t_answered = time.time()
        rec["job_id"] = jid
        try:
            got = proof.follow_sse(self.node, jid, deadline, log=x.note)
        except (node_api.ApiError, OSError, http.client.HTTPException) as e:
            return self.keep(spec, dict(rec, ok=False, state="not followed", error=str(e)))
        t_seen = time.time()
        if got.exit is None:
            self.node.call("POST", f"/v1/jobs/{jid}/cancel", None, "submit")
            return self.keep(spec, dict(rec, ok=False, state="timed out", records=len(got.records)))
        j = self.node.job(jid)
        report = (got.report or {}).get("report") or {}
        resources = report.get("resources") or {}
        wall_ms = report.get("wall_ms")
        started, submitted = j.get("started"), j.get("submitted")
        rec.update({
            "state": j.get("state"), "exit": got.exit,
            "ok": j.get("state") == "completed" and got.exit.get("code") == 0,
            "operational_s": round(wall_ms / 1e3, 3) if isinstance(wall_ms, (int, float)) else None,
            "operational_by": "the engine's report, wall_ms",
            "startup_s": started - submitted if isinstance(started, int) and isinstance(submitted, int) else None,
            "startup_by": "the node's job record, submitted to started, whole seconds",
            "startup_within_s": 1,
            "marks": {"sent": t_sent, "answered": t_answered, "exit_seen": t_seen,
                      "end_to_end_s": round(t_seen - t_answered, 3),
                      "node_times": {k: j.get(k) for k in ("submitted", "started", "ended")},
                      "wall_ms": wall_ms, "first_rows_s": node_api.first_rows_s(got.events()),
                      "resumed": got.resumed},
            "cpu_s": resources.get("cpu_core_s"),
            "peak_bytes": report.get("peak_anon_bytes") or resources.get("peak_memory_bytes"),
            "split": collect.report_split(report),
            "report_budget": report.get("budget"), "object_io": report.get("object_io")})
        if rec["ok"]:
            check = MEASURE + ["check", "--workload", spec["job"], "--out", f"{DATA}/work/node-{spec['id']}", "--id",
                               spec["id"], "--root", ENG, "--results", f"{DATA}/results", "--blob-prefix",
                               spec["output"]] + self.reference_args(spec)
            out = x.az(azure.run_command(self.c, shlex.join(check), self.store_params()),
                       out=f"checked {spec['id']}", secrets=s)
            verdict = (measured(out) or {}).get("output") or ""
            # As measure.py's own records say it, so the collector reads it alike (collect.output_verdict).
            rec["output_check"] = {"verdict": verdict, "ok": verdict.startswith(("bit for bit", "within")),
                                   "hash_all": verdict == "bit for bit"}
        else:
            rec["diagnostic"] = got.exit.get("diagnostic")
        x.save(os.path.join(self.results, "node", f"{spec['id']}.job.json"),
               {"job": j, "report": report, "events": got.events(), "exit": got.exit})
        return self.keep(spec, rec)

    def keep(self, spec, rec):
        self.x.save(os.path.join(self.results, "node", f"{spec['id']}.json"), rec)
        if not isinstance(self.x, DryRun):
            outcome = (rec.get("output_check") or {}).get("verdict") or rec.get("error") or rec.get("diagnostic")
            self.x.say(f"      {spec['id']}: {rec.get('state')}, operational {rec.get('operational_s')} s, {outcome}")
        return rec

    # 3. Collect ------------------------------------------------------------------------------------
    def collect(self, log=True):
        """The Linux machine's records into the results, then the summary, the table and, unless the
        run stopped early (`log` false), the run's entry in the benchmark log."""
        x, s = self.x, self.secrets
        script = (f"tar -czf {DATA}/results.tgz -C {DATA} results && curl -sSf -X PUT -H 'x-ms-blob-type: BlockBlob' "
                  f'-T {DATA}/results.tgz "$BASE/results/linux.tgz?$SAS_RESULTS"')
        x.az(azure.run_command(self.c, script, [f"BASE={self.base()}", f"SAS_RESULTS={s['sas-results']}"]), secrets=s)
        x.az(azure.blob(self.c, s["storage-key"], "blob", "download", "-c", "results", "-n", "linux.tgz", "-f",
                        f"{self.results}/linux.tgz", "-o", "none"), secrets=s)
        x.local(["tar", "-xzf", f"{self.results}/linux.tgz", "-C", self.results])
        self.collected = True
        x.finish(self.plan, self.results, self.meta(), log=log)

    def meta(self):
        commit = subprocess.run(["git", "-C", REPO, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
        setup = self.setup or {}
        image = getattr(self.a, "image", None) or {}
        node_image = self.a.vhd
        if image:
            node_image = f"{self.a.vhd} ({image['bytes']} bytes, sha256 {image['sha256']})"
        return {"date": self.c.stamp, "commit": commit, "node_image": node_image,
                "machines": f"{self.c.size}, zone {self.c.zone}, {self.c.location}, subscription {self.c.subscription}",
                "linux_image": setup.get("linux_image"),
                "data": f"read and written in Blob container `data`; 0.6.6's wheel {setup.get('wheel_066')} "
                        f"in {setup.get('wheel_066_s')} s, outside every timing",
                "results": os.path.relpath(self.results, REPO)}

    # 4. Tear down ----------------------------------------------------------------------------------
    def teardown(self):
        x = self.x
        x.note("tear down: always, even when a step failed")
        try:
            x.az(azure.group_delete(self.c))
        finally:
            gone = x.az(azure.group_exists(self.c), out="false")
            if not isinstance(x, DryRun) and gone.strip() != "false":
                raise RuntimeError(f"the resource group {self.c.group} is still there: delete it by hand")

    def go(self):
        try:
            self.prepare()
            self.provision()
            if self.with_node:
                self.node_ready()
            self.setup_linux()
            if self.with_node:
                self.register()
            for spec in self.runs:
                x = self.x
                x.note(f"run {spec['id']}")
                (self.moruna if spec["where"] == "node" else self.linux)(spec)
            self.collect()
        except BaseException:
            # What the Linux machine holds is kept before the group goes; the run is not logged.
            if self.setup is not None and not self.collected:
                try:
                    self.collect(log=False)
                except Exception as e:  # noqa: BLE001 (the first failure is the one raised)
                    self.x.note(f"the partial results were not collected: {e}")
            raise
        finally:
            self.teardown()


def live_record(spec, got):
    """A Linux run's record from measure.py's summary line (the full one stays on the machine
    until the end): what the table needs, marked `live`, which collect.load lets the full one
    replace. A run with no summary line is a failed run."""
    got = got or {}
    rec = {"spec": spec, "live": True, "ok": bool(got.get("ok")), "memory_gib": spec.get("mem_gib")}
    for k in ("startup_s", "operational_s", "cpu_s", "peak_bytes", "oom_kills", "data", "copy_in_s", "copy_out_s"):
        if got.get(k) is not None:
            rec[k] = got[k]
    rec["output_check"] = got.get("check") or {"verdict": got.get("output")}
    if not got:
        rec["error"] = "no summary line in the run's output"
    return rec


def measured(text):
    """The record's summary line from measure.py's output (run-command keeps the last 4 KiB)."""
    for line in reversed((text or "").splitlines()):
        if line.startswith("MEASURED "):
            return json.loads(line[len("MEASURED "):])
    return None


def finish(plan, results, meta, log=LOG):
    """The summary, the table and the run's log entry (none when `log` is None), from the records
    under `results`."""
    summary = collect.summarise(collect.load(results), plan)
    collect.write(results, summary)
    open(os.path.join(results, "table.md"), "w").write(table.render(summary) + "\n")
    if log:
        misses = "\n".join(rootcause.entry(m, f"{meta['date']}, plan {plan['name']}")
                           for m in rootcause.misses(summary))
        table.append(log, table.entry(summary, meta, misses))
    return summary


def local(plan, args, out=sys.stdout, python=sys.executable):
    """The plan's Linux runs on this machine, measured by measure.py (the Mac's check). A `blob`
    plan needs a local emulator (`--store-endpoint`, Azurite): the inputs go into its container
    first, outside every timing, and each engine then reads and writes it with its own reader."""
    import store as stores
    results = os.path.abspath(args.results)
    work = os.path.join(results, "work")
    os.makedirs(work, exist_ok=True)
    src = os.path.expanduser(plan["dataset"]["source_dir"])
    env = dict(os.environ)
    if plan["linux_data"] == "blob":
        if not getattr(args, "store_endpoint", ""):
            raise SystemExit("a blob plan here needs --store-endpoint, a local emulator's (Azurite's)")
        store = stores.Store(stores.AZURITE_ACCOUNT, stores.AZURITE_KEY, "data", args.store_endpoint)
        store.create()
        for f in plan["dataset"]["files"]:
            store.upload(os.path.join(src, f), f"nyc/{f}")
        env.update(store.env())
    every = plans.runs(plan, DATA)
    skipped = [r["id"] for r in every if r["where"] != "linux"]
    if skipped:
        out.write(f"# not run here, Moruna's runs need the node: {', '.join(skipped)}\n")
    for r in (r for r in every if r["where"] == "linux"):
        r = dict(r, files=[os.path.join(src, f) for f in plan["dataset"]["files"]])
        for k in ("profiles", "staging"):
            if r.get(k):
                r[k] = r[k].replace(DATA, work, 1)
        spec = os.path.join(work, f"{r['id']}.spec.json")
        json.dump(r, open(spec, "w"))
        reference = os.path.expanduser(args.reference_json.format(job=r["job"])) if args.reference_json else ""
        ref = ["--reference-json", reference] if reference and os.path.exists(reference) else []
        cmd = [python, os.path.join(HERE, "engines", "measure.py"), "run", "--spec", spec, "--root",
               os.path.abspath(args.engines_root), "--results", results, "--work", work] + ref
        p = subprocess.run(cmd, capture_output=True, text=True, env=env)
        line = measured(p.stdout)
        out.write((json.dumps(line) if line else f"{r['id']}: no result: {p.stderr[-500:]}") + "\n")
        out.flush()
    return 0


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(MiB), b""):
            h.update(chunk)
    return h.hexdigest()


def image_facts(path):
    """The node's image as Azure takes it (AZURE.md; 1A): a fixed VHD, its disk a whole number of
    MiB with the 512-byte footer after it. Its size and SHA-256, or a SystemExit saying what is wrong."""
    if not os.path.isfile(path):
        raise SystemExit(f"run.py: no VHD at {path}")
    size = os.path.getsize(path)
    with open(path, "rb") as f:
        f.seek(max(size - 512, 0))
        footer = f.read(512)
    if len(footer) < 512 or footer[:8] != b"conectix":
        raise SystemExit(f"run.py: {path} is not a VHD: no footer")
    if int.from_bytes(footer[60:64], "big") != 2:
        raise SystemExit(f"run.py: {path} is not a fixed VHD (package.py --formats vhd makes one)")
    disk = int.from_bytes(footer[48:56], "big")
    if disk != size - 512 or disk % MiB:
        raise SystemExit(f"run.py: {path}: its disk of {disk} bytes is not the file less its footer, a whole MiB")
    return {"path": path, "bytes": size, "disk_mib": disk // MiB, "sha256": sha256_of(path)}


def task_facts(plan, tasks):
    """The plan's gate tasks in `tasks`, each an x86_64 ELF program (images/gate/tasks/build.sh's,
    built for the core): their SHA-256s, or a SystemExit naming the one missing or wrong."""
    out = {}
    for job in plan["jobs"]:
        f = os.path.join(tasks, f"gate-{job}")
        if not os.path.isfile(f):
            raise SystemExit(f"run.py: no gate task at {f}")
        with open(f, "rb") as fh:
            head = fh.read(20)
        if head[:4] != b"\x7fELF" or int.from_bytes(head[18:20], "little") != 62:
            raise SystemExit(f"run.py: {f} is not an x86_64 ELF program: the tasks are built for the core")
        out[f"gate-{job}"] = sha256_of(f)
    return out


def check_inputs(plan, a, real):
    """What a run is given, checked before anything is created: the plan's data, and with the
    node the image, the gate tasks and the operator's address (TCP 443 is admitted from it alone,
    AZURE.md). A real run needs them all; a dry run checks those it is given. Returns the lines
    the run notes first; a SystemExit says what is wrong."""
    lines, with_node = [], "moruna-node" in plan["engines"]
    if real and with_node and not (a.vhd and a.tasks and a.operator):
        raise SystemExit("run.py: a real run needs --vhd, --tasks and --from")
    src = os.path.expanduser(plan["dataset"]["source_dir"])
    missing = [f for f in plan["dataset"]["files"] if not os.path.isfile(os.path.join(src, f))]
    if missing:
        raise SystemExit(f"run.py: the plan's data is not all in {src}: no {', '.join(missing)}")
    total = sum(os.path.getsize(os.path.join(src, f)) for f in plan["dataset"]["files"])
    lines.append(f"data: {len(plan['dataset']['files'])} files from {src}, {total / MiB:.0f} MiB to upload")
    if not with_node:
        return lines
    if a.operator:
        try:
            ipaddress.IPv4Address(a.operator)
        except ValueError:
            raise SystemExit(f"run.py: --from {a.operator!r} is not an IPv4 address") from None
        lines.append(f"the node admits TCP 443 from {a.operator}/32 alone")
    a.vhd = os.path.expanduser(a.vhd)
    a.image = image_facts(a.vhd) if a.vhd else None
    if a.image:
        lines.append(f"node image: {a.image['path']}, {a.image['bytes']} bytes, a fixed VHD of "
                     f"{a.image['disk_mib']} MiB, sha256 {a.image['sha256']}")
    if a.tasks:
        a.tasks = os.path.expanduser(a.tasks)
        lines.append(f"gate tasks: {a.tasks}: " + ", ".join(f"{k} {v[:12]}" for k, v in task_facts(plan, a.tasks).items()))
    return lines


def main(argv=None, out=sys.stdout):
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--plan", required=True, help="a plan file, or a name under bench/node/plans")
    mode = p.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true")
    mode.add_argument("--go", action="store_true", help="create the run's resources: the PM's go-ahead was given")
    mode.add_argument("--local", action="store_true")
    mode.add_argument("--table", metavar="RESULTS", default="",
                      help="the table of what a run's results directory holds so far (summary.json, table.md, "
                           "and printed); no log entry. Safe while the run goes on")
    p.add_argument("--vhd", default="", help="the node's fixed VHD (package.py --formats vhd)")
    p.add_argument("--tasks", default="", help="the gate's native tasks (images/gate/tasks/build.sh)")
    p.add_argument("--from", dest="operator", default="",
                   help="this machine's public IPv4 address: the node admits TCP 443 from it alone (AZURE.md)")
    # 0.6.6 has no tag yet: its release commit and the CI commits after it, fix/adaptive-runtime's head.
    p.add_argument("--ref066", default="8018c96ab02867c7201450a9375ce3d9bc4da00e",
                   help="0.6.6's commit, built on the Linux machine")
    p.add_argument("--stamp", default="", help="yyyymmddHHMM; now, in UTC, by default")
    p.add_argument("--engines-root", default="", help="--local: install.py's root")
    p.add_argument("--reference-json", default="",
                   help="--local: a canon.py reference to check outputs against; `{job}` names the job")
    p.add_argument("--store-endpoint", default="", help="--local: a local Blob emulator's endpoint (Azurite)")
    p.add_argument("--results", default="")
    a = p.parse_args(argv)
    path = a.plan if os.path.exists(a.plan) else plans.default_plan_path(a.plan)
    plan = plans.load(path)
    stamp = a.stamp or datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%d%H%M")
    if a.table:
        if not os.path.isdir(a.table):
            p.error(f"--table: no results directory {a.table}")
        summary = finish(plan, a.table, None, log=None)
        done = sum(1 for _ in collect.load(a.table))
        out.write(f"# {done} of {len(plans.runs(plan, DATA))} runs recorded in {a.table}\n\n")
        out.write(table.render(summary) + "\n")
        return 0
    if a.local:
        if not a.engines_root or not a.results:
            p.error("--local needs --engines-root and --results")
        return local(plan, a, out)
    m = plan["machine"]
    cloud = azure.Cloud(stamp, size=m["size"], zone=str(m["zone"]),
                        linux_image=m.get("image", azure.Cloud.linux_image), os_disk_gib=m.get("os_disk_gib", 128))
    results = os.path.abspath(a.results or os.path.join(HERE, "results", stamp))
    a.checked = check_inputs(plan, a, real=not a.dry_run)
    if a.dry_run:
        Bench(plan, DryRun(out), cloud, a, results, "<secrets>").go()
        return 0
    os.makedirs(results, exist_ok=True)
    shutil.copy(path, os.path.join(results, "plan.json"))
    secrets = tempfile.mkdtemp(prefix="moruna-bench-")
    try:
        Bench(plan, Real(cloud, os.path.join(results, "commands.log"), out), cloud, a, results, secrets).go()
    finally:
        shutil.rmtree(secrets, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
