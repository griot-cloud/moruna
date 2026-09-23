//! `RuleSizer`, the additive-increase multiplicative-decrease sizer that ships in v1 (f.4).

use super::{Envelope, Observation, Proposal, Sizer, SizerOutcome};

/// The exponential weight of one observation on the peak ratio (f.4).
const ALPHA: f64 = 0.2;
/// Below this fraction of the allowed peak the sizer increases (f.4).
const INCREASE_BELOW: f64 = 0.85;
/// Above this fraction of the allowed peak the sizer halves (f.4).
const DECREASE_ABOVE: f64 = 1.0;

/// The rule sizer: additive increase while the stage is using less of its allowance than
/// `target_fraction` asks for, multiplicative decrease when it is using more.
///
/// It predicts nothing (`Proposal::predicted_peak` is `None`) and is therefore also the
/// shadow the learned sizer's prediction error is compared against (f.8), where the shadow's
/// prediction is `peak_ewma x bytes_in`.
pub struct RuleSizer {
    target_fraction: f64,
    increase_step: f64,
    peak_ewma: f64,
    observations: u64,
}

impl RuleSizer {
    /// A rule sizer with `controller.target_fraction` and `controller.increase_step`.
    pub fn new(target_fraction: f32, increase_step: f32) -> RuleSizer {
        RuleSizer {
            target_fraction: f64::from(target_fraction),
            increase_step: f64::from(increase_step),
            peak_ewma: 0.0,
            observations: 0,
        }
    }

    /// The exponentially weighted `peak_delta / bytes_in` this sizer has seen (f.4, f.8).
    pub fn peak_ewma(&self) -> f64 {
        self.peak_ewma
    }
}

impl Sizer for RuleSizer {
    fn propose(&mut self, obs: &Observation, envelope: &Envelope) -> Proposal {
        // RC-I3: the sizer honours damping itself, so a proposal never arrives early.
        if obs.completions_since_adjust < obs.damping {
            return Proposal {
                morsel_target: obs.target,
                predicted_peak: None,
            };
        }
        // How much of the peak this stage is allowed the stage is actually using. The
        // allowance at the top of the envelope is what `target_fraction` aims at, so a stage
        // whose measured peak is well under it has room to grow.
        let allowance_target =
            self.target_fraction * envelope.max as f64 * obs.a_k * f64::from(obs.safety);
        if allowance_target <= 0.0 || self.observations == 0 {
            return Proposal {
                morsel_target: obs.target,
                predicted_peak: None,
            };
        }
        let ratio = self.peak_ewma * f64::from(obs.safety) * obs.target as f64 / allowance_target;
        let morsel_target = if ratio < INCREASE_BELOW {
            crate::model::scale(obs.target, 1.0 + self.increase_step)
        } else if ratio > DECREASE_ABOVE {
            obs.target / 2
        } else {
            obs.target
        };
        Proposal {
            morsel_target,
            predicted_peak: None,
        }
    }

    fn observe(&mut self, _obs: &Observation, outcome: &SizerOutcome) {
        // Never divide by a zero input: a record with no input bytes says nothing about
        // amplification and is skipped for sizing (l, numerics).
        if outcome.bytes_in == 0 {
            return;
        }
        let ratio = outcome.peak_delta as f64 / outcome.bytes_in as f64;
        self.peak_ewma = if self.observations == 0 {
            ratio
        } else {
            ALPHA * ratio + (1.0 - ALPHA) * self.peak_ewma
        };
        self.observations += 1;
    }

    fn confidence(&self) -> f32 {
        // The rule sizer is a rule, not a model: it is as confident as it has evidence, and
        // the figure gates nothing.
        self.observations.min(100) as f32 / 100.0
    }

    fn name(&self) -> &'static str {
        "rule"
    }
}
