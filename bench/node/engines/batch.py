"""One batch of steps on the benchmark's Linux machine, one at a time, detached from the harness
(work item 6.2b): the engines' setup, every Linux run of a plan, or the checks of the node's outputs.

    batch.py --name NAME --steps FILE --env FILE --dir DIR --logs DIR

run.py starts it with one `az vm run-command` as a systemd unit of its own, so no command outlives
Azure's 90 minutes, and follows it from the Mac through the run's Blob container: after each step,
the batch's state (`DIR/NAME.state.json`) is put at `results/batch/NAME.json`.

A step is `{"id", "argv", "env", "secrets", "timeout_s"}`: `argv` runs as a fresh process of its
own, in the order of the file, with `env` and the secrets it names (from the env file, which the
starting run-command wrote from its parameters) over the machine's `PATH` and `HOME`. A step is
killed at its `timeout_s`, with every cgroup measure.py made under /sys/fs/cgroup/bench, so nothing
of it runs into the next. Its exit code and the tails of its output are kept in the state; the
whole of its output goes to `LOGS/ID.out` and `LOGS/ID.err`. A step that fails is a result, as a
failed run is: the batch goes on. The state is `running` until the last step ends, then `done`;
`failed` if the batch itself stopped.

Standard library only, for Python 3.10 and later: it runs on the machine's own `python3`, before
anything is installed.
"""
import argparse
import glob
import json
import os
import signal
import subprocess
import sys
import time
import urllib.request

CGROUPS = "/sys/fs/cgroup/bench"
OUT_TAIL, ERR_TAIL = 8192, 2000
# The machine's own variables, as the starting run-command had them: every step runs with these.
MACHINE = ("PATH", "HOME")


def load_env(path):
    """`KEY=VALUE` lines (the value may hold `=`); a missing file is no variables."""
    out = {}
    try:
        lines = open(path).read().splitlines()
    except OSError:
        return out
    for line in lines:
        key, sep, value = line.partition("=")
        if sep and key:
            out[key] = value
    return out


def tail(path, n):
    try:
        with open(path, "rb") as f:
            f.seek(0, os.SEEK_END)
            f.seek(max(f.tell() - n, 0))
            return f.read().decode("utf-8", "replace")
    except OSError:
        return ""


def kill_leftovers(root=CGROUPS):
    """Every process left in a run's cgroup (measure.py's), killed."""
    for path in glob.glob(os.path.join(root, "*", "cgroup.kill")):
        try:
            with open(path, "w") as f:
                f.write("1")
        except OSError:
            pass


def step_env(step, machine, secrets):
    env = {k: machine[k] for k in MACHINE if machine.get(k)}
    env.update(step.get("env") or {})
    env.update({k: secrets[k] for k in step.get("secrets") or () if k in secrets})
    return env


def run_step(step, env, logs, leftovers=CGROUPS):
    os.makedirs(logs, exist_ok=True)
    out_path, err_path = os.path.join(logs, f"{step['id']}.out"), os.path.join(logs, f"{step['id']}.err")
    rec = {"id": step["id"], "started": time.time()}
    with open(out_path, "wb") as out, open(err_path, "wb") as err:
        try:
            p = subprocess.Popen(step["argv"], stdout=out, stderr=err, stdin=subprocess.DEVNULL, env=env,
                                 start_new_session=True)
        except OSError as e:
            rec.update(exit=None, error=f"not started: {e}", ended=time.time())
            return rec
        try:
            rec["exit"] = p.wait(timeout=step.get("timeout_s"))
        except subprocess.TimeoutExpired:
            try:
                os.killpg(p.pid, signal.SIGKILL)
            except OSError:
                pass
            p.wait()
            kill_leftovers(leftovers)
            rec.update(exit=None, timed_out=True, error=f"killed at the batch's bound, {step['timeout_s']} s")
    rec["ended"] = time.time()
    rec["stdout"], rec["stderr"] = tail(out_path, OUT_TAIL), tail(err_path, ERR_TAIL)
    return rec


class Publisher:
    """The state, written to DIR and put at `results/batch/NAME.json` with the results SAS."""

    def __init__(self, name, directory, base="", sas="", tries=3):
        self.name, self.path, self.base, self.sas, self.tries = name, os.path.join(directory, f"{name}.state.json"), \
            base, sas, tries

    def __call__(self, state):
        body = json.dumps(state, default=str).encode()
        tmp = self.path + ".tmp"
        with open(tmp, "wb") as f:
            f.write(body)
        os.replace(tmp, self.path)
        if not (self.base and self.sas):
            return
        url = f"{self.base}/results/batch/{self.name}.json?{self.sas}"
        headers = {"x-ms-blob-type": "BlockBlob", "Content-Type": "application/json"}
        for attempt in range(self.tries):
            try:
                req = urllib.request.Request(url, data=body, method="PUT", headers=headers)
                urllib.request.urlopen(req, timeout=30).close()
                return
            except OSError as e:  # the next step's state carries this one's: a lost put is not fatal
                sys.stderr.write(f"batch {self.name}: the state was not put ({attempt + 1}): {e}\n")
                if attempt + 1 < self.tries:
                    time.sleep(2 * (attempt + 1))


def run_batch(name, steps, machine, secrets, logs, publish, leftovers=CGROUPS):
    state = {"name": name, "state": "running", "of": len(steps), "steps": [], "started": time.time()}
    publish(state)
    try:
        for step in steps:
            state["steps"].append(run_step(step, step_env(step, machine, secrets), logs, leftovers))
            publish(state)
        state["state"] = "done"
    except BaseException as e:
        state.update(state="failed", error=f"{type(e).__name__}: {e}"[-500:])
        raise
    finally:
        state["ended"] = time.time()
        publish(state)
    return state


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--name", required=True)
    p.add_argument("--steps", required=True, help="the steps, a JSON list")
    p.add_argument("--env", required=True, help="KEY=VALUE lines: the machine's PATH and HOME, and the secrets")
    p.add_argument("--dir", required=True, help="where the state is kept")
    p.add_argument("--logs", required=True, help="where each step's whole output goes")
    a = p.parse_args(argv)
    steps = json.load(open(a.steps))
    env = load_env(a.env)
    publish = Publisher(a.name, a.dir, env.get("BASE", ""), env.get("SAS_RESULTS", ""))
    state = run_batch(a.name, steps, env, env, a.logs, publish)
    return 0 if state["state"] == "done" else 1


if __name__ == "__main__":
    sys.exit(main())
