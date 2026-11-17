"""The gate's run plans (make_plan.py gate-nyc): the PM's NYC matrix, as plan entries driver.py
runs. Kept apart from make_plan.py, which globs /data at import, so it can be tested anywhere.

Rows, each labelled by the language its function is in (`function` in a run's result):

1. moruna-vm: 0.7 on the data OS, the Rust function as a native task.
2. rust-baseline: the plain Rust thread pool, the Rust function (control 2).
3. moruna: 0.6.6, the Python function; moruna066-rust: 0.6.6, the Rust function through
   images/gate/py066 (control 1).
4. loop, polars, duckdb, ray, spark, dask: the Python function, as runner.py runs them.
5. duckdb-sql (Enrich, Occupancy) and polars-expr (Enrich): native expressions.
"""

MACHINES = {
    # The 0.6.6 table's limits on the D8ds_v6; the D4ls_v5 (4 vCPU, 8 GiB) keeps 2 GiB for the OS.
    "d8": {"cpus": 8, "mem": 24},
    "d4": {"cpus": 4, "mem": 6},
}
NYC = ("score", "occupancy", "enrich", "audit")
GROUPS = {
    1: ("moruna-vm", "moruna-vm-free1"),
    2: ("rust-baseline",),
    3: ("moruna", "moruna066-rust"),
    4: ("loop", "polars", "duckdb", "ray", "spark", "dask"),
    5: ("duckdb-sql", "polars-expr"),
}
NATIVE_EXPR = {"duckdb-sql": ("enrich", "occupancy"), "polars-expr": ("enrich",)}
# 0.7 with one vCPU left to the engine (tasks = vCPUs - 1), against all vCPUs: whether anything
# of the `share` stall (fixed in bdf24e1f) is left. Occupancy and Enrich.
FREE1 = ("occupancy", "enrich")
# 0.7's VM (stream D): a 24 GiB VM takes a job budget of 20.6 GiB or less; its cgroup holds the
# guest's memory and QEMU's.
VM = {"vm_memory_gib": 24, "budget_gib": 20.5, "mem": 27}
# The engine's scratch holds a run's outputs until the output disk takes them, inside the VM's
# memory, so a job with a large output gives up budget for it (2024's outputs, measured on
# moruna-bench: Enrich's is the largest).
SCRATCH_GIB = {"enrich": 4, "score": 2, "audit": 2, "occupancy": 1}
# The gate image's layer stacks (setup on moruna-bench), FROM scratch: MORUNA_PROCESSES=8 and =7.
STACKS = {"moruna-vm": "/data/gate/stack8s", "moruna-vm-free1": "/data/gate/stack7s"}
# The loop's best settings from 0.6.5's grid (make_plan.py rerun): 16 threads over 250,000-row
# batches, 8 threads for Occupancy.
LOOP = {"enrich": {"threads": 16, "batch": 250000}, "audit": {"threads": 16, "batch": 250000},
        "occupancy": {"threads": 8, "batch": 250000}, "score": {"threads": 16, "batch": 250000}}
REPEATS = 5


def group_of(engine: str) -> int:
    return next(g for g, es in GROUPS.items() if engine in es)


def engines_for(workload: str, with_066_rust: bool = True, groups=tuple(GROUPS)):
    out = []
    for g in sorted(GROUPS):
        if g not in groups:
            continue
        for e in GROUPS[g]:
            if e == "moruna066-rust" and not with_066_rust:
                continue
            if e in NATIVE_EXPR and workload not in NATIVE_EXPR[e]:
                continue
            if e == "moruna-vm-free1" and workload not in FREE1:
                continue
            out.append(e)
    return out


def options(engine: str, workload: str, machine: str, cpus: int):
    """The engine's settings for a run: the loop's best, the baseline's pool, 0.6.6's profiles."""
    if engine == "loop":
        return dict(LOOP[workload])
    if engine == "rust-baseline":
        return {"threads": cpus, "batch": 0}
    if engine in ("moruna", "moruna066-rust"):
        return {"profiles": f"/data/profiles/gate-{machine}-{engine}-{workload}", "staging": "/data/staging"}
    if engine in ("spark", "ray", "dask"):
        return {"timeout_s": 900}
    if engine in STACKS:
        scratch = SCRATCH_GIB.get(workload, 1)
        return {"image_stack": STACKS[engine], "vm_memory_gib": VM["vm_memory_gib"],
                "budget_gib": VM["budget_gib"] + 1 - scratch, "scratch_gib": scratch, "timeout_s": 900}
    return {}


def nyc(files, machine: str = "d8", with_066_rust: bool = True, groups=tuple(GROUPS), noise: bool = True):
    m = MACHINES[machine]
    plan = []

    def add(rid, engine, workload, **kw):
        mem = VM["mem"] if engine in STACKS else m["mem"]
        plan.append({"id": rid, "engine": engine, "workload": workload, "files": files,
                     "cpus": m["cpus"], "mem_gib": mem, "group": group_of(engine),
                     "machine": machine, **options(engine, workload, machine, m["cpus"]), **kw})

    for wl in NYC:
        engines = engines_for(wl, with_066_rust, groups)
        if not engines:
            continue
        for e in engines:
            # The learning run: 0.6.6 starts from an empty profile store.
            fresh = {"fresh_profiles": True} if e in ("moruna", "moruna066-rust") else {}
            add(f"g-{machine}-{wl}-{e}-r0", e, wl, repeat=0, **fresh)
        for r in range(1, REPEATS + 1):
            for e in (engines if r % 2 else list(reversed(engines))):
                add(f"g-{machine}-{wl}-{e}-r{r}", e, wl, repeat=r)
        # The session's noise: 0.6.6 against itself, two pairs.
        for p in ((1, 2) if noise else ()):
            for side in ("a", "b"):
                add(f"g-{machine}-{wl}-noise-p{p}{side}", "moruna", wl, repeat=None, noise=f"p{p}")
    return plan


# The large jobs: once per build, the order changing from job to job (arrow-core-0.7.md section 9).
OOC = {"ov_shape": "overture", "ov_attrs": "overture", "fw_minhash": "fineweb"}
OOC_LOOP = {"ov_shape": {"threads": 16, "batch": 250000}, "ov_attrs": {"threads": 16, "batch": 250000},
            "fw_minhash": {"threads": 16, "batch": 20000}}
OOC_ENGINES = ("moruna-vm", "moruna", "moruna066-rust", "rust-baseline", "loop", "ray")


def ooc(files: dict, machine: str = "d8", with_066_rust: bool = True):
    """`files` maps a dataset (overture, fineweb) to its files."""
    m = MACHINES[machine]
    engines = [e for e in OOC_ENGINES if with_066_rust or e != "moruna066-rust"]
    plan = []
    for k, (wl, dataset) in enumerate(OOC.items()):
        order = engines if k % 2 == 0 else list(reversed(engines))
        for e in order:
            extra = dict(OOC_LOOP[wl]) if e == "loop" else {}
            if e == "ray":
                extra = {"batch": OOC_LOOP[wl]["batch"]}
            if e in ("moruna", "moruna066-rust"):
                extra = {"profiles": f"/data/profiles/gate-{machine}-{e}-{wl}", "fresh_profiles": True,
                         "staging": "/data/staging"}
            if e == "rust-baseline":
                extra = {"threads": m["cpus"], "batch": OOC_LOOP[wl]["batch"]}
            plan.append({"id": f"g-{machine}-{wl}-{e}-r1", "engine": e, "workload": wl, "files": files[dataset],
                         "cpus": m["cpus"], "mem_gib": m["mem"], "group": group_of(e), "machine": machine,
                         "repeat": 1, "timeout_s": 5400, **extra})
    return plan
