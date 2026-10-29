//! The regression's features: logarithms of section 3's sizes, centred on a reference unit, and
//! what is known of each across a key's history (SDD 24 f.1).

use serde::{Deserialize, Serialize};

use crate::exact;
use crate::inputs::Inputs;

/// The intercept and five features: `b0 + b1·log(rows) + b2·log(bytes_per_row) +
/// b3·log(columns) + b4·log(distinct) + b5·log(workers)` (section 5.3).
pub(crate) const COEFFICIENTS: usize = 6;
/// The features without the intercept.
pub(crate) const FEATURES: usize = COEFFICIENTS - 1;

/// The reference unit the logarithms are centred on: ten thousand rows of 64 bytes, 8 columns,
/// a thousand distinct keys, one worker. The intercept is the cost of that unit, so its prior is
/// a figure a person can read.
pub(crate) const REFERENCE: [f64; FEATURES] = [10_000.0, 64.0, 8.0, 1_000.0, 1.0];
/// Feature index of rows.
pub(crate) const ROWS: usize = 0;
/// Feature index of bytes per row.
pub(crate) const BYTES_PER_ROW: usize = 1;
/// Feature index of distinct keys.
pub(crate) const DISTINCT: usize = 3;

/// The variance, in squared log units, given to a feature missing at prediction time and never
/// seen in the history: one e-fold either way.
const UNSEEN_VARIANCE: f64 = 1.0;
/// How far past the measured range a feature may lie, in log units, and still be inside it: a
/// tenth. A sample of sizes rarely reaches the ends of the range it was drawn from, and a size
/// 2% past the largest measured is not a guess about unmeasured behaviour.
const EXTRAPOLATION_MARGIN: f64 = 0.095_310_179_804_324_86; // ln 1.1

/// One unit's features, each when its input is known.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Features {
    pub x: [Option<f64>; FEATURES],
}

impl Features {
    /// The features of `inputs`. A size below one counts as one, so a logarithm exists.
    pub fn of(inputs: &Inputs) -> Features {
        let raw = [
            inputs.data.rows.map(|r| r as f64),
            inputs.bytes_per_row(),
            inputs.columns().map(f64::from),
            inputs.distinct().map(|d| d as f64),
            inputs.resources.workers.map(f64::from),
        ];
        let mut x = [None; FEATURES];
        for (i, value) in raw.into_iter().enumerate() {
            x[i] = value
                .filter(|v| v.is_finite())
                .map(|v| (v.max(1.0) / REFERENCE[i]).ln());
        }
        Features { x }
    }
}

/// What a feature vector needs at prediction time: the vector with missing features filled with
/// their means, the variance each missing one adds, and how far outside the measured range the
/// present ones lie.
pub(crate) struct Context {
    pub phi: [f64; COEFFICIENTS],
    pub missing: [f64; FEATURES],
    pub outside: f64,
}

/// Each feature's count, weighted mean and variance, and range, over a history.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct FeatureStats {
    n: [u64; FEATURES],
    #[serde(with = "exact::array")]
    weight: [f64; FEATURES],
    #[serde(with = "exact::array")]
    mean: [f64; FEATURES],
    #[serde(with = "exact::array")]
    var: [f64; FEATURES],
    #[serde(with = "exact::array")]
    min: [f64; FEATURES],
    #[serde(with = "exact::array")]
    max: [f64; FEATURES],
}

impl FeatureStats {
    /// Add one unit's features, forgetting at `lambda` (an exponentially weighted mean and
    /// variance, so the substitute for a missing feature follows the recent units).
    pub fn push(&mut self, f: &Features, lambda: f64) {
        for (i, x) in f.x.iter().enumerate() {
            let Some(x) = *x else { continue };
            if self.n[i] == 0 {
                self.min[i] = x;
                self.max[i] = x;
            } else {
                self.min[i] = self.min[i].min(x);
                self.max[i] = self.max[i].max(x);
            }
            self.n[i] += 1;
            self.weight[i] = lambda * self.weight[i] + 1.0;
            let alpha = 1.0 / self.weight[i];
            let delta = x - self.mean[i];
            self.mean[i] += alpha * delta;
            self.var[i] = (1.0 - alpha) * (self.var[i] + alpha * delta * delta);
        }
    }

    /// Whether any unit had this feature.
    pub fn seen(&self, i: usize) -> bool {
        self.n[i] > 0
    }

    /// The weighted mean and variance of one feature.
    pub fn moments(&self, i: usize) -> (f64, f64) {
        (self.mean[i], self.var[i])
    }

    /// The vector an observation is learned at: a missing feature takes its mean, so it teaches
    /// nothing about that feature's coefficient.
    pub fn phi(&self, f: &Features) -> [f64; COEFFICIENTS] {
        let mut phi = [1.0; COEFFICIENTS];
        for i in 0..FEATURES {
            phi[i + 1] = f.x[i].unwrap_or(if self.n[i] > 0 { self.mean[i] } else { 0.0 });
        }
        phi
    }

    /// The prediction context of `f` against this history.
    pub fn context(&self, f: &Features) -> Context {
        let mut missing = [0.0; FEATURES];
        let mut outside = 0.0;
        for (i, (slot, x)) in missing.iter_mut().zip(f.x).enumerate() {
            match x {
                None => {
                    *slot = if self.n[i] > 0 {
                        self.var[i]
                    } else {
                        UNSEEN_VARIANCE
                    };
                }
                Some(x) if self.n[i] > 0 => {
                    outside +=
                        ((self.min[i] - x).max(x - self.max[i]) - EXTRAPOLATION_MARGIN).max(0.0);
                }
                Some(_) => {}
            }
        }
        Context {
            phi: self.phi(f),
            missing,
            outside,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_are_centred_logarithms() {
        let mut inputs = Inputs::batch(10_000, 640_000);
        inputs.resources.workers = Some(4);
        let f = Features::of(&inputs);
        assert_eq!(f.x[ROWS], Some(0.0));
        assert_eq!(f.x[BYTES_PER_ROW], Some(0.0));
        assert_eq!(f.x[2], None);
        assert_eq!(f.x[DISTINCT], None);
        assert!((f.x[4].expect("workers") - 4f64.ln()).abs() < 1e-12);
        let zero = Features::of(&Inputs::batch(0, 0));
        assert_eq!(zero.x[ROWS], Some((1.0f64 / 10_000.0).ln()));
    }

    #[test]
    fn missing_and_outside() {
        let mut stats = FeatureStats::default();
        let empty = Features {
            x: [None; FEATURES],
        };
        let ctx = stats.context(&empty);
        assert_eq!(ctx.missing, [UNSEEN_VARIANCE; FEATURES]);
        assert_eq!(ctx.phi, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        for x in [-1.0, 1.0] {
            stats.push(
                &Features {
                    x: [Some(x), None, None, None, None],
                },
                1.0,
            );
        }
        assert!(stats.seen(ROWS) && !stats.seen(1));
        let (mean, var) = stats.moments(ROWS);
        assert!(mean.abs() < 1e-12 && var > 0.0);
        let inside = stats.context(&Features {
            x: [Some(0.5), None, None, None, None],
        });
        assert_eq!(inside.outside, 0.0);
        let near = stats.context(&Features {
            x: [Some(1.05), None, None, None, None],
        });
        assert_eq!(near.outside, 0.0, "within the margin");
        let beyond = stats.context(&Features {
            x: [Some(3.0), None, None, None, None],
        });
        assert!((beyond.outside - (2.0 - EXTRAPOLATION_MARGIN)).abs() < 1e-12);
        let missing = stats.context(&empty);
        assert_eq!(missing.missing[ROWS], var);
        assert_eq!(missing.phi[1], mean);
    }
}
