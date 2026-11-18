#!/usr/bin/env python3
"""azure-up.sh (work item 1.13), against stand-ins for `az`, `openssl`, `curl` and `nc`.

Nothing here reaches Azure. The stand-ins record every call and answer as Azure and the node
would; one that is never to be called fails the test if it is.

- The dry run prints every command, exactly the list below, and runs none.
- `kernel/node/AZURE.md` holds every command the script runs, and the script runs every command
  it holds but two: the one that shows the login, and the script's own line.
- The guard refuses any subscription but griot-dev, any group not named `moruna-node-*`, an
  address that is not IPv4, and a group that already exists, before anything runs.
- A run that goes well checks the node and deletes the group; one that fails part-way deletes it
  too.
- `--data-disk` makes an empty 4 GiB StandardSSD disk the machine takes at LUN 0, and prints the
  console's storvsc lines at the end; `--job` runs a rows-to-count job through the API, follows
  its events to its exit, and checks exit 0, the sink's 10,000,000 rows, and prints the wall time.

Run: python3 -I kernel/node/tools/test_azure_up.py
"""
import base64
import json
import os
from pathlib import Path
import datetime
import re
import stat
import subprocess
import sys
import tempfile
import time
import unittest

TOOLS = Path(__file__).resolve().parent
SCRIPT = TOOLS / "azure-up.sh"
RECIPE = TOOLS.parent / "AZURE.md"
GROUP = "moruna-node-name"
INSTANCE = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0"
PIN = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"

# Every command of a dry run, in order, with RUNDIR, OUTDIR, GROUP, the account and the address
# as the tests give them.
DRY_RUN = """\
mkdir -m 700 RUNDIR
openssl genpkey -algorithm ed25519 -out RUNDIR/token-key.pem
openssl pkey -in RUNDIR/token-key.pem -pubout -outform DER | tail -c 32 | base64 > RUNDIR/token-key.pub
printf '{"moruna":1,"api":{"token_keys":["%s"]}}\\n' "$(cat RUNDIR/token-key.pub)" > RUNDIR/node-config.json
[ "$(az group exists -n moruna-node-name --subscription griot-dev)" = false ]
az group create -n moruna-node-name -l centralindia --tags purpose=moruna-node item=1.13 --subscription griot-dev -o none
az storage account create -n mrnnodename -g moruna-node-name -l centralindia --sku Standard_LRS --kind StorageV2 --min-tls-version TLS1_2 --allow-blob-public-access false --subscription griot-dev -o none
(umask 077 && az storage account keys list -n mrnnodename -g moruna-node-name --query '[0].value' -o tsv --subscription griot-dev > RUNDIR/account.key)
AZURE_STORAGE_KEY="$(cat RUNDIR/account.key)" az storage container create -n vhds --account-name mrnnodename --auth-mode key --subscription griot-dev -o none
AZURE_STORAGE_KEY="$(cat RUNDIR/account.key)" az storage blob upload -c vhds -n moruna-node.vhd -f OUTDIR/moruna-node.vhd --type page --account-name mrnnodename --auth-mode key --subscription griot-dev -o none
AZURE_STORAGE_KEY="$(cat RUNDIR/account.key)" az storage blob url -c vhds -n moruna-node.vhd --account-name mrnnodename --auth-mode key --subscription griot-dev -o tsv > RUNDIR/vhd.url
az storage account show -n mrnnodename -g moruna-node-name --query id -o tsv --subscription griot-dev > RUNDIR/account.id
az disk create -n moruna-node-os -g moruna-node-name -l centralindia --zone 3 --source "$(cat RUNDIR/vhd.url)" --source-storage-account-id "$(cat RUNDIR/account.id)" --hyper-v-generation V2 --os-type Linux --sku Standard_LRS --subscription griot-dev -o none
az network public-ip create -n moruna-node-ip -g moruna-node-name -l centralindia --zone 3 --sku Standard --allocation-method Static --version IPv4 --subscription griot-dev -o none
az network public-ip show -n moruna-node-ip -g moruna-node-name --query ipAddress -o tsv --subscription griot-dev > RUNDIR/address
az network nsg create -n moruna-node-nsg -g moruna-node-name -l centralindia --subscription griot-dev -o none
az network nsg rule create --nsg-name moruna-node-nsg -n https-from-operator -g moruna-node-name --priority 100 --direction Inbound --access Allow --protocol Tcp --source-address-prefixes 192.0.2.1/32 --source-port-ranges '*' --destination-address-prefixes '*' --destination-port-ranges 443 --subscription griot-dev -o none
az network nsg rule list --nsg-name moruna-node-nsg -g moruna-node-name --include-default -o table --subscription griot-dev
az vm create -n moruna-node -g moruna-node-name -l centralindia --zone 3 --size Standard_D2s_v4 --attach-os-disk moruna-node-os --os-type linux --security-type Standard --accelerated-networking false --public-ip-address moruna-node-ip --nsg moruna-node-nsg --boot-diagnostics-storage mrnnodename --user-data RUNDIR/node-config.json --no-wait --subscription griot-dev -o none && date -u +%s > RUNDIR/created.at
deadline=$(( $(date -u +%s) + 600 )); while [ "$(date -u +%s)" -lt "$deadline" ]; do [ "$(curl -sk --max-time 5 -o /dev/null -w '%{http_code}' "https://$(cat RUNDIR/address)/v1/health")" = 200 ] && { date -u +%s > RUNDIR/ready.at; break; }; sleep 2; done; [ -s RUNDIR/ready.at ]
for _ in $(seq 30); do az vm boot-diagnostics get-boot-log -n moruna-node -g moruna-node-name --subscription griot-dev > RUNDIR/console.log; tr -d '\\r' < RUNDIR/console.log | grep -a -q -e 'moruna: ready' -e 'moruna: fatal' && break; sleep 10; done; tr -d '\\r' < RUNDIR/console.log | grep -a 'moruna: '
tr -d '\\r' < RUNDIR/console.log | grep -a -q 'moruna: ready api='
tr -d '\\r' < RUNDIR/console.log | grep -a -o 'moruna: cloud azure instance [^ ]*' | head -1 | cut -d' ' -f5 > RUNDIR/instance && [ -s RUNDIR/instance ]
nc -vz -w 5 "$(cat RUNDIR/address)" 443
! nc -vz -w 5 "$(cat RUNDIR/address)" 22
az vm get-instance-view -n moruna-node -g moruna-node-name --query "instanceView.statuses[?starts_with(code, 'PowerState/')].displayStatus" -o tsv --subscription griot-dev | grep -q 'VM running'
tr -d '\\r' < RUNDIR/console.log | grep -a -o 'moruna: tls sha256=[0-9a-f]*' | head -1 | cut -d= -f2 > RUNDIR/console.pin
openssl s_client -connect "$(cat RUNDIR/address)":443 < /dev/null 2> /dev/null | openssl x509 -outform DER | openssl dgst -sha256 -r | cut -d' ' -f1 > RUNDIR/served.pin && cmp RUNDIR/console.pin RUNDIR/served.pin
printf '%s' '{"alg":"EdDSA","typ":"JWT"}' | openssl base64 -A | tr '/+' '_-' | tr -d '=' > RUNDIR/token.header
printf '{"aud":"%s","exp":%s,"scope":["observe"]}' "$(cat RUNDIR/instance)" "$(( $(date -u +%s) + 600 ))" | openssl base64 -A | tr '/+' '_-' | tr -d '=' > RUNDIR/token.claims
printf '%s.%s' "$(cat RUNDIR/token.header)" "$(cat RUNDIR/token.claims)" > RUNDIR/token.signed
openssl pkeyutl -sign -inkey RUNDIR/token-key.pem -rawin -in RUNDIR/token.signed | openssl base64 -A | tr '/+' '_-' | tr -d '=' > RUNDIR/token.signature
printf '%s.%s\\n' "$(cat RUNDIR/token.signed)" "$(cat RUNDIR/token.signature)" > RUNDIR/token
curl -sS -k --max-time 10 -H "Authorization: Bearer $(cat RUNDIR/token)" "https://$(cat RUNDIR/address)/v1/node" > RUNDIR/node.json && grep -q '"cloud":"azure"' RUNDIR/node.json && cat RUNDIR/node.json
python3 -I REPO/kernel/node/tools/api-checks.py --url "https://$(cat RUNDIR/address):443" --pin "$(cat RUNDIR/console.pin)" --token-key RUNDIR/token-key.pem --instance "$(cat RUNDIR/instance)" --openssl openssl --results RUNDIR/api-checks.json --cloud azure
az vm boot-diagnostics get-boot-log -n moruna-node -g moruna-node-name --subscription griot-dev > RUNDIR/console.log
az group delete -n moruna-node-name --yes --subscription griot-dev
az group exists -n moruna-node-name --subscription griot-dev
az group list --subscription griot-dev --query "[?starts_with(name, 'moruna-node-')].name" -o tsv
az network watcher list --subscription griot-dev -o table
""".replace("REPO/", f"{TOOLS.parents[2]}/").splitlines()

DATA_DISK = ("az disk create -n moruna-node-data -g moruna-node-name -l centralindia --zone 3 --size-gb 4 "
             "--sku StandardSSD_LRS --subscription griot-dev -o none")
DATA_DISK_LINES = "tr -d '\\r' < RUNDIR/console.log | grep -a -e 'storvsc: ' -e 'storvsc-check: '"
AUTH = 'Authorization: Bearer $(cat RUNDIR/job-token)'
JOB = [
    "printf '%s' '{\"v1\":{\"moruna_spec\":1,\"source\":{\"kind\":\"rows\",\"rows\":10000000},"
    "\"sink\":{\"kind\":\"count\"}}}' > RUNDIR/job.json",
    "printf '{\"aud\":\"%s\",\"exp\":%s,\"scope\":[\"submit\"]}' \"$(cat RUNDIR/instance)\" "
    "\"$(( $(date -u +%s) + 1800 ))\" | openssl base64 -A | tr '/+' '_-' | tr -d '=' > RUNDIR/job-token.claims",
    "printf '%s.%s' \"$(cat RUNDIR/token.header)\" \"$(cat RUNDIR/job-token.claims)\" > RUNDIR/job-token.signed",
    "openssl pkeyutl -sign -inkey RUNDIR/token-key.pem -rawin -in RUNDIR/job-token.signed | openssl base64 -A "
    "| tr '/+' '_-' | tr -d '=' > RUNDIR/job-token.signature",
    "printf '%s.%s\\n' \"$(cat RUNDIR/job-token.signed)\" \"$(cat RUNDIR/job-token.signature)\" > RUNDIR/job-token",
    f"curl -sS -k --max-time 60 -H \"{AUTH}\" -H 'Content-Type: application/json' --data-binary @RUNDIR/job.json "
    "\"https://$(cat RUNDIR/address)/v1/jobs/validate\" > RUNDIR/job-validate.json && cat RUNDIR/job-validate.json "
    "&& grep -q '\"ok\":true' RUNDIR/job-validate.json",
    f"curl -sS -k --max-time 60 -o RUNDIR/job-submit.json -w '%{{http_code}}' -H \"{AUTH}\" "
    "-H 'Content-Type: application/json' --data-binary @RUNDIR/job.json \"https://$(cat RUNDIR/address)/v1/jobs\" "
    "> RUNDIR/job-submit.code && [ \"$(cat RUNDIR/job-submit.code)\" = 201 ] && grep -o '\"id\":\"[^\"]*\"' "
    "RUNDIR/job-submit.json | head -1 | cut -d'\"' -f4 > RUNDIR/job.id && [ -s RUNDIR/job.id ]",
    f"curl -sS -k -N --max-time 600 -H \"{AUTH}\" \"https://$(cat RUNDIR/address)/v1/jobs/$(cat RUNDIR/job.id)/events\" "
    "> RUNDIR/job-events.txt; grep -a '^data: ' RUNDIR/job-events.txt | grep -a '\"type\":\"exit\"' | tail -1 "
    "| cut -c7- > RUNDIR/job-exit.json && cat RUNDIR/job-exit.json && grep -q '\"code\":0[,}]' RUNDIR/job-exit.json",
    f"curl -sS -k --max-time 60 -H \"{AUTH}\" \"https://$(cat RUNDIR/address)/v1/jobs/$(cat RUNDIR/job.id)/report\" "
    "> RUNDIR/job-report.json && python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))[\"sinks\"][0][\"rows\"])' "
    "RUNDIR/job-report.json > RUNDIR/job.rows && [ \"$(cat RUNDIR/job.rows)\" = 10000000 ]",
    "python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))[\"wall_ms\"])' RUNDIR/job-report.json > RUNDIR/job.wall "
    "&& [ -s RUNDIR/job.wall ] && printf 'job: %s rows counted, wall_ms %s\\n' \"$(cat RUNDIR/job.rows)\" "
    "\"$(cat RUNDIR/job.wall)\"",
]


# The repository whose bench/node/proof.py --proof runs, as the script names it.
REPO = str(TOOLS.parents[2])
PROOF_ARGS = ("--proof", "--tasks", "TASKS", "--nyc", "NYCDIR", "--gate-canon", "GATECANON")
# With --proof: the function key and node config with it, in place of the base node config.
PROOF_KEYS = [
    "openssl genpkey -algorithm ed25519 -out RUNDIR/function-key.pem",
    "openssl pkey -in RUNDIR/function-key.pem -pubout -outform DER | tail -c 32 | base64 > RUNDIR/function-key.pub",
    "printf '{\"moruna\":1,\"api\":{\"token_keys\":[\"%s\"]},\"functions\":{\"keys\":[\"%s\"]}}\\n' "
    "\"$(cat RUNDIR/token-key.pub)\" \"$(cat RUNDIR/function-key.pub)\" > RUNDIR/node-config.json",
]
KEYED = 'AZURE_STORAGE_KEY="$(cat RUNDIR/account.key)" az storage '
ON_ACCOUNT = "--account-name mrnnodename --auth-mode key --subscription griot-dev"
# With --proof, after the account's ID: the containers, the data, the tasks and the SAS.
PROOF_STORAGE = [
    f"{KEYED}container create -n data {ON_ACCOUNT} -o none",
    f"{KEYED}container create -n functions {ON_ACCOUNT} -o none",
    f"{KEYED}blob upload-batch -d data --destination-path nyc -s NYCDIR/norm --pattern "
    f"'yellow_tripdata_2024-*.parquet' {ON_ACCOUNT} -o none",
] + [f"{KEYED}blob upload -c functions -n gate-{j} -f TASKS/gate-{j} {ON_ACCOUNT} -o none"
     for j in ("score", "occupancy", "enrich", "audit")] + [
    f"(umask 077 && {KEYED}container generate-sas -n functions --permissions r --expiry \"$(python3 -I -c "
    "'import datetime as d; print((d.datetime.now(d.timezone.utc) + d.timedelta(hours=6))"
    ".strftime(\"%Y-%m-%dT%H:%MZ\"))')\" --https-only " + ON_ACCOUNT + " -o tsv > RUNDIR/functions.sas)",
]
# With --proof, after the node's checks (and --job's): the proof itself.
PROOF = (
    "BENCH_KEY=\"$(cat RUNDIR/account.key)\" FUNCTIONS_SAS=\"$(cat RUNDIR/functions.sas)\" "
    f"MORUNA_BENCH_OPENSSL=openssl python3 -I {REPO}/bench/node/proof.py "
    "--url \"https://$(cat RUNDIR/address):443\" --pin \"$(cat RUNDIR/console.pin)\" "
    "--token-key RUNDIR/token-key.pem --instance \"$(cat RUNDIR/instance)\" --function-key RUNDIR/function-key.pem "
    "--functions TASKS --function-base https://mrnnodename.blob.core.windows.net/functions --data blob "
    "--blob-account mrnnodename --blob-container data --periods 2024-01,2024 --references NYCDIR "
    "--gate-canon GATECANON --results RUNDIR/proof --work RUNDIR/proof-work")


def with_options(data_disk: bool, job: bool, proof: bool = False):
    """The dry run's commands with `--data-disk`, `--job` or `--proof`: the base list with theirs
    put in."""
    lines = []
    for line in DRY_RUN:
        if data_disk and line.startswith("az vm create "):
            line = line.replace("--user-data RUNDIR/node-config.json --no-wait",
                                "--user-data RUNDIR/node-config.json --attach-data-disks moruna-node-data --no-wait")
        if proof and line.startswith("az vm create "):
            line = line.replace("--size Standard_D2s_v4 ", "--size Standard_D4s_v4 ")
        if proof and line.startswith("printf '{\"moruna\":1,"):
            lines += PROOF_KEYS
            continue
        lines.append(line)
        if data_disk and line.startswith("az disk create -n moruna-node-os "):
            lines.append(DATA_DISK)
        if proof and line.startswith("az storage account show "):
            lines += PROOF_STORAGE
        if job and "/v1/node\"" in line:
            lines += JOB
        if proof and "/v1/node\"" in line:
            lines.append(PROOF)
        if data_disk and line.startswith("az vm boot-diagnostics get-boot-log ") and "for _ in" not in line:
            lines.append(DATA_DISK_LINES)
    return lines


DRY_RUN_OPTIONS = with_options(True, True)
DRY_RUN_PROOF = with_options(False, True, proof=True)

# The stand-ins. Each records its call in STUB_LOG, the storage key it was given beside `az`'s.
AZ = r"""#!/bin/sh
{ printf 'az'; for a in "$@"; do printf ' %s' "$a"; done; printf ' |key=%s\n' "${AZURE_STORAGE_KEY:-}"; } >> "$STUB_LOG"
case " $* " in
  *" group exists "*) if [ -n "${STUB_GROUP_EXISTS:-}" ]; then echo true; else echo false; fi ;;
  *" storage account create "*) if [ -n "${STUB_FAIL:-}" ]; then echo "the account was refused" >&2; exit 1; fi ;;
  *" keys list "*) echo the-account-key ;;
  *" blob url "*) echo https://account.blob.core.windows.net/vhds/moruna-node.vhd ;;
  *" account show "*) echo /subscriptions/s/resourceGroups/g/providers/Microsoft.Storage/storageAccounts/a ;;
  *" public-ip show "*) echo 203.0.113.7 ;;
  *" get-boot-log "*) printf 'BdsDxe: loading\r\nmoruna: core 0.7.0 cpus=2 memory=8589934592\r\nmoruna: services started\r\nmoruna: network wan 10.0.0.4/24 gateway 10.0.0.1\r\nmoruna: cloud azure instance %s\r\nmoruna: tls sha256=%s\r\nmoruna: ready api=10.0.0.4:443 boot_ms=2140\r\n' "$STUB_INSTANCE" "$STUB_PIN" ;;
  *" get-instance-view "*) echo "VM running" ;;
  *" generate-sas "*) echo "${STUB_SAS:-}" ;;
esac
exit 0
"""
# A container SAS as `az` prints one, every value fake.
SAS = "se=2026-11-18T18%3A00Z&sp=r&spr=https&sv=2022-11-02&sr=c&sig=FAKEfakeFAKEfake%2Bfake%2Ffake%3D"
# A stand-in for python3: proof.py and api-checks.py are recorded with the secrets their
# environment gave them, and write their results; anything else is the real Python (the SAS's
# expiry).
PYTHON = f"""#!/bin/sh
case "$2" in
  */bench/node/proof.py | */kernel/node/tools/api-checks.py)
    {{ printf 'python3'; for a in "$@"; do printf ' %s' "$a"; done
       printf ' |key=%s|sas=%s|openssl=%s\\n' "${{BENCH_KEY:-}}" "${{FUNCTIONS_SAS:-}}" "${{MORUNA_BENCH_OPENSSL:-}}"; }} >> "$STUB_LOG"
    results=""; previous=""
    for a in "$@"; do [ "$previous" = --results ] && results="$a"; previous="$a"; done
    case "$2" in
      */proof.py)
        mkdir -p "$results" && printf '{{"ok":%s}}' "${{STUB_PROOF_OK:-true}}" > "$results/proof.json"
        echo "proof: score-2024-01 pass"
        exit "${{STUB_PROOF_EXIT:-0}}" ;;
      *)
        printf '{{"ok":%s}}' "${{STUB_CHECKS_OK:-true}}" > "$results"
        echo "api-checks: all 9 checks pass"
        exit "${{STUB_CHECKS_EXIT:-0}}" ;;
    esac ;;
esac
exec {sys.executable} "$@"
"""
OPENSSL = r"""#!/bin/sh
echo "openssl $*" >> "$STUB_LOG"
case "$1" in
  genpkey) while [ "$#" -gt 0 ]; do [ "$1" = -out ] && echo private > "$2"; shift; done ;;
  pkey) printf '%044d' 0 ;;
  s_client) echo certificate ;;
  x509) cat ;;
  dgst) cat > /dev/null; echo "$STUB_PIN *stdin" ;;
  base64) base64 | tr -d '\n' ;;
  pkeyutl) printf signature ;;
esac
"""
CURL = r"""#!/bin/sh
echo "curl $*" >> "$STUB_LOG"
out=""
previous=""
for argument in "$@"; do [ "$previous" = -o ] && out="$argument"; previous="$argument"; done
case "$*" in
  */v1/health*) printf 200 ;;
  */v1/jobs/validate*) printf '{"ok":true}' ;;
  */events*) printf 'id: 1\nevent: event\ndata: {"type":"event","kind":"attempt_started"}\n\nevent: heartbeat\ndata: {"type":"heartbeat","rows_out":5000000}\n\nevent: report\ndata: {"type":"report","report":{"sinks":[{"sink":0,"rows":%s}]}}\n\nevent: exit\ndata: {"type":"exit","code":%s,"diagnostic":null}\n\n' "${STUB_ROWS:-10000000}" "${STUB_EXIT:-0}" ;;
  */report*) printf '{"run_id":"0123456789abcdef0123456789abcdef","wall_ms":2.5,"stages":[{"name":"count","rows":3}],"sinks":[{"sink":0,"rows":%s,"bytes":0,"files":[],"snapshot_id":null,"version":null}]}' "${STUB_ROWS:-10000000}" ;;
  */v1/jobs*) printf '{"id":"0123456789abcdef0123456789abcdef"}' > "$out"; printf 201 ;;
  */v1/node*) printf '{"version":"0.7.0","cloud":"azure"}' ;;
esac
"""
NC = r"""#!/bin/sh
echo "nc $*" >> "$STUB_LOG"
case "$*" in *" 443") exit 0 ;; *) exit 1 ;; esac
"""
NEVER = """#!/bin/sh
echo "$(basename "$0") $* (called in a dry run)" >> "$STUB_LOG"
exit 99
"""


def commands(markdown: str):
    """The commands in `markdown`'s fenced blocks: each line, continued lines joined, its spaces
    collapsed."""
    found = []
    for block in re.findall(r"^ *```\n(.*?)^ *```", markdown, re.S | re.M):
        joined = re.sub(r"\\\n\s*", " ", block)
        found += [re.sub(r" +", " ", line.strip()) for line in joined.splitlines() if line.strip()]
    return found


class AzureUp(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        (self.root / "OUTDIR").mkdir()
        (self.root / "OUTDIR/moruna-node.vhd").write_bytes(b"\0" * 1024)
        # The proof's inputs: the four tasks, the twelve months and the eight references, and
        # gate-canon.
        (self.root / "TASKS").mkdir()
        (self.root / "NYCDIR/norm").mkdir(parents=True)
        for job in ("score", "occupancy", "enrich", "audit"):
            (self.root / f"TASKS/gate-{job}").write_bytes(b"\x7fELF a task")
            for period in ("2024-01", "2024"):
                (self.root / f"NYCDIR/reference-{job}-{period}.json").write_text("{}")
        for month in range(1, 13):
            (self.root / f"NYCDIR/norm/yellow_tripdata_2024-{month:02d}.parquet").write_bytes(b"PAR1")
        (self.root / "GATECANON").write_text("#!/bin/sh\n")
        (self.root / "GATECANON").chmod(0o755)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "calls"
        self.log.touch()

    def stubs(self, **scripts):
        # python3 is the stand-in unless a test names another: no test reaches a node.
        scripts.setdefault("python3", PYTHON)
        for name, text in scripts.items():
            path = self.bin / name
            path.write_text(text)
            path.chmod(0o755)

    def calls(self):
        return self.log.read_text().splitlines()

    def run_script(self, *arguments, extra=None):
        env = {"PATH": f"{self.bin}:/usr/bin:/bin", "STUB_LOG": str(self.log), "MORUNA_OPENSSL": "openssl",
               "STUB_PIN": PIN, "STUB_INSTANCE": INSTANCE, **(extra or {})}
        return subprocess.run(["bash", str(SCRIPT), *arguments], cwd=self.root, capture_output=True, text=True,
                              env=env, timeout=120, check=False)

    def arguments(self, *more, group=GROUP, run="RUNDIR"):
        return ["--vhd", "OUTDIR/moruna-node.vhd", "--run", run, "--from", "192.0.2.1", "--group", group, *more]

    def test_a_dry_run_prints_every_command_and_runs_none(self):
        self.stubs(az=NEVER, openssl=NEVER, curl=NEVER, nc=NEVER)
        done = self.run_script(*self.arguments("--dry-run"))
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout.splitlines(), DRY_RUN)
        self.assertEqual(self.calls(), [], "a dry run runs nothing")
        self.assertFalse((self.root / "RUNDIR").exists())
        self.assertEqual(done.stderr, "")

    def test_a_dry_run_with_both_options_prints_their_commands_too(self):
        self.stubs(az=NEVER, openssl=NEVER, curl=NEVER, nc=NEVER)
        done = self.run_script(*self.arguments("--data-disk", "--job", "--dry-run"))
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout.splitlines(), DRY_RUN_OPTIONS)
        self.assertEqual(self.calls(), [])
        # Each option alone adds its own commands and nothing else.
        for option, expected in (("--data-disk", with_options(True, False)), ("--job", with_options(False, True))):
            done = self.run_script(*self.arguments(option, "--dry-run"))
            self.assertEqual(done.stdout.splitlines(), expected, option)

    def test_a_dry_run_of_the_proof_prints_its_commands_and_runs_none(self):
        self.stubs(az=NEVER, openssl=NEVER, curl=NEVER, nc=NEVER, python3=NEVER)
        done = self.run_script(*self.arguments(*PROOF_ARGS, "--job", "--dry-run"))
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout.splitlines(), DRY_RUN_PROOF)
        done = self.run_script(*self.arguments(*PROOF_ARGS, "--dry-run"))
        self.assertEqual(done.stdout.splitlines(), with_options(False, False, proof=True))
        self.assertEqual(self.calls(), [], "a dry run runs nothing")
        self.assertFalse((self.root / "RUNDIR").exists())
        # The proof's machine is a D4s_v4, and only the proof's.
        self.assertEqual(sum(" --size Standard_D4s_v4 " in line for line in DRY_RUN_PROOF), 1)
        self.assertFalse(any("Standard_D4s_v4" in line for line in DRY_RUN_OPTIONS))

    def test_every_az_command_names_the_subscription_and_the_run_group(self):
        for line in DRY_RUN_OPTIONS + DRY_RUN_PROOF:
            # As many subscriptions as `az` commands on the line, each griot-dev.
            self.assertEqual(len(re.findall(r"(?:^|[\s(])az ", line)), line.count("--subscription griot-dev"), line)
            self.assertEqual(line.count("--subscription"), line.count("--subscription griot-dev"), line)
            for group in re.findall(r"(?:\s-g|--resource-group) (\S+)", line):
                self.assertEqual(group, GROUP, line)
            for group in re.findall(r"az group (?:create|delete|exists) -n (\S+)", line):
                self.assertEqual(group, GROUP, line)
        self.assertNotIn("account set", "\n".join(DRY_RUN))

    def test_the_recipe_holds_every_command_the_script_runs_and_no_other(self):
        recipe = commands(RECIPE.read_text())
        placeholders = [(REPO, "<REPO>"), ("RUNDIR", "<RUN>"), ("OUTDIR/", "<OUT>/"), (GROUP, "<NAME>"),
                        ("mrnnodename", "<ACCOUNT>"), ("192.0.2.1", "<FROM>"), ("TASKS", "<TASKS>"),
                        ("NYCDIR", "<NYC>"), ("GATECANON", "<GATE_CANON>")]
        script = []
        for line in DRY_RUN + [line for line in DRY_RUN_OPTIONS + DRY_RUN_PROOF if line not in DRY_RUN]:
            for value, placeholder in placeholders:
                line = line.replace(value, placeholder)
            if line not in script:
                script.append(line)
        for line in script:
            self.assertIn(line, recipe)
        only_in_recipe = [line for line in recipe if line not in script]
        self.assertEqual(only_in_recipe, [
            "kernel/node/tools/azure-up.sh --vhd <OUT>/moruna-node.vhd --run <RUN> --from <FROM> --group <NAME> "
            "2>&1 | tee <RUN>.log",
            "kernel/node/tools/azure-up.sh --vhd <OUT>/moruna-node.vhd --run <RUN> --from <FROM> --group <NAME> "
            "--proof --tasks <TASKS> --nyc <NYC> --gate-canon <GATE_CANON> 2>&1 | tee <RUN>.log",
            "az account show --subscription griot-dev --query name -o tsv",
            "python3 -I -c 'import pyarrow.fs; print(pyarrow.__version__, pyarrow.fs.AzureFileSystem)'",
        ])

    def test_the_guard_refuses_before_anything_runs(self):
        self.stubs(az=NEVER, openssl=NEVER, curl=NEVER, nc=NEVER)
        refusals = [
            (["--subscription", "griot-prod"], "subscription 'griot-prod' is not griot-dev"),
            (["--subscription", "Griot-Dev"], "is not griot-dev"),
            (["--group", "my-group"], "is not named moruna-node-*"),
            (["--group", "moruna-bench-20261118"], "is not named moruna-node-*"),
            (["--group", "moruna-node-"], "lowercase letters, digits and inner dashes"),
            (["--group", "moruna-node-ABC"], "lowercase letters"),
            (["--group", "moruna-node--x"], "inner dashes"),
            (["--group", "moruna-node-x-"], "inner dashes"),
            (["--group", "moruna-node-x;rm"], "lowercase letters"),
            (["--group", "moruna-node-" + "x" * 60], "longer than 64"),
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
                self.assertEqual(done.stdout, "", "nothing is printed, not even in a dry run")
        for address in ["1.2.3", "256.1.1.1", "1.2.3.4.5", "01.2.3.4", "a.b.c.d", "1.2.3.4/32", ""]:
            arguments = self.arguments("--dry-run")
            arguments[arguments.index("--from") + 1] = address
            done = self.run_script(*arguments)
            self.assertEqual(done.returncode, 2, address)
        done = self.run_script("--vhd", "OUTDIR/none.vhd", "--run", "RUNDIR", "--from", "192.0.2.1")
        self.assertEqual(done.returncode, 2)
        self.assertIn("no VHD", done.stderr)
        self.assertEqual(self.run_script("--vhd", "OUTDIR/moruna-node.vhd").returncode, 2, "usage")
        self.assertEqual(self.calls(), [], "a refusal runs nothing")
        # A run's directory that exists already is refused too: the run's files are its own.
        (self.root / "RUNDIR").mkdir()
        done = self.run_script(*self.arguments())
        self.assertEqual(done.returncode, 2)
        self.assertIn("must be new", done.stderr)
        self.assertEqual(self.calls(), [])

    def test_the_proof_is_refused_without_its_inputs_before_anything_runs(self):
        self.stubs(az=NEVER, openssl=NEVER, curl=NEVER, nc=NEVER, python3=NEVER)
        for arguments, reason in [
            (["--proof"], "--proof needs --tasks, --nyc and --gate-canon"),
            (["--proof", "--tasks", "TASKS", "--nyc", "NYCDIR"], "--proof needs --tasks, --nyc and --gate-canon"),
            ([*PROOF_ARGS, "--data-disk"], "--proof takes no --data-disk"),
            (["--tasks", "TASKS"], "--tasks, --nyc and --gate-canon go with --proof"),
            (["--gate-canon", "GATECANON"], "--tasks, --nyc and --gate-canon go with --proof"),
        ]:
            for dry in ([], ["--dry-run"]):
                done = self.run_script(*self.arguments(*arguments, *dry))
                self.assertEqual(done.returncode, 2, (arguments, done.stderr))
                self.assertIn(reason, done.stderr)
                self.assertEqual(done.stdout, "")
        for missing, reason in [
            ("TASKS/gate-audit", "no gate task at TASKS/gate-audit"),
            ("NYCDIR/norm/yellow_tripdata_2024-07.parquet", "no input at NYCDIR/norm/yellow_tripdata_2024-07.parquet"),
            ("NYCDIR/reference-score-2024.json", "no reference at NYCDIR/reference-score-2024.json"),
            ("GATECANON", "no gate-canon program at GATECANON"),
        ]:
            path = self.root / missing
            kept = path.read_bytes()
            path.unlink()
            try:
                done = self.run_script(*self.arguments(*PROOF_ARGS, "--dry-run"))
                self.assertEqual(done.returncode, 2, missing)
                self.assertIn(reason, done.stderr)
            finally:
                path.write_bytes(kept)
        # gate-canon must be a program, not only a file.
        (self.root / "GATECANON").chmod(0o644)
        done = self.run_script(*self.arguments(*PROOF_ARGS))
        self.assertEqual(done.returncode, 2)
        self.assertIn("no gate-canon program", done.stderr)
        self.assertEqual(self.calls(), [], "a refusal runs nothing")

    def test_a_group_that_exists_already_is_neither_used_nor_deleted(self):
        self.stubs(az=AZ, openssl=OPENSSL, curl=NEVER, nc=NEVER)
        done = self.run_script(*self.arguments(), extra={"STUB_GROUP_EXISTS": "1"})
        self.assertEqual(done.returncode, 2, done.stderr)
        self.assertIn(f"group {GROUP} exists already", done.stderr)
        az = [call for call in self.calls() if call.startswith("az ")]
        self.assertEqual(len(az), 1)
        self.assertTrue(az[0].startswith(f"az group exists -n {GROUP} --subscription griot-dev"))

    def test_a_failure_after_the_group_is_made_still_deletes_it(self):
        self.stubs(az=AZ, openssl=OPENSSL, curl=NEVER, nc=NEVER)
        done = self.run_script(*self.arguments(), extra={"STUB_FAIL": "1"})
        self.assertEqual(done.returncode, 1, done.stderr)
        az = [call.split(" |key=")[0] for call in self.calls() if call.startswith("az ")]
        self.assertIn("az storage account create", az[2])
        self.assertFalse(any(call.startswith("az vm create") for call in az))
        self.assertEqual(
            az[3:],
            [f"az vm boot-diagnostics get-boot-log -n moruna-node -g {GROUP} --subscription griot-dev",
             f"az group delete -n {GROUP} --yes --subscription griot-dev",
             f"az group exists -n {GROUP} --subscription griot-dev",
             "az group list --subscription griot-dev --query [?starts_with(name, 'moruna-node-')].name -o tsv",
             "az network watcher list --subscription griot-dev -o table"])
        self.assertIn("the account was refused", done.stderr)

    def test_a_run_checks_the_node_and_deletes_its_group(self):
        self.stubs(az=AZ, openssl=OPENSSL, curl=CURL, nc=NC)
        started = time.time()
        done = self.run_script(*self.arguments())
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertRegex(done.stdout, r"create-to-ready \d+ s, boot_ms 2140\n$")
        self.assertIn('{"version":"0.7.0","cloud":"azure"}', done.stdout)
        calls = self.calls()
        az = [call for call in calls if call.startswith("az ")]
        self.assertTrue(az[0].startswith(f"az group exists -n {GROUP} "))
        self.assertTrue(az[1].startswith(f"az group create -n {GROUP} "))
        self.assertTrue(az[-4].startswith(f"az group delete -n {GROUP} --yes "))
        self.assertTrue(all(" --subscription griot-dev" in call for call in az))
        # The storage account's key reaches the data-plane commands in the environment alone.
        storage = [call for call in az if call.startswith(("az storage container", "az storage blob"))]
        self.assertEqual(len(storage), 3)
        for call in storage:
            self.assertTrue(call.endswith(" |key=the-account-key"), call)
        self.assertFalse(any("the-account-key" in call.split(" |key=")[0] for call in calls))
        run = self.root / "RUNDIR"
        self.assertEqual(stat.S_IMODE(run.stat().st_mode), 0o700)
        config = json.loads((run / "node-config.json").read_text())
        self.assertEqual(config, {"moruna": 1, "api": {"token_keys": [base64.b64encode(b"0" * 32).decode()]}})
        vm = next(call for call in az if call.startswith("az vm create"))
        self.assertIn(f"--user-data {run.name}/node-config.json ", vm)
        # The token: an EdDSA JWT for the node's instance, for observe, expiring in 10 minutes.
        header, claims, signature = (run / "token").read_text().strip().split(".")
        decode = lambda part: json.loads(base64.urlsafe_b64decode(part + "=" * (-len(part) % 4)))
        self.assertEqual(decode(header), {"alg": "EdDSA", "typ": "JWT"})
        claims = decode(claims)
        self.assertEqual((claims["aud"], claims["scope"]), (INSTANCE, ["observe"]))
        self.assertTrue(started + 590 <= claims["exp"] <= time.time() + 610)
        self.assertEqual(base64.urlsafe_b64decode(signature + "=" * (-len(signature) % 4)), b"signature")
        node = [call for call in calls if call.startswith("curl ") and "/v1/node" in call]
        self.assertEqual(len(node), 1)
        self.assertIn(f"Authorization: Bearer {header}.", node[0])
        self.assertIn("https://203.0.113.7/v1/node", node[0])
        self.assertEqual([call for call in calls if call.startswith("nc ")],
                         ["nc -vz -w 5 203.0.113.7 443", "nc -vz -w 5 203.0.113.7 22"])
        self.assertEqual((run / "instance").read_text().strip(), INSTANCE)
        self.assertEqual((run / "served.pin").read_text(), (run / "console.pin").read_text())
        # The API's checks (2.6), last, pinned to the console's certificate, for its instance,
        # given no secret; then the group is deleted.
        python = [call for call in calls if call.startswith("python3 ")]
        self.assertEqual(python, [
            f"python3 -I {REPO}/kernel/node/tools/api-checks.py --url https://203.0.113.7:443 --pin {PIN} "
            f"--token-key RUNDIR/token-key.pem --instance {INSTANCE} --openssl openssl "
            "--results RUNDIR/api-checks.json --cloud azure |key=|sas=|openssl="])
        self.assertGreater(calls.index(python[0]), calls.index(node[0]))
        self.assertLess(calls.index(python[0]),
                        next(i for i, call in enumerate(calls) if call.startswith(f"az group delete -n {GROUP}")))
        self.assertEqual(json.loads((run / "api-checks.json").read_text()), {"ok": True})
        self.assertIn("api-checks: all 9 checks pass\n", done.stdout)

    def test_the_api_checks_failing_fail_the_run_and_delete_the_group(self):
        self.stubs(az=AZ, openssl=OPENSSL, curl=CURL, nc=NC)
        done = self.run_script(*self.arguments(), extra={"STUB_CHECKS_EXIT": "1", "STUB_CHECKS_OK": "false"})
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertNotIn("create-to-ready", done.stdout)
        self.assertTrue(any(call.startswith(f"az group delete -n {GROUP} --yes") for call in self.calls()))
        self.assertEqual(json.loads((self.root / "RUNDIR/api-checks.json").read_text()), {"ok": False})

    def test_a_data_disk_is_made_and_taken_at_lun_0(self):
        self.stubs(az=AZ, openssl=OPENSSL, curl=CURL, nc=NC)
        done = self.run_script(*self.arguments("--data-disk"))
        self.assertEqual(done.returncode, 0, done.stderr)
        az = [call.split(" |key=")[0] for call in self.calls() if call.startswith("az ")]
        disks = [call for call in az if call.startswith("az disk create")]
        self.assertEqual(disks[1], DATA_DISK)
        vm = next(call for call in az if call.startswith("az vm create"))
        self.assertIn(" --attach-data-disks moruna-node-data --no-wait ", vm)
        self.assertLess(az.index(disks[1]), az.index(vm), "the disk is made before the machine takes it")

    def test_a_job_runs_through_the_api_to_its_exit_and_its_rows_are_counted(self):
        self.stubs(az=AZ, openssl=OPENSSL, curl=CURL, nc=NC)
        started = time.time()
        done = self.run_script(*self.arguments("--job"))
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("job: 10000000 rows counted, wall_ms 2.5\n", done.stdout)
        self.assertRegex(done.stdout, r"create-to-ready \d+ s, boot_ms 2140, job wall_ms 2.5\n$")
        run = self.root / "RUNDIR"
        self.assertEqual(json.loads((run / "job.json").read_text()),
                         {"v1": {"moruna_spec": 1, "source": {"kind": "rows", "rows": 10000000},
                                 "sink": {"kind": "count"}}})
        # The job's token: for submit, the node's instance, 30 minutes.
        header, claims, _ = (run / "job-token").read_text().strip().split(".")
        decode = lambda part: json.loads(base64.urlsafe_b64decode(part + "=" * (-len(part) % 4)))
        self.assertEqual(decode(header), {"alg": "EdDSA", "typ": "JWT"})
        claims = decode(claims)
        self.assertEqual((claims["aud"], claims["scope"]), (INSTANCE, ["submit"]))
        self.assertTrue(started + 1790 <= claims["exp"] <= time.time() + 1810)
        job = [call for call in self.calls() if call.startswith("curl ") and "/v1/jobs" in call]
        self.assertEqual([call.split()[-1] for call in job], [
            "https://203.0.113.7/v1/jobs/validate",
            "https://203.0.113.7/v1/jobs",
            "https://203.0.113.7/v1/jobs/0123456789abcdef0123456789abcdef/events",
            "https://203.0.113.7/v1/jobs/0123456789abcdef0123456789abcdef/report",
        ])
        token = (run / "job-token").read_text().strip()
        self.assertTrue(all(f"Authorization: Bearer {token}" in call for call in job))
        self.assertIn("--data-binary @RUNDIR/job.json", job[0])
        self.assertIn(" -N ", job[2], "the events are read as they come")
        self.assertEqual((run / "job.id").read_text().strip(), "0123456789abcdef0123456789abcdef")
        self.assertEqual(json.loads((run / "job-exit.json").read_text()),
                         {"type": "exit", "code": 0, "diagnostic": None})

    def test_a_job_that_fails_or_counts_short_fails_the_run_and_deletes_the_group(self):
        for extra in ({"STUB_EXIT": "1"}, {"STUB_EXIT": "10"}, {"STUB_ROWS": "9999999"}):
            self.log.write_text("")
            run = self.root / "RUNDIR"
            if run.exists():
                for path in sorted(run.iterdir()):
                    path.unlink()
                run.rmdir()
            self.stubs(az=AZ, openssl=OPENSSL, curl=CURL, nc=NC)
            done = self.run_script(*self.arguments("--job"), extra=extra)
            self.assertEqual(done.returncode, 1, (extra, done.stderr))
            self.assertNotIn("create-to-ready", done.stdout)
            self.assertTrue(any(call.startswith(f"az group delete -n {GROUP} --yes") for call in self.calls()))

    def test_a_served_certificate_that_is_not_the_consoles_fails_and_deletes_the_group(self):
        self.stubs(az=AZ, openssl=OPENSSL.replace("$STUB_PIN *stdin", "0000 *stdin"), curl=CURL, nc=NC)
        done = self.run_script(*self.arguments())
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertTrue(any(call.startswith(f"az group delete -n {GROUP} --yes") for call in self.calls()))
        self.assertFalse(any("/v1/node" in call for call in self.calls()), "no token is sent to a node not pinned")

    def test_a_proof_runs_on_a_d4s_v4_from_blob_with_its_functions_by_sas(self):
        self.stubs(az=AZ, openssl=OPENSSL, curl=CURL, nc=NC, python3=PYTHON)
        started = datetime.datetime.now(datetime.timezone.utc)
        done = self.run_script(*self.arguments(*PROOF_ARGS), extra={"STUB_SAS": SAS})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("proof: score-2024-01 pass\n", done.stdout)
        self.assertRegex(done.stdout, r"create-to-ready \d+ s, boot_ms 2140\n$")
        calls = self.calls()
        az = [call.split(" |key=")[0] for call in calls if call.startswith("az ")]
        vm = next(call for call in az if call.startswith("az vm create"))
        self.assertIn(" --size Standard_D4s_v4 ", vm)
        self.assertNotIn("--attach-data-disks", vm)
        # Node config: the token key and the run's function key (3E).
        run = self.root / "RUNDIR"
        config = json.loads((run / "node-config.json").read_text())
        key = base64.b64encode(b"0" * 32).decode()
        self.assertEqual(config, {"moruna": 1, "api": {"token_keys": [key]}, "functions": {"keys": [key]}})
        self.assertTrue((run / "function-key.pem").exists())
        # The data and the four tasks in their containers, with the account's key in the
        # environment alone, and the SAS made from it, before the machine.
        storage = [call for call in calls if call.startswith(("az storage container", "az storage blob"))]
        self.assertEqual([" ".join(call.split()[1:5]) for call in storage], [
            "storage container create -n", "storage blob upload -c", "storage blob url -c",
            "storage container create -n", "storage container create -n", "storage blob upload-batch -d",
            "storage blob upload -c", "storage blob upload -c", "storage blob upload -c", "storage blob upload -c",
            "storage container generate-sas -n"])
        for call in storage:
            self.assertTrue(call.endswith(" |key=the-account-key"), call)
        batch = next(call for call in az if " upload-batch " in call)
        self.assertIn(" -d data --destination-path nyc -s NYCDIR/norm --pattern yellow_tripdata_2024-*.parquet ", batch)
        tasks = [call for call in az if call.startswith("az storage blob upload -c functions")]
        self.assertEqual([call.split(" -f ")[1].split()[0] for call in tasks],
                         [f"TASKS/gate-{job}" for job in ("score", "occupancy", "enrich", "audit")])
        sas = next(call for call in az if " generate-sas " in call)
        self.assertIn(" -n functions --permissions r ", sas)
        self.assertIn(" --https-only ", sas)
        expiry = datetime.datetime.strptime(re.search(r" --expiry (\S+) ", sas).group(1), "%Y-%m-%dT%H:%MZ")
        expiry = expiry.replace(tzinfo=datetime.timezone.utc)
        hours = datetime.timedelta(hours=6)
        self.assertTrue(started + hours - datetime.timedelta(minutes=2) <= expiry
                        <= datetime.datetime.now(datetime.timezone.utc) + hours, expiry)
        self.assertLess(az.index(sas), az.index(vm))
        self.assertEqual((run / "functions.sas").read_text().strip(), SAS)
        self.assertEqual(stat.S_IMODE((run / "functions.sas").stat().st_mode), 0o600)
        # proof.py: once, after the node's checks, the key and the SAS in its environment only;
        # then the API's checks, given neither.
        python = [call for call in calls if call.startswith("python3 ")]
        self.assertEqual(len(python), 2)
        proof, checks = python[:1], python[1]
        self.assertIn("/kernel/node/tools/api-checks.py ", checks)
        self.assertTrue(checks.endswith(" --cloud azure |key=|sas=|openssl="), checks)
        argv, env = proof[0].split(" |key=")
        self.assertEqual(env, f"the-account-key|sas={SAS}|openssl=openssl")
        self.assertEqual(argv, (
            f"python3 -I {REPO}/bench/node/proof.py --url https://203.0.113.7:443 --pin {PIN} "
            f"--token-key RUNDIR/token-key.pem --instance {INSTANCE} --function-key RUNDIR/function-key.pem "
            "--functions TASKS --function-base https://mrnnodename.blob.core.windows.net/functions --data blob "
            "--blob-account mrnnodename --blob-container data --periods 2024-01,2024 --references NYCDIR "
            "--gate-canon GATECANON --results RUNDIR/proof --work RUNDIR/proof-work"))
        node = next(i for i, call in enumerate(calls) if "/v1/node" in call)
        delete = next(i for i, call in enumerate(calls) if call.startswith(f"az group delete -n {GROUP} --yes"))
        self.assertLess(node, calls.index(proof[0]))
        self.assertLess(calls.index(proof[0]), delete)
        self.assertEqual(json.loads((run / "proof/proof.json").read_text()), {"ok": True})
        # Neither secret is on any command line, nor in what the script said.
        for call in calls:
            line = call.split(" |key=")[0]
            self.assertNotIn("the-account-key", line)
            self.assertNotIn("FAKEfakeFAKEfake", line)
        for said in (done.stdout, done.stderr):
            self.assertNotIn("the-account-key", said)
            self.assertNotIn("FAKEfakeFAKEfake", said)

    def test_a_proof_that_fails_fails_the_run_keeps_its_results_and_deletes_the_group(self):
        self.stubs(az=AZ, openssl=OPENSSL, curl=CURL, nc=NC, python3=PYTHON)
        done = self.run_script(*self.arguments(*PROOF_ARGS),
                               extra={"STUB_SAS": SAS, "STUB_PROOF_EXIT": "1", "STUB_PROOF_OK": "false"})
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertNotIn("create-to-ready", done.stdout)
        calls = self.calls()
        proof = next(i for i, call in enumerate(calls) if call.startswith("python3 "))
        self.assertTrue(any(call.startswith(f"az group delete -n {GROUP} --yes") for call in calls[proof:]))
        self.assertEqual(json.loads((self.root / "RUNDIR/proof/proof.json").read_text()), {"ok": False})
        # The API's checks still run, after the proof and before the group is deleted.
        python = [call for call in calls if call.startswith("python3 ")]
        self.assertEqual([call.split()[2].rsplit("/", 1)[1] for call in python], ["proof.py", "api-checks.py"])
        delete = next(i for i, call in enumerate(calls) if call.startswith(f"az group delete -n {GROUP} --yes"))
        self.assertLess(calls.index(python[1]), delete)
        self.assertEqual(json.loads((self.root / "RUNDIR/api-checks.json").read_text()), {"ok": True})


if __name__ == "__main__":
    unittest.main()
