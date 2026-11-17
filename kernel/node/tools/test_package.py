#!/usr/bin/env python3
"""package.py's profiles (work item 1.1): what each takes, what each refuses, what each builds.

Run: python3 -I kernel/node/tools/test_package.py
"""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

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

    def test_the_user_build_compiles_c_as_build_sh_does(self):
        flags = "--target=x86_64-unknown-none -ffreestanding -mno-red-zone -fno-stack-protector"
        # Nothing set by the caller: clang, and build.sh's flags.
        self.assertEqual(package.user_c({}), {"CC_x86_64_moruna_user": "clang", "CFLAGS_x86_64_moruna_user": flags})
        self.assertEqual(package.user_c({"CC_x86_64_moruna_user": "", "CFLAGS_x86_64_moruna_user": " "}),
                         {"CC_x86_64_moruna_user": "clang", "CFLAGS_x86_64_moruna_user": flags})
        # The caller's compiler is kept, and its flags follow build.sh's.
        self.assertEqual(package.user_c({"CC_x86_64_moruna_user": "clang-18", "CFLAGS_x86_64_moruna_user": "-O1", "CC": "gcc"}),
                         {"CC_x86_64_moruna_user": "clang-18", "CFLAGS_x86_64_moruna_user": f"{flags} -O1"})
        # The same two variables, and the same values, as kernel/tools/build.sh gives its user build.
        build = (TOOL.parents[2] / "tools/build.sh").read_text()
        self.assertIn('user_cc="${CC_x86_64_moruna_user:-clang}"', build)
        self.assertIn('CC_x86_64_moruna_user="$user_cc" \\\n', build)
        self.assertIn(f'CFLAGS_x86_64_moruna_user="{flags} ${{CFLAGS_x86_64_moruna_user:-}}" \\\n', build)

    def test_the_user_build_alone_gets_the_c_compiler(self):
        """main(), run up to its second build with nothing built: the kernel's build has no C
        compiler set, and the user programs' build has build.sh's."""
        class Stop(Exception):
            pass

        builds = []

        def record(args, cwd, env=None):
            builds.append(([str(a) for a in args], dict(env)))
            if len(builds) == 2:
                raise Stop

        with tempfile.TemporaryDirectory() as tmp:
            assets = Path(tmp) / "assets"
            assets.mkdir()
            (assets / "node.conf").write_text("moruna-node 1\n")
            description = Path(tmp) / "cloud.image"
            description.write_text("moruna-image 1\n")
            argv = ["package.py", "--profile", "cloud", "--description", str(description), "--assets", str(assets),
                    "--out", str(Path(tmp) / "out"), "--node-memory", str(2 << 30)]
            with mock.patch.object(package, "command", record), \
                    mock.patch.object(package.shutil, "which", lambda tool: f"/usr/bin/{tool}"), \
                    mock.patch.object(sys, "argv", argv), mock.patch.dict(os.environ):
                os.environ.pop("CC_x86_64_moruna_user", None)
                os.environ["CFLAGS_x86_64_moruna_user"] = "-O1"
                with self.assertRaises(Stop):
                    package.main()
        (kernel_args, kernel_env), (user_args, user_env) = builds
        self.assertIn("moruna-microkernel-boot", kernel_args)
        self.assertNotIn("CC_x86_64_moruna_user", kernel_env)
        self.assertEqual(kernel_env["CFLAGS_x86_64_moruna_user"], "-O1")
        self.assertIn("-Zjson-target-spec", user_args)
        self.assertEqual([user_args[i + 1] for i, word in enumerate(user_args) if word == "-p"], package.PROGRAMS["cloud"])
        self.assertEqual(user_env["CC_x86_64_moruna_user"], "clang")
        self.assertEqual(user_env["CFLAGS_x86_64_moruna_user"],
                         "--target=x86_64-unknown-none -ffreestanding -mno-red-zone -fno-stack-protector -O1")
        self.assertIn("-C link-arg=-static", user_env["RUSTFLAGS"])

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
