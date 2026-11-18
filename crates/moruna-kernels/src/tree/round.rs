//! `round`, `floor`, `ceil` and `trunc` of `moruna.expr` (spec/expr.md, "Rounding").
//!
//! `round(x, ndigits, mode)` is defined on the exact binary value of `x`: the decimal multiple of
//! `10^-ndigits` that `mode` picks from the two that bracket `x` exactly, then the float nearest
//! that decimal. This is Python's `round` for `half_to_even`. Arrow C++ instead scales, rounds and
//! unscales in floating point, which differs where the scaling rounds onto or across a tie (7.095
//! at two digits: Arrow 7.1, here 7.09, as Python).
//!
//! Two paths compute it: a fast one, exact for `|ndigits| <= 22` while the scaled value is below
//! 2^52, which finds the scaling's rounding error with a fused multiply-add; and the definition
//! itself, on the decimal digits of the exact value, for every other case. The tests check one
//! against the other.

/// How a value between two multiples is rounded (Arrow's `RoundMode`, every one of its ten).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum RoundMode {
    /// Toward negative infinity.
    Down,
    /// Toward positive infinity.
    Up,
    /// Toward zero.
    TowardsZero,
    /// Away from zero.
    TowardsInfinity,
    /// To the nearest; a tie toward negative infinity.
    HalfDown,
    /// To the nearest; a tie toward positive infinity.
    HalfUp,
    /// To the nearest; a tie toward zero.
    HalfTowardsZero,
    /// To the nearest; a tie away from zero.
    HalfTowardsInfinity,
    /// To the nearest; a tie to the even multiple.
    HalfToEven,
    /// To the nearest; a tie to the odd multiple.
    HalfToOdd,
}

/// Every mode, by its Arrow name.
pub const MODES: [(&str, RoundMode); 10] = [
    ("down", RoundMode::Down),
    ("up", RoundMode::Up),
    ("towards_zero", RoundMode::TowardsZero),
    ("towards_infinity", RoundMode::TowardsInfinity),
    ("half_down", RoundMode::HalfDown),
    ("half_up", RoundMode::HalfUp),
    ("half_towards_zero", RoundMode::HalfTowardsZero),
    ("half_towards_infinity", RoundMode::HalfTowardsInfinity),
    ("half_to_even", RoundMode::HalfToEven),
    ("half_to_odd", RoundMode::HalfToOdd),
];

impl RoundMode {
    /// The mode Arrow names `name`.
    pub fn parse(name: &str) -> Option<RoundMode> {
        MODES.iter().find(|(n, _)| *n == name).map(|(_, m)| *m)
    }

    /// The mode's Arrow name.
    pub fn name(self) -> &'static str {
        MODES
            .iter()
            .find(|(_, m)| *m == self)
            .map(|(n, _)| *n)
            .unwrap_or("half_to_even")
    }
}

/// Where the exact scaled magnitude lies past the multiple below it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Past {
    Zero,
    BelowHalf,
    Half,
    AboveHalf,
}

/// Whether the magnitude goes to the next multiple up, for a value of sign `negative` whose
/// magnitude's lower multiple is even when `lower_even`.
fn magnitude_up(mode: RoundMode, negative: bool, lower_even: bool, past: Past) -> bool {
    use RoundMode::*;
    match (past, mode) {
        (Past::Zero, _) => false,
        (_, Down) => negative,
        (_, Up) => !negative,
        (_, TowardsZero) => false,
        (_, TowardsInfinity) => true,
        (Past::BelowHalf, _) => false,
        (Past::AboveHalf, _) => true,
        (Past::Half, HalfDown) => negative,
        (Past::Half, HalfUp) => !negative,
        (Past::Half, HalfTowardsZero) => false,
        (Past::Half, HalfTowardsInfinity) => true,
        (Past::Half, HalfToEven) => !lower_even,
        (Past::Half, HalfToOdd) => lower_even,
    }
}

/// 10^0 to 10^22, every one exact in a double.
const POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

const TWO_52: f64 = 4_503_599_627_370_496.0;

/// The multiple below the exact scaled magnitude and where the magnitude lies past it, by the
/// fast path; `None` where only the definition can say.
fn bracket(a: f64, ndigits: i64) -> Option<(f64, Past)> {
    if (0..=22).contains(&ndigits) {
        let p10 = POW10[ndigits as usize];
        let p = a * p10;
        // Not below 2^52, or NaN: only the definition can say.
        if p.partial_cmp(&TWO_52) != Some(core::cmp::Ordering::Less) {
            return None;
        }
        // The product's rounding error, exactly: a * p10 = p + err.
        let err = a.mul_add(p10, -p);
        let n0 = p.floor();
        let frac = p - n0;
        Some(if frac == 0.0 {
            match err.partial_cmp(&0.0)? {
                core::cmp::Ordering::Equal => (n0, Past::Zero),
                core::cmp::Ordering::Greater => (n0, Past::BelowHalf),
                core::cmp::Ordering::Less => (n0 - 1.0, Past::AboveHalf),
            }
        } else {
            // frac - 0.5 is exact, and one rounding keeps the sum's sign.
            let s = (frac - 0.5) + err;
            let past = if s < 0.0 {
                Past::BelowHalf
            } else if s > 0.0 {
                Past::AboveHalf
            } else {
                Past::Half
            };
            (n0, past)
        })
    } else if (-22..0).contains(&ndigits) {
        let p10 = POW10[(-ndigits) as usize];
        let q = a / p10;
        if q.partial_cmp(&TWO_52) != Some(core::cmp::Ordering::Less) {
            return None;
        }
        // The division's remainder, exactly: a = q * p10 + rem, so a / p10 = q + rem / p10.
        let rem = (-q).mul_add(p10, a);
        let n0 = q.floor();
        let frac = q - n0;
        Some(if frac == 0.0 {
            match rem.partial_cmp(&0.0)? {
                core::cmp::Ordering::Equal => (n0, Past::Zero),
                core::cmp::Ordering::Greater => (n0, Past::BelowHalf),
                core::cmp::Ordering::Less => (n0 - 1.0, Past::AboveHalf),
            }
        } else {
            let s = (frac - 0.5).mul_add(p10, rem);
            let past = if s < 0.0 {
                Past::BelowHalf
            } else if s > 0.0 {
                Past::AboveHalf
            } else {
                Past::Half
            };
            (n0, past)
        })
    } else {
        None
    }
}

/// `round(x, ndigits, mode)` of a double.
pub fn round_f64(x: f64, ndigits: i64, mode: RoundMode) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    let negative = x < 0.0;
    let a = x.abs();
    if let Some((lower, past)) = bracket(a, ndigits) {
        let up = magnitude_up(mode, negative, lower % 2.0 == 0.0, past);
        let n = if up { lower + 1.0 } else { lower };
        let r = if ndigits >= 0 {
            n / POW10[ndigits as usize]
        } else {
            n * POW10[(-ndigits) as usize]
        };
        return r.copysign(x);
    }
    by_digits(x, ndigits, mode, |s| s.parse::<f64>().ok()).unwrap_or(x)
}

/// `round(x, ndigits, mode)` of a single: the exact value is the same as the double's, and the
/// result is the single nearest the chosen decimal.
pub fn round_f32(x: f32, ndigits: i64, mode: RoundMode) -> f32 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    let negative = x < 0.0;
    let a = f64::from(x.abs());
    if (-10..=10).contains(&ndigits)
        && let Some((lower, past)) = bracket(a, ndigits)
    {
        let up = magnitude_up(mode, negative, lower % 2.0 == 0.0, past);
        let n = if up { lower + 1.0 } else { lower };
        // n and 10^|ndigits| (up to 10^10) are exact in a single, so one division or
        // multiplication in singles is the single nearest the decimal.
        if n < 16_777_216.0 {
            let p10 = POW10[ndigits.unsigned_abs() as usize] as f32;
            let r = if ndigits >= 0 {
                n as f32 / p10
            } else {
                n as f32 * p10
            };
            return r.copysign(x);
        }
    }
    by_digits(f64::from(x), ndigits, mode, |s| s.parse::<f32>().ok()).unwrap_or(x)
}

/// The definition: the decimal digits of `x`'s exact value, rounded at `ndigits` after the point
/// by `mode`, then read back by `parse`, which rounds a decimal to the nearest float.
fn by_digits<F>(
    x: f64,
    ndigits: i64,
    mode: RoundMode,
    parse: impl Fn(&str) -> Option<F>,
) -> Option<F> {
    let negative = x < 0.0;
    // A double's exact expansion has at most 1074 digits after the point.
    let text = format!("{:.1100}", x.abs());
    let (int, frac) = text.split_once('.')?;
    let int = int.trim_start_matches('0');
    let digits: Vec<u8> = int.bytes().chain(frac.bytes()).map(|b| b - b'0').collect();
    let point = int.len() as i64;
    // Past 2000 either way the answer no longer changes: every digit is kept, or none is.
    let nd = ndigits.clamp(-2000, 2000);
    let cut = point + nd;
    if cut >= digits.len() as i64 {
        return parse(&format!("{}{}", if negative { "-" } else { "" }, text));
    }
    let (kept, rest): (Vec<u8>, Vec<u8>) = if cut <= 0 {
        (Vec::new(), digits.clone())
    } else {
        (
            digits[..cut as usize].to_vec(),
            digits[cut as usize..].to_vec(),
        )
    };
    let rest_nonzero = rest.iter().any(|&d| d != 0);
    let past = if !rest_nonzero {
        Past::Zero
    } else if cut < 0 {
        // At least one zero digit stands between the cut and the first digit.
        Past::BelowHalf
    } else {
        match rest[0] {
            0..=4 => Past::BelowHalf,
            5 if !rest[1..].iter().any(|&d| d != 0) => Past::Half,
            _ => Past::AboveHalf,
        }
    };
    let lower_even = kept.last().is_none_or(|d| d % 2 == 0);
    let mut n = kept;
    if magnitude_up(mode, negative, lower_even, past) {
        let mut i = n.len();
        loop {
            if i == 0 {
                n.insert(0, 1);
                break;
            }
            i -= 1;
            if n[i] == 9 {
                n[i] = 0;
            } else {
                n[i] += 1;
                break;
            }
        }
    }
    let mantissa: String = if n.is_empty() {
        "0".into()
    } else {
        n.iter().map(|d| char::from(b'0' + d)).collect()
    };
    // The kept digits are the magnitude in units of 10^-nd.
    parse(&format!(
        "{}{}e{}",
        if negative { "-" } else { "" },
        mantissa,
        -nd
    ))
}

/// `round(v, ndigits, mode)` of an integer of `bits` bits, signed or not: unchanged for
/// `ndigits >= 0`; otherwise to a multiple of `10^-ndigits`, `None` when that overflows the
/// type.
pub fn round_int(v: i128, ndigits: i64, mode: RoundMode, bits: u32, signed: bool) -> Option<i128> {
    if ndigits >= 0 {
        return Some(v);
    }
    let m = ndigits.unsigned_abs().min(38) as u32;
    let p = 10i128.pow(m);
    let negative = v < 0;
    let a = v.unsigned_abs() as i128;
    let (lower, r) = (a / p, a % p);
    let past = match (r * 2).cmp(&p) {
        _ if r == 0 => Past::Zero,
        core::cmp::Ordering::Less => Past::BelowHalf,
        core::cmp::Ordering::Equal => Past::Half,
        core::cmp::Ordering::Greater => Past::AboveHalf,
    };
    let n = lower + i128::from(magnitude_up(mode, negative, lower % 2 == 0, past));
    let magnitude = n.checked_mul(p)?;
    let out = if negative { -magnitude } else { magnitude };
    let (min, max) = if signed {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    } else {
        (0, (1i128 << bits) - 1)
    };
    (min..=max).contains(&out).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn half_even(x: f64, n: i64) -> f64 {
        round_f64(x, n, RoundMode::HalfToEven)
    }

    #[test]
    fn pythons_round_on_the_values_arrow_rounds_otherwise() {
        // Each from Audit's month, where pyarrow 24's round differs from Python's.
        for (x, want) in [
            (86.64999999999999, 86.65),
            (5.3100000000000005, 5.31),
            (7.095, 7.09),
            (10.315, 10.31),
            (11.049999999999999, 11.05),
            (28.585, 28.59),
            (8.215, 8.21),
            (7.6899999999999995, 7.69),
            (22.075, 22.07),
            (25.925, 25.93),
        ] {
            assert_eq!(half_even(x, 2).to_bits(), f64::to_bits(want), "{x}");
        }
        assert_eq!(half_even(2.675, 2), 2.67);
        assert_eq!(half_even(0.125, 2), 0.12);
        assert_eq!(half_even(0.375, 2), 0.38);
        assert_eq!(half_even(1.115, 2), 1.11);
        assert_eq!(half_even(0.5, 0), 0.0);
        assert_eq!(half_even(1.5, 0), 2.0);
        assert_eq!(half_even(2.5, 0), 2.0);
        assert_eq!(half_even(-2.5, 0), -2.0);
        assert_eq!(half_even(1250.0, -2), 1200.0);
        assert_eq!(half_even(1350.0, -2), 1400.0);
        assert_eq!(half_even(1e300, 2), 1e300);
        assert_eq!(half_even(1e300, -2), 1e300);
        assert_eq!(half_even(0.1, 30), 0.1);
        assert_eq!(half_even(0.1, 17), 0.1);
        assert!(half_even(f64::NAN, 2).is_nan());
        assert_eq!(half_even(f64::INFINITY, 2), f64::INFINITY);
    }

    #[test]
    fn a_zero_result_keeps_the_sign_of_the_value() {
        assert_eq!(half_even(-0.001, 2).to_bits(), (-0.0f64).to_bits());
        assert_eq!(half_even(-0.001, 0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(half_even(0.001, 0).to_bits(), 0.0f64.to_bits());
        assert_eq!(half_even(-0.0, 2).to_bits(), (-0.0f64).to_bits());
        assert_eq!(
            round_f32(-0.001, 1, RoundMode::HalfUp).to_bits(),
            (-0.0f32).to_bits()
        );
    }

    #[test]
    fn every_mode_on_ties_and_non_ties() {
        // Arrow's own table: 2.5, -2.5 and 2.45 at zero digits.
        let want = [
            ("down", [2.0, -3.0, 2.0]),
            ("up", [3.0, -2.0, 3.0]),
            ("towards_zero", [2.0, -2.0, 2.0]),
            ("towards_infinity", [3.0, -3.0, 3.0]),
            ("half_down", [2.0, -3.0, 2.0]),
            ("half_up", [3.0, -2.0, 2.0]),
            ("half_towards_zero", [2.0, -2.0, 2.0]),
            ("half_towards_infinity", [3.0, -3.0, 2.0]),
            ("half_to_even", [2.0, -2.0, 2.0]),
            ("half_to_odd", [3.0, -3.0, 2.0]),
        ];
        for (name, values) in want {
            let mode = RoundMode::parse(name).expect(name);
            assert_eq!(mode.name(), name);
            for (x, w) in [2.5, -2.5, 2.45].into_iter().zip(values) {
                assert_eq!(round_f64(x, 0, mode), w, "{name} {x}");
                assert_eq!(round_f32(x as f32, 0, mode), w as f32, "{name} {x}");
            }
        }
        assert_eq!(RoundMode::parse("nearest"), None);
    }

    #[test]
    fn singles_round_their_exact_value_to_the_nearest_single() {
        // 2.675 as a single is 2.67499995..., so two digits give 2.67.
        assert_eq!(round_f32(2.675, 2, RoundMode::HalfToEven), 2.67f32);
        assert_eq!(round_f32(0.125, 2, RoundMode::HalfToEven), 0.12f32);
        assert_eq!(round_f32(1e30, 2, RoundMode::HalfToEven), 1e30f32);
        assert_eq!(round_f32(123456.0, -3, RoundMode::HalfUp), 123000.0);
        assert_eq!(round_f32(0.1, 12, RoundMode::HalfToEven), 0.1f32);
    }

    /// A deterministic sequence for the cross-check: SplitMix64.
    fn next(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    #[test]
    fn the_fast_path_is_the_definition() {
        let mut state = 7u64;
        for i in 0..12_000 {
            let r = next(&mut state);
            // Decimal-looking values (ties at their digits are common), then arbitrary ones.
            let x = if i % 2 == 0 {
                let cents = (r % 2_000_000) as f64 - 1_000_000.0;
                cents / [1.0, 10.0, 100.0, 1000.0, 8.0][(r >> 40) as usize % 5]
            } else {
                f64::from_bits(r >> 2) * if r & 1 == 1 { -1.0 } else { 1.0 }
            };
            if !x.is_finite() || x == 0.0 {
                continue;
            }
            let ndigits = ((r >> 20) % 30) as i64 - 8;
            let (_, mode) = MODES[(r >> 50) as usize % 10];
            let fast = round_f64(x, ndigits, mode);
            let slow = by_digits(x, ndigits, mode, |s| s.parse::<f64>().ok()).expect("digits");
            assert_eq!(fast.to_bits(), slow.to_bits(), "{x} {ndigits} {mode:?}");
            let xs = x as f32;
            if xs.is_finite() && xs != 0.0 {
                let fast = round_f32(xs, ndigits, mode);
                let slow = by_digits(f64::from(xs), ndigits, mode, |s| s.parse::<f32>().ok())
                    .expect("digits");
                assert_eq!(fast.to_bits(), slow.to_bits(), "{xs} {ndigits} {mode:?}");
            }
        }
    }

    #[test]
    fn far_digits_and_huge_values() {
        assert_eq!(half_even(5e-324, 400), 5e-324);
        assert_eq!(half_even(1.5, -400), 0.0);
        assert_eq!(round_f64(1.5, -400, RoundMode::Up), f64::INFINITY);
        assert_eq!(round_f64(-1.5, -400, RoundMode::Down), f64::NEG_INFINITY);
        assert_eq!(half_even(9.9e307, -308), 1e308);
        assert_eq!(half_even(4.5e15, 0), 4.5e15);
        assert_eq!(
            half_even(4_503_599_627_370_497.5, 0),
            4_503_599_627_370_498.0
        );
    }

    #[test]
    fn integers_round_to_tens_and_refuse_overflow() {
        let he = RoundMode::HalfToEven;
        assert_eq!(round_int(15, 2, he, 64, true), Some(15));
        assert_eq!(round_int(15, -1, he, 64, true), Some(20));
        assert_eq!(round_int(25, -1, he, 64, true), Some(20));
        assert_eq!(
            round_int(-15, -1, RoundMode::HalfToOdd, 64, true),
            Some(-10)
        );
        assert_eq!(round_int(127, -1, RoundMode::HalfUp, 8, true), None);
        assert_eq!(round_int(250, -1, RoundMode::HalfDown, 8, false), Some(250));
        assert_eq!(round_int(255, -1, RoundMode::HalfUp, 8, false), None);
        assert_eq!(round_int(i64::MAX as i128, -1, he, 64, true), None);
        assert_eq!(round_int(4, -40, RoundMode::Up, 64, true), None);
        assert_eq!(round_int(4, -40, he, 64, true), Some(0));
        assert_eq!(round_int(-4, -1, RoundMode::Down, 64, true), Some(-10));
    }
}
