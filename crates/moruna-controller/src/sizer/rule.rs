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
        // How much of the growth this stage is allowed the stage is actually using, in the
        // quantity the records measure: the process's growth per morsel, `peak_ewma` per byte.
        // The allowance at the top of the envelope is what `target_fraction` aims at, so a stage
        // whose measured growth is well under it has room to grow.
        //
        // Both sides are the part that scales with the morsel. The fixed term is paid per morsel
        // in flight whatever the target, so halving cannot buy it back: counted, a kernel whose
        // growth is mostly fixed read as over its allowance at every size and was halved to the
        // floor. And both are the anonymous slope (2026-11-14): RB3 made `a_k` the in-arena
        // ratio, about 1.2, while `peak_ewma` stayed the anonymous growth, 2 to 8 for a Python
        // kernel, so every stage read as several times over and was halved while the process
        // sat at a sixth of its ceiling (Audit on the VM: 22 MB morsels, 3.8 of 21.6 GiB).
        let allowance = self.target_fraction * envelope.max as f64 * obs.a_anon;
        if allowance <= 0.0 || self.observations == 0 {
            return Proposal {
                morsel_target: obs.target,
                predicted_peak: None,
            };
        }
        let using = (self.peak_ewma * obs.target as f64 - obs.anon_fixed as f64).max(0.0);
        let ratio = using / allowance;
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

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::MorselFeatures;

    const MIB: u64 = 1 << 20;

    fn observation(target: u64, a_anon: f64, anon_fixed: u64) -> Observation {
        Observation {
            stage: 1,
            features: MorselFeatures::default(),
            active_workers: 8,
            // The in-arena ratio of RB3, a fraction of the anonymous one for a Python kernel.
            a_k: 1.2,
            a_anon,
            anon_fixed,
            safety: 1.5,
            target,
            completions_since_adjust: 8,
            damping: 8,
            recent: Vec::new(),
        }
    }

    fn after(peak_delta: u64, obs: &Observation) -> Proposal {
        let mut sizer = RuleSizer::new(0.85, 0.25);
        sizer.observe(
            obs,
            &SizerOutcome {
                peak_delta,
                bytes_in: obs.target,
                wall_ns: 1,
            },
        );
        sizer.propose(
            obs,
            &Envelope {
                min: MIB,
                max: 64 * MIB,
            },
        )
    }

    /// RC-T23 (f.4, 2026-11-14): growth within the anonymous allowance grows the morsel, however
    /// small the in-arena ratio. Audit's records grew about 2.7 bytes per byte against a fitted
    /// slope of 3.5; compared with `a_k`, 1.2, a 32 MiB morsel read as a third over its allowance
    /// and was halved, and the run settled at 22 MB with the process at a sixth of its ceiling.
    #[test]
    fn rc_t23_growth_within_the_anonymous_allowance_grows_the_morsel() {
        let obs = observation(32 * MIB, 3.5, 0);
        let proposal = after((2.7 * (32 * MIB) as f64) as u64, &obs);
        assert!(proposal.morsel_target > 32 * MIB, "{proposal:?}");
    }

    /// RC-T23: what a morsel costs whatever its size is not halved away; only the part that
    /// scales with the morsel is weighed against the allowance.
    #[test]
    fn rc_t23_a_fixed_cost_does_not_halve_the_morsel() {
        let obs = observation(16 * MIB, 0.5, 400 * MIB);
        let proposal = after(400 * MIB + 8 * MIB, &obs);
        assert!(proposal.morsel_target >= 16 * MIB, "{proposal:?}");
    }

    /// RC-T23: growth past the allowance still halves.
    #[test]
    fn rc_t23_growth_past_the_allowance_halves() {
        let obs = observation(16 * MIB, 3.5, 0);
        let proposal = after(20 * 16 * MIB, &obs);
        assert_eq!(proposal.morsel_target, 8 * MIB, "{proposal:?}");
    }
}
