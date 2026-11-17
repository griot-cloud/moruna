"""Writes the run plans. make_plan.py preflight|grid|main|...|gate-nyc|gate-ooc MACHINE [--no-066-rust]"""
import json, sys, glob
D = "/data/nyc/norm/yellow_tripdata_"
ALL = sorted(glob.glob(D + "*.parquet"))
Y2024 = [f for f in ALL if "_2024-" in f]
Q1 = Y2024[:3]
Y3 = [f for f in ALL if any(f"_{y}-" in f for y in (2022, 2023, 2024))]
WL = ["enrich", "audit", "occupancy"]
mode = sys.argv[1]
plan = []
def add(id, engine, wl, files, cpus=8, mem=24, **kw):
    plan.append({"id": id, "engine": engine, "workload": wl, "files": files, "cpus": cpus, "mem_gib": mem, **kw})
if mode == "preflight":
    for wl in WL:
        for e in ["moruna", "loop", "polars", "duckdb"]:
            add(f"pre-{wl}-{e}", e, wl, Y2024[:1], threads=8, timeout_s=300)
elif mode == "grid":
    for wl in WL:
        for t in (8, 16):
            for b in (0, 250_000):
                add(f"grid-{wl}-t{t}-b{b}", "loop", wl, Q1, threads=t, batch=b)
elif mode == "main":
    best = {}
    for wl in WL:
        runs = [json.load(open(p)) for p in glob.glob(f"/data/results/grid-{wl}-*.json")]
        runs = [r for r in runs if r["ok"]]
        b = min(runs, key=lambda r: r["wall_s"])["spec"]
        best[wl] = {"threads": b["threads"], "batch": b["batch"]}
    json.dump(best, open("/data/results/loop-best.json", "w"))
    L = lambda wl, **o: {**best[wl], **o}
    # 1. speed, 2024 (41.2M trips), 8 CPUs, 24 GiB
    for wl in WL:
        for r in (1, 2, 3):
            add(f"speed-{wl}-moruna-r{r}", "moruna", wl, Y2024)
            add(f"speed-{wl}-loop-r{r}", "loop", wl, Y2024, **L(wl))
            if wl != "audit" or r == 1:
                add(f"speed-{wl}-polars-r{r}", "polars", wl, Y2024, timeout_s=900)
                add(f"speed-{wl}-duckdb-r{r}", "duckdb", wl, Y2024, timeout_s=900)
    # 2. the whole dataset (259M trips, ~37 GB in memory, more than the machine)
    for wl in ("enrich", "occupancy"):
        add(f"full-{wl}-moruna", "moruna", wl, ALL)
        add(f"full-{wl}-loop", "loop", wl, ALL, **L(wl))
    # 3. CPU scaling, audit on 2024 Q1
    for c in (1, 2, 4, 8):
        add(f"scale-audit-moruna-c{c}", "moruna", "audit", Q1, cpus=c)
        add(f"scale-audit-loop-c{c}", "loop", "audit", Q1, cpus=c, threads=c, batch=best["audit"]["batch"])
    # 4. tight memory, occupancy on 2024
    for m in (2, 4, 8, 16):
        for e in ("moruna", "loop", "polars", "duckdb"):
            add(f"mem-occupancy-{e}-m{m}", e, "occupancy", Y2024, mem=m, timeout_s=900, **(L("occupancy") if e == "loop" else {}))
    # 5. live squeeze: notice through memory.high, then the kill line 5 s later; restore later
    sq_a = [{"t": 20, "cpus": 4, "high_gib": 6}, {"t": 25, "mem_gib": 6}, {"t": 60, "cpus": 8, "mem_gib": 24, "high_gib": None}]
    sq_o = [{"t": 15, "cpus": 4, "high_gib": 4}, {"t": 20, "mem_gib": 4}, {"t": 45, "cpus": 8, "mem_gib": 24, "high_gib": None}]
    for e in ("moruna", "loop"):
        add(f"squeeze-audit-{e}", e, "audit", Y3, schedule=sq_a, **(L("audit") if e == "loop" else {}))
        add(f"squeeze-occupancy-{e}", e, "occupancy", ALL, schedule=sq_o, **(L("occupancy") if e == "loop" else {}))
    # 6. Moruna with its disk tier on (the staging crash, for the record)
    add("staging-enrich-moruna", "moruna", "enrich", Y2024, staging="/data/staging")
elif mode == "rtpre":
    for wl in ("audit", "score", "occupancy"):
        for e in ("spark", "ray", "dask", "duckdbudf", "moruna", "loop"):
            if e == "duckdbudf" and wl == "occupancy": continue
            if e in ("moruna", "loop") and wl != "score": continue
            add(f"rtpre-{wl}-{e}", e, wl, Y2024[:1], threads=16, batch=250_000, timeout_s=300)
elif mode == "rt":
    RT = ("spark", "ray", "dask")
    # speed, 2024: the new runtimes on the three UDF jobs; Moruna and the loop on score (audit/occupancy already ran)
    for r in (1, 2):
        for wl in ("audit", "score", "occupancy"):
            for e in RT + (("duckdbudf",) if wl != "occupancy" else ()):
                add(f"speed-{wl}-{e}-r{r}", e, wl, Y2024, timeout_s=900)
    for r in (1, 2, 3):
        add(f"speed-score-moruna-r{r}", "moruna", "score", Y2024)
        add(f"speed-score-loop-r{r}", "loop", "score", Y2024, threads=16, batch=250_000)
    # why the loop wins: Moruna with its allocator guard off
    add("guard-audit-morunang-c1", "morunang", "audit", Q1, cpus=1)
    add("guard-audit-morunang-c8", "morunang", "audit", Q1, cpus=8)
    add("guard-occupancy-morunang-m2", "morunang", "occupancy", Y2024, mem=2)
    # tight memory, occupancy on 2024
    for m in (2, 4, 8):
        for e in RT:
            add(f"mem-occupancy-{e}-m{m}", e, "occupancy", Y2024, mem=m, timeout_s=900)
    # live squeeze, audit on 2022-2024
    sq_a = [{"t": 20, "cpus": 4, "high_gib": 6}, {"t": 25, "mem_gib": 6}, {"t": 60, "cpus": 8, "mem_gib": 24, "high_gib": None}]
    for e in RT:
        add(f"squeeze-audit-{e}", e, "audit", Y3, schedule=sq_a, timeout_s=900)
elif mode == "rerun":
    # fix/adaptive-runtime (2026-11-14): every engine writes zstd. The loop keeps the settings
    # its grid chose in the 0.6.5 run.
    best = {"enrich": {"threads": 16, "batch": 250000}, "audit": {"threads": 16, "batch": 250000},
            "occupancy": {"threads": 8, "batch": 250000}, "score": {"threads": 16, "batch": 250000}}
    L = lambda wl, **o: {**best[wl], **o}
    W4 = ["enrich", "audit", "score", "occupancy"]
    # 1. speed, 2024 (41.2M trips), 8 CPUs, 24 GiB
    for wl in W4:
        for r in (1, 2):
            add(f"speed-{wl}-moruna-r{r}", "moruna", wl, Y2024)
            add(f"speed-{wl}-loop-r{r}", "loop", wl, Y2024, **L(wl))
    for wl in ("audit", "score", "occupancy"):
        for e in ("ray", "dask", "spark") + (("duckdbudf",) if wl != "occupancy" else ()):
            add(f"speed-{wl}-{e}-r1", e, wl, Y2024, timeout_s=900)
    # 2. the whole dataset (259M trips)
    for wl in ("enrich", "occupancy"):
        add(f"full-{wl}-moruna", "moruna", wl, ALL)
        add(f"full-{wl}-loop", "loop", wl, ALL, **L(wl))
    # 3. tight memory, occupancy on 2024
    for m in (2, 4, 8):
        add(f"mem-occupancy-moruna-m{m}", "moruna", "occupancy", Y2024, mem=m, timeout_s=900)
        add(f"mem-occupancy-loop-m{m}", "loop", "occupancy", Y2024, mem=m, timeout_s=900, **L("occupancy"))
    # 4. live squeeze
    sq_a = [{"t": 20, "cpus": 4, "high_gib": 6}, {"t": 25, "mem_gib": 6}, {"t": 60, "cpus": 8, "mem_gib": 24, "high_gib": None}]
    sq_o = [{"t": 15, "cpus": 4, "high_gib": 4}, {"t": 20, "mem_gib": 4}, {"t": 45, "cpus": 8, "mem_gib": 24, "high_gib": None}]
    for e in ("moruna", "loop"):
        add(f"squeeze-audit-{e}", e, "audit", Y3, schedule=sq_a, **(L("audit") if e == "loop" else {}))
        add(f"squeeze-occupancy-{e}", e, "occupancy", ALL, schedule=sq_o, **(L("occupancy") if e == "loop" else {}))
    # 5. Moruna with its disk tier on
    add("staging-enrich-moruna", "moruna", "enrich", Y2024, staging="/data/staging")
elif mode == "rerun2":
    # f2fe65e7: the lazy arena, the oversized-file and refused-output fixes. Moruna only; the
    # loop and the other runtimes stand from "rerun".
    for wl in ["enrich", "audit", "score", "occupancy"]:
        for r in (1, 2):
            add(f"b2-speed-{wl}-moruna-r{r}", "moruna", wl, Y2024)
    for wl in ("enrich", "occupancy"):
        add(f"b2-full-{wl}-moruna", "moruna", wl, ALL)
    add("b2-mem-occupancy-moruna-m2", "moruna", "occupancy", Y2024, mem=2, timeout_s=900)
    sq_a = [{"t": 20, "cpus": 4, "high_gib": 6}, {"t": 25, "mem_gib": 6}, {"t": 60, "cpus": 8, "mem_gib": 24, "high_gib": None}]
    sq_o = [{"t": 15, "cpus": 4, "high_gib": 4}, {"t": 20, "mem_gib": 4}, {"t": 45, "cpus": 8, "mem_gib": 24, "high_gib": None}]
    add("b2-squeeze-audit-moruna", "moruna", "audit", Y3, schedule=sq_a)
    add("b2-squeeze-occupancy-moruna", "moruna", "occupancy", ALL, schedule=sq_o)
    add("b2-staging-enrich-moruna", "moruna", "enrich", Y2024, staging="/data/staging")
elif mode == "rerun3":
    # 2026-11-14 afternoon: growth by default, memory walls make room, side regions, workers
    # funded at real morsel size, named staging directories made. Moruna runs with its disk
    # tier on, as a user gets it by default; the loop reruns on this VM so every comparison is
    # within one machine.
    best = {"enrich": {"threads": 16, "batch": 250000}, "audit": {"threads": 16, "batch": 250000},
            "occupancy": {"threads": 8, "batch": 250000}, "score": {"threads": 16, "batch": 250000}}
    L = lambda wl, **o: {**best[wl], **o}
    S = {"staging": "/data/staging"}
    W4 = ["enrich", "audit", "score", "occupancy"]
    # 1. speed, 2024 (41.2M trips), 8 CPUs, 24 GiB
    for r in (1, 2):
        for wl in W4:
            add(f"c3-speed-{wl}-moruna-r{r}", "moruna", wl, Y2024, **S)
            add(f"c3-speed-{wl}-loop-r{r}", "loop", wl, Y2024, **L(wl))
    # 2. the whole dataset (259M trips)
    for wl in ("enrich", "occupancy"):
        add(f"c3-full-{wl}-moruna", "moruna", wl, ALL, trace=True, **S)
        add(f"c3-full-{wl}-loop", "loop", wl, ALL, **L(wl))
    # 3. memory budgets, occupancy on 2024
    for m in (1, 2, 4, 8):
        add(f"c3-mem-occupancy-moruna-m{m}", "moruna", "occupancy", Y2024, mem=m, timeout_s=900, trace=(m == 1), **S)
        add(f"c3-mem-occupancy-loop-m{m}", "loop", "occupancy", Y2024, mem=m, timeout_s=900, **L("occupancy"))
    # 4. live squeeze (taken away), and a machine that grows (given)
    sq_a = [{"t": 20, "cpus": 4, "high_gib": 6}, {"t": 25, "mem_gib": 6}, {"t": 60, "cpus": 8, "mem_gib": 24, "high_gib": None}]
    sq_o = [{"t": 15, "cpus": 4, "high_gib": 4}, {"t": 20, "mem_gib": 4}, {"t": 45, "cpus": 8, "mem_gib": 24, "high_gib": None}]
    grow = [{"t": 10, "cpus": 8, "mem_gib": 24, "high_gib": None}]
    for e in ("moruna", "loop"):
        m = dict(trace=True, **S) if e == "moruna" else {}
        add(f"c3-squeeze-audit-{e}", e, "audit", Y3, schedule=sq_a, **(L("audit") if e == "loop" else m))
        add(f"c3-squeeze-occupancy-{e}", e, "occupancy", ALL, schedule=sq_o, **(L("occupancy") if e == "loop" else m))
        add(f"c3-grow-occupancy-{e}", e, "occupancy", ALL, cpus=4, mem=4, schedule=grow, **(L("occupancy") if e == "loop" else m))
    # 5. memory the data does not fit: the disk tier has to carry it
    for e in ("moruna", "loop"):
        m = dict(trace=True, **S) if e == "moruna" else {}
        add(f"c3-small-score-{e}-m4", e, "score", Y2024, mem=4, timeout_s=900, **(L("score") if e == "loop" else m))
        add(f"c3-small-enrich-{e}-m4", e, "enrich", ALL, mem=4, timeout_s=1200, **(L("enrich") if e == "loop" else m))
    # 6. where the CPU goes, on Linux: perf on one run each
    for wl in ("enrich", "occupancy", "audit"):
        add(f"c3-perf-{wl}-moruna", "moruna", wl, Y2024, perf=True, **S)
        add(f"c3-perf-{wl}-loop", "loop", wl, Y2024, perf=True, **L(wl))
elif mode == "rerun4":
    # 2026-11-14 evening, afb3eac7: fixed resources only (production is one node with fixed
    # memory and CPUs). Moruna with its disk tier on, as shipped; the loop reruns on this VM.
    best = {"enrich": {"threads": 16, "batch": 250000}, "audit": {"threads": 16, "batch": 250000},
            "occupancy": {"threads": 8, "batch": 250000}, "score": {"threads": 16, "batch": 250000}}
    L = lambda wl, **o: {**best[wl], **o}
    S = {"staging": "/data/staging"}
    W4 = ["enrich", "audit", "score", "occupancy"]
    for r in (1, 2):
        for wl in W4:
            add(f"c4-speed-{wl}-moruna-r{r}", "moruna", wl, Y2024, **S)
            add(f"c4-speed-{wl}-loop-r{r}", "loop", wl, Y2024, **L(wl))
    for wl in ("enrich", "occupancy"):
        add(f"c4-full-{wl}-moruna", "moruna", wl, ALL, trace=True, **S)
        add(f"c4-full-{wl}-loop", "loop", wl, ALL, **L(wl))
    for m in (1, 2, 4, 8):
        add(f"c4-mem-occupancy-moruna-m{m}", "moruna", "occupancy", Y2024, mem=m, timeout_s=900, **S)
        add(f"c4-mem-occupancy-loop-m{m}", "loop", "occupancy", Y2024, mem=m, timeout_s=900, **L("occupancy"))
    for e in ("moruna", "loop"):
        m = dict(trace=True, **S) if e == "moruna" else {}
        add(f"c4-small-score-{e}-m4", e, "score", Y2024, mem=4, timeout_s=900, **(L("score") if e == "loop" else m))
        add(f"c4-small-enrich-{e}-m4", e, "enrich", ALL, mem=4, timeout_s=1200, **(L("enrich") if e == "loop" else m))
    for wl in ("enrich", "occupancy", "audit"):
        add(f"c4-perf-{wl}-moruna", "moruna", wl, Y2024, perf=True, **S)
        add(f"c4-perf-{wl}-loop", "loop", wl, Y2024, perf=True, **L(wl))
elif mode == "rerun5":
    # 9c2aa2e8: freed NumPy blocks kept for reuse. Fixed resources; the loop on this VM.
    best = {"enrich": {"threads": 16, "batch": 250000}, "audit": {"threads": 16, "batch": 250000},
            "occupancy": {"threads": 8, "batch": 250000}, "score": {"threads": 16, "batch": 250000}}
    L = lambda wl, **o: {**best[wl], **o}
    S = {"staging": "/data/staging"}
    for r in (1, 2):
        for wl in ["enrich", "audit", "score", "occupancy"]:
            add(f"c5-speed-{wl}-moruna-r{r}", "moruna", wl, Y2024, **S)
            add(f"c5-speed-{wl}-loop-r{r}", "loop", wl, Y2024, **L(wl))
    add("c5-full-occupancy-moruna", "moruna", "occupancy", ALL, **S)
    add("c5-full-occupancy-loop", "loop", "occupancy", ALL, **L("occupancy"))
    add("c5-mem-occupancy-moruna-m1", "moruna", "occupancy", Y2024, mem=1, timeout_s=900, **S)
    add("c5-small-score-moruna-m4", "moruna", "score", Y2024, mem=4, timeout_s=900, **S)
    for wl in ("occupancy", "enrich"):
        add(f"c5-perf-{wl}-moruna", "moruna", wl, Y2024, perf=True, **S)
        add(f"c5-perf-{wl}-loop", "loop", wl, Y2024, perf=True, **L(wl))
elif mode == "rerun6":
    # Moruna's own large buffers reused within a run; the guard's per-object path slimmed.
    best = {"enrich": {"threads": 16, "batch": 250000}, "audit": {"threads": 16, "batch": 250000},
            "occupancy": {"threads": 8, "batch": 250000}, "score": {"threads": 16, "batch": 250000}}
    L = lambda wl, **o: {**best[wl], **o}
    S = {"staging": "/data/staging"}
    for r in (1, 2):
        for wl in ["enrich", "audit", "score", "occupancy"]:
            add(f"c6-speed-{wl}-moruna-r{r}", "moruna", wl, Y2024, **S)
            add(f"c6-speed-{wl}-loop-r{r}", "loop", wl, Y2024, **L(wl))
    add("c6-full-enrich-moruna", "moruna", "enrich", ALL, **S)
    add("c6-full-enrich-loop", "loop", "enrich", ALL, **L("enrich"))
    # F8.9's ground: small ceilings, the whole process under them.
    add("c6-mem-occupancy-moruna-m1", "moruna", "occupancy", Y2024, mem=1, timeout_s=900, **S)
    add("c6-mem-enrich-moruna-m2", "moruna", "enrich", Y2024, mem=2, timeout_s=900, **S)
    add("c6-small-score-moruna-m4", "moruna", "score", Y2024, mem=4, timeout_s=900, **S)
    for wl in ("audit", "enrich"):
        add(f"c6-perf-{wl}-moruna", "moruna", wl, Y2024, perf=True, **S)
        add(f"c6-perf-{wl}-loop", "loop", wl, Y2024, perf=True, **L(wl))
elif mode == "rerun7":
    # 5d01c17e: probes fixed, small Python requests past the guard, repeat runs sized from the
    # runs before them. Then two datasets far larger than memory.
    best = {"enrich": {"threads": 16, "batch": 250000}, "audit": {"threads": 16, "batch": 250000},
            "occupancy": {"threads": 8, "batch": 250000}, "score": {"threads": 16, "batch": 250000}}
    L = lambda wl, **o: {**best[wl], **o}
    S = {"staging": "/data/staging"}
    # A. NYC 2024: run 1 starts from an empty profile store (a first run); runs 2 and 3 keep it.
    for wl in ["enrich", "audit", "score", "occupancy"]:
        P = {"profiles": f"/data/profiles/{wl}"}
        add(f"c7-speed-{wl}-moruna-r1", "moruna", wl, Y2024, fresh_profiles=True, **P, **S)
        add(f"c7-speed-{wl}-loop-r1", "loop", wl, Y2024, **L(wl))
        add(f"c7-speed-{wl}-moruna-r2", "moruna", wl, Y2024, **P, **S)
        add(f"c7-speed-{wl}-loop-r2", "loop", wl, Y2024, **L(wl))
        add(f"c7-speed-{wl}-moruna-r3", "moruna", wl, Y2024, **P, **S)
    # The cost model on (moruna.CostSession), its store and the profiles both fresh for run 1.
    for wl in ["enrich", "audit", "score", "occupancy"]:
        C = {"profiles": f"/data/profiles/cost-{wl}", "cost": f"/data/cost/{wl}"}
        add(f"c7-cost-{wl}-moruna-r1", "moruna", wl, Y2024, fresh_profiles=True, fresh_cost=True, **C, **S)
        add(f"c7-cost-{wl}-moruna-r2", "moruna", wl, Y2024, **C, **S)
        add(f"c7-cost-{wl}-moruna-r3", "moruna", wl, Y2024, **C, **S)
    for wl in ("audit", "enrich"):
        add(f"c7-perf-{wl}-moruna", "moruna", wl, Y2024, perf=True, profiles=f"/data/profiles/{wl}", **S)
        add(f"c7-perf-{wl}-loop", "loop", wl, Y2024, perf=True, **L(wl))
    # B. Out of core: FineWeb text and Overture buildings, several times the 24 GiB limit.
    FW = sorted(glob.glob("/data/fineweb/*.parquet")); OV = sorted(glob.glob("/data/overture/*.parquet"))
    LO = {"fw_clean": {"threads": 16, "batch": 20000}, "fw_minhash": {"threads": 16, "batch": 20000},
          "ov_shape": {"threads": 16, "batch": 250000}, "ov_attrs": {"threads": 16, "batch": 250000}}
    T = {"timeout_s": 5400}
    for wl, files in (("ov_shape", OV), ("ov_attrs", OV), ("fw_minhash", FW), ("fw_clean", FW)):
        add(f"c7-ooc-{wl}-moruna", "moruna", wl, files, profiles=f"/data/profiles/{wl}", fresh_profiles=True, **T, **S)
        add(f"c7-ooc-{wl}-loop", "loop", wl, files, **LO[wl], **T)
        add(f"c7-ooc-{wl}-ray", "ray", wl, files, batch=LO[wl]["batch"], **T)
        add(f"c7-ooc-{wl}-moruna065", "moruna065", wl, files, **T, **S)
    # C. The same jobs on a smaller node: 8 GiB.
    for wl, files in (("ov_shape", OV), ("fw_minhash", FW)):
        add(f"c7-ooc-{wl}-moruna-m8", "moruna", wl, files, mem=8, profiles=f"/data/profiles/{wl}", **T, **S)
        add(f"c7-ooc-{wl}-loop-m8", "loop", wl, files, mem=8, **LO[wl], **T)
        add(f"c7-ooc-{wl}-ray-m8", "ray", wl, files, mem=8, batch=LO[wl]["batch"], **T)
elif mode == "gate-nyc":
    # The PM's NYC matrix for the 0.7 gate (arrow-core-0.7.md section 9): the 2024 set, one
    # learning run (r0) and five measured repeats (r1-r5) of every row, the order reversed on every
    # other repeat; then two pairs of 0.6.6 against itself for the session's noise. The machine is
    # the second argument: d8 (D8ds_v6) first, then d4 (D4ls_v5).
    import gate_plan
    machine = sys.argv[2] if len(sys.argv) > 2 else "d8"
    # --core: groups 1, 2, 3 and 5 first, with the noise pairs; --rest: group 4 after.
    groups = (1, 2, 3, 5) if "--core" in sys.argv else (4,) if "--rest" in sys.argv else (1, 2, 3, 4, 5)
    plan = gate_plan.nyc(Y2024, machine, with_066_rust="--no-066-rust" not in sys.argv, groups=groups,
                         noise="--rest" not in sys.argv)
    mode = f"gate-nyc-{machine}" + ("-core" if "--core" in sys.argv else "-rest" if "--rest" in sys.argv else "")
    # --no-vm: the rows that boot no Moruna VM (they may run while 0.7's kernel is held); --vm: 0.7's.
    if "--no-vm" in sys.argv:
        plan = [p for p in plan if not p["engine"].startswith("moruna-vm")]
        mode += "-controls"
    elif "--vm" in sys.argv:
        plan = [p for p in plan if p["engine"].startswith("moruna-vm")]
        mode += "-vm"
elif mode == "gate-ooc":
    # The large jobs, once per build: Overture and FineWeb as rerun7 downloaded them.
    import gate_plan
    machine = sys.argv[2] if len(sys.argv) > 2 else "d8"
    files = {"overture": sorted(glob.glob("/data/overture/*.parquet")), "fineweb": sorted(glob.glob("/data/fineweb/*.parquet"))}
    plan = gate_plan.ooc(files, machine, with_066_rust="--no-066-rust" not in sys.argv)
    mode = f"gate-ooc-{machine}"
json.dump(plan, open(f"/data/results/plan-{mode}.json", "w"), indent=1)
print(mode, len(plan), "runs")
