"""Installs the engines the node benchmark runs on Linux (system design 6A, 6B), with uv.

The engines and their packages are bench/vm/install.py's (`PIP`), one virtual environment each.
Each engine runs on the newest CPython it supports, found by trying (work item 6.2c): its
environment is made on the first of its interpreters (uv's managed builds, newest first) on which
its packages install and its modules (`IMPORTS`) import; each interpreter tried before it is kept
in installed.json with why it failed. 0.6.6 has one interpreter, its wheel's.

| Engine | Interpreter | Packages |
| --- | --- | --- |
| `moruna066`, `moruna066-rust` | CPython 3.14t, free-threaded | 0.6.6's wheel, pyarrow, numpy; and `moruna_gate` (images/gate/py066) |
| `ray` | the newest of CPython 3.14, 3.13, 3.12, 3.11, 3.10 it installs on, with the GIL | ray[data], typing-extensions, pyarrow, pandas, numpy |
| `spark` | the same | pyspark, pyarrow, pandas, numpy; a JVM (setup-linux.sh installs openjdk-17-jre-headless) |
| `dask` | the same | dask[distributed,dataframe], adlfs, pyarrow, pandas, numpy |
| `duckdb` | the same | duckdb, pyarrow, numpy |
| `polars` | the same | polars, pyarrow, numpy |
| `rust-pool` | none | `gate-baseline` and `gate-canon` from images/gate, `cargo build --release` |
| `harness` | the same | pyarrow, numpy: measure.py's own environment, for bench/vm/canon.py |

On 2026-11-18 (Ubuntu 24.04, uv 0.10.9) Ray 2.60.0, Dask 2026.8.0, DuckDB 2.0.0 and PySpark 4.3.0
installed and imported on CPython 3.14.3 with the GIL, the first tried.

**Ray omits a dependency it imports.** Ray 2.60.0's `ray.data` imports `typing_extensions`, which
Ray does not declare, and which only a dependency of its own brings in below CPython 3.13. As
published, Ray therefore imports on 3.12 and not on 3.13 or 3.14. `typing-extensions` is
installed beside it, so Ray runs on the newest CPython: every engine gets its best configuration
(the PM, 2026-11-18).

An engine that did not install keeps, in installed.json, each interpreter tried and the end of
its error (`tried`), so a setup that refuses to go on says why (setup-linux.sh, run.py).

Packages are not pinned, as bench/vm's were not: each run records its engine's version. Each
engine's Blob reader comes with it (store.py), installed here, outside every timing: Dask's adlfs,
DuckDB's `azure` extension (into ROOT/duckdb-ext, which a run loads from with no download), and
Spark's hadoop-azure, the version of the Hadoop its pyspark carries, resolved into ROOT/ivy (that
needs the JVM) and its jars recorded, which a run's session is given as they are (`spark.jars`).
`--moruna066` names 0.6.6's wheel (setup-linux.sh's: given, from PyPI once 0.6.6 is released there,
or built), or a source tree (a `git archive` of the 0.6.6 commit) that is built into one with
`uv build`. Builds run `cargo` with `--jobs` jobs.

    install.py --root DIR --gate-src images/gate [--moruna066 WHEEL_OR_TREE] [--engines a,b]

Writes DIR/installed.json: per engine, its interpreter, its GIL, the interpreters tried before it
and why each failed, its environment, the versions of its packages, and whether it installed. Run
again, it keeps what is there. It exits 1 when an engine it was asked for did not install.
"""
import argparse
import glob
import json
import os
import shutil
import subprocess
import sys
import time

# The CPython versions an engine is tried on, newest first, as uv's requests: `+gil` names the
# standard build, since a bare `3.14` can resolve to the free-threaded one when that is the managed
# 3.14 uv finds first (uv 0.10.9 on the Mac). Only uv's managed builds (`--managed-python`), so no
# engine lands on the machine's own python3.
PYTHONS = ("3.14", "3.13", "3.12", "3.11", "3.10")
GIL = tuple(f"{v}+gil" for v in PYTHONS)
PIP = {
    "moruna066": (("3.14t",), ["{wheel}", "pyarrow", "numpy"]),
    # typing-extensions: `ray.data` imports it and Ray does not declare it (see the module's note).
    "ray": (GIL, ["ray[data]", "typing-extensions", "pyarrow", "pandas", "numpy"]),
    "spark": (GIL, ["pyspark", "pyarrow", "pandas", "numpy"]),
    "dask": (GIL, ["dask[distributed,dataframe]", "adlfs", "pyarrow", "pandas", "numpy"]),
    "duckdb": (GIL, ["duckdb", "pyarrow", "numpy"]),
    "polars": (GIL, ["polars", "pyarrow", "numpy"]),
    "harness": (GIL, ["pyarrow", "numpy"]),
}
# What a run imports (engine.py, measure.py): each must import for the engine to be installed on
# an interpreter.
IMPORTS = {"moruna066": ["moruna"], "moruna066-rust": ["moruna_gate"], "ray": ["ray", "ray.data"],
           "spark": ["pyspark", "pyspark.sql"], "dask": ["dask", "dask.dataframe", "dask.distributed", "adlfs"],
           "duckdb": ["duckdb"], "polars": ["polars"], "harness": ["pyarrow"]}
# The environment each engine runs in: 0.6.6 runs the Rust port from the same one as its Python.
VENV = {"moruna066": "moruna066", "moruna066-rust": "moruna066", "ray": "ray", "spark": "spark",
        "dask": "dask", "duckdb": "duckdb", "polars": "polars", "harness": "harness"}
ENGINES = ("harness", "moruna066", "moruna066-rust", "rust-pool", "duckdb", "polars", "ray", "spark", "dask")


def sh(cmd, log, env=None, cwd=None):
    """Runs `cmd`, its output appended to `log`; raises with the output's tail when it fails."""
    with open(log, "a") as fh:
        fh.write("$ " + " ".join(cmd) + "\n")
        fh.flush()
        p = subprocess.run(cmd, stdout=fh, stderr=subprocess.STDOUT, env=env, cwd=cwd)
    if p.returncode != 0:
        tail = open(log).read()[-800:]
        raise RuntimeError(f"{' '.join(cmd[:3])}... exited {p.returncode}: {tail}")


def build_env(jobs):
    env = dict(os.environ)
    env.update({"CARGO_BUILD_JOBS": str(jobs), "CARGO_INCREMENTAL": "0"})
    return env


def venv_python(root, engine):
    return os.path.join(root, f"venv-{VENV[engine]}", "bin", "python")


def wheel066(root, source, log, jobs):
    """0.6.6's wheel: `source` itself, or the one built from the tree `source`."""
    if os.path.isfile(source):
        return source
    out = os.path.join(root, "wheels")
    found = glob.glob(os.path.join(out, "moruna-0.6.6-*.whl"))
    if found:
        return found[0]
    env = build_env(jobs)
    env["CARGO_TARGET_DIR"] = os.path.join(root, "target-066")
    sh(["uv", "build", "--wheel", "--python", PIP["moruna066"][0], "--out-dir", out, source], log, env)
    return glob.glob(os.path.join(out, "moruna-0.6.6-*.whl"))[0]


def make_venv(root, name, python, packages, log):
    venv = os.path.join(root, f"venv-{name}")
    if not os.path.exists(os.path.join(venv, "bin", "python")):
        sh(["uv", "venv", "-q", "--managed-python", "-p", python, venv], log)
    sh(["uv", "pip", "install", "-q", "--python", os.path.join(venv, "bin", "python")] + packages, log)
    return venv


class NotInstalled(RuntimeError):
    """An engine that held on none of its interpreters: `tried` is each one and why it failed."""

    def __init__(self, message, tried):
        super().__init__(message)
        self.tried = tried


def imported(python, modules):
    """`versions` of `modules` in `python`, or a RuntimeError naming what did not import."""
    got = versions(python, modules)
    if "error" in got:
        raise RuntimeError(f"the interpreter did not answer: {got['error']}")
    missing = {m: v for m, v in got["modules"].items() if str(v).startswith("missing")}
    if missing:
        raise RuntimeError("did not import: " + ", ".join(f"{m} ({v})" for m, v in missing.items()))
    return got


def newest(engine, a, log, packages):
    """The engine's environment on the newest of its interpreters on which `packages` install and
    its modules import: `versions` of them, its `interpreter`, and `tried`, each interpreter before
    it and why it failed. One that fails leaves no environment behind; none that holds is an error."""
    venv = os.path.join(a.root, f"venv-{VENV[engine]}")
    modules = IMPORTS[engine] + [m for m in ("pyarrow", "numpy") if m not in IMPORTS[engine]]
    tried = []
    for interp in PIP[engine][0]:
        with open(log, "a") as fh:
            fh.write(f"# {engine} on CPython {interp}\n")
        try:
            # uv finds a `+gil` request only among the interpreters it has: each is installed first.
            sh(["uv", "python", "install", interp.split("+")[0]], log)
            make_venv(a.root, VENV[engine], interp, packages, log)
            got = imported(venv_python(a.root, engine), modules)
        except (RuntimeError, OSError, ValueError) as e:
            tried.append({"python": interp, "error": str(e)[-500:]})
            shutil.rmtree(venv, ignore_errors=True)
            continue
        return dict(got, interpreter=interp, tried=tried)
    raise NotInstalled(f"{engine} installs and imports on none of CPython {', '.join(PIP[engine][0])}: "
                       + "; ".join(f"{t['python']}: {t['error'][-200:]}" for t in tried), tried)


def versions(python, modules):
    code = ("import importlib, json, sys\nout = {}\n"
            "for m in sys.argv[1:]:\n"
            "    try:\n        out[m] = getattr(importlib.import_module(m), '__version__', 'unknown')\n"
            "    except Exception as e:\n        out[m] = 'missing: ' + type(e).__name__\n"
            "print(json.dumps({'python': sys.version.split()[0], 'gil': getattr(sys, '_is_gil_enabled', "
            "lambda: True)(), 'modules': out}))")
    p = subprocess.run([python, "-I", "-c", code] + modules, capture_output=True, text=True)
    return json.loads(p.stdout) if p.returncode == 0 else {"error": p.stderr[-300:]}


def java():
    """The JVM Spark would use, or None."""
    home = os.environ.get("JAVA_HOME")
    exe = os.path.join(home, "bin", "java") if home else shutil.which("java")
    if not exe:
        return None
    p = subprocess.run([exe, "-version"], capture_output=True, text=True)
    return p.stderr.splitlines()[0] if p.returncode == 0 and p.stderr else None


def spark_azure(root, log, have_java):
    """hadoop-azure for Spark's abfss:// (store.py): the version of the Hadoop client pyspark
    carries, its jars resolved into ROOT/ivy by a session that only starts, and recorded (`jars`),
    so a run's session is given them as they are and resolves nothing."""
    python = venv_python(root, "spark")
    try:
        found = subprocess.run([python, "-I", "-c", "import glob, os, pyspark; print(' '.join(glob.glob(os.path.join("
                                "os.path.dirname(pyspark.__file__), 'jars', 'hadoop-client-api-*.jar'))))"],
                               capture_output=True, text=True).stdout.split()
    except OSError:
        found = []
    if not found:
        return {"packages": "", "ivy": "", "azure_note": "pyspark carries no Hadoop client jar"}
    version = os.path.basename(found[0])[len("hadoop-client-api-"):-len(".jar")]
    out = {"packages": f"org.apache.hadoop:hadoop-azure:{version}", "ivy": os.path.join(root, "ivy")}
    if have_java:
        sh([python, "-I", "-c", "import sys; from pyspark.sql import SparkSession as S; "
            "S.builder.master('local[1]').config('spark.jars.packages', sys.argv[1])"
            ".config('spark.jars.ivy', sys.argv[2]).getOrCreate().stop()", out["packages"], out["ivy"]], log)
        out["jars"] = sorted(glob.glob(os.path.join(out["ivy"], "jars", "*.jar")))
        if not any(os.path.basename(j).startswith("org.apache.hadoop_hadoop-azure-") for j in out["jars"]):
            raise RuntimeError(f"{out['packages']} was not resolved into {out['ivy']}/jars")
    else:
        out["azure_note"] = "hadoop-azure not resolved: no JVM"
    return out


def install(engine, a, log):
    rec = {"engine": engine, "started": time.time()}
    if engine == "rust-pool":
        env = build_env(a.jobs)
        env["CARGO_TARGET_DIR"] = os.path.join(a.root, "target-gate")
        sh(["cargo", "build", "--release", "-j", str(a.jobs), "-p", "moruna-gate-baseline", "--bins"], log, env,
           cwd=a.gate_src)
        os.makedirs(os.path.join(a.root, "bin"), exist_ok=True)
        for b in ("gate-baseline", "gate-canon"):
            shutil.copy2(os.path.join(a.root, "target-gate", "release", b), os.path.join(a.root, "bin", b))
        rec["bin"] = os.path.join(a.root, "bin")
        rec["settings"] = "cargo build --release (images/gate/.cargo/config.toml: AVX2 and FMA on x86_64)"
    elif engine == "moruna066-rust":
        python = venv_python(a.root, engine)
        if not os.path.exists(python):
            raise RuntimeError("moruna066-rust runs in moruna066's environment: install moruna066 first")
        env = build_env(a.jobs)
        env["CARGO_TARGET_DIR"] = os.path.join(a.root, "target-py066")
        sh(["uv", "pip", "install", "-q", "--python", python, os.path.join(a.gate_src, "py066")], log, env)
        rec.update(imported(python, IMPORTS[engine] + ["pyarrow", "numpy"]))
    else:
        pkgs = PIP[engine][1]
        if engine == "moruna066":
            if not a.moruna066:
                raise RuntimeError("0.6.6 is not on PyPI: --moruna066 names its wheel or its source tree")
            pkgs = [p.format(wheel=wheel066(a.root, a.moruna066, log, a.jobs)) for p in pkgs]
        rec.update(newest(engine, a, log, pkgs))
    if engine == "duckdb":
        ext = os.path.join(a.root, "duckdb-ext")
        sh([venv_python(a.root, engine), "-I", "-c",
            "import duckdb, sys; c = duckdb.connect(); c.execute(f\"SET extension_directory='{sys.argv[1]}'\"); "
            "c.install_extension('azure'); c.load_extension('azure')", ext], log)
        rec["azure_extension"] = ext
    if engine == "spark":
        rec["java"] = java()
        rec.update(spark_azure(a.root, log, rec["java"]))
        if not rec["java"]:
            # Not installed: a run could not start its session (setup-linux.sh installs the JVM).
            rec["error"] = "no JVM: Spark cannot run here"
    rec["install_s"] = round(time.time() - rec.pop("started"), 1)
    return rec


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--root", required=True)
    p.add_argument("--gate-src", required=True, help="images/gate")
    p.add_argument("--moruna066", default="", help="0.6.6's wheel, or a source tree to build it from")
    p.add_argument("--engines", default=",".join(ENGINES))
    p.add_argument("--jobs", type=int, default=2)
    a = p.parse_args(argv)
    a.root, a.gate_src = os.path.abspath(a.root), os.path.abspath(a.gate_src)
    if a.moruna066:
        a.moruna066 = os.path.abspath(a.moruna066)
    os.makedirs(a.root, exist_ok=True)
    path = os.path.join(a.root, "installed.json")
    done = json.load(open(path)) if os.path.exists(path) else {}
    for engine in a.engines.split(","):
        if engine not in ENGINES:
            raise SystemExit(f"install.py: no engine {engine!r}; the engines are {', '.join(ENGINES)}")
        if done.get(engine, {}).get("ok"):
            continue
        log = os.path.join(a.root, f"install-{engine}.log")
        try:
            rec = install(engine, a, log)
            rec["ok"] = "error" not in rec
        except Exception as e:  # an engine that does not install is a result, as a failed run is
            rec = {"engine": engine, "ok": False, "error": str(e)[-800:]}
            if getattr(e, "tried", None):
                rec["tried"] = e.tried  # each interpreter's own error: the whole of why
        done[engine] = rec
        json.dump(done, open(path, "w"), indent=1)
        print(json.dumps(rec), flush=True)
    return 0 if all(done.get(e, {}).get("ok") for e in a.engines.split(",")) else 1


if __name__ == "__main__":
    sys.exit(main())
