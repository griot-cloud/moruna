//! Payloads and the conversions between a table column and a tensor (contracts d.4, e.2, e.3,
//! f.1, f.3, f.4, f.5). `unsafe` is permitted here for building Arrow buffers over a tensor's
//! bytes, tensors over an array's bytes, and the `*_in` constructors (section l).

use std::sync::Arc;

use arrow::array::{Array, ArrayData, ArrayRef, FixedSizeListArray, make_array};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;

use crate::buffer::{Allocator, arena_tier_of};
use crate::error::{MorunaError, ConvertError};
use crate::tensor::ManagedTensor;
use crate::tier::Tier;

/// Element type of a tensor. Maps one-to-one to DLPack dtype codes and to the
/// Arrow primitive types listed in section e.3.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
#[allow(missing_docs)]
pub enum DType {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F16,
    BF16,
    F32,
    F64,
    Bool,
}

impl DType {
    /// Bytes per element (b: item size).
    pub fn item_size(&self) -> usize {
        match self {
            DType::I8 | DType::U8 | DType::Bool => 1,
            DType::I16 | DType::U16 | DType::F16 | DType::BF16 => 2,
            DType::I32 | DType::U32 | DType::F32 => 4,
            DType::I64 | DType::U64 | DType::F64 => 8,
        }
    }

    /// The `MRB1` dtype code (e.4): I8=0, I16=1, I32=2, I64=3, U8=4, U16=5, U32=6, U64=7,
    /// F16=8, BF16=9, F32=10, F64=11, Bool=12.
    pub fn code(&self) -> u8 {
        match self {
            DType::I8 => 0,
            DType::I16 => 1,
            DType::I32 => 2,
            DType::I64 => 3,
            DType::U8 => 4,
            DType::U16 => 5,
            DType::U32 => 6,
            DType::U64 => 7,
            DType::F16 => 8,
            DType::BF16 => 9,
            DType::F32 => 10,
            DType::F64 => 11,
            DType::Bool => 12,
        }
    }

    /// The dtype for an `MRB1` code; `None` for an unknown code.
    pub fn from_code(code: u8) -> Option<DType> {
        Some(match code {
            0 => DType::I8,
            1 => DType::I16,
            2 => DType::I32,
            3 => DType::I64,
            4 => DType::U8,
            5 => DType::U16,
            6 => DType::U32,
            7 => DType::U64,
            8 => DType::F16,
            9 => DType::BF16,
            10 => DType::F32,
            11 => DType::F64,
            12 => DType::Bool,
            _ => return None,
        })
    }

    /// Every dtype, in code order.
    pub const ALL: [DType; 13] = [
        DType::I8,
        DType::I16,
        DType::I32,
        DType::I64,
        DType::U8,
        DType::U16,
        DType::U32,
        DType::U64,
        DType::F16,
        DType::BF16,
        DType::F32,
        DType::F64,
        DType::Bool,
    ];

    /// The Arrow primitive type this dtype converts to (e.3); `None` for `BF16` and `Bool`,
    /// which have no zero-copy Arrow form.
    pub fn arrow_type(&self) -> Option<DataType> {
        Some(match self {
            DType::I8 => DataType::Int8,
            DType::I16 => DataType::Int16,
            DType::I32 => DataType::Int32,
            DType::I64 => DataType::Int64,
            DType::U8 => DataType::UInt8,
            DType::U16 => DataType::UInt16,
            DType::U32 => DataType::UInt32,
            DType::U64 => DataType::UInt64,
            DType::F16 => DataType::Float16,
            DType::F32 => DataType::Float32,
            DType::F64 => DataType::Float64,
            DType::BF16 | DType::Bool => return None,
        })
    }

    /// The dtype an Arrow primitive type maps to (e.3); `None` for a type that is not a
    /// numeric column (`Boolean` included: a bitmap is not one byte per value).
    pub fn of_arrow(dt: &DataType) -> Option<DType> {
        Some(match dt {
            DataType::Int8 => DType::I8,
            DataType::Int16 => DType::I16,
            DataType::Int32 => DType::I32,
            DataType::Int64 => DType::I64,
            DataType::UInt8 => DType::U8,
            DataType::UInt16 => DType::U16,
            DataType::UInt32 => DType::U32,
            DataType::UInt64 => DType::U64,
            DataType::Float16 => DType::F16,
            DataType::Float32 => DType::F32,
            DataType::Float64 => DType::F64,
            _ => return None,
        })
    }
}

/// The data inside a morsel.
pub enum Payload {
    /// An Arrow record batch and the tier its buffers are in.
    Table(RecordBatch, Tier),
    /// A DLPack-backed tensor and the tier its bytes are in.
    Tensor(ManagedTensor, Tier),
}

/// What a kernel or sink wants to receive.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum PayloadKind {
    /// A record batch.
    Table,
    /// A tensor.
    Tensor,
    /// Whichever the producer made.
    Either,
}

/// Which tier a consumer wants its payload resident in.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum TierPref {
    /// `Host` or `PinnedHost`.
    Host,
    /// `Device(_)`.
    Device,
    /// Any resident tier.
    Any,
}

/// What a kernel or sink wants: a payload kind and a tier preference.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct PayloadSpec {
    /// The payload kind wanted.
    pub kind: PayloadKind,
    /// The tier wanted.
    pub tier: TierPref,
}

/// Schema of a source's output or a kernel's output.
#[derive(Clone, Debug)]
pub enum SourceSchema {
    /// An Arrow schema.
    Table(SchemaRef),
    /// A tensor dtype and shape; `shape[0] == -1` means "batch dimension, variable".
    Tensor {
        /// Element type.
        dtype: DType,
        /// Shape, with `-1` in position 0 for a variable batch dimension.
        shape: Vec<i64>,
    },
}

impl SourceSchema {
    /// BLAKE3 over the Arrow IPC schema message bytes (Table) or over
    /// `"tensor:" || dtype code || shape as i64 LE` (Tensor). Keys the profile store (RC e.3).
    pub fn hash(&self) -> [u8; 32] {
        match self {
            SourceSchema::Table(schema) => {
                let generator = arrow::ipc::writer::IpcDataGenerator::default();
                let mut tracker = arrow::ipc::writer::DictionaryTracker::new(false);
                let encoded = generator.schema_to_bytes_with_dictionary_tracker(
                    schema,
                    &mut tracker,
                    &arrow::ipc::writer::IpcWriteOptions::default(),
                );
                *blake3::hash(&encoded.ipc_message).as_bytes()
            }
            SourceSchema::Tensor { dtype, shape } => {
                let mut hasher = blake3::Hasher::new();
                hasher.update(b"tensor:");
                hasher.update(&[dtype.code()]);
                for d in shape {
                    hasher.update(&d.to_le_bytes());
                }
                *hasher.finalize().as_bytes()
            }
        }
    }
}

impl PayloadSpec {
    /// Plan-time check (CT-I5): Ok if every payload conforming to `schema` can be
    /// delivered as `self.kind`. For `Tensor` on a table schema, every projected
    /// column must be a numeric column and all must share one dtype.
    pub fn check(&self, schema: &SourceSchema) -> crate::Result<()> {
        match (self.kind, schema) {
            (PayloadKind::Either, _) => Ok(()),
            (PayloadKind::Table, SourceSchema::Table(_)) => Ok(()),
            (PayloadKind::Tensor, SourceSchema::Tensor { .. }) => Ok(()),
            (PayloadKind::Tensor, SourceSchema::Table(schema)) => {
                let mut seen: Option<DType> = None;
                for field in schema.fields() {
                    let dtype = column_dtype(field.data_type()).ok_or_else(|| {
                        MorunaError::Plan(format!("column {} is not numeric", field.name()))
                    })?;
                    if field.is_nullable() {
                        return Err(MorunaError::Plan(format!(
                            "column {} is nullable; a tensor has no null slots",
                            field.name()
                        )));
                    }
                    if let DataType::FixedSizeList(inner, _) = field.data_type()
                        && inner.is_nullable()
                    {
                        return Err(MorunaError::Plan(format!(
                            "column {} has nullable list items; a tensor has no null slots",
                            field.name()
                        )));
                    }
                    match seen {
                        None => seen = Some(dtype),
                        Some(prev) if prev != dtype => {
                            return Err(MorunaError::Plan(
                                "columns have mixed dtypes; a tensor has one".into(),
                            ));
                        }
                        Some(_) => {}
                    }
                }
                Ok(())
            }
            (PayloadKind::Table, SourceSchema::Tensor { dtype, shape }) => {
                if shape.len() > 2 {
                    return Err(MorunaError::Plan(format!(
                        "tensor rank {} cannot become a column (need 1 or 2)",
                        shape.len()
                    )));
                }
                if dtype.arrow_type().is_none() {
                    return Err(MorunaError::Plan(format!(
                        "tensor dtype {dtype:?} has no zero-copy Arrow form"
                    )));
                }
                Ok(())
            }
        }
    }
}

/// The dtype of a numeric column (e.3): a primitive from the mapping, or a `FixedSizeList` of
/// one with a positive list size. `None` otherwise.
fn column_dtype(dt: &DataType) -> Option<DType> {
    match dt {
        DataType::FixedSizeList(inner, n) if *n > 0 => DType::of_arrow(inner.data_type()),
        other => DType::of_arrow(other),
    }
}

impl Payload {
    /// Infer the tier from the batch's buffers (arena metadata); error if buffers
    /// are not arena-owned and not host memory.
    pub fn table(batch: RecordBatch) -> crate::Result<Payload> {
        let tier = infer_tier(&batch, &|buf| arena_tier_of(buf))?;
        Ok(Payload::Table(batch, tier))
    }

    /// As `table`, but infers the tier through `alloc.tier_of` on each buffer's pointer
    /// rather than the arena token; for batches whose buffers were built over a
    /// foreign pointer that happens to lie in the arena (adapters AD-I2).
    pub fn table_with(batch: RecordBatch, alloc: &dyn Allocator) -> crate::Result<Payload> {
        let tier = infer_tier(&batch, &|buf| alloc.tier_of(buf.as_ptr()))?;
        Ok(Payload::Table(batch, tier))
    }

    /// Wrap a tensor; the tier is the tensor's.
    pub fn tensor(t: ManagedTensor) -> crate::Result<Payload> {
        let tier = t.tier();
        Ok(Payload::Tensor(t, tier))
    }

    /// Caller asserts residency in `tier`.
    ///
    /// # Safety
    /// The batch's bytes must be addressable in `tier` (CT-I2).
    pub unsafe fn table_in(batch: RecordBatch, tier: Tier) -> Payload {
        Payload::Table(batch, tier)
    }

    /// Caller asserts residency in `tier`.
    ///
    /// # Safety
    /// The tensor's bytes must be addressable in `tier` (CT-I2).
    pub unsafe fn tensor_in(t: ManagedTensor, tier: Tier) -> Payload {
        Payload::Tensor(t, tier)
    }

    /// The tier the bytes are in.
    pub fn tier(&self) -> Tier {
        match self {
            Payload::Table(_, tier) => *tier,
            Payload::Tensor(_, tier) => *tier,
        }
    }

    /// `Table` or `Tensor`.
    pub fn kind(&self) -> PayloadKind {
        match self {
            Payload::Table(_, _) => PayloadKind::Table,
            Payload::Tensor(_, _) => PayloadKind::Tensor,
        }
    }

    /// CT-I3 accounting: `get_array_memory_size` for a table, `element_count × item_size`
    /// for a tensor (f.1).
    pub fn bytes(&self) -> u64 {
        match self {
            Payload::Table(batch, _) => batch.get_array_memory_size() as u64,
            Payload::Tensor(t, _) => t.byte_len(),
        }
    }

    /// `batch.num_rows()` or `shape[0]` (1 for a 0-d tensor).
    pub fn rows(&self) -> u64 {
        match self {
            Payload::Table(batch, _) => batch.num_rows() as u64,
            Payload::Tensor(t, _) => t.shape().first().map_or(1, |d| *d as u64),
        }
    }

    /// Zero-copy view of a numeric column (or all columns of one dtype when
    /// `column` is None, as a 2-D tensor rows × columns) as a tensor. CT-I4.
    /// Errors: NotNumeric, HasNulls, MixedDTypes, NotContiguous.
    pub fn as_tensor(&self, column: Option<&str>) -> crate::Result<ManagedTensor> {
        let (batch, tier) = match self {
            Payload::Table(batch, tier) => (batch, *tier),
            Payload::Tensor(_, _) => {
                return Err(MorunaError::Plan(
                    "as_tensor: the payload is already a tensor".into(),
                ));
            }
        };
        if !tier.is_resident() {
            return Err(MorunaError::Staging("payload not resident".into()));
        }
        match column {
            Some(name) => {
                let index = batch
                    .schema()
                    .index_of(name)
                    .map_err(|_| MorunaError::Plan(format!("as_tensor: no column named {name}")))?;
                let array = batch.column(index);
                let (dtype, ptr, shape) = numeric_view(name, array)?;
                let owner: ArrayRef = Arc::clone(array);
                // SAFETY: `ptr` is the array's values pointer (f.3) and `owner` is a clone of
                // the array, which keeps its buffers alive for the tensor's lifetime.
                unsafe { ManagedTensor::over_owner(owner, ptr, 0, dtype, shape, tier) }
            }
            None => {
                let rows = batch.num_rows() as i64;
                let mut dtype: Option<DType> = None;
                let mut views = Vec::with_capacity(batch.num_columns());
                for (field, array) in batch.schema().fields().iter().zip(batch.columns()) {
                    let (d, ptr, shape) = numeric_view(field.name(), array)?;
                    if shape.len() != 1 {
                        return Err(ConvertError::NotContiguous.into());
                    }
                    match dtype {
                        None => dtype = Some(d),
                        Some(prev) if prev != d => return Err(ConvertError::MixedDTypes.into()),
                        Some(_) => {}
                    }
                    views.push(ptr);
                }
                let Some(dtype) = dtype else {
                    return Err(ConvertError::NotNumeric("<no columns>".into()).into());
                };
                let stride = rows as usize * dtype.item_size();
                for pair in views.windows(2) {
                    if pair[0].wrapping_add(stride) != pair[1] {
                        return Err(ConvertError::NotContiguous.into());
                    }
                }
                let owner: Vec<ArrayRef> = batch.columns().to_vec();
                let shape = vec![rows, views.len() as i64];
                // SAFETY: the columns were checked to be adjacent in one buffer, so
                // `views[0]` addresses `rows × columns` elements; `owner` (clones of every
                // column) keeps the buffers alive.
                unsafe { ManagedTensor::over_owner(owner, views[0], 0, dtype, shape, tier) }
            }
        }
    }

    /// Zero-copy view of a contiguous tensor as one Arrow column named `name`:
    /// 1-D becomes a primitive array, 2-D becomes FixedSizeList(width). CT-I4.
    pub fn as_column(&self, name: &str) -> crate::Result<ArrayRef> {
        let (tensor, tier) = match self {
            Payload::Tensor(t, tier) => (t, *tier),
            Payload::Table(_, _) => {
                return Err(MorunaError::Plan(
                    "as_column: the payload is already a table".into(),
                ));
            }
        };
        if !tier.is_host() {
            return Err(MorunaError::Staging(
                "payload not resident in host memory".into(),
            ));
        }
        if !tensor.is_contiguous() {
            return Err(ConvertError::NotContiguous.into());
        }
        let shape = tensor.shape();
        if shape.is_empty() || shape.len() > 2 {
            return Err(ConvertError::Rank(shape.len()).into());
        }
        let dtype = tensor.dtype();
        let Some(arrow_type) = dtype.arrow_type() else {
            return Err(ConvertError::NotNumeric(name.to_string()).into());
        };
        let (base, offset) = tensor.data_ptr();
        let len = tensor.byte_len() as usize;
        let ptr = base.wrapping_add(offset as usize);
        let owner = Arc::new(TensorOwner(tensor.share()));
        let values = match std::ptr::NonNull::new(ptr) {
            // SAFETY: the tensor's bytes start at `data + byte_offset` and span `byte_len`
            // bytes (DLPack contract); `owner` keeps the tensor alive for Arrow's lifetime.
            Some(nn) => unsafe { arrow::buffer::Buffer::from_custom_allocation(nn, len, owner) },
            None if len == 0 => arrow::buffer::Buffer::from(Vec::<u8>::new()),
            None => return Err(MorunaError::Staging("tensor data pointer is null".into())),
        };
        let count = shape.iter().map(|d| *d as usize).product::<usize>();
        let primitive = ArrayData::builder(arrow_type.clone())
            .len(count)
            .add_buffer(values)
            .build()
            .map_err(|e| MorunaError::Plan(format!("as_column: {e}")))?;
        if shape.len() == 1 {
            return Ok(make_array(primitive));
        }
        let width = i32::try_from(shape[1])
            .map_err(|_| MorunaError::Plan(format!("as_column: width {} exceeds i32", shape[1])))?;
        let field = Arc::new(Field::new("item", arrow_type, false));
        let list = ArrayData::builder(DataType::FixedSizeList(field, width))
            .len(shape[0] as usize)
            .add_child_data(primitive)
            .build()
            .map_err(|e| MorunaError::Plan(format!("as_column: {e}")))?;
        Ok(make_array(list))
    }

    /// Replace or append column `name` in a Table payload with `array` (same length).
    pub fn with_column(self, name: &str, array: ArrayRef) -> crate::Result<Payload> {
        let batch = match self {
            Payload::Table(batch, _) => batch,
            Payload::Tensor(_, _) => {
                return Err(MorunaError::Plan(
                    "with_column: the payload is a tensor".into(),
                ));
            }
        };
        if array.len() != batch.num_rows() {
            return Err(MorunaError::Plan(format!(
                "with_column: column {name} has {} rows, the batch has {}",
                array.len(),
                batch.num_rows()
            )));
        }
        let schema = batch.schema();
        let mut fields: Vec<Arc<Field>> = schema.fields().iter().cloned().collect();
        let mut columns: Vec<ArrayRef> = batch.columns().to_vec();
        let nullable = array.null_count() > 0;
        match schema.index_of(name) {
            Ok(i) => {
                let existing = &fields[i];
                fields[i] = Arc::new(
                    Field::new(
                        name,
                        array.data_type().clone(),
                        existing.is_nullable() || nullable,
                    )
                    .with_metadata(existing.metadata().clone()),
                );
                columns[i] = array;
            }
            Err(_) => {
                fields.push(Arc::new(Field::new(
                    name,
                    array.data_type().clone(),
                    nullable,
                )));
                columns.push(array);
            }
        }
        let new_schema = Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone()));
        let batch = RecordBatch::try_new(new_schema, columns)
            .map_err(|e| MorunaError::Plan(format!("with_column: {e}")))?;
        Payload::table(batch)
    }
}

impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Payload::Table(batch, tier) => f
                .debug_struct("Payload::Table")
                .field("rows", &batch.num_rows())
                .field("columns", &batch.num_columns())
                .field("tier", tier)
                .finish(),
            Payload::Tensor(t, tier) => f
                .debug_struct("Payload::Tensor")
                .field("tensor", t)
                .field("tier", tier)
                .finish(),
        }
    }
}

/// Keeps a tensor alive behind an Arrow buffer built over its bytes (f.4).
struct TensorOwner(#[allow(dead_code)] ManagedTensor);
impl std::panic::RefUnwindSafe for TensorOwner {}

/// The tier of every buffer of every column (recursively), which must agree; a buffer with
/// no known provenance is `Host` (e.2). Mixed tiers are a `Plan` error.
fn infer_tier(
    batch: &RecordBatch,
    tier_of: &dyn Fn(&arrow::buffer::Buffer) -> Option<Tier>,
) -> crate::Result<Tier> {
    let mut seen: Option<Tier> = None;
    for column in batch.columns() {
        walk_buffers(&column.to_data(), &mut |buf| {
            let tier = tier_of(buf).unwrap_or(Tier::Host);
            match seen {
                None => seen = Some(tier),
                Some(prev) if prev != tier => {
                    return Err(MorunaError::Plan(format!(
                        "batch has buffers in more than one tier ({prev:?} and {tier:?})"
                    )));
                }
                Some(_) => {}
            }
            Ok(())
        })?;
    }
    Ok(seen.unwrap_or(Tier::Host))
}

fn walk_buffers(
    data: &ArrayData,
    f: &mut dyn FnMut(&arrow::buffer::Buffer) -> crate::Result<()>,
) -> crate::Result<()> {
    if let Some(nulls) = data.nulls() {
        f(nulls.buffer())?;
    }
    for buf in data.buffers() {
        f(buf)?;
    }
    for child in data.child_data() {
        walk_buffers(child, f)?;
    }
    Ok(())
}

/// A numeric column's dtype, values pointer and tensor shape (f.3, e.3), or the conversion
/// error that refuses it.
fn numeric_view(name: &str, array: &ArrayRef) -> crate::Result<(DType, *mut u8, Vec<i64>)> {
    let len = array.len() as i64;
    match array.data_type() {
        DataType::FixedSizeList(inner, width) => {
            let Some(dtype) = DType::of_arrow(inner.data_type()) else {
                return Err(ConvertError::NotNumeric(name.to_string()).into());
            };
            if *width <= 0 {
                return Err(ConvertError::NotNumeric(name.to_string()).into());
            }
            if array.null_count() > 0 {
                return Err(ConvertError::HasNulls(name.to_string()).into());
            }
            let Some(list) = array.as_any().downcast_ref::<FixedSizeListArray>() else {
                return Err(ConvertError::NotNumeric(name.to_string()).into());
            };
            let values = list.values();
            if values.null_count() > 0 {
                return Err(ConvertError::HasNulls(name.to_string()).into());
            }
            let ptr = values_ptr(&values.to_data(), dtype);
            Ok((dtype, ptr, vec![len, i64::from(*width)]))
        }
        other => {
            let Some(dtype) = DType::of_arrow(other) else {
                return Err(ConvertError::NotNumeric(name.to_string()).into());
            };
            if array.null_count() > 0 {
                return Err(ConvertError::HasNulls(name.to_string()).into());
            }
            let ptr = values_ptr(&array.to_data(), dtype);
            Ok((dtype, ptr, vec![len]))
        }
    }
}

/// `buffer.as_ptr() + offset × item_size` for a primitive array's values buffer (f.3).
fn values_ptr(data: &ArrayData, dtype: DType) -> *mut u8 {
    let base = data
        .buffers()
        .first()
        .map_or(std::ptr::null(), |b| b.as_ptr());
    base.wrapping_add(data.offset() * dtype.item_size())
        .cast_mut()
}
