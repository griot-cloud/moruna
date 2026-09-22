//! A deterministic, portable pseudo random number generator.
//!
//! `rand` is not in the preamble's dependency table (6.2), so the generator
//! carries its own: SplitMix64, which is a few lines, has no state beyond a
//! `u64`, and is exactly reproducible. Only addition, multiplication, shifts and
//! division appear here, so a value depends on the seed alone and not on the
//! host's libm: that is what makes a dataset byte identical across machines as
//! well as across runs.

/// A SplitMix64 stream.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

/// Mix a label into a seed so that each column, tensor and dataset draws from
/// its own stream. Two datasets generated from the same seed therefore never
/// share values, and adding a column does not shift the values of the others.
pub fn substream_seed(seed: u64, label: &str) -> u64 {
    // FNV-1a over the label, then one SplitMix64 mixing round with the seed.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in label.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut mixed = seed ^ hash;
    mixed = mixed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = mixed;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Rng {
    /// A stream from a raw seed.
    pub fn new(seed: u64) -> Self {
        Rng { state: seed }
    }

    /// The stream a named part of a dataset draws from.
    pub fn substream(seed: u64, label: &str) -> Self {
        Rng::new(substream_seed(seed, label))
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// The next 32 bits.
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// A `f64` in `[0, 1)`, from the top 53 bits (exact in binary floating
    /// point, so the same on every IEEE 754 host).
    pub fn next_f64(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) * (1.0 / 9_007_199_254_740_992.0)
    }

    /// `true` with probability `p`. `p <= 0` is never, `p >= 1` is always.
    pub fn bernoulli(&mut self, p: f64) -> bool {
        if p <= 0.0 {
            return false;
        }
        if p >= 1.0 {
            return true;
        }
        self.next_f64() < p
    }

    /// A `usize` in `[0, n)`; `0` when `n` is zero.
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % (n as u64)) as usize
    }

    /// A standard normal draw by the Irwin Hall construction: the sum of twelve
    /// uniforms minus six has mean 0 and variance 1, and uses no transcendental
    /// function, so it is reproducible on any IEEE 754 host (Box Muller would
    /// depend on the host's `ln`, `sqrt` and `cos`).
    pub fn normal(&mut self) -> f64 {
        let mut sum = 0.0_f64;
        for _ in 0..12 {
            sum += self.next_f64();
        }
        sum - 6.0
    }

    /// A normal draw with the given mean and standard deviation, rounded to a
    /// length of at least `min`.
    pub fn normal_len(&mut self, mean: f64, stddev: f64, min: usize) -> usize {
        let value = mean + stddev * self.normal();
        if value < min as f64 {
            min
        } else {
            // A length cannot exceed what a f64 can hold exactly here: the
            // callers clamp the mean and the deviation well below 2^53.
            value.round() as usize
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_stream() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(8);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn substreams_are_per_label_and_stable() {
        assert_eq!(substream_seed(1, "i64_0"), substream_seed(1, "i64_0"));
        assert_ne!(substream_seed(1, "i64_0"), substream_seed(1, "i64_1"));
        assert_ne!(substream_seed(1, "i64_0"), substream_seed(2, "i64_0"));
        let mut s = Rng::substream(1, "text_0");
        assert_ne!(s.next_u64(), 0);
    }

    #[test]
    fn uniforms_are_in_the_unit_interval() {
        let mut rng = Rng::new(3);
        for _ in 0..10_000 {
            let value = rng.next_f64();
            assert!((0.0..1.0).contains(&value), "{value}");
        }
    }

    #[test]
    fn bernoulli_honours_its_bounds_and_its_ratio() {
        let mut rng = Rng::new(11);
        assert!(!rng.bernoulli(0.0));
        assert!(rng.bernoulli(1.0));
        assert!(!rng.bernoulli(-0.5));
        assert!(rng.bernoulli(1.5));
        let n = 100_000;
        let hits = (0..n).filter(|_| rng.bernoulli(0.25)).count();
        let ratio = hits as f64 / n as f64;
        assert!((ratio - 0.25).abs() < 0.01, "ratio {ratio}");
    }

    #[test]
    fn below_is_in_range() {
        let mut rng = Rng::new(5);
        assert_eq!(rng.below(0), 0);
        for _ in 0..1000 {
            assert!(rng.below(7) < 7);
        }
        assert!(rng.next_u32() > 0);
    }

    #[test]
    fn the_normal_has_mean_zero_and_variance_one() {
        let mut rng = Rng::new(13);
        let n = 200_000;
        let mut sum = 0.0;
        let mut sum_sq = 0.0;
        for _ in 0..n {
            let value = rng.normal();
            sum += value;
            sum_sq += value * value;
        }
        let mean = sum / f64::from(n);
        let variance = sum_sq / f64::from(n) - mean * mean;
        assert!(mean.abs() < 0.02, "mean {mean}");
        assert!((variance - 1.0).abs() < 0.05, "variance {variance}");
    }

    #[test]
    fn normal_len_clamps_to_the_minimum() {
        let mut rng = Rng::new(17);
        for _ in 0..1000 {
            assert!(rng.normal_len(4.0, 100.0, 1) >= 1);
        }
        assert_eq!(rng.normal_len(-1000.0, 0.0, 3), 3);
    }
}
