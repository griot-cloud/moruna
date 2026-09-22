//! Applying a proposal (f.5), the working-set check (RC-I1) and the sizer fallback (f.8).
//!
//! This is where D7 is enforced. A sizer proposes; the envelope clamps; the working-set check
//! decides; only then is a knob written. The clamp is counted, and a sizer that is clamped
//! more often than it is believed, or whose predictions are worse than the rule sizer's by the
//! configured ratio, is replaced by the rule sizer for the rest of the run.

use amoru_kernel::{Knob, MorselFeatures, TraceRecord};

use crate::{
    Actions, ControllerState, FALLBACK_MIN_PROPOSALS, FLIP_WINDOW, FREEZE_MEAN_OVER, Observation,
    Proposal, RuleSizer, model, profile,
};

/// The coefficient of variation under which f.4 tightens the safety margin.
const STEADY_CV: f64 = 0.15;
/// How much the margin tightens per accepted increase (f.4).
const SAFETY_STEP: f32 = 0.02;
/// Records the steadiness test of f.4 looks back over.
const STEADY_WINDOW: usize = 20;

/// RC-I1 as a statement rather than a hope: in a debug build, the set the controller is about
/// to write is checked against the working-set inequality one more time. In a release build it
/// costs nothing; in a test it is what turns a mistake in one of the paths that writes a target
/// into a failure at the write rather than into a breach much later.
///
/// The one set that may fail either inequality is the smallest set there is: every target at the
/// floor, and for the arena's half the queues at zero as well. That is not a controller that has
/// written something inconsistent, it is a run whose kernels no longer leave room for one morsel
/// per worker, and f.6's StateGrowth row and f.7's termination are the paths that end it.
pub(crate) fn assert_consistent(state: &ControllerState) {
    debug_assert!(
        {
            let targets: Vec<u64> = state.stages.iter().map(|s| s.target).collect();
            let at_floor = targets.iter().all(|target| *target <= state.cfg.morsel_min);
            let arena = model::fits_arena(state, &targets) || (at_floor && state.high_water == 0);
            let anon = model::fits_anon(state, &targets) || at_floor;
            targets.is_empty() || (arena && anon)
        },
        "RC-I1: a knob set that does not satisfy the working-set inequalities was about to be \
         written"
    );
}

/// RC-I1, in one place. Reduce the proposed targets until they fit, take the queues down after
/// them if the targets alone were not enough, and write exactly what the check left.
pub(crate) fn commit_targets(state: &mut ControllerState, proposed: &[u64], actions: &mut Actions) {
    let was_high_water = state.high_water;
    let targets = model::enforce(state, proposed);
    model::fit_scalars(state, &targets);
    for (at, target) in targets.iter().enumerate() {
        if state.stages[at].target == *target {
            continue;
        }
        state.stages[at].target = *target;
        state.stages[at].recent_targets.push_back(*target);
        while state.stages[at].recent_targets.len() > FREEZE_MEAN_OVER {
            state.stages[at].recent_targets.pop_front();
        }
        actions.knobs.push(Knob::MorselTarget {
            stage: state.stages[at].stage,
            bytes: *target,
        });
    }
    if state.high_water != was_high_water {
        let tier = model::host_tier(state);
        for queue in 0..state.queue_count() {
            actions.knobs.push(Knob::HighWater {
                stage: u16::try_from(queue).unwrap_or(u16::MAX),
                tier,
                bytes: state.high_water,
            });
        }
    }
    model::refresh_envelopes(state);
    assert_consistent(state);
}

/// RC-I1 again, as a standing obligation rather than a check at a write: the state term grows
/// under the controller between ticks, so a set that fitted when it was written can stop
/// fitting without anything being written at all. Every tick puts that right before it decides
/// anything else.
pub(crate) fn repair(state: &mut ControllerState, actions: &mut Actions) {
    let targets: Vec<u64> = state.stages.iter().map(|s| s.target).collect();
    if targets.is_empty() || model::fits(state, &targets) {
        return;
    }
    commit_targets(state, &targets, actions);
}

/// The observation one stage's sizer is given (d.1). A sizer sees this and nothing else.
pub(crate) fn observation(
    state: &ControllerState,
    at: usize,
    recent: Vec<TraceRecord>,
) -> Observation {
    let ctl = &state.stages[at];
    let features = ctl
        .last
        .as_ref()
        .map(|last| MorselFeatures {
            rows: last.rows_in,
            bytes: last.bytes_in,
            column_bytes: last.column_bytes.clone(),
            mean_string_len: Some(last.mean_string_len),
            null_ratio: Some(last.null_ratio),
            shape: None,
            dtype: None,
        })
        .unwrap_or_default();
    Observation {
        stage: ctl.stage,
        features,
        active_workers: state.active_workers,
        a_k: ctl.a_k,
        safety: ctl.safety,
        target: ctl.target,
        completions_since_adjust: ctl.completions_since_adjust,
        damping: model::damping(state),
        recent,
    }
}

/// f.5. Clamp the proposal to the envelope, count the clamp, respect the freeze, detect
/// oscillation, run the working-set check and only then write the knob.
pub(crate) fn apply_proposal(
    state: &mut ControllerState,
    at: usize,
    proposal: Proposal,
    actions: &mut Actions,
) {
    let envelope = state.stages[at].envelope;
    let (clamped, was_clamped) = envelope.clamp(proposal.morsel_target);
    {
        let ctl = &mut state.stages[at];
        ctl.sizer_proposals = ctl.sizer_proposals.saturating_add(1);
        ctl.predicted_peak = proposal.predicted_peak;
        if was_clamped {
            ctl.sizer_clamps = ctl.sizer_clamps.saturating_add(1);
        }
    }

    // RC-I3. The damping is the controller's rule, not the sizer's courtesy: `Observation`
    // carries both numbers so a sizer can honour it, and this is what happens when one does
    // not. The breach path of f.7 and the device path of f.11 do not come through here, which
    // is how the exception RC-I3 names stays an exception.
    if state.stages[at].completions_since_adjust < model::damping(state) {
        return;
    }

    // A frozen stage takes no proposal at all, which is the point of the freeze (RC-I5).
    if let Some(until) = state.stages[at].frozen_until {
        if state.stages[at].completions < until {
            return;
        }
        state.stages[at].frozen_until = None;
    }

    let current = state.stages[at].target;
    if clamped == current {
        return;
    }
    let sign: i8 = if clamped > current { 1 } else { -1 };

    // RC-I5: a stage that keeps changing its mind is not converging, and the cheapest way to
    // stop a controller oscillating is to stop it deciding for a while.
    {
        let ctl = &mut state.stages[at];
        let flipped = ctl.last_adjust_sign != 0 && ctl.last_adjust_sign != sign;
        ctl.flips_window.push_back(i8::from(flipped));
        while ctl.flips_window.len() > FLIP_WINDOW {
            ctl.flips_window.pop_front();
        }
    }
    let flips: u32 = state.stages[at]
        .flips_window
        .iter()
        .map(|flip| u32::from(*flip > 0))
        .sum();
    if flips > state.cfg.oscillation_flips {
        freeze(state, at, actions);
        return;
    }

    // RC-I1: the check is on the whole set, not on the stage, because the budget is shared.
    let mut targets: Vec<u64> = state.stages.iter().map(|s| s.target).collect();
    targets[at] = clamped;
    commit_targets(state, &targets, actions);
    {
        let ctl = &mut state.stages[at];
        ctl.last_adjust_sign = sign;
        ctl.completions_since_adjust = 0;
    }
    if sign > 0 {
        tighten_safety(state, at);
    }
}

/// RC-I5. Freeze the target at the geometric mean of the last ten, for `freeze_morsels`
/// completions of that stage, and say so.
fn freeze(state: &mut ControllerState, at: usize, actions: &mut Actions) {
    let mean = geometric_mean(&state.stages[at].recent_targets)
        .unwrap_or(state.stages[at].target)
        .clamp(state.cfg.morsel_min, state.cfg.morsel_max);
    let until = state.stages[at]
        .completions
        .saturating_add(u64::from(state.cfg.freeze_morsels));
    let stage = state.stages[at].stage;
    let mut targets: Vec<u64> = state.stages.iter().map(|s| s.target).collect();
    targets[at] = mean;
    {
        let ctl = &mut state.stages[at];
        ctl.frozen_until = Some(until);
        ctl.flips_window.clear();
        ctl.last_adjust_sign = 0;
        ctl.completions_since_adjust = 0;
    }
    commit_targets(state, &targets, actions);
    state.freezes = state.freezes.saturating_add(1);
    tracing::warn!(target: "ctl.freeze", stage, bytes = mean, "oscillation freeze");
    state.note(format!("oscillation freeze on stage {stage}"));
}

/// The geometric mean of a window of targets; `None` for an empty window.
pub(crate) fn geometric_mean(window: &std::collections::VecDeque<u64>) -> Option<u64> {
    if window.is_empty() {
        return None;
    }
    let mut sum = 0.0f64;
    let mut count = 0.0f64;
    for target in window {
        if *target == 0 {
            continue;
        }
        sum += (*target as f64).ln();
        count += 1.0;
    }
    if count == 0.0 {
        return None;
    }
    Some(model::scale(1, (sum / count).exp()))
}

/// f.4's second half: an accepted increase over a steady window buys the margin down a little,
/// never below the floor. The floor is the guarantee; what evidence moves is the margin.
fn tighten_safety(state: &mut ControllerState, at: usize) {
    let floor = state.cfg.safety_floor;
    let ctl = &mut state.stages[at];
    let ratios: Vec<f64> = ctl
        .peak_ratios
        .iter()
        .rev()
        .take(STEADY_WINDOW)
        .copied()
        .collect();
    if ratios.len() < STEADY_WINDOW {
        return;
    }
    let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
    if mean <= 0.0 {
        return;
    }
    let variance = ratios.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / ratios.len() as f64;
    if variance.sqrt() / mean < STEADY_CV {
        ctl.safety = (ctl.safety - SAFETY_STEP).max(floor);
    }
}

/// f.8. Score the active sizer's prediction against the shadow rule sizer's and swap when the
/// clamp rate or the error ratio says the model is not earning its place.
pub(crate) fn score_prediction(
    state: &mut ControllerState,
    at: usize,
    peak_delta: u64,
    bytes_in: u64,
) {
    if peak_delta == 0 || bytes_in == 0 {
        return;
    }
    let observed = peak_delta as f64;
    let predicted = state.stages[at].predicted_peak;
    let shadow = state.stages[at].peak_ewma * bytes_in as f64;
    let ctl = &mut state.stages[at];
    // A sizer that predicts nothing is scored as wrong by the whole of the observation, which
    // is what makes the rule sizer the shadow rather than the candidate (f.8).
    let error = match predicted {
        Some(peak) => (peak as f64 - observed).abs() / observed,
        None => 1.0,
    };
    ctl.sizer_errors.push_back(error);
    while ctl.sizer_errors.len() > FLIP_WINDOW {
        ctl.sizer_errors.pop_front();
    }
    ctl.shadow_errors
        .push_back((shadow - observed).abs() / observed);
    while ctl.shadow_errors.len() > FLIP_WINDOW {
        ctl.shadow_errors.pop_front();
    }
}

/// f.8. Replace a misbehaving sizer with the rule sizer for the rest of the run.
pub(crate) fn maybe_fall_back(state: &mut ControllerState, at: usize, seq: amoru_kernel::Seq) {
    if state.stages[at].sizer.name() == "rule" {
        return;
    }
    if state.stages[at].sizer_proposals < FALLBACK_MIN_PROPOSALS {
        return;
    }
    let ratio = f64::from(state.cfg.fallback_error_ratio);
    let clamp_rate =
        f64::from(state.stages[at].sizer_clamps) / f64::from(state.stages[at].sizer_proposals);
    let clamped_too_often = ratio > 0.0 && clamp_rate > 1.0 / ratio;
    let mine = profile::percentile(&state.stages[at].sizer_errors, 0.95);
    let shadow = profile::percentile(&state.stages[at].shadow_errors, 0.95);
    let worse_than_shadow = !state.stages[at].shadow_errors.is_empty() && mine > ratio * shadow;
    if !clamped_too_often && !worse_than_shadow {
        return;
    }
    let stage = state.stages[at].stage;
    let was = state.stages[at].sizer.name();
    let target_fraction = state.cfg.target_fraction;
    let increase_step = state.cfg.increase_step;
    state.stages[at].sizer = Box::new(RuleSizer::new(target_fraction, increase_step));
    state.stages[at].sizer_clamps = 0;
    state.stages[at].sizer_proposals = 0;
    state.stages[at].predicted_peak = None;
    if state.fallback_at.is_none() {
        state.fallback_at = Some(seq);
    }
    tracing::warn!(target: "ctl.fallback", stage, was, "sizer replaced by the rule sizer");
    let why = if clamped_too_often {
        "clamped too often"
    } else {
        "prediction error above the rule sizer's"
    };
    state.note(format!(
        "sizer fallback on stage {stage}: {was} replaced by rule ({why})"
    ));
}
