//! The probe protocol's decisions (f.2) and the resume seeding of f.14.
//!
//! The scheduler executes the probe through `Prober`; what the controller owns is which stage
//! is probed, with how many bytes, what the measurement means and how much margin the evidence
//! earns. RC-I6 is the rule the order exists for: no stage is sized at anything but
//! `morsel.probe_bytes` until its amplification has been measured or a profile the interrupted
//! run wrote has been trusted in its place.

use amoru_kernel::ProbeResult;

use crate::{Inner, MIN_AMPLIFICATION, Phase, Result, model, profile};

/// The relative distance between the probe and the profile that counts as drift (f.2).
const DRIFT: f64 = 0.5;
/// `k` in the safety derivation of f.2: how much variance costs.
const VARIANCE_WEIGHT: f64 = 2.0;
/// `c` in the safety derivation of f.2: how much inexperience costs. Sixteen samples add 1.0
/// to the margin, forty thousand add 0.02.
const EVIDENCE_WEIGHT: f64 = 4.0;

/// f.2 (`probe_all`) and f.14 (`probe_missing`).
pub(crate) fn probe_all(ctl: &Inner, resuming: bool) -> Result<()> {
    {
        let mut state = ctl.held();
        if state.phase != Phase::Prepared {
            return Err(amoru_kernel::AmoruError::Config {
                name: "controller",
                msg: format!("probe called in phase {:?}", state.phase),
            });
        }
        state.tiny = model::is_tiny(&state);
        if state.tiny {
            state.note("small dataset: no adaptation".into());
        }
        // The profile store is the controller's memory across runs, and on a resumed run it is
        // its memory of the run that was interrupted (f.14). It is read once, here.
        load_profiles(&mut state);
        // Nothing to probe for: a run with no kernels has no amplification, and a dataset that
        // fits four times over in the budget is not worth a measurement (RC-I6, f.3, f.10).
        if state.stage_count() == 0 || state.tiny {
            state.phase = Phase::Probed;
            return Ok(());
        }
    }

    let mut seeded = 0usize;
    let mut probed = 0usize;
    let mut upstream: Option<ProbeResult> = None;
    let stages = {
        let state = ctl.held();
        state.stages.len()
    };

    for at in 0..stages {
        // Decide with the lock held, call with it released (RC-I10).
        let request = {
            let mut state = ctl.held();
            seed(&mut state, at);
            if resuming && state.stages[at].profile.is_some() {
                state.stages[at].seeded = true;
                None
            } else {
                Some((
                    state.stages[at].stage,
                    probe_bytes(&state, at, upstream.as_ref()),
                ))
            }
        };
        let Some((stage, bytes)) = request else {
            seeded += 1;
            continue;
        };
        let result = ctl.peers().prober.probe(stage, bytes)?;
        probed += 1;
        {
            let mut state = ctl.held();
            record_probe(&mut state, at, &result);
            let ctl_stage = &state.stages[at];
            tracing::info!(
                target: "ctl.probe",
                stage,
                bytes,
                a_k = ctl_stage.a_k,
                a_k_dev = ctl_stage.a_k_dev,
                safety = ctl_stage.safety,
                "probe"
            );
        }
        upstream = Some(result);
    }

    {
        let mut state = ctl.held();
        if resuming {
            state.note(format!("resumed: {seeded} stages seeded, {probed} probed"));
        }
        state.phase = Phase::Probed;
    }
    if resuming {
        // A second crash should not cost what this run was just told (f.14).
        profile::write_all(ctl);
    }
    Ok(())
}

/// Read the profile store once, per stage (e.3). A file that cannot be parsed is ignored with
/// a note: a stale opinion is not worth failing a run over.
fn load_profiles(state: &mut crate::ControllerState) {
    let Some(dir) = state.cfg.profiles_dir.clone() else {
        return;
    };
    for at in 0..state.stages.len() {
        let stage = state.stages[at].stage;
        let Some(kernel) = state.kernels.iter().find(|k| k.stage == stage) else {
            continue;
        };
        match profile::load(&dir, &kernel.fingerprint, &kernel.schema_hash) {
            profile::Loaded::Found(found) => state.stages[at].profile = Some(found),
            profile::Loaded::Missing => {}
            profile::Loaded::Unreadable(why) => {
                state.note(format!("profile for stage {stage} ignored: {why}"));
            }
        }
    }
}

/// Seed the amplification and the margin before the probe runs (f.2): the profile's p95 if
/// there is one, the kernel's hint if not, and 4.0 if neither, which is the amplification of a
/// kernel nobody has said anything about.
fn seed(state: &mut crate::ControllerState, at: usize) {
    let stage = state.stages[at].stage;
    let hints = state.hints(stage);
    let safety_initial = state.cfg.safety_initial;
    let safety_floor = state.cfg.safety_floor;
    let profile = state.stages[at].profile.clone();
    let ctl = &mut state.stages[at];
    match &profile {
        Some(found) => {
            ctl.a_k = found.a_k_p95.max(MIN_AMPLIFICATION);
            ctl.a_k_dev = found.a_k_dev_p95;
            ctl.safety = safety_from_evidence(found, safety_floor, safety_initial);
        }
        None => {
            ctl.a_k = hints
                .expected_amplification
                .unwrap_or(4.0)
                .max(MIN_AMPLIFICATION);
            ctl.a_k_dev = 0.0;
            ctl.safety = safety_initial;
        }
    }
}

/// f.2's safety derivation: the margin is a function of evidence, not a constant. A kernel
/// seen forty thousand times with a steady ratio starts near the floor; one seen sixteen times
/// starts near the initial value; one seen often but erratically stays well above the floor.
/// The floor itself never moves: what shrinks with evidence is the margin, not the guarantee.
pub(crate) fn safety_from_evidence(profile: &profile::Profile, floor: f32, initial: f32) -> f32 {
    let p50 = if profile.a_k_p50 > 0.0 {
        profile.a_k_p50
    } else {
        profile.a_k_p95.max(MIN_AMPLIFICATION)
    };
    let samples = profile.a_k_samples.max(1) as f64;
    let from_variance = VARIANCE_WEIGHT * profile.a_k_var.max(0.0).sqrt() / p50;
    let from_evidence = EVIDENCE_WEIGHT / samples.sqrt();
    let safety = f64::from(floor) + from_variance + from_evidence;
    (safety as f32).clamp(floor, initial)
}

/// The probe size of f.2: `morsel.probe_bytes`, or the rows the kernel said it would rather
/// have converted to bytes, clamped into the morsel range. A `morsel_max` below the probe size
/// is a misconfiguration the clamp answers by probing at `morsel_max` (h).
fn probe_bytes(state: &crate::ControllerState, at: usize, upstream: Option<&ProbeResult>) -> u64 {
    let stage = state.stages[at].stage;
    let hints = state.hints(stage);
    let bytes = match hints.preferred_rows {
        Some(rows) if rows > 0 => {
            let per_row = bytes_per_row(state, upstream);
            if per_row > 0.0 {
                model::scale(rows, per_row)
            } else {
                state.cfg.probe_bytes
            }
        }
        _ => state.cfg.probe_bytes,
    };
    bytes.clamp(state.cfg.morsel_min, state.cfg.morsel_max)
}

/// Bytes per row: the plan's own ratio for stage 1, and the upstream probe's for a later
/// stage, because a kernel changes how wide a row is (f.2).
fn bytes_per_row(state: &crate::ControllerState, upstream: Option<&ProbeResult>) -> f64 {
    match upstream {
        Some(result) if result.rows_in > 0 => result.bytes_in as f64 / result.rows_in as f64,
        _ => {
            if state.cfg.plan.total_rows > 0 {
                state.cfg.plan.total_bytes as f64 / state.cfg.plan.total_rows as f64
            } else {
                0.0
            }
        }
    }
}

/// What one probe result means (f.2).
fn record_probe(state: &mut crate::ControllerState, at: usize, result: &ProbeResult) {
    let safety_initial = state.cfg.safety_initial;
    let stage = state.stages[at].stage;
    let hints = state.hints(stage);
    let profile = state.stages[at].profile.clone();
    let mut drift = false;
    {
        let ctl = &mut state.stages[at];
        if result.bytes_in > 0 {
            // A stage whose amplification measures below the floor (a filter that drops most
            // rows) is held at the floor, so the allowance stays conservative (h).
            let measured = result.peak_delta as f64 / result.bytes_in as f64;
            ctl.a_k = measured.max(MIN_AMPLIFICATION);
            if hints.uses_device_memory {
                ctl.a_k_dev = result.dev_peak_delta as f64 / result.bytes_in as f64;
            }
        }
        if let Some(found) = &profile
            && found.a_k_p95 > 0.0
        {
            let distance = (ctl.a_k - found.a_k_p95).abs() / found.a_k_p95;
            if distance > DRIFT {
                // The probe is what is true now; the profile is what was true before. Trust
                // the probe and put the margin back where an unseen kernel starts.
                drift = true;
                ctl.safety = safety_initial;
            }
        }
        ctl.wall_ewma = result.wall_ns as f64;
    }
    if drift {
        state.note(format!("profile drift on stage {stage}"));
    }
}
