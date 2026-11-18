#!/usr/bin/env python3
"""system-layer.sh (items 3.2, 3.3 and 3.4), against stand-ins for cargo, the compilers and the
engine.

Nothing here builds the engine. The stand-in cargo records how it was called and leaves a
stand-in engine where the build would; that engine records the tree it is asked to pack and
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

# The engine `volume pack TREE OUT`: the tree's entries with their modes, and its program's
# digest, to STANDIN_RECORD; then a layer to OUT.
ENGINE = """#!{python}
import hashlib, json, os, sys
assert sys.argv[1:3] == ["volume", "pack"], sys.argv
tree, out = sys.argv[3], sys.argv[4]
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

# cargo: its arguments, directory and the build's variables to STANDIN_CARGO; the engine where
# the build leaves it, unless STANDIN_NO_ENGINE.
CARGO = """#!{python}
import json, os, shutil, sys
names = ("RUSTFLAGS", "CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER", "CC_x86_64_unknown_linux_musl",
         "CFLAGS_x86_64_unknown_linux_musl", "CARGO_ENCODED_RUSTFLAGS")
json.dump({{"args": sys.argv[1:], "cwd": os.getcwd(), "env": {{n: os.environ.get(n) for n in names}}}},
          open(os.environ["STANDIN_CARGO"], "w"))
if not os.environ.get("STANDIN_NO_ENGINE"):
    release = os.path.join(os.environ["CARGO_TARGET_DIR"], "{musl}", "release")
    os.makedirs(release, exist_ok=True)
    shutil.copyfile(os.environ["STANDIN_ENGINE"], os.path.join(release, "moruna-engine"))
    os.chmod(os.path.join(release, "moruna-engine"), 0o755)
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
        self.target = self.tmp / "target"
        self.env = {
            "PATH": str(self.bin),
            "HOME": str(self.tmp),
            "TMPDIR": str(self.tmp),
            "CARGO_TARGET_DIR": str(self.target),
            "CARGO_ENCODED_RUSTFLAGS": "-Cinherited",
            "STANDIN_ENGINE": str(self.engine),
            "STANDIN_CARGO": str(self.tmp / "cargo.json"),
            "STANDIN_RECORD": str(self.tmp / "record.json"),
        }

    def stand_in(self, name, text):
        path = self.bin / name
        path.write_text(text)
        path.chmod(0o755)

    def x86_64_machine(self):
        self.stand_in("uname", "#!/bin/sh\necho x86_64\n")
        self.stand_in("gcc", "#!/bin/sh\nexit 0\n")
        self.stand_in("musl-gcc", "#!/bin/sh\nexit 0\n")
        self.stand_in("cargo", CARGO.format(python=sys.executable, musl=MUSL))

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
        self.assertFalse((self.tmp / "cargo.json").exists())

    def test_the_engine_is_built_as_build_sh_builds_it_and_packed_with_tmp(self):
        self.x86_64_machine()
        out = self.tmp / "engine.layer"
        result = self.run_script("engine.layer")
        self.assertEqual(result.returncode, 0, result)
        self.assertIn(f"{out}: a layer of 1 pages", result.stdout)
        self.assertTrue(out.read_bytes().startswith(b"MRNLAYR1"))
        cargo = json.loads((self.tmp / "cargo.json").read_text())
        self.assertEqual(cargo["args"], ["build", "--quiet", "--release", "--locked", "--target", MUSL,
                                         "-p", "moruna-runtime", "--bin", "moruna-engine"])
        self.assertEqual(Path(cargo["cwd"]).resolve(), ROOT.resolve())
        self.assertEqual(cargo["env"], {
            "RUSTFLAGS": "--cfg polyval_force_soft -C link-self-contained=yes -C target-feature=+crt-static -C strip=symbols",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER": "gcc",
            "CC_x86_64_unknown_linux_musl": "musl-gcc",
            "CFLAGS_x86_64_unknown_linux_musl": "",
            "CARGO_ENCODED_RUSTFLAGS": None,
        })
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
        # The tree it packed is gone.
        self.assertFalse(Path(record["tree"]).exists())

    def test_a_build_that_leaves_no_engine_is_said(self):
        self.x86_64_machine()
        result = self.run_script(str(self.tmp / "engine.layer"), STANDIN_NO_ENGINE="1")
        self.assertEqual(result.returncode, 1, result)
        self.assertIn(f"the build left no engine at {self.target}/{MUSL}/release/moruna-engine", result.stderr)
        self.assertFalse((self.tmp / "engine.layer").exists())


if __name__ == "__main__":
    unittest.main()
