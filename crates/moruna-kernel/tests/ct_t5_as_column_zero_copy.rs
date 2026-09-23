//! CT-T5 as_column_zero_copy: the reverse of CT-T4 for rank 1 and rank 2; pointer equality;
//! round trip `as_column(as_tensor(x)) == x` by value. Proves CT-I4.

mod common;

use std::sync::Arc;

use moruna_kernel::{DType, ManagedTensor, Payload, Tier};
use arrow::array::{Array, ArrayData, ArrayRef, FixedSizeListArray, Int32Array, make_array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::{FakeAllocator, arrow_dtypes};

#[test]
fn ct_t5_as_column_zero_copy() {
    let alloc = FakeAllocator::new();

    for dtype in arrow_dtypes() {
        // Rank 1: a primitive array over the tensor's own bytes.
        let buffer = alloc.buffer(8 * dtype.item_size(), Tier::Host);
        let base = buffer.host_ptr().expect("host buffer");
        let tensor =
            ManagedTensor::from_buffer(buffer, 0, dtype, vec![8]).expect("tensor over a buffer");
        let payload = Payload::tensor(tensor).expect("tensor payload");
        let before = alloc.allocations_total();
        let column = payload.as_column("c").expect("tensor to column");
        assert_eq!(alloc.allocations_total(), before, "{dtype:?} allocated");
        assert_eq!(column.len(), 8);
        assert_eq!(column.data_type(), &dtype.arrow_type().expect("arrow form"));
        assert_eq!(
            column.to_data().buffers()[0].as_ptr(),
            base.cast_const(),
            "{dtype:?}"
        );

        // Rank 2: a FixedSizeList of the width.
        let buffer = alloc.buffer(6 * dtype.item_size(), Tier::Host);
        let base = buffer.host_ptr().expect("host buffer");
        let tensor =
            ManagedTensor::from_buffer(buffer, 0, dtype, vec![2, 3]).expect("tensor over a buffer");
        let payload = Payload::tensor(tensor).expect("tensor payload");
        let column = payload.as_column("c").expect("tensor to list column");
        let list = column
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .expect("a fixed size list");
        assert_eq!(list.len(), 2);
        assert_eq!(list.value_length(), 3);
        assert_eq!(
            list.values().to_data().buffers()[0].as_ptr(),
            base.cast_const()
        );
    }

    // Round trip: a column becomes a tensor and comes back equal by value.
    let values: Vec<i32> = (0..32).collect();
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    let buffer = alloc.arrow_buffer(&bytes, Tier::Host);
    let data = ArrayData::builder(DataType::Int32)
        .len(values.len())
        .add_buffer(buffer)
        .build()
        .expect("int array");
    let original: ArrayRef = make_array(data);
    let schema = Arc::new(Schema::new(vec![Field::new("c", DataType::Int32, false)]));
    let batch = RecordBatch::try_new(schema, vec![Arc::clone(&original)]).expect("batch");
    let table = Payload::table(batch).expect("table payload");
    let tensor = table.as_tensor(Some("c")).expect("column to tensor");
    let round_tripped = Payload::tensor(tensor)
        .expect("tensor payload")
        .as_column("c")
        .expect("tensor to column");
    assert_eq!(
        round_tripped
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("int array")
            .values(),
        original
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("int array")
            .values()
    );

    // `with_column` replaces a column in place and keeps the row count.
    let table = Payload::table(common::mixed_batch(4)).expect("table payload");
    let replacement: ArrayRef = Arc::new(Int32Array::from_iter_values(0..4));
    let updated = table
        .with_column("i", Arc::clone(&replacement))
        .expect("replace a column");
    match &updated {
        Payload::Table(batch, _) => {
            assert_eq!(batch.num_columns(), 3);
            assert_eq!(batch.column(0).len(), 4);
        }
        Payload::Tensor(_, _) => panic!("with_column returned a tensor"),
    }
    // and appends one that is not there yet.
    let appended = updated
        .with_column("new", replacement)
        .expect("append a column");
    match &appended {
        Payload::Table(batch, _) => assert_eq!(batch.num_columns(), 4),
        Payload::Tensor(_, _) => panic!("with_column returned a tensor"),
    }
    // A column of the wrong length is refused, and so is `with_column` on a tensor payload.
    let short: ArrayRef = Arc::new(Int32Array::from_iter_values(0..2));
    let err = appended
        .with_column("i", short)
        .expect_err("a short column must be refused");
    assert!(
        matches!(err, moruna_kernel::MorunaError::Plan(_)),
        "got {err}"
    );
    assert!(err.to_string().contains("rows"));

    let buffer = alloc.buffer(16, Tier::Host);
    let tensor = ManagedTensor::from_buffer(buffer, 0, DType::F32, vec![4]).expect("a tensor");
    let column: ArrayRef = Arc::new(Int32Array::from_iter_values(0..4));
    let err = Payload::tensor(tensor)
        .expect("tensor payload")
        .with_column("c", column)
        .expect_err("with_column on a tensor must be refused");
    assert!(
        matches!(err, moruna_kernel::MorunaError::Plan(_)),
        "got {err}"
    );

    let dtypes: Vec<DType> = arrow_dtypes();
    assert!(dtypes.contains(&DType::F64) && !dtypes.contains(&DType::Bool));
}
