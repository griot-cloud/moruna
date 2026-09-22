# Amoru SDD 10: Scheduler (`amoru-scheduler`)

**Document type:** software design document, component 10 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/amoru-runtime-design.md` section 5.5; decisions D1, D6, D8; criteria S4, S6, S9; global invariants G-I4, G-I5, G-I9
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.5 (`Morsel`), d.6 (`Source`), d.7 (`Kernel`), d.8 (`Sink`), d.10 (`Placement`), d.11 (`Knobs`, `StatsSource`, `Prober`, `SchedulerStats`, `StageStats`, `ErrorPolicy`, `CancelToken`, `RecordHook`), d.12 (`Sampler`), d.13 (`TraceRecord`, `TraceSink`)
**Component location:** `crates/amoru-scheduler`, Rust
**Consumes:** contracts (1), sinks (8, `SinkHandle`, `ReorderBuffer`); placement, sources, sinks, the sampler and the trace arrive as trait objects. **Consumed by:** controller (11, through `Knobs`, `StatsSource` and `Prober`), runtime facade (12)

**Decisions worth your eye:** (1) the source-drive loop and the sink-drive loop are two dedicated threads owned by the scheduler, not workers and not reactor threads, so read-ahead and writes never consume a worker slot and the only blocking waits on reactor completions in the process are theirs (contracts CT-I7); (2) the admission rule is evaluated per worker pick with an atomic stage cursor and one short lock, not by a central dispatcher thread; (3) errors terminate by default, `skip` records and continues, and a `budget(n)` policy terminates on the n-th failure; (4) every stateful instance exists before the first morsel is read (`init_instances`), so a failing `init` costs nothing and the baseline the controller samples already contains every loaded model.

---

## a. Purpose and boundary

The scheduler turns a linear chain of stages into work for a pool of role-free threads. It owns the workers, the stage table, the admission rule that decides which stage a free worker serves next, the stateful instance pools, the source-drive and sink-drive loops, completion detection, the error policy, the checkpoint thread, the worker heartbeat, and the emission of every trace record. It implements `Knobs`, `StatsSource` and `Prober` (contracts d.11) so the controller can move its parameters, read its counters and run the probe protocol without knowing its internals.

It refuses to know: where morsel bytes are (placement); how big a morsel should be (it reads the target from its knob and passes it to the source); why a worker count was chosen (controller); what a kernel does.

## b. Vocabulary

**Task.** One unit a worker executes: `Apply { stage, morsel }`. Source reads and sink writes are not tasks; they are driven by the two drive threads.

**Stage table.** Per kernel stage: the kernel, its instance pool, its input queue id (stage−1) and output queue id (stage), and its stats.

**Active set.** Workers permitted to take tasks; the rest are parked (`workers.active ≤ workers.max`).

**Admissible stage.** A stage with a resident head in its input queue and whose output queue is not full.

**Instance pool.** For a stateful kernel, exactly `max_instances` `KernelState`s, all created by `init_instances` before the run starts, with worker affinity. A stateless stage has no pool and no instances.

**Source drive.** The thread that issues `Source::read` calls up to `read_ahead` in flight, waits on their completions, and pushes results to Q0.

**Sink drive.** The thread that pops from the last queue and calls `Sink::write` up to `sink.concurrency` in flight.

**Drive-side helper.** A request the source drive services on behalf of another thread (the probe's stage-1 read, f.9), so that the requesting thread never waits on a reactor completion itself.

**Heartbeat.** A per-worker timestamp each worker refreshes at every loop head and after every `apply`; a worker that is neither running `apply` nor refreshing for `heartbeat_interval_ms` is dead (f.14).

**Completion.** The condition: source exhausted, every queue closed and empty, no task in flight, no sink write in flight.

## c. Invariants

**SC-I1. Workers run only `apply`.** A worker thread never calls the reactor, never waits on a `Completion`, never allocates payload memory; it pops, applies, pushes, records. Upholds preamble 4.1 and G-I9.

**SC-I2. Admission prefers the emptiest output.** Among admissible stages, a worker takes the one whose output queue holds the fewest bytes; ties break toward the later stage (closer to the sink). The rule is evaluated at every pick.

**SC-I3. Source work is admitted only when Q0 is below high water and read-ahead has room.** The source drive issues a read iff `!placement.is_full(0)` and `in_flight_reads < read_ahead`.

**SC-I4. Every morsel yields exactly one trace record per stage.** Emitted by the worker after `apply` returns or fails, and by the probing worker for the probe record (f.9, `probe.rs`); never twice, never skipped. Upholds G-I4.

**SC-I5. Knobs have one writer and take effect within one pick.** `Knobs::set` stores atomically; the next worker pick and the next drive iteration observe the new value. Upholds G-I5.

**SC-I6. A stateful instance is used by one worker at a time and affinity is preferred.** An instance is acquired under the pool lock; a worker re-acquires the instance it last used when free; every instance is created by `init_instances` before the run and the pool is full from the start (f.4).

**SC-I7. Parking is lossless.** Lowering `active_workers` parks workers only between tasks; a parked worker holds no morsel and no instance.

**SC-I8. Errors follow the policy and never panic the pool.** A kernel error or panic is caught on the worker; under `Terminate`, the run enters termination with the diagnostic; under `Skip`, a trace record with `Outcome::Error` is written, the morsel is dropped (and the sink told to skip its seq); under `Budget(n)`, errors 1 to n−1 are skipped and the n-th terminates. Upholds G-I8.

**SC-I9. Completion is exact.** `run` and `run_resumed` return only when the completion condition holds or the run terminated or was cancelled; no morsel is left in any queue on normal completion; every run that entered `Running` returns `Ok(RunOutcome)`.

**SC-I10. Knobs are clamped here and forwarded once.** A knob value outside the preamble's range is clamped to the nearest bound and counted in `SchedulerStats::knob_clamps`; `StagingTrigger`, `HighWater` and `PromotionWindow` are forwarded to the placement engine by the scheduler and by nobody else (contracts d.11).

**SC-I11. A dead worker is noticed within two heartbeat intervals.** The heartbeat table is checked at least once per `heartbeat_interval_ms` for the whole run; a worker found dead terminates the run with an internal diagnostic naming the worker and its last task, never a hang. Upholds G-I8.

## d. Interfaces

### d.1 Exposed

```rust
pub struct Pipeline {
    pub source: Arc<dyn Source>,
    pub kernels: Vec<Arc<dyn Kernel>>,            // stages 1..=n; may be empty (h, zero kernels)
    pub sink: amoru_sinks::SinkHandle,            // Plain or Ordered (08 d.1); the scheduler never names the inner type
}

pub struct SchedulerConfig {
    pub workers_max: u16,
    pub workers_active: u16,
    pub read_ahead: u16,
    pub sink_concurrency: u16,
    pub error_policy: ErrorPolicy,                // contracts d.11: Terminate | Skip | Budget(u32)
    pub initial_morsel_target: u64,               // per stage, before the controller sets knobs (morsel.probe_bytes)
    pub morsel_min: u64, pub morsel_max: u64,     // clamps for MorselTarget and for the source drive's row range (f.5)
    pub checkpoint_enabled: bool,                 // checkpoint.enabled and the placement engine has a staging directory
    pub checkpoint_interval_ms: u64,              // checkpoint.interval_ms
    pub heartbeat_interval_ms: u64,               // 1000; f.14
    pub resuming: bool,                           // true when the facade will call apply_resume_point; `new` then skips `open`
    pub node: NodeId,                             // LOCAL_NODE in v1; passed to Origin
}

pub struct Scheduler { /* private */ }
impl Scheduler {
    /// Validates the chain and the plan, opens the sink on a fresh run, spawns the workers parked
    /// (f.1). The drive threads exist but drive nothing until `run` (the source drive only services
    /// probe requests, f.9); the checkpoint thread is not started here.
    pub fn new(cfg: SchedulerConfig, pipeline: Pipeline, placement: Arc<dyn Placement>, alloc: Arc<dyn Allocator>, trace: Arc<dyn TraceSink>, sampler: Arc<dyn Sampler>) -> Result<Scheduler>;
    /// Eagerly runs `Kernel::init` for every instance up to `max_instances` of every stateful stage,
    /// on the worker that will own the instance (f.4). The facade calls it before the controller's
    /// `prepare`, so the baseline it samples includes every loaded model (preamble section 2).
    pub fn init_instances(&self) -> Result<()>;
    /// Resume, step one (f.13): refuses when `!checkpoint_enabled`, resumes the sink instead of opening
    /// it, sets the source cursor and sequence counter, restores `Checkpoint` instances and re-inits
    /// `Reinit` ones, sets the commit watermark. Called by the facade before the controller's
    /// `probe_missing`. Nothing is read or written.
    pub fn apply_resume_point(&self, point: ResumePoint) -> Result<()>;
    /// Starts the drives and the checkpoint thread, enters `Running`, runs to completion, termination
    /// or cancellation. Blocks the caller (the facade's runtime thread). `Ok(RunOutcome)` for every run
    /// that entered `Running`; `Err` only for a failure before that (wrong state, a thread that could
    /// not be spawned).
    pub fn run(&self, cancel: CancelToken) -> Result<RunOutcome>;
    /// Resume, step two (f.13): re-reads `to_recompute` through the source path, then continues as `run`.
    pub fn run_resumed(&self, cancel: CancelToken) -> Result<RunOutcome>;
    /// Installed by the facade after the controller exists (contracts d.11 `RecordHook`); called on the
    /// recording thread after every `TraceSink::record`.
    pub fn set_record_hook(&self, hook: RecordHook);
    /// Sets cancel, unparks and joins the workers, stops the drives and the checkpoint thread (f.10).
    /// Idempotent; also called from `Drop`.
    pub fn shutdown(&self);
}
impl Knobs for Scheduler { /* contracts d.11: set (f.15), snapshot, terminate (f.8) */ }
impl StatsSource for Scheduler { /* contracts d.11: scheduler_stats */ }
impl Prober for Scheduler { /* contracts d.11: probe (f.9) */ }
impl Drop for Scheduler { /* shutdown() */ }

pub enum RunOutcome {
    Completed { sink: SinkSummary },
    /// `manifest` is the path of the last manifest written, when checkpointing was on, so the
    /// surface can tell the user the run is resumable. The facade maps this variant to an error
    /// with the partial report attached (12 f.1).
    Terminated { diagnostic: AmoruError, manifest: Option<std::path::PathBuf> },
    Cancelled { manifest: Option<std::path::PathBuf> },
}
```

`SchedulerStats`, `StageStats`, `ErrorPolicy`, `CancelToken`, `RecordHook`, `ProbeResult` and `SizerKind` are the contracts' types (d.11); this crate defines none of them. `CancelToken` is set by the surface on `KeyboardInterrupt`/SIGINT (12 f.5).

### d.2 Consumed

Contracts as listed, plus `Locality`, `NodeId`, `LOCAL_NODE`, `ResumePolicy`, `CheckpointExtras`, `SourceCursor`, `ResumePoint`, `Sample`, `Completion` (the drives `wait` on it); `Placement` through the trait only (`push`, `pop_blocking`, `peek_resident`, `evicted`, `replace`, `is_full`, `close`, `set_consumer`, `set_staging`, `set_water`, `set_promotion_window`, `set_committed`, `checkpoint`, `shutdown`, `stats`); `amoru_sinks::{SinkHandle, ReorderBuffer}` (`is_stalled`, `next_expected`; the contract's `skip`); `crossbeam` (worker parking, channels between drives and workers); `std::thread`; `libc::clock_gettime` for per-thread CPU time (l).

## e. Data model, formats and state machines

### e.1 Worker state machine

`Parked` → (unpark, active slot available) → `Idle` → (pick) → `Running(stage, seq)` → `Idle` → (active lowered) → `Parked`. Cancellation moves `Idle` and `Parked` workers to `Exited` and lets `Running` finish first. `init_instances` and the probe (f.9) run on a worker in `Idle` with the others `Parked`.

### e.2 Run state machine

`Init` (after `new`) → (`init_instances`; the controller's probes through `Prober`; on a resumed run also `apply_resume_point`) → `Running` (entered by `run` or `run_resumed`, which start the drives and the checkpoint thread) → `Draining` (source exhausted, closing queues in order as each empties) → `Finishing` (sink `finish`) → `Completed`; from `Running` or `Draining`, `Terminating` (error under policy, or `Knobs::terminate`) → `Terminated`; `Cancelling` → `Cancelled`. The checkpoint thread runs while the state is `Running` or `Draining` and is stopped on leaving them. In `Terminating` and `Cancelling`: source drive stops issuing; workers finish current tasks; placement `shutdown`; in-flight sink writes complete; the final manifest is written by the scheduler when checkpointing is on (f.12); trace `flush` (the facade calls `finish` once, 04 TR-I5).

### e.3 Stage table

```rust
struct StageEntry { stage: StageId, kernel: Arc<dyn Kernel>, spec: PayloadSpec, pool: Option<InstancePool>, out_bytes: AtomicU64 /* mirrored from placement stats each pick */, tasks: AtomicU64, busy_ns: AtomicU64, errors: AtomicU32, skipped: AtomicU32 }
struct InstancePool { states: Mutex<Vec<Slot>>, max: usize }   // Slot { state: Box<dyn KernelState>, owner: Option<u16 /* worker */>, in_use: bool, retired: bool }
```

## f. Algorithms and policies

**f.1 Startup (`new`).** In the facade's startup order (12 PY-I1) the scheduler is built after placement and before the controller. `new`: validate the chain, `PayloadSpec::check` of each kernel against the upstream schema (contracts CT-I5) and the sink's `accepts` against the last kernel's `output_schema` (or the source schema when there are no kernels); call `source.plan()` once and keep the splits (the facade's own `plan` call, 12 f.1, is the same call and must return the same list; a source caches it); `set_consumer` on every queue; detect resumability: `sink.checkpoint()? == None` means a non-resumable sink (contracts d.8, the detection rule stated there), so `checkpoint_enabled` is forced false with `sched.not_resumable` naming the sink, and `source.repeatable() == false` forces it false too, naming the source; when `!cfg.resuming`, `sink.open(schema)`; spawn `workers_max` threads parked; spawn the two drive threads idle (the source drive answers only helper requests, f.9, and issues no read of its own; the sink drive pops nothing) until `run` or `run_resumed` starts them driving. The checkpoint thread is not started; the sink is not opened when `cfg.resuming` (`apply_resume_point` resumes it, so a resumed run never opens it twice). Then the facade calls `init_instances` (f.4), the controller's `prepare`, `probe_all` (which calls `Prober::probe`, f.9), `start`, and finally `run`.

**f.2 Worker loop.**

```
loop:
  heartbeat[w] = now()
  if !active_slot(): park(); continue
  stage = pick()                                     // f.3; None → park 1 ms, continue
  (morsel, wait_us) = placement.pop_blocking(stage-1, spec[stage], Locality::Any)   // resident by construction of pick, so wait_us is normally 0; None (closed) → continue
  (state, slot) = acquire_instance(stage)            // f.4; NoState for a stateless stage
  t0, c0, s0 = now(), thread_cpu(), sampler.sample()
  result = catch_unwind(|| kernel.apply(state, morsel.payload))
  t1, c1, s1 = now(), thread_cpu(), sampler.sample()
  heartbeat[w] = now()
  state_bytes = state.footprint().unwrap_or(0)       // while the instance is still held
  release_instance(slot)
  match result:
    Ok(out)  → out_morsel = morsel.with_output(out); placement.push(stage, out_morsel); record(Ok)
    Err(e)   → apply_policy(e); record(Error)
  loop
```

`record` builds the `TraceRecord` (contracts d.13) from `morsel` (features, bytes_in), `out` (bytes_out, rows_out, tier_out), timings, the knob snapshot, `placement_miss_wait_us = wait_us` (the second element `pop_blocking` returned, PL-I9), `state_bytes`, `instance` (the slot index, `u16::MAX` for a stateless stage), `cpu_time_us = c1 − c0`, and the memory fields with these semantics (shared with RC f.4): `mem_anon_before = s0.anon_bytes`, `mem_anon_peak = max(s0.anon_bytes, s1.anon_bytes)`, `throttled_delta_us = s1.throttled_us − s0.throttled_us`, `dev_mem_peak = s1.device_used[d]` for the instance's device (0 otherwise). The sampler's monotonic `peak_anon_bytes` is not used per record; it belongs to the probe (f.9). After `trace.record(r)` the worker calls the installed `RecordHook`, if any, with `&r` (the controller's `on_record`, RC d.1); the hook is cheap by contract and runs on the worker.

**f.3 `pick`.** Snapshot `placement.stats()` output bytes per queue (cached per 1 ms to avoid contention; the cache is refreshed by whichever worker finds it stale); for stages 1..=n: admissible iff the input queue's head is resident (`placement.peek_resident(stage−1, spec[stage], Locality::Any)`, the contract's non-consuming check) and `!placement.is_full(stage)` and (stateless or an instance is free); choose the admissible stage with the smallest output bytes, ties to the higher stage; return it. Cost: O(stages) per pick with no lock except the peek.

**f.4 Instances.** `init_instances`: for each stateful stage in order and each instance `i` in `0..max_instances`, the worker `i mod workers_max` (unparked for the call, the others parked) runs `kernel.init(InitCtx { instance: i, device: assigned round-robin over devices when `uses_device_memory`, alloc })` and stores the slot with `owner = Some(that worker)`; the first `Err` stops the loop and is returned as is (a `Kernel` error from `init`, adapters AD-I7), with nothing read or written (architecture 7, "stateful init fails"; SC-T18). Stateless stages get no instances. `acquire_instance(stage)`: lock pool; prefer a free slot whose `owner == this worker`; else any free slot; else return `None` and the worker re-picks (the stage was admissible only if a slot was free, so this is a race, not a policy). A slot is retired after a kernel error or panic (adapters AD-I7 rationale) and re-created by `kernel.init` on the next acquire by the worker that acquires it; that is the only `init` after `init_instances`. On a resumed run, `apply_resume_point` fills the pools (f.13) and `init_instances` is not called by the facade.

**f.5 Source drive.** A thread, idle until `run` (f.1). Iterate the splits kept by `new`; for each, while `in_flight < read_ahead` and `!placement.is_full(0)` and not stalled by an ordered sink (`sink.is_stalled()`, `Ordered` handles only): compute the row range from the morsel target (`morsel_target[1]`, the first kernel's target, or `morsel_target[0]` when there is no kernel, h) and the split's bytes per row (`uncompressed_bytes / rows`), clamp the byte size to `[morsel_min, morsel_max]`, sub-split when `sub_splittable`, and never below one row: when one row's estimated bytes exceed `morsel_max` the range is exactly one row and the morsel passes at its natural size (architecture 7, "a single row larger than the maximum morsel"; the trace shows it through `bytes_in`, and the controller sizes around it, RC h); issue `source.read(split, range, alloc, tier0)` where `tier0` is `PinnedHost` if `alloc.is_pinned()` else `Host`; on completion (the drive polls the returned future and `wait`s on the completions inside it; this thread is one of the two the contract permits, CT-I7), `placement.push(0, Morsel::new(seq++, 0, payload, origin))` with `origin.node = cfg.node`. The drive keeps its position as a `SourceCursor { split_index, row_offset, next_seq }` (the next range to issue, not the last completed one) and exposes it for f.12; sequence numbers are assigned at issue in cursor order, so the cursor and the sequence counter always agree. Also service `placement.evicted(0)` by re-issuing reads for evicted entries and calling `replace(0, morsel)`, and service drive-side helper requests (f.9). A read that fails with `Alloc` (a single row larger than the budget) or `Source` terminates the run naming the split and the row range (h). When the plan is exhausted and no read is in flight, `placement.close(0)` and set `source_exhausted`. With `read_ahead = 0` the drive issues one read at a time only when Q0 is empty (a floor of one).

**f.6 Sink drive.** A thread, idle until `run` (f.1). Loop: pop the head of Qn (`n` = number of kernels; Q0 when there are none) with `pop_blocking(n, sink_spec, Locality::Any)` when checkpointing is on, or with `pop` followed by a park bounded by `heartbeat_interval_ms` (unparked by any push to Qn) when it is off, so that the heartbeat check (f.14) runs at least once per interval in either configuration; on `Some(m)`: `sink.write(m.seq, m.payload)` (the reorder buffer, when present, is the sink and orders internally); keep up to `sink_concurrency` writes in flight, waiting on their completions; after each write completes, read `sink.committed_seq()` and, when it moved, call `placement.set_committed(w)` (f.11). The sink is not a stage and emits no trace record; its throughput is visible through the last queue's drain rate and `SinkStats`. On `None` (closed and empty) with no write in flight: `finish` the sink, signal completion.

**f.7 Draining and closing.** When the source is exhausted and Q0 is empty and no stage-1 task is running, `close(1)`; and so on down the chain; each queue is closed when its producer stage has no task running and its input is closed and empty. This is evaluated by whichever worker or drive observes the condition (a monotonic check, safe to evaluate concurrently).

**f.8 Error policy.** `apply_policy(e)`: `Terminate` → set run state `Terminating` with `e` as the diagnostic (enriched with the morsel's seq, stage, features and the `s1` sample); `Skip` → drop the morsel and call `sink.skip(seq)` (the contract's method; a `ReorderBuffer` advances past the sequence and forwards it, a file sink counts it as committed for its watermark, SI f.7), so a skipped morsel never holds the commit watermark back; on resume a skipped sequence above the watermark is replayed like any other and may be skipped again; `Budget(n)` → errors 1 to n−1 are `Skip`, the n-th (when `errors_total == n`) is `Terminate`; `Budget(1)` therefore equals `Terminate`. A kernel panic is converted to `AmoruError::Kernel { msg: "panic: ..." }` first. One exception precedes the policy: an `Alloc { tier: Device(_) }` error from `apply` (device out of memory, architecture 7) is built into a `TraceRecord` (`Outcome::Error`) that is passed to the record hook only, not to `TraceSink::record` (so SC-I4 and TR-I2 hold: the trace sees one record per stage and morsel), the controller's f.11 shrinks the stage's device footprint inside that hook, and the morsel is then retried once on the same instance; the retry's outcome is the record the trace receives, and only a second failure of that morsel reaches `apply_policy`. `Knobs::terminate(diagnostic)` (contracts d.11) takes the `Terminate` path with the controller's diagnostic, from the controller's thread, regardless of the policy.

**f.9 Probe protocol (`Prober::probe(stage, bytes)`).** Park every worker but one (the probing worker). Obtain the input: for `stage == 1`, send the source drive a helper request for one read of about `bytes` (rows = `bytes / bytes per row` of the current split, clamped to the split and to one row at least) at the cursor, advancing it; the drive issues and waits on the read and pushes the morsel to Q0 with the next `seq`, then acknowledges; for `stage > 1` the input is the head of Q(stage−1), the previous stage's probe output. The drive answers helper requests in `Init` too, before `run`, which is when the controller probes. Then on the probing worker: `sampler.reset_peak()`; `s0 = sampler.sample()`; pop with `pop_blocking(stage−1, spec[stage], Locality::Any)`; run f.2's body once for `stage`, capturing `s1 = sampler.sample()` after `apply`; push the output downstream as normal (nothing is wasted; with the sink already open, f.1, a last-stage probe output simply waits in Qn until the sink drive starts in `run`); write the trace record with `Outcome::Probe` from the probing worker (SC-I4); return `ProbeResult { bytes_in, rows_in, peak_delta = s1.peak_anon_bytes − s0.anon_bytes (saturating), dev_peak_delta = s1.device_used[d] − s0.device_used[d] (saturating, 0 without a device), wall_ns, cpu_ns }`; unpark. All other workers are parked for the whole call, so the peak the sampler reports is the probe's own. A probe of a stage with no kernel (`kernels=[]`) is a `Plan` error; the controller does not ask (RC f.3).

**f.10 Cancellation and shutdown.** `cancel` observed by drives and workers between tasks; state `Cancelling`; f.7 is skipped; placement `shutdown`; sink writes in flight complete; the final manifest is written by the scheduler (f.12) when checkpointing is on; sink `finish` is not called; the checkpoint thread is stopped; trace `flush`; return `Cancelled { manifest }`. `shutdown()` (d.1): set the cancel flag, unpark every worker and join all of them (a `Running` worker finishes its `apply` first, bounded by the longest kernel), stop the drive threads (the source drive lets in-flight reads resolve and drops the buffers; the sink drive lets in-flight writes complete) and join them, stop and join the checkpoint thread; idempotent; called from `Drop`, so dropping a scheduler that is not running costs a few joins.

**f.11 Commit watermark.** The sink drive owns the watermark `w`: after each completed write and after each `sink.skip`, `w' = sink.committed_seq()` (the sink already counts skipped sequences, SI f.7); when `w' > w`, `placement.set_committed(w')` and `stats.committed_seq = w'`. With a sink whose `committed_seq` is always `None` (not resumable, detected at `new`, f.1), `w` never moves, the lineage index grows for the run, and `checkpoint_enabled` is false; the run is correct, only not resumable. At `Finishing`, after `sink.finish()`, `set_committed(next_seq − 1)`.

**f.12 Checkpoint thread.** When `checkpoint_enabled`: a dedicated thread (preamble 4.1; not a reactor thread, because `KernelState::checkpoint` may run kernel code, and not a worker), started by `run` or `run_resumed` on entering `Running` and stopped on leaving `Running`/`Draining`; it wakes every `checkpoint_interval_ms`, checks the heartbeat table (f.14), and calls the internal `checkpoint_now`. `checkpoint_now`: gather `CheckpointExtras { kernel_states, sink_state, committed_seq: w, source_cursor }` where `kernel_states` comes from calling `KernelState::checkpoint` on every instance of every `ResumePolicy::Checkpoint` stage (each instance is acquired for the call like a task, so no `apply` runs concurrently on it; SC-I6), `sink_state` from `sink.checkpoint()`, and the cursor from the source drive; then `placement.checkpoint(&extras)`, which writes the manifest with `std::fs` on the calling thread (placement f.12: temp file, fsync, rename; not through the reactor). A `Checkpoint` kernel whose `checkpoint` returns `Ok(None)` is a kernel bug: the run terminates with `Resume` naming the stage. Failures are counted and logged; three consecutive terminate the run (placement h). The thread never holds the stage table lock across the placement call. On termination and cancellation the scheduler itself calls `checkpoint_now` once more, from the thread that drives the exit, before stopping the checkpoint thread; the facade never writes a manifest, and there is no public `checkpoint` method.

**f.13 Resume.** Two steps, both on the facade's runtime thread. `apply_resume_point(point)`, in `Init`, in this order: refuse with `Resume` if `!checkpoint_enabled` before touching the sink (a resumed run must be able to write its own manifests); the message names the sink when the reason is a non-resumable sink (the f.1 detection) and the staging directory otherwise; `sink.resume(schema, sink_state, committed_seq)` instead of `open`, with `sink_state` the checkpointed bytes or an empty slice when `extras.sink_state` is `None` (a sink returning `Resume` ends the attempt with that error naming the sink; nothing has been written); set the source drive's cursor and sequence counter from `point.extras.source_cursor`; set `w = committed_seq`; for each `Checkpoint` stage, build instances from `kernel_states` through `kernel.restore(InitCtx, bytes)` in instance order on the owning worker; for each `Reinit` stage run the `init_instances` loop of f.4 for that stage (`Forbid` was refused by `Placement::restore`); keep `point.to_recompute`. The facade then runs the controller's `probe_missing` (RC f.14) and `start`, then calls `run_resumed(cancel)`: before the source drive issues new reads, it re-reads every `(seq, origin)` in `to_recompute` in order through the normal source path (`source.read(split_of(origin), Some(origin.row_start..origin.row_end), alloc, tier0)`) and pushes each to Q0 with its original `seq` (`Morsel::new(seq, 0, payload, origin)`), counting `stats.recomputed`; the re-reads obey `read_ahead` and `is_full(0)` like any other read, so a large recompute list does not blow the budget; then it continues exactly as `run` from `Running` (drives, checkpoint thread, `stats.resumed = true`). Sequence numbers above the cursor's `next_seq` are never reused, and the ones below it that are not in the lineage are, by construction, committed or skipped. Output equals an uninterrupted run (PL-T17).

**f.14 Worker heartbeat.** `heartbeat[w]` is an `AtomicU64` of nanoseconds, written at the points marked in f.2, plus `task[w]: (stage, seq, t_start)` while in `Running`. A checker runs at least once per `heartbeat_interval_ms` (1 s): on the checkpoint thread at every wake when checkpointing is on, otherwise on the sink drive at every wake of its bounded park (f.6). A worker is dead when it is not parked, its thread handle reports finished (`JoinHandle::is_finished`), and it did not exit through cancellation; a worker in `Running` for longer than the interval is not dead (a long kernel), it is reported in `sched.slow_task` at debug level. A dead worker terminates the run with `AmoruError::Kernel { stage, seq, msg: "worker <w> died outside apply" }` for its last task (a scheduler bug, never a kernel's fault, h failures).

**f.15 Knob handling (`Knobs::set`).** Clamp to the preamble's range: `MorselTarget.bytes` to `[morsel_min, morsel_max]`, `ActiveWorkers` to `[1, workers_max]`, `ReadAhead` to `[0, 64]`, `PromotionWindow.morsels` to `[1, 32]`; `HighWater.bytes` is passed as given; each clamp increments `SchedulerStats::knob_clamps` and logs `sched.knob_clamp` once per knob kind. Store `MorselTarget`, `ActiveWorkers` (unparking or letting workers self-park at the next loop head) and `ReadAhead` atomically; forward `StagingTrigger { stage, on }` to `placement.set_staging(stage, on)`, `HighWater { stage, tier, bytes }` to `placement.set_water(stage, tier, bytes / 2, bytes)`, and `PromotionWindow { stage, morsels }` to `placement.set_promotion_window(stage, morsels)`; record every forwarded value so `snapshot()` returns it (`KnobSnapshot.high_water`, `promotion_window`, contracts d.11). A `set` after the run has exited is a no-op (RC h).

## g. Concurrency within the component

Lock order (preamble 4.2): the stage table lock (position 1) is taken only during startup, `init_instances` and stage close; instance pool mutexes (part of position 1 by convention); the placement queue lock (position 2) is taken inside the engine's `pop`/`push` and never held by the scheduler across `apply` or across any call into another component. The stats cache is atomics plus a timestamp. Workers park on per-worker `crossbeam::Parker`s. The two drives are threads created by `new` and started driving by `run` (`std::thread`), not workers and not reactor threads: they are the only non-reactor threads that wait on a `Completion` (contracts CT-I7, RE-I2), they hold no scheduler lock while waiting, and they exchange requests with the probe and the checkpoint thread over `crossbeam` channels. The checkpoint thread and the heartbeat checker read the heartbeat table without locks. The `RecordHook` runs on the recording worker after the trace channel push (position 5), with no scheduler lock held.

## h. Behaviour

**Normal path.** `new` validates and opens; `init_instances` fills every pool; the controller samples the baseline and probes each stage through `Prober` (one worker active per probe); the controller sets initial knobs; `run`: drives and the checkpoint thread start; the source drive fills Q0 to high water, workers pick stage 1, outputs fill Q1, workers spread across stages by the emptiest-output rule, the sink drive drains Qn; source exhausts; queues close in order; sink finishes; `Completed`.

**Edge cases.** Zero splits: Q0 closes immediately; queues close in order; sink `finish` writes an empty output; `Completed`. Zero kernels (`kernels=[]`, 12 h): allowed; there are no stages, no probes (the controller sets `MorselTarget { stage: 0 }`, RC f.3) and no instance pools; the source drive reads at `morsel_target[0]` and the sink drive pops Q0; workers stay parked for the whole run; the chain check is the sink's `accepts` against the source schema. One stage, stateless, identity kernel: workers alternate stage 1; the emptiest-output rule is trivial. All workers parked (`active = 0`, controller bug): impossible after clamping (f.15, min 1). A kernel that returns a payload in a tier the next consumer cannot accept: placement handles it (promotion); the scheduler does nothing. `read_ahead = 0`: the floor of one read (f.5). A single row larger than `morsel_max`: passed at its natural size (f.5, SC-T17).

**Failures.** Source read failure: `Terminating` with the split named (there is no skip for source errors); a single row larger than the budget fails the read with `Alloc` and terminates with the split and row range in the diagnostic. Stateful `init` failure: `init_instances` returns the error and the facade never calls `run`; nothing has been read or written (SC-T18). Sink write failure: `Terminating` with the sink error; in-flight writes complete or fail; `finish` not called; the sink's own cleanup runs on drop. Placement `pop_blocking` returning `Err(Staging)` for a head that cannot be produced: `Terminating` with the placement diagnostic. Worker thread death outside `apply` (a scheduler bug): the heartbeat checker (f.14) terminates the run with an internal error rather than hanging (SC-T19). Sampler failure: the sampler never fails after construction (DS-I3); the controller decides when to terminate on read errors (RC h) through `Knobs::terminate`.

## i. Configuration

`workers.max`, `workers.active`, `readahead.splits`, `sink.concurrency`, `errors.policy`, `ordering.required`, `morsel.min_bytes`, `morsel.max_bytes` (as clamps in f.5 and f.15), `queue.promotion_window` (clamped in f.15), `checkpoint.enabled`, `checkpoint.interval_ms`. Range clamping for the scheduler-side knobs is owned here and counted (preamble section 5).

## j. Observability

`SchedulerStats` through `StatsSource` (contracts d.11), including `sink_concurrency` and `knob_clamps`; `TraceRecord` emission (the trace is the scheduler's primary output); `tracing`: `sched.start` (info: workers, stages), `sched.init_instances` (info: per stage, instances, duration), `sched.probe` (info), `sched.close_stage` (debug), `sched.policy` (warn on skip, error on terminate), `sched.cancel` (info), `sched.checkpoint` (debug: committed_seq, cursor, duration_us), `sched.resume` (info: committed_seq, to_recompute, cursor), `sched.not_resumable` (warn: the sink or the source), `sched.knob_clamp` (warn: knob, value, bound), `sched.slow_task` (debug), `sched.worker_dead` (error).

## k. Tests

Tests use `FakePlacement`, `FakeSource`, `FakeSink`, `FakeKernel`, `FakeTrace`, `FakeSampler` and `FakeAllocator` from contracts d.15 and name only the knobs listed there; a behaviour no fake knob provides comes from a test-local implementation of the contract trait, named as such.

**SC-T1 workers_only_apply.** Thread-id instrumentation: no worker thread ever executes reactor code or `Completion::wait`; the two drive threads are the only ones that do. SC-I1.

**SC-T2 admission_rule.** Three stages with controlled output-queue sizes; 1,000 picks; each pick chose the emptiest-output admissible stage with the specified tie-break. SC-I2.

**SC-T3 source_admission.** Q0 full → no reads issued; `read_ahead` respected under a slow `FakeSource` (`FakeReactor::with_latency` is not involved; the fake source's `read` future resolves after the test's own delay). SC-I3.

**SC-T4 one_record_per_morsel_stage.** 10,000 morsels × 3 stages → exactly 30,000 records with unique `(stage, seq)`, plus one probe record per stage, every probe record carrying the probing worker's id. SC-I4, G-I4.

**SC-T5 knobs_immediate.** Set `ActiveWorkers(2)` on a 16-worker pool; within one task duration, at most 2 workers are busy; raise to 16, all busy. SC-I5, SC-I7.

**SC-T6 instance_affinity.** `FakeKernel::stateful(4, 0)` on 8 workers; `init_calls == 4` after `init_instances` and before any morsel; no instance used concurrently; workers reacquire their previous instance ≥ 90% of the time under steady load. SC-I6.

**SC-T7 error_policies.** For `Terminate`, `Skip`, `Budget(3)`: `FakeKernel::fail_on([5, 9, 12, 20])`, which fails the 5th, 9th, 12th and 20th apply (contracts d.15: the fake counts applies, because a kernel never sees a sequence number); the assertions are on the scheduler's own record of which sequence numbers those applies carried, read from the trace, and outcomes match (`Budget(3)` skips 5 and 9 and terminates on 12); the sink's `skipped()` lists exactly the sequence numbers the trace marks `Skipped`, and no others, under `Skip`; `FakeKernel::panic_on([7])` produces a `Kernel` error whose message starts with `panic:`, not a pool crash. SC-I8.

**SC-T8 completion_exact.** Random pipeline sizes and speeds; `run` returns `Ok(Completed)` with every queue empty and the sink's row count equal to the source's (identity). SC-I9.

**SC-T9 draining_order.** Queues close in stage order; a stage never closes while its producer has a task running. f.7.

**SC-T10 probe_protocol.** `Prober::probe(1, 16 MiB)` runs with exactly one worker active, issues exactly one `FakeSource` read (`reads()` grows by one, at the cursor) and advances the cursor; `probe(2, ..)` issues no read and pops Q1's head; each returns a `ProbeResult` whose `peak_delta` equals the `FakeSampler::scripted` peak minus the scripted `anon_bytes` before; the output morsel appears in the next queue; `FakeSampler.peak_resets == 2`. f.9.

**SC-T11 cancel.** Cancel mid-run; returns `Ok(Cancelled)` within the longest kernel duration + 1 s; `FakeTrace.flush_calls >= 1` (contracts d.15); `FakeSink.finish_calls == 0`; the checkpoint thread is joined; `shutdown` afterwards is a no-op. f.10.

**SC-T12 utilisation.** (reference host, E1) Compute-bound `FakeKernel::latency(50 ms)` on 8 workers; busy fraction ≥ 85% over 60 s. S4.

**SC-T13 evicted_replay.** `FakePlacement::with_pressure(0, evict_after_bytes)` so entries past the byte count on Q0 become `Evicted`; the source drive re-reads each (`FakeSource.reads()` shows the origin ranges again) and calls `replace`; the sink's `written()` equals the run without pressure. PL-I6 interplay.

**SC-T14 watermark_and_skips.** `FakeSink::commit_every(10)` (implements `skip`, d.15); `Skip` policy with `FakeKernel::fail_on([7, 23, 24])`, the 7th, 23rd and 24th apply; `FakeSink.skipped()` equals, exactly and in order, the three sequence numbers the trace marks `Skipped`; `FakePlacement.committed()` shows `set_committed` called with exactly the model's watermark (skips do not hold it back) and never with a lower value than before. f.11, f.8.

**SC-T15 checkpoint_tick.** With `checkpoint_interval_ms = 50` and `FakeKernel::stateful(2, 0).resume(ResumePolicy::Checkpoint)`: `checkpoint_calls` grows by two per tick, on the checkpoint thread (thread-id assertion: never a worker, never a drive), with no concurrent `apply` on that instance (asserted inside the fake kernel); `FakePlacement::with_manifest_store()` receives, in `manifests_written()`, a cursor that equals the next range the source drive issues; the thread starts only at `run` (no manifest before it) and a manifest is written on termination without any call from the test; a `Checkpoint` kernel returning `Ok(None)` (a test-local `Kernel`) terminates with `Resume`. f.12.

**SC-T16 resume_equivalence.** Run a 3-stage pipeline with `FakeSource` (`reads()` counting) to `FakeSink::resumable(true).commit_every(10)` over `FakePlacement::with_manifest_store()`; drop the scheduler after a random number of commits; build a new scheduler with `resuming = true` over a `Placement::restore`d fake, call `apply_resume_point`, then `run_resumed`; the sink's final `written()` set equals an uninterrupted run; `Source::read` was called once per recomputed origin and once per new range, never for a committed sequence; a `FakeKernel::resume(Reinit)` kernel's `init_calls` grew again in `apply_resume_point`, a `FakeKernel::resume(Checkpoint)` kernel's `restore_calls` equals its instance count with the bytes it checkpointed; with `FakeSink::resumable(false)` the scheduler's `new` forces `checkpoint_enabled` false and `apply_resume_point` returns `Resume` naming the sink before any `resume_calls`. f.13, S17.

**SC-T17 single_row_larger_than_max.** `FakeSource::splits(1, 4, 4 GiB)` (1 GiB per row) with `morsel_max = 512 MiB` and `sub_splittable(true)`: the source drive issues four one-row reads (`reads()` shows four ranges of one row); each stage-1 record has `bytes_in ≈ 1 GiB > morsel_max`; the run completes; with `FakeAllocator::with_limit(Host, 512 MiB)` instead, the first read fails with `Alloc` and the run terminates with a diagnostic naming split 0 and rows 0..1, before any `written()`. f.5, h (architecture 7).

**SC-T18 stateful_init_fails.** A test-local `Kernel` (`KernelKind::Stateful { max_instances: 2 }`) whose `init` returns `Kernel { msg: "init failed" }` for instance 1: `init_instances` returns that error; `FakeSource.reads()` is empty, `FakeSink.written()` is empty and `finish_calls == 0`; `run` was never called; `shutdown` joins the parked workers cleanly. f.4, h (architecture 7).

**SC-T19 worker_heartbeat.** With `heartbeat_interval_ms = 1000`: a test seam (`Scheduler::test_kill_worker(w)`, compiled under `cfg(test)`) makes worker `w` exit its loop mid-run without reporting; with checkpointing on (`FakePlacement::with_manifest_store()`, interval 50 ms) the checkpoint thread reports the death and the run terminates with the internal diagnostic naming `w` within 2 s; with checkpointing off, the sink drive's bounded park does the same within 2 s; a `FakeKernel::latency(3 s)` kernel is never reported dead. SC-I11, f.14.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/pipeline.rs` (validation, f.1), `src/worker.rs` (e.1, f.2, the heartbeat writes), `src/pick.rs` (f.3), `src/instances.rs` (f.4 `init_instances`, and the restore path of f.13), `src/source_drive.rs` (f.5, the cursor, the helper requests of f.9, the recompute pass of f.13), `src/sink_drive.rs` (f.6, f.11, the heartbeat check when checkpointing is off), `src/lifecycle.rs` (e.2, f.7, f.10, `run`, `run_resumed`, `apply_resume_point`, `shutdown`), `src/policy.rs` (f.8, `terminate`), `src/checkpoint.rs` (f.12, the checkpoint thread, `checkpoint_now`, the heartbeat check when checkpointing is on), `src/heartbeat.rs` (f.14), `src/probe.rs` (f.9), `src/knobs.rs` (f.15, atomics, `snapshot`), `src/stats.rs` (`StatsSource`), `src/cputime.rs`. `unsafe` is permitted only in `cputime.rs`, for the single `libc::clock_gettime(CLOCK_THREAD_CPUTIME_ID)` call behind a safe `thread_cpu_ns()` (`// SAFETY:` a valid out-pointer to a `timespec`); nowhere else outside tests (E9). `catch_unwind` around `apply` requires kernels to be `UnwindSafe`; wrap with `AssertUnwindSafe` and document that a panicking kernel's state is retired.

The drives poll the `BoxFuture`s that `Source::read` and `Sink::write` return with a minimal single-future executor (a `Waker` over the drive's `Parker`); the futures inside await `Completion`s, which implement `Future` (contracts d.9). No tokio dependency in this crate.

Anti-patterns: no central dispatcher thread; no worker holding a placement lock across `apply`; no trace record built anywhere but `worker.rs` and `probe.rs`; no `unwrap` on `pop` results; no public `checkpoint` method; no local definition of a type the contracts define.

## m. Open items

None. (Every placement method the scheduler uses is on the `Placement` trait, contracts d.10.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| D1, D6 | SC-I1, SC-I2, SC-I6 | SC-T1, SC-T2, SC-T6 |
| S4 | f.2, f.3 | SC-T12 |
| S6, G-I8 | SC-I8, f.8 | SC-T7 |
| S9, G-I4 | SC-I4 | SC-T4 |
| S17, D13 | f.11, f.12, f.13 | SC-T14, SC-T15, SC-T16 |
| S16, D12 | `Locality::Any` at every pop, `Origin.node` | (compile-time; CT-T14 lint) |
| G-I5 | SC-I5, SC-I10 | SC-T5 |
| G-I9 | SC-I1 | SC-T1 |
| D8 | f.1 (linear chain) | SC-T8 |
| preamble 4.3 | f.10 | SC-T11 |
| D3, RC f.2 (probe protocol) | f.9 | SC-T10 |
| architecture 7 (single row larger than max morsel) | f.5 | SC-T17 |
| architecture 7 (stateful init fails) | f.4 | SC-T18 |
| G-I8 (no hang on a dead worker) | SC-I11, f.14 | SC-T19 |

## o. Deferred (post-v1)

None.
