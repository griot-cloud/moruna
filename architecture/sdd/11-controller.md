# Moruna SDD 11: Resource controller (`moruna-controller`)

**Document type:** software design document, component 11 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/moruna-runtime-design.md` section 5.8; decisions D2, D3, D7; criteria S1, S2, S3, S6, S11; global invariants G-I1, G-I5, G-I8, G-I10
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.10 (`Placement::set_budgets`, `TierBudgets`), d.11 (`Knobs`, `Knob`, `KnobSnapshot`, `StatsSource`, `SchedulerStats`, `Prober`, `ProbeResult`, `SizerKind`), d.12 (`Limits`, `Sampler`, `Sample`), d.13 (`TraceRecord`, `TraceTail`)
**Component location:** `crates/moruna-controller`, Rust
**Consumes:** contracts (1) only; the sampler (3), the trace tail (4), the placement engine (9, `set_budgets`) and the scheduler (10, `Knobs`, `StatsSource`, `Prober`) arrive as trait objects. **Consumed by:** runtime facade (12)

**Decisions worth your eye:** (1) the sizing decision is a trait with a rule implementation shipped and a learned implementation stubbed, and the envelope that clamps both is computed from the probe and the budget by the controller, never by the sizer; (2) there are two working-set inequalities, one on the arena's capacity and one on the process's anonymous memory, and both are enforced as a hard check before any knob is written, so an inconsistent set of knobs is impossible by construction; the second is the one S1 is measured from and it is fitted to `TraceRecord::mem_anon_peak`; (3) the profile store is a JSON file per kernel fingerprint and schema hash, advisory only, overridden by the probe on drift; (4) the controller names no other crate: everything it touches is a contracts trait, so it is testable entirely with the testkit fakes.

---

## a. Purpose and boundary

The controller is the one writer of every knob. It derives the budget from the limits, measures each kernel's amplification with a probe, sizes morsels and workers from the working-set equation, adjusts them from measured outcomes, classifies the bottleneck each tick and moves the knob that relieves it, keeps device memory as a second budget, and falls back safely when a learned sizer misbehaves. It is the component that does not exist elsewhere (parent 5.8).

It owns: budget arithmetic; the probe protocol's decisions (the scheduler executes it through `Prober`); the working-set model; the `Sizer` trait and `RuleSizer`; the envelope; bottleneck classification; damping and oscillation freeze; the profile store; the bottleneck timeline and notes for the report.

It refuses to know: morsel bytes; queue internals; how the scheduler picks; how the reactor moves; anything about Python.

## b. Vocabulary

**Allowance.** Per stage, the bytes one in-flight morsel of that stage may occupy at peak: `morsel_target × A_k × safety`.

**Working set.** Two inequalities, both of which a knob set must satisfy (f.3).

* **Arena.** `Σ_stages share(stage) × allowance(stage) + Σ_queues high_water + read_ahead × split_bytes + state`, which must fit `arena_bytes`. This is what the runtime may hold.
* **Anonymous.** `resting_anon + state + Σ_stages share(stage) × anon_allowance(stage)`, which must fit `limits.memory_ceiling`. This is what the *process* may hold, and it is the quantity S1 is measured from.

**Resting anonymous memory.** `baseline + arena_bytes`. 02 f.1 touches every page of the region at `new`, so the process holds this much from the moment the arena exists, whether the runtime is using it or not. The controller cannot move it.

**Anonymous headroom.** `ceiling − resting_anon`, which with the facade's arena sizing is the reserve, the declared kernel state and the share of the allowance the facade withheld from the arena for what the chain allocates outside it (12 f.1). It is the whole allowance for what kernels allocate outside the arena, and it is the numerator of the anonymous inequality.

**Out-of-arena amplification (`a_anon`).** Anonymous bytes a kernel adds to the process per byte of morsel *in flight*, fitted to `TraceRecord::mem_anon_peak` (f.3). It is seeded from the probe at the same value as `A_k`, because the probe runs one morsel on one worker (SC f.9).

**Anon allowance.** Per stage, `morsel_target × a_anon × safety`: what one in-flight morsel costs the process outside the arena.

**Envelope.** Per stage, the closed interval of morsel targets the controller permits the sizer to propose: `[morsel.min_bytes, min(morsel.max_bytes, budget_for_stage / (share × A_k × safety))]`.

**Adjustment.** One change of a stage's morsel target; additive increase or multiplicative decrease.

**Damping.** The minimum number of completions between adjustments for a stage.

**Flip.** A change of sign between consecutive adjustments of one stage.

**Tick.** The periodic evaluation every `controller.tick_ms`; also triggered synchronously after every trace record, because the facade installs `on_record` as the scheduler's `RecordHook` (contracts d.11).

**Profile.** A stored summary of a kernel's measured behaviour keyed by fingerprint and input schema hash.

**State.** Per stage, the bytes its instances hold regardless of morsel size: `instances_live × max(state_bytes over the stage's last 32 records)`, grouped by the record's `instance` column when present (f.3).

## c. Invariants

**RC-I1. Knobs are consistent before they are written.** Every knob write is preceded by both working-set checks of b; if a proposed set violates either, the controller reduces morsel targets (largest stage first) until both hold, and only then writes. If reducing the targets to `morsel_min` is not enough, the queue high waters follow them down, but only for the arena inequality, because the queues live in the arena and emptying them gives the process's anonymous peak back nothing at all. The one set that may still fail is the smallest set there is: every target at the floor and every queue at zero, which is not an inconsistent write but a run f.6's StateGrowth row or f.7's termination is about to end. The check is also an obligation between writes: the `state` term and `a_anon` both move under the controller, so a set that fitted when it was written can stop fitting with nothing written at all, and every tick restores it before it decides anything else. A write from that repair is not an adjustment and is not damped (RC-I3). Upholds G-I1 and, through the anonymous inequality, S1.

**RC-I2. The sizer never escapes the envelope.** A sizer's proposal is clamped to the envelope, which is bounded by both inequalities of b and refreshed whenever a record moves `a_anon` (f.3); the clamp is counted. The learned sizer is replaced by `RuleSizer` for the rest of the run when, after 20 proposals, more than `1 / sizer.fallback_error_ratio` of its proposals were clamped (the default ratio 2.0 means more than half), or when its rolling prediction-error p95 exceeds `sizer.fallback_error_ratio ×` the shadow rule sizer's (f.8). Upholds D7.

**RC-I3. Adjustments are damped.** No stage's morsel target changes more often than once per `controller.damping_completions` completions of that stage, except the two decreases that must not wait: the multiplicative decrease on a breach (RC-I4) and the RC-I1 repair after a record refits `a_anon`. Every downward path is immediate; only the increase is damped.

**RC-I4. A breach shrinks immediately.** When a record's `mem_anon_peak` (contracts d.13; the larger of the samples before and after `apply`, SC f.2) exceeds the **breach line**, which is `resting_anon + 0.5 × anon_headroom` capped at the ceiling, or a record's `dev_mem_peak` exceeds the device budget, the affected stage's target is halved at once, `safety` for that stage is raised by 0.2 (capped at 3.0), `a_anon` is refitted from the record, and the worker count goes straight to what the anonymous inequality then allows (f.7). Upholds G-I8.

**RC-I5. Oscillation is frozen.** If a stage's adjustments flip sign more than `controller.oscillation_flips` times in its last 100 adjustments, the target is frozen at the geometric mean of the last 10 for `controller.freeze_morsels` completions, and the event is recorded. Upholds S11.

**RC-I6. The probe precedes sizing.** `start` refuses a controller that has not probed (`Config`), so this holds by construction and not by the facade keeping to the order of preamble 4.4. No stage receives a morsel target other than `morsel.probe_bytes` until its probe has completed and `A_k` is set; a profile may seed `A_k` but does not skip the probe, except on a resumed run (f.14), where the profile the interrupted run wrote is trusted for the stages it covers and only the others are probed, and on the tiny-dataset and zero-kernel paths (f.10, f.3), which have nothing to probe for.

**RC-I7. Worker count never exceeds what memory can feed.** `active_workers ≤ min(floor(budget_for_workers / max_stage_allowance), floor(anon_for_kernels / max_stage_anon_allowance))`, re-evaluated at every tick. The tiny-dataset path of f.10 is exempt from the first bound and never from the second: a small dataset does not make a greedy kernel cheap, and the ceiling S1 measures against is the process's either way.

**RC-I8. Bottleneck classification moves at most one knob per tick.** The table in f.6 selects one action; the controller never adjusts read-ahead and workers in the same tick.

**RC-I9. The controller never touches payload bytes or blocks on IO.** It reads stats and samples, writes knobs, and reads the trace tail; nothing else.

**RC-I10. A tick holds locks for at most 5 ms.** The controller's own mutex is held for the arithmetic of one tick and released before every call into another component; a sample that takes longer than 5 ms is skipped and counted (preamble 4.2).

## d. Interfaces

### d.1 Exposed

```rust
/// What the facade learned from `Source::plan` (12 f.1); the controller never sees the splits.
#[derive(Clone, Debug, Default)]
pub struct PlanSummary {
    pub total_bytes: u64,             // Σ uncompressed_bytes
    pub total_rows: u64,              // Σ rows; with total_bytes gives the bytes per row the probe size needs (f.2)
    pub splits: u32,
    pub max_split_bytes: u64,         // largest split
    pub sub_splittable_all: bool,     // every split can be read in row ranges
}

pub struct ControllerConfig {
    pub limits: Limits,
    /// The arena's host capacity, which is the controller's whole host allowance (f.1). The
    /// facade sizes the arena by f.1's rule and passes the same number here, so the bytes the
    /// controller believes it may hold are the bytes the arena can actually give it.
    pub arena_bytes: u64,
    /// The process's anonymous memory sampled *before* the arena was created (f.1). Reported
    /// in `Budgets` and never subtracted from `arena_bytes`.
    pub baseline_bytes: u64,
    pub plan: PlanSummary,
    pub workers_max: u16,                          // workers.max, the W of f.3
    pub pinned: bool,                              // Allocator::is_pinned(); decides which host pool TierBudgets fills (f.1)
    pub reserve_fraction: f32, pub target_fraction: f32,
    pub safety_initial: f32, pub safety_floor: f32, pub increase_step: f32,
    pub tick_ms: u64, pub oscillation_flips: u32, pub freeze_morsels: u32,
    pub morsel_min: u64, pub morsel_max: u64, pub probe_bytes: u64,
    pub sizer: SizerKind,                          // contracts d.11: Rule | Learned
    pub fallback_error_ratio: f32,
    pub profiles_dir: Option<std::path::PathBuf>,
    pub disk_budget: u64,
    pub checkpoint_enabled: bool,                  // periodic profile writes, f.9
    pub checkpoint_interval_ms: u64,               // cadence of those writes
}

/// Every method takes `&self`: the controller's mutable state lives in one `Mutex<ControllerState>`
/// (g), so the facade can hold it in an `Arc`, install `on_record` as the scheduler's record hook
/// before `prepare`, and still call `prepare`, `probe_all`, `probe_missing`, `start` and `stop`
/// through the same shared handle.
pub struct Controller { /* private: Mutex<ControllerState>, the tick thread handle, the record queue */ }
impl Controller {
    pub fn new(
        cfg: ControllerConfig,
        knobs: Arc<dyn Knobs>,            // the scheduler (contracts d.11)
        stats: Arc<dyn StatsSource>,      // the scheduler
        prober: Arc<dyn Prober>,          // the scheduler
        sampler: Arc<dyn Sampler>,        // discovery's sampler, shared with the scheduler (contracts d.12)
        trace: Arc<dyn TraceTail>,        // the trace writer (04 d.1)
        placement: Arc<dyn Placement>,    // `set_budgets` only; no other placement method is ever called
        kernels: Vec<KernelInfo>,
    ) -> Result<Controller>;
    /// Phase 1: sample the baseline (after `Scheduler::init_instances`, SC f.4), compute budgets,
    /// `placement.set_budgets`. Called by the facade before probes.
    pub fn prepare(&self) -> Result<Budgets>;
    /// Phase 2: for each stage, `prober.probe(stage, bytes)` and set A_k (f.2).
    pub fn probe_all(&self) -> Result<()>;
    /// Phase 3: initial knobs from the working-set equation; then start the tick thread.
    pub fn start(&self) -> Result<()>;
    /// Synchronous hook the scheduler calls after each trace record (cheap; enqueues for the tick
    /// thread, except the breach path f.7 and the device path f.11). The facade wraps it as the
    /// `RecordHook` it installs with `Scheduler::set_record_hook` (12 f.1).
    pub fn on_record(&self, r: &TraceRecord);
    pub fn stop(&self) -> ControllerSummary;          // joins the tick thread; returns the timeline, notes and sizer state for the report
    /// Resume variant of `probe_all` (f.14): probes only stages without a usable profile, seeds the
    /// rest from the profile store, and writes the profile at once so a second crash keeps it.
    pub fn probe_missing(&self) -> Result<()>;

    // Added by the component 11 executor, 2026-09-22, with the reason for each. None of them
    // changes a cross-component interface: every one is this crate's own surface (E10 does not
    // apply), and each exists because a section k test or the phase 8 sizer cannot be written
    // without it.

    /// Install the factory that builds each stage's `Sizer`, instead of the one `cfg.sizer`
    /// names. Must be called before `prepare`. `Sizer` is public and `new` takes no sizer, so
    /// without this seam no sizer but the two this crate ships could ever reach the
    /// controller: it is how RC-T2's test sizers are installed and how the phase 8 learned
    /// sizer will be.
    pub fn set_sizer_factory(&self, factory: SizerFactory);
    /// Run one tick on the calling thread; the tick thread's body. Public so a test can drive
    /// the loop deterministically (RC-T8, RC-T9, RC-T17) instead of sleeping on `tick_ms`.
    pub fn tick_once(&self);
    /// The summary as it stands, without stopping. `stop` is the end of the run, and a test
    /// that asserts on the timeline or the notes mid-run needs this.
    pub fn summary(&self) -> ControllerSummary;
    /// The longest the controller's mutex has been held, in nanoseconds: the guard timer
    /// RC-T17 measures RC-I10 with.
    pub fn max_lock_held_ns(&self) -> u64;
    /// Whether the mutex is held right now. RC-T17 wraps each fake with this to assert that no
    /// call into another component happens under the lock.
    pub fn lock_is_held(&self) -> bool;
}

/// Builds one `Sizer` per stage; the argument to `set_sizer_factory`.
pub type SizerFactory = std::sync::Arc<dyn Fn(StageId) -> Box<dyn Sizer> + Send + Sync>;
/// The lock bound of RC-I10 in milliseconds, so RC-T17 and the run report quote one number.
pub const TICK_BOUND_MS: u64 = 5;

/// `schema_hash` is `SourceSchema::hash()` (contracts d.4) of the stage's input schema, which the
/// facade has from the chain validation.
pub struct KernelInfo { pub stage: StageId, pub fingerprint: Fingerprint, pub schema_hash: [u8; 32], pub hints: KernelHints, pub kind: KernelKind }
pub struct Budgets { pub host: u64, pub device: [u64; 8], pub baseline: u64, pub reserve: u64 }
// `host` is the arena's capacity (f.1); `baseline` is the pre-arena sample the facade took and
// `reserve` the headroom, both reported rather than subtracted.

pub trait Sizer: Send {
    fn propose(&mut self, obs: &Observation, envelope: &Envelope) -> Proposal;
    fn observe(&mut self, obs: &Observation, outcome: &SizerOutcome);
    fn confidence(&self) -> f32;
    fn name(&self) -> &'static str;
}
pub struct Observation {
    pub stage: StageId, pub features: MorselFeatures, pub active_workers: u16,
    pub a_k: f64, pub safety: f32,
    pub target: u64,                       // the stage's current morsel target
    pub completions_since_adjust: u32,     // for damping, so a sizer cannot ignore it
    pub damping: u32,                      // controller.damping_completions as in force now
    pub recent: Vec<TraceRecord>,          // trace.tail(stage, 32)
}
pub struct Envelope { pub min: u64, pub max: u64 }
pub struct Proposal { pub morsel_target: u64, pub predicted_peak: Option<u64> /* the peak the sizer expects at that target; None for RuleSizer; feeds f.8 */ }
pub struct SizerOutcome { pub peak_delta: u64, pub bytes_in: u64, pub wall_ns: u64 }
pub struct RuleSizer { /* f.4 */ }
// `RuleSizer::new(target_fraction, increase_step)` and `LearnedSizer::new(..)` take those two
// configuration values, because f.4's rule is written in terms of them and `Observation` does
// not carry them; the alternative would break section l's rule that a sizer reads nothing but
// its `Observation` (executor, 2026-09-22).
pub struct LearnedSizer { /* stub in v1: returns RuleSizer's proposal with predicted_peak = None; the phase 8 implementation replaces the body without changing the trait */ }

#[derive(Clone, Debug)]
pub struct ControllerSummary {
    pub timeline: Vec<(f64, Bottleneck)>, pub sizer: &'static str, pub fallback_at: Option<Seq>,
    pub freezes: u32, pub breaches: u32, pub final_knobs: KnobSnapshot,
    pub notes: Vec<String>,                // "profile drift on stage 2", "small dataset: no adaptation", "resumed: 2 stages seeded, 1 probed", "device OOM retry on stage 3"; the facade appends them to the report's notes (04 d.1 RunMeta.controller_notes)
}
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Bottleneck { IoRead, Memory, Sink, Compute, CpuQuota, Idle, StateGrowth }
```

### d.2 Consumed

`moruna_kernel::{Knobs, Knob, KnobSnapshot, StatsSource, SchedulerStats, StageStats, Prober, ProbeResult, SizerKind, Sampler, Sample, TraceTail, TraceRecord, Outcome, Placement, TierBudgets, TierKind, Limits, MorselFeatures, KernelHints, KernelKind, Fingerprint, SourceSchema, StageId, Seq, MorunaError}`; `serde_json` for profiles; `blake3` for profile keys. No `moruna_discovery`, `moruna_trace`, `moruna_placement` or `moruna_scheduler` dependency.

## e. Data model, formats and state machines

### e.1 Per-stage state

```rust
struct StageCtl {
    stage: StageId, a_k: f64, a_k_dev: f64, safety: f32,
    a_anon: f64 /* f.3, fitted to mem_anon_peak */, a_anon_seed: f64 /* the probe's value */,
    anon_ratios: VecDeque<f64> /* last 32; a_anon is their maximum */,
    target: u64, envelope: Envelope,
    completions_since_adjust: u32, last_adjust_sign: i8, flips_window: VecDeque<i8> /* last 100 */,
    frozen_until: Option<u64 /* completions */>,
    peak_ewma: f64, wall_ewma: f64,
    state_bytes: u64 /* f.3 state term */, device_breaches: u8 /* f.11 */,
    sizer: Box<dyn Sizer>, sizer_clamps: u32, sizer_proposals: u32,
}
```

### e.2 Controller state machine

`Created` → `Prepared` (baseline, budgets) → `Probed` (all `A_k` set, or nothing to probe) → `Running` (tick thread) → `Stopped`. `on_record` before `Running` is buffered and processed at start (the probe records arrive here).

### e.3 Profile file

`<profiles_dir>/<fingerprint hex>-<schema hash hex>.json`:

```json
{ "version": 1, "fingerprint": "...", "schema_hash": "...", "updated": "2026-09-15T18:00:00Z",
  "a_k_p50": 4.2, "a_k_p95": 5.9, "a_k_dev_p95": 0.0,
  "a_k_samples": 41200, "a_k_var": 0.31,
  "state_bytes_max": 0,
  "final_target": 67108864, "final_workers": 8, "final_safety": 1.25,
  "runs": 3, "prediction_error_p95": 0.18 }
```

`a_k_samples` is the number of trace records the statistics were computed from, summed across runs; `a_k_var` is the running variance of per-record `peak_delta / bytes_in` (Welford, merged across runs); `state_bytes_max` is the largest `state_bytes` any instance reported. These three are what make the safety margin a function of evidence (f.2) rather than a constant: a kernel seen forty thousand times and a kernel seen once do not deserve the same margin.

Unknown `version` → ignored with a note. Written at `stop` when the run completed (not on termination), merging with the existing file by EWMA (weight 0.3 to the new run).

## f. Algorithms and policies

One bound to keep in view throughout this section: `TraceTail::tail(stage, n)` returns records from the trace writer's in-memory chunks only (04 f.3), so `n` is capped in practice by `trace.memory_limit` (preamble section 5) and a larger window silently returns fewer records, not an error. Every rule below that reads a window states what it does with a short one, and none may read a short answer as evidence that a stage was idle.


**f.1 Budgets (`prepare`).** The host budget is the arena's capacity, `cfg.arena_bytes`, and nothing is subtracted from it here. The arena is where morsels live and its accounting is what enforces G-I1, so the controller's allowance for *allocating* morsels is exactly the bytes the arena can hand out. It is not, however, the allowance for what a kernel allocates outside the arena: those bytes are added to the process on top of the arena's own resident pages, and f.3's second inequality is what bounds them against the ceiling. `budget.reserve_fraction` is the numerator of that inequality rather than a margin the design hopes covers the case (f.3, architecture section 8). `host_budget = cfg.arena_bytes`; if ≤ `morsel_min × 2`, `Config { name: "budget.host", msg }` naming the ceiling, the baseline and the reserve the facade sized it from. `baseline = cfg.baseline_bytes`, the process's anonymous memory **before the arena was created**, which the facade samples there and passes in; `reserve = reserve_fraction × limits.memory_ceiling`. Both are reported in `Budgets` and neither is subtracted again. Rationale (PM, 2026-09-22, on the first end-to-end run): the arena touches every page of its region at `new` (02 f.1), so a baseline sampled after the arena exists already contains the arena, and `ceiling − baseline − reserve` charged the arena twice, leaving the controller zero at any ceiling for an arena sized at the budget. The arena's capacity is `ceiling − baseline − reserve − expected kernel state`, computed by the facade (12 f.1, 02 f.1); expected kernel state is the sum of `KernelHints::state_bytes` over the stateful instances that will be created, defaulting to zero where a kernel declares none. `prepare` still takes a sample, for `throttled_us` and the tick clock, but not for the budget. Device: `device_budget[d] = 0.9 × devices[d].free_bytes` after init (kernel weights are loaded). Split for placement: one host pool, on the tier the arena actually has (contracts e.1): `TierBudgets { pinned_host: 0.5 × host_budget, host: 0 }` when `cfg.pinned`, else `{ host: 0.5 × host_budget, pinned_host: 0 }`; `device: 0.6 × device_budget`; `disk: disk_budget`. The other half of the host budget is the workers' in-flight allowance. This split is the initial one; f.6 adjusts the queue share through `Knob::HighWater`, never through a placement setter; `set_budgets` is the only placement call the controller makes, here and when `state` changes (f.3).

**f.2 Probe (`probe_all`).** For each stage in order: seed `a_k` from the profile if present (p95) else from `hints.expected_amplification` else 4.0; choose the probe size: `probe_bytes`, or `hints.preferred_rows × bytes per row` when `preferred_rows` is set (bytes per row = `plan.total_bytes / plan.total_rows` for stage 1, and `bytes_in / rows_in` of the upstream stage's `ProbeResult` for later stages), clamped to `[morsel_min, morsel_max]`; call `prober.probe(stage, bytes)` (the scheduler runs it with one worker, SC f.9); `a_k = max(peak_delta / bytes_in, 0.5)`; if a profile existed and `|a_k − profile.a_k_p95| / profile.a_k_p95 > 0.5`, note "profile drift" and use the measured value; `a_k_dev` likewise from `dev_peak_delta` when `hints.uses_device_memory`; `safety` from the evidence: with no profile, `safety_initial`; with a profile, `safety = clamp(safety_floor + k × sqrt(a_k_var) / a_k_p50 + c / sqrt(a_k_samples), safety_floor, safety_initial)` with `k = 2` and `c = 4` (so 16 samples add 1.0 to the margin and 40,000 add 0.02), which starts a well-known kernel near the floor and an unseen one at the initial value; drift (above) resets `safety` to `safety_initial` for the run. The margin never goes below `safety_floor`; the guarantee is not relaxed, only the margin shrinks with evidence. `ProbeResult.wall_ns` and `cpu_ns` seed `wall_ewma` and the first Compute/IoRead classification.

*What the ceiling counts, and the floor under S1 (open, N-7 on the board).* The figure S1 is measured from is what the process has resident, which includes pages an allocator is holding for its own reuse rather than using. pyarrow allocates through jemalloc on Linux and jemalloc returns nothing until it is asked, so a caller that has built and freed a dataset can reach `run` resident in hundreds of megabytes it is not using, and the arena is then sized from that. Measured: a runner reached this suite's 512 MiB case holding 429 MB, which left 53 MB above the arena for a kernel needing 84 MB per morsel (2026-09-23). That is the caller's memory and the caller can hand it back, which is worth saying in the refusal as well as in the README. Underneath it is a bound nothing can close: the probe costs what one morsel of the kernel costs, because it is one, so a budget smaller than that measurement is passed while measuring, before any figure the controller could refuse on exists. Whether S1 should state that bound is open.

**f.3 Working-set equations and initial knobs (`start`).** Let `W = workers_max`, `S` the number of kernel stages.

There are two budgets and they are not the same budget. `arena_bytes` is what the runtime may hold; `limits.memory_ceiling` is what the *process* may hold, and it is the second that S1 is measured from: "the process's peak anonymous memory is at or below the budget, measured and not estimated". A model fitted only to the first does not bound the second. 02 f.1 touches every page of the region at `new`, so the process holds `resting_anon = baseline + arena_bytes` from the moment the arena exists, and every byte a kernel allocates for itself inside `apply` is added on top of that rather than carved out of it. Charging those bytes against the arena's capacity permits a process anonymous peak of `resting_anon + (arena_bytes − state) / 2`, which is over the ceiling by construction: that is what the first Python program measured, 1.11 × a 512 MiB ceiling on a completed run, while the controller shed a worker on every one of the ten records it had (PM, 2026-09-23). The controller had seen the overshoot and had reacted; morsel size and worker count were simply not the terms that bound the quantity the criterion measures.

So `start` solves both inequalities of b and writes only a set that satisfies both.

*The state term.* `state(stage) = instances_live × max(hints.state_bytes, profile.state_bytes_max, max(state_bytes over the stage's last 32 records grouped by instance))`, and `state = Σ state(stage)`: the memory instances hold regardless of morsel size; it is subtracted from both budgets before anything else and updated at every tick from the trace tail (the `instance` column, contracts d.13, identifies which instance grew).

*The arena inequality.* `budget_for_stage = (arena_bytes − state) / 2 / S`. The read-ahead's bytes are spent out of the queue half rather than on top of it (they land in Q0), so the division is `state + (arena_bytes − state) / 2 for the workers + (arena_bytes − state) / 2 for the queues and the read-ahead together`, which is what makes b's first inequality an accounting of the arena rather than a hope. `share = active_workers / S` (initially `W / S`), capped by the morsels the dataset has at that target. For each stage, `allowance = target × a_k × safety`.

*The anonymous inequality.* `anon_headroom = ceiling − resting_anon`, `for_kernels = kernel_anon_share × (anon_headroom − state)` and `anon_for_kernels = for_kernels − Σ_stages c_anon`. For each stage, `anon_allowance = target × a_anon × safety`, and `Σ_stages (c_anon + share × anon_allowance) ≤ for_kernels`. **Not all of the headroom is the kernels' to be planned into**, which is the correction of 2026-09-23: a Parquet writer's encoder, the Arrow builders a batch is converted through and the interpreter's own growth are all out-of-arena bytes, none of them a morsel, none of them in the fit, and every one of them in the figure S1 measures. A model that divides the whole headroom among morsels spends the reserve twice, once as the margin the arena was sized to leave and again as the allowance it hands the kernels, and what the runtime then takes for itself comes out of the ceiling. `kernel_anon_share` is 0.6, measured on the Parquet to Python to Parquet job at a 512 MiB ceiling over a sweep of resting footprints from 0 to 340 MB: the unmodelled remainder there runs to about a fifth of the headroom, and with the whole headroom planned into, peaks of 0.98 to 1.01 of the ceiling were ordinary and two Linux runs passed it at 1.005 and 1.012. With the share withheld the same sweep peaks at 0.97 and the runs that no longer fit are refused rather than run. It is also the cushion f.7's breach line assumes: a record arrives after the `apply` that produced it, so whatever that morsel cost above its prediction is in the process before the controller can answer, and a plan that fills the headroom exactly leaves that surprise nowhere to land. **`c_anon` is charged once and `a_anon` per byte in flight**, because the process's out-of-arena residency has a part that scales with the morsel and a part that does not, and only one of them answers to the morsel size. With the facade's arena sizing (12 f.1: a share `2 / (2 + a_anon)` of `ceiling − baseline − reserve − declared kernel state`) the headroom is the reserve, that declared state and the share the facade withheld, so `budget.reserve_fraction` stops being an allowance the design hopes is enough and becomes one term of an inequality the controller solves. Until 2026-09-23 the facade gave the arena the whole allowance and the reserve was the only term there was, which refused an ordinary Python job below about 2 GiB.

*The fit.* Both terms come from the two memory columns of a trace record, which answer two different questions. `mem_anon_before` is the process's anonymous residency before this morsel's `apply`, so whatever of it stands above `resting_anon` and above the funded `state` was not caused by this morsel: that is `c_anon`, **measured and not inferred**. `mem_anon_peak − mem_anon_before` is the growth across the morsel, which is the term that scales, and it carries `a_anon`. Their sum is `mem_anon_peak − resting_anon − state`, the quantity S1 is measured from, so a pair that bounds both bounds it. Each reading is kept with the bytes the model believed were in flight, `x = share × max(target, bytes_in)`, which is the unit the inequality multiplies back; `bytes_in` alone, as `A_k` uses, reads a one megabyte row group against a four megabyte target as four times too expensive, which at a 1 GiB budget was the difference between a run that fitted on one worker and a run that terminated on the first record. Over the last 32 records: `a_anon` is the least-squares slope of the **growth** against `x` when the window holds at least four readings and a spread of 5% in `x`, and otherwise the largest `growth / x` in it, which is the conservative reading of a window that cannot resolve a line; the floor is `MIN_AMPLIFICATION`, so a morsel is never free. `c_anon` is then, at every reading, the larger of the residency measured before `apply` and whatever the reading needs above the slope's line, so the pair bounds every point in the window, which is what RC-I1 asks of the sizing, and it never prices the fixed part below what the process was actually holding. A window rather than the whole run, because a fit that could only rise would leave a kernel sized for its worst morsel forever and f.4's margin, which tightens only on an accepted increase, would never tighten. `c_anon` is the same kind of quantity as the state term and is charged the same way, once and before anything is multiplied: `KernelHints::state_bytes` and `TraceRecord::state_bytes` carry it for a kernel that can declare it, and `c_anon` is the part no one declared. The two do not double count, because the fit is of what is left above the funded state. It is not written to the profile (e.3): a resumed run measures it again on its first record.

Rationale (PM, 2026-09-23). The fit was one term: the whole of `mem_anon_peak − resting_anon` divided by `x`. A Python job's 86 MiB of allocator and interpreter retention was therefore charged to every morsel byte, `a_anon × safety` reached about 42, and `floor_footprint` became 176 MB for a 4 MiB morsel against 152 MB of headroom. Because halving the target halves `x` while that numerator does not move, **the refusal threshold rose as the morsel shrank**: the controller's only remedy made its own estimate worse, which is the opposite of a control loop. Fitting a line to the whole reading was tried and rejected: within one run every record arrives at the same target and the same worker count, so `x` does not vary and the two terms are not identifiable at all, and the job this exists for is ten morsels long and sits at `morsel_min` from its first record, so no spread ever appears. `mem_anon_before` needs no spread, which is why the constant is read from it. Measured on that job's own trace over sixteen configurations of budget and CPU quota, 160 records (`trace=` a path): the fixed part is about 86 MiB and the slope a little under one byte per byte in flight. Holding out each configuration and predicting its peak from the other fifteen, this fit's mean relative error is 0.42 against the one-term form's 1.76 and its median 0.44 against 1.11, both bounding the held-out peak in 15 of 16 cases. Predicting each record from the ones before it inside a single run, which is what the controller actually does, it bounds the next morsel's peak in 117 of 144 cases against the one-term form's 45. End to end, the job completes inside 512 MiB at 0.645 to 0.674 of the ceiling over eight runs, where the one-term form refused it with `footprint 186002119 exceeds budget 152665344`. The breach path performs the same refit itself (f.7), because the tick thread has not seen the record yet and the point of f.7 is to be smaller by the time the next morsel is picked up.

*The solve.* Solve for one common target `t` such that both inequalities hold, and for device stages `share × t × a_k_dev × safety ≤ device_budget × 0.4`; then clamp per stage: `target(stage) = clamp(t, morsel_min, morsel_max)`. Set `active_workers` per RC-I7, which is the smaller of the two bounds. `split_bytes = target[1]` when `plan.sub_splittable_all`, else `max(target[1], plan.max_split_bytes)` (a split that cannot be sub-split arrives whole). Set `read_ahead = 2`, subject to `read_ahead × split_bytes` fitting the queue half, else lower it to what fits, floor 1. Set `promotion_window = 2`. Set high water per queue and tier from the placement half of the arena divided equally across queues (`Knob::HighWater { stage, tier, bytes }`; the scheduler derives low = high / 2, contracts d.11). Write knobs in the order: high waters, promotion windows, morsel targets, active workers, read-ahead (RC-I1 check before the batch). With zero kernels (`S = 0`, 12 h): skip probes, write `MorselTarget { stage: 0, bytes: min(morsel_max, arena_bytes / 2) }` (the source drive reads at it, SC f.5), `ActiveWorkers(1)`, `ReadAhead(2)`; `share = W / max(S, 1)` everywhere.

*The envelope.* Both inequalities bound it (b), and it is recomputed whenever a record moves `a_anon`. A stale envelope is not a harmless one: it clamps the sizer to the target it was computed from, so a stage whose measured cost has come down would be held at the size it had when the probe spoke, and f.4's increase would never be accepted.

**f.4 `RuleSizer` (per record of stage s).** `observe`: update `peak_ewma` (α = 0.2) of `peak_delta / bytes_in`. `propose`: if `obs.completions_since_adjust < obs.damping` return `obs.target`; let `ratio = peak_ewma × safety × target / allowance_target(stage)` where `allowance_target = target_fraction × envelope.max × a_k × safety` (i.e., how much of the allowed peak the stage is using); if `ratio < 0.85` propose `target × (1 + increase_step)`; if `ratio > 1.0` propose `target / 2` (this is the AIMD; the immediate breach path in RC-I4 is separate and stronger); else propose the current target; `predicted_peak = None`. Also tighten `safety` toward `safety_floor` by 0.02 per accepted increase when the last 20 records' peak ratios have coefficient of variation under 0.15.

**f.5 Applying a proposal.** Clamp to the envelope (count a clamp if changed); if frozen, ignore; if the sign of the change flips relative to `last_adjust_sign`, push to `flips_window`; if flips in window > `oscillation_flips` → freeze (RC-I5); else run the working-set check (RC-I1) with the new target and write `Knob::MorselTarget`; reset `completions_since_adjust`.

**f.6 Bottleneck classification (each tick).** Inputs, all from `stats.scheduler_stats()` (contracts d.11) unless said otherwise: `busy = workers_busy / workers_active`, `q0_bytes` and `qn_bytes` from the trace tail's `q_bytes_after`, `reads_in_flight`, `qn_bytes` trend over the last 4 ticks, `writes_in_flight` against `sink_concurrency`, `throttled_delta` over the tick from the sampler, anon sample vs ceiling, `state` from f.3.

| Condition (evaluated in order) | Class | Action (one only) |
|---|---|---|
| anon ≥ ceiling − reserve × 0.5 and `state` grew by more than 10% since the last tick | StateGrowth | re-run f.3 with the new `state` (targets and workers shrink to fund it) and `set_budgets` with the smaller placement half; if `state` alone exceeds `host_budget / 2`, `knobs.terminate(Budget { seq, stage, footprint: state(stage), budget: host_budget / 2, features })` naming the stage (a kernel whose state grows without bound is not sizeable) |
| anon ≥ ceiling − reserve × 0.5 | Memory | halve the largest stage target; raise its safety; `Knob::StagingTrigger { stage: n, on: true }` if not already on (the scheduler forwards it to `set_staging`, contracts d.11) |
| throttled_delta / tick > 10% | CpuQuota | `ActiveWorkers −1` (min 1) |
| busy < 0.7 and q0_bytes < low(q0) and reads_in_flight == read_ahead | IoRead | `ReadAhead +1` (max 64) if the working set allows; else shrink queue high waters by 10% (`Knob::HighWater`) to fund it |
| busy < 0.7 and q0_bytes ≥ low(q0) | Memory (workers parked by budget) | shrink queue high waters by 10% (`Knob::HighWater`) and re-run f.3's worker formula (`ActiveWorkers +1` if it now allows) |
| busy > 0.9 and qn_bytes rising 4 ticks and writes_in_flight == sink_concurrency | Sink | `Knob::StagingTrigger { stage: n, on: true }`; `ReadAhead −1` (min 1) |
| busy > 0.9 | Compute | none; record |
| otherwise | Idle | none |

The class and tick time are appended to the timeline (compressed: consecutive equal classes are one entry with a duration).

**f.7 Breach handling (RC-I4).** The breach line is `resting_anon + 0.5 × anon_headroom`, capped at the ceiling.

It is not `ceiling − reserve`: f.1 sizes the arena at `ceiling − baseline − reserve − kernel state` and 02 f.1 touches every page of it at `new`, so `resting_anon` is the process's resting anonymous memory and a line there sits exactly on it, so every run would breach on its first record, which is what the first end-to-end run did (PM, 2026-09-22). And it is no longer `resting_anon + reserve`, which is the ceiling: that line is the first signal the controller gets, and by the time it arrives S1 has already failed, so the halving is a post-mortem rather than a guard. The line sits inside the headroom instead, at half of it, which leaves the other half as the cushion the reaction has to work in and puts it where f.6's memory rows already fire.

On `on_record` with `mem_anon_peak` above the breach line or `dev_mem_peak` above the device budget:

1. Refit both terms of f.3 from the record. A host breach is the model being wrong about what the kernel costs the process, and every decision below is made on the corrected figures. The constant comes from `mem_anon_before` and is therefore corrected on this record alone, without waiting for the halving in step 2 to put a second morsel size into the window: a run that breaches at `morsel_min` has no second size to offer and is exactly the run that needs the correction.
2. Halve that stage's target, raise its safety by 0.2, write the knob, refresh the envelopes. Outside the damping and outside the freeze: a frozen stage that is breaching is exactly the stage that must move.
3. Set `active_workers` to what the anonymous inequality now allows, in one step. Not one per breach: one per breach is a reaction slower than a short run, and the run that made this explicit went from ten workers to two over ten morsels and then ran out of morsels, so it neither fitted nor terminated.
4. If the stage is at `morsel_min`, `active_workers` is 1, and one morsel of the floor per stage still does not satisfy the anonymous inequality, `knobs.terminate(MorunaError::Budget { seq, stage, footprint, budget, features })` with `footprint` the out-of-arena bytes that smallest set would cost, `state + Σ_stages (c_anon + morsel_min × a_anon × safety)`, and `budget` the anonymous headroom it did not fit (G-I8's diagnostic; the scheduler enters `Terminating`, SC f.8). The fixed term is what makes this a floor at all, because it is the cost that shrinking the morsel cannot remove, and it is the honest reason a run ends. While it rode in the slope this figure rose every time the target halved, so the refusal it gates became easier to trigger the harder the controller tried to avoid it (PM, 2026-09-23). Every knob the controller owns is at the end of its travel and no further record will change that, so the run ends here rather than finishing over the ceiling, which is a legitimate termination under S6. The two figures the message compares are the same kind of number; a per-morsel delta against an absolute line read as "footprint 835584 exceeds budget 536818484", which says nothing.
5. Otherwise the counter remains as the backstop, for the case the model cannot see: three breaches at the floor with one worker terminate the run, with the measured footprint above `resting_anon` against the line it crossed.

A run that would have completed over budget fails. A criterion that moves to meet what the runtime happens to do is not a criterion.

**f.8 Learned sizer fallback (RC-I2).** `LearnedSizer` proposals are subject to the same clamp; the controller counts clamps per sizer and tracks `prediction_error = |predicted_peak − observed_peak| / observed_peak` per record for the active sizer (from `Proposal.predicted_peak`; a `None` prediction counts as an error of 1.0) and for a shadow `RuleSizer` (whose prediction is `peak_ewma × bytes_in`); after 20 proposals, if `sizer_clamps / sizer_proposals > 1 / fallback_error_ratio`, or if the learned sizer's rolling error p95 exceeds `fallback_error_ratio × RuleSizer`'s, swap to `RuleSizer` for the run, record `fallback_at`, and add a note.

**f.9 Profile write (`stop`).** The run completed when the source is exhausted (`SchedulerStats::source_exhausted`, contracts d.11) and the controller did not terminate it; `stop` takes no argument and that is how it knows. A terminated run's figures describe a kernel that did not fit, and storing them would teach the next run the wrong lesson, so they are not stored. If the run completed: merge per e.3 and write atomically (temp file + rename). If `profiles_dir` is None or unwritable: skip with a note. When `checkpoint_enabled`, the profile is also written every `checkpoint_interval_ms` from the tick thread after the first 50 records per stage, with the same merge, so a run that dies keeps what it learned; f.14 reads it back.

**f.10 Tiny dataset fast path.** If `plan.total_bytes < arena_bytes / 4`: skip probes, set every target to `min(morsel_max, the largest target the anonymous inequality allows)`, `active_workers = W` subject to RC-I7's anonymous bound, `read_ahead = min(plan.splits, 8)`, and add the note "small dataset: no adaptation". (Architecture section 8.) RC-I1 still applies to the knobs this path writes, because `morsel_max × a_k × safety` per worker can exceed the budget even when the whole dataset does not: the in-flight count per stage is `min(share, ceil(total_bytes / target))`, since a dataset of two morsels cannot put one on each of eight workers, and both working-set checks run on the result as they do everywhere else. What this path skips is the probe and the adaptation, never either inequality: the run measured on 2026-09-23 reached 1.11 × its ceiling on a *tiny* dataset, with every worker running at `morsel_max`. The target is capped rather than the worker count, because the two are not equivalent: a 512 MiB morsel at four times its input funds one worker, and the same headroom funds all eight at 34 MiB.

**f.11 Device out of memory (architecture 7).** The scheduler retries an `Alloc { tier: Device(d) }` error from `apply` once on the same morsel, after the record hook has run (SC f.8). In that hook, on the first such record for a stage (`outcome == Error`, `error` names `alloc` in a `Device` tier): halve the stage's target (bypassing damping, as f.7 does), write `Knob::HighWater { stage: stage − 1, tier: TierKind::Device, bytes: 0 }` so the engine demotes promoted-but-unconsumed morsels of the input queue back to the host tier, set `device_breaches[stage] = 1`, and add the note "device OOM retry on stage k"; the next tick restores that high water from f.3. On a second such record for the same stage, `knobs.terminate(Budget { seq, stage, footprint: dev_mem_peak, budget: device_budget[d], features })` with the device figures in the diagnostic.

**f.14 Resume (`probe_missing`).** Called by the facade instead of `probe_all` on a resumed run, after `prepare` and after the scheduler's `apply_resume_point` (SC f.13). For each stage: if the profile store has an entry for its fingerprint (written by f.9 during the interrupted run, or by any earlier run), seed `a_k` and `a_k_dev` from it and skip the probe; otherwise probe as in f.2. Then `start` as usual: the working-set equation runs on the seeded values, so a resumed run begins at the sizes the interrupted run had learned rather than at `probe_bytes`. The controller does not read the manifest; the profile store is its memory, keyed by kernel fingerprint, and it is deliberately node-independent (a directory the operator can place on the same durable volume as the staging directory; `profiles.dir`). The note "resumed: N stages seeded, M probed" is added. A stage with no profile on a run whose plan the interrupted run consumed cannot be probed at all: the scheduler's helper has no range left to read and answers `Plan("the source plan is exhausted; there is nothing to probe with")`. That is not a failure of the resumed run and must not end it. `probe_missing` catches it, seeds `a_k` from `hints.expected_amplification` or the 4.0 default as f.2 would without a probe, and adds the note "resumed: stage k could not be probed, the plan is exhausted; seeded from hints" (PM, 2026-09-22: the first resume failed here, because `profiles.dir` defaulted to `None` and so no profile had been written; 12 f.1 now resolves the preamble's `~/.moruna/profiles` default, and this rule is what remains if it can still happen). `probe_all` on a fresh run keeps the error: a fresh run with nothing to read is a plan error.

## g. Concurrency within the component

One tick thread. All mutable state (the per-stage table of e.1, the phase of e.2, the sizer, the budgets, the notes) sits in a single `Mutex<ControllerState>`; every public method takes `&self` and locks it, so the facade shares the controller as `Arc<Controller>` and installs `on_record` as the scheduler's record hook before it calls `prepare` (12 f.1), with no `&mut` borrow to conflict with the hook. `on_record` runs on the recording worker (it is the scheduler's `RecordHook`) and pushes a compact copy of the record's numeric fields into a bounded lock-free queue (capacity 65,536; overflow drops the oldest with a counter, since the trace is the durable copy and the controller only needs recency) drained by the tick thread. All knob writes happen on the tick thread except the breach path in f.7 and the device path in f.11, which run on the calling worker thread under the controller's single mutex (held for microseconds; lock order position 6 in preamble 4.2, after the trace channel at 5, since `on_record` is called after `record`). No lock is held across any call into another component: the mutex is released before `Knobs::set`, `stats.scheduler_stats()`, `sampler.sample()`, `trace.tail()` and `placement.set_budgets()`, and a tick's decisions are computed from copies taken under the lock (RC-I10).

## h. Behaviour

**Normal path (compute-bound tokeniser, 8 cores, 8 GiB ceiling).** Baseline 400 MiB; reserve 800 MiB; host budget 6.8 GiB; probe measures `a_k = 7`; f.3 sets `target ≈ 6.8 GiB / 2 / (8 × 7 × 1.5) ≈ 40 MiB`, `active = 8`; ticks classify Compute; the sizer nudges the target to ~60 MiB as safety tightens; steady state ~85% of ceiling in use.

**Normal path (IO-bound identity from S3).** `a_k ≈ 1`; targets hit `morsel_max`; busy < 0.7; ticks classify IoRead and raise read-ahead toward the bandwidth-delay product until busy rises or the working set is exhausted.

**Adversarial (S6).** At the midpoint `a_k` jumps 4×; the next record breaches at half the anonymous headroom, with the other half still between it and the ceiling; f.7 refits `a_anon`, halves the target, raises safety and drops the worker count to what the anonymous inequality allows; the target reaches the level the new figures allow; the run completes. If one `morsel_min` on one worker does not satisfy that inequality, the run terminates with the diagnostic naming the morsel, the footprint and the headroom it did not fit, which is the outcome for a kernel whose out-of-arena footprint is larger than the anonymous headroom 12 f.1 left it however the knobs are set.

**A Python kernel at a tight budget (measured, 2026-09-23, re-measured the same day under the corrected sampler and 12 f.1's arena share).** 200,000 rows, 20,000 to a row group, a kernel that builds a Python list of 20,000 strings and a pyarrow array from it, on a ten-core host. Before f.3 had the second inequality, both budgets completed and the 512 MiB one peaked at 1.09–1.11 × its ceiling. With the inequality and the arena taking the whole allowance, the 512 MiB run terminated with 51 MiB of headroom and the 2 GiB run completed at 0.94 of the ceiling. With 12 f.1's arena share the 512 MiB headroom is 138 MiB and the 2 GiB run completes at 0.36. The 512 MiB run still terminates, and for a reason outside this component: the Parquet sink's default buffer asks for 129 MiB and so costs a 256 MiB arena size class, which is what sets 12 f.1's floor and holds the arena at 288 MiB. Given a sink whose buffer does not cross a class (`row_group_bytes` 16 MiB, floor 160 MiB) the same job completes inside 512 MiB at 0.53 of the ceiling, so the budget does hold the job and the sink is what stops it.

**Edge cases.** A stage whose `a_k` < 0.5 (a filter that drops most rows): clamped to 0.5 so the allowance stays conservative. A pipeline with one stage: `share = active_workers`. Zero kernels: f.3's zero-kernel path; no probes, one active worker, no ticks change anything but read-ahead. Device stage with no device present (feature off): `Config` at `prepare`. `morsel_max` smaller than the probe size (misconfiguration): probe at `morsel_max`. A single row larger than `morsel_max` (SC f.5): the record's `bytes_in` exceeds the target; the allowance is computed from the observed `bytes_in`, `active_workers` drops to what the budget allows for it (possibly one), and the note "row larger than morsel target on stage k" is added once.

**Failures.** Sampler stalls (`Sample.at_ns` unchanged for 100 consecutive ticks; the contracts' sampler never returns an error, DS-I3): `knobs.terminate(Config { name: "sampler" })`. Profile file corrupt: ignore with a note. Knob write when the scheduler has exited (race at completion): ignored (the scheduler's `set` is a no-op after exit, SC f.15).

## i. Configuration

All `controller.*`, `budget.*`, `morsel.*`, `workers.active`, `readahead.splits`, `queue.*`, `profiles.dir`, `sizer`, `sizer.fallback_error_ratio` rows (preamble section 5). The controller writes knobs inside the ranges; the scheduler clamps and counts anything outside them (SC f.15), and `budget.*` clamping belongs to discovery.

## j. Observability

`ControllerSummary` (timeline, sizer, fallback, freezes, breaches, final knobs, notes) into the run report; `tracing`: `ctl.budgets` (info), `ctl.probe` (info per stage: `a_k`, `a_k_dev`, probe bytes), `ctl.adjust` (debug: stage, old, new, reason), `ctl.breach` (warn), `ctl.device_oom` (warn), `ctl.freeze` (warn), `ctl.class` (debug per tick), `ctl.fallback` (warn), `ctl.terminate` (error), `ctl.tick_skipped` (warn: a sample over 5 ms).

## k. Tests

Tests drive the controller with the contracts d.15 fakes and name only their knobs: `FakeKnobs` (`stats(SchedulerStats)`, `probe_result(stage, ProbeResult)`; observables `writes()`, `terminated()`) as `Knobs`, `StatsSource` and `Prober`; `FakeSampler::scripted(Vec<Sample>)`; `FakeTrace` (`capacity(n)`) as `TraceTail`, fed synthetic `TraceRecord`s by the test through `TraceSink::record` and delivered to `on_record` by the test in the same order; `FakePlacement` for `set_budgets`.

**RC-T19 allowance_is_the_arena.** `prepare` with `arena_bytes` set and a baseline sample the fake sampler reports larger than the arena returns `Budgets.host == arena_bytes`, `Budgets.baseline` equal to `baseline_bytes` and a non-zero morsel target from `start`; the same configuration under the old `ceiling - baseline - reserve` rule leaves nothing. f.1.

**RC-T21 anon_fit_separates_fixed_from_scaling.** Over windows of `(bytes in flight, residency before `apply`, growth across `apply`)` readings synthesised from the measured shape of the job in `python/tests/test_budget.py`, `fit_anon` recovers both terms and the pair bounds every reading in the window. A window with no spread in `x` at all, which is every run that never leaves one morsel target, still recovers the constant, because it is measured rather than fitted. The one-term form prices the floor morsel more than five times higher from a window of small morsels than from a window of large ones, for the same morsel, and prices a large morsel at more than three times its cost, which is the refusal; the two-term fit gives the same constant either way and the two answers for the floor are within half of each other. With the spread one halving produces, the slope is fitted too and the pair answers within a megabyte at every morsel size, the floor and four times the largest reading included. Fewer readings than the slope needs leaves the probe's seed governing the slope with the constant still measured; a cost that does not scale at all fits a zero slope and is held at `MIN_AMPLIFICATION`; an empty window is the seed. f.3, f.7, RC-I1.

**RC-T20 resume_without_a_profile.** `probe_missing` with no profile for a stage and a `Prober` that answers `Plan("the source plan is exhausted; there is nothing to probe with")` reaches `Probed`, seeds `a_k` from the stage's hint, and adds the note naming the stage; `probe_all` with the same prober returns the error. f.14.

**RC-T21 anon_bound_on_a_greedy_kernel.** (`python/tests/test_budget.py`, on a built wheel) A Python kernel that builds a list of 20,000 strings and a pyarrow array from it, 200,000 rows at 20,000 to a row group. At 512 MiB the run either completes with `peak_fraction_of_ceiling ≤ 1.0` or raises `moruna.BudgetError` whose message names the morsel, the stage, a footprint and a budget the footprint exceeds; at 2 GiB it must *complete* with `peak_fraction_of_ceiling ≤ 1.0`, which is what stops the bound being a refusal of everything. Both runs are driven in a subprocess, because the arena is sized from the process's anonymous memory before it exists and a test sharing a process with twenty earlier runs measures their allocator retention instead. Against the wheel of 2026-09-22 the 512 MiB case fails at 1.09–1.11. f.3, f.7, S1, S6, G-I1, G-I8.

**RC-T1 consistent_knobs.** Property test: for random budgets, `a_k`s and worker counts, every written knob set satisfies both working-set inequalities of b. RC-I1.

**RC-T2 envelope_clamp_and_fallback.** A test `Sizer` proposing 10× the envelope with `predicted_peak = Some(envelope.max)` is clamped every time and replaced by `RuleSizer` at the 21st proposal (more than half clamped at ratio 2.0); a second test sizer that stays inside the envelope but predicts half the observed peak is replaced once its error p95 exceeds twice the shadow rule sizer's; `fallback_at` is set and the note appears in `ControllerSummary.notes`. RC-I2, f.8.

**RC-T3 damping.** Increases per stage never closer than `damping` completions; decreases are exempt, because the breach halving and the RC-I1 repair after a refit of `a_anon` are both immediate by design; `Observation.completions_since_adjust` and `damping` equal what the controller enforces. The growth round runs long enough to fill f.3's 32-record anonymous window, since until it has the probe's seed governs `a_anon` and there is nothing for the increase to grow into. RC-I3.

**RC-T4 breach_immediate.** A record with `mem_anon_peak` over `resting_anon + 0.5 × anon_headroom` halves the target before the next tick (a `MorselTarget` write appears in `FakeKnobs.writes()` from inside `on_record`); the first breach at the floor with one worker whose refitted `a_anon` puts the floor outside the anonymous headroom produces `FakeKnobs.terminated() == Some(Budget { .. })` carrying seq, stage, footprint, budget, features, with the footprint the larger of the two figures the message compares. RC-I4, G-I8.

**RC-T5 oscillation_freeze.** A synthetic trace alternating high and low peaks; flips exceed the threshold; the target freezes at the geometric mean for `freeze_morsels`; S11's bound holds over the run. RC-I5.

**RC-T6 probe_first.** With `FakeKnobs::probe_result(k, ..)` for each stage, no `MorselTarget` knob other than `probe_bytes` is written before `probe_all` returns; `probe_all` calls `probe` once per stage in order; with `hints.preferred_rows` set on stage 2, the requested bytes equal `preferred_rows × bytes_in / rows_in` of stage 1's result. RC-I6, f.2.

**RC-T7 workers_bounded_by_memory.** Large `a_k` and small budget → `active_workers` < `workers_max` by the formula, which is the smaller of the arena and anonymous bounds; the targets of three stages with different `a_k` are the one common `t` clamped per stage. RC-I7, f.3.

**RC-T8 one_action_per_tick.** Every tick writes at most one of read-ahead or workers. RC-I8.

**RC-T9 classification_table.** For each row of f.6, `FakeKnobs::stats(SchedulerStats { .. })` and `FakeSampler::scripted` describe that state and the tick produces that class and exactly that action in `FakeKnobs.writes()`: row 2 writes `StagingTrigger { stage: n, on: true }`, rows 4 and 5 write `HighWater`, the StateGrowth row appends a smaller `TierBudgets` to `FakePlacement.budgets_set()` (contracts d.15) and, past half the budget, `terminated()` is a `Budget` naming the stage. f.6.

**RC-T10 profile_roundtrip.** Write, read, merge with EWMA; corrupt file ignored; drift detection triggers the note. e.3, f.2, f.9.

**RC-T11 tiny_dataset.** `PlanSummary.total_bytes` under a quarter of budget → no `probe` calls, `read_ahead == min(splits, 8)`, the note, and the target the largest the anonymous inequality allows rather than `morsel_max`: at a 16 GiB ceiling with the 4.0 default and eight workers that is 34 MiB, and every worker is kept, which is the difference between capping the target and capping the worker count. f.10.

**RC-T12 end_to_end_budget.** (integration, closes in wave 4; container with `--memory`) All five benchmark kernels complete with peak anon ≤ ceiling through the Rust facade; adversarial completes or terminates cleanly. S1, S6, S2.

**RC-T13 throughput.** (reference host, E1) Defaults reach ≥ 80% of the hand-tuned baseline for normalise, tokenise-explode and wide-intermediate. S3.

**RC-T14 resume_seeds_from_profile.** With profiles for two of three stages, `probe_missing` calls `probe` exactly once and starts with targets computed from the seeded `a_k`s; with `checkpoint_enabled` and `checkpoint_interval_ms = 50`, a profile file exists after the first write past 50 records; the note reads "resumed: 2 stages seeded, 1 probed". f.14, f.9.

**RC-T15 safety_from_evidence.** With no profile, `safety == safety_initial`; with a profile of 16 samples and low variance, `safety` is near the initial value; with 40,000 samples and low variance, near the floor; with 40,000 samples and high variance, well above the floor; drift resets it. f.2.

**RC-T16 state_growth.** Synthetic records for a stateful stage with `instance` 0 and 1 whose `state_bytes` grow by 64 MiB per record, `instances_live = 2` in `FakeKnobs::stats`, and `FakeSampler::scripted` anon rising to match: the `StateGrowth` class fires, targets shrink to fund the state and `FakePlacement.budgets_set()` (contracts d.15) ends with the smaller half, and `terminated()` is a `Budget` naming the stage once `state` alone exceeds half the budget; no written knob set ever violates either working-set inequality. f.3, f.6.

**RC-T17 tick_lock_bound.** A `FakeSampler::scripted` sequence and a `FakeTrace` holding 100,000 records; 1,000 ticks; no call into `FakeKnobs`, `FakeSampler`, `FakeTrace` or `FakePlacement` happens while the controller mutex is held (each fake is wrapped by the test to assert the mutex is free), and the mutex is held inside the 5 ms bound. The bound is asserted on the distribution of holds (p99 inside it, median an order of magnitude inside it) and the longest single hold is reported rather than asserted: the longest is a wall-clock figure, and on a host with more runnable threads than cores it measures the scheduler, since the same twenty microseconds of arithmetic reads as milliseconds when the holder is descheduled in the middle of it. An implementation that does more work under the lock moves the whole distribution, which is what the assertion catches; a preempted holder moves only the tail, which is what the report carries as a provisional figure (preamble E1). RC-I10, preamble 4.2.

**RC-T18 device_oom_retry.** A record with `outcome == Error` and `error` "alloc 1 GiB bytes in Device(0)" for stage 3: `on_record` writes `MorselTarget` halved and `HighWater { stage: 2, tier: Device, bytes: 0 }` before returning, the note "device OOM retry on stage 3" appears, and the next tick restores the high water; a second such record for stage 3 gives `terminated() == Some(Budget { stage: 3, .. })` with `footprint == dev_mem_peak` and the device budget. f.11 (architecture 7).

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/budget.rs` (f.1), `src/probe.rs` (f.2), `src/model.rs` (working set, envelope, f.3), `src/sizer/{mod.rs (trait, Observation, Proposal, SizerOutcome), rule.rs (f.4), learned.rs (stub + shadow error tracking f.8)}`, `src/apply.rs` (f.5, RC-I1 check), `src/classify.rs` (f.6), `src/breach.rs` (f.7), `src/device.rs` (f.11), `src/profile.rs` (e.3, f.9), `src/tick.rs` (thread, queue drain, RC-I10 timer), `src/summary.rs`. No `unsafe`.

Numerics: all ratios in `f64`; bytes in `u64`; conversions saturating; never divide by `bytes_in == 0` (skip such records for sizing).

Anti-patterns: no knob write outside `apply.rs`, `breach.rs` and `device.rs`; no reading morsel payloads; no blocking on the trace writer (use `tail`, which is in-memory); no sizer that reads anything but `Observation`; no placement call other than `set_budgets`; no concrete crate other than the contracts.

## m. Open items

None. (The learned sizer's model is Phase 8; the stub and the shadow error tracking ship now so the fallback path is tested.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| S1, G-I1 | RC-I1, RC-I7 | RC-T1, RC-T7, RC-T12, RC-T21 |
| S2, G-I10 | f.3 (no user parameters) | RC-T12 |
| S3 | f.4, f.6 | RC-T13 |
| S6, G-I8 | RC-I4, f.7 | RC-T4, RC-T12, RC-T21 |
| S17 | f.9 (periodic write), f.14 | RC-T14 |
| S1, S6 (stateful kernels) | f.3 `state`, f.6 StateGrowth | RC-T16 |
| S3, S11 (margin from evidence) | f.2 safety derivation | RC-T15 |
| S11 | RC-I3, RC-I5 | RC-T3, RC-T5 |
| D2 | RC-I8, f.6 | RC-T8, RC-T9 |
| D3 | RC-I6, f.2 | RC-T6, RC-T11 |
| S1 (out-of-arena allocation; architecture 8) | f.3 anonymous inequality, f.7 | RC-T21, RC-T4 |
| D7 | RC-I2, f.8 | RC-T2 |
| G-I5 | (sole writer, by construction of d.1) | RC-T1 |
| preamble 4.2 (tick bound) | RC-I10 | RC-T17 |
| architecture 7 (device out of memory) | f.11 | RC-T18 |

## o. Deferred (post-v1)

None.
