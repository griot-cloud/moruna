"""The proof (work item 3.8): the four NYC taxi jobs through one node's API give 0.6.6's output.

    proof.py --url https://ADDRESS:443 --pin SHA256 (--token TOKEN | --token-key PEM --instance ID)
             --function-key PEM --functions DIR --function-base URL
             --data volume --source-serial S --sink-serial S --volume-image FILE [--unpacker BIN]
           | --data blob --blob-account A --blob-container C [--blob-endpoint URL]
             [--jobs score,occupancy,enrich,audit] [--periods 2024-01,2024]
             [--references ~/moruna-data/nyc] [--gate-canon BIN] [--events sse|mcp]
             [--memory-gib G] [--cpus N] [--results DIR] [--work DIR]

Against one node, in order:

1. **Registers the jobs' functions** (3E): each gate task `DIR/gate-JOB`, served at
   `--function-base/gate-JOB`, by its SHA-256 and an Ed25519 signature over it made with the
   test function key whose public half is in the node's config. One the node already lists with
   the same SHA-256 is left as it is. A private store's tasks are given by a SAS (3E: `https://`
   only, a pre-signed or SAS URL; admission holds no store credentials): with `FUNCTIONS_SAS` in
   the environment, each address is `--function-base/gate-JOB?SAS`, for Blob
   `https://ACCOUNT.blob.core.windows.net/CONTAINER/gate-JOB?SAS`.
2. **Runs Score, Occupancy, Enrich and Audit** (bench/vm/workloads.py) on January 2024, then on
   all of 2024, one job at a time (3A: a node runs one), each as gate_jobs.py's document with
   its function the registered task, `{"kind": "function", "function": "gate-JOB"}` (3.6): the
   normalised files from the source volume (3C, `disk://SERIAL/FILE`) or from Blob (3.5,
   `az://CONTAINER/nyc/FILE`, the account key by secret name), the output to the sink volume or
   to Blob. The sink writes the volume's layer from its first byte, so each run's output is read
   back (`moruna-engine volume unpack` on the volume's image, 3.3) before the next run.
3. **Follows each job's events** to its report and exit: over Server-Sent Events, or over MCP's
   `moruna_jobs_events` (3.7). A lost connection resumes after the last record it had (`from` is
   that record's number: the records after it come).
4. **Checks the output** against 0.6.6's reference, `REFERENCES/reference-JOB-PERIOD.json`
   (references.py): its rows and canonical hashes (gate-canon/1; `gate-canon` when it is given,
   else canon.py, which the year does not fit), within the tolerance of the columns the
   reference names tolerant. canon.py reads Parquet with pyarrow; where there is none, only
   gate-canon hashes, and a month whose tolerant values differ is not compared.

Every run's record is written to `RESULTS/JOB-PERIOD.json`, the summary to `RESULTS/proof.json`,
and a table is printed. The exit code is 0 only when all of them pass.

The Blob account key (`BENCH_KEY`) and the functions' SAS (`FUNCTIONS_SAS`, the query string
Azure gives, with or without its `?`) are read from the environment, never from the command line,
and never printed or written: wherever either would show (an address, a node's diagnostic that
quotes one, an error), the logs and the results name it instead, `<FUNCTIONS_SAS>` or
`<BENCH_KEY>`.
"""
import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(1, os.path.join(HERE, "engines"))
sys.path.insert(2, os.path.join(HERE, "..", "vm"))
import gate_jobs  # noqa: E402
import node_api  # noqa: E402

try:
    import canon  # noqa: E402 (it reads Parquet with pyarrow)
except ImportError:  # no pyarrow here: gate-canon hashes
    canon = None
NO_CANON = "canon.py needs pyarrow, which is not here"

JOBS = ("score", "occupancy", "enrich", "audit")
PERIODS = ("2024-01", "2024")
MiB = 1 << 20
# What a node keeps from a job's budget (3A): 512 MiB for its services, 256 MiB for a check.
NODE_KEEPS = 768 * MiB
BLOB_SECRET = "blob-key"
# The environment's secrets: the Blob account key, and the SAS each function's address carries.
BLOB_KEY_ENV = "BENCH_KEY"
SAS_ENV = "FUNCTIONS_SAS"
# A SAS's signature is looked for alone only when it is this long: a real one is 44 characters of
# base64, and a shorter piece would hide ordinary text.
SIG_LEAST = 8


def sas_of(env=None):
    """The functions' SAS from the environment: Azure's query string without its `?`, or ""."""
    env = os.environ if env is None else env
    return env.get(SAS_ENV, "").strip().lstrip("?")


def address(function_base, name, sas=""):
    """Where the node fetches function `name`: `function_base/name`, with the SAS when given."""
    return f"{function_base}/{name}" + (f"?{sas}" if sas else "")


class Redact:
    """Text with each secret's value replaced by its name in angle brackets. A SAS is also
    found by its signature alone (`sig`, the only part of it that is secret), as given or
    decoded, so a message that quotes it decoded or cut is still redacted."""

    def __init__(self, secrets):
        pairs = {}
        for name, value in secrets.items():
            if not value:
                continue
            pairs[value] = f"<{name}>"
            if name == SAS_ENV:
                for sig in re.findall(r"(?:^|&)sig=([^&]*)", value):
                    for piece in {sig, urllib.parse.unquote(sig)}:
                        if len(piece) >= SIG_LEAST:
                            pairs[piece] = f"<{name}>"
        # The longest first, so that a whole SAS goes before its signature.
        self.pairs = sorted(pairs.items(), key=lambda p: -len(p[0]))

    def __call__(self, text):
        text = str(text)
        for value, name in self.pairs:
            text = text.replace(value, name)
        return text


def files(period):
    """The normalised input files of a period: a month's one, or the year's twelve."""
    months = [period] if "-" in period else [f"{period}-{m:02d}" for m in range(1, 13)]
    return [f"yellow_tripdata_{m}.parquet" for m in months]


def plan(jobs, periods):
    """The runs, every job on a period before the next period: the month first, then the year."""
    return [{"job": j, "period": p, "id": f"{j}-{p}"} for p in periods for j in jobs]


class Volumes:
    """Sources on one volume, outputs on another, by serial (3C); the sink volume's image is
    `image` where this runs, read back by `unpacker volume unpack` (3.3)."""

    def __init__(self, source, sink, image, unpacker="moruna-engine"):
        self.source, self.sink, self.image, self.unpacker = source, sink, image, unpacker

    def document_extras(self):
        return {}, {}

    def inputs(self, period):
        return [f"disk://{self.source}/{f}" for f in files(period)]

    def output(self, run):
        return f"disk://{self.sink}/proof/{run['id']}/"

    def fetch(self, run, dest):
        """The run's output: the sink volume unpacked (the last run's layer), its `proof/RUN`."""
        unpacked = dest.rstrip("/") + ".volume"
        shutil.rmtree(unpacked, ignore_errors=True)
        try:
            p = subprocess.run([self.unpacker, "volume", "unpack", self.image, unpacked],
                               capture_output=True, text=True, check=False)
            if p.returncode != 0:
                raise RuntimeError(f"volume unpack: {p.stderr.strip()[-300:] or p.returncode}")
            src = os.path.join(unpacked, "proof", run["id"])
            if not os.path.isdir(src):
                found = sorted(os.path.relpath(os.path.join(d, f), unpacked)
                               for d, _, fs in os.walk(unpacked) for f in fs)
                raise FileNotFoundError(f"no proof/{run['id']} in the sink volume: {found[:8]}")
            shutil.copytree(src, dest, dirs_exist_ok=True)
        finally:
            shutil.rmtree(unpacked, ignore_errors=True)


class Blob:
    """Sources and outputs in one Blob container (3.5), the account key by secret name."""

    def __init__(self, store, inputs_prefix="nyc/", outputs_prefix="proof/"):
        self.store, self.inputs_prefix, self.outputs_prefix = store, inputs_prefix, outputs_prefix

    def document_extras(self):
        azure = {"account": self.store.account, "container": self.store.container,
                 "access_key": {"secret": BLOB_SECRET}}
        if self.store.endpoint.startswith("http://"):
            azure["allow_http"] = True
        return {"object_store": {"azure": azure}}, {BLOB_SECRET: self.store.key}

    def inputs(self, period):
        return [self.store.az(self.inputs_prefix + f) for f in files(period)]

    def output(self, run):
        return self.store.az(f"{self.outputs_prefix}{run['id']}/")

    def fetch(self, run, dest):
        self.store.download(f"{self.outputs_prefix}{run['id']}/", dest)


def document(run, data, cpus, memory_bytes):
    """The run's job: gate_jobs.py's document over `data`'s inputs and output, its function the
    registered gate task by name (3.6), and the secrets it names."""
    doc = gate_jobs.document(run["job"], data.inputs(run["period"]), data.output(run), {}, cpus=cpus,
                             mem_gib=memory_bytes / (1 << 30))
    doc["kernels"] = [{"kind": "function", "function": f"gate-{run['job']}"}]
    doc["budget"]["memory_bytes"] = int(memory_bytes)
    extras, secrets = data.document_extras()
    doc.update(extras)
    return doc, secrets


def register(node, jobs, functions_dir, function_base, sign, log, sas=""):
    """Each job's gate task as a function (3E), at `address`; one listed already with its
    SHA-256 stays. `log` is main's, which redacts the SAS in the address it shows."""
    listed = {}
    try:
        answer = node.json("GET", "/v1/functions")
        for f in answer.get("functions", []) if isinstance(answer, dict) else answer:
            listed[f.get("name")] = f.get("sha256")
    except node_api.ApiError as e:
        log(f"the node lists no functions ({e}); registering each")
    done = []
    for job in jobs:
        name = f"gate-{job}"
        with open(os.path.join(functions_dir, name), "rb") as f:
            digest = hashlib.sha256(f.read()).hexdigest()
        if listed.get(name) == digest:
            log(f"{name}: registered already ({digest[:12]})")
            continue
        where = address(function_base, name, sas)
        node.register(name, where, digest, sign(digest))
        log(f"{name}: registered ({digest[:12]}), fetched from {where}")
        done.append(name)
    return done


# --- Following a job ------------------------------------------------------------------------

class Followed:
    """What a job's stream said: its records by number, its report and its exit."""

    def __init__(self):
        self.records, self.report, self.exit, self.last, self.resumed = {}, None, None, 0, 0

    def take(self, event, rid, data):
        if event == "event" and rid is not None:
            self.records.setdefault(rid, data)
            self.last = max(self.last, rid)
        elif event == "report":
            self.report = data
        elif event == "exit":
            self.exit = data

    def events(self):
        return [self.records[k] for k in sorted(self.records)]


def sse_events(lines):
    """Server-Sent Events from a stream's lines: (event, id, data as JSON) for each one complete."""
    event, rid, data = "message", None, []
    for line in lines:
        if line == "":
            if data:
                try:
                    yield event, rid, json.loads("\n".join(data))
                except ValueError:
                    pass
            event, rid, data = "message", None, []
        elif line.startswith(":"):
            continue
        else:
            field, _, value = line.partition(":")
            value = value[1:] if value.startswith(" ") else value
            if field == "event":
                event = value
            elif field == "id":
                rid = int(value) if value.isdigit() else None
            elif field == "data":
                data.append(value)


def follow_sse(node, jid, deadline, retries=20, sleep=time.sleep, clock=time.time, log=print):
    """The job's events over SSE to its exit, resuming after the last record on a lost connection."""
    got, failures = Followed(), 0
    while got.exit is None:
        if clock() > deadline:
            return got
        path = f"/v1/jobs/{jid}/events?from={got.last}"
        try:
            for event, rid, data in sse_events(node.t.stream("GET", path, node.tokens.get("observe"))):
                got.take(event, rid, data)
                failures = 0
            if got.exit is None:
                raise ConnectionError("the stream ended before the job's exit")
        except (OSError, ConnectionError) + _http_errors() as e:
            failures += 1
            if failures > retries:
                raise
            got.resumed += 1
            log(f"      the events' connection was lost ({e}); resuming after record {got.last}")
            sleep(min(2 ** failures, 30))
    return got


def follow_mcp(node, jid, deadline, poll_s=2.0, retries=20, sleep=time.sleep, clock=time.time, log=print):
    """The job's events over MCP's `moruna_jobs_events`, a page at a time, from `next` to the exit."""
    got, failures, n = Followed(), 0, 0
    while got.exit is None:
        if clock() > deadline:
            return got
        n += 1
        call = {"jsonrpc": "2.0", "id": n, "method": "tools/call",
                "params": {"name": "moruna_jobs_events", "arguments": {"id": jid, "from": got.last}}}
        try:
            answer = node.json("POST", "/mcp", call, "observe")
        except (OSError, ConnectionError) + _http_errors() as e:
            failures += 1
            if failures > retries:
                raise
            got.resumed += 1
            log(f"      the MCP call was lost ({e}); asking again after record {got.last}")
            sleep(min(2 ** failures, 30))
            continue
        failures = 0
        if "error" in answer:
            raise node_api.ApiError(f"moruna_jobs_events: {answer['error']}")
        result = answer.get("result") or {}
        if result.get("isError"):
            raise node_api.ApiError(f"moruna_jobs_events: {result.get('content')}")
        page = result.get("structuredContent")
        if page is None:
            page = json.loads(result["content"][0]["text"])
        for r in page.get("records", []):
            got.take(r.get("event"), r.get("id"), r.get("data"))
        got.last = max(got.last, page.get("next", got.last))
        if got.exit is None and not page.get("records"):
            sleep(poll_s)
    return got


def _http_errors():
    import http.client
    return (http.client.HTTPException,)


# --- Checking an output ----------------------------------------------------------------------

def canonical(job, out, gate_canon=None, dump=None):
    """The output's canonical form: gate-canon's digest (its dump kept at `dump`), else canon.py's."""
    if gate_canon:
        cmd = [gate_canon, "digest", job] + (["--dump", dump] if dump else []) + [out]
        p = subprocess.run(cmd, capture_output=True, text=True, check=False)
        if p.returncode != 0:
            return {"error": p.stderr.strip()[-300:] or f"gate-canon exited {p.returncode}"}
        d = json.loads(p.stdout)
        d["by"], d["dump"] = "gate-canon", dump
        return d
    if canon is None:
        return {"error": f"{NO_CANON}: give --gate-canon"}
    table = canon.read_output([out])
    if table is None:
        return {"error": "no output files"}
    d = canon.canonical(job, table)
    d["by"] = "canon.py"
    return d


def compare(job, mine, ref, refs_dir, gate_canon=None):
    """Whether `mine` is the reference `ref`: bit for bit, within the job's tolerance, or not."""
    if "error" in mine:
        return {"ok": False, "verdict": f"no canonical form: {mine['error']}"}
    c = {"rows": mine["rows"] == ref["rows"], "hash_exact": mine["hash_exact"] == ref["hash_exact"],
         "hash_all": mine["hash_all"] == ref["hash_all"], "rows_mine": mine["rows"], "rows_ref": ref["rows"],
         "by": mine.get("by")}
    tolerant = ref.get("tolerant")
    if c["hash_all"] and c["rows"]:
        c.update(ok=True, verdict="bit for bit")
    elif not (c["rows"] and c["hash_exact"] and tolerant):
        c.update(ok=False, verdict="DIFFERS: rows" if not c["rows"] else "DIFFERS")
    elif ref.get("dump") and mine.get("dump") and gate_canon:
        # The year: the tolerant rows are in the references' gate-canon dump.
        p = subprocess.run([gate_canon, "compare", job, os.path.join(refs_dir, ref["dump"]), mine["dump"]],
                           capture_output=True, text=True, check=False)
        detail = json.loads(p.stdout) if p.stdout.strip().startswith("{") else {"error": p.stderr.strip()[-300:]}
        c.update(ok=p.returncode == 0, tolerance=detail)
        c["verdict"] = "within tolerance (gate-canon)" if c["ok"] else "DIFFERS beyond the tolerance (gate-canon)"
    elif "tolerant_rows" in ref and "tolerant_rows" in mine and canon is not None:
        t = canon.compare(job, ref, mine)
        c.update(ok=t["ok"], differing=t["differing"], max_abs=t["max_abs"])
        c["verdict"] = (f"within tolerance ({t['differing']} values differ, max {t['max_abs']:.1e})" if t["ok"]
                        else "DIFFERS beyond the tolerance")
    elif "tolerant_rows" in ref and canon is None:
        c.update(ok=False, verdict=f"not compared: the tolerant values differ and {NO_CANON}")
    else:
        c.update(ok=False, verdict="not compared: no tolerant rows to compare (give --gate-canon for the year)")
    return c


# --- One run, and all of them ----------------------------------------------------------------

def submit(node, doc, secrets, deadline, sleep=time.sleep, clock=time.time, log=print):
    """`POST /v1/jobs`, waiting while the node is busy with another job (3A answers `busy`)."""
    body = {"v1": doc}
    if secrets:
        body["secrets"] = secrets
    while True:
        try:
            return node.json("POST", "/v1/jobs", body, "submit")["id"]
        except node_api.ApiError as e:
            if ": 503 " not in str(e) or clock() > deadline:
                raise
            log("      the node is busy with another job; waiting")
            sleep(10)


def run_one(node, run, data, a, follow, log=print, sleep=time.sleep, clock=time.time):
    """One job on the node, followed to its end, its output checked."""
    t0 = clock()
    deadline = t0 + (a.timeout_s if run["period"] != "2024" else a.year_timeout_s)
    doc, secrets = document(run, data, a.cpus, a.memory_bytes)
    rec = {"run": run, "data": a.data, "events_by": a.events}
    try:
        jid = submit(node, doc, secrets, deadline, sleep=sleep, clock=clock, log=log)
    except node_api.ApiError as e:
        return dict(rec, ok=False, state="refused", error=str(e), seconds=round(clock() - t0, 3))
    rec["job_id"] = jid
    log(f"   {run['id']}: job {jid}")
    try:
        got = follow(node, jid, deadline, sleep=sleep, clock=clock, log=log)
    except (node_api.ApiError, OSError) + _http_errors() as e:
        # The events could not be followed (the node refused, or every retry was lost): the
        # job is left to the node, and the run fails saying why.
        return dict(rec, ok=False, state="not followed", error=str(e), seconds=round(clock() - t0, 3))
    if got.exit is None:
        node.call("POST", f"/v1/jobs/{jid}/cancel", None, "submit")
        rec.update(ok=False, state="timed out", seconds=round(clock() - t0, 3), records=len(got.records))
        return rec
    status = node.job(jid)
    rec.update(state=status.get("state"), exit=got.exit, times=node_api.job_times(status),
               seconds=round(clock() - t0, 3), records=len(got.records), resumed=got.resumed,
               first_rows_s=node_api.first_rows_s(got.events()))
    report = (got.report or {}).get("report") if got.report else None
    if report:
        rec["report"] = {k: report.get(k) for k in ("rows_out", "cpu_s", "peak_anon_bytes", "exit") if k in report}
    if status.get("state") != "completed" or got.exit.get("code") != 0:
        rec.update(ok=False, diagnostic=got.exit.get("diagnostic"))
        return rec
    out = os.path.join(a.work, run["id"])
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    try:
        data.fetch(run, out)
    except Exception as e:  # noqa: BLE001 (whatever stops the fetch is the run's result)
        rec.update(ok=False, output_check={"ok": False, "verdict": f"output not fetched: {e}"})
        return rec
    ref_path = os.path.join(a.references, f"reference-{run['job']}-{run['period']}.json")
    if not os.path.exists(ref_path):
        rec.update(ok=False, output_check={"ok": False, "verdict": f"no reference at {ref_path}"})
        return rec
    ref = json.load(open(ref_path))
    dump = os.path.join(a.work, f"{run['id']}.dump") if a.gate_canon and ref.get("dump") else None
    mine = canonical(run["job"], out, a.gate_canon, dump)
    if a.gate_canon and canon is not None and "error" not in mine and "tolerant_rows" in ref and ref.get("tolerant") \
            and mine["hash_exact"] == ref["hash_exact"] and mine["hash_all"] != ref["hash_all"]:
        mine = dict(mine, tolerant_rows=canonical(run["job"], out)["tolerant_rows"])
    rec["canon"] = {k: v for k, v in mine.items() if k != "tolerant_rows"}
    rec["output_check"] = compare(run["job"], mine, ref, a.references, a.gate_canon)
    rec["ok"] = bool(rec["output_check"]["ok"])
    if rec["ok"] and not a.keep_output:
        shutil.rmtree(out, ignore_errors=True)
    return rec


def outcome(rec):
    """What a run came to, in a few words: its check's verdict, else why it has none."""
    c = rec.get("output_check") or {}
    return c.get("verdict") or rec.get("diagnostic") or rec.get("error") or rec.get("state") or ""


def table(records):
    rows = [f"{'run':<20} {'state':<10} {'seconds':>9} {'rows':>11}  output"]
    for r in records:
        rows_mine = str((r.get("output_check") or {}).get("rows_mine", ""))
        rows.append(f"{r['run']['id']:<20} {r.get('state', ''):<10} {r.get('seconds', 0):>9.1f} "
                    f"{rows_mine:>11}  {outcome(r)}")
    return "\n".join(rows)


def tokens_of(a):
    if a.token:
        return {"observe": a.token, "submit": a.token, "admin": a.token}
    if not (a.token_key and a.instance):
        raise SystemExit("proof.py: give --token, or --token-key and --instance")
    return node_api.Tokens(a.token_key, a.instance)


def data_of(a):
    if a.data == "volume":
        if not (a.source_serial and a.sink_serial and a.volume_image):
            raise SystemExit("proof.py: --data volume needs --source-serial, --sink-serial and --volume-image")
        return Volumes(a.source_serial, a.sink_serial, a.volume_image, a.unpacker)
    if not (a.blob_account and a.blob_container and os.environ.get("BENCH_KEY")):
        raise SystemExit("proof.py: --data blob needs --blob-account, --blob-container "
                         "and BENCH_KEY in the environment")
    import store
    return Blob(store.Store(a.blob_account, os.environ["BENCH_KEY"], a.blob_container, a.blob_endpoint or ""),
                a.blob_inputs, a.blob_outputs)


def parse(argv):
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--url", required=True, help="the node's API, https://ADDRESS:443")
    p.add_argument("--pin", required=True, help="the SHA-256 the console's `moruna: tls` line printed")
    p.add_argument("--token", default=None, help="a token with the admin scope (2C)")
    p.add_argument("--token-key", default=None, help="else the token key, to make one for each scope")
    p.add_argument("--instance", default=None, help="and the node's instance ID, the tokens' `aud`")
    p.add_argument("--function-key", required=True, help="the test function key (its public half in node config)")
    p.add_argument("--functions", required=True, help="the directory of the gate tasks, gate-JOB")
    p.add_argument("--function-base", required=True,
                   help="where the node fetches them from, URL/gate-JOB (?FUNCTIONS_SAS from the environment)")
    p.add_argument("--data", choices=("volume", "blob"), required=True)
    p.add_argument("--source-serial", default=None)
    p.add_argument("--sink-serial", default=None)
    p.add_argument("--volume-image", default=None, help="the sink volume's image where this runs, read back after each run")
    p.add_argument("--unpacker", default="moruna-engine", help="the engine that unpacks it (`volume unpack`)")
    p.add_argument("--blob-account", default=None)
    p.add_argument("--blob-container", default=None)
    p.add_argument("--blob-endpoint", default=None, help="a local emulator's (Azurite's) endpoint")
    p.add_argument("--blob-inputs", default="nyc/", help="the prefix of the normalised files in the container")
    p.add_argument("--blob-outputs", default=f"proof/{time.strftime('%Y%m%d-%H%M%S')}/")
    p.add_argument("--jobs", default=",".join(JOBS))
    p.add_argument("--periods", default=",".join(PERIODS))
    p.add_argument("--references", default="~/moruna-data/nyc")
    p.add_argument("--gate-canon", default=None, help="the gate-canon binary (needed for the year)")
    p.add_argument("--events", choices=("sse", "mcp"), default="sse")
    p.add_argument("--memory-gib", type=float, default=None,
                   help="each job's budget; by default the most the node takes (its job_memory_bytes, 3A)")
    p.add_argument("--cpus", type=int, default=4)
    p.add_argument("--timeout-s", type=float, default=3600, help="a month's run, at most")
    p.add_argument("--year-timeout-s", type=float, default=6 * 3600)
    p.add_argument("--results", default="proof-results")
    p.add_argument("--work", default="proof-work")
    p.add_argument("--keep-output", action="store_true")
    a = p.parse_args(argv)
    a.references = os.path.expanduser(a.references)
    for j in a.jobs.split(","):
        if j not in JOBS:
            raise SystemExit(f"proof.py: no job {j!r}")
    for per in a.periods.split(","):
        if per not in PERIODS:
            raise SystemExit(f"proof.py: no period {per!r}")
    if "2024" in a.periods.split(",") and not a.gate_canon:
        raise SystemExit("proof.py: the year's check needs --gate-canon (canon.py does not fit it)")
    return a


def main(argv=None, transport=None, sign=None, log=print, sleep=time.sleep, clock=time.time):
    a = parse(argv)
    sas = sas_of()
    # Every line said and every record written goes through it: no secret is shown or kept.
    redact = Redact({SAS_ENV: sas, BLOB_KEY_ENV: os.environ.get(BLOB_KEY_ENV, "")})
    say = log
    log = lambda line: say(redact(line))  # noqa: E731

    def write(path, obj):
        with open(path, "w") as f:
            f.write(redact(json.dumps(obj, indent=1, default=str)))

    node = node_api.Node(transport or node_api.Https(a.url, a.pin), tokens_of(a))
    data = data_of(a)
    sign = sign or (lambda digest: node_api.sign_digest(a.function_key, digest))
    facts = node.node()
    a.memory_bytes = (int(a.memory_gib * (1 << 30)) if a.memory_gib
                      else facts.get("job_memory_bytes") or facts["memory_bytes"] - NODE_KEEPS)
    os.makedirs(a.results, exist_ok=True)
    os.makedirs(a.work, exist_ok=True)
    log(f"proof: node {facts.get('instance')} on {facts.get('cloud')}, version {facts.get('version')}; "
        f"budget {a.memory_bytes / (1 << 30):.2f} GiB, {a.cpus} CPUs; data from {a.data}; events by {a.events}")
    jobs = a.jobs.split(",")
    try:
        register(node, jobs, a.functions, a.function_base, sign, log, sas)
    except node_api.ApiError as e:
        raise SystemExit(f"proof.py: a function was not registered: {redact(e)}") from None
    follow = follow_sse if a.events == "sse" else follow_mcp
    records = []
    for run in plan(jobs, a.periods.split(",")):
        rec = run_one(node, run, data, a, follow, log=log, sleep=sleep, clock=clock)
        write(os.path.join(a.results, f"{run['id']}.json"), rec)
        log(f"   {run['id']}: {'pass' if rec.get('ok') else 'FAIL'}: {outcome(rec)}")
        records.append(rec)
    summary = {"node": facts, "data": a.data, "events": a.events, "ok": all(r.get("ok") for r in records),
               "runs": [{"id": r["run"]["id"], "ok": r.get("ok"), "state": r.get("state"),
                         "seconds": r.get("seconds"), "verdict": (r.get("output_check") or {}).get("verdict")}
                        for r in records]}
    write(os.path.join(a.results, "proof.json"), summary)
    log(table(records))
    return 0 if summary["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
