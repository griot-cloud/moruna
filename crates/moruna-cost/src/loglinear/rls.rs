//! One target of one key: recursive least squares on logarithms with forgetting, a fixed
//! overhead fitted at the smallest sizes, and the spread (SDD 24 f.1 to f.3).
//!
//! The model is `y = c + exp(θ·φ)`: `c` is the fixed per-unit overhead (start-up and per-batch
//! time, a baseline allocation), and the line in logarithms carries what grows with size, so
//! one straight line covers linear, n log n and power-law growth.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::features::{COEFFICIENTS, Context, ROWS};
use crate::exact;

/// The prior variance of the intercept, in squared log units: the reference unit's cost is
/// known to within a factor of e^10 before anything is measured.
const PRIOR_INTERCEPT_VARIANCE: f64 = 100.0;
/// The prior variance of each slope, in RLS units (it is scaled by the noise like the rest of
/// the covariance). One unit's worth: a handful of observations over a decade of sizes outweighs
/// it.
const PRIOR_SLOPE_VARIANCE: f64 = 1.0;
/// The prior of the noise: a standard deviation of 0.3 in log units (about 35%), worth one
/// observation, so the first estimates are wide and the measured noise takes over.
const NOISE_PRIOR_SD: f64 = 0.3;
const NOISE_PRIOR_WEIGHT: f64 = 1.0;
/// Units kept raw for choosing the overhead: enough to span the sizes a run has seen recently.
const WINDOW: usize = 32;
/// The overhead is chosen again every this many units.
const RESELECT_EVERY: u32 = 8;
/// The overhead is chosen only when the window spans at least this ratio of rows (e^ln), since
/// a fixed term and a growing one cannot be told apart at one size.
const OVERHEAD_SPAN: f64 = 1.386_294_361_119_890_6; // ln 4
/// The smallest sizes: within this ratio (as a logarithm) of the window's smallest rows.
const SMALLEST_BAND: f64 = 0.405_465_108_108_164_4; // ln 1.5
/// Overheads tried, as shares of the cost at the smallest sizes.
const OVERHEAD_SHARES: [f64; 7] = [0.0, 0.25, 0.5, 0.65, 0.8, 0.9, 0.95];
/// A new overhead replaces the old only when it explains the window at least this much better.
const OVERHEAD_SWITCH: f64 = 0.9;
/// A unit whose cost above the overhead is less than this share of its cost is the overhead's
/// evidence, not the line's: in it, `y - c` is the noise of `c`.
const OVERHEAD_DOMINATED: f64 = 0.25;
/// The spread added per log unit a prediction lies outside the measured range: a slope wrong
/// by a quarter is plausible past the data.
const EXTRAPOLATION_SD: f64 = 0.25;
/// The widest p95 over p50, as a logarithm (e^9.2, about 10,000 times), so a guess far from
/// anything measured is wide and finite.
const MAX_LOG_SPREAD: f64 = 9.2;
/// The largest logarithm a prediction takes, so it never overflows.
const MAX_LOG: f64 = 690.0;

/// One unit kept for choosing the overhead.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Point {
    #[serde(with = "exact::array")]
    phi: [f64; COEFFICIENTS],
    #[serde(with = "exact::float")]
    y: f64,
}

/// The fit of one target.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TargetFit {
    #[serde(with = "exact::array")]
    prior: [f64; COEFFICIENTS],
    #[serde(with = "exact::array")]
    theta: [f64; COEFFICIENTS],
    #[serde(with = "exact::array")]
    p: [f64; COEFFICIENTS * COEFFICIENTS],
    #[serde(with = "exact::float")]
    overhead: f64,
    /// Forgetting-weighted sums of the line's standardised residuals and their weight.
    #[serde(with = "exact::float")]
    line_ss: f64,
    #[serde(with = "exact::float")]
    line_w: f64,
    /// The same for the whole prediction's residual, in `log(y + floor)`.
    #[serde(with = "exact::float")]
    full_ss: f64,
    #[serde(with = "exact::float")]
    full_w: f64,
    n: u64,
    since_choice: u32,
    window: VecDeque<Point>,
}

fn p0(i: usize) -> f64 {
    if i == 0 {
        PRIOR_INTERCEPT_VARIANCE
    } else {
        PRIOR_SLOPE_VARIANCE
    }
}

fn dot(a: &[f64; COEFFICIENTS], b: &[f64; COEFFICIENTS]) -> f64 {
    let mut s = 0.0;
    for i in 0..COEFFICIENTS {
        s += a[i] * b[i];
    }
    s
}

/// The one-sided 95% quantile of Student's t with `dof` degrees of freedom: wider than the
/// normal's 1.645 while the noise rests on few observations.
pub(crate) fn t95(dof: f64) -> f64 {
    const TABLE: [f64; 30] = [
        6.314, 2.920, 2.353, 2.132, 2.015, 1.943, 1.895, 1.860, 1.833, 1.812, 1.796, 1.782, 1.771,
        1.761, 1.753, 1.746, 1.740, 1.734, 1.729, 1.725, 1.721, 1.717, 1.714, 1.711, 1.708, 1.706,
        1.703, 1.701, 1.699, 1.697,
    ];
    if dof < 30.0 {
        TABLE[(dof.max(1.0) as usize).clamp(1, 30) - 1]
    } else {
        1.645 + (1.697 - 1.645) * 30.0 / dof
    }
}

impl TargetFit {
    /// A fit that knows only its prior coefficients.
    pub fn new(prior: [f64; COEFFICIENTS]) -> TargetFit {
        let mut fit = TargetFit {
            prior,
            theta: prior,
            p: [0.0; COEFFICIENTS * COEFFICIENTS],
            overhead: 0.0,
            line_ss: 0.0,
            line_w: 0.0,
            full_ss: 0.0,
            full_w: 0.0,
            n: 0,
            since_choice: 0,
            window: VecDeque::with_capacity(WINDOW),
        };
        fit.reset_line();
        fit
    }

    fn reset_line(&mut self) {
        self.theta = self.prior;
        self.p = [0.0; COEFFICIENTS * COEFFICIENTS];
        for i in 0..COEFFICIENTS {
            self.p[i * COEFFICIENTS + i] = p0(i);
        }
    }

    /// Units observed.
    pub fn n(&self) -> u64 {
        self.n
    }

    /// The fixed overhead.
    pub fn overhead(&self) -> f64 {
        self.overhead
    }

    /// `P·phi`.
    fn p_times(&self, phi: &[f64; COEFFICIENTS]) -> [f64; COEFFICIENTS] {
        let mut out = [0.0; COEFFICIENTS];
        for (slot, row) in out.iter_mut().zip(self.p.as_chunks::<COEFFICIENTS>().0) {
            *slot = dot(row, phi);
        }
        out
    }

    /// The median prediction at `phi`.
    fn point(&self, phi: &[f64; COEFFICIENTS], floor: f64) -> f64 {
        self.overhead + (dot(&self.theta, phi).min(MAX_LOG).exp() - floor).max(0.0)
    }

    /// Learn one unit.
    pub fn observe(&mut self, phi: [f64; COEFFICIENTS], y: f64, floor: f64, lambda: f64) {
        if self.n > 0 {
            let r = (y + floor).ln() - (self.point(&phi, floor) + floor).ln();
            self.full_ss = lambda * self.full_ss + r * r;
            self.full_w = lambda * self.full_w + 1.0;
        }
        self.n += 1;
        if self.window.len() == WINDOW {
            self.window.pop_front();
        }
        self.window.push_back(Point { phi, y });
        self.regress(&phi, y, floor, lambda, true);
        self.since_choice += 1;
        if self.since_choice >= RESELECT_EVERY {
            self.choose_overhead(floor, lambda);
        }
    }

    /// One recursive least squares step on `log(y - c)`, unless the unit is the overhead's.
    fn regress(&mut self, phi: &[f64; COEFFICIENTS], y: f64, floor: f64, lambda: f64, track: bool) {
        let grown = y - self.overhead;
        if self.overhead > 0.0 && grown < OVERHEAD_DOMINATED * y {
            return;
        }
        let z = (grown.max(0.0) + floor).ln();
        let pphi = self.p_times(phi);
        let quad = dot(phi, &pphi);
        let e = z - dot(&self.theta, phi);
        if track {
            self.line_ss = lambda * self.line_ss + e * e / (1.0 + quad);
            self.line_w = lambda * self.line_w + 1.0;
        }
        let denom = lambda + quad;
        for (t, pp) in self.theta.iter_mut().zip(pphi) {
            *t += pp / denom * e;
        }
        for i in 0..COEFFICIENTS {
            for j in 0..COEFFICIENTS {
                let at = i * COEFFICIENTS + j;
                self.p[at] = (self.p[at] - pphi[i] * pphi[j] / denom) / lambda;
            }
        }
        // Forgetting divides by lambda in every direction, so a direction no unit excites (a
        // feature that never varies) would grow without bound. Each variance is held at its
        // prior: an unexcited direction forgets back to what was known before any data.
        for i in 0..COEFFICIENTS {
            let d = self.p[i * COEFFICIENTS + i];
            if d > p0(i) {
                let s = (p0(i) / d).sqrt();
                for j in 0..COEFFICIENTS {
                    self.p[i * COEFFICIENTS + j] *= s;
                    self.p[j * COEFFICIENTS + i] *= s;
                }
            }
        }
    }

    /// Choose the overhead the window's units at the smallest sizes say, and refit the line
    /// over the window when it changes (f.2).
    pub fn choose_overhead(&mut self, floor: f64, lambda: f64) {
        self.since_choice = 0;
        if self.window.len() < 6 {
            return;
        }
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for p in &self.window {
            lo = lo.min(p.phi[ROWS + 1]);
            hi = hi.max(p.phi[ROWS + 1]);
        }
        if hi - lo < OVERHEAD_SPAN {
            return;
        }
        let smallest = self
            .window
            .iter()
            .filter(|p| p.phi[ROWS + 1] <= lo + SMALLEST_BAND)
            .map(|p| p.y)
            .fold(f64::INFINITY, f64::min);
        if !(smallest.is_finite() && smallest > 0.0) {
            return;
        }
        let current = self.score(self.overhead, floor);
        let mut best = (self.overhead, current);
        for share in OVERHEAD_SHARES {
            let c = share * smallest;
            let s = self.score(c, floor);
            if s < best.1 {
                best = (c, s);
            }
        }
        if best.0 != self.overhead && best.1 < OVERHEAD_SWITCH * current {
            self.overhead = best.0;
            self.refit(floor, lambda);
        }
    }

    /// How badly `c + exp(a + b·log(rows))`, fitted over the window, explains it: the sum of
    /// squared errors in `log(y + floor)`. Infinite when the line cannot be fitted.
    fn score(&self, c: f64, floor: f64) -> f64 {
        let (mut n, mut sx, mut sz, mut sxx, mut sxz) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for p in &self.window {
            if c > 0.0 && p.y - c < OVERHEAD_DOMINATED * p.y {
                continue;
            }
            let x = p.phi[ROWS + 1];
            let z = ((p.y - c).max(0.0) + floor).ln();
            n += 1.0;
            sx += x;
            sz += z;
            sxx += x * x;
            sxz += x * z;
        }
        let det = n * sxx - sx * sx;
        if n < 3.0 || det <= 1e-12 * n * n {
            return f64::INFINITY;
        }
        let b = (n * sxz - sx * sz) / det;
        let a = (sz - b * sx) / n;
        self.window
            .iter()
            .map(|p| {
                let fit = c + ((a + b * p.phi[ROWS + 1]).min(MAX_LOG).exp() - floor).max(0.0);
                let r = (p.y + floor).ln() - (fit + floor).ln();
                r * r
            })
            .sum()
    }

    /// The line again from its prior over the window, at the overhead now chosen, and the noise
    /// from the window's residuals against it.
    fn refit(&mut self, floor: f64, lambda: f64) {
        self.reset_line();
        let window: Vec<Point> = self.window.iter().cloned().collect();
        for p in &window {
            self.regress(&p.phi, p.y, floor, lambda, false);
        }
        let (mut line_ss, mut line_w, mut full_ss, mut full_w) = (0.0, 0.0, 0.0, 0.0);
        let mut weight = 1.0;
        for p in window.iter().rev() {
            let r = (p.y + floor).ln() - (self.point(&p.phi, floor) + floor).ln();
            full_ss += weight * r * r;
            full_w += weight;
            let grown = p.y - self.overhead;
            if !(self.overhead > 0.0 && grown < OVERHEAD_DOMINATED * p.y) {
                let e = (grown.max(0.0) + floor).ln() - dot(&self.theta, &p.phi);
                line_ss += weight * e * e;
                line_w += weight;
            }
            weight *= lambda;
        }
        // Residuals against a line fitted to them understate the noise; two coefficients are
        // what the window can pin down, so correct for two.
        let correct = |ss: f64, w: f64| if w > 3.0 { ss * w / (w - 2.0) } else { ss };
        self.line_ss = correct(line_ss, line_w);
        self.line_w = line_w;
        self.full_ss = correct(full_ss, full_w);
        self.full_w = full_w;
    }

    /// The median, the 95th percentile, and whether it extrapolates.
    pub fn predict(&self, ctx: &Context, floor: f64) -> (f64, f64, bool) {
        let mu = dot(&self.theta, &ctx.phi).min(MAX_LOG);
        let grown = (mu.exp() - floor).max(0.0);
        let p50 = self.overhead + grown;
        let share = if p50 > 0.0 { grown / p50 } else { 1.0 };
        let prior = NOISE_PRIOR_WEIGHT * NOISE_PRIOR_SD * NOISE_PRIOR_SD;
        let line_var = (self.line_ss + prior) / (self.line_w + NOISE_PRIOR_WEIGHT);
        let full_var = (self.full_ss + prior) / (self.full_w + NOISE_PRIOR_WEIGHT);
        let quad = dot(&ctx.phi, &self.p_times(&ctx.phi));
        let mut missing = 0.0;
        for (t, m) in self.theta[1..].iter().zip(ctx.missing) {
            missing += t * t * m;
        }
        let outside = EXTRAPOLATION_SD * ctx.outside;
        let var =
            full_var + share * share * (line_var * quad.max(0.0) + missing + outside * outside);
        let t = t95(self.full_w + NOISE_PRIOR_WEIGHT - 1.0);
        let spread = (t * var.sqrt()).min(MAX_LOG_SPREAD);
        let p95 = ((p50 + floor) * spread.exp() - floor).max(p50);
        (p50, p95, ctx.outside > 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(x: f64) -> Context {
        Context {
            phi: [1.0, x, 0.0, 0.0, 0.0, 0.0],
            missing: [0.0; super::super::features::FEATURES],
            outside: 0.0,
        }
    }

    #[test]
    fn t_quantiles() {
        assert_eq!(t95(0.0), 6.314);
        assert_eq!(t95(1.0), 6.314);
        assert_eq!(t95(29.5), 1.699);
        assert!((t95(30.0) - 1.697).abs() < 1e-12);
        assert!(t95(1e9) - 1.645 < 1e-6);
    }

    #[test]
    fn a_power_law_is_learned_exactly() {
        let mut fit = TargetFit::new([0.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
        for k in 0..200 {
            let x = (k % 7) as f64 - 3.0;
            fit.observe(
                [1.0, x, 0.0, 0.0, 0.0, 0.0],
                (2.0 + 1.5 * x).exp(),
                1e-9,
                0.98,
            );
        }
        let (p50, p95, outside) = fit.predict(&ctx(1.0), 1e-9);
        assert!((p50 / 3.5f64.exp() - 1.0).abs() < 1e-3, "{p50}");
        assert!(p95 >= p50 && !outside);
        assert_eq!(fit.overhead(), 0.0, "a pure power law has no overhead");
        assert_eq!(fit.n(), 200);
    }

    #[test]
    fn an_overhead_is_found_at_the_smallest_sizes() {
        let mut fit = TargetFit::new([0.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
        for k in 0..64 {
            let x = (k % 8) as f64 - 4.0;
            fit.observe([1.0, x, 0.0, 0.0, 0.0, 0.0], 5.0 + x.exp(), 1e-9, 0.98);
        }
        assert!(fit.overhead() > 2.0, "overhead {}", fit.overhead());
        for x in [-4.0, 0.0, 3.0] {
            let (p50, _, _) = fit.predict(&ctx(x), 1e-9);
            let truth = 5.0 + f64::exp(x);
            assert!((p50 / truth - 1.0).abs() < 0.1, "x {x}: {p50} vs {truth}");
        }
    }

    #[test]
    fn a_line_that_cannot_be_fitted_scores_infinite() {
        let mut fit = TargetFit::new([0.0; COEFFICIENTS]);
        for _ in 0..4 {
            fit.observe([1.0, 0.0, 0.0, 0.0, 0.0, 0.0], 1.0, 1e-9, 1.0);
        }
        assert_eq!(fit.score(0.0, 1e-9), f64::INFINITY);
        fit.choose_overhead(1e-9, 1.0);
        assert_eq!(fit.overhead(), 0.0);
    }
}
