//! The gate's canonical form of a job's output and its content hash, gate-canon/1: the Rust
//! implementation of `bench/vm/canon.py`, which is the reference and documents the spec.
//!
//! A [`Canon`] takes an output's batches in any order and split ([`Canon::add`]), and two of
//! them merge ([`Canon::merge`]), so files can be read in parallel. [`Canon::finish`] gives the
//! canonical row count, `hash_exact`, `hash_all`, and the tolerant columns' rows in canonical
//! order for [`compare`].

use std::collections::HashMap;
use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

use arrow::array::{
    Array, ArrayRef, AsArray, BooleanArray, FixedSizeListArray, LargeListArray, ListArray,
    RecordBatch, StructArray,
};
use arrow::datatypes::{
    DataType, Float16Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type,
    Schema, TimeUnit, TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
    TimestampSecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub const TAG: &[u8] = b"gate-canon/1\0";
const CANON_NAN: u64 = 0x7FF8_0000_0000_0000;

/// What gate-canon/1 knows of a workload: its tolerant columns `(name, abs, rel)` and its
/// regrouping `(keys, sums)`, as `canon.py`'s `SPECS`.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub tolerant: &'static [(&'static str, f64, f64)],
    pub regroup: Option<(&'static [&'static str], &'static [&'static str])>,
}

pub fn spec(workload: &str) -> Option<Spec> {
    let plain = Spec {
        tolerant: &[],
        regroup: None,
    };
    Some(match workload {
        "score" => Spec {
            tolerant: &[("predicted_tip", 1.1e-6, 0.0)],
            regroup: None,
        },
        "occupancy" => Spec {
            tolerant: &[],
            regroup: Some((&["zone", "minute_of_day"], &["car_minutes"])),
        },
        "ov_shape" => Spec {
            tolerant: &[
                ("area_m2", 0.0, 1e-9),
                ("perimeter_m", 0.0, 1e-9),
                ("lon", 1e-9, 0.0),
                ("lat", 1e-9, 0.0),
                ("compactness", 0.0, 1e-9),
            ],
            regroup: None,
        },
        "ov_attrs" => Spec {
            tolerant: &[("volume_m3", 0.0, 1e-12)],
            regroup: None,
        },
        "enrich" | "audit" | "fw_minhash" | "fw_clean" => plain,
        _ => return None,
    })
}

/// The coarse type a column is hashed under (`canon.py`'s `type_class`).
pub fn type_class(t: &DataType) -> Result<String> {
    Ok(match t {
        DataType::Boolean => "bool".into(),
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => "int".into(),
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => "uint".into(),
        DataType::Float16 | DataType::Float32 | DataType::Float64 => "float".into(),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "str".into(),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => "bin".into(),
        DataType::Timestamp(u, _) => format!("ts:{}", unit(u)),
        DataType::List(f) | DataType::LargeList(f) | DataType::FixedSizeList(f, _) => {
            format!("list<{}>", type_class(f.data_type())?)
        }
        DataType::Struct(fields) => {
            let mut s = String::from("struct<");
            for (i, f) in fields.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&format!("{}:{}", f.name(), type_class(f.data_type())?));
            }
            s.push('>');
            s
        }
        other => {
            return Err(Error::Schema(format!(
                "gate-canon/1 has no encoding for {other}"
            )));
        }
    })
}

fn unit(u: &TimeUnit) -> &'static str {
    match u {
        TimeUnit::Second => "s",
        TimeUnit::Millisecond => "ms",
        TimeUnit::Microsecond => "us",
        TimeUnit::Nanosecond => "ns",
    }
}

fn signature(schema: &Schema, cols: &[usize]) -> Result<Vec<u8>> {
    let mut s = Vec::new();
    for &c in cols {
        let f = schema.field(c);
        s.extend_from_slice(f.name().as_bytes());
        s.push(0);
        s.extend_from_slice(type_class(f.data_type())?.as_bytes());
        s.push(0);
    }
    Ok(s)
}

fn float_bits(v: f64) -> u64 {
    if v.is_nan() { CANON_NAN } else { v.to_bits() }
}

/// Value `i` of `a`, as gate-canon/1 encodes it (`canon.py`'s `encode_value`).
pub fn encode(a: &dyn Array, i: usize, out: &mut Vec<u8>) -> Result<()> {
    if a.is_null(i) {
        out.push(0);
        return Ok(());
    }
    out.push(1);
    macro_rules! int {
        ($t:ty) => {
            out.extend_from_slice(&(a.as_primitive::<$t>().value(i) as i64).to_le_bytes())
        };
    }
    macro_rules! uint {
        ($t:ty) => {
            out.extend_from_slice(&(a.as_primitive::<$t>().value(i) as u64).to_le_bytes())
        };
    }
    match a.data_type() {
        DataType::Boolean => {
            let b: &BooleanArray = a.as_boolean();
            out.push(b.value(i) as u8);
        }
        DataType::Int8 => int!(Int8Type),
        DataType::Int16 => int!(Int16Type),
        DataType::Int32 => int!(Int32Type),
        DataType::Int64 => int!(Int64Type),
        DataType::UInt8 => uint!(UInt8Type),
        DataType::UInt16 => uint!(UInt16Type),
        DataType::UInt32 => uint!(UInt32Type),
        DataType::UInt64 => uint!(UInt64Type),
        DataType::Timestamp(TimeUnit::Second, _) => int!(TimestampSecondType),
        DataType::Timestamp(TimeUnit::Millisecond, _) => int!(TimestampMillisecondType),
        DataType::Timestamp(TimeUnit::Microsecond, _) => int!(TimestampMicrosecondType),
        DataType::Timestamp(TimeUnit::Nanosecond, _) => int!(TimestampNanosecondType),
        DataType::Float16 => {
            let v = f64::from(a.as_primitive::<Float16Type>().value(i).to_f32());
            out.extend_from_slice(&float_bits(v).to_le_bytes());
        }
        DataType::Float32 => {
            let v = f64::from(a.as_primitive::<Float32Type>().value(i));
            out.extend_from_slice(&float_bits(v).to_le_bytes());
        }
        DataType::Float64 => {
            let v = a.as_primitive::<Float64Type>().value(i);
            out.extend_from_slice(&float_bits(v).to_le_bytes());
        }
        DataType::Utf8 => bytes(a.as_string::<i32>().value(i).as_bytes(), out),
        DataType::LargeUtf8 => bytes(a.as_string::<i64>().value(i).as_bytes(), out),
        DataType::Utf8View => bytes(a.as_string_view().value(i).as_bytes(), out),
        DataType::Binary => bytes(a.as_binary::<i32>().value(i), out),
        DataType::LargeBinary => bytes(a.as_binary::<i64>().value(i), out),
        DataType::BinaryView => bytes(a.as_binary_view().value(i), out),
        DataType::List(_) => {
            let l: &ListArray = a.as_list();
            items(&l.value(i), out)?;
        }
        DataType::LargeList(_) => {
            let l: &LargeListArray = a.as_list();
            items(&l.value(i), out)?;
        }
        DataType::FixedSizeList(_, _) => {
            let l: &FixedSizeListArray = a.as_fixed_size_list();
            items(&l.value(i), out)?;
        }
        DataType::Struct(_) => {
            let s: &StructArray = a.as_struct();
            for c in s.columns() {
                encode(c.as_ref(), i, out)?;
            }
        }
        other => {
            return Err(Error::Schema(format!(
                "gate-canon/1 has no encoding for {other}"
            )));
        }
    }
    Ok(())
}

fn bytes(b: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(b.len() as u64).to_le_bytes());
    out.extend_from_slice(b);
}

fn items(a: &ArrayRef, out: &mut Vec<u8>) -> Result<()> {
    out.extend_from_slice(&(a.len() as u64).to_le_bytes());
    for j in 0..a.len() {
        encode(a.as_ref(), j, out)?;
    }
    Ok(())
}

fn row_hash(enc: &[u8]) -> u128 {
    let d = Sha256::digest(enc);
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    u128::from_le_bytes(b)
}

fn f64_at(a: &dyn Array, i: usize) -> Result<Option<f64>> {
    if a.is_null(i) {
        return Ok(None);
    }
    Ok(Some(match a.data_type() {
        DataType::Float64 => a.as_primitive::<Float64Type>().value(i),
        DataType::Float32 => f64::from(a.as_primitive::<Float32Type>().value(i)),
        other => {
            return Err(Error::Schema(format!(
                "a tolerant column must be floating point, not {other}"
            )));
        }
    }))
}

/// One tolerant row: the hash of its exact columns, then its tolerant values.
pub type TolerantRow = (u128, Vec<Option<f64>>);

/// An output being reduced to its canonical form.
pub struct Canon {
    spec: Spec,
    schema: Schema,
    exact: Vec<usize>,
    tolerant: Vec<usize>,
    /// Keep the tolerant rows (they cost memory a row).
    keep_rows: bool,
    rows: u64,
    sum_exact: u128,
    sum_all: u128,
    tolerant_rows: Vec<TolerantRow>,
    /// The regrouped rows: the keys' encoding, and the sums.
    groups: HashMap<Vec<u8>, Vec<i128>>,
    sums: Vec<usize>,
}

/// The canonical form, finished.
#[derive(Clone, Debug)]
pub struct Digest2 {
    pub rows: u64,
    pub hash_exact: String,
    pub hash_all: String,
    pub exact: Vec<String>,
    pub tolerant: Vec<String>,
    pub tolerant_rows: Vec<TolerantRow>,
}

impl Canon {
    /// A canonical form for `workload`'s output of `schema`. `keep_rows` keeps the tolerant
    /// columns' rows for [`compare`].
    pub fn new(workload: &str, schema: &Schema, keep_rows: bool) -> Result<Canon> {
        let spec = spec(workload)
            .ok_or_else(|| Error::Schema(format!("gate-canon/1 has no workload {workload:?}")))?;
        let index = |name: &str| {
            schema
                .index_of(name)
                .map_err(|_| Error::Schema(format!("the output has no column {name:?}")))
        };
        let mut sums = Vec::new();
        let mut names: Vec<usize> = (0..schema.fields().len()).collect();
        if let Some((keys, s)) = spec.regroup {
            // The regrouped table is the keys, then the sums.
            names = keys
                .iter()
                .chain(s.iter())
                .map(|n| index(n))
                .collect::<Result<_>>()?;
            sums = s.iter().map(|n| index(n)).collect::<Result<_>>()?;
            if names.len() != schema.fields().len() {
                return Err(Error::Schema(String::from(
                    "a regrouped output has columns besides its keys and sums",
                )));
            }
        }
        let is_tol = |c: &usize| {
            spec.tolerant
                .iter()
                .any(|(n, _, _)| *n == schema.field(*c).name())
        };
        let tolerant: Vec<usize> = names.iter().copied().filter(is_tol).collect();
        let exact: Vec<usize> = names.iter().copied().filter(|c| !is_tol(c)).collect();
        for c in &names {
            type_class(schema.field(*c).data_type())?;
        }
        Ok(Canon {
            spec,
            schema: schema.clone(),
            exact,
            tolerant,
            keep_rows,
            rows: 0,
            sum_exact: 0,
            sum_all: 0,
            tolerant_rows: Vec::new(),
            groups: HashMap::new(),
            sums,
        })
    }

    fn check_schema(&self, schema: &Schema) -> Result<()> {
        if schema.fields().len() != self.schema.fields().len() {
            return Err(Error::Schema(String::from(
                "batches of one output differ in columns",
            )));
        }
        for (a, b) in schema.fields().iter().zip(self.schema.fields()) {
            if a.name() != b.name() || type_class(a.data_type())? != type_class(b.data_type())? {
                return Err(Error::Schema(format!(
                    "batches of one output differ: {} {} and {} {}",
                    a.name(),
                    a.data_type(),
                    b.name(),
                    b.data_type()
                )));
            }
        }
        Ok(())
    }

    /// Takes a batch of the output.
    pub fn add(&mut self, batch: &RecordBatch) -> Result<()> {
        self.check_schema(batch.schema_ref())?;
        let cols = batch.columns();
        if self.spec.regroup.is_some() {
            let keys: Vec<usize> = self
                .exact
                .iter()
                .chain(&self.tolerant)
                .copied()
                .filter(|c| !self.sums.contains(c))
                .collect();
            let mut enc = Vec::new();
            for i in 0..batch.num_rows() {
                enc.clear();
                for &c in &keys {
                    encode(cols[c].as_ref(), i, &mut enc)?;
                }
                let mut vals = Vec::with_capacity(self.sums.len());
                for &c in &self.sums {
                    let a = cols[c].as_ref();
                    if a.is_null(i) || a.data_type() != &DataType::Int64 {
                        return Err(Error::Value(String::from(
                            "a regrouped sum must be a non-null int64",
                        )));
                    }
                    vals.push(i128::from(a.as_primitive::<Int64Type>().value(i)));
                }
                match self.groups.get_mut(&enc) {
                    Some(acc) => acc.iter_mut().zip(vals).for_each(|(a, v)| *a += v),
                    None => {
                        self.groups.insert(enc.clone(), vals);
                    }
                }
            }
            return Ok(());
        }
        let mut enc = Vec::new();
        let mut tail = Vec::new();
        for i in 0..batch.num_rows() {
            enc.clear();
            for &c in &self.exact {
                encode(cols[c].as_ref(), i, &mut enc)?;
            }
            let d_exact = row_hash(&enc);
            let d_all = if self.tolerant.is_empty() {
                d_exact
            } else {
                tail.clear();
                for c in cols {
                    encode(c.as_ref(), i, &mut tail)?;
                }
                row_hash(&tail)
            };
            self.rows += 1;
            self.sum_exact = self.sum_exact.wrapping_add(d_exact);
            self.sum_all = self.sum_all.wrapping_add(d_all);
            if self.keep_rows && !self.tolerant.is_empty() {
                let vals = self
                    .tolerant
                    .iter()
                    .map(|&c| f64_at(cols[c].as_ref(), i))
                    .collect::<Result<_>>()?;
                self.tolerant_rows.push((d_exact, vals));
            }
        }
        Ok(())
    }

    /// Takes another part of the same output.
    pub fn merge(&mut self, other: Canon) -> Result<()> {
        self.check_schema(&other.schema)?;
        self.rows += other.rows;
        self.sum_exact = self.sum_exact.wrapping_add(other.sum_exact);
        self.sum_all = self.sum_all.wrapping_add(other.sum_all);
        self.tolerant_rows.extend(other.tolerant_rows);
        for (k, v) in other.groups {
            match self.groups.get_mut(&k) {
                Some(acc) => acc.iter_mut().zip(v).for_each(|(a, b)| *a += b),
                None => {
                    self.groups.insert(k, v);
                }
            }
        }
        Ok(())
    }

    fn digest(&self, cols: &[usize], rows: u64, sum: u128) -> Result<String> {
        let mut h = Sha256::new();
        h.update(TAG);
        h.update(signature(&self.schema, cols)?);
        h.update(rows.to_le_bytes());
        h.update(sum.to_le_bytes());
        Ok(hex(&h.finalize()))
    }

    /// The canonical row count, the hashes and the tolerant rows in canonical order.
    pub fn finish(mut self) -> Result<Digest2> {
        if self.spec.regroup.is_some() {
            // Every regrouped column is exact: the keys' encodings, then each sum's.
            let mut sum = 0u128;
            let mut enc = Vec::new();
            for (k, sums) in &self.groups {
                enc.clear();
                enc.extend_from_slice(k);
                for s in sums {
                    let v = i64::try_from(*s)
                        .map_err(|_| Error::Value(String::from("a regrouped sum past int64")))?;
                    enc.push(1);
                    enc.extend_from_slice(&v.to_le_bytes());
                }
                sum = sum.wrapping_add(row_hash(&enc));
            }
            self.rows = self.groups.len() as u64;
            self.sum_exact = sum;
            self.sum_all = sum;
        }
        let all: Vec<usize> = self.exact.iter().chain(&self.tolerant).copied().collect();
        // Column order for the signature: the output's, or the regrouped table's.
        let mut all_ordered = all.clone();
        if self.spec.regroup.is_none() {
            all_ordered.sort_unstable();
        }
        let hash_exact = self.digest(&self.exact, self.rows, self.sum_exact)?;
        let hash_all = self.digest(&all_ordered, self.rows, self.sum_all)?;
        let mut rows = core::mem::take(&mut self.tolerant_rows);
        rows.sort_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| {
                for (x, y) in a.1.iter().zip(&b.1) {
                    let o = total_key(*x).cmp(&total_key(*y));
                    if o.is_ne() {
                        return o;
                    }
                }
                core::cmp::Ordering::Equal
            })
        });
        let name = |c: &usize| self.schema.field(*c).name().clone();
        Ok(Digest2 {
            rows: self.rows,
            hash_exact,
            hash_all,
            exact: self.exact.iter().map(name).collect(),
            tolerant: self.tolerant.iter().map(name).collect(),
            tolerant_rows: rows,
        })
    }
}

/// Floats in IEEE total order, nulls first (`canon.py`'s `_total_key`).
fn total_key(v: Option<f64>) -> (u8, i64) {
    match v {
        None => (0, 0),
        Some(v) => {
            let bits = v.to_bits() as i64;
            (1, if bits < 0 { bits ^ i64::MAX } else { bits })
        }
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// `canon.py`'s `within`.
pub fn within(a: Option<f64>, b: Option<f64>, abs: f64, rel: f64) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            if a.is_nan() || b.is_nan() {
                a.is_nan() && b.is_nan()
            } else if a.is_infinite() || b.is_infinite() {
                a == b
            } else {
                (a - b).abs() <= abs + rel * a.abs().max(b.abs())
            }
        }
        (None, None) => true,
        _ => false,
    }
}

/// How two canonical forms of one workload's output compare.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Comparison {
    pub ok: bool,
    pub rows: bool,
    pub hash_exact: bool,
    pub hash_all: bool,
    /// Tolerant values that differ at all, and the largest finite gap.
    pub differing: u64,
    pub max_abs: f64,
}

/// `canon.py`'s `compare`.
pub fn compare(workload: &str, a: &Digest2, b: &Digest2) -> Result<Comparison> {
    let spec = spec(workload)
        .ok_or_else(|| Error::Schema(format!("gate-canon/1 has no workload {workload:?}")))?;
    let mut out = Comparison {
        rows: a.rows == b.rows,
        hash_exact: a.hash_exact == b.hash_exact,
        hash_all: a.hash_all == b.hash_all,
        ..Comparison::default()
    };
    let mut ok = out.rows && out.hash_exact;
    if ok && !out.hash_all {
        if a.tolerant_rows.len() != b.tolerant_rows.len() || a.tolerant_rows.len() as u64 != a.rows
        {
            return Err(Error::Value(String::from(
                "the hashes differ and the tolerant rows were not kept",
            )));
        }
        for (ra, rb) in a.tolerant_rows.iter().zip(&b.tolerant_rows) {
            if ra.0 != rb.0 {
                ok = false;
            }
            for ((name, x), y) in a.tolerant.iter().zip(&ra.1).zip(&rb.1) {
                let (abs, rel) = spec
                    .tolerant
                    .iter()
                    .find(|(n, _, _)| n == name)
                    .map(|(_, a, r)| (*a, *r))
                    .unwrap_or((0.0, 0.0));
                if !within(*x, *y, abs, rel) {
                    ok = false;
                }
                let same = match (x, y) {
                    (Some(x), Some(y)) => x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()),
                    (None, None) => true,
                    _ => false,
                };
                if !same {
                    out.differing += 1;
                    if let (Some(x), Some(y)) = (x, y)
                        && x.is_finite()
                        && y.is_finite()
                    {
                        out.max_abs = out.max_abs.max((x - y).abs());
                    }
                }
            }
        }
    }
    out.ok = ok;
    Ok(out)
}

/// The canonical form of `batches`, all of one output.
pub fn canonical(workload: &str, batches: &[RecordBatch], keep_rows: bool) -> Result<Digest2> {
    let Some(first) = batches.first() else {
        return Err(Error::Schema(String::from("no batches")));
    };
    let mut c = Canon::new(workload, first.schema_ref(), keep_rows)?;
    for b in batches {
        c.add(b)?;
    }
    c.finish()
}

impl Digest2 {
    /// The digest as one JSON object, as `canon.py` prints it.
    pub fn json(&self) -> String {
        let list = |v: &[String]| {
            let items: Vec<String> = v
                .iter()
                .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
                .collect();
            format!("[{}]", items.join(", "))
        };
        format!(
            "{{\"rows\": {}, \"hash_exact\": \"{}\", \"hash_all\": \"{}\", \"exact\": {}, \"tolerant\": {}}}",
            self.rows,
            self.hash_exact,
            self.hash_all,
            list(&self.exact),
            list(&self.tolerant)
        )
    }
}

impl Comparison {
    pub fn json(&self) -> String {
        format!(
            "{{\"ok\": {}, \"rows\": {}, \"hash_exact\": {}, \"hash_all\": {}, \"differing\": {}, \"max_abs\": {}}}",
            self.ok, self.rows, self.hash_exact, self.hash_all, self.differing, self.max_abs
        )
    }
}

impl core::fmt::Display for Comparison {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.json())
    }
}

/// For messages: the error as a string.
pub fn describe(e: &Error) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::vec;

    use arrow::array::{
        BinaryArray, BooleanArray, Float32Array, Float64Array, Int64Array, LargeStringArray,
        StringArray, TimestampMicrosecondArray, UInt8Array,
    };
    use arrow::datatypes::{Field, Fields};

    fn batch(cols: Vec<(&str, ArrayRef)>) -> RecordBatch {
        RecordBatch::try_from_iter(cols).unwrap()
    }

    #[test]
    fn every_type_has_an_encoding() {
        let items = Arc::new(Field::new("item", DataType::Int64, true));
        let list = arrow::array::ListArray::from_iter_primitive::<Int64Type, _, _>(vec![
            Some(vec![Some(1), None]),
            None,
        ]);
        let st = StructArray::new(
            Fields::from(vec![Field::new("x", DataType::Int64, true)]),
            vec![Arc::new(Int64Array::from(vec![1, 2])) as ArrayRef],
            Some(vec![true, false].into()),
        );
        let b = batch(vec![
            (
                "b",
                Arc::new(BooleanArray::from(vec![Some(true), None])) as ArrayRef,
            ),
            ("u", Arc::new(UInt8Array::from(vec![1, 2]))),
            (
                "f",
                Arc::new(Float32Array::from(vec![Some(1.5), Some(f32::NAN)])),
            ),
            ("s", Arc::new(StringArray::from(vec![Some("x"), None]))),
            ("ls", Arc::new(LargeStringArray::from(vec!["a", "b"]))),
            ("bin", Arc::new(BinaryArray::from(vec![&b"\0"[..], b"z"]))),
            ("t", Arc::new(TimestampMicrosecondArray::from(vec![1, 2]))),
            ("l", Arc::new(list)),
            ("st", Arc::new(st)),
        ]);
        let _ = items;
        let d = canonical("enrich", core::slice::from_ref(&b), false).unwrap();
        assert_eq!(d.rows, 2);
        assert!(d.json().contains("\"rows\": 2"));
        // A second part with the same rows doubles the count and keeps the types.
        let mut c = Canon::new("enrich", b.schema_ref(), false).unwrap();
        c.add(&b).unwrap();
        let mut c2 = Canon::new("enrich", b.schema_ref(), false).unwrap();
        c2.add(&b).unwrap();
        c.merge(c2).unwrap();
        assert_eq!(c.finish().unwrap().rows, 4);
        assert_eq!(
            type_class(&DataType::Timestamp(TimeUnit::Nanosecond, None)).unwrap(),
            "ts:ns"
        );
        assert!(type_class(&DataType::Date32).is_err());
    }

    #[test]
    fn refusals_and_comparisons() {
        let i = |v: Vec<i64>| Arc::new(Int64Array::from(v)) as ArrayRef;
        let b = batch(vec![
            ("zone", i(vec![1, 1])),
            ("minute_of_day", i(vec![2, 2])),
            ("car_minutes", i(vec![3, 4])),
        ]);
        let d = canonical("occupancy", core::slice::from_ref(&b), false).unwrap();
        assert_eq!(d.rows, 1);
        assert!(canonical("nope", core::slice::from_ref(&b), false).is_err());
        assert!(canonical("occupancy", &[], false).is_err());
        let extra = batch(vec![
            ("zone", i(vec![1])),
            ("minute_of_day", i(vec![2])),
            ("car_minutes", i(vec![3])),
            ("x", i(vec![0])),
        ]);
        assert!(canonical("occupancy", &[extra], false).is_err());
        let other = batch(vec![("zone", i(vec![1])), ("minute_of_day", i(vec![2]))]);
        let mut c = Canon::new("occupancy", b.schema_ref(), false).unwrap();
        assert!(c.add(&other).is_err());
        let wrong = batch(vec![
            ("zone", i(vec![1])),
            ("minute_of_day", i(vec![2])),
            (
                "car_minutes",
                Arc::new(StringArray::from(vec!["x"])) as ArrayRef,
            ),
        ]);
        assert!(c.check_schema(wrong.schema_ref()).is_err());
        // Score: a tolerant column; hashes that differ with no rows kept cannot be compared.
        let f = |v: Vec<f64>| Arc::new(Float64Array::from(v)) as ArrayRef;
        let s1 = batch(vec![
            ("vendor_id", i(vec![1])),
            ("predicted_tip", f(vec![0.5])),
        ]);
        let s2 = batch(vec![
            ("vendor_id", i(vec![1])),
            ("predicted_tip", f(vec![0.5000001])),
        ]);
        let s3 = batch(vec![
            ("vendor_id", i(vec![1])),
            ("predicted_tip", f(vec![0.6])),
        ]);
        let (a, b2, c3) = (
            canonical("score", core::slice::from_ref(&s1), true).unwrap(),
            canonical("score", core::slice::from_ref(&s2), true).unwrap(),
            canonical("score", &[s3], true).unwrap(),
        );
        let ok = compare("score", &a, &b2).unwrap();
        assert!(ok.ok && ok.differing == 1 && ok.max_abs > 0.0);
        assert!(!compare("score", &a, &c3).unwrap().ok);
        let bare = canonical("score", &[s2], false).unwrap();
        assert!(compare("score", &a, &bare).is_err());
        assert!(compare("nope", &a, &a).is_err());
        assert!(
            compare("score", &a, &a)
                .unwrap()
                .json()
                .contains("\"ok\": true")
        );
        assert!(within(Some(f64::INFINITY), Some(f64::INFINITY), 0.0, 0.0));
        assert!(!within(Some(f64::NAN), Some(1.0), 1.0, 0.0));
        assert!(within(None, None, 0.0, 0.0) && !within(None, Some(1.0), 0.0, 0.0));
        assert_eq!(describe(&Error::Value("v".into())), "value: v");
        // A tolerant column must be floating point.
        let ints = batch(vec![
            ("vendor_id", i(vec![1])),
            ("predicted_tip", i(vec![1])),
        ]);
        assert!(canonical("score", &[ints], true).is_err());
    }
}
