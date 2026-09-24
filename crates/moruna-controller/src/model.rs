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
//!
//! That division is an accounting of the *arena*, and the arena is not the quantity S1 measures.
//! 02 f.1 touches every page of the region at `new`, so the process holds `baseline +
//! arena_bytes` of anonymous memory from the moment the arena exists, and every byte a kernel
//! allocates for itself inside `apply` is added on top of that, not carved out of it. A model
//! that charges those bytes against the arena's capacity permits a process anonymous peak of
//! `baseline + arena_bytes + (arena_bytes - state) / 2`, which is over the ceiling by
//! construction: that is the defect measured on 2026-09-23 (a Python kernel at a 512 MiB budget
//! reached 1.11 x the ceiling while the controller shed a worker on every record).
//!
//! So there are two inequalities and a knob set has to satisfy both:
//!
//! ```text
//! arena:  S share x target x a_k x safety + S high_water + read_ahead x split_bytes <= arena_bytes
//! anon:   S (c_anon + share x target x a_anon x safety) <= for_kernels
//!         where for_kernels = KERNEL_ANON_SHARE x (ceiling - baseline - arena_bytes - state)
//! ```
//!
//! The share in the second is the runtime's own out-of-arena bytes, which are not a morsel and not
//! in the fit: a Parquet encoder, the Arrow builders a batch crosses through, the interpreter's own
//! growth. Planning morsels into the whole headroom spends the reserve twice and what the runtime
//! then takes comes out of the ceiling, which is how runs satisfying every inequality here still
//! sat at 0.98 of it and two Linux runs passed it (2026-09-23).
//!
//! The second is fitted to `TraceRecord::mem_anon_peak` (10 f.3), which is the figure S1 is
//! measured from, so a kernel whose Python objects cost three times its input is sized for what
//! it actually costs the process. Where the floor of `morsel.min_bytes` on one worker still does
//! not satisfy it, f.7 ends the run with the diagnostic rather than let it finish over budget.

use moruna_kernel::{Knob, TierKind};

use crate::{
    Actions, ControllerState, Envelope, Inner, MIN_AMPLIFICATION, Phase, Result, apply, budget,
    tick,
};

/// The share of the device budget one stage's in-flight morsels may hold (f.3).
const DEVICE_WORKER_SHARE: f64 = 0.4;
/// The share of the headroom above the arena the kernels may be planned into, the rest being what
/// the runtime itself allocates outside the arena and never declared (f.3).
const KERNEL_ANON_SHARE: f64 = 0.6;
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

/// The process's resting anonymous memory: what it held before the arena, plus the arena, every
/// page of which 02 f.1 touches at `new` (f.3).
///
/// This is the floor of the quantity S1 measures and the controller cannot move it: the arena's
/// size is the facade's decision and its pages are resident whether the runtime is using them
/// or not. What the controller can move is everything above it.
pub(crate) fn resting_anon(state: &ControllerState) -> u64 {
    state.budgets.baseline.saturating_add(state.budgets.host)
}

/// The anonymous bytes the run may add above the resting figure before the ceiling S1 measures
/// against is reached (f.3).
///
/// With the arena sized at `ceiling - baseline - reserve - declared kernel state`, this is the
/// reserve plus that declared state: the allowance architecture section 8 names for what a
/// kernel allocates outside the arena, now a term in the model rather than a hope about it.
pub(crate) fn anon_headroom(state: &ControllerState) -> u64 {
    state
        .cfg
        .limits
        .memory_ceiling
        .saturating_sub(resting_anon(state))
}

/// The fixed term of f.3's fit summed over the stages: anonymous bytes the chain holds above the
/// resting figure and above its funded state whatever the morsel is.
///
/// It is charged once, like `state_total` and for the same reason: it does not scale with the
/// morsel, so dividing it by the bytes in flight and multiplying it back is not an accounting of
/// it but a way of making it move when the morsel does. It is also measured that way (f.3 reads it
/// from `mem_anon_before`, which is a figure for the whole process), so charging it per morsel in
/// flight would count the same bytes as many times as there are workers.
pub(crate) fn anon_fixed(state: &ControllerState) -> u64 {
    state
        .stages
        .iter()
        .fold(0u64, |total, ctl| total.saturating_add(ctl.c_anon))
}

/// The part of the headroom the kernels may have: what is left once the instances' state is
/// funded, less the share the runtime keeps for its own bytes outside the arena (f.3).
///
/// Not everything above the arena belongs to the kernels. A Parquet writer's encoder, the Arrow
/// builders a batch is converted through, the interpreter's own growth: none of it is a morsel and
/// none of it is in the fit, and all of it is in the figure S1 measures. Handing the kernels the
/// whole headroom spends the reserve twice, once as the margin the arena was sized to leave and
/// again as the allowance the model divides among morsels, and what the runtime then takes comes
/// out of the ceiling. Measured on the Parquet to Python to Parquet job at a 512 MiB ceiling, that
/// unmodelled remainder is about a fifth of the headroom; the kernels get `KERNEL_ANON_SHARE` of
/// it and the rest is nobody's to plan with (2026-09-23).
pub(crate) fn anon_for_kernels(state: &ControllerState) -> u64 {
    for_kernels(state).saturating_sub(anon_fixed(state))
}

/// The headroom the kernels may be planned into at all, fixed term included: `KERNEL_ANON_SHARE`
/// of what the arena left above itself, less the bytes the instances hold.
pub(crate) fn for_kernels(state: &ControllerState) -> u64 {
    scale(
        anon_headroom(state).saturating_sub(state.state_total),
        KERNEL_ANON_SHARE,
    )
}

/// What one in-flight morsel of a stage costs the process outside the arena (f.3): the anon
/// analogue of `allowance`.
pub(crate) fn anon_allowance(state: &ControllerState, at: usize, target: u64) -> u64 {
    let ctl = &state.stages[at];
    scale(
        target,
        ctl.a_anon.max(MIN_AMPLIFICATION) * f64::from(ctl.safety),
    )
}

/// The anonymous footprint a proposed set of targets would reach, above the resting figure: the
/// funded state, each stage's fixed term, and each stage's morsels (f.3).
pub(crate) fn anon_footprint(state: &ControllerState, targets: &[u64]) -> u64 {
    let mut total = state.state_total;
    for (at, target) in targets.iter().enumerate() {
        let allowance = anon_allowance(state, at, *target);
        total = total
            .saturating_add(state.stages[at].c_anon)
            .saturating_add(scale(allowance, share(state, *target)));
    }
    total
}

/// The anon half of RC-I1: whether a proposed set of targets keeps the kernels inside the
/// allowance the headroom leaves them. This is the inequality S1 is measured against.
///
/// It answers to `anon_for_kernels` rather than to the ceiling directly, so the share the runtime
/// keeps for its own out-of-arena bytes is not a plan the model may spend. That share is also the
/// cushion f.7's breach line assumes: reacting to a breach takes a record, a record arrives after
/// the `apply` that produced it, and whatever that morsel cost above its prediction is already in
/// the process by then. A plan allowed to fill the headroom exactly leaves that surprise nowhere
/// to land, which is how runs satisfying every inequality here still reached 0.99 of the ceiling
/// and on one host passed it (2026-09-23).
pub(crate) fn fits_anon(state: &ControllerState, targets: &[u64]) -> bool {
    anon_footprint(state, targets) <= state.state_total.saturating_add(for_kernels(state))
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
///
/// Both inequalities of the module header bound it. The arena's bound was the only one here
/// until 2026-09-23, and a sizer proposing up to it could be written a target the anon
/// inequality had already refused -- RC-I2 held against an envelope that was not the whole
/// envelope.
pub(crate) fn envelope(state: &ControllerState, at: usize) -> Envelope {
    let stages = state.stage_count().max(1) as u64;
    let budget_for_stage = worker_half(state) / stages;
    let anon_for_stage = anon_for_kernels(state) / stages;
    let ctl = &state.stages[at];
    let in_flight = share(state, ctl.target);
    let per_byte = in_flight * ctl.a_k * f64::from(ctl.safety);
    let from_budget = if per_byte <= 0.0 {
        state.cfg.morsel_max
    } else {
        scale(budget_for_stage, 1.0 / per_byte)
    };
    // `anon_for_kernels` has already funded every stage's fixed term (f.3), so what is divided
    // here is what the morsels may have and the divisor is the slope alone.
    let per_anon_byte = in_flight * ctl.a_anon.max(MIN_AMPLIFICATION) * f64::from(ctl.safety);
    let from_anon = if per_anon_byte <= 0.0 {
        state.cfg.morsel_max
    } else {
        scale(anon_for_stage, 1.0 / per_anon_byte)
    };
    Envelope {
        min: state.cfg.morsel_min,
        max: from_budget
            .min(from_anon)
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

/// The anon half of RC-I1 read without the fixed term: what the targets cost if every byte the
/// probe saw scales with the morsel.
///
/// One probe cannot tell a kernel that holds a fixed 160 MB from one that holds ten bytes per
/// byte, so f.3 funds the fixed term and plans small until a record separates them. That is the
/// right way to be wrong about a morsel target. It is the wrong way to be wrong about whether a
/// run may start at all: the fixed reading of a steep probe exceeds many a ceiling on its own,
/// and refusing on it would refuse runs whose second record would have shown the cost scaling.
/// So the refusal at `start` answers to this reading, and the plan answers to the other one.
///
/// It answers to the ceiling, too, and not to `for_kernels`: the share withheld there is a margin
/// the runtime keeps for its own bytes, and a plan that overruns a margin is a plan to tighten,
/// not a run to refuse.
pub(crate) fn fits_anon_optimistically(state: &ControllerState, targets: &[u64]) -> bool {
    let scaling = anon_footprint(state, targets).saturating_sub(anon_fixed(state));
    resting_anon(state).saturating_add(scaling) <= state.cfg.limits.memory_ceiling
}

/// RC-I1: whether a proposed set of targets fits the budget -- both of the budgets in the
/// module header, because a set that fits the arena and not the ceiling is the set that broke
/// S1 and a set that fits the ceiling and not the arena cannot be allocated.
pub(crate) fn fits(state: &ControllerState, targets: &[u64]) -> bool {
    fits_arena(state, targets) && fits_anon(state, targets)
}

/// The arena half of RC-I1: whether the runtime's own bytes fit the region it allocates from.
/// This is the half the queue high waters are in, so it is the half `fit_scalars` answers to.
pub(crate) fn fits_arena(state: &ControllerState, targets: &[u64]) -> bool {
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
        //
        // The anon inequality is not an adaptation, though, and this path does not escape it: a
        // small dataset does not make a greedy kernel cheap, and the ceiling S1 measures against
        // is the process's either way. Capping the target here rather than leaving it to the
        // worker count is what keeps the workers: a 512 MiB morsel guessed at four times its
        // input funds one worker, and the same headroom funds all of them at 34 MiB.
        state.cfg.morsel_max.min(anon_target(state))
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
    let max_anon_allowance = (0..state.stages.len())
        .map(|at| anon_allowance(state, at, targets[at]))
        .max()
        .unwrap_or(0);
    // RC-I7: no more workers than either budget can feed one morsel each. The arena cap is
    // skipped on the tiny path (f.10), which has nothing to adapt to; the anon cap never is,
    // because a small dataset does not make a greedy kernel cheap and the process's peak is
    // what S1 measures either way.
    let workers_max = state.cfg.workers_max.max(1);
    let mut allowed = workers_max;
    if !state.tiny
        && let Some(arena_cap) = worker_half.checked_div(max_allowance)
    {
        allowed = allowed.min(u16::try_from(arena_cap).unwrap_or(u16::MAX));
    }
    if let Some(anon_cap) = anon_for_kernels(state).checked_div(max_anon_allowance) {
        allowed = allowed.min(u16::try_from(anon_cap).unwrap_or(u16::MAX));
    }
    let active_workers = allowed.clamp(1, workers_max);

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
    // The anon inequality of the module header. The arena's capacity bounds what the runtime
    // may hold; the ceiling bounds what the process may hold, and the second is the one S1 is
    // measured against, so whichever binds first is the target.
    target = target.min(anon_target(state));
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

/// The largest common target the anon inequality of the module header allows at the worker count
/// as it stands: what is left of `anon_for_kernels` once the fixed term of every morsel in flight
/// is funded, over `share x S a_anon x safety`.
///
/// This is the bound S1 is measured against, expressed as a morsel size. It is applied on every
/// path that writes a target, the tiny-dataset path of f.10 included.
fn anon_target(state: &ControllerState) -> u64 {
    let stages = state.stage_count();
    if stages == 0 {
        return state.cfg.morsel_max;
    }
    let share_each = f64::from(state.active_workers) / stages as f64;
    let sum_anon: f64 = state
        .stages
        .iter()
        .map(|ctl| ctl.a_anon.max(MIN_AMPLIFICATION) * f64::from(ctl.safety))
        .sum();
    if share_each <= 0.0 || sum_anon <= 0.0 {
        return state.cfg.morsel_max;
    }
    scale(anon_for_kernels(state), 1.0 / (share_each * sum_anon))
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
            return Err(moruna_kernel::MorunaError::Config {
                name: "controller",
                msg: format!(
                    "start called in phase {:?}: probe_all or probe_missing must run first \
                     (RC-I6)",
                    state.phase
                ),
            });
        }
        state.active_workers = state.cfg.workers_max.max(1);
        initial_knobs(&mut state)?
    };
    ctl.perform(actions);
    // e.2: the records that arrived before `Running` (the probes' own) are processed here, so
    // the first tick sees a controller that already knows what the probes measured.
    tick::absorb_queue(ctl);
    Ok(())
}

/// The knob set f.3 writes at `start`, in the order f.3 fixes: high waters, promotion windows,
/// morsel targets, active workers, read-ahead, with the RC-I1 check before the batch.
pub(crate) fn initial_knobs(state: &mut ControllerState) -> Result<Actions> {
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
        return Ok(actions);
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
    // And a floor that still does not fit is not a run to start. `enforce` halves until the
    // targets fit or reach `morsel_min`, and the floor reaching it without fitting was left to
    // f.7: the run began, one morsel ran, and the breach path reported the footprint it had
    // already reached. That report is true and it is too late, because the morsel's own growth
    // is what passed the ceiling. Measured: a Python kernel at a 512 MiB ceiling on a host whose
    // interpreter rests at 406 MB was refused at its third morsel with the peak at 1.005 of the
    // ceiling, so S1 was broken by the diagnostic rather than saved by it (2026-09-23). The
    // arithmetic is known here, before a morsel exists.
    //
    // The reading it refuses on is the optimistic one: the slope alone, without the fixed term
    // the probe's single measurement cannot distinguish from it. Planning stays conservative
    // (the fixed term is funded above, which is what keeps the first morsel small), but refusing
    // a run on a term that one observation only *might* support would turn every kernel with a
    // steep probe into a budget error, and the evidence for that is not in yet. What is not
    // arguable is the slope: it is the probe's own ratio, and a floor morsel that does not fit
    // even at that has nowhere to run.
    // The anon half only. A working set that still does not fit the arena is answered by the
    // queues (`fit_scalars` above), by the read-ahead and in the end by backpressure: the arena
    // hands out what it has and a morsel waits for a token. The ceiling has no such answer, and
    // it is the one S1 is measured against.
    if !fits_anon_optimistically(state, &targets) {
        return Err(cannot_be_sized(state, &targets));
    }
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
    Ok(actions)
}

/// The refusal of f.3 when the floor does not fit: a budget this job cannot be run inside, said
/// before anything runs rather than after a morsel has proved it.
///
/// Every figure it names is one the caller can act on: what the process was already holding, what
/// the arena took, what the smallest morsel the runtime can form is predicted to cost outside it,
/// and the ceiling all of that has to sit under. `Config` rather than `Budget`, because nothing
/// ran: `Budget` is the diagnostic for a morsel that was admitted and cost more than it was
/// planned for, and there is no morsel here.
fn cannot_be_sized(state: &ControllerState, targets: &[u64]) -> moruna_kernel::MorunaError {
    let ceiling = state.cfg.limits.memory_ceiling;
    let resting = resting_anon(state);
    let predicted = anon_footprint(state, targets).saturating_sub(anon_fixed(state));
    let msg = format!(
        "this budget cannot hold this job on this host: the ceiling is {ceiling} bytes, the \
             process held {} bytes before the arena existed, the arena took {} more, and the \
             smallest morsel the runtime can form is measured to cost {predicted} bytes outside \
             it, which is {} bytes past the ceiling. The figure the process held is what it has \
             resident, which counts pages an allocator holds for its own reuse as well as pages in \
             use. Otherwise give the run a budget of about {} bytes, or a kernel that holds less \
             of each morsel",
        state.budgets.baseline,
        state.budgets.host,
        resting.saturating_add(predicted).saturating_sub(ceiling),
        resting.saturating_add(predicted),
    );
    moruna_kernel::MorunaError::Config {
        name: "budget",
        msg,
    }
}

/// f.10: a dataset smaller than a quarter of the host budget needs no adaptation.
pub(crate) fn is_tiny(state: &ControllerState) -> bool {
    state.budgets.host > 0
        && state.cfg.plan.total_bytes > 0
        && state.cfg.plan.total_bytes < state.budgets.host / TINY_FRACTION
}

/// Shrink the queue high waters until the arena's half of the working set fits (RC-I1).
///
/// The queues are the first thing to give when the arena tightens: a smaller queue costs
/// throughput, and a morsel target that does not fit costs the process. This runs after the
/// targets have been enforced, so it only ever has to make up what the targets could not.
///
/// It answers to `fits_arena` and not to `fits`, because the queues live in the arena and the
/// arena's pages are resident either way: emptying them gives the process's anonymous peak back
/// nothing at all. An anon inequality that does not hold is answered by the morsel targets, the
/// worker count, and failing those, f.7.
pub(crate) fn fit_scalars(state: &mut ControllerState, targets: &[u64]) {
    for _ in 0..64 {
        if fits_arena(state, targets) || state.high_water == 0 {
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
