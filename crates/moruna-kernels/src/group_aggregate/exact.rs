//! Exact summation of Float64 values, so `sum` and `mean` of floats give the same bits however
//! the rows are split across instances, batches or devices (spec `group_aggregate.md`, rounding).
//!
//! Every finite double is `m × 2^(q − 1074)` for an integer `m < 2^53` and `0 ≤ q ≤ 2045`, so a
//! sum of doubles is an integer multiple of `2^−1074`. [`ExactSum`] keeps that integer exactly
//! and rounds it once, to nearest with ties to even, when the value is read. The result is a
//! function of the multiset of values alone: the order of the additions, and how they were
//! grouped into partial sums that were then merged, cannot change it.
//!
//! The integer is held in one of two forms. The **window** is an `i128` mantissa `s` and the
//! exponent `e` of its least significant bit, so the value is `s × 2^(e − 1074)`. It holds any
//! group whose values span about seventy binades (prices, distances, durations), and costs 32
//! bytes. A value that does not fit the window **spills** it into a superaccumulator of
//! 32-bit digits over the whole exponent range (Neal's "small superaccumulator"), which holds
//! every sum of fewer than 2^62 finite doubles exactly. Infinities and NaN are kept as flags.

/// The fraction bits of a double.
const FRAC_MASK: u64 = (1 << 52) - 1;
/// The implicit leading bit of a normal double's significand.
const HIDDEN: u64 = 1 << 52;
/// The spilled form's digits, 32 bits apart: digit `k` weighs `2^(32k − 1074)`. A double's
/// lowest bit is at `q ≤ 2045` and a window's at most 127 bits higher, past the highest digit a
/// carry of 2^62 additions can reach.
const DIGITS: usize = 72;
/// Additions a spilled sum takes between two carry propagations: each adds at most `2^33` to a
/// digit, so a digit stays inside `i64`.
const CARRY_EVERY: u32 = 1 << 28;
/// The largest exponent a window may carry: a window's lowest set bit is never above the sum's
/// highest bit, which is below `2^(2046 + 64)`.
const MAX_E: i32 = 2200;

const NAN: u8 = 1;
const POS_INF: u8 = 2;
const NEG_INF: u8 = 4;

/// Tags of [`ExactSum::encode`].
const TAG_WINDOW: u8 = 0;
const TAG_DIGITS: u8 = 1;

/// One exact sum of Float64 values. `Default` is the empty sum, whose value is `+0.0`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExactSum {
    s: i128,
    e: i32,
    flags: u8,
    spill: Option<Box<Digits>>,
}

/// The spilled form: carry-save digits, and additions since the last carry propagation.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Digits {
    d: [i64; DIGITS],
    adds: u32,
}

/// Bits of `v` that can be shifted left without changing its sign or value.
#[inline]
fn room(v: i128) -> u32 {
    let lz = if v >= 0 {
        v.leading_zeros()
    } else {
        (!v).leading_zeros()
    };
    lz.saturating_sub(1)
}

impl Digits {
    fn new() -> Box<Digits> {
        Box::new(Digits {
            d: [0; DIGITS],
            adds: 0,
        })
    }

    /// Add `v × 2^(q − 1074)`.
    fn add(&mut self, v: i128, q: i32) {
        if v == 0 {
            return;
        }
        let neg = v < 0;
        let mut u = v.unsigned_abs();
        let mut q = q;
        while u != 0 {
            let chunk = (u & 0xffff_ffff) as u64;
            if chunk != 0 {
                let k = (q / 32) as usize;
                let w = (chunk as u128) << (q % 32);
                let lo = (w & 0xffff_ffff) as i64;
                let hi = (w >> 32) as i64;
                // A chunk at a digit's own boundary has no high half: the top digit, which a
                // spilled sum's merge adds at `q = 32 × (DIGITS − 1)`, has no digit above it.
                if neg {
                    self.d[k] -= lo;
                    if hi != 0 {
                        self.d[k + 1] -= hi;
                    }
                } else {
                    self.d[k] += lo;
                    if hi != 0 {
                        self.d[k + 1] += hi;
                    }
                }
            }
            u >>= 32;
            q += 32;
        }
        self.adds += 1;
        if self.adds >= CARRY_EVERY {
            self.carry();
        }
    }

    /// Propagate carries: every digit but the last in `[0, 2^32)`, the last signed.
    fn carry(&mut self) {
        for k in 0..DIGITS - 1 {
            let c = self.d[k] >> 32;
            self.d[k] -= c << 32;
            self.d[k + 1] += c;
        }
        self.adds = 0;
    }

    /// The sum as a window, `s × 2^(e − 1074)` with `s` odd, when it fits one.
    fn window(&self) -> Option<(i128, i32)> {
        let mut d = self.clone();
        d.carry();
        let neg = d.d[DIGITS - 1] < 0;
        if neg {
            for x in d.d.iter_mut() {
                *x = -*x;
            }
            d.carry();
        }
        let Some(k0) = d.d.iter().position(|&x| x != 0) else {
            return Some((0, 0));
        };
        let k1 = d.d.iter().rposition(|&x| x != 0).unwrap_or(k0);
        let low = 32 * k0 as i64 + d.d[k0].trailing_zeros() as i64;
        let high = 32 * k1 as i64 + 63 - d.d[k1].leading_zeros() as i64;
        if high - low > 125 {
            return None;
        }
        let mut m: u128 = 0;
        for k in k0..=k1 {
            let at = 32 * k as i64 - low;
            let x = d.d[k] as u128;
            m |= if at >= 0 { x << at } else { x >> -at };
        }
        let m = m as i128;
        Some((if neg { -m } else { m }, low as i32))
    }

    /// The sum rounded to the nearest double, ties to even.
    fn round(&self) -> f64 {
        let mut d = self.clone();
        d.carry();
        let neg = d.d[DIGITS - 1] < 0;
        if neg {
            for x in d.d.iter_mut() {
                *x = -*x;
            }
            d.carry();
        }
        let Some(top) = d.d.iter().rposition(|&x| x != 0) else {
            return 0.0;
        };
        let low = top.saturating_sub(3);
        let mut m: u128 = 0;
        for k in (low..=top).rev() {
            m = (m << 32) | d.d[k] as u128;
        }
        // Anything below the four digits only breaks a tie: the four hold at least 97
        // significant bits, so the lowest one is far below the rounding position.
        if d.d[..low].iter().any(|&x| x != 0) {
            m |= 1;
        }
        let r = scale(m as f64, 32 * low as i32 - 1074);
        if neg { -r } else { r }
    }
}

/// `x × 2^k`, rounded once. Every intermediate lies between `x` and the result, so it is
/// normal whenever the result is.
fn scale(mut x: f64, mut k: i32) -> f64 {
    let step_up = f64::from_bits(((1000 + 1023) as u64) << 52);
    let step_down = f64::from_bits(((-1000 + 1023) as u64) << 52);
    while k > 1000 {
        x *= step_up;
        k -= 1000;
    }
    while k < -1000 {
        x *= step_down;
        k += 1000;
    }
    let p = if k >= -1022 {
        f64::from_bits(((k + 1023) as u64) << 52)
    } else {
        f64::from_bits(1u64 << (k + 1074))
    };
    x * p
}

impl ExactSum {
    /// The empty sum.
    pub const fn new() -> ExactSum {
        ExactSum {
            s: 0,
            e: 0,
            flags: 0,
            spill: None,
        }
    }

    /// Add one value. `−0.0` and `+0.0` add nothing; an infinity or a NaN is remembered.
    #[inline]
    pub fn add(&mut self, x: f64) {
        let bits = x.to_bits();
        let exp = ((bits >> 52) & 0x7ff) as i32;
        let frac = bits & FRAC_MASK;
        let neg = bits >> 63 != 0;
        if exp == 0x7ff {
            self.flags |= if frac != 0 {
                NAN
            } else if neg {
                NEG_INF
            } else {
                POS_INF
            };
            return;
        }
        let (m, q) = if exp == 0 {
            if frac == 0 {
                return;
            }
            (frac, 0)
        } else {
            (frac | HIDDEN, exp - 1)
        };
        let v = if neg { -(m as i128) } else { m as i128 };
        self.add_scaled(v, q);
    }

    /// Add `v × 2^(q − 1074)`, exactly.
    #[inline]
    fn add_scaled(&mut self, v: i128, q: i32) {
        if v == 0 {
            return;
        }
        let tz = v.trailing_zeros();
        let (v, q) = (v >> tz, q + tz as i32);
        if let Some(d) = &mut self.spill {
            d.add(v, q);
            return;
        }
        if self.s == 0 {
            self.s = v;
            self.e = q;
            return;
        }
        if q >= self.e {
            let mut gap = (q - self.e) as u32;
            if gap > room(v) {
                // Raise the window's base by the mantissa's own trailing zeros, which loses
                // nothing.
                let k = self.s.trailing_zeros().min(gap);
                self.s >>= k;
                self.e += k as i32;
                gap -= k;
            }
            if gap <= room(v)
                && let Some(s) = self.s.checked_add(v << gap)
            {
                self.s = s;
                return;
            }
        } else {
            let gap = (self.e - q) as u32;
            if gap < room(self.s)
                && let Some(s) = (self.s << gap).checked_add(v)
            {
                self.s = s;
                self.e = q;
                return;
            }
        }
        let mut d = Digits::new();
        d.add(self.s, self.e);
        d.add(v, q);
        self.s = 0;
        self.e = 0;
        self.spill = Some(d);
    }

    /// Add another exact sum: the result is the sum of both multisets of values.
    pub fn merge(&mut self, other: &ExactSum) {
        self.flags |= other.flags;
        match &other.spill {
            Some(d) => {
                for (k, &x) in d.d.iter().enumerate() {
                    self.add_scaled(x as i128, 32 * k as i32);
                }
            }
            None => self.add_scaled(other.s, other.e),
        }
    }

    /// Whether the sum spilled out of its window.
    pub fn spilled(&self) -> bool {
        self.spill.is_some()
    }

    /// Heap bytes beyond the 32 of the value itself.
    pub fn heap_bytes(&self) -> u64 {
        if self.spill.is_some() {
            core::mem::size_of::<Digits>() as u64
        } else {
            0
        }
    }

    /// The sum rounded once to the nearest double, ties to even. NaN when a NaN was added or
    /// both infinities were; an infinity when one was; an exact zero is `+0.0`; a sum too large
    /// for a double is the infinity of its sign.
    pub fn value(&self) -> f64 {
        if self.flags & NAN != 0 || self.flags & (POS_INF | NEG_INF) == POS_INF | NEG_INF {
            return f64::NAN;
        }
        if self.flags & POS_INF != 0 {
            return f64::INFINITY;
        }
        if self.flags & NEG_INF != 0 {
            return f64::NEG_INFINITY;
        }
        match &self.spill {
            Some(d) => d.round(),
            None if self.s == 0 => 0.0,
            None => {
                let r = scale(self.s.unsigned_abs() as f64, self.e - 1074);
                if self.s < 0 { -r } else { r }
            }
        }
    }

    /// The partial-result form (spec: the state of `sum` and `mean` of floats), one encoding
    /// for each value: the flags, then the window (tag 0, `e` as i32 and `s` as i128,
    /// little-endian, `s` odd, or both zero for zero) when the sum fits 126 bits, or else the
    /// digits after carry propagation (tag 1, each an i64, little-endian).
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.flags);
        let window = match &self.spill {
            None if self.s == 0 => Some((0, 0)),
            None => {
                let tz = self.s.trailing_zeros();
                Some((self.s >> tz, self.e + tz as i32))
            }
            Some(d) => d.window(),
        };
        match (window, &self.spill) {
            (Some((s, e)), _) => {
                out.push(TAG_WINDOW);
                out.extend_from_slice(&e.to_le_bytes());
                out.extend_from_slice(&s.to_le_bytes());
            }
            (None, None) => unreachable!("a window always has a window form"),
            (None, Some(d)) => {
                let mut d = d.clone();
                d.carry();
                out.push(TAG_DIGITS);
                for x in d.d {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
    }

    /// Read [`ExactSum::encode`]'s form, refusing anything it does not write: every value has
    /// one encoding.
    pub fn decode(bytes: &[u8]) -> Result<ExactSum, String> {
        let bad = || format!("an exact sum of {} bytes is malformed", bytes.len());
        let sum = Self::decode_any(bytes).ok_or_else(bad)?;
        let mut again = Vec::with_capacity(bytes.len());
        sum.encode(&mut again);
        if again != bytes {
            return Err(bad());
        }
        Ok(sum)
    }

    fn decode_any(bytes: &[u8]) -> Option<ExactSum> {
        let (&flags, rest) = bytes.split_first()?;
        if flags & !(NAN | POS_INF | NEG_INF) != 0 {
            return None;
        }
        let (&tag, rest) = rest.split_first()?;
        match tag {
            TAG_WINDOW if rest.len() == 20 => {
                let e = i32::from_le_bytes(rest[..4].try_into().ok()?);
                let s = i128::from_le_bytes(rest[4..].try_into().ok()?);
                (0..=MAX_E).contains(&e).then_some(ExactSum {
                    s,
                    e,
                    flags,
                    spill: None,
                })
            }
            TAG_DIGITS if rest.len() == 8 * DIGITS => {
                let mut d = Digits::new();
                for (k, chunk) in rest.as_chunks::<8>().0.iter().enumerate() {
                    let x = i64::from_le_bytes(*chunk);
                    let carried = if k == DIGITS - 1 {
                        (-(1i64 << 31)..(1i64 << 31)).contains(&x)
                    } else {
                        (0..(1i64 << 32)).contains(&x)
                    };
                    if !carried {
                        return None;
                    }
                    d.d[k] = x;
                }
                Some(ExactSum {
                    s: 0,
                    e: 0,
                    flags,
                    spill: Some(d),
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(values: &[f64]) -> f64 {
        let mut s = ExactSum::new();
        for &v in values {
            s.add(v);
        }
        s.value()
    }

    /// A small deterministic generator, so the tests need no dependency.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
        fn f64_wide(&mut self) -> f64 {
            // Any finite double: random bits, rejecting infinities and NaN.
            loop {
                let x = f64::from_bits(self.next());
                if x.is_finite() {
                    return x;
                }
            }
        }
        fn f64_money(&mut self) -> f64 {
            (self.next() % 100_000) as f64 / 100.0 - 300.0
        }
    }

    #[test]
    fn small_sums_are_the_exact_sums_rounded_once() {
        assert_eq!(sum(&[]), 0.0);
        assert_eq!(sum(&[1.5, 2.25]), 3.75);
        assert_eq!(sum(&[0.1, 0.2]), 0.30000000000000004);
        assert_eq!(sum(&[1e100, 1.0, -1e100]), 1.0);
        assert_eq!(sum(&[1e308, 1e308, -1e308]), 1e308);
        assert_eq!(sum(&[-0.0, -0.0]).to_bits(), 0.0f64.to_bits());
        assert_eq!(sum(&[3.0, -3.0]).to_bits(), 0.0f64.to_bits());
        // 2^53 + 1 is a tie between 2^53 and 2^53 + 2; even wins.
        assert_eq!(sum(&[9007199254740992.0, 1.0]), 9007199254740992.0);
        // A tie broken by a tiny value below it rounds up.
        assert_eq!(sum(&[9007199254740992.0, 1.0, 1e-300]), 9007199254740994.0);
        assert_eq!(sum(&[9007199254740992.0, 1.0, -1e-300]), 9007199254740992.0);
    }

    #[test]
    fn subnormals_add_exactly() {
        let tiny = f64::from_bits(1);
        assert_eq!(sum(&[tiny, tiny, tiny]), f64::from_bits(3));
        assert_eq!(
            sum(&[f64::MIN_POSITIVE, -tiny]),
            f64::from_bits((1 << 52) - 1)
        );
        assert_eq!(sum(&[-tiny]), -tiny);
    }

    #[test]
    fn specials_follow_ieee() {
        assert!(sum(&[1.0, f64::NAN]).is_nan());
        assert!(sum(&[f64::INFINITY, f64::NEG_INFINITY]).is_nan());
        assert_eq!(sum(&[f64::INFINITY, 1.0]), f64::INFINITY);
        assert_eq!(sum(&[f64::NEG_INFINITY, -1.0]), f64::NEG_INFINITY);
        assert_eq!(sum(&[f64::MAX, f64::MAX]), f64::INFINITY);
        assert_eq!(sum(&[-f64::MAX, -f64::MAX]), f64::NEG_INFINITY);
        // The sum of two maxima less one maximum is the maximum: no overflow in between.
        assert_eq!(sum(&[f64::MAX, f64::MAX, -f64::MAX]), f64::MAX);
    }

    #[test]
    fn the_window_spills_when_values_span_too_far() {
        let mut s = ExactSum::new();
        s.add(1e300);
        assert!(!s.spilled());
        s.add(1e-300);
        assert!(s.spilled());
        assert!(s.heap_bytes() > 0);
        s.add(-1e300);
        assert_eq!(s.value(), 1e-300);
    }

    /// The point of the type: any order and any split into merged partial sums gives the same
    /// bits, for values over the whole range and for values like money.
    #[test]
    fn order_and_split_do_not_change_the_bits() {
        for (seed, wide) in [(1u64, true), (2, false), (3, true), (4, false)] {
            let mut rng = Lcg(seed);
            let values: Vec<f64> = (0..3000)
                .map(|_| {
                    if wide {
                        rng.f64_wide()
                    } else {
                        rng.f64_money()
                    }
                })
                .collect();
            let reference = sum(&values);
            let mut reversed = values.clone();
            reversed.reverse();
            assert_eq!(sum(&reversed).to_bits(), reference.to_bits());
            for parts in [2usize, 3, 7, 64] {
                let mut partials = vec![ExactSum::new(); parts];
                for (i, &v) in values.iter().enumerate() {
                    partials[(rng.next() as usize + i) % parts].add(v);
                }
                let mut merged = ExactSum::new();
                for p in partials.iter().rev() {
                    // Each through its encoded form, as partial results travel.
                    let mut bytes = Vec::new();
                    p.encode(&mut bytes);
                    merged.merge(&ExactSum::decode(&bytes).unwrap());
                }
                assert_eq!(
                    merged.value().to_bits(),
                    reference.to_bits(),
                    "{seed} {parts}"
                );
            }
        }
    }

    /// Money-like values never leave the window, and their exact sum matches an integer sum of
    /// the same cents rounded once.
    #[test]
    fn money_stays_in_the_window_and_is_exact() {
        let mut rng = Lcg(9);
        let cents: Vec<i64> = (0..100_000)
            .map(|_| (rng.next() % 100_000) as i64 - 30_000)
            .collect();
        let mut s = ExactSum::new();
        for &c in &cents {
            s.add(c as f64 / 100.0);
        }
        assert!(!s.spilled());
        // Each value is the double nearest c/100, so the exact sum is close to, not equal to,
        // the cents' sum; it must be within the values' own rounding.
        let total: i64 = cents.iter().sum();
        assert!((s.value() - total as f64 / 100.0).abs() < 1e-6);
    }

    #[test]
    fn carries_survive_many_additions_of_a_spilled_sum() {
        let mut s = ExactSum::new();
        s.add(1e-300);
        s.add(1e300);
        assert!(s.spilled());
        for _ in 0..1000 {
            s.add(f64::from_bits(u64::MAX >> 12)); // a large subnormal pattern
        }
        let mut d = s.spill.clone().unwrap();
        d.carry();
        assert!(d.d[..DIGITS - 1].iter().all(|&x| (0..1 << 32).contains(&x)));
        assert_eq!(s.value(), 1e300);
    }

    #[test]
    fn decode_refuses_what_encode_does_not_write() {
        assert!(ExactSum::decode(&[]).is_err());
        assert!(ExactSum::decode(&[0]).is_err());
        assert!(ExactSum::decode(&[8, 0]).is_err());
        assert!(ExactSum::decode(&[0, 7]).is_err());
        assert!(ExactSum::decode(&[0, TAG_WINDOW, 1, 2]).is_err());
        let mut window = vec![0, TAG_WINDOW];
        window.extend_from_slice(&(-1i32).to_le_bytes());
        window.extend_from_slice(&0i128.to_le_bytes());
        assert!(ExactSum::decode(&window).is_err());
        let mut digits = vec![0, TAG_DIGITS];
        digits.extend_from_slice(&[0xff; 8 * DIGITS]);
        assert!(ExactSum::decode(&digits).is_err());
        let mut spilled = ExactSum::new();
        spilled.add(f64::INFINITY);
        spilled.add(1e-300);
        spilled.add(1e300);
        let mut bytes = Vec::new();
        spilled.encode(&mut bytes);
        let back = ExactSum::decode(&bytes).unwrap();
        assert_eq!(back.value(), f64::INFINITY);
        assert!(back.spilled());
    }

    fn encoded(s: &ExactSum) -> Vec<u8> {
        let mut out = Vec::new();
        s.encode(&mut out);
        out
    }

    /// One value, one encoding: whatever the order, the window or the digits it was kept in.
    #[test]
    fn the_encoding_is_canonical() {
        let mut a = ExactSum::new();
        let mut b = ExactSum::new();
        for v in [0.5, 0.25, 3.0, -0.125] {
            a.add(v);
        }
        for v in [-0.125, 3.0, 0.25, 0.5] {
            b.add(v);
        }
        assert_eq!(encoded(&a), encoded(&b));
        // A spilled sum that comes back into 126 bits is encoded as a window.
        let mut c = ExactSum::new();
        c.add(1e300);
        c.add(1e-300);
        c.add(-1e300);
        assert!(c.spilled());
        let mut d = ExactSum::new();
        d.add(1e-300);
        assert_eq!(encoded(&c), encoded(&d));
        assert!(!ExactSum::decode(&encoded(&c)).unwrap().spilled());
        // Negative spilled sums keep their sign through the window.
        let mut n = ExactSum::new();
        n.add(-1e300);
        n.add(-1e-300);
        n.add(1e300);
        let mut m = ExactSum::new();
        m.add(-1e-300);
        assert_eq!(encoded(&n), encoded(&m));
        // An even window mantissa, and digits that fit a window, are not the encoding.
        let mut even = vec![0, TAG_WINDOW];
        even.extend_from_slice(&3i32.to_le_bytes());
        even.extend_from_slice(&2i128.to_le_bytes());
        assert!(ExactSum::decode(&even).is_err());
        let mut digits = vec![0, TAG_DIGITS];
        digits.extend_from_slice(&1i64.to_le_bytes());
        digits.extend_from_slice(&[0; 8 * (DIGITS - 1)]);
        assert!(ExactSum::decode(&digits).is_err());
        // Zero is (0, 0), whatever cancelled.
        let mut z = ExactSum::new();
        z.add(1e300);
        z.add(1e-300);
        z.add(-1e-300);
        z.add(-1e300);
        assert_eq!(encoded(&z), encoded(&ExactSum::new()));
        assert_eq!(ExactSum::decode(&encoded(&z)).unwrap().value().to_bits(), 0);
    }
}
