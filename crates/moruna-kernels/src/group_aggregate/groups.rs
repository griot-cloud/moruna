//! Which group each row belongs to: the dense path over a declared key domain, and the hash
//! path for any keys. Both give each group a number, `0..len`, that the accumulators index, and
//! both give the groups back in ascending key order (spec `group_aggregate.md`, ordering).

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, ArrayRef, AsArray, Int64Array, UInt32Array};
use moruna_kernel::arrow::compute::{SortOptions, cast, concat, take};
use moruna_kernel::arrow::datatypes::{
    DataType, Date32Type, Date64Type, Decimal32Type, Decimal64Type, DurationMicrosecondType,
    DurationMillisecondType, DurationNanosecondType, DurationSecondType, Float16Type, Float32Type,
    Float64Type, Int8Type, Int16Type, Int32Type, Int64Type, Time32MillisecondType,
    Time32SecondType, Time64MicrosecondType, Time64NanosecondType, TimeUnit,
    TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
    TimestampSecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use moruna_kernel::arrow::row::{RowConverter, SortField};
use moruna_kernel::{MorunaError, Result};

use super::failed;

/// An inclusive range of an integer key, declared by the job (spec: the dense path).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyRange {
    /// The least value.
    pub lo: i64,
    /// The greatest value.
    pub hi: i64,
}

impl KeyRange {
    /// Its values, and one more slot for null.
    pub(crate) fn slots(&self) -> u128 {
        (self.hi as i128 - self.lo as i128 + 2) as u128
    }
}

/// The ascending order every output follows: keys in column order, nulls last.
pub(crate) const ASCENDING: SortOptions = SortOptions {
    descending: false,
    nulls_first: false,
};

/// Whether `dt` is a key type of `@1`: fixed width, strings or binaries.
pub(crate) fn key_type_ok(dt: &DataType) -> bool {
    use DataType::*;
    matches!(
        dt,
        Boolean
            | Int8
            | Int16
            | Int32
            | Int64
            | UInt8
            | UInt16
            | UInt32
            | UInt64
            | Float16
            | Float32
            | Float64
            | Date32
            | Date64
            | Time32(_)
            | Time64(_)
            | Timestamp(..)
            | Duration(_)
            | Decimal32(..)
            | Decimal64(..)
            | Decimal128(..)
            | Decimal256(..)
            | FixedSizeBinary(_)
            | Utf8
            | LargeUtf8
            | Utf8View
            | Binary
            | LargeBinary
            | BinaryView
    )
}

/// Whether `dt` is an integer type, which alone may have a declared range.
pub(crate) fn dense_type_ok(dt: &DataType) -> bool {
    dt.is_integer()
}

/// Float keys in their canonical form: `−0.0` as `+0.0` and every NaN as the one positive
/// quiet NaN, so equal values group together and order is IEEE total order on the rest.
pub(crate) fn canonical_key(key: &ArrayRef) -> ArrayRef {
    match key.data_type() {
        DataType::Float64 => Arc::new(key.as_primitive::<Float64Type>().unary::<_, Float64Type>(
            |v| {
                if v.is_nan() {
                    f64::NAN
                } else if v == 0.0 {
                    0.0
                } else {
                    v
                }
            },
        )),
        DataType::Float32 => Arc::new(key.as_primitive::<Float32Type>().unary::<_, Float32Type>(
            |v| {
                if v.is_nan() {
                    f32::NAN
                } else if v == 0.0 {
                    0.0
                } else {
                    v
                }
            },
        )),
        DataType::Float16 => Arc::new(key.as_primitive::<Float16Type>().unary::<_, Float16Type>(
            |v| {
                let b = v.to_bits();
                let canon = if b & 0x7c00 == 0x7c00 && b & 0x03ff != 0 {
                    0x7e00
                } else if b & 0x7fff == 0 {
                    0
                } else {
                    b
                };
                <Float16Type as moruna_kernel::arrow::datatypes::ArrowPrimitiveType>::Native::from_bits(
                    canon,
                )
            },
        )),
        _ => Arc::clone(key),
    }
}

/// A fast hash for group keys: FxHash's mixing, finished with MurmurHash3's avalanche so the
/// table's low and high bits are both good.
#[derive(Default, Clone, Copy)]
pub(crate) struct Mix(u64);

impl Mix {
    #[inline]
    fn add(&mut self, x: u64) {
        self.0 = (self.0.rotate_left(5) ^ x).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl Hasher for Mix {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            self.add(u64::from_le_bytes(*c));
        }
        if !rest.is_empty() {
            let mut b = [0u8; 8];
            b[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(b) ^ ((rest.len() as u64) << 56));
        }
    }
    #[inline]
    fn write_u8(&mut self, x: u8) {
        self.add(x as u64);
    }
    #[inline]
    fn write_u64(&mut self, x: u64) {
        self.add(x);
    }
    #[inline]
    fn write_usize(&mut self, x: usize) {
        self.add(x as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        let mut h = self.0;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        h ^ (h >> 33)
    }
}

type MixMap<K> = HashMap<K, u32, BuildHasherDefault<Mix>>;

/// The keys of up to four key columns of at most 8 bytes each, as their bits and which are null.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Packed {
    v: [u64; 4],
    nulls: u8,
}

impl Hash for Packed {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        for v in self.v {
            state.write_u64(v);
        }
        state.write_u8(self.nulls);
    }
}

/// Whether a key of type `dt` packs into 64 bits.
fn packs(dt: &DataType) -> bool {
    use DataType::*;
    key_type_ok(dt)
        && matches!(
            dt,
            Boolean
                | Int8
                | Int16
                | Int32
                | Int64
                | UInt8
                | UInt16
                | UInt32
                | UInt64
                | Float16
                | Float32
                | Float64
                | Date32
                | Date64
                | Time32(_)
                | Time64(_)
                | Timestamp(..)
                | Duration(_)
                | Decimal32(..)
                | Decimal64(..)
        )
}

/// A packable key column's values as 64 bits each (nulls' slots are whatever the array holds;
/// the caller masks them).
fn bits(key: &ArrayRef) -> Result<Vec<u64>> {
    macro_rules! prim {
        ($t:ty, $f:expr) => {
            key.as_primitive::<$t>().values().iter().map($f).collect()
        };
    }
    use DataType::*;
    Ok(match key.data_type() {
        Boolean => key.as_boolean().values().iter().map(u64::from).collect(),
        Int8 => prim!(Int8Type, |&v| v as u64),
        Int16 => prim!(Int16Type, |&v| v as u64),
        Int32 => prim!(Int32Type, |&v| v as u64),
        Int64 => prim!(Int64Type, |&v| v as u64),
        UInt8 => prim!(UInt8Type, |&v| v as u64),
        UInt16 => prim!(UInt16Type, |&v| v as u64),
        UInt32 => prim!(UInt32Type, |&v| v as u64),
        UInt64 => prim!(UInt64Type, |&v| v),
        Float16 => prim!(Float16Type, |&v| v.to_bits() as u64),
        Float32 => prim!(Float32Type, |&v| v.to_bits() as u64),
        Float64 => prim!(Float64Type, |&v| v.to_bits()),
        Date32 => prim!(Date32Type, |&v| v as u64),
        Date64 => prim!(Date64Type, |&v| v as u64),
        Time32(TimeUnit::Second) => prim!(Time32SecondType, |&v| v as u64),
        Time32(_) => prim!(Time32MillisecondType, |&v| v as u64),
        Time64(TimeUnit::Microsecond) => prim!(Time64MicrosecondType, |&v| v as u64),
        Time64(_) => prim!(Time64NanosecondType, |&v| v as u64),
        Timestamp(TimeUnit::Second, _) => prim!(TimestampSecondType, |&v| v as u64),
        Timestamp(TimeUnit::Millisecond, _) => prim!(TimestampMillisecondType, |&v| v as u64),
        Timestamp(TimeUnit::Microsecond, _) => prim!(TimestampMicrosecondType, |&v| v as u64),
        Timestamp(TimeUnit::Nanosecond, _) => prim!(TimestampNanosecondType, |&v| v as u64),
        Duration(TimeUnit::Second) => prim!(DurationSecondType, |&v| v as u64),
        Duration(TimeUnit::Millisecond) => prim!(DurationMillisecondType, |&v| v as u64),
        Duration(TimeUnit::Microsecond) => prim!(DurationMicrosecondType, |&v| v as u64),
        Duration(TimeUnit::Nanosecond) => prim!(DurationNanosecondType, |&v| v as u64),
        Decimal32(..) => prim!(Decimal32Type, |&v| v as u64),
        Decimal64(..) => prim!(Decimal64Type, |&v| v as u64),
        other => return Err(failed(format!("a key of type {other} does not pack"))),
    })
}

/// How the hash path finds a row's group.
enum KeyMap {
    /// Up to four keys of at most 8 bytes: their bits.
    Packed(MixMap<Packed>),
    /// Any keys: Arrow's row format, which is also their order.
    Rows {
        converter: RowConverter,
        map: MixMap<Box<[u8]>>,
        bytes: u64,
    },
}

/// The hash path's groups: the map from key to group, and each group's key, in group order.
pub(crate) struct HashGroups {
    types: Vec<DataType>,
    map: KeyMap,
    chunks: Vec<Vec<ArrayRef>>,
    len: usize,
}

/// A value outside its declared range: the dense path declines it (spec: the dense path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Decline {
    /// The key's position.
    pub key: usize,
    /// The value, as text.
    pub value: String,
    /// Its declared range.
    pub range: KeyRange,
}

/// The dense path's groups: one cell for each combination of key values and null.
pub(crate) struct DenseGroups {
    ranges: Vec<KeyRange>,
    strides: Vec<u64>,
    cells: usize,
}

/// The groups of one instance, on the path it is on.
pub(crate) enum Groups {
    /// A declared domain, small enough.
    Dense(DenseGroups),
    /// Any keys.
    Hash(HashGroups),
}

impl DenseGroups {
    /// The cells of `ranges`, last key fastest, so cell order is key order.
    pub(crate) fn new(ranges: &[KeyRange]) -> DenseGroups {
        let mut strides = vec![1u64; ranges.len()];
        for i in (0..ranges.len().saturating_sub(1)).rev() {
            strides[i] = strides[i + 1] * ranges[i + 1].slots() as u64;
        }
        let cells = ranges.first().map_or(1, |r| strides[0] * r.slots() as u64) as usize;
        DenseGroups {
            ranges: ranges.to_vec(),
            strides,
            cells,
        }
    }

    /// Each row's cell into `ids`, or the first value outside its range. Nothing is changed by
    /// a decline but `ids`.
    fn assign(&self, keys: &[ArrayRef], ids: &mut Vec<u32>) -> core::result::Result<(), Decline> {
        let n = keys.first().map_or(0, |k| k.len());
        ids.clear();
        ids.resize(n, 0);
        for (i, key) in keys.iter().enumerate() {
            let r = self.ranges[i];
            let stride = self.strides[i];
            let null_cell = ((r.hi - r.lo + 1) as u64 * stride) as u32;
            macro_rules! walk {
                ($t:ty, $wide:expr) => {{
                    let arr = key.as_primitive::<$t>();
                    let values = arr.values();
                    let decline = |v: String| Decline {
                        key: i,
                        value: v,
                        range: r,
                    };
                    match arr.nulls().filter(|n| n.null_count() > 0) {
                        None => {
                            for (id, &v) in ids.iter_mut().zip(values.iter()) {
                                let x: Option<i64> = $wide(v);
                                match x {
                                    Some(x) if x >= r.lo && x <= r.hi => {
                                        *id += (x.wrapping_sub(r.lo) as u64 * stride) as u32
                                    }
                                    _ => return Err(decline(v.to_string())),
                                }
                            }
                        }
                        Some(nulls) => {
                            for (row, (id, &v)) in ids.iter_mut().zip(values.iter()).enumerate() {
                                if nulls.is_null(row) {
                                    *id += null_cell;
                                    continue;
                                }
                                let x: Option<i64> = $wide(v);
                                match x {
                                    Some(x) if x >= r.lo && x <= r.hi => {
                                        *id += (x.wrapping_sub(r.lo) as u64 * stride) as u32
                                    }
                                    _ => return Err(decline(v.to_string())),
                                }
                            }
                        }
                    }
                }};
            }
            match key.data_type() {
                DataType::Int8 => walk!(Int8Type, |v: i8| Some(v as i64)),
                DataType::Int16 => walk!(Int16Type, |v: i16| Some(v as i64)),
                DataType::Int32 => walk!(Int32Type, |v: i32| Some(v as i64)),
                DataType::Int64 => walk!(Int64Type, |v: i64| Some(v)),
                DataType::UInt8 => walk!(UInt8Type, |v: u8| Some(v as i64)),
                DataType::UInt16 => walk!(UInt16Type, |v: u16| Some(v as i64)),
                DataType::UInt32 => walk!(UInt32Type, |v: u32| Some(v as i64)),
                DataType::UInt64 => walk!(UInt64Type, |v: u64| i64::try_from(v).ok()),
                // The plan admits only integer keys to the dense path.
                _ => unreachable!("a dense key is an integer"),
            }
        }
        Ok(())
    }

    /// The occupied cells in ascending order, and their keys as arrays of `types`.
    fn emit(&self, occupied: &[i64], types: &[DataType]) -> Result<(Vec<u32>, Vec<ArrayRef>)> {
        let order: Vec<u32> = occupied
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .map(|(cell, _)| cell as u32)
            .collect();
        let keys = self.keys_of(&order, types)?;
        Ok((order, keys))
    }

    /// The keys of `cells`, as arrays of `types`.
    fn keys_of(&self, cells: &[u32], types: &[DataType]) -> Result<Vec<ArrayRef>> {
        let mut keys = Vec::with_capacity(self.ranges.len());
        for (i, r) in self.ranges.iter().enumerate() {
            let slots = r.slots() as u64;
            let values: Int64Array = cells
                .iter()
                .map(|&c| {
                    let slot = (c as u64 / self.strides[i]) % slots;
                    (slot < slots - 1).then(|| r.lo.wrapping_add(slot as i64))
                })
                .collect();
            keys.push(cast(&values, &types[i]).map_err(arrow_err)?);
        }
        Ok(keys)
    }
}

impl HashGroups {
    /// Empty groups for keys of `types`.
    pub(crate) fn new(types: &[DataType]) -> Result<HashGroups> {
        let map = if types.len() <= 4 && types.iter().all(packs) {
            KeyMap::Packed(MixMap::default())
        } else {
            let fields = types
                .iter()
                .map(|t| SortField::new_with_options(t.clone(), ASCENDING))
                .collect();
            KeyMap::Rows {
                converter: RowConverter::new(fields).map_err(arrow_err)?,
                map: MixMap::default(),
                bytes: 0,
            }
        };
        Ok(HashGroups {
            types: types.to_vec(),
            map,
            chunks: vec![Vec::new(); types.len()],
            len: 0,
        })
    }

    /// Each row's group into `ids`, making a group for each key not seen before.
    fn assign(&mut self, keys: &[ArrayRef], ids: &mut Vec<u32>) -> Result<()> {
        let n = keys.first().map_or(0, |k| k.len());
        ids.clear();
        ids.reserve(n);
        let mut fresh: Vec<u32> = Vec::new();
        let mut next = self.len as u32;
        match &mut self.map {
            KeyMap::Packed(map) => {
                let columns = keys.iter().map(bits).collect::<Result<Vec<_>>>()?;
                let nulls: Vec<_> = keys
                    .iter()
                    .map(|k| k.logical_nulls().filter(|n| n.null_count() > 0))
                    .collect();
                for row in 0..n {
                    let mut p = Packed::default();
                    for (i, column) in columns.iter().enumerate() {
                        match &nulls[i] {
                            Some(nn) if nn.is_null(row) => p.nulls |= 1 << i,
                            _ => p.v[i] = column[row],
                        }
                    }
                    let id = *map.entry(p).or_insert_with(|| {
                        fresh.push(row as u32);
                        next += 1;
                        next - 1
                    });
                    ids.push(id);
                }
            }
            KeyMap::Rows {
                converter,
                map,
                bytes,
            } => {
                let rows = converter.convert_columns(keys).map_err(arrow_err)?;
                for row in 0..n {
                    let r = rows.row(row);
                    let id = match map.get(r.as_ref()) {
                        Some(&id) => id,
                        None => {
                            let key: Box<[u8]> = r.as_ref().into();
                            *bytes += key.len() as u64;
                            map.insert(key, next);
                            fresh.push(row as u32);
                            next += 1;
                            next - 1
                        }
                    };
                    ids.push(id);
                }
            }
        }
        if !fresh.is_empty() {
            let at = UInt32Array::from(fresh);
            for (chunks, key) in self.chunks.iter_mut().zip(keys) {
                chunks.push(take(key, &at, None).map_err(arrow_err)?);
            }
        }
        self.len = next as usize;
        Ok(())
    }

    /// Every group's key, in group order.
    fn keys(&self) -> Result<Vec<ArrayRef>> {
        self.chunks
            .iter()
            .zip(&self.types)
            .map(|(chunks, ty)| {
                if chunks.is_empty() {
                    Ok(moruna_kernel::arrow::array::new_empty_array(ty))
                } else {
                    let parts: Vec<&dyn Array> = chunks.iter().map(|c| c.as_ref()).collect();
                    concat(&parts).map_err(arrow_err)
                }
            })
            .collect()
    }

    /// The groups in ascending key order, and their keys in that order.
    fn emit(&self) -> Result<(Vec<u32>, Vec<ArrayRef>)> {
        let keys = self.keys()?;
        let fields = self
            .types
            .iter()
            .map(|t| SortField::new_with_options(t.clone(), ASCENDING))
            .collect();
        let converter = RowConverter::new(fields).map_err(arrow_err)?;
        let rows = converter.convert_columns(&keys).map_err(arrow_err)?;
        let mut order: Vec<u32> = (0..self.len as u32).collect();
        order.sort_unstable_by(|&a, &b| rows.row(a as usize).cmp(&rows.row(b as usize)));
        let at = UInt32Array::from(order.clone());
        let keys = keys
            .iter()
            .map(|k| take(k, &at, None).map_err(arrow_err))
            .collect::<Result<Vec<_>>>()?;
        Ok((order, keys))
    }

    /// Bytes the map and the keys hold, estimated from their sizes.
    fn bytes(&self) -> u64 {
        let keys: u64 = self
            .chunks
            .iter()
            .flatten()
            .map(|c| c.get_array_memory_size() as u64)
            .sum();
        let map = match &self.map {
            KeyMap::Packed(map) => {
                map.capacity() as u64 * (core::mem::size_of::<Packed>() + 5) as u64
            }
            KeyMap::Rows { map, bytes, .. } => {
                map.capacity() as u64 * (core::mem::size_of::<(Box<[u8]>, u32)>() + 1) as u64
                    + bytes
            }
        };
        keys + map
    }
}

impl Groups {
    /// The number of groups (on the dense path, of cells).
    pub(crate) fn len(&self) -> usize {
        match self {
            Groups::Dense(d) => d.cells,
            Groups::Hash(h) => h.len,
        }
    }

    /// Whether the groups are on the dense path.
    pub(crate) fn is_dense(&self) -> bool {
        matches!(self, Groups::Dense(_))
    }

    /// Each row's group into `ids`. On the dense path a value outside its range declines the
    /// whole batch, and nothing has changed but `ids`.
    pub(crate) fn assign(
        &mut self,
        keys: &[ArrayRef],
        ids: &mut Vec<u32>,
    ) -> Result<core::result::Result<(), Decline>> {
        match self {
            Groups::Dense(d) => Ok(d.assign(keys, ids)),
            Groups::Hash(h) => h.assign(keys, ids).map(Ok),
        }
    }

    /// Leave the dense path: the hash path's groups holding the occupied cells, numbered in
    /// cell order, and for each new group the cell it was.
    pub(crate) fn to_hash(
        &self,
        occupied: &[i64],
        types: &[DataType],
    ) -> Result<(Groups, Vec<u32>)> {
        let Groups::Dense(d) = self else {
            return Err(MorunaError::Plan(
                "moruna.group_aggregate@1: only the dense path declines".into(),
            ));
        };
        let (cells, keys) = d.emit(occupied, types)?;
        let mut hash = HashGroups::new(types)?;
        let mut ids = Vec::new();
        if !cells.is_empty() {
            hash.assign(&keys, &mut ids)?;
        }
        Ok((Groups::Hash(hash), cells))
    }

    /// The occupied groups in ascending key order, and their keys in that order. `occupied`
    /// is each group's row count; on the hash path every group is occupied.
    pub(crate) fn emit(
        &self,
        occupied: &[i64],
        types: &[DataType],
    ) -> Result<(Vec<u32>, Vec<ArrayRef>)> {
        match self {
            Groups::Dense(d) => d.emit(occupied, types),
            Groups::Hash(h) => h.emit(),
        }
    }

    /// Bytes beyond the accumulators': the hash path's map and keys.
    pub(crate) fn bytes(&self) -> u64 {
        match self {
            Groups::Dense(_) => 0,
            Groups::Hash(h) => h.bytes(),
        }
    }
}

pub(crate) fn arrow_err(e: moruna_kernel::arrow::error::ArrowError) -> MorunaError {
    failed(format!("{e}"))
}
