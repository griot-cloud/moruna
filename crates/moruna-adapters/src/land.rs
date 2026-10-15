//! The boundary copy of a table (05 f.3, AD-I2), with no feature: the Python adapter lands a
//! kernel's returned table with it, and so do both runtimes of a packaged kernel (17 f.4, f.8),
//! so there is one copy rule for every kernel whose output the arena does not own.

use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, ArrayData, ArrayRef, make_array};
use moruna_kernel::arrow::buffer::{BooleanBuffer, Buffer as ArrowBuffer, NullBuffer};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{Allocator, MorunaError, Payload, Result, Tier};

/// f.3: `batch` as a payload over arena buffers, copied once when the arena does not own them and
/// counted in `boundary_copies_total`; returns the payload and the bytes copied.
pub fn land_table(batch: RecordBatch, alloc: &Arc<dyn Allocator>) -> Result<(Payload, u64)> {
    let (payload, copied) = table(batch, alloc.as_ref())?;
    if copied > 0 {
        alloc.note_boundary_copy(copied);
    }
    Ok((payload, copied))
}

/// The run's one host tier: `PinnedHost` when the arena is page locked, `Host` otherwise
/// (contracts e.1).
pub(crate) fn host_tier(alloc: &dyn Allocator) -> Tier {
    if alloc.is_pinned() {
        Tier::PinnedHost
    } else {
        Tier::Host
    }
}

/// f.3 for a table: the payload over arena buffers and the bytes copied, 0 when the batch's
/// buffers are already the arena's. Not counted; [`land_table`] counts.
pub(crate) fn table(batch: RecordBatch, alloc: &dyn Allocator) -> Result<(Payload, u64)> {
    if batch_is_arena_owned(&batch, alloc) {
        return Ok((Payload::table_with(batch, alloc)?, 0));
    }
    let tier = host_tier(alloc);
    let mut copied = 0u64;
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    for column in batch.columns() {
        let data = copy_array_data(&column.to_data(), alloc, tier, &mut copied)?;
        columns.push(make_array(data));
    }
    let schema = batch.schema();
    let copy = RecordBatch::try_new_with_options(
        schema,
        columns,
        &moruna_kernel::arrow::record_batch::RecordBatchOptions::new()
            .with_row_count(Some(batch.num_rows())),
    )
    .map_err(|e| MorunaError::Plan(format!("boundary copy: {e}")))?;
    Ok((Payload::table_with(copy, alloc)?, copied))
}

/// True when every buffer of every column already lies in a region this allocator owns, which is
/// the case when the kernel returned a slice of what it was given.
fn batch_is_arena_owned(batch: &RecordBatch, alloc: &dyn Allocator) -> bool {
    let mut any = false;
    for column in batch.columns() {
        if !data_is_arena_owned(&column.to_data(), alloc, &mut any) {
            return false;
        }
    }
    any
}

fn data_is_arena_owned(data: &ArrayData, alloc: &dyn Allocator, any: &mut bool) -> bool {
    if let Some(nulls) = data.nulls() {
        *any = true;
        if !alloc.contains(nulls.buffer().as_ptr()) {
            return false;
        }
    }
    for buffer in data.buffers() {
        *any = true;
        if !alloc.contains(buffer.as_ptr()) {
            return false;
        }
    }
    for child in data.child_data() {
        if !data_is_arena_owned(child, alloc, any) {
            return false;
        }
    }
    true
}

fn copy_array_data(
    data: &ArrayData,
    alloc: &dyn Allocator,
    tier: Tier,
    copied: &mut u64,
) -> Result<ArrayData> {
    let mut builder = ArrayData::builder(data.data_type().clone())
        .len(data.len())
        .offset(data.offset());
    if let Some(nulls) = data.nulls() {
        let bits = copy_buffer(nulls.buffer(), alloc, tier, copied)?;
        let boolean = BooleanBuffer::new(bits, nulls.offset(), nulls.len());
        builder = builder.nulls(Some(NullBuffer::new(boolean)));
    }
    for buffer in data.buffers() {
        builder = builder.add_buffer(copy_buffer(buffer, alloc, tier, copied)?);
    }
    for child in data.child_data() {
        builder = builder.add_child_data(copy_array_data(child, alloc, tier, copied)?);
    }
    builder
        .build()
        .map_err(|e| MorunaError::Plan(format!("boundary copy: rebuilding an array: {e}")))
}

/// Copy one Arrow buffer into an arena buffer of the same length.
fn copy_buffer(
    source: &ArrowBuffer,
    alloc: &dyn Allocator,
    tier: Tier,
    copied: &mut u64,
) -> Result<ArrowBuffer> {
    let bytes = source.as_slice();
    let mut buffer = alloc.alloc(bytes.len().max(1), tier)?;
    buffer[..bytes.len()].copy_from_slice(bytes);
    *copied += bytes.len() as u64;
    Ok(buffer
        .into_arrow_buffer()?
        .slice_with_length(0, bytes.len()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moruna_kernel::arrow::array::{Int64Array, StringArray};
    use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
    use moruna_kernel::arrow::record_batch::RecordBatch;
    use moruna_kernel::{Allocator, Payload, Tier};
    use moruna_testkit::FakeAllocator;

    use super::land_table;

    fn heap_batch() -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("n", DataType::Int64, true),
                Field::new("s", DataType::Utf8, true),
            ])),
            vec![
                Arc::new(Int64Array::from(vec![Some(1), None, Some(3)])),
                Arc::new(StringArray::from(vec!["a", "bb", "ccc"])),
            ],
        )
        .unwrap()
    }

    /// AD-I2 through the feature-free path: a heap batch is copied once and counted; the landed
    /// batch is the arena's and equal; landing it again copies nothing.
    #[test]
    fn a_heap_table_is_copied_once_and_an_arena_table_not_at_all() {
        let fake = Arc::new(FakeAllocator::new());
        let alloc: Arc<dyn Allocator> = fake.clone();
        let batch = heap_batch();
        let (payload, copied) = land_table(batch.clone(), &alloc).unwrap();
        assert!(copied > 0);
        assert_eq!(
            fake.boundary_copies(),
            copied,
            "the bytes copied are counted"
        );
        let Payload::Table(landed, tier) = payload else {
            panic!("a table lands as a table");
        };
        assert_eq!(tier, Tier::Host);
        assert_eq!(landed, batch);
        let (_, again) = land_table(landed, &alloc).unwrap();
        assert_eq!(again, 0);
        assert_eq!(
            fake.boundary_copies(),
            copied,
            "an arena table is not copied"
        );
        // A failing allocation surfaces as the allocator's error.
        let failing: Arc<dyn Allocator> = Arc::new(FakeAllocator::new().fail_next(1));
        assert!(land_table(heap_batch(), &failing).is_err());
        let pinned: Arc<dyn Allocator> = Arc::new(FakeAllocator::new().pinned(true));
        let (payload, _) = land_table(heap_batch(), &pinned).unwrap();
        assert_eq!(payload.tier(), Tier::PinnedHost);
    }
}
