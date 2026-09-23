//! CT-T11 features_cost: `MorselFeatures::from_payload` is O(columns), never O(rows), and on
//! the reference host it runs in under 1 ms on a 10 M-row string batch. Proves f.2.
//!
//! The timing half is tagged "(reference host, E1)" in the SDD, so it runs only when
//! `MORUNA_REFERENCE_HOST=1` names this host as the reference host; the structural half runs
//! everywhere and is what proves the O(columns) property: the batch is built over a values
//! buffer far larger than the strings it holds, so an implementation that read the values (or
//! took the buffer's length) would compute a different mean from the one the offsets give.

mod common;

use std::sync::Arc;
use std::time::Instant;

use arrow::array::{ArrayData, ArrayRef, make_array};
use arrow::buffer::Buffer as ArrowBuffer;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::FakeAllocator;
use moruna_kernel::{MorselFeatures, Payload, Tier};

/// A `Utf8` column of `rows` strings of `width` bytes each, whose values buffer is an arena
/// region of `values_bytes` bytes: far more than the strings occupy, so only the offsets can
/// give the right answer.
fn wide_values_column(
    alloc: &FakeAllocator,
    rows: usize,
    width: usize,
    values_bytes: usize,
) -> ArrayRef {
    let offsets: Vec<i32> = (0..=rows).map(|i| (i * width) as i32).collect();
    let offsets_bytes: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
    let values = alloc.buffer(values_bytes, Tier::Host);
    let values: ArrowBuffer = values.into_arrow_buffer().expect("an arena values buffer");
    let data = ArrayData::builder(DataType::Utf8)
        .len(rows)
        .add_buffer(ArrowBuffer::from(offsets_bytes))
        .add_buffer(values)
        .build()
        .expect("a string array over an arena values buffer");
    make_array(data)
}

fn batch_of(column: ArrayRef) -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![Field::new("s", DataType::Utf8, false)]));
    RecordBatch::try_new(schema, vec![column]).expect("batch")
}

#[test]
fn ct_t11_features_cost() {
    let alloc = FakeAllocator::new();
    let rows = 1024usize;
    let width = 7usize;
    let values_bytes = 8 * 1024 * 1024;
    let column = wide_values_column(&alloc, rows, width, values_bytes);
    let batch = batch_of(column);
    let payload = Payload::table(batch).expect("table payload");

    let features = MorselFeatures::from_payload(&payload);

    // The string total is the offsets buffer's last value, not the values buffer's length:
    // an O(rows) implementation that walked the values, or one that took the buffer size,
    // would not produce this number.
    assert_eq!(features.mean_string_len, Some(width as f32));
    assert!(values_bytes as f32 / rows as f32 > width as f32 * 100.0);
    assert_eq!(features.rows, rows as u64);
    assert_eq!(features.column_bytes.len(), 1);
    assert_eq!(features.null_ratio, Some(0.0));
    assert_eq!(features.shape, None);
    assert_eq!(features.dtype, None);

    // A slice of the same column reports the mean of the slice's own offsets.
    let sliced = batch_of(make_array(
        wide_values_column(&alloc, rows, width, values_bytes)
            .to_data()
            .slice(16, 32),
    ));
    let sliced = MorselFeatures::from_payload(&Payload::table(sliced).expect("table payload"));
    assert_eq!(sliced.rows, 32);
    assert_eq!(sliced.mean_string_len, Some(width as f32));

    // A batch with no string column has no mean, and a tensor carries shape and dtype instead.
    let plain = MorselFeatures::from_payload(
        &Payload::table(common::int_batch_in(&alloc, &[1, 2, 3], Tier::Host))
            .expect("table payload"),
    );
    assert_eq!(plain.mean_string_len, None);
    let tensor = moruna_kernel::ManagedTensor::from_buffer(
        alloc.buffer(4 * 4, Tier::Host),
        0,
        moruna_kernel::DType::F32,
        vec![2, 2],
    )
    .expect("tensor over a buffer");
    let tensor = MorselFeatures::from_payload(&Payload::tensor(tensor).expect("tensor payload"));
    assert_eq!(tensor.shape, Some(vec![2, 2]));
    assert_eq!(tensor.dtype, Some(moruna_kernel::DType::F32));
    assert_eq!(tensor.mean_string_len, None);
    assert_eq!(tensor.null_ratio, None);
    assert!(tensor.column_bytes.is_empty());

    // Nulls are counted over cells.
    let nullable: ArrayRef = Arc::new(arrow::array::Int32Array::from(vec![Some(1), None]));
    let schema = Arc::new(Schema::new(vec![Field::new("i", DataType::Int32, true)]));
    let batch = RecordBatch::try_new(schema, vec![nullable]).expect("batch");
    let with_nulls = MorselFeatures::from_payload(&Payload::table(batch).expect("table payload"));
    assert_eq!(with_nulls.null_ratio, Some(0.5));

    // A large string column with no bytes at all still reports a mean (of zero), not None.
    let empty = batch_of(wide_values_column(&alloc, 4, 0, 4096));
    let empty = MorselFeatures::from_payload(&Payload::table(empty).expect("table payload"));
    assert_eq!(empty.mean_string_len, Some(0.0));

    // An empty batch has no cells, so its null ratio is zero rather than undefined (h), and a
    // string column with no rows has a mean of zero rather than None.
    let empty = MorselFeatures::from_payload(
        &Payload::table(common::mixed_batch(0)).expect("table payload"),
    );
    assert_eq!(empty.rows, 0);
    assert_eq!(empty.null_ratio, Some(0.0));
    assert_eq!(empty.mean_string_len, Some(0.0));

    // A `LargeUtf8` column is read from its 64-bit offsets, the same way (f.2).
    let long: ArrayRef = Arc::new(arrow::array::LargeStringArray::from_iter_values(
        (0..8).map(|i| "x".repeat(i)),
    ));
    let schema = Arc::new(Schema::new(vec![Field::new(
        "ls",
        DataType::LargeUtf8,
        false,
    )]));
    let batch = RecordBatch::try_new(schema, vec![long]).expect("batch");
    let features = MorselFeatures::from_payload(&Payload::table(batch).expect("table payload"));
    let total: usize = (0..8).sum();
    assert_eq!(features.mean_string_len, Some(total as f32 / 8.0));

    // The timing half (reference host, E1).
    if std::env::var("MORUNA_REFERENCE_HOST").as_deref() == Ok("1") {
        let rows = 10_000_000usize;
        let batch = batch_of(wide_values_column(&alloc, rows, 8, rows * 8));
        let payload = Payload::table(batch).expect("table payload");
        let start = Instant::now();
        let features = MorselFeatures::from_payload(&payload);
        let elapsed = start.elapsed();
        assert_eq!(features.rows, rows as u64);
        assert!(
            elapsed.as_micros() < 1000,
            "feature extraction over {rows} rows took {elapsed:?}, over the 1 ms bound"
        );
    }
}
