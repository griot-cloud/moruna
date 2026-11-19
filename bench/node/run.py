"""The node benchmark from one command on the Mac (system design 6, work items 6.1, 6.2 and 6.2b).

    bench/node/run.py --plan PLAN --dry-run [--vhd F] [--tasks DIR] [--from ADDRESS] [--ref066 REF]
                      [--wheel066 WHEEL] [--data-from-mac] [--stamp yyyymmddHHMM] [--engines a,b,...]
    bench/node/run.py --plan PLAN --go --vhd F --tasks DIR --from ADDRESS [--ref066 REF] [--wheel066 WHEEL]
                      [--data-from-mac] [--engines a,b,...]
    bench/node/run.py --plan PLAN --local --engines-root DIR --results DIR [--store-endpoint URL]
                      [--reference-json F] [--engines a,b,...]
    bench/node/run.py --plan PLAN --table bench/node/results/<stamp> [bench/node/results/<stamp> ...]
                      [--results DIR]

`--engines a,b,...` runs only those engines' runs of the plan, into a new results directory, so a
rerun after a fix, or of the node with a new image, repeats nothing else (work item 6.2c); when
0.6.6 is not one of them, its learning runs come too, since they make on the new machine the
reference every output is checked against, and no median counts them. The Linux machine installs
only what those runs need, and the run stops before any measured run when one does not install.
Without `moruna-node` among them, no node is made and --vhd, --tasks and --from are not needed.
`--table A B ...` then merges the directories, the newest record of each run (by each directory's
stamp, its run.json or its name), printed and, with `--results`, written there.

Each run's record lands in `bench/node/results/<stamp>/` as the run finishes (`node/` for Moruna's,
`live/` for the Linux machine's, from measure.py's summary line), so `--table` gives the table of
the runs so far while the plan goes on; at the end the Linux machine's full records replace the
live ones.

One run, in order, every step on the run's resource group in `griot-dev` (azure.py):

1. **Provision:** the group; a storage account with containers for the node's disk, the data
   (the one container every engine reads its inputs from and writes its output to), the code,
   the functions and the results; the harness and the plan's runs uploaded; and one Linux machine
   (Ubuntu 24.04) for the other engines, the plan's size in its zone.
2. **Two sides at once,** since they are two machines; each side makes its runs one at a time.
   - **The Linux machine** runs its steps as detached batches (engines/batch.py, each started by
     one managed run command (v2) as a systemd unit, so no command outlives Azure's 90 minutes,
     its values given as protected parameters, and followed from here through the run's
     container, `results/batch/NAME.json`):
     - `setup`: the plan's data, made in Azure (engines/data-linux.sh: fetched from its public
       source, normalised by bench/vm/normalize.py, each file checked against the plan's SHA-256,
       uploaded to the container), then every engine installed with its Blob reader
       (engines/setup-linux.sh; 0.6.6's wheel from --wheel066, else PyPI, else built there);
     - `runs`: every Linux run of the plan, in its order, each a fresh measure.py process, as each
       run-command ran one before (engines/measure.py), the container's account and key given to
       it as environment variables. Each run's summary line lands in `live/` as it ends.
   - **The node**, from its VHD (`kernel/node/AZURE.md`'s steps, as `azure-up.sh --proof` runs
     them: TCP 443 from `--from` alone), ready when GET /v1/health answers 200; its pin from its
     console (the boot diagnostics blob, read directly with the account's key), which the
     certificate it serves must match (openssl s_client, 1E); its instance the machine's vmId,
     cross-checked with the console's; the gate tasks
     registered as its functions as proof.py does (3E: each in container `functions`, at its SAS
     address, by its SHA-256 and a signature with the run's function key); then, once the data is
     in the container, Moruna's runs in the plan's order, each through its API as proof.py runs a
     job (its document, the account key by secret name, its events followed over SSE to the exit).
3. **Check** each node output against 0.6.6's on the Linux machine, as one more batch (`checks`)
   after the Linux runs, so no check runs beside a measured run.
4. **Collect** the records into `bench/node/results/<stamp>/`, the table into its `table.md`, and
   the run's entry, with a root-cause entry for each miss (rootcause.py), into
   `architecture/benchmark-log.md`. When a step fails, what the Linux machine holds is still
   collected (summary.json and table.md, no log entry) before the tear-down; a failure on either
   side stops the other at its next run.
5. **Tear down:** the resource group is deleted, always, even when a step failed, and shown gone.

`--data-from-mac` uploads the plan's data from `source_dir` instead (as before 6.2b, at the Mac's
upload speed): the fallback when the public source cannot be fetched or does not normalise to the
plan's SHA-256s, which stops a run before any measured run. `--wheel066` gives the Linux machine
0.6.6's wheel (a run that builds it keeps it in its results directory for the next one).

`--dry-run` prints every command of that run, in order (the Linux side's first, then the node's),
and does nothing else: no `az` command runs, nothing is created and no file is written. Values
only a real run learns (the storage key, the node's address) are printed as `<names>`. The image,
the tasks and the operator's address are checked when given, as a real run checks them before it
creates anything. Creating resources needs the PM's go-ahead each time (1E), so a real run needs
`--go`, which says it was given.

No secret is shown or kept: the storage key and every SAS are replaced by their names in what is
printed, in commands.log and in every record, the functions' SAS also by its signature alone
(proof.Redact). On the Linux machine they live in the batch's env file (mode 600), never in a
command line.

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
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
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


def _engines_install():
    """engines/install.py, by its path: bench/vm has an install.py of its own, ahead on the path."""
    import importlib.util
    spec = importlib.util.spec_from_file_location("engines_install", os.path.join(HERE, "engines", "install.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


install = _engines_install()

ENG, CODE, DATA = "/opt/engines", "/opt/bench", "/data"
MEASURE = [f"{ENG}/venv-harness/bin/python", f"{CODE}/bench/node/engines/measure.py"]
JAVA_HOME = "/usr/lib/jvm/java-17-openjdk-amd64"
# The node is ready when GET /v1/health answers 200, asked every 10 s for at most 10 minutes
# (AZURE.md's bound, azure-up.sh step 7); then its console is read every 10 s, for at most 10
# minutes more, until it shows the pin and `moruna: ready`.
HEALTH_WAIT = (10, 600)
CONSOLE_WAIT = (10, 600)
MiB, GiB = 1 << 20, 1 << 30
LOG = os.path.join(REPO, "architecture", "benchmark-log.md")
# The Linux machine's batches (engines/batch.py): their state, steps and env files.
BATCH = f"{DATA}/batch"
# The variables the starting run command writes to a batch's env file (mode 600): the machine's own
# PATH and HOME, as each run had them under run-command, and the values the steps name, which the
# start is given as protected parameters (azure.run_command_create), so they reach it as its
# environment, never as text of its script.
BATCH_ENV = ("PATH", "HOME", "BASE", "SAS_CODE", "SAS_RESULTS", "BENCH_KEY")
# A run's step is killed by the batch at the run's own timeout and this much more: measure.py's
# fetch of the output, its canonical form and the comparison, all outside every timing.
MEASURE_SLACK_S = 900
# The other steps' bounds: the data (fetch, normalise, check, upload); the engines' setup, with
# 0.6.6's build (45-60 min) when no wheel is found; runs.json; one node output's check.
DATA_BOUND_S, SETUP_BOUND_S, FETCH_BOUND_S, CHECK_BOUND_S = 1800, 3 * 3600, 300, 1800
# How long past a step's bound the harness waits for the batch's word before it asks the machine.
STALL_S = 600
# How often each batch's state is read from the container.
POLL_S = {"setup": 30, "runs": 30, "checks": 20}
# The install.py engine a plan's engine runs in, where the names differ.
INSTALL_OF = {"duckdb-sql": "duckdb", "polars-expr": "polars"}


class DryRun:
    """Prints each step; answers with `<names>` where a real run would learn a value. The two
    sides run one after the other, the Linux machine's first."""

    answers = False  # its answers are made up: what the machine says is not judged

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

    def parallel(self, sides, stop):
        """The sides one after the other; the first failure stops the rest."""
        for side in sides:
            if stop.is_set():
                return
            try:
                side()
            except BaseException:
                stop.set()
                raise

    def wait_for(self, event, stop, what):
        self.note(f"the node's runs wait until {what}")
        return event.is_set()

    def health(self, address):
        self.step("api", f"GET https://{address}/v1/health (no token, any certificate: readiness only)")
        return True

    def served_pin(self, address):
        o = node_api.OPENSSL
        self.step("local", f"{o} s_client -connect {address}:443 | {o} x509 -outform DER | {o} dgst -sha256 -r")
        return "<served-pin>"

    def read_text(self, path):
        return ""

    def read_state(self, path, steps):
        """A batch's state as the machine would put it: every step ended."""
        return {"state": "done", "steps": [{"id": st["id"], "exit": 0, "stdout": ""} for st in steps]}


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
    in what is said, logged and saved. The two sides run at once, each in a thread of its own."""

    answers = True
    # After one side fails, how long the others are given to reach their next step; how often a
    # side that waits looks whether the other has stopped.
    WIND_DOWN_S, WAIT_SLICE_S = 60, 5

    def __init__(self, cloud, log_path, out=sys.stdout):
        self.cloud, self.log, self.out = cloud, open(log_path, "a"), out
        self.transport, self.secrets = None, {}
        self.lock = threading.Lock()

    def redact(self, text, extra=None):
        return proof.Redact({**self.secrets, **(extra or {})})(text)

    def say(self, text):
        text = self.redact(text)
        with self.lock:
            self.out.write(text + "\n")
            self.out.flush()

    def _run(self, cmd, text):
        with self.lock:
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
        """A file of the run's own, readable by this user alone (some hold secrets)."""
        with open(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), "w") as f:
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

    def parallel(self, sides, stop):
        """Every side at once, each in a thread; returns when all have ended. The first failure
        sets `stop`, which each side reads before its next run, and is raised when the others have
        ended, or WIND_DOWN_S after it at most (a node's job is not cut short: the group goes)."""
        errors = []

        def run(side):
            try:
                side()
            except BaseException as e:  # noqa: BLE001 (raised from the calling thread)
                errors.append(e)
                stop.set()

        threads = [threading.Thread(target=run, args=(s,), name=getattr(s, "__name__", "side"), daemon=True)
                   for s in sides]
        for t in threads:
            t.start()
        try:
            failed_at = None
            while True:
                alive = [t for t in threads if t.is_alive()]
                if not alive:
                    break
                if errors and failed_at is None:
                    failed_at = time.time()
                if failed_at is not None and time.time() - failed_at > self.WIND_DOWN_S:
                    break
                alive[0].join(min(1.0, self.WIND_DOWN_S))
        except BaseException:  # Ctrl-C here: the sides stop at their next run, and the run tears down
            stop.set()
            raise
        if errors:
            raise errors[0]

    def wait_for(self, event, stop, what):
        if not event.is_set():
            self.note(f"the node's runs wait until {what}")
        while not event.wait(self.WAIT_SLICE_S):
            if stop.is_set():
                return False
        return True

    def read_state(self, path, steps):
        """A batch's state as `az` downloaded it to `path`, or None; the file is removed."""
        try:
            return json.loads(self.read_text(path) or "")
        except ValueError:
            return None

    def read_text(self, path):
        """What `az` downloaded to `path`, or None; the file is removed."""
        try:
            with open(path, encoding="utf-8", errors="replace") as f:
                return f.read()
        except OSError:
            return None
        finally:
            try:
                os.remove(path)
            except OSError:
                pass

    def health(self, address):
        return node_api.health(address) == 200

    def served_pin(self, address):
        return node_api.served_pin(address)


class SpecBlob(proof.Blob):
    """proof.py's Blob data for one run of a plan: the spec's inputs, its output prefix."""

    def __init__(self, store, spec):
        super().__init__(store)
        self.spec = spec

    def inputs(self, _period):
        return [self.store.az(n) for n in self.spec["inputs"]]

    def output(self, _run):
        return self.store.az(self.spec["output"].rstrip("/") + "/")


def batch_start(name):
    """The script that starts batch `name` on the Linux machine and returns at once: the harness
    (once) and the batch's steps from container `code`, its env file from the environment the run
    command gives it (BATCH_ENV), then batch.py as systemd unit `moruna-bench-NAME`. It prints
    `STARTED NAME`, or `ALREADY NAME` when that batch was started before, so a repeated command
    never runs a second batch beside the first. It holds no value: each is quoted where used."""
    unit, steps, env = f"moruna-bench-{name}", f"{BATCH}/{name}.steps.json", f"{BATCH}/{name}.env"
    batch = f"{CODE}/bench/node/engines/batch.py"
    started = f"{BATCH}/{name}.started"
    return "; ".join([
        "set -eu",
        f"mkdir -p {CODE} {BATCH}",
        f"chmod 700 {BATCH}",
        f"if [ -e {started} ]; then echo ALREADY {name}; exit 0; fi",
        f'if [ ! -f {batch} ]; then curl -sSf "$BASE/code/harness.tar.gz?$SAS_CODE" | tar -xz -C {CODE}; fi',
        f'curl -sSf -o {steps} "$BASE/code/batch-{name}.json?$SAS_CODE"',
        "umask 077",
        "printf '%s=%s\\n' " + " ".join(f'{k} "${{{k}:-}}"' for k in BATCH_ENV) + f" > {env}",
        f"touch {started}",
        f"systemd-run --unit={unit} --description='Moruna benchmark, batch {name}' /usr/bin/python3 {batch} "
        f"--name {name} --steps {steps} --env {env} --dir {BATCH} --logs {DATA}/results/batch/{name} "
        f"|| {{ rm -f {started}; exit 1; }}",
        f"echo STARTED {name}"])


class Bench:
    def __init__(self, plan, x, cloud, args, results, secrets_dir):
        self.plan, self.x, self.c, self.a = plan, x, cloud, args
        self.results, self.sec = results, secrets_dir
        # `--engines`: those engines' runs alone, and 0.6.6's learning runs when the reference
        # must be made again (plan.select).
        self.engines = getattr(args, "engines", None)
        self.runs, self.reference_added = plans.select(plan, plans.runs(plan, DATA), self.engines)
        self.by_id = {r["id"]: r for r in self.runs}
        self.with_node = any(r["where"] == "node" for r in self.runs)
        # The run's own table and log entry show the engines it was asked for (0.6.6's learning runs
        # count in no row); --table merges it with the run it completes.
        self.table_plan = dict(plan, engines=[e for e in plan["engines"] if not self.engines or e in self.engines])
        self.from_mac = bool(getattr(args, "data_from_mac", False))
        self.wheel066 = getattr(args, "wheel066", "") or ""
        # Every secret the run learns, by name: the executor redacts them wherever they would show.
        self.secrets, self.setup, self.collected = {}, None, False
        x.secrets = self.secrets
        self.node, self.boot_ms, self.budget = None, None, None
        # `stop`: a side failed, the other stops at its next run; `data_ready`: the node may start.
        self.stop, self.data_ready = threading.Event(), threading.Event()
        self.node_recs, self.polls = {}, 0
        # The node's console: its blob once known, the reads so far, the last note said of it.
        self.console_at, self.console_reads, self.last_note = None, 0, None

    def base(self):
        return f"https://{self.c.account}.blob.core.windows.net"

    def tasks(self):
        return getattr(self.a, "tasks", "") or "<images/gate/tasks/build.sh's output>"

    def operator(self):
        return getattr(self.a, "operator", "") or "<operator-address>"

    def where(self, side):
        return [r for r in self.runs if r["where"] == side]

    # 1. Provision ---------------------------------------------------------------------------------
    def prepare(self):
        x, sec = self.x, self.sec
        m = self.plan["machine"]
        machines = 2 if self.with_node else 1
        x.note(f"run {self.c.stamp}: group {self.c.group} in {self.c.subscription}, {len(self.runs)} runs; "
               f"{machines} machines of {self.c.size} at once, {machines * m['cpus']} vCPUs")
        side = f"the Linux machine's {len(self.where('linux'))} runs as one detached batch, one at a time"
        if self.with_node:
            side += (f"; the node's {len(self.where('node'))} alongside, one at a time, once the data is in "
                     "the container")
        x.note(side)
        if self.engines:
            x.note(f"--engines {','.join(self.engines)}: those engines' runs of the plan alone, into a new results "
                   f"directory; run.py --table merges it with the run it completes")
        if self.reference_added:
            x.note(f"and 0.6.6's learning runs, which make again the reference every output is checked against "
                   f"(no median counts them): {', '.join(self.reference_added)}")
        x.note(f"the Linux machine installs {', '.join(self.installs())}; the run stops before any measured run "
               "if one does not install")
        for line in getattr(self.a, "checked", ()):
            x.note(line)
        token_key = x.keygen(f"{sec}/token-key.pem")
        function_key = x.keygen(f"{sec}/function-key.pem")
        x.write(f"{sec}/node-config.json", json.dumps(node_api.node_config(token_key, [function_key]), indent=1))
        x.local(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", f"{sec}/linux-key"])
        x.local(["git", "-C", REPO, "archive", "--format=tar.gz", "-o", f"{sec}/harness.tar.gz", "HEAD"])
        if not self.wheel066:
            # 0.6.6's tree, for the build when PyPI has no 0.6.6 either.
            x.local(["git", "-C", REPO, "archive", "--format=tar.gz", "-o", f"{sec}/moruna-066.tar.gz", self.a.ref066])
        x.write(f"{sec}/runs.json", json.dumps(self.runs, indent=1))

    def provision(self):
        """What both sides need, and the Linux machine; the node's own resources are its side's
        first step (provision_node), so its upload of the image is not on the Linux side's way."""
        x, c, sec = self.x, self.c, self.sec
        x.az(azure.group_create(c))
        for cmd in azure.storage(c):
            x.az(cmd)
        s = self.secrets
        s["storage-key"] = key = x.az(azure.storage_key(c), out="storage-key")
        for cmd in azure.containers(c, key):
            x.az(cmd, secrets=s)
        if self.from_mac:
            src = os.path.expanduser(self.plan["dataset"]["source_dir"])
            for f in self.plan["dataset"]["files"]:
                # The one container every engine reads from and writes to (store.py): inputs under nyc/.
                x.az(azure.upload(c, key, "data", f"nyc/{f}", os.path.join(src, f)), secrets=s)
            self.data_ready.set()
        names = ["harness.tar.gz", "runs.json"] + ([] if self.wheel066 else ["moruna-066.tar.gz"])
        for name in names:
            x.az(azure.upload(c, key, "code", name, f"{sec}/{name}"), secrets=s)
        if self.wheel066:
            x.az(azure.upload(c, key, "code", os.path.basename(self.wheel066), self.wheel066), secrets=s)
        # Long enough for the slowest plan; the account goes with the group at the end. `code` is
        # written once, by the setup's 0.6.6 wheel; `functions` is read only, by the node (3E). Every
        # SAS is made here, before the sides start.
        expiry = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=48)).strftime("%Y-%m-%dT%H:%MZ")
        for container, perms in (("code", "racw"), ("results", "racwl")):
            s[f"sas-{container}"] = x.az(azure.sas(c, key, container, perms, expiry), out=f"sas-{container}", secrets=s)
        if self.with_node:
            s[proof.SAS_ENV] = x.az(azure.sas(c, key, "functions", "r", expiry), out=proof.SAS_ENV, secrets=s)
        for cmd in azure.nsgs(c, self.operator()):
            x.az(cmd)
        x.az(azure.linux_vm(c, f"{sec}/linux-key.pub"))

    def provision_node(self):
        """The node's own resources: the proof's functions (the gate tasks in `functions`, fetched
        by a read-only SAS), its image as a page blob, its disk, address and machine."""
        x, c, s = self.x, self.c, self.secrets
        key = s["storage-key"]
        for job in self.plan["jobs"]:
            x.az(azure.upload(c, key, "functions", f"gate-{job}", f"{self.tasks()}/gate-{job}"), secrets=s)
        x.az(azure.upload(c, key, "vhds", "node.vhd", getattr(self.a, "vhd", "") or "<node.vhd>", page=True),
             secrets=s)
        vhd = x.az(azure.blob_url(c, key, "vhds", "node.vhd"), out="vhd-url", secrets=s)
        x.az(azure.node_disk(c, vhd, x.az(azure.account_id(c), out="account-id")))
        x.az(azure.node_ip(c))
        x.az(azure.node_vm(c, f"{self.sec}/node-config.json"))

    def console(self):
        """The node's serial console, parsed (node_api.console), or None while it cannot be read:
        the boot diagnostics blob in the run's account, read directly with its key; the CLI's
        get-boot-log only when that fails (on 2026-11-18 it failed on every call, on the CLI's own
        bug). A `moruna: fatal` line stops the run."""
        text = self.console_text()
        if text is None:
            return None
        info = node_api.console(text or "")
        if info.get("fatal"):
            raise RuntimeError(f"the node stopped: moruna: fatal {info['fatal']}")
        return info

    def console_text(self):
        """The node's serial console as it stands, or None while it cannot be read (see console)."""
        x, c, s = self.x, self.c, self.secrets
        try:
            if self.console_at is None:
                uri = x.az(azure.boot_log_uri(c), out="serial-console-uri")
                self.console_at = (azure.console_blob(c, uri) if x.answers
                                   else ("<boot-diagnostics-container>", "<serial-console-log>"))
            self.console_reads += 1
            path = os.path.join(self.sec, f"console-{self.console_reads}.log")
            x.az(azure.blob_download(c, s["storage-key"], *self.console_at, path), secrets=s)
            text = x.read_text(path)
        except (RuntimeError, ValueError) as e:
            # Made with --no-wait: until the machine and its log exist, `az` says so; look again.
            self.once(f"the console's blob was not read ({str(e)[-160:]}); get-boot-log instead")
            try:
                text = x.az(azure.boot_log(c), out="console")
            except RuntimeError as e2:
                self.once(f"nor by get-boot-log: {str(e2)[-160:]}")
                return None
        return text

    def keep_meters(self, spec):
        """Item 6.3ab: the node's network meters as they stood when a job ended, beside its
        record. The network server and netvsc say a meter line at most every 10 s while bytes
        move, and the network server one more when a job's last TLS socket closes; each counts
        from its process's start, so the difference between two jobs' last lines is what the job
        moved. The console is read once more after the job, never during it."""
        text = self.console_text()
        if not text:
            return
        keep = [line for line in text.splitlines()
                if "netstack: meter" in line or "netvsc: meter" in line or "netstack: crypto" in line]
        self.x.save(os.path.join(self.results, "node", f"{spec['id']}.meters.json"), {"lines": keep})

    def once(self, text):
        """A note, unless it is the one said last (a poll says the same thing many times)."""
        if text != self.last_note:
            self.last_note = text
            self.x.note(text)

    def node_ready(self):
        """The node ready, as azure-up.sh finds it (1E): GET /v1/health answers 200; its console
        shows `moruna: tls sha256=` and `moruna: ready`; the certificate it serves is that pin
        (openssl s_client), or the run stops rather than connect unpinned; its instance is the
        machine's vmId, and the console's when it names one."""
        x, c = self.x, self.c
        address = x.value("node-address", x.az(azure.node_address(c), out="node-address"))

        def up():
            if x.health(address):
                return True
            self.console()  # a `moruna: fatal` line ends the wait
            return None

        if not x.poll("GET /v1/health answers 200 (or the console says `moruna: fatal`)", *HEALTH_WAIT, up):
            raise RuntimeError(f"the node did not answer GET https://{address}/v1/health with 200 within "
                               f"{HEALTH_WAIT[1]} s")

        def lines():
            info = self.console() or {}
            return info if (info.get("pin") and "boot_ms" in info) or not x.answers else None

        info = x.poll("its console shows `moruna: tls sha256=` and `moruna: ready`", *CONSOLE_WAIT, lines)
        if info is None:
            raise RuntimeError(f"the node answers on {address}, but its console was not read within "
                               f"{CONSOLE_WAIT[1]} s: without its pin the run does not connect (1E)")
        pin = x.value("node-pin", info.get("pin")).lower()
        served = x.served_pin(address)
        if x.answers and served != pin:
            raise RuntimeError(f"the certificate {address} serves (sha256 {served or 'not read'}) is not the one its "
                               f"console printed ({pin}): the run does not connect (1E)")
        vm_id = x.value("instance-id", x.az(azure.node_vm_id(c), out="instance-id"))
        if x.answers and info.get("instance") and info["instance"].lower() != vm_id.lower():
            raise RuntimeError(f"the console's instance {info['instance']} is not the node machine's vmId {vm_id}")
        x.connect(f"https://{address}", pin)
        self.boot_ms = info.get("boot_ms")
        # The tokens' audience is the instance as the node itself names it.
        tokens = x.tokens(f"{self.sec}/token-key.pem", info.get("instance") or vm_id)
        self.node = node_api.Node(x.transport, tokens)
        facts = self.node.node()
        # The most the node takes (3A: a larger budget is refused), as proof.py gives every job.
        bound = facts.get("job_memory_bytes") or (facts["memory_bytes"] - proof.NODE_KEEPS
                                                  if facts.get("memory_bytes") else None)
        self.budget = x.value("job-memory-bytes", bound)
        x.note(f"node {facts.get('instance', '<instance>')}, version {facts.get('version', '<version>')}: "
               f"each job's budget {self.budget} bytes")

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

    # The Linux machine's batches ----------------------------------------------------------------
    def batch_params(self):
        """The starting run command's protected parameters: what its env file keeps (BATCH_ENV)."""
        s = self.secrets
        return {"BASE": self.base(), "SAS_CODE": s["sas-code"], "SAS_RESULTS": s["sas-results"],
                "BENCH_KEY": s["storage-key"]}

    def param_files(self):
        """Each protected parameter's value in a file of the secrets directory (mode 600), for az
        to read itself (`NAME=@FILE`): no value is on a command line."""
        files = {}
        for k, v in self.batch_params().items():
            files[k] = os.path.join(self.sec, f"param-{k}")
            self.x.write(files[k], v)
        return files

    def hide(self, text):
        """`text` with every secret the run holds replaced by its name."""
        return proof.Redact(self.secrets)(text or "")

    def store_env(self):
        """The run's container, as measure.py and the engines read it (store.py); its key is the
        secret BENCH_KEY, from the batch's env file."""
        return {"BENCH_ACCOUNT": self.c.account, "BENCH_CONTAINER": "data"}

    def batch(self, name, steps, on_step, after_start=None):
        """`steps` run on the Linux machine as one detached batch, started by one run-command and
        followed from here: `on_step(step, ended)` as each step's end is read from the container.
        A step that outlasts its bound by STALL_S more, with no word from the batch, stops the run,
        with what the machine says of the unit."""
        x, s, c = self.x, self.secrets, self.c
        path = os.path.join(self.sec, f"batch-{name}.json")
        x.write(path, json.dumps(steps, indent=1))
        x.az(azure.upload(c, s["storage-key"], "code", f"batch-{name}.json", path), secrets=s)
        unit = f"moruna-bench-{name}"
        try:
            x.az(azure.run_command_create(c, unit, batch_start(name), self.param_files()), secrets=s)
            view = x.az(azure.run_command_view(c, unit), out=f"{unit}'s instance view", secrets=s)
        finally:
            # Read, the run command is removed from the machine's model; the unit it started goes on.
            try:
                x.az(azure.run_command_delete(c, unit), secrets=s)
            except RuntimeError as e:
                x.note(f"the run command {unit} was not deleted (the group's tear-down removes it): {str(e)[-200:]}")
        if x.answers:
            try:
                view = json.loads(view or "{}") or {}
            except ValueError:
                view = {"error": f"not JSON: {view}"}
            if not re.search(rf"^(STARTED|ALREADY) {name}$", view.get("output") or "", re.M):
                said = f"{view.get('output') or ''}\n{view.get('error') or ''}".strip()
                raise RuntimeError(self.hide(
                    f"the batch {name} did not start on the Linux machine ({view.get('executionState')}, exit "
                    f"{view.get('exitCode')}): {said[-500:]}"))
        x.note(f"the Linux machine runs batch {name}: {len(steps)} steps, one at a time")
        if after_start:
            after_start()
        by_id = {st["id"]: st for st in steps}
        seen, since = [0], [time.time()]

        def look():
            if self.stop.is_set():
                return {"state": "stopped"}
            state = self.batch_state(name, steps)
            ended = (state or {}).get("steps") or []
            for rec in ended[seen[0]:]:
                on_step(by_id[rec["id"]], rec)
            if len(ended) > seen[0]:
                seen[0], since[0] = len(ended), time.time()
            if state and state.get("state") in ("done", "failed"):
                x.save(os.path.join(self.results, "batch", f"{name}.json"), state)
                if state["state"] == "failed":
                    raise RuntimeError(f"the batch {name} stopped on the Linux machine: {state.get('error')}")
                return state
            bound = (steps[seen[0]]["timeout_s"] if seen[0] < len(steps) else 0) + STALL_S
            if time.time() - since[0] > bound:
                self.stalled(name, bound)
            return None

        total = sum(st["timeout_s"] for st in steps) + STALL_S * (len(steps) + 1)
        got = x.poll(f"batch {name} ends (each step within its bound and {STALL_S} s more)", POLL_S[name], total, look)
        if got is None:
            raise RuntimeError(f"the batch {name} did not end within {total} s")
        return got

    def batch_state(self, name, steps):
        """The batch's state from `results/batch/NAME.json`, or None while there is none."""
        x, s = self.x, self.secrets
        self.polls += 1
        local = os.path.join(self.sec, f"state-{name}-{self.polls}.json")
        try:
            x.az(azure.blob_download(self.c, s["storage-key"], "results", f"batch/{name}.json", local), secrets=s)
        except RuntimeError as e:
            if "BlobNotFound" not in str(e):
                x.note(f"the state of batch {name} was not read, read again next time: {str(e)[-200:]}")
            return None
        return x.read_state(local, steps)

    def stalled(self, name, bound):
        out = self.x.az(azure.run_command(
            self.c, f"systemctl status moruna-bench-{name} --no-pager -n 20 || true; "
                    f"tail -c 1000 {BATCH}/{name}.state.json || true"), out="the unit's status", secrets=self.secrets)
        raise RuntimeError(self.hide(f"the batch {name}: no step ended within {bound} s of the last; the machine "
                                     f"says: {(out or '')[-800:]}"))

    # The Linux side ---------------------------------------------------------------------------------
    def linux_side(self):
        self.batch("setup", self.setup_steps(), self.setup_done)
        if not self.stop.is_set():
            self.batch("runs", self.linux_steps(), self.linux_done, after_start=self.keep_wheel)

    def installs(self):
        """What the Linux machine installs, in install.py's order: measure.py's own environment
        and the Rust pool's `gate-canon`, which makes and compares every check's canonical form,
        then the engine of each of the run's Linux runs (0.6.6's Rust function runs in 0.6.6's
        environment)."""
        need = {"harness", "rust-pool"}
        need.update(INSTALL_OF.get(r["engine"], r["engine"]) for r in self.runs if r["where"] == "linux")
        if "moruna066-rust" in need:
            need.add("moruna066")
        return [e for e in install.ENGINES if e in need]

    def setup_steps(self):
        """The data (unless it came from the Mac), then the engines (setup-linux.sh), then the
        plan's runs.json: every one outside every timing."""
        st, d = [], self.plan["dataset"]
        if not self.from_mac:
            spec = {"source_url": d["source_url"], "sha256": d["sha256"], "prefix": "nyc/",
                    "normalize": f"{CODE}/bench/vm/normalize.py"}
            st.append({"id": "data", "argv": ["bash", f"{CODE}/bench/node/engines/data-linux.sh", d["pyarrow"],
                                              json.dumps(spec, separators=(",", ":"))],
                       "env": self.store_env(), "secrets": ["BENCH_KEY"], "timeout_s": DATA_BOUND_S})
        env = {"BASE": self.base(), "V066": getattr(self.a, "version066", "") or "0.6.6",
               "ENGINES": ",".join(self.installs())}
        if self.wheel066:
            env["WHEEL066"] = os.path.basename(self.wheel066)
        st.append({"id": "setup", "argv": ["bash", f"{CODE}/bench/node/engines/setup-linux.sh"], "env": env,
                   "secrets": ["SAS_CODE"], "timeout_s": SETUP_BOUND_S})
        fetch = f'curl -sSf -o {DATA}/runs.json "$BASE/code/runs.json?$SAS_CODE"'
        st.append({"id": "runs-json", "argv": ["bash", "-c", fetch], "env": {"BASE": self.base()},
                   "secrets": ["SAS_CODE"], "timeout_s": FETCH_BOUND_S})
        return st

    def setup_done(self, step, ended):
        x = self.x
        if step["id"] == "data":
            got = tagged(ended.get("stdout"), "DATA ") or {
                "ok": False, "error": ended.get("error") or f"no DATA line, exit {ended.get('exit')}: "
                                                             f"{(ended.get('stderr') or '')[-300:]}"}
            x.save(os.path.join(self.results, "data.json"), got)
            if x.answers and not got.get("ok"):
                raise RuntimeError(f"the data was not made in Azure: {got.get('error')}. Run again with "
                                   "--data-from-mac to upload the Mac's copies")
            took = "" if got.get("fetch_s") is None else (
                f": fetched in {got['fetch_s']} s, normalised in {got.get('normalize_s')} s, "
                f"uploaded in {got.get('upload_s')} s")
            x.note(f"the data is in the container, each file the plan's SHA-256{took}")
            self.data_ready.set()
        elif step["id"] == "setup":
            image = x.az(azure.image_version(self.c), out="linux-image-version")
            self.setup = dict(tagged(ended.get("stdout"), "SETUP ") or {}, linux_image=image)
            if x.answers and ended.get("exit") != 0:
                # A setup that failed keeps what the batch saw of it, beside each engine's own
                # error (the SETUP line's `errors` and `tried`): setup.json then says why.
                self.setup.update(exit=ended.get("exit"),
                                  stderr=(ended.get("stderr") or ended.get("error") or "")[-1500:])
            x.save(os.path.join(self.results, "setup.json"), self.setup)
            if x.answers:
                self.refuse_setup(ended)
        elif x.answers and ended.get("exit") != 0:
            raise RuntimeError(f"the setup's step {step['id']} failed: exit {ended.get('exit')}: "
                               f"{(ended.get('stderr') or ended.get('error') or '')[-300:]}")

    def refuse_setup(self, ended):
        """The run goes on only when every engine it runs installed (work item 6.2c): one that did
        not stops it before any measured run, with why each failed, rather than failing every one
        of its runs. The why is setup-linux.sh's SETUP line, already in setup.json: each engine's
        error and, for one tried on several interpreters, each interpreter's own last lines."""
        setup = self.setup or {}
        installed = setup.get("installed") or {}
        missing = [e for e in self.installs() if not installed.get(e)]
        if not missing and ended.get("exit") == 0:
            return
        errors, tried = setup.get("errors") or {}, setup.get("tried") or {}

        def said(engine):
            # The message has the end of each; setup.json has all the setup's line kept.
            each = "; ".join(f"on CPython {t['python']}: {str(t['error'])[-200:]}" for t in tried.get(engine) or ())
            return each or errors.get(engine) or "no record"
        why = "; ".join(f"{e}: {said(e)}" for e in missing) or (
            f"exit {ended.get('exit')}: {(ended.get('stderr') or ended.get('error') or '')[-300:]}")
        raise RuntimeError(f"the setup did not install {', '.join(missing) or 'every engine'}, so the run stops "
                           f"before any measured run: {why}")

    def keep_wheel(self):
        """0.6.6's wheel, when the Linux machine built it, kept beside the results for the next
        run's --wheel066 (a build takes 45-60 min); fetched while the runs go on."""
        setup = self.setup or {}
        name = setup.get("wheel_066_name")
        if setup.get("wheel_066") != "built" or not name:
            return
        path = os.path.join(self.results, name)
        try:
            key = self.secrets["storage-key"]
            self.x.az(azure.blob_download(self.c, key, "code", name, path), secrets=self.secrets)
        except RuntimeError as e:
            self.x.note(f"0.6.6's built wheel was not kept: {str(e)[-200:]}")
            return
        self.x.note(f"0.6.6's wheel, built on the Linux machine in {setup.get('wheel_066_s')} s, is kept at {path}: "
                    f"the next run takes it with --wheel066 {path}")

    def reference_args(self, spec):
        ref = f"{DATA}/ref/{spec['job']}.dump"
        if spec["engine"] == plans.REFERENCE and spec["repeat"] == 0:
            return ["--make-reference", ref]
        return ["--reference-dump", ref]

    def linux_steps(self):
        """Every Linux run of the plan, in its order: measure.py's command as each run-command ran
        it before, a fresh process each, with the container's account and key."""
        blob = self.plan["linux_data"] == "blob"
        steps = []
        for spec in self.where("linux"):
            cmd = MEASURE + ["run", "--spec", f"{DATA}/runs.json", "--id", spec["id"], "--root", ENG, "--results",
                             f"{DATA}/results", "--work", f"{DATA}/work"] + self.reference_args(spec)
            env = dict(self.store_env() if blob else {}, JAVA_HOME=JAVA_HOME)
            steps.append({"id": spec["id"], "argv": cmd, "env": env, "secrets": ["BENCH_KEY"] if blob else [],
                          "timeout_s": spec["timeout_s"] + MEASURE_SLACK_S})
        return steps

    def linux_done(self, step, ended):
        """A Linux run's record lands here as it ends, from measure.py's summary line, so a partial
        table can be made while the plan runs (`--table`); the full one replaces it at the end."""
        spec = self.by_id[step["id"]]
        got = measured(ended.get("stdout"))
        if got:
            self.x.say(f"      {json.dumps(got)}")
        detail = ended.get("error") or (ended.get("stderr") or "").strip()[-300:] or None
        self.x.save(os.path.join(self.results, "live", f"{spec['id']}.json"), live_record(spec, got, detail))

    # The node's side --------------------------------------------------------------------------------
    def node_side(self):
        self.provision_node()
        self.node_ready()
        self.register()
        if not self.x.wait_for(self.data_ready, self.stop, "the data is in the container"):
            return
        for spec in self.where("node"):
            if self.stop.is_set():
                return
            self.x.note(f"run {spec['id']}")
            self.moruna(spec)
            self.keep_meters(spec)

    def moruna(self, spec):
        """One job on the node, as proof.py runs one: its document (the registered function by
        name, the inputs and output in the run's container, the account key by secret name), its
        events followed over SSE to the exit. Every engine's operational time is its own wall on
        the machine that ran it (the PM, 2026-11-18): Moruna's is its report's `wall_ms`. Its
        start-up is the node's `submitted` to `started`: to the millisecond from their `_ms` fields
        (item 2.4), or on a node without them whole seconds, so within 1 s. Every mark
        is kept: this machine's end-to-end time (the answer to the exit seen), the boot's
        `boot_ms` and the first rows at the sink among them. A job the node refuses, or that is
        not followed to its end, is a failed run, and the next run goes on. Its output is checked
        against 0.6.6's after the Linux runs (check_node_outputs)."""
        x = self.x
        store = stores.Store(self.c.account, self.secrets.get("storage-key", "<storage-key>"), "data")
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
        # The node's times, to the millisecond where it gives them (item 2.4).
        times = node_api.job_times(j)
        started, submitted = times["started"], times["submitted"]
        to_ms = j.get("started_ms") is not None and j.get("submitted_ms") is not None
        startup = None if started is None or submitted is None else round(started - submitted, 3)
        rec.update({
            "state": j.get("state"), "exit": got.exit,
            "ok": j.get("state") == "completed" and got.exit.get("code") == 0,
            "operational_s": round(wall_ms / 1e3, 3) if isinstance(wall_ms, (int, float)) else None,
            "operational_by": "the engine's report, wall_ms",
            "startup_s": startup,
            "startup_by": ("the node's job record, submitted_ms to started_ms" if to_ms
                           else "the node's job record, submitted to started, whole seconds"),
            # Whole seconds are marked ± in the table; milliseconds, as the others' times, are not.
            "startup_within_s": None if to_ms else 1,
            "marks": {"sent": t_sent, "answered": t_answered, "exit_seen": t_seen,
                      "end_to_end_s": round(t_seen - t_answered, 3),
                      "node_times": times,
                      "wall_ms": wall_ms, "first_rows_s": node_api.first_rows_s(got.events()),
                      "resumed": got.resumed},
            # The node's CPU-seconds for the job are its report's `resources.cpu_core_s`, when the
            # report says it measured them; run 3's node said `usage_measured: false` with 0.0,
            # which is no measurement (docs/run-report.md: a measurement not made is absent).
            "cpu_s": resources.get("cpu_core_s") if resources.get("usage_measured", True) else None,
            "cpu_by": ("the engine's report, resources.cpu_core_s" if resources.get("usage_measured", True) else
                       "not measured: the report's resources.usage_measured is false"),
            "peak_bytes": report.get("peak_anon_bytes") or resources.get("peak_memory_bytes"),
            "split": collect.report_split(report),
            "report_budget": report.get("budget"), "object_io": report.get("object_io")})
        if not rec["ok"]:
            rec["diagnostic"] = got.exit.get("diagnostic")
        x.save(os.path.join(self.results, "node", f"{spec['id']}.job.json"),
               {"job": j, "report": report, "events": got.events(), "exit": got.exit})
        return self.keep(spec, rec)

    def keep(self, spec, rec):
        self.node_recs[spec["id"]] = rec
        self.x.save(os.path.join(self.results, "node", f"{spec['id']}.json"), rec)
        if not isinstance(self.x, DryRun):
            outcome = ((rec.get("output_check") or {}).get("verdict") or rec.get("error") or rec.get("diagnostic")
                       or "its output checked after the Linux runs")
            self.x.say(f"      {spec['id']}: {rec.get('state')}, operational {rec.get('operational_s')} s, {outcome}")
        return rec

    # 3. Check the node's outputs ----------------------------------------------------------------------
    def check_step(self, spec):
        argv = MEASURE + ["check", "--workload", spec["job"], "--out", f"{DATA}/work/node-{spec['id']}", "--id",
                          spec["id"], "--root", ENG, "--results", f"{DATA}/results", "--blob-prefix", spec["output"]]
        return {"id": spec["id"], "argv": argv + self.reference_args(spec), "env": self.store_env(),
                "secrets": ["BENCH_KEY"], "timeout_s": CHECK_BOUND_S}

    def check_node_outputs(self):
        """Each completed node run's output against 0.6.6's, on the Linux machine, as one batch
        after the Linux runs: measure.py's check, the reference's dump as every Linux run's."""
        steps = [self.check_step(self.by_id[rid]) for rid, rec in self.node_recs.items() if rec.get("ok")]
        if steps:
            self.batch("checks", steps, self.check_done)

    def check_done(self, step, ended):
        spec, rec = self.by_id[step["id"]], self.node_recs[step["id"]]
        verdict = (measured(ended.get("stdout")) or {}).get("output") or ""
        # As measure.py's own records say it, so the collector reads it alike (collect.output_verdict).
        rec["output_check"] = {"verdict": verdict, "ok": verdict.startswith(("bit for bit", "within")),
                               "hash_all": verdict == "bit for bit"}
        self.keep(spec, rec)

    # 4. Collect ------------------------------------------------------------------------------------
    def collect(self, log=True):
        """The Linux machine's records into the results, then the summary, the table and, unless the
        run stopped early (`log` false), the run's entry in the benchmark log. A batch still going
        (the run stopped early) is stopped first, so its files hold still."""
        x, s = self.x, self.secrets
        units = " ".join(f"moruna-bench-{n}" for n in POLL_S)
        # The address and its SAS from the setup batch's env file, on the machine (v1 run commands
        # carry no value: azure.run_command); `sed` prints each as written, quoted where used.
        env = f"{BATCH}/setup.env"
        script = (f"systemctl stop {units} 2>/dev/null || true; "
                  f"BASE=$(sed -n 's/^BASE=//p' {env}); SAS_RESULTS=$(sed -n 's/^SAS_RESULTS=//p' {env}); "
                  f"tar -czf {DATA}/results.tgz -C {DATA} results && curl -sSf -X PUT -H 'x-ms-blob-type: BlockBlob' "
                  f'-T {DATA}/results.tgz "$BASE/results/linux.tgz?$SAS_RESULTS"')
        x.az(azure.run_command(self.c, script), secrets=s)
        x.az(azure.blob_download(self.c, s["storage-key"], "results", "linux.tgz", f"{self.results}/linux.tgz"),
             secrets=s)
        x.local(["tar", "-xzf", f"{self.results}/linux.tgz", "-C", self.results])
        self.collected = True
        x.finish(self.table_plan, self.results, self.meta(), log=log)

    def meta(self):
        commit = subprocess.run(["git", "-C", REPO, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
        setup = self.setup or {}
        image = getattr(self.a, "image", None) or {}
        node_image = self.a.vhd
        if image:
            node_image = f"{self.a.vhd} ({image['bytes']} bytes, sha256 {image['sha256']})"
        made = ("uploaded from this machine" if self.from_mac else
                f"made in Azure from {self.plan['dataset'].get('source_url')}, each file the plan's SHA-256")
        # 0.6.6's wheel (source, name, SHA-256) is in the table itself (summary.json, table.md).
        meta = {"date": self.c.stamp, "commit": commit, "node_image": node_image if self.with_node else None,
                "machines": f"{self.c.size}, zone {self.c.zone}, {self.c.location}, subscription {self.c.subscription}",
                "linux_image": setup.get("linux_image"),
                "data": f"read and written in Blob container `data`, {made}",
                "results": os.path.relpath(self.results, REPO)}
        if self.engines:
            added = " and 0.6.6's learning runs, the reference" if self.reference_added else ""
            meta["engines"] = (f"{', '.join(self.engines)} alone (--engines){added}; run.py --table merges these runs "
                               "with the run they complete")
        return meta

    # 5. Tear down ----------------------------------------------------------------------------------
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
            self.x.parallel([self.linux_side] + ([self.node_side] if self.with_node else []), self.stop)
            if self.with_node:
                self.check_node_outputs()
            self.collect()
        except BaseException:
            self.stop.set()
            # What the Linux machine holds is kept before the group goes; the run is not logged.
            if self.setup is not None and not self.collected:
                try:
                    self.collect(log=False)
                except Exception as e:  # noqa: BLE001 (the first failure is the one raised)
                    self.x.note(f"the partial results were not collected: {e}")
            raise
        finally:
            self.teardown()


def live_record(spec, got, detail=None):
    """A Linux run's record from measure.py's summary line (the full one stays on the machine
    until the end): what the table needs, marked `live`, which collect.load lets the full one
    replace. A run with no summary line is a failed run; `detail` says what the batch saw."""
    got = got or {}
    rec = {"spec": spec, "live": True, "ok": bool(got.get("ok")), "memory_gib": spec.get("mem_gib")}
    for k in ("startup_s", "operational_s", "cpu_s", "peak_bytes", "oom_kills", "data", "copy_in_s", "copy_out_s",
              "exit_code", "error_lines", "python", "gil"):
        if got.get(k) is not None:
            rec[k] = got[k]
    rec["output_check"] = got.get("check") or {"verdict": got.get("output")}
    if not got:
        rec["error"] = "no summary line in the run's output" + (f": {detail}" if detail else "")
    return rec


def tagged(text, tag):
    """The JSON of the last line of `text` that starts with `tag` (`MEASURED `, `SETUP `, `DATA `)."""
    for line in reversed((text or "").splitlines()):
        if line.startswith(tag):
            return json.loads(line[len(tag):])
    return None


def measured(text):
    """The record's summary line from measure.py's output (the batch keeps its last 8 KiB)."""
    return tagged(text, "MEASURED ")


def finish(plan, results, meta, log=LOG):
    """The summary, the table and the run's log entry (none when `log` is None), from the records
    under `results`."""
    summary = collect.summarise(collect.load(results), plan)
    wheel = wheel_066(results)
    if wheel:
        summary["wheel_066"] = wheel
    collect.write(results, summary)
    open(os.path.join(results, "table.md"), "w").write(table.render(summary) + "\n")
    if log:
        misses = "\n".join(rootcause.entry(m, f"{meta['date']}, plan {plan['name']}")
                           for m in rootcause.misses(summary))
        table.append(log, table.entry(summary, meta, misses))
    return summary


def merged(plan, dirs, out_dir=None):
    """The table of several results directories as one (work item 6.2c): the newest record of each
    run (collect.merge), with where each came from. With `out_dir`, its summary.json and table.md
    are written there, and merged.json, the directory each run's record came from; no directory
    given is written to. Returns (the summary, the table's text)."""
    records, order, _ = collect.merge(dirs)
    summary = collect.summarise(records, plan)
    summary["sources"] = [{"dir": d, "stamp": collect.stamp_of(d), "runs": sum(r["from"] == d for r in records),
                           "wheel_066": wheel_066(d)} for d in order]
    text = table.render(summary)
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)
        collect.write(out_dir, summary)
        with open(os.path.join(out_dir, "table.md"), "w") as f:
            f.write(text + "\n")
        with open(os.path.join(out_dir, "merged.json"), "w") as f:
            json.dump({"dirs": order, "runs": {r["spec"]["id"]: r["from"] for r in records}}, f, indent=1)
    return summary, text


def wheel_066(results):
    """0.6.6's wheel as the Linux machine's setup recorded it (`setup.json`): where it came from
    (`container`: given with --wheel066; `pypi`; `built` on the machine), its name, its SHA-256 and
    the seconds it took. None before the setup has ended."""
    try:
        with open(os.path.join(results, "setup.json")) as f:
            setup = json.load(f)
    except (OSError, ValueError):
        return None
    if not isinstance(setup, dict) or not setup.get("wheel_066"):
        return None
    return {"source": setup["wheel_066"], "name": setup.get("wheel_066_name"),
            "sha256": setup.get("wheel_066_sha256"), "seconds": setup.get("wheel_066_s")}


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
    every, _ = plans.select(plan, plans.runs(plan, DATA), getattr(args, "engines", None))
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


def data_facts(plan, from_mac):
    """Where the run's data comes from, checked: made in Azure from the plan's public source
    (the plan must name it and every file's SHA-256), or with `from_mac` the Mac's copies in
    `source_dir`, each the plan's SHA-256 when the plan gives them. A line to note; a SystemExit
    says what is wrong."""
    d = plan["dataset"]
    files = d["files"]
    if not from_mac:
        if not d.get("source_url"):
            raise SystemExit("run.py: the plan names no public source for its data (dataset.source_url): "
                             "run with --data-from-mac")
        return (f"data: {len(files)} files made in Azure by the Linux machine from {d['source_url']}: normalised by "
                f"bench/vm/normalize.py with pyarrow {d['pyarrow']}, each checked against the plan's SHA-256 "
                f"before it is uploaded to the container")
    src = os.path.expanduser(d["source_dir"])
    missing = [f for f in files if not os.path.isfile(os.path.join(src, f))]
    if missing:
        raise SystemExit(f"run.py: the plan's data is not all in {src}: no {', '.join(missing)}")
    wrong = [f for f in files if d.get("sha256") and sha256_of(os.path.join(src, f)) != d["sha256"][f]]
    if wrong:
        raise SystemExit(f"run.py: in {src}, not the plan's SHA-256: {', '.join(wrong)}")
    total = sum(os.path.getsize(os.path.join(src, f)) for f in files)
    checked = ", each the plan's SHA-256" if d.get("sha256") else ""
    return f"data: {len(files)} files from {src}{checked}, {total / MiB:.0f} MiB to upload from this machine"


def check_inputs(plan, a, real):
    """What a run is given, checked before anything is created: the plan's data, 0.6.6's wheel
    when one is given, and with the node the image, the gate tasks and the operator's address
    (TCP 443 is admitted from it alone, AZURE.md). A real run needs them all; a dry run checks
    those it is given. Returns the lines the run notes first; a SystemExit says what is wrong."""
    lines, with_node = [], "moruna-node" in (getattr(a, "engines", None) or plan["engines"])
    if real and with_node and not (a.vhd and a.tasks and a.operator):
        raise SystemExit("run.py: a real run needs --vhd, --tasks and --from")
    lines.append(data_facts(plan, getattr(a, "data_from_mac", False)))
    wheel = getattr(a, "wheel066", "")
    if wheel:
        a.wheel066 = wheel = os.path.abspath(os.path.expanduser(wheel))
        name = os.path.basename(wheel)
        if not os.path.isfile(wheel) or not re.fullmatch(r"moruna-[0-9.]+-cp314-cp314t-\w+\.whl", name):
            raise SystemExit(f"run.py: --wheel066 {wheel}: not a CPython 3.14t wheel of moruna")
        lines.append(f"0.6.6's wheel: {name} from this machine, sha256 {sha256_of(wheel)}")
    else:
        lines.append(f"0.6.6's wheel: moruna {getattr(a, 'version066', '0.6.6')} from PyPI if it is there, "
                     f"else built on the Linux machine from {a.ref066}")
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
    mode.add_argument("--table", metavar="RESULTS", nargs="+", default=None,
                      help="the table of what a run's results directory holds so far (summary.json, table.md, "
                           "and printed); no log entry. Safe while the run goes on. Several directories are merged, "
                           "the newest record of each run, and printed, and written to --results when given")
    p.add_argument("--engines", default="",
                   help="a,b,...: only those engines' runs of the plan (and 0.6.6's learning runs, the reference, "
                        "when 0.6.6 is not one of them), into a new results directory")
    p.add_argument("--vhd", default="", help="the node's fixed VHD (package.py --formats vhd)")
    p.add_argument("--tasks", default="", help="the gate's native tasks (images/gate/tasks/build.sh)")
    p.add_argument("--from", dest="operator", default="",
                   help="this machine's public IPv4 address: the node admits TCP 443 from it alone (AZURE.md)")
    # 0.6.6 has no tag yet: its release commit and the CI commits after it, fix/adaptive-runtime's head.
    p.add_argument("--ref066", default="8018c96ab02867c7201450a9375ce3d9bc4da00e",
                   help="0.6.6's commit, built on the Linux machine when neither --wheel066 nor PyPI gives its wheel")
    p.add_argument("--version066", default="0.6.6",
                   help="0.6.6's version on PyPI: its wheel is taken from there when PyPI has it")
    p.add_argument("--wheel066", default="",
                   help="0.6.6's wheel (CPython 3.14t, x86_64) to give the Linux machine, such as the one a run "
                        "that built it kept in its results directory")
    p.add_argument("--data-from-mac", action="store_true",
                   help="upload the plan's data from source_dir, instead of making it in Azure from its public source")
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
        for d in a.table:
            if not os.path.isdir(d):
                p.error(f"--table: no results directory {d}")
        if len(a.table) == 1:
            summary = finish(plan, a.table[0], None, log=None)
            done = sum(1 for _ in collect.load(a.table[0]))
            out.write(f"# {done} of {len(plans.runs(plan, DATA))} runs recorded in {a.table[0]}\n\n")
            out.write(table.render(summary) + "\n")
            return 0
        if a.results and os.path.abspath(a.results) in {os.path.abspath(d) for d in a.table}:
            p.error("--results: a directory of its own, not one the table merges (its table.md is that run's)")
        summary, text = merged(plan, a.table, a.results or None)
        done = sum(s["runs"] for s in summary["sources"])
        where = f"; written to {a.results}" if a.results else ""
        out.write(f"# {done} of {len(plans.runs(plan, DATA))} runs, the newest of each from {len(a.table)} results "
                  f"directories{where}\n\n{text}\n")
        return 0
    a.engines = [e for e in a.engines.split(",") if e] or None
    try:
        plans.select(plan, [], a.engines)
    except plans.PlanError as e:
        p.error(str(e))
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
    # The run's stamp, which orders results directories when --table merges them, and its engines.
    with open(os.path.join(results, "run.json"), "w") as f:
        json.dump({"stamp": stamp, "plan": plan["name"], "engines": a.engines or "all"}, f, indent=1)
    secrets = tempfile.mkdtemp(prefix="moruna-bench-")
    try:
        Bench(plan, Real(cloud, os.path.join(results, "commands.log"), out), cloud, a, results, secrets).go()
    finally:
        shutil.rmtree(secrets, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
