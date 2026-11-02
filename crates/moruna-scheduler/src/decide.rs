//! Deciding what runs next, and admitting what it makes (f.3, f.20; contracts d.17, d.18;
//! adaptive-runtime 9.4, 12.5).
//!
//! Each time a worker is free the scheduler gathers the units that are ready, each with its
//! prediction (the run's cost model's, with a spread, or the stage's measured ratios when there
//! is none, f.22), its critical-path weight and how long it has waited, and a snapshot of every
//! stage's backlog and units under way; asks the run's policy for its actions within a decision
//! budget; and runs the first unit whose predicted output it can claim from the buffer catalog. Every push the scheduler makes is admitted under
//! a claim: a unit's output tops up the claim it ran under, a read or a replacement claims its
//! bytes, a claim that is pending waits for the room on its way, and a batch memory cannot
//! admit at all is written straight to disk. A unit that does not fit is never run, whatever its
//! score, and no push puts the catalog past its budget.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use moruna_kernel::{
    Action, BufferId, Consumer, Locality, Machine, Morsel, MorunaError, PlacementStats,
    PolicyStats, PortView, PredictedBy, Prediction, Reads, Ready, Reservation, Reserve, Result,
    SchedulerState, SchedulingPolicy, Spread, StageId, StageView, Tier, TierKind,
};

use crate::shared::Shared;

/// The disk bandwidth a run is taken to have when its embedder declares none
/// (`SchedulerConfig::disk_bytes_per_second`): what a lookahead plays a spill and a read-back at.
pub const DISK_BYTES_PER_SECOND: u64 = 500_000_000;

/// A decision may take this share of the median unit's duration (9.4 step 4, G9: 1 percent).
pub(crate) const DECISION_BUDGET_PERMILLE: u64 = 10;
/// The budget before any unit has finished, or when units are shorter than this allows.
pub(crate) const DECISION_BUDGET_FLOOR_NS: u64 = 50_000;
/// How many recent unit durations the median is taken over.
const DURATIONS_KEPT: usize = 64;
/// How long a push waits for room before the run ends saying why (12.5: when nothing can leave
/// memory and there is no disk to write to, waiting longer cannot help).
pub(crate) const PUSH_STALL: Duration = Duration::from_secs(30);
/// How long a push waiting for room parks between tries.
const PUSH_PARK: Duration = Duration::from_millis(1);

/// What a push waiting for room tells the time by and waits with (f.20): the system's clock in a
/// run, a clock a test steps in a test, so a wait of `PUSH_STALL` is proved in milliseconds.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Time since the clock started.
    fn now(&self) -> Duration;
    /// Wait about `pause`.
    fn pause(&self, pause: Duration);
}

/// The system's monotonic clock.
#[derive(Debug)]
pub struct SystemClock(Instant);

impl Default for SystemClock {
    fn default() -> SystemClock {
        SystemClock(Instant::now())
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }

    fn pause(&self, pause: Duration) {
        std::thread::sleep(pause);
    }
}

/// The run's policy, the baseline it falls back to, and what deciding cost (9.7).
pub(crate) struct Decider {
    policy: Box<dyn SchedulingPolicy>,
    baseline: Box<dyn SchedulingPolicy>,
    stats: PolicyStats,
    durations: VecDeque<u64>,
    /// When unused memory was last sampled, how much, and whether a unit was ready then.
    sampled: Option<(Instant, u64, bool)>,
    /// Per queue, the head ready there and since when (G8's ageing).
    ready_since: Vec<Option<(BufferId, Instant)>>,
    /// The actions buffer every decision reuses.
    actions: Vec<Action>,
}

impl Decider {
    /// The configured policy and the baseline, both made from the registry, for a run of
    /// `queues` queues.
    pub(crate) fn new(
        policy: Box<dyn SchedulingPolicy>,
        baseline: Box<dyn SchedulingPolicy>,
        queues: usize,
    ) -> Decider {
        let name = policy.id().name.to_string();
        Decider {
            policy,
            baseline,
            stats: PolicyStats {
                name,
                ..PolicyStats::default()
            },
            durations: VecDeque::with_capacity(DURATIONS_KEPT),
            sampled: None,
            ready_since: vec![None; queues],
            actions: Vec::with_capacity(8),
        }
    }

    /// What the policy reads, and the baseline.
    pub(crate) fn reads(&self) -> (Reads, Reads) {
        (self.policy.reads(), self.baseline.reads())
    }

    /// How long each ready unit's head has waited for its stage: since the decision that first
    /// saw that head ready on its queue. Queues with no ready head are forgotten.
    fn age(&mut self, ready: &mut [Ready], now: Instant) {
        for slot in &mut self.ready_since {
            if let Some((id, _)) = slot
                && !ready.iter().any(|r| r.head.id == *id)
            {
                *slot = None;
            }
        }
        for unit in ready {
            let Some(slot) = self.ready_since.get_mut(usize::from(unit.queue)) else {
                continue;
            };
            let since = match slot {
                Some((id, at)) if *id == unit.head.id => *at,
                _ => {
                    *slot = Some((unit.head.id, now));
                    now
                }
            };
            unit.waited_ns = now.duration_since(since).as_nanos() as u64;
        }
    }

    /// The decision budget now: a share of the median recent unit, never below the floor. Read
    /// without allocating.
    pub(crate) fn budget_ns(&self) -> u64 {
        if self.durations.is_empty() {
            return DECISION_BUDGET_FLOOR_NS;
        }
        let mut kept = [0u64; DURATIONS_KEPT];
        let n = self.durations.len();
        for (slot, d) in kept.iter_mut().zip(&self.durations) {
            *slot = *d;
        }
        let median = *kept[..n].select_nth_unstable(n / 2).1;
        (median * DECISION_BUDGET_PERMILLE / 1000).max(DECISION_BUDGET_FLOOR_NS)
    }

    /// A unit ran for `ns`.
    pub(crate) fn finished(&mut self, ns: u64) {
        if self.durations.len() == DURATIONS_KEPT {
            self.durations.pop_front();
        }
        self.durations.push_back(ns);
        self.stats.overhead.units += 1;
        self.stats.overhead.unit_ns += ns;
    }

    /// The memory unused now, sampled: the time since the last sample counts toward
    /// `unused_ready_byte_ns` when a unit was ready then.
    fn sample_unused(&mut self, unused: u64, ready: bool) {
        let now = Instant::now();
        if let Some((at, before, was_ready)) = self.sampled
            && was_ready
        {
            let ns = now.duration_since(at).as_nanos();
            self.stats.unused_ready_byte_ns += u128::from(before) * ns;
        }
        self.sampled = Some((now, unused, ready));
    }

    /// What deciding has cost so far, and what the policy counted of its own decisions.
    pub(crate) fn stats(&self) -> PolicyStats {
        PolicyStats {
            counters: self.policy.counters(),
            ..self.stats.clone()
        }
    }
}

/// What one worker's decisions reuse, so a decision allocates nothing once its buffers have
/// grown to the run's shape: the ready units, their models' predictions, the stages' snapshot
/// and the critical-path weights.
#[derive(Default)]
pub(crate) struct Scratch {
    ready: Vec<Ready>,
    learn: Vec<Option<Arc<moruna_cost::Prediction>>>,
    views: Vec<StageView>,
    weights: Vec<u64>,
    /// By stage: a unit's median and the batches waiting, for the weights when the policy reads
    /// no snapshot.
    stage_units: Vec<(u64, u64, u16)>,
}

/// What a free worker does next.
pub(crate) enum Decision {
    /// Run the unit of `stage` on its `port`, under `claim`; `learn` is the cost model's
    /// prediction it is settled against when it finishes (f.22), when the run has a model.
    Run {
        stage: StageId,
        port: u16,
        claim: Reservation,
        learn: Option<Arc<moruna_cost::Prediction>>,
    },
    /// Nothing runs; `ready` says whether a unit was ready nonetheless (its claim was not
    /// granted, or the policy waited).
    Idle { ready: bool },
}

/// The tier the run's batches are pushed in: its one host tier (placement e.1).
pub(crate) fn host_tier(shared: &Shared) -> Tier {
    if shared.alloc.is_pinned() {
        Tier::PinnedHost
    } else {
        Tier::Host
    }
}

/// What asking for predictions cost one decision.
#[derive(Default)]
struct Asking {
    ns: u64,
    asked: u64,
    cached: u64,
}

/// What a unit of stage `stage_ix` on a batch of `bytes` is predicted to make and take (f.22):
/// the run's cost model's answer through its port, with its spread, from the stage's cache
/// while its model is unchanged, and the model's own to settle the unit against; with no model,
/// the stage's measured ratio of bytes out to bytes in so far (one before it has run) and its
/// mean unit time, each a point, marked `MeasuredRatio`.
fn prediction(
    shared: &Shared,
    stage_ix: usize,
    bytes: u64,
    asking: &mut Asking,
) -> (Prediction, Option<Arc<moruna_cost::Prediction>>) {
    if let Some(learning) = shared.learning.get() {
        let asked = learning.predict(shared, stage_ix, bytes);
        asking.ns += asked.ns;
        if asked.cached {
            asking.cached += 1;
        } else {
            asking.asked += 1;
        }
        return (asked.prediction, Some(asked.model));
    }
    let entry = &shared.stages[stage_ix];
    let (made, taken) = (
        entry.bytes_out.load(Ordering::SeqCst),
        entry.bytes_in.load(Ordering::SeqCst),
    );
    let out = if taken == 0 {
        bytes
    } else {
        (u128::from(bytes) * u128::from(made)).div_ceil(u128::from(taken)) as u64
    };
    let (busy, tasks) = (
        entry.busy_ns.load(Ordering::SeqCst),
        entry.tasks.load(Ordering::SeqCst),
    );
    let time = busy.checked_div(tasks).unwrap_or(0);
    (
        Prediction {
            out_bytes: Spread::exact(out),
            duration_ns: Spread::exact(time),
            by: PredictedBy::MeasuredRatio,
            drifting: false,
            observations: 0,
        },
        None,
    )
}

/// The most a unit's claim may be (f.22): the budget above the run's floor, plus the one batch
/// the floor keeps for a unit's output. A claim above it could wait for ever behind what the
/// floor holds, so a model that predicts too much is held to it; an output larger than its claim
/// is topped up or goes to disk (f.20), so the hard rule never rests on the prediction. The
/// floor is the one the scheduler keeps (`Shared::floor_bytes`), counted at `new` and again when
/// it forwards a promotion window, so a decision locks no queue for it.
fn claim_cap(shared: &Shared, budget: u64) -> u64 {
    budget
        .saturating_sub(shared.floor_bytes.load(Ordering::Relaxed))
        .saturating_add(shared.cfg.morsel_min)
}

/// The units ready for `worker` (9.1) into `scratch`: a stage it may serve, none of whose output
/// queues is full, on each input port whose head is resident for the spec the stage wants;
/// each with its prediction and the model's own, aligned.
fn ready_units(
    shared: &Shared,
    worker: u16,
    stats: &PlacementStats,
    scratch: &mut Scratch,
    asking: &mut Asking,
) {
    scratch.ready.clear();
    scratch.learn.clear();
    for (index, entry) in shared.stages.iter().enumerate() {
        if !crate::instances::admissible(shared, index, worker) {
            continue;
        }
        if entry.outputs.iter().any(|&q| shared.placement.is_full(q)) {
            continue;
        }
        let out_bytes: u64 = entry
            .outputs
            .iter()
            .map(|&q| Shared::queue_bytes(stats, q))
            .sum();
        for (port, &q) in entry.inputs.iter().enumerate() {
            if !shared.placement.peek_resident(q, entry.spec, Locality::Any) {
                continue;
            }
            let Some(head) = shared.placement.head(q) else {
                continue;
            };
            let (predicted, model) = prediction(shared, index, head.bytes, asking);
            scratch.ready.push(Ready {
                stage: entry.stage,
                port: port as u16,
                queue: q,
                head,
                queue_bytes: Shared::queue_bytes(stats, q),
                out_bytes,
                predicted,
                critical_ns: 0,
                waited_ns: 0,
            });
            scratch.learn.push(model);
        }
    }
}

/// A stage's typical batch: its mean input so far, else its backlog's mean batch, else the
/// morsel target.
fn typical(shared: &Shared, stage_ix: usize, batches: u64, queued: u64) -> u64 {
    let entry = &shared.stages[stage_ix];
    let (taken, bytes_in) = (
        entry.taken.load(Ordering::SeqCst),
        entry.bytes_in.load(Ordering::SeqCst),
    );
    bytes_in
        .checked_div(taken)
        .or_else(|| queued.checked_div(batches))
        .unwrap_or_else(|| shared.knobs.morsel_target(entry.stage))
}

/// How many of a stage's units may run at once: its instances for a stateful stage, the active
/// workers otherwise.
fn stage_cap(shared: &Shared, stage_ix: usize, workers: u16) -> u16 {
    let cap = match &shared.stages[stage_ix].pool {
        Some(pool) => u16::try_from(pool.max).unwrap_or(u16::MAX),
        None => workers,
    };
    cap.clamp(1, workers)
}

/// A stage's unit prediction: a ready head's of the stage when it has one, else at its typical
/// batch, so a decision asks the model once per stage at most beside its ready units.
fn stage_unit(
    shared: &Shared,
    stage_ix: usize,
    ready: &[Ready],
    batches: u64,
    queued: u64,
    asking: &mut Asking,
) -> Prediction {
    let stage = shared.stages[stage_ix].stage;
    match ready.iter().find(|r| r.stage == stage) {
        Some(r) => r.predicted,
        None => {
            prediction(
                shared,
                stage_ix,
                typical(shared, stage_ix, batches, queued),
                asking,
            )
            .0
        }
    }
}

/// Every kernel stage as the snapshot reads it (9.4 step 3), into `scratch.views`, its buffers
/// reused: its ports' backlogs in memory and on disk, its units under way by how long each has
/// run (from the heartbeat table), how many may run at once, and a unit's prediction.
fn stage_views(
    shared: &Shared,
    stats: &PlacementStats,
    tier: Tier,
    scratch: &mut Scratch,
    asking: &mut Asking,
) {
    let now = crate::cputime::now_ns();
    let workers = shared.knobs.active_workers().max(1);
    scratch
        .views
        .resize_with(shared.stages.len(), StageView::default);
    for (index, entry) in shared.stages.iter().enumerate() {
        let view = &mut scratch.views[index];
        view.stage = entry.stage;
        view.ports.clear();
        view.running_ns.clear();
        let (mut batches, mut queued) = (0u64, 0u64);
        for &q in &entry.inputs {
            let queue = stats.queues.get(usize::from(q));
            let port = PortView {
                queue: q,
                batches: queue.map_or(0, |s| s.count),
                memory_bytes: queue.map_or(0, |s| s.bytes_by_tier[tier.index()]),
                disk_bytes: queue.map_or(0, |s| s.bytes_by_tier[TierKind::Disk.index()]),
            };
            batches += port.batches;
            queued += port.memory_bytes + port.disk_bytes;
            view.ports.push(port);
        }
        view.cap = stage_cap(shared, index, workers);
        view.unit = stage_unit(shared, index, &scratch.ready, batches, queued, asking);
    }
    for row in &shared.heartbeat.rows {
        if row.running.load(Ordering::SeqCst) {
            let stage = row.task_stage.load(Ordering::SeqCst) as usize;
            if let Some(view) = stage.checked_sub(1).and_then(|i| scratch.views.get_mut(i)) {
                view.running_ns
                    .push(now.saturating_sub(row.task_start_ns.load(Ordering::SeqCst)));
            }
        }
    }
}

/// Each stage's critical-path weight (9.4 step 2, G8), into `weights`: the predicted work
/// queued for it (its backlog's units times a unit's median, over the units it may run at once)
/// plus the heaviest such weight among the stages its output feeds. `units` gives, by stage, a
/// unit's median, the batches waiting and the cap. Stages are numbered so every queue goes
/// forward, so one pass from the last stage back does it.
pub(crate) fn critical(
    units: impl Fn(usize) -> (u64, u64, u16),
    stages: usize,
    topology: &moruna_kernel::Topology,
    weights: &mut Vec<u64>,
) {
    weights.clear();
    weights.resize(stages, 0);
    for index in (0..stages).rev() {
        let (median, backlog, cap) = units(index);
        let own = backlog
            .saturating_mul(median)
            .checked_div(u64::from(cap.max(1)))
            .unwrap_or(0);
        let stage = index as StageId + 1;
        let downstream = topology
            .outputs(stage)
            .iter()
            .filter_map(|&q| match topology.link(q).map(|l| l.to) {
                Some(Consumer::Kernel { stage, .. }) => {
                    usize::from(stage).checked_sub(1).map(|i| weights[i])
                }
                _ => None,
            })
            .max()
            .unwrap_or(0);
        weights[index] = own.saturating_add(downstream);
    }
}

/// Decide for a free worker (9.4): the ready units, the policy's actions within its budget,
/// and the first `Run` whose claim is granted. Only what the policy deciding this time reads is
/// gathered (`SchedulingPolicy::reads`): `admission` and `fifo` pay for no snapshot, no weights
/// and no ageing. The gathering is timed with the policy's `decide` against the budget. A
/// policy's `Spill` or `Reload` the catalog refuses ends the run: the policy asked for what the
/// catalog's rules forbid.
pub(crate) fn decide(shared: &Shared, worker: u16) -> Result<Decision> {
    if shared.gated() {
        return Ok(Decision::Idle { ready: false });
    }
    let began = Instant::now();
    let mut guard = shared.scratch[usize::from(worker)]
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let scratch = &mut *guard;
    let stats = shared.placement_stats();
    let mut asking = Asking::default();
    ready_units(shared, worker, &stats, scratch, &mut asking);
    let tier = host_tier(shared);
    let slot = tier.index();
    let catalog = &stats.catalog;
    let held = catalog.resident_by_tier[slot].saturating_add(catalog.reserved_by_tier[slot]);
    let budget = catalog.budget_by_tier[slot];
    let unused = budget.saturating_sub(held);
    let cap = claim_cap(shared, budget);
    // Whether the baseline decides is read before the gathering, so only what it reads is
    // gathered; it is claimed under the decision lock below.
    let baseline = shared.fallback_next.load(Ordering::SeqCst);
    let reads = if baseline {
        shared.reads.1
    } else {
        shared.reads.0
    };
    let mut snapshot_ns = 0;
    if !scratch.ready.is_empty() {
        let workers = shared.knobs.active_workers().max(1);
        if reads.stages {
            let at = Instant::now();
            stage_views(shared, &stats, tier, scratch, &mut asking);
            snapshot_ns = at.elapsed().as_nanos() as u64;
        } else {
            scratch.views.clear();
        }
        if reads.critical_path {
            if !reads.stages {
                scratch.stage_units.clear();
                for index in 0..shared.stages.len() {
                    let (mut batches, mut queued) = (0u64, 0u64);
                    for &q in &shared.stages[index].inputs {
                        if let Some(s) = stats.queues.get(usize::from(q)) {
                            batches += s.count;
                            queued += s.bytes_by_tier.iter().sum::<u64>();
                        }
                    }
                    let unit =
                        stage_unit(shared, index, &scratch.ready, batches, queued, &mut asking);
                    scratch.stage_units.push((
                        unit.duration_ns.p50,
                        batches,
                        stage_cap(shared, index, workers),
                    ));
                }
                let units = &scratch.stage_units;
                critical(
                    |i| units[i],
                    shared.stages.len(),
                    &shared.topology,
                    &mut scratch.weights,
                );
            } else {
                let views = &scratch.views;
                critical(
                    |i| {
                        let v = &views[i];
                        (
                            v.unit.duration_ns.p50,
                            v.ports.iter().map(|p| p.batches).sum(),
                            v.cap,
                        )
                    },
                    shared.stages.len(),
                    &shared.topology,
                    &mut scratch.weights,
                );
            }
            for unit in &mut scratch.ready {
                unit.critical_ns = scratch.weights[usize::from(unit.stage) - 1];
            }
        }
    }
    let machine = Machine {
        workers: shared.knobs.active_workers().max(1),
        budget_bytes: budget,
        held_bytes: held,
        disk_bytes_per_second: shared.cfg.disk_bytes_per_second,
        claim_cap_bytes: cap,
    };
    let mut decider = shared.decider.lock().unwrap_or_else(|e| e.into_inner());
    let decider = &mut *decider;
    decider.sample_unused(unused, !scratch.ready.is_empty());
    if scratch.ready.is_empty() {
        return Ok(Decision::Idle { ready: false });
    }
    if reads.waiting {
        decider.age(&mut scratch.ready, Instant::now());
    }
    let budget_ns = decider.budget_ns();
    let state = SchedulerState {
        worker,
        ready: &scratch.ready,
        topology: &shared.topology,
        residency: &*shared.placement,
        budget_ns,
        machine,
        stages: &scratch.views,
    };
    let mut actions = std::mem::take(&mut decider.actions);
    actions.clear();
    let gathered = began.elapsed().as_nanos() as u64;
    let started = Instant::now();
    if baseline {
        // Another worker may have claimed the hand-over since; this decision is the baseline's
        // either way, since only what the baseline reads was gathered, but it is the fallback
        // only once.
        if shared.fallback_next.swap(false, Ordering::SeqCst) {
            decider.stats.fallbacks += 1;
        }
        decider.baseline.decide(&state, &mut actions);
    } else {
        decider.policy.decide(&state, &mut actions);
    }
    let decided = started.elapsed().as_nanos() as u64;
    let took = gathered + decided;
    let o = &mut decider.stats.overhead;
    o.gather_ns += gathered;
    o.decide_ns += decided;
    o.predict_ns += asking.ns;
    o.predictions += asking.asked;
    o.cached += asking.cached;
    o.snapshot_ns += snapshot_ns;
    decider.stats.decisions += 1;
    decider.stats.decision_ns += took;
    if took > budget_ns {
        decider.stats.over_budget += 1;
        // 9.4 step 4: the next decision uses the shallower policy, and the run says so.
        if !baseline {
            shared.fallback_next.store(true, Ordering::SeqCst);
        }
    }
    let mut refused = 0u64;
    let mut decision = Decision::Idle { ready: true };
    let mut outcome = Ok(());
    for &action in &actions {
        match action {
            Action::Run { stage, port } => {
                let Some(at) = scratch
                    .ready
                    .iter()
                    .position(|r| r.stage == stage && r.port == port)
                else {
                    outcome = Err(MorunaError::Plan(format!(
                        "the policy chose stage {stage} port {port}, which is not ready"
                    )));
                    break;
                };
                let unit = &scratch.ready[at];
                // The prediction's 95th percentile, held to what the floor leaves room for
                // (`claim_cap`). A take that is its batch's last gives the batch's bytes back
                // before the unit's output is pushed (f.20), so only the growth needs claiming.
                let predicted = unit.predicted.claim().min(cap);
                let claim = if unit.head.last {
                    predicted.saturating_sub(unit.head.bytes)
                } else {
                    predicted
                };
                match shared.placement.reserve(tier, claim) {
                    Ok(Reserve::Granted(claim)) => {
                        decision = Decision::Run {
                            stage,
                            port,
                            claim,
                            learn: scratch.learn[at].clone(),
                        };
                        match unit.predicted.by {
                            PredictedBy::Model => decider.stats.predicted_by_model += 1,
                            PredictedBy::MeasuredRatio => decider.stats.predicted_by_ratio += 1,
                        }
                        break;
                    }
                    Ok(Reserve::Pending { .. } | Reserve::Refused { .. }) => refused += 1,
                    Err(e) => {
                        outcome = Err(e);
                        break;
                    }
                }
            }
            Action::Spill(batch) => {
                if let Err(e) = shared.placement.spill(batch) {
                    outcome = Err(e);
                    break;
                }
            }
            Action::Reload(batch) => {
                if let Err(e) = shared.placement.reload(batch) {
                    outcome = Err(e);
                    break;
                }
            }
            Action::Wait => break,
        }
    }
    decider.actions = actions;
    decider.stats.not_claimed += refused;
    if matches!(decision, Decision::Run { .. }) {
        decider.stats.admitted += 1;
    }
    outcome.map(|()| decision)
}

/// A worker idled `ns` while a unit was ready.
pub(crate) fn idled_while_ready(shared: &Shared, ns: u64) {
    let mut decider = shared.decider.lock().unwrap_or_else(|e| e.into_inner());
    decider.stats.idle_ready_ns += ns;
}

/// A unit ran for `ns`: what the decision budget is a share of.
pub(crate) fn unit_finished(shared: &Shared, ns: u64) {
    let mut decider = shared.decider.lock().unwrap_or_else(|e| e.into_inner());
    decider.finished(ns);
}

/// True while the run should keep waiting for room.
fn live(shared: &Shared) -> bool {
    !shared.stopping.load(Ordering::SeqCst) && !shared.is_cancelled() && !shared.has_exit()
}

/// Push `morsel` into `stages` under a claim (12.5): `claim` when the unit ran under one, topped
/// up to the batch's bytes; a pending top-up waits for the room on its way; a refused one writes
/// the batch straight to disk; with no disk to write to, it waits for consumers to make room,
/// and after `PUSH_STALL` with none the run ends saying why. A shared share of the payload is
/// what `push_spilled` is handed, so a refusal loses nothing.
pub(crate) fn admit_push(
    shared: &Shared,
    stages: &[StageId],
    morsel: Morsel,
    claim: Option<Reservation>,
) -> Result<()> {
    let tier = morsel.payload.tier();
    let bytes = morsel.bytes;
    let mut claim = match claim {
        Some(claim) if claim.slot() == tier.index() => claim,
        // A claim in another tier (a device output) is given back as it is dropped.
        Some(_) | None => match shared.placement.reserve(tier, 0)? {
            Reserve::Granted(claim) => claim,
            Reserve::Pending { .. } | Reserve::Refused { .. } => {
                return Err(MorunaError::Staging(
                    "a claim of no bytes was not granted".into(),
                ));
            }
        },
    };
    let clock = &shared.cfg.clock;
    let started = clock.now();
    let mut waited = false;
    let _waiting = Waiting::enter(shared);
    loop {
        if claim.bytes() >= bytes {
            return shared.placement.push_reserved(stages, morsel, claim);
        }
        let more = bytes - claim.bytes();
        let last = match shared.placement.reserve(tier, more)? {
            Reserve::Granted(top) => {
                claim.join(top)?;
                continue;
            }
            Reserve::Pending { need } => format!("{need} bytes are on their way out"),
            Reserve::Refused { need, budget } => {
                let copy = Morsel {
                    seq: morsel.seq,
                    stage: morsel.stage,
                    payload: morsel.payload.share(),
                    bytes: morsel.bytes,
                    origin: morsel.origin.clone(),
                    features: morsel.features.clone(),
                };
                match shared.placement.push_spilled(stages, copy) {
                    Ok(()) => {
                        let mut decider = shared.decider.lock().unwrap_or_else(|e| e.into_inner());
                        decider.stats.overflow_spills += 1;
                        return Ok(());
                    }
                    Err(e) => format!("{need} bytes more than a budget of {budget} holds: {e}"),
                }
            }
        };
        if !waited {
            waited = true;
            let mut decider = shared.decider.lock().unwrap_or_else(|e| e.into_inner());
            decider.stats.push_waits += 1;
        }
        if !live(shared) {
            return Err(MorunaError::Cancelled);
        }
        if clock.now().saturating_sub(started) >= PUSH_STALL {
            return Err(MorunaError::Staging(format!(
                "morsel {} ({bytes} bytes) waited {} s for room in {stages:?} and none came: {last}",
                morsel.seq,
                PUSH_STALL.as_secs()
            )));
        }
        clock.pause(PUSH_PARK);
    }
}

/// `replace` of an evicted batch under a claim, waiting for room as `admit_push` does (a
/// replacement cannot go straight to disk: it is what its consumer is waiting for in memory).
pub(crate) fn admit_replace(shared: &Shared, stage: StageId, morsel: Morsel) -> Result<()> {
    let tier = morsel.payload.tier();
    let clock = &shared.cfg.clock;
    let started = clock.now();
    let _waiting = Waiting::enter(shared);
    loop {
        match shared.placement.reserve(tier, morsel.bytes)? {
            Reserve::Granted(claim) => {
                return shared.placement.replace_reserved(stage, morsel, claim);
            }
            Reserve::Pending { .. } | Reserve::Refused { .. } => {}
        }
        if !live(shared) {
            return Err(MorunaError::Cancelled);
        }
        if clock.now().saturating_sub(started) >= PUSH_STALL {
            return Err(MorunaError::Staging(format!(
                "the replacement of morsel {} ({} bytes) on queue {stage} waited {} s for room \
                 and none came",
                morsel.seq,
                morsel.bytes,
                PUSH_STALL.as_secs()
            )));
        }
        clock.pause(PUSH_PARK);
    }
}

/// The unit a free worker would run now, its claim given straight back: what SC-T2 asserts the
/// admission rule on.
#[cfg(test)]
pub(crate) fn chosen(shared: &Shared, worker: u16) -> Option<(StageId, u16)> {
    match decide(shared, worker) {
        Ok(Decision::Run { stage, port, .. }) => Some((stage, port)),
        Ok(Decision::Idle { .. }) | Err(_) => None,
    }
}

/// A push is under way: the stall detector leaves the run alone while one waits, because a
/// waiting push ends itself with its own diagnostic after `PUSH_STALL`.
struct Waiting<'a>(&'a Shared);

impl<'a> Waiting<'a> {
    fn enter(shared: &'a Shared) -> Waiting<'a> {
        shared.pushes_waiting.fetch_add(1, Ordering::SeqCst);
        Waiting(shared)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.pushes_waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The run's floor (12.5, placement f.18 rule 13): every queue's promotion window and one
/// unit's input and output, at the smallest batch the scheduler can make, `morsel.min_bytes`.
/// A budget under it is refused before anything runs, naming the floor and its terms; the
/// floor's bytes otherwise, which the claim cap is counted from (f.22).
pub(crate) fn check_floor(
    placement: &dyn moruna_kernel::Placement,
    tier: Tier,
    batch: u64,
) -> Result<u64> {
    let floor = placement.floor(tier, batch);
    if floor.holds() {
        return Ok(floor.bytes);
    }
    Err(MorunaError::Config {
        name: "budget.host",
        msg: format!(
            "the placement's budget of {} bytes in {tier:?} is below this run's floor of {}; \
             a head on disk could not be read back. Give the run at least {} bytes, or smaller \
             morsels",
            floor.budget,
            floor.terms(),
            floor.bytes
        ),
    })
}

/// The stall detector (12.5's backstop, f.21): at each heartbeat check, the run is quiescent when
/// it is live and not gated, no worker is busy, no read, write or move is in flight, no push is
/// waiting, and no claim is outstanding. Quiescent at two checks in a row with nothing it counts
/// as progress moved between them (units taken, pushes from sources, writes, frees, releases,
/// spills, reloads, evictions), no event can come that would move it: no unit can make progress,
/// and the run ends naming every queue's batches and bytes and the catalog's budget.
pub(crate) fn stall_check(shared: &Shared) {
    if !shared.run_state().is_live() || shared.gated() || shared.has_exit() {
        *shared.stall.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return;
    }
    let stats = shared.placement.stats();
    let quiet = shared.knobs.slots.busy() == 0
        && shared.reads_in_flight.load(Ordering::SeqCst) == 0
        && shared.writes_in_flight.load(Ordering::SeqCst) == 0
        && shared.pushes_waiting.load(Ordering::SeqCst) == 0
        && stats.in_flight_bytes == 0
        && stats.catalog.reserved_by_tier.iter().all(|b| *b == 0)
        && stats.queues.iter().any(|q| q.count > 0);
    let mut seen = shared.stall.lock().unwrap_or_else(|e| e.into_inner());
    if !quiet {
        *seen = None;
        return;
    }
    let c = &stats.catalog;
    let mut signature = vec![
        c.released,
        c.freed,
        c.spills,
        c.reloads,
        c.evictions,
        c.overflows,
    ];
    signature.extend(shared.stages.iter().map(|e| e.taken.load(Ordering::SeqCst)));
    signature.extend(
        shared
            .sources
            .iter()
            .map(|s| s.out.morsels.load(Ordering::SeqCst)),
    );
    signature.extend(
        shared
            .sinks
            .iter()
            .map(|s| s.input.morsels.load(Ordering::SeqCst)),
    );
    if seen.as_ref() != Some(&signature) {
        *seen = Some(signature);
        return;
    }
    drop(seen);
    let tier = host_tier(shared);
    let queues: Vec<String> = stats
        .queues
        .iter()
        .filter(|q| q.count > 0)
        .map(|q| {
            let head = shared.placement.head(q.stage);
            format!(
                "queue {} holds {} batches, {} bytes in memory and {} on disk, its head {}",
                q.stage,
                q.count,
                q.bytes_by_tier[tier.index()],
                q.bytes_by_tier[moruna_kernel::TierKind::Disk.index()],
                match head {
                    Some(h) => format!("batch {} of {} bytes", h.seq, h.bytes),
                    None => "unknown".to_string(),
                }
            )
        })
        .collect();
    crate::policy::terminate(
        shared,
        MorunaError::Staging(format!(
            "no unit can make progress: nothing is running, nothing is in flight, no claim is \
             granted and nothing more can be read back, with {} of {} budget bytes resident; {}",
            c.resident_by_tier[tier.index()],
            c.budget_by_tier[tier.index()],
            queues.join("; ")
        )),
    );
}
