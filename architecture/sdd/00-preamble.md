# Amoru SDD: Preamble

**Document type:** software design document, shared preamble (read by every agent before its component SDD)
**Status:** DRAFT · 2026-09-15
**Parent:** `architecture/amoru-runtime-design.md` (revision 2), the architecture design; this preamble does not repeat its context or its alternatives, it decides what the architecture left open and fixes what every component shares.
**Language and repository:** Rust 2024 edition for the runtime, Python 3.13 and 3.14 for the surface, one Cargo workspace at the repository root.
**Reference hardware:** to be named (escalation E1). Until named, benchmark gates run on the developer's machine and are reported as provisional.

This preamble plus the contracts SDD (`01-contracts.md`) plus one component SDD is the complete brief for an agent building that component. Nothing else in the repository is required reading.

---

## 1. Purpose, boundary and component map

### 1.1 What the runtime is

Amoru runs a full pass over a dataset larger than the memory budget of the process it lives in, applying a transformation a query engine cannot express, and writes the result out, at close to the budget's capacity, without the user choosing a batch size, a worker count, a read-ahead depth or a spill threshold. The dataset is tabular (Parquet, delivered as Arrow record batches) or tensor (safetensors, NumPy, aligned binary, delivered as DLPack tensors). The budget is discovered from the host (a cgroup, the machine) or given explicitly. The unit of everything is a morsel: one Arrow batch or one tensor with a header, moved between memory tiers by DMA, never copied by the CPU after it has been decoded once.

### 1.2 What the runtime refuses to be

Not a distributed engine: one process, one host. Not a query planner: kernels are opaque and there is no optimiser. Not a streaming system: no per-record latency guarantee. Not a storage system: sources read from whatever store exists. Not a model server: weight-major execution is a later document (parent D11).

### 1.3 Components and the edges between them

Twelve components. The number is also the build order and the SDD file number. An edge "A → B" means A calls B through an interface in the contracts crate; the contract on the edge is the one sentence that the calling side may rely on.

| # | Component | SDD file | Consumes | Contract on each consumed edge |
|---|---|---|---|---|
| 1 | Contracts crate | `01-contracts.md` | nothing internal | (defines every interface below) |
| 2 | Memory arena | `02-arena.md` | 1 | implements `Allocator`; every buffer it returns is 64-byte aligned, page-aligned when larger than a page, and lives in the tier requested or the call fails |
| 3 | Resource discovery and host profile | `03-discovery.md` | 1 | produces `Limits` and `HostProfile`; values are read from the host at call time, never cached across calls |
| 4 | Trace writer and run report | `04-trace.md` | 1 | accepts `TraceRecord` from any thread without blocking the caller for longer than a bounded channel push; flushes on `finish` and on abort |
| 5 | Kernel adapters | `05-adapters.md` | 1 | each adapter is a `Kernel`; the Python adapter never copies payload bytes across the interpreter boundary |
| 6 | IO reactor | `06-reactor.md` | 2, 3 | implements `Reactor`; every operation completes exactly once, into the buffer it was given, on the reactor's threads, never on a worker |
| 7 | Sources | `07-sources.md` | 1, 2, 6 | implement `Source`; `plan` is complete before the first `read`; `read` returns a payload in the tier the allocator was asked for |
| 8 | Sinks | `08-sinks.md` | 1, 2, 6 | implement `Sink`; `write` takes ownership of the payload; `finish` is called exactly once after the last `write` completes |
| 9 | Placement engine | `09-placement.md` | 1, 2, 3, 6 | implements `Placement`; `pop` returns a morsel already resident in the tier the caller asked for, or blocks; `push` never blocks |
| 10 | Scheduler | `10-scheduler.md` | 1, 9 | implements `Knobs`; workers only ever run `Kernel::apply` and nothing that blocks on IO |
| 11 | Resource controller | `11-controller.md` | 3, 4, 9, 10 | the only writer of every knob; reads stats, never morsels |
| 12 | Python surface | `12-python.md` | all | the only component that knows what a user is |

The agent building component N is handed this preamble, `01-contracts.md`, and `0N-<name>.md`, and the fakes for every interface N consumes (section 6.4).

### 1.4 Per-component schema

Every component SDD has these thirteen sections, in this order, with the component's two-letter prefix (CT, AR, DS, TR, AD, RE, SO, SI, PL, SC, RC, PY) on every invariant and test id:

a. Purpose and boundary. b. Vocabulary specific to the component. c. Invariants, `XX-I1..`. d. Interfaces: exposed and consumed, complete signatures. e. Data model, formats and state machines. f. Algorithms and policies. g. Concurrency within the component. h. Behaviour: normal path, edge cases, failures. i. Configuration: this component's rows of the global table. j. Observability. k. Tests, `XX-T1..`. l. Implementation notes for the agent. m. Open items (must be empty, or moved to the escalation list, before hand-off).

The test for every sentence in a component SDD: could two competent implementers build two different things from it? If yes, it is not finished.

---

## 2. Global vocabulary

Terms used by more than one component are defined here once. A component SDD adds terms only it uses. Units are bytes unless stated.

**Morsel.** One unit of work: a payload plus a header (sequence number, stage, byte size, origin, features). The scheduler hands out morsels, the placement engine moves them, queues hold them, the trace records them.

**Payload.** The data inside a morsel: either an Arrow `RecordBatch` (a table) or a DLPack-backed tensor, each tagged with the tier where its bytes currently are.

**Tier.** Where bytes physically live: `Device(id)` (accelerator memory), `PinnedHost` (page-locked host RAM, valid DMA source and target), `Host` (ordinary host RAM), `Disk(segment)` (a staging segment on local storage; no resident bytes).

**Stage.** One position in the linear chain source → kernel₁ → … → kernelₙ → sink. Stage 0 is the source's output; stage k is kernel k's output; the sink consumes the last stage.

**Queue.** The ordered set of morsels between two adjacent stages, owned by the placement engine. Q0 is between the source and kernel 1.

**Budget.** The bytes the runtime may hold in a tier. The host budget is derived from the discovered or explicit limit minus baseline and reserve; the device budget likewise per device; the disk budget is the staging limit.

**Baseline.** Anonymous host memory of the process after all kernels' `init` have completed and before the first morsel is read.

**Reserve.** The fraction of the ceiling deliberately never allocated, so a misestimate lands in headroom rather than in the OOM killer.

**Amplification (`A_k`).** For kernel k, the ratio of peak working-set bytes during `apply` to input payload bytes, measured by the probe and refined per morsel.

**Probe.** The first morsel of each kernel stage, run alone at a fixed small size to measure `A_k` before the controller sizes real morsels.

**Morsel target.** The controller's current intended payload size for a stage, in bytes; sources and the placement engine split or coalesce to approach it.

**Split.** A unit the source can read independently (a Parquet row group, a tensor or a slice of its leading dimension), with metadata known before reading.

**Look-ahead.** Using a split's metadata (row count, uncompressed bytes per column, shape) to size morsels before the bytes are read.

**Promotion / demotion.** Moving a morsel's bytes up toward the tier its consumer wants / down toward disk, by DMA. Promotion is toward the head of a queue; demotion is from the tail.

**Head stays hot.** The invariant that the next morsel a consumer will pop is never the one being demoted.

**Staging segment.** A file on local disk holding one or more demoted morsels in their in-memory layout (Arrow IPC or Amoru aligned binary), page-aligned, written with direct IO.

**Direct IO.** Reads and writes that bypass the page cache (`O_DIRECT`), so bytes move between disk and a runtime buffer without an intermediate kernel copy; requires aligned buffers, offsets and lengths.

**Host profile.** The set of guarantees a platform declares about the host (huge pages present, memlock permitted, io_uring allowed, GPUDirect Storage present, NVMe staging path), which lets the runtime skip probing and treat a failure on a guaranteed path as a platform bug.

**Knob.** One of the controller's outputs: morsel target per stage, active worker count, read-ahead depth, staging trigger per queue, high-water mark per queue and tier.

**Trace record.** One row per morsel completion, in the schema fixed in the contracts SDD; the run report is computed from the trace alone.

**Fingerprint.** A stable identity for a kernel (code identity plus configuration hash) used to key profiles across runs.

**Fake.** A test implementation of a contracts-crate interface with deterministic, configurable behaviour, used by every component's tests in place of the real implementation of its dependencies.

---

## 3. Global invariants

These hold across components. Each component SDD cites the ones it upholds and adds its own. Each is checkable; the test that checks it is named in the component SDD that owns the mechanism.

**G-I1. The budget is never exceeded.** At every sample, anonymous host memory of the process is at or below the host ceiling, and device memory allocated by the runtime is at or below the device budget. Owner: arena (allocation), controller (sizing), placement (demotion). Parent S1.

**G-I2. No CPU in the data path after decode.** Once a payload exists in a tier, every subsequent movement of its bytes is a DMA operation issued by the reactor or a pointer hand-off; no component copies payload bytes with the CPU except a source decoding from a non-layout-preserving format (Parquet) and a sink encoding to one. Owner: reactor, placement, adapters. Parent D10, S13, S14.

**G-I3. The head of every queue is resident.** When a consumer pops from a queue, the morsel it receives is already in the tier the consumer declared; the consumer never waits on a disk read that could have been started earlier, and a wait that does happen is traced as a placement miss. Owner: placement. Parent D10.

**G-I4. Every morsel leaves exactly one trace record.** Every morsel that enters stage k produces one `TraceRecord` for stage k on completion, error or skip; none is dropped; the run report is a pure function of the trace and the limits. Owner: scheduler (emission), trace (durability). Parent S9.

**G-I5. Knobs have one writer.** Only the controller writes knobs; every other component reads them. Owner: controller, scheduler. Parent D2.

**G-I6. Two payload layouts and no more.** A payload is a `RecordBatch` or a DLPack tensor. No component introduces a third representation of morsel bytes, including in staging files. Owner: contracts. Parent D9.

**G-I7. Direct paths are performance, never correctness.** Every hardware-direct path (direct IO, io_uring, pinned memory, GPUDirect Storage, RDMA) has a fallback selected at start that produces byte-identical output, and the run report names the path taken. Owner: discovery (selection), reactor, arena, placement (fallbacks). Parent section 8 of the architecture document.

**G-I8. Failure is diagnosed, never signalled.** The runtime terminates a run itself, with a diagnostic naming the morsel, its features, its measured footprint and the budget, before the host can terminate it by signal; a run never ends in SIGKILL from the OOM killer under any kernel amplification. Owner: controller, placement, arena. Parent S6.

**G-I9. Parallelism does not depend on Python.** The scheduler, reactor, placement engine, arena and controller run at full parallelism on any Python build or with no Python present; the GIL affects only how many Python kernel invocations run concurrently. Owner: scheduler, adapters. Parent D4, S8.

**G-I10. Zero configuration beyond the budget.** A run started with defaults and no budget inside a cgroup, or with only a budget outside one, completes every benchmark in the suite. Owner: all. Parent S2, S5.

---

## 4. Process and concurrency model

### 4.1 Threads

One process. Five kinds of thread, fixed at start:

| Thread kind | Count | Created by | May touch | Must never |
|---|---|---|---|---|
| Main | 1 | the caller | builds the pipeline, calls `run`, blocks until completion | run a kernel; issue IO |
| Worker | N = discovered CPU ceiling (parked/active split managed by the scheduler) | scheduler | `Kernel::apply`, `Placement::pop` and `push`, `TraceRecord` emission, the arena | block on IO; write a knob; touch the reactor |
| Reactor | R = `reactor.threads` (default 2, section 5) | reactor | file and object-store IO, DMA copy issuance and completion, staging segment IO | run a kernel; allocate outside the arena |
| Controller | 1 | controller | discovery sampling, placement and scheduler stats, knob writes, trace reads | touch payload bytes; block on IO |
| Trace writer | 1 | trace | drains the trace channel to the trace file | anything else |

Kernels may use threads internally (a BLAS pool, Torch's intra-op pool) provided they are joined before `apply` returns; the scheduler cannot see them and the controller sizes for them only through observed CPU time.

### 4.2 Synchronisation and lock order

Locks are acquired in this order and never in reverse: (1) scheduler stage table, (2) placement queue order, (3) placement tier accounting, (4) arena free lists, (5) trace channel. A component that needs two of these takes the lower-numbered first. No lock is held across a call into another component except (1) held across a `Placement::pop`, which is documented in `10-scheduler.md`.

Lock-free paths, with the argument for each in the owning SDD: worker task pickup (scheduler, atomic stage cursor), tier byte counters (placement, atomics), arena size-class pop (arena, per-class lock-free stack or a mutex per class; the arena SDD decides and records the benchmark that justified it).

Maximum blocking: a worker blocks only in `Placement::pop` waiting for a resident head, and in `Kernel::apply` for as long as the kernel takes. The reactor never blocks on a lock held by a worker. The controller's tick is bounded at 5 ms of held locks; if a sample takes longer, it is skipped and counted.

### 4.3 Shutdown and cancellation

Three exits: completion (source exhausted, every queue drained, sink finished), termination by the runtime (diagnostic produced), cancellation (SIGINT or `KeyboardInterrupt` forwarded by the surface). All three follow the same sequence: the scheduler stops admitting source work; in-flight `apply` calls complete (bounded by the longest kernel); the placement engine cancels in-flight moves and releases tier accounting; the sink's `finish` runs on completion only, otherwise its completed files remain and uncommitted buffers are dropped; the trace channel drains and the writer flushes; the arena is released; the report is produced with the exit reason. Every component's SDD names what it does at each step.

---

## 5. Global configuration table

Every tunable in every component. Owner is who may set it at runtime: `user` (Python surface), `controller` (knob), `platform` (host profile or environment variable), `compile` (feature flag or constant). A value outside its range is clamped to the nearest bound and the clamp is reported. Component SDDs copy their rows into section i and may not add rows without adding them here.

| Name | Component | Type | Default | Range | Owner | Effect |
|---|---|---|---|---|---|---|
| `budget.host` | 3, 11 | bytes | discovered: `memory.high`, else 0.9 × `memory.max`, else 0.9 × total RAM | 256 MiB .. host | user, platform (`AMORU_BUDGET`) | host ceiling |
| `budget.reserve_fraction` | 11 | f32 | 0.10 | 0.02 .. 0.30 | platform | headroom never allocated |
| `budget.device` | 3, 11 | bytes per device | discovered free device memory × 0.9 | 64 MiB .. device | user | device ceiling |
| `budget.disk` | 9 | bytes | 20% of free space in staging dir, or pod ephemeral limit | 0 .. free | user, platform (`AMORU_SPILL_LIMIT`) | staging cap; 0 disables the disk tier |
| `staging.dir` | 9 | path | platform scratch (`/tmp`, pod ephemeral volume, `/local_disk0`) | existing writable dir | user, platform (`AMORU_SPILL_DIR`) | where segments go |
| `staging.segment_bytes` | 9 | bytes | 128 MiB | 64 MiB .. 256 MiB | compile | segment file size |
| `morsel.min_bytes` | 11 | bytes | 4 MiB | 1 MiB .. 64 MiB | compile | lower clamp on morsel target |
| `morsel.max_bytes` | 11 | bytes | 512 MiB | 64 MiB .. 2 GiB | compile | upper clamp on morsel target |
| `morsel.probe_bytes` | 11 | bytes | 16 MiB | 4 MiB .. 64 MiB | compile | probe morsel size |
| `morsel.alignment` | 1, 2 | bytes | 64 | fixed | compile | buffer alignment |
| `page.bytes` | 1, 2, 6 | bytes | 4096 (discovered when huge pages present) | fixed per host | discovery | direct IO alignment |
| `controller.target_fraction` | 11 | f32 | 0.85 | 0.60 .. 0.95 | compile | fraction of allowance the AIMD aims at |
| `controller.safety_initial` | 11 | f32 | 1.5 | 1.1 .. 3.0 | compile | initial multiplier on `A_k` |
| `controller.safety_floor` | 11 | f32 | 1.2 | 1.05 .. 1.5 | compile | lowest safety after convergence |
| `controller.increase_step` | 11 | f32 | 0.10 | 0.02 .. 0.25 | compile | additive increase per accepted adjustment |
| `controller.tick_ms` | 11 | ms | 250 | 50 .. 2000 | compile | periodic tick |
| `controller.damping_completions` | 11 | count | = active workers | 1 .. 64 | controller | min completions between adjustments |
| `controller.oscillation_flips` | 11 | count | 5 per 100 | fixed | compile | freeze trigger (S11) |
| `controller.freeze_morsels` | 11 | count | 100 | fixed | compile | freeze duration |
| `workers.max` | 3, 10 | count | discovered CPU quota (ceil) | 1 .. 1024 | platform | thread pool size |
| `workers.active` | 10 | count | = `workers.max` at start | 1 .. `workers.max` | controller | workers allowed to take tasks |
| `readahead.splits` | 7, 11 | count | 2 | 0 .. 64 | controller | source splits in flight |
| `queue.high_water` | 9 | bytes per queue per tier | set by controller from budget split | 0 .. tier budget | controller | admission threshold |
| `queue.low_water` | 9 | bytes per queue per tier | 0.5 × high | 0 .. high | controller | demotion stops here |
| `queue.promotion_window` | 9 | count | 2 | 1 .. 32 | controller | morsels promoted ahead of the head |
| `reactor.threads` | 6 | count | 2 | 1 .. 8 | platform | reactor thread count |
| `reactor.object_concurrency` | 6 | count | 8 | 1 .. 64 | controller (via readahead) | in-flight object-store requests |
| `arena.huge_pages` | 2 | bool | discovered | fixed | discovery | back arena with huge pages |
| `arena.pin` | 2 | bool | true when a device is present and memlock allows | fixed | discovery | page-lock the arena |
| `trace.path` | 4 | path | none (in-memory only) | writable path | user | write the trace file |
| `trace.channel_capacity` | 4 | count | 4096 | 256 .. 65536 | compile | records buffered before the writer |
| `errors.policy` | 10 | enum | `terminate` | `terminate`, `skip`, `budget(n)` | user | kernel error handling |
| `ordering.required` | 8, 10 | bool | false | fixed per sink | sink | reorder buffer on |
| `ordering.buffer_bytes` | 8 | bytes | 256 MiB | 16 MiB .. host budget/4 | compile | reorder buffer cap |
| `python.allow_gil` | 12 | bool | false | fixed | user | proceed serialised under a GIL |
| `profiles.dir` | 11 | path | `~/.amoru/profiles` | writable path or none | user, platform | profile store |
| `sizer` | 11 | enum | `rule` | `rule`, `learned` | user | decision function |
| `sizer.fallback_error_ratio` | 11 | f32 | 2.0 | 1.2 .. 5.0 | compile | learned → rule fallback trigger |
| `host_profile` | 3 | struct | probed | declared via `AMORU_HOST_PROFILE` | platform | guarantees; see `03-discovery.md` |

---

## 6. Crate layout, dependencies, build order

### 6.1 Workspace

```
amoru/
  Cargo.toml                 workspace; members below; shared [workspace.dependencies] with pinned versions
  crates/
    amoru-kernel/            component 1: contracts. Depends on arrow, dlpark, thiserror only.
    amoru-arena/             component 2
    amoru-discovery/         component 3
    amoru-trace/             component 4
    amoru-adapters/          component 5 (features: python, polars, datafusion)
    amoru-reactor/           component 6
    amoru-sources/           component 7
    amoru-sinks/             component 8
    amoru-placement/         component 9
    amoru-scheduler/         component 10
    amoru-controller/        component 11
    amoru-runtime/           facade: wires 2..11 into `Runtime::run`; no logic of its own
    amoru-py/                component 12: PyO3 module, built by maturin
    amoru-testkit/           fakes for every contracts interface, benchmark data generators, host probes
  python/amoru/              Python package source (thin; the module is amoru-py)
  bench/                     benchmark suite runner and baselines (section 6.5)
  architecture/              this documentation
```

Kernel authors depend on `amoru-kernel` alone. The Polars and DataFusion bridges in `amoru-adapters` depend on `amoru-kernel` alone plus the host engine.

### 6.2 Dependencies

Pinned in `[workspace.dependencies]`; the agent building component 1 pins the latest stable of each at the time of writing and records the versions in a table at the end of this section (escalation E2 covers version bumps thereafter).

| Crate | Used by | Purpose | Notes |
|---|---|---|---|
| `arrow` (arrow-rs) | 1, 5, 7, 8, 9 | in-memory format, C Data Interface, IPC | features: `ffi`, `ipc` |
| `parquet` | 7, 8 | reader with footer metadata, projection, row selection; writer | features: `arrow`, `async`, `object_store` |
| `object_store` | 6, 7, 8 | S3-compatible, GCS, Azure, local | features per backend behind runtime features |
| `dlpark` | 1, 5 | DLPack `DLManagedTensor` safe wrapper | verify v1.0 versioned struct support |
| `safetensors` | 7, 8 | model and tensor files | header parsing only; bytes are mapped, not copied |
| `tokio` | 6 | reactor runtime | `rt-multi-thread`, `fs`, `sync` |
| `io-uring` | 6 | direct IO on Linux | optional feature `uring`; fallback is `pread`/`pwrite` on a blocking pool |
| `crossbeam` | 9, 10 | bounded channels, deque | |
| `cudarc` | 2, 3, 6, 9 | CUDA driver: device memory, pinned host alloc, streams, copies | feature `cuda`; absent, `Tier::Device` is unconstructible |
| `pyo3` ≥ 0.28 | 5, 12 | Python bindings | free-threaded default; version-specific wheels, no abi3 |
| `pyo3-arrow` | 5 | Arrow ↔ pyarrow zero-copy | |
| `maturin` (build) | 12 | wheels | 3.13, 3.13t, 3.14, 3.14t |
| `thiserror` | all | error types | |
| `tracing` | all | log events (not the morsel trace) | |
| `mimalloc` | runtime | global allocator for non-arena allocations | returns freed memory promptly |
| `blake3` | 1, 11 | fingerprint, trace schema hash, profile keys | |

Pinned versions (filled by the component 1 agent): _pending_.

### 6.3 Feature flags

`cuda` (device tiers, pinned memory, copy engines), `uring` (io_uring path), `gds` (GPUDirect Storage; implies `cuda`), `rdma` (arena registration and `RdmaSource`; post-v1), `python`, `polars`, `datafusion` (adapters). Default features: none of these. `amoru-py` enables `python` and, on Linux, `uring`.

### 6.4 Test infrastructure (`amoru-testkit`)

Built alongside component 1 and extended by every component. It contains, for every interface in the contracts crate, a fake with deterministic and configurable behaviour: `FakeAllocator` (counts allocations, can refuse above a limit, can return misaligned buffers when asked to, to prove callers check), `FakeReactor` (in-memory files, configurable latency and failure injection, records every operation), `FakePlacement` (tier moves are instantaneous, records misses), `FakeSource` (emits generated splits and payloads), `FakeSink` (records writes, can throttle to a rate), `FakeKernel` (identity, or configurable amplification and delay, or failure on the nth morsel), `FakeKnobs`, `FakeTrace`. It also contains the benchmark data generator (section 6.5) and the host probes (a one-line check for each direct path, used by discovery's tests and by CI to label the host).

### 6.5 Benchmark suite

The generator writes Parquet with controllable row count, column mix (ints, floats, short strings, long text with configurable mean length and variance), null ratio and row-group size, to local disk and to an S3-compatible store (MinIO in a container), and writes safetensors and aligned binary tensors of controllable shape. Kernels: identity (A about 1), normalise (Rust regex over text, A about 1.5), tokenise-explode (A 5 to 10), wide-intermediate (Python NumPy, A about 20, releases the GIL), adversarial (A jumps 4× at the midpoint), embed-score (numeric columns to tensor, small matmul, back to column), torch-score (stateful GPU model, weights via TensorSource). For each, a hand-tuned baseline script (plain loop, fixed batch and threads, grid-searched) reports rows per second. Every gate runs in a container with `--memory` and `--cpus` set and on the bare host; results name the machine.

### 6.6 Build order and gates

Waves are the parallelism plan; the gate names the sufficiency criteria (parent S-ids) and global invariants the wave must close, measured by the tests the component SDDs name.

| Wave | Components | Agents in parallel | Gate |
|---|---|---|---|
| 0 | 1 | 1 | crate compiles with no runtime dependency (`cargo tree`); every fake compiles against the traits; CT tests pass; trace schema hash test pinned |
| 1 | 2, 3, 4, 5, testkit | up to 5 | AR, DS, TR, AD tests pass against fakes; G-I2 for the Python adapter (zero payload copies); S13 partial |
| 2 | 6 (and design of 9) | 1 | RE tests pass; direct IO and fallback both exercised on the developer host; G-I7 |
| 3 | 7, 8, 9 | 3 | SO, SI, PL tests pass; S10, S15 with `FakeSink` throttle; G-I3 |
| 4 | 10, 11 | 2 | end-to-end with fakes and with real components: S1, S2, S4, S5, S6, S11; G-I1, G-I4, G-I5, G-I8 |
| 5 | 12, runtime facade, bench | 1 | S3, S7, S8, S9, S12 on the reference hardware; G-I9, G-I10 |

No wave starts until the previous wave's gate is green, except that wave 2's design work on component 9 runs during wave 1.

### 6.7 Conventions every agent follows

Branch `component/NN-<name>`; one pull request per component; commits signed off (`-s`); `cargo fmt`, `cargo clippy -- -D warnings` clean; no `unwrap`/`expect` outside tests; every `unsafe` block has a `// SAFETY:` comment naming the invariant; tests named after the SDD ids (`ct_t3_payload_roundtrip`); benchmarks record the host in their output; no em dashes in documentation.

---

## 7. Escalation list

Decisions no agent makes on its own. Each carries the assumption the agent takes until a human answers.

**E1. Reference hardware.** Unnamed. Assumption: the developer's machine, results labelled provisional; direct-path gates that need a GPU or GDS are skipped and listed as skipped, never marked passed.

**E2. Dependency version bumps after wave 0.** Assumption: stay on the pinned versions; open an issue for a bump.

**E3. Ordering default.** Parent Q2. Assumption: unordered; `ordering.required` is opt-in per sink.

**E4. Error policy default.** Parent Q3. Assumption: `terminate`.

**E5. DAG support.** Parent Q4. Assumption: linear chain only; an agent that finds a DAG necessary stops and reports.

**E6. Profile store sharing.** Parent Q5. Assumption: per-user local directory.

**E7. Databricks budget discovery.** Parent Q6. Assumption: explicit budget required; discovery reports `LimitSource::Explicit`.

**E8. Weight-major execution.** Parent Q7. Assumption: out of scope; no component may add a dependency that would preclude it (a payload that is not `Tier`-tagged, a queue that cannot hold source morsels on disk).

**E9. Any `unsafe` outside the arena, the DLPack wrapper, the C Data Interface crossing and the direct IO calls.** Assumption: not permitted; the agent stops and reports.

**E10. Any new cross-component interface.** Assumption: not permitted in a component branch; it is a change to `01-contracts.md` and the contracts crate first, in its own pull request.

---

## 8. Traceability

Filled as component SDDs land. The parent's S-ids and D-ids map to the invariants and tests that close them; a row with no entry is a gap.

| Parent id | Closed by (invariants) | Proved by (tests) |
|---|---|---|
| S1 | G-I1, AR-*, RC-*, PL-* | wave 4 end-to-end |
| S2 | G-I10 | wave 4, wave 5 |
| S3 | RC-* | bench suite, wave 5 |
| S4 | SC-*, RC-* | wave 4 |
| S5 | DS-* | DS tests in container |
| S6 | G-I8 | wave 4 adversarial |
| S7 | CT-*, AD-* | wave 1, wave 5 |
| S8 | G-I9, AD-* | wave 1 (GIL detection), wave 5 (speedup) |
| S9 | G-I4, TR-* | wave 1, wave 4 |
| S10 | PL-* | wave 3 |
| S11 | RC-* | wave 4 |
| S12 | wave 0 skeleton | wave 5 |
| S13 | G-I2, CT-*, AD-* | wave 1, wave 3 |
| S14 | G-I2, PL-*, RE-* | wave 3 (pinned path), Phase 9 (GDS) |
| S15 | PL-* | wave 3 |
| D1 | SC-* | wave 4 |
| D2 | G-I5, RC-* | wave 4 |
| D3 | SO-*, RC-* | wave 3, wave 4 |
| D4 | G-I9, AD-*, PY-* | wave 1, wave 5 |
| D5 | PL-* | wave 3 |
| D6 | SC-* | wave 4 |
| D7 | RC-* | wave 4 (learned sizer post-v1) |
| D8 | SC-* | wave 4 |
| D9 | G-I6, CT-* | wave 0 |
| D10 | G-I2, G-I3, PL-* | wave 3 |
| D11 | E8 | (deferred) |

---

## 9. Hand-off protocol

An agent receives: this preamble, `01-contracts.md`, its component SDD, and the `amoru-testkit` fakes for every interface its component consumes. It works on branch `component/NN-<name>`. It is done when every test in its SDD's section k exists and passes, every invariant in section c is cited by at least one test, `cargo clippy` is clean, section m of its SDD is empty, and its pull request's description lists the invariants and tests by id. It stops and reports, rather than deciding, on anything in section 7. It does not read other components' SDDs; if it believes it needs to, that is a contracts gap and is reported.
