//! Following the machine (MH 4.4).
//!
//! The controller's ceiling used to be a constant of the run, copied from `cfg.limits` at start.
//! It is now read every tick from the sample, which carries the limits the discovery watcher last
//! published (`Sample::ceiling_bytes`, `Sample::cpu_limit`), and from the facade when it resizes
//! the arena. Nothing here is a new decision rule: a moved limit re-runs f.3's solve inside the
//! new limits, a lowered ceiling also takes the memory row of f.6 at once rather than waiting for
//! the anonymous memory to reach its trip line, and the CPU limit bounds the worker count the
//! existing rules may choose.

use moruna_kernel::{Knob, Sample};

use crate::{Actions, ControllerState, Phase, classify, model};

/// Nanoseconds since the Unix epoch, the trace records' clock; zero before it.
pub(crate) fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Whether a record was measured under limits that have since moved.
pub(crate) fn stale(state: &ControllerState, t_start_ns: u64) -> bool {
    t_start_ns < state.limits_epoch_ns
}

/// A CPU quota as a worker count: rounded up, at least one, at most the pool (the same rule as
/// the facade's `workers.max`, preamble section 5).
pub(crate) fn workers_for(cpu_limit: f64, workers_max: u16) -> u16 {
    let ceil = cpu_limit.ceil();
    let workers = if !ceil.is_finite() || ceil < 1.0 {
        1
    } else {
        ceil.min(f64::from(u16::MAX)) as u16
    };
    workers.clamp(1, workers_max.max(1))
}

/// The most workers any rule may make active now: the pool, bounded by the current CPU limit
///. Until a sample has carried a limit, the pool.
pub(crate) fn workers_bound(state: &ControllerState) -> u16 {
    let pool = state.cfg.workers_max.max(1);
    match state.cpu_bound {
        Some(bound) => bound.clamp(1, pool),
        None => pool,
    }
}

/// Take the limits a sample carries. Zero in either field means the sampler knows none
/// (a fake), and the controller keeps what it has.
pub(crate) fn follow_sample(state: &mut ControllerState, sample: &Sample, actions: &mut Actions) {
    follow(state, sample.ceiling_bytes, sample.cpu_limit, actions);
}

/// Take a ceiling and a CPU limit: note the change, and when the run is going, re-plan
/// inside the new limits.
pub(crate) fn follow(
    state: &mut ControllerState,
    ceiling_bytes: u64,
    cpu_limit: f64,
    actions: &mut Actions,
) {
    let mut moved = false;
    let mut lowered_ceiling = false;

    if cpu_limit > 0.0 {
        let bound = workers_for(cpu_limit, state.cfg.workers_max);
        if state.cpu_bound != Some(bound) {
            if let Some(was) = state.cpu_bound {
                state.note(format!(
                    "the CPU limit moved from {was} to {bound} workers; the worker count follows it"
                ));
                moved = true;
            }
            state.cpu_bound = Some(bound);
            state.cfg.limits.cpu_quota = cpu_limit;
            if state.phase == Phase::Running && state.active_workers > bound {
                // Parked down to the limit within the tick that saw it, not discovered one
                // worker at a time through throttling.
                state.active_workers = bound;
                actions.knobs.push(Knob::ActiveWorkers(bound));
            }
        }
    }

    if ceiling_bytes > 0 && ceiling_bytes != state.cfg.limits.memory_ceiling {
        let was = state.cfg.limits.memory_ceiling;
        state.cfg.limits.memory_ceiling = ceiling_bytes;
        lowered_ceiling = ceiling_bytes < was;
        state.note(format!(
            "the memory ceiling moved from {was} to {ceiling_bytes} bytes; the plan follows it"
        ));
        state.limits_changes = state.limits_changes.saturating_add(1);
        state.limits_epoch_ns = now_ns();
        moved = true;
    }

    if moved && state.phase == Phase::Running {
        replan(state, lowered_ceiling, actions);
    }
}

/// The arena's capacity moved: the facade grew or shrank it and tells the controller the
/// new host budget, which is the controller's whole arena allowance (f.1).
pub(crate) fn set_host_budget(state: &mut ControllerState, bytes: u64, actions: &mut Actions) {
    if bytes == state.budgets.host {
        return;
    }
    let lowered = bytes < state.budgets.host;
    state.note(format!(
        "the arena moved from {} to {bytes} bytes",
        state.budgets.host
    ));
    state.budgets.host = bytes;
    state.cfg.arena_bytes = bytes;
    state.limits_epoch_ns = now_ns();
    if state.phase == Phase::Running {
        replan(state, lowered, actions);
    }
}

/// An engine source's operator memory moved: it is resident beside the arena (`resting_anon`),
/// so what the kernels may be planned into moves the other way.
pub(crate) fn set_engine(state: &mut ControllerState, bytes: u64, actions: &mut Actions) {
    if bytes == state.cfg.engine_bytes {
        return;
    }
    let lowered_room = bytes > state.cfg.engine_bytes;
    state.note(format!(
        "the plan's operator memory moved from {} to {bytes} bytes",
        state.cfg.engine_bytes
    ));
    state.cfg.engine_bytes = bytes;
    state.limits_epoch_ns = now_ns();
    if state.phase == Phase::Running {
        replan(state, lowered_room, actions);
    }
}

/// Re-run f.3's solve inside the limits as they now stand, with the worker count the limit
/// allows as its `W`, and, when the budget came down, take f.6's memory row at once: the
/// largest target halves and the last queue stages, so the run stops filling what it is about to
/// lose instead of waiting for its anonymous memory to reach the trip line.
fn replan(state: &mut ControllerState, lowered: bool, actions: &mut Actions) {
    if state.stage_count() == 0 {
        return;
    }
    let bound = workers_bound(state);
    let was_active = state.active_workers;
    if !lowered {
        // A raised limit is room to use: solve with every worker the limit allows, and let the
        // memory inequalities of RC-I7 bring it down if they must.
        state.active_workers = bound;
    } else {
        state.active_workers = state.active_workers.min(bound);
    }
    model::resize(state, actions);
    if state.active_workers != was_active
        && !actions
            .knobs
            .iter()
            .any(|knob| matches!(knob, Knob::ActiveWorkers(_)))
    {
        actions
            .knobs
            .push(Knob::ActiveWorkers(state.active_workers));
    }
    if lowered {
        let last_stage = u16::try_from(state.stage_count()).unwrap_or(u16::MAX);
        classify::memory_pressure(state, last_stage, actions);
    }
}
