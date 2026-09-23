//! CT-T4 as_tensor_zero_copy: for each numeric type in e.3, `as_tensor` returns a tensor whose
//! `data_ptr` equals the array's values pointer and `FakeAllocator.allocations_total` is
//! unchanged. Proves CT-I4.

mod common;

use std::sync::Arc;

use moruna_kernel::{DType, Payload, Tier};
use arrow::array::{ArrayData, ArrayRef, make_array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::{FakeAllocator, arrow_dtypes};

/// A one-column batch whose values buffer is an arena region of `alloc`, filled with `rows`
/// elements of `dtype` (the bytes themselves do not matter to a pointer test).
fn column_batch(alloc: &FakeAllocator, dtype: DType, rows: usize) -> (RecordBatch, *const u8) {
    let bytes = vec![0u8; rows * dtype.item_size()];
    let buffer = alloc.arrow_buffer(&bytes, Tier::PinnedHost);
    let values_ptr = buffer.as_ptr();
    let arrow_type = dtype.arrow_type().expect("a dtype with an Arrow form");
    let data = ArrayData::builder(arrow_type.clone())
        .len(rows)
        .add_buffer(buffer)
        .build()
        .expect("primitive array over an arena buffer");
    let column: ArrayRef = make_array(data);
    let schema = Arc::new(Schema::new(vec![Field::new("c", arrow_type, false)]));
    (
        RecordBatch::try_new(schema, vec![column]).expect("batch"),
        values_ptr,
    )
}

#[test]
fn ct_t4_as_tensor_zero_copy() {
    let alloc = FakeAllocator::new();
    for dtype in arrow_dtypes() {
        let (batch, values_ptr) = column_batch(&alloc, dtype, 16);
        let payload = Payload::table(batch).expect("table payload");
        let before = alloc.allocations_total();

        let tensor = payload.as_tensor(Some("c")).expect("column to tensor");

        assert_eq!(alloc.allocations_total(), before, "{dtype:?} allocated");
        let (base, offset) = tensor.data_ptr();
        assert_eq!(
            base.wrapping_add(offset as usize).cast_const(),
            values_ptr,
            "{dtype:?}"
        );
        assert_eq!(tensor.dtype(), dtype);
        assert_eq!(tensor.shape(), [16]);
        assert_eq!(tensor.tier(), Tier::PinnedHost);
        assert!(tensor.is_contiguous());
        assert_eq!(tensor.byte_len(), 16 * dtype.item_size() as u64);
    }

    // A FixedSizeList column becomes a 2-D tensor over the child values (e.3).
    let bytes = vec![0u8; 6 * 4];
    let buffer = alloc.arrow_buffer(&bytes, Tier::Host);
    let values_ptr = buffer.as_ptr();
    let child = ArrayData::builder(DataType::Float32)
        .len(6)
        .add_buffer(buffer)
        .build()
        .expect("child values");
    let field = Arc::new(Field::new("item", DataType::Float32, false));
    let list = ArrayData::builder(DataType::FixedSizeList(field.clone(), 3))
        .len(2)
        .add_child_data(child)
        .build()
        .expect("fixed size list");
    let column: ArrayRef = make_array(list);
    let schema = Arc::new(Schema::new(vec![Field::new(
        "v",
        DataType::FixedSizeList(field, 3),
        false,
    )]));
    let batch = RecordBatch::try_new(schema, vec![column]).expect("batch");
    let payload = Payload::table(batch).expect("table payload");
    let before = alloc.allocations_total();
    let tensor = payload.as_tensor(Some("v")).expect("list column to tensor");
    assert_eq!(alloc.allocations_total(), before);
    assert_eq!(tensor.shape(), [2, 3]);
    let (base, offset) = tensor.data_ptr();
    assert_eq!(base.wrapping_add(offset as usize).cast_const(), values_ptr);

    // `as_tensor(None)` over adjacent columns of one dtype in one buffer: a 2-D view of
    // rows by columns, with no allocation.
    let rows = 4usize;
    let whole = alloc.arrow_buffer(&vec![0u8; 2 * rows * 4], Tier::Host);
    let mut columns: Vec<ArrayRef> = Vec::new();
    for c in 0..2 {
        let slice = whole.slice_with_length(c * rows * 4, rows * 4);
        let data = ArrayData::builder(DataType::Int32)
            .len(rows)
            .add_buffer(slice)
            .build()
            .expect("column over a slice of one buffer");
        columns.push(make_array(data));
    }
    let schema = Arc::new(Schema::new(vec![
        Field::new("a", DataType::Int32, false),
        Field::new("b", DataType::Int32, false),
    ]));
    let batch = RecordBatch::try_new(schema, columns).expect("batch");
    let payload = Payload::table(batch).expect("table payload");
    let before = alloc.allocations_total();
    let tensor = payload
        .as_tensor(None)
        .expect("adjacent columns to a 2-D tensor");
    assert_eq!(alloc.allocations_total(), before);
    assert_eq!(tensor.shape(), [rows as i64, 2]);
    assert_eq!(tensor.dtype(), DType::I32);
    let (base, offset) = tensor.data_ptr();
    assert_eq!(
        base.wrapping_add(offset as usize).cast_const(),
        whole.as_ptr()
    );

    // Columns in separate allocations are `NotContiguous`: e.3 states this so that no
    // implementer copies them together.
    let payload = Payload::table(common::mixed_batch(4)).expect("table payload");
    assert!(payload.as_tensor(None).is_err());
}
