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
import shutil
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
        # Step 1's processes (system design 1A): the network server, api and platform beside
        # the supervisor, the device manager and its drivers (items 1.1, 1.3 and 1.4), and the
        # VMBus processes, which the supervisor starts on Hyper-V only (1F).
        self.assertEqual(cloud, ["moruna-node-supervisor", "devices", "netstack", "moruna-node-api",
                                 "moruna-node-platform", "virtio-blk", "virtio-net", "nvme", "gve",
                                 "moruna-vmbus-server", "moruna-storvsc-server"])
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



class DiskGuid(unittest.TestCase):
    """The image's own GPT disk GUID (system design 3C, item 3.3)."""

    def files(self, tmp, *contents):
        paths = []
        for i, data in enumerate(contents):
            path = Path(tmp) / f"f{i}"
            path.write_bytes(data)
            paths.append(path)
        return paths

    def test_the_same_content_gives_the_same_guid_and_other_content_another(self):
        with tempfile.TemporaryDirectory() as tmp:
            one = package.disk_guid(self.files(tmp, b"kernel", b"initramfs"))
            again = package.disk_guid(self.files(tmp, b"kernel", b"initramfs"))
            self.assertEqual(one, again)
            self.assertNotEqual(one, package.disk_guid(self.files(tmp, b"kernel", b"initramfs!")))
            # The files' bounds count: the same bytes split otherwise are another image.
            self.assertNotEqual(one, package.disk_guid(self.files(tmp, b"kerneli", b"nitramfs")))
            self.assertNotEqual(one, package.PLACEHOLDER)

    def test_the_guid_is_sgdisks_text_form_of_a_version_8_guid(self):
        with tempfile.TemporaryDirectory() as tmp:
            guid = package.disk_guid(self.files(tmp, b"x"))
        self.assertRegex(guid, r"^[0-9A-F]{8}-[0-9A-F]{4}-8[0-9A-F]{3}-[89AB][0-9A-F]{3}-[0-9A-F]{12}$")

    def test_the_payload_names_it_and_the_cloud_supervisor_passes_it_to_devices_and_storvsc(self):
        node = TOOL.parents[1]
        for profile in ("cloud", "cloud-check"):
            image = (node / f"{profile}.image").read_text()
            self.assertIn(f"  handle data:{package.DISK_GUID} disk.guid\n", image)
            # A slot is granted to one service: storvsc's is a second binding of the same file.
            self.assertIn(f"  handle data:{package.DISK_GUID} disk.guid.storvsc\n", image)
            conf = (node / profile / "node.conf").read_text()
            self.assertIn("grant devices disk.guid disk.guid\n", conf)
            self.assertIn("grant storvsc disk.guid disk.guid.storvsc\n", conf)



def a_layer(directory, pages=3, seed=1):
    """A file shaped as a layer: the magic, then `pages` pages of bytes."""
    path = Path(directory) / f"layer-{seed}.img"
    body = bytes((i * 7 + seed) % 251 for i in range(pages * package.PAGE))
    path.write_bytes(package.LAYER_MAGIC + body[len(package.LAYER_MAGIC):])
    return path


class SystemPartition(unittest.TestCase):
    """The boot disk's moruna-system partition and the payload's linux.layers (item 3.3)."""

    def test_a_layer_is_named_by_its_seal_and_anything_else_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            layer = a_layer(tmp)
            seal = package.system_layer(layer)
            header = layer.read_bytes()[:package.PAGE]
            import hashlib
            self.assertEqual(seal, hashlib.sha256(header).hexdigest())
            self.assertEqual(package.linux_layers(seal), f"moruna-layers 1\nlayer system {seal}\n")
            for name, data in (("short", package.LAYER_MAGIC), ("other", b"x" * package.PAGE),
                               ("empty", b"")):
                path = Path(tmp) / name
                path.write_bytes(data)
                with self.assertRaises(ValueError):
                    package.system_layer(path)
            with self.assertRaises(ValueError):
                package.system_layer(Path(tmp))
            link = Path(tmp) / "link"
            link.symlink_to(layer)
            with self.assertRaises(ValueError):
                package.system_layer(link)

    def test_the_layout_puts_the_system_partition_after_the_esp_in_whole_mib(self):
        bare = package.layout(64, 0)
        self.assertEqual((bare["disk_mib"], bare["system_mib"], bare["system_sector"]), (68, 0, None))
        self.assertEqual(bare["sgdisk"], ["--new=1:2048:+64M", "--typecode=1:ef00"])
        with_layer = package.layout(64, 5 * package.MIB + 1)
        self.assertEqual(with_layer["system_mib"], 6)
        self.assertEqual(with_layer["disk_mib"], 74)
        self.assertEqual(with_layer["system_sector"], 2048 + 64 * 2048)
        self.assertEqual(with_layer["sgdisk"][2:], ["--new=2:133120:+6M", "--typecode=2:8300",
                                                    "--change-name=2:moruna-system"])

    def assembled(self, tmp, layer, run):
        esp = Path(tmp) / "esp.img"
        esp.write_bytes(b"E" * package.MIB)
        disk = Path(tmp) / "disk.raw"
        planned = package.assemble(disk, esp, layer, "8A1F-TEST", 2, Path(tmp), run=run)
        return disk, planned

    def test_the_payloads_seal_is_the_partitions_layer(self):
        with tempfile.TemporaryDirectory() as tmp:
            layer = a_layer(tmp, pages=5)
            seen = []
            disk, planned = self.assembled(tmp, layer, lambda args, cwd: seen.append(args))
            self.assertEqual(seen[0][:3], ["sgdisk", "--clear", "--disk-guid=8A1F-TEST"])
            self.assertIn("--change-name=2:moruna-system", seen[0])
            data = disk.read_bytes()
            self.assertEqual(len(data), planned["disk_mib"] * package.MIB)
            self.assertEqual(data[2048 * 512:2048 * 512 + package.MIB], b"E" * package.MIB)
            at = planned["system_sector"] * 512
            self.assertEqual(data[at:at + layer.stat().st_size], layer.read_bytes())
            # The seal linux.layers names is the partition's layer's: its header page's SHA-256.
            import hashlib
            on_disk = hashlib.sha256(data[at:at + package.PAGE]).hexdigest()
            named = package.linux_layers(package.system_layer(layer)).split()[-1]
            self.assertEqual(named, on_disk)
            # Another layer, another seal.
            self.assertNotEqual(package.system_layer(a_layer(tmp, seed=2)), named)

    def test_without_a_layer_the_disk_is_its_esp_alone(self):
        with tempfile.TemporaryDirectory() as tmp:
            seen = []
            disk, planned = self.assembled(tmp, None, lambda args, cwd: seen.append(args))
            self.assertNotIn("--change-name=2:moruna-system", seen[0])
            self.assertEqual(disk.stat().st_size, 6 * package.MIB)

    @unittest.skipUnless(shutil.which("sgdisk"), "sgdisk is not installed")
    def test_sgdisk_writes_the_partition_the_layer_is_in(self):
        import struct, zlib
        with tempfile.TemporaryDirectory() as tmp:
            layer = a_layer(tmp, pages=300)
            disk, planned = self.assembled(tmp, layer, package.command)
            data = disk.read_bytes()
            header = data[512:1024]
            self.assertEqual(header[:8], b"EFI PART")
            lba, count, size = struct.unpack_from("<QII", header, 72)
            table = data[lba * 512:lba * 512 + count * size]
            self.assertEqual(zlib.crc32(table) & 0xffffffff, struct.unpack_from("<I", header, 88)[0])
            names = {}
            for i in range(count):
                entry = table[i * size:(i + 1) * size]
                first, last = struct.unpack_from("<QQ", entry, 32)
                name = entry[56:128].decode("utf-16-le").rstrip("\0")
                if name:
                    names[name] = (first, last)
            first, last = names["moruna-system"]
            self.assertEqual(first, planned["system_sector"])
            self.assertEqual(last - first + 1, planned["system_mib"] * 2048)
            self.assertEqual(data[first * 512:first * 512 + layer.stat().st_size], layer.read_bytes())

    def test_a_description_that_binds_linux_layers_needs_the_flag_before_anything_is_built(self):
        self.assertTrue(package.binds("process x\n  handle data:linux.layers layers\n", "linux.layers"))
        self.assertFalse(package.binds("process x\n  handle data:linux.layers.old layers\n", "linux.layers"))
        self.assertFalse(package.binds("# handle data:linux.layers layers\n", "linux.layers"))
        with tempfile.TemporaryDirectory() as tmp:
            description = Path(tmp) / "d.image"
            description.write_text("moruna-image 1\nprocess node-supervisor\n"
                                   "  handle data:linux.layers linux.layers\n")
            (Path(tmp) / "assets").mkdir()
            out = Path(tmp) / "out"
            argv = ["--profile", "cloud", "--description", str(description), "--assets", "assets",
                    "--out", str(out), "--node-memory", str(2 << 30)]
            result = subprocess.run([sys.executable, "-I", str(TOOL), *argv], capture_output=True, text=True,
                                    cwd=tmp)
            self.assertEqual(result.returncode, 2)
            self.assertIn("this description binds linux.layers: pass --system-layer", result.stderr)
            self.assertFalse(out.exists(), "refused before anything is built")

    def test_the_command_takes_a_system_layer(self):
        _, args = package.arguments(["--profile", "cloud", *BASE, "--system-layer", "engine.layer"])
        self.assertEqual(args.system_layer, Path("engine.layer"))


if __name__ == "__main__":
    unittest.main()
