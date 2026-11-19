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
    /// A stage waits on a port's answer, and the rest of the chain is not saturated (MH 4.10):
    /// nothing a knob does makes the answer come sooner.
    Waiting,
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

// stream P: the controller's fit and decisions in the run's report.
/// The most decisions the report lists; the rest are counted.
const DECISIONS_KEPT: usize = 96;

/// The knobs the controller moved, in order, each with the second it moved it, for the run's
/// report: what it decided, beside the fit it decided from (`fit_notes`). The workers, the
/// morsel targets, the read-ahead and the staging; the high waters and promotion windows,
/// which follow from those, are counted only.
#[derive(Debug)]
pub(crate) struct Decisions {
    started: std::time::Instant,
    kept: Vec<(f64, String)>,
    more: u64,
}

impl Decisions {
    pub(crate) fn new() -> Decisions {
        Decisions {
            started: std::time::Instant::now(),
            kept: Vec::new(),
            more: 0,
        }
    }

    pub(crate) fn note(&mut self, knob: &moruna_kernel::Knob) {
        use moruna_kernel::Knob;
        let text = match knob {
            Knob::ActiveWorkers(n) => format!("workers {n}"),
            Knob::MorselTarget { stage, bytes } => format!("stage {stage} morsel {bytes}"),
            Knob::ReadAhead(n) => format!("read-ahead {n}"),
            Knob::StagingTrigger { stage, on } => {
                format!("stage {stage} staging {}", if *on { "on" } else { "off" })
            }
            _ => {
                self.more += 1;
                return;
            }
        };
        self.keep(text);
    }

    pub(crate) fn ended(&mut self, diagnostic: &moruna_kernel::MorunaError) {
        self.keep(format!("ended the run: {diagnostic}"));
    }

    fn keep(&mut self, text: String) {
        if self.kept.len() < DECISIONS_KEPT {
            let at = self.started.elapsed().as_secs_f64();
            self.kept.push((at, text));
        } else {
            self.more += 1;
        }
    }

    /// One line: the decisions at each second they were made, oldest first.
    pub(crate) fn line(&self) -> Option<String> {
        if self.kept.is_empty() {
            return None;
        }
        let mut line = String::from("controller decisions:");
        let mut at = None;
        for (t, text) in &self.kept {
            let stamp = format!("{t:.2} s");
            if at.as_deref() == Some(stamp.as_str()) {
                line.push_str(&format!(", {text}"));
            } else {
                line.push_str(&format!(
                    "{} {stamp} {text}",
                    if at.is_some() { ";" } else { "" }
                ));
                at = Some(stamp);
            }
        }
        if self.more > 0 {
            line.push_str(&format!(
                "; and {} more (high waters, promotion windows, or past {DECISIONS_KEPT})",
                self.more
            ));
        }
        Some(line)
    }
}

/// The fit each stage is sized from, and what bounds the workers, as the run ended: the
/// figures a reader needs to tell which knob held the run, which no run left behind before.
pub(crate) fn fit_notes(state: &crate::ControllerState) -> Vec<String> {
    let mut notes: Vec<String> = state
        .stages
        .iter()
        .map(|ctl| {
            // Which slope stands (item 6.3w), once a record has been fitted.
            let from = ctl
                .anon_slope
                .map(|from| format!(" {}", from.said()))
                .unwrap_or_default();
            format!(
                "controller stage {}: amplification {:.2}, anon fit {} bytes fixed plus {:.3} bytes a morsel byte{from}, safety {:.2}, morsel target {} bytes, {} breaches at the floor",
                ctl.stage, ctl.a_k, ctl.c_anon, ctl.a_anon, ctl.safety, ctl.target, ctl.floor_breaches
            )
        })
        .collect();
    if !state.stages.is_empty() {
        notes.push(format!(
            "controller workers: {} active at the end; the CPU allows {}, the arena's half {}, the anon fit {}; {} bytes of anon for kernels, breach line {} bytes, stop line {} bytes; {} breaches, {} freezes",
            state.active_workers,
            crate::elastic::workers_bound(state),
            crate::classify::workers_allowed(state),
            crate::breach::anon_workers(state),
            crate::model::anon_for_kernels(state),
            state.breach_line(),
            state.stop_line(),
            state.breaches,
            state.freezes
        ));
    }
    notes
}

/// The summary as it stands. Reads the knob snapshot with the lock released (RC-I10).
pub(crate) fn summary(ctl: &Inner) -> ControllerSummary {
    let final_knobs = ctl.knob_snapshot();
    let decisions = ctl
        .decisions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .line();
    let state = ctl.held();
    let mut notes = state.notes.clone();
    notes.extend(fit_notes(&state));
    notes.extend(decisions);
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
