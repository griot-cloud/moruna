"""The node benchmark from one command on the Mac (system design 6, work item 6.1).

    bench/node/run.py --plan PLAN --dry-run [--vhd F] [--tasks DIR] [--ref066 REF] [--stamp yyyymmddHHMM]
    bench/node/run.py --plan PLAN --go --vhd F --tasks DIR [--ref066 REF]
    bench/node/run.py --plan PLAN --local --engines-root DIR --results DIR [--reference-json F]

One run, in order, every step on the run's resource group in `griot-dev` (azure.py):

1. **Provision:** the group; a storage account with containers for the node's disk, the data,
   the code and the results; the plan's data uploaded; the Moruna node from its VHD (item 1.13's
   steps) and one Linux machine (Ubuntu 24.04) for the other engines, each the plan's size in its
   zone. The node is read from its console until `ready`; the Linux machine installs every engine
   (engines/setup-linux.sh) and copies the data to its disk, timed apart.
2. **Run** the plan's runs one at a time, in its order (plan.py): Moruna's through its API
   (node_api.py), every other engine's over `az vm run-command` (engines/measure.py). Each output
   is checked against 0.6.6's on the Linux machine.
3. **Collect** the records into `bench/node/results/<stamp>/`, the table into its `table.md`, and
   the run's entry, with a root-cause entry for each miss (rootcause.py), into
   `architecture/benchmark-log.md`.
4. **Tear down:** the resource group is deleted, always, even when a step failed, and shown gone.

`--dry-run` prints every command of that run, in order, and does nothing else: no `az` command
runs, nothing is created and no file is written. Values only a real run learns (the storage key,
the node's address) are printed as `<names>`. Creating resources needs the PM's go-ahead each
time (1E), so a real run needs `--go`, which says it was given.

`--local` runs the plan's Linux runs on this machine with measure.py, with no cloud: the engines'
check on the Mac (work item 6.1, phase 1). Moruna's runs are skipped there.
"""
import argparse
import datetime
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
import azure  # noqa: E402
import collect  # noqa: E402
import node_api  # noqa: E402
import plan as plans  # noqa: E402
import rootcause  # noqa: E402
import runner_split  # noqa: E402, bench/vm: the time split from a Moruna report
import table  # noqa: E402

ENG, CODE, DATA = "/opt/engines", "/opt/bench", "/data"
MEASURE = [f"{ENG}/venv-harness/bin/python", f"{CODE}/bench/node/engines/measure.py"]
JAVA = "JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64"
CONSOLE_WAIT = (10, 300)        # the console is read every 10 s, for at most 5 minutes (OPENSTACK.md)
NODE_RESERVE_MIB = 768          # 3A: a job's budget is at most the node's memory less 768 MiB
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
        pass

    def http(self, method, path, body=None, token=None):
        shown = "" if body is None else " " + json.dumps(body, separators=(",", ":"))
        self.step("api", f"{method} https://<node-address>{path}{shown}")
        if path == "/v1/jobs" and method == "POST":
            return 201, b'{"id": "<job-id>"}'
        if path.startswith("/v1/jobs/") and path.count("/") == 3:
            return 200, b'{"state": "completed"}'
        if "/events" in path:
            return 200, b""
        return 200, b"{}"

    def poll(self, what, every_s, at_most_s, fn):
        got = fn()
        self.say(f"      (again every {every_s} s, for at most {at_most_s} s, until {what})")
        return got

    def value(self, name, real):
        return real if real not in (None, "") else f"<{name}>"

    def save(self, path, obj):
        self.step("write", path)

    def finish(self, plan, results, meta):
        self.step("local", f"collect {results}: summary.json, table.md; the run's entry into {os.path.relpath(LOG, REPO)}")

    def note(self, text):
        self.say(f"      # {text}")


class Real:
    """Runs each step. Every `az` command is checked by azure.guard first, and logged, with its
    secrets replaced by their names, to the run's commands.log."""

    def __init__(self, cloud, log_path, out=sys.stdout):
        self.cloud, self.log, self.out = cloud, open(log_path, "a"), out
        self.transport = None

    def say(self, text):
        self.out.write(text + "\n")
        self.out.flush()

    def _run(self, cmd, text):
        self.log.write(f"{time.strftime('%H:%M:%S')} {text}\n")
        self.log.flush()
        p = subprocess.run(cmd, capture_output=True, text=True)
        if p.returncode != 0:
            raise RuntimeError(f"{text[:200]}: exit {p.returncode}: {p.stderr.strip()[-500:]}")
        return p.stdout.strip()

    def az(self, cmd, out=None, secrets=None):
        azure.guard(self.cloud, cmd)
        return self._run(cmd, azure.redact(cmd, secrets or {}))

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
        json.dump(obj, open(path, "w"), default=str, indent=1)

    def finish(self, plan, results, meta):
        finish(plan, results, meta)

    def note(self, text):
        self.say(f"# {text}")


class Bench:
    def __init__(self, plan, x, cloud, args, results, secrets_dir):
        self.plan, self.x, self.c, self.a = plan, x, cloud, args
        self.results, self.sec = results, secrets_dir
        self.runs = plans.runs(plan, DATA)
        self.with_node = "moruna-node" in plan["engines"]
        self.secrets, self.setup = {}, None
        self.node, self.boot_ms, self.budget_gib = None, None, None

    def base(self):
        return f"https://{self.c.account}.blob.core.windows.net"

    def tasks(self):
        return self.a.tasks or "<images/gate/tasks/build.sh's output>"

    # 1. Provision ---------------------------------------------------------------------------------
    def prepare(self):
        x, sec = self.x, self.sec
        x.note(f"run {self.c.stamp}: group {self.c.group} in {self.c.subscription}, {len(self.runs)} runs")
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
            x.az(azure.upload(c, key, "data", f, os.path.join(src, f)), secrets=s)
        for name in ("harness.tar.gz", "moruna-066.tar.gz", "runs.json"):
            x.az(azure.upload(c, key, "code", name, f"{sec}/{name}"), secrets=s)
        # Long enough for the slowest plan; the account goes with the group at the end.
        expiry = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=48)).strftime("%Y-%m-%dT%H:%MZ")
        for container, perms in (("code", "r"), ("data", "r"), ("results", "racwl")):
            s[f"sas-{container}"] = x.az(azure.sas(c, key, container, perms, expiry), out=f"sas-{container}", secrets=s)
        for cmd in azure.nsgs(c):
            x.az(cmd)
        if self.with_node:
            x.az(azure.upload(c, key, "vhds", "node.vhd", self.a.vhd or "<node.vhd>", page=True), secrets=s)
            for job in self.plan["jobs"]:
                x.az(azure.upload(c, key, "code", f"gate-{job}", f"{self.tasks()}/gate-{job}"), secrets=s)
            vhd = x.az(azure.blob_url(c, key, "vhds", "node.vhd"), out="vhd-url", secrets=s)
            x.az(azure.node_disk(c, vhd, x.az(azure.account_id(c), out="account-id")))
            x.az(azure.node_vm(c, f"{sec}/node-config.json"))
            for cmd in azure.boot_diagnostics(c):
                x.az(cmd)
        x.az(azure.linux_vm(c, f"{sec}/linux-key.pub"))

    def node_ready(self):
        x = self.x

        def look():
            info = node_api.console(x.az(azure.boot_log(self.c), out="console") or "")
            if info.get("fatal"):
                raise RuntimeError(f"the node stopped: moruna: fatal {info['fatal']}")
            return info if "boot_ms" in info else None

        info = x.poll("the console shows `moruna: ready` or `moruna: fatal`", *CONSOLE_WAIT, look) or {}
        address = x.value("node-address", x.az(azure.public_ip(self.c, azure.NODE), out="node-address"))
        x.connect(f"https://{address}", x.value("node-pin", info.get("pin")))
        self.boot_ms = info.get("boot_ms")
        tokens = x.tokens(f"{self.sec}/token-key.pem", x.value("instance-id", info.get("instance")))
        self.node = node_api.Node(x.http, tokens)
        memory = self.node.node().get("memory_bytes")
        if memory:
            self.budget_gib = (memory / (1 << 20) - NODE_RESERVE_MIB) / 1024

    def setup_linux(self):
        x, s = self.x, self.secrets
        params = [f"BASE={self.base()}", f"SAS_CODE={s['sas-code']}", f"SAS_DATA={s['sas-data']}",
                  "FILES=" + " ".join(self.plan["dataset"]["files"])]
        out = x.az(azure.run_command(self.c, "@" + os.path.join(HERE, "engines", "setup-linux.sh"), params),
                   out="setup", secrets=s)
        x.az(azure.run_command(self.c, f'curl -sSf -o {DATA}/runs.json "$BASE/code/runs.json?$SAS_CODE"',
                               [f"BASE={self.base()}", f"SAS_CODE={s['sas-code']}"]), secrets=s)
        image = x.az(azure.image_version(self.c), out="linux-image-version")
        setup = next((json.loads(line[6:]) for line in (out or "").splitlines() if line.startswith("SETUP ")), {})
        self.setup = dict(setup, linux_image=image)
        x.save(os.path.join(self.results, "setup.json"), self.setup)

    def register(self):
        """The gate's native tasks as the node's functions (3E): each image's SHA-256, signed."""
        x = self.x
        for job in self.plan["jobs"]:
            digest = x.local(["shasum", "-a", "256", f"{self.tasks()}/gate-{job}"], out=f"sha256-of-gate-{job}").split()[0]
            sig = x.sign(f"{self.sec}/function-key.pem", digest)
            self.node.register(f"gate-{job}", f"{self.base()}/code/gate-{job}", digest, sig)

    # 2. Run ----------------------------------------------------------------------------------------
    def reference_args(self, spec):
        ref = f"{DATA}/ref/{spec['job']}.dump"
        if spec["engine"] == plans.REFERENCE and spec["repeat"] == 0:
            return ["--make-reference", ref]
        return ["--reference-dump", ref]

    def linux(self, spec):
        cmd = MEASURE + ["run", "--spec", f"{DATA}/runs.json", "--id", spec["id"], "--root", ENG, "--results",
                         f"{DATA}/results", "--work", f"{DATA}/work"] + self.reference_args(spec)
        out = self.x.az(azure.run_command(self.c, f"{JAVA} {shlex.join(cmd)}"), out=f"measured {spec['id']}")
        got = measured(out)
        if got:
            self.x.say(f"      {json.dumps(got)}")
        return got

    def moruna(self, spec):
        x, s = self.x, self.secrets
        urls = [f"{self.base()}/data/{f}" for f in self.plan["dataset"]["files"]]
        doc = node_api.job_document(spec["job"], urls, f"{self.base()}/results/node/{spec['id']}/",
                                    {"location": f"{self.base()}/code/gate-{spec['job']}"}, spec["cpus"],
                                    self.budget_gib or self.plan["machine"]["mem_gib"])
        x.note(node_api.PENDING)
        t_submit = time.time()
        jid = self.node.submit(doc)

        def ended():
            j = self.node.job(jid)
            return j if j.get("state") in node_api.ENDED else None

        j = x.poll("the job has ended", 2, spec["timeout_s"], ended)
        if j is None:
            self.node.call("POST", f"/v1/jobs/{jid}/cancel", None, "submit")
            j = {"state": "timed out"}
        events, report = self.node.events(jid), self.node.report(jid)
        first = node_api.first_rows_s(events)
        times = j.get("times") or {}
        job_s = times["ended"] - times["started"] if times.get("ended") and times.get("started") else None
        rec = {"spec": spec, "job_id": jid, "state": j.get("state"), "ok": j.get("state") == "completed",
               "startup_s": self.boot_ms / 1e3 if self.boot_ms else None, "boot_ms": self.boot_ms,
               "engine_start_s": first, "job_s": job_s,
               "operational_s": job_s - first if job_s is not None and first is not None else None,
               "submit_to_end_s": round(time.time() - t_submit, 3),
               "cpu_s": report.get("cpu_s"), "peak_bytes": report.get("peak_anon_bytes"),
               "split": runner_split.split(report)["split"]}
        check = MEASURE + ["check", "--workload", spec["job"], "--out", f"{DATA}/work/node-{spec['id']}", "--id",
                           spec["id"], "--root", ENG, "--results", f"{DATA}/results", "--blob-prefix",
                           f"node/{spec['id']}/"] + self.reference_args(spec)
        out = x.az(azure.run_command(self.c, shlex.join(check), [f"BASE={self.base()}", f"SAS_RESULTS={s['sas-results']}"]),
                   out=f"checked {spec['id']}", secrets=s)
        rec["output_check"] = {"verdict": (measured(out) or {}).get("output")}
        rec["output_check"]["ok"] = (rec["output_check"]["verdict"] or "").startswith(("bit for bit", "within"))
        x.save(os.path.join(self.results, "node", f"{spec['id']}.json"), rec)
        x.save(os.path.join(self.results, "node", f"{spec['id']}.job.json"), {"job": j, "report": report, "events": events})
        return rec

    # 3. Collect ------------------------------------------------------------------------------------
    def collect(self):
        x, s = self.x, self.secrets
        script = (f"tar -czf {DATA}/results.tgz -C {DATA} results && curl -sSf -X PUT -H 'x-ms-blob-type: BlockBlob' "
                  f'-T {DATA}/results.tgz "$BASE/results/linux.tgz?$SAS_RESULTS"')
        x.az(azure.run_command(self.c, script, [f"BASE={self.base()}", f"SAS_RESULTS={s['sas-results']}"]), secrets=s)
        x.az(azure.blob(self.c, s["storage-key"], "blob", "download", "-c", "results", "-n", "linux.tgz", "-f",
                        f"{self.results}/linux.tgz", "-o", "none"), secrets=s)
        x.local(["tar", "-xzf", f"{self.results}/linux.tgz", "-C", self.results])
        x.finish(self.plan, self.results, self.meta())

    def meta(self):
        commit = subprocess.run(["git", "-C", REPO, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
        setup = self.setup or {}
        return {"date": self.c.stamp, "commit": commit, "node_image": self.a.vhd,
                "machines": f"{self.c.size}, zone {self.c.zone}, {self.c.location}, subscription {self.c.subscription}",
                "linux_image": setup.get("linux_image"),
                "data": f"{setup.get('copy_bytes')} bytes copied to the Linux machine in {setup.get('copy_s')} s",
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
        finally:
            self.teardown()


def measured(text):
    """The record's summary line from measure.py's output (run-command keeps the last 4 KiB)."""
    for line in reversed((text or "").splitlines()):
        if line.startswith("MEASURED "):
            return json.loads(line[len("MEASURED "):])
    return None


def finish(plan, results, meta, log=LOG):
    """The summary, the table and the run's log entry, from the records under `results`."""
    summary = collect.summarise(collect.load(results), plan)
    collect.write(results, summary)
    open(os.path.join(results, "table.md"), "w").write(table.render(summary) + "\n")
    misses = "\n".join(rootcause.entry(m, f"{meta['date']}, plan {plan['name']}") for m in rootcause.misses(summary))
    table.append(log, table.entry(summary, meta, misses))
    return summary


def local(plan, args, out=sys.stdout, python=sys.executable):
    """The plan's Linux runs on this machine, measured by measure.py (the Mac's check)."""
    results = os.path.abspath(args.results)
    work = os.path.join(results, "work")
    os.makedirs(work, exist_ok=True)
    src = os.path.expanduser(plan["dataset"]["source_dir"])
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
        ref = ["--reference-json", os.path.expanduser(args.reference_json)] if args.reference_json else []
        cmd = [python, os.path.join(HERE, "engines", "measure.py"), "run", "--spec", spec, "--root",
               os.path.abspath(args.engines_root), "--results", results, "--work", work] + ref
        p = subprocess.run(cmd, capture_output=True, text=True)
        line = measured(p.stdout)
        out.write((json.dumps(line) if line else f"{r['id']}: no result: {p.stderr[-500:]}") + "\n")
        out.flush()
    return 0


def main(argv=None, out=sys.stdout):
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--plan", required=True, help="a plan file, or a name under bench/node/plans")
    mode = p.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true")
    mode.add_argument("--go", action="store_true", help="create the run's resources: the PM's go-ahead was given")
    mode.add_argument("--local", action="store_true")
    p.add_argument("--vhd", default="", help="the node's fixed VHD (package.py --formats vhd)")
    p.add_argument("--tasks", default="", help="the gate's native tasks (images/gate/tasks/build.sh)")
    # 0.6.6 has no tag yet: its release commit and the CI commits after it, fix/adaptive-runtime's head.
    p.add_argument("--ref066", default="8018c96ab02867c7201450a9375ce3d9bc4da00e",
                   help="0.6.6's commit, built on the Linux machine")
    p.add_argument("--stamp", default="", help="yyyymmddHHMM; now, in UTC, by default")
    p.add_argument("--engines-root", default="", help="--local: install.py's root")
    p.add_argument("--reference-json", default="", help="--local: a canon.py reference to check outputs against")
    p.add_argument("--results", default="")
    a = p.parse_args(argv)
    path = a.plan if os.path.exists(a.plan) else plans.default_plan_path(a.plan)
    plan = plans.load(path)
    stamp = a.stamp or datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%d%H%M")
    if a.local:
        if not a.engines_root or not a.results:
            p.error("--local needs --engines-root and --results")
        return local(plan, a, out)
    m = plan["machine"]
    cloud = azure.Cloud(stamp, size=m["size"], zone=str(m["zone"]),
                        linux_image=m.get("image", azure.Cloud.linux_image), os_disk_gib=m.get("os_disk_gib", 128))
    results = os.path.abspath(a.results or os.path.join(HERE, "results", stamp))
    if a.dry_run:
        Bench(plan, DryRun(out), cloud, a, results, "<secrets>").go()
        return 0
    if not (a.vhd and a.tasks):
        p.error("a real run needs --vhd and --tasks")
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
