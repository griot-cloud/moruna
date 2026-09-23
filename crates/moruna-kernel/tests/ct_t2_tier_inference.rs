//! CT-T2 tier_inference: `Payload::table` on a batch built from `FakeAllocator` buffers tagged
//! `PinnedHost` yields `Tier::PinnedHost`; mixed-tier buffers yield `Plan`. Proves CT-I2.

mod common;

use std::sync::Arc;

use arrow::array::{ArrayData, ArrayRef, make_array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::{FakeAllocator, int_batch_in, mixed_batch};
use moruna_kernel::{MorunaError, Payload, Tier};

fn heap_int_column(values: &[i32]) -> ArrayRef {
    Arc::new(arrow::array::Int32Array::from_iter_values(
        values.iter().copied(),
    ))
}

fn int_column(alloc: &FakeAllocator, values: &[i32], tier: Tier) -> ArrayRef {
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    let buffer = alloc.arrow_buffer(&bytes, tier);
    let data = ArrayData::builder(DataType::Int32)
        .len(values.len())
        .add_buffer(buffer)
        .build()
        .expect("int array over an arena buffer");
    make_array(data)
}

#[test]
fn ct_t2_tier_inference() {
    let alloc = FakeAllocator::new();

    // Arena buffers tagged PinnedHost: the payload is PinnedHost.
    let batch = int_batch_in(&alloc, &[1, 2, 3, 4], Tier::PinnedHost);
    let payload = Payload::table(batch).expect("pinned table");
    assert_eq!(payload.tier(), Tier::PinnedHost);

    // The same through the allocator's own pointer map (adapters AD-I2).
    let batch = int_batch_in(&alloc, &[5, 6], Tier::PinnedHost);
    let payload = Payload::table_with(batch, &alloc).expect("pinned table by pointer");
    assert_eq!(payload.tier(), Tier::PinnedHost);

    // Buffers that are not arena-owned are host memory (e.2).
    let payload = Payload::table(mixed_batch(3)).expect("heap table");
    assert_eq!(payload.tier(), Tier::Host);

    // Mixed tiers in one batch are a plan error.
    let schema = Arc::new(Schema::new(vec![
        Field::new("a", DataType::Int32, false),
        Field::new("b", DataType::Int32, false),
    ]));
    let mixed = RecordBatch::try_new(
        schema,
        vec![
            int_column(&alloc, &[1, 2], Tier::PinnedHost),
            heap_int_column(&[3, 4]),
        ],
    )
    .expect("mixed batch");
    let err = Payload::table(mixed).expect_err("mixed tiers must be refused");
    assert!(
        matches!(err, MorunaError::Plan(_)),
        "expected Plan, got {err}"
    );
    assert!(err.to_string().contains("more than one tier"));
}
