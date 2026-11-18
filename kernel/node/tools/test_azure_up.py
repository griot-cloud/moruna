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

Run: python3 -I kernel/node/tools/test_azure_up.py
"""
import base64
import json
import os
from pathlib import Path
import re
import stat
import subprocess
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
az vm boot-diagnostics get-boot-log -n moruna-node -g moruna-node-name --subscription griot-dev > RUNDIR/console.log
az group delete -n moruna-node-name --yes --subscription griot-dev
az group exists -n moruna-node-name --subscription griot-dev
az group list --subscription griot-dev --query "[?starts_with(name, 'moruna-node-')].name" -o tsv
az network watcher list --subscription griot-dev -o table
""".splitlines()

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
esac
exit 0
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
case "$*" in
  */v1/health*) printf 200 ;;
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
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "calls"
        self.log.touch()

    def stubs(self, **scripts):
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

    def test_every_az_command_names_the_subscription_and_the_run_group(self):
        for line in DRY_RUN:
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
        placeholders = [("RUNDIR", "<RUN>"), ("OUTDIR/", "<OUT>/"), (GROUP, "<NAME>"), ("mrnnodename", "<ACCOUNT>"),
                        ("192.0.2.1", "<FROM>")]
        script = []
        for line in DRY_RUN:
            for value, placeholder in placeholders:
                line = line.replace(value, placeholder)
            script.append(line)
        for line in script:
            self.assertIn(line, recipe)
        only_in_recipe = [line for line in recipe if line not in script]
        self.assertEqual(only_in_recipe, [
            "kernel/node/tools/azure-up.sh --vhd <OUT>/moruna-node.vhd --run <RUN> --from <FROM> --group <NAME> "
            "2>&1 | tee <RUN>.log",
            "az account show --subscription griot-dev --query name -o tsv",
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

    def test_a_served_certificate_that_is_not_the_consoles_fails_and_deletes_the_group(self):
        self.stubs(az=AZ, openssl=OPENSSL.replace("$STUB_PIN *stdin", "0000 *stdin"), curl=CURL, nc=NC)
        done = self.run_script(*self.arguments())
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertTrue(any(call.startswith(f"az group delete -n {GROUP} --yes") for call in self.calls()))
        self.assertFalse(any("/v1/node" in call for call in self.calls()), "no token is sent to a node not pinned")


if __name__ == "__main__":
    unittest.main()
