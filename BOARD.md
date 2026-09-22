# Amoru build board

The PM agent's working board. It is the one place that says what is done, what is next and who is waiting on whom; the documents in `architecture/` remain the specification and this board never restates them. When every box on this board is ticked, `amoru` is a production-ready package: every wave gate of preamble 6.6 is green, every sufficiency criterion S1 to S17 is closed on the reference host or listed as skipped by id with Brackly's acceptance, every decision Q1 to Q10 is decided or its assumption is explicitly accepted, and wheels for the four interpreters are published from a tagged release.

## How the board works

- **Epic** = one wave of preamble 6.6 (E0 to E5) plus one release-readiness epic (E6). Waves are not re-planned here; a wave's gate is the preamble's.
- **Feature** = one unit of work for one agent in one session: the code plus the tests that prove it, at or above 90% line coverage (preamble 6.7, `tools/quality/check.sh`). A feature is sized so an Opus executor can carry it from brief to a green gate without stopping for the PM, and so that no feature owns more than about a third of a component's invariants.
- **Component branch and pull request.** One pull request per component (CONTRIBUTING.md). A component split into several features lands them as consecutive sessions on the same `component/NN-<slug>` branch; the pull request opens after the first feature and is reviewed against the SDD when the last feature is done (decision D-B1 below).
- **Tasks** under a feature are the checklist the executor and the PM tick. Every feature carries the same closing tasks: the pull request template filled, the PM checklist in `architecture/agents/pm.md` section 4 green, section m of the SDD still empty.
- **Status** per feature: `todo`, `briefed`, `in progress`, `in review`, `returned`, `merged`, `blocked (why)`. The PM updates this file in the same commit as the review or the merge.
- **Test ids** are the SDD's section k ids. A tag in parentheses is the SDD's: `(E1)` skipped and listed by id (device or GDS) or provisional with the host name (timing); `(integration, wave N)` closes in that wave.

Executors are briefed from `architecture/agents/executor.md`; filled briefs live in `architecture/agents/briefs/`.

## Now

| | |
|---|---|
| Current wave | 0, in progress since 2026-09-22 |
| Delegation | Brackly delegated decision making to the PM on 2026-09-22 ("with great power comes great responsibility"); the PM now decides every item the preamble's table routes to the human, records each in this board's decisions table with the date, and reports it in the wave report; a decision that changes an SDD's behaviour still goes through the design-change template first |
| Next action | F0.1 executor running (workspace skeleton, CI, lint); the PM reviews its pull request, then briefs F0.2 |
| Blocking Brackly items | none; D-B3 (reference host access) is needed at wave 5 |

---

## E0. Wave 0: contracts, testkit, workspace, CI, quality gate

Gate (preamble 6.6, wave 0): the workspace compiles with every member crate as a stub; `amoru-kernel` has no runtime dependency (CT-T12); every fake in contracts d.15 compiles and exercises every knob (CT-T13); CT tests pass; the trace schema hash is pinned (CT-T9); `tools/lint/no_tier_wildcard.sh` runs in CI (CT-T14); `tools/quality/check.sh` is green; the four CI jobs are green on the stubs.

| Id | Feature | Branch | Scope (SDD sections, tests by id) | Status |
|---|---|---|---|---|
| F0.0 | PM preparation: quality gate, hook, board, brief for component 1 | `infra/pm-wave0-prep` | preamble 6.1, 6.6, 6.7 edits; `tools/quality`, `tools/hooks`; this board | merged |
| F0.1 | Workspace skeleton, CI with four jobs, `tools/lint`, pinned versions | `infra/workspace` | preamble 6.1, 6.2, 6.3, 6.6; CT-T12, CT-T14 (the lint half) | in progress |
| F0.2 | Contracts crate `amoru-kernel` | `component/01-contracts` | 01 d.1 to d.14, e.1 to e.7, f.1 to f.6; CT-T1 to CT-T12, CT-T14 to CT-T19 | todo |
| F0.3 | Testkit `amoru-testkit` | `component/01-contracts` (same PR as F0.2) | 01 d.15; CT-T13 | todo |
| F0.4 | Wave 0 gate and wave report (PM) | `main` | pm.md sections 2, 4, 8 | todo |

### F0.0 tasks
- [x] `tools/quality/check.sh`: em dashes, fmt, clippy, lint, tests, per-crate coverage at 90% (`coverage_gate.py`)
- [x] `tools/hooks/pre-commit` and `tools/hooks/install.sh` (`core.hooksPath`); installed in this clone
- [x] Preamble 6.1, 6.6, 6.7, CONTRIBUTING, PR template, pm.md, executor.md carry the gate and the 90% floor
- [x] `DECISIONS.md` Q8 date column filled
- [x] `BOARD.md` (this file) with every epic, feature and task
- [x] Filled brief for component 1 in `architecture/agents/briefs/01-contracts.md`
- [x] D-B1 and D-B2 decided under delegation, D-B3 deferred to wave 5; branch merged to `main` and pushed to origin

### F0.1 tasks
- [ ] Environment facts verified in the PR (rustc, cargo, edition 2024; docker and MinIO reachable in CI)
- [ ] `rust-toolchain.toml` pins stable; `Cargo.toml` workspace with every member of preamble 6.1; `[workspace.dependencies]` pinned; the versions recorded in the preamble 6.2 table (replaces `_pending_`)
- [ ] Every crate of 6.1 as a compiling stub with its `Cargo.toml`, feature flags per 6.3, and a one-line `lib.rs` doc comment naming its SDD
- [ ] `python/amoru/` and `bench/` placeholders that the later waves fill
- [ ] `tools/lint/no_tier_wildcard.sh` (CT-T14 lint) with a self-test fixture
- [ ] `.github/workflows/ci.yml`: job 1 `tools/quality/check.sh` on Linux; job 2 container gate with `--memory` and `--cpus`; job 3 MinIO service for the object-store paths; job 4 Python matrix 3.13, 3.13t, 3.14, 3.14t (proves the matrix runs)
- [ ] `cargo tree -p amoru-kernel` is free of tokio, cudarc, pyo3, parquet, object_store (CT-T12 as a CI step until F0.2 makes it a test)
- [ ] PR template complete; PM checklist green; merged

### F0.2 tasks
- [ ] Environment facts from 01 section l verified and recorded (cargo version, `blake3`, `dlpark` versioned-struct support)
- [ ] `src/` files exactly as 01 section l lists them; `unsafe` only in `tensor.rs`, `payload.rs`, `view.rs`, `buffer.rs` with `// SAFETY:` on every block
- [ ] d.1 to d.3: ids, `Tier`, `TierKind`, `StagingCodec`, `Buffer`, `BufferView`, `Allocator`, `AllocStats`
- [ ] d.4 to d.5: `DType`, `ManagedTensor` (dlpark wrapper, `from_buffer`), `Payload`, `PayloadSpec::check`, `SourceSchema::hash`, `Morsel`, `MorselFeatures` (f.2 O(columns))
- [ ] d.6 to d.8: `Source`, `Kernel`, `KernelState`, `Sink` with the resume defaults that refuse
- [ ] d.9: `Completion` over std, `Reactor`, `ObjectMetadata`, copy endpoints; d.10 `Placement` and the resume types; d.11 `Knobs`, `StatsSource`, `Prober`, `CancelToken`, `RecordHook`; d.12 `Limits`, `HostProfile`, `Sampler`; d.13 `TraceRecord`, `SCHEMA_HASH`, `TraceSink`, `TraceTail`; d.14 errors
- [ ] e.4 `AMB1` reader and writer over slices; e.7 page-aligned IPC `encode_framing` and `decode`; e.6 `Fingerprint::compute`
- [ ] Tests CT-T1 to CT-T12 and CT-T14 to CT-T19 under their SDD names; CT-T9 pins the hash value in the test; CT-T11 timing reported with the host name (see finding N-2)
- [ ] Every CT invariant CT-I1 to CT-I12 cited by at least one test in the PR
- [ ] Coverage at or above 90% for `amoru-kernel`; PR template complete

### F0.3 tasks
- [ ] Every fake of d.15 (`FakeAllocator`, `FakeReactor`, `FakePlacement`, `FakeSource`, `FakeSink`, `FakeKernel`, `FakeSampler`, `FakeTrace`, `FakeKnobs`) with exactly the knobs and observables the table lists, and a `shutdown_calls` counter wherever the trait has `shutdown`
- [ ] `FakeAllocator` buffers are real heap allocations tagged with the tier so tier inference, `into_arrow_buffer` and `BufferView::of_arrow` work
- [ ] `FakePlacement::with_manifest_store` round-trips `checkpoint`/`restore` across engine instances
- [ ] CT-T13: every method and every knob exercised once
- [ ] `amoru-testkit` depends on `amoru-kernel` only; coverage at or above 90%; same PR as F0.2

### F0.4 tasks
- [ ] PM review of F0.1 and F0.2+F0.3 against pm.md section 4, lines 1 to 11
- [ ] Wave 0 report: tests run, tests skipped by id, provisional figures with host names, escalations opened
- [ ] E1 environment items for wave 1 recorded (Python interpreters with NumPy and pyarrow, cgroup v2 host)
- [ ] SDD 01 listed for Brackly to flip to HANDOFF-READY once E1 and E2 assumptions are accepted

---

## E1. Wave 1: arena, discovery, trace, adapters, bench generator and kernels

Gate (6.6, wave 1): AR, DS, TR, AD tests pass against fakes; G-I2 for the Python adapter (zero payload copies); S13 partial; the generator writes every dataset shape the suite names to local disk and MinIO. Environment: wave 0 plus the four interpreters with NumPy and pyarrow, and a cgroup v2 host (a container is enough) for component 3.

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F1.1 | Memory arena `amoru-arena` | `component/02-arena` | 02 in full; AR-T1 to AR-T12; `cuda` arms present, device tests listed | todo |
| F1.2 | Resource discovery `amoru-discovery` | `component/03-discovery` | 03 in full; DS-T1 to DS-T12; DS-T3 timing provisional (E1); DS-T9 (integration, wave 1: the CI container job) | todo |
| F1.3 | Trace writer and run report `amoru-trace` | `component/04-trace` | 04 in full; TR-T1 to TR-T11 | todo |
| F1.4 | Python kernel adapter `amoru-adapters` | `component/05-adapters` | 05 in full except the bridges; AD-T1, T2, T4 to T8, T12; AD-T3, AD-T11 (E1, skipped and listed) | todo |
| F1.5 | Polars and DataFusion bridges `amoru-polars`, `amoru-datafusion` | `component/05-adapters` (same PR) | 05 bridge sections; AD-T9, AD-T10 (integration, wave 1; need F1.7's `normalise` kernel) | todo |
| F1.6 | Bench data generator | `infra/bench` | preamble 6.5: Parquet with controllable rows, column mix, null ratio, row-group size, to local disk and MinIO; safetensors and aligned binary tensors | todo |
| F1.7 | Bench kernels | `infra/bench` (same PR as F1.6) | preamble 6.5: identity, normalise, tokenise-explode, adversarial, wide-intermediate (Python, releases the GIL), embed-score; torch-score deferred (no GPU host, E1) | todo |
| F1.8 | Wave 1 gate and report (PM) | `main` | | todo |

### F1.1 tasks
- [ ] Environment facts from 02 section l verified; `unsafe` only in the modules 02 section l names
- [ ] `Allocator` implemented per 1.3's edge contract: 64-byte alignment, page alignment above a page, tier honoured or the call fails; `is_pinned` all or nothing
- [ ] Size classes, free lists and the lock-free versus mutex decision recorded with the benchmark that justified it (preamble 4.2)
- [ ] `contains`, `tier_of`, `AllocStats` exact; small-budget mode; large coalescing
- [ ] AR-T1 to AR-T12 under SDD names; every AR-I cited; coverage at or above 90%; PR template complete

### F1.2 tasks
- [ ] Environment facts from 03 section l verified (cgroup v2 paths in a container, `memory.peak` presence)
- [ ] `discover`: precedence explicit, cgroup, OS; ceiling below kill; `LimitSource`; size strings; Databricks refusal (E7 assumption)
- [ ] `HostProfile` parsing from `AMORU_HOST_PROFILE`; every `Unknown` becomes `Probed`; `Present` on tmpfs or overlay staging refused
- [ ] `Sampler` with `reset_peak`, anon not current, idempotent, sample cost measured and labelled with the host
- [ ] DS-T1 to DS-T12; DS-T9 wired into the CI container job; every DS-I cited; coverage; PR template

### F1.3 tasks
- [ ] Environment facts from 04 section l verified
- [ ] Bounded channel, writer thread, in-memory chunks with `trace.memory_limit` overflow to disk, flush on `finish` and on abort, `TraceTail`
- [ ] Run report as a pure function of the trace and the limits (formulas of 04), staging versus source bandwidth, disk-full handling
- [ ] TR-T1 to TR-T11; every TR-I cited; coverage; PR template

### F1.4 tasks
- [ ] Environment facts from 05 section l verified (the four interpreters, NumPy, pyarrow, pyo3 at or above 0.28)
- [ ] Python callable as `Kernel`: zero-copy crossing both ways, one boundary copy of non-arena output (AD-I2, `boundary_copies_total`), deleter attaches to the interpreter, exception context, fingerprint from qualified name plus source hash, class-kernel shape
- [ ] GIL detection and flip reported per stage (`GilState`)
- [ ] AD-T1, T2, T4 to T8, T12 pass; AD-T3 and AD-T11 exist, ignored with the E1 reason, listed; every AD-I cited; coverage; PR template

### F1.5 tasks
- [ ] `amoru-polars`: a kernel as a Polars expression plugin, no kernel logic in the wrapper; `amoru-datafusion`: a kernel as a `ScalarUDF`
- [ ] AD-T9, AD-T10 against the bench `normalise` kernel (after F1.7 merges); S7 closed for the two hosts
- [ ] Both crates depend on `amoru-kernel` plus the host engine only; feature flags per 6.3; coverage; same PR as F1.4 or a follow-up commit on the branch

### F1.6 tasks
- [ ] `bench/` generator: every dataset shape 6.5 names, deterministic by seed, to local disk and to MinIO
- [ ] Output names the machine and the generator version; a `bench/README.md` states how to regenerate
- [ ] Tests for the generator itself at or above 90% coverage (the bench runner is code too)

### F1.7 tasks
- [ ] Six kernels with their declared amplification classes; `wide-intermediate` in Python releasing the GIL
- [ ] Each kernel usable by AD-T9/T10 and by the wave 4 end-to-end tests
- [ ] Tests and coverage; PR with F1.6

### F1.8 tasks
- [ ] PM reviews five PRs against pm.md section 4; merges; wave 1 report; E1 items for wave 2 (an `O_DIRECT` filesystem, io_uring where allowed)
- [ ] SDDs 02 to 05 listed for Brackly to flip

---

## E2. Wave 2: IO reactor

Gate (6.6, wave 2): RE tests pass; direct IO and the fallback both exercised on the developer host; G-I7. Environment: wave 1 plus an `O_DIRECT` filesystem (ext4 or xfs on a local disk), io_uring where the host allows, MinIO; GDS and CUDA tests tagged for the reference host (E1).

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F2.1 | Reactor core: completions, file and object IO, concurrency bounds, shutdown | `component/06-reactor` | 06 d, f (except f.5), g, h; RE-T1, T2, T3, T6, T7, T8, T9, T13, T14, T15 | todo |
| F2.2 | Reactor direct paths: direct IO, io_uring, sticky fallback, copy table with `cuda`/`gds`/`rdma` arms | `component/06-reactor` (same PR) | 06 f.5, section l FFI notes; RE-T4, T10, T12; RE-T5 (E1, skipped and listed); RE-T11 (E1, provisional with host name) | todo |
| F2.3 | Wave 2 gate and report (PM) | `main` | | todo |

### F2.1 tasks
- [ ] Environment facts from 06 section l verified (filesystem accepts `O_DIRECT`, io_uring probe result, MinIO reachable)
- [ ] tokio reactor with `reactor.threads`; every operation completes exactly once into the buffer it was given, on a reactor thread; submission never blocks; `then` runs on the reactor thread
- [ ] `read_file`, `read_file_opt`, `write_file`, `read_object`, `write_object`, multipart, `ObjectMetadata`, segment registry, `paths()`, `shutdown` within the longest operation
- [ ] RE-T1, T2, T3, T6, T7, T8, T9, T13, T14, T15; every RE-I cited; coverage; PR template

### F2.2 tasks
- [ ] Direct IO when aligned, buffered otherwise; io_uring path behind `uring` with byte-equal fallback; sticky fallback after a failure; `Probed` versus `Present` behaviour (G-I7)
- [ ] Copy table f.5 with every arm named: `cuda` copy engine rows, `gds` rows over the minimal `libcufile` FFI (section l), `rdma` rows returning `Unsupported("rdma")`
- [ ] RE-T4, T10, T12 pass; RE-T5 skipped and listed; RE-T11 measured and labelled provisional with the host name
- [ ] Coverage for the non-device paths; PR template; same PR as F2.1

### F2.3 tasks
- [ ] PM review, merge, wave 2 report; E1 items for wave 3 (10 GiB free local disk, MinIO)
- [ ] SDD 06 listed for Brackly to flip

---

## E3. Wave 3: sources, sinks, placement

Gate (6.6, wave 3): SO, SI, PL tests pass; S10 and S15 with `FakeSink` throttle; G-I3; manifest round trip and sink commit tracking (PL-T16, SI-T12, SI-T13). Environment: wave 2 plus 10 GiB free for the staging tests and MinIO. Three executors in parallel.

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F3.1 | `ParquetSource` over local paths and object stores | `component/07-sources` | 07 a to h for Parquet; SO-T1 to T8, T11, T13 to T16; SO-T12 (E1, provisional) | todo |
| F3.2 | `TensorSource` (safetensors, AMB1) and `PyIteratorSource` | `component/07-sources` (same PR) | 07 tensor and iterator sections; SO-T9; SO-T10 (integration, wave 3; `python`) | todo |
| F3.3 | `ParquetSink`, `SinkHandle`, commit tracking and resume | `component/08-sinks` | 08 a to h for Parquet, e.5, resume; SI-T1 to T4, T7, T11 to T16; SI-T8 (integration, wave 3) | todo |
| F3.4 | `TensorSink`, `ArrowIpcSink`, `ReorderBuffer` | `component/08-sinks` (same PR) | 08 e.3, reorder; SI-T5, T9; SI-T10 (integration, wave 3); SI-T6 (E1, skipped and listed) | todo |
| F3.5 | Placement core: queues, accounting, admission, head hot, promotion window, move table | `component/09-placement` | 09 c, d, e.1, e.4, f (promotion and demotion), g; PL-T1 to T4, T8, T9, T11, T19 to T21, T24 | todo |
| F3.6 | Placement staging: segments, record format, disk bound, Q0 eviction and replace, move failures | `component/09-placement` (same PR) | 09 e.2, e.3, f.5 to f.10, h; PL-T5 to T7, T10, T12, T14, T15; PL-T13 (E1, provisional) | todo |
| F3.7 | Placement lineage and manifest: checkpoint, restore, lineage index, lock order, shutdown | `component/09-placement` (same PR) | 09 e.5, f.11, f.12; PL-T16, T18, T22, T23; PL-T17 (integration, wave 4) | todo |
| F3.8 | Wave 3 gate and report (PM) | `main` | | todo |

### F3.1 tasks
- [ ] Environment facts from 07 section l verified; bench generator files available in test setup (F1.6)
- [ ] `plan` complete before the first `read`, exact metadata from the footer, row-group sub-splitting with `RowSelection`, pruning, one decode copy into the arena in the requested tier, zero-row and oversized-row handling, schema mismatch, read failure paths, local paths through `read_file` only
- [ ] SO-T1 to T8, T11, T13 to T16 pass; SO-T12 provisional with host name; every SO-I cited; coverage; PR template

### F3.2 tasks
- [ ] `TensorSource`: safetensors header parse, mapped bytes, aligned copy only when the offset is unaligned (architecture 2.2 row), AMB1 read; `PyIteratorSource` with `repeatable() == false`
- [ ] SO-T9 passes; SO-T10 under the `python` feature; coverage; same PR as F3.1

### F3.3 tasks
- [ ] Environment facts from 08 section l verified
- [ ] `ParquetSink`: ownership once, encode-only copy into the arena, row-group and file roll targets, `finish` exactly once, no partial final file, `SinkSummary` exact, `sink.concurrency`, slow store behaviour
- [ ] `committed_seq` never overstates (SI-I8), `skip`, `checkpoint` returns `Some` from open, `resume` discards uncommitted output; the non-resumable path says so (SI-T14)
- [ ] `SinkHandle` for the scheduler (SI-T15)
- [ ] SI-T1 to T4, T7, T11 to T16 pass; SI-T8 closes when `ParquetSource` merges; every SI-I cited; coverage; PR template

### F3.4 tasks
- [ ] `TensorSink` writing AMB1 and safetensors; `ArrowIpcSink` writing the page-aligned IPC of contracts e.7; `ReorderBuffer` bounded by `ordering.buffer_bytes`
- [ ] SI-T5, T9 pass; SI-T10 closes with `TensorSource`; SI-T6 skipped and listed; coverage; same PR as F3.3

### F3.5 tasks
- [ ] Environment facts from 09 section l verified; `unsafe` only where 09 section l permits
- [ ] Entry state machine e.1 with the disk-copy dimension; per-queue, per-tier atomic byte counters exact; FIFO; `is_full` against high water; head stays hot; promotion window ahead of the head; consumer tier from `set_consumer`
- [ ] Move table e.4 with every arm named; `Tier::Remote` and `Locality::Local` return `Unsupported("rdma")` (PL-T19); completions observed through `then`, never a placement thread
- [ ] PL-T1 to T4, T8, T9, T11, T19 to T21, T24 pass; the PL-I they cite listed; coverage

### F3.6 tasks
- [ ] Staging segments: `staging.segment_bytes` files, page-aligned records (e.3) with the `StagingCodec` byte, direct IO through the reactor, `register_segment`/`unregister_segment`, segment deleted when empty, `budget.disk` bound with the S10 diagnostic
- [ ] Q0 eviction (D5) with `evicted`/`replace`; Q0 staging when the source is not repeatable; move failure paths; concurrency test
- [ ] PL-T5 to T7, T10, T12, T14, T15 pass; PL-T13 (S15 throughput) provisional with host name; coverage

### F3.7 tasks
- [ ] Manifest e.5 written atomically (write, fsync, rename) on the calling thread; `restore` validates plan digest and fingerprints, rebuilds `OnDisk` entries, lists `to_recompute`; lineage index bounded by `set_committed` (f.11); lock order of preamble 4.2 (PL-T23); `shutdown` idempotent
- [ ] PL-T16, T18, T22, T23 pass; PL-T17 tagged for wave 4; every PL-I1 to PL-I15 cited across F3.5 to F3.7; PR template complete; PR reviewed as one

### F3.8 tasks
- [ ] PM reviews three PRs; S10 and S15 evidence with `FakeSink` throttle recorded; G-I3 evidence (PL-T1, PL-T8)
- [ ] Wave 3 report; E1 items for wave 4 (a container runtime honouring `--memory` and `--cpus`)
- [ ] SDDs 07, 08, 09 listed for Brackly to flip

---

## E4. Wave 4: scheduler, controller, Rust facade

Gate (6.6, wave 4): end-to-end with fakes and with real components: S1, S2, S4, S5, S6, S11; G-I1, G-I4, G-I5, G-I8; kill-and-resume equivalence (SC-T16, PL-T17); the facade's lifecycle (preamble 4.4) driven by RC-T12. Environment: wave 3 plus a container runtime that honours `--memory` and `--cpus`. Three executors; the facade executor reads every SDD.

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F4.1 | Scheduler core: chain validation, worker pool, pick and admission, instance pools, trace emission, knobs, heartbeat | `component/10-scheduler` | 10 c, d, e, f.1 to f.4, g; SC-T1 to T6, T8, T17, T18, T19 | todo |
| F4.2 | Scheduler drives, probe protocol, error policies, cancellation, evicted replay | `component/10-scheduler` (same PR) | 10 f.5 to f.10, h; SC-T7, T9, T10, T11, T13; SC-T12 (E1, provisional) | todo |
| F4.3 | Scheduler checkpoint thread, watermark, `apply_resume_point`, `run_resumed` | `component/10-scheduler` (same PR) | 10 f.11 onward, resume; SC-T14, T15; SC-T16 (integration, wave 4) | todo |
| F4.4 | Controller sizing: prepare, probe, envelope, rule sizer, AIMD, damping, oscillation freeze, breach, state growth, tick bound | `component/11-controller` | 11 c, d, f.1 to f.5; RC-T1 to T8, T11, T15 to T18 | todo |
| F4.5 | Controller classification, profile store, resume seeding, learned-sizer stub with shadow error | `component/11-controller` (same PR) | 11 f.6 onward, e.3; RC-T9, T10, T14; RC-T13 (E1); RC-T12 (integration, wave 4) | todo |
| F4.6 | Rust facade `amoru-runtime`: lifecycle 4.4, `RunSpec`, report, cancel, mimalloc | `component/12-runtime` | 12 sections for the facade (f.1, f.2, f.7, section l Rust files); the lifecycle of preamble 4.4 fresh and resumed | todo |
| F4.7 | Wave 4 integration closure with real components | `component/12-runtime` (same PR) or `infra/wave4-integration` | RC-T12 in a container, SC-T16, PL-T17; S1, S2, S4, S5, S6, S11 evidence | todo |
| F4.8 | Wave 4 gate and report (PM) | `main` | | todo |

### F4.1 tasks
- [ ] Environment facts from 10 section l verified
- [ ] Chain validation (`PayloadSpec::check` at plan time), N workers spawned parked, atomic stage cursor pick, admission rule (least output buffered, source last), `peek_resident` before pop, instance pools with affinity, eager `init_instances`, failure of a stateful init
- [ ] One `TraceRecord` per morsel per stage (G-I4), sampler before and after `apply`, `state_bytes` from `footprint`, `RecordHook`
- [ ] `Knobs` with clamping counted in `knob_clamps`, forwarding to placement setters; `StatsSource`; worker heartbeat; single row larger than max handled
- [ ] SC-T1 to T6, T8, T17 to T19 pass; coverage

### F4.2 tasks
- [ ] Source and sink drives (preamble 4.1), `Completion::wait` only there, `read_ahead` and `is_full` obeyed, `SinkHandle` writes, `set_committed`
- [ ] `Prober`: stage 1 by one read at the cursor, later stages by popping the previous head, one worker with the rest parked
- [ ] Error policies `terminate`, `skip`, `budget(n)` with `Sink::skip`; draining order; cancel token; evicted replay through `evicted`/`replace`
- [ ] SC-T7, T9, T10, T11, T13 pass; SC-T12 provisional with host name

### F4.3 tasks
- [ ] Checkpoint thread per preamble 4.1 (`KernelState::checkpoint`, `Sink::checkpoint`, `Placement::checkpoint`), final manifest on termination and cancel from the scheduler's thread; watermark and skips
- [ ] `apply_resume_point`: refusal when checkpoint disabled naming sink or staging dir, `Sink::resume`, cursor and sequence, `Checkpoint` restore and `Reinit` re-init; `run_resumed` re-reads `to_recompute`
- [ ] SC-T14, T15 pass; SC-T16 written and tagged; every SC-I1 to SC-I11 cited; PR template complete

### F4.4 tasks
- [ ] Environment facts from 11 section l verified
- [ ] `prepare` (baseline after every init), `probe_all` and `probe_missing`, envelope from budget and probe, rule sizer with AIMD, `damping_completions`, oscillation freeze (S11), breach handling with the G-I8 diagnostic through `Knobs::terminate`, workers bounded by memory, one action per tick, tiny dataset, tick lock bound (RC-T17), state growth, device OOM retry
- [ ] Every knob written only by the controller (G-I5); `set_budgets` the only placement setter it calls
- [ ] RC-T1 to T8, T11, T15 to T18 pass; coverage

### F4.5 tasks
- [ ] Bottleneck classification table (f.6) from `SchedulerStats`, placement stats and samples; profile store keyed by fingerprint and schema hash (e.3) in `profiles.dir`; resume seeds the profile; learned sizer stub with shadow error tracking and `sizer.fallback_error_ratio` fallback
- [ ] RC-T9, T10, T14 pass; RC-T13 tagged E1; RC-T12 tagged integration; every RC-I1 to RC-I10 cited; PR template

### F4.6 tasks
- [ ] Facade executor briefed with "reads every SDD" (preamble 9); environment facts verified
- [ ] `Runtime::run` builds and starts components in the order of preamble 4.4, fresh and resumed; `RunReport::compute`; `Terminated` mapped to an error with the partial report; `shutdown` and `Drop`; trace `finish` exactly once; clamping of every non-scheduler, non-discovery row reported in `notes`; `#[global_allocator]` mimalloc
- [ ] Unit tests with fakes for the ordering (PY-T1 startup_order lives here on the Rust side); coverage

### F4.7 tasks
- [ ] RC-T12 in the CI container job with `--memory`: all five benchmark kernels complete within B (S1, S2, S5, S6)
- [ ] SC-T16 and PL-T17: SIGKILL at a random point, resume from the manifest, output equal (S17 on the CI host)
- [ ] S4 and S11 evidence from the trace; G-I1, G-I4, G-I5, G-I8 named with their tests in the report
- [ ] Wave 4 timing figures labelled provisional with the CI host name

### F4.8 tasks
- [ ] PM reviews three PRs; merges; wave 4 report; E1 items for wave 5 (maturin, four interpreters, Polars and DuckDB, reference host access)
- [ ] SDDs 10, 11 and the facade half of 12 listed for Brackly to flip

---

## E5. Wave 5: Python package, wheels, bench baselines, reference host

Gate (6.6, wave 5): S3, S7, S8, S9, S12, S17 on the reference hardware; G-I9, G-I10, G-I12 (PY-T12); the whole configuration table clamped (PY-T13). Environment: wave 4 plus maturin and the four interpreters for the wheel matrix, Polars and DuckDB for the engine baseline, and the reference host for the timing gates.

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F5.1 | `amoru-py` module and `python/amoru` package | `component/12-python` | 12 a to h; PY-T1 (Python side), T2, T4, T5, T6, T8, T9, T14 | todo |
| F5.2 | Wheel matrix, configuration clamping walk, end-to-end in a container | `component/12-python` (same PR) | 12 section l packaging; PY-T7, PY-T13; PY-T3 and PY-T12 (integration, wave 5) | todo |
| F5.3 | Bench runner and hand-tuned baselines | `infra/bench` | preamble 6.5 wave 5 part: grid-searched plain loop per kernel, rows per second, host named; `AMORU_BENCH_MORSEL_BYTES` for S12 | todo |
| F5.4 | Engine baseline (Polars streaming, DuckDB UDF) | `infra/bench` (same PR) | preamble 6.5: same kernel, same files, same container, defaults and then the documented memory limit; reported beside the tuned baseline, never a gate | todo |
| F5.5 | Reference-host campaign | `infra/reference-host` | every test tagged `(reference host, E1)` run on the Griot server in Nairobi: DS-T3, AD-T11, RE-T11, SO-T12, PL-T13, SC-T12, RC-T13, PY-T10, PY-T11; the device and GDS tests (AD-T3, RE-T5, SI-T6) listed as skipped by id | blocked (D-B3: agent access to the reference host) |
| F5.6 | Wave 5 gate and report (PM) | `main` | | todo |

### F5.1 tasks
- [ ] Environment facts from 12 section l verified (maturin, the four interpreters, pyo3-arrow)
- [ ] `amoru.run(source, kernels, sink, ...)` and the source, kernel and sink wrappers; exception mapping from `AmoruError`; the report object; `KeyboardInterrupt` forwarded as cancel; GIL refusal unless `python.allow_gil`; no pandas dependency; re-entrancy
- [ ] PY-T1, T2, T4, T5, T6, T8, T9, T14 pass; every PY-I cited; Python tests at or above 90% coverage (`ruff` and `pytest --cov` in the gate); PR template

### F5.2 tasks
- [ ] CI wheel matrix 3.13, 3.13t, 3.14, 3.14t (PY-T7); version-specific wheels, no abi3; `amoru-py` enables `python` and, on Linux, `uring`
- [ ] PY-T13 walks the whole preamble section 5 table with one out-of-range value per row, checks the clamp owner and the report note
- [ ] PY-T3 in the container job; PY-T12 three-stage resume end to end with real components (G-I12)

### F5.3 tasks
- [ ] Bench runner: runs every benchmark in a container with `--memory` and `--cpus` and on the bare host; output names the machine
- [ ] Hand-tuned baseline per kernel by grid search over batch size and worker count; S3 ratio computed; S12 with the identity kernel and 256 MB morsels against the plain loop, defaults and the pinned figure both reported
- [ ] Tests and coverage for the runner

### F5.4 tasks
- [ ] Polars and DuckDB baselines per kernel, defaults then the memory limit set to the budget; reported beside the tuned baseline
- [ ] Documented as external evidence, not a gate

### F5.5 tasks
- [ ] Access path for an agent (or Brackly by hand) to run the suite on the reference host agreed (D-B3)
- [ ] Every E1-tagged timing test run there; figures recorded with the host name in the wave 5 report and the bench output
- [ ] Device and GDS tests listed as skipped by id with the reason "no GPU host (E1, decided 2026-09-22)"

### F5.6 tasks
- [ ] PM reviews, merges, wave 5 report: S3, S7, S8, S9, S12, S17 closed with host names; G-I9, G-I10, G-I12 evidence
- [ ] SDD 12 listed for Brackly to flip

---

## E6. Release readiness: a production-ready `amoru`

No preamble gate; this epic is what "done" means beyond the waves. Nothing here changes a design document without going through the escalation path.

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F6.1 | Decisions and document status closed | `main` | `DECISIONS.md` Q2 to Q7, Q9, Q10 decided or their assumptions explicitly accepted by Brackly; every SDD HANDOFF-READY; preamble section 8 traceability table with no gap row | todo |
| F6.2 | Sufficiency sign-off | `main` | S1 to S17 each cited to the test and the host that closed it, or listed as skipped by id with Brackly's acceptance (device and GDS parts of S14) | todo |
| F6.3 | Packaging and publishing | `infra/release` | Q1 remainder: PyPI, crates.io and GitHub namespace checks, trademark and domain; version `0.1.0`; `CHANGELOG.md`; trusted publishing workflow for wheels and sdist; PY-O1 (GPU wheel) decided or the separate-wheel assumption accepted | todo |
| F6.5 | Supply chain and security | `infra/release` (same PR as F6.3) | `cargo audit` and `cargo deny` (licences, advisories) in CI; `SECURITY.md` contact verified; pinned CI actions; DCO sign-off on every commit checked in CI | todo |
| F6.6 | Release `v0.1.0` | `main` | tag, GitHub release with the wave 5 report and the bench figures (host named), wheels on PyPI for the four interpreters, `README.md` status changed from "Design"; E7 complete first | todo |

### F6.1 tasks
- [ ] Each of Q2, Q3, Q4, Q5, Q6, Q7, Q9, Q10 has a `decided <date>` row or an `assumption accepted <date>` row in `DECISIONS.md`, applied to the document that carries it in the same PR
- [ ] Every SDD status line reads HANDOFF-READY (flipped by Brackly)
- [ ] Preamble section 8 traceability: every parent id has invariants and tests; no "(deferred)" row except D11 (E8)

### F6.2 tasks
- [ ] A sufficiency table in the wave 5 report: S-id, test ids, host, figure, verdict
- [ ] S14 device and GDS halves and S13's device part recorded as skipped by id with acceptance
- [ ] Brackly signs the table

### F6.3 tasks
- [ ] Namespace checks run and recorded (Q1 remainder); names reserved where needed
- [ ] `CHANGELOG.md`; `Cargo.toml` and `pyproject.toml` versions; release workflow builds wheels for the matrix and publishes on a tag with trusted publishing
- [ ] PY-O1 decided; if the assumption stands, the `amoru-cuda` wheel is documented as not shipped in `0.1.0`

### F6.5 tasks
- [ ] `cargo audit` and `cargo deny check` jobs green; licence allow-list matches Apache-2.0 compatibility
- [ ] DCO check on pull requests; actions pinned by SHA

### F6.6 tasks
- [ ] E7 documentation epic complete and published
- [ ] `v0.1.0` tagged from a green `main`; release notes; wheels visible on PyPI; `pip install amoru` works on a clean 3.13 and 3.14 interpreter
- [ ] `README.md` status updated; this board's "Now" section says released

---

## E7. Documentation: intensive, end-of-build

Written after wave 5 so it describes what shipped, from the design documents and the run report, never reverse-engineered from code. Gate: every page below exists, builds in CI, cites its SDD or preamble section for every number and default, and a reader who has never seen the repository can install a wheel and complete each tutorial. Every executor here reads the SDDs its pages cover and the merged code as evidence, and reports a mismatch as a bug (`.github/ISSUE_TEMPLATE/bug.md`), never by documenting the code's behaviour over the document's.

| Id | Feature | Branch | Scope | Status |
|---|---|---|---|---|
| F7.1 | Docs site scaffold and CI | `docs/site` | `docs/` as an mdBook (Rust-native, no Node); a CI job that builds it, checks links and runs the em-dash and no-tabs conventions; published from `main` to GitHub Pages | todo |
| F7.2 | User guide | `docs/user-guide` | install from a wheel per interpreter; `amoru.run` walkthrough; sources (Parquet local and object store, safetensors, NumPy, Python iterator) and sinks (Parquet, tensor, Arrow IPC); ordering and error policies; budgets and the two environment-variable families; reading the run report and the trace; cancellation and resume (`resume=`, `checkpoint.*`) | todo |
| F7.3 | API reference | `docs/api` | Python: every public name in `python/amoru` from docstrings (the module's docstrings are the source); Rust: `cargo doc` for `amoru-kernel` with every trait's contract sentence from preamble 1.3 on its doc comment, published beside the book; the configuration table of preamble section 5 rendered with owner and range per row | todo |
| F7.4 | Kernel author guide | `docs/kernels` | a Rust kernel against `amoru-kernel` alone; the same kernel as a Polars plugin and a DataFusion function (S7); a Python kernel, GIL and free-threading, what releases the GIL; stateful kernels, instances, `ResumePolicy`; hints and `footprint`; the zero-copy rules and what breaks them (G-I2, CT-I4) | todo |
| F7.5 | Operator and hosting guide | `docs/hosting` | one page per host class of architecture section 6: laptop, cgroup v2 container and Kubernetes pod (memory.high, cpu.max, io_uring seccomp), Databricks single node (explicit budget, E7), Griot Cloud pod profile (`AMORU_HOST_PROFILE`, durable staging); staging disk sizing; profiles directory; diagnosing a `Budget` termination; what the direct paths need and how the report names the path taken (G-I7) | todo |
| F7.6 | Architecture and internals for contributors | `docs/internals` | the component map and waves from the preamble; how a morsel moves (tiers, the move table, staging segments, the manifest); how the controller decides (probe, envelope, AIMD, classification); how to read an SDD and where each crate's tests map to its ids; the escalation path and the agent prompts; benchmark methodology and how to reproduce a figure on any host | todo |
| F7.7 | Tutorials and examples | `docs/tutorials`, `examples/` | end-to-end runnable examples with generated data: score a Parquet table with a NumPy kernel; embed a text column to a tensor; kill and resume a run; run inside a container with a budget; each example is a CI smoke test | todo |
| F7.8 | Documentation gate and release notes (PM) | `main` | every page reviewed against its cited sections; bug issues filed for mismatches; `CHANGELOG.md` and release notes for `v0.1.0` drafted from the wave reports | todo |

### F7.1 tasks
- [ ] mdBook scaffold with the page tree of F7.2 to F7.7; CI job builds it, fails on broken links, em dashes and a page with no section citation
- [ ] Published from `main` (GitHub Pages or equivalent); `README.md` links it

### F7.2 tasks
- [ ] Every default and range cites its preamble section 5 row; every environment variable named with its owner
- [ ] Run report and trace pages show a real report from the wave 5 run with the host named

### F7.3 tasks
- [ ] Python docstrings complete for every public name (checked by a docstring-coverage step in the gate); Rust `cargo doc --no-deps` warning-free with `#![deny(missing_docs)]` on `amoru-kernel`
- [ ] Configuration table rendered from one source (the preamble) so it cannot drift

### F7.4 tasks
- [ ] The three-host kernel example (runtime, Polars, DataFusion) compiles and runs in CI (reuses AD-T9, AD-T10)
- [ ] Python kernel page shows the GIL-serialised report line and the free-threaded speedup figure with host

### F7.5 tasks
- [ ] Each host class page verified by running the container example there where the wave 5 environment allows; otherwise marked "described, not verified on this class" with the reason
- [ ] Termination diagnostics page reproduces the S6 adversarial run's diagnostic verbatim

### F7.6 tasks
- [ ] Diagrams are the preamble's and SDDs' Mermaid blocks, included, not redrawn
- [ ] Bench methodology page reproduces one S3 figure from the wave 5 report step by step

### F7.7 tasks
- [ ] Four examples runnable from a clean interpreter with a published wheel; each a CI smoke test with a small budget
- [ ] Resume example kills with SIGKILL and shows byte-equal output (S17)

### F7.8 tasks
- [ ] PM review of every page against its cited sections; mismatches filed as bugs, none documented around
- [ ] `CHANGELOG.md` and `v0.1.0` release notes drafted; F6.6 unblocked

---

## Escalations and findings

Open items the PM found while reading, routed per preamble section 7. Ids: `N-` a documentation finding from the PM's read, `E1-env-` an environment gap for a wave, `D-B` a decision about the board itself.

| Id | What | Where | Who decides | Status |
|---|---|---|---|---|
| N-1 | Preamble 6.2 listed `tracing` as used by "all", contradicting 01 section a and CT-T12 (`amoru-kernel` depends on `arrow`, `dlpark`, `thiserror`, `blake3` only) | preamble 6.2 | PM (documentation, no interface change) | fixed on `infra/pm-wave0-prep` |
| N-2 | CT-T11 asserts "under 1 ms" and is untagged; the gate rule labels a timing figure measured off the reference host provisional, and a hard bound in a unit test is flaky on CI | 01 k, CT-T11 | PM (under delegation, 2026-09-22) | fixed on `infra/pm-wave0-prep`: tagged, timing provisional off the reference host, structural half runs everywhere |
| N-3 | Contracts d.12 cites "(DS, `Config` error)" without a section id; an executor may only read cited sections | 01 d.12, 03 (the guarantee table near line 168) | PM (under delegation) | fixed: cites 03 e.4 |
| N-4 | CONTRIBUTING and the preamble say one pull request per component; a session-sized feature plan needs several sessions per large component (placement, scheduler, contracts) | this board | PM | decided, D-B1 |
| N-5 | AD-T9 and AD-T10 (integration, wave 1) need the bench `normalise` kernel from the same wave; the bench PR must merge before those two close | 05 k, preamble 6.5 | PM (sequencing) | F1.5 waits for F1.7; noted |
| N-6 | `origin/main` is at `a972737`; the seven commits carrying the SDDs, the preamble revisions and the agents' prompts are local only; executors branch from `main` on GitHub | repository | PM | pushed 2026-09-22 |
| E1-env-1 | No `docker` on this machine; the container gate and the MinIO job run in CI only; an executor cannot run them locally before pushing | wave 0 environment (6.6) | PM | accepted: CI is the gate host for the container and MinIO jobs; executors record the fact under "Environment facts verified" |
| E1-env-2 | `python3.13t` and `python3.14t` are absent locally (3.13.7 and 3.14.3 GIL builds present); needed to prove the matrix locally in wave 0 and for component 5 in wave 1 | wave 0 and 1 environment | PM | closed 2026-09-22: installed with `uv python install 3.13t 3.14t` |
| E1-env-3 | `maturin` absent locally (wave 5) | wave 5 environment | PM | `uv tool install maturin` when wave 5 starts |
| E1-ref-1 | The reference host is the Griot server in Nairobi; no access path for an agent is recorded | E1, wave 5 | Brackly | deferred to wave 5 (D-B3); the only open Brackly item |

### Decisions the board needs from Brackly

| Id | Decision | PM recommendation | Status |
|---|---|---|---|
| D-B1 | Several session-sized features per large component, all on the one component branch, one pull request reviewed against the SDD when the last feature lands (keeps "one PR per component") versus one PR per feature | the former | decided 2026-09-22 (PM, under delegation): features are sessions on one component branch; the PM reviews each feature's commits as they land; the SDD review is once, at the end |
| D-B2 | Coverage floor 90% line coverage per crate, test code excluded, judged per crate, stubs not measured (as written into preamble 6.7 on this branch) | accept | decided 2026-09-22 (PM, under delegation) |
| D-B3 | How an agent runs the E1-tagged suite on the reference host: SSH access for the executor session, or Brackly runs `bench/` by hand and pastes the output into the wave 5 report | SSH for a wave 5 executor, read-only elsewhere | open until wave 5; Brackly |

## Wave reports

Filed here as each gate closes (pm.md section 2); the detail stays in the pull requests.

- Wave 0: not yet run.

### Delegated decisions log

Every item the preamble routes to the human that the PM decided under the delegation of 2026-09-22, newest last. A row here is also mirrored to `DECISIONS.md` when it traces to a Q-item.

| Date | Item | Decision | Where applied |
|---|---|---|---|
| 2026-09-22 | N-2 CT-T11 timing test | tagged (reference host, E1) for the timing; structural half everywhere | 01 k |
| 2026-09-22 | N-3 contracts d.12 citation | cites 03 e.4 | 01 d.12 |
| 2026-09-22 | D-B1, D-B2 | as recommended | this board, preamble 6.7 |
| 2026-09-22 | E1-env-1 | CI is the gate host for docker-dependent jobs | this board |
