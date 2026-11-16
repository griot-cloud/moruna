//! The four NYC functions of `bench/vm/workloads.py`, over column slices.
//!
//! Timestamps are their raw `int64` values, as `cast(pa.int64())` gives them. Each function
//! returns its computed columns; the caller (`batch`, or a task's record codec) assembles the
//! output batch, passing the input columns the function passes through.

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use crate::cols::{BitsBuf, Col};
use crate::error::{Error, Result};
use crate::pyfloat::{np_clip, np_divmod, np_f64_to_i64, np_maximum, np_round6, py_max, py_round2};
use crate::score_weights::{
    B1, B2, COS_HOUR, EMB, EMBED, FEATURES, HIDDEN, SIN_HOUR, W1, W2, W3, ZONES,
};

/// The range in which arrow's cast of an `int64` to a `double` is exact; arrow refuses the rest.
const EXACT: i64 = 1 << 53;
const US_PER_HOUR: i64 = 3_600_000_000;
const US_PER_DAY: i64 = 86_400_000_000;
const US_PER_MINUTE: i64 = 60_000_000;

/// Enrich's computed columns, for the rows it keeps.
#[derive(Debug, Default)]
pub struct Enriched {
    /// The input rows kept, in order.
    pub keep: Vec<u32>,
    pub duration_s: Vec<f64>,
    pub mph: Vec<f64>,
    pub fare_per_mile: Vec<f64>,
    /// Null where the trip's tip is null.
    pub tip_share: Vec<f64>,
    pub tip_valid: BitsBuf,
}

/// W1, `enrich`: duration, speed, fare per mile and tip share; drops impossible trips.
///
/// `dur = (dropoff - pickup) / 1e6` (pyarrow's subtract wraps, and its cast to double refuses a
/// difference past 2**53); a row is kept when `60 <= dur <= 14400`, `0 < dist < 100` and
/// `fare > 0`, all non-null (`and_` is null on a null, and `filter` drops nulls).
pub fn enrich(
    pickup: Col<i64>,
    dropoff: Col<i64>,
    dist: Col<f64>,
    fare: Col<f64>,
    tip: Col<f64>,
) -> Result<Enriched> {
    let n = pickup.len();
    if n > u32::MAX as usize {
        return Err(Error::Layout(format!("a batch of {n} rows")));
    }
    // The cast runs over every row before the filter, so any row refuses the batch.
    let mut dur = Vec::with_capacity(n);
    for i in 0..n {
        match (pickup.get(i), dropoff.get(i)) {
            (Some(a), Some(b)) => {
                let d = b.wrapping_sub(a);
                if !(-EXACT..=EXACT).contains(&d) {
                    return Err(Error::Value(format!(
                        "ArrowInvalid: Integer value {d} not in range: -{EXACT} to {EXACT}"
                    )));
                }
                dur.push(Some(d as f64 / 1_000_000.0));
            }
            _ => dur.push(None),
        }
    }
    let mut out = Enriched::default();
    for (i, dur) in dur.into_iter().enumerate() {
        let (Some(dur), Some(d), Some(f)) = (dur, dist.get(i), fare.get(i)) else {
            continue;
        };
        if !((60.0..=4.0 * 3600.0).contains(&dur) && d > 0.0 && d < 100.0 && f > 0.0) {
            continue;
        }
        out.keep.push(i as u32);
        out.duration_s.push(dur);
        out.mph.push(d / (dur / 3600.0));
        out.fare_per_mile.push(f / d);
        match tip.get(i) {
            Some(t) => {
                out.tip_share.push(t / f);
                out.tip_valid.push(true);
            }
            None => {
                out.tip_share.push(0.0);
                out.tip_valid.push(false);
            }
        }
    }
    Ok(out)
}

/// W2, `audit`: re-prices each trip from a simplified meter rule and flags the gap. Returns
/// `expected_fare` and `flagged`. A null pickup or dropoff raises in Python (`b - a` on `None`).
pub fn audit(
    pickup: Col<i64>,
    dropoff: Col<i64>,
    dist: Col<f64>,
    fare: Col<f64>,
    rate: Col<f64>,
) -> Result<(Vec<f64>, Vec<bool>)> {
    let n = pickup.len();
    let mut expected = Vec::with_capacity(n);
    let mut flag = Vec::with_capacity(n);
    for i in 0..n {
        let (Some(a), Some(b)) = (pickup.get(i), dropoff.get(i)) else {
            return Err(Error::Value(format!(
                "TypeError: unsupported operand type(s) for -: a null timestamp at row {i}"
            )));
        };
        // Python's integers do not overflow; their difference is converted once, rounded.
        let minutes = py_max((b as i128 - a as i128) as f64 / 60e6, 0.0);
        let r = rate.get(i);
        let e = if r == Some(2.0) {
            70.0
        } else {
            // `d or 0.0`: a null, 0.0 or -0.0 distance is 0.0; NaN is truthy and stays.
            let d = match dist.get(i) {
                Some(d) if d != 0.0 => d,
                _ => 0.0,
            };
            let mut e = 3.0 + 3.5 * d + 0.35 * py_max(minutes - d * 2.0, 0.0);
            let hour = a.div_euclid(US_PER_HOUR).rem_euclid(24);
            if (16..20).contains(&hour) {
                e += 2.5;
            }
            e
        };
        expected.push(py_round2(e));
        flag.push(match fare.get(i) {
            Some(f) => libm::fabs(f - e) > py_max(5.0, 0.25 * e),
            None => false,
        });
    }
    Ok((expected, flag))
}

/// Occupancy's output: integer columns, or `double` ones when a pickup zone is null (NumPy then
/// holds the zones as floats, and the keys follow).
#[derive(Debug, PartialEq)]
pub enum Occupied {
    Int {
        zone: Vec<i64>,
        minute_of_day: Vec<i64>,
        car_minutes: Vec<i64>,
    },
    Float {
        zone: Vec<f64>,
        minute_of_day: Vec<f64>,
        car_minutes: Vec<i64>,
    },
}

/// The most counters the dense path keeps (16 MiB of `u32`); a wider key range is sorted.
const DENSE_MAX: i64 = 1 << 22;

/// W3, `occupancy`: every trip expanded into one row per minute it ran (at most 241), then
/// cars counted per pickup zone per minute of the day, sorted by `zone * 1440 + minute`.
///
/// A null pickup or dropoff raises in Python (`np.repeat` refuses float counts).
pub fn occupancy(pickup: Col<i64>, dropoff: Col<i64>, zone: Col<i64>) -> Result<Occupied> {
    if pickup.null_count() > 0 || dropoff.null_count() > 0 {
        return Err(Error::Value(
            "TypeError: Cannot cast array data from dtype('float64') to dtype('int64') \
             according to the rule 'safe'"
                .into(),
        ));
    }
    let n = pickup.len();
    // Each trip: its first minute and its minute count.
    let mut first = Vec::with_capacity(n);
    let mut count = Vec::with_capacity(n);
    for i in 0..n {
        let a = pickup.values[i].div_euclid(US_PER_MINUTE);
        let b = dropoff.values[i].div_euclid(US_PER_MINUTE);
        first.push(a);
        count.push(b.wrapping_sub(a).clamp(0, 240) + 1);
    }
    if zone.null_count() > 0 {
        return Ok(occupancy_float(&first, &count, zone));
    }
    let zones = &zone.values[..n];
    let (zone_out, minute_out, counts) = match dense_range(zones) {
        Some((lo, width)) => count_dense(zones, &first, &count, lo, width),
        None => count_sorted(zones, &first, &count),
    };
    Ok(Occupied::Int {
        zone: zone_out,
        minute_of_day: minute_out,
        car_minutes: counts,
    })
}

/// The lowest key and the key range, when every key `zone * 1440 + m` fits the dense counters
/// with no overflow.
fn dense_range(zones: &[i64]) -> Option<(i64, i64)> {
    let lo = *zones.iter().min()?;
    let hi = *zones.iter().max()?;
    let lo_key = lo.checked_mul(1440)?;
    let width = hi.checked_sub(lo)?.checked_add(1)?.checked_mul(1440)?;
    lo_key.checked_add(width)?;
    (width <= DENSE_MAX).then_some((lo_key, width))
}

fn emit(key: i64, n: i64, z: &mut Vec<i64>, m: &mut Vec<i64>, c: &mut Vec<i64>) {
    z.push(key.div_euclid(1440));
    m.push(key.rem_euclid(1440));
    c.push(n);
}

fn count_dense(
    zones: &[i64],
    first: &[i64],
    count: &[i64],
    lo: i64,
    width: i64,
) -> (Vec<i64>, Vec<i64>, Vec<i64>) {
    let mut counters = vec![0u64; width as usize];
    for i in 0..zones.len() {
        let base = (zones[i] * 1440 - lo) as usize;
        let mut m = first[i].rem_euclid(1440) as usize;
        for _ in 0..count[i] {
            counters[base + m] += 1;
            m += 1;
            if m == 1440 {
                m = 0;
            }
        }
    }
    let (mut z, mut m, mut c) = (Vec::new(), Vec::new(), Vec::new());
    for (k, &n) in counters.iter().enumerate() {
        if n > 0 {
            emit(lo + k as i64, n as i64, &mut z, &mut m, &mut c);
        }
    }
    (z, m, c)
}

fn count_sorted(zones: &[i64], first: &[i64], count: &[i64]) -> (Vec<i64>, Vec<i64>, Vec<i64>) {
    let total: i64 = count.iter().sum();
    let mut keys = Vec::with_capacity(total.max(0) as usize);
    for i in 0..zones.len() {
        let base = zones[i].wrapping_mul(1440);
        for j in 0..count[i] {
            keys.push(base.wrapping_add(first[i].wrapping_add(j).rem_euclid(1440)));
        }
    }
    keys.sort_unstable();
    let (mut z, mut m, mut c) = (Vec::new(), Vec::new(), Vec::new());
    let mut i = 0;
    while i < keys.len() {
        let mut j = i + 1;
        while j < keys.len() && keys[j] == keys[i] {
            j += 1;
        }
        emit(keys[i], (j - i) as i64, &mut z, &mut m, &mut c);
        i = j;
    }
    (z, m, c)
}

/// The float path: keys `zone * 1440.0 + m` as doubles (NaN for a null zone), `np.unique`'s
/// order (NaN last, one NaN group), then `//` and `%` as NumPy computes them on doubles.
fn occupancy_float(first: &[i64], count: &[i64], zone: Col<i64>) -> Occupied {
    let mut keys = Vec::new();
    let mut nan = 0i64;
    for i in 0..first.len() {
        let z = zone.get(i).map(|z| z as f64);
        for j in 0..count[i] {
            match z {
                Some(z) => keys.push(z * 1440.0 + first[i].wrapping_add(j).rem_euclid(1440) as f64),
                None => nan += 1,
            }
        }
    }
    keys.sort_unstable_by(f64::total_cmp);
    let (mut zo, mut mo, mut co) = (Vec::new(), Vec::new(), Vec::new());
    let mut i = 0;
    while i < keys.len() {
        let mut j = i + 1;
        while j < keys.len() && keys[j] == keys[i] {
            j += 1;
        }
        let (d, m) = np_divmod(keys[i], 1440.0);
        zo.push(d);
        mo.push(m);
        co.push((j - i) as i64);
        i = j;
    }
    if nan > 0 {
        zo.push(f64::NAN);
        mo.push(f64::NAN);
        co.push(nan);
    }
    Occupied::Float {
        zone: zo,
        minute_of_day: mo,
        car_minutes: co,
    }
}

/// The columns Score reads.
#[derive(Clone, Copy, Debug)]
pub struct Trips<'a> {
    pub pickup: Col<'a, i64>,
    pub dropoff: Col<'a, i64>,
    pub trip_distance: Col<'a, f64>,
    pub passenger_count: Col<'a, f64>,
    pub ratecode_id: Col<'a, f64>,
    pub pu_location_id: Col<'a, i64>,
    pub do_location_id: Col<'a, i64>,
}

/// Rows scored at once: each weight row is read once a block.
const BLOCK: usize = 8;

/// W4, `score`: 23 features a trip, a 23-128-128-1 network, `np.round(y, 6)`.
///
/// NumPy holds a timestamp column with a null as doubles (NaN for the null), so the arithmetic
/// on it is float arithmetic; within +/- 2**53 microseconds that gives the integer path's values.
/// The network's sums run in order over `k`, unfused; BLAS orders them its own way, so
/// `predicted_tip` is matched within one step of its sixth decimal (images/gate/README.md).
pub fn score(t: &Trips) -> Vec<f64> {
    let n = t.pickup.len();
    let float_a = t.pickup.null_count() > 0;
    let float_b = t.dropoff.null_count() > 0;
    let mut out = Vec::with_capacity(n);
    let mut x = [[0.0f64; FEATURES]; BLOCK];
    let mut h1 = [[0.0f64; HIDDEN]; BLOCK];
    let mut h2 = [[0.0f64; HIDDEN]; BLOCK];
    let mut start = 0;
    while start < n {
        let m = BLOCK.min(n - start);
        for (r, row) in x[..m].iter_mut().enumerate() {
            features(t, start + r, float_a, float_b, row);
        }
        layer(&x[..m], &W1, &B1, &mut h1[..m]);
        layer(&h1[..m], &W2, &B2, &mut h2[..m]);
        for h in &h2[..m] {
            let mut acc = 0.0;
            for k in 0..HIDDEN {
                acc += h[k] * W3[k];
            }
            out.push(np_round6(acc));
        }
        start += m;
    }
    out
}

/// Rows and columns of the register tile `layer` accumulates in.
const TILE_ROWS: usize = 4;
const TILE_COLS: usize = 8;

/// `h = maximum(x @ w + b, 0)` for a block of rows, each sum in order over `k` from 0, unfused:
/// the order fixes every bit, whatever width the vectors are. The sums are kept in a tile of
/// registers across `k`, so each weight is loaded once for four rows.
fn layer<const K: usize>(x: &[[f64; K]], w: &[f64], b: &[f64; HIDDEN], h: &mut [[f64; HIDDEN]]) {
    let m = x.len().min(h.len());
    let mut r0 = 0;
    while r0 < m {
        // A partial tile reads zero rows and drops their sums.
        let mut xs = [[0.0f64; K]; TILE_ROWS];
        let rows = TILE_ROWS.min(m - r0);
        xs[..rows].copy_from_slice(&x[r0..r0 + rows]);
        for j0 in (0..HIDDEN).step_by(TILE_COLS) {
            let mut acc = [[0.0f64; TILE_COLS]; TILE_ROWS];
            for k in 0..K {
                let at = k * HIDDEN + j0;
                let wk: &[f64; TILE_COLS] = w
                    .get(at..at + TILE_COLS)
                    .and_then(|s| s.try_into().ok())
                    .unwrap_or(&[0.0; TILE_COLS]);
                for (a, xr) in acc.iter_mut().zip(&xs) {
                    let xv = xr[k];
                    for jj in 0..TILE_COLS {
                        a[jj] += xv * wk[jj];
                    }
                }
            }
            for (r, a) in acc.iter().enumerate().take(rows) {
                for jj in 0..TILE_COLS {
                    h[r0 + r][j0 + jj] = np_maximum(a[jj] + b[j0 + jj], 0.0);
                }
            }
        }
        r0 += rows;
    }
}

/// `(a // q) % r` on one timestamp, as NumPy computes it on an int64 or a float64 column.
fn floor_mod(a: Option<i64>, as_float: bool, q: i64, r: i64) -> f64 {
    match (a, as_float) {
        (None, _) => f64::NAN,
        (Some(a), false) => a.div_euclid(q).rem_euclid(r) as f64,
        (Some(a), true) => np_divmod(np_divmod(a as f64, q as f64).0, r as f64).1,
    }
}

/// An embedding row: the zone (null as 0) through float and back to int64 as NumPy does on
/// x86-64, then clipped to 0..=299.
fn embedding(zone: Option<i64>) -> &'static [f64] {
    let z = np_f64_to_i64(zone.unwrap_or(0) as f64).clamp(0, ZONES as i64 - 1) as usize;
    &EMB[z * EMBED..(z + 1) * EMBED]
}

fn features(t: &Trips, i: usize, float_a: bool, float_b: bool, x: &mut [f64; FEATURES]) {
    let a = t.pickup.get(i);
    let b = t.dropoff.get(i);
    let diff = match (a, b) {
        (Some(a), Some(b)) if !(float_a || float_b) => b.wrapping_sub(a) as f64,
        (Some(a), Some(b)) => b as f64 - a as f64,
        _ => f64::NAN,
    };
    let dist = np_clip(t.trip_distance.get(i).unwrap_or(0.0), 0.0, 200.0);
    let dur = np_clip(diff / 60e6, 0.0, 240.0);
    let hour = floor_mod(a, float_a, US_PER_HOUR, 24);
    x[0] = libm::log1p(dist);
    x[1] = libm::log1p(dur);
    // hour is an integer from 0 to 23, or NaN.
    let (s, c) = if hour.is_nan() {
        (f64::NAN, f64::NAN)
    } else {
        let h = (hour as usize).min(23);
        (SIN_HOUR[h], COS_HOUR[h])
    };
    x[2] = s;
    x[3] = c;
    x[4] = floor_mod(a, float_a, US_PER_DAY, 7) / 6.0;
    x[5] = t.passenger_count.get(i).unwrap_or(1.0) / 4.0;
    x[6] = t.ratecode_id.get(i).unwrap_or(1.0) / 6.0;
    x[7..15].copy_from_slice(embedding(t.pu_location_id.get(i)));
    x[15..23].copy_from_slice(embedding(t.do_location_id.get(i)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cols::Bits;

    fn col<T: Copy>(v: &[T]) -> Col<'_, T> {
        Col::new(v, None)
    }

    #[test]
    fn occupancy_paths_agree() {
        let pickup = [0i64, 59 * 60_000_000, 1439 * 60_000_000, -60_000_000];
        let dropoff = [
            5 * 60_000_000i64,
            61 * 60_000_000,
            1442 * 60_000_000,
            300 * 60_000_000,
        ];
        let zones = [1i64, 1, 2, 1];
        let dense = occupancy(col(&pickup), col(&dropoff), col(&zones)).unwrap();
        let first: Vec<i64> = pickup.iter().map(|a| a.div_euclid(US_PER_MINUTE)).collect();
        let count: Vec<i64> = pickup
            .iter()
            .zip(&dropoff)
            .map(|(a, b)| {
                (b.div_euclid(US_PER_MINUTE) - a.div_euclid(US_PER_MINUTE)).clamp(0, 240) + 1
            })
            .collect();
        let sorted = count_sorted(&zones, &first, &count);
        let Occupied::Int {
            zone,
            minute_of_day,
            car_minutes,
        } = dense
        else {
            panic!("int path")
        };
        assert_eq!((zone, minute_of_day, car_minutes), sorted);
        // A zone range too wide for counters, and one that overflows, are sorted.
        assert!(dense_range(&[0, i64::MAX / 1440]).is_none());
        assert!(dense_range(&[i64::MIN, 0]).is_none());
        let wide = [i64::MAX, -5];
        let out = occupancy(col(&[0, 0]), col(&[0, 0]), col(&wide)).unwrap();
        let Occupied::Int { zone, .. } = out else {
            panic!()
        };
        assert_eq!(zone.len(), 2);
    }

    #[test]
    fn occupancy_refuses_null_times() {
        let bits = [0b10u8];
        let v = [0i64, 0];
        let nulls = Col::new(&v[..], Some(Bits::new(&bits, 0, 2).unwrap()));
        assert!(occupancy(nulls, col(&v), col(&v)).is_err());
        assert!(occupancy(col(&v), nulls, col(&v)).is_err());
        let out = occupancy(col(&v), col(&v), nulls).unwrap();
        let Occupied::Float {
            zone, car_minutes, ..
        } = out
        else {
            panic!()
        };
        assert!(zone[1].is_nan());
        assert_eq!(car_minutes, [1, 1]);
    }

    #[test]
    fn enrich_refuses_a_huge_duration() {
        let a = [0i64];
        let b = [(1i64 << 53) + 1];
        let f = [1.0];
        assert!(enrich(col(&a), col(&b), col(&f), col(&f), col(&f)).is_err());
        let b = [1i64 << 53];
        assert!(enrich(col(&a), col(&b), col(&f), col(&f), col(&f)).is_ok());
    }

    #[test]
    fn audit_refuses_null_times() {
        let bits = [0b0u8];
        let v = [0i64];
        let f = [1.0];
        let nulls = Col::new(&v[..], Some(Bits::new(&bits, 0, 1).unwrap()));
        assert!(audit(nulls, col(&v), col(&f), col(&f), col(&f)).is_err());
    }
}
