//! Warm-up's data (AR section 6.1): Arrow batches generated from the function's input schema,
//! honouring the embedder's table statistics where they are given and the type's defaults where
//! they are not, with the key columns generated twice, uniform and skewed.
//!
//! Every value is a function of the seed, the batch's stream and the column's position alone
//! ([`Rng::for_column`]), and the arithmetic that turns statistics into draws is integer or
//! IEEE-exact (sums, products, quotients; no transcendental function), so one seed gives one set
//! of bytes on any host.
//!
//! What each batch turned out to be is measured from the values drawn, not assumed from what was
//! asked: the null fraction, the lengths, and for the key columns the distinct count and the
//! skew (the share of rows in the top 1% of keys, AR section 3.1). Those are what an
//! observation records as its inputs.

use std::sync::Arc;

use moruna_cost::{Column, Data};
use moruna_kernel::arrow::array::{
    ArrayRef, BinaryBuilder, BooleanArray, Date32Array, Date64Array, Decimal128Array, Float32Array,
    Float64Array, Int8Array, Int16Array, Int32Array, Int64Array, LargeBinaryBuilder,
    LargeStringBuilder, ListArray, StringBuilder, StringViewBuilder, TimestampMicrosecondArray,
    TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use moruna_kernel::arrow::buffer::{NullBuffer, OffsetBuffer};
use moruna_kernel::arrow::datatypes::{DataType, Field, SchemaRef, TimeUnit};
use moruna_kernel::arrow::record_batch::{RecordBatch, RecordBatchOptions};
use serde::Serialize;

use crate::check::synth::Rng;

/// Draws are made in millionths: a probability `p` is the draw `below(MILLION) < p * MILLION`.
const MILLION: u64 = 1_000_000;
/// The share of a skewed key column's rows drawn from its hottest keys, in millionths.
pub const HOT_SHARE: u64 = 800_000;
/// The hottest keys of a skewed column: one in this many of the keys a batch can draw (its
/// domain, or its rows when they are fewer), at least one. One in a hundred of the domain would
/// leave most of the domain undrawn and the top 1% of the keys a batch holds a fraction of the
/// hot set, so its skew (AR section 3.1) would be well below the hot share; one in five hundred
/// puts the hot set within the top 1% of the keys a batch holds, whose share is then most of the
/// hot share.
pub const HOT_KEYS_ONE_IN: u64 = 500;
/// The share of a nullable column's values that are null when the statistics give none: one in
/// eight, as `moruna check`'s own batches.
pub const DEFAULT_NULL_FRACTION: f64 = 0.125;
/// String and binary lengths when the statistics give none: uniform from 0 to this many bytes,
/// as `moruna check`'s own batches.
pub const DEFAULT_MAX_BYTES: u64 = 16;
/// List lengths when the statistics give none: uniform from 0 to this many items.
pub const DEFAULT_MAX_ITEMS: u64 = 3;
/// The width an item of a variable-width type is taken to have when a list's statistics, which
/// are in bytes, are turned into a count of items.
const VARIABLE_ITEM_BYTES: f64 = 8.0;
/// The salt that separates warm-up's streams from `moruna check`'s batches of one seed.
const SALT: u64 = 0x5741_524D_5550_0001;

const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const SECONDS_1970_TO_2100: u64 = 4_102_444_800;
const DAYS_1970_TO_2100: u64 = 47_482;

/// How a batch's key columns are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Keys {
    /// Every key of the domain equally likely.
    Uniform,
    /// [`HOT_SHARE`] of the rows on the hottest one in [`HOT_KEYS_ONE_IN`] keys, the rest
    /// uniform over the domain.
    Skewed,
}

/// One batch to generate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spec {
    /// Rows.
    pub rows: u64,
    /// How the key columns are drawn.
    pub keys: Keys,
    /// The key columns' domain, in place of what the statistics or the rows give (the state
    /// pass's increasing distinct counts, AR section 6.5).
    pub distinct: Option<u64>,
    /// Which stream: two specs that differ only here draw different values.
    pub stream: u64,
}

/// A generated batch and what it measured.
#[derive(Clone, Debug)]
pub struct Generated {
    /// The batch.
    pub batch: RecordBatch,
    /// Per column, in schema order: its name, the mean bytes per value (and the 95th percentile
    /// for strings, binary and lists), the null fraction, whether it was generated as a key and,
    /// for a key column or one the statistics gave a distinct count, its distinct count; for a
    /// key column, its skew.
    pub columns: Vec<Column>,
}

/// The lengths of a variable-width column: a share at zero, a share uniform on `lo..=hi`, and
/// the rest at `tail`, all in millionths. Chosen so the mean and the 95th percentile are the
/// ones the statistics give.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Lengths {
    zero: u64,
    body: u64,
    lo: u64,
    hi: u64,
    tail: u64,
}

impl Lengths {
    fn uniform(hi: u64) -> Lengths {
        Lengths {
            zero: 0,
            body: MILLION,
            lo: 0,
            hi,
            tail: hi,
        }
    }

    /// The draws whose mean is `mean` and whose 95th percentile is `p95`, as nearly as a
    /// mixture of a point at zero, a uniform body and a point at the percentile can make them:
    /// 95% of the draws at or below `p95`, 5% at it. A mean above `0.525 * p95` raises the
    /// body's floor; one below it puts the missing mass at zero, down to a mean of 5% of the
    /// percentile, which is the least this family has (the 5% at the percentile). Only `mean`
    /// given: uniform on `0..=2 * mean`. Only `p95`: a mean of half of it. What a batch drew is
    /// measured and recorded either way.
    fn of(mean: Option<f64>, p95: Option<f64>) -> Option<Lengths> {
        let (mean, p95) = match (mean, p95) {
            (None, None) => return None,
            (Some(mean), None) => return Some(Lengths::uniform(round(2.0 * mean))),
            (None, Some(p95)) => (p95 / 2.0, p95),
            (Some(mean), Some(p95)) => (mean, p95.max(mean)),
        };
        let q = round(p95);
        if q == 0 {
            return Some(Lengths::uniform(0));
        }
        let q = q as f64;
        if mean >= 0.525 * q {
            let lo = round((mean - 0.525 * q) / 0.475).min(q as u64);
            return Some(Lengths {
                zero: 0,
                body: 950_000,
                lo,
                hi: q as u64,
                tail: q as u64,
            });
        }
        let zero = (1.05 - 2.0 * mean / q).clamp(0.0, 0.95);
        let zero = round(zero * MILLION as f64).min(950_000);
        Some(Lengths {
            zero,
            body: 950_000 - zero,
            lo: 0,
            hi: q as u64,
            tail: q as u64,
        })
    }

    fn draw(&self, rng: &mut Rng) -> u64 {
        let u = rng.below(MILLION);
        if u < self.zero {
            0
        } else if u < self.zero + self.body {
            self.lo + rng.below(self.hi - self.lo + 1)
        } else {
            self.tail
        }
    }
}

fn round(x: f64) -> u64 {
    if x.is_finite() && x > 0.0 {
        x.round() as u64
    } else {
        0
    }
}

/// How one column is generated.
#[derive(Clone, Debug)]
struct Plan {
    /// Generated as a key column: uniform or skewed over its domain, and measured.
    key: bool,
    /// Nulls, in millionths; zero for a column the schema says cannot be null.
    nulls: u64,
    /// The domain the statistics give (their distinct count), for a key or any other column.
    distinct: Option<u64>,
    /// Lengths of a string, binary or list column, in bytes (items for a list).
    lengths: Lengths,
}

/// Generates the batches of one warm-up.
pub struct Generator {
    schema: SchemaRef,
    plans: Vec<Plan>,
    seed: u64,
    notes: Vec<String>,
}

impl Generator {
    /// A generator for `schema`, following `statistics` where they are given.
    ///
    /// Refused, naming the column: statistics for a column the schema does not have, or twice
    /// for one; a null fraction outside 0 to 1, or above 0 for a column the schema says cannot
    /// be null; a length that is not a finite amount; a distinct count of zero; a list marked as
    /// a key; a type warm-up cannot generate.
    pub fn new(
        schema: &SchemaRef,
        statistics: Option<&Data>,
        seed: u64,
    ) -> Result<Generator, String> {
        let given: &[Column] = statistics.map_or(&[], |d| d.columns.as_slice());
        for (at, column) in given.iter().enumerate() {
            if schema.field_with_name(&column.name).is_err() {
                return Err(format!(
                    "the statistics describe column `{}`, which the declared input does not have",
                    column.name
                ));
            }
            if given[..at].iter().any(|c| c.name == column.name) {
                return Err(format!(
                    "the statistics describe column `{}` twice",
                    column.name
                ));
            }
        }
        let declared_keys = given.iter().any(|c| c.key);
        let mut notes = Vec::new();
        if !declared_keys {
            notes.push(
                "no key column is marked in the statistics, so every column that can be a key \
                 (all but lists) is generated as one, uniform and skewed"
                    .to_string(),
            );
        }
        let mut plans = Vec::with_capacity(schema.fields().len());
        for field in schema.fields() {
            let stats = given.iter().find(|c| &c.name == field.name());
            plans.push(plan(field, stats, declared_keys, &mut notes)?);
        }
        Ok(Generator {
            schema: schema.clone(),
            plans,
            seed,
            notes,
        })
    }

    /// Whether any column is generated as a key, so that a skewed batch differs from a uniform
    /// one.
    pub fn has_keys(&self) -> bool {
        self.plans.iter().any(|p| p.key)
    }

    /// What the generator decided about the statistics, for the report.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// The batch `spec` names.
    pub fn generate(&self, spec: &Spec) -> Result<Generated, String> {
        let rows = usize::try_from(spec.rows)
            .map_err(|_| format!("{} rows do not fit in memory", spec.rows))?;
        let stream = spec.stream.wrapping_mul(4).wrapping_add(match spec.keys {
            Keys::Uniform => 0,
            Keys::Skewed => 1,
        });
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(self.plans.len());
        let mut columns = Vec::with_capacity(self.plans.len());
        for (at, (field, plan)) in self.schema.fields().iter().zip(&self.plans).enumerate() {
            let mut rng = Rng::for_column(self.seed ^ SALT, stream as usize, at);
            let (array, column) = generate_column(field, plan, rows, spec, &mut rng)
                .map_err(|e| format!("column `{}`: {e}", field.name()))?;
            arrays.push(array);
            columns.push(column);
        }
        let batch = RecordBatch::try_new_with_options(
            self.schema.clone(),
            arrays,
            &RecordBatchOptions::new().with_row_count(Some(rows)),
        )
        .map_err(|e| format!("building a warm-up batch: {e}"))?;
        Ok(Generated { batch, columns })
    }
}

fn plan(
    field: &Field,
    stats: Option<&Column>,
    declared_keys: bool,
    notes: &mut Vec<String>,
) -> Result<Plan, String> {
    let name = field.name();
    let dt = field.data_type();
    let list = matches!(dt, DataType::List(_));
    if !generatable(dt) {
        return Err(format!(
            "column `{name}`: warm-up cannot generate values of type {}",
            moruna_kernel::declare::type_name(dt)
        ));
    }
    let key = if declared_keys {
        stats.is_some_and(|s| s.key)
    } else {
        !list
    };
    if key && list {
        return Err(format!(
            "column `{name}`: the statistics mark a list column as a key, and warm-up draws keys \
             only of scalar types"
        ));
    }
    let null_fraction = match stats.and_then(|s| s.null_fraction) {
        Some(f) if !(0.0..=1.0).contains(&f) => {
            return Err(format!(
                "column `{name}`: the statistics give a null fraction of {f}, which is not \
                 between 0 and 1"
            ));
        }
        Some(f) if f > 0.0 && !field.is_nullable() => {
            return Err(format!(
                "column `{name}`: the statistics give a null fraction of {f}, and the declared \
                 input says the column cannot be null"
            ));
        }
        Some(f) => f,
        None if field.is_nullable() => DEFAULT_NULL_FRACTION,
        None => 0.0,
    };
    let distinct = match stats.and_then(|s| s.distinct) {
        Some(0) => {
            return Err(format!(
                "column `{name}`: the statistics give a distinct count of 0"
            ));
        }
        other => other,
    };
    for (what, value) in [
        ("mean length", stats.and_then(|s| s.bytes_mean)),
        ("95th percentile length", stats.and_then(|s| s.bytes_p95)),
    ] {
        if let Some(v) = value
            && !(v.is_finite() && v >= 0.0)
        {
            return Err(format!(
                "column `{name}`: the statistics give a {what} of {v}, which is not an amount"
            ));
        }
    }
    let (mean, p95) = (
        stats.and_then(|s| s.bytes_mean),
        stats.and_then(|s| s.bytes_p95),
    );
    let lengths = match dt {
        DataType::Utf8
        | DataType::LargeUtf8
        | DataType::Utf8View
        | DataType::Binary
        | DataType::LargeBinary => {
            Lengths::of(mean, p95).unwrap_or(Lengths::uniform(DEFAULT_MAX_BYTES))
        }
        DataType::List(item) => {
            let width = item
                .data_type()
                .primitive_width()
                .map_or(VARIABLE_ITEM_BYTES, |w| w as f64);
            Lengths::of(mean.map(|m| m / width), p95.map(|q| q / width))
                .unwrap_or(Lengths::uniform(DEFAULT_MAX_ITEMS))
        }
        _ => {
            if mean.is_some() || p95.is_some() {
                notes.push(format!(
                    "the statistics give lengths for `{name}`, whose type {} has a fixed width; \
                     they do not apply",
                    moruna_kernel::declare::type_name(dt)
                ));
            }
            Lengths::uniform(0)
        }
    };
    Ok(Plan {
        key,
        nulls: round(null_fraction * MILLION as f64).min(MILLION),
        distinct,
        lengths,
    })
}

fn generatable(dt: &DataType) -> bool {
    match dt {
        DataType::List(item) => {
            generatable(item.data_type()) && !matches!(item.data_type(), DataType::List(_))
        }
        _ => cardinality(dt).is_some() || unbounded(dt),
    }
}

/// Types whose values are not drawn from a bounded domain.
fn unbounded(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Int64
            | DataType::UInt64
            | DataType::Utf8
            | DataType::LargeUtf8
            | DataType::Utf8View
            | DataType::Binary
            | DataType::LargeBinary
    )
}

/// How many distinct values warm-up draws a type from, when that is fewer than 2^64: what a
/// domain is capped at. `None` for an unbounded type or one warm-up does not generate.
fn cardinality(dt: &DataType) -> Option<u64> {
    Some(match dt {
        DataType::Boolean => 2,
        DataType::Int8 | DataType::UInt8 => 1 << 8,
        DataType::Int16 | DataType::UInt16 => 1 << 16,
        DataType::Int32 | DataType::UInt32 => 1 << 32,
        DataType::Float32 => 1 << 24,
        DataType::Float64 => 1 << 53,
        DataType::Date32 | DataType::Date64 => DAYS_1970_TO_2100,
        DataType::Timestamp(_, _) => SECONDS_1970_TO_2100,
        DataType::Decimal128(p, _) => 10u64.pow(u32::from((*p).clamp(1, 18))),
        _ => return None,
    })
}

/// The draws of one column: validity, the index each value is made from, and its length.
struct Draws {
    valid: Vec<bool>,
    index: Vec<u64>,
    len: Vec<u64>,
    /// The domain the indexes were drawn from, when bounded by a distinct count.
    domain: Option<u64>,
    /// What a value of a column with a domain is made from beside its index, so that one index
    /// is always one value (its length and its filler are the index's, not the row's).
    value_seed: u64,
}

/// Whether values of `dt` have a length that is drawn.
fn has_lengths(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Utf8
            | DataType::LargeUtf8
            | DataType::Utf8View
            | DataType::Binary
            | DataType::LargeBinary
            | DataType::List(_)
    )
}

/// The stream a value of index `index` draws its length (`part` 0) or its filler (`part` 1)
/// from, in a column with a domain.
fn keyed(value_seed: u64, index: u64, part: usize) -> Rng {
    Rng::for_column(
        value_seed ^ index.wrapping_mul(0xD6E8_FEB8_6659_FD93),
        part,
        0,
    )
}

fn draw(dt: &DataType, plan: &Plan, rows: usize, spec: &Spec, rng: &mut Rng) -> Draws {
    let card = cardinality(dt);
    let cap = |d: u64| card.map_or(d, |c| d.min(c)).max(1);
    let domain = if plan.key {
        Some(cap(spec
            .distinct
            .or(plan.distinct)
            .unwrap_or(spec.rows.max(1))))
    } else {
        plan.distinct.map(cap)
    };
    let hot = domain.map(|d| (d.min(spec.rows) / HOT_KEYS_ONE_IN).max(1));
    let variable = has_lengths(dt);
    let value_seed = rng.next_u64();
    let mut out = Draws {
        valid: Vec::with_capacity(rows),
        index: Vec::with_capacity(rows),
        len: Vec::with_capacity(if variable { rows } else { 0 }),
        domain,
        value_seed,
    };
    for _ in 0..rows {
        let null = plan.nulls > 0 && rng.below(MILLION) < plan.nulls;
        out.valid.push(!null);
        if null {
            out.index.push(0);
            if variable {
                out.len.push(0);
            }
            continue;
        }
        let index = match (domain, plan.key, spec.keys) {
            (Some(d), true, Keys::Skewed) => {
                let hot = hot.unwrap_or(1);
                if rng.below(MILLION) < HOT_SHARE {
                    rng.below(hot)
                } else {
                    rng.below(d)
                }
            }
            (Some(d), _, _) => rng.below(d),
            (None, _, _) => {
                let x = rng.next_u64();
                card.map_or(x, |c| x % c)
            }
        };
        out.index.push(index);
        if variable {
            let len = match domain {
                Some(_) => plan.lengths.draw(&mut keyed(value_seed, index, 0)),
                None => plan.lengths.draw(rng),
            };
            out.len.push(len);
        }
    }
    out
}

fn generate_column(
    field: &Field,
    plan: &Plan,
    rows: usize,
    spec: &Spec,
    rng: &mut Rng,
) -> Result<(ArrayRef, Column), String> {
    let dt = field.data_type();
    let draws = draw(dt, plan, rows, spec, rng);
    let nulls = draws.valid.iter().filter(|v| !**v).count();
    let mut column = Column {
        name: field.name().clone(),
        null_fraction: Some(if rows == 0 {
            0.0
        } else {
            nulls as f64 / rows as f64
        }),
        key: plan.key,
        ..Column::default()
    };
    if draws.domain.is_some() {
        let (distinct, skew) = distinct_and_skew(&draws);
        column.distinct = Some(distinct);
        if plan.key {
            column.skew = Some(skew);
        }
    }
    let (array, lengths) = build(dt, &draws, rng)?;
    match lengths {
        Some(lengths) => {
            let (mean, p95) = mean_and_p95(lengths);
            column.bytes_mean = Some(mean);
            column.bytes_p95 = Some(p95);
        }
        None => {
            column.bytes_mean = dt.primitive_width().map(|w| w as f64).or(match dt {
                DataType::Boolean => Some(0.125),
                _ => None,
            });
        }
    }
    Ok((array, column))
}

/// The distinct indexes among the valid rows, and the share of the valid rows on the top 1% of
/// them (at least one), the skew of AR section 3.1.
fn distinct_and_skew(draws: &Draws) -> (u64, f64) {
    let mut seen: Vec<u64> = draws
        .index
        .iter()
        .zip(&draws.valid)
        .filter(|(_, v)| **v)
        .map(|(i, _)| *i)
        .collect();
    if seen.is_empty() {
        return (0, 0.0);
    }
    let total = seen.len();
    seen.sort_unstable();
    let mut counts = Vec::new();
    let mut run = 1u64;
    for pair in seen.windows(2) {
        if pair[0] == pair[1] {
            run += 1;
        } else {
            counts.push(run);
            run = 1;
        }
    }
    counts.push(run);
    let distinct = counts.len() as u64;
    counts.sort_unstable_by(|a, b| b.cmp(a));
    let top = distinct.div_ceil(100) as usize;
    let on_top: u64 = counts[..top].iter().sum();
    (distinct, on_top as f64 / total as f64)
}

/// The mean and the nearest-rank 95th percentile of the valid values' lengths in bytes.
fn mean_and_p95(mut lengths: Vec<u64>) -> (f64, f64) {
    if lengths.is_empty() {
        return (0.0, 0.0);
    }
    let mean = lengths.iter().sum::<u64>() as f64 / lengths.len() as f64;
    lengths.sort_unstable();
    let rank = ((lengths.len() as f64 - 1.0) * 0.95).round() as usize;
    (mean, lengths[rank.min(lengths.len() - 1)] as f64)
}

/// The characters (or bytes) a value of index `index` in a domain of `domain` starts with: the
/// index in base 62 (or big-endian bytes), padded to the width of the domain's largest index,
/// so that two indexes never make one value whatever is appended.
fn digits(index: u64, domain: u64) -> Vec<u8> {
    let mut width = 1;
    let mut top = domain.saturating_sub(1) / 62;
    while top > 0 {
        width += 1;
        top /= 62;
    }
    let mut out = vec![b'0'; width];
    let mut i = index;
    for slot in out.iter_mut().rev() {
        *slot = DIGITS[(i % 62) as usize];
        i /= 62;
    }
    out
}

fn prefix_bytes(index: u64, domain: u64) -> Vec<u8> {
    let width = (64 - domain.saturating_sub(1).leading_zeros())
        .div_ceil(8)
        .max(1) as usize;
    index.to_be_bytes()[8 - width..].to_vec()
}

/// A string value: random alphanumerics to the drawn length; in a column with a domain, the
/// index's digits first and the filler the index's own, so one index is one value.
fn text(d: &Draws, index: u64, len: u64, rng: &mut Rng, out: &mut String) {
    out.clear();
    let mut own;
    let filler = match d.domain {
        Some(domain) => {
            out.extend(digits(index, domain).into_iter().map(char::from));
            own = keyed(d.value_seed, index, 1);
            &mut own
        }
        None => rng,
    };
    while (out.len() as u64) < len {
        out.push(ALNUM[filler.below(ALNUM.len() as u64) as usize] as char);
    }
}

/// A binary value, as [`text`] with bytes.
fn blob(d: &Draws, index: u64, len: u64, rng: &mut Rng, out: &mut Vec<u8>) {
    out.clear();
    let mut own;
    let filler = match d.domain {
        Some(domain) => {
            out.extend(prefix_bytes(index, domain));
            own = keyed(d.value_seed, index, 1);
            &mut own
        }
        None => rng,
    };
    while (out.len() as u64) < len {
        out.push(filler.next_u64() as u8);
    }
}

fn scale(unit: &TimeUnit) -> i64 {
    match unit {
        TimeUnit::Second => 1,
        TimeUnit::Millisecond => 1_000,
        TimeUnit::Microsecond => 1_000_000,
        TimeUnit::Nanosecond => 1_000_000_000,
    }
}

/// The array of `dt` the draws make, and for a variable-width type the valid values' lengths in
/// bytes.
fn build(dt: &DataType, d: &Draws, rng: &mut Rng) -> Result<(ArrayRef, Option<Vec<u64>>), String> {
    fn fixed<T>(d: &Draws, f: impl Fn(u64) -> T) -> Vec<Option<T>> {
        d.index
            .iter()
            .zip(&d.valid)
            .map(|(i, v)| v.then(|| f(*i)))
            .collect()
    }
    macro_rules! strings {
        ($builder:ty) => {{
            let mut builder = <$builder>::with_capacity(d.index.len(), 0);
            let mut lengths = Vec::with_capacity(d.index.len());
            let mut value = String::new();
            for ((i, v), len) in d.index.iter().zip(&d.valid).zip(&d.len) {
                if *v {
                    text(d, *i, *len, rng, &mut value);
                    lengths.push(value.len() as u64);
                    builder.append_value(&value);
                } else {
                    builder.append_null();
                }
            }
            (Arc::new(builder.finish()) as ArrayRef, Some(lengths))
        }};
    }
    macro_rules! blobs {
        ($builder:ty) => {{
            let mut builder = <$builder>::with_capacity(d.index.len(), 0);
            let mut lengths = Vec::with_capacity(d.index.len());
            let mut value = Vec::new();
            for ((i, v), len) in d.index.iter().zip(&d.valid).zip(&d.len) {
                if *v {
                    blob(d, *i, *len, rng, &mut value);
                    lengths.push(value.len() as u64);
                    builder.append_value(&value);
                } else {
                    builder.append_null();
                }
            }
            (Arc::new(builder.finish()) as ArrayRef, Some(lengths))
        }};
    }
    Ok(match dt {
        DataType::Boolean => (Arc::new(BooleanArray::from(fixed(d, |i| i & 1 == 1))), None),
        DataType::Int8 => (Arc::new(Int8Array::from(fixed(d, |i| i as i8))), None),
        DataType::Int16 => (Arc::new(Int16Array::from(fixed(d, |i| i as i16))), None),
        DataType::Int32 => (Arc::new(Int32Array::from(fixed(d, |i| i as i32))), None),
        DataType::Int64 => (Arc::new(Int64Array::from(fixed(d, |i| i as i64))), None),
        DataType::UInt8 => (Arc::new(UInt8Array::from(fixed(d, |i| i as u8))), None),
        DataType::UInt16 => (Arc::new(UInt16Array::from(fixed(d, |i| i as u16))), None),
        DataType::UInt32 => (Arc::new(UInt32Array::from(fixed(d, |i| i as u32))), None),
        DataType::UInt64 => (Arc::new(UInt64Array::from(fixed(d, |i| i))), None),
        DataType::Float32 => (Arc::new(Float32Array::from(fixed(d, |i| i as f32))), None),
        DataType::Float64 => (Arc::new(Float64Array::from(fixed(d, |i| i as f64))), None),
        DataType::Date32 => (Arc::new(Date32Array::from(fixed(d, |i| i as i32))), None),
        DataType::Date64 => (
            Arc::new(Date64Array::from(fixed(d, |i| i as i64 * 86_400_000))),
            None,
        ),
        DataType::Timestamp(unit, tz) => {
            let s = scale(unit);
            let v = fixed(d, |i| i as i64 * s);
            let array: ArrayRef = match unit {
                TimeUnit::Second => {
                    Arc::new(TimestampSecondArray::from(v).with_timezone_opt(tz.clone()))
                }
                TimeUnit::Millisecond => {
                    Arc::new(TimestampMillisecondArray::from(v).with_timezone_opt(tz.clone()))
                }
                TimeUnit::Microsecond => {
                    Arc::new(TimestampMicrosecondArray::from(v).with_timezone_opt(tz.clone()))
                }
                TimeUnit::Nanosecond => {
                    Arc::new(TimestampNanosecondArray::from(v).with_timezone_opt(tz.clone()))
                }
            };
            (array, None)
        }
        DataType::Decimal128(p, s) => (
            Arc::new(
                Decimal128Array::from(fixed(d, i128::from))
                    .with_precision_and_scale(*p, *s)
                    .map_err(|e| e.to_string())?,
            ),
            None,
        ),
        DataType::Utf8 => strings!(StringBuilder),
        DataType::LargeUtf8 => strings!(LargeStringBuilder),
        DataType::Utf8View => {
            let mut builder = StringViewBuilder::with_capacity(d.index.len());
            let mut lengths = Vec::with_capacity(d.index.len());
            let mut value = String::new();
            for ((i, v), len) in d.index.iter().zip(&d.valid).zip(&d.len) {
                if *v {
                    text(d, *i, *len, rng, &mut value);
                    lengths.push(value.len() as u64);
                    builder.append_value(&value);
                } else {
                    builder.append_null();
                }
            }
            (Arc::new(builder.finish()) as ArrayRef, Some(lengths))
        }
        DataType::Binary => blobs!(BinaryBuilder),
        DataType::LargeBinary => blobs!(LargeBinaryBuilder),
        DataType::List(item) => list(item, d, rng)?,
        other => {
            return Err(format!(
                "warm-up cannot generate values of type {}",
                moruna_kernel::declare::type_name(other)
            ));
        }
    })
}

/// A list column: each valid row's drawn length in items, the items generated from the item
/// type's defaults; its lengths are reported in bytes (items times the item's width).
fn list(
    item: &Arc<Field>,
    d: &Draws,
    rng: &mut Rng,
) -> Result<(ArrayRef, Option<Vec<u64>>), String> {
    let lengths: Vec<usize> = d
        .len
        .iter()
        .zip(&d.valid)
        .map(|(len, v)| if *v { *len as usize } else { 0 })
        .collect();
    let total: usize = lengths.iter().sum();
    let item_plan = Plan {
        key: false,
        nulls: if item.is_nullable() {
            round(DEFAULT_NULL_FRACTION * MILLION as f64)
        } else {
            0
        },
        distinct: None,
        lengths: Lengths::uniform(DEFAULT_MAX_BYTES),
    };
    let spec = Spec {
        rows: total as u64,
        keys: Keys::Uniform,
        distinct: None,
        stream: 0,
    };
    let item_draws = draw(item.data_type(), &item_plan, total, &spec, rng);
    let (values, _) = build(item.data_type(), &item_draws, rng)?;
    let width = item
        .data_type()
        .primitive_width()
        .map_or(VARIABLE_ITEM_BYTES, |w| w as f64);
    let bytes: Vec<u64> = lengths
        .iter()
        .zip(&d.valid)
        .filter(|(_, v)| **v)
        .map(|(len, _)| round(*len as f64 * width))
        .collect();
    let nulls = if d.valid.iter().all(|v| *v) {
        None
    } else {
        Some(NullBuffer::from(d.valid.clone()))
    };
    let array = ListArray::try_new(
        item.clone(),
        OffsetBuffer::from_lengths(lengths),
        values,
        nulls,
    )
    .map_err(|e| e.to_string())?;
    Ok((Arc::new(array) as ArrayRef, Some(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::arrow::array::{Array, AsArray};
    use moruna_kernel::arrow::datatypes::{Int64Type, Schema};

    fn schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("k", DataType::Int64, false),
            Field::new("s", DataType::Utf8, true),
            Field::new(
                "l",
                DataType::List(Arc::new(Field::new("item", DataType::Int32, true))),
                true,
            ),
        ]))
    }

    fn spec(rows: u64, keys: Keys) -> Spec {
        Spec {
            rows,
            keys,
            distinct: None,
            stream: 3,
        }
    }

    fn stats() -> Data {
        Data {
            columns: vec![
                Column {
                    name: "k".into(),
                    distinct: Some(1_000),
                    key: true,
                    ..Column::default()
                },
                Column {
                    name: "s".into(),
                    bytes_mean: Some(20.0),
                    bytes_p95: Some(30.0),
                    null_fraction: Some(0.3),
                    ..Column::default()
                },
                Column {
                    name: "l".into(),
                    bytes_mean: Some(40.0),
                    ..Column::default()
                },
            ],
            ..Data::default()
        }
    }

    #[test]
    fn a_seed_fixes_every_byte_and_another_changes_them() {
        let g = Generator::new(&schema(), Some(&stats()), 9).expect("generator");
        let a = g.generate(&spec(5_000, Keys::Skewed)).expect("a");
        let b = g.generate(&spec(5_000, Keys::Skewed)).expect("b");
        assert_eq!(a.batch, b.batch);
        assert_eq!(a.columns, b.columns);
        let other = Generator::new(&schema(), Some(&stats()), 10).expect("generator");
        assert_ne!(
            other.generate(&spec(5_000, Keys::Skewed)).expect("c").batch,
            a.batch
        );
        let mut stream = spec(5_000, Keys::Skewed);
        stream.stream = 4;
        assert_ne!(g.generate(&stream).expect("d").batch, a.batch);
    }

    #[test]
    fn statistics_are_honoured_and_measured() {
        let g = Generator::new(&schema(), Some(&stats()), 1).expect("generator");
        assert!(g.notes().is_empty(), "{:?}", g.notes());
        let out = g.generate(&spec(100_000, Keys::Uniform)).expect("batch");
        let [k, s, l] = &out.columns[..] else {
            panic!("three columns")
        };
        assert!(k.key && !s.key && !l.key);
        assert_eq!(k.distinct, Some(1_000), "a thousand keys over 100,000 rows");
        assert!(k.skew.expect("skew") < 0.02, "uniform: {k:?}");
        assert_eq!(k.null_fraction, Some(0.0));
        let nulls = s.null_fraction.expect("nulls");
        assert!((nulls - 0.3).abs() < 0.01, "{nulls}");
        let mean = s.bytes_mean.expect("mean");
        assert!((mean - 20.0).abs() < 0.5, "{mean}");
        assert_eq!(s.bytes_p95, Some(30.0));
        let items = l.bytes_mean.expect("list bytes");
        assert!((items - 40.0).abs() < 1.0, "{items}");
        assert_eq!(out.batch.column(1).null_count() as f64 / 100_000.0, nulls);
    }

    #[test]
    fn skewed_keys_are_skewed_over_the_same_domain() {
        let g = Generator::new(&schema(), Some(&stats()), 1).expect("generator");
        let uniform = g.generate(&spec(100_000, Keys::Uniform)).expect("u");
        let skewed = g.generate(&spec(100_000, Keys::Skewed)).expect("s");
        let (u, s) = (&uniform.columns[0], &skewed.columns[0]);
        assert!(s.skew.expect("skew") > 0.75, "{s:?}");
        assert!(u.skew.expect("skew") < 0.02, "{u:?}");
        assert_eq!(s.distinct, u.distinct, "the same domain");
        let keys = skewed.batch.column(0).as_primitive::<Int64Type>();
        assert!(keys.values().iter().all(|k| (0..1_000).contains(k)));
    }

    #[test]
    fn without_statistics_every_scalar_column_is_a_key_and_types_default() {
        let g = Generator::new(&schema(), None, 1).expect("generator");
        assert!(g.has_keys());
        assert_eq!(g.notes().len(), 1);
        let out = g.generate(&spec(10_000, Keys::Uniform)).expect("batch");
        assert!(out.columns[0].key && out.columns[1].key && !out.columns[2].key);
        let mean = out.columns[1].bytes_mean.expect("mean");
        assert!(
            mean > 4.0 && mean < 12.0,
            "default lengths 0 to 16 bytes, at least the key's digits: {mean}"
        );
        let nulls = out.columns[1].null_fraction.expect("nulls");
        assert!((nulls - DEFAULT_NULL_FRACTION).abs() < 0.02, "{nulls}");
        assert_eq!(out.columns[0].null_fraction, Some(0.0), "not nullable");
        // The domain is the rows: distinct strings are distinct values.
        let strings = out.batch.column(1).as_string::<i32>();
        let mut seen: Vec<&str> = strings.iter().flatten().collect();
        let n = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len() as u64,
            out.columns[1].distinct.expect("distinct")
        );
        assert!(seen.len() < n, "some keys repeat");
    }

    #[test]
    fn a_distinct_override_sets_the_key_domain() {
        let g = Generator::new(&schema(), Some(&stats()), 1).expect("generator");
        let mut s = spec(50_000, Keys::Uniform);
        s.distinct = Some(10);
        let out = g.generate(&s).expect("batch");
        assert_eq!(out.columns[0].distinct, Some(10));
        assert_eq!(out.batch.num_rows(), 50_000);
        assert!(
            g.generate(&spec(0, Keys::Skewed))
                .expect("empty")
                .batch
                .num_rows()
                == 0
        );
    }

    #[test]
    fn refusals_name_the_column() {
        let s = schema();
        let refuse = |columns: Vec<Column>| {
            Generator::new(
                &s,
                Some(&Data {
                    columns,
                    ..Data::default()
                }),
                0,
            )
            .err()
            .expect("refused")
        };
        let ghost = refuse(vec![Column {
            name: "ghost".into(),
            ..Column::default()
        }]);
        assert!(ghost.contains("`ghost`"), "{ghost}");
        let twice = refuse(vec![
            Column {
                name: "k".into(),
                ..Column::default()
            },
            Column {
                name: "k".into(),
                ..Column::default()
            },
        ]);
        assert!(twice.contains("twice"), "{twice}");
        let fraction = refuse(vec![Column {
            name: "s".into(),
            null_fraction: Some(1.5),
            ..Column::default()
        }]);
        assert!(fraction.contains("between 0 and 1"), "{fraction}");
        let not_null = refuse(vec![Column {
            name: "k".into(),
            null_fraction: Some(0.1),
            ..Column::default()
        }]);
        assert!(not_null.contains("cannot be null"), "{not_null}");
        let zero = refuse(vec![Column {
            name: "k".into(),
            distinct: Some(0),
            ..Column::default()
        }]);
        assert!(zero.contains("distinct count of 0"), "{zero}");
        let length = refuse(vec![Column {
            name: "s".into(),
            bytes_mean: Some(f64::NAN),
            ..Column::default()
        }]);
        assert!(length.contains("not an amount"), "{length}");
        let list_key = refuse(vec![Column {
            name: "l".into(),
            key: true,
            ..Column::default()
        }]);
        assert!(list_key.contains("list column as a key"), "{list_key}");
        let odd = Arc::new(Schema::new(vec![Field::new(
            "t",
            DataType::Time32(TimeUnit::Second),
            true,
        )]));
        let err = Generator::new(&odd, None, 0).err().expect("refused");
        assert!(err.contains("column `t`"), "{err}");
    }

    #[test]
    fn lengths_meet_their_mean_and_percentile() {
        let mut rng = Rng::for_column(1, 2, 3);
        for (mean, p95) in [(10.0, 10.0), (20.0, 30.0), (5.0, 40.0), (12.0, 100.0)] {
            let lengths = Lengths::of(Some(mean), Some(p95)).expect("lengths");
            let draws: Vec<u64> = (0..200_000).map(|_| lengths.draw(&mut rng)).collect();
            let (m, q) = mean_and_p95(draws);
            assert!(
                (m - mean).abs() <= mean * 0.05 + 0.6,
                "{mean}/{p95}: mean {m}"
            );
            assert!((q - p95).abs() <= 1.0, "{mean}/{p95}: p95 {q}");
        }
        assert_eq!(Lengths::of(None, None), None);
        assert_eq!(Lengths::of(Some(4.0), None), Some(Lengths::uniform(8)));
        assert_eq!(Lengths::of(Some(0.0), Some(0.0)), Some(Lengths::uniform(0)));
        let only_p95 = Lengths::of(None, Some(10.0)).expect("lengths");
        assert_eq!(only_p95.hi, 10);
    }

    #[test]
    fn every_scalar_type_generates_as_a_key() {
        let tz: Arc<str> = Arc::from("UTC");
        let types = vec![
            DataType::Boolean,
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::UInt8,
            DataType::UInt16,
            DataType::UInt32,
            DataType::UInt64,
            DataType::Float32,
            DataType::Float64,
            DataType::Utf8,
            DataType::LargeUtf8,
            DataType::Utf8View,
            DataType::Binary,
            DataType::LargeBinary,
            DataType::Date32,
            DataType::Date64,
            DataType::Timestamp(TimeUnit::Second, None),
            DataType::Timestamp(TimeUnit::Millisecond, None),
            DataType::Timestamp(TimeUnit::Microsecond, Some(tz)),
            DataType::Timestamp(TimeUnit::Nanosecond, None),
            DataType::Decimal128(10, 2),
        ];
        let fields: Vec<Field> = types
            .iter()
            .enumerate()
            .map(|(i, t)| Field::new(format!("c{i}"), t.clone(), true))
            .collect();
        let schema = Arc::new(Schema::new(fields));
        let g = Generator::new(&schema, None, 5).expect("generator");
        let out = g.generate(&spec(2_000, Keys::Skewed)).expect("batch");
        let uniform = g.generate(&spec(2_000, Keys::Uniform)).expect("batch");
        for ((column, flat), t) in out.columns.iter().zip(&uniform.columns).zip(&types) {
            assert!(column.key, "{t}");
            let distinct = column.distinct.expect("distinct");
            assert!(distinct >= 1, "{t}");
            if *t == DataType::Boolean {
                assert!(distinct <= 2);
            }
            assert!(
                column.skew.expect("skew") > 1.5 * flat.skew.expect("skew"),
                "{t}: {column:?} against {flat:?}"
            );
            assert!(column.bytes_mean.is_some(), "{t}");
        }
        // Distinct indexes are distinct values for every type.
        for (at, column) in out.columns.iter().enumerate() {
            let array = out.batch.column(at);
            let values: std::collections::BTreeSet<String> = (0..array.len())
                .filter(|i| array.is_valid(*i))
                .map(|i| {
                    moruna_kernel::arrow::util::display::array_value_to_string(array, i)
                        .expect("displays")
                })
                .collect();
            assert_eq!(
                values.len() as u64,
                column.distinct.expect("distinct"),
                "{}",
                types[at]
            );
        }
    }
}
