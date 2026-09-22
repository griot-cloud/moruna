# Amoru SDD 11: Resource controller (`amoru-controller`)

**Document type:** software design document, component 11 of 12
**Status:** DRAFT · 2026-09-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/amoru-runtime-design.md` section 5.8; decisions D2, D3, D7; criteria S1, S2, S3, S6, S11; global invariants G-I1, G-I5, G-I8, G-I10
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.10 (`Placement::set_budgets`, `TierBudgets`), d.11 (`Knobs`, `Knob`, `KnobSnapshot`, `StatsSource`, `SchedulerStats`, `Prober`, `ProbeResult`, `SizerKind`), d.12 (`Limits`, `Sampler`, `Sample`), d.13 (`TraceRecord`, `TraceTail`)
**Component location:** `crates/amoru-controller`, Rust
**Consumes:** contracts (1) only; the sampler (3), the trace tail (4), the placement engine (9, `set_budgets`) and the scheduler (10, `Knobs`, `StatsSource`, `Prober`) arrive as trait objects. **Consumed by:** runtime facade (12)

**Decisions worth your eye:** (1) the sizing decision is a trait with a rule implementation shipped and a learned implementation stubbed, and the envelope that clamps both is computed from the probe and the budget by the controller, never by the sizer; (2) the working-set equation is enforced as a hard check before any knob is written, so an inconsistent set of knobs is impossible by construction; (3) the profile store is a JSON file per kernel fingerprint and schema hash, advisory only, overridden by the probe on drift; (4) the controller names no other crate: everything it touches is a contracts trait, so it is testable entirely with the testkit fakes.

---

## a. Purpose and boundary

The controller is the one writer of every knob. It derives the budget from the limits, measures each kernel's amplification with a probe, sizes morsels and workers from the working-set equation, adjusts them from measured outcomes, classifies the bottleneck each tick and moves the knob that relieves it, keeps device memory as a second budget, and falls back safely when a learned sizer misbehaves. It is the component that does not exist elsewhere (parent 5.8).

It owns: budget arithmetic; the probe protocol's decisions (the scheduler executes it through `Prober`); the working-set model; the `Sizer` trait and `RuleSizer`; the envelope; bottleneck classification; damping and oscillation freeze; the profile store; the bottleneck timeline and notes for the report.

It refuses to know: morsel bytes; queue internals; how the scheduler picks; how the reactor moves; anything about Python.

## b. Vocabulary

**Allowance.** Per stage, the bytes one in-flight morsel of that stage may occupy at peak: `morsel_target × A_k × safety`.

**Working set.** `Σ_stages share(stage) × allowance(stage) + Σ_queues high_water + read_ahead × split_bytes + state`, which must fit the budget.

**Envelope.** Per stage, the closed interval of morsel targets the controller permits the sizer to propose: `[morsel.min_bytes, min(morsel.max_bytes, budget_for_stage / (share × A_k × safety))]`.

**Adjustment.** One change of a stage's morsel target; additive increase or multiplicative decrease.

**Damping.** The minimum number of completions between adjustments for a stage.

**Flip.** A change of sign between consecutive adjustments of one stage.

**Tick.** The periodic evaluation every `controller.tick_ms`; also triggered synchronously after every trace record, because the facade installs `on_record` as the scheduler's `RecordHook` (contracts d.11).

**Profile.** A stored summary of a kernel's measured behaviour keyed by fingerprint and input schema hash.

**State.** Per stage, the bytes its instances hold regardless of morsel size: `instances_live × max(state_bytes over the stage's last 32 records)`, grouped by the record's `instance` column when present (f.3).

## c. Invariants

**RC-I1. Knobs are consistent before they are written.** Every knob write is preceded by the working-set check; if a proposed set violates it, the controller reduces morsel targets (largest stage first) until it holds, and only then writes. If reducing the targets to `morsel_min` is not enough, the queue high waters follow them down, and the one set that may still fail the inequality is the smallest set there is: every target at the floor and every queue at zero, which is not an inconsistent write but a run f.6's StateGrowth row or f.7's third breach is about to end. The check is also an obligation between writes: the `state` term of f.3 grows under the controller, so a set that fitted when it was written can stop fitting with nothing written at all, and every tick restores it before it decides anything else. Upholds G-I1.

**RC-I2. The sizer never escapes the envelope.** A sizer's proposal is clamped to the envelope; the clamp is counted. The learned sizer is replaced by `RuleSizer` for the rest of the run when, after 20 proposals, more than `1 / sizer.fallback_error_ratio` of its proposals were clamped (the default ratio 2.0 means more than half), or when its rolling prediction-error p95 exceeds `sizer.fallback_error_ratio ×` the shadow rule sizer's (f.8). Upholds D7.

**RC-I3. Adjustments are damped.** No stage's morsel target changes more often than once per `controller.damping_completions` completions of that stage, except the multiplicative decrease on a breach, which is immediate.

**RC-I4. A breach shrinks immediately.** When a record's `mem_anon_peak` (contracts d.13; the larger of the samples before and after `apply`, SC f.2) exceeds `ceiling − reserve`, or a record's `dev_mem_peak` exceeds the device budget, the affected stage's target is halved at once and `safety` for that stage is raised by 0.2 (capped at 3.0). Upholds G-I8.

**RC-I5. Oscillation is frozen.** If a stage's adjustments flip sign more than `controller.oscillation_flips` times in its last 100 adjustments, the target is frozen at the geometric mean of the last 10 for `controller.freeze_morsels` completions, and the event is recorded. Upholds S11.

**RC-I6. The probe precedes sizing.** No stage receives a morsel target other than `morsel.probe_bytes` until its probe has completed and `A_k` is set; a profile may seed `A_k` but does not skip the probe, except on a resumed run (f.14), where the profile the interrupted run wrote is trusted for the stages it covers and only the others are probed, and on the tiny-dataset and zero-kernel paths (f.10, f.3), which have nothing to probe for.

**RC-I7. Worker count never exceeds what memory can feed.** `active_workers ≤ floor(budget_for_workers / max_stage_allowance)`, re-evaluated at every tick.

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

`amoru_kernel::{Knobs, Knob, KnobSnapshot, StatsSource, SchedulerStats, StageStats, Prober, ProbeResult, SizerKind, Sampler, Sample, TraceTail, TraceRecord, Outcome, Placement, TierBudgets, TierKind, Limits, MorselFeatures, KernelHints, KernelKind, Fingerprint, SourceSchema, StageId, Seq, AmoruError}`; `serde_json` for profiles; `blake3` for profile keys. No `amoru_discovery`, `amoru_trace`, `amoru_placement` or `amoru_scheduler` dependency.

## e. Data model, formats and state machines

### e.1 Per-stage state

```rust
struct StageCtl {
    stage: StageId, a_k: f64, a_k_dev: f64, safety: f32,
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


**f.1 Budgets (`prepare`).** Sample once, after the scheduler's `init_instances` has run every stateful `init` (preamble section 2, "baseline"): `baseline = sample.anon_bytes`. `reserve = reserve_fraction × limits.memory_ceiling`. `host_budget = limits.memory_ceiling − baseline − reserve`; if ≤ `morsel_min × 2`, `Config { name: "budget.host", msg }`. Device: `device_budget[d] = 0.9 × devices[d].free_bytes` after init (kernel weights are loaded). Split for placement: one host pool, on the tier the arena actually has (contracts e.1): `TierBudgets { pinned_host: 0.5 × host_budget, host: 0 }` when `cfg.pinned`, else `{ host: 0.5 × host_budget, pinned_host: 0 }`; `device: 0.6 × device_budget`; `disk: disk_budget`. The other half of the host budget is the workers' in-flight allowance. This split is the initial one; f.6 adjusts the queue share through `Knob::HighWater`, never through a placement setter; `set_budgets` is the only placement call the controller makes, here and when `state` changes (f.3).

**f.2 Probe (`probe_all`).** For each stage in order: seed `a_k` from the profile if present (p95) else from `hints.expected_amplification` else 4.0; choose the probe size: `probe_bytes`, or `hints.preferred_rows × bytes per row` when `preferred_rows` is set (bytes per row = `plan.total_bytes / plan.total_rows` for stage 1, and `bytes_in / rows_in` of the upstream stage's `ProbeResult` for later stages), clamped to `[morsel_min, morsel_max]`; call `prober.probe(stage, bytes)` (the scheduler runs it with one worker, SC f.9); `a_k = max(peak_delta / bytes_in, 0.5)`; if a profile existed and `|a_k − profile.a_k_p95| / profile.a_k_p95 > 0.5`, note "profile drift" and use the measured value; `a_k_dev` likewise from `dev_peak_delta` when `hints.uses_device_memory`; `safety` from the evidence: with no profile, `safety_initial`; with a profile, `safety = clamp(safety_floor + k × sqrt(a_k_var) / a_k_p50 + c / sqrt(a_k_samples), safety_floor, safety_initial)` with `k = 2` and `c = 4` (so 16 samples add 1.0 to the margin and 40,000 add 0.02), which starts a well-known kernel near the floor and an unseen one at the initial value; drift (above) resets `safety` to `safety_initial` for the run. The margin never goes below `safety_floor`; the guarantee is not relaxed, only the margin shrinks with evidence. `ProbeResult.wall_ns` and `cpu_ns` seed `wall_ewma` and the first Compute/IoRead classification.

**f.3 Working-set equation and initial knobs (`start`).** Let `W = workers_max`, `S` the number of kernel stages. `state(stage) = instances_live × max(hints.state_bytes, profile.state_bytes_max, max(state_bytes over the stage's last 32 records grouped by instance))`, and `state = Σ state(stage)`: the memory instances hold regardless of morsel size; it is subtracted from the budget before anything else and updated at every tick from the trace tail (the `instance` column, contracts d.13, identifies which instance grew). `budget_for_stage = (host_budget − state) / 2 / S`. The read-ahead's bytes are spent out of the queue half rather than on top of it (they land in Q0), so the division is `state + (host_budget − state) / 2 for the workers + (host_budget − state) / 2 for the queues and the read-ahead together`, which is what makes b's inequality an accounting of the budget rather than a hope. `share = active_workers / S` (initially `W / S`). For each stage, `allowance = target × a_k × safety`. Solve for one common target `t` such that `Σ_stages share × t × a_k × safety ≤ (host_budget − state) / 2` and, for device stages, `share × t × a_k_dev × safety ≤ device_budget × 0.4`; then clamp per stage: `target(stage) = clamp(t, morsel_min, morsel_max)`. Set `active_workers = min(W, floor(((host_budget − state) / 2) / max_stage_allowance))` (RC-I7). `split_bytes = target[1]` when `plan.sub_splittable_all`, else `max(target[1], plan.max_split_bytes)` (a split that cannot be sub-split arrives whole). Set `read_ahead = 2`, subject to `read_ahead × split_bytes` fitting the working set (b), else lower it to what fits, floor 1. Set `promotion_window = 2`. Set high water per queue and tier from the placement half of the budget divided equally across queues (`Knob::HighWater { stage, tier, bytes }`; the scheduler derives low = high / 2, contracts d.11). Write knobs in the order: high waters, promotion windows, morsel targets, active workers, read-ahead (RC-I1 check before the batch). With zero kernels (`S = 0`, 12 h): skip probes, write `MorselTarget { stage: 0, bytes: min(morsel_max, host_budget / 2) }` (the source drive reads at it, SC f.5), `ActiveWorkers(1)`, `ReadAhead(2)`; `share = W / max(S, 1)` everywhere.

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

**f.7 Breach handling (RC-I4).** On `on_record` with `mem_anon_peak > ceiling − reserve` or `dev_mem_peak` above the device budget: immediately (not waiting for the tick) halve that stage's target, raise its safety by 0.2, write the knob; if the stage is already at `morsel_min` and a second breach occurs, lower `active_workers` by one; if `active_workers == 1` and at `morsel_min` and a third breach occurs, `knobs.terminate(AmoruError::Budget { seq, stage, footprint: mem_anon_peak − mem_anon_before, budget, features })` (G-I8's diagnostic; the scheduler enters `Terminating`, SC f.8).

**f.8 Learned sizer fallback (RC-I2).** `LearnedSizer` proposals are subject to the same clamp; the controller counts clamps per sizer and tracks `prediction_error = |predicted_peak − observed_peak| / observed_peak` per record for the active sizer (from `Proposal.predicted_peak`; a `None` prediction counts as an error of 1.0) and for a shadow `RuleSizer` (whose prediction is `peak_ewma × bytes_in`); after 20 proposals, if `sizer_clamps / sizer_proposals > 1 / fallback_error_ratio`, or if the learned sizer's rolling error p95 exceeds `fallback_error_ratio × RuleSizer`'s, swap to `RuleSizer` for the run, record `fallback_at`, and add a note.

**f.9 Profile write (`stop`).** The run completed when the source is exhausted (`SchedulerStats::source_exhausted`, contracts d.11) and the controller did not terminate it; `stop` takes no argument and that is how it knows. A terminated run's figures describe a kernel that did not fit, and storing them would teach the next run the wrong lesson, so they are not stored. If the run completed: merge per e.3 and write atomically (temp file + rename). If `profiles_dir` is None or unwritable: skip with a note. When `checkpoint_enabled`, the profile is also written every `checkpoint_interval_ms` from the tick thread after the first 50 records per stage, with the same merge, so a run that dies keeps what it learned; f.14 reads it back.

**f.10 Tiny dataset fast path.** If `plan.total_bytes < host_budget / 4`: skip probes, set every target to `morsel_max`, `active_workers = W`, `read_ahead = min(plan.splits, 8)`, and add the note "small dataset: no adaptation". (Architecture section 8.) RC-I1 still applies to the knobs this path writes, because `morsel_max x a_k x safety` per worker can exceed the budget even when the whole dataset does not: the in-flight count per stage is `min(share, ceil(total_bytes / target))`, since a dataset of two morsels cannot put one on each of eight workers, and the working-set check runs on the result as it does everywhere else.

**f.11 Device out of memory (architecture 7).** The scheduler retries an `Alloc { tier: Device(d) }` error from `apply` once on the same morsel, after the record hook has run (SC f.8). In that hook, on the first such record for a stage (`outcome == Error`, `error` names `alloc` in a `Device` tier): halve the stage's target (bypassing damping, as f.7 does), write `Knob::HighWater { stage: stage − 1, tier: TierKind::Device, bytes: 0 }` so the engine demotes promoted-but-unconsumed morsels of the input queue back to the host tier, set `device_breaches[stage] = 1`, and add the note "device OOM retry on stage k"; the next tick restores that high water from f.3. On a second such record for the same stage, `knobs.terminate(Budget { seq, stage, footprint: dev_mem_peak, budget: device_budget[d], features })` with the device figures in the diagnostic.

**f.14 Resume (`probe_missing`).** Called by the facade instead of `probe_all` on a resumed run, after `prepare` and after the scheduler's `apply_resume_point` (SC f.13). For each stage: if the profile store has an entry for its fingerprint (written by f.9 during the interrupted run, or by any earlier run), seed `a_k` and `a_k_dev` from it and skip the probe; otherwise probe as in f.2. Then `start` as usual: the working-set equation runs on the seeded values, so a resumed run begins at the sizes the interrupted run had learned rather than at `probe_bytes`. The controller does not read the manifest; the profile store is its memory, keyed by kernel fingerprint, and it is deliberately node-independent (a directory the operator can place on the same durable volume as the staging directory; `profiles.dir`). The note "resumed: N stages seeded, M probed" is added.

## g. Concurrency within the component

One tick thread. All mutable state (the per-stage table of e.1, the phase of e.2, the sizer, the budgets, the notes) sits in a single `Mutex<ControllerState>`; every public method takes `&self` and locks it, so the facade shares the controller as `Arc<Controller>` and installs `on_record` as the scheduler's record hook before it calls `prepare` (12 f.1), with no `&mut` borrow to conflict with the hook. `on_record` runs on the recording worker (it is the scheduler's `RecordHook`) and pushes a compact copy of the record's numeric fields into a bounded lock-free queue (capacity 65,536; overflow drops the oldest with a counter, since the trace is the durable copy and the controller only needs recency) drained by the tick thread. All knob writes happen on the tick thread except the breach path in f.7 and the device path in f.11, which run on the calling worker thread under the controller's single mutex (held for microseconds; lock order position 6 in preamble 4.2, after the trace channel at 5, since `on_record` is called after `record`). No lock is held across any call into another component: the mutex is released before `Knobs::set`, `stats.scheduler_stats()`, `sampler.sample()`, `trace.tail()` and `placement.set_budgets()`, and a tick's decisions are computed from copies taken under the lock (RC-I10).

## h. Behaviour

**Normal path (compute-bound tokeniser, 8 cores, 8 GiB ceiling).** Baseline 400 MiB; reserve 800 MiB; host budget 6.8 GiB; probe measures `a_k = 7`; f.3 sets `target ≈ 6.8 GiB / 2 / (8 × 7 × 1.5) ≈ 40 MiB`, `active = 8`; ticks classify Compute; the sizer nudges the target to ~60 MiB as safety tightens; steady state ~85% of ceiling in use.

**Normal path (IO-bound identity from S3).** `a_k ≈ 1`; targets hit `morsel_max`; busy < 0.7; ticks classify IoRead and raise read-ahead toward the bandwidth-delay product until busy rises or the working set is exhausted.

**Adversarial (S6).** At the midpoint `a_k` jumps 4×; the next record breaches; f.7 halves the target and raises safety; two more breaches at the new target may occur as in-flight morsels complete; the target reaches the level the new `a_k` allows; the run completes; if the new `a_k` cannot fit even one `morsel_min` per worker, the run terminates with the diagnostic.

**Edge cases.** A stage whose `a_k` < 0.5 (a filter that drops most rows): clamped to 0.5 so the allowance stays conservative. A pipeline with one stage: `share = active_workers`. Zero kernels: f.3's zero-kernel path; no probes, one active worker, no ticks change anything but read-ahead. Device stage with no device present (feature off): `Config` at `prepare`. `morsel_max` smaller than the probe size (misconfiguration): probe at `morsel_max`. A single row larger than `morsel_max` (SC f.5): the record's `bytes_in` exceeds the target; the allowance is computed from the observed `bytes_in`, `active_workers` drops to what the budget allows for it (possibly one), and the note "row larger than morsel target on stage k" is added once.

**Failures.** Sampler stalls (`Sample.at_ns` unchanged for 100 consecutive ticks; the contracts' sampler never returns an error, DS-I3): `knobs.terminate(Config { name: "sampler" })`. Profile file corrupt: ignore with a note. Knob write when the scheduler has exited (race at completion): ignored (the scheduler's `set` is a no-op after exit, SC f.15).

## i. Configuration

All `controller.*`, `budget.*`, `morsel.*`, `workers.active`, `readahead.splits`, `queue.*`, `profiles.dir`, `sizer`, `sizer.fallback_error_ratio` rows (preamble section 5). The controller writes knobs inside the ranges; the scheduler clamps and counts anything outside them (SC f.15), and `budget.*` clamping belongs to discovery.

## j. Observability

`ControllerSummary` (timeline, sizer, fallback, freezes, breaches, final knobs, notes) into the run report; `tracing`: `ctl.budgets` (info), `ctl.probe` (info per stage: `a_k`, `a_k_dev`, probe bytes), `ctl.adjust` (debug: stage, old, new, reason), `ctl.breach` (warn), `ctl.device_oom` (warn), `ctl.freeze` (warn), `ctl.class` (debug per tick), `ctl.fallback` (warn), `ctl.terminate` (error), `ctl.tick_skipped` (warn: a sample over 5 ms).

## k. Tests

Tests drive the controller with the contracts d.15 fakes and name only their knobs: `FakeKnobs` (`stats(SchedulerStats)`, `probe_result(stage, ProbeResult)`; observables `writes()`, `terminated()`) as `Knobs`, `StatsSource` and `Prober`; `FakeSampler::scripted(Vec<Sample>)`; `FakeTrace` (`capacity(n)`) as `TraceTail`, fed synthetic `TraceRecord`s by the test through `TraceSink::record` and delivered to `on_record` by the test in the same order; `FakePlacement` for `set_budgets`.

**RC-T1 consistent_knobs.** Property test: for random budgets, `a_k`s and worker counts, every written knob set satisfies the working-set inequality. RC-I1.

**RC-T2 envelope_clamp_and_fallback.** A test `Sizer` proposing 10× the envelope with `predicted_peak = Some(envelope.max)` is clamped every time and replaced by `RuleSizer` at the 21st proposal (more than half clamped at ratio 2.0); a second test sizer that stays inside the envelope but predicts half the observed peak is replaced once its error p95 exceeds twice the shadow rule sizer's; `fallback_at` is set and the note appears in `ControllerSummary.notes`. RC-I2, f.8.

**RC-T3 damping.** Adjustments per stage never closer than `damping` completions except breach halvings; `Observation.completions_since_adjust` and `damping` equal what the controller enforces. RC-I3.

**RC-T4 breach_immediate.** A record with `mem_anon_peak` over `ceiling − reserve` halves the target before the next tick (a `MorselTarget` write appears in `FakeKnobs.writes()` from inside `on_record`); three breaches at the floor produce `FakeKnobs.terminated() == Some(Budget { .. })` carrying seq, stage, footprint, budget, features. RC-I4, G-I8.

**RC-T5 oscillation_freeze.** A synthetic trace alternating high and low peaks; flips exceed the threshold; the target freezes at the geometric mean for `freeze_morsels`; S11's bound holds over the run. RC-I5.

**RC-T6 probe_first.** With `FakeKnobs::probe_result(k, ..)` for each stage, no `MorselTarget` knob other than `probe_bytes` is written before `probe_all` returns; `probe_all` calls `probe` once per stage in order; with `hints.preferred_rows` set on stage 2, the requested bytes equal `preferred_rows × bytes_in / rows_in` of stage 1's result. RC-I6, f.2.

**RC-T7 workers_bounded_by_memory.** Large `a_k` and small budget → `active_workers` < `workers_max` by the formula; the targets of three stages with different `a_k` are the one common `t` clamped per stage. RC-I7, f.3.

**RC-T8 one_action_per_tick.** Every tick writes at most one of read-ahead or workers. RC-I8.

**RC-T9 classification_table.** For each row of f.6, `FakeKnobs::stats(SchedulerStats { .. })` and `FakeSampler::scripted` describe that state and the tick produces that class and exactly that action in `FakeKnobs.writes()`: row 2 writes `StagingTrigger { stage: n, on: true }`, rows 4 and 5 write `HighWater`, the StateGrowth row appends a smaller `TierBudgets` to `FakePlacement.budgets_set()` (contracts d.15) and, past half the budget, `terminated()` is a `Budget` naming the stage. f.6.

**RC-T10 profile_roundtrip.** Write, read, merge with EWMA; corrupt file ignored; drift detection triggers the note. e.3, f.2, f.9.

**RC-T11 tiny_dataset.** `PlanSummary.total_bytes` under a quarter of budget → no `probe` calls, max targets, `read_ahead == min(splits, 8)`, the note. f.10.

**RC-T12 end_to_end_budget.** (integration, closes in wave 4; container with `--memory`) All five benchmark kernels complete with peak anon ≤ ceiling through the Rust facade; adversarial completes or terminates cleanly. S1, S6, S2.

**RC-T13 throughput.** (reference host, E1) Defaults reach ≥ 80% of the hand-tuned baseline for normalise, tokenise-explode and wide-intermediate. S3.

**RC-T14 resume_seeds_from_profile.** With profiles for two of three stages, `probe_missing` calls `probe` exactly once and starts with targets computed from the seeded `a_k`s; with `checkpoint_enabled` and `checkpoint_interval_ms = 50`, a profile file exists after the first write past 50 records; the note reads "resumed: 2 stages seeded, 1 probed". f.14, f.9.

**RC-T15 safety_from_evidence.** With no profile, `safety == safety_initial`; with a profile of 16 samples and low variance, `safety` is near the initial value; with 40,000 samples and low variance, near the floor; with 40,000 samples and high variance, well above the floor; drift resets it. f.2.

**RC-T16 state_growth.** Synthetic records for a stateful stage with `instance` 0 and 1 whose `state_bytes` grow by 64 MiB per record, `instances_live = 2` in `FakeKnobs::stats`, and `FakeSampler::scripted` anon rising to match: the `StateGrowth` class fires, targets shrink to fund the state and `FakePlacement.budgets_set()` (contracts d.15) ends with the smaller half, and `terminated()` is a `Budget` naming the stage once `state` alone exceeds half the budget; no written knob set ever violates the working-set inequality. f.3, f.6.

**RC-T17 tick_lock_bound.** A `FakeSampler::scripted` sequence and a `FakeTrace` holding 100,000 records; 1,000 ticks; the controller mutex is never held longer than 5 ms (instrumented with a test-only guard timer), and no call into `FakeKnobs`, `FakeSampler`, `FakeTrace` or `FakePlacement` happens while it is held (each fake is wrapped by the test to assert the mutex is free). RC-I10, preamble 4.2.

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
| S1, G-I1 | RC-I1, RC-I7 | RC-T1, RC-T7, RC-T12 |
| S2, G-I10 | f.3 (no user parameters) | RC-T12 |
| S3 | f.4, f.6 | RC-T13 |
| S6, G-I8 | RC-I4, f.7 | RC-T4, RC-T12 |
| S17 | f.9 (periodic write), f.14 | RC-T14 |
| S1, S6 (stateful kernels) | f.3 `state`, f.6 StateGrowth | RC-T16 |
| S3, S11 (margin from evidence) | f.2 safety derivation | RC-T15 |
| S11 | RC-I3, RC-I5 | RC-T3, RC-T5 |
| D2 | RC-I8, f.6 | RC-T8, RC-T9 |
| D3 | RC-I6, f.2 | RC-T6, RC-T11 |
| D7 | RC-I2, f.8 | RC-T2 |
| G-I5 | (sole writer, by construction of d.1) | RC-T1 |
| preamble 4.2 (tick bound) | RC-I10 | RC-T17 |
| architecture 7 (device out of memory) | f.11 | RC-T18 |

## o. Deferred (post-v1)

None.
