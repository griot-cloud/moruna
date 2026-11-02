//! Drawing from an estimate (SDD 24 f.6): a seeded generator and the standard normal's quantile,
//! so the simulator's Monte Carlo runs draw the same numbers on every machine and need no crate.

/// The standard normal's 95th percentile: an estimate's p95 is its median times
/// `exp(Z95 · sigma)` in log space.
pub const Z95: f64 = 1.644_853_626_951_472;

/// SplitMix64: small, fast, and the same sequence for a seed on every machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rng(u64);

impl Rng {
    /// A generator seeded with `seed`.
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in the open interval (0, 1), so a quantile of it is always finite.
    pub fn open01(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// A standard normal, by the inverse of its distribution function.
    pub fn normal(&mut self) -> f64 {
        normal_quantile(self.open01())
    }
}

/// The standard normal's quantile at `p` in (0, 1): Acklam's rational approximation, relative
/// error below 1.2e-9. `p` at or outside the ends gives an infinity of its side.
pub fn normal_quantile(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    const LOW: f64 = 0.024_25;
    if p.is_nan() {
        return f64::NAN;
    }
    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    if p < LOW {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - LOW {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_quantile_inverts_the_normal() {
        assert!(normal_quantile(0.5).abs() < 1e-9);
        assert!((normal_quantile(0.95) - Z95).abs() < 1e-7);
        assert!((normal_quantile(0.05) + Z95).abs() < 1e-7);
        assert!((normal_quantile(0.001) + 3.090_232_306).abs() < 1e-6);
        assert!((normal_quantile(0.999) - 3.090_232_306).abs() < 1e-6);
        assert_eq!(normal_quantile(0.0), f64::NEG_INFINITY);
        assert_eq!(normal_quantile(1.0), f64::INFINITY);
        assert!(normal_quantile(f64::NAN).is_nan());
    }

    #[test]
    fn the_generator_is_seeded_and_open() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        let mut sum = 0.0;
        for _ in 0..10_000 {
            let u = a.open01();
            assert_eq!(u, b.open01());
            assert!(u > 0.0 && u < 1.0);
            let z = a.normal();
            assert_eq!(z, b.normal());
            sum += z;
        }
        assert!((sum / 10_000.0).abs() < 0.05);
        assert_ne!(Rng::new(8).next_u64(), Rng::new(7).next_u64());
    }
}
