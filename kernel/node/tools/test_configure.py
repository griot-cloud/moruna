#!/usr/bin/env python3
"""configure.py's data disks (stream D): the grants, links and refusals it renders.

Run: python3 -I kernel/node/tools/test_configure.py
"""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

TOOL = Path(__file__).resolve().parent / "configure.py"
MIB = 1024 * 1024


def profile(**extra):
    base = {"version": 1, "node_memory_bytes": 3072 * MIB, "cpus": 2, "agent_port": 5100,
            "job_memory_bytes": 1024 * MIB, "job_account_bytes": 1408 * MIB, "scratch_bytes": 256 * MIB,
            "engine_program": "/moruna/bin/moruna-engine", "storage_provider": "single-node-home",
            "service_accounts": {"devices": 128 * MIB, "imagefs": 768 * MIB, "linux": 256 * MIB,
                                 "network": 64 * MIB, "admission": 1536 * MIB, "storage": 128 * MIB}}
    base.update(extra)
    return base


def render(root, value):
    (root / "profile.json").write_text(json.dumps(value))
    inputs = []
    for name in ("image-record", "layers", "network", "tls-roots", "engine-bindings", "storage-config", "storage-grants", "storage-admission"):
        path = root / name
        path.write_text("x")
        inputs += [f"--{name}", str(path)]
    out = root / "assets"
    done = subprocess.run([sys.executable, "-I", str(TOOL), "--profile", str(root / "profile.json"), "--out", str(out), *inputs],
                          capture_output=True, text=True)
    return done, out


class DataDisks(unittest.TestCase):
    def test_inputs_and_the_output_are_granted_in_device_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            done, out = render(Path(tmp), profile(data_disks={"inputs": ["nyc", "zones"], "output_bytes": 64 * MIB}))
            self.assertEqual(done.returncode, 0, done.stderr)
            engine = (out / "engine.conf").read_text().splitlines()
            self.assertIn("grant disk.in.nyc disk data0", engine)
            self.assertIn("grant disk.in.zones disk data1", engine)
            self.assertIn("grant disk.out disk data2", engine)
            self.assertIn("link devices control.engine admission initial.handover", (out / "node.conf").read_text().splitlines())
            self.assertIn("grant handover initial.handover", (out / "initial.job").read_text().splitlines())
            self.assertEqual(json.loads((out / "profile.json").read_text())["data_disks"]["inputs"], ["nyc", "zones"])

    def test_without_data_disks_nothing_is_linked_or_granted(self):
        with tempfile.TemporaryDirectory() as tmp:
            done, out = render(Path(tmp), profile())
            self.assertEqual(done.returncode, 0, done.stderr)
            for name in ("engine.conf", "node.conf", "initial.job"):
                text = (out / name).read_text()
                self.assertNotIn("disk.", text)
                self.assertNotIn("handover", text)

    def test_an_output_alone_is_the_first_data_disk(self):
        with tempfile.TemporaryDirectory() as tmp:
            done, out = render(Path(tmp), profile(data_disks={"inputs": [], "output_bytes": 4096}))
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertIn("grant disk.out disk data0", (out / "engine.conf").read_text().splitlines())

    def test_a_data_disk_section_that_cannot_be_built_is_refused(self):
        for bad in ({"inputs": ["Upper"], "output_bytes": 0},
                    {"inputs": ["a" * 25], "output_bytes": 0},
                    {"inputs": ["a", "a"], "output_bytes": 0},
                    {"inputs": [], "output_bytes": 4095},
                    {"inputs": [f"i{n}" for n in range(15)], "output_bytes": 4096},
                    {"inputs": []},
                    ["nyc"]):
            with tempfile.TemporaryDirectory() as tmp:
                done, out = render(Path(tmp), profile(data_disks=bad))
                self.assertNotEqual(done.returncode, 0, bad)
                self.assertIn("data_disks", done.stderr)
                self.assertFalse(out.exists())

    def test_another_unknown_field_is_still_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            done, _ = render(Path(tmp), profile(other=1))
            self.assertNotEqual(done.returncode, 0)


if __name__ == "__main__":
    unittest.main()
