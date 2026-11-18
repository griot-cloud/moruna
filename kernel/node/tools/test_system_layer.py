#!/usr/bin/env python3
"""system-layer.sh (items 3.2, 3.3 and 3.4), against stand-ins for cargo, the compilers, the
engine and the packer.

Nothing here builds anything. The stand-in cargo records each build it is asked for and leaves
a stand-in where that build would: the x86_64 engine, the layer's payload, which notes it if it
is ever run; and this machine's `moruna-layer`, which records the tree it is asked to pack and
writes the layer file. The script runs with a PATH of only those stand-ins and the few tools it
uses, so what the machine has installed does not change what it finds.

Run: python3 -I kernel/node/tools/test_system_layer.py
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest

TOOLS = Path(__file__).resolve().parent
SCRIPT = TOOLS / "system-layer.sh"
ROOT = TOOLS.parents[2]
MUSL = "x86_64-unknown-linux-musl"
# The tools the script runs besides the compilers and cargo.
SYSTEM = ("bash", "cat", "chmod", "cp", "dirname", "mkdir", "mktemp", "rm")

# The x86_64 engine: only a payload. Run, it says so in STANDIN_RAN.
ENGINE = """#!{python}
import os, sys
open(os.environ["STANDIN_RAN"], "a").write(" ".join(sys.argv[1:]) + "\\n")
"""

# This machine's `moruna-layer pack TREE OUT`: the tree's entries with their modes, and its
# program's digest, to STANDIN_RECORD; then a layer to OUT.
PACKER = """#!{python}
import hashlib, json, os, sys
assert sys.argv[1:2] == ["pack"] and len(sys.argv) == 4, sys.argv
tree, out = sys.argv[2], sys.argv[3]
entries = {{}}
for top, dirs, files in os.walk(tree):
    for name in dirs + files:
        path = os.path.join(top, name)
        entries[os.path.relpath(path, tree)] = oct(os.lstat(path).st_mode)
program = open(os.path.join(tree, "moruna/bin/moruna"), "rb").read()
json.dump({{"tree": tree, "root_mode": oct(os.lstat(tree).st_mode), "entries": entries,
           "program": hashlib.sha256(program).hexdigest()}}, open(os.environ["STANDIN_RECORD"], "w"))
open(out, "wb").write(b"MRNLAYR1" + bytes(4088))
print(f"{{out}}: a layer of 1 pages")
"""

# cargo: each build's arguments, directory and variables, appended to STANDIN_CARGO; then what
# that build leaves: the engine for the musl target (unless STANDIN_NO_ENGINE), the packer for
# this machine (unless STANDIN_NO_PACKER).
CARGO = """#!{python}
import json, os, shutil, sys
names = ("RUSTFLAGS", "CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER", "CC_x86_64_unknown_linux_musl",
         "CFLAGS_x86_64_unknown_linux_musl", "CARGO_ENCODED_RUSTFLAGS")
with open(os.environ["STANDIN_CARGO"], "a") as f:
    f.write(json.dumps({{"args": sys.argv[1:], "cwd": os.getcwd(),
                        "env": {{n: os.environ.get(n) for n in names}}}}) + "\\n")
target = os.environ["CARGO_TARGET_DIR"]
if "--target" in sys.argv:
    if os.environ.get("STANDIN_NO_ENGINE"):
        sys.exit(0)
    release, name, source = os.path.join(target, "{musl}", "release"), "moruna-engine", "STANDIN_ENGINE"
else:
    if os.environ.get("STANDIN_NO_PACKER"):
        sys.exit(0)
    release, name, source = os.path.join(target, "release"), "moruna-layer", "STANDIN_PACKER"
os.makedirs(release, exist_ok=True)
shutil.copyfile(os.environ[source], os.path.join(release, name))
os.chmod(os.path.join(release, name), 0o755)
"""


class SystemLayer(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        for name in SYSTEM:
            found = shutil.which(name)
            self.assertIsNotNone(found, f"{name} is needed to run the script")
            (self.bin / name).symlink_to(found)
        self.engine = self.tmp / "engine"
        self.engine.write_text(ENGINE.format(python=sys.executable))
        self.engine.chmod(0o755)
        self.packer = self.tmp / "packer"
        self.packer.write_text(PACKER.format(python=sys.executable))
        self.packer.chmod(0o755)
        self.target = self.tmp / "target"
        self.env = {
            "PATH": str(self.bin),
            "HOME": str(self.tmp),
            "TMPDIR": str(self.tmp),
            "CARGO_TARGET_DIR": str(self.target),
            "CARGO_ENCODED_RUSTFLAGS": "-Cinherited",
            "STANDIN_ENGINE": str(self.engine),
            "STANDIN_PACKER": str(self.packer),
            "STANDIN_CARGO": str(self.tmp / "cargo.jsonl"),
            "STANDIN_RECORD": str(self.tmp / "record.json"),
            "STANDIN_RAN": str(self.tmp / "engine-ran"),
        }

    def stand_in(self, name, text):
        path = self.bin / name
        path.write_text(text)
        path.chmod(0o755)

    def machine(self, arch):
        """A machine whose `uname -m` is `arch`, with gcc, musl-gcc and cargo."""
        self.stand_in("uname", f"#!/bin/sh\necho {arch}\n")
        self.stand_in("gcc", "#!/bin/sh\nexit 0\n")
        self.stand_in("musl-gcc", "#!/bin/sh\nexit 0\n")
        self.stand_in("cargo", CARGO.format(python=sys.executable, musl=MUSL))

    def builds(self):
        return [json.loads(line) for line in (self.tmp / "cargo.jsonl").read_text().splitlines()]

    def run_script(self, *args, **env):
        return subprocess.run([str(self.bin / "bash"), str(SCRIPT), *args], env={**self.env, **env},
                              cwd=self.tmp, capture_output=True, text=True, timeout=60)

    def test_it_takes_one_new_file(self):
        for args in ((), ("a", "b")):
            result = self.run_script(*args)
            self.assertEqual(result.returncode, 2, result)
            self.assertIn("usage: system-layer.sh OUT", result.stderr)
        there = self.tmp / "there.layer"
        there.write_bytes(b"x")
        result = self.run_script(str(there))
        self.assertEqual(result.returncode, 2, result)
        self.assertIn(f"{there} exists; the layer is written to a new file", result.stderr)
        self.assertEqual(there.read_bytes(), b"x")

    def test_it_needs_a_compiler_for_x86_64_linux(self):
        self.stand_in("uname", "#!/bin/sh\necho aarch64\n")
        self.stand_in("gcc", "#!/bin/sh\nexit 0\n")
        result = self.run_script(str(self.tmp / "engine.layer"))
        self.assertEqual(result.returncode, 1, result)
        self.assertIn("no C compiler for x86_64 Linux", result.stderr)
        self.assertFalse((self.tmp / "cargo.jsonl").exists())

    def test_the_engine_is_built_as_build_sh_builds_it_and_packed_by_this_machines_packer(self):
        self.machine("x86_64")
        out = self.tmp / "engine.layer"
        result = self.run_script("engine.layer")
        self.assertEqual(result.returncode, 0, result)
        self.assertIn(f"{out}: a layer of 1 pages", result.stdout)
        self.assertTrue(out.read_bytes().startswith(b"MRNLAYR1"))
        engine, packer = self.builds()
        self.assertEqual(engine["args"], ["build", "--quiet", "--release", "--locked", "--target", MUSL,
                                          "-p", "moruna-runtime", "--bin", "moruna-engine"])
        self.assertEqual(engine["env"], {
            "RUSTFLAGS": "--cfg polyval_force_soft -C link-self-contained=yes -C target-feature=+crt-static -C strip=symbols",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER": "gcc",
            "CC_x86_64_unknown_linux_musl": "musl-gcc",
            "CFLAGS_x86_64_unknown_linux_musl": "",
            "CARGO_ENCODED_RUSTFLAGS": None,
        })
        # The packer for this machine, no target named, with the host tools' flags only.
        self.assertEqual(packer["args"], ["build", "--quiet", "--release", "--locked",
                                          "-p", "moruna-image", "--bin", "moruna-layer"])
        self.assertEqual(packer["env"], {
            "RUSTFLAGS": "--cfg polyval_force_soft",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER": None,
            "CC_x86_64_unknown_linux_musl": None,
            "CFLAGS_x86_64_unknown_linux_musl": None,
            "CARGO_ENCODED_RUSTFLAGS": None,
        })
        for build in (engine, packer):
            self.assertEqual(Path(build["cwd"]).resolve(), ROOT.resolve())
        record = json.loads((self.tmp / "record.json").read_text())
        directory = oct(stat.S_IFDIR | 0o755)
        self.assertEqual(record["root_mode"], directory)
        self.assertEqual(record["entries"], {
            "moruna": directory,
            "moruna/bin": directory,
            "moruna/bin/moruna": oct(stat.S_IFREG | 0o755),
            "tmp": directory,
        })
        built = self.target / MUSL / "release" / "moruna-engine"
        self.assertEqual(record["program"], hashlib.sha256(built.read_bytes()).hexdigest())
        # The engine is the payload and never runs; the tree it was packed from is gone.
        self.assertFalse((self.tmp / "engine-ran").exists())
        self.assertFalse(Path(record["tree"]).exists())

    def test_an_arm64_machine_packs_the_x86_64_engine_without_running_it(self):
        # moruna-arm: aarch64 with the x86_64 cross compiler and musl-dev:amd64's specs, and no
        # x86_64 user emulation. The specs are a real file the script reads, so where this test
        # runs without them it checks the refusal that says so instead.
        self.machine("aarch64")
        self.stand_in("x86_64-linux-gnu-gcc", "#!/bin/sh\nexit 0\n")
        specs = Path("/usr/lib/x86_64-linux-musl/musl-gcc.specs")
        result = self.run_script("engine.layer")
        if not specs.is_file():
            self.assertEqual(result.returncode, 1, result)
            self.assertIn("no musl for x86_64", result.stderr)
            return
        self.assertEqual(result.returncode, 0, result)
        engine, packer = self.builds()
        self.assertEqual(engine["env"]["CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER"], "x86_64-linux-gnu-gcc")
        self.assertEqual(engine["env"]["CFLAGS_x86_64_unknown_linux_musl"], f"-specs={specs}")
        self.assertNotIn("--target", packer["args"])
        self.assertFalse((self.tmp / "engine-ran").exists())
        self.assertTrue((self.tmp / "engine.layer").is_file())

    def test_a_build_that_leaves_no_engine_is_said(self):
        self.machine("x86_64")
        result = self.run_script(str(self.tmp / "engine.layer"), STANDIN_NO_ENGINE="1")
        self.assertEqual(result.returncode, 1, result)
        self.assertIn(f"the build left no engine at {self.target}/{MUSL}/release/moruna-engine", result.stderr)
        self.assertEqual(len(self.builds()), 1)
        self.assertFalse((self.tmp / "engine.layer").exists())

    def test_a_build_that_leaves_no_packer_is_said(self):
        self.machine("x86_64")
        result = self.run_script(str(self.tmp / "engine.layer"), STANDIN_NO_PACKER="1")
        self.assertEqual(result.returncode, 1, result)
        self.assertIn(f"the build left no packer at {self.target}/release/moruna-layer", result.stderr)
        self.assertFalse((self.tmp / "engine.layer").exists())
        self.assertFalse((self.tmp / "engine-ran").exists())


if __name__ == "__main__":
    unittest.main()
