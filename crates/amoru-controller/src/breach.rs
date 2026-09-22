//! Breach handling (RC-I4, f.7).
//!
//! This is the path G-I8 rests on. A breach is not waited on: the halving happens on the
//! worker thread that recorded it, before the next tick, because the whole point is to be
//! smaller than the budget by the time the next morsel is picked up. A breach with nothing left
//! to give produces the diagnostic the runtime terminates itself with, which is the difference
//! between a run that ends with a sentence naming the morsel and a run that ends with SIGKILL.
//!
//! Two things here are what they are because of the overshoot measured on 2026-09-23, where a
//! Python kernel at a 512 MiB budget finished at 1.11 x the ceiling. Neither was a controller
//! that failed to look: it breached on every one of the ten records and shed a worker each time.
//!
//!   * The worker count is shed to what the anon inequality of f.3 allows in one step, not by
//!     one. One per breach is a reaction slower than a short run: that run went from ten workers
//!     to two over ten morsels and then ran out of morsels, so it neither fitted nor terminated.
//!   * Termination is decided by the model and not by a counter. `floor_breaches >= 3 and
//!     active_workers <= 1` needs `workers_max + 3` breaching records before it can fire, and a
//!     run with fewer morsels than that completes over budget instead. When the floor on one
//!     worker does not satisfy the anon inequality, there is nothing left to try and the run
//!     ends there (S6, G-I8). The counter stays as the backstop for the other case: a model that
//!     says the set fits and a process that says otherwise.

use amoru_kernel::{AmoruError, Knob, TraceRecord};

use crate::{Actions, BREACH_SHARE, ControllerState, SAFETY_CAP, features_of, model};

/// How much a breach raises the stage's safety multiplier (RC-I4).
const SAFETY_BUMP: f32 = 0.2;
/// Breaches at the floor before the worker count is lowered (f.7).
const BREACHES_BEFORE_WORKERS: u32 = 2;
/// Breaches at the floor with one worker left before the run is terminated (f.7).
const BREACHES_BEFORE_TERMINATE: u32 = 3;

/// The out-of-arena amplification this record implies, which the breach path fits before it
/// decides anything: the tick thread's own fit (10 f.3) has not seen this record yet, and the
/// whole point of f.7 is to be smaller by the time the next morsel is picked up.
fn refit_anon(state: &mut ControllerState, at: usize, r: &TraceRecord) {
    let target = state.stages[at].target;
    let in_flight = model::share(state, target).max(1.0);
    let resting = model::resting_anon(state);
    // The same two readings and the same unit as the tick thread's fit (10 f.3): the bytes the
    // model believed were in flight, which is what the inequality multiplies back.
    let in_flight_bytes = (in_flight * target.max(r.bytes_in) as f64).max(1.0);
    let from_peak = r.mem_anon_peak.saturating_sub(resting) as f64 / in_flight_bytes;
    let from_delta = r.mem_anon_peak.saturating_sub(r.mem_anon_before) as f64 / in_flight_bytes;
    state.stages[at].observe_anon(from_peak.max(from_delta));
}

/// The workers the anon inequality of f.3 allows at the targets as they now stand, floor 1.
fn anon_workers(state: &ControllerState) -> u16 {
    let max_anon_allowance = (0..state.stages.len())
        .map(|at| model::anon_allowance(state, at, state.stages[at].target))
        .max()
        .unwrap_or(0);
    match model::anon_for_kernels(state).checked_div(max_anon_allowance) {
        Some(allowed) => u16::try_from(allowed)
            .unwrap_or(u16::MAX)
            .clamp(1, state.cfg.workers_max.max(1)),
        None => state.cfg.workers_max.max(1),
    }
}

/// The out-of-arena bytes one morsel of `morsel_min` per stage on one worker would cost: the
/// smallest set of knobs the controller has to offer. It is also the footprint the diagnostic
/// names, against the headroom it has to fit in, so the two figures the message compares are
/// the same kind of number -- which a per-morsel delta against an absolute line was not.
fn floor_footprint(state: &ControllerState) -> u64 {
    let mut footprint = state.state_total;
    for at in 0..state.stages.len() {
        footprint =
            footprint.saturating_add(model::anon_allowance(state, at, state.cfg.morsel_min));
    }
    footprint
}

/// f.7. Called from `on_record` on the recording worker.
pub(crate) fn check(state: &mut ControllerState, r: &TraceRecord, actions: &mut Actions) {
    let Some(at) = state.stages.iter().position(|s| s.stage == r.stage) else {
        return;
    };
    let host_breach = r.mem_anon_peak > state.breach_line();
    let device_budget = state.budgets.device[0];
    let device_breach = device_budget > 0 && r.dev_mem_peak > device_budget;
    if !host_breach && !device_breach {
        return;
    }
    if host_breach {
        refit_anon(state, at, r);
    }

    state.breaches = state.breaches.saturating_add(1);
    let morsel_min = state.cfg.morsel_min;
    let at_floor = state.stages[at].target <= morsel_min;
    {
        let ctl = &mut state.stages[at];
        ctl.safety = (ctl.safety + SAFETY_BUMP).min(SAFETY_CAP);
        let halved = (ctl.target / 2).max(morsel_min);
        ctl.target = halved;
        ctl.completions_since_adjust = 0;
        ctl.last_adjust_sign = -1;
        // The breach path is deliberately outside the freeze and outside the damping: RC-I3
        // makes the exception explicit, because a frozen stage that is breaching is exactly
        // the stage that must move.
        ctl.frozen_until = None;
        if at_floor {
            ctl.floor_breaches = ctl.floor_breaches.saturating_add(1);
        } else {
            ctl.floor_breaches = 0;
        }
    }
    let bytes = state.stages[at].target;
    let stage = state.stages[at].stage;
    tracing::warn!(
        target: "ctl.breach",
        stage,
        seq = r.seq,
        peak = r.mem_anon_peak,
        line = state.breach_line(),
        bytes,
        "breach"
    );
    actions.knobs.push(Knob::MorselTarget { stage, bytes });
    crate::model::refresh_envelopes(state);

    // The worker count goes straight to what the anon inequality allows at the new target. A
    // host breach is the model being wrong about what a kernel costs the process, and the
    // refit above has just corrected it; shedding one worker would leave the rest of the
    // correction for records that may not exist.
    if host_breach {
        let allowed = anon_workers(state);
        if allowed < state.active_workers {
            state.active_workers = allowed;
            actions
                .knobs
                .push(Knob::ActiveWorkers(state.active_workers));
        }
    }

    if !at_floor {
        return;
    }
    let floor_breaches = state.stages[at].floor_breaches;
    // Nothing left to give: the smallest morsel on one worker does not fit what the process has
    // above the arena, so the run is diagnosed here rather than finished over budget or killed
    // by the host (S6, G-I8). The model decides it, so a run with three morsels ends on the same
    // evidence as a run with three million.
    if host_breach
        && state.active_workers <= 1
        && floor_footprint(state) > model::anon_headroom(state)
    {
        state.terminated = true;
        actions.terminate = Some(AmoruError::Budget {
            seq: r.seq,
            stage,
            footprint: floor_footprint(state),
            budget: model::anon_headroom(state),
            features: features_of(r),
        });
        return;
    }
    if floor_breaches >= BREACHES_BEFORE_TERMINATE && state.active_workers <= 1 {
        // The backstop: the model says this set fits and the process keeps saying otherwise.
        // The figures are the measured footprint above the resting anonymous memory against the
        // line it crossed, so the two are comparable and the first is the larger by definition.
        let (footprint, budget) = if host_breach {
            (
                r.mem_anon_peak.saturating_sub(model::resting_anon(state)),
                model::scale(model::anon_headroom(state), BREACH_SHARE),
            )
        } else {
            (r.dev_mem_peak, device_budget)
        };
        state.terminated = true;
        actions.terminate = Some(AmoruError::Budget {
            seq: r.seq,
            stage,
            footprint,
            budget,
            features: features_of(r),
        });
        return;
    }
    if floor_breaches >= BREACHES_BEFORE_WORKERS && state.active_workers > 1 {
        state.active_workers -= 1;
        actions
            .knobs
            .push(Knob::ActiveWorkers(state.active_workers));
    }
}
