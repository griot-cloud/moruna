//! What the controller hands the run report (d.1, j), and `stop` (f.9).

use moruna_kernel::{KnobSnapshot, Seq};

use crate::{Inner, Phase, profile};

/// What the controller decided was limiting the run at a moment (f.6).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Bottleneck {
    /// The source cannot deliver fast enough.
    IoRead,
    /// Memory is the binding constraint.
    Memory,
    /// The sink cannot absorb fast enough.
    Sink,
    /// The kernels are the binding constraint, which is where a tuned run wants to be.
    Compute,
    /// The CPU quota is throttling the process.
    CpuQuota,
    /// Nothing is limiting anything.
    Idle,
    /// A kernel's state is growing with morsels seen.
    StateGrowth,
}

/// The controller's contribution to the run report (d.1, j).
#[derive(Clone, Debug)]
pub struct ControllerSummary {
    /// Consecutive equal classes compressed into one entry each, with its duration in seconds.
    pub timeline: Vec<(f64, Bottleneck)>,
    /// The sizer the run ended with.
    pub sizer: &'static str,
    /// The morsel at which a learned sizer was replaced by the rule sizer, if it was (f.8).
    pub fallback_at: Option<Seq>,
    /// Oscillation freezes (RC-I5).
    pub freezes: u32,
    /// Breaches (RC-I4).
    pub breaches: u32,
    /// Every knob's value at the end.
    pub final_knobs: KnobSnapshot,
    /// Human-readable facts the report appends to its own notes (04 d.1 `controller_notes`).
    pub notes: Vec<String>,
}

/// The summary as it stands. Reads the knob snapshot with the lock released (RC-I10).
pub(crate) fn summary(ctl: &Inner) -> ControllerSummary {
    let final_knobs = ctl.knob_snapshot();
    let state = ctl.held();
    let mut notes = state.notes.clone();
    if state.ticks_skipped > 0 {
        notes.push(format!(
            "{} ticks skipped: the sample took longer than the tick bound",
            state.ticks_skipped
        ));
    }
    if state.records_dropped > 0 {
        notes.push(format!(
            "{} trace records dropped from the controller's window; the trace itself is complete",
            state.records_dropped
        ));
    }
    ControllerSummary {
        timeline: state.timeline.clone(),
        sizer: state
            .stages
            .first()
            .map(|stage| stage.sizer.name())
            .unwrap_or("rule"),
        fallback_at: state.fallback_at,
        freezes: state.freezes,
        breaches: state.breaches,
        final_knobs,
        notes,
    }
}

/// f.9. The profile is written when the run completed, which the controller reads off the
/// scheduler rather than being told: a run that exhausted its source and was not terminated by
/// the controller is a run whose measurements are worth keeping. A terminated run's numbers
/// describe a kernel that did not fit, and storing them would teach the next run the wrong
/// lesson.
pub(crate) fn stop(ctl: &Inner) -> ControllerSummary {
    let exhausted = ctl.peers().stats.scheduler_stats().source_exhausted;
    let completed = {
        let mut state = ctl.held();
        state.phase = Phase::Stopped;
        state.source_exhausted = state.source_exhausted || exhausted;
        state.source_exhausted && !state.terminated
    };
    if completed {
        profile::write_all(ctl);
    } else {
        let mut state = ctl.held();
        if state.cfg.profiles_dir.is_some() {
            state.note("profile not written: the run did not complete".into());
        }
    }
    summary(ctl)
}
