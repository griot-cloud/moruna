#!/usr/bin/env python3
"""nyc-blob.sh and nyc-data.sh (work item 3.8b), against stand-ins for `az` and `ssh-keygen`.

Nothing here reaches Azure. The stand-in `az` records every call, with the storage key its
environment gave it, and answers as Azure would; one that is never to be called fails the test if
it is.

- The dry run prints every command, exactly the list below, and runs none; `--delete`'s too.
- Every `az` command names subscription griot-dev, and every group it names is the run's.
- The guard refuses any subscription but griot-dev, any group not named `moruna-nyc-*`, a group
  that exists already, missing tasks, a run's directory that exists, and a --delete with no group
  of this script's, before anything runs.
- A run that goes well fills `functions` from the Mac and `data` on the machine, whose script
  carries the code and never the key (the key is the run command's protected parameter, read from
  its file); checks the `DATA` line and the twelve months; deletes the machine and all made for
  it; then makes the two SASes, writes them to RUN/blob (mode 600) and shows neither.
- A run whose data is not right deletes the group, and makes no SAS.
- --delete deletes the group RUN/group names and shows it gone.
- nyc-data.sh, as the machine runs it: the code unpacked from the script, the plan's pyarrow and
  the spec given to data-linux.sh.

Run: python3 -I kernel/node/tools/test_nyc_blob.py
"""
import base64
import io
import json
from pathlib import Path
import re
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest

TOOLS = Path(__file__).resolve().parent
SCRIPT = TOOLS / "nyc-blob.sh"
REPO = TOOLS.parents[2]
GROUP = "moruna-nyc-name"
ACCOUNT = "mrnnycname"
CODE = ["bench/node/engines/data-linux.sh", "bench/node/engines/dataprep.py", "bench/node/engines/store.py",
        "bench/vm/normalize.py", "bench/node/plans/nyc-2024.json"]
KEYED = 'AZURE_STORAGE_KEY="$(cat RUNDIR/account.key)" az storage '
ON = f"--account-name {ACCOUNT} --auth-mode key --subscription griot-dev"
JOBS = ("score", "occupancy", "enrich", "audit")
VM_CREATE = (
    f"az vm create -n nyc-data -g {GROUP} -l centralindia --zone 3 --image Canonical:ubuntu-24_04-lts:server:latest "
    "--size Standard_D2s_v4 --security-type Standard --accelerated-networking false --public-ip-sku Standard "
    "--public-ip-address nyc-data-ip --vnet-name nyc-data-vnet --subnet nyc-data-subnet --nsg nyc-data-nsg "
    "--nsg-rule NONE --os-disk-name nyc-data-os --storage-sku StandardSSD_LRS --os-disk-delete-option Delete "
    "--nic-delete-option Delete --admin-username nyc --ssh-key-values RUNDIR/vm-key.pub --subscription griot-dev "
    "-o none")
CHECK_DATA = (
    "python3 -I -c 'import json, sys; v = json.load(open(sys.argv[1])); out = v.get(\"output\") or \"\"; "
    "print(out[-2000:]); print(\"error:\", (v.get(\"error\") or \"\")[-1000:]); print(\"state\", "
    "v.get(\"executionState\"), \"exit\", v.get(\"exitCode\")); data = [l[5:] for l in out.splitlines() "
    "if l.startswith(\"DATA \")]; d = json.loads(data[-1]) if data else {}; open(sys.argv[2], \"w\")"
    ".write(json.dumps(d)); sys.exit(0 if v.get(\"exitCode\") == 0 and d.get(\"ok\") is True else 1)' "
    "RUNDIR/data-view.json RUNDIR/data.json")
GONE = [
    f"az group delete -n {GROUP} --yes --subscription griot-dev",
    f"[ \"$(az group exists -n {GROUP} --subscription griot-dev)\" = false ]",
    "az group list --subscription griot-dev --query \"[?starts_with(name, 'moruna-nyc-')].name\" -o tsv",
]
# Every command of a dry run, in order; last, the deletion a run that fails runs.
DRY_RUN = [
    "mkdir -m 700 RUNDIR",
    f"printf '%s\\n' {GROUP} > RUNDIR/group",
    "ssh-keygen -q -t ed25519 -N '' -C nyc-data -f RUNDIR/vm-key",
    f"[ \"$(az group exists -n {GROUP} --subscription griot-dev)\" = false ]",
    f"az group create -n {GROUP} -l centralindia --tags purpose=moruna-nyc item=3.8b --subscription griot-dev -o none",
    f"az storage account create -n {ACCOUNT} -g {GROUP} -l centralindia --sku Standard_LRS --kind StorageV2 "
    "--min-tls-version TLS1_2 --allow-blob-public-access false --subscription griot-dev -o none",
    f"(umask 077 && az storage account keys list -n {ACCOUNT} -g {GROUP} --query '[0].value' -o tsv "
    "--subscription griot-dev > RUNDIR/account.key)",
    f"{KEYED}container create -n data {ON} -o none",
    f"{KEYED}container create -n functions {ON} -o none",
    *[f"{KEYED}blob upload -c functions -n gate-{j} -f TASKS/gate-{j} {ON} -o none" for j in JOBS],
    f"(cd {REPO} && COPYFILE_DISABLE=1 tar --format ustar -czf - {' '.join(CODE)}) | base64 | tr -d '\\n' "
    "> RUNDIR/code.b64",
    "{ printf \"#!/bin/bash\\nCODE='%s'\\nexport BENCH_ACCOUNT=%s BENCH_CONTAINER=data\\n\" "
    f"\"$(cat RUNDIR/code.b64)\" {ACCOUNT}; cat {REPO}/kernel/node/tools/nyc-data.sh; }} > RUNDIR/data.sh",
    f"az network nsg create -n nyc-data-nsg -g {GROUP} -l centralindia --subscription griot-dev -o none",
    VM_CREATE,
    f"perl -e 'alarm shift; exec @ARGV; exit 127' 4000 az vm run-command create --run-command-name nyc-data "
    f"--vm-name nyc-data -g {GROUP} --location centralindia --script \"$(cat RUNDIR/data.sh)\" "
    "--async-execution false --timeout-in-seconds 3600 --protected-parameters BENCH_KEY=@RUNDIR/account.key "
    "--subscription griot-dev -o none",
    f"az vm run-command show --run-command-name nyc-data --vm-name nyc-data -g {GROUP} --instance-view "
    "--query instanceView -o json --subscription griot-dev > RUNDIR/data-view.json",
    CHECK_DATA,
    f"{KEYED}blob list -c data --prefix nyc/ --query '[].name' -o tsv {ON} > RUNDIR/data.list && "
    "[ \"$(grep -c -x 'nyc/yellow_tripdata_2024-[01][0-9]\\.parquet' RUNDIR/data.list)\" = 12 ]",
    f"az vm delete -n nyc-data -g {GROUP} --yes --subscription griot-dev -o none",
    f"az network public-ip delete -n nyc-data-ip -g {GROUP} --subscription griot-dev",
    f"az network vnet delete -n nyc-data-vnet -g {GROUP} --subscription griot-dev",
    f"az network nsg delete -n nyc-data-nsg -g {GROUP} --subscription griot-dev",
    f"[ \"$(az resource list -g {GROUP} --query '[].type' -o tsv --subscription griot-dev)\" = "
    "Microsoft.Storage/storageAccounts ]",
    "python3 -I -c 'import datetime as d; print((d.datetime.now(d.timezone.utc) + d.timedelta(hours=6))"
    ".strftime(\"%Y-%m-%dT%H:%MZ\"))' > RUNDIR/expires",
    f"(umask 077 && {KEYED}container generate-sas -n data --permissions racwl --expiry \"$(cat RUNDIR/expires)\" "
    f"--https-only {ON} -o tsv > RUNDIR/data.sas)",
    f"(umask 077 && {KEYED}container generate-sas -n functions --permissions r --expiry "
    f"\"$(cat RUNDIR/expires)\" --https-only {ON} -o tsv > RUNDIR/functions.sas)",
    "(umask 077 && printf 'account=%s\\ncontainer=data\\nfunctions=functions\\ndata_sas=%s\\nfunctions_sas=%s\\n"
    f"expires=%s\\n' {ACCOUNT} \"$(cat RUNDIR/data.sas)\" \"$(cat RUNDIR/functions.sas)\" "
    "\"$(cat RUNDIR/expires)\" > RUNDIR/blob)",
    "[ -s RUNDIR/data.sas ] && [ -s RUNDIR/functions.sas ]",
    *GONE,
]

DATA_SAS = "se=2026-11-18T20%3A00Z&sp=racwl&spr=https&sv=2022-11-02&sr=c&sig=DATAdataDATAdata%2Bdata%3D"
FUNCTIONS_SAS = "se=2026-11-18T20%3A00Z&sp=r&spr=https&sv=2022-11-02&sr=c&sig=FUNCfuncFUNCfunc%2Bfunc%3D"
KEY = "the-account-key=="
# The stand-in for `az`: each call recorded with the storage key its environment gave it, and
# answered as Azure would. The run command's script and what its protected parameter's file held
# are kept in STUB_STATE.
AZ = f"""#!{sys.executable} -I
import json, os, sys
argv = sys.argv[1:]
log, state = os.environ["STUB_LOG"], os.environ["STUB_STATE"]
with open(log, "a") as f:
    f.write("az " + " ".join(argv) + " |key=" + os.environ.get("AZURE_STORAGE_KEY", "") + "\\n")
cmd = " ".join(argv)
env = lambda k: os.environ.get(k, "")
calls = open(log).read()
if cmd.startswith("group exists "):
    print("false" if " group delete " in calls or not env("STUB_GROUP_EXISTS") else "true")
elif cmd.startswith("storage account create ") and env("STUB_FAIL"):
    sys.stderr.write("the account was refused\\n")
    sys.exit(1)
elif cmd.startswith("storage account keys list "):
    print({KEY!r})
elif cmd.startswith("vm run-command create "):
    open(os.path.join(state, "script.sh"), "w").write(argv[argv.index("--script") + 1])
    name, _, path = argv[argv.index("--protected-parameters") + 1].partition("=@")
    open(os.path.join(state, "protected"), "w").write(name + "=" + open(path).read().rstrip("\\n"))
elif cmd.startswith("vm run-command show "):
    data = {{"ok": env("STUB_DATA") != "bad", "files": {{}}}}
    print(json.dumps({{"executionState": "Succeeded", "exitCode": int(env("STUB_EXIT") or 0), "error": "",
                      "output": "apt and pip, quiet\\nDATA " + json.dumps(data) + "\\n"}}))
elif cmd.startswith("storage blob list "):
    n = 11 if env("STUB_FEW") else 12
    print("\\n".join(["nyc/yellow_tripdata_2024-%02d.parquet" % m for m in range(1, n + 1)] + ["nyc/other.txt"]))
elif cmd.startswith("resource list "):
    print("Microsoft.Storage/storageAccounts")
elif cmd.startswith("storage container generate-sas -n data "):
    print({DATA_SAS!r})
elif cmd.startswith("storage container generate-sas -n functions "):
    print({FUNCTIONS_SAS!r})
"""
SSH_KEYGEN = r"""#!/bin/sh
echo "ssh-keygen $*" >> "$STUB_LOG"
while [ "$#" -gt 0 ]; do [ "$1" = -f ] && { echo private > "$2"; echo "ssh-ed25519 AAAA nyc-data" > "$2.pub"; }; shift; done
"""
NEVER = """#!/bin/sh
echo "$(basename "$0") $* (called in a dry run)" >> "$STUB_LOG"
exit 99
"""


class NycBlob(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        (self.root / "TASKS").mkdir()
        for job in JOBS:
            (self.root / f"TASKS/gate-{job}").write_bytes(b"\x7fELF a task")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.state = self.root / "state"
        self.state.mkdir()
        self.log = self.root / "calls"
        self.log.touch()

    def stubs(self, **scripts):
        for name, text in scripts.items():
            path = self.bin / name
            path.write_text(text)
            path.chmod(0o755)
        python = self.bin / "python3"
        if not python.exists():
            python.write_text(f"#!/bin/sh\nexec {sys.executable} \"$@\"\n")
            python.chmod(0o755)

    def live(self):
        self.stubs(az=AZ, **{"ssh-keygen": SSH_KEYGEN})

    def calls(self):
        return self.log.read_text().splitlines()

    def az(self):
        return [call.split(" |key=")[0] for call in self.calls() if call.startswith("az ")]

    def run_script(self, *arguments, extra=None):
        env = {"PATH": f"{self.bin}:/usr/bin:/bin", "STUB_LOG": str(self.log), "STUB_STATE": str(self.state),
               **(extra or {})}
        return subprocess.run(["bash", str(SCRIPT), *arguments], cwd=self.root, capture_output=True, text=True,
                              env=env, timeout=120, check=False)

    def arguments(self, *more, group=GROUP, run="RUNDIR"):
        return ["--run", run, "--tasks", "TASKS", "--group", group, *more]

    def test_a_dry_run_prints_every_command_and_runs_none(self):
        self.stubs(az=NEVER, perl=NEVER, tar=NEVER, **{"ssh-keygen": NEVER})
        done = self.run_script(*self.arguments("--dry-run"))
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout.splitlines(), DRY_RUN)
        self.assertEqual(done.stderr, "")
        self.assertEqual(self.calls(), [], "a dry run runs nothing")
        self.assertFalse((self.root / "RUNDIR").exists())
        # --delete's dry run: the group RUN/group names, deleted and shown gone.
        (self.root / "OLD").mkdir()
        (self.root / "OLD/group").write_text(f"{GROUP}\n")
        done = self.run_script("--delete", "--run", "OLD", "--dry-run")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout.splitlines(), GONE)
        self.assertEqual(self.calls(), [])

    def test_every_az_command_names_the_subscription_and_the_run_group(self):
        for line in DRY_RUN:
            self.assertEqual(len(re.findall(r"(?:^|[\s(])az ", line)), line.count("--subscription griot-dev"), line)
            self.assertEqual(line.count("--subscription"), line.count("--subscription griot-dev"), line)
            for group in re.findall(r"(?:\s-g|--resource-group) (\S+)", line):
                self.assertEqual(group, GROUP, line)
            for group in re.findall(r"az group (?:create|delete|exists) -n (\S+)", line):
                self.assertEqual(group, GROUP, line)
        self.assertNotIn("account set", "\n".join(DRY_RUN))
        # No value is given to the run command in the clear: the key by its file alone.
        command = next(line for line in DRY_RUN if " run-command create " in line)
        self.assertNotIn("--parameters", command.replace("--protected-parameters", ""))
        self.assertIn(" --protected-parameters BENCH_KEY=@RUNDIR/account.key ", command)

    def test_the_guard_refuses_before_anything_runs(self):
        self.stubs(az=NEVER, perl=NEVER, tar=NEVER, **{"ssh-keygen": NEVER})
        refusals = [
            (["--subscription", "griot-prod"], "subscription 'griot-prod' is not griot-dev"),
            (["--group", "moruna-node-x"], "is not named moruna-nyc-*"),
            (["--group", "rg-x"], "is not named moruna-nyc-*"),
            (["--group", "moruna-nyc-"], "lowercase letters, digits and inner dashes"),
            (["--group", "moruna-nyc-ABC"], "lowercase letters"),
            (["--group", "moruna-nyc--x"], "inner dashes"),
            (["--group", "moruna-nyc-x;rm"], "lowercase letters"),
            (["--group", "moruna-nyc-" + "x" * 60], "longer than 64"),
        ]
        for extra, reason in refusals:
            for dry in ([], ["--dry-run"]):
                arguments = self.arguments(*dry)
                for flag, value in zip(extra[::2], extra[1::2]):
                    if flag in arguments:
                        arguments[arguments.index(flag) + 1] = value
                    else:
                        arguments += [flag, value]
                done = self.run_script(*arguments)
                self.assertEqual(done.returncode, 2, (extra, dry, done.stderr))
                self.assertIn("refused", done.stderr)
                self.assertIn(reason, done.stderr)
                self.assertEqual(done.stdout, "")
        (self.root / "TASKS/gate-audit").unlink()
        done = self.run_script(*self.arguments("--dry-run"))
        self.assertEqual(done.returncode, 2)
        self.assertIn("no gate task at TASKS/gate-audit", done.stderr)
        (self.root / "TASKS/gate-audit").write_bytes(b"task")
        for arguments in (["--run", "RUNDIR"], ["--tasks", "TASKS"], ["--what"]):
            self.assertEqual(self.run_script(*arguments).returncode, 2, arguments)
        # --delete deletes only a group this script made: the one RUN/group names.
        done = self.run_script("--delete", "--run", "NOPE")
        self.assertEqual(done.returncode, 2)
        self.assertIn("no group named in NOPE/group", done.stderr)
        (self.root / "OLD").mkdir()
        (self.root / "OLD/group").write_text("moruna-node-x\n")
        done = self.run_script("--delete", "--run", "OLD")
        self.assertEqual(done.returncode, 2)
        self.assertIn("is not named moruna-nyc-*", done.stderr)
        (self.root / "OLD/group").write_text(f"{GROUP}\n")
        for more in (["--group", GROUP], ["--tasks", "TASKS"]):
            done = self.run_script("--delete", "--run", "OLD", *more)
            self.assertEqual(done.returncode, 2, more)
            self.assertIn("--delete takes the group RUN/group names", done.stderr)
        self.assertEqual(self.calls(), [], "a refusal runs nothing")
        (self.root / "RUNDIR").mkdir()
        done = self.run_script(*self.arguments())
        self.assertEqual(done.returncode, 2)
        self.assertIn("must be new", done.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_group_that_exists_already_is_neither_used_nor_deleted(self):
        self.live()
        done = self.run_script(*self.arguments(), extra={"STUB_GROUP_EXISTS": "1"})
        self.assertEqual(done.returncode, 2, done.stderr)
        self.assertIn(f"group {GROUP} exists already", done.stderr)
        self.assertEqual(self.az(), [f"az group exists -n {GROUP} --subscription griot-dev"])

    def test_without_a_group_the_run_gets_one_of_its_own(self):
        self.stubs(az=NEVER, perl=NEVER, tar=NEVER, **{"ssh-keygen": NEVER})
        done = self.run_script("--run", "RUNDIR", "--tasks", "TASKS", "--dry-run")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(len(set(re.findall(r"moruna-nyc-[0-9]{12}\b", done.stdout))), 1)
        self.assertEqual(len(set(re.findall(r"\bmrnnyc[0-9]{12}\b", done.stdout))), 1)

    def test_a_run_fills_both_containers_deletes_the_machine_and_writes_the_sases(self):
        self.live()
        done = self.run_script(*self.arguments())
        self.assertEqual(done.returncode, 0, done.stderr)
        run = self.root / "RUNDIR"
        self.assertEqual(stat.S_IMODE(run.stat().st_mode), 0o700)
        self.assertEqual((run / "group").read_text(), f"{GROUP}\n")
        az = self.az()
        self.assertEqual([" ".join(call.split()[1:3]) for call in az], [
            "group exists", "group create", "storage account", "storage account", "storage container",
            "storage container", "storage blob", "storage blob", "storage blob", "storage blob", "network nsg",
            "vm create", "vm run-command", "vm run-command", "storage blob", "vm delete", "network public-ip",
            "network vnet", "network nsg", "resource list", "storage container", "storage container"])
        # The key: in the data-plane commands' environment, and in the run command's protected
        # parameter, read from its file; on no command line, in no script, said nowhere.
        for call in self.calls():
            if call.startswith("az storage container") or call.startswith("az storage blob"):
                self.assertTrue(call.endswith(f" |key={KEY}"), call)
            self.assertNotIn(KEY, call.split(" |key=")[0])
        self.assertEqual((self.state / "protected").read_text(), f"BENCH_KEY={KEY}")
        script = (self.state / "script.sh").read_text()
        self.assertNotIn(KEY, script)
        self.assertEqual(script, (run / "data.sh").read_text().rstrip("\n"))
        lines = script.splitlines()
        self.assertEqual(lines[0], "#!/bin/bash")
        self.assertEqual(lines[2], f"export BENCH_ACCOUNT={ACCOUNT} BENCH_CONTAINER=data")
        self.assertEqual("\n".join(lines[3:]), (TOOLS / "nyc-data.sh").read_text().rstrip("\n"))
        # The code it carries: the five files, as they are here.
        code = re.fullmatch(r"CODE='([A-Za-z0-9+/=]+)'", lines[1]).group(1)
        with tarfile.open(fileobj=io.BytesIO(base64.b64decode(code)), mode="r:gz") as tar:
            self.assertEqual(tar.getnames(), CODE)
            for name in CODE:
                self.assertEqual(tar.extractfile(name).read(), (REPO / name).read_bytes(), name)
        # The machine is deleted once the data is checked, before the SASes are made.
        delete = az.index(f"az vm delete -n nyc-data -g {GROUP} --yes --subscription griot-dev -o none")
        self.assertLess(az.index(next(c for c in az if c.startswith("az storage blob list "))), delete)
        self.assertLess(delete, az.index(next(c for c in az if " generate-sas -n data " in c)))
        self.assertIn(" --permissions racwl ", next(c for c in az if " generate-sas -n data " in c))
        self.assertIn(" --permissions r ", next(c for c in az if " generate-sas -n functions " in c))
        self.assertFalse(any(c.startswith("az group delete") for c in az), "a run that succeeds keeps its group")
        # RUN/blob: mode 600, the account, the containers and both SASes.
        blob = run / "blob"
        self.assertEqual(stat.S_IMODE(blob.stat().st_mode), 0o600)
        expires = (run / "expires").read_text().strip()
        self.assertRegex(expires, r"^\d{4}-\d\d-\d\dT\d\d:\d\dZ$")
        self.assertEqual(blob.read_text(), f"account={ACCOUNT}\ncontainer=data\nfunctions=functions\n"
                                           f"data_sas={DATA_SAS}\nfunctions_sas={FUNCTIONS_SAS}\nexpires={expires}\n")
        for name in ("account.key", "data.sas", "functions.sas"):
            self.assertEqual(stat.S_IMODE((run / name).stat().st_mode), 0o600, name)
        self.assertEqual(json.loads((run / "data.json").read_text())["ok"], True)
        # What it says: where, and until when; never a SAS or the key.
        self.assertEqual(done.stdout.splitlines()[-1],
                         f"nyc-blob: account {ACCOUNT}, container data (nyc/, the twelve months) and functions "
                         f"(the four tasks), SASes until {expires} in RUNDIR/blob (mode 600); delete with: "
                         "nyc-blob.sh --delete --run RUNDIR")
        for said in (done.stdout, done.stderr):
            self.assertNotIn("sig=", said)
            self.assertNotIn(KEY, said)

    def test_data_that_is_not_right_deletes_the_group_and_makes_no_sas(self):
        for extra in ({"STUB_DATA": "bad"}, {"STUB_EXIT": "1"}, {"STUB_FEW": "1"}):
            self.log.write_text("")
            run = self.root / "RUNDIR"
            if run.exists():
                for path in sorted(run.iterdir()):
                    path.unlink()
                run.rmdir()
            self.live()
            done = self.run_script(*self.arguments(), extra=extra)
            self.assertEqual(done.returncode, 1, (extra, done.stderr))
            az = self.az()
            self.assertFalse(any(" generate-sas " in call for call in az), extra)
            self.assertFalse((run / "blob").exists())
            self.assertEqual(az[-3:], [GONE[0], f"az group exists -n {GROUP} --subscription griot-dev",
                                       "az group list --subscription griot-dev --query "
                                       "[?starts_with(name, 'moruna-nyc-')].name -o tsv"])

    def test_a_failure_after_the_group_is_made_deletes_it(self):
        self.live()
        done = self.run_script(*self.arguments(), extra={"STUB_FAIL": "1"})
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertIn("the account was refused", done.stderr)
        az = self.az()
        self.assertFalse(any(call.startswith("az vm ") for call in az))
        self.assertEqual(az[3], GONE[0])

    def test_delete_deletes_the_group_the_run_made_and_shows_it_gone(self):
        self.live()
        self.assertEqual(self.run_script(*self.arguments()).returncode, 0)
        self.log.write_text("")
        done = self.run_script("--delete", "--run", "RUNDIR")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.az(), [GONE[0], f"az group exists -n {GROUP} --subscription griot-dev",
                                     "az group list --subscription griot-dev --query "
                                     "[?starts_with(name, 'moruna-nyc-')].name -o tsv"])
        self.assertIn(f"nyc-blob: group {GROUP} is gone", done.stdout)


class NycData(unittest.TestCase):
    """nyc-data.sh as the machine runs it, its script made by nyc-blob.sh's own commands, with a
    stand-in for the `bash` it hands data-linux.sh to."""

    def test_the_machine_unpacks_the_code_and_gives_data_linux_the_plan_s_pyarrow_and_spec(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run = root / "run"
            run.mkdir()
            out = subprocess.run(["bash", "-c", DRY_RUN[13].replace("RUNDIR", str(run))], capture_output=True,
                                 text=True, check=False)
            self.assertEqual(out.returncode, 0, out.stderr)
            out = subprocess.run(["bash", "-c", DRY_RUN[14].replace("RUNDIR", str(run))], capture_output=True,
                                 text=True, check=False)
            self.assertEqual(out.returncode, 0, out.stderr)
            stand_in = root / "bin"
            stand_in.mkdir()
            (stand_in / "bash").write_text("#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$SAID\"\n")
            (stand_in / "bash").chmod(0o755)
            (stand_in / "python3").write_text(f"#!/bin/sh\nexec {sys.executable} \"$@\"\n")
            (stand_in / "python3").chmod(0o755)
            said = root / "said"
            env = {"PATH": f"{stand_in}:/usr/bin:/bin", "SAID": str(said), "BENCH_KEY": "k",
                   "NYC_DATA_ROOT": str(root / "opt")}
            done = subprocess.run(["/bin/bash", str(run / "data.sh")], env=env, capture_output=True, text=True,
                                  timeout=60, check=False)
            self.assertEqual(done.returncode, 0, done.stderr)
            argv = said.read_text().splitlines()
            plan = json.loads((REPO / "bench/node/plans/nyc-2024.json").read_text())["dataset"]
            self.assertEqual(argv[0], str(root / "opt/bench/node/engines/data-linux.sh"))
            self.assertEqual(argv[1], plan["pyarrow"])
            self.assertEqual(json.loads(argv[2]), {"source_url": plan["source_url"], "sha256": plan["sha256"],
                                                   "prefix": "nyc/",
                                                   "normalize": str(root / "opt/bench/vm/normalize.py")})
            for name in CODE:
                self.assertEqual((root / "opt" / name).read_bytes(), (REPO / name).read_bytes(), name)
            # Without the key (its protected parameter), nothing runs.
            env.pop("BENCH_KEY")
            said.unlink()
            done = subprocess.run(["/bin/bash", str(run / "data.sh")], env=env, capture_output=True, text=True,
                                  timeout=60, check=False)
            self.assertNotEqual(done.returncode, 0)
            self.assertIn("the account key, a protected parameter", done.stderr)
            self.assertFalse(said.exists())


if __name__ == "__main__":
    unittest.main()
