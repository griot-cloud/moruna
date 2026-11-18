"""The table of system design 6B, and the run's entry in architecture/benchmark-log.md.

Per job, a row per engine: the medians of operational time, start-up, CPU-seconds and peak
memory, the time split where the engine reports one, and whether its output is 0.6.6's. Under it,
the bar's two steps for Moruna, judged on operational time as bench/vm/report.py judges 0.7:

1. **No regression:** faster than both 0.6.6 rows (the Python and the Rust function).
2. **The bar:** at least 1.5 times the best other engine's operational time, with CPU-seconds no
   more than the least of any other row.

A miss names the stage of Moruna's split that takes the most time: 6C's first step.
"""
import os

TARGET = 1.5
ZERO_SIX = ("moruna066", "moruna066-rust")
NODE = "moruna-node"
STAGES = {"read_decode_s": "read and decode", "function_s": "the function", "encode_write_s": "encode and write",
          "wait_s": "waits"}


def fmt(x, nd=1):
    return "" if x is None else f"{x:.{nd}f}"


def losing_stage(row):
    split = {k: v for k, v in (row.get("split") or {}).items() if v}
    if not split:
        return None, None
    k = max(split, key=split.get)
    return STAGES[k], split[k]


def verdicts(engines: dict) -> list:
    """The bar's two steps for Moruna's row, as dicts: step, passed (or None), sentence."""
    node = engines.get(NODE)
    if not node or not node.get("operational_s"):
        return [{"step": "no regression", "passed": None, "text": "Moruna has no measured run"}]
    ops = node["operational_s"]
    out = []
    six = [(e, engines[e]) for e in ZERO_SIX if (engines.get(e) or {}).get("operational_s") is not None]
    if len(six) < len(ZERO_SIX):
        missing = [engines.get(e, {}).get("label", e) for e in ZERO_SIX if e not in dict(six)]
        out.append({"step": "no regression", "passed": None,
                    "text": f"no regression: unchecked, no row for {' or '.join(missing)}"})
    else:
        passed = all(ops < r["operational_s"] for _, r in six)
        parts = ", ".join(f"{r['label']} {r['operational_s']:.1f} s ({r['operational_s'] / ops:.2f}x)" for _, r in six)
        out.append({"step": "no regression", "passed": passed,
                    "text": f"**{'PASS' if passed else 'MISS'}** no regression: Moruna's operational {ops:.1f} s "
                            f"against {parts}"})
    others = {e: r for e, r in engines.items() if e != NODE and r.get("operational_s")}
    if not others:
        out.append({"step": "bar", "passed": None, "text": "the bar: unchecked, no other engine has a measured run"})
        return out
    best_ops, best = min((r["operational_s"], e) for e, r in others.items())
    cpus = [(r["cpu_s"], e) for e, r in others.items() if r.get("cpu_s") is not None]
    least_cpu, cheapest = min(cpus) if cpus else (None, None)
    cpu_ok = node.get("cpu_s") is not None and least_cpu is not None and node["cpu_s"] <= least_cpu
    x = best_ops / ops
    passed = x >= TARGET and cpu_ok
    out.append({"step": "bar", "passed": passed,
                "text": f"**{'PASS' if passed else 'MISS'}** the bar: {x:.2f}x the best engine's operational time "
                        f"({others[best]['label']}, {best_ops:.1f} s, against {ops:.1f} s; the bar is {TARGET}x), "
                        f"CPU {fmt(node.get('cpu_s'))} s against the least of any row "
                        f"({others[cheapest]['label'] if cheapest else 'none'}, {fmt(least_cpu)} s)"})
    if not all(v["passed"] for v in out if v["passed"] is not None):
        stage, secs = losing_stage(node)
        out.append({"step": "where", "passed": None,
                    "text": f"the stage that loses most: {stage}, {secs:.1f} s" if stage else
                            "the stage that loses most: unknown, the report has no split"})
    return out


def render(summary: dict) -> str:
    out = []
    for job, j in summary["jobs"].items():
        out.append(f"### {job}\n")
        out.append("| Engine | Function | Operational s | Start-up s | CPU s | Peak GiB | Memory given GiB | Data "
                   "| Read+decode s | Function s | Encode+write s | Waits s | Output against 0.6.6 | Runs ok |")
        out.append("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
        for r in j["engines"].values():
            s = r["split"]
            data = r.get("data") or ""
            if "local copy" in data:
                data += f", {fmt(r.get('copy_s'))} s apart"
            out.append(f"| {r['label']} | {r['function']} | {fmt(r['operational_s'])} | {fmt(r['startup_s'], 2)} | "
                       f"{fmt(r['cpu_s'])} | {fmt(r['peak_gib'], 2)} | {fmt(r.get('memory_gib'), 2)} | {data} | "
                       f"{fmt(s['read_decode_s'])} | {fmt(s['function_s'])} | {fmt(s['encode_write_s'])} | "
                       f"{fmt(s['wait_s'])} | {r['output']} | {r['ok']}/{r['runs']} |")
        out.append("")
        out.extend(f"- {v['text']}" for v in verdicts(j["engines"]))
        n = j.get("noise") or {}
        if n.get("spread") is not None:
            out.append(f"- noise: 0.6.6 against itself, {n['runs']} runs, spread {n['spread']:.1%} of the median")
        out.append("")
    out.append("Operational time runs from the job's being handed to a ready engine (Moruna: `POST /v1/jobs` "
               "answered; the others: the call that starts the job, after the session or cluster is up) to the "
               "output durably written in Blob. Start-up is Moruna's boot-to-ready and the others' process "
               "start, imports and session or cluster. Every engine reads and writes the run's one Blob "
               "container with its own reader inside operational time; a row marked `local copy` has no Blob "
               "reader, and its copies are timed apart. Memory given is what each platform leaves on the same "
               "machine: the node's memory less 768 MiB for Moruna, the plan's figure on Linux. Medians of the "
               "measured repeats; the learning run and the noise pairs are left out.")
    return "\n".join(out)


def entry(summary: dict, meta: dict, misses_text: str = "") -> str:
    """The run's section of architecture/benchmark-log.md."""
    lines = [f"## {meta['date']}: plan {summary['plan']}", ""]
    for k in ("commit", "node_image", "machines", "linux_image", "data", "results"):
        if meta.get(k):
            lines.append(f"- {k.replace('_', ' ').capitalize()}: {meta[k]}")
    lines += ["", render(summary)]
    if misses_text:
        lines += ["", "### Misses (6C)", "", misses_text]
    return "\n".join(lines) + "\n"


LOG_HEAD = ("# The benchmark log\n\nEvery run of the node benchmark and every fix it leads to (system design 6C). "
            "Written by bench/node/run.py; root causes are filled in by item 6.3.\n")


def append(log_path: str, text: str) -> None:
    new = not os.path.exists(log_path)
    with open(log_path, "a") as f:
        if new:
            f.write(LOG_HEAD)
        f.write("\n" + text)
