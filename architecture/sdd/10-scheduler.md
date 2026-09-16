# Amoru SDD 10: Scheduler (`amoru-scheduler`)

**Document type:** software design document, component 10 of 12
**Status:** DRAFT · 2026-09-15
**Parent:** `architecture/amoru-runtime-design.md` section 5.5; decisions D1, D6, D8; criteria S4, S6, S9; global invariants G-I4, G-I5, G-I9
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.5 (`Morsel`), d.6 (`Source`), d.7 (`Kernel`), d.8 (`Sink`), d.10 (`Placement`), d.11 (`Knobs`), d.13 (`TraceRecord`)
**Component location:** `crates/amoru-scheduler`, Rust
**Consumes:** contracts (1), placement (9); drives sources (7) and sinks (8) through the reactor. **Consumed by:** controller (11, through `Knobs` and stats), runtime facade (12)

**Decisions worth your eye:** (1) the source-drive loop and the sink-drive loop are two dedicated tasks on the reactor, not workers, so read-ahead and writes never consume a worker slot; (2) the admission rule is evaluated per worker pick with an atomic stage cursor and one short lock, not by a central dispatcher thread; (3) errors terminate by default, `skip` records and continues, and a `budget(n)` policy terminates after n failures.

---

## a. Purpose and boundary

The scheduler turns a linear chain of stages into work for a pool of role-free threads. It owns the workers, the stage table, the admission rule that decides which stage a free worker serves next, the stateful instance pools, the source-drive and sink-drive loops, completion detection, the error policy, and the emission of every trace record. It implements `Knobs` so the controller can move its parameters without knowing its internals.

It refuses to know: where morsel bytes are (placement); how big a morsel should be (it reads the target from its knob and passes it to the source); why a worker count was chosen (controller); what a kernel does.

## b. Vocabulary

**Task.** One unit a worker executes: `Apply { stage, morsel }`. Source reads and sink writes are not tasks; they are driven by the two reactor loops.

**Stage table.** Per kernel stage: the kernel, its instance pool, its input queue id (stage−1) and output queue id (stage), and its stats.

**Active set.** Workers permitted to take tasks; the rest are parked (`workers.active ≤ workers.max`).

**Admissible stage.** A stage with a resident head in its input queue and whose output queue is not full.

**Instance pool.** For a stateful kernel, up to `max_instances` `KernelState`s with worker affinity.

**Source drive.** The reactor task that issues `Source::read` calls up to `read_ahead` in flight and pushes results to Q0.

**Sink drive.** The reactor task that pops from the last queue and calls `Sink::write` up to `sink.concurrency` in flight.

**Completion.** The condition: source exhausted, every queue closed and empty, no task in flight, no sink write in flight.

## c. Invariants

**SC-I1. Workers run only `apply`.** A worker thread never calls the reactor, never waits on a `Completion`, never allocates payload memory; it pops, applies, pushes, records. Upholds preamble 4.1 and G-I9.

**SC-I2. Admission prefers the emptiest output.** Among admissible stages, a worker takes the one whose output queue holds the fewest bytes; ties break toward the later stage (closer to the sink). The rule is evaluated at every pick.

**SC-I3. Source work is admitted only when Q0 is below high water and read-ahead has room.** The source drive issues a read iff `!placement.is_full(0)` and `in_flight_reads < read_ahead`.

**SC-I4. Every morsel yields exactly one trace record per stage.** Emitted by the worker after `apply` returns or fails, and by the source drive for the probe record; never twice, never skipped. Upholds G-I4.

**SC-I5. Knobs have one writer and take effect within one pick.** `Knobs::set` stores atomically; the next worker pick and the next drive iteration observe the new value. Upholds G-I5.

**SC-I6. A stateful instance is used by one worker at a time and affinity is preferred.** An instance is acquired under the pool lock; a worker re-acquires the instance it last used when free; instances are created lazily up to `max_instances` on the first workers to need them.

**SC-I7. Parking is lossless.** Lowering `active_workers` parks workers only between tasks; a parked worker holds no morsel and no instance.

**SC-I8. Errors follow the policy and never panic the pool.** A kernel error or panic is caught on the worker; under `terminate`, the run enters termination with the diagnostic; under `skip`, a trace record with `Outcome::Error` is written, the morsel is dropped (and the reorder buffer told to skip its seq); under `budget(n)`, `skip` until the nth error, then `terminate`. Upholds G-I8.

**SC-I9. Completion is exact.** `run` returns only when the completion condition holds or the run terminated or was cancelled; no morsel is left in any queue on normal completion.

## d. Interfaces

### d.1 Exposed

```rust
pub struct Pipeline {
    pub source: Arc<dyn Source>,
    pub kernels: Vec<Arc<dyn Kernel>>,            // stages 1..=n
    pub sink: SinkHandle,                         // Plain(Box<dyn Sink>) | Ordered(ReorderBuffer<Box<dyn Sink>>)
}

pub struct SchedulerConfig {
    pub workers_max: u16,
    pub workers_active: u16,
    pub read_ahead: u16,
    pub sink_concurrency: u16,
    pub error_policy: ErrorPolicy,                // Terminate | Skip | Budget(u32)
    pub initial_morsel_target: u64,               // per stage, before the controller sets knobs (morsel.probe_bytes for stage 0's first read)
    pub checkpoint_enabled: bool,                 // checkpoint.enabled and the placement engine has a staging directory
    pub checkpoint_interval_ms: u64,              // checkpoint.interval_ms
    pub node: NodeId,                             // LOCAL_NODE in v1; passed to Origin
}

pub struct Scheduler { /* private */ }
impl Scheduler {
    pub fn new(cfg: SchedulerConfig, pipeline: Pipeline, placement: Arc<dyn Placement>, reactor: Arc<Reactor>, alloc: Arc<dyn Allocator>, trace: Arc<dyn TraceSink>, sampler_hook: Arc<dyn Fn() -> Sample + Send + Sync>) -> Result<Scheduler>;
    /// Runs the pipeline to completion, termination or cancellation. Blocks the caller (the facade's main thread).
    pub fn run(&self, cancel: CancelToken) -> Result<RunOutcome>;
    /// Continue a run from a `ResumePoint` the placement engine produced (f.13). Called by the
    /// facade instead of `run`, after `Placement::restore` and before any probe; it sets the
    /// source cursor and sequence counter, resumes the sink, restores or re-inits instances,
    /// re-reads the recompute list, then behaves as `run`.
    pub fn resume(&self, point: ResumePoint, cancel: CancelToken) -> Result<RunOutcome>;
    /// Write a manifest now (f.12). Called by the checkpoint tick and by the facade on termination.
    pub fn checkpoint(&self) -> Result<()>;
    pub fn stats(&self) -> SchedulerStats;
    /// Called by the controller to run the probe protocol for stage `stage` (see 11); the scheduler executes it with one worker.
    pub fn probe(&self, stage: StageId, morsel: Morsel) -> Result<ProbeResult>;
}
impl Knobs for Scheduler { /* contracts d.11 */ }

pub enum RunOutcome {
    Completed { sink: SinkSummary },
    /// `manifest` is the path of the last manifest written, when checkpointing was on, so the
    /// surface can tell the user the run is resumable.
    Terminated { diagnostic: AmoruError, manifest: Option<std::path::PathBuf> },
    Cancelled { manifest: Option<std::path::PathBuf> },
}

#[derive(Clone, Debug, Default)]
pub struct SchedulerStats {
    pub per_stage: Vec<StageStats>,               // tasks, busy_ns, errors, skipped, instances_live
    pub workers_active: u16, pub workers_busy: u16,
    pub reads_in_flight: u16, pub writes_in_flight: u16,
    pub source_exhausted: bool, pub seq_issued: Seq,
    pub committed_seq: Option<Seq>, pub checkpoints: u64, pub last_checkpoint_us: u64, pub resumed: bool, pub recomputed: u64,
}
pub struct ProbeResult { pub peak_delta: u64, pub dev_peak_delta: u64, pub wall_ns: u64, pub output: Morsel }
```

`CancelToken` is a clonable flag set by the surface on `KeyboardInterrupt`/SIGINT.

### d.2 Consumed

Contracts as listed, plus `Locality`, `NodeId`, `LOCAL_NODE`, `ResumePolicy`, `CheckpointExtras`, `SourceCursor`, `ResumePoint`; `amoru_placement::PlacementEngine` (`evicted`, `replace`, `peek_resident`, and the contract's `set_committed`, `checkpoint`, `restore`); `amoru_sinks::ReorderBuffer` (`is_stalled`, `skip`, `next_expected`); `amoru_reactor::Reactor` (to spawn the two drive tasks and the checkpoint tick); `crossbeam` (worker parking); `std::thread`.

## e. Data model, formats and state machines

### e.1 Worker state machine

`Parked` → (unpark, active slot available) → `Idle` → (pick) → `Running(stage, seq)` → `Idle` → (active lowered) → `Parked`. Cancellation moves `Idle` and `Parked` workers to `Exited` and lets `Running` finish first.

### e.2 Run state machine

`Init` → (probes done, or `resume` applied) → `Running` → `Draining` (source exhausted, closing queues in order as each empties) → `Finishing` (sink `finish`) → `Completed`; from `Running` or `Draining`, `Terminating` (error under policy) → `Terminated`; `Cancelling` → `Cancelled`. In `Terminating` and `Cancelling`: source drive stops issuing; workers finish current tasks; placement `shutdown`; in-flight sink writes complete; trace `finish`.

### e.3 Stage table

```rust
struct StageEntry { stage: StageId, kernel: Arc<dyn Kernel>, spec: PayloadSpec, pool: Option<InstancePool>, out_bytes: AtomicU64 /* mirrored from placement stats each pick */, tasks: AtomicU64, busy_ns: AtomicU64, errors: AtomicU32 }
struct InstancePool { states: Mutex<Vec<Slot>>, max: usize }   // Slot { state: Box<dyn KernelState>, owner: Option<u16 /* worker */>, in_use: bool }
```

## f. Algorithms and policies

**f.1 Startup.** Validate the chain: `PayloadSpec::check` of each kernel against the upstream schema (contracts CT-I5); the sink's `accepts` against the last kernel's `output_schema`; `set_consumer` on every queue; spawn `workers_max` threads parked (the sink is opened by `run`, or resumed by `resume`, not here, so that a resumed run never opens it twice); spawn the source drive and sink drive on the reactor; run the probe protocol per stage in order when the controller asks (the facade sequences: controller → `probe` → controller sets knobs → `run`).

**f.2 Worker loop.**

```
loop:
  if !active_slot(): park(); continue
  stage = pick()                                     // f.3; None → park 1 ms, continue
  morsel = placement.pop(stage-1, spec[stage], Locality::Any)   // resident by construction of pick; if None (raced), continue
  (state, slot) = acquire_instance(stage)            // f.4; None for stateless → NoState
  t0, s0 = now(), sampler_hook()
  result = catch_unwind(|| kernel.apply(state, morsel.payload))
  t1, s1 = now(), sampler_hook()
  release_instance(slot)
  match result:
    Ok(out)  → out_morsel = morsel.with_output(out); placement.push(stage, out_morsel); record(Ok)
    Err(e)   → apply_policy(e); record(Error)
  loop
```

`record` builds the `TraceRecord` (contracts d.13) from `morsel` (features, bytes_in), `out` (bytes_out, rows_out, tier_out), timings, `s0`/`s1` (mem before, peak, throttled delta, device), the knob snapshot, and the miss wait the placement engine left in the thread-local (PL f.4).

**f.3 `pick`.** Snapshot `placement.stats()` output bytes per queue (cached per 1 ms to avoid contention; the cache is refreshed by whichever worker finds it stale); for stages 1..=n: admissible iff the input queue's head is resident (`placement.pop` would succeed; checked with a non-consuming `peek_resident(stage-1, spec)` the placement engine exposes as an inherent method) and `!placement.is_full(stage)` and (stateless or an instance is free or creatable); choose the admissible stage with the smallest output bytes, ties to the higher stage; return it. Cost: O(stages) per pick with no lock except the peek.

**f.4 Instances.** `acquire_instance(stage)`: lock pool; prefer a free slot whose `owner == this worker`; else any free slot; else if `states.len() < max`, create by `kernel.init(InitCtx { instance: len, device: assigned per instance round-robin over devices when `uses_device_memory`, alloc })`; else return `None` and the worker re-picks (the stage was admissible only if a slot was free or creatable, so this is a race, not a policy). Instances are retired (dropped and re-created on next acquire) after a kernel error (adapters AD-I7 rationale).

**f.5 Source drive.** On the reactor: `plan` once; iterate splits; for each, while `in_flight < read_ahead` and `!placement.is_full(0)` and not stalled by an ordered sink (`sink.is_stalled()`): compute the row range from `morsel_target[1]` (the first kernel's target) and the split's bytes per row (`uncompressed_bytes / rows`), clamp to `[morsel.min_bytes, morsel.max_bytes]`, sub-split when `sub_splittable`; issue `source.read(split, range, alloc, tier0)` where `tier0` is `PinnedHost` if the arena is pinned else `Host`; on completion, `placement.push(0, Morsel::new(seq++, 0, payload, origin))` with `origin.node = cfg.node`. The drive keeps its position as a `SourceCursor { split_index, row_offset, next_seq }` (the next range to issue, not the last completed one) and exposes it for f.12; sequence numbers are assigned at issue in cursor order, so the cursor and the sequence counter always agree. Also service `placement.evicted(0)` by re-issuing reads for evicted entries and calling `replace`. When the plan is exhausted and no read is in flight, `placement.close(0)` and set `source_exhausted`.

**f.6 Sink drive.** On the reactor: loop `placement.pop_blocking(n, sink_spec, Locality::Any)`; on `Some(m)`: `sink.write(m.seq, m.payload)` (the reorder buffer, when present, is the sink and orders internally); keep up to `sink_concurrency` writes in flight; after each write completes, read `sink.committed_seq()` and, when it moved, call `placement.set_committed(w)` (f.11); record a trace record for stage n+1? No: the sink is not a stage; its throughput is visible through the last queue's drain rate and `SinkStats`. On `None` (closed and empty): `finish` the sink, signal completion.

**f.7 Draining and closing.** When the source is exhausted and Q0 is empty and no stage-1 task is running, `close(1)`; and so on down the chain; each queue is closed when its producer stage has no task running and its input is closed and empty. This is evaluated by whichever worker or drive observes the condition (a monotonic check, safe to evaluate concurrently).

**f.8 Error policy.** `apply_policy(e)`: `Terminate` → set run state `Terminating` with `e` as the diagnostic (enriched with the morsel's seq, stage, features and the `s1` sample); `Skip` → drop the morsel and call `sink.skip(seq)` (the contract's method; a `ReorderBuffer` advances past the sequence and forwards it, a file sink counts it as committed for its watermark, SI f.7), so a skipped morsel never holds the commit watermark back; on resume a skipped sequence above the watermark is replayed like any other and may be skipped again; `Budget(n)` → `Skip` until `errors_total == n`, then `Terminate`. A kernel panic is converted to `AmoruError::Kernel { msg: "panic: ..." }` first.

**f.9 Probe protocol (called by the controller).** `probe(stage, morsel)`: park all workers but one; on that worker run f.2's body once for `stage` with the given morsel, capturing `s0`, `s1` and device samples; return `ProbeResult` with the output morsel pushed to the next queue as normal (nothing is wasted) and a trace record with `Outcome::Probe`; unpark.

**f.10 Cancellation.** `cancel` observed by drives and workers between tasks; state `Cancelling`; f.7 is skipped; placement `shutdown`; sink writes in flight complete; a final manifest is written (f.12) when checkpointing is on; sink `finish` is not called; trace `finish`; return `Cancelled { manifest }`.

**f.11 Commit watermark.** The sink drive owns the watermark `w`: after each completed write and after each `sink.skip`, `w' = sink.committed_seq()` (the sink already counts skipped sequences, SI f.7); when `w' > w`, `placement.set_committed(w')` and `stats.committed_seq = w'`. With a sink whose `committed_seq` is always `None` (not resumable), `w` never moves, the lineage index grows for the run, and `checkpoint_enabled` is forced false at startup with a `warn` naming the sink; the run is correct, only not resumable. At `Finishing`, after `sink.finish()`, `set_committed(next_seq − 1)`.

**f.12 Checkpoint tick.** When `checkpoint_enabled`: a dedicated checkpoint thread (preamble 4.1; not a reactor task, because `KernelState::checkpoint` may run kernel code, and not a worker) wakes every `checkpoint_interval_ms`; the facade also calls `checkpoint()` on termination and cancellation, from the main thread. `checkpoint()`: gather `CheckpointExtras { kernel_states, sink_state, committed_seq: w, source_cursor }` where `kernel_states` comes from calling `KernelState::checkpoint` on every instance of every `ResumePolicy::Checkpoint` stage (each instance is acquired for the call like a task, so no `apply` runs concurrently on it; SC-I6), `sink_state` from `sink.checkpoint()`, and the cursor from the source drive; then `placement.checkpoint(&extras)`. A `Checkpoint` kernel whose `checkpoint` returns `Ok(None)` is a kernel bug: the run terminates with `Resume` naming the stage. Failures are counted and logged; three consecutive terminate the run (placement h). The thread never holds the stage table lock across the placement call.

**f.13 Resume.** `resume(point, cancel)`, in `Init`, in this order: `sink.resume(schema, sink_state, committed_seq)` instead of `open` (a sink returning `Resume` ends the attempt with that error naming the sink; nothing has been written); then refuse if `!checkpoint_enabled` (a resumed run must be able to write its own manifests); set the source drive's cursor and sequence counter from `point.extras.source_cursor`; set `w = committed_seq`; for each `Checkpoint` stage, build instances from `kernel_states` through `kernel.restore(InitCtx, bytes)` in instance order, and for `Reinit` stages let f.4 create instances lazily as usual (`Forbid` was refused by `Placement::restore`); then, before the source drive starts issuing new reads, re-read every `(seq, origin)` in `point.to_recompute` in order through the normal source path (`source.read(split_of(origin), Some(origin.row_start..origin.row_end), alloc, tier0)`) and push each to Q0 with its original `seq` (`Morsel::new(seq, 0, payload, origin)`), counting `stats.recomputed`; the re-reads obey `read_ahead` and `is_full(0)` like any other read, so a large recompute list does not blow the budget; then continue as `run` from `Running` (the controller's `probe_missing`, RC f.14, ran before `resume` was called, in the facade's sequence, PY-I1). Sequence numbers above the cursor's `next_seq` are never reused, and the ones below it that are not in the lineage are, by construction, committed or skipped. Output equals an uninterrupted run (PL-T17).

## g. Concurrency within the component

Lock order: the stage table lock (position 1) is taken only during startup and stage close; instance pool mutexes (part of position 1 by convention); the placement queue lock (position 2) is taken inside `pop`/`push` and never held by the scheduler across `apply`. The stats cache is atomics plus a timestamp. Workers park on per-worker `crossbeam::Parker`s; `Knobs::set(ActiveWorkers)` unparks or lets workers self-park at the next loop head. The two drive tasks are tokio tasks and hold no locks across awaits.

## h. Behaviour

**Normal path.** Startup validates and opens; probes run per stage; the controller sets initial knobs; `run`: the source drive fills Q0 to high water, workers pick stage 1, outputs fill Q1, workers spread across stages by the emptiest-output rule, the sink drive drains Qn; source exhausts; queues close in order; sink finishes; `Completed`.

**Edge cases.** Zero splits: Q0 closes immediately; queues close in order; sink `finish` writes an empty output; `Completed`. One stage, stateless, identity kernel: workers alternate stage 1; the emptiest-output rule is trivial. All workers parked (`active = 0`, controller bug): guarded by the knob's range (min 1). A kernel that returns a payload in a tier the next consumer cannot accept: placement handles it (promotion); the scheduler does nothing. `read_ahead = 0`: the source drive issues exactly one read at a time only when Q0 is empty (a floor of one, documented).

**Failures.** Source read failure: `Terminating` with the split named (there is no skip for source errors). Sink write failure: `Terminating` with the sink error; in-flight writes complete or fail; `finish` not called; the sink's own cleanup runs on drop. Placement `pop` returning `Err(Staging)` for a head that cannot be produced: `Terminating` with the placement diagnostic. Worker thread panic outside `apply` (a scheduler bug): the pool detects the dead thread by a heartbeat and terminates the run with an internal error rather than hanging.

## i. Configuration

`workers.max`, `workers.active`, `readahead.splits`, `sink.concurrency`, `errors.policy`, `ordering.required`, `morsel.min_bytes`, `morsel.max_bytes` (as clamps in f.5), `checkpoint.enabled`, `checkpoint.interval_ms`.

## j. Observability

`SchedulerStats`; `TraceRecord` emission (the trace is the scheduler's primary output); `tracing`: `sched.start` (info: workers, stages), `sched.probe` (info), `sched.close_stage` (debug), `sched.policy` (warn on skip, error on terminate), `sched.cancel` (info), `sched.checkpoint` (debug: committed_seq, cursor, duration_us), `sched.resume` (info: committed_seq, to_recompute, cursor), `sched.not_resumable` (warn: the sink).

## k. Tests

Tests use `FakePlacement`, `FakeSource`, `FakeSink`, `FakeKernel`, `FakeTrace`.

**SC-T1 workers_only_apply.** Thread-id instrumentation: no worker thread ever executes reactor code or `Completion::wait`. SC-I1.

**SC-T2 admission_rule.** Three stages with controlled output-queue sizes; 1,000 picks; each pick chose the emptiest-output admissible stage with the specified tie-break. SC-I2.

**SC-T3 source_admission.** Q0 full → no reads issued; `read_ahead` respected under a slow fake source. SC-I3.

**SC-T4 one_record_per_morsel_stage.** 10,000 morsels × 3 stages → exactly 30,000 records with unique `(stage, seq)`, plus probe records. SC-I4, G-I4.

**SC-T5 knobs_immediate.** Set `ActiveWorkers(2)` on a 16-worker pool; within one task duration, at most 2 workers are busy; raise to 16, all busy. SC-I5, SC-I7.

**SC-T6 instance_affinity.** Stateful kernel with 4 instances on 8 workers; no instance used concurrently; workers reacquire their previous instance ≥ 90% of the time under steady load. SC-I6.

**SC-T7 error_policies.** For `Terminate`, `Skip`, `Budget(3)`: the fake kernel fails on morsels 5, 9, 12, 20; outcomes match; the reorder buffer's `skip` is called under `Skip`; a panicking kernel produces a `Kernel` error, not a pool crash. SC-I8.

**SC-T8 completion_exact.** Random pipeline sizes and speeds; `run` returns `Completed` with every queue empty and the sink's row count equal to the source's (identity). SC-I9.

**SC-T9 draining_order.** Queues close in stage order; a stage never closes while its producer has a task running. f.7.

**SC-T10 probe_protocol.** `probe` runs with exactly one worker active, returns a `ProbeResult`, and the output morsel appears in the next queue. f.9.

**SC-T11 cancel.** Cancel mid-run; returns `Cancelled` within the longest kernel duration + 1 s; trace finished; sink `finish` not called. f.10.

**SC-T12 utilisation.** (real components, reference host) Compute-bound fake kernel of 50 ms per morsel on 8 workers; busy fraction ≥ 85% over 60 s. S4.

**SC-T13 evicted_replay.** With Q0 staging off and pressure, evicted entries are re-read and replaced; output equals the no-pressure run. PL-I6 interplay.

**SC-T14 watermark_and_skips.** Fake sink that commits every 10 sequences and implements `skip`; `Skip` policy with failures on sequences 7, 23, 24; `sink.skip` is called for each; `set_committed` is called with exactly the model's watermark (skips do not hold it back) and never with a lower value than before. f.11, f.8.

**SC-T15 checkpoint_tick.** With `checkpoint_interval_ms = 50` and a `Checkpoint` kernel: `KernelState::checkpoint` is called on every instance at each tick, on the checkpoint thread (thread-id assertion: never a worker, never a reactor thread), with no concurrent `apply` on that instance (asserted inside the fake kernel); `Placement::checkpoint` receives the cursor that equals the next range the source drive issues; a `Checkpoint` kernel returning `Ok(None)` terminates with `Resume`. f.12.

**SC-T16 resume_equivalence.** Run a 3-stage pipeline with `FakeSource` (counting reads) to a fake resumable sink; kill the scheduler (drop it) after a random number of commits; build a new scheduler over a `Placement::restore`d engine and call `resume`; the sink's final row set equals an uninterrupted run; `Source::read` was called once per recomputed origin and once per new range, never for a committed sequence; a `Reinit` kernel saw `init` again, a `Checkpoint` kernel saw `restore` with the bytes it checkpointed, and a non-resumable sink made `resume` return `Resume` before any write. f.13, S17.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/pipeline.rs` (validation, f.1), `src/worker.rs` (e.1, f.2), `src/pick.rs` (f.3), `src/instances.rs` (f.4, and the restore path of f.13), `src/source_drive.rs` (f.5, the cursor, the recompute pass of f.13), `src/sink_drive.rs` (f.6, f.11), `src/lifecycle.rs` (e.2, f.7, f.10, `resume`), `src/policy.rs` (f.8), `src/checkpoint.rs` (f.12, the checkpoint thread), `src/probe.rs` (f.9), `src/knobs.rs` (atomics), `src/stats.rs`. No `unsafe`. `catch_unwind` around `apply` requires kernels to be `UnwindSafe`; wrap with `AssertUnwindSafe` and document that a panicking kernel's state is retired.

`peek_resident` is an inherent method on `PlacementEngine` (`09-placement.md` d.1); `FakePlacement` implements it too.

Anti-patterns: no central dispatcher thread; no worker holding a placement lock across `apply`; no trace record built anywhere but `worker.rs` and `probe.rs`; no `unwrap` on `pop` results.

## m. Open items

None. (`PlacementEngine::peek_resident` is in `09-placement.md` d.1.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| D1, D6 | SC-I1, SC-I2, SC-I6 | SC-T1, SC-T2, SC-T6 |
| S4 | f.2, f.3 | SC-T12 |
| S6, G-I8 | SC-I8, f.8 | SC-T7 |
| S9, G-I4 | SC-I4 | SC-T4 |
| S17, D13 | f.11, f.12, f.13 | SC-T14, SC-T15, SC-T16 |
| S16, D12 | `Locality::Any` at every pop, `Origin.node` | (compile-time; CT-T14 lint) |
| G-I5 | SC-I5 | SC-T5 |
| G-I9 | SC-I1 | SC-T1 |
| D8 | f.1 (linear chain) | SC-T8 |
| preamble 4.3 | f.10 | SC-T11 |
