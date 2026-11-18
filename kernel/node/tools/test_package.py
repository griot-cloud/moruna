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
STANDALONE = [*BASE, "--agent-port", "5100", "--cpus", "2", "--profile-json", "profile.json"]


def refused(argv):
    """The exit status and message of a command line the parser refuses."""
    error = io.StringIO()
    with contextlib.redirect_stderr(error), unittest.TestCase().assertRaises(SystemExit) as stop:
        package.arguments(argv)
    return stop.exception.code, error.getvalue()


def first_builds(test, profile):
    """main() for `profile`, run up to its second build with nothing built: the kernel's build
    and the user programs' build, each as its arguments and environment."""
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
        description.write_text("moruna-image 1\nprocess node-supervisor\n  handle data:node.conf config\n")
        argv = ["package.py", "--profile", profile, "--description", str(description), "--assets", str(assets),
                "--out", str(Path(tmp) / "out"), "--node-memory", str(2 << 30)]
        with mock.patch.object(package, "command", record), \
                mock.patch.object(package.shutil, "which", lambda tool: f"/usr/bin/{tool}"), \
                mock.patch.object(sys, "argv", argv), mock.patch.dict(os.environ):
            os.environ.pop("CC_x86_64_moruna_user", None)
            os.environ["CFLAGS_x86_64_moruna_user"] = "-O1"
            with test.assertRaises(Stop):
                package.main()
    return builds


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
        self.assertIn("the following arguments are required: --agent-port, --cpus, --profile-json", message)
        code, message = refused([*BASE, "--cpus", "2", "--profile-json", "p.json"])
        self.assertIn("required: --agent-port", message)
        code, message = refused([*BASE, "--cpus", "2", "--agent-port", "5100"])
        self.assertIn("required: --profile-json", message)

    def test_standalone_refuses_what_it_refused(self):
        for extra in (["--cid", "2"], ["--cid", str(1 << 32)], ["--agent-port", "0"]):
            argv = [*STANDALONE, *extra]
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
                      ["--writable-home", "h.raw"], ["--user-network"], ["--cid", "3"], ["--profile-json", "p.json"]):
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
        # Step 1's processes (system design 1A): the network server, api and platform beside
        # the supervisor, the device manager and its drivers (items 1.1, 1.3 and 1.4), and the
        # VMBus processes, which the supervisor starts on Hyper-V only (1F).
        self.assertEqual(cloud, ["moruna-node-supervisor", "devices", "netstack", "moruna-node-api",
                                 "moruna-node-platform", "virtio-blk", "virtio-net", "nvme", "gve",
                                 "moruna-vmbus-server", "moruna-storvsc-server", "moruna-netvsc-server"])
        for absent in ("virtio-vsock", "imagefs", "linux", "node-admission", "moruna-storage-service", "engine",
                       "storvsc-check"):
            self.assertNotIn(absent, cloud)

    def test_the_check_variant_is_the_cloud_profile_and_the_check_alone(self):
        """Item 1.10b's Azure check: the cloud profile's description and assets, with
        `storvsc-check`'s program, service and directory link added and nothing else."""
        node = TOOL.parents[1]

        def records(path):
            return [line for line in (node / path).read_text().splitlines()
                    if line.strip() and not line.lstrip().startswith("#")]

        check = records("cloud-check.image")
        self.assertEqual([line for line in check if "storvsc-check" not in line], records("cloud.image"))
        self.assertIn("  handle program:storvsc-check image.storvsc-check", check)
        self.assertEqual(records("cloud-check/node.conf"),
                         [*records("cloud/node.conf"), "service storvsc-check image.storvsc-check 33554432",
                          "link devices directory-check storvsc-check directory"])
        # Its assets are the cloud profile's but for node.conf: the same roots, byte for byte.
        self.assertEqual(sorted(p.name for p in (node / "cloud-check").iterdir()),
                         sorted(p.name for p in (node / "cloud").iterdir()))
        self.assertEqual((node / "cloud-check/tls.roots").read_bytes(), (node / "cloud/tls.roots").read_bytes())
        self.assertEqual((node / "cloud-check/network.conf").read_bytes(), (node / "cloud/network.conf").read_bytes())
        self.assertEqual((node / "cloud-check/network.conf").read_text(), (node / "cloud/network.conf").read_text())

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
        (kernel_args, kernel_env), (user_args, user_env) = first_builds(self, "cloud")
        self.assertIn("moruna-microkernel-boot", kernel_args)
        self.assertNotIn("CC_x86_64_moruna_user", kernel_env)
        self.assertEqual(kernel_env["CFLAGS_x86_64_moruna_user"], "-O1")
        self.assertIn("-Zjson-target-spec", user_args)
        self.assertEqual([user_args[i + 1] for i, word in enumerate(user_args) if word == "-p"], package.PROGRAMS["cloud"])
        self.assertEqual(user_env["CC_x86_64_moruna_user"], "clang")
        self.assertEqual(user_env["CFLAGS_x86_64_moruna_user"],
                         "--target=x86_64-unknown-none -ffreestanding -mno-red-zone -fno-stack-protector -O1")
        self.assertIn("-C link-arg=-static", user_env["RUSTFLAGS"])

    def payload_of(self, argv):
        """main(), stopped at its first build: the files it put in the payload, by relative path,
        and the description it rendered, if it rendered one."""
        class Stop(Exception):
            pass

        def stop(args, cwd, env=None):
            raise Stop

        out = Path(argv[argv.index("--out") + 1])
        with mock.patch.object(package, "command", stop), \
                mock.patch.object(package.shutil, "which", lambda tool: f"/usr/bin/{tool}"), \
                mock.patch.object(sys, "argv", ["package.py", *argv]):
            with self.assertRaises(Stop):
                package.main()
        payload = out / "payload"
        files = sorted(p.relative_to(payload).as_posix() for p in payload.rglob("*") if p.is_file())
        rendered = out / "standalone.image"
        return files, rendered.read_text() if rendered.exists() else None

    def test_the_committed_cloud_profile_s_assets_are_exactly_what_its_image_binds(self):
        """kernel/node/cloud/ holds what ships; the roots' source and record are kept apart."""
        kernel = TOOL.parents[2]
        assets = kernel / "node/cloud"
        self.assertEqual(sorted(p.name for p in assets.iterdir()), ["network.conf", "node.conf", "tls.roots"])
        with tempfile.TemporaryDirectory() as tmp:
            files, rendered = self.payload_of(
                ["--profile", "cloud", "--description", str(kernel / "node/cloud.image"), "--assets", str(assets),
                 "--out", str(Path(tmp) / "out"), "--node-memory", str(2 << 30)])
        self.assertEqual(files, ["network.conf", "node.conf", "tls.roots"])
        self.assertIsNone(rendered)

    def test_a_launcher_s_node_carries_its_bound_files_its_rendered_ones_and_its_layers(self):
        with tempfile.TemporaryDirectory() as tmp:
            assets = Path(tmp) / "assets"
            (assets / "layers").mkdir(parents=True)
            for name in ("node.conf", "storage.credential", "layers/layer-1.img"):
                (assets / name).write_text("x\n")
            # The bucket provider binds the credential (package.py's rendering of configure.py's profile,
            # which is kept beside the assets).
            (Path(tmp) / "profile.json").write_text('{"storage_provider": "bucket"}\n')
            description = Path(tmp) / "node.image"
            description.write_text("moruna-image 1\nprocess node-supervisor\n  handle data:node.conf config\n")
            files, rendered = self.payload_of(
                ["--description", str(description), "--assets", str(assets), "--out", str(Path(tmp) / "out"),
                 "--node-memory", str(2 << 30), "--agent-port", "5100", "--cpus", "2",
                 "--profile-json", str(Path(tmp) / "profile.json"), "--layer", "layers/layer-1.img"])
        self.assertEqual(files, ["layers/layer-1.img", "node.conf", "storage.credential"])
        self.assertIn("  handle data:storage.credential storage.credential\n", rendered)

    def test_an_asset_the_description_does_not_bind_is_refused_by_name(self):
        """Nothing is left out unseen: a file the image would not carry is refused before anything
        is built, whether it is a stray file, an unnamed layer or a config with no recipient."""
        with tempfile.TemporaryDirectory() as tmp:
            for profile, extra in (("cloud", ["cacert.pem", "tls-roots.txt"]),
                                   ("standalone", ["layers/other.img", "maintenance.conf", "profile.json"])):
                assets = Path(tmp) / profile / "assets"
                (assets / "layers").mkdir(parents=True)
                for name in ("node.conf", *extra):
                    (assets / name).write_text("x\n")
                (Path(tmp) / profile / "profile.json").write_text("{}\n")
                description = Path(tmp) / profile / "node.image"
                description.write_text("moruna-image 1\nprocess node-supervisor\n  handle data:node.conf config\n")
                out = Path(tmp) / profile / "out"
                argv = ["package.py", "--description", str(description), "--assets", str(assets), "--out", str(out),
                        "--node-memory", str(2 << 30)]
                argv += ["--profile", "cloud"] if profile == "cloud" else [
                    "--agent-port", "5100", "--cpus", "2", "--profile-json", str(Path(tmp) / profile / "profile.json")]
                built = []
                error = io.StringIO()
                with mock.patch.object(package, "command", lambda *a, **k: built.append(a)), \
                        mock.patch.object(package.shutil, "which", lambda tool: f"/usr/bin/{tool}"), \
                        mock.patch.object(sys, "argv", argv), contextlib.redirect_stderr(error), \
                        self.assertRaises(SystemExit):
                    package.main()
                self.assertIn(f"assets the description does not bind: {', '.join(sorted(extra))}", error.getvalue())
                self.assertEqual(built, [])
                self.assertFalse(out.exists())

    def test_a_launcher_s_node_whose_profile_cannot_be_read_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            assets = Path(tmp) / "assets"
            assets.mkdir()
            description = Path(tmp) / "node.image"
            description.write_text("moruna-image 1\n")
            out = Path(tmp) / "out"
            argv = ["package.py", "--description", str(description), "--assets", str(assets), "--out", str(out),
                    "--node-memory", str(2 << 30), "--agent-port", "5100", "--cpus", "2",
                    "--profile-json", str(Path(tmp) / "none.json")]
            error = io.StringIO()
            with mock.patch.object(package.shutil, "which", lambda tool: f"/usr/bin/{tool}"), \
                    mock.patch.object(sys, "argv", argv), contextlib.redirect_stderr(error), \
                    self.assertRaises(SystemExit):
                package.main()
            self.assertIn("the profile ", error.getvalue())
            self.assertIn("none.json", error.getvalue())
            self.assertFalse(out.exists())

    def test_data_the_description_binds_and_the_assets_lack_is_refused_before_anything(self):
        with tempfile.TemporaryDirectory() as tmp:
            assets = Path(tmp) / "assets"
            assets.mkdir()
            (assets / "node.conf").write_text("moruna-node 1\n")
            description = Path(tmp) / "cloud.image"
            description.write_text("moruna-image 1\nprocess node-supervisor\n  handle data:node.conf config\n"
                                   "  handle data:tls.roots tls.roots\n")
            out = Path(tmp) / "out"
            argv = ["package.py", "--profile", "cloud", "--description", str(description), "--assets", str(assets),
                    "--out", str(out), "--node-memory", str(2 << 30)]
            built = []
            error = io.StringIO()
            with mock.patch.object(package, "command", lambda *a, **k: built.append(a)), \
                    mock.patch.object(package.shutil, "which", lambda tool: f"/usr/bin/{tool}"), \
                    mock.patch.object(sys, "argv", argv), contextlib.redirect_stderr(error), \
                    self.assertRaises(SystemExit):
                package.main()
            self.assertIn("the description binds data the assets do not hold: tls.roots", error.getvalue())
            self.assertEqual(built, [])
            self.assertFalse(out.exists())
        self.assertEqual(package.bound("moruna-image 1\n  handle data:a.conf x\n  handle program:b image.b\n"
                                       "  handle data:ke2/roots tls-roots\n  # handle data:c.conf y\n"),
                         {"a.conf", "ke2/roots"})

    def test_cloud_test_is_the_cloud_profile_for_tests_only(self):
        """Work item 2.2c: the cloud's arguments and programs, its refusals named for it."""
        _, args = package.arguments(["--profile", "cloud-test", *BASE])
        self.assertEqual(args.profile, "cloud-test")
        self.assertEqual(package.PROGRAMS["cloud-test"], package.PROGRAMS["cloud"])
        for extra in (["--agent-port", "5100"], ["--cpus", "2"], ["--layer", "l.img"]):
            code, message = refused(["--profile", "cloud-test", *BASE, *extra])
            self.assertEqual(code, 2, extra)
            self.assertIn(f"the cloud-test profile takes no {extra[0]}", message)

    def test_the_release_cloud_profile_builds_api_without_its_test_route(self):
        """Work item 2.2c: api's feature `test-exit`, its route to /v1/test/exit, is turned on
        by cloud-test's user build alone; cloud's turns on no feature at all."""
        self.assertEqual([profile for profile, named in package.FEATURES.items()
                          if any("test-exit" in feature for feature in named)], ["cloud-test"])
        _, (release, _) = first_builds(self, "cloud")
        self.assertNotIn("--features", release)
        self.assertFalse(any("test-exit" in word for word in release), release)
        _, (test, _) = first_builds(self, "cloud-test")
        self.assertEqual(test[test.index("--features") + 1], "moruna-node-api/test-exit")
        self.assertEqual([test[i + 1] for i, word in enumerate(test) if word == "-p"], package.PROGRAMS["cloud"])

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
