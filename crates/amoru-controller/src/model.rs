//! The working-set model: the envelope, the equation of b, and the initial knobs of f.3.
//!
//! The budget is divided once and the division is what every later decision moves inside.
//! Writing it out, with `avail = host_budget - state`:
//!
//! ```text
//! host_budget = state                                   the bytes instances hold
//!             + avail / 2                               the workers' in-flight allowance
//!             + avail / 2                               the queues and the read-ahead
//! ```
//!
//! The read-ahead's bytes land in Q0, so they are spent out of the queue half rather than on
//! top of it; that is what makes the inequality of b hold as an equality of the budget rather
//! than as a hope, and it is why f.3 lowers the read-ahead when its bytes do not fit.

use amoru_kernel::{Knob, TierKind};

use crate::{
    Actions, ControllerState, Envelope, Inner, MIN_AMPLIFICATION, Phase, Result, apply, budget,
    tick,
};

/// The share of the device budget one stage's in-flight morsels may hold (f.3).
const DEVICE_WORKER_SHARE: f64 = 0.4;
/// The most of the queue half the read-ahead may take before f.3 lowers it.
const READ_AHEAD_SHARE: f64 = 0.5;
/// The read-ahead depth f.3 starts from (`readahead.splits`).
const READ_AHEAD_INITIAL: u16 = 2;
/// The promotion window f.3 starts from (`queue.promotion_window`).
const PROMOTION_WINDOW_INITIAL: u16 = 2;
/// The read-ahead the tiny-dataset path allows at most (f.10).
const TINY_READ_AHEAD_MAX: u32 = 8;
/// The fraction of the budget below which a dataset needs no adaptation (f.10).
const TINY_FRACTION: u64 = 4;

/// `bytes x factor`, saturating and never negative (l, numerics).
pub(crate) fn scale(bytes: u64, factor: f64) -> u64 {
    if !factor.is_finite() || factor <= 0.0 || bytes == 0 {
        return 0;
    }
    let scaled = bytes as f64 * factor;
    if scaled >= u64::MAX as f64 {
        u64::MAX
    } else {
        scaled as u64
    }
}

/// The host tier the arena actually has (contracts e.1): a pinned arena puts the queues in
/// pinned host memory, an unpinned one in ordinary host memory, and never both.
pub(crate) fn host_tier(state: &ControllerState) -> TierKind {
    if state.cfg.pinned {
        TierKind::PinnedHost
    } else {
        TierKind::Host
    }
}

/// `controller.damping_completions`: the active worker count, clamped to its range.
pub(crate) fn damping(state: &ControllerState) -> u32 {
    u32::from(state.active_workers).clamp(1, 64)
}

/// The state term of f.3: the bytes a stage's instances hold whatever the morsel size is.
///
/// The three sources are the kernel's hint, the profile's high-water mark and what the trace
/// has actually reported, the largest of the three per instance, because a kernel whose state
/// grows with morsels seen is exactly the case the linear model `a_k x bytes_in` cannot see.
pub(crate) fn recompute_state(state: &mut ControllerState) {
    let mut total = 0u64;
    for at in 0..state.stages.len() {
        let stage = state.stages[at].stage;
        let hint = state.hints(stage).state_bytes.unwrap_or(0);
        let from_profile = state.stages[at]
            .profile
            .as_ref()
            .map(|p| p.state_bytes_max)
            .unwrap_or(0);
        let observed = state.stages[at]
            .state_by_instance
            .values()
            .copied()
            .max()
            .unwrap_or(0);
        let per_instance = hint.max(from_profile).max(observed);
        let instances = u64::from(state.stages[at].instances_live.max(1));
        let stage_state = per_instance.saturating_mul(instances);
        state.stages[at].state_bytes = stage_state;
        total = total.saturating_add(stage_state);
    }
    state.state_total = total;
}

/// The bytes left once the instances have been funded (f.3).
pub(crate) fn available(state: &ControllerState) -> u64 {
    state.budgets.host.saturating_sub(state.state_total)
}

/// The workers' half of what is left (f.3).
pub(crate) fn worker_half(state: &ControllerState) -> u64 {
    available(state) / 2
}

/// The queues' half of what is left, including the read-ahead's bytes (f.3).
pub(crate) fn queue_half(state: &ControllerState) -> u64 {
    available(state) - worker_half(state)
}

/// How many morsels of one stage may be in flight at once: one per worker's share of the
/// stages, but never more morsels than the dataset has at that target.
pub(crate) fn share(state: &ControllerState, target: u64) -> f64 {
    let stages = state.stage_count().max(1) as f64;
    let per_stage = f64::from(state.active_workers) / stages;
    if target == 0 {
        return per_stage;
    }
    let morsels = state.cfg.plan.total_bytes.div_ceil(target).max(1) as f64;
    per_stage.min(morsels)
}

/// The envelope of b: the closed interval of morsel targets the sizer may propose for a stage.
pub(crate) fn envelope(state: &ControllerState, at: usize) -> Envelope {
    let stages = state.stage_count().max(1) as u64;
    let budget_for_stage = worker_half(state) / stages;
    let ctl = &state.stages[at];
    let per_byte = share(state, ctl.target) * ctl.a_k * f64::from(ctl.safety);
    let from_budget = if per_byte <= 0.0 {
        state.cfg.morsel_max
    } else {
        scale(budget_for_stage, 1.0 / per_byte)
    };
    Envelope {
        min: state.cfg.morsel_min,
        max: from_budget
            .min(state.cfg.morsel_max)
            .max(state.cfg.morsel_min),
    }
}

/// Recompute every stage's envelope from the budget and the amplification as they stand.
pub(crate) fn refresh_envelopes(state: &mut ControllerState) {
    for at in 0..state.stages.len() {
        let envelope = envelope(state, at);
        state.stages[at].envelope = envelope;
    }
}

/// The working set of b for a proposed set of targets, in bytes.
pub(crate) fn working_set(state: &ControllerState, targets: &[u64]) -> u64 {
    let mut total = state.state_total;
    for (at, target) in targets.iter().enumerate() {
        let ctl = &state.stages[at];
        let allowance = ctl.allowance(*target);
        total = total.saturating_add(scale(allowance, share(state, *target)));
    }
    let queues = state.queue_count() as u64;
    total = total.saturating_add(state.high_water.saturating_mul(queues));
    total = total.saturating_add(u64::from(state.read_ahead).saturating_mul(state.split_bytes));
    total
}

/// RC-I1: whether a proposed set of targets fits the budget.
pub(crate) fn fits(state: &ControllerState, targets: &[u64]) -> bool {
    working_set(state, targets) <= state.budgets.host
}

/// RC-I1: reduce morsel targets, largest stage first, until the working set fits. Returns the
/// targets that fit, which is what the controller then writes and nothing else.
pub(crate) fn enforce(state: &ControllerState, targets: &[u64]) -> Vec<u64> {
    let mut targets = targets.to_vec();
    // Each pass halves the largest target. Every target is bounded below by `morsel_min`, so
    // this terminates: at worst every stage reaches the floor, and a floor that still does not
    // fit is a budget the run cannot be sized for, which f.7 turns into a diagnostic rather
    // than into an unbounded loop here.
    for _ in 0..64 {
        if fits(state, &targets) {
            break;
        }
        let Some(largest) = largest_index(&targets) else {
            break;
        };
        let reduced = (targets[largest] / 2).max(state.cfg.morsel_min);
        if reduced == targets[largest] {
            break;
        }
        targets[largest] = reduced;
    }
    targets
}

fn largest_index(targets: &[u64]) -> Option<usize> {
    targets
        .iter()
        .enumerate()
        .max_by_key(|(_, bytes)| **bytes)
        .map(|(at, _)| at)
}

/// The initial solution of f.3: one common target `t`, the worker count memory can feed, the
/// read-ahead that fits, and the queue high waters.
pub(crate) struct Solution {
    pub targets: Vec<u64>,
    pub active_workers: u16,
    pub read_ahead: u16,
    pub split_bytes: u64,
    pub high_water: u64,
    pub device_high_water: u64,
}

/// f.3, the solve. `state.active_workers` is the `W` this is called with; the worker count it
/// returns is what RC-I7 allows.
pub(crate) fn solve(state: &ControllerState) -> Solution {
    let stages = state.stage_count();
    let worker_half = worker_half(state);
    let queue_half = queue_half(state);

    let target = if state.tiny {
        // f.10. A dataset smaller than a quarter of the budget cannot fill the pipeline, so
        // there is nothing to adapt to: take the largest morsel allowed and stop moving.
        state.cfg.morsel_max
    } else {
        common_target(state, worker_half)
    };
    let targets: Vec<u64> = (0..stages)
        .map(|_| target.clamp(state.cfg.morsel_min, state.cfg.morsel_max))
        .collect();

    let max_allowance = state
        .stages
        .iter()
        .zip(targets.iter())
        .map(|(ctl, target)| ctl.allowance(*target))
        .max()
        .unwrap_or(0);
    // RC-I7: no more workers than the budget can feed one morsel each.
    let active_workers = match worker_half.checked_div(max_allowance) {
        Some(allowed) if !state.tiny => u16::try_from(allowed)
            .unwrap_or(u16::MAX)
            .clamp(1, state.cfg.workers_max.max(1)),
        // A dataset that needs no adaptation, or a stage with no allowance yet to divide by,
        // gets every worker the host has (f.10, f.3).
        _ => state.cfg.workers_max.max(1),
    };

    let split_bytes = split_bytes(
        state,
        targets.first().copied().unwrap_or(state.cfg.morsel_max),
    );
    let read_ahead = read_ahead(state, queue_half, split_bytes);
    let for_queues = queue_half.saturating_sub(u64::from(read_ahead).saturating_mul(split_bytes));
    let queues = state.queue_count() as u64;
    let high_water = for_queues / queues.max(1);
    let device_high_water =
        scale(state.budgets.device[0], 1.0 - DEVICE_WORKER_SHARE) / queues.max(1);

    Solution {
        targets,
        active_workers,
        read_ahead,
        split_bytes,
        high_water,
        device_high_water,
    }
}

/// The one common target `t` of f.3: the largest target that keeps every stage's share of the
/// worker half within it, and every device stage within its share of the device budget.
fn common_target(state: &ControllerState, worker_half: u64) -> u64 {
    let stages = state.stage_count();
    if stages == 0 {
        return state.cfg.morsel_max;
    }
    let share_each = f64::from(state.active_workers) / stages as f64;
    let sum_amplified: f64 = state
        .stages
        .iter()
        .map(|ctl| ctl.a_k.max(MIN_AMPLIFICATION) * f64::from(ctl.safety))
        .sum();
    let mut target = if share_each <= 0.0 || sum_amplified <= 0.0 {
        state.cfg.morsel_max
    } else {
        scale(worker_half, 1.0 / (share_each * sum_amplified))
    };
    // The device is a second budget, not a second opinion: a stage that allocates on the
    // device is held to whichever of the two budgets binds first.
    for ctl in &state.stages {
        if ctl.a_k_dev <= 0.0 {
            continue;
        }
        let device_allowance = scale(state.budgets.device[0], DEVICE_WORKER_SHARE);
        let per_byte = share_each * ctl.a_k_dev * f64::from(ctl.safety);
        if per_byte > 0.0 {
            target = target.min(scale(device_allowance, 1.0 / per_byte));
        }
    }
    target
}

/// The bytes one read-ahead slot holds: the stage 1 target when the source can be read in row
/// ranges, and the whole split when it cannot, because a split that cannot be sub-split
/// arrives whole whatever the target says (f.3).
fn split_bytes(state: &ControllerState, first_target: u64) -> u64 {
    if state.cfg.plan.sub_splittable_all {
        first_target
    } else {
        first_target.max(state.cfg.plan.max_split_bytes)
    }
}

/// The read-ahead depth that fits in the queue half (f.3), floor 1.
fn read_ahead(state: &ControllerState, queue_half: u64, split_bytes: u64) -> u16 {
    if state.tiny {
        let splits = state.cfg.plan.splits.clamp(1, TINY_READ_AHEAD_MAX);
        return u16::try_from(splits).unwrap_or(u16::MAX);
    }
    if split_bytes == 0 {
        return READ_AHEAD_INITIAL;
    }
    let room = scale(queue_half, READ_AHEAD_SHARE) / split_bytes;
    let allowed = u16::try_from(room).unwrap_or(u16::MAX);
    allowed.clamp(1, READ_AHEAD_INITIAL)
}

/// f.3. The initial knobs, then the tick thread.
pub(crate) fn start(ctl: &Inner) -> Result<()> {
    let actions = {
        let mut state = ctl.held();
        // RC-I6 is the controller's own rule and not the facade's good manners: a caller that
        // skipped `probe_all` would be sized from a hint or from 4.0, which is exactly the
        // guess the probe exists to replace. Both probe entry points reach `Probed`, including
        // on the tiny-dataset and zero-kernel paths that have nothing to probe for.
        if state.phase != Phase::Probed {
            return Err(amoru_kernel::AmoruError::Config {
                name: "controller",
                msg: format!(
                    "start called in phase {:?}: probe_all or probe_missing must run first \
                     (RC-I6)",
                    state.phase
                ),
            });
        }
        state.active_workers = state.cfg.workers_max.max(1);
        initial_knobs(&mut state)
    };
    ctl.perform(actions);
    // e.2: the records that arrived before `Running` (the probes' own) are processed here, so
    // the first tick sees a controller that already knows what the probes measured.
    tick::absorb_queue(ctl);
    Ok(())
}

/// The knob set f.3 writes at `start`, in the order f.3 fixes: high waters, promotion windows,
/// morsel targets, active workers, read-ahead, with the RC-I1 check before the batch.
pub(crate) fn initial_knobs(state: &mut ControllerState) -> Actions {
    recompute_state(state);
    let mut actions = Actions::default();

    if state.stage_count() == 0 {
        // The zero-kernel path of f.3: there is no stage to size for, so the source drive
        // reads at a target of its own and one worker moves the morsels through.
        let bytes = state.cfg.morsel_max.min(state.budgets.host / 2);
        state.active_workers = 1;
        state.read_ahead = READ_AHEAD_INITIAL;
        actions.knobs.push(Knob::MorselTarget { stage: 0, bytes });
        actions.knobs.push(Knob::ActiveWorkers(1));
        actions.knobs.push(Knob::ReadAhead(READ_AHEAD_INITIAL));
        state.phase = Phase::Running;
        return actions;
    }

    let solution = solve(state);
    state.active_workers = solution.active_workers;
    state.read_ahead = solution.read_ahead;
    state.split_bytes = solution.split_bytes;
    state.high_water = solution.high_water;
    state.promotion_window = PROMOTION_WINDOW_INITIAL;

    // RC-I1: the check happens before the batch, not after it, and what it returns is what is
    // written. A set of targets that does not fit is never a set of knobs that was written.
    let targets = enforce(state, &solution.targets);
    fit_scalars(state, &targets);
    for (at, target) in targets.iter().enumerate() {
        state.stages[at].target = *target;
        state.stages[at].recent_targets.push_back(*target);
    }
    refresh_envelopes(state);

    let tier = host_tier(state);
    let queues = state.queue_count();
    for queue in 0..queues {
        let stage = u16::try_from(queue).unwrap_or(u16::MAX);
        actions.knobs.push(Knob::HighWater {
            stage,
            tier,
            bytes: state.high_water,
        });
        if solution.device_high_water > 0 {
            actions.knobs.push(Knob::HighWater {
                stage,
                tier: TierKind::Device,
                bytes: solution.device_high_water,
            });
        }
    }
    for queue in 0..queues {
        actions.knobs.push(Knob::PromotionWindow {
            stage: u16::try_from(queue).unwrap_or(u16::MAX),
            morsels: state.promotion_window,
        });
    }
    for at in 0..state.stages.len() {
        actions.knobs.push(Knob::MorselTarget {
            stage: state.stages[at].stage,
            bytes: state.stages[at].target,
        });
    }
    actions
        .knobs
        .push(Knob::ActiveWorkers(state.active_workers));
    actions.knobs.push(Knob::ReadAhead(state.read_ahead));

    // The placement half moves when the state term does, and `set_budgets` is the only
    // placement call the controller ever makes (f.1).
    let placement_host = scale(available(state), budget::PLACEMENT_SHARE);
    let budgets = state.budgets;
    actions.budgets = Some(budget::tier_budgets(state, &budgets, placement_host));
    state.tier_budgets = actions.budgets.clone().unwrap_or_default();
    state.phase = Phase::Running;
    apply::assert_consistent(state);
    actions
}

/// f.10: a dataset smaller than a quarter of the host budget needs no adaptation.
pub(crate) fn is_tiny(state: &ControllerState) -> bool {
    state.budgets.host > 0
        && state.cfg.plan.total_bytes > 0
        && state.cfg.plan.total_bytes < state.budgets.host / TINY_FRACTION
}

/// Shrink the queue high waters until the working set fits (RC-I1).
///
/// The queues are the first thing to give when the budget tightens: a smaller queue costs
/// throughput, and a morsel target that does not fit costs the process. This runs after the
/// targets have been enforced, so it only ever has to make up what the targets could not.
pub(crate) fn fit_scalars(state: &mut ControllerState, targets: &[u64]) {
    for _ in 0..64 {
        if fits(state, targets) || state.high_water == 0 {
            return;
        }
        state.high_water /= 2;
    }
}

/// Re-solve f.3 with the state term as it stands and write what changed: the morsel targets,
/// the worker count, the queue high waters and the placement half of the budget.
///
/// The read-ahead moves here only when the worker count did not, because RC-I8 forbids the two
/// in one tick; a read-ahead the solution wants lower is taken on the next tick instead.
pub(crate) fn resize(state: &mut ControllerState, actions: &mut Actions) {
    if state.stage_count() == 0 {
        return;
    }
    let solution = solve(state);
    // The scalars go in first, so that the RC-I1 check below sees the picture that will
    // actually be written rather than the one being replaced.
    let was_high_water = state.high_water;
    let workers_changed = solution.active_workers != state.active_workers;
    state.split_bytes = solution.split_bytes;
    state.high_water = solution.high_water;
    state.active_workers = solution.active_workers;
    if !workers_changed && solution.read_ahead != state.read_ahead {
        state.read_ahead = solution.read_ahead;
        actions.knobs.push(Knob::ReadAhead(state.read_ahead));
    }

    let targets = enforce(state, &solution.targets);
    fit_scalars(state, &targets);

    for (at, target) in targets.iter().enumerate() {
        if state.stages[at].target == *target {
            continue;
        }
        state.stages[at].target = *target;
        state.stages[at].completions_since_adjust = 0;
        actions.knobs.push(Knob::MorselTarget {
            stage: state.stages[at].stage,
            bytes: *target,
        });
    }
    if workers_changed {
        actions
            .knobs
            .push(Knob::ActiveWorkers(state.active_workers));
    }
    if state.high_water != was_high_water {
        let tier = host_tier(state);
        for queue in 0..state.queue_count() {
            actions.knobs.push(Knob::HighWater {
                stage: u16::try_from(queue).unwrap_or(u16::MAX),
                tier,
                bytes: state.high_water,
            });
        }
    }
    let placement_host = scale(available(state), budget::PLACEMENT_SHARE);
    let budgets = state.budgets;
    let tier_budgets = budget::tier_budgets(state, &budgets, placement_host);
    state.tier_budgets = tier_budgets.clone();
    actions.budgets = Some(tier_budgets);
    refresh_envelopes(state);
    apply::assert_consistent(state);
}
