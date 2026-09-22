//! The decode copy (f.4): the one CPU copy of payload bytes a source is allowed (G-I2,
//! SO-I5).
//!
//! Every buffer of every decoded array, its validity bitmap and its children included, is
//! copied into a buffer from `alloc` in `tier`, and the array is rebuilt over those buffers.
//! `Buffer::into_arrow_buffer` keeps the arena token on the Arrow buffer, so `Payload::table`
//! infers the tier from the batch itself (contracts e.2) and the budget sees every payload
//! byte (G-I1).
//!
//! Dictionary-encoded columns are unpacked to plain arrays before the copy: the dictionary
//! would be a third buffer family and would complicate the tensor crossing.

use std::sync::Arc;

use amoru_kernel::arrow::array::{Array, ArrayData, RecordBatch, RecordBatchOptions, make_array};
use amoru_kernel::arrow::buffer::{BooleanBuffer, NullBuffer};
use amoru_kernel::arrow::datatypes::{DataType, Field, Fields, Schema};
use amoru_kernel::{Allocator, AmoruError, Result, Tier};

/// Copy every buffer of `batch` into the arena and rebuild it. Returns the new batch and the
/// bytes copied.
pub(crate) fn copy_batch(
    batch: &RecordBatch,
    alloc: &dyn Allocator,
    tier: Tier,
) -> Result<(RecordBatch, u64)> {
    let mut copied = 0u64;
    let mut columns = Vec::with_capacity(batch.num_columns());
    let mut fields = Vec::with_capacity(batch.num_columns());
    for (field, column) in batch.schema().fields().iter().zip(batch.columns()) {
        let plain = unpack_dictionary(column)?;
        // Unpacking changes the column's type, so the schema is rebuilt with it.
        fields.push(Arc::new(
            Field::new(field.name(), plain.data_type().clone(), field.is_nullable())
                .with_metadata(field.metadata().clone()),
        ));
        let data = copy_data(&plain.to_data(), alloc, tier, &mut copied)?;
        columns.push(make_array(data));
    }
    let schema = Arc::new(
        Schema::new(Fields::from(fields)).with_metadata(batch.schema().metadata().clone()),
    );
    let batch = RecordBatch::try_new_with_options(
        schema,
        columns,
        &RecordBatchOptions::new().with_row_count(Some(batch.num_rows())),
    )
    .map_err(|e| AmoruError::Plan(format!("rebuilding the decoded batch: {e}")))?;
    Ok((batch, copied))
}

/// A dictionary column as a plain array of its value type; anything else unchanged (f.4).
fn unpack_dictionary(
    column: &amoru_kernel::arrow::array::ArrayRef,
) -> Result<amoru_kernel::arrow::array::ArrayRef> {
    match column.data_type() {
        DataType::Dictionary(_, values) => {
            amoru_kernel::arrow::compute::cast(column, values.as_ref())
                .map_err(|e| AmoruError::Plan(format!("unpacking a dictionary column: {e}")))
        }
        _ => Ok(column.clone()),
    }
}

/// Copy one array's buffers, bitmap and children into the arena, keeping its offset and length.
///
/// The whole of each buffer is copied and the offset is kept, rather than compacting to offset
/// zero, because compacting is a second copy and SO-I5 allows exactly one.
fn copy_data(
    data: &ArrayData,
    alloc: &dyn Allocator,
    tier: Tier,
    copied: &mut u64,
) -> Result<ArrayData> {
    let mut builder = ArrayData::builder(data.data_type().clone())
        .len(data.len())
        .offset(data.offset());
    for buffer in data.buffers() {
        builder = builder.add_buffer(copy_buffer(buffer, alloc, tier, copied)?);
    }
    if let Some(nulls) = data.nulls() {
        let inner = nulls.inner();
        let bits = copy_buffer(inner.inner(), alloc, tier, copied)?;
        let boolean = BooleanBuffer::new(bits, inner.offset(), inner.len());
        builder = builder.nulls(Some(NullBuffer::new(boolean)));
    }
    for child in data.child_data() {
        builder = builder.add_child_data(copy_data(child, alloc, tier, copied)?);
    }
    builder
        .build()
        .map_err(|e| AmoruError::Plan(format!("rebuilding a decoded column: {e}")))
}

/// One Arrow buffer copied into an arena buffer of `tier`.
fn copy_buffer(
    source: &amoru_kernel::arrow::buffer::Buffer,
    alloc: &dyn Allocator,
    tier: Tier,
    copied: &mut u64,
) -> Result<amoru_kernel::arrow::buffer::Buffer> {
    let bytes = source.as_slice();
    let mut buffer = alloc.alloc(bytes.len(), tier)?;
    buffer[..bytes.len()].copy_from_slice(bytes);
    *copied += bytes.len() as u64;
    let arrow = buffer.into_arrow_buffer()?;
    Ok(arrow.slice_with_length(0, bytes.len()))
}
