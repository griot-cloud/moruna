# Amoru SDD 11: Resource controller (`amoru-controller`)

**Document type:** software design document, component 11 of 12
**Status:** DRAFT · 2026-09-15
**Parent:** `architecture/amoru-runtime-design.md` section 5.8; decisions D2, D3, D7; criteria S1, S2, S3, S6, S11; global invariants G-I1, G-I5, G-I8, G-I10
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.10 (`PlacementStats`, `TierBudgets`), d.11 (`Knobs`, `Knob`), d.12 (`Limits`, `Sample`), d.13 (`TraceRecord`)
**Component location:** `crates/amoru-controller`, Rust
**Consumes:** discovery (3, `Sampler`), trace (4, `TraceView::tail`), placement (9, stats and budgets), scheduler (10, `Knobs`, `probe`). **Consumed by:** runtime facade (12)

**Decisions worth your eye:** (1) the sizing decision is a trait with a rule implementation shipped and a learned implementation stubbed, and the envelope that clamps both is computed from the probe and the budget by the controller, never by the sizer; (2) the working-set equation is enforced as a hard check before any knob is written, so an inconsistent set of knobs is impossible by construction; (3) the profile store is a JSON file per kernel fingerprint and schema hash, advisory only, overridden by the probe on drift.

---

## a. Purpose and boundary

The controller is the one writer of every knob. It derives the budget from the limits, measures each kernel's amplification with a probe, sizes morsels and workers from the working-set equation, adjusts them from measured outcomes, classifies the bottleneck each tick and moves the knob that relieves it, keeps device memory as a second budget, and falls back safely when a learned sizer misbehaves. It is the component that does not exist elsewhere (parent 5.8).

It owns: budget arithmetic; the probe protocol's decisions (the scheduler executes it); the working-set model; the `Sizer` trait and `RuleSizer`; the envelope; bottleneck classification; damping and oscillation freeze; the profile store; the bottleneck timeline for the report.

It refuses to know: morsel bytes; queue internals; how the scheduler picks; how the reactor moves; anything about Python.

## b. Vocabulary

**Allowance.** Per stage, the bytes one in-flight morsel of that stage may occupy at peak: `morsel_target × A_k × safety`.

**Working set.** `Σ_stages active_workers_share(stage) × allowance(stage) + Σ_queues high_water + read_ahead × split_bytes`, which must fit the budget.

**Envelope.** Per stage, the closed interval of morsel targets the controller permits the sizer to propose: `[morsel.min_bytes, min(morsel.max_bytes, budget_for_stage / (active × A_k × safety))]`.

**Adjustment.** One change of a stage's morsel target; additive increase or multiplicative decrease.

**Damping.** The minimum number of completions between adjustments for a stage.

**Flip.** A change of sign between consecutive adjustments of one stage.

**Tick.** The periodic evaluation every `controller.tick_ms`; also triggered synchronously after every trace record (by the scheduler calling `on_record`).

**Profile.** A stored summary of a kernel's measured behaviour keyed by fingerprint and input schema hash.

## c. Invariants

**RC-I1. Knobs are consistent before they are written.** Every knob write is preceded by the working-set check; if a proposed set violates it, the controller reduces morsel targets (largest stage first) until it holds, and only then writes. Upholds G-I1.

**RC-I2. The sizer never escapes the envelope.** A sizer's proposal is clamped to the envelope; the clamp is counted; a sizer proposing outside its envelope more than `sizer.fallback_error_ratio × N` times (N = its proposals so far, once N ≥ 20) is replaced by `RuleSizer`. Upholds D7.

**RC-I3. Adjustments are damped.** No stage's morsel target changes more often than once per `controller.damping_completions` completions of that stage, except the multiplicative decrease on a breach, which is immediate.

**RC-I4. A breach shrinks immediately.** When a record's `mem_anon_peak` exceeds the ceiling minus the reserve, or a device sample exceeds the device budget, the affected stage's target is halved at once and `safety` for that stage is raised by 0.2 (capped at 3.0). Upholds G-I8.

**RC-I5. Oscillation is frozen.** If a stage's adjustments flip sign more than `controller.oscillation_flips` times in its last 100 adjustments, the target is frozen at the geometric mean of the last 10 for `controller.freeze_morsels` completions, and the event is recorded. Upholds S11.

**RC-I6. The probe precedes sizing.** No stage receives a morsel target other than `morsel.probe_bytes` until its probe has completed and `A_k` is set; a profile may seed `A_k` but does not skip the probe.

**RC-I7. Worker count never exceeds what memory can feed.** `active_workers ≤ floor(budget_for_workers / max_stage_allowance)`, re-evaluated at every tick.

**RC-I8. Bottleneck classification moves at most one knob per tick.** The table in f.6 selects one action; the controller never adjusts read-ahead and workers in the same tick.

**RC-I9. The controller never touches payload bytes or blocks on IO.** It reads stats and samples, writes knobs, and reads the trace tail; nothing else.

## d. Interfaces

### d.1 Exposed

```rust
pub struct ControllerConfig {
    pub limits: Limits,
    pub reserve_fraction: f32, pub target_fraction: f32,
    pub safety_initial: f32, pub safety_floor: f32, pub increase_step: f32,
    pub tick_ms: u64, pub oscillation_flips: u32, pub freeze_morsels: u32,
    pub morsel_min: u64, pub morsel_max: u64, pub probe_bytes: u64,
    pub sizer: SizerKind,                          // Rule | Learned
    pub fallback_error_ratio: f32,
    pub profiles_dir: Option<std::path::PathBuf>,
    pub disk_budget: u64,
}

pub struct Controller { /* private */ }
impl Controller {
    pub fn new(cfg: ControllerConfig, sampler: Sampler, knobs: Arc<dyn Knobs>, placement: Arc<dyn Placement>, trace: Arc<TraceWriter>, kernels: Vec<KernelInfo>) -> Result<Controller>;
    /// Phase 1: measure baseline (after kernel init), compute budgets, set placement budgets. Called by the facade before probes.
    pub fn prepare(&mut self) -> Result<Budgets>;
    /// Phase 2: for each stage, ask the scheduler to run the probe protocol and set A_k. The facade passes a closure that runs `Scheduler::probe`.
    pub fn probe_all(&mut self, run_probe: &mut dyn FnMut(StageId, u64 /* probe bytes */) -> Result<ProbeResult>) -> Result<()>;
    /// Phase 3: initial knobs from the working-set equation; then start the tick thread.
    pub fn start(&mut self) -> Result<()>;
    /// Synchronous hook the scheduler calls after each trace record (cheap; enqueues for the tick thread).
    pub fn on_record(&self, r: &TraceRecord);
    pub fn stop(&mut self) -> ControllerSummary;    // joins the tick thread; returns the timeline and sizer state for the report
}

pub struct KernelInfo { pub stage: StageId, pub fingerprint: Fingerprint, pub schema_hash: [u8; 32], pub hints: KernelHints, pub kind: KernelKind }
pub struct Budgets { pub host: u64, pub device: [u64; 8], pub baseline: u64, pub reserve: u64 }

pub trait Sizer: Send {
    fn propose(&mut self, obs: &Observation, envelope: &Envelope) -> Proposal;
    fn observe(&mut self, obs: &Observation, outcome: &Outcome);
    fn confidence(&self) -> f32;
    fn name(&self) -> &'static str;
}
pub struct Observation { pub stage: StageId, pub features: MorselFeatures, pub active_workers: u16, pub a_k: f64, pub safety: f32, pub recent: Vec<TraceRecord> /* tail(stage, 32) */ }
pub struct Envelope { pub min: u64, pub max: u64 }
pub struct Proposal { pub morsel_target: u64 }
pub struct Outcome { pub peak_delta: u64, pub bytes_in: u64, pub wall_ns: u64 }
pub struct RuleSizer { /* f.4 */ }
pub struct LearnedSizer { /* stub in v1: returns RuleSizer's proposal; the phase 8 implementation replaces the body without changing the trait */ }

#[derive(Clone, Debug)]
pub struct ControllerSummary { pub timeline: Vec<(f64, Bottleneck)>, pub sizer: &'static str, pub fallback_at: Option<Seq>, pub freezes: u32, pub breaches: u32, pub final_knobs: KnobSnapshot }
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Bottleneck { IoRead, Memory, Sink, Compute, CpuQuota, Idle }
```

### d.2 Consumed

`amoru_discovery::Sampler`; `amoru_trace::TraceWriter::snapshot().tail`; `amoru_kernel::{Knobs, Knob, KnobSnapshot, Placement, PlacementStats, TierBudgets, Limits, Sample, TraceRecord, MorselFeatures, KernelHints, KernelKind, Fingerprint, StageId, Seq, AmoruError}`; `amoru_scheduler::ProbeResult` (through the closure); `serde_json` for profiles; `blake3` for profile keys.

## e. Data model, formats and state machines

### e.1 Per-stage state

```rust
struct StageCtl {
    stage: StageId, a_k: f64, a_k_dev: f64, safety: f32,
    target: u64, envelope: Envelope,
    completions_since_adjust: u32, last_adjust_sign: i8, flips_window: VecDeque<i8> /* last 100 */,
    frozen_until: Option<u64 /* completions */>,
    peak_ewma: f64, wall_ewma: f64,
    sizer: Box<dyn Sizer>, sizer_clamps: u32, sizer_proposals: u32,
}
```

### e.2 Controller state machine

`Created` → `Prepared` (baseline, budgets) → `Probed` (all `A_k` set) → `Running` (tick thread) → `Stopped`. `on_record` before `Running` is buffered and processed at start.

### e.3 Profile file

`<profiles_dir>/<fingerprint hex>-<schema hash hex>.json`:

```json
{ "version": 1, "fingerprint": "...", "schema_hash": "...", "updated": "2026-09-15T18:00:00Z",
  "a_k_p50": 4.2, "a_k_p95": 5.9, "a_k_dev_p95": 0.0,
  "final_target": 67108864, "final_workers": 8, "final_safety": 1.25,
  "runs": 3, "prediction_error_p95": 0.18 }
```

Unknown `version` → ignored with a note. Written at `stop` when the run completed (not on termination), merging with the existing file by EWMA (weight 0.3 to the new run).

## f. Algorithms and policies

**f.1 Budgets (`prepare`).** Sample once after kernel init: `baseline = sample.anon_bytes`. `reserve = reserve_fraction × limits.memory_ceiling`. `host_budget = limits.memory_ceiling − baseline − reserve`; if ≤ `morsel_min × 2`, `Config { name: "budget.host", msg }`. Device: `device_budget[d] = 0.9 × devices[d].free_bytes` after init (kernel weights are loaded). Split for placement: `TierBudgets { host: 0.5 × host_budget, pinned_host: (pinned ? same : 0), device: 0.6 × device_budget, disk: disk_budget }`; the other half of host budget is the workers' in-flight allowance. This split is the initial one; f.6 adjusts the queue share.

**f.2 Probe (`probe_all`).** For each stage in order: seed `a_k` from the profile if present (p95) else from `hints.expected_amplification` else 4.0; run the probe at `probe_bytes` with one worker; `a_k = max(peak_delta / bytes_in, 0.5)`; if a profile existed and `|a_k − profile.a_k_p95| / profile.a_k_p95 > 0.5`, note "profile drift" and use the measured value; `a_k_dev` likewise from `dev_peak_delta` when `hints.uses_device_memory`; `safety = safety_initial`.

**f.3 Working-set equation and initial knobs (`start`).** Let `W = workers.max`, `S` the stages. Assume equal worker share per stage initially: `share = W / S`. For each stage, `allowance = target × a_k × safety`. Choose `target` per stage as the largest value in `[morsel_min, morsel_max]` such that `Σ share × allowance ≤ host_budget / 2` (the in-flight half) and, for device stages, `share × target × a_k_dev × safety ≤ device_budget × 0.4`. Set `active_workers = min(W, floor((host_budget / 2) / max_stage_allowance))` (RC-I7). Set `read_ahead = 2`. Set `promotion_window = 2`. Set high water per queue and tier from the placement half of the budget divided equally across queues, low = high / 2. Write knobs in the order: high waters, promotion windows, morsel targets, active workers, read-ahead (RC-I1 check before the batch).

**f.4 `RuleSizer` (per record of stage s).** `observe`: update `peak_ewma` (α = 0.2) of `peak_delta / bytes_in`. `propose`: if `completions_since_adjust < damping` return the current target; let `ratio = peak_ewma × safety × target / allowance_target(stage)` where `allowance_target = target_fraction × envelope.max × a_k × safety` (i.e., how much of the allowed peak the stage is using); if `ratio < 0.85` propose `target × (1 + increase_step)`; if `ratio > 1.0` propose `target / 2` (this is the AIMD; the immediate breach path in RC-I4 is separate and stronger); else propose the current target. Also tighten `safety` toward `safety_floor` by 0.02 per accepted increase when the last 20 records' peak ratios have coefficient of variation under 0.15.

**f.5 Applying a proposal.** Clamp to the envelope (count a clamp if changed); if frozen, ignore; if the sign of the change flips relative to `last_adjust_sign`, push to `flips_window`; if flips in window > `oscillation_flips` → freeze (RC-I5); else run the working-set check (RC-I1) with the new target and write `Knob::MorselTarget`; reset `completions_since_adjust`.

**f.6 Bottleneck classification (each tick).** Inputs: `busy = workers_busy / active_workers` (from scheduler stats, mirrored through the trace's `knob_active_workers` and busy time), `q0_bytes`, `reads_in_flight`, `qn_bytes` trend over the last 4 ticks, sink writes in flight, `throttled_delta` over the tick, anon sample vs ceiling.

| Condition (evaluated in order) | Class | Action (one only) |
|---|---|---|
| anon ≥ ceiling − reserve × 0.5 | Memory | halve the largest stage target; raise its safety; `set_staging(qn, true)` if not already |
| throttled_delta / tick > 10% | CpuQuota | `ActiveWorkers −1` (min 1) |
| busy < 0.7 and q0_bytes < low(q0) and reads_in_flight == read_ahead | IoRead | `ReadAhead +1` (max 64) if the working set allows; else shrink queue high waters by 10% to fund it |
| busy < 0.7 and q0_bytes ≥ low(q0) | Memory (workers parked by budget) | shrink queue high waters by 10% and re-run f.3's worker formula (`ActiveWorkers +1` if it now allows) |
| busy > 0.9 and qn_bytes rising 4 ticks and sink writes in flight == max | Sink | `StagingTrigger(qn, true)`; `ReadAhead −1` (min 1) |
| busy > 0.9 | Compute | none; record |
| otherwise | Idle | none |

The class and tick time are appended to the timeline (compressed: consecutive equal classes are one entry with a duration).

**f.7 Breach handling (RC-I4).** On `on_record` with `mem_anon_peak > ceiling − reserve` or a device sample above budget: immediately (not waiting for the tick) halve that stage's target, raise its safety by 0.2, write the knob; if the stage is already at `morsel_min` and a second breach occurs, lower `active_workers` by one; if `active_workers == 1` and at `morsel_min` and a third breach occurs, terminate the run with `AmoruError::Budget { seq, stage, footprint, budget, features }` (G-I8's diagnostic).

**f.8 Learned sizer fallback (RC-I2).** `LearnedSizer` proposals are subject to the same clamp; the controller tracks `prediction_error = |predicted_peak − observed_peak| / observed_peak` per record for both the active sizer and a shadow `RuleSizer`; if, after 50 records, the learned sizer's rolling error p95 exceeds `fallback_error_ratio × RuleSizer`'s, swap to `RuleSizer` for the run and record `fallback_at`.

**f.9 Profile write (`stop`).** If the run completed: merge per e.3 and write atomically (temp file + rename). If `profiles_dir` is None or unwritable: skip with a note.

**f.10 Tiny dataset fast path.** If the source's total planned bytes < `host_budget / 4` (the facade passes the plan total): skip probes, set every target to `morsel_max`, `active_workers = W`, `read_ahead = min(splits, 8)`, and mark the report "small dataset: no adaptation". (Architecture section 8.)

## g. Concurrency within the component

One tick thread. `on_record` pushes a compact copy of the record's numeric fields into a bounded lock-free queue (capacity 65,536; overflow drops the oldest with a counter, since the trace is the durable copy and the controller only needs recency) drained by the tick thread. All knob writes happen on the tick thread except the breach path in f.7, which runs on the calling worker thread under the controller's single mutex (held for microseconds; lock order: after the trace channel, position 5, since `on_record` is called after `record`). No lock is held across any call into another component except `Knobs::set`, which is atomic.

## h. Behaviour

**Normal path (compute-bound tokeniser, 8 cores, 8 GiB ceiling).** Baseline 400 MiB; reserve 800 MiB; host budget 6.8 GiB; probe measures `a_k = 7`; f.3 sets `target ≈ 6.8 GiB / 2 / (8 × 7 × 1.5) ≈ 40 MiB`, `active = 8`; ticks classify Compute; the sizer nudges the target to ~60 MiB as safety tightens; steady state ~85% of ceiling in use.

**Normal path (IO-bound identity from S3).** `a_k ≈ 1`; targets hit `morsel_max`; busy < 0.7; ticks classify IoRead and raise read-ahead toward the bandwidth-delay product until busy rises or the working set is exhausted.

**Adversarial (S6).** At the midpoint `a_k` jumps 4×; the next record breaches; f.7 halves the target and raises safety; two more breaches at the new target may occur as in-flight morsels complete; the target reaches the level the new `a_k` allows; the run completes; if the new `a_k` cannot fit even one `morsel_min` per worker, the run terminates with the diagnostic.

**Edge cases.** A stage whose `a_k` < 0.5 (a filter that drops most rows): clamped to 0.5 so the allowance stays conservative. A pipeline with one stage: `share = W`. Device stage with no device present (feature off): `Config` at `prepare`. `morsel_max` smaller than the probe size (misconfiguration): probe at `morsel_max`.

**Failures.** Sampler read errors: use the last sample, count; after 100 consecutive, terminate with `Config { name: "sampler" }`. Profile file corrupt: ignore with a note. Knob write when the scheduler has exited (race at completion): ignored (the scheduler's `set` is a no-op after exit).

## i. Configuration

All `controller.*`, `budget.*`, `morsel.*`, `workers.active`, `readahead.splits`, `queue.*`, `profiles.dir`, `sizer`, `sizer.fallback_error_ratio` rows (preamble section 5).

## j. Observability

`ControllerSummary` (timeline, sizer, fallback, freezes, breaches, final knobs) into the run report; `tracing`: `ctl.budgets` (info), `ctl.probe` (info per stage: `a_k`, `a_k_dev`), `ctl.adjust` (debug: stage, old, new, reason), `ctl.breach` (warn), `ctl.freeze` (warn), `ctl.class` (debug per tick), `ctl.fallback` (warn), `ctl.terminate` (error).

## k. Tests

Tests drive the controller with `FakeKnobs`, `FakePlacement`, a scripted `Sampler` (test constructor from a sample sequence) and a synthetic trace.

**RC-T1 consistent_knobs.** Property test: for random budgets, `a_k`s and worker counts, every written knob set satisfies the working-set inequality. RC-I1.

**RC-T2 envelope_clamp_and_fallback.** A test sizer proposing 10× the envelope is clamped every time and replaced after the ratio threshold. RC-I2.

**RC-T3 damping.** Adjustments per stage never closer than `damping` completions except breach halvings. RC-I3.

**RC-T4 breach_immediate.** A record over the line halves the target before the next tick; three breaches at the floor terminate with `Budget` carrying seq, stage, footprint, budget, features. RC-I4, G-I8.

**RC-T5 oscillation_freeze.** A synthetic trace alternating high and low peaks; flips exceed the threshold; the target freezes at the geometric mean for `freeze_morsels`; S11's bound holds over the run. RC-I5.

**RC-T6 probe_first.** No `MorselTarget` knob other than `probe_bytes` is written before `probe_all` returns. RC-I6.

**RC-T7 workers_bounded_by_memory.** Large `a_k` and small budget → `active_workers` < `workers.max` by the formula. RC-I7.

**RC-T8 one_action_per_tick.** Every tick writes at most one of read-ahead or workers. RC-I8.

**RC-T9 classification_table.** For each row of f.6, a synthetic state produces that class and that action. f.6.

**RC-T10 profile_roundtrip.** Write, read, merge with EWMA; corrupt file ignored; drift detection triggers the note. e.3, f.2, f.9.

**RC-T11 tiny_dataset.** Plan total under a quarter of budget → no probes, max targets, report note. f.10.

**RC-T12 end_to_end_budget.** (real components, container with `--memory`) All five benchmark kernels complete with peak anon ≤ ceiling; adversarial completes or terminates cleanly. S1, S6, S2.

**RC-T13 throughput.** (reference host) Defaults reach ≥ 80% of the hand-tuned baseline for normalise, tokenise-explode and wide-intermediate. S3.

## l. Implementation notes for the agent

Files: `src/lib.rs`, `src/budget.rs` (f.1), `src/probe.rs` (f.2), `src/model.rs` (working set, envelope, f.3), `src/sizer/{mod.rs (trait), rule.rs (f.4), learned.rs (stub + shadow error tracking f.8)}`, `src/apply.rs` (f.5, RC-I1 check), `src/classify.rs` (f.6), `src/breach.rs` (f.7), `src/profile.rs` (e.3, f.9), `src/tick.rs` (thread, queue drain), `src/summary.rs`. No `unsafe`.

Numerics: all ratios in `f64`; bytes in `u64`; conversions saturating; never divide by `bytes_in == 0` (skip such records for sizing).

Anti-patterns: no knob write outside `apply.rs` and `breach.rs`; no reading morsel payloads; no blocking on the trace writer (use `tail`, which is in-memory); no sizer that reads anything but `Observation`.

## m. Open items

None. (The learned sizer's model is Phase 8; the stub and the shadow error tracking ship now so the fallback path is tested.)

## n. Traceability

| Parent id | Invariant | Test |
|---|---|---|
| S1, G-I1 | RC-I1, RC-I7 | RC-T1, RC-T7, RC-T12 |
| S2, G-I10 | f.3 (no user parameters) | RC-T12 |
| S3 | f.4, f.6 | RC-T13 |
| S6, G-I8 | RC-I4, f.7 | RC-T4, RC-T12 |
| S11 | RC-I3, RC-I5 | RC-T3, RC-T5 |
| D2 | RC-I8, f.6 | RC-T8, RC-T9 |
| D3 | RC-I6, f.2 | RC-T6 |
| D7 | RC-I2, f.8 | RC-T2 |
| G-I5 | (sole writer, by construction of d.1) | RC-T1 |
