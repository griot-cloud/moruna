//! `LearnedSizer`, the v1 stub (d.1, f.8).
//!
//! The model itself is phase 8. What ships now is the stub and the shadow error tracking the
//! controller does around it, so the fallback path of RC-I2 is built and tested before there
//! is anything to fall back from: the phase 8 implementation replaces this body without
//! changing the trait, and the clamp and the fallback stay exactly where they are.

use super::{Envelope, Observation, Proposal, RuleSizer, Sizer, SizerOutcome};

/// The learned sizer. In v1 it returns the rule sizer's proposal with no prediction, which
/// f.8 scores as a prediction error of 1.0 on every record.
pub struct LearnedSizer {
    inner: RuleSizer,
}

impl LearnedSizer {
    /// A learned sizer over the rule sizer's parameters.
    pub fn new(target_fraction: f32, increase_step: f32) -> LearnedSizer {
        LearnedSizer {
            inner: RuleSizer::new(target_fraction, increase_step),
        }
    }
}

impl Sizer for LearnedSizer {
    fn propose(&mut self, obs: &Observation, envelope: &Envelope) -> Proposal {
        Proposal {
            predicted_peak: None,
            ..self.inner.propose(obs, envelope)
        }
    }

    fn observe(&mut self, obs: &Observation, outcome: &SizerOutcome) {
        self.inner.observe(obs, outcome);
    }

    fn confidence(&self) -> f32 {
        // A stub knows nothing; the number is advisory and the fallback of f.8 is what acts.
        0.0
    }

    fn name(&self) -> &'static str {
        "learned"
    }
}
