#!/usr/bin/env python3
"""The disk's formats (work item 1.5): package.py --formats and uefi-boot.sh's FORMAT.

Nothing here builds or boots the node's image. The conversions are made from small stand-in
raw files, and the boot script is run against a stand-in for QEMU that records its arguments.

Run: python3 -I kernel/node/tools/test_formats.py

Two of its classes need more than Python, and fail (they are never skipped) without it:
- `Conversions` needs `qemu-img` on PATH, the tool package.py --formats runs. In moruna-arm:
  `export PATH=/opt/qemu/bin:$PATH` (kernel/node/HOST.md).
- `BootDisk` needs a bash with EPOCHREALTIME (5.0 or later), which uefi-boot.sh itself needs.
  macOS's /bin/bash is 3.2; Homebrew's is found when it is installed.
"""
import contextlib
import hashlib
import importlib.util
import io
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest

TOOLS = Path(__file__).resolve().parent
TOOL = TOOLS / "package.py"
BOOT = TOOLS / "uefi-boot.sh"
SPEC = importlib.util.spec_from_file_location("package", TOOL)
package = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(package)

MIB = 1024 * 1024
BASE = ["--description", "d.image", "--assets", "assets", "--out", "out", "--node-memory", str(2 << 30)]
STANDALONE = [*BASE, "--agent-port", "5100", "--cpus", "2"]
CLOUD = ["--profile", "cloud", *BASE]
# The VHD footer's fields (Microsoft's Virtual Hard Disk Image Format Specification, "Hard Disk
# Footer Format"): 512 bytes, big-endian, at the end of the file.
FOOTER = 512
FIXED = 2


def refused(argv):
    """The exit status and message of a command line the parser refuses."""
    error = io.StringIO()
    with contextlib.redirect_stderr(error), unittest.TestCase().assertRaises(SystemExit) as stop:
        package.arguments(argv)
    return stop.exception.code, error.getvalue()


def stand_in(directory, size):
    """A raw file of `size` bytes with no zero byte in it, so padding and data can't be confused."""
    disk = Path(directory) / "moruna-node.raw"
    disk.write_bytes(bytes(1 + index % 251 for index in range(1021)) * (size // 1021 + 1))
    os.truncate(disk, size)
    return disk


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def footer(vhd):
    """The fields of a VHD's footer this item fixes, its checksum verified."""
    raw = vhd.read_bytes()[-FOOTER:]
    cookie, original, current, kind, checksum = (
        raw[0:8], *struct.unpack(">QQ", raw[40:56]), *struct.unpack(">II", raw[60:68]))
    summed = ~sum(raw[:64] + raw[68:]) & 0xffffffff
    return {"cookie": cookie, "original": original, "current": current, "type": kind,
            "checksum_ok": checksum == summed}


class Arguments(unittest.TestCase):
    def test_a_build_without_formats_writes_only_the_raw_disk(self):
        for argv in (STANDALONE, CLOUD):
            _, args = package.arguments(argv)
            self.assertEqual(args.formats, ["raw"])

    def test_formats_takes_a_list_for_either_profile(self):
        for argv in (STANDALONE, CLOUD):
            _, args = package.arguments([*argv, "--formats", "raw", "qcow2", "vhd"])
            self.assertEqual(args.formats, ["raw", "qcow2", "vhd"])
            _, args = package.arguments(["--formats", "vhd", *argv])
            self.assertEqual(args.formats, ["vhd"])
        self.assertEqual(package.FORMATS, ("raw", "qcow2", "vhd"))

    def test_an_unknown_format_is_refused(self):
        for name in ("vmdk", "vpc", "VHD", "qcow2,vhd"):
            code, message = refused([*CLOUD, "--formats", "qcow2", name])
            self.assertEqual(code, 2, name)
            self.assertIn(f"argument --formats: invalid choice: '{name}'", message)

    def test_formats_needs_at_least_one(self):
        code, message = refused([*CLOUD, "--formats"])
        self.assertEqual(code, 2)
        self.assertIn("argument --formats: expected at least one argument", message)

    def test_the_command_refuses_before_building_anything(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            out, assets, description, stubs = tmp / "out", tmp / "assets", tmp / "d.image", tmp / "bin"
            assets.mkdir()
            description.write_text("moruna-image 1\n")
            argv = ["--profile", "cloud", "--description", str(description), "--assets", str(assets),
                    "--out", str(out), "--node-memory", str(2 << 30), "--formats"]
            result = subprocess.run([sys.executable, "-I", str(TOOL), *argv, "raw", "vhdx"],
                                    capture_output=True, text=True, cwd=tmp, check=False)
            self.assertEqual(result.returncode, 2)
            self.assertIn("invalid choice: 'vhdx'", result.stderr)
            self.assertFalse(out.exists())
            # Every other tool on PATH and no qemu-img: a format that needs it is refused first.
            stubs.mkdir()
            for name in ("cargo", "rustc", "grub-mkstandalone", "sgdisk", "mformat", "mmd", "mcopy"):
                (stubs / name).write_text("#!/bin/sh\nexit 1\n")
                (stubs / name).chmod(0o755)
            for formats in (["qcow2"], ["raw", "vhd"]):
                result = subprocess.run([sys.executable, "-I", str(TOOL), *argv, *formats], capture_output=True,
                                        text=True, cwd=tmp, env={"PATH": str(stubs)}, check=False)
                self.assertEqual(result.returncode, 2, formats)
                self.assertIn("required tool unavailable: qemu-img", result.stderr)
                self.assertFalse(out.exists())
            # The raw disk alone needs no qemu-img: the build starts, and stops at the stub cargo.
            result = subprocess.run([sys.executable, "-I", str(TOOL), *argv, "raw"], capture_output=True,
                                    text=True, cwd=tmp, env={"PATH": str(stubs)}, check=False)
            self.assertNotIn("required tool unavailable", result.stderr)
            self.assertIn("CalledProcessError", result.stderr)
            self.assertTrue((out / "payload").is_dir())


class Commands(unittest.TestCase):
    """What package.py asks of qemu-img, with a recorder standing in for it."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.calls = []

    def run_recorded(self, args, cwd):
        """Record a command and, for a conversion, what its source held when it ran."""
        source = Path(args[-2])
        self.calls.append({"args": [str(a) for a in args], "cwd": cwd,
                           "source_size": source.stat().st_size, "source": source.read_bytes()})
        Path(args[-1]).write_bytes(b"converted")

    def test_whole_mib_rounds_up(self):
        for size, rounded in ((0, 0), (1, MIB), (MIB - 1, MIB), (MIB, MIB), (MIB + 1, 2 * MIB),
                              (68 * MIB, 68 * MIB), (68 * MIB + 512, 69 * MIB)):
            self.assertEqual(package.whole_mib(size), rounded, size)

    def test_raw_is_the_disk_itself(self):
        disk = stand_in(self.tmp.name, 4096)
        self.assertEqual(package.convert(disk, ["raw"], self.run_recorded), [])
        self.assertEqual(self.calls, [])
        self.assertEqual(sorted(p.name for p in disk.parent.iterdir()), ["moruna-node.raw"])

    def test_qcow2_is_one_conversion_of_the_raw_disk(self):
        disk = stand_in(self.tmp.name, 4096)
        self.assertEqual(package.convert(disk, ["qcow2"], self.run_recorded), [disk.with_name("moruna-node.qcow2")])
        self.assertEqual([call["args"] for call in self.calls], [[
            "qemu-img", "convert", "-f", "raw", "-O", "qcow2", str(disk), str(disk.with_name("moruna-node.qcow2"))]])
        self.assertEqual(self.calls[0]["cwd"], disk.parent)

    def test_a_whole_mib_disk_becomes_the_vhd_directly(self):
        disk = stand_in(self.tmp.name, 2 * MIB)
        self.assertEqual(package.convert(disk, ["vhd"], self.run_recorded), [disk.with_name("moruna-node.vhd")])
        self.assertEqual([call["args"] for call in self.calls], [[
            "qemu-img", "convert", "-f", "raw", "-O", "vpc", "-o", "subformat=fixed,force_size",
            str(disk), str(disk.with_name("moruna-node.vhd"))]])

    def test_another_disk_is_rounded_up_to_a_whole_mib_for_the_vhd(self):
        disk = stand_in(self.tmp.name, MIB + 512)
        before = disk.read_bytes()
        package.convert(disk, ["vhd"], self.run_recorded)
        (call,) = self.calls
        self.assertEqual(call["args"][:8], [
            "qemu-img", "convert", "-f", "raw", "-O", "vpc", "-o", "subformat=fixed,force_size"])
        self.assertEqual(call["args"][8:], [str(disk.with_name("moruna-node.vhd-source")),
                                            str(disk.with_name("moruna-node.vhd"))])
        self.assertEqual(call["source_size"], 2 * MIB)
        self.assertEqual(call["source"], before + bytes(MIB - 512))
        # The rounded copy is gone, and the raw disk is as it was.
        self.assertEqual(sorted(p.name for p in disk.parent.iterdir()), ["moruna-node.raw", "moruna-node.vhd"])
        self.assertEqual(disk.read_bytes(), before)

    def test_the_rounded_copy_is_removed_when_the_conversion_fails(self):
        disk = stand_in(self.tmp.name, MIB + 512)

        def fail(args, cwd):
            self.assertTrue(Path(args[-2]).is_file())
            raise subprocess.CalledProcessError(1, args)

        with self.assertRaises(subprocess.CalledProcessError):
            package.convert(disk, ["vhd"], fail)
        self.assertEqual(sorted(p.name for p in disk.parent.iterdir()), ["moruna-node.raw"])

    def test_each_format_is_written_once_in_the_order_named(self):
        disk = stand_in(self.tmp.name, MIB)
        written = package.convert(disk, ["vhd", "raw", "qcow2", "vhd", "raw"], self.run_recorded)
        self.assertEqual([path.name for path in written], ["moruna-node.vhd", "moruna-node.qcow2"])
        self.assertEqual([call["args"][5] for call in self.calls], ["vpc", "qcow2"])

    def test_a_format_it_does_not_know_is_refused(self):
        disk = stand_in(self.tmp.name, MIB)
        with self.assertRaisesRegex(ValueError, "unknown disk format: vmdk"):
            package.convert(disk, ["qcow2", "vmdk"], self.run_recorded)

    def test_evidence_holds_each_file_s_size_and_sha256(self):
        disk = stand_in(self.tmp.name, 4096)
        other = disk.with_name("moruna-node.qcow2")
        other.write_bytes(b"moruna")
        # `printf moruna | shasum -a 256`
        self.assertEqual(package.recorded([disk, other]), {
            str(disk): {"size": 4096, "sha256": sha256(disk)},
            str(other): {"size": 6, "sha256": "97e350159dd8a61fbdb6ad9ad73a4d3092d4dc6154bc49540d7fe4caa01a56a0"},
        })
        self.assertEqual(list(package.recorded([other, disk])), [str(other), str(disk)])


class Conversions(unittest.TestCase):
    """The formats qemu-img really writes, from stand-in raw disks."""

    @classmethod
    def setUpClass(cls):
        if shutil.which("qemu-img") is None:
            raise AssertionError("qemu-img is not on PATH: package.py --formats and these tests need it "
                                 "(moruna-arm: export PATH=/opt/qemu/bin:$PATH)")

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def back_to_raw(self, image, driver):
        """The disk QEMU reads from `image` through `driver`, as bytes."""
        raw = image.with_suffix(".read-back")
        subprocess.run(["qemu-img", "convert", "-f", driver, "-O", "raw", str(image), str(raw)], check=True)
        return raw.read_bytes()

    def test_the_vhd_is_fixed_and_a_whole_mib(self):
        # package.py's own disk is a whole MiB already; the others show the rounding.
        for size in (4 * MIB, 3 * MIB + 512, 2 * MIB + 4096 * 9, MIB - 512):
            with self.subTest(size=size):
                directory = Path(self.tmp.name) / str(size)
                directory.mkdir()
                disk = stand_in(directory, size)
                before = disk.read_bytes()
                (vhd,) = package.convert(disk, ["vhd"])
                self.assertEqual(vhd, directory / "moruna-node.vhd")
                rounded = package.whole_mib(size)
                fields = footer(vhd)
                self.assertEqual(fields["cookie"], b"conectix")
                self.assertTrue(fields["checksum_ok"])
                self.assertEqual(fields["type"], FIXED)
                # Azure's rule: the disk the VHD holds is a whole number of MiB.
                self.assertEqual(fields["current"] % MIB, 0)
                self.assertEqual((fields["current"], fields["original"]), (rounded, rounded))
                # A fixed VHD is the disk and then the footer: nothing else, no header.
                self.assertEqual(vhd.stat().st_size, rounded + FOOTER)
                self.assertEqual(vhd.read_bytes()[:-FOOTER], before + bytes(rounded - size))
                # QEMU's vpc driver, which uefi-boot.sh boots the VHD with, reads that same disk.
                self.assertEqual(self.back_to_raw(vhd, "vpc"), before + bytes(rounded - size))
                self.assertEqual(disk.read_bytes(), before)
                self.assertEqual(sorted(p.name for p in directory.iterdir()),
                                 ["moruna-node.raw", "moruna-node.read-back", "moruna-node.vhd"])

    def test_the_qcow2_holds_the_raw_disk(self):
        disk = stand_in(self.tmp.name, 3 * MIB)
        before = disk.read_bytes()
        (qcow2,) = package.convert(disk, ["qcow2"])
        self.assertEqual(qcow2, disk.with_name("moruna-node.qcow2"))
        self.assertEqual(qcow2.read_bytes()[:4], b"QFI\xfb")
        self.assertEqual(self.back_to_raw(qcow2, "qcow2"), before)
        self.assertEqual(disk.read_bytes(), before)

    def test_every_format_at_once_with_its_evidence(self):
        disk = stand_in(self.tmp.name, 2 * MIB)
        written = package.convert(disk, ["raw", "qcow2", "vhd"])
        self.assertEqual([path.name for path in written], ["moruna-node.qcow2", "moruna-node.vhd"])
        evidence = package.recorded([disk, *written])
        self.assertEqual(list(evidence), [str(disk), str(written[0]), str(written[1])])
        for path in (disk, *written):
            self.assertEqual(evidence[str(path)], {"size": path.stat().st_size, "sha256": sha256(path)})
        self.assertEqual(evidence[str(written[1])]["size"], 2 * MIB + FOOTER)


def modern_bash():
    """A bash that has EPOCHREALTIME, as uefi-boot.sh needs."""
    for name in ("bash", "/opt/homebrew/bin/bash", "/usr/local/bin/bash"):
        path = shutil.which(name)
        if path and subprocess.run([path, "-c", '[ -n "${EPOCHREALTIME:-}" ]'], check=False).returncode == 0:
            return path
    raise AssertionError("no bash with EPOCHREALTIME (5.0 or later) was found: uefi-boot.sh needs one")


class BootDisk(unittest.TestCase):
    """uefi-boot.sh's FORMAT: the disk it boots and the driver QEMU is told to read it with."""

    # In place of QEMU: its arguments to a file, the awaited line to the serial port's file, then
    # it waits to be stopped, as a running machine does.
    QEMU = """#!/bin/sh
printf '%s\\n' "$@" > "$(dirname "$0")/arguments"
for argument in "$@"; do
  case "$argument" in file:*) echo 'moruna: services started' > "${argument#file:}" ;; esac
done
exec sleep 60
"""

    @classmethod
    def setUpClass(cls):
        cls.bash = modern_bash()

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.out = self.root / "out"
        self.out.mkdir()
        (self.root / "bin").mkdir()
        (self.root / "share/qemu").mkdir(parents=True)
        for name in ("edk2-x86_64-code.fd", "edk2-i386-vars.fd"):
            (self.root / "share/qemu" / name).write_bytes(b"firmware")
        qemu = self.root / "bin/qemu-system-x86_64"
        qemu.write_text(self.QEMU)
        qemu.chmod(0o755)

    def boot(self, *arguments):
        path = f"{self.root / 'bin'}:/usr/bin:/bin"
        return subprocess.run([self.bash, str(BOOT), *arguments], capture_output=True, text=True,
                              env={"PATH": path}, timeout=60, check=False)

    def drive(self):
        """The boot disk's -drive argument, as QEMU got it."""
        arguments = (self.root / "bin/arguments").read_text().splitlines()
        (boot,) = [a for a in arguments if a.startswith("if=none,id=boot,")]
        self.assertEqual(arguments[arguments.index(boot) - 1], "-drive")
        return boot

    def test_each_format_boots_its_own_file_with_its_driver(self):
        for name in ("raw", "qcow2", "vhd"):
            (self.out / f"moruna-node.{name}").write_bytes(b"disk")
        for arguments, name, driver in (((), "raw", "raw"), (("raw",), "raw", "raw"),
                                        (("qcow2",), "qcow2", "qcow2"), (("vhd",), "vhd", "vpc")):
            with self.subTest(arguments=arguments):
                result = self.boot(str(self.out), "1", *arguments)
                self.assertEqual(result.returncode, 0, result.stderr)
                disk = self.out / f"moruna-node.{name}"
                self.assertEqual(self.drive(), f"if=none,id=boot,format={driver},readonly=on,file={disk}")
                self.assertIn(f"uefi-boot: boot disk {disk}, read as {driver}", result.stderr)
                self.assertIn('"moruna: services started"', result.stderr)

    def test_the_machine_is_the_same_for_each_format(self):
        machines = []
        for name in ("raw", "qcow2", "vhd"):
            (self.out / f"moruna-node.{name}").write_bytes(b"disk")
            self.assertEqual(self.boot(str(self.out), "2", name).returncode, 0)
            arguments = (self.root / "bin/arguments").read_text().splitlines()
            machines.append([a for a in arguments if not a.startswith("if=none,id=boot,")])
        self.assertEqual(machines[0], machines[1])
        self.assertEqual(machines[0], machines[2])
        self.assertEqual(machines[0][:4], ["-machine", "q35", "-smp", "2"])

    def test_an_unknown_format_is_refused(self):
        (self.out / "moruna-node.raw").write_bytes(b"disk")
        for name in ("vpc", "vmdk", "QCOW2"):
            result = self.boot(str(self.out), "1", name)
            self.assertEqual(result.returncode, 2, name)
            self.assertEqual(result.stderr, f"uefi-boot.sh: FORMAT must be raw, qcow2 or vhd: {name}\n")
            self.assertFalse((self.root / "bin/arguments").exists())

    def test_a_format_that_was_not_built_is_refused(self):
        (self.out / "moruna-node.raw").write_bytes(b"disk")
        for name in ("qcow2", "vhd"):
            result = self.boot(str(self.out), "1", name)
            self.assertEqual(result.returncode, 2, name)
            self.assertEqual(result.stderr, f"uefi-boot.sh: no disk image at {self.out}/moruna-node.{name} "
                                            f"(package.py --profile cloud --formats {name} writes it)\n")
            self.assertFalse((self.root / "bin/arguments").exists())

    def test_it_takes_two_or_three_arguments(self):
        (self.out / "moruna-node.raw").write_bytes(b"disk")
        for arguments in ((), (str(self.out),), (str(self.out), "1", "raw", "more")):
            result = self.boot(*arguments)
            self.assertEqual(result.returncode, 2, arguments)
            self.assertEqual(result.stderr, "usage: uefi-boot.sh OUT CPUS [FORMAT]\n")


if __name__ == "__main__":
    unittest.main()
