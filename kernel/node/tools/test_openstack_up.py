#!/usr/bin/env python3
"""openstack-up.sh (work item 1.5b), against stand-ins for `openstack`, `openssl`, `nc` and `sleep`.

Nothing here reaches a cloud. The stand-in `openstack` records every call and answers as
Servercore would; one that is never to be called fails the test if it is.

- The dry run prints every command, exactly the lists below, for a boot from the image and one
  from a volume, and runs none.
- `kernel/node/OPENSTACK.md` holds every command the script runs, and the script runs every command
  it holds but the two lines that run the script.
- Every `openstack` call is bounded by perl's alarm and names cloud servercore and the region; the
  bound ends a call that hangs.
- The guard refuses any cloud but servercore, any name not `moruna-node-*`, a bad region, zone or
  boot, and a name the project already has, before anything is made.
- A run that goes well checks the node and deletes everything it made, in the reverse of the order
  it was made; one that fails part-way deletes what it made so far, and nothing else; a call that
  fails or is cut short in the deletion does not stop the ones after it; anything of the name left
  at the end fails the run.
- The console is read from the create on, its errors kept, and the longest log kept.

Run: python3 -I kernel/node/tools/test_openstack_up.py
"""
import base64
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
import time
import unittest

TOOLS = Path(__file__).resolve().parent
SCRIPT = TOOLS / "openstack-up.sh"
RECIPE = TOOLS.parent / "OPENSTACK.md"
NAME = "moruna-node-name"
REGION = "region-1"
ZONE = "zone-1a"
INSTANCE = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0"

BOUND = "perl -e 'alarm shift; exec @ARGV; exit 127'"
OS = f"{BOUND} 120 openstack --os-cloud servercore --os-region-name {REGION}"
UPLOAD = f"{BOUND} 600 openstack --os-cloud servercore --os-region-name {REGION}"


def cat(file):
    return f'"$(cat RUNDIR/{file})"'


SERVER = cat("server.id")
CONSOLE = (f"{OS} console log show {SERVER} > RUNDIR/console.now 2> RUNDIR/console.now.err; "
           "cat RUNDIR/console.now.err >> RUNDIR/console.err; "
           '[ "$(wc -c < RUNDIR/console.now)" -le "$(wc -c < RUNDIR/console.log)" ] '
           "|| cp RUNDIR/console.now RUNDIR/console.log")
STATUS = f"{OS} server show {SERVER} -f value -c status > RUNDIR/status"
ROUND = ("printf '%s %s, console %s bytes %s\\n' \"$(date -u +%H:%M:%S)\" \"$(cat RUNDIR/status)\" "
         "\"$(( $(wc -c < RUNDIR/console.now) ))\" \"$(head -c 160 RUNDIR/console.now.err | tr '\\n' ' ')\"")
NAMED = [
    f'ids="$({OS} image list --name {NAME} -f value -c ID)" && [ -z "$ids" ]',
    f'names="$({OS} security group list -f value -c Name)" && ! printf \'%s\\n\' "$names" | grep -q -x -e {NAME}',
    f'ids="$({OS} network list --name {NAME} -f value -c ID)" && [ -z "$ids" ]',
    f'ids="$({OS} subnet list --name {NAME} -f value -c ID)" && [ -z "$ids" ]',
    f'ids="$({OS} router list --name {NAME} -f value -c ID)" && [ -z "$ids" ]',
    f'ids="$({OS} volume list --name {NAME} -f value -c ID)" && [ -z "$ids" ]',
    f'ids="$({OS} server list --name \'^{NAME}$\' -f value -c ID)" && [ -z "$ids" ]',
]

# Every command of a dry run that boots from the image, in order, with RUNDIR, OUTDIR, the name,
# the region and the zone as the tests give them.
DRY_RUN = [
    "mkdir -m 700 RUNDIR",
    "openssl genpkey -algorithm ed25519 -out RUNDIR/token-key.pem",
    "openssl pkey -in RUNDIR/token-key.pem -pubout -outform DER | tail -c 32 | base64 > RUNDIR/token-key.pub",
    "printf '{\"moruna\":1,\"api\":{\"token_keys\":[\"%s\"]}}\\n' \"$(cat RUNDIR/token-key.pub)\" > RUNDIR/node-config.json",
    *NAMED,
    f"{UPLOAD} image create --disk-format qcow2 --container-format bare --property hw_firmware_type=uefi "
    "--property hw_machine_type=q35 --property hw_disk_bus=virtio --property hw_vif_model=virtio "
    f"--file OUTDIR/moruna-node.qcow2 -f value -c id {NAME} > RUNDIR/image.id",
    f"{OS} image show {cat('image.id')} -c status -c size -c disk_format -c container_format -c properties",
    f"[ \"$({OS} image show {cat('image.id')} -f value -c status)\" = active ]",
    f"{OS} security group create --description 'Moruna node: TCP 443 inbound only' -f value -c id {NAME} "
    "> RUNDIR/group.id",
    f"{OS} security group rule create --ingress --ethertype IPv4 --protocol tcp --dst-port 443 "
    f"--remote-ip 0.0.0.0/0 {cat('group.id')} -f value -c id",
    f"{OS} security group rule list {cat('group.id')}",
    f"{OS} network create -f value -c id {NAME} > RUNDIR/network.id",
    f"{OS} subnet create --network {cat('network.id')} --subnet-range 192.168.77.0/24 --dhcp -f value -c id "
    f"{NAME} > RUNDIR/subnet.id",
    f"{OS} router create -f value -c id {NAME} > RUNDIR/router.id",
    f"{OS} router set --external-gateway external-network {cat('router.id')}",
    f"{OS} router add subnet {cat('router.id')} {cat('subnet.id')}",
    f"{OS} server create --image {cat('image.id')} --flavor BL1.1-2048-16 --network {cat('network.id')} "
    f"--security-group {cat('group.id')} --user-data RUNDIR/node-config.json -f value -c id {NAME} "
    "> RUNDIR/server.id && date -u +%s > RUNDIR/created.at",
    "touch RUNDIR/console.log RUNDIR/console.err; deadline=$(( $(date -u +%s) + 300 )); "
    f"while [ \"$(date -u +%s)\" -lt \"$deadline\" ]; do {STATUS}; {CONSOLE}; {ROUND}; "
    "[ \"$(cat RUNDIR/status)\" = BUILD ] || [ ! -s RUNDIR/status ] || break; sleep 2; done",
    f"{OS} server show {SERVER} -c status -c addresses -c fault",
    "[ \"$(cat RUNDIR/status)\" = ACTIVE ]",
    f"{OS} floating ip create external-network -f value -c id > RUNDIR/fip.id",
    f"{OS} floating ip show {cat('fip.id')} -f value -c floating_ip_address > RUNDIR/address",
    f"{OS} server add floating ip {SERVER} {cat('address')}",
    "deadline=$(( $(date -u +%s) + 300 )); while [ \"$(date -u +%s)\" -lt \"$deadline\" ]; do "
    f"{STATUS}; {CONSOLE}; {ROUND}; "
    "tr -d '\\r' < RUNDIR/console.log | grep -a -q -e 'moruna: ready' -e 'moruna: fatal' && break; "
    "[ \"$(cat RUNDIR/status)\" = ACTIVE ] || [ ! -s RUNDIR/status ] || break; sleep 2; done; "
    "date -u +%s > RUNDIR/seen.at; tr -d '\\r' < RUNDIR/console.log | grep -a 'moruna: '",
    "tr -d '\\r' < RUNDIR/console.log | grep -a -q 'moruna: ready api='",
    "tr -d '\\r' < RUNDIR/console.log | grep -a -q 'moruna: cloud openstack instance '",
    "nc -vz -w 5 \"$(cat RUNDIR/address)\" 443",
    "! nc -vz -w 5 \"$(cat RUNDIR/address)\" 22",
    f"[ \"$({OS} server show {SERVER} -f value -c status)\" = ACTIVE ]",
    f"touch RUNDIR/console.log RUNDIR/console.err; {CONSOLE}",
    f"{OS} floating ip delete {cat('fip.id')}",
    f"{OS} server delete --wait {SERVER}",
    f"{OS} router remove subnet {cat('router.id')} {cat('subnet.id')}",
    f"{OS} router unset --external-gateway {cat('router.id')}",
    f"{OS} router delete {cat('router.id')}",
    f"{OS} subnet delete {cat('subnet.id')}",
    f"{OS} network delete {cat('network.id')}",
    f"{OS} security group delete {cat('group.id')}",
    f"{OS} image delete {cat('image.id')}",
    *NAMED,
    f'ids="$({OS} floating ip list -f value -c ID)" && ! printf \'%s\\n\' "$ids" | grep -q -x -e {cat("fip.id")}',
]

# A boot from a volume: the volume made, waited for and read back before the server, which is made
# from it on the zero-disk flavor in its zone; and the volume deleted once the server lets go.
VOLUME = [
    f"{OS} volume create --image {cat('image.id')} --size 2 --bootable --type basic.{ZONE} "
    f"--availability-zone {ZONE} -f value -c id {NAME} > RUNDIR/volume.id",
    f"for _ in $(seq 60); do {OS} volume show {cat('volume.id')} -f value -c status > RUNDIR/volume.status; "
    "cat RUNDIR/volume.status; case \"$(cat RUNDIR/volume.status)\" in available | error) break ;; esac; "
    "sleep 5; done; [ \"$(cat RUNDIR/volume.status)\" = available ]",
    f"{OS} volume show {cat('volume.id')} -c status -c size -c bootable -c volume_image_metadata",
]
VOLUME_SERVER = (
    f"{OS} server create --volume {cat('volume.id')} --flavor BL1.1-2048 --availability-zone {ZONE} "
    f"--network {cat('network.id')} --security-group {cat('group.id')} --user-data RUNDIR/node-config.json "
    f"-f value -c id {NAME} > RUNDIR/server.id && date -u +%s > RUNDIR/created.at")
VOLUME_DELETE = [
    f"for _ in $(seq 24); do {OS} volume show {cat('volume.id')} -f value -c status > RUNDIR/volume.status; "
    "cat RUNDIR/volume.status; case \"$(cat RUNDIR/volume.status)\" in in-use | detaching) sleep 5 ;; "
    "*) break ;; esac; done",
    f"{OS} volume delete {cat('volume.id')}",
]


def from_volume():
    lines = []
    for line in DRY_RUN:
        if " server create --image " in line:
            lines += VOLUME + [VOLUME_SERVER]
            continue
        lines.append(line)
        if " server delete --wait " in line:
            lines += VOLUME_DELETE
    return lines


DRY_RUN_VOLUME = from_volume()

# The stand-in for `openstack`: records every call, refuses one off cloud servercore and the
# region, and answers as Servercore would. Each made thing's ID is `id-<kind>`.
OPENSTACK = f"""#!{sys.executable} -I
import os, sys
log, state = os.environ["STUB_LOG"], os.environ["STUB_STATE"]
argv = sys.argv[1:]
with open(log, "a") as f:
    f.write("openstack " + " ".join(argv) + "\\n")
if argv[:4] != ["--os-cloud", "servercore", "--os-region-name", os.environ["STUB_REGION"]]:
    sys.stderr.write("stub: not cloud servercore and the run's region\\n")
    sys.exit(98)
cmd = " ".join(argv[4:])
env = lambda key: os.environ.get(key, "")
calls = lambda: open(log).read()
def count(name):
    path = os.path.join(state, name)
    n = int(open(path).read()) + 1 if os.path.exists(path) else 1
    open(path, "w").write(str(n))
    return n
if env("STUB_FAIL") and cmd.startswith(env("STUB_FAIL")):
    sys.stderr.write("stub: " + env("STUB_FAIL") + " refused\\n")
    sys.exit(1)
if env("STUB_HANG") and cmd.startswith(env("STUB_HANG")):
    sys.exit(142)
name = env("STUB_NAME")
def holds(kind):
    return env("STUB_TAKEN") == kind or (env("STUB_LEFT") == kind and " delete " in calls())
for kind in ("image", "network", "subnet", "router", "volume", "server"):
    if cmd.startswith(kind + " list "):
        if holds(kind):
            print("id-taken-" + kind)
        sys.exit(0)
if cmd.startswith("security group list "):
    print("default")
    if holds("group"):
        print(name)
    sys.exit(0)
if cmd.startswith("floating ip list "):
    print("id-other-fip")
    if holds("fip"):
        print("id-fip")
    sys.exit(0)
if cmd.startswith("image create "):
    path = argv[argv.index("--file") + 1]
    if not os.path.isfile(path):
        sys.stderr.write("stub: no file " + path + "\\n")
        sys.exit(1)
    print("id-image")
elif cmd.startswith("image show ") and "-f value -c status" in cmd:
    print("active")
elif cmd.startswith("image show "):
    print("status active")
elif cmd.startswith("security group create "):
    print("id-group")
elif cmd.startswith("security group rule create "):
    print("id-rule")
elif cmd.startswith("security group rule list "):
    print("id-rule tcp IPv4 0.0.0.0/0 443:443 ingress")
elif cmd.startswith(("network create ", "subnet create ", "router create ", "volume create ", "server create ")):
    print("id-" + cmd.split()[0])
elif cmd.startswith("volume show ") and "-f value -c status" in cmd:
    if " server delete " in calls():
        print({{1: "in-use", 2: "detaching"}}.get(count("volume-detach"), "available"))
    else:
        print({{1: "downloading"}}.get(count("volume-create"), "available"))
elif cmd.startswith("volume show "):
    print("bootable true")
elif cmd.startswith("server show ") and "-f value -c status" in cmd:
    print("BUILD" if count("server-show") == 1 else env("STUB_SERVER") or "ACTIVE")
elif cmd.startswith("server show "):
    print("status " + (env("STUB_SERVER") or "ACTIVE") + " addresses " + name + "=192.168.77.139")
elif cmd.startswith("console log show "):
    n = count("console")
    if env("STUB_NO_CONSOLE"):
        sys.stderr.write("NotFoundException: 404: Guest does not have a console available.\\n")
        sys.exit(1)
    if env("STUB_CONSOLE_ONCE") and n > 1:
        sys.exit(0)
    sys.stdout.write(env("STUB_CONSOLE"))
elif cmd.startswith("floating ip create "):
    print("id-fip")
elif cmd.startswith("floating ip show "):
    print("203.0.113.7")
"""
CONSOLE_LINES = ("BdsDxe: loading\r\nmoruna: core 0.7.0 cpus=1 memory=2147483648\r\nmoruna: services started\r\n"
                 "moruna: network wan 192.168.77.139/24 gateway 192.168.77.1\r\n"
                 f"moruna: cloud openstack instance {INSTANCE}\r\n"
                 "moruna: tls sha256=9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08\r\n"
                 "moruna: ready api=192.168.77.139:443 boot_ms=2140\r\n")
OPENSSL = r"""#!/bin/sh
echo "openssl $*" >> "$STUB_LOG"
case "$1" in
  genpkey) while [ "$#" -gt 0 ]; do [ "$1" = -out ] && echo private > "$2"; shift; done ;;
  pkey) printf '%044d' 0 ;;
esac
"""
NC = r"""#!/bin/sh
echo "nc $*" >> "$STUB_LOG"
case "$*" in *" 443") exit 0 ;; *) exit 1 ;; esac
"""
SLEEP = """#!/bin/sh
exit 0
"""
NEVER = """#!/bin/sh
echo "$(basename "$0") $* (called in a dry run)" >> "$STUB_LOG"
exit 99
"""
PREFIX = f"openstack --os-cloud servercore --os-region-name {REGION} "


def commands(markdown: str):
    """The commands in `markdown`'s fenced blocks: each line, continued lines joined, its spaces
    collapsed."""
    found = []
    for block in re.findall(r"^ *```\n(.*?)^ *```", markdown, re.S | re.M):
        joined = re.sub(r"\\\n\s*", " ", block)
        found += [re.sub(r" +", " ", line.strip()) for line in joined.splitlines() if line.strip()]
    return found


class OpenstackUp(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        (self.root / "OUTDIR").mkdir()
        (self.root / "OUTDIR/moruna-node.qcow2").write_bytes(b"QFI\xfb" + b"\0" * 1020)
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

    def live(self):
        self.stubs(openstack=OPENSTACK, openssl=OPENSSL, nc=NC, sleep=SLEEP)

    def calls(self):
        return self.log.read_text().splitlines()

    def openstack(self):
        """The openstack calls, each without its cloud and region."""
        return [call[len(PREFIX):] for call in self.calls() if call.startswith("openstack ")]

    def run_script(self, *arguments, extra=None):
        env = {"PATH": f"{self.bin}:/usr/bin:/bin", "STUB_LOG": str(self.log), "STUB_STATE": str(self.state),
               "STUB_REGION": REGION, "STUB_NAME": NAME, "STUB_CONSOLE": CONSOLE_LINES, "MORUNA_OPENSSL": "openssl",
               **(extra or {})}
        return subprocess.run(["bash", str(SCRIPT), *arguments], cwd=self.root, capture_output=True, text=True,
                              env=env, timeout=120, check=False)

    def arguments(self, *more, boot="image", name=NAME, run="RUNDIR"):
        zone = ["--zone", ZONE] if boot == "volume" else []
        return ["--qcow2", "OUTDIR/moruna-node.qcow2", "--run", run, "--region", REGION, "--boot", boot, *zone,
                "--name", name, *more]

    def test_a_dry_run_prints_every_command_and_runs_none(self):
        self.stubs(openstack=NEVER, openssl=NEVER, nc=NEVER, perl=NEVER)
        for boot, expected in (("image", DRY_RUN), ("volume", DRY_RUN_VOLUME)):
            done = self.run_script(*self.arguments("--dry-run", boot=boot))
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual(done.stdout.splitlines(), expected, boot)
            self.assertEqual(done.stderr, "")
        self.assertEqual(self.calls(), [], "a dry run runs nothing")
        self.assertFalse((self.root / "RUNDIR").exists())

    def test_every_openstack_call_is_bounded_on_cloud_servercore_and_the_region(self):
        for line in DRY_RUN_VOLUME:
            rest = line.replace(f"{BOUND} 120 openstack --os-cloud servercore --os-region-name {REGION} ", "")
            rest = rest.replace(f"{BOUND} 600 openstack --os-cloud servercore --os-region-name {REGION} ", "")
            rest = rest.replace("moruna: cloud openstack instance", "")
            self.assertNotIn("openstack", rest, line)
            self.assertNotIn("alarm", rest, line)
        # The upload alone has 600 s.
        self.assertEqual([line for line in DRY_RUN_VOLUME if " 600 openstack " in line],
                         [line for line in DRY_RUN_VOLUME if " image create " in line])
        # No zone and no volume in a boot from the image.
        self.assertFalse(any(ZONE in line or "volume create" in line for line in DRY_RUN))

    def test_the_bound_ends_a_call_that_hangs(self):
        started = time.monotonic()
        done = subprocess.run(["bash", "-c", f"{BOUND} 1 sleep 30; exit $?"], capture_output=True, check=False,
                              timeout=20)
        self.assertEqual(done.returncode, 142, "SIGALRM")
        self.assertLess(time.monotonic() - started, 10)
        done = subprocess.run(["bash", "-c", f"{BOUND} 5 true"], capture_output=True, check=False, timeout=20)
        self.assertEqual(done.returncode, 0)
        done = subprocess.run(["bash", "-c", f"{BOUND} 5 false"], capture_output=True, check=False, timeout=20)
        self.assertEqual(done.returncode, 1)
        done = subprocess.run(["bash", "-c", f"{BOUND} 5 /nonexistent/openstack"], capture_output=True,
                              check=False, timeout=20)
        self.assertEqual(done.returncode, 127, "a call that cannot start fails")

    def test_the_recipe_holds_every_command_the_script_runs_and_no_other(self):
        recipe = commands(RECIPE.read_text())
        placeholders = [("RUNDIR", "<RUN>"), ("OUTDIR/", "<OUT>/"), (NAME, "<NAME>"), (ZONE, "<ZONE>"),
                        (REGION, "<REGION>")]
        script = []
        for line in DRY_RUN + [line for line in DRY_RUN_VOLUME if line not in DRY_RUN]:
            for value, placeholder in placeholders:
                line = line.replace(value, placeholder)
            if line not in script:
                script.append(line)
        for line in script:
            self.assertIn(line, recipe)
        only_in_recipe = [line for line in recipe if line not in script]
        self.assertEqual(only_in_recipe, [
            "kernel/node/tools/openstack-up.sh --qcow2 <OUT>/moruna-node.qcow2 --run <RUN> --region <REGION> "
            "--boot image --name <NAME> 2>&1 | tee <RUN>.log",
            "kernel/node/tools/openstack-up.sh --qcow2 <OUT>/moruna-node.qcow2 --run <RUN> --region <REGION> "
            "--boot volume --zone <ZONE> --name <NAME> 2>&1 | tee <RUN>.log",
        ])

    def test_the_guard_refuses_before_anything_runs(self):
        self.stubs(openstack=NEVER, openssl=NEVER, nc=NEVER, perl=NEVER)
        refusals = [
            (["--cloud", "servercore-prod"], "cloud 'servercore-prod' is not servercore"),
            (["--cloud", "Servercore"], "is not servercore"),
            (["--name", "my-node"], "is not moruna-node-*"),
            (["--name", "moruna-bench-1"], "is not moruna-node-*"),
            (["--name", "moruna-node-"], "lowercase letters, digits and inner dashes"),
            (["--name", "moruna-node-ABC"], "lowercase letters"),
            (["--name", "moruna-node--x"], "inner dashes"),
            (["--name", "moruna-node-x-"], "inner dashes"),
            (["--name", "moruna-node-x;rm"], "lowercase letters"),
            (["--name", "moruna-node-x.y"], "lowercase letters"),
            (["--region", "RU-3"], "region 'RU-3'"),
            (["--region", "ru-3;rm"], "region 'ru-3;rm'"),
            (["--region", "-ru"], "region '-ru'"),
            (["--boot", "both"], "--boot is image or volume, not 'both'"),
            (["--zone", "ru-3b"], "--zone goes with --boot volume"),
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
        # A boot from a volume needs its zone, and a zone like a region.
        volume = self.arguments("--dry-run", boot="volume")
        zone = volume.index("--zone")
        done = self.run_script(*(volume[:zone] + volume[zone + 2:]))
        self.assertEqual(done.returncode, 2)
        self.assertIn("--boot volume needs --zone", done.stderr)
        for bad in ("ZONE", "zone;rm", "zone-"):
            volume[zone + 1] = bad
            done = self.run_script(*volume)
            self.assertEqual(done.returncode, 2, bad)
            self.assertIn(f"zone '{bad}'", done.stderr)
        done = self.run_script("--qcow2", "OUTDIR/none.qcow2", "--run", "RUNDIR", "--region", REGION, "--boot", "image")
        self.assertEqual(done.returncode, 2)
        self.assertIn("no qcow2 image", done.stderr)
        for missing in ("--qcow2", "--run", "--region", "--boot"):
            arguments = self.arguments()
            at = arguments.index(missing)
            done = self.run_script(*(arguments[:at] + arguments[at + 2:]))
            self.assertEqual(done.returncode, 2, missing)
            self.assertIn("usage", done.stderr)
        self.assertEqual(self.run_script(*self.arguments("--what")).returncode, 2, "usage")
        self.assertEqual(self.calls(), [], "a refusal runs nothing")
        # A run's directory that exists already is refused too: the run's files are its own.
        (self.root / "RUNDIR").mkdir()
        done = self.run_script(*self.arguments())
        self.assertEqual(done.returncode, 2)
        self.assertIn("must be new", done.stderr)
        self.assertEqual(self.calls(), [])

    def test_without_a_name_the_run_gets_one_of_its_own(self):
        self.stubs(openstack=NEVER, openssl=NEVER, nc=NEVER, perl=NEVER)
        arguments = self.arguments("--dry-run")
        at = arguments.index("--name")
        done = self.run_script(*(arguments[:at] + arguments[at + 2:]))
        self.assertEqual(done.returncode, 0, done.stderr)
        names = set(re.findall(r"moruna-node-[0-9]{12}\b", done.stdout))
        self.assertEqual(len(names), 1, names)

    def test_a_name_the_project_has_is_refused_and_nothing_is_made_or_deleted(self):
        for kind in ("image", "group", "network", "subnet", "router", "volume", "server"):
            self.log.write_text("")
            run = self.root / "RUNDIR"
            if run.exists():
                for path in sorted(run.iterdir()):
                    path.unlink()
                run.rmdir()
            self.live()
            done = self.run_script(*self.arguments(), extra={"STUB_TAKEN": kind})
            self.assertEqual(done.returncode, 2, (kind, done.stderr))
            self.assertIn(f"the project has something named {NAME} already", done.stderr)
            calls = self.openstack()
            self.assertTrue(calls, kind)
            self.assertTrue(all(" list " in f" {call} " for call in calls), (kind, calls))
            self.assertFalse(any(re.search(r" (create|delete|set|add|remove|unset) ", f" {call} ") for call in calls))

    def test_a_run_from_the_image_checks_the_node_and_deletes_everything_it_made(self):
        self.live()
        done = self.run_script(*self.arguments())
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertRegex(done.stdout, r"create-to-ready \d+ s, boot_ms 2140\n$")
        run = self.root / "RUNDIR"
        self.assertEqual(stat.S_IMODE(run.stat().st_mode), 0o700)
        config = json.loads((run / "node-config.json").read_text())
        self.assertEqual(config, {"moruna": 1, "api": {"token_keys": [base64.b64encode(b"0" * 32).decode()]}})
        calls = self.openstack()
        made = [call for call in calls if re.match(r"(.* create |router set |router add |server add )", call)]
        self.assertEqual([" ".join(call.split()[:3]) for call in made], [
            "image create --disk-format", "security group create", "security group rule",
            "network create -f", "subnet create --network", "router create -f",
            "router set --external-gateway", "router add subnet",
            "server create --image", "floating ip create", "server add floating"])
        server = next(call for call in calls if call.startswith("server create "))
        self.assertEqual(server, "server create --image id-image --flavor BL1.1-2048-16 --network id-network "
                                 f"--security-group id-group --user-data RUNDIR/node-config.json -f value -c id {NAME}")
        self.assertIn(f"image create --disk-format qcow2 --container-format bare --property hw_firmware_type=uefi "
                      "--property hw_machine_type=q35 --property hw_disk_bus=virtio --property hw_vif_model=virtio "
                      f"--file OUTDIR/moruna-node.qcow2 -f value -c id {NAME}", calls)
        self.assertIn("subnet create --network id-network --subnet-range 192.168.77.0/24 --dhcp -f value -c id "
                      f"{NAME}", calls)
        self.assertIn("server add floating ip id-server 203.0.113.7", calls)
        self.assertFalse(any("volume" in call for call in calls if " list " not in f" {call} "))
        # Everything deleted, in the reverse of the order it was made.
        first = calls.index("server show id-server -f value -c status", calls.index("server add floating ip "
                                                                                    "id-server 203.0.113.7") + 1)
        deleted = [call for call in calls[first:] if re.search(r" (delete|remove|unset) ", f" {call} ")]
        self.assertEqual(deleted, [
            "floating ip delete id-fip", "server delete --wait id-server", "router remove subnet id-router id-subnet",
            "router unset --external-gateway id-router", "router delete id-router", "subnet delete id-subnet",
            "network delete id-network", "security group delete id-group", "image delete id-image"])
        # Then nothing of the name is left, and the floating IP is gone.
        tail = calls[calls.index("image delete id-image") + 1:]
        self.assertEqual([call.split(" -f ")[0] for call in tail], [
            f"image list --name {NAME}", "security group list", f"network list --name {NAME}",
            f"subnet list --name {NAME}", f"router list --name {NAME}", f"volume list --name {NAME}",
            f"server list --name ^{NAME}$", "floating ip list"])
        # The checks, on the floating IP.
        self.assertEqual([call for call in self.calls() if call.startswith("nc ")],
                         ["nc -vz -w 5 203.0.113.7 443", "nc -vz -w 5 203.0.113.7 22"])
        self.assertEqual((run / "address").read_text().strip(), "203.0.113.7")
        self.assertIn("moruna: ready api=192.168.77.139:443 boot_ms=2140", done.stdout)
        self.assertEqual((run / "console.log").read_bytes(), CONSOLE_LINES.encode())

    def test_the_console_is_read_from_the_create_on(self):
        self.live()
        done = self.run_script(*self.arguments())
        self.assertEqual(done.returncode, 0, done.stderr)
        calls = self.openstack()
        create = calls.index(next(call for call in calls if call.startswith("server create ")))
        # The first round after the create: the status, then the console, before anything else.
        self.assertEqual(calls[create + 1:create + 3],
                         ["server show id-server -f value -c status", "console log show id-server"])
        # Each round prints the time, the status and the console's size.
        self.assertRegex(done.stdout, r"\d\d:\d\d:\d\d BUILD, console \d+ bytes")
        self.assertRegex(done.stdout, r"\d\d:\d\d:\d\d ACTIVE, console \d+ bytes")

    def test_a_run_from_a_volume_makes_it_in_the_zone_and_deletes_it_once_it_is_let_go(self):
        self.live()
        done = self.run_script(*self.arguments(boot="volume"))
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertRegex(done.stdout, r"create-to-ready \d+ s, boot_ms 2140\n$")
        calls = self.openstack()
        self.assertIn(f"volume create --image id-image --size 2 --bootable --type basic.{ZONE} "
                      f"--availability-zone {ZONE} -f value -c id {NAME}", calls)
        server = next(call for call in calls if call.startswith("server create "))
        self.assertEqual(server, f"server create --volume id-volume --flavor BL1.1-2048 --availability-zone {ZONE} "
                                 "--network id-network --security-group id-group --user-data RUNDIR/node-config.json "
                                 f"-f value -c id {NAME}")
        # The server is made once the volume is available: downloading, then available.
        self.assertEqual(done.stdout.split("downloading\n")[1].split("\n")[0], "available")
        self.assertLess(calls.index(f"volume create --image id-image --size 2 --bootable --type basic.{ZONE} "
                                    f"--availability-zone {ZONE} -f value -c id {NAME}"), calls.index(server))
        deleted = [call for call in calls if re.search(r" (delete|remove|unset) ", f" {call} ")]
        self.assertEqual(deleted, [
            "floating ip delete id-fip", "server delete --wait id-server", "volume delete id-volume",
            "router remove subnet id-router id-subnet", "router unset --external-gateway id-router",
            "router delete id-router", "subnet delete id-subnet", "network delete id-network",
            "security group delete id-group", "image delete id-image"])
        # The volume is deleted once the server has let it go: in-use, detaching, then available.
        between = calls[calls.index("server delete --wait id-server") + 1:calls.index("volume delete id-volume")]
        self.assertEqual(between, ["volume show id-volume -f value -c status"] * 3)
        self.assertIn("in-use\ndetaching\navailable\n", done.stdout)

    def test_a_failure_part_way_deletes_what_was_made_and_nothing_else(self):
        self.live()
        done = self.run_script(*self.arguments(), extra={"STUB_FAIL": "router set "})
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertIn("stub: router set  refused", done.stderr)
        calls = self.openstack()
        self.assertFalse(any(call.startswith(("server ", "floating ip ", "console ", "volume ")) and " list " not in
                             f" {call} " for call in calls))
        deleted = [call for call in calls if re.search(r" (delete|remove|unset) ", f" {call} ")]
        self.assertEqual(deleted, [
            "router remove subnet id-router id-subnet", "router unset --external-gateway id-router",
            "router delete id-router", "subnet delete id-subnet", "network delete id-network",
            "security group delete id-group", "image delete id-image"])
        self.assertNotIn("create-to-ready", done.stdout)
        # An upload that fails leaves no image to delete.
        self.log.write_text("")
        for path in sorted((self.root / "RUNDIR").iterdir()):
            path.unlink()
        (self.root / "RUNDIR").rmdir()
        done = self.run_script(*self.arguments(), extra={"STUB_FAIL": "image create "})
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertFalse(any(re.search(r" (delete|remove|unset) ", f" {call} ") for call in self.openstack()))

    def test_a_node_that_stops_fails_the_run_keeps_the_consoles_errors_and_deletes_everything(self):
        self.live()
        done = self.run_script(*self.arguments(), extra={"STUB_SERVER": "SHUTOFF", "STUB_NO_CONSOLE": "1"})
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertIn("SHUTOFF, console 0 bytes NotFoundException: 404: Guest does not have a console available.",
                      done.stdout)
        run = self.root / "RUNDIR"
        errors = (run / "console.err").read_text().splitlines()
        self.assertEqual(errors, ["NotFoundException: 404: Guest does not have a console available."] * 3,
                         "the two rounds' errors, and the last read's")
        calls = self.openstack()
        self.assertIn("server show id-server -c status -c addresses -c fault", calls)
        self.assertFalse(any(call.startswith("floating ip create") for call in calls), "no floating IP for a stopped server")
        deleted = [call for call in calls if re.search(r" (delete|remove|unset) ", f" {call} ")]
        self.assertEqual(deleted[0], "server delete --wait id-server")
        self.assertEqual(deleted[-1], "image delete id-image")
        self.assertNotIn("create-to-ready", done.stdout)

    def test_a_fatal_line_fails_the_run_after_the_console_is_shown(self):
        self.live()
        fatal = "moruna: core 0.7.0 cpus=1 memory=2147483648\r\nmoruna: fatal netstack: no lease within 30 s\r\n"
        done = self.run_script(*self.arguments(), extra={"STUB_CONSOLE": fatal})
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertIn("moruna: fatal netstack: no lease within 30 s\n", done.stdout)
        calls = self.openstack()
        self.assertEqual(sum(call == "console log show id-server" for call in calls), 4,
                         "the watch's two rounds (BUILD, ACTIVE), the console's one, and the last read")
        self.assertIn("floating ip delete id-fip", calls)
        self.assertFalse(any(call.startswith("nc ") for call in self.calls()), "no check of a node that stopped")

    def test_the_longest_console_is_kept(self):
        self.live()
        done = self.run_script(*self.arguments(), extra={"STUB_CONSOLE_ONCE": "1"})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual((self.root / "RUNDIR/console.log").read_bytes(), CONSOLE_LINES.encode())
        self.assertEqual((self.root / "RUNDIR/console.now").read_text(), "")

    def test_a_call_cut_short_in_the_deletion_does_not_stop_the_ones_after_it(self):
        self.live()
        done = self.run_script(*self.arguments(), extra={"STUB_HANG": "router unset "})
        self.assertEqual(done.returncode, 0, done.stderr)
        deleted = [call for call in self.openstack() if re.search(r" (delete|remove|unset) ", f" {call} ")]
        self.assertEqual(deleted[deleted.index("router unset --external-gateway id-router") + 1:], [
            "router delete id-router", "subnet delete id-subnet", "network delete id-network",
            "security group delete id-group", "image delete id-image"])

    def test_anything_of_the_name_left_fails_the_run(self):
        for kind in ("image", "group", "network", "subnet", "router", "volume", "server", "fip"):
            self.log.write_text("")
            for path in sorted(self.state.iterdir()):
                path.unlink()
            run = self.root / "RUNDIR"
            if run.exists():
                for path in sorted(run.iterdir()):
                    path.unlink()
                run.rmdir()
            self.live()
            done = self.run_script(*self.arguments(), extra={"STUB_LEFT": kind})
            self.assertEqual(done.returncode, 1, (kind, done.stderr))
            self.assertIn(f"something of {NAME} may be left", done.stderr)
            self.assertNotIn("create-to-ready", done.stdout)


if __name__ == "__main__":
    unittest.main()
