//! Evaluating a [`Program`] over a batch: one pass of chunks of [`CHUNK`] rows (spec/expr.md,
//! "Evaluation").
//!
//! For each chunk, every instruction runs once, in order, reading its arguments' registers and
//! writing its own; a register is one chunk of values and a validity byte per row, so a tree's
//! intermediate values stay in the core's cache. At the end of a chunk each computed output is
//! appended to its column, which is the only allocation the size of the batch. A column passed
//! through is the input's array, not a copy.

use std::sync::Arc;

use moruna_kernel::arrow::array::{
    Array, ArrayData, ArrayRef, AsArray, BooleanBufferBuilder, Datum, LargeStringBuilder,
    NullBufferBuilder, Scalar, StringBuilder, make_array,
};
use moruna_kernel::arrow::buffer::Buffer;
use moruna_kernel::arrow::compute::kernels::cmp;
use moruna_kernel::arrow::datatypes::{self as t, DataType, TimeUnit};
use moruna_kernel::arrow::record_batch::{RecordBatch, RecordBatchOptions};
use moruna_kernel::{MorunaError, Result};

use super::bind::{Arith, Cmp, Instr, IntTy, Lane, Lit, Logic, Math, Op, Program};
use super::round::{round_f32, round_f64, round_int};
use super::temporal::{self, Zone};

/// The rows of one chunk: a register of eight-byte values is 8 KiB, and a tree of a few dozen
/// registers stays within a core's L2 cache.
pub const CHUNK: usize = 1024;

/// A failure on a row of a chunk.
struct Fail {
    row: usize,
    msg: String,
}

type Step = core::result::Result<(), Fail>;

fn fail(row: usize, msg: impl Into<String>) -> Fail {
    Fail {
        row,
        msg: msg.into(),
    }
}

/// One register's values.
#[derive(Default)]
enum Vals<'a> {
    #[default]
    Empty,
    Bool(Vec<u8>),
    I64(Vec<i64>),
    U64(Vec<u64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    Str(Vec<&'a str>),
    /// An opaque column's chunk, or an opaque literal (`true`: one value for every row).
    Opaque(ArrayRef, bool),
}

/// A register: one chunk of values, and whether each row is valid.
#[derive(Default)]
struct Reg<'a> {
    vals: Vals<'a>,
    valid: Vec<u8>,
    all_valid: bool,
}

impl Reg<'_> {
    #[inline]
    fn ok(&self, j: usize) -> bool {
        self.all_valid || self.valid[j] != 0
    }
}

/// A type a register can hold.
trait LaneVal<'a>: Copy + Default {
    fn of<'r>(reg: &'r Reg<'a>) -> &'r [Self];
    fn of_mut<'r>(reg: &'r mut Reg<'a>, len: usize) -> &'r mut [Self];
}

macro_rules! lane_val {
    ($t:ty, $var:ident) => {
        impl<'a> LaneVal<'a> for $t {
            fn of<'r>(reg: &'r Reg<'a>) -> &'r [$t] {
                match &reg.vals {
                    Vals::$var(v) => v,
                    _ => &[],
                }
            }
            fn of_mut<'r>(reg: &'r mut Reg<'a>, len: usize) -> &'r mut [$t] {
                if !matches!(reg.vals, Vals::$var(_)) {
                    reg.vals = Vals::$var(Vec::with_capacity(CHUNK));
                }
                match &mut reg.vals {
                    Vals::$var(v) => {
                        v.resize(len, <$t>::default());
                        &mut v[..len]
                    }
                    _ => &mut [],
                }
            }
        }
    };
}

lane_val!(u8, Bool);
lane_val!(i64, I64);
lane_val!(u64, U64);
lane_val!(f32, F32);
lane_val!(f64, F64);
lane_val!(&'a str, Str);

/// Comparison of a lane's values, in IEEE order or in total order.
trait Ordered: Copy + PartialOrd {
    fn lt_total(a: Self, b: Self) -> bool {
        a < b
    }
    fn eq_total(a: Self, b: Self) -> bool {
        a == b
    }
}

impl Ordered for u8 {}
impl Ordered for i64 {}
impl Ordered for u64 {}
impl Ordered for &str {}
impl Ordered for f32 {
    fn lt_total(a: f32, b: f32) -> bool {
        a.total_cmp(&b).is_lt()
    }
    fn eq_total(a: f32, b: f32) -> bool {
        a.to_bits() == b.to_bits()
    }
}
impl Ordered for f64 {
    fn lt_total(a: f64, b: f64) -> bool {
        a.total_cmp(&b).is_lt()
    }
    fn eq_total(a: f64, b: f64) -> bool {
        a.to_bits() == b.to_bits()
    }
}

/// The two float lanes.
trait Fl:
    Copy
    + Default
    + PartialOrd
    + Ordered
    + core::ops::Add<Output = Self>
    + core::ops::Sub<Output = Self>
    + core::ops::Mul<Output = Self>
    + core::ops::Div<Output = Self>
    + core::ops::Neg<Output = Self>
    + for<'a> LaneVal<'a>
{
    const ZERO: Self;
    const ONE: Self;
    fn is_nan(self) -> bool;
    fn is_infinite(self) -> bool;
    fn abs(self) -> Self;
    fn powf(self, e: Self) -> Self;
    fn atan2(self, x: Self) -> Self;
    fn floor(self) -> Self;
    fn ceil(self) -> Self;
    fn trunc(self) -> Self;
    fn math(self, f: Math) -> Self;
    fn round_to(self, ndigits: i64, mode: super::round::RoundMode) -> Self;
}

macro_rules! float {
    ($t:ty, $round:ident) => {
        impl Fl for $t {
            const ZERO: $t = 0.0;
            const ONE: $t = 1.0;
            fn is_nan(self) -> bool {
                <$t>::is_nan(self)
            }
            fn is_infinite(self) -> bool {
                <$t>::is_infinite(self)
            }
            fn abs(self) -> $t {
                <$t>::abs(self)
            }
            fn powf(self, e: $t) -> $t {
                <$t>::powf(self, e)
            }
            fn atan2(self, x: $t) -> $t {
                <$t>::atan2(self, x)
            }
            fn floor(self) -> $t {
                <$t>::floor(self)
            }
            fn ceil(self) -> $t {
                <$t>::ceil(self)
            }
            fn trunc(self) -> $t {
                <$t>::trunc(self)
            }
            fn math(self, f: Math) -> $t {
                match f {
                    Math::Sqrt => self.sqrt(),
                    Math::Exp => self.exp(),
                    Math::Ln => self.ln(),
                    Math::Log10 => self.log10(),
                    Math::Log2 => self.log2(),
                    Math::Log1p => self.ln_1p(),
                    Math::Sin => self.sin(),
                    Math::Cos => self.cos(),
                    Math::Tan => self.tan(),
                    Math::Asin => self.asin(),
                    Math::Acos => self.acos(),
                    Math::Atan => self.atan(),
                }
            }
            fn round_to(self, ndigits: i64, mode: super::round::RoundMode) -> $t {
                $round(self, ndigits, mode)
            }
        }
    };
}

float!(f32, round_f32);
float!(f64, round_f64);

/// The validity of a row-wise function of `args`: valid where every one is.
fn and_valid(out: &mut Reg<'_>, args: &[&Reg<'_>], len: usize) {
    if args.iter().all(|a| a.all_valid) {
        out.all_valid = true;
        return;
    }
    out.all_valid = false;
    out.valid.clear();
    out.valid.resize(len, 1);
    for a in args {
        if !a.all_valid {
            for (o, v) in out.valid.iter_mut().zip(&a.valid[..len]) {
                *o &= *v;
            }
        }
    }
}

fn all_valid(out: &mut Reg<'_>) {
    out.all_valid = true;
}

/// A column's chunk into a register.
fn load<'a>(array: &'a ArrayRef, start: usize, len: usize, out: &mut Reg<'a>) {
    match array.logical_nulls().filter(|n| n.null_count() > 0) {
        None => out.all_valid = true,
        Some(nulls) => {
            out.all_valid = false;
            out.valid.clear();
            out.valid
                .extend(nulls.inner().slice(start, len).iter().map(u8::from));
        }
    }
    let a = array.as_ref();
    macro_rules! prim {
        ($ty:ty, $lane:ty) => {{
            let src = &a.as_primitive::<$ty>().values()[start..start + len];
            let dst = <$lane>::of_mut(out, len);
            for (d, s) in dst.iter_mut().zip(src) {
                *d = *s as $lane;
            }
        }};
    }
    match a.data_type() {
        DataType::Boolean => {
            let values = a.as_boolean().values().slice(start, len);
            let dst = u8::of_mut(out, len);
            for (d, s) in dst.iter_mut().zip(values.iter()) {
                *d = u8::from(s);
            }
        }
        DataType::Int8 => prim!(t::Int8Type, i64),
        DataType::Int16 => prim!(t::Int16Type, i64),
        DataType::Int32 => prim!(t::Int32Type, i64),
        DataType::Int64 => prim!(t::Int64Type, i64),
        DataType::Date32 => prim!(t::Date32Type, i64),
        DataType::Timestamp(TimeUnit::Second, _) => prim!(t::TimestampSecondType, i64),
        DataType::Timestamp(TimeUnit::Millisecond, _) => prim!(t::TimestampMillisecondType, i64),
        DataType::Timestamp(TimeUnit::Microsecond, _) => prim!(t::TimestampMicrosecondType, i64),
        DataType::Timestamp(TimeUnit::Nanosecond, _) => prim!(t::TimestampNanosecondType, i64),
        DataType::Duration(TimeUnit::Second) => prim!(t::DurationSecondType, i64),
        DataType::Duration(TimeUnit::Millisecond) => prim!(t::DurationMillisecondType, i64),
        DataType::Duration(TimeUnit::Microsecond) => prim!(t::DurationMicrosecondType, i64),
        DataType::Duration(TimeUnit::Nanosecond) => prim!(t::DurationNanosecondType, i64),
        DataType::UInt8 => prim!(t::UInt8Type, u64),
        DataType::UInt16 => prim!(t::UInt16Type, u64),
        DataType::UInt32 => prim!(t::UInt32Type, u64),
        DataType::UInt64 => prim!(t::UInt64Type, u64),
        DataType::Float32 => prim!(t::Float32Type, f32),
        DataType::Float64 => prim!(t::Float64Type, f64),
        DataType::Utf8 => {
            let s = a.as_string::<i32>();
            let dst = <&str>::of_mut(out, len);
            for (j, d) in dst.iter_mut().enumerate() {
                *d = s.value(start + j);
            }
        }
        DataType::LargeUtf8 => {
            let s = a.as_string::<i64>();
            let dst = <&str>::of_mut(out, len);
            for (j, d) in dst.iter_mut().enumerate() {
                *d = s.value(start + j);
            }
        }
        _ => out.vals = Vals::Opaque(array.slice(start, len), false),
    }
}

/// A literal into a register, `len` rows of it.
fn fill<'a>(lit: &'a Lit, len: usize, out: &mut Reg<'a>, lane: Lane) {
    out.all_valid = !matches!(lit, Lit::Null);
    if !out.all_valid {
        out.valid.clear();
        out.valid.resize(len, 0);
    }
    match (lit, lane) {
        (Lit::Bool(b), _) => u8::of_mut(out, len).fill(u8::from(*b)),
        (Lit::I64(v), _) => i64::of_mut(out, len).fill(*v),
        (Lit::U64(v), _) => u64::of_mut(out, len).fill(*v),
        (Lit::F32(v), _) => f32::of_mut(out, len).fill(*v),
        (Lit::F64(v), _) => f64::of_mut(out, len).fill(*v),
        (Lit::Str(s), _) => <&str>::of_mut(out, len).fill(s.as_str()),
        (Lit::Opaque(a), _) => {
            out.all_valid = a.is_valid(0);
            if !out.all_valid {
                out.valid.clear();
                out.valid.resize(len, 0);
            }
            out.vals = Vals::Opaque(Arc::clone(a), true);
        }
        (Lit::Null, Lane::Bool) => u8::of_mut(out, len).fill(0),
        (Lit::Null, Lane::I64) => i64::of_mut(out, len).fill(0),
        (Lit::Null, Lane::U64) => u64::of_mut(out, len).fill(0),
        (Lit::Null, Lane::F32) => f32::of_mut(out, len).fill(0.0),
        (Lit::Null, Lane::F64) => f64::of_mut(out, len).fill(0.0),
        (Lit::Null, Lane::Str) => <&str>::of_mut(out, len).fill(""),
        (Lit::Null, Lane::Opaque) => {}
    }
}

/// Where a selection takes each row from: an argument's index, or [`NONE`] for null.
const NONE: u16 = u16::MAX;

fn gather<'a, T: LaneVal<'a>>(srcs: &[&Reg<'a>], pick: &[u16], out: &mut Reg<'a>, len: usize) {
    let views: Vec<&[T]> = srcs.iter().map(|r| T::of(r)).collect();
    let dst = T::of_mut(out, len);
    for j in 0..len {
        let p = pick[j];
        dst[j] = if p == NONE {
            T::default()
        } else {
            views[p as usize][j]
        };
    }
    out.valid.clear();
    out.valid.extend(
        pick[..len]
            .iter()
            .enumerate()
            .map(|(j, &p)| u8::from(p != NONE && srcs[p as usize].ok(j))),
    );
    out.all_valid = out.valid.iter().all(|v| *v == 1);
}

fn compare<'a, T: LaneVal<'a> + Ordered>(
    a: &Reg<'a>,
    b: &Reg<'a>,
    op: Cmp,
    total: bool,
    out: &mut Reg<'a>,
    len: usize,
) {
    let (x, y) = (&T::of(a)[..len], &T::of(b)[..len]);
    let o = u8::of_mut(out, len);
    macro_rules! each {
        ($f:expr) => {
            for j in 0..len {
                o[j] = u8::from($f(x[j], y[j]));
            }
        };
    }
    if total {
        match op {
            Cmp::Eq => each!(T::eq_total),
            Cmp::Ne => each!(|p, q| !T::eq_total(p, q)),
            Cmp::Lt => each!(T::lt_total),
            Cmp::Le => each!(|p, q| T::lt_total(p, q) || T::eq_total(p, q)),
            Cmp::Gt => each!(|p, q| T::lt_total(q, p)),
            Cmp::Ge => each!(|p, q| T::lt_total(q, p) || T::eq_total(p, q)),
        }
    } else {
        match op {
            Cmp::Eq => each!(|p: T, q: T| p == q),
            Cmp::Ne => each!(|p: T, q: T| p != q),
            Cmp::Lt => each!(|p: T, q: T| p < q),
            Cmp::Le => each!(|p: T, q: T| p <= q),
            Cmp::Gt => each!(|p: T, q: T| p > q),
            Cmp::Ge => each!(|p: T, q: T| p >= q),
        }
    }
    and_valid(out, &[a, b], len);
}

fn float_arith<'a, T: Fl>(
    name: &str,
    op: Arith,
    checked: bool,
    a: &Reg<'a>,
    b: &Reg<'a>,
    out: &mut Reg<'a>,
    len: usize,
) -> Step {
    and_valid(out, &[a, b], len);
    let (x, y) = (&T::of(a)[..len], &T::of(b)[..len]);
    if checked && op == Arith::Div {
        for (j, v) in y.iter().enumerate() {
            if *v == T::ZERO && out.ok(j) {
                return Err(fail(j, format!("{name}: divide by zero")));
            }
        }
    }
    let o = T::of_mut(out, len);
    match op {
        Arith::Add => (0..len).for_each(|j| o[j] = x[j] + y[j]),
        Arith::Sub => (0..len).for_each(|j| o[j] = x[j] - y[j]),
        Arith::Mul => (0..len).for_each(|j| o[j] = x[j] * y[j]),
        Arith::Div => (0..len).for_each(|j| o[j] = x[j] / y[j]),
        Arith::Pow => (0..len).for_each(|j| o[j] = x[j].powf(y[j])),
    }
    Ok(())
}

/// `base^exp` wrapping at 64 bits (the caller wraps to the type's width).
fn wrapping_pow(base: u64, mut exp: u64) -> u64 {
    let (mut result, mut b) = (1u64, base);
    while exp > 0 {
        if exp & 1 == 1 {
            result = result.wrapping_mul(b);
        }
        b = b.wrapping_mul(b);
        exp >>= 1;
    }
    result
}

/// `base^exp` exactly, `None` past the type's range.
fn checked_pow(base: i128, exp: u64, ty: IntTy) -> Option<i128> {
    match base {
        0 => return Some(i128::from(exp == 0)),
        1 => return Some(1),
        -1 => return Some(if exp.is_multiple_of(2) { 1 } else { -1 }),
        _ => {}
    }
    if exp >= 128 {
        return None;
    }
    let mut r: i128 = 1;
    for _ in 0..exp {
        r = r.checked_mul(base).filter(|v| ty.fits(*v))?;
    }
    Some(r)
}

fn int_arith<'a>(
    ins: &Instr,
    op: Arith,
    checked: bool,
    a: &Reg<'a>,
    b: &Reg<'a>,
    out: &mut Reg<'a>,
    len: usize,
) -> Step {
    let ty = IntTy::of(&ins.ty).unwrap_or(IntTy {
        bits: 64,
        signed: true,
    });
    let name = ins.name.as_str();
    and_valid(out, &[a, b], len);
    let valid: Vec<bool> = (0..len).map(|j| out.ok(j)).collect();
    let overflow = |j: usize| fail(j, format!("{name}: overflow"));
    if ins.lane == Lane::I64 {
        let (x, y) = (&i64::of(a)[..len], &i64::of(b)[..len]);
        let o = i64::of_mut(out, len);
        match (op, checked) {
            (Arith::Add, false) if ty.bits == 64 => {
                (0..len).for_each(|j| o[j] = x[j].wrapping_add(y[j]))
            }
            (Arith::Sub, false) if ty.bits == 64 => {
                (0..len).for_each(|j| o[j] = x[j].wrapping_sub(y[j]))
            }
            (Arith::Add | Arith::Sub | Arith::Mul, false) => {
                for j in 0..len {
                    let v = match op {
                        Arith::Add => x[j].wrapping_add(y[j]),
                        Arith::Sub => x[j].wrapping_sub(y[j]),
                        _ => x[j].wrapping_mul(y[j]),
                    };
                    o[j] = ty.wrap_i(v);
                }
            }
            (Arith::Add | Arith::Sub | Arith::Mul, true) => {
                for j in 0..len {
                    let (p, q) = (i128::from(x[j]), i128::from(y[j]));
                    let v = match op {
                        Arith::Add => p + q,
                        Arith::Sub => p - q,
                        _ => p * q,
                    };
                    if ty.fits(v) {
                        o[j] = v as i64;
                    } else if valid[j] {
                        return Err(overflow(j));
                    } else {
                        o[j] = 0;
                    }
                }
            }
            (Arith::Div, _) => {
                let min = ty.min() as i64;
                for j in 0..len {
                    o[j] = if y[j] == 0 {
                        if valid[j] {
                            return Err(fail(j, format!("{name}: divide by zero")));
                        }
                        0
                    } else if x[j] == min && y[j] == -1 {
                        if checked && valid[j] {
                            return Err(overflow(j));
                        }
                        0
                    } else {
                        x[j] / y[j]
                    };
                }
            }
            (Arith::Pow, _) => {
                for j in 0..len {
                    if y[j] < 0 {
                        if valid[j] {
                            return Err(fail(
                                j,
                                format!(
                                    "{name}: integers to negative integer powers are not allowed"
                                ),
                            ));
                        }
                        o[j] = 0;
                    } else if checked {
                        match checked_pow(i128::from(x[j]), y[j] as u64, ty) {
                            Some(v) => o[j] = v as i64,
                            None if valid[j] => return Err(overflow(j)),
                            None => o[j] = 0,
                        }
                    } else {
                        o[j] = ty.wrap_i(wrapping_pow(x[j] as u64, y[j] as u64) as i64);
                    }
                }
            }
        }
    } else {
        let (x, y) = (&u64::of(a)[..len], &u64::of(b)[..len]);
        let o = u64::of_mut(out, len);
        for j in 0..len {
            let (p, q) = (i128::from(x[j]), i128::from(y[j]));
            o[j] = match op {
                Arith::Add | Arith::Sub | Arith::Mul => {
                    let exact = match op {
                        Arith::Add => Some(p + q),
                        Arith::Sub => Some(p - q),
                        _ => p.checked_mul(q),
                    };
                    match exact.filter(|v| ty.fits(*v)) {
                        Some(v) => v as u64,
                        None if checked && valid[j] => return Err(overflow(j)),
                        None => ty.wrap_u(match op {
                            Arith::Add => x[j].wrapping_add(y[j]),
                            Arith::Sub => x[j].wrapping_sub(y[j]),
                            _ => x[j].wrapping_mul(y[j]),
                        }),
                    }
                }
                Arith::Div => match x[j].checked_div(y[j]) {
                    Some(v) => v,
                    None if valid[j] => return Err(fail(j, format!("{name}: divide by zero"))),
                    None => 0,
                },
                Arith::Pow => {
                    if checked {
                        match checked_pow(p, y[j], ty) {
                            Some(v) => v as u64,
                            None if valid[j] => return Err(overflow(j)),
                            None => 0,
                        }
                    } else {
                        ty.wrap_u(wrapping_pow(x[j], y[j]))
                    }
                }
            };
        }
    }
    Ok(())
}

/// A cast's failure for an integer that does not fit.
fn out_of_range(j: usize, v: i128, to: IntTy) -> Fail {
    fail(
        j,
        format!(
            "cast: integer value {v} not in range: {} to {}",
            to.min(),
            to.max()
        ),
    )
}

/// A value of an integer lane, exactly.
fn int_values(r: &Reg<'_>, lane: Lane, len: usize) -> Vec<i128> {
    match lane {
        Lane::I64 => i64::of(r)[..len].iter().map(|v| i128::from(*v)).collect(),
        Lane::U64 => u64::of(r)[..len].iter().map(|v| i128::from(*v)).collect(),
        Lane::Bool => u8::of(r)[..len].iter().map(|v| i128::from(*v)).collect(),
        _ => vec![0; len],
    }
}

fn write_ints<'a>(out: &mut Reg<'a>, lane: Lane, values: &[i128]) {
    let len = values.len();
    match lane {
        Lane::I64 => {
            let o = i64::of_mut(out, len);
            for j in 0..len {
                o[j] = values[j] as i64;
            }
        }
        _ => {
            let o = u64::of_mut(out, len);
            for j in 0..len {
                o[j] = values[j] as u64;
            }
        }
    }
}

fn cast_step<'a>(ins: &Instr, from: &DataType, a: &Reg<'a>, out: &mut Reg<'a>, len: usize) -> Step {
    let Op::Cast { safe } = ins.op else {
        return Ok(());
    };
    let to = &ins.ty;
    let (lf, lt) = (super::bind::lane(from), ins.lane);
    out.all_valid = a.all_valid;
    if !a.all_valid {
        out.valid.clear();
        out.valid.extend_from_slice(&a.valid[..len]);
    }
    let ok = |j: usize| a.ok(j);
    // The hot casts first: a 64-bit relabel (a duration as its int64), and an int64 as a double.
    let relabel = matches!(
        (from, to),
        (
            DataType::Int64,
            DataType::Timestamp(..) | DataType::Duration(_)
        ) | (
            DataType::Timestamp(..) | DataType::Duration(_),
            DataType::Int64
        )
    );
    if relabel {
        i64::of_mut(out, len).copy_from_slice(&i64::of(a)[..len]);
        return Ok(());
    }
    if lf == Lane::I64 && lt == Lane::F64 {
        let x = &i64::of(a)[..len];
        if safe {
            const LIMIT: i64 = 1 << 53;
            for (j, v) in x.iter().enumerate() {
                if !(-LIMIT..=LIMIT).contains(v) && ok(j) {
                    return Err(fail(
                        j,
                        format!(
                            "cast: integer value {v} not in range: {} to {LIMIT}",
                            -LIMIT
                        ),
                    ));
                }
            }
        }
        for (o, v) in f64::of_mut(out, len).iter_mut().zip(x) {
            *o = *v as f64;
        }
        return Ok(());
    }
    match (lf, lt) {
        (Lane::Str, Lane::Str) => {
            let src = <&str>::of(a)[..len].to_vec();
            <&str>::of_mut(out, len).copy_from_slice(&src);
        }
        (Lane::F32 | Lane::F64, Lane::F32 | Lane::F64) => {
            let src: Vec<f64> = floats(a, lf, len);
            match lt {
                Lane::F32 => {
                    let o = f32::of_mut(out, len);
                    for j in 0..len {
                        o[j] = src[j] as f32;
                    }
                }
                _ => f64::of_mut(out, len).copy_from_slice(&src),
            }
        }
        (Lane::I64 | Lane::U64 | Lane::Bool, Lane::F32 | Lane::F64) => {
            let src = int_values(a, lf, len);
            let limit: i128 = if lt == Lane::F32 { 1 << 24 } else { 1 << 53 };
            if safe && lf != Lane::Bool {
                for (j, v) in src.iter().enumerate() {
                    if v.abs() > limit && ok(j) {
                        return Err(fail(
                            j,
                            format!(
                                "cast: integer value {v} not in range: {} to {limit}",
                                -limit
                            ),
                        ));
                    }
                }
            }
            match lt {
                Lane::F32 => {
                    let o = f32::of_mut(out, len);
                    for j in 0..len {
                        o[j] = src[j] as f32;
                    }
                }
                _ => {
                    let o = f64::of_mut(out, len);
                    for j in 0..len {
                        o[j] = src[j] as f64;
                    }
                }
            }
        }
        (Lane::F32 | Lane::F64, Lane::I64 | Lane::U64) => {
            let it = IntTy::of(to).unwrap_or(IntTy {
                bits: 64,
                signed: true,
            });
            let src = floats(a, lf, len);
            let mut vals = vec![0i128; len];
            for j in 0..len {
                let f = src[j];
                let t = f.trunc();
                let exact = f.is_finite() && t == f && it.fits(t as i128);
                if safe && !exact && ok(j) {
                    return Err(fail(
                        j,
                        format!("cast: float value {f} was truncated converting to {to}"),
                    ));
                }
                // NaN is 0; anything else is truncated and held at the type's ends.
                vals[j] = if f.is_nan() {
                    0
                } else {
                    (t.clamp(-1.8e19, 1.9e19) as i128).clamp(it.min(), it.max())
                };
            }
            write_ints(out, lt, &vals);
        }
        (Lane::F32 | Lane::F64 | Lane::I64 | Lane::U64, Lane::Bool) => {
            let o: Vec<u8> = match lf {
                Lane::F32 | Lane::F64 => floats(a, lf, len)
                    .iter()
                    .map(|f| u8::from(*f != 0.0))
                    .collect(),
                _ => int_values(a, lf, len)
                    .iter()
                    .map(|v| u8::from(*v != 0))
                    .collect(),
            };
            u8::of_mut(out, len).copy_from_slice(&o);
        }
        (Lane::I64 | Lane::U64 | Lane::Bool, Lane::I64 | Lane::U64) => {
            let mut src = int_values(a, lf, len);
            let it = IntTy::of(to).unwrap_or(IntTy {
                bits: 64,
                signed: true,
            });
            // First the exact value each row becomes, then one pass that refuses (safe) or wraps
            // (unsafe) what the target type cannot hold.
            let mut timestamp = false;
            match (from, to) {
                (DataType::Timestamp(u1, _), DataType::Timestamp(u2, _))
                | (DataType::Duration(u1), DataType::Duration(u2)) => {
                    timestamp = true;
                    let (f1, f2) = (temporal::per_second(*u1), temporal::per_second(*u2));
                    for (j, slot) in src.iter_mut().enumerate().take(len) {
                        let v = *slot;
                        *slot = if f2 >= f1 {
                            v * i128::from(f2 / f1)
                        } else {
                            let d = i128::from(f1 / f2);
                            if safe && v % d != 0 && ok(j) {
                                return Err(fail(
                                    j,
                                    format!(
                                        "cast: casting from {from} to {to} would lose data: {v}"
                                    ),
                                ));
                            }
                            // Toward zero, as Arrow truncates a time.
                            v / d
                        };
                    }
                }
                (DataType::Timestamp(unit, _), DataType::Date32) => {
                    let per_day = i128::from(temporal::per_second(*unit)) * 86_400;
                    let zone = ins.zone.clone().unwrap_or(Zone::Utc);
                    for (j, slot) in src.iter_mut().enumerate().take(len) {
                        let local = match zone.local(*slot as i64, *unit) {
                            Some(l) => i128::from(l),
                            None if ok(j) => {
                                return Err(fail(
                                    j,
                                    format!("cast: {} is out of the calendar's range", *slot),
                                ));
                            }
                            None => 0,
                        };
                        *slot = local.div_euclid(per_day);
                    }
                }
                (DataType::Date32, DataType::Timestamp(unit, _)) => {
                    timestamp = true;
                    let per_day = i128::from(temporal::per_second(*unit)) * 86_400;
                    for v in src.iter_mut() {
                        *v *= per_day;
                    }
                }
                _ => {}
            }
            for (j, slot) in src.iter_mut().enumerate().take(len) {
                let v = *slot;
                if !it.fits(v) {
                    if safe && ok(j) {
                        return Err(if timestamp {
                            fail(
                                j,
                                format!(
                                    "cast: casting from {from} to {to} would result in out of bounds timestamp: {v}"
                                ),
                            )
                        } else {
                            out_of_range(j, v, it)
                        });
                    }
                    *slot = if it.signed {
                        i128::from(it.wrap_i(v as i64))
                    } else {
                        i128::from(it.wrap_u(v as u64))
                    };
                }
            }
            write_ints(out, lt, &src);
        }
        (Lane::Bool, Lane::Bool) => {
            let src = u8::of(a)[..len].to_vec();
            u8::of_mut(out, len).copy_from_slice(&src);
        }
        _ => {
            return Err(fail(0, format!("cast: no cast from {from} to {to}")));
        }
    }
    Ok(())
}

fn floats(r: &Reg<'_>, lane: Lane, len: usize) -> Vec<f64> {
    match lane {
        Lane::F32 => f32::of(r)[..len].iter().map(|v| f64::from(*v)).collect(),
        _ => f64::of(r)[..len].to_vec(),
    }
}

fn float_unary<'a, T: Fl>(ins: &Instr, a: &Reg<'a>, out: &mut Reg<'a>, len: usize) -> Step {
    let name = ins.name.as_str();
    out.all_valid = a.all_valid;
    if !a.all_valid {
        out.valid.clear();
        out.valid.extend_from_slice(&a.valid[..len]);
    }
    let x = &T::of(a)[..len];
    if let Op::Math { f, checked: true } = ins.op {
        for (j, &v) in x.iter().enumerate() {
            if !a.ok(j) {
                continue;
            }
            let neg_one = -T::ONE;
            let err = match f {
                Math::Sqrt if v < T::ZERO => Some("square root of negative number"),
                Math::Ln | Math::Log10 | Math::Log2 if v == T::ZERO => Some("logarithm of zero"),
                Math::Ln | Math::Log10 | Math::Log2 if v < T::ZERO => {
                    Some("logarithm of negative number")
                }
                Math::Log1p if v == neg_one => Some("logarithm of zero"),
                Math::Log1p if v < neg_one => Some("logarithm of negative number"),
                Math::Sin | Math::Cos | Math::Tan if v.is_infinite() => Some("domain error"),
                Math::Asin | Math::Acos if v.abs() > T::ONE => Some("domain error"),
                _ => None,
            };
            if let Some(e) = err {
                return Err(fail(j, format!("{name}: {e}")));
            }
        }
    }
    let o = T::of_mut(out, len);
    match ins.op {
        Op::Math { f, .. } => (0..len).for_each(|j| o[j] = x[j].math(f)),
        Op::Neg { .. } => (0..len).for_each(|j| o[j] = -x[j]),
        Op::Abs { .. } => (0..len).for_each(|j| o[j] = x[j].abs()),
        Op::Sign => (0..len).for_each(|j| {
            let v = x[j];
            o[j] = if v.is_nan() {
                v
            } else if v > T::ZERO {
                T::ONE
            } else if v < T::ZERO {
                -T::ONE
            } else {
                T::ZERO
            };
        }),
        Op::Floor => (0..len).for_each(|j| o[j] = x[j].floor()),
        Op::Ceil => (0..len).for_each(|j| o[j] = x[j].ceil()),
        Op::Trunc => (0..len).for_each(|j| o[j] = x[j].trunc()),
        Op::Round { ndigits, mode } => (0..len).for_each(|j| o[j] = x[j].round_to(ndigits, mode)),
        _ => {}
    }
    Ok(())
}

fn int_unary<'a>(ins: &Instr, a: &Reg<'a>, out: &mut Reg<'a>, len: usize) -> Step {
    let name = ins.name.as_str();
    let from = IntTy::of(&ins.ty).unwrap_or(IntTy {
        bits: 64,
        signed: true,
    });
    out.all_valid = a.all_valid;
    if !a.all_valid {
        out.valid.clear();
        out.valid.extend_from_slice(&a.valid[..len]);
    }
    let src = int_values(a, ins.lane, len);
    let mut vals = vec![0i128; len];
    for j in 0..len {
        let v = src[j];
        vals[j] = match ins.op {
            Op::Neg { checked } => {
                let r = -v;
                if from.fits(r) {
                    r
                } else if checked && a.ok(j) {
                    return Err(fail(j, format!("{name}: overflow")));
                } else if from.signed {
                    i128::from(from.wrap_i((v as i64).wrapping_neg()))
                } else {
                    i128::from(from.wrap_u((v as u64).wrapping_neg()))
                }
            }
            Op::Abs { checked } => {
                let r = v.abs();
                if from.fits(r) {
                    r
                } else if checked && a.ok(j) {
                    return Err(fail(j, format!("{name}: overflow")));
                } else {
                    v
                }
            }
            Op::Round { ndigits, mode } => {
                match round_int(v, ndigits, mode, from.bits, from.signed) {
                    Some(r) => r,
                    None if a.ok(j) => {
                        return Err(fail(
                            j,
                            format!("{name}: rounding {v} to {ndigits} digits would overflow"),
                        ));
                    }
                    None => 0,
                }
            }
            _ => v,
        };
    }
    write_ints(out, ins.lane, &vals);
    Ok(())
}

/// `out = op(a, b)` over Boolean registers, with Kleene logic where the function has it.
fn logic<'a>(op: Logic, a: &Reg<'a>, b: &Reg<'a>, out: &mut Reg<'a>, len: usize) {
    let (x, y) = (&u8::of(a)[..len], &u8::of(b)[..len]);
    let kleene = matches!(op, Logic::AndKleene | Logic::OrKleene | Logic::AndNotKleene);
    if !kleene || (a.all_valid && b.all_valid) {
        let vals: Vec<u8> = (0..len)
            .map(|j| match op {
                Logic::And | Logic::AndKleene => x[j] & y[j],
                Logic::Or | Logic::OrKleene => x[j] | y[j],
                Logic::Xor => x[j] ^ y[j],
                Logic::AndNot | Logic::AndNotKleene => x[j] & (1 - y[j]),
            })
            .collect();
        u8::of_mut(out, len).copy_from_slice(&vals);
        and_valid(out, &[a, b], len);
        return;
    }
    let mut vals = vec![0u8; len];
    let mut valid = vec![0u8; len];
    for j in 0..len {
        let (va, vb) = (u8::from(a.ok(j)), u8::from(b.ok(j)));
        let (p, q) = (x[j] & va, y[j] & vb);
        // A known false (or, for `or`, a known true) decides the row alone.
        let (v, ok) = match op {
            Logic::AndKleene => (p & q, (va & vb) | (va & (1 - x[j])) | (vb & (1 - y[j]))),
            Logic::OrKleene => (p | q, (va & vb) | p | q),
            _ => (p & (vb & (1 - y[j])), (va & vb) | (va & (1 - x[j])) | q),
        };
        vals[j] = v & ok;
        valid[j] = ok;
    }
    u8::of_mut(out, len).copy_from_slice(&vals);
    out.all_valid = valid.iter().all(|v| *v == 1);
    out.valid = valid;
}

#[allow(clippy::too_many_arguments)]
fn min_max<'a, T: LaneVal<'a> + PartialOrd>(
    max: bool,
    skip_nulls: bool,
    float: bool,
    srcs: &[&Reg<'a>],
    out: &mut Reg<'a>,
    len: usize,
    is_nan: impl Fn(T) -> bool,
    is_neg_zero: impl Fn(T) -> bool,
) {
    let views: Vec<&[T]> = srcs.iter().map(|r| T::of(r)).collect();
    let mut pick = vec![NONE; len];
    for (j, p) in pick.iter_mut().enumerate() {
        if !skip_nulls && srcs.iter().any(|r| !r.ok(j)) {
            continue;
        }
        for (k, view) in views.iter().enumerate() {
            if !srcs[k].ok(j) {
                continue;
            }
            let x = view[j];
            if *p == NONE {
                *p = k as u16;
                continue;
            }
            let cur = views[*p as usize][j];
            let better = if float {
                if is_nan(cur) {
                    !is_nan(x)
                } else if is_nan(x) {
                    false
                } else if x == cur {
                    // +0 is above -0.
                    is_neg_zero(cur) != is_neg_zero(x) && (is_neg_zero(cur) == max)
                } else {
                    (x > cur) == max
                }
            } else {
                x != cur && (x > cur) == max
            };
            if better {
                *p = k as u16;
            }
        }
    }
    gather::<T>(srcs, &pick, out, len);
}

impl Program {
    fn step<'a>(
        &self,
        ins: &Instr,
        regs: &[Reg<'a>],
        out: &mut Reg<'a>,
        cols: &'a [ArrayRef],
        start: usize,
        len: usize,
    ) -> Step {
        let arg = |k: usize| &regs[self.instrs[ins.args[k]].reg];
        let arg_lane = |k: usize| self.instrs[ins.args[k]].lane;
        match &ins.op {
            Op::Column(c) => load(&cols[*c], start, len, out),
            Op::Literal(_) => {}
            Op::Cast { .. } => {
                let from = &self.instrs[ins.args[0]].ty;
                return cast_step(ins, from, arg(0), out, len);
            }
            Op::Arith { op, checked } => {
                let (a, b) = (arg(0), arg(1));
                return match ins.lane {
                    Lane::F64 => float_arith::<f64>(&ins.name, *op, *checked, a, b, out, len),
                    Lane::F32 => float_arith::<f32>(&ins.name, *op, *checked, a, b, out, len),
                    _ => int_arith(ins, *op, *checked, a, b, out, len),
                };
            }
            Op::Neg { .. } | Op::Abs { .. } | Op::Round { .. } => {
                return match ins.lane {
                    Lane::F64 => float_unary::<f64>(ins, arg(0), out, len),
                    Lane::F32 => float_unary::<f32>(ins, arg(0), out, len),
                    _ => int_unary(ins, arg(0), out, len),
                };
            }
            Op::Math { .. } | Op::Floor | Op::Ceil | Op::Trunc => {
                return match ins.lane {
                    Lane::F32 => float_unary::<f32>(ins, arg(0), out, len),
                    _ => float_unary::<f64>(ins, arg(0), out, len),
                };
            }
            Op::Sign => {
                return match ins.lane {
                    Lane::F64 => float_unary::<f64>(ins, arg(0), out, len),
                    Lane::F32 => float_unary::<f32>(ins, arg(0), out, len),
                    _ => {
                        let a = arg(0);
                        let src = int_values(a, arg_lane(0), len);
                        let o = i64::of_mut(out, len);
                        for j in 0..len {
                            o[j] = src[j].signum() as i64;
                        }
                        out.all_valid = a.all_valid;
                        if !a.all_valid {
                            out.valid.clear();
                            out.valid.extend_from_slice(&a.valid[..len]);
                        }
                        Ok(())
                    }
                };
            }
            Op::Atan2 => {
                let (a, b) = (arg(0), arg(1));
                and_valid(out, &[a, b], len);
                match ins.lane {
                    Lane::F32 => {
                        let (y, x) = (f32::of(a), f32::of(b));
                        let o = f32::of_mut(out, len);
                        (0..len).for_each(|j| o[j] = Fl::atan2(y[j], x[j]));
                    }
                    _ => {
                        let (y, x) = (f64::of(a), f64::of(b));
                        let o = f64::of_mut(out, len);
                        (0..len).for_each(|j| o[j] = Fl::atan2(y[j], x[j]));
                    }
                }
            }
            Op::Cmp { op, total } => {
                let (a, b) = (arg(0), arg(1));
                match arg_lane(0) {
                    Lane::Bool => compare::<u8>(a, b, *op, *total, out, len),
                    Lane::I64 => compare::<i64>(a, b, *op, *total, out, len),
                    Lane::U64 => compare::<u64>(a, b, *op, *total, out, len),
                    Lane::F32 => compare::<f32>(a, b, *op, *total, out, len),
                    Lane::F64 => compare::<f64>(a, b, *op, *total, out, len),
                    Lane::Str => compare::<&str>(a, b, *op, *total, out, len),
                    Lane::Opaque => return Err(fail(0, "an opaque comparison in a lane")),
                }
            }
            Op::CmpOpaque { op } => {
                let datum = |r: &Reg<'a>| -> Option<(ArrayRef, bool)> {
                    match &r.vals {
                        Vals::Opaque(a, scalar) => Some((Arc::clone(a), *scalar)),
                        _ => None,
                    }
                };
                let (Some((x, xs)), Some((y, ys))) = (datum(arg(0)), datum(arg(1))) else {
                    return Err(fail(
                        0,
                        format!("{}: an opaque comparison of a lane", ins.name),
                    ));
                };
                let as_datum = |a: ArrayRef, scalar: bool| -> Box<dyn Datum> {
                    if scalar {
                        Box::new(Scalar::new(a))
                    } else {
                        Box::new(a)
                    }
                };
                let (l, r) = (as_datum(x, xs), as_datum(y, ys));
                let (l, r): (&dyn Datum, &dyn Datum) = (l.as_ref(), r.as_ref());
                let result = match op {
                    Cmp::Eq => cmp::eq(l, r),
                    Cmp::Ne => cmp::neq(l, r),
                    Cmp::Lt => cmp::lt(l, r),
                    Cmp::Le => cmp::lt_eq(l, r),
                    Cmp::Gt => cmp::gt(l, r),
                    Cmp::Ge => cmp::gt_eq(l, r),
                }
                .map_err(|e| fail(0, format!("{}: {e}", ins.name)))?;
                // Two scalars give one value: every row has it.
                let one = result.len() == 1 && len != 1;
                let o = u8::of_mut(out, len);
                for (j, d) in o.iter_mut().enumerate() {
                    let k = if one { 0 } else { j };
                    *d = u8::from(result.is_valid(k) && result.value(k));
                }
                out.valid.clear();
                out.valid
                    .extend((0..len).map(|j| u8::from(result.is_valid(if one { 0 } else { j }))));
                out.all_valid = result.null_count() == 0;
            }
            Op::Logic(op) => logic(*op, arg(0), arg(1), out, len),
            Op::Invert => {
                let a = arg(0);
                let vals: Vec<u8> = u8::of(a)[..len].iter().map(|v| 1 - v).collect();
                u8::of_mut(out, len).copy_from_slice(&vals);
                out.all_valid = a.all_valid;
                if !a.all_valid {
                    out.valid.clear();
                    out.valid.extend_from_slice(&a.valid[..len]);
                }
            }
            Op::IsNull { nan_is_null } => {
                let a = arg(0);
                let nan: Vec<bool> = match (*nan_is_null, arg_lane(0)) {
                    (true, Lane::F32) => f32::of(a)[..len].iter().map(|v| v.is_nan()).collect(),
                    (true, Lane::F64) => f64::of(a)[..len].iter().map(|v| v.is_nan()).collect(),
                    _ => vec![false; len],
                };
                let vals: Vec<u8> = (0..len).map(|j| u8::from(!a.ok(j) || nan[j])).collect();
                u8::of_mut(out, len).copy_from_slice(&vals);
                all_valid(out);
            }
            Op::IsValid => {
                let a = arg(0);
                let vals: Vec<u8> = (0..len).map(|j| u8::from(a.ok(j))).collect();
                u8::of_mut(out, len).copy_from_slice(&vals);
                all_valid(out);
            }
            Op::IsNan | Op::IsFinite | Op::IsInf => {
                let a = arg(0);
                let f = |v: f64| match ins.op {
                    Op::IsNan => v.is_nan(),
                    Op::IsFinite => v.is_finite(),
                    _ => v.is_infinite(),
                };
                let vals: Vec<u8> = match arg_lane(0) {
                    Lane::F32 | Lane::F64 => floats(a, arg_lane(0), len)
                        .into_iter()
                        .map(|v| u8::from(f(v)))
                        .collect(),
                    _ => vec![u8::from(ins.op == Op::IsFinite); len],
                };
                u8::of_mut(out, len).copy_from_slice(&vals);
                out.all_valid = a.all_valid;
                if !a.all_valid {
                    out.valid.clear();
                    out.valid.extend_from_slice(&a.valid[..len]);
                }
            }
            Op::IfElse | Op::CaseWhen { .. } | Op::Coalesce => {
                let srcs: Vec<&Reg<'a>> = (0..ins.args.len()).map(arg).collect();
                let mut pick = vec![NONE; len];
                match ins.op {
                    Op::IfElse => {
                        let c = u8::of(srcs[0]);
                        for (j, p) in pick.iter_mut().enumerate() {
                            if srcs[0].ok(j) {
                                *p = if c[j] == 1 { 1 } else { 2 };
                            }
                        }
                    }
                    Op::CaseWhen { has_else } => {
                        let clauses = ins.args.len() / 2;
                        for (j, p) in pick.iter_mut().enumerate() {
                            for k in 0..clauses {
                                let c = srcs[2 * k];
                                if c.ok(j) && u8::of(c)[j] == 1 {
                                    *p = (2 * k + 1) as u16;
                                    break;
                                }
                            }
                            if *p == NONE && has_else {
                                *p = (2 * clauses) as u16;
                            }
                        }
                    }
                    _ => {
                        for (j, p) in pick.iter_mut().enumerate() {
                            if let Some(k) = srcs.iter().position(|r| r.ok(j)) {
                                *p = k as u16;
                            }
                        }
                    }
                }
                match ins.lane {
                    Lane::Bool => gather::<u8>(&srcs, &pick, out, len),
                    Lane::I64 => gather::<i64>(&srcs, &pick, out, len),
                    Lane::U64 => gather::<u64>(&srcs, &pick, out, len),
                    Lane::F32 => gather::<f32>(&srcs, &pick, out, len),
                    Lane::F64 => gather::<f64>(&srcs, &pick, out, len),
                    Lane::Str => gather::<&str>(&srcs, &pick, out, len),
                    Lane::Opaque => return Err(fail(0, "an opaque selection")),
                }
            }
            Op::MinMax { max, skip_nulls } => {
                let srcs: Vec<&Reg<'a>> = (0..ins.args.len()).map(arg).collect();
                let (m, s) = (*max, *skip_nulls);
                match ins.lane {
                    Lane::I64 => min_max::<i64>(m, s, false, &srcs, out, len, |_| false, |_| false),
                    Lane::U64 => min_max::<u64>(m, s, false, &srcs, out, len, |_| false, |_| false),
                    Lane::F32 => min_max::<f32>(m, s, true, &srcs, out, len, f32::is_nan, |v| {
                        v == 0.0 && v.is_sign_negative()
                    }),
                    _ => min_max::<f64>(m, s, true, &srcs, out, len, f64::is_nan, |v| {
                        v == 0.0 && v.is_sign_negative()
                    }),
                }
            }
            Op::Part(part) => {
                let a = arg(0);
                let src = &i64::of(a)[..len];
                let from = &self.instrs[ins.args[0]].ty;
                let mut vals = vec![0i64; len];
                match from {
                    DataType::Timestamp(unit, _) => {
                        let zone = ins.zone.clone().unwrap_or(Zone::Utc);
                        for j in 0..len {
                            let local = match zone.local(src[j], *unit) {
                                Some(l) => l,
                                None if a.ok(j) => {
                                    return Err(fail(
                                        j,
                                        format!(
                                            "{}: {} is out of the calendar's range",
                                            ins.name, src[j]
                                        ),
                                    ));
                                }
                                None => 0,
                            };
                            vals[j] = temporal::of_local(local, *unit, *part);
                        }
                    }
                    _ => {
                        for j in 0..len {
                            vals[j] = temporal::of_days(src[j], *part).unwrap_or(0);
                        }
                    }
                }
                i64::of_mut(out, len).copy_from_slice(&vals);
                out.all_valid = a.all_valid;
                if !a.all_valid {
                    out.valid.clear();
                    out.valid.extend_from_slice(&a.valid[..len]);
                }
            }
        }
        Ok(())
    }

    /// Evaluate every output over `batch`, in one pass of chunks. The batch must have the
    /// schema the program was checked against.
    pub fn eval(&self, batch: &RecordBatch) -> Result<RecordBatch> {
        let rows = batch.num_rows();
        let schema = batch.schema();
        for (i, f) in self.input.fields().iter().enumerate() {
            let matches = schema
                .fields()
                .get(i)
                .is_some_and(|g| g.name() == f.name() && g.data_type() == f.data_type());
            if !matches {
                return Err(MorunaError::Plan(format!(
                    "moruna.std.expr: the batch's column {i} is not the `{}` {} the tree was checked against",
                    f.name(),
                    f.data_type()
                )));
            }
        }
        let cols: Vec<ArrayRef> = batch.columns().to_vec();
        let mut regs: Vec<Reg<'_>> = (0..self.regs).map(|_| Reg::default()).collect();
        let mut bufs: Vec<Option<OutBuf>> = self
            .outputs
            .iter()
            .map(|o| match o.column {
                Some(_) => None,
                None => Some(OutBuf::new(&self.instrs[o.instr].ty, rows)),
            })
            .collect();
        let mut start = 0usize;
        while start < rows {
            let len = CHUNK.min(rows - start);
            for ins in &self.instrs {
                if ins.constant && start > 0 {
                    continue;
                }
                let mut out = std::mem::take(&mut regs[ins.reg]);
                let done = match &ins.op {
                    Op::Literal(lit) => {
                        fill(lit, len, &mut out, ins.lane);
                        Ok(())
                    }
                    _ => self.step(ins, &regs, &mut out, &cols, start, len),
                };
                regs[ins.reg] = out;
                if let Err(Fail { row, msg }) = done {
                    let row = if ins.constant { 0 } else { start + row };
                    return Err(MorunaError::Plan(format!(
                        "moruna.std.expr: row {row}: {msg}"
                    )));
                }
            }
            for (o, buf) in self.outputs.iter().zip(bufs.iter_mut()) {
                if let Some(buf) = buf {
                    buf.append(&regs[self.instrs[o.instr].reg], len);
                }
            }
            start += len;
        }
        let mut columns = Vec::with_capacity(self.outputs.len());
        for (o, buf) in self.outputs.iter().zip(bufs) {
            columns.push(match (o.column, buf) {
                (Some(c), _) => Arc::clone(&cols[c]),
                (None, Some(buf)) => buf.finish(&self.instrs[o.instr].ty)?,
                (None, None) => {
                    return Err(MorunaError::Plan(
                        "moruna.std.expr: an output without a column".into(),
                    ));
                }
            });
        }
        RecordBatch::try_new_with_options(
            self.schema(),
            columns,
            &RecordBatchOptions::new().with_row_count(Some(rows)),
        )
        .map_err(|e| MorunaError::Plan(format!("moruna.std.expr: {e}")))
    }
}

/// An output column as it is filled, chunk by chunk.
enum OutVals {
    Bool(BooleanBufferBuilder),
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    Utf8(StringBuilder),
    LargeUtf8(LargeStringBuilder),
    Opaque(Vec<ArrayRef>),
}

struct OutBuf {
    vals: OutVals,
    nulls: NullBufferBuilder,
}

impl OutBuf {
    fn new(ty: &DataType, rows: usize) -> OutBuf {
        let vals = match ty {
            DataType::Boolean => OutVals::Bool(BooleanBufferBuilder::new(rows)),
            DataType::Int8 => OutVals::I8(Vec::with_capacity(rows)),
            DataType::Int16 => OutVals::I16(Vec::with_capacity(rows)),
            DataType::Int32 | DataType::Date32 => OutVals::I32(Vec::with_capacity(rows)),
            DataType::Int64 | DataType::Timestamp(..) | DataType::Duration(_) => {
                OutVals::I64(Vec::with_capacity(rows))
            }
            DataType::UInt8 => OutVals::U8(Vec::with_capacity(rows)),
            DataType::UInt16 => OutVals::U16(Vec::with_capacity(rows)),
            DataType::UInt32 => OutVals::U32(Vec::with_capacity(rows)),
            DataType::UInt64 => OutVals::U64(Vec::with_capacity(rows)),
            DataType::Float32 => OutVals::F32(Vec::with_capacity(rows)),
            DataType::Float64 => OutVals::F64(Vec::with_capacity(rows)),
            DataType::Utf8 => OutVals::Utf8(StringBuilder::with_capacity(rows, rows * 8)),
            DataType::LargeUtf8 => {
                OutVals::LargeUtf8(LargeStringBuilder::with_capacity(rows, rows * 8))
            }
            _ => OutVals::Opaque(Vec::new()),
        };
        OutBuf {
            vals,
            nulls: NullBufferBuilder::new(rows),
        }
    }

    fn append(&mut self, reg: &Reg<'_>, len: usize) {
        if reg.all_valid {
            self.nulls.append_n_non_nulls(len);
        } else {
            for v in &reg.valid[..len] {
                self.nulls.append(*v != 0);
            }
        }
        macro_rules! ext {
            ($v:expr, $src:ty, $t:ty) => {
                $v.extend(<$src>::of(reg)[..len].iter().map(|x| *x as $t))
            };
        }
        match &mut self.vals {
            OutVals::Bool(b) => {
                for v in &u8::of(reg)[..len] {
                    b.append(*v != 0);
                }
            }
            OutVals::I8(v) => ext!(v, i64, i8),
            OutVals::I16(v) => ext!(v, i64, i16),
            OutVals::I32(v) => ext!(v, i64, i32),
            OutVals::I64(v) => v.extend_from_slice(&i64::of(reg)[..len]),
            OutVals::U8(v) => ext!(v, u64, u8),
            OutVals::U16(v) => ext!(v, u64, u16),
            OutVals::U32(v) => ext!(v, u64, u32),
            OutVals::U64(v) => v.extend_from_slice(&u64::of(reg)[..len]),
            OutVals::F32(v) => v.extend_from_slice(&f32::of(reg)[..len]),
            OutVals::F64(v) => v.extend_from_slice(&f64::of(reg)[..len]),
            OutVals::Utf8(b) => {
                for (j, s) in <&str>::of(reg)[..len].iter().enumerate() {
                    if reg.ok(j) {
                        b.append_value(s);
                    } else {
                        b.append_null();
                    }
                }
            }
            OutVals::LargeUtf8(b) => {
                for (j, s) in <&str>::of(reg)[..len].iter().enumerate() {
                    if reg.ok(j) {
                        b.append_value(s);
                    } else {
                        b.append_null();
                    }
                }
            }
            OutVals::Opaque(parts) => {
                if let Vals::Opaque(a, scalar) = &reg.vals {
                    if *scalar {
                        for _ in 0..len {
                            parts.push(Arc::clone(a));
                        }
                    } else {
                        parts.push(Arc::clone(a));
                    }
                }
            }
        }
    }

    fn finish(self, ty: &DataType) -> Result<ArrayRef> {
        let OutBuf { vals, mut nulls } = self;
        let nulls = nulls.finish();
        let err = |e: moruna_kernel::arrow::error::ArrowError| {
            MorunaError::Plan(format!("moruna.std.expr: an output column: {e}"))
        };
        let prim = |len: usize, buffer: Buffer| -> Result<ArrayRef> {
            let data = ArrayData::builder(ty.clone())
                .len(len)
                .add_buffer(buffer)
                .nulls(nulls.clone())
                .build()
                .map_err(err)?;
            Ok(make_array(data))
        };
        match vals {
            OutVals::Bool(mut b) => {
                let len = b.len();
                let data = ArrayData::builder(DataType::Boolean)
                    .len(len)
                    .add_buffer(b.finish().into_inner())
                    .nulls(nulls.clone())
                    .build()
                    .map_err(err)?;
                Ok(make_array(data))
            }
            OutVals::I8(v) => prim(v.len(), Buffer::from_vec(v)),
            OutVals::I16(v) => prim(v.len(), Buffer::from_vec(v)),
            OutVals::I32(v) => prim(v.len(), Buffer::from_vec(v)),
            OutVals::I64(v) => prim(v.len(), Buffer::from_vec(v)),
            OutVals::U8(v) => prim(v.len(), Buffer::from_vec(v)),
            OutVals::U16(v) => prim(v.len(), Buffer::from_vec(v)),
            OutVals::U32(v) => prim(v.len(), Buffer::from_vec(v)),
            OutVals::U64(v) => prim(v.len(), Buffer::from_vec(v)),
            OutVals::F32(v) => prim(v.len(), Buffer::from_vec(v)),
            OutVals::F64(v) => prim(v.len(), Buffer::from_vec(v)),
            OutVals::Utf8(mut b) => Ok(Arc::new(b.finish())),
            OutVals::LargeUtf8(mut b) => Ok(Arc::new(b.finish())),
            OutVals::Opaque(parts) => {
                let refs: Vec<&dyn Array> = parts.iter().map(|a| a.as_ref()).collect();
                if refs.is_empty() {
                    return Ok(moruna_kernel::arrow::array::new_empty_array(ty));
                }
                moruna_kernel::arrow::compute::concat(&refs).map_err(err)
            }
        }
    }
}
