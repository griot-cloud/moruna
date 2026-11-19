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
    # The data made in Azure: its source, the writer's pyarrow and every file's SHA-256, together.
    (lambda p: p["dataset"].pop("pyarrow"), "go together"),
    (lambda p: p["dataset"].update(source_url="https://example.org/data.parquet"), "`{file}`"),
    (lambda p: p["dataset"].update(source_url="http://example.org/{file}"), "https"),
    (lambda p: p["dataset"]["sha256"].pop("yellow_tripdata_2024-12.parquet"), "every file"),
    (lambda p: p["dataset"]["sha256"].update({"yellow_tripdata_2024-01.parquet": "AB" * 32}), "lowercase hex"),
    (lambda p: p["dataset"].update(pyarrow=""), "pyarrow"),
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
             azure.blob_download(c, "K", "results", "batch/runs.json", "/s"),
             azure.blob_url(c, "K", "vhds", "node.vhd"), azure.node_disk(c, "u", "i"), azure.account_id(c),
             *azure.nsgs(c, "203.0.113.9"), azure.node_ip(c), azure.node_vm(c, "/cfg"), azure.boot_log(c),
             azure.node_address(c), azure.linux_vm(c, "/k.pub"), azure.image_version(c),
             azure.run_command(c, "true"), azure.run_command_create(c, "rc", "true", {"A": "/a"}),
             azure.run_command_view(c, "rc"), azure.run_command_delete(c, "rc"), azure.group_delete(c),
             azure.group_exists(c)]
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
    # Run 2's start (2026-11-18): a secret through v1's parameters; and v2's plain ones.
    ["az", "vm", "run-command", "invoke", "-n", "bench-linux", "--command-id", "RunShellScript", "--scripts", "true",
     "--parameters", "BENCH_KEY=K", "-g", "moruna-bench-20261118", "--subscription", "griot-dev"],
    ["az", "vm", "run-command", "create", "--run-command-name", "r", "--vm-name", "bench-linux", "--script", "true",
     "--parameters", "SAS=a&b", "-g", "moruna-bench-20261118", "--subscription", "griot-dev"],
])
def test_the_guard_refuses_what_is_not_the_run(cmd):
    with pytest.raises(ValueError, match="refused"):
        azure.guard(cloud(), cmd)


def test_commands_are_well_formed():
    c = cloud()
    page = azure.upload(c, "K", "vhds", "node.vhd", "/v", page=True)
    assert page[page.index("--type") + 1] == "page"
    # The v1 run command takes no parameter: no value can reach it that way (see
    # test_v1_parameters_lose_a_sas_and_put_values_in_the_script).
    assert "--parameters" not in azure.run_command(c, "true")
    with pytest.raises(TypeError):
        azure.run_command(c, "true", ["SAS=a=b&c"])
    # The v2 one, as the PM ran it on a real machine (2026-11-18): by --run-command-name (az takes
    # no -n there), synchronous, every value a protected parameter read by az from its file.
    create = azure.run_command_create(c, "moruna-bench-runs", "echo hi", {"SAS": "/s/param-SAS", "KEY": "/s/k"})
    assert create[:4] == ["az", "vm", "run-command", "create"] and "-n" not in create
    for flag, value in (("--run-command-name", "moruna-bench-runs"), ("--vm-name", "bench-linux"),
                        ("--location", "centralindia"), ("--script", "echo hi"), ("--async-execution", "false"),
                        ("--timeout-in-seconds", "600"), ("-o", "none"), ("-g", c.group)):
        assert create[create.index(flag) + 1] == value, flag
    i = create.index("--protected-parameters")
    assert create[i + 1:i + 3] == ["SAS=@/s/param-SAS", "KEY=@/s/k"] and create[i + 3].startswith("-")
    assert "--parameters" not in create
    view = azure.run_command_view(c, "moruna-bench-runs")
    assert view[:4] == ["az", "vm", "run-command", "show"] and "--instance-view" in view and "-n" not in view
    assert view[view.index("--run-command-name") + 1] == "moruna-bench-runs"
    gone = azure.run_command_delete(c, "moruna-bench-runs")
    assert gone[:4] == ["az", "vm", "run-command", "delete"] and "--yes" in gone and "-n" not in gone
    assert gone[gone.index("--run-command-name") + 1] == "moruna-bench-runs"
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
    # The Linux machine: three batches (its setup, its runs, the node outputs' checks), each started
    # by one managed run command that returns at once, its values as protected parameters az reads
    # from files; then the collection, a v1 run command with no parameter at all.
    starts = [a for a in azs if "run-command create" in a and "batch.py --name" in a]
    assert [a.split("batch.py --name ")[1].split()[0] for a in starts] == ["setup", "runs", "checks"]
    assert all("--protected-parameters 'BASE=@<secrets>/param-BASE' 'SAS_CODE=@<secrets>/param-SAS_CODE' "
               "'SAS_RESULTS=@<secrets>/param-SAS_RESULTS' 'BENCH_KEY=@<secrets>/param-BENCH_KEY' -o none" in a
               and " --parameters " not in a and "<storage-key>" not in a for a in starts)
    # Each read, then deleted from the machine's model.
    for n in ("setup", "runs", "checks"):
        at = [i for i, a in enumerate(azs) if f"--run-command-name moruna-bench-{n} " in a]
        assert [azs[i].split()[3] for i in at] == ["create", "show", "delete"] and at == list(range(at[0], at[0] + 3))
    invokes = [a for a in azs if "run-command invoke" in a]
    assert len(invokes) == 1 and "linux.tgz" in invokes[0] and "--parameters" not in invokes[0]
    assert all(sum(f"-n batch-{n}.json" in a for a in azs) == 1 for n in ("setup", "runs", "checks"))
    # Each Linux run's record lands as the batch's state says it ended, in the plan's order.
    live = [line.rsplit("/live/", 1)[1][:-len(".json")] for line in steps if "/live/" in line]
    assert live == [r["id"] for r in linux]
    assert sum(" POST https://<node-address>/v1/jobs " in line for line in steps) == len(node)
    # Each job followed over SSE to its exit, as proof.py follows one.
    assert sum("/events?from=0 (Server-Sent Events" in line for line in steps) == len(node)
    # The proof's registration: each task at its SAS address.
    assert sum(" POST https://<node-address>/v1/functions " in line for line in steps) == 4
    assert any('"address":"https://mrnb202611180900.blob.core.windows.net/functions/gate-score?<FUNCTIONS_SAS>"'
               in line for line in steps)
    assert sum("blob upload -c functions -n gate-" in a for a in azs) == 4
    assert any("generate-sas -n functions --permissions r " in a for a in azs)
    # The data is made in Azure: none of it goes from this machine. The Linux machine is made
    # before the node's image goes up, so its setup does not wait for that upload.
    assert not any("blob upload -c data" in a for a in azs)
    first = [i for i, a in enumerate(azs) if "vm create -n bench-linux" in a or "-c vhds -n node.vhd" in a]
    assert "bench-linux" in azs[first[0]] and "node.vhd" in azs[first[1]]
    # The node: ready by /v1/health, its pin from its console's blob (get-boot-log only as a
    # fallback), checked against the certificate it serves, its instance the machine's vmId.
    assert any(" GET https://<node-address>/v1/health " in line for line in steps)
    assert any("get-boot-log-uris" in a for a in azs) and not any("get-boot-log -n" in a for a in azs)
    assert any("blob download -c '<boot-diagnostics-container>'" in a for a in azs)
    assert any("s_client -connect <node-address>:443" in line for line in steps)
    assert any("vm show -n moruna-node --query vmId" in a for a in azs)
    notes = "\n".join(line for line in lines if line.startswith("      # "))
    assert "the node's runs wait until the data is in the container" in notes
    assert "data: 12 files made in Azure by the Linux machine from https://d37ci6vzurychx.cloudfront.net" in notes
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
    assert "2 machines of Standard_D4s_v4 at once, 8 vCPUs" in notes and "12 files made in Azure" in notes
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
        run.main(["--plan", str(path), "--dry-run", "--data-from-mac", "--results", str(tmp_path / "r")],
                 out=io.StringIO())
    # Made in Azure, the data needs nothing from this machine, but the plan must say where it is.
    run.main(["--plan", str(path), "--dry-run", "--results", str(tmp_path / "r")], out=io.StringIO())
    for k in plans.MADE_IN_AZURE:
        plan["dataset"].pop(k)
    path.write_text(json.dumps(plan))
    with pytest.raises(SystemExit, match="names no public source"):
        run.main(["--plan", str(path), "--dry-run", "--results", str(tmp_path / "r")], out=io.StringIO())


def mac_data(tmp_path, wrong=()):
    """A plan whose data is three small files in `tmp_path/norm`, each the plan's SHA-256 but `wrong`."""
    plan = nyc_plan()
    src = tmp_path / "norm"
    src.mkdir()
    files = [f"yellow_tripdata_2024-0{m}.parquet" for m in (1, 2, 3)]
    for f in files:
        (src / f).write_bytes(f.encode())
    plan["dataset"].update(source_dir=str(src), files=files,
                           sha256={f: run.sha256_of(str(src / f)) if f not in wrong else "0" * 64 for f in files})
    path = tmp_path / "p.json"
    path.write_text(json.dumps(plan))
    return str(path), files


def test_the_data_from_the_mac_is_the_fallback(tmp_path):
    """--data-from-mac uploads the Mac's copies as before, each checked against the plan's SHA-256,
    and the Linux machine's setup then makes no data."""
    path, files = mac_data(tmp_path)
    out = io.StringIO()
    run.main(["--plan", path, "--dry-run", "--data-from-mac", "--results", str(tmp_path / "r")], out=out)
    lines = out.getvalue().splitlines()
    assert sum("blob upload -c data -n nyc/yellow_tripdata_2024-" in line for line in lines) == len(files)
    assert any(f"data: 3 files from {tmp_path / 'norm'}, each the plan's SHA-256, 0 MiB to upload" in line
               for line in lines)
    assert "the node's runs wait until" in out.getvalue()
    (tmp_path / "w").mkdir()
    wrong, _ = mac_data(tmp_path / "w", wrong=(files[1],))
    with pytest.raises(SystemExit, match=f"not the plan's SHA-256: {files[1]}"):
        run.main(["--plan", wrong, "--dry-run", "--data-from-mac", "--results", str(tmp_path / "r")], out=io.StringIO())
    b = run.Bench(plans.load(path), run.DryRun(io.StringIO()), cloud(), Args(), str(tmp_path), "/s")
    assert [s["id"] for s in b.setup_steps()] == ["data", "setup", "runs-json"]
    b.from_mac = True
    assert [s["id"] for s in b.setup_steps()] == ["setup", "runs-json"]


def test_a_wheel_of_0_6_6_given_to_the_run(tmp_path):
    """--wheel066 goes to the container under its own name for the setup to take (WHEEL066), and
    0.6.6's tree is then neither archived nor uploaded."""
    wheel = tmp_path / "moruna-0.6.6-cp314-cp314t-linux_x86_64.whl"
    wheel.write_bytes(b"wheel")
    lines = dry(tmp_path, "--wheel066", str(wheel))
    text = "\n".join(lines)
    assert f"0.6.6's wheel: {wheel.name} from this machine, sha256 {run.sha256_of(str(wheel))}" in text
    assert f"blob upload -c code -n {wheel.name} -f {wheel}" in text and "moruna-066.tar.gz" not in text
    args = Args()
    args.wheel066 = str(wheel)
    b = run.Bench(nyc_plan(), run.DryRun(io.StringIO()), cloud(), args, str(tmp_path), "/s")
    setup = next(s for s in b.setup_steps() if s["id"] == "setup")
    assert setup["env"] == {"BASE": b.base(), "V066": "0.6.6", "WHEEL066": wheel.name,
                            "ENGINES": "harness,moruna066,moruna066-rust,rust-pool,duckdb,polars,ray,spark,dask"}
    assert setup["secrets"] == ["SAS_CODE"] and setup["argv"] == ["bash", "/opt/bench/bench/node/engines/setup-linux.sh"]
    bad = tmp_path / "moruna-0.6.6-cp314-cp314-linux_x86_64.whl"
    bad.write_bytes(b"gil")
    with pytest.raises(SystemExit, match="not a CPython 3.14t wheel"):
        dry(tmp_path, "--wheel066", str(bad))
    # Without one: PyPI's 0.6.6 when it is there, else a build of --ref066.
    assert any("moruna 0.6.6 from PyPI if it is there, else built on the Linux machine from 8018c96a" in line
               for line in dry(tmp_path))


def test_the_batches_steps(tmp_path):
    """The Linux runs as one batch: measure.py's command for each, as each run-command ran it,
    in the plan's order, its container's account by name and its key a secret of the env file."""
    b = run.Bench(nyc_plan(), run.DryRun(io.StringIO()), cloud(), Args(), str(tmp_path), "/s")
    b.secrets.update({"storage-key": "KEY", "sas-code": "sv=c", "sas-results": "sv=r"})
    linux = [r for r in plans.runs(nyc_plan(), run.DATA) if r["where"] == "linux"]
    steps = b.linux_steps()
    assert [s["id"] for s in steps] == [r["id"] for r in linux]
    assert steps[0]["argv"] == run.MEASURE + [
        "run", "--spec", "/data/runs.json", "--id", "score-moruna066-r0", "--root", "/opt/engines", "--results",
        "/data/results", "--work", "/data/work", "--make-reference", "/data/ref/score.dump"]
    assert steps[1]["argv"][-2:] == ["--reference-dump", "/data/ref/score.dump"]
    assert steps[0]["env"] == {"BENCH_ACCOUNT": "mrnb202611180900", "BENCH_CONTAINER": "data",
                               "JAVA_HOME": "/usr/lib/jvm/java-17-openjdk-amd64"}
    assert all(s["secrets"] == ["BENCH_KEY"] and s["timeout_s"] == 1800 + run.MEASURE_SLACK_S for s in steps)
    data = b.setup_steps()[0]
    spec = json.loads(data["argv"][3])
    assert data["argv"][:3] == ["bash", "/opt/bench/bench/node/engines/data-linux.sh", "24.0.0"]
    assert spec["source_url"] == "https://d37ci6vzurychx.cloudfront.net/trip-data/{file}" and spec["prefix"] == "nyc/"
    assert spec["sha256"] == nyc_plan()["dataset"]["sha256"] and spec["normalize"] == "/opt/bench/bench/vm/normalize.py"
    assert data["secrets"] == ["BENCH_KEY"] and data["env"]["BENCH_CONTAINER"] == "data"
    node = next(r for r in plans.runs(nyc_plan(), run.DATA) if r["where"] == "node" and r["repeat"] == 1)
    check = b.check_step(node)
    assert check["argv"][2:8] == ["check", "--workload", "score", "--out", "/data/work/node-score-moruna-node-r1", "--id"]
    assert "--blob-prefix" in check["argv"] and check["argv"][-1] == "/data/ref/score.dump"
    # No secret is in a step: the batch's env file holds them, from the run-command's parameters.
    text = json.dumps([steps, b.setup_steps(), check])
    assert "KEY" not in text.replace("BENCH_KEY", "") and "sv=" not in text
    assert b.batch_params() == {"BASE": "https://mrnb202611180900.blob.core.windows.net", "SAS_CODE": "sv=c",
                                "SAS_RESULTS": "sv=r", "BENCH_KEY": "KEY"}


# Values shaped like the run's own: an account key (88 characters of base64, with `/` and `+`),
# the containers' SAS (as `az storage container generate-sas` writes it: `&`, `=`, `%`), and a `?`.
REAL_KEY = "hQF1JvrcxsFx/+6u64TPkFDueXXd4e4FML+ytnpPCV1faYjHC6X5sSDYc3mVddlv+on3UZbbE/AStiFllBA9xQ=="
REAL_SAS = "se=2026-11-20T15%3A43Z&sp=rl&spr=https&sv=2024-11-04&sr=c&sig=Qm9%2Fv%2BZ8xT1kq%2F%3D"
REAL_SAS_RESULTS = "se=2026-11-20T15%3A43Z&sp=racwl&spr=https&sv=2024-11-04&sr=c&sig=a%2Bb%2Fc%3D&x=y?z"
SH = [s for s in ("/bin/dash",) if os.path.exists(s)]


def v1_script_sh(script, params):
    """script.sh as the v1 run command writes it (measured on Ubuntu 24.04, 2026-11-18): the
    named parameters, unquoted, as `export N=V ...` on line 1 and `set -- V ...` on line 2, then
    the script; the v1 handler (run-command-extension-linux main/cmds.go writeTempScript) writes
    that text verbatim, and runs it with `/bin/sh -c <script.sh>`."""
    return ("export " + " ".join(f"{k}={v}" for k, v in params.items()) + "\n"
            + "set -- " + " ".join(params.values()) + "\n" + script)


def v2_az(argv, files):
    """What az does with the harness's `az vm run-command create` (azure-cli 2.92): each argument
    whose text after its first `=` is `@FILE` becomes that file's text, its line end stripped
    (core/commands `_expand_file_prefixed_files`); then each protected parameter is split at its
    first `=` (vm/custom.py `vm_run_command_create`). The script and the parameters it sends."""
    args = []
    for a in argv:
        k, eq, v = a.partition("=")
        args.append(f"{k}={files[v[1:]].rstrip(os.linesep)}" if eq and v.startswith("@") else a)
    i = args.index("--protected-parameters") + 1
    end = next(j for j in range(i, len(args)) if args[j].startswith("-"))
    return args[args.index("--script") + 1], dict(p.split("=", 1) for p in args[i:end])


def v2_handler(tmp_path, script, params, path, shell="/bin/bash"):
    """What the v2 handler does with them (run-command-handler-linux internal/cmds `runCmd`,
    internal/files `SaveScriptFile`, internal/exec `Exec` and `SetEnvironmentVariables`): the
    script saved verbatim as script.sh (mode 0500), each named parameter with a value set in the
    environment it inherits (os.Setenv), run as `<shell> -c <script.sh>` in its directory."""
    d = tmp_path / "download" / "moruna-bench-runs" / "0"
    d.mkdir(parents=True, exist_ok=True)
    sh = d / "script.sh"
    if sh.exists():
        sh.chmod(0o700)
    sh.write_text(script)
    sh.chmod(0o500)
    env = {"PATH": path, "HOME": "/root", **{k: v for k, v in params.items() if v}}
    # bash as the handler runs it; dash reads the same file itself (see v1's test for why).
    argv = [shell, "-c", str(sh)] if shell == "/bin/bash" else [shell, str(sh)]
    return subprocess.run(argv, capture_output=True, text=True, env=env, cwd=d), sh


@pytest.mark.skipif(not os.path.exists("/bin/dash"), reason="needs dash, Ubuntu's /bin/sh")
def test_v1_parameters_lose_a_sas_and_put_values_in_the_script(tmp_path):
    """The failure of run 2 (2026-11-18), as the PM measured it on Ubuntu 24.04: v1's named
    parameters are written unquoted, so `&` cuts the line, BASE and the SAS are never set, the key
    becomes a command, and every value sits in the script. The harness gives v1 none."""
    params = {"BASE": "https://x.blob.core.windows.net", "SAS": "sv=2024-11-04&ss=b&sig=ab%2Fc%2B%3D&se=2026?x",
              "KEY": "hQF1/+ab+cd/ef=="}
    text = v1_script_sh('printf "BASE=%s SAS=%s KEY=%s ARGS=%s\\n" "${BASE-unset}" "${SAS-unset}" "${KEY-unset}" "$#"',
                        params)
    assert text.splitlines()[:2] == [
        "export BASE=https://x.blob.core.windows.net SAS=sv=2024-11-04&ss=b&sig=ab%2Fc%2B%3D&se=2026?x KEY=hQF1/+ab+cd/ef==",
        "set -- https://x.blob.core.windows.net sv=2024-11-04&ss=b&sig=ab%2Fc%2B%3D&se=2026?x hQF1/+ab+cd/ef=="]
    sh = tmp_path / "script.sh"
    sh.write_text(text)
    sh.chmod(0o500)
    # Ubuntu's `/bin/sh -c script.sh` is dash running that file (no #! line); dash is asked to read
    # it here directly, since macOS would hand a file with no #! line to its own /bin/sh, bash.
    p = subprocess.run(["/bin/dash", str(sh)], capture_output=True, text=True, cwd=tmp_path,
                       env={"PATH": "/usr/bin:/bin"})
    assert p.stdout.strip() == "BASE=unset SAS=unset KEY=hQF1/+ab+cd/ef== ARGS=0"
    assert f"{sh}: 2: hQF1/+ab+cd/ef==: not found" in p.stderr
    # The harness's v1 run commands (the collection, a stalled batch's status) take no parameter.
    c = cloud()
    assert "--parameters" not in azure.run_command(c, "true")
    with pytest.raises(TypeError):
        azure.run_command(c, "true", ["BASE=x"])


@pytest.mark.parametrize("shell", ["/bin/bash"] + SH)
def test_the_batch_start_gets_every_value_whole_through_v2(tmp_path, monkeypatch, shell):
    """The batch's start, formed as az and the v2 handler form it (from their source), with a key
    and SAS shaped like the real ones: no value is in the command line or the script; each reaches
    the env file and every URL as given; the unit starts once. Under bash (the v2 handler's
    shell) and sh (dash, as v1 ran it): the script is plain POSIX sh."""
    for name, value in (("DATA", tmp_path / "data"), ("BATCH", tmp_path / "data" / "batch"),
                        ("CODE", tmp_path / "opt" / "bench")):
        monkeypatch.setattr(run, name, str(value))
    shims = tmp_path / "bin"
    shims.mkdir()
    (shims / "curl").write_text(f'#!/bin/sh\nprintf "%s\\n" "$@" >> {tmp_path}/curl.args\n'
                                'while [ $# -gt 0 ]; do [ "$1" = -o ] && echo "[]" > "$2"; shift; done\n')
    (shims / "tar").write_text("#!/bin/sh\ncat > /dev/null\n")
    (shims / "systemd-run").write_text(f'#!/bin/sh\nprintf "%s\\n" "$@" >> {tmp_path}/units\n')
    for f in shims.iterdir():
        f.chmod(0o755)
    path = f"{shims}:/usr/bin:/bin"
    # The harness's own command: its values in files of the secrets directory.
    x = run.DryRun(io.StringIO())
    b = run.Bench(nyc_plan(), x, cloud(), Args(), str(tmp_path), str(tmp_path / "s"))
    b.secrets.update({"storage-key": REAL_KEY, "sas-code": REAL_SAS, "sas-results": REAL_SAS_RESULTS})
    values = b.batch_params()
    files = {f"{tmp_path}/s/param-{k}": v + "\n" for k, v in values.items()}  # a line end: az strips it
    argv = azure.run_command_create(cloud(), "moruna-bench-runs", run.batch_start("runs"),
                                    {k: f"{tmp_path}/s/param-{k}" for k in values})
    command_line = " ".join(argv)
    assert not any(v in command_line for v in (REAL_KEY, REAL_SAS, REAL_SAS_RESULTS, "sig="))
    script, params = v2_az(argv, files)
    assert params == values and script == run.batch_start("runs")
    assert not any(v in script for v in (REAL_KEY, REAL_SAS, REAL_SAS_RESULTS))
    first, sh = v2_handler(tmp_path, script, params, path, shell)
    assert first.returncode == 0 and first.stdout.strip() == "STARTED runs", first.stderr
    assert not any(v in sh.read_text() for v in (REAL_KEY, REAL_SAS, REAL_SAS_RESULTS))
    # The harness is fetched once, and the steps, each by the SAS whole.
    fetched = (tmp_path / "curl.args").read_text().splitlines()
    assert f"{values['BASE']}/code/harness.tar.gz?{REAL_SAS}" in fetched
    assert f"{values['BASE']}/code/batch-runs.json?{REAL_SAS}" in fetched
    envfile = tmp_path / "data" / "batch" / "runs.env"
    assert oct(envfile.stat().st_mode & 0o777) == "0o600"
    import batch
    got = batch.load_env(str(envfile))
    assert {k: got[k] for k in values} == values and got["PATH"] == path and got["HOME"] == "/root"
    unit = (tmp_path / "units").read_text()
    assert unit.startswith("--unit=moruna-bench-runs\n") and "--name\nruns\n--steps\n" in unit
    assert not any(v in unit for v in (REAL_KEY, REAL_SAS, REAL_SAS_RESULTS))
    again, _ = v2_handler(tmp_path, script, params, path, shell)
    assert again.stdout.strip() == "ALREADY runs" and (tmp_path / "units").read_text() == unit


def test_the_collection_reads_its_values_on_the_machine(tmp_path, monkeypatch):
    """The collection's v1 run command carries no value: it reads the address and the results'
    SAS from the setup batch's env file, each whole, under sh."""
    for name, value in (("DATA", tmp_path / "data"), ("BATCH", tmp_path / "data" / "batch")):
        monkeypatch.setattr(run, name, str(value))
    (tmp_path / "data" / "batch").mkdir(parents=True)
    (tmp_path / "data" / "results").mkdir()
    (tmp_path / "data" / "batch" / "setup.env").write_text(
        f"PATH=/usr/bin\nBASE=https://a.blob.core.windows.net\nSAS_CODE={REAL_SAS}\n"
        f"SAS_RESULTS={REAL_SAS_RESULTS}\nBENCH_KEY={REAL_KEY}\n")
    shims = tmp_path / "bin"
    shims.mkdir()
    (shims / "curl").write_text(f'#!/bin/sh\nprintf "%s\\n" "$@" > {tmp_path}/curl.args\n')
    (shims / "systemctl").write_text("#!/bin/sh\nexit 0\n")
    for f in shims.iterdir():
        f.chmod(0o755)
    class Stop(Fake):
        def az(self, cmd, out=None, secrets=None):
            if "run-command" in cmd:
                self.cmd = cmd
                raise RuntimeError("enough: the command is what is wanted")
            return super().az(cmd, out, secrets)
    x = Stop(str(tmp_path / "r"))
    b = run.Bench(nyc_plan(), x, cloud(), Args(), str(tmp_path / "r"), str(tmp_path / "s"))
    b.secrets.update({"storage-key": REAL_KEY, "sas-code": REAL_SAS, "sas-results": REAL_SAS_RESULTS})
    with pytest.raises(RuntimeError, match="enough"):
        b.collect(log=False)
    assert x.cmd[:3] == ["az", "vm", "run-command"] and x.cmd[3] == "invoke" and "--parameters" not in x.cmd
    script = x.cmd[x.cmd.index("--scripts") + 1]
    assert not any(v in " ".join(x.cmd) for v in (REAL_KEY, REAL_SAS, REAL_SAS_RESULTS))
    for shell in ["/bin/bash"] + SH:
        p = subprocess.run([shell, "-c", script], capture_output=True, text=True,
                           env={"PATH": f"{shims}:/usr/bin:/bin"})
        assert p.returncode == 0, p.stderr
        assert f"https://a.blob.core.windows.net/results/linux.tgz?{REAL_SAS_RESULTS}" in \
            (tmp_path / "curl.args").read_text().splitlines()


def test_a_script_az_would_read_as_a_file_is_refused():
    for script in ("@/etc/passwd", "X=@/etc/passwd; true"):
        with pytest.raises(ValueError, match="as a file's name"):
            azure.run_command(cloud(), script)
    azure.no_file_prefix("a=b @c")


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
WHEEL_SHA = "6f" * 32
CONSOLE_URI = ("https://mrnb202611180900.blob.core.windows.net/bootdiagnostics-morunanod-0b1c/"
               "moruna-node.0b1c.serialconsole.log")


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
    """A run's executor whose cloud is a dictionary: what a real run would learn, made up. The
    Linux machine runs a batch's steps when it is started, as engines/batch.py would, and its
    state is read back from the container `reveal` more steps at a time (all at once by default).
    `fail_on` fails the `az` command, or stops the batch at the step, whose text holds it."""

    answers = True

    def __init__(self, results, fail_on=None, node=None, console_after=0, reveal=None, data_ok=True,
                 wheel="built", health_after=0, served=PIN, vm_id="i-1", console_text=None, not_installed=()):
        super().__init__(io.StringIO())
        self.results, self.fail_on, self.azs, self.node = results, fail_on, [], node or FakeNode()
        self.not_installed = not_installed  # the engines the setup does not install
        self.console_after, self.reveal, self.data_ok, self.wheel = console_after, reveal, data_ok, wheel
        self.files, self.uploaded, self.states, self.shown, self.started = {}, {}, {}, {}, []
        self.health_after, self.served, self.vm_id, self.healths = health_after, served, vm_id, 0
        self.views = {}  # each managed run command's instance view, by batch
        # Azure's serial log, as the node writes it (1B), with its line ends.
        self.console_text = console_text or (f"moruna: cloud azure instance i-1\r\nmoruna: tls sha256={PIN}\r\n"
                                             "moruna: ready api=10.0.0.4:443 boot_ms=2140\r\n")

    def write(self, path, text):
        self.files[path] = text

    def read_state(self, path, steps):
        return self.files.pop(path, None)

    def read_text(self, path):
        return self.files.pop(path, None)

    def health(self, address):
        assert address == "203.0.113.9"
        self.healths += 1
        return self.healths > self.health_after

    def served_pin(self, address):
        return self.served

    def machine(self, name, steps):
        """The state batch.py would leave after `steps`: each one's end, or stopped at `fail_on`."""
        ended = []
        for st in steps:
            argv = " ".join(st["argv"]) + " "
            if self.fail_on and self.fail_on in argv:
                return {"name": name, "state": "failed", "steps": ended, "error": f"failed: {self.fail_on}"}
            failed = st["id"] == "setup" and self.not_installed
            ended.append({"id": st["id"], "exit": 1 if failed else 0, "stdout": self.step_out(st, argv),
                          "stderr": "Downloading cpython-3.14.3-linux-x86_64-gnu (download)\n" if failed else ""})
        return {"name": name, "state": "done", "steps": ended}

    def step_out(self, st, text):
        if st["id"] == "data":
            spec = json.loads(st["argv"][3])
            files = {f: {"sha256": h if self.data_ok else "0" * 64, "ok": self.data_ok} for f, h in spec["sha256"].items()}
            rec = {"ok": self.data_ok, "files": files, "fetch_s": 9.0, "normalize_s": 20.0, "upload_s": 5.0}
            if not self.data_ok:
                rec["error"] = f"12 of 12 files differ from the plan's SHA-256: {', '.join(files)}"
            return "normalized rows 41169720\nDATA " + json.dumps(rec)
        if st["id"] == "setup":
            # setup-linux.sh's line: each engine run.py named (ENGINES), and why one did not install.
            names = st["env"]["ENGINES"].split(",")
            why = "uv pip install... exited 1: no wheel for this interpreter"
            return "SETUP " + json.dumps({"installed": {e: e not in self.not_installed for e in names},
                                          "errors": {e: "installs and imports on none of CPython 3.14+gil, 3.13+gil"
                                                     for e in self.not_installed},
                                          "tried": {e: [{"python": "3.14+gil", "error": why},
                                                        {"python": "3.13+gil", "error": why}]
                                                    for e in self.not_installed},
                                          "wheel_066": self.wheel, "wheel_066_s": 1800,
                                          "wheel_066_name": "moruna-0.6.6-cp314-cp314t-linux_x86_64.whl",
                                          "wheel_066_sha256": WHEEL_SHA})
        if "measure.py run" in text:
            assert st["env"]["BENCH_ACCOUNT"] == "mrnb202611180900" and st["secrets"] == ["BENCH_KEY"]
            rid = text.split("--id ")[1].split()[0]
            spec = next(r for r in plans.runs(nyc_plan(), run.DATA) if r["id"] == rid)
            fast = {"moruna066": 9.0, "moruna066-rust": 6.0, "rust-pool": 4.5}.get(spec["engine"], 12.0)
            copy = {"data": "local copy", "copy_in_s": 2.0, "copy_out_s": 1.0} if spec["engine"] == "rust-pool" else \
                {"data": "blob"}
            # Each engine's interpreter, as measure.py records it: 0.6.6 free-threaded, the Rust pool none.
            python = {} if spec["engine"] == "rust-pool" else {
                "python": "3.14.3", "gil": spec["engine"] not in ("moruna066", "moruna066-rust")}
            rec = {"spec": spec, "ok": True, "startup_s": 0.5, "operational_s": fast, "cpu_s": 30.0,
                   "peak_bytes": 2 << 30, "memory_gib": spec["mem_gib"], **copy, **python,
                   "output_check": {"ok": True, "hash_all": True, "verdict": "bit for bit"}}
            os.makedirs(os.path.join(self.results, "results"), exist_ok=True)
            json.dump(rec, open(os.path.join(self.results, "results", f"{rid}.json"), "w"))
            return "MEASURED " + json.dumps({"id": rid, "ok": True, "output": "bit for bit", **python})
        if "measure.py check" in text:
            return 'MEASURED {"ok": true, "output": "bit for bit"}'
        return ""

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
        if "get-boot-log-uris" in text:
            return CONSOLE_URI
        if "get-boot-log" in text:
            # The CLI's own bug, on every call (2026-11-18): the console is read from its blob.
            raise RuntimeError("az: TypeError: unsupported operand type(s) for +=: 'int' and 'NoneType'")
        if "blob download -c bootdiagnostics-" in text:
            # Made with --no-wait: the first looks may find no console yet.
            if sum("-c bootdiagnostics-" in a for a in self.azs) <= self.console_after:
                raise RuntimeError("The specified blob does not exist. ErrorCode:BlobNotFound")
            self.files[cmd[cmd.index("-f") + 1]] = self.console_text
            return ""
        if "--query vmId" in text:
            return self.vm_id
        if "public-ip show" in text:
            return "203.0.113.9"
        if "group exists" in text:
            return "false"
        if "blob upload -c code -n batch-" in text:
            self.uploaded[text.split("-n batch-")[1].split(".json")[0]] = json.loads(self.files[cmd[cmd.index("-f") + 1]])
            return ""
        if "run-command create" in text and "batch.py --name" in text:
            name = text.split("batch.py --name ")[1].split()[0]
            # The values reach az as files it reads itself, never as text of the command.
            assert "KEY" not in text.replace("BENCH_KEY", "") and "sig=" not in text
            given = dict(p.split("=@", 1) for p in cmd[cmd.index("--protected-parameters") + 1:cmd.index("-o")])
            values = {k: self.files[path] for k, path in given.items()}
            assert values == {"BASE": "https://mrnb202611180900.blob.core.windows.net", "SAS_CODE": "sv=1&sig=x",
                              "SAS_RESULTS": "sv=1&sig=x", "BENCH_KEY": "KEY"}
            if name in self.states:
                self.views[name] = {"executionState": "Succeeded", "exitCode": 0, "output": f"ALREADY {name}\n"}
                return ""
            self.started.append(name)
            self.states[name] = self.machine(name, self.uploaded[name])
            self.views[name] = {"executionState": "Succeeded", "exitCode": 0, "output": f"STARTED {name}\n",
                                "error": ""}
            return ""
        if "run-command show" in text and "--instance-view" in text:
            return json.dumps(self.views[text.split("--run-command-name moruna-bench-")[1].split()[0]])
        if "blob download -c results -n batch/" in text:
            name = text.split("-n batch/")[1].split(".json")[0]
            if name not in self.states:
                raise RuntimeError("ErrorCode:BlobNotFound")
            state = self.states[name]
            shown = self.shown[name] = self.shown.get(name, 0) + (self.reveal or len(state["steps"]) or 1)
            self.files[cmd[cmd.index("-f") + 1]] = dict(state, steps=state["steps"][:shown],
                                                        state=state["state"] if shown >= len(state["steps"]) else "running")
            return ""
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
        for _ in range(500):
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
    # The console's blob was read again while it was not there yet (get-boot-log, tried each time it
    # was not, failed on the CLI's bug); its address asked once. After it, once more after each node
    # job for the network meters (6.3ab), which are kept beside the job's record.
    jobs = sorted(n for n in os.listdir(tmp_path / "node") if n.endswith(".job.json"))
    assert sum("blob download -c bootdiagnostics-morunanod-0b1c -n moruna-node.0b1c.serialconsole.log" in a
               for a in x.azs) == 3 + len(jobs)
    assert all((tmp_path / "node" / n.replace(".job.json", ".meters.json")).exists() for n in jobs)
    assert sum("get-boot-log-uris" in a for a in x.azs) == 1
    assert sum("get-boot-log -n" in a for a in x.azs) == 2
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
    assert ("| Moruna 0.7, the node | rust | none | 3.0 | 0.75 | 20.0 | 3.00 | 13.00 | blob | 0.7 | 0.3 | 4.0 | 0.2 | "
            "0.3 | 0.0 | bit for bit | 5/5 |") in table_text
    assert ("| the plain Rust pool | rust | none | 4.5 | 0.50 | 30.0 | 2.00 | 14.00 | local copy, 3.0 s apart |"
            in table_text)
    # The Python column: each engine's interpreter and its GIL, as its runs recorded them.
    assert "| 0.6.6, Python function | python | 3.14.3, free-threaded | 9.0 |" in table_text
    assert "| 0.6.6, Rust function | rust | 3.14.3, free-threaded | 6.0 |" in table_text
    assert "| Ray | python | 3.14.3, GIL | 12.0 |" in table_text
    log = open(tmp_path / "benchmark-log.md").read()
    assert log.startswith("# The benchmark log") and "## 202611180900: plan nyc-2024" in log
    # 3.0 s against the Rust pool's 4.5 s is 1.5x: the bar's time holds; its CPU (20 s) is the least.
    assert "**PASS** no regression" in log and "**PASS** the bar: 1.50x" in log
    assert "#### Miss" not in log
    assert json.load(open(tmp_path / "setup.json"))["wheel_066"] == "built"
    # Which 0.6.6 ran: the wheel's source and SHA-256, in summary.json, table.md and the log's table.
    wheel = {"source": "built", "name": "moruna-0.6.6-cp314-cp314t-linux_x86_64.whl", "sha256": WHEEL_SHA,
             "seconds": 1800}
    assert json.load(open(tmp_path / "summary.json"))["wheel_066"] == wheel == x.summary["wheel_066"]
    line = ("0.6.6's wheel: built on the Linux machine, in 1800 s outside every timing: "
            f"`moruna-0.6.6-cp314-cp314t-linux_x86_64.whl`, sha256 {WHEEL_SHA}.")
    assert table_text.startswith(line + "\n") and log.count(line) == 1
    # Three batches on the Linux machine, each started once; the node's outputs checked in the last,
    # after every Linux run, and its records then carry the verdict.
    assert x.started == ["setup", "runs", "checks"]
    assert [s["id"] for s in x.uploaded["checks"]] == [r["id"] for r in plans.runs(p) if r["where"] == "node"]
    assert json.load(open(tmp_path / "data.json"))["ok"] and json.load(open(tmp_path / "batch" / "runs.json"))["state"] == "done"
    # 0.6.6's wheel, built on the machine, kept beside the results for the next run's --wheel066.
    assert any(f"blob download -c code -n moruna-0.6.6-cp314-cp314t-linux_x86_64.whl -f {tmp_path}/moruna-0.6.6-" in a
               for a in x.azs)
    assert "--wheel066" in x.out.getvalue()
    assert "uploaded from this machine" not in log and "made in Azure from https://" in log


def test_a_wheel_from_pypi_or_given_is_not_kept_again(tmp_path):
    _, tasks = fake_inputs(tmp_path)
    args = Args()
    args.tasks = tasks
    x = Fake(str(tmp_path), wheel="pypi")
    run.Bench(nyc_plan(), x, azure.Cloud("202611180900"), args, str(tmp_path), "/s").go()
    assert not any("blob download -c code" in a for a in x.azs)
    assert open(tmp_path / "table.md").read().startswith("0.6.6's wheel: PyPI's release, in 1800 s")


def test_the_table_names_the_wheel_066_it_ran(tmp_path):
    """The wheel's source and SHA-256 from setup.json, in summary.json and at the table's head
    (also under --table while the plan runs); no line before the setup has ended."""
    assert run.wheel_066(str(tmp_path)) is None
    (tmp_path / "setup.json").write_text("not json")
    assert run.wheel_066(str(tmp_path)) is None
    (tmp_path / "setup.json").write_text(json.dumps({"installed": {}}))
    assert run.wheel_066(str(tmp_path)) is None
    (tmp_path / "setup.json").write_text(json.dumps({"wheel_066": "container", "wheel_066_name": "w.whl",
                                                     "wheel_066_sha256": WHEEL_SHA, "wheel_066_s": 2}))
    assert run.wheel_066(str(tmp_path)) == {"source": "container", "name": "w.whl", "sha256": WHEEL_SHA, "seconds": 2}
    out = io.StringIO()
    assert run.main(["--plan", "nyc-2024", "--table", str(tmp_path)], out=out) == 0
    assert (f"0.6.6's wheel: given with --wheel066, in 2 s outside every timing: `w.whl`, sha256 {WHEEL_SHA}."
            in out.getvalue())
    assert json.load(open(tmp_path / "summary.json"))["wheel_066"]["sha256"] == WHEEL_SHA
    assert table.wheel_line({"source": "odd"}) == "0.6.6's wheel: odd: `name not recorded`, sha256 not recorded."
    assert "0.6.6's wheel" not in table.render(summary_of([rec("duckdb", 1, 3.5)]))


def test_each_run_lands_as_it_finishes_and_the_table_reads_them_so_far(tmp_path):
    """While the plan runs: the Linux machine's live records are on this machine run by run, as
    its batch's state says each ended, and `--table` makes the table of those so far; the full
    records replace them. A batch that stops stops the run, and the other side does not start."""
    _, tasks = fake_inputs(tmp_path)
    args = Args()
    args.tasks = tasks
    results = tmp_path / "results"
    x = Fake(str(results), fail_on="--id occupancy-moruna066-r0 ", reveal=7)
    with pytest.raises(RuntimeError, match="the batch runs stopped on the Linux machine: failed: --id occupancy"):
        run.Bench(nyc_plan(), x, azure.Cloud("202611180900"), args, str(results), "/s").go()
    live = sorted(os.listdir(results / "live"))
    assert "score-duckdb-r1.json" in live and "score-noise-p2b.json" in live and len(live) == 9 * 6 - 6 + 4
    # Read 7 runs at a time: the state was read again while the batch was still running.
    assert sum("-n batch/runs.json" in a for a in x.azs) == -(-(9 * 6 - 6 + 4) // 7)
    rec = json.load(open(results / "live" / "score-duckdb-r1.json"))
    assert rec["live"] and rec["spec"]["id"] == "score-duckdb-r1" and rec["ok"] and rec["memory_gib"] == 14
    # The full records (here written by the fake machine) replace the live ones: one record a run.
    assert len([r for r in collect.load(str(results)) if r["spec"]["job"] == "score"]) == 9 * 6 - 6 + 4
    assert not (results / "node").exists() and x.started == ["setup", "runs"]
    out = io.StringIO()
    assert run.main(["--plan", "nyc-2024", "--table", str(results)], out=out) == 0
    text = out.getvalue()
    assert text.startswith(f"# {9 * 6 - 6 + 4} of 250 runs recorded in {results}")
    assert "| DuckDB | python | 3.14.3, GIL | 12.0 |" in text and "### occupancy" in text
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
    # What the batch saw of a run that printed none: its stderr's tail, or that it was killed.
    told = run.live_record(spec, None, "killed at the batch's bound, 2700 s")
    assert told["error"] == "no summary line in the run's output: killed at the batch's bound, 2700 s"
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
    # Only the outputs of the jobs that completed are checked.
    assert len(x.uploaded["checks"]) == 24 - 2 and "output_check" not in r0


def test_the_data_made_in_azure_stops_the_run_when_it_differs(tmp_path):
    """Files that do not normalise to the plan's SHA-256s are never uploaded (dataprep.py), and the
    run stops before any measured run, saying how to fall back; the node never starts a job."""
    _, tasks = fake_inputs(tmp_path)
    args = Args()
    args.tasks = tasks
    x = Fake(str(tmp_path), data_ok=False)
    with pytest.raises(RuntimeError, match="files differ from the plan's SHA-256.*--data-from-mac"):
        run.Bench(nyc_plan(), x, azure.Cloud("202611180900"), args, str(tmp_path), "/s").go()
    assert x.started == ["setup"] and x.node.jobs == 0 and not (tmp_path / "live").exists()
    assert not json.load(open(tmp_path / "data.json"))["ok"]
    assert x.azs[-2].startswith("az group delete") and x.azs[-1].startswith("az group exists")


@pytest.mark.parametrize("ended,message", [
    ({"id": "data", "exit": 1, "stdout": "", "stderr": "E: Unable to locate package"}, "no DATA line, exit 1: E: Unable"),
    ({"id": "runs-json", "exit": 22, "stdout": "", "stderr": "curl: (22) 403"}, "runs-json failed: exit 22"),
])
def test_a_failed_setup_step_stops_the_run(tmp_path, ended, message):
    b = run.Bench(nyc_plan(), Fake(str(tmp_path)), cloud(), Args(), str(tmp_path), "/s")
    with pytest.raises(RuntimeError, match=message):
        b.setup_done({"id": ended["id"]}, ended)
    assert not b.data_ready.is_set()
    # An engines' setup that fails stops the run before any measured run (work item 6.2c), its
    # record kept; before, the runs failed one by one.
    with pytest.raises(RuntimeError, match="the setup did not install harness, moruna066.*: harness: no record"):
        b.setup_done({"id": "setup"}, {"id": "setup", "exit": 1, "stdout": "", "stderr": "boom"})
    assert b.setup == {"linux_image": "<linux-image-version>", "exit": 1, "stderr": "boom"}
    with pytest.raises(RuntimeError, match="the setup did not install every engine.*exit 1: boom"):
        b.setup_done({"id": "setup"}, {"id": "setup", "exit": 1, "stderr": "boom", "stdout": "SETUP " + json.dumps(
            {"installed": {e: True for e in b.installs()}})})


def started_bench(tmp_path, x, name="runs"):
    b = run.Bench(nyc_plan(), x, cloud(), Args(), str(tmp_path), str(tmp_path))
    b.secrets.update({"storage-key": "KEY", "sas-code": "sv=1&sig=x", "sas-results": "sv=1&sig=x"})
    return b


def test_a_batch_that_does_not_start_or_goes_silent_stops_the_run(tmp_path, monkeypatch):
    class NoStart(Fake):
        def __init__(self, results, view):
            super().__init__(results)
            self.view = view

        def az(self, cmd, out=None, secrets=None):
            if "run-command show" in " ".join(cmd):
                self.azs.append(" ".join(cmd))
                return self.view
            return super().az(cmd, out, secrets)
    # What the machine said is quoted, any secret in it by its name.
    failed = json.dumps({"executionState": "Failed", "exitCode": 1, "output": "",
                         "error": "systemd-run: Failed to start transient service unit; key KEY"})
    b = started_bench(tmp_path, NoStart(str(tmp_path), failed))
    with pytest.raises(RuntimeError, match=r"(?s)did not start on the Linux machine \(Failed, exit 1\): "
                                           r".*Failed to start transient service unit; key <storage-key>$"):
        b.batch("runs", b.linux_steps()[:2], b.linux_done)
    b = started_bench(tmp_path, NoStart(str(tmp_path), "Internal error"))
    with pytest.raises(RuntimeError, match="did not start on the Linux machine .*not JSON: Internal error"):
        b.batch("runs", b.linux_steps()[:2], b.linux_done)

    # A batch whose state stops changing: past the step's bound and STALL_S more, the machine is asked.
    class Silent(Fake):
        def machine(self, name, steps):
            return {"name": name, "state": "running", "steps": []}
    x = Silent(str(tmp_path))
    b = started_bench(tmp_path, x)
    clock = iter(range(0, 10 ** 9, 1000))
    monkeypatch.setattr(run.time, "time", lambda: float(next(clock)))
    with pytest.raises(RuntimeError, match="batch runs: no step ended within 3300 s of the last"):
        b.batch("runs", b.linux_steps()[:2], b.linux_done)
    assert "systemctl status moruna-bench-runs" in x.azs[-1]
    # A state that is not there yet is looked for again, quietly; any other failure to read it is said.
    assert b.batch_state("checks", []) is None and "was not read" not in x.out.getvalue()
    x.fail_on = "-n batch/runs.json"
    assert b.batch_state("runs", []) is None and "the state of batch runs was not read" in x.out.getvalue()


def test_a_batch_already_started_is_not_started_again(tmp_path):
    x = Fake(str(tmp_path))
    b = started_bench(tmp_path, x)
    steps = b.linux_steps()[:3]
    b.batch("runs", steps, b.linux_done)
    b.batch("runs", steps, b.linux_done)
    assert x.started == ["runs"] and len(os.listdir(tmp_path / "live")) == 3
    # A side told to stop stops at its next look.
    b.stop.set()
    assert b.batch("runs", steps, b.linux_done) == {"state": "stopped"}
    # A run command that cannot be deleted is said and left to the group's tear-down; one that
    # fails to be made is still deleted.
    x = Fake(str(tmp_path / "d"), fail_on="run-command delete")
    b = started_bench(tmp_path / "d", x)
    b.batch("runs", steps, b.linux_done)
    assert x.started == ["runs"] and "the run command moruna-bench-runs was not deleted" in x.out.getvalue()
    x = Fake(str(tmp_path / "c"), fail_on="run-command create")
    b = started_bench(tmp_path / "c", x)
    with pytest.raises(RuntimeError, match="failed: run-command create"):
        b.batch("runs", steps, b.linux_done)
    assert x.azs[-1].startswith("az vm run-command delete --run-command-name moruna-bench-runs ")


def test_the_sides_run_at_once_and_the_first_failure_stops_the_other(tmp_path):
    x = run.Real(cloud(), str(tmp_path / "log"), io.StringIO())
    stop, seen = run.threading.Event(), []

    def node():
        seen.append("node")
        raise RuntimeError("the node stopped: moruna: fatal")

    def linux():
        for _ in range(500):
            if stop.wait(0.01):
                seen.append("linux saw stop")
                return
    with pytest.raises(RuntimeError, match="fatal"):
        x.parallel([linux, node], stop)
    assert stop.is_set() and set(seen) == {"node", "linux saw stop"}
    # A side that does not stop is left after WIND_DOWN_S: the run tears down regardless.
    x.WIND_DOWN_S, x.WAIT_SLICE_S = 0.2, 0.01
    stop.clear()
    hang = run.threading.Event()
    with pytest.raises(RuntimeError, match="fatal"):
        x.parallel([lambda: hang.wait(30), node], stop)
    hang.set()
    stop.clear()
    x.parallel([lambda: seen.append("a"), lambda: seen.append("b")], stop)
    assert seen[-2:] in (["a", "b"], ["b", "a"]) and not stop.is_set()
    # The node waits for the data, and gives up when the other side stops.
    ready = run.threading.Event()
    run.threading.Timer(0.05, ready.set).start()
    assert x.wait_for(ready, stop, "the data is in the container")
    stop.set()
    assert not x.wait_for(run.threading.Event(), stop, "never")
    # A state file is read once, then removed; a missing or broken one is no state.
    (tmp_path / "s.json").write_text('{"state": "done"}')
    assert x.read_state(str(tmp_path / "s.json"), []) == {"state": "done"} and not (tmp_path / "s.json").exists()
    (tmp_path / "s.json").write_text("{")
    assert x.read_state(str(tmp_path / "s.json"), []) is None and x.read_state(str(tmp_path / "none"), []) is None


def test_the_dry_run_runs_the_sides_one_after_the_other(tmp_path):
    x, stop, seen = run.DryRun(io.StringIO()), run.threading.Event(), []

    def fails():
        raise RuntimeError("no")
    with pytest.raises(RuntimeError):
        x.parallel([lambda: seen.append(1), fails, lambda: seen.append(3)], stop)
    assert seen == [1] and stop.is_set()
    x.parallel([lambda: seen.append(4)], stop)
    assert seen == [1]


def ready(tmp_path, **kw):
    """A Bench through node_ready against a Fake whose get-boot-log always fails on the CLI's bug."""
    x = Fake(str(tmp_path), **kw)
    b = run.Bench(nyc_plan(), x, cloud(), Args(), str(tmp_path), str(tmp_path))
    b.secrets["storage-key"] = "KEY"
    return x, b


def test_the_node_is_ready_by_its_health_and_pinned_by_its_console_blob(tmp_path):
    """The 2026-11-18 failure: get-boot-log raised the CLI's TypeError on every call while the node
    answered /v1/health. Readiness is /v1/health; the console is its blob, read with the key."""
    x, b = ready(tmp_path, health_after=3, console_after=1)
    b.node_ready()
    assert x.healths == 4 and x.transport is x.node and b.budget == 13 << 30 and b.boot_ms == 2140
    # While /v1/health said no, the console was read for a `fatal` line; then for the pin.
    reads = [a for a in x.azs if "-c bootdiagnostics-" in a]
    assert len(reads) == 4 and all("--account-key KEY" in a for a in reads)
    assert sum("get-boot-log -n" in a for a in x.azs) == 1
    out = x.out.getvalue()
    assert out.count("get-boot-log instead") == 1 and out.count("TypeError: unsupported operand") == 1
    # The instance is the machine's vmId, the console's when it names one: the tokens' audience.
    assert any("vm show -n moruna-node --query vmId" in a for a in x.azs)


@pytest.mark.parametrize("kw,message", [
    # No console within the bound: the run does not connect unpinned.
    ({"console_after": 10 ** 6}, "console was not read within 600 s: without its pin the run does not connect"),
    # The served certificate is not the console's: 1E.
    ({"served": "cd" * 32}, f"sha256 {'cd' * 32}\\) is not the one its console printed \\({PIN}\\)"),
    ({"served": ""}, "sha256 not read"),
    ({"vm_id": "i-2"}, "the console's instance i-1 is not the node machine's vmId i-2"),
    ({"health_after": 10 ** 6}, "did not answer GET https://203.0.113.9/v1/health with 200 within 600 s"),
    ({"health_after": 10 ** 6, "console_text": "moruna: fatal network: no lease\r\n"},
     "the node stopped: moruna: fatal network: no lease"),
])
def test_a_node_that_is_not_ready_or_not_its_console_is_never_connected(tmp_path, kw, message):
    x, b = ready(tmp_path, **kw)
    with pytest.raises(RuntimeError, match=message):
        b.node_ready()
    assert not hasattr(x, "transport") and b.node is None


def test_the_console_without_an_instance_line_takes_the_vm_id(tmp_path):
    x, b = ready(tmp_path, vm_id="i-1", console_text=f"moruna: tls sha256={PIN.upper()}\r\nmoruna: ready boot_ms=9\r\n")
    b.node_ready()
    assert b.boot_ms == 9 and x.transport is x.node


def test_the_console_blob_is_in_the_run_account():
    c = cloud()
    assert azure.console_blob(c, CONSOLE_URI + "\n") == ("bootdiagnostics-morunanod-0b1c",
                                                          "moruna-node.0b1c.serialconsole.log")
    for uri in ("", "<serial-console-uri>", "https://other.blob.core.windows.net/c/n",
                "http://mrnb202611180900.blob.core.windows.net/c/n", "https://mrnb202611180900.blob.core.windows.net/c"):
        with pytest.raises(ValueError, match="not a blob of the run's account"):
            azure.console_blob(c, uri)
    assert "serialConsoleLogBlobUri" in azure.boot_log_uri(c) and "vmId" in azure.node_vm_id(c)
    for cmd in (azure.boot_log_uri(c), azure.node_vm_id(c)):
        azure.guard(c, cmd)


def test_a_console_address_that_is_not_the_runs_falls_back_to_get_boot_log(tmp_path):
    class Elsewhere(Fake):
        def az(self, cmd, out=None, secrets=None):
            if "get-boot-log-uris" in " ".join(cmd):
                self.azs.append(" ".join(cmd))
                return "https://elsewhere.blob.core.windows.net/c/n"
            if "get-boot-log -n" in " ".join(cmd):
                self.azs.append(" ".join(cmd))
                return self.console_text
            return super().az(cmd, out, secrets)
    x = Elsewhere(str(tmp_path))
    b = run.Bench(nyc_plan(), x, cloud(), Args(), str(tmp_path), str(tmp_path))
    b.secrets["storage-key"] = "KEY"
    b.node_ready()
    assert "not a blob of the run's account" in x.out.getvalue() and b.boot_ms == 2140


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
    x = Fake(str(tmp_path), fail_on="vm create -n bench-linux")
    with pytest.raises(RuntimeError, match="failed"):
        run.Bench(nyc_plan(), x, azure.Cloud("202611180900"), Args(), str(tmp_path), "/s").go()
    assert x.azs[-2].startswith("az group delete") and x.azs[-1].startswith("az group exists")
    assert not any("run-command" in a for a in x.azs)
    # The node's side failing: the Linux machine's results are still collected, its checks never start.
    x = Fake(str(tmp_path / "n"), fail_on="vm create -n moruna-node")
    with pytest.raises(RuntimeError, match="failed"):
        run.Bench(nyc_plan(), x, azure.Cloud("202611180900"), Args(), str(tmp_path / "n"), "/s").go()
    assert x.started == ["setup", "runs"] and any("results/linux.tgz" in a for a in x.azs) and x.logged is False
    assert x.azs[-2].startswith("az group delete")


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
    assert ("| Moruna 0.7, the node | rust | none | 2.0 | 0.40 | 10.0 | 1.00 |  |  | 2.0 (read and decode) |  | 5.0 |  | "
            "1.0 (encode and write) | 0.5 | bit for bit | 1/1 |") in text
    six = {"read_s": 0.7, "decode_s": 0.3, "function_s": 4.0, "encode_s": 0.2, "write_s": 0.3, "wait_s": 0.5}
    text6 = table.render(summary_of([rec("moruna-node", 1, 2.0, split=six), rec("moruna066", 1, 9.0)]))
    assert "| blob |" not in text6 and "|  | 0.7 | 0.3 | 4.0 | 0.2 | 0.3 | 0.5 | bit for bit | 1/1 |" in text6
    assert text6.splitlines()[2].count("|") == text6.splitlines()[3].count("|") == 18
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

        def do_GET(self):
            self.send_response(200 if self.path == "/v1/health" else 404)
            self.send_header("Content-Length", "0")
            self.end_headers()

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
    # Readiness takes any certificate; the pin is read from the served one as azure-up.sh does.
    port = srv.server_address[1]
    th = threading.Thread(target=srv.handle_request)
    th.start()
    assert node_api.health("127.0.0.1", port) == 200
    th.join()
    th = threading.Thread(target=srv.handle_request)
    th.start()
    assert node_api.served_pin("127.0.0.1", port, openssl=OPENSSL) == hashlib.sha256(der).hexdigest()
    th.join()
    srv.server_close()
    assert node_api.health("127.0.0.1", port, timeout=2) is None
    assert node_api.served_pin("127.0.0.1", port, openssl=OPENSSL) == ""


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
    got = capsys.readouterr()
    res = json.loads(got.out.split("RESULT ", 1)[1])
    assert res["error"] and res["marks"]["main"] and res["settings"]["threads"] == 1 and res["data"] == "local"
    # Its traceback is on stderr, whose last lines measure.py keeps (work item 6.2c).
    assert got.err.startswith("Traceback") and res["error"].split(":")[0] in got.err.splitlines()[-1]
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
    # Spark's ABFS (Hadoop 3.5 refuses wasbs://), over the Blob API on an account without a
    # hierarchical namespace; an emulator by its address, over its own scheme.
    assert azure_store.abfss("nyc/f") == "abfss://data@acct.dfs.core.windows.net/nyc/f"
    assert azure_store.moruna_storage() == {"account": "acct", "access_key": "KEY"} and azure_store.moruna_env() == {}
    assert azure_store.polars_options() == {"account_name": "acct", "account_key": "KEY"}
    assert azure_store.adlfs_options() == {"account_name": "acct", "account_key": "KEY"}
    assert azure_store.spark_conf() == {"spark.hadoop.fs.azure.account.key.acct.dfs.core.windows.net": "KEY",
                                        "spark.hadoop.fs.azure.account.key.acct.blob.core.windows.net": "KEY",
                                        "spark.hadoop.fs.azure.account.hns.enabled": "false",
                                        "spark.hadoop.fs.azure.fns.account.service.type": "BLOB"}
    tls = stores.Store("devstoreaccount1", "K", "data", "https://192.168.64.1:10111/devstoreaccount1")
    assert tls.abfss("x") == "abfss://data@devstoreaccount1.dfs.core.windows.net/x"
    assert tls.spark_conf()["spark.hadoop.fs.azure.abfs.endpoint"] == "192.168.64.1:10111"
    assert tls.spark_conf()["spark.hadoop.fs.azure.always.use.https"] == "true"
    plain = stores.Store("devstoreaccount1", "K", "data", "http://127.0.0.1:10000/devstoreaccount1")
    assert plain.abfss("x") == "abfs://data@devstoreaccount1.dfs.core.windows.net/x"
    assert plain.spark_conf()["spark.hadoop.fs.azure.always.use.https"] == "false"
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
    # Spark with no JVM did not install: a run could not start its session.
    assert install.main(["--root", root, "--gate-src", str(tmp_path), "--engines", "rust-pool,spark,moruna066",
                         "--moruna066", str(src), "--jobs", "3"]) == 1
    done = json.load(open(os.path.join(root, "installed.json")))
    assert done["spark"]["error"].startswith("no JVM") and not done["spark"]["ok"] and done["rust-pool"]["ok"]
    assert done["moruna066"]["interpreter"] == "3.14t" and done["moruna066"]["tried"] == []
    assert ["cargo", "build", "--release", "-j", "3", "-p", "moruna-gate-baseline", "--bins"] in [c[0] for c in cmds]
    assert any(c[0][:2] == ["uv", "build"] and c[2] == "3" for c in cmds)
    assert any(c[0][-3:] == [os.path.join(root, "wheels", "moruna-0.6.6-cp314-cp314t-x.whl"), "pyarrow", "numpy"]
               for c in cmds)
    assert any(c[0][:4] == ["uv", "venv", "-q", "--managed-python"] and "3.14+gil" in c[0] for c in cmds)
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


# --- The Linux machine's batch and the data made in Azure (item 6.2b) --------------------------

import batch  # noqa: E402
import dataprep  # noqa: E402


class Puts:
    """A local stand-in for the container: each PUT kept, as Blob would take it."""

    def __init__(self):
        import http.server
        import threading
        puts = self.puts = []

        class H(http.server.BaseHTTPRequestHandler):
            def do_PUT(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                puts.append((self.path, self.headers.get("x-ms-blob-type"), json.loads(body)))
                self.send_response(201)
                self.end_headers()

            def log_message(self, *a):
                pass
        self.server = http.server.HTTPServer(("127.0.0.1", 0), H)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.base = f"http://127.0.0.1:{self.server.server_port}"

    def close(self):
        self.server.shutdown()


def test_a_batch_runs_its_steps_one_at_a_time_and_puts_its_state_after_each(tmp_path):
    """Each step a fresh process, in order, with the machine's PATH, its own env and only the
    secrets it names; one that fails, overruns its bound (killed, with what measure.py left in
    its cgroups) or cannot start is a result, and the batch goes on."""
    py = sys.executable
    show = ("import json, os; print('noise'); print('MEASURED ' + json.dumps({k: os.environ.get(k) for k in "
            "('PATH', 'HOME', 'BENCH_KEY', 'SAS_CODE', 'X')}))")
    steps = [{"id": "a", "argv": [py, "-c", show], "env": {"X": "1"}, "secrets": ["BENCH_KEY"], "timeout_s": 60},
             {"id": "b", "argv": [py, "-c", "import sys; sys.stderr.write('boom'); sys.exit(3)"], "timeout_s": 60},
             {"id": "c", "argv": [py, "-c", "import time; time.sleep(30)"], "timeout_s": 0.5},
             {"id": "d", "argv": [str(tmp_path / "none")]},
             {"id": "e", "argv": [py, "-c", "print('MEASURED {}')"]}]
    cg = tmp_path / "cg" / "score-duckdb-r1"
    cg.mkdir(parents=True)
    (cg / "cgroup.kill").write_text("0")
    puts = Puts()
    try:
        state = batch.run_batch("runs", steps, {"PATH": os.environ["PATH"], "HOME": "", "SAS_CODE": "s"},
                                {"BENCH_KEY": "K", "SAS_CODE": "s"}, str(tmp_path / "logs"),
                                batch.Publisher("runs", str(tmp_path), puts.base, "sv=1&sig=r"),
                                leftovers=str(tmp_path / "cg"))
    finally:
        puts.close()
    ended = state["steps"]
    assert state["state"] == "done" and [s["id"] for s in ended] == ["a", "b", "c", "d", "e"]
    assert all(x["ended"] <= y["started"] for x, y in zip(ended, ended[1:]))
    got = run.measured(ended[0]["stdout"])
    assert got == {"PATH": os.environ["PATH"], "HOME": None, "BENCH_KEY": "K", "SAS_CODE": None, "X": "1"}
    assert (ended[1]["exit"], ended[1]["stderr"]) == (3, "boom")
    assert ended[2]["timed_out"] and ended[2]["exit"] is None and (cg / "cgroup.kill").read_text() == "1"
    assert ended[3]["error"].startswith("not started") and ended[4]["exit"] == 0
    assert (tmp_path / "logs" / "a.out").read_text().startswith("noise\nMEASURED ")
    # The state, put after each step at results/batch/runs.json with the SAS, and kept here.
    assert len(puts.puts) == 1 + len(steps) + 1
    path, kind, last = puts.puts[-1]
    assert path == "/results/batch/runs.json?sv=1&sig=r" and kind == "BlockBlob" and last["state"] == "done"
    assert [len(p[2]["steps"]) for p in puts.puts] == [0, 1, 2, 3, 4, 5, 5]
    assert json.load(open(tmp_path / "runs.state.json"))["state"] == "done"


def test_a_batch_that_stops_says_so_and_a_lost_put_is_not_fatal(tmp_path, capsys):
    states = []
    with pytest.raises(TypeError):
        batch.run_batch("runs", [{"id": "a", "argv": None}], {}, {}, str(tmp_path), lambda s: states.append(dict(s)))
    assert states[-1]["state"] == "failed" and "TypeError" in states[-1]["error"]
    batch.Publisher("runs", str(tmp_path), "http://127.0.0.1:9", "sv=1", tries=1)({"state": "running"})
    assert "the state was not put (1)" in capsys.readouterr().err
    assert json.load(open(tmp_path / "runs.state.json")) == {"state": "running"}
    assert batch.load_env(str(tmp_path / "none")) == {} and batch.tail(str(tmp_path / "none"), 10) == ""


def test_batch_main_reads_its_steps_and_its_env_file(tmp_path):
    (tmp_path / "steps.json").write_text(json.dumps([{"id": "a", "argv": [sys.executable, "-c", "print('hi')"]}]))
    (tmp_path / "env").write_text(f"PATH={os.environ['PATH']}\nHOME=\nBENCH_KEY=a=b==\nnot a line\n")
    assert batch.load_env(str(tmp_path / "env"))["BENCH_KEY"] == "a=b=="
    assert batch.main(["--name", "setup", "--steps", str(tmp_path / "steps.json"), "--env", str(tmp_path / "env"),
                       "--dir", str(tmp_path), "--logs", str(tmp_path / "logs")]) == 0
    state = json.load(open(tmp_path / "setup.state.json"))
    assert state["steps"][0]["stdout"] == "hi\n" and state["of"] == 1


def test_the_linux_scripts_parse():
    for script in ("setup-linux.sh", "data-linux.sh"):
        p = subprocess.run(["bash", "-n", os.path.join(HERE, "engines", script)], capture_output=True, text=True)
        assert p.returncode == 0, p.stderr


NORMALIZE = os.path.join(HERE, "..", "vm", "normalize.py")


def raw_months(tmp_path, n=2):
    """TLC-shaped months (their own column names and types): normalize.py's input."""
    src = tmp_path / "src"
    src.mkdir()
    for m in range(1, n + 1):
        t = pa.table({"VendorID": pa.array([1, 2, 2], pa.int32()),
                      "tpep_pickup_datetime": pa.array([m * 10 ** 9, m * 10 ** 9 + 60, m * 10 ** 9 + 99],
                                                       pa.timestamp("us")),
                      "tpep_dropoff_datetime": pa.array([m * 10 ** 9 + 600] * 3, pa.timestamp("us")),
                      "PULocationID": pa.array([132, 161, 237], pa.int32()), "fare_amount": [10.5, 7.0, 52.0]})
        pq.write_table(t, src / f"yellow_tripdata_2024-0{m}.parquet")
    return src


def normalised_digests(tmp_path, src):
    out = tmp_path / "expected"
    subprocess.run([sys.executable, NORMALIZE, str(src), str(out)], check=True, capture_output=True)
    return {f: run.sha256_of(str(out / f)) for f in sorted(os.listdir(out))}


def test_the_data_is_fetched_normalised_checked_then_uploaded(tmp_path):
    src = raw_months(tmp_path)
    digests = normalised_digests(tmp_path, src)
    spec = {"source_url": f"file://{src}/{{file}}", "sha256": digests, "prefix": "nyc/", "normalize": NORMALIZE}
    uploaded = []
    rec = dataprep.prepare(spec, str(tmp_path / "work"), upload=lambda p, n: uploaded.append((n, run.sha256_of(p))))
    assert rec["ok"] and sorted(uploaded) == sorted((f"nyc/{f}", h) for f, h in digests.items())
    assert all(rec["files"][f]["ok"] and rec["files"][f]["raw_bytes"] > 0 for f in digests)
    assert not (tmp_path / "work" / "raw").exists() and not (tmp_path / "work" / "norm").exists()
    # One file that does not normalise to the plan's digest: nothing is uploaded.
    wrong = dict(digests, **{"yellow_tripdata_2024-02.parquet": "0" * 64})
    uploaded.clear()
    rec = dataprep.prepare(dict(spec, sha256=wrong), str(tmp_path / "work"), upload=lambda p, n: uploaded.append(n))
    assert not rec["ok"] and uploaded == [] and "1 of 2 files differ" in rec["error"]
    assert "yellow_tripdata_2024-02.parquet" in rec["error"] and rec["files"]["yellow_tripdata_2024-01.parquet"]["ok"]


def test_data_that_cannot_be_fetched_or_normalised_is_not_uploaded(tmp_path):
    import functools
    src = raw_months(tmp_path)
    digests = normalised_digests(tmp_path, src)
    os.remove(src / "yellow_tripdata_2024-02.parquet")
    spec = {"source_url": f"file://{src}/{{file}}", "sha256": digests, "normalize": NORMALIZE}
    fetcher = functools.partial(dataprep.fetch, tries=2, wait_s=0)
    rec = dataprep.prepare(spec, str(tmp_path / "w"), upload=pytest.fail, fetcher=fetcher)
    assert not rec["ok"] and rec["error"] == "1 of 2 files not fetched"
    assert "not fetched from file://" in rec["files"]["yellow_tripdata_2024-02.parquet"]["error"]
    bad = tmp_path / "bad.py"
    bad.write_text("import sys; sys.exit(4)")
    spec = dict(spec, sha256={"yellow_tripdata_2024-01.parquet": digests["yellow_tripdata_2024-01.parquet"]},
                normalize=str(bad))
    rec = dataprep.prepare(spec, str(tmp_path / "w"), upload=pytest.fail)
    assert not rec["ok"] and rec["error"].startswith("normalize.py exited 4")


def test_dataprep_main_prints_its_record(tmp_path, capsys, monkeypatch):
    src = raw_months(tmp_path, n=1)
    digests = normalised_digests(tmp_path, src)
    spec = {"source_url": f"file://{src}/{{file}}", "sha256": digests, "prefix": "nyc/", "normalize": NORMALIZE}
    got = []
    assert dataprep.main(["--spec", json.dumps(spec), "--work", str(tmp_path / "w")],
                         upload=lambda p, n: got.append(n)) == 0
    line = run.tagged(capsys.readouterr().out, "DATA ")
    assert line["ok"] and got == ["nyc/yellow_tripdata_2024-01.parquet"]
    (tmp_path / "spec.json").write_text(json.dumps(dict(spec, sha256={k: "0" * 64 for k in digests})))
    assert dataprep.main(["--spec", str(tmp_path / "spec.json"), "--work", str(tmp_path / "w")], upload=got.append) == 1
    for k in ("BENCH_ACCOUNT", "BENCH_KEY", "BENCH_CONTAINER"):
        monkeypatch.delenv(k, raising=False)
    with pytest.raises(SystemExit, match="no container"):
        dataprep.main(["--spec", json.dumps(spec), "--work", str(tmp_path / "w")])
    # With a container in the environment, its store's upload is the one used.
    import store as stores
    monkeypatch.setattr(stores.Store, "upload", lambda self, local, name: got.append((self.container, name)))
    monkeypatch.setenv("BENCH_ACCOUNT", "acct")
    monkeypatch.setenv("BENCH_KEY", "KEY")
    monkeypatch.setenv("BENCH_CONTAINER", "data")
    assert dataprep.main(["--spec", json.dumps(spec), "--work", str(tmp_path / "w")]) == 0
    assert got[-1] == ("data", "nyc/yellow_tripdata_2024-01.parquet")


JANUARY = os.path.expanduser("~/moruna-data/nyc/yellow_tripdata_2024-01.parquet")


@pytest.mark.skipif(not os.path.exists(JANUARY) or pa.__version__ != nyc_plan()["dataset"]["pyarrow"],
                    reason="needs TLC's January 2024 on this machine and the plan's pyarrow")
def test_tlcs_january_normalises_to_the_plans_digest(tmp_path):
    """TLC's own January 2024 (the CloudFront file, kept on this machine), through dataprep.py and
    normalize.py with the plan's pyarrow, is the plan's copy bit for bit."""
    jan = "yellow_tripdata_2024-01.parquet"
    spec = {"source_url": f"file://{os.path.dirname(JANUARY)}/{{file}}",
            "sha256": {jan: nyc_plan()["dataset"]["sha256"][jan]}, "normalize": NORMALIZE}
    rec = dataprep.prepare(spec, str(tmp_path), upload=lambda p, n: None)
    assert rec["ok"], rec


# --- Work item 6.2c: the engines that did not run, the node's CPU, a run repeated in part -------

def fake_uv(tmp_path, refuse):
    """install.sh as uv would run it: `uv venv` makes the interpreter; `refuse(interp)` says which
    interpreter's environment cannot import what."""
    seen = []

    def sh(cmd, log, env=None, cwd=None):
        seen.append(cmd)
        if cmd[:2] == ["uv", "venv"]:
            py = os.path.join(cmd[-1], "bin", "python")
            os.makedirs(os.path.dirname(py), exist_ok=True)
            open(py, "w").write(cmd[cmd.index("-p") + 1])
        if cmd[:3] == ["uv", "pip", "install"] and refuse(open(cmd[cmd.index("--python") + 1]).read()) == "install":
            raise RuntimeError("uv pip install... exited 1: no wheel for this interpreter")

    def versions(python, modules):
        interp = open(python).read()
        bad = refuse(interp)
        return {"python": interp.split("+")[0] + ".3", "gil": True,
                "modules": {m: (f"missing: {bad}" if m == "ray.data" and bad not in (None, "install") else "2.60")
                            for m in modules}}
    return seen, sh, versions


def test_each_engine_lands_on_the_newest_cpython_it_installs_and_imports_on(tmp_path, monkeypatch):
    """An engine that installs on 3.14 and 3.13 but whose module does not import there, as Ray
    2.60.0 did as published (2026-11-18: `ray.data` imports typing_extensions, which only a
    dependency below 3.13 brings): it lands on 3.12. Each interpreter tried before is kept with
    why, and leaves no environment behind. Ray itself now gets typing-extensions beside it, the
    dependency it omits, and so its best configuration, the newest CPython."""
    seen, sh, versions = fake_uv(tmp_path, lambda i: "ModuleNotFoundError" if i[:4] in ("3.14", "3.13") else None)
    monkeypatch.setattr(install, "sh", sh)
    monkeypatch.setattr(install, "versions", versions)
    root = tmp_path / "root"
    assert install.main(["--root", str(root), "--gate-src", str(tmp_path), "--engines", "ray"]) == 0
    assert install.PIP["ray"][1] == ["ray[data]", "typing-extensions", "aiohttp", "pyarrow", "pandas", "numpy"]
    assert all(c[-len(install.PIP["ray"][1]):] == install.PIP["ray"][1] for c in seen if c[:3] == ["uv", "pip", "install"])
    ray = json.load(open(root / "installed.json"))["ray"]
    assert (ray["ok"], ray["interpreter"], ray["python"], ray["gil"]) == (True, "3.12+gil", "3.12.3", True)
    assert ray["tried"] == [{"python": "3.14+gil", "error": "did not import: ray.data (missing: ModuleNotFoundError)"},
                            {"python": "3.13+gil", "error": "did not import: ray.data (missing: ModuleNotFoundError)"}]
    venvs = [c[-1] for c in seen if c[:2] == ["uv", "venv"]]
    assert venvs == [str(root / "venv-ray")] * 3 and open(root / "venv-ray" / "bin" / "python").read() == "3.12+gil"
    assert all("--managed-python" in c for c in seen if c[:2] == ["uv", "venv"])
    # None that holds: the engine did not install, and each interpreter's failure is said.
    seen, sh, versions = fake_uv(tmp_path, lambda i: "install")
    monkeypatch.setattr(install, "sh", sh)
    monkeypatch.setattr(install, "versions", versions)
    assert install.main(["--root", str(tmp_path / "r2"), "--gate-src", str(tmp_path), "--engines", "dask"]) == 1
    dask = json.load(open(tmp_path / "r2" / "installed.json"))["dask"]
    assert not dask["ok"] and "dask installs and imports on none of CPython 3.14+gil, 3.13+gil" in dask["error"]
    assert "3.10+gil: uv pip install... exited 1" in dask["error"] and not (tmp_path / "r2" / "venv-dask").exists()
    # Each interpreter's own error is kept whole, not only the end of them all: the setup's line
    # and setup.json carry it, so a setup that refuses says why (run 3's did not).
    assert [t["python"] for t in dask["tried"]] == list(install.GIL)
    assert all(t["error"] == "uv pip install... exited 1: no wheel for this interpreter" for t in dask["tried"])
    # An interpreter that does not answer at all.
    monkeypatch.setattr(install, "versions", lambda python, modules: {"error": "Segmentation fault"})
    with pytest.raises(RuntimeError, match="the interpreter did not answer: Segmentation fault"):
        install.imported("/py", ["ray"])


def test_spark_and_duckdb_get_their_blob_readers_ahead(tmp_path, monkeypatch):
    """hadoop-azure resolved into ROOT/ivy at setup, its jars recorded; DuckDB's azure extension
    installed into ROOT/duckdb-ext and loaded there."""
    jar = "/v/pyspark/jars/hadoop-client-api-3.5.0.jar"
    monkeypatch.setattr(install.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, jar + "\n", ""))
    made = []

    def sh(cmd, log, env=None, cwd=None):
        made.append(cmd)
        jars = tmp_path / "ivy" / "jars"
        jars.mkdir(parents=True, exist_ok=True)
        for j in ("org.apache.hadoop_hadoop-azure-3.5.0.jar", "com.microsoft.azure_azure-storage-7.0.1.jar"):
            (jars / j).write_text("jar")
    monkeypatch.setattr(install, "sh", sh)
    out = install.spark_azure(str(tmp_path), str(tmp_path / "log"), "openjdk 17")
    assert out["packages"] == "org.apache.hadoop:hadoop-azure:3.5.0" and out["ivy"] == str(tmp_path / "ivy")
    assert out["jars"] == [str(tmp_path / "ivy" / "jars" / "com.microsoft.azure_azure-storage-7.0.1.jar"),
                           str(tmp_path / "ivy" / "jars" / "org.apache.hadoop_hadoop-azure-3.5.0.jar")]
    assert "spark.jars.packages" in made[0][3]
    monkeypatch.setattr(install, "sh", lambda *a, **k: None)
    shutil.rmtree(tmp_path / "ivy")
    with pytest.raises(RuntimeError, match="hadoop-azure:3.5.0 was not resolved"):
        install.spark_azure(str(tmp_path), str(tmp_path / "log"), "openjdk 17")
    assert install.spark_azure(str(tmp_path), "", None)["azure_note"] == "hadoop-azure not resolved: no JVM"
    monkeypatch.setattr(install.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", ""))
    assert install.spark_azure(str(tmp_path), "", "openjdk 17")["azure_note"] == "pyspark carries no Hadoop client jar"
    # A run's session is given the jars as they are, and resolves nothing; before, by coordinate.
    import engine as eng
    import store as stores

    class A:
        mem_gib, tmp, root = 2, str(tmp_path / "t"), str(tmp_path)
    (tmp_path / "installed.json").write_text(json.dumps({"spark": {"jars": out["jars"], "packages": out["packages"]}}))
    st = stores.Store("acct", "KEY", "data")
    conf = eng.spark_settings(A, st)
    assert conf["spark.jars"] == ",".join(out["jars"]) and "spark.jars.packages" not in conf
    assert "KEY" not in json.dumps(conf)
    (tmp_path / "installed.json").write_text(json.dumps({"spark": {"packages": "p:q:1", "ivy": "/ivy"}}))
    assert eng.spark_settings(A, st)["spark.jars.packages"] == "p:q:1" and "spark.jars" not in eng.spark_settings(A, st)
    assert "spark.jars" not in eng.spark_settings(A, None) and "spark.jars.packages" not in eng.spark_settings(A, None)


def test_duckdb_reads_azure_through_its_curl_transport_and_installs_nothing(monkeypatch):
    """Run 3's DuckDB runs: the Azure SDK's own transport finds no CA certificates on Ubuntu, so
    every read failed ("Problem with the SSL CA cert"). The extension comes from ROOT/duckdb-ext,
    nothing is installed in the run, and its curl transport reads with the system's."""
    duckdb = pytest.importorskip("duckdb")
    import engine as eng
    import store as stores
    said = []

    class Con:
        def execute(self, sql):
            said.append(sql)
    monkeypatch.setattr(duckdb, "connect", lambda: Con())

    class A:
        mem_gib, cpus, root, inputs = 2, 2, "/opt/engines", ["nyc/a.parquet"]
    res = {}
    con, src = eng.duckdb_session(A, res, stores.Store("acct", "KEY", "data"))
    assert said[2:] == ["SET extension_directory='/opt/engines/duckdb-ext'", "SET autoinstall_known_extensions=false",
                        "LOAD azure", "SET azure_transport_option_type='curl'",
                        stores.Store("acct", "KEY", "data").duckdb_secret()]
    assert src == "read_parquet(['az://data/nyc/a.parquet'])" and "curl transport" in res["settings"]["azure"]


def failing_engine(lines):
    """An engine that prints `lines` on stderr, a RESULT with its error, and exits 1."""
    return ("import json, sys\n" + "".join(f"sys.stderr.write({line!r} + '\\n')\n" for line in lines)
            + "print('RESULT ' + json.dumps({'engine': 'x', 'error': 'IOException: the read failed'}))\nsys.exit(1)\n")


def test_a_failed_runs_own_error_reaches_the_batch_state_and_the_table(tmp_path, monkeypatch, capsys):
    """Run 3 kept no cause: the engine's stderr stayed in its record on the machine. Now its last
    lines are in the summary line the batch keeps (runs.json) and on measure.py's own stderr, which
    the batch keeps too, and the table's row shows the last of them."""
    lines = [f"line {i}" for i in range(20)] + ["x" * 400, "_duckdb.IOException: IO Error: Problem with the SSL CA cert"]
    monkeypatch.setattr(measure, "command", lambda *a: [sys.executable, "-c", failing_engine(lines)])
    monkeypatch.delenv("BENCH_ACCOUNT", raising=False)
    spec = {"id": "score-duckdb-r1", "job": "score", "engine": "duckdb", "workload": "score", "repeat": 1, "files": [],
            "cpus": 1, "mem_gib": 1}
    (tmp_path / "spec.json").write_text(json.dumps(spec))
    assert measure.main(["run", "--spec", str(tmp_path / "spec.json"), "--root", str(tmp_path), "--results",
                         str(tmp_path / "r"), "--work", str(tmp_path / "w")]) == 1
    got = capsys.readouterr()
    brief = run.measured(got.out)
    kept = brief["error_lines"].splitlines()
    assert len(kept) == measure.ERROR_LINES and kept[-1].endswith("Problem with the SSL CA cert")
    assert kept[-2] == "x" * 300 and kept[0] == "line 10"
    assert "score-duckdb-r1: exit 1: the engine's last lines:" in got.err and kept[-1] in got.err
    assert len(got.out) < 4096 and brief["exit_code"] == 1
    full = json.load(open(tmp_path / "r" / "score-duckdb-r1.json"))
    assert full["error_lines"] == brief["error_lines"] and "line 0" in full["stderr"]
    # The live record keeps them, and the table's row shows the last line.
    live = run.live_record(spec, brief)
    assert live["error_lines"] == brief["error_lines"] and not live["ok"]
    s = summary_of([live, rec("moruna066", 1, 9.0)])
    assert s["jobs"]["score"]["engines"]["duckdb"]["failures"] == [
        {"error": "_duckdb.IOException: IO Error: Problem with the SSL CA cert", "runs": ["score-duckdb-r1"]}]
    assert "| no run passed | 0/1: _duckdb.IOException: IO Error: Problem with the SSL CA cert |" in table.render(s)
    # With nothing on stderr: the RESULT's error, else the exit code.
    assert measure.error_lines("", {"error": "E: x"}, 1) == "E: x"
    assert measure.error_lines("\n\n", None, -9) == "no output on stderr; exit -9"
    assert len(measure.error_lines("y" * 299 + "\n" + ("z" * 299 + "\n") * 10, None, 1)) <= measure.ERROR_BYTES


def test_each_failed_runs_error_in_one_line_on_its_row():
    one = {"spec": {"id": "a"}, "ok": False, "error_lines": "Traceback\n  File x\nValueError: no | pipe\n\n"}
    two = {"spec": {"id": "b"}, "ok": False, "result": {"error": "Unsupported\n  WASB"}}
    three = {"spec": {"id": "c"}, "ok": False, "exit_code": 1}
    four = {"spec": {"id": "d"}, "ok": False, "diagnostic": "the job stopped"}
    five = {"spec": {"id": "e"}, "ok": False}
    assert [collect.error_line(r) for r in (one, two, three, four, five)] == [
        "ValueError: no | pipe", "Unsupported WASB", "exit 1", "the job stopped", "no error recorded"]
    fails = collect.failures([one, dict(one, spec={"id": "a2"}), two, {"spec": {"id": "ok"}, "ok": True}])
    assert fails == [{"error": "ValueError: no | pipe", "runs": ["a", "a2"]}, {"error": "Unsupported WASB", "runs": ["b"]}]
    row = {"ok": 1, "runs": 4, "failures": fails}
    assert table.runs_cell(row) == "1/4: ValueError: no \\| pipe (2 of 4); Unsupported WASB (1 of 4)"
    assert table.runs_cell({"ok": 2, "runs": 2, "failures": []}) == "2/2"
    assert table.cell("a  b\nc" + "d" * 300, 20) == "a b c" + "d" * 12 + "..."


def test_runs_of_some_engines_alone_with_the_reference_made_again():
    p = nyc_plan()
    every = plans.runs(p, run.DATA)
    assert plans.select(p, every, None) == (every, [])
    four, added = plans.select(p, every, ["ray", "dask", "duckdb", "spark"])
    assert added == [f"{j}-moruna066-r0" for j in p["jobs"]]
    assert {r["engine"] for r in four} == {"ray", "dask", "duckdb", "spark", "moruna066"}
    assert len(four) == 4 * 4 * 6 + 4 and [r["id"] for r in four] == [r["id"] for r in every if r in four]
    # Each job's reference run comes first in it, before any run checked against it.
    for job in p["jobs"]:
        assert next(r for r in four if r["job"] == job)["id"] == f"{job}-moruna066-r0"
    node, added = plans.select(p, every, ["moruna-node"])
    assert {r["engine"] for r in node} == {"moruna-node", "moruna066"} and len(node) == 24 + 4
    # 0.6.6 among them: its own runs, the noise pairs too, and nothing added.
    six, added = plans.select(p, every, ["moruna066"])
    assert added == [] and len(six) == 4 * 6 + 4 * 4
    for bad in (["nope"], []):
        with pytest.raises(plans.PlanError, match="--engines"):
            plans.select(p, every, bad)


def dry_engines(tmp_path, engines, *extra):
    return dry(tmp_path, "--engines", engines, *extra)


def test_the_dry_run_of_the_four_linux_engines(tmp_path):
    """`--engines ray,dask,duckdb,spark`: no node, no image; the Linux machine installs those four
    and what every check needs, makes 0.6.6's reference again, and runs those engines' runs."""
    lines = dry_engines(tmp_path, "ray,dask,duckdb,spark")
    text = "\n".join(lines)
    steps = [line for line in lines if line.startswith("[")]
    assert "-n node.vhd" not in text and "vm create -n moruna-node" not in text and "/v1/" not in text
    assert "1 machines of Standard_D4s_v4 at once, 4 vCPUs" in text
    assert "--engines ray,dask,duckdb,spark: those engines' runs of the plan alone" in text
    assert "0.6.6's learning runs, which make again the reference" in text and "score-moruna066-r0" in text
    assert "the Linux machine installs harness, moruna066, rust-pool, duckdb, ray, spark, dask" in text
    live = [line.rsplit("/live/", 1)[1][:-len(".json")] for line in steps if "/live/" in line]
    four, _ = plans.select(nyc_plan(), plans.runs(nyc_plan()), ["ray", "dask", "duckdb", "spark"])
    assert live == [r["id"] for r in four] and len(live) == 100
    starts = [s for s in steps if "run-command create" in s and "batch.py --name" in s]
    assert [s.split("batch.py --name ")[1].split()[0] for s in starts] == ["setup", "runs"]
    assert not (tmp_path / "results").exists()


def test_the_dry_run_of_the_node_alone(tmp_path):
    """`--engines moruna-node`, a new image: the node's runs, 0.6.6's reference made again on the
    Linux machine, and the node's outputs checked against it; no other engine runs."""
    vhd, tasks = fake_inputs(tmp_path)
    lines = dry_engines(tmp_path, "moruna-node", "--vhd", vhd, "--tasks", tasks, "--from", "203.0.113.9")
    text = "\n".join(lines)
    steps = [line for line in lines if line.startswith("[")]
    assert "the Linux machine installs harness, moruna066, rust-pool;" in text
    live = [line.rsplit("/live/", 1)[1][:-len(".json")] for line in steps if "/live/" in line]
    assert live == [f"{j}-moruna066-r0" for j in nyc_plan()["jobs"]]
    assert sum(" POST https://<node-address>/v1/jobs " in line for line in steps) == 24
    starts = [s for s in steps if "run-command create" in s and "batch.py --name" in s]
    assert [s.split("batch.py --name ")[1].split()[0] for s in starts] == ["setup", "runs", "checks"]
    with pytest.raises(SystemExit):
        dry_engines(tmp_path, "nope")


def test_a_whole_run_of_some_engines_and_its_table(tmp_path):
    """The rerun of the four Linux engines against fakes: only their runs and the reference's;
    its own table has their rows alone."""
    args = Args()
    args.engines = ["ray", "dask", "duckdb", "spark"]
    x = Fake(str(tmp_path))
    run.Bench(nyc_plan(), x, azure.Cloud("202611180900"), args, str(tmp_path), "/s").go()
    assert x.started == ["setup", "runs"] and x.node.jobs == 0
    assert x.uploaded["setup"][1]["env"]["ENGINES"] == "harness,moruna066,rust-pool,duckdb,ray,spark,dask"
    assert {s["id"].rsplit("-r", 1)[0].split("-", 1)[1] for s in x.uploaded["runs"]} == {
        "moruna066", "ray", "dask", "duckdb", "spark"}
    rows = x.summary["jobs"]["score"]["engines"]
    assert list(rows) == ["duckdb", "ray", "spark", "dask"] and rows["ray"]["runs"] == 5
    log = open(tmp_path / "benchmark-log.md").read()
    assert "- Engines: ray, dask, duckdb, spark alone (--engines) and 0.6.6's learning runs, the reference" in log
    assert "Node image" not in log


def test_the_setup_refuses_to_go_on_when_an_engine_did_not_install(tmp_path):
    args = Args()
    args.engines = ["ray", "duckdb"]
    x = Fake(str(tmp_path), not_installed=("ray",))
    with pytest.raises(RuntimeError, match="the setup did not install ray, so the run stops before any measured run: "
                                           r"ray: on CPython 3.14\+gil: uv pip install... exited 1: no wheel for this "
                                           r"interpreter; on CPython 3.13\+gil: uv pip install"):
        run.Bench(nyc_plan(), x, azure.Cloud("202611180900"), args, str(tmp_path), "/s").go()
    assert x.started == ["setup"] and not (tmp_path / "live").exists()
    # setup.json explains the refusal by itself: each interpreter tried and its install's error
    # lines, the step's exit code and what the batch kept of its stderr.
    kept = json.load(open(tmp_path / "setup.json"))
    assert kept["installed"]["ray"] is False and kept["installed"]["duckdb"] is True
    assert kept["tried"]["ray"][0] == {"python": "3.14+gil",
                                       "error": "uv pip install... exited 1: no wheel for this interpreter"}
    assert kept["errors"]["ray"].startswith("installs and imports on none of CPython 3.14+gil")
    assert kept["exit"] == 1 and kept["stderr"].startswith("Downloading cpython-3.14.3")
    assert x.azs[-2].startswith("az group delete") and x.logged is False
    # A setup with no line at all (it died before install.py): the step's own stderr is the why.
    b = run.Bench(nyc_plan(), Fake(str(tmp_path / "b")), cloud(), Args(), str(tmp_path / "b"), "/s")
    with pytest.raises(RuntimeError, match="no record"):
        b.setup_done({"id": "setup"}, {"id": "setup", "exit": 100, "stdout": "", "stderr": "E: Unable to locate"})
    assert json.load(open(tmp_path / "b" / "setup.json"))["stderr"] == "E: Unable to locate"


def test_a_rerun_writes_its_stamp_and_engines(tmp_path, monkeypatch):
    """A real run's directory holds run.json, which orders it when --table merges; a run without
    the node needs no image, tasks or address."""
    made = []

    class Stub:
        def __init__(self, plan, x, cloud, a, results, secrets):
            made.append((a.engines, results))

        def go(self):
            pass
    monkeypatch.setattr(run, "Bench", Stub)
    out = io.StringIO()
    assert run.main(["--plan", "nyc-2024", "--go", "--engines", "ray,dask", "--stamp", "202611190800", "--results",
                     str(tmp_path / "rerun")], out=out) == 0
    assert made == [(["ray", "dask"], str(tmp_path / "rerun"))]
    assert json.load(open(tmp_path / "rerun" / "run.json")) == {"stamp": "202611190800", "plan": "nyc-2024",
                                                                "engines": ["ray", "dask"]}
    with pytest.raises(SystemExit, match="a real run needs --vhd"):
        run.main(["--plan", "nyc-2024", "--go", "--engines", "moruna-node", "--results", str(tmp_path / "n")], out=out)


def results_dir(path, records, stamp=None, setup=None):
    os.makedirs(path / "live", exist_ok=True)
    for r in records:
        json.dump(r, open(path / "live" / f"{r['spec']['id']}.json", "w"))
    if stamp:
        (path / "run.json").write_text(json.dumps({"stamp": stamp}))
    if setup:
        (path / "setup.json").write_text(json.dumps(setup))
    return str(path)


def test_the_table_merges_results_directories_newest_per_run(tmp_path):
    """Run 3, then a rerun of DuckDB: the rerun's records replace run 3's for the same runs, the
    others stay; the directories ordered by their stamps whatever order they are given in."""
    # Run 3's full records hold the interpreter in the engine's RESULT; newer ones beside it too.
    old = results_dir(tmp_path / "202611181311", [rec("moruna066", 1, 9.0), rec("duckdb", 1, None, ok=False),
                                                  dict(rec("polars", 1, 15.0), result={"python": "3.14.2", "gil": True})],
                      setup={"wheel_066": "built", "wheel_066_name": "w.whl", "wheel_066_sha256": WHEEL_SHA})
    new = results_dir(tmp_path / "rerun", [dict(rec("duckdb", 1, 12.0), python="3.14.3", gil=True),
                                           rec("moruna066", 0, 9.5)], stamp="202611190800")
    p = nyc_plan()
    out = io.StringIO()
    assert run.main(["--plan", "nyc-2024", "--table", new, old, "--results", str(tmp_path / "merged")], out=out) == 0
    text = out.getvalue()
    assert text.startswith(f"# 4 of {len(plans.runs(p))} runs, the newest of each from 2 results directories; "
                           f"written to {tmp_path / 'merged'}")
    assert "the directories by their stamps, oldest first:" in text
    assert text.index(f"`{old}` (202611181311): 2 runs; 0.6.6's wheel: built") < text.index(f"`{new}` (202611190800): 2 runs")
    assert "| DuckDB | python | 3.14.3, GIL | 12.0 |" in text and "| Polars | python | 3.14.2, GIL | 15.0 |" in text
    merged = json.load(open(tmp_path / "merged" / "merged.json"))
    assert merged["dirs"] == [old, new] and merged["runs"]["score-duckdb-r1"] == new
    assert merged["runs"]["score-polars-r1"] == old
    assert (tmp_path / "merged" / "table.md").read_text() == text.split("\n\n", 1)[1]
    assert json.load(open(tmp_path / "merged" / "summary.json"))["sources"][1]["stamp"] == "202611190800"
    assert not (tmp_path / "202611181311" / "table.md").exists()
    # A directory with no stamp: the order given, oldest first, and said so.
    loose = results_dir(tmp_path / "loose", [rec("duckdb", 1, 30.0)])
    records, order, by_stamp = collect.merge([loose, old])
    assert (order, by_stamp) == ([loose, old], False)
    assert next(r for r in records if r["spec"]["id"] == "score-duckdb-r1")["from"] == old
    out = io.StringIO()
    assert run.main(["--plan", "nyc-2024", "--table", old, loose], out=out) == 0
    assert "a directory has no stamp, so in the order given" in out.getvalue() and "| DuckDB | python |  | 30.0 |" in out.getvalue()
    assert not (tmp_path / "loose" / "summary.json").exists()
    (tmp_path / "bad").mkdir()
    (tmp_path / "bad" / "run.json").write_text("[1]")
    assert collect.stamp_of(str(tmp_path / "bad")) is None
    with pytest.raises(SystemExit):
        run.main(["--plan", "nyc-2024", "--table", old, str(tmp_path / "none")], out=io.StringIO())
    # The merged table never goes into a directory it merges: that run's own table.md stays.
    with pytest.raises(SystemExit):
        run.main(["--plan", "nyc-2024", "--table", old, new, "--results", old], out=io.StringIO())
    assert not (tmp_path / "202611181311" / "table.md").exists()


class Unmeasured(FakeNode):
    """A node whose report did not measure the job's CPU (run 3's: usage_measured false, 0.0)."""

    def stream(self, method, path, token=None):
        for line in super().stream(method, path, token):
            if line.startswith("data: ") and '"type": "report"' in line:
                body = json.loads(line[6:])
                body["report"] = dict(body["report"], resources={"cpu_core_s": 0.0, "usage_measured": False})
                line = "data: " + json.dumps(body)
            yield line


def test_the_nodes_cpu_is_not_a_zero_it_did_not_measure(tmp_path):
    """The node's CPU-seconds are its report's resources.cpu_core_s when it measured them; run 3's
    0.0 with usage_measured false is no measurement: blank in the table, and the bar's CPU half
    unchecked rather than passed."""
    _, tasks = fake_inputs(tmp_path)
    args = Args()
    args.tasks = tasks
    x = Fake(str(tmp_path), node=Unmeasured())
    run.Bench(nyc_plan(), x, azure.Cloud("202611180900"), args, str(tmp_path), "/s").go()
    node_rec = json.load(open(tmp_path / "node" / "score-moruna-node-r1.json"))
    assert node_rec["cpu_s"] is None and node_rec["cpu_by"].startswith("not measured: the report's resources")
    text = open(tmp_path / "table.md").read()
    assert "| Moruna 0.7, the node | rust | none | 3.0 | 0.75 |  | 3.00 |" in text
    assert "- the bar: unchecked: 1.50x the best engine's operational time" in text
    assert "Moruna's CPU-seconds were not measured" in text
    # A measured run keeps its figure, and one slower than the bar misses whatever its CPU.
    assert json.load(open(tmp_path / "node" / "score-moruna-node-r1.json"))["cpu_by"] != "the engine's report"
    s = summary_of([rec("moruna-node", 1, 9.0, cpu=None), rec("moruna066", 1, 9.5), rec("moruna066-rust", 1, 9.6),
                    rec("duckdb", 1, 3.0)])
    bar = next(v for v in table.verdicts(s["jobs"]["score"]["engines"]) if v["step"] == "bar")
    assert bar["passed"] is False and "CPU not measured against the least of any row" in bar["text"]


def test_a_directory_markers_blob_is_not_the_output(tmp_path):
    """ABFS over the Blob API leaves an empty blob at Spark's output prefix beside the files under
    it: the download takes the files, and a prefix that names one blob takes that blob."""
    import pyarrow.fs as pafs
    import store as stores

    class Info:
        def __init__(self, path, kind, size=0):
            self.path, self.type, self.size = path, kind, size

    class FS:
        def __init__(self, files, marker):
            self.files, self.marker = files, marker

        def get_file_info(self, what):
            if isinstance(what, pafs.FileSelector):
                if what.base_dir in self.files:
                    raise NotADirectoryError(what.base_dir)
                return [Info(p, pafs.FileType.File, 5) for p in self.files if p.startswith(what.base_dir + "/")]
            kind = pafs.FileType.File if what in self.files or what == self.marker else pafs.FileType.NotFound
            return Info(what, kind, 0 if what == self.marker else 5)

    copied = []

    class S(stores.Store):
        def pyarrow_fs(self):
            return fs
    import unittest.mock as mock
    st = S("acct", "KEY", "data")
    d = str(tmp_path / "d")
    fs = FS(["data/out/x/_SUCCESS", "data/out/x/part-0.parquet"], marker="data/out/x")
    with mock.patch.object(pafs, "copy_files", lambda src, dst, source_filesystem: copied.append((src, dst))):
        assert st.download("out/x", d) == 10
        assert copied == [("data/out/x/_SUCCESS", f"{d}/_SUCCESS"), ("data/out/x/part-0.parquet", f"{d}/part-0.parquet")]
        copied.clear()
        fs = FS(["data/nyc/a.parquet"], marker=None)
        assert st.download("nyc/a.parquet", d) == 5 and copied == [("data/nyc/a.parquet", f"{d}/a.parquet")]
        copied.clear()
        assert st.download("nyc/none", d) == 0 and copied == []
    # The markers above a run's output are deleted before it starts: pyarrow's Azure filesystem
    # takes them for files, and Ray's writer and the Rust pool's upload fail under one
    # ("Not a directory: 'data/out'", the emulator, 2026-11-18).
    fs = FS(["data/out/x/part-0.parquet", "data/out/y/z"], marker="data/out")
    deleted = []
    fs.delete_file = deleted.append
    assert st.drop_markers("out/new-run") == ["data/out"] and deleted == ["data/out"]
    fs = FS(["data/out/x/part-0.parquet", "data/out/y/z"], marker=None)
    fs.delete_file = deleted.append
    assert st.drop_markers("out/y/z/run") == [] and deleted == ["data/out"]


def test_spark_reads_and_writes_through_abfs(tmp_path, monkeypatch):
    """Spark's session as engine.py builds it, against a stand-in pyspark: the recorded jars, the
    account key in the session's configuration but not in the record, and abfss:// both ways."""
    import types
    import engine as eng
    import store as stores
    calls = []

    class Chain:
        def __getattr__(self, name):
            def step(*a, **k):
                calls.append((name, a))
                return self
            return step

        @property
        def builder(self):
            return self

        @property
        def sparkContext(self):  # noqa: N802 (pyspark's name)
            return self

        @property
        def read(self):
            return self

        @property
        def write(self):
            return self

    chain = Chain()
    sql = types.ModuleType("pyspark.sql")
    sql.SparkSession = chain
    pyspark = types.ModuleType("pyspark")
    pyspark.__version__, pyspark.sql = "4.3.0", sql
    monkeypatch.setitem(sys.modules, "pyspark", pyspark)
    monkeypatch.setitem(sys.modules, "pyspark.sql", sql)
    for k in ("PYSPARK_PYTHON", "PYSPARK_DRIVER_PYTHON"):  # spark() sets them; restored after
        monkeypatch.setenv(k, "")
    (tmp_path / "installed.json").write_text(json.dumps({"spark": {"jars": ["/ivy/jars/hadoop-azure.jar"]}}))

    class A:
        mem_gib, cpus, tmp, root, workload = 2, 2, str(tmp_path / "t"), str(tmp_path), "score"
        inputs, out = ["nyc/a.parquet"], "out/score-spark-r1"
    marks, res = {}, {}
    eng.spark(A, lambda b: b, marks, res, stores.Store("acct", "KEY", "data"))
    configs = dict(a for name, a in calls if name == "config")
    assert configs["spark.jars"] == "/ivy/jars/hadoop-azure.jar"
    assert configs["spark.hadoop.fs.azure.account.key.acct.dfs.core.windows.net"] == "KEY"
    assert configs["spark.hadoop.fs.azure.fns.account.service.type"] == "BLOB"
    assert ("parquet", ("abfss://data@acct.dfs.core.windows.net/nyc/a.parquet",)) in calls
    assert ("parquet", ("abfss://data@acct.dfs.core.windows.net/out/score-spark-r1",)) in calls
    assert "KEY" not in json.dumps(res) and res["version"] == "4.3.0" and marks["done"] >= marks["start"]


def test_a_failed_0_6_6_run_prints_its_traceback(tmp_path, monkeypatch, capsys):
    import types
    import engine as eng
    fake = types.ModuleType("moruna")
    fake.__version__ = "0.6.6"
    fake.ParquetSource = fake.ParquetSink = fake.function = lambda *a, **k: None

    def run(*a, **k):
        raise RuntimeError("the job stopped")
    fake.run = run
    monkeypatch.setitem(sys.modules, "moruna", fake)

    class A:
        staging = profiles = report = ""
        cpus, inputs, out = 1, ["/in.parquet"], str(tmp_path / "o")
    res = {}
    eng.moruna(A, None, {}, res, None)
    assert res["error"] == "RuntimeError: the job stopped"
    assert capsys.readouterr().err.splitlines()[-1] == "RuntimeError: the job stopped"


def test_0_6_6s_rust_function_is_installed_and_imported_in_0_6_6s_environment(tmp_path, monkeypatch):
    py = tmp_path / "venv-moruna066" / "bin" / "python"
    py.parent.mkdir(parents=True)
    py.write_text("")
    monkeypatch.setattr(install, "sh", lambda *a, **k: None)
    monkeypatch.setattr(install, "versions", lambda python, modules: {
        "python": "3.14.3", "gil": False, "modules": {m: "1" for m in modules}})
    assert install.main(["--root", str(tmp_path), "--gate-src", str(tmp_path), "--engines", "moruna066-rust"]) == 0
    got = json.load(open(tmp_path / "installed.json"))["moruna066-rust"]
    assert got["ok"] and got["gil"] is False and set(got["modules"]) == {"moruna_gate", "pyarrow", "numpy"}


def test_measure_drops_a_marker_above_its_output_first(tmp_path, monkeypatch):
    """An empty blob where a run's output prefix has a directory goes before the run, outside
    every timing, and the record says so."""
    import store as stores
    blob = tmp_path / "blob"
    st = LocalStore(blob)
    st.upload(str(fixture_parquet(tmp_path)), "nyc/in.parquet")
    (blob / "data" / "out").write_bytes(b"")
    monkeypatch.setattr(stores.Store, "from_env", classmethod(lambda cls, env=None: st))
    src = os.path.join(FIX, "score", "main.expected.arrow")
    monkeypatch.setattr(measure, "command", lambda spec, root, out, flist, tmp, report: [
        sys.executable, "-c", stand_in(src, True), out])
    spec = {"id": "score-ray-r1", "engine": "ray", "workload": "score", "files": [], "cpus": 1, "mem_gib": 1,
            "inputs": ["nyc/in.parquet"], "output": "out/score-ray-r1", "env": {"BLOBROOT": str(blob)}}
    (tmp_path / "spec.json").write_text(json.dumps(spec))
    assert measure.main(["run", "--spec", str(tmp_path / "spec.json"), "--root", str(tmp_path / "eng"), "--results",
                         str(tmp_path / "r"), "--work", str(tmp_path / "w"),
                         "--reference-json", str(reference_json(tmp_path))]) == 0
    r = json.load(open(tmp_path / "r" / "score-ray-r1.json"))
    assert r["ok"] and r["markers_dropped"] == ["data/out"] and (blob / "data" / "out" / "score-ray-r1").is_dir()


def test_the_tables_python_column_names_each_engines_interpreter_and_gil(tmp_path, monkeypatch, capsys):
    """The engines differ now: each row says the CPython its runs recorded and whether it has the
    GIL. measure.py lifts both from the engine's RESULT into its record and its summary line."""
    gil = dict(rec("duckdb", 1, 3.0), python="3.14.3", gil=True)
    free = dict(rec("moruna066", 1, 9.0), python="3.14.3", gil=False)
    old = dict(rec("moruna066-rust", 1, 6.0), result={"python": "3.14.2", "gil": False})  # a full record of run 3
    assert collect.interpreter([gil], "duckdb") == "3.14.3, GIL"
    assert collect.interpreter([free, free], "moruna066") == "3.14.3, free-threaded"
    assert collect.interpreter([old], "moruna066-rust") == "3.14.2, free-threaded"
    assert collect.interpreter([rec("ray", 1, 1.0)], "ray") == "" and collect.interpreter([], "ray") == ""
    assert collect.interpreter([gil], "rust-pool") == "none" and collect.interpreter([], "moruna-node") == "none"
    # Runs of two setups, in a merged table: each distinct interpreter. No GIL recorded reads as GIL.
    two = [dict(rec("ray", 1, 1.0), python="3.12.13", gil=True), dict(rec("ray", 2, 1.0), python="3.14.3")]
    assert collect.interpreter(two, "ray") == "3.12.13, GIL; 3.14.3, GIL"
    text = table.render(summary_of([gil, free, old, rec("moruna-node", 1, 2.0)]))
    head = next(line for line in text.splitlines() if line.startswith("| Engine"))
    assert head.startswith("| Engine | Function | Python | Operational s |")
    rule = text.splitlines()[text.splitlines().index(head) + 1]
    assert rule.count("---") == head.count("|") - 1 == 17
    assert "| DuckDB | python | 3.14.3, GIL | 3.0 |" in text
    assert "| 0.6.6, Python function | python | 3.14.3, free-threaded | 9.0 |" in text
    assert "| 0.6.6, Rust function | rust | 3.14.2, free-threaded | 6.0 |" in text
    assert "| Moruna 0.7, the node | rust | none | 2.0 |" in text
    # measure.py: the engine's RESULT says its interpreter; the record and the summary line keep it.
    engine = ("import json, time\nt = time.time()\nprint('RESULT ' + json.dumps({'engine': 'x', 'python': '3.14.3', "
              "'gil': False, 'marks': {'main': t, 'ready': t, 'start': t, 'done': t}}))\n")
    monkeypatch.setattr(measure, "command", lambda *a: [sys.executable, "-c", engine])
    monkeypatch.delenv("BENCH_ACCOUNT", raising=False)
    spec = {"id": "score-moruna066-r1", "job": "score", "engine": "moruna066", "workload": "score", "repeat": 1,
            "files": [], "cpus": 1, "mem_gib": 1}
    (tmp_path / "spec.json").write_text(json.dumps(spec))
    measure.main(["run", "--spec", str(tmp_path / "spec.json"), "--root", str(tmp_path), "--results",
                  str(tmp_path / "r"), "--work", str(tmp_path / "w")])
    brief = run.measured(capsys.readouterr().out)
    full = json.load(open(tmp_path / "r" / "score-moruna066-r1.json"))
    assert (brief["python"], brief["gil"]) == (full["python"], full["gil"]) == ("3.14.3", False)
    live = run.live_record(spec, brief)
    assert collect.interpreter([live], "moruna066") == "3.14.3, free-threaded"


def setup_line(tmp_path, installed):
    """setup-linux.sh's last step, run as the script runs it: the Python between its `<<'EOF'` and
    `EOF`, given installed.json; the JSON of its SETUP line and the line's size in bytes."""
    script = open(os.path.join(HERE, "engines", "setup-linux.sh")).read()
    code = script.split("<<'EOF'\n", 1)[1].split("\nEOF\n", 1)[0]
    (tmp_path / "installed.json").write_text(json.dumps(installed))
    wheel = tmp_path / "moruna-0.6.6-cp314-cp314t-linux_x86_64.whl"
    wheel.write_bytes(b"wheel")
    p = subprocess.run([sys.executable, "-I", "-c", code, str(tmp_path / "installed.json"), "container", "3",
                        "openjdk 17", str(wheel)], capture_output=True, text=True)
    assert p.returncode == 0 and p.stdout.startswith("SETUP ") and p.stdout.count("\n") == 1, p.stderr
    return json.loads(p.stdout[len("SETUP "):]), len(p.stdout.encode())


def test_the_setups_line_says_why_each_engine_did_not_install(tmp_path):
    """Run 3's setup.json said `"ray": false` and no more. The setup's line now carries, for an
    engine that did not install, its error's last lines and each interpreter's own, and stays
    inside the 8 KiB of output the batch keeps however many failed."""
    uv = "  × No solution found when resolving dependencies:\n  ╰─▶ Because ray has no wheels with a matching ABI tag\n"
    tried = [{"python": f"{v}+gil", "error": f"uv pip install... exited 1: on {v}\n" + uv * 4} for v in install.PYTHONS]
    ok = {"ok": True, "python": "3.14.3", "gil": True, "tried": []}
    installed = {"harness": ok, "ray": {"ok": False, "error": "ray installs and imports on none of CPython", "tried": tried},
                 "duckdb": {"ok": False, "error": "x" * 900 + "IO Error: the extension has no valid signature"},
                 "dask": dict(ok, python="3.13.9", tried=[{"python": "3.14+gil", "error": "did not import: adlfs"}])}
    got, size = setup_line(tmp_path, installed)
    assert got["installed"] == {"harness": True, "ray": False, "duckdb": False, "dask": True}
    assert got["errors"]["ray"] == "installs and imports on none of CPython " + ", ".join(install.GIL)
    assert [t["python"] for t in got["tried"]["ray"]] == list(install.GIL)
    assert all("No solution found" in t["error"] and "ray has no wheels" in t["error"] for t in got["tried"]["ray"])
    assert got["errors"]["duckdb"].endswith("IO Error: the extension has no valid signature") and "duckdb" not in got["tried"]
    assert got["interpreters"]["dask"] == {"python": "3.13.9", "gil": True, "tried": ["3.14+gil"]}
    assert got["wheel_066"] == "container" and got["wheel_066_sha256"] == run.sha256_of(
        str(tmp_path / "moruna-0.6.6-cp314-cp314t-linux_x86_64.whl")) and size <= 6000
    # Every Python engine failing on every interpreter: each error is cut, and the line still fits.
    many = {e: {"ok": False, "error": "none", "tried": tried} for e in install.PIP}
    got, size = setup_line(tmp_path, many)
    assert size <= 6000 and set(got["tried"]) == set(install.PIP) and len(got["tried"]["polars"]) == 5
    assert all(t["error"] for t in got["tried"]["ray"])
    # run.py reads the same line: the refusal names each interpreter and its error.
    b = run.Bench(nyc_plan(), Fake(str(tmp_path / "b")), cloud(), Args(), str(tmp_path / "b"), "/s")
    line = "SETUP " + json.dumps(got)
    with pytest.raises(RuntimeError, match=r"(?s)ray: on CPython 3.14\+gil: .*; on CPython 3.10\+gil: "):
        b.setup_done({"id": "setup"}, {"id": "setup", "exit": 1, "stdout": line, "stderr": ""})
    assert json.load(open(tmp_path / "b" / "setup.json"))["tried"]["ray"] == got["tried"]["ray"]
