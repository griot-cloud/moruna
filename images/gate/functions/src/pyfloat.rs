//! Python's and NumPy's rules for numbers, where Rust's differ: rounding, `max`, clipping,
//! floor division and the float-to-integer cast. Each function names what it reproduces.

const TWO_52: f64 = 4_503_599_627_370_496.0;
const SIGN: u64 = 1 << 63;

/// `x` with the sign of `s`.
#[inline]
pub fn copysign(x: f64, s: f64) -> f64 {
    f64::from_bits((x.to_bits() & !SIGN) | (s.to_bits() & SIGN))
}

#[inline]
pub fn fabs(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & !SIGN)
}

/// NumPy's `rint`: the nearest integer, ties to even, keeping the sign (`rint(-0.4)` is -0).
#[inline]
pub fn rint(x: f64) -> f64 {
    let a = fabs(x);
    if a >= TWO_52 || a.is_nan() {
        return x; // already an integer, or infinite, or NaN
    }
    // Adding 2**52 leaves no fraction bits, so the add rounds to an integer, ties to even.
    copysign((a + TWO_52) - TWO_52, x)
}

/// NumPy's `np.round(x, 6)`: `rint(x * 1e6) / 1e6`, as `PyArray_Round` computes it.
#[inline]
pub fn np_round6(x: f64) -> f64 {
    rint(x * 1e6) / 1e6
}

/// Python's two-argument `max(a, b)`: `b` only when `b > a`, so a NaN `a` is kept.
#[inline]
pub fn py_max(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// NumPy's `np.maximum(a, b)`: NaN when `a` is NaN, else the larger, `a` on a tie.
#[inline]
pub fn np_maximum(a: f64, b: f64) -> f64 {
    if a >= b || a.is_nan() { a } else { b }
}

/// NumPy's `np.clip(x, lo, hi)` on floats (`_NPY_CLIP`): NaN stays NaN, and a tie keeps the bound
/// (`clip(-0.0, 0.0, 1.0)` is +0).
#[inline]
pub fn np_clip(x: f64, lo: f64, hi: f64) -> f64 {
    let m = if x.is_nan() || x > lo { x } else { lo };
    if m.is_nan() || m < hi { m } else { hi }
}

/// NumPy's `npy_divmod` on floats: `(a // b, a % b)` with Python's signs, as `np.floor_divide`
/// and `np.remainder` compute them.
pub fn np_divmod(a: f64, b: f64) -> (f64, f64) {
    let mut m = libm::fmod(a, b);
    if b == 0.0 {
        return (a / b, m);
    }
    let mut div = (a - m) / b;
    if m != 0.0 || m.is_nan() {
        if (b < 0.0) != (m < 0.0) {
            m += b;
            div -= 1.0;
        }
    } else {
        m = copysign(0.0, b);
    }
    let floordiv = if div != 0.0 || div.is_nan() {
        let mut f = libm::floor(div);
        if div - f > 0.5 {
            f += 1.0;
        }
        f
    } else {
        copysign(0.0, a / b)
    };
    (floordiv, m)
}

/// NumPy's `astype(np.int64)` of a float on x86-64 (`cvttsd2si`): truncation, and the integer
/// indefinite (`i64::MIN`) for NaN and for anything outside the range. The gate runs on x86-64,
/// where 0.6.6 ran.
#[inline]
pub fn np_f64_to_i64(x: f64) -> i64 {
    const LIMIT: f64 = 9_223_372_036_854_775_808.0; // 2**63
    if x.is_nan() || !(-LIMIT..LIMIT).contains(&x) {
        i64::MIN
    } else {
        x as i64
    }
}

/// Python's `round(x, 2)`: the double nearest the decimal that `x`'s exact binary value rounds
/// to at two places, ties to even (CPython's `double_round`, through dtoa's mode 3). Exact
/// integer arithmetic, no formatting.
pub fn py_round2(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let bits = x.to_bits() & !SIGN;
    let exp = (bits >> 52) as i32;
    let frac = bits & ((1 << 52) - 1);
    // |x| = m * 2**e exactly.
    let (m, e) = if exp == 0 {
        (frac, -1074)
    } else {
        (frac | 1 << 52, exp - 1075)
    };
    if e >= 0 {
        return x; // an integer
    }
    let k = (-e) as u32;
    let p = (m as u128) * 100; // below 2**60
    let n: u128 = if k > 61 {
        0 // p / 2**k is below a half
    } else {
        let q = p >> k;
        let r = p & ((1u128 << k) - 1);
        let half = 1u128 << (k - 1);
        if r > half || (r == half && q & 1 == 1) {
            q + 1
        } else {
            q
        }
    };
    copysign(ratio_to_f64(n, 100), x)
}

/// The double nearest `n / d`, ties to even, for `n` below 2**64 and `d` below 2**32.
pub fn ratio_to_f64(n: u128, d: u128) -> f64 {
    if n == 0 || d == 0 {
        return if d == 0 { f64::NAN } else { 0.0 };
    }
    if n < 1 << 53 && d < 1 << 53 {
        // Both exact, and IEEE division rounds the true quotient once.
        return n as f64 / d as f64;
    }
    let nb = 128 - n.leading_zeros() as i32;
    let db = 128 - d.leading_zeros() as i32;
    // Scale so the quotient has 55 or 56 bits: two to round with, and a sticky remainder.
    let s = 55 - (nb - db);
    let (num, den) = if s >= 0 { (n << s, d) } else { (n, d << -s) };
    let q = num / den;
    let sticky = num % den != 0;
    let qb = 128 - q.leading_zeros() as i32;
    let extra = qb - 53;
    let mut mant = q >> extra;
    let dropped = q & ((1u128 << extra) - 1);
    let half = 1u128 << (extra - 1);
    if dropped > half || (dropped == half && (sticky || mant & 1 == 1)) {
        mant += 1;
    }
    // mant * 2**(extra - s); mant is at most 2**53, so the conversion is exact.
    let scale = extra - s;
    (mant as f64) * pow2(scale)
}

/// 2**p for a normal exponent.
fn pow2(p: i32) -> f64 {
    f64::from_bits(((1023 + p) as u64) << 52)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rint_ties_to_even() {
        assert_eq!(rint(0.5), 0.0);
        assert_eq!(rint(1.5), 2.0);
        assert_eq!(rint(2.5), 2.0);
        assert_eq!(rint(-2.5), -2.0);
        assert_eq!(rint(-0.4).to_bits(), (-0.0f64).to_bits());
        assert_eq!(rint(1e300), 1e300);
        assert!(rint(f64::NAN).is_nan());
        assert_eq!(rint(f64::INFINITY), f64::INFINITY);
    }

    #[test]
    fn numpy_rules() {
        assert!(np_maximum(f64::NAN, 0.0).is_nan());
        assert_eq!(np_maximum(-1.0, 0.0), 0.0);
        assert_eq!(np_maximum(-0.0, 0.0).to_bits(), (-0.0f64).to_bits());
        assert!(np_clip(f64::NAN, 0.0, 1.0).is_nan());
        assert_eq!(np_clip(-0.0, 0.0, 1.0).to_bits(), 0.0f64.to_bits());
        assert_eq!(np_clip(5.0, 0.0, 1.0), 1.0);
        assert!(py_max(f64::NAN, 0.0).is_nan());
        assert_eq!(py_max(0.0, f64::NAN), 0.0);
        assert_eq!(py_max(-0.0, 0.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(np_divmod(7.0, 2.0), (3.0, 1.0));
        assert_eq!(np_divmod(-7.0, 2.0), (-4.0, 1.0));
        assert_eq!(np_divmod(7.0, -2.0), (-4.0, -1.0));
        let (d, m) = np_divmod(-1440.0, 1440.0);
        assert_eq!((d, m.to_bits()), (-1.0, 0.0f64.to_bits()));
        let (d, m) = np_divmod(0.0, -3.0);
        assert_eq!(
            (d.to_bits(), m.to_bits()),
            ((-0.0f64).to_bits(), (-0.0f64).to_bits())
        );
        let (d, m) = np_divmod(1.0, 0.0);
        assert!(d.is_infinite() && m.is_nan());
        let (d, m) = np_divmod(f64::NAN, 3.0);
        assert!(d.is_nan() && m.is_nan());
        assert_eq!(np_f64_to_i64(f64::NAN), i64::MIN);
        assert_eq!(np_f64_to_i64(9.3e18), i64::MIN);
        assert_eq!(np_f64_to_i64(-2.7), -2);
        assert_eq!(np_round6(0.0000015), 2e-6);
    }

    #[test]
    fn round2_ties_and_signs() {
        assert_eq!(py_round2(2.675), 2.67);
        assert_eq!(py_round2(0.125), 0.12);
        assert_eq!(py_round2(0.375), 0.38);
        assert_eq!(py_round2(-0.001).to_bits(), (-0.0f64).to_bits());
        assert_eq!(py_round2(1e300), 1e300);
        assert!(py_round2(f64::NAN).is_nan());
        assert_eq!(py_round2(5e-324), 0.0);
        assert_eq!(ratio_to_f64(0, 100), 0.0);
        assert!(ratio_to_f64(1, 0).is_nan());
    }
}
