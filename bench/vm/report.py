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
        # A run's result; its engine's report (ID.report.json) and its job (ID.job.json) sit
        # beside it with no spec.
        try:
            rec = json.load(open(p))
        except ValueError:
            continue
        if isinstance(rec, dict) and isinstance(rec.get("spec"), dict):
            if rec["spec"].get("engine", "").startswith("moruna-vm") and "first_rows_s" not in rec:
                rec["first_rows_s"] = first_rows(os.path.join(results_dir, rec["spec"]["id"] + ".stdout"))
            out.append(rec)
    return out


def first_rows(stdout_path):
    """When a 0.7 run's first rows reached its sink, in seconds from the engine's start: the
    sink's task starting to write (its event's t_us), else the first heartbeat with rows out (to
    about a second). The engine's start-up is the time before it: the arena, the planes, the
    first morsel."""
    beat = None
    try:
        lines = open(stdout_path, errors="replace")
    except OSError:
        return None
    for line in lines:
        try:
            e = json.loads(line)
        except ValueError:
            continue
        if e.get("type") == "event" and e.get("kind") == "task_started" and e.get("task") == "sink":
            return e["t_us"] / 1e6
        if beat is None and e.get("type") == "heartbeat" and (e.get("rows_out") or 0) > 0:
            beat = e["t_ms"] / 1e3
    return beat


def phases(rec):
    """A 0.7 run's wall in three (Brackly, 2026-11-17): the VM's start-up (the node built, the
    inputs staged, QEMU launched, the boot: all before the engine's job begins), the engine's
    start-up (before its first rows), and the operational time (the first rows to the output
    written, the outputs read off the output disk included). The bar is judged on the last."""
    vm = rec.get("vm") or {}
    job, outputs = vm.get("job_s"), vm.get("outputs_s") or 0
    if job is None:
        return None, None, None
    vm_start = rec["wall_s"] - job - outputs
    engine_start = rec.get("first_rows_s")
    operational = job + outputs - (engine_start or 0)
    return vm_start, engine_start, operational


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
            # 0.7's wall in three (phases): the bar is judged on the operational time.
            "vm_start_s": med([phases(r)[0] for r in ok]) if e.startswith("moruna-vm") else None,
            "engine_start_s": med([phases(r)[1] for r in ok]) if e.startswith("moruna-vm") else None,
            "operational_s": med([phases(r)[2] for r in ok]) if e.startswith("moruna-vm") else None,
            "cpu_s": med([r["cpu_s"] for r in ok]),
            "peak_gib": med([r["cg_peak_bytes"] / (1 << 30) for r in ok]),
            "oom": sum(r.get("oom_kills", 0) for r in runs),
            "boot_s": med([r.get("boot_s") for r in ok]),
            "vm": {k: med([(r.get("vm") or {}).get(k) for r in ok])
                   for k in ("launch_s", "boot_s", "stage_s", "node_s", "job_s", "outputs_s", "total_s")},
            "split": {k: med([s.get(k) for s in splits]) for k in
                      ("read_decode_s", "function_s", "encode_write_s", "wait_s")},
            "output": match(ok[0], ref, workload) if ok else "no run passed",
        }
    noise = [r["wall_s"] for r in mine if r["spec"].get("noise") and r.get("ok")]
    return table, noise


def fmt(x, nd=1):
    return "" if x is None else f"{x:.{nd}f}"


# Brackly's first gate (2026-11-17): no regressions. 0.7 must be faster than both 0.6.6 rows, its
# Rust function and its Python one, on every job before the 1.5x bar applies.
SIX = (("moruna066-rust", "0.6.6 with the Rust function"), ("moruna", "0.6.6 with the Python function"))


def no_regression(table):
    """The first gate's verdict for 0.7's row: (verdict, the sentence). 0.7's operational time
    against each 0.6.6 row's wall, as the bar is judged; unchecked when a 0.6.6 row is missing."""
    vm = table.get("moruna-vm")
    if not vm or vm.get("operational_s") is None:
        return None, None
    missing = [name for e, name in SIX if not (table.get(e) or {}).get("wall_s")]
    if missing:
        return "unchecked", f"no regression: unchecked, no row for {' or '.join(missing)}"
    ops = vm["operational_s"]
    parts = [f"{name} {table[e]['wall_s']:.1f} s ({table[e]['wall_s'] / ops:.2f}x)" for e, name in SIX]
    verdict = "PASS" if all(ops < table[e]["wall_s"] for e, _ in SIX) else "MISS"
    return verdict, (f"**{verdict}** no regression (the first gate: faster than both 0.6.6 rows): 0.7's "
                     f"operational {ops:.1f} s against {', '.join(parts)}; with start-up {vm['wall_s']:.1f} s")


def speedups(table):
    """0.7 against every other row. Brackly's bar (2026-11-17): PASS when 0.7's median wall is at
    least 1.5 times faster than the best other row's and its CPU-seconds are no more than any
    row's; a miss names where 0.7's time goes."""
    vm = table.get("moruna-vm")
    if not vm or vm["wall_s"] is None:
        return ["0.7 (moruna-vm) has no measured run."]
    others = {e: r for e, r in table.items() if not e.startswith("moruna-vm") and r["wall_s"]}
    operational = vm.get("operational_s")
    _, gate = no_regression(table)
    lines = [f"- {gate}"] if gate else []
    for group, name in ((2, "the plain Rust pool"), (5, "native expressions"), (3, "0.6.6"),
                        (4, "the Python engines")):
        rows = [(r["wall_s"], e) for e, r in others.items() if r["group"] == group]
        if rows:
            w, e = min(rows)
            ops = f"{w / operational:.2f}x on operational time; " if operational else ""
            lines.append(f"- against {name} (best: {e}, {w:.1f} s): {ops}{w / vm['wall_s']:.2f}x with start-up")
    if not others:
        return lines
    best_wall, best = min((r["wall_s"], e) for e, r in others.items())
    least_cpu, cheapest = min((r["cpu_s"], e) for e, r in others.items() if r["cpu_s"] is not None)
    cpu_ok = vm["cpu_s"] is not None and vm["cpu_s"] <= least_cpu
    passed = True
    for label, wall in (("on operational time (the bar)", operational),
                        ("with start-up (the total)", vm["wall_s"])):
        if not wall:
            continue
        x = best_wall / wall
        ok = x >= TARGET and cpu_ok
        passed = passed and ok
        lines.append(f"- **{'PASS' if ok else 'MISS'}** {label}: {x:.2f}x the best row's wall ({best}, "
                     f"{best_wall:.1f} s, against {wall:.1f} s; the bar is {TARGET}x), CPU {vm['cpu_s']} s "
                     f"against the least of any row ({cheapest}, {least_cpu} s)")
    if not passed:
        s, v = vm["split"], vm.get("vm") or {}
        parts = {"read and decode": s.get("read_decode_s"), "the function": s.get("function_s"),
                 "encode and write": s.get("encode_write_s"), "waits": s.get("wait_s"),
                 "the VM's start-up": vm.get("vm_start_s"), "the engine's start-up": vm.get("engine_start_s"),
                 "outputs": v.get("outputs_s")}
        known = {k: x for k, x in parts.items() if x}
        job = v.get("job_s")
        where = ", ".join(f"{k} {x:.1f} s" for k, x in sorted(known.items(), key=lambda kv: -kv[1]))
        lines.append(f"  - where 0.7's time goes: {where or 'no split'}; the VM's job {job} s of "
                     f"its total {v.get('total_s')} s; the wall {vm['wall_s']:.1f} s")
        if known:
            lines.append(f"  - the stage that loses most: {max(known, key=known.get)}")
    return lines


def sizes_line(path=os.environ.get("GATE_SIZES", "/home/bench/gate/sizes.json")):
    """The images a 0.7 run boots and attaches, from the setup's sizes.json, in MB."""
    try:
        z = json.load(open(path))
    except (OSError, ValueError):
        return None
    mb = lambda b: f"{b / 1e6:.1f} MB"
    parts = ", ".join(f"{k} {mb(v)}" for k, v in z.get("booted", {}).items())
    return (f"Images: the node 0.7 boots {mb(sum(z.get('booted', {}).values()))} ({parts}); the gate task "
            f"image {mb(z.get('gate_image', 0))} (its tasks {mb(z.get('gate_tasks', 0))})")


def render(recs, machines):
    out = []
    line = sizes_line()
    if line:
        out.append(line + "\n")
    for m in machines:
        out.append(f"## {m}: {gate_plan.MACHINES[m]['cpus']} CPUs, {gate_plan.MACHINES[m]['mem']} GiB\n")
        for wl in gate_plan.NYC:
            table, noise = rows_for(recs, m, wl)
            if not table:
                continue
            out.append(f"### {wl}\n")
            out.append("| Group | Engine | Function | Operational s | Total s | No regression | Start-up: VM s | "
                       "Start-up: engine s | CPU s | Peak GiB | OOM | Launch s | Boot s | Stage s | Read+decode s | "
                       "Function s | Encode+write s | Waits s | Output | Runs ok |")
            out.append("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- "
                       "| --- | --- | --- | --- |")
            gate, _ = no_regression(table)
            for e, r in sorted(table.items(), key=lambda kv: (kv[1]["group"], kv[0])):
                s = r["split"]
                v = r.get("vm") or {}
                ops = r.get("operational_s") if r.get("operational_s") is not None else r["wall_s"]
                out.append(f"| {r['group']} | {e} | {r['function']} | {fmt(ops)} | {fmt(r['wall_s'])} | "
                           f"{gate if e == 'moruna-vm' and gate else ''} | "
                           f"{fmt(r.get('vm_start_s'), 2)} | {fmt(r.get('engine_start_s'), 2)} | {fmt(r['cpu_s'])} | "
                           f"{fmt(r['peak_gib'], 2)} | {r['oom']} | {fmt(v.get('launch_s'), 2)} | "
                           f"{fmt(v.get('boot_s'), 2)} | {fmt(v.get('stage_s'), 2)} | "
                           f"{fmt(s['read_decode_s'])} | {fmt(s['function_s'])} | {fmt(s['encode_write_s'])} | "
                           f"{fmt(s['wait_s'])} | {r['output']} | {r['ok']}/{r['runs']} |")
            out.append("")
            out.extend(speedups(table))
            if len(noise) >= 2:
                spread = (max(noise) - min(noise)) / statistics.median(noise)
                out.append(f"- noise: 0.6.6 against itself, {len(noise)} runs, spread {spread:.1%} of the median")
            out.append("")
    out.append("Operational time is a run's first rows to its output written; 0.7's start-up is apart: "
               "the VM's (the node built, the inputs staged, the launch and the boot; gone in mode 1, where "
               "Moruna is the node) and the engine's (before its first rows: the arena, the planes, the first "
               "morsel). A control's own start (its process, imports, session) stays inside its wall, as "
               "measured, so its operational time is its wall. The bar is judged on operational time, after "
               "the first gate: no regressions, 0.7 faster than both 0.6.6 rows (the Rust and the Python "
               "function) on every job.\n")
    out.append("A split column is empty where the engine's report has no figure for it: the Python "
               "engines report none, and 0.6.6's report does not split decode from read or encode "
               "from write.")
    return "\n".join(out)


if __name__ == "__main__":
    recs = load(sys.argv[1])
    print(render(recs, sys.argv[2:] or list(gate_plan.MACHINES)))
