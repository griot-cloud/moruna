//! The sizing decision (d.1, f.4, f.8).
//!
//! Learning proposes and rules bound (D7). A sizer sees one `Observation` and nothing else,
//! and its proposal is clamped by the controller to an `Envelope` derived from the budget and
//! the probe, so a sizer that is confidently wrong changes how fast the run converges and
//! never whether it fits the budget.

mod learned;
mod rule;

pub use learned::LearnedSizer;
pub use rule::RuleSizer;

use moruna_kernel::{MorselFeatures, StageId, TraceRecord};

/// The closed interval of morsel targets the controller permits a sizer to propose (b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Envelope {
    /// `morsel.min_bytes`.
    pub min: u64,
    /// `min(morsel.max_bytes, budget_for_stage / (share x a_k x safety))`.
    pub max: u64,
}

impl Envelope {
    /// The target clamped into the envelope, and whether the clamp changed it (RC-I2).
    pub fn clamp(&self, target: u64) -> (u64, bool) {
        let high = self.max.max(self.min);
        let clamped = target.clamp(self.min, high);
        (clamped, clamped != target)
    }
}

/// Everything a sizer is allowed to see about a stage.
pub struct Observation {
    /// The stage.
    pub stage: StageId,
    /// The features of the most recent morsel of this stage.
    pub features: MorselFeatures,
    /// Workers allowed to take tasks right now.
    pub active_workers: u16,
    /// The stage's measured amplification.
    pub a_k: f64,
    /// The safety multiplier in force.
    pub safety: f32,
    /// The stage's current morsel target.
    pub target: u64,
    /// Completions since the last adjustment, so a sizer cannot ignore damping.
    pub completions_since_adjust: u32,
    /// `controller.damping_completions` as in force now.
    pub damping: u32,
    /// `trace.tail(stage, 32)`. It may hold fewer records than that, or none: the trace tail
    /// answers out of the trace writer's in-memory chunks only (04 f.3), so what a window
    /// holds is bounded by `trace.memory_limit`. A sizer must read a short window as less
    /// evidence and never as evidence that the stage was idle (11 f).
    pub recent: Vec<TraceRecord>,
}

/// One sizer's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proposal {
    /// The morsel target it proposes, before the envelope clamp.
    pub morsel_target: u64,
    /// The peak the sizer expects at that target; `None` for [`RuleSizer`]. Feeds f.8, where
    /// a `None` prediction counts as a prediction error of 1.0.
    pub predicted_peak: Option<u64>,
}

/// What one completed morsel measured, fed back to the sizer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizerOutcome {
    /// Peak anonymous host bytes above the sample taken before `apply`.
    pub peak_delta: u64,
    /// Input payload bytes.
    pub bytes_in: u64,
    /// Wall nanoseconds inside `apply`.
    pub wall_ns: u64,
}

/// The decision function that sizes morsels for one stage.
///
/// One instance per stage. The controller clamps every proposal to the envelope (RC-I2) and
/// replaces a misbehaving sizer with [`RuleSizer`] for the rest of the run (f.8), so an
/// implementation of this trait cannot break the budget, only the convergence.
pub trait Sizer: Send {
    /// Propose a morsel target for the stage. The controller clamps the result.
    fn propose(&mut self, obs: &Observation, envelope: &Envelope) -> Proposal;
    /// Feed back what a completed morsel measured.
    fn observe(&mut self, obs: &Observation, outcome: &SizerOutcome);
    /// How much the sizer trusts itself, between 0 and 1. Advisory; it gates nothing.
    fn confidence(&self) -> f32;
    /// The name the run report calls it by.
    fn name(&self) -> &'static str;
}
