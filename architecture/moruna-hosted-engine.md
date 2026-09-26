# Moruna as a Hosted Engine: Architecture Design

**Document type:** full architecture design (not an ADR); a companion to `moruna-runtime-design.md` revision 3, which it amends where stated and does not repeat.
**Status:** DRAFT for review · 2026-09-26 · nothing in it has been built.
**Scope:** what Moruna must become so that it is the *programming model* a platform can bias every author toward (a kernel is Arrow in, Arrow out, declared and checkable without a platform) and so that it can be *packaged as a microVM* and run as the only thing inside it: a way to hand it a job from outside the process, a way to hear back, a budget that follows the machine while the run is in progress, a governed query engine as a source and a sink, and the microVM monitor itself, `moruna-vmm`, which boots that guest on any Linux machine with KVM. Griot Cloud is the first host; the design names no Griot type and every requirement below is stated as a property of Moruna. The Griot-side design, `griot-cloud/docs/design/griot-and-moruna.md`, consumes this document by section id.
**Explicitly out of scope:** multi-node execution (still §11 of the parent, still reserved; see §9 Q10 here); packing, placement and scheduling of VMs (the host platform's); disk custody, keys and egress policy (the host platform's, the monitor attaches devices it is given and exposes one socket, nothing more); weight-major execution (parent D11).
**Companions:** `architecture/sdd/00-preamble.md` (E11 forbids multi-node work in v1 and is respected here), `03-discovery.md`, `02-arena.md`, `10-scheduler.md`, `11-controller.md`, `04-trace.md`, `07-sources.md`, `09-placement.md`, `12-python.md`; `DECISIONS.md` Q9 and Q10.

The parent said: *"Griot Cloud's compute plane is a consumer of this runtime, not part of this document."* This document is the runtime's half of that consumer relationship.

---

## 1. Problem statement

**P1. There is no way to give Moruna a job from outside the process.** Configuration is function arguments plus five environment variables; the parent says "there is no configuration file" (`moruna-runtime-design.md:483`). There is no bin target, no `__main__`, no job document and no RPC. A host that wants to run a Moruna job must run Python and call `moruna.run(...)`. Inside a microVM whose only channel to the world is a socket, that means the host writes a Python program per job. The job, not a program, is the unit a platform dispatches.

**P2. The budget is discovered once and never again.** `discover()` is "one-shot" (`crates/moruna-discovery/src/lib.rs:97-133`). The arena is sized before anything runs and every page is touched (`crates/moruna-runtime/src/run.rs:180-237`); the controller's memory ceiling is a constant for the run (`crates/moruna-controller/src/classify.rs:48`); the sampler reads `memory.stat`, `memory.peak` and `cpu.stat` but never `memory.max` or `cpu.max` (`sampler.rs:141-208`), and `Sample` has no limit fields (`crates/moruna-kernel/src/limits.rs:121-134`). A host that gives the machine more memory while a run is in progress is giving it to nobody; a host that takes memory away moves the kill line under a controller that does not know it moved. The parent's own doctrine, no user-chosen sizes, the runtime follows the budget, stops at the process boundary the moment the budget changes.

**P3. The worker pool is fixed at the CPU ceiling seen at start.** `workers_from(cpu_quota)` creates N threads once (`run.rs:740-746`; preamble 6.6, "fixed at start"). The controller varies only how many are *active* (parent §5.8, `:329`). A CPU quota raised mid-run is unusable above the starting N; a quota lowered is discovered indirectly, one worker at a time, through throttling.

**P4. The run reports at the end, to the caller's memory.** `RunReport` is complete and honest (`crates/moruna-trace/src/report.rs:151-201`) but it is a return value. A host that started the run in another machine cannot see the committed watermark, the current bottleneck, or the fact that the run is still alive, until the process exits and something reads a file.

**P5. Resume after SIGKILL is designed but unproven.** `rt_t6` resumes after a *kernel-error* termination (`crates/moruna-runtime/tests/end_to_end.rs:162`). The SIGKILL cases (SC-T16, PL-T17) are F4.7, still open. Cross-node resume needs `durable_staging=present` (Q9), which nothing yet declares. A microVM that is destroyed by its host is exactly SIGKILL, and the disk it was writing to is exactly the durable staging Q9 imagines.

**P6. A governed query engine cannot be a source or a sink.** Sources are `ParquetSource`, `TensorSource` and `IteratorSource` (`python/moruna/_core.pyi:12-27`). A host whose reads must pass a policy, row filters, masks, noise, a contract check, has no way to put that engine in front of Moruna's queue except to run the engine to completion and hand Moruna the file, which defeats the out-of-core point. `moruna-datafusion` exists as a bridge crate (preamble 1.3) and is the natural seam; it does not yet turn a DataFusion plan into a `Source`.

**P7. Object-store credentials are per run and in the spec, but the guest may have no network at all.** `AmazonS3Builder::from_env()` plus a `storage` dict (`crates/moruna-py/src/run.rs:500-570`; `crates/moruna-reactor/src/object.rs:347-366`) assumes the process can open a TCP connection to the endpoint. A guest with no NIC cannot. The host proxies; Moruna must be able to reach an object store through a socket the host names.

**P8. There is no way to run Moruna in isolation from the machine it is on.** Moruna is a library and, after P1, a binary; both run as a process with the rights of whoever started it. A host that runs other people's kernels, a platform running tenant code, or a laptop running a model someone else wrote, has only the process boundary: a cgroup, a seccomp filter, a user id. The parent's own doctrine is that the budget is the machine's ceiling; the strongest form of that is a machine whose ceiling *is* the budget, with no network to leak through. Packaging Moruna as a microVM is the one packaging that gives every host that property without each host building it.

**P9. A kernel cannot be checked without running it on real data.** `@moruna.kernel` takes hints (`expected_amplification`, `preferred_rows`, `stateful`, `state_bytes`, `releases_gil`, `accepts`, `tier`; `python/moruna/__init__.py:142-185`) but no declaration of what columns the kernel needs or produces, and there is no command that answers "is this a valid kernel, and what does it cost" before a run. An author learns their function is wrong from a failed run; a platform that wants to register functions has to invent its own check. The paradigm, think in kernels, needs a tool that says whether you have one.

There is one inherited half-solution to retire: the environment variables `MORUNA_BUDGET`, `MORUNA_CPU`, `MORUNA_SPILL_DIR`, `MORUNA_SPILL_LIMIT`, `MORUNA_HOST_PROFILE` as the *only* out-of-band configuration. They stay as a developer convenience for the library path, and the job document of §4.1 supersedes them for the hosted path. The precedence rule in §4.1 says which wins.

---

## 2. Context

### 2.1 The host's shape, as it constrains this design

Stated as properties any host must provide, not as Griot's implementation.

- **The guest is a Linux microVM whose only I/O is virtio, and Moruna ships the monitor that boots it** (§4.8). Block devices (the dataset disk, a scratch disk), a `virtio-vsock` socket to the host, a serial console, and memory and vCPUs that the host can add and remove while the guest runs (`virtio-mem`, vCPU hot-add). **There is no network interface.** Consequence: every byte Moruna reads or writes is on an attached disk, or crosses vsock through the host; and the properties of the guest are Moruna's to guarantee, not each host's.
- **The rust-vmm crates exist and are what the major Rust monitors are built from.** `kvm-ioctls`, `kvm-bindings`, `vm-memory`, `linux-loader`, `vm-virtio`/`virtio-devices`, `vm-superio`, `vmm-sys-util`, `vhost`. Cloud Hypervisor and Firecracker are glue over them. Consequence: a monitor with a five-device model is a bounded amount of glue, and the pieces that are genuinely hard (snapshot/restore, PCI, GPU passthrough) are not needed for one run.
- **The host reads the guest's resource use through the VMM, not a cgroup.** Inside the guest there may be no cgroup at all; the machine *is* the budget. Consequence: discovery's OS path (`0.9 × RAM`, logical CPUs) is the one that runs in the guest, and it must notice when RAM or CPUs change.
- **The host may destroy the guest at any time** (preemption, packing, a deadline). Consequence: P5 is a first-class path, not a corner case.
- **The host has measured profiles of earlier runs** (the run report's `peak_anon_bytes`, `amplification_p95`, `worker_busy_fraction`) and sizes the guest from them. Consequence: the report is Moruna's contribution to the host's sizing, and the report must carry the *ceiling timeline*, not just the peak, or the host cannot tell "used 90% of 8 GB" from "used 90% of 8 GB for one minute and 3 GB the rest of the time".
- **One host process per guest talks to Moruna.** Consequence: the protocol in §4.3 has exactly one peer and need not multiplex.

### 2.2 Verified current state

Every row was read in `/Users/brackly/Desktop/Projects/amoru` at `7ce3c4d` (main, 2026-09-26). The executing agent re-verifies before acting.

| Fact | Where |
|---|---|
| Discovery runs once; precedence is explicit, `MORUNA_BUDGET`, cgroup, OS | `crates/moruna-discovery/src/lib.rs:97-133`; `limits.rs:52-109` |
| Ceiling is `memory.high`, else 0.9 × `memory.max`, else 0.9 × RAM, clamped to 0.95 × `memory.max` | `limits.rs:52-100` |
| Arena is sized once and pre-touched: ceiling − baseline − reserve − expected kernel state | `crates/moruna-runtime/src/run.rs:180-237`; parent `:559` |
| Worker threads: N = CPU ceiling at start, capped at 1024; controller changes `active` only | `run.rs:173, 740-746`; parent `:329` |
| Controller tick 250 ms; samples `memory.stat`, `memory.peak`, `cpu.stat`; no limit fields | `crates/moruna-controller/src/tick.rs:28-140`, `sampler.rs:141-208`; `moruna-kernel/src/limits.rs:121-134` |
| `RunSpec` is a Rust struct with source, kernels, sink, budget, cpu, staging, error policy, ordering, sizer, profiles, object-store config, checkpoint, resume | `crates/moruna-runtime/src/spec.rs:106` |
| `Source` trait: `schema`, `plan() -> Vec<Split>`, `read(split, rows, alloc, tier)`, `repeatable()` | `crates/moruna-kernel/src/source.rs:38` |
| Manifest at `staging_dir/moruna-<run_id>/manifest.json`, written every 5 s, at segment roll, at termination | `crates/moruna-placement/src/lib.rs:265-267`; parent §10 |
| `rt_t6` resumes after kernel-error termination; SIGKILL resume (SC-T16, PL-T17) is F4.7, open | `crates/moruna-runtime/tests/end_to_end.rs:162`; `BOARD.md` |
| Sources: Parquet (local, s3, gs, azure), tensor, iterator; sinks: Parquet, tensor, Arrow IPC | `python/moruna/_core.pyi:12-50` |
| S3 needs a `storage` dict or an `s3://` URL fails "no s3 configuration" | `crates/moruna-py/src/run.rs:143-146`; `object.rs:307-312` |
| `Tier::Remote` returns `Unsupported("rdma")`; `LOCAL_NODE` is the only `NodeId` | `crates/moruna-placement/src/moves.rs:542`; `ids.rs:22-25` |
| `RunReport` fields and per-stage figures; `to_json()` exists | `crates/moruna-trace/src/report.rs:110-201` |
| No bin target, CLI or job document; configuration is arguments plus five env vars | `python/moruna/__init__.py:82-139`; parent `:483` |
| Multi-node is reserved and forbidden in v1 | parent §11 (`:634-646`); preamble E11 (`00-preamble.md:534`) |

---

## 3. Sufficiency criteria

- **H1. A job is a document.** A complete run is described by one file that names the source, the kernels, the sink and the budget, and `moruna run <file>` executes it with no Python program written by the host. *Test:* every argument of `moruna.run(...)` has a field; a spec round-trips through JSON; a run from a file and the same run from Python produce identical reports (modulo timing).
- **H2. The host hears the run.** From the moment `moruna run` starts until it exits, a peer on the named socket receives a heartbeat at least every 2 s carrying progress, and receives the full `RunReport` before exit. *Test:* kill the peer's read; the run continues and the report is also on disk.
- **H3. The budget follows the machine.** With RAM raised by the host mid-run, the controller's ceiling rises within one tick, the arena grows, and `peak_anon_bytes` on a memory-bound run exceeds the starting ceiling. With RAM lowered, the ceiling falls within one tick and the run does not exceed the new ceiling. *Test:* a run under a host that resizes at a scripted moment; the report's ceiling timeline shows the change.
- **H4. CPUs follow the machine.** With CPUs added mid-run, `worker_busy_fraction` on a CPU-bound run rises and the active-worker count exceeds the starting N. *Test:* as H3 for CPUs.
- **H5. A destroyed guest resumes.** A run killed by SIGKILL at an arbitrary point, whose staging directory is on a disk that survives, resumes from its manifest and produces a sink identical (row multiset; byte-identical when `ordered`) to an uninterrupted run. *Test:* SC-T16 and PL-T17 with `durable_staging=present`.
- **H6. A governed plan is a source; a governed write is a sink.** A DataFusion `LogicalPlan` from a contract-native engine is a `Source`, and Moruna streams its partitions through the queue without materializing the result; the engine's contract write path is a `Sink`. *Test:* a gated plan over a file larger than the budget completes inside the budget; a write larger than the budget lands with a valid manifest.
- **H7. No network needed.** A spec whose object URLs name a proxy socket completes with the guest's network disabled. *Test:* run under `unshare -n`.
- **H8. Nothing superseded remains.** The environment variables are documented as the library-path convenience only; every hosted path goes through the spec. *Test:* `moruna run` refuses a spec that relies on an environment variable for a field the spec defines, and says which.
- **H9. E11 is intact.** No `NodeId` other than `LOCAL_NODE` is constructed; `Tier::Remote` still returns `Unsupported`. *Test:* the existing lint (S16, D12).
- **H10. The guest has no network and the monitor cannot give it one.** `moruna-vmm`'s device model has no network device type; a guest boots with `lo` only. *Test:* `ip link` in the guest; the monitor's configuration type has no field for a NIC.
- **H11. A run in a VM costs one boot.** `moruna run --vm spec.json` boots the guest, runs the spec with a disk attached, and returns the report; time from invocation to `hello` is under 300 ms on x86 and under 1 s on a Raspberry Pi 5, measured and written into this document. *Test:* the same spec run as a process and as a VM produce the same report modulo timing.
- **H13. A kernel is checkable without a platform or data.** A kernel declares its input and output schema; `moruna check` loads it, runs it on a synthetic batch derived from the declared input schema, compares the output schema to the declaration, and reports its first profile (amplification, state footprint, wall time per row), with no source, no sink and no platform. A Polars-signature function is accepted and checked as a kernel through the Polars bridge. *Test:* a kernel whose output disagrees with its declaration is refused naming the column; the profile row `moruna check` writes is read by a subsequent `moruna run`'s sizer.
- **H12. Resize is real hardware.** Memory and vCPUs added by `moruna-vmm resize` appear in the guest's `/proc/meminfo` and `cpu/online` within one second, and H3/H4 then hold. *Test:* H3/H4 under `moruna-vmm`, not only under a cgroup.

---

## 4. Architecture

### 4.1 The job document: `RunSpec` as a file

`RunSpec` (`spec.rs:106`) already is the document; it is a Rust struct. The change is a serialization and a loader.

```jsonc
{
  "moruna_spec": 1,
  "run_id": "01J…",                      // host-assigned; default: generated
  "source": {
    "kind": "parquet",                     // parquet | tensor | datafusion | iterator (library only)
    "url": "file:///data/raw/events/",     // or s3://…, gs://…; see object_store
    "options": {}
  },
  "kernels": [
    { "kind": "python", "module": "/job/kernels.py", "callable": "shout",
      "fingerprint": "sha256:…",           // from `moruna check`; refused if the loaded kernel's differs
      "expected_amplification": 1.5, "releases_gil": true },
    { "kind": "rust",   "crate": "…", "symbol": "…" }                // reserved
  ],
  "sink":   { "kind": "parquet", "url": "file:///data/refined/events/" },
  "budget": {
    "memory_bytes": null,                  // null: discover
    "cpu": null,
    "elastic": { "memory_max_bytes": 34359738368, "cpu_max": 32 }    // §4.4
  },
  "staging": { "dir": "/staging", "limit_bytes": null, "durable": true },
  "object_store": { "s3": { "endpoint": "unix:///run/moruna/egress.sock", "…": "…" } },
  "checkpoint": { "enabled": true, "interval_ms": 5000, "keep": false },
  "resume": null,                          // or a manifest path
  "error_policy": "terminate",
  "ordered": false,
  "report": { "socket": "vsock://2:5000", "file": "/staging/moruna-<run_id>/report.json" }
}
```

Rules:
- Every field of the Rust `RunSpec` has a JSON field; the Python surface's `run(...)` becomes a constructor of the same struct, so there is one code path (H1).
- **Precedence:** a spec field set → the spec wins; a spec field `null` → discovery, then the environment variable if the field has one, then the default. `moruna run` prints every field it resolved from the environment as a `notes` entry, and **refuses** when `--strict` (the hosted default) and any such resolution happened (H8).
- The spec is content-addressed: `sha256(canonical JSON)` is the `plan digest` the manifest already records (parent §10), so a resume with a changed spec is refused as today.
- `report.file` is always written, even when the socket is unset; the socket is additive.

### 4.2 `moruna run` and `moruna serve`

Two entry points, one binary (`moruna`), built from `moruna-runtime` with the Python adapter compiled in.

- **`moruna run <spec.json> [--strict]`**, load, resolve, execute, report, exit. Exit code: 0 completed; 2 spec refused (named field); 3 budget refused (parent P5 diagnostic); 4 kernel error under `terminate`; 5 resume refused; 130 cancelled.
- **`moruna serve --listen vsock://-1:5000`**, the guest-agent mode. Wait for one `spec` message on the socket, run it exactly as `moruna run` would, stream heartbeats, send the report, exit. One job per process, by design: the VM exists for one run.

The Python package keeps `moruna.run(...)`; it builds the same `RunSpec` and calls the same facade. Nothing in the library path changes for a notebook user.

### 4.3 The host protocol

Newline-delimited JSON over the socket named in `report.socket` (vsock in a guest; a Unix socket in tests). One peer. Messages from Moruna:

| message | when | fields |
|---|---|---|
| `hello` | on connect | `moruna_version`, `spec_digest`, `limits` (as discovered) |
| `heartbeat` | every ≤ 2 s | `committed_seq`, `rows_out`, `bytes_out`, `active_workers`, `ceiling_bytes`, `anon_bytes`, `bottleneck`, `staging_bytes` |
| `limits_changed` | when discovery observes a change (§4.4) | old and new `Limits`, `reason` (`"memory"`, `"cpu"`) |
| `report` | before exit | the full `RunReport` JSON |
| `exit` | last message | `code`, `diagnostic?` |

Messages to Moruna:

| message | effect |
|---|---|
| `spec` (serve mode only) | the `RunSpec`; a second `spec` is refused |
| `cancel` | the existing `CancelToken`; manifest written; exit 130 |
| `checkpoint` | write the manifest now (the host is about to destroy the guest) |

The protocol is deliberately dumb: Moruna never receives a resize *command*. The host resizes the machine; Moruna notices (§4.4). This keeps Moruna free of any host API and keeps the guest's trust boundary at the hardware: a hostile peer on the socket can cancel a run, not enlarge one.

### 4.4 The budget follows the machine

This section amends parent §5.8 and the SDDs for discovery (03), arena (02), scheduler (10), controller (11) and trace (04).

**Discovery becomes a watcher.** `discover()` stays as the initial read. A new `LimitsWatch` re-reads the same sources every controller tick (250 ms; cheap: two files) and publishes a `Limits` into an `Arc<ArcSwap<Limits>>` that the controller and the facade hold. In a guest the sources are `/proc/meminfo` `MemTotal` and `/sys/devices/system/cpu/online`; under a cgroup they are `memory.max`/`memory.high` and `cpu.max`; the precedence of `limits.rs` is unchanged, only re-evaluated. A change is a `TraceRecord` and a `limits_changed` message. The ceiling that the classifier reads (`classify.rs:48`) is loaded from the swap each tick instead of copied from `cfg` once.

**The arena grows and shrinks by region.** The arena is already a set of regions (F4.9's second defect is about region routing). Two operations are added to the arena, callable only by the facade's watcher task:

- `grow(bytes)`: map and touch a new region of `bytes`, rounded to the huge-page size; the allocator's free list gains it. Bounded above by `budget.elastic.memory_max_bytes`; a spec with no `elastic` never grows.
- `shrink(bytes)`: mark the newest regions totalling `bytes` *draining*: no new allocations there; when a draining region's last buffer is freed, unmap it. Shrink is therefore **eventual**, and the controller is told the *target* ceiling immediately so it stops filling: morsel sizes and queue depths come down under the existing memory-pressure rule, spill engages, and the regions drain. The report records how long the drain took.

The sizing arithmetic (ceiling − baseline − reserve − expected kernel state) is re-run on each `Limits` change to produce the new arena target; the difference is a `grow` or a `shrink`.

**The worker pool has a ceiling and a target.** Threads are created up to `budget.elastic.cpu_max` (default: the starting N, so an inelastic spec behaves exactly as today) and parked; creating a parked thread costs a stack, nothing else. The controller's `active` knob is bounded by the *current* CPU limit from the swap, not by N. Raising CPUs raises the bound; the controller's existing "raise active when workers are the bottleneck" rule does the rest. Lowering CPUs lowers the bound and the controller parks workers down to it within one tick instead of discovering throttling one worker at a time.

**The report carries the timeline.** `RunReport.limits` becomes `limits_initial` plus `limits_timeline: [(t_ms, Limits)]`, and `peak_fraction_of_ceiling` is computed against the ceiling in force at the time of the peak. Per-stage figures are unchanged. This is the host's sizing input (§2.1).

**What does not change:** the controller's decision rules, the morsel model, the placement engine, the sinks. Elasticity is a moving ceiling under the same controller, which is why it is small.

### 4.5 A DataFusion plan is a source

`moruna-datafusion` gains `PlanSource`: it takes an `Arc<dyn ExecutionPlan>` and a `SessionContext`; `schema()` is the plan's schema; `plan()` returns one `Split` per output partition (partition index, no row count until read, so the manifest digest covers partition ids); `read(split)` executes that partition's stream on the reactor, allocating each `RecordBatch` into the arena via the C data interface, morsel by morsel, never holding the partition in memory. `repeatable()` is `true` only when the plan's provider declares deterministic partition order (file-backed scans do; a plan with a hash exchange does not), so an unrepeatable plan stages Q0 and refuses resume, exactly as `IteratorSource` does today.

A governed engine needs nothing from Moruna beyond this: it hands over the plan and Moruna sees only the batches the plan emits. The spec's `source.kind = "datafusion"` names the engine by crate feature; the first engine is peQL 0.4.0 (`github.com/griot-cloud/peQL`), whose `Engine::view(name, caller)` returns a `LogicalPlan` already wrapped in its `Gate` node with the caller's context bound (`src/engine.rs:621-670`), so `PlanSource` executes a plan that peQL's own `ensure_gated` invariant has approved. peQL's binding is a streaming `ListingTable` over the attached disk (`src/binding.rs:117-150`); its per-partition streams are what `read` consumes.

**The write side is the same seam in reverse.** `PeqlSink` wraps `Engine::write(name, batches, mode)` (`src/engine.rs:389-495`): Moruna's sink queue delivers morsels, peQL conforms them to the contract's row schema, computes the flag and derived columns its `WritePlan` demands, writes partitioned, clustered, bloom-filtered Parquet, and refreshes the manifest on `finish`. Moruna makes that path out-of-core; peQL's semantics are untouched. Both `PlanSource` and `PeqlSink` live in `moruna-datafusion` behind a `peql` feature, so the bridge crate depends on `peql` and `peql` never depends on Moruna.

### 4.6 Object storage through a socket

The `ObjectStoreConfig` endpoint accepts `unix://` and `vsock://` in addition to `http(s)://`. The reactor's `AmazonS3Builder` gets a custom HTTP connector that dials that socket and speaks plain HTTP/1.1 over it; the host's proxy terminates it and applies whatever egress policy it has. No change to any source or sink: they see an `object_store::ObjectStore` as before (H7). A `file://` URL on an attached disk needs nothing at all and is the default for the hosted path.

### 4.7 Resume on a surviving disk

Nothing new is designed; F4.7 is finished and Q9 is decided by the spec: `staging.durable = true` sets `durable_staging=present`, the manifest cadence is `checkpoint.interval_ms`, and the `checkpoint` message (§4.3) lets a cooperative host flush before it destroys the guest. `moruna run --resume auto` looks for `staging.dir/moruna-*/manifest.json` and resumes the newest whose spec digest matches. The one known gap, a read in flight at checkpoint time is lost because the cursor is "next to issue" (`10-scheduler.md:287`), is closed by recording *issued* splits in the manifest and re-issuing them on resume; re-reading an issued split is safe because `read` is deterministic for repeatable sources (CT-I12).

### 4.8 `moruna-vmm`: Moruna packaged as a microVM

A second binary in the workspace, `moruna-vmm`, and a guest image. The monitor is built on the rust-vmm crates, not on Cloud Hypervisor, Firecracker or Kata: the device set below is smaller than any of theirs, the absence of a network device is the point, and the later shared-memory device (§4.8.6) is only possible if Moruna owns the monitor.

**4.8.1 What it is and is not.** A monitor that boots exactly one guest for exactly one run and exits with the run's exit code. It attaches block devices it is given as paths, exposes the guest's vsock, and resizes on request. It has no scheduler, no policy, no notion of a tenant, a key, a contract or a cluster. Those are the host platform's (Griot's design keeps them in its host agent). The parent's "not a host platform" stands: `moruna-vmm` is a packaging of the runtime, as the wheel is.

**4.8.2 Guest devices**, exactly these, all `virtio-mmio` (no PCI until GPU passthrough is needed):

| device | purpose |
|---|---|
| `virtio-blk` × N | the disks the host names: the dataset (read-only or read-write, per device), scratch/staging |
| `virtio-mem` | memory hot-plug and unplug; the guest kernel auto-onlines (`memhp_default_state=online_movable`) |
| vCPU hot-add | up to `cpu_max` given at boot |
| `virtio-vsock` | the only channel: the host protocol of §4.3, object-store proxying of §4.6, and any stream the host chooses to terminate |
| serial | console → the monitor's stdout |

There is no `virtio-net` and no configuration field for one (H10). The device model is a trait per device from day one, so a device can be added later without touching the others.

**4.8.3 Boot.** A pinned Linux kernel loaded with `linux-loader`; an initramfs with Moruna as init running `moruna serve --listen vsock://-1:5000` (§4.2), free-threaded CPython 3.14 with `pyarrow` and the `moruna` wheel, a read-only rootfs. The image is released with Moruna (§4.8.7) so a host pins it by digest. Target: under 300 ms to `hello` on x86 (H11). Snapshot/restore is not in scope; a host that wants interactive latency keeps a booted guest waiting for its `spec`.

**4.8.4 Interface.**

```
moruna-vmm boot   --image <path> --disk <path>[:ro] ... --memory <bytes> --memory-max <bytes> --cpus <n> --cpus-max <n> --vsock <cid>
moruna-vmm resize --memory <bytes> | --cpus <n>          (over the monitor's own control socket)
moruna run --vm <spec.json> [--image ...] [--disk ...]   (boots, sends the spec, streams the report, exits)
```

`boot` blocks until the guest exits and returns its exit code. The host talks to the guest through the vsock CID it chose; the monitor never reads the traffic. `resize` hot-plugs within the maxima given at boot. A hostile peer on the control socket can resize or kill a run; it cannot give the guest a device it does not have.

**4.8.5 Resize inside the guest.** Hot-plugged memory blocks are onlined by the guest kernel (`memhp_default_state`); hot-added vCPUs are onlined by a udev rule in the initramfs. `LimitsWatch` (§4.4) sees the change in `/proc/meminfo` and `cpu/online`; nothing else is needed. This is H12.

**4.8.6 Later, not now.** A custom virtio device giving the guest a zero-copy window onto a host-managed arena (the shared-memory source of parent §9, Phase 9, becomes a device); GPU passthrough (VFIO, needs PCI); snapshot/restore. Recorded so the device-model trait precludes none of them.

**4.8.7 Release.** Moruna's release publishes, beside the wheels: `moruna` and `moruna-vmm` binaries for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` (the monitor is Linux-only; macOS and Windows have no KVM), `.sha256` and cosign signatures, and the guest image as an OCI artefact by digest with an SBOM. A host builds nothing; it downloads and verifies.

**4.8.8 Host requirements.** `/dev/kvm`. On a cloud VM that means nested virtualization on a machine family that supports it; on bare metal and a Raspberry Pi 5 it means the stock kernel. Whether a virtio-mmio guest under this monitor boots on the Pi 5's interrupt controller is **unverified** and is E8.7's last gate.

### 4.9 The kernel as the programming model: declarations and `moruna check`

**Declared schemas.** `@moruna.kernel` and the `Kernel` trait gain two optional declarations, `input_schema` and `output_schema`, each an Arrow schema or a subset spelled as `{column: type}`. A kernel that declares them is *checkable*; one that does not is still a kernel (the library path is unchanged) but `moruna check` says so and a hosted spec may require them. `output_schema` may be expressed relative to the input (`adds`, `drops`, `changes`) so a kernel that appends one column does not restate every input column.

**Polars as first-class syntax.** A function whose annotated signature is `pl.DataFrame -> pl.DataFrame` (or `pl.LazyFrame`) is accepted by the decorator and by `moruna check` and is registered as a kernel wrapped by `moruna-polars`: the batch is handed to Polars zero-copy through the Arrow C data interface and the result comes back the same way. The interface is Arrow; the syntax is Polars. pandas is not a signature; inside a body it is the author's cost and shows in the profile.

**`moruna check <module-or-file> [--kernel name]`**, the local half of a platform's registration, and the whole of it for an author with no platform:
1. **load**: import the module under the adapter; the decorator or the Polars signature parses; hints are read.
2. **synthetic batch**: from `input_schema`, generate batches, empty, one row, `preferred_rows` rows, all-null columns, type edge values (min/max integers, empty strings, NaN, epoch boundaries), deterministically from a seed.
3. **run** each batch through the kernel in-process with the arena and the trace on, exactly as a run would.
4. **compare** the produced schema with `output_schema`; refuse on a mismatch naming the column and the types.
5. **profile**: write the first profile-store row for the kernel's fingerprint (amplification p50/p95, state bytes, wall per row, GIL held or released) so a later `run` sizes from evidence.
6. **fingerprint**: `sha256(canonical source ∥ lockfile bytes ∥ Moruna ABI version)`, printed, and recorded in the profile row; the same value a hosted `RunSpec` pins in `kernels[].fingerprint`.

Output is a JSON report (`--json`) and a human summary; exit 0 on a checkable, agreeing kernel, 2 on a refused one. A Rust kernel is checked the same way through a small harness the `Kernel` trait exposes.

**Standard kernels, by arguments.** Not every transformation deserves user code. A crate `moruna-kernels` ships the common ones in Rust, constructed by arguments rather than written: `cast`, `rename`, `select`, `drop`, `filter(expr)`, `fill_null`, `dedupe(keys)`, `hash(cols, algo)`, `mask(cols, mode)`, `explode`, `concat_str`, `date_trunc`. Each declares its schemas as a function of its arguments, ships with a profile, releases the GIL by construction, and two adjacent standard kernels in a chain are fused into one stage when their combination is expressible as one (`select` after `cast`, `filter` after `fill_null`). A `RunSpec` names one as `{ "kind": "std", "name": "dedupe", "args": {...} }` and its fingerprint is `name ∥ canonical args ∥ crate version`. This is where a host optimises: a chain that is all standard kernels never enters Python.

**What this is not.** Not a test framework (the author's tests are theirs), not a type checker for Python, and not a guarantee about behaviour on real data, it is the guarantee that the function *is a kernel* and a first measurement of what it costs.

---

## 5. Component amendments, by SDD

| SDD | amendment |
|---|---|
| `01-contracts.md` | `Limits` gains `observed_at`; `Allocator` gains `grow`/`shrink` (facade-only, documented as such); `Sample` gains `ceiling_bytes`, `cpu_limit`; `TraceRecord::LimitsChanged`; `RunSpec` serde derive and the JSON schema of §4.1 as the byte-exact format |
| `02-arena.md` | regions as the unit of growth; the draining state; the huge-page rounding rule; the invariant that a draining region accepts no allocation |
| `03-discovery.md` | `LimitsWatch`; the guest sources (`/proc/meminfo`, `cpu/online`); the rule that a change smaller than one huge page or one CPU is ignored |
| `04-trace.md` | `limits_timeline`; `peak_fraction_of_ceiling` against the ceiling in force; the report file and socket writers |
| `05-adapters.md` | `input_schema`/`output_schema` on the decorator and the `Kernel` trait (absolute, or `adds`/`drops`/`changes` relative to the input); the Polars-signature acceptance path through `moruna-polars`; the kernel fingerprint |
| `07-sources.md` | `PlanSource` in `moruna-datafusion`; the `repeatable()` rule for plans; the socket endpoint for object URLs |
| `09-placement.md` | issued splits in the manifest; `checkpoint` on demand |
| `10-scheduler.md` | parked threads up to `cpu_max`; `active` bounded by the current limit |
| `11-controller.md` | ceiling read from the swap each tick; the shrink target rule; no new decision rules |
| `12-python.md` | `moruna.run(...)` builds a `RunSpec`; the bin entry points `run`, `serve`, `check`; the `moruna` console script; the decorator's schema declarations and the Polars signature |
| **new `15-check.md`** | `moruna check`: synthetic batch generation (byte-exact, seeded), the schema comparison rules (`adds`/`drops`/`changes`), the profile row it writes, the JSON report, exit codes (§4.9) |
| **new `13-host.md`** | the protocol of §4.3, byte-exact; `moruna serve`; exit codes; the strict-mode refusal texts |
| **new `14-vmm.md`** | `moruna-vmm`: the device model trait and the five devices, boot, the control socket, resize, the guest image recipe, the release artefacts (§4.8) |

---

## 6. Failure modes and degraded behaviour

| failure | behaviour | detection | response |
|---|---|---|---|
| Socket peer gone | Run continues; report written to file; `exit` not delivered | Host sees no heartbeat for > 6 s | Host reads `report.file` from the disk after the guest exits |
| Host lowers RAM faster than the arena can drain | Anon memory approaches the new ceiling; the controller is already shedding; the kill line is the machine's | `limits_changed` then rising `anon_bytes` in heartbeats | Host waits for the drain (heartbeat `ceiling_bytes` = target) before lowering again; a host that does not is the parent's P5 and gets the parent's diagnostic on resume |
| Host raises RAM but `elastic.memory_max_bytes` is lower | Ceiling clamps at the spec's maximum; a note is recorded | `limits_changed` shows the clamp | The host sized the spec wrong; the report says so |
| CPU hot-add not onlined by the guest kernel | `cpu/online` unchanged; nothing happens | Host sees no change in `active_workers` | Guest kernel must auto-online; a host responsibility named in §9 |
| `spec` refused in serve mode | `exit 2` with the field named | Immediate | Host fixes the spec; the guest is cheap to restart |
| Resume with a changed spec | Refused (digest mismatch), as today | Exit 5 | By design |
| An unrepeatable plan under `resume` | Refused at plan time, named | Exit 5 | Use a repeatable source or drop `resume` |
| Object-store proxy socket absent | `IoError` naming the socket at the first read | Immediate | Host wiring; FAIL LOUD |
| `/dev/kvm` absent or not permitted | `moruna-vmm boot` exits 2 naming the device and the reason | Immediate | The host's responsibility (§4.8.8); `moruna run` without `--vm` still works as a process |
| Guest kernel panics | The monitor exits with the serial log on stderr and code 6 | Immediate | The image is pinned and released; a panic is a release defect |

---

## 7. Explicit risks and accepted positions

1. **Shrink is eventual.** A run can exceed a lowered ceiling for as long as the drain takes. Accepted: the alternative, forcibly evicting live buffers, would corrupt in-flight morsels. The heartbeat makes the drain observable, and the host is told to pace.
2. **Parked threads cost stacks.** `cpu_max = 32` parks 31 threads' stacks (8 MB virtual each, resident only when touched). Accepted; measured in the H4 gate.
3. **`PlanSource` inherits DataFusion's memory behaviour inside the plan.** A sort or a hash aggregate inside the plan allocates outside the arena, which is the parent's §8 honest limit restated. Accepted for v1; the trace's `state_bytes` seam applies; the spec's `expected_amplification` on the source lets the sizer account for it.
4. **Two configuration paths (spec, environment) coexist for the library.** Bounded by H8's strict mode on the hosted path and by the precedence rule.
5. **Building a monitor is the largest cost in this document.** The device model, boot, hot-plug and the control socket are months of work that Cloud Hypervisor already has. Accepted because the device set is five, the no-NIC guarantee and the later shared-memory device need ownership, and the crates are the same ones the existing monitors use. Contained by E8.7's gate and by the rule that nothing in E8.0–E8.6 depends on it: Moruna as a process is complete without the monitor.
6. **The protocol is JSON lines, not a schema'd RPC.** Accepted for one peer and a dozen messages; it is trivially testable with a Unix socket.

---

## 8. Implementation plan

Each phase is one or more features on `BOARD.md` under a new epic **E8, Hosted engine**, sized per preamble 6.7. Gates cite §3.

- **E8.0. The job document.** `RunSpec` serde, the JSON format of §4.1, `moruna run`, the report file, strict mode. *Gate:* H1, H8. No VM needed; runs on a laptop.
- **E8.1. The host protocol.** `13-host.md`; `moruna serve`; heartbeats; `cancel`, `checkpoint`. *Gate:* H2, over a Unix socket.
- **E8.2. CPUs follow the machine.** `LimitsWatch` for CPUs; parked threads; the bounded `active`. *Gate:* H4 under a cgroup whose `cpu.max` is rewritten mid-run (no VM needed).
- **E8.3. Memory follows the machine.** `grow`/`shrink`; the ceiling from the swap; the timeline in the report. *Gate:* H3 under a cgroup whose `memory.max` is rewritten mid-run, then again under a real `virtio-mem` guest supplied by the host.
- **E8.4. Resume on a surviving disk.** F4.7 closed; issued splits in the manifest; `--resume auto`. *Gate:* H5.
- **E8.5. A plan is a source, a contract write is a sink.** `PlanSource` over peQL's `Engine::view` and `PeqlSink` over `Engine::write`, in `moruna-datafusion` behind a `peql` feature. *Gate:* H6 with a governed plan over a file larger than the budget, and a write larger than the budget whose manifest verdict is `valid`.
- **E8.6. No network.** Socket endpoints for object stores. *Gate:* H7.

- **E8.8. The kernel is checkable.** Schema declarations on the decorator and the trait; the Polars-signature path; `moruna check` with the synthetic batch, the comparison, the profile row and the fingerprint; `RunSpec.kernels[].fingerprint` enforced at load. *Gate:* H13. Starts now; depends on nothing else in E8.
- **E8.7. `moruna-vmm`.** The device-model trait; block, vsock, serial; boot with `linux-loader`; the guest image; then `virtio-mem` and vCPU hot-add; the control socket; `moruna run --vm`; the release artefacts. *Gate:* H10, H11, H12 on the Nairobi reference host; then boot on a Raspberry Pi 5 (§4.8.8) or a written finding that it cannot.

E8.0–E8.2 can start now and in parallel; E8.3 after E8.2 (it reuses the watcher); E8.4 and E8.5 are independent of the others. E8.7 starts after E8.1 (it needs `moruna serve`) and runs beside everything else. Nothing in E8.0–E8.6 waits for the monitor: every gate but the second half of E8.3 and H12 runs under a cgroup.

---

## 9. Open questions for Brackly

| # | question | recommendation |
|---|---|---|
| H-Q1 | Is shrink required to be honoured, or best-effort with pacing by the host? | Best-effort with the heartbeat as the pacing signal (§7.1). Forced eviction is not worth its bugs. |
| H-Q2 | Who owns the guest image (kernel config with `memhp_default_state=online_movable`, CPython build, rootfs)? | **Moruna** (decided 2026-09-26 with the monitor): the image is released with `moruna-vmm`; a host adds a layer on top by digest. |
| H-Q3 | Free-threaded CPython in the guest? | Yes, as the default guest interpreter: the guest is one job, the GIL serialisation the parent avoids is the whole point of parking threads. Standard CPython remains available for kernels that need it (`allow_gil`). |
| H-Q4 | Should `elastic` default to "may grow to the machine" when the spec omits it? | No. An inelastic spec behaves exactly as today; growth is opt-in by the host, which knows what it will hot-plug. |
| H-Q5 | Q9, decided by this document? | Yes: `staging.durable` in the spec, 5000 ms default cadence, plus the on-demand `checkpoint`. |
| H-Q6 | Q10, does this bring multi-node forward? | No. One resizable machine is v1's answer to "more resources". Multi-node stays after v1 and E11 stands. |
| H-Q8 | Should `output_schema` be **required** for a kernel used in a hosted `RunSpec`, or merely checked when present? | Required in a hosted spec (the platform's pipeline editor needs it to type-check edges); optional on the library path. |
| H-Q7 | Is `moruna-vmm` part of the `moruna` crate graph (a workspace member depending on `moruna-kernel` for the spec types) or a sibling with no dependency? | A workspace member that depends only on `moruna-kernel`'s `RunSpec` types, so `moruna run --vm` and the monitor agree on one schema. |

---

## Sources

- Repository evidence: every path in §2.2, read at `7ce3c4d`.
- The parent design's §5.8 (controller), §8 (honest limit), §10 (recovery), §11 (reserved multi-node), §12 (Q9, Q10).
- rust-vmm: `github.com/rust-vmm/community` and the crates named in §2.1.
- Griot's consumer design: `griot-cloud/docs/design/griot-and-moruna.md` (cites this document's §4.1, §4.3, §4.4, §4.5, §4.8).
