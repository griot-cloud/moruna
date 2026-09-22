//! Breach handling (RC-I4, f.7).
//!
//! This is the path G-I8 rests on. A breach is not waited on: the halving happens on the
//! worker thread that recorded it, before the next tick, because the whole point is to be
//! smaller than the budget by the time the next morsel is picked up. Three breaches with
//! nothing left to give produce the diagnostic the runtime terminates itself with, which is
//! the difference between a run that ends with a sentence naming the morsel and a run that
//! ends with SIGKILL.

use amoru_kernel::{AmoruError, Knob, TraceRecord};

use crate::{Actions, ControllerState, SAFETY_CAP, features_of};

/// How much a breach raises the stage's safety multiplier (RC-I4).
const SAFETY_BUMP: f32 = 0.2;
/// Breaches at the floor before the worker count is lowered (f.7).
const BREACHES_BEFORE_WORKERS: u32 = 2;
/// Breaches at the floor with one worker left before the run is terminated (f.7).
const BREACHES_BEFORE_TERMINATE: u32 = 3;

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

    if !at_floor {
        return;
    }
    let floor_breaches = state.stages[at].floor_breaches;
    if floor_breaches >= BREACHES_BEFORE_TERMINATE && state.active_workers <= 1 {
        // Nothing left to give: the smallest morsel on one worker still does not fit, so the
        // run is diagnosed here rather than killed by the host (G-I8).
        let budget = if host_breach {
            state.breach_line()
        } else {
            device_budget
        };
        let footprint = if host_breach {
            r.mem_anon_peak.saturating_sub(r.mem_anon_before)
        } else {
            r.dev_mem_peak
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
