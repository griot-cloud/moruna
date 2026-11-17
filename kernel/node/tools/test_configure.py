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
                    {"inputs": [f"i{n}" for n in range(40)], "output_bytes": 4096},
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


class NodeSizes(unittest.TestCase):
    """node-for-job.py's node: the job's memory, its scratch and the node's services."""

    def setUp(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location("node_for_job", TOOL.parent / "node-for-job.py")
        self.tool = importlib.util.module_from_spec(spec)
        sys.dont_write_bytecode = True
        spec.loader.exec_module(self.tool)

    def test_a_node_holds_its_job_and_passes_configure(self):
        node, job, account, scratch, admission = self.tool.sizes(384 * MIB, 1024 * MIB)
        self.assertEqual((job, scratch), (384 * MIB, 1024 * MIB))
        self.assertGreater(account, job + scratch)
        self.assertGreater(admission, account)
        self.assertEqual(node % (2 * MIB), 0)
        self.assertLessEqual(node, 6 * 1024 * MIB, "a booted test's node stays within 6 GiB")
        accounts = dict(self.tool.SERVICES, admission=admission)
        value = profile(node_memory_bytes=node, job_memory_bytes=job, job_account_bytes=account,
                        scratch_bytes=scratch, service_accounts=accounts)
        with tempfile.TemporaryDirectory() as tmp:
            done, _ = render(Path(tmp), value)
            self.assertEqual(done.returncode, 0, done.stderr)

    def test_an_exact_node_is_refused_when_the_job_does_not_fit(self):
        gib = 1024 * MIB
        node, *_ = self.tool.sizes(20 * gib, gib, 24 * gib)
        self.assertEqual(node, 24 * gib)
        with self.assertRaisesRegex(ValueError, "needs at least"):
            self.tool.sizes(24 * gib, gib, 24 * gib)
        with self.assertRaisesRegex(ValueError, "2 MiB"):
            self.tool.sizes(gib, gib, 24 * gib + 4096)

    def test_a_job_and_its_scratch_fit_beside_what_the_kernel_keeps(self):
        # stream E hook (D's file): on a 24 GiB node the processes get 95.17%, so 17 GiB with
        # 4 GiB of scratch fits and 17.5 with 4 is refused before a launch (it booted and no
        # service connected, 2026-11-17).
        gib = 1024 * MIB
        node, job, *_ = self.tool.sizes(17 * gib, 4 * gib, 24 * gib)
        self.assertEqual((node, job), (24 * gib, 17 * gib))
        with self.assertRaisesRegex(ValueError, r"needs at least \d+ \(24\.\d\d GiB\)"):
            self.tool.sizes(int(17.5 * gib), 4 * gib, 24 * gib)
        with self.assertRaisesRegex(ValueError, "needs at least"):
            self.tool.sizes(int(20.5 * gib), gib, 24 * gib)
        node, *_ = self.tool.sizes(int(20.5 * gib), gib // 2, 24 * gib)
        self.assertEqual(node, 24 * gib)
        least, *_ = self.tool.sizes(17 * gib, 4 * gib)
        self.assertLessEqual(least, 24 * gib)
        self.assertGreater(least * 24526286848 // (24 * gib), 17 * gib + 4 * gib + sum(self.tool.SERVICES.values()))

    def test_a_stack_from_block0_is_attached_alone(self):
        with tempfile.TemporaryDirectory() as tmp:
            build, stack = Path(tmp), Path(tmp) / "stack.layers"
            fixtures = [build / "layers/layer-1.img", build / "layers/layer-2.img"]
            stack.write_text("moruna-layers 1\nlayer block0 aa\nlayer block1 bb\n")
            self.assertEqual(self.tool.kit_layers(build, stack), [])
            stack.write_text("moruna-layers 1\nlayer block2 aa\n")
            self.assertEqual(self.tool.kit_layers(build, stack), fixtures)
            for text, refused in (("moruna-layers 1\nlayer block1 aa\n", "block1"),
                                  ("moruna-layers 1\n", "no layer")):
                stack.write_text(text)
                with self.assertRaisesRegex(ValueError, refused):
                    self.tool.kit_layers(build, stack)


if __name__ == "__main__":
    unittest.main()
