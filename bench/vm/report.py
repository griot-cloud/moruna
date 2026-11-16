"""The PM's NYC matrix (arrow-core-0.7.md section 9) from a gate plan's results.

    report.py RESULTS_DIR [MACHINE...]     # prints the matrix as Markdown

For each machine and workload, a row per engine, labelled by the language its function runs in:
the median over the measured repeats (r1-r5, the learning run r0 left out) of wall time,
CPU-seconds and peak memory, the OOM kills, the median time split (read and decode, the function,
encode and write, waits) where the engine reports one, the VM's boot for moruna-vm, and whether
its output matched 0.6.6's (gate-canon: bit for bit, within the named tolerance, or not).

Then 0.7's speedup (moruna-vm) against the best row of group 2 (the plain Rust pool) and of
group 5 (native expressions), and against 0.6.6, beside the PM's target: at least 1.5 times the
median wall of the best like-for-like row (group 2) with CPU-seconds no worse. Any gap is named.
The session's noise is the spread of the 0.6.6 pairs against themselves.
"""
import glob
import json
import os
import statistics
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import canon as reference  # noqa: E402
import gate_plan  # noqa: E402

TARGET = 1.5
GATE = os.environ.get("GATE_BIN", "/home/bench/gate/bin")


def load(results_dir):
    out = []
    for p in glob.glob(os.path.join(results_dir, "g-*.json")):
        try:
            out.append(json.load(open(p)))
        except ValueError:
            continue
    return out


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def match(rec, ref, workload):
    """How a run's output compares with the reference's (0.6.6, the Python function)."""
    a, b = (rec or {}).get("canon") or {}, (ref or {}).get("canon") or {}
    if not a or not b or "hash_all" not in a or "hash_all" not in b:
        return "unchecked"
    if a["rows"] != b["rows"] or a["hash_exact"] != b["hash_exact"]:
        return "DIFFERS"
    if a["hash_all"] == b["hash_all"]:
        return "bit for bit"
    if not reference.SPECS[workload].get("tolerant"):
        return "DIFFERS"
    if a.get("dump") and b.get("dump") and os.path.exists(f"{GATE}/gate-canon"):
        p = subprocess.run([f"{GATE}/gate-canon", "compare", workload, a["dump"], b["dump"]],
                           capture_output=True, text=True)
        if p.returncode == 0:
            c = json.loads(p.stdout)
            return f"within tolerance ({c['differing']} values differ, max {c['max_abs']:.1e})"
        return "OUTSIDE TOLERANCE"
    return "exact columns match; tolerant columns unchecked"


def rows_for(recs, machine, workload):
    """Each engine's medians, and the reference run for the output check."""
    mine = [r for r in recs if r["spec"].get("machine") == machine and r["spec"]["workload"] == workload]
    ref = next((r for r in sorted(mine, key=lambda r: r["spec"]["id"])
                if r["spec"]["engine"] == "moruna" and r.get("ok") and r["spec"].get("repeat") == 1), None)
    table = {}
    for e in gate_plan.engines_for(workload):
        runs = [r for r in mine if r["spec"]["engine"] == e and (r["spec"].get("repeat") or 0) >= 1]
        if not runs:
            continue
        ok = [r for r in runs if r.get("ok")]
        splits = [r.get("split") or {} for r in ok]
        table[e] = {
            "group": gate_plan.group_of(e),
            "function": next((r.get("function") for r in ok if r.get("function")),
                             "rust" if e in ("moruna-vm", "rust-baseline", "moruna066-rust") else "python"),
            "runs": len(runs), "ok": len(ok),
            "wall_s": med([r["wall_s"] for r in ok]),
            "cpu_s": med([r["cpu_s"] for r in ok]),
            "peak_gib": med([r["cg_peak_bytes"] / (1 << 30) for r in ok]),
            "oom": sum(r.get("oom_kills", 0) for r in runs),
            "boot_s": med([r.get("boot_s") for r in ok]),
            "split": {k: med([s.get(k) for s in splits]) for k in
                      ("read_decode_s", "function_s", "encode_write_s", "wait_s")},
            "output": match(ok[0], ref, workload) if ok else "no run passed",
        }
    noise = [r["wall_s"] for r in mine if r["spec"].get("noise") and r.get("ok")]
    return table, noise


def fmt(x, nd=1):
    return "" if x is None else f"{x:.{nd}f}"


def speedups(table):
    vm = table.get("moruna-vm")
    lines = []
    if not vm or vm["wall_s"] is None:
        return ["0.7 (moruna-vm) has no measured run."]

    def best(group):
        rows = [(r["wall_s"], e, r) for e, r in table.items() if r["group"] == group and r["wall_s"]]
        return min(rows, key=lambda x: x[0]) if rows else None

    for group, name in ((2, "the plain Rust pool"), (5, "native expressions"), (3, "0.6.6"), (4, "the Python engines")):
        b = best(group)
        if not b:
            continue
        x = b[0] / vm["wall_s"]
        line = f"- against {name} (best: {b[1]}, {b[0]:.1f} s): {x:.2f}x"
        if group == 2:
            cpu_ok = vm["cpu_s"] is not None and b[2]["cpu_s"] is not None and vm["cpu_s"] <= b[2]["cpu_s"]
            met = x >= TARGET and cpu_ok
            line += (f"; the PM's target ({TARGET}x, CPU-seconds no worse: {vm['cpu_s']} against "
                     f"{b[2]['cpu_s']}) is {'met' if met else 'NOT met'}")
            if not met:
                gap = []
                if x < TARGET:
                    gap.append(f"wall {x:.2f}x of {TARGET}x")
                if not cpu_ok:
                    gap.append("CPU-seconds higher")
                line += f" (gap: {', '.join(gap)})"
        lines.append(line)
    return lines


def render(recs, machines):
    out = []
    for m in machines:
        out.append(f"## {m}: {gate_plan.MACHINES[m]['cpus']} CPUs, {gate_plan.MACHINES[m]['mem']} GiB\n")
        for wl in gate_plan.NYC:
            table, noise = rows_for(recs, m, wl)
            if not table:
                continue
            out.append(f"### {wl}\n")
            out.append("| Group | Engine | Function | Wall s | CPU s | Peak GiB | OOM | Boot s | Read+decode s | "
                       "Function s | Encode+write s | Waits s | Output | Runs ok |")
            out.append("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
            for e, r in sorted(table.items(), key=lambda kv: (kv[1]["group"], kv[0])):
                s = r["split"]
                out.append(f"| {r['group']} | {e} | {r['function']} | {fmt(r['wall_s'])} | {fmt(r['cpu_s'])} | "
                           f"{fmt(r['peak_gib'], 2)} | {r['oom']} | {fmt(r['boot_s'], 2)} | "
                           f"{fmt(s['read_decode_s'])} | {fmt(s['function_s'])} | {fmt(s['encode_write_s'])} | "
                           f"{fmt(s['wait_s'])} | {r['output']} | {r['ok']}/{r['runs']} |")
            out.append("")
            out.extend(speedups(table))
            if len(noise) >= 2:
                spread = (max(noise) - min(noise)) / statistics.median(noise)
                out.append(f"- noise: 0.6.6 against itself, {len(noise)} runs, spread {spread:.1%} of the median")
            out.append("")
    out.append("A split column is empty where the engine's report has no figure for it: the Python "
               "engines report none, and 0.6.6's report does not split decode from read or encode "
               "from write.")
    return "\n".join(out)


if __name__ == "__main__":
    recs = load(sys.argv[1])
    print(render(recs, sys.argv[2:] or list(gate_plan.MACHINES)))
