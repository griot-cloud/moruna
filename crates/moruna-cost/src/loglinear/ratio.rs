//! A value-dependent ratio kept as a ratio (section 5.3): selectivity and output bytes per row
//! are not regressed on size; their median and 95th percentile over the recent units are.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::exact;

/// Units a ratio's percentiles are taken over. The window is its forgetting.
const RATIO_WINDOW: usize = 128;

/// One ratio's recent values and their percentiles, computed when a value arrives so a
/// prediction reads two numbers.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Ratio {
    #[serde(with = "exact::list")]
    values: VecDeque<f64>,
    #[serde(with = "exact::float")]
    p50: f64,
    #[serde(with = "exact::float")]
    p95: f64,
    n: u64,
}

impl Ratio {
    /// Add one value; a value that is not finite and non-negative is not a ratio and is not kept.
    pub fn push(&mut self, value: f64) {
        if !(value.is_finite() && value >= 0.0) {
            return;
        }
        if self.values.len() == RATIO_WINDOW {
            self.values.pop_front();
        }
        self.values.push_back(value);
        self.n += 1;
        let mut sorted: Vec<f64> = self.values.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        self.p50 = quantile(&sorted, 0.5);
        self.p95 = quantile(&sorted, 0.95);
    }

    /// Values seen.
    pub fn n(&self) -> u64 {
        self.n
    }

    /// The median and 95th percentile.
    pub fn percentiles(&self) -> (f64, f64) {
        (self.p50, self.p95)
    }
}

/// The `q` quantile of sorted values, interpolated between ranks.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    match sorted.len() {
        0 => 0.0,
        1 => sorted[0],
        n => {
            let at = q * (n - 1) as f64;
            let lo = at.floor() as usize;
            let hi = (lo + 1).min(n - 1);
            sorted[lo] + (sorted[hi] - sorted[lo]) * (at - lo as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_follow_the_window() {
        let mut r = Ratio::default();
        assert_eq!(r.percentiles(), (0.0, 0.0));
        r.push(f64::NAN);
        r.push(-1.0);
        assert_eq!(r.n(), 0);
        r.push(2.0);
        assert_eq!(r.percentiles(), (2.0, 2.0));
        for v in 1..=100 {
            r.push(v as f64);
        }
        let (p50, p95) = r.percentiles();
        assert!(
            (p50 - 50.0).abs() < 1.0 && (p95 - 95.0).abs() < 1.5,
            "{p50} {p95}"
        );
        for _ in 0..RATIO_WINDOW {
            r.push(0.5);
        }
        assert_eq!(r.percentiles(), (0.5, 0.5), "the window forgets");
        assert_eq!(r.n(), 101 + RATIO_WINDOW as u64);
    }
}
