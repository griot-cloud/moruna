//! Device out of memory (f.11, architecture 7).
//!
//! The scheduler retries an allocation failure in a device tier once on the same morsel, after
//! the record hook has run. That retry only helps if the device has room by the time it
//! happens, so this path does two things at once on the recording thread: it halves the stage
//! that failed, and it takes the input queue's device high water to zero so the placement
//! engine demotes what it had promoted but not yet handed over. The next tick puts the high
//! water back.

use amoru_kernel::{AmoruError, Knob, Outcome, TierKind, TraceRecord};

use crate::{Actions, ControllerState, features_of};

/// f.11. Called from `on_record` on the recording worker, before the breach check, because a
/// device allocation failure is an error record and not a peak.
pub(crate) fn check(state: &mut ControllerState, r: &TraceRecord, actions: &mut Actions) {
    if r.outcome != Outcome::Error {
        return;
    }
    let Some(device) = device_alloc_failure(r.error.as_deref()) else {
        return;
    };
    let Some(at) = state.stages.iter().position(|s| s.stage == r.stage) else {
        return;
    };
    let stage = state.stages[at].stage;
    let morsel_min = state.cfg.morsel_min;

    if state.stages[at].device_breaches >= 1 {
        // Once is a retry worth making; twice on the same stage is a device budget the stage
        // cannot be sized into, and the diagnostic carries the device figures.
        let budget = state.budgets.device[usize::from(device).min(7)];
        state.terminated = true;
        actions.terminate = Some(AmoruError::Budget {
            seq: r.seq,
            stage,
            footprint: r.dev_mem_peak,
            budget,
            features: features_of(r),
        });
        return;
    }

    state.stages[at].device_breaches = 1;
    let bytes = (state.stages[at].target / 2).max(morsel_min);
    state.stages[at].target = bytes;
    state.stages[at].completions_since_adjust = 0;
    crate::model::refresh_envelopes(state);
    tracing::warn!(target: "ctl.device_oom", stage, seq = r.seq, device, "device out of memory");
    actions.knobs.push(Knob::MorselTarget { stage, bytes });
    // The input queue of the failing stage is the one holding promoted device bytes.
    let input = stage.saturating_sub(1);
    actions.knobs.push(Knob::HighWater {
        stage: input,
        tier: TierKind::Device,
        bytes: 0,
    });
    state.high_water_override = Some((input, 0));
    state.note(format!("device OOM retry on stage {stage}"));
}

/// Whether an error message is an allocation failure in a device tier, and which device.
///
/// The message is the `Display` of `AmoruError::Alloc` (contracts d.14), whose tier is
/// formatted with `Debug`, so the device reads `Device(DeviceId(0))`; the shorter
/// `Device(0)` that 11 k writes is accepted too, so a scheduler that formats the tier more
/// briefly does not silently turn this path off.
fn device_alloc_failure(error: Option<&str>) -> Option<u8> {
    let error = error?;
    if !error.contains("alloc ") {
        return None;
    }
    let after = error.split("Device(").nth(1)?;
    let inner = after.strip_prefix("DeviceId(").unwrap_or(after);
    let digits: String = inner.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return Some(0);
    }
    digits.parse().ok()
}
