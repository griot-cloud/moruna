#!/usr/bin/env python3
"""node-up.sh and node-down.sh (work items 1.3, 1.4 and 2.2b), against a stand-in for QEMU.

Nothing here boots the node. The stand-in records its arguments, writes the console line a test
gives it, and then waits to be stopped, as a running machine does, or ends, as one that powers
itself off does.

Run: python3 -I kernel/node/tools/test_node_up.py

It needs a bash with EPOCHREALTIME (5.0 or later), as the scripts do; macOS's /bin/bash is 3.2,
and Homebrew's is found when it is installed. Without one it fails, never skips.
"""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest

TOOLS = Path(__file__).resolve().parent
UP = TOOLS / "node-up.sh"
DOWN = TOOLS / "node-down.sh"

# In place of QEMU: its arguments to a file, then the line STANDIN_LINE to the serial port's
# file, and then it waits to be stopped, or with STANDIN_EXIT set, ends at once.
QEMU = """#!/bin/sh
printf '%s\\n' "$@" > "$(dirname "$0")/arguments"
for argument in "$@"; do
  case "$argument" in file:*) printf '%s\\n' "$STANDIN_LINE" > "${argument#file:}" ;; esac
done
if [ -n "${STANDIN_EXIT:-}" ]; then exit 0; fi
exec sleep 60
"""


def modern_bash():
    for name in ("bash", "/opt/homebrew/bin/bash", "/usr/local/bin/bash", "/bin/bash"):
        path = shutil.which(name)
        if path and subprocess.run([path, "-c", '[ -n "${EPOCHREALTIME:-}" ]'], check=False).returncode == 0:
            return path
    raise AssertionError("no bash with EPOCHREALTIME (5.0 or later) was found: node-up.sh needs one")


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    # A process that has ended and not yet been reaped is not running.
    state = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout
    return bool(state.strip()) and not state.strip().startswith("Z")


class NodeUp(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.bash = modern_bash()

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.out = self.root / "out"
        self.out.mkdir()
        (self.out / "moruna-node.raw").write_bytes(b"disk")
        (self.root / "bin").mkdir()
        (self.root / "share/qemu").mkdir(parents=True)
        for name in ("edk2-x86_64-code.fd", "edk2-i386-vars.fd"):
            (self.root / "share/qemu" / name).write_bytes(b"firmware")
        qemu = self.root / "bin/qemu-system-x86_64"
        qemu.write_text(QEMU)
        qemu.chmod(0o755)
        self.addCleanup(self.down)

    def run_script(self, script, *arguments, line="", extra=None):
        env = {"PATH": f"{self.root / 'bin'}:/usr/bin:/bin", "STANDIN_LINE": line, **(extra or {})}
        return subprocess.run([self.bash, str(script), *arguments], capture_output=True, text=True,
                              env=env, timeout=90, check=False)

    def up(self, *arguments, line="moruna: network wan 169.254.169.15/24 gateway 169.254.169.254", extra=None):
        return self.run_script(UP, str(self.out), *arguments, line=line, extra=extra)

    def down(self):
        if (self.out / "qemu.pid").exists():
            self.run_script(DOWN, str(self.out))

    def arguments(self):
        return (self.root / "bin/arguments").read_text().splitlines()

    def test_it_returns_at_the_network_line_and_leaves_the_machine_running(self):
        result = self.up("2")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('"moruna: network wan 169.254.169.15/24 gateway 169.254.169.254"', result.stderr)
        pid = int((self.out / "qemu.pid").read_text())
        self.assertTrue(alive(pid))
        arguments = self.arguments()
        self.assertEqual(arguments[arguments.index("-smp") + 1], "2")
        self.assertIn(f"file:{self.out / 'console.log'}", arguments)
        # Stopped, it is gone and its process ID with it; its console stays.
        down = self.run_script(DOWN, str(self.out))
        self.assertEqual(down.returncode, 0, down.stderr)
        self.assertFalse((self.out / "qemu.pid").exists())
        self.assertFalse(alive(pid))
        self.assertTrue((self.out / "console.log").exists())
        # Again, there is nothing to stop.
        self.assertEqual(self.run_script(DOWN, str(self.out)).returncode, 0)

    def test_the_network_is_the_checks_one(self):
        self.assertEqual(self.up("1").returncode, 0)
        arguments = self.arguments()
        netdev = arguments[arguments.index("-netdev") + 1]
        self.assertEqual(
            netdev,
            "user,id=net0,net=169.254.169.0/24,host=169.254.169.254,"
            "hostfwd=tcp::8443-:443,hostfwd=tcp::8444-:444",
        )
        self.assertIn(f"filter-dump,id=dump0,netdev=net0,file={self.out / 'net.pcap'}", arguments)
        self.assertIn("virtio-net-pci,netdev=net0,disable-legacy=on", arguments)
        # The machine is uefi-boot.sh's: q35, 2 GiB, the disk read-only, no reboot.
        for expected in ("q35", "2G", "-no-reboot", "-nodefaults",
                         f"if=none,id=boot,format=raw,readonly=on,file={self.out / 'moruna-node.raw'}"):
            self.assertIn(expected, arguments)

    def test_with_no_card_the_machine_has_no_network(self):
        result = self.up("1", extra={"NODE_UP_CARD": "none"})
        self.assertEqual(result.returncode, 0, result.stderr)
        arguments = self.arguments()
        self.assertNotIn("-netdev", arguments)
        self.assertFalse(any(a.startswith("filter-dump") for a in arguments))

    def test_a_fatal_line_and_the_power_off_are_reported(self):
        result = self.up("1", line="moruna: fatal network: no DHCP lease within 30 s",
                         extra={"STANDIN_EXIT": "1"})
        self.assertEqual(result.returncode, 1)
        self.assertIn('"moruna: fatal network: no DHCP lease within 30 s"', result.stderr)
        self.assertIn("powered off", result.stderr)
        self.assertFalse((self.out / "qemu.pid").exists())

    def test_a_fatal_line_without_a_power_off_stops_the_machine(self):
        started = time.monotonic()
        result = self.up("1", line="moruna: fatal network: no DHCP lease within 30 s")
        self.assertEqual(result.returncode, 1)
        self.assertIn("did not power off within 30 s", result.stderr)
        self.assertGreaterEqual(time.monotonic() - started, 29)
        self.assertFalse((self.out / "qemu.pid").exists())

    def test_qemu_ending_first_is_a_failure(self):
        result = self.up("1", line="", extra={"STANDIN_EXIT": "1"})
        self.assertEqual(result.returncode, 1)
        self.assertIn("QEMU ended with status 0", result.stderr)

    def test_a_second_node_from_the_same_directory_is_refused(self):
        self.assertEqual(self.up("1").returncode, 0)
        again = self.up("1")
        self.assertEqual(again.returncode, 2)
        self.assertIn("already up", again.stderr)

    def test_its_arguments_are_checked_before_qemu_starts(self):
        for arguments, text in (((), "usage"), (("0",), "positive number"), (("x",), "positive number"),
                                (("1", "vhd"), "raw or qcow2"), (("1", "qcow2"), "no disk image")):
            result = self.run_script(UP, str(self.out), *arguments)
            self.assertEqual(result.returncode, 2, arguments)
            self.assertIn(text, result.stderr)
        result = self.up("1", extra={"NODE_UP_CARD": "e1000"})
        self.assertEqual(result.returncode, 2)
        self.assertIn("virtio or none", result.stderr)
        self.assertFalse((self.root / "bin/arguments").exists())
        self.assertEqual(self.run_script(DOWN).returncode, 2)


if __name__ == "__main__":
    unittest.main()
