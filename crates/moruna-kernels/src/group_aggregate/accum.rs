//! The accumulators: one per aggregate, a vector indexed by group. Each takes rows (`update`)
//! or partial results (`merge`), and gives the partial result or the final value of the groups
//! named, in the order named (spec `group_aggregate.md`: state shape, the merge rule).

use std::sync::Arc;

use moruna_kernel::arrow::array::{
    Array, ArrayRef, AsArray, BinaryBuilder, Decimal128Array, Float64Array, Int64Array,
    UInt64Array, make_array,
};
use moruna_kernel::arrow::compute::cast;
use moruna_kernel::arrow::datatypes::{
    DataType, Decimal128Type, Float64Type, Int64Type, UInt64Type,
};
use moruna_kernel::{MorunaError, Result};

use super::binned::BinnedSum;
use super::exact::ExactSum;
use super::groups::arrow_err;
use super::{AggOp, failed};

/// The physical class of a value column: what `sum`, `mean`, `min` and `max` compute in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// Signed integers, and temporal types (for `min` and `max`), as `i64`.
    Signed,
    /// Unsigned integers, as `u64`.
    Unsigned,
    /// Floats, as `f64`.
    Float,
}

/// The class of a value column of type `dt` under `op`, or `None` when `op` does not take it.
pub(crate) fn class_of(op: AggOp, dt: &DataType) -> Option<Class> {
    use DataType::*;
    let numeric = match dt {
        Int8 | Int16 | Int32 | Int64 => Some(Class::Signed),
        UInt8 | UInt16 | UInt32 | UInt64 => Some(Class::Unsigned),
        Float16 | Float32 | Float64 => Some(Class::Float),
        _ => None,
    };
    let temporal = matches!(
        dt,
        Date32 | Date64 | Time32(_) | Time64(_) | Timestamp(..) | Duration(_)
    );
    match op {
        AggOp::Count => Some(Class::Signed),
        AggOp::Sum | AggOp::Mean => numeric,
        AggOp::Min | AggOp::Max => numeric.or(temporal.then_some(Class::Signed)),
    }
}

/// The physical primitive type that carries `dt`'s values unchanged.
fn carrier(dt: &DataType) -> Option<DataType> {
    use DataType::*;
    match dt {
        Date32 | Time32(_) => Some(Int32),
        Date64 | Time64(_) | Timestamp(..) | Duration(_) => Some(Int64),
        _ => None,
    }
}

/// `arr` as Int64, UInt64 or Float64, exactly.
pub(crate) fn physical(arr: &ArrayRef, class: Class) -> Result<ArrayRef> {
    let arr = match carrier(arr.data_type()) {
        Some(carrier) => make_array(
            arr.to_data()
                .into_builder()
                .data_type(carrier)
                .build()
                .map_err(arrow_err)?,
        ),
        None => Arc::clone(arr),
    };
    let target = match class {
        Class::Signed => DataType::Int64,
        Class::Unsigned => DataType::UInt64,
        Class::Float => DataType::Float64,
    };
    cast(&arr, &target).map_err(arrow_err)
}

/// Physical values back as `target`, the type they came from.
pub(crate) fn from_physical(values: &ArrayRef, target: &DataType) -> Result<ArrayRef> {
    match carrier(target) {
        Some(carrier) => {
            let carried = cast(values, &carrier).map_err(arrow_err)?;
            Ok(make_array(
                carried
                    .to_data()
                    .into_builder()
                    .data_type(target.clone())
                    .build()
                    .map_err(arrow_err)?,
            ))
        }
        None => cast(values, target).map_err(arrow_err),
    }
}

/// Floats in IEEE total order, NaN canonical first, so `−0.0 < +0.0` and NaN is greatest.
#[inline]
fn total_key(x: f64) -> i64 {
    let x = if x.is_nan() { f64::NAN } else { x };
    let b = x.to_bits() as i64;
    b ^ (((b >> 63) as u64) >> 1) as i64
}

/// A sum that does not fit its type.
#[derive(Debug)]
pub(crate) struct Overflow {
    /// The group's position in the output.
    pub row: usize,
    /// The exact total.
    pub total: i128,
}

/// One aggregate's state.
pub(crate) enum Acc {
    /// `count` of rows, kept in the groups' row counts.
    Rows,
    /// `count` of a column's non-null values, or of rows in a merge.
    Count(Vec<i64>),
    /// `sum` of integers, exactly in `i128`.
    SumInt {
        sum: Vec<i128>,
        seen: Vec<bool>,
        unsigned: bool,
    },
    /// `sum` of floats, exactly.
    SumFloat { sum: Vec<ExactSum>, seen: Vec<bool> },
    /// `min` or `max` of signed integers and temporal values.
    ExtInt {
        v: Vec<i64>,
        seen: Vec<bool>,
        max: bool,
    },
    /// `min` or `max` of unsigned integers.
    ExtUInt {
        v: Vec<u64>,
        seen: Vec<bool>,
        max: bool,
    },
    /// `min` or `max` of floats, in total order.
    ExtFloat {
        v: Vec<f64>,
        seen: Vec<bool>,
        max: bool,
    },
    /// `mean` of integers: an exact sum and a count.
    MeanInt { sum: Vec<i128>, n: Vec<i64> },
    /// `mean` of floats: an exact sum and a count.
    MeanFloat { sum: Vec<ExactSum>, n: Vec<i64> },
    /// `sum` of floats, binned: for measurement only, single mode.
    SumBinned {
        sum: Vec<BinnedSum>,
        seen: Vec<bool>,
    },
    /// `sum` of floats, plainly in order: for measurement only, single mode.
    SumPlain { sum: Vec<f64>, seen: Vec<bool> },
}

macro_rules! each_valid {
    ($arr:expr, $ids:expr, |$g:ident, $v:ident| $body:block) => {{
        let arr = $arr;
        let values = arr.values();
        match arr.nulls().filter(|n| n.null_count() > 0) {
            None => {
                for (&$g, &$v) in $ids.iter().zip(values.iter()) {
                    let $g = $g as usize;
                    $body
                }
            }
            Some(nulls) => {
                for (row, (&$g, &$v)) in $ids.iter().zip(values.iter()).enumerate() {
                    if nulls.is_valid(row) {
                        let $g = $g as usize;
                        $body
                    }
                }
            }
        }
    }};
}

impl Acc {
    /// An empty accumulator of `op` over values of `class`; `rows` when `count` of rows is
    /// kept in the groups' row counts.
    pub(crate) fn new(op: AggOp, class: Class, rows: bool) -> Acc {
        match (op, class) {
            (AggOp::Count, _) if rows => Acc::Rows,
            (AggOp::Count, _) => Acc::Count(Vec::new()),
            (AggOp::Sum, Class::Float) => Acc::SumFloat {
                sum: Vec::new(),
                seen: Vec::new(),
            },
            (AggOp::Sum, c) => Acc::SumInt {
                sum: Vec::new(),
                seen: Vec::new(),
                unsigned: c == Class::Unsigned,
            },
            (AggOp::Min | AggOp::Max, c) => {
                let max = op == AggOp::Max;
                match c {
                    Class::Signed => Acc::ExtInt {
                        v: Vec::new(),
                        seen: Vec::new(),
                        max,
                    },
                    Class::Unsigned => Acc::ExtUInt {
                        v: Vec::new(),
                        seen: Vec::new(),
                        max,
                    },
                    Class::Float => Acc::ExtFloat {
                        v: Vec::new(),
                        seen: Vec::new(),
                        max,
                    },
                }
            }
            (AggOp::Mean, Class::Float) => Acc::MeanFloat {
                sum: Vec::new(),
                n: Vec::new(),
            },
            (AggOp::Mean, _) => Acc::MeanInt {
                sum: Vec::new(),
                n: Vec::new(),
            },
        }
    }

    /// Room for `len` groups.
    pub(crate) fn grow(&mut self, len: usize) {
        match self {
            Acc::Rows => {}
            Acc::Count(n) => n.resize(len, 0),
            Acc::SumInt { sum, seen, .. } => {
                sum.resize(len, 0);
                seen.resize(len, false);
            }
            Acc::SumFloat { sum, seen } => {
                sum.resize(len, ExactSum::new());
                seen.resize(len, false);
            }
            Acc::ExtInt { v, seen, .. } => {
                v.resize(len, 0);
                seen.resize(len, false);
            }
            Acc::ExtUInt { v, seen, .. } => {
                v.resize(len, 0);
                seen.resize(len, false);
            }
            Acc::ExtFloat { v, seen, .. } => {
                v.resize(len, 0.0);
                seen.resize(len, false);
            }
            Acc::MeanInt { sum, n } => {
                sum.resize(len, 0);
                n.resize(len, 0);
            }
            Acc::MeanFloat { sum, n } => {
                sum.resize(len, ExactSum::new());
                n.resize(len, 0);
            }
            Acc::SumBinned { sum, seen } => {
                sum.resize(len, BinnedSum::new());
                seen.resize(len, false);
            }
            Acc::SumPlain { sum, seen } => {
                sum.resize(len, 0.0);
                seen.resize(len, false);
            }
        }
    }

    /// Keep only the groups `keep`, renumbered in that order (the dense path declining).
    pub(crate) fn compact(&mut self, keep: &[u32]) {
        fn pick<T: Clone>(v: &mut Vec<T>, keep: &[u32]) {
            *v = keep.iter().map(|&g| v[g as usize].clone()).collect();
        }
        match self {
            Acc::Rows => {}
            Acc::Count(n) => pick(n, keep),
            Acc::SumInt { sum, seen, .. } => {
                pick(sum, keep);
                pick(seen, keep);
            }
            Acc::SumFloat { sum, seen } => {
                pick(sum, keep);
                pick(seen, keep);
            }
            Acc::ExtInt { v, seen, .. } => {
                pick(v, keep);
                pick(seen, keep);
            }
            Acc::ExtUInt { v, seen, .. } => {
                pick(v, keep);
                pick(seen, keep);
            }
            Acc::ExtFloat { v, seen, .. } => {
                pick(v, keep);
                pick(seen, keep);
            }
            Acc::MeanInt { sum, n } => {
                pick(sum, keep);
                pick(n, keep);
            }
            Acc::MeanFloat { sum, n } => {
                pick(sum, keep);
                pick(n, keep);
            }
            Acc::SumBinned { sum, seen } => {
                pick(sum, keep);
                pick(seen, keep);
            }
            Acc::SumPlain { sum, seen } => {
                pick(sum, keep);
                pick(seen, keep);
            }
        }
    }

    /// Accumulate rows: `ids[r]` is row `r`'s group, `values` the aggregate's column in its
    /// physical class (none for `count` of rows).
    pub(crate) fn update(&mut self, ids: &[u32], values: Option<&ArrayRef>) {
        let Some(values) = values else { return };
        match self {
            Acc::Rows => {}
            Acc::Count(n) => match values.logical_nulls().filter(|m| m.null_count() > 0) {
                None => ids.iter().for_each(|&g| n[g as usize] += 1),
                Some(nulls) => {
                    for (row, &g) in ids.iter().enumerate() {
                        if nulls.is_valid(row) {
                            n[g as usize] += 1;
                        }
                    }
                }
            },
            Acc::SumInt {
                sum,
                seen,
                unsigned: false,
            } => each_valid!(values.as_primitive::<Int64Type>(), ids, |g, v| {
                sum[g] += v as i128;
                seen[g] = true;
            }),
            Acc::SumInt {
                sum,
                seen,
                unsigned: true,
            } => each_valid!(values.as_primitive::<UInt64Type>(), ids, |g, v| {
                sum[g] += v as i128;
                seen[g] = true;
            }),
            Acc::SumFloat { sum, seen } => {
                each_valid!(values.as_primitive::<Float64Type>(), ids, |g, v| {
                    sum[g].add(v);
                    seen[g] = true;
                })
            }
            Acc::ExtInt { v, seen, max } => {
                let max = *max;
                each_valid!(values.as_primitive::<Int64Type>(), ids, |g, x| {
                    if !seen[g] || (max && x > v[g]) || (!max && x < v[g]) {
                        v[g] = x;
                        seen[g] = true;
                    }
                })
            }
            Acc::ExtUInt { v, seen, max } => {
                let max = *max;
                each_valid!(values.as_primitive::<UInt64Type>(), ids, |g, x| {
                    if !seen[g] || (max && x > v[g]) || (!max && x < v[g]) {
                        v[g] = x;
                        seen[g] = true;
                    }
                })
            }
            Acc::ExtFloat { v, seen, max } => {
                let max = *max;
                each_valid!(values.as_primitive::<Float64Type>(), ids, |g, x| {
                    let k = total_key(x);
                    if !seen[g] || (max && k > total_key(v[g])) || (!max && k < total_key(v[g])) {
                        v[g] = if x.is_nan() { f64::NAN } else { x };
                        seen[g] = true;
                    }
                })
            }
            Acc::MeanInt { sum, n } => match values.data_type() {
                DataType::UInt64 => {
                    each_valid!(values.as_primitive::<UInt64Type>(), ids, |g, v| {
                        sum[g] += v as i128;
                        n[g] += 1;
                    })
                }
                _ => each_valid!(values.as_primitive::<Int64Type>(), ids, |g, v| {
                    sum[g] += v as i128;
                    n[g] += 1;
                }),
            },
            Acc::MeanFloat { sum, n } => {
                each_valid!(values.as_primitive::<Float64Type>(), ids, |g, v| {
                    sum[g].add(v);
                    n[g] += 1;
                })
            }
            Acc::SumBinned { sum, seen } => {
                each_valid!(values.as_primitive::<Float64Type>(), ids, |g, v| {
                    sum[g].add(v);
                    seen[g] = true;
                })
            }
            Acc::SumPlain { sum, seen } => {
                each_valid!(values.as_primitive::<Float64Type>(), ids, |g, v| {
                    sum[g] += v;
                    seen[g] = true;
                })
            }
        }
    }

    /// Merge partial results: `ids[r]` is partial row `r`'s group, `state` the aggregate's
    /// partial columns (one, or two for `mean`), min and max in their physical class.
    pub(crate) fn merge(&mut self, ids: &[u32], state: &[ArrayRef]) -> Result<()> {
        match self {
            Acc::Rows => {}
            Acc::Count(n) => each_valid!(state[0].as_primitive::<Int64Type>(), ids, |g, v| {
                n[g] += v;
            }),
            Acc::SumInt { sum, seen, .. } => {
                each_valid!(state[0].as_primitive::<Decimal128Type>(), ids, |g, v| {
                    sum[g] = sum[g].checked_add(v).ok_or_else(sum_too_large)?;
                    seen[g] = true;
                })
            }
            Acc::SumFloat { sum, seen } => {
                let arr = state[0].as_binary::<i32>();
                for (row, &g) in ids.iter().enumerate() {
                    if arr.is_valid(row) {
                        let s = ExactSum::decode(arr.value(row)).map_err(failed)?;
                        sum[g as usize].merge(&s);
                        seen[g as usize] = true;
                    }
                }
            }
            Acc::ExtInt { .. } | Acc::ExtUInt { .. } | Acc::ExtFloat { .. } => {
                self.update(ids, Some(&state[0]));
            }
            Acc::MeanInt { sum, n } => {
                let sums = state[0].as_primitive::<Decimal128Type>();
                let counts = state[1].as_primitive::<Int64Type>();
                for (row, &g) in ids.iter().enumerate() {
                    let g = g as usize;
                    sum[g] = sum[g]
                        .checked_add(sums.value(row))
                        .ok_or_else(sum_too_large)?;
                    n[g] += counts.value(row);
                }
            }
            Acc::MeanFloat { sum, n } => {
                let sums = state[0].as_binary::<i32>();
                let counts = state[1].as_primitive::<Int64Type>();
                for (row, &g) in ids.iter().enumerate() {
                    let s = ExactSum::decode(sums.value(row)).map_err(failed)?;
                    sum[g as usize].merge(&s);
                    n[g as usize] += counts.value(row);
                }
            }
            Acc::SumBinned { .. } | Acc::SumPlain { .. } => return Err(measurement_only()),
        }
        Ok(())
    }

    /// The final value of the groups `order`, in that order; `rows` the groups' row counts;
    /// `ty` the final type for `min` and `max`.
    pub(crate) fn emit_final(
        &self,
        order: &[u32],
        rows: &[i64],
        ty: &DataType,
    ) -> Result<core::result::Result<ArrayRef, Overflow>> {
        let at = |v: &[i64]| -> Int64Array { order.iter().map(|&g| v[g as usize]).collect() };
        Ok(Ok(match self {
            Acc::Rows => Arc::new(at(rows)),
            Acc::Count(n) => Arc::new(at(n)),
            Acc::SumInt {
                sum,
                seen,
                unsigned,
            } => {
                for (row, &g) in order.iter().enumerate() {
                    let total = sum[g as usize];
                    let fits = if *unsigned {
                        u64::try_from(total).is_ok()
                    } else {
                        i64::try_from(total).is_ok()
                    };
                    if !fits {
                        return Ok(Err(Overflow { row, total }));
                    }
                }
                let pick = |g: &u32| seen[*g as usize].then_some(sum[*g as usize]);
                if *unsigned {
                    Arc::new(
                        order
                            .iter()
                            .map(|g| pick(g).map(|s| s as u64))
                            .collect::<UInt64Array>(),
                    )
                } else {
                    Arc::new(
                        order
                            .iter()
                            .map(|g| pick(g).map(|s| s as i64))
                            .collect::<Int64Array>(),
                    )
                }
            }
            Acc::SumFloat { sum, seen } => Arc::new(
                order
                    .iter()
                    .map(|&g| seen[g as usize].then(|| sum[g as usize].value()))
                    .collect::<Float64Array>(),
            ),
            Acc::ExtInt { v, seen, .. } => {
                let values: ArrayRef = Arc::new(
                    order
                        .iter()
                        .map(|&g| seen[g as usize].then_some(v[g as usize]))
                        .collect::<Int64Array>(),
                );
                from_physical(&values, ty)?
            }
            Acc::ExtUInt { v, seen, .. } => {
                let values: ArrayRef = Arc::new(
                    order
                        .iter()
                        .map(|&g| seen[g as usize].then_some(v[g as usize]))
                        .collect::<UInt64Array>(),
                );
                from_physical(&values, ty)?
            }
            Acc::ExtFloat { v, seen, .. } => {
                let values: ArrayRef = Arc::new(
                    order
                        .iter()
                        .map(|&g| seen[g as usize].then_some(v[g as usize]))
                        .collect::<Float64Array>(),
                );
                from_physical(&values, ty)?
            }
            Acc::MeanInt { sum, n } => Arc::new(
                order
                    .iter()
                    .map(|&g| {
                        let g = g as usize;
                        (n[g] > 0).then(|| sum[g] as f64 / n[g] as f64)
                    })
                    .collect::<Float64Array>(),
            ),
            Acc::MeanFloat { sum, n } => Arc::new(
                order
                    .iter()
                    .map(|&g| {
                        let g = g as usize;
                        (n[g] > 0).then(|| sum[g].value() / n[g] as f64)
                    })
                    .collect::<Float64Array>(),
            ),
            Acc::SumBinned { sum, seen } => Arc::new(
                order
                    .iter()
                    .map(|&g| seen[g as usize].then(|| sum[g as usize].value()))
                    .collect::<Float64Array>(),
            ),
            Acc::SumPlain { sum, seen } => Arc::new(
                order
                    .iter()
                    .map(|&g| seen[g as usize].then_some(sum[g as usize]))
                    .collect::<Float64Array>(),
            ),
        }))
    }

    /// The partial result of the groups `order`, in that order: one column, or two for `mean`
    /// (its sum, then its count).
    pub(crate) fn emit_partial(
        &self,
        order: &[u32],
        rows: &[i64],
        ty: &DataType,
    ) -> Result<Vec<ArrayRef>> {
        let decimal = |values: Vec<Option<i128>>| -> Result<ArrayRef> {
            Ok(Arc::new(
                Decimal128Array::from(values)
                    .with_precision_and_scale(38, 0)
                    .map_err(arrow_err)?,
            ))
        };
        Ok(match self {
            Acc::SumInt { sum, seen, .. } => vec![decimal(
                order
                    .iter()
                    .map(|&g| seen[g as usize].then_some(sum[g as usize]))
                    .collect(),
            )?],
            Acc::SumFloat { sum, seen } => vec![exact_column(
                order
                    .iter()
                    .map(|&g| seen[g as usize].then_some(&sum[g as usize])),
            )],
            Acc::MeanInt { sum, n } => vec![
                decimal(order.iter().map(|&g| Some(sum[g as usize])).collect())?,
                Arc::new(order.iter().map(|&g| n[g as usize]).collect::<Int64Array>()),
            ],
            Acc::MeanFloat { sum, n } => vec![
                exact_column(order.iter().map(|&g| Some(&sum[g as usize]))),
                Arc::new(order.iter().map(|&g| n[g as usize]).collect::<Int64Array>()),
            ],
            Acc::SumBinned { .. } | Acc::SumPlain { .. } => return Err(measurement_only()),
            // Counts, minima and maxima are their own partial results.
            _ => match self.emit_final(order, rows, ty)? {
                Ok(a) => vec![a],
                Err(_) => unreachable!("only sums overflow"),
            },
        })
    }

    /// Bytes this accumulator holds.
    pub(crate) fn bytes(&self) -> u64 {
        fn of<T>(v: &Vec<T>) -> u64 {
            (v.capacity() * core::mem::size_of::<T>()) as u64
        }
        let spills = |sum: &Vec<ExactSum>| sum.iter().map(ExactSum::heap_bytes).sum::<u64>();
        match self {
            Acc::Rows => 0,
            Acc::Count(n) => of(n),
            Acc::SumInt { sum, seen, .. } => of(sum) + of(seen),
            Acc::SumFloat { sum, seen } => of(sum) + of(seen) + spills(sum),
            Acc::ExtInt { v, seen, .. } => of(v) + of(seen),
            Acc::ExtUInt { v, seen, .. } => of(v) + of(seen),
            Acc::ExtFloat { v, seen, .. } => of(v) + of(seen),
            Acc::MeanInt { sum, n } => of(sum) + of(n),
            Acc::MeanFloat { sum, n } => of(sum) + of(n) + spills(sum),
            Acc::SumBinned { sum, seen } => of(sum) + of(seen),
            Acc::SumPlain { sum, seen } => of(sum) + of(seen),
        }
    }
}

/// The most bytes one group of `op` holds, over every class (the dense path's declared size).
pub(crate) fn bytes_per_group(op: AggOp) -> u64 {
    let exact = core::mem::size_of::<ExactSum>() as u64;
    match op {
        AggOp::Count => 8,
        AggOp::Sum => exact.max(16) + 1,
        AggOp::Min | AggOp::Max => 9,
        AggOp::Mean => exact.max(16) + 8,
    }
}

fn sum_too_large() -> MorunaError {
    failed("a partial sum is past what i128 holds")
}

/// Exact sums in their partial form, a null for a group with no value.
fn exact_column<'a>(sums: impl Iterator<Item = Option<&'a ExactSum>>) -> ArrayRef {
    let mut b = BinaryBuilder::new();
    let mut buf = Vec::new();
    for s in sums {
        match s {
            Some(s) => {
                buf.clear();
                s.encode(&mut buf);
                b.append_value(&buf);
            }
            None => b.append_null(),
        }
    }
    Arc::new(b.finish())
}

fn measurement_only() -> MorunaError {
    failed("a measurement-only float sum has no partial form")
}
