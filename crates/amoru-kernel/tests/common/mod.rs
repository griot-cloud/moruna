//! Test-only helpers shared by the CT tests.
//!
//! The allocator is the testkit's `FakeAllocator` (contracts d.15), which feature F0.3 built in
//! this pull request: CT-T2, CT-T4 and CT-T17 name it. What is left here is the batch fixtures
//! those tests build over it.

#![allow(dead_code)]

use std::sync::Arc;

use amoru_kernel::{ArenaHandle, Buffer, DType, Tier};
pub use amoru_testkit::FakeAllocator;
use arrow::array::{ArrayRef, Float32Array, Int32Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;

/// A batch of `rows` with an int column, a float column and a string column, over ordinary
/// heap buffers.
pub fn mixed_batch(rows: usize) -> RecordBatch {
    let ints: ArrayRef = Arc::new(Int32Array::from_iter_values(0..rows as i32));
    let floats: ArrayRef = Arc::new(Float32Array::from_iter_values((0..rows).map(|i| i as f32)));
    let strings: ArrayRef = Arc::new(StringArray::from_iter_values(
        (0..rows).map(|i| format!("row-{i}")),
    ));
    let schema = Arc::new(Schema::new(vec![
        Field::new("i", DataType::Int32, false),
        Field::new("f", DataType::Float32, false),
        Field::new("s", DataType::Utf8, false),
    ]));
    match RecordBatch::try_new(schema, vec![ints, floats, strings]) {
        Ok(batch) => batch,
        Err(e) => panic!("mixed_batch: {e}"),
    }
}

/// A one-column batch of `values` in `tier`, its values buffer allocated from `alloc`.
pub fn int_batch_in(alloc: &FakeAllocator, values: &[i32], tier: Tier) -> RecordBatch {
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    let buffer = alloc.arrow_buffer(&bytes, tier);
    let data = arrow::array::ArrayData::builder(DataType::Int32)
        .len(values.len())
        .add_buffer(buffer)
        .build();
    let data = match data {
        Ok(d) => d,
        Err(e) => panic!("int_batch_in: {e}"),
    };
    let column: ArrayRef = arrow::array::make_array(data);
    let schema = Arc::new(Schema::new(vec![Field::new("i", DataType::Int32, false)]));
    match RecordBatch::try_new(schema, vec![column]) {
        Ok(batch) => batch,
        Err(e) => panic!("int_batch_in: {e}"),
    }
}

/// Every dtype that has a zero-copy Arrow form (e.3).
pub fn arrow_dtypes() -> Vec<DType> {
    DType::ALL
        .into_iter()
        .filter(|d| d.arrow_type().is_some())
        .collect()
}

/// An arena token that owns nothing: for a test buffer over leaked memory.
struct NullArena;

impl ArenaHandle for NullArena {
    fn release(&self, _ptr: *mut u8, _len: usize, _tier: Tier) {}
}

/// A buffer of `len` zero bytes tagged with `tier`, over memory this process leaks, so a test
/// can reach a code path that a real allocation of that tier would need hardware for.
pub fn tagged_buffer(len: usize, tier: Tier) -> Buffer {
    let mut bytes = vec![0u8; len.max(1)];
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    // SAFETY: the bytes are leaked, so they outlive every buffer over them, and `NullArena`
    // releases nothing.
    unsafe { Buffer::from_raw(ptr, len, tier, Arc::new(NullArena)) }
}
