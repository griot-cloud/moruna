//! A kind's scaling curve (SDD 24 f.8): what running another unit of it at once gives, from its
//! measured speed-ups at 2 and 4 workers.

/// The least an extra concurrent unit of a kind must add, in units' worth of throughput, for the
/// simulator to run it: below this the kind is saturated and another unit only takes a worker.
pub const MIN_MARGINAL_GAIN: f64 = 0.05;

/// The throughput of `k` concurrent units of a kind, in single units' worth: `s(1) = 1`, through
/// `s(2)` and `s(4)` by straight lines, and past 4 by Amdahl's law through `s(4)`, so the curve
/// is continuous, never falls and never passes `k`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scaling {
    s2: f64,
    s4: f64,
    /// Amdahl's parallel share fitted through `s(4)`, for `k > 4`.
    parallel: f64,
}

impl Scaling {
    /// Units that do not slow each other: `s(k) = k`. What a stateless kind with no measured
    /// curve gets, since its morsels are independent.
    pub fn linear() -> Scaling {
        Scaling {
            s2: 2.0,
            s4: 4.0,
            parallel: 1.0,
        }
    }

    /// A kind that gains nothing from a second unit at once: `s(k) = 1`.
    pub fn serial() -> Scaling {
        Scaling {
            s2: 1.0,
            s4: 1.0,
            parallel: 0.0,
        }
    }

    /// The curve through the measured speed-ups (`Kernel::speedup_2`, `Kernel::speedup_4`). With
    /// neither, [`Scaling::linear`]; with one, Amdahl's law through it gives the other. Each is
    /// held to what is possible: `1 <= s(2) <= 2` and `s(2) <= s(4) <= 4`; a figure that is not
    /// finite is taken as missing.
    pub fn from_speedups(speedup_2: Option<f64>, speedup_4: Option<f64>) -> Scaling {
        let ok = |v: Option<f64>| v.filter(|v| v.is_finite());
        let (s2, s4) = match (ok(speedup_2), ok(speedup_4)) {
            (None, None) => return Scaling::linear(),
            (Some(s2), None) => {
                let s2 = s2.clamp(1.0, 2.0);
                (s2, amdahl(share(s2, 2.0), 4.0))
            }
            (None, Some(s4)) => {
                let s4 = s4.clamp(1.0, 4.0);
                (amdahl(share(s4, 4.0), 2.0), s4)
            }
            (Some(s2), Some(s4)) => {
                let s2 = s2.clamp(1.0, 2.0);
                (s2, s4.clamp(s2, 4.0))
            }
        };
        Scaling {
            s2,
            s4,
            parallel: share(s4, 4.0),
        }
    }

    /// The throughput of `k` concurrent units, in single units' worth (`s(0) = 0`).
    pub fn speedup(&self, k: u32) -> f64 {
        match k {
            0 => 0.0,
            1 => 1.0,
            2 => self.s2,
            3 => (self.s2 + self.s4) / 2.0,
            4 => self.s4,
            _ => amdahl(self.parallel, f64::from(k)).max(self.s4),
        }
    }

    /// How fast each of `k` concurrent units progresses, as a share of its speed alone:
    /// `s(k) / k`.
    pub fn each(&self, k: u32) -> f64 {
        if k == 0 {
            1.0
        } else {
            self.speedup(k) / f64::from(k)
        }
    }

    /// The most units of the kind worth running at once, out of `workers`: while another adds at
    /// least [`MIN_MARGINAL_GAIN`].
    pub fn cap(&self, workers: u32) -> u32 {
        let mut k = 1;
        while k < workers && self.speedup(k + 1) - self.speedup(k) >= MIN_MARGINAL_GAIN {
            k += 1;
        }
        k
    }
}

/// Amdahl's parallel share that gives speed-up `s` at `k` workers, held to [0, 1].
fn share(s: f64, k: f64) -> f64 {
    if s <= 1.0 {
        return 0.0;
    }
    ((1.0 - 1.0 / s) * k / (k - 1.0)).clamp(0.0, 1.0)
}

/// Amdahl's speed-up at `k` workers for parallel share `p`.
fn amdahl(p: f64, k: f64) -> f64 {
    1.0 / ((1.0 - p) + p / k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_from_speedups() {
        let linear = Scaling::from_speedups(None, None);
        assert_eq!(linear, Scaling::linear());
        for k in 0..12 {
            assert!((linear.speedup(k) - f64::from(k)).abs() < 1e-12);
        }
        assert_eq!(linear.cap(8), 8);
        assert_eq!(linear.each(0), 1.0);

        let serial = Scaling::serial();
        assert_eq!(serial.cap(8), 1);
        assert_eq!(serial.speedup(6), 1.0);
        assert_eq!(Scaling::from_speedups(Some(1.0), Some(1.0)).cap(8), 1);

        let both = Scaling::from_speedups(Some(1.8), Some(3.0));
        assert_eq!(both.speedup(2), 1.8);
        assert!((both.speedup(3) - 2.4).abs() < 1e-12);
        assert_eq!(both.speedup(4), 3.0);
        let mut last = 3.0;
        for k in 5..64 {
            let s = both.speedup(k);
            assert!(s >= last && s <= f64::from(k), "k {k}: {s}");
            last = s;
        }
        // Amdahl through s(4) = 3: p = 8/9, saturating at 9.
        assert!(both.speedup(1_000) < 9.0);
        assert!(both.cap(64) > 4 && both.cap(64) < 64);
        assert!((both.each(2) - 0.9).abs() < 1e-12);

        let two = Scaling::from_speedups(Some(1.5), None);
        // p = 2/3: s(4) = 1 / (1/3 + 1/6) = 2.
        assert!((two.speedup(4) - 2.0).abs() < 1e-12);
        let four = Scaling::from_speedups(None, Some(2.0));
        // p = 2/3 again: s(2) = 1.5.
        assert!((four.speedup(2) - 1.5).abs() < 1e-12);
        let wild = Scaling::from_speedups(Some(9.0), Some(f64::NAN));
        assert_eq!(wild.speedup(2), 2.0);
        assert_eq!(Scaling::from_speedups(Some(0.2), Some(0.1)).speedup(4), 1.0);
    }
}
