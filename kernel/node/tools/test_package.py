#!/usr/bin/env python3
"""package.py's profiles (work item 1.1): what each takes, what each refuses, what each builds.

Run: python3 -I kernel/node/tools/test_package.py
"""
import contextlib
import importlib.util
import io
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

TOOL = Path(__file__).resolve().parent / "package.py"
SPEC = importlib.util.spec_from_file_location("package", TOOL)
package = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(package)

BASE = ["--description", "d.image", "--assets", "assets", "--out", "out", "--node-memory", str(2 << 30)]
STANDALONE = [*BASE, "--agent-port", "5100", "--cpus", "2"]


def refused(argv):
    """The exit status and message of a command line the parser refuses."""
    error = io.StringIO()
    with contextlib.redirect_stderr(error), unittest.TestCase().assertRaises(SystemExit) as stop:
        package.arguments(argv)
    return stop.exception.code, error.getvalue()


class Profiles(unittest.TestCase):
    def test_an_unknown_profile_is_refused(self):
        code, message = refused(["--profile", "azure", *STANDALONE])
        self.assertEqual(code, 2)
        self.assertIn("invalid choice: 'azure'", message)

    def test_standalone_is_the_default_and_unchanged(self):
        _, default = package.arguments(STANDALONE)
        _, named = package.arguments(["--profile", "standalone", *STANDALONE])
        self.assertEqual(default.profile, "standalone")
        self.assertEqual(vars(default), vars(named))
        self.assertEqual((default.cid, default.agent_port, default.cpus), (3, 5100, 2))
        self.assertEqual(package.PROGRAMS["standalone"], [
            "moruna-node-supervisor", "node-admission", "moruna-storage-service", "devices", "imagefs", "linux",
            "netstack", "engine", "virtio-blk", "virtio-vsock", "virtio-net", "nvme", "gve"])

    def test_standalone_still_needs_its_agent_port_and_cpus(self):
        code, message = refused(BASE)
        self.assertEqual(code, 2)
        self.assertIn("the following arguments are required: --agent-port, --cpus", message)
        code, message = refused([*BASE, "--cpus", "2"])
        self.assertIn("required: --agent-port", message)

    def test_standalone_refuses_what_it_refused(self):
        for extra in (["--cid", "2"], ["--cid", str(1 << 32)], ["--agent-port", "0"]):
            argv = [*BASE, "--agent-port", "5100", "--cpus", "2", *extra]
            code, message = refused(argv)
            self.assertEqual(code, 2)
            self.assertIn("invalid guest CID or layer count", message)
        code, message = refused([*STANDALONE, *[w for i in range(17) for w in ("--layer", f"l{i}")]])
        self.assertIn("invalid guest CID or layer count", message)

    def test_cloud_takes_only_its_own_arguments(self):
        _, args = package.arguments(["--profile", "cloud", *BASE])
        self.assertEqual(args.profile, "cloud")
        self.assertEqual((args.agent_port, args.cpus, args.cid, args.layer, args.writable_home, args.user_network),
                         (None, None, None, [], [], False))

    def test_cloud_refuses_every_launcher_flag(self):
        for extra in (["--agent-port", "5100"], ["--cpus", "2"], ["--layer", "l.img"],
                      ["--writable-home", "h.raw"], ["--user-network"], ["--cid", "3"]):
            code, message = refused(["--profile", "cloud", *BASE, *extra])
            self.assertEqual(code, 2, extra)
            self.assertIn(f"the cloud profile takes no {extra[0]}", message)
        code, message = refused(["--profile", "cloud", *BASE, "--cpus", "2", "--user-network"])
        self.assertIn("takes no --cpus, --user-network", message)

    def test_cloud_still_needs_its_node_memory(self):
        code, message = refused(["--profile", "cloud", *BASE[:-2]])
        self.assertEqual(code, 2)
        self.assertIn("--node-memory", message)

    def test_cloud_builds_only_its_programs(self):
        cloud = package.PROGRAMS["cloud"]
        self.assertEqual(cloud, ["moruna-node-supervisor", "devices", "virtio-blk", "virtio-net", "nvme", "gve"])
        for absent in ("virtio-vsock", "imagefs", "linux", "node-admission", "moruna-storage-service", "engine", "netstack"):
            self.assertNotIn(absent, cloud)

    def test_the_command_refuses_before_building_anything(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            for argv, text in ((["--profile", "cloud", *BASE[:4], "--out", str(out), "--node-memory", "1", "--agent-port", "1"],
                                "the cloud profile takes no --agent-port"),
                               (["--profile", "bios", *STANDALONE], "invalid choice: 'bios'")):
                result = subprocess.run([sys.executable, "-I", str(TOOL), *argv], capture_output=True, text=True, cwd=tmp)
                self.assertEqual(result.returncode, 2)
                self.assertIn(text, result.stderr)
                self.assertFalse(out.exists())


if __name__ == "__main__":
    unittest.main()
