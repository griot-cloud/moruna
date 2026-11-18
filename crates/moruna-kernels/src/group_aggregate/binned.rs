//! A binned reproducible sum of Float64 values (Demmel and Nguyen; ReproBLAS's "binned"
//! type, fold 3, bins 40 bits apart), kept to measure against [`super::exact::ExactSum`] for
//! the PM's choice of float summation (4G.4, 2026-11-18). It is not part of `@1`: no job can
//! ask for it, and it has no partial form.
//!
//! Bins are fixed binades `e_b = 1020 − 40·b`. A bin's primary value starts at `1.5 × 2^e_b`, so
//! adding a value to it rounds the value to the bin's ulp, `2^(e_b − 52)`. A value is deposited
//! into the three bins from the one its magnitude selects downwards, its lowest bit set first so
//! that no deposit is a tie: each bin then receives the same amount from each value whatever the
//! order, and what falls below the third bin is dropped the same way. Carries keep each primary
//! inside its binade. Values above `2^1006` are not supported (they count as overflow).

/// Bins kept per sum.
const FOLD: usize = 3;
/// Bits between bins.
const WIDTH: i32 = 40;
/// The top bin's binade.
const TOP_E: i32 = 1020;
/// The number of bins over the range.
const BINS: i32 = 52;
/// Deposits between carry steps: each moves a primary by less than `2^(e − 13)`, so 2^10 of
/// them move it less than a quarter of its binade.
const ENDURANCE: u16 = 1 << 10;

fn pow2(e: i32) -> f64 {
    if e >= -1022 {
        f64::from_bits(((e + 1023) as u64) << 52)
    } else {
        f64::from_bits(1u64 << (e + 1074).max(0))
    }
}

fn bin_e(b: i32) -> i32 {
    TOP_E - WIDTH * b
}

/// One binned sum.
#[derive(Clone, Debug, Default)]
pub struct BinnedSum {
    /// The top bin's index; `-1` while empty.
    top: i32,
    p: [f64; FOLD],
    c: [f64; FOLD],
    deposits: u16,
    flags: u8,
}

impl BinnedSum {
    /// The empty sum.
    pub fn new() -> BinnedSum {
        BinnedSum {
            top: -1,
            ..Default::default()
        }
    }

    /// The top bin a value of binade `ex` needs: the deepest whose bound `2^(e − 13)` is above it.
    fn index_of(ex: i32) -> i32 {
        ((TOP_E - ex - 14).div_euclid(WIDTH)).clamp(0, BINS - FOLD as i32)
    }

    fn reset_bins(&mut self, top: i32) {
        let old_top = self.top;
        let (old_p, old_c) = (self.p, self.c);
        for k in 0..FOLD {
            let b = top + k as i32;
            let from = b - old_top;
            if old_top >= 0 && (0..FOLD as i32).contains(&from) {
                self.p[k] = old_p[from as usize];
                self.c[k] = old_c[from as usize];
            } else {
                self.p[k] = 1.5 * pow2(bin_e(b));
                self.c[k] = 0.0;
            }
        }
        self.top = top;
    }

    /// Add one value.
    #[inline]
    pub fn add(&mut self, x: f64) {
        if !x.is_finite() {
            self.flags |= 1;
            return;
        }
        if x == 0.0 {
            return;
        }
        let ex = ((x.to_bits() >> 52) & 0x7ff) as i32 - 1023;
        if ex > TOP_E - 14 {
            self.flags |= 1;
            return;
        }
        let want = Self::index_of(ex);
        if self.top < 0 || want < self.top {
            self.reset_bins(want);
        }
        let mut r = x;
        for k in 0..FOLD {
            let m = self.p[k];
            let q = m + f64::from_bits(r.to_bits() | 1);
            self.p[k] = q;
            r -= q - m;
        }
        self.deposits += 1;
        if self.deposits >= ENDURANCE {
            self.carry(0.25);
        }
    }

    /// Move quarters of each binade between primary and carry, keeping the primary within
    /// `±width` of `1.5 × 2^e`.
    fn carry(&mut self, width: f64) {
        for k in 0..FOLD {
            let unit = pow2(bin_e(self.top + k as i32));
            let quarter = 0.25 * unit;
            while self.p[k] >= (1.5 + width) * unit {
                self.p[k] -= quarter;
                self.c[k] += 1.0;
            }
            while self.p[k] < (1.5 - width) * unit {
                self.p[k] += quarter;
                self.c[k] -= 1.0;
            }
        }
        self.deposits = 0;
    }

    /// The sum: each bin's total (canonical after the narrow carry step) added in bin order.
    pub fn value(&self) -> f64 {
        if self.flags != 0 {
            return f64::NAN;
        }
        if self.top < 0 {
            return 0.0;
        }
        let mut s = self.clone();
        s.carry(0.125);
        let mut r = 0.0;
        for k in 0..FOLD {
            let unit = pow2(bin_e(s.top + k as i32));
            r += s.c[k] * 0.25 * unit;
            r += s.p[k] - 1.5 * unit;
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binned_sums_do_not_depend_on_order() {
        let mut seed = 5u64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed
        };
        let values: Vec<f64> = (0..5000)
            .map(|i| {
                let v = (next() % 1_000_000) as f64 / 7.0;
                if i % 3 == 0 { -v * 1e-6 } else { v }
            })
            .collect();
        let sum = |vs: &[f64]| {
            let mut s = BinnedSum::new();
            for &v in vs {
                s.add(v);
            }
            s.value()
        };
        let reference = sum(&values);
        let mut shuffled = values.clone();
        shuffled.reverse();
        assert_eq!(sum(&shuffled).to_bits(), reference.to_bits());
        shuffled.sort_by(|a, b| a.total_cmp(b));
        assert_eq!(sum(&shuffled).to_bits(), reference.to_bits());
        let plain: f64 = values.iter().sum();
        assert!((reference - plain).abs() <= plain.abs() * 1e-12);
        assert_eq!(sum(&[]), 0.0);
        assert!(sum(&[f64::INFINITY]).is_nan());
        assert!(sum(&[1e307]).is_nan());
        assert_eq!(sum(&[1.0, 2.0, 1e10, -1e10]), 3.0);
    }
}
